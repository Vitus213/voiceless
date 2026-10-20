//! Contract tests for `inject.prefer = "fcitx5"` when the bridge is
//! unreachable: the failure is reported as the fcitx5 method, and the
//! clipboard fallback takes over when enabled. The session bus points at a
//! dead socket so these tests never reach a real Fcitx5.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use voiceless::config::{AppConfig, InjectConfig};
use voiceless::stream::inject_text;

fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script).expect("write fake command");
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod fake command");
}

fn unreachable_session_bus(dir: &Path) {
    std::env::set_var(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", dir.join("no-bus").display()),
    );
}

fn fcitx5_config(dir: &Path, clipboard_fallback: bool) -> InjectConfig {
    let wl_copy = dir.join("wl-copy");
    let paste = dir.join("paste");
    write_executable(
        &wl_copy,
        &format!("#!/bin/sh\ncat > {}\n", dir.join("clipboard.txt").display()),
    );
    write_executable(&paste, "#!/bin/sh\nexit 0\n");
    InjectConfig {
        prefer: "fcitx5".to_owned(),
        wl_copy_command: wl_copy.display().to_string(),
        paste_tool: paste.display().to_string(),
        clipboard_fallback,
        timeout_seconds: 5.0,
        ..InjectConfig::default()
    }
}

#[tokio::test]
async fn unreachable_bridge_falls_back_to_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    unreachable_session_bus(dir.path());
    let config = fcitx5_config(dir.path(), true);

    let result = inject_text(&config, "你好").await;

    assert!(result.ok, "clipboard fallback should succeed: {result:?}");
    assert_eq!(result.method, "clipboard");
    let copied = std::fs::read_to_string(dir.path().join("clipboard.txt")).unwrap();
    assert_eq!(copied, "你好");
}

#[tokio::test]
async fn unreachable_bridge_without_fallback_reports_fcitx5_failure() {
    let dir = tempfile::tempdir().unwrap();
    unreachable_session_bus(dir.path());
    let config = fcitx5_config(dir.path(), false);

    let result = inject_text(&config, "你好").await;

    assert!(!result.ok);
    assert_eq!(result.method, "fcitx5");
    assert!(!result.message.is_empty());
    assert!(!dir.path().join("clipboard.txt").exists());
}

#[test]
fn prefer_accepts_fcitx5() {
    assert!(AppConfig::from_toml("[inject]\nprefer = \"fcitx5\"\n").is_ok());
}
