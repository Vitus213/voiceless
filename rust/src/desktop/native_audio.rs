//! Capture from the native host device. The host stream stays on the thread
//! that opened it. The audio callback publishes bounded mono PCM16 frames.

use std::io;
use std::str::FromStr;
use std::sync::{mpsc as control, Arc};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, DeviceId, FromSample, Sample, SizedSample};
use parking_lot::Mutex;
use serde::Serialize;
use sherpa_onnx::LinearResampler;
use tokio::sync::{mpsc, oneshot, watch};

use crate::capture::{CHUNK_BYTES, SAMPLE_RATE};

static CAPTURING: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AudioDevices {
    pub microphones: Vec<Microphone>,
    pub system_default: Option<String>,
    pub capturing: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Microphone {
    pub id: String,
    pub label: String,
}

pub(crate) async fn audio_devices() -> Result<AudioDevices, String> {
    tokio::task::spawn_blocking(query_devices)
        .await
        .map_err(|error| error.to_string())?
}

pub(crate) fn query_devices() -> Result<AudioDevices, String> {
    let host = cpal::default_host();
    let system_default = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    #[cfg(target_os = "windows")]
    let inputs = host.input_devices().map_err(|error| error.to_string())?;
    // CPAL's CoreAudio input_devices and description probe stream formats by
    // creating an AudioUnit. Settings must only read device properties.
    #[cfg(target_os = "macos")]
    let inputs = host.devices().map_err(|error| error.to_string())?;
    let mut microphones: Vec<_> = inputs
        .filter_map(|device| {
            let device_id = device.id().ok()?;
            let id = device_id.to_string();
            #[cfg(target_os = "macos")]
            let label = crate::desktop::native::input_label(&device_id.1)?;
            #[cfg(target_os = "windows")]
            let label = device
                .description()
                .map_or_else(|_| id.clone(), |info| info.name().to_owned());
            Some(Microphone { id, label })
        })
        .collect();
    microphones.sort_unstable_by(|left, right| left.label.cmp(&right.label));
    Ok(AudioDevices {
        microphones,
        system_default,
        capturing: CAPTURING.lock().clone(),
    })
}

pub(crate) async fn require_source(source: &str) -> Result<DeviceId, String> {
    let id = DeviceId::from_str(source)
        .map_err(|error| format!("invalid microphone id {source}: {error}"))?;
    let devices = audio_devices().await?;
    devices
        .microphones
        .iter()
        .any(|device| device.id == source)
        .then_some(id)
        .ok_or_else(|| {
            format!("microphone {source} is unavailable; reconnect it or select another one")
        })
}

pub struct RawCapture {
    control: Option<control::Sender<std::fs::File>>,
    done: Option<oneshot::Receiver<Vec<u8>>>,
    frames: mpsc::Receiver<Vec<u8>>,
    health: watch::Receiver<Option<String>>,
}

/// A running device that discards its callbacks until the owner commits to
/// recording. Its worker owns no capture lease while macOS waits for consent.
pub(crate) struct PreparedCapture {
    activate: control::Sender<std::fs::File>,
    capture: RawCapture,
}

impl PreparedCapture {
    pub(crate) fn open(target: Option<DeviceId>, system: bool) -> io::Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(5);
        #[cfg(target_os = "macos")]
        let pending = system
            .then(crate::desktop::native::SystemAudioStart::acquire)
            .transpose()?;
        #[cfg(target_os = "macos")]
        let deadline = pending.as_ref().map_or(deadline, |guard| guard.deadline);
        let (activate, commands) = control::channel();
        let (started, ready) = control::sync_channel(1);
        let (finished, done) = oneshot::channel();
        let (samples, frames) = mpsc::channel(16);
        let (errors, health) = watch::channel(None);
        std::thread::Builder::new()
            .name("voiceless-input".to_owned())
            .spawn(move || {
                let result = open_stream(target, system, samples, &errors);
                #[cfg(target_os = "macos")]
                drop(pending);
                let capture = match result {
                    Ok(capture) => capture,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                if started.send(Ok(())).is_err() {
                    return;
                }
                let Ok(lease) = commands.recv() else { return };
                if !system {
                    *CAPTURING.lock() = capture.microphone_id;
                }
                capture.encoder.lock().recording = true;
                // The owner drops the sole sender to stop the device.
                #[cfg(target_os = "macos")]
                if let Some(tap) = &capture.tap {
                    // Core Audio can keep the private aggregate streaming silence after
                    // the selected output vanishes. Check that output even on a normal stop.
                    loop {
                        let stop = commands.recv_timeout(Duration::from_secs(1));
                        if !tap.output_alive() {
                            errors.send_replace(Some(
                                "the selected system output disconnected".into(),
                            ));
                            break;
                        }
                        if !matches!(stop, Err(control::RecvTimeoutError::Timeout)) {
                            break;
                        }
                    }
                } else {
                    let _ = commands.recv();
                }
                #[cfg(windows)]
                let _ = commands.recv();
                drop(capture.stream);
                #[cfg(target_os = "macos")]
                drop(capture.tap);
                if !system {
                    *CAPTURING.lock() = None;
                }
                let tail = capture.encoder.lock().flush();
                drop(lease);
                let _ = finished.send(tail);
            })?;
        match ready.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result?,
            Err(control::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("the audio startup worker exited"));
            }
            Err(control::RecvTimeoutError::Timeout) => {
                #[cfg(target_os = "macos")]
                if system {
                    return Err(crate::desktop::native::unanswered());
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the selected audio device did not start",
                ));
            }
        }
        Ok(Self {
            activate,
            capture: RawCapture {
                control: None,
                done: Some(done),
                frames,
                health,
            },
        })
    }

    pub(crate) fn record(mut self, lease: std::fs::File) -> io::Result<RawCapture> {
        self.activate
            .send(lease)
            .map_err(|_| io::Error::other("the audio device closed before recording"))?;
        self.capture.control = Some(self.activate);
        Ok(self.capture)
    }
}

impl RawCapture {
    pub fn start(target: Option<DeviceId>) -> io::Result<Self> {
        let lease = crate::runtime::lock_capture()?;
        PreparedCapture::open(target, false)?.record(lease)
    }

    pub async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if let Some(error) = self.health.borrow().as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        tokio::select! {
            biased;
            _ = self.health.changed() => Err(io::Error::other(self.health.borrow().clone().unwrap_or_else(|| "the microphone disconnected".to_owned()))),
            frame = self.frames.recv() => frame.map(Some).ok_or_else(|| io::Error::other("the microphone disconnected")),
        }
    }

    pub async fn stop(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.request_stop();
        let mut tail = Vec::new();
        if let Some(done) = self.done.take() {
            let remaining = done
                .await
                .map_err(|_| io::Error::other("the microphone failed to stop"))?;
            while let Ok(frame) = self.frames.try_recv() {
                tail.extend_from_slice(&frame);
            }
            tail.extend(remaining);
        }
        if let Some(error) = self.health.borrow().as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        Ok((!tail.is_empty()).then_some(tail))
    }

    pub(crate) async fn abort(&mut self) {
        self.request_stop();
        if let Some(done) = self.done.take() {
            let _ = done.await;
        }
    }

    fn request_stop(&mut self) {
        drop(self.control.take());
    }
}

impl Drop for RawCapture {
    fn drop(&mut self) {
        self.request_stop();
    }
}

struct Capture {
    stream: cpal::Stream,
    #[cfg(target_os = "macos")]
    tap: Option<crate::desktop::native::MeetingTap>,
    encoder: Arc<Mutex<Encoder>>,
    microphone_id: Option<String>,
}

fn open_stream(
    target: Option<DeviceId>,
    system: bool,
    frames: mpsc::Sender<Vec<u8>>,
    errors: &watch::Sender<Option<String>>,
) -> io::Result<Capture> {
    let host = cpal::default_host();
    #[cfg(target_os = "macos")]
    let tap = if system {
        let output = target
            .as_ref()
            .ok_or_else(|| io::Error::other("a system recording needs a selected output"))?;
        Some(crate::desktop::native::MeetingTap::open(output)?)
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let tapped_device = tap.as_ref().map(|tap| tap.device());
    #[cfg(windows)]
    let tapped_device: Option<Device> = None;
    let device = tapped_device
        .or_else(|| match target.as_ref() {
            Some(id) => host.device_by_id(id),
            None => host.default_input_device(),
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the selected audio device is unavailable",
            )
        })?;
    let supported = if cfg!(windows) && system {
        device.default_output_config()
    } else {
        device.default_input_config()
    }
    .map_err(io::Error::other)?;
    let config = supported.config();
    let encoder = Arc::new(Mutex::new(Encoder::new(
        config.channels,
        config.sample_rate,
    )?));
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => input::<f32>(&device, &config, &encoder, frames, errors),
        cpal::SampleFormat::F64 => input::<f64>(&device, &config, &encoder, frames, errors),
        cpal::SampleFormat::I16 => input::<i16>(&device, &config, &encoder, frames, errors),
        cpal::SampleFormat::I24 => input::<cpal::I24>(&device, &config, &encoder, frames, errors),
        cpal::SampleFormat::I32 => input::<i32>(&device, &config, &encoder, frames, errors),
        cpal::SampleFormat::U16 => input::<u16>(&device, &config, &encoder, frames, errors),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported microphone sample format {other}"),
            ))
        }
    }
    .map_err(io::Error::other)?;
    let microphone_id = if system {
        None
    } else {
        Some(device.id().map_err(io::Error::other)?.to_string())
    };
    stream.play().map_err(io::Error::other)?;
    Ok(Capture {
        stream,
        #[cfg(target_os = "macos")]
        tap,
        encoder,
        microphone_id,
    })
}

fn input<T: SizedSample>(
    device: &Device,
    config: &cpal::StreamConfig,
    encoder: &Arc<Mutex<Encoder>>,
    frames: mpsc::Sender<Vec<u8>>,
    errors: &watch::Sender<Option<String>>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: FromSample<T>,
{
    let encoder = encoder.clone();
    let backend_error = errors.clone();
    let frame_error = errors.clone();
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            if frame_error.borrow().is_none() && !encoder.lock().accept(data, &frames) {
                frame_error.send_replace(Some(
                    "audio input did not keep up with the capture queue".to_owned(),
                ));
            }
        },
        move |error| {
            backend_error.send_replace(Some(format!("audio input unavailable: {error}")));
        },
        None,
    )
}

struct Encoder {
    recording: bool,
    channels: usize,
    convert: Option<LinearResampler>,
    mono: Vec<f32>,
    pcm: Vec<u8>,
}

impl Encoder {
    fn new(channels: u16, rate: u32) -> io::Result<Self> {
        if channels == 0 || rate == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the microphone reported an invalid audio format",
            ));
        }
        let convert = if rate == SAMPLE_RATE {
            None
        } else {
            Some(
                LinearResampler::create(
                    rate.try_into().map_err(io::Error::other)?,
                    SAMPLE_RATE as i32,
                )
                .ok_or_else(|| io::Error::other("cannot resample microphone input"))?,
            )
        };
        Ok(Self {
            recording: false,
            channels: usize::from(channels),
            convert,
            mono: Vec::new(),
            pcm: Vec::with_capacity(CHUNK_BYTES),
        })
    }

    fn accept<T: Sample>(&mut self, data: &[T], frames: &mpsc::Sender<Vec<u8>>) -> bool
    where
        f32: FromSample<T>,
    {
        if !self.recording {
            return true;
        }
        self.mono.clear();
        let scale = 1.0 / self.channels as f32;
        self.mono
            .extend(data.chunks_exact(self.channels).map(|frame| {
                frame
                    .iter()
                    .map(|sample| sample.to_sample::<f32>())
                    .sum::<f32>()
                    * scale
            }));
        let converted;
        let mono = if let Some(convert) = &self.convert {
            converted = convert.resample(&self.mono, false);
            converted.as_slice()
        } else {
            self.mono.as_slice()
        };
        for sample in mono {
            self.pcm.extend_from_slice(&pcm16(*sample));
            if self.pcm.len() == CHUNK_BYTES {
                let full = std::mem::replace(&mut self.pcm, Vec::with_capacity(CHUNK_BYTES));
                if frames.try_send(full).is_err() {
                    return false;
                }
            }
        }
        true
    }

    fn flush(&mut self) -> Vec<u8> {
        if let Some(convert) = &self.convert {
            for sample in convert.resample(&[], true) {
                self.pcm.extend_from_slice(&pcm16(sample));
            }
        }
        std::mem::take(&mut self.pcm)
    }
}

fn pcm16(value: f32) -> [u8; 2] {
    ((value.clamp(-1.0, 1.0) * 32768.0) as i16).to_le_bytes()
}
