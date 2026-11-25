use super::*;
use crate::session_bus::private_bus;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "test-console-token";

struct HeldSession(Arc<AtomicUsize>);

impl SessionRunner for HeldSession {
    fn start(
        &mut self,
        _session_id: u64,
        _mode: &str,
        _inject: Option<bool>,
        _purpose: SessionPurpose,
        _context: crate::stream::DictationContext,
        mut control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            match control.recv().await {
                Some(ControlCommand::Cancel) => Ok(130),
                other => Err(format!("unexpected command {other:?}")),
            }
        })
    }
}

async fn request(
    port: u16,
    method: &'static str,
    path: &'static str,
    body: Option<Value>,
) -> (u16, Value) {
    request_with_token(port, Arc::from(TOKEN), method, path, body).await
}

/// `request` against a console that minted its own token.
async fn request_with_token(
    port: u16,
    token: Arc<str>,
    method: &'static str,
    path: &'static str,
    body: Option<Value>,
) -> (u16, Value) {
    tokio::task::spawn_blocking(move || {
        let client = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(3))
            .build();
        let request = client
            .request(method, &format!("http://127.0.0.1:{port}/api/{path}"))
            .set("X-Voiceless-Token", token.as_ref());
        let response = match body {
            Some(body) => request
                .set("Content-Type", "application/json")
                .send_string(&body.to_string()),
            None => request.call(),
        };
        let response = match response {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(error) => panic!("{method} {path}: {error}"),
        };
        (
            response.status(),
            serde_json::from_reader(response.into_reader()).unwrap(),
        )
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn concurrent_dbus_and_console_starts_share_one_session() {
    let (_bus, address) = private_bus().await;
    let connect = || async {
        zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    };
    let server = connect().await;
    let connection = connect().await;
    let root = tempfile::tempdir().expect("runtime directory");
    let config_path = root.path().join("config.toml");
    std::fs::write(&config_path, "[audio]\npause_media = false\n").unwrap();
    let config = Arc::new(crate::console::settings::load_revision(&config_path, true).unwrap());
    let (shared, commands) = DaemonShared::new(
        config_path.clone(),
        root.path().canonicalize().unwrap(),
        "quick".to_owned(),
        config,
    );
    let mut events = shared.events.subscribe();
    register_controller(&server, &shared).await.unwrap();

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let console = tokio::spawn(crate::console::serve_listener(
        listener,
        Some(shared.clone()),
        root.path().join("history.sqlite3"),
        config_path,
        TOKEN.to_owned(),
    ));
    let starts = Arc::new(AtomicUsize::new(0));
    let daemon = tokio::spawn(serve_with_gateway(
        Gateway::new(HeldSession(starts.clone())),
        shared.clone(),
        commands,
        server,
    ));
    let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .unwrap();
    let (dbus, (code, web)) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            proxy.call::<_, _, u64>("StartSessionWithId", &("quick", false)),
            request(port, "POST", "session/start", None),
        )
    })
    .await
    .expect("start replies");
    let id = match (dbus, code) {
        (Ok(id), 409) => {
            assert!(web["error"].as_str().unwrap().contains("recording"));
            id
        }
        (Err(error), 200) => {
            assert!(error.to_string().contains("recording"), "{error}");
            assert_eq!(web["outcome"], "started");
            web["session_id"].as_u64().unwrap()
        }
        (dbus, code) => panic!("expected one accepted start; D-Bus: {dbus:?}, HTTP {code}: {web}"),
    };
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    let mut sink = DaemonEventSink {
        shared,
        session_id: id,
        sequence: 0,
    };
    sink.emit(OutputEvent::BackendSelected(BackendSelection {
        backend: "local-offline".to_owned(),
        model: "qwen3-asr-1.7b-gguf".to_owned(),
        requested_device: Some("auto".to_owned()),
        fallback: None,
    }))
    .unwrap();
    use crate::offline_asr::{
        ComputeApi, ComputeDevice, EngineDevices, InferenceDevice, InferenceEngine,
    };
    sink.emit(OutputEvent::DevicesLoaded(Box::new(EngineDevices {
        primary: InferenceDevice {
            requested: ComputeDevice::Auto,
            engine: InferenceEngine::TranscribeCpp,
            api: ComputeApi::Vulkan,
            backend: "Vulkan0".to_owned(),
            name: Some("Vulkan0".to_owned()),
            description: Some("NVIDIA GeForce RTX 3070".to_owned()),
            device_id: None,
            memory_bytes: 0,
            cpu_fallback: None,
        },
        preview: Some(InferenceDevice::sherpa_cpu()),
    })))
    .unwrap();
    let (code, status) = request(port, "GET", "status", None).await;
    assert_eq!(code, 200);
    assert_eq!(status["session_id"], json!(id));
    assert_eq!(status["state"], "recording");
    let session = &status["session"];
    assert_eq!(session["backend_selection"]["backend"], "local-offline");
    assert_eq!(session["devices"]["primary"]["requested"], "auto");
    assert_eq!(session["devices"]["primary"]["api"], "vulkan");
    assert_eq!(session["devices"]["preview"]["api"], "cpu");

    let snapshot: Value = tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let response = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(3))
            .build()
            .get(&format!("http://127.0.0.1:{port}/api/events"))
            .set("X-Voiceless-Token", TOKEN)
            .call()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(response.into_reader())
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str(line.strip_prefix("data: ").unwrap()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["status"]["runtime_id"], status["runtime_id"]);
    assert_eq!(snapshot["status"]["session"]["id"], json!(id));
    assert_eq!(snapshot["status"]["session"]["devices"], session["devices"]);
    assert!(
        snapshot["status"]["event_sequence"].as_u64().unwrap()
            >= status["event_sequence"].as_u64().unwrap()
    );

    let initial = &status["config_apply"]["active_revision"];
    assert_eq!(&session["config_revision"], initial);
    let (code, first_edit) = request(
        port,
        "PUT",
        "config",
        Some(json!({
            "revision": initial, "fields": {"local.num_threads": 6}
        })),
    )
    .await;
    assert_eq!(code, 200, "{first_edit}");
    let (code, queued) = request(
        port,
        "POST",
        "engine/reload",
        Some(json!({
            "revision": first_edit["revision"]
        })),
    )
    .await;
    assert_eq!(code, 200, "{queued}");
    assert_eq!(queued["config_apply"]["state"], "queued");
    let (_, during) = request(port, "GET", "status", None).await;
    assert_eq!(during["state"], "recording");
    assert_eq!(&during["config_apply"]["active_revision"], initial);
    assert_eq!(&during["session"]["config_revision"], initial);

    let (code, next_edit) = request(
        port,
        "PUT",
        "config",
        Some(json!({
            "revision": first_edit["revision"], "fields": {"local.num_threads": 8}
        })),
    )
    .await;
    assert_eq!(code, 200, "{next_edit}");

    let (code, _) = request(
        port,
        "POST",
        "session/cancel",
        Some(json!({"runtime_id": "old-runtime", "session_id": id})),
    )
    .await;
    assert_eq!(code, 409);

    let stale = proxy
        .call::<_, _, ()>("StopSession", &(id + 1,))
        .await
        .unwrap_err();
    assert!(stale.to_string().contains("no longer active"));
    proxy
        .call::<_, _, ()>("CancelSession", &(id,))
        .await
        .unwrap();
    let idle = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event: Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
            if event["type"] == "state" && event["state"] == "idle" {
                break event;
            }
        }
    })
    .await
    .expect("session returned to idle");
    assert_eq!(idle["session_id"], json!(id));

    let failed = wait_for_apply(&mut events, "failed").await;
    assert_eq!(&failed["active_revision"], initial);
    assert_eq!(failed["target_revision"], first_edit["revision"]);
    assert!(failed["error"].as_str().unwrap().contains("newer revision"));

    let (code, receipt) = request(
        port,
        "POST",
        "engine/reload",
        Some(json!({
            "revision": next_edit["revision"]
        })),
    )
    .await;
    assert_eq!(code, 200, "{receipt}");
    let applied = wait_for_apply(&mut events, "applied").await;
    assert_eq!(applied["active_revision"], next_edit["revision"]);

    let (code, invalid) = request(
        port,
        "PUT",
        "config",
        Some(json!({
            "revision": next_edit["revision"], "fields": {
                "asr.streaming_backend": "local-streaming",
                "asr.local_model_dir": root.path().join("missing-paraformer"),
            }
        })),
    )
    .await;
    assert_eq!(code, 200, "{invalid}");
    let (code, _) = request(
        port,
        "POST",
        "engine/reload",
        Some(json!({
            "revision": invalid["revision"]
        })),
    )
    .await;
    assert_eq!(code, 200);
    let failed = wait_for_apply(&mut events, "failed").await;
    assert_eq!(failed["active_revision"], applied["active_revision"]);
    assert_eq!(failed["target_revision"], invalid["revision"]);
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("missing-paraformer"),
        "{failed}"
    );

    let (code, started) = request(port, "POST", "session/start", None).await;
    assert_eq!(code, 200, "{started}");
    let (_, status) = request(port, "GET", "status", None).await;
    assert_eq!(status["session"]["config_revision"], next_edit["revision"]);
    assert_eq!(status["backend"], "cloud-realtime");
    let (code, _) = request(
        port,
        "POST",
        "config/restore",
        Some(json!({
            "revision": first_edit["revision"]
        })),
    )
    .await;
    assert_eq!(code, 409);
    let (code, _) = request(
        port,
        "POST",
        "config/restore",
        Some(json!({
            "revision": invalid["revision"]
        })),
    )
    .await;
    assert_eq!(code, 200);
    let (_, saved) = request(port, "GET", "config", None).await;
    assert_eq!(saved["revision"], next_edit["revision"]);
    assert_eq!(saved["local"]["num_threads"], 8);
    assert_eq!(saved["asr"]["streaming_backend"], "cloud-realtime");
    let (code, _) = request(port, "POST", "session/cancel", None).await;
    assert_eq!(code, 200);
    console.abort();
    daemon.abort();
}

async fn wait_for_apply(
    events: &mut tokio::sync::broadcast::Receiver<String>,
    state: &str,
) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event: Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
            if event["config_apply"]["state"] == state {
                return event["config_apply"].clone();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("apply did not reach {state}"))
}

#[tokio::test]
async fn release_cannot_stop_a_session_started_after_the_hold_was_cancelled() {
    use crate::hotkey::{Action, KeyEvent, Transition};

    let mut gateway = Gateway::new(HeldSession(Arc::new(AtomicUsize::new(0))));
    let mut gestures = shortcuts::Gestures::new(&crate::config::HotkeyConfig::default());
    let press = KeyEvent {
        action: Action::Hold,
        transition: Transition::Press { ctrl: false },
        at: std::time::Instant::now(),
    };
    let Some(DaemonCommand::Start { mode, .. }) = gestures.command(press, &gateway) else {
        panic!("hold must start a session");
    };
    gateway.start(&mode).unwrap();
    let held_session = gateway.session_id();
    gateway.cancel().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while gateway.poll().await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    gateway.start("quick").unwrap();
    let release = KeyEvent {
        transition: Transition::Release,
        ..press
    };
    assert_ne!(gateway.session_id(), held_session);
    assert!(gestures.command(release, &gateway).is_none());
    assert_eq!(gateway.state(), State::Recording);
    gateway.cancel().await;
}

/// A daemon over a private bus whose console took a free port: the bus
/// process and the runtime directory live as long as this does.
struct Running {
    _bus: tokio::process::Child,
    _root: tempfile::TempDir,
    shared: Arc<DaemonShared>,
    connection: zbus::Connection,
    port: u16,
    /// The token the console's page carries.
    token: Arc<str>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

async fn running_daemon<R: SessionRunner + Send + 'static>(runner: R) -> Running {
    let (bus, address) = private_bus().await;
    let connect = || async {
        zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    };
    let server = connect().await;
    let connection = connect().await;
    let root = tempfile::tempdir().expect("runtime directory");
    let config_path = root.path().join("config.toml");
    std::fs::write(&config_path, "[audio]\npause_media = false\n").unwrap();
    let config = Arc::new(crate::console::settings::load_revision(&config_path, true).unwrap());
    let (shared, commands) = DaemonShared::new(
        config_path,
        root.path().canonicalize().unwrap(),
        "quick".to_owned(),
        config,
    );
    register_controller(&server, &shared).await.unwrap();
    // The gateway takes the bus name; the console then publishes where it listens.
    let task = tokio::spawn(serve_with_gateway(
        Gateway::new(runner),
        shared.clone(),
        commands,
        server,
    ));
    let console_shared = shared.clone();
    tokio::spawn(async move { crate::console::serve_daemon(console_shared, "127.0.0.1", 0).await });
    let url = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(url) = shared.console_url() {
                break url;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the console publishes its address");
    let port: u16 = url
        .trim_start_matches("http://127.0.0.1:")
        .trim_end_matches('/')
        .parse()
        .unwrap();
    // The console mints its own token; its page carries it.
    let token: Arc<str> = tokio::task::spawn_blocking(move || {
        let page = ureq::get(&url).call().unwrap().into_string().unwrap();
        let meta = "name=\"voiceless-token\" content=\"";
        let start = page.find(meta).expect("the token meta tag") + meta.len();
        let end = start + page[start..].find('"').expect("the token's closing quote");
        Arc::from(page[start..end].to_owned())
    })
    .await
    .unwrap();
    Running {
        _bus: bus,
        _root: root,
        shared,
        connection,
        port,
        token,
        task,
    }
}

/// The first published event of the given type within three seconds.
async fn event_of(events: &mut tokio::sync::broadcast::Receiver<String>, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let line = events.recv().await.unwrap();
            let event: Value = serde_json::from_str(&line).unwrap();
            if event["type"] == kind {
                break event;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no `{kind}` event"))
}

/// The console's status once the daemon is `idle` again, within three seconds.
async fn idle_status(port: u16, token: Arc<str>) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (code, status) =
                request_with_token(port, token.clone(), "GET", "status", None).await;
            assert_eq!(code, 200);
            if status["state"] == "idle" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the cancelled session returns to idle")
}

/// Pausing leaves the session in progress alone but closes the gate to
/// every start -- D-Bus, the console, and so the hotkey -- with one
/// reason; the console hears of it at once, resuming opens the gate
/// again, and D-Bus tells where the console listens.
#[tokio::test]
async fn pausing_refuses_every_start_until_resumed() {
    let starts = Arc::new(AtomicUsize::new(0));
    let running = running_daemon(HeldSession(starts.clone())).await;
    // Borrowed whole: the ignored fields keep the bus process and the runtime directory alive.
    let Running {
        shared,
        connection,
        port,
        token,
        task,
        ..
    } = &running;
    let proxy = zbus::Proxy::new(connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .unwrap();
    let mut events = shared.events.subscribe();

    // D-Bus tells where the console listens.
    let url: String = proxy.call("ConsoleUrl", &()).await.unwrap();
    assert_eq!(url, format!("http://127.0.0.1:{port}/"));
    assert_eq!(shared.console_url().as_deref(), Some(url.as_str()));

    // A pause leaves the session in progress alone, and the console hears of it.
    proxy.call::<_, _, String>("Toggle", &("",)).await.unwrap();
    assert_eq!(starts.load(Ordering::Relaxed), 1);
    let reply: String = proxy.call("SetPaused", &(true,)).await.unwrap();
    assert_eq!(reply, "paused");
    let event = event_of(&mut events, "paused").await;
    assert_eq!(
        (event["type"].as_str(), event["paused"].as_bool()),
        (Some("paused"), Some(true))
    );
    let (code, status) = request_with_token(*port, token.clone(), "GET", "status", None).await;
    assert_eq!(
        (code, status["state"].as_str(), status["paused"].as_bool()),
        (200, Some("recording"), Some(true))
    );
    proxy.call::<_, _, String>("Cancel", &()).await.unwrap();
    assert_eq!(idle_status(*port, token.clone()).await["paused"], true);

    // Every start is refused with the same reason.
    let refused = proxy
        .call::<_, _, String>("Toggle", &("",))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("dictation is paused"),
        "{refused}"
    );
    let (code, web) = request_with_token(
        *port,
        token.clone(),
        "POST",
        "session/start",
        Some(json!({"mode": "quick"})),
    )
    .await;
    assert_eq!(code, 409);
    assert!(web["error"]
        .as_str()
        .unwrap()
        .contains("dictation is paused"));
    assert_eq!(starts.load(Ordering::Relaxed), 1);

    // Resuming opens the gate again.
    let reply: String = proxy.call("SetPaused", &(false,)).await.unwrap();
    assert_eq!(reply, "resumed");
    proxy.call::<_, _, String>("Toggle", &("",)).await.unwrap();
    assert_eq!(starts.load(Ordering::Relaxed), 2);
    task.abort();
}

/// A second runtime on the same bus is refused, and the running one keeps
/// its name: clients are never handed to a runtime they did not start.
#[tokio::test]
async fn a_second_runtime_does_not_take_the_bus_name() {
    let (_bus, address) = private_bus().await;
    let connect = || async {
        zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    };
    let running = connect().await;
    let second = connect().await;
    let root = tempfile::tempdir().expect("runtime directory");
    let config_path = root.path().join("config.toml");
    std::fs::write(&config_path, "[audio]\npause_media = false\n").unwrap();
    let config = Arc::new(crate::console::settings::load_revision(&config_path, true).unwrap());
    let data_dir = root.path().canonicalize().unwrap();
    let (first, _commands) = DaemonShared::new(
        config_path.clone(),
        data_dir.clone(),
        "quick".to_owned(),
        config.clone(),
    );
    let (other, _commands) = DaemonShared::new(config_path, data_dir, "quick".to_owned(), config);
    register_controller(&running, &first).await.unwrap();

    let refused = register_controller(&second, &other).await.unwrap_err();
    assert!(
        refused.contains("another Voiceless runtime already owns"),
        "{refused}"
    );

    // The running one still owns the name.
    let dbus = zbus::fdo::DBusProxy::new(&running).await.unwrap();
    let name = zbus::names::BusName::try_from(BUS_NAME).unwrap();
    let owner = dbus.get_name_owner(name).await.unwrap();
    assert_eq!(
        owner.to_string(),
        running.unique_name().unwrap().to_string()
    );
}
