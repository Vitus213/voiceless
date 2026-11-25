//! Windows cases for the desktop_contract integration target.
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[test]
fn windows_doctor_identifies_the_cpu_engine_even_without_a_microphone() {
    let root = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_voiceless"))
        .arg("doctor")
        .env("XDG_DATA_HOME", root.path())
        .output()
        .unwrap();
    let report = String::from_utf8(result.stdout).unwrap();
    assert!(
        report.contains("Windows inference engines: x86_64 build; compiled providers: cpu"),
        "{report}"
    );
    assert!(report.contains("global input permission:"), "{report}");
    assert!(report.contains("microphone:"), "{report}");
}

#[tokio::test]
async fn windows_console_retains_an_unplugged_wasapi_selection() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    let missing = "WASAPI:{voiceless-removed-endpoint}";
    std::fs::write(&config, format!("[audio]\nsource = \"{missing}\"\n")).unwrap();
    let mut server = tokio::process::Command::new(env!("CARGO_BIN_EXE_voiceless"))
        .arg("--config")
        .arg(&config)
        .args(["web", "--no-open", "--port", "0"])
        .env("XDG_DATA_HOME", root.path())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let url = line.strip_prefix("Voiceless console: ").unwrap().to_owned();
    let audio: serde_json::Value = tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .timeout(Duration::from_secs(5))
            .build();
        let page = agent.get(&url).call().unwrap().into_string().unwrap();
        let token = page
            .split_once("name=\"voiceless-token\" content=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap();
        let response = agent
            .get(&format!("{url}api/desktop/audio"))
            .set("X-Voiceless-Token", token)
            .call()
            .unwrap();
        serde_json::from_reader(response.into_reader()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(audio["selected"], missing);
    assert!(audio["effective"].is_null(), "{audio}");
    assert!(
        audio["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty()),
        "{audio}"
    );
    server.kill().await.unwrap();
}
