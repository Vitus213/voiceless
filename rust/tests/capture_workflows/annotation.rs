use crate::session_bus::private_bus;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{fs, io::ErrorKind, net::TcpListener, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::{sleep, timeout},
};

#[tokio::test]
async fn annotation_without_local_model_never_contacts_cloud_primary() {
    let root = tempfile::tempdir().unwrap();
    let receipt = TcpListener::bind("127.0.0.1:0").unwrap();
    receipt.set_nonblocking(true).unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, format!("[console]\nport=0\n[cloud]\napi_key='test-only'\nrealtime_endpoint='wss://{}'\n[local]\nmodel='sense-voice-int8'\n[audio]\npause_media=false\n", receipt.local_addr().unwrap())).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let (_bus, address) = private_bus().await;
    let fixtures =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend/e2e/fixtures");
    let mut paths = vec![fixtures.join("bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_voiceless"))
        .arg("--config")
        .arg(config)
        .args(["daemon", "--no-inject"])
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("HOME", root.path())
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_RUNTIME_DIR", root.path())
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("VOICELESS_E2E_PCM", fixtures.join("jfk-16k-mono-s16le.pcm"))
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(daemon.stdout.take().unwrap()).lines();
    let url = timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some(url) = line.strip_prefix("Voiceless console: ") {
                return url.to_owned();
            }
        }
        panic!("daemon exited before serving console");
    })
    .await
    .unwrap();
    let shell = ureq::get(&url).call().unwrap().into_string().unwrap();
    let token = shell
        .split_once("name=\"voiceless-token\" content=\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap();
    let decode = |response: ureq::Response| -> Value {
        serde_json::from_reader(response.into_reader()).unwrap()
    };
    let status = || {
        decode(
            ureq::get(&format!("{url}api/status"))
                .set("X-Voiceless-Token", token)
                .call()
                .unwrap(),
        )
    };
    assert_eq!(status()["backend"], "cloud-realtime");
    let started = decode(
        ureq::post(&format!("{url}api/session/narrate"))
            .set("X-Voiceless-Token", token)
            .set("Content-Type", "application/json")
            .send_string(&json!({"runtime_id": status()["runtime_id"]}).to_string())
            .unwrap(),
    );
    timeout(Duration::from_secs(15), async {
        loop {
            let state = status();
            if state["state"] == "idle" && state["session"]["id"] == started["session_id"] {
                let session = &state["session"];
                assert_eq!(session["purpose"], "narration");
                assert_eq!(session["phase"], "failed", "{session}");
                assert!(session["injection"].is_null());
                assert!(
                    session["error"].as_str().unwrap().contains("model"),
                    "{session}"
                );
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(receipt.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}
