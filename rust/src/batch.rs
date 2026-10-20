//! Batch transcription: WAV parsing plus cloud (DashScope multimodal
//! generation) and local (Sherpa offline SenseVoice) backends.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;

const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("wav file invalid: {0}")]
    Wav(String),
    #[error("cloud transcription failed: {0}")]
    Cloud(String),
    #[error("cloud authentication failed: HTTP {0}")]
    CloudAuth(u16),
    #[error("local transcription failed: {0}")]
    Local(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResult {
    pub text: String,
    pub backend: String,
}

/// Load PCM16 samples from a 16 kHz mono uncompressed WAV. The `data` chunk
/// length is clamped to the file end so RF64-style placeholder sizes still
/// decode.
pub fn load_wav_pcm16(path: &Path) -> Result<Vec<u8>, BatchError> {
    let bytes = fs::read(path)?;
    let error = |message: String| BatchError::Wav(message);
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(error(format!("not a RIFF/WAVE file: {}", path.display())));
    }
    let mut cursor = 12;
    let mut fmt_seen = false;
    let mut audio_format: u16 = 0;
    let mut channels: u16 = 0;
    let mut rate: u32 = 0;
    let mut bits: u16 = 0;
    while cursor + 8 <= bytes.len() {
        let chunk_id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body_start = cursor + 8;
        let body_end = (body_start + size).min(bytes.len());
        match chunk_id {
            b"fmt " => {
                if body_end - body_start < 16 {
                    return Err(error("fmt chunk too short".to_owned()));
                }
                audio_format = u16::from_le_bytes([bytes[body_start], bytes[body_start + 1]]);
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
                rate = u32::from_le_bytes([
                    bytes[body_start + 4],
                    bytes[body_start + 5],
                    bytes[body_start + 6],
                    bytes[body_start + 7],
                ]);
                bits = u16::from_le_bytes([bytes[body_start + 14], bytes[body_start + 15]]);
                fmt_seen = true;
            }
            b"data" => {
                if !fmt_seen {
                    return Err(error("data chunk before fmt chunk".to_owned()));
                }
                if audio_format != 1 {
                    return Err(error("wav must be uncompressed PCM".to_owned()));
                }
                if channels != 1 {
                    return Err(error("wav must be mono".to_owned()));
                }
                if rate != SAMPLE_RATE {
                    return Err(error(format!(
                        "wav sample rate must be {SAMPLE_RATE} Hz, got {rate}"
                    )));
                }
                if bits != 16 {
                    return Err(error("wav must be PCM16".to_owned()));
                }
                let mut pcm = bytes[body_start..body_end].to_vec();
                if pcm.len() % 2 != 0 {
                    pcm.pop();
                }
                return Ok(pcm);
            }
            _ => {}
        }
        cursor = body_start + size;
        if cursor % 2 != 0 {
            cursor += 1;
        }
    }
    Err(error("no data chunk found".to_owned()))
}

/// HTTP boundary for the batch endpoint; injectable for fixture tests.
pub trait BatchPoster {
    fn post_json(
        &self,
        url: &str,
        body: &str,
        bearer: &str,
        timeout: Duration,
    ) -> Result<(u16, String), BatchError>;
}

pub struct UreqBatchPoster;

impl BatchPoster for UreqBatchPoster {
    fn post_json(
        &self,
        url: &str,
        body: &str,
        bearer: &str,
        timeout: Duration,
    ) -> Result<(u16, String), BatchError> {
        let request = ureq::post(url)
            .timeout(timeout)
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Bearer {bearer}"));
        match request.send_string(body) {
            Ok(response) => {
                let status = response.status();
                let body = response
                    .into_string()
                    .map_err(|error| BatchError::Cloud(error.to_string()))?;
                Ok((status, body))
            }
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Ok((status, body))
            }
            Err(error) => Err(BatchError::Cloud(error.to_string())),
        }
    }
}

const GENERATION_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

pub struct CloudBatchClient<P: BatchPoster> {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    poster: P,
}

impl<P: BatchPoster> CloudBatchClient<P> {
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        timeout: Duration,
        poster: P,
    ) -> Self {
        Self {
            base_url,
            api_key,
            model,
            timeout,
            poster,
        }
    }

    /// Transcribe a whole WAV payload (bytes already decoded from disk).
    /// Retries 429/5xx and network errors up to three times; 401/403 are
    /// authentication errors.
    pub fn transcribe_wav_bytes(&self, wav_bytes: &[u8]) -> Result<String, BatchError> {
        if self.api_key.trim().is_empty() {
            return Err(BatchError::CloudAuth(0));
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(wav_bytes);
        let content = serde_json::json!([{ "audio": format!("data:audio/wav;base64,{encoded}") }]);
        let payload = serde_json::json!({
            "model": self.model,
            "input": { "messages": [{ "role": "user", "content": content }] },
        });
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), GENERATION_PATH);
        let mut last_error = BatchError::Cloud("no attempts made".to_owned());
        for attempt in 0..3 {
            match self
                .poster
                .post_json(&url, &payload.to_string(), &self.api_key, self.timeout)
            {
                Ok((200, body)) => return extract_text(&body),
                Ok((status, _body)) if status == 401 || status == 403 => {
                    return Err(BatchError::CloudAuth(status))
                }
                Ok((status, _body)) if status == 429 || status >= 500 => {
                    last_error =
                        BatchError::Cloud(format!("HTTP {status} (attempt {})", attempt + 1));
                    std::thread::sleep(Duration::from_millis(250 * 2_u64.pow(attempt)));
                }
                Ok((status, _body)) => return Err(BatchError::Cloud(format!("HTTP {status}"))),
                Err(error) => {
                    last_error = error;
                    std::thread::sleep(Duration::from_millis(250 * 2_u64.pow(attempt)));
                }
            }
        }
        Err(last_error)
    }
}

fn extract_text(body: &str) -> Result<String, BatchError> {
    let parsed: serde_json::Value =
        serde_json::from_str(body).map_err(|error| BatchError::Cloud(error.to_string()))?;
    let content = &parsed["output"]["choices"][0]["message"]["content"];
    let Some(items) = content.as_array() else {
        return Err(BatchError::Cloud(
            "response has no content array".to_owned(),
        ));
    };
    if items.is_empty() {
        return Ok(String::new());
    }
    items[0]["text"]
        .as_str()
        .map(str::trim)
        .map(ToOwned::to_owned)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| BatchError::Cloud("response text is empty".to_owned()))
}

/// Local batch recognizer: Sherpa offline SenseVoice.
pub struct LocalBatchRecognizer {
    recognizer: sherpa_onnx::OfflineRecognizer,
}

impl LocalBatchRecognizer {
    pub fn new(model_dir: &Path) -> Result<Self, BatchError> {
        let model = model_dir.join("model.int8.onnx");
        let tokens = model_dir.join("tokens.txt");
        for path in [&model, &tokens] {
            if !path.is_file() {
                return Err(BatchError::Local(format!(
                    "local model file missing: {}",
                    path.display()
                )));
            }
        }
        let mut config = sherpa_onnx::OfflineRecognizerConfig::default();
        config.model_config.sense_voice.model = Some(path_string(&model)?);
        config.model_config.tokens = Some(path_string(&tokens)?);
        config.model_config.num_threads = 1;
        let recognizer = sherpa_onnx::OfflineRecognizer::create(&config).ok_or_else(|| {
            BatchError::Local(format!(
                "cannot create offline recognizer from {}",
                model_dir.display()
            ))
        })?;
        Ok(Self { recognizer })
    }

    pub fn transcribe_pcm16(&self, pcm: &[u8]) -> Result<String, BatchError> {
        let samples: Vec<f32> = pcm
            .chunks_exact(2)
            .map(|bytes| f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / f32::from(i16::MAX))
            .collect();
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, &samples);
        self.recognizer.decode(&stream);
        let text = stream
            .get_result()
            .map(|result| result.text.trim().to_owned())
            .unwrap_or_default();
        Ok(text)
    }
}

fn path_string(path: &Path) -> Result<String, BatchError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| BatchError::Local("model path is not valid UTF-8".to_owned()))
}

/// Transcribe one WAV through the selected backend.
pub fn transcribe(
    wav_path: &Path,
    backend: &str,
    config: &crate::config::AppConfig,
    model_dir: Option<&Path>,
) -> Result<BatchResult, BatchError> {
    let pcm = load_wav_pcm16(wav_path)?;
    match backend {
        "cloud" => {
            let client = CloudBatchClient::new(
                config.cloud.base_url.clone(),
                config.cloud.api_key.clone(),
                config.cloud.model.clone(),
                Duration::from_secs_f64(config.cloud.timeout_seconds),
                UreqBatchPoster,
            );
            let wav_bytes = fs::read(wav_path)?;
            let text = client.transcribe_wav_bytes(&wav_bytes)?;
            Ok(BatchResult {
                text,
                backend: "cloud".to_owned(),
            })
        }
        "sensevoice" | "local" => {
            let dir = model_dir.ok_or_else(|| {
                BatchError::Local("local batch transcription requires a model directory".to_owned())
            })?;
            let recognizer = LocalBatchRecognizer::new(dir)?;
            let text = recognizer.transcribe_pcm16(&pcm)?;
            Ok(BatchResult {
                text,
                backend: backend.to_owned(),
            })
        }
        other => Err(BatchError::Local(format!(
            "batch backend not implemented in this build: {other}"
        ))),
    }
}

/// Record N seconds to a WAV file via `pw-record`.
pub fn record_seconds(seconds: f64, destination: &Path) -> Result<(), BatchError> {
    let status = std::process::Command::new("pw-record")
        .args([
            "--rate",
            &SAMPLE_RATE.to_string(),
            "--channels",
            "1",
            "--format",
            "s16",
        ])
        .arg("--file-format=wav")
        .arg(destination)
        .stdin(std::process::Stdio::null())
        .status()?;
    let _ = seconds;
    if !status.success() {
        return Err(BatchError::Local(format!("pw-record exited with {status}")));
    }
    Ok(())
}

pub fn temp_wav_path() -> PathBuf {
    std::env::temp_dir().join(format!("voiceless-record-{}.wav", std::process::id()))
}
