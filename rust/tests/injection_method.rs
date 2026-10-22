//! Contract tests for the `[inject]` method selection: clipboard injection
//! copies the text to `wl-copy` and synthesizes the paste keypress; wtype
//! failure falls back to the clipboard when enabled. The external tools
//! (`wl-copy`, `wl-paste`, `wtype`, an X11 writer, a focus and a paste
//! tool) are replaced by shell scripts in a temp dir that record their
//! stdin or arguments, so `stream::inject_text` runs end to end without a
//! Wayland session.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use voiceless::config::{AppConfig, InjectConfig};
use voiceless::stream::inject_text;

/// Write a `0755` shell script standing in for an external tool.
fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script).expect("write fake command");
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod fake command");
}

/// Fake `wl-copy` (records stdin) and `wtype` (records args). When
/// `fail_typing` is set, `wtype` fails only on `--` typing invocations so the
/// clipboard fallback's paste step still succeeds.
fn fake_commands(dir: &Path, fail_typing: bool) -> (String, String) {
    std::fs::create_dir_all(dir).expect("create fixture dir");
    let wl_copy = dir.join("wl-copy");
    let wtype = dir.join("wtype");
    let copy_out = dir.join("clipboard.txt");
    let type_out = dir.join("wtype-args.txt");
    write_executable(
        &wl_copy,
        &format!("#!/bin/sh\ncat > {}\n", copy_out.display()),
    );
    let fail = if fail_typing {
        "if [ \"$1\" = \"--\" ]; then exit 1; fi"
    } else {
        "true"
    };
    write_executable(
        &wtype,
        &format!(
            "#!/bin/sh\necho \"$@\" >> {}\n{}\nexit 0\n",
            type_out.display(),
            fail
        ),
    );
    (wl_copy.display().to_string(), wtype.display().to_string())
}

/// `[inject]` pointing at the fake tools, with the clipboard fallback on.
fn config_with(prefer: &str, wl_copy: &str, wtype: &str) -> InjectConfig {
    InjectConfig {
        prefer: prefer.to_owned(),
        wtype_command: wtype.to_owned(),
        wl_copy_command: wl_copy.to_owned(),
        paste_command: "-M ctrl -k v".to_owned(),
        paste_tool: wtype.to_owned(),
        focus_command: String::new(),
        x11_clipboard_command: String::new(),
        clipboard_fallback: true,
        timeout_seconds: 5.0,
        preedit: false,
    }
}

/// `prefer = "clipboard"`: the text (newline included) reaches `wl-copy`
/// and the paste tool receives the configured keystroke arguments.
#[tokio::test]
async fn clipboard_injection_copies_text_and_pastes() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let config = config_with("clipboard", &wl_copy, &wtype);

    let result = inject_text(&config, "你好\n世界").await;

    assert!(result.ok, "{result:?}");
    assert_eq!(result.method, "clipboard");
    let copied = std::fs::read_to_string(dir.path().join("clipboard.txt")).unwrap();
    assert_eq!(copied, "你好\n世界");
    let args = std::fs::read_to_string(dir.path().join("wtype-args.txt")).unwrap();
    assert_eq!(args.trim(), "-M ctrl -k v");
}

#[tokio::test]
async fn clipboard_injection_mirrors_to_x11_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let xsel = dir.path().join("xsel");
    let x11_out = dir.path().join("x11.txt");
    write_executable(&xsel, &format!("#!/bin/sh\ncat > {}\n", x11_out.display()));
    let mut config = config_with("clipboard", &wl_copy, &wtype);
    config.x11_clipboard_command = xsel.display().to_string();

    let result = inject_text(&config, "镜像").await;

    assert!(result.ok, "{result:?}");
    let mirrored = std::fs::read_to_string(x11_out).unwrap();
    assert_eq!(mirrored, "镜像");
}

/// `wtype` fails on typing: the result names `clipboard` and the text was
/// copied.
#[tokio::test]
async fn wtype_failure_falls_back_to_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), true);
    let config = config_with("wtype", &wl_copy, &wtype);

    let result = inject_text(&config, "fallback").await;

    assert!(result.ok, "{result:?}");
    assert_eq!(result.method, "clipboard");
    let copied = std::fs::read_to_string(dir.path().join("clipboard.txt")).unwrap();
    assert_eq!(copied, "fallback");
}

#[tokio::test]
async fn wtype_success_does_not_touch_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let config = config_with("wtype", &wl_copy, &wtype);

    let result = inject_text(&config, "typed").await;

    assert!(result.ok, "{result:?}");
    assert_eq!(result.method, "wtype");
    assert!(!dir.path().join("clipboard.txt").exists());
    let args = std::fs::read_to_string(dir.path().join("wtype-args.txt")).unwrap();
    assert!(args.contains("-- typed"));
}

/// A missing `wl-copy` yields `ok = false` with method `clipboard`.
#[tokio::test]
async fn clipboard_failure_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (_, wtype) = fake_commands(dir.path(), false);
    let missing = dir.path().join("missing-wl-copy");
    let config = config_with("clipboard", &missing.display().to_string(), &wtype);

    let result = inject_text(&config, "text").await;

    assert!(!result.ok);
    assert_eq!(result.method, "clipboard");
}

#[tokio::test]
async fn clipboard_focus_and_custom_paste_tool_run_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let order = dir.path().join("order.txt");

    let focus = dir.path().join("focus");
    write_executable(
        &focus,
        &format!("#!/bin/sh\necho focus >> {}\n", order.display()),
    );
    let paste = dir.path().join("paste-tool");
    write_executable(
        &paste,
        &format!("#!/bin/sh\necho \"paste:$@\" >> {}\n", order.display()),
    );

    let mut config = config_with("clipboard", &wl_copy, &wtype);
    config.paste_tool = paste.display().to_string();
    config.paste_command = "key --clearmodifiers ctrl+v".to_owned();
    config.focus_command = focus.display().to_string();

    let result = inject_text(&config, "微信").await;

    assert!(result.ok, "{result:?}");
    let log = std::fs::read_to_string(&order).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines[0], "focus", "focus must run before paste");
    assert_eq!(lines[1], "paste:key --clearmodifiers ctrl+v");
}

/// The prior selection read through `wl-paste` is written back after the
/// transcript was pasted.
#[tokio::test]
async fn clipboard_injection_restores_previous_selection() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);

    // Paired wl-paste reports the prior clipboard; the fake wl-copy appends
    // every stdin write so we can observe copy-then-restore ordering.
    let wl_paste = dir.path().join("wl-paste");
    write_executable(&wl_paste, "#!/bin/sh\necho PRIOR");
    let log = dir.path().join("clipboard.txt");
    write_executable(
        &dir.path().join("wl-copy"),
        &format!(
            "#!/bin/sh\ncat >> {}\necho >> {}\n",
            log.display(),
            log.display()
        ),
    );

    let config = config_with("clipboard", &wl_copy, &wtype);
    let result = inject_text(&config, "转录文本").await;

    assert!(result.ok, "{result:?}");
    let content = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines[0], "转录文本", "copy the transcript first");
    assert_eq!(lines[1], "PRIOR", "then restore the previous clipboard");
}

/// Config validation of `[inject].prefer` (`wtype`, `clipboard`, `fcitx5`
/// are accepted; anything else is rejected).
#[test]
fn prefer_accepts_wtype_and_clipboard_only() {
    let bad = AppConfig::from_toml("[inject]\nprefer = \"bogus\"\n");
    assert!(bad.is_err(), "bogus prefer must be rejected");
    let good = AppConfig::from_toml("[inject]\nprefer = \"clipboard\"\n");
    assert!(good.is_ok(), "{good:?}");
}
