//! GGUF ASR models (Qwen3-ASR 1.7B, …) on transcribe.cpp/ggml. On the CUDA
//! build the model runs on the GPU: Qwen3-ASR-1.7B Q8_0 decodes 8 s of
//! speech in ~0.2 s on an RTX 3070 and holds ~2.5 GB of VRAM.
//!
//! This is the GGUF `Decoder` of the `local-offline` backend
//! (`[asr] streaming_backend = "local-offline"` with a `[local] model`
//! ending in `-gguf`: `qwen3-asr-1.7b-gguf`, `qwen3-asr-0.6b-gguf`;
//! `[local] provider = "cuda" | "cpu"`, default cuda). The module is
//! compiled only with the `transcribe` cargo feature (`cuda` adds the GPU
//! build of transcribe.cpp); the Nix `voiceless` package enables it and
//! `voiceless-cpu` does not, in which case `offline_asr::create_decoder`
//! reports the GGUF family as unavailable. Only `create_decoder` constructs
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
//! Failure semantics: a missing file or a load/warm-up failure is an
//! `OfflineAsrError` (`MissingModelFile`, `EngineUnavailable`) that the
//! session reports once as an error; a decode failure is logged to stderr
//! and yields `""` for that segment, so the session continues and that
//! segment's text is simply absent. No retries.

use std::path::Path;
use std::sync::Mutex;

use transcribe_cpp::{Backend, Feature, Model, ModelOptions, RunOptions, Session};

use crate::offline_asr::{Decoder, OfflineAsrError, OfflineOptions};

/// Catalog GGUF models install as this single file.
pub const MODEL_FILE: &str = "model.gguf";

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
}

impl GgufDecoder {
    /// Load `path` on the device named by `options.provider` (`"cpu"` is
    /// the CPU backend; anything else selects CUDA, with the default
    /// device) and run one second of silence through it so kernel and
    /// buffer setup is paid here rather than on the first utterance.
    /// Blocking: seconds for a cold ~2 GB load. Errors: `MissingModelFile`
    /// when `path` is not a file; `EngineUnavailable` (naming path and
    /// provider) when the engine cannot load the model, open a session or
    /// complete the warm-up, e.g. no CUDA device on a `cuda` provider.
    pub fn load(path: &Path, options: &OfflineOptions) -> Result<Self, OfflineAsrError> {
        if !path.is_file() {
            return Err(OfflineAsrError::MissingModelFile(path.to_owned()));
        }
        // `config.rs` only allows cpu | cuda, so the fallthrough is cuda.
        let backend = match options.provider.as_str() {
            "cpu" => Backend::Cpu,
            _ => Backend::Cuda,
        };
        let unavailable = |error: transcribe_cpp::Error| {
            OfflineAsrError::EngineUnavailable(format!(
                "cannot load {} on {}: {error}",
                path.display(),
                options.provider
            ))
        };
        let model = Model::load_with(
            path,
            &ModelOptions {
                backend,
                device: None,
            },
        )
        .map_err(unavailable)?;
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
        let mut session = model.session().map_err(unavailable)?;
        // First run pays kernel and buffer setup; do it before any session.
        session.run(&vec![0.0; 16_000], &run).map_err(unavailable)?;
        Ok(Self {
            session: Mutex::new(session),
            options: run,
            context_prompt,
        })
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
}
