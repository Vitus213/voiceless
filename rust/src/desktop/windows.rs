//! Windows desktop adapter: which window the user is typing in, the
//! configured shortcut chords, the clipboard with one guarded paste, and
//! the default browser.
//!
//! * Focus is the foreground window. Its handle and owning process tell one
//!   window from another; the program behind the focused child window names
//!   the application.
//! * Chords come from a `WH_KEYBOARD_LL` hook on a thread of its own. The
//!   callback classifies the keys that carry a chord and returns at once,
//!   and the thread's message loop forwards what it produced. Modifiers are
//!   read from the system when a chord key goes down, so a release the hook
//!   could not see (a lock screen or a UAC prompt took the desktop) leaves
//!   nothing pressed.
//! * The paste is one `SendInput` burst of Ctrl+V, sent only while the
//!   window recorded at the start of the dictation is still in front and
//!   Windows is known to take input into it. The transcript goes on the
//!   clipboard as `CF_UNICODETEXT`; the text that was there is put back
//!   once the application had time to read, unless someone else copied in
//!   the meantime.
//!
//! Every function here blocks for milliseconds at a time and needs no async
//! runtime: call them from a thread that may block.
//!
//! Behavior follows the Windows API documentation: LowLevelKeyboardProc,
//! SendInput, GetAsyncKeyState, KBDLLHOOKSTRUCT, OpenClipboard,
//! EmptyClipboard, SetClipboardData, Standard Clipboard Formats, Clipboard
//! Formats (cloud clipboard and clipboard history), OpenProcessToken,
//! GetWindowTextW, ShellExecuteW, and "Windows Integrity Mechanism Design"
//! for the integrity levels behind User Interface Privilege Isolation.

#[path = "windows/keys.rs"]
mod keys;
#[path = "windows/shortcuts.rs"]
mod shortcuts;
pub(crate) use keys::key_code;

use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, error::TrySendError};
use windows_sys::w;
use windows_sys::Win32::Foundation::{
    CloseHandle, GlobalFree, ERROR_INVALID_DATA, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT,
    WAIT_FAILED, WPARAM,
};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    SECURITY_MAX_SID_SIZE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, Sleep, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, MapVirtualKeyExW, SendInput, HKL, INPUT, INPUT_0,
    INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VK_LCONTROL, VK_LMENU, VK_LSHIFT,
    VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_V,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteW, SE_ERR_ACCESSDENIED, SE_ERR_NOASSOC};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetGUIThreadInfo, GetMessageW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, GUITHREADINFO, HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, LLKHF_UP, MSG, MWMO_INPUTAVAILABLE, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT,
    SW_SHOWNORMAL, WH_KEYBOARD_LL, WM_APP,
};

use super::native_keys::Chord;
use crate::desktop::focus::FocusedWindow;
use crate::hotkey::KeyEvent;

use keys::{ALT, CTRL, SHIFT, SUPER};

/// How long the hook thread gets to install its hook before [`start_keys`]
/// gives up on it.
const HOOK_START: Duration = Duration::from_secs(5);
/// How long another program may keep the clipboard busy before a copy or a
/// paste is given up.
const CLIPBOARD_BUSY: Duration = Duration::from_secs(2);
/// The pause between two attempts on a busy clipboard.
const CLIPBOARD_POLL: Duration = Duration::from_millis(10);
/// How long Shift, Alt or Win may stay down before the paste is abandoned.
const MODIFIER_RELEASE: Duration = Duration::from_millis(300);
/// How long the focused application gets to read the transcript before the
/// earlier clipboard text returns.
const PASTE_SETTLE: Duration = Duration::from_millis(300);
/// Posted to the hook thread to make its message loop look at its queue again.
const WAKE: u32 = WM_APP;
/// `CF_UNICODETEXT`: Unicode text whose lines end with CRLF and which ends
/// with a NUL.
const CF_UNICODETEXT: u32 = 13;
/// Integrity levels as winnt.h defines them (`SECURITY_MANDATORY_*_RID`):
/// the last sub-authority of the integrity SID in a token.
const LOW: u32 = 0x1000;
const MEDIUM: u32 = 0x2000;
const HIGH: u32 = 0x3000;
const SYSTEM: u32 = 0x4000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the last system call on this thread failed with.
fn failure(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

/// A kernel handle, closed on every exit path.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this struct owns the handle, exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

// Focus.

/// The window the user is working in, or `None` while the desktop has none.
pub(crate) fn focused_window() -> Option<FocusedWindow> {
    let target = foreground()?;
    Some(FocusedWindow {
        source: "windows",
        key: target.key,
        app_id: target.app_id,
        title: target.title,
    })
}

/// One reading of the foreground window.
struct Target {
    /// The thread that created `window`: it owns the keyboard layout and
    /// the keyboard focus.
    thread: u32,
    /// The window handle in hexadecimal and its process: a handle alone is
    /// reused once its window is gone.
    key: String,
    /// The process that owns `window`.
    process: u32,
    /// The process of the focused child window when it is another one: the
    /// window of a Store application is a frame of a system process around
    /// content of its own.
    content: Option<u32>,
    app_id: Option<String>,
    title: Option<String>,
}

impl Target {
    /// The processes whose integrity level decides whether input reaches
    /// the window.
    fn processes(&self) -> impl Iterator<Item = u32> + '_ {
        std::iter::once(self.process).chain(self.content)
    }

    fn describe(&self) -> &str {
        self.app_id
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or(&self.key)
    }
}

/// The foreground window; `None` while the desktop has none, as when the
/// screen is locked, a secure desktop is up, or a window is losing
/// activation.
fn foreground() -> Option<Target> {
    // SAFETY: queries of window properties. Each answers zero or NULL for a
    // window that is gone.
    unsafe {
        let window = GetForegroundWindow();
        let (thread, process) = process_of(window)?;
        let content = focused_child(thread);
        let (_, content_pid) = process_of(content)?;
        Some(Target {
            // Include the focus control: two editors can share a top-level window.
            thread,
            key: format!(
                "{:x}:{process}/{:x}:{content_pid}",
                window as usize, content as usize
            ),
            process,
            content: (content_pid != process).then_some(content_pid),
            app_id: application_id(content),
            title: window_title(window),
        })
    }
}

/// The thread that created `window` and the process that owns it; neither
/// is zero for a window that exists.
unsafe fn process_of(window: HWND) -> Option<(u32, u32)> {
    let mut process = 0u32;
    let thread = GetWindowThreadProcessId(window, &mut process);
    (thread != 0 && process != 0).then_some((thread, process))
}

/// The window with the keyboard focus on `thread`, or NULL when Windows
/// does not say.
unsafe fn focused_child(thread: u32) -> HWND {
    let mut info = GUITHREADINFO::default();
    info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    if GetGUIThreadInfo(thread, &mut info) == 0 {
        return std::ptr::null_mut();
    }
    info.hwndFocus
}

/// The title of `window`, for display. Windows copies it from its own record
/// for a window of another program, so a program that hangs cannot hang this
/// one.
unsafe fn window_title(window: HWND) -> Option<String> {
    let length = GetWindowTextLengthW(window);
    if length <= 0 {
        return None;
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32);
    let title = String::from_utf16_lossy(&buffer[..copied as usize]);
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// The program behind `window`, named by its executable without its
/// extension and in lower case: `notepad`, `alacritty`, `windowsterminal`.
/// `None` when Windows does not let this process look.
unsafe fn application_id(window: HWND) -> Option<String> {
    let (_, process) = process_of(window)?;
    let process = open_process(process)?;
    let image = image_name(process.0)?;
    let program = Path::new(&image).file_stem()?;
    Some(program.to_string_lossy().to_lowercase())
}

/// A process opened for the little this adapter asks of it.
unsafe fn open_process(process: u32) -> Option<OwnedHandle> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process);
    (!handle.is_null()).then(|| OwnedHandle(handle))
}

/// The full path of a process's executable.
unsafe fn image_name(process: HANDLE) -> Option<OsString> {
    // A path longer than MAX_PATH is rare; a failed first call then says
    // how long it is.
    let mut buffer = [0u16; 260];
    let mut length = buffer.len() as u32;
    if QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_WIN32,
        buffer.as_mut_ptr(),
        &mut length,
    ) != 0
    {
        return Some(OsString::from_wide(&buffer[..length as usize]));
    }
    let mut longer = vec![0u16; 32768];
    length = longer.len() as u32;
    if QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_WIN32,
        longer.as_mut_ptr(),
        &mut length,
    ) == 0
    {
        return None;
    }
    longer.truncate(length as usize);
    Some(OsString::from_wide(&longer))
}

/// The foreground window when it is still the one `expected_key` names;
/// otherwise why it is not.
fn confirm_focus(expected_key: &str) -> Result<Target, String> {
    confirm_observed_focus(expected_key, foreground())
}

fn confirm_observed_focus(expected_key: &str, observed: Option<Target>) -> Result<Target, String> {
    match (observed, expected_key.is_empty()) {
        (Some(target), false) if target.key == expected_key => Ok(target),
        (Some(target), false) => Err(format!(
            "the focused window changed to {}",
            target.describe()
        )),
        _ => Err("the original focused window could not be confirmed".to_owned()),
    }
}

// Permissions and integrity.

/// Check desktop microphone consent without starting an audio stream. Missing
/// overrides inherit Windows defaults; WASAPI checks effective policy at open.
pub(crate) fn microphone_permission() -> Result<(), String> {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS,
    };
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
    };
    let device = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone");
    let desktop = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone\\NonPackaged");
    for (hive, path) in [
        (HKEY_LOCAL_MACHINE, device),
        (HKEY_CURRENT_USER, device),
        (HKEY_CURRENT_USER, desktop),
    ] {
        let mut value = [0u16; 32];
        let mut size = std::mem::size_of_val(&value) as u32;
        let result = unsafe {
            RegGetValueW(
                hive,
                path,
                w!("Value"),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut size,
            )
        };
        match result {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => continue,
            ERROR_SUCCESS => {
                let length = value
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(value.len());
                if String::from_utf16_lossy(&value[..length]).eq_ignore_ascii_case("Deny") {
                    return Err("Microphone access is denied. In Windows Settings > Privacy > Microphone, enable microphone access for desktop apps.".to_owned());
                }
            }
            code => {
                return Err(format!(
                    "cannot read Windows microphone privacy settings: {}",
                    io::Error::from_raw_os_error(code as i32)
                ))
            }
        }
    }
    Ok(())
}

pub(crate) fn request_microphone_permission() -> Result<(), String> {
    microphone_permission()
}

/// Whether this process can hear the shortcut and type into the windows of
/// the signed-in desktop, and otherwise why not.
///
/// Windows has no switch and no prompt for global input: any program of the
/// interactive desktop may install the keyboard hook and call `SendInput`.
/// What it refuses is input into a window of a higher integrity level than
/// its own, which [`paste_text`] reports when it happens.
pub(crate) fn input_permission() -> Result<(), String> {
    // SAFETY: names of this process's window station and of this thread's
    // desktop, and a query of this process's token.
    unsafe {
        let station = user_object_name(GetProcessWindowStation())?;
        let desktop = user_object_name(GetThreadDesktop(GetCurrentThreadId()))?;
        if !station.eq_ignore_ascii_case("WinSta0") || !desktop.eq_ignore_ascii_case("Default") {
            return Err(format!(
                "Voiceless runs on the window station {station}\\{desktop}, not on the signed-in \
                 desktop WinSta0\\Default where shortcuts and pasted text arrive; start it from \
                 that desktop, not from a service, a scheduled task or an SSH session"
            ));
        }
        let own = integrity(GetCurrentProcessId()).map_err(|error| {
            format!("Windows does not say the integrity level of this process: {error}")
        })?;
        if own < MEDIUM {
            return Err(format!(
                "this process runs at {} integrity, from where Windows refuses input into the \
                 medium-integrity windows of the desktop",
                integrity_name(own)
            ));
        }
    }
    Ok(())
}

/// The same check: Windows has nothing to ask the user for global input,
/// so a request can only say whether input works.
pub(crate) fn request_input_permission() -> Result<(), String> {
    input_permission()
}

/// The name of a window station or desktop.
unsafe fn user_object_name(object: HANDLE) -> Result<String, String> {
    let mut name = [0u16; 256];
    let mut needed = 0u32;
    if GetUserObjectInformationW(
        object,
        UOI_NAME,
        name.as_mut_ptr().cast(),
        std::mem::size_of_val(&name) as u32,
        &mut needed,
    ) == 0
    {
        return Err(failure("GetUserObjectInformationW(UOI_NAME)"));
    }
    let length = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..length]))
}

/// The integrity level of a process, as the RID of its token's mandatory
/// label.
unsafe fn integrity(process: u32) -> io::Result<u32> {
    let token = token_of(process)?;
    // The token API writes the SID after the aligned mandatory label.
    #[repr(C)]
    struct Buffer {
        label: TOKEN_MANDATORY_LABEL,
        sid: [u8; SECURITY_MAX_SID_SIZE as usize],
    }
    let mut buffer = Buffer {
        label: TOKEN_MANDATORY_LABEL::default(),
        sid: [0; SECURITY_MAX_SID_SIZE as usize],
    };
    let mut returned = 0u32;
    if GetTokenInformation(
        token.0,
        TokenIntegrityLevel,
        (&mut buffer as *mut Buffer).cast(),
        std::mem::size_of::<Buffer>() as u32,
        &mut returned,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid = buffer.label.Label.Sid;
    if sid.is_null() {
        return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
    }
    let count = *GetSidSubAuthorityCount(sid);
    if count == 0 {
        return Err(io::Error::from_raw_os_error(ERROR_INVALID_DATA as i32));
    }
    Ok(*GetSidSubAuthority(sid, u32::from(count) - 1))
}

/// The token of a process, opened to be asked about it.
unsafe fn token_of(process_id: u32) -> io::Result<OwnedHandle> {
    let own = process_id == GetCurrentProcessId();
    // The current process is a pseudo-handle, which is not closed.
    let process = if own {
        GetCurrentProcess()
    } else {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        process
    };
    let _process = (!own).then(|| OwnedHandle(process));
    let mut token = std::ptr::null_mut();
    if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(token))
}

fn integrity_name(level: u32) -> &'static str {
    match level {
        SYSTEM.. => "system",
        HIGH.. => "high (elevated)",
        MEDIUM.. => "medium",
        LOW.. => "low",
        _ => "untrusted",
    }
}

/// Why Windows would drop input meant for `target`, or `None` when it takes
/// it. User Interface Privilege Isolation lets a process send input only to
/// windows of an equal or a lower integrity level, and `SendInput` reports
/// nothing when it drops the keystrokes; so the levels are compared here,
/// before anything is sent.
fn refusal_for(target: &Target) -> Option<String> {
    // SAFETY: read-only queries of tokens.
    unsafe {
        let own = match integrity(GetCurrentProcessId()) {
            Ok(level) => level,
            Err(error) => return Some(format!("cannot verify Voiceless integrity level: {error}")),
        };
        let app = target.describe();
        target
            .processes()
            .find_map(|process| match integrity(process) {
                Ok(level) if level <= own => None,
                Ok(level) => Some(format!(
                "Windows refuses input from this {}-integrity process into {app}, which runs at {} \
                 integrity; dictate into a window at the same integrity level",
                integrity_name(own),
                integrity_name(level)
            )),
                Err(error) => Some(format!(
                    "Windows does not say the integrity level of the process of {app} ({error})"
                )),
            })
    }
}

// Shortcut chords.

/// Owns the keyboard hook. Dropping it removes the hook, and only then
/// lets its thread go.
pub(crate) struct KeyListener {
    thread: Option<JoinHandle<()>>,
    /// The message queue the hook thread serves; zero without a hook.
    thread_id: u32,
    stop: Arc<AtomicBool>,
}

/// Starts hearing `chords` from every keyboard of the desktop and reports
/// their transitions on `events` until the listener or the receiver is
/// dropped. Keys without a chord reach the hook and are forgotten there.
pub(crate) fn start_keys(
    chords: Arc<[Chord]>,
    events: mpsc::Sender<KeyEvent>,
) -> Result<KeyListener, String> {
    let stop = Arc::new(AtomicBool::new(false));
    if chords.is_empty() {
        return Ok(KeyListener {
            thread: None,
            thread_id: 0,
            stop,
        });
    }
    input_permission()?;
    let (report, installed) = std::sync::mpsc::channel();
    let running = stop.clone();
    let thread = std::thread::Builder::new()
        .name("voiceless-keys".to_owned())
        .spawn(move || serve(chords, events, running, report))
        .map_err(|error| format!("cannot start the keyboard hook thread: {error}"))?;
    let mut listener = KeyListener {
        thread: Some(thread),
        thread_id: 0,
        stop,
    };
    match installed.recv_timeout(HOOK_START) {
        Ok(Ok(thread_id)) => listener.thread_id = thread_id,
        Ok(Err(error)) => {
            drop(listener);
            return Err(error);
        }
        Err(_) => {
            drop(listener);
            return Err("the keyboard hook thread did not answer in time".to_owned());
        }
    }
    Ok(listener)
}

impl Drop for KeyListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // SAFETY: the id is the hook thread's; a thread that already
            // left makes the post fail harmlessly.
            unsafe { PostThreadMessageW(self.thread_id, WAKE, 0, 0) };
            let _ = thread.join();
        }
    }
}

/// An installed hook, removed on every exit path.
struct Hook(HHOOK);

impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: a handle `SetWindowsHookExW` returned that nothing else
        // removed.
        unsafe { UnhookWindowsHookEx(self.0) };
    }
}

/// What the hook thread owns while it serves the hook.
struct HookRun {
    tracker: Mutex<keys::ChordTracker>,
    events: mpsc::Sender<KeyEvent>,
    /// Events the channel had no room for, first in line once it has.
    pending: Mutex<VecDeque<KeyEvent>>,
    /// The receiver is gone: the hook has nobody left to tell.
    closed: AtomicBool,
    stop: Arc<AtomicBool>,
}

thread_local! {
    /// The run served by this thread's hook; NULL once the hook thread stops.
    static RUN: Cell<*const HookRun> = const { Cell::new(std::ptr::null_mut()) };
}

/// The hook thread: installs the hook, then runs the message loop Windows
/// calls it through.
fn serve(
    chords: Arc<[Chord]>,
    events: mpsc::Sender<KeyEvent>,
    stop: Arc<AtomicBool>,
    report: std::sync::mpsc::Sender<Result<u32, String>>,
) {
    let _reservations = match shortcuts::reserve(&chords) {
        Ok(reservations) => reservations,
        Err(error) => {
            let _ = report.send(Err(error));
            return;
        }
    };
    let run = HookRun {
        tracker: Mutex::new(keys::ChordTracker::new(chords)),
        events,
        pending: Mutex::new(VecDeque::new()),
        closed: AtomicBool::new(false),
        stop,
    };
    // SAFETY: this thread is the only one that uses `run`, and it owns it
    // until `RUN` is cleared again.
    unsafe {
        // Windows reaches the hook through this thread's message queue:
        // the queue has to exist before anyone can post to it.
        let mut message = MSG::default();
        PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        let module = GetModuleHandleW(std::ptr::null());
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0);
        if hook.is_null() {
            let _ = report.send(Err(failure("SetWindowsHookExW(WH_KEYBOARD_LL)")));
            return;
        }
        let _hook = Hook(hook);
        let notifications = ClipboardOwner::new().and_then(|window| {
            use windows_sys::Win32::UI::Input::{
                RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_DEVNOTIFY,
            };
            use windows_sys::Win32::UI::Shell::SetWindowSubclass;
            if SetWindowSubclass(window.0, Some(device_change), 1, 0) == 0 {
                return Err(failure("install keyboard device notification handler"));
            }
            let keyboard = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_DEVNOTIFY,
                hwndTarget: window.0,
            };
            if RegisterRawInputDevices(&keyboard, 1, std::mem::size_of_val(&keyboard) as u32) == 0 {
                return Err(failure("register keyboard device notifications"));
            }
            Ok(window)
        });
        let _window = match notifications {
            Ok(window) => window,
            Err(error) => {
                let _ = report.send(Err(error));
                return;
            }
        };
        RUN.with(|served| served.set(&run));
        let _ = report.send(Ok(GetCurrentThreadId()));
        while !run.stopped() {
            let mut message = MSG::default();
            // Zero answers WM_QUIT and -1 an error: both end the loop.
            if GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) <= 0 {
                break;
            }
            DispatchMessageW(&message);
            run.flush();
        }
        RUN.with(|served| served.set(std::ptr::null_mut()));
    }
    // `_hook` is dropped here: the hook is gone before the thread ends, and
    // so before a dropped listener returns.
}

unsafe extern "system" fn device_change(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GIDC_REMOVAL, WM_INPUT_DEVICE_CHANGE};
    if message == WM_INPUT_DEVICE_CHANGE && wparam == GIDC_REMOVAL as usize {
        RUN.with(|served| {
            if let Some(run) = served.get().as_ref() {
                let mut tracker = lock(&run.tracker);
                tracker.disconnect(Instant::now());
                for event in tracker.events.drain(..) {
                    run.forward(event);
                }
            }
        });
    }
    windows_sys::Win32::UI::Shell::DefSubclassProc(window, message, wparam, lparam)
}

/// Windows calls this for every keystroke of the desktop, on the thread
/// that installed the hook and under a deadline past which the hook is
/// removed without a word. So it only classifies the key and returns what
/// the chain returns.
unsafe extern "system" fn hook_proc(code: i32, _message: WPARAM, event: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        RUN.with(|served| {
            let run = served.get();
            if !run.is_null() {
                // SAFETY: `RUN` is set for exactly as long as the hook
                // thread owns the run, and `HC_ACTION` promises a
                // KBDLLHOOKSTRUCT.
                unsafe { (*run).on_key(&*(event as *const KBDLLHOOKSTRUCT)) };
            }
        });
    }
    // SAFETY: passing the event on is what a keystroke nobody held back
    // needs.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, _message, event) }
}

impl HookRun {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire)
    }

    /// Folds one keystroke in. Keys without a chord are forgotten here;
    /// nothing of them is kept.
    fn on_key(&self, event: &KBDLLHOOKSTRUCT) {
        // Injected by any program, this one's own Ctrl+V included.
        if event.flags & LLKHF_INJECTED != 0 {
            return;
        }
        let Ok(key) = u16::try_from(event.vkCode) else {
            return;
        };
        let mut tracker = lock(&self.tracker);
        if !tracker.watches(key) {
            return;
        }
        let down = event.flags & LLKHF_UP == 0;
        tracker.on_key(key, down, held_modifiers(key), Instant::now(), key_down);
        for event in tracker.events.drain(..) {
            if !self.forward(event) {
                break;
            }
        }
    }

    /// Hands an event to the daemon without waiting; `false` once nobody
    /// receives.
    fn forward(&self, event: KeyEvent) -> bool {
        let mut pending = lock(&self.pending);
        if !pending.is_empty() {
            pending.push_back(event);
            return true;
        }
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(event)) => {
                pending.push_back(event);
                self.wake();
                true
            }
            Err(TrySendError::Closed(_)) => {
                self.closed.store(true, Ordering::Release);
                self.wake();
                false
            }
        }
    }

    /// Sends what the channel had no room for, now that it may have.
    fn flush(&self) {
        while !self.stopped() {
            let event = lock(&self.pending).pop_front();
            let Some(event) = event else {
                return;
            };
            match self.events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    lock(&self.pending).push_front(event);
                    std::thread::sleep(Duration::from_millis(2));
                    self.wake();
                    return;
                }
                Err(TrySendError::Closed(_)) => self.closed.store(true, Ordering::Release),
            }
        }
        lock(&self.pending).clear();
    }

    fn wake(&self) {
        // SAFETY: this thread's own message queue.
        unsafe { PostThreadMessageW(GetCurrentThreadId(), WAKE, 0, 0) };
    }
}

/// The modifier keys that are down, as `Chord::modifiers` bits. They are
/// read from the system at every chord key instead of being followed through
/// the hook: a release the hook never saw then misleads nobody.
fn held_modifiers(key: u16) -> u8 {
    const HELD: [(u16, u8); 8] = [
        (VK_LCONTROL, CTRL),
        (VK_RCONTROL, CTRL),
        (VK_LMENU, ALT),
        (VK_RMENU, ALT),
        (VK_LSHIFT, SHIFT),
        (VK_RSHIFT, SHIFT),
        (VK_LWIN, SUPER),
        (VK_RWIN, SUPER),
    ];
    let held = HELD
        .iter()
        .filter(|(modifier, _)| key_down(*modifier))
        .fold(0, |held, (_, bit)| held | bit);
    // A chord on a modifier key itself does not count that key as its own
    // modifier, as on Linux.
    held & !keys::modifier_bit(key).unwrap_or(0)
}

/// Whether `key` is down. Inside the hook, Windows has not updated the
/// state of the key of the event yet, which is why it is never asked about.
fn key_down(key: u16) -> bool {
    // SAFETY: reads whether a key is down. Zero, the answer when Windows
    // refuses to say, reads as up.
    unsafe { GetAsyncKeyState(i32::from(key)) as u16 & 0x8000 != 0 }
}

// Clipboard and paste.

/// One clipboard user at a time in this process, so that a second paste
/// never takes the transcript of the first for the user's clipboard.
static CLIPBOARD: Mutex<()> = Mutex::new(());

/// A message-only window of the calling thread that owns what this process
/// puts on the clipboard: `SetClipboardData` fails for an owner-less
/// clipboard. It is destroyed before its thread stops answering messages,
/// so that nobody is left waiting on it.
struct ClipboardOwner(HWND);

impl ClipboardOwner {
    fn new() -> Result<Self, String> {
        // SAFETY: a window of the system's own STATIC class, parented to
        // HWND_MESSAGE: it shows nothing, receives no input and belongs to
        // this thread.
        let window = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        (!window.is_null())
            .then(|| Self(window))
            .ok_or_else(|| failure("CreateWindowExW(HWND_MESSAGE)"))
    }

    /// Opens the clipboard, waiting for a program that has it open to let
    /// go of it.
    fn open(&self) -> Result<ClipboardOpen, String> {
        let deadline = Instant::now() + CLIPBOARD_BUSY;
        loop {
            // SAFETY: a live window of this thread.
            if unsafe { OpenClipboard(self.0) } != 0 {
                return Ok(ClipboardOpen);
            }
            let error = failure("OpenClipboard");
            if Instant::now() >= deadline {
                return Err(format!("{error}; another program keeps the clipboard busy"));
            }
            wait(CLIPBOARD_POLL);
        }
    }

    /// Saved Unicode text including its terminator, ready for SetClipboardData.
    fn text(&self) -> Result<Option<Vec<u16>>, String> {
        // The caller holds the clipboard across the snapshot and replacement.
        // SAFETY: the clipboard is open for this thread.
        let data = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if data.is_null() {
            return Ok(None);
        }
        // SAFETY: `data` is the clipboard's global memory for
        // CF_UNICODETEXT, good for as long as the clipboard stays open.
        unsafe { global_text(data) }
            .map(Some)
            .ok_or_else(|| failure("read clipboard text"))
    }

    /// Replaces the clipboard with `text`, a terminated `CF_UNICODETEXT`,
    /// and makes this window the clipboard's owner. The clipboard has to be
    /// open on this thread.
    fn set(&self, text: &[u16]) -> Result<(), String> {
        // SAFETY: the clipboard is open for this thread through this
        // window, of which `EmptyClipboard` makes the owner.
        unsafe {
            let data = global_memory(text)?;
            if EmptyClipboard() == 0 {
                let error = failure("EmptyClipboard");
                GlobalFree(data);
                return Err(error);
            }
            if SetClipboardData(CF_UNICODETEXT, data).is_null() {
                let error = failure("SetClipboardData(CF_UNICODETEXT)");
                GlobalFree(data);
                return Err(error);
            }
            // Keeps the transcript out of the clipboard history and off the
            // cloud clipboard. These are hints: a program that ignores them
            // still pastes, so nothing turns on their failure.
            for name in [
                w!("ExcludeClipboardContentFromMonitorProcessing"),
                w!("CanIncludeInClipboardHistory"),
                w!("CanUploadToCloudClipboard"),
            ] {
                let format = RegisterClipboardFormatW(name);
                if format == 0 {
                    continue;
                }
                if let Ok(data) = global_memory(&[0, 0]) {
                    if SetClipboardData(format, data).is_null() {
                        GlobalFree(data);
                    }
                }
            }
        }
        Ok(())
    }

    /// Puts `text` back, unless the clipboard belongs to someone else by
    /// now: a copy the user made in the meantime stays.
    fn restore(&self, text: Option<Vec<u16>>) -> Result<(), String> {
        let _open = self.open()?;
        // SAFETY: the clipboard is open for this thread.
        if unsafe { GetClipboardOwner() } != self.0 {
            return Ok(());
        }
        match text {
            Some(text) => self.set(&text),
            None if unsafe { EmptyClipboard() } != 0 => Ok(()),
            None => Err(failure("restore empty clipboard")),
        }
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        // SAFETY: the window was created on this thread and nothing else
        // destroys it.
        unsafe { DestroyWindow(self.0) };
    }
}

/// The clipboard held open by this thread; closed on every exit path.
struct ClipboardOpen;

impl Drop for ClipboardOpen {
    fn drop(&mut self) {
        // SAFETY: made only right after `OpenClipboard` succeeded on this
        // thread.
        unsafe { CloseClipboard() };
    }
}

/// Copies the text in the clipboard's global memory `data`, up to its
/// terminator.
unsafe fn global_text(data: HANDLE) -> Option<Vec<u16>> {
    let source = GlobalLock(data).cast::<u16>();
    if source.is_null() {
        return None;
    }
    let units = std::slice::from_raw_parts(source, GlobalSize(data) / std::mem::size_of::<u16>());
    let length = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    let mut text = Vec::with_capacity(length + 1);
    text.extend_from_slice(&units[..length]);
    text.push(0);
    GlobalUnlock(data);
    Some(text)
}

/// `text` in movable global memory, the form the clipboard takes.
unsafe fn global_memory(text: &[u16]) -> Result<HGLOBAL, String> {
    let bytes = std::mem::size_of_val(text);
    let data = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if data.is_null() {
        let error = io::Error::last_os_error();
        return Err(format!(
            "GlobalAlloc({bytes} bytes) for the clipboard: {error}"
        ));
    }
    let target = GlobalLock(data).cast::<u16>();
    if target.is_null() {
        let error = failure("GlobalLock");
        GlobalFree(data);
        return Err(error);
    }
    std::ptr::copy_nonoverlapping(text.as_ptr(), target, text.len());
    GlobalUnlock(data);
    Ok(data)
}

/// Puts `text` on the clipboard for the user to paste. What was there is
/// replaced.
pub(crate) fn copy_text(text: &str) -> Result<(), String> {
    let _turn = lock(&CLIPBOARD);
    let owner = ClipboardOwner::new()?;
    let _open = owner.open()?;
    owner.set(&keys::clipboard_text(text))
}

/// Pastes `text` into the window whose key is `expected_key`
/// ([`FocusedWindow::key`]): the transcript goes on the clipboard and one
/// Ctrl+V follows.
///
/// `Ok(())` means Windows took the four keystrokes; whether the application
/// made text of them is not something `SendInput` can tell, so the caller
/// reports the delivery as unconfirmed. An `Err` means nothing was sent,
/// and nothing of the user's was touched unless the message says so. The
/// keystrokes are never sent twice.
pub(crate) fn paste_text(text: &str, expected_key: &str) -> Result<(), String> {
    let _turn = lock(&CLIPBOARD);
    // Refused before anything of the user's is touched.
    let target = confirm_focus(expected_key)?;
    if let Some(reason) = refusal_for(&target) {
        return Err(reason);
    }
    let owner = ClipboardOwner::new()?;
    let previous = {
        let _open = owner.open()?;
        // A text-only snapshot cannot restore images, HTML or delayed formats.
        // Preserve the original clipboard when it contains non-text data.
        use windows_sys::Win32::System::DataExchange::EnumClipboardFormats;
        let metadata = unsafe {
            [
                RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
                RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")),
                RegisterClipboardFormatW(w!("CanUploadToCloudClipboard")),
            ]
        };
        let mut format = unsafe { EnumClipboardFormats(0) };
        while format != 0 {
            if !matches!(format, 1 | 7 | CF_UNICODETEXT | 16) && !metadata.contains(&format) {
                return Err(
                    "clipboard contains non-text data; use explicit copy to replace it".to_owned(),
                );
            }
            format = unsafe { EnumClipboardFormats(format) };
        }
        let previous = owner.text()?;
        owner.set(&keys::clipboard_text(text))?;
        previous
    };
    let pasted = paste_into(&owner, expected_key);
    // A failed restore only leaves the transcript on the clipboard: the
    // paste is neither repeated nor taken back.
    let _ = owner.restore(previous);
    pasted
}

/// The keystroke, sent only once the way is clear.
fn paste_into(owner: &ClipboardOwner, expected_key: &str) -> Result<(), String> {
    release_modifiers()?;
    // The clipboard steps and the wait took time: this is the last look
    // before the keystroke.
    if unsafe { GetClipboardOwner() } != owner.0 {
        return Err("clipboard changed before paste".to_owned());
    }
    let current = confirm_focus(expected_key)?;
    if let Some(reason) = refusal_for(&current) {
        return Err(reason);
    }
    // SAFETY: a query of the keyboard layout of the foreground window's thread.
    let layout = unsafe { GetKeyboardLayout(current.thread) };
    send_paste(layout)
}

/// Waits for Shift, Alt and Win to come up: with one of them down, Ctrl+V
/// is another shortcut or nothing.
fn release_modifiers() -> Result<(), String> {
    let deadline = Instant::now() + MODIFIER_RELEASE;
    loop {
        let Some(key) = held_modifier() else {
            return Ok(());
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "the {key} key is still held, so Ctrl+V would not be a plain paste"
            ));
        }
        wait(CLIPBOARD_POLL);
    }
}

fn held_modifier() -> Option<&'static str> {
    key_down(VK_SHIFT)
        .then_some("Shift")
        .or_else(|| (key_down(VK_LCONTROL) || key_down(VK_RCONTROL)).then_some("Ctrl"))
        .or_else(|| key_down(VK_MENU).then_some("Alt"))
        .or_else(|| (key_down(VK_LWIN) || key_down(VK_RWIN)).then_some("Win"))
}

/// One Ctrl+V, sent as a single burst so that no keystroke of the user's
/// lands inside it. Windows takes nothing back: when only part of the burst
/// went in, what went down is released and the shortfall is reported.
fn send_paste(layout: HKL) -> Result<(), String> {
    let keys = [
        keystroke(VK_LCONTROL, false, layout),
        keystroke(VK_V, false, layout),
        keystroke(VK_V, true, layout),
        keystroke(VK_LCONTROL, true, layout),
    ];
    // SAFETY: `keys` holds the declared number of INPUT structures.
    let sent = unsafe {
        SendInput(
            keys.len() as u32,
            keys.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    if sent == 0 {
        return Err(failure("Windows refused the paste keystrokes"));
    }
    if sent < keys.len() as u32 {
        // Only release keys whose down event made it into the input queue.
        let release = if sent == 2 { &keys[2..] } else { &keys[3..] };
        unsafe {
            SendInput(
                release.len() as u32,
                release.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if sent == 1 {
            return Err("Windows accepted Ctrl but refused the paste key".to_owned());
        }
        // V went down: delivery is uncertain and must never be retried.
    }
    wait(PASTE_SETTLE);
    Ok(())
}

/// A key event for `SendInput`: the virtual key with its scan code under the
/// focused window's layout. Neither Ctrl nor V is an extended key, which is
/// the one flag a keystroke might need.
fn keystroke(key: u16, up: bool, layout: HKL) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: unsafe { MapVirtualKeyExW(u32::from(key), MAPVK_VK_TO_VSC, layout) } as u16,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Waits `duration`, answering the messages sent to windows of this thread
/// in the meantime, so that no other program waits on this thread.
fn wait(duration: Duration) {
    let deadline = Instant::now() + duration;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // SAFETY: no handles, and only this thread's own messages.
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&message);
            }
            if left.is_zero() {
                return;
            }
            if MsgWaitForMultipleObjectsEx(
                0,
                std::ptr::null(),
                left.as_millis() as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            ) == WAIT_FAILED
            {
                Sleep(left.as_millis() as u32);
                return;
            }
        }
    }
}

// Browser.

/// Opens a web address in the user's default browser. Nothing else is
/// opened: a path or another protocol would start a program.
pub(crate) fn open_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !["http://", "https://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return Err(format!("only web addresses are opened, not {url:?}"));
    }
    let address: Vec<u16> = url.encode_utf16().chain([0]).collect();
    // SAFETY: the reserved argument is null and the flags are the pair the
    // ShellExecuteW documentation names.
    let com = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    // SAFETY: "open" on a web address is handed to the program registered
    // for its protocol.
    let opened = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            w!("open"),
            address.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // S_OK and S_FALSE: this call entered COM and has to leave it again.
    if com == 0 || com == 1 {
        // SAFETY: balances the `CoInitializeEx` above.
        unsafe { CoUninitialize() };
    }
    let code = opened as usize as u32;
    if code > 32 {
        return Ok(());
    }
    let reason = match code {
        2 => "nothing answers this address",
        SE_ERR_ACCESSDENIED => "access is denied",
        SE_ERR_NOASSOC => "no browser is registered for web addresses",
        _ => "Windows did not start the browser",
    };
    Err(format!(
        "{reason} (ShellExecuteW error {code}); open {url} by hand"
    ))
}

/// Build capabilities for doctor. Actual inference devices come from the loaded engine.
pub(crate) fn inference_check() -> crate::doctor::Check {
    use crate::offline_asr::ComputeDevice;
    let providers = [
        ComputeDevice::Cpu,
        ComputeDevice::Cuda,
        ComputeDevice::Vulkan,
    ]
    .into_iter()
    .filter(|device| device.is_built_in())
    .map(|device| device.as_str())
    .collect::<Vec<_>>()
    .join(", ");
    crate::doctor::Check {
        name: "Windows inference engines".to_owned(),
        present: true,
        required: true,
        detail: format!(
            "{} build; compiled providers: {providers}; Sherpa-ONNX uses CPU",
            std::env::consts::ARCH
        ),
    }
}

#[cfg(test)]
#[path = "windows/contract.rs"]
mod contract;

#[cfg(test)]
#[path = "windows/interactive.rs"]
mod interactive;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_names_follow_the_levels_of_winnt() {
        assert_eq!(integrity_name(0), "untrusted");
        assert_eq!(integrity_name(LOW), "low");
        assert_eq!(integrity_name(MEDIUM), "medium");
        assert_eq!(integrity_name(MEDIUM + 0x10), "medium");
        assert_eq!(integrity_name(HIGH), "high (elevated)");
        assert_eq!(integrity_name(SYSTEM), "system");
    }
}
