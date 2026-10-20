//! `voiceless doctor`: verify runtime dependencies without requiring models.

use std::path::PathBuf;

#[derive(Debug, serde::Serialize)]
pub struct Check {
    pub name: String,
    pub present: bool,
    pub required: bool,
    pub detail: String,
}

impl Check {
    pub fn pass(&self) -> bool {
        self.present || !self.required
    }
}

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
fn fcitx5_bridge_check() -> Check {
    use crate::stream::{FCITX5_BRIDGE_INTERFACE, FCITX5_BRIDGE_PATH, FCITX5_BRIDGE_SERVICE};
    let status = zbus::blocking::Connection::session().and_then(|connection| {
        connection
            .call_method(
                Some(FCITX5_BRIDGE_SERVICE),
                FCITX5_BRIDGE_PATH,
                Some(FCITX5_BRIDGE_INTERFACE),
                "Status",
                &(),
            )?
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
