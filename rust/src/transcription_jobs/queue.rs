//! The queue: its jobs, their states, and the rules for changing them.
//!
//! Two sides use it. The console and the daemon's control paths drive a
//! job's life ([`JobQueue::submit`], `pause`, `resume`, `cancel`,
//! `retry`, `reselect`, `remove`, `export`) and read it (`list`, `get`).
//! Workers run it: [`JobQueue::start`] starts the queue's own threads,
//! and the worker-facing operations are public for a daemon that runs the
//! steps on threads of its own ([`worker::import`], [`worker::transcribe`]).
//!
//! # What makes a result
//!
//! A segment exists once its line is synced to the job's journal. The
//! record (`job.json`) is replaced atomically on every change, and only
//! after the change is durable is it visible in memory. So whatever the
//! process does to a job, on disk it is either not yet done or whole.
//!
//! # What makes a run stale
//!
//! Every lifecycle command bumps the job's generation. A worker's
//! [`JobLease`] carries the generation it was claimed under; once the
//! generation moved on, everything the worker reports is refused: it can
//! neither commit a segment nor change the state. That is what keeps a
//! unit still running through a pause, a cancel, a reselect or a removal
//! from producing a result or a completion, without interrupting a decode.
//!
//! # What a restart does
//!
//! A job the previous process left `running` is queued again and
//! continues from the end of its last committed segment. Interrupted
//! copies and record writes are removed; [`JobQueue::recovery`] reports
//! what was found.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use tempfile::NamedTempFile;

use super::export::{self, ExportDocument, ExportFormat};
use super::job::{
    seconds, Activity, FailureCode, InputSummary, JobFailure, JobSummary, JobView, MediaSummary,
    Progress, RecognitionRequest, Recovery, Segment, Stage, Status, Transport,
};
use super::media::{Ffmpeg, MediaDecoder, QueueOptions};
use super::recognize::{Recognition, NOT_AUTHORIZED};
use super::resource::SharedResource;
use super::store::{self, JobDir, JobRecord, Journal, RecordError};
use super::worker;

/// Statuses `pause` accepts.
const PAUSABLE: &[Status] = &[Status::Queued, Status::Running];
/// Statuses `cancel` accepts.
const CANCELLABLE: &[Status] = &[Status::Queued, Status::Running, Status::Paused];
/// Statuses `retry` accepts.
const RETRYABLE: &[Status] = &[Status::Failed, Status::Cancelled];
/// Statuses `reselect` accepts: everything no worker holds.
const RESELECTABLE: &[Status] = &[
    Status::Queued,
    Status::Paused,
    Status::Failed,
    Status::Cancelled,
];
const JOURNAL_CLOSED: &str =
    "the committed results of this job cannot be written; remove the job and import the file again";

/// Why a request to the queue was refused.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("no such job: {0}")]
    NotFound(String),
    #[error("job {id} is {status}; it cannot {action}")]
    InvalidTransition {
        id: String,
        status: Status,
        action: &'static str,
    },
    /// The request names no model, or a reselected upload could not be
    /// stored.
    #[error("{0}")]
    Invalid(String),
    /// Another process serves this data directory's queue.
    #[error(
        "another Voiceless runtime already processes the transcription queue of this data directory"
    )]
    Busy,
    #[error("the queue's workers are already running")]
    AlreadyRunning,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where an import takes its media from.
#[derive(Debug)]
pub enum ImportSource {
    /// A file on this machine. The queue copies it into its store; the
    /// original is neither moved nor changed.
    Path(PathBuf),
    /// A complete upload the caller wrote into [`JobQueue::incoming_dir`].
    /// The queue takes the file over as its own copy of the media.
    Upload(NamedTempFile),
}

/// One file to transcribe.
#[derive(Debug)]
pub struct ImportRequest {
    pub source: ImportSource,
    /// Name to list the job under; the file's own when `None`.
    pub name: Option<String>,
    pub recognition: RecognitionRequest,
}

/// A worker's claim of one run of a job. It is valid while the job's
/// generation is unchanged and the job is `running`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLease {
    id: String,
    generation: u64,
}

impl JobLease {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// What a worker needs to know when it claims a job.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub request: RecognitionRequest,
    /// Where the user selected the file; `None` for an upload.
    pub source_path: Option<PathBuf>,
    /// The queue's own copy of the media.
    pub media: PathBuf,
    /// Where a copy is made, or an uploaded reselection waits.
    pub partial: PathBuf,
    /// Digest the media must have; `None` until the job has owned media.
    pub sha256: Option<String>,
    /// The owned media still has to be made from the source.
    pub needs_source: bool,
    /// `partial` is a whole uploaded reselection.
    pub staged_upload: bool,
    pub decoder: Option<MediaDecoder>,
    pub duration_samples: Option<u64>,
    /// End of the last committed segment: where a run continues.
    pub committed_until: u64,
}

/// A claimed job.
#[derive(Debug, Clone)]
pub struct Claim {
    pub lease: JobLease,
    pub snapshot: JobSnapshot,
}

/// What the queue's threads recognize with, and what they yield to.
pub struct Workers {
    pub recognition: Arc<dyn Recognition>,
    pub resource: Arc<dyn SharedResource>,
}

/// One job as the queue holds it.
struct Job {
    record: JobRecord,
    segments: Vec<Segment>,
    committed_until: u64,
    /// Media position of the current or the last run.
    position: u64,
    activity: Option<Activity>,
    /// `None` when the journal cannot be written to; such a job fails as
    /// soon as a worker claims it.
    journal: Option<Journal>,
}

impl Job {
    /// Whether `lease` is the run that currently holds the job.
    fn runs(&self, lease: &JobLease) -> bool {
        self.record.status == Status::Running && self.record.generation == lease.generation
    }

    fn summary(&self) -> JobSummary {
        let record = &self.record;
        let duration_seconds = record.duration_samples.map(seconds);
        let position_seconds = seconds(self.position);
        JobSummary {
            id: record.id.clone(),
            sequence: record.sequence,
            created_at: record.created_at.clone(),
            status: record.status,
            stage: record.stage,
            activity: self.activity,
            input: InputSummary {
                name: record.name.clone(),
                path: record
                    .source_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                bytes: record.bytes,
                sha256: record.sha256.clone(),
            },
            media: MediaSummary {
                decoder: record.decoder,
                duration_seconds,
            },
            model: record.request.model.clone(),
            language: record
                .request
                .language
                .as_deref()
                .unwrap_or("auto")
                .to_owned(),
            transport: record.request.transport,
            attempts: record.attempts,
            progress: Progress {
                position_seconds,
                committed_seconds: seconds(self.committed_until),
                fraction: duration_seconds
                    .filter(|duration| *duration > 0.0)
                    .map(|duration| (position_seconds / duration).min(1.0)),
                segments: self.segments.len(),
            },
            error: record.error.clone(),
            completed_at: record.completed_at.clone(),
        }
    }
}

struct State {
    jobs: BTreeMap<String, Job>,
    next_sequence: u64,
}

/// What the queue's threads and its owner share. [`JobQueue`] owns it.
pub(crate) struct QueueCore {
    /// `<data dir>/jobs`, absolute.
    root: PathBuf,
    /// `<data dir>/jobs/incoming`, where uploads are received.
    incoming: PathBuf,
    options: QueueOptions,
    state: Mutex<State>,
    /// Wakes workers when work appears or the queue stops.
    wake: Condvar,
    stop: AtomicBool,
    recovery: Recovery,
}

impl QueueCore {
    fn load(root: PathBuf, options: QueueOptions) -> Result<Self, QueueError> {
        let mut recovery = Recovery {
            removed_files: store::clear_incoming(&root),
            ..Recovery::default()
        };
        let mut jobs = BTreeMap::new();
        let mut next_sequence = 0;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Ok(id) = entry.file_name().into_string() else {
                continue;
            };
            if id == store::INCOMING {
                continue;
            }
            let dir = JobDir::new(&root, &id);
            let mut record = match dir.read() {
                Ok(record) => record,
                // The creation of the job got as far as its directory.
                Err(RecordError::Missing) => {
                    let _ = dir.remove();
                    recovery.removed_directories += 1;
                    continue;
                }
                // Not this build's record, or damaged: left exactly as it is.
                Err(RecordError::Unreadable(reason)) => {
                    recovery.unreadable.push(format!("{id}: {reason}"));
                    continue;
                }
            };
            // An uploaded reselection is whole; a copy in progress is not.
            let staged = record.staged_upload && record.stage == Stage::Import;
            recovery.removed_files += dir.sweep(staged);
            let (journal, segments, damage) = match Journal::open(&dir.journal()) {
                Ok((journal, segments)) => (Some(journal), segments, None),
                Err(error) => (None, Vec::new(), Some(error.to_string())),
            };
            let interrupted = record.status == Status::Running;
            if interrupted {
                // The process that held the job is gone; the user's intent stands.
                record.status = Status::Queued;
                recovery.resumed.push(id.clone());
            }
            if let Some(reason) = damage {
                record.status = Status::Failed;
                record.error = Some(
                    FailureCode::JournalCorrupt
                        .failure(format!("the committed results cannot be read: {reason}")),
                );
            }
            if interrupted || journal.is_none() {
                dir.write(&record)?;
            }
            next_sequence = next_sequence.max(record.sequence);
            let committed_until = segments.last().map_or(0, |segment| segment.end);
            let position = if record.status == Status::Completed {
                record.duration_samples.unwrap_or(committed_until)
            } else {
                committed_until
            };
            jobs.insert(
                id,
                Job {
                    record,
                    segments,
                    committed_until,
                    position,
                    activity: None,
                    journal,
                },
            );
        }
        let incoming = root.join(store::INCOMING);
        Ok(Self {
            root,
            incoming,
            options,
            state: Mutex::new(State {
                jobs,
                next_sequence,
            }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            recovery,
        })
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub(crate) fn ffmpeg(&self) -> &Ffmpeg {
        &self.options.ffmpeg
    }

    fn dir(&self, id: &str) -> JobDir {
        JobDir::new(&self.root, id)
    }

    fn announce(&self) {
        self.wake.notify_all();
    }

    fn snapshot(&self, job: &Job) -> JobSnapshot {
        let dir = self.dir(&job.record.id);
        JobSnapshot {
            request: job.record.request.clone(),
            source_path: job.record.source_path.clone(),
            media: dir.media(),
            partial: dir.partial(),
            sha256: job.record.sha256.clone(),
            needs_source: job.record.needs_source,
            staged_upload: job.record.staged_upload,
            decoder: job.record.decoder,
            duration_samples: job.record.duration_samples,
            committed_until: job.committed_until,
        }
    }

    // ---- Workers. Only the thread that holds a `Claim` calls these. ----

    /// Take the next queued job of `stage` and mark it running. The record
    /// is saved before the job is handed out, so a crash after this point
    /// finds a job to resume.
    pub(crate) fn claim(&self, stage: Stage) -> Option<Claim> {
        let mut state = self.state.lock();
        let id = state
            .jobs
            .values()
            .filter(|job| job.record.status == Status::Queued && job.record.stage == stage)
            .min_by_key(|job| job.record.sequence)
            .map(|job| job.record.id.clone())?;
        let dir = self.dir(&id);
        let job = state.jobs.get_mut(&id)?;
        // Nothing may be recognized for a job whose results cannot be saved.
        if stage == Stage::Transcribe && job.journal.is_none() {
            fail_unsaved(
                &dir,
                job,
                FailureCode::JournalCorrupt.failure(JOURNAL_CLOSED),
            );
            return None;
        }
        let mut record = job.record.clone();
        record.generation += 1;
        record.status = Status::Running;
        record.error = None;
        if stage == Stage::Transcribe {
            record.attempts += 1;
        }
        if let Err(error) = dir.write(&record) {
            // The durable state could not follow: the job does not start.
            fail_unsaved(
                &dir,
                job,
                FailureCode::Internal.failure(format!("cannot save the job state: {error}")),
            );
            return None;
        }
        job.record = record;
        job.activity = Some(match stage {
            Stage::Import => Activity::Copying,
            Stage::Transcribe => Activity::Recognizing,
        });
        let lease = JobLease {
            id: id.clone(),
            generation: job.record.generation,
        };
        Some(Claim {
            lease,
            snapshot: self.snapshot(job),
        })
    }

    /// Wait for queued work, for at most `timeout`. Returns at once when
    /// there is some, or when the queue is stopping.
    pub(crate) fn wait_for_work(&self, stage: Option<Stage>, timeout: Duration) {
        let mut state = self.state.lock();
        let queued = state.jobs.values().any(|job| {
            job.record.status == Status::Queued
                && stage.is_none_or(|stage| job.record.stage == stage)
        });
        if !queued && !self.stopped() {
            self.wake.wait_for(&mut state, timeout);
        }
    }

    /// Whether `lease` is still the run that holds its job.
    pub(crate) fn is_current(&self, lease: &JobLease) -> bool {
        self.state
            .lock()
            .jobs
            .get(&lease.id)
            .is_some_and(|job| job.runs(lease))
    }

    pub(crate) fn set_activity(&self, lease: &JobLease, activity: Option<Activity>) -> bool {
        let mut state = self.state.lock();
        let Some(job) = state.jobs.get_mut(&lease.id) else {
            return false;
        };
        if !job.runs(lease) {
            return false;
        }
        job.activity = activity;
        true
    }

    /// Commit `segments` and record that the run has read up to 16 kHz
    /// sample `position` of the media. Each segment is synced to the
    /// journal before it counts; a run that was superseded commits
    /// nothing.
    pub(crate) fn commit(&self, lease: &JobLease, segments: &[Segment], position: u64) -> bool {
        let mut state = self.state.lock();
        let dir = self.dir(&lease.id);
        let Some(job) = state.jobs.get_mut(&lease.id) else {
            return false;
        };
        if !job.runs(lease) {
            return false;
        }
        job.position = position;
        for segment in segments {
            let appended = job.journal.as_mut().map(|journal| journal.append(segment));
            match appended {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    fail_unsaved(
                        &dir,
                        job,
                        FailureCode::Internal
                            .failure(format!("cannot save a recognized segment: {error}")),
                    );
                    return false;
                }
                None => {
                    fail_unsaved(
                        &dir,
                        job,
                        FailureCode::JournalCorrupt.failure(JOURNAL_CLOSED),
                    );
                    return false;
                }
            }
            job.segments.push(segment.clone());
            job.committed_until = segment.end;
        }
        true
    }

    /// Change the record of the job `lease` holds while its run is still
    /// the current one, and make the change durable before it shows.
    /// `false` when the run was superseded or the change could not be
    /// saved.
    fn edit(&self, lease: &JobLease, change: impl FnOnce(&mut JobRecord)) -> bool {
        let mut state = self.state.lock();
        let dir = self.dir(&lease.id);
        let Some(job) = state.jobs.get_mut(&lease.id) else {
            return false;
        };
        if !job.runs(lease) {
            return false;
        }
        let mut record = job.record.clone();
        change(&mut record);
        if let Err(error) = dir.write(&record) {
            // Memory must not claim what disk does not hold.
            fail_unsaved(
                &dir,
                job,
                FailureCode::Internal.failure(format!("cannot save the job state: {error}")),
            );
            return false;
        }
        let running = record.status == Status::Running;
        job.record = record;
        if !running {
            job.activity = None;
        }
        true
    }

    /// The job owns its media: `bytes` of SHA-256 `sha256`.
    pub(crate) fn media_ready(&self, lease: &JobLease, bytes: u64, sha256: String) -> bool {
        self.edit(lease, |record| {
            record.bytes = Some(bytes);
            record.sha256 = Some(sha256);
            record.needs_source = false;
            record.staged_upload = false;
        })
    }

    /// A superseded copy may finish hashing but must never replace a new source.
    pub(crate) fn publish(
        &self,
        lease: &JobLease,
        partial: &Path,
        media: &Path,
    ) -> io::Result<bool> {
        let state = self.state.lock();
        if !state.jobs.get(&lease.id).is_some_and(|job| job.runs(lease)) {
            return Ok(false);
        }
        fs::rename(partial, media)?;
        Ok(true)
    }

    /// The media is measured; the job is queued for transcription.
    pub(crate) fn imported(
        &self,
        lease: &JobLease,
        decoder: MediaDecoder,
        duration_samples: u64,
    ) -> bool {
        let imported = self.edit(lease, |record| {
            record.decoder = Some(decoder);
            record.duration_samples = Some(duration_samples);
            record.stage = Stage::Transcribe;
            record.status = Status::Queued;
        });
        if imported {
            self.announce();
        }
        imported
    }

    pub(crate) fn complete(&self, lease: &JobLease) -> bool {
        self.edit(lease, |record| {
            record.status = Status::Completed;
            record.completed_at = Some(now());
        })
    }

    pub(crate) fn fail(&self, lease: &JobLease, failure: JobFailure) -> bool {
        self.edit(lease, |record| {
            record.status = Status::Failed;
            record.error = Some(failure);
        })
    }

    // ---- Job life ----

    pub(crate) fn list(&self) -> Vec<JobSummary> {
        let state = self.state.lock();
        let mut summaries: Vec<JobSummary> = state.jobs.values().map(Job::summary).collect();
        summaries.sort_unstable_by_key(|summary| summary.sequence);
        summaries
    }

    pub(crate) fn get(&self, id: &str) -> Result<JobView, QueueError> {
        let state = self.state.lock();
        let job = state
            .jobs
            .get(id)
            .ok_or_else(|| QueueError::NotFound(id.to_owned()))?;
        Ok(JobView {
            summary: job.summary(),
            segments: job.segments.clone(),
        })
    }

    pub(crate) fn submit(&self, request: ImportRequest) -> Result<String, QueueError> {
        let ImportRequest {
            source,
            name,
            mut recognition,
        } = request;
        if recognition.model.trim().is_empty() {
            return Err(QueueError::Invalid(
                "the recognition model must be named".to_owned(),
            ));
        }
        if recognition.language.as_deref() == Some("auto") {
            recognition.language = None;
        }
        if let Some(language) = &recognition.language {
            if recognition.transport != Transport::Local
                || recognition.model != "sense-voice-int8"
                || !matches!(language.as_str(), "en" | "zh" | "yue" | "ja" | "ko")
            {
                return Err(QueueError::Invalid(format!(
                    "unsupported language {language} for this model and transport"
                )));
            }
        }
        let Admitted {
            name,
            source_path,
            upload_bytes,
            upload,
            failure,
        } = admit(source, name, &recognition)?;
        let id = new_job_id();
        let dir = self.dir(&id);
        dir.create()?;
        // An upload is the owned media: it is moved in before any worker
        // can see the job. An upload that cannot run is dropped instead.
        let stored = match (upload, &failure) {
            (Some(upload), None) => upload.as_file().sync_all().and_then(|()| {
                upload
                    .persist(dir.media())
                    .map(drop)
                    .map_err(|error| error.error)
            }),
            _ => Ok(()),
        };
        let needs_source = source_path.is_some();
        let stored = stored.and_then(|()| {
            let mut state = self.state.lock();
            let sequence = state.next_sequence + 1;
            let record = JobRecord {
                version: store::RECORD_VERSION,
                id: id.clone(),
                sequence,
                created_at: now(),
                name,
                source_path,
                bytes: upload_bytes,
                sha256: None,
                decoder: None,
                duration_samples: None,
                request: recognition,
                status: if failure.is_none() {
                    Status::Queued
                } else {
                    Status::Failed
                },
                stage: Stage::Import,
                needs_source,
                staged_upload: false,
                attempts: 0,
                generation: 1,
                error: failure,
                completed_at: None,
            };
            let journal = dir
                .write(&record)
                .and_then(|()| Journal::open(&dir.journal()).map(|(journal, _)| journal));
            match journal {
                Ok(journal) => {
                    state.next_sequence = sequence;
                    state.jobs.insert(
                        id.clone(),
                        Job {
                            record,
                            segments: Vec::new(),
                            committed_until: 0,
                            position: 0,
                            activity: None,
                            journal: Some(journal),
                        },
                    );
                    Ok(())
                }
                Err(error) => Err(error),
            }
        });
        if let Err(error) = stored {
            let _ = dir.remove();
            return Err(QueueError::Io(error));
        }
        self.announce();
        Ok(id)
    }

    /// Move job `id` out of one of `from` into whatever `change` makes of
    /// its record, durably. Every command starts a new generation: what
    /// the previous one still has in flight no longer counts.
    fn transition(
        &self,
        id: &str,
        action: &'static str,
        from: &[Status],
        change: impl FnOnce(&mut JobRecord) -> Result<(), QueueError>,
    ) -> Result<(), QueueError> {
        let mut state = self.state.lock();
        let job = state
            .jobs
            .get_mut(id)
            .ok_or_else(|| QueueError::NotFound(id.to_owned()))?;
        if !from.contains(&job.record.status) {
            return Err(QueueError::InvalidTransition {
                id: id.to_owned(),
                status: job.record.status,
                action,
            });
        }
        let mut record = job.record.clone();
        record.generation += 1;
        change(&mut record)?;
        self.dir(id).write(&record)?;
        job.record = record;
        job.activity = None;
        drop(state);
        self.announce();
        Ok(())
    }

    pub(crate) fn pause(&self, id: &str) -> Result<(), QueueError> {
        self.transition(id, "be paused", PAUSABLE, |record| {
            record.status = Status::Paused;
            Ok(())
        })?;
        Ok(())
    }

    pub(crate) fn resume(&self, id: &str) -> Result<(), QueueError> {
        self.transition(id, "be resumed", &[Status::Paused], |record| {
            record.status = Status::Queued;
            Ok(())
        })?;
        Ok(())
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<(), QueueError> {
        self.transition(id, "be cancelled", CANCELLABLE, |record| {
            record.status = Status::Cancelled;
            record.error = None;
            Ok(())
        })?;
        Ok(())
    }

    pub(crate) fn retry(&self, id: &str) -> Result<(), QueueError> {
        self.transition(id, "be retried", RETRYABLE, |record| {
            if record.request.transport == Transport::Cloud && !record.request.cloud_authorized {
                return Err(QueueError::Invalid(NOT_AUTHORIZED.to_owned()));
            }
            record.status = Status::Queued;
            record.error = None;
            Ok(())
        })?;
        Ok(())
    }

    pub(crate) fn reselect(&self, id: &str, source: ImportSource) -> Result<(), QueueError> {
        self.transition(id, "be reselected", RESELECTABLE, |record| {
            if record.request.transport == Transport::Cloud && !record.request.cloud_authorized {
                return Err(QueueError::Invalid(NOT_AUTHORIZED.to_owned()));
            }
            if record.decoder.is_none() {
                record.sha256 = None;
            }
            record.needs_source = true;
            record.status = Status::Queued;
            record.stage = Stage::Import;
            record.error = None;
            record.staged_upload = matches!(&source, ImportSource::Upload(_));
            match source {
                ImportSource::Path(path) => {
                    record.source_path = Some(path);
                }
                ImportSource::Upload(file) => {
                    if file.as_file().metadata()?.len() == 0 {
                        return Err(QueueError::Invalid("the replacement file is empty".into()));
                    }
                    file.as_file().sync_all()?;
                    file.persist(self.dir(id).partial())
                        .map_err(|error| error.error)?;
                    record.source_path = None;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn remove(&self, id: &str) -> Result<(), QueueError> {
        {
            let mut state = self.state.lock();
            if state.jobs.remove(id).is_none() {
                return Err(QueueError::NotFound(id.to_owned()));
            }
        }
        self.dir(id).remove()?;
        Ok(())
    }

    pub(crate) fn export(
        &self,
        id: &str,
        format: ExportFormat,
    ) -> Result<ExportDocument, QueueError> {
        let view = self.get(id)?;
        Ok(export::render(&view, format))
    }
}

/// What is known about an import before anything is stored.
struct Admitted {
    name: String,
    source_path: Option<PathBuf>,
    /// Size of an upload, which is the owned media from the start.
    upload_bytes: Option<u64>,
    upload: Option<NamedTempFile>,
    /// Why the job cannot run, if that is already known.
    failure: Option<JobFailure>,
}

/// Look at what was submitted. A file that cannot be read, or a cloud job
/// nobody authorized, is not refused: it becomes a job that failed before
/// it started, so it is listed with its reason while the other files of
/// the batch go on.
fn admit(
    source: ImportSource,
    name: Option<String>,
    recognition: &RecognitionRequest,
) -> Result<Admitted, QueueError> {
    let mut admitted = match source {
        ImportSource::Path(path) => {
            let failure = match fs::metadata(&path) {
                Err(error) => Some(unreadable(&path, &error)),
                Ok(metadata) if !metadata.is_file() => Some(
                    FailureCode::Unreadable
                        .failure(format!("{} is not a regular file", path.display())),
                ),
                Ok(metadata) if metadata.len() == 0 => {
                    Some(FailureCode::Corrupt.failure("the file is empty"))
                }
                Ok(_) => None,
            };
            Admitted {
                name: listed(name, file_name(&path)),
                source_path: Some(path),
                upload_bytes: None,
                upload: None,
                failure,
            }
        }
        ImportSource::Upload(file) => {
            let bytes = file.as_file().metadata()?.len();
            Admitted {
                name: listed(name, "upload".to_owned()),
                source_path: None,
                upload_bytes: Some(bytes),
                failure: (bytes == 0)
                    .then(|| FailureCode::Corrupt.failure("the uploaded file is empty")),
                upload: Some(file),
            }
        }
    };
    if admitted.failure.is_none()
        && recognition.transport == Transport::Cloud
        && !recognition.cloud_authorized
    {
        admitted.failure = Some(FailureCode::CloudNotAuthorized.failure(NOT_AUTHORIZED));
    }
    Ok(admitted)
}

/// Mark `job` failed, in memory and as far as the store still allows:
/// for failures of the store itself.
fn fail_unsaved(dir: &JobDir, job: &mut Job, failure: JobFailure) {
    job.record.status = Status::Failed;
    job.record.error = Some(failure);
    job.activity = None;
    let _ = dir.write(&job.record);
}

fn listed(name: Option<String>, fallback: String) -> String {
    name.filter(|name| !name.trim().is_empty())
        .unwrap_or(fallback)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn unreadable(path: &Path, error: &io::Error) -> JobFailure {
    FailureCode::Unreadable.failure(format!("{} cannot be read: {error}", path.display()))
}

/// Random job id: 16 bytes of the OS random source, in hex.
fn new_job_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn spawn(name: &'static str, run: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(name.to_owned()).spawn(run)
}

/// The transcription queue of one data directory.
///
/// Open it once per runtime ([`JobQueue::open`]); a second process is
/// refused with [`QueueError::Busy`]. Dropping it stops the workers and
/// waits for the unit in flight, then releases the queue.
pub struct JobQueue {
    pub(crate) core: Arc<QueueCore>,
    /// Threads of [`JobQueue::start`], joined when the queue is dropped.
    workers: Mutex<Option<Vec<JoinHandle<()>>>>,
    /// The advisory lock: this process is the one serving the queue.
    _owner: File,
}

impl JobQueue {
    /// Open the queue under `<data dir>/jobs`, creating it when missing.
    ///
    /// Takes the directory's queue lock, removes what an interrupted
    /// process left behind and queues again the jobs it found running
    /// ([`JobQueue::recovery`]). No worker runs until
    /// [`JobQueue::start`]: a queue that is only read and exported needs
    /// none.
    pub fn open(data_dir: &Path, options: QueueOptions) -> Result<Self, QueueError> {
        fs::create_dir_all(data_dir)?;
        let root = data_dir.join(store::JOBS_DIR);
        store::private_dir(&root)?;
        let root = root.canonicalize()?;
        let owner = store::lock_queue(&root).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                QueueError::Busy
            } else {
                QueueError::Io(error)
            }
        })?;
        store::private_dir(&root.join(store::INCOMING))?;
        let core = QueueCore::load(root, options)?;
        Ok(Self {
            core: Arc::new(core),
            workers: Mutex::new(None),
            _owner: owner,
        })
    }

    /// What opening the queue found left behind, and did about it.
    pub fn recovery(&self) -> &Recovery {
        &self.core.recovery
    }

    /// Where an upload is written before it is submitted as
    /// [`ImportSource::Upload`].
    pub fn incoming_dir(&self) -> &Path {
        &self.core.incoming
    }

    /// Start the queue's threads: one that imports, one that transcribes,
    /// claiming jobs in submission order. `Err` when they already run.
    pub fn start(self: &Arc<Self>, workers: Workers) -> Result<(), QueueError> {
        let mut running = self.workers.lock();
        if running.is_some() {
            return Err(QueueError::AlreadyRunning);
        }
        let importing = spawn("voiceless-jobs-import", {
            let core = Arc::clone(&self.core);
            move || worker::import_loop(core)
        })?;
        let transcribing = spawn("voiceless-jobs-transcribe", {
            let core = Arc::clone(&self.core);
            move || worker::transcribe_loop(core, workers.recognition, workers.resource)
        });
        let transcribing = match transcribing {
            Ok(transcribing) => transcribing,
            Err(error) => {
                self.core.stop.store(true, Ordering::Release);
                self.core.wake.notify_all();
                let _ = importing.join();
                return Err(error.into());
            }
        };
        *running = Some(vec![importing, transcribing]);
        Ok(())
    }

    // ---- Job life: the console and the daemon's control paths. ----

    /// Add one file. The job is queued, or failed at once when the file
    /// cannot be read or a cloud job was not authorized to send it; the
    /// returned id names it either way.
    pub fn submit(&self, request: ImportRequest) -> Result<String, QueueError> {
        self.core.submit(request)
    }

    /// Every job in submission order, without its segments.
    pub fn list(&self) -> Vec<JobSummary> {
        self.core.list()
    }

    /// One job with the segments committed so far, in media order.
    pub fn get(&self, id: &str) -> Result<JobView, QueueError> {
        self.core.get(id)
    }

    /// Stop the job at its next unit boundary. The segments committed so
    /// far stay; `resume` continues from the last of them.
    pub fn pause(&self, id: &str) -> Result<(), QueueError> {
        self.core.pause(id)
    }

    pub fn resume(&self, id: &str) -> Result<(), QueueError> {
        self.core.resume(id)
    }

    /// Drop the job. Nothing more is recognized for it and it never
    /// completes; its owned media stays until it is removed, so the job
    /// can be retried or its committed segments exported.
    pub fn cancel(&self, id: &str) -> Result<(), QueueError> {
        self.core.cancel(id)
    }

    /// Queue a failed or cancelled job again. Committed segments are
    /// kept: the run continues after the last of them.
    pub fn retry(&self, id: &str) -> Result<(), QueueError> {
        self.core.retry(id)
    }

    /// Name the file of a job again, after its source or the queue's copy
    /// of it went missing. The file is verified against the digest of the
    /// imported media before it replaces anything; a mismatch fails the
    /// job and leaves it as it was.
    pub fn reselect(&self, id: &str, source: ImportSource) -> Result<(), QueueError> {
        self.core.reselect(id, source)
    }

    /// Delete the job with its media and results. A worker that still
    /// holds it stops at its next unit boundary.
    pub fn remove(&self, id: &str) -> Result<(), QueueError> {
        self.core.remove(id)
    }

    /// The job's committed segments as a file.
    pub fn export(&self, id: &str, format: ExportFormat) -> Result<ExportDocument, QueueError> {
        self.core.export(id, format)
    }

    // ---- Workers: for threads of the daemon's own, with `worker::import`
    // and `worker::transcribe`. ----

    /// Take the next queued job of `stage` and mark it running.
    pub fn claim(&self, stage: Stage) -> Option<Claim> {
        self.core.claim(stage)
    }

    /// Block for at most `timeout` until a job is queued or the queue is
    /// dropping.
    pub fn wait_for_work(&self, timeout: Duration) {
        self.core.wait_for_work(None, timeout)
    }

    /// Whether the run that holds `lease` is still the job's current one.
    pub fn is_current(&self, lease: &JobLease) -> bool {
        self.core.is_current(lease)
    }

    /// Say what the run holding `lease` does right now.
    pub fn set_activity(&self, lease: &JobLease, activity: Option<Activity>) -> bool {
        self.core.set_activity(lease, activity)
    }

    /// Commit `segments` of the run holding `lease`, as the queue's own
    /// workers do. `false` when the run was superseded or the journal
    /// could not be written.
    pub fn commit(&self, lease: &JobLease, segments: &[Segment], position: u64) -> bool {
        self.core.commit(lease, segments, position)
    }

    /// Report that the run holding `lease` owns its media.
    pub fn media_ready(&self, lease: &JobLease, bytes: u64, sha256: String) -> bool {
        self.core.media_ready(lease, bytes, sha256)
    }

    /// Report the media of the run holding `lease` as measured; the job
    /// is queued for transcription.
    pub fn imported(&self, lease: &JobLease, decoder: MediaDecoder, duration_samples: u64) -> bool {
        self.core.imported(lease, decoder, duration_samples)
    }

    /// End the run holding `lease` successfully.
    pub fn complete(&self, lease: &JobLease) -> bool {
        self.core.complete(lease)
    }

    /// End the run holding `lease` with `failure`.
    pub fn fail(&self, lease: &JobLease, failure: JobFailure) -> bool {
        self.core.fail(lease, failure)
    }
}

impl Drop for JobQueue {
    fn drop(&mut self) {
        self.core.stop.store(true, Ordering::Release);
        self.core.wake.notify_all();
        // Each worker ends the unit it is in (at most one copy block or
        // one segment decode) and exits without touching its job again.
        if let Some(workers) = self.workers.lock().take() {
            for worker in workers {
                let _ = worker.join();
            }
        }
    }
}
