//! Settings endpoints: config (comment-preserving TOML edits, write-only
//! secrets), text-cleanup preview, organize prompt and modes.
//!
//! Mounted under `/api` by `super::router`; the "Config" and "Modes" tables
//! of `docs/console-api.md`. None of these need the daemon, so they work
//! under `voiceless history serve` too. Files: `config.toml` at
//! `ConsoleState::config_path` (read on every request; patched with
//! `toml_edit` so the user's comments and layout survive; written 0600
//! through a temp file and rename) and `modes.json` next to it
//! (`crate::stream::modes_path`, via `ModesRepository`).
//!
//! Invariants: the four `SECRETS` are never returned (`masked` blanks them
//! and adds `has_<key>`); a patch is validated with the normal config
//! loader before anything is written, so an invalid value leaves the file
//! untouched (400); after a write the caller learns from `restart_engine`
//! whether the daemon must reload (`POST /api/engine/reload`) for the
//! change to take effect, as opposed to the next session picking it up.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use super::{ApiError, ApiResult, ConsoleState};
use crate::config::{AppConfig, TextConfig};
use crate::modes::{ModesError, ModesRepository};

/// `(section, key)` pairs never returned to the page: blanked by `masked`,
/// and an empty string written for them by `patch_file` means "keep".
const SECRETS: &[(&str, &str)] = &[
    ("cloud", "api_key"),
    ("doubao", "api_key"),
    ("doubao", "access_key"),
    ("processing", "api_key"),
];

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
/// with secrets masked. 200, or 500 when `config.toml` cannot be loaded
/// (invalid TOML, unknown key, or it holds keys while readable by others).
pub async fn get_config(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    Ok(Json(masked(&state.config()?)?))
}

/// `PUT /api/config` (console API "Config"): body is a partial config
/// `{section: {key: value}}`, merged into `config.toml` by `patch_file`.
/// Answers `{"ok": true, "restart_engine": bool}`, the flag coming from a
/// before/after comparison of the engine and hotkey settings. 400 with the
/// loader's message for an invalid patch (nothing written), 500 if the
/// current file cannot be read. The page sends only the sections it
/// changed and strips the `has_*` flags, which are ignored here anyway.
pub async fn put_config(
    State(state): State<Arc<ConsoleState>>,
    Json(patch): Json<Value>,
) -> ApiResult {
    let before = state.config()?;
    let after = patch_file(&state.config_path, &patch).map_err(ApiError::bad_request)?;
    Ok(Json(json!({
        "ok": true,
        "restart_engine": needs_engine_reload(&before, &after),
    })))
}

/// Settings the warm engine or hotkey listener was built from.
///
/// `[asr] streaming_backend`, the whole `[local]` section, `[text]
/// hotwords` (biased into the local decoder) and the whole `[hotkey]`
/// section; compared as JSON so nested lists count. Everything else is
/// read afresh by the next session.
fn needs_engine_reload(before: &AppConfig, after: &AppConfig) -> bool {
    let engine = |config: &AppConfig| {
        json!([
            config.asr.streaming_backend,
            config.local,
            config.text.hotwords,
            config.hotkey,
        ])
    };
    engine(before) != engine(after)
}

/// Merge `patch` (`{section: {key: value}}`) into the TOML file at `path`,
/// keeping its comments and layout, validate the result with the normal
/// loader and write it with 0600 permissions. `null` removes a key; an
/// empty string for a secret keeps the stored one.
///
/// A missing file starts from an empty document (sections are created as
/// needed); an existing file must parse as TOML. Keys starting with `has_`
/// are skipped (the page echoes the read-only flags back). Removing a key
/// is how the page clears a secret, and it also reverts any other key to
/// its default. Values go through `to_item`: arrays of objects become
/// `[[section.key]]` tables, other arrays and objects stay inline. Errors
/// are user-facing strings (callers map them to 400): a patch that is not
/// an object of sections, a section that is not a table, an unsupported
/// value, the loader's validation message, or an I/O error. Returns the
/// validated config so the caller can diff it. Not locked against
/// concurrent writers; the rename is atomic, so a reader never sees a
/// half-written file.
pub fn patch_file(path: &Path, patch: &Value) -> Result<AppConfig, String> {
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let mut document: DocumentMut = original
        .parse()
        .map_err(|error| format!("config.toml is not valid TOML: {error}"))?;
    let sections = patch
        .as_object()
        .ok_or_else(|| "the patch must be an object of sections".to_owned())?;
    for (section, keys) in sections {
        let keys = keys
            .as_object()
            .ok_or_else(|| format!("[{section}] must be an object"))?;
        let table = document
            .entry(section)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| format!("[{section}] is not a table in config.toml"))?;
        for (key, value) in keys {
            if key.starts_with("has_") {
                continue; // read-only flags echoed back by the page
            }
            match value {
                Value::Null => {
                    table.remove(key);
                }
                Value::String(secret) if secret.is_empty() && is_secret(section, key) => {}
                value => {
                    table.insert(key, to_item(value)?);
                }
            }
        }
    }
    let text = document.to_string();
    let config = AppConfig::from_toml(&text).map_err(|error| error.to_string())?;
    write_private(path, &text).map_err(|error| error.to_string())?;
    Ok(config)
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
/// only meaningful as a key's whole value (handled by `patch_file`), so it
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

/// Atomic write with 0600: the file holds API keys.
///
/// Creates the parent dir, writes `<name>.toml.tmp` next to the target with
/// mode 0600 (so the key is never world-readable, not even for an instant),
/// fsyncs, and renames over `path`. Keeping the temp file in the same dir
/// keeps the rename on one filesystem. Renaming over an existing 0644 file
/// leaves a 0600 file, which is what `AppConfig::load` requires.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("toml.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
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
    let config = match body.get("config") {
        Some(config) if !config.is_null() => {
            serde_json::from_value::<TextConfig>(config.clone()).map_err(ApiError::bad_request)?
        }
        _ => state.config()?.text,
    };
    let cleaner = crate::text::TextCleaner::new(&config).map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "ok": true, "text": cleaner.clean(text) })))
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
/// written to `[processing] prompt` through `patch_file` (comments kept).
/// 400 when `prompt` is not a string or the result fails validation.
/// Saving an empty string makes `get_prompt` fall back to the built-in.
pub async fn save_prompt(
    State(state): State<Arc<ConsoleState>>,
    Json(body): Json<Value>,
) -> ApiResult {
    let prompt = body["prompt"]
        .as_str()
        .ok_or_else(|| ApiError::bad_request("missing `prompt` string field"))?;
    patch_file(
        &state.config_path,
        &json!({ "processing": { "prompt": prompt } }),
    )
    .map_err(ApiError::bad_request)?;
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
