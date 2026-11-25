//! Resident session daemon (`voiceless daemon`): one session at a time,
//! hotkey-style toggle semantics (idle -> start, recording -> normal stop,
//! stopping -> reject), explicit start/stop/cancel, and a warm engine shared
//! by every session so a hotkey never waits for a model load. Session events
//! are broadcast as `Event` signals (JSON lines, the `stream --json` schema)
//! for overlays and `voiceless ctl watch`.
//!
//! Activation sources: XDG Desktop Portal GlobalShortcuts (best effort,
//! requires a compositor portal backend), the `dev.voiceless.Daemon` D-Bus
//! control bus (`voiceless ctl`, the web console, compositor bindings), and
//! the evdev push-to-talk key from `[hotkey] key` (`hotkey` module).
//!
//! # Structure
//!
//! - `Gateway<R: SessionRunner>` is the session state machine. Production
//!   uses `StreamRunner` with the applied settings and warm engine. Tests
//!   inject scripted runners.
//! - `serve` builds `DaemonShared` (the contract with `console`), loads the
//!   engine, starts the console, then `serve_with_gateway` exports the D-Bus
//!   object and runs the single command loop.
//! - Every activation source only sends a `DaemonCommand` into one bounded
//!   mpsc channel (capacity 8); the loop in `serve_with_gateway` is the only
//!   code that touches the gateway, so commands are serialized and no two
//!   sources can race a session start.
//! - `HotkeyMachine` turns raw key presses/releases into gateway actions
//!   (hold past `[hotkey] hold_threshold_ms` stops on release, a tap toggles).
//!
//! # D-Bus contract
//!
//! Bus name `dev.voiceless.Daemon`, object `/dev/voiceless/Daemon`, interface
//! `dev.voiceless.Daemon.Controller`: `Activate()`, `Toggle(s mode)`,
//! `Start(s mode)`, `StartSession(s mode, b inject)`, `Stop()`, `Cancel()`
//! each wait for the command loop to act and reply `"started"`,
//! `"stopping"` or `"cancelled"`, or fail with
//! `org.freedesktop.DBus.Error.Failed` carrying the gateway's reason when
//! the command is rejected (`Start` while recording, `Toggle` while
//! stopping, `Stop` while idle);
//! `Status()` returns `idle`, `recording` or `stopping`; the `Event(s json)`
//! signal carries every broadcast line. An empty mode means the daemon's
//! `--mode` default.
//!
//! # Configuration
//!
//! `[asr] streaming_backend` / `fallback_backend` (which engine to warm),
//! `[hotkey] key`, `mode`, `ctrl_mode`, `hold_threshold_ms`, `[audio]
//! pause_media`, `cues`, and `[console] enabled`, `host`, `port`. The
//! console can queue a saved revision for validation and activation when
//! idle. Sessions use a snapshot of the last applied configuration.
//!
//! # Failure behaviour
//!
//! No session bus, an unexportable object or an unclaimable bus name make
//! `serve` return an error (the daemon cannot run without its control bus).
//! Everything else degrades: a warm-engine load failure is logged and
//! sessions load on demand; an unreadable `/dev/input` is reported in
//! `HotkeyStatus`; a console bind failure and a missing portal backend are
//! logged. A failed session task is reaped with exit code 1.

use base64::Engine as _;
#[cfg(target_os = "linux")]
use futures_util::StreamExt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

#[cfg(target_os = "linux")]
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use tokio::sync::mpsc;

use crate::coordinator::{
    BackendSelection, ControlCommand, EventSink, InjectionResult, OutputEvent, SessionPhase,
};

mod apply;
mod jobs;
mod shortcuts;

use crate::console::settings::{lock_revision, needs_engine_reload, ConfigFile};
pub use apply::ConfigApplyStatus;

tokio::task_local! {
    /// Passed to the offline worker when this session opens its transport.
    pub(crate) static OFFLINE_HANDOFF: (Arc<crate::transcription_jobs::PriorityHold>, std::time::Instant);
}

/// What a successful `Gateway::toggle`, `start` or `stop` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateOutcome {
    /// A new session task was spawned; the gateway is now `Recording`.
    Started,
    /// A normal stop was requested; the gateway is now `Stopping` until the
    /// task finishes and `Gateway::poll` reaps it.
    Stopping,
    /// The previous completed result was delivered again by explicit request.
    Repasted,
    /// A short-lived practice request is armed for the next activation.
    PracticeArmed,
}

pub(crate) const PRACTICE_ARM_MS: u64 = 20_000;

/// The workflow that owns a recording and decides where its audio and text may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionPurpose {
    Dictation,
    Practice,
    Narration,
}

/// Session factory boundary. Production wraps the stream coordinator;
/// tests inject scripted fixtures. `inject` overrides the daemon's
/// injection setting for this session only.
pub trait SessionRunner {
    /// Build one session as a future that the gateway spawns as a task.
    /// A blank `mode` lets the runtime apply a rule or the daemon default.
    /// `control` delivers the gateway's
    /// `Stop`/`Cancel`, and the future resolves to the session exit code
    /// (`0`, `1` or `130`, the `stream` contract) or a setup error message.
    /// Must return quickly: the gateway is `Recording` as soon as it returns.
    fn start(
        &mut self,
        session_id: u64,
        mode: &str,
        inject: Option<bool>,
        purpose: SessionPurpose,
        context: crate::stream::DictationContext,
        control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>>;
}

/// Gateway state as published to D-Bus `Status`, the `state` event and the
/// console. There is no separate "finished" state: a session that ends on
/// its own goes straight from `Recording` to `Idle` once reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No session task; start commands are accepted.
    Idle,
    /// A session task runs and has not been asked to stop.
    Recording,
    /// Stop or cancel was sent; the task is flushing and nothing but
    /// `Cancel` is accepted until it exits.
    Stopping,
}

impl State {
    /// Lower-case name used on the wire (`idle`, `recording`, `stopping`).
    pub fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Recording => "recording",
            State::Stopping => "stopping",
        }
    }
}

/// Single-session state machine. Not synchronized: exactly one owner (the
/// daemon's command loop, or a test) calls into it, which is what makes
/// "one session at a time" a local invariant rather than a locking problem.
/// The state only returns to `Idle` through `poll`.
pub struct Gateway<R: SessionRunner> {
    runner: R,
    state: State,
    last_session_id: u64,
    shared: Option<std::sync::Arc<DaemonShared>>,
    practice_until: Option<std::time::Instant>,
    /// Control channel of the running session (`None` while idle).
    control_tx: Option<mpsc::Sender<ControlCommand>>,
    /// The spawned session task; `Some` from start until `poll` reaps it.
    task: Option<tokio::task::JoinHandle<Result<i32, String>>>,
}

impl<R: SessionRunner> Gateway<R> {
    /// An idle gateway around `runner`.
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            state: State::Idle,
            last_session_id: 0,
            shared: None,
            practice_until: None,
            control_tx: None,
            task: None,
        }
    }

    /// Hotkey activation in the `quick` mode; see `Gateway::toggle`. (The
    /// daemon resolves its own `--mode` default before calling `toggle`, so
    /// this shortcut is mainly for tests.)
    pub fn activate(&mut self) -> Result<ActivateOutcome, String> {
        self.toggle("quick")
    }

    /// Toggle: idle starts a session in `mode`, an active session stops
    /// normally, and a session already stopping rejects further activation
    /// so no concurrent session or duplicate injection can occur.
    pub fn toggle(&mut self, mode: &str) -> Result<ActivateOutcome, String> {
        match self.state {
            State::Idle => self.start(mode),
            State::Recording => self.stop(),
            State::Stopping => Err("session is already stopping".to_owned()),
        }
    }

    /// Start a session in `mode`; only valid when idle (hold-to-talk press).
    /// Any other state is an error and leaves the running session alone, so
    /// a repeated press can never start a second session.
    pub fn start(&mut self, mode: &str) -> Result<ActivateOutcome, String> {
        self.start_with(mode, None)
    }

    /// `Gateway::start` with a per-session injection override (`None` keeps
    /// the daemon's `--no-inject` setting). Spawns the runner's future on the
    /// current Tokio runtime and moves to `Recording` immediately, before the
    /// session has connected or started capturing.
    pub fn start_with(
        &mut self,
        mode: &str,
        inject: Option<bool>,
    ) -> Result<ActivateOutcome, String> {
        self.start_session(mode, inject, SessionPurpose::Dictation, Default::default())
    }

    fn start_session(
        &mut self,
        mode: &str,
        inject: Option<bool>,
        purpose: SessionPurpose,
        context: crate::stream::DictationContext,
    ) -> Result<ActivateOutcome, String> {
        if self.state != State::Idle {
            return Err(format!("cannot start: session is {}", self.state.name()));
        }
        let armed = self
            .practice_until
            .take()
            .is_some_and(|until| until > std::time::Instant::now());
        let purpose = if armed && purpose == SessionPurpose::Dictation {
            SessionPurpose::Practice
        } else {
            purpose
        };
        let (mode, inject) = if purpose != SessionPurpose::Dictation {
            (crate::modes::RAW_MODE_ID, Some(false))
        } else {
            (mode, inject)
        };
        let session_id = self.last_session_id + 1;
        let (tx, rx) = mpsc::channel(8);
        let future = self
            .runner
            .start(session_id, mode, inject, purpose, context, rx);
        self.last_session_id = session_id;
        self.control_tx = Some(tx);
        self.state = State::Recording;
        if let Some(shared) = &self.shared {
            shared.publish_state(self.state, session_id, None, Some((mode, purpose)));
        }
        self.task = Some(tokio::spawn(future));
        Ok(ActivateOutcome::Started)
    }

    /// Normal stop of a recording session (hold-to-talk release): sends
    /// `Stop` and moves to `Stopping`. Rejected while idle or already
    /// stopping, so the coordinator receives at most one stop and injects at
    /// most once. The send is non-blocking; a full or closed control channel
    /// is ignored because the task is reaped by `poll` either way.
    pub fn stop(&mut self) -> Result<ActivateOutcome, String> {
        if self.state != State::Recording {
            return Err(format!("cannot stop: session is {}", self.state.name()));
        }
        let tx = self
            .control_tx
            .clone()
            .ok_or_else(|| "session without a control channel".to_owned())?;
        let _ = tx.try_send(ControlCommand::Stop);
        self.state = State::Stopping;
        Ok(ActivateOutcome::Stopping)
    }

    /// Current state (what `poll` last established plus any pending
    /// start/stop/cancel transition).
    pub fn state(&self) -> State {
        self.state
    }

    fn session_id(&self) -> Option<u64> {
        (self.state != State::Idle).then_some(self.last_session_id)
    }

    /// Force cancel: valid from any active state; idle is a no-op. Also
    /// valid while `Stopping`, which is how a stop flush that hangs (no final
    /// event) gets unblocked; the coordinator then exits with `130` and types
    /// nothing. Awaits the channel send (capacity 8, so it only waits if the
    /// session is not draining commands).
    pub async fn cancel(&mut self) {
        if matches!(self.state, State::Idle) {
            return;
        }
        if let Some(tx) = &self.control_tx {
            let _ = tx.send(ControlCommand::Cancel).await;
        }
        self.state = State::Stopping;
    }

    /// Reap a finished session, returning its exit code. Returns `None`
    /// while a session is still running (or none ran), and restores the
    /// idle state once the task completes. A runner error is printed and
    /// counted as `1`, as is a panicked or aborted task. Non-blocking in
    /// practice: it only awaits a task that `is_finished` already reported.
    pub async fn poll(&mut self) -> Option<i32> {
        let task = self.task.as_ref()?;
        if !task.is_finished() {
            return None;
        }
        let finished = self.task.take().expect("task present");
        let code = match finished.await {
            Ok(Ok(code)) => code,
            Ok(Err(message)) => {
                eprintln!("Voiceless: session failed: {message}");
                if let Some(shared) = &self.shared {
                    shared.update(|status| {
                        if let Some(session) = &mut status.session {
                            session.error = Some(message);
                        }
                    });
                }
                1
            }
            Err(_) => 1,
        };
        self.reset();
        Some(code)
    }

    /// Whether a session task is currently running or finishing (i.e. not
    /// yet reaped by `poll`).
    pub fn is_active(&self) -> bool {
        self.task.is_some()
    }

    /// Back to idle with no control channel; only called by `poll`.
    fn reset(&mut self) {
        self.state = State::Idle;
        self.control_tx = None;
    }
}

/// Production runner: executes one streaming session per activation through
/// the shared coordinator, with the daemon's warm engine and event
/// broadcast, controlled exclusively via the gateway channel.
pub struct StreamRunner {
    /// The daemon's `stream` options (`--config`, `--no-inject`, `--mode`);
    /// `mode` and `inject` are overridden per session.
    pub options: crate::stream::StreamOptions,
    /// Source of the warm engine and the event broadcast for each session.
    pub shared: std::sync::Arc<DaemonShared>,
}

impl SessionRunner for StreamRunner {
    fn start(
        &mut self,
        session_id: u64,
        mode: &str,
        inject: Option<bool>,
        purpose: SessionPurpose,
        context: crate::stream::DictationContext,
        mut control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>> {
        let mut options = self.options.clone();
        let default_mode = std::mem::replace(&mut options.mode, mode.to_owned());
        options.context = context;
        if let Some(inject) = inject {
            options.inject = inject;
        }
        let active = self.shared.applied_config();
        let local = |backend: &str| matches!(backend, "local-offline" | "local-streaming");
        let primary = options
            .backend
            .as_deref()
            .unwrap_or(&active.file.config.asr.streaming_backend);
        let fallback = active.file.config.asr.fallback_backend.as_deref();
        let may_stream = primary == "local-streaming" || fallback == Some("local-streaming");
        let needs_decoder = if purpose == SessionPurpose::Narration {
            // Consent to screen narration covers local recognition only.
            let backend = [Some(primary), fallback]
                .into_iter()
                .flatten()
                .find(|id| local(id));
            options.backend = Some(backend.unwrap_or("local-offline").to_owned());
            true
        } else {
            local(primary) || fallback.is_some_and(local)
        };
        let offline_only = needs_decoder && !may_stream;
        let hold = needs_decoder.then(|| Arc::new(self.shared.transcription_resource.hold()));
        let mut sink = DaemonEventSink {
            shared: self.shared.clone(),
            session_id,
            sequence: 0,
        };
        Box::pin(async move {
            if let Some(hold) = hold.as_ref().filter(|_| !offline_only) {
                let waiting = hold.clone();
                let background = tokio::task::spawn_blocking(move || {
                    waiting.wait_for_background(crate::coordinator::DEFAULT_READY_TIMEOUT)
                });
                let available = tokio::select! {
                    biased;
                    _ = control.recv() => return Ok(crate::coordinator::cancelled(&mut sink)),
                    result = background => result.map_err(|error| error.to_string())?,
                };
                if !available {
                    return Ok(crate::coordinator::fail(
                        &mut sink,
                        "a background transcription did not release the local decoder in time",
                    ));
                }
            }
            let deadline = std::time::Instant::now() + crate::coordinator::DEFAULT_READY_TIMEOUT;
            let env = crate::stream::SessionEnv {
                warm: active.engine,
                sink: Some(Box::new(sink)),
            };
            let mut config = std::borrow::Cow::Borrowed(&active.file.config);
            if purpose != SessionPurpose::Dictation {
                let session = config.to_mut();
                session.history.enabled = false;
                session.history.save_audio = false;
                session.history.save_failed_audio = false;
                if purpose == SessionPurpose::Narration {
                    session.asr.fallback_backend = None;
                }
            }
            let session =
                crate::stream::run_with_config(options, control, env, config, &default_mode);
            match hold.as_ref().filter(|_| offline_only) {
                Some(hold) => {
                    OFFLINE_HANDOFF
                        .scope((hold.clone(), deadline), session)
                        .await
                }
                None => session.await,
            }
            .map_err(|error| error.to_string())
        })
    }
}

/// Well-known session bus name the daemon claims; `ctl::stream_via_daemon`
/// checks its owner to decide whether a daemon is running.
#[cfg(target_os = "linux")]
pub const BUS_NAME: &str = "dev.voiceless.Daemon";
/// Object path of the controller.
#[cfg(target_os = "linux")]
pub const OBJECT_PATH: &str = "/dev/voiceless/Daemon";
/// Interface name of the controller (methods and the `Event` signal).
#[cfg(target_os = "linux")]
pub const INTERFACE: &str = "dev.voiceless.Daemon.Controller";

/// D-Bus control interface. Toggle/Activate map to the gateway hotkey
/// semantics, Start/Stop to hold-to-talk press/release, Cancel force-stops.
/// An empty mode selects the daemon's default mode.
///
/// Every mutating method sends a `DaemonCommand` with a reply channel and
/// waits for the command loop to act on it, so the reply reports the
/// outcome: `"started"`, `"stopping"` or `"cancelled"` on success, and
/// `org.freedesktop.DBus.Error.Failed` with the gateway's reason when the
/// command was rejected (`Start` while recording, `Toggle` while stopping,
/// `Stop` while idle). `voiceless ctl` turns that error into exit code 1.
/// A reload in progress delays replies by the model load; the wait is
/// bounded by the caller's method timeout.
#[cfg(target_os = "linux")]
#[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
impl ControllerInterface {
    /// `Toggle` in the daemon's default mode (the portal shortcut and
    /// mode-less bindings).
    async fn activate(&self) -> zbus::fdo::Result<String> {
        self.run(|reply| DaemonCommand::Toggle {
            mode: String::new(),
            reply: Some(reply),
        })
        .await
    }

    /// Start a session in `mode` when idle, stop it when recording; an
    /// error while stopping.
    async fn toggle(&self, mode: &str) -> zbus::fdo::Result<String> {
        self.run(|reply| DaemonCommand::Toggle {
            mode: mode.to_owned(),
            reply: Some(reply),
        })
        .await
    }

    /// Hold-to-talk press: start a session in `mode`; an error when one is
    /// already active.
    async fn start(&self, mode: &str) -> zbus::fdo::Result<String> {
        self.run(|reply| DaemonCommand::Start {
            mode: mode.to_owned(),
            reply: Some(reply),
        })
        .await
    }

    /// Start with an explicit injection choice (`voiceless stream`
    /// delegating to the daemon, e.g. with `--no-inject`).
    async fn start_session(&self, mode: &str, inject: bool) -> zbus::fdo::Result<String> {
        self.run(|reply| DaemonCommand::StartSession {
            mode: mode.to_owned(),
            inject,
            context: Default::default(),
            reply: Some(reply),
        })
        .await
    }

    /// Start a session and return the identity carried by its events.
    async fn start_session_with_id(&self, mode: &str, inject: bool) -> zbus::fdo::Result<u64> {
        self.start_session_with_context(mode, inject, "", "").await
    }

    /// Start dictation with a hostname and project explicitly supplied by a local client.
    async fn start_session_with_context(
        &self,
        mode: &str,
        inject: bool,
        site: &str,
        project: &str,
    ) -> zbus::fdo::Result<u64> {
        let receipt = self
            .command(|reply| DaemonCommand::StartSession {
                mode: mode.to_owned(),
                inject,
                context: crate::stream::DictationContext {
                    site: (!site.trim().is_empty()).then(|| site.to_owned()),
                    project: (!project.trim().is_empty()).then(|| project.to_owned()),
                },
                reply: Some(reply),
            })
            .await?;
        receipt.session_id.ok_or_else(|| {
            zbus::fdo::Error::Failed("the start command produced no session".to_owned())
        })
    }

    async fn stop_session(&self, session_id: u64) -> zbus::fdo::Result<()> {
        self.command(|reply| DaemonCommand::ControlSession {
            session_id,
            action: ControlCommand::Stop,
            reply,
        })
        .await
        .map(|_| ())
    }

    async fn cancel_session(&self, session_id: u64) -> zbus::fdo::Result<()> {
        self.command(|reply| DaemonCommand::ControlSession {
            session_id,
            action: ControlCommand::Cancel,
            reply,
        })
        .await
        .map(|_| ())
    }

    /// Resolved data directory, preserving non-UTF-8 path bytes.
    fn data_directory(&self) -> Vec<u8> {
        self.shared.data_dir.as_os_str().as_encoded_bytes().to_vec()
    }

    /// Hold-to-talk release: normal stop, flush and type the final text;
    /// an error when nothing is recording.
    async fn stop(&self) -> zbus::fdo::Result<String> {
        self.run(|reply| DaemonCommand::Stop { reply: Some(reply) })
            .await
    }

    /// Abort the session without typing anything (exit code 130); a no-op
    /// when idle.
    async fn cancel(&self) -> zbus::fdo::Result<String> {
        self.run(|reply| DaemonCommand::Cancel { reply: Some(reply) })
            .await
    }

    /// Deliver the last completed result to the current input target.
    async fn repaste(&self) -> zbus::fdo::Result<String> {
        self.run(|reply| DaemonCommand::Repaste { reply: Some(reply) })
            .await
    }

    /// Last published session state, using the same snapshot as the console.
    fn status(&self) -> String {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .state
            .to_owned()
    }

    /// Suspend dictation (`true`) or resume it; replies `paused` or `resumed`.
    /// While paused the hotkey and every start request are refused; a
    /// session in progress is left alone.
    async fn set_paused(&self, paused: bool) -> zbus::fdo::Result<String> {
        self.command(|reply| DaemonCommand::SetPaused { paused, reply })
            .await?;
        Ok(if paused { "paused" } else { "resumed" }.to_owned())
    }

    /// Whether dictation is suspended.
    fn paused(&self) -> bool {
        self.shared.is_paused()
    }

    /// The loopback URL of the daemon's console (`http://127.0.0.1:PORT/`),
    /// or "" when it serves none: what a desktop window opens.
    fn console_url(&self) -> String {
        self.shared.console_url().unwrap_or_default()
    }

    /// One session event as a JSON line (`stream --json` schema), plus
    /// `{"type":"state","state":...,"mode":...}` on every state change,
    /// `{"type":"log","line":...}` for daemon log lines and
    /// `{"type":"session",...}` metrics after each finalized session.
    #[zbus(signal)]
    async fn event(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        json: &str,
    ) -> zbus::Result<()>;
}

#[cfg(target_os = "linux")]
impl ControllerInterface {
    /// Send one command and wait for the loop's verdict. A gone loop, or a
    /// loop that dropped the reply, is `org.freedesktop.DBus.Error.Failed`.
    async fn run(
        &self,
        build: impl FnOnce(CommandReply) -> DaemonCommand,
    ) -> zbus::fdo::Result<String> {
        self.command(build)
            .await
            .map(|receipt| outcome_name(receipt.outcome).to_owned())
    }

    async fn command(
        &self,
        build: impl FnOnce(CommandReply) -> DaemonCommand,
    ) -> zbus::fdo::Result<CommandReceipt> {
        let (reply, verdict) = tokio::sync::oneshot::channel();
        self.shared
            .commands
            .send(build(reply))
            .await
            .map_err(|_| zbus::fdo::Error::Failed("daemon command loop is gone".to_owned()))?;
        match verdict.await {
            Ok(Ok(receipt)) => Ok(receipt),
            Ok(Err(message)) => Err(zbus::fdo::Error::Failed(message)),
            Err(_) => Err(zbus::fdo::Error::Failed(
                "daemon dropped the command".to_owned(),
            )),
        }
    }
}

/// Accepted outcome and the identity of the session it operated on.
#[derive(Debug, Clone, Copy)]
pub struct CommandReceipt {
    pub outcome: Option<ActivateOutcome>,
    pub session_id: Option<u64>,
}

/// A command receipt or the reason the gateway rejected it.
pub type CommandVerdict = Result<CommandReceipt, String>;
/// Reply channel the control commands carry; `None` for sources that do
/// not wait (hotkey, portal).
pub type CommandReply = tokio::sync::oneshot::Sender<CommandVerdict>;

/// Wire name of an outcome in D-Bus method replies and console responses.
pub fn outcome_name(outcome: Option<ActivateOutcome>) -> &'static str {
    match outcome {
        Some(ActivateOutcome::Started) => "started",
        Some(ActivateOutcome::Stopping) => "stopping",
        Some(ActivateOutcome::Repasted) => "repasted",
        Some(ActivateOutcome::PracticeArmed) => "practice-armed",
        None => "cancelled",
    }
}

/// Everything the command loop can be asked to do, from any source (D-Bus
/// methods, the console API, the hotkey listener, the portal shortcut).
/// Mode strings may be empty, meaning the daemon's `--mode` default.
#[derive(Debug)]
pub enum DaemonCommand {
    /// `Gateway::toggle` in the given mode.
    Toggle {
        mode: String,
        reply: Option<CommandReply>,
    },
    /// `Gateway::start` in the given mode (hold-to-talk press).
    Start {
        mode: String,
        reply: Option<CommandReply>,
    },
    /// `Gateway::start_with` an explicit injection choice (`stream`
    /// delegation through `StartSession`).
    StartSession {
        mode: String,
        inject: bool,
        context: crate::stream::DictationContext,
        reply: Option<CommandReply>,
    },
    /// `Gateway::stop` (hold-to-talk release).
    Stop { reply: Option<CommandReply> },
    /// `Gateway::cancel`; never fails, idle is a no-op.
    Cancel { reply: Option<CommandReply> },
    /// Repeat delivery on explicit user request, with no new capture or history row.
    Repaste { reply: Option<CommandReply> },
    /// Start or arm an explicit practice session with automatic delivery disabled.
    Practice { arm: bool, reply: CommandReply },
    /// Local-only screen narration with an in-memory transcript.
    Narrate { reply: CommandReply },
    /// Clear a pending practice request without affecting any current recording.
    DisarmPractice { reply: CommandReply },
    /// Suspend dictation, or resume it: the hotkey and every start request
    /// are refused while paused.
    SetPaused { paused: bool, reply: CommandReply },
    /// Stop or cancel only the session that the client started.
    ControlSession {
        session_id: u64,
        action: ControlCommand,
        reply: CommandReply,
    },
    /// A key transition scoped to the listener that observed it.
    Key(u64, crate::hotkey::KeyEvent),
    /// Queue validation and activation of a saved revision.
    Reload {
        revision: Option<String>,
        reply: ConfigApplyReply,
    },
    /// Restore the file from the active revision and supersede queued work.
    Restore {
        revision: String,
        reply: ConfigApplyReply,
    },
}

pub(crate) type ConfigApplyReply = tokio::sync::oneshot::Sender<Result<ConfigApplyStatus, String>>;

/// What a push-to-talk key event asks of the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Start a session in this mode (empty means the daemon default).
    Start(String),
    /// Normal stop of the running session.
    Stop,
    /// Ignore the event (a tap release, or any key while stopping).
    Nothing,
}

/// One key, two gestures (as in Handy): hold it past the threshold and the
/// session stops on release; tap it and recording continues until the next
/// press. Any press while recording stops, whoever started the session.
///
/// The machine is driven with the gateway's state at the time of the event
/// and the event's timestamp, so it is pure and testable; the daemon
/// rebuilds it on every config reload.
pub struct HotkeyMachine {
    /// `[hotkey] hold_threshold_ms`: a press held at least this long is a
    /// hold-to-talk gesture, anything shorter is a tap.
    hold: std::time::Duration,
    /// Mode for a plain press (`[hotkey] mode`; empty means the daemon
    /// default).
    mode: String,
    /// Mode for a press with a Ctrl key held (`[hotkey] ctrl_mode`).
    ctrl_mode: String,
    /// When the press that started the current session happened; `None`
    /// once the gesture has been decided (tap or stop).
    pressed_at: Option<std::time::Instant>,
}

impl HotkeyMachine {
    /// A machine with no pending press.
    pub fn new(hold: std::time::Duration, mode: String, ctrl_mode: String) -> Self {
        Self {
            hold,
            mode,
            ctrl_mode,
            pressed_at: None,
        }
    }

    /// Key went down. Idle: start (Ctrl picks `ctrl_mode`) and remember the
    /// time so the matching release can tell a hold from a tap. Recording:
    /// stop, even for a session started elsewhere (`ctl`, console), and
    /// forget the press so the following release does nothing. Stopping:
    /// nothing.
    pub fn on_press(&mut self, state: State, ctrl: bool, now: std::time::Instant) -> KeyAction {
        match state {
            State::Idle => {
                self.pressed_at = Some(now);
                KeyAction::Start(if ctrl {
                    self.ctrl_mode.clone()
                } else {
                    self.mode.clone()
                })
            }
            State::Recording => {
                self.pressed_at = None;
                KeyAction::Stop
            }
            State::Stopping => KeyAction::Nothing,
        }
    }

    /// Key went up. Stops only when this release ends a press that started
    /// the session, the session is still recording, and the key was held at
    /// least `hold`; a shorter press is a tap that latches the recording. A
    /// release without a pending press (the session was started by someone
    /// else, or the press already stopped it) does nothing.
    pub fn on_release(&mut self, state: State, now: std::time::Instant) -> KeyAction {
        match self.pressed_at.take() {
            Some(at) if state == State::Recording && now.duration_since(at) >= self.hold => {
                KeyAction::Stop
            }
            // A tap: keep recording until the next press.
            _ => KeyAction::Nothing,
        }
    }
}

/// The D-Bus controller uses the daemon's command queue and snapshot.
#[cfg(target_os = "linux")]
struct ControllerInterface {
    shared: std::sync::Arc<DaemonShared>,
}

/// Engine state reported to the console (`/api/status` `engine`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EngineStatus {
    /// A warm engine is held; `false` for cloud backends (nothing to warm)
    /// and after a failed load.
    pub loaded: bool,
    /// `WarmEngine::describe`, e.g. `"<model> on <device>"`.
    pub description: Option<String>,
    /// Wall-clock seconds the last successful load took.
    pub load_seconds: Option<f64>,
    /// Why the load failed; sessions then load on demand.
    pub error: Option<String>,
    pub model: Option<String>,
    pub requested_device: Option<String>,
    pub devices: Option<crate::offline_asr::EngineDevices>,
}

impl EngineStatus {
    fn loaded(
        config: &crate::config::AppConfig,
        engine: Option<&crate::engine::WarmEngine>,
        elapsed: f64,
    ) -> Self {
        let backend = &config.asr.streaming_backend;
        Self {
            loaded: engine.is_some(),
            description: engine.map(crate::engine::WarmEngine::describe),
            load_seconds: engine.map(|_| elapsed),
            error: None,
            model: Some(crate::stream::backend_model(config, backend)),
            requested_device: crate::stream::requested_device(config, backend),
            devices: engine.map(crate::engine::WarmEngine::devices),
        }
    }
}

/// Push-to-talk listener state reported to the console (`/api/status`
/// `hotkey`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HotkeyStatus {
    /// Configured evdev key name; `None` when `[hotkey] key` is unset.
    pub key: Option<String>,
    /// Input devices being watched for the key; follows hotplug. 0 means no
    /// read access or no keyboard with the key plugged in.
    pub keyboards: usize,
    /// Unknown key name or no readable keyboard.
    pub error: Option<String>,
    bindings: Vec<HotkeyBindingStatus>,
    portal: PortalShortcutStatus,
}

#[derive(Debug, Clone, serde::Serialize)]
struct HotkeyBindingStatus {
    field: &'static str,
    chord: String,
    keyboards: usize,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
struct PortalShortcutStatus {
    trigger: Option<String>,
    error: Option<String>,
}

/// Timing of the last completed session, measured from daemon events.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMetrics {
    /// Backend that produced the session's final transcript.
    pub backend: String,
    /// Wall-clock seconds from the `recording` to the `stopping` state
    /// transition; includes the connect/ready time before capture starts.
    pub audio_seconds: f64,
    /// Milliseconds from the `stopping` transition to the `finalized` event.
    pub stop_to_final_ms: u64,
    /// Characters in the final transcript (`authoritative_text`).
    pub chars: usize,
    /// RFC 3339 UTC time of the `finalized` event.
    pub ended_at: String,
}

/// Latest session, retained through the return to idle for reconnecting clients.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionView {
    pub id: u64,
    pub practice: bool,
    pub purpose: SessionPurpose,
    pub phase: &'static str,
    pub transcript: Option<crate::session::TranscriptSnapshot>,
    pub injection: Option<InjectionResult>,
    pub error: Option<String>,
    pub backend_selection: Option<BackendSelection>,
    pub devices: Option<Box<crate::offline_asr::EngineDevices>>,
    pub output: Option<crate::output::Trace>,
    pub config_revision: String,
}

#[derive(serde::Serialize)]
struct RuntimeEvent<'a, T> {
    runtime_id: &'a str,
    event_sequence: u64,
    #[serde(flatten)]
    body: T,
}

/// Snapshot the console renders (`/api/status`, plus fields the console
/// adds itself such as uptime and GPU memory).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonStatus {
    /// Stable for this daemon instance. A restart resets the event sequence.
    pub runtime_id: String,
    pub event_sequence: u64,
    pub session: Option<SessionView>,
    /// Whether recognition is set up (see `recognition_configured`); the
    /// console opens on its first-run page until it is.
    pub configured: bool,
    /// Whether dictation is suspended (tray, console, `voiceless ctl pause`).
    pub paused: bool,
    /// `State::name` of the last published state.
    pub state: &'static str,
    /// Identity of the active session, shared by all command sources.
    pub session_id: Option<u64>,
    /// Mode of the current or most recent session (the default until one
    /// starts).
    pub mode: String,
    /// `[asr] streaming_backend` as of the last (re)load.
    pub backend: String,
    /// `[asr] fallback_backend` as of the last (re)load.
    pub fallback_backend: Option<String>,
    pub engine: EngineStatus,
    pub config_apply: ConfigApplyStatus,
    pub hotkey: HotkeyStatus,
    /// `None` until the first session finalizes.
    pub last_session: Option<SessionMetrics>,
}

/// Ring-buffer size of `DaemonShared::logs` (`/api/logs`).
const LOG_LINES: usize = 500;

/// The refusal every start gets while dictation is paused.
const PAUSED: &str =
    "dictation is paused; resume it from the tray, the console or `voiceless ctl resume`";

/// `Err(PAUSED)` while dictation is paused, else what `action` answers.
fn unless_paused(
    paused: bool,
    action: impl FnOnce() -> Result<ActivateOutcome, String>,
) -> Result<ActivateOutcome, String> {
    if paused {
        Err(PAUSED.to_owned())
    } else {
        action()
    }
}

#[derive(Clone)]
struct AppliedConfig {
    file: Arc<ConfigFile>,
    engine: Option<Arc<crate::engine::WarmEngine>>,
}

/// What the daemon shares with the web console: the command channel, the
/// event broadcast, the hot-swappable warm engine, status and recent logs.
/// All methods are synchronous, lock briefly and tolerate poisoned locks,
/// so they are safe from axum handlers and the command loop alike.
pub struct DaemonShared {
    /// Into the command loop; the console's session and reload endpoints
    /// send here.
    pub commands: mpsc::Sender<DaemonCommand>,
    /// Every event line (session events from the coordinator sink, `state`,
    /// `log`, `session`); `/api/events` and the D-Bus `Event` signal
    /// subscribe here. Capacity 256; slow subscribers lag rather than block.
    pub events: tokio::sync::broadcast::Sender<String>,
    /// The config file the console edits and `Reload` re-reads.
    pub config_path: std::path::PathBuf,
    /// Canonical data directory this runtime owns.
    pub(crate) data_dir: std::path::PathBuf,
    /// The daemon's `--mode`, used when a request names no mode.
    pub default_mode: String,
    /// Daemon start time (console uptime).
    pub started: std::time::Instant,
    pub(crate) jobs: std::sync::OnceLock<Arc<crate::transcription_jobs::JobQueue>>,
    pub(crate) meetings: std::sync::OnceLock<Arc<crate::meeting_capture::MeetingService>>,
    pub(crate) transcription_resource: crate::transcription_jobs::PriorityResource,
    applied: std::sync::RwLock<AppliedConfig>,
    hotkey_generation: AtomicU64,
    status: std::sync::Mutex<DaemonStatus>,
    last_final_text: parking_lot::Mutex<Option<Arc<str>>>,
    /// Last `LOG_LINES` log lines, oldest first.
    logs: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// Where the console listens, once the daemon serves one (`set_console_url`).
    console_url: std::sync::Mutex<Option<String>>,
}

/// Whether the user has set up recognition: a credential for a cloud
/// service, or a backend picked in place of the default. A file that only
/// holds console, audio or hotkey settings is no setup.
fn recognition_configured(config: &crate::config::AppConfig) -> bool {
    !config.cloud.api_key.is_empty()
        || !config.doubao.access_key.is_empty()
        || config.asr.streaming_backend != crate::config::AsrConfig::default().streaming_backend
}

impl DaemonShared {
    fn new(
        config_path: std::path::PathBuf,
        data_dir: std::path::PathBuf,
        default_mode: String,
        file: Arc<ConfigFile>,
    ) -> (std::sync::Arc<Self>, mpsc::Receiver<DaemonCommand>) {
        let (events, _) = tokio::sync::broadcast::channel(256);
        let (commands, command_rx) = mpsc::channel(8);
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).expect("generate runtime identifier");
        let config = &file.config;
        let configured = recognition_configured(config);
        let shared = std::sync::Arc::new(Self {
            commands,
            events,
            config_path,
            data_dir,
            started: std::time::Instant::now(),
            applied: std::sync::RwLock::new(AppliedConfig {
                file: file.clone(),
                engine: None,
            }),
            hotkey_generation: AtomicU64::new(0),
            last_final_text: parking_lot::Mutex::new(None),
            jobs: std::sync::OnceLock::new(),
            meetings: std::sync::OnceLock::new(),
            transcription_resource: crate::transcription_jobs::PriorityResource::default(),
            status: std::sync::Mutex::new(DaemonStatus {
                runtime_id: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random),
                event_sequence: 0,
                session: None,
                configured,
                paused: false,
                state: State::Idle.name(),
                session_id: None,
                mode: default_mode.clone(),
                backend: config.asr.streaming_backend.clone(),
                fallback_backend: config.asr.fallback_backend.clone(),
                engine: EngineStatus::default(),
                config_apply: ConfigApplyStatus {
                    active_revision: file.revision.clone(),
                    target_revision: None,
                    state: "idle",
                    error: None,
                },
                hotkey: HotkeyStatus::default(),
                last_session: None,
            }),
            default_mode,
            logs: std::sync::Mutex::new(std::collections::VecDeque::new()),
            console_url: std::sync::Mutex::new(None),
        });
        (shared, command_rx)
    }

    fn publish_state(
        &self,
        state: State,
        session_id: u64,
        exit_code: Option<i32>,
        start: Option<(&str, SessionPurpose)>,
    ) {
        let overlay = self.active_file().config.audio.overlay;
        self.update(|status| {
            status.state = state.name();
            status.session_id = (state != State::Idle).then_some(session_id);
            if let Some((mode, purpose)) = start {
                status.mode = if mode.is_empty() {
                    self.default_mode.clone()
                } else {
                    mode.to_owned()
                };
                status.session = Some(SessionView {
                    id: session_id,
                    practice: purpose == SessionPurpose::Practice,
                    purpose,
                    phase: "preparing",
                    transcript: None,
                    injection: None,
                    error: None,
                    backend_selection: None,
                    devices: None,
                    output: None,
                    config_revision: status.config_apply.active_revision.clone(),
                });
            } else if let Some(session) = &mut status.session {
                match exit_code {
                    Some(130) => session.phase = "cancelled",
                    Some(0) if session.phase != "no-speech" => session.phase = "completed",
                    Some(0) => {}
                    Some(_) => session.phase = "failed",
                    None if matches!(session.phase, "preparing" | "recording") => {
                        session.phase = "recognizing";
                    }
                    None => {}
                }
            }
            let body = serde_json::json!({
                "type": "state",
                "state": state.name(),
                "mode": status.mode,
                "session_id": session_id,
                "exit_code": exit_code,
                "config_revision": status.session.as_ref().map(|session| &session.config_revision),
                "practice": status.session.as_ref().is_some_and(|session| session.practice),
                "purpose": status.session.as_ref().map(|session| session.purpose),
                "overlay": overlay,
                "phase": status.session.as_ref().map(|session| session.phase),
                "error": status.session.as_ref().and_then(|session| session.error.as_deref()),
            });
            let _ = self.publish_locked(status, body);
        });
    }

    fn publish_locked(
        &self,
        status: &mut DaemonStatus,
        body: impl serde::Serialize,
    ) -> std::io::Result<()> {
        let event_sequence = status.event_sequence + 1;
        let line = serde_json::to_string(&RuntimeEvent {
            runtime_id: &status.runtime_id,
            event_sequence,
            body,
        })?;
        status.event_sequence = event_sequence;
        let _ = self.events.send(line);
        Ok(())
    }

    pub(crate) fn has_instance(&self, runtime_id: &str) -> bool {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .runtime_id
            == runtime_id
    }

    /// Print a log line, keep it for `/api/logs` and broadcast it.
    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        println!("{line}");
        let mut logs = self.logs.lock().unwrap_or_else(|error| error.into_inner());
        if logs.len() == LOG_LINES {
            logs.pop_front();
        }
        logs.push_back(line.clone());
        drop(logs);
        self.update(|status| {
            let _ = self.publish_locked(status, serde_json::json!({"type": "log", "line": line}));
        });
    }

    /// Whether dictation is suspended.
    pub fn is_paused(&self) -> bool {
        self.update(|status| status.paused)
    }

    /// Suspend or resume dictation and tell the console; true when the
    /// state changed. Nothing in progress is touched.
    pub fn set_paused(&self, paused: bool) -> bool {
        self.update(|status| {
            if status.paused == paused {
                return false;
            }
            status.paused = paused;
            let _ = self.publish_locked(
                status,
                serde_json::json!({ "type": "paused", "paused": paused }),
            );
            true
        })
    }

    /// The console's loopback URL, once it listens.
    pub fn console_url(&self) -> Option<String> {
        self.console_url
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Remember where the console was bound (`console::serve_daemon`).
    pub fn set_console_url(&self, url: String) {
        *self
            .console_url
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(url);
    }

    /// Copy of the retained log lines, oldest first.
    pub fn logs(&self) -> Vec<String> {
        let logs = self.logs.lock().unwrap_or_else(|error| error.into_inner());
        logs.iter().cloned().collect()
    }

    /// The current warm engine, if one is loaded (cloud backends and failed
    /// loads give `None`; sessions then load on demand).
    pub fn warm(&self) -> Option<std::sync::Arc<crate::engine::WarmEngine>> {
        self.applied
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .engine
            .clone()
    }

    fn applied_config(&self) -> AppliedConfig {
        self.applied
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn active_file(&self) -> Arc<ConfigFile> {
        self.applied
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .file
            .clone()
    }

    fn install_config(&self, candidate: apply::Prepared) {
        let configured = recognition_configured(&candidate.file.config);
        let mut applied = self
            .applied
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::mem::replace(
            &mut *applied,
            AppliedConfig {
                file: candidate.file,
                engine: candidate.engine,
            },
        );
        let config = &applied.file.config;
        self.update(|status| {
            status.configured = configured;
            status.engine = candidate.status;
            status.backend.clone_from(&config.asr.streaming_backend);
            status
                .fallback_backend
                .clone_from(&config.asr.fallback_backend);
            status
                .config_apply
                .active_revision
                .clone_from(&applied.file.revision);
            status.config_apply.target_revision = Some(applied.file.revision.clone());
            status.config_apply.state = "applied";
            status.config_apply.error = None;
            let event = serde_json::json!({
                "type": "engine",
                "configured": status.configured,
                "backend": status.backend,
                "fallback_backend": status.fallback_backend,
                "engine": status.engine,
                "hotkey": status.hotkey,
                "config_apply": status.config_apply,
            });
            let _ = self.publish_locked(status, event);
        });
        drop(applied);
        drop(previous);
    }

    fn refresh_session_settings(&self, active: &ConfigFile) -> Option<Arc<ConfigFile>> {
        let file = active.changed_on_disk(&self.config_path).ok()??;
        if needs_engine_reload(&active.config, &file.config) {
            return None;
        }
        let _writers = lock_revision(&self.config_path, &file.revision).ok()?;
        let file = Arc::new(file);
        self.install_config(apply::Prepared {
            file: file.clone(),
            engine: self.warm(),
            status: self.engine_status(),
        });
        Some(file)
    }

    fn publish_apply(
        &self,
        stage: &'static str,
        target: Option<String>,
        error: Option<String>,
    ) -> ConfigApplyStatus {
        self.update(|status| {
            status.config_apply.state = stage;
            if target.is_some() {
                status.config_apply.target_revision = target;
            }
            status.config_apply.error = error;
            let event =
                serde_json::json!({"type": "config-apply", "config_apply": status.config_apply});
            let _ = self.publish_locked(status, event);
            status.config_apply.clone()
        })
    }

    fn publish_hotkey(&self, status: &mut DaemonStatus) {
        let _ = self.publish_locked(
            status,
            serde_json::json!({"type": "hotkey", "hotkey": status.hotkey}),
        );
    }

    /// Snapshot of the status for `/api/status`.
    pub fn status(&self) -> DaemonStatus {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn engine_status(&self) -> EngineStatus {
        self.update(|status| status.engine.clone())
    }

    pub(crate) fn session_active(&self) -> bool {
        self.update(|status| status.session_id.is_some())
    }

    /// Mutate the status under its lock.
    fn update<T>(&self, change: impl FnOnce(&mut DaemonStatus) -> T) -> T {
        change(
            &mut self
                .status
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    /// Load the config file at `config_path` (the one the console edits).
    /// A missing file yields the defaults; an unreadable or invalid one is
    /// an error, so a reload never silently falls back to defaults.
    pub fn config(&self) -> Result<crate::config::AppConfig, String> {
        crate::config::AppConfig::load_optional(Some(&self.config_path))
            .or_else(|error| {
                if self.config_path.exists() {
                    Err(error)
                } else {
                    Ok(crate::config::AppConfig::default())
                }
            })
            .map_err(|error| error.to_string())
    }
}

struct DaemonEventSink {
    shared: std::sync::Arc<DaemonShared>,
    session_id: u64,
    sequence: u64,
}

impl EventSink for DaemonEventSink {
    fn emit(&mut self, event: OutputEvent) -> std::io::Result<()> {
        let mut status = self
            .shared
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if status.session_id != Some(self.session_id) {
            return Ok(());
        }
        self.sequence += 1;
        self.shared.publish_locked(
            &mut status,
            crate::stream::json_event(&event, self.sequence, Some(self.session_id)),
        )?;
        let recording = status.state == State::Recording.name();
        if let OutputEvent::Output(trace) = &event {
            status.mode.clone_from(&trace.selection.mode);
        }
        if let Some(session) = &mut status.session {
            match event {
                OutputEvent::Ready if recording => session.phase = "recording",
                OutputEvent::BackendSelected(backend) => session.backend_selection = Some(backend),
                OutputEvent::DevicesLoaded(devices) => session.devices = Some(devices),
                OutputEvent::Output(trace) => session.output = Some(*trace),
                OutputEvent::Transcript(transcript) => session.transcript = Some(transcript),
                OutputEvent::Phase(SessionPhase::Processing) => session.phase = "processing",
                OutputEvent::Phase(SessionPhase::Delivering) => session.phase = "delivering",
                OutputEvent::Phase(SessionPhase::NoSpeech) => {
                    session.phase = "no-speech";
                    session.transcript = None;
                }
                OutputEvent::Finalized { injection } => {
                    session.phase = "completed";
                    session.injection = injection;
                }
                OutputEvent::Cancelled => {
                    session.phase = "cancelled";
                    session.transcript = None;
                }
                OutputEvent::Error(error) => {
                    session.phase = "failed";
                    session.error = Some(error);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn retain_final_text(&mut self, text: &str) {
        let status = self
            .shared
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if status.session.as_ref().is_some_and(|session| {
            session.id == self.session_id && session.purpose == SessionPurpose::Dictation
        }) {
            *self.shared.last_final_text.lock() = Some(Arc::from(text));
        }
    }
}

async fn prune_history(owner: std::sync::Weak<DaemonShared>) {
    use crate::history::retention::{apply_policy, RecoveryStore, RetentionPolicy};
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(3600));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        let Some(shared) = owner.upgrade() else {
            return;
        };
        let directory = shared.data_dir.clone();
        let config_path = shared.config_path.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let file = crate::console::settings::load_revision(&config_path, false)
                .map_err(|e| e.to_string())?;
            let policy = RetentionPolicy::of(&file.config.history);
            let database = directory.join("history.sqlite3");
            if !database.exists() && !directory.join("recovery").exists() {
                return Ok(());
            }
            let store = crate::history::HistoryStore::open(database).map_err(|e| e.to_string())?;
            let recovery = RecoveryStore::new(directory.join("recovery"));
            let report = apply_policy(
                &store,
                &recovery,
                &policy,
                &directory.join("audio"),
                time::OffsetDateTime::now_utc(),
            )
            .map_err(|e| e.to_string())?;
            if report.failed > 0 {
                return Err(format!(
                    "could not remove {} expired recordings",
                    report.failed
                ));
            }
            Ok(())
        })
        .await;
        if let Err(error) = result.map_err(|e| e.to_string()).and_then(|result| result) {
            shared.log(format!("Voiceless: history retention: {error}"));
        }
    }
}

/// Serve the daemon until the task ends: loads the warm engine, owns the
/// gateway, the D-Bus control bus, the event signal, the web console and
/// the reaping loop.
///
/// Reserve the data directory and session bus name before loading the model
/// or starting any desktop listeners. All those resources belong to this
/// process until the daemon exits.
pub async fn serve(options: crate::stream::StreamOptions) -> Result<(), String> {
    let config_path = options
        .config_path
        .clone()
        .unwrap_or_else(crate::config::default_config_path);
    let file = Arc::new(
        crate::console::settings::load_revision(&config_path, options.config_path.is_some())
            .map_err(|error| error.to_string())?,
    );
    let config = &file.config;
    let data_dir = crate::models::default_data_dir();
    let _runtime_lock =
        crate::runtime::lock_runtime(&data_dir).map_err(|error| error.to_string())?;
    let data_dir = data_dir.canonicalize().map_err(|error| error.to_string())?;
    let (shared, command_rx) =
        DaemonShared::new(config_path, data_dir, options.mode.clone(), file.clone());
    #[cfg(target_os = "linux")]
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| format!("session bus unavailable: {error}"))?;
    #[cfg(target_os = "linux")]
    register_controller(&connection, &shared).await?;
    load_initial_engine(&shared, file.clone()).await;
    let queue_owner = shared.clone();
    match tokio::task::spawn_blocking(move || {
        jobs::start(&queue_owner)?;
        let owner = Arc::downgrade(&queue_owner);
        let queue = queue_owner
            .jobs
            .get()
            .expect("the queue has started")
            .clone();
        let runtime = crate::meeting_capture::MeetingRuntime::new(queue_owner.data_dir.clone());
        let model = queue_owner.active_file().config.local.model.clone();
        match crate::meeting_capture::start(runtime, queue, model, move |event| {
            if let Some(shared) = owner.upgrade() {
                let _ = shared.update(|status| shared.publish_locked(status, event));
            }
        }) {
            Ok(meetings) => {
                let _ = queue_owner.meetings.set(meetings);
            }
            Err(error) => {
                queue_owner.log(format!("Voiceless: meeting capture unavailable: {error}"))
            }
        }
        Ok::<(), String>(())
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => shared.log(format!(
            "Voiceless: file transcription unavailable: {error}"
        )),
        Err(error) => shared.log(format!(
            "Voiceless: file transcription failed to initialize: {error}"
        )),
    }
    tokio::spawn(prune_history(Arc::downgrade(&shared)));
    tokio::spawn(track_sessions(shared.clone()));
    #[cfg(target_os = "linux")]
    if config.console.enabled {
        let console = shared.clone();
        let (host, port) = (config.console.host.clone(), config.console.port);
        // The console is optional: a bind failure (port in use) is logged
        // and the daemon keeps serving D-Bus and the hotkey.
        tokio::spawn(async move {
            if let Err(error) = crate::console::serve_daemon(console.clone(), &host, port).await {
                console.log(format!("Voiceless: console unavailable: {error}"));
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _control_server = crate::console::start_native(shared.clone(), &config.console).await?;
    #[cfg(target_os = "linux")]
    tokio::spawn(portal_shortcut_listener(shared.clone()));
    let gateway = Gateway::new(StreamRunner {
        options,
        shared: shared.clone(),
    });
    #[cfg(target_os = "linux")]
    return serve_with_gateway(gateway, shared, command_rx, connection).await;
    #[cfg(not(target_os = "linux"))]
    serve_with_gateway(gateway, shared, command_rx).await
}

async fn load_initial_engine(shared: &DaemonShared, file: Arc<ConfigFile>) {
    let started = std::time::Instant::now();
    let config = file.clone();
    let loaded =
        tokio::task::spawn_blocking(move || crate::engine::WarmEngine::load(&config.config))
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
    let (engine, error) = match loaded {
        Ok(engine) => (engine.map(Arc::new), None),
        Err(message) => {
            shared.log(format!("Voiceless: warm engine unavailable: {message}"));
            (None, Some(message))
        }
    };
    let mut status = EngineStatus::loaded(
        &file.config,
        engine.as_deref(),
        started.elapsed().as_secs_f64(),
    );
    status.error = error;
    shared.install_config(apply::Prepared {
        file,
        engine,
        status,
    });
}

/// Validate replacement streams before changing the generation. At startup,
/// allow missing keyboards so they can recover on hotplug.
fn start_hotkey(
    shared: &Arc<DaemonShared>,
    config: &crate::config::HotkeyConfig,
    require_keyboard: bool,
) -> Result<Vec<tokio::task::JoinHandle<()>>, String> {
    use crate::hotkey::{self, Binding};

    let bindings: Arc<[Binding]> = hotkey::bindings(config)?.into();
    if bindings.is_empty() {
        shared.hotkey_generation.fetch_add(1, Ordering::Relaxed);
        shared.update(|status| {
            status.hotkey = HotkeyStatus {
                portal: std::mem::take(&mut status.hotkey.portal),
                ..HotkeyStatus::default()
            };
            shared.publish_hotkey(status);
        });
        return Ok(Vec::new());
    }
    let names = hotkey::configured_keys(config).map(|name| name.map(str::to_owned));
    let (key_tx, mut key_rx) = mpsc::channel(16);
    let listener = hotkey::Listener::open(bindings.clone(), key_tx);
    #[cfg(target_os = "linux")]
    let setup_error: Option<String> = None;
    #[cfg(not(target_os = "linux"))]
    let setup_error = listener.error().map(str::to_owned);
    let support = listener.support();
    if require_keyboard {
        if let Some(error) = &setup_error {
            return Err(error.clone());
        }
        if let Some(binding) = bindings
            .iter()
            .find(|binding| support.for_action(binding.action) == 0)
        {
            let name = names[binding.action as usize]
                .as_deref()
                .expect("configured binding");
            return Err(format!(
                "no readable keyboard provides {name}; check hotkey.{} and /dev/input permissions",
                binding.action.field()
            ));
        }
    }
    let generation = shared.hotkey_generation.fetch_add(1, Ordering::Relaxed) + 1;
    let reporter = shared.clone();
    let listener = listener.start(move |support| {
        if reporter.hotkey_generation.load(Ordering::Relaxed) != generation {
            return;
        }
        let bindings: Vec<_> = bindings
            .iter()
            .map(|binding| HotkeyBindingStatus {
                field: binding.action.field(),
                chord: names[binding.action as usize]
                    .as_ref()
                    .expect("configured binding")
                    .clone(),
                keyboards: support.for_action(binding.action),
            })
            .collect();
        let error = setup_error.clone().or_else(|| {
            bindings
                .iter()
                .find(|binding| binding.keyboards == 0)
                .map(|binding| {
                    format!(
                        "no readable keyboard provides {}; check the key and input permissions",
                        binding.chord
                    )
                })
        });
        reporter.update(|status| {
            if reporter.hotkey_generation.load(Ordering::Relaxed) != generation {
                return;
            }
            status.hotkey.key = names[0].clone();
            status.hotkey.keyboards = support.total;
            status.hotkey.error = error;
            status.hotkey.bindings = bindings;
            reporter.publish_hotkey(status);
        });
    });
    let commands = shared.commands.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(event) = key_rx.recv().await {
            if commands
                .send(DaemonCommand::Key(generation, event))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(vec![listener, forwarder])
}

/// Derive per-session metrics from the event stream: audio length (record
/// to stop), stop-to-final latency and character count; published as
/// `{"type":"session",...}` and in the status.
///
/// Runs for the daemon's lifetime as a broadcast subscriber. Timestamps are
/// taken when the `state` events are observed here, not when the gateway
/// changed state, and a lagged receiver skips events, so the numbers are
/// indicative (what the console's live page shows), not a benchmark.
async fn track_sessions(shared: std::sync::Arc<DaemonShared>) {
    let mut events = shared.events.subscribe();
    let mut recording_at: Option<std::time::Instant> = None;
    let mut stopping_at: Option<std::time::Instant> = None;
    let mut result = None;
    loop {
        let line = match events.recv().await {
            Ok(line) => line,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("state") => match event["state"].as_str() {
                Some("recording") => {
                    // Audio starts at `ready` (after connect/model load),
                    // not at the state event; reset until it arrives.
                    recording_at = None;
                    stopping_at = None;
                    result = None;
                }
                Some("stopping") => stopping_at = Some(std::time::Instant::now()),
                _ => {}
            },
            Some("ready") => recording_at = Some(std::time::Instant::now()),
            Some("transcript") if event["transcript"]["is_final"] == true => {
                let transcript = &event["transcript"];
                if let (Some(backend), Some(text)) = (
                    transcript["backend"].as_str(),
                    transcript["authoritative_text"].as_str(),
                ) {
                    result = Some((backend.to_owned(), text.chars().count()));
                }
            }
            Some("finalized") => {
                // A session that ends without passing through `stopping`
                // (error, capture ended by itself) has no stop latency.
                let (Some(recording), Some(stopping), Some((backend, chars))) =
                    (recording_at, stopping_at, result.take())
                else {
                    continue;
                };
                let metrics = SessionMetrics {
                    backend,
                    audio_seconds: stopping.duration_since(recording).as_secs_f64(),
                    stop_to_final_ms: stopping.elapsed().as_millis() as u64,
                    chars,
                    ended_at: time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                };
                let mut payload = serde_json::to_value(&metrics).unwrap_or_default();
                payload["type"] = "session".into();
                shared.update(|status| {
                    status.last_session = Some(metrics);
                    let _ = shared.publish_locked(status, payload);
                });
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "linux")]
async fn register_controller(
    connection: &zbus::Connection,
    shared: &std::sync::Arc<DaemonShared>,
) -> Result<(), String> {
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            ControllerInterface {
                shared: shared.clone(),
            },
        )
        .await
        .map_err(|error| format!("cannot export controller object: {error}"))?;
    // Neither `AllowReplacement` nor `ReplaceExisting`: a second runtime
    // on this bus fails here instead of taking the name from the running
    // one, whose clients would then reach a runtime they never started.
    match connection
        .request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(_) => Ok(()),
        Err(zbus::Error::NameTaken) => Err(format!(
            "another Voiceless runtime already owns {BUS_NAME} on this session bus"
        )),
        Err(error) => Err(format!("cannot claim bus name {BUS_NAME}: {error}")),
    }
}

/// Serialize requests from every client and reap completed sessions.
/// Publish each transition before replying to its caller.
async fn serve_with_gateway<R>(
    mut gateway: Gateway<R>,
    shared: std::sync::Arc<DaemonShared>,
    mut command_rx: mpsc::Receiver<DaemonCommand>,
    #[cfg(target_os = "linux")] connection: zbus::Connection,
) -> Result<(), String>
where
    R: SessionRunner,
{
    gateway.shared = Some(shared.clone());
    let mut config = shared.active_file();
    let mut hotkey = shortcuts::Gestures::new(&config.config.hotkey);
    let mut hotkey_tasks = start_hotkey(&shared, &config.config.hotkey, false)?;
    let mut apply_queue = apply::ApplyQueue::default();
    // Players paused for the current session; resumed when it ends.
    #[cfg(target_os = "linux")]
    let mut paused_media: Option<tokio::task::JoinHandle<Vec<String>>> = None;

    #[cfg(target_os = "linux")]
    {
        // Forward broadcast events as D-Bus signals. A lagged subscriber only
        // loses signals; `ctl watch` and the relay tolerate gaps.
        let emitter = connection
            .object_server()
            .interface::<_, ControllerInterface>(OBJECT_PATH)
            .await
            .map_err(|error| format!("cannot resolve controller object: {error}"))?
            .signal_emitter()
            .clone();
        let mut forwarded = shared.events.subscribe();
        tokio::spawn(async move {
            loop {
                match forwarded.recv().await {
                    Ok(line) => {
                        let _ = ControllerInterface::event(&emitter, &line).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        shared.log(format!("Voiceless daemon listening on {BUS_NAME}"));
    }
    // Only announce transitions once the gateway has acted.
    let mut published = State::Idle;
    loop {
        if gateway.state() == State::Idle {
            if let Some(candidate) = apply_queue.next(&shared).await {
                let _writers = match lock_revision(&shared.config_path, &candidate.file.revision) {
                    Ok(writers) => writers,
                    Err(error) => {
                        shared.publish_apply("failed", None, Some(error));
                        continue;
                    }
                };
                let candidate_config = &candidate.file.config;
                if crate::hotkey::configured_keys(&config.config.hotkey)
                    != crate::hotkey::configured_keys(&candidate_config.hotkey)
                {
                    let tasks = match start_hotkey(&shared, &candidate_config.hotkey, true) {
                        Ok(tasks) => tasks,
                        Err(error) => {
                            shared.publish_apply("failed", None, Some(error));
                            continue;
                        }
                    };
                    for task in std::mem::replace(&mut hotkey_tasks, tasks) {
                        task.abort();
                    }
                }
                hotkey = shortcuts::Gestures::new(&candidate_config.hotkey);
                config = candidate.file.clone();
                shared.install_config(candidate);
            }
        }
        let mut response = None;
        let mut exit_code = None;
        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else { break };
                // Translate key transitions before applying any session command.
                let command = match command {
                    DaemonCommand::Key(generation, event) => {
                        if generation != shared.hotkey_generation.load(Ordering::Relaxed) {
                            continue;
                        }
                        let Some(command) = hotkey.command(event, &gateway) else { continue };
                        command
                    }
                    DaemonCommand::Reload { revision, reply } => {
                        let _ = reply.send(apply_queue.request(&shared, revision));
                        continue;
                    }
                    DaemonCommand::Restore { revision, reply } => {
                        let _ = reply.send(apply_queue.restore(&shared, &revision));
                        continue;
                    }
                    command => command,
                };
                if gateway.state() == State::Idle && !apply_queue.is_pending()
                    && matches!(command, DaemonCommand::Start { .. } | DaemonCommand::StartSession { .. } | DaemonCommand::Toggle { .. } | DaemonCommand::Practice { arm: false, .. } | DaemonCommand::Narrate { .. })
                {
                    if let Some(file) = shared.refresh_session_settings(&config) {
                        config = file;
                    }
                }
                let (result, reply) = match command {
                    DaemonCommand::Toggle { mode: requested, reply } => {
                        (unless_paused(shared.is_paused(), || gateway.toggle(&requested)).map(Some), reply)
                    }
                    DaemonCommand::Start { mode: requested, reply } => {
                        (unless_paused(shared.is_paused(), || gateway.start(&requested)).map(Some), reply)
                    }
                    DaemonCommand::StartSession { mode: requested, inject, context, reply } => (
                        unless_paused(shared.is_paused(), || gateway.start_session(&requested, Some(inject), SessionPurpose::Dictation, context)).map(Some),
                        reply,
                    ),
                    DaemonCommand::Practice { arm, reply } => {
                        let result = if shared.is_paused() {
                            Err(PAUSED.to_owned())
                        } else if arm && gateway.state() != State::Idle {
                            Err("cannot arm practice during an active dictation".to_owned())
                        } else if arm {
                            gateway.practice_until = Some(std::time::Instant::now() + std::time::Duration::from_millis(PRACTICE_ARM_MS));
                            Ok(ActivateOutcome::PracticeArmed)
                        } else {
                            gateway.start_session(crate::modes::RAW_MODE_ID, Some(false), SessionPurpose::Practice, Default::default())
                        };
                        (result.map(Some), Some(reply))
                    }
                    DaemonCommand::Narrate { reply } => (
                        unless_paused(shared.is_paused(), || gateway.start_session(crate::modes::RAW_MODE_ID, Some(false), SessionPurpose::Narration, Default::default())).map(Some),
                        Some(reply),
                    ),
                    DaemonCommand::DisarmPractice { reply } => {
                        gateway.practice_until = None;
                        (Ok(None), Some(reply))
                    }
                    DaemonCommand::SetPaused { paused, reply } => {
                        if shared.set_paused(paused) {
                            shared.log(if paused { "Voiceless: dictation paused" } else { "Voiceless: dictation resumed" });
                        }
                        (Ok(None), Some(reply))
                    }
                    DaemonCommand::Stop { reply } => (gateway.stop().map(Some), reply),
                    DaemonCommand::Repaste { reply } => {
                        let result = if gateway.state() == State::Idle {
                            repaste(&shared, &config.config.inject).await
                        } else {
                            Err("cannot repaste while a dictation is active".to_owned())
                        };
                        (result.map(Some), reply)
                    }
                    DaemonCommand::Key(_, _) | DaemonCommand::Reload { .. } | DaemonCommand::Restore { .. } => {
                        unreachable!("handled above")
                    }
                    DaemonCommand::Cancel { reply } => {
                        gateway.cancel().await;
                        (Ok(None), reply)
                    }
                    DaemonCommand::ControlSession { session_id, action, reply } => {
                        let result = if gateway.session_id() != Some(session_id) {
                            Err(format!("session {session_id} is no longer active"))
                        } else {
                            match action {
                                ControlCommand::Stop => gateway.stop().map(Some),
                                ControlCommand::Cancel => {
                                    gateway.cancel().await;
                                    Ok(None)
                                }
                            }
                        };
                        (result, Some(reply))
                    }
                };
                if let Err(message) = &result {
                    shared.log(format!("Voiceless: {message}"));
                }
                response = reply.map(|reply| (reply, result.map(|outcome| CommandReceipt {
                    outcome,
                    session_id: gateway.session_id(),
                })));
            },
            // Reaping tick: the gateway only returns to idle through `poll`,
            // and nothing wakes this loop when a session task ends on its
            // own, so it is polled every 20 ms (bounds the idle latency).
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                if let Some(code) = gateway.poll().await {
                    exit_code = Some(code);
                    shared.log(format!("session finished with exit code {code}"));
                }
            }
        }
        // Publish a transition at most once per loop iteration. Side effects
        // are keyed on the (from, to) pair: a session that ends straight from
        // Recording (error, capture EOF) plays no stop cue but still resumes
        // the media it paused.
        if gateway.state() != published {
            #[cfg(target_os = "linux")]
            match (published, gateway.state()) {
                (State::Idle, State::Recording) => {
                    if config.config.audio.cues {
                        crate::media::play_cue(crate::media::Cue::Start);
                    }
                    if config.config.audio.pause_media {
                        // Pausing talks to every MPRIS player over D-Bus;
                        // keep it off this loop and collect the names later.
                        paused_media = Some(tokio::spawn(crate::media::pause_playing()));
                    }
                }
                (State::Recording, State::Stopping) if config.config.audio.cues => {
                    crate::media::play_cue(crate::media::Cue::Stop);
                }
                (_, State::Idle) => {
                    if let Some(paused) = paused_media.take() {
                        tokio::spawn(async move {
                            if let Ok(names) = paused.await {
                                crate::media::resume(names).await;
                            }
                        });
                    }
                }
                _ => {}
            }
            published = gateway.state();
            // Starting is published before spawning the session worker.
            if published != State::Recording {
                shared.publish_state(published, gateway.last_session_id, exit_code, None);
            }
        }
        if let Some((reply, result)) = response {
            let _ = reply.send(result);
        }
    }
    Ok(())
}

async fn repaste(
    shared: &DaemonShared,
    inject: &crate::config::InjectConfig,
) -> Result<ActivateOutcome, String> {
    let text = shared
        .last_final_text
        .lock()
        .clone()
        .ok_or_else(|| "there is no completed dictation to repaste".to_owned())?;
    let mut inject = inject.clone();
    inject.focus_command.clear();
    let result = crate::stream::repaste_text(&inject, &text).await;
    if !result.ok {
        return Err(format!(
            "repaste via {} failed: {}",
            result.method, result.message
        ));
    }
    Ok(ActivateOutcome::Repasted)
}

/// Id of the one shortcut registered with the portal; the user binds the
/// actual key in the compositor's portal dialog.
#[cfg(target_os = "linux")]
const TOGGLE_SHORTCUT_ID: &str = "toggle";

#[cfg(target_os = "linux")]
async fn portal_shortcut_listener(shared: Arc<DaemonShared>) {
    if let Err(error) = listen_to_portal(&shared).await {
        shared.log(format!("Voiceless: {error}"));
        shared.update(|status| {
            status.hotkey.portal = PortalShortcutStatus {
                trigger: None,
                error: Some(error),
            };
            shared.publish_hotkey(status);
        });
    }
}

/// Retain the portal's existing toggle. Evdev and compositor bindings keep
/// working when the system rejects the portal request.
#[cfg(target_os = "linux")]
async fn listen_to_portal(shared: &DaemonShared) -> Result<(), String> {
    let portal = GlobalShortcuts::new()
        .await
        .map_err(|error| format!("GlobalShortcuts unavailable: {error}"))?;
    let session = portal
        .create_session()
        .await
        .map_err(|error| format!("GlobalShortcuts session: {error}"))?;
    let mut closed = session
        .receive_closed()
        .await
        .map_err(|error| format!("GlobalShortcuts session status: {error}"))?;
    let mut activated = portal
        .receive_activated()
        .await
        .map_err(|error| format!("GlobalShortcuts activations: {error}"))?;
    let mut changed = portal
        .receive_shortcuts_changed()
        .await
        .map_err(|error| format!("GlobalShortcuts bindings: {error}"))?;
    let shortcut = NewShortcut::new(TOGGLE_SHORTCUT_ID, "开始或停止 Voiceless 语音输入");
    let request = portal
        .bind_shortcuts(&session, &[shortcut], None)
        .await
        .map_err(|error| format!("GlobalShortcuts binding: {error}"))?;
    let bound = request
        .response()
        .map_err(|error| format!("GlobalShortcuts binding rejected: {error}"))?;
    let report = |shortcuts: &[ashpd::desktop::global_shortcuts::Shortcut]| {
        let trigger = shortcuts
            .iter()
            .find(|entry| entry.id() == TOGGLE_SHORTCUT_ID)
            .map(|entry| entry.trigger_description())
            .filter(|trigger| !trigger.is_empty())
            .map(str::to_owned);
        let error = trigger
            .is_none()
            .then(|| "The desktop portal has no toggle binding".to_owned());
        shared.update(|status| {
            status.hotkey.portal = PortalShortcutStatus { trigger, error };
            shared.publish_hotkey(status);
        });
    };
    report(bound.shortcuts());
    loop {
        tokio::select! {
            event = activated.next() => {
                let Some(event) = event else { break };
                if event.shortcut_id() == TOGGLE_SHORTCUT_ID
                    && shared.commands.send(DaemonCommand::Toggle { mode: String::new(), reply: None }).await.is_err()
                {
                    return Ok(());
                }
            }
            event = changed.next() => {
                let Some(event) = event else { break };
                report(event.shortcuts());
            }
            _ = closed.next() => break,
        }
    }
    Err("The desktop portal closed the GlobalShortcuts session".to_owned())
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
