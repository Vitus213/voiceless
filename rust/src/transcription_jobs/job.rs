//! Job data: the state names, the committed result and the failure
//! vocabulary that the queue, its workers and the console share.
//!
//! Positions and durations count samples of [`SAMPLE_RATE`] in the decoded
//! 16 kHz mono stream, never bytes or the length of the input file, so
//! every time the queue reports is media time.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::media::MediaDecoder;

/// Sample rate of every decoded stream.
pub const SAMPLE_RATE: u32 = 16_000;

/// Media time of `samples`, in seconds.
pub(crate) fn seconds(samples: u64) -> f64 {
    samples as f64 / f64::from(SAMPLE_RATE)
}

/// Where a job is in its life, spelled as the console shows it.
///
/// ```text
/// submit ──────────────────────────────► queued ──worker──► running ──► completed
///   │                                     │  ▲                  │
///   │ unreadable or unauthorized file     │  │ resume            │ error, or
///   ▼                                     ▼  │                   ▼ pause / cancel
/// failed ◄──── import failed ──────── paused  │                 paused / cancelled / failed
///   │                                          │
///   └── retry or reselect ────────────────────┴──► queued
/// ```
///
/// Only `queued` jobs are claimed. A job leaves `running` through its
/// worker (end of the media, or an error) or through `pause` and `cancel`,
/// which take effect at the next unit boundary. `remove` deletes a job
/// from any status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Admitted and waiting for a worker.
    Queued,
    /// A worker holds the job.
    Running,
    /// Stopped by the user at a safe boundary; `resume` continues it.
    Paused,
    /// Dropped by the user: nothing more is recognized or published for it.
    Cancelled,
    /// The whole media was recognized.
    Completed,
    /// Stopped by an error; `error` says which and what to do about it.
    Failed,
}

impl Status {
    /// Lower-case wire name (`queued`, `running`, ...).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which worker a job waits for or runs on. Import comes first: a job
/// owns its media before anything is recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Copying, verifying and measuring the media.
    Import,
    /// Recognizing it segment by segment.
    Transcribe,
}

/// What the worker holding a `running` job does right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activity {
    /// Copying the selected file into the queue's store.
    Copying,
    /// Decoding the whole media once to measure it.
    Probing,
    /// Recognizing one bounded unit.
    Recognizing,
    /// Holding at a unit boundary until live dictation releases the shared
    /// CPU/GPU resource.
    WaitingForResource,
}

/// Where the media is recognized, and therefore whether it leaves the
/// machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// On this device with a local model. Media is never uploaded.
    Local,
    /// By the configured cloud service: every segment is sent to it.
    Cloud,
}

/// One speech segment that is committed, with the media time it occupies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// First sample of the segment.
    pub start: u64,
    /// One past the last sample; always greater than `start`.
    pub end: u64,
    /// Recognized text, trimmed and not empty.
    pub text: String,
}

/// Why a job stopped. Each code has the remedy every failure of that code
/// shares; the message names what happened to this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureCode {
    /// The selected file cannot be read.
    Unreadable,
    /// No decoder here can read this media.
    Unsupported,
    /// The container holds no audio track.
    NoAudio,
    /// The media is damaged, truncated or not what it claims to be.
    Corrupt,
    /// The data directory cannot hold the media.
    NoSpace,
    /// Cloud transport was selected without the user's permission to send
    /// the media. Nothing was uploaded.
    CloudNotAuthorized,
    /// The cloud service rejected or was given no credential.
    CloudCredentials,
    /// The cloud service could not be reached or did not answer a segment.
    CloudTransport,
    /// The local model or the voice activity detector cannot be loaded.
    Model,
    /// A segment could not be recognized.
    Recognize,
    /// The queue's copy of the media is gone or unreadable.
    SourceUnavailable,
    /// A reselected file is not the media that was imported.
    SourceMismatch,
    /// The committed results of the job cannot be read.
    JournalCorrupt,
    /// Any other failure, such as an I/O error of the store.
    Internal,
}

/// The error state of a `failed` job: what happened and what to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFailure {
    pub code: FailureCode,
    pub message: String,
    pub remedy: Option<String>,
}

impl JobFailure {
    /// The same failure with `remedy` in place of the code's default one.
    pub(crate) fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

impl FailureCode {
    /// A failure of this code with `message` and the remedy every failure
    /// of the code shares.
    pub(crate) fn failure(self, message: impl Into<String>) -> JobFailure {
        JobFailure {
            code: self,
            message: message.into(),
            remedy: self.remedy().map(str::to_owned),
        }
    }

    fn remedy(self) -> Option<&'static str> {
        Some(match self {
            Self::Unreadable => {
                "Check that the file exists and that this user can read it, then reselect it."
            }
            Self::Unsupported => {
                "Convert the file to an uncompressed WAV, or install FFmpeg on PATH to decode other audio and video formats."
            }
            Self::NoAudio => "Choose a file that contains an audio track.",
            Self::Corrupt => {
                "The file is damaged. Export or download it again, then reselect it."
            }
            Self::NoSpace => "Free space in the Voiceless data directory, then retry.",
            Self::CloudNotAuthorized => {
                "Allow sending media to the configured cloud service when importing, or choose local transcription. Nothing was uploaded."
            }
            Self::CloudCredentials => {
                "Set a valid `[cloud] api_key`, or choose local transcription."
            }
            Self::CloudTransport => {
                "Check the network connection and the cloud service, then retry. Finished segments are kept."
            }
            Self::Model => {
                "Install or select a local model that can run on this device, then retry."
            }
            Self::Recognize => "Retry the job. Finished segments are kept.",
            Self::SourceUnavailable => {
                "Reselect the original file. Its content is verified before the job continues."
            }
            Self::SourceMismatch => {
                "Choose the file that was imported: the reselected content differs from it."
            }
            Self::JournalCorrupt => "Remove the job and import the file again.",
            Self::Internal => {
                "Retry the job. If this persists, check the permissions and free space of the Voiceless data directory."
            }
        })
    }
}

impl fmt::Display for JobFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(remedy) = &self.remedy {
            write!(f, " {remedy}")?;
        }
        Ok(())
    }
}

/// How a job's media is recognized. Fixed when the job is submitted and
/// kept with it, so a retry or a resume after a restart recognizes with
/// what the user chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecognitionRequest {
    /// Catalog id of the local model (`qwen3-asr-1.7b-gguf`, ...), or the
    /// cloud service's model name for `Transport::Cloud`.
    pub model: String,
    pub transport: Transport,
    /// Explicit SenseVoice language, or automatic recognition when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Whether the user allowed this job's media to be sent to the cloud
    /// service. A cloud job without it fails before anything is decoded.
    pub cloud_authorized: bool,
}

/// The input file as the user sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InputSummary {
    /// File name the job is listed under.
    pub name: String,
    /// Where the file was selected, if it was selected on this machine.
    pub path: Option<String>,
    /// Size of the queue's own copy, once the job owns one.
    pub bytes: Option<u64>,
    /// SHA-256 of the media, once the job owns it.
    pub sha256: Option<String>,
}

/// What the queue measured about the media.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MediaSummary {
    /// What decodes the file, once it has been probed.
    pub decoder: Option<MediaDecoder>,
    /// Decoded length at 16 kHz.
    pub duration_seconds: Option<f64>,
}

/// How far a job got.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Progress {
    /// Media time the worker has fed to recognition.
    pub position_seconds: f64,
    /// End of the last committed segment: where a resumed run continues.
    pub committed_seconds: f64,
    /// `position` over the measured duration, once it is known.
    pub fraction: Option<f64>,
    /// Committed segments.
    pub segments: usize,
}

/// A job without its segments, as `JobQueue::list` returns them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobSummary {
    pub id: String,
    /// Submission order; jobs are claimed lowest first.
    pub sequence: u64,
    /// Submission time, RFC 3339 UTC.
    pub created_at: String,
    pub status: Status,
    pub stage: Stage,
    pub activity: Option<Activity>,
    pub input: InputSummary,
    pub media: MediaSummary,
    pub model: String,
    /// Requested language, or `auto` when the model selects it.
    pub language: String,
    pub transport: Transport,
    /// Recognition runs started for the job, retries and resumes included.
    pub attempts: u32,
    pub progress: Progress,
    pub error: Option<JobFailure>,
    /// Completion time, RFC 3339 UTC.
    pub completed_at: Option<String>,
}

/// A job with its committed segments, in media order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobView {
    #[serde(flatten)]
    pub summary: JobSummary,
    pub segments: Vec<Segment>,
}

/// What `JobQueue::open` found left behind by an earlier process, and did
/// about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Recovery {
    /// Jobs a previous process left `running`; they are `queued` again and
    /// continue from their last committed segment.
    pub resumed: Vec<String>,
    /// Job directories whose record cannot be read. They are left alone.
    pub unreadable: Vec<String>,
    /// Interrupted copies and record writes that were removed.
    pub removed_files: usize,
    /// Directories of jobs whose creation was interrupted, removed.
    pub removed_directories: usize,
}
