//! Warm recognition engines. A resident daemon loads the configured local
//! model once and hands it to every session, so a hotkey press never waits
//! for a model load. One-shot `voiceless stream` runs load on demand.
//!
//! `daemon.rs` builds a `WarmEngine` at start-up (and again on an engine
//! reload) and lends it to each session through `stream::SessionEnv::warm`.
//! Sessions reuse a loaded model when their configuration matches. Managed
//! models are verified at load time so a warm session skips the payload hash.
//!
//! Every engine knows where its models really run: `WarmEngine::devices`
//! reports the facts the decoders read from their loaded models, so status
//! output never has to trust the configured device name.
//!
//! An engine also holds a lease on the models its configuration uses
//! (`models::ModelLeases`) for as long as it lives, so neither
//! `voiceless model remove` nor the console can delete them. A session
//! leases the models it may load the same way (`model_ids_for`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{AppConfig, LocalConfig};
use crate::local_asr::LocalStreamingRecognizer;
use crate::models::{lease_models, ModelLeases};
use crate::offline_asr::{
    create_decoder, Decoder, EngineDevices, InferenceDevice, OfflineAsrError, OfflineFamily,
    OfflineOptions,
};

/// An engine kept loaded across sessions.
pub enum WarmEngine {
    /// `local-offline`: the GPU/CPU decoder plus the Silero VAD model.
    Offline {
        /// The `[local]` settings (with `[text].hotwords` merged) the
        /// decoder was built with; sessions compare against them before
        /// reusing it.
        local: LocalConfig,
        /// Shared decoder, cloned (`Arc`) into each session.
        recognizer: Arc<dyn Decoder>,
        /// Optional CPU decoder for live previews.
        preview: Option<Arc<dyn Decoder>>,
        /// Verified once at load; sessions skip the per-session hash check.
        vad_model: PathBuf,
        /// The models of this configuration, leased until the engine is
        /// dropped.
        leases: ModelLeases,
    },
    /// `local-streaming`: the loaded Paraformer model, without session state.
    Streaming {
        model_dir: Option<PathBuf>,
        recognizer: Arc<sherpa_onnx::OnlineRecognizer>,
        /// Fixed at load: `LocalStreamingRecognizer::load_model` creates the
        /// model on the CPU and Sherpa-ONNX has no device read-back.
        device: InferenceDevice,
        /// The models of this configuration, leased until the engine is
        /// dropped.
        leases: ModelLeases,
    },
}

impl WarmEngine {
    /// Load the engine the configured streaming backend needs, if it has a
    /// loadable model. Cloud backends have nothing to keep warm. The models
    /// of the configuration are leased before anything is read
    /// (`models::lease_models`) and stay leased for as long as the engine
    /// lives.
    ///
    /// Blocking: the model load runs synchronously on the caller's thread.
    /// Errors are human-readable (unknown, missing or corrupt model or VAD,
    /// unsupported model family, a device the engine cannot use, decoder
    /// construction failure).
    pub fn load(config: &AppConfig) -> Result<Option<Self>, String> {
        let backend = config.asr.streaming_backend.as_str();
        match backend {
            "local-offline" => {
                let local = config.local_engine();
                // Leased before the integrity check and the load, so a
                // removal cannot slip in between.
                let leases = lease_models(model_ids_for(config, backend));
                let model_dir = crate::models::installed_model_dir(&local.model)?;
                let vad_model =
                    crate::models::installed_model_dir("silero-vad")?.join("silero_vad.onnx");
                let preview_dir = local
                    .preview_model()
                    .map(crate::models::installed_model_dir)
                    .transpose()?;
                let family =
                    OfflineFamily::for_model(&local.model).map_err(|error| error.to_string())?;
                let recognizer = create_decoder(family, &model_dir, &offline_options(&local))
                    .map_err(|error| error.to_string())?;
                let preview = preview_dir
                    .as_deref()
                    .map(|path| create_preview_decoder(path, local.num_threads))
                    .transpose()
                    .map_err(|error| error.to_string())?;
                Ok(Some(Self::Offline {
                    local,
                    recognizer,
                    preview,
                    vad_model,
                    leases,
                }))
            }
            "local-streaming" => {
                let leases = lease_models(model_ids_for(config, backend));
                let model_dir = config.asr.local_model_dir.clone();
                let recognizer = load_streaming_model(model_dir.as_deref())?;
                Ok(Some(Self::Streaming {
                    model_dir,
                    recognizer,
                    device: InferenceDevice::sherpa_cpu(),
                    leases,
                }))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn matches_config(&self, config: &AppConfig) -> bool {
        match self {
            Self::Offline { local, .. } => {
                config.asr.streaming_backend == "local-offline"
                    && matches_offline_config(local, &config.local_engine())
            }
            Self::Streaming { model_dir, .. } => {
                config.asr.streaming_backend == "local-streaming"
                    && model_dir == &config.asr.local_model_dir
            }
        }
    }

    /// The warm offline recognizer and VAD model path, when the recognizer
    /// was built for the same model, device and hotwords as `local`.
    pub fn offline_for(&self, local: &LocalConfig) -> Option<(Arc<dyn Decoder>, PathBuf)> {
        match self {
            Self::Offline {
                local: loaded,
                recognizer,
                vad_model,
                ..
            } if matches_offline_config(loaded, local) => {
                Some((recognizer.clone(), vad_model.clone()))
            }
            _ => None,
        }
    }

    pub(crate) fn preview_decoder(&self) -> Option<Arc<dyn Decoder>> {
        match self {
            Self::Offline { preview, .. } => preview.clone(),
            Self::Streaming { .. } => None,
        }
    }

    pub(crate) fn streaming_for(
        &self,
        directory: Option<&Path>,
    ) -> Option<LocalStreamingRecognizer> {
        match self {
            Self::Streaming {
                model_dir,
                recognizer,
                ..
            } if model_dir.as_deref() == directory => {
                Some(LocalStreamingRecognizer::from_model(Arc::clone(recognizer)))
            }
            _ => None,
        }
    }

    /// Ids of the models this engine's configuration keeps leased: the
    /// offline model, the VAD, the preview model and the speech denoiser,
    /// or the catalog's streaming model.
    pub fn model_ids(&self) -> Vec<String> {
        match self {
            Self::Offline { local, .. } => offline_model_ids(local),
            Self::Streaming {
                model_dir: None, ..
            } => vec![crate::models::streaming_paraformer_spec().id],
            Self::Streaming { .. } => Vec::new(),
        }
    }

    /// The warm decoder and VAD model, for offline jobs (re-transcription,
    /// benchmarks) and the text cleanup config of the session. The third
    /// element is the VAD segment cap in seconds (`LocalConfig::segment_limit`).
    pub(crate) fn offline_decoder(&self) -> Option<(Arc<dyn Decoder>, PathBuf, f32)> {
        match self {
            Self::Offline {
                local,
                recognizer,
                vad_model,
                ..
            } => Some((recognizer.clone(), vad_model.clone(), local.segment_limit())),
            Self::Streaming { .. } => None,
        }
    }

    /// Where the loaded models run, as their engines reported them after
    /// the load: the primary decoder and, when previews use a separate
    /// model, that decoder. Read from the loaded models, never from the
    /// configuration, and not recomputed: cheap enough for status output.
    pub fn devices(&self) -> EngineDevices {
        match self {
            Self::Offline {
                recognizer,
                preview,
                ..
            } => EngineDevices::of_decoders(recognizer.as_ref(), preview.as_deref()),
            Self::Streaming { device, .. } => EngineDevices {
                primary: device.clone(),
                preview: None,
            },
        }
    }

    /// Human-readable `<model> on <device>` for status output and logs.
    /// The device is where the model really runs, not the request: `auto`
    /// shows the GPU it resolved to, or the CPU with the reason.
    pub fn describe(&self) -> String {
        match self {
            Self::Offline {
                local, recognizer, ..
            } => {
                format!("{} on {}", local.model, recognizer.device())
            }
            Self::Streaming {
                model_dir, device, ..
            } => {
                let source = model_dir.as_ref().map_or_else(
                    || crate::models::streaming_paraformer_spec().id,
                    |dir| dir.display().to_string(),
                );
                format!("{source} on {device}")
            }
        }
    }
}

/// The models an offline engine needs for `[local]`: the model, the VAD,
/// the preview model and, when `[local].denoise` is set, the speech
/// denoiser every session loads.
fn offline_model_ids(local: &LocalConfig) -> Vec<String> {
    let mut ids = vec![local.model.clone(), "silero-vad".to_owned()];
    ids.extend(local.preview_model().map(str::to_owned));
    if local.denoise {
        ids.push(crate::models::gtcrn_spec().id);
    }
    ids
}

/// Ids of the models the engine of `backend` takes from the store for
/// `config`: the model, the VAD, the preview model and the speech denoiser
/// for `local-offline`; the catalog's streaming model for
/// `local-streaming` without `[asr].local_model_dir`. Empty for a cloud
/// backend or a directory the user named. Warm engines and sessions lease
/// them for as long as they run (`models::lease_models`), so nothing
/// deletes a model that is about to be loaded.
pub fn model_ids_for(config: &AppConfig, backend: &str) -> Vec<String> {
    match backend {
        "local-offline" => offline_model_ids(&config.local_engine()),
        "local-streaming" if config.asr.local_model_dir.is_none() => {
            vec![crate::models::streaming_paraformer_spec().id]
        }
        _ => Vec::new(),
    }
}

fn matches_offline_config(loaded: &LocalConfig, requested: &LocalConfig) -> bool {
    loaded.model == requested.model
        && loaded.device() == requested.device()
        && loaded.num_threads == requested.num_threads
        && loaded.hotwords == requested.hotwords
        && loaded.preview_model() == requested.preview_model()
}

pub(crate) fn load_streaming_model(
    directory: Option<&Path>,
) -> Result<Arc<sherpa_onnx::OnlineRecognizer>, String> {
    let managed;
    let directory = match directory {
        Some(path) => path,
        None => {
            managed =
                crate::models::installed_model_dir(&crate::models::streaming_paraformer_spec().id)?;
            &managed
        }
    };
    LocalStreamingRecognizer::load_model(directory).map_err(|error| error.to_string())
}

/// Decoder options derived from `[local]`: compute provider (device),
/// thread count and hotwords. Shared by the warm load and cold sessions so
/// both build identical decoders.
pub fn offline_options(local: &LocalConfig) -> OfflineOptions {
    OfflineOptions {
        provider: local.device(),
        num_threads: local.num_threads,
        hotwords: local.hotwords.clone(),
    }
}

/// Use the same CPU preview recognizer in warm and one-shot sessions.
pub(crate) fn create_preview_decoder(
    model_dir: &Path,
    num_threads: i32,
) -> Result<Arc<dyn Decoder>, OfflineAsrError> {
    create_decoder(
        OfflineFamily::SenseVoice,
        model_dir,
        &OfflineOptions {
            provider: "cpu".to_owned(),
            num_threads,
            hotwords: Vec::new(),
        },
    )
}
