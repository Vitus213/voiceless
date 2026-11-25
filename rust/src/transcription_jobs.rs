//! Durable media imports, transcription workers and their shared decoder budget.

mod export;
mod job;
mod media;
mod queue;
mod recognize;
mod resource;
mod store;
pub mod worker;

pub use export::{ExportDocument, ExportFormat};
pub use job::{
    Activity, FailureCode, JobFailure, JobSummary, JobView, RecognitionRequest, Recovery, Segment,
    Stage, Status, Transport,
};
pub use media::{
    media_support, Ffmpeg, MediaDecoder, MediaSupport, QueueOptions, FFMPEG_FORMATS, NATIVE_FORMATS,
};
pub(crate) use media::{PcmSource, WavSource};
pub use queue::{Claim, ImportRequest, ImportSource, JobLease, JobQueue, QueueError, Workers};
pub use recognize::{Recognition, RuntimeRecognition, SegmentRecognizer};
pub use resource::{PriorityHold, PriorityResource, SharedResource};
