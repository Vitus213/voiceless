use std::sync::Arc;
use std::time::Instant;

use tokio::task::JoinHandle;

use super::{AppliedConfig, DaemonShared, EngineStatus};
use crate::console::settings::{load_revision, restore_revision, saved_revision, ConfigFile};
use crate::engine::WarmEngine;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigApplyStatus {
    pub active_revision: String,
    pub target_revision: Option<String>,
    pub state: &'static str,
    pub error: Option<String>,
}

struct Request {
    revision: String,
    force_engine_reload: bool,
}

pub(super) struct Prepared {
    pub file: Arc<ConfigFile>,
    pub engine: Option<Arc<WarmEngine>>,
    pub status: EngineStatus,
}

struct Load {
    task: JoinHandle<Result<Prepared, String>>,
    superseded: bool,
}

#[derive(Default)]
pub(super) struct ApplyQueue {
    queued: Option<Request>,
    loading: Option<Load>,
}

impl ApplyQueue {
    pub fn request(
        &mut self,
        shared: &DaemonShared,
        expected: Option<String>,
    ) -> Result<ConfigApplyStatus, String> {
        let revision = saved_revision(&shared.config_path).map_err(|error| error.to_string())?;
        if expected
            .as_ref()
            .is_some_and(|expected| *expected != revision)
        {
            return Err(
                "settings changed before they could be applied; refresh and try again".to_owned(),
            );
        }
        let status = shared.publish_apply("queued", Some(revision.clone()), None);
        self.queued = Some(Request {
            revision,
            force_engine_reload: expected.is_none(),
        });
        Ok(status)
    }

    pub fn restore(
        &mut self,
        shared: &DaemonShared,
        revision: &str,
    ) -> Result<ConfigApplyStatus, String> {
        let active = shared.active_file();
        restore_revision(&shared.config_path, revision, &active)?;
        self.queued = None;
        if let Some(load) = &mut self.loading {
            load.superseded = true;
        }
        Ok(shared.publish_apply("applied", Some(active.revision.clone()), None))
    }

    pub fn is_pending(&self) -> bool {
        self.queued.is_some() || self.loading.is_some()
    }

    /// Keep one native model load in flight. The command loop can accept new
    /// requests and start sessions with the active config while it loads.
    /// A caller commits the returned candidate only while the gateway is idle.
    pub async fn next(&mut self, shared: &DaemonShared) -> Option<Prepared> {
        if self
            .loading
            .as_ref()
            .is_some_and(|load| load.task.is_finished())
        {
            let load = self.loading.take().expect("load in progress");
            let result = load.task.await;
            if !load.superseded && self.queued.is_none() {
                match result.unwrap_or_else(|error| Err(error.to_string())) {
                    Ok(candidate) => return Some(candidate),
                    Err(error) => {
                        shared.publish_apply("failed", None, Some(error));
                    }
                }
            }
        }
        if self.loading.is_none() {
            if let Some(request) = self.queued.take() {
                shared.publish_apply("loading", Some(request.revision.clone()), None);
                let path = shared.config_path.clone();
                let active = shared.applied_config();
                let active_status = shared.engine_status();
                let task = tokio::spawn(async move {
                    let file = load_revision(&path, false).map_err(|error| error.to_string())?;
                    if file.revision != request.revision {
                        return Err(
                            "a newer revision was saved while the apply request was queued"
                                .to_owned(),
                        );
                    }
                    if file.config.audio.source != active.file.config.audio.source {
                        if let Some(source) = &file.config.audio.source {
                            crate::desktop::audio::require_source(source).await?;
                        }
                    }
                    tokio::task::spawn_blocking(move || {
                        prepare(file, &active, active_status, request.force_engine_reload)
                    })
                    .await
                    .map_err(|error| error.to_string())?
                });
                self.loading = Some(Load {
                    task,
                    superseded: false,
                });
            }
        }
        None
    }
}

fn prepare(
    file: ConfigFile,
    active: &AppliedConfig,
    active_status: EngineStatus,
    force_engine_reload: bool,
) -> Result<Prepared, String> {
    let reusable = active
        .engine
        .as_ref()
        .filter(|engine| !force_engine_reload && engine.matches_config(&file.config));
    let (engine, status) = if let Some(engine) = reusable {
        (Some(Arc::clone(engine)), active_status)
    } else {
        let started = Instant::now();
        let engine = WarmEngine::load(&file.config)?.map(Arc::new);
        let status = EngineStatus::loaded(
            &file.config,
            engine.as_deref(),
            started.elapsed().as_secs_f64(),
        );
        (engine, status)
    };
    Ok(Prepared {
        file: Arc::new(file),
        engine,
        status,
    })
}
