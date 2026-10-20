//! Production wiring: config -> coordinator with pw-record capture,
//! cloud/local transports, stdout/stderr event sink and configurable
//! text injection (wtype or clipboard).

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

#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub config_path: Option<PathBuf>,
    pub backend: Option<String>,
    pub json: bool,
    pub inject: bool,
    pub mode: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("stream backend {0:?} is not implemented yet")]
    UnsupportedBackend(String),
    #[error("cloud realtime API key is missing")]
    MissingApiKey,
    #[error("doubao credentials are missing: set doubao.api_key, or doubao.app_key and doubao.access_key")]
    MissingDoubaoCredentials,
    #[error("realtime session: {0}")]
    Realtime(#[from] io::Error),
    #[error("capture: {0}")]
    Capture(String),
}

#[derive(Debug, Serialize)]
struct JsonEvent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    sequence: u64,
    transcript: Option<&'a TranscriptSnapshot>,
    message: Option<&'a str>,
    injection: Option<&'a InjectionResult>,
}

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
    pub warm: Option<std::sync::Arc<crate::engine::WarmEngine>>,
    pub events: Option<tokio::sync::broadcast::Sender<String>>,
}

impl SessionEnv {
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

pub async fn run_with_env(
    options: StreamOptions,
    control: tokio::sync::mpsc::Receiver<ControlCommand>,
    env: SessionEnv,
) -> Result<i32, StreamError> {
    let mut config = AppConfig::load_optional(options.config_path.as_deref())?;
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
        Err(OpenError::Model(message)) => {
            let _ = sink.emit(OutputEvent::Error(message));
            let _ = sink.emit(OutputEvent::Completed);
            return Ok(1);
        }
    };
    let mut plan = session_plan(&config, &options, &backend, &pipeline.mode_id);
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

struct Pipeline {
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

fn modes_path() -> std::path::PathBuf {
    let root = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned()))
                .join(".config")
        });
    root.join("voiceless").join("modes.json")
}

struct PipelineProcessor {
    prompts: Vec<(String, String)>,
    chat: Option<crate::processing::ChatProcessor<crate::processing::UreqPoster>>,
    cleaner: crate::text::TextCleaner,
}

impl TextProcessor for PipelineProcessor {
    fn cleanup(&mut self, text: &str) -> String {
        self.cleaner.clean(text)
    }

    async fn process(&mut self, mode_id: &str, text: &str) -> Result<String, String> {
        let prompt = self
            .prompts
            .iter()
            .find(|(id, _)| id == mode_id)
            .map(|(_, prompt)| crate::modes::render_template(prompt, text, "", ""))
            .unwrap_or_else(|| text.to_owned());
        let Some(chat) = self.chat.clone() else {
            return Err("no text processing provider configured".to_owned());
        };
        tokio::task::spawn_blocking(move || chat.process(&prompt))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }
}

struct SqliteHistory {
    store: Option<crate::history::HistoryStore>,
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

fn control_receiver() -> tokio::sync::mpsc::Receiver<ControlCommand> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut first_stop = false;
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
/// decoded events before each await.
struct LocalTransport {
    state: parking_lot::Mutex<LocalTransportState>,
}

struct LocalTransportState {
    recognizer: crate::local_asr::LocalStreamingRecognizer,
    pending: std::collections::VecDeque<io::Result<RealtimeEvent>>,
    ready_sent: bool,
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

struct StdoutSink {
    json: bool,
    sequence: u64,
}

impl StdoutSink {
    fn new(json: bool) -> Self {
        Self { json, sequence: 0 }
    }
}

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
        let (kind, transcript, message, injection) = event_fields(&event);
        let line = serde_json::to_string(&JsonEvent {
            kind,
            sequence: self.sequence,
            transcript,
            message,
            injection,
        })?;
        // No subscriber is fine: nobody is watching.
        let _ = self.events.send(line);
        Ok(())
    }
}

type EventFields<'a> = (
    &'a str,
    Option<&'a TranscriptSnapshot>,
    Option<&'a str>,
    Option<&'a InjectionResult>,
);

fn event_fields(event: &OutputEvent) -> EventFields<'_> {
    match event {
        OutputEvent::Ready => ("ready", None, None, None),
        OutputEvent::Transcript(snapshot) => ("transcript", Some(snapshot), None, None),
        OutputEvent::Finalized { injection } => ("finalized", None, None, injection.as_ref()),
        OutputEvent::Warning(message) => ("warning", None, Some(message.as_str()), None),
        OutputEvent::Error(message) => ("error", None, Some(message.as_str()), None),
        OutputEvent::Cancelled => ("cancelled", None, None, None),
        OutputEvent::Completed => ("completed", None, None, None),
    }
}

impl EventSink for StdoutSink {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        self.sequence += 1;
        let (kind, transcript, message, injection) = event_fields(&event);
        if self.json {
            let payload = JsonEvent {
                kind,
                sequence: self.sequence,
                transcript,
                message,
                injection,
            };
            println!("{}", serde_json::to_string(&payload)?);
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

/// Config-driven injector: selects the wtype or clipboard method from
/// `[inject]` and applies the clipboard fallback when wtype fails.
pub struct ConfiguredInjector {
    config: InjectConfig,
    fcitx5: Option<crate::fcitx5::Fcitx5Client>,
}

impl ConfiguredInjector {
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
pub async fn inject_text(config: &InjectConfig, text: &str) -> InjectionResult {
    inject_text_with(config, text, None).await
}

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
        Ok((false, reason)) => method_result(false, "fcitx5", &reason),
        Err(error) => method_result(false, "fcitx5", &error),
    }
}

/// Inject text through `wtype` with default settings. Newlines are typed as
/// Shift+Enter so a chat input breaks the line without submitting.
pub async fn inject_via_wtype(text: &str) -> InjectionResult {
    inject_via_wtype_with(&InjectConfig::default(), text).await
}

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

async fn run_command(command: &str, args: &[&str], limit: Duration) -> Result<(), String> {
    run_command_with_stdin(command, args, "", true, limit).await
}

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

fn method_result(ok: bool, method: &str, message: &str) -> InjectionResult {
    InjectionResult {
        ok,
        method: method.to_owned(),
        message: message.to_owned(),
    }
}
