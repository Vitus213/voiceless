//! `voiceless doctor`: verify runtime dependencies without requiring models.
//!
//! Checks are chosen from the default config's `[inject] prefer`: `pw-record`
//! and the data directory are always required; `wtype` is required unless
//! the Fcitx5 bridge is the injector; `wl-copy` is required only for the
//! clipboard method; with `prefer = "fcitx5"` the bridge's `Status` method
//! is called. Results are plain data (serializable for the console's
//! `/api/doctor`); `main` prints them and exits 1 if any required check
//! fails. `run_checks` blocks (PATH lookups, `mkdir -p`, a blocking D-Bus
//! call), so callers on a Tokio runtime use `spawn_blocking`.

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
pub fn run_checks() -> Vec<Check> {
    let prefer = crate::config::load_default_config()
        .map(|config| config.inject.prefer)
        .unwrap_or_default();
    let mut checks = vec![
        executable_check("pw-record", true),
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

/// The bridge is reachable when its `Status` call answers; a missing focused
/// input context (doctor runs from a terminal) still counts as present.
/// Uses the blocking zbus API, which must not run on a Tokio runtime thread
/// (it would panic in `block_on`); `main` runs it under `spawn_blocking`.
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
