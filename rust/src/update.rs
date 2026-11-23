//! Application updates preserve the manager that owns the active executable.

use std::path::Path;
use std::process::Command;

use semver::Version;

mod bun;
mod standalone;

/// Verify and install a standalone release archive for the current platform.
pub fn install(archive: &Path, sha256: &str) -> Result<(), String> {
    require_portable_target()?;
    standalone::install(archive, sha256)
}

/// Update the current installation, or activate a previous standalone release.
pub fn update(rollback: bool) -> Result<(), String> {
    let executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("cannot locate the running executable: {error}"))?;
    if executable.starts_with("/nix/store") {
        println!(
            "This Voiceless is managed by Nix. Use the owning flake or profile to {}.",
            if rollback { "roll back" } else { "update" }
        );
        return Ok(());
    }
    require_portable_target()?;
    if standalone::owns(&executable)? {
        return standalone::update(&executable, rollback);
    }
    if let Some(launcher) = std::env::var_os(bun::LAUNCHER_ENV) {
        return bun::update(Path::new(&launcher), &executable, rollback);
    }
    Err(format!(
        "cannot identify the installer that owns {}; run the Bun global command or use the installer for this copy",
        executable.display()
    ))
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

fn verify_executable(path: &Path, expected: &Version) -> Result<(), String> {
    let scratch =
        tempfile::tempdir().map_err(|error| format!("verification directory: {error}"))?;
    let result = Command::new(path)
        .arg("--version")
        .current_dir(scratch.path())
        .env_clear()
        .env("XDG_CONFIG_HOME", scratch.path())
        .env("XDG_DATA_HOME", scratch.path())
        .env("XDG_CACHE_HOME", scratch.path())
        .output()
        .map_err(|error| format!("cannot run {}: {error}", path.display()))?;
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
