//! Warm recognition engines. A resident daemon loads the configured local
//! model once and hands it to every session, so a hotkey press never waits
//! for a model load. One-shot `voiceless stream` runs load on demand.
//!
//! `daemon.rs` builds a `WarmEngine` at start-up (and again on an engine
//! reload) and lends it to each session through `stream::SessionEnv::warm`.
//! Sessions reuse a loaded model when their configuration matches. Managed
//! models are verified at load time so a warm session skips the payload hash.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{AppConfig, LocalConfig};
use crate::local_asr::LocalStreamingRecognizer;
use crate::offline_asr::{create_decoder, Decoder, OfflineAsrError, OfflineFamily, OfflineOptions};

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
    },
    /// `local-streaming`: the loaded Paraformer model, without session state.
    Streaming {
        model_dir: Option<PathBuf>,
        recognizer: Arc<sherpa_onnx::OnlineRecognizer>,
    },
}

impl WarmEngine {
    /// Load the engine the configured streaming backend needs, if it has a
    /// loadable model. Cloud backends have nothing to keep warm.
    ///
    /// Blocking: the model load runs synchronously on the caller's thread.
    /// Errors are human-readable (unknown, missing or corrupt model or VAD,
    /// unsupported model family, decoder construction failure).
    pub fn load(config: &AppConfig) -> Result<Option<Self>, String> {
        match config.asr.streaming_backend.as_str() {
            "local-offline" => {
                let local = config.local_engine();
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
                }))
            }
            "local-streaming" => {
                let model_dir = config.asr.local_model_dir.clone();
                let recognizer = load_streaming_model(model_dir.as_deref())?;
                Ok(Some(Self::Streaming {
                    model_dir,
                    recognizer,
                }))
            }
            _ => Ok(None),
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
            } if loaded.model == local.model
                && loaded.device() == local.device()
                && loaded.num_threads == local.num_threads
                && loaded.hotwords == local.hotwords
                && loaded.preview_model() == local.preview_model() =>
            {
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
            } if model_dir.as_deref() == directory => {
                Some(LocalStreamingRecognizer::from_model(Arc::clone(recognizer)))
            }
            _ => None,
        }
    }

    /// Catalog ids this engine keeps loaded (model and VAD).
    pub fn model_ids(&self) -> Vec<String> {
        match self {
            Self::Offline { local, .. } => {
                let mut ids = vec![local.model.clone(), "silero-vad".to_owned()];
                ids.extend(local.preview_model().map(str::to_owned));
                ids
            }
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

    /// Human-readable `<model> on <device>` for status output and logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Offline { local, .. } => format!("{} on {}", local.model, local.device()),
            Self::Streaming { model_dir, .. } => {
                let source = model_dir.as_ref().map_or_else(
                    || crate::models::streaming_paraformer_spec().id,
                    |dir| dir.display().to_string(),
                );
                format!("{source} on cpu")
            }
        }
    }
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
