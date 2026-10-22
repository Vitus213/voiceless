//! Library endpoints: model catalog and installs, history records and
//! recordings, re-transcription with the warm engine, correction
//! suggestions and the replay benchmark.
//!
//! Mounted under `/api` by `super::router`; the "Models", "History" and
//! "Diagnostics" tables of `docs/console-api.md`. Touches the model store
//! (`ModelManager` over `<data dir>/models` with `versions/` beneath it and
//! `<cache dir>/model-downloads` for partial downloads; the dirs default to
//! `~/.local/share/voiceless` and `~/.cache/voiceless`), the history
//! database and the WAV recordings its rows point at, and `config.toml`
//! (`[text]`, for the cleanup applied to re-transcriptions).
//!
//! Endpoints that decode audio (`retranscribe`, `benchmark`) need the
//! daemon's warm local engine (`streaming_backend = "local-offline"`) and
//! answer 503 otherwise; everything else also works under `voiceless
//! history serve`. Record ids are validated as UUIDs before they reach SQL,
//! and audio is only ever read from the path stored in the record, never
//! from anything in the request.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::{ApiError, ApiResult, ConsoleState};
use crate::history::HistoryRecord;
use crate::models::{catalog, default_cache_dir, default_data_dir, HttpDownloader, ModelManager};

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
/// GPU models, ids ending in `-gguf`), `vad` (`silero-vad`), `streaming`
/// (`streaming-*` Paraformer) or `sherpa` for everything else.
fn kind(id: &str) -> &'static str {
    if id.ends_with("-gguf") {
        "gguf"
    } else if id == "silero-vad" {
        "vad"
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

/// `GET /api/models` (console API "Models"): every catalog entry with
/// `installed` (an intact `current` pointer in the store; a broken one
/// reads as not installed), `installing`, `progress` (bytes of the
/// `.partial` download over `size_bytes`, 0..1, only while installing; it
/// sits at 1.0 while the archive is verified and extracted, and is never
/// `null` during an install), `error` from the last failed install,
/// `kind` and `in_use`. Always 200; works without a daemon (`in_use` is
/// then always false).
pub async fn list_models(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let manager = manager();
    let in_use = models_in_use(&state);
    // Snapshot the map so the lock is not held across the filesystem probes.
    let installs = state
        .installs
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let models: Vec<Value> = catalog()
        .into_iter()
        .map(|spec| {
            let install = installs.get(&spec.id);
            let installing = matches!(install, Some(None));
            let progress = installing.then(|| {
                std::fs::metadata(manager.download_path(&spec))
                    .map(|metadata| metadata.len() as f64 / spec.size_bytes as f64)
                    .unwrap_or(0.0)
                    .min(1.0)
            });
            json!({
                "id": spec.id,
                "version": spec.version,
                "size_bytes": spec.size_bytes,
                "license_status": spec.license_status,
                "installed": matches!(manager.resolve(&spec.id), Ok(Some(_))),
                "installing": installing,
                "progress": progress,
                "error": install.cloned().flatten(),
                "kind": kind(&spec.id),
                "in_use": in_use.contains(&spec.id),
            })
        })
        .collect();
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
/// has it loaded (switch models and reload first), 404 if nothing is
/// installed under that id, 500 on filesystem errors; `{"ok": true}`
/// otherwise. No catalog check: any installed id can be removed.
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
    let removed = manager().remove(&id).map_err(ApiError::internal)?;
    if !removed {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{id} is not installed"),
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

/// A `HistoryRecord` as the API's `HistoryRecord` JSON plus `has_audio`,
/// which checks that the referenced WAV still exists (the row keeps its
/// `audio_path` even after the file was removed by hand).
fn record_json(record: &HistoryRecord) -> Value {
    let mut value = serde_json::to_value(record).unwrap_or_default();
    value["has_audio"] = json!(record
        .audio_path
        .as_deref()
        .is_some_and(|path| Path::new(path).is_file()));
    value
}

/// Query parameters of `GET /api/records`.
#[derive(serde::Deserialize)]
pub struct Page {
    /// Page size; default 200, clamped to 1..=1000.
    limit: Option<i64>,
    /// Opaque `next_cursor` from the previous page.
    cursor: Option<String>,
}

/// `GET /api/records?limit=&cursor=` (console API "History"): newest
/// first, `{"records": [HistoryRecord + has_audio], "next_cursor"}` where
/// `next_cursor` is `null` on the last page. `limit` defaults to 200 and
/// is clamped to 1..=1000. 500 for database errors, including a cursor
/// that does not decode.
pub async fn records(
    State(state): State<Arc<ConsoleState>>,
    Query(page): Query<Page>,
) -> ApiResult {
    let page = state
        .store
        .query(
            page.limit.unwrap_or(200).clamp(1, 1_000),
            page.cursor.as_deref(),
        )
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "records": page.records.iter().map(record_json).collect::<Vec<_>>(),
        "next_cursor": page.next_cursor,
    })))
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
fn find_record(state: &ConsoleState, id: &str) -> Result<HistoryRecord, ApiError> {
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
        Some((start, end)) if start < length => {
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
    let record = find_record(&state, &id)?;
    state
        .store
        .delete(std::slice::from_ref(&id))
        .map_err(ApiError::internal)?;
    if let Some(path) = record.audio_path {
        let _ = std::fs::remove_file(path);
    }
    Ok(Json(json!({ "ok": true })))
}

/// Decode a WAV with the daemon's warm engine through the live VAD
/// segmentation; returns (text, decode seconds, audio seconds).
///
/// Mirrors a real session so timings are comparable: the file is read with
/// sherpa's WAV reader, resampled to 16 kHz if needed, fed in 512-sample
/// chunks to a `SegmentedRecognizer` built from the engine's decoder, VAD
/// model and segment limit, and the `Finished` transcript is taken. The
/// partial interval is set to an hour so no partial decodes inflate the
/// time. Blocking and GPU-heavy: callers run it on the blocking pool.
/// Errors are strings: unreadable file, resampler or VAD creation failure.
fn decode_file(
    engine: &crate::engine::WarmEngine,
    path: &str,
) -> Result<(String, f64, f64), String> {
    let wave = sherpa_onnx::Wave::read(path).ok_or_else(|| format!("cannot read {path}"))?;
    let samples = if wave.sample_rate() == 16_000 {
        wave.samples().to_vec()
    } else {
        sherpa_onnx::LinearResampler::create(wave.sample_rate(), 16_000)
            .ok_or("cannot resample")?
            .resample(wave.samples(), true)
    };
    let (decoder, vad_model, segment_limit) = engine.decoder();
    let vad =
        crate::offline_asr::create_vad(&vad_model, segment_limit).map_err(|e| e.to_string())?;
    // No partials: only the segment decodes count.
    let mut recognizer =
        crate::offline_asr::SegmentedRecognizer::new(decoder, vad, Duration::from_secs(3_600));
    let started = Instant::now();
    for chunk in samples.chunks(512) {
        recognizer.accept(chunk);
    }
    let transcript = recognizer
        .finish()
        .into_iter()
        .find_map(|event| match event {
            crate::realtime::RealtimeEvent::Finished { transcript } => Some(transcript),
            _ => None,
        })
        .unwrap_or_default();
    Ok((
        transcript,
        started.elapsed().as_secs_f64(),
        samples.len() as f64 / 16_000.0,
    ))
}

/// The daemon's warm local engine, or 503: `daemon not running` without a
/// daemon, `no local engine is loaded` when the backend is not
/// `local-offline` or the load failed.
fn warm_engine(state: &ConsoleState) -> Result<Arc<crate::engine::WarmEngine>, ApiError> {
    state.daemon()?.warm().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no local engine is loaded (streaming_backend = \"local-offline\")".to_owned(),
        )
    })
}

/// `POST /api/records/{id}/retranscribe` (console API "History"): decodes
/// the record's recording again with the warm engine and answers
/// `{"ok": true, "text", "cleaned", "seconds", "engine"}` — `text` raw,
/// `cleaned` after the current `[text]` cleanup, `seconds` decode time,
/// `engine` the `describe()` string. Nothing is written to history. 503
/// without a daemon or local engine (checked before the id, so an unknown
/// id also gets 503 then), 404 for a malformed/unknown id or a missing
/// WAV, 400 if the saved `[text]` rules do not compile, 500 for decode
/// errors. Occupies the GPU for the decode.
pub async fn retranscribe(
    State(state): State<Arc<ConsoleState>>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult {
    let engine = warm_engine(&state)?;
    let record = find_record(&state, &id)?;
    let path = record
        .audio_path
        .filter(|path| Path::new(path).is_file())
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "record has no saved audio".to_owned(),
            )
        })?;
    let description = engine.describe();
    let (text, seconds, _) = tokio::task::spawn_blocking(move || decode_file(&engine, &path))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    let cleaned = crate::text::TextCleaner::new(&state.config()?.text)
        .map_err(ApiError::bad_request)?
        .clean(&text);
    Ok(Json(json!({
        "ok": true,
        "text": text,
        "cleaned": cleaned,
        "seconds": seconds,
        "engine": description,
    })))
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
/// errors. Decodes run one after another on the blocking pool and hold
/// the GPU for the whole run; the request blocks until all are done.
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
    let results = tokio::task::spawn_blocking(move || {
        samples
            .into_iter()
            .map(|record| {
                let path = record.audio_path.clone().unwrap_or_default();
                match decode_file(&engine, &path) {
                    Ok((text, decode_seconds, audio_seconds)) => json!({
                        "id": record.id,
                        "audio_seconds": audio_seconds,
                        "decode_seconds": decode_seconds,
                        "rtf": decode_seconds / audio_seconds.max(0.01),
                        "reference": record.raw_text,
                        "text": text,
                        "cer": cer(&record.raw_text, &text),
                    }),
                    Err(error) => json!({ "id": record.id, "error": error }),
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(ApiError::internal)?;
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
