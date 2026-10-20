//! Real `local-offline` smoke: streams the Qwen3-ASR sample through the
//! VAD-segmented transport. Requires the installed `qwen3-asr-0.6b-int8` and
//! `silero-vad` models and `VOICELESS_REAL_OFFLINE_ASR=1`; ignored by default
//! so CI without the models never fabricates a pass.

use std::time::Duration;

use voiceless::models::{
    default_cache_dir, default_data_dir, qwen3_asr_spec, silero_vad_spec, ModelManager,
};
use voiceless::offline_asr::{
    create_recognizer, create_vad, OfflineFamily, OfflineOptions, OfflineTransport,
    SegmentedRecognizer,
};
use voiceless::realtime::RealtimeEvent;

#[tokio::test]
#[ignore = "requires VOICELESS_REAL_OFFLINE_ASR=1 and the installed qwen3-asr/silero-vad models"]
async fn qwen3_transcribes_the_sample_through_vad_segments() {
    if std::env::var("VOICELESS_REAL_OFFLINE_ASR").unwrap_or_default() != "1" {
        eprintln!("set VOICELESS_REAL_OFFLINE_ASR=1 to run the real offline ASR smoke");
        return;
    }
    let manager = ModelManager::new(&default_data_dir(), &default_cache_dir());
    let model = manager
        .check(&qwen3_asr_spec())
        .expect("qwen3-asr installed");
    let vad = manager
        .check(&silero_vad_spec())
        .expect("silero-vad installed");
    assert!(
        model.1.ok() && vad.1.ok(),
        "installed models must be intact"
    );
    let (model_dir, vad_dir) = (model.0, vad.0);
    let provider = std::env::var("VOICELESS_OFFLINE_PROVIDER").unwrap_or_else(|_| "cpu".into());

    let wave =
        sherpa_onnx::Wave::read(model_dir.join("test_wavs/raokouling.wav").to_str().unwrap())
            .expect("sample wav");
    // The sample is 44.1 kHz; capture always delivers 16 kHz.
    let samples = sherpa_onnx::LinearResampler::create(wave.sample_rate(), 16_000)
        .expect("resampler")
        .resample(wave.samples(), true);
    let pcm: Vec<u8> = samples
        .iter()
        .flat_map(|sample| ((sample * f32::from(i16::MAX)) as i16).to_le_bytes())
        .collect();

    let mut transport = OfflineTransport::start(move || {
        let recognizer = create_recognizer(
            OfflineFamily::Qwen3Asr,
            &model_dir,
            &OfflineOptions {
                provider,
                num_threads: 4,
                hotwords: Vec::new(),
            },
        )?;
        let vad = create_vad(&vad_dir.join("silero_vad.onnx"), 15.0)?;
        Ok(SegmentedRecognizer::new(
            recognizer,
            vad,
            Duration::from_millis(800),
        ))
    });
    assert_eq!(transport.next_event().await.unwrap(), RealtimeEvent::Ready);
    for chunk in pcm.chunks(1_024) {
        transport.send_audio(chunk).unwrap();
    }
    transport.finish().unwrap();

    let transcript = loop {
        match tokio::time::timeout(Duration::from_secs(60), transport.next_event())
            .await
            .expect("final within 60s")
            .unwrap()
        {
            RealtimeEvent::Finished { transcript } => break transcript,
            RealtimeEvent::Error(message) => panic!("offline ASR failed: {message}"),
            _ => {}
        }
    };
    eprintln!("{transcript}");
    for expected in ["广西壮族自治区", "出租车司机", "南宁市"] {
        assert!(
            transcript.contains(expected),
            "missing {expected}: {transcript}"
        );
    }
}
