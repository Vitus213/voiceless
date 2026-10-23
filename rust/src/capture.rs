//! Microphone capture for the realtime pipeline. `RawCapture` runs
//! PipeWire's `pw-record` as a child process and re-frames its stdout into
//! fixed 32 ms PCM16 chunks; `WavRecorder` decorates any
//! `coordinator::AudioCapture` and mirrors the audio into a WAV file for
//! history retention.
//!
//! `stream::PwCapture` is the production `AudioCapture` over `RawCapture`,
//! and `coordinator::run_session` always wraps whatever capture it is
//! given in a `WavRecorder` aimed at `SessionPlan::save_audio_dir`
//! (`[history].save_audio`, files under `~/.local/share/voiceless/audio/`).
//! `wav_header` is also used by tests that fabricate WAV fixtures.
//!
//! Format contract: 16 kHz, mono, signed 16-bit little-endian
//! (`SAMPLE_RATE`), delivered in `CHUNK_BYTES` (1024 B = 32 ms) chunks;
//! only the last chunk before end of stream, and the tail returned by
//! `stop`, may have another length. An odd byte count (a torn sample) is
//! reported as `InvalidData`, never passed on.
//!
//! Failure behaviour: a missing `pw-record` fails `RawCapture::start`, and
//! with it the session before `Ready`; the WAV mirror is best effort and
//! never affects recognition, injection or history.

use std::io::{self, Seek, Write};
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};

use crate::coordinator::AudioCapture;

/// Capture sample rate in Hz; every backend consumes 16 kHz mono PCM16.
pub const SAMPLE_RATE: u32 = 16_000;
/// Bytes per chunk: 32 ms of 16 kHz mono PCM16 (0.032 * 16000 * 2).
pub const CHUNK_BYTES: usize = 1_024;

/// A running `pw-record` child whose stdout is re-framed into
/// `CHUNK_BYTES` chunks. Dropping it kills the child (`kill_on_drop`).
pub struct RawCapture {
    child: Child,
    stdout: ChildStdout,
    /// Bytes read from the child but not yet returned as a full chunk.
    pending: Vec<u8>,
}

impl RawCapture {
    /// Spawn `pw-record --rate 16000 --channels 1 --format s16 --raw
    /// --latency 32ms -` with stdout piped and stderr inherited, so PipeWire
    /// diagnostics reach the terminal. Fails when the binary is missing or
    /// cannot be spawned; a PipeWire-level failure shows up later as end of
    /// stream from `next_chunk`.
    pub fn start() -> io::Result<Self> {
        let mut child = Command::new("pw-record")
            .args([
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "s16",
                "--raw",
                "--latency",
                "32ms",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "pw-record stdout unavailable")
        })?;
        Ok(Self {
            child,
            stdout,
            pending: Vec::with_capacity(CHUNK_BYTES),
        })
    }

    /// Next `CHUNK_BYTES` chunk, waiting until that many bytes arrived. At
    /// end of stream the remainder is returned as one shorter chunk, then
    /// `Ok(None)`; an odd remainder is an `InvalidData` error. Cancellation
    /// safe: a dropped call loses no bytes (the read is atomic with the
    /// append to `pending`).
    pub async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buffer = [0_u8; CHUNK_BYTES];
        loop {
            let read = self.stdout.read(&mut buffer).await?;
            if read == 0 {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                if !self.pending.len().is_multiple_of(2) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "capture ended with an incomplete PCM16 sample",
                    ));
                }
                return Ok(Some(std::mem::take(&mut self.pending)));
            }
            self.pending.extend_from_slice(&buffer[..read]);
            if self.pending.len() >= CHUNK_BYTES {
                return Ok(Some(self.pending.drain(..CHUNK_BYTES).collect()));
            }
        }
    }

    /// Graceful stop: SIGINT `pw-record` so it flushes and exits, read
    /// everything it still writes, then reap it. Returns the leftover bytes
    /// (any even length) as the tail frame, `Ok(None)` when nothing is left
    /// or the child failed without output, and `InvalidData` for an odd
    /// remainder.
    pub async fn stop(&mut self) -> io::Result<Option<Vec<u8>>> {
        if let Some(pid) = self.child.id() {
            // SIGINT lets pw-record drain its buffer; kill() would truncate
            // the tail of the utterance.
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGINT);
            }
            #[cfg(not(unix))]
            self.child.kill().await?;
        }
        let mut tail = Vec::new();
        self.stdout.read_to_end(&mut tail).await?;
        self.pending.extend_from_slice(&tail);
        let status = self.child.wait().await?;
        // A failed child that produced nothing is simply "no tail"; one
        // that produced audio still gets its bytes delivered.
        if !status.success() && self.pending.is_empty() {
            return Ok(None);
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
}

/// Streaming WAV recorder wrapping a capture boundary: every chunk that
/// passes through the coordinator is mirrored to a PCM16 WAV file in a
/// single write pass. The mirror is strictly best-effort — any filesystem
/// failure disables saving for the rest of the session and can never fail
/// recognition, injection or history. Cancelled or failed sessions leave
/// no file behind; a successful session finalizes the header and renames
/// the partial file to the destination.
///
/// The file is finalized only through `finish_recording`, which the
/// coordinator calls on the success path with recognized text; every
/// other outcome (cancel, failure, no speech, mirror error) ends in
/// `disable`, directly or from `Drop`, which deletes the partial file.
pub struct WavRecorder<C> {
    inner: C,
    sample_rate: u32,
    /// `None` when saving is disabled, has failed, or is finished.
    state: Option<RecorderState>,
}

/// The open partial file plus what is needed to finalize or discard it.
struct RecorderState {
    file: std::fs::File,
    /// `<final_path>.partial`; removed on every outcome but success.
    temp_path: std::path::PathBuf,
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
            let temp_path = final_path.with_extension("wav.partial");
            let file = std::fs::File::create(&temp_path).ok()?;
            Some(RecorderState {
                file,
                temp_path,
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
        if state.file.write_all(pcm).is_err() {
            self.disable();
        } else {
            state.data_bytes += pcm.len() as u64;
        }
    }

    /// Stop mirroring for the rest of the session and delete the partial
    /// file. Idempotent.
    fn disable(&mut self) {
        if let Some(state) = self.state.take() {
            drop(state.file);
            let _ = std::fs::remove_file(&state.temp_path);
        }
    }

    /// Patch the header and rename the partial file into place. Returns
    /// the final path only when the recording finished with at least one
    /// aligned sample and both the rewrite and rename succeeded.
    fn finish(&mut self) -> Option<std::path::PathBuf> {
        let mut state = self.state.take()?;
        if state.data_bytes == 0 || state.data_bytes % 2 != 0 {
            let _ = std::fs::remove_file(&state.temp_path);
            return None;
        }
        let finalized = state.file.seek(io::SeekFrom::Start(0)).is_ok()
            && state
                .file
                .write_all(&wav_header(state.data_bytes, self.sample_rate))
                .is_ok();
        drop(state.file);
        if finalized && std::fs::rename(&state.temp_path, &state.final_path).is_ok() {
            Some(state.final_path)
        } else {
            let _ = std::fs::remove_file(&state.temp_path);
            None
        }
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

    fn abort(&mut self) {
        // Remove the partial file before aborting the inner capture so a
        // cancelled session never leaks audio on disk.
        self.disable();
        self.inner.abort();
    }

    fn finish_recording(&mut self) -> Option<std::path::PathBuf> {
        self.finish()
    }
}

impl<C> Drop for WavRecorder<C> {
    /// A recorder that was not finalized (cancel, failure, no speech)
    /// deletes its partial file.
    fn drop(&mut self) {
        self.disable();
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
