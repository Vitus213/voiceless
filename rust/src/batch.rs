//! Batch transcription: WAV parsing plus cloud (DashScope multimodal
//! generation) and local (Sherpa offline SenseVoice) backends, selected by
//! `[asr] batch_backend = "cloud" | "local"` (`"sensevoice"` is accepted
//! as an alias of `local`). Used by `batch_cmd.rs` (`voiceless transcribe`,
//! `voiceless record`); `load_wav_pcm16` is also the WAV reader of
//! `benchmark_cmd.rs`, `latency_cmd.rs` and the audio-retention test.
//!
//! This is the non-realtime path: no coordinator, no `RealtimeEvent`s, no
//! ready gate and no partial text. One blocking HTTP request (cloud) or one
//! offline decode (local) returns the whole transcript, or an error. The
//! cloud backend reads `[cloud] base_url`, `api_key`, `model` and
//! `timeout_seconds` (the HTTP model, not `realtime_model`); the local
//! backend reads only `[asr] batch_model_dir`, a directory holding
//! `model.int8.onnx` and `tokens.txt`. There is no managed-catalog
//! resolution on this path: without `batch_model_dir` the local backend
//! fails with a "requires a model directory" error.
//!
//! Failure semantics: every error is a `BatchError`; the cloud client
//! retries transient HTTP failures up to three attempts and nothing here
//! ever switches backend. Everything blocks (ureq, Sherpa, the `pw-record`
//! child), so `batch_cmd.rs` runs it on `spawn_blocking`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;

const SAMPLE_RATE: u32 = 16_000;

/// Any batch-path failure, rendered as the `Voiceless: ...` stderr line.
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    /// The input is not a 16 kHz mono PCM16 RIFF/WAVE file.
    #[error("wav file invalid: {0}")]
    Wav(String),
    /// HTTP transport, non-2xx status or an unparsable DashScope response.
    #[error("cloud transcription failed: {0}")]
    Cloud(String),
    /// HTTP 401/403 from DashScope, or `0` when `[cloud] api_key` is empty
    /// (no request is made in that case).
    #[error("cloud authentication failed: HTTP {0}")]
    CloudAuth(u16),
    /// Local model problems, unknown backend names and `pw-record` exits.
    #[error("local transcription failed: {0}")]
    Local(String),
    /// Reading the WAV or spawning `pw-record` failed.
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// A finished batch transcription: the trimmed text (possibly empty for
/// silence) and the backend name as the caller selected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResult {
    pub text: String,
    pub backend: String,
}

/// Load PCM16 samples from a 16 kHz mono uncompressed WAV. The `data` chunk
/// length is clamped to the file end so RF64-style placeholder sizes still
/// decode.
///
/// Returns the raw little-endian sample bytes (an odd trailing byte is
/// dropped). Walks the RIFF chunk list, so extra chunks (`LIST`, `fact`)
/// before `data` are skipped. Errors are all `BatchError::Wav` with the
/// reason, or `Io` for an unreadable file. Reads the whole file into
/// memory.
pub fn load_wav_pcm16(path: &Path) -> Result<Vec<u8>, BatchError> {
    let bytes = fs::read(path)?;
    let error = |message: String| BatchError::Wav(message);
    // RIFF header: bytes 0..4 "RIFF", 4..8 file size (ignored), 8..12 "WAVE".
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(error(format!("not a RIFF/WAVE file: {}", path.display())));
    }
    let mut cursor = 12;
    let mut fmt_seen = false;
    let mut audio_format: u16 = 0;
    let mut channels: u16 = 0;
    let mut rate: u32 = 0;
    let mut bits: u16 = 0;
    // Each chunk: 4-byte id, u32 LE body size, body, then padding to an
    // even offset.
    while cursor + 8 <= bytes.len() {
        let chunk_id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body_start = cursor + 8;
        // Clamp: streaming writers (and RF64) leave 0xFFFFFFFF here.
        let body_end = (body_start + size).min(bytes.len());
        match chunk_id {
            b"fmt " => {
                // PCM fmt body: 0..2 format tag, 2..4 channels, 4..8 sample
                // rate, 8..12 byte rate, 12..14 block align, 14..16 bits.
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
                // Format tag 1 = integer PCM; the capture format exactly.
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
        // RIFF pads odd-sized chunk bodies with one byte.
        if cursor % 2 != 0 {
            cursor += 1;
        }
    }
    Err(error("no data chunk found".to_owned()))
}

/// HTTP boundary for the batch endpoint; injectable for fixture tests.
pub trait BatchPoster {
    /// POST `body` (JSON) to `url` with `Authorization: Bearer <bearer>`
    /// and the given overall timeout. Returns the HTTP status and body for
    /// any status (the client decides what a status means); `Err` only for
    /// transport failures. Blocking.
    fn post_json(
        &self,
        url: &str,
        body: &str,
        bearer: &str,
        timeout: Duration,
    ) -> Result<(u16, String), BatchError>;
}

/// Production `BatchPoster` over ureq (blocking, one connection per call).
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
            // ureq reports 4xx/5xx as errors; surface them as statuses so
            // the retry and auth logic sees them like any other response.
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Ok((status, body))
            }
            Err(error) => Err(BatchError::Cloud(error.to_string())),
        }
    }
}

/// DashScope multimodal generation endpoint, appended to `[cloud] base_url`.
const GENERATION_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

/// DashScope batch client: one multimodal-generation request per WAV.
/// Generic over the poster so tests script responses without a network.
pub struct CloudBatchClient<P: BatchPoster> {
    /// `[cloud] base_url`; a trailing `/` is tolerated.
    pub base_url: String,
    /// `[cloud] api_key`, sent as a bearer token; empty means no request.
    pub api_key: String,
    /// `[cloud] model` (the HTTP model, e.g. `qwen3-asr-flash-2026-02-10`).
    pub model: String,
    /// Per-attempt HTTP timeout (`[cloud] timeout_seconds`).
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
    ///
    /// The request body is `{"model", "input": {"messages": [{"role":
    /// "user", "content": [{"audio": "data:audio/wav;base64,..."}]}]}}`;
    /// the WAV bytes travel as a base64 data URL, so the file is the
    /// payload and this function never inspects it. Backoff between
    /// attempts is 250 ms, 500 ms, 1 s (the last sleep happens even when
    /// no attempt follows). Any other non-200 status fails at once. On
    /// exhaustion the last transient error is returned. Blocking.
    pub fn transcribe_wav_bytes(&self, wav_bytes: &[u8]) -> Result<String, BatchError> {
        // No key, no request: the error is the same auth failure a 401
        // would produce, without leaking an empty bearer to the network.
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
                // Bad or revoked key: retrying cannot help, and the
                // distinct error lets `doctor`/CLI messages say so.
                Ok((status, _body)) if status == 401 || status == 403 => {
                    return Err(BatchError::CloudAuth(status))
                }
                // Rate limit or server side: exponential backoff, then retry.
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

/// Pull the transcript out of a DashScope response:
/// `output.choices[0].message.content[0].text`, trimmed. An empty
/// `content` array means silence and yields `""`; a missing array or an
/// empty first text is a `Cloud` error (the service answered with
/// something that is not a transcript).
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
///
/// Independent of `offline_asr.rs`: it builds its own recognizer from
/// `model.int8.onnx` + `tokens.txt` with one thread, the default
/// provider and ITN left at Sherpa's default (off), and decodes the whole
/// file as one segment without VAD. Loading is blocking and the model is
/// dropped with the recognizer after each command.
pub struct LocalBatchRecognizer {
    recognizer: sherpa_onnx::OfflineRecognizer,
}

impl LocalBatchRecognizer {
    /// Load SenseVoice from `model_dir`. Errors (`BatchError::Local`): a
    /// missing `model.int8.onnx` or `tokens.txt` (named), a non-UTF-8
    /// path, or Sherpa refusing the model. Blocking.
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

    /// Decode PCM16 LE 16 kHz mono bytes (from `load_wav_pcm16`) in one
    /// pass and return the trimmed text, `""` for silence. Blocking; never
    /// fails (the `Result` keeps the signature uniform with the cloud path).
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

/// Sherpa takes `String` paths; non-UTF-8 paths are rejected explicitly.
fn path_string(path: &Path) -> Result<String, BatchError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| BatchError::Local("model path is not valid UTF-8".to_owned()))
}

/// Transcribe one WAV through the selected backend.
///
/// `backend` is `"cloud"`, `"local"` or its alias `"sensevoice"`; anything
/// else is a `Local` error ("not implemented in this build"). The WAV is
/// always validated with `load_wav_pcm16` first, so the cloud backend
/// never uploads a file the local path would reject; the cloud request
/// then sends the original file bytes (re-read from disk), the local path
/// decodes the parsed PCM. `model_dir` is `[asr] batch_model_dir` and is
/// required for the local backend. Blocking (network or model load plus
/// decode).
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
///
/// Spawns `pw-record --rate 16000 --channels 1 --format s16
/// --file-format=wav <destination>` with stdin closed and blocks until the
/// child exits; a non-zero exit is a `Local` error, a failed spawn an
/// `Io` error. The `seconds` argument is currently not passed to the
/// child (see the `let _ = seconds` below): no duration limit is applied
/// here, the recording lasts until `pw-record` itself exits.
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

/// Scratch WAV for `voiceless record`: `$TMPDIR/voiceless-record-<pid>.wav`
/// (unique per process, removed by `batch_cmd::record` afterwards).
pub fn temp_wav_path() -> PathBuf {
    std::env::temp_dir().join(format!("voiceless-record-{}.wav", std::process::id()))
}
