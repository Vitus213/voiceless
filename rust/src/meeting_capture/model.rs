//! The meeting capture vocabulary shared by the runtime, the daemon, the
//! console and the background queue: what can be selected, what a meeting
//! looks like while it records and after it ended, why one could not run
//! and what the user can do about it. Everything here is plain data that
//! serializes as it is; `MeetingRuntime`, in the module root, owns the
//! lifecycle. The JSON names are the console's identifiers.
//!
//! Failure behaviour: `MeetingError` is the refusal of a request (another
//! meeting is active, the microphone is taken, the meeting is unknown) and
//! `code()` is its stable wire name. `Failure` is why a recording could not
//! run or ended early, with the actions that help. A meeting that cannot
//! record is refused with the same `Failure` and leaves nothing on disk.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Sample rate of every meeting track: the format the whole crate decodes
/// (`batch::load_wav_pcm16`), mono signed 16-bit little-endian.
pub const SAMPLE_RATE: u32 = crate::capture::SAMPLE_RATE;

/// PCM bytes one second of one track takes: `SAMPLE_RATE` samples of two.
pub const BYTES_PER_SECOND: u64 = SAMPLE_RATE as u64 * 2;

/// One recording of a meeting: the microphone, or the monitor of an output
/// (what the system plays through it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrackKind {
    Microphone,
    System,
}

impl TrackKind {
    /// Wire name: `microphone` or `system`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::System => "system",
        }
    }

    /// File name of the track's WAV inside its meeting directory.
    pub(super) fn file(self) -> String {
        format!("{}.wav", self.name())
    }
}

impl fmt::Display for TrackKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Microphone => "microphone",
            Self::System => "system output",
        })
    }
}

/// One selectable PipeWire node: its `node.name`, which lasts across
/// restarts and replugging, and the label the settings show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// `node.name`: what a `Selection` names and the journal keeps.
    pub node: String,
    /// `node.description`, else `node.nick`, else the name.
    pub label: String,
}

/// What a meeting records. The microphone is required; the system output
/// is optional and one of `SourceCatalog::system_outputs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// `node.name` of an `Audio/Source` node.
    pub microphone: String,
    /// `node.name` of an `Audio/Sink` node, whose monitor is recorded.
    pub system: Option<String>,
}

/// The sources a meeting was started with: its scope as the interface
/// shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub microphone: SourceRef,
    pub system: Option<SourceRef>,
}

/// What PipeWire offers a meeting, from one read of its graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCatalog {
    /// `Audio/Source` nodes, sorted by label.
    pub microphones: Vec<SourceRef>,
    /// `Audio/Sink` nodes whose monitor carries what the system plays,
    /// sorted by label.
    pub system_outputs: Vec<SourceRef>,
    /// `node.name` the system default names as its microphone, when it
    /// names one.
    pub default_microphone: Option<String>,
    /// `node.name` the system default names as its output, when it names
    /// one.
    pub default_system_output: Option<String>,
}

/// Where a meeting is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeetingState {
    /// Recorders run and append to the tracks.
    Recording,
    /// A stop was asked for; the recorders drain and the tracks finalize.
    /// Never written to the journal: a runtime that dies here leaves
    /// `Recording` behind.
    Stopping,
    /// Stopped as asked. Every track with audio is a finalized WAV.
    Completed,
    /// A failure ended the recording. The tracks keep what was safely
    /// written until the user recovers or discards the meeting.
    Interrupted,
    /// An interrupted meeting whose kept tracks were finalized on the
    /// user's request.
    Recovered,
}

impl MeetingState {
    /// Wire name, the same as the JSON value.
    pub fn name(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Recovered => "recovered",
        }
    }
}

/// How much of a track there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrackStatus {
    /// Audio is being appended.
    Recording,
    /// A finalized WAV of `committed_bytes` PCM bytes.
    Finalized,
    /// No audio was written.
    Empty,
    /// Its own failure ended it before it was finalized; `error` says why.
    Failed,
    /// The runtime died before it was finalized. Recovery finalizes what it
    /// kept.
    Unfinished,
}

/// Who ended a meeting that ran to its stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoppedBy {
    /// The user asked for the stop.
    User,
    /// A track reached `Limits::max_track_bytes`.
    Limit,
}

/// What the user can do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Start again once the cause is fixed.
    Retry,
    /// Pick another microphone or output.
    ChooseSource,
    /// Give Voiceless access to the source in the system's settings.
    GrantAccess,
    /// Make room for the data directory, then start again.
    FreeSpace,
    /// Finalize what an interrupted meeting kept, to transcribe it.
    Recover,
    /// Delete an interrupted meeting's recordings.
    Discard,
}

/// Why a recording could not run or ended early. Serialized as one object
/// whose `kind` is `Failure::kind`; only the fields of that kind are
/// present, so `permission-denied` reads as `{"kind": "permission-denied",
/// "source": "microphone", "node": "...", "reason": "...", "remedy":
/// "..."}` and `process-interrupted` as `{"kind": "process-interrupted"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Failure {
    /// PipeWire could not be asked for its graph: `pw-dump` is absent, no
    /// daemon answers or its output is unusable.
    GraphUnreadable { reason: String },
    /// The selected node is not in the graph.
    SourceMissing { source: TrackKind, node: String },
    /// The recorder program could not be started for a track.
    NoRecorder { source: TrackKind, reason: String },
    /// PipeWire reported the stream as not permitted. `remedy` says where
    /// access is decided on this machine.
    PermissionDenied {
        source: TrackKind,
        node: String,
        reason: String,
        remedy: String,
    },
    /// The stream was not connected to a source that was in the graph when
    /// the meeting started, and no audio ever came.
    StreamUnconnected {
        source: TrackKind,
        node: String,
        reason: String,
    },
    /// A source that was recording stopped delivering audio.
    StreamEnded {
        source: TrackKind,
        node: String,
        reason: String,
    },
    /// The recorder misbehaved: an exit nobody asked for with nothing to
    /// learn from it, or a torn sample.
    RecorderFailed { source: TrackKind, reason: String },
    /// The data directory could not hold the recording.
    DataUnwritable { reason: String },
    /// The runtime that was recording went away without stopping the
    /// meeting.
    ProcessInterrupted,
}

impl std::error::Error for Failure {}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GraphUnreadable { reason } => {
                write!(f, "audio sources are unavailable: {reason}")
            }
            Self::SourceMissing { source, node } => write!(
                f,
                "selected {source} {node:?} is unavailable; reconnect it or choose another source"
            ),
            Self::NoRecorder { source, reason } => {
                write!(f, "cannot start the recorder for the {source}: {reason}")
            }
            Self::PermissionDenied {
                source,
                node,
                reason,
                ..
            } => write!(
                f,
                "the system denied access to the {source} {node:?}: {reason}"
            ),
            Self::StreamUnconnected {
                source,
                node,
                reason,
            } => write!(f, "the {source} {node:?} could not be connected: {reason}"),
            Self::StreamEnded {
                source,
                node,
                reason,
            } => write!(
                f,
                "the {source} {node:?} stopped delivering audio: {reason}"
            ),
            Self::RecorderFailed { source, reason } => {
                write!(f, "the recorder for the {source} failed: {reason}")
            }
            Self::DataUnwritable { reason } => {
                write!(f, "the recording could not be kept: {reason}")
            }
            Self::ProcessInterrupted => {
                f.write_str("the runtime stopped while the meeting was recording")
            }
        }
    }
}

impl Failure {
    /// Stable wire name, the `kind` of the JSON form.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::GraphUnreadable { .. } => "graph-unreadable",
            Self::SourceMissing { .. } => "source-missing",
            Self::NoRecorder { .. } => "no-recorder",
            Self::PermissionDenied { .. } => "permission-denied",
            Self::StreamUnconnected { .. } => "stream-unconnected",
            Self::StreamEnded { .. } => "stream-ended",
            Self::RecorderFailed { .. } => "recorder-failed",
            Self::DataUnwritable { .. } => "data-unwritable",
            Self::ProcessInterrupted => "process-interrupted",
        }
    }

    /// The track the failure is about, when it is about one.
    pub fn source(&self) -> Option<TrackKind> {
        match self {
            Self::SourceMissing { source, .. }
            | Self::NoRecorder { source, .. }
            | Self::PermissionDenied { source, .. }
            | Self::StreamUnconnected { source, .. }
            | Self::StreamEnded { source, .. }
            | Self::RecorderFailed { source, .. } => Some(*source),
            Self::GraphUnreadable { .. }
            | Self::DataUnwritable { .. }
            | Self::ProcessInterrupted => None,
        }
    }

    /// What helps with the cause itself, in the order the interface offers
    /// it. Recover and discard come from the meeting, see `Meeting::actions`.
    pub fn actions(&self) -> &'static [Action] {
        match self {
            Self::GraphUnreadable { .. }
            | Self::NoRecorder { .. }
            | Self::RecorderFailed { .. } => &[Action::Retry],
            Self::SourceMissing { .. } => &[Action::ChooseSource],
            Self::PermissionDenied { .. } => &[Action::GrantAccess, Action::ChooseSource],
            Self::StreamUnconnected { .. } | Self::StreamEnded { .. } => {
                &[Action::ChooseSource, Action::Retry]
            }
            Self::DataUnwritable { .. } => &[Action::FreeSpace, Action::Retry],
            Self::ProcessInterrupted => &[],
        }
    }
}

/// Why a request was refused.
#[derive(Debug, thiserror::Error)]
pub enum MeetingError {
    #[error("a meeting recording is already active ({id})")]
    MeetingActive { id: String },
    #[error("no meeting is recording")]
    NoMeeting,
    #[error("the meeting is already stopping")]
    AlreadyStopping,
    /// The same lock, file and message as a dictation session's
    /// `runtime::lock_capture`: another capture is recording with this
    /// data directory, in this or another process.
    #[error("Voiceless is already recording with this data directory")]
    CaptureBusy,
    #[error("unknown meeting {0}")]
    UnknownMeeting(String),
    #[error("meeting {0} is still recording")]
    StillRecording(String),
    #[error("meeting {0} is {1}, not interrupted")]
    NotInterrupted(String, &'static str),
    /// The recording could not be made; nothing was left on disk.
    #[error(transparent)]
    Failure(#[from] Failure),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl MeetingError {
    /// Stable wire name for the console's error answers.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MeetingActive { .. } => "meeting-active",
            Self::NoMeeting => "no-meeting",
            Self::AlreadyStopping => "already-stopping",
            Self::CaptureBusy => "capture-busy",
            Self::UnknownMeeting(_) => "unknown-meeting",
            Self::StillRecording(_) => "still-recording",
            Self::NotInterrupted(..) => "not-interrupted",
            Self::Failure(failure) => failure.kind(),
            Self::Io(_) => "io",
        }
    }
}

/// One track of a meeting as the interface and the queue see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackView {
    pub kind: TrackKind,
    pub job_id: Option<String>,
    /// `node.name` that was recorded.
    pub node: String,
    pub label: String,
    pub status: TrackStatus,
    /// PCM bytes that are on disk for good: synced and journaled. Once the
    /// track ended it is the size of the recording.
    pub committed_bytes: u64,
    /// PCM bytes the recorder delivered so far, including what is not
    /// committed yet; equals `committed_bytes` once the track ended.
    pub received_bytes: u64,
    /// Loudness (RMS, 0..1) of the last audio written; `None` before the
    /// first audio and once the track ended.
    pub level: Option<f32>,
    pub started_at: String,
    /// When the first audio of the track arrived; `None` until then.
    pub first_audio_at: Option<String>,
    /// The finalized WAV; `None` until the track is `Finalized`.
    pub path: Option<PathBuf>,
    /// Why this track ended early or has no file, when it did.
    pub error: Option<String>,
}

/// A meeting as the daemon, the console and the queue see it: live while
/// it records, and the record of it afterwards.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Meeting {
    pub id: String,
    pub state: MeetingState,
    /// RFC 3339 UTC, when the recorders were started.
    pub started_at: String,
    /// RFC 3339 UTC; `None` while the meeting records and for a meeting
    /// the runtime did not get to stop.
    pub ended_at: Option<String>,
    /// Seconds of audio in the longest kept track.
    pub audio_seconds: f64,
    /// The directory Voiceless keeps this meeting's files in.
    pub data_location: PathBuf,
    pub scope: Scope,
    pub tracks: Vec<TrackView>,
    /// Set when the meeting ran to its stop.
    pub stopped_by: Option<StoppedBy>,
    /// Set when the meeting did not run to its stop.
    pub failure: Option<Failure>,
    /// What the user can do next, in the order the interface offers it.
    pub actions: Vec<Action>,
}

impl Meeting {
    /// The actions this meeting offers: those of its failure, then recover
    /// for an interrupted meeting that kept audio, then discard for every
    /// interrupted meeting.
    pub fn actions(&self) -> Vec<Action> {
        let mut actions: Vec<Action> = self
            .failure
            .iter()
            .flat_map(|failure| failure.actions().iter().copied())
            .collect();
        if self.state == MeetingState::Interrupted {
            if self.tracks.iter().any(|track| track.committed_bytes > 0) {
                actions.push(Action::Recover);
            }
            actions.push(Action::Discard);
        }
        actions.dedup();
        actions
    }

    /// The tracks the background queue can transcribe: every finalized one,
    /// in track order. They are 16 kHz mono PCM16 WAV, what
    /// `batch::load_wav_pcm16` reads; transcribing them is the queue's
    /// batch path, which never types into a window.
    pub fn transcription_inputs(&self) -> Vec<TranscriptionInput> {
        self.tracks
            .iter()
            .filter(|track| track.status == TrackStatus::Finalized)
            .filter_map(|track| {
                let path = track.path.as_ref()?;
                Some(TranscriptionInput {
                    meeting_id: self.id.clone(),
                    track: track.kind,
                    path: path.clone(),
                    bytes: track.committed_bytes,
                    seconds: track.committed_bytes as f64 / BYTES_PER_SECOND as f64,
                })
            })
            .collect()
    }
}

/// One finalized track as handed to the background queue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TranscriptionInput {
    pub meeting_id: String,
    pub track: TrackKind,
    /// A WAV of 16 kHz mono PCM16 that `batch::load_wav_pcm16` decodes.
    pub path: PathBuf,
    /// PCM bytes in the file, without its 44-byte header.
    pub bytes: u64,
    pub seconds: f64,
}

/// What the runtime reports at a glance; the body of the console's status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeetingStatus {
    /// The directory every meeting of this data directory is kept in:
    /// `<data directory>/meetings`.
    pub data_location: PathBuf,
    /// The meeting that records or is stopping.
    pub active: Option<Meeting>,
    /// Interrupted meetings waiting for recover or discard, oldest first.
    pub recoveries: Vec<Meeting>,
    /// Why `recoveries` could not be read from disk, when it could not.
    pub recoveries_error: Option<String>,
    /// The last meeting that ended in this runtime, kept for clients that
    /// reconnect after it did.
    pub last: Option<Meeting>,
    pub limits: Limits,
}

/// What one sweep of the meetings directory found and did.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SweepReport {
    /// Recordings whose runtime died while they ran, now interrupted.
    pub adopted: Vec<Meeting>,
    /// Interrupted meetings waiting for recover or discard, oldest first:
    /// those just adopted and those found earlier.
    pub interrupted: Vec<Meeting>,
    /// Interrupted meetings the retention bounds discarded.
    pub expired: Vec<Meeting>,
    /// Meeting ids the sweep could not read or remove. Their files are
    /// left alone.
    pub untouched: Vec<String>,
    /// Another process holds the capture lock, so recordings it may own
    /// were not inspected.
    pub recorder_active: bool,
}

/// Bounds of one runtime's meetings: how long a track may grow, how often
/// audio is made durable, how long a recorder may take to stop, and how
/// long an interrupted meeting waits for the user's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Longest track, in PCM bytes (`BYTES_PER_SECOND` per second). A
    /// meeting whose first track reaches it is stopped and completed.
    /// Default: four hours.
    pub max_track_bytes: u64,
    /// PCM bytes between two commits of a track. A commit syncs what the
    /// track wrote since the previous one and journals it as kept, so a
    /// runtime that dies loses at most this much per track. Default:
    /// 256 KiB, 8.2 seconds.
    pub commit_bytes: u64,
    /// How long a recorder may take to drain once it was asked to stop
    /// before it is killed. Default: three seconds.
    pub stop_grace_ms: u64,
    /// Interrupted meetings kept at most; older ones are discarded by the
    /// next sweep. Default: 10.
    pub max_recoverable: usize,
    /// How long an interrupted meeting is kept; older ones are discarded by
    /// the next sweep. Default: 168, a week of hours.
    pub recovery_retention_hours: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_track_bytes: 4 * 60 * 60 * BYTES_PER_SECOND,
            commit_bytes: 256 * 1024,
            stop_grace_ms: 3_000,
            max_recoverable: 10,
            recovery_retention_hours: 7 * 24,
        }
    }
}

impl Limits {
    /// The limits with values that cannot break a recording: byte counts
    /// are whole samples of at least one, a commit never exceeds a track,
    /// and a stop outlasts the recorder's longest wait for audio.
    pub(super) fn sanitized(self) -> Self {
        let samples = |bytes: u64| bytes.max(2) & !1;
        let max_track_bytes = samples(self.max_track_bytes.min(u64::from(u32::MAX - 44)));
        Self {
            max_track_bytes,
            commit_bytes: samples(self.commit_bytes).min(max_track_bytes),
            // The recorder looks at its flags every 100 ms; a shorter grace
            // would cut a healthy drain.
            stop_grace_ms: self.stop_grace_ms.max(200),
            max_recoverable: self.max_recoverable,
            recovery_retention_hours: self.recovery_retention_hours,
        }
    }
}

/// The PipeWire tools a runtime runs, by path. The defaults are looked up
/// on `PATH` like the dictation's; tests name stand-ins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    /// Prints the PipeWire graph as JSON; ships with `pw-record`.
    pub dump: PathBuf,
    /// Records a node's audio as raw PCM16.
    pub record: PathBuf,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            dump: PathBuf::from("pw-dump"),
            record: PathBuf::from("pw-record"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_that_cannot_record_are_straightened() {
        let limits = Limits {
            max_track_bytes: 1,
            commit_bytes: 4097,
            stop_grace_ms: 0,
            max_recoverable: 2,
            recovery_retention_hours: 1,
        }
        .sanitized();
        assert_eq!(limits.max_track_bytes, 2);
        assert_eq!(limits.commit_bytes, 2);
        assert_eq!(limits.stop_grace_ms, 200);
        assert_eq!(limits.max_recoverable, 2);
        assert_eq!(limits.recovery_retention_hours, 1);
        let limits = Limits {
            commit_bytes: 1 << 40,
            stop_grace_ms: 9_999,
            ..Limits::default()
        }
        .sanitized();
        assert_eq!(limits.commit_bytes, limits.max_track_bytes);
        assert_eq!(limits.stop_grace_ms, 9_999);
    }

    #[test]
    fn a_failure_offers_the_actions_that_help_its_cause() {
        let denied = Failure::PermissionDenied {
            source: TrackKind::System,
            node: "alsa_output.board".to_owned(),
            reason: "permission denied".to_owned(),
            remedy: "grant access".to_owned(),
        };
        assert_eq!(denied.kind(), "permission-denied");
        assert_eq!(denied.source(), Some(TrackKind::System));
        assert_eq!(
            denied.actions(),
            &[Action::GrantAccess, Action::ChooseSource]
        );
        assert_eq!(
            denied.to_string(),
            "the system denied access to the system output \"alsa_output.board\": permission denied"
        );
        assert_eq!(Failure::ProcessInterrupted.source(), None);
        assert_eq!(Failure::ProcessInterrupted.actions(), &[]);

        let json = serde_json::to_value(&denied).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "permission-denied",
                "source": "system",
                "node": "alsa_output.board",
                "reason": "permission denied",
                "remedy": "grant access",
            })
        );
        assert_eq!(
            serde_json::to_value(Failure::ProcessInterrupted).unwrap(),
            serde_json::json!({"kind": "process-interrupted"})
        );
        assert_eq!(
            serde_json::from_value::<Failure>(serde_json::json!({
                "kind": "stream-ended", "source": "microphone", "node": "n", "reason": "r"
            }))
            .unwrap(),
            Failure::StreamEnded {
                source: TrackKind::Microphone,
                node: "n".to_owned(),
                reason: "r".to_owned(),
            }
        );
    }

    #[test]
    fn an_interrupted_meeting_offers_what_its_audio_allows() {
        let track = |kind, bytes| TrackView {
            kind,
            job_id: None,
            node: String::new(),
            label: String::new(),
            status: TrackStatus::Failed,
            committed_bytes: bytes,
            received_bytes: bytes,
            level: None,
            started_at: String::new(),
            first_audio_at: None,
            path: None,
            error: None,
        };
        let mut meeting = Meeting {
            id: String::new(),
            state: MeetingState::Interrupted,
            started_at: String::new(),
            ended_at: None,
            audio_seconds: 0.0,
            data_location: PathBuf::new(),
            scope: Scope {
                microphone: SourceRef {
                    node: String::new(),
                    label: String::new(),
                },
                system: None,
            },
            tracks: vec![track(TrackKind::Microphone, 0)],
            stopped_by: None,
            failure: Some(Failure::PermissionDenied {
                source: TrackKind::Microphone,
                node: String::new(),
                reason: String::new(),
                remedy: String::new(),
            }),
            actions: Vec::new(),
        };
        assert_eq!(
            meeting.actions(),
            vec![Action::GrantAccess, Action::ChooseSource, Action::Discard]
        );
        meeting.tracks[0].committed_bytes = 2;
        assert_eq!(
            meeting.actions(),
            vec![
                Action::GrantAccess,
                Action::ChooseSource,
                Action::Recover,
                Action::Discard
            ]
        );
        meeting.state = MeetingState::Recovered;
        assert_eq!(
            meeting.actions(),
            vec![Action::GrantAccess, Action::ChooseSource]
        );
        meeting.failure = None;
        assert_eq!(meeting.actions(), Vec::new());
    }
}
