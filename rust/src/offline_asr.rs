//! Offline (non-streaming) models driven as a streaming backend: the
//! `local-offline` backend (`[asr] streaming_backend = "local-offline"`,
//! the one the daemon keeps warm). Models are Sherpa-ONNX archives
//! (`qwen3-asr-0.6b-int8`, `sense-voice-int8`; CPU only) or single-file
//! GGUF models on transcribe.cpp (`qwen3-asr-1.7b-gguf`,
//! `qwen3-asr-0.6b-gguf`; CUDA or CPU, see `gguf_asr.rs`, compiled with the
//! `transcribe` feature). Silero VAD splits speech at pauses: every closed
//! segment gets a final decode. The preview worker re-decodes the open
//! segment and uses a separate, ordered finalization thread when fast
//! previews are enabled. Pending segments keep their last preview visible.
//! Optional GTCRN enhancement runs on the same worker ahead of Silero.
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
//! since the last pause), so it may rewrite earlier words. Each VAD segment
//! is finalized with the tail of the preceding confirmed text as context.
//! With fast previews, the confirmation also carries the newer tentative
//! suffix so the UI updates both in one event. `finish` flushes the audio
//! and VAD and waits for all final decodes before sending `Finished`.
//! Cancellation discards pending work and joins any active native decode.
//!
//! Failure semantics: `create_decoder`, `create_recognizer` and
//! `create_vad` return `OfflineAsrError`; inside the transport such an
//! error surfaces once as `RealtimeEvent::Error` and ends the worker.
//! Decoders never fail a session: a decode error yields an empty string for
//! that segment (see `gguf_asr.rs`). No retries and no fallback here.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sherpa_onnx::{
    OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig, OnlineSpeechDenoiser,
    OnlineSpeechDenoiserConfig, SpeechSegment, VadModelConfig, VoiceActivityDetector,
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
/// Retry an empty first preview after more speech arrives.
const FIRST_PREVIEW_RETRY: Duration = Duration::from_millis(160);
/// Retry an unchanged preview while keeping the compute-based lower bound.
const UNCHANGED_PREVIEW_RETRY: Duration = Duration::from_millis(250);
/// Bound a worker batch so queued audio cannot starve preview evaluation.
const WORKER_BATCH_LIMIT: usize = 8;

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
    /// The GTCRN enhancement model could not be loaded at the capture rate.
    #[error("16 kHz speech denoiser could not be created from {0}")]
    DenoiserUnavailable(PathBuf),
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

/// Load Silero for offline file decoding with a 250 ms speech onset.
pub fn create_vad(
    model: &Path,
    max_segment_seconds: f32,
) -> Result<VoiceActivityDetector, OfflineAsrError> {
    configured_vad(model, max_segment_seconds, 0.5, 0.25)
}

pub(crate) fn create_session_vad(
    model: &Path,
    max_segment_seconds: f32,
    enhanced: bool,
) -> Result<VoiceActivityDetector, OfflineAsrError> {
    // Enhanced speech can stay below the raw-input threshold at low SNR.
    // Require a longer onset on the enhanced path to reject short noise bursts.
    let (threshold, min_speech_seconds) = if enhanced { (0.25, 0.25) } else { (0.5, 0.16) };
    configured_vad(model, max_segment_seconds, threshold, min_speech_seconds)
}

fn configured_vad(
    model: &Path,
    max_segment_seconds: f32,
    threshold: f32,
    min_speech_seconds: f32,
) -> Result<VoiceActivityDetector, OfflineAsrError> {
    let mut config = VadModelConfig::default();
    config.silero_vad.model = Some(path_string(model)?);
    config.silero_vad.threshold = threshold;
    // Dictation pauses mid-sentence; only a real pause closes a segment.
    config.silero_vad.min_silence_duration = 0.5;
    config.silero_vad.min_speech_duration = min_speech_seconds;
    config.silero_vad.max_speech_duration = max_segment_seconds;
    config.silero_vad.window_size = VAD_WINDOW as i32;
    config.sample_rate = SAMPLE_RATE;
    config.num_threads = 1;
    config.provider = Some("cpu".to_owned());
    VoiceActivityDetector::create(&config, max_segment_seconds + 10.0)
        .ok_or_else(|| OfflineAsrError::VadUnavailable(model.to_owned()))
}

struct SpeechEnhancer {
    denoiser: OnlineSpeechDenoiser,
    input: VecDeque<f32>,
    level: f32,
}

impl SpeechEnhancer {
    fn new(model: &Path) -> Result<Self, OfflineAsrError> {
        let mut config = OnlineSpeechDenoiserConfig::default();
        config.model.gtcrn.model = Some(path_string(model)?);
        let denoiser = OnlineSpeechDenoiser::create(&config)
            .filter(|denoiser| denoiser.sample_rate() == SAMPLE_RATE)
            .ok_or_else(|| OfflineAsrError::DenoiserUnavailable(model.to_owned()))?;
        Ok(Self {
            denoiser,
            input: VecDeque::new(),
            level: 0.0,
        })
    }

    fn accept(&mut self, samples: &[f32]) -> Vec<f32> {
        self.input.extend(samples);
        let audio = self.denoiser.run(samples, SAMPLE_RATE).samples;
        self.mix_and_amplify(audio)
    }

    fn finish(&mut self) -> Vec<f32> {
        let audio = self.denoiser.flush().samples;
        self.mix_and_amplify(audio)
    }

    fn mix_and_amplify(&mut self, mut samples: Vec<f32>) -> Vec<f32> {
        // Sherpa aligns its delayed output to the input sample sequence.
        // Retain a little original signal for weak consonants and onsets.
        let count = samples.len();
        for (sample, dry) in samples.iter_mut().zip(self.input.drain(..count)) {
            *sample = 0.65 * *sample + 0.35 * dry;
        }
        self.apply_gain(&mut samples);
        samples
    }

    fn apply_gain(&mut self, samples: &mut [f32]) {
        // Track gain over short frames, with headroom for transients.
        for frame in samples.chunks_mut(VAD_WINDOW) {
            let mut sum_squares = 0.0;
            let mut peak: f32 = 0.0;
            for &sample in frame.iter() {
                sum_squares += sample * sample;
                peak = peak.max(sample.abs());
            }
            let rms = (sum_squares / frame.len() as f32).sqrt();
            // Track louder speech quickly and release slowly between syllables.
            self.level = rms.max(self.level * 0.93);
            // Target about -26 dBFS, capped at +18 dB with peak headroom.
            let gain = (0.05 / self.level).clamp(1.0, 8.0).min(0.98 / peak);
            for sample in frame {
                *sample *= gain;
            }
        }
    }
}

/// When previews use a separate model, a single decoder thread owns the
/// closed segments and their context. The capture worker owns visible text.
struct BackgroundDecoder {
    jobs: Option<std_mpsc::Sender<(SpeechSegment, usize)>>,
    previews: VecDeque<String>,
    cancelled: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl BackgroundDecoder {
    fn start(
        decoder: Arc<dyn Decoder>,
        cancelled: Arc<AtomicBool>,
        results: std_mpsc::Sender<Command>,
    ) -> Self {
        let (jobs, queue) = std_mpsc::channel::<(SpeechSegment, usize)>();
        let results = FinalizationResults(results);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            let mut context = String::new();
            for (segment, length) in queue {
                if worker_cancelled.load(Ordering::Relaxed) {
                    break;
                }
                let text = decoder.decode(&segment.samples()[..length], &context);
                context.push_str(&text);
                let excess = context.len() - context_tail(&context).len();
                context.drain(..excess);
                if results.send(text).is_err() {
                    break;
                }
            }
        });
        Self {
            jobs: Some(jobs),
            previews: VecDeque::new(),
            cancelled,
            worker: Some(worker),
        }
    }

    fn submit(&mut self, segment: SpeechSegment, length: usize, preview: String) -> bool {
        if self.jobs.as_ref().unwrap().send((segment, length)).is_err() {
            return false;
        }
        self.previews.push_back(preview);
        true
    }
}

impl Drop for BackgroundDecoder {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// Wake the capture worker if the decoder exits while it still expects results.
struct FinalizationResults(std_mpsc::Sender<Command>);

impl FinalizationResults {
    fn send(&self, text: String) -> Result<(), std_mpsc::SendError<Command>> {
        self.0.send(Command::SegmentDecoded(text))
    }
}

impl Drop for FinalizationResults {
    fn drop(&mut self) {
        let _ = self.0.send(Command::FinalizerStopped);
    }
}

fn context_tail(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .nth(CONTEXT_CHARS - 1)
        .map_or(0, |(start, _)| start);
    &text[start..]
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
    preview: Option<Arc<dyn Decoder>>,
    background: Option<BackgroundDecoder>,
    vad: VoiceActivityDetector,
    /// Target cadence after the first live text (`[local].partial_interval_ms`).
    partial_interval: Duration,
    /// Samples not yet forming a full VAD window.
    pending: Vec<f32>,
    /// Audio since the last closed segment (bounded to the pre-roll until
    /// speech starts); the input of the live partial.
    open_segment: Vec<f32>,
    processed_samples: usize,
    /// Start time, cooldown and audio length of the last preview attempt.
    last_preview: Option<(Instant, Duration, usize)>,
    enhancement: Option<SpeechEnhancer>,
    last_partial: String,
    /// Concatenated text of the closed segments in capture order.
    confirmed: String,
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
            preview: None,
            background: None,
            vad,
            partial_interval,
            pending: Vec::new(),
            open_segment: Vec::new(),
            processed_samples: 0,
            last_preview: None,
            enhancement: None,
            last_partial: String::new(),
            confirmed: String::new(),
        }
    }

    /// Use a separate decoder for tentative text. Closed segments always
    /// use the configured primary decoder.
    pub(crate) fn with_preview(mut self, preview: Option<Arc<dyn Decoder>>) -> Self {
        self.preview = preview;
        self
    }

    /// Enable streaming GTCRN and bounded gain ahead of the VAD and decoder.
    pub(crate) fn with_denoiser(mut self, model: &Path) -> Result<Self, OfflineAsrError> {
        self.enhancement = Some(SpeechEnhancer::new(model)?);
        Ok(self)
    }

    /// Feed samples through the VAD; close finished segments.
    ///
    /// Closed segments are decoded here unless the transport has enabled
    /// background finalization for a separate preview model.
    /// Samples that do not fill a whole 512-sample window stay in
    /// `pending` for the next call.
    pub fn accept(&mut self, samples: &[f32]) -> Vec<RealtimeEvent> {
        if let Some(enhancement) = &mut self.enhancement {
            let audio = enhancement.accept(samples);
            self.accept_enhanced(&audio)
        } else {
            self.accept_enhanced(samples)
        }
    }

    fn accept_enhanced(&mut self, mut samples: &[f32]) -> Vec<RealtimeEvent> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let needed = VAD_WINDOW - self.pending.len();
            let (prefix, remainder) = samples.split_at(needed.min(samples.len()));
            self.pending.extend_from_slice(prefix);
            if self.pending.len() < VAD_WINDOW {
                return events;
            }
            let mut pending = std::mem::take(&mut self.pending);
            self.accept_window(&pending, &mut events);
            pending.clear();
            self.pending = pending;
            samples = remainder;
        }
        let mut windows = samples.chunks_exact(VAD_WINDOW);
        for window in &mut windows {
            self.accept_window(window, &mut events);
        }
        self.pending.extend_from_slice(windows.remainder());
        events
    }

    fn accept_window(&mut self, window: &[f32], events: &mut Vec<RealtimeEvent>) {
        self.vad.accept_waveform(window);
        self.processed_samples += window.len();
        self.open_segment.extend_from_slice(window);
        self.drain_segments(events);
        if !self.vad.detected() {
            let excess = self.open_segment.len().saturating_sub(PRE_ROLL_SAMPLES);
            self.open_segment.drain(..excess);
        }
    }

    /// Re-decode the open segment when the partial interval has elapsed.
    ///
    /// Called by the worker after every batch of audio. Returns at most one
    /// `Partial`: nothing before speech is detected, nothing while the
    /// interval has not elapsed, nothing when the text is empty or
    /// unchanged. The decode covers the whole open segment, so the partial
    /// is a full rewrite rather than an append (the UI replaces it).
    pub fn partial(&mut self) -> Vec<RealtimeEvent> {
        if !self.vad.detected() {
            return Vec::new();
        }
        let samples = self.open_segment.len();
        if let Some((at, delay, previous_samples)) = self.last_preview {
            if at.elapsed() < delay || samples <= previous_samples {
                return Vec::new();
            }
        }
        let started = Instant::now();
        let text = match &self.preview {
            Some(preview) => preview.decode(&self.open_segment, ""),
            None => self
                .decoder
                .decode(&self.open_segment, context_tail(&self.confirmed)),
        };
        let interval = if text.is_empty() && self.last_partial.is_empty() {
            self.partial_interval.min(FIRST_PREVIEW_RETRY)
        } else if text.is_empty() || text == self.last_partial {
            self.partial_interval.min(UNCHANGED_PREVIEW_RETRY)
        } else {
            self.partial_interval
        };
        let delay = interval.max(started.elapsed().saturating_mul(2));
        self.last_preview = Some((started, delay, samples));
        if text.is_empty() || text == self.last_partial {
            return Vec::new();
        }
        self.last_partial = text;
        vec![RealtimeEvent::Partial {
            text: self.visible_preview(),
            stash: String::new(),
        }]
    }

    /// Flush and finalize a synchronous recognition run. The streaming
    /// transport flushes separately when it must wait for background results.
    pub fn finish(&mut self) -> Vec<RealtimeEvent> {
        let mut events = self.flush_audio();
        events.push(RealtimeEvent::Finished {
            transcript: self.confirmed.clone(),
        });
        events
    }

    fn flush_audio(&mut self) -> Vec<RealtimeEvent> {
        let mut events = if let Some(enhancement) = &mut self.enhancement {
            let audio = enhancement.finish();
            self.accept_enhanced(&audio)
        } else {
            Vec::new()
        };
        if !self.pending.is_empty() {
            let mut tail = std::mem::take(&mut self.pending);
            self.processed_samples += tail.len();
            tail.resize(VAD_WINDOW, 0.0);
            self.vad.accept_waveform(&tail);
        }
        self.vad.flush();
        self.drain_segments(&mut events);
        events
    }

    fn visible_preview(&self) -> String {
        self.background
            .iter()
            .flat_map(|background| background.previews.iter())
            .chain(std::iter::once(&self.last_partial))
            .map(String::as_str)
            .collect()
    }

    fn finalized_segment(&mut self, transcript: String) -> RealtimeEvent {
        self.background.as_mut().unwrap().previews.pop_front();
        self.confirmed.push_str(&transcript);
        RealtimeEvent::SegmentFinalized {
            transcript,
            preview: self.visible_preview(),
        }
    }

    fn has_pending_finalizations(&self) -> bool {
        self.background
            .as_ref()
            .is_some_and(|worker| !worker.previews.is_empty())
    }

    fn start_finalizer(&mut self, cancelled: Arc<AtomicBool>, results: std_mpsc::Sender<Command>) {
        if self.preview.is_some() {
            self.background = Some(BackgroundDecoder::start(
                self.decoder.clone(),
                cancelled,
                results,
            ));
        }
    }

    /// Decode closed segments at their window boundary. Keep only trailing
    /// audio after the segment as pre-roll for the next speech onset.
    fn drain_segments(&mut self, events: &mut Vec<RealtimeEvent>) {
        while let Some(segment) = self.vad.front() {
            self.vad.pop();
            let start = segment.start() as usize;
            let length = segment
                .samples()
                .len()
                .min(self.processed_samples.saturating_sub(start));
            let end = start + length;
            let keep = self
                .processed_samples
                .saturating_sub(end)
                .min(PRE_ROLL_SAMPLES)
                .min(self.open_segment.len());
            self.open_segment.drain(..self.open_segment.len() - keep);
            let preview = std::mem::take(&mut self.last_partial);
            self.last_preview = None;
            if let Some(background) = &mut self.background {
                if !background.submit(segment, length, preview) {
                    events.push(finalizer_stopped());
                    return;
                }
            } else {
                let text = self
                    .decoder
                    .decode(&segment.samples()[..length], context_tail(&self.confirmed));
                if !text.is_empty() {
                    self.confirmed.push_str(&text);
                    events.push(RealtimeEvent::Completed { transcript: text });
                }
            }
        }
    }
}

fn finalizer_stopped() -> RealtimeEvent {
    RealtimeEvent::Error("offline segment decoder stopped".to_owned())
}

/// Inputs and ordered decode results handled by the capture worker.
enum Command {
    Audio(Vec<f32>),
    Finish,
    Cancel,
    SegmentDecoded(String),
    FinalizerStopped,
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
    cancelled: Arc<AtomicBool>,
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
        let worker_commands = command_tx.clone();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
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
            recognizer.start_finalizer(Arc::clone(&worker_cancelled), worker_commands);
            let send = |event: RealtimeEvent| {
                let failed = matches!(&event, RealtimeEvent::Error(_));
                !worker_cancelled.load(Ordering::Relaxed) && event_tx.send(event).is_ok() && !failed
            };
            // A result wakes the same loop as incoming audio. After flushing,
            // wait for all submitted segments while still accepting cancellation.
            let mut finishing = false;
            while let Ok(command) = command_rx.recv() {
                let mut finish = false;
                for command in std::iter::once(command)
                    .chain(command_rx.try_iter().take(WORKER_BATCH_LIMIT - 1))
                {
                    if worker_cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    match command {
                        Command::Audio(samples) if !finishing => {
                            if !recognizer.accept(&samples).into_iter().all(&send) {
                                return;
                            }
                        }
                        Command::Audio(_) => {}
                        Command::SegmentDecoded(text) => {
                            if !send(recognizer.finalized_segment(text)) {
                                return;
                            }
                        }
                        Command::FinalizerStopped => {
                            send(finalizer_stopped());
                            return;
                        }
                        Command::Finish => {
                            finish = true;
                            break;
                        }
                        Command::Cancel => return,
                    }
                }
                if worker_cancelled.load(Ordering::Relaxed) {
                    return;
                }
                if finish && !finishing {
                    if !recognizer.flush_audio().into_iter().all(&send) {
                        return;
                    }
                    finishing = true;
                }
                if finishing {
                    if !recognizer.has_pending_finalizations() {
                        send(RealtimeEvent::Finished {
                            transcript: std::mem::take(&mut recognizer.confirmed),
                        });
                        return;
                    }
                } else if !recognizer.partial().into_iter().all(&send) {
                    return;
                }
            }
        });
        Self {
            commands: command_tx,
            events: tokio::sync::Mutex::new(event_rx),
            finished: false,
            cancelled,
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

    /// Next worker event: `Ready`, then live previews and segment
    /// confirmations, then `Finished` or `Error`. Returns `UnexpectedEof`
    /// once the worker has exited and all events have been read.
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

    /// Cancel pending audio and wake an idle worker. An in-flight native
    /// decode is allowed to release its resources before the worker exits.
    pub fn cancel(&self) -> std::io::Result<()> {
        self.cancelled.store(true, Ordering::Relaxed);
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
        // Release the model before process teardown, even on cancellation.
        let _ = self.cancel();
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
