//! End-to-end CLI contract of the `local-streaming` backend when its model
//! is unavailable: `voiceless stream --json --no-inject` must fail at the
//! open step with exit code 1, report the problem as an `error` event
//! followed by `completed`, and never touch the cloud. Fixtures: the built
//! binary (`CARGO_BIN_EXE_voiceless`), an explicit `--config` file and a
//! throwaway XDG root so no installed model or user config leaks in; no
//! model is needed.

use std::fs;
use std::process::Command;

use tempfile::tempdir;

fn voiceless() -> Command {
    Command::new(env!("CARGO_BIN_EXE_voiceless"))
}

/// No model installed: the JSON stream is exactly `error`, `completed`,
/// and the output mentions neither DashScope nor an authorization header.
#[test]
fn explicit_local_backend_reports_model_error_without_cloud() {
    let root = tempdir().expect("temporary directory");
    let config = root.path().join("config.toml");
    fs::write(&config, "[asr]\nstreaming_backend = \"local-streaming\"\n").expect("config write");

    let output = voiceless()
        .args([
            "--config",
            config.to_str().expect("config path"),
            "stream",
            "--json",
            "--no-inject",
        ])
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .output()
        .expect("voiceless binary should be runnable");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("JSON output should be UTF-8");
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("each output line should be JSON"))
        .collect();
    assert_eq!(
        events
            .iter()
            .filter_map(|event| event.get("type").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>(),
        vec!["error", "completed"]
    );
    let rendered = stdout.to_ascii_lowercase();
    assert!(!rendered.contains("dashscope"));
    assert!(!rendered.contains("authorization"));
}

/// `local_model_dir` points at an empty directory: the error names the
/// first missing required file (`encoder.int8.onnx`).
#[test]
fn local_backend_reports_the_missing_required_model_file() {
    let root = tempdir().expect("temporary directory");
    let model_dir = root.path().join("model");
    fs::create_dir(&model_dir).expect("model directory");
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "[asr]\nstreaming_backend = \"local-streaming\"\nlocal_model_dir = {:?}\n",
            model_dir
        ),
    )
    .expect("config write");

    let output = voiceless()
        .args([
            "--config",
            config.to_str().expect("config path"),
            "stream",
            "--json",
            "--no-inject",
        ])
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .output()
        .expect("voiceless binary should be runnable");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("JSON output should be UTF-8");
    assert!(stdout.contains("encoder.int8.onnx"), "{stdout}");
    assert!(stdout.contains("\"type\":\"error\""), "{stdout}");
    assert!(stdout.contains("\"type\":\"completed\""), "{stdout}");
}
