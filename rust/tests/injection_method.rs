//! Contract tests for the `[inject]` method selection: clipboard injection
//! copies the text to `wl-copy` and synthesizes the paste keypress. Fallback
//! requires a primary failure before any keystrokes could have been sent.
//! Temporary shell scripts record stdin or arguments so the tests can assert
//! what was sent without a compositor.

use std::path::Path;

use voiceless::config::{AppConfig, InjectConfig};
use voiceless::coordinator::{Delivery, TextInjector};
use voiceless::desktop::focus::FocusedWindow;
use voiceless::stream::{inject_text, ConfiguredInjector};

/// Feed freshly written scripts to a stable interpreter. Concurrent forks can
/// briefly retain a writer and make a direct exec of the script fail with ETXTBSY.
fn shell_script(path: &Path, script: &str) -> String {
    std::fs::write(path, script).expect("write fake command");
    format!("/bin/sh {}", path.display())
}

/// Fake `wl-copy` (records stdin) and `wtype` (records args). When
/// `fail_typing` is set, `wtype` fails only on `--` typing invocations so the
/// clipboard fallback's paste step still succeeds.
fn fake_commands(dir: &Path, fail_typing: bool) -> (String, String) {
    std::fs::create_dir_all(dir).expect("create fixture dir");
    let copy_out = dir.join("clipboard.txt");
    let type_out = dir.join("wtype-args.txt");
    let wl_copy = shell_script(
        &dir.join("wl-copy"),
        &format!("cat > {}", copy_out.display()),
    );
    let fail = if fail_typing {
        "if [ \"$1\" = \"--\" ]; then exit 1; fi"
    } else {
        "true"
    };
    let wtype = shell_script(
        &dir.join("wtype"),
        &format!("echo \"$@\" >> {}\n{}\nexit 0\n", type_out.display(), fail),
    );
    (wl_copy, wtype)
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
        ..InjectConfig::default()
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
    assert_eq!(args.trim(), "-M ctrl -k v -p Control_L");
}

#[tokio::test]
async fn clipboard_injection_mirrors_to_x11_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let x11_out = dir.path().join("x11.txt");
    let xsel = shell_script(
        &dir.path().join("xsel"),
        &format!("cat > {}", x11_out.display()),
    );
    let mut config = config_with("clipboard", &wl_copy, &wtype);
    config.x11_clipboard_command = xsel;

    let result = inject_text(&config, "镜像").await;

    assert!(result.ok, "{result:?}");
    let mirrored = std::fs::read_to_string(x11_out).unwrap();
    assert_eq!(mirrored, "镜像");
}

/// The primary tool could not start, so it is safe to fall back.
#[tokio::test]
async fn a_missing_wtype_falls_back_to_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, paste_tool) = fake_commands(dir.path(), false);
    let mut config = config_with("wtype", &wl_copy, &paste_tool);
    config.wtype_command = dir.path().join("not-installed").display().to_string();

    let result = inject_text(&config, "fallback").await;

    assert!(result.ok, "{result:?}");
    assert_eq!(result.method, "clipboard");
    let copied = std::fs::read_to_string(dir.path().join("clipboard.txt")).unwrap();
    assert_eq!(copied, "fallback");
}

#[tokio::test]
async fn a_typing_process_that_started_then_failed_is_not_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), true);
    let config = config_with("wtype", &wl_copy, &wtype);

    let result = inject_text(&config, "already sent").await;

    assert!(!result.ok, "{result:?}");
    assert_eq!(result.delivery, Delivery::Unconfirmed);
    assert_eq!(typed(dir.path()).trim(), "-- already sent");
    assert!(!dir.path().join("clipboard.txt").exists());
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

    let focus = shell_script(
        &dir.path().join("focus"),
        &format!("echo focus >> {}", order.display()),
    );
    let paste = shell_script(
        &dir.path().join("paste-tool"),
        &format!("echo \"paste:$@\" >> {}", order.display()),
    );

    let mut config = config_with("clipboard", &wl_copy, &wtype);
    config.paste_tool = paste;
    config.paste_command = "key --clearmodifiers ctrl+v".to_owned();
    config.focus_command = focus;

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
    std::fs::write(dir.path().join("wl-paste"), "echo PRIOR").unwrap();
    let log = dir.path().join("clipboard.txt");
    std::fs::write(
        dir.path().join("wl-copy"),
        format!("cat >> {}\necho >> {}\n", log.display(), log.display()),
    )
    .unwrap();

    let config = config_with("clipboard", &wl_copy, &wtype);
    let result = inject_text(&config, "转录文本").await;

    assert!(result.ok, "{result:?}");
    let content = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines[0], "转录文本", "copy the transcript first");
    assert_eq!(lines[1], "PRIOR", "then restore the previous clipboard");
}

/// Config validation of `[inject].prefer` (`wtype`, `clipboard`, `fcitx5`
/// and `copy` are accepted; anything else is rejected).
#[test]
fn prefer_accepts_wtype_clipboard_fcitx5_and_copy() {
    let bad = AppConfig::from_toml("[inject]\nprefer = \"bogus\"\n");
    assert!(bad.is_err(), "bogus prefer must be rejected");
    for method in ["wtype", "clipboard", "fcitx5", "copy"] {
        let good = AppConfig::from_toml(&format!("[inject]\nprefer = \"{method}\"\n"));
        assert!(good.is_ok(), "{method}: {good:?}");
    }
}

/// A window of the scripted desktop: Niri's answer, by id and application.
fn window(key: &str, app_id: &str) -> FocusedWindow {
    FocusedWindow {
        source: "niri",
        key: key.to_owned(),
        app_id: Some(app_id.to_owned()),
        title: None,
    }
}

/// What the fake `wtype` was given; empty when it never ran.
fn typed(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("wtype-args.txt")).unwrap_or_default()
}

/// A session delivers to the window the user was typing in when it began.
#[tokio::test]
async fn text_is_withheld_when_the_focus_moved_to_another_window() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let focus = voiceless::desktop::focus::script(vec![
        Some(window("7", "org.mozilla.firefox")),
        Some(window("9", "org.mozilla.thunderbird")),
    ]);
    let mut injector = ConfiguredInjector::watching(config_with("wtype", &wl_copy, &wtype), focus);

    let result = injector.inject("你好").await;

    assert!(!result.ok, "{result:?}");
    assert_eq!(result.delivery, Delivery::NotDelivered);
    assert!(
        result.message.contains("org.mozilla.firefox")
            && result.message.contains("org.mozilla.thunderbird"),
        "{}",
        result.message
    );
    // Nothing reaches the other window, and the clipboard fallback does not run either.
    assert_eq!(typed(dir.path()), "");
    assert!(!dir.path().join("clipboard.txt").exists());
}

#[tokio::test]
async fn the_window_the_session_began_in_gets_the_text_as_configured() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let focus = voiceless::desktop::focus::script(vec![Some(window("7", "org.mozilla.firefox"))]);
    let mut injector = ConfiguredInjector::watching(config_with("wtype", &wl_copy, &wtype), focus);

    let result = injector.inject("你好").await;

    // The keystrokes left Voiceless; whether the window took them is not known.
    assert!(result.ok, "{result:?}");
    assert_eq!(
        (result.method.as_str(), result.delivery),
        ("wtype", Delivery::Unconfirmed)
    );
    assert_eq!(typed(dir.path()).trim(), "-- 你好");
}

#[tokio::test]
async fn a_desktop_that_cannot_confirm_the_original_window_withholds_text() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let mut injector = ConfiguredInjector::watching(
        config_with("wtype", &wl_copy, &wtype),
        voiceless::desktop::focus::script(vec![None]),
    );

    let result = injector.inject("你好").await;

    assert!(!result.ok, "{result:?}");
    assert_eq!(result.delivery, Delivery::NotDelivered);
    assert_eq!(typed(dir.path()), "");
    assert!(!dir.path().join("clipboard.txt").exists());
}

#[tokio::test]
async fn clipboard_rechecks_the_window_after_preparing_to_paste() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, paste_tool) = fake_commands(dir.path(), false);
    let original = Some(window("7", "editor"));
    let focus = voiceless::desktop::focus::script(vec![
        original.clone(),
        original.clone(),
        original,
        Some(window("8", "mail")),
    ]);
    let mut injector =
        ConfiguredInjector::watching(config_with("clipboard", &wl_copy, &paste_tool), focus);

    let result = injector.inject("for the editor").await;

    assert_eq!(result.delivery, Delivery::NotDelivered);
    assert!(result.message.contains("mail"), "{result:?}");
    assert_eq!(typed(dir.path()), "");
}

/// A terminal runs each typed line: text with line breaks goes in as one
/// paste, with the terminal's own paste chord.
#[tokio::test]
async fn text_with_line_breaks_is_pasted_into_a_terminal_and_never_typed() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let focus = voiceless::desktop::focus::script(vec![Some(window("3", "kitty"))]);
    let mut config = config_with("wtype", &wl_copy, &wtype);
    config.terminal_paste_command = "-P".to_owned();
    let mut injector = ConfiguredInjector::watching(config, focus);

    let result = injector.inject("echo one\necho two").await;

    assert!(result.ok, "{result:?}");
    assert_eq!(
        (result.method.as_str(), result.delivery),
        ("clipboard", Delivery::Unconfirmed)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("clipboard.txt")).unwrap(),
        "echo one\necho two"
    );
    assert_eq!(typed(dir.path()).trim(), "-P");
}

#[tokio::test]
async fn one_line_is_typed_into_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let focus = voiceless::desktop::focus::script(vec![Some(window("3", "kitty"))]);
    let mut injector = ConfiguredInjector::watching(config_with("wtype", &wl_copy, &wtype), focus);

    let result = injector.inject("echo one").await;

    assert!(result.ok, "{result:?}");
    assert_eq!(
        (result.method.as_str(), result.delivery),
        ("wtype", Delivery::Unconfirmed)
    );
    assert_eq!(typed(dir.path()).trim(), "-- echo one");
}

#[tokio::test]
async fn the_copy_method_leaves_the_text_on_the_clipboard_and_types_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (wl_copy, wtype) = fake_commands(dir.path(), false);
    let mut injector = ConfiguredInjector::watching(
        config_with("copy", &wl_copy, &wtype),
        voiceless::desktop::focus::script(vec![None]),
    );

    let result = injector.inject("留给我粘贴").await;

    assert!(result.ok, "{result:?}");
    assert_eq!(
        (result.method.as_str(), result.delivery),
        ("copy", Delivery::Copied)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("clipboard.txt")).unwrap(),
        "留给我粘贴"
    );
    assert_eq!(typed(dir.path()), "");
}
