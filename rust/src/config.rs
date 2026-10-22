//! Strict `config.toml` schema: loading, validation and the secret gate.
//!
//! Every command that touches audio, a recognizer, the injector or the
//! console starts here. `stream::run_with_env`, `daemon::serve`,
//! `batch_cmd`, `benchmark_cmd`, `latency_cmd`, `doctor` and the console
//! (`console::ConsoleState::config`, `console::settings::patch_file`) all go
//! through `AppConfig::load_optional`, `load_default_config` or `from_toml`.
//! The file is `$XDG_CONFIG_HOME/voiceless/config.toml`
//! (`~/.config/voiceless/config.toml` when the variable is unset); a missing
//! file means `AppConfig::default()`. Each section struct maps 1:1 to a
//! TOML table: `[asr]`, `[cloud]`, `[doubao]`, `[local]`, `[text]`,
//! `[hotkey]`, `[audio]`, `[console]`, `[inject]`, `[processing]`,
//! `[history]`. The README "Configuration" section is the user-facing key
//! reference and repeats the defaults defined in the `Default` impls below.
//!
//! Contract:
//!
//! - Every key is optional (`#[serde(default)]`) and every table rejects
//!   unknown keys (`deny_unknown_fields`, `ConfigError::UnknownField`), so a
//!   typo never silently falls back to a default.
//! - Removed legacy values are rejected, never rewritten: the type4me-era
//!   backends `cloud-vad` and `sensevoice-vad` fail the backend allowlist in
//!   `validate`, and `asr.final_backend` is still deserialized only so that
//!   `validate` can reject it with a specific message.
//! - `validate` runs on every parse (`from_toml`), so an `AppConfig` built
//!   from text is either fully valid or never constructed. Checks that need
//!   other modules (model family, evdev key name, regex rules) call into
//!   them instead of duplicating their parsers.
//! - Secret gate (unix): `load` refuses a file whose text contains
//!   `api_key` or `access_key` unless its mode has no group/other bits
//!   (0600 or stricter), with `ConfigError::InsecurePermissions`. The check
//!   is textual, so a commented-out key or `processing.api_key_env` also
//!   triggers it. `from_toml` has no such gate; it never sees a path.
//!
//! Writers: the console's `PUT /api/config` edits the file with `toml_edit`
//! (`console::settings::patch_file`), re-validates the merged text with
//! `from_toml` and writes it back with 0600 permissions, preserving comments
//! and layout. `AppConfig::save` instead serializes the whole struct (the
//! file's comments are lost); nothing in the crate calls it today. `stream`
//! re-validates a `--backend` override by round-tripping the struct through
//! TOML, and the console serializes it to JSON for `GET /api/config` (secrets
//! are masked there, not here); both are why every section derives
//! `Serialize` and why `None` options are skipped when serializing.
//!
//! `[text].hotwords` is the one list the user maintains for both recognizer
//! biasing and phonetic correction: `AppConfig::local_engine` and
//! `AppConfig::doubao_engine` merge it into the engine-specific lists.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

// Defaults the README key reference repeats; keep both in sync.
const DEFAULT_REALTIME_ENDPOINT: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/realtime";
const DEFAULT_REALTIME_MODEL: &str = "qwen3-asr-flash-realtime";
const DEFAULT_DOUBAO_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
const DEFAULT_DOUBAO_RESOURCE_ID: &str = "volc.seedasr.sauc.duration";
const DEFAULT_PROCESSING_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const DEFAULT_PROCESSING_MODEL: &str = "deepseek-v4-flash-0731";

/// Why a configuration could not be loaded. Commands print the `Display`
/// text after `Voiceless: ` and exit non-zero; the console answers 400 or
/// 500 with it.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A key no section declares. `field` carries the whole serde message,
    /// which names the key and the accepted ones.
    #[error("unknown configuration field: {field}")]
    UnknownField { field: String },
    /// A value failed `AppConfig::validate` (message names `section.key`),
    /// or `save` could not serialize the config.
    #[error("invalid configuration: {0}")]
    Invalid(String),
    /// Not valid TOML, or a type mismatch (any serde error other than an
    /// unknown field).
    #[error("configuration parse error: {0}")]
    Parse(String),
    /// The file could not be read, written, created or chmod'ed.
    #[error("cannot read configuration: {0}")]
    Io(#[from] std::io::Error),
    /// The file mentions `api_key` or `access_key` and is readable by the
    /// group or others (see `AppConfig::load`).
    #[error(
        "configuration file containing a cloud API key must have permissions 0600 or stricter"
    )]
    InsecurePermissions,
}

/// The whole `config.toml`. `Default` is the documented default
/// configuration; every table is optional and every unknown key is an
/// error. Values obtained through `from_toml`/`load` have passed
/// `validate`; a struct assembled by hand (tests, `--backend` override)
/// has not.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct AppConfig {
    /// `[asr]`: which streaming and batch engines to use.
    pub asr: AsrConfig,
    /// `[cloud]`: DashScope credentials and endpoints.
    pub cloud: CloudConfig,
    /// `[doubao]`: Volcengine streaming ASR credentials and options.
    pub doubao: DoubaoConfig,
    /// `[local]`: offline model, device and segmentation.
    pub local: LocalConfig,
    /// `[text]`: final-text cleanup rules.
    pub text: TextConfig,
    /// `[hotkey]`: daemon push-to-talk key.
    pub hotkey: HotkeyConfig,
    /// `[audio]`: media pause and start/stop cues.
    pub audio: AudioConfig,
    /// `[console]`: the daemon's web console listener.
    pub console: ConsoleConfig,
    /// `[inject]`: how the final text reaches the focused window.
    pub inject: InjectConfig,
    /// `[processing]`: the LLM used by every mode other than `quick`.
    pub processing: ProcessingConfig,
    /// `[history]`: SQLite history and WAV retention.
    pub history: HistoryConfig,
}

/// `[asr]`: engine selection for realtime sessions and whole-file
/// transcription.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    /// `asr.streaming_backend`: the realtime engine of `stream` and daemon
    /// sessions: `cloud-realtime` (DashScope WebSocket), `doubao-realtime`
    /// (Volcengine), `local-streaming` (Sherpa-ONNX online Paraformer) or
    /// `local-offline` (VAD segments decoded by the `[local]` model).
    /// Default `cloud-realtime`; any other value fails validation.
    pub streaming_backend: String,
    /// Backend used when `streaming_backend` cannot open (model missing,
    /// credentials absent, cloud unreachable) before a session starts.
    /// Same allowlist as `streaming_backend`; unset (the default) means no
    /// fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_backend: Option<String>,
    /// `asr.batch_backend`: engine of `transcribe`/`record` when no
    /// `--backend` is given: `cloud` (DashScope REST) or `local`
    /// (SenseVoice). Default `cloud`. Checked by the batch command when it
    /// runs, not by `validate`.
    pub batch_backend: String,
    /// Removed type4me key. Kept in the schema only so `validate` can reject
    /// it with a specific message instead of a generic unknown-field error;
    /// always `None` in a validated config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_backend: Option<String>,
    /// Directory of the streaming Paraformer model for `local-streaming`,
    /// replacing the managed catalog install. Unset: the catalog's current
    /// version under the data directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_model_dir: Option<std::path::PathBuf>,
    /// Directory of the SenseVoice model for the `local` batch backend,
    /// replacing the managed catalog install.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_model_dir: Option<std::path::PathBuf>,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            streaming_backend: "cloud-realtime".to_owned(),
            fallback_backend: None,
            batch_backend: "cloud".to_owned(),
            final_backend: None,
            local_model_dir: None,
            batch_model_dir: None,
        }
    }
}

/// `[cloud]`: DashScope (Alibaba BaiLian) access, shared by the
/// `cloud-realtime` streaming backend and the `cloud` batch backend.
/// Independent of `[processing]`, which has its own key and endpoint.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudConfig {
    /// DashScope API key. Empty (the default) means the cloud backends
    /// cannot open and the benchmarks report skip. A file that sets it must
    /// be 0600 (see `AppConfig::load`); the console never returns it.
    pub api_key: String,
    /// REST base of the batch (whole-file) API; default
    /// `https://dashscope.aliyuncs.com`.
    pub base_url: String,
    /// Model of the batch API; default `qwen3-asr-flash-2026-02-10`.
    pub model: String,
    /// Timeout of one batch REST request in seconds; default 60. The
    /// realtime WebSocket has its own deadlines.
    pub timeout_seconds: f64,
    /// WebSocket URL of the realtime API; must start with `wss://`.
    pub realtime_endpoint: String,
    /// Realtime model name; must not be blank. Default
    /// `qwen3-asr-flash-realtime`.
    pub realtime_model: String,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://dashscope.aliyuncs.com".to_owned(),
            model: "qwen3-asr-flash-2026-02-10".to_owned(),
            timeout_seconds: 60.0,
            realtime_endpoint: DEFAULT_REALTIME_ENDPOINT.to_owned(),
            realtime_model: DEFAULT_REALTIME_MODEL.to_owned(),
        }
    }
}

/// Daemon push-to-talk key read from evdev (needs the `input` group). Hold
/// it at least `hold_threshold_ms` to talk while held; tap it to toggle
/// (tap again to stop). With Ctrl held the session uses `ctrl_mode`.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// evdev key name, e.g. `KEY_SCROLLLOCK`; unset disables the listener.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Mode for a plain press; empty uses the daemon's `--mode`.
    pub mode: String,
    /// Mode for a press with Ctrl held; default `prompt-optimize`.
    pub ctrl_mode: String,
    /// Press duration in milliseconds from which a press counts as
    /// hold-to-talk (release stops) instead of a tap (toggle); default 350.
    pub hold_threshold_ms: u64,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: None,
            mode: String::new(),
            ctrl_mode: "prompt-optimize".to_owned(),
            hold_threshold_ms: 350,
        }
    }
}

/// Web console served by the daemon (loopback only by default).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsoleConfig {
    /// Serve the console from the daemon; default true. `history serve`
    /// ignores this and takes its own `--host`/`--port`.
    pub enabled: bool,
    /// Listen address; default `127.0.0.1`. The console also rejects any
    /// non-loopback `Host` header, so a wider bind alone does not expose it.
    pub host: String,
    /// Listen port; default 8790.
    pub port: u16,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".to_owned(),
            port: 8790,
        }
    }
}

/// Daemon session side effects.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Pause MPRIS players while recording; resume the paused ones after.
    pub pause_media: bool,
    /// Short start/stop tones through `pw-play`.
    pub cues: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            pause_media: true,
            cues: false,
        }
    }
}

/// Cleanup applied to every final transcript before mode processing and
/// injection. `hotwords` both bias the recognizer (local models, Doubao)
/// and correct sound-alike spans phonetically (Chinese words of two or
/// more characters, pinyin similarity ≥ `hotword_threshold`).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextConfig {
    /// Drop "呃" everywhere and a standalone "嗯", fixing punctuation.
    pub remove_fillers: bool,
    /// Drop one trailing "。" or "." (chat boxes).
    pub strip_trailing_period: bool,
    /// Preferred words. Every entry is merged into the recognizer biasing
    /// lists (`AppConfig::local_engine`, `AppConfig::doubao_engine`);
    /// all-Chinese entries of two or more characters additionally rewrite
    /// sound-alike spans in `text::TextCleaner`.
    pub hotwords: Vec<String>,
    /// Minimum mean per-syllable pinyin similarity (0.5..=1.0, default 0.9)
    /// for a span to be rewritten. Identical tone-less syllables score 1.0,
    /// a known confusion (zh/z, n/l, f/h, -ng/-n) 0.8, anything else 0, so
    /// the default tolerates about one confused syllable in two or three.
    pub hotword_threshold: f32,
    /// Exact replacements, applied in order.
    pub replacements: Vec<TextReplacement>,
    /// Regex replacements (`$1` captures), applied in order after them.
    pub rules: Vec<TextRule>,
}

impl Default for TextConfig {
    fn default() -> Self {
        Self {
            remove_fillers: true,
            strip_trailing_period: false,
            hotwords: Vec::new(),
            hotword_threshold: 0.9,
            replacements: Vec::new(),
            rules: Vec::new(),
        }
    }
}

/// One `[[text.replacements]]` entry: a literal substring swap, no regex.
/// Both keys are required (no defaults).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextReplacement {
    /// Text to find; an empty value is ignored at cleanup time.
    pub from: String,
    /// Replacement; may be empty to delete `from`.
    pub to: String,
}

/// One `[[text.rules]]` entry: a `regex` crate pattern, compiled during
/// validation (an invalid pattern is a `ConfigError::Invalid`). Both keys
/// are required.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextRule {
    /// Regular expression (`regex` syntax, no look-around).
    pub pattern: String,
    /// Replacement with `$1`/`${name}` capture references.
    pub replace: String,
}

/// `local-offline` backend: an offline catalog model run on VAD segments.
/// The open segment is re-decoded every `partial_interval_ms` for the live
/// partial; `max_segment_seconds` caps a segment (the Sherpa Qwen3-ASR
/// export holds ~25 s, GGUF models up to 60 s here).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// Catalog id of the offline model: `qwen3-asr-1.7b-gguf` (default) or
    /// `qwen3-asr-0.6b-gguf` (GPU, transcribe.cpp), `qwen3-asr-0.6b-int8`
    /// or `sense-voice-int8` (CPU, Sherpa-ONNX). Validation accepts any id
    /// `offline_asr::OfflineFamily::for_model` can classify.
    pub model: String,
    /// Execution device: cuda | cpu. Unset: cuda for GGUF models, cpu for
    /// Sherpa-ONNX ones (see [`LocalConfig::device`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// ONNX Runtime intra-op threads for Sherpa-ONNX models; default 4.
    /// GGUF models ignore it.
    pub num_threads: i32,
    /// How often the open VAD segment is re-decoded for the live partial,
    /// in milliseconds; default 500.
    pub partial_interval_ms: u64,
    /// Unset: 30 s for GGUF models, 15 s for Sherpa-ONNX ones (see
    /// [`LocalConfig::segment_limit`]). Validated to 1..=60 (GGUF) or
    /// 1..=25 (Sherpa-ONNX).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_segment_seconds: Option<f32>,
    /// Biasing words (Qwen3-ASR only).
    pub hotwords: Vec<String>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            model: "qwen3-asr-1.7b-gguf".to_owned(),
            provider: None,
            num_threads: 4,
            partial_interval_ms: 500,
            max_segment_seconds: None,
            hotwords: Vec::new(),
        }
    }
}

impl AppConfig {
    /// `[local]` with `[text].hotwords` merged into its recognizer hotwords.
    /// The engine's own list comes first, then the `[text]` entries it does
    /// not already contain (exact match). This is the config the engine is
    /// built from and compared against for reloads.
    pub fn local_engine(&self) -> LocalConfig {
        let mut local = self.local.clone();
        merge_hotwords(&mut local.hotwords, &self.text.hotwords);
        local
    }

    /// `[doubao]` with `[text].hotwords` merged into its hotwords (same
    /// order rule as `local_engine`). `stream` connects with this config.
    pub fn doubao_engine(&self) -> DoubaoConfig {
        let mut doubao = self.doubao.clone();
        merge_hotwords(&mut doubao.hotwords, &self.text.hotwords);
        doubao
    }
}

/// Append the entries of `extra` that `target` lacks, keeping order and
/// never duplicating a word the engine already biases.
fn merge_hotwords(target: &mut Vec<String>, extra: &[String]) {
    for word in extra {
        if !target.contains(word) {
            target.push(word.clone());
        }
    }
}

impl LocalConfig {
    /// True for ids the offline module classifies as GGUF (`-gguf` suffix);
    /// an unknown id counts as non-GGUF here and fails `validate` later.
    fn is_gguf(&self) -> bool {
        matches!(
            crate::offline_asr::OfflineFamily::for_model(&self.model),
            Ok(crate::offline_asr::OfflineFamily::Gguf)
        )
    }

    /// The configured device, or the model family's default (`cuda` for
    /// GGUF, `cpu` otherwise). Validation guarantees `cpu` or `cuda`.
    pub fn device(&self) -> String {
        self.provider
            .clone()
            .unwrap_or_else(|| if self.is_gguf() { "cuda" } else { "cpu" }.to_owned())
    }

    /// The configured segment cap in seconds, or the model family's default
    /// (30 for GGUF, 15 otherwise). Feeds the VAD's maximum speech duration.
    pub fn segment_limit(&self) -> f32 {
        self.max_segment_seconds
            .unwrap_or(if self.is_gguf() { 30.0 } else { 15.0 })
    }
}

/// Volcengine (Doubao) bigmodel streaming ASR. New-console keys go in
/// `api_key` (`X-Api-Key`); legacy-console apps use `app_key` (App ID) and
/// `access_key` (Access Token). `resource_id` must match the activated
/// service: `volc.seedasr.sauc.*` for Seed ASR 2.0, `volc.bigasr.sauc.*`
/// for 1.0, `.duration` (hourly) or `.concurrent` billing.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DoubaoConfig {
    /// WebSocket URL; must start with `wss://`. Default: the
    /// `bigmodel_async` endpoint.
    pub endpoint: String,
    /// New-console APP Key, sent as `X-Api-Key`. Secret (0600 gate, never
    /// returned by the console).
    pub api_key: String,
    /// Legacy-console App ID; only meaningful together with `access_key`.
    pub app_key: String,
    /// Legacy-console Access Token. Secret (0600 gate, write-only in the
    /// console).
    pub access_key: String,
    /// Service resource id; default `volc.seedasr.sauc.duration`.
    pub resource_id: String,
    /// Inverse text normalization (numbers, dates as digits); default true.
    pub enable_itn: bool,
    /// Server-side punctuation; default true.
    pub enable_punc: bool,
    /// Semantic smoothing that drops filler words; default false (the
    /// `[text]` cleanup handles fillers locally).
    pub enable_ddc: bool,
    /// Second-pass (nonstream) recognition on each sentence; more accurate
    /// confirmed text, only on the bigmodel_async endpoint.
    pub enable_nonstream: bool,
    /// Milliseconds of silence that close a sentence; unset leaves the
    /// service default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_window_size: Option<u32>,
    /// Biasing words; `[text].hotwords` are merged in by
    /// `AppConfig::doubao_engine`.
    pub hotwords: Vec<String>,
}

impl Default for DoubaoConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_DOUBAO_ENDPOINT.to_owned(),
            api_key: String::new(),
            app_key: String::new(),
            access_key: String::new(),
            resource_id: DEFAULT_DOUBAO_RESOURCE_ID.to_owned(),
            enable_itn: true,
            enable_punc: true,
            enable_ddc: false,
            enable_nonstream: false,
            end_window_size: None,
            hotwords: Vec::new(),
        }
    }
}

impl DoubaoConfig {
    /// True when the new-console `api_key` is set, or both legacy `app_key`
    /// and `access_key` are; whitespace-only values count as unset. `stream`
    /// checks this before opening a Doubao session so a missing key is a
    /// clean "cannot open" (and triggers the fallback backend) rather than a
    /// handshake failure.
    pub fn has_credentials(&self) -> bool {
        !self.api_key.trim().is_empty()
            || (!self.app_key.trim().is_empty() && !self.access_key.trim().is_empty())
    }
}

/// `[inject]`: how the final text reaches the focused window
/// (`stream::inject_text`). Command strings are split on whitespace and
/// spawned directly, never through a shell.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InjectConfig {
    /// Primary method: `wtype` (type through the Wayland virtual keyboard),
    /// `clipboard` (`wl-copy` + synthesized paste keystroke) or `fcitx5`
    /// (commit through the Voiceless Fcitx5 bridge). Default `wtype`; any
    /// other value fails validation.
    pub prefer: String,
    /// Typing executable for the `wtype` method; default `wtype`.
    pub wtype_command: String,
    /// Executable that sets the Wayland clipboard; default `wl-copy`. The
    /// matching `wl-paste` is derived from it to snapshot and restore the
    /// previous selection.
    pub wl_copy_command: String,
    /// Arguments handed to `paste_tool` to synthesize the paste keystroke;
    /// default `-M ctrl -k v` (wtype syntax).
    pub paste_command: String,
    /// Executable that receives `paste_command`; default `wtype`
    /// (`xdotool` for X11 setups).
    pub paste_tool: String,
    /// Optional command run before the paste to raise the target window;
    /// empty (default) disables. Failures are ignored.
    pub focus_command: String,
    /// Optional command that receives the text on stdin to mirror it into
    /// the X11 clipboard (rootless Xwayland does not forward Wayland
    /// selections), e.g. `xsel --clipboard --input`; the previous X11
    /// selection is restored after the paste. Empty (default) disables.
    pub x11_clipboard_command: String,
    /// Retry through the clipboard when the `wtype` or `fcitx5` method
    /// fails; default true. Has no effect when `prefer = "clipboard"`.
    pub clipboard_fallback: bool,
    /// Timeout in seconds for each clipboard subprocess; default 10.
    /// Values below 0.1 are raised to 0.1.
    pub timeout_seconds: f64,
    /// With `prefer = "fcitx5"`: show the live transcript as preedit
    /// (underlined, uncommitted text) in the focused field while speaking.
    pub preedit: bool,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            prefer: "wtype".to_owned(),
            wtype_command: "wtype".to_owned(),
            wl_copy_command: "wl-copy".to_owned(),
            paste_command: "-M ctrl -k v".to_owned(),
            paste_tool: "wtype".to_owned(),
            focus_command: String::new(),
            x11_clipboard_command: String::new(),
            clipboard_fallback: true,
            timeout_seconds: 10.0,
            preedit: true,
        }
    }
}

/// `[processing]`: the chat LLM behind every mode other than `quick`
/// (`processing` module). Deliberately independent of `[cloud]`: the
/// recognizer and the text processor can be different services.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessingConfig {
    /// `none` (default: non-quick modes keep the recognized text with a
    /// warning), `openai-compatible` (`POST {base_url}/chat/completions`) or
    /// `ollama` (`POST {base_url}/api/chat`). Not checked by `validate`;
    /// `processing::from_config` rejects other values when the session
    /// pipeline is built.
    pub provider: String,
    /// Endpoint root; default the BaiLian OpenAI-compatible endpoint. A
    /// trailing `/` is tolerated.
    pub base_url: String,
    /// Chat model name; default `deepseek-v4-flash-0731`.
    pub model: String,
    /// Bearer token sent as `Authorization: Bearer …`; takes precedence
    /// over `api_key_env`. Secret (0600 gate, write-only in the console).
    pub api_key: String,
    /// Name of an environment variable holding the key, read only when
    /// `api_key` is blank; an unset variable means no Authorization header.
    pub api_key_env: String,
    /// Overrides the builtin prompt-optimize template; `{text}` is replaced
    /// by the cleaned transcript (`modes::render_template`). Empty (default)
    /// keeps the builtin. Other modes keep their own prompts.
    pub prompt: String,
    /// Whole-request timeout of one chat call in seconds; default 30.
    pub timeout_seconds: f64,
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            provider: "none".to_owned(),
            base_url: DEFAULT_PROCESSING_BASE_URL.to_owned(),
            model: DEFAULT_PROCESSING_MODEL.to_owned(),
            api_key: String::new(),
            api_key_env: String::new(),
            prompt: String::new(),
            timeout_seconds: 30.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    /// Record successful sessions in `<data dir>/history.sqlite3`; default
    /// true. When false, `stream` opens no store and removes the session's
    /// recording instead of leaving an orphan file.
    pub enabled: bool,
    /// Keep the WAV recording of every successful streaming session next
    /// to the history database (`~/.local/share/voiceless/audio/`).
    pub save_audio: bool,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            save_audio: true,
        }
    }
}

impl AppConfig {
    /// Parse and validate TOML text. Unknown keys become `UnknownField`,
    /// other deserialization errors `Parse`, `validate` failures `Invalid`.
    /// No filesystem access and no permission gate; callers that read a
    /// file go through `load`. Also the validator the console runs on its
    /// merged document before writing it.
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config = toml::from_str::<Self>(input).map_err(|error| {
            let message = error.to_string();
            // toml's error is one string; the "unknown field" wording is how
            // serde reports a key rejected by `deny_unknown_fields`.
            if message.contains("unknown field") {
                ConfigError::UnknownField { field: message }
            } else {
                ConfigError::Parse(message)
            }
        })?;
        config.validate()
    }
    /// Load `path` when given (the `--config` flag), else the XDG default
    /// through `load_default_config`. An explicit path must exist (its read
    /// error is `Io`); the default path may be missing.
    pub fn load_optional(path: Option<&Path>) -> Result<Self, ConfigError> {
        match path {
            Some(path) => Self::load(path),
            None => load_default_config(),
        }
    }

    /// Read, permission-check and validate the file at `path`.
    ///
    /// Secret gate (unix only): when the text mentions `api_key` or
    /// `access_key`, the file mode must have no group/other bits, otherwise
    /// `InsecurePermissions` is returned before parsing. Errors: `Io`
    /// (unreadable file or metadata), `InsecurePermissions`, then whatever
    /// `from_toml` reports.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let input = fs::read_to_string(path)?;
        // A file that carries a cloud/LLM key must not be readable by other
        // users. The check is textual on purpose: it also fires on a
        // commented-out or empty key, so the user tightens the mode before a
        // key is ever stored. `0o077` masks every group/other bit.
        #[cfg(unix)]
        if !input.is_empty() && (input.contains("api_key") || input.contains("access_key")) {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(ConfigError::InsecurePermissions);
            }
        }
        Self::from_toml(&input)
    }

    /// Serialize the full config to TOML and write it back to `path` with
    /// 0600 permissions (the file may contain an API key).
    ///
    /// Parent directories are created. The output is `toml::to_string` of
    /// the struct, so comments, ordering and omitted defaults of the
    /// original file are lost; the console uses
    /// `console::settings::patch_file` for comment-preserving edits and the
    /// crate has no caller of this method today. On unix the mode is
    /// tightened after the write (a new file briefly has the umask mode).
    /// Errors: `Invalid` (serialization), `Io` (write or chmod).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let content =
            toml::to_string(self).map_err(|error| ConfigError::Invalid(error.to_string()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Cross-field checks after deserialization. The first failure wins and
    /// becomes `ConfigError::Invalid` naming the offending `section.key`:
    ///
    /// 1. `asr.streaming_backend` and `asr.fallback_backend` in the backend
    ///    allowlist (this is where the legacy `cloud-vad` and
    ///    `sensevoice-vad` are refused, with the replacements named).
    /// 2. `asr.final_backend` absent (removed key).
    /// 3. `inject.prefer` one of `wtype`, `clipboard`, `fcitx5`.
    /// 4. `local.model` known to `offline_asr::OfflineFamily`; device `cpu`
    ///    or `cuda`; Sherpa-ONNX families on `cpu` only; effective segment
    ///    limit within 1..=60 s (GGUF) or 1..=25 s (Sherpa-ONNX).
    /// 5. `hotkey.key`, when set, a known evdev key name.
    /// 6. `text.hotword_threshold` within 0.5..=1.0 and every `text.rules`
    ///    pattern compiling (by building a `text::TextCleaner`).
    /// 7. `doubao.endpoint` and `cloud.realtime_endpoint` using `wss://`;
    ///    `cloud.realtime_model` non-blank.
    ///
    /// Not checked here: `processing.provider` (`processing::from_config`),
    /// `asr.batch_backend` (the batch command) and whether the cloud keys
    /// are present (each backend reports that when it opens).
    fn validate(self) -> Result<Self, ConfigError> {
        let backends = std::iter::once(("streaming_backend", &self.asr.streaming_backend)).chain(
            self.asr
                .fallback_backend
                .iter()
                .map(|value| ("fallback_backend", value)),
        );
        for (key, value) in backends {
            if !matches!(
                value.as_str(),
                "cloud-realtime" | "doubao-realtime" | "local-streaming" | "local-offline"
            ) {
                return Err(ConfigError::Invalid(format!(
                    "asr.{key}={value:?}; expected cloud-realtime, doubao-realtime, local-streaming or local-offline"
                )));
            }
        }
        // Rejected rather than ignored: silently dropping a removed key would
        // hide that the user's intended backend no longer applies.
        if self.asr.final_backend.is_some() {
            return Err(ConfigError::Invalid(
                "asr.final_backend was removed; choose cloud-realtime or local-streaming"
                    .to_owned(),
            ));
        }
        match self.inject.prefer.as_str() {
            "wtype" | "clipboard" | "fcitx5" => {}
            value => {
                return Err(ConfigError::Invalid(format!(
                    "inject.prefer={value:?}; expected wtype, clipboard or fcitx5"
                )))
            }
        }
        let family = crate::offline_asr::OfflineFamily::for_model(&self.local.model).map_err(|_| {
            ConfigError::Invalid(format!(
                "local.model={:?}; expected qwen3-asr-1.7b-gguf, qwen3-asr-0.6b-gguf, qwen3-asr-0.6b-int8 or sense-voice-int8",
                self.local.model
            ))
        })?;
        let device = self.local.device();
        if !matches!(device.as_str(), "cpu" | "cuda") {
            return Err(ConfigError::Invalid(format!(
                "local.provider={device:?}; expected cpu or cuda"
            )));
        }
        // The Sherpa int8 exports run slower on CUDA than on CPU and abort
        // in ONNX Runtime's CUDA teardown at exit; the GPU path is GGUF.
        if family != crate::offline_asr::OfflineFamily::Gguf && device != "cpu" {
            return Err(ConfigError::Invalid(format!(
                "local.provider={device:?} with {}: Sherpa-ONNX models run on cpu; use qwen3-asr-1.7b-gguf for the GPU",
                self.local.model
            )));
        }
        // Hard ceilings of the decoders: the Sherpa Qwen3-ASR export holds
        // about 25 s of audio; the GGUF path accepts up to 60 s per segment.
        let max_segment = match family {
            crate::offline_asr::OfflineFamily::Gguf => 60.0,
            _ => 25.0,
        };
        if !(1.0..=max_segment).contains(&self.local.segment_limit()) {
            return Err(ConfigError::Invalid(format!(
                "local.max_segment_seconds must be between 1 and {max_segment} for {}",
                self.local.model
            )));
        }
        if let Some(key) = &self.hotkey.key {
            crate::hotkey::parse_key(key)
                .map_err(|error| ConfigError::Invalid(format!("hotkey.key: {error}")))?;
        }
        if !(0.5..=1.0).contains(&self.text.hotword_threshold) {
            return Err(ConfigError::Invalid(
                "text.hotword_threshold must be between 0.5 and 1.0".to_owned(),
            ));
        }
        // Compile the regex rules now so a bad pattern fails at load time,
        // not in the middle of a session's final-text step.
        crate::text::TextCleaner::new(&self.text).map_err(ConfigError::Invalid)?;
        // Keys travel over these sockets; plain ws:// is never acceptable.
        if !self.doubao.endpoint.starts_with("wss://") {
            return Err(ConfigError::Invalid(
                "doubao.endpoint must use wss://".to_owned(),
            ));
        }
        if !self.cloud.realtime_endpoint.starts_with("wss://") {
            return Err(ConfigError::Invalid(
                "cloud.realtime_endpoint must use wss://".to_owned(),
            ));
        }
        if self.cloud.realtime_model.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "cloud.realtime_model must not be empty".to_owned(),
            ));
        }
        Ok(self)
    }
}

/// Path to the Voiceless config file under the XDG config root:
/// `$XDG_CONFIG_HOME/voiceless/config.toml`, else `$HOME/.config/...`, else
/// `./.config/...` when neither variable is set. Same resolution as
/// `models::default_config_dir`; the daemon and `history serve` hand this
/// path to the console as the file it edits.
pub fn default_config_path() -> PathBuf {
    let root = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned())).join(".config")
        });
    root.join("voiceless").join("config.toml")
}

/// Load the Voiceless XDG config file when present, otherwise defaults.
/// Missing files are valid; unreadable or invalid files are errors. Used by
/// the commands that take no `--config` flag (batch, benchmarks, doctor).
pub fn load_default_config() -> Result<AppConfig, ConfigError> {
    let path = default_config_path();
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    AppConfig::load(&path)
}
