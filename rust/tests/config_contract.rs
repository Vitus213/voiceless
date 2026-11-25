//! Contract of `config::AppConfig`: the documented defaults, the strict
//! schema (unknown keys and the legacy `cloud-vad`/`sensevoice-vad`
//! backends are rejected with the replacement named), the 0600 secret
//! gate of `AppConfig::load`, and the `[processing]` defaults and
//! overrides. Fixtures: TOML snippets parsed with `from_toml`; a real
//! temporary file (`tempfile`) for the permission check. No fakes.

use std::fs;

use tempfile::tempdir;
use voiceless::config::{AppConfig, ConfigError, ProcessingConfig};

#[test]
fn defaults_to_cloud_realtime_without_a_legacy_final_backend() {
    let config = AppConfig::default();

    assert_eq!(config.asr.batch_backend, "cloud");
    assert_eq!(config.asr.streaming_backend, "cloud-realtime");
    assert!(config.asr.final_backend.is_none());
}

/// An unknown field is a schema error. Removed ASR settings explain the
/// migration before the runtime can open a recognizer or microphone.
#[test]
fn rejects_unknown_keys_and_legacy_streaming_backends() {
    let unknown = "[asr]\nunknown = true\n";
    let err = AppConfig::from_toml(unknown).expect_err("unknown keys must fail");
    assert!(matches!(err, ConfigError::UnknownField { .. }));

    for (setting, replacement) in [
        ("streaming_backend = \"cloud-vad\"", "cloud-realtime"),
        ("fallback_backend = \"sensevoice-vad\"", "local-offline"),
        ("final_backend = \"cloud\"", "remove asr.final_backend"),
    ] {
        let input = format!("[asr]\n{setting}\n");
        let error = AppConfig::from_toml(&input).expect_err("legacy setting must fail");
        let message = error.to_string();
        assert!(message.contains(replacement), "{message}");
        assert!(message.contains("migration"), "{message}");
    }
}

#[test]
fn accepts_current_realtime_and_batch_backends() {
    for backend in [
        "cloud-realtime",
        "doubao-realtime",
        "local-streaming",
        "local-offline",
    ] {
        let config = AppConfig::from_toml(&format!(
            "[asr]\nstreaming_backend = \"{backend}\"\nfallback_backend = \"{backend}\"\n",
        ))
        .expect("current streaming and fallback backend");
        assert_eq!(config.asr.streaming_backend, backend);
        assert_eq!(config.asr.fallback_backend.as_deref(), Some(backend));
    }
    for backend in ["cloud", "local", "sensevoice"] {
        let config = AppConfig::from_toml(&format!("[asr]\nbatch_backend = \"{backend}\"\n"))
            .expect("supported batch backend or alias");
        assert_eq!(config.asr.batch_backend, backend);
    }
    for backend in ["fake", "qwen3-sherpa", "hybrid"] {
        let error = AppConfig::from_toml(&format!("[asr]\nbatch_backend = \"{backend}\"\n"))
            .expect_err("retired batch backend");
        let message = error.to_string();
        assert!(message.contains("asr.batch_backend"), "{message}");
        assert!(message.contains(backend), "{message}");
        assert!(message.contains("cloud or local"), "{message}");
    }
}

#[test]
fn malformed_config_reports_location_without_printing_credentials() {
    for input in [
        "[cloud]\napi_key = test-secret-without-quotes\n",
        "[doubao]\naccess_key = [\"test-secret-in-array\"]\n",
    ] {
        let message = AppConfig::from_toml(input).unwrap_err().to_string();
        assert!(message.contains("line 2"), "{message}");
        assert!(message.contains("credential field"), "{message}");
        assert!(!message.contains("test-secret"), "{message}");
    }
}

/// A 0640 file that sets `cloud.api_key` must be refused with the 0600
/// message before its content is used.
#[cfg(unix)]
#[test]
fn rejects_cloud_keys_in_group_readable_config_files() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().expect("temporary directory");
    let path = root.path().join("config.toml");
    for input in [
        "[cloud]\napi_key = \"sk-test\"\n",
        "[cloud]\n\"\\u0061pi_key\" = \"sk-test\"\n",
    ] {
        fs::write(&path, input).expect("config write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("permissions");
        let error = AppConfig::load(&path).expect_err("wide permissions must fail");
        assert!(error.to_string().contains("0600"));
    }
}

#[test]
fn cli_config_patch_validates_and_preserves_the_previous_file_on_failure() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let root = tempdir().unwrap();
    let path = root.path().join("config.toml");
    let patch = |body: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_voiceless"))
            .arg("--config")
            .arg(&path)
            .args(["config", "patch"])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let original = "# my config\n[local]\nnum_threads = 4 # number of workers\n";
    fs::write(&path, original).unwrap();
    let output = patch(r#"{"local":{"bad_field":true}}"#);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bad_field"));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    let threads = r#"{"local":{"num_threads":8}}"#;
    let lock = path.with_extension("toml.lock");
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    let output = patch(threads);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("config.toml.lock"));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    fs::remove_dir(&lock).unwrap();
    let output = patch(threads);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(AppConfig::load(&path).unwrap().local.num_threads, 8);
    assert!(fs::read_to_string(&path)
        .unwrap()
        .contains("num_threads = 8 # number of workers"));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
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

/// Setting only `provider` and `api_key` keeps the default model.
#[test]
fn processing_accepts_a_direct_api_key() {
    let input = "[processing]\nprovider = \"openai-compatible\"\napi_key = \"sk-direct\"\n";
    let config = AppConfig::from_toml(input).expect("valid processing config");
    assert_eq!(config.processing.provider, "openai-compatible");
    assert_eq!(config.processing.api_key, "sk-direct");
    assert_eq!(config.processing.model, "deepseek-v4-flash-0731");
}

/// The `{text}` placeholder is stored verbatim; expansion happens in
/// `modes::render_template`, not at load time.
#[test]
fn processing_prompt_defaults_empty_and_accepts_override() {
    let default = ProcessingConfig::default();
    assert!(default.prompt.is_empty());

    let input = "[processing]\nprompt = \"整理：{text}\"\n";
    let config = AppConfig::from_toml(input).expect("valid prompt config");
    assert_eq!(config.processing.prompt, "整理：{text}");
}
