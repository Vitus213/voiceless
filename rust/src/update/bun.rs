//! Bun owns both its global launcher and the exact-version platform package.
//! `voiceless update` only asks Bun to install a newer exact version, after
//! saving a data snapshot, and never writes Bun's directories itself.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use semver::Version;
use serde::Deserialize;

use super::{data, BeforeSwitch, Outcome};

pub(super) const LAUNCHER_ENV: &str = "VOICELESS_BUN_LAUNCHER";
const PACKAGE: &str = "voiceless";
const LAUNCHER: &str = "bin/voiceless";

/// How the updater reaches Bun: `bun` from `PATH`, or in tests a script
/// that answers like Bun.
pub(super) struct Bun {
    program: PathBuf,
}

impl Bun {
    fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.args(args).env("NO_COLOR", "1");
        command
    }

    /// Run Bun and keep what it printed; the arguments are on the error.
    fn output(&self, args: &[&str]) -> Result<String, String> {
        let Output {
            status,
            stdout,
            stderr,
        } = self
            .command(args)
            .output()
            .map_err(|error| format!("cannot start Bun: {error}"))?;
        if !status.success() {
            return Err(format!(
                "bun {} failed ({status}): {}",
                args.join(" "),
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        String::from_utf8(stdout).map_err(|error| format!("invalid Bun output: {error}"))
    }
}

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
    fn discover(bun: &Bun) -> Result<Self, String> {
        let bin = PathBuf::from(bun.output(&["pm", "bin", "-g"])?.trim());
        let listing = bun.output(&["pm", "ls", "-g"])?;
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

/// Update the Bun global installation that owns `executable`; rolling back
/// is Bun's own to do.
pub(super) fn update(
    launcher: &Path,
    executable: &Path,
    rollback: bool,
    before_switch: BeforeSwitch<'_>,
) -> Result<Outcome, String> {
    update_with(
        &Bun::new("bun"),
        launcher,
        executable,
        rollback,
        before_switch,
    )
}

fn update_with(
    bun: &Bun,
    launcher: &Path,
    executable: &Path,
    rollback: bool,
    before_switch: BeforeSwitch<'_>,
) -> Result<Outcome, String> {
    let platform = format!("{PACKAGE}-{}", super::target());
    let install = GlobalInstall::discover(bun)?;
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
    let response = bun.output(&[
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
        let mut lines = vec![format!(
            "Installed: Voiceless {installed}; target: {installed} (Bun global package)."
        )];
        lines.insert(
            0,
            format!("Voiceless {installed} is current in this Bun global installation."),
        );
        return Ok(Outcome {
            lines,
            installed,
            previous: None,
            command: Some(owned_executable),
            immutable: Vec::new(),
            snapshot: None,
        });
    }

    // The data as the installed version wrote it, before Bun replaces the program.
    let snapshot = before_switch(data::Request {
        reason: data::Reason::Upgrade,
        channel: "bun",
        from: installed.clone(),
        to: Some(expected.clone()),
    })?;
    let spec = format!("{PACKAGE}@{expected}");
    let recovery = |error: String| {
        let mut message = format!("{error}. Restore the previous program with `bun add -g --exact {PACKAGE}@{installed}`.");
        if let Some(saved) = &snapshot {
            message.push_str(&format!("\n{saved}"));
        }
        message
    };
    let status = bun
        .command(&[
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
        return Err(recovery(format!("Bun did not install {spec}: {status}")));
    }
    let (_, binary, active) = install.active(&platform).map_err(&recovery)?;
    if active != expected {
        return Err(recovery(format!(
            "the Bun global installation reports {active}; expected {expected}"
        )));
    }
    super::verify_executable(&binary, &expected).map_err(recovery)?;
    Ok(Outcome {
        lines: vec![
            format!(
                "Updated Voiceless to {expected} at {}. Restart a running daemon when idle to use the new version.",
                install.bin.join(PACKAGE).display()
            ),
            format!(
                "Installed: Voiceless {expected} (was {installed}); target: {expected} (Bun global package)."
            ),
        ],
        installed: active,
        previous: Some(installed),
        command: Some(binary),
        immutable: Vec::new(),
        snapshot,
    })
}

/// Bun's Linux linker can leave the running inode hard-linked in its cache.
/// Cache eviction or a different linker leaves the daemon version unknown.
pub(super) fn cached_version(image: &super::running::Image) -> Option<Version> {
    let cache = Bun::new("bun").output(&["pm", "cache"]).ok()?;
    let platform = format!("{PACKAGE}-{}", super::target());
    let versions = fs::read_dir(Path::new(cache.trim()).join(&platform)).ok()?;
    versions.flatten().find_map(|entry| {
        let directory = entry.path();
        let file = fs::metadata(directory.join(LAUNCHER)).ok()?;
        if file.dev() != image.device || file.ino() != image.inode {
            return None;
        }
        let package = Package::from_dir(&directory).ok()?;
        if package.name != platform {
            return None;
        }
        Version::parse(&package.version).ok()
    })
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// A Bun installation under a scratch directory: the global project with
    /// its command link, and a `bun` that answers like the real one. It
    /// offers `offered` as the latest version.
    struct Installed {
        scratch: tempfile::TempDir,
        bun: Bun,
        launcher: PathBuf,
        executable: PathBuf,
    }

    fn installed(offered: &str) -> Installed {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        let platform = format!("{PACKAGE}-{}", super::super::target());
        let version = env!("CARGO_PKG_VERSION");
        let project = root.join("bun-global/node_modules");
        let optional = format!("{{\"{platform}\":\"{version}\"}}");
        let manifest = |name: &str, extra: &str| {
            let directory = project.join(name);
            fs::create_dir_all(directory.join("bin")).unwrap();
            // Spelled as npm writes it: the script's `sed` looks for the space.
            let json = format!("{{\"name\": \"{name}\", \"version\": \"{version}\"{extra}}}");
            fs::write(directory.join("package.json"), json).unwrap();
        };
        manifest(PACKAGE, &format!(", \"optionalDependencies\": {optional}"));
        manifest(&platform, "");
        // The root package's launcher, which the updater never runs, and the
        // platform binary that reports the installed version.
        let launcher = project.join(PACKAGE).join(LAUNCHER);
        let binary = project.join(&platform).join(LAUNCHER);
        fs::write(&launcher, "#!/bin/sh\nexit 1\n").unwrap();
        fs::write(
            &binary,
            format!("#!/bin/sh\necho \"voiceless {version}\"\n"),
        )
        .unwrap();
        for file in [&launcher, &binary] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let bin = root.join("bun-bin");
        fs::create_dir_all(&bin).unwrap();
        symlink(&launcher, bin.join(PACKAGE)).unwrap();
        fs::write(
            root.join("info.json"),
            format!("{{\"name\":\"{PACKAGE}\",\"version\":\"{offered}\",\"optionalDependencies\":{{\"{platform}\":\"{offered}\"}}}}"),
        )
        .unwrap();
        // The script stands in for Bun: it reports the layout above, and
        // `add` rewrites it the way `bun add --global --exact` would. It
        // refuses to install before the snapshot exists, and whenever asked.
        let shown = root.display();
        fs::write(
            root.join("bun"),
            format!(
                r#"#!/bin/sh
root={shown}
case "$1" in
  pm)
    case "$2" in
      bin) printf '%s\n' "$root/bun-bin" ;;
      ls) printf '%s/bun-global node_modules (2)\n' "$root" ;;
    esac ;;
  info) cat "$root/info.json" ;;
  add)
    test -f "$root/snapshot-taken" || {{ echo 'bun add ran before the snapshot' >&2; exit 3; }}
    test ! -f "$root/refuse-add" || {{ echo 'registry unreachable' >&2; exit 1; }}
    version="${{6#voiceless@}}"
    printf '#!/bin/sh\necho "voiceless %s"\n' "$version" > "$root/bun-global/node_modules/{platform}/bin/voiceless"
    for package in {PACKAGE} {platform}; do
      sed -i 's/"{version}"/"'"$version"'"/g' "$root/bun-global/node_modules/$package/package.json"
    done ;;
esac
"#
            ),
        )
        .unwrap();
        fs::set_permissions(root.join("bun"), fs::Permissions::from_mode(0o755)).unwrap();
        // Another channel's files, which this update must leave alone.
        fs::create_dir_all(root.join("releases/other-channel")).unwrap();
        fs::write(root.join("releases/other-channel/payload"), "untouched").unwrap();

        Installed {
            launcher: fs::canonicalize(&launcher).unwrap(),
            executable: fs::canonicalize(&binary).unwrap(),
            bun: Bun::new(root.join("bun")),
            scratch,
        }
    }

    fn running(installed: &Installed) -> Version {
        let platform = format!("{PACKAGE}-{}", super::super::target());
        GlobalInstall::discover(&installed.bun)
            .unwrap()
            .active(&platform)
            .unwrap()
            .2
    }

    #[test]
    fn an_upgrade_snapshots_the_data_and_then_lets_bun_install() {
        let installed = installed("99.0.0");
        let root = installed.scratch.path();
        let requests = std::cell::RefCell::new(Vec::new());
        let outcome = update_with(
            &installed.bun,
            &installed.launcher,
            &installed.executable,
            false,
            &|request| {
                // Bun finds the snapshot in place when it installs.
                fs::write(root.join("snapshot-taken"), "").unwrap();
                assert_eq!(running(&installed), super::super::version());
                requests.borrow_mut().push(request);
                Ok(None)
            },
        )
        .unwrap();
        let requests = requests.into_inner();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].reason, data::Reason::Upgrade);
        assert_eq!(requests[0].channel, "bun");
        assert_eq!(requests[0].from, super::super::version());
        assert_eq!(requests[0].to, Some(Version::new(99, 0, 0)));
        assert_eq!(outcome.installed, Version::new(99, 0, 0));
        assert_eq!(outcome.previous, Some(super::super::version()));
        assert_eq!(outcome.command, Some(installed.executable.clone()));
        assert!(outcome.lines[0].starts_with("Updated Voiceless to 99.0.0 at "));
        assert!(outcome.lines[0].contains("Restart a running daemon when idle"));
        assert_eq!(
            outcome.lines[1],
            format!(
                "Installed: Voiceless 99.0.0 (was {}); target: 99.0.0 (Bun global package).",
                super::super::version()
            )
        );
        assert_eq!(
            fs::read_to_string(root.join("releases/other-channel/payload")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn a_current_installation_is_reported_and_left_alone() {
        let installed = installed(env!("CARGO_PKG_VERSION"));
        let outcome = update_with(
            &installed.bun,
            &installed.launcher,
            &installed.executable,
            false,
            &|_| panic!("nothing changes, so nothing is snapshotted"),
        )
        .unwrap();

        assert_eq!(
            outcome.lines[0],
            format!(
                "Voiceless {} is current in this Bun global installation.",
                super::super::version()
            )
        );
        assert_eq!(outcome.installed, super::super::version());
        assert_eq!(outcome.previous, None);
        assert!(outcome.snapshot.is_none());
    }

    #[test]
    fn a_failed_install_keeps_the_installed_version() {
        let installed = installed("99.0.0");
        fs::write(installed.scratch.path().join("refuse-add"), "").unwrap();
        let error = update_with(
            &installed.bun,
            &installed.launcher,
            &installed.executable,
            false,
            &|_| {
                fs::write(installed.scratch.path().join("snapshot-taken"), "").unwrap();
                Ok(None)
            },
        )
        .unwrap_err();

        assert!(
            error.contains("Bun did not install voiceless@99.0.0"),
            "{error}"
        );
        assert!(error.contains(&format!(
            "bun add -g --exact voiceless@{}",
            super::super::version()
        )));
        assert_eq!(running(&installed), super::super::version());
    }

    #[test]
    fn a_rollback_and_a_foreign_launcher_are_left_to_bun() {
        let installed = installed("99.0.0");
        let error = update_with(
            &installed.bun,
            &installed.launcher,
            &installed.executable,
            true,
            &|_| panic!("nothing is switched"),
        )
        .unwrap_err();
        assert!(
            error.contains("bun add -g --exact voiceless@<version>"),
            "{error}"
        );

        let error = update_with(
            &installed.bun,
            Path::new("/elsewhere/bin/voiceless"),
            &installed.executable,
            false,
            &|_| panic!("nothing is switched"),
        )
        .unwrap_err();
        assert!(error.contains("does not match the active Bun global installation"));
    }
}
