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
    let clipboard_preferred = crate::config::load_default_config()
        .map(|config| config.inject.prefer == "clipboard")
        .unwrap_or(false);
    vec![
        executable_check("pw-record", true),
        executable_check("wtype", true),
        // wl-copy powers the clipboard injection method and the wtype
        // fallback; it is required when clipboard is the preferred method.
        executable_check("wl-copy", clipboard_preferred),
        directory_check(),
    ]
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
