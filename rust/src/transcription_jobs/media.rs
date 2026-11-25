//! Media of a job: the decoder that reads a file, the bounded stream of
//! 16 kHz mono samples it becomes, and the reasons a file cannot be one.
//!
//! Which decoder reads a file is decided by its content. A RIFF/WAVE file
//! goes through the native reader here: PCM and IEEE float samples of any
//! width, any channel count and any rate, mixed to mono and resampled to
//! 16 kHz while reading. Everything else, and WAV files that hold
//! compressed audio, is decoded by FFmpeg, which is found on `PATH` when
//! it is installed. Without FFmpeg only uncompressed WAV can be imported.
//!
//! Both readers stream: one block of encoded audio and one block of
//! decoded samples are in memory at a time, however long the file is.
//! FFmpeg runs as a child that writes raw `f32le` samples to a pipe; it is
//! killed and reaped when the stream is dropped, and told to die with the
//! runtime, so a crash leaves no decoder behind.
//!
//! A file is measured when it is imported: FFmpeg media are decoded once
//! to the end to count their samples, which also proves that the whole
//! file decodes before any of it is recognized. For WAV the count follows
//! from the header. All durations the queue reports are these counts.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::mem::size_of;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};
use sherpa_onnx::LinearResampler;

use super::job::{FailureCode, JobFailure, SAMPLE_RATE};
use super::recognize::MAX_MEDIA_SAMPLES;

/// Formats this build decodes on its own.
pub const NATIVE_FORMATS: &str = "WAV with uncompressed PCM or IEEE float samples";
/// Formats FFmpeg decodes once it is installed.
pub const FFMPEG_FORMATS: &str = "FLAC, MP3, M4A/AAC, Ogg Vorbis and Opus, WMA, AIFF, MP4, MKV, WebM, MOV and the other containers FFmpeg knows";

const NO_FFMPEG: &str = "FFmpeg was not found, so only uncompressed WAV can be decoded";
/// Encoded bytes one native read block holds.
const RAW_BLOCK_BYTES: usize = 256 * 1024;
/// Decoded samples the probe counts by.
const COUNT_SAMPLES: usize = 16_000;

/// What decodes a file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MediaDecoder {
    /// The native RIFF/WAVE reader.
    #[default]
    Wav,
    /// The FFmpeg child.
    Ffmpeg,
}

impl MediaDecoder {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Ffmpeg => "ffmpeg",
        }
    }
}

/// The FFmpeg executable a queue uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Ffmpeg {
    /// `ffmpeg` from `PATH`, looked up each time a file is decoded so
    /// that installing it later is picked up.
    #[default]
    Discover,
    /// One fixed executable.
    At(PathBuf),
}

/// Options of a queue that its owner sets once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueOptions {
    pub ffmpeg: Ffmpeg,
}

/// What the queue can import, for the page to say before a file is chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaSupport {
    /// The FFmpeg that decodes [`FFMPEG_FORMATS`]; `None` when only
    /// [`NATIVE_FORMATS`] can be imported.
    pub ffmpeg: Option<PathBuf>,
}

/// What a queue opened with `options` can import.
pub fn media_support(options: &QueueOptions) -> MediaSupport {
    MediaSupport {
        ffmpeg: locate(&options.ffmpeg),
    }
}

pub(crate) fn locate(ffmpeg: &Ffmpeg) -> Option<PathBuf> {
    match ffmpeg {
        Ffmpeg::At(path) => Some(path.clone()),
        Ffmpeg::Discover => std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)))
                .find(|path| is_executable(path))
        }),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Why media cannot be decoded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MediaError {
    #[error("the file is empty")]
    Empty,
    /// No decoder here can read this; the string says which is missing.
    #[error("{0}")]
    Unsupported(String),
    #[error("the media is too long for the voice activity detector")]
    TooLong,
    #[error("the file has no audio track")]
    NoAudio,
    #[error("the media is damaged after {seconds:.1} s of audio: {detail}")]
    Damaged { seconds: f64, detail: String },
    #[error("{0}")]
    Read(#[from] io::Error),
}

/// The error state of a job whose media, the queue's own copy at `owned`,
/// could not be decoded.
pub(crate) fn failure(error: MediaError, owned: &Path) -> JobFailure {
    match error {
        MediaError::Empty => FailureCode::Corrupt.failure("the file is empty"),
        MediaError::Unsupported(reason) => FailureCode::Unsupported.failure(reason),
        MediaError::TooLong => super::recognize::too_long(),
        MediaError::NoAudio => FailureCode::NoAudio.failure("the file has no audio track"),
        MediaError::Damaged { seconds, detail } => FailureCode::Corrupt.failure(format!(
            "the media is damaged after {seconds:.1} s of audio: {detail}"
        )),
        MediaError::Read(source) => match source.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => {
                FailureCode::SourceUnavailable.failure(format!(
                    "the queue's copy of the media cannot be read: {}: {source}",
                    owned.display()
                ))
            }
            _ => {
                FailureCode::Internal.failure(format!("cannot read {}: {source}", owned.display()))
            }
        },
    }
}

/// What probing a file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Media {
    pub decoder: MediaDecoder,
    /// Samples of the 16 kHz stream the whole file decodes to.
    pub duration_samples: u64,
}

/// Read the header of a file and tell which decoder reads it. `None` for
/// an empty file.
pub(crate) fn detect(path: &Path) -> Result<Option<MediaDecoder>, MediaError> {
    let mut file = File::open(path).map_err(MediaError::Read)?;
    let mut header = [0_u8; 12];
    let read = read_up_to(&mut file, &mut header).map_err(MediaError::Read)?;
    if read == 0 {
        return Ok(None);
    }
    // Decided by content, not by the file name: a renamed file still decodes.
    let wav = read == header.len() && &header[..4] == b"RIFF" && &header[8..12] == b"WAVE";
    Ok(Some(if wav {
        MediaDecoder::Wav
    } else {
        MediaDecoder::Ffmpeg
    }))
}

/// Find out what decodes `path` and how long its audio is. A file that
/// cannot be decoded to the end is refused here, before any of it is
/// recognized. Blocking: FFmpeg media are decoded once in full.
pub(crate) fn probe(path: &Path, ffmpeg: &Ffmpeg) -> Result<Media, MediaError> {
    let media = match detect(path)? {
        None => return Err(MediaError::Empty),
        Some(MediaDecoder::Wav) => match wav_media(path) {
            Ok(media) => media,
            // Compressed audio in a WAV container is FFmpeg's.
            Err(MediaError::Unsupported(reason)) => ffmpeg_media(path, ffmpeg, reason)?,
            Err(error) => return Err(error),
        },
        Some(MediaDecoder::Ffmpeg) => ffmpeg_media(path, ffmpeg, NO_FFMPEG.to_owned())?,
    };
    if media.duration_samples >= MAX_MEDIA_SAMPLES {
        return Err(MediaError::TooLong);
    }
    Ok(media)
}

/// A bounded stream of 16 kHz mono samples in [-1, 1].
pub(crate) trait PcmSource: Send {
    /// Decode the next samples into `out`, at most `out.len()` of them.
    /// `Ok(0)` means the end of the media, and stays that way. `out` must
    /// not be empty.
    fn read(&mut self, out: &mut [f32]) -> Result<usize, MediaError>;
}

/// Start reading `path` with `decoder` at 16 kHz sample `skip`.
pub(crate) fn open_stream(
    path: &Path,
    decoder: MediaDecoder,
    skip: u64,
    ffmpeg: &Ffmpeg,
) -> Result<Box<dyn PcmSource>, MediaError> {
    match decoder {
        MediaDecoder::Wav => Ok(Box::new(WavSource::open(path, skip)?)),
        MediaDecoder::Ffmpeg => {
            let program =
                locate(ffmpeg).ok_or_else(|| MediaError::Unsupported(NO_FFMPEG.to_owned()))?;
            Ok(Box::new(FfmpegSource::spawn(&program, path, skip)?))
        }
    }
}

fn wav_media(path: &Path) -> Result<Media, MediaError> {
    let file = File::open(path).map_err(MediaError::Read)?;
    let file_len = file.metadata().map_err(MediaError::Read)?.len();
    let mut reader = BufReader::new(file);
    let header = WavHeader::read(&mut reader, file_len)?;
    let frames = header.data_bytes / header.frame_bytes;
    let rate = u128::from(header.rate);
    let duration_samples = ((frames as u128 * u128::from(SAMPLE_RATE) + rate / 2) / rate) as u64;
    Ok(Media {
        decoder: MediaDecoder::Wav,
        duration_samples,
    })
}

/// Decode all of `path` with FFmpeg and count the samples. `reason` names
/// what FFmpeg is needed for.
fn ffmpeg_media(path: &Path, ffmpeg: &Ffmpeg, reason: String) -> Result<Media, MediaError> {
    let program =
        locate(ffmpeg).ok_or_else(|| MediaError::Unsupported(format!("{reason}: {NO_FFMPEG}")))?;
    let mut source = FfmpegSource::spawn(&program, path, 0)?;
    let mut buffer = vec![0.0_f32; COUNT_SAMPLES];
    let mut decoded = 0_u64;
    loop {
        if decoded >= MAX_MEDIA_SAMPLES {
            return Err(MediaError::TooLong);
        }
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        decoded += count as u64;
    }
    Ok(Media {
        decoder: MediaDecoder::Ffmpeg,
        duration_samples: decoded,
    })
}

/// Read into `buffer` until it is full or the reader ends.
fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            // A signal interrupted the call; the read is simply retried.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleFormat {
    Pcm8,
    Pcm16,
    Pcm24,
    Pcm32,
    Float32,
    Float64,
}

impl SampleFormat {
    /// The format of RIFF tag `tag` with `bits` bits per sample; `None`
    /// for anything compressed.
    fn of(tag: u16, bits: u16) -> Option<Self> {
        match (tag, bits) {
            (1, 8) => Some(Self::Pcm8),
            (1, 16) => Some(Self::Pcm16),
            (1, 24) => Some(Self::Pcm24),
            (1, 32) => Some(Self::Pcm32),
            (3, 32) => Some(Self::Float32),
            (3, 64) => Some(Self::Float64),
            _ => None,
        }
    }

    fn bytes(self) -> u64 {
        match self {
            Self::Pcm8 => 1,
            Self::Pcm16 => 2,
            Self::Pcm24 => 3,
            Self::Pcm32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }

    /// The sample encoded in the first bytes of `bytes`.
    fn sample(self, bytes: &[u8]) -> f32 {
        match self {
            Self::Pcm8 => f32::from(bytes[0]) / 128.0 - 1.0,
            Self::Pcm16 => f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32_768.0,
            Self::Pcm24 => {
                (i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8) as f32 / 8_388_608.0
            }
            Self::Pcm32 => {
                i32::from_le_bytes(bytes[..4].try_into().unwrap()) as f32 / 2_147_483_648.0
            }
            Self::Float32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            Self::Float64 => f64::from_le_bytes(bytes[..8].try_into().unwrap()) as f32,
        }
    }
}

/// Where the samples of an uncompressed WAV are and how they are encoded.
struct WavHeader {
    format: SampleFormat,
    channels: u16,
    rate: u32,
    /// Bytes of one sample of every channel.
    frame_bytes: u64,
    data_offset: u64,
    data_bytes: u64,
}

impl WavHeader {
    /// Walk the chunk list of a RIFF/WAVE file of `file_len` bytes up to
    /// its `data` chunk. Extra chunks (`LIST`, `fact`) are skipped.
    fn read(reader: &mut (impl BufRead + Seek), file_len: u64) -> Result<Self, MediaError> {
        let damaged = |detail: String| MediaError::Damaged {
            seconds: 0.0,
            detail: format!("the WAV file is invalid: {detail}"),
        };
        let io_error = |error: io::Error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                damaged("the file ends before its chunks are complete".to_owned())
            } else {
                MediaError::Read(error)
            }
        };
        let mut riff = [0_u8; 12];
        reader.read_exact(&mut riff).map_err(io_error)?;
        if &riff[..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
            return Err(damaged("it does not start with RIFF and WAVE".to_owned()));
        }
        let riff_size = u32::from_le_bytes(riff[4..8].try_into().unwrap());
        // RIFF tag, channels, sample rate and bits per sample of the fmt chunk.
        let mut fmt: Option<(u16, u16, u32, u16)> = None;
        let mut offset = 12_u64;
        loop {
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(MediaError::Read)?;
            let mut chunk = [0_u8; 8];
            reader.read_exact(&mut chunk).map_err(io_error)?;
            let size = u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
            let body = offset + 8;
            match &chunk[..4] {
                b"fmt " => {
                    if size < 16 {
                        return Err(damaged("the fmt chunk is shorter than 16 bytes".to_owned()));
                    }
                    // WAVE_FORMAT_EXTENSIBLE carries its details up to byte 40.
                    let length = size.min(40) as usize;
                    let mut fields = [0_u8; 40];
                    reader.read_exact(&mut fields[..length]).map_err(io_error)?;
                    let mut tag = u16::from_le_bytes([fields[0], fields[1]]);
                    // Its subformat GUID starts with the tag it stands for.
                    if tag == 0xFFFE && length >= 26 {
                        tag = u16::from_le_bytes([fields[24], fields[25]]);
                    }
                    fmt = Some((
                        tag,
                        u16::from_le_bytes([fields[2], fields[3]]),
                        u32::from_le_bytes([fields[4], fields[5], fields[6], fields[7]]),
                        u16::from_le_bytes([fields[14], fields[15]]),
                    ));
                }
                b"data" => {
                    let (tag, channels, rate, bits) = fmt.ok_or_else(|| {
                        damaged("the data chunk comes before the fmt chunk".to_owned())
                    })?;
                    let format = SampleFormat::of(tag, bits).ok_or_else(|| {
                        MediaError::Unsupported(format!(
                            "the WAV file holds compressed audio (format {tag}, {bits}-bit)"
                        ))
                    })?;
                    if channels == 0 || rate == 0 {
                        return Err(damaged(
                            "the fmt chunk names no channel or no sample rate".to_owned(),
                        ));
                    }
                    let available = file_len.saturating_sub(body);
                    // Some streaming encoders leave data length unspecified.
                    // DashScope TTS uses the same two placeholder lengths together.
                    let data_bytes = match (riff_size, size) {
                        (_, 0 | 0xffff_ffff) => available,
                        (0x7fff_ffbf, 0x7fff_ff9b) if size > available => available,
                        _ => size,
                    };
                    let frame_bytes = u64::from(channels) * format.bytes();
                    if data_bytes > available || data_bytes % frame_bytes != 0 {
                        return Err(damaged(
                            "the data chunk ends before its PCM frames are complete".to_owned(),
                        ));
                    }
                    return Ok(Self {
                        format,
                        channels,
                        rate,
                        frame_bytes,
                        data_offset: body,
                        data_bytes,
                    });
                }
                _ => {}
            }
            // Chunks are padded to an even size.
            offset = body + size + (size & 1);
            if offset >= file_len {
                return Err(damaged("it has no data chunk".to_owned()));
            }
        }
    }
}

/// An uncompressed WAV read block by block. Channels are mixed to mono
/// and the rate converted to 16 kHz as the blocks arrive.
pub(crate) struct WavSource {
    reader: BufReader<File>,
    header: WavHeader,
    /// Bytes of the data chunk not read yet.
    remaining: u64,
    resampler: Option<LinearResampler>,
    /// One block of encoded frames.
    raw: Vec<u8>,
    /// The block as mono samples at the file's rate.
    mono: Vec<f32>,
    /// Decoded samples not handed out yet.
    ready: Vec<f32>,
    flushed: bool,
}

impl WavSource {
    pub(crate) fn open(path: &Path, skip: u64) -> Result<Self, MediaError> {
        let file = File::open(path).map_err(MediaError::Read)?;
        let file_len = file.metadata().map_err(MediaError::Read)?.len();
        let mut reader = BufReader::new(file);
        let header = WavHeader::read(&mut reader, file_len)?;
        // A resumed run starts at a 16 kHz sample: in the file that is the
        // frame of the original rate that contains it.
        let frames = header.data_bytes / header.frame_bytes;
        let skip_frames = ((skip as u128 * u128::from(header.rate)) / u128::from(SAMPLE_RATE))
            .min(frames as u128) as u64;
        let skipped = skip_frames * header.frame_bytes;
        reader
            .seek(SeekFrom::Start(header.data_offset + skipped))
            .map_err(MediaError::Read)?;
        let resampler = if header.rate == SAMPLE_RATE {
            None
        } else {
            Some(
                LinearResampler::create(header.rate as i32, SAMPLE_RATE as i32).ok_or_else(
                    || {
                        MediaError::Unsupported(format!(
                            "{} Hz audio cannot be resampled to 16 kHz",
                            header.rate
                        ))
                    },
                )?,
            )
        };
        let frame = header.frame_bytes as usize;
        Ok(Self {
            reader,
            remaining: header.data_bytes - skipped,
            header,
            resampler,
            raw: vec![0_u8; (RAW_BLOCK_BYTES / frame).max(1) * frame],
            mono: Vec::new(),
            ready: Vec::new(),
            flushed: false,
        })
    }

    /// Decode one block of the file into `ready`.
    fn fill(&mut self) -> Result<(), MediaError> {
        let frame = self.header.frame_bytes as usize;
        let want = (self.remaining.min(self.raw.len() as u64) as usize) / frame * frame;
        let got = read_up_to(&mut self.reader, &mut self.raw[..want]).map_err(MediaError::Read)?;
        if got != want {
            let frames = (self.header.data_bytes - self.remaining) / self.header.frame_bytes;
            return Err(MediaError::Damaged {
                seconds: frames as f64 / f64::from(self.header.rate),
                detail: "the WAV file ended while it was being decoded".to_owned(),
            });
        }
        self.remaining -= got as u64;
        let channels = usize::from(self.header.channels);
        self.mono.clear();
        for samples in self.raw[..got].chunks_exact(frame) {
            let mut mixed = 0.0_f32;
            for sample in samples.chunks_exact(frame / channels) {
                mixed += self.header.format.sample(sample);
            }
            self.mono.push(mixed / channels as f32);
        }
        match &self.resampler {
            Some(resampler) => self
                .ready
                .extend_from_slice(&resampler.resample(&self.mono, false)),
            None => self.ready.extend_from_slice(&self.mono),
        }
        Ok(())
    }
}

impl PcmSource for WavSource {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, MediaError> {
        while self.ready.is_empty() && !self.flushed {
            if self.remaining == 0 {
                // End of the data: let the resampler produce what it still holds.
                self.flushed = true;
                if let Some(resampler) = &self.resampler {
                    let tail = resampler.resample(&[], true);
                    self.ready.extend_from_slice(&tail);
                }
            } else {
                self.fill()?;
            }
        }
        let count = self.ready.len().min(out.len());
        out[..count].copy_from_slice(&self.ready[..count]);
        self.ready.drain(..count);
        Ok(count)
    }
}

/// FFmpeg decoding one file into 16 kHz mono `f32le` samples on a pipe.
/// Dropping it kills FFmpeg and reaps it, whatever ended the read.
struct FfmpegSource {
    child: Child,
    stdout: std::process::ChildStdout,
    /// Collects FFmpeg's diagnostics; returns its last error line.
    diagnostics: Option<JoinHandle<Option<String>>>,
    /// Raw bytes of one read, at most one sample too many to carry a cut one over.
    bytes: Vec<u8>,
    /// Leading bytes at `bytes` of a sample a read cut in half.
    leftover: usize,
    /// Samples handed out so far.
    decoded: u64,
    finished: bool,
}

impl FfmpegSource {
    /// Run FFmpeg on `input`, starting at 16 kHz sample `skip`.
    fn spawn(program: &Path, input: &Path, skip: u64) -> Result<Self, MediaError> {
        let mut command = Command::new(program);
        command.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
        if skip > 0 {
            // Sought before the input, FFmpeg decodes from the previous
            // seek point and drops what precedes the position, so the
            // stream starts there.
            command
                .arg("-ss")
                .arg(format!("{:.7}", skip as f64 / f64::from(SAMPLE_RATE)));
        }
        command
            .arg("-i")
            .arg(input)
            // The first audio track only: no video, no subtitles.
            .args([
                "-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "1", "-ar", "16000", "-f", "f32le",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "linux")]
        // SAFETY: `pre_exec` runs in the child between `fork` and `exec`.
        // `prctl` is a system call, which is safe to make there, and the
        // closure allocates nothing.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let unavailable = |error: io::Error| {
            MediaError::Unsupported(format!(
                "FFmpeg ({}) could not be started: {error}",
                program.display()
            ))
        };
        let mut child = command.spawn().map_err(unavailable)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| unavailable(io::Error::other("it opened no output")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| unavailable(io::Error::other("it opened no diagnostics")))?;
        let diagnostics = std::thread::Builder::new()
            .name("voiceless-ffmpeg-stderr".to_owned())
            .spawn(move || last_error_line(stderr));
        let diagnostics = match diagnostics {
            Ok(diagnostics) => diagnostics,
            Err(error) => {
                // Nobody would drain the pipe: do not leave the child running.
                let _ = child.kill();
                let _ = child.wait();
                return Err(unavailable(error));
            }
        };
        Ok(Self {
            child,
            stdout,
            diagnostics: Some(diagnostics),
            bytes: Vec::new(),
            leftover: 0,
            decoded: 0,
            finished: false,
        })
    }

    /// How FFmpeg's exit stops the stream.
    fn exited(&self, status: &ExitStatus, detail: &str) -> MediaError {
        let detail = if detail.is_empty() {
            format!("FFmpeg stopped with {status}")
        } else {
            format!("FFmpeg stopped with {status}: {detail}")
        };
        if self.decoded != 0 {
            MediaError::Damaged {
                seconds: self.decoded as f64 / f64::from(SAMPLE_RATE),
                detail,
            }
        } else if detail.contains("matches no streams") {
            MediaError::NoAudio
        } else {
            MediaError::Unsupported(format!("FFmpeg cannot decode the file as audio: {detail}"))
        }
    }

    /// The pipe ended: reap FFmpeg and turn how it exited into the end of
    /// the stream or its failure.
    fn finish(&mut self) -> Result<usize, MediaError> {
        self.finished = true;
        let status = self.child.wait()?;
        let detail = self
            .diagnostics
            .take()
            .and_then(|diagnostics| diagnostics.join().ok())
            .flatten()
            .unwrap_or_default();
        if !status.success() {
            return Err(self.exited(&status, &detail));
        }
        if self.leftover != 0 {
            return Err(MediaError::Damaged {
                seconds: self.decoded as f64 / f64::from(SAMPLE_RATE),
                detail: "the decoded stream ends with an incomplete sample".to_owned(),
            });
        }
        Ok(0)
    }
}

impl Drop for FfmpegSource {
    fn drop(&mut self) {
        // Killed first, so waiting cannot block on a full pipe.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(diagnostics) = self.diagnostics.take() {
            let _ = diagnostics.join();
        }
    }
}

impl PcmSource for FfmpegSource {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, MediaError> {
        if self.finished || out.is_empty() {
            return Ok(0);
        }
        let sample = size_of::<f32>();
        let capacity = std::mem::size_of_val(out);
        if self.bytes.len() != capacity {
            self.bytes.resize(capacity, 0);
        }
        let read = self.stdout.read(&mut self.bytes[self.leftover..])?;
        let available = self.leftover + read;
        let whole = available / sample * sample;
        for (slot, bytes) in out.iter_mut().zip(self.bytes[..whole].chunks_exact(sample)) {
            *slot = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        self.leftover = available - whole;
        self.bytes.copy_within(whole..available, 0);
        let samples = whole / sample;
        self.decoded += samples as u64;
        if samples == 0 {
            return self.finish();
        }
        Ok(samples)
    }
}

/// The last non-empty line FFmpeg wrote: the most specific reason it
/// stopped.
fn last_error_line(stderr: ChildStderr) -> Option<String> {
    let mut last = None;
    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if !line.is_empty() {
            last = Some(line.to_owned());
        }
    }
    last
}
