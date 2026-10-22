//! Warm recognition engines. A resident daemon loads the configured local
//! model once and hands it to every session, so a hotkey press never waits
//! for a model load. One-shot `voiceless stream` runs load on demand.
//!
//! `daemon.rs` builds a `WarmEngine` at start-up (and again on an engine
//! reload) and lends it to each session through `stream::SessionEnv::warm`;
//! `stream::open_local_offline` takes the decoder from it when the
//! `[local]` settings still match (`offline_for`) and otherwise loads
//! cold. Only the `local-offline` backend (`asr.streaming_backend`) has
//! anything to keep warm. Models come from the managed catalog
//! (`models::installed_model_dir`), verified once here so sessions skip the
//! hash check of the multi-gigabyte payload.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{AppConfig, LocalConfig};
use crate::offline_asr::{create_decoder, Decoder, OfflineFamily, OfflineOptions};

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
        /// Verified once at load; sessions skip the per-session hash check.
        vad_model: PathBuf,
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
                let family =
                    OfflineFamily::for_model(&local.model).map_err(|error| error.to_string())?;
                let recognizer = create_decoder(family, &model_dir, &offline_options(&local))
                    .map_err(|error| error.to_string())?;
                Ok(Some(Self::Offline {
                    local,
                    recognizer,
                    vad_model,
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
            } if loaded.model == local.model
                && loaded.device() == local.device()
                && loaded.num_threads == local.num_threads
                && loaded.hotwords == local.hotwords =>
            {
                Some((recognizer.clone(), vad_model.clone()))
            }
            Self::Offline { .. } => None,
        }
    }

    /// Catalog ids this engine keeps loaded (model and VAD).
    pub fn model_ids(&self) -> Vec<String> {
        match self {
            Self::Offline { local, .. } => vec![local.model.clone(), "silero-vad".to_owned()],
        }
    }

    /// The warm decoder and VAD model, for offline jobs (re-transcription,
    /// benchmarks) and the text cleanup config of the session. The third
    /// element is the VAD segment cap in seconds (`LocalConfig::segment_limit`).
    pub fn decoder(&self) -> (Arc<dyn Decoder>, PathBuf, f32) {
        match self {
            Self::Offline {
                local,
                recognizer,
                vad_model,
            } => (recognizer.clone(), vad_model.clone(), local.segment_limit()),
        }
    }

    /// Human-readable `<model> on <device>` for status output and logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Offline { local, .. } => format!("{} on {}", local.model, local.device()),
        }
    }
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
