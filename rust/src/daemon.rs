//! Headless session daemon: one session at a time, hotkey-style Activate
//! semantics (idle -> start, recording -> normal stop, stopping -> reject)
//! and explicit cancel. The gateway owns no capture or backend state; it
//! only forwards commands to the session runner.
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
/// tests inject scripted fixtures.
pub trait SessionRunner {
    fn start(
        &mut self,
        control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Recording,
    Stopping,
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

    /// Hotkey activation: idle starts a session, an active session stops
    /// normally, and a session already stopping rejects further activation
    /// so no concurrent session or duplicate injection can occur.
    pub fn activate(&mut self) -> Result<ActivateOutcome, String> {
        match self.state {
            State::Idle => {
                let (tx, rx) = mpsc::channel(8);
                let future = self.runner.start(rx);
                self.task = Some(tokio::spawn(future));
                self.control_tx = Some(tx);
                self.state = State::Recording;
                Ok(ActivateOutcome::Started)
            }
            State::Recording => {
                let tx = self
                    .control_tx
                    .clone()
                    .ok_or_else(|| "session without a control channel".to_owned())?;
                let _ = tx.try_send(ControlCommand::Stop);
                self.state = State::Stopping;
                Ok(ActivateOutcome::Stopping)
            }
            State::Stopping => Err("session is already stopping".to_owned()),
        }
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

/// Production runner: executes one `voiceless stream` session per activation
/// through the shared coordinator, controlled exclusively via the gateway
/// channel.
pub struct StreamRunner {
    pub options: crate::stream::StreamOptions,
}

impl SessionRunner for StreamRunner {
    fn start(
        &mut self,
        control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>> {
        let options = self.options.clone();
        Box::pin(async move {
            crate::stream::run_with_control(options, control)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

pub const BUS_NAME: &str = "dev.voiceless.Daemon";
pub const OBJECT_PATH: &str = "/dev/voiceless/Daemon";

/// D-Bus control interface. Activate maps to the gateway hotkey semantics;
/// Cancel force-stops.
#[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
impl ControllerInterface {
    fn activate(&self) -> zbus::fdo::Result<String> {
        self.commands
            .try_send(DaemonCommand::Activate)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok("queued".to_owned())
    }

    fn cancel(&self) -> zbus::fdo::Result<String> {
        self.commands
            .try_send(DaemonCommand::Cancel)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok("queued".to_owned())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DaemonCommand {
    Activate,
    Cancel,
}

struct ControllerInterface {
    commands: mpsc::Sender<DaemonCommand>,
}

/// Serve the daemon until the task ends: owns the gateway, the D-Bus
/// control bus and the reaping loop.
pub async fn serve(options: crate::stream::StreamOptions) -> Result<(), String> {
    let gateway = Gateway::new(StreamRunner { options });
    serve_with_gateway(gateway).await
}

async fn serve_with_gateway<R>(mut gateway: Gateway<R>) -> Result<(), String>
where
    R: SessionRunner,
{
    let (command_tx, mut command_rx) = mpsc::channel::<DaemonCommand>(8);

    let connection = zbus::Connection::session()
        .await
        .map_err(|error| format!("session bus unavailable: {error}"))?;
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            ControllerInterface {
                commands: command_tx.clone(),
            },
        )
        .await
        .map_err(|error| format!("cannot export controller object: {error}"))?;
    connection
        .request_name(BUS_NAME)
        .await
        .map_err(|error| format!("cannot claim bus name {BUS_NAME}: {error}"))?;

    // Best-effort portal global shortcut; unavailable portals fall back to
    // compositor bindings over the control bus above.
    tokio::spawn(portal_shortcut_listener(command_tx.clone()));

    println!("Voiceless daemon listening on {BUS_NAME}");
    loop {
        tokio::select! {
            command = command_rx.recv() => match command {
                Some(DaemonCommand::Activate) => match gateway.activate() {
                    Ok(outcome) => println!("activate: {outcome:?}"),
                    Err(message) => eprintln!("Voiceless: {message}"),
                },
                Some(DaemonCommand::Cancel) => gateway.cancel().await,
                None => break,
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                if let Some(code) = gateway.poll().await {
                    println!("session finished with exit code {code}");
                }
            }
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
            let _ = commands.send(DaemonCommand::Activate).await;
        }
    }
}
