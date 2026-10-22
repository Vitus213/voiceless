//! `voiceless ctl`: thin client for the resident daemon. Commands are one
//! D-Bus call each, so a compositor binding returns in milliseconds; `watch`
//! prints every daemon event as a JSON line until interrupted.
//!
//! Everything talks to `dev.voiceless.Daemon` / `/dev/voiceless/Daemon` /
//! `dev.voiceless.Daemon.Controller` (constants in `daemon`) on the session
//! bus. The daemon answers mutating calls with `"queued"` before acting, so
//! `ctl toggle|start|stop|cancel` exiting 0 means the command was accepted
//! into the daemon's queue, not that a session started or stopped; use
//! `status`, `watch` or the daemon log for the outcome. A missing daemon is
//! turned into an actionable message (`explain`).
//!
//! The second half of the module is `stream_via_daemon`: when `voiceless
//! stream` would need the local model a running daemon already holds,
//! `main` runs the session inside the daemon through `StartSession` and this
//! process relays the `Event` signal to stdout/stderr and Ctrl-C to
//! `Stop`/`Cancel`, reproducing the standalone `stream` exit codes.

use futures_util::StreamExt;

use crate::daemon::{BUS_NAME, INTERFACE, OBJECT_PATH};

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

/// `voiceless stream` while the daemon runs: the daemon already holds the
/// local model and a second copy would not fit in VRAM, so the session runs
/// there and this process relays its events and the Ctrl-C stop/cancel.
/// `None` when no daemon owns the bus name (or the session bus itself is
/// unreachable), in which case `main` runs `stream` standalone; `Some` is
/// the relayed session's exit code or a relay error (daemon busy, outdated
/// daemon, bus failure), which `main` reports as exit 1.
pub async fn stream_via_daemon(
    mode: &str,
    inject: bool,
    json: bool,
) -> Option<Result<i32, String>> {
    let connection = zbus::Connection::session().await.ok()?;
    let dbus = zbus::fdo::DBusProxy::new(&connection).await.ok()?;
    let name = zbus::names::BusName::try_from(BUS_NAME).ok()?;
    if !dbus.name_has_owner(name).await.ok()? {
        return None;
    }
    Some(
        relay(&connection, mode, inject, json)
            .await
            .map_err(|error| error.to_string()),
    )
}

/// Run one session inside the daemon and follow it to its end.
///
/// Steps: refuse unless the daemon is idle; subscribe to `Event`; call
/// `StartSession(mode, inject)`; then loop over the local SIGINT/SIGTERM
/// control channel (first Ctrl-C -> `Stop`, second or SIGTERM -> `Cancel`)
/// and the event stream, printing session events like standalone `stream`
/// and returning once `RelayedSession` sees the daemon idle again. The
/// idle check and the start are two calls, so another client could start a
/// session in between; the daemon would then reject this start (logged
/// only) and the relay would follow that other session instead.
async fn relay(
    connection: &zbus::Connection,
    mode: &str,
    inject: bool,
    json: bool,
) -> zbus::Result<i32> {
    let state: String = connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), "Status", &())
        .await?
        .body()
        .deserialize()?;
    if state != "idle" {
        return Err(zbus::Error::Failure(format!(
            "the daemon is busy ({state}); try again when it is idle"
        )));
    }
    // Subscribe before starting so the session's first events are not lost.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(INTERFACE)?
        .member("Event")?
        .path(OBJECT_PATH)?
        .build();
    let mut events = zbus::MessageStream::for_match_rule(rule, connection, None).await?;
    connection
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(INTERFACE),
            "StartSession",
            &(mode, inject),
        )
        .await
        .map_err(|error| match &error {
            // `StartSession` was added with stream delegation; an older
            // daemon still answers the other methods but not this one.
            zbus::Error::MethodError(name, _, _)
                if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
            {
                zbus::Error::Failure(format!(
                    "the running daemon predates `stream` delegation; upgrade voiceless and restart it (`systemctl --user restart voiceless`) ({error})"
                ))
            }
            _ => error,
        })?;
    // Same signal mapping as standalone `stream`, forwarded as D-Bus calls.
    let mut control = crate::stream::control_receiver();
    let mut session = RelayedSession::default();
    loop {
        tokio::select! {
            Some(command) = control.recv() => {
                let method = match command {
                    crate::coordinator::ControlCommand::Stop => "Stop",
                    crate::coordinator::ControlCommand::Cancel => "Cancel",
                };
                connection
                    .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, &())
                    .await?;
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
                    return Ok(code);
                }
                if RelayedSession::is_session_event(&event) {
                    print_event(&event, &line, json);
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
    /// Feed one event; returns the exit code once the daemon is idle again
    /// after this session started. Cancellation wins over failure, failure
    /// (or a missing `completed`) over success.
    pub fn observe(&mut self, event: &serde_json::Value) -> Option<i32> {
        match event["type"].as_str() {
            Some("state") => match event["state"].as_str() {
                Some("recording") => self.recording = true,
                Some("idle") if self.recording => {
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
            Some("state" | "log" | "session" | "level")
        )
    }
}

/// Print like the standalone `stream` stdout sink: with `--json` the raw
/// line; otherwise partials overwrite one stderr line (`\r`), the final
/// text goes to stdout, warnings and errors to stderr. Other event kinds
/// (`ready`, `finalized`, `cancelled`, `completed`) print nothing.
fn print_event(event: &serde_json::Value, line: &str, json: bool) {
    use std::io::Write;
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
                println!("{text}");
            } else {
                eprint!("\r{text}");
                std::io::stderr().flush().ok();
            }
        }
        Some("warning") => eprintln!("warning: {message}"),
        Some("error") => eprintln!("{message}"),
        _ => {}
    }
}
