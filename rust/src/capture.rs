//! Microphone capture for the realtime pipeline. `RawCapture` runs a
//! PipeWire recorder and re-frames its raw stdout into fixed 32 ms PCM16
//! chunks; `WavRecorder` decorates any
//! `coordinator::AudioCapture` and mirrors the audio into a WAV file for
//! history retention.
//!
//! `stream::PwCapture` is the production `AudioCapture` over `RawCapture`,
//! and `coordinator::run_session` always wraps whatever capture it is
//! given in a `WavRecorder` aimed at `SessionPlan::save_audio_dir`
//! (`[history].save_audio`, files under `~/.local/share/voiceless/audio/`).
//! `wav_header` is also used by tests that fabricate WAV fixtures.
//!
//! Linux: `recorder` is the recorder that the dictation capture and the
//! meeting tracks (`meeting_capture`) share. It says what records on this
//! machine, points a stream at one exact node and has the graph prove that
//! the node feeds it before a sample is kept.
//!
//! Format contract: 16 kHz, mono, signed 16-bit little-endian
//! (`SAMPLE_RATE`), delivered in `CHUNK_BYTES` (1024 B = 32 ms) chunks;
//! only the last chunk before end of stream, and the tail returned by
//! `stop`, may have another length. An odd byte count (a torn sample) is
//! reported as `InvalidData`, never passed on.
//!
//! Failure behaviour: a missing recorder fails `RawCapture::start`, and
//! with it the session before `Ready`; so does a stream that is not fed by
//! the selected microphone, which is never replaced by another one. The
//! WAV mirror is best effort and never affects recognition, injection or
//! history.

use std::io::{self, Seek, Write};

#[cfg(target_os = "linux")]
pub(crate) mod recorder;

#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use recorder::{Diagnostic, RecorderBackend, RecorderProfile, Target, LINK_WATCH};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
#[cfg(target_os = "linux")]
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
#[cfg(target_os = "linux")]
use tokio::task::JoinHandle;

use crate::coordinator::AudioCapture;

/// Capture sample rate in Hz; every backend consumes 16 kHz mono PCM16.
pub const SAMPLE_RATE: u32 = 16_000;
/// Bytes per chunk: 32 ms of 16 kHz mono PCM16 (0.032 * 16000 * 2).
pub const CHUNK_BYTES: usize = 1_024;

#[cfg(not(target_os = "linux"))]
pub use crate::desktop::audio::RawCapture;

#[cfg(target_os = "linux")]
/// A running recorder child whose stdout is re-framed into
/// `CHUNK_BYTES` chunks. Dropping it kills the child (`kill_on_drop`).
pub struct RawCapture {
    child: Child,
    stdout: ChildStdout,
    /// Reads the recorder's stderr as it comes: what it reported once it ended.
    diagnostics: JoinHandle<io::Result<Diagnostic>>,
    /// What records, and how this stream was pointed at its node: its name
    /// and serial when a microphone was selected.
    profile: RecorderProfile,
    stream: String,
    target: Option<(String, u64)>,
    /// First observation of a missing link while the selected device still exists.
    unlinked: Option<Instant>,
    watched: Instant,
    /// Bytes read from the child but not yet returned as a full chunk.
    pending: Vec<u8>,
    _capture_lock: std::fs::File,
}

#[cfg(target_os = "linux")]
impl RawCapture {
    /// Open one recording stream: the selected microphone's, or the
    /// system default's when `source` is `None`. The recorder program and
    /// the microphone are looked up again, so a microphone that
    /// disappeared since it was selected fails here, by name.
    pub async fn start(source: Option<&str>) -> io::Result<Self> {
        let capture_lock = crate::runtime::lock_capture()?;
        let profile = RecorderProfile::detect_on_path().await?;
        let target = match source {
            Some(name) => Target::Node {
                name,
                serial: crate::desktop::audio::require_source(name)
                    .await
                    .map_err(io::Error::other)?,
                monitor: false,
            },
            None => Target::Default,
        };
        let node = crate::desktop::audio::capture_node_name();
        let mut command = Command::from(profile.command(&node, target));
        command.stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot start {}: {error}",
                    command.as_std().get_program().to_string_lossy()
                ),
            )
        })?;
        let stdout = child.stdout.take().expect("recorder stdout is piped");
        let stderr = child.stderr.take().expect("recorder stderr is piped");
        let diagnostics = tokio::spawn(recorder_diagnostics(stderr));
        let mut capture = Self {
            child,
            stdout,
            diagnostics,
            profile,
            target: target
                .node()
                .map(|(name, serial)| (name.to_owned(), serial)),
            stream: node,
            unlinked: None,
            watched: Instant::now(),
            pending: Vec::with_capacity(CHUNK_BYTES),
            _capture_lock: capture_lock,
        };
        // The session manager may fall back if the target disappears during
        // setup. Keep the first samples in the pipe until the link has been
        // verified.
        if target.node().is_some() {
            if let Err(error) = capture
                .profile
                .verify(
                    &capture.stream,
                    target,
                    capture.child.id(),
                    || match capture.child.try_wait()? {
                        None => Ok(()),
                        Some(status) => {
                            Err(io::Error::other(format!("recorder exited with {status}")))
                        }
                    },
                )
                .await
            {
                return Err(capture.refused(error).await);
            }
        }
        Ok(capture)
    }

    /// The recorder could not be verified: end it and say why. What the
    /// recorder reported comes before what was observed of it.
    async fn refused(mut self, error: io::Error) -> io::Error {
        // Retain the capture lock until the child has been reaped.
        let _ = self.child.kill().await;
        // What it wrote but was never read, which on PipeWire before 1.4
        // ends with the line saying why its stream ended.
        let mut output = Vec::new();
        let read = self.stdout.read_to_end(&mut output);
        let _ = tokio::time::timeout(Duration::from_secs(1), read).await;
        self.said().await.with_output(&output).reported(&error)
    }

    /// What the recorder reported. Its stderr is read as it comes, so this
    /// is everything once the recorder ended; a second is the longest wait
    /// for it, in case something the recorder started still holds the pipe.
    async fn said(&mut self) -> Diagnostic {
        tokio::time::timeout(Duration::from_secs(1), &mut self.diagnostics)
            .await
            .ok()
            .and_then(|said| said.ok())
            .and_then(|said| said.ok())
            .unwrap_or_default()
    }

    /// The node this stream was pointed at, when a microphone was selected.
    fn target(&self) -> Target<'_> {
        match &self.target {
            Some((name, serial)) => Target::Node {
                name,
                serial: *serial,
                monitor: false,
            },
            None => Target::Default,
        }
    }

    /// One read from the recorder. For a recorder that does not end when
    /// its source is gone, a second without audio is the moment to look at
    /// the graph.
    async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.profile.watches_link() {
            return self.stdout.read(buffer).await;
        }
        loop {
            if self.watched.elapsed() >= LINK_WATCH {
                self.watched = Instant::now();
                self.watch().await?;
            }
            if let Ok(read) = tokio::time::timeout(LINK_WATCH, self.stdout.read(buffer)).await {
                return read;
            }
        }
    }

    /// End a recorder that kept running after its source disconnected.
    async fn watch(&mut self) -> io::Result<()> {
        let Ok(link) = self
            .profile
            .linked(&self.stream, self.target(), self.child.id())
            .await
        else {
            // An unreadable graph gives no evidence that the source left.
            return Ok(());
        };
        let Some(error) = link.loss(&mut self.unlinked, &self.stream) else {
            return Ok(());
        };
        let _ = self.child.kill().await;
        if error.kind() == io::ErrorKind::PermissionDenied {
            return Err(self.said().await.reported(&error));
        }
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("microphone disconnected: {error}"),
        ))
    }

    /// Read one chunk. EOF before a stop is a recorder failure. Dropping a
    /// read future preserves all bytes already received from the child.
    pub async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buffer = [0_u8; CHUNK_BYTES];
        loop {
            let read = self.read(&mut buffer).await?;
            if read == 0 {
                let mut reason =
                    "microphone disconnected or recorder stopped unexpectedly".to_owned();
                // `pw-record` on PipeWire before 1.4 says why its stream
                // ended with the last bytes of its output.
                if matches!(self.profile.backend(), RecorderBackend::PwRecord { .. }) {
                    let (_, lines) = recorder::split_output(&self.pending);
                    if !lines.is_empty() {
                        reason.push_str(": ");
                        reason.push_str(&lines.join(" "));
                    }
                }
                let observed = io::Error::new(io::ErrorKind::BrokenPipe, reason);
                return Err(self.said().await.reported(&observed));
            }
            self.pending.extend_from_slice(&buffer[..read]);
            if self.pending.len() >= CHUNK_BYTES {
                return Ok(Some(self.pending.drain(..CHUNK_BYTES).collect()));
            }
        }
    }

    /// Ask the recorder to flush, then reap it and return any aligned tail.
    /// A recorder that already exited or reported a failure interrupted capture.
    pub async fn stop(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.child.try_wait()?.is_some() {
            let observed = io::Error::new(io::ErrorKind::BrokenPipe, "recorder exited before stop");
            return Err(self.said().await.reported(&observed));
        }
        if let Some(pid) = self.child.id() {
            // SIGINT lets pw-record drain its buffer; kill() would truncate
            // the tail of the utterance.
            // SAFETY: the pid is a child this process spawned and has not reaped.
            if unsafe { libc::kill(pid as i32, libc::SIGINT) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        self.stdout.read_to_end(&mut self.pending).await?;
        let status = self.child.wait().await?;
        let said = self.said().await;
        if !recorder::stopped_as_asked(&status, self.profile.backend()) || said.fatal.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                said.fatal
                    .unwrap_or_else(|| format!("recorder exited while stopping ({status})")),
            ));
        }
        if !self.pending.len().is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture ended with an incomplete PCM16 sample",
            ));
        }
        if self.pending.is_empty() {
            Ok(None)
        } else {
            Ok(Some(std::mem::take(&mut self.pending)))
        }
    }

    pub(crate) async fn abort(&mut self) {
        // Retain the capture lock until the child has been reaped.
        let _ = self.child.kill().await;
    }
}

#[cfg(target_os = "linux")]
/// Keep the recorder's diagnostics visible and remember what it reported:
/// a fatal error of its stream, and whether that is a denial.
async fn recorder_diagnostics(stderr: ChildStderr) -> io::Result<Diagnostic> {
    let mut reader = BufReader::new(stderr);
    let mut bytes = Vec::new();
    let mut diagnostic = Diagnostic::default();
    while reader.read_until(b'\n', &mut bytes).await? != 0 {
        let line = String::from_utf8_lossy(&bytes);
        eprint!("{line}");
        diagnostic.absorb(&line);
        bytes.clear();
    }
    Ok(diagnostic)
}

/// Mirror captured PCM once into a private temporary file. The coordinator
/// publishes it only after a successful transcript or an opted-in failure.
/// Dropping or aborting a recorder discards unclaimed audio.
pub struct WavRecorder<C> {
    inner: C,
    sample_rate: u32,
    /// `None` when saving is disabled, has failed, or is finished.
    state: Option<RecorderState>,
}

/// The open partial file plus what is needed to finalize or discard it.
struct RecorderState {
    file: tempfile::NamedTempFile,
    final_path: std::path::PathBuf,
    /// PCM bytes written after the 44-byte header.
    data_bytes: u64,
}

impl<C> WavRecorder<C> {
    /// `destination` is the final WAV path; samples are written to a
    /// sibling `.partial` file until the session completes successfully.
    /// `None` (saving disabled or uncreatable) makes the wrapper a
    /// pass-through.
    pub fn new(inner: C, sample_rate: u32, destination: Option<std::path::PathBuf>) -> Self {
        let state = destination.and_then(|final_path| {
            let file = tempfile::Builder::new()
                .prefix(".voiceless-")
                .suffix(".wav.partial")
                .tempfile_in(final_path.parent()?)
                .ok()?;
            Some(RecorderState {
                file,
                final_path,
                data_bytes: 0,
            })
        });
        Self {
            inner,
            sample_rate,
            state,
        }
    }

    /// Write the placeholder header (data size 0) once the inner capture
    /// has started; `finish` patches the real size in.
    fn write_header(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state
            .file
            .write_all(&wav_header(0, self.sample_rate))
            .is_err()
        {
            self.disable();
        }
    }

    fn append(&mut self, pcm: &[u8]) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // The recognizer and injector must never depend on the mirror
        // file: a failed write disables saving for the whole session.
        if state.data_bytes.saturating_add(pcm.len() as u64) > u64::from(u32::MAX - 36)
            || state.file.write_all(pcm).is_err()
        {
            self.disable();
        } else {
            state.data_bytes += pcm.len() as u64;
        }
    }

    /// Stop mirroring for the rest of the session and delete the partial
    /// file. Idempotent.
    fn disable(&mut self) {
        self.state = None;
    }

    /// Patch the sizes before handing the closed file to either store.
    fn finalize(&mut self) -> Option<RecorderState> {
        let mut state = self.state.take()?;
        if state.data_bytes < 2 || state.data_bytes % 2 != 0 {
            return None;
        }
        state.file.seek(io::SeekFrom::Start(0)).ok()?;
        state
            .file
            .write_all(&wav_header(state.data_bytes, self.sample_rate))
            .ok()?;
        Some(state)
    }

    pub(crate) fn failed_recording(&mut self) -> Option<tempfile::TempPath> {
        // An interrupted PCM16 frame can still have a playable prefix.
        let state = self.state.as_mut()?;
        if state.data_bytes % 2 != 0 {
            state.data_bytes -= 1;
            state.file.as_file().set_len(44 + state.data_bytes).ok()?;
        }
        Some(self.finalize()?.file.into_temp_path())
    }
}

impl<C: AudioCapture> AudioCapture for WavRecorder<C> {
    async fn start(&mut self) -> io::Result<()> {
        let result = self.inner.start().await;
        if result.is_ok() {
            self.write_header();
        }
        result
    }

    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let chunk = self.inner.next_chunk().await?;
        if let Some(pcm) = &chunk {
            self.append(pcm);
        }
        Ok(chunk)
    }

    async fn stop_capture(&mut self) -> io::Result<Option<Vec<u8>>> {
        let tail = self.inner.stop_capture().await?;
        if let Some(pcm) = &tail {
            self.append(pcm);
        }
        Ok(tail)
    }

    async fn abort(&mut self) {
        // Remove the partial file before aborting the inner capture so a
        // cancelled session never leaks audio on disk.
        self.disable();
        self.inner.abort().await;
    }

    fn finish_recording(&mut self) -> Option<std::path::PathBuf> {
        let state = self.finalize()?;
        state.file.persist_noclobber(&state.final_path).ok()?;
        Some(state.final_path)
    }
}

/// Minimal canonical PCM16 mono WAV header (44 bytes). `data_bytes` may be
/// 0 at creation time and is patched once the session finishes. Sizes are
/// truncated to `u32` as the RIFF format requires.
pub fn wav_header(data_bytes: u64, sample_rate: u32) -> [u8; 44] {
    let riff_size = (36 + data_bytes) as u32;
    let byte_rate = sample_rate * 2;
    let mut header = [0_u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&1_u16.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&2_u16.to_le_bytes());
    header[34..36].copy_from_slice(&16_u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&(data_bytes as u32).to_le_bytes());
    header
}
