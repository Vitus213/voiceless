//! CPAL's CoreAudio adapter silently discards disconnect errors for a device
//! that was the default at stream creation. Bound the wait for the next frame
//! and keep the actual device UID for a final disconnect check on stop.
#[path = "../native_audio.rs"]
pub(crate) mod shared;

pub(crate) use shared::{audio_devices, require_source};

/// Doctor's synchronous probe must distinguish an enumerated, denied input
/// from a usable one. Settings can still enumerate devices before consent.
pub(crate) fn query_devices() -> Result<shared::AudioDevices, String> {
    crate::desktop::native::microphone_permission()?;
    shared::query_devices()
}
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::DeviceId;
use std::{io, time::Duration};

pub struct RawCapture {
    stream: shared::RawCapture,
    device: DeviceId,
    stalled: bool,
}

impl RawCapture {
    pub fn start(target: Option<DeviceId>) -> io::Result<Self> {
        crate::desktop::native::request_microphone_permission().map_err(io::Error::other)?;
        let device = match target {
            Some(device) => device,
            None => cpal::default_host()
                .default_input_device()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no default CoreAudio input device")
                })?
                .id()
                .map_err(io::Error::other)?,
        };
        let stream = shared::RawCapture::start(Some(device.clone()))?;
        Ok(Self {
            stream,
            device,
            stalled: false,
        })
    }

    pub async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if !self.stalled {
            if let Ok(frame) =
                tokio::time::timeout(Duration::from_secs(3), self.stream.next_chunk()).await
            {
                return frame;
            }
            self.stalled = true;
        }
        Err(io::Error::new(io::ErrorKind::TimedOut,
            "CoreAudio stopped delivering microphone frames; reconnect the microphone or select another input"))
    }

    pub async fn stop(&mut self) -> io::Result<Option<Vec<u8>>> {
        let tail = self.stream.stop().await?;
        if self.stalled || cpal::default_host().device_by_id(&self.device).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the recording microphone disconnected",
            ));
        }
        Ok(tail)
    }

    pub(crate) async fn abort(&mut self) {
        self.stream.abort().await;
    }
}
