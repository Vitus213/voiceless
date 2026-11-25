//! Settings endpoints: config (comment-preserving TOML edits, write-only
//! secrets), text-cleanup preview, organize prompt and modes.
//!
//! Mounted under `/api` by `super::router`; the "Config" and "Modes" tables
//! of `docs/console-api.md`. None of these need the daemon, so they work
//! under `voiceless history serve` too. Files: `config.toml` at
//! `ConsoleState::config_path` (read on every request; patched with
//! `toml_edit` so the user's comments and layout survive; written 0600
//! through a temp file and rename), `config.toml.lock` next to it (the
//! writers' lock) and `modes.json` (`crate::stream::modes_path`, via
//! `ModesRepository`).
//!
//! Invariants: the four `SECRETS` are never returned (`masked` blanks them
//! and adds `has_<key>`), and no other answer carries them. A revision is
//! a tag of the whole file under a random key of this process: it changes
//! with a secret, but it verifies no guess of one, as the key never leaves
//! the process. A conflict compares fingerprints. A change is validated
//! with the normal config loader before anything is written, so an invalid
//! value leaves the file untouched (400); after a write the caller learns
//! from `restart_engine` whether this edit changes resources that need an
//! explicit apply. Ordinary edits can take effect before the next session.
//!
//! Concurrent edits. `GET /api/config` hands out a revision with the
//! config. A page that edits sends back that revision and only the fields
//! it changed (`PUT /api/config` with `{"revision", "fields"}`), and the
//! runtime merges three ways: it remembers a fingerprint of every field for
//! each revision it handed out (`Baselines`) and writes a field unless the
//! file's value for it is no longer the one the revision held. Then nothing
//! is written and the answer is 409 with the current masked config and
//! revision, so the page keeps its draft and can show both values. A
//! revision this process does not remember (older than the last
//! `REVISIONS_REMEMBERED`, or issued by a previous run) conflicts in every
//! field: nothing is lost, the user confirms against the current values. A
//! `PUT` without a revision is the older last-writer-wins contract.
//!
//! Writers here (`put_config`, `save_prompt`) hold an exclusive lock on
//! `config.toml.lock` from the read to the rename, whatever process they
//! run in, so two of them never read the same version and write in turn. A
//! file editor takes no lock: the file is compared once more right before
//! the rename, and the attempt starts over when it differs.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use super::{ApiError, ApiResult, ConsoleState};
use crate::config::{AppConfig, ConfigError, TextConfig, SECRETS};
use crate::modes::{ModesError, ModesRepository};

/// Revisions whose fields this process still remembers; an older one
/// conflicts in every field.
const REVISIONS_REMEMBERED: usize = 256;
/// How long a writer waits for the one before it, and how often it looks
/// whether the lock is free meanwhile.
const WRITER_WAIT: Duration = Duration::from_secs(5);
const WRITER_POLL: Duration = Duration::from_millis(5);
/// Save attempts when a file editor changes the configuration mid-write.
const ATTEMPTS: usize = 3;
/// The 503 of a writer that gave up waiting.
const WRITERS_BUSY: &str = "another writer is saving config.toml; try again";

/// Whether `[section] key` is one of `SECRETS`.
fn is_secret(section: &str, key: &str) -> bool {
    SECRETS.contains(&(section, key))
}

/// The config as JSON with secrets blanked and `has_<key>` flags added.
///
/// Serialises the whole `AppConfig` (every section, defaults filled in),
/// then for each `SECRETS` entry replaces the value with `""` and adds
/// `has_<key>: true` when a non-empty secret is stored. 500 only if
/// serialisation fails.
pub fn masked(config: &AppConfig) -> Result<Value, ApiError> {
    let mut value = serde_json::to_value(config).map_err(ApiError::internal)?;
    for (section, key) in SECRETS {
        if let Some(table) = value.get_mut(*section).and_then(Value::as_object_mut) {
            let set = table
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|secret| !secret.is_empty());
            table.insert((*key).to_owned(), json!(""));
            table.insert(format!("has_{key}"), json!(set));
        }
    }
    Ok(value)
}

/// `GET /api/config` (console API "Config"): the full `AppConfig` as JSON
/// with secrets masked, plus `"revision"`: the opaque revision of the
/// `config.toml` that was read, which a field update sends back
/// (`put_config`). 200, or 500 when `config.toml` cannot be loaded (invalid
/// TOML, unknown key, or it holds keys while readable by others).
pub async fn get_config(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let file = read_config(&state.config_path, &BASELINES).map_err(ApiError::internal)?;
    let mut value = masked(&file.config)?;
    value["revision"] = json!(file.revision);
    Ok(Json(value))
}

/// Export the configured settings as reusable TOML without credentials.
pub async fn export_template(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let mut template = toml::Value::try_from(state.config()?).map_err(ApiError::internal)?;
    for (section, key) in SECRETS
        .iter()
        .copied()
        .chain([("processing", "api_key_env")])
    {
        if let Some(values) = template
            .get_mut(section)
            .and_then(toml::Value::as_table_mut)
        {
            values.remove(key);
        }
    }
    let text = toml::to_string_pretty(&template).map_err(ApiError::internal)?;
    Ok(Json(json!({"toml": text})))
}

/// `PUT /api/config` (console API "Config"). Two request shapes:
///
/// - `{section: {key: value}}`: the shape without a revision. Every key is
///   set whatever the file holds, so the last writer wins. Answers
///   `{"ok": true, "restart_engine": bool}`.
/// - `{"revision": "…", "fields": {"section.key": value}}`: only the
///   edited fields, merged against `revision`. 200 with `{"ok": true,
///   "revision", "restart_engine", "config"}`, the saved masked config and
///   its revision, the pair a `GET` answers. 409 with `{"error",
///   "revision", "config", "conflicts": ["section.key", …]}` when a field
///   changed since `revision`: nothing is written, and `config` and
///   `revision` describe the file as it is.
///
/// In both shapes `null` removes a key and `""` keeps a secret. 400 with
/// the loader's message for an invalid update (nothing written), 500 when
/// the file cannot be read or written, 503 when another writer holds it
/// for longer than `WRITER_WAIT`.
pub async fn put_config(
    State(state): State<Arc<ConsoleState>>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let edit = requested_edit(&body).map_err(ApiError::bad_request)?;
    let checked = matches!(edit, Edit::Checked { .. });
    let path = state.config_path.clone();
    let result = tokio::task::spawn_blocking(move || save_config(&path, &BASELINES, &edit))
        .await
        .map_err(ApiError::internal)?;
    let saved = match result {
        Err(UpdateError::Conflict(conflict)) => return conflict_response(conflict),
        result => result.map_err(UpdateError::into_api_error)?,
    };
    if !checked {
        let answer = json!({ "ok": true, "restart_engine": saved.restart_engine });
        return Ok(Json(answer).into_response());
    }
    let answer = json!({
        "ok": true,
        "revision": saved.file.revision,
        "restart_engine": saved.restart_engine,
        "config": masked(&saved.file.config)?,
    });
    Ok(Json(answer).into_response())
}

/// 409 for a refused field update: the paths that changed since the
/// request's revision, with the file as it is now, so the page keeps its
/// draft and shows both values (a secret as presence only).
fn conflict_response(conflict: Conflict) -> Result<Response, ApiError> {
    let answer = json!({
        "error": changed_elsewhere(&conflict.fields),
        "revision": conflict.current.revision,
        "config": masked(&conflict.current.config)?,
        "conflicts": conflict.fields,
    });
    Ok((StatusCode::CONFLICT, Json(answer)).into_response())
}

/// The message of a refused update: the paths, and no value.
fn changed_elsewhere(paths: &[String]) -> String {
    format!(
        "config.toml was changed elsewhere after it was loaded; review {}",
        paths.join(", ")
    )
}

/// `PUT /api/config` body → the edit it asks for. A top-level `revision`
/// or `fields` selects the revision-aware shape; no section is so named,
/// so the older shape stays unambiguous.
fn requested_edit(body: &Value) -> Result<Edit, String> {
    let sections = body
        .as_object()
        .ok_or_else(|| "the patch must be an object of sections".to_owned())?;
    let mut fields = Vec::new();
    let revision = if sections.contains_key("revision") || sections.contains_key("fields") {
        let other = sections
            .keys()
            .find(|key| !matches!(key.as_str(), "revision" | "fields"));
        if let Some(other) = other {
            return Err(format!(
                "a revision-aware update takes only `revision` and `fields`, not `{other}`"
            ));
        }
        let revision = sections
            .get("revision")
            .and_then(Value::as_str)
            .filter(|revision| !revision.is_empty())
            .ok_or_else(|| "`revision` must come from `GET /api/config`".to_owned())?;
        let changes = sections
            .get("fields")
            .and_then(Value::as_object)
            .ok_or_else(|| "`fields` must be an object of `section.key` paths".to_owned())?;
        fields = changes
            .iter()
            .map(|(path, value)| Field::at(path, value.clone()))
            .collect::<Result<_, _>>()?;
        Some(revision.to_owned())
    } else {
        for (section, keys) in sections {
            let keys = keys
                .as_object()
                .ok_or_else(|| format!("[{section}] must be an object"))?;
            for (key, value) in keys {
                if key.starts_with("has_") {
                    continue; // read-only flags echoed back by the page
                }
                fields.push(Field {
                    section: section.clone(),
                    key: key.clone(),
                    value: value.clone(),
                });
            }
        }
        None
    };
    Ok(match revision {
        Some(revision) => Edit::Checked { revision, fields },
        None => Edit::Overwrite(fields),
    })
}

/// Settings the warm engine or hotkey listener was built from.
///
/// Compare the backend, its Paraformer model path when selected, the
/// `[local]` options, decoder hotwords and the hotkey listener settings.
pub(crate) fn needs_engine_reload(before: &AppConfig, after: &AppConfig) -> bool {
    before.asr.streaming_backend != after.asr.streaming_backend
        || (after.asr.streaming_backend == "local-streaming"
            && before.asr.local_model_dir != after.asr.local_model_dir)
        || before.local != after.local
        || before.text.hotwords != after.text.hotwords
        || before.hotkey != after.hotkey
        || before.audio.source != after.audio.source
}

/// The config file as it was read: its text, the revision of that text and
/// the config it loads to. It holds secrets, so its `Debug` prints the
/// revision only.
pub(crate) struct ConfigFile {
    pub(crate) revision: String,
    text: String,
    pub(crate) config: AppConfig,
}

impl ConfigFile {
    fn parse(text: String) -> Result<Self, ConfigError> {
        Ok(Self {
            config: AppConfig::from_toml(&text)?,
            revision: revision_of(&text),
            text,
        })
    }

    pub(crate) fn changed_on_disk(&self, path: &Path) -> Result<Option<Self>, ConfigError> {
        let text = read_text(path)?;
        (text != self.text).then(|| Self::parse(text)).transpose()
    }
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigFile")
            .field("revision", &self.revision)
            .finish()
    }
}

/// The text of the config at `path`: empty when there is no file, which
/// loads as the defaults.
fn read_text(path: &Path) -> Result<String, ConfigError> {
    match crate::config::read_private_text(path) {
        Err(ConfigError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        result => result,
    }
}

/// The revision of a config text: the tag of its bytes under the process
/// key, as 64 hex characters. Texts share a revision only when they are
/// the same bytes of the same run, so every save changes it, a file
/// editor's as well, comments included, and a revision from an earlier run
/// never names a current file.
fn revision_of(text: &str) -> String {
    PROCESS_KEY
        .tag(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// HMAC-SHA256 (RFC 2104) under one key, ready for messages: the two hash
/// states that already absorbed the key pads.
struct Mac {
    inner: Sha256,
    outer: Sha256,
}

impl Mac {
    /// A `Mac` under `key`. The key is shorter than SHA-256's 64-byte
    /// block, so HMAC zero-pads it before the xor.
    fn new(key: [u8; 32]) -> Self {
        let mut mac = Self {
            inner: Sha256::new(),
            outer: Sha256::new(),
        };
        for byte in key {
            mac.inner.update([byte ^ 0x36]);
            mac.outer.update([byte ^ 0x5c]);
        }
        mac.inner.update([0x36; 32]);
        mac.outer.update([0x5c; 32]);
        mac
    }

    /// The 32-byte tag of `message`. Nobody can compute or check one
    /// without the key, so a tag verifies nothing about `message`.
    fn tag(&self, message: &[u8]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        inner.update(message);
        let mut outer = self.outer.clone();
        outer.update(inner.finalize());
        let mut tag = [0; 32];
        tag.copy_from_slice(&outer.finalize());
        tag
    }
}

/// The key of every revision and fingerprint: 32 bytes from the OS, once
/// per process. `sha2` has no HMAC and no crate for one is locked in, so
/// `Mac` spells the construction out; `tests` checks it against RFC 4231.
/// Panics when the OS random source is unavailable, as the console token
/// does: better no revision than a guessable key.
static PROCESS_KEY: LazyLock<Mac> = LazyLock::new(|| {
    let mut key = [0_u8; 32];
    getrandom::getrandom(&mut key).expect("OS random source available");
    Mac::new(key)
});

pub(crate) fn load_revision(path: &Path, required: bool) -> Result<ConfigFile, ConfigError> {
    let text = if required {
        crate::config::read_private_text(path)?
    } else {
        read_text(path)?
    };
    ConfigFile::parse(text)
}

pub(crate) fn saved_revision(path: &Path) -> Result<String, ConfigError> {
    read_text(path).map(|text| revision_of(&text))
}

/// The config at `path` as one consistent state, which `baselines` then
/// remembers.
fn read_config(path: &Path, baselines: &Mutex<Baselines>) -> Result<ConfigFile, ConfigError> {
    let file = load_revision(path, false)?;
    baselines
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remember(&file)?;
    Ok(file)
}

/// The tag of one field's JSON value under the process key.
type Fingerprint = [u8; 32];

/// What one revision holds: every `section.key` of the config with the
/// fingerprint of its value.
type Baseline = BTreeMap<String, Fingerprint>;

/// Every `section.key` of `config` with the tag of its JSON value.
/// Fingerprints tell whether a field changed without keeping a secret's
/// value in memory, and never leave the process.
fn fingerprints(config: &AppConfig) -> Result<Baseline, serde_json::Error> {
    let mut fields = Baseline::new();
    let Value::Object(sections) = serde_json::to_value(config)? else {
        return Ok(fields);
    };
    for (section, keys) in sections {
        let Value::Object(keys) = keys else {
            continue;
        };
        for (key, value) in keys {
            let mut fingerprint = [0; 32];
            fingerprint.copy_from_slice(&PROCESS_KEY.tag(value.to_string().as_bytes()));
            fields.insert(format!("{section}.{key}"), fingerprint);
        }
    }
    Ok(fields)
}

/// The revisions handed out recently, most recent first.
#[derive(Default)]
struct Baselines {
    revisions: Vec<(String, Baseline)>,
}

impl Baselines {
    /// What `revision` held, when it is still remembered.
    fn fields_at(&self, revision: &str) -> Option<Baseline> {
        self.revisions
            .iter()
            .find(|(known, _)| known == revision)
            .map(|(_, fields)| fields.clone())
    }

    /// Make `file.revision` the most recent one, remembering its fields
    /// when they are not known yet.
    fn remember(&mut self, file: &ConfigFile) -> Result<(), ConfigError> {
        let known = self
            .revisions
            .iter()
            .position(|(revision, _)| *revision == file.revision);
        if let Some(index) = known {
            let recent = self.revisions.remove(index);
            self.revisions.insert(0, recent);
            return Ok(());
        }
        let fields = fingerprints(&file.config).map_err(|error| {
            ConfigError::Invalid(format!("cannot serialise the configuration: {error}"))
        })?;
        self.revisions.insert(0, (file.revision.clone(), fields));
        self.revisions.truncate(REVISIONS_REMEMBERED);
        Ok(())
    }
}

/// The revisions handed out by this process. A daemon's console and a
/// standalone one are separate processes with separate memories, and a page
/// only talks to the one that served it its revision.
static BASELINES: LazyLock<Mutex<Baselines>> = LazyLock::new(|| Mutex::new(Baselines::default()));

/// One `section.key` of the config and the value to store there: `null`
/// removes the key.
struct Field {
    section: String,
    key: String,
    value: Value,
}

impl Field {
    /// `section.key`, as a conflict names it.
    fn path(&self) -> String {
        format!("{}.{}", self.section, self.key)
    }

    /// The field at `path`: two bare TOML keys and nothing else.
    fn at(path: &str, value: Value) -> Result<Self, String> {
        match path.split_once('.') {
            Some((section, key)) if is_bare_key(section) && is_bare_key(key) => Ok(Self {
                section: section.to_owned(),
                key: key.to_owned(),
                value,
            }),
            _ => Err(format!("{path:?} is not a `section.key` path")),
        }
    }
}

/// A TOML bare key, as `config.toml` names every section and field.
fn is_bare_key(text: &str) -> bool {
    let is_word = |character: char| character.is_ascii_alphanumeric() || "_-".contains(character);
    !text.is_empty() && text.chars().all(is_word)
}

/// Whether a field's value asks to keep the stored secret: `""` for one of
/// `SECRETS`. There is nothing to set then.
fn keeps_secret(section: &str, key: &str, value: &Value) -> bool {
    value.as_str() == Some("") && is_secret(section, key)
}

/// One requested change to `config.toml`.
enum Edit {
    /// Set every field whatever the file holds: the last writer wins (a
    /// `PUT /api/config` without a revision, `POST /api/prompt`).
    Overwrite(Vec<Field>),
    /// Set every field whose value is still the one `revision` held.
    Checked {
        revision: String,
        fields: Vec<Field>,
    },
}

/// What an edit means for the file as read.
enum Plan {
    /// No field to set: there were none, or each asked to keep its secret.
    Unchanged,
    /// These fields changed since the edit's revision: nothing is written.
    Conflict(Vec<String>),
    /// The text to write.
    Save(String),
}

impl Edit {
    /// What the edit means for the file as read.
    fn plan(&self, file: &ConfigFile, baselines: &Mutex<Baselines>) -> Result<Plan, UpdateError> {
        // `""` for a secret keeps the stored one: there is nothing to set.
        let (revision, fields) = match self {
            Self::Overwrite(fields) => (None, fields),
            Self::Checked { revision, fields } => (Some(revision), fields),
        };
        let fields: Vec<&Field> = fields
            .iter()
            .filter(|field| !keeps_secret(&field.section, &field.key, &field.value))
            .collect();
        if let Some(revision) = revision {
            let conflicts = changed_since(revision, &fields, file, baselines)?;
            if !conflicts.is_empty() {
                return Ok(Plan::Conflict(conflicts));
            }
        }
        if fields.is_empty() {
            return Ok(Plan::Unchanged);
        }
        patched(&file.text, &fields)
            .map(Plan::Save)
            .map_err(UpdateError::Invalid)
    }
}

/// The paths among `fields` whose value in `file` is no longer what it was
/// at `revision`. A revision this process cannot remember conflicts in
/// every field: it cannot tell which ones changed.
fn changed_since(
    revision: &str,
    fields: &[&Field],
    file: &ConfigFile,
    baselines: &Mutex<Baselines>,
) -> Result<Vec<String>, UpdateError> {
    if revision == file.revision {
        return Ok(Vec::new());
    }
    let known = baselines.lock().unwrap_or_else(|error| error.into_inner());
    let Some(base) = known.fields_at(revision) else {
        return Ok(fields.iter().map(|field| field.path()).collect());
    };
    let current = fingerprints(&file.config).map_err(failed)?;
    Ok(fields
        .iter()
        .filter(|field| base.get(&field.path()) != current.get(&field.path()))
        .map(|field| field.path())
        .collect())
}

/// The file's text with every `field` set, keeping comments and layout:
/// `null` removes the key, anything else is stored through `to_item`.
/// A missing file starts from an empty document, sections are created as
/// needed, and removing a key is how a secret is cleared or any other key
/// reverts to its default.
fn patched(text: &str, fields: &[&Field]) -> Result<String, String> {
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error| format!("config.toml is not valid TOML: {error}"))?;
    for field in fields {
        let table = document
            .entry(&field.section)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| format!("[{}] is not a table in config.toml", field.section))?;
        match &field.value {
            Value::Null => {
                table.remove(&field.key);
            }
            value => {
                let mut replacement = to_item(value)?;
                if let Some(existing) = table.get_mut(&field.key) {
                    if let (Some(before), Some(after)) =
                        (existing.as_value(), replacement.as_value_mut())
                    {
                        *after.decor_mut() = before.decor().clone();
                    }
                    *existing = replacement;
                } else {
                    table.insert(&field.key, replacement);
                }
            }
        }
    }
    Ok(document.to_string())
}

/// The file after an update: what was written (or left alone) and whether
/// the warm engine must be reloaded for it.
struct Saved {
    file: ConfigFile,
    restart_engine: bool,
}

/// A refused field update: the fields that changed since the request's
/// revision, and the file as it is now.
#[derive(Debug)]
struct Conflict {
    fields: Vec<String>,
    current: Box<ConfigFile>,
}

/// Why an update wrote nothing.
#[derive(Debug)]
enum UpdateError {
    /// The request cannot be applied: not a `section.key` path, an
    /// unsupported value, or a merged file the config loader rejects.
    Invalid(String),
    /// A field update's field changed since its revision.
    Conflict(Conflict),
    /// The file could not be read, loaded or written.
    Failed(String),
    /// Another writer held the lock for longer than `WRITER_WAIT`.
    Busy,
}

impl UpdateError {
    /// The `{"error"}` answer, by recovery: fix the request (400), review
    /// the current file (409), try again (500, 503). `put_config` answers a
    /// conflict with the current config instead (`conflict_response`).
    fn into_api_error(self) -> ApiError {
        match self {
            Self::Invalid(message) => ApiError::bad_request(message),
            Self::Conflict(conflict) => {
                let reason = changed_elsewhere(&conflict.fields);
                ApiError(StatusCode::CONFLICT, reason)
            }
            Self::Failed(message) => ApiError::internal(message),
            Self::Busy => ApiError(StatusCode::SERVICE_UNAVAILABLE, WRITERS_BUSY.to_owned()),
        }
    }
}

/// `UpdateError::Invalid` for any error text.
fn invalid(error: impl ToString) -> UpdateError {
    UpdateError::Invalid(error.to_string())
}

/// `UpdateError::Failed` for any error text.
fn failed(error: impl ToString) -> UpdateError {
    UpdateError::Failed(error.to_string())
}

/// The lock file of the writers of the config at `path`: `<name>.toml.lock`
/// next to it, like the temp file of `write_private`.
fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("toml.lock")
}

/// Take the writers' lock of the config at `path`, waiting at most `wait`.
///
/// The lock is an exclusive `flock` on the lock file, created 0600 and
/// never removed so that every writer locks the same file. The owning
/// handle releases it, also when its process dies. `WouldBlock` when
/// another writer holds it for longer than `wait`. Blocking: call it off
/// the runtime thread.
fn lock_writers(path: &Path, wait: Duration) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock = lock_path(path);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&lock).map_err(|error| {
        let reason = format!("cannot open {}: {error}", lock.display());
        io::Error::new(error.kind(), reason)
    })?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(WRITER_POLL);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, WRITERS_BUSY));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
}

pub(crate) fn patch_cli(path: &Path, body: &Value) -> Result<(), String> {
    let edit = requested_edit(body)?;
    save_config(path, &BASELINES, &edit)
        .map(|_| ())
        .map_err(|error| error.into_api_error().1)
}

pub(crate) fn lock_revision(path: &Path, expected: &str) -> Result<File, String> {
    let writers = lock_writers(path, Duration::ZERO).map_err(|error| error.to_string())?;
    if saved_revision(path).map_err(|error| error.to_string())? != expected {
        return Err("a newer revision was saved before these settings could be applied".to_owned());
    }
    Ok(writers)
}

pub(crate) fn restore_revision(
    path: &Path,
    expected: &str,
    active: &ConfigFile,
) -> Result<(), String> {
    let _writers = lock_writers(path, Duration::ZERO).map_err(|error| error.to_string())?;
    let before = read_text(path).map_err(|error| error.to_string())?;
    if revision_of(&before) != expected {
        return Err("settings changed before restore; refresh and try again".to_owned());
    }
    if before != active.text
        && !write_private(path, &before, &active.text).map_err(|error| error.to_string())?
    {
        return Err("settings changed before restore; refresh and try again".to_owned());
    }
    Ok(())
}

/// Apply `edit` to the config at `path` for every writer of that file.
///
/// The writers' lock is held from the read to the rename, so two writers
/// never read the same version and write in turn: the second reads what the
/// first wrote. A file editor takes no lock, so the file is compared with
/// what was read right before the rename, and the attempt starts over when
/// it differs (`ATTEMPTS` times at most). The file is replaced only with a
/// text the config loader accepts, through `write_private`.
fn save_config(
    path: &Path,
    baselines: &Mutex<Baselines>,
    edit: &Edit,
) -> Result<Saved, UpdateError> {
    let _writers = lock_writers(path, WRITER_WAIT).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            UpdateError::Busy
        } else {
            failed(error)
        }
    })?;
    for _ in 0..ATTEMPTS {
        let file = read_config(path, baselines).map_err(failed)?;
        let text = match edit.plan(&file, baselines)? {
            Plan::Unchanged => {
                return Ok(Saved {
                    file,
                    restart_engine: false,
                })
            }
            Plan::Conflict(fields) => {
                return Err(UpdateError::Conflict(Conflict {
                    fields,
                    current: Box::new(file),
                }))
            }
            Plan::Save(text) => text,
        };
        let config = AppConfig::from_toml(&text).map_err(invalid)?;
        if write_private(path, &file.text, &text).map_err(failed)? {
            let saved = ConfigFile {
                revision: revision_of(&text),
                text,
                config,
            };
            let restart_engine = needs_engine_reload(&file.config, &saved.config);
            baselines
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remember(&saved)
                .map_err(failed)?;
            return Ok(Saved {
                file: saved,
                restart_engine,
            });
        }
    }
    Err(UpdateError::Failed(
        "config.toml kept changing while it was saved; nothing was written".to_owned(),
    ))
}

/// JSON → `toml_edit::Item`. A non-empty array whose elements are all
/// objects becomes an array of tables (`[[text.replacements]]` blocks, the
/// way the rest of the config is written); anything else is a plain value
/// via `to_value`.
fn to_item(value: &Value) -> Result<Item, String> {
    if let Value::Array(items) = value {
        if !items.is_empty() && items.iter().all(Value::is_object) {
            let mut tables = ArrayOfTables::new();
            for item in items {
                let mut table = Table::new();
                for (key, value) in item.as_object().expect("checked above") {
                    table.insert(key, to_item(value)?);
                }
                tables.push(table);
            }
            return Ok(Item::ArrayOfTables(tables));
        }
    }
    Ok(Item::Value(to_value(value)?))
}

/// JSON → inline TOML value: bools, integers (any JSON number that fits
/// `i64`, otherwise a float), strings, arrays and inline tables. `null` is
/// only meaningful as a key's whole value (handled by `patched`), so it
/// is an error anywhere nested.
fn to_value(value: &Value) -> Result<toml_edit::Value, String> {
    Ok(match value {
        Value::Bool(flag) => (*flag).into(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.into(),
            None => number
                .as_f64()
                .ok_or_else(|| format!("unsupported number {number}"))?
                .into(),
        },
        Value::String(text) => text.as_str().into(),
        Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(to_value(item)?);
            }
            toml_edit::Value::Array(array)
        }
        Value::Object(fields) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in fields {
                table.insert(key, to_value(value)?);
            }
            toml_edit::Value::InlineTable(table)
        }
        Value::Null => return Err("null is only allowed as a key's whole value".to_owned()),
    })
}

/// Sync a 0600 temporary file next to the target. Compare again after the
/// write so a manual edit during the write can be incorporated on retry.
fn write_private(path: &Path, expected: &str, text: &str) -> Result<bool, ConfigError> {
    use std::io::Write;
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::Builder::new()
        .prefix(".voiceless-config-")
        .tempfile_in(directory)?;
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    if read_text(path)? != expected {
        return Ok(false);
    }
    file.persist(path).map_err(|error| error.error)?;
    Ok(true)
}

/// `POST /api/text/preview` (console API "Config"): body `{"text",
/// "config"?}`; runs the `[text]` cleanup over `text` and answers
/// `{"ok": true, "text": cleaned}`. `config` is a `TextConfig` object
/// (missing fields default, unknown ones are rejected) so the page can
/// preview unsaved rules; absent or `null` uses the saved section. 400
/// when `config` does not deserialize or a regex rule does not compile
/// (the message quotes the pattern, which the page uses to highlight the
/// row), 500 if the saved config cannot be read. Nothing is written.
pub async fn preview(State(state): State<Arc<ConsoleState>>, Json(body): Json<Value>) -> ApiResult {
    let text = body["text"].as_str().unwrap_or_default();
    let app = state.config()?;
    let config = match body.get("config") {
        Some(config) if !config.is_null() => {
            serde_json::from_value::<TextConfig>(config.clone()).map_err(ApiError::bad_request)?
        }
        _ => app.text.clone(),
    };
    crate::text::vocabulary::validate(&config.scopes).map_err(ApiError::bad_request)?;
    let config = crate::text::vocabulary::effective(
        &config,
        body["mode"].as_str(),
        body["project"].as_str(),
    );
    let cleaner = crate::text::TextCleaner::new(&config).map_err(ApiError::bad_request)?;
    Ok(Json(json!({
        "ok": true,
        "text": cleaner.clean(text),
        "recognition_bias": crate::text::vocabulary::recognition_bias(&app),
        "backend": app.asr.streaming_backend,
        "model": (app.asr.streaming_backend == "local-offline").then_some(app.local.model),
    })))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VocabularyImport {
    base: crate::text::vocabulary::Terms,
    incoming: crate::text::vocabulary::Terms,
}

pub async fn merge_vocabulary(Json(mut request): Json<VocabularyImport>) -> ApiResult {
    let report = request
        .base
        .import(request.incoming)
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "terms": request.base, "report": report })))
}

/// `GET /api/prompt` (console API "Modes"): `{"prompt", "model",
/// "provider"}` for the organize ("prompt-optimize") mode. `prompt` is
/// `[processing] prompt`, or the built-in optimize prompt when that is
/// blank, so the page always shows the template in effect. 500 if the
/// config cannot be read.
pub async fn get_prompt(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let config = state.config()?;
    let prompt = if config.processing.prompt.trim().is_empty() {
        crate::modes::builtin_prompt_optimize_prompt()
    } else {
        config.processing.prompt.clone()
    };
    Ok(Json(json!({
        "prompt": prompt,
        "model": config.processing.model,
        "provider": config.processing.provider,
    })))
}

/// `POST /api/prompt` (console API "Modes"): body `{"prompt": "…"}`,
/// written to `[processing] prompt` through the shared writer (comments
/// kept, the writers' lock held). 400 when `prompt` is not a string or the
/// result fails validation, 500 when the file cannot be read or written,
/// 503 while another writer holds it. Saving an empty string makes
/// `get_prompt` fall back to the built-in.
pub async fn save_prompt(
    State(state): State<Arc<ConsoleState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    let prompt = body["prompt"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("missing `prompt` string field"))?
        .to_owned();
    let edit = Edit::Overwrite(vec![Field {
        section: "processing".to_owned(),
        key: "prompt".to_owned(),
        value: Value::String(prompt),
    }]);
    let path = state.config_path.clone();
    tokio::task::spawn_blocking(move || save_config(&path, &BASELINES, &edit))
        .await
        .map_err(ApiError::internal)?
        .map_err(UpdateError::into_api_error)?;
    Ok(Json(json!({ "ok": true })))
}

/// Open `modes.json` (built-ins only, in memory, when the file does not
/// exist yet); 500 if it cannot be read or fails validation.
fn modes() -> Result<ModesRepository, ApiError> {
    ModesRepository::open(crate::stream::modes_path()).map_err(ApiError::internal)
}

/// `ModesError` → status: unknown id 404; editing/deleting a built-in or
/// a duplicate name 409; empty name 400; file errors 500.
fn mode_error(error: ModesError) -> ApiError {
    let status = match error {
        ModesError::NotFound(_) => StatusCode::NOT_FOUND,
        ModesError::BuiltinImmutable(_) | ModesError::DuplicateName(_) => StatusCode::CONFLICT,
        ModesError::EmptyName => StatusCode::BAD_REQUEST,
        ModesError::Read(..) | ModesError::Write(..) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError(status, error.to_string())
}

/// String field of a JSON body, `None` when absent or not a string.
fn field<'a>(body: &'a Value, name: &str) -> Option<&'a str> {
    body.get(name).and_then(Value::as_str)
}

/// `GET /api/modes` (console API "Modes"): every mode as
/// `[{"id","name","prompt","processing_label","builtin","sort_order"}]`
/// in repository order (the page sorts by `sort_order`). 500 if
/// `modes.json` is unreadable.
pub async fn list_modes() -> ApiResult {
    Ok(Json(
        serde_json::to_value(modes()?.list()).map_err(ApiError::internal)?,
    ))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPreview {
    text: String,
    #[serde(default)]
    mode: String,
    app_id: Option<String>,
    site: Option<String>,
    project: Option<String>,
    output: Option<crate::output::OutputConfig>,
    #[serde(default)]
    send_model: bool,
}

/// Preview uses the same pipeline as live dictation without capture or delivery.
pub async fn preview_output(
    State(state): State<Arc<ConsoleState>>,
    Json(request): Json<OutputPreview>,
) -> ApiResult {
    let mut config = state.config()?;
    if let Some(output) = request.output {
        output.validate().map_err(ApiError::bad_request)?;
        config.output = output;
    }
    let default = state
        .daemon
        .as_ref()
        .map_or("quick", |daemon| daemon.default_mode.as_str());
    let pipeline = crate::output::Pipeline::prepare(
        &config,
        &request.mode,
        default,
        crate::output::Context {
            app_id: request.app_id.as_deref(),
            app_source: Some("preview"),
            site: request.site.as_deref(),
            site_source: Some("preview"),
            project: request.project.as_deref(),
        },
    )
    .map_err(ApiError::bad_request)?;
    let result = pipeline.run(&request.text, true, request.send_model).await;
    serde_json::to_value(result)
        .map(Json)
        .map_err(ApiError::internal)
}

/// `POST /api/modes` (console API "Modes"): body `{"name", "prompt",
/// "processing_label"}` (missing strings default to empty) → the created
/// mode with its new id. 400 for an empty name, 409 for a duplicate name,
/// 500 on read/write errors.
pub async fn add_mode(Json(body): Json<Value>) -> ApiResult {
    let mode = modes()?
        .add(
            field(&body, "name").unwrap_or_default(),
            field(&body, "prompt").unwrap_or_default(),
            field(&body, "processing_label").unwrap_or_default(),
        )
        .map_err(mode_error)?;
    Ok(Json(
        serde_json::to_value(mode).map_err(ApiError::internal)?,
    ))
}

/// `PUT /api/modes/{id}` (console API "Modes"): any subset of `{"name",
/// "prompt", "processing_label"}`; absent fields are left as they were.
/// Returns the updated mode. 404 unknown id, 409 built-in or duplicate
/// name, 400 empty name, 500 on read/write errors.
pub async fn update_mode(UrlPath(id): UrlPath<String>, Json(body): Json<Value>) -> ApiResult {
    let mode = modes()?
        .update(
            &id,
            field(&body, "name"),
            field(&body, "prompt"),
            field(&body, "processing_label"),
        )
        .map_err(mode_error)?;
    Ok(Json(
        serde_json::to_value(mode).map_err(ApiError::internal)?,
    ))
}

/// `DELETE /api/modes/{id}` (console API "Modes"): `{"ok": true}`; 404
/// unknown id, 409 for a built-in, 500 on read/write errors.
pub async fn remove_mode(UrlPath(id): UrlPath<String>) -> ApiResult {
    modes()?.remove(&id).map_err(mode_error)?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests;
