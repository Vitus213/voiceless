#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `voiceless-desktop`: the desktop host. One window over the console the
//! daemon serves and, on a desktop that has a tray, a tray with what a
//! dictation user reaches for between utterances: open the window, pause,
//! type the last result again, start at login, quit.
//!
//! Nothing of the runtime lives here. The daemon is found through the local
//! control transport and started when absent (`voiceless::desktop::runtime`). Closing the
//! window hides it to the tray or, without one, leaves the host; the
//! daemon keeps serving the hotkey and `voiceless web` either way.
//! Quitting with a session in progress says so first; the session itself
//! is the daemon's and finishes regardless. The decisions are
//! `voiceless::desktop::lifecycle`, in the core, next to their tests.
//!
//! The window gets no Tauri commands: it is the daemon's own console,
//! loaded from its loopback address. When the runtime cannot be reached
//! the window shows the bundled page with the reason.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::ManagerExt as Autostart;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use voiceless::desktop::lifecycle::{self, Autostart as LoginStart, Close, Quit};
use voiceless::desktop::runtime as daemon;
use voiceless::desktop::runtime::RUNTIME_START_TIMEOUT;

const WINDOW: &str = "main";
const TRAY: &str = "main";
/// The argument the login start passes: the window then stays out of the
/// way until the launcher or the tray opens it.
const AT_LOGIN: &str = "--autostart";

/// The runtime connection and the tray the desktop provides.
#[derive(Default)]
struct Desktop {
    bus: Option<daemon::RuntimeClient>,
    tray: bool,
}

/// Where the window points once the runtime answered.
enum Page {
    /// The console's loopback address.
    Console(String),
    /// What to say on the bundled page.
    Message(String),
}

/// The tray's pause and login items, to keep them in step with the runtime.
static PAUSE_ITEM: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();
static LOGIN_ITEM: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();
/// What the bundled page is to say; read when that page has loaded.
static MESSAGE: OnceLock<String> = OnceLock::new();

type Action = Pin<Box<dyn Future<Output = ()> + Send>>;

fn desktop(app: &AppHandle) -> tauri::State<'_, Mutex<Desktop>> {
    app.state::<Mutex<Desktop>>()
}

fn main() {
    let at_login = std::env::args()
        .skip(1)
        .any(|argument| argument == AT_LOGIN);
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AT_LOGIN]),
        ))
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_window(app)
        }))
        .manage(Mutex::new(Desktop::default()))
        .setup(move |app| {
            tauri::async_runtime::spawn(start(app.handle().clone(), at_login));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("the desktop host starts")
        .run(|_app, _event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _event {
                show_window(_app);
            }
        });
}

/// Find or start the runtime, put the tray where the desktop has one, and
/// open the window over the console or over what went wrong.
async fn start(app: AppHandle, at_login: bool) {
    let bus = match daemon::connect().await {
        Ok(bus) => bus,
        Err(error) => {
            open(
                &app,
                Page::Message(format!("无法连接本机后台服务：{error}")),
                at_login,
            );
            return;
        }
    };
    let has_tray = daemon::tray_available(&bus).await;
    {
        let state = desktop(&app);
        let mut desktop = state.lock().unwrap_or_else(|error| error.into_inner());
        desktop.bus = Some(bus.clone());
        desktop.tray = has_tray;
    }
    if has_tray {
        if let Err(error) = tray(&app, &bus).await {
            eprintln!("Voiceless: the tray is not available: {error}");
        }
    }
    let page = match daemon::ensure_console(
        &bus,
        RUNTIME_START_TIMEOUT,
        None,
        &voiceless::models::default_data_dir(),
    )
    .await
    {
        Ok(Some(url)) => {
            eprintln!("Voiceless desktop: console {url}");
            Page::Console(url)
        }
        Ok(None) => Page::Message(
            "后台服务在运行，但没有开启控制台。在 config.toml 里设置 [console] enabled = true，再重启 voiceless daemon。".to_owned(),
        ),
        Err(error) => {
            eprintln!("Voiceless desktop: {error}");
            match error {
                daemon::RuntimeError::TooOld => Page::Message(
                    "正在运行的 voiceless 守护进程早于桌面应用，没有控制台接口。更新 voiceless 并重启守护进程（systemctl --user restart voiceless）后重新打开窗口。".to_owned(),
                ),
                error => Page::Message(format!("无法连接后台服务：{error}")),
            }
        }
    };
    open(&app, page, at_login);
}

/// Build the window over `page` and, unless started at login, bring it
/// forward. The console is the window's first page, so nothing loads twice.
fn open(app: &AppHandle, page: Page, at_login: bool) {
    let built = match page {
        Page::Console(url) => WebviewWindowBuilder::new(
            app,
            WINDOW,
            WebviewUrl::External(url.parse().expect("the daemon's console URL")),
        )
        .title("Voiceless")
        .inner_size(1100.0, 820.0)
        .visible(false)
        .build(),
        Page::Message(text) => {
            let _ = MESSAGE.set(text);
            WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::App("index.html".into()))
                .title("Voiceless")
                .inner_size(1100.0, 820.0)
                .visible(false)
                .on_page_load(|webview, payload| {
                    if payload.event() == PageLoadEvent::Finished {
                        if let Some(text) = MESSAGE.get() {
                            let script = format!(
                                "window.showMessage({})",
                                serde_json::to_string(text).expect("a JSON string")
                            );
                            let _ = webview.eval(&script);
                        }
                    }
                })
                .build()
        }
    };
    let window = match built {
        Ok(window) => window,
        Err(error) => {
            eprintln!("Voiceless: the window could not be built: {error}");
            return;
        }
    };
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            // The runtime is not the window's: hide it to the tray, or leave.
            api.prevent_close();
            close_window(&handle);
        }
    });
    if !at_login {
        show_window(app);
    }
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn close_window(app: &AppHandle) {
    let tray = desktop(app)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .tray;
    match lifecycle::on_close(tray) {
        Close::Hide => {
            if let Some(window) = app.get_webview_window(WINDOW) {
                let _ = window.hide();
            }
        }
        Close::Exit => app.exit(0),
    }
}

/// Who starts the host at login: the system's entries win over the host's own.
fn login_start(app: &AppHandle) -> LoginStart {
    let executable = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let directories = lifecycle::system_autostart_directories();
    let entries = lifecycle::autostart_entries(
        directories.iter().map(std::path::PathBuf::as_path),
        &executable,
    );
    lifecycle::autostart_state(app.autolaunch().is_enabled().unwrap_or(false), &entries)
}

/// The tray: open, pause, repaste, login start, quit. Built only where a
/// tray host is registered.
async fn tray(app: &AppHandle, bus: &daemon::RuntimeClient) -> tauri::Result<()> {
    let paused = daemon::paused(bus).await.ok().flatten().unwrap_or(false);
    let open = MenuItem::with_id(app, "open", "打开窗口", true, None::<&str>)?;
    let pause_item = CheckMenuItem::with_id(app, "pause", "暂停听写", true, paused, None::<&str>)?;
    let repaste = MenuItem::with_id(app, "repaste", "重贴上次结果", true, None::<&str>)?;
    let login = match login_start(app) {
        LoginStart::Off => {
            CheckMenuItem::with_id(app, "login", "登录时启动", true, false, None::<&str>)?
        }
        LoginStart::App => {
            CheckMenuItem::with_id(app, "login", "登录时启动", true, true, None::<&str>)?
        }
        LoginStart::System => CheckMenuItem::with_id(
            app,
            "login",
            "登录时启动（由系统管理）",
            false,
            true,
            None::<&str>,
        )?,
    };
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &pause_item,
            &repaste,
            &login,
            &PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;
    let _ = PAUSE_ITEM.set(pause_item);
    let _ = LOGIN_ITEM.set(login);
    let icon = app
        .default_window_icon()
        .cloned()
        .expect("the bundled icon");
    TrayIconBuilder::with_id(TRAY)
        .icon(icon)
        .tooltip("Voiceless")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "pause" => on_bus(app, move |bus| Box::pin(pause(bus))),
            "repaste" => on_bus(app, |bus| {
                Box::pin(async move {
                    if let Err(error) = daemon::repaste(&bus).await {
                        eprintln!("Voiceless: {error}");
                    }
                })
            }),
            "login" => toggle_login(),
            "quit" => quit(app.clone()),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Run `action` with the session bus, once there is one.
fn on_bus<F>(app: &AppHandle, action: F)
where
    F: FnOnce(daemon::RuntimeClient) -> Action + Send + 'static,
{
    let bus = desktop(app)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .bus
        .clone();
    tauri::async_runtime::spawn(async move {
        if let Some(bus) = bus {
            action(bus).await;
        }
    });
}

async fn pause(bus: daemon::RuntimeClient) {
    let paused = daemon::paused(&bus).await.ok().flatten().unwrap_or(false);
    match daemon::set_paused(&bus, !paused).await {
        Ok(()) => {
            if let Some(item) = PAUSE_ITEM.get() {
                let _ = item.set_checked(!paused);
                let _ = item.set_text(if paused {
                    "暂停听写"
                } else {
                    "继续听写"
                });
            }
        }
        Err(error) => eprintln!("Voiceless: {error}"),
    }
}

fn toggle_login() {
    let Some(item) = LOGIN_ITEM.get() else {
        return;
    };
    let app = item.app_handle().clone();
    let enabled = item.is_checked().unwrap_or(false);
    let result = if enabled {
        app.autolaunch().disable()
    } else {
        app.autolaunch().enable()
    };
    if let Err(error) = result {
        eprintln!("Voiceless: cannot change the login start: {error}");
        let _ = item.set_checked(!enabled);
    }
}

/// Quitting is the one act that looks at the runtime first: a session in
/// progress goes on in the daemon, and the user is told so before the tray
/// goes away.
fn quit(app: AppHandle) {
    let borrowed = app.clone();
    on_bus(&borrowed, move |bus| {
        let app = app.clone();
        Box::pin(async move {
            let state = daemon::runtime_state(&bus)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "idle".to_owned());
            match lifecycle::on_quit(&state) {
                Quit::Now => app.exit(0),
                Quit::Announce(state) => {
                    let quitting = app.clone();
                    app.dialog()
                        .message(format!(
                            "听写正在进行（{state}）。退出桌面窗口不会打断它：后台服务会完成识别和上屏，快捷键和 voiceless web 照常可用。"
                        ))
                        .title("Voiceless")
                        .kind(MessageDialogKind::Info)
                        .buttons(MessageDialogButtons::OkCancelCustom("退出".into(), "取消".into()))
                        .show(move |answer| {
                            if answer {
                                quitting.exit(0);
                            }
                        });
                }
            }
        })
    });
}
