use std::fs;

use tempfile::tempdir;
use voiceless::config::{AppConfig, ConfigError, ProcessingConfig};

#[test]
fn defaults_to_cloud_realtime_without_a_legacy_final_backend() {
    let config = AppConfig::default();

    assert_eq!(config.asr.streaming_backend, "cloud-realtime");
    assert!(config.asr.final_backend.is_none());
}

#[test]
fn rejects_unknown_keys_and_legacy_streaming_backends() {
    let unknown = "[asr]\nunknown = true\n";
    let err = AppConfig::from_toml(unknown).expect_err("unknown keys must fail");
    assert!(matches!(err, ConfigError::UnknownField { .. }));

    for backend in ["cloud-vad", "sensevoice-vad"] {
        let input = format!("[asr]\nstreaming_backend = \"{backend}\"\n");
        let err = AppConfig::from_toml(&input).expect_err("legacy backend must fail");
        assert!(err.to_string().contains("cloud-realtime"));
    }
}

#[cfg(unix)]
#[test]
fn rejects_cloud_keys_in_group_readable_config_files() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().expect("temporary directory");
    let path = root.path().join("config.toml");
    fs::write(
        &path,
        "[cloud]\napi_key = \"sk-test\"\n[asr]\nstreaming_backend = \"cloud-realtime\"\n",
    )
    .expect("config write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("permissions");

    let err = AppConfig::load(&path).expect_err("wide permissions must fail");
    assert!(err.to_string().contains("0600"));
}

#[test]
fn processing_defaults_point_at_bailian_deepseek_flash() {
    let config = ProcessingConfig::default();
    assert_eq!(config.provider, "none");
    assert_eq!(
        config.base_url,
        "https://dashscope.aliyuncs.com/compatible-mode/v1"
    );
    assert_eq!(config.model, "deepseek-v4-flash-0731");
    assert!(config.api_key.is_empty());
    assert!(config.api_key_env.is_empty());
}

#[test]
fn processing_accepts_a_direct_api_key() {
    let input = "[processing]\nprovider = \"openai-compatible\"\napi_key = \"sk-direct\"\n";
    let config = AppConfig::from_toml(input).expect("valid processing config");
    assert_eq!(config.processing.provider, "openai-compatible");
    assert_eq!(config.processing.api_key, "sk-direct");
    assert_eq!(config.processing.model, "deepseek-v4-flash-0731");
}

#[test]
fn processing_prompt_defaults_empty_and_accepts_override() {
    let default = ProcessingConfig::default();
    assert!(default.prompt.is_empty());

    let input = "[processing]\nprompt = \"整理：{text}\"\n";
    let config = AppConfig::from_toml(input).expect("valid prompt config");
    assert_eq!(config.processing.prompt, "整理：{text}");
}
