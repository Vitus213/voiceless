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

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::{timeout, timeout_at};

use crate::realtime::RealtimeEvent;
use crate::session::{
    RecognitionSession, SessionAction, SessionState, SessionUpdate, TranscriptSnapshot,
};

pub const DEFAULT_QUEUE_CAPACITY: usize = 16;
pub const DEFAULT_SEND_DEADLINE: Duration = Duration::from_millis(500);
pub const DEFAULT_FINISH_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    Stop,
    Cancel,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InjectionResult {
    pub ok: bool,
    pub method: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum OutputEvent {
    Ready,
    /// Mic level (RMS, 0..1) while recording, at most ~15 Hz; only when
    /// `SessionPlan::emit_levels` (live UIs) is set.
    Level(f32),
    Transcript(TranscriptSnapshot),
    Finalized {
        injection: Option<InjectionResult>,
    },
    Warning(String),
    Error(String),
    Cancelled,
    Completed,
}

#[derive(Debug, Clone)]
pub struct SessionPlan {
    pub backend: String,
    pub inject: bool,
    pub mode_id: String,
    pub queue_capacity: usize,
    pub send_deadline: Duration,
    pub finish_timeout: Duration,
    pub ready_timeout: Duration,
    /// Directory where each successful session's WAV recording is saved;
    /// `None` disables audio retention.
    pub save_audio_dir: Option<PathBuf>,
    pub emit_levels: bool,
}

/// Minimum spacing of `OutputEvent::Level`.
const LEVEL_INTERVAL: Duration = Duration::from_millis(66);

impl SessionPlan {
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
            emit_levels: false,
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
pub trait AudioCapture {
    async fn start(&mut self) -> io::Result<()>;
    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>>;
    async fn stop_capture(&mut self) -> io::Result<Option<Vec<u8>>>;
    fn abort(&mut self) {}
    /// Finalize a saved recording after a successful session; `None` when
    /// nothing was recorded or saving is disabled. Never fails the session.
    fn finish_recording(&mut self) -> Option<PathBuf> {
        None
    }
}

/// Realtime backend boundary. Events follow the same semantics for cloud and
/// local backends (`RealtimeEvent`). `send_audio` and `next_event` share the
/// session concurrently; `finish` and `cancel` terminate it.
pub trait BackendTransport {
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()>;
    async fn next_event(&self) -> io::Result<RealtimeEvent>;
    async fn finish(&mut self) -> io::Result<()>;
    async fn cancel(&mut self) -> io::Result<()>;
}

pub trait TextInjector {
    async fn inject(&mut self, text: &str) -> InjectionResult;
}

/// Final-text processing boundary. `quick` mode never calls this; other
/// modes send the authoritative transcript through it.
pub trait TextProcessor {
    async fn process(&mut self, mode_id: &str, text: &str) -> Result<String, String>;

    /// Rule-based cleanup of every final transcript (fillers, hotwords,
    /// replacements), in every mode and before `process`.
    fn cleanup(&mut self, text: &str) -> String {
        text.to_owned()
    }
}

/// History boundary. Only successful sessions with authoritative text are
/// recorded; cancelled, failed and empty sessions never reach here.
pub trait HistoryRecorder {
    fn record(&mut self, entry: HistoryEntry);
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub raw_text: String,
    pub processed_text: Option<String>,
    pub final_text: String,
    pub processing_mode: String,
    pub status: String,
    pub backend: String,
    pub duration_seconds: Option<f64>,
    pub audio_path: Option<String>,
}

pub trait EventSink {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()>;
}

/// Control input wrapper: a closed channel stops delivering commands but is
/// never treated as a cancellation itself.
struct ControlInput {
    receiver: mpsc::Receiver<ControlCommand>,
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

enum Step<T> {
    Value(T),
    TimedOut,
    Command(ControlCommand),
}

/// Run one recognition session to completion and return the process exit
/// code: `0` success, `1` failure, `130` cancellation.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<C, T, P, J, H, S>(
    plan: SessionPlan,
    mut capture: C,
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

    // Ready gate: capture never starts before the backend is ready, so
    // auth/connect failures exit before any audio is produced.
    let ready_deadline = tokio::time::Instant::now() + plan.ready_timeout;
    loop {
        tokio::select! {
            biased;
            command = control.receiver.recv(), if !control.closed => match command {
                Some(_) | None => {
                    if command.is_none() {
                        control.closed = true;
                    }
                    return cancel_path(&mut capture, &mut transport, sink).await;
                }
            },
            result = timeout_at(ready_deadline, transport.next_event()) => match result {
                Ok(Ok(RealtimeEvent::Ready)) => break,
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    let _ = transport.cancel().await;
                    return fail(sink, &format!("backend ready: {error}"));
                }
                Err(_) => {
                    let _ = transport.cancel().await;
                    return fail(sink, "backend ready timed out");
                }
            },
        }
    }
    let audio_destination = plan.save_audio_dir.as_ref().and_then(|dir| {
        std::fs::create_dir_all(dir)
            .ok()
            .map(|_| dir.join(format!("{}.wav", new_audio_stem())))
    });
    let mut capture =
        crate::capture::WavRecorder::new(capture, crate::capture::SAMPLE_RATE, audio_destination);
    if let Err(error) = capture.start().await {
        let _ = transport.cancel().await;
        return fail(sink, &format!("capture: {error}"));
    }
    if sink.emit(OutputEvent::Ready).is_err() {
        return 1;
    }

    let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
    let mut audio_bytes: u64 = 0;
    let mut last_level = tokio::time::Instant::now();

    loop {
        tokio::select! {
            biased;
            command = control.receiver.recv(), if !control.closed => {
                let action = match command {
                    Some(ControlCommand::Stop) => session.request_stop(),
                    Some(ControlCommand::Cancel) => SessionAction::Cancel,
                    None => {
                        control.closed = true;
                        continue;
                    }
                };
                match action {
                    SessionAction::Finish => break,
                    SessionAction::Cancel => {
                        return cancel_path(&mut capture, &mut transport, sink).await
                    }
                    SessionAction::Ignore => {}
                }
            }
            chunk = capture.next_chunk() => match chunk {
                Ok(Some(pcm)) => {
                    if queue.len() >= plan.queue_capacity {
                        return overrun(&mut capture, &mut transport, sink).await;
                    }
                    audio_bytes += pcm.len() as u64;
                    if plan.emit_levels && last_level.elapsed() >= LEVEL_INTERVAL {
                        last_level = tokio::time::Instant::now();
                        let _ = sink.emit(OutputEvent::Level(pcm_rms(&pcm)));
                    }
                    queue.push_back(pcm);
                }
                Ok(None) => {
                    if session.state() == SessionState::Recording {
                        let _ = session.request_stop();
                    }
                    break;
                }
                Err(error) => return fail(sink, &format!("capture: {error}")),
            },
            send = timeout(plan.send_deadline, transport.send_audio(queue.front().map(Vec::as_slice).unwrap_or(&[]))), if !queue.is_empty() => {
                match send {
                    Ok(Ok(())) => {
                        queue.pop_front();
                    }
                    Ok(Err(error)) => {
                        return send_failed(&mut capture, &mut transport, sink, &format!("audio send: {error}")).await
                    }
                    Err(_) => {
                        return send_failed(&mut capture, &mut transport, sink, "audio send exceeded the deadline").await
                    }
                }
            }
            event = transport.next_event() => match event {
                Ok(event) => {
                    if let Err(code) = dispatch_event(&mut session, sink, event) {
                        let _ = sink.emit(OutputEvent::Completed);
                        return code;
                    }
                    if session.state() == SessionState::Failed {
                        let _ = sink.emit(OutputEvent::Completed);
                        return 1;
                    }
                }
                Err(error) => return fail(sink, &format!("realtime receive: {error}")),
            },
        }
    }

    // Stop flush: graceful capture stop, then FIFO drain of the queue and
    // the tail frame, then exactly one finish, then final events.
    let stop_deadline = tokio::time::Instant::now() + plan.finish_timeout;
    let tail = match timed_step(&mut control, stop_deadline, capture.stop_capture()).await {
        Step::Value(Ok(tail)) => tail,
        Step::Value(Err(error)) => return fail(sink, &format!("capture stop: {error}")),
        Step::TimedOut => return fail(sink, "capture stop timed out"),
        Step::Command(command) => {
            return handle_flush_command(command, &mut session, &mut capture, &mut transport, sink)
                .await
        }
    };
    if let Some(tail) = tail {
        if tail.len() % 2 != 0 {
            capture.abort();
            let _ = transport.cancel().await;
            return fail(
                sink,
                &format!("incomplete PCM16 tail frame ({} bytes)", tail.len()),
            );
        }
        if !tail.is_empty() {
            audio_bytes += tail.len() as u64;
            queue.push_back(tail);
        }
    }
    while let Some(pcm) = queue.pop_front() {
        match timed_step(&mut control, stop_deadline, transport.send_audio(&pcm)).await {
            Step::Value(Ok(())) => {}
            Step::Value(Err(error)) => return fail(sink, &format!("flush send: {error}")),
            Step::TimedOut => return fail(sink, "flush send timed out"),
            Step::Command(command) => {
                return handle_flush_command(
                    command,
                    &mut session,
                    &mut capture,
                    &mut transport,
                    sink,
                )
                .await
            }
        }
    }
    match timed_step(&mut control, stop_deadline, transport.finish()).await {
        Step::Value(Ok(())) => {}
        Step::Value(Err(error)) => return fail(sink, &format!("backend finish: {error}")),
        Step::TimedOut => return fail(sink, "backend finish timed out"),
        Step::Command(command) => {
            return handle_flush_command(command, &mut session, &mut capture, &mut transport, sink)
                .await
        }
    }

    loop {
        let step = timed_step(&mut control, stop_deadline, transport.next_event()).await;
        let event = match step {
            Step::Value(Ok(event)) => event,
            Step::Value(Err(error)) => return fail(sink, &format!("final receive: {error}")),
            Step::TimedOut => return fail(sink, "final event timed out"),
            Step::Command(command) => {
                return handle_flush_command(
                    command,
                    &mut session,
                    &mut capture,
                    &mut transport,
                    sink,
                )
                .await
            }
        };
        if let Err(code) = dispatch_event(&mut session, sink, event) {
            let _ = sink.emit(OutputEvent::Completed);
            return code;
        }
        match session.state() {
            SessionState::Completed => break,
            SessionState::Failed | SessionState::Cancelled => {
                let _ = sink.emit(OutputEvent::Completed);
                return 1;
            }
            _ => {}
        }
    }

    match session.take_injection_text() {
        Some(raw_text) => {
            let cleaned = processor.cleanup(&raw_text);
            let (final_text, processed_text) = if plan.mode_id == "quick" {
                (
                    cleaned.clone(),
                    (cleaned != raw_text).then(|| cleaned.clone()),
                )
            } else {
                match processor.process(&plan.mode_id, &cleaned).await {
                    Ok(processed) => (processed.clone(), Some(processed)),
                    Err(error) => {
                        let _ = sink.emit(OutputEvent::Warning(format!(
                            "processing failed, keeping recognized text: {error}"
                        )));
                        (cleaned.clone(), None)
                    }
                }
            };
            let injection = if plan.inject {
                Some(injector.inject(&final_text).await)
            } else {
                None
            };
            history.record(HistoryEntry {
                raw_text,
                processed_text,
                final_text,
                processing_mode: plan.mode_id.clone(),
                status: "completed".to_owned(),
                backend: plan.backend.clone(),
                duration_seconds: (audio_bytes >= 2).then(|| audio_bytes as f64 / 32_000.0),
                audio_path: capture
                    .finish_recording()
                    .map(|path| path.display().to_string()),
            });
            let _ = sink.emit(OutputEvent::Finalized { injection });
        }
        None => {
            let _ = sink.emit(OutputEvent::Warning(
                "no speech text was recognized".to_owned(),
            ));
            let _ = sink.emit(OutputEvent::Finalized { injection: None });
        }
    }
    let _ = sink.emit(OutputEvent::Completed);
    0
}

/// Any control command arriving during the stop flush escalates to cancel:
/// a second stop request or an explicit cancel both abort the flush.
async fn handle_flush_command<C, T, S>(
    command: ControlCommand,
    session: &mut RecognitionSession,
    capture: &mut C,
    transport: &mut T,
    sink: &mut S,
) -> i32
where
    C: AudioCapture,
    T: BackendTransport,
    S: EventSink,
{
    let _ = command;
    let _ = session.request_stop();
    cancel_path(capture, transport, sink).await
}

async fn overrun<C, T, S>(capture: &mut C, transport: &mut T, sink: &mut S) -> i32
where
    C: AudioCapture,
    T: BackendTransport,
    S: EventSink,
{
    capture.abort();
    let _ = transport.cancel().await;
    fail(sink, "audio backlog exceeded the bounded queue capacity")
}

async fn send_failed<C, T, S>(
    capture: &mut C,
    transport: &mut T,
    sink: &mut S,
    message: &str,
) -> i32
where
    C: AudioCapture,
    T: BackendTransport,
    S: EventSink,
{
    capture.abort();
    let _ = transport.cancel().await;
    fail(sink, message)
}

async fn timed_step<F, T>(
    control: &mut ControlInput,
    deadline: tokio::time::Instant,
    future: F,
) -> Step<io::Result<T>>
where
    F: Future<Output = io::Result<T>>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            command = control.receiver.recv(), if !control.closed => match command {
                Some(command) => return Step::Command(command),
                None => control.closed = true,
            },
            result = timeout_at(deadline, &mut future) => {
                return match result {
                    Ok(value) => Step::Value(value),
                    Err(_) => Step::TimedOut,
                };
            }
        }
    }
}

async fn cancel_path<C, T, S>(capture: &mut C, transport: &mut T, sink: &mut S) -> i32
where
    C: AudioCapture,
    T: BackendTransport,
    S: EventSink,
{
    capture.abort();
    let _ = transport.cancel().await;
    let _ = sink.emit(OutputEvent::Cancelled);
    let _ = sink.emit(OutputEvent::Completed);
    130
}

fn fail<S>(sink: &mut S, message: &str) -> i32
where
    S: EventSink,
{
    let _ = sink.emit(OutputEvent::Error(message.to_owned()));
    let _ = sink.emit(OutputEvent::Completed);
    1
}

fn dispatch_event<S>(
    session: &mut RecognitionSession,
    sink: &mut S,
    event: RealtimeEvent,
) -> Result<(), i32>
where
    S: EventSink,
{
    let update = session.apply(event);
    match update {
        SessionUpdate::Transcript(snapshot) | SessionUpdate::Final(snapshot) => {
            if !(snapshot.is_final && snapshot.authoritative_text.is_empty()) {
                let _ = sink.emit(OutputEvent::Transcript(snapshot));
            }
        }
        SessionUpdate::Error(message) => {
            let _ = sink.emit(OutputEvent::Error(message));
            return Err(1);
        }
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
