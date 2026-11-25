//! `voiceless doctor`: verify runtime dependencies without requiring models.
//!
//! On Linux the capture check probes the recorder and reports the selected
//! PipeWire or GStreamer path. Injection checks follow `[inject] prefer`.
//! With `prefer = "fcitx5"` the bridge's `Status` method
//! is called. Results are plain data (serializable for the console's
//! `/api/doctor`); `main` prints them and exits 1 if any required check
//! fails. `run_checks` blocks (PATH lookups, `mkdir -p`, a blocking D-Bus
//! call), so callers on a Tokio runtime use `spawn_blocking`.

#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// One dependency check result.
#[derive(Debug, serde::Serialize)]
pub struct Check {
    /// Executable name, `data directory` or `fcitx5 bridge`.
    pub name: String,
    /// The dependency was found / answered.
    pub present: bool,
    /// Whether absence fails `doctor` (depends on the configured injector).
    pub required: bool,
    /// Resolved path, directory, bridge status text or the error.
    pub detail: String,
}

impl Check {
    /// A check passes when present, or when it was optional anyway.
    pub fn pass(&self) -> bool {
        self.present || !self.required
    }
}

/// Run every applicable check and return them in display order. Blocking.
/// Side effect: creates the data directory if it is missing. An unreadable
/// or invalid config is treated as the defaults here (so `doctor` still
/// runs), which means the checks reflect `prefer = "wtype"` in that case.
#[cfg(target_os = "linux")]
pub fn run_checks() -> Vec<Check> {
    let prefer = crate::config::load_default_config()
        .map(|config| config.inject.prefer)
        .unwrap_or_default();
    let recorder = crate::capture::recorder::RecorderProfile::on_path()
        .map(|profile| profile.describe())
        .map_err(|error| error.to_string());
    let mut checks = vec![
        dependency_check("pw-record", recorder, true),
        executable_check("wtype", prefer != "fcitx5"),
        // wl-copy powers the clipboard injection method and the fallback; it
        // is required when clipboard is the preferred method.
        executable_check("wl-copy", prefer == "clipboard"),
        directory_check(),
    ];
    if prefer == "fcitx5" {
        checks.push(fcitx5_bridge_check());
    }
    checks
}

/// Check the native audio and input permissions without opening the mic.
#[cfg(not(target_os = "linux"))]
pub fn run_checks() -> Vec<Check> {
    let microphone = crate::desktop::audio::query_devices().and_then(|devices| {
        if devices.microphones.is_empty() {
            Err(
                "no microphones detected; connect a device or check system microphone access"
                    .to_owned(),
            )
        } else {
            Ok(format!(
                "{} microphone(s) available",
                devices.microphones.len()
            ))
        }
    });
    let keyboard = crate::desktop::native::input_permission();
    vec![
        dependency_check("microphone", microphone, true),
        dependency_check(
            "global input permission",
            keyboard.map(|_| "available".to_owned()),
            false,
        ),
        #[cfg(target_os = "windows")]
        crate::desktop::native::inference_check(),
        directory_check(),
    ]
}

fn dependency_check(name: &str, result: Result<String, String>, required: bool) -> Check {
    Check {
        name: name.to_owned(),
        present: result.is_ok(),
        required,
        detail: result.unwrap_or_else(|error| error),
    }
}

/// The bridge is reachable when its `Status` call answers; a missing focused
/// input context (doctor runs from a terminal) still counts as present.
/// Uses the blocking zbus API, which must not run on a Tokio runtime thread
/// (it would panic in `block_on`); `main` runs it under `spawn_blocking`.
#[cfg(target_os = "linux")]
fn fcitx5_bridge_check() -> Check {
    use crate::fcitx5::{INTERFACE, PATH, SERVICE};
    let status = zbus::blocking::Connection::session().and_then(|connection| {
        connection
            .call_method(Some(SERVICE), PATH, Some(INTERFACE), "Status", &())?
            .body()
            .deserialize::<(bool, String)>()
    });
    let (present, detail) = match status {
        Ok((_, detail)) => (true, detail),
        Err(error) => (
            false,
            format!("{error}; add voiceless-fcitx5-bridge to the Fcitx5 addons"),
        ),
    };
    Check {
        name: "fcitx5 bridge".to_owned(),
        present,
        required: true,
        detail,
    }
}

/// Look `name` up in `PATH`; the detail is the resolved path when found.
#[cfg(target_os = "linux")]
fn executable_check(name: &str, required: bool) -> Check {
    match find_in_path(name) {
        Some(path) => Check {
            name: name.to_owned(),
            present: true,
            required,
            detail: path.display().to_string(),
        },
        None => Check {
            name: name.to_owned(),
            present: false,
            required,
            detail: "not found in PATH".to_owned(),
        },
    }
}

/// Create (if needed) and report the XDG data directory that holds models,
/// recordings and the history database.
fn directory_check() -> Check {
    let data_dir = crate::models::default_data_dir();
    match std::fs::create_dir_all(&data_dir) {
        Ok(()) => Check {
            name: "data directory".to_owned(),
            present: true,
            required: true,
            detail: data_dir.display().to_string(),
        },
        Err(error) => Check {
            name: "data directory".to_owned(),
            present: false,
            required: true,
            detail: error.to_string(),
        },
    }
}

/// First regular file named `name` in `PATH` (no executable-bit check).
#[cfg(target_os = "linux")]
fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
