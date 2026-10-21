//! `voiceless ctl`: thin client for the resident daemon. Commands are one
//! D-Bus call each, so a compositor binding returns in milliseconds; `watch`
//! prints every daemon event as a JSON line until interrupted.

use futures_util::StreamExt;

use crate::daemon::{BUS_NAME, INTERFACE, OBJECT_PATH};

#[derive(Debug, Clone)]
pub enum CtlAction {
    Toggle(String),
    Start(String),
    Stop,
    Cancel,
    Status,
    Watch,
}

pub async fn run(action: CtlAction) -> i32 {
    match execute(action).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("voiceless ctl: {error}");
            1
        }
    }
}

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
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(INTERFACE)?
                .member("Event")?
                .path(OBJECT_PATH)?
                .build();
            let mut stream = zbus::MessageStream::for_match_rule(rule, &connection, None).await?;
            while let Some(message) = stream.next().await {
                let line: String = message?.body().deserialize()?;
                println!("{line}");
            }
        }
    }
    Ok(())
}

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
/// `None` when no daemon owns the bus name.
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
            zbus::Error::MethodError(name, _, _)
                if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
            {
                zbus::Error::Failure(format!(
                    "the running daemon predates `stream` delegation; upgrade voiceless and restart it (`systemctl --user restart voiceless`) ({error})"
                ))
            }
            _ => error,
        })?;
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
#[derive(Debug, Default)]
pub struct RelayedSession {
    recording: bool,
    completed: bool,
    failed: bool,
    cancelled: bool,
}

impl RelayedSession {
    /// Feed one event; returns the exit code once the daemon is idle again
    /// after this session started.
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

/// Print like the standalone `stream` stdout sink.
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
