//! Native calls execute in the daemon process so TCC checks the same identity
//! that captures input, observes shortcuts and delivers the transcript.
#[path = "macos/keys.rs"]
mod keys;
pub(crate) use keys::{key_code, start_keys, KeyListener};

use super::focus::FocusedWindow;
use std::ffi::{c_char, CStr};
use std::process::Command;

extern "C" {
    fn vl_mic_permission(request: bool) -> isize;
    fn vl_listen_permission(request: bool) -> bool;
    fn vl_accessibility(request: bool) -> bool;
    fn vl_focus(app: *mut *mut c_char, title: *mut *mut c_char) -> u64;
    fn vl_paste(text: *const u8, length: usize, key: u64) -> i32;
    fn vl_copy(text: *const u8, length: usize) -> bool;
}

pub(crate) fn microphone_permission() -> Result<(), String> {
    microphone(false)
}
pub(crate) fn request_microphone_permission() -> Result<(), String> {
    microphone(true)
}

fn microphone(request: bool) -> Result<(), String> {
    match unsafe { vl_mic_permission(request) } {
        -2 => Err("Unlock the Mac to answer the microphone permission request, then start recording again".into()),
        -3 => Err("The microphone permission request was not answered within 30 seconds".into()),
        3 => Ok(()),
        0 => Err("Microphone access has not been requested; start a recording to allow access".into()),
        1 => Err("Microphone access is restricted by system policy".into()),
        _ => Err("Allow microphone access in System Settings > Privacy & Security > Microphone, then reopen Voiceless".into()),
    }
}

pub(crate) fn input_permission() -> Result<(), String> {
    input(false)
}
pub(crate) fn request_input_permission() -> Result<(), String> {
    input(true)
}
fn input(request: bool) -> Result<(), String> {
    let mut missing = Vec::new();
    if !unsafe { vl_listen_permission(request) } {
        missing.push("Input Monitoring (global shortcuts)");
    }
    if !unsafe { vl_accessibility(request) } {
        missing.push("Accessibility (focus and paste)");
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Allow {} in System Settings > Privacy & Security, then reopen Voiceless",
        missing.join(" and ")
    ))
}

fn take_string(value: *mut c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // The bridge returns strdup-owned UTF-8 strings or NULL.
    unsafe {
        let text = CStr::from_ptr(value).to_string_lossy().into_owned();
        libc::free(value.cast());
        Some(text)
    }
}

pub(crate) fn input_label(uid: &str) -> Option<String> {
    audio_label(uid, false)
}

pub(crate) fn audio_label(uid: &str, output: bool) -> Option<String> {
    extern "C" {
        fn vl_audio_label(uid: *const c_char, output: bool) -> *mut c_char;
    }
    let uid = std::ffi::CString::new(uid).ok()?;
    take_string(unsafe { vl_audio_label(uid.as_ptr(), output) })
}

/// Core Audio may wait for system audio consent while creating the tap or
/// starting its aggregate input stream. Only one worker may wait on that dialog.
static ASKING: parking_lot::Mutex<bool> = parking_lot::Mutex::new(false);
static ANSWERED: parking_lot::Condvar = parking_lot::Condvar::new();

/// A dialog nobody answered in time.
pub(crate) fn unanswered() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "The system audio permission request was not answered within 30 seconds",
    )
}

pub(crate) struct SystemAudioStart {
    pub(crate) deadline: std::time::Instant,
}

impl SystemAudioStart {
    pub(crate) fn acquire() -> std::io::Result<Self> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut asking = ASKING.lock();
        if ANSWERED
            .wait_while_until(&mut asking, |busy| *busy, deadline)
            .timed_out()
        {
            return Err(unanswered());
        }
        *asking = true;
        Ok(Self { deadline })
    }
}

impl Drop for SystemAudioStart {
    fn drop(&mut self) {
        *ASKING.lock() = false;
        ANSWERED.notify_all();
    }
}

/// A process tap on `output` with the aggregate device that plays it to an
/// input stream.
pub(crate) struct MeetingTap {
    tap: u32,
    aggregate: u32,
}

impl MeetingTap {
    /// Create the tap and private aggregate for the selected output. Its
    /// stream must also start before the meeting can record either source.
    pub fn open(output: &cpal::DeviceId) -> std::io::Result<Self> {
        extern "C" {
            fn vl_meeting_tap(
                output: *const c_char,
                uid: *const c_char,
                tap: *mut u32,
                aggregate: *mut u32,
            ) -> i32;
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let uid = format!(
            "dev.voiceless.meeting.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let output = std::ffi::CString::new(output.1.as_str())?;
        let name = std::ffi::CString::new(uid.as_str())?;
        let (mut tap, mut aggregate) = (0, 0);
        let status =
            unsafe { vl_meeting_tap(output.as_ptr(), name.as_ptr(), &mut tap, &mut aggregate) };
        if status != 0 {
            let kind = match status {
                // Core Audio's device permissions error and the system-audio TCC refusal.
                code if [*b"!hog", *b"perm"].contains(&code.to_be_bytes()) => {
                    std::io::ErrorKind::PermissionDenied
                }
                _ => std::io::ErrorKind::Other,
            };
            return Err(std::io::Error::new(
                kind,
                format!("Core Audio system tap setup failed (OSStatus {status:#x})"),
            ));
        }
        Ok(Self { tap, aggregate })
    }

    pub fn device(&self) -> cpal::Device {
        type CoreDevice = <cpal::platform::CoreAudioHost as cpal::traits::HostTrait>::Device;
        CoreDevice::new(self.aggregate).into()
    }
}

impl Drop for MeetingTap {
    fn drop(&mut self) {
        extern "C" {
            fn vl_close_meeting_tap(tap: u32, aggregate: u32);
        }
        unsafe { vl_close_meeting_tap(self.tap, self.aggregate) };
    }
}

pub(crate) fn focused_window() -> Option<FocusedWindow> {
    let mut app = std::ptr::null_mut();
    let mut title = std::ptr::null_mut();
    let key = unsafe { vl_focus(&mut app, &mut title) };
    let app_id = take_string(app);
    let title = take_string(title);
    (key != 0).then(|| FocusedWindow {
        source: "macos",
        key: key.to_string(),
        app_id,
        title,
    })
}

pub(crate) fn copy_text(text: &str) -> Result<(), String> {
    unsafe { vl_copy(text.as_ptr(), text.len()) }
        .then_some(())
        .ok_or_else(|| "macOS pasteboard write failed".into())
}

pub(crate) fn paste_text(text: &str, expected_key: &str) -> Result<(), String> {
    let key = expected_key
        .parse::<u64>()
        .ok()
        .filter(|key| *key != 0)
        .ok_or("missing original focus")?;
    match unsafe { vl_paste(text.as_ptr(), text.len(), key) } {
        0 => Ok(()),
        1 => {
            Err("The original input no longer has focus, or the focused input is protected".into())
        }
        2 => Err(
            "Allow Accessibility in System Settings > Privacy & Security, then reopen Voiceless"
                .into(),
        ),
        _ => Err("macOS could not prepare the pasteboard or the paste keystroke".into()),
    }
}

pub(crate) fn open_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("expected a web URL".into());
    }
    Command::new("/usr/bin/open")
        .arg(url)
        .status()
        .map_err(|e| e.to_string())?
        .success()
        .then_some(())
        .ok_or_else(|| "macOS could not open the browser".into())
}
