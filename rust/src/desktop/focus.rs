//! Where the user is typing when a dictation starts, and whether that is
//! still in front when the text is ready.
//!
//! Niri and Sway report the focused client through their compositor IPC.
//! On an X11 session, `xdotool` reads the active window and `xprop` its
//! class. An unavailable or ambiguous focus leaves guarded typing pending.
//!
//! [`FocusedWindow::key`] is the compositor or X11 window id. The title
//! is kept for display only, as it changes while the user types. An
//! application id that names a terminal makes text with line breaks go
//! in as one paste: a terminal would run each typed line.

#[cfg(target_os = "linux")]
use std::process::Output;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;
#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(target_os = "linux")]
use tokio::process::Command;

/// A desktop tool answers within this, or says nothing.
#[cfg(target_os = "linux")]
const LIMIT: Duration = Duration::from_secs(2);

/// Application ids and X11 window classes of terminals, in lower case.
const TERMINALS: &[&str] = &[
    "alacritty",
    "com.mitchellh.ghostty",
    "contour",
    "foot",
    "footclient",
    "gnome-terminal",
    "hterm",
    "io.elementary.terminal",
    "kitty",
    "konsole",
    "org.codeberg.dnkl.foot",
    "org.gnome.terminal",
    "org.kde.konsole",
    "org.wezfurlong.wezterm",
    "st",
    "st-256color",
    "termite",
    "urxvt",
    "uxterm",
    "xterm",
    "xterm-256color",
    "xterm-color",
];

/// One read of the window that has the keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FocusedWindow {
    /// The platform or compositor that supplied the window id.
    pub source: &'static str,
    /// What tells this window from another one at another moment: the
    /// compositor's window id.
    pub key: String,
    /// Application id (Wayland `app-id`), or the X11 window class; `None`
    /// when the tool does not say.
    pub app_id: Option<String>,
    /// Title at the time of the read; shown to the user, never compared.
    pub title: Option<String>,
}

impl FocusedWindow {
    /// The window as a message names it: application id, else title, else id.
    pub fn describe(&self) -> &str {
        self.app_id
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or(&self.key)
    }
}

/// How the injector reads the focused window: the desktop's tools in
/// production, a script in a test.
pub type FocusReader = Arc<dyn Fn() -> BoxFuture<'static, Option<FocusedWindow>> + Send + Sync>;

/// Read the focused client of the current compositor or native X11 session.
#[cfg(target_os = "linux")]
pub fn desktop() -> FocusReader {
    Arc::new(|| {
        Box::pin(async {
            match (
                std::env::var_os("NIRI_SOCKET").is_some(),
                std::env::var_os("SWAYSOCK").is_some(),
            ) {
                (true, false) => niri().await,
                (false, true) => sway().await,
                (false, false)
                    if std::env::var_os("WAYLAND_DISPLAY").is_none()
                        && std::env::var_os("WAYLAND_SOCKET").is_none()
                        && std::env::var_os("XDG_SESSION_TYPE").as_deref()
                            != Some(std::ffi::OsStr::new("wayland"))
                        && std::env::var_os("DISPLAY").is_some() =>
                {
                    x11().await
                }
                _ => None,
            }
        })
    })
}

/// Read the focused window through the system's native focus facilities.
#[cfg(not(target_os = "linux"))]
pub fn desktop() -> FocusReader {
    Arc::new(|| {
        Box::pin(async {
            tokio::task::spawn_blocking(super::native::focused_window)
                .await
                .ok()
                .flatten()
        })
    })
}

/// Whether the user is typing into a terminal, by application id or window
/// class; `extra` names more.
pub fn is_terminal(app_id: Option<&str>, extra: &[String]) -> bool {
    let Some(app_id) = app_id else {
        return false;
    };
    let app_id = app_id.trim().to_ascii_lowercase();
    TERMINALS.contains(&app_id.as_str())
        || extra
            .iter()
            .any(|name| name.trim().eq_ignore_ascii_case(&app_id))
}

/// Run a desktop tool to completion under `LIMIT`; `None` when it is
/// absent, fails or does not answer in time.
#[cfg(target_os = "linux")]
async fn run(args: &[&str]) -> Option<Output> {
    let mut command = Command::new(args[0]);
    command.args(&args[1..]).kill_on_drop(true);
    let output = tokio::time::timeout(LIMIT, command.output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output)
}

#[cfg(target_os = "linux")]
async fn niri() -> Option<FocusedWindow> {
    from_niri(
        &run(&["niri", "msg", "--json", "focused-window"])
            .await?
            .stdout,
    )
}

/// The focused window out of what `niri msg --json focused-window` prints.
/// Niri 26 writes `app_id` and `is_focused`; older versions wrote
/// `app-id` and `is-focused`.
#[cfg(target_os = "linux")]
fn from_niri(json: &[u8]) -> Option<FocusedWindow> {
    let value: Value = serde_json::from_slice(json).ok()?;
    let focused = value.get("is_focused").or_else(|| value.get("is-focused"));
    if focused.and_then(Value::as_bool) == Some(false) {
        return None;
    }
    Some(FocusedWindow {
        source: "niri",
        key: value.get("id")?.as_u64()?.to_string(),
        app_id: text(&value, "app_id").or_else(|| text(&value, "app-id")),
        title: text(&value, "title"),
    })
}

#[cfg(target_os = "linux")]
async fn sway() -> Option<FocusedWindow> {
    let (seats, tree) = tokio::join!(
        run(&["swaymsg", "-r", "-t", "get_seats"]),
        run(&["swaymsg", "-r", "-t", "get_tree"]),
    );
    from_sway(&seats?.stdout, &tree?.stdout)
}

#[cfg(target_os = "linux")]
fn from_sway(seats: &[u8], tree: &[u8]) -> Option<FocusedWindow> {
    // A virtual keyboard cannot identify its target from the default seat's
    // tree alone when multiple seats have independent keyboard focus.
    let [seat]: [Value; 1] = serde_json::from_slice(seats).ok()?;
    let id = seat.get("focus")?.as_u64()?;
    sway_client(&serde_json::from_slice(tree).ok()?, id)
}

#[cfg(target_os = "linux")]
fn sway_client(node: &Value, id: u64) -> Option<FocusedWindow> {
    if node.get("id").and_then(Value::as_u64) == Some(id) {
        // Sway can focus an empty workspace or a layout container. A mapped
        // client has a shell or an X11 window id, including floating clients.
        let client = node["shell"].is_string() || node["window"].is_u64();
        if client && node.get("focused")?.as_bool()? {
            return Some(FocusedWindow {
                source: "sway",
                key: id.to_string(),
                app_id: text(node, "app_id").or_else(|| text(&node["window_properties"], "class")),
                title: text(node, "name"),
            });
        }
        return None;
    }
    ["nodes", "floating_nodes"]
        .into_iter()
        .filter_map(|key| node[key].as_array())
        .flatten()
        .find_map(|child| sway_client(child, id))
}

#[cfg(target_os = "linux")]
async fn x11() -> Option<FocusedWindow> {
    let key = String::from_utf8_lossy(&run(&["xdotool", "getactivewindow"]).await?.stdout)
        .trim()
        .to_owned();
    if key.is_empty() {
        return None;
    }
    let title = run(&["xdotool", "getwindowname", &key])
        .await
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|title| !title.is_empty());
    let class = run(&["xprop", "-id", &key, "WM_CLASS"])
        .await
        .and_then(|output| window_class(&String::from_utf8_lossy(&output.stdout)));
    Some(FocusedWindow {
        source: "x11",
        key,
        app_id: class,
        title,
    })
}

#[cfg(target_os = "linux")]
fn text(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The class in what `xprop -id <window> WM_CLASS` prints:
/// `WM_CLASS(STRING) = "instance", "Class"`.
#[cfg(target_os = "linux")]
fn window_class(line: &str) -> Option<String> {
    line.split('"')
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != ",")
        .nth(2)
        .map(str::to_owned)
}

/// A reader from a script, for tests: each call answers the next entry of
/// `windows`, and the last one from then on.
#[doc(hidden)]
pub fn script(windows: Vec<Option<FocusedWindow>>) -> FocusReader {
    let windows = Arc::new(std::sync::Mutex::new(windows));
    Arc::new(move || {
        let next = {
            let mut windows = windows.lock().unwrap_or_else(|error| error.into_inner());
            if windows.len() > 1 {
                windows.remove(0)
            } else {
                windows[0].clone()
            }
        };
        Box::pin(async move { next })
    })
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests {
    use super::*;

    fn window(source: &'static str, key: &str, app_id: Option<&str>) -> FocusedWindow {
        FocusedWindow {
            source,
            key: key.to_owned(),
            app_id: app_id.map(str::to_owned),
            title: None,
        }
    }

    #[test]
    fn niri_names_the_focused_window_by_its_id() {
        // Niri 26 writes app_id and is_focused.
        let window = from_niri(
        r#"{"id":42,"title":"inbox — Mail","app_id":"org.mozilla.thunderbird","pid":81,"workspace_id":1,"is_focused":true,"is_floating":false}"#.as_bytes(),
    )
    .expect("a focused window");
        assert_eq!(
            window,
            FocusedWindow {
                source: "niri",
                key: "42".to_owned(),
                app_id: Some("org.mozilla.thunderbird".to_owned()),
                title: Some("inbox — Mail".to_owned()),
            }
        );
        // Older versions wrote app-id and is-focused.
        let older = from_niri(
            br#"{"id":7,"title":"Term","app-id":"kitty","workspace-id":1,"is-focused":true}"#,
        )
        .expect("a focused window");
        assert_eq!(older.app_id.as_deref(), Some("kitty"));
        // A window without the focus, no window at all, or anything but a window, is no answer.
        assert!(
            from_niri(br#"{"id":42,"title":"Mail","app_id":null,"is_focused":false}"#).is_none()
        );
        assert!(from_niri(b"null").is_none());
        assert!(from_niri(b"niri: error: cannot connect").is_none());
    }

    #[test]
    fn sway_identifies_the_focused_client_in_tiled_and_floating_layouts() {
        let seat = br#"[{"name":"seat0","focus":5}]"#;
        let tiled = br#"{"nodes":[{"nodes":[{"id":5,"name":"Notes","app_id":"editor","shell":"xdg_shell","focused":true}]}]}"#;
        let target = from_sway(seat, tiled).unwrap();
        assert_eq!(target.source, "sway");
        assert_eq!(target.key, "5");
        assert_eq!(target.app_id.as_deref(), Some("editor"));
        assert_eq!(target.title.as_deref(), Some("Notes"));
        let floating = br#"{"nodes":[{"floating_nodes":[{"id":5,"window":314,"window_properties":{"class":"XTerm"},"focused":true}]}]}"#;
        let target = from_sway(seat, floating).unwrap();
        assert!(is_terminal(target.app_id.as_deref(), &[]));
    }

    #[test]
    fn sway_withholds_an_empty_workspace_a_changed_focus_or_an_ambiguous_seat() {
        let seat = br#"[{"focus":5}]"#;
        assert!(from_sway(seat, br#"{"id":5,"type":"workspace","focused":true}"#).is_none());
        let client = br#"{"id":8,"shell":"xdg_shell","focused":true}"#;
        assert!(from_sway(seat, client).is_none());
        assert!(from_sway(br#"[{"focus":8},{"focus":5}]"#, client).is_none());
        assert!(from_sway(br#"[]"#, client).is_none());
        assert!(from_sway(seat, br#"{"id":5,"shell":"xdg_shell","focused":false}"#).is_none());
    }

    #[test]
    fn xprop_names_the_window_class_after_its_instance() {
        assert_eq!(
            window_class("WM_CLASS(STRING) = \" Navigator\", \"Navigator\"\n").as_deref(),
            Some("Navigator")
        );
        assert_eq!(
            window_class("WM_CLASS(STRING) = \"kitty\", \"kitty\"\n").as_deref(),
            Some("kitty")
        );
        assert_eq!(
            window_class("WM_CLASS:  no such property on window #0x0\n"),
            None
        );
    }

    #[test]
    fn terminals_are_known_by_id_or_class_and_by_the_users_own_names() {
        for app in [
            "kitty",
            "org.wezfurlong.wezterm",
            "Alacritty",
            "org.gnome.Terminal",
            "XTerm",
        ] {
            assert!(is_terminal(Some(app), &[]), "{app}");
        }
        assert!(!is_terminal(Some("org.mozilla.firefox"), &[]));
        assert!(!is_terminal(None, &[]));
        assert!(is_terminal(Some("MyTerm"), &["myterm".to_owned()]));
        assert!(!is_terminal(Some("MyTerm"), &[]));
    }

    #[test]
    fn a_message_names_a_window_by_its_application_then_its_title_then_its_id() {
        assert_eq!(window("niri", "1", None).describe(), "1");
        assert_eq!(
            FocusedWindow {
                title: Some("Untitled".to_owned()),
                ..window("x11", "2", None)
            }
            .describe(),
            "Untitled"
        );
        assert_eq!(window("niri", "3", Some("kitty")).describe(), "kitty");
    }

    #[tokio::test]
    async fn a_scripted_reader_answers_its_script_and_repeats_its_last_answer() {
        let reader = script(vec![Some(window("niri", "1", None)), None]);
        assert_eq!(reader().await, Some(window("niri", "1", None)));
        assert_eq!(reader().await, None);
        assert_eq!(reader().await, None);
    }
}
