//! CPAL endpoints are selected by host-qualified ID. Enumeration never opens a stream.
//! Start the selected native devices before creating a journal. Their callbacks
//! discard audio until the meeting gives each worker its capture lease.
use super::track::PrepareError;
use super::{Failure, SourceCatalog, SourceRef, Tools, TrackKind, TrackSpec};
pub(super) use crate::desktop::audio::shared::PreparedCapture;
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
                input: None,
                node,
                label: source.label.clone(),
            }),
            None => Err(Failure::SourceMissing { source: kind, node }),
        }
    }
}

pub(super) fn prepare_inputs(specs: &mut [TrackSpec]) -> Result<(), Failure> {
    // A meeting always selects its microphone first and optionally an output.
    let microphone = &specs[0];
    crate::desktop::native::request_microphone_permission()
        .map_err(|reason| refused(microphone.kind, &microphone.node, reason))?;
    // Starting Core Audio I/O can display the system audio dialog even after tap
    // creation succeeded. Leave the microphone closed until that request returns.
    for spec in specs.iter_mut().rev() {
        let id = spec
            .node
            .parse::<cpal::DeviceId>()
            .map_err(|error| unconnected(spec.kind, &spec.node, error.to_string()))?;
        let input = PreparedCapture::open(Some(id), spec.kind == TrackKind::System)
            .map_err(|error| open_error(spec.kind, &spec.node, error))?;
        spec.input = Some(input);
    }
    Ok(())
}

pub(super) struct AudioInput {
    stream: RawCapture,
    runtime: Option<tokio::runtime::Runtime>,
    pub(super) drained: bool,
    last_audio: Instant,
    kind: TrackKind,
}

/// A native source that would not open, as one that could not be connected.
fn unconnected(source: TrackKind, node: &str, reason: String) -> Failure {
    Failure::StreamUnconnected {
        source,
        node: node.into(),
        reason,
    }
}

/// The system's refusal of a native source, with where its access is decided.
fn refused(source: TrackKind, node: &str, reason: String) -> Failure {
    let remedy = match source {
        TrackKind::Microphone => crate::desktop::native::MICROPHONE_REMEDY,
        TrackKind::System => "Check system audio recording access in the system privacy settings",
    };
    Failure::PermissionDenied {
        source,
        node: node.into(),
        reason,
        remedy: remedy.into(),
    }
}

/// What a native stream reported: the system's refusal of its source, or a source that would not open.
fn open_error(source: TrackKind, node: &str, error: io::Error) -> Failure {
    if error.kind() == io::ErrorKind::PermissionDenied {
        refused(source, node, error.to_string())
    } else {
        unconnected(source, node, error.to_string())
    }
}

impl AudioInput {
    pub(super) fn start(
        input: PreparedCapture,
        kind: TrackKind,
        node: &str,
        lease: &std::fs::File,
    ) -> Result<Self, PrepareError> {
        let lease = lease.try_clone().map_err(PrepareError::Storage)?;
        let stream = input
            .record(lease)
            .map_err(|error| PrepareError::Native(open_error(kind, node, error)))?;
        Ok(Self {
            stream,
            runtime: None,
            drained: false,
            kind,
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
