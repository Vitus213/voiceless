//! Sherpa-ONNX online (streaming) recognition: the `local-streaming`
//! backend (`[asr] streaming_backend = "local-streaming"`). Fully offline:
//! no network, no `[cloud]` or `[doubao]` keys, and none of the `[local]`
//! keys either (those belong to `local-offline`). The model directory is
//! `[asr] local_model_dir` or, by default, the managed catalog model
//! `streaming-paraformer-bilingual-zh-en`, which `stream.rs` resolves and
//! integrity-checks before every session; it must contain
//! `encoder.int8.onnx`, `decoder.int8.onnx` and `tokens.txt`.
//!
//! `LocalStreamingRecognizer` is synchronous and single-threaded (CPU, one
//! thread, greedy search). `stream.rs` wraps it in `LocalTransport`, which
//! synthesises the `Ready` event and queues the events returned by
//! `accept_pcm` and `finish` for the coordinator; `benchmark_cmd.rs` drives
//! it directly to replay corpus audio in 32 ms chunks. Ready gate: the
//! model is loaded in `new`, so once the transport exists the gate passes
//! immediately and a model problem is reported before any capture.
//!
//! Text production: after each chunk the recognizer decodes whatever the
//! model has ready; the decoded text of the open utterance is a `Partial`
//! (emitted only when it changes). When Sherpa's endpoint rules fire
//! (trailing silence, or a 20 s utterance) that text is appended to the
//! confirmed transcript as a `Completed` and the stream is reset. `finish`
//! marks the input complete, decodes the rest and reports `Finished` with
//! the confirmed text plus any trailing text; the trailing text is not
//! emitted as a `Completed` of its own. There is no cancel here:
//! `LocalTransport` stops feeding audio and drops the recognizer.
//!
//! Failure semantics: `new` returns a `LocalAsrError` naming the missing
//! directory or file, which `stream.rs` reports as a session `error` event
//! (eligible for `fallback_backend`); `accept_pcm` fails only on an odd byte
//! count. Decoding itself cannot fail. No retries or fallback inside.

use std::path::{Path, PathBuf};

use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};
use thiserror::Error;

use crate::realtime::RealtimeEvent;

const SAMPLE_RATE: i32 = 16_000;
// File names of the upstream `sherpa-onnx-streaming-paraformer-bilingual-zh-en`
// archive (int8 quantised encoder and decoder plus the token table).
const MODEL_ENCODER: &str = "encoder.int8.onnx";
const MODEL_DECODER: &str = "decoder.int8.onnx";
const MODEL_TOKENS: &str = "tokens.txt";

/// Why the local-streaming recognizer could not be built or fed. Every
/// variant names the offending path or size so the session error (and the
/// `--json` `error` event) tells the user what to install or fix.
#[derive(Debug, Error)]
pub enum LocalAsrError {
    /// The model directory is absent or not a directory.
    #[error("local-streaming model directory does not exist: {0}")]
    ModelDirectoryMissing(PathBuf),
    /// One of the three required files is missing (the path names it).
    #[error("local-streaming model is missing required file: {0}")]
    MissingModelFile(PathBuf),
    /// Sherpa needs UTF-8 path strings; this path cannot be converted.
    #[error("local-streaming model path is not valid UTF-8: {0}")]
    InvalidModelPath(PathBuf),
    /// Sherpa refused the configuration (corrupt or mismatched model files).
    #[error("local-streaming recognizer could not be created from model directory: {0}")]
    RecognizerUnavailable(PathBuf),
    /// `accept_pcm` got an odd number of bytes: not whole PCM16 samples.
    #[error("local-streaming received an incomplete PCM16 frame ({0} bytes)")]
    IncompletePcm(usize),
}

/// Resolved paths of the three required model files.
#[derive(Debug, Clone)]
struct ModelFiles {
    encoder: PathBuf,
    decoder: PathBuf,
    tokens: PathBuf,
}

/// One streaming Paraformer session: a Sherpa `OnlineRecognizer` plus its
/// single `OnlineStream`, and the transcript bookkeeping that turns Sherpa's
/// cumulative result into `Partial`/`Completed`/`Finished` events. Not
/// thread-safe by itself; `LocalTransport` guards it with a mutex. Owns the
/// model, so building one loads it (hundreds of MB) and dropping it frees it.
pub struct LocalStreamingRecognizer {
    recognizer: OnlineRecognizer,
    stream: OnlineStream,
    /// Scratch buffer for the PCM16 -> f32 conversion, reused per chunk.
    samples: Vec<f32>,
    /// Last partial emitted, to suppress identical repeats.
    last_partial: String,
    /// Text of all endpointed utterances so far, concatenated without
    /// separators (Chinese text needs none).
    confirmed_text: String,
}

impl LocalStreamingRecognizer {
    /// Load the model from `model_dir` and open one stream.
    ///
    /// Blocking and slow (model load). Errors: `ModelDirectoryMissing`,
    /// `MissingModelFile`, `InvalidModelPath`, `RecognizerUnavailable`.
    /// Fixed settings: CPU provider, one thread, greedy search, endpoint
    /// detection with Sherpa's three rules (2.4 s of trailing silence when
    /// nothing was decoded yet, 1.2 s once some text was decoded, or an
    /// utterance of 20 s regardless of silence).
    pub fn new(model_dir: impl AsRef<Path>) -> Result<Self, LocalAsrError> {
        let model_dir = model_dir.as_ref();
        let files = required_model_files(model_dir)?;
        let mut config = OnlineRecognizerConfig::default();
        config.model_config.paraformer.encoder = Some(path_string(&files.encoder)?);
        config.model_config.paraformer.decoder = Some(path_string(&files.decoder)?);
        config.model_config.tokens = Some(path_string(&files.tokens)?);
        config.model_config.provider = Some("cpu".to_owned());
        config.model_config.num_threads = 1;
        config.decoding_method = Some("greedy_search".to_owned());
        // Endpointing turns Sherpa's cumulative result into confirmed
        // segments: a long pause before any text (rule 1), a shorter pause
        // after text (rule 2), or a hard cap on utterance length (rule 3)
        // so the partial never grows unbounded.
        config.enable_endpoint = true;
        config.rule1_min_trailing_silence = 2.4;
        config.rule2_min_trailing_silence = 1.2;
        config.rule3_min_utterance_length = 20.0;

        let recognizer = OnlineRecognizer::create(&config)
            .ok_or_else(|| LocalAsrError::RecognizerUnavailable(model_dir.to_owned()))?;
        let stream = recognizer.create_stream();
        Ok(Self {
            recognizer,
            stream,
            samples: Vec::new(),
            last_partial: String::new(),
            confirmed_text: String::new(),
        })
    }

    /// Feed one PCM16 LE 16 kHz mono chunk and decode what is ready.
    ///
    /// Returns the events this chunk produced, in order: at most one
    /// `Completed` (endpoint reached) or one `Partial` (text changed), often
    /// none. Blocking (CPU decode, proportional to the audio fed). Errors:
    /// `IncompletePcm` for an odd byte count; nothing is fed in that case.
    pub fn accept_pcm(&mut self, pcm: &[u8]) -> Result<Vec<RealtimeEvent>, LocalAsrError> {
        if !pcm.len().is_multiple_of(2) {
            return Err(LocalAsrError::IncompletePcm(pcm.len()));
        }
        // PCM16 -> f32 in [-1, 1], as Sherpa expects; the buffer is reused
        // so the per-chunk hot path does not allocate.
        self.samples.clear();
        self.samples.reserve(pcm.len() / 2);
        for bytes in pcm.chunks_exact(2) {
            let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
            self.samples.push(f32::from(sample) / f32::from(i16::MAX));
        }
        self.stream.accept_waveform(SAMPLE_RATE, &self.samples);
        Ok(self.decode_ready())
    }

    /// End the input, decode the remainder and report the final transcript.
    ///
    /// Returns any `Completed`/`Partial` the remaining audio produced,
    /// followed by exactly one `Finished` whose transcript is the confirmed
    /// text plus whatever the open utterance still held. Call once; the
    /// stream is not reusable afterwards. Never fails (the `Result` is for
    /// interface symmetry with `accept_pcm`).
    pub fn finish(&mut self) -> Result<Vec<RealtimeEvent>, LocalAsrError> {
        self.stream.input_finished();
        let mut events = self.decode_ready();
        // Text left in the stream after the last decode never saw an
        // endpoint; it goes straight into the final transcript.
        let trailing = self.current_text();
        let transcript = if trailing.is_empty() {
            self.confirmed_text.clone()
        } else {
            format!("{}{}", self.confirmed_text, trailing)
        };
        events.push(RealtimeEvent::Finished { transcript });
        Ok(events)
    }

    /// Run the decoder for every frame Sherpa has buffered, then turn the
    /// current result into events: an endpoint confirms the text and resets
    /// the stream (so the next result starts empty); otherwise a changed,
    /// non-empty text is a new partial.
    fn decode_ready(&mut self) -> Vec<RealtimeEvent> {
        while self.recognizer.is_ready(&self.stream) {
            self.recognizer.decode(&self.stream);
        }

        let text = self.current_text();
        if self.recognizer.is_endpoint(&self.stream) {
            let mut events = Vec::new();
            if !text.is_empty() {
                self.confirmed_text.push_str(&text);
                events.push(RealtimeEvent::Completed { transcript: text });
            }
            self.last_partial.clear();
            // Reset clears the decoded text but keeps the model state, so
            // the next utterance starts from an empty result.
            self.recognizer.reset(&self.stream);
            events
        } else if !text.is_empty() && text != self.last_partial {
            self.last_partial = text.clone();
            vec![RealtimeEvent::Partial {
                text,
                stash: String::new(),
            }]
        } else {
            Vec::new()
        }
    }

    /// Sherpa's cumulative text for the current (not yet reset) utterance.
    fn current_text(&self) -> String {
        self.recognizer
            .get_result(&self.stream)
            .map(|result| result.text)
            .unwrap_or_default()
    }
}

/// Check the directory and the three required files before touching Sherpa,
/// so a half-installed model yields a precise error instead of a generic
/// `RecognizerUnavailable`.
fn required_model_files(model_dir: &Path) -> Result<ModelFiles, LocalAsrError> {
    if !model_dir.is_dir() {
        return Err(LocalAsrError::ModelDirectoryMissing(model_dir.to_owned()));
    }
    let encoder = required_file(model_dir, MODEL_ENCODER)?;
    let decoder = required_file(model_dir, MODEL_DECODER)?;
    let tokens = required_file(model_dir, MODEL_TOKENS)?;
    Ok(ModelFiles {
        encoder,
        decoder,
        tokens,
    })
}

/// `model_dir/name` if it is a regular file, else `MissingModelFile`.
fn required_file(model_dir: &Path, name: &str) -> Result<PathBuf, LocalAsrError> {
    let path = model_dir.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(LocalAsrError::MissingModelFile(path))
    }
}

/// Sherpa takes `String` paths; non-UTF-8 paths are rejected explicitly.
fn path_string(path: &Path) -> Result<String, LocalAsrError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| LocalAsrError::InvalidModelPath(path.to_owned()))
}
