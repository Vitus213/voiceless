//! GGUF ASR models (Qwen3-ASR 1.7B, …) on transcribe.cpp/ggml. On the CUDA
//! build the model runs on the GPU: Qwen3-ASR-1.7B Q8_0 decodes 8 s of
//! speech in ~0.2 s on an RTX 3070 and holds ~2.5 GB of VRAM.
//!
//! This is the GGUF `Decoder` of the `local-offline` backend
//! (`[asr] streaming_backend = "local-offline"` with a `[local] model`
//! ending in `-gguf`: `qwen3-asr-1.7b-gguf`, `qwen3-asr-0.6b-gguf`;
//! `[local] provider = "cuda" | "vulkan" | "auto" | "cpu"`; see
//! `LocalConfig::device` for the build's default). The module requires
//! the `transcribe` feature. `cuda` and `vulkan` enable GPU inference.
//! Only `create_decoder` constructs
//! a `GgufDecoder`; the daemon keeps one warm across sessions
//! (`engine::WarmEngine`), one-shot `voiceless stream` loads it on the
//! offline worker thread.
//!
//! Model file: `<model dir>/model.gguf` (`MODEL_FILE`, the single allowed
//! member of the catalog entry). `[local] hotwords` (plus `[text]
//! hotwords`) become the engine's vocabulary when the model advertises
//! that feature; the confirmed-text tail handed in by `SegmentedRecognizer`
//! becomes the context prompt when the model supports prompts.
//!
//! Devices: after the load the decoder asks the model which backend and
//! device it was bound to (`Model::backend`, `Model::device`) and keeps
//! the answer as its `Decoder::device`. An explicit `cuda` or `vulkan`
//! fails when the engine cannot use it; `auto` takes the first GPU device
//! that initializes and otherwise the CPU, and the device then says why.
//!
//! Identity: the load succeeds only for a model that serves speech
//! recognition and reports the audio it takes (`GgufIdentity`, read from
//! the engine after the load). A VAD, a language-ID or a text model is
//! `EngineUnavailable` before any session is opened. `models::import`
//! relies on this load, on the CPU, to accept a local GGUF file.
//!
//! Failure semantics: a missing file or a load/warm-up failure is an
//! `OfflineAsrError` (`MissingModelFile`, `DeviceUnavailable`,
//! `EngineUnavailable`) that the session reports once as an error; a
//! decode failure is logged to stderr and yields `""` for that segment, so
//! the session continues and that segment's text is simply absent. No
//! retries.

use std::path::Path;
use std::sync::Mutex;

use transcribe_cpp::{
    backend_available, devices, Backend, DeviceType, Feature, Model, ModelOptions, Role,
    RunOptions, Session,
};

use crate::offline_asr::{
    ComputeApi, ComputeDevice, CpuFallback, Decoder, InferenceDevice, InferenceEngine,
    OfflineAsrError, OfflineOptions,
};

/// File a GGUF model is stored as inside its payload (`models::GGUF_MODEL_FILE`).
pub const MODEL_FILE: &str = crate::models::GGUF_MODEL_FILE;

/// What a loaded GGUF model is, read from the engine after the load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufIdentity {
    /// `general.architecture` of the file (`qwen3_asr`).
    pub architecture: String,
    /// `stt.variant` of the file; empty when it has none.
    pub variant: String,
    /// Audio the model is made for, in Hz. Every speech family sets it;
    /// a model that serves another role leaves it at zero.
    pub native_sample_rate: i32,
    /// Languages the model recognizes (BCP-47); empty when the file does
    /// not list them.
    pub languages: Vec<String>,
    /// Longest audio one decode takes, in ms; 0 when there is no limit.
    pub max_audio_ms: i64,
}

/// A loaded GGUF model with one warmed-up transcribe.cpp session. `Send +
/// Sync` through the mutex, so the daemon shares it as `Arc<dyn Decoder>`
/// between the live session and console jobs; decodes serialise on the
/// mutex. Dropping it frees the model (and its VRAM), which is why
/// `OfflineTransport` joins its worker before the process exits.
pub struct GgufDecoder {
    /// One session per model: sessions decode one segment at a time.
    session: Mutex<Session>,
    /// Per-run options shared by every decode (vocabulary from hotwords).
    options: RunOptions,
    /// The model accepts a text prompt; set from `Feature::ContextPrompt`.
    context_prompt: bool,
    /// Asked from the model after the load; see `Decoder::device`.
    device: InferenceDevice,
    /// Asked from the model after the load; see `GgufIdentity`.
    identity: GgufIdentity,
}

impl GgufDecoder {
    /// Load `path` for the requested device, then decode one second of
    /// silence to warm up kernels and buffers before the first utterance.
    /// Returns `MissingModelFile` for a missing file, `DeviceUnavailable`
    /// when the engine could not use the requested device and
    /// `EngineUnavailable` for any other load or warm-up failure. The
    /// loaded model is asked where it runs: `auto` takes the first GPU
    /// device that initializes and otherwise the CPU; an explicit device
    /// never moves the model elsewhere.
    pub fn load(
        path: &Path,
        requested: ComputeDevice,
        options: &OfflineOptions,
    ) -> Result<Self, OfflineAsrError> {
        if !path.is_file() {
            return Err(OfflineAsrError::MissingModelFile(path.to_owned()));
        }
        let failed = |error: transcribe_cpp::Error| load_error(error, path, requested);
        let model = Model::load_with(
            path,
            &ModelOptions {
                backend: backend(requested),
                device: None,
            },
        )
        .map_err(failed)?;
        let identity = identity_of(&model, path)?;
        let device = reported_device(&model, requested, path)?;
        // Hotwords only make sense for models with a vocabulary input;
        // passing them to another model would be an engine error.
        let run = RunOptions {
            vocabulary: if model.supports(Feature::Vocabulary) {
                options.hotwords.clone()
            } else {
                Vec::new()
            },
            ..RunOptions::default()
        };
        let context_prompt = model.supports(Feature::ContextPrompt);
        let mut session = model.session().map_err(failed)?;
        // First run pays kernel and buffer setup; do it before any session.
        session.run(&vec![0.0; 16_000], &run).map_err(failed)?;
        Ok(Self {
            session: Mutex::new(session),
            options: run,
            context_prompt,
            device,
            identity,
        })
    }

    /// What the loaded model is, as the engine reported it after the load.
    pub fn identity(&self) -> &GgufIdentity {
        &self.identity
    }
}

impl Decoder for GgufDecoder {
    /// Decode one segment (16 kHz mono f32), conditioning on `context` as
    /// the prompt when the model supports it. Blocking, serialised on the
    /// session mutex. Returns trimmed text, or `""` after logging a decode
    /// error to stderr; a poisoned mutex is reused rather than propagated,
    /// so one panicking decode does not disable the warm engine.
    fn decode(&self, samples: &[f32], context: &str) -> String {
        let mut options = self.options.clone();
        if self.context_prompt && !context.is_empty() {
            options.prompt = Some(context.to_owned());
        }
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match session.run(samples, &options) {
            Ok(transcript) => transcript.text.trim().to_owned(),
            Err(error) => {
                eprintln!("Voiceless: GGUF decode failed: {error}");
                String::new()
            }
        }
    }

    fn device(&self) -> &InferenceDevice {
        &self.device
    }
}

/// The transcribe.cpp request for a device.
fn backend(device: ComputeDevice) -> Backend {
    match device {
        ComputeDevice::Cpu => Backend::Cpu,
        ComputeDevice::Cuda => Backend::Cuda,
        ComputeDevice::Vulkan => Backend::Vulkan,
        ComputeDevice::Auto => Backend::Auto,
    }
}

/// The load or warm-up failure of a model that was requested on
/// `requested`.
fn load_error(
    error: transcribe_cpp::Error,
    path: &Path,
    requested: ComputeDevice,
) -> OfflineAsrError {
    match error {
        // The engine rejects a device it cannot use with `Backend` and no
        // useful text, so the cause comes from what it has instead.
        transcribe_cpp::Error::Backend(_) => OfflineAsrError::DeviceUnavailable {
            model: path.to_owned(),
            requested,
            cause: requested
                .unavailable_cause(backend_available(backend(requested)))
                .to_owned(),
        },
        error => OfflineAsrError::EngineUnavailable(format!(
            "cannot load {} on {}: {error}",
            path.display(),
            requested
        )),
    }
}

/// The identity of a loaded model, and the proof that it is a speech
/// recognition model: it must serve the ASR role and report the audio it
/// takes. A VAD, a language-ID or a text model fails here, before a
/// session is opened.
fn identity_of(model: &Model, path: &Path) -> Result<GgufIdentity, OfflineAsrError> {
    let unusable = |detail: &str| {
        OfflineAsrError::EngineUnavailable(format!(
            "{} is not a usable speech model: {detail}",
            path.display()
        ))
    };
    if !model.roles().contains(Role::Asr) {
        return Err(unusable(
            "the engine has no speech recognition for this kind of model",
        ));
    }
    let capabilities = model
        .capabilities()
        .map_err(|error| unusable(&format!("cannot read its audio capabilities: {error}")))?;
    if capabilities.native_sample_rate <= 0 {
        return Err(unusable("it reports no audio sample rate"));
    }
    Ok(GgufIdentity {
        architecture: model.arch(),
        variant: model.variant(),
        native_sample_rate: capabilities.native_sample_rate,
        languages: capabilities.languages,
        max_audio_ms: capabilities.max_audio_ms,
    })
}

/// The device facts of a loaded model, asked from the model itself: the
/// backend it is bound to and the device that owns its weights. Nothing
/// here is inferred from the request or from the GPUs a machine has.
fn reported_device(
    model: &Model,
    requested: ComputeDevice,
    path: &Path,
) -> Result<InferenceDevice, OfflineAsrError> {
    let backend = model.backend();
    if backend.is_empty() {
        return Err(unreadable("backend", "the model is bound to none", path));
    }
    let device = match model.device() {
        Ok(device) => device,
        Err(error) => return Err(unreadable("device", error, path)),
    };
    let api = match device.kind.as_str() {
        "cpu" => ComputeApi::Cpu,
        "cuda" => ComputeApi::Cuda,
        "vulkan" => ComputeApi::Vulkan,
        _ => ComputeApi::Other,
    };
    // `auto` prefers a GPU: under `auto`, a CPU model is the fallback.
    let cpu_fallback = if requested == ComputeDevice::Auto && api == ComputeApi::Cpu {
        Some(CpuFallback::for_cpu_model(gpu_built_in(), gpu_registered()))
    } else {
        None
    };
    Ok(InferenceDevice {
        requested,
        engine: InferenceEngine::TranscribeCpp,
        api,
        backend,
        name: Some(device.name),
        description: Some(device.description),
        device_id: device.device_id,
        memory_bytes: device.memory_total,
        cpu_fallback,
    })
}

/// The engine loaded the model but does not say where it runs.
fn unreadable(what: &str, detail: impl std::fmt::Display, path: &Path) -> OfflineAsrError {
    OfflineAsrError::EngineUnavailable(format!(
        "cannot read the {what} of {} after load: {detail}",
        path.display()
    ))
}

/// Whether this build carries a GPU engine for transcribe.cpp.
fn gpu_built_in() -> bool {
    cfg!(any(feature = "cuda", feature = "vulkan"))
}

/// Whether the engine registered a GPU device. Read once `auto` has
/// loaded a model onto the CPU: `auto` takes the first registered GPU
/// device that initializes, so this tells "no GPU found" from "found but
/// not initialized". It is never evidence that a model runs on a GPU.
fn gpu_registered() -> bool {
    devices()
        .iter()
        .any(|device| matches!(device.device_type, DeviceType::Gpu | DeviceType::Igpu))
}
