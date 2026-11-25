use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::{Extension, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use time::OffsetDateTime;

use super::{ApiError, ApiResult, ConsoleState};
use crate::history::retention::{
    self, Applied, ExportFormat, RecoveryStore, RetentionError, RetentionPolicy,
};
use crate::history::{HistoryError, HistoryFilter, HistoryStore};
use crate::transcription_jobs::{ImportRequest, ImportSource, RecognitionRequest, Transport};

// Only a snapshot actually issued by this console may be applied. Consume each
// handle once, including when the store requests a fresh confirmation.
#[derive(Clone, Default)]
struct Previews(Arc<Mutex<VecDeque<(String, String)>>>);

impl Previews {
    fn issue(&self, mut preview: retention::RetentionPreview) -> Result<Value, ApiError> {
        let handle = super::random_token();
        let mut tokens = self.0.lock().map_err(ApiError::internal)?;
        if tokens.len() == 64 {
            tokens.pop_front();
        }
        tokens.push_back((
            handle.clone(),
            std::mem::replace(&mut preview.snapshot, handle),
        ));
        Ok(json!(preview))
    }

    fn take(&self, handle: &str) -> Result<String, ApiError> {
        let mut tokens = self.0.lock().map_err(ApiError::internal)?;
        let index = tokens
            .iter()
            .position(|(key, _)| key == handle)
            .ok_or_else(|| ApiError::bad_request("preview again before confirming"))?;
        Ok(tokens.remove(index).expect("matched handle").1)
    }
}

pub(super) fn routes() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/api/history/retention/preview", post(preview))
        .route("/api/history/retention/confirm", post(confirm))
        .route("/api/history/export", post(export))
        .route("/api/history/recovery", get(recovery))
        .route("/api/history/recovery/{id}/reopen", post(reopen))
        .layer(Extension(Previews::default()))
}

fn error(err: RetentionError) -> ApiError {
    match err {
        RetentionError::Policy(_)
        | RetentionError::Snapshot(_)
        | RetentionError::History(HistoryError::Filter(_)) => ApiError::bad_request(err),
        RetentionError::TooLarge { .. } => ApiError(StatusCode::PAYLOAD_TOO_LARGE, err.to_string()),
        _ => ApiError::internal(err),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)?
}

fn data_dir(state: &ConsoleState) -> &std::path::Path {
    state
        .store
        .path()
        .parent()
        .expect("history database has a directory")
}

fn area(state: &ConsoleState) -> RecoveryStore {
    RecoveryStore::new(data_dir(state).join("recovery"))
}

async fn preview(
    State(state): State<Arc<ConsoleState>>,
    Extension(handles): Extension<Previews>,
    Json(policy): Json<RetentionPolicy>,
) -> ApiResult {
    blocking(move || {
        let preview = retention::preview(
            &state.store,
            &area(&state),
            &policy,
            &data_dir(&state).join("audio"),
            OffsetDateTime::now_utc(),
        )
        .map_err(error)?;
        handles.issue(preview).map(Json)
    })
    .await
}

#[derive(Deserialize)]
struct Confirmation {
    snapshot: String,
}

async fn confirm(
    State(state): State<Arc<ConsoleState>>,
    Extension(handles): Extension<Previews>,
    Json(body): Json<Confirmation>,
) -> Result<Response, ApiError> {
    let snapshot = handles.take(&body.snapshot)?;
    blocking(move || {
        match retention::apply(&state.store, &area(&state), &snapshot, &data_dir(&state).join("audio"), OffsetDateTime::now_utc()).map_err(error)? {
            Applied::Removed(report) => Ok(Json(json!({"report": report})).into_response()),
            Applied::Changed(preview) => Ok((StatusCode::CONFLICT, Json(json!({"error": "selection changed; review again", "preview": handles.issue(preview)?}))).into_response()),
        }
    }).await
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Filter {
    q: String,
    from: Option<String>,
    to: Option<String>,
    mode: Option<String>,
    app: Option<String>,
    model: Option<String>,
    audio: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportRequest {
    format: ExportFormat,
    #[serde(default)]
    include_audio: bool,
    #[serde(default)]
    filter: Filter,
    ids: Option<Vec<String>>,
}

// Materialize only explicit selections in a private, short-lived SQLite store.
// Both selection modes then use exactly the same document and ZIP writers.
fn selected_store(
    state: &ConsoleState,
    ids: &[String],
    dir: &std::path::Path,
) -> Result<HistoryStore, ApiError> {
    let path = dir.join("selection.sqlite3");
    let store = HistoryStore::open(path.clone()).map_err(ApiError::internal)?;
    let db = rusqlite::Connection::open(path).map_err(ApiError::internal)?;
    let source = rusqlite::types::ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Text(
        state.store.path().as_os_str().as_encoded_bytes(),
    ));
    db.execute("ATTACH DATABASE ?1 AS source", [source])
        .map_err(ApiError::internal)?;
    const COLUMNS: &str = "id,created_at,duration_seconds,raw_text,processing_mode,processed_text,final_text,status,character_count,asr_provider,asr_model,audio_path,app_id";
    let sql = format!("INSERT INTO main.recognition_history ({COLUMNS}) SELECT {COLUMNS} FROM source.recognition_history WHERE id IN (SELECT value FROM json_each(?1))");
    db.execute(
        &sql,
        [serde_json::to_string(ids).map_err(ApiError::internal)?],
    )
    .map_err(ApiError::internal)?;
    Ok(store)
}

async fn export(
    State(state): State<Arc<ConsoleState>>,
    Json(body): Json<ExportRequest>,
) -> Result<Response, ApiError> {
    blocking(move || {
        let f = body.filter;
        let filter = HistoryFilter {
            text: f.q,
            from: f.from,
            to: f.to,
            mode: f.mode,
            app: f.app,
            model: f.model,
            audio: f.audio,
        };
        let now = OffsetDateTime::now_utc();
        let result = match body.ids {
            None => state
                .store
                .export(&filter, body.format, body.include_audio, now),
            Some(ids) => {
                let mut builder = tempfile::Builder::new();
                builder.prefix(".history-export-");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    builder.permissions(std::fs::Permissions::from_mode(0o700));
                }
                let dir = builder
                    .tempdir_in(data_dir(&state))
                    .map_err(ApiError::internal)?;
                selected_store(&state, &ids, dir.path())?.export(
                    &filter,
                    body.format,
                    body.include_audio,
                    now,
                )
            }
        }
        .map_err(error)?;
        Ok((
            [
                (header::CONTENT_TYPE, result.media_type.to_owned()),
                (header::CACHE_CONTROL, "no-store".into()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", result.file_name),
                ),
            ],
            result.bytes,
        )
            .into_response())
    })
    .await
}

async fn recovery(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    blocking(move || Ok(Json(json!({"recordings": area(&state).list().map_err(ApiError::internal)?, "can_reopen": state.daemon.as_ref().is_some_and(|daemon| daemon.jobs.get().is_some())})))).await
}

async fn reopen(State(state): State<Arc<ConsoleState>>, Path(id): Path<String>) -> ApiResult {
    let queue = state.daemon()?.jobs.get().cloned().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "transcription queue unavailable".into(),
        )
    })?;
    blocking(move || {
        let recovery = area(&state);
        let path = recovery
            .get(&id)
            .map_err(ApiError::internal)?
            .and_then(|entry| recovery.audio_path(&entry.id))
            .ok_or_else(|| {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "recovery recording unavailable".into(),
                )
            })?;
        let id = queue
            .submit(ImportRequest {
                source: ImportSource::Path(path),
                name: Some(format!("{id}.wav")),
                recognition: RecognitionRequest {
                    model: state.config()?.local.model,
                    transport: Transport::Local,
                    language: None,
                    cloud_authorized: false,
                },
            })
            .map_err(ApiError::internal)?;
        Ok(Json(json!({"id": id})))
    })
    .await
}
