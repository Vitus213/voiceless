//! The durable layout of the queue under `<data dir>/jobs/`:
//!
//! ```text
//! jobs/
//!   queue.lock          advisory lock: one process serves the queue
//!   incoming/           uploads on their way in (`JobQueue::incoming_dir`)
//!   <job id>/
//!     job.json          the job's record, replaced atomically on every change
//!     segments.jsonl    one committed segment per line, appended and synced
//!     media             the queue's own copy of the input
//!     media.partial     a copy still being made, or an uploaded reselection
//! ```
//!
//! A segment is committed when its line is synced to `segments.jsonl`; the
//! journal is the only place results live, so a record can never claim a
//! result that is not on disk. The record holds everything else: the
//! user's intent (status, stage), the input and the resume generation.
//! Records are written to a temporary file, synced and renamed over the
//! old one, so a reader sees either the previous or the new record.
//!
//! Everything under `jobs/` is private to the user (0700 directories,
//! 0600 files): media and transcripts are personal data.
//!
//! After a crash, `job.json` is always whole. What a crash can leave
//! behind is removed when the queue is opened: a `media.partial` that is
//! not a staged reselection, the temporary files of interrupted record
//! writes, and job directories that have no record yet.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::job::{JobFailure, RecognitionRequest, Segment, Stage, Status};
use super::media::MediaDecoder;

/// Directory of the queue inside the Voiceless data directory.
pub(crate) const JOBS_DIR: &str = "jobs";
/// Directory uploads are received in.
pub(crate) const INCOMING: &str = "incoming";

pub(crate) const RECORD_VERSION: u32 = 1;
const RECORD: &str = "job.json";
const JOURNAL: &str = "segments.jsonl";
const MEDIA: &str = "media";
const PARTIAL: &str = "media.partial";
const LOCK: &str = "queue.lock";
const TEMPORARY_PREFIX: &str = ".job-";

const BUSY: &str =
    "another Voiceless runtime already processes the transcription queue of this data directory";

/// What a queue keeps about one job. Serialized as `job.json`; every
/// change is made durable before it is visible.
///
/// `generation` numbers the job's lifecycle commands. A worker's lease
/// carries the generation it was claimed under, and every result it hands
/// back is refused once the generation moved on, so a unit that was still
/// running when the user paused, cancelled, reselected or removed the job
/// can neither commit a segment nor change the state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JobRecord {
    pub(crate) version: u32,
    pub id: String,
    /// Submission order; jobs are claimed lowest first.
    pub sequence: u64,
    /// Submission time, RFC 3339 UTC.
    pub created_at: String,
    /// File name the job is listed under.
    pub name: String,
    /// Where the user selected the file; `None` for an upload.
    pub source_path: Option<PathBuf>,
    /// Size of the owned media, once the job owns it.
    pub bytes: Option<u64>,
    /// SHA-256 of the media. `None` until the job has owned media; from
    /// then on the digest a reselected file must match.
    pub sha256: Option<String>,
    pub decoder: Option<MediaDecoder>,
    pub duration_samples: Option<u64>,
    pub request: RecognitionRequest,
    pub status: Status,
    pub stage: Stage,
    /// The owned media still has to be made from the source: a job that
    /// was never imported, or one whose file was reselected.
    pub needs_source: bool,
    /// `media.partial` is an uploaded reselection, whole and waiting to be
    /// verified; it survives a restart, unlike a copy in progress.
    pub staged_upload: bool,
    /// Recognition runs started for the job.
    pub attempts: u32,
    /// Incremented by every lifecycle command; see the type's documentation.
    pub generation: u64,
    pub error: Option<JobFailure>,
    /// Completion time, RFC 3339 UTC.
    pub completed_at: Option<String>,
}

/// Why a job directory holds no usable record.
#[derive(Debug)]
pub(crate) enum RecordError {
    /// No `job.json`: the creation of the job was interrupted.
    Missing,
    /// The record cannot be parsed, or was written in a layout this build
    /// does not know. The directory is left alone.
    Unreadable(String),
}

/// Create `path` readable by its owner only. `Ok` when it already exists.
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        // The owner of an existing directory set its mode before.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Create or truncate `path` readable and writable by its owner only.
pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Make the directory entries created or renamed under `path` durable.
#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Where a directory cannot be opened for syncing, the rename is left to
/// the operating system.
#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Take the queue's advisory lock. The file stays in place so every
/// process locks the same inode; the returned handle owns the lock.
pub(crate) fn lock_queue(root: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(root.join(LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(io::ErrorKind::WouldBlock, BUSY)),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Remove what earlier processes left in the upload directory: an upload
/// belongs to the request that received it, and no request outlives a
/// process. Returns the number of entries removed.
pub(crate) fn clear_incoming(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root.join(INCOMING)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let gone = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed += usize::from(gone.is_ok());
    }
    removed
}

/// One job's directory.
#[derive(Debug, Clone)]
pub(crate) struct JobDir {
    path: PathBuf,
}

impl JobDir {
    pub fn new(root: &Path, id: &str) -> Self {
        Self {
            path: root.join(id),
        }
    }

    pub fn media(&self) -> PathBuf {
        self.path.join(MEDIA)
    }

    pub fn partial(&self) -> PathBuf {
        self.path.join(PARTIAL)
    }

    pub fn journal(&self) -> PathBuf {
        self.path.join(JOURNAL)
    }

    pub fn create(&self) -> io::Result<()> {
        private_dir(&self.path)
    }

    /// Replace the record: written to a temporary file in the job
    /// directory, synced, renamed over the old record, and the rename
    /// synced too. Nothing changes on a failure.
    pub fn write(&self, record: &JobRecord) -> io::Result<()> {
        let body = serde_json::to_vec_pretty(record).map_err(json_error)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(TEMPORARY_PREFIX)
            .tempfile_in(&self.path)?;
        temporary.write_all(&body)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.path.join(RECORD))
            .map_err(|error| error.error)?;
        sync_directory(&self.path)
    }

    pub fn read(&self) -> Result<JobRecord, RecordError> {
        let body = match fs::read(self.path.join(RECORD)) {
            Ok(body) => body,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RecordError::Missing)
            }
            Err(error) => return Err(RecordError::Unreadable(error.to_string())),
        };
        let record: JobRecord = serde_json::from_slice(&body)
            .map_err(|error| RecordError::Unreadable(error.to_string()))?;
        if record.version != RECORD_VERSION {
            return Err(RecordError::Unreadable(format!(
                "record version {} is not {RECORD_VERSION}",
                record.version
            )));
        }
        Ok(record)
    }

    /// Remove the leftovers of an interrupted write or import: every file
    /// that is not the record, the journal or the media. `media.partial`
    /// is kept when `keep_partial` says it is a staged reselection.
    /// Returns how many files were removed; what cannot be removed is
    /// left for the next start.
    pub fn sweep(&self, keep_partial: bool) -> usize {
        let Ok(entries) = fs::read_dir(&self.path) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let kept = name == RECORD
                || name == JOURNAL
                || name == MEDIA
                || (name == PARTIAL && keep_partial);
            if !kept && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Delete the job directory with everything in it. `Ok` when it is
    /// already gone.
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

/// The committed segments of one job: an append-only journal, one JSON
/// segment per line, synced after every line.
pub(crate) struct Journal {
    file: File,
    /// Bytes of the file that hold whole lines.
    durable_bytes: u64,
}

impl Journal {
    /// Open the journal of `path` for appending and return the segments
    /// committed so far. A crash can leave the last line half written:
    /// such a line is never a committed segment, and is cut off.
    pub fn open(path: &Path) -> io::Result<(Self, Vec<Segment>)> {
        let existing = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut segments = Vec::new();
        let mut offset = 0;
        while offset < existing.len() {
            let end = existing[offset..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(existing.len(), |at| offset + at + 1);
            let complete = existing[end - 1] == b'\n';
            let line = &existing[offset..if complete { end - 1 } else { end }];
            let segment = serde_json::from_slice::<Segment>(line)
                .ok()
                .filter(|segment| segment.end > segment.start);
            match (segment, complete) {
                (Some(segment), true) => segments.push(segment),
                // A whole line that is not a segment is not a torn tail but damage.
                (None, true) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{}: line {} is not a committed segment",
                            path.display(),
                            segments.len() + 1
                        ),
                    ));
                }
                // Written but not synced whole: the writer never reported it as committed.
                (_, false) => {
                    let file = OpenOptions::new().write(true).open(path)?;
                    file.set_len(offset as u64)?;
                    break;
                }
            }
            offset = end;
        }
        let durable_bytes = offset as u64;
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        Ok((
            Self {
                file,
                durable_bytes,
            },
            segments,
        ))
    }

    /// Append one segment and sync it. On a failure the journal is cut
    /// back to its last whole line, so the next line cannot land behind a
    /// torn one.
    pub fn append(&mut self, segment: &Segment) -> io::Result<()> {
        let mut line = serde_json::to_vec(segment).map_err(json_error)?;
        line.push(b'\n');
        if let Err(error) = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_all())
        {
            return match self.file.set_len(self.durable_bytes) {
                Ok(()) => Err(error),
                Err(repair) => Err(io::Error::new(
                    error.kind(),
                    format!("{error}; the results journal could not be repaired: {repair}"),
                )),
            };
        }
        self.durable_bytes += line.len() as u64;
        Ok(())
    }
}
