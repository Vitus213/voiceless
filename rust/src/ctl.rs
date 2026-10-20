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
