use std::sync::Arc;

use super::DaemonShared;
use crate::transcription_jobs::{JobQueue, QueueOptions, RuntimeRecognition, Workers};

pub(super) fn start(shared: &Arc<DaemonShared>) -> Result<(), String> {
    let options = QueueOptions::default();
    let jobs =
        Arc::new(JobQueue::open(&shared.data_dir, options).map_err(|error| error.to_string())?);
    // The queue lives in DaemonShared. Its workers must not keep it alive when
    // the runtime shuts down.
    let warm = Arc::downgrade(shared);
    let config = warm.clone();
    let recognition = RuntimeRecognition::new(
        move || warm.upgrade().and_then(|state| state.warm()),
        move || {
            config
                .upgrade()
                .map_or_else(crate::config::AppConfig::default, |state| {
                    state.active_file().config.clone()
                })
        },
    );
    jobs.start(Workers {
        recognition: Arc::new(recognition),
        resource: Arc::new(shared.transcription_resource.clone()),
    })
    .map_err(|error| error.to_string())?;
    shared
        .jobs
        .set(jobs)
        .map_err(|_| "the transcription queue has already started".to_owned())
}
