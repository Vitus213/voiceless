//! Application updates preserve the manager that owns the active executable.
//!
//! Three channels can own a `voiceless` program, and only the one that owns
//! the running executable changes it: Nix (`/nix/store`, through its flake or
//! profile), a Bun global package (`bun add`) and the standalone release
//! tree (`install`, `update`, `update --rollback`). `voiceless update`
//! identifies the channel from the executable, so an update never writes to
//! another channel's files.
//!
//! User data belongs to no channel. Before every program switch the updater
//! saves a private snapshot of `config.toml`, `modes.json` and the history
//! database (`data`); `voiceless update --snapshot`, `--list` and `--restore`
//! manage snapshots by hand, whatever the channel. Models and recordings
//! stay where they are.
//!
//! A running daemon is never touched. The report names the installed
//! version, the version the channel offers and the version the daemon on the
//! session bus actually runs (`running`); its owner (systemd, the user)
//! restarts it once the dictation in progress has finished.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use semver::Version;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

mod bun;
mod data;
mod running;
mod standalone;

/// Nix keeps every program below this prefix; a `voiceless` there is its
/// flake's or profile's to change.
const NIX_STORE: &str = "/nix/store";

/// Takes the data snapshot that precedes a program switch; `Ok(None)` when
/// there is no data to save.
type BeforeSwitch<'a> = &'a dyn Fn(data::Request) -> Result<Option<data::Saved>, String>;

/// What `voiceless update` does; the fields are its flags. A plain
/// `voiceless update` leaves them all unset.
#[derive(Debug, Default)]
pub struct Update {
    /// Switch a standalone installation to its previous local version.
    pub rollback: bool,
    /// Only save a snapshot of the configuration and history.
    pub snapshot: bool,
    /// Only list the saved snapshots.
    pub list: bool,
    /// Only return the configuration and history to the snapshot with this id.
    pub restore: Option<String>,
}

/// The channel that owns the running executable.
#[derive(Debug, PartialEq, Eq)]
enum Channel<'a> {
    Nix,
    Debian,
    Standalone,
    Bun(&'a Path),
}

/// What an update found and changed, as `run` reports it.
#[derive(Debug)]
struct Outcome {
    /// The channel's own report: what it did, then the installed and target versions.
    lines: Vec<String>,
    /// Version the channel's command starts now.
    installed: Version,
    /// Version the command started before, when this update changed it.
    previous: Option<Version>,
    /// Where the channel's command leads, resolved; a process started from
    /// this file runs `installed`.
    command: Option<PathBuf>,
    /// Program trees that are never rewritten in place, so a file there
    /// still is what a running process started from.
    immutable: Vec<PathBuf>,
    /// The data snapshot saved before the program switched.
    snapshot: Option<data::Saved>,
}

/// Verify and install a standalone release archive for the current platform.
pub fn install(archive: &Path, sha256: &str) -> Result<(), String> {
    require_portable_target()?;
    let layout = data::Layout::from_environment()?;
    standalone::install(archive, sha256, &|request: data::Request| {
        data::snapshot(&layout, request)
    })
}

/// Run `voiceless update`: list, save or restore a data snapshot, or update
/// the channel that owns the running executable and report what runs now.
pub async fn run(update: Update) -> Result<(), String> {
    // The data commands need the user's directories; a program update only
    // does when it saves a snapshot.
    if update.list || update.snapshot || update.restore.is_some() {
        let layout = data::Layout::from_environment()?;
        if let Some(id) = update.restore.as_deref() {
            if let running::Runtime::Daemon(daemon) =
                running::observe(&layout.data_dir, &|_| None).await
            {
                return Err(format!(
                    "cannot restore while daemon pid {} is {}; stop it and let any recording finish first",
                    daemon.pid, daemon.state
                ));
            }
            for line in data::restore(&layout, id)? {
                println!("{line}");
            }
        } else if update.snapshot {
            match data::snapshot(&layout, data::Request::manual())? {
                Some(saved) => println!("{saved}"),
                None => println!(
                    "Nothing to snapshot: there is no config.toml, modes.json or history database."
                ),
            }
        } else {
            for line in data::list(&layout)? {
                println!("{line}");
            }
        }
        return Ok(());
    }

    let executable = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("cannot locate the running executable: {error}"))?;
    let launcher = std::env::var_os(bun::LAUNCHER_ENV);
    let before_switch =
        |request: data::Request| data::snapshot(&data::Layout::from_environment()?, request);
    let channel = classify(&executable, launcher.as_deref(), Path::new(NIX_STORE))?;
    let outcome = match channel {
        Channel::Nix | Channel::Debian => Outcome::managed(&executable, update.rollback),
        Channel::Standalone => standalone::update(&executable, update.rollback, &before_switch)?,
        Channel::Bun(launcher) => {
            bun::update(launcher, &executable, update.rollback, &before_switch)?
        }
    };
    for line in &outcome.lines {
        println!("{line}");
    }
    if let Some(saved) = &outcome.snapshot {
        println!("{saved}");
    }
    if update.rollback {
        if let Some(left) = &outcome.previous {
            let before = data::Layout::from_environment()
                .ok()
                .and_then(|layout| data::before_upgrade(&layout, &outcome.installed, left));
            if let Some(id) = before {
                // An older restored CLI can predate --restore. The executable that
                // performed this rollback remains in its pinned standalone payload.
                let updater = executable.display().to_string().replace('\'', "'\\''");
                println!(
                    "Voiceless {left} may have written data that {} cannot read; `'{updater}' update --restore {id}` returns to the snapshot taken before that upgrade.",
                    outcome.installed
                );
            }
        }
    }
    let data_dir = crate::models::default_data_dir();
    let active = running::observe(&data_dir, &|image| {
        outcome.running_version(image).or_else(|| match channel {
            Channel::Bun(_) => bun::cached_version(image),
            _ => None,
        })
    })
    .await;
    for line in active.report(&outcome.installed) {
        println!("{line}");
    }
    Ok(())
}

/// Which channel owns `executable`, a canonical path. A Bun launcher sets
/// `VOICELESS_BUN_LAUNCHER`; a machine with Bun installed on it does not make
/// an install Bun's.
fn classify<'a>(
    executable: &Path,
    launcher: Option<&'a OsStr>,
    nix_store: &Path,
) -> Result<Channel<'a>, String> {
    if executable.starts_with(nix_store) {
        return Ok(Channel::Nix);
    }
    if executable == Path::new("/usr/lib/voiceless-desktop/bin/voiceless") {
        return Ok(Channel::Debian);
    }
    require_portable_target()?;
    if standalone::owns(executable)? {
        return Ok(Channel::Standalone);
    }
    if let Some(launcher) = launcher {
        return Ok(Channel::Bun(Path::new(launcher)));
    }
    Err(format!(
        "cannot identify the installer that owns {}; run the Bun global command or use the installer for this copy",
        executable.display()
    ))
}

impl Outcome {
    fn managed(executable: &Path, rollback: bool) -> Self {
        let version = version();
        let action = if rollback { "roll back" } else { "update" };
        let (owner, manager, immutable) = if executable.starts_with(NIX_STORE) {
            (
                "Nix",
                "the owning flake or profile",
                vec![PathBuf::from(NIX_STORE)],
            )
        } else {
            (
                "dpkg",
                "the owning voiceless-desktop Debian package",
                Vec::new(),
            )
        };
        Self {
            lines: vec![
                format!("This Voiceless is managed by {owner}. Use {manager} to {action}."),
                format!("Installed: Voiceless {version}; target: set by {manager}."),
            ],
            installed: version,
            previous: None,
            command: Some(executable.to_path_buf()),
            immutable,
            snapshot: None,
        }
    }

    /// The version of the process started from `image`: `installed` when it
    /// is the command's own file, else what its file reports when that file
    /// lies in a tree that is never rewritten, else unknown.
    fn running_version(&self, image: &running::Image) -> Option<Version> {
        if image.replaced {
            return None;
        }
        if self.command.as_ref() == Some(&image.path) {
            return Some(self.installed.clone());
        }
        self.immutable
            .iter()
            .any(|root| image.path.starts_with(root))
            .then(|| executable_version(&image.path))
            .flatten()
    }
}

fn require_portable_target() -> Result<(), String> {
    if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )) {
        Ok(())
    } else {
        Err("portable releases require glibc Linux on x86_64 or aarch64".to_owned())
    }
}

fn version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version is SemVer")
}

fn target() -> String {
    format!("linux-{}-gnu", std::env::consts::ARCH)
}

/// Run `path --version` with none of the user's environment: a program being
/// installed or watched must not touch the user's configuration.
fn version_output(path: &Path) -> Result<Output, String> {
    let scratch =
        tempfile::tempdir().map_err(|error| format!("verification directory: {error}"))?;
    Command::new(path)
        .arg("--version")
        .current_dir(scratch.path())
        .env_clear()
        .env("XDG_CONFIG_HOME", scratch.path())
        .env("XDG_DATA_HOME", scratch.path())
        .env("XDG_CACHE_HOME", scratch.path())
        .output()
        .map_err(|error| format!("cannot run {}: {error}", path.display()))
}

fn verify_executable(path: &Path, expected: &Version) -> Result<(), String> {
    let result = version_output(path)?;
    let reported = String::from_utf8_lossy(&result.stdout);
    if result.status.success() && reported.trim_end() == format!("voiceless {expected}") {
        Ok(())
    } else {
        Err(format!(
            "{} did not report voiceless {expected} ({}; {}{})",
            path.display(),
            result.status,
            reported.trim_end(),
            String::from_utf8_lossy(&result.stderr).trim_end()
        ))
    }
}

/// The Voiceless version `path --version` reports, when it reports one.
fn executable_version(path: &Path) -> Option<Version> {
    let result = version_output(path).ok()?;
    if !result.status.success() {
        return None;
    }
    let reported = String::from_utf8_lossy(&result.stdout);
    Version::parse(reported.trim_end().strip_prefix("voiceless ")?).ok()
}

// Filesystem primitives shared by the channels and the snapshots. Every path
// is used as given: no symlink is followed where a link could lead elsewhere,
// and whatever this code creates is the user's own and private.

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn copy_and_hash(mut source: impl Read, mut output: impl Write) -> io::Result<(u64, String)> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut size = 0;
    loop {
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        output.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        size += count as u64;
    }
    Ok((size, format!("{:x}", hash.finalize())))
}

fn effective_uid() -> u32 {
    // geteuid has no arguments and does not mutate process state.
    unsafe { libc::geteuid() }
}

fn check_owner_and_mode(path: &Path, meta: &fs::Metadata, private: bool) -> Result<(), String> {
    let forbidden = if private { 0o7077 } else { 0o7022 };
    if meta.uid() != effective_uid() || meta.mode() & forbidden != 0 {
        return Err(format!(
            "unsafe owner or permissions for {}",
            path.display()
        ));
    }
    Ok(())
}

fn owned_directory(path: &Path, private: bool) -> Result<(), String> {
    let meta = at(path, fs::symlink_metadata(path))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a Voiceless directory", path.display()));
    }
    check_owner_and_mode(path, &meta, private)
}

/// Create `path` as a new file with `mode`; it must not exist.
fn create_file(path: &Path, mode: u32) -> Result<File, String> {
    at(
        path,
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path),
    )
}

fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Create the private leaf directory `path`; its parent must exist. An
/// existing one must already be the user's own and private.
fn create_private_directory(path: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_directory(parent(path)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => owned_directory(path, true),
        Err(error) => Err(format!("creating {}: {error}", path.display())),
    }
}

fn create_directories(path: &Path) -> Result<(), String> {
    if !path.is_dir() {
        create_directories(parent(path))?;
        match fs::DirBuilder::new().mode(0o755).create(path) {
            Ok(()) => sync_directory(parent(path))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(error) => return Err(format!("creating {}: {error}", path.display())),
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    at(path, File::open(path).and_then(|file| file.sync_all()))
}

/// The directory holding `path`; every path here has one.
fn parent(path: &Path) -> &Path {
    path.parent().expect("standalone path has a parent")
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8], label: &str) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("invalid {label}: {error}"))
}

/// Prefix the error with the path it concerns.
fn at<T>(path: &Path, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_comes_from_the_executable_not_from_the_tools_installed() {
        let store = Path::new("/nix/store");
        let nix = Path::new("/nix/store/abc-voiceless-0.1.3/bin/voiceless");
        assert_eq!(classify(nix, None, store), Ok(Channel::Nix));
        let desktop = Path::new("/usr/lib/voiceless-desktop/bin/voiceless");
        assert_eq!(classify(desktop, None, store), Ok(Channel::Debian));

        let unmanaged = Path::new("/home/user/Downloads/voiceless");
        let launcher = OsStr::new("/home/user/.bun/bin/voiceless");
        assert_eq!(
            classify(unmanaged, Some(launcher), store),
            Ok(Channel::Bun(Path::new("/home/user/.bun/bin/voiceless")))
        );

        let error = classify(unmanaged, None, store).unwrap_err();
        assert!(error.contains("cannot identify the installer that owns"));
    }
}
