//! Model supply chain: the pinned catalog, integrity-checked
//! install/check/remove, and the versioned pointer layout (compatible with
//! the Python manager) that every local backend reads its model from:
//! `<data>/models/versions/<id>/<version>-<digest12>/` payloads and
//! `<data>/models/<id>/current` symlinks.
//!
//! Position in the pipeline: nothing here touches audio. The module answers
//! "where is an intact copy of model X?" for
//! - `stream.rs`: `local-streaming` resolves the Paraformer entry (unless
//!   `[asr].local_model_dir` points elsewhere); a cold `local-offline`
//!   session resolves `[local].model` plus `silero-vad`;
//! - `engine.rs`: the daemon's warm engine verifies once at start and warm
//!   sessions skip the per-session check;
//! - `benchmark_cmd.rs`: `benchmark asr --backend local-streaming`;
//! - `model_cmd.rs`: `voiceless model list|install|check|remove`;
//! - `console/library.rs`: `GET/POST/DELETE /api/models` (installs run on a
//!   blocking task; progress is read from `download_path`).
//!
//! Supply-chain contract (README "Model catalog"):
//! - every entry pins an HTTPS URL, the exact byte count, an SRI SHA-256 of
//!   the download, the archive members that may be extracted, the members
//!   that must exist, and a license note;
//! - a download is accepted only when its SHA-256 and size match the pin;
//!   a tarball member outside the allow-list aborts the install;
//! - the payload gets a `manifest.json` with per-file SHA-256 and size;
//!   `check` re-hashes every listed file and reports missing, corrupt and
//!   extra files;
//! - activation is atomic: the fully verified staging directory is renamed
//!   to `versions/<id>/<version>-<digest12>` and the relative `current`
//!   symlink is swapped by rename, so readers see the old or the new
//!   payload, never a partial one;
//! - sessions resolve the pointer afresh and re-verify through
//!   `installed_model_dir`; a corrupted install is reported, never used.
//!
//! Files: downloads are staged as
//! `<cache>/model-downloads/<id>-<version>.partial`; the XDG helpers
//! (`default_data_dir`, `default_cache_dir`, `default_config_dir`,
//! `default_audio_dir`) live here and are shared by history, audio
//! retention, modes and doctor.
//!
//! Failure behaviour: every operation is synchronous (blocking filesystem
//! and network I/O) and reports `ModelError`. A failed install leaves no
//! half-installed payload: the `.partial` file and the staging directory
//! are always removed. If a corrupt existing payload had to be wiped before
//! the retry, the model reads as "not installed" until an install succeeds.
//! There is no locking: concurrent installs of the same id are serialized
//! by the console, not here.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Per-payload manifest written at install time and re-read by `check`.
const MANIFEST_NAME: &str = "manifest.json";

/// One pinned catalog entry: everything needed to fetch, verify, unpack and
/// later re-verify a model. Built by the `*_spec()` constructors below;
/// tests construct their own entries around fixture archives.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Stable catalog id: the value of `[local].model`, the argument of
    /// `voiceless model install <id>`, and the directory name under both
    /// `models/` (pointer) and `models/versions/` (payloads).
    pub id: String,
    /// Upstream release label. It prefixes the payload directory name and
    /// the `.partial` download name; a new version (or digest) therefore
    /// lands in a new side-by-side directory.
    pub version: String,
    /// Download source. `HttpDownloader` refuses anything but `https://`.
    pub url: String,
    /// Subresource-Integrity string `sha256-<base64>` of the download bytes
    /// (the whole tarball or the single file); decoded by `decode_sri`.
    pub sha256_sri: String,
    /// Exact download size. Verified after the hash, and used by the
    /// console to derive install progress from the `.partial` file size.
    pub size_bytes: u64,
    /// How the download becomes a payload directory.
    pub archive_type: ArchiveType,
    /// Tarball directory prefix stripped from every member path; `None`
    /// for single-file downloads.
    pub top_level_directory: Option<String>,
    /// Regular-file members (relative to the stripped top level) that may
    /// be written to disk; any other member aborts the install and any
    /// other file found later is reported as `extra`. For
    /// `ArchiveType::File` the single entry is the payload file name.
    pub allowed_members: Vec<String>,
    /// Members that must exist after extraction and at every later check:
    /// the files the backend actually opens.
    pub required_paths: Vec<String>,
    /// Human-readable license note surfaced by `model list --json` and
    /// `/api/models`. Voiceless only displays it; the user verifies the
    /// license before redistributing a model.
    pub license_status: String,
}

/// Shape of the pinned download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveType {
    /// bzip2-compressed tarball (Sherpa-ONNX release archives), unpacked
    /// member by member against `ModelSpec::allowed_members`.
    TarBz2,
    /// A single file (GGUF model, ONNX VAD) copied into the payload under
    /// `allowed_members[0]`.
    File,
}

/// Streaming Paraformer bilingual zh/en catalog entry pinned from the Python
/// catalog: URL, byte count, SRI, allowed/required members and license note.
///
/// The only streaming (online) local model: the `local-streaming` backend
/// (`local_asr.rs`, Sherpa-ONNX `OnlineRecognizer`, CPU, one thread) opens
/// `encoder.int8.onnx`, `decoder.int8.onnx` and `tokens.txt` from the
/// payload; the fp32 `.onnx` files and `test_wavs/` are allowed so the
/// upstream tarball unpacks unchanged but are never read. Also the model of
/// `benchmark asr --backend local-streaming`. Not a `[local].model` value.
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

/// Owned copies of a static member list (catalog literals stay `&str`).
fn names(members: &[&str]) -> Vec<String> {
    members.iter().map(|member| (*member).to_owned()).collect()
}

/// GitHub release that hosts every Sherpa-ONNX asset in the catalog.
const SHERPA_RELEASE: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models";
/// License note for Sherpa-ONNX re-exports whose archive carries no license
/// file: "the upstream page ships no separate license text; verify before
/// use".
const UNVERIFIED_LICENSE: &str =
    "上游模型页面未在归档内提供独立许可证文本；使用前需用户自行核验许可。";

/// Qwen3-ASR 0.6B (int8): offline, punctuated zh/en/multilingual model
/// driven through VAD segmentation by the `local-offline` backend.
///
/// Sherpa-ONNX offline export (`OfflineFamily::Qwen3Asr`, CPU only: the
/// config rejects `provider = "cuda"` for Sherpa models). The decoder opens
/// `conv_frontend.onnx`, `encoder.int8.onnx`, `decoder.int8.onnx` and the
/// `tokenizer/` directory; `[local].hotwords` biasing applies to this
/// family. Segments are capped at ~25 s by the export (15 s default).
pub fn qwen3_asr_spec() -> ModelSpec {
    ModelSpec {
        id: "qwen3-asr-0.6b-int8".to_owned(),
        version: "asr-models-2026-03-25".to_owned(),
        url: format!("{SHERPA_RELEASE}/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2"),
        sha256_sri: "sha256-OT+KFOL1+5Z0aqqzQpl6QGQQAfvVv5WSoICoMpF47pY=".to_owned(),
        size_bytes: 878_702_423,
        archive_type: ArchiveType::TarBz2,
        top_level_directory: Some("sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25".to_owned()),
        allowed_members: names(&[
            "README.md",
            "conv_frontend.onnx",
            "decoder.int8.onnx",
            "encoder.int8.onnx",
            "test_wavs/README.md",
            "test_wavs/ar1.wav",
            "test_wavs/cantonese.wav",
            "test_wavs/codeswitch.wav",
            "test_wavs/de.wav",
            "test_wavs/es1.wav",
            "test_wavs/f1_noise.wav",
            "test_wavs/fast1.wav",
            "test_wavs/fr1.wav",
            "test_wavs/ja1.wav",
            "test_wavs/noise1-en.wav",
            "test_wavs/noise2.wav",
            "test_wavs/qiqiu1.wav",
            "test_wavs/raokouling.wav",
            "test_wavs/rap1.wav",
            "test_wavs/ru1.wav",
            "test_wavs/transcript.txt",
            "tokenizer/merges.txt",
            "tokenizer/tokenizer_config.json",
            "tokenizer/vocab.json",
        ]),
        required_paths: names(&[
            "conv_frontend.onnx",
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "tokenizer/vocab.json",
            "tokenizer/merges.txt",
            "tokenizer/tokenizer_config.json",
        ]),
        license_status: UNVERIFIED_LICENSE.to_owned(),
    }
}

/// SenseVoice small (int8): very fast offline zh/en/ja/ko/yue model
/// without punctuation.
///
/// Sherpa-ONNX offline export (`OfflineFamily::SenseVoice`, CPU only) for
/// the `local-offline` backend; the decoder opens `model.int8.onnx` and
/// `tokens.txt`. The smallest ASR download in the catalog (~166 MB).
pub fn sense_voice_spec() -> ModelSpec {
    ModelSpec {
        id: "sense-voice-int8".to_owned(),
        version: "asr-models-2025-09-09".to_owned(),
        url: format!(
            "{SHERPA_RELEASE}/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2"
        ),
        sha256_sri: "sha256-cwX3kFv893+gs5OIoxPz2jXGjZcWYaZUdbVvshYsjmM=".to_owned(),
        size_bytes: 165_783_878,
        archive_type: ArchiveType::TarBz2,
        top_level_directory: Some(
            "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09".to_owned(),
        ),
        allowed_members: names(&[
            "README.md",
            "model.int8.onnx",
            "tokens.txt",
            "test_wavs/en.wav",
            "test_wavs/ja.wav",
            "test_wavs/ko.wav",
            "test_wavs/zh.wav",
            "test_wavs/yue.wav",
            "test_wavs/yue-0.wav",
            "test_wavs/yue-1.wav",
            "test_wavs/yue-2.wav",
            "test_wavs/yue-3.wav",
            "test_wavs/yue-4.wav",
            "test_wavs/yue-5.wav",
            "test_wavs/yue-6.wav",
            "test_wavs/yue-7.wav",
            "test_wavs/yue-8.wav",
            "test_wavs/yue-9.wav",
            "test_wavs/yue-10.wav",
            "test_wavs/yue-11.wav",
            "test_wavs/yue-12.wav",
            "test_wavs/yue-13.wav",
            "test_wavs/yue-14.wav",
            "test_wavs/yue-15.wav",
            "test_wavs/yue-16.wav",
            "test_wavs/yue-17.wav",
        ]),
        required_paths: names(&["model.int8.onnx", "tokens.txt"]),
        license_status: UNVERIFIED_LICENSE.to_owned(),
    }
}

/// Hugging Face host for the GGUF conversions (`resolve/main/<file>` URLs
/// serve the raw file, not an HTML page).
const HF: &str = "https://huggingface.co";

/// Qwen3-ASR 1.7B (GGUF Q8_0) for the transcribe.cpp engine: the most
/// accurate local model; needs the CUDA build to be fast.
///
/// Default `[local].model`. `OfflineFamily::Gguf`: decoded by `gguf_asr.rs`
/// (crate feature `transcribe`), device defaults to `cuda` and the model
/// holds ~2.5 GB of VRAM; segments up to 30 s by default. Installs as the
/// single file `model.gguf` (`gguf_asr::MODEL_FILE`), so `allowed_members`
/// and `required_paths` are that one name.
pub fn qwen3_asr_1_7b_gguf_spec() -> ModelSpec {
    ModelSpec {
        id: "qwen3-asr-1.7b-gguf".to_owned(),
        version: "Q8_0".to_owned(),
        url: format!(
            "{HF}/handy-computer/Qwen3-ASR-1.7B-gguf/resolve/main/Qwen3-ASR-1.7B-Q8_0.gguf"
        ),
        sha256_sri: "sha256-mg2BeS3+otXyeLimPes+puAhOc5CwjAfMuoZxPd1Jrc=".to_owned(),
        size_bytes: 2_185_030_624,
        archive_type: ArchiveType::File,
        top_level_directory: None,
        allowed_members: names(&["model.gguf"]),
        required_paths: names(&["model.gguf"]),
        license_status: "Apache-2.0 (Qwen3-ASR; GGUF conversion by handy-computer).".to_owned(),
    }
}

/// Qwen3-ASR 0.6B (GGUF Q8_0): smaller and faster transcribe.cpp model.
///
/// Same family, engine and `model.gguf` layout as the 1.7B entry; the
/// choice for GPUs with less memory or for CPU decoding.
pub fn qwen3_asr_0_6b_gguf_spec() -> ModelSpec {
    ModelSpec {
        id: "qwen3-asr-0.6b-gguf".to_owned(),
        version: "Q8_0".to_owned(),
        url: format!(
            "{HF}/handy-computer/Qwen3-ASR-0.6B-gguf/resolve/main/Qwen3-ASR-0.6B-Q8_0.gguf"
        ),
        sha256_sri: "sha256-8IGy1eI71mnZLMMx1yKooGgZQ7jm80tImW/Vwxm1rNg=".to_owned(),
        size_bytes: 850_423_456,
        archive_type: ArchiveType::File,
        top_level_directory: None,
        allowed_members: names(&["model.gguf"]),
        required_paths: names(&["model.gguf"]),
        license_status: "Apache-2.0 (Qwen3-ASR; GGUF conversion by handy-computer).".to_owned(),
    }
}

/// Silero VAD: segments speech for the `local-offline` backend.
///
/// Not an ASR model and not a valid `[local].model`: every `local-offline`
/// session needs it next to the ASR model (`stream.rs` cold path,
/// `engine.rs` warm load) and `offline_asr::create_vad` opens the single
/// `silero_vad.onnx` file. The Sherpa-ONNX release hosts the file bare, so
/// it is an `ArchiveType::File` entry with an unversioned release label.
pub fn silero_vad_spec() -> ModelSpec {
    ModelSpec {
        id: "silero-vad".to_owned(),
        version: "asr-models".to_owned(),
        url: format!("{SHERPA_RELEASE}/silero_vad.onnx"),
        sha256_sri: "sha256-niRJ4Qh0ltjUyrqQfyPgvT942R+lUkebucI6wJy7H9Y=".to_owned(),
        size_bytes: 643_854,
        archive_type: ArchiveType::File,
        top_level_directory: None,
        allowed_members: names(&["silero_vad.onnx"]),
        required_paths: names(&["silero_vad.onnx"]),
        license_status: "MIT (snakers4/silero-vad).".to_owned(),
    }
}

/// The whole pinned catalog, in the order `model list` and the console
/// show it: streaming Paraformer first, then the GGUF (GPU) models, then
/// the Sherpa-ONNX offline models, then the VAD. Ids are unique; lookups
/// (`installed_model_dir`, `model_cmd::find_spec`, `stream.rs`) take the
/// first match.
pub fn catalog() -> Vec<ModelSpec> {
    vec![
        streaming_paraformer_spec(),
        qwen3_asr_1_7b_gguf_spec(),
        qwen3_asr_0_6b_gguf_spec(),
        qwen3_asr_spec(),
        sense_voice_spec(),
        silero_vad_spec(),
    ]
}

/// Failures of the model manager. `Display` strings are what the CLI and
/// console surface to the user.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The id is not in `catalog()`. (Defined for callers; the manager
    /// itself takes a `ModelSpec` and never produces it.)
    #[error("unknown model id: {0}")]
    UnknownId(String),
    /// No `current` pointer exists for the model (also after a dangling
    /// pointer, see `ModelManager::resolve`).
    #[error("model {0} is not installed")]
    NotInstalled(String),
    /// A verification failed: SHA-256 or size mismatch of a download, a
    /// malformed SRI string, a pointer that is not a symlink or escapes the
    /// model directory, or a non-regular entry inside a payload. The first
    /// field is the model id (or the offending path/SRI for helper errors).
    #[error("model {0}: {1}")]
    Integrity(String, String),
    /// Transport failure or a non-HTTPS catalog URL; nothing was verified.
    #[error("model download failed: {0}")]
    Download(String),
    /// The downloaded archive cannot be unpacked as pinned: a tar/bzip2
    /// error, a member outside the allow-list, a path containing `..`, or a
    /// missing required member.
    #[error("model archive invalid: {0}")]
    Archive(String),
    /// Any other filesystem error (create, rename, remove, read).
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Download boundary; production uses HTTPS, tests inject fixtures.
///
/// Contract: write the complete bytes of `spec.url` to `destination`
/// (creating or truncating it) and return `Err` on any failure. The
/// implementation may leave a partial file behind on error; the manager
/// deletes it and never trusts its content. Blocking.
pub trait Downloader {
    /// Fetch `spec.url` into `destination`. See the trait docs for the
    /// contract.
    fn download(&self, spec: &ModelSpec, destination: &Path) -> Result<(), ModelError>;
}

/// Production downloader: a single blocking `ureq` GET streamed to disk.
/// No resume, no retry; the whole transfer (DNS, connect, redirects, body)
/// must finish within one hour or the install fails.
pub struct HttpDownloader;

impl Downloader for HttpDownloader {
    fn download(&self, spec: &ModelSpec, destination: &Path) -> Result<(), ModelError> {
        // Transport check on the pinned URL only. The bytes themselves are
        // guarded by the hash + size check in `install_from_network`, so
        // even a redirect to another host cannot substitute content
        // unnoticed; HTTPS here protects the pin against downgrade and
        // keeps the request confidential.
        if !spec.url.starts_with("https://") {
            return Err(ModelError::Download(format!(
                "model source must use HTTPS: {}",
                spec.url
            )));
        }
        // ureq 2's `timeout` bounds the entire request including reading the
        // body: one hour is enough for the 2.2 GB GGUF at a few MB/s and
        // keeps a stalled connection from pinning the console's install slot
        // forever.
        let response = ureq::get(&spec.url)
            .timeout(std::time::Duration::from_secs(3_600))
            .call()
            .map_err(|error| ModelError::Download(error.to_string()))?;
        let mut file = fs::File::create(destination)?;
        let mut reader = response.into_reader();
        // Streamed copy: the file grows as bytes arrive, which is what the
        // console's progress bar reads via `download_path`.
        io::copy(&mut reader, &mut file)
            .map_err(|error| ModelError::Download(error.to_string()))?;
        Ok(())
    }
}

/// Result of verifying an installed payload against its manifest and the
/// catalog entry. Relative paths, sorted where it matters. Serialized as-is
/// into console/benchmark output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckReport {
    /// Files listed in the manifest or in `required_paths` that are absent;
    /// `manifest.json` itself when the payload has none.
    pub missing: Vec<String>,
    /// Files on disk that are neither in the manifest nor in the allow-list
    /// (sorted, deduplicated).
    pub extra: Vec<String>,
    /// Files present but whose SHA-256 or size differs from the manifest.
    pub corrupt: Vec<String>,
    /// Structural problems: payload directory name does not match the
    /// pinned digest, unparsable manifest, or a file that cannot be hashed.
    pub errors: Vec<String>,
}

impl CheckReport {
    /// `true` only when every list is empty; the single condition under
    /// which a payload may be used.
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
            && self.extra.is_empty()
            && self.corrupt.is_empty()
            && self.errors.is_empty()
    }
}

/// `manifest.json` written into every payload: provenance of the download
/// plus a per-file digest table that `check` verifies against. Compact
/// JSON, keys sorted (BTreeMap). Unsigned: it detects corruption and stray
/// files, not an attacker who can rewrite the payload directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    id: String,
    version: String,
    source_url: String,
    source_sri: String,
    /// Hex SHA-256 of the download (the decoded SRI).
    source_sha256: String,
    /// Relative path -> digest/size of every regular file except the
    /// manifest itself.
    files: BTreeMap<String, ManifestFile>,
}

/// One payload file as recorded at install time.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestFile {
    sha256: String,
    size: u64,
}

/// Filesystem-level manager over one data/cache directory pair. Stateless
/// apart from the three roots: every call re-reads the disk, so several
/// instances (CLI, console, sessions) can coexist. All methods block.
pub struct ModelManager {
    /// `<data>/models`: holds `<id>/current` pointers and `versions/`.
    models_root: PathBuf,
    /// `<data>/models/versions`: `<id>/<version>-<digest12>/` payloads and
    /// `<id>/.staging-*` directories during an install.
    versions_root: PathBuf,
    /// `<cache>/model-downloads`: `.partial` downloads (disposable).
    downloads_root: PathBuf,
}

impl ModelManager {
    /// Bind the roots under `data_dir` and `cache_dir` (normally
    /// `default_data_dir()` / `default_cache_dir()`). Creates nothing; the
    /// directories appear on the first install.
    pub fn new(data_dir: &Path, cache_dir: &Path) -> Self {
        Self {
            models_root: data_dir.join("models"),
            versions_root: data_dir.join("models").join("versions"),
            downloads_root: cache_dir.join("model-downloads"),
        }
    }

    /// Install (or repair) `spec` and make it the current version. Returns
    /// the payload directory `versions/<id>/<version>-<digest12>`.
    ///
    /// Idempotent and self-healing: if that directory already exists and
    /// passes `check_payload`, no download happens and the pointer is simply
    /// (re)activated, which also repairs a missing or dangling `current`.
    /// A corrupt existing payload is wiped first, then re-downloaded.
    ///
    /// Download, verification, extraction, manifest and the final rename
    /// happen in `install_from_network`; on any error the `.partial` file
    /// and the staging directory are removed before the error is returned,
    /// so a failed install never leaves anything under the canonical
    /// payload name. Errors: `Integrity` (bad SRI, hash or size mismatch),
    /// `Archive` (member outside the allow-list, `..` paths, missing
    /// required member, tar/bzip2 failure), `Download`, `Io`.
    ///
    /// Blocking for the duration of the download and of hashing the payload
    /// (gigabytes for the GGUF models); the console runs it on
    /// `spawn_blocking`. Not safe against a concurrent install of the same
    /// id from another process (no lock); the console serializes its own.
    pub fn install<D: Downloader>(
        &self,
        spec: &ModelSpec,
        downloader: &D,
    ) -> Result<PathBuf, ModelError> {
        // The payload name embeds the first 12 hex chars of the pinned
        // digest, so a changed pin never overwrites a payload installed from
        // a different pin and `check_payload` can confirm the directory
        // belongs to this spec.
        let digest_hex = decode_sri(&spec.sha256_sri)?;
        let version_name = format!("{}-{}", spec.version, &digest_hex[..12]);
        let model_versions = self.versions_root.join(&spec.id);
        fs::create_dir_all(&model_versions)?;
        let destination = model_versions.join(&version_name);

        if destination.exists() {
            // Re-verify instead of trusting the name: an earlier install may
            // have been corrupted since. A clean payload is reused as-is.
            let report = self.check_payload(spec, &destination)?;
            if report.ok() {
                self.activate(&spec.id, &destination)?;
                return Ok(destination);
            }
            // Corrupt: remove the whole directory before downloading again.
            // From here until the new payload is renamed into place the
            // `current` pointer dangles and the model reads as not
            // installed, which is the safe state (never a half-valid one).
            fs::remove_dir_all(&destination)?;
        }

        fs::create_dir_all(&self.downloads_root)?;
        let partial = self.download_path(spec);
        // Staging lives next to the destination (same filesystem) so the
        // final `rename` is atomic; the unique suffix keeps a crashed
        // earlier staging directory from being reused.
        let staging = model_versions.join(format!(".staging-{}", unique_suffix()));
        let result = self.install_from_network(spec, downloader, &partial, &staging, &digest_hex);
        // Always clean up, success or failure: the `.partial` is never
        // resumed and a leftover staging directory is never trusted.
        let _ = fs::remove_file(&partial);
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        let staged = result?;
        self.activate(&spec.id, &destination)?;
        Ok(staged)
    }

    /// Download into `partial`, verify the bytes, unpack into `staging`,
    /// write the manifest and rename `staging` to the canonical payload
    /// directory. Returns that directory. Every early return leaves
    /// `partial` and/or `staging` behind for `install` to delete.
    fn install_from_network<D: Downloader>(
        &self,
        spec: &ModelSpec,
        downloader: &D,
        partial: &Path,
        staging: &Path,
        expected_digest_hex: &str,
    ) -> Result<PathBuf, ModelError> {
        // No resume: a `.partial` left by a crashed or killed earlier run
        // has unknown content, so it is discarded and the download restarts
        // from byte zero. (The downloader truncates anyway; this also
        // covers a downloader that appends.)
        if partial.exists() {
            let _ = fs::remove_file(partial);
        }
        downloader.download(spec, partial)?;
        // Integrity gate 1: full-file SHA-256 of the download against the
        // catalog pin. This is the supply-chain check proper: nothing is
        // unpacked, and no manifest hash is derived, from bytes that did not
        // match. A truncated or substituted download fails here.
        let actual_digest = sha256_file(partial)?;
        if actual_digest != expected_digest_hex {
            return Err(ModelError::Integrity(
                spec.id.to_owned(),
                format!("SHA-256 mismatch: expected {expected_digest_hex}, got {actual_digest}"),
            ));
        }
        // Integrity gate 2: exact byte count. After a hash match this can
        // only fail when the catalog's `size_bytes` disagrees with its own
        // SRI, i.e. it catches a bad pin (and keeps the size the console
        // uses for progress honest) rather than a bad download.
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
        // Hash what actually landed on disk (not the archive's claims) and
        // record it: `check` later compares the payload to this manifest.
        // `walk_relative` also rejects symlinks or other non-regular entries
        // an archive might have planted.
        let files = hash_staged_files(staging)?;
        write_manifest(staging, spec, expected_digest_hex, &files)?;

        let version_name = staging.file_name().expect("staging name");
        let destination = staging.parent().unwrap().join(format!(
            "{}-{}",
            spec.version,
            &expected_digest_hex[..12]
        ));
        let _ = version_name;
        // Atomic publish: only a fully extracted, hashed and manifested
        // directory is renamed into the canonical name, and `rename` on one
        // filesystem is atomic, so a concurrent `resolve`/`check` never sees
        // a half-written payload under that name. A directory that appeared
        // meanwhile (another installer) is replaced rather than merged.
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(staging, &destination)?;
        Ok(destination)
    }

    /// Where an in-flight download is written; its size over
    /// `spec.size_bytes` is the install progress.
    ///
    /// `<cache>/model-downloads/<id>-<version>.partial`. The console polls
    /// its metadata for `/api/models` `progress`; the file is deleted when
    /// the install finishes either way, so absence means "not downloading".
    pub fn download_path(&self, spec: &ModelSpec) -> PathBuf {
        self.downloads_root
            .join(format!("{}-{}.partial", spec.id, spec.version))
    }

    /// Verify the currently active payload of `spec`. Returns the payload
    /// path together with the report; the report (not an error) carries
    /// content problems. Errors: `NotInstalled` when no pointer exists,
    /// `Integrity` when the pointer itself is malformed or the payload
    /// contains a non-regular entry, `Io` on read failures. Hashes every
    /// manifest file, so it costs a full read of the payload.
    pub fn check(&self, spec: &ModelSpec) -> Result<(PathBuf, CheckReport), ModelError> {
        let payload = self
            .resolve(&spec.id)?
            .ok_or_else(|| ModelError::NotInstalled(spec.id.to_owned()))?;
        let report = self.check_payload(spec, &payload)?;
        Ok((payload, report))
    }

    /// Resolve the current payload for an installed model without caching;
    /// returns `None` when no pointer exists.
    ///
    /// The `current` link is followed by hand (`symlink_metadata` +
    /// `read_link`) instead of canonicalized so that a pointer that is a
    /// plain file or directory, or one whose target is not exactly
    /// `../versions/<id>/<dir>`, is an `Integrity` error rather than a
    /// silently accepted foreign path. Does not verify the payload content;
    /// `check` does. Note that a dangling symlink reads as `None` (not
    /// installed), because `Path::exists` follows links.
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
        // Only the exact relative shape written by `activate` is accepted;
        // anything else could point a session at files outside the
        // versioned payload tree.
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

    /// Delete the pointer directory and every installed version of
    /// `model_id`. Returns `Ok(false)` when neither existed. Does not touch
    /// an in-flight `.partial` and does not know whether a running engine
    /// still has the files open (the console refuses to remove the loaded
    /// engine's model; the CLI does not).
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

    /// Compare a payload directory with its own `manifest.json` and with the
    /// catalog entry. Three sources are cross-checked: the manifest's file
    /// table (hash + size of each file), `spec.required_paths` (must exist)
    /// and `spec.allowed_members` (everything else is `extra`). The
    /// directory name must carry the spec's version and digest prefix, so
    /// a payload installed from a different pin is not accepted for this
    /// one. Returns `Err` only for a malformed SRI, read failures, or a
    /// non-regular entry inside the payload.
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

        // Without a readable manifest there is nothing to compare against:
        // report just that and stop (the payload is unusable either way).
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

        // Every manifest file is re-hashed in full; a size check alone would
        // miss bit flips and same-length tampering.
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
        // Required members are checked against the spec, independently of
        // the manifest, so a manifest that omits them cannot hide their
        // absence.
        for required in &spec.required_paths {
            if !payload.join(required).is_file() && !report.missing.contains(required) {
                report.missing.push(required.clone());
            }
        }
        // Strays: anything on disk the manifest did not record or the
        // catalog does not allow. The two conditions usually coincide, hence
        // the sort + dedup.
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

    /// Point `models/<id>/current` at `destination` atomically.
    ///
    /// The link is created under a temporary name and then renamed over
    /// `current`: `rename(2)` replaces an existing symlink atomically, so a
    /// concurrent `resolve` sees either the previous or the new target,
    /// never a missing pointer. The target is relative
    /// (`../versions/<id>/<dir>`) so the data directory can be moved as a
    /// whole; `validate_pointer_target` accepts exactly this shape.
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
        // Defensive: after a successful rename the temporary name is gone;
        // this only matters if rename somehow left it behind.
        if temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        Ok(())
    }
}

/// Unpack a hash-verified tar.bz2 into `staging`, member by member, under
/// the allow-list. Streams the archive once; stops at the first offending
/// member (the caller discards the staging directory).
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
        // Path traversal guard, deliberately coarse: any `..` anywhere in
        // the member name is rejected, not just a leading component.
        if raw.contains("..") {
            return Err(ModelError::Archive(format!(
                "archive member escapes the model directory: {raw}"
            )));
        }
        // Sherpa tarballs wrap everything in one directory; strip it so the
        // payload holds `encoder.int8.onnx`, not `<tarball>/encoder...`.
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
        // Allow-list gate. The hash pin says which bytes are expected; the
        // allow-list says which files those bytes may become. Even a
        // hash-matching tarball cannot write anything the catalog author
        // did not enumerate (a renamed member, an absolute or odd path, a
        // file added upstream), which keeps a careless re-pin from
        // smuggling content in and lets `check` flag strays later. Exact
        // string match: no globs, no normalisation.
        if !spec.allowed_members.contains(&relative) {
            return Err(ModelError::Archive(format!(
                "archive member {relative} is not in the allowed manifest"
            )));
        }
        let target = staging.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // `unpack` writes whatever entry type this is (a symlink member with
        // an allowed name would be created as a symlink); such entries are
        // then rejected by `walk_relative` when the payload is hashed.
        entry
            .unpack(&target)
            .map_err(|error| ModelError::Archive(error.to_string()))?;
    }
    // The archive must contain everything the backend will open; a tarball
    // that merely passes the allow-list but lacks a model file is useless.
    for required in &spec.required_paths {
        if !staging.join(required).is_file() {
            return Err(ModelError::Archive(format!(
                "archive is missing required member {required}"
            )));
        }
    }
    Ok(())
}

/// Place a verified single-file download into `staging` under the catalog
/// name (`allowed_members[0]`). Copied, not renamed: the cache and data
/// directories may be on different filesystems.
fn install_single_file(spec: &ModelSpec, archive: &Path, staging: &Path) -> Result<(), ModelError> {
    let name = spec
        .allowed_members
        .first()
        .ok_or_else(|| ModelError::Archive("file model has no member name".to_owned()))?;
    fs::copy(archive, staging.join(name))?;
    Ok(())
}

/// SHA-256 and size of every regular file under `staging` (recursive,
/// sorted by relative path): the manifest's file table.
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

/// Write `manifest.json` into `staging` (compact JSON). Happens before the
/// staging directory is renamed, so a published payload always has one.
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

/// Sorted relative paths of all regular files under `root`, recursing into
/// directories. Any other entry type (symlink, device, socket) is an
/// `Integrity` error: payloads consist of regular files only, which is how
/// a symlink smuggled in by an archive, or planted afterwards, fails both
/// the install and every later `check` instead of being followed.
fn walk_relative(root: &Path) -> Result<Vec<String>, ModelError> {
    let mut collected = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            // `DirEntry::file_type` does not follow symlinks, so a link to a
            // directory or file is seen as a link and rejected below.
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

/// Lower-case hex SHA-256 of a file, streamed in 64 KiB blocks so
/// multi-gigabyte models are hashed without loading them into memory.
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

/// Lower-case hex encoding of raw digest bytes.
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Turn an SRI string (`sha256-<standard base64>`) into the 64-char hex
/// digest used for file comparison and directory names. Only sha256 is
/// accepted and the decoded digest must be exactly 32 bytes; anything else
/// is an `Integrity` error whose first field is the SRI string itself.
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

/// Accept only a `current` link of the exact form `../versions/<id>/<dir>`
/// and return `<dir>`. Absolute targets, a different model id, extra or
/// missing components and `.`/`..` as the final component are all
/// `Integrity` errors: the pointer must stay inside this model's own
/// versions tree.
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

/// Nanosecond timestamp plus pid, for staging-directory and temporary
/// symlink names that must not collide with a concurrent or crashed run.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}-{}", std::process::id())
}

/// Payload directory of an installed, intact catalog model.
///
/// The per-session re-verification entry point: `stream.rs` calls it for
/// every `local-streaming` session and every cold `local-offline` session,
/// `engine.rs` once when the daemon loads its warm engine. It resolves the
/// pointer afresh and runs a full `check` (hashing the payload), so a model
/// corrupted after installation is refused instead of loaded. Errors are
/// user-facing strings: unknown id, "not installed; run `voiceless model
/// install <id>`", a corruption summary with counts, or the manager error.
/// Uses the default XDG directories.
pub fn installed_model_dir(id: &str) -> Result<PathBuf, String> {
    let spec = catalog()
        .into_iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| format!("unknown model id: {id}"))?;
    let manager = ModelManager::new(&default_data_dir(), &default_cache_dir());
    match manager.check(&spec) {
        Ok((path, report)) if report.ok() => Ok(path),
        Ok((_, report)) => Err(format!(
            "model {id} is corrupted: {} missing, {} corrupt, {} extra; reinstall it",
            report.missing.len(),
            report.corrupt.len(),
            report.extra.len()
        )),
        Err(ModelError::NotInstalled(_)) => Err(format!(
            "model {id} is not installed; run `voiceless model install {id}`"
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// `$XDG_DATA_HOME/voiceless` (default `~/.local/share/voiceless`): models,
/// `history.sqlite3` and `audio/`. Only computed here; `install` and
/// `doctor` create it on demand.
pub fn default_data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("voiceless")
}

/// `$XDG_CACHE_HOME/voiceless` (default `~/.cache/voiceless`): only
/// disposable `.partial` downloads live here.
pub fn default_cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache").join("voiceless")
}

/// Retained WAV recordings from successful streaming sessions.
pub fn default_audio_dir() -> PathBuf {
    default_data_dir().join("audio")
}

/// `$XDG_CONFIG_HOME/voiceless` (default `~/.config/voiceless`):
/// `config.toml` and `modes.json`.
pub fn default_config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("voiceless")
}

/// Value of an XDG base-directory variable, or `$HOME/<fallback>`; falls
/// back to the current directory when even `HOME` is unset.
fn xdg_dir(variable: &str, fallback: &str) -> PathBuf {
    std::env::var(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned())).join(fallback)
        })
}
