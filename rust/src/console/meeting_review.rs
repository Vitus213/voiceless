use super::{ApiError, ApiResult, ConsoleState};
use crate::{
    meeting_capture::{Meeting, MeetingRuntime, MeetingState, TrackKind},
    meeting_review::{local_highlights, Destination, Error, Highlights, Review, SpeakerSource},
    transcription_jobs::Status,
};
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::{io, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub(super) fn routes() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/api/meetings/review", get(list))
        .route("/api/meetings/{id}/review", get(view).post(change))
        .route("/api/meetings/{id}/review/audio/{track}", get(audio))
        .route("/api/meetings/{id}/review/export", post(export))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("private, no-store"),
                );
                response
            },
        ))
}

fn capture(state: &ConsoleState) -> MeetingRuntime {
    MeetingRuntime::new(
        state
            .store
            .path()
            .parent()
            .expect("history database has a directory")
            .to_owned(),
    )
}
fn meeting(state: &ConsoleState, id: &str) -> Result<Meeting, ApiError> {
    let m = capture(state)
        .meeting(id)
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, e.to_string()))?;
    if !matches!(m.state, MeetingState::Completed | MeetingState::Recovered) {
        return Err(ApiError::bad_request(
            "Finalize or recover the meeting first",
        ));
    }
    Ok(m)
}
fn error(e: Error) -> ApiError {
    match e {
        Error::Conflict => ApiError(StatusCode::CONFLICT, e.to_string()),
        Error::Invalid(_) => ApiError::bad_request(e),
        _ => ApiError::internal(e),
    }
}
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)?
}
async fn list(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    blocking(move || {
        let mut items = vec![];
        for m in capture(&state)
            .meetings()
            .into_iter()
            .rev()
            .filter(|m| matches!(m.state, MeetingState::Completed | MeetingState::Recovered))
        {
            items.push(json!({"id":m.id,"started_at":m.started_at}));
        }
        Ok(Json(json!(items)))
    })
    .await
}
fn destination(config: &crate::config::ProcessingConfig) -> Option<Destination> {
    // Never put userinfo, signed query strings or fragments on a review page.
    let (_, authority) = config.base_url.split_once("://")?;
    if !matches!(config.provider.as_str(), "ollama" | "openai-compatible")
        || config.model.is_empty()
        || authority.split('/').next()?.contains('@')
        || authority.contains(['?', '#'])
    {
        return None;
    }
    Some(Destination {
        provider: config.provider.clone(),
        endpoint: config.base_url.clone(),
        model: config.model.clone(),
    })
}
async fn view(State(state): State<Arc<ConsoleState>>, Path(id): Path<String>) -> ApiResult {
    blocking(move || {
        let m = meeting(&state,&id)?;
        let review = Review::load(&m).map_err(error)?;
        let jobs: Vec<_> = state.daemon.as_ref().and_then(|d| d.jobs.get()).into_iter().flat_map(|queue| queue.list()).filter(|job| job.status == Status::Completed)
            .filter_map(|job| {
                let track = crate::meeting_review::track_for_job(&m, &job)?;
                Some(json!({"id":job.id,"track":track.kind,"model":job.model}))
            }).collect();
        let provider = state.config().ok().and_then(|cfg| destination(&cfg.processing));
        Ok(Json(json!({"review":review,"jobs":jobs,"tracks":m.tracks,"data_location":m.data_location,"destination":provider})))
    }).await
}

#[derive(Deserialize)]
struct Request {
    revision: u64,
    #[serde(flatten)]
    action: Action,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
enum Action {
    Attach {
        job: String,
    },
    Separate,
    Edit {
        title: String,
        paragraphs: Vec<Edit>,
        highlights: Option<String>,
    },
    Local {
        segments: Vec<String>,
    },
    Remote {
        segments: Vec<String>,
        consent: bool,
        destination: Destination,
    },
}
#[derive(Deserialize)]
struct Edit {
    id: String,
    text: String,
    speaker: Option<String>,
    confirm_speaker: bool,
}
async fn change(
    State(state): State<Arc<ConsoleState>>,
    Path(id): Path<String>,
    Json(request): Json<Request>,
) -> ApiResult {
    blocking(move || {
        let m = meeting(&state,&id)?;
        let mut r = Review::load(&m).map_err(error)?;
        if r.revision != request.revision { return Err(error(Error::Conflict)); }
        match request.action {
            Action::Attach { job } => {
                let queue = state.daemon()?.jobs.get().ok_or_else(|| ApiError(StatusCode::SERVICE_UNAVAILABLE,"Transcription queue unavailable".into()))?;
                r.attach(&m,queue.get(&job).map_err(ApiError::bad_request)?).map_err(error)?;
            }
            Action::Separate => r.separate(&m).map_err(error)?,
            Action::Edit { title, paragraphs, highlights } => {
                if title.trim().is_empty() { return Err(ApiError::bad_request("Enter a meeting title")); }
                r.title = title;
                for edit in paragraphs {
                    let p = r.paragraphs.iter_mut().find(|p| p.id == edit.id).ok_or_else(|| ApiError::bad_request("Unknown paragraph"))?;
                    p.text = edit.text;
                    if edit.confirm_speaker {
                        p.speaker = edit.speaker.filter(|s| !s.trim().is_empty());
                        p.speaker_source = SpeakerSource::Manual;
                    }
                }
                if let Some(text) = highlights {
                    r.highlights.as_mut().ok_or_else(|| ApiError::bad_request("Generate highlights first"))?.text = text;
                }
            }
            Action::Local { segments } => {
                let selected = r.selected(&segments).map_err(error)?;
                let text = local_highlights(&selected);
                r.highlights = Some(Highlights { generated:text.clone(), text, excerpts:selected.join("\n\n"), segments, source_revision:r.revision, destination:None });
            }
            Action::Remote { segments, consent, destination: grant } => {
                let cfg = state.config()?.processing;
                if !consent || destination(&cfg).as_ref() != Some(&grant) { return Err(ApiError(StatusCode::FORBIDDEN,"Confirm the current provider and selected paragraphs".into())); }
                let selected = r.selected(&segments).map_err(error)?;
                let processor = crate::processing::from_config(&cfg).map_err(ApiError::bad_request)?.ok_or_else(|| ApiError::bad_request("Configure a summary provider"))?;
                let excerpts = selected.join("\n\n");
                let prompt = format!("Summarize these excerpts as concise meeting highlights in their language.\n\n{excerpts}");
                let text = processor.process(&prompt).map_err(|_| ApiError(StatusCode::BAD_GATEWAY,"The summary provider did not return highlights".into()))?;
                r.highlights = Some(Highlights {generated:text.clone(),text,excerpts,segments,source_revision:r.revision,destination:Some(grant)});
            }
        }
        r.save(&m).map_err(error)?;
        Ok(Json(json!(r)))
    }).await
}
async fn export(State(state): State<Arc<ConsoleState>>, Path(id): Path<String>) -> ApiResult {
    blocking(move || {
        let r = Review::load(&meeting(&state,&id)?).map_err(error)?;
        Ok(Json(json!({"name":format!("meeting-{id}.json"),"text":serde_json::to_string_pretty(&r).map_err(ApiError::internal)?})))
    }).await
}

async fn audio(
    State(state): State<Arc<ConsoleState>>,
    Path((id, track)): Path<(String, TrackKind)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let path = blocking(move || {
        meeting(&state, &id)?
            .transcription_inputs()
            .into_iter()
            .find(|i| i.track == track)
            .map(|i| i.path)
            .ok_or_else(|| {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "No finalized audio for this track".into(),
                )
            })
    })
    .await?;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(ApiError::internal)?;
    let size = file.metadata().await.map_err(ApiError::internal)?.len();
    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(header::CACHE_CONTROL, "private, no-store");
    let (start, length) = if let Some(range) = headers.get(header::RANGE) {
        let interval = range
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("bytes="))
            .and_then(|s| s.split_once('-'))
            .and_then(|(from, to)| {
                if size == 0 {
                    return None;
                }
                if from.is_empty() {
                    let n = to.parse::<u64>().ok()?;
                    return (n > 0).then_some((size.saturating_sub(n), size - 1));
                }
                let start = from.parse().ok()?;
                let end = if to.is_empty() {
                    size - 1
                } else {
                    to.parse::<u64>().ok()?.min(size - 1)
                };
                (start <= end).then_some((start, end))
            });
        let Some((start, end)) = interval else {
            return response
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .body(Body::empty())
                .map_err(ApiError::internal);
        };
        response = response
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
        (start, end - start + 1)
    } else {
        (0, size)
    };
    file.seek(io::SeekFrom::Start(start))
        .await
        .map_err(ApiError::internal)?;
    let stream = futures_util::stream::try_unfold((file, length), |(mut file, left)| async move {
        if left == 0 {
            return Ok::<_, io::Error>(None);
        }
        let mut buf = vec![0; left.min(65536) as usize];
        file.read_exact(&mut buf).await?;
        let remaining = left - buf.len() as u64;
        Ok(Some((Bytes::from(buf), (file, remaining))))
    });
    response
        .header(header::CONTENT_LENGTH, length)
        .body(Body::from_stream(stream))
        .map_err(ApiError::internal)
}
