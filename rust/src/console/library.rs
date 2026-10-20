//! Library endpoints: model catalog and installs, history records and
//! recordings, re-transcription with the warm engine, correction
//! suggestions and the replay benchmark.

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
#[derive(Default)]
pub struct Installs(Mutex<HashMap<String, Option<String>>>);

fn manager() -> ModelManager {
    ModelManager::new(&default_data_dir(), &default_cache_dir())
}

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

fn models_in_use(state: &ConsoleState) -> Vec<String> {
    state
        .daemon
        .as_ref()
        .and_then(|daemon| daemon.warm())
        .map(|engine| engine.model_ids())
        .unwrap_or_default()
}

pub async fn list_models(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let manager = manager();
    let in_use = models_in_use(&state);
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
        installs.insert(id.clone(), None);
    }
    let console = state.clone();
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

fn record_json(record: &HistoryRecord) -> Value {
    let mut value = serde_json::to_value(record).unwrap_or_default();
    value["has_audio"] = json!(record
        .audio_path
        .as_deref()
        .is_some_and(|path| Path::new(path).is_file()));
    value
}

#[derive(serde::Deserialize)]
pub struct Page {
    limit: Option<i64>,
    cursor: Option<String>,
}

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

pub async fn totals(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    Ok(Json(
        serde_json::to_value(state.store.totals().map_err(ApiError::internal)?)
            .map_err(ApiError::internal)?,
    ))
}

fn is_record_id(id: &str) -> bool {
    id.len() == 36
        && id.as_bytes().iter().enumerate().all(|(index, byte)| {
            byte.is_ascii_hexdigit() || (matches!(index, 8 | 13 | 18 | 23) && *byte == b'-')
        })
}

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

fn warm_engine(state: &ConsoleState) -> Result<Arc<crate::engine::WarmEngine>, ApiError> {
    state.daemon()?.warm().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no local engine is loaded (streaming_backend = \"local-offline\")".to_owned(),
        )
    })
}

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
