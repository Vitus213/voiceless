//! Warm recognition engines. A resident daemon loads the configured local
//! model once and hands it to every session, so a hotkey press never waits
//! for a model load. One-shot `voiceless stream` runs load on demand.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{AppConfig, LocalConfig};
use crate::offline_asr::{create_recognizer, OfflineFamily, OfflineOptions};

/// An engine kept loaded across sessions.
pub enum WarmEngine {
    Offline {
        local: LocalConfig,
        recognizer: Arc<sherpa_onnx::OfflineRecognizer>,
        /// Verified once at load; sessions skip the per-session hash check.
        vad_model: PathBuf,
    },
}

impl WarmEngine {
    /// Load the engine the configured streaming backend needs, if it has a
    /// loadable model. Cloud backends have nothing to keep warm.
    pub fn load(config: &AppConfig) -> Result<Option<Self>, String> {
        match config.asr.streaming_backend.as_str() {
            "local-offline" => {
                let local = config.local.clone();
                let model_dir = crate::models::installed_model_dir(&local.model)?;
                let vad_model =
                    crate::models::installed_model_dir("silero-vad")?.join("silero_vad.onnx");
                let family =
                    OfflineFamily::for_model(&local.model).map_err(|error| error.to_string())?;
                let recognizer = create_recognizer(family, &model_dir, &offline_options(&local))
                    .map_err(|error| error.to_string())?;
                Ok(Some(Self::Offline {
                    local,
                    recognizer: Arc::new(recognizer),
                    vad_model,
                }))
            }
            _ => Ok(None),
        }
    }

    /// The warm offline recognizer and VAD model path, when the recognizer
    /// was built for the same model, device and hotwords as `local`.
    pub fn offline_for(
        &self,
        local: &LocalConfig,
    ) -> Option<(Arc<sherpa_onnx::OfflineRecognizer>, PathBuf)> {
        match self {
            Self::Offline {
                local: loaded,
                recognizer,
                vad_model,
            } if loaded.model == local.model
                && loaded.provider == local.provider
                && loaded.num_threads == local.num_threads
                && loaded.hotwords == local.hotwords =>
            {
                Some((recognizer.clone(), vad_model.clone()))
            }
            Self::Offline { .. } => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Offline { local, .. } => format!("{} on {}", local.model, local.provider),
        }
    }
}

pub fn offline_options(local: &LocalConfig) -> OfflineOptions {
    OfflineOptions {
        provider: local.provider.clone(),
        num_threads: local.num_threads,
        hotwords: local.hotwords.clone(),
    }
}
