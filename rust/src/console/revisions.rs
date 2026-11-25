use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{library, ApiError, ApiResult, ConsoleState};
use crate::history::revisions::{RevisionDraft, RevisionKind};
use crate::history::HistoryError;

/// Relative to the console's authenticated /api router.
pub(super) fn routes() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/records/{id}/revisions", get(list).post(correct))
        .route("/records/{id}/revisions/{revision}/adopt", post(adopt))
}

pub(super) fn history_error(error: HistoryError) -> ApiError {
    let status = match error {
        HistoryError::NotFound(_) => StatusCode::NOT_FOUND,
        HistoryError::Conflict(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError(status, error.to_string())
}

async fn list(State(state): State<Arc<ConsoleState>>, Path(id): Path<String>) -> ApiResult {
    let versions = state.store.revisions(&id).map_err(history_error)?;
    Ok(Json(json!({ "revisions": versions })))
}

#[derive(Deserialize)]
struct Correction {
    text: String,
    expected: String,
}

async fn correct(
    State(state): State<Arc<ConsoleState>>,
    Path(id): Path<String>,
    Json(input): Json<Correction>,
) -> ApiResult {
    if input.text.trim().is_empty() {
        return Err(ApiError::bad_request("corrected text is empty"));
    }
    let original = library::find_record(&state, &id)?;
    let draft = RevisionDraft {
        kind: RevisionKind::Correction,
        raw_text: original.raw_text,
        processed_text: Some(input.text.clone()),
        final_text: input.text,
        asr_provider: original.asr_provider,
        asr_model: original.asr_model,
        device: None,
    };
    let (record, revision) = state
        .store
        .propose_revision(&id, &draft, false, Some(&input.expected))
        .map_err(history_error)?;
    Ok(Json(
        json!({ "record": library::record_json(&record), "revision": revision }),
    ))
}

#[derive(Deserialize)]
struct Confirmation {
    expected: String,
}

async fn adopt(
    State(state): State<Arc<ConsoleState>>,
    Path((id, revision)): Path<(String, String)>,
    Json(input): Json<Confirmation>,
) -> ApiResult {
    let (record, revision) = state
        .store
        .adopt_revision(&id, &revision, Some(&input.expected))
        .map_err(history_error)?;
    Ok(Json(
        json!({ "record": library::record_json(&record), "revision": revision }),
    ))
}
