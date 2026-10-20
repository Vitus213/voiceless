//! Client of the Voiceless Fcitx5 bridge module (`fcitx5-bridge/`): commits
//! the final text and shows the live partial as preedit in the focused
//! input field.
//!
//! One actor task owns one D-Bus connection and applies requests strictly
//! in order, so a stale preedit can never land after the commit that
//! replaced it; consecutive preedit updates collapse to the newest.

use std::io;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::coordinator::{EventSink, OutputEvent};

pub const SERVICE: &str = "org.fcitx.Fcitx5";
pub const PATH: &str = "/voiceless";
pub const INTERFACE: &str = "dev.voiceless.Fcitx5Bridge1";

const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Bridge reply: (ok, target program or failure reason).
pub type Reply = Result<(bool, String), String>;

enum Request {
    Preedit(String),
    Clear,
    Commit(String, oneshot::Sender<Reply>),
}

#[derive(Clone)]
pub struct Fcitx5Client {
    requests: mpsc::UnboundedSender<Request>,
}

impl Fcitx5Client {
    /// Start the actor on the current Tokio runtime. The bus connection is
    /// opened lazily, so constructing a client never fails.
    pub fn spawn() -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run(receiver));
        Self { requests }
    }

    /// Show `text` as preedit (best effort, never blocks).
    pub fn preedit(&self, text: &str) {
        let _ = self.requests.send(Request::Preedit(text.to_owned()));
    }

    /// Remove this bridge's preedit (best effort, never blocks).
    pub fn clear(&self) {
        let _ = self.requests.send(Request::Clear);
    }

    /// Commit `text`, after every earlier preedit request.
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

async fn run(mut receiver: mpsc::UnboundedReceiver<Request>) {
    let mut connection: Option<zbus::Connection> = None;
    while let Some(first) = receiver.recv().await {
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
pub struct PreeditSink<S> {
    inner: S,
    client: Fcitx5Client,
    shown: bool,
}

impl<S> PreeditSink<S> {
    pub fn new(inner: S, client: Fcitx5Client) -> Self {
        Self {
            inner,
            client,
            shown: false,
        }
    }
}

impl<S: EventSink> EventSink for PreeditSink<S> {
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
    fn clear(&mut self) {
        if std::mem::take(&mut self.shown) {
            self.client.clear();
        }
    }
}
