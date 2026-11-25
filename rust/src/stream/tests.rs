use super::*;
use base64::Engine as _;
use flate2::read::GzDecoder;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::Read;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};

struct PcmCapture(Option<Vec<u8>>);

impl AudioCapture for PcmCapture {
    async fn start(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self.0.take() {
            Some(pcm) => Ok(Some(pcm)),
            None => std::future::pending().await,
        }
    }

    async fn stop_capture(&mut self) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

struct Events(mpsc::UnboundedSender<OutputEvent>);

impl EventSink for Events {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        self.0.send(event).map_err(io::Error::other)
    }
}

async fn listen() -> (TcpListener, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    (listener, endpoint)
}

async fn accept(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (socket, _) = listener.accept().await.unwrap();
    accept_async(socket).await.unwrap()
}

async fn send_json(socket: &mut WebSocketStream<TcpStream>, body: Value) {
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .unwrap();
}

fn cloud_config(primary: String, backup: String) -> AppConfig {
    let mut config = AppConfig::default();
    config.asr.streaming_backend = "cloud-realtime".to_owned();
    config.asr.fallback_backend = Some("doubao-realtime".to_owned());
    config.cloud.realtime_endpoint = primary;
    config.cloud.api_key = "test-key".to_owned();
    config.doubao.endpoint = backup;
    config.doubao.api_key = "test-key".to_owned();
    config.history.enabled = false;
    config
}

async fn run(
    config: &AppConfig,
    control: mpsc::Receiver<ControlCommand>,
    pcm: Vec<u8>,
) -> (i32, Vec<OutputEvent>) {
    let options = StreamOptions {
        config_path: None,
        backend: None,
        json: true,
        inject: false,
        mode: "quick".to_owned(),
        context: Default::default(),
    };
    let mut isolated = config.clone();
    isolated.history.enabled = false;
    let pipeline = build_pipeline(&isolated, &options, "quick").await.unwrap();
    let (sender, mut events) = mpsc::unbounded_channel();
    let env = SessionEnv {
        warm: None,
        sink: Some(Box::new(Events(sender))),
    };
    let exit = run_configured(
        config,
        &options,
        control,
        env,
        pipeline,
        PcmCapture(Some(pcm)),
    )
    .await
    .unwrap();
    let mut output = Vec::new();
    while let Ok(event) = events.try_recv() {
        output.push(event);
    }
    (exit, output)
}

#[tokio::test]
async fn startup_rejection_sends_audio_only_to_the_configured_fallback() {
    let (primary, primary_endpoint) = listen().await;
    let (backup, backup_endpoint) = listen().await;
    let config = cloud_config(primary_endpoint, backup_endpoint);
    let (stop, control) = mpsc::channel(1);
    let pcm = vec![42; crate::doubao::AUDIO_PACKET_BYTES];
    let primary = async {
        let mut socket = accept(&primary).await;
        send_json(&mut socket, json!({"type": "session.created"})).await;
        let mut appended = 0;
        while let Some(Ok(message)) = socket.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let body: Value = serde_json::from_str(&text).unwrap();
            match body["type"].as_str().unwrap() {
                "session.update" => {
                    send_json(
                        &mut socket,
                        json!({"type": "error", "message": "startup rejected"}),
                    )
                    .await;
                }
                "input_audio_buffer.append" => appended += 1,
                kind => panic!("unexpected primary request {kind}"),
            }
        }
        appended
    };
    let backup = async {
        let mut socket = accept(&backup).await;
        let Message::Binary(request) = socket.next().await.unwrap().unwrap() else {
            panic!("expected Doubao setup frame");
        };
        assert_eq!(request[1], 0x10);
        let mut audio = Vec::new();
        while let Some(Ok(Message::Binary(packet))) = socket.next().await {
            assert_eq!(packet[1] >> 4, 2);
            GzDecoder::new(&packet[8..])
                .read_to_end(&mut audio)
                .unwrap();
            if packet[1] & 2 != 0 {
                break;
            }
            stop.send(ControlCommand::Stop).await.unwrap();
        }
        let body = br#"{"result":{"text":"fallback result"}}"#;
        let mut response = vec![0x11, 0x92, 0x10, 0];
        response.extend_from_slice(&(body.len() as u32).to_be_bytes());
        response.extend_from_slice(body);
        socket.send(Message::Binary(response.into())).await.unwrap();
        audio
    };
    let ((exit, events), primary_audio_count, backup_audio) =
        timeout(Duration::from_secs(8), async {
            tokio::join!(run(&config, control, pcm.clone()), primary, backup)
        })
        .await
        .expect("primary and backup sessions terminate");
    assert_eq!(exit, 0, "{events:?}");
    assert_eq!(primary_audio_count, 0);
    assert_eq!(backup_audio, pcm);
    let selection = events
        .iter()
        .find_map(|event| match event {
            OutputEvent::BackendSelected(selection) => Some(selection),
            _ => None,
        })
        .expect("selected backend recorded");
    assert_eq!(selection.backend, "doubao-realtime");
    let fallback = selection.fallback.as_ref().expect("fallback recorded");
    assert_eq!(fallback.primary, "cloud-realtime");
    assert!(fallback.reason.contains("startup rejected"));
    assert!(events.iter().any(|event| matches!(event, OutputEvent::Warning(message) if message.contains("startup rejected") && message.contains("doubao-realtime"))));
    assert!(events.iter().any(|event| matches!(event, OutputEvent::Transcript(result) if result.is_final && result.backend == "doubao-realtime")));
}

#[tokio::test]
async fn disconnect_after_audio_never_opens_the_backup() {
    let (primary, primary_endpoint) = listen().await;
    let (backup, backup_endpoint) = listen().await;
    let config = cloud_config(primary_endpoint, backup_endpoint);
    let (_control, receiver) = mpsc::channel(1);
    let pcm = vec![13; 1024];
    let server = async {
        let mut socket = accept(&primary).await;
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            Message::Text(_)
        ));
        send_json(&mut socket, json!({"type": "session.updated"})).await;
        let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
            panic!("expected an audio append");
        };
        let append: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(append["type"], "input_audio_buffer.append");
        send_json(
            &mut socket,
            json!({"type": "conversation.item.input_audio_transcription.text", "text": "partial"}),
        )
        .await;
        socket.close(None).await.unwrap();
        base64::engine::general_purpose::STANDARD
            .decode(append["audio"].as_str().unwrap())
            .unwrap()
    };
    let ((exit, events), primary_audio) = timeout(Duration::from_secs(3), async {
        tokio::join!(run(&config, receiver, pcm.clone()), server)
    })
    .await
    .expect("mid-session disconnect reported promptly");
    assert_eq!(exit, 1, "{events:?}");
    assert_eq!(primary_audio, pcm);
    assert!(events
        .iter()
        .any(|event| matches!(event, OutputEvent::Error(_))));
    assert!(!events.iter().any(|event| matches!(
        event,
        OutputEvent::Finalized { .. } | OutputEvent::Warning(_)
    )));
    assert!(timeout(Duration::from_millis(20), backup.accept())
        .await
        .is_err());
}

#[tokio::test]
async fn missing_local_model_cannot_send_to_an_unselected_cloud_service() {
    let (cloud, cloud_endpoint) = listen().await;
    let (backup, backup_endpoint) = listen().await;
    let mut config = cloud_config(cloud_endpoint, backup_endpoint);
    let root = tempfile::tempdir().unwrap();
    config.asr.streaming_backend = "local-streaming".to_owned();
    config.asr.local_model_dir = Some(root.path().join("nonexistent-model"));
    config.asr.fallback_backend = None;
    let (_control, receiver) = mpsc::channel(1);
    let (exit, events) = timeout(
        Duration::from_secs(3),
        run(&config, receiver, vec![12; 1024]),
    )
    .await
    .expect("local failure reported promptly");
    assert_eq!(exit, 1, "{events:?}");
    assert!(matches!(
        &events[..],
        [
            OutputEvent::Output(_),
            OutputEvent::Error(_),
            OutputEvent::Completed
        ]
    ));
    assert!(timeout(Duration::from_millis(20), cloud.accept())
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(20), backup.accept())
        .await
        .is_err());
}
