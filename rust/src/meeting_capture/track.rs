//! One meeting track: a recorder child whose audio a thread appends to
//! the track's file. The thread owns the child from `spawn` to `wait`: it
//! alone signals or kills it, and it clears the child once it reaped it, so
//! no signal can reach a pid the kernel gave to somebody else.
//!
//! Recording is the dictation capture's (`capture::RawCapture`) with a
//! stream of its own, started by `capture::recorder`: pointed at the node's
//! `object.serial`, never to fall back to another one, 16 kHz mono PCM16
//! raw on stdout. Nothing of it is kept until the graph shows that exact
//! node feeding the stream. `pw-record` on PipeWire before 1.4 ends its
//! output with the lines that say why its stream ended, which are read as
//! that and not kept as audio. The stream is named `voiceless-meeting-<pid>-<kind>`,
//! so the graph never mistakes it for the dictation capture. A recorder
//! that does not end when its source is gone (`gst-launch-1.0`) has its
//! link watched in the graph while the track records: gone for a few
//! seconds, the source is lost, the track ends and keeps the audio it has.
//!
//! The owner (the runtime) only sets flags: `stop` for a graceful stop,
//! `abort` when it goes away. The thread looks at them at least every
//! 100 ms, also when the source delivers nothing. A stop sends SIGINT, the
//! signal with which the recorder drains its buffers, and keeps writing what
//! arrives until the child closes stdout or the stop grace has passed; then
//! the child is killed. A recorder that ends on its own is a failure, read
//! from its exit status and from what it reported.
//!
//! Durability: every `Limits::commit_bytes` the file is synced and the
//! journaled `committed_bytes` raised, in that order. Whatever ends the
//! track, the audio the recorder delivered until then is synced and
//! journaled the same way; when the disk refuses, the file is cut back to
//! the last range the journal keeps.

use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::io::Read;
use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(target_os = "linux")]
use std::process::{ChildStdout, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{Builder, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use crate::capture::recorder::{
    self, Diagnostic, RecorderBackend, RecorderProfile, Target, LINK_WATCH, OUTPUT_TAIL,
};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use super::journal::{self, Journal};
use super::model::{Failure, Limits, TrackKind, TrackStatus, SAMPLE_RATE};

#[cfg(target_os = "linux")]
type AudioInput = ChildStdout;
#[cfg(not(target_os = "linux"))]
use super::graph::AudioInput;

/// PCM bytes read at a time: about a second of a track.
#[cfg(target_os = "linux")]
const READ_BYTES: usize = 32 * 1024;
/// Longest wait for audio before the flags are looked at again, so a
/// source that delivers nothing can still be stopped.
#[cfg(target_os = "linux")]
const TICK: Duration = Duration::from_millis(100);
/// What a torn sample is called, as in `capture::RawCapture::stop`.
const TORN: &str = "capture ended with an incomplete PCM16 sample";

/// What the owner tells a track and what the track publishes while it runs.
pub(super) struct Shared {
    /// The owner asks for a graceful stop.
    pub stop: AtomicBool,
    /// The owner is going away: its recorder is killed and nobody will act
    /// on the outcome.
    pub abort: AtomicBool,
    /// PCM bytes appended to the file so far.
    pub received: AtomicU64,
    /// Loudness of the last audio written (RMS, 0..1), as the bits of an
    /// `f32`.
    pub level: AtomicU32,
    /// When the first audio arrived, once it did.
    pub first_audio_at: OnceLock<String>,
    /// The recorder while it has not been reaped. The thread that reaps it
    /// clears this.
    pub child: Mutex<Option<Child>>,
    pub journal: Arc<Journal>,
    pub notices: UnboundedSender<TrackNotice>,
}

/// What a track's thread tells the owner.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TrackNotice {
    /// The track reached its limit and is draining; the owner stops the
    /// others.
    LimitReached,
    /// The thread is done with the recorder, the file and the journal.
    Ended {
        kind: TrackKind,
        /// The failure that ended the track, journaled on it; `None` for a
        /// track that stopped as asked.
        failure: Option<Failure>,
    },
}

impl Shared {
    fn new(
        child: Option<Child>,
        journal: Arc<Journal>,
        notices: UnboundedSender<TrackNotice>,
    ) -> Self {
        Self {
            stop: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            received: AtomicU64::new(0),
            level: AtomicU32::new(0),
            first_audio_at: OnceLock::new(),
            child: Mutex::new(child),
            journal,
            notices,
        }
    }

    /// End the recorder and reap it. Only for a track that has no thread:
    /// a thread clears the child as it reaps it.
    pub(super) fn kill(&self) {
        let mut slot = self.child.lock();
        if let Some(child) = slot.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        slot.take();
    }
}

/// A started track as its owner sees it.
pub(super) struct TrackHandle {
    pub kind: TrackKind,
    pub shared: Arc<Shared>,
    /// `None` once the thread ended and was joined.
    pub thread: Option<JoinHandle<()>>,
    /// Set when the thread reported, or when it was found ended without
    /// reporting.
    pub ended: bool,
}

/// One track to start.
pub(super) struct Spec<'a> {
    pub kind: TrackKind,
    /// `node.name`, for what the recorder reports.
    pub node: &'a str,
    /// The node's `object.serial`, what the recorder is pointed at.
    #[cfg(target_os = "linux")]
    pub serial: u64,
    /// The recorder programs, from the runtime's `Tools`.
    #[cfg(target_os = "linux")]
    pub recorder: &'a RecorderProfile,
    #[cfg(not(target_os = "linux"))]
    pub lease: &'a File,
    /// The meeting's directory.
    pub directory: &'a Path,
    pub limits: &'a Limits,
}

/// Why a track could not be prepared.
#[derive(Debug)]
pub(super) enum PrepareError {
    /// Its files could not be made.
    Storage(io::Error),
    /// Its recorder could not be started.
    #[cfg(target_os = "linux")]
    Recorder(io::Error),
    #[cfg(not(target_os = "linux"))]
    Native(Failure),
}

/// A track whose recorder runs and whose files are open, with nothing
/// reading it yet.
pub(super) struct Prepared {
    pub kind: TrackKind,
    node: String,
    directory: PathBuf,
    stdout: AudioInput,
    file: File,
    log: PathBuf,
    shared: Arc<Shared>,
    limits: Limits,
    #[cfg(target_os = "linux")]
    link: SourceLink,
}

/// What the graph has to show before the track keeps a sample.
#[cfg(target_os = "linux")]
struct SourceLink {
    recorder: RecorderProfile,
    pid: u32,
    serial: u64,
    stream: String,
}

impl Prepared {
    /// Start the recorder of `spec` and open its files. Nothing reads the
    /// recorder until `start`: a start that fails half-way has nothing
    /// running to unwind.
    pub fn prepare(
        spec: &Spec<'_>,
        journal: Arc<Journal>,
        notices: UnboundedSender<TrackNotice>,
    ) -> Result<Self, PrepareError> {
        let mut file = journal::private()
            .open(journal::partial_path(spec.directory, spec.kind))
            .map_err(PrepareError::Storage)?;
        // The header of an empty recording; the real one is written when the track is finalized.
        let header = crate::capture::wav_header(0, SAMPLE_RATE);
        file.write_all(&header).map_err(PrepareError::Storage)?;
        file.sync_data().map_err(PrepareError::Storage)?;
        let log = journal::log_path(spec.directory, spec.kind);
        #[cfg(target_os = "linux")]
        let stream = format!(
            "voiceless-meeting-{}-{}",
            std::process::id(),
            spec.kind.name()
        );
        #[cfg(target_os = "linux")]
        let target = Target::Node {
            name: spec.node,
            serial: spec.serial,
            monitor: spec.kind == TrackKind::System,
        };
        #[cfg(target_os = "linux")]
        let (child, stdout, pid) = {
            let stderr = journal::private()
                .open(&log)
                .map_err(PrepareError::Storage)?;
            // The recorder's stderr is the track's log; the rest of how it
            // runs is the recorder's to say.
            let mut command = spec.recorder.command(&stream, target);
            command.stderr(stderr);
            let mut child = command.spawn().map_err(PrepareError::Recorder)?;
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PrepareError::Recorder(io::Error::other(
                    "the recorder left its audio closed",
                )));
            };
            let pid = child.id();
            (Some(child), stdout, pid)
        };
        #[cfg(not(target_os = "linux"))]
        let (child, stdout) = (None, AudioInput::open(spec)?);
        Ok(Self {
            kind: spec.kind,
            #[cfg(target_os = "linux")]
            link: SourceLink {
                recorder: spec.recorder.clone(),
                pid,
                serial: spec.serial,
                stream,
            },
            node: spec.node.to_owned(),
            directory: spec.directory.to_path_buf(),
            stdout,
            file,
            log,
            shared: Arc::new(Shared::new(child, journal, notices)),
            limits: *spec.limits,
        })
    }

    /// Undo a prepared track: end its recorder and remove what it created.
    pub fn kill(self) {
        let Self {
            kind,
            directory,
            file,
            log,
            shared,
            ..
        } = self;
        shared.kill();
        drop(file);
        let _ = fs::remove_file(log);
        let _ = fs::remove_file(journal::partial_path(&directory, kind));
    }

    /// Hand the track to its thread.
    pub fn start(self) -> io::Result<TrackHandle> {
        let Self {
            kind,
            node,
            #[cfg(target_os = "linux")]
            link,
            directory,
            stdout,
            file,
            log,
            shared,
            limits,
        } = self;
        let mut recorder = Recorder {
            kind,
            node,
            #[cfg(target_os = "linux")]
            link,
            directory,
            stdout,
            file,
            log,
            max_bytes: limits.max_track_bytes,
            commit_bytes: limits.commit_bytes,
            stop_grace: Duration::from_millis(limits.stop_grace_ms),
        };
        let owner = Arc::clone(&shared);
        let thread = Builder::new()
            .name(format!("voiceless-meeting-{}", kind.name()))
            .spawn(move || {
                let failure = recorder.record(&shared);
                let _ = shared.notices.send(TrackNotice::Ended { kind, failure });
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                owner.kill();
                return Err(error);
            }
        };
        Ok(TrackHandle {
            kind,
            shared: owner,
            thread: Some(thread),
            ended: false,
        })
    }
}

/// What the thread owns while it records.
struct Recorder {
    kind: TrackKind,
    node: String,
    #[cfg(target_os = "linux")]
    link: SourceLink,
    directory: PathBuf,
    stdout: AudioInput,
    file: File,
    log: PathBuf,
    max_bytes: u64,
    commit_bytes: u64,
    stop_grace: Duration,
}

/// Why a recorder was asked to end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// The owner asked: what the recorder flushes on its way out is recorded.
    Stop,
    /// The track is at its limit: what comes now is not recorded.
    Limit,
}

impl Recorder {
    /// Record until the recorder is gone, then settle the file and the
    /// journal. Returns the failure that ended the track, when one did.
    fn record(&mut self, shared: &Shared) -> Option<Failure> {
        let mut received = 0_u64;
        let mut committed = 0_u64;
        // What may still be recorded before the track is at its limit.
        let mut budget = self.max_bytes;
        // `buffer[..head]` is what a read left at the front: the first byte
        // of a sample a read cut in two and, on Linux, the end of the
        // recorder's output that is kept back until it is known for audio.
        let mut head = 0_usize;
        #[cfg(target_os = "linux")]
        let mut buffer = vec![0_u8; READ_BYTES + OUTPUT_TAIL];
        #[cfg(not(target_os = "linux"))]
        let mut buffer = Vec::new();
        let mut draining: Option<(Instant, Drain)> = None;
        let mut recorder_error: Option<io::Error> = None;
        let mut storage_error: Option<io::Error> = None;
        let mut torn = false;
        // Whether the recorder closed its output, as against being killed.
        #[cfg(target_os = "linux")]
        let mut ended = false;
        // A present device gets time to relink. Its removal ends the track.
        #[cfg(target_os = "linux")]
        let mut unlinked = None;
        #[cfg(target_os = "linux")]
        let mut watched = Instant::now();
        #[cfg(target_os = "linux")]
        let mut lost = None;
        #[cfg(target_os = "linux")]
        if let Some(failure) = self.unverified(shared) {
            return Some(failure);
        }
        // Every sample the track keeps is written, metered and journaled here.
        let mut keep = |pcm: &[u8]| -> io::Result<()> {
            self.file.write_all(pcm)?;
            received += pcm.len() as u64;
            shared.received.store(received, Ordering::Relaxed);
            shared.level.store(level(pcm), Ordering::Relaxed);
            if shared.first_audio_at.get().is_none() {
                let _ = shared.first_audio_at.set(journal::now());
            }
            if received - committed >= self.commit_bytes {
                commit(&self.file, shared, self.kind, received)?;
                committed = received;
            }
            Ok(())
        };
        loop {
            #[cfg(not(target_os = "linux"))]
            if shared.abort.load(Ordering::Acquire) {
                break;
            }
            if let Some((deadline, _)) = &draining {
                if Instant::now() >= *deadline {
                    #[cfg(not(target_os = "linux"))]
                    if !self.stdout.drained {
                        recorder_error = Some(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "native drain deadline exceeded",
                        ));
                    }
                    break;
                }
            } else if shared.stop.load(Ordering::Acquire) {
                #[cfg(target_os = "linux")]
                interrupt(shared);
                draining = Some((Instant::now() + self.stop_grace, Drain::Stop));
            } else if budget == 0 {
                #[cfg(target_os = "linux")]
                interrupt(shared);
                draining = Some((Instant::now() + self.stop_grace, Drain::Limit));
                let _ = shared.notices.send(TrackNotice::LimitReached);
            }
            // `gst-launch-1.0` keeps running with no audio once its source
            // is gone: the graph is asked for the link until it is not there.
            #[cfg(target_os = "linux")]
            if draining.is_none()
                && self.link.recorder.watches_link()
                && watched.elapsed() >= LINK_WATCH
            {
                watched = Instant::now();
                let target = Target::Node {
                    name: &self.node,
                    serial: self.link.serial,
                    monitor: self.kind == TrackKind::System,
                };
                if let Ok(link) =
                    self.link
                        .recorder
                        .linked_now(&self.link.stream, target, self.link.pid)
                {
                    lost = link.loss(&mut unlinked, &self.link.stream);
                    if lost.is_some() {
                        break;
                    }
                }
            }
            #[cfg(target_os = "linux")]
            match wait_readable(&self.stdout, TICK) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    recorder_error = Some(error);
                    break;
                }
            }
            #[cfg(target_os = "linux")]
            let read = match self.stdout.read(&mut buffer[head..]) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    recorder_error = Some(error);
                    break;
                }
            };
            #[cfg(not(target_os = "linux"))]
            let read = match self
                .stdout
                .read(&mut buffer, draining.is_some(), self.stop_grace)
            {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(error) => {
                    recorder_error = Some(error);
                    break;
                }
            };
            if read == 0 {
                #[cfg(not(target_os = "linux"))]
                {
                    torn = head == 1;
                }
                #[cfg(target_os = "linux")]
                {
                    ended = true;
                }
                break;
            }
            // A recorder draining after its limit delivers audio that is not recorded.
            if matches!(draining, Some((_, Drain::Limit))) {
                #[cfg(target_os = "linux")]
                {
                    head = 0;
                }
                continue;
            }
            let total = head + read;
            #[cfg(not(target_os = "linux"))]
            let usable = (total & !1).min(fits(budget));
            // Linux: the end of the output stays back, because it may be
            // the line `pw-record` prints after its last sample.
            #[cfg(target_os = "linux")]
            let usable = (total.saturating_sub(OUTPUT_TAIL) & !1).min(fits(budget));
            if usable > 0 {
                match keep(&buffer[..usable]) {
                    Ok(()) => budget -= usable as u64,
                    Err(error) => {
                        storage_error = Some(error);
                        break;
                    }
                }
            }
            // The first byte of a sample a read cut in two moves to the
            // front; a frame the limit cut leaves nothing to carry.
            #[cfg(not(target_os = "linux"))]
            {
                head = if usable == total {
                    0
                } else if usable + 1 == total {
                    buffer[0] = buffer[usable];
                    1
                } else {
                    0
                };
            }
            // What was not kept moves to the front.
            #[cfg(target_os = "linux")]
            {
                let rest = total - usable;
                buffer.copy_within(usable..total, 0);
                head = rest;
            }
        }

        // Linux: what was kept back is audio once the recorder is gone. It
        // ends with the line `pw-record` prints after its last sample when
        // the recorder closed its output; a recorder killed for outliving
        // its drain time was still writing audio.
        #[cfg(target_os = "linux")]
        let (audio, said) = if ended {
            output_ends(self.link.recorder.backend(), &buffer[..head])
        } else {
            (head, Vec::new())
        };
        #[cfg(target_os = "linux")]
        if storage_error.is_none() && !matches!(draining, Some((_, Drain::Limit))) {
            let usable = (audio & !1).min(fits(budget));
            if usable > 0 {
                if let Err(error) = keep(&buffer[..usable]) {
                    storage_error = Some(error);
                }
            }
            // A sample the stream's end cut in two is not a recorder that
            // misbehaved when the recorder said why it ended.
            torn = audio % 2 == 1 && said.is_empty();
        }

        #[cfg(target_os = "linux")]
        let deadline = draining.map_or_else(|| Instant::now() + TICK, |(deadline, _)| deadline);
        #[cfg(target_os = "linux")]
        let exit = self.reap(shared, deadline);
        #[cfg(target_os = "linux")]
        // The recorder's own words about its stream come before its log.
        let diagnostic = Diagnostic::default().with_lines(said).with_log(&self.log);

        // Whatever ended the track, its audio is worth keeping: make it
        // durable and journal it, as a commit does.
        if storage_error.is_none() {
            if let Err(error) = commit(&self.file, shared, self.kind, received) {
                storage_error = Some(error);
            }
        }
        if storage_error.is_some() {
            // The journal says how much is on disk for good; the file is cut back to that.
            let kept = journaled(shared, self.kind);
            let _ = self.file.set_len(journal::HEADER_BYTES as u64 + kept);
        }

        let failure = if shared.abort.load(Ordering::Acquire) {
            Some(Failure::ProcessInterrupted)
        } else if let Some(error) = &storage_error {
            Some(Failure::DataUnwritable {
                reason: format!(
                    "{}: {error}",
                    journal::partial_path(&self.directory, self.kind).display()
                ),
            })
        } else if let Some(error) = &recorder_error {
            #[cfg(not(target_os = "linux"))]
            {
                Some(Failure::StreamEnded {
                    source: self.kind,
                    node: self.node.clone(),
                    reason: error.to_string(),
                })
            }
            #[cfg(target_os = "linux")]
            Some(Failure::RecorderFailed {
                source: self.kind,
                reason: format!("its audio could not be read: {error}"),
            })
        } else if torn {
            Some(Failure::RecorderFailed {
                source: self.kind,
                reason: TORN.to_owned(),
            })
        } else {
            #[cfg(not(target_os = "linux"))]
            {
                None
            }
            #[cfg(target_os = "linux")]
            if let Some(error) = lost {
                Some(if error.kind() == io::ErrorKind::PermissionDenied {
                    self.denied(&error.to_string())
                } else {
                    Failure::StreamEnded {
                        source: self.kind,
                        node: self.node.clone(),
                        reason: error.to_string(),
                    }
                })
            } else {
                self.ended(exit, &diagnostic, draining.is_some(), received)
            }
        };
        match failure {
            None => self.finish(shared, received),
            Some(failure) => {
                fail(shared, self.kind, &failure);
                Some(failure)
            }
        }
    }

    /// Wait for the graph to show this track's node feeding its stream,
    /// with the recorder's first samples left in its pipe. `Some` is the
    /// failure of a track whose stream could not be verified: its recorder
    /// is ended by then and nothing of it is kept.
    #[cfg(target_os = "linux")]
    fn unverified(&mut self, shared: &Shared) -> Option<Failure> {
        // Use a private reactor: runtime shutdown may synchronously join this thread.
        let verified = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .and_then(|runtime| {
                runtime.block_on(self.link.recorder.verify(
                    &self.link.stream,
                    self.target(),
                    Some(self.link.pid),
                    || {
                        if shared.abort.load(Ordering::Acquire)
                            || shared.stop.load(Ordering::Acquire)
                        {
                            return Err(io::Error::other("stopped before verifying source"));
                        }
                        if let Some(child) = shared.child.lock().as_mut() {
                            if let Some(exit) = child.try_wait()? {
                                return Err(io::Error::other(format!(
                                    "recorder exited with {exit}"
                                )));
                            }
                        }
                        Ok(())
                    },
                ))
            });
        let Err(error) = verified else {
            return None;
        };
        self.reap(shared, Instant::now() + TICK);
        // What the recorder wrote and nobody read: on PipeWire before 1.4
        // the line that says why its stream ended is at the end of it.
        let output = leftover(&mut self.stdout);
        let diagnostic = Diagnostic::default()
            .with_output(&output)
            .with_log(&self.log);
        let failure = if error.kind() == io::ErrorKind::PermissionDenied {
            self.denied(&error.to_string())
        } else if diagnostic.permission {
            self.denied(diagnostic.fatal.as_deref().unwrap_or("permission denied"))
        } else if shared.abort.load(Ordering::Acquire) {
            Failure::ProcessInterrupted
        } else if shared.stop.load(Ordering::Acquire) && diagnostic.fatal.is_none() {
            return self.finish(shared, 0);
        } else {
            Failure::StreamUnconnected {
                source: self.kind,
                node: self.node.clone(),
                reason: diagnostic.fatal.unwrap_or_else(|| error.to_string()),
            }
        };
        fail(shared, self.kind, &failure);
        Some(failure)
    }

    /// This track's node, as its recorder was pointed at it.
    #[cfg(target_os = "linux")]
    fn target(&self) -> Target<'_> {
        Target::Node {
            name: &self.node,
            serial: self.link.serial,
            monitor: self.kind == TrackKind::System,
        }
    }

    /// Wait for the recorder, killing it if it outlived its drain time,
    /// and clear it so that nobody signals its pid again.
    #[cfg(target_os = "linux")]
    fn reap(&self, shared: &Shared, deadline: Instant) -> Option<ExitStatus> {
        let mut slot = shared.child.lock();
        let child = slot.as_mut()?;
        // Closing stdout and becoming waitable are separate steps of process exit.
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                _ => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        let status = child.wait().ok();
        slot.take();
        status
    }

    /// Why the recorder is gone when nobody asked for it, or why a stop
    /// did not go cleanly.
    #[cfg(target_os = "linux")]
    fn ended(
        &self,
        exit: Option<ExitStatus>,
        diagnostic: &Diagnostic,
        requested: bool,
        received: u64,
    ) -> Option<Failure> {
        let Some(exit) = exit else {
            return Some(Failure::RecorderFailed {
                source: self.kind,
                reason: "its exit status could not be read".to_owned(),
            });
        };
        let reported = diagnostic.fatal.as_deref();
        if requested {
            // A stop was asked for: only a recorder that ended as asked is a stop.
            return match reported {
                Some(reason) if diagnostic.permission => Some(self.denied(reason)),
                Some(reason) => Some(Failure::RecorderFailed {
                    source: self.kind,
                    reason: format!("it failed while stopping: {reason}"),
                }),
                None if recorder::stopped_as_asked(&exit, self.link.recorder.backend()) => None,
                None => Some(Failure::RecorderFailed {
                    source: self.kind,
                    reason: format!("it exited while stopping ({exit})"),
                }),
            };
        }
        // No audio ever came: the stream was not connected. Audio came and
        // stopped: the source left.
        match (reported, diagnostic.permission, received) {
            (Some(reason), true, _) => Some(self.denied(reason)),
            (Some(reason), false, 0) => Some(Failure::StreamUnconnected {
                source: self.kind,
                node: self.node.clone(),
                reason: reason.to_owned(),
            }),
            (Some(reason), false, _) => Some(Failure::StreamEnded {
                source: self.kind,
                node: self.node.clone(),
                reason: reason.to_owned(),
            }),
            (None, _, 0) => Some(Failure::StreamUnconnected {
                source: self.kind,
                node: self.node.clone(),
                reason: format!("it exited before delivering audio ({exit})"),
            }),
            (None, _, _) => Some(Failure::StreamEnded {
                source: self.kind,
                node: self.node.clone(),
                reason: format!("it exited on its own ({exit})"),
            }),
        }
    }

    #[cfg(target_os = "linux")]
    fn denied(&self, reason: &str) -> Failure {
        Failure::PermissionDenied {
            source: self.kind,
            node: self.node.clone(),
            reason: reason.to_owned(),
            remedy: recorder::remedy().to_owned(),
        }
    }

    /// A track that stopped as asked: finalize its file when it holds
    /// audio, and journal that.
    fn finish(&self, shared: &Shared, received: u64) -> Option<Failure> {
        let failure = |error: io::Error| Failure::DataUnwritable {
            reason: format!(
                "cannot finalize {}: {error}",
                journal::partial_path(&self.directory, self.kind).display()
            ),
        };
        let path = match journal::finish_track(&self.directory, self.kind, received) {
            Ok(path) => path,
            Err(error) => {
                let failure = failure(error);
                fail(shared, self.kind, &failure);
                return Some(failure);
            }
        };
        if let Err(error) = shared.journal.update(|doc| {
            let entry = doc.track_mut(self.kind);
            entry.status = if path.is_some() {
                TrackStatus::Finalized
            } else {
                TrackStatus::Empty
            };
            entry.committed_bytes = received;
            entry.note_first_audio(shared.first_audio_at.get().map(String::as_str));
        }) {
            return Some(failure(error));
        }
        let _ = fs::remove_file(&self.log);
        None
    }
}

/// Ask the recorder to end. SIGINT lets it drain its buffers; SIGKILL
/// would cut the tail of what was said.
#[cfg(target_os = "linux")]
fn interrupt(shared: &Shared) {
    let child = shared.child.lock();
    if let Some(child) = child.as_ref() {
        // SAFETY: the pid is a child this process spawned and has not
        // reaped; the thread that owns it clears it as it does.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGINT);
        }
    }
}

/// Make the first `bytes` of the track durable, then journal them as kept.
/// The order is the guarantee: audio is never journaled before it is on
/// disk.
fn commit(file: &File, shared: &Shared, kind: TrackKind, bytes: u64) -> io::Result<()> {
    file.sync_data()?;
    shared.journal.update(|doc| {
        let entry = doc.track_mut(kind);
        entry.committed_bytes = bytes;
        entry.note_first_audio(shared.first_audio_at.get().map(String::as_str));
    })?;
    Ok(())
}

/// What the journal last kept for `kind`: the truth once a commit failed
/// part-way.
fn journaled(shared: &Shared, kind: TrackKind) -> u64 {
    shared
        .journal
        .get()
        .tracks
        .iter()
        .find(|track| track.kind == kind)
        .map_or(0, |track| track.committed_bytes)
}

/// Journal that `kind` ended with `failure`. Its file stays unfinished
/// until the meeting is recovered or discarded.
fn fail(shared: &Shared, kind: TrackKind, failure: &Failure) {
    let _ = shared.journal.update(|doc| {
        let entry = doc.track_mut(kind);
        entry.status = TrackStatus::Failed;
        entry.error = Some(failure.to_string());
        entry.note_first_audio(shared.first_audio_at.get().map(String::as_str));
    });
}

/// Whether the recorder has audio to read or ended, after at most
/// `timeout`. `false` when nothing came in time.
#[cfg(target_os = "linux")]
fn wait_readable(stdout: &ChildStdout, timeout: Duration) -> io::Result<bool> {
    let mut poller = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `poller` is live for the call, and `poll` reads that one entry and nothing else.
    let ready = unsafe { libc::poll(&mut poller, 1, timeout) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        };
    }
    Ok(ready > 0)
}

/// What a reaped recorder left in its pipe, read for at most [`TICK`]
/// after the last of it: the recorder is gone, but a stand-in's helper may
/// still hold the pipe open.
#[cfg(target_os = "linux")]
fn leftover(stdout: &mut ChildStdout) -> Vec<u8> {
    let mut left = Vec::new();
    let mut buffer = [0_u8; 8192];
    while matches!(wait_readable(stdout, TICK), Ok(true)) {
        match stdout.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => left.extend_from_slice(&buffer[..read]),
        }
    }
    left
}

/// The end of a recorder's output as audio: where the audio stops, and the
/// lines `pw-record` prints after its last sample on PipeWire before 1.4
/// when it printed them. `gst-launch-1.0` prints nothing into its output.
#[cfg(target_os = "linux")]
fn output_ends(backend: RecorderBackend, held: &[u8]) -> (usize, Vec<String>) {
    match backend {
        RecorderBackend::PwRecord { .. } => recorder::split_output(held),
        RecorderBackend::Gstreamer => (held.len(), Vec::new()),
    }
}

/// A byte count as a size on this platform: never more than there is.
fn fits(bytes: u64) -> usize {
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// RMS of a PCM16 little-endian frame, scaled to 0..1: the dictation
/// level's definition (`coordinator`).
fn level(pcm: &[u8]) -> u32 {
    let samples = pcm.len() / 2;
    if samples == 0 {
        return 0.0_f32.to_bits();
    }
    let sum: f64 = pcm
        .chunks_exact(2)
        .map(|pair| {
            let sample = f64::from(i16::from_le_bytes([pair[0], pair[1]])) / 32768.0;
            sample * sample
        })
        .sum();
    ((sum / samples as f64).sqrt() as f32).to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_count_never_exceeds_what_this_platform_can_size() {
        assert_eq!(fits(u64::MAX), usize::MAX);
        assert_eq!(fits(50), 50);
    }

    #[test]
    fn levels_follow_the_dictation_definition() {
        assert_eq!(f32::from_bits(level(&[])), 0.0);
        assert_eq!(f32::from_bits(level(&[0, 0, 0, 0])), 0.0);
        // 16384 and -16384 are +0.5 and -0.5: their RMS is 0.5.
        assert_eq!(f32::from_bits(level(&[0x00, 0x40, 0x00, 0xC0])), 0.5);
        // The loudest sample is 1, and the odd byte of a torn sample is not one.
        assert!((f32::from_bits(level(&[0xFF, 0x7F, 0xFF])) - 1.0).abs() < 1e-4);
    }
}
