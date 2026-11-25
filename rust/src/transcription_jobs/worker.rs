//! The queue's workers: one step of a job at a time.
//!
//! [`import`] makes a job own its media and measures it; [`transcribe`]
//! recognizes it, one bounded unit at a time. The queue's own threads
//! (`import_loop`, `transcribe_loop`) claim jobs and call them, and so can
//! threads of the daemon's own, through the worker-facing methods of
//! [`JobQueue`](super::queue::JobQueue).
//!
//! A step ends in one of three ways: the job moved on (`imported`,
//! `complete`), it failed (`fail`), or the run was superseded because the
//! user paused, cancelled, reselected or removed the job, or the queue is
//! stopping. A superseded run leaves nothing behind: its results are not
//! committed, and what it was still writing is removed.
//!
//! # Units and the shared resource
//!
//! Transcription reads one second of media at a time. Before each second
//! the worker takes the shared resource for that unit: the second through
//! the segmenter, plus the decode of the segments it closed, and nothing
//! else. So live dictation that needs the same decoder never waits for
//! more than one segment decode, and a job waiting for it says so
//! ([`Activity::WaitingForResource`]).
//!
//! # Resuming
//!
//! A run starts at the end of the last committed segment. Everything after
//! that point is read again, so nothing is skipped and nothing is
//! recognized twice: what was committed stays, what was not is redone.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::job::{Activity, FailureCode, JobFailure, Stage, SAMPLE_RATE};
use super::media::{self, Media};
use super::queue::{Claim, JobQueue, QueueCore};
use super::recognize::Recognition;
use super::resource::SharedResource;
use super::store;

/// Samples of one unit: one second of media.
const UNIT_SAMPLES: usize = SAMPLE_RATE as usize;
/// How long an idle worker waits before it looks for work again.
const IDLE: Duration = Duration::from_secs(1);
/// Bytes copied between two checks of whether the copy is still wanted.
const COPY_BLOCK_BYTES: usize = 1024 * 1024;

/// Why a step ended before its job moved on.
enum Stopped {
    /// The run was superseded: the job was paused, cancelled, reselected
    /// or removed, or the queue is stopping. Nothing is reported.
    Superseded,
    /// The job failed.
    Failure(JobFailure),
}

type Step<T> = Result<T, Stopped>;

/// Run one import: the job comes to own its media, which is then measured
/// and queued for transcription. Blocking, for as long as the copy and
/// the probe take.
pub fn import(queue: &JobQueue, claim: Claim) {
    run_import(&queue.core, claim)
}

/// Run one transcription from the job's last committed segment to the end
/// of its media, or until the run is superseded. Blocking; the shared
/// `resource` is taken per unit, for the job's transport.
pub fn transcribe(
    queue: &JobQueue,
    claim: Claim,
    recognition: &dyn Recognition,
    resource: &dyn SharedResource,
) {
    run_transcription(&queue.core, claim, recognition, resource)
}

/// The queue's import thread: claim, import, wait, until the queue stops.
pub(crate) fn import_loop(queue: Arc<QueueCore>) {
    while !queue.stopped() {
        match queue.claim(Stage::Import) {
            Some(claim) => run_import(&queue, claim),
            None => queue.wait_for_work(Some(Stage::Import), IDLE),
        }
    }
}

/// The queue's transcription thread: claim, transcribe, wait, until the
/// queue stops.
pub(crate) fn transcribe_loop(
    queue: Arc<QueueCore>,
    recognition: Arc<dyn Recognition>,
    resource: Arc<dyn SharedResource>,
) {
    while !queue.stopped() {
        match queue.claim(Stage::Transcribe) {
            Some(claim) => {
                run_transcription(&queue, claim, recognition.as_ref(), resource.as_ref())
            }
            None => queue.wait_for_work(Some(Stage::Transcribe), IDLE),
        }
    }
}

fn run_import(queue: &QueueCore, claim: Claim) {
    match import_media(queue, &claim) {
        Ok(media) => {
            queue.imported(&claim.lease, media.decoder, media.duration_samples);
        }
        Err(Stopped::Superseded) => {}
        Err(Stopped::Failure(failure)) => {
            queue.fail(&claim.lease, failure);
        }
    }
}

/// Make the job own its media and measure it.
fn import_media(queue: &QueueCore, claim: &Claim) -> Step<Media> {
    let lease = &claim.lease;
    let aborted = || queue.stopped() || !queue.is_current(lease);
    let snapshot = &claim.snapshot;
    if aborted() {
        return Err(Stopped::Superseded);
    }
    if snapshot.needs_source {
        queue.set_activity(lease, Some(Activity::Copying));
        let (bytes, digest) = if snapshot.staged_upload {
            // A crash after the rename can leave the staged file already published.
            let pending = snapshot.partial.is_file();
            let path = if pending {
                &snapshot.partial
            } else {
                &snapshot.media
            };
            let sha256 = hash(path, &aborted)?;
            verify(&sha256, snapshot.sha256.as_deref())?;
            if aborted() {
                return Err(Stopped::Superseded);
            }
            let bytes = file_len(path)?;
            if pending && !queue.publish(lease, path, &snapshot.media).map_err(disk)? {
                return Err(Stopped::Superseded);
            }
            (bytes, sha256)
        } else {
            let source = snapshot.source_path.as_deref().ok_or_else(|| {
                Stopped::Failure(
                    FailureCode::SourceUnavailable.failure("reselect the original media file"),
                )
            })?;
            let directory = snapshot
                .media
                .parent()
                .expect("media lives in a job directory");
            let partial = tempfile::NamedTempFile::new_in(directory).map_err(disk)?;
            let copied = copy(source, partial.path(), &aborted)?;
            verify(&copied.sha256, snapshot.sha256.as_deref())?;
            if !queue
                .publish(lease, partial.path(), &snapshot.media)
                .map_err(disk)?
            {
                return Err(Stopped::Superseded);
            }
            (copied.bytes, copied.sha256)
        };
        if !queue.media_ready(lease, bytes, digest) {
            return Err(Stopped::Superseded);
        }
    }
    if !snapshot.media.is_file() {
        return Err(Stopped::Failure(FailureCode::SourceUnavailable.failure(
            format!(
                "the queue's copy of the media is missing: {}",
                snapshot.media.display()
            ),
        )));
    }
    // An upload is owned from the start; its digest is taken once, here.
    if !snapshot.needs_source && snapshot.sha256.is_none() {
        let sha256 = hash(&snapshot.media, &aborted)?;
        if !queue.media_ready(lease, file_len(&snapshot.media)?, sha256) {
            return Err(Stopped::Superseded);
        }
    }
    match snapshot.decoder {
        Some(decoder) => Ok(Media {
            decoder,
            duration_samples: snapshot.duration_samples.unwrap_or(0),
        }),
        None => {
            queue.set_activity(lease, Some(Activity::Probing));
            media::probe(&snapshot.media, queue.ffmpeg())
                .map_err(|error| Stopped::Failure(media::failure(error, &snapshot.media)))
        }
    }
}

fn run_transcription(
    queue: &QueueCore,
    claim: Claim,
    recognition: &dyn Recognition,
    resource: &dyn SharedResource,
) {
    let lease = &claim.lease;
    let aborted = || queue.stopped() || !queue.is_current(lease);
    let snapshot = &claim.snapshot;
    if !snapshot.media.is_file() {
        queue.fail(
            lease,
            FailureCode::SourceUnavailable
                .failure("the imported media is missing; reselect the original file"),
        );
        return;
    }
    let Some(decoder) = snapshot.decoder else {
        queue.fail(
            lease,
            FailureCode::Internal
                .failure("the job was queued for transcription before its media was measured"),
        );
        return;
    };
    let mut source = match media::open_stream(
        &snapshot.media,
        decoder,
        snapshot.committed_until,
        queue.ffmpeg(),
    ) {
        Ok(source) => source,
        Err(error) => {
            queue.fail(lease, media::failure(error, &snapshot.media));
            return;
        }
    };
    queue.set_activity(lease, Some(Activity::WaitingForResource));
    let Some(load) = resource.unit_for(snapshot.request.transport, &aborted) else {
        return;
    };
    queue.set_activity(lease, Some(Activity::Recognizing));
    let mut recognizer = match recognition.open(&snapshot.request, snapshot.committed_until) {
        Ok(recognizer) => recognizer,
        Err(failure) => {
            queue.fail(lease, failure);
            return;
        }
    };
    drop(load);
    let mut buffer = vec![0.0_f32; UNIT_SAMPLES];
    let mut position = snapshot.committed_until;
    loop {
        if aborted() {
            return;
        }
        let read = match source.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                queue.fail(lease, media::failure(error, &snapshot.media));
                return;
            }
        };
        if read == 0 {
            break;
        }
        queue.set_activity(lease, Some(Activity::WaitingForResource));
        let Some(_unit) = resource.unit_for(snapshot.request.transport, &aborted) else {
            return;
        };
        queue.set_activity(lease, Some(Activity::Recognizing));
        let segments = match recognizer.push(&buffer[..read]) {
            Ok(segments) => segments,
            Err(failure) => {
                queue.fail(lease, failure);
                return;
            }
        };
        drop(_unit);
        position += read as u64;
        if !queue.commit(lease, &segments, position) {
            return;
        }
    }
    // The end of the media closes what is still open: one more unit.
    if aborted() {
        return;
    }
    queue.set_activity(lease, Some(Activity::WaitingForResource));
    let Some(_unit) = resource.unit_for(snapshot.request.transport, &aborted) else {
        return;
    };
    queue.set_activity(lease, Some(Activity::Recognizing));
    let segments = match recognizer.finish() {
        Ok(segments) => segments,
        Err(failure) => {
            queue.fail(lease, failure);
            return;
        }
    };
    drop(_unit);
    if !queue.commit(lease, &segments, position) {
        return;
    }
    queue.complete(lease);
}

/// SHA-256 of `path`, read in blocks while `aborted` stays false.
fn hash(path: &Path, aborted: &dyn Fn() -> bool) -> Step<String> {
    let unreadable = |error: io::Error| Stopped::Failure(media::failure(error.into(), path));
    let mut file = File::open(path).map_err(unreadable)?;
    let mut hasher = Sha256::new();
    let mut block = vec![0_u8; COPY_BLOCK_BYTES];
    loop {
        if aborted() {
            return Err(Stopped::Superseded);
        }
        let read = file.read(&mut block).map_err(unreadable)?;
        if read == 0 {
            break;
        }
        hasher.update(&block[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

struct Copied {
    bytes: u64,
    sha256: String,
}

/// Copy `source` to `partial`, hashing it on the way.
fn copy(source: &Path, partial: &Path, aborted: &dyn Fn() -> bool) -> Step<Copied> {
    let unreadable = |error: io::Error| {
        Stopped::Failure(
            FailureCode::Unreadable
                .failure(format!("{} cannot be read: {error}", source.display())),
        )
    };
    let mut input = File::open(source).map_err(unreadable)?;
    let expected = input.metadata().map_err(unreadable)?.len();
    // Checked first: a copy that cannot fit would fill the disk to its
    // last byte before it failed.
    let directory = partial.parent().expect("media lives in a job directory");
    if let Ok(available) = crate::models::available_space(directory) {
        if available < expected {
            return Err(Stopped::Failure(FailureCode::NoSpace.failure(format!(
                "{} needs {expected} bytes and the data directory has {available} left",
                source.display()
            ))));
        }
    }
    let mut output = store::private_file(partial).map_err(disk)?;
    let mut hasher = Sha256::new();
    let mut block = vec![0_u8; COPY_BLOCK_BYTES];
    let mut bytes = 0_u64;
    loop {
        if aborted() {
            return Err(Stopped::Superseded);
        }
        let read = match input.read(&mut block) {
            Ok(0) => break,
            Ok(read) => read,
            // A signal interrupted the call; the read is simply retried.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(unreadable(error)),
        };
        hasher.update(&block[..read]);
        output.write_all(&block[..read]).map_err(disk)?;
        bytes += read as u64;
    }
    output.sync_all().map_err(disk)?;
    Ok(Copied {
        bytes,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// Check that `sha256` is the digest the job's media must have.
fn verify(sha256: &str, expected: Option<&str>) -> Step<()> {
    match expected {
        Some(expected) if sha256 != expected => Err(Stopped::Failure(
            FailureCode::SourceMismatch.failure(format!(
                "the reselected file is not the media that was imported: its SHA-256 is {sha256}, the imported media has {expected}"
            )),
        )),
        _ => Ok(()),
    }
}

fn file_len(path: &Path) -> Step<u64> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| Stopped::Failure(media::failure(error.into(), path)))
}

/// A failure of the data directory while media is stored.
fn disk(error: io::Error) -> Stopped {
    if out_of_space(&error) {
        Stopped::Failure(
            FailureCode::NoSpace.failure(format!("the data directory is full: {error}")),
        )
    } else {
        Stopped::Failure(FailureCode::Internal.failure(format!("cannot store the media: {error}")))
    }
}

#[cfg(unix)]
fn out_of_space(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSPC) | Some(libc::EDQUOT)
    )
}

#[cfg(not(unix))]
fn out_of_space(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    )
}
