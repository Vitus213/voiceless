use super::{
    journal, Meeting, MeetingError, MeetingRuntime, MeetingState, Selection, SourceCatalog,
    TrackStatus,
};
use crate::transcription_jobs::{
    ImportRequest, ImportSource, JobQueue, RecognitionRequest, Transport,
};
use serde_json::{json, Value};
use std::{io, sync::Arc, time::Duration};
use tokio::sync::Mutex;

/// One service per daemon. `publish` forwards bodies through the daemon's sequenced event stream.
/// The caller supplies its configured local recognition model and the queue for the same data directory.
pub fn start(
    mut runtime: MeetingRuntime,
    jobs: Arc<JobQueue>,
    local_model: String,
    publish: impl Fn(Value) + Send + Sync + 'static,
) -> Result<Arc<MeetingService>, MeetingError> {
    runtime.sweep()?;
    let meetings = runtime.meetings();
    let service = Arc::new(MeetingService {
        runtime: Mutex::new(runtime),
        jobs,
        local_model,
        publish: Box::new(publish),
    });
    // A crash can occur after finalization or queue submission but before the job ID is journaled.
    for meeting in meetings {
        if ready(&meeting)
            && meeting
                .tracks
                .iter()
                .any(|t| t.status == TrackStatus::Finalized && t.job_id.is_none())
        {
            service.finish_handoff(meeting);
        }
    }
    Ok(service)
}

pub struct MeetingService {
    runtime: Mutex<MeetingRuntime>,
    jobs: Arc<JobQueue>,
    local_model: String,
    publish: Box<dyn Fn(Value) + Send + Sync>,
}

fn ready(meeting: &Meeting) -> bool {
    matches!(
        meeting.state,
        MeetingState::Completed | MeetingState::Recovered
    )
}

impl MeetingService {
    pub async fn list(&self) -> Value {
        let runtime = self.runtime.lock().await;
        json!({"data_location": runtime.data_location(), "meetings": runtime.meetings()})
    }

    pub async fn get(&self, id: &str) -> Result<Meeting, MeetingError> {
        self.runtime.lock().await.meeting(id)
    }

    pub async fn sources(&self) -> Result<SourceCatalog, MeetingError> {
        self.runtime.lock().await.sources().await
    }

    pub async fn record(self: &Arc<Self>, selection: Selection) -> Result<Meeting, MeetingError> {
        let mut runtime = self.runtime.lock().await;
        let meeting = runtime.start(selection).await?;
        self.event(&meeting, None);
        let owner = Arc::downgrade(self);
        let id = meeting.id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let Some(owner) = owner.upgrade() else { break };
                let mut runtime = owner.runtime.lock().await;
                if runtime.active_id() != Some(id.as_str()) {
                    break;
                }
                if let Some(meeting) = runtime.poll() {
                    owner.finish_handoff(meeting);
                    break;
                }
            }
        });
        Ok(meeting)
    }

    pub async fn stop(&self, id: &str) -> Result<Meeting, MeetingError> {
        let mut runtime = self.runtime.lock().await;
        let meeting = runtime.meeting(id)?;
        if meeting.state != MeetingState::Recording {
            return Ok(meeting);
        }
        if runtime.active_id() != Some(id) {
            return Err(MeetingError::StillRecording(id.into()));
        }
        let meeting = runtime.stop()?;
        self.event(&meeting, None);
        Ok(meeting)
    }

    pub async fn recover(&self, id: &str) -> Result<Meeting, MeetingError> {
        let mut runtime = self.runtime.lock().await;
        let mut meeting = runtime.meeting(id)?;
        if meeting.state != MeetingState::Recovered {
            meeting = runtime.recover(id)?;
        }
        self.enqueue(&meeting)?;
        let meeting = runtime.meeting(id)?;
        self.event(&meeting, None);
        Ok(meeting)
    }

    pub async fn retry(&self, id: &str) -> Result<Meeting, MeetingError> {
        let runtime = self.runtime.lock().await;
        let meeting = runtime.meeting(id)?;
        self.enqueue(&meeting)?;
        let meeting = runtime.meeting(id)?;
        self.event(&meeting, None);
        Ok(meeting)
    }

    pub async fn discard(&self, id: &str) -> Result<(), MeetingError> {
        let mut runtime = self.runtime.lock().await;
        let meeting = runtime.meeting(id)?;
        if meeting.state != MeetingState::Interrupted {
            return Err(MeetingError::NotInterrupted(
                id.into(),
                meeting.state.name(),
            ));
        }
        runtime.discard(id)?;
        (self.publish)(json!({"type":"meeting", "meeting_id":id, "state":"discarded"}));
        Ok(())
    }

    fn event(&self, meeting: &Meeting, error: Option<String>) {
        (self.publish)(
            json!({"type":"meeting", "meeting_id":meeting.id, "state":meeting.state, "error":error}),
        );
    }

    fn finish_handoff(&self, meeting: Meeting) {
        let error = if ready(&meeting) {
            self.enqueue(&meeting).err().map(|e| e.to_string())
        } else {
            None
        };
        self.event(&meeting, error);
    }

    // Call while holding the runtime lock. The queue retains the original path, even after import.
    // This closes the restart window between its durable submission and the meeting journal update.
    fn enqueue(&self, meeting: &Meeting) -> Result<(), MeetingError> {
        if !ready(meeting) {
            return Err(MeetingError::StillRecording(meeting.id.clone()));
        }
        let directory = &meeting.data_location;
        let mut doc = journal::read(directory)?;
        for track in &meeting.tracks {
            if track.status != TrackStatus::Finalized || track.job_id.is_some() {
                continue;
            }
            let path = journal::final_path(directory, track.kind).canonicalize()?;
            let name = format!("{}-{}.wav", meeting.id, track.kind.name());
            let existing = self.jobs.list().into_iter().find(|job| {
                job.transport == Transport::Local
                    && job.input.name == name
                    && job.input.path.as_deref().map(std::path::Path::new) == Some(path.as_path())
            });
            let id = match existing {
                Some(job) => job.id,
                None => self
                    .jobs
                    .submit(ImportRequest {
                        source: ImportSource::Path(path),
                        name: Some(name),
                        recognition: RecognitionRequest {
                            model: self.local_model.clone(),
                            transport: Transport::Local,
                            language: None,
                            cloud_authorized: false,
                        },
                    })
                    .map_err(io::Error::other)?,
            };
            doc.track_mut(track.kind).job_id = Some(id);
            journal::write(directory, &doc)?;
        }
        Ok(())
    }
}
