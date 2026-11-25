//! `voiceless update` through the built binary, with `HOME` and every
//! `XDG_*_HOME` root in a throwaway directory: the data snapshot commands
//! (`--snapshot`, `--list`, `--restore`) that serve any installation, the
//! refusal to update a program no channel owns, and a first standalone
//! install that has nothing to roll back to. The user's own configuration,
//! history, state directory and session bus are never touched, and no
//! network is used.

#![cfg(all(target_os = "linux", target_env = "gnu"))]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::{write::GzEncoder, Compression};
use sha2::{Digest, Sha256};
use voiceless::coordinator::HistoryEntry;
use voiceless::history::HistoryStore;

/// A user with nothing else: a scratch home whose XDG roots the programs run
/// with, and a `PATH` without another Voiceless on it.
struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .prefix("voiceless user's ")
                .tempdir()
                .unwrap(),
        )
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn config(&self) -> PathBuf {
        self.path().join("config/voiceless/config.toml")
    }

    fn data(&self) -> PathBuf {
        self.path().join("data/voiceless")
    }

    fn snapshots(&self) -> PathBuf {
        self.path().join("state/voiceless/update")
    }

    /// `program` with this home's roots.
    fn command(&self, program: &Path) -> Command {
        let root = self.path();
        let mut command = Command::new(program);
        command
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/voiceless-test-bus",
            )
            .env("PATH", root.join("bin"))
            .env_remove("VOICELESS_BUN_LAUNCHER");
        command
    }

    /// The binary under test.
    fn voiceless(&self) -> Command {
        self.command(Path::new(env!("CARGO_BIN_EXE_voiceless")))
    }
}

fn run(command: &mut Command) -> Output {
    command.output().expect("the program runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777
}

fn entry(text: &str) -> HistoryEntry {
    HistoryEntry {
        raw_text: text.to_owned(),
        processed_text: None,
        final_text: text.to_owned(),
        processing_mode: "quick".to_owned(),
        status: "completed".to_owned(),
        backend: "cloud-realtime".to_owned(),
        model: None,
        duration_seconds: Some(1.5),
        audio_path: None,
    }
}

/// One session as the daemon records it.
fn record(data: &Path, text: &str) {
    HistoryStore::open(data.join("history.sqlite3"))
        .expect("the history database opens")
        .insert(&entry(text), "cloud-realtime")
        .expect("the session is recorded");
}

/// The texts a history database holds, in alphabetical order.
fn texts(database: &Path) -> Vec<String> {
    let mut texts: Vec<String> = rusqlite::Connection::open(database)
        .expect("the database opens")
        .prepare("SELECT final_text FROM recognition_history")
        .expect("the table exists")
        .query_map((), |row| row.get(0))
        .expect("the rows are read")
        .collect::<rusqlite::Result<_>>()
        .expect("each history row contains text");
    texts.sort();
    texts
}

/// The id a `--snapshot` run reports.
fn saved_id(output: &Output) -> String {
    stdout(output)
        .trim_start_matches("Data snapshot ")
        .split_whitespace()
        .next()
        .expect("the report names the snapshot")
        .to_owned()
}

#[test]
fn snapshots_are_saved_listed_and_restored_whatever_the_installation() {
    let home = Home::new();

    let output = run(home.voiceless().args(["update", "--snapshot"]));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Nothing to snapshot"));
    assert!(!home.snapshots().exists());

    let config = home.config();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let before = "# before the upgrade\n[asr]\nstreaming_backend = \"cloud-realtime\"\n";
    fs::write(&config, before).unwrap();
    record(&home.data(), "before the upgrade");

    let output = run(home.voiceless().args(["update", "--snapshot"]));
    assert!(output.status.success(), "{}", stderr(&output));
    let id = saved_id(&output);
    let report = stdout(&output);
    let snapshot = home.snapshots().join(&id);
    assert_eq!(mode(&home.snapshots()), 0o700);
    assert_eq!(mode(&snapshot), 0o700);
    for name in ["config.toml", "history.sqlite3", "snapshot.json"] {
        assert_eq!(mode(&snapshot.join(name)), 0o600, "{name}");
    }
    assert!(
        report.contains(&format!("`voiceless update --restore {id}`")),
        "{report}"
    );
    assert_eq!(
        fs::read_to_string(snapshot.join("config.toml")).unwrap(),
        before
    );
    assert_eq!(
        texts(&snapshot.join("history.sqlite3")),
        ["before the upgrade"]
    );

    // The newer program changes the data before the user decides to go back.
    fs::write(&config, "# after the upgrade\n").unwrap();
    record(&home.data(), "after the upgrade");
    let listing = run(home.voiceless().args(["update", "--list"]));
    assert!(listing.status.success());
    let listing = stdout(&listing);
    assert!(
        listing.contains(&id) && listing.contains("manual"),
        "{listing}"
    );

    let output = run(home.voiceless().args(["update", "--restore", &id]));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("What it replaced is kept in"));
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    assert_eq!(
        texts(&home.data().join("history.sqlite3")),
        ["before the upgrade"]
    );

    // Nothing is lost: the data the restore replaced is kept with the snapshot.
    let displaced = fs::read_dir(&snapshot)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("displaced-"))
        })
        .expect("the restore keeps what it replaced");
    assert_eq!(
        fs::read_to_string(displaced.join("config.toml")).unwrap(),
        "# after the upgrade\n"
    );
    assert_eq!(
        texts(&displaced.join("history.sqlite3")),
        ["after the upgrade", "before the upgrade"]
    );

    let output = run(home
        .voiceless()
        .args(["update", "--restore", "no-such-snapshot"]));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no snapshot named no-such-snapshot"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_restore_waits_for_the_daemon_to_leave() {
    let home = Home::new();
    let config = home.config();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "config").unwrap();
    let output = run(home.voiceless().args(["update", "--snapshot"]));
    assert!(output.status.success(), "{}", stderr(&output));
    let id = saved_id(&output);
    fs::write(&config, "changed").unwrap();

    // A daemon, or a standalone stream, holds the data directory.
    let daemon = voiceless::runtime::lock_runtime(&home.data()).unwrap();
    let output = run(home.voiceless().args(["update", "--restore", &id]));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("stop it and let any recording finish first"),
        "{}",
        stderr(&output)
    );
    drop(daemon);
    assert_eq!(fs::read_to_string(&config).unwrap(), "changed");

    let capture = fs::File::create(home.data().join("capture.lock")).unwrap();
    capture.lock().unwrap();
    let output = run(home.voiceless().args(["update", "--restore", &id]));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("a recording is active"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), "changed");
    drop(capture);

    let output = run(home.voiceless().args(["update", "--restore", &id]));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(&config).unwrap(), "config");
}

#[test]
fn a_program_no_channel_owns_is_not_updated() {
    let home = Home::new();
    // A test binary from the Nix store is the Nix channel's, and says so.
    let nix = std::fs::canonicalize(env!("CARGO_BIN_EXE_voiceless"))
        .unwrap()
        .starts_with("/nix/store");
    for arguments in [&["update"][..], &["update", "--rollback"][..]] {
        let output = run(home.voiceless().args(arguments));
        if nix {
            assert_eq!(output.status.code(), Some(0), "{arguments:?}");
            assert!(stdout(&output).contains("This Voiceless is managed by Nix"));
        } else {
            assert_eq!(output.status.code(), Some(1), "{arguments:?}");
            assert!(
                stderr(&output).contains("cannot identify the installer that owns"),
                "{}",
                stderr(&output)
            );
        }
    }
}

#[test]
fn the_snapshot_commands_exclude_each_other() {
    let home = Home::new();
    let output = run(home.voiceless().args(["update", "--snapshot", "--list"]));
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("cannot be used with"),
        "{}",
        stderr(&output)
    );
    let output = run(home
        .voiceless()
        .args(["update", "--rollback", "--restore", "x"]));
    assert_eq!(output.status.code(), Some(2));
    let output = run(home.voiceless().args(["update", "--help"]));
    assert!(output.status.success());
    for flag in ["--rollback", "--snapshot", "--list", "--restore <ID>"] {
        assert!(stdout(&output).contains(flag), "{flag}");
    }
}

fn file_digest(path: &Path) -> String {
    let mut hash = Sha256::new();
    std::io::copy(&mut fs::File::open(path).unwrap(), &mut hash).unwrap();
    format!("{:x}", hash.finalize())
}

/// Package Cargo's real binary and the non-glibc libraries resolved by its
/// loader, with the same relative runpaths as the native release payload.
fn archive(work: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_BIN_EXE_voiceless"));
    let trace = run(Command::new(source).env("LD_TRACE_LOADED_OBJECTS", "1"));
    assert!(trace.status.success(), "{}", stderr(&trace));
    let binary = work.join("bin/voiceless");
    fs::create_dir(binary.parent().unwrap()).unwrap();
    fs::create_dir(work.join("lib")).unwrap();
    let stripped = run(Command::new("strip")
        .args(["--strip-debug", "-o"])
        .arg(&binary)
        .arg(source));
    assert!(stripped.status.success(), "{}", stderr(&stripped));
    let mut paths = vec![("bin/voiceless".to_owned(), binary)];
    for line in stdout(&trace).lines() {
        let Some((name, resolved)) = line.trim().split_once(" => ") else {
            continue;
        };
        if name.starts_with('/')
            || matches!(
                name,
                "libc.so.6"
                    | "libm.so.6"
                    | "libdl.so.2"
                    | "libpthread.so.0"
                    | "librt.so.1"
                    | "libutil.so.1"
                    | "linux-vdso.so.1"
            )
        {
            continue;
        }
        let (source, _) = resolved
            .rsplit_once(" (")
            .expect("resolved library address");
        assert_eq!(Path::new(name).file_name().unwrap(), name);
        let relative = format!("lib/{name}");
        let library = work.join(&relative);
        fs::copy(source, &library).unwrap();
        fs::set_permissions(&library, fs::Permissions::from_mode(0o755)).unwrap();
        paths.push((relative, library));
    }
    for (name, path) in &paths {
        let rpath = if name.starts_with("bin/") {
            "$ORIGIN/../lib"
        } else {
            "$ORIGIN"
        };
        let patched = run(Command::new("patchelf")
            .args(["--set-rpath", rpath])
            .arg(path));
        assert!(
            patched.status.success(),
            "{}: {}",
            path.display(),
            stderr(&patched)
        );
    }
    for (name, contents) in [
        ("LICENSE", "MIT (local test fixture)"),
        ("THIRD_PARTY_NOTICES.txt", "Local integration-test fixture"),
    ] {
        let path = work.join(name);
        fs::write(&path, contents).unwrap();
        paths.push((name.to_owned(), path));
    }
    let files: BTreeMap<_, _> = paths
        .iter()
        .map(|(name, path)| (name, file_digest(path)))
        .collect();
    let target = format!("linux-{}-gnu", std::env::consts::ARCH);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "target": target,
        "variant": "cpu",
        "files": files,
    }))
    .unwrap();
    let manifest_path = work.join("release.json");
    fs::write(&manifest_path, manifest).unwrap();
    paths.push(("release.json".to_owned(), manifest_path));
    let archive = work.join("voiceless.tar.gz");
    let encoder = GzEncoder::new(fs::File::create(&archive).unwrap(), Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    for (name, path) in paths {
        tar.append_path_with_name(path, name).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    archive
}

#[test]
fn a_first_standalone_install_has_nothing_to_roll_back_to() {
    let home = Home::new();
    let staging = tempfile::tempdir().unwrap();
    let path = archive(staging.path());

    let output = run(home
        .voiceless()
        .arg("install")
        .arg(path.as_os_str())
        .arg("--sha256")
        .arg(file_digest(&path)));
    assert!(output.status.success(), "{}", stderr(&output));
    let command = home.path().join(".local/bin/voiceless");
    assert!(fs::symlink_metadata(&command).unwrap().is_symlink());
    let active = command.canonicalize().unwrap();
    let rejected = run(home
        .command(&command)
        .arg("install")
        .arg(&path)
        .arg("--sha256")
        .arg("0".repeat(64)));
    assert_eq!(rejected.status.code(), Some(1));
    assert!(
        stderr(&rejected).contains("archive SHA-256 mismatch"),
        "{}",
        stderr(&rejected)
    );
    assert_eq!(command.canonicalize().unwrap(), active);
    let version = run(home
        .command(&command)
        .env_remove("LD_LIBRARY_PATH")
        .arg("--version"));
    assert!(version.status.success(), "{}", stderr(&version));
    assert_eq!(
        stdout(&version).trim(),
        format!("voiceless {}", env!("CARGO_PKG_VERSION"))
    );

    // Nothing was installed before, so there is nothing to go back to.
    let output = run(home.command(&command).args(["update", "--rollback"]));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no previous standalone release"),
        "{}",
        stderr(&output)
    );
    assert!(!home.snapshots().exists());
}
