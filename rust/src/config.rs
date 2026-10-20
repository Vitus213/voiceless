use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

const DEFAULT_REALTIME_ENDPOINT: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/realtime";
const DEFAULT_REALTIME_MODEL: &str = "qwen3-asr-flash-realtime";
const DEFAULT_DOUBAO_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";
const DEFAULT_DOUBAO_RESOURCE_ID: &str = "volc.seedasr.sauc.duration";
const DEFAULT_PROCESSING_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const DEFAULT_PROCESSING_MODEL: &str = "deepseek-v4-flash-0731";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unknown configuration field: {field}")]
    UnknownField { field: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("configuration parse error: {0}")]
    Parse(String),
    #[error("cannot read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "configuration file containing a cloud API key must have permissions 0600 or stricter"
    )]
    InsecurePermissions,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct AppConfig {
    pub asr: AsrConfig,
    pub cloud: CloudConfig,
    pub doubao: DoubaoConfig,
    pub local: LocalConfig,
    pub text: TextConfig,
    pub hotkey: HotkeyConfig,
    pub audio: AudioConfig,
    pub console: ConsoleConfig,
    pub inject: InjectConfig,
    pub processing: ProcessingConfig,
    pub history: HistoryConfig,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    pub streaming_backend: String,
    /// Backend used when `streaming_backend` cannot open (model missing,
    /// credentials absent, cloud unreachable) before a session starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_backend: Option<String>,
    pub batch_backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_model_dir: Option<std::path::PathBuf>,
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

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub timeout_seconds: f64,
    pub realtime_endpoint: String,
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
    pub ctrl_mode: String,
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
    pub enabled: bool,
    pub host: String,
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
    pub hotwords: Vec<String>,
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

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextReplacement {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextRule {
    pub pattern: String,
    pub replace: String,
}

/// `local-offline` backend: an offline catalog model run on VAD segments.
/// The open segment is re-decoded every `partial_interval_ms` for the live
/// partial; `max_segment_seconds` caps a segment (the Sherpa Qwen3-ASR
/// export holds ~25 s, GGUF models up to 60 s here).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub model: String,
    /// Execution device: cuda | cpu. Unset: cuda for GGUF models, cpu for
    /// Sherpa-ONNX ones (see [`LocalConfig::device`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub num_threads: i32,
    pub partial_interval_ms: u64,
    /// Unset: 30 s for GGUF models, 15 s for Sherpa-ONNX ones (see
    /// [`LocalConfig::segment_limit`]).
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
    pub fn local_engine(&self) -> LocalConfig {
        let mut local = self.local.clone();
        merge_hotwords(&mut local.hotwords, &self.text.hotwords);
        local
    }

    /// `[doubao]` with `[text].hotwords` merged into its hotwords.
    pub fn doubao_engine(&self) -> DoubaoConfig {
        let mut doubao = self.doubao.clone();
        merge_hotwords(&mut doubao.hotwords, &self.text.hotwords);
        doubao
    }
}

fn merge_hotwords(target: &mut Vec<String>, extra: &[String]) {
    for word in extra {
        if !target.contains(word) {
            target.push(word.clone());
        }
    }
}

impl LocalConfig {
    fn is_gguf(&self) -> bool {
        matches!(
            crate::offline_asr::OfflineFamily::for_model(&self.model),
            Ok(crate::offline_asr::OfflineFamily::Gguf)
        )
    }

    /// The configured device, or the model family's default.
    pub fn device(&self) -> String {
        self.provider
            .clone()
            .unwrap_or_else(|| if self.is_gguf() { "cuda" } else { "cpu" }.to_owned())
    }

    /// The configured segment cap, or the model family's default.
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
    pub endpoint: String,
    pub api_key: String,
    pub app_key: String,
    pub access_key: String,
    pub resource_id: String,
    pub enable_itn: bool,
    pub enable_punc: bool,
    pub enable_ddc: bool,
    /// Second-pass (nonstream) recognition on each sentence; more accurate
    /// confirmed text, only on the bigmodel_async endpoint.
    pub enable_nonstream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_window_size: Option<u32>,
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
    pub fn has_credentials(&self) -> bool {
        !self.api_key.trim().is_empty()
            || (!self.app_key.trim().is_empty() && !self.access_key.trim().is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct InjectConfig {
    pub prefer: String,
    pub wtype_command: String,
    pub wl_copy_command: String,
    pub paste_command: String,
    pub paste_tool: String,
    pub focus_command: String,
    pub x11_clipboard_command: String,
    pub clipboard_fallback: bool,
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

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessingConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub api_key_env: String,
    pub prompt: String,
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
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config = toml::from_str::<Self>(input).map_err(|error| {
            let message = error.to_string();
            if message.contains("unknown field") {
                ConfigError::UnknownField { field: message }
            } else {
                ConfigError::Parse(message)
            }
        })?;
        config.validate()
    }
    pub fn load_optional(path: Option<&Path>) -> Result<Self, ConfigError> {
        match path {
            Some(path) => Self::load(path),
            None => load_default_config(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let input = fs::read_to_string(path)?;
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
        crate::text::TextCleaner::new(&self.text).map_err(ConfigError::Invalid)?;
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

/// Path to the Voiceless config file under the XDG config root.
pub fn default_config_path() -> PathBuf {
    let root = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned())).join(".config")
        });
    root.join("voiceless").join("config.toml")
}

/// Load the Voiceless XDG config file when present, otherwise defaults.
/// Missing files are valid; unreadable or invalid files are errors.
pub fn load_default_config() -> Result<AppConfig, ConfigError> {
    let path = default_config_path();
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    AppConfig::load(&path)
}
