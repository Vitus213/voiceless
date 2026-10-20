use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use voiceless::realtime::{RealtimeEvent, RealtimeSession};

#[tokio::test]
async fn streams_pcm_and_exposes_partial_and_final_events() {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test server");
    let endpoint = format!("ws://{}/", listener.local_addr().expect("local address"));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept client");
        let mut socket = accept_async(stream).await.expect("websocket handshake");

        let update = next_json(&mut socket).await;
        assert_eq!(update["type"], "session.update");
        assert_eq!(update["session"]["modalities"], json!(["text"]));
        assert_eq!(update["session"]["input_audio_format"], "pcm");
        assert_eq!(update["session"]["sample_rate"], 16_000);
        socket
            .send(Message::Text(
                json!({"type": "session.updated"}).to_string().into(),
            ))
            .await
            .expect("send ready");

        let append = next_json(&mut socket).await;
        assert_eq!(append["type"], "input_audio_buffer.append");

        socket
            .send(Message::Text(
                json!({
                    "type": "conversation.item.input_audio_transcription.text",
                    "text": "你好",
                    "stash": "世界"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send partial");
        socket
            .send(Message::Text(
                json!({
                    "type": "conversation.item.input_audio_transcription.completed",
                    "transcript": "你好世界"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send completed");

        let finish = next_json(&mut socket).await;
        assert_eq!(finish["type"], "session.finish");
        socket
            .send(Message::Text(
                json!({
                    "type": "session.finished",
                    "transcript": "你好世界"
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send final");

        append["audio"].as_str().expect("base64 audio").to_owned()
    });

    let mut session = RealtimeSession::connect(&endpoint, "sk-test", "qwen3-asr-flash-realtime")
        .await
        .expect("connect realtime session");
    assert_eq!(
        session.next_event().await.expect("ready event"),
        RealtimeEvent::Ready
    );

    let pcm = [0_u8, 1, 2, 253, 254, 255];
    session.send_audio(&pcm).await.expect("send pcm");
    assert_eq!(
        session.next_event().await.expect("partial event"),
        RealtimeEvent::Partial {
            text: "你好".to_owned(),
            stash: "世界".to_owned(),
        }
    );
    assert_eq!(
        session.next_event().await.expect("completed event"),
        RealtimeEvent::Completed {
            transcript: "你好世界".to_owned(),
        }
    );

    session.finish().await.expect("finish realtime session");
    assert_eq!(
        session.next_event().await.expect("finished event"),
        RealtimeEvent::Finished {
            transcript: "你好世界".to_owned(),
        }
    );

    let encoded = server.await.expect("server task");
    assert_eq!(encoded, "AAEC/f7/");
}

async fn next_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match socket.next().await.expect("websocket message") {
            Ok(Message::Text(text)) => return serde_json::from_str(&text).expect("JSON message"),
            Ok(Message::Ping(payload)) => socket
                .send(Message::Pong(payload))
                .await
                .expect("pong response"),
            Ok(other) => panic!("unexpected websocket message: {other:?}"),
            Err(error) => panic!("websocket error: {error}"),
        }
    }
}
