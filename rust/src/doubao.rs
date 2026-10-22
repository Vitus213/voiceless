//! Volcengine (Doubao) bigmodel streaming ASR over the OpenSpeech binary
//! WebSocket protocol: the `doubao-realtime` streaming backend
//! (`[asr] streaming_backend = "doubao-realtime"`, also the usual
//! `fallback_backend`). Events are mapped onto `RealtimeEvent` so the
//! coordinator drives it like the DashScope transport: definite utterances
//! become `Completed`, the open utterance becomes `Partial`, and the
//! last-packet response becomes `Finished`.
//!
//! Config: `[doubao]` (`endpoint`, `api_key` or the legacy `app_key` +
//! `access_key` pair, `resource_id`, `enable_itn`, `enable_punc`,
//! `enable_ddc`, `enable_nonstream`, `end_window_size`, `hotwords`) as
//! merged with `[text] hotwords` by `AppConfig::doubao_engine`. `stream.rs`
//! wraps `DoubaoSession` in `DoubaoTransport` and refuses to open the
//! backend when `DoubaoConfig::has_credentials` is false, so a missing key
//! never reaches the network. `config.rs` enforces a `wss://` endpoint.
//!
//! Wire format (the `sauc` `bigmodel_async` endpoint): every frame is a
//! 4-byte header, an optional 4-byte big-endian sequence number, a 4-byte
//! big-endian payload size and the payload. `header`, `frame` and
//! `decode_server_packet` document the byte layout. The client sends one
//! gzip-JSON full request (`session_request`) and then gzip PCM audio-only
//! packets of 100 ms (`AUDIO_PACKET_BYTES`), the last one flagged as last.
//! The server answers with gzip-JSON full responses whose `result.text` is
//! the complete text so far (`result_type = "full"`) and whose
//! `result.utterances` (`show_utterances = true`) carry per-utterance
//! `text` and `definite`; errors come as a dedicated error frame.
//!
//! Ready gate: the WebSocket upgrade is the credential and resource-id check
//! (HTTP 401/403 fail `connect`), the full request is written on the socket,
//! and `Ready` is queued locally at once. The server sends no session
//! acknowledgement before its first result, so nothing else could pass the
//! gate. Partial text is the join of the not-yet-definite utterances of the
//! latest response; confirmed text is one `Completed` per utterance that
//! turned definite (`ResultTracker`). `finish` sends the buffered tail as
//! the last packet, and the response flagged as last becomes `Finished`
//! with the server's full `result.text`. `cancel` sends a Close frame.
//!
//! Failure semantics: no retries, reconnects or timeouts here (the
//! coordinator enforces ready 5 s, send 500 ms, finish 2 s). Server error
//! frames and unexpected text frames become `RealtimeEvent::Error` tagged
//! with the connection's `X-Tt-Logid`; socket errors, EOF and undecodable
//! frames end the event stream with an `io::Error`. Fallback to another
//! backend happens only in `stream.rs`, and only when `connect` fails.

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

// Byte 0 of every frame: protocol version in the high nibble, header size in
// the low nibble counted in 4-byte words (1 = the 4-byte header used here).
const PROTOCOL_VERSION: u8 = 0b0001;
const HEADER_WORDS: u8 = 0b0001;

// Byte 1, high nibble: message type. Client -> server types have the high
// bit clear, server -> client types have it set.
/// JSON session parameters; sent once, first.
const FULL_CLIENT_REQUEST: u8 = 0b0001;
/// One PCM packet (no JSON).
const AUDIO_ONLY_REQUEST: u8 = 0b0010;
/// A recognition result (JSON).
const FULL_SERVER_RESPONSE: u8 = 0b1001;
/// An error code plus message; the server closes after it.
const SERVER_ERROR: u8 = 0b1111;

// Byte 1, low nibble: message-type-specific flags. Bit 0 says a 4-byte
// sequence number follows the header; bit 1 marks the last packet of the
// stream (the protocol's "negative sequence" packet). The client never
// numbers its packets, so it only sends FLAG_NONE and FLAG_LAST; the server
// may use any of the four.
const FLAG_NONE: u8 = 0b0000;
const FLAG_SEQUENCE: u8 = 0b0001;
const FLAG_LAST: u8 = 0b0010;
const FLAG_LAST_WITH_SEQUENCE: u8 = 0b0011;

// Byte 2: payload serialization in the high nibble (0 raw, 1 JSON) and
// compression in the low nibble (0 none, 1 gzip). Byte 3 is reserved (0).
const SERIALIZATION_NONE: u8 = 0b0000;
const SERIALIZATION_JSON: u8 = 0b0001;
const COMPRESSION_NONE: u8 = 0b0000;
const COMPRESSION_GZIP: u8 = 0b0001;

/// 100 ms of 16 kHz mono PCM16 (16 000 samples/s * 2 bytes / 10); the
/// service recommends 100-200 ms packets. `DoubaoSession::send_audio`
/// re-packetises the coordinator's 32 ms chunks to this size.
pub const AUDIO_PACKET_BYTES: usize = 3_200;

/// One decoded server frame (see `decode_server_packet`).
#[derive(Debug, PartialEq)]
pub enum ServerPacket {
    /// A full server response. `payload` is the parsed JSON (`Null` when
    /// the body is empty or not JSON-serialised); `last` is the
    /// last-packet flag, i.e. the response to the final audio packet.
    Response { payload: Value, last: bool },
    /// A server error frame: the service error `code` (for example
    /// 45000151 "invalid audio format") and its text message.
    Error { code: u32, message: String },
}

/// Build the 4-byte frame header:
/// `[version<<4 | header words, type<<4 | flags, serialization<<4 | compression, 0]`.
fn header(message_type: u8, flags: u8, serialization: u8, compression: u8) -> [u8; 4] {
    [
        (PROTOCOL_VERSION << 4) | HEADER_WORDS,
        (message_type << 4) | flags,
        (serialization << 4) | compression,
        0,
    ]
}

/// gzip `bytes` in memory (fastest level: audio packets go out every 100 ms).
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(bytes)
        .expect("in-memory gzip write cannot fail");
    encoder.finish().expect("in-memory gzip finish cannot fail")
}

/// Assemble a client frame: header (4 bytes) + payload size (u32 big
/// endian, 4 bytes) + payload. Client frames carry no sequence number, so
/// the size always sits at byte offset 4 and the payload at offset 8.
fn frame(head: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&head);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Encode the full client request (gzip JSON) that opens a session:
/// type `0b0001`, no flags, JSON serialization, gzip compression.
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

/// Encode an audio-only packet (type `0b0010`, raw serialization, gzip
/// PCM); `last` sets the last-packet flag (`0b0010`) that tells the server
/// to produce its final response. An empty `pcm` with `last` is valid and
/// is how `finish` ends a session whose tail buffer is empty.
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

/// `InvalidData` error for a malformed server frame.
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Big-endian u32 at `offset`, or `InvalidData` when the frame is too short.
fn read_u32(raw: &[u8], offset: usize) -> io::Result<u32> {
    raw.get(offset..offset + 4)
        .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or_else(|| invalid("doubao packet truncated"))
}

/// Undo the payload compression named by the header's compression nibble.
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
///
/// Layout, offsets in bytes:
/// - `0`: version (high nibble, must be 1) and header size in words (low
///   nibble; the payload starts after `words * 4` bytes, so a longer header
///   is skipped rather than rejected).
/// - `1`: message type (high nibble) and flags (low nibble).
/// - `2`: serialization (high nibble) and compression (low nibble).
/// - `3`: reserved.
/// - Full server response (`0b1001`): `[sequence u32 BE]` only when flag
///   bit 0 is set, then payload size (u32 BE), then the payload (gzip when
///   flagged; parsed as JSON when serialization is JSON and the body is not
///   empty, otherwise `Null`).
/// - Server error (`0b1111`): error code (u32 BE), payload size (u32 BE),
///   then the message bytes; no sequence number. A body that does not
///   gunzip is used verbatim so the error text is never lost.
///
/// Errors (`InvalidData`): fewer than 4 bytes, another protocol version,
/// an unknown message type or compression, a truncated payload, or a JSON
/// body that does not parse.
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
    // Header size is declared in 4-byte words; honour it instead of
    // assuming 4 bytes.
    let mut offset = usize::from(raw[0] & 0x0F) * 4;
    let message_type = raw[1] >> 4;
    let flags = raw[1] & 0x0F;
    let serialization = raw[2] >> 4;
    let compression = raw[2] & 0x0F;
    match message_type {
        FULL_SERVER_RESPONSE => {
            // Flag bit 0: a 4-byte sequence number precedes the size. Its
            // value is not needed (results are cumulative), only skipped.
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
                // Flag bit 1: this is the response to the last audio packet.
                last: matches!(flags, FLAG_LAST | FLAG_LAST_WITH_SEQUENCE),
            })
        }
        SERVER_ERROR => {
            // Error frames: code, then size, then message; no sequence.
            let code = read_u32(raw, offset)?;
            let size = read_u32(raw, offset + 4)? as usize;
            offset += 8;
            let body = raw
                .get(offset..offset + size)
                .ok_or_else(|| invalid("doubao error payload truncated"))?;
            // The frame already is an error; a body that fails to gunzip
            // is used verbatim rather than hiding the reason behind a
            // decode error.
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
///
/// Shape: `user.uid` (the caller's app key, or `"voiceless"`),
/// `audio` (`format: "pcm"`, `codec: "raw"`, `rate: 16000`, `bits: 16`,
/// `channel: 1`, i.e. the capture format sent verbatim) and `request`:
/// - `model_name: "bigmodel"` (fixed by the endpoint),
/// - `enable_itn` (inverse text normalisation: spoken numbers to digits),
///   `enable_punc` (punctuation), `enable_ddc` (semantic smoothing that
///   drops fillers) straight from the config,
/// - `show_utterances: true` so responses carry `result.utterances` with
///   the `definite` flag `ResultTracker` relies on,
/// - `result_type: "full"` so every response carries the complete text,
/// - `enable_nonstream: true` only when configured (second-pass decode of
///   each sentence, bigmodel_async only),
/// - `end_window_size` (ms of silence that closes a sentence) only when set,
/// - `corpus.context`: the hotword list as a JSON *string*
///   (`{"hotwords":[{"word":...}]}` serialised and embedded), the format
///   the service expects; omitted when there are no hotwords.
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
        // `context` is a JSON document encoded as a string, not an object.
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
///
/// Every response repeats the whole session (`result_type = "full"`), so
/// the tracker remembers how many leading utterances it has already
/// confirmed (`confirmed`) and the last partial it emitted
/// (`last_partial`) to report only what changed. One tracker lives on the
/// reader task of a `DoubaoSession`; it is also usable standalone (tests).
#[derive(Debug, Default)]
pub struct ResultTracker {
    /// Number of leading utterances already reported as `Completed`.
    confirmed: usize,
    /// Last partial text emitted, to suppress identical repeats.
    last_partial: String,
}

impl ResultTracker {
    /// Translate one response `payload` (`{"result": {"text", "utterances"}}`)
    /// into zero or more events. With `last` set (the response to the last
    /// audio packet) the result is a single `Finished` carrying the
    /// trimmed `result.text`, regardless of utterances. Otherwise each not
    /// yet confirmed utterance with `definite = true` yields one `Completed`
    /// (empty text skipped), but only while no open utterance precedes it:
    /// a definite utterance behind a non-definite one stays part of the
    /// partial until the earlier one closes, so confirmed text never
    /// reorders. The non-definite remainder, joined, becomes a `Partial`
    /// when it differs from the previous one. Responses without an
    /// `utterances` array use the whole `result.text` as the partial.
    pub fn events(&mut self, payload: &Value, last: bool) -> Vec<RealtimeEvent> {
        let result = &payload["result"];
        let text = result["text"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if last {
            // The final response's full text is authoritative; its
            // utterances are not re-examined.
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
            // Only a definite prefix is confirmed; once an open utterance
            // has been seen, everything after it is partial.
            if definite && partial.is_empty() {
                self.confirmed = index + 1;
                // The partial that just got confirmed must not suppress an
                // identical next partial.
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

    /// Emit `text` as a `Partial` unless it is empty or unchanged.
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

/// What the writer task puts on the socket: one encoded binary frame, or
/// the close handshake (after which the task exits).
enum Command {
    Audio(Vec<u8>),
    Close,
}

/// One Doubao streaming session: the socket split across a writer task
/// (binary frames, in order) and a reader task (decodes frames, runs the
/// `ResultTracker`). `send_audio` and `next_event` take `&self` and run
/// concurrently in the coordinator; both channels hold 64 entries. Dropping
/// the session drops the command sender, which ends the writer task and
/// closes the socket.
pub struct DoubaoSession {
    commands: mpsc::Sender<Command>,
    events: tokio::sync::Mutex<mpsc::Receiver<io::Result<RealtimeEvent>>>,
    /// Capture bytes not yet sent: less than one 100 ms packet, flushed as
    /// the last packet by `finish`.
    pending: tokio::sync::Mutex<Vec<u8>>,
    /// `finish` must send the last packet at most once per session.
    finish_sent: bool,
}

impl DoubaoSession {
    /// Open the WebSocket, send the full client request and report `Ready`
    /// once the upgrade succeeded (auth failures surface as a 401/403 here).
    ///
    /// Upgrade headers: `X-Api-Key` when `api_key` is set, otherwise the
    /// legacy `X-Api-App-Key` + `X-Api-Access-Key` pair (values trimmed);
    /// `X-Api-Resource-Id` (the activated service, e.g.
    /// `volc.seedasr.sauc.duration`); `X-Api-Connect-Id` (a fresh random
    /// 32-hex-char id per connection). The response's `X-Tt-Logid` is kept
    /// to tag later error messages for support requests. Errors:
    /// `InvalidInput` for an unparsable endpoint or header value,
    /// `ConnectionRefused` when the upgrade fails (including the HTTP
    /// 401/403 for bad credentials or resource id), `ConnectionAborted`
    /// when the full request cannot be written. Side effects: one network
    /// connection and two spawned tokio tasks.
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
        // New-console key wins; the legacy pair is only used without it.
        if config.api_key.trim().is_empty() {
            insert("X-Api-App-Key", config.app_key.trim())?;
            insert("X-Api-Access-Key", config.access_key.trim())?;
        } else {
            insert("X-Api-Key", config.api_key.trim())?;
        }
        insert("X-Api-Resource-Id", &config.resource_id)?;
        insert("X-Api-Connect-Id", &connect_id)?;

        // The upgrade is where the service checks credentials and the
        // resource id, so a misconfiguration fails before any capture.
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

        // `user.uid` identifies the caller to the service; the legacy App ID
        // is the natural value, new-console setups have none.
        let uid = if config.app_key.trim().is_empty() {
            "voiceless".to_owned()
        } else {
            config.app_key.trim().to_owned()
        };
        // The full request goes out directly, before the writer task owns
        // the sink, so it is guaranteed to precede every audio packet.
        writer
            .send(Message::Binary(
                encode_full_request(&session_request(config, &uid)).into(),
            ))
            .await
            .map_err(socket_error)?;
        // Ready is local: the service sends nothing until the first result.
        event_tx
            .send(Ok(RealtimeEvent::Ready))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "doubao event channel"))?;

        // Writer task: binary frames in order; exits after the close frame,
        // on a send failure, or when the last sender is dropped.
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

        // Reader task: decodes every binary frame and maps it through the
        // tracker. Terminal conditions (socket error, EOF, close frame,
        // undecodable frame) are delivered once as `Err` and end the task.
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
                        // Server errors are session failures, not transport
                        // failures: the session reports them and never
                        // injects.
                        Ok(ServerPacket::Error { code, message }) => {
                            vec![RealtimeEvent::Error(error_message(code, &message, &log_id))]
                        }
                        Err(error) => {
                            let _ = event_tx.send(Err(error)).await;
                            break;
                        }
                    },
                    // The protocol is binary only; a text frame is an
                    // out-of-band error message from the gateway.
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
    ///
    /// `pcm` is PCM16 LE 16 kHz mono of any length; the coordinator's 32 ms
    /// chunks accumulate in `pending` and every full `AUDIO_PACKET_BYTES`
    /// goes out as one gzip audio-only frame (roughly every third or fourth
    /// chunk). The remainder stays buffered until the next call or
    /// `finish`, so audio is never dropped or reordered. Fails with
    /// `BrokenPipe` once the writer task has exited.
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

    /// Next event in arrival order: the local `Ready` first, then tracker
    /// events and server errors. `Err` is terminal: `ConnectionAborted`
    /// (socket error), `UnexpectedEof` (close frame, EOF, or reader task
    /// gone), `InvalidData` (undecodable frame).
    pub async fn next_event(&self) -> io::Result<RealtimeEvent> {
        self.events.lock().await.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "doubao event task closed")
        })?
    }

    /// Flush the buffered tail as the last packet.
    ///
    /// Sent at most once; later calls return `Ok(())`. The tail may be
    /// shorter than a packet or empty: the last-packet flag is what makes
    /// the server emit its final response, which `next_event` then
    /// delivers as `Finished`. Does not close the socket.
    pub async fn finish(&mut self) -> io::Result<()> {
        if self.finish_sent {
            return Ok(());
        }
        self.finish_sent = true;
        let tail = std::mem::take(&mut *self.pending.lock().await);
        self.send(Command::Audio(encode_audio(&tail, true))).await
    }

    /// Queue a WebSocket Close frame; packets already queued go out first,
    /// then the writer task exits. Buffered tail audio is discarded. Fails
    /// with `BrokenPipe` if the writer task is already gone.
    pub async fn cancel(&self) -> io::Result<()> {
        self.send(Command::Close).await
    }

    /// Hand one command to the writer task (awaits while 64 are queued).
    async fn send(&self, command: Command) -> io::Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "doubao writer closed"))
    }
}

/// Render a server error for the session's `Error` event: `code` 0 means
/// "no numeric code" (gateway text frames); the `X-Tt-Logid` of the
/// connection is appended when known so users can quote it to support.
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

/// 16 random bytes from the OS as 32 lowercase hex chars (the per-connection
/// `X-Api-Connect-Id`). Panics only if the OS random source is unavailable.
fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// tungstenite errors after the upgrade all count as an aborted connection.
fn socket_error(error: tokio_tungstenite::tungstenite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
}
