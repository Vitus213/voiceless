//! Client of the Voiceless Fcitx5 bridge module (`fcitx5-bridge/`): commits
//! the final text and shows the live partial as preedit in the focused
//! input field.
//!
//! One actor task owns one D-Bus connection and applies requests strictly
//! in order, so a stale preedit can never land after the commit that
//! replaced it; consecutive preedit updates collapse to the newest.
//!
//! The bridge lives on Fcitx5's own bus name (`org.fcitx.Fcitx5`) at
//! `/voiceless`, interface `dev.voiceless.Fcitx5Bridge1`, with four methods
//! that all reply `(b ok, s detail)`: `CommitString(s)`, `SetPreedit(s)`,
//! `ClearPreedit()` and `Status()`. `detail` is the focused program (e.g.
//! `"firefox (wayland)"`) on success and the reason otherwise ("no focused
//! input context", "empty text"). `CommitString` clears the bridge's own
//! preedit before committing, which is why one ordered channel is enough.
//!
//! Wired by `stream::run_with_env` when `inject.prefer = "fcitx5"` and
//! injection is on: the `ConfiguredInjector` commits through the client and,
//! with `inject.preedit = true`, `PreeditSink` wraps the event sink so
//! partial transcripts appear in the original field. The doctor command
//! can query the current context read-only with `Status`.

use std::io;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::coordinator::{EventSink, OutputEvent};

/// Fcitx5's well-known session bus name; the bridge addon registers under
/// it.
pub const SERVICE: &str = "org.fcitx.Fcitx5";
/// Object path of the bridge.
pub const PATH: &str = "/voiceless";
/// Interface of the bridge.
pub const INTERFACE: &str = "dev.voiceless.Fcitx5Bridge1";

/// Upper bound for connecting plus one method call; keeps a hung Fcitx5
/// from stalling the final injection indefinitely.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a failed bridge request could have reached an input field.
#[derive(Clone, Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Target(String),
    #[error("{0}")]
    Unconfirmed(String),
}

/// Bridge reply. A false verdict is an explicit refusal by the bridge.
pub type Reply = Result<(bool, String), BridgeError>;

/// Message from a client handle to the actor.
enum Request {
    /// `SetPreedit(text)`; superseded by any later request in the same
    /// batch.
    Preedit(String),
    /// `ClearPreedit()`.
    Clear,
    /// `CommitString(text)`, answered through the oneshot.
    Commit(String, oneshot::Sender<Reply>),
    /// `Status()`, answered through the oneshot: which program has the input focus.
    Status(oneshot::Sender<Reply>),
}

/// Cheap, cloneable handle to the bridge actor. All clones share one
/// request queue, so a `PreeditSink` and the injector holding clones of the
/// same client are strictly ordered with respect to each other.
#[derive(Clone)]
pub struct Fcitx5Client {
    requests: mpsc::UnboundedSender<Request>,
}

impl Fcitx5Client {
    /// Start the actor on the current Tokio runtime. The bus connection is
    /// opened lazily, so constructing a client never fails. The actor stops
    /// once every handle is dropped.
    pub fn spawn() -> Self {
        Self::spawn_client(false)
    }

    /// Bind preview and commit to the input context focused at session start.
    pub(crate) fn spawn_for_dictation() -> Self {
        Self::spawn_client(true)
    }

    fn spawn_client(bind_target: bool) -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run(receiver, bind_target));
        Self { requests }
    }

    /// Show `text` as preedit (best effort, never blocks). Only the newest
    /// of several queued previews is sent to Fcitx5.
    pub fn preedit(&self, text: &str) {
        let _ = self.requests.send(Request::Preedit(text.to_owned()));
    }

    /// Remove this bridge's preedit (best effort, never blocks).
    pub fn clear(&self) {
        let _ = self.requests.send(Request::Clear);
    }

    /// Commit `text`, after every earlier preedit request. Waits for the
    /// bridge reply (bounded by `CALL_TIMEOUT`); `Err` if the actor is gone.
    pub async fn commit(&self, text: &str) -> Reply {
        let (reply, receiver) = oneshot::channel();
        self.requests
            .send(Request::Commit(text.to_owned(), reply))
            .map_err(|_| BridgeError::Unavailable("fcitx5 client stopped".to_owned()))?;
        receiver
            .await
            .map_err(|_| BridgeError::Unconfirmed("fcitx5 client stopped".to_owned()))?
    }

    /// Which program has the input focus, as the bridge names it
    /// (`"name (frontend)"`); `Ok((false, _))` when no input context has
    /// it. Waits for the bridge reply (bounded by `CALL_TIMEOUT`); `Err`
    /// if the actor is gone.
    pub async fn status(&self) -> Reply {
        let (reply, receiver) = oneshot::channel();
        self.requests
            .send(Request::Status(reply))
            .map_err(|_| BridgeError::Unavailable("fcitx5 client stopped".to_owned()))?;
        receiver
            .await
            .map_err(|_| BridgeError::Unavailable("fcitx5 client stopped".to_owned()))?
    }
}

/// The actor: drains the queue in batches and performs the D-Bus calls one
/// at a time. Within a batch every preedit except the last is dropped (the
/// user only ever sees the newest partial, and a commit queued behind it
/// would replace it anyway); clears and commits are always executed, in
/// order. Ends when the last `Fcitx5Client` is dropped.
async fn run(mut receiver: mpsc::UnboundedReceiver<Request>, bind_target: bool) {
    let mut connection: Option<zbus::Connection> = None;
    let target = if bind_target {
        Some(match call(&mut connection, "FocusedContext", &()).await {
            Ok((true, id)) => Ok(id),
            Ok((false, detail)) | Err(BridgeError::Unconfirmed(detail)) => {
                Err(BridgeError::Target(detail))
            }
            Err(error) => Err(error),
        })
    } else {
        None
    };
    while let Some(first) = receiver.recv().await {
        // Collect everything already queued so the collapse below can see
        // all preedits that arrived while the previous call was in flight.
        let mut batch = vec![first];
        while let Ok(next) = receiver.try_recv() {
            batch.push(next);
        }
        let last = batch.len() - 1;
        for (index, request) in batch.into_iter().enumerate() {
            match request {
                // A newer request in the same batch supersedes this preview.
                Request::Preedit(_) if index < last => {}
                Request::Preedit(text) => match &target {
                    Some(Ok(id)) => {
                        let _ = call(&mut connection, "PreeditToContext", &(id, &text)).await;
                    }
                    Some(Err(_)) => {}
                    None => {
                        let _ = call(&mut connection, "SetPreedit", &(&text,)).await;
                    }
                },
                Request::Clear => {
                    let _ = call(&mut connection, "ClearPreedit", &()).await;
                }
                Request::Commit(text, reply) => {
                    let result = match &target {
                        Some(Ok(id)) => call(&mut connection, "CommitToContext", &(id, &text))
                            .await
                            .and_then(|(ok, detail)| {
                                if ok {
                                    Ok((true, detail))
                                } else {
                                    Err(BridgeError::Target(detail))
                                }
                            }),
                        Some(Err(error)) => Err(error.clone()),
                        None => call(&mut connection, "CommitString", &(&text,)).await,
                    };
                    let _ = reply.send(result);
                }
                Request::Status(reply) => {
                    let _ = reply.send(call(&mut connection, "Status", &()).await);
                }
            }
        }
    }
}

/// One bridge method call on the cached connection (opened on first use),
/// with the `(b, s)` reply decoded. Any error drops the cached connection so
/// the next call reconnects; a timeout is reported as its own message.
async fn call<B>(connection: &mut Option<zbus::Connection>, method: &str, body: &B) -> Reply
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let result = tokio::time::timeout(CALL_TIMEOUT, async {
        if connection.is_none() {
            *connection = Some(
                zbus::Connection::session()
                    .await
                    .map_err(|error| BridgeError::Unavailable(error.to_string()))?,
            );
        }
        let bus = connection.as_ref().expect("connected above");
        let reply = bus
            .call_method(Some(SERVICE), PATH, Some(INTERFACE), method, body)
            .await
            .map_err(|error| match &error {
                zbus::Error::MethodError(name, _, _)
                    if matches!(
                        name.as_str(),
                        "org.freedesktop.DBus.Error.ServiceUnknown"
                            | "org.freedesktop.DBus.Error.NameHasNoOwner"
                            | "org.freedesktop.DBus.Error.UnknownObject"
                            | "org.freedesktop.DBus.Error.UnknownInterface"
                            | "org.freedesktop.DBus.Error.UnknownMethod"
                    ) =>
                {
                    BridgeError::Unavailable(error.to_string())
                }
                _ => BridgeError::Unconfirmed(error.to_string()),
            })?;
        reply
            .body()
            .deserialize::<(bool, String)>()
            .map_err(|error| BridgeError::Unconfirmed(error.to_string()))
    })
    .await;
    match result {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => {
            *connection = None;
            Err(error)
        }
        Err(_) => {
            *connection = None;
            Err(BridgeError::Unconfirmed(
                "fcitx5 bridge timed out".to_owned(),
            ))
        }
    }
}

/// Event sink decorator mirroring the live transcript into the focused
/// field as preedit, and clearing it whenever a session ends without a
/// commit replacing it.
///
/// Every event is passed through to `inner` unchanged; the preedit calls are
/// fire-and-forget on the shared client, so the sink never blocks the
/// coordinator. Installed by `stream` only when `inject.prefer = "fcitx5"`,
/// injection is on and `inject.preedit = true`.
pub struct PreeditSink<S> {
    inner: S,
    client: Fcitx5Client,
    /// A preedit was sent and not yet cleared; avoids a needless
    /// `ClearPreedit` for sessions that never produced a partial.
    shown: bool,
}

impl<S> PreeditSink<S> {
    /// Wrap `inner`; `client` must be the same handle the injector commits
    /// through, or the ordering guarantee does not hold.
    pub fn new(inner: S, client: Fcitx5Client) -> Self {
        Self {
            inner,
            client,
            shown: false,
        }
    }
}

impl<S: EventSink> EventSink for PreeditSink<S> {
    /// Mirror live transcripts as preedit. Clear it when the recognizer
    /// retracts the preview or the session ends without a commit.
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        match &event {
            OutputEvent::Transcript(snapshot) if !snapshot.is_final => {
                if snapshot.authoritative_text.is_empty() {
                    self.clear();
                } else {
                    self.client.preedit(&snapshot.authoritative_text);
                    self.shown = true;
                }
            }
            // The commit (queued after every preedit) replaces the preedit;
            // a session that ends without one clears it.
            OutputEvent::Finalized { injection } if injection.is_none() => self.clear(),
            OutputEvent::Error(_) | OutputEvent::Cancelled | OutputEvent::Completed => self.clear(),
            _ => {}
        }
        self.inner.emit(event)
    }

    fn retain_final_text(&mut self, text: &str) {
        self.inner.retain_final_text(text);
    }
}

impl<S> PreeditSink<S> {
    /// Queue a `ClearPreedit` once per shown preedit.
    fn clear(&mut self) {
        if std::mem::take(&mut self.shown) {
            self.client.clear();
        }
    }
}
