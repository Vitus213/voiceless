//! Turning media into text: the adapters between the queue's workers and
//! the recognition engines.
//!
//! A [`SegmentRecognizer`] receives the decoded stream in bounded chunks
//! and hands back the speech segments each chunk closed, with their media
//! time. [`Recognition`] builds one for a run of a job. The queue's worker
//! knows nothing beyond these two traits, which is what lets tests script
//! recognition and lets the shared resource be taken per unit.
//!
//! What the runtime provides:
//!
//! - [`VadRecognizer`]: Silero VAD finds the speech, every closed segment
//!   is decoded by a [`SegmentDecoder`], and the segment's time is where
//!   the VAD found it in the decoded stream. It is the file path of the
//!   live `local-offline` backend, without previews.
//! - [`LocalModel`]: the daemon's warm decoder when its model, device and
//!   hotwords are the job's, else a decoder loaded from the model store
//!   and leased for the run.
//! - [`CloudDecoder`]: the DashScope batch endpoint, one request per
//!   segment, over `batch::CloudBatchClient`.
//! - [`RuntimeRecognition`]: chooses between them from the applied
//!   configuration, and refuses a cloud job the user did not authorize
//!   before anything is decoded.
//!
//! A resumed run restarts at the end of the last committed segment. The
//! detector's state is empty there, so feeding the audio again from that
//! point finds the same segments the interrupted run would have found.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sherpa_onnx::VoiceActivityDetector;

use crate::batch::{BatchError, BatchPoster, CloudBatchClient, UreqBatchPoster};
use crate::config::{AppConfig, LocalConfig};
use crate::engine::WarmEngine;
use crate::models::ModelLeases;
use crate::offline_asr::{create_decoder, create_vad, Decoder, OfflineFamily};

use super::job::{FailureCode, JobFailure, RecognitionRequest, Segment, Transport, SAMPLE_RATE};

/// Catalog id of the voice activity detector every job segments with.
const VAD_MODEL: &str = "silero-vad";
const VAD_FILE: &str = "silero_vad.onnx";
/// Samples per detector window: the window `offline_asr::create_vad`
/// configures.
const VAD_WINDOW: usize = 512;
/// Characters of confirmed text handed to the next decode as context,
/// the bound of the live path (`offline_asr`).
const CONTEXT_CHARS: usize = 120;
/// Longest segment a cloud job makes: short enough to caption, long enough
/// for the service to read a whole sentence.
const CLOUD_SEGMENT_SECONDS: f32 = 15.0;
/// Why a cloud job without the user's permission fails, at submission and
/// again before a run decodes anything.
pub(crate) const NOT_AUTHORIZED: &str = "cloud transcription was selected without permission to send this media to the configured cloud service; nothing was uploaded";
const TOO_LONG: &str =
    "the media is longer than the voice activity detector can address (about 37 hours at 16 kHz)";

/// Longest media the detector can address. It indexes samples in 32-bit
/// signed integers: 37 h 15 min at 16 kHz.
pub(crate) const MAX_MEDIA_SAMPLES: u64 = i32::MAX as u64;

/// The failure of media longer than [`MAX_MEDIA_SAMPLES`].
pub(crate) fn too_long() -> JobFailure {
    FailureCode::Unsupported.failure(TOO_LONG).with_remedy(
        "Split the file into parts shorter than 37 hours and import them one after another.",
    )
}

/// What one run of a job turns the decoded stream into.
pub trait SegmentRecognizer: Send {
    /// Feed the next chunk of the stream and return the speech segments
    /// it closed, in media time counted from the start of the media.
    fn push(&mut self, chunk: &[f32]) -> Result<Vec<Segment>, JobFailure>;

    /// The stream ended: return the segments still open.
    fn finish(&mut self) -> Result<Vec<Segment>, JobFailure>;
}

/// What turns one speech segment into text. `context` is the tail of the
/// text confirmed before the segment.
pub trait SegmentDecoder: Send {
    fn decode(&self, samples: &[f32], context: &str) -> Result<String, JobFailure>;
}

/// Builds what recognizes a job, each time a run of it starts.
pub trait Recognition: Send + Sync {
    /// The recognizer of a run that starts at 16 kHz sample `origin` of
    /// the media.
    fn open(
        &self,
        request: &RecognitionRequest,
        origin: u64,
    ) -> Result<Box<dyn SegmentRecognizer>, JobFailure>;
}

/// What keeps the model files of a decoder in the store while the run
/// uses them.
enum Lease {
    /// The daemon's engine, which leases its models for as long as it lives.
    Engine { _engine: Arc<WarmEngine> },
    /// Leases this run took.
    Models { _leases: ModelLeases },
}

struct SenseVoiceLanguage {
    recognizer: sherpa_onnx::OfflineRecognizer,
    device: crate::offline_asr::InferenceDevice,
}

impl SenseVoiceLanguage {
    fn load(directory: &Path, language: &str, settings: &LocalConfig) -> Result<Self, JobFailure> {
        let mut config = sherpa_onnx::OfflineRecognizerConfig::default();
        let file = |name: &str| {
            directory
                .join(name)
                .into_os_string()
                .into_string()
                .map_err(|_| FailureCode::Model.failure("SenseVoice needs a UTF-8 model path"))
        };
        config.model_config.sense_voice.model = Some(file("model.int8.onnx")?);
        config.model_config.sense_voice.language = Some(language.to_owned());
        config.model_config.sense_voice.use_itn = true;
        config.model_config.tokens = Some(file("tokens.txt")?);
        config.model_config.provider = Some("cpu".to_owned());
        config.model_config.num_threads = settings.num_threads;
        let recognizer = sherpa_onnx::OfflineRecognizer::create(&config).ok_or_else(|| {
            FailureCode::Model.failure(format!("cannot load SenseVoice for {language}"))
        })?;
        Ok(Self {
            recognizer,
            device: crate::offline_asr::InferenceDevice::sherpa_cpu(),
        })
    }
}

impl Decoder for SenseVoiceLanguage {
    fn decode(&self, samples: &[f32], _context: &str) -> String {
        crate::offline_asr::decode(&self.recognizer, samples)
    }

    fn device(&self) -> &crate::offline_asr::InferenceDevice {
        &self.device
    }
}

/// A local offline model ready to transcribe segments, with the voice
/// activity detector and segment cap its segmentation uses.
pub struct LocalModel {
    decoder: Arc<dyn Decoder>,
    vad_model: PathBuf,
    segment_limit: f32,
    _lease: Lease,
}

impl LocalModel {
    /// Load catalog model `model` with the `[local]` device, threads and
    /// hotwords of `settings`.
    ///
    /// `warm` is used when its decoder was built for exactly that model
    /// and those settings. Otherwise the model is verified and loaded
    /// from the store, and its files leased for as long as this value
    /// lives. Blocking: a cold load takes seconds. Errors are `Model`
    /// failures naming what is missing or cannot run.
    pub fn load(
        model: &str,
        language: Option<&str>,
        settings: &LocalConfig,
        warm: Option<&Arc<WarmEngine>>,
    ) -> Result<Self, JobFailure> {
        let unavailable = |reason: String| FailureCode::Model.failure(reason);
        let mut settings = settings.clone();
        settings.select_file_model(model);
        let (decoder, vad_model, lease) =
            match warm.filter(|_| language.is_none()).and_then(|engine| {
                engine
                    .offline_for(&settings)
                    .map(|loaded| (Arc::clone(engine), loaded))
            }) {
                Some((engine, (decoder, vad_model))) => {
                    (decoder, vad_model, Lease::Engine { _engine: engine })
                }
                None => {
                    // Leased before anything is read, so a removal cannot slip
                    // in between the check and the load.
                    let leases = crate::models::lease_models([model, VAD_MODEL]);
                    let directory =
                        crate::models::installed_model_dir(model).map_err(unavailable)?;
                    let vad_model = crate::models::installed_model_dir(VAD_MODEL)
                        .map_err(unavailable)?
                        .join(VAD_FILE);
                    let family = OfflineFamily::for_model(model)
                        .map_err(|error| unavailable(error.to_string()))?;
                    let decoder: Arc<dyn Decoder> = if let Some(language) = language {
                        Arc::new(SenseVoiceLanguage::load(&directory, language, &settings)?)
                    } else {
                        create_decoder(
                            family,
                            &directory,
                            &crate::engine::offline_options(&settings),
                        )
                        .map_err(|error| unavailable(error.to_string()))?
                    };
                    (decoder, vad_model, Lease::Models { _leases: leases })
                }
            };
        Ok(Self {
            decoder,
            vad_model,
            segment_limit: settings.segment_limit(),
            _lease: lease,
        })
    }

    /// The voice activity detector model the segmentation loads.
    pub fn vad_model(&self) -> &Path {
        &self.vad_model
    }

    /// Longest speech segment the segmentation makes, in seconds.
    pub fn segment_limit(&self) -> f32 {
        self.segment_limit
    }
}

impl SegmentDecoder for LocalModel {
    fn decode(&self, samples: &[f32], context: &str) -> Result<String, JobFailure> {
        // A `Decoder` reports no failure: it answers an empty string for a
        // segment it could not decode, as it does in a live session.
        Ok(self.decoder.decode(samples, context))
    }
}

/// The DashScope batch endpoint as a segment decoder: one request per
/// segment, the segment as a 16 kHz mono WAV in the request body.
pub struct CloudDecoder<P: BatchPoster> {
    client: CloudBatchClient<P>,
}

impl<P: BatchPoster> CloudDecoder<P> {
    pub fn new(client: CloudBatchClient<P>) -> Self {
        Self { client }
    }
}

impl<P: BatchPoster + Send> SegmentDecoder for CloudDecoder<P> {
    fn decode(&self, samples: &[f32], _context: &str) -> Result<String, JobFailure> {
        self.client
            .transcribe_wav_bytes(&wav(samples))
            .map_err(cloud_failure)
    }
}

/// The failure state of a cloud request that did not produce text.
fn cloud_failure(error: BatchError) -> JobFailure {
    match error {
        BatchError::CloudAuth(0) => {
            FailureCode::CloudCredentials.failure("cloud transcription needs a `[cloud] api_key`")
        }
        BatchError::CloudAuth(status) => FailureCode::CloudCredentials.failure(format!(
            "the cloud service rejected the API key (HTTP {status})"
        )),
        BatchError::Cloud(reason) => FailureCode::CloudTransport.failure(format!(
            "the cloud service did not transcribe the segment: {reason}"
        )),
        other => FailureCode::Internal.failure(other.to_string()),
    }
}

/// `samples` as a 16 kHz mono PCM16 WAV.
fn wav(samples: &[f32]) -> Vec<u8> {
    let data_bytes = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_bytes);
    wav.extend_from_slice(&crate::capture::wav_header(data_bytes as u64, SAMPLE_RATE));
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        wav.extend_from_slice(&value.to_le_bytes());
    }
    wav
}

/// Speech found by Silero VAD in the decoded stream, each segment decoded
/// as it closes.
pub struct VadRecognizer {
    vad: VoiceActivityDetector,
    decoder: Box<dyn SegmentDecoder>,
    /// Media sample the stream starts at.
    origin: u64,
    /// Real samples handed to the detector since `origin`.
    fed: u64,
    /// Samples of the last chunk that did not fill a window.
    pending: Vec<f32>,
    /// Tail of the text confirmed so far: the context of the next decode.
    context: String,
}

impl VadRecognizer {
    /// Segment with the detector of `vad_model` (see [`LocalModel`]),
    /// capped at `segment_limit` seconds, and decode with `decoder`.
    /// The stream handed to this recognizer starts at 16 kHz sample
    /// `origin` of the media.
    pub fn new(
        vad_model: &Path,
        segment_limit: f32,
        decoder: Box<dyn SegmentDecoder>,
        origin: u64,
    ) -> Result<Self, JobFailure> {
        if origin >= MAX_MEDIA_SAMPLES {
            return Err(too_long());
        }
        let vad = create_vad(vad_model, segment_limit).map_err(|error| {
            FailureCode::Model.failure(format!("cannot load the voice activity detector: {error}"))
        })?;
        Ok(Self {
            vad,
            decoder,
            origin,
            fed: 0,
            pending: Vec::new(),
            context: String::new(),
        })
    }

    /// Decode the segments the detector has closed and drop them from its queue.
    fn close(&mut self, segments: &mut Vec<Segment>) -> Result<(), JobFailure> {
        while let Some(segment) = self.vad.front() {
            let start = segment.start();
            let samples = segment.samples();
            if start < 0 || samples.is_empty() {
                self.vad.pop();
                continue;
            }
            let text = self.decoder.decode(samples, &self.context)?;
            self.vad.pop();
            let start = self.origin + start as u64;
            // The last window may be padded silence.
            let end = (start + samples.len() as u64).min(self.origin + self.fed);
            if text.is_empty() || end <= start {
                continue;
            }
            self.context.push_str(&text);
            trim_context(&mut self.context);
            segments.push(Segment { start, end, text });
        }
        Ok(())
    }
}

impl SegmentRecognizer for VadRecognizer {
    fn push(&mut self, chunk: &[f32]) -> Result<Vec<Segment>, JobFailure> {
        self.pending.extend_from_slice(chunk);
        // Whole windows are taken out of `pending` for this call; what is
        // left over waits for the next chunk.
        let mut buffer = std::mem::take(&mut self.pending);
        let whole = buffer.len() - buffer.len() % VAD_WINDOW;
        let mut segments = Vec::new();
        let mut fed = 0;
        let result = buffer[..whole].chunks_exact(VAD_WINDOW).try_for_each(
            |window| -> Result<(), JobFailure> {
                if self.origin + self.fed >= MAX_MEDIA_SAMPLES {
                    return Err(too_long());
                }
                self.vad.accept_waveform(window);
                self.fed += VAD_WINDOW as u64;
                fed += VAD_WINDOW;
                self.close(&mut segments)
            },
        );
        buffer.drain(..fed);
        self.pending = buffer;
        result?;
        Ok(segments)
    }

    fn finish(&mut self) -> Result<Vec<Segment>, JobFailure> {
        let mut segments = Vec::new();
        if !self.pending.is_empty() {
            // The last partial window is padded with silence, so the
            // detector sees the end of the audio.
            let mut window = std::mem::take(&mut self.pending);
            let real = window.len();
            window.resize(VAD_WINDOW, 0.0);
            self.vad.accept_waveform(&window);
            self.fed += real as u64;
            self.close(&mut segments)?;
        }
        self.vad.flush();
        self.close(&mut segments)?;
        Ok(segments)
    }
}

/// Keep the last `CONTEXT_CHARS` characters of the confirmed text.
fn trim_context(context: &mut String) {
    let start = context
        .char_indices()
        .rev()
        .nth(CONTEXT_CHARS - 1)
        .map_or(0, |(start, _)| start);
    if start > 0 {
        context.drain(..start);
    }
}

/// What the runtime recognizes with: the applied configuration, and the
/// daemon's warm engine when there is one.
pub struct RuntimeRecognition {
    warm: Box<dyn Fn() -> Option<Arc<WarmEngine>> + Send + Sync>,
    config: Box<dyn Fn() -> AppConfig + Send + Sync>,
}

impl RuntimeRecognition {
    /// `warm` is the engine the daemon currently keeps loaded and
    /// `config` the configuration currently applied; both are read each
    /// time a run starts.
    pub fn new<W, C>(warm: W, config: C) -> Self
    where
        W: Fn() -> Option<Arc<WarmEngine>> + Send + Sync + 'static,
        C: Fn() -> AppConfig + Send + Sync + 'static,
    {
        Self {
            warm: Box::new(warm),
            config: Box::new(config),
        }
    }
}

impl Recognition for RuntimeRecognition {
    fn open(
        &self,
        request: &RecognitionRequest,
        origin: u64,
    ) -> Result<Box<dyn SegmentRecognizer>, JobFailure> {
        // Refused before anything is decoded or sent.
        if request.transport == Transport::Cloud && !request.cloud_authorized {
            return Err(FailureCode::CloudNotAuthorized.failure(NOT_AUTHORIZED));
        }
        let config = (self.config)();
        let (vad_model, segment_limit, decoder): (PathBuf, f32, Box<dyn SegmentDecoder>) =
            match request.transport {
                Transport::Local => {
                    let model = LocalModel::load(
                        &request.model,
                        request.language.as_deref(),
                        &config.local_engine(),
                        (self.warm)().as_ref(),
                    )?;
                    (
                        model.vad_model().to_path_buf(),
                        model.segment_limit(),
                        Box::new(model),
                    )
                }
                Transport::Cloud => {
                    if config.cloud.api_key.trim().is_empty() {
                        return Err(FailureCode::CloudCredentials.failure(
                            "cloud transcription is selected but `[cloud] api_key` is empty",
                        ));
                    }
                    let vad_model = crate::models::installed_model_dir(VAD_MODEL)
                        .map_err(|reason| FailureCode::Model.failure(reason))?
                        .join(VAD_FILE);
                    let client = CloudBatchClient::new(
                        config.cloud.base_url,
                        config.cloud.api_key,
                        request.model.clone(),
                        Duration::from_secs_f64(config.cloud.timeout_seconds),
                        UreqBatchPoster,
                    );
                    (
                        vad_model,
                        CLOUD_SEGMENT_SECONDS,
                        Box::new(CloudDecoder::new(client)),
                    )
                }
            };
        Ok(Box::new(VadRecognizer::new(
            &vad_model,
            segment_limit,
            decoder,
            origin,
        )?))
    }
}
