//! Settings endpoints: config (comment-preserving TOML edits, write-only
//! secrets), text-cleanup preview, organize prompt and modes.

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

/// `(section, key)` pairs never returned to the page.
const SECRETS: &[(&str, &str)] = &[
    ("cloud", "api_key"),
    ("doubao", "api_key"),
    ("doubao", "access_key"),
    ("processing", "api_key"),
];

fn is_secret(section: &str, key: &str) -> bool {
    SECRETS.contains(&(section, key))
}

/// The config as JSON with secrets blanked and `has_<key>` flags added.
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

pub async fn get_config(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    Ok(Json(masked(&state.config()?)?))
}

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

fn modes() -> Result<ModesRepository, ApiError> {
    ModesRepository::open(crate::stream::modes_path()).map_err(ApiError::internal)
}

fn mode_error(error: ModesError) -> ApiError {
    let status = match error {
        ModesError::NotFound(_) => StatusCode::NOT_FOUND,
        ModesError::BuiltinImmutable(_) | ModesError::DuplicateName(_) => StatusCode::CONFLICT,
        ModesError::EmptyName => StatusCode::BAD_REQUEST,
        ModesError::Read(..) | ModesError::Write(..) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError(status, error.to_string())
}

fn field<'a>(body: &'a Value, name: &str) -> Option<&'a str> {
    body.get(name).and_then(Value::as_str)
}

pub async fn list_modes() -> ApiResult {
    Ok(Json(
        serde_json::to_value(modes()?.list()).map_err(ApiError::internal)?,
    ))
}

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

pub async fn remove_mode(UrlPath(id): UrlPath<String>) -> ApiResult {
    modes()?.remove(&id).map_err(mode_error)?;
    Ok(Json(json!({ "ok": true })))
}
