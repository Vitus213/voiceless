//! Real local ASR smoke: requires an installed
//! `streaming-paraformer-bilingual-zh-en` model and `VOICELESS_REAL_LOCAL_ASR=1`.
//! Ignored by default so CI without the model never fabricates a pass.
//! Uses the real `ModelManager` in the user's XDG data dir (no fixtures):
//! the model is resolved and integrity-checked exactly as `stream.rs`
//! does before a session, then `LocalStreamingRecognizer` is fed one
//! second of synthetic PCM.

use std::time::Duration;

use voiceless::local_asr::LocalStreamingRecognizer;
use voiceless::models::{
    default_cache_dir, default_data_dir, streaming_paraformer_spec, ModelManager,
};
use voiceless::realtime::RealtimeEvent;

/// Without the env var the test returns early (and is ignored anyway).
/// With it: the installed model passes the integrity check, the recognizer
/// builds, a 440 Hz sine yields only Partial/Completed events (if any),
/// and `finish` always produces a Finished event.
#[test]
#[ignore = "requires VOICELESS_REAL_LOCAL_ASR=1 and the installed streaming model"]
fn real_local_asr_recognizer_processes_and_finishes() {
    if std::env::var("VOICELESS_REAL_LOCAL_ASR").unwrap_or_default() != "1" {
        eprintln!("set VOICELESS_REAL_LOCAL_ASR=1 to run the real local ASR smoke");
        return;
    }
    let spec = streaming_paraformer_spec();
    let manager = ModelManager::new(&default_data_dir(), &default_cache_dir());
    let payload = manager
        .resolve(&spec.id)
        .expect("resolve installed model")
        .expect("streaming model installed; run `voiceless model install streaming-paraformer-bilingual-zh-en`");
    let (_, report) = manager.check(&spec).expect("check model");
    assert!(report.ok(), "installed model must be intact: {report:?}");

    let mut recognizer = LocalStreamingRecognizer::new(&payload)
        .expect("recognizer must build from the installed model");

    // One second of 440 Hz sine at 16 kHz mono PCM16.
    let mut pcm = Vec::with_capacity(32_000);
    for index in 0..16_000 {
        let sample = (440.0 * 2.0 * std::f64::consts::PI * index as f64 / 16_000.0).sin() * 0.25;
        let value = (sample * f64::from(i16::MAX)) as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    let events = recognizer.accept_pcm(&pcm).expect("accept real PCM");
    for event in &events {
        assert!(matches!(
            event,
            RealtimeEvent::Partial { .. } | RealtimeEvent::Completed { .. }
        ));
    }
    // Sine carries no speech; finish must still complete without error.
    let finished = recognizer.finish().expect("finish must not fail");
    assert!(finished
        .iter()
        .any(|event| matches!(event, RealtimeEvent::Finished { .. })));
    let _ = Duration::from_secs(1);
}
