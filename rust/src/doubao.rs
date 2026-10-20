//! Volcengine (Doubao) bigmodel streaming ASR over the OpenSpeech binary
//! WebSocket protocol. Events are mapped onto `RealtimeEvent` so the
//! coordinator drives it like the DashScope transport: definite utterances
//! become `Completed`, the open utterance becomes `Partial`, and the
//! last-packet response becomes `Finished`.

use std::io::{self, Read, Write};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::config::DoubaoConfig;
use crate::realtime::RealtimeEvent;

const PROTOCOL_VERSION: u8 = 0b0001;
const HEADER_WORDS: u8 = 0b0001;

const FULL_CLIENT_REQUEST: u8 = 0b0001;
const AUDIO_ONLY_REQUEST: u8 = 0b0010;
const FULL_SERVER_RESPONSE: u8 = 0b1001;
const SERVER_ERROR: u8 = 0b1111;

const FLAG_NONE: u8 = 0b0000;
const FLAG_SEQUENCE: u8 = 0b0001;
const FLAG_LAST: u8 = 0b0010;
const FLAG_LAST_WITH_SEQUENCE: u8 = 0b0011;

const SERIALIZATION_NONE: u8 = 0b0000;
const SERIALIZATION_JSON: u8 = 0b0001;
const COMPRESSION_NONE: u8 = 0b0000;
const COMPRESSION_GZIP: u8 = 0b0001;

/// 100 ms of 16 kHz mono PCM16; the service recommends 100-200 ms packets.
pub const AUDIO_PACKET_BYTES: usize = 3_200;

/// One decoded server frame.
#[derive(Debug, PartialEq)]
pub enum ServerPacket {
    Response { payload: Value, last: bool },
    Error { code: u32, message: String },
}

fn header(message_type: u8, flags: u8, serialization: u8, compression: u8) -> [u8; 4] {
    [
        (PROTOCOL_VERSION << 4) | HEADER_WORDS,
        (message_type << 4) | flags,
        (serialization << 4) | compression,
        0,
    ]
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(bytes)
        .expect("in-memory gzip write cannot fail");
    encoder.finish().expect("in-memory gzip finish cannot fail")
}

fn frame(head: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&head);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Encode the full client request (gzip JSON) that opens a session.
pub fn encode_full_request(request: &Value) -> Vec<u8> {
    let payload = gzip(request.to_string().as_bytes());
    frame(
        header(
            FULL_CLIENT_REQUEST,
            FLAG_NONE,
            SERIALIZATION_JSON,
            COMPRESSION_GZIP,
        ),
        &payload,
    )
}

/// Encode an audio-only packet; `last` marks the final (negative) packet.
pub fn encode_audio(pcm: &[u8], last: bool) -> Vec<u8> {
    let flags = if last { FLAG_LAST } else { FLAG_NONE };
    frame(
        header(
            AUDIO_ONLY_REQUEST,
            flags,
            SERIALIZATION_NONE,
            COMPRESSION_GZIP,
        ),
        &gzip(pcm),
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn read_u32(raw: &[u8], offset: usize) -> io::Result<u32> {
    raw.get(offset..offset + 4)
        .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or_else(|| invalid("doubao packet truncated"))
}

fn decompress(payload: &[u8], compression: u8) -> io::Result<Vec<u8>> {
    match compression {
        COMPRESSION_NONE => Ok(payload.to_vec()),
        COMPRESSION_GZIP => {
            let mut out = Vec::new();
            GzDecoder::new(payload).read_to_end(&mut out)?;
            Ok(out)
        }
        other => Err(invalid(format!("doubao compression {other} unsupported"))),
    }
}

/// Decode one binary server frame.
pub fn decode_server_packet(raw: &[u8]) -> io::Result<ServerPacket> {
    if raw.len() < 4 {
        return Err(invalid("doubao packet shorter than its header"));
    }
    if raw[0] >> 4 != PROTOCOL_VERSION {
        return Err(invalid(format!(
            "doubao protocol version {} unsupported",
            raw[0] >> 4
        )));
    }
    let mut offset = usize::from(raw[0] & 0x0F) * 4;
    let message_type = raw[1] >> 4;
    let flags = raw[1] & 0x0F;
    let serialization = raw[2] >> 4;
    let compression = raw[2] & 0x0F;
    match message_type {
        FULL_SERVER_RESPONSE => {
            if matches!(flags, FLAG_SEQUENCE | FLAG_LAST_WITH_SEQUENCE) {
                offset += 4;
            }
            let size = read_u32(raw, offset)? as usize;
            offset += 4;
            let body = raw
                .get(offset..offset + size)
                .ok_or_else(|| invalid("doubao payload truncated"))?;
            let body = decompress(body, compression)?;
            let payload = if serialization == SERIALIZATION_JSON && !body.is_empty() {
                serde_json::from_slice(&body).map_err(|error| invalid(error.to_string()))?
            } else {
                Value::Null
            };
            Ok(ServerPacket::Response {
                payload,
                last: matches!(flags, FLAG_LAST | FLAG_LAST_WITH_SEQUENCE),
            })
        }
        SERVER_ERROR => {
            let code = read_u32(raw, offset)?;
            let size = read_u32(raw, offset + 4)? as usize;
            offset += 8;
            let body = raw
                .get(offset..offset + size)
                .ok_or_else(|| invalid("doubao error payload truncated"))?;
            let body = decompress(body, compression).unwrap_or_else(|_| body.to_vec());
            Ok(ServerPacket::Error {
                code,
                message: String::from_utf8_lossy(&body).trim().to_owned(),
            })
        }
        other => Err(invalid(format!("doubao message type {other} unsupported"))),
    }
}

/// Build the full client request JSON from the `[doubao]` config.
pub fn session_request(config: &DoubaoConfig, uid: &str) -> Value {
    let mut request = json!({
        "model_name": "bigmodel",
        "enable_itn": config.enable_itn,
        "enable_punc": config.enable_punc,
        "enable_ddc": config.enable_ddc,
        "show_utterances": true,
        "result_type": "full",
    });
    if config.enable_nonstream {
        request["enable_nonstream"] = json!(true);
    }
    if let Some(window) = config.end_window_size {
        request["end_window_size"] = json!(window);
    }
    if !config.hotwords.is_empty() {
        let words: Vec<Value> = config
            .hotwords
            .iter()
            .map(|word| json!({ "word": word }))
            .collect();
        request["corpus"] = json!({
            "context": json!({ "hotwords": words }).to_string(),
        });
    }
    json!({
        "user": { "uid": uid },
        "audio": {
            "format": "pcm",
            "codec": "raw",
            "rate": 16_000,
            "bits": 16,
            "channel": 1,
        },
        "request": request,
    })
}

/// Maps full-result responses onto session events. Definite utterances are
/// confirmed once, in order; the remaining utterances form the partial.
#[derive(Debug, Default)]
pub struct ResultTracker {
    confirmed: usize,
    last_partial: String,
}

impl ResultTracker {
    pub fn events(&mut self, payload: &Value, last: bool) -> Vec<RealtimeEvent> {
        let result = &payload["result"];
        let text = result["text"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if last {
            return vec![RealtimeEvent::Finished { transcript: text }];
        }
        let Some(utterances) = result["utterances"].as_array() else {
            return self.partial(text);
        };
        let mut events = Vec::new();
        let mut partial = String::new();
        for (index, utterance) in utterances.iter().enumerate() {
            let piece = utterance["text"].as_str().unwrap_or_default();
            let definite = utterance["definite"].as_bool().unwrap_or(false);
            if index < self.confirmed {
                continue;
            }
            if definite && partial.is_empty() {
                self.confirmed = index + 1;
                self.last_partial.clear();
                if !piece.is_empty() {
                    events.push(RealtimeEvent::Completed {
                        transcript: piece.to_owned(),
                    });
                }
            } else {
                partial.push_str(piece);
            }
        }
        events.extend(self.partial(partial));
        events
    }

    fn partial(&mut self, text: String) -> Vec<RealtimeEvent> {
        if text.is_empty() || text == self.last_partial {
            return Vec::new();
        }
        self.last_partial = text.clone();
        vec![RealtimeEvent::Partial {
            text,
            stash: String::new(),
        }]
    }
}

enum Command {
    Audio(Vec<u8>),
    Close,
}

pub struct DoubaoSession {
    commands: mpsc::Sender<Command>,
    events: tokio::sync::Mutex<mpsc::Receiver<io::Result<RealtimeEvent>>>,
    pending: tokio::sync::Mutex<Vec<u8>>,
    finish_sent: bool,
}

impl DoubaoSession {
    /// Open the WebSocket, send the full client request and report `Ready`
    /// once the upgrade succeeded (auth failures surface as a 401/403 here).
    pub async fn connect(config: &DoubaoConfig) -> io::Result<Self> {
        let mut request = config
            .endpoint
            .as_str()
            .into_client_request()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let connect_id = random_id();
        let headers = request.headers_mut();
        let mut insert = |name: &'static str, value: &str| -> io::Result<()> {
            let value = HeaderValue::from_str(value)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            headers.insert(name, value);
            Ok(())
        };
        if config.api_key.trim().is_empty() {
            insert("X-Api-App-Key", config.app_key.trim())?;
            insert("X-Api-Access-Key", config.access_key.trim())?;
        } else {
            insert("X-Api-Key", config.api_key.trim())?;
        }
        insert("X-Api-Resource-Id", &config.resource_id)?;
        insert("X-Api-Connect-Id", &connect_id)?;

        let (socket, response) = connect_async(request)
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error.to_string()))?;
        let log_id = response
            .headers()
            .get("X-Tt-Logid")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let (mut writer, mut reader) = socket.split();
        let (command_tx, mut command_rx) = mpsc::channel::<Command>(64);
        let (event_tx, event_rx) = mpsc::channel::<io::Result<RealtimeEvent>>(64);

        let uid = if config.app_key.trim().is_empty() {
            "voiceless".to_owned()
        } else {
            config.app_key.trim().to_owned()
        };
        writer
            .send(Message::Binary(
                encode_full_request(&session_request(config, &uid)).into(),
            ))
            .await
            .map_err(socket_error)?;
        event_tx
            .send(Ok(RealtimeEvent::Ready))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "doubao event channel"))?;

        tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                let message = match command {
                    Command::Audio(frame) => Message::Binary(frame.into()),
                    Command::Close => Message::Close(None),
                };
                let close = matches!(message, Message::Close(_));
                if writer.send(message).await.is_err() || close {
                    break;
                }
            }
            let _ = writer.close().await;
        });

        tokio::spawn(async move {
            let mut tracker = ResultTracker::default();
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
                                "doubao socket closed",
                            )))
                            .await;
                        break;
                    }
                };
                let events = match message {
                    Message::Binary(raw) => match decode_server_packet(&raw) {
                        Ok(ServerPacket::Response { payload, last }) => {
                            tracker.events(&payload, last)
                        }
                        Ok(ServerPacket::Error { code, message }) => {
                            vec![RealtimeEvent::Error(error_message(code, &message, &log_id))]
                        }
                        Err(error) => {
                            let _ = event_tx.send(Err(error)).await;
                            break;
                        }
                    },
                    Message::Text(text) => {
                        vec![RealtimeEvent::Error(error_message(0, &text, &log_id))]
                    }
                    Message::Close(frame) => {
                        let reason = frame
                            .map(|value| value.reason.to_string())
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "doubao socket closed".to_owned());
                        let _ = event_tx
                            .send(Err(io::Error::new(io::ErrorKind::UnexpectedEof, reason)))
                            .await;
                        break;
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Vec::new(),
                };
                for event in events {
                    if event_tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self {
            commands: command_tx,
            events: tokio::sync::Mutex::new(event_rx),
            pending: tokio::sync::Mutex::new(Vec::with_capacity(AUDIO_PACKET_BYTES * 2)),
            finish_sent: false,
        })
    }

    /// Buffer capture chunks into 100 ms packets before sending.
    pub async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        let mut pending = self.pending.lock().await;
        pending.extend_from_slice(pcm);
        while pending.len() >= AUDIO_PACKET_BYTES {
            let packet: Vec<u8> = pending.drain(..AUDIO_PACKET_BYTES).collect();
            self.send(Command::Audio(encode_audio(&packet, false)))
                .await?;
        }
        Ok(())
    }

    pub async fn next_event(&self) -> io::Result<RealtimeEvent> {
        self.events.lock().await.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "doubao event task closed")
        })?
    }

    /// Flush the buffered tail as the last packet.
    pub async fn finish(&mut self) -> io::Result<()> {
        if self.finish_sent {
            return Ok(());
        }
        self.finish_sent = true;
        let tail = std::mem::take(&mut *self.pending.lock().await);
        self.send(Command::Audio(encode_audio(&tail, true))).await
    }

    pub async fn cancel(&self) -> io::Result<()> {
        self.send(Command::Close).await
    }

    async fn send(&self, command: Command) -> io::Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "doubao writer closed"))
    }
}

fn error_message(code: u32, message: &str, log_id: &str) -> String {
    let mut text = if code == 0 {
        format!("doubao ASR error: {message}")
    } else {
        format!("doubao ASR error {code}: {message}")
    };
    if !log_id.is_empty() {
        text.push_str(&format!(" (logid {log_id})"));
    }
    text
}

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn socket_error(error: tokio_tungstenite::tungstenite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
}
