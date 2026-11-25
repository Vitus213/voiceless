use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use super::run_trial;
use crate::benchmark_cmd::replay_cloud;
use crate::config::AppConfig;
use crate::latency::TrialStage;

#[derive(Clone, Copy, Debug)]
enum Reply {
    RejectUpdate,
    RejectAudio,
    RejectFinish,
    CloseAfterSegment,
    Finish(&'static str),
}

async fn serve(listener: &TcpListener, reply: Reply) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    while let Some(frame) = socket.next().await {
        let command: Value = serde_json::from_str(frame.unwrap().to_text().unwrap()).unwrap();
        let response = match (command["type"].as_str().unwrap(), reply) {
            ("session.update", Reply::RejectUpdate)
            | ("input_audio_buffer.append", Reply::RejectAudio) => {
                json!({"type": "error", "message": "request rejected"})
            }
            ("session.update", _) => json!({"type": "session.updated"}),
            ("input_audio_buffer.append", _) => continue,
            ("session.finish", _) => {
                for transcript in ["first", " second"] {
                    let segment = json!({
                        "type": "conversation.item.input_audio_transcription.completed",
                        "transcript": transcript,
                    });
                    socket
                        .send(Message::Text(segment.to_string().into()))
                        .await
                        .unwrap();
                }
                match reply {
                    Reply::RejectFinish => json!({"type": "error", "message": "request rejected"}),
                    Reply::CloseAfterSegment => {
                        socket.close(None).await.unwrap();
                        return;
                    }
                    Reply::Finish(text) => json!({"type": "session.finished", "transcript": text}),
                    _ => unreachable!(),
                }
            }
            (kind, _) => panic!("unexpected client event {kind}"),
        };
        let ready = response["type"] == "session.updated";
        socket
            .send(Message::Text(response.to_string().into()))
            .await
            .unwrap();
        if !ready {
            return;
        }
    }
    panic!("client disconnected before scripted response");
}

#[tokio::test]
async fn errors_fail_both_benchmarks_and_only_finished_commits_text() {
    for (reply, expected) in [
        (Reply::RejectUpdate, Err("request rejected")),
        (Reply::RejectAudio, Err("request rejected")),
        (Reply::RejectFinish, Err("request rejected")),
        (Reply::CloseAfterSegment, Err("realtime socket closed")),
        (Reply::Finish("authoritative"), Ok("authoritative")),
        (Reply::Finish(""), Ok("first second")),
    ] {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let mut config = AppConfig::default();
        config.cloud.realtime_endpoint = format!("ws://{}/", listener.local_addr().unwrap());
        config.cloud.api_key = "test-only".into();
        let pcm = [0; crate::capture::CHUNK_BYTES];
        tokio::time::timeout(Duration::from_secs(5), async {
            let (quality, ()) = tokio::join!(
                replay_cloud(&config, &pcm, Duration::ZERO),
                serve(&listener, reply),
            );
            assert_eq!(
                quality.as_deref().map_err(String::as_str),
                expected,
                "{reply:?}"
            );
            let (trial, ()) = tokio::join!(
                run_trial(0, &config, "sample", &pcm, false),
                serve(&listener, reply)
            );
            assert_eq!(trial.error.as_deref(), expected.err(), "{reply:?}");
            assert_eq!(trial.success, expected.is_ok(), "{reply:?}");
            let received_final = trial
                .stages
                .iter()
                .any(|(stage, _)| *stage == TrialStage::FinalReceived);
            assert_eq!(received_final, expected.is_ok(), "{reply:?}");
        })
        .await
        .expect("loopback benchmarks timed out");
    }
}
