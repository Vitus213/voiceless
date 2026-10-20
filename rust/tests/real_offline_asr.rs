//! Real `local-offline` smoke: streams the Qwen3-ASR sample through the
//! VAD-segmented transport. Requires `VOICELESS_REAL_OFFLINE_ASR=1`, the
//! installed `silero-vad` and `qwen3-asr-0.6b-int8` models (the sample wav
//! ships with the latter) and, for `VOICELESS_OFFLINE_MODEL`, that model
//! (GGUF ones need `--features cuda` and `VOICELESS_OFFLINE_PROVIDER=cuda`).
//! Ignored by default so CI without the models never fabricates a pass.

use std::time::Duration;

use voiceless::models::installed_model_dir;
use voiceless::offline_asr::{
    create_decoder, create_vad, OfflineFamily, OfflineOptions, OfflineTransport,
    SegmentedRecognizer,
};
use voiceless::realtime::RealtimeEvent;

#[tokio::test]
#[ignore = "requires VOICELESS_REAL_OFFLINE_ASR=1 and installed models"]
async fn qwen3_transcribes_the_sample_through_vad_segments() {
    if std::env::var("VOICELESS_REAL_OFFLINE_ASR").unwrap_or_default() != "1" {
        eprintln!("set VOICELESS_REAL_OFFLINE_ASR=1 to run the real offline ASR smoke");
        return;
    }
    let model_id =
        std::env::var("VOICELESS_OFFLINE_MODEL").unwrap_or_else(|_| "qwen3-asr-0.6b-int8".into());
    let provider = std::env::var("VOICELESS_OFFLINE_PROVIDER").unwrap_or_else(|_| "cpu".into());
    let sample_dir = installed_model_dir("qwen3-asr-0.6b-int8").expect("sample model installed");
    let model_dir = installed_model_dir(&model_id).expect("model under test installed");
    let vad_dir = installed_model_dir("silero-vad").expect("silero-vad installed");

    let sample = sample_dir.join("test_wavs/raokouling.wav");
    let wave = sherpa_onnx::Wave::read(sample.to_str().unwrap()).expect("sample wav");
    // The sample is 44.1 kHz; capture always delivers 16 kHz.
    let samples = sherpa_onnx::LinearResampler::create(wave.sample_rate(), 16_000)
        .expect("resampler")
        .resample(wave.samples(), true);
    let pcm: Vec<u8> = samples
        .iter()
        .flat_map(|sample| ((sample * f32::from(i16::MAX)) as i16).to_le_bytes())
        .collect();

    let mut transport = OfflineTransport::start(move || {
        let decoder = create_decoder(
            OfflineFamily::for_model(&model_id)?,
            &model_dir,
            &OfflineOptions {
                provider,
                num_threads: 4,
                hotwords: Vec::new(),
            },
        )?;
        let vad = create_vad(&vad_dir.join("silero_vad.onnx"), 15.0)?;
        Ok(SegmentedRecognizer::new(
            decoder,
            vad,
            Duration::from_millis(500),
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
