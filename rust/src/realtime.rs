//! DashScope realtime ASR transport: the `cloud-realtime` streaming backend
//! (`[asr] streaming_backend = "cloud-realtime"`, the default). It also
//! defines `RealtimeEvent`, the backend-neutral event vocabulary that
//! `doubao.rs`, `local_asr.rs` and `offline_asr.rs` reuse, so
//! `coordinator.rs` and `session.rs` never see backend specifics.
//!
//! Wire protocol: DashScope's OpenAI-Realtime-style WebSocket API, configured
//! by `[cloud] realtime_endpoint`, `realtime_model` and `api_key`.
//! `RealtimeSession` owns one socket and two tokio tasks: a writer that sends
//! JSON text frames and the close frame, and a reader that parses server
//! events. `stream.rs` wraps the session in `CloudTransport` (its
//! `BackendTransport` impl); `benchmark_cmd.rs` and `latency_cmd.rs` drive it
//! directly to replay corpus audio.
//!
//! Messages sent (JSON text frames, all built in this module):
//! - `session.update`, once right after the upgrade: text modality, raw
//!   16 kHz PCM input, Chinese transcription, server VAD (threshold 0.3,
//!   600 ms of silence closes an utterance).
//! - `input_audio_buffer.append` with the base64 PCM16 chunk, one per
//!   `send_audio`.
//! - `session.finish`, exactly once, from `finish`.
//! - a WebSocket Close frame from `cancel`.
//!
//! Messages received (JSON text frames, mapped by `parse_event`):
//! - `session.created` / `session.updated` -> `Ready` (passes the ready gate).
//! - `conversation.item.input_audio_transcription.text` -> `Partial`
//!   (`text` plus the still-revisable `stash`).
//! - `conversation.item.input_audio_transcription.completed` -> `Completed`
//!   (`transcript`: one definite utterance).
//! - `session.finished` -> `Finished` (`transcript`: the authoritative text
//!   of the whole session).
//! - `error` -> `Error` (`error.message`, or top-level `message`).
//! - every other event type is ignored.
//!
//! Ready gate: `connect` returns only after the HTTP upgrade succeeded (a
//! rejected key fails here, before any capture) and the `session.update` is
//! queued; the coordinator then waits up to 5 s for `Ready` before it starts
//! `pw-record`. Partial and confirmed text both come from the server as-is;
//! the `session.finished` transcript replaces the confirmed segments.
//!
//! Failure semantics: no retries, reconnects or timeouts live here. A socket
//! error, a close frame, EOF or malformed JSON ends the event stream with an
//! `io::Error`; a server `error` event is delivered as `RealtimeEvent::Error`.
//! Both end the session in the coordinator. Fallback to another backend is
//! decided only by `stream.rs` when `connect` fails, never inside this
//! module. All deadlines (ready 5 s, send 500 ms, finish 2 s) are enforced by
//! the coordinator, not by the transport.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message;

/// Backend-neutral recognition event, produced by every streaming backend
/// and consumed by `session::RecognitionSession::apply` through the
/// coordinator. Ordering within one session is the backend's arrival order.
#[derive(Debug, PartialEq, Eq)]
pub enum RealtimeEvent {
    /// The backend accepted the session. The coordinator's ready gate waits
    /// for the first `Ready` before starting capture; later ones are
    /// ignored by the session state machine.
    Ready,
    /// Live, revisable text of the open utterance. `text` is the part the
    /// server considers stable so far and `stash` the trailing part it may
    /// still rewrite; the session displays `text` + `stash` and clears both
    /// on the next `Completed`. Local backends always send an empty `stash`.
    Partial { text: String, stash: String },
    /// One utterance became definite. The session appends `transcript` to
    /// its confirmed segments (empty or repeated transcripts are skipped)
    /// and clears the partial.
    Completed { transcript: String },
    /// The backend's answer to `finish`: the authoritative transcript of the
    /// whole session. An empty transcript makes the session fall back to the
    /// joined confirmed segments. Only honoured while the session is
    /// stopping; the coordinator stops reading events after it.
    Finished { transcript: String },
    /// A failure reported by the backend itself (server error frame, model
    /// load failure). The session moves to `Failed` and never injects.
    /// Transport-level failures are `io::Error`s from `next_event` instead.
    Error(String),
}

/// What the writer task puts on the socket: one JSON text frame, or the
/// close handshake (after which the task exits).
enum Command {
    Text(String),
    Close,
}

/// One DashScope realtime WebSocket session.
///
/// `send_audio` and `next_event` take `&self` and run concurrently inside
/// the coordinator: commands go through a bounded channel (16) to the
/// writer task, events come from the reader task through a bounded channel
/// (32) whose receiver sits behind an async mutex. Dropping the session
/// drops the command sender, which ends the writer task and closes the
/// socket; the reader task ends when the socket does.
pub struct RealtimeSession {
    commands: mpsc::Sender<Command>,
    events: tokio::sync::Mutex<mpsc::Receiver<io::Result<RealtimeEvent>>>,
    /// `finish` must send `session.finish` at most once per session.
    finish_sent: bool,
}

impl RealtimeSession {
    /// Open the socket and queue the initial `session.update`.
    ///
    /// `model` is appended to `endpoint` as the `model` query parameter
    /// (`?` or `&` depending on whether the endpoint already has a query).
    /// The upgrade request carries `Authorization: Bearer <api_key>` and
    /// `OpenAI-Beta: realtime=v1`. Errors: `InvalidInput` for an unparsable
    /// endpoint or key, `ConnectionRefused` when the upgrade fails (DNS,
    /// TLS, or the HTTP rejection of a bad key), `BrokenPipe` if the writer
    /// task died before the update could be queued. Side effects: one
    /// network connection and two spawned tokio tasks. No `Ready` is
    /// produced here; it arrives from the server as `session.created` or
    /// `session.updated`.
    pub async fn connect(endpoint: &str, api_key: &str, model: &str) -> io::Result<Self> {
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let url = format!("{endpoint}{separator}model={model}");
        let mut request = url
            .into_client_request()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let authorization = format!("Bearer {api_key}");
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&authorization)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?,
        );
        request
            .headers_mut()
            .insert("OpenAI-Beta", HeaderValue::from_static("realtime=v1"));

        // The upgrade is the credential check: DashScope rejects a bad key
        // with an HTTP error here, so no audio is ever captured for it.
        let (socket, _) = connect_async(request)
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error.to_string()))?;
        let (mut writer, mut reader) = socket.split();
        // Commands: 16 slots, i.e. 16 queued 32 ms chunks (~0.5 s of audio)
        // before `send_audio` awaits; the coordinator's 500 ms send
        // deadline turns a longer stall into a session failure.
        let (command_tx, mut command_rx) = mpsc::channel::<Command>(16);
        let (event_tx, event_rx) = mpsc::channel::<io::Result<RealtimeEvent>>(32);

        // Writer task: serialises commands in order; exits after the close
        // frame, on a send failure, or when the last sender is dropped.
        tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                let should_close = matches!(&command, Command::Close);
                let message = match command {
                    Command::Text(text) => Message::Text(text.into()),
                    Command::Close => Message::Close(None),
                };
                if writer.send(message).await.is_err() {
                    break;
                }
                if should_close {
                    break;
                }
            }
            let _ = writer.close().await;
        });

        // Reader task: every terminal condition (socket error, close frame,
        // EOF, bad JSON) is delivered once as `Err` and ends the task, so
        // `next_event` always observes why the stream ended.
        tokio::spawn(async move {
            loop {
                let message = match reader.next().await {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        let _ = event_tx.send(Err(socket_error(error))).await;
                        break;
                    }
                    None => {
                        let _ = event_tx
                            .send(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "realtime socket closed",
                            )))
                            .await;
                        break;
                    }
                };
                match message {
                    Message::Text(text) => match parse_event(text.as_ref()) {
                        Ok(Some(event)) => {
                            if event_tx.send(Ok(event)).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = event_tx.send(Err(error)).await;
                            break;
                        }
                    },
                    Message::Ping(payload) => {
                        // A server ping (keepalive) is reported as `Ready`
                        // too: tungstenite answers the pong itself, and the
                        // session ignores `Ready` after the ready gate.
                        let _ = event_tx.send(Ok(RealtimeEvent::Ready)).await;
                        let _ = payload;
                    }
                    Message::Close(frame) => {
                        let reason = frame
                            .map(|value| value.reason.to_string())
                            .unwrap_or_else(|| "realtime socket closed".to_owned());
                        let _ = event_tx
                            .send(Err(io::Error::new(io::ErrorKind::UnexpectedEof, reason)))
                            .await;
                        break;
                    }
                    Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        });

        let session = Self {
            commands: command_tx,
            events: tokio::sync::Mutex::new(event_rx),
            finish_sent: false,
        };
        // Session parameters. Voiceless wants transcription only, so the
        // response modality is text and the input is the raw capture format
        // (16 kHz mono PCM16, no container). Server VAD segments utterances
        // into `completed` items: an utterance closes after 600 ms of
        // silence at a 0.3 speech threshold. The server acknowledges this
        // with `session.updated`, which is the `Ready` the gate waits for.
        session
            .send_text(json!({
                "type": "session.update",
                "session": {
                    "modalities": ["text"],
                    "input_audio_format": "pcm",
                    "sample_rate": 16000,
                    "input_audio_transcription": {"language": "zh"},
                    "turn_detection": {
                        "type": "server_vad",
                        "threshold": 0.3,
                        "silence_duration_ms": 600
                    }
                }
            }))
            .await?;
        Ok(session)
    }

    /// Queue one PCM16 LE 16 kHz mono chunk as an `input_audio_buffer.append`
    /// (`audio`: standard base64 with padding). Any chunk length is
    /// accepted; the coordinator sends 32 ms (1024 byte) chunks and the
    /// benchmarks replay the same size. Awaits while the 16-slot command
    /// channel is full. Fails with `BrokenPipe` once the writer task has
    /// exited (socket gone, or `cancel` already sent).
    pub async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        let audio = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, pcm);
        self.send_text(json!({
            "type": "input_audio_buffer.append",
            "audio": audio
        }))
        .await
    }

    /// Next server event in arrival order. `Err` is terminal for the
    /// session: `ConnectionAborted` for a socket error, `UnexpectedEof` for
    /// a close frame, EOF or a finished reader task, `InvalidData` for a
    /// non-JSON text frame. Holds the receiver mutex only while waiting; the
    /// coordinator is the single caller, so there is no contention.
    pub async fn next_event(&self) -> io::Result<RealtimeEvent> {
        self.events.lock().await.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "realtime event task closed")
        })?
    }

    /// Queue `session.finish` exactly once; repeated calls return `Ok(())`
    /// without sending. The server answers with `session.finished`, which
    /// `next_event` delivers as `Finished`. Does not wait for that answer
    /// and does not close the socket: the coordinator keeps reading until
    /// `Finished` (or its 2 s finish deadline).
    pub async fn finish(&mut self) -> io::Result<()> {
        if self.finish_sent {
            return Ok(());
        }
        self.finish_sent = true;
        self.send_text(json!({"type": "session.finish"})).await
    }

    /// Queue a WebSocket Close frame. Commands queued before it (audio, a
    /// pending finish) are still written first; the writer task then exits.
    /// The reader subsequently reports `UnexpectedEof`, which cancel and
    /// error paths in the coordinator ignore. Fails with `BrokenPipe` if the
    /// writer task is already gone, which is harmless for a cancel.
    pub async fn cancel(&self) -> io::Result<()> {
        self.commands
            .send(Command::Close)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "realtime writer closed"))
    }

    /// Serialise `value` and hand it to the writer task as one text frame.
    async fn send_text(&self, value: Value) -> io::Result<()> {
        self.commands
            .send(Command::Text(value.to_string()))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "realtime writer closed"))
    }
}

/// Map one server text frame onto a `RealtimeEvent` by its `type` field.
/// `Ok(None)` for event types Voiceless does not use (acks such as
/// `input_audio_buffer.committed`, `response.*`, ...), so unknown events
/// never end the session; `Err(InvalidData)` only when the frame is not
/// JSON at all. Missing string fields decode as empty strings, not errors.
fn parse_event(text: &str) -> io::Result<Option<RealtimeEvent>> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let event = match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        // Either acknowledgement passes the ready gate; which one arrives
        // first depends on the server.
        "session.created" | "session.updated" => Some(RealtimeEvent::Ready),
        "conversation.item.input_audio_transcription.text" => Some(RealtimeEvent::Partial {
            text: string_field(&value, "text"),
            stash: string_field(&value, "stash"),
        }),
        "conversation.item.input_audio_transcription.completed" => Some(RealtimeEvent::Completed {
            transcript: string_field(&value, "transcript"),
        }),
        "session.finished" => Some(RealtimeEvent::Finished {
            transcript: string_field(&value, "transcript"),
        }),
        // Two shapes are handled: `{"error": {"message": ...}}` (OpenAI
        // style) and a flat `{"message": ...}`; a generic text is the
        // fallback so the session still fails with a readable reason.
        "error" => {
            return Ok(Some(RealtimeEvent::Error(
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .unwrap_or("realtime ASR request failed")
                    .to_owned(),
            )))
        }
        _ => None,
    };
    Ok(event)
}

/// String field `name` of a JSON object, or `""` when absent or not a string.
fn string_field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// tungstenite errors after the upgrade all count as an aborted connection.
fn socket_error(error: tokio_tungstenite::tungstenite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
}
