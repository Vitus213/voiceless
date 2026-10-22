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
//!   uses `StreamRunner`, which runs `stream::run_with_env` with the warm
//!   engine and the event broadcast; tests inject scripted runners.
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
//! each reply `"queued"` as soon as the command is in the channel (they do
//! not wait for the gateway, so a rejected command is only logged);
//! `Status()` returns `idle`, `recording` or `stopping`; the `Event(s json)`
//! signal carries every broadcast line. An empty mode means the daemon's
//! `--mode` default.
//!
//! # Configuration
//!
//! `[asr] streaming_backend` / `fallback_backend` (which engine to warm),
//! `[hotkey] key`, `mode`, `ctrl_mode`, `hold_threshold_ms`, `[audio]
//! pause_media`, `cues`, and `[console] enabled`, `host`, `port`. The
//! console's `POST /api/engine/reload` re-reads the file (`DaemonCommand::
//! Reload`), which is only honoured while idle.
//!
//! # Failure behaviour
//!
//! No session bus, an unexportable object or an unclaimable bus name make
//! `serve` return an error (the daemon cannot run without its control bus).
//! Everything else degrades: a warm-engine load failure is logged and
//! sessions load on demand; an unreadable `/dev/input` is reported in
//! `HotkeyStatus`; a console bind failure and a missing portal backend are
//! logged. A failed session task is reaped with exit code 1.

use futures_util::StreamExt;
use std::future::Future;
use std::pin::Pin;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use tokio::sync::mpsc;

use crate::coordinator::ControlCommand;

/// What a successful `Gateway::toggle`, `start` or `stop` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateOutcome {
    /// A new session task was spawned; the gateway is now `Recording`.
    Started,
    /// A normal stop was requested; the gateway is now `Stopping` until the
    /// task finishes and `Gateway::poll` reaps it.
    Stopping,
}

/// Session factory boundary. Production wraps the stream coordinator;
/// tests inject scripted fixtures. `inject` overrides the daemon's
/// injection setting for this session only.
pub trait SessionRunner {
    /// Build one session as a future that the gateway spawns as a task.
    /// `mode` is the resolved mode id (the daemon has already replaced an
    /// empty request with its default), `control` delivers the gateway's
    /// `Stop`/`Cancel`, and the future resolves to the session exit code
    /// (`0`, `1` or `130`, the `stream` contract) or a setup error message.
    /// Must return quickly: the gateway is `Recording` as soon as it returns.
    fn start(
        &mut self,
        mode: &str,
        inject: Option<bool>,
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
        if self.state != State::Idle {
            return Err(format!("cannot start: session is {}", self.state.name()));
        }
        let (tx, rx) = mpsc::channel(8);
        let future = self.runner.start(mode, inject, rx);
        self.task = Some(tokio::spawn(future));
        self.control_tx = Some(tx);
        self.state = State::Recording;
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
        mode: &str,
        inject: Option<bool>,
        control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>> {
        let mut options = self.options.clone();
        options.mode = mode.to_owned();
        if let Some(inject) = inject {
            options.inject = inject;
        }
        // The engine current at session start. A reload is only honoured
        // while idle, so this Arc is never swapped under a running session;
        // holding it here just keeps the engine alive until the task ends.
        let env = crate::stream::SessionEnv {
            warm: self.shared.warm(),
            events: Some(self.shared.events.clone()),
        };
        Box::pin(async move {
            crate::stream::run_with_env(options, control, env)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// Well-known session bus name the daemon claims; `ctl::stream_via_daemon`
/// checks its owner to decide whether a daemon is running.
pub const BUS_NAME: &str = "dev.voiceless.Daemon";
/// Object path of the controller.
pub const OBJECT_PATH: &str = "/dev/voiceless/Daemon";
/// Interface name of the controller (methods and the `Event` signal).
pub const INTERFACE: &str = "dev.voiceless.Daemon.Controller";

/// D-Bus control interface. Toggle/Activate map to the gateway hotkey
/// semantics, Start/Stop to hold-to-talk press/release, Cancel force-stops.
/// An empty mode selects the daemon's default mode.
///
/// Every mutating method only enqueues a `DaemonCommand` and replies
/// `"queued"`; the gateway acts on it later in the command loop. A caller
/// therefore learns about a rejected command (e.g. `Start` while recording)
/// only from the daemon log or the next `Status`/`Event`, never from the
/// method reply. The reply is an error only when the queue (8 slots) is
/// full or the daemon loop has gone.
#[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
impl ControllerInterface {
    /// `Toggle` in the daemon's default mode (the portal shortcut and
    /// mode-less bindings).
    fn activate(&self) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Toggle(String::new()))
    }

    /// Start a session in `mode` when idle, stop it when recording, and
    /// reject (logged only) while stopping.
    fn toggle(&self, mode: &str) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Toggle(mode.to_owned()))
    }

    /// Hold-to-talk press: start a session in `mode`; ignored (logged) when
    /// one is already active.
    fn start(&self, mode: &str) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Start(mode.to_owned()))
    }

    /// Start with an explicit injection choice (`voiceless stream`
    /// delegating to the daemon, e.g. with `--no-inject`).
    fn start_session(&self, mode: &str, inject: bool) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::StartSession {
            mode: mode.to_owned(),
            inject,
        })
    }

    /// Hold-to-talk release: normal stop, flush and type the final text.
    fn stop(&self) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Stop)
    }

    /// Abort the session without typing anything (exit code 130).
    fn cancel(&self) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Cancel)
    }

    /// Current session state: idle, recording or stopping. Reads the copy
    /// the command loop publishes after each transition, so it can trail the
    /// gateway by one loop iteration.
    fn status(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_string()
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

impl ControllerInterface {
    /// Enqueue without waiting; maps a full or closed channel to
    /// `org.freedesktop.DBus.Error.Failed`.
    fn queue(&self, command: DaemonCommand) -> zbus::fdo::Result<String> {
        self.commands
            .try_send(command)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok("queued".to_owned())
    }
}

/// Everything the command loop can be asked to do, from any source (D-Bus
/// methods, the console API, the hotkey listener, the portal shortcut).
/// Mode strings may be empty, meaning the daemon's `--mode` default.
#[derive(Debug)]
pub enum DaemonCommand {
    /// `Gateway::toggle` in the given mode.
    Toggle(String),
    /// `Gateway::start` in the given mode (hold-to-talk press).
    Start(String),
    /// `Gateway::start_with` an explicit injection choice (`stream`
    /// delegation through `StartSession`).
    StartSession { mode: String, inject: bool },
    /// `Gateway::stop` (hold-to-talk release).
    Stop,
    /// `Gateway::cancel`; never fails, idle is a no-op.
    Cancel,
    /// A raw push-to-talk press or release, interpreted by `HotkeyMachine`.
    Key(crate::hotkey::KeyEvent),
    /// Re-read the config, reload the warm engine and hotkey (idle only).
    /// The reply carries the new `EngineStatus`, or the reason (busy or an
    /// invalid config file) when nothing was changed.
    Reload(tokio::sync::oneshot::Sender<Result<EngineStatus, String>>),
}

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

/// State behind the `dev.voiceless.Daemon.Controller` object: a sender into
/// the command loop and the last published state name for `Status`.
struct ControllerInterface {
    commands: mpsc::Sender<DaemonCommand>,
    state: std::sync::Arc<std::sync::Mutex<&'static str>>,
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
}

/// Push-to-talk listener state reported to the console (`/api/status`
/// `hotkey`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HotkeyStatus {
    /// Configured evdev key name; `None` when `[hotkey] key` is unset.
    pub key: Option<String>,
    /// Input devices being watched for the key (0 means no read access).
    pub keyboards: usize,
    /// Unknown key name or no readable keyboard.
    pub error: Option<String>,
}

/// Timing of the last completed session, measured from daemon events.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMetrics {
    /// Configured primary backend at the time (not the fallback actually
    /// used, if any).
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

/// Snapshot the console renders (`/api/status`, plus fields the console
/// adds itself such as uptime and GPU memory).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonStatus {
    /// `State::name` of the last published state.
    pub state: &'static str,
    /// Mode of the current or most recent session (the default until one
    /// starts).
    pub mode: String,
    /// `[asr] streaming_backend` as of the last (re)load.
    pub backend: String,
    /// `[asr] fallback_backend` as of the last (re)load.
    pub fallback_backend: Option<String>,
    pub engine: EngineStatus,
    pub hotkey: HotkeyStatus,
    /// `None` until the first session finalizes.
    pub last_session: Option<SessionMetrics>,
}

/// Ring-buffer size of `DaemonShared::logs` (`/api/logs`).
const LOG_LINES: usize = 500;

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
    /// The config file the console edits and `Reload` re-reads: `--config`
    /// or the XDG default.
    pub config_path: std::path::PathBuf,
    /// The daemon's `--mode`, used when a request names no mode.
    pub default_mode: String,
    /// Daemon start time (console uptime).
    pub started: std::time::Instant,
    /// The warm engine, replaced only by `load_engine` while idle.
    warm: std::sync::RwLock<Option<std::sync::Arc<crate::engine::WarmEngine>>>,
    status: std::sync::Mutex<DaemonStatus>,
    /// Last `LOG_LINES` log lines, oldest first.
    logs: std::sync::Mutex<std::collections::VecDeque<String>>,
}

impl DaemonShared {
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
        let _ = self
            .events
            .send(serde_json::json!({"type": "log", "line": line}).to_string());
    }

    /// Copy of the retained log lines, oldest first.
    pub fn logs(&self) -> Vec<String> {
        let logs = self.logs.lock().unwrap_or_else(|error| error.into_inner());
        logs.iter().cloned().collect()
    }

    /// The current warm engine, if one is loaded (cloud backends and failed
    /// loads give `None`; sessions then load on demand).
    pub fn warm(&self) -> Option<std::sync::Arc<crate::engine::WarmEngine>> {
        self.warm
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Snapshot of the status for `/api/status`.
    pub fn status(&self) -> DaemonStatus {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Mutate the status under its lock.
    fn update(&self, change: impl FnOnce(&mut DaemonStatus)) {
        change(
            &mut self
                .status
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
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

/// Serve the daemon until the task ends: loads the warm engine, owns the
/// gateway, the D-Bus control bus, the event signal, the web console and
/// the reaping loop.
///
/// Order matters: the config is validated first (an invalid file is fatal),
/// the engine is loaded before anything can start a session (so the first
/// hotkey press never waits), the console is started best-effort, and only
/// then is the control bus exported by `serve_with_gateway`. Returns an
/// error for a bad config or an unavailable session bus/bus name; otherwise
/// runs until the command channel closes, which in practice is never.
pub async fn serve(options: crate::stream::StreamOptions) -> Result<(), String> {
    let config_path = options
        .config_path
        .clone()
        .unwrap_or_else(crate::config::default_config_path);
    let config = crate::config::AppConfig::load_optional(options.config_path.as_deref())
        .map_err(|error| error.to_string())?;
    let (events, _) = tokio::sync::broadcast::channel::<String>(256);
    let (command_tx, command_rx) = mpsc::channel::<DaemonCommand>(8);
    let shared = std::sync::Arc::new(DaemonShared {
        commands: command_tx,
        events: events.clone(),
        config_path,
        default_mode: options.mode.clone(),
        started: std::time::Instant::now(),
        warm: std::sync::RwLock::new(None),
        status: std::sync::Mutex::new(DaemonStatus {
            state: State::Idle.name(),
            mode: options.mode.clone(),
            backend: config.asr.streaming_backend.clone(),
            fallback_backend: config.asr.fallback_backend.clone(),
            engine: EngineStatus::default(),
            hotkey: HotkeyStatus::default(),
            last_session: None,
        }),
        logs: std::sync::Mutex::new(std::collections::VecDeque::new()),
    });
    load_engine(&shared, &config).await;
    tokio::spawn(track_sessions(shared.clone()));
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
    let gateway = Gateway::new(StreamRunner {
        options,
        shared: shared.clone(),
    });
    serve_with_gateway(gateway, shared, command_rx, config).await
}

/// (Re)load the warm engine for `config`, releasing the old one first so
/// its VRAM is free for the new model. Never fails: the outcome lands in
/// `DaemonStatus::engine` and the log. Also refreshes the status' backend
/// names, since a reload may have changed them.
async fn load_engine(shared: &std::sync::Arc<DaemonShared>, config: &crate::config::AppConfig) {
    *shared
        .warm
        .write()
        .unwrap_or_else(|error| error.into_inner()) = None;
    let started = std::time::Instant::now();
    let warm_config = config.clone();
    // Model loading is CPU/GPU-bound and can take seconds: keep it off the
    // runtime threads so D-Bus and the console stay responsive meanwhile.
    let loaded = tokio::task::spawn_blocking(move || crate::engine::WarmEngine::load(&warm_config))
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
    let seconds = started.elapsed().as_secs_f64();
    let status = match loaded {
        Ok(Some(engine)) => {
            let description = engine.describe();
            shared.log(format!("Voiceless: {description} loaded in {seconds:.1}s"));
            *shared
                .warm
                .write()
                .unwrap_or_else(|error| error.into_inner()) = Some(std::sync::Arc::new(engine));
            EngineStatus {
                loaded: true,
                description: Some(description),
                load_seconds: Some(seconds),
                error: None,
            }
        }
        // Cloud backend: nothing to keep warm.
        Ok(None) => EngineStatus::default(),
        Err(message) => {
            // Sessions still work: they load on demand and report the error.
            shared.log(format!("Voiceless: warm engine unavailable: {message}"));
            EngineStatus {
                error: Some(message),
                ..EngineStatus::default()
            }
        }
    };
    shared.update(|current| {
        current.engine = status;
        current.backend = config.asr.streaming_backend.clone();
        current.fallback_backend = config.asr.fallback_backend.clone();
    });
}

/// Start the evdev push-to-talk listener for `[hotkey] key`; returns the
/// tasks to abort on reload (one per watched device plus the forwarder that
/// turns `KeyEvent`s into `DaemonCommand::Key`). Every failure (key unset,
/// unknown key name, no readable keyboard) yields no tasks and is reported
/// through `DaemonStatus::hotkey`; the daemon keeps running without a key.
fn start_hotkey(
    shared: &std::sync::Arc<DaemonShared>,
    config: &crate::config::AppConfig,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some(name) = config.hotkey.key.clone() else {
        shared.update(|status| status.hotkey = HotkeyStatus::default());
        return Vec::new();
    };
    let key = match crate::hotkey::parse_key(&name) {
        Ok(key) => key,
        Err(error) => {
            shared.update(|status| {
                status.hotkey = HotkeyStatus {
                    key: Some(name.clone()),
                    keyboards: 0,
                    error: Some(error),
                }
            });
            return Vec::new();
        }
    };
    let (key_tx, mut key_rx) = mpsc::channel(16);
    let mut tasks = crate::hotkey::listen(key, key_tx);
    // `listen` returns no task when no device could be opened, which on a
    // desktop almost always means the user is not in the `input` group.
    let error = if tasks.is_empty() {
        let message = format!(
            "no readable keyboard has {name}; add your user to the `input` group and log in again"
        );
        shared.log(format!("Voiceless: {message}"));
        Some(message)
    } else {
        shared.log(format!(
            "Voiceless: push-to-talk on {name} ({} keyboards)",
            tasks.len()
        ));
        None
    };
    shared.update(|status| {
        status.hotkey = HotkeyStatus {
            key: Some(name.clone()),
            keyboards: tasks.len(),
            error,
        }
    });
    let commands = shared.commands.clone();
    tasks.push(tokio::spawn(async move {
        while let Some(event) = key_rx.recv().await {
            if commands.send(DaemonCommand::Key(event)).await.is_err() {
                break;
            }
        }
    }));
    tasks
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
    let mut chars = 0;
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
                    recording_at = Some(std::time::Instant::now());
                    stopping_at = None;
                    chars = 0;
                }
                Some("stopping") => stopping_at = Some(std::time::Instant::now()),
                _ => {}
            },
            Some("transcript") if event["transcript"]["is_final"] == true => {
                chars = event["transcript"]["authoritative_text"]
                    .as_str()
                    .map_or(0, |text| text.chars().count());
            }
            Some("finalized") => {
                // A session that ends without passing through `stopping`
                // (error, capture ended by itself) has no stop latency.
                let (Some(recording), Some(stopping)) = (recording_at, stopping_at) else {
                    continue;
                };
                let metrics = SessionMetrics {
                    backend: shared.status().backend,
                    audio_seconds: stopping.duration_since(recording).as_secs_f64(),
                    stop_to_final_ms: stopping.elapsed().as_millis() as u64,
                    chars,
                    ended_at: time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                };
                let mut payload = serde_json::to_value(&metrics).unwrap_or_default();
                payload["type"] = "session".into();
                let _ = shared.events.send(payload.to_string());
                shared.update(|status| status.last_session = Some(metrics));
            }
            _ => {}
        }
    }
}

/// The daemon's command loop, generic over the runner so tests can drive it
/// with fixtures. Exports the controller on the session bus and claims
/// `BUS_NAME` (both fatal on failure), forwards the event broadcast as the
/// `Event` signal, registers the portal shortcut, then loops until the
/// command channel closes:
///
/// - every `DaemonCommand` is applied to the gateway here and nowhere else;
/// - a 20 ms tick reaps finished sessions through `Gateway::poll`;
/// - after each iteration a state change is published exactly once: media
///   pause/cues, the `Status` copy, `DaemonStatus` and the `state` event.
///
/// A rejected gateway command is logged, never returned to the caller (the
/// D-Bus reply was already sent as "queued").
async fn serve_with_gateway<R>(
    mut gateway: Gateway<R>,
    shared: std::sync::Arc<DaemonShared>,
    mut command_rx: mpsc::Receiver<DaemonCommand>,
    mut config: crate::config::AppConfig,
) -> Result<(), String>
where
    R: SessionRunner,
{
    let events = shared.events.clone();
    let default_mode = shared.default_mode.clone();
    let command_tx = shared.commands.clone();
    // Rebuilt on reload so new `[hotkey]` thresholds and modes apply.
    let hotkey_machine = |config: &crate::config::AppConfig| {
        HotkeyMachine::new(
            std::time::Duration::from_millis(config.hotkey.hold_threshold_ms),
            config.hotkey.mode.clone(),
            config.hotkey.ctrl_mode.clone(),
        )
    };
    let mut hotkey = hotkey_machine(&config);
    let mut hotkey_tasks = start_hotkey(&shared, &config);
    // Players paused for the current session; resumed when it ends.
    let mut paused_media: Option<tokio::task::JoinHandle<Vec<String>>> = None;
    // The state name `Status()` answers with; written on every transition.
    let state = std::sync::Arc::new(std::sync::Mutex::new(State::Idle.name()));

    let connection = zbus::Connection::session()
        .await
        .map_err(|error| format!("session bus unavailable: {error}"))?;
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            ControllerInterface {
                commands: command_tx.clone(),
                state: state.clone(),
            },
        )
        .await
        .map_err(|error| format!("cannot export controller object: {error}"))?;
    connection
        .request_name(BUS_NAME)
        .await
        .map_err(|error| format!("cannot claim bus name {BUS_NAME}: {error}"))?;

    // Forward broadcast events as D-Bus signals. A lagged subscriber only
    // loses signals; `ctl watch` and the relay tolerate gaps.
    let emitter = connection
        .object_server()
        .interface::<_, ControllerInterface>(OBJECT_PATH)
        .await
        .map_err(|error| format!("cannot resolve controller object: {error}"))?
        .signal_emitter()
        .clone();
    let mut forwarded = events.subscribe();
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

    // Best-effort portal global shortcut; unavailable portals fall back to
    // compositor bindings over the control bus above.
    tokio::spawn(portal_shortcut_listener(command_tx.clone()));

    shared.log(format!("Voiceless daemon listening on {BUS_NAME}"));
    // `published` is the last state announced to listeners; `session_mode`
    // the mode of the session being started, reported with the state event.
    let mut published = State::Idle;
    let mut session_mode = default_mode.clone();
    loop {
        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else { break };
                let mode = |requested: String| {
                    if requested.is_empty() { default_mode.clone() } else { requested }
                };
                // Capture the requested mode before the gateway acts, so the
                // idle->recording event names the mode the session runs in.
                if gateway.state() == State::Idle {
                    if let DaemonCommand::Toggle(requested)
                    | DaemonCommand::Start(requested)
                    | DaemonCommand::StartSession {
                        mode: requested, ..
                    } = &command
                    {
                        session_mode = mode(requested.clone());
                    }
                }
                // Key events and reloads are resolved first: a key becomes a
                // plain Start/Stop (or nothing), a reload is handled in full.
                let command = match command {
                    DaemonCommand::Key(event) => {
                        let now = std::time::Instant::now();
                        let action = match event {
                            crate::hotkey::KeyEvent::Press { ctrl } => {
                                hotkey.on_press(gateway.state(), ctrl, now)
                            }
                            crate::hotkey::KeyEvent::Release => {
                                hotkey.on_release(gateway.state(), now)
                            }
                        };
                        match action {
                            KeyAction::Start(requested) => {
                                session_mode = mode(requested.clone());
                                DaemonCommand::Start(requested)
                            }
                            KeyAction::Stop => DaemonCommand::Stop,
                            KeyAction::Nothing => continue,
                        }
                    }
                    DaemonCommand::Reload(reply) => {
                        // Swapping the engine or the key under a running
                        // session is never attempted; the console shows the
                        // error and the user retries when idle.
                        if gateway.state() != State::Idle {
                            let _ = reply.send(Err("a session is running; reload when idle".to_owned()));
                            continue;
                        }
                        match shared.config() {
                            Ok(fresh) => {
                                config = fresh;
                                // Stop the old listener before the new one
                                // so a key is never delivered twice.
                                for task in hotkey_tasks.drain(..) {
                                    task.abort();
                                }
                                // Blocks this loop (no commands are handled)
                                // for the duration of the model load.
                                load_engine(&shared, &config).await;
                                hotkey = hotkey_machine(&config);
                                hotkey_tasks = start_hotkey(&shared, &config);
                                let _ = reply.send(Ok(shared.status().engine));
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                            }
                        }
                        continue;
                    }
                    command => command,
                };
                let result = match command {
                    DaemonCommand::Toggle(requested) => gateway.toggle(&mode(requested)).map(Some),
                    DaemonCommand::Start(requested) => gateway.start(&mode(requested)).map(Some),
                    DaemonCommand::StartSession { mode: requested, inject } => {
                        gateway.start_with(&mode(requested), Some(inject)).map(Some)
                    }
                    DaemonCommand::Stop => gateway.stop().map(Some),
                    DaemonCommand::Key(_) | DaemonCommand::Reload(_) => {
                        unreachable!("handled above")
                    }
                    DaemonCommand::Cancel => {
                        gateway.cancel().await;
                        Ok(None)
                    }
                };
                if let Err(message) = result {
                    shared.log(format!("Voiceless: {message}"));
                }
            },
            // Reaping tick: the gateway only returns to idle through `poll`,
            // and nothing wakes this loop when a session task ends on its
            // own, so it is polled every 20 ms (bounds the idle latency).
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                if let Some(code) = gateway.poll().await {
                    shared.log(format!("session finished with exit code {code}"));
                }
            }
        }
        // Publish a transition at most once per loop iteration. Side effects
        // are keyed on the (from, to) pair: a session that ends straight from
        // Recording (error, capture EOF) plays no stop cue but still resumes
        // the media it paused.
        if gateway.state() != published {
            match (published, gateway.state()) {
                (State::Idle, State::Recording) => {
                    if config.audio.cues {
                        crate::media::play_cue(crate::media::Cue::Start);
                    }
                    if config.audio.pause_media {
                        // Pausing talks to every MPRIS player over D-Bus;
                        // keep it off this loop and collect the names later.
                        paused_media = Some(tokio::spawn(crate::media::pause_playing()));
                    }
                }
                (State::Recording, State::Stopping) if config.audio.cues => {
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
            // Order: D-Bus `Status` copy, console status, then the event, so
            // a listener that reacts to the event sees a consistent status.
            published = gateway.state();
            *state.lock().unwrap_or_else(|error| error.into_inner()) = published.name();
            shared.update(|status| {
                status.state = published.name();
                status.mode = session_mode.clone();
            });
            let _ = events.send(
                serde_json::json!({
                    "type": "state",
                    "state": published.name(),
                    "mode": session_mode,
                })
                .to_string(),
            );
        }
    }
    Ok(())
}

/// Id of the one shortcut registered with the portal; the user binds the
/// actual key in the compositor's portal dialog.
const TOGGLE_SHORTCUT_ID: &str = "toggle";

/// Register a single global shortcut through the XDG Desktop Portal and
/// forward activations to the daemon command channel. Never fatal: compositors
/// without a GlobalShortcuts backend (or broken ones, e.g. GNOME 48) simply
/// skip portal binding and keep working through `dev.voiceless.Daemon`.
/// Each activation is a `Toggle` in the default mode; the portal offers no
/// release event, so hold-to-talk is only available through `[hotkey]` or
/// `ctl start`/`stop`.
async fn portal_shortcut_listener(commands: mpsc::Sender<DaemonCommand>) {
    let portal = match GlobalShortcuts::new().await {
        Ok(portal) => portal,
        Err(error) => {
            eprintln!(
                "Voiceless: GlobalShortcuts portal unavailable ({error}); use compositor bindings on {BUS_NAME}"
            );
            return;
        }
    };
    let session = match portal.create_session().await {
        Ok(session) => session,
        Err(error) => {
            eprintln!("Voiceless: cannot create GlobalShortcuts session: {error}");
            return;
        }
    };
    // The description is what the portal's binding dialog shows the user.
    // No preferred trigger is suggested; the compositor (or the user) picks.
    let shortcut = NewShortcut::new(TOGGLE_SHORTCUT_ID, "开始或停止 Voiceless 语音输入");
    match portal.bind_shortcuts(&session, &[shortcut], None).await {
        Ok(request) => match request.response() {
            Ok(bound) => {
                let bound_ids: Vec<&str> =
                    bound.shortcuts().iter().map(|entry| entry.id()).collect();
                if bound_ids.is_empty() {
                    eprintln!(
                        "Voiceless: portal bound no shortcuts; configure them in your compositor or use {BUS_NAME}"
                    );
                    return;
                }
                println!("Voiceless: portal shortcuts bound: {bound_ids:?}");
            }
            Err(error) => {
                eprintln!("Voiceless: portal shortcut binding rejected: {error}");
                return;
            }
        },
        Err(error) => {
            eprintln!("Voiceless: cannot bind GlobalShortcuts: {error}");
            return;
        }
    }
    let mut activated = match portal.receive_activated().await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("Voiceless: cannot subscribe to GlobalShortcuts: {error}");
            return;
        }
    };
    // This daemon owns exactly one portal session with one shortcut id, and
    // the portal only delivers Activated for sessions this application
    // created, so matching the shortcut id is sufficient.
    while let Some(event) = activated.next().await {
        if event.shortcut_id() == TOGGLE_SHORTCUT_ID {
            let _ = commands.send(DaemonCommand::Toggle(String::new())).await;
        }
    }
}
