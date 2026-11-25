//! CPAL endpoints are selected by host-qualified ID. Enumeration never opens a stream.
use super::track::{PrepareError, Spec};
use super::{Failure, SourceCatalog, SourceRef, Tools, TrackKind, TrackSpec};
use crate::desktop::audio::shared::{query_devices, RawCapture};
use cpal::traits::{DeviceTrait, HostTrait};
use std::io;
use std::time::{Duration, Instant};

pub(super) async fn read(_: &Tools) -> Result<SourceCatalog, String> {
    tokio::task::spawn_blocking(|| {
        let inputs = query_devices()?;
        let host = cpal::default_host();
        #[cfg(windows)]
        let devices = host.output_devices();
        #[cfg(target_os = "macos")]
        let devices = host.devices();
        let mut outputs: Vec<_> = devices
            .map_err(|error| error.to_string())?
            .filter_map(|device| {
                let id = device.id().ok()?;
                #[cfg(target_os = "macos")]
                let label = crate::desktop::native::audio_label(&id.1, true)?;
                #[cfg(windows)]
                let label = device
                    .description()
                    .map_or_else(|_| id.to_string(), |info| info.name().to_owned());
                Some(SourceRef {
                    node: id.to_string(),
                    label,
                })
            })
            .collect();
        outputs.sort_unstable_by(|a, b| a.label.cmp(&b.label).then(a.node.cmp(&b.node)));
        let default_output = host
            .default_output_device()
            .and_then(|device| device.id().ok())
            .map(|id| id.to_string())
            .filter(|id| outputs.iter().any(|source| &source.node == id));
        Ok(SourceCatalog {
            microphones: inputs
                .microphones
                .into_iter()
                .map(|mic| SourceRef {
                    node: mic.id,
                    label: mic.label,
                })
                .collect(),
            system_outputs: outputs,
            default_microphone: inputs.system_default,
            default_system_output: default_output,
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

impl SourceCatalog {
    pub(super) fn catalog(self) -> Self {
        self
    }

    pub(super) fn resolve(&self, kind: TrackKind, node: String) -> Result<TrackSpec, Failure> {
        let sources = match kind {
            TrackKind::Microphone => &self.microphones,
            TrackKind::System => &self.system_outputs,
        };
        match sources.iter().find(|source| source.node == node) {
            Some(source) => Ok(TrackSpec {
                kind,
                node,
                label: source.label.clone(),
            }),
            None => Err(Failure::SourceMissing { source: kind, node }),
        }
    }
}

pub(super) struct AudioInput {
    stream: RawCapture,
    runtime: Option<tokio::runtime::Runtime>,
    pub(super) drained: bool,
    last_audio: Instant,
    kind: TrackKind,
}

impl AudioInput {
    pub(super) fn open(spec: &Spec<'_>) -> Result<Self, PrepareError> {
        if spec.kind == TrackKind::Microphone {
            crate::desktop::native::request_microphone_permission().map_err(|reason| {
                PrepareError::Native(Failure::PermissionDenied {
                    source: spec.kind,
                    node: spec.node.into(),
                    reason,
                    remedy: "Check microphone access in the system privacy settings".into(),
                })
            })?;
        }
        let failed = |reason| {
            PrepareError::Native(Failure::StreamUnconnected {
                source: spec.kind,
                node: spec.node.into(),
                reason,
            })
        };
        let id = spec
            .node
            .parse::<cpal::DeviceId>()
            .map_err(|error| failed(error.to_string()))?;
        let lease = spec.lease.try_clone().map_err(PrepareError::Storage)?;
        let stream =
            RawCapture::open(Some(id), spec.kind == TrackKind::System, lease).map_err(|error| {
                if error.kind() == io::ErrorKind::PermissionDenied {
                    PrepareError::Native(Failure::PermissionDenied {
                        source: spec.kind,
                        node: spec.node.into(),
                        reason: error.to_string(),
                        remedy:
                            "Check system audio recording access in the system privacy settings"
                                .into(),
                    })
                } else {
                    failed(error.to_string())
                }
            })?;
        Ok(Self {
            stream,
            runtime: None,
            drained: false,
            kind: spec.kind,
            last_audio: Instant::now(),
        })
    }

    /// Runs on the journal writer thread, including when the daemon runtime is shutting down.
    pub(super) fn read(
        &mut self,
        buffer: &mut Vec<u8>,
        stopping: bool,
        grace: Duration,
    ) -> io::Result<Option<usize>> {
        if self.drained {
            return Ok(Some(0));
        }
        if self.runtime.is_none() {
            self.runtime = Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()?,
            );
        }
        let runtime = self.runtime.as_ref().expect("writer runtime");
        let frame = if stopping {
            self.drained = true;
            runtime
                .block_on(async { tokio::time::timeout(grace, self.stream.stop()).await })
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "native audio did not drain before the stop deadline",
                    )
                })??
        } else {
            match runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(100), self.stream.next_chunk()).await
            }) {
                Ok(frame) => frame?,
                Err(_)
                    if self.kind == TrackKind::Microphone
                        && self.last_audio.elapsed() >= Duration::from_secs(3) =>
                {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the selected microphone stopped delivering audio",
                    ))
                }
                Err(_) => return Ok(None),
            }
        };
        if let Some(frame) = frame {
            self.last_audio = Instant::now();
            *buffer = frame;
            Ok(Some(buffer.len()))
        } else {
            Ok(Some(0))
        }
    }
}
