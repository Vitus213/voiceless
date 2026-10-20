//! Model catalog, integrity-checked install/check/remove and versioned
//! pointer layout compatible with the Python manager:
//! `<data>/models/versions/<id>/<version>-<digest12>` payloads and
//! `<data>/models/<id>/current` symlinks.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MANIFEST_NAME: &str = "manifest.json";

#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: String,
    pub version: String,
    pub url: String,
    pub sha256_sri: String,
    pub size_bytes: u64,
    pub archive_type: ArchiveType,
    pub top_level_directory: Option<String>,
    pub allowed_members: Vec<String>,
    pub required_paths: Vec<String>,
    pub license_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveType {
    TarBz2,
    File,
}

/// Streaming Paraformer bilingual zh/en catalog entry pinned from the Python
/// catalog: URL, byte count, SRI, allowed/required members and license note.
pub fn streaming_paraformer_spec() -> ModelSpec {
    ModelSpec {
        id: "streaming-paraformer-bilingual-zh-en".to_owned(),
        version: "asr-models-2024-03-10".to_owned(),
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-paraformer-bilingual-zh-en.tar.bz2".to_owned(),
        sha256_sri: "sha256-VGKh/OQmk96uVyrx6MRocSSxKqhf5h/00xaLtSgOIF8=".to_owned(),
        size_bytes: 1_047_319_737,
        archive_type: ArchiveType::TarBz2,
        top_level_directory: Some("sherpa-onnx-streaming-paraformer-bilingual-zh-en".to_owned()),
        allowed_members: [
            "README.md",
            "decoder.int8.onnx",
            "decoder.onnx",
            "encoder.int8.onnx",
            "encoder.onnx",
            "test_wavs/0.wav",
            "test_wavs/1.wav",
            "test_wavs/2.wav",
            "test_wavs/3.wav",
            "test_wavs/8k.wav",
            "tokens.txt",
        ]
        .iter()
        .map(|member| (*member).to_owned())
        .collect(),
        required_paths: ["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]
            .iter()
            .map(|member| (*member).to_owned())
            .collect(),
        license_status: "上游模型页面未在归档内提供独立许可证文本；使用前需用户自行核验许可。".to_owned(),
    }
}

pub fn catalog() -> Vec<ModelSpec> {
    vec![streaming_paraformer_spec()]
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("unknown model id: {0}")]
    UnknownId(String),
    #[error("model {0} is not installed")]
    NotInstalled(String),
    #[error("model {0}: {1}")]
    Integrity(String, String),
    #[error("model download failed: {0}")]
    Download(String),
    #[error("model archive invalid: {0}")]
    Archive(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Download boundary; production uses HTTPS, tests inject fixtures.
pub trait Downloader {
    fn download(&self, spec: &ModelSpec, destination: &Path) -> Result<(), ModelError>;
}

pub struct HttpDownloader;

impl Downloader for HttpDownloader {
    fn download(&self, spec: &ModelSpec, destination: &Path) -> Result<(), ModelError> {
        if !spec.url.starts_with("https://") {
            return Err(ModelError::Download(format!(
                "model source must use HTTPS: {}",
                spec.url
            )));
        }
        let response = ureq::get(&spec.url)
            .timeout(std::time::Duration::from_secs(600))
            .call()
            .map_err(|error| ModelError::Download(error.to_string()))?;
        let mut file = fs::File::create(destination)?;
        let mut reader = response.into_reader();
        io::copy(&mut reader, &mut file)
            .map_err(|error| ModelError::Download(error.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckReport {
    pub missing: Vec<String>,
    pub extra: Vec<String>,
    pub corrupt: Vec<String>,
    pub errors: Vec<String>,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
            && self.extra.is_empty()
            && self.corrupt.is_empty()
            && self.errors.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    id: String,
    version: String,
    source_url: String,
    source_sri: String,
    source_sha256: String,
    files: BTreeMap<String, ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestFile {
    sha256: String,
    size: u64,
}

pub struct ModelManager {
    models_root: PathBuf,
    versions_root: PathBuf,
    downloads_root: PathBuf,
}

impl ModelManager {
    pub fn new(data_dir: &Path, cache_dir: &Path) -> Self {
        Self {
            models_root: data_dir.join("models"),
            versions_root: data_dir.join("models").join("versions"),
            downloads_root: cache_dir.join("model-downloads"),
        }
    }

    pub fn install<D: Downloader>(
        &self,
        spec: &ModelSpec,
        downloader: &D,
    ) -> Result<PathBuf, ModelError> {
        let digest_hex = decode_sri(&spec.sha256_sri)?;
        let version_name = format!("{}-{}", spec.version, &digest_hex[..12]);
        let model_versions = self.versions_root.join(&spec.id);
        fs::create_dir_all(&model_versions)?;
        let destination = model_versions.join(&version_name);

        if destination.exists() {
            let report = self.check_payload(spec, &destination)?;
            if report.ok() {
                self.activate(&spec.id, &destination)?;
                return Ok(destination);
            }
            fs::remove_dir_all(&destination)?;
        }

        fs::create_dir_all(&self.downloads_root)?;
        let partial = self
            .downloads_root
            .join(format!("{}-{}.partial", spec.id, spec.version));
        let staging = model_versions.join(format!(".staging-{}", unique_suffix()));
        let result = self.install_from_network(spec, downloader, &partial, &staging, &digest_hex);
        let _ = fs::remove_file(&partial);
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        let staged = result?;
        self.activate(&spec.id, &destination)?;
        Ok(staged)
    }

    fn install_from_network<D: Downloader>(
        &self,
        spec: &ModelSpec,
        downloader: &D,
        partial: &Path,
        staging: &Path,
        expected_digest_hex: &str,
    ) -> Result<PathBuf, ModelError> {
        if partial.exists() {
            let _ = fs::remove_file(partial);
        }
        downloader.download(spec, partial)?;
        let actual_digest = sha256_file(partial)?;
        if actual_digest != expected_digest_hex {
            return Err(ModelError::Integrity(
                spec.id.to_owned(),
                format!("SHA-256 mismatch: expected {expected_digest_hex}, got {actual_digest}"),
            ));
        }
        let metadata = fs::metadata(partial)?;
        if metadata.len() != spec.size_bytes {
            return Err(ModelError::Integrity(
                spec.id.to_owned(),
                format!(
                    "unexpected archive size: expected {} bytes, got {}",
                    spec.size_bytes,
                    metadata.len()
                ),
            ));
        }

        fs::create_dir_all(staging)?;
        match spec.archive_type {
            ArchiveType::TarBz2 => extract_tar_bz2(spec, partial, staging)?,
            ArchiveType::File => install_single_file(spec, partial, staging)?,
        }
        let files = hash_staged_files(staging)?;
        write_manifest(staging, spec, expected_digest_hex, &files)?;

        let version_name = staging.file_name().expect("staging name");
        let destination = staging.parent().unwrap().join(format!(
            "{}-{}",
            spec.version,
            &expected_digest_hex[..12]
        ));
        let _ = version_name;
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(staging, &destination)?;
        Ok(destination)
    }

    pub fn check(&self, spec: &ModelSpec) -> Result<(PathBuf, CheckReport), ModelError> {
        let payload = self
            .resolve(&spec.id)?
            .ok_or_else(|| ModelError::NotInstalled(spec.id.to_owned()))?;
        let report = self.check_payload(spec, &payload)?;
        Ok((payload, report))
    }

    /// Resolve the current payload for an installed model without caching;
    /// returns `None` when no pointer exists.
    pub fn resolve(&self, model_id: &str) -> Result<Option<PathBuf>, ModelError> {
        let pointer = self.models_root.join(model_id).join("current");
        if !pointer.exists() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&pointer)?;
        if !metadata.file_type().is_symlink() {
            return Err(ModelError::Integrity(
                model_id.to_owned(),
                "current pointer is not a symlink".to_owned(),
            ));
        }
        let target = fs::read_link(&pointer)?;
        let target_name = validate_pointer_target(model_id, &target)?;
        let payload = self.versions_root.join(model_id).join(target_name);
        let metadata = fs::symlink_metadata(&payload).map_err(|_| {
            ModelError::Integrity(
                model_id.to_owned(),
                "pointer target does not exist".to_owned(),
            )
        })?;
        if !metadata.is_dir() {
            return Err(ModelError::Integrity(
                model_id.to_owned(),
                "pointer target is not a directory".to_owned(),
            ));
        }
        Ok(Some(payload))
    }

    pub fn remove(&self, model_id: &str) -> Result<bool, ModelError> {
        let pointer_dir = self.models_root.join(model_id);
        if !pointer_dir.exists() && !self.versions_root.join(model_id).exists() {
            return Ok(false);
        }
        if pointer_dir.exists() {
            fs::remove_dir_all(&pointer_dir)?;
        }
        let versions = self.versions_root.join(model_id);
        if versions.exists() {
            fs::remove_dir_all(&versions)?;
        }
        Ok(true)
    }

    fn check_payload(&self, spec: &ModelSpec, payload: &Path) -> Result<CheckReport, ModelError> {
        let mut report = CheckReport::default();
        let digest_hex = decode_sri(&spec.sha256_sri)?;
        let expected_name = format!("{}-{}", spec.version, &digest_hex[..12]);
        if payload
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name != expected_name)
            .unwrap_or(true)
        {
            report
                .errors
                .push("payload directory name does not match the digest".to_owned());
        }

        let manifest_path = payload.join(MANIFEST_NAME);
        let manifest = match fs::read_to_string(&manifest_path) {
            Ok(raw) => match serde_json::from_str::<Manifest>(&raw) {
                Ok(manifest) => manifest,
                Err(error) => {
                    report
                        .errors
                        .push(format!("cannot parse manifest: {error}"));
                    return Ok(report);
                }
            },
            Err(_) => {
                report.missing.push(MANIFEST_NAME.to_owned());
                return Ok(report);
            }
        };

        for (relative, entry) in &manifest.files {
            let file_path = payload.join(relative);
            if !file_path.is_file() {
                report.missing.push(relative.clone());
                continue;
            }
            match sha256_file(&file_path) {
                Ok(digest) if digest == entry.sha256 => {
                    let size = fs::metadata(&file_path)
                        .map(|m| m.len())
                        .unwrap_or(u64::MAX);
                    if size != entry.size {
                        report.corrupt.push(relative.clone());
                    }
                }
                Ok(_) => report.corrupt.push(relative.clone()),
                Err(error) => report
                    .errors
                    .push(format!("cannot hash {relative}: {error}")),
            }
        }
        for required in &spec.required_paths {
            if !payload.join(required).is_file() && !report.missing.contains(required) {
                report.missing.push(required.clone());
            }
        }
        for entry in walk_relative(payload)? {
            if entry == MANIFEST_NAME {
                continue;
            }
            if !manifest.files.contains_key(&entry) {
                report.extra.push(entry.clone());
            }
            if !spec.allowed_members.contains(&entry) {
                report.extra.push(entry);
            }
        }
        report.extra.sort();
        report.extra.dedup();
        Ok(report)
    }

    fn activate(&self, model_id: &str, destination: &Path) -> Result<(), ModelError> {
        let pointer_dir = self.models_root.join(model_id);
        fs::create_dir_all(&pointer_dir)?;
        let pointer = pointer_dir.join("current");
        let target = PathBuf::from("..")
            .join("versions")
            .join(model_id)
            .join(destination.file_name().expect("destination name"));
        let temporary = pointer_dir.join(format!(".current-{}", unique_suffix()));
        std::os::unix::fs::symlink(&target, &temporary)?;
        fs::rename(&temporary, &pointer)?;
        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        Ok(())
    }
}

fn extract_tar_bz2(spec: &ModelSpec, archive: &Path, staging: &Path) -> Result<(), ModelError> {
    let file = fs::File::open(archive)?;
    let decoder = bzip2::read::BzDecoder::new(file);
    let mut entries = tar::Archive::new(decoder);
    let top_level = spec.top_level_directory.as_deref();
    for entry in entries
        .entries()
        .map_err(|error| ModelError::Archive(error.to_string()))?
    {
        let mut entry = entry.map_err(|error| ModelError::Archive(error.to_string()))?;
        let raw_path = entry
            .path()
            .map_err(|error| ModelError::Archive(error.to_string()))?
            .to_path_buf();
        let raw = raw_path.to_string_lossy();
        if raw.contains("..") {
            return Err(ModelError::Archive(format!(
                "archive member escapes the model directory: {raw}"
            )));
        }
        let relative = match top_level {
            Some(top) => raw
                .strip_prefix(top)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(raw.as_ref())
                .to_owned(),
            None => raw.into_owned(),
        };
        if relative.is_empty() {
            continue;
        }
        // Directory entries are implicit parents of allowed members; only
        // regular files are checked against the manifest allowlist.
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(staging.join(relative.trim_end_matches('/')))?;
            continue;
        }
        if !spec.allowed_members.contains(&relative) {
            return Err(ModelError::Archive(format!(
                "archive member {relative} is not in the allowed manifest"
            )));
        }
        let target = staging.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        entry
            .unpack(&target)
            .map_err(|error| ModelError::Archive(error.to_string()))?;
    }
    for required in &spec.required_paths {
        if !staging.join(required).is_file() {
            return Err(ModelError::Archive(format!(
                "archive is missing required member {required}"
            )));
        }
    }
    Ok(())
}

fn install_single_file(spec: &ModelSpec, archive: &Path, staging: &Path) -> Result<(), ModelError> {
    let name = spec
        .allowed_members
        .first()
        .ok_or_else(|| ModelError::Archive("file model has no member name".to_owned()))?;
    fs::copy(archive, staging.join(name))?;
    Ok(())
}

fn hash_staged_files(staging: &Path) -> Result<BTreeMap<String, ManifestFile>, ModelError> {
    let mut files = BTreeMap::new();
    for relative in walk_relative(staging)? {
        let path = staging.join(&relative);
        files.insert(
            relative,
            ManifestFile {
                sha256: sha256_file(&path)?,
                size: fs::metadata(&path)?.len(),
            },
        );
    }
    Ok(files)
}

fn write_manifest(
    staging: &Path,
    spec: &ModelSpec,
    digest_hex: &str,
    files: &BTreeMap<String, ManifestFile>,
) -> Result<(), ModelError> {
    let manifest = Manifest {
        id: spec.id.to_owned(),
        version: spec.version.to_owned(),
        source_url: spec.url.clone(),
        source_sri: spec.sha256_sri.clone(),
        source_sha256: digest_hex.to_owned(),
        files: (*files).clone(),
    };
    let mut file = fs::File::create(staging.join(MANIFEST_NAME))?;
    file.write_all(
        serde_json::to_string(&manifest)
            .map_err(|error| ModelError::Integrity(spec.id.clone(), error.to_string()))?
            .as_bytes(),
    )?;
    Ok(())
}

fn walk_relative(root: &Path) -> Result<Vec<String>, ModelError> {
    let mut collected = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walked under root")
                    .to_string_lossy()
                    .to_string();
                collected.push(relative);
            } else {
                return Err(ModelError::Integrity(
                    root.display().to_string(),
                    format!("unexpected non-file entry: {}", path.display()),
                ));
            }
        }
    }
    collected.sort();
    Ok(collected)
}

fn sha256_file(path: &Path) -> Result<String, ModelError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_sri(sri: &str) -> Result<String, ModelError> {
    let digest = sri
        .strip_prefix("sha256-")
        .ok_or_else(|| ModelError::Integrity(sri.to_owned(), "SRI must use sha256".to_owned()))?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(digest)
        .map_err(|error| ModelError::Integrity(sri.to_owned(), error.to_string()))?;
    if bytes.len() != 32 {
        return Err(ModelError::Integrity(
            sri.to_owned(),
            "SRI digest must be 32 bytes".to_owned(),
        ));
    }
    Ok(hex_digest(&bytes))
}

fn validate_pointer_target(model_id: &str, target: &Path) -> Result<String, ModelError> {
    let parts: Vec<String> = target
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    let expected_prefix = ["..", "versions", model_id];
    if parts.len() != 4
        || parts[0] != expected_prefix[0]
        || parts[1] != expected_prefix[1]
        || parts[2] != expected_prefix[2]
        || parts[3].contains('/')
        || parts[3] == "."
        || parts[3] == ".."
    {
        return Err(ModelError::Integrity(
            model_id.to_owned(),
            format!("pointer escapes the model directory: {}", target.display()),
        ));
    }
    Ok(parts[3].clone())
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}-{}", std::process::id())
}

pub fn default_data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("voiceless")
}

pub fn default_cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache").join("voiceless")
}

/// Retained WAV recordings from successful streaming sessions.
pub fn default_audio_dir() -> PathBuf {
    default_data_dir().join("audio")
}

pub fn default_config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("voiceless")
}

fn xdg_dir(variable: &str, fallback: &str) -> PathBuf {
    std::env::var(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned())).join(fallback)
        })
}
