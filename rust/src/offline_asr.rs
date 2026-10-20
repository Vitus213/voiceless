//! Offline (non-streaming) models driven as a streaming backend: Sherpa-ONNX
//! archives, or GGUF models on transcribe.cpp (CUDA) with the `transcribe`
//! feature. Silero VAD splits speech at pauses: every closed segment gets a
//! final decode (`Completed`), and the open segment is re-decoded every
//! `partial_interval` for the live partial. Decoding runs on a dedicated
//! worker thread, so a slow decode never stalls audio capture; queued audio
//! is coalesced before the next partial.

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
const VAD_WINDOW: usize = 512;
/// Audio kept before detected speech so the first syllable is not clipped.
const PRE_ROLL_SAMPLES: usize = 10 * VAD_WINDOW;
/// Confirmed text handed to context-aware decoders, in characters.
const CONTEXT_CHARS: usize = 120;

/// One offline decode of a speech segment. `context` is the tail of the
/// text confirmed before this segment; decoders that support context
/// conditioning use it to keep segments coherent, others ignore it.
pub trait Decoder: Send + Sync {
    fn decode(&self, samples: &[f32], context: &str) -> String;
}

impl Decoder for OfflineRecognizer {
    fn decode(&self, samples: &[f32], _context: &str) -> String {
        decode(self, samples)
    }
}

#[derive(Debug, Error)]
pub enum OfflineAsrError {
    #[error("offline model directory does not exist: {0}")]
    ModelDirectoryMissing(PathBuf),
    #[error("offline model is missing required file: {0}")]
    MissingModelFile(PathBuf),
    #[error("offline model path is not valid UTF-8: {0}")]
    InvalidModelPath(PathBuf),
    #[error("offline recognizer could not be created from {0} (provider {1})")]
    RecognizerUnavailable(PathBuf, String),
    #[error("VAD could not be created from {0}")]
    VadUnavailable(PathBuf),
    #[error("no offline model family for model id: {0}")]
    UnknownFamily(String),
    #[error("{0}")]
    EngineUnavailable(String),
}

/// Offline model families: Sherpa-ONNX archives, or single-file GGUF
/// models run by transcribe.cpp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineFamily {
    Qwen3Asr,
    SenseVoice,
    Gguf,
}

impl OfflineFamily {
    /// Family of a catalog model id (`qwen3-asr-1.7b-gguf`,
    /// `qwen3-asr-0.6b-int8`, `sense-voice-int8`).
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

#[derive(Debug, Clone)]
pub struct OfflineOptions {
    /// Execution device: `cuda` or `cpu`.
    pub provider: String,
    pub num_threads: i32,
    pub hotwords: Vec<String>,
}

/// Build the recognizer for a model directory. File names follow the
/// upstream Sherpa-ONNX release archives.
pub fn create_recognizer(
    family: OfflineFamily,
    model_dir: &Path,
    options: &OfflineOptions,
) -> Result<OfflineRecognizer, OfflineAsrError> {
    if !model_dir.is_dir() {
        return Err(OfflineAsrError::ModelDirectoryMissing(model_dir.to_owned()));
    }
    let file = |name: &str| required(model_dir, name);
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
            config.model_config.tokens = Some(String::new());
        }
        OfflineFamily::SenseVoice => {
            let _ = hotwords; // SenseVoice has no hotword biasing.
            config.model_config.sense_voice.model = Some(file("model.int8.onnx")?);
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

pub fn decode(recognizer: &OfflineRecognizer, samples: &[f32]) -> String {
    let stream = recognizer.create_stream();
    stream.accept_waveform(SAMPLE_RATE, samples);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(|result| result.text.trim().to_owned())
        .unwrap_or_default()
}

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
pub struct SegmentedRecognizer {
    decoder: Arc<dyn Decoder>,
    vad: VoiceActivityDetector,
    partial_interval: Duration,
    pending: Vec<f32>,
    open_segment: Vec<f32>,
    speech_started: bool,
    last_partial_at: Option<Instant>,
    last_partial: String,
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
        if !self.speech_started && self.open_segment.len() > PRE_ROLL_SAMPLES {
            let excess = self.open_segment.len() - PRE_ROLL_SAMPLES;
            self.open_segment.drain(..excess);
        }
        self.drain_segments()
    }

    /// Re-decode the open segment when the partial interval has elapsed.
    pub fn partial(&mut self) -> Vec<RealtimeEvent> {
        if !self.speech_started {
            return Vec::new();
        }
        if let Some(at) = self.last_partial_at {
            if at.elapsed() < self.partial_interval {
                return Vec::new();
            }
        }
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

enum Command {
    Audio(Vec<f32>),
    Finish,
    Cancel,
}

/// Streaming transport over a `SegmentedRecognizer` running on its own
/// thread. The worker loads the model itself, so `Ready` (and capture)
/// starts immediately; audio sent during the load queues and is decoded
/// once the model is up. A load failure surfaces as an `Error` event.
pub struct OfflineTransport {
    commands: std_mpsc::Sender<Command>,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<RealtimeEvent>>,
    finished: bool,
    /// Joined on drop: the worker must release the decoder (GPU buffers)
    /// before the process exits, or CUDA's own exit teardown races the free
    /// and aborts.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl OfflineTransport {
    pub fn start<F>(load: F) -> Self
    where
        F: FnOnce() -> Result<SegmentedRecognizer, OfflineAsrError> + Send + 'static,
    {
        let (command_tx, command_rx) = std_mpsc::channel::<Command>();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let _ = event_tx.send(RealtimeEvent::Ready);
        let worker = std::thread::spawn(move || {
            let mut recognizer = match load() {
                Ok(recognizer) => recognizer,
                Err(error) => {
                    let _ = event_tx.send(RealtimeEvent::Error(error.to_string()));
                    return;
                }
            };
            let send = |events: Vec<RealtimeEvent>| {
                events.into_iter().all(|event| event_tx.send(event).is_ok())
            };
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
                        Command::Finish => finish = true,
                        Command::Cancel => return,
                    }
                }
                if finish {
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

    pub async fn next_event(&self) -> std::io::Result<RealtimeEvent> {
        self.events.lock().await.recv().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "offline worker closed")
        })
    }

    pub fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.send(Command::Finish)
    }

    pub fn cancel(&self) -> std::io::Result<()> {
        let _ = self.commands.send(Command::Cancel);
        Ok(())
    }

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

fn required(model_dir: &Path, name: &str) -> Result<String, OfflineAsrError> {
    let path = model_dir.join(name);
    if path.exists() {
        path_string(&path)
    } else {
        Err(OfflineAsrError::MissingModelFile(path))
    }
}

fn path_string(path: &Path) -> Result<String, OfflineAsrError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| OfflineAsrError::InvalidModelPath(path.to_owned()))
}
