//! Resident session daemon: one session at a time, hotkey-style toggle
//! semantics (idle -> start, recording -> normal stop, stopping -> reject),
//! explicit start/stop/cancel, and a warm engine shared by every session so
//! a hotkey never waits for a model load. Session events are broadcast as
//! `Event` signals (JSON lines, the `stream --json` schema) for overlays and
//! `voiceless ctl watch`.
//!
//! Activation sources: XDG Desktop Portal GlobalShortcuts (best effort,
//! requires a compositor portal backend), the `dev.voiceless.Daemon` D-Bus
//! control bus, and compositor bindings that call either.

use futures_util::StreamExt;
use std::future::Future;
use std::pin::Pin;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use tokio::sync::mpsc;

use crate::coordinator::ControlCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateOutcome {
    Started,
    Stopping,
}

/// Session factory boundary. Production wraps the stream coordinator;
/// tests inject scripted fixtures. `inject` overrides the daemon's
/// injection setting for this session only.
pub trait SessionRunner {
    fn start(
        &mut self,
        mode: &str,
        inject: Option<bool>,
        control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Stopping,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Recording => "recording",
            State::Stopping => "stopping",
        }
    }
}

pub struct Gateway<R: SessionRunner> {
    runner: R,
    state: State,
    control_tx: Option<mpsc::Sender<ControlCommand>>,
    task: Option<tokio::task::JoinHandle<Result<i32, String>>>,
}

impl<R: SessionRunner> Gateway<R> {
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            state: State::Idle,
            control_tx: None,
            task: None,
        }
    }

    /// Hotkey activation in the default mode; see [`Gateway::toggle`].
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
    pub fn start(&mut self, mode: &str) -> Result<ActivateOutcome, String> {
        self.start_with(mode, None)
    }

    /// [`Gateway::start`] with a per-session injection override.
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

    /// Normal stop of a recording session (hold-to-talk release).
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

    pub fn state(&self) -> State {
        self.state
    }

    /// Force cancel: valid from any active state; idle is a no-op.
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
    /// idle state once the task completes.
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

    /// Whether a session task is currently running or finishing.
    pub fn is_active(&self) -> bool {
        self.task.is_some()
    }

    fn reset(&mut self) {
        self.state = State::Idle;
        self.control_tx = None;
    }
}

/// Production runner: executes one streaming session per activation through
/// the shared coordinator, with the daemon's warm engine and event
/// broadcast, controlled exclusively via the gateway channel.
pub struct StreamRunner {
    pub options: crate::stream::StreamOptions,
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
        // The engine current at session start (a reload may swap it later).
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

pub const BUS_NAME: &str = "dev.voiceless.Daemon";
pub const OBJECT_PATH: &str = "/dev/voiceless/Daemon";
pub const INTERFACE: &str = "dev.voiceless.Daemon.Controller";

/// D-Bus control interface. Toggle/Activate map to the gateway hotkey
/// semantics, Start/Stop to hold-to-talk press/release, Cancel force-stops.
/// An empty mode selects the daemon's default mode.
#[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
impl ControllerInterface {
    fn activate(&self) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Toggle(String::new()))
    }

    fn toggle(&self, mode: &str) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Toggle(mode.to_owned()))
    }

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

    fn stop(&self) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Stop)
    }

    fn cancel(&self) -> zbus::fdo::Result<String> {
        self.queue(DaemonCommand::Cancel)
    }

    /// Current session state: idle, recording or stopping.
    fn status(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .to_string()
    }

    /// One session event as a JSON line (`stream --json` schema), plus
    /// `{"type":"state","state":...}` on every state change.
    #[zbus(signal)]
    async fn event(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        json: &str,
    ) -> zbus::Result<()>;
}

impl ControllerInterface {
    fn queue(&self, command: DaemonCommand) -> zbus::fdo::Result<String> {
        self.commands
            .try_send(command)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok("queued".to_owned())
    }
}

#[derive(Debug)]
pub enum DaemonCommand {
    Toggle(String),
    Start(String),
    StartSession {
        mode: String,
        inject: bool,
    },
    Stop,
    Cancel,
    Key(crate::hotkey::KeyEvent),
    /// Re-read the config, reload the warm engine and hotkey (idle only).
    Reload(tokio::sync::oneshot::Sender<Result<EngineStatus, String>>),
}

/// What a push-to-talk key event asks of the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Start(String),
    Stop,
    Nothing,
}

/// One key, two gestures (as in Handy): hold it past the threshold and the
/// session stops on release; tap it and recording continues until the next
/// press. Any press while recording stops, whoever started the session.
pub struct HotkeyMachine {
    hold: std::time::Duration,
    mode: String,
    ctrl_mode: String,
    pressed_at: Option<std::time::Instant>,
}

impl HotkeyMachine {
    pub fn new(hold: std::time::Duration, mode: String, ctrl_mode: String) -> Self {
        Self {
            hold,
            mode,
            ctrl_mode,
            pressed_at: None,
        }
    }

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

struct ControllerInterface {
    commands: mpsc::Sender<DaemonCommand>,
    state: std::sync::Arc<std::sync::Mutex<&'static str>>,
}

/// Engine state reported to the console.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EngineStatus {
    pub loaded: bool,
    pub description: Option<String>,
    pub load_seconds: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HotkeyStatus {
    pub key: Option<String>,
    pub keyboards: usize,
    pub error: Option<String>,
}

/// Timing of the last completed session, measured from daemon events.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMetrics {
    pub backend: String,
    pub audio_seconds: f64,
    pub stop_to_final_ms: u64,
    pub chars: usize,
    pub ended_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonStatus {
    pub state: &'static str,
    pub mode: String,
    pub backend: String,
    pub fallback_backend: Option<String>,
    pub engine: EngineStatus,
    pub hotkey: HotkeyStatus,
    pub last_session: Option<SessionMetrics>,
}

const LOG_LINES: usize = 500;

/// What the daemon shares with the web console: the command channel, the
/// event broadcast, the hot-swappable warm engine, status and recent logs.
pub struct DaemonShared {
    pub commands: mpsc::Sender<DaemonCommand>,
    pub events: tokio::sync::broadcast::Sender<String>,
    pub config_path: std::path::PathBuf,
    pub default_mode: String,
    pub started: std::time::Instant,
    warm: std::sync::RwLock<Option<std::sync::Arc<crate::engine::WarmEngine>>>,
    status: std::sync::Mutex<DaemonStatus>,
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

    pub fn logs(&self) -> Vec<String> {
        let logs = self.logs.lock().unwrap_or_else(|error| error.into_inner());
        logs.iter().cloned().collect()
    }

    pub fn warm(&self) -> Option<std::sync::Arc<crate::engine::WarmEngine>> {
        self.warm
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn status(&self) -> DaemonStatus {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut DaemonStatus)) {
        change(
            &mut self
                .status
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
    }

    /// Path the console edits; `--config` or the XDG default.
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
/// its VRAM is free for the new model.
async fn load_engine(shared: &std::sync::Arc<DaemonShared>, config: &crate::config::AppConfig) {
    *shared
        .warm
        .write()
        .unwrap_or_else(|error| error.into_inner()) = None;
    let started = std::time::Instant::now();
    let warm_config = config.clone();
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
/// tasks to abort on reload.
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
    let hotkey_machine = |config: &crate::config::AppConfig| {
        HotkeyMachine::new(
            std::time::Duration::from_millis(config.hotkey.hold_threshold_ms),
            config.hotkey.mode.clone(),
            config.hotkey.ctrl_mode.clone(),
        )
    };
    let mut hotkey = hotkey_machine(&config);
    let mut hotkey_tasks = start_hotkey(&shared, &config);
    let mut paused_media: Option<tokio::task::JoinHandle<Vec<String>>> = None;
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

    // Forward broadcast events as D-Bus signals.
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
    let mut published = State::Idle;
    let mut session_mode = default_mode.clone();
    loop {
        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else { break };
                let mode = |requested: String| {
                    if requested.is_empty() { default_mode.clone() } else { requested }
                };
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
                        if gateway.state() != State::Idle {
                            let _ = reply.send(Err("a session is running; reload when idle".to_owned()));
                            continue;
                        }
                        match shared.config() {
                            Ok(fresh) => {
                                config = fresh;
                                for task in hotkey_tasks.drain(..) {
                                    task.abort();
                                }
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
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                if let Some(code) = gateway.poll().await {
                    shared.log(format!("session finished with exit code {code}"));
                }
            }
        }
        if gateway.state() != published {
            match (published, gateway.state()) {
                (State::Idle, State::Recording) => {
                    if config.audio.cues {
                        crate::media::play_cue(crate::media::Cue::Start);
                    }
                    if config.audio.pause_media {
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

const TOGGLE_SHORTCUT_ID: &str = "toggle";

/// Register a single global shortcut through the XDG Desktop Portal and
/// forward activations to the daemon command channel. Never fatal: compositors
/// without a GlobalShortcuts backend (or broken ones, e.g. GNOME 48) simply
/// skip portal binding and keep working through `dev.voiceless.Daemon`.
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
