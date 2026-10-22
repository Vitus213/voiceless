//! Identity cutover: the repository ships Voiceless as the sole product
//! identity; no `type4me-linux` command, package attribute, Home Manager
//! option or desktop entry survives as a compatibility path.
//!
//! Three tests grep repository files (`flake.nix`, `nix/home-manager.nix`,
//! legacy paths) and skip themselves when those files are absent, as in the
//! Nix build sandbox; the last one runs the built binary's `--help`.

use std::fs;
use std::path::Path;

/// Parent of the `rust/` crate directory.
fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root above rust/")
        .to_path_buf()
}

/// Read a file or panic with its path.
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Repo-root files are absent inside the Nix build sandbox (only `rust/`
/// is shipped). Repo-level identity checks only run in a full checkout.
fn repo_file(name: &str) -> Option<std::path::PathBuf> {
    let path = repo_root().join(name);
    path.exists().then_some(path)
}

#[test]
fn flake_default_package_is_voiceless_and_drops_python_identity() {
    let Some(flake_path) = repo_file("flake.nix") else {
        return;
    };
    let flake = read(&flake_path);
    assert!(
        flake.contains("packages.default = voiceless;"),
        "default package must be voiceless"
    );
    assert!(
        !flake.contains("packages.type4me-linux"),
        "no type4me-linux package attribute may remain"
    );
    assert!(
        !flake.contains("buildPythonApplication"),
        "the Python application package must be removed"
    );
    assert!(
        !flake.contains("io.github.vitus.Type4Me"),
        "the old desktop identity must be gone"
    );
}

#[test]
fn home_manager_module_exposes_only_voiceless_options() {
    let Some(module_path) = repo_file("nix/home-manager.nix") else {
        return;
    };
    let module = read(&module_path);
    assert!(module.contains("options.programs.voiceless"));
    assert!(
        !module.contains("programs.type4me-linux"),
        "old Home Manager options must not remain"
    );
    assert!(
        !module.contains("type4me-linux toggle"),
        "old CLI commands must not be referenced"
    );
}

/// Absence checks need no file read, so this runs in the sandbox too.
#[test]
fn no_desktop_entry_or_python_distribution_survives() {
    let root = repo_root();
    assert!(!root.join("data/io.github.vitus.Type4Me.desktop").exists());
    assert!(!root.join("src/type4me_linux").exists());
    assert!(!root.join("pyproject.toml").exists());
}

#[test]
fn rust_binary_reports_voiceless_identity() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_voiceless"))
        .arg("--help")
        .output()
        .expect("voiceless binary should be runnable");
    let help = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(help.contains("voiceless"));
    assert!(
        !help.contains("type4me"),
        "help must not mention the old identity"
    );
}
