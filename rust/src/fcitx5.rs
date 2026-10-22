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
//! partial transcripts appear in the field. `doctor` calls `Status` through
//! the same constants. Failures (no Fcitx5, no bridge addon, no focused
//! field, 2 s timeout) are reported to the injector, which can fall back to
//! the clipboard method.

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

/// Bridge reply: (ok, target program or failure reason). The outer `Err`
/// is a transport failure (no bus, no bridge, timeout, actor gone); the
/// inner `false` is the bridge refusing (no focused field, empty text).
pub type Reply = Result<(bool, String), String>;

/// Message from a client handle to the actor.
enum Request {
    /// `SetPreedit(text)`; superseded by any later request in the same
    /// batch.
    Preedit(String),
    /// `ClearPreedit()`.
    Clear,
    /// `CommitString(text)`, answered through the oneshot.
    Commit(String, oneshot::Sender<Reply>),
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
        let (requests, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run(receiver));
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
            .map_err(|_| "fcitx5 client stopped".to_owned())?;
        receiver
            .await
            .map_err(|_| "fcitx5 client stopped".to_owned())?
    }
}

/// The actor: drains the queue in batches and performs the D-Bus calls one
/// at a time. Within a batch every preedit except the last is dropped (the
/// user only ever sees the newest partial, and a commit queued behind it
/// would replace it anyway); clears and commits are always executed, in
/// order. Ends when the last `Fcitx5Client` is dropped.
async fn run(mut receiver: mpsc::UnboundedReceiver<Request>) {
    let mut connection: Option<zbus::Connection> = None;
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
                Request::Preedit(text) => {
                    let _ = call(&mut connection, "SetPreedit", Some(&text)).await;
                }
                Request::Clear => {
                    let _ = call(&mut connection, "ClearPreedit", None).await;
                }
                Request::Commit(text, reply) => {
                    let _ = reply.send(call(&mut connection, "CommitString", Some(&text)).await);
                }
            }
        }
    }
}

/// One bridge method call on the cached connection (opened on first use),
/// with the `(b, s)` reply decoded. Any error drops the cached connection so
/// the next call reconnects; a timeout is reported as its own message.
async fn call(
    connection: &mut Option<zbus::Connection>,
    method: &str,
    text: Option<&str>,
) -> Reply {
    let result = tokio::time::timeout(CALL_TIMEOUT, async {
        if connection.is_none() {
            *connection = Some(zbus::Connection::session().await?);
        }
        let bus = connection.as_ref().expect("connected above");
        let reply = match text {
            Some(text) => {
                bus.call_method(Some(SERVICE), PATH, Some(INTERFACE), method, &(text,))
                    .await?
            }
            None => {
                bus.call_method(Some(SERVICE), PATH, Some(INTERFACE), method, &())
                    .await?
            }
        };
        reply.body().deserialize::<(bool, String)>()
    })
    .await;
    match result {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => {
            // Reconnect next time: the bus or Fcitx5 may have restarted.
            *connection = None;
            Err(error.to_string())
        }
        Err(_) => Err("fcitx5 bridge timed out".to_owned()),
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
    /// Mirror non-final, non-empty transcripts as preedit; clear on any end
    /// of session that did not commit (`Finalized` without an injection
    /// result, error, cancel, completion). The final transcript itself is
    /// not previewed: the commit that follows it replaces the preedit.
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        match &event {
            OutputEvent::Transcript(snapshot)
                if !snapshot.is_final && !snapshot.authoritative_text.is_empty() =>
            {
                self.client.preedit(&snapshot.authoritative_text);
                self.shown = true;
            }
            // The commit (queued after every preedit) replaces the preedit;
            // a session that ends without one clears it.
            OutputEvent::Finalized { injection } if injection.is_none() => self.clear(),
            OutputEvent::Error(_) | OutputEvent::Cancelled | OutputEvent::Completed => self.clear(),
            _ => {}
        }
        self.inner.emit(event)
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
