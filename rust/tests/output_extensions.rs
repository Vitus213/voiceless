use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn preview(root: &Path, output: &str, arguments: &[&str]) -> (i32, Value) {
    let config = root.join("config.toml");
    std::fs::write(&config, output).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_voiceless"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .arg("--config")
        .arg(config)
        .args(["mode", "preview", "--json", "--text", "SDK /tmp/v1"])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        result.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!root.join("data").exists(), "preview stored session data");
    (
        result.status.code().unwrap(),
        serde_json::from_slice(&result.stdout).unwrap(),
    )
}

fn script(source: &str, allowed: bool, timeout_ms: u64) -> String {
    format!(
        r#"
[[output.pipelines]]
mode = "quick"
steps = ["script"]
[output.pipelines.script]
source = '''{source}'''
allowed = {allowed}
timeout_ms = {timeout_ms}
"#
    )
}

#[test]
fn permissions_gate_the_text_script_and_preserve_the_input_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let sample = "SDK /tmp/v1";
    let source = r#""docs: " + text"#;
    let (status, denied) = preview(root.path(), &script(source, false, 5000), &[]);
    assert_eq!(status, 1);
    assert_eq!(denied["text"], sample);
    assert!(denied["error"].as_str().unwrap().contains("permission"));

    let (status, allowed) = preview(root.path(), &script(source, true, 5000), &[]);
    assert_eq!(status, 0, "{allowed}");
    assert_eq!(allowed["text"], "docs: SDK /tmp/v1");
    assert_eq!(allowed["trace"]["steps"][0]["before"], sample);

    let (status, failure) = preview(root.path(), &script("read_file(text)", true, 5000), &[]);
    assert_eq!(status, 1);
    assert_eq!(failure["text"], sample);
    assert!(failure["trace"]["steps"][0]["error"]
        .as_str()
        .unwrap()
        .contains("read_file"));
}

#[test]
fn preview_bounds_script_time_and_retains_the_input() {
    let root = tempfile::tempdir().unwrap();
    let (status, result) = preview(root.path(), &script("loop { let x = 1; }", true, 1), &[]);
    assert_eq!(status, 1);
    assert_eq!(result["text"], "SDK /tmp/v1");
    assert_eq!(result["error"], "output script exceeded 1 ms");
}

#[test]
fn permissions_gate_site_rules_and_remote_processing() {
    let root = tempfile::tempdir().unwrap();
    let config = |allowed: bool| {
        format!(
            r#"
[output]
allow_site_context = {allowed}
rules = [{{site = "docs.example.com", project = "manual", mode = "voice-polish"}}]
"#
        )
    };
    let request = ["--site", "Docs.Example.Com.", "--send-model"];
    let (status, denied) = preview(root.path(), &config(false), &request);
    assert_eq!(status, 0, "{denied}");
    assert_eq!(denied["trace"]["selection"]["source"], "default");
    assert!(denied["trace"]["selection"].get("site").is_none());

    let (status, allowed) = preview(root.path(), &config(true), &request);
    assert_eq!(status, 1, "{allowed}");
    assert_eq!(allowed["trace"]["selection"]["source"], "site-rule");
    assert_eq!(allowed["trace"]["selection"]["site_source"], "preview");
    assert_eq!(allowed["trace"]["selection"]["project"], "manual");
    assert!(allowed["error"]
        .as_str()
        .unwrap()
        .contains("explicit permission"));
}
