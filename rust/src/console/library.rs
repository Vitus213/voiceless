//! Library endpoints: model catalog, installs and GGUF imports, history
//! records (listing, search, facets and CSV export) and recordings,
//! re-transcription with the warm engine, correction suggestions and the
//! replay benchmark.
//! Mounted under `/api` by `super::router`; the "Models", "History" and
//! "Diagnostics" tables of `docs/console-api.md`. Touches the model store
//! (`ModelManager` over `<data dir>/models` with `versions/` beneath it and
//! `<cache dir>/model-downloads` for partial downloads; the dirs default to
//! `~/.local/share/voiceless` and `~/.cache/voiceless`), the history
//! database and the WAV recordings its rows point at, and `config.toml`
//! (`[text]`, for the cleanup applied to re-transcriptions).
//!
//! Re-transcription uses the daemon's background decoder budget and loads
//! the selected local model. The benchmark requires a warm offline engine.
//! Audio is read from the path stored in the record.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{ApiError, ApiResult, ConsoleState};
use crate::history::{HistoryError, HistoryFilter, HistoryRecord, HistoryStore};
use crate::models::{
    catalog, default_cache_dir, default_data_dir, import_filename, model_info, HttpDownloader,
    ImportedModel, ModelError, ModelManager, ModelSpec,
};
use crate::transcription_jobs::{PcmSource, SharedResource, WavSource};

/// Background model installs: id → last error (`None` while running).
///
/// Shared through `ConsoleState`. An entry is inserted when an install is
/// accepted, removed on success and replaced by the error message on
/// failure, so `list_models` can report `installing` / `error` and a
/// second `POST .../install` for a running id is refused with 409. A
/// poisoned mutex is recovered, not propagated.
#[derive(Default)]
pub struct Installs(Mutex<HashMap<String, Option<String>>>);

/// A `ModelManager` over the default data and cache dirs; it holds only
/// paths, so one per call is fine.
fn manager() -> ModelManager {
    ModelManager::new(&default_data_dir(), &default_cache_dir())
}

/// Catalog id → the `kind` reported to the page: `gguf` (transcribe.cpp
/// GPU models, ids ending in `-gguf`), `vad` (`silero-vad`), `denoiser`
/// (`gtcrn`), `streaming` (Paraformer) or `sherpa` (offline ASR).
fn kind(id: &str) -> &'static str {
    if id.ends_with("-gguf") {
        "gguf"
    } else if id == "silero-vad" {
        "vad"
    } else if id == "gtcrn" {
        "denoiser"
    } else if id.starts_with("streaming-") {
        "streaming"
    } else {
        "sherpa"
    }
}

/// Catalog ids the daemon's warm engine has loaded (model plus VAD); empty
/// without a daemon or without a local engine. These cannot be removed.
fn models_in_use(state: &ConsoleState) -> Vec<String> {
    state
        .daemon
        .as_ref()
        .and_then(|daemon| daemon.warm())
        .map(|engine| engine.model_ids())
        .unwrap_or_default()
}

/// One entry of `GET /api/models`: what `model_info` says about the model
/// (`name`, `languages`, `devices`) over what the store holds for `spec`.
/// `install` is its `Installs` entry and `imported` the imported model
/// behind it (`None` for a catalog entry).
fn model_json(
    manager: &ModelManager,
    spec: &ModelSpec,
    imported: Option<&ImportedModel>,
    install: Option<&Option<String>>,
    in_use: &[String],
) -> Value {
    let info = model_info(spec, imported);
    let installing = matches!(install, Some(None));
    // Download ratio while bytes are still arriving; `null` once the file
    // is complete and the installer is verifying/extracting, so the page
    // can show an indeterminate "verifying" state.
    let progress = installing
        .then(|| {
            std::fs::metadata(manager.download_path(spec))
                .map(|metadata| metadata.len() as f64 / spec.size_bytes as f64)
                .unwrap_or(0.0)
        })
        .filter(|ratio| *ratio < 1.0);
    json!({
        "id": spec.id,
        "name": info.name,
        "version": spec.version,
        "size_bytes": spec.size_bytes,
        "license_status": spec.license_status,
        "installed": matches!(manager.resolve(&spec.id), Ok(Some(_))),
        "installing": installing,
        "progress": progress,
        "error": install.cloned().flatten(),
        "kind": kind(&spec.id),
        "in_use": in_use.contains(&spec.id),
        "languages": info.languages,
        "devices": info.devices,
        "imported": imported.is_some(),
    })
}

/// `GET /api/models` (console API "Models"): every catalog entry and
/// every imported GGUF model with `installed` (an intact `current`
/// pointer in the store; a broken one reads as not installed),
/// `installing`, `progress` (bytes of the `.partial` download over
/// `size_bytes`, 0..1, only while installing; it sits at 1.0 while the
/// archive is verified and extracted, and is never `null` during an
/// install), `error` from the last failed install, `kind`, `in_use` and
/// `imported`, plus what the page lists and filters by: `name`,
/// `languages` and `devices` from `model_info`. The devices are those
/// this build was compiled for; where a loaded model really runs is the
/// engine's report in the status. Works without a daemon (`in_use` is
/// then always false); 500 only when the store cannot list the imported
/// models.
pub async fn list_models(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let in_use = models_in_use(&state);
    // Snapshot the map so the lock is not held across the filesystem probes.
    let installs = state
        .installs
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let models =
        tokio::task::spawn_blocking(move || {
            let manager = manager();
            let imported = manager.list_imported()?;
            let mut models: Vec<Value> = catalog()
                .iter()
                .map(|spec| model_json(&manager, spec, None, installs.get(&spec.id), &in_use))
                .collect();
            models.extend(imported.iter().map(|imported| {
                model_json(&manager, &imported.spec, Some(imported), None, &in_use)
            }));
            Ok::<_, ModelError>(models)
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(format!("cannot list imported models: {error}")))?;
    Ok(Json(json!(models)))
}

/// `POST /api/models/{id}/install` (console API "Models"): starts a
/// background install of a catalog model and answers `{"ok": true}` at
/// once; the page polls `GET /api/models` for progress. 404 for an id not
/// in the catalog, 409 if that id is already installing. The install
/// (download to the cache dir, SHA-256 and size check, extraction,
/// activation) runs on the blocking pool; its outcome is recorded in
/// `Installs` and, with a daemon, logged as `model <id> installed`. A
/// failed install leaves its error for `list_models` and can be retried.
pub async fn install_model(
    State(state): State<Arc<ConsoleState>>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult {
    let spec = catalog()
        .into_iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown model id: {id}")))?;
    {
        let mut installs = state
            .installs
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if matches!(installs.get(&id), Some(None)) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("{id} is already installing"),
            ));
        }
        // Reserve the id under the lock so two clicks cannot start two downloads.
        installs.insert(id.clone(), None);
    }
    let console = state.clone();
    // Detached: the response does not wait, and a client that navigates away
    // does not cancel the download.
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || manager().install(&spec, &HttpDownloader))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
        let mut installs = console
            .installs
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match result {
            Ok(_) => {
                installs.remove(&id);
                if let Some(daemon) = &console.daemon {
                    daemon.log(format!("Voiceless: model {id} installed"));
                }
            }
            Err(error) => {
                installs.insert(id, Some(error));
            }
        }
    });
    Ok(Json(json!({ "ok": true })))
}

/// `DELETE /api/models/{id}` (console API "Models"): deletes the model's
/// pointer and every stored version. 409 while the daemon's warm engine
/// has it loaded (switch models and reload first) or while the store
/// holds it for a running operation (`ModelError::InUse` / `Busy`, with
/// the store's reason), 404 if nothing is installed under that id, 500 on
/// filesystem errors; `{"ok": true}` otherwise. No catalog check: any
/// installed id, imported or not, can be removed.
pub async fn remove_model(
    State(state): State<Arc<ConsoleState>>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult {
    if models_in_use(&state).contains(&id) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("{id} is loaded by the engine; switch models and reload first"),
        ));
    }
    let removed = manager().remove(&id).map_err(|error| match error {
        ModelError::InUse(message) | ModelError::Busy(message) => {
            ApiError(StatusCode::CONFLICT, message)
        }
        error => ApiError::internal(error),
    })?;
    if !removed {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{id} is not installed"),
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

/// Header naming the file behind an import: its base name,
/// percent-encoded as `encodeURIComponent` leaves it.
const IMPORT_FILENAME: &str = "x-voiceless-filename";

/// Body frames the channel holds for the writer besides the one it is
/// writing; bounds what a multi-gigabyte upload keeps in memory.
const IMPORT_FRAMES_IN_FLIGHT: usize = 4;

/// `POST /api/models/import` (console API "Models"): imports a GGUF model
/// from a file on this machine. The body is the file's raw bytes with a
/// `Content-Length`; `X-Voiceless-Filename` optionally names the file.
/// Answers `{"ok": true, "id"}` once the model is stored and verified,
/// with the content-addressed `-gguf` id that `[local].model` takes.
///
/// The bytes are streamed into a temporary file of the model store
/// (`begin_import`, which first checks that the disk can hold the
/// declared size) and that file is handed to `finish_import`: nothing is
/// buffered whole or copied. A body that fails or ends short of its
/// `Content-Length` is an interrupted transfer (400) and the partial
/// file is removed. 411 without a `Content-Length`, 400 for an empty
/// upload or a name that names a path, 409 for a model another operation
/// holds, 507 when the disk is full, 422 for bytes that are not a model
/// this build can run, 500 for other store failures. A refusal before any
/// byte is stored is answered only after the body was read to its end
/// (`drain`). With a daemon the import is logged as `model <id>
/// imported`. The blocking steps run on the blocking pool, and the
/// request stays open for as long as the upload and the verification
/// take.
pub async fn import_model(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    let (file, bytes, name) = match start_import(&headers).await {
        Ok(started) => started,
        Err(error) => return Err(drain(body, error).await),
    };
    let file = receive(file, body, bytes).await?;
    let imported = tokio::task::spawn_blocking(move || manager().finish_import(file, &name, bytes))
        .await
        .map_err(ApiError::internal)?
        .map_err(import_error)?;
    if let Some(daemon) = &state.daemon {
        daemon.log(format!("Voiceless: model {} imported", imported.spec.id));
    }
    Ok(Json(json!({ "ok": true, "id": imported.spec.id })))
}

/// Check the request and reserve room for it: the declared size, the
/// name and the temporary file to receive the bytes in.
async fn start_import(
    headers: &HeaderMap,
) -> Result<(tempfile::NamedTempFile, u64, String), ApiError> {
    let bytes = content_length(headers)?;
    let name = import_name(headers)?;
    let file = tokio::task::spawn_blocking(move || manager().begin_import(bytes))
        .await
        .map_err(ApiError::internal)?
        .map_err(import_error)?;
    Ok((file, bytes, name))
}

/// The size the client declares for the upload. Required: it is reserved
/// on disk before the first byte is written, and the bytes that arrive
/// are counted against it.
pub(super) fn content_length(headers: &HeaderMap) -> Result<u64, ApiError> {
    headers
        .get(header::CONTENT_LENGTH)
        .ok_or_else(|| {
            ApiError(
                StatusCode::LENGTH_REQUIRED,
                "Content-Length is required".to_owned(),
            )
        })?
        .to_str()
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| ApiError::bad_request("Content-Length must be a positive number of bytes"))
}

/// The name an imported model is listed under: `X-Voiceless-Filename`,
/// decoded by the store's `import_filename`. Empty without the header and
/// when the value gives no usable name (it does not decode, or is too
/// long): the store then names the model after its own metadata, as for
/// a CLI import, so a name never keeps a valid file out. A value that
/// names a location (a slash or backslash, raw or escaped) is refused
/// rather than shortened: no request ever carries a path on this
/// machine.
fn import_name(headers: &HeaderMap) -> Result<String, ApiError> {
    let invalid = |reason: &str| ApiError::bad_request(format!("X-Voiceless-Filename {reason}"));
    let Some(value) = headers.get(IMPORT_FILENAME) else {
        return Ok(String::new());
    };
    let encoded = value
        .to_str()
        .map_err(|_| invalid("must be ASCII with the name percent-encoded"))?;
    let lower = encoded.to_ascii_lowercase();
    if encoded.contains(['/', '\\']) || lower.contains("%2f") || lower.contains("%5c") {
        return Err(invalid("must be a file name, not a path"));
    }
    Ok(import_filename(encoded).unwrap_or_default())
}

/// Write the request body to `file`; the file back once the body ended
/// whole, or the interrupted transfer.
///
/// Frames are read on the runtime and handed over a bounded channel to
/// one blocking task that writes them, so the file grows as the bytes
/// arrive and a few frames are all that is held. When the request is
/// dropped (the client went away) the channel closes, that task returns
/// the file and dropping it removes the partial copy. A body that fails
/// before `expected` bytes, or ends early, is reported with the count
/// that did arrive.
pub(super) async fn receive(
    file: tempfile::NamedTempFile,
    body: Body,
    expected: u64,
) -> Result<tempfile::NamedTempFile, ApiError> {
    let (sender, mut receiver) = mpsc::channel::<Bytes>(IMPORT_FRAMES_IN_FLIGHT);
    let writer = tokio::task::spawn_blocking(move || {
        let mut file = file;
        let mut written = 0_u64;
        while let Some(frame) = receiver.blocking_recv() {
            file.write_all(&frame)?;
            written += frame.len() as u64;
        }
        Ok::<_, io::Error>((file, written))
    });
    let mut stream = body.into_data_stream();
    let interrupted = loop {
        match stream.next().await {
            Some(Ok(frame)) => {
                if sender.send(frame).await.is_err() {
                    break None; // The writer failed; its error is the answer.
                }
            }
            Some(Err(error)) => break Some(error.to_string()),
            None => break None,
        }
    };
    // Closing the channel lets the writer write what it holds and return.
    drop(sender);
    let (file, written) = writer
        .await
        .map_err(ApiError::internal)?
        .map_err(disk_error)?;
    match interrupted {
        Some(reason) => Err(ApiError::bad_request(format!(
            "transfer interrupted after {written} of {expected} bytes: {reason}"
        ))),
        None if written != expected => Err(ApiError::bad_request(format!(
            "received {written} bytes, Content-Length declared {expected}"
        ))),
        None => Ok(file),
    }
}

/// Read `body` to its end, then hand back `error`: the answer to an
/// import refused before its bytes were stored. A browser that is still
/// uploading would otherwise see the connection close and report a
/// network failure instead of the reason.
pub(super) async fn drain(body: Body, error: ApiError) -> ApiError {
    let mut stream = body.into_data_stream();
    while (stream.next().await).is_some() {}
    error
}

/// A refusal of the model store as the API error it is for an import:
/// 409 for a model another operation holds (`InUse`, `Busy`), 507 when
/// the disk cannot hold the file, 400 for a transfer shorter than its
/// `Content-Length` (`Incomplete`), 422 for bytes that are not a model
/// this build can run (`Integrity`, `Archive`, `Unsupported`, `Invalid`),
/// 500 otherwise.
fn import_error(error: ModelError) -> ApiError {
    match error {
        ModelError::InUse(message) | ModelError::Busy(message) => {
            ApiError(StatusCode::CONFLICT, message)
        }
        ModelError::NoSpace { .. } => ApiError(StatusCode::INSUFFICIENT_STORAGE, error.to_string()),
        ModelError::Incomplete { .. } => ApiError::bad_request(error.to_string()),
        ModelError::Integrity(..)
        | ModelError::Archive(_)
        | ModelError::Unsupported(_)
        | ModelError::Invalid(_) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
        ModelError::Io(error) => disk_error(error),
        error => ApiError::internal(error),
    }
}

/// An I/O failure while storing a model: 507 when the disk or the user's
/// quota is full, 500 otherwise.
pub(super) fn disk_error(error: io::Error) -> ApiError {
    if matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) {
        return ApiError(
            StatusCode::INSUFFICIENT_STORAGE,
            format!("no space left for the model: {error}"),
        );
    }
    ApiError::internal(error)
}

/// A `HistoryRecord` as the API's `HistoryRecord` JSON plus `has_audio`,
/// which checks that the referenced WAV still exists (the row keeps its
/// `audio_path` even after the file was removed by hand).
pub(super) fn record_json(record: &HistoryRecord) -> Value {
    let mut value = serde_json::to_value(record).unwrap_or_default();
    value["has_audio"] = json!(record
        .audio_path
        .as_deref()
        .is_some_and(|path| Path::new(path).is_file()));
    value
}

/// Query parameters of `GET /api/records` and `GET /api/records/export`:
/// `limit` and `cursor` page the former, the other six select the rows.
#[derive(serde::Deserialize)]
pub struct Page {
    /// Page size; default 200, clamped to 1..=1000.
    limit: Option<i64>,
    /// Opaque `next_cursor` from the previous page.
    cursor: Option<String>,
    /// `q`: substring of the raw or final text.
    #[serde(rename = "q")]
    text: Option<String>,
    /// `from`: created at or after this RFC 3339 time, on a whole second.
    from: Option<String>,
    /// `to`: created before such a time.
    to: Option<String>,
    /// `mode`: exact `processing_mode`.
    mode: Option<String>,
    /// `app`: exact `app_id`.
    app: Option<String>,
    /// `model`: exact `asr_model`.
    model: Option<String>,
    /// `audio`: `with` or `without` a saved recording.
    audio: Option<String>,
}

impl Page {
    /// The rows selected, as the store takes them: a parameter that is
    /// missing or blank selects anything.
    fn filter(&self) -> HistoryFilter {
        HistoryFilter {
            text: self.text.clone().unwrap_or_default(),
            from: self.from.clone(),
            to: self.to.clone(),
            mode: self.mode.clone(),
            app: self.app.clone(),
            model: self.model.clone(),
            audio: self.audio.clone(),
        }
    }
}

/// A failure of the history store as the API error it is: 400 for a
/// filter it rejects, 500 for everything else (a database failure, a
/// cursor that does not decode or was issued for another filter).
fn history_error(error: HistoryError) -> ApiError {
    match error {
        HistoryError::Filter(message) => ApiError::bad_request(message),
        error => ApiError::internal(error),
    }
}

/// Run `work` on the store in the blocking pool: a search, the facets or
/// an export may read the whole library, which must not stall the runtime.
async fn on_store<T: Send + 'static>(
    state: Arc<ConsoleState>,
    work: impl FnOnce(&HistoryStore) -> Result<T, HistoryError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || work(&state.store))
        .await
        .map_err(ApiError::internal)?
        .map_err(history_error)
}

/// `GET /api/records?limit=&cursor=&q=&from=&to=&mode=&app=&model=&audio=`
/// (console API "History"): the rows the filter selects, newest first,
/// `{"records": [HistoryRecord + has_audio], "next_cursor"}` where
/// `next_cursor` is `null` on the last page. Without the filter
/// parameters it lists every row as before. `limit` defaults to 200 and is
/// clamped to 1..=1000. `q` is a substring of the raw or final text, ASCII
/// letter case ignored; `from` is inclusive and `to` exclusive, both an
/// RFC 3339 time on a whole second; `mode`, `app` and `model` compare for
/// equality; `audio` is `with` or `without`. A cursor belongs to the
/// filter it was issued for and is refused under another. 400 for a filter
/// value the store rejects, 500 for database errors and for a cursor that
/// does not decode.
pub async fn records(
    State(state): State<Arc<ConsoleState>>,
    Query(page): Query<Page>,
) -> ApiResult {
    let filter = page.filter();
    let limit = page.limit.unwrap_or(200).clamp(1, 1_000);
    let cursor = page.cursor;
    let page = on_store(state, move |store| {
        store.search(&filter, limit, cursor.as_deref())
    })
    .await?;
    Ok(Json(json!({
        "records": page.records.iter().map(record_json).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor,
    })))
}

/// `GET /api/records/facets` (console API "History"): what the search's
/// menus offer, `{"modes": [{"value", "records"}], "apps": [...],
/// "models": [...]}`: every `processing_mode`, `app_id` and `asr_model`
/// the library holds, each with the rows carrying it, the most frequent
/// first and by value among equals. 500 on database errors.
pub async fn record_facets(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let facets = on_store(state, |store| store.facets()).await?;
    Ok(Json(
        serde_json::to_value(facets).map_err(ApiError::internal)?,
    ))
}

/// `GET /api/records/export?q=&from=&to=&mode=&app=&model=&audio=` (console
/// API "History"): every row the filter selects, in the order the list
/// shows and chosen by the same parameters, as `{"csv", "exported"}`.
/// The CSV is one header line and one line per record with the fields of
/// `HistoryRecord` in order (`id,created_at,…,audio_path,app_id`): a field
/// with a comma, a quote or a line break is quoted with its quotes
/// doubled, NULL fields are empty. `limit` and `cursor` mean nothing here.
/// 400 for a filter value the store rejects, 500 on database errors.
pub async fn export_records(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<Page>,
) -> ApiResult {
    let filter = query.filter();
    let (csv, exported) = on_store(state, move |store| {
        let mut csv = Vec::new();
        let exported = store.write_csv(&mut csv, &filter)?;
        Ok((csv, exported))
    })
    .await?;
    let csv = String::from_utf8(csv).map_err(ApiError::internal)?;
    Ok(Json(json!({ "csv": csv, "exported": exported })))
}

/// `GET /api/totals` (console API "History"):
/// `{"records", "characters", "duration_seconds"}` over the whole
/// database. 500 on database errors.
pub async fn totals(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    Ok(Json(
        serde_json::to_value(state.store.totals().map_err(ApiError::internal)?)
            .map_err(ApiError::internal)?,
    ))
}

/// Strict UUID shape: 36 chars, hex with dashes at 8/13/18/23. Rejecting
/// anything else up front keeps path-like or wildcard strings out of the
/// SQL lookup and makes the `.wav` suffix stripping in `audio` unambiguous.
fn is_record_id(id: &str) -> bool {
    id.len() == 36
        && id.as_bytes().iter().enumerate().all(|(index, byte)| {
            byte.is_ascii_hexdigit() || (matches!(index, 8 | 13 | 18 | 23) && *byte == b'-')
        })
}

/// Look a record up by id; a malformed id and an unknown one are the same
/// 404 `no such record`, a database error 500.
pub(super) fn find_record(state: &ConsoleState, id: &str) -> Result<HistoryRecord, ApiError> {
    let not_found = || ApiError(StatusCode::NOT_FOUND, "no such record".to_owned());
    if !is_record_id(id) {
        return Err(not_found());
    }
    state
        .store
        .get(id)
        .map_err(ApiError::internal)?
        .ok_or_else(not_found)
}

/// The recording of a record, with `Range` support so the player can seek.
///
/// `GET /api/audio/{id}?token=` (console API "History"): the record's WAV
/// as `audio/wav`. The id may carry a `.wav` suffix (the page adds one so
/// the browser infers the type). Responses: 200 whole file with
/// `Accept-Ranges: bytes`; 206 with `Content-Range` for a single
/// `bytes=a-b` range (an open end `a-` or an end past EOF is clipped);
/// 416 with `Content-Range: bytes */len` when the start is past EOF;
/// multi-range and suffix (`bytes=-n`) requests fall back to the whole
/// file; 404 for an unknown id or a record saved without audio; 410 when
/// the row has a path but the file is gone. The whole file is read into
/// memory (recordings are ~115 KB per minute) and only the path stored in
/// the record is opened.
pub async fn audio(
    State(state): State<Arc<ConsoleState>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = id.strip_suffix(".wav").unwrap_or(&id);
    let record = find_record(&state, id)?;
    let path = record.audio_path.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "record has no saved audio".to_owned(),
        )
    })?;
    let bytes = std::fs::read(&path)
        .map_err(|_| ApiError(StatusCode::GONE, "audio file is gone".to_owned()))?;
    let length = bytes.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok())));
    Ok(match range {
        // A reversed range (`bytes=50-10`) must not reach the slice below:
        // `start..=end` with `end < start` would panic the handler task.
        Some((start, end)) if start < length && !matches!(end, Some(end) if end < start) => {
            let end = end.map_or(length - 1, |end| end.min(length - 1));
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "audio/wav".to_owned()),
                    (header::ACCEPT_RANGES, "bytes".to_owned()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{length}"),
                    ),
                ],
                bytes[start as usize..=end as usize].to_vec(),
            )
                .into_response()
        }
        Some(_) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{length}"))],
        )
            .into_response(),
        None => (
            [
                (header::CONTENT_TYPE, "audio/wav".to_owned()),
                (header::ACCEPT_RANGES, "bytes".to_owned()),
            ],
            bytes,
        )
            .into_response(),
    })
}

/// `DELETE /api/records/{id}` (console API "History"): removes the row and
/// then its WAV (best effort: a missing file is not an error). 404 for a
/// malformed or unknown id, 500 for database errors; `{"ok": true}`
/// otherwise.
pub async fn delete_record(
    State(state): State<Arc<ConsoleState>>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult {
    // Existence check first so an unknown id is 404 rather than a silent
    // no-op; `HistoryStore::delete` removes the recording with the row.
    find_record(&state, &id)?;
    state
        .store
        .delete(std::slice::from_ref(&id))
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })))
}

struct DecodedRecording {
    text: String,
    device: String,
    decode_seconds: f64,
    audio_seconds: f64,
}

/// Decode with the same segmentation as live dictation. Release the decoder between
/// blocks so a foreground session can start. Exclude queueing and file I/O from RTF.
fn decode_file(
    engine: &crate::engine::WarmEngine,
    mut source: WavSource,
    resource: &dyn SharedResource,
    cancelled: &dyn Fn() -> bool,
) -> Result<DecodedRecording, ApiError> {
    let unit = || resource.unit(cancelled).ok_or_else(request_ended);
    let (mut recognizer, device) = {
        let _unit = unit()?;
        let (decoder, vad_path, limit) = engine
            .offline_decoder()
            .ok_or_else(|| ApiError::bad_request("selected model has no offline decoder"))?;
        let device = decoder.device().to_string();
        let vad = crate::offline_asr::create_vad(&vad_path, limit).map_err(ApiError::internal)?;
        (
            crate::offline_asr::SegmentedRecognizer::new(decoder, vad, Duration::from_secs(3600)),
            device,
        )
    };
    let mut samples = [0.0; crate::capture::SAMPLE_RATE as usize];
    let mut count = 0_u64;
    let mut elapsed = Duration::ZERO;
    loop {
        let n = source.read(&mut samples).map_err(ApiError::bad_request)?;
        if n == 0 {
            break;
        }
        let _unit = unit()?;
        let started = Instant::now();
        recognizer.accept(&samples[..n]);
        elapsed += started.elapsed();
        count += n as u64;
    }
    let _unit = unit()?;
    let started = Instant::now();
    let text = recognizer
        .finish()
        .into_iter()
        .find_map(|event| match event {
            crate::realtime::RealtimeEvent::Finished { transcript } => Some(transcript),
            _ => None,
        })
        .unwrap_or_default();
    Ok(DecodedRecording {
        text,
        device,
        decode_seconds: (elapsed + started.elapsed()).as_secs_f64(),
        audio_seconds: count as f64 / f64::from(crate::capture::SAMPLE_RATE),
    })
}

fn request_ended() -> ApiError {
    ApiError(
        StatusCode::REQUEST_TIMEOUT,
        "recognition request ended".to_owned(),
    )
}

/// The daemon's warm local engine, or 503: `daemon not running` without a
/// daemon, `no local engine is loaded` when the backend is not
/// `local-offline` or the load failed.
fn warm_engine(state: &ConsoleState) -> Result<Arc<crate::engine::WarmEngine>, ApiError> {
    state
        .daemon()?
        .warm()
        .filter(|engine| matches!(engine.as_ref(), crate::engine::WarmEngine::Offline { .. }))
        .ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "no local engine is loaded (streaming_backend = \"local-offline\")".to_owned(),
            )
        })
}

/// Propose the selected local model's result. An older bodyless caller uses
/// the active local model and still receives the preview fields it expects.
#[derive(serde::Deserialize)]
pub struct Retranscribe {
    model: String,
}

pub async fn retranscribe(
    State(state): State<Arc<ConsoleState>>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<Retranscribe>>,
) -> ApiResult {
    use crate::history::revisions::{RevisionDraft, RevisionKind};

    let daemon = state.daemon()?.clone();
    let record = find_record(&state, &id)?;
    let path = record
        .audio_path
        .filter(|p| Path::new(p).is_file())
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "record has no saved audio".to_owned(),
            )
        })?;
    let active = daemon.active_file();
    let model = body.map_or_else(
        || active.config.local.model.clone(),
        |Json(input)| input.model,
    );
    if model.is_empty()
        || !model
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return Err(ApiError::bad_request("choose an installed offline model"));
    }
    crate::offline_asr::OfflineFamily::for_model(&model).map_err(ApiError::bad_request)?;
    let cleaner =
        crate::text::TextCleaner::new(&state.config()?.text).map_err(ApiError::bad_request)?;
    let mut config = active.config.clone();
    config.local.select_file_model(&model);
    config.asr.streaming_backend = "local-offline".to_owned();
    // At most one history request retains a cold model between decoder units.
    static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let slot = SLOT.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::CONFLICT,
            "another history recording is being recognized".to_owned(),
        )
    })?;
    let (cancel, request) = tokio::sync::oneshot::channel::<()>();
    let (text, device, seconds) = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        let _slot = slot;
        let resource: &dyn SharedResource = &daemon.transcription_resource;
        let cancelled = || cancel.is_closed();
        let started = Instant::now();
        let source = WavSource::open(Path::new(&path), 0).map_err(ApiError::bad_request)?;
        let engine = {
            let _unit = resource.unit(&cancelled).ok_or_else(request_ended)?;
            match daemon
                .warm()
                .filter(|engine| engine.matches_config(&config))
            {
                Some(warm) => warm,
                None => {
                    config.local.fast_preview = false;
                    config.local.denoise = false;
                    Arc::new(
                        crate::engine::WarmEngine::load(&config)
                            .map_err(ApiError::bad_request)?
                            .ok_or_else(|| {
                                ApiError::bad_request("cannot load selected offline model")
                            })?,
                    )
                }
            }
        };
        let decoded = decode_file(&engine, source, resource, &cancelled)?;
        Ok((
            decoded.text,
            decoded.device,
            started.elapsed().as_secs_f64(),
        ))
    })
    .await
    .map_err(ApiError::internal)??;
    drop(request);
    let cleaned = cleaner.clean(&text);
    if cleaned.trim().is_empty() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no speech recognized".to_owned(),
        ));
    }
    let draft = RevisionDraft {
        kind: RevisionKind::Retranscription,
        raw_text: text,
        processed_text: Some(cleaned.clone()),
        final_text: cleaned,
        asr_provider: Some("local-offline".to_owned()),
        asr_model: Some(model.clone()),
        device: Some(device.clone()),
    };
    let (record, revision) = state
        .store
        .propose_revision(&id, &draft, false, None)
        .map_err(super::revisions::history_error)?;
    Ok(Json(
        json!({ "ok": true, "text": draft.raw_text, "cleaned": draft.final_text,
        "seconds": seconds, "engine": format!("{model} ({device})"),
        "record": record_json(&record), "revision": revision }),
    ))
}

/// `POST /api/corrections` (console API "History"): body `{"original",
/// "corrected"}` → `{"ok": true, "suggestions": [{"from","to"}]}` from
/// `suggest_replacements`. Pure computation, no daemon or database;
/// missing fields count as empty strings. Always 200.
pub async fn corrections(Json(body): Json<Value>) -> ApiResult {
    let original = body["original"].as_str().unwrap_or_default();
    let corrected = body["corrected"].as_str().unwrap_or_default();
    let suggestions: Vec<Value> = suggest_replacements(original, corrected)
        .into_iter()
        .map(|(from, to)| json!({"from": from, "to": to}))
        .collect();
    Ok(Json(json!({ "ok": true, "suggestions": suggestions })))
}

/// Replacement pairs turning `original` into `corrected`: each changed span
/// of a character diff, widened with unchanged neighbours to at least three
/// characters so the rule does not fire on every occurrence of one
/// character (`峰` → `丰` would; `张三峰` → `张三丰` does not).
///
/// Both inputs are capped at 2 000 chars (the LCS table is O(n·m) `u16`
/// cells). Widening only consumes alphanumeric context, so punctuation and
/// spaces bound a suggestion. Spans whose `from` is blank are dropped (an
/// insertion with no context has nothing to match), as are no-op pairs.
/// Each suggestion is a candidate `[text] replacements` entry; the page
/// may turn same-length Chinese ones into hotwords instead.
pub fn suggest_replacements(original: &str, corrected: &str) -> Vec<(String, String)> {
    let a: Vec<char> = original.chars().take(2_000).collect();
    let b: Vec<char> = corrected.chars().take(2_000).collect();
    // LCS table, then walk it into aligned (a_index, b_index) matches.
    let mut lcs = vec![vec![0_u16; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut matches = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // Sentinel so the trailing changed span (after the last match) is emitted.
    matches.push((a.len(), b.len()));
    let is_word = |ch: char| ch.is_alphanumeric();
    let mut suggestions = Vec::new();
    let (mut previous_a, mut previous_b) = (0, 0);
    for (match_a, match_b) in matches {
        if match_a > previous_a || match_b > previous_b {
            let (mut start_a, mut start_b) = (previous_a, previous_b);
            let (mut end_a, mut end_b) = (match_a, match_b);
            // Widen with shared context (equal on both sides) up to 3 chars.
            while (end_a - start_a) < 3
                && start_a > 0
                && start_b > 0
                && a[start_a - 1] == b[start_b - 1]
                && is_word(a[start_a - 1])
            {
                start_a -= 1;
                start_b -= 1;
            }
            while (end_a - start_a) < 3
                && end_a < a.len()
                && end_b < b.len()
                && a[end_a] == b[end_b]
                && is_word(a[end_a])
            {
                end_a += 1;
                end_b += 1;
            }
            let from: String = a[start_a..end_a].iter().collect();
            let to: String = b[start_b..end_b].iter().collect();
            if !from.trim().is_empty() && from != to {
                suggestions.push((from, to));
            }
        }
        previous_a = match_a + 1;
        previous_b = match_b + 1;
    }
    suggestions
}

/// Character error rate between two transcripts, ignoring punctuation,
/// whitespace and letter case.
///
/// Levenshtein distance over the normalised characters (single-row DP)
/// divided by the reference length. Empty reference: 0.0 if the hypothesis
/// is empty too, else 1.0. Can exceed 1.0 when the hypothesis is much
/// longer than the reference.
pub fn cer(reference: &str, hypothesis: &str) -> f64 {
    let normalize = |text: &str| -> Vec<char> {
        text.chars()
            .filter(|ch| ch.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let (reference, hypothesis) = (normalize(reference), normalize(hypothesis));
    if reference.is_empty() {
        return if hypothesis.is_empty() { 0.0 } else { 1.0 };
    }
    let mut row: Vec<usize> = (0..=hypothesis.len()).collect();
    for (i, r) in reference.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, h) in hypothesis.iter().enumerate() {
            let substitution = diagonal + usize::from(r != h);
            diagonal = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(row[j + 1] + 1);
        }
    }
    row[hypothesis.len()] as f64 / reference.len() as f64
}

/// `POST /api/benchmark` (console API "Diagnostics"): body `{"limit": 10}`
/// (clamped to 1..=50; no body = 10). Takes the newest `limit` records
/// among the latest 500 whose WAV still exists, decodes each with the warm
/// engine through `decode_file`, and answers `{"ok": true, "engine",
/// "results": [...], "mean_rtf", "mean_cer"}`. Per record: `audio_seconds`,
/// `decode_seconds`, `rtf` (decode ÷ audio), `reference` (the record's
/// `raw_text`, whatever backend produced it), `text` and `cer`; a record
/// that fails to decode yields `{"id", "error"}` instead and is left out
/// of the means (which are `null` if nothing decoded). 503 without a
/// daemon or local engine, 404 when no record has audio, 500 for database
/// errors. Background decodes yield to live dictation between blocks.
pub async fn benchmark(
    State(state): State<Arc<ConsoleState>>,
    body: Option<Json<Value>>,
) -> ApiResult {
    let engine = warm_engine(&state)?;
    let limit = body
        .as_ref()
        .and_then(|Json(body)| body["limit"].as_u64())
        .unwrap_or(10)
        .clamp(1, 50) as usize;
    let page = state.store.query(500, None).map_err(ApiError::internal)?;
    let samples: Vec<HistoryRecord> = page
        .records
        .into_iter()
        .filter(|record| {
            record
                .audio_path
                .as_deref()
                .is_some_and(|path| Path::new(path).is_file())
        })
        .take(limit)
        .collect();
    if samples.is_empty() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "no history records with saved audio".to_owned(),
        ));
    }
    let description = engine.describe();
    let resource = state.daemon()?.transcription_resource.clone();
    let (cancel, request) = tokio::sync::oneshot::channel::<()>();
    let results = tokio::task::spawn_blocking(move || {
        let mut results = Vec::with_capacity(samples.len());
        for record in samples {
            let Some(path) = record.audio_path.as_deref() else {
                continue;
            };
            let decoded = WavSource::open(Path::new(path), 0)
                .map_err(ApiError::bad_request)
                .and_then(|source| decode_file(&engine, source, &resource, &|| cancel.is_closed()));
            let row = match decoded {
                Ok(DecodedRecording {
                    text,
                    decode_seconds,
                    audio_seconds,
                    ..
                }) => json!({
                    "id": record.id,
                    "audio_seconds": audio_seconds,
                    "decode_seconds": decode_seconds,
                    "rtf": decode_seconds / audio_seconds.max(0.01),
                    "reference": record.raw_text,
                    "text": text,
                    "cer": cer(&record.raw_text, &text),
                }),
                Err(error) if cancel.is_closed() => return Err(error),
                Err(error) => json!({ "id": record.id, "error": error.1 }),
            };
            results.push(row);
        }
        Ok(results)
    })
    .await
    .map_err(ApiError::internal)??;
    drop(request);
    let mean = |field: &str| {
        let values: Vec<f64> = results
            .iter()
            .filter_map(|row| row[field].as_f64())
            .collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    Ok(Json(json!({
        "ok": true,
        "engine": description,
        "mean_rtf": mean("rtf"),
        "mean_cer": mean("cer"),
        "results": results,
    })))
}
