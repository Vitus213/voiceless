use std::process::Command;

fn voiceless() -> Command {
    Command::new(env!("CARGO_BIN_EXE_voiceless"))
}

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
