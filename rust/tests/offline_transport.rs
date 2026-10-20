//! Contract tests for the `local-offline` backend that need no model:
//! config validation, catalog consistency, and load-failure reporting.

use voiceless::config::AppConfig;
use voiceless::offline_asr::{OfflineAsrError, OfflineFamily, OfflineTransport};
use voiceless::realtime::RealtimeEvent;

#[test]
fn local_offline_config_is_validated() {
    let config = AppConfig::from_toml("[asr]\nstreaming_backend = \"local-offline\"\n").unwrap();
    assert_eq!(config.local.model, "qwen3-asr-0.6b-int8");
    assert_eq!(config.local.provider, "cpu");
    assert!(AppConfig::from_toml("[local]\nprovider = \"rocm\"\n").is_err());
    assert!(AppConfig::from_toml("[local]\nmodel = \"whisper\"\n").is_err());
    assert!(AppConfig::from_toml("[local]\nmax_segment_seconds = 60.0\n").is_err());
    assert!(AppConfig::from_toml(
        "[local]\nmodel = \"sense-voice-int8\"\nprovider = \"cuda\"\nhotwords = [\"豆包\"]\n"
    )
    .is_ok());
}

#[test]
fn offline_catalog_entries_are_consistent() {
    let catalog = voiceless::models::catalog();
    for id in ["qwen3-asr-0.6b-int8", "sense-voice-int8", "silero-vad"] {
        let spec = catalog
            .iter()
            .find(|spec| spec.id == id)
            .unwrap_or_else(|| panic!("{id} in catalog"));
        assert!(spec.url.starts_with("https://"));
        assert!(spec.sha256_sri.starts_with("sha256-"));
        for required in &spec.required_paths {
            assert!(
                spec.allowed_members.contains(required),
                "{id}: required {required} must be an allowed member"
            );
        }
    }
    assert_eq!(
        OfflineFamily::for_model("qwen3-asr-0.6b-int8").unwrap(),
        OfflineFamily::Qwen3Asr
    );
    assert_eq!(
        OfflineFamily::for_model("sense-voice-int8").unwrap(),
        OfflineFamily::SenseVoice
    );
}

#[tokio::test]
async fn load_failure_is_reported_after_ready() {
    let transport = OfflineTransport::start(|| {
        Err(OfflineAsrError::ModelDirectoryMissing(
            "/nonexistent".into(),
        ))
    });
    // Capture may start before the model is up; audio sent meanwhile is
    // accepted, and the load failure ends the session with an error.
    assert_eq!(transport.next_event().await.unwrap(), RealtimeEvent::Ready);
    transport.send_audio(&[0_u8; 1_024]).ok();
    match transport.next_event().await.unwrap() {
        RealtimeEvent::Error(message) => assert!(message.contains("/nonexistent")),
        other => panic!("expected load error, got {other:?}"),
    }
}
