//! GGUF ASR models (Qwen3-ASR 1.7B, …) on transcribe.cpp/ggml. On the CUDA
//! build the model runs on the GPU: Qwen3-ASR-1.7B Q8_0 decodes 8 s of
//! speech in ~0.2 s on an RTX 3070 and holds ~2.5 GB of VRAM.

use std::path::Path;
use std::sync::Mutex;

use transcribe_cpp::{Backend, Feature, Model, ModelOptions, RunOptions, Session};

use crate::offline_asr::{Decoder, OfflineAsrError, OfflineOptions};

/// Catalog GGUF models install as this single file.
pub const MODEL_FILE: &str = "model.gguf";

pub struct GgufDecoder {
    /// One session per model: sessions decode one segment at a time.
    session: Mutex<Session>,
    options: RunOptions,
    context_prompt: bool,
}

impl GgufDecoder {
    pub fn load(path: &Path, options: &OfflineOptions) -> Result<Self, OfflineAsrError> {
        if !path.is_file() {
            return Err(OfflineAsrError::MissingModelFile(path.to_owned()));
        }
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
