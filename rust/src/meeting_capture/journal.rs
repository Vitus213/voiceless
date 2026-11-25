//! What a meeting leaves on disk and how it is read back. Everything lives
//! under `<data directory>/meetings/<id>/`, a directory Voiceless creates
//! for one meeting and is the only writer of: the journal `meeting.json`,
//! the tracks `microphone.wav` and `system.wav`, and, while the meeting is
//! unfinished, `microphone.wav.partial`, `system.wav.partial` and the
//! recorders' output `microphone.log`, `system.log`. A track is 16 kHz mono
//! PCM16 with the 44-byte header `capture::wav_header` writes.
//!
//! The journal is the record of truth. Every change replaces the whole file
//! through a synced temporary and a rename, so it is either the old or the
//! new document. A track's `committed_bytes` is the audio that is on disk
//! for good: a recorder syncs the file before it journals a larger range,
//! never after. Whatever cuts a runtime short, at most the audio written
//! since the last commit is lost.
//!
//! An unfinished track is finalized (trimmed to what the journal keeps,
//! given its real header and renamed from `.partial`) only when a meeting
//! ends as asked or the user recovers an interrupted one. A meeting's
//! `.wav` is therefore always a complete file.
//!
//! Failure behaviour: a journal that cannot be parsed, was written by
//! another version of the format, names another id or holds an inconsistent
//! track is an error for its directory. The caller reports it and leaves
//! the files alone.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::model::{Failure, MeetingState, StoppedBy, TrackKind, TrackStatus, SAMPLE_RATE};

/// Version of the journal format this build writes and reads.
const VERSION: u32 = 1;
/// Name of the journal in a meeting directory.
const FILE: &str = "meeting.json";
/// Length of the WAV header `capture::wav_header` writes.
pub(super) const HEADER_BYTES: usize = 44;

/// The journal: what a runtime needs to know about a meeting when it finds
/// the directory later. Unknown fields are refused, as with the config; a
/// journal of a newer build has a newer `version` and is not reinterpreted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalDoc {
    pub version: u32,
    pub id: String,
    pub state: MeetingState,
    /// RFC 3339 UTC, when the recorders were started.
    pub started_at: String,
    /// RFC 3339 UTC; `None` while the meeting records, and for a runtime
    /// that did not get to stop it.
    pub ended_at: Option<String>,
    pub stopped_by: Option<StoppedBy>,
    pub failure: Option<Failure>,
    /// Process that recorded the meeting: whose it is while `state` is
    /// `recording`.
    pub pid: u32,
    /// The microphone first, the system output second.
    pub tracks: Vec<TrackRecord>,
}

/// One track of the journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackRecord {
    pub kind: TrackKind,
    #[serde(default)]
    pub job_id: Option<String>,
    pub node: String,
    pub label: String,
    pub status: TrackStatus,
    /// PCM bytes that are on disk for good (see the module doc).
    pub committed_bytes: u64,
    /// RFC 3339 UTC of the first audio; `None` until there was any.
    pub first_audio_at: Option<String>,
    /// Why the track is `Failed` or has no file.
    pub error: Option<String>,
}

impl JournalDoc {
    /// The journal of a meeting that is recording `tracks` now.
    pub fn new(
        id: &str,
        pid: u32,
        tracks: impl IntoIterator<Item = (TrackKind, String, String)>,
    ) -> Self {
        Self {
            version: VERSION,
            id: id.to_owned(),
            state: MeetingState::Recording,
            started_at: now(),
            ended_at: None,
            stopped_by: None,
            failure: None,
            pid,
            tracks: tracks
                .into_iter()
                .map(|(kind, node, label)| TrackRecord {
                    kind,
                    job_id: None,
                    node,
                    label,
                    status: TrackStatus::Recording,
                    committed_bytes: 0,
                    first_audio_at: None,
                    error: None,
                })
                .collect(),
        }
    }

    /// The record of `kind`. A journal holds every track the meeting was
    /// started with.
    pub fn track_mut(&mut self, kind: TrackKind) -> &mut TrackRecord {
        self.tracks
            .iter_mut()
            .find(|track| track.kind == kind)
            .expect("the journal holds every selected track")
    }

    /// The longest kept track, in PCM bytes.
    pub fn kept_bytes(&self) -> u64 {
        self.tracks
            .iter()
            .map(|track| track.committed_bytes)
            .max()
            .unwrap_or(0)
    }
}

/// A meeting's journal while its recorders write it. Updates are
/// serialized and each replaces the file; a reader only ever clones the
/// document as it was last written and never waits for a write.
pub struct Journal {
    path: PathBuf,
    /// Held while one updated document is written, so that writes cannot
    /// overtake each other.
    writer: Mutex<()>,
    /// The document as last written.
    latest: Mutex<Arc<JournalDoc>>,
}

impl Journal {
    /// Write the first journal of `directory`.
    pub fn create(directory: &Path, doc: JournalDoc) -> io::Result<Self> {
        let path = directory.join(FILE);
        write_atomic(&path, &doc)?;
        Ok(Self {
            path,
            writer: Mutex::new(()),
            latest: Mutex::new(Arc::new(doc)),
        })
    }

    /// The document as last written.
    pub fn get(&self) -> Arc<JournalDoc> {
        Arc::clone(&self.latest.lock())
    }

    /// Change the journal and write it. On a failed write the document is
    /// the old one, in memory as on disk.
    pub fn update(&self, change: impl FnOnce(&mut JournalDoc)) -> io::Result<()> {
        let _write = self.writer.lock();
        let mut doc = JournalDoc::clone(&self.latest.lock());
        change(&mut doc);
        write_atomic(&self.path, &doc)?;
        *self.latest.lock() = Arc::new(doc);
        Ok(())
    }
}

/// The directory of every meeting of a data directory.
pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("meetings")
}

/// The directory of one meeting. Only a valid id names one, so nothing a
/// caller passes can point outside the meetings directory.
pub fn directory(root: &Path, id: &str) -> io::Result<PathBuf> {
    if !valid_id(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a meeting id: {id:?}"),
        ));
    }
    Ok(root.join(id))
}

/// The unfinished file of a track.
pub(super) fn partial_path(directory: &Path, kind: TrackKind) -> PathBuf {
    directory.join(format!("{}.partial", kind.file()))
}

/// The finalized WAV of a track.
pub(super) fn final_path(directory: &Path, kind: TrackKind) -> PathBuf {
    directory.join(kind.file())
}

/// Where the recorder of a track writes its stderr.
pub(super) fn log_path(directory: &Path, kind: TrackKind) -> PathBuf {
    directory.join(format!("{}.log", kind.name()))
}

/// `OpenOptions` for a new file only the user may read: meeting audio is
/// private.
pub(super) fn private() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Make `meetings/<id>/` (and `meetings/`), readable by the user only.
pub fn create(root: &Path, id: &str) -> io::Result<PathBuf> {
    let directory = directory(root, id)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory)?;
    Ok(directory)
}

impl TrackRecord {
    /// Take the time of its first audio from the recorder, once there was
    /// any.
    pub fn note_first_audio(&mut self, first_audio_at: Option<&str>) {
        if self.first_audio_at.is_none() {
            self.first_audio_at = first_audio_at.map(str::to_owned);
        }
    }
}

/// Every meeting directory under `root`, as `(id, path)`. A symbolic link
/// is not followed, and a `root` that does not exist yet has none.
pub fn directories(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut meetings = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let id = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|id| valid_id(id));
        if let (true, Some(id)) = (is_directory, id) {
            meetings.push((id.to_owned(), path));
        }
    }
    Ok(meetings)
}

/// The journal of one meeting directory. `NotFound` when the directory has
/// none; `InvalidData` when it is not one this build understands.
pub fn read(directory: &Path) -> io::Result<JournalDoc> {
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|id| valid_id(id))
        .ok_or_else(|| {
            invalid(format!(
                "{} is not a meeting directory",
                directory.display()
            ))
        })?;
    let bytes = fs::read(directory.join(FILE))?;
    let doc: JournalDoc = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("{FILE} is not a meeting journal: {error}")))?;
    if doc.version != VERSION {
        return Err(invalid(format!(
            "{FILE} is written in version {}; this build reads version {VERSION}",
            doc.version
        )));
    }
    if doc.id != id {
        return Err(invalid(format!(
            "{FILE} names meeting {} in the directory of {id}",
            doc.id
        )));
    }
    // One microphone and at most one system output, each a whole number of
    // samples that fits the 32-bit sizes of a WAV.
    let microphones = doc
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Microphone)
        .count();
    let outputs = doc.tracks.len() - microphones;
    let consistent = microphones == 1
        && outputs <= 1
        && doc.tracks.iter().all(|track| {
            track.committed_bytes.is_multiple_of(2)
                && track.committed_bytes <= u64::from(u32::MAX - 44)
        });
    if !consistent {
        return Err(invalid(format!(
            "the tracks of meeting {id} are inconsistent"
        )));
    }
    Ok(doc)
}

/// Replace a meeting's journal.
pub fn write(directory: &Path, doc: &JournalDoc) -> io::Result<()> {
    write_atomic(&directory.join(FILE), doc)
}

/// Delete a meeting's directory. Only a valid id names one, so nothing
/// outside `root` can be reached; a meeting that is already gone is fine.
pub fn remove(root: &Path, id: &str) -> io::Result<()> {
    match fs::remove_dir_all(directory(root, id)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Turn the journal of a recording whose runtime died into an interrupted
/// one: every unfinished track is cut back to what the journal keeps.
pub fn interrupt(directory: &Path, mut doc: JournalDoc) -> io::Result<JournalDoc> {
    for track in &mut doc.tracks {
        if track.status == TrackStatus::Finalized {
            continue;
        }
        if track.committed_bytes > 0 && final_path(directory, track.kind).is_file() {
            finish_track(directory, track.kind, track.committed_bytes)?;
            track.status = TrackStatus::Finalized;
            continue;
        }
        let kept = trim_track(directory, track.kind, track.committed_bytes)?;
        if track.status == TrackStatus::Recording {
            track.status = if kept > 0 {
                TrackStatus::Unfinished
            } else {
                TrackStatus::Empty
            };
        } else if kept == 0 && track.committed_bytes > 0 {
            // A track that had failed when the runtime died on top of it.
            track.status = TrackStatus::Empty;
            track.error = Some("its recorded audio was lost with the runtime".to_owned());
        }
        track.committed_bytes = kept;
    }
    doc.state = MeetingState::Interrupted;
    doc.failure = Some(Failure::ProcessInterrupted);
    doc.stopped_by = None;
    doc.ended_at = None;
    write(directory, &doc)?;
    Ok(doc)
}

/// Cut an unfinished track down to `committed` bytes. Returns what could be
/// kept: less when the file lost data, `0` when there is no file of ours (a
/// file that is not ours is left for the user to look at).
pub fn trim_track(directory: &Path, kind: TrackKind, committed: u64) -> io::Result<u64> {
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(partial_path(directory, kind))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if !track_header(&mut file, committed) {
        return Ok(0);
    }
    let present = file.metadata()?.len().saturating_sub(HEADER_BYTES as u64) & !1;
    let kept = committed.min(present);
    file.set_len(HEADER_BYTES as u64 + kept)?;
    file.sync_data()?;
    Ok(kept)
}

/// Finalize a track: give its unfinished file the header of a recording of
/// `bytes` PCM bytes and rename it to the WAV. Returns the WAV's path, or
/// `None` for a track without audio, whose unfinished file is removed. A
/// track that was finalized before the runtime died is accepted as it is.
pub fn finish_track(directory: &Path, kind: TrackKind, bytes: u64) -> io::Result<Option<PathBuf>> {
    let partial = partial_path(directory, kind);
    let final_path = final_path(directory, kind);
    if bytes == 0 {
        match fs::remove_file(&partial) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        return Ok(None);
    }
    let mut file = match OpenOptions::new().read(true).write(true).open(&partial) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound && final_path.is_file() => {
            return if fs::metadata(&final_path)?.len() == HEADER_BYTES as u64 + bytes {
                Ok(Some(final_path))
            } else {
                Err(short(&final_path, bytes))
            };
        }
        Err(error) => return Err(error),
    };
    if !track_header(&mut file, bytes) {
        return Err(invalid(format!(
            "{} is not an unfinished meeting track",
            partial.display()
        )));
    }
    if file.metadata()?.len() < HEADER_BYTES as u64 + bytes {
        return Err(short(&partial, bytes));
    }
    file.set_len(HEADER_BYTES as u64 + bytes)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&crate::capture::wav_header(bytes, SAMPLE_RATE))?;
    file.sync_data()?;
    drop(file);
    fs::rename(&partial, &final_path)?;
    let _ = File::open(directory).and_then(|directory| directory.sync_all());
    Ok(Some(final_path))
}

/// Remove what only an unfinished meeting needs: the recorders' output and
/// the journal temporaries a crash may have left.
pub fn remove_scratch(directory: &Path) {
    for kind in [TrackKind::Microphone, TrackKind::System] {
        let _ = fs::remove_file(log_path(directory, kind));
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(".meeting-") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Random UUID v4 in hyphenated form, from the OS CSPRNG: the layout
/// `history` uses for record ids.
pub fn new_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// Whether `id` has the shape `new_id` writes (lowercase hex and dashes),
/// which also makes it a safe file name.
pub fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
        })
}

/// RFC 3339 UTC with nanosecond precision, as `history` writes its times.
pub fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|error| format!("<invalid time: {error}>"))
}

/// A crash between updating the header and renaming the partial file leaves a final header.
fn track_header(file: &mut File, committed: u64) -> bool {
    let mut header = [0_u8; HEADER_BYTES];
    file.read_exact(&mut header).is_ok()
        && (header == crate::capture::wav_header(0, SAMPLE_RATE)
            || header == crate::capture::wav_header(committed, SAMPLE_RATE))
}

/// The error of a track file that holds less than the journal says was kept.
fn short(path: &Path, bytes: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!(
            "{} holds less than the {bytes} PCM bytes the journal keeps",
            path.display()
        ),
    )
}

fn invalid(reason: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// Write the journal through a synced temporary and a rename: the file on
/// disk is always one whole document.
fn write_atomic(path: &Path, doc: &JournalDoc) -> io::Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut text = serde_json::to_vec_pretty(doc)?;
    text.push(b'\n');
    // `tempfile` creates the file with mode 0600: a journal is private.
    let mut file = tempfile::Builder::new()
        .prefix(".meeting-")
        .tempfile_in(directory)?;
    file.write_all(&text)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    // The rename is on disk only once the directory entry is.
    let _ = File::open(directory).and_then(|directory| directory.sync_all());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const OTHER: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    fn microphone(id: &str) -> JournalDoc {
        JournalDoc::new(
            id,
            7,
            [(
                TrackKind::Microphone,
                "alsa_input.usb-Headset.mono-fallback".to_owned(),
                "Lian II Mono".to_owned(),
            )],
        )
    }

    fn size(path: &Path) -> Option<u64> {
        fs::metadata(path).ok().map(|metadata| metadata.len())
    }

    #[test]
    fn only_a_meeting_id_names_a_directory() {
        let id = new_id().unwrap();
        assert!(valid_id(&id), "{id}");
        assert_ne!(id, new_id().unwrap());
        for id in [
            "",
            "../escape",
            "3F2504E0-4f89-41d3-9a0c-0305e82c3301",
            "3f2504e0-4f89-41d3-9a0c-0305e82c330",
            "3f2504e0_4f89_41d3_9a0c_0305e82c3301",
            "3f2504eg-4f89-41d3-9a0c-0305e82c3301",
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301/",
        ] {
            assert!(!valid_id(id), "{id:?}");
            assert_eq!(
                directory(Path::new("/meetings"), id).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn a_journal_is_read_only_as_this_build_wrote_it() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("meetings");
        let directory = create(&root, ID).unwrap();
        assert_eq!(
            directories(&root).unwrap(),
            vec![(ID.to_owned(), directory.clone())]
        );
        assert_eq!(
            read(&directory).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let doc = microphone(ID);
        Journal::create(&directory, doc.clone()).unwrap();
        assert_eq!(read(&directory).unwrap(), doc);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            // Meeting audio and its journal are private.
            assert_eq!(mode(&directory), 0o700);
            assert_eq!(mode(&directory.join(FILE)), 0o600);
            assert_eq!(mode(&root), 0o700);
        }

        // A newer build's journal is not reinterpreted ...
        let text = fs::read_to_string(directory.join(FILE)).unwrap();
        assert!(text.contains("\"version\": 1"));
        fs::write(
            directory.join(FILE),
            text.replace("\"version\": 1", "\"version\": 2"),
        )
        .unwrap();
        assert_eq!(
            read(&directory).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // ... nor one with a field this build does not know ...
        assert!(text.contains("\"pid\": 7"));
        fs::write(
            directory.join(FILE),
            text.replace("\"pid\": 7", "\"pid\": 7, \"extra\": true"),
        )
        .unwrap();
        assert_eq!(
            read(&directory).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // ... nor a track whose size is not a whole number of samples ...
        let mut odd = doc.clone();
        odd.tracks[0].committed_bytes = 3;
        write(&directory, &odd).unwrap();
        assert_eq!(
            read(&directory).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // ... nor a second microphone ...
        let mut twice = doc.clone();
        twice.tracks.push(doc.tracks[0].clone());
        write(&directory, &twice).unwrap();
        assert_eq!(
            read(&directory).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // ... nor a journal kept under another meeting's id.
        let other = create(&root, OTHER).unwrap();
        fs::copy(directory.join(FILE), other.join(FILE)).unwrap();
        assert_eq!(read(&other).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            read(scratch.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        write(&directory, &doc).unwrap();
        assert_eq!(read(&directory).unwrap(), doc);
        assert_eq!(directories(&root).unwrap().len(), 2);
        remove(&root, OTHER).unwrap();
        remove(&root, OTHER).unwrap();
        assert_eq!(
            directories(&root).unwrap(),
            vec![(ID.to_owned(), directory)]
        );
        assert_eq!(
            remove(&root, "../escape").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!scratch.path().join("escape").exists());
    }

    #[test]
    fn a_live_journal_keeps_its_last_written_document() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("meetings");
        let directory = create(&root, ID).unwrap();
        let journal = Journal::create(&directory, microphone(ID)).unwrap();
        journal
            .update(|doc| doc.track_mut(TrackKind::Microphone).committed_bytes = 8)
            .unwrap();
        assert_eq!(journal.get().tracks[0].committed_bytes, 8);
        assert_eq!(read(&directory).unwrap().tracks[0].committed_bytes, 8);

        // A write that cannot happen leaves the document as it was.
        fs::remove_file(directory.join(FILE)).unwrap();
        fs::remove_dir(&directory).unwrap();
        assert!(journal
            .update(|doc| doc.state = MeetingState::Completed)
            .is_err());
        assert_eq!(journal.get().state, MeetingState::Recording);
        assert_eq!(journal.get().tracks[0].committed_bytes, 8);
    }

    #[test]
    fn an_unfinished_track_is_trimmed_and_finalized_to_what_the_journal_keeps() {
        let scratch = tempfile::tempdir().unwrap();
        let directory = create(&scratch.path().join("meetings"), ID).unwrap();
        let partial = partial_path(&directory, TrackKind::Microphone);
        let mut file = private().open(&partial).unwrap();
        file.write_all(&crate::capture::wav_header(0, SAMPLE_RATE))
            .unwrap();
        file.write_all(&[0_u8; 10]).unwrap();
        drop(file);
        assert_eq!(size(&partial), Some(54));

        // What the runtime did not commit is cut.
        assert_eq!(trim_track(&directory, TrackKind::Microphone, 6).unwrap(), 6);
        assert_eq!(size(&partial), Some(50));
        // Less is there than the journal keeps: only what is there counts.
        assert_eq!(trim_track(&directory, TrackKind::Microphone, 8).unwrap(), 6);

        // A track that never recorded has nothing to trim or to finalize.
        assert_eq!(trim_track(&directory, TrackKind::System, 4).unwrap(), 0);
        assert_eq!(
            finish_track(&directory, TrackKind::System, 0).unwrap(),
            None
        );
        assert!(!partial_path(&directory, TrackKind::System).exists());

        let path = finish_track(&directory, TrackKind::Microphone, 6)
            .unwrap()
            .unwrap();
        assert_eq!(path, final_path(&directory, TrackKind::Microphone));
        assert!(!partial.exists());
        let wav = fs::read(&path).unwrap();
        assert_eq!(wav.len(), 50);
        assert_eq!(&wav[..44], &crate::capture::wav_header(6, SAMPLE_RATE));
        // Finalized once already, it is found and accepted, at its size only.
        assert_eq!(
            finish_track(&directory, TrackKind::Microphone, 6).unwrap(),
            Some(path.clone())
        );
        assert_eq!(
            finish_track(&directory, TrackKind::Microphone, 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        // A file that is not ours is neither trimmed nor finalized.
        let foreign = partial_path(&directory, TrackKind::System);
        fs::write(&foreign, b"not a voiceless track").unwrap();
        assert_eq!(trim_track(&directory, TrackKind::System, 4).unwrap(), 0);
        assert_eq!(
            finish_track(&directory, TrackKind::System, 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(&foreign).unwrap(), b"not a voiceless track");
        // Nor one that holds less than the journal keeps.
        fs::remove_file(&foreign).unwrap();
        let mut file = private().open(&foreign).unwrap();
        file.write_all(&crate::capture::wav_header(0, SAMPLE_RATE))
            .unwrap();
        drop(file);
        assert_eq!(
            finish_track(&directory, TrackKind::System, 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_recording_whose_runtime_died_becomes_interrupted_with_what_was_committed() {
        let scratch = tempfile::tempdir().unwrap();
        let directory = create(&scratch.path().join("meetings"), ID).unwrap();
        let mut doc = JournalDoc::new(
            ID,
            7,
            [
                (TrackKind::Microphone, "mic".to_owned(), "Mic".to_owned()),
                (
                    TrackKind::System,
                    "speakers".to_owned(),
                    "Speakers".to_owned(),
                ),
            ],
        );
        doc.track_mut(TrackKind::Microphone).committed_bytes = 200;
        write(&directory, &doc).unwrap();
        let mut file = private()
            .open(partial_path(&directory, TrackKind::Microphone))
            .unwrap();
        file.write_all(&crate::capture::wav_header(0, SAMPLE_RATE))
            .unwrap();
        file.write_all(&[7_u8; 250]).unwrap();
        drop(file);

        // Interrupting waits for no recorder: it only ever lowers what is kept.
        let doc = interrupt(&directory, doc).unwrap();
        assert_eq!(read(&directory).unwrap(), doc);
        assert_eq!(doc.state, MeetingState::Interrupted);
        assert_eq!(doc.failure, Some(Failure::ProcessInterrupted));
        let microphone = &doc.tracks[0];
        assert_eq!(microphone.status, TrackStatus::Unfinished);
        assert_eq!(microphone.committed_bytes, 200);
        let system = &doc.tracks[1];
        assert_eq!(system.status, TrackStatus::Empty);
        assert_eq!(system.committed_bytes, 0);
        assert_eq!(
            size(&partial_path(&directory, TrackKind::Microphone)),
            Some(244)
        );
    }
}
