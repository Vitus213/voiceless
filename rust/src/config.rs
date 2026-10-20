use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

const DEFAULT_REALTIME_ENDPOINT: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/realtime";
const DEFAULT_REALTIME_MODEL: &str = "qwen3-asr-flash-realtime";
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
    pub inject: InjectConfig,
    pub processing: ProcessingConfig,
    pub history: HistoryConfig,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    pub streaming_backend: String,
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
        if !input.is_empty() && input.contains("api_key") {
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
        match self.asr.streaming_backend.as_str() {
            "cloud-realtime" | "local-streaming" => {}
            value => {
                return Err(ConfigError::Invalid(format!(
                    "asr.streaming_backend={value:?}; expected cloud-realtime or local-streaming"
                )))
            }
        }
        if self.asr.final_backend.is_some() {
            return Err(ConfigError::Invalid(
                "asr.final_backend was removed; choose cloud-realtime or local-streaming"
                    .to_owned(),
            ));
        }
        match self.inject.prefer.as_str() {
            "wtype" | "clipboard" => {}
            value => {
                return Err(ConfigError::Invalid(format!(
                    "inject.prefer={value:?}; expected wtype or clipboard"
                )))
            }
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
