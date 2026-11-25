//! What the desktop window does when it is closed or asked to quit, and
//! who starts it at login. Pure decisions: the desktop host (the Tauri
//! window and tray, `desktop/`) applies them.
//!
//! Closing the window never stops the runtime. With a tray the window hides
//! and the tray, or the launcher, brings it back. Without one there is
//! nowhere to hide, so the host leaves and the launcher starts it again;
//! the runtime keeps serving the hotkey and `voiceless web` meanwhile.
//!
//! Quitting with a session in progress asks first. The session belongs to
//! the daemon and finishes whatever the host does; the question only makes
//! that known before the tray goes away.

use std::path::{Path, PathBuf};

/// What closing the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Close {
    /// Hide the window; the tray and the launcher reopen it.
    Hide,
    /// Leave the host; the runtime keeps running and the launcher starts a new host.
    Exit,
}

/// A tray on the desktop means the window has somewhere to go when it is
/// closed; otherwise leaving is the only honest close.
pub fn on_close(tray_available: bool) -> Close {
    if tray_available {
        Close::Hide
    } else {
        Close::Exit
    }
}

/// What asking the host to quit does, given the daemon's last state
/// (`idle`, `recording` or `stopping`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quit {
    /// Nothing is in progress: leave at once.
    Now,
    /// A session is in progress: say that it goes on in the daemon, and
    /// leave when the user still says so.
    Announce(String),
}

/// The daemon's state decides whether quitting needs a word first.
pub fn on_quit(daemon_state: &str) -> Quit {
    if daemon_state == "idle" {
        Quit::Now
    } else {
        Quit::Announce(daemon_state.to_owned())
    }
}

/// Who starts the desktop host at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autostart {
    /// Nobody.
    Off,
    /// The host's own entry in the user's autostart directory: its menu
    /// can switch it off.
    App,
    /// An entry outside that directory, the system's or a package's: the
    /// menu reports it and leaves it alone.
    System,
}

/// The autostart files under `directories` whose `Exec` line runs
/// `executable` (its first word, quotes removed), in directory order. A
/// file that cannot be read is skipped: the list is a report, never a
/// failure.
pub fn autostart_entries<'a>(
    directories: impl IntoIterator<Item = &'a Path>,
    executable: &str,
) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    for directory in directories {
        let Ok(files) = std::fs::read_dir(directory) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if !path.to_string_lossy().ends_with(".desktop") {
                continue;
            }
            let runs_executable = std::fs::read_to_string(&path).is_ok_and(|content| {
                content
                    .lines()
                    .filter_map(|line| line.strip_prefix("Exec="))
                    .any(|command| {
                        command
                            .split_whitespace()
                            .next()
                            .is_some_and(|program| program.trim_matches('"') == executable)
                    })
            });
            if runs_executable {
                entries.push(path);
            }
        }
    }
    entries
}

/// Whether the user's autostart entry exists; it is written by the host
/// itself and lives in `$XDG_CONFIG_HOME/autostart` (`~/.config/autostart`).
pub fn user_autostart_entry(config_home: Option<&Path>) -> PathBuf {
    let base = config_home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("autostart").join("voiceless-desktop.desktop")
}

/// The autostart directories the user does not own: every `autostart` under
/// `XDG_CONFIG_DIRS`, then `/etc/xdg/autostart` unless it is already among them.
pub fn system_autostart_directories() -> Vec<PathBuf> {
    let defaults = [PathBuf::from("/etc/xdg")];
    let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_else(|| defaults.to_vec());
    let mut directories: Vec<PathBuf> = Vec::new();
    for dir in config_dirs {
        let dir = dir.join("autostart");
        // A directory listed twice is looked at once.
        if !directories.contains(&dir) {
            directories.push(dir);
        }
    }
    let system = PathBuf::from("/etc/xdg/autostart");
    if !directories.contains(&system) {
        directories.push(system);
    }
    directories
}

/// Who starts `executable` (the host's file name) at login.
pub fn autostart_state(user_entry: bool, system_entries: &[PathBuf]) -> Autostart {
    match (system_entries.is_empty(), user_entry) {
        (false, _) => Autostart::System,
        (true, true) => Autostart::App,
        (true, false) => Autostart::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_hides_with_a_tray_and_leaves_without_one() {
        assert_eq!(on_close(true), Close::Hide);
        assert_eq!(on_close(false), Close::Exit);
    }

    #[test]
    fn quitting_asks_only_while_a_session_runs() {
        assert_eq!(on_quit("idle"), Quit::Now);
        assert_eq!(on_quit("recording"), Quit::Announce("recording".to_owned()));
        assert_eq!(on_quit("stopping"), Quit::Announce("stopping".to_owned()));
    }

    #[test]
    fn login_start_belongs_to_the_system_the_user_or_nobody() {
        let system = vec![PathBuf::from("/etc/xdg/autostart/voiceless.desktop")];
        assert_eq!(autostart_state(false, &[]), Autostart::Off);
        assert_eq!(autostart_state(true, &[]), Autostart::App);
        assert_eq!(autostart_state(false, &system), Autostart::System);
        assert_eq!(autostart_state(true, &system), Autostart::System);
    }

    #[test]
    fn autostart_entries_are_those_that_start_the_host_and_only_those() {
        let root = tempfile::tempdir().unwrap();
        let write = |name: &str, content: &str| {
            let path = root.path().join(name);
            std::fs::write(&path, content).unwrap();
        };
        write(
            "voiceless-desktop.desktop",
            "[Desktop Entry]\nType=Application\nExec=voiceless-desktop --autostart\n",
        );
        write(
            "other.desktop",
            "[Desktop Entry]\nExec=voiceless-desktop-other\n",
        );
        write("notes.txt", "Exec=voiceless-desktop\n");
        let missing = root.path().join("missing");
        assert_eq!(
            autostart_entries([missing.as_path(), root.path()], "voiceless-desktop"),
            vec![root.path().join("voiceless-desktop.desktop")]
        );
        // The host's name is matched whole: `voiceless` is the runtime, not the host.
        assert_eq!(
            autostart_entries([root.path()], "voiceless"),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn the_users_entry_follows_the_config_home_and_the_system_directories_come_once() {
        assert_eq!(
            user_autostart_entry(Some(Path::new("/tmp/config"))),
            PathBuf::from("/tmp/config/autostart/voiceless-desktop.desktop")
        );
        let directories = system_autostart_directories();
        let system = PathBuf::from("/etc/xdg/autostart");
        assert!(directories.contains(&system));
        assert_eq!(
            directories
                .iter()
                .filter(|directory| *directory == &system)
                .count(),
            1
        );
        assert!(directories
            .iter()
            .all(|directory| directory.ends_with("autostart")));
        let mut unique = directories.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), directories.len(), "{directories:?}");
    }
}
