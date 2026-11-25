use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::{library, ApiError, ApiResult, ConsoleState};
use crate::transcription_jobs::{
    media_support, ExportFormat, ImportRequest, ImportSource, JobQueue, QueueError, QueueOptions,
    RecognitionRequest, Transport, FFMPEG_FORMATS, NATIVE_FORMATS,
};

const MAX_UPLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;

fn queue(state: &ConsoleState) -> Result<&Arc<JobQueue>, ApiError> {
    state.daemon()?.jobs.get().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the transcription queue is unavailable".to_owned(),
        )
    })
}

fn queue_error(error: QueueError) -> ApiError {
    let status = match error {
        QueueError::NotFound(..) => StatusCode::NOT_FOUND,
        QueueError::Invalid(..) => StatusCode::BAD_REQUEST,
        QueueError::InvalidTransition { .. } => StatusCode::CONFLICT,
        QueueError::Busy => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError(status, error.to_string())
}

pub(super) async fn list(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let jobs = queue(&state)?;
    let formats = media_support(&QueueOptions::default());
    Ok(Json(json!({
        "jobs": jobs.list(),
        "recovery": jobs.recovery(),
        "native_formats": NATIVE_FORMATS,
        "additional_formats": formats.ffmpeg.is_some().then_some(FFMPEG_FORMATS),
        "max_upload_bytes": MAX_UPLOAD_BYTES,
    })))
}

pub(super) async fn get(
    State(state): State<Arc<ConsoleState>>,
    Path(id): Path<String>,
) -> ApiResult {
    let view = queue(&state)?.get(&id).map_err(queue_error)?;
    serde_json::to_value(view)
        .map(Json)
        .map_err(ApiError::internal)
}

#[derive(Deserialize)]
pub(super) struct Submission {
    name: String,
    model: String,
    transport: Transport,
    language: Option<String>,
    #[serde(default)]
    cloud_authorized: bool,
}

pub(super) async fn submit(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<Submission>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    if query.name.trim().is_empty()
        || query.name.len() > 1024
        || query.name.contains(['/', '\\', '\0'])
    {
        return Err(library::drain(
            body,
            ApiError::bad_request("provide the media file's name without a path"),
        )
        .await);
    }
    if query.model.trim().is_empty() || query.model.len() > 1024 {
        return Err(
            library::drain(body, ApiError::bad_request("choose a recognition model")).await,
        );
    }
    let jobs = queue(&state)?.clone();
    let source = receive(&jobs, headers, body).await?;
    let request = ImportRequest {
        source,
        name: Some(query.name),
        recognition: RecognitionRequest {
            model: query.model,
            transport: query.transport,
            language: query.language,
            cloud_authorized: query.cloud_authorized,
        },
    };
    let id = tokio::task::spawn_blocking(move || jobs.submit(request))
        .await
        .map_err(ApiError::internal)?
        .map_err(queue_error)?;
    Ok(Json(json!({ "id": id })))
}

pub(super) async fn reselect(
    State(state): State<Arc<ConsoleState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    let jobs = queue(&state)?.clone();
    let source = receive(&jobs, headers, body).await?;
    tokio::task::spawn_blocking(move || jobs.reselect(&id, source))
        .await
        .map_err(ApiError::internal)?
        .map_err(queue_error)?;
    Ok(Json(json!({ "ok": true })))
}

async fn receive(
    jobs: &JobQueue,
    headers: HeaderMap,
    body: Body,
) -> Result<ImportSource, ApiError> {
    let bytes = match library::content_length(&headers) {
        Ok(bytes) => bytes,
        Err(error) => return Err(library::drain(body, error).await),
    };
    if bytes > MAX_UPLOAD_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "media files may be at most 8 GiB".to_owned(),
        ));
    }
    let file = tempfile::NamedTempFile::new_in(jobs.incoming_dir()).map_err(library::disk_error)?;
    library::receive(file, body, bytes)
        .await
        .map(ImportSource::Upload)
}

pub(super) async fn action(
    State(state): State<Arc<ConsoleState>>,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult {
    let jobs = queue(&state)?;
    match action.as_str() {
        "pause" => jobs.pause(&id),
        "resume" => jobs.resume(&id),
        "cancel" => jobs.cancel(&id),
        "retry" => jobs.retry(&id),
        _ => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "unknown transcription job action".to_owned(),
            ))
        }
    }
    .map_err(queue_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn remove(
    State(state): State<Arc<ConsoleState>>,
    Path(id): Path<String>,
) -> ApiResult {
    queue(&state)?.remove(&id).map_err(queue_error)?;
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn export(
    State(state): State<Arc<ConsoleState>>,
    Path((id, format)): Path<(String, String)>,
) -> ApiResult {
    let format = ExportFormat::parse(&format)
        .ok_or_else(|| ApiError::bad_request("choose txt, json, srt or vtt"))?;
    let output = queue(&state)?.export(&id, format).map_err(queue_error)?;
    Ok(Json(
        json!({ "name": output.file_name, "type": output.media_type, "text": output.content }),
    ))
}
