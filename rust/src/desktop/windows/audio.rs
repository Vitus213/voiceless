//! WASAPI uses the shared CPAL capture and PCM encoder. Desktop privacy
//! settings may deny capture even when Windows still enumerates endpoints.
#[path = "../native_audio.rs"]
pub(crate) mod shared;

use crate::desktop::native::microphone_permission;
use cpal::DeviceId;
use std::io;

pub(crate) use shared::require_source;

pub(crate) fn query_devices() -> Result<shared::AudioDevices, String> {
    microphone_permission()?;
    shared::query_devices()
}

pub(crate) async fn audio_devices() -> Result<shared::AudioDevices, String> {
    tokio::task::spawn_blocking(query_devices)
        .await
        .map_err(|error| error.to_string())?
}

pub struct RawCapture(shared::RawCapture);

impl RawCapture {
    pub fn start(target: Option<DeviceId>) -> io::Result<Self> {
        microphone_permission()
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
        shared::RawCapture::start(target).map(Self).map_err(|error| io::Error::new(error.kind(),
            format!("WASAPI could not start recording: {error}. Check the selected input and Settings > Privacy > Microphone > Let desktop apps access your microphone.")))
    }

    pub async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.0.next_chunk().await
    }
    pub async fn stop(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.0.stop().await
    }
    pub(crate) async fn abort(&mut self) {
        self.0.abort().await;
    }
}
