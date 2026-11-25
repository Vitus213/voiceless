use super::{ApiError, ApiResult, ConsoleState};
use crate::meeting_capture::{Failure, MeetingError, MeetingService, Selection};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(super) fn routes() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/api/meetings", get(list))
        .route("/api/meetings/sources", get(sources))
        .route("/api/meetings/start", post(record))
        .route("/api/meetings/{id}", get(detail))
        .route("/api/meetings/{id}/{action}", post(action))
}

fn service(state: &ConsoleState) -> Result<&Arc<MeetingService>, ApiError> {
    state.daemon()?.meetings.get().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "meeting capture is unavailable".into(),
        )
    })
}

fn scoped<'a>(
    state: &'a ConsoleState,
    runtime_id: &str,
) -> Result<&'a Arc<MeetingService>, ApiError> {
    if !state.daemon()?.has_instance(runtime_id) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "the runtime has restarted".into(),
        ));
    }
    service(state)
}

fn error(e: MeetingError) -> ApiError {
    let status = match &e {
        MeetingError::UnknownMeeting(_) => StatusCode::NOT_FOUND,
        MeetingError::Failure(Failure::GraphUnreadable { .. } | Failure::NoRecorder { .. }) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        MeetingError::Io(_) | MeetingError::Failure(Failure::DataUnwritable { .. }) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        _ => StatusCode::CONFLICT,
    };
    ApiError(status, e.to_string())
}

async fn list(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let mut list = service(&state)?.list().await;
    list["runtime_id"] = json!(state.daemon()?.status().runtime_id);
    Ok(Json(list))
}

async fn sources(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    Ok(Json(json!(service(&state)?
        .sources()
        .await
        .map_err(error)?)))
}

async fn detail(State(state): State<Arc<ConsoleState>>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!(service(&state)?
        .get(&id)
        .await
        .map_err(error)?)))
}

#[derive(Deserialize)]
struct Start {
    runtime_id: String,
    #[serde(flatten)]
    selection: Selection,
}

async fn record(State(state): State<Arc<ConsoleState>>, Json(input): Json<Start>) -> ApiResult {
    Ok(Json(json!(scoped(&state, &input.runtime_id)?
        .record(input.selection)
        .await
        .map_err(error)?)))
}

#[derive(Deserialize)]
struct Instance {
    runtime_id: String,
}

async fn action(
    State(state): State<Arc<ConsoleState>>,
    Path((id, action)): Path<(String, String)>,
    Json(instance): Json<Instance>,
) -> ApiResult {
    let service = scoped(&state, &instance.runtime_id)?;
    let meeting = match action.as_str() {
        "stop" => service.stop(&id).await,
        "recover" => service.recover(&id).await,
        "retry" => service.retry(&id).await,
        "discard" => {
            service.discard(&id).await.map_err(error)?;
            return Ok(Json(json!({"ok":true})));
        }
        _ => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "unknown meeting action".into(),
            ))
        }
    }
    .map_err(error)?;
    Ok(Json(json!(meeting)))
}
