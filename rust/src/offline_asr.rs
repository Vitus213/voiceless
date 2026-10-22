//! Offline (non-streaming) models driven as a streaming backend: the
//! `local-offline` backend (`[asr] streaming_backend = "local-offline"`,
//! the one the daemon keeps warm). Models are Sherpa-ONNX archives
//! (`qwen3-asr-0.6b-int8`, `sense-voice-int8`; CPU only) or single-file
//! GGUF models on transcribe.cpp (`qwen3-asr-1.7b-gguf`,
//! `qwen3-asr-0.6b-gguf`; CUDA or CPU, see `gguf_asr.rs`, compiled with the
//! `transcribe` feature). Silero VAD splits speech at pauses: every closed
//! segment gets a final decode (`Completed`), and the open segment is
//! re-decoded every `partial_interval` for the live partial. Decoding runs
//! on a dedicated worker thread, so a slow decode never stalls audio
//! capture; queued audio is coalesced before the next partial.
//!
//! Config: `[local]` (`model`, `provider`, `num_threads`,
//! `partial_interval_ms`, `max_segment_seconds`, `hotwords`) merged with
//! `[text] hotwords` by `AppConfig::local_engine`; `LocalConfig::device`
//! and `segment_limit` supply the per-family defaults. The VAD model is the
//! catalog's `silero-vad`. Callers: `stream.rs` (`open_local_offline`
//! builds the `SegmentedRecognizer` inside `OfflineTransport::start`),
//! `engine.rs` (the daemon's warm `Decoder`, shared across sessions as an
//! `Arc`), `console/library.rs` (re-transcription and benchmark of saved
//! recordings through `SegmentedRecognizer` without partials) and the
//! real-model smoke test.
//!
//! Ready gate: `OfflineTransport::start` queues `Ready` before the worker
//! has loaded anything, so capture starts immediately and speech during a
//! cold model load is kept (queued, decoded once the model is up). A load
//! failure therefore arrives after capture started, as an `Error` event,
//! and does not trigger `fallback_backend`: `stream.rs` only falls back
//! when the model cannot be resolved before the transport starts. Partial
//! text is a re-decode of the whole open segment (pre-roll plus everything
//! since the last pause), so it may rewrite earlier words; confirmed text
//! is one `Completed` per VAD segment, decoded with the tail of the
//! confirmed text as context for decoders that support it. `finish` feeds
//! the remaining samples, flushes the VAD, decodes the segments that closes
//! and reports `Finished` with the concatenated confirmed text (which is
//! why `stream.rs` raises the coordinator's finish timeout to 20 s for this
//! backend). `cancel` stops the worker without decoding; nothing is
//! injected.
//!
//! Failure semantics: `create_decoder`, `create_recognizer` and
//! `create_vad` return `OfflineAsrError`; inside the transport such an
//! error surfaces once as `RealtimeEvent::Error` and ends the worker.
//! Decoders never fail a session: a decode error yields an empty string for
//! that segment (see `gguf_asr.rs`). No retries and no fallback here.

use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sherpa_onnx::{
    OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig, VadModelConfig,
    VoiceActivityDetector,
};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::realtime::RealtimeEvent;

const SAMPLE_RATE: i32 = 16_000;
/// Silero VAD frame: 512 samples = 32 ms at 16 kHz, the same duration as
/// one capture chunk.
const VAD_WINDOW: usize = 512;
/// Audio kept before detected speech so the first syllable is not clipped
/// (10 windows = 320 ms).
const PRE_ROLL_SAMPLES: usize = 10 * VAD_WINDOW;
/// Confirmed text handed to context-aware decoders, in characters.
const CONTEXT_CHARS: usize = 120;

/// One offline decode of a speech segment. `context` is the tail of the
/// text confirmed before this segment; decoders that support context
/// conditioning use it to keep segments coherent, others ignore it.
///
/// `samples` are 16 kHz mono f32 in [-1, 1]. Implementations are blocking
/// and must be safe to call from the worker thread while the daemon holds
/// another reference (`Send + Sync`); they return `""` on failure rather
/// than an error, so one bad segment never ends a session.
pub trait Decoder: Send + Sync {
    fn decode(&self, samples: &[f32], context: &str) -> String;
}

/// Sherpa offline recognizers ignore the context (no prompt input).
impl Decoder for OfflineRecognizer {
    fn decode(&self, samples: &[f32], _context: &str) -> String {
        decode(self, samples)
    }
}

/// Why a `local-offline` engine could not be built. Rendered verbatim as
/// the session error, so each variant names what to install or change.
#[derive(Debug, Error)]
pub enum OfflineAsrError {
    /// The model directory is absent or not a directory.
    #[error("offline model directory does not exist: {0}")]
    ModelDirectoryMissing(PathBuf),
    /// A file the family needs is absent (the path names it).
    #[error("offline model is missing required file: {0}")]
    MissingModelFile(PathBuf),
    /// Sherpa and transcribe.cpp take UTF-8 path strings only.
    #[error("offline model path is not valid UTF-8: {0}")]
    InvalidModelPath(PathBuf),
    /// Sherpa refused the configuration (corrupt files, or an execution
    /// provider the library build lacks); carries directory and provider.
    #[error("offline recognizer could not be created from {0} (provider {1})")]
    RecognizerUnavailable(PathBuf, String),
    /// The Silero VAD model could not be loaded from this path.
    #[error("VAD could not be created from {0}")]
    VadUnavailable(PathBuf),
    /// The catalog id matches no family (`for_model`).
    #[error("no offline model family for model id: {0}")]
    UnknownFamily(String),
    /// The transcribe.cpp engine is missing from this build or failed to
    /// load the GGUF model; the message is already user-facing.
    #[error("{0}")]
    EngineUnavailable(String),
}

/// Offline model families: Sherpa-ONNX archives, or single-file GGUF
/// models run by transcribe.cpp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineFamily {
    /// Sherpa-ONNX Qwen3-ASR export (`conv_frontend`, encoder, decoder,
    /// `tokenizer/`); supports hotwords.
    Qwen3Asr,
    /// Sherpa-ONNX SenseVoice (`model.int8.onnx` + `tokens.txt`); no
    /// hotword biasing, ITN on.
    SenseVoice,
    /// A `model.gguf` for transcribe.cpp (`gguf_asr::GgufDecoder`).
    Gguf,
}

impl OfflineFamily {
    /// Family of a catalog model id (`qwen3-asr-1.7b-gguf`,
    /// `qwen3-asr-0.6b-int8`, `sense-voice-int8`). Decided by id shape
    /// only (`-gguf` suffix, `qwen3-asr`/`sense-voice` prefix), so
    /// `config.rs` can use it during validation without touching disk.
    pub fn for_model(id: &str) -> Result<Self, OfflineAsrError> {
        if id.ends_with("-gguf") {
            Ok(Self::Gguf)
        } else if id.starts_with("qwen3-asr") {
            Ok(Self::Qwen3Asr)
        } else if id.starts_with("sense-voice") {
            Ok(Self::SenseVoice)
        } else {
            Err(OfflineAsrError::UnknownFamily(id.to_owned()))
        }
    }
}

/// Build the decoder for a model directory: GGUF models through
/// transcribe.cpp, everything else through Sherpa-ONNX.
///
/// Blocking and slow (loads the model: hundreds of MB to ~2 GB, plus a GPU
/// warm-up run for GGUF). Without the `transcribe` feature a GGUF model
/// is an `EngineUnavailable` error telling the user to install the CUDA
/// package. The returned `Arc` is what the daemon keeps warm.
pub fn create_decoder(
    family: OfflineFamily,
    model_dir: &Path,
    options: &OfflineOptions,
) -> Result<Arc<dyn Decoder>, OfflineAsrError> {
    match family {
        #[cfg(feature = "transcribe")]
        OfflineFamily::Gguf => Ok(Arc::new(crate::gguf_asr::GgufDecoder::load(
            &model_dir.join(crate::gguf_asr::MODEL_FILE),
            options,
        )?)),
        #[cfg(not(feature = "transcribe"))]
        OfflineFamily::Gguf => Err(OfflineAsrError::EngineUnavailable(
            "GGUF models need the transcribe.cpp engine: install the CUDA package (voiceless) instead of voiceless-cpu".to_owned(),
        )),
        family => Ok(Arc::new(create_recognizer(family, model_dir, options)?)),
    }
}

/// Engine options derived from `[local]` by `engine::offline_options`.
#[derive(Debug, Clone)]
pub struct OfflineOptions {
    /// Execution device: `cuda` or `cpu`.
    pub provider: String,
    /// ONNX Runtime threads (Sherpa models only; ignored by GGUF).
    pub num_threads: i32,
    /// Biasing words (Qwen3-ASR Sherpa export and GGUF vocabulary; ignored
    /// by SenseVoice).
    pub hotwords: Vec<String>,
}

/// Build the recognizer for a model directory. File names follow the
/// upstream Sherpa-ONNX release archives.
///
/// Blocking (model load). Errors: `ModelDirectoryMissing`,
/// `MissingModelFile` (checked before Sherpa runs, for a precise
/// message), `InvalidModelPath`, `RecognizerUnavailable`, and
/// `EngineUnavailable` for the GGUF family, which is not a Sherpa model.
pub fn create_recognizer(
    family: OfflineFamily,
    model_dir: &Path,
    options: &OfflineOptions,
) -> Result<OfflineRecognizer, OfflineAsrError> {
    if !model_dir.is_dir() {
        return Err(OfflineAsrError::ModelDirectoryMissing(model_dir.to_owned()));
    }
    let file = |name: &str| required(model_dir, name);
    // Sherpa takes hotwords as one comma-separated string.
    let hotwords = (!options.hotwords.is_empty()).then(|| options.hotwords.join(","));
    let mut config = OfflineRecognizerConfig::default();
    match family {
        OfflineFamily::Qwen3Asr => {
            config.model_config.qwen3_asr = OfflineQwen3ASRModelConfig {
                conv_frontend: Some(file("conv_frontend.onnx")?),
                encoder: Some(file("encoder.int8.onnx")?),
                decoder: Some(file("decoder.int8.onnx")?),
                tokenizer: Some(file("tokenizer")?),
                hotwords,
                ..Default::default()
            };
            // This export ships a `tokenizer/` directory instead of
            // `tokens.txt`; the generic field is set to an empty string
            // rather than left unset.
            config.model_config.tokens = Some(String::new());
        }
        OfflineFamily::SenseVoice => {
            let _ = hotwords; // SenseVoice has no hotword biasing.
            config.model_config.sense_voice.model = Some(file("model.int8.onnx")?);
            // Inverse text normalisation: spoken numbers come out as digits.
            config.model_config.sense_voice.use_itn = true;
            config.model_config.tokens = Some(file("tokens.txt")?);
        }
        OfflineFamily::Gguf => {
            return Err(OfflineAsrError::EngineUnavailable(
                "GGUF models are not Sherpa-ONNX models".to_owned(),
            ))
        }
    }
    config.model_config.provider = Some(options.provider.clone());
    config.model_config.num_threads = options.num_threads;
    OfflineRecognizer::create(&config).ok_or_else(|| {
        OfflineAsrError::RecognizerUnavailable(model_dir.to_owned(), options.provider.clone())
    })
}

/// Decode one complete segment with a Sherpa offline recognizer: a fresh
/// stream per call (offline streams are single-use), trimmed text, `""`
/// when the model produced nothing. Blocking.
pub fn decode(recognizer: &OfflineRecognizer, samples: &[f32]) -> String {
    let stream = recognizer.create_stream();
    stream.accept_waveform(SAMPLE_RATE, samples);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(|result| result.text.trim().to_owned())
        .unwrap_or_default()
}

/// Load the Silero VAD from `model` (`silero_vad.onnx`) tuned for
/// dictation: speech probability threshold 0.5, a segment closes after
/// 0.5 s of silence, speech shorter than 0.25 s is ignored, and a segment
/// is force-closed at `max_segment_seconds` (`[local] max_segment_seconds`
/// or the family default) so one segment never exceeds what the decoder
/// can hold. Always on the CPU with one thread; the VAD's internal buffer
/// holds the longest segment plus 10 s of slack. One VAD per session (it
/// is stateful); the decoder behind it may be shared.
pub fn create_vad(
    model: &Path,
    max_segment_seconds: f32,
) -> Result<VoiceActivityDetector, OfflineAsrError> {
    let mut config = VadModelConfig::default();
    config.silero_vad.model = Some(path_string(model)?);
    config.silero_vad.threshold = 0.5;
    // Dictation pauses mid-sentence; only a real pause closes a segment.
    config.silero_vad.min_silence_duration = 0.5;
    config.silero_vad.min_speech_duration = 0.25;
    config.silero_vad.max_speech_duration = max_segment_seconds;
    config.silero_vad.window_size = VAD_WINDOW as i32;
    config.sample_rate = SAMPLE_RATE;
    config.num_threads = 1;
    config.provider = Some("cpu".to_owned());
    VoiceActivityDetector::create(&config, max_segment_seconds + 10.0)
        .ok_or_else(|| OfflineAsrError::VadUnavailable(model.to_owned()))
}

/// VAD-segmented recognition state; synchronous, owned by the worker.
///
/// Audio flows `accept` -> `pending` (whole VAD windows only) -> the VAD
/// and `open_segment` in parallel. The VAD decides where segments end;
/// `open_segment` is the audio the live partial is decoded from and is
/// trimmed to the pre-roll while no speech has been detected. `confirmed`
/// grows by one entry per decoded segment and is what `finish` reports.
pub struct SegmentedRecognizer {
    decoder: Arc<dyn Decoder>,
    vad: VoiceActivityDetector,
    /// Minimum time between two partial decodes (`[local]
    /// partial_interval_ms`; one hour in the console's offline jobs to
    /// disable partials).
    partial_interval: Duration,
    /// Samples not yet forming a full VAD window.
    pending: Vec<f32>,
    /// Audio since the last closed segment (bounded to the pre-roll until
    /// speech starts); the input of the live partial.
    open_segment: Vec<f32>,
    /// The VAD has seen speech in the open segment; partials are only
    /// decoded (and the pre-roll trim stops) once this is set.
    speech_started: bool,
    last_partial_at: Option<Instant>,
    last_partial: String,
    /// Text of every closed segment, in order.
    confirmed: Vec<String>,
}

impl SegmentedRecognizer {
    /// `decoder` may be shared: a daemon keeps one warm across sessions.
    pub fn new(
        decoder: Arc<dyn Decoder>,
        vad: VoiceActivityDetector,
        partial_interval: Duration,
    ) -> Self {
        Self {
            decoder,
            vad,
            partial_interval,
            pending: Vec::new(),
            open_segment: Vec::new(),
            speech_started: false,
            last_partial_at: None,
            last_partial: String::new(),
            confirmed: Vec::new(),
        }
    }

    /// Feed samples through the VAD; close finished segments.
    ///
    /// Returns one `Completed` per segment the VAD closed with these
    /// samples (decoded synchronously, so this blocks for the decode).
    /// Samples that do not fill a whole 512-sample window stay in
    /// `pending` for the next call.
    pub fn accept(&mut self, samples: &[f32]) -> Vec<RealtimeEvent> {
        self.pending.extend_from_slice(samples);
        let mut offset = 0;
        while offset + VAD_WINDOW <= self.pending.len() {
            let window = &self.pending[offset..offset + VAD_WINDOW];
            self.vad.accept_waveform(window);
            self.open_segment.extend_from_slice(window);
            if !self.speech_started && self.vad.detected() {
                self.speech_started = true;
            }
            offset += VAD_WINDOW;
        }
        self.pending.drain(..offset);
        // Silence before speech is kept only as a short pre-roll, so the
        // partial decode never runs over minutes of leading silence.
        if !self.speech_started && self.open_segment.len() > PRE_ROLL_SAMPLES {
            let excess = self.open_segment.len() - PRE_ROLL_SAMPLES;
            self.open_segment.drain(..excess);
        }
        self.drain_segments()
    }

    /// Re-decode the open segment when the partial interval has elapsed.
    ///
    /// Called by the worker after every batch of audio. Returns at most one
    /// `Partial`: nothing before speech is detected, nothing while the
    /// interval has not elapsed, nothing when the text is empty or
    /// unchanged. The decode covers the whole open segment, so the partial
    /// is a full rewrite rather than an append (the UI replaces it).
    pub fn partial(&mut self) -> Vec<RealtimeEvent> {
        if !self.speech_started {
            return Vec::new();
        }
        if let Some(at) = self.last_partial_at {
            if at.elapsed() < self.partial_interval {
                return Vec::new();
            }
        }
        // Timed from the start of the decode: a slow decoder self-limits
        // to one partial per interval plus decode time.
        self.last_partial_at = Some(Instant::now());
        let text = self.decoder.decode(&self.open_segment, &self.context());
        if text.is_empty() || text == self.last_partial {
            return Vec::new();
        }
        self.last_partial = text.clone();
        vec![RealtimeEvent::Partial {
            text,
            stash: String::new(),
        }]
    }

    /// Flush the VAD, close the last segment and report the transcript.
    ///
    /// The sub-window remainder in `pending` is fed as-is, the VAD is
    /// flushed so speech without a trailing pause still becomes a segment,
    /// every closed segment is decoded (`Completed`), and the last event is
    /// `Finished` with all confirmed text concatenated. The open-segment
    /// partial plays no part in the final text. Blocking for the decodes.
    pub fn finish(&mut self) -> Vec<RealtimeEvent> {
        let tail = std::mem::take(&mut self.pending);
        if !tail.is_empty() {
            self.vad.accept_waveform(&tail);
            self.open_segment.extend_from_slice(&tail);
        }
        self.vad.flush();
        let mut events = self.drain_segments();
        events.push(RealtimeEvent::Finished {
            transcript: self.confirmed.concat(),
        });
        events
    }

    /// The last `CONTEXT_CHARS` characters of the confirmed transcript.
    fn context(&self) -> String {
        let confirmed = self.confirmed.concat();
        let skip = confirmed.chars().count().saturating_sub(CONTEXT_CHARS);
        confirmed.chars().skip(skip).collect()
    }

    /// Decode every segment the VAD has closed, in order. The segment audio
    /// comes from the VAD (speech bounds), not from `open_segment`; closing
    /// a segment resets the open segment and the partial state so the next
    /// utterance starts clean. Empty decodes confirm nothing.
    fn drain_segments(&mut self) -> Vec<RealtimeEvent> {
        let mut events = Vec::new();
        while let Some(segment) = self.vad.front() {
            self.vad.pop();
            let text = self.decoder.decode(segment.samples(), &self.context());
            self.open_segment.clear();
            self.speech_started = false;
            self.last_partial.clear();
            self.last_partial_at = None;
            if !text.is_empty() {
                self.confirmed.push(text.clone());
                events.push(RealtimeEvent::Completed { transcript: text });
            }
        }
        events
    }
}

/// Worker commands: samples to feed, the single finish, or stop at once.
enum Command {
    Audio(Vec<f32>),
    Finish,
    Cancel,
}

/// Streaming transport over a `SegmentedRecognizer` running on its own
/// thread. The worker loads the model itself, so `Ready` (and capture)
/// starts immediately; audio sent during the load queues and is decoded
/// once the model is up. A load failure surfaces as an `Error` event.
///
/// Channels: commands go over an unbounded std channel (so `send_audio` is
/// synchronous and never blocks the coordinator; backlog during a load or
/// a slow decode is bounded only by memory), events come back over an
/// unbounded tokio channel. `stream.rs` adapts this to `BackendTransport`
/// (`OfflineTransportAdapter`).
pub struct OfflineTransport {
    commands: std_mpsc::Sender<Command>,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<RealtimeEvent>>,
    /// `finish` sends `Command::Finish` at most once.
    finished: bool,
    /// Joined on drop: the worker must release the decoder (GPU buffers)
    /// before the process exits, or CUDA's own exit teardown races the free
    /// and aborts.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl OfflineTransport {
    /// Spawn the worker thread; `load` runs on it (model and VAD
    /// construction, possibly seconds for a cold GGUF load) and its error
    /// becomes the session's `Error` event. `Ready` is queued before the
    /// thread even starts. Returns immediately.
    pub fn start<F>(load: F) -> Self
    where
        F: FnOnce() -> Result<SegmentedRecognizer, OfflineAsrError> + Send + 'static,
    {
        let (command_tx, command_rx) = std_mpsc::channel::<Command>();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        // Ready before load: capture must not wait for the model, and
        // audio sent meanwhile simply queues on the command channel.
        let _ = event_tx.send(RealtimeEvent::Ready);
        let worker = std::thread::spawn(move || {
            let mut recognizer = match load() {
                Ok(recognizer) => recognizer,
                Err(error) => {
                    // Returning closes the command channel, so later
                    // `send_audio` calls fail with BrokenPipe as well.
                    let _ = event_tx.send(RealtimeEvent::Error(error.to_string()));
                    return;
                }
            };
            let send = |events: Vec<RealtimeEvent>| {
                events.into_iter().all(|event| event_tx.send(event).is_ok())
            };
            // Each loop turn: one blocking wait, then everything that
            // accumulated, then at most one partial. A receiver that went
            // away (transport dropped) ends the loop through `send`.
            while let Ok(command) = command_rx.recv() {
                // Coalesce everything queued while the last decode ran.
                let mut batch = vec![command];
                batch.extend(command_rx.try_iter());
                let mut finish = false;
                for command in batch {
                    match command {
                        Command::Audio(samples) => {
                            if !send(recognizer.accept(&samples)) {
                                return;
                            }
                        }
                        // Audio queued behind Finish in the same batch is
                        // still fed first: the loop reaches it before the
                        // flag is acted on.
                        Command::Finish => finish = true,
                        Command::Cancel => return,
                    }
                }
                if finish {
                    // The worker exits after Finished; the event channel
                    // closes once `event_tx` drops with it.
                    send(recognizer.finish());
                    return;
                }
                if !send(recognizer.partial()) {
                    return;
                }
            }
        });
        Self {
            commands: command_tx,
            events: tokio::sync::Mutex::new(event_rx),
            finished: false,
            worker: Some(worker),
        }
    }

    /// Queue one PCM16 LE 16 kHz mono chunk (converted to f32 here, on the
    /// caller's thread). Synchronous and non-blocking. Errors:
    /// `InvalidData` for an odd byte count (nothing queued), `BrokenPipe`
    /// once the worker has exited (after finish, cancel or a load failure).
    pub fn send_audio(&self, pcm: &[u8]) -> std::io::Result<()> {
        if !pcm.len().is_multiple_of(2) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("incomplete PCM16 frame ({} bytes)", pcm.len()),
            ));
        }
        let samples = pcm
            .chunks_exact(2)
            .map(|bytes| f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / f32::from(i16::MAX))
            .collect();
        self.send(Command::Audio(samples))
    }

    /// Next worker event: `Ready` first, then `Completed`/`Partial` in
    /// decode order, then `Finished` (or one `Error`). `Err(UnexpectedEof)`
    /// once the worker has exited and the queue is drained.
    pub async fn next_event(&self) -> std::io::Result<RealtimeEvent> {
        self.events.lock().await.recv().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "offline worker closed")
        })
    }

    /// Ask the worker to flush and finish; at most once (later calls are
    /// no-ops). Returns as soon as the command is queued; the decodes
    /// happen on the worker and `Finished` arrives via `next_event`.
    /// `BrokenPipe` if the worker already exited.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.send(Command::Finish)
    }

    /// Ask the worker to stop without decoding. Never fails: a worker that
    /// already exited needs no cancel.
    pub fn cancel(&self) -> std::io::Result<()> {
        let _ = self.commands.send(Command::Cancel);
        Ok(())
    }

    /// Queue one command; `BrokenPipe` once the worker has exited.
    fn send(&self, command: Command) -> std::io::Result<()> {
        self.commands.send(command).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "offline worker closed")
        })
    }
}

impl Drop for OfflineTransport {
    fn drop(&mut self) {
        // A finished worker has already returned; otherwise stop it. Either
        // way wait, bounded by one decode.
        let _ = self.commands.send(Command::Cancel);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// `model_dir/name` as a Sherpa path string if it exists (file or
/// directory: the Qwen3-ASR tokenizer is a directory), else
/// `MissingModelFile`.
fn required(model_dir: &Path, name: &str) -> Result<String, OfflineAsrError> {
    let path = model_dir.join(name);
    if path.exists() {
        path_string(&path)
    } else {
        Err(OfflineAsrError::MissingModelFile(path))
    }
}

/// Sherpa takes `String` paths; non-UTF-8 paths are rejected explicitly.
fn path_string(path: &Path) -> Result<String, OfflineAsrError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| OfflineAsrError::InvalidModelPath(path.to_owned()))
}
