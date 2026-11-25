//! Native clients discover the console through the locked data directory.
//! A descriptor names only a loopback socket and its current runtime token.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::runtime::RuntimeError;

const DESCRIPTOR: &str = "console-endpoint.json";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_EVENT_BYTES: u64 = 4 * 1024 * 1024;

fn transport_error(error: impl ToString) -> RuntimeError {
    RuntimeError::Local(error.to_string())
}

#[derive(Clone)]
/// Discover and control the runtime of the current user.
pub struct RuntimeClient {
    directory: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Endpoint {
    address: SocketAddr,
    runtime_id: String,
    token: String,
    console: bool,
}

impl Endpoint {
    fn url(&self) -> String {
        format!("http://{}/", self.address)
    }

    /// The instance identity used for session commands and events.
    pub(crate) fn id(&self) -> &str {
        &self.runtime_id
    }

    pub(crate) async fn request(
        self: &std::sync::Arc<Self>,
        suffix: &'static str,
        body: Option<Value>,
    ) -> Result<Value, RuntimeError> {
        let endpoint = self.clone();
        tokio::task::spawn_blocking(move || request(&endpoint, suffix, body))
            .await
            .map_err(transport_error)?
    }

    /// Subscribe before starting the session. The server sends a snapshot
    /// first and an SSE heartbeat every 15 seconds.
    pub(crate) async fn events(
        self: &std::sync::Arc<Self>,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<String, RuntimeError>>, RuntimeError> {
        let (ready, subscribed) = tokio::sync::oneshot::channel();
        let (send, receive) = tokio::sync::mpsc::channel(64);
        let endpoint = self.clone();
        tokio::task::spawn_blocking(move || {
            let agent = agent().timeout_read(EVENT_TIMEOUT).build();
            let response = agent
                .get(&format!("{}api/events", endpoint.url()))
                .set("X-Voiceless-Token", &endpoint.token)
                .call()
                .map_err(transport_error);
            let response = match response {
                Ok(response) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    response
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if let Err(error) = forward_events(BufReader::new(response.into_reader()), &send) {
                let _ = send.blocking_send(Err(transport_error(error)));
            }
        });
        subscribed.await.map_err(transport_error)??;
        Ok(receive)
    }
}

impl RuntimeClient {
    pub(crate) fn local() -> Self {
        Self {
            directory: crate::models::default_data_dir(),
        }
    }

    pub(crate) async fn endpoint(&self) -> Result<Option<std::sync::Arc<Endpoint>>, RuntimeError> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            endpoint(&directory).map(|value| value.map(std::sync::Arc::new))
        })
        .await
        .map_err(transport_error)?
    }

    pub(crate) async fn console_url_in(
        &self,
        directory: &Path,
    ) -> Result<Option<String>, RuntimeError> {
        let directory = directory.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let Some(endpoint) = endpoint(&directory)? else {
                return Ok(None);
            };
            match request(&endpoint, "api/status", None) {
                Ok(status) if status["runtime_id"] == endpoint.runtime_id => {
                    Ok(Some(if endpoint.console {
                        endpoint.url()
                    } else {
                        String::new()
                    }))
                }
                // The runtime can already hold its lock while warming its
                // model and replacing an earlier process's descriptor.
                Ok(_) | Err(_) => Ok(None),
            }
        })
        .await
        .map_err(transport_error)?
    }

    async fn status(&self) -> Result<Option<Value>, RuntimeError> {
        match self.endpoint().await? {
            Some(endpoint) => endpoint.request("api/status", None).await.map(Some),
            None => Ok(None),
        }
    }
}

pub(crate) fn publish(
    directory: &Path,
    address: SocketAddr,
    runtime_id: String,
    token: &str,
    console: bool,
) -> Result<(), String> {
    let descriptor = Endpoint {
        address,
        runtime_id,
        token: token.to_owned(),
        console,
    };
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, &descriptor).map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    file.persist(directory.join(DESCRIPTOR))
        .map_err(|error| error.error.to_string())?;
    Ok(())
}

fn endpoint(directory: &Path) -> Result<Option<Endpoint>, RuntimeError> {
    match crate::runtime::lock_runtime(directory) {
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(transport_error(error)),
    }
    let file = match File::open(directory.join(DESCRIPTOR)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(transport_error(error)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .map_err(transport_error)?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(transport_error(
                "the runtime endpoint descriptor must be private",
            ));
        }
    }
    let endpoint: Endpoint = serde_json::from_reader(file.take(2048)).map_err(transport_error)?;
    if !endpoint.address.ip().is_loopback() || endpoint.address.port() == 0 {
        return Err(transport_error(
            "the runtime endpoint must be a loopback address",
        ));
    }
    Ok(Some(endpoint))
}

fn agent() -> ureq::AgentBuilder {
    ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .redirects(0)
        .timeout_connect(REQUEST_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
}

fn forward_events(
    mut input: impl BufRead,
    send: &tokio::sync::mpsc::Sender<Result<String, RuntimeError>>,
) -> io::Result<()> {
    let mut line = String::new();
    while !send.is_closed() {
        line.clear();
        let bytes = (&mut input).take(MAX_EVENT_BYTES).read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete or oversized runtime event",
            ));
        }
        if let Some(event) = line.trim_end_matches(['\r', '\n']).strip_prefix("data:") {
            if send
                .blocking_send(Ok(event.strip_prefix(' ').unwrap_or(event).to_owned()))
                .is_err()
            {
                break;
            }
        }
    }
    Ok(())
}

fn request(endpoint: &Endpoint, suffix: &str, body: Option<Value>) -> Result<Value, RuntimeError> {
    let agent = agent().timeout(REQUEST_TIMEOUT).build();
    let url = format!("{}{suffix}", endpoint.url());
    let result = match body {
        Some(body) => agent
            .post(&url)
            .set("X-Voiceless-Token", &endpoint.token)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string()),
        None => agent
            .get(&url)
            .set("X-Voiceless-Token", &endpoint.token)
            .call(),
    };
    match result {
        Ok(response) => serde_json::from_reader(response.into_reader().take(4 * 1024 * 1024))
            .map_err(transport_error),
        Err(ureq::Error::Status(status, response)) => {
            let body: Value =
                serde_json::from_reader(response.into_reader().take(64 * 1024)).unwrap_or_default();
            Err(transport_error(
                body.get("error")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("HTTP {status}"), str::to_owned),
            ))
        }
        Err(error) => Err(transport_error(error)),
    }
}

/// The live console URL of the runtime for this user.
pub async fn console_url(client: &RuntimeClient) -> Result<Option<String>, RuntimeError> {
    client.console_url_in(&client.directory).await
}

/// The runtime's last published state, when it is running.
pub async fn runtime_state(client: &RuntimeClient) -> Result<Option<String>, RuntimeError> {
    Ok(client
        .status()
        .await?
        .and_then(|status| status["state"].as_str().map(str::to_owned)))
}

/// Whether the running daemon is accepting new dictation sessions.
pub async fn paused(client: &RuntimeClient) -> Result<Option<bool>, RuntimeError> {
    Ok(client
        .status()
        .await?
        .and_then(|status| status["paused"].as_bool()))
}

/// Suspend or resume dictation. The active session can finish.
pub async fn set_paused(client: &RuntimeClient, paused: bool) -> Result<(), RuntimeError> {
    dispatch(client, "api/session/pause", json!({ "paused": paused })).await
}

/// Deliver the last completed text at the current target.
pub async fn repaste(client: &RuntimeClient) -> Result<(), RuntimeError> {
    dispatch(client, "api/session/repaste", json!({})).await
}

async fn dispatch(
    client: &RuntimeClient,
    action: &'static str,
    body: Value,
) -> Result<(), RuntimeError> {
    let endpoint = client
        .endpoint()
        .await?
        .ok_or_else(|| transport_error("the Voiceless runtime is not running"))?;
    endpoint.request(action, Some(body)).await.map(|_| ())
}
