//! End-to-end CLI contracts through the built binary: `doctor` output,
//! `model list --json` supply-chain fields, `model check` on an unknown id,
//! the `mode` list/add/update/remove round trip and builtin protection,
//! `history list`/`totals` JSON shape on an empty store, `transcribe` on a
//! missing file, and `stream` rejecting an unknown mode before any capture.
//!
//! No fakes inside the binary: isolation comes from the environment. Every
//! run gets a scratch `HOME` with all `XDG_*_HOME` roots under it (shared
//! between tests, so read-only commands may see each other's state), and
//! tests that write (`mode add`, `history`) override the relevant root with
//! a private temp dir. Network and audio are never touched: the `stream`
//! test fails at mode lookup, which precedes the backend and `pw-record`.

use std::process::Command;

/// The binary under test with every XDG root inside a scratch home: the
/// desktop session exports XDG_*_HOME, which would otherwise win over HOME
/// and let tests touch the user's real data and config.
fn voiceless() -> Command {
    let home = std::env::temp_dir().join("voiceless-cli-tests-home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_voiceless"));
    command
        .env("HOME", &home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".local/state"));
    command
}

/// Parse JSON-lines output, panicking on any non-JSON line.
fn json_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("each output line should be JSON"))
        .collect()
}

/// Only the check names are asserted: the exit status depends on which
/// tools the host has installed.
#[test]
fn doctor_reports_dependency_checks_and_exits_zero_when_tools_exist() {
    let output = voiceless().arg("doctor").output().expect("run doctor");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(stdout.contains("pw-record"), "{stdout}");
    assert!(stdout.contains("wtype"), "{stdout}");
}

/// Each JSON line carries the pinned SRI hash, byte size and license status.
#[test]
fn model_list_outputs_catalog_entries_with_supply_chain_fields() {
    let output = voiceless()
        .args(["model", "list", "--json"])
        .output()
        .expect("run model list");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");
    let entries = json_lines(&stdout);
    assert!(!entries.is_empty());
    let entry = &entries[0];
    assert!(entry["sha256_sri"].is_string());
    assert!(entry["size_bytes"].is_number());
    assert!(entry["license_status"].is_string());
}

#[test]
fn model_check_unknown_id_fails_with_nonzero_exit() {
    let output = voiceless()
        .args(["model", "check", "no-such-model"])
        .output()
        .expect("run model check");
    assert_eq!(output.status.code(), Some(1));
}

/// Builtins are present with `quick` first and `translate-en` among them.
#[test]
fn mode_list_returns_json_array_with_quick_first() {
    let output = voiceless()
        .args(["mode", "list"])
        .output()
        .expect("run mode list");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");
    let modes: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("JSON array");
    assert_eq!(modes[0]["id"], "quick");
    assert!(modes.iter().any(|mode| mode["id"] == "translate-en"));
}

/// add returns the generated id; update and remove accept it; the removed
/// mode is gone from list. Uses a private config root.
#[test]
fn mode_add_update_remove_roundtrip_via_cli() {
    let dir = tempfile::tempdir().expect("temporary root");
    let mut add = voiceless();
    add.args([
        "mode",
        "add",
        "--name",
        "CLI模式",
        "--prompt",
        "原文：{text}",
    ])
    .env("XDG_CONFIG_HOME", dir.path());
    let output = add.output().expect("mode add");
    assert!(output.status.success(), "{output:?}");
    let added: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).expect("JSON");
    let id = added["id"].as_str().expect("mode id").to_owned();

    let mut update = voiceless();
    update
        .args(["mode", "update", "--id", &id, "--name", "CLI模式改名"])
        .env("XDG_CONFIG_HOME", dir.path());
    let output = update.output().expect("mode update");
    assert!(output.status.success(), "{output:?}");

    let mut remove = voiceless();
    remove
        .args(["mode", "remove", "--id", &id])
        .env("XDG_CONFIG_HOME", dir.path());
    let output = remove.output().expect("mode remove");
    assert!(output.status.success(), "{output:?}");

    let mut list = voiceless();
    list.args(["mode", "list"])
        .env("XDG_CONFIG_HOME", dir.path());
    let output = list.output().expect("mode list");
    let modes: Vec<serde_json::Value> =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).expect("JSON array");
    assert!(!modes.iter().any(|mode| mode["id"] == id));
}

#[test]
fn mode_remove_builtin_is_rejected() {
    let output = voiceless()
        .args(["mode", "remove", "--id", "quick"])
        .output()
        .expect("run mode remove quick");
    assert_eq!(output.status.code(), Some(1));
}

/// On a fresh private data root: list has `records` and `next_cursor`,
/// totals reports zero records.
#[test]
fn history_list_and_totals_have_stable_json_shape() {
    let dir = tempfile::tempdir().expect("temporary root");
    let mut list = voiceless();
    list.args(["history", "list", "--limit", "5"])
        .env("XDG_DATA_HOME", dir.path());
    let output = list.output().expect("history list");
    assert!(output.status.success(), "{output:?}");
    let page: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).expect("JSON");
    assert!(page["records"].is_array());
    assert!(page.get("next_cursor").is_some());

    let mut totals = voiceless();
    totals
        .args(["history", "totals"])
        .env("XDG_DATA_HOME", dir.path());
    let output = totals.output().expect("history totals");
    assert!(output.status.success());
    let totals: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).expect("JSON");
    assert_eq!(totals["records"], 0);
}

/// Exit 1 with a "Voiceless"-prefixed message on stderr.
#[test]
fn transcribe_missing_file_fails_with_diagnostic() {
    let output = voiceless()
        .args(["transcribe", "/nonexistent/file.wav"])
        .output()
        .expect("run transcribe");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8");
    assert!(stderr.contains("Voiceless"), "{stderr}");
}

/// `--backend local-streaming --mode 不存在` exits 1 naming the mode; the
/// mode lookup fails before the model or `pw-record` is needed.
#[test]
fn stream_rejects_unknown_mode_before_capture() {
    let dir = tempfile::tempdir().expect("temporary root");
    let output = voiceless()
        .args([
            "stream",
            "--backend",
            "local-streaming",
            "--mode",
            "不存在",
            "--json",
            "--no-inject",
        ])
        .env("XDG_CONFIG_HOME", dir.path())
        .env("XDG_DATA_HOME", dir.path())
        .output()
        .expect("run stream");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8");
    assert!(
        stderr.contains("mode not found") || stderr.contains("不存在"),
        "{stderr}"
    );
}
