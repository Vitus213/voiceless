//! `voiceless ctl`: thin client for the resident daemon. Commands are one
//! D-Bus call each, so a compositor binding returns in milliseconds; `watch`
//! prints every daemon event as a JSON line until interrupted.
//!
//! Everything talks to `dev.voiceless.Daemon` / `/dev/voiceless/Daemon` /
//! `dev.voiceless.Daemon.Controller` (constants in `daemon`) on the session
//! bus. The daemon acts before it answers: a mutating call replies
//! `started`, `stopping` or `cancelled`, or fails with the gateway's reason
//! (start while recording, toggle while stopping, stop while idle), which
//! `ctl` prints and turns into exit code 1. A missing daemon is turned into
//! an actionable message (`explain`).
//!
//! The second half of the module is `stream_via_daemon`: when `voiceless
//! stream` would need the local model a running daemon already holds,
//! `main` runs the session inside the daemon through `StartSession` and this
//! process relays the `Event` signal to stdout/stderr and Ctrl-C to
//! `Stop`/`Cancel`, reproducing the standalone `stream` exit codes.

#[cfg(target_os = "linux")]
use futures_util::StreamExt;

#[cfg(target_os = "linux")]
use crate::coordinator::ControlCommand;
#[cfg(target_os = "linux")]
use crate::daemon::{BUS_NAME, INTERFACE, OBJECT_PATH};

#[cfg(not(target_os = "linux"))]
#[path = "desktop/native_ctl.rs"]
mod native;
#[cfg(not(target_os = "linux"))]
use native::execute;
#[cfg(not(target_os = "linux"))]
pub use native::stream_via_daemon;

/// One `voiceless ctl` subcommand, mapped from the clap variants in `main`.
#[derive(Debug, Clone)]
pub enum CtlAction {
    /// `Toggle(mode)`: start when idle, stop when recording. Empty mode
    /// means the daemon default.
    Toggle(String),
    /// `Start(mode)`: hold-to-talk press.
    Start(String),
    /// `Stop()`: hold-to-talk release.
    Stop,
    /// `Cancel()`: abort without typing.
    Cancel,
    /// `Pause()`: refuse every start until `Resume()`; a session in progress is left alone.
    Pause,
    /// `Resume()`: accept starts again.
    Resume,
    /// `Repaste()`: deliver the last completed text to the current input target.
    Repaste,
    /// `Status()`: print `idle`, `recording` or `stopping`.
    Status,
    /// Subscribe to the `Event` signal and print each line until killed.
    Watch,
}

/// Run one action; exit code `0` on success, `1` with the error on stderr
/// (including "the daemon is not running"). `Watch` only returns on error.
pub async fn run(action: CtlAction) -> i32 {
    match execute(action).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("voiceless ctl: {error}");
            1
        }
    }
}

/// Open the session bus and perform `action`. Method calls take `(s)` for
/// the mode-carrying commands and `()` otherwise; `Status` deserializes the
/// `s` reply, the others discard the `"queued"` string.
#[cfg(target_os = "linux")]
async fn execute(action: CtlAction) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let call = |method: &'static str, mode: Option<String>| {
        let connection = connection.clone();
        async move {
            let reply = match mode {
                Some(mode) => {
                    connection
                        .call_method(
                            Some(BUS_NAME),
                            OBJECT_PATH,
                            Some(INTERFACE),
                            method,
                            &(mode,),
                        )
                        .await
                }
                None => {
                    connection
                        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, &())
                        .await
                }
            };
            reply.map_err(explain)
        }
    };
    match action {
        CtlAction::Toggle(mode) => drop(call("Toggle", Some(mode)).await?),
        CtlAction::Start(mode) => drop(call("Start", Some(mode)).await?),
        CtlAction::Stop => drop(call("Stop", None).await?),
        CtlAction::Cancel => drop(call("Cancel", None).await?),
        CtlAction::Repaste => drop(call("Repaste", None).await?),
        CtlAction::Pause | CtlAction::Resume => {
            let paused = matches!(action, CtlAction::Pause);
            let reply = connection
                .call_method(
                    Some(BUS_NAME),
                    OBJECT_PATH,
                    Some(INTERFACE),
                    "SetPaused",
                    &(paused,),
                )
                .await
                .map_err(explain)?;
            drop(reply);
        }
        CtlAction::Status => {
            let state: String = call("Status", None).await?.body().deserialize()?;
            println!("{state}");
        }
        CtlAction::Watch => {
            // Match on interface, member and path only (no sender): the
            // signal is delivered whichever connection owns the name, and
            // a daemon restart keeps the stream alive.
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(INTERFACE)?
                .member("Event")?
                .path(OBJECT_PATH)?
                .build();
            let mut stream = zbus::MessageStream::for_match_rule(rule, &connection, None).await?;
            // Each signal body is one already-serialized JSON line; it is
            // printed verbatim rather than re-encoded.
            while let Some(message) = stream.next().await {
                let line: String = message?.body().deserialize()?;
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// Replace the bus's `ServiceUnknown` error (nobody owns `BUS_NAME`) with a
/// message that says how to start the daemon; other errors pass through.
#[cfg(target_os = "linux")]
fn explain(error: zbus::Error) -> zbus::Error {
    match &error {
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
        {
            zbus::Error::Failure(format!(
                "the daemon is not running; start it with `systemctl --user start voiceless` or `voiceless daemon` ({error})"
            ))
        }
        _ => error,
    }
}

/// Delegate streaming to the runtime that owns this data directory.
/// With no matching daemon, `main` starts a standalone session.
#[cfg(target_os = "linux")]
pub async fn stream_via_daemon(
    mode: &str,
    inject: bool,
    json: bool,
    context: &crate::stream::DictationContext,
) -> Option<Result<i32, String>> {
    let connection = zbus::Connection::session().await.ok()?;
    let dbus = zbus::fdo::DBusProxy::new(&connection).await.ok()?;
    let name = zbus::names::BusName::try_from(BUS_NAME).ok()?;
    let owner = dbus.get_name_owner(name).await.ok()?;
    let proxy = match zbus::Proxy::new(&connection, owner.as_str(), OBJECT_PATH, INTERFACE).await {
        Ok(proxy) => proxy,
        Err(error) => return Some(Err(error.to_string())),
    };
    let directory: zbus::Result<Vec<u8>> = proxy.call("DataDirectory", &()).await;
    let scoped = match directory {
        Ok(directory) => {
            let local = crate::models::default_data_dir().canonicalize().ok()?;
            if directory != local.as_os_str().as_encoded_bytes() {
                return None;
            }
            true
        }
        Err(error) if unknown_method(&error) => {
            // Previous releases delegated only local streaming sessions.
            let config = crate::config::load_default_config().ok()?;
            if !matches!(
                config.asr.streaming_backend.as_str(),
                "local-offline" | "local-streaming"
            ) {
                return None;
            }
            false
        }
        Err(error) => return Some(Err(error.to_string())),
    };
    Some(
        relay(&proxy, mode, inject, json, scoped, context)
            .await
            .map_err(|error| error.to_string()),
    )
}

#[cfg(target_os = "linux")]
fn unknown_method(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod")
}

/// Subscribe before requesting a session. Use its D-Bus owner's unique
/// name for both events and controls, so a restart cannot claim this relay.
#[cfg(target_os = "linux")]
async fn relay(
    proxy: &zbus::Proxy<'_>,
    mode: &str,
    inject: bool,
    json: bool,
    scoped: bool,
    context: &crate::stream::DictationContext,
) -> zbus::Result<i32> {
    // Subscribe before starting so the session's first events are not lost.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(proxy.destination().clone())?
        .interface(INTERFACE)?
        .member("Event")?
        .path(OBJECT_PATH)?
        .build();
    let mut events = zbus::MessageStream::for_match_rule(rule, proxy.connection(), None).await?;
    let dbus = zbus::fdo::DBusProxy::new(proxy.connection()).await?;
    let mut owner_changes = dbus
        .receive_name_owner_changed_with_args(&[(0, proxy.destination().as_str())])
        .await?;
    let session_id = if !context.is_empty() {
        let site = context.site.as_deref().unwrap_or_default();
        let project = context.project.as_deref().unwrap_or_default();
        Some(proxy.call("StartSessionWithContext", &(mode, inject, site, project)).await.map_err(|error| {
            if unknown_method(&error) {
                zbus::Error::Failure("restart the daemon with a Voiceless version that supports session context".to_owned())
            } else { error }
        })?)
    } else if scoped {
        Some(proxy.call("StartSessionWithId", &(mode, inject)).await?)
    } else {
        let _: String = proxy.call("StartSession", &(mode, inject)).await.map_err(|error| {
            if unknown_method(&error) {
                zbus::Error::Failure("restart the daemon with a Voiceless version that supports stream delegation".to_owned())
            } else {
                error
            }
        })?;
        None
    };
    // Same signal mapping as standalone `stream`, forwarded as D-Bus calls.
    let mut control = crate::stream::control_receiver();
    let mut session = session_id
        .map(RelayedSession::for_session)
        .unwrap_or_default();
    let mut preview = crate::stream::TerminalPreview::default();
    loop {
        tokio::select! {
            _ = owner_changes.next() => {
                preview.clear();
                return Err(zbus::Error::Failure("the runtime disconnected during this session".to_owned()));
            }
            Some(command) = control.recv() => {
                let (scoped_method, legacy_method) = match command {
                    ControlCommand::Stop => ("StopSession", "Stop"),
                    ControlCommand::Cancel => ("CancelSession", "Cancel"),
                };
                if let Some(id) = session_id {
                    proxy.call::<_, _, ()>(scoped_method, &(id,)).await?;
                } else {
                    let _: String = proxy.call(legacy_method, &()).await?;
                }
            }
            message = events.next() => {
                // The stream only ends when the bus connection drops; the
                // daemon session may still be running then, so report it.
                let Some(message) = message else {
                    return Err(zbus::Error::Failure("the daemon event stream closed".to_owned()));
                };
                let line: String = message?.body().deserialize()?;
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if let Some(code) = session.observe(&event) {
                    preview.clear();
                    if let Some(error) = event["error"].as_str().filter(|_| !session.failed) {
                        eprintln!("Voiceless: {error}");
                    }
                    return Ok(code);
                }
                if session.accepts(&event) && RelayedSession::is_session_event(&event) {
                    print_event(&event, &line, json, &mut preview);
                }
            }
        }
    }
}

/// Exit code of a session followed through the daemon's event signal:
/// `0` success, `1` failure, `130` cancellation (the `stream` contract).
///
/// Pure state tracker (no I/O) so the mapping is unit-testable
/// (`tests/ctl_relay.rs`). It keys on the daemon's `state` events for the
/// session boundaries and on the coordinator's `error`/`cancelled`/
/// `completed` events for the outcome.
#[derive(Debug, Default)]
pub struct RelayedSession {
    session_id: Option<u64>,
    /// A `recording` state was seen: the session we started is running.
    /// An `idle` before that is a leftover from before our start.
    recording: bool,
    /// The coordinator's final `completed` event was seen; an idle without
    /// it means the session task died early and counts as failure.
    completed: bool,
    failed: bool,
    cancelled: bool,
}

impl RelayedSession {
    /// Track the ID returned by an acknowledged start request.
    pub fn for_session(session_id: u64) -> Self {
        Self {
            session_id: Some(session_id),
            ..Self::default()
        }
    }

    fn accepts(&self, event: &serde_json::Value) -> bool {
        self.session_id.is_none() || event["session_id"].as_u64() == self.session_id
    }

    /// Feed one event; returns the exit code once the daemon is idle again
    /// after this session started. Cancellation wins over failure, failure
    /// (or a missing `completed`) over success.
    pub fn observe(&mut self, event: &serde_json::Value) -> Option<i32> {
        if !self.accepts(event) {
            return None;
        }
        match event["type"].as_str() {
            Some("state") => match event["state"].as_str() {
                Some("recording") => self.recording = true,
                Some("idle") if self.recording || self.session_id.is_some() => {
                    if let Some(code) = event["exit_code"].as_i64() {
                        return Some(match code {
                            0 => 0,
                            130 => 130,
                            _ => 1,
                        });
                    }
                    return Some(if self.cancelled {
                        130
                    } else if self.failed || !self.completed {
                        1
                    } else {
                        0
                    });
                }
                _ => {}
            },
            Some("error") => self.failed = true,
            Some("cancelled") => self.cancelled = true,
            Some("completed") => self.completed = true,
            _ => {}
        }
        None
    }

    /// Events a standalone `stream` would print: not the daemon's state,
    /// log, metrics or level-meter lines.
    pub fn is_session_event(event: &serde_json::Value) -> bool {
        !matches!(
            event["type"].as_str(),
            Some("state" | "log" | "session" | "level" | "engine")
        )
    }
}

/// Print like the standalone `stream` stdout sink: with `--json` the raw
/// line; otherwise partials overwrite one stderr line (`\r`), the final
/// text goes to stdout, warnings and errors to stderr. Other event kinds
/// (`ready`, `finalized`, `cancelled`, `completed`) print nothing.
fn print_event(
    event: &serde_json::Value,
    line: &str,
    json: bool,
    preview: &mut crate::stream::TerminalPreview,
) {
    if json {
        println!("{line}");
        return;
    }
    let message = event["message"].as_str().unwrap_or_default();
    match event["type"].as_str() {
        Some("transcript") => {
            let text = event["transcript"]["authoritative_text"]
                .as_str()
                .unwrap_or_default();
            if event["transcript"]["is_final"] == true {
                preview.clear();
                println!("{text}");
            } else {
                preview.render(text);
            }
        }
        Some("warning") => {
            preview.clear();
            eprintln!("warning: {message}");
        }
        Some("error") => {
            preview.clear();
            eprintln!("{message}");
        }
        Some("cancelled" | "completed") => preview.clear(),
        _ => {}
    }
}
