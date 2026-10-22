//! Voiceless: realtime voice input for NixOS/Wayland in one Rust binary.
//!
//! The `voiceless` binary (`src/main.rs`) is a thin clap front end over this
//! crate. Everything user-facing lives here so the CLI, the resident daemon,
//! the web console and the integration tests share one pipeline:
//!
//! ```text
//! pw-record (16 kHz mono PCM16) -> streaming ASR transport -> [text] cleanup
//!   -> optional LLM "mode" processing -> one injection into the focused window
//!   (wtype / clipboard paste / Fcitx5 bridge with in-field preedit)
//!   -> SQLite history + WAV retention
//! ```
//!
//! `coordinator::run_session` is the only orchestration boundary of a
//! realtime session. Every side of it (capture, transport, processor,
//! injector, history, event sink) is a trait, so tests replace any side with
//! a fake while production wiring lives in `stream`.
//!
//! # Module map
//!
//! Command layer (one module per `voiceless` subcommand, each returning the
//! process exit code):
//!
//! - `stream`: `voiceless stream`; builds the production pipeline from
//!   `config` and runs it through the coordinator; also the text injectors.
//! - `batch_cmd`: `voiceless transcribe` and `voiceless record`.
//! - `model_cmd`, `mode_cmd`, `history_cmd`: `voiceless model|mode|history`.
//! - `benchmark_cmd`, `latency_cmd`: `voiceless benchmark asr|latency`.
//! - `daemon`: `voiceless daemon`; the resident gateway, warm engine, D-Bus
//!   control interface, evdev hotkey wiring and console host.
//! - `ctl`: `voiceless ctl`; one-shot D-Bus client of the daemon, and the
//!   relay that lets `stream` run inside the daemon.
//! - `doctor`: `voiceless doctor`; dependency checks.
//!
//! Session core:
//!
//! - `coordinator`: the single-owner session state machine (ready gate,
//!   bounded audio queue, fixed stop order, single injection).
//! - `session`: transcript accumulation (confirmed segments + partial).
//! - `capture`: `pw-record` child process and the `WavRecorder` decorator.
//!
//! Recognition backends and engines:
//!
//! - `realtime`: DashScope realtime WebSocket (`cloud-realtime`).
//! - `doubao`: Volcengine Doubao streaming ASR.
//! - `local_asr`: Sherpa-ONNX online streaming (`local-streaming`).
//! - `offline_asr`: offline models driven as a streaming backend with VAD
//!   segmentation (`local-offline`).
//! - `gguf_asr` (feature `transcribe`): GGUF models on transcribe.cpp/ggml.
//! - `engine`: `WarmEngine`, the preloaded local model the daemon shares.
//! - `batch`: one-shot WAV transcription (cloud or local).
//!
//! Text:
//!
//! - `text`: `[text]` cleanup (fillers, hotwords, replacements, rules).
//! - `modes`: builtin and user-defined processing modes.
//! - `processing`: OpenAI-compatible / Ollama chat calls for non-quick modes.
//!
//! Persistence (all private XDG paths, never the Nix store):
//!
//! - `config`: strict `config.toml` contract (unknown keys are errors, 0600
//!   gate when an API key is present).
//! - `history`: SQLite recognition history (WAL, 0600, schema v2).
//! - `models`: pinned model catalog with integrity-checked installs.
//!
//! Desktop integration:
//!
//! - `hotkey`: evdev push-to-talk key listener.
//! - `media`: MPRIS pause/resume and `pw-play` start/stop cues.
//! - `fcitx5`: client of the Fcitx5 bridge module (commit + preedit).
//! - `console`: loopback axum web console (`docs/console-api.md`).
//!
//! Quality gates:
//!
//! - `benchmark`: CER corpus replay; `latency`: stop-to-inject trials.
//!
//! # Invariants
//!
//! - Audio flow is bounded and lossless: chunks queue in 16 slots of 32 ms
//!   and one send may take at most 500 ms; exceeding either fails the session
//!   rather than dropping or reordering audio.
//! - The transport connects (or the local model loads) before `pw-record`
//!   starts, so authentication failures produce no audio.
//! - Stop order is fixed: stop new chunks, stop capture, flush the tail
//!   frame, drain FIFO, send exactly one finish, read the final events, inject
//!   at most once. Cancel and error paths never inject partial text.
//! - Session exit codes: `0` success, `1` failure, `130` cancelled; the CLI
//!   maps the first SIGINT to stop and the second (or SIGTERM) to cancel, the
//!   daemon maps hotkey and D-Bus commands through its gateway, which allows
//!   one session at a time.
//! - Only successful sessions keep their WAV and history row; both are
//!   created and deleted together.
//! - The console listens on loopback only, checks `Host`, requires a
//!   per-start token and never returns API keys.

/// One-shot WAV transcription (cloud DashScope or local models).
pub mod batch;
/// `voiceless transcribe` and `voiceless record` subcommands.
pub mod batch_cmd;
/// CER quality benchmark over the versioned corpus.
pub mod benchmark;
/// `voiceless benchmark asr` subcommand.
pub mod benchmark_cmd;
/// `pw-record` capture and the WAV-mirroring recorder decorator.
pub mod capture;
/// Strict `config.toml` loading, validation and persistence.
pub mod config;
/// Loopback web console served by the daemon or `history serve`.
pub mod console;
/// Single-owner realtime session coordinator and its environment traits.
pub mod coordinator;
/// `voiceless ctl` D-Bus client and the `stream`-via-daemon relay.
pub mod ctl;
/// `voiceless daemon`: gateway, warm engine, D-Bus control and hotkey.
pub mod daemon;
/// `voiceless doctor` dependency checks.
pub mod doctor;
/// Volcengine Doubao streaming ASR transport.
pub mod doubao;
/// Warm (preloaded) local recognition engines shared by daemon sessions.
pub mod engine;
/// Client of the Fcitx5 bridge module: commit and in-field preedit.
pub mod fcitx5;
/// GGUF ASR models on transcribe.cpp (feature `transcribe`).
#[cfg(feature = "transcribe")]
pub mod gguf_asr;
/// SQLite recognition history with audio retention.
pub mod history;
/// `voiceless history` subcommands.
pub mod history_cmd;
/// evdev push-to-talk key listener.
pub mod hotkey;
/// Stop-to-inject latency gate (trial runner and percentiles).
pub mod latency;
/// `voiceless benchmark latency` subcommand.
pub mod latency_cmd;
/// Sherpa-ONNX online streaming recognizer (`local-streaming`).
pub mod local_asr;
/// MPRIS pause/resume and start/stop audio cues.
pub mod media;
/// `voiceless mode` subcommands.
pub mod mode_cmd;
/// `voiceless model` subcommands.
pub mod model_cmd;
/// Pinned model catalog and integrity-checked install/check/remove.
pub mod models;
/// Builtin and user-defined text processing modes.
pub mod modes;
/// Offline models driven as a streaming backend with VAD (`local-offline`).
pub mod offline_asr;
/// LLM post-processing for non-quick modes.
pub mod processing;
/// DashScope realtime WebSocket transport (`cloud-realtime`).
pub mod realtime;
/// Transcript state: confirmed segments, partial text, final snapshot.
pub mod session;
/// `voiceless stream`: production pipeline wiring and text injection.
pub mod stream;
/// `[text]` cleanup applied to every final transcript.
pub mod text;
