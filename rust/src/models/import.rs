//! Importing a local GGUF model into the versioned store: the same
//! payload layout, `manifest.json` and `current` pointer as a catalog
//! model, under a content-addressed id. Callers: `model_cmd`
//! (`voiceless model import <file>`) and the console's
//! `POST /api/models/import`, both through the same methods.
//!
//! Contract, in three steps:
//! - `ModelManager::begin_import(bytes)` fails before anything is written
//!   when this build cannot run GGUF models (`Unsupported`) or the data
//!   filesystem has no room for `bytes` (`NoSpace`), then returns an empty
//!   file on that filesystem to stream the upload into.
//! - `ModelManager::finish_import(file, name, bytes)` accepts the file only
//!   when it has exactly `bytes` bytes, is a complete GGUF container
//!   (`gguf.rs`) and loads through the production CPU decoder, native
//!   session warm-up included (`gguf_asr::GgufDecoder`): a VAD, a
//!   language-ID or a text model, or a download cut short, is refused as
//!   `Invalid`.
//! - The accepted file is renamed into
//!   `versions/<id>/imported-<digest12>/` (one filesystem, no second copy)
//!   with a manifest that holds its SHA-256 and identity, and
//!   `<id>/current` is swapped by rename. Nothing existing is written to:
//!   the id derives from the content and cannot be a catalog id, so a
//!   pinned package is never replaced, and a running engine keeps its
//!   files until the new model is selected and loaded.
//!
//! Ids are `imported-<architecture>-<sha256 prefix>-gguf`. They end in
//! `-gguf`, so configuration, the engine and every list treat them like
//! any GGUF model, and the same bytes always import to the same id
//! (importing twice reuses the published payload).
//!
//! Interrupted imports clean up after themselves. The staged file is
//! deleted with its handle, so an upload that stops early leaves nothing.
//! A payload is published by one rename, so a process killed before it
//! leaves at most a `.staging-*` directory or a staged file nobody holds;
//! the next install or import deletes both (`sweep_abandoned`,
//! `ModelManager::sweep_staging`).

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use super::gguf::{self, GgufHeader};
use super::{
    encode_sri, ArchiveType, Manifest, ManifestFile, ModelError, ModelManager, ModelSpec,
    GGUF_MODEL_FILE, MANIFEST_NAME,
};

/// Imported ids start with this, so none can be a catalog id.
const IMPORTED_PREFIX: &str = "imported-";
/// Release label of an imported model; its payload directory is
/// `imported-<digest12>`.
const IMPORTED_VERSION: &str = "imported";
/// A staged upload, `<versions>/.import-<random>.partial`.
const STAGED_PREFIX: &str = ".import-";
const STAGED_SUFFIX: &str = ".partial";
/// Room the published payload needs besides the model: `manifest.json`
/// and the directory entries.
const PUBLISH_OVERHEAD: u64 = 64 * 1024;
/// Longest display name that is kept.
const MAX_NAME_CHARS: usize = 120;
/// Longest architecture fragment of an id.
const MAX_FAMILY_CHARS: usize = 24;
const IMPORT_NEEDS_ENGINE: &str = "this build has no transcribe.cpp engine; import models with a GGUF-enabled package such as voiceless-vulkan";
const IMPORTED_LICENSE: &str = "Imported from a local file; Voiceless did not verify its license.";

/// A model imported from a local GGUF file, as `model list`, `model check`
/// and the console show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedModel {
    /// The imported model as every store operation takes it: `check`,
    /// `resolve`, `installed_model_dir`. `url` is empty (there is no
    /// download source); `sha256_sri` and `size_bytes` are those of the
    /// imported file.
    pub spec: ModelSpec,
    /// File name the user gave, else the name inside the GGUF file.
    pub name: String,
    /// Languages the model recognizes, as the engine reported them after
    /// the load (else those of its family).
    pub languages: Vec<String>,
    /// `general.architecture` of the file (`qwen3_asr`).
    pub family: String,
}

/// Identity of an imported model, kept in its `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ImportedManifest {
    name: String,
    family: String,
    languages: Vec<String>,
    /// `stt.variant`; empty when the file has none.
    variant: String,
    /// Audio rate the model is made for, in Hz.
    native_sample_rate: i32,
    /// Longest audio the engine takes from this model, in ms (0: no limit).
    max_audio_ms: i64,
}

/// Whether this build can run GGUF models, and so verify an import.
pub fn gguf_import_supported() -> bool {
    cfg!(feature = "transcribe")
}

/// The file name a client names for an import, from the value of the
/// `X-Voiceless-Filename` header: percent-encoded UTF-8, of which only the
/// final path component is kept. `None` when the value does not decode,
/// names no file or is longer than 120 characters; the import then takes
/// the name inside the GGUF file.
pub fn import_filename(header: &str) -> Option<String> {
    let raw = header.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            let digits = raw.get(index + 1..index + 3)?;
            let high = (digits[0] as char).to_digit(16)?;
            let low = (digits[1] as char).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(bytes).ok()?;
    match decoded.rsplit(['/', '\\']).next().unwrap_or_default() {
        "" | "." | ".." => None,
        basename => display_name(basename),
    }
}

impl ModelManager {
    /// Start an import of a file of `bytes` bytes: the empty file it
    /// returns, on the data filesystem, takes the upload and is passed to
    /// `finish_import`; dropped without finishing, it is deleted. Fails
    /// before anything is written with `Unsupported` when this build
    /// cannot run GGUF models, `Invalid` for an empty file and `NoSpace`
    /// when the data filesystem cannot hold the file (after leftovers of
    /// interrupted installs were removed). Blocking only for the space
    /// probe.
    pub fn begin_import(&self, bytes: u64) -> Result<NamedTempFile, ModelError> {
        if !gguf_import_supported() {
            return Err(ModelError::Unsupported(IMPORT_NEEDS_ENGINE.to_owned()));
        }
        if bytes == 0 {
            return Err(ModelError::Invalid("the file is empty".to_owned()));
        }
        fs::create_dir_all(&self.versions_root)?;
        self.sweep_abandoned();
        self.sweep_staging(None);
        ensure_space(bytes, available_space(&self.versions_root)?)?;
        let file = tempfile::Builder::new()
            .prefix(STAGED_PREFIX)
            .suffix(STAGED_SUFFIX)
            .rand_bytes(8)
            .tempfile_in(&self.versions_root)?;
        // Held until the file is published or dropped: it is what tells a
        // later import that this upload is still running.
        lock_exclusive(file.as_file())?;
        Ok(file)
    }

    /// Verify the staged `file` and publish it as an imported model.
    /// `bytes` is the size the upload announced and `name` the file name
    /// the user gave (`import_filename`; empty when there is none).
    ///
    /// Errors, none of which leave anything behind: `Incomplete` when the
    /// file is not exactly `bytes` bytes, `Invalid` when it is not a
    /// complete GGUF file or the engine cannot load it as a speech model,
    /// `Busy` while another operation imports the same model. Blocking for
    /// the hash of the file and its load on the CPU (seconds to a minute
    /// for a multi-gigabyte model); the console runs it on
    /// `spawn_blocking`.
    pub fn finish_import(
        &self,
        file: NamedTempFile,
        name: &str,
        bytes: u64,
    ) -> Result<ImportedModel, ModelError> {
        if !gguf_import_supported() {
            return Err(ModelError::Unsupported(IMPORT_NEEDS_ENGINE.to_owned()));
        }
        let actual = file.as_file().metadata()?.len();
        if actual != bytes {
            return Err(ModelError::Incomplete {
                expected: bytes,
                actual,
            });
        }
        let digest_hex = super::sha256_file(file.path())?;
        let header = gguf::inspect(file.path())?;
        let spec = imported_spec(&header.architecture, &digest_hex, bytes);
        let model_versions = self.versions_root.join(&spec.id);
        let destination = model_versions.join(format!("{IMPORTED_VERSION}-{}", &digest_hex[..12]));

        // One importer at a time owns this model, whichever process it
        // runs in, from the check for an earlier import to the pointer.
        let _install = self.lock_mutation(&spec.id)?;
        if destination.exists() {
            let previous = match read_manifest(&destination) {
                Ok(manifest) if manifest.source_sha256 != digest_hex => {
                    return Err(ModelError::Integrity(
                        spec.id,
                        "an installed model already uses this digest prefix".to_owned(),
                    ));
                }
                Ok(manifest) => recorded(&spec.id, &manifest, &digest_hex).ok().filter(|_| {
                    self.check_payload(&spec, &destination)
                        .is_ok_and(|report| report.ok())
                }),
                Err(_) => None,
            };
            if let Some(model) = previous {
                self.activate(&spec.id, &destination)?;
                return Ok(model);
            }
        }

        let identity = verified_identity(file.path(), name, &header)?;
        let model = ImportedModel {
            name: identity.name.clone(),
            languages: identity.languages.clone(),
            family: identity.family.clone(),
            spec: spec.clone(),
        };
        fs::create_dir_all(&model_versions)?;
        self.sweep_staging(Some(&model_versions));
        let staging = model_versions.join(format!(".staging-{}", super::unique_suffix()));
        let published =
            self.publish_import(&staging, &destination, file, &spec, &digest_hex, &identity);
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        published?;
        self.activate(&spec.id, &destination)?;
        Ok(model)
    }

    /// Import the file that `source` reads, of `bytes` bytes and called
    /// `name`: `begin_import`, a streamed copy, then `finish_import`. The
    /// copy stops at `bytes`; a source with fewer bytes is `Incomplete`
    /// and one with more `Integrity`, and a read error is `Io`. This is
    /// the path of `voiceless model import`. Blocking.
    pub fn import_from<R: Read>(
        &self,
        source: &mut R,
        name: &str,
        bytes: u64,
    ) -> Result<ImportedModel, ModelError> {
        let mut file = self.begin_import(bytes)?;
        // Only the announced size is copied; a longer source is caught below.
        let copied = {
            let mut announced = (&mut *source).take(bytes);
            io::copy(&mut announced, file.as_file_mut())
        };
        match copied {
            Ok(copied) if copied == bytes => {}
            Ok(actual) => {
                return Err(ModelError::Incomplete {
                    expected: bytes,
                    actual,
                })
            }
            Err(error) => return Err(error.into()),
        }
        // The size was taken before the copy: a file that grew since is
        // not the file that was announced.
        let mut extra = [0_u8; 1];
        match source.read(&mut extra) {
            Ok(0) => {}
            Ok(_) => {
                return Err(ModelError::Integrity(
                    name.to_owned(),
                    format!("the file grew during the import past {bytes} bytes"),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
        self.finish_import(file, name, bytes)
    }

    /// The imported models of this store that are installed (an intact
    /// `current` pointer), by id. Reads the manifests only; nothing is
    /// hashed. A model whose pointer or manifest is damaged is left out;
    /// `remove` still deletes it by id.
    pub fn list_imported(&self) -> Result<Vec<ImportedModel>, ModelError> {
        let Ok(entries) = fs::read_dir(&self.versions_root) else {
            return Ok(Vec::new());
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|id| id.starts_with(IMPORTED_PREFIX) && id.ends_with("-gguf"))
            .collect();
        ids.sort();
        let mut models = Vec::new();
        for id in ids {
            let Ok(Some(payload)) = self.resolve(&id) else {
                continue;
            };
            let Ok(manifest) = read_manifest(&payload) else {
                continue;
            };
            if let Ok(model) = recorded(&id, &manifest, &manifest.source_sha256) {
                models.push(model);
            }
        }
        Ok(models)
    }

    /// Delete staged uploads whose importer is gone. A staged file is
    /// locked by its importer from `begin_import` until it is published or
    /// dropped, so one that can be locked now belongs to an import that
    /// was interrupted.
    fn sweep_abandoned(&self) {
        let Ok(entries) = fs::read_dir(&self.versions_root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !(name.starts_with(STAGED_PREFIX) && name.ends_with(STAGED_SUFFIX)) {
                continue;
            }
            let Ok(staged) = File::open(entry.path()) else {
                continue;
            };
            if staged.try_lock().is_ok() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Move the staged file into a staging directory with its manifest and
    /// rename that to the payload directory. Nothing is visible under the
    /// payload name before the rename.
    fn publish_import(
        &self,
        staging: &Path,
        destination: &Path,
        file: NamedTempFile,
        spec: &ModelSpec,
        digest_hex: &str,
        identity: &ImportedManifest,
    ) -> Result<(), ModelError> {
        fs::create_dir_all(staging)?;
        let payload_file = staging.join(GGUF_MODEL_FILE);
        file.persist(&payload_file)
            .map_err(|failed| ModelError::Io(failed.error))?;
        // `tempfile` creates the staged file private to its owner; the
        // payload is readable like every other.
        #[cfg(unix)]
        fs::set_permissions(&payload_file, fs::Permissions::from_mode(0o644))?;
        let files = BTreeMap::from([(
            GGUF_MODEL_FILE.to_owned(),
            ManifestFile {
                sha256: digest_hex.to_owned(),
                size: spec.size_bytes,
            },
        )]);
        super::write_manifest(staging, spec, digest_hex, &files, Some(identity))?;
        self.publish_payload(&spec.id, staging, destination)
    }
}

/// `imported-<architecture>-<first 12 hex of the digest>-gguf`: addressed
/// by the content, and a GGUF id to everything that reads one.
fn imported_id(architecture: &str, digest_hex: &str) -> String {
    format!(
        "{IMPORTED_PREFIX}{}-{}-gguf",
        slug(architecture),
        &digest_hex[..12]
    )
}

/// The spec of an imported file: a single-file model with no download
/// source, addressed by its content.
fn imported_spec(architecture: &str, digest_hex: &str, bytes: u64) -> ModelSpec {
    ModelSpec {
        id: imported_id(architecture, digest_hex),
        version: IMPORTED_VERSION.to_owned(),
        url: String::new(),
        sha256_sri: encode_sri(digest_hex),
        size_bytes: bytes,
        archive_type: ArchiveType::File,
        top_level_directory: None,
        allowed_members: vec![GGUF_MODEL_FILE.to_owned()],
        required_paths: vec![GGUF_MODEL_FILE.to_owned()],
        license_status: IMPORTED_LICENSE.to_owned(),
    }
}

/// The architecture as a lowercase id fragment.
fn slug(architecture: &str) -> String {
    let mut slug = String::new();
    for character in architecture.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let cut = slug
        .char_indices()
        .nth(MAX_FAMILY_CHARS)
        .map_or(slug.len(), |(index, _)| index);
    slug.truncate(cut);
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "model".to_owned()
    } else {
        slug
    }
}

/// The `manifest.json` of a payload directory.
fn read_manifest(payload: &Path) -> Result<Manifest, ModelError> {
    let raw = fs::read_to_string(payload.join(MANIFEST_NAME))?;
    serde_json::from_str(&raw).map_err(|error| {
        ModelError::Integrity(
            payload.display().to_string(),
            format!("damaged manifest: {error}"),
        )
    })
}

/// The imported model `manifest` records, when it is the import of the
/// bytes hashed to `digest_hex`.
fn recorded(id: &str, manifest: &Manifest, digest_hex: &str) -> Result<ImportedModel, ModelError> {
    let identity = manifest.imported.clone().ok_or_else(|| {
        ModelError::Integrity(id.to_owned(), "the payload was not imported".to_owned())
    })?;
    if manifest.source_sha256 != digest_hex {
        return Err(ModelError::Integrity(
            id.to_owned(),
            "a different file is imported under this digest prefix".to_owned(),
        ));
    }
    Ok(ImportedModel {
        spec: ModelSpec {
            id: id.to_owned(),
            version: manifest.version.clone(),
            url: String::new(),
            sha256_sri: manifest.source_sri.clone(),
            size_bytes: manifest
                .files
                .get(GGUF_MODEL_FILE)
                .map_or(0, |file| file.size),
            archive_type: ArchiveType::File,
            top_level_directory: None,
            allowed_members: vec![GGUF_MODEL_FILE.to_owned()],
            required_paths: vec![GGUF_MODEL_FILE.to_owned()],
            license_status: IMPORTED_LICENSE.to_owned(),
        },
        name: identity.name,
        languages: identity.languages,
        family: identity.family,
    })
}

/// The identity of the staged file, once the production decoder has
/// loaded it. The name is the one the user gave, else the one inside the
/// file, else the architecture.
#[cfg(feature = "transcribe")]
fn verified_identity(
    path: &Path,
    name: &str,
    header: &GgufHeader,
) -> Result<ImportedManifest, ModelError> {
    use super::info::known_languages;
    use crate::offline_asr::{ComputeDevice, OfflineOptions};

    // The production decoder on the CPU, which every build has. Its load
    // opens the native session and runs one warm-up decode, and it refuses
    // a model that does not serve speech recognition.
    let options = OfflineOptions {
        provider: ComputeDevice::Cpu.as_str().to_owned(),
        num_threads: 0,
        hotwords: Vec::new(),
    };
    let decoder = crate::gguf_asr::GgufDecoder::load(path, ComputeDevice::Cpu, &options).map_err(
        |error| {
            let known = if name.is_empty() { "the file" } else { name };
            ModelError::Invalid(format!(
                "the engine cannot use it: {}",
                error
                    .to_string()
                    .replace(&path.display().to_string(), known)
            ))
        },
    )?;
    let loaded = decoder.identity().clone();
    Ok(ImportedManifest {
        name: display_name(name)
            .or_else(|| display_name(&header.name))
            .unwrap_or_else(|| header.architecture.clone()),
        family: header.architecture.clone(),
        languages: if loaded.languages.is_empty() {
            known_languages(&header.architecture)
                .iter()
                .map(|language| (*language).to_owned())
                .collect()
        } else {
            loaded.languages
        },
        variant: loaded.variant,
        native_sample_rate: loaded.native_sample_rate,
        max_audio_ms: loaded.max_audio_ms,
    })
}

#[cfg(not(feature = "transcribe"))]
fn verified_identity(
    _path: &Path,
    _name: &str,
    _header: &GgufHeader,
) -> Result<ImportedManifest, ModelError> {
    // Only `finish_import` calls this, and it refuses such a build first.
    Err(ModelError::Unsupported(IMPORT_NEEDS_ENGINE.to_owned()))
}

/// `name` as one line of printable characters of at most `MAX_NAME_CHARS`;
/// `None` when nothing is left or it is longer.
fn display_name(name: &str) -> Option<String> {
    let mut clean = String::new();
    let mut length = 0;
    for character in name.trim().chars() {
        if character.is_whitespace() {
            if clean.is_empty() {
                continue;
            }
            clean.push(' ');
        } else if !character.is_control() {
            clean.push(character);
        } else {
            continue;
        }
        length += 1;
        if length > MAX_NAME_CHARS {
            return None;
        }
    }
    clean.truncate(clean.trim_end().len());
    (!clean.is_empty()).then_some(clean)
}

/// Bytes a regular user can still write to the filesystem of `directory`.
// `c_ulong` is `u64` on 64-bit targets and narrower on 32-bit ones: widen for both.
#[cfg(unix)]
#[allow(clippy::useless_conversion)]
pub(crate) fn available_space(directory: &Path) -> io::Result<u64> {
    let path = std::ffi::CString::new(directory.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "store path holds a NUL byte"))?;
    let mut status = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `statvfs` reads the path as a C string, which `CString` is,
    // and writes exactly one `statvfs` behind the pointer it is given.
    if unsafe { libc::statvfs(path.as_ptr(), status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: initialized by the successful call above.
    let status = unsafe { status.assume_init() };
    Ok(u64::from(status.f_bavail) * u64::from(status.f_frsize))
}

#[cfg(windows)]
pub(crate) fn available_space(directory: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut path: Vec<u16> = directory.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "store path holds a NUL code unit",
        ));
    }
    path.push(0);
    let mut available = 0_u64;
    // SAFETY: the path is a NUL-terminated UTF-16 string and `available`
    // holds the one output value we ask for.
    let result = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

/// `NoSpace` unless `bytes` and the payload around them fit in `available`.
fn ensure_space(bytes: u64, available: u64) -> Result<(), ModelError> {
    let required = bytes.saturating_add(PUBLISH_OVERHEAD);
    if available < required {
        return Err(ModelError::NoSpace {
            required,
            available,
        });
    }
    Ok(())
}

/// Take the exclusive lock of a file this process just created.
fn lock_exclusive(file: &File) -> io::Result<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the staged file is already locked",
        )),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn store() -> (tempfile::TempDir, ModelManager) {
        let root = tempfile::tempdir().expect("temporary root");
        let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
        (root, manager)
    }

    fn write(path: &Path, bytes: &[u8]) -> PathBuf {
        fs::write(path, bytes).expect("write file");
        path.to_path_buf()
    }

    fn entries(directory: &Path) -> Vec<String> {
        let Ok(read) = fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut names: Vec<String> = read
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The id names the family, ends in `-gguf` and follows the content:
    /// the same bytes give the same id, other bytes another.
    #[test]
    fn imported_ids_are_content_addressed_and_end_in_gguf() {
        let id = imported_id("qwen3_asr", DIGEST);
        assert_eq!(id, "imported-qwen3-asr-0123456789ab-gguf");
        assert_eq!(id, imported_id("qwen3_asr", DIGEST));
        assert_ne!(
            id,
            imported_id("qwen3_asr", &DIGEST.replace("0123", "ffff"))
        );
        assert_ne!(id, imported_id("whisper", DIGEST));
        for architecture in ["Whisper", "moonshine-streaming", "granite5_ctc"] {
            let id = imported_id(architecture, DIGEST);
            assert!(id.starts_with("imported-") && id.ends_with("-gguf"), "{id}");
            assert!(id.chars().all(|character| character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'));
        }
        assert_eq!(slug("qwen3_asr"), "qwen3-asr");
        assert_eq!(slug("--Moonshine__Streaming--"), "moonshine-streaming");
        assert_eq!(slug("..."), "model");
        assert_eq!(
            slug("a-very-long-architecture-name-with-many-parts"),
            "a-very-long-architecture"
        );

        let spec = imported_spec("qwen3_asr", DIGEST, 7);
        assert_eq!(spec.id, id);
        assert_eq!(spec.version, IMPORTED_VERSION);
        assert!(spec.url.is_empty());
        assert_eq!(spec.size_bytes, 7);
        assert_eq!(spec.allowed_members, [GGUF_MODEL_FILE]);
        assert_eq!(spec.required_paths, [GGUF_MODEL_FILE]);
        assert_eq!(
            super::super::decode_sri(&spec.sha256_sri).expect("SRI of the digest"),
            DIGEST
        );
        assert!(matches!(
            crate::offline_asr::OfflineFamily::for_model(&spec.id),
            Ok(crate::offline_asr::OfflineFamily::Gguf)
        ));
    }

    /// Only the final path component of the decoded value is a name, as
    /// one printable line of at most 120 characters; anything else is no
    /// name.
    #[test]
    fn the_filename_header_keeps_only_a_plain_basename() {
        assert_eq!(
            import_filename("Qwen3-ASR-0.6B-Q8_0.gguf").as_deref(),
            Some("Qwen3-ASR-0.6B-Q8_0.gguf")
        );
        assert_eq!(
            import_filename("%E6%A8%A1%E5%9E%8B%20v2.gguf").as_deref(),
            Some("模型 v2.gguf")
        );
        assert_eq!(
            import_filename("a%2Fb%5Cc%2Egguf").as_deref(),
            Some("c.gguf")
        );
        assert_eq!(
            import_filename("C:\\models\\x.gguf").as_deref(),
            Some("x.gguf")
        );
        assert_eq!(
            import_filename("  spaced\tname\n.gguf ").as_deref(),
            Some("spaced name .gguf")
        );
        let longest = "x".repeat(MAX_NAME_CHARS);
        assert_eq!(import_filename(&longest).as_deref(), Some(longest.as_str()));
        assert_eq!(
            import_filename("x%E2%80%A8y.gguf").as_deref(),
            Some("x y.gguf")
        );
        for none in ["", "%", "%2", "%zz", "..", ".", "/", "a/", "%FF%FE.gguf"] {
            assert_eq!(import_filename(none), None, "{none:?}");
        }
        assert_eq!(import_filename(&(longest + "x")), None);
        assert_eq!(display_name("模型"), Some("模型".to_owned()));
        assert_eq!(display_name("\u{7f}\u{85}"), None);
    }

    /// A file that cannot fit is refused before a byte is staged; one that
    /// fits, with the room the payload needs, is not.
    #[test]
    fn space_is_prechecked_against_the_exact_size() {
        let bytes = 1 << 30;
        assert!(ensure_space(bytes, bytes + PUBLISH_OVERHEAD).is_ok());
        assert!(matches!(
            ensure_space(bytes, bytes + PUBLISH_OVERHEAD - 1),
            Err(ModelError::NoSpace { required, available })
                if required == bytes + PUBLISH_OVERHEAD
                    && available == bytes + PUBLISH_OVERHEAD - 1
        ));
        assert!(matches!(
            ensure_space(u64::MAX, u64::MAX - 1),
            Err(ModelError::NoSpace { required, available }) if required == u64::MAX && available == u64::MAX - 1
        ));
        let (_root, manager) = store();
        fs::create_dir_all(&manager.versions_root).expect("versions directory");
        assert!(available_space(&manager.versions_root).expect("free space") > 0);
    }

    /// Nothing is staged for an import that cannot start: a file larger
    /// than the disk, or an empty file, each fails with its own reason.
    #[test]
    fn begin_import_fails_before_writing() {
        let (_root, manager) = store();
        let too_large = 1_u64 << 60;
        let error = manager
            .begin_import(too_large)
            .expect_err("larger than any disk");
        if gguf_import_supported() {
            fs::create_dir_all(&manager.versions_root).expect("versions directory");
            assert!(matches!(
                error,
                ModelError::NoSpace { required, .. } if required == too_large + PUBLISH_OVERHEAD
            ));
            assert!(matches!(
                manager.begin_import(0),
                Err(ModelError::Invalid(_))
            ));
            assert!(entries(&manager.versions_root).is_empty());
        } else {
            assert!(
                matches!(error, ModelError::Unsupported(message) if message.contains("transcribe.cpp"))
            );
            assert!(!manager.versions_root.exists());
        }
    }

    /// A staged upload nobody holds is deleted; one its importer still
    /// holds is left alone, as is anything that is not a staged upload.
    #[test]
    fn abandoned_staged_uploads_are_reclaimed_and_live_ones_kept() {
        let (_root, manager) = store();
        fs::create_dir_all(&manager.versions_root).expect("versions directory");
        let abandoned = write(
            &manager.versions_root.join(".import-dead.partial"),
            b"staged",
        );
        let live = write(
            &manager.versions_root.join(".import-live.partial"),
            b"staged",
        );
        let payload = write(&manager.versions_root.join("imported-x-gguf"), b"dir");
        let importer = File::open(&live).expect("open the live upload");
        importer.try_lock().expect("lock the live upload");

        manager.sweep_abandoned();

        assert!(!abandoned.exists());
        assert!(live.exists());
        assert!(payload.exists());
    }

    /// An import never takes the id of a catalog model, so the pinned
    /// packages stay where they are.
    #[test]
    fn an_imported_id_is_never_a_catalog_id() {
        let catalog = super::super::catalog();
        for digest in [DIGEST, &DIGEST.replace("0123", "ffff")] {
            for spec in &catalog {
                let imported = imported_id(&spec.id, digest);
                assert_ne!(imported, spec.id);
                assert!(!catalog.iter().any(|entry| entry.id == imported));
            }
        }
    }

    /// Without the engine no file is staged, whatever its size.
    #[cfg(not(feature = "transcribe"))]
    #[test]
    fn an_import_needs_the_engine_and_stages_nothing_without_it() {
        let (_root, manager) = store();
        let mut bytes = &b"GGUF model file"[..];
        for (name, size) in [("cut.gguf", 4_u64), ("notes.gguf", 19), ("short.gguf", 19)] {
            let error = manager
                .import_from(&mut bytes, name, size)
                .expect_err("no engine");
            assert!(matches!(error, ModelError::Unsupported(_)), "{error:?}");
        }
        assert!(!manager.versions_root.exists());
        assert!(manager.list_imported().expect("list").is_empty());
    }

    /// Every kind of wrong file is refused with its reason before anything
    /// is published, and the store holds nothing of it afterwards.
    #[cfg(feature = "transcribe")]
    #[test]
    fn wrong_files_are_refused_and_leave_nothing_behind() {
        let (_root, manager) = store();
        let pinned = super::super::sense_voice_spec();
        assert!(manager.resolve(&pinned.id).expect("resolve").is_none());

        let mut magic_only = &b"GGUF"[..];
        let error = manager
            .import_from(&mut magic_only, "cut.gguf", 4)
            .expect_err("magic only");
        assert!(
            matches!(error, ModelError::Invalid(ref message) if message.starts_with("truncated GGUF file")),
            "{error:?}"
        );

        let mut text = &b"this is not a model"[..];
        let error = manager
            .import_from(&mut text, "notes.gguf", 19)
            .expect_err("not GGUF");
        assert!(matches!(error, ModelError::Invalid(message) if message == "not a GGUF file"));

        let mut short = &b"GGUF model file"[..6];
        let error = manager
            .import_from(&mut short, "short.gguf", 19)
            .expect_err("short read");
        assert!(matches!(
            error,
            ModelError::Incomplete {
                expected: 19,
                actual: 6
            }
        ));

        let mut extra = &b"GGUF model file"[..];
        let error = manager
            .import_from(&mut extra, "grew.gguf", 6)
            .expect_err("more bytes than announced");
        assert!(
            matches!(error, ModelError::Integrity(ref name, ref message) if name == "grew.gguf" && message.contains("grew")),
            "{error:?}"
        );

        assert!(manager.list_imported().expect("list").is_empty());
        assert!(entries(&manager.versions_root).is_empty());
        assert!(manager.resolve(&pinned.id).expect("resolve").is_none());
    }
}
