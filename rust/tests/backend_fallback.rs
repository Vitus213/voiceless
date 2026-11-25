//! `asr.fallback_backend`: when the primary backend cannot open before a
//! session starts, the fallback is tried and a warning event says so.
//! Data and config dirs point at a temp dir, never the user's home.
//! Runs the real `stream::run_with_env` with a test event sink. Both
//! backends fail to open, so no audio capture starts.

use voiceless::coordinator::{EventSink, OutputEvent};
use voiceless::stream::{run_with_env, SessionEnv, StreamError, StreamOptions};

struct Events(tokio::sync::mpsc::UnboundedSender<OutputEvent>);

impl EventSink for Events {
    fn emit(&mut self, event: OutputEvent) -> std::io::Result<()> {
        self.0.send(event).map_err(std::io::Error::other)
    }
}

/// `local-offline` has no installed model: a warning names the fallback
/// and the primary's reason; the fallback's own credential error is what
/// the command returns.
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

    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (_control_tx, control) = tokio::sync::mpsc::channel(1);
    let result = run_with_env(
        StreamOptions {
            config_path: Some(config),
            backend: None,
            json: true,
            inject: false,
            mode: "quick".to_owned(),
            context: Default::default(),
        },
        control,
        SessionEnv {
            warm: None,
            sink: Some(Box::new(Events(events))),
        },
    )
    .await;

    // The fallback has no credentials either: its own error surfaces.
    assert!(
        matches!(result, Err(StreamError::MissingDoubaoCredentials)),
        "{result:?}"
    );
    assert!(matches!(received.try_recv(), Ok(OutputEvent::Output(_))));
    let OutputEvent::Warning(warning) = received.try_recv().expect("fallback warning event") else {
        panic!("expected the fallback warning");
    };
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
