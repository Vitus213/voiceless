//! Contract tests for the Volcengine (Doubao) streaming transport against a
//! fake OpenSpeech server: handshake headers, the gzip full client request,
//! 100 ms audio packets, the last-packet flag, and the mapping of
//! utterances onto session events.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use voiceless::config::{AppConfig, DoubaoConfig};
use voiceless::doubao::{
    decode_server_packet, encode_audio, DoubaoSession, ResultTracker, ServerPacket,
    AUDIO_PACKET_BYTES,
};
use voiceless::realtime::RealtimeEvent;

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut out).unwrap();
    out
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// Client frame as seen by the server: (message type, flags, payload).
fn parse_client_frame(raw: &[u8]) -> (u8, u8, Vec<u8>) {
    assert_eq!(raw[0], 0x11, "protocol v1, one-word header");
    let size = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let payload = &raw[8..8 + size];
    let payload = if raw[2] & 0x0F == 1 {
        gunzip(payload)
    } else {
        payload.to_vec()
    };
    (raw[1] >> 4, raw[1] & 0x0F, payload)
}

/// Server response frame with a sequence number; `last` sets the negative
/// sequence flag.
fn server_response(sequence: i32, payload: Value, last: bool) -> Message {
    let body = gzip(payload.to_string().as_bytes());
    let flags = if last { 0b0011 } else { 0b0001 };
    let mut frame = vec![0x11, (0b1001 << 4) | flags, 0x11, 0];
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Message::Binary(frame.into())
}

fn utterance(text: &str, definite: bool) -> Value {
    json!({ "text": text, "definite": definite })
}

fn config(endpoint: String) -> DoubaoConfig {
    DoubaoConfig {
        endpoint,
        app_key: "app-123".to_owned(),
        access_key: "token-456".to_owned(),
        hotwords: vec!["Voiceless".to_owned()],
        ..DoubaoConfig::default()
    }
}

// tungstenite fixes the handshake callback signature, including its error type.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn streams_packets_and_maps_utterances_to_events() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let headers = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let seen = headers.clone();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket =
            tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
                let mut seen = seen.lock().unwrap();
                for (name, value) in request.headers() {
                    seen.push((name.to_string(), value.to_str().unwrap().to_owned()));
                }
                Ok(response)
            })
            .await
            .unwrap();

        let (kind, _, request) = match socket.next().await.unwrap().unwrap() {
            Message::Binary(raw) => parse_client_frame(&raw),
            other => panic!("expected binary full request, got {other:?}"),
        };
        assert_eq!(kind, 0b0001);
        let request: Value = serde_json::from_slice(&request).unwrap();

        // Audio arrives in 100 ms packets; the tail goes out with the last flag.
        let mut packets = Vec::new();
        loop {
            let Message::Binary(raw) = socket.next().await.unwrap().unwrap() else {
                continue;
            };
            let (kind, flags, pcm) = parse_client_frame(&raw);
            assert_eq!(kind, 0b0010);
            packets.push((flags, pcm.len()));
            match packets.len() {
                1 => {
                    socket
                        .send(server_response(1, json!({"result": {"text": "你好", "utterances": [utterance("你好", false)]}}), false))
                        .await
                        .unwrap();
                    socket
                        .send(server_response(2, json!({"result": {"text": "你好世界。", "utterances": [utterance("你好世界。", true), utterance("再", false)]}}), false))
                        .await
                        .unwrap();
                }
                _ if flags == 0b0010 => break,
                _ => {}
            }
        }
        socket
            .send(server_response(
                -3,
                json!({"result": {"text": "你好世界。再见。"}}),
                true,
            ))
            .await
            .unwrap();
        (request, packets)
    });

    let mut session = DoubaoSession::connect(&config(endpoint)).await.unwrap();
    assert_eq!(session.next_event().await.unwrap(), RealtimeEvent::Ready);
    // 2.5 packets of audio in capture-sized chunks.
    for _ in 0..(AUDIO_PACKET_BYTES * 5 / 2 / 1_024) {
        session.send_audio(&[0_u8; 1_024]).await.unwrap();
    }
    session.send_audio(&[0_u8; 512]).await.unwrap();

    assert_eq!(
        session.next_event().await.unwrap(),
        RealtimeEvent::Partial {
            text: "你好".to_owned(),
            stash: String::new()
        }
    );
    assert_eq!(
        session.next_event().await.unwrap(),
        RealtimeEvent::Completed {
            transcript: "你好世界。".to_owned()
        }
    );
    assert_eq!(
        session.next_event().await.unwrap(),
        RealtimeEvent::Partial {
            text: "再".to_owned(),
            stash: String::new()
        }
    );
    session.finish().await.unwrap();
    assert_eq!(
        session.next_event().await.unwrap(),
        RealtimeEvent::Finished {
            transcript: "你好世界。再见。".to_owned()
        }
    );

    let (request, packets) = server.await.unwrap();
    let total = AUDIO_PACKET_BYTES * 5 / 2 / 1_024 * 1_024 + 512;
    assert_eq!(packets[0], (0b0000, AUDIO_PACKET_BYTES));
    assert_eq!(packets[1], (0b0000, AUDIO_PACKET_BYTES));
    assert_eq!(packets[2], (0b0010, total - 2 * AUDIO_PACKET_BYTES));

    assert_eq!(request["audio"]["rate"], 16_000);
    assert_eq!(request["audio"]["format"], "pcm");
    assert_eq!(request["request"]["model_name"], "bigmodel");
    assert_eq!(request["request"]["show_utterances"], true);
    assert_eq!(request["request"]["enable_punc"], true);
    let context: Value =
        serde_json::from_str(request["request"]["corpus"]["context"].as_str().unwrap()).unwrap();
    assert_eq!(context, json!({"hotwords": [{"word": "Voiceless"}]}));

    let headers = headers.lock().unwrap();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(header("x-api-app-key").as_deref(), Some("app-123"));
    assert_eq!(header("x-api-access-key").as_deref(), Some("token-456"));
    assert_eq!(
        header("x-api-resource-id").as_deref(),
        Some("volc.seedasr.sauc.duration")
    );
    assert!(header("x-api-key").is_none());
    assert_eq!(header("x-api-connect-id").map(|id| id.len()), Some(32));
}

#[tokio::test]
async fn server_error_frame_becomes_error_event() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let _request = socket.next().await;
        let message = b"invalid audio format";
        let mut frame = vec![0x11, 0b1111 << 4, 0x10, 0];
        frame.extend_from_slice(&45_000_151_u32.to_be_bytes());
        frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
        frame.extend_from_slice(message);
        socket.send(Message::Binary(frame.into())).await.unwrap();
        let _ = socket.next().await;
    });

    let session = DoubaoSession::connect(&config(endpoint)).await.unwrap();
    assert_eq!(session.next_event().await.unwrap(), RealtimeEvent::Ready);
    assert_eq!(
        session.next_event().await.unwrap(),
        RealtimeEvent::Error("doubao ASR error 45000151: invalid audio format".to_owned())
    );
}

#[test]
fn credentials_accept_api_key_or_legacy_pair() {
    // `has_credentials` accepts either the new-console key or the legacy pair.
    let mut config = DoubaoConfig::default();
    assert!(!config.has_credentials());
    config.api_key = "key".to_owned();
    assert!(config.has_credentials());
    let legacy = DoubaoConfig {
        app_key: "a".to_owned(),
        access_key: "b".to_owned(),
        ..DoubaoConfig::default()
    };
    assert!(legacy.has_credentials());
}

#[test]
fn audio_frame_round_trips_and_marks_last_packet() {
    let frame = encode_audio(&[1, 2, 3, 4], true);
    assert_eq!(frame[1], (0b0010 << 4) | 0b0010);
    let size = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;
    assert_eq!(gunzip(&frame[8..8 + size]), vec![1, 2, 3, 4]);
}

#[test]
fn decodes_uncompressed_response_without_sequence() {
    let body = br#"{"result":{"text":"hi"}}"#;
    let mut frame = vec![0x11, (0b1001 << 4) | 0b0010, 0x10, 0];
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    assert_eq!(
        decode_server_packet(&frame).unwrap(),
        ServerPacket::Response {
            payload: json!({"result": {"text": "hi"}}),
            last: true
        }
    );
}

#[test]
fn tracker_confirms_each_definite_utterance_once() {
    let mut tracker = ResultTracker::default();
    let first =
        json!({"result": {"utterances": [utterance("一。", true), utterance("二", false)]}});
    assert_eq!(
        tracker.events(&first, false),
        vec![
            RealtimeEvent::Completed {
                transcript: "一。".to_owned()
            },
            RealtimeEvent::Partial {
                text: "二".to_owned(),
                stash: String::new()
            },
        ]
    );
    // Same state again: nothing new to report.
    assert!(tracker.events(&first, false).is_empty());
    let second =
        json!({"result": {"utterances": [utterance("一。", true), utterance("二。", true)]}});
    assert_eq!(
        tracker.events(&second, false),
        vec![RealtimeEvent::Completed {
            transcript: "二。".to_owned()
        }]
    );
}

#[test]
fn doubao_backend_and_endpoint_are_validated() {
    assert!(AppConfig::from_toml("[asr]\nstreaming_backend = \"doubao-realtime\"\n").is_ok());
    assert!(AppConfig::from_toml("[doubao]\nendpoint = \"ws://insecure\"\n").is_err());
    let config =
        AppConfig::from_toml("[doubao]\napi_key = \"k\"\nhotwords = [\"豆包\"]\n").unwrap();
    assert_eq!(config.doubao.hotwords, vec!["豆包".to_owned()]);
}
