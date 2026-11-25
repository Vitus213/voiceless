#![allow(async_fn_in_trait)]
//! Single owner of one recognition session: capture lifecycle, backend
//! transport, transcript accumulation and the one-shot final injection.
//!
//! The coordinator enforces the fixed stop order (stop accepting chunks ->
//! capture stop -> FIFO drain of queued chunks and the tail frame -> single
//! backend finish -> final event -> optional single injection) and
//! guarantees that cancel/error paths never inject partial text. Audio
//! backlog is bounded: queue overrun or a send deadline breach fails the
//! session instead of dropping or reordering audio.
//!
//! `run_session` is the only place where the realtime pipeline touches its
//! side effects, and every side is a trait, so the CLI (`stream.rs`), the
//! daemon (`daemon.rs` through `stream::run_with_env`) and the lifecycle
//! tests (`tests/audio_backpressure.rs`, `tests/session_failures.rs`,
//! `tests/session_finalize.rs`, `tests/final_pipeline.rs`,
//! `tests/audio_retention.rs`) share one implementation:
//!
//! - `AudioCapture`: the microphone (`capture::RawCapture` behind
//!   `stream::PwCapture`), always wrapped here in a `capture::WavRecorder`;
//! - `BackendTransport`: cloud or local ASR speaking `realtime::RealtimeEvent`;
//! - `TextProcessor`: `[text]` cleanup plus the optional LLM mode;
//! - `TextInjector`: typing into the focused window;
//! - `HistoryRecorder`: the SQLite history row;
//! - `EventSink`: where `OutputEvent`s go (stdout, the daemon broadcast, a
//!   test vector). `stream::json_event` maps each variant to one
//!   `stream --json` line; the variant docs name the `type` values.
//!
//! Lifecycle, in order:
//!
//! 1. Ready gate: wait for `RealtimeEvent::Ready` (at most
//!    `SessionPlan::ready_timeout`) before any capture exists, so an
//!    authentication or connection failure produces no audio.
//! 2. Recording: chunks pass through a bounded FIFO queue
//!    (`SessionPlan::queue_capacity`, default 16 slots of one 32 ms chunk).
//!    A full queue, or one `send_audio` call exceeding
//!    `SessionPlan::send_deadline` (default 500 ms), fails the session.
//! 3. Stop flush: stop accepting chunks -> graceful capture stop -> queue
//!    the aligned tail -> drain FIFO -> exactly one `finish`. The shared
//!    deadline also covers final events except from the owned offline worker.
//!    Control commands can cancel every step.
//! 4. Final pipeline: `cleanup`, then `process` unless the mode is `quick`
//!    (a processing failure keeps the cleaned transcript and emits a
//!    warning), at most one `inject`, one history row, then `Finalized`.
//!
//! Guarantees: every path except a failing sink ends with exactly one
//! `OutputEvent::Completed`; cancel and error paths never inject, never
//! record history and never keep an unfinished WAV file. Duplicate stop
//! requests leave the flush running. The return value is the process exit
//! code: `0` success (also for "no speech"), `1` failure, `130` cancelled.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::timeout_at;

use crate::realtime::RealtimeEvent;
use crate::session::{RecognitionSession, SessionState, SessionUpdate, TranscriptSnapshot};

/// Default bound of the audio queue in chunks: 16 slots of one 32 ms
/// `capture::CHUNK_BYTES` chunk, about half a second of backlog.
pub const DEFAULT_QUEUE_CAPACITY: usize = 16;
/// Default limit for one `BackendTransport::send_audio` call while
/// recording; a slower send fails the session instead of growing a backlog.
pub const DEFAULT_SEND_DEADLINE: Duration = Duration::from_millis(500);
/// Default deadline for stop and flush. The local offline worker reports
/// its own completion or failure after the finish request is queued.
pub const DEFAULT_FINISH_TIMEOUT: Duration = Duration::from_secs(2);
/// Default wait for `RealtimeEvent::Ready` before any capture starts.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Command from the control source. `stream::control_receiver` maps
/// SIGINT/SIGTERM; the daemon gateway maps the hotkey and D-Bus calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// Finish: stop capturing, flush and inject the final text once.
    Stop,
    /// Abort now: no finish, no final text, no injection, no history row;
    /// the session exits with code 130.
    Cancel,
}

/// Outcome of the single injection attempt, carried by
/// `OutputEvent::Finalized` (the `"injection"` object of `stream --json`).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct InjectionResult {
    /// Whether the text left Voiceless: committed, sent or copied.
    pub ok: bool,
    /// Method that produced this result: `wtype`, `clipboard`, `fcitx5`
    /// or `copy`. After a clipboard fallback this is `clipboard` when the
    /// fallback succeeded, otherwise the primary method with its own message.
    pub method: String,
    /// Failure reason (tool stderr, bridge reply, timeout, the focus that
    /// moved); empty on success.
    pub message: String,
    /// How much is known about the delivery.
    pub delivery: Delivery,
}

/// How much of a delivery is known, as the console reports it.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    /// Nothing reached the user's application: not requested, withheld or failed.
    #[default]
    NotDelivered,
    /// The focused input context took the text (an Fcitx5 commit).
    Committed,
    /// A send may have reached the application without a receipt, including
    /// when its tool failed after it had already started sending.
    Unconfirmed,
    /// The text is on the clipboard for the user to paste.
    Copied,
}

/// Observable work after capture has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionPhase {
    Processing,
    Delivering,
    NoSpeech,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackendFallback {
    pub primary: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackendSelection {
    pub backend: String,
    pub model: String,
    pub requested_device: Option<String>,
    pub fallback: Option<BackendFallback>,
}

/// Observable session events, in the order a sink receives them. Each
/// variant becomes one `stream --json` line whose `"type"` is given below
/// (`stream::json_event`), with a per-session `"sequence"` counter; the
/// daemon's D-Bus `Event` signal and `voiceless ctl watch` carry the same
/// lines.
#[derive(Debug, Clone)]
pub enum OutputEvent {
    /// `"ready"`: the backend accepted the session and capture started;
    /// speech is recorded from this point on.
    Ready,
    /// The backend whose startup succeeded and why it replaced the primary.
    BackendSelected(BackendSelection),
    /// The actual decoder devices, available after model load.
    DevicesLoaded(Box<crate::offline_asr::EngineDevices>),
    /// `"level"` (field `"rms"`): mic level (RMS, 0..1) while recording, at
    /// most ~15 Hz; only when `SessionPlan::emit_levels` (live UIs) is set.
    Level(f32),
    /// `"transcript"` (field `"transcript"`): the accumulated text so far.
    /// The snapshot with `is_final == true` is emitted at most once, after
    /// `finish`, and only when its authoritative text is not empty.
    Transcript(TranscriptSnapshot),
    /// A change in what the session is doing after capture.
    Phase(SessionPhase),
    /// Mode selection and receipts from the output steps that ran.
    Output(Box<crate::output::Trace>),
    /// `"finalized"` (field `"injection"`): the final pipeline ran.
    Finalized {
        /// `None` when injection was disabled or no text was recognized.
        injection: Option<InjectionResult>,
    },
    /// `"warning"` (field `"message"`): non-fatal (no speech, processing
    /// failure, backend fallback); the session still exits with 0.
    Warning(String),
    /// `"error"` (field `"message"`): the session failed (exit code 1).
    /// Always followed by `Completed`; nothing was injected.
    Error(String),
    /// `"cancelled"`: the session was cancelled (exit code 130). Always
    /// followed by `Completed`; nothing was injected.
    Cancelled,
    /// `"completed"`: last event of every session, whatever the outcome.
    Completed,
}

/// Per-session parameters chosen by the caller (`stream::session_plan`
/// derives them from the config and CLI flags; tests start from `new`).
#[derive(Debug, Clone)]
pub struct SessionPlan {
    /// Backend id recorded in snapshots and history (`cloud-realtime`,
    /// `doubao-realtime`, `local-streaming`, `local-offline`).
    pub backend: String,
    /// Whether to call the injector with the final text (`--no-inject`
    /// clears it); processing and history happen either way.
    pub inject: bool,
    /// Processing mode id; `quick` skips `TextProcessor::process`.
    pub mode_id: String,
    /// Bound of the audio queue in chunks; see `DEFAULT_QUEUE_CAPACITY`.
    pub queue_capacity: usize,
    /// Per-send limit while recording; see `DEFAULT_SEND_DEADLINE`.
    pub send_deadline: Duration,
    /// Stop-flush deadline, including final events for realtime backends.
    pub finish_timeout: Duration,
    /// Wait for the backend's `Ready`; see `DEFAULT_READY_TIMEOUT`.
    pub ready_timeout: Duration,
    /// Directory where each successful session's WAV recording is saved;
    /// `None` disables audio retention.
    pub save_audio_dir: Option<PathBuf>,
    /// Failed audio is staged separately when successful recordings are disabled.
    pub failure_recovery: Option<FailureRecovery>,
    /// Emit `OutputEvent::Level` while recording (daemon sessions only).
    pub emit_levels: bool,
    /// Model identifier recorded with the history row (`asr_model`): the
    /// cloud model name, the Doubao resource id or the local catalog id.
    /// Empty when unknown (tests), stored as NULL.
    pub model: String,
}

/// Destination and provenance for explicitly enabled failed-session recovery.
#[derive(Debug, Clone)]
pub struct FailureRecovery {
    pub directory: PathBuf,
    pub app_id: Option<String>,
}

/// Minimum spacing of `OutputEvent::Level`.
const LEVEL_INTERVAL: Duration = Duration::from_millis(66);

impl SessionPlan {
    /// Plan with the default bounds and timeouts, mode `quick`, no audio
    /// retention and no level events.
    pub fn new(backend: impl Into<String>, inject: bool) -> Self {
        Self {
            backend: backend.into(),
            inject,
            mode_id: "quick".to_owned(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            send_deadline: DEFAULT_SEND_DEADLINE,
            finish_timeout: DEFAULT_FINISH_TIMEOUT,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            save_audio_dir: None,
            failure_recovery: None,
            emit_levels: false,
            model: String::new(),
        }
    }
}

/// RMS of a PCM16 little-endian chunk, scaled to 0..1.
fn pcm_rms(pcm: &[u8]) -> f32 {
    let samples = pcm.len() / 2;
    if samples == 0 {
        return 0.0;
    }
    let sum: f64 = pcm
        .chunks_exact(2)
        .map(|bytes| {
            let sample = f64::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32768.0;
            sample * sample
        })
        .sum();
    (sum / samples as f64).sqrt() as f32
}

/// Audio source boundary. `start` MUST be called only after the backend is
/// ready; `stop_capture` performs a graceful stop and returns an optional
/// aligned tail frame; `abort` terminates capture without draining.
///
/// Chunks are 16 kHz mono PCM16 little-endian. `next_chunk` is polled
/// inside a `tokio::select!` and its future is dropped whenever another
/// branch wins, so it must be cancellation safe (no bytes lost).
pub trait AudioCapture {
    /// Open or spawn the source. An error fails the session before `Ready`.
    async fn start(&mut self) -> io::Result<()>;
    /// Next PCM chunk; `Ok(None)` once the source ended, which the
    /// coordinator treats like a stop request.
    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>>;
    /// Graceful stop: let the source flush and return what it still had
    /// buffered. An odd byte count (a torn sample) fails the session.
    async fn stop_capture(&mut self) -> io::Result<Option<Vec<u8>>>;
    /// Hard stop on cancel and failure; wait for capture resources to be released.
    async fn abort(&mut self) {}
    /// Finalize a saved recording after a successful session; `None` when
    /// nothing was recorded or saving is disabled. Never fails the session.
    fn finish_recording(&mut self) -> Option<PathBuf> {
        None
    }
}

/// Realtime backend boundary. Events follow the same semantics for cloud and
/// local backends (`RealtimeEvent`). `send_audio` and `next_event` share the
/// session concurrently; `finish` and `cancel` terminate it.
///
/// While recording, `send_audio` and `next_event` are polled inside a
/// `tokio::select!` and may be dropped before completion; the coordinator
/// then retries the same chunk, so a send must either deliver the chunk or
/// leave no trace of it.
pub trait BackendTransport {
    /// Deliver one PCM16 chunk, in capture order. Called from the recording
    /// loop (bounded by `SessionPlan::send_deadline`) and the stop flush.
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()>;
    /// Next backend event: `Ready` first, `Finished` last (after `finish`).
    /// An `Err` fails the session.
    async fn next_event(&self) -> io::Result<RealtimeEvent>;
    /// Tell the backend no more audio follows. Called exactly once per
    /// session, after the last chunk and the tail frame.
    async fn finish(&mut self) -> io::Result<()>;
    /// Abandon the session (cancel and failure paths); its result is ignored.
    async fn cancel(&mut self) -> io::Result<()>;
}

/// Injection boundary: type the final text into the focused window. Called
/// at most once per session, only after a successful finish, never with
/// partial text. The result is reported in `OutputEvent::Finalized`; the
/// coordinator does not retry.
pub trait TextInjector {
    /// Inject `text`; never fails the session, the result is only reported.
    async fn inject(&mut self, text: &str) -> InjectionResult;
}

pub struct CompletedText {
    pub text: String,
    pub modified: bool,
    pub error: Option<String>,
    pub trace: Option<crate::output::Trace>,
    pub may_deliver: bool,
}

/// Final-text processing boundary. `quick` mode never calls this; other
/// modes send the authoritative transcript through it.
pub trait TextProcessor {
    /// Transform the cleaned transcript for `mode_id` (an LLM call in
    /// production). An `Err` is reported as a warning and the cleaned text
    /// is injected instead; it never fails the session.
    async fn process(&mut self, mode_id: &str, text: &str) -> Result<String, String>;

    /// Rule-based cleanup of every final transcript (fillers, hotwords,
    /// replacements), in every mode and before `process`.
    fn cleanup(&mut self, text: &str) -> String {
        text.to_owned()
    }

    fn announces_processing(&self, mode_id: &str) -> bool {
        mode_id != "quick"
    }

    async fn finalize(&mut self, mode_id: &str, raw: &str) -> CompletedText {
        let mut text = self.cleanup(raw);
        let mut modified = text != raw;
        let mut error = None;
        if mode_id != "quick" {
            match self.process(mode_id, &text).await {
                Ok(processed) => {
                    text = processed;
                    modified = true;
                }
                Err(reason) => {
                    error = Some(reason);
                    modified = false;
                }
            }
        }
        CompletedText {
            text,
            modified,
            error,
            trace: None,
            may_deliver: true,
        }
    }
}

/// History boundary. Only successful sessions with authoritative text are
/// recorded; cancelled, failed and empty sessions never reach here.
pub trait HistoryRecorder {
    /// Persist one completed session. Cannot fail the session: the
    /// recorder handles its own errors (and removes an orphaned recording).
    fn record(&mut self, entry: HistoryEntry);
}

/// One completed session as handed to `HistoryRecorder::record`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// Authoritative transcript exactly as the backend returned it.
    pub raw_text: String,
    /// In `quick` mode the cleaned text, only when cleanup changed it; in
    /// other modes the processor output, `None` when processing failed.
    pub processed_text: Option<String>,
    /// The text that was injected (or would have been, with `inject` off).
    pub final_text: String,
    /// `SessionPlan::mode_id`.
    pub processing_mode: String,
    /// Always `"completed"`: only successful sessions are recorded.
    pub status: String,
    /// `SessionPlan::backend`.
    pub backend: String,
    /// Model the backend ran (`SessionPlan::model`); `None` when unknown.
    pub model: Option<String>,
    /// Captured audio length: bytes / 32 000 (16 kHz mono PCM16);
    /// `None` when less than one sample was captured.
    pub duration_seconds: Option<f64>,
    /// Path of the retained WAV when audio retention is on and the file was
    /// finalized; `None` otherwise.
    pub audio_path: Option<String>,
}

/// Event output boundary. `emit` is synchronous and should be cheap. Its
/// errors are ignored except for `Ready`: a sink that cannot deliver
/// `Ready` aborts the session with exit code 1.
pub trait EventSink {
    /// Deliver one event; the sink owns the formatting (JSON line, plain
    /// text, broadcast, in-memory vector).
    fn emit(&mut self, event: OutputEvent) -> io::Result<()>;

    /// Retain the completed delivery text if this sink supports explicit repaste.
    fn retain_final_text(&mut self, _text: &str) {}
}

/// Control input wrapper: a closed channel stops delivering commands and,
/// once recording has started, is never treated as a cancellation itself
/// (during the ready gate it cancels like any command would).
struct ControlInput {
    receiver: mpsc::Receiver<ControlCommand>,
    /// Set once `recv` returned `None`; the branch is then disabled so the
    /// `select!` does not spin on a closed channel.
    closed: bool,
}

impl ControlInput {
    fn new(receiver: mpsc::Receiver<ControlCommand>) -> Self {
        Self {
            receiver,
            closed: false,
        }
    }
}

/// Outcome of one awaited flush step (`timed_step`).
enum Step<T> {
    /// The step completed with this value.
    Value(T),
    /// The shared stop deadline passed first.
    TimedOut,
    /// Explicit cancel before delivery.
    Cancelled,
}

pub(crate) enum ReadyError {
    Cancelled,
    Failed(String),
}

/// Control wins until capture starts. Only the backend's acknowledgement
/// can open the gate, so an error can still be handled without any audio.
pub(crate) async fn wait_until_ready<T: BackendTransport>(
    transport: &T,
    control: &mut mpsc::Receiver<ControlCommand>,
    deadline: tokio::time::Instant,
) -> Result<(), ReadyError> {
    loop {
        let message = tokio::select! {
            biased;
            _ = control.recv() => return Err(ReadyError::Cancelled),
            result = timeout_at(deadline, transport.next_event()) => match result {
                Ok(Ok(RealtimeEvent::Ready)) => return Ok(()),
                Ok(Ok(RealtimeEvent::Error(message))) => message,
                Ok(Ok(_)) => continue,
                Ok(Err(error)) => error.to_string(),
                Err(_) => return Err(ReadyError::Failed("backend ready timed out".to_owned())),
            },
        };
        return Err(ReadyError::Failed(format!("backend ready: {message}")));
    }
}

/// Run one recognition session to completion and return the process exit
/// code: `0` success, `1` failure, `130` cancellation.
///
/// `plan` selects the backend id, mode, bounds and audio retention.
/// `capture` is wrapped in a `capture::WavRecorder` writing into
/// `plan.save_audio_dir` (created on demand; a failure there only disables
/// saving). `control` delivers `Stop`/`Cancel`; a closed channel is ignored
/// once recording has started. Events are pushed to `sink` as they happen;
/// see the module doc for the ordering guarantees.
///
/// Side effects, all behind the traits: starts and stops the capture
/// process, drives the backend transport, calls the injector at most once,
/// writes at most one history row and one WAV file.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<C, T, P, J, H, S>(
    plan: SessionPlan,
    mut capture: C,
    mut transport: T,
    processor: P,
    injector: J,
    history: H,
    mut control: mpsc::Receiver<ControlCommand>,
    sink: &mut S,
) -> i32
where
    C: AudioCapture,
    T: BackendTransport,
    P: TextProcessor,
    J: TextInjector,
    H: HistoryRecorder,
    S: EventSink,
{
    let deadline = tokio::time::Instant::now() + plan.ready_timeout;
    match wait_until_ready(&transport, &mut control, deadline).await {
        Ok(()) => {}
        Err(ReadyError::Cancelled) => return cancel_path(&mut capture, &mut transport, sink).await,
        Err(ReadyError::Failed(message)) => {
            let _ = transport.cancel().await;
            return fail(sink, &message);
        }
    }
    run_ready_session(
        plan, capture, transport, processor, injector, history, control, sink,
    )
    .await
}

/// Run the capture and delivery phases after a backend has been selected
/// and passed the readiness gate. These phases never switch backends.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_ready_session<C, T, P, J, H, S>(
    plan: SessionPlan,
    capture: C,
    mut transport: T,
    mut processor: P,
    mut injector: J,
    mut history: H,
    control: mpsc::Receiver<ControlCommand>,
    sink: &mut S,
) -> i32
where
    C: AudioCapture,
    T: BackendTransport,
    P: TextProcessor,
    J: TextInjector,
    H: HistoryRecorder,
    S: EventSink,
{
    let mut control = ControlInput::new(control);
    let mut session = RecognitionSession::new(plan.backend.clone());
    // Audio retention: one fresh file name per session; an uncreatable
    // directory silently disables saving (never the session).
    let destination_dir = plan.save_audio_dir.as_ref().or_else(|| {
        plan.failure_recovery
            .as_ref()
            .map(|recovery| &recovery.directory)
    });
    let audio_destination = destination_dir.and_then(|dir| {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(dir)
            .ok()
            .map(|_| dir.join(format!("{}.wav", new_audio_stem())))
    });
    let mut capture =
        crate::capture::WavRecorder::new(capture, crate::capture::SAMPLE_RATE, audio_destination);
    if let Err(error) = capture.start().await {
        let _ = transport.cancel().await;
        return fail(sink, &format!("capture: {error}"));
    }
    // A sink that cannot deliver `Ready` cannot deliver anything else:
    // give up without the usual Error/Completed pair. Capture and
    // transport are released by drop.
    if sink.emit(OutputEvent::Ready).is_err() {
        return 1;
    }

    // FIFO backlog between capture and backend; bounded by the plan.
    let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
    let mut head_deadline = tokio::time::Instant::now();
    // Total PCM bytes accepted (chunks + tail), for the history duration.
    let mut audio_bytes: u64 = 0;
    let mut last_level = tokio::time::Instant::now();
    let mut prefer_capture = false;

    let recognition: Result<(), String> = 'recognize: {
        loop {
            // Control and pending sends win. After a backend event, give
            // capture a turn so a burst of events cannot delay audio or EOF.
            tokio::select! {
                biased;
                command = control.receiver.recv(), if !control.closed => match command {
                    Some(ControlCommand::Stop) => {
                        let _ = session.request_stop();
                        break;
                    }
                    Some(ControlCommand::Cancel) => return cancel_path(&mut capture, &mut transport, sink).await,
                    None => control.closed = true,
                },
                send = timeout_at(head_deadline, transport.send_audio(queue.front().map(Vec::as_slice).unwrap_or(&[]))), if !queue.is_empty() => {
                    match send {
                        Ok(Ok(())) => {
                            queue.pop_front();
                            head_deadline = tokio::time::Instant::now() + plan.send_deadline;
                        }
                        Ok(Err(error)) => {
                            break 'recognize Err(format!("audio send: {error}"));
                        }
                        Err(_) => {
                            break 'recognize Err("audio send exceeded the deadline".to_owned());
                        }
                    }
                }
                // Backend events while recording: partials and confirmed
                // segments update the transcript; `Error` fails the session;
                // a `Finished` before any stop is ignored by the state machine.
                event = transport.next_event(), if !prefer_capture => match event {
                    Ok(event) => {
                        prefer_capture = true;
                        if let Err(reason) = dispatch_event(&mut session, sink, event) {
                            break 'recognize Err(reason);
                        }
                    }
                    Err(error) => break 'recognize Err(format!("realtime receive: {error}")),
                },
                chunk = capture.next_chunk() => match chunk {
                    Ok(Some(pcm)) => {
                        prefer_capture = false;
                        if queue.len() >= plan.queue_capacity {
                            break 'recognize Err("audio backlog exceeded the bounded queue capacity".to_owned());
                        }
                        audio_bytes += pcm.len() as u64;
                        if plan.emit_levels && last_level.elapsed() >= LEVEL_INTERVAL {
                            last_level = tokio::time::Instant::now();
                            let _ = sink.emit(OutputEvent::Level(pcm_rms(&pcm)));
                        }
                        if queue.is_empty() {
                            head_deadline = tokio::time::Instant::now() + plan.send_deadline;
                        }
                        queue.push_back(pcm);
                    }
                    // A finite source ended normally. The live microphone
                    // adapter reports unexpected EOF as an error instead.
                    Ok(None) => {
                        if session.state() == SessionState::Recording {
                            let _ = session.request_stop();
                        }
                        break;
                    }
                    Err(error) => break 'recognize Err(format!("capture: {error}")),
                },
                () = std::future::ready(()), if prefer_capture => prefer_capture = false,
            }
        }

        // Stop flush: graceful capture stop, then FIFO drain of the queue and
        // the tail frame, then exactly one finish. Cancel can interrupt the flush.
        let stop_deadline = tokio::time::Instant::now() + plan.finish_timeout;
        let tail = match timed_step(&mut control, Some(stop_deadline), capture.stop_capture()).await
        {
            Step::Value(Ok(tail)) => tail,
            Step::Value(Err(error)) => break 'recognize Err(format!("capture stop: {error}")),
            Step::TimedOut => break 'recognize Err("capture stop timed out".to_owned()),
            Step::Cancelled => return cancel_path(&mut capture, &mut transport, sink).await,
        };
        if let Some(tail) = tail {
            // Half a sample would misalign every later PCM16 frame: refuse it
            // rather than hand the backend corrupt audio.
            if tail.len() % 2 != 0 {
                break 'recognize Err(format!(
                    "incomplete PCM16 tail frame ({} bytes)",
                    tail.len()
                ));
            }
            if !tail.is_empty() {
                audio_bytes += tail.len() as u64;
                queue.push_back(tail);
            }
        }
        while let Some(pcm) = queue.pop_front() {
            match timed_step(
                &mut control,
                Some(stop_deadline),
                transport.send_audio(&pcm),
            )
            .await
            {
                Step::Value(Ok(())) => {}
                Step::Value(Err(error)) => break 'recognize Err(format!("flush send: {error}")),
                Step::TimedOut => break 'recognize Err("flush send timed out".to_owned()),
                Step::Cancelled => return cancel_path(&mut capture, &mut transport, sink).await,
            }
        }
        match timed_step(&mut control, Some(stop_deadline), transport.finish()).await {
            Step::Value(Ok(())) => {}
            Step::Value(Err(error)) => break 'recognize Err(format!("backend finish: {error}")),
            Step::TimedOut => break 'recognize Err("backend finish timed out".to_owned()),
            Step::Cancelled => return cancel_path(&mut capture, &mut transport, sink).await,
        }

        // Local offline finish only queues the request. Wait for the owned
        // worker's result or channel closure while still accepting cancellation.
        let final_deadline = (plan.backend != "local-offline").then_some(stop_deadline);
        loop {
            let step = timed_step(&mut control, final_deadline, transport.next_event()).await;
            let event = match step {
                Step::Value(Ok(event)) => event,
                Step::Value(Err(error)) => break 'recognize Err(format!("final receive: {error}")),
                Step::TimedOut => break 'recognize Err("final event timed out".to_owned()),
                Step::Cancelled => return cancel_path(&mut capture, &mut transport, sink).await,
            };
            if let Err(reason) = dispatch_event(&mut session, sink, event) {
                break 'recognize Err(reason);
            }
            if session.state() == SessionState::Completed {
                break 'recognize Ok(());
            }
        }
    };
    if let Err(reason) = recognition {
        let recording = plan
            .failure_recovery
            .as_ref()
            .and_then(|_| capture.failed_recording());
        capture.abort().await;
        let _ = transport.cancel().await;
        if let (Some(recovery), Some(wav)) = (&plan.failure_recovery, recording) {
            use crate::history::retention::{FailedSession, RecoveryStore};
            let metadata = FailedSession {
                reason: &reason,
                backend: &plan.backend,
                mode: &plan.mode_id,
                app_id: recovery.app_id.as_deref(),
            };
            if let Err(error) = RecoveryStore::new(recovery.directory.clone()).keep(&wav, metadata)
            {
                let _ = sink.emit(OutputEvent::Warning(format!(
                    "could not keep the failed recording: {error}"
                )));
            }
        }
        return fail(sink, &reason);
    }

    // Final pipeline. `take` hands the text out once, which is what bounds
    // injection to a single attempt.
    match session.take_injection_text() {
        Some(raw_text) => {
            let (final_text, processed_text, may_deliver) =
                if plan.mode_id == crate::modes::RAW_MODE_ID {
                    (raw_text.clone(), None, true)
                } else {
                    if processor.announces_processing(&plan.mode_id) {
                        let _ = sink.emit(OutputEvent::Phase(SessionPhase::Processing));
                    }
                    let outcome = match timed_step(
                        &mut control,
                        None,
                        processor.finalize(&plan.mode_id, &raw_text),
                    )
                    .await
                    {
                        Step::Value(outcome) => outcome,
                        Step::Cancelled => {
                            return cancel_path(&mut capture, &mut transport, sink).await
                        }
                        Step::TimedOut => unreachable!("no processing deadline was supplied"),
                    };
                    if let Some(trace) = outcome.trace {
                        let _ = sink.emit(OutputEvent::Output(Box::new(trace)));
                    }
                    if let Some(error) = outcome.error {
                        let _ = sink.emit(OutputEvent::Warning(format!(
                            "processing failed, keeping recognized text: {error}"
                        )));
                    }
                    let processed = outcome.modified.then(|| outcome.text.clone());
                    (outcome.text, processed, outcome.may_deliver)
                };
            if matches!(
                timed_step(&mut control, None, std::future::ready(())).await,
                Step::Cancelled
            ) {
                return cancel_path(&mut capture, &mut transport, sink).await;
            }
            let injection = if plan.inject && may_deliver && !final_text.is_empty() {
                let _ = sink.emit(OutputEvent::Phase(SessionPhase::Delivering));
                Some(injector.inject(&final_text).await)
            } else {
                None
            };
            sink.retain_final_text(&final_text);
            history.record(HistoryEntry {
                raw_text,
                processed_text,
                final_text,
                processing_mode: plan.mode_id.clone(),
                status: "completed".to_owned(),
                backend: plan.backend.clone(),
                model: (!plan.model.is_empty()).then(|| plan.model.clone()),
                // 16 kHz mono PCM16 is 32 000 bytes per second.
                duration_seconds: (audio_bytes >= 2).then(|| audio_bytes as f64 / 32_000.0),
                audio_path: plan
                    .save_audio_dir
                    .as_ref()
                    .and_then(|_| capture.finish_recording())
                    .map(|path| path.display().to_string()),
            });
            let _ = sink.emit(OutputEvent::Finalized { injection });
        }
        // Silence or nothing recognized: success without injection or
        // history; the recorder is dropped unfinished, so no WAV remains.
        None => {
            let _ = sink.emit(OutputEvent::Warning(
                "no speech text was recognized".to_owned(),
            ));
            let _ = sink.emit(OutputEvent::Phase(SessionPhase::NoSpeech));
        }
    }
    let _ = sink.emit(OutputEvent::Completed);
    0
}

/// Await a step while prioritizing an explicit cancel. Repeated stops and
/// a closed command channel leave the step running.
async fn timed_step<F, T>(
    control: &mut ControlInput,
    deadline: Option<tokio::time::Instant>,
    future: F,
) -> Step<T>
where
    F: Future<Output = T>,
{
    let wait = async {
        match deadline {
            Some(deadline) => match timeout_at(deadline, future).await {
                Ok(value) => Step::Value(value),
                Err(_) => Step::TimedOut,
            },
            None => Step::Value(future.await),
        }
    };
    tokio::pin!(wait);
    loop {
        tokio::select! {
            biased;
            command = control.receiver.recv(), if !control.closed => match command {
                Some(ControlCommand::Cancel) => return Step::Cancelled,
                Some(ControlCommand::Stop) => {},
                None => control.closed = true,
            },
            step = &mut wait => return step,
        }
    }
}

/// Common cancel exit: abort capture (which removes any partial WAV),
/// cancel the backend, emit `Cancelled` + `Completed`, exit code 130.
/// Never injects and never records history.
async fn cancel_path<C, T, S>(capture: &mut C, transport: &mut T, sink: &mut S) -> i32
where
    C: AudioCapture,
    T: BackendTransport,
    S: EventSink,
{
    capture.abort().await;
    let _ = transport.cancel().await;
    cancelled(sink)
}

pub(crate) fn cancelled(sink: &mut impl EventSink) -> i32 {
    let _ = sink.emit(OutputEvent::Cancelled);
    let _ = sink.emit(OutputEvent::Completed);
    130
}

/// Common failure exit: emit `Error(message)` + `Completed`, exit code 1.
/// Touches neither capture nor transport; callers abort/cancel first when
/// those must not linger until drop.
pub(crate) fn fail<S>(sink: &mut S, message: &str) -> i32
where
    S: EventSink,
{
    let _ = sink.emit(OutputEvent::Error(message.to_owned()));
    let _ = sink.emit(OutputEvent::Completed);
    1
}

/// Feed one backend event through the state machine and forward the
/// resulting snapshot. A final snapshot with empty text is suppressed (the
/// caller emits the "no speech" warning instead). A backend error is
/// returned to the caller, which releases the capture before reporting it.
fn dispatch_event<S>(
    session: &mut RecognitionSession,
    sink: &mut S,
    event: RealtimeEvent,
) -> Result<(), String>
where
    S: EventSink,
{
    if let RealtimeEvent::DevicesLoaded(devices) = event {
        let _ = sink.emit(OutputEvent::DevicesLoaded(devices));
        return Ok(());
    }
    let update = session.apply(event);
    match update {
        SessionUpdate::Transcript(snapshot) | SessionUpdate::Final(snapshot) => {
            if !(snapshot.is_final && snapshot.authoritative_text.is_empty()) {
                let _ = sink.emit(OutputEvent::Transcript(snapshot));
            }
        }
        SessionUpdate::Error(message) => return Err(message),
        SessionUpdate::Ignored => {}
    }
    Ok(())
}

/// Audio file stem: unix milliseconds plus a random suffix so concurrent
/// sessions never collide and names sort chronologically.
fn new_audio_stem() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let mut bytes = [0_u8; 4];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{millis}-{suffix}")
}
