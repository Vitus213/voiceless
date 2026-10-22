//! Smoke tests of the built `voiceless` binary's surface: `--help` lists the
//! core subcommands, `doctor` runs with nothing installed (no Python, no
//! models) under a throwaway XDG root, and `daemon --help` exposes `--mode`.
//! The binary is spawned as a child process; `--help` runs inherit the real
//! environment because they touch no user data.

use std::process::Command;

/// The binary under test, as built by Cargo for this test run.
fn voiceless() -> Command {
    Command::new(env!("CARGO_BIN_EXE_voiceless"))
}

/// Every top-level command of the README table (except ctl/benchmark)
/// appears in `--help`.
#[test]
fn help_lists_the_core_cli_commands() {
    let output = voiceless()
        .arg("--help")
        .output()
        .expect("voiceless binary should be runnable");

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("help should be UTF-8");
    for command in [
        "stream",
        "transcribe",
        "record",
        "model",
        "doctor",
        "mode",
        "history",
        "daemon",
    ] {
        assert!(
            help.contains(command),
            "missing command {command} in {help}"
        );
    }
}

/// `doctor` exits 0 and prints its banner with every XDG root pointed at an
/// empty temp dir (it creates the data directory itself).
#[test]
fn doctor_runs_without_python_or_model_files() {
    let root = tempfile::tempdir().expect("temporary XDG root");
    let output = voiceless()
        .arg("doctor")
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .output()
        .expect("voiceless binary should be runnable");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("doctor output should be UTF-8");
    assert!(stdout.contains("Voiceless"));
}

#[test]
fn daemon_accepts_a_mode_argument() {
    let output = voiceless()
        .args(["daemon", "--help"])
        .output()
        .expect("voiceless binary should be runnable");

    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("daemon help should be UTF-8");
    assert!(
        help.contains("--mode"),
        "daemon help should list --mode, got {help}"
    );
}
