//! The `voiceless stream` command and the production wiring of a realtime
//! session: config -> `coordinator::run_session` with `pw-record` capture
//! (`PwCapture`), one backend transport, the final-text pipeline (`[text]`
//! cleanup, modes, LLM processing), SQLite history with WAV retention,
//! stdout/JSON or daemon-broadcast event sinks, and text injection
//! (`[inject]`: wtype, clipboard or the Fcitx5 bridge).
//!
//! Entry points: `run` (CLI: SIGINT/SIGTERM become `Stop`/`Cancel`),
//! `run_with_control` (caller-provided control channel) and `run_with_env`
//! (adds the daemon's warm engine and event broadcast; `daemon.rs` calls
//! it once per activation). `main.rs` reaches `run` only for a standalone
//! session: when the configured backend is local (`local-offline` or
//! `local-streaming`) and a daemon owns `dev.voiceless.Daemon`, it delegates
//! to `ctl::stream_via_daemon` instead (D-Bus `StartSession(mode, inject)`,
//! events and Ctrl-C relayed), unless `--config` or `--backend` was given.
//!
//! Config read here: `[asr].streaming_backend`, `fallback_backend`,
//! `local_model_dir`; `[cloud]`, `[doubao]`, `[local]` through `AppConfig`;
//! `[processing]`; `[text]`; `[history].enabled`, `save_audio`; `[inject]`.
//! Files: `$XDG_CONFIG_HOME/voiceless/modes.json` (`modes_path`),
//! `$XDG_DATA_HOME/voiceless/history.sqlite3` and `audio/`
//! (`models::default_data_dir`, `default_audio_dir`), the managed model
//! directories. Processes spawned: `pw-record`, `wtype`, `wl-copy`,
//! `wl-paste` and the configured paste, focus and X11 clipboard commands.
//!
//! The mode and final-text pipeline are resolved before opening a backend.
//! A configured fallback may open once if the primary fails before capture.
//! Configuration and connection errors use `StreamError`. Model and
//! readiness failures use the session's `error` and `completed` events.
//!
//! `stream --json` prints one `JsonEvent` line per `OutputEvent`
//! (`json_event`); the daemon broadcasts the same lines as D-Bus `Event`
//! signals, which `voiceless ctl watch` and the console relay.

use std::io::{self, Write};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
#[cfg(target_os = "linux")]
use tokio::io::AsyncWriteExt;
#[cfg(target_os = "linux")]
use tokio::process::Command;
use tokio::time::{timeout, timeout_at, Instant};

use crate::capture::RawCapture;
use crate::config::{validate_streaming_backend, AppConfig, ConfigError, InjectConfig};
use crate::coordinator::{
    AudioCapture, BackendFallback, BackendSelection, BackendTransport, ControlCommand, Delivery,
    EventSink, HistoryEntry, HistoryRecorder, InjectionResult, OutputEvent, ReadyError,
    SessionPhase, SessionPlan, TextInjector, TextProcessor, DEFAULT_READY_TIMEOUT,
};
use crate::realtime::{RealtimeEvent, RealtimeSession};
use crate::session::TranscriptSnapshot;

#[cfg(test)]
mod tests;

/// Context explicitly supplied by an authorized local session caller.
#[derive(Debug, Clone, Default)]
pub struct DictationContext {
    pub site: Option<String>,
    pub project: Option<String>,
}

/// Options of one `stream` invocation. `main.rs` fills them from the CLI;
/// the daemon keeps one copy and overrides `mode` and `inject` per session.
#[derive(Debug, Clone)]
pub struct StreamOptions {
    /// `--config` path; `None` loads the default XDG config (optional file).
    pub config_path: Option<PathBuf>,
    /// `--backend`: overrides `asr.streaming_backend`, validated like a
    /// config file would be.
    pub backend: Option<String>,
    /// `--json`: one JSON line per event on stdout instead of plain text.
    pub json: bool,
    /// Inject the final text into the focused window (`--no-inject` clears
    /// it); processing and history happen either way.
    pub inject: bool,
    /// Mode id or (case-insensitive) name resolved against `modes.json`;
    /// the CLI default is `quick`.
    pub mode: String,
    pub context: DictationContext,
}

impl DictationContext {
    pub(crate) fn is_empty(&self) -> bool {
        self.site.is_none() && self.project.is_none()
    }
}

/// Startup failures of a standalone session, all raised before any audio
/// is captured. `main.rs` prints them as `Voiceless: <error>` and exits 1.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// Config file problems: permissions, unknown keys, invalid values, or
    /// an invalid `--backend` override.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// `asr.streaming_backend` (or `--backend`) names an unknown backend.
    #[error("stream backend {0:?} is not implemented yet")]
    UnsupportedBackend(String),
    /// `cloud-realtime` selected without `[cloud].api_key`.
    #[error("cloud realtime API key is missing")]
    MissingApiKey,
    /// `doubao-realtime` selected without `[doubao]` credentials.
    #[error("doubao credentials are missing: set doubao.api_key, or doubao.app_key and doubao.access_key")]
    MissingDoubaoCredentials,
    /// Connecting a cloud WebSocket failed (DNS, TLS, 401/403, ...).
    #[error("realtime session: {0}")]
    Realtime(#[from] io::Error),
    /// Pipeline setup failed: unreadable `modes.json`, unknown mode or an
    /// invalid processing provider config. The message keeps the historical
    /// `capture:` prefix.
    #[error("capture: {0}")]
    Capture(String),
}

/// One `stream --json` line. `type` is the event kind (`ready`, `level`,
/// `transcript`, `finalized`, `warning`, `error`, `cancelled`,
/// `completed`); `sequence` counts from 1 per session; `transcript`,
/// `message` and `injection` are `null` unless the kind carries them; `rms`
/// is present only on `level`.
#[derive(Debug, Serialize)]
pub(crate) struct JsonEvent<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<SessionPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend_selection: Option<&'a BackendSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    devices: Option<&'a crate::offline_asr::EngineDevices>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<&'a crate::output::Trace>,
    #[serde(rename = "type")]
    kind: &'a str,
    sequence: u64,
    transcript: Option<&'a TranscriptSnapshot>,
    message: Option<&'a str>,
    injection: Option<&'a InjectionResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rms: Option<f32>,
}

/// Run `voiceless stream` standalone: the first SIGINT stops, a second
/// SIGINT or SIGTERM cancels. Returns the session exit code (`0`, `1`,
/// `130`) or a startup error.
pub async fn run(options: StreamOptions) -> Result<i32, StreamError> {
    let _runtime_lock = crate::runtime::lock_runtime(&crate::models::default_data_dir())
        .map_err(|error| StreamError::Capture(error.to_string()))?;
    let control = control_receiver();
    run_with_control(options, control).await
}

/// Run a streaming session driven by an externally supplied control channel
/// instead of process signals. Used by the headless daemon.
pub async fn run_with_control(
    options: StreamOptions,
    control: tokio::sync::mpsc::Receiver<ControlCommand>,
) -> Result<i32, StreamError> {
    run_with_env(options, control, SessionEnv::default()).await
}

/// Resources lent by a resident daemon to a session.
#[derive(Default)]
pub struct SessionEnv {
    /// Model loaded by the daemon. Standalone sessions load on demand.
    pub warm: Option<std::sync::Arc<crate::engine::WarmEngine>>,
    /// Receives session events and mic levels when supplied. Defaults to stdout.
    pub sink: Option<Box<dyn EventSink + Send>>,
}

/// Full session setup and run; see the module doc for the order. Returns
/// `Ok(code)` once a session ran, or a model error was reported as events,
/// and `Err` for configuration and connection errors found before capture.
pub async fn run_with_env(
    options: StreamOptions,
    control: tokio::sync::mpsc::Receiver<ControlCommand>,
    env: SessionEnv,
) -> Result<i32, StreamError> {
    let config = AppConfig::load_optional(options.config_path.as_deref())?;
    run_with_config(
        options,
        control,
        env,
        std::borrow::Cow::Owned(config),
        "quick",
    )
    .await
}

pub(crate) async fn run_with_config(
    options: StreamOptions,
    control: tokio::sync::mpsc::Receiver<ControlCommand>,
    env: SessionEnv,
    mut config: std::borrow::Cow<'_, AppConfig>,
    default_mode: &str,
) -> Result<i32, StreamError> {
    // An override changes only the backend of an already validated config.
    if let Some(backend) = options.backend.as_deref() {
        validate_streaming_backend("streaming_backend", backend)?;
        config.to_mut().asr.streaming_backend = backend.to_owned();
    }
    let config = config.as_ref();
    let pipeline = build_pipeline(config, &options, default_mode).await?;
    let capture = PwCapture {
        inner: None,
        source: config.audio.source.as_deref(),
    };
    run_configured(config, &options, control, env, pipeline, capture).await
}

async fn run_configured<C: AudioCapture>(
    config: &AppConfig,
    options: &StreamOptions,
    mut control: tokio::sync::mpsc::Receiver<ControlCommand>,
    mut env: SessionEnv,
    pipeline: Pipeline,
    capture: C,
) -> Result<i32, StreamError> {
    let daemon_events = env.sink.is_some();
    let mut sink = env
        .sink
        .take()
        .unwrap_or_else(|| Box::new(StdoutSink::new(options.json)));
    #[cfg(target_os = "linux")]
    if let Some(client) = pipeline
        .injector
        .fcitx5
        .as_ref()
        .filter(|_| config.inject.preedit)
    {
        sink = Box::new(crate::fcitx5::PreeditSink::new(sink, client.clone()));
    }
    let _ = sink.emit(OutputEvent::Output(Box::new(crate::output::Trace {
        selection: pipeline.processor.selection().clone(),
        steps: Vec::new(),
    })));
    let primary = config.asr.streaming_backend.as_str();
    let fallback = config
        .asr
        .fallback_backend
        .as_deref()
        .filter(|&id| id != primary);
    let warm = env.warm.as_deref();
    // Nothing removes the models this session, or its fallback, may load
    // while it runs; the leases live until the session has ended.
    let _model_leases = crate::models::lease_models(
        [primary, fallback.unwrap_or(primary)]
            .into_iter()
            .flat_map(|backend| crate::engine::model_ids_for(config, backend)),
    );
    let hotwords = pipeline
        .processor
        .scoped_hotwords()
        .unwrap_or(&config.text.hotwords);
    let opened = match open_ready_transport(primary, config, warm, hotwords, &mut control).await {
        Ok(transport) => Ok((primary, transport, None)),
        Err(error) => match fallback {
            Some(fallback) if !matches!(error, OpenError::Cancelled) => {
                let reason = error.to_string();
                let _ = sink.emit(OutputEvent::Warning(format!(
                    "{primary} unavailable ({reason}); falling back to {fallback}"
                )));
                open_ready_transport(fallback, config, warm, hotwords, &mut control)
                    .await
                    .map(|transport| {
                        let fallback_reason = BackendFallback {
                            primary: primary.to_owned(),
                            reason,
                        };
                        (fallback, transport, Some(fallback_reason))
                    })
            }
            _ => Err(error),
        },
    };
    let (backend, mut transport, fallback) = match opened {
        Ok(opened) => opened,
        Err(OpenError::Stream(error)) => return Err(error),
        Err(OpenError::Model(message) | OpenError::Readiness(message)) => {
            return Ok(crate::coordinator::fail(&mut sink, &message));
        }
        Err(OpenError::Cancelled) => return Ok(crate::coordinator::cancelled(&mut sink)),
    };
    let mut plan = session_plan(config, options, backend, &pipeline.mode_id);
    if config.history.save_failed_audio {
        plan.failure_recovery = Some(crate::coordinator::FailureRecovery {
            directory: crate::history::retention::RecoveryStore::default_dir(),
            app_id: pipeline.processor.selection().app_id.clone(),
        });
    }
    if sink
        .emit(OutputEvent::BackendSelected(BackendSelection {
            backend: plan.backend.clone(),
            model: plan.model.clone(),
            requested_device: requested_device(config, backend),
            fallback,
        }))
        .is_err()
    {
        let _ = transport.cancel().await;
        return Ok(1);
    }
    if backend == "local-streaming" {
        let _ = sink.emit(OutputEvent::DevicesLoaded(Box::new(
            crate::offline_asr::EngineDevices {
                primary: crate::offline_asr::InferenceDevice::sherpa_cpu(),
                preview: None,
            },
        )));
    }
    // Live UIs (daemon console, overlay) draw a mic level meter.
    plan.emit_levels = daemon_events;
    if backend == "local-offline" {
        // Bound capture drain and queuing the finish request. The worker's
        // final event can arrive later, including after a cold model load.
        plan.finish_timeout = Duration::from_secs(20);
    }
    let code = crate::coordinator::run_ready_session(
        plan,
        capture,
        transport,
        pipeline.processor,
        pipeline.injector,
        pipeline.history,
        control,
        &mut sink,
    )
    .await;
    Ok(code)
}

/// A backend can fail while opening the transport or while the startup
/// acknowledgement is pending. Cancellation abandons both attempts.
enum OpenError {
    Stream(StreamError),
    Model(String),
    Readiness(String),
    Cancelled,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Stream(error) => error.fmt(f),
            OpenError::Model(message) | OpenError::Readiness(message) => f.write_str(message),
            OpenError::Cancelled => f.write_str("startup cancelled"),
        }
    }
}

impl From<StreamError> for OpenError {
    fn from(error: StreamError) -> Self {
        OpenError::Stream(error)
    }
}

async fn open_ready_transport(
    backend: &str,
    config: &AppConfig,
    warm: Option<&crate::engine::WarmEngine>,
    hotwords: &[String],
    control: &mut tokio::sync::mpsc::Receiver<ControlCommand>,
) -> Result<AnyTransport, OpenError> {
    let deadline = Instant::now() + DEFAULT_READY_TIMEOUT;
    let mut transport = tokio::select! {
        biased;
        _ = control.recv() => return Err(OpenError::Cancelled),
        result = timeout_at(deadline, open_transport(backend, config, warm, hotwords)) => {
            result.map_err(|_| OpenError::Readiness(format!("opening {backend} timed out")))??
        }
    };
    let error = match crate::coordinator::wait_until_ready(&transport, control, deadline).await {
        Ok(()) => return Ok(transport),
        Err(ReadyError::Cancelled) => OpenError::Cancelled,
        Err(ReadyError::Failed(message)) => OpenError::Readiness(message),
    };
    let _ = transport.cancel().await;
    Err(error)
}

/// Connect or load the transport for one backend, before any capture.
async fn open_transport(
    backend: &str,
    config: &AppConfig,
    warm: Option<&crate::engine::WarmEngine>,
    hotwords: &[String],
) -> Result<AnyTransport, OpenError> {
    match backend {
        "cloud-realtime" => {
            if config.cloud.api_key.trim().is_empty() {
                return Err(StreamError::MissingApiKey.into());
            }
            let session = RealtimeSession::connect(
                &config.cloud.realtime_endpoint,
                &config.cloud.api_key,
                &config.cloud.realtime_model,
            )
            .await
            .map_err(StreamError::from)?;
            Ok(AnyTransport::Cloud(CloudTransport(session)))
        }
        "doubao-realtime" => {
            if !config.doubao.has_credentials() {
                return Err(StreamError::MissingDoubaoCredentials.into());
            }
            let session = crate::doubao::DoubaoSession::connect(
                &config.doubao_engine_with_hotwords(hotwords),
            )
            .await
            .map_err(StreamError::from)?;
            Ok(AnyTransport::Doubao(DoubaoTransport(session)))
        }
        "local-streaming" => {
            let directory = config.asr.local_model_dir.as_deref();
            let recognizer = match warm.and_then(|engine| engine.streaming_for(directory)) {
                Some(recognizer) => recognizer,
                None => {
                    let model =
                        crate::engine::load_streaming_model(directory).map_err(OpenError::Model)?;
                    crate::local_asr::LocalStreamingRecognizer::from_model(model)
                }
            };
            Ok(AnyTransport::Local(LocalTransport::new(recognizer)))
        }
        "local-offline" => open_local_offline(config, warm, hotwords).map(AnyTransport::Offline),
        value => Err(StreamError::UnsupportedBackend(value.to_owned()).into()),
    }
}

/// Build the `local-offline` transport: reuse the daemon's warm decoder
/// when `[local]` still matches it, otherwise resolve and verify the model
/// (a catalog entry or an imported one) and the Silero VAD from the store
/// and load them on the worker thread.
/// Never blocks on the load itself.
fn open_local_offline(
    config: &AppConfig,
    warm: Option<&crate::engine::WarmEngine>,
    hotwords: &[String],
) -> Result<OfflineTransportAdapter, OpenError> {
    let local = if matches!(
        crate::offline_asr::OfflineFamily::for_model(&config.local.model),
        Ok(crate::offline_asr::OfflineFamily::SenseVoice)
    ) {
        config.local_engine()
    } else {
        config.local_engine_with_hotwords(hotwords)
    };
    let denoise_model = local
        .denoise
        .then(|| {
            resolve_managed_model(&crate::models::gtcrn_spec())
                .map(|dir| dir.join("gtcrn_simple.onnx"))
                .map_err(OpenError::Model)
        })
        .transpose()?;
    // A warm engine was verified at daemon start: skip the per-session hash
    // check of ~1-2 GB of model files, which would delay capture.
    let warm = warm.and_then(|engine| {
        engine
            .offline_for(&local)
            .map(|(decoder, vad_model)| (decoder, engine.preview_decoder(), vad_model))
    });
    let (warm_decoders, model_dirs, vad_model) = match warm {
        Some((decoder, preview, vad_model)) => (Some((decoder, preview)), None, vad_model),
        None => {
            // A catalog model or an imported one, verified through its
            // pointer.
            let model_dir =
                crate::models::installed_model_dir(&local.model).map_err(OpenError::Model)?;
            let vad_dir = resolve_managed_model(&crate::models::silero_vad_spec())
                .map_err(OpenError::Model)?;
            let preview_dir = local
                .preview_model()
                .map(crate::models::installed_model_dir)
                .transpose()
                .map_err(OpenError::Model)?;
            (
                None,
                Some((model_dir, preview_dir)),
                vad_dir.join("silero_vad.onnx"),
            )
        }
    };
    // Without a warm engine the model loads on the worker, so capture
    // starts at once either way.
    let transport = crate::offline_asr::OfflineTransport::start(move || {
        use crate::offline_asr::{create_decoder, create_session_vad, OfflineFamily};
        let (decoder, preview) = match (warm_decoders, model_dirs) {
            (Some(decoders), _) => decoders,
            (None, Some((model_dir, preview_dir))) => {
                let decoder = create_decoder(
                    OfflineFamily::for_model(&local.model)?,
                    &model_dir,
                    &crate::engine::offline_options(&local),
                )?;
                let preview = preview_dir
                    .as_deref()
                    .map(|path| crate::engine::create_preview_decoder(path, local.num_threads))
                    .transpose()?;
                (decoder, preview)
            }
            (None, None) => unreachable!("cold sessions resolve the model directories"),
        };
        let vad = create_session_vad(&vad_model, local.segment_limit(), denoise_model.is_some())?;
        let recognizer = crate::offline_asr::SegmentedRecognizer::new(
            decoder,
            vad,
            Duration::from_millis(local.partial_interval_ms),
        )
        .with_preview(preview);
        match denoise_model {
            Some(path) => recognizer.with_denoiser(&path),
            None => Ok(recognizer),
        }
    });
    Ok(OfflineTransportAdapter(transport))
}

/// The transport of whichever backend opened.
enum AnyTransport {
    Cloud(CloudTransport),
    Doubao(DoubaoTransport),
    Local(LocalTransport),
    Offline(OfflineTransportAdapter),
}

impl BackendTransport for AnyTransport {
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        match self {
            AnyTransport::Cloud(transport) => transport.send_audio(pcm).await,
            AnyTransport::Doubao(transport) => transport.send_audio(pcm).await,
            AnyTransport::Local(transport) => transport.send_audio(pcm).await,
            AnyTransport::Offline(transport) => transport.send_audio(pcm).await,
        }
    }

    async fn next_event(&self) -> io::Result<RealtimeEvent> {
        match self {
            AnyTransport::Cloud(transport) => transport.next_event().await,
            AnyTransport::Doubao(transport) => transport.next_event().await,
            AnyTransport::Local(transport) => transport.next_event().await,
            AnyTransport::Offline(transport) => transport.next_event().await,
        }
    }

    async fn finish(&mut self) -> io::Result<()> {
        match self {
            AnyTransport::Cloud(transport) => transport.finish().await,
            AnyTransport::Doubao(transport) => transport.finish().await,
            AnyTransport::Local(transport) => transport.finish().await,
            AnyTransport::Offline(transport) => transport.finish().await,
        }
    }

    async fn cancel(&mut self) -> io::Result<()> {
        match self {
            AnyTransport::Cloud(transport) => transport.cancel().await,
            AnyTransport::Doubao(transport) => transport.cancel().await,
            AnyTransport::Local(transport) => transport.cancel().await,
            AnyTransport::Offline(transport) => transport.cancel().await,
        }
    }
}

/// Final-text pipeline of one session, built before the backend opens.
struct Pipeline {
    /// Resolved mode id (the CLI may have given a name).
    mode_id: String,
    processor: crate::output::Pipeline,
    history: SqliteHistory,
    injector: ConfiguredInjector,
}

/// Build the final-text pipeline once, before any capture starts: unknown
/// modes and invalid processing config are startup errors.
async fn build_pipeline(
    config: &AppConfig,
    options: &StreamOptions,
    default_mode: &str,
) -> Result<Pipeline, StreamError> {
    let track_app = config.history.enabled || !config.output.rules.is_empty();
    let mut injector = ConfiguredInjector::capture(
        config.inject.clone(),
        crate::desktop::focus::desktop(),
        options.inject,
        track_app,
    );
    let window = if track_app {
        injector.read_original().await
    } else {
        None
    };
    let context = crate::output::Context {
        app_id: window.and_then(|window| window.app_id.as_deref()),
        app_source: window.map(|window| window.source),
        site: options.context.site.as_deref(),
        site_source: Some("local-client"),
        project: options.context.project.as_deref(),
    };
    let processor = crate::output::Pipeline::prepare(config, &options.mode, default_mode, context)
        .map_err(StreamError::Capture)?;

    // A history database that cannot open disables history for this
    // session instead of failing it; recordings are then removed too.
    let store = if config.history.enabled {
        crate::history::HistoryStore::open(
            crate::models::default_data_dir().join("history.sqlite3"),
        )
        .ok()
    } else {
        None
    };
    let history_app = if store.is_some() && options.inject {
        processor.selection().app_id.clone()
    } else {
        None
    };

    Ok(Pipeline {
        mode_id: processor.selection().mode.clone(),
        processor,
        injector,
        history: SqliteHistory {
            store,
            audio_dir: audio_dir(config),
            app_id: history_app,
        },
    })
}

/// `$XDG_CONFIG_HOME/voiceless/modes.json` (falling back to `~/.config`):
/// the processing modes file, shared with the console settings API.
pub fn modes_path() -> std::path::PathBuf {
    let root = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned()))
                .join(".config")
        });
    root.join("voiceless").join("modes.json")
}

impl TextProcessor for crate::output::Pipeline {
    async fn process(&mut self, _mode_id: &str, text: &str) -> Result<String, String> {
        let result = self.run(text, false, true).await;
        match result.error {
            Some(error) => Err(error),
            None => Ok(result.text),
        }
    }

    fn announces_processing(&self, _mode_id: &str) -> bool {
        self.uses_model()
    }

    async fn finalize(&mut self, _mode_id: &str, raw: &str) -> crate::coordinator::CompletedText {
        let result = self.run(raw, false, true).await;
        let succeeded = result.error.is_none();
        crate::coordinator::CompletedText {
            modified: succeeded && (self.uses_model() || result.text != raw),
            text: result.text,
            error: result.error,
            trace: Some(result.trace),
            may_deliver: succeeded,
        }
    }
}

/// Production `HistoryRecorder`: inserts into the SQLite history and keeps
/// rows and audio files paired, deleting a recording whose row was not
/// written.
struct SqliteHistory {
    /// `None` when history is disabled or the database failed to open.
    store: Option<crate::history::HistoryStore>,
    /// Managed audio directory; only files directly inside it are removed.
    audio_dir: Option<PathBuf>,
    app_id: Option<String>,
}

impl HistoryRecorder for SqliteHistory {
    fn record(&mut self, entry: HistoryEntry) {
        let Some(store) = &self.store else {
            // History disabled: never keep orphan recordings behind.
            remove_audio(entry.audio_path.as_deref(), self.audio_dir.as_deref());
            return;
        };
        match store.insert_with_app(&entry, &entry.backend, self.app_id.as_deref()) {
            Ok(_) => {}
            Err(_) => remove_audio(entry.audio_path.as_deref(), self.audio_dir.as_deref()),
        }
    }
}

/// Audio retention directory: only when history is enabled and
/// `[history].save_audio` is set (a recording without a row is useless).
fn audio_dir(config: &AppConfig) -> Option<PathBuf> {
    if config.history.enabled && config.history.save_audio {
        Some(crate::models::default_audio_dir())
    } else {
        None
    }
}

/// Remove a saved recording when its history row was not persisted, so a
/// failure never leaves audio without a matching record. Only files that
/// live directly inside the managed audio directory are touched.
fn remove_audio(audio_path: Option<&str>, audio_dir: Option<&std::path::Path>) {
    let (Some(path), Some(dir)) = (audio_path, audio_dir) else {
        return;
    };
    let path = PathBuf::from(path);
    if path.parent() == Some(dir) {
        let _ = std::fs::remove_file(&path);
    }
}

/// Resolve the managed streaming model freshly through the pointer and
/// re-verify its payload; a corrupted install is never reused.
fn resolve_managed_model(spec: &crate::models::ModelSpec) -> Result<std::path::PathBuf, String> {
    crate::models::installed_model_dir(&spec.id)
}

/// Session parameters from config and CLI: backend id, injection, mode
/// and the retention directory. Bounds and timeouts stay at the
/// coordinator defaults; `run_with_env` adjusts them per backend.
fn session_plan(
    config: &AppConfig,
    options: &StreamOptions,
    backend: &str,
    mode_id: &str,
) -> SessionPlan {
    let mut plan = SessionPlan::new(backend, options.inject);
    plan.mode_id = mode_id.to_owned();
    plan.model = backend_model(config, backend);
    plan.save_audio_dir = audio_dir(config);
    plan
}

/// The model identifier a history row records as `asr_model` for
/// `backend`: the cloud model name, the Doubao resource id, the local
/// catalog id, or the overriding directory for `local-streaming`.
pub(crate) fn backend_model(config: &AppConfig, backend: &str) -> String {
    match backend {
        "cloud-realtime" => config.cloud.realtime_model.clone(),
        "doubao-realtime" => config.doubao.resource_id.clone(),
        "local-streaming" => config.asr.local_model_dir.as_ref().map_or_else(
            || crate::models::streaming_paraformer_spec().id,
            |dir| dir.display().to_string(),
        ),
        "local-offline" => config.local.model.clone(),
        _ => String::new(),
    }
}

pub(crate) fn requested_device(config: &AppConfig, backend: &str) -> Option<String> {
    match backend {
        "local-offline" => Some(config.local.device()),
        "local-streaming" => Some("cpu".to_owned()),
        _ => None,
    }
}

/// The first interrupt stops and a second cancels. Termination cancels
/// immediately. The relay uses the same control channel.
pub(crate) fn control_receiver() -> tokio::sync::mpsc::Receiver<ControlCommand> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut first_stop = false;
        #[cfg(unix)]
        let (mut stop, mut cancel) = {
            use tokio::signal::unix::{signal, SignalKind};
            (
                signal(SignalKind::interrupt()).expect("install SIGINT handler"),
                signal(SignalKind::terminate()).expect("install SIGTERM handler"),
            )
        };
        #[cfg(windows)]
        let (mut stop, mut cancel) = {
            use tokio::signal::windows::{ctrl_break, ctrl_c};
            (
                ctrl_c().expect("install Ctrl-C handler"),
                ctrl_break().expect("install Ctrl-Break handler"),
            )
        };
        loop {
            tokio::select! {
                _ = stop.recv() => {
                    let command = if first_stop {
                        ControlCommand::Cancel
                    } else {
                        first_stop = true;
                        ControlCommand::Stop
                    };
                    if tx.send(command).await.is_err() {
                        break;
                    }
                }
                _ = cancel.recv() => {
                    let _ = tx.send(ControlCommand::Cancel).await;
                    break;
                }
                _ = tx.closed() => break,
            }
        }
    });
    rx
}

/// pw-record capture adapted to the coordinator boundary. Construction is
/// deferred to `start` so the ready gate can fail before any capture exists.
struct PwCapture<'a> {
    inner: Option<RawCapture>,
    source: Option<&'a str>,
}

impl AudioCapture for PwCapture<'_> {
    async fn start(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        let capture = RawCapture::start(self.source).await?;
        #[cfg(not(target_os = "linux"))]
        let target = match self.source {
            Some(source) => Some(
                crate::desktop::audio::require_source(source)
                    .await
                    .map_err(io::Error::other)?,
            ),
            None => None,
        };
        #[cfg(not(target_os = "linux"))]
        let capture = tokio::task::spawn_blocking(move || RawCapture::start(target))
            .await
            .map_err(io::Error::other)??;
        self.inner = Some(capture);
        Ok(())
    }

    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        match &mut self.inner {
            Some(capture) => capture.next_chunk().await,
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "capture not started",
            )),
        }
    }

    async fn stop_capture(&mut self) -> io::Result<Option<Vec<u8>>> {
        match &mut self.inner {
            Some(capture) => capture.stop().await,
            None => Ok(None),
        }
    }

    async fn abort(&mut self) {
        if let Some(mut capture) = self.inner.take() {
            capture.abort().await;
        }
    }
}

/// DashScope realtime WebSocket (`realtime::RealtimeSession`) behind the
/// trait; `finish` sends `session.finish` once, `cancel` closes the socket.
struct CloudTransport(RealtimeSession);

impl BackendTransport for CloudTransport {
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        self.0.send_audio(pcm).await
    }

    async fn next_event(&self) -> io::Result<RealtimeEvent> {
        self.0.next_event().await
    }

    async fn finish(&mut self) -> io::Result<()> {
        self.0.finish().await
    }

    async fn cancel(&mut self) -> io::Result<()> {
        self.0.cancel().await
    }
}

/// `local-offline` worker thread (`offline_asr::OfflineTransport`).
/// `send_audio`, `finish` and `cancel` are synchronous channel sends, so
/// the session never waits on a decode; `Ready` is queued immediately and
/// a model-load failure arrives later as an `Error` event.
struct OfflineTransportAdapter(crate::offline_asr::OfflineTransport);

impl BackendTransport for OfflineTransportAdapter {
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        self.0.send_audio(pcm)
    }

    async fn next_event(&self) -> io::Result<RealtimeEvent> {
        self.0.next_event().await
    }

    async fn finish(&mut self) -> io::Result<()> {
        self.0.finish()
    }

    async fn cancel(&mut self) -> io::Result<()> {
        self.0.cancel()
    }
}

/// Volcengine Doubao WebSocket (`doubao::DoubaoSession`); it buffers
/// chunks into 100 ms packets and `finish` flushes the remainder as the
/// last packet.
struct DoubaoTransport(crate::doubao::DoubaoSession);

impl BackendTransport for DoubaoTransport {
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        self.0.send_audio(pcm).await
    }

    async fn next_event(&self) -> io::Result<RealtimeEvent> {
        self.0.next_event().await
    }

    async fn finish(&mut self) -> io::Result<()> {
        self.0.finish().await
    }

    async fn cancel(&mut self) -> io::Result<()> {
        self.0.cancel().await
    }
}

/// Local streaming backend exposed through the same event boundary. The
/// recognizer is synchronous; it reports Ready immediately and drains
/// decoded events before each await. Decoding happens inline in
/// `send_audio` on the runtime thread (Sherpa-ONNX online models are
/// cheap per 32 ms chunk); `next_event` polls the queue every 2 ms.
struct LocalTransport {
    state: parking_lot::Mutex<LocalTransportState>,
}

struct LocalTransportState {
    recognizer: crate::local_asr::LocalStreamingRecognizer,
    /// Decoded events not yet handed to the coordinator.
    pending: std::collections::VecDeque<io::Result<RealtimeEvent>>,
    /// `Ready` is delivered exactly once, on the first `next_event`.
    ready_sent: bool,
    /// Set by `finish` and `cancel`; audio is refused afterwards.
    finished: bool,
}

impl LocalTransport {
    fn new(recognizer: crate::local_asr::LocalStreamingRecognizer) -> Self {
        Self {
            state: parking_lot::Mutex::new(LocalTransportState {
                recognizer,
                pending: std::collections::VecDeque::new(),
                ready_sent: false,
                finished: false,
            }),
        }
    }
}

impl BackendTransport for LocalTransport {
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock();
        if state.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "local session already finished",
            ));
        }
        match state.recognizer.accept_pcm(pcm) {
            Ok(events) => {
                for event in events {
                    state.pending.push_back(Ok(event));
                }
                Ok(())
            }
            Err(error) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                error.to_string(),
            )),
        }
    }

    async fn next_event(&self) -> io::Result<RealtimeEvent> {
        loop {
            {
                let mut state = self.state.lock();
                if !state.ready_sent {
                    state.ready_sent = true;
                    return Ok(RealtimeEvent::Ready);
                }
                if let Some(event) = state.pending.pop_front() {
                    return event;
                }
                // Nothing left after finish: report EOF so a recognizer
                // that never produced `Finished` fails the session instead
                // of hanging until the flush deadline.
                if state.finished {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "local session drained",
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn finish(&mut self) -> io::Result<()> {
        let mut state = self.state.lock();
        if state.finished {
            return Ok(());
        }
        state.finished = true;
        match state.recognizer.finish() {
            Ok(events) => {
                for event in events {
                    state.pending.push_back(Ok(event));
                }
                Ok(())
            }
            Err(error) => Err(io::Error::other(error.to_string())),
        }
    }

    async fn cancel(&mut self) -> io::Result<()> {
        self.state.lock().finished = true;
        Ok(())
    }
}

/// CLI sink. With `--json`, one `JsonEvent` line per event on stdout.
/// In plain mode live partials redraw a single stderr line (`\r`), the
/// final transcript is the only stdout line (so `stream` composes with
/// pipes), warnings and errors go to stderr, other events are silent.
struct StdoutSink {
    json: bool,
    sequence: u64,
    preview: TerminalPreview,
}

impl StdoutSink {
    fn new(json: bool) -> Self {
        Self {
            json,
            sequence: 0,
            preview: TerminalPreview::default(),
        }
    }
}

/// Lets `run_with_env` stack sinks (`PreeditSink` over the base sink)
/// behind one trait object.
impl<T: EventSink + ?Sized> EventSink for Box<T> {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        (**self).emit(event)
    }

    fn retain_final_text(&mut self, text: &str) {
        (**self).retain_final_text(text);
    }
}

/// Map an `OutputEvent` to its JSON line; the `type` names are listed on
/// `JsonEvent` and on each `OutputEvent` variant.
pub(crate) fn json_event(
    event: &OutputEvent,
    sequence: u64,
    session_id: Option<u64>,
) -> JsonEvent<'_> {
    let (kind, transcript, message, injection, rms) = match event {
        OutputEvent::Ready => ("ready", None, None, None, None),
        OutputEvent::BackendSelected(_) => ("backend-selected", None, None, None, None),
        OutputEvent::DevicesLoaded(_) => ("devices-loaded", None, None, None, None),
        OutputEvent::Level(rms) => ("level", None, None, None, Some(*rms)),
        OutputEvent::Transcript(snapshot) => ("transcript", Some(snapshot), None, None, None),
        OutputEvent::Finalized { injection } => ("finalized", None, None, injection.as_ref(), None),
        OutputEvent::Warning(message) => ("warning", None, Some(message.as_str()), None, None),
        OutputEvent::Phase(_) => ("phase", None, None, None, None),
        OutputEvent::Output(_) => ("output", None, None, None, None),
        OutputEvent::Error(message) => ("error", None, Some(message.as_str()), None, None),
        OutputEvent::Cancelled => ("cancelled", None, None, None, None),
        OutputEvent::Completed => ("completed", None, None, None, None),
    };
    JsonEvent {
        session_id,
        phase: match event {
            OutputEvent::Phase(phase) => Some(*phase),
            _ => None,
        },
        backend_selection: match event {
            OutputEvent::BackendSelected(backend) => Some(backend),
            _ => None,
        },
        devices: match event {
            OutputEvent::DevicesLoaded(devices) => Some(devices),
            _ => None,
        },
        output: match event {
            OutputEvent::Output(trace) => Some(trace),
            _ => None,
        },
        kind,
        sequence,
        transcript,
        message,
        injection,
        rms,
    }
}

impl EventSink for StdoutSink {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        self.sequence += 1;
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&json_event(&event, self.sequence, None))?
            );
            return Ok(());
        }
        match &event {
            OutputEvent::Transcript(snapshot) => {
                if snapshot.is_final {
                    self.preview.clear();
                    println!("{}", snapshot.authoritative_text);
                } else {
                    self.preview.render(&snapshot.authoritative_text);
                }
            }
            OutputEvent::Warning(message) => {
                self.preview.clear();
                eprintln!("warning: {message}");
            }
            OutputEvent::Error(message) => {
                self.preview.clear();
                eprintln!("{message}");
            }
            OutputEvent::Cancelled | OutputEvent::Completed => self.preview.clear(),
            _ => {}
        }
        Ok(())
    }
}

/// One redrawable stderr line, shared by standalone and delegated streams.
#[derive(Default)]
pub(crate) struct TerminalPreview {
    shown: bool,
}

impl TerminalPreview {
    pub(crate) fn render(&mut self, text: &str) {
        use std::io::IsTerminal;

        let mut stderr = io::stderr().lock();
        let terminal = stderr.is_terminal();
        let erase_line = if terminal { "\x1b[K" } else { "" };
        let _ = write!(stderr, "\r{erase_line}{text}");
        let _ = stderr.flush();
        self.shown = terminal && !text.is_empty();
    }

    pub(crate) fn clear(&mut self) {
        if std::mem::take(&mut self.shown) {
            self.render("");
        }
    }
}

impl Drop for TerminalPreview {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Automatic delivery uses the focused window from the start of the session.
/// Fcitx5 also binds its preview and commit to the original input context.
pub struct ConfiguredInjector {
    config: InjectConfig,
    /// Bridge client shared with the session's preedit sink, so the commit
    /// is ordered after the last preedit update.
    #[cfg(target_os = "linux")]
    fcitx5: Option<crate::fcitx5::Fcitx5Client>,
    focus: crate::desktop::focus::FocusReader,
    origin_window: Option<tokio::task::JoinHandle<Option<crate::desktop::focus::FocusedWindow>>>,
    original: Option<crate::desktop::focus::FocusedWindow>,
}

impl ConfiguredInjector {
    /// Capture the original target using the current desktop's focus facilities.
    pub fn new(config: InjectConfig) -> Self {
        Self::for_session(config, true)
    }

    fn for_session(config: InjectConfig, inject: bool) -> Self {
        Self::capture(config, crate::desktop::focus::desktop(), inject, false)
    }

    /// An injector that reads the focused window through `focus`: the
    /// desktop's tools, or a test's script.
    pub fn watching(config: InjectConfig, focus: crate::desktop::focus::FocusReader) -> Self {
        Self::capture(config, focus, true, false)
    }

    fn capture(
        config: InjectConfig,
        focus: crate::desktop::focus::FocusReader,
        inject: bool,
        track_app: bool,
    ) -> Self {
        let origin_window = (track_app || (inject && config.prefer != "copy"))
            .then(|| tokio::spawn(read_window(focus.clone())));
        #[cfg(target_os = "linux")]
        let fcitx5 = (inject && config.prefer == "fcitx5")
            .then(crate::fcitx5::Fcitx5Client::spawn_for_dictation);
        Self {
            config,
            #[cfg(target_os = "linux")]
            fcitx5,
            focus,
            origin_window,
            original: None,
        }
    }

    async fn read_original(&mut self) -> Option<&crate::desktop::focus::FocusedWindow> {
        if let Some(window) = self.origin_window.take() {
            self.original = window.await.ok().flatten();
        }
        self.original.as_ref()
    }
}

/// Read the focused window once within the desktop query budget.
async fn read_window(
    focus: crate::desktop::focus::FocusReader,
) -> Option<crate::desktop::focus::FocusedWindow> {
    tokio::time::timeout(Duration::from_secs(2), focus())
        .await
        .ok()
        .flatten()
}

const RESULT_KEPT: &str = "the text is kept for you to copy or type again";

struct WindowGuard<'a> {
    original: Option<&'a crate::desktop::focus::FocusedWindow>,
    focus: &'a crate::desktop::focus::FocusReader,
}

impl WindowGuard<'_> {
    fn refusal(&self, now: Option<&crate::desktop::focus::FocusedWindow>) -> Option<String> {
        match (self.original, now) {
            (Some(before), Some(now)) if before.source == now.source && before.key == now.key => {
                None
            }
            (Some(before), Some(now)) => Some(format!(
                "the focused window changed from {} to {}; {RESULT_KEPT}",
                before.describe(),
                now.describe()
            )),
            _ => Some(format!(
                "the original focused window could not be confirmed; {RESULT_KEPT}"
            )),
        }
    }

    #[cfg(target_os = "linux")]
    async fn recheck(&self) -> Option<String> {
        self.refusal(read_window(self.focus.clone()).await.as_ref())
    }
}

impl TextInjector for ConfiguredInjector {
    #[cfg(target_os = "linux")]
    async fn inject(&mut self, text: &str) -> InjectionResult {
        if self.config.prefer == "copy" {
            return copy_text(&self.config, text).await;
        }
        self.read_original().await;
        let original = &self.original;
        let now = read_window(self.focus.clone()).await;
        let guard = WindowGuard {
            original: original.as_ref(),
            focus: &self.focus,
        };
        // A checked Fcitx5 commit can identify the original input context on
        // desktops without a focused-window API. Multiline text still needs
        // a window to tell whether it is going to a terminal.
        let context_only = self.fcitx5.is_some()
            && original.is_none()
            && now.is_none()
            && !text.contains(['\r', '\n']);
        if !context_only {
            if let Some(reason) = guard.refusal(now.as_ref()) {
                return method_result(false, &self.config.prefer, &reason);
            }
        }
        let terminal = is_terminal(&self.config, now.as_ref());
        inject_text_with(
            &self.config,
            text,
            self.fcitx5.as_ref(),
            terminal,
            Some(&guard),
        )
        .await
    }

    #[cfg(not(target_os = "linux"))]
    async fn inject(&mut self, text: &str) -> InjectionResult {
        if self.config.prefer == "copy" {
            return copy_native(text).await;
        }
        let target = self.read_original().await;
        inject_native(text, target).await
    }
}

#[cfg(target_os = "linux")]
fn is_terminal(
    config: &InjectConfig,
    window: Option<&crate::desktop::focus::FocusedWindow>,
) -> bool {
    crate::desktop::focus::is_terminal(
        window.and_then(|window| window.app_id.as_deref()),
        &config.terminal_apps,
    )
}

/// An explicit repaste targets the newly focused field. Without an app id,
/// keep multiline text on the clipboard so the terminal cannot run its lines.
#[cfg(target_os = "linux")]
pub(crate) async fn repaste_text(config: &InjectConfig, text: &str) -> InjectionResult {
    let window = read_window(crate::desktop::focus::desktop()).await;
    if window
        .as_ref()
        .and_then(|window| window.app_id.as_deref())
        .is_none()
        && text.contains(['\r', '\n'])
    {
        return copy_text(config, text).await;
    }
    inject_text_with(
        config,
        text,
        None,
        is_terminal(config, window.as_ref()),
        None,
    )
    .await
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn repaste_text(config: &InjectConfig, text: &str) -> InjectionResult {
    if config.prefer == "copy" {
        return copy_native(text).await;
    }
    let target = read_window(crate::desktop::focus::desktop()).await;
    inject_native(text, target.as_ref()).await
}

#[cfg(not(target_os = "linux"))]
async fn copy_native(text: &str) -> InjectionResult {
    let text = text.to_owned();
    match tokio::task::spawn_blocking(move || crate::desktop::native::copy_text(&text)).await {
        Ok(Ok(())) => method_result(true, "copy", ""),
        Ok(Err(error)) => method_result(false, "copy", &error),
        Err(error) => method_result(false, "copy", &error.to_string()),
    }
}

#[cfg(not(target_os = "linux"))]
async fn inject_native(
    text: &str,
    target: Option<&crate::desktop::focus::FocusedWindow>,
) -> InjectionResult {
    let Some(target) = target else {
        return method_result(
            false,
            "clipboard",
            &format!("the original focused window could not be confirmed; {RESULT_KEPT}"),
        );
    };
    let text = text.to_owned();
    let key = target.key.clone();
    match tokio::task::spawn_blocking(move || crate::desktop::native::paste_text(&text, &key)).await
    {
        Ok(Ok(())) => method_result(true, "clipboard", ""),
        Ok(Err(error)) => method_result(false, "clipboard", &format!("{error}; {RESULT_KEPT}")),
        // A native call that failed after being dispatched may already have posted input.
        Err(error) => uncertain_result("clipboard", &error.to_string()),
    }
}

/// Inject `text` following the `[inject]` configuration: `prefer` selects
/// the primary method and `clipboard_fallback` retries through the
/// clipboard when the primary wtype or fcitx5 path fails. The clipboard
/// method copies the text with `wl-copy` and synthesizes a paste keypress,
/// so apps whose input method (Fcitx5) reinterprets typed keys receive the
/// text verbatim. The fcitx5 method commits through the Voiceless Fcitx5
/// bridge module, the same path as text typed with the input method. The
/// copy method only puts the text on the clipboard.
///
/// Clipboard fallback is only attempted when the primary action could not
/// have delivered. The explicit caller is responsible for its own focus.
#[cfg(target_os = "linux")]
pub async fn inject_text(config: &InjectConfig, text: &str) -> InjectionResult {
    inject_text_with(config, text, None, false, None).await
}

/// Deliver text after reading the explicitly targeted current window.
#[cfg(not(target_os = "linux"))]
pub async fn inject_text(config: &InjectConfig, text: &str) -> InjectionResult {
    repaste_text(config, text).await
}

/// Dispatch one delivery, with the original-window guard for automatic paths.
#[cfg(target_os = "linux")]
async fn inject_text_with(
    config: &InjectConfig,
    text: &str,
    fcitx5: Option<&crate::fcitx5::Fcitx5Client>,
    terminal: bool,
    guard: Option<&WindowGuard<'_>>,
) -> InjectionResult {
    let paste_args = if terminal {
        &config.terminal_paste_command
    } else {
        &config.paste_command
    };
    let as_paste = terminal && text.contains(['\r', '\n']);
    let result = match config.prefer.as_str() {
        "copy" => return copy_text(config, text).await,
        "clipboard" => return inject_via_clipboard(config, text, paste_args, guard).await,
        _ if as_paste => return inject_via_clipboard(config, text, paste_args, guard).await,
        "fcitx5" => match commit_via_fcitx5(text, fcitx5).await {
            Ok((ok, detail)) => method_result(ok, "fcitx5", if ok { "" } else { &detail }),
            Err(crate::fcitx5::BridgeError::Target(detail)) => {
                return method_result(false, "fcitx5", &detail)
            }
            Err(crate::fcitx5::BridgeError::Unconfirmed(detail)) => {
                return uncertain_result("fcitx5", &detail)
            }
            Err(crate::fcitx5::BridgeError::Unavailable(detail)) => {
                method_result(false, "fcitx5", &detail)
            }
        },
        _ => inject_via_wtype_with(config, text, guard).await,
    };
    if result.delivery == Delivery::NotDelivered && config.clipboard_fallback {
        let fallback = inject_via_clipboard(config, text, paste_args, guard).await;
        if fallback.delivery != Delivery::NotDelivered {
            return fallback;
        }
    }
    result
}

/// Reuse the session's ordered bridge channel when one is already attached.
#[cfg(target_os = "linux")]
async fn commit_via_fcitx5(
    text: &str,
    client: Option<&crate::fcitx5::Fcitx5Client>,
) -> crate::fcitx5::Reply {
    let owned;
    let client = match client {
        Some(client) => client,
        None => {
            owned = crate::fcitx5::Fcitx5Client::spawn();
            &owned
        }
    };
    client.commit(text).await
}

/// Inject text through `wtype` with default settings. Newlines are typed as
/// Shift+Enter so a chat input breaks the line without submitting.
#[cfg(target_os = "linux")]
pub async fn inject_via_wtype(text: &str) -> InjectionResult {
    inject_via_wtype_with(&InjectConfig::default(), text, None).await
}

/// A failed later invocation may have written a prefix, so do not replay it.
#[cfg(target_os = "linux")]
async fn inject_via_wtype_with(
    config: &InjectConfig,
    text: &str,
    guard: Option<&WindowGuard<'_>>,
) -> InjectionResult {
    let limit = Duration::from_secs_f64(config.timeout_seconds.max(0.1));
    for (index, args) in wtype_invocations(text).into_iter().enumerate() {
        if index > 0 {
            if let Some(guard) = guard {
                if let Some(reason) = guard.recheck().await {
                    return uncertain_result("wtype", &reason);
                }
            }
        }
        if let Err(error) = run_command(&config.wtype_command, &args, limit).await {
            return if index > 0 || error.started {
                uncertain_result("wtype", &error.message)
            } else {
                method_result(false, "wtype", &error.message)
            };
        }
    }
    method_result(true, "wtype", "")
}

/// Put `text` on the Wayland clipboard and leave it there for the user to
/// paste; the selection that was there is replaced.
#[cfg(target_os = "linux")]
async fn copy_text(config: &InjectConfig, text: &str) -> InjectionResult {
    let limit = Duration::from_secs_f64(config.timeout_seconds.max(0.1));
    // wl-copy forks a selection owner that may keep inherited pipes open.
    match run_command_with_stdin(&config.wl_copy_command, &[], text, false, limit).await {
        Ok(()) => method_result(true, "copy", ""),
        Err(error) => method_result(false, "copy", &error.message),
    }
}

/// Inject text via the clipboard without clobbering the user's clipboard:
/// save the previous Wayland (and X11, when mirrored) selection, copy the
/// text with `wl-copy`, run `focus_command` (if set) to raise the target
/// window, synthesize the paste keypress `paste_command` with `paste_tool`,
/// then restore the previous selection after the paste has been consumed.
#[cfg(target_os = "linux")]
async fn inject_via_clipboard(
    config: &InjectConfig,
    text: &str,
    paste_command: &str,
    guard: Option<&WindowGuard<'_>>,
) -> InjectionResult {
    // Apply the same subprocess budget to the paste shortcut as to typing.
    let limit = Duration::from_secs_f64(config.timeout_seconds.max(0.1));

    let prev_wayland = capture_stdout(&wayland_paste_command(&config.wl_copy_command), limit).await;
    let prev_x11 = match x11_read_command(&config.x11_clipboard_command) {
        Some(command) => capture_stdout(&command, limit).await,
        None => None,
    };
    if let Some(guard) = guard {
        if let Some(reason) = guard.recheck().await {
            return method_result(false, "clipboard", &reason);
        }
    }
    if let Err(error) =
        run_command_with_stdin(&config.wl_copy_command, &[], text, false, limit).await
    {
        return method_result(false, "clipboard", &error.message);
    }
    if !config.x11_clipboard_command.trim().is_empty() {
        // Best effort: XWayland apps (WeChat) read the X11 clipboard, which
        // rootless Xwayland does not forward from Wayland. A failure here
        // must not fail the injection.
        let _ = run_command_with_stdin(
            &config.x11_clipboard_command,
            &[],
            text,
            false,
            Duration::from_secs(1),
        )
        .await;
    }
    if !config.focus_command.trim().is_empty() {
        // Best effort: raise the target window (e.g. WeChat) before pasting so
        // the text lands there even when another window is currently focused.
        let _ = run_command(&config.focus_command, &[], Duration::from_secs(1)).await;
    }
    let focus_error = match guard {
        Some(guard) => guard.recheck().await,
        None => None,
    };
    let pasted = if let Some(reason) = focus_error {
        method_result(false, "clipboard", &reason)
    } else {
        let mut paste_args: Vec<&str> = paste_command.split_whitespace().collect();
        // GTK 3 omits the last keycode when looking up shortcut bindings. Give
        // wtype's synthetic map a key after V, including for saved defaults.
        if std::path::Path::new(&config.paste_tool).file_name() == Some("wtype".as_ref())
            && matches!(
                paste_args.as_slice(),
                ["-M", "ctrl", "-k", "v"] | ["-M", "ctrl", "-M", "shift", "-k", "v"]
            )
        {
            paste_args.extend(["-p", "Control_L"]);
        }
        match run_command(&config.paste_tool, &paste_args, limit).await {
            Ok(()) => method_result(true, "clipboard", ""),
            Err(error) if error.started => uncertain_result("clipboard", &error.message),
            Err(error) => method_result(false, "clipboard", &error.message),
        }
    };

    // Let the focused app consume the selection, then restore the user's
    // previous clipboard so injection does not leave the transcript behind.
    if pasted.delivery == Delivery::Unconfirmed {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    restore_clipboard(&config.wl_copy_command, prev_wayland, limit).await;
    if !config.x11_clipboard_command.trim().is_empty() {
        restore_clipboard(&config.x11_clipboard_command, prev_x11, limit).await;
    }
    pasted
}

/// Derive the `wl-paste` command from the configured `wl-copy` command, so a
/// custom `wl_copy_command` path keeps its paired reader.
#[cfg(target_os = "linux")]
fn wayland_paste_command(copy_command: &str) -> String {
    match copy_command.strip_suffix("wl-copy") {
        Some(prefix) => format!("{prefix}wl-paste --no-newline"),
        None => "wl-paste --no-newline".to_owned(),
    }
}

/// Put `previous` back with the same writer command (`wl-copy` or the X11
/// writer). An unreadable or empty previous selection is not restored, so
/// the transcript then stays in the clipboard.
#[cfg(target_os = "linux")]
async fn restore_clipboard(command: &str, previous: Option<String>, limit: Duration) {
    // Nothing readable before injection: leave the selection as-is.
    let Some(previous) = previous else {
        return;
    };
    let _ = run_command_with_stdin(command, &[], &previous, false, limit).await;
}

/// Run a command and return its trimmed stdout on success. Best effort; used
/// to snapshot the clipboard before injection overwrites it.
#[cfg(target_os = "linux")]
async fn capture_stdout(command: &str, limit: Duration) -> Option<String> {
    let mut parts = command.split_whitespace();
    let program = parts.next()?;
    let child = Command::new(program)
        .args(parts)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    match timeout(limit, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

/// Derive the X11 would-be reader from the configured writer command, so the
/// previous X11 selection can be snapshotted when an input command is set.
/// Returns `None` when the command is empty or has no recognizable word-swap.
#[cfg(target_os = "linux")]
fn x11_read_command(input_command: &str) -> Option<String> {
    let trimmed = input_command.trim();
    if trimmed.is_empty() {
        return None;
    }
    let swapped = trimmed
        .replace(" --input", " --output")
        .replace(" -i", " -o");
    Some(swapped)
}

/// Build the `wtype` invocations for `text`: each line is typed literally and
/// each newline becomes a Shift+Enter keypress.
#[cfg(target_os = "linux")]
pub fn wtype_invocations(text: &str) -> Vec<Vec<&str>> {
    let mut invocations = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            invocations.push(vec!["-M", "shift", "-k", "Return"]);
        }
        invocations.push(vec!["--", line]);
    }
    invocations
}

#[cfg(target_os = "linux")]
struct CommandFailure {
    started: bool,
    message: String,
}

/// Run `command` with `args` and no stdin, capturing stderr for the error
/// message.
#[cfg(target_os = "linux")]
async fn run_command(command: &str, args: &[&str], limit: Duration) -> Result<(), CommandFailure> {
    run_command_with_stdin(command, args, "", true, limit).await
}

/// A failed spawn guarantees that the tool did nothing. After a successful
/// spawn it may already have issued input, even if it exits with an error.
#[cfg(target_os = "linux")]
async fn run_command_with_stdin(
    command: &str,
    args: &[&str],
    stdin_text: &str,
    piped_stderr: bool,
    limit: Duration,
) -> Result<(), CommandFailure> {
    let mut parts = command.split_whitespace();
    let program = match parts.next().filter(|name| !name.is_empty()) {
        Some(name) => name,
        None => {
            return Err(CommandFailure {
                started: false,
                message: "empty command".to_owned(),
            })
        }
    };
    let mut child = Command::new(program)
        .args(parts.chain(args.iter().copied()))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(if piped_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // On timeout the wait future (and the child handle) is dropped:
        // kill the tool instead of leaving a hung wtype/paste process.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| CommandFailure {
            started: false,
            message: format!("{program}: {error}"),
        })?;
    let output = timeout(limit, async {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(stdin_text.as_bytes()).await?;
        }
        child.wait_with_output().await
    })
    .await;
    let message = match output {
        Ok(Ok(output)) if output.status.success() => return Ok(()),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if stderr.is_empty() {
                format!("{command} failed")
            } else {
                stderr
            }
        }
        Ok(Err(error)) => format!("{command}: {error}"),
        Err(_) => format!("{command} timed out"),
    };
    Err(CommandFailure {
        started: true,
        message,
    })
}

/// Build an `InjectionResult` for `method`; how much of the delivery is
/// known follows from the method.
fn method_result(ok: bool, method: &str, message: &str) -> InjectionResult {
    let delivery = match (ok, method) {
        (true, "fcitx5") => Delivery::Committed,
        (true, "copy") => Delivery::Copied,
        (true, _) => Delivery::Unconfirmed,
        (false, _) => Delivery::NotDelivered,
    };
    InjectionResult {
        ok,
        method: method.to_owned(),
        message: message.to_owned(),
        delivery,
    }
}

fn uncertain_result(method: &str, message: &str) -> InjectionResult {
    InjectionResult {
        ok: false,
        method: method.to_owned(),
        message: format!("delivery could not be confirmed ({message}); {RESULT_KEPT}"),
        delivery: Delivery::Unconfirmed,
    }
}
