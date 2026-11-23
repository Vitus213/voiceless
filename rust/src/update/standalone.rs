use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flate2::bufread::GzDecoder;
use semver::Version;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir};

const REPOSITORY: &str = "Vitus213/voiceless";
const COMMAND: &str = "bin/voiceless";
const MANIFEST: &str = "release.json";
const NOTICES: &str = "THIRD_PARTY_NOTICES.txt";
const MAX_ARCHIVE: u64 = (2 << 30) - 1;
const MAX_EXPANDED: u64 = 4 << 30;
const MAX_ENTRIES: usize = 16384;
const MAX_MANIFEST: u64 = 1 << 20;
const MAX_RELEASE_METADATA: u64 = 2 << 20;
const MAX_ACTIVATION: u64 = 8192;

type VerifyExecutable = fn(&Path, &Version) -> Result<(), String>;

struct InstallPaths {
    root: PathBuf,
    command: PathBuf,
}

impl InstallPaths {
    fn from_environment() -> Result<Self, String> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("standalone installation requires an absolute HOME directory")?;
        let root = Self::data_root();
        if !root.is_absolute() {
            return Err("standalone installation requires an absolute XDG data directory".into());
        }
        Ok(Self {
            root,
            command: home.join(".local").join(COMMAND),
        })
    }

    fn data_root() -> PathBuf {
        crate::models::default_data_dir().join("releases")
    }

    fn canonical(&self) -> Result<Self, String> {
        owned_directory(&self.root, true)?;
        Ok(Self {
            root: at(&self.root, self.root.canonicalize())?,
            command: self.command.clone(),
        })
    }

    fn create(&self) -> Result<Self, String> {
        let parent = parent(&self.root);
        create_directories(parent)?;
        create_private_directory(&self.root)?;
        self.canonical()
    }

    fn payload(&self, release: &PinnedRelease) -> PathBuf {
        self.root.join("payloads").join(release.directory_name())
    }

    fn command_destination(&self) -> PathBuf {
        self.root.join("current").join(COMMAND)
    }

    fn check_command(&self) -> Result<(), String> {
        let directory = parent(&self.command);
        owned_directory(&at(directory, directory.canonicalize())?, false)?;
        let meta = at(&self.command, fs::symlink_metadata(&self.command))?;
        if !meta.is_symlink() || meta.uid() != effective_uid() {
            return Err(format!(
                "{} is not a user-owned standalone command link",
                self.command.display()
            ));
        }
        if at(&self.command, fs::read_link(&self.command))? != self.command_destination() {
            return Err(format!(
                "{} points outside this standalone installation",
                self.command.display()
            ));
        }
        Ok(())
    }
}

// Each activation owns the rollback record and the link to its active payload.
// Renaming `current` commits both changes with one filesystem operation.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    current: PinnedRelease,
    previous: Option<PinnedRelease>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PinnedRelease {
    version: String,
    archive_sha256: String,
    manifest_sha256: String,
}

impl PinnedRelease {
    fn directory_name(&self) -> String {
        format!("v{}-{}", self.version, self.archive_sha256)
    }

    fn version(&self) -> Result<Version, String> {
        if !valid_digest(&self.archive_sha256) || !valid_digest(&self.manifest_sha256) {
            return Err("standalone activation contains an invalid release digest".into());
        }
        stable_version(&self.version)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    target: String,
    variant: String,
    files: BTreeMap<String, String>,
}

struct Candidate {
    directory: TempDir,
    release: PinnedRelease,
}

pub(super) fn install(archive: &Path, sha256: &str) -> Result<(), String> {
    let paths = InstallPaths::from_environment()?;
    let digest = sha256.to_ascii_lowercase();
    if !valid_digest(&digest) {
        return Err("--sha256 must be a 64-digit hexadecimal SHA-256 digest".into());
    }
    let file = at(
        archive,
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(archive),
    )?;
    if !at(archive, file.metadata())?.is_file() {
        return Err(format!(
            "{} is not a regular release archive",
            archive.display()
        ));
    }
    install_at(
        &paths,
        file,
        &digest,
        env::var_os("PATH").as_deref(),
        super::verify_executable,
    )
}

pub(super) fn owns(executable: &Path) -> Result<bool, String> {
    let paths = match InstallPaths::from_environment() {
        Ok(paths) => paths,
        Err(error) if inside_root(executable, &InstallPaths::data_root()) => return Err(error),
        Err(_) => return Ok(false),
    };
    owns_at(&paths, executable, &super::version())
}

fn inside_root(executable: &Path, root: &Path) -> bool {
    executable.starts_with(root)
        || root
            .canonicalize()
            .is_ok_and(|root| executable.starts_with(root))
}

fn owns_at(paths: &InstallPaths, executable: &Path, version: &Version) -> Result<bool, String> {
    if !inside_root(executable, &paths.root) {
        return Ok(false);
    }
    let paths = paths.canonical()?;
    let active = check_active_run(&paths, executable, version)?;
    read_pinned_manifest(&paths.payload(&active.current), &active.current)?;
    Ok(true)
}

pub(super) fn update(executable: &Path, rollback: bool) -> Result<(), String> {
    let paths = InstallPaths::from_environment()?.canonical()?;
    update_at(
        &paths,
        executable,
        &super::version(),
        rollback,
        super::verify_executable,
        |paths, version| fetch_candidate(paths, version, super::verify_executable),
    )
}

fn install_at(
    paths: &InstallPaths,
    archive: impl Read,
    digest: &str,
    search_path: Option<&OsStr>,
    verify: VerifyExecutable,
) -> Result<(), String> {
    supported_target()?;
    let paths = paths.create()?;
    let _lock = lock(&paths.root)?;
    let has_command = exists(&paths.command)?;
    if has_command {
        paths.check_command()?;
    } else {
        check_path_conflicts(&paths.command, search_path)?;
    }
    let active = if exists(&paths.root.join("current"))? {
        Some(read_activation(&paths)?)
    } else if has_command {
        return Err("standalone command exists with no activation record".into());
    } else {
        None
    };

    let candidate = stage_candidate(&paths, archive, None, digest, None, verify)?;
    if !has_command {
        let directory = parent(&paths.command);
        create_directories(directory)?;
        owned_directory(&at(directory, directory.canonicalize())?, false)?;
    }
    let version = candidate.release.version()?;
    let active = match active {
        Some(active) if candidate.release == active.current => {
            validate_pinned(&paths, &active.current, verify)?;
            active
        }
        Some(active) => {
            if !has_command {
                return Err("standalone installation is missing its stable command link; retry with the active archive".into());
            }
            if active.previous.as_ref() != Some(&candidate.release) {
                if version.cmp_precedence(&active.current.version()?) != Ordering::Greater {
                    return Err(format!(
                        "standalone {} is already active; release {version} cannot replace it",
                        active.current.version
                    ));
                }
                validate_payload(&paths.payload(&active.current), &active.current)?;
            }
            let current = retain_candidate(&paths, candidate, verify)?;
            let next = Activation {
                current,
                previous: Some(active.current),
            };
            activate(&paths, &next)?;
            next
        }
        None => {
            let current = retain_candidate(&paths, candidate, verify)?;
            let next = Activation {
                current,
                previous: None,
            };
            activate(&paths, &next)?;
            next
        }
    };

    if !has_command {
        at(
            &paths.command,
            symlink(paths.command_destination(), &paths.command),
        )?;
        sync_after_commit(
            parent(&paths.command),
            &paths.command,
            &active.current.version,
        )?;
    }
    println!(
        "Standalone Voiceless {} is active at {}.",
        active.current.version,
        paths.command.display()
    );
    Ok(())
}

fn update_at(
    paths: &InstallPaths,
    executable: &Path,
    running_version: &Version,
    rollback: bool,
    verify: VerifyExecutable,
    fetch: impl FnOnce(&InstallPaths, &Version) -> Result<Option<Candidate>, String>,
) -> Result<(), String> {
    let _lock = lock(&paths.root)?;
    let active = check_active_run(paths, executable, running_version)?;
    validate_payload(&paths.payload(&active.current), &active.current)?;
    let current = if rollback {
        let previous = active
            .previous
            .ok_or("no previous standalone release is available")?;
        validate_pinned(paths, &previous, verify)?;
        previous
    } else {
        let Some(candidate) = fetch(paths, running_version)? else {
            println!("Standalone Voiceless {running_version} is already up to date.");
            return Ok(());
        };
        retain_candidate(paths, candidate, verify)?
    };
    let next = Activation {
        current,
        previous: Some(active.current),
    };
    activate(paths, &next)?;
    println!(
        "Standalone Voiceless {} is active at {}.",
        next.current.version,
        paths.command.display()
    );
    Ok(())
}

fn check_active_run(
    paths: &InstallPaths,
    executable: &Path,
    running_version: &Version,
) -> Result<Activation, String> {
    paths.check_command()?;
    let active = read_activation(paths)?;
    let expected = paths.payload(&active.current).join(COMMAND);
    owned_directory(&paths.root.join("payloads"), true)?;
    owned_directory(parent(parent(&expected)), true)?;
    owned_directory(parent(&expected), false)?;
    owned_file(&expected)?;
    let command = at(&paths.command, paths.command.canonicalize())?;
    let running = at(executable, executable.canonicalize())?;
    if running != expected || command != expected || &active.current.version()? != running_version {
        return Err(format!(
            "running executable does not match active standalone Voiceless {}; run {}",
            active.current.version,
            paths.command.display()
        ));
    }
    Ok(active)
}

fn read_activation(paths: &InstallPaths) -> Result<Activation, String> {
    let current = paths.root.join("current");
    owned_link(&current)?;
    let activation = at(&current, fs::read_link(&current))?;
    let name = activation.to_str().unwrap_or_default();
    if !name.strip_prefix("activation-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err("invalid standalone activation link".into());
    }
    let directory = paths.root.join(activation);
    owned_directory(&directory, true)?;
    let state = directory.join("state.json");
    let active: Activation = json_from_path(&state, MAX_ACTIVATION)?;
    active.current.version()?;
    if let Some(previous) = &active.previous {
        previous.version()?;
        if previous.directory_name() == active.current.directory_name() {
            return Err("standalone activation references the same release twice".into());
        }
    }
    let bin = directory.join("bin");
    owned_link(&bin)?;
    let expected = Path::new("../payloads")
        .join(active.current.directory_name())
        .join("bin");
    if at(&bin, fs::read_link(&bin))? != expected {
        return Err("standalone activation command does not match its release record".into());
    }
    Ok(active)
}

fn activate(paths: &InstallPaths, active: &Activation) -> Result<(), String> {
    let directory = private_tempdir(&paths.root, "activation-")?;
    let state = directory.path().join("state.json");
    let mut file = create_file(&state, 0o600)?;
    serde_json::to_writer(&mut file, active)
        .map_err(|error| format!("writing {}: {error}", state.display()))?;
    at(&state, file.sync_all())?;
    let bin = directory.path().join("bin");
    let destination = Path::new("../payloads")
        .join(active.current.directory_name())
        .join("bin");
    at(&bin, symlink(destination, &bin))?;
    sync_directory(directory.path())?;
    sync_directory(&paths.root)?;

    let name = directory
        .path()
        .file_name()
        .expect("temporary activation has a name");
    let pending = paths
        .root
        .join(format!(".current-{}", name.to_string_lossy()));
    let current = paths.root.join("current");
    at(&pending, symlink(name, &pending))?;
    if let Err(error) = at(&current, fs::rename(&pending, &current)) {
        let _ = fs::remove_file(&pending);
        return Err(error);
    }
    let _ = directory.keep();
    sync_after_commit(&paths.root, &paths.command, &active.current.version)
}

fn sync_after_commit(directory: &Path, command: &Path, version: &str) -> Result<(), String> {
    sync_directory(directory).map_err(|error| {
        let observed = command.canonicalize().map_or_else(|error| error.to_string(), |path| path.display().to_string());
        format!("standalone release {version} was switched, but its durability is unknown ({error}); {} resolves to {observed}", command.display())
    })
}

fn retain_candidate(
    paths: &InstallPaths,
    candidate: Candidate,
    verify: VerifyExecutable,
) -> Result<PinnedRelease, String> {
    let destination = paths.payload(&candidate.release);
    if exists(&destination)? {
        validate_pinned(paths, &candidate.release, verify)?;
    } else {
        create_private_directory(parent(&destination))?;
        at(
            &destination,
            fs::rename(candidate.directory.path(), &destination),
        )?;
        let _ = candidate.directory.keep();
        sync_directory(parent(&destination))?;
        sync_directory(&paths.root)?;
    }
    Ok(candidate.release)
}

fn validate_pinned(
    paths: &InstallPaths,
    release: &PinnedRelease,
    verify: VerifyExecutable,
) -> Result<(), String> {
    let version = release.version()?;
    let directory = paths.payload(release);
    owned_directory(parent(&directory), true)?;
    validate_payload(&directory, release)?;
    verify(&directory.join(COMMAND), &version)
}

fn stage_candidate(
    paths: &InstallPaths,
    archive: impl Read,
    size: Option<u64>,
    digest: &str,
    expected_version: Option<&Version>,
    verify: VerifyExecutable,
) -> Result<Candidate, String> {
    let mut snapshot = at(&paths.root, tempfile::tempfile_in(&paths.root))?;
    let limit = size.unwrap_or(MAX_ARCHIVE);
    let (actual_size, actual_digest) = copy_and_hash(archive.take(limit + 1), &mut snapshot)
        .map_err(|error| format!("reading release archive: {error}"))?;
    if actual_size == 0 || actual_size > limit || size.is_some_and(|size| size != actual_size) {
        return Err(format!(
            "release archive has an unexpected size ({actual_size} bytes)"
        ));
    }
    if actual_digest != digest {
        return Err(format!(
            "release archive SHA-256 mismatch: expected {digest}, got {actual_digest}"
        ));
    }
    snapshot
        .rewind()
        .map_err(|error| format!("rewinding release archive: {error}"))?;

    let directory = private_tempdir(&paths.root, ".stage-")?;
    unpack(snapshot, directory.path())?;
    let manifest_path = directory.path().join(MANIFEST);
    let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST)?;
    let manifest: Manifest = parse_json(&manifest_bytes, &manifest_path.display().to_string())?;
    let version = stable_version(&manifest.version)?;
    if expected_version.is_some_and(|expected| expected != &version) {
        return Err(format!(
            "release manifest version {version} does not match the selected tag"
        ));
    }
    let release = PinnedRelease {
        version: manifest.version.clone(),
        archive_sha256: digest.to_owned(),
        manifest_sha256: format!("{:x}", Sha256::digest(&manifest_bytes)),
    };
    check_inventory(directory.path(), &release, manifest)?;
    verify(&directory.path().join(COMMAND), &version)?;
    sync_directory(directory.path())?;
    Ok(Candidate { directory, release })
}

fn unpack(snapshot: File, destination: &Path) -> Result<(), String> {
    let reader = GzDecoder::new(BufReader::new(snapshot)).take(MAX_EXPANDED + 1);
    let mut archive = tar::Archive::new(reader);
    let mut seen = std::collections::BTreeSet::new();
    let entries = archive
        .entries()
        .map_err(|error| format!("reading release tar archive: {error}"))?;
    for item in entries.raw(true) {
        if seen.len() == MAX_ENTRIES {
            return Err("release archive contains too many entries".into());
        }
        let mut entry = item.map_err(|error| format!("reading release tar header: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("reading release entry path: {error}"))?;
        let kind = entry.header().entry_type();
        let relative = normalized_relative(&path)?;
        if !seen.insert(relative.clone()) {
            return Err(format!("duplicate archive entry {}", relative.display()));
        }
        if relative.as_os_str().is_empty() && kind.is_dir() && entry.size() == 0 {
            continue;
        }
        let output = destination.join(&relative);
        if kind.is_dir() && entry.size() == 0 && allowed_directory(&relative) {
            create_directories(&output)?;
        } else if kind.is_file() && (relative == Path::new(MANIFEST) || allowed_file(&relative)) {
            let size = entry.size();
            if size > MAX_EXPANDED || (relative == Path::new(MANIFEST) && size > MAX_MANIFEST) {
                return Err(format!("archive entry {} is too large", relative.display()));
            }
            create_directories(parent(&output))?;
            let mut file = create_file(
                &output,
                if relative == Path::new(COMMAND) {
                    0o700
                } else {
                    0o600
                },
            )?;
            let copied = at(&output, io::copy(&mut entry, &mut file))?;
            if copied != size {
                return Err(format!("truncated archive entry {}", relative.display()));
            }
            at(&output, file.sync_all())?;
            sync_directory(parent(&output))?;
        } else {
            return Err(format!(
                "unexpected release archive entry {}",
                relative.display()
            ));
        }
    }

    // The tar reader stops at the first zero block. Consume gzip's trailer and
    // allow only zero tar padding after the last entry.
    let mut reader = archive.into_inner();
    let mut buffer = [0; 8192];
    let mut padding = 0;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("checking release gzip trailer: {error}"))?;
        if count == 0 {
            break;
        }
        padding += count;
        if buffer[..count].iter().any(|&byte| byte != 0) {
            return Err("unexpected data after the release tar entries".into());
        }
    }
    if reader.limit() == 0 {
        return Err("expanded release archive exceeds the size limit".into());
    }
    if padding < 512 || padding % 512 != 0 {
        return Err("release tar archive is missing its end marker".into());
    }
    let mut gzip_source = reader.into_inner().into_inner();
    if !gzip_source
        .fill_buf()
        .map_err(|error| format!("checking end of release archive: {error}"))?
        .is_empty()
    {
        return Err("unexpected data after release gzip stream".into());
    }
    Ok(())
}

fn validate_payload(directory: &Path, release: &PinnedRelease) -> Result<(), String> {
    let manifest = read_pinned_manifest(directory, release)?;
    check_inventory(directory, release, manifest)
}

fn read_pinned_manifest(directory: &Path, release: &PinnedRelease) -> Result<Manifest, String> {
    owned_directory(directory, true)?;
    let path = directory.join(MANIFEST);
    let bytes = read_bounded(&path, MAX_MANIFEST)?;
    if format!("{:x}", Sha256::digest(&bytes)) != release.manifest_sha256 {
        return Err(format!(
            "installed release manifest SHA-256 mismatch at {}",
            path.display()
        ));
    }
    parse_json(&bytes, &path.display().to_string())
}

fn check_inventory(
    directory: &Path,
    release: &PinnedRelease,
    manifest: Manifest,
) -> Result<(), String> {
    let target = supported_target()?;
    if manifest.schema != 1
        || manifest.version != release.version
        || manifest.target != target
        || manifest.variant != "cpu"
    {
        return Err(format!(
            "invalid release manifest for Voiceless {} ({target}, cpu)",
            release.version
        ));
    }
    for required in [COMMAND, "LICENSE", NOTICES] {
        if !manifest.files.contains_key(required) {
            return Err(format!("release manifest is missing {required}"));
        }
    }
    if !manifest.files.keys().any(|name| name.starts_with("lib/")) {
        return Err("release manifest has no runtime libraries".into());
    }
    let mut files = manifest.files;
    verify_entries(directory, Path::new(""), &mut files)?;
    if let Some((missing, _)) = files.first_key_value() {
        return Err(format!("release payload is missing {missing}"));
    }
    check_elf(&directory.join(COMMAND), &target)
}

fn verify_entries(
    directory: &Path,
    prefix: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let full = directory.join(prefix);
    for entry in at(&full, fs::read_dir(&full))? {
        let entry = at(&full, entry)?;
        let path = entry.path();
        let relative = prefix.join(entry.file_name());
        let meta = at(&path, fs::symlink_metadata(&path))?;
        check_owner_and_mode(&path, &meta, false)?;
        if meta.is_dir() && allowed_directory(&relative) {
            verify_entries(directory, &relative, files)?;
        } else if meta.is_file() && relative == Path::new(MANIFEST) {
            continue;
        } else if meta.is_file() && allowed_file(&relative) {
            let name = relative
                .to_str()
                .ok_or_else(|| format!("release path {} is not UTF-8", relative.display()))?;
            let expected = files
                .remove(name)
                .ok_or_else(|| format!("unexpected release file {name}"))?;
            if !valid_digest(&expected) || meta.len() > MAX_EXPANDED {
                return Err(format!("invalid release file digest or size for {name}"));
            }
            let (_, actual) = at(
                &path,
                copy_and_hash(open_read(&path)?.take(MAX_EXPANDED + 1), io::sink()),
            )?;
            if actual != expected {
                return Err(format!("release file SHA-256 mismatch for {name}"));
            }
        } else {
            return Err(format!(
                "unexpected release payload entry {}",
                relative.display()
            ));
        }
    }
    Ok(())
}

fn check_elf(path: &Path, target: &str) -> Result<(), String> {
    let mut file = open_read(path)?;
    let mut header = [0; 20];
    at(path, file.read_exact(&mut header))?;
    let kind = u16::from_le_bytes([header[16], header[17]]);
    let machine = u16::from_le_bytes([header[18], header[19]]);
    let expected = if target == "linux-x86_64-gnu" {
        62
    } else {
        183
    };
    if !header.starts_with(b"\x7fELF\x02\x01\x01") || !matches!(kind, 2 | 3) || machine != expected
    {
        return Err(format!(
            "{} is not a native {target} executable",
            path.display()
        ));
    }
    if at(path, file.metadata())?.mode() & 0o100 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(())
}

fn allowed_directory(path: &Path) -> bool {
    path == Path::new("bin")
        || path == Path::new("lib")
        || (path.starts_with("licenses") && path.as_os_str().len() <= 256)
}

fn allowed_file(path: &Path) -> bool {
    matches!(path.to_str(), Some(COMMAND | "LICENSE" | NOTICES))
        || (path.parent() == Some(Path::new("lib")) && path.file_name().is_some())
        || (path.starts_with("licenses") && path != Path::new("licenses"))
}

fn normalized_relative(path: &Path) -> Result<PathBuf, String> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if name.to_str().is_some() => relative.push(name),
            Component::CurDir => {}
            _ => return Err(format!("invalid release archive path {}", path.display())),
        }
    }
    Ok(relative)
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

fn fetch_candidate(
    paths: &InstallPaths,
    running: &Version,
    verify: VerifyExecutable,
) -> Result<Option<Candidate>, String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .user_agent(concat!("voiceless/", env!("CARGO_PKG_VERSION")))
        .build();
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let response = request(
        &agent,
        &url,
        "application/vnd.github+json",
        Duration::from_secs(30),
    )?;
    if response.status() != 200 {
        return Err(format!(
            "GitHub release metadata returned HTTP {}",
            response.status()
        ));
    }
    let metadata = bounded_bytes(response.into_reader(), MAX_RELEASE_METADATA)
        .map_err(|error| format!("reading GitHub release metadata: {error}"))?;
    let release: GithubRelease = parse_json(&metadata, "GitHub release metadata")?;
    let version = release
        .tag_name
        .strip_prefix('v')
        .ok_or("GitHub release tag must start with v")?;
    let version = stable_version(version)?;
    if release.draft || release.prerelease {
        return Err("GitHub returned a draft or prerelease for the stable release".into());
    }
    if version.cmp_precedence(running) != Ordering::Greater {
        return Ok(None);
    }
    let target = supported_target()?;
    let name = format!("voiceless-v{version}-{target}.tar.gz");
    let mut assets = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == name);
    let asset = assets.next().ok_or_else(|| {
        format!(
            "GitHub release {} has no CPU asset named {name}",
            release.tag_name
        )
    })?;
    if assets.next().is_some()
        || asset.state != "uploaded"
        || !(1..=MAX_ARCHIVE).contains(&asset.size)
    {
        return Err(format!(
            "GitHub asset {name} has an invalid upload state, size or duplicate name"
        ));
    }
    let url = format!(
        "https://github.com/{REPOSITORY}/releases/download/{}/{name}",
        release.tag_name
    );
    if asset.browser_download_url != url && asset.browser_download_url != url.replace('+', "%2B") {
        return Err(format!(
            "GitHub asset {name} has an unexpected download URL"
        ));
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| valid_digest(digest))
        .ok_or_else(|| format!("GitHub asset {name} provides no valid SHA-256 digest"))?;
    let response = download(&agent, &asset.browser_download_url)?;
    if let Some(length) = response.header("Content-Length") {
        if length.parse::<u64>().ok() != Some(asset.size) {
            return Err(format!(
                "download size for {name} does not match GitHub release metadata"
            ));
        }
    }
    stage_candidate(
        paths,
        response.into_reader(),
        Some(asset.size),
        digest,
        Some(&version),
        verify,
    )
    .map(Some)
}

fn download(agent: &ureq::Agent, url: &str) -> Result<ureq::Response, String> {
    let mut url = url.to_owned();
    for _ in 0..=5 {
        let response = request(
            agent,
            &url,
            "application/octet-stream",
            Duration::from_secs(600),
        )?;
        match response.status() {
            200 => return Ok(response),
            301 | 302 | 303 | 307 | 308 => {
                let redirect = response
                    .header("Location")
                    .ok_or("GitHub asset redirect has no location")?;
                let host = redirect
                    .strip_prefix("https://")
                    .and_then(|rest| rest.split_once('/'))
                    .map(|(host, _)| host);
                if !matches!(
                    host,
                    Some(
                        "github.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                            | "github-releases.githubusercontent.com"
                    )
                ) {
                    return Err("GitHub asset redirected outside its HTTPS download hosts".into());
                }
                url = redirect.to_owned();
            }
            status => return Err(format!("GitHub asset download returned HTTP {status}")),
        }
    }
    Err("GitHub asset download exceeded its redirect limit".into())
}

fn request(
    agent: &ureq::Agent,
    url: &str,
    accept: &str,
    timeout: Duration,
) -> Result<ureq::Response, String> {
    agent.get(url).timeout(timeout).set("Accept", accept).set("Accept-Encoding", "identity").call().map_err(|error| {
        match error {
            ureq::Error::Status(status, response) => {
                if status == 403 || status == 429 {
                    if let Some(seconds) = response.header("Retry-After").and_then(|value| value.parse::<u64>().ok()) {
                        return format!("GitHub returned HTTP {status}; retry after {seconds} seconds");
                    }
                    if response.header("X-RateLimit-Remaining") == Some("0") {
                        return format!("GitHub returned HTTP {status}; its unauthenticated API rate limit is exhausted");
                    }
                }
                format!("GitHub returned HTTP {status}")
            }
            ureq::Error::Transport(error) => format!("GitHub request failed: {error}"),
        }
    })
}

fn check_path_conflicts(command: &Path, search_path: Option<&OsStr>) -> Result<(), String> {
    let Some(search_path) = search_path else {
        return Ok(());
    };
    let command_directory = parent(command);
    let canonical_directory = command_directory.canonicalize().ok();
    let current_dir = env::current_dir().map_err(|error| format!("checking PATH: {error}"))?;
    for directory in env::split_paths(search_path) {
        let directory = current_dir.join(directory);
        if directory == command_directory
            || canonical_directory
                .as_ref()
                .is_some_and(|path| directory.canonicalize().ok().as_ref() == Some(path))
        {
            break;
        }
        let other = directory.join("voiceless");
        match fs::metadata(&other) {
            Ok(meta) if meta.is_file() && meta.mode() & 0o111 != 0 => {
                return Err(format!(
                    "{} would take precedence over {} on PATH",
                    other.display(),
                    command.display()
                ));
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::NotADirectory
                        | io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => return Err(format!("checking {}: {error}", other.display())),
        }
    }
    Ok(())
}

fn stable_version(value: &str) -> Result<Version, String> {
    let version = Version::parse(value)
        .map_err(|error| format!("invalid release version {value:?}: {error}"))?;
    if !version.pre.is_empty() || version.to_string() != value || value.len() > 96 {
        return Err(format!("invalid stable release version {value:?}"));
    }
    Ok(version)
}

fn supported_target() -> Result<String, String> {
    let target = super::target();
    if matches!(target.as_str(), "linux-x86_64-gnu" | "linux-aarch64-gnu") {
        Ok(target)
    } else {
        Err(format!("standalone releases are unavailable for {target}"))
    }
}

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

fn lock(directory: &Path) -> Result<File, String> {
    let path = directory.join(".lock");
    let file = at(
        &path,
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path),
    )?;
    check_regular_file(&path, &at(&path, file.metadata())?)?;
    loop {
        // The descriptor remains live through the update and closes on drop.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("locking {}: {error}", path.display()));
        }
    }
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
        return Err(format!("{} is not a standalone directory", path.display()));
    }
    check_owner_and_mode(path, &meta, private)
}

fn owned_link(path: &Path) -> Result<(), String> {
    let meta = at(path, fs::symlink_metadata(path))?;
    if !meta.is_symlink() || meta.uid() != effective_uid() {
        return Err(format!(
            "{} is not a user-owned standalone link",
            path.display()
        ));
    }
    Ok(())
}

fn check_regular_file(path: &Path, meta: &fs::Metadata) -> Result<(), String> {
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular standalone file",
            path.display()
        ));
    }
    check_owner_and_mode(path, meta, false)
}

fn owned_file(path: &Path) -> Result<(), String> {
    check_regular_file(path, &at(path, fs::symlink_metadata(path))?)
}

fn open_read(path: &Path) -> Result<File, String> {
    let file = at(
        path,
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path),
    )?;
    check_regular_file(path, &at(path, file.metadata())?)?;
    Ok(file)
}

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

fn parent(path: &Path) -> &Path {
    path.parent().expect("standalone path has a parent")
}

fn bounded_bytes(source: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    source.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "size limit exceeded",
        ));
    }
    Ok(bytes)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    at(path, bounded_bytes(open_read(path)?, limit))
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8], label: &str) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|error| format!("invalid {label}: {error}"))
}

fn json_from_path<T: DeserializeOwned>(path: &Path, limit: u64) -> Result<T, String> {
    parse_json(&read_bounded(path, limit)?, &path.display().to_string())
}

fn at<T>(path: &Path, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("{}: {error}", path.display()))
}

fn private_tempdir(root: &Path, prefix: &str) -> Result<TempDir, String> {
    at(
        root,
        Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(root),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};

    fn paths(scratch: &TempDir) -> InstallPaths {
        let home = scratch.path();
        InstallPaths {
            root: home.join("data/voiceless/releases"),
            command: home.join(".local/bin/voiceless"),
        }
    }

    fn verify_stub(_: &Path, _: &Version) -> Result<(), String> {
        Ok(())
    }

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn archive(version: &str, declared_library: &[u8], library: &[u8]) -> Vec<u8> {
        let target = supported_target().unwrap();
        let mut binary = [0; 20];
        binary[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        binary[16] = 3;
        binary[18] = if target == "linux-x86_64-gnu" {
            62
        } else {
            183
        };
        let payload = [
            (COMMAND, binary.as_slice()),
            ("lib/libfixture.so", library),
            ("LICENSE", b"license".as_slice()),
            (NOTICES, b"third-party notices".as_slice()),
        ];
        let mut files = payload
            .iter()
            .map(|&(name, bytes)| (name, digest(bytes)))
            .collect::<BTreeMap<_, _>>();
        files.insert("lib/libfixture.so", digest(declared_library));
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "version": version, "target": target, "variant": "cpu", "files": files
        }))
        .unwrap();

        let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (name, bytes) in payload.into_iter().chain([(MANIFEST, manifest.as_slice())]) {
            let mut header = tar::Header::new_ustar();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            archive.append_data(&mut header, name, bytes).unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap()
    }

    fn install_fixture(paths: &InstallPaths, bytes: &[u8]) {
        install_at(paths, bytes, &digest(bytes), None, verify_stub).unwrap();
    }

    fn offline_rollback(paths: &InstallPaths, version: Version) -> Result<(), String> {
        let executable = paths.command.canonicalize().unwrap();
        update_at(paths, &executable, &version, true, verify_stub, |_, _| {
            panic!("offline rollback attempted to fetch a release")
        })
    }

    #[test]
    fn rejects_archive_and_payload_corruption_without_switching() {
        let scratch = tempfile::tempdir().unwrap();
        let paths = paths(&scratch);
        let initial = archive("1.0.0", b"first library", b"first library");
        install_fixture(&paths, &initial);
        let active = paths.command.canonicalize().unwrap();

        let bad = archive("1.1.0", b"expected library", b"corrupt library");
        let incorrect_sha =
            install_at(&paths, bad.as_slice(), &digest(&initial), None, verify_stub).unwrap_err();
        assert!(incorrect_sha.contains("archive SHA-256 mismatch"));
        let incorrect_library =
            install_at(&paths, bad.as_slice(), &digest(&bad), None, verify_stub).unwrap_err();
        assert!(incorrect_library.contains("release file SHA-256 mismatch for lib/libfixture.so"));

        let good = archive("1.1.0", b"next library", b"next library");
        let mut trailing_bytes = good.clone();
        trailing_bytes.extend_from_slice(b"trailing bytes");
        let error = install_at(
            &paths,
            trailing_bytes.as_slice(),
            &digest(&trailing_bytes),
            None,
            verify_stub,
        )
        .unwrap_err();
        assert!(error.contains("unexpected data after release gzip stream"));
        assert_eq!(paths.command.canonicalize().unwrap(), active);
        assert!(read_activation(&paths).unwrap().previous.is_none());
    }

    #[test]
    fn rolls_back_offline_only_to_an_intact_payload() {
        let scratch = tempfile::tempdir().unwrap();
        let paths = paths(&scratch);
        let initial = archive("1.0.0", b"first library", b"first library");
        install_fixture(&paths, &initial);
        let paths = paths.canonical().unwrap();
        let first = paths.command.canonicalize().unwrap();
        let initial_version = Version::new(1, 0, 0);
        assert!(offline_rollback(&paths, initial_version.clone())
            .unwrap_err()
            .contains("no previous"));

        let next_version = Version::new(1, 1, 0);
        let next = archive("1.1.0", b"next library", b"next library");
        update_at(
            &paths,
            &first,
            &initial_version,
            false,
            verify_stub,
            |paths, running| {
                assert_eq!(running, &initial_version);
                stage_candidate(
                    paths,
                    next.as_slice(),
                    Some(next.len() as u64),
                    &digest(&next),
                    Some(&next_version),
                    verify_stub,
                )
                .map(Some)
            },
        )
        .unwrap();
        let second = paths.command.canonicalize().unwrap();
        assert_ne!(first, second);

        let rollback_library = parent(parent(&first)).join("lib/libfixture.so");
        fs::write(&rollback_library, b"damaged").unwrap();
        let error = offline_rollback(&paths, next_version.clone()).unwrap_err();
        assert!(error.contains("release file SHA-256 mismatch for lib/libfixture.so"));
        assert_eq!(paths.command.canonicalize().unwrap(), second);

        fs::write(&rollback_library, b"first library").unwrap();
        offline_rollback(&paths, next_version).unwrap();
        assert_eq!(paths.command.canonicalize().unwrap(), first);
        assert_eq!(
            read_activation(&paths).unwrap().previous.unwrap().version,
            "1.1.0"
        );
        assert!(second.is_file());
    }

    #[test]
    fn recovers_from_an_archived_previous_release() {
        let scratch = tempfile::tempdir().unwrap();
        let paths = paths(&scratch);
        let initial = archive("1.0.0", b"first library", b"first library");
        install_fixture(&paths, &initial);
        let first = paths.command.canonicalize().unwrap();

        let next = archive("1.1.0", b"next library", b"next library");
        install_fixture(&paths, &next);
        let second = paths.command.canonicalize().unwrap();
        fs::remove_file(&second).unwrap();
        assert!(paths.command.canonicalize().is_err());

        let older = archive("0.9.0", b"older library", b"older library");
        assert!(install_at(&paths, older.as_slice(), &digest(&older), None, verify_stub).is_err());
        install_fixture(&paths, &initial);
        assert_eq!(paths.command.canonicalize().unwrap(), first);
    }

    #[test]
    fn distinguishes_foreign_executable_from_malformed_install() {
        let scratch = tempfile::tempdir().unwrap();
        let paths = paths(&scratch);
        let version = Version::new(1, 0, 0);
        let foreign = scratch.path().join("unmanaged/bin/voiceless");
        assert!(!owns_at(&paths, &foreign, &version).unwrap());

        let initial = archive("1.0.0", b"first library", b"first library");
        install_fixture(&paths, &initial);
        let active = paths.command.canonicalize().unwrap();
        assert!(owns_at(&paths, &active, &version).unwrap());
        fs::remove_file(&paths.root.join("current")).unwrap();
        assert!(owns_at(&paths, &active, &version).is_err());
        assert!(!owns_at(&paths, &foreign, &version).unwrap());
    }
}
