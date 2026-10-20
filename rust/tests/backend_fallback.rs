//! `asr.fallback_backend`: when the primary backend cannot open before a
//! session starts, the fallback is tried and a warning event says so.
//! Data and config dirs point at a temp dir, never the user's home.

use voiceless::stream::{run_with_env, SessionEnv, StreamError, StreamOptions};

#[tokio::test]
async fn missing_local_model_falls_back_and_reports_the_switch() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_DATA_HOME", root.path().join("data"));
    std::env::set_var("XDG_CACHE_HOME", root.path().join("cache"));
    std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        "[asr]\nstreaming_backend = \"local-offline\"\nfallback_backend = \"doubao-realtime\"\n\n[history]\nenabled = false\n",
    )
    .unwrap();

    let (events, mut received) = tokio::sync::broadcast::channel(16);
    let (_control_tx, control) = tokio::sync::mpsc::channel(1);
    let result = run_with_env(
        StreamOptions {
            config_path: Some(config),
            backend: None,
            json: true,
            inject: false,
            mode: "quick".to_owned(),
        },
        control,
        SessionEnv {
            warm: None,
            events: Some(events),
        },
    )
    .await;

    // The fallback has no credentials either: its own error surfaces.
    assert!(
        matches!(result, Err(StreamError::MissingDoubaoCredentials)),
        "{result:?}"
    );
    let warning = received.try_recv().expect("fallback warning event");
    assert!(warning.contains(r#""type":"warning""#), "{warning}");
    assert!(
        warning.contains("falling back to doubao-realtime"),
        "{warning}"
    );
    assert!(warning.contains("is not installed"), "{warning}");
}

#[test]
fn fallback_backend_is_validated() {
    use voiceless::config::AppConfig;
    assert!(AppConfig::from_toml("[asr]\nfallback_backend = \"whisper\"\n").is_err());
    assert!(AppConfig::from_toml("[asr]\nfallback_backend = \"doubao-realtime\"\n").is_ok());
}
