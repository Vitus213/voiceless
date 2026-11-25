use serde_json::{json, Value};

use super::{print_event, CtlAction, RelayedSession};
use crate::coordinator::ControlCommand;
use crate::desktop::runtime::{RuntimeClient, RuntimeError};
use crate::stream::{DictationContext, TerminalPreview};

pub(super) async fn execute(action: CtlAction) -> Result<(), RuntimeError> {
    let client = RuntimeClient::local();
    let endpoint = client.endpoint().await?.ok_or_else(|| {
        RuntimeError::Local(
            "the daemon is not running; start it with `voiceless daemon` or open the desktop app"
                .to_owned(),
        )
    })?;
    let (path, body) = match action {
        CtlAction::Toggle(mode) => ("api/session/toggle", json!({ "mode": mode })),
        CtlAction::Start(mode) => ("api/session/start", json!({ "mode": mode })),
        CtlAction::Stop => ("api/session/stop", json!({})),
        CtlAction::Cancel => ("api/session/cancel", json!({})),
        CtlAction::Repaste => ("api/session/repaste", json!({})),
        CtlAction::Pause | CtlAction::Resume => (
            "api/session/pause",
            json!({ "paused": matches!(action, CtlAction::Pause) }),
        ),
        CtlAction::Status => {
            let status = endpoint.request("api/status", None).await?;
            let state = status["state"].as_str().ok_or_else(|| {
                RuntimeError::Local("the runtime did not report a state".to_owned())
            })?;
            println!("{state}");
            return Ok(());
        }
        CtlAction::Watch => {
            let mut events = endpoint.events().await?;
            while let Some(line) = events.recv().await {
                println!("{}", line?);
            }
            return Err(RuntimeError::Local(
                "the runtime event stream closed".to_owned(),
            ));
        }
    };
    endpoint.request(path, Some(body)).await?;
    Ok(())
}

/// Run streaming inside the resident runtime when one owns the data directory.
pub async fn stream_via_daemon(
    mode: &str,
    inject: bool,
    json: bool,
    context: &DictationContext,
) -> Option<Result<i32, String>> {
    let endpoint = match RuntimeClient::local().endpoint().await {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => return None,
        Err(error) => return Some(Err(error.to_string())),
    };
    let relay = async {
        let mut events = endpoint.events().await.map_err(|error| error.to_string())?;
        let mut control = crate::stream::control_receiver();
        let result = endpoint
            .request(
                "api/session/start",
                Some(json!({
                    "mode": mode, "inject": inject, "runtime_id": endpoint.id(),
                    "site": context.site, "project": context.project,
                })),
            )
            .await
            .map_err(|error| error.to_string())?;
        let id = result["session_id"]
            .as_u64()
            .ok_or("the runtime did not acknowledge a session")?;
        let mut session = RelayedSession::for_session(id);
        let mut preview = TerminalPreview::default();
        loop {
            tokio::select! {
                Some(command) = control.recv() => {
                    let path = match command {
                        ControlCommand::Stop => "api/session/stop",
                        ControlCommand::Cancel => "api/session/cancel",
                    };
                    endpoint.request(path, Some(json!({ "session_id": id, "runtime_id": endpoint.id() })))
                        .await.map_err(|error| error.to_string())?;
                }
                line = events.recv() => {
                    let line = line.ok_or("the runtime disconnected during this session")?
                        .map_err(|error| error.to_string())?;
                    let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                    if event["runtime_id"].as_str() != Some(endpoint.id()) { continue; }
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
    };
    Some(relay.await)
}
