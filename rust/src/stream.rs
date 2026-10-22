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
//! Order of operations: the mode, processing provider and text cleaner are
//! resolved before any backend opens, and the backend opens (with at most
//! one fallback) before any capture starts, so configuration errors never
//! produce audio. Backend-open failures that are configuration errors
//! (`StreamError`) abort the command: `main.rs` prints `Voiceless: <error>`
//! and exits 1. A missing or corrupt local model is instead reported as
//! `error` + `completed` events (exit code 1) so `--json` consumers and
//! overlays see it.
//!
//! `stream --json` prints one `JsonEvent` line per `OutputEvent`
//! (`json_event`); the daemon broadcasts the same lines as D-Bus `Event`
//! signals, which `voiceless ctl watch` and the console relay.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::capture::RawCapture;
use crate::config::{AppConfig, ConfigError, InjectConfig};
use crate::coordinator::{
    AudioCapture, BackendTransport, ControlCommand, EventSink, HistoryEntry, HistoryRecorder,
    InjectionResult, OutputEvent, SessionPlan, TextInjector, TextProcessor,
};
use crate::realtime::{RealtimeEvent, RealtimeSession};
use crate::session::TranscriptSnapshot;

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
struct JsonEvent<'a> {
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

/// What a resident daemon lends a session: a warm engine and the event
/// broadcast that replaces stdout.
#[derive(Default, Clone)]
pub struct SessionEnv {
    /// Engine the daemon loaded at start; `None` makes `local-offline`
    /// load its model cold inside the session.
    pub warm: Option<std::sync::Arc<crate::engine::WarmEngine>>,
    /// Daemon event broadcast (JSON lines). When set, events go there
    /// through `BroadcastSink` instead of stdout, and level events are on.
    pub events: Option<tokio::sync::broadcast::Sender<String>>,
}

impl SessionEnv {
    /// The event sink for this environment: the broadcast when lent by the
    /// daemon, otherwise stdout (`json` selects the line format).
    fn sink(&self, json: bool) -> Box<dyn EventSink + Send> {
        match &self.events {
            Some(events) => Box::new(BroadcastSink {
                events: events.clone(),
                sequence: 0,
            }),
            None => Box::new(StdoutSink::new(json)),
        }
    }
}

/// Full session setup and run; see the module doc for the order. Returns
/// `Ok(code)` once a session ran, or a model error was reported as events,
/// and `Err` for configuration and connection errors found before capture.
pub async fn run_with_env(
    options: StreamOptions,
    control: tokio::sync::mpsc::Receiver<ControlCommand>,
    env: SessionEnv,
) -> Result<i32, StreamError> {
    let mut config = AppConfig::load_optional(options.config_path.as_deref())?;
    // Round-trip the overridden config through TOML so `--backend` gets
    // the same validation (allowed names, cross-field rules) as a file.
    if let Some(backend) = options.backend.as_deref() {
        config.asr.streaming_backend = backend.to_owned();
        config = AppConfig::from_toml(&toml::to_string(&config).map_err(|error| {
            ConfigError::Invalid(format!("cannot apply backend override: {error}"))
        })?)?;
    }
    let pipeline = build_pipeline(&config, &options)?;
    let mut sink = env.sink(options.json);
    let mut injector = ConfiguredInjector::new(config.inject.clone());
    if options.inject && config.inject.prefer == "fcitx5" {
        // One ordered bridge channel for the live preedit and the commit.
        let client = crate::fcitx5::Fcitx5Client::spawn();
        if config.inject.preedit {
            sink = Box::new(crate::fcitx5::PreeditSink::new(sink, client.clone()));
        }
        injector = injector.with_fcitx5(client);
    }
    // At most one fallback, and only to a different backend; its warning
    // precedes `ready` in the event stream so consumers learn the switch.
    let primary = config.asr.streaming_backend.clone();
    let opened = match open_transport(&primary, &config, &env).await {
        Ok(transport) => Ok((primary, transport)),
        Err(error) => match config.asr.fallback_backend.clone() {
            Some(fallback) if fallback != primary => {
                let _ = sink.emit(OutputEvent::Warning(format!(
                    "{primary} unavailable ({error}); falling back to {fallback}"
                )));
                open_transport(&fallback, &config, &env)
                    .await
                    .map(|transport| (fallback, transport))
            }
            _ => Err(error),
        },
    };
    let (backend, transport) = match opened {
        Ok(opened) => opened,
        Err(OpenError::Stream(error)) => return Err(error),
        // Model problems are session events (exit 1 after error/completed),
        // not a command error: overlays and `--json` readers must see them.
        Err(OpenError::Model(message)) => {
            let _ = sink.emit(OutputEvent::Error(message));
            let _ = sink.emit(OutputEvent::Completed);
            return Ok(1);
        }
    };
    let mut plan = session_plan(&config, &options, &backend, &pipeline.mode_id);
    // Live UIs (daemon console, overlay) draw a mic level meter.
    plan.emit_levels = env.events.is_some();
    if backend == "local-offline" {
        // The final waits for the last segment's decode, and for the model
        // load when the utterance is shorter than it.
        plan.finish_timeout = Duration::from_secs(20);
    }
    let code = crate::coordinator::run_session(
        plan,
        PwCapture::default(),
        transport,
        pipeline.processor,
        injector,
        pipeline.history,
        control,
        &mut sink,
    )
    .await;
    Ok(code)
}

/// Why a backend could not open. `Stream` errors abort the command;
/// `Model` errors (missing or broken local models) are reported as session
/// events so overlays and `--json` consumers see them.
enum OpenError {
    Stream(StreamError),
    Model(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Stream(error) => error.fmt(f),
            OpenError::Model(message) => f.write_str(message),
        }
    }
}

impl From<StreamError> for OpenError {
    fn from(error: StreamError) -> Self {
        OpenError::Stream(error)
    }
}

/// Connect or load the transport for one backend, before any capture.
async fn open_transport(
    backend: &str,
    config: &AppConfig,
    env: &SessionEnv,
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
            let session = crate::doubao::DoubaoSession::connect(&config.doubao_engine())
                .await
                .map_err(StreamError::from)?;
            Ok(AnyTransport::Doubao(DoubaoTransport(session)))
        }
        "local-streaming" => {
            // An explicit `local_model_dir` is used as-is; the managed model
            // is re-verified against its manifest on every open.
            let model_dir = match config.asr.local_model_dir.clone() {
                Some(path) => path,
                None => resolve_managed_model(&crate::models::streaming_paraformer_spec())
                    .map_err(OpenError::Model)?,
            };
            let recognizer = crate::local_asr::LocalStreamingRecognizer::new(&model_dir)
                .map_err(|error| OpenError::Model(error.to_string()))?;
            Ok(AnyTransport::Local(LocalTransport::new(recognizer)))
        }
        "local-offline" => open_local_offline(config, env).map(AnyTransport::Offline),
        value => Err(StreamError::UnsupportedBackend(value.to_owned()).into()),
    }
}

/// Build the `local-offline` transport: reuse the daemon's warm decoder
/// when `[local]` still matches it, otherwise resolve and verify the model
/// and the Silero VAD from the catalog and load them on the worker thread.
/// Never blocks on the load itself.
fn open_local_offline(
    config: &AppConfig,
    env: &SessionEnv,
) -> Result<OfflineTransportAdapter, OpenError> {
    let local = config.local_engine();
    // A warm engine was verified at daemon start: skip the per-session hash
    // check of ~1-2 GB of model files, which would delay capture.
    let warm = env
        .warm
        .as_ref()
        .and_then(|engine| engine.offline_for(&local));
    let (decoder, model_dir, vad_model) = match warm {
        Some((decoder, vad_model)) => (Some(decoder), None, vad_model),
        None => {
            let spec = crate::models::catalog()
                .into_iter()
                .find(|spec| spec.id == local.model)
                .ok_or_else(|| OpenError::Model(format!("unknown model id: {}", local.model)))?;
            let model_dir = resolve_managed_model(&spec).map_err(OpenError::Model)?;
            let vad_dir = resolve_managed_model(&crate::models::silero_vad_spec())
                .map_err(OpenError::Model)?;
            (None, Some(model_dir), vad_dir.join("silero_vad.onnx"))
        }
    };
    // Without a warm engine the model loads on the worker, so capture
    // starts at once either way.
    let transport = crate::offline_asr::OfflineTransport::start(move || {
        use crate::offline_asr::{create_decoder, create_vad, OfflineFamily};
        let decoder = match (decoder, model_dir) {
            (Some(decoder), _) => decoder,
            (None, Some(model_dir)) => create_decoder(
                OfflineFamily::for_model(&local.model)?,
                &model_dir,
                &crate::engine::offline_options(&local),
            )?,
            (None, None) => unreachable!("cold sessions resolve the model directory"),
        };
        let vad = create_vad(&vad_model, local.segment_limit())?;
        Ok(crate::offline_asr::SegmentedRecognizer::new(
            decoder,
            vad,
            Duration::from_millis(local.partial_interval_ms),
        ))
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
    processor: PipelineProcessor,
    history: SqliteHistory,
}

/// Build the final-text pipeline once, before any capture starts: unknown
/// modes and invalid processing config are startup errors.
fn build_pipeline(config: &AppConfig, options: &StreamOptions) -> Result<Pipeline, StreamError> {
    let repository = crate::modes::ModesRepository::open(modes_path())
        .map_err(|error| StreamError::Capture(error.to_string()))?;
    let mode = repository
        .resolve(Some(&options.mode))
        .map_err(|error| StreamError::Capture(error.to_string()))?;
    let mode_id = mode.id.clone();

    let chat = crate::processing::from_config(&config.processing)
        .map_err(|error| StreamError::Capture(error.to_string()))?;

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

    let prompts = crate::modes::prompt_table(repository.list(), &config.processing.prompt);

    Ok(Pipeline {
        mode_id: mode_id.clone(),
        processor: PipelineProcessor {
            prompts,
            chat,
            cleaner: crate::text::TextCleaner::new(&config.text)
                .map_err(|error| StreamError::Config(ConfigError::Invalid(error)))?,
        },
        history: SqliteHistory {
            store,
            audio_dir: audio_dir(config),
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

/// Production `TextProcessor`: `[text]` cleanup, then the mode's prompt
/// rendered around the text and sent to the configured chat provider.
struct PipelineProcessor {
    /// `(mode id, prompt template)` pairs from `modes::prompt_table`.
    prompts: Vec<(String, String)>,
    /// `None` when `[processing]` configures no provider.
    chat: Option<crate::processing::ChatProcessor<crate::processing::UreqPoster>>,
    cleaner: crate::text::TextCleaner,
}

impl TextProcessor for PipelineProcessor {
    fn cleanup(&mut self, text: &str) -> String {
        self.cleaner.clean(text)
    }

    async fn process(&mut self, mode_id: &str, text: &str) -> Result<String, String> {
        // A mode without a prompt in the table sends the transcript itself.
        let prompt = self
            .prompts
            .iter()
            .find(|(id, _)| id == mode_id)
            .map(|(_, prompt)| crate::modes::render_template(prompt, text, "", ""))
            .unwrap_or_else(|| text.to_owned());
        let Some(chat) = self.chat.clone() else {
            return Err("no text processing provider configured".to_owned());
        };
        // The HTTP client is blocking (ureq); keep it off the runtime thread.
        tokio::task::spawn_blocking(move || chat.process(&prompt))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
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
}

impl HistoryRecorder for SqliteHistory {
    fn record(&mut self, entry: HistoryEntry) {
        let Some(store) = &self.store else {
            // History disabled: never keep orphan recordings behind.
            remove_audio(entry.audio_path.as_deref(), self.audio_dir.as_deref());
            return;
        };
        let backend = entry.backend.clone();
        match store.insert(&entry, &backend) {
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
    plan.save_audio_dir = audio_dir(config);
    plan
}

/// Control channel fed by process signals: the first SIGINT sends `Stop`,
/// every later SIGINT sends `Cancel`, SIGTERM sends `Cancel` and ends the
/// task. Also used by `ctl::stream_via_daemon`, which relays the same
/// commands to the daemon over D-Bus.
pub(crate) fn control_receiver() -> tokio::sync::mpsc::Receiver<ControlCommand> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut first_stop = false;
        // The signal streams are re-created on every iteration.
        loop {
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("install SIGINT handler");
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {
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
                _ = sigterm.recv() => {
                    let _ = tx.send(ControlCommand::Cancel).await;
                    break;
                }
            }
        }
    });
    rx
}

/// pw-record capture adapted to the coordinator boundary. Construction is
/// deferred to `start` so the ready gate can fail before any capture exists.
#[derive(Default)]
struct PwCapture {
    inner: Option<RawCapture>,
}

impl AudioCapture for PwCapture {
    async fn start(&mut self) -> io::Result<()> {
        let capture = RawCapture::start()?;
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

    fn abort(&mut self) {
        // Dropping the child kills it (kill_on_drop) without draining.
        self.inner = None;
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
}

impl StdoutSink {
    fn new(json: bool) -> Self {
        Self { json, sequence: 0 }
    }
}

/// Lets `run_with_env` stack sinks (`PreeditSink` over the base sink)
/// behind one trait object.
impl<T: EventSink + ?Sized> EventSink for Box<T> {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        (**self).emit(event)
    }
}

/// Daemon sink: every event as one JSON line (the `stream --json` schema)
/// on the broadcast channel behind `voiceless ctl watch`.
struct BroadcastSink {
    events: tokio::sync::broadcast::Sender<String>,
    sequence: u64,
}

impl EventSink for BroadcastSink {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        self.sequence += 1;
        let line = serde_json::to_string(&json_event(&event, self.sequence))?;
        // No subscriber is fine: nobody is watching.
        let _ = self.events.send(line);
        Ok(())
    }
}

/// Map an `OutputEvent` to its JSON line; the `type` names are listed on
/// `JsonEvent` and on each `OutputEvent` variant.
fn json_event(event: &OutputEvent, sequence: u64) -> JsonEvent<'_> {
    let (kind, transcript, message, injection, rms) = match event {
        OutputEvent::Ready => ("ready", None, None, None, None),
        OutputEvent::Level(rms) => ("level", None, None, None, Some(*rms)),
        OutputEvent::Transcript(snapshot) => ("transcript", Some(snapshot), None, None, None),
        OutputEvent::Finalized { injection } => ("finalized", None, None, injection.as_ref(), None),
        OutputEvent::Warning(message) => ("warning", None, Some(message.as_str()), None, None),
        OutputEvent::Error(message) => ("error", None, Some(message.as_str()), None, None),
        OutputEvent::Cancelled => ("cancelled", None, None, None, None),
        OutputEvent::Completed => ("completed", None, None, None, None),
    };
    JsonEvent {
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
                serde_json::to_string(&json_event(&event, self.sequence))?
            );
            return Ok(());
        }
        match &event {
            OutputEvent::Transcript(snapshot) => {
                if snapshot.is_final {
                    println!("{}", snapshot.authoritative_text);
                } else {
                    eprint!("\r{}", snapshot.authoritative_text);
                    io::stderr().flush().ok();
                }
            }
            OutputEvent::Warning(message) => eprintln!("warning: {message}"),
            OutputEvent::Error(message) => eprintln!("{message}"),
            _ => {}
        }
        Ok(())
    }
}

/// Config-driven injector: selects the wtype, clipboard or fcitx5 method
/// from `[inject].prefer` and applies the clipboard fallback when the
/// primary method fails (see `inject_text` for the rules).
pub struct ConfiguredInjector {
    config: InjectConfig,
    /// Bridge client shared with the session's preedit sink, so the commit
    /// is ordered after the last preedit update.
    fcitx5: Option<crate::fcitx5::Fcitx5Client>,
}

impl ConfiguredInjector {
    /// Injector without a bridge client; the `fcitx5` method then spawns a
    /// client per injection.
    pub fn new(config: InjectConfig) -> Self {
        Self {
            config,
            fcitx5: None,
        }
    }

    /// Commit through `client`, the same ordered channel the preedit uses.
    pub fn with_fcitx5(mut self, client: crate::fcitx5::Fcitx5Client) -> Self {
        self.fcitx5 = Some(client);
        self
    }
}

impl TextInjector for ConfiguredInjector {
    async fn inject(&mut self, text: &str) -> InjectionResult {
        inject_text_with(&self.config, text, self.fcitx5.as_ref()).await
    }
}

/// Inject `text` following the `[inject]` configuration: `prefer` selects
/// the primary method and `clipboard_fallback` retries through the
/// clipboard when the primary wtype or fcitx5 path fails. The clipboard
/// method copies the text with `wl-copy` and synthesizes a paste keypress,
/// so apps whose input method (Fcitx5) reinterprets typed keys receive the
/// text verbatim. The fcitx5 method commits through the Voiceless Fcitx5
/// bridge module, the same path as text typed with the input method.
///
/// Rules: `prefer = "clipboard"` never falls back. When the fallback runs
/// and succeeds, its result (`method: "clipboard"`) is returned; when it
/// fails too, the primary method's failure is returned. Used directly by
/// `transcribe --inject` and the latency benchmark; sessions go through
/// `ConfiguredInjector`.
pub async fn inject_text(config: &InjectConfig, text: &str) -> InjectionResult {
    inject_text_with(config, text, None).await
}

/// Shared implementation of `inject_text`; `fcitx5` reuses the session's
/// bridge client when given so the commit queues after its preedit.
async fn inject_text_with(
    config: &InjectConfig,
    text: &str,
    fcitx5: Option<&crate::fcitx5::Fcitx5Client>,
) -> InjectionResult {
    let prefer_clipboard = config.prefer == "clipboard";
    let result = match config.prefer.as_str() {
        "clipboard" => inject_via_clipboard(config, text).await,
        "fcitx5" => inject_via_fcitx5(text, fcitx5).await,
        _ => inject_via_wtype_with(config, text).await,
    };
    if !result.ok && config.clipboard_fallback && !prefer_clipboard {
        let fallback = inject_via_clipboard(config, text).await;
        if fallback.ok {
            return fallback;
        }
    }
    result
}

/// Commit text into the focused input field through the Fcitx5 bridge
/// module (`fcitx5-bridge/`). Fails when Fcitx5 or the module is not
/// running, or when no input context has focus.
async fn inject_via_fcitx5(
    text: &str,
    client: Option<&crate::fcitx5::Fcitx5Client>,
) -> InjectionResult {
    let owned;
    let client = match client {
        Some(client) => client,
        None => {
            owned = crate::fcitx5::Fcitx5Client::spawn();
            &owned
        }
    };
    match client.commit(text).await {
        Ok((true, _)) => method_result(true, "fcitx5", ""),
        // The bridge answered but could not commit (no focused context).
        Ok((false, reason)) => method_result(false, "fcitx5", &reason),
        Err(error) => method_result(false, "fcitx5", &error),
    }
}

/// Inject text through `wtype` with default settings. Newlines are typed as
/// Shift+Enter so a chat input breaks the line without submitting.
pub async fn inject_via_wtype(text: &str) -> InjectionResult {
    inject_via_wtype_with(&InjectConfig::default(), text).await
}

/// One `wtype` process per invocation from `wtype_invocations`, 1 s each.
/// The first failure aborts the rest, so part of the text may already have
/// been typed when the clipboard fallback runs.
async fn inject_via_wtype_with(config: &InjectConfig, text: &str) -> InjectionResult {
    for args in wtype_invocations(text) {
        let result = run_command(&config.wtype_command, &args, Duration::from_secs(1)).await;
        if let Err(message) = result {
            return method_result(false, "wtype", &message);
        }
    }
    method_result(true, "wtype", "")
}

/// Inject text via the clipboard without clobbering the user's clipboard:
/// save the previous Wayland (and X11, when mirrored) selection, copy the
/// text with `wl-copy`, run `focus_command` (if set) to raise the target
/// window, synthesize the paste keypress with `paste_tool`, then restore the
/// previous selection after the paste has been consumed.
async fn inject_via_clipboard(config: &InjectConfig, text: &str) -> InjectionResult {
    // `timeout_seconds` bounds the clipboard reads/writes; the paste, focus
    // and X11 mirror steps use a fixed 1 s each.
    let limit = Duration::from_secs_f64(config.timeout_seconds.max(0.1));

    let prev_wayland = capture_stdout(&wayland_paste_command(&config.wl_copy_command), limit).await;
    let prev_x11 = match x11_read_command(&config.x11_clipboard_command) {
        Some(command) => capture_stdout(&command, limit).await,
        None => None,
    };
    if let Err(message) =
        run_command_with_stdin(&config.wl_copy_command, &[], text, false, limit).await
    {
        return method_result(false, "clipboard", &message);
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
    let paste_args: Vec<&str> = config.paste_command.split_whitespace().collect();
    let pasted = match run_command(&config.paste_tool, &paste_args, Duration::from_secs(1)).await {
        Ok(()) => method_result(true, "clipboard", ""),
        Err(message) => method_result(false, "clipboard", &message),
    };

    // Let the focused app consume the selection, then restore the user's
    // previous clipboard so injection does not leave the transcript behind.
    tokio::time::sleep(Duration::from_millis(300)).await;
    restore_clipboard(&config.wl_copy_command, prev_wayland, limit).await;
    if !config.x11_clipboard_command.trim().is_empty() {
        restore_clipboard(&config.x11_clipboard_command, prev_x11, limit).await;
    }
    pasted
}

/// Derive the `wl-paste` command from the configured `wl-copy` command, so a
/// custom `wl_copy_command` path keeps its paired reader.
fn wayland_paste_command(copy_command: &str) -> String {
    match copy_command.strip_suffix("wl-copy") {
        Some(prefix) => format!("{prefix}wl-paste"),
        None => "wl-paste".to_owned(),
    }
}

/// Put `previous` back with the same writer command (`wl-copy` or the X11
/// writer). An unreadable or empty previous selection is not restored, so
/// the transcript then stays in the clipboard.
async fn restore_clipboard(command: &str, previous: Option<String>, limit: Duration) {
    // Nothing readable before injection: leave the selection as-is.
    let Some(previous) = previous else {
        return;
    };
    let _ = run_command_with_stdin(command, &[], &previous, false, limit).await;
}

/// Run a command and return its trimmed stdout on success. Best effort; used
/// to snapshot the clipboard before injection overwrites it.
async fn capture_stdout(command: &str, limit: Duration) -> Option<String> {
    let mut parts = command.split_whitespace();
    let program = parts.next()?;
    let child = Command::new(program)
        .args(parts)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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

/// Run `command` with `args` and no stdin, capturing stderr for the error
/// message.
async fn run_command(command: &str, args: &[&str], limit: Duration) -> Result<(), String> {
    run_command_with_stdin(command, args, "", true, limit).await
}

/// Spawn `command` split on whitespace (no shell, no quoting) followed by
/// `args`, write `stdin_text` to its stdin, and wait up to `limit`. A
/// non-zero exit reports the trimmed stderr (when `piped_stderr`) or
/// `<command> failed`; a timeout reports `<command> timed out` and
/// abandons the wait without killing the child.
async fn run_command_with_stdin(
    command: &str,
    args: &[&str],
    stdin_text: &str,
    piped_stderr: bool,
    limit: Duration,
) -> Result<(), String> {
    let mut parts = command.split_whitespace();
    let program = match parts.next().filter(|name| !name.is_empty()) {
        Some(name) => name,
        None => return Err("empty command".to_owned()),
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
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(stdin_text.as_bytes()).await;
    }
    // Close stdin so tools like wl-copy see EOF and return.
    child.stdin.take();
    match timeout(limit, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Err(if stderr.is_empty() {
                format!("{command} failed")
            } else {
                stderr
            })
        }
        Ok(Err(error)) => Err(format!("{command}: {error}")),
        Err(_) => Err(format!("{command} timed out")),
    }
}

/// Build an `InjectionResult` for `method`.
fn method_result(ok: bool, method: &str, message: &str) -> InjectionResult {
    InjectionResult {
        ok,
        method: method.to_owned(),
        message: message.to_owned(),
    }
}
