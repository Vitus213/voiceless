//! Meeting capture: an optional workflow of its own that records a
//! selected microphone and, when the user asks for it, what the system
//! plays through a selected output, into one meeting that the background
//! queue transcribes afterwards. Nothing of it reaches the dictation
//! pipeline: a meeting never types into a window, plays no cue and writes
//! no history row.
//!
//! `MeetingService` owns a single `MeetingRuntime`, serializes commands, reaps
//! recorders independently of console clients, and links local jobs to their tracks.
//!
//! Lifecycle. `start` refreshes the device catalog, refuses a source that
//! left since it was selected, takes the data directory's capture lock (the
//! dictation's `runtime::lock_capture`: the same file, so a dictation
//! session and a meeting exclude each other in every process), creates the
//! meeting directory and opens one recorder per selected track. Linux runs
//! `pw-record`, macOS and Windows open native streams. A failed start
//! leaves nothing on disk. `stop` asks the recorders to drain. `poll`
//! returns the meeting once every track settled, as `Completed` when it ran
//! to its stop or as `Interrupted` when a failure ended it, and releases
//! the capture lock. `sources` lists what can be selected.
//!
//! Recovery. A track's audio is kept in committed ranges (`track`); a
//! runtime that dies loses at most the range it was writing. `sweep`, run
//! once at startup, turns the journals of recordings whose runtime died
//! into interrupted meetings, and only while no other process holds the
//! capture lock: a live recorder is never touched. The interface then
//! offers `recover`, which finalizes what was kept and returns the meeting
//! for the queue, or `discard`, which deletes it. `Limits` bound how much a
//! meeting records and how long interrupted ones wait.
//!
//! Failure behaviour: `MeetingError` refuses a request and leaves the
//! runtime as it was. A track that ends early fails its meeting, which is
//! reported with the source, the cause (`Failure`) and the actions that
//! help; its other tracks are stopped and kept.

#[cfg(target_os = "linux")]
mod graph;
#[cfg(not(target_os = "linux"))]
#[path = "meeting_capture/native.rs"]
mod graph;
mod journal;
mod model;
mod service;
mod track;

pub use model::{
    Action, Failure, Limits, Meeting, MeetingError, MeetingState, MeetingStatus, Scope, Selection,
    SourceCatalog, SourceRef, StoppedBy, SweepReport, Tools, TrackKind, TrackStatus, TrackView,
    TranscriptionInput,
};
pub use service::{start, MeetingService};

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use journal::{Journal, JournalDoc, TrackRecord};
use model::BYTES_PER_SECOND;
use track::{PrepareError, TrackHandle, TrackNotice};

/// What another capture is told when it finds the lock taken;
/// `runtime::lock_capture` says the same.
const CAPTURE_BUSY: &str = "Voiceless is already recording with this data directory";

/// The capture state and journals owned by one meeting service.
pub struct MeetingRuntime {
    data_dir: PathBuf,
    /// `<data directory>/meetings`.
    meetings: PathBuf,
    limits: Limits,
    tools: Tools,
    active: Option<Recording>,
    last: Option<Meeting>,
}

impl MeetingRuntime {
    /// A runtime for the meetings of `data_dir`, the daemon's data
    /// directory, with the default limits and the PipeWire tools of `PATH`.
    /// Nothing is read or written until one of the methods is called.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            meetings: journal::root(&data_dir),
            data_dir,
            limits: Limits::default(),
            tools: Tools::default(),
            active: None,
            last: None,
        }
    }

    /// Record under these limits, with values that cannot break a
    /// recording.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits.sanitized();
        self
    }

    /// Run these PipeWire tools instead of the ones on `PATH`.
    pub fn with_tools(mut self, tools: Tools) -> Self {
        self.tools = tools;
        self
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The directory every meeting of this runtime is kept in:
    /// `<data directory>/meetings`.
    pub fn data_location(&self) -> &Path {
        &self.meetings
    }

    /// The id of the meeting that records or is stopping.
    pub fn active_id(&self) -> Option<&str> {
        self.active.as_ref().map(|recording| recording.id.as_str())
    }

    /// Whether a meeting records or is stopping. While one does the runtime
    /// holds the data directory's capture lock: a dictation session cannot
    /// start.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// What can be selected now, from a fresh read of the PipeWire graph.
    /// A graph that cannot be read is `Failure::GraphUnreadable`.
    pub async fn sources(&mut self) -> Result<SourceCatalog, MeetingError> {
        let graph = graph::read(&self.tools)
            .await
            .map_err(|reason| Failure::GraphUnreadable { reason })?;
        Ok(graph.catalog())
    }

    /// Start a meeting with `selection`. The graph is read again first: a
    /// selected node that is not there is `Failure::SourceMissing`, never
    /// replaced. Another capture (a dictation session, a batch recording,
    /// a meeting) makes it `MeetingError::CaptureBusy`. Once this returns,
    /// the recorders run and the runtime holds the capture lock; a failure
    /// leaves nothing on disk.
    pub async fn start(&mut self, selection: Selection) -> Result<Meeting, MeetingError> {
        if let Some(active) = &self.active {
            return Err(MeetingError::MeetingActive {
                id: active.id.clone(),
            });
        }
        let graph = graph::read(&self.tools)
            .await
            .map_err(|reason| Failure::GraphUnreadable { reason })?;
        let mut specs = vec![graph.resolve(TrackKind::Microphone, selection.microphone)?];
        if let Some(node) = selection.system {
            specs.push(graph.resolve(TrackKind::System, node)?);
        }
        // The recorder is found before anything is made that would have to
        // be undone.
        #[cfg(target_os = "linux")]
        let recorder = crate::capture::recorder::RecorderProfile::detect(
            self.tools.record.clone(),
            self.tools.dump.clone(),
        )
        .await
        .map_err(|error| Failure::NoRecorder {
            source: TrackKind::Microphone,
            reason: error.to_string(),
        })?;
        let lock = lock_capture(&self.data_dir).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                MeetingError::CaptureBusy
            } else {
                MeetingError::Io(error)
            }
        })?;
        let (notices, receiver) = mpsc::unbounded_channel();
        let recording = match Recording::start(
            &self.meetings,
            #[cfg(target_os = "linux")]
            &recorder,
            &self.limits,
            &specs,
            lock,
            receiver,
            notices,
        ) {
            Ok(recording) => recording,
            Err(error) => return Err(error),
        };
        let view = recording.view();
        self.active = Some(recording);
        Ok(view)
    }

    /// Stop the active meeting: its recorders drain and its tracks
    /// finalize. The returned meeting is `Stopping` until `poll` has it.
    pub fn stop(&mut self) -> Result<Meeting, MeetingError> {
        let recording = self.active.as_mut().ok_or(MeetingError::NoMeeting)?;
        if recording.stop.is_some() {
            return Err(MeetingError::AlreadyStopping);
        }
        recording.request_stop(StopReason::User);
        Ok(recording.view())
    }

    /// Take note of what the recorders reported. Returns the meeting once
    /// it ended, `Completed` or `Interrupted`, and gives up the capture
    /// lock with it; call it on every turn of the owner's loop. Before that
    /// it returns `None`.
    pub fn poll(&mut self) -> Option<Meeting> {
        let recording = self.active.as_mut()?;
        recording.collect();
        if !recording.settled() {
            return None;
        }
        let meeting = recording.finish();
        self.active = None;
        self.last = Some(meeting.clone());
        Some(meeting)
    }

    /// Resolve once a track of the active meeting has something to report,
    /// so that an owner which selects on it need not poll for that. Pends
    /// while no meeting is active: guard the branch with `is_active`.
    pub async fn changed(&mut self) {
        let Some(recording) = self.active.as_mut() else {
            std::future::pending::<()>().await;
            return;
        };
        if let Some(notice) = recording.notices.recv().await {
            recording.notice(notice);
        }
    }

    /// The status for the daemon's snapshot and the console.
    pub fn status(&self) -> MeetingStatus {
        let (recoveries, recoveries_error) = match journal::directories(&self.meetings) {
            Ok(directories) => (waiting(self.listed(directories)), None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        MeetingStatus {
            data_location: self.meetings.clone(),
            active: self.active.as_ref().map(Recording::view),
            recoveries,
            recoveries_error,
            last: self.last.clone(),
            limits: self.limits,
        }
    }

    /// One meeting: the active one as it is now, any other as its journal
    /// says. `MeetingError::UnknownMeeting` for anything that is not a
    /// meeting this build can read.
    pub fn meeting(&self, id: &str) -> Result<Meeting, MeetingError> {
        if let Some(recording) = self.active.as_ref().filter(|recording| recording.id == id) {
            return Ok(recording.view());
        }
        let directory = self.directory_of(id)?;
        let doc = journal::read(&directory).map_err(|_| unknown(id))?;
        Ok(Meeting::from_journal(&directory, &doc, None))
    }

    /// Every meeting on disk that this build can read, oldest first; the
    /// active one included, live.
    pub fn meetings(&self) -> Vec<Meeting> {
        self.listed(journal::directories(&self.meetings).unwrap_or_default())
    }

    /// Interrupted meetings waiting for `recover` or `discard`, oldest
    /// first.
    pub fn recoveries(&self) -> Vec<Meeting> {
        waiting(self.meetings())
    }

    /// Look for recordings whose runtime died and make them interrupted
    /// meetings, then discard what exceeds the retention bounds. Run it
    /// once at startup. A journal that still says `recording` is only
    /// touched when no other process holds the capture lock: a recorder
    /// that runs is never cut short, and this runtime's own meeting is left
    /// alone.
    pub fn sweep(&mut self) -> Result<SweepReport, MeetingError> {
        let mut report = SweepReport::default();
        let hold = self.hold()?;
        report.recorder_active = matches!(hold, Hold::Busy);
        let mut interrupted = Vec::new();
        for (id, directory) in journal::directories(&self.meetings)? {
            let doc = match journal::read(&directory) {
                Ok(doc) => doc,
                // A directory made just before its runtime died holds nothing.
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound && self.orphan(&hold, &id) =>
                {
                    let _ = fs::remove_dir(&directory);
                    continue;
                }
                Err(_) => {
                    report.untouched.push(id);
                    continue;
                }
            };
            let doc = if doc.state == MeetingState::Recording {
                if !self.orphan(&hold, &id) {
                    continue;
                }
                match journal::interrupt(&directory, doc) {
                    Ok(doc) => {
                        report
                            .adopted
                            .push(Meeting::from_journal(&directory, &doc, None));
                        doc
                    }
                    Err(_) => {
                        report.untouched.push(id);
                        continue;
                    }
                }
            } else {
                doc
            };
            if doc.state == MeetingState::Interrupted {
                interrupted.push(Meeting::from_journal(&directory, &doc, None));
            }
        }
        drop(hold);
        self.apply_retention(&mut report, interrupted);
        Ok(report)
    }

    /// Finalize what an interrupted meeting kept and hand it over: its
    /// tracks with audio become WAVs for the queue, the failure that ended
    /// the meeting stays on the record. `NotInterrupted` for a meeting that
    /// did not wait for this.
    pub fn recover(&mut self, id: &str) -> Result<Meeting, MeetingError> {
        if self.active_id() == Some(id) {
            return Err(MeetingError::StillRecording(id.to_owned()));
        }
        let directory = self.directory_of(id)?;
        let mut doc = journal::read(&directory).map_err(|_| unknown(id))?;
        if doc.state != MeetingState::Interrupted {
            return Err(MeetingError::NotInterrupted(
                id.to_owned(),
                doc.state.name(),
            ));
        }
        for track in &mut doc.tracks {
            if track.status == TrackStatus::Finalized {
                continue;
            }
            match journal::finish_track(&directory, track.kind, track.committed_bytes) {
                Ok(path) => {
                    track.status = if path.is_some() {
                        TrackStatus::Finalized
                    } else {
                        TrackStatus::Empty
                    };
                }
                Err(error) => return Err(MeetingError::Io(error)),
            }
        }
        doc.state = MeetingState::Recovered;
        journal::write(&directory, &doc)?;
        journal::remove_scratch(&directory);
        Ok(Meeting::from_journal(&directory, &doc, None))
    }

    /// Delete a meeting's recordings. A meeting that still records, here or
    /// in another process, is `StillRecording`; the active one has to be
    /// stopped first. Only a meeting id names a directory.
    pub fn discard(&mut self, id: &str) -> Result<(), MeetingError> {
        if self.active_id() == Some(id) {
            return Err(MeetingError::StillRecording(id.to_owned()));
        }
        let directory = self.directory_of(id)?;
        let doc = journal::read(&directory).map_err(|_| unknown(id))?;
        // A journal that still says `recording` is whoever's the capture lock is.
        let evidence = if doc.state == MeetingState::Recording {
            Some(self.hold()?)
        } else {
            None
        };
        if matches!(evidence, Some(Hold::Busy)) {
            return Err(MeetingError::StillRecording(id.to_owned()));
        }
        journal::remove(&self.meetings, id)?;
        Ok(())
    }

    /// The directory of the meeting `id` names; anything else is an
    /// unknown meeting.
    fn directory_of(&self, id: &str) -> Result<PathBuf, MeetingError> {
        journal::directory(&self.meetings, id).map_err(|_| unknown(id))
    }

    /// The meetings in `directories`, oldest first. The active one is live;
    /// a journal this build cannot read is skipped.
    fn listed(&self, directories: Vec<(String, PathBuf)>) -> Vec<Meeting> {
        let mut meetings: Vec<Meeting> = directories
            .into_iter()
            .filter_map(|(_, directory)| {
                let doc = journal::read(&directory).ok()?;
                if let Some(recording) = self
                    .active
                    .as_ref()
                    .filter(|recording| recording.directory == directory)
                {
                    return Some(recording.view());
                }
                Some(Meeting::from_journal(&directory, &doc, None))
            })
            .collect();
        meetings.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        meetings
    }

    /// Who holds the data directory's capture lock, which says whose a
    /// journal that still says `recording` is: this runtime when it
    /// records, else whoever holds the lock.
    fn hold(&self) -> Result<Hold, MeetingError> {
        if self.active.is_some() {
            return Ok(Hold::Ours);
        }
        match lock_capture(&self.data_dir) {
            Ok(lock) => Ok(Hold::Free { _lock: lock }),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(Hold::Busy),
            Err(error) => Err(MeetingError::Io(error)),
        }
    }

    /// Whether the recording of `id` belongs to a runtime that is gone:
    /// nobody holds the capture lock, except this runtime, and it is not
    /// the one recording `id`.
    fn orphan(&self, hold: &Hold, id: &str) -> bool {
        match hold {
            Hold::Ours => self
                .active
                .as_ref()
                .is_none_or(|recording| recording.id != id),
            Hold::Free { .. } => true,
            Hold::Busy => false,
        }
    }

    /// Discard the interrupted meetings that exceed the retention bounds,
    /// oldest first; `report.interrupted` gets the ones that remain.
    fn apply_retention(&self, report: &mut SweepReport, mut interrupted: Vec<Meeting>) {
        interrupted.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let now = OffsetDateTime::now_utc();
        let hours = i64::try_from(self.limits.recovery_retention_hours).unwrap_or(i64::MAX);
        let retention = time::Duration::seconds(hours.saturating_mul(3600));
        for (index, meeting) in interrupted.iter().enumerate() {
            // A start time this build cannot read is not held against the meeting.
            let aged_out = OffsetDateTime::parse(&meeting.started_at, &Rfc3339)
                .is_ok_and(|started| now - started >= retention);
            if interrupted.len() - index <= self.limits.max_recoverable && !aged_out {
                continue;
            }
            match journal::remove(&self.meetings, &meeting.id) {
                Ok(()) => report.expired.push(meeting.clone()),
                Err(_) => report.untouched.push(meeting.id.clone()),
            }
        }
        report.interrupted = interrupted
            .into_iter()
            .filter(|meeting| !report.expired.contains(meeting))
            .collect();
    }
}

/// Who holds the data directory's capture lock.
enum Hold {
    /// This runtime: it records a meeting.
    Ours,
    /// Nobody. The lock was free and is held until this is dropped.
    Free { _lock: File },
    /// Another process.
    Busy,
}

/// What set a meeting on its way to end. The first one counts.
#[derive(Debug, Clone, PartialEq)]
enum StopReason {
    /// The user asked.
    User,
    /// A track reached its limit.
    Limit,
    /// A track failed.
    Failure(Failure),
}

/// One track to start, as `start` resolved it.
struct TrackSpec {
    kind: TrackKind,
    node: String,
    label: String,
    #[cfg(target_os = "linux")]
    serial: u64,
}

/// One meeting while its recorders run, owned by the runtime alone.
struct Recording {
    id: String,
    directory: PathBuf,
    /// The data directory's capture lock, held until every recorder is
    /// gone.
    _lock: File,
    journal: Arc<Journal>,
    tracks: Vec<TrackHandle>,
    notices: UnboundedReceiver<TrackNotice>,
    stop: Option<StopReason>,
}

impl Recording {
    /// Create the meeting, start its recorders and their threads. Every
    /// step that can fail comes before anything runs on its own, so a start
    /// that fails leaves nothing behind.
    fn start(
        meetings: &Path,
        #[cfg(target_os = "linux")] recorder: &crate::capture::recorder::RecorderProfile,
        limits: &Limits,
        specs: &[TrackSpec],
        lock: File,
        receiver: UnboundedReceiver<TrackNotice>,
        notices: UnboundedSender<TrackNotice>,
    ) -> Result<Self, MeetingError> {
        let id = journal::new_id()?;
        let directory = journal::create(meetings, &id)
            .map_err(|error| unwritable("create the meeting directory", &error))?;
        let doc = JournalDoc::new(
            &id,
            std::process::id(),
            specs
                .iter()
                .map(|spec| (spec.kind, spec.node.clone(), spec.label.clone())),
        );
        let journal = match Journal::create(&directory, doc) {
            Ok(journal) => Arc::new(journal),
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(unwritable("write the meeting journal", &error));
            }
        };
        let mut prepared = Vec::with_capacity(specs.len());
        let mut failure = None;
        for spec in specs {
            let spec = track::Spec {
                kind: spec.kind,
                node: &spec.node,
                #[cfg(target_os = "linux")]
                serial: spec.serial,
                #[cfg(target_os = "linux")]
                recorder,
                #[cfg(not(target_os = "linux"))]
                lease: &lock,
                directory: &directory,
                limits,
            };
            let track = track::Prepared::prepare(&spec, Arc::clone(&journal), notices.clone());
            match track {
                Ok(track) => prepared.push(track),
                Err(PrepareError::Storage(error)) => {
                    failure = Some(unwritable("open a track", &error));
                    break;
                }
                #[cfg(not(target_os = "linux"))]
                Err(PrepareError::Native(cause)) => {
                    failure = Some(cause.into());
                    break;
                }
                #[cfg(target_os = "linux")]
                Err(PrepareError::Recorder(error)) => {
                    failure = Some(
                        Failure::NoRecorder {
                            source: spec.kind,
                            reason: format!("{}: {error}", recorder.record().display()),
                        }
                        .into(),
                    );
                    break;
                }
            }
        }
        if let Some(failure) = failure {
            for track in prepared {
                track.kill();
            }
            let _ = fs::remove_dir_all(&directory);
            return Err(failure);
        }
        let mut recording = Self {
            id,
            directory,
            _lock: lock,
            journal,
            tracks: Vec::with_capacity(prepared.len()),
            notices: receiver,
            stop: None,
        };
        let mut prepared = prepared.into_iter();
        while let Some(track) = prepared.next() {
            let kind = track.kind;
            match track.start() {
                Ok(handle) => recording.tracks.push(handle),
                Err(error) => {
                    for pending in prepared {
                        pending.kill();
                    }
                    let directory = recording.directory.clone();
                    drop(recording);
                    let _ = fs::remove_dir_all(directory);
                    return Err(Failure::NoRecorder {
                        source: kind,
                        reason: format!("cannot start a recording thread: {error}"),
                    }
                    .into());
                }
            }
        }
        Ok(recording)
    }

    /// The meeting as it is now.
    fn view(&self) -> Meeting {
        Meeting::from_journal(
            &self.directory,
            &self.journal.get(),
            Some((&self.tracks, self.stop.is_some())),
        )
    }

    /// Put the meeting on its way to end: every recorder is asked to stop.
    fn request_stop(&mut self, reason: StopReason) {
        if self.stop.is_some() {
            return;
        }
        self.stop = Some(reason);
        for track in &self.tracks {
            track.shared.stop.store(true, Ordering::Release);
        }
    }

    /// A track failed: the meeting ends with that failure, unless an
    /// earlier one already ends it.
    fn escalate(&mut self, failure: Failure) {
        match self.stop.take() {
            Some(StopReason::Failure(first)) => self.stop = Some(StopReason::Failure(first)),
            _ => self.request_stop(StopReason::Failure(failure)),
        }
    }

    fn notice(&mut self, notice: TrackNotice) {
        match notice {
            TrackNotice::LimitReached => self.request_stop(StopReason::Limit),
            TrackNotice::Ended { kind, failure } => {
                if let Some(handle) = self.tracks.iter_mut().find(|handle| handle.kind == kind) {
                    handle.ended = true;
                }
                if let Some(failure) = failure {
                    self.escalate(failure);
                }
            }
        }
    }

    fn collect(&mut self) {
        while let Ok(notice) = self.notices.try_recv() {
            self.notice(notice);
        }
        let mut lost: Option<Failure> = None;
        for track in &mut self.tracks {
            if track.ended {
                if let Some(thread) = track.thread.take() {
                    let _ = thread.join();
                }
                continue;
            }
            // A thread that ended without reporting is a bug in the recorder: its meeting ends like a failed track.
            if track.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                if track.thread.take().expect("finished thread").join().is_ok() {
                    // Its notice may have arrived just after try_recv returned empty.
                    continue;
                }
                track.ended = true;
                track.shared.kill();
                lost = lost.or_else(|| {
                    Some(Failure::RecorderFailed {
                        source: track.kind,
                        reason: "the recording thread ended without reporting".to_owned(),
                    })
                });
            }
        }
        if let Some(failure) = lost {
            self.escalate(failure);
        }
    }

    /// Whether every track is done and its thread gone.
    fn settled(&self) -> bool {
        self.tracks
            .iter()
            .all(|track| track.ended && track.thread.is_none())
    }

    /// Write the end of the meeting to its journal and say how it ended.
    fn finish(&mut self) -> Meeting {
        let stopped_by = match self.stop {
            Some(StopReason::User) => Some(StoppedBy::User),
            Some(StopReason::Limit) => Some(StoppedBy::Limit),
            _ => None,
        };
        // No track ends without a stop being asked for. Were the journals
        // ever to disagree, the audio is kept and the user decides.
        let failure = match self.stop.take() {
            Some(StopReason::Failure(failure)) => Some(failure),
            Some(StopReason::User | StopReason::Limit) => None,
            None => Some(Failure::RecorderFailed {
                source: self
                    .tracks
                    .first()
                    .map_or(TrackKind::Microphone, |track| track.kind),
                reason: "the recorders stopped without a stop being asked for".to_owned(),
            }),
        };
        let state = if failure.is_some() {
            MeetingState::Interrupted
        } else {
            MeetingState::Completed
        };
        let ended_at = journal::now();
        let written = self.journal.update(|doc| {
            doc.state = state;
            doc.stopped_by = stopped_by;
            doc.failure = failure.clone();
            doc.ended_at = Some(ended_at);
        });
        let doc = match written {
            Ok(()) => self.journal.get(),
            Err(error) => {
                let failure = Failure::DataUnwritable {
                    reason: format!(
                        "cannot write the end of the meeting into {}: {error}",
                        self.directory.display()
                    ),
                };
                let _ = self.journal.update(|doc| {
                    doc.state = MeetingState::Interrupted;
                    doc.failure = Some(failure.clone());
                });
                let mut doc = JournalDoc::clone(&self.journal.get());
                if doc.state != MeetingState::Interrupted {
                    // Not even the failure could be journaled. The journal
                    // on disk still says `recording`, which the next sweep
                    // reads as interrupted.
                    doc.state = MeetingState::Interrupted;
                    doc.failure = Some(failure);
                }
                Arc::new(doc)
            }
        };
        Meeting::from_journal(&self.directory, &doc, None)
    }

    /// End every recorder now, for an owner that will not read the
    /// outcome. The threads settle their files on their own.
    fn abort(&self) {
        for track in &self.tracks {
            track.shared.abort.store(true, Ordering::Release);
            if let Some(child) = track.shared.child.lock().as_mut() {
                let _ = child.kill();
            }
        }
    }
}

impl Drop for Recording {
    /// An owner that goes away while recording leaves no recorder behind.
    /// The journal then still says `recording`, which the next sweep reads
    /// as interrupted.
    fn drop(&mut self) {
        self.abort();
        for track in &mut self.tracks {
            if let Some(thread) = track.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Meeting {
    /// A meeting from its journal. `live` gives the tracks of the recording
    /// it belongs to and whether its stop was asked for.
    fn from_journal(
        directory: &Path,
        doc: &JournalDoc,
        live: Option<(&[TrackHandle], bool)>,
    ) -> Self {
        let (handles, stopping) = live.unwrap_or((&[], false));
        let state = match (doc.state, stopping) {
            (MeetingState::Recording, true) => MeetingState::Stopping,
            (state, _) => state,
        };
        let tracks = doc
            .tracks
            .iter()
            .map(|entry| {
                let handle = handles.iter().find(|handle| handle.kind == entry.kind);
                let recording = handle.filter(|_| entry.status == TrackStatus::Recording);
                TrackView {
                    kind: entry.kind,
                    job_id: entry.job_id.clone(),
                    node: entry.node.clone(),
                    label: entry.label.clone(),
                    status: entry.status,
                    committed_bytes: entry.committed_bytes,
                    received_bytes: recording.map_or(entry.committed_bytes, |handle| {
                        handle.shared.received.load(Ordering::Relaxed)
                    }),
                    level: recording
                        .map(|handle| f32::from_bits(handle.shared.level.load(Ordering::Relaxed))),
                    started_at: doc.started_at.clone(),
                    first_audio_at: entry.first_audio_at.clone(),
                    path: (entry.status == TrackStatus::Finalized)
                        .then(|| journal::final_path(directory, entry.kind))
                        .filter(|path| path.is_file()),
                    error: entry.error.clone(),
                }
            })
            .collect();
        let source = |track: &TrackRecord| SourceRef {
            node: track.node.clone(),
            label: track.label.clone(),
        };
        let mut meeting = Self {
            id: doc.id.clone(),
            state,
            started_at: doc.started_at.clone(),
            ended_at: doc.ended_at.clone(),
            audio_seconds: doc.kept_bytes() as f64 / BYTES_PER_SECOND as f64,
            data_location: directory.to_path_buf(),
            scope: Scope {
                microphone: source(
                    doc.tracks
                        .iter()
                        .find(|track| track.kind == TrackKind::Microphone)
                        .expect("a meeting has a microphone track"),
                ),
                system: doc
                    .tracks
                    .iter()
                    .find(|track| track.kind == TrackKind::System)
                    .map(source),
            },
            tracks,
            stopped_by: doc.stopped_by,
            failure: doc.failure.clone(),
            actions: Vec::new(),
        };
        meeting.actions = meeting.actions();
        meeting
    }
}

/// The meetings that wait for recover or discard.
fn waiting(meetings: Vec<Meeting>) -> Vec<Meeting> {
    meetings
        .into_iter()
        .filter(|meeting| meeting.state == MeetingState::Interrupted)
        .collect()
}

/// The failure of a data directory that cannot hold a meeting.
fn unwritable(what: &str, error: &io::Error) -> MeetingError {
    Failure::DataUnwritable {
        reason: format!("cannot {what}: {error}"),
    }
    .into()
}

fn unknown(id: &str) -> MeetingError {
    MeetingError::UnknownMeeting(id.to_owned())
}

/// Take the data directory's capture lock, the one `runtime::lock_capture`
/// hands to a dictation session. It is the same file, so a meeting and a
/// dictation session exclude each other in this process and in any other.
fn lock_capture(data_dir: &Path) -> io::Result<File> {
    use std::fs::TryLockError;
    fs::create_dir_all(data_dir)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(data_dir.join("capture.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            CAPTURE_BUSY.to_owned(),
        )),
        Err(TryLockError::Error(error)) => Err(error),
    }
}
