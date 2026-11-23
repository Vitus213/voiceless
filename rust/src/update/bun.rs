//! Bun owns both its global launcher and the exact-version platform package.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use semver::Version;
use serde::Deserialize;

pub(super) const LAUNCHER_ENV: &str = "VOICELESS_BUN_LAUNCHER";
const PACKAGE: &str = "voiceless";
const LAUNCHER: &str = "bin/voiceless";

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    #[serde(default, rename = "optionalDependencies")]
    platform_packages: BTreeMap<String, String>,
}

impl Package {
    fn from_dir(path: &Path) -> Result<Self, String> {
        let manifest = path.join("package.json");
        let data =
            fs::read(&manifest).map_err(|error| format!("{}: {error}", manifest.display()))?;
        serde_json::from_slice(&data).map_err(|error| format!("{}: {error}", manifest.display()))
    }

    fn platform_version(&self, platform: &str) -> Result<Version, String> {
        if self.name != PACKAGE || self.platform_packages.get(platform) != Some(&self.version) {
            return Err(format!(
                "the Bun release does not pin its {platform} package to the application version"
            ));
        }
        let version = Version::parse(&self.version)
            .map_err(|error| format!("invalid Bun release version: {error}"))?;
        if !version.pre.is_empty() || !version.build.is_empty() {
            return Err(format!(
                "Bun release version {version} is not a stable release"
            ));
        }
        Ok(version)
    }
}

struct GlobalInstall {
    bin: PathBuf,
    project: PathBuf,
}

impl GlobalInstall {
    fn discover() -> Result<Self, String> {
        let bin = PathBuf::from(bun_output(&["pm", "bin", "-g"])?.trim());
        let listing = bun_output(&["pm", "ls", "-g"])?;
        let project = listing
            .lines()
            .find_map(|line| {
                line.rsplit_once(" node_modules (")
                    .map(|(path, _)| PathBuf::from(path))
            })
            .filter(|path| path.is_absolute())
            .ok_or_else(|| "Bun did not report an absolute global project directory".to_owned())?;
        if !bin.is_absolute() {
            return Err("Bun did not report an absolute global binary directory".to_owned());
        }
        Ok(Self { bin, project })
    }

    fn active(&self, platform: &str) -> Result<(PathBuf, PathBuf, Version), String> {
        let modules = self.project.join("node_modules");
        let package = modules.join(PACKAGE);
        let entry = self.bin.join(PACKAGE);
        let launcher = canonical(&entry)?;
        if !entry.is_symlink() || launcher != canonical(&package.join(LAUNCHER))? {
            return Err(format!(
                "{} does not resolve to the Voiceless package in this Bun global project",
                entry.display()
            ));
        }
        let version = Package::from_dir(&package)?.platform_version(platform)?;
        // Bun's isolated linker places leaf links in the root package's
        // node_modules. The hoisted linker puts the leaf next to it.
        let nested = package.join("node_modules").join(platform);
        let leaf = if nested.exists() {
            nested
        } else {
            modules.join(platform)
        };
        let manifest = Package::from_dir(&leaf)?;
        if manifest.name != platform || manifest.version != version.to_string() {
            return Err(format!(
                "the installed {platform} package does not match Voiceless {version}"
            ));
        }
        let binary = canonical(&leaf.join(LAUNCHER))?;
        Ok((launcher, binary, version))
    }
}

pub(super) fn update(launcher: &Path, executable: &Path, rollback: bool) -> Result<(), String> {
    let platform = format!("{PACKAGE}-{}", super::target());
    let install = GlobalInstall::discover()?;
    let (owned_launcher, owned_executable, installed) = install.active(&platform)?;
    if launcher != owned_launcher || executable != owned_executable || installed != super::version()
    {
        return Err(
            "the running Voiceless does not match the active Bun global installation".to_owned(),
        );
    }
    if rollback {
        return Err("Bun owns this installation. To restore a specific release, use bun add -g --exact voiceless@<version>.".to_owned());
    }

    // Both commands run with the same Bun global project and registry
    // settings. Pin the checked version so a moving latest tag cannot win.
    let response = bun_output(&[
        "info",
        "--global",
        "--json",
        "--no-cache",
        "--no-env-file",
        "voiceless@latest",
    ])?;
    let latest: Package = serde_json::from_str(&response)
        .map_err(|error| format!("invalid Bun registry metadata for Voiceless: {error}"))?;
    let expected = latest.platform_version(&platform)?;
    if expected <= installed {
        println!("Voiceless {installed} is current in this Bun global installation.");
        return Ok(());
    }
    let spec = format!("{PACKAGE}@{expected}");
    let status = Command::new("bun")
        .args([
            "add",
            "--global",
            "--exact",
            "--ignore-scripts",
            "--no-env-file",
            &spec,
        ])
        .status()
        .map_err(|error| format!("cannot start Bun: {error}"))?;
    if !status.success() {
        return Err(format!("Bun did not install {spec}: {status}"));
    }
    let (_, binary, active) = install.active(&platform)?;
    if active != expected {
        return Err(format!(
            "the Bun global installation reports {active}; expected {expected}"
        ));
    }
    super::verify_executable(&binary, &expected)?;
    println!("Updated Voiceless to {expected} at {}. Restart a running daemon when idle to use the new version.", install.bin.join(PACKAGE).display());
    Ok(())
}

fn bun_output(args: &[&str]) -> Result<String, String> {
    let result = Command::new("bun")
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .map_err(|error| format!("cannot start Bun: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "bun {} failed ({}): {}",
            args.join(" "),
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    String::from_utf8(result.stdout).map_err(|error| format!("invalid Bun output: {error}"))
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))
}
