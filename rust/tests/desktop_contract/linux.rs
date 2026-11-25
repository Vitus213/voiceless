//! Desktop capabilities through the embedded console with controllable OS
//! probes, and the desktop host's contract with a running runtime
//! (`voiceless::desktop::{lifecycle, runtime}`): the window reuses the
//! daemon however often it opens, the tray's actions reach it, and what
//! closing or quitting does follows the desktop and the daemon's state.

use crate::session_bus::private_bus;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use voiceless::desktop::lifecycle::{self, Autostart, Close, Quit};
use voiceless::desktop::runtime::{self as host, RuntimeError};

#[tokio::test]
async fn microphone_default_and_explicit_selection_survive_device_changes() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(&config, "# microphone test\n").unwrap();
    let graph = root.path().join("graph.json");
    let write_probe = |name, program| {
        let path = root.path().join(name);
        std::fs::write(&path, program).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    write_probe(
        "pw-dump",
        "#!/bin/sh\ncat \"$VOICELESS_TEST_AUDIO_GRAPH\"\n",
    );
    write_probe(
        "wayland-info",
        "#!/bin/sh\nprintf \"interface: 'wl_compositor', version: 6, name: 1\\n\"\n",
    );
    let node = |id, name| {
        json!({"id": id, "type": "PipeWire:Interface:Node", "info": {"props": {
            "media.class": "Audio/Source", "object.serial": id, "node.name": name, "node.description": name,
        }}})
    };
    let graph_file = graph.clone();
    let update_graph = move |default, usb_connected| {
        let mut objects = vec![node(14, "headset")];
        if usb_connected {
            objects.push(node(13, "usb"));
        }
        objects.push(json!({"id": 4, "type": "PipeWire:Interface:Metadata", "props": {"metadata.name": "default"},
            "metadata": [{"subject": 0, "key": "default.audio.source", "type": "Spa:String:JSON", "value": {"name": default}}]}));
        std::fs::write(&graph_file, json!(objects).to_string()).unwrap();
    };
    update_graph("usb", true);
    let path = std::env::join_paths(std::iter::once(root.path().to_owned()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_voiceless"))
        .arg("--config")
        .arg(&config)
        .args(["web", "--no-open", "--port", "0"])
        .env("VOICELESS_TEST_AUDIO_GRAPH", &graph)
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("WAYLAND_DISPLAY", "desktop-contract")
        .env("PATH", path)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(process.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let url = line.strip_prefix("Voiceless console: ").unwrap().to_owned();
    tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .timeout(Duration::from_secs(3))
            .build();
        let page = agent.get(&url).call().unwrap().into_string().unwrap();
        let token = page
            .split_once("name=\"voiceless-token\" content=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap();
        let request = |method, path, body: Option<Value>| -> Value {
            let request = agent
                .request(method, &format!("{url}api/{path}"))
                .set("X-Voiceless-Token", token);
            let response = if let Some(body) = body {
                request
                    .set("Content-Type", "application/json")
                    .send_string(&body.to_string())
            } else {
                request.call()
            };
            serde_json::from_reader(response.unwrap().into_reader()).unwrap()
        };
        let audio = || request("GET", "desktop/audio", None);
        let initial = audio();
        assert_eq!(initial["effective"], "usb");
        assert_eq!(initial["microphones"].as_array().unwrap().len(), 2);
        update_graph("headset", true);
        assert_eq!(audio()["effective"], "headset");

        assert_eq!(
            request("PUT", "config", Some(json!({"audio":{"source":"usb"}})))["restart_engine"],
            true
        );
        assert_eq!(audio()["effective"], "usb");
        update_graph("headset", false);
        let disconnected = audio();
        assert_eq!(disconnected["system_default"], "headset");
        assert_eq!(disconnected["selected"], "usb");
        assert!(disconnected["effective"].is_null());
        assert!(disconnected["error"].as_str().unwrap().contains("usb"));
        request("PUT", "config", Some(json!({"audio":{"source":null}})));
        assert_eq!(audio()["effective"], "headset");
        update_graph("usb", false);
        let stale_default = audio();
        assert_eq!(stale_default["system_default"], "usb");
        assert!(stale_default["effective"].is_null());
        assert!(stale_default["error"]
            .as_str()
            .is_some_and(|error| error.contains("default")));

        let overlay = request("GET", "desktop/overlay", None);
        assert_eq!(overlay["available"], false);
        assert!(
            overlay["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("layer")),
            "{overlay}"
        );
    })
    .await
    .unwrap();
    process.kill().await.unwrap();
}

/// A daemon started the way a user's service starts it, on a private bus:
/// the bus address for the host's own connection. The bus process, the
/// daemon and its directories live as long as this does.
struct StartedDaemon {
    _bus: tokio::process::Child,
    address: String,
    _daemon: tokio::process::Child,
    _root: tempfile::TempDir,
    /// The daemon's data directory: its runtime lock tells it from none.
    data: std::path::PathBuf,
}

async fn started_daemon() -> StartedDaemon {
    let (bus, address) = private_bus().await;
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        "[console]\nport = 0\n[audio]\npause_media = false\n",
    )
    .unwrap();
    let daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_voiceless"))
        .arg("--config")
        .arg(&config)
        .args(["daemon", "--no-inject"])
        .env("DBUS_SESSION_BUS_ADDRESS", &address)
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    StartedDaemon {
        _bus: bus,
        address,
        _daemon: daemon,
        data: root.path().join("data"),
        _root: root,
    }
}

/// The host's connection to the daemon's bus.
async fn host_of(address: &str) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// Opening the window again and again shows the console of the runtime
/// that is already running and starts nothing.
#[tokio::test]
async fn activation_reuses_the_running_runtime_however_often_the_window_opens() {
    let running = started_daemon().await;
    let host = host_of(&running.address).await;
    // A runtime the host cannot start: reuse is the only way to succeed.
    let nothing = std::path::Path::new("/nonexistent/voiceless");
    let open =
        || host::ensure_console(&host, Duration::from_secs(20), Some(nothing), &running.data);
    let url = tokio::time::timeout(Duration::from_secs(30), open())
        .await
        .expect("the running runtime answers")
        .expect("nothing is started")
        .expect("the daemon serves its console");
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), open())
            .await
            .unwrap(),
        Ok(Some(url.clone()))
    );
    assert_eq!(
        host::runtime_state(&host).await,
        Ok(Some("idle".to_owned()))
    );
    // The console is served there, with the token its pages carry.
    let page =
        tokio::task::spawn_blocking(move || ureq::get(&url).call().unwrap().into_string().unwrap())
            .await
            .unwrap();
    assert!(page.contains("name=\"voiceless-token\""));
}

/// The tray's actions reach the runtime, and quitting asks only while a
/// session runs. Without a tray host on the bus, closing the window leaves
/// the host; the login start is the system's or nobody's here.
#[tokio::test]
async fn lifecycle_tray_actions_reach_the_runtime_and_quitting_follows_the_session() {
    let running = started_daemon().await;
    let host = host_of(&running.address).await;
    let nothing = std::path::Path::new("/nonexistent/voiceless");
    host::ensure_console(&host, Duration::from_secs(20), Some(nothing), &running.data)
        .await
        .unwrap()
        .unwrap();
    assert!(!host::tray_available(&host).await);
    assert_eq!(lifecycle::on_close(false), Close::Exit);
    assert_eq!(lifecycle::on_close(true), Close::Hide);
    let system = lifecycle::system_autostart_directories();
    let entries = lifecycle::autostart_entries(
        system.iter().map(std::path::PathBuf::as_path),
        "voiceless-desktop",
    );
    assert_ne!(lifecycle::autostart_state(false, &entries), Autostart::App);

    assert_eq!(host::paused(&host).await, Ok(Some(false)));
    host::set_paused(&host, true).await.unwrap();
    assert_eq!(host::paused(&host).await, Ok(Some(true)));
    host::set_paused(&host, false).await.unwrap();
    assert_eq!(host::paused(&host).await, Ok(Some(false)));
    // Nothing was dictated yet: the runtime says so, in its own words, rather than typing anything.
    let error = host::repaste(&host).await.unwrap_err();
    assert!(
        matches!(&error, RuntimeError::Bus(message) if message.contains("no completed dictation")),
        "{error}"
    );

    let state = host::runtime_state(&host)
        .await
        .unwrap()
        .expect("the runtime runs");
    assert_eq!(lifecycle::on_quit(&state), Quit::Now);
}

/// A runtime the host starts that never takes its name is reported as
/// such once the wait is over.
#[tokio::test]
async fn activation_reports_a_runtime_that_never_comes() {
    let (_bus, address) = private_bus().await;
    let host = host_of(&address).await;
    assert_eq!(host::runtime_state(&host).await, Ok(None));
    let script = tempfile::tempdir().unwrap();
    let script = script.path().join("voiceless");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = host::ensure_console(
        &host,
        Duration::from_secs(3),
        Some(&script),
        script.parent().unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, RuntimeError::Timeout(Duration::from_secs(3)));
    // The runtime it started is detached and keeps running until stopped by hand.
    let started = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(script.to_str().unwrap())
        .output()
        .unwrap();
    let pids = String::from_utf8(started.stdout).unwrap();
    assert!(
        !pids.trim().is_empty(),
        "the started runtime survives the host"
    );
    std::process::Command::new("pkill")
        .arg("-f")
        .arg(script.to_str().unwrap())
        .status()
        .unwrap();
}
