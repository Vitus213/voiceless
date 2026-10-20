//! Benchmark an offline model on WAV files: load time, whole-file decode
//! time, and the stop-to-final latency of the VAD-segmented path (the time
//! `finish()` takes once all audio has been fed).
//!
//! cargo run --release --example offline_bench -- <model-id> <model-dir> <cpu|cuda> <threads> <silero_vad.onnx> <wav>...

use std::path::Path;
use std::time::{Duration, Instant};

use voiceless::offline_asr::{
    create_recognizer, create_vad, decode, OfflineFamily, OfflineOptions, SegmentedRecognizer,
};
use voiceless::realtime::RealtimeEvent;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [family, model_dir, provider, threads, vad_model, wavs @ ..] = args.as_slice() else {
        eprintln!(
            "usage: offline_bench <model-id> <model-dir> <cpu|cuda> <threads> <vad> <wav>..."
        );
        std::process::exit(2);
    };
    let family = OfflineFamily::for_model(family).expect("family");
    let options = OfflineOptions {
        provider: provider.clone(),
        num_threads: threads.parse().expect("threads"),
        hotwords: Vec::new(),
    };

    let started = Instant::now();
    let recognizer = create_recognizer(family, Path::new(model_dir), &options).expect("recognizer");
    println!("load {:.2}s", started.elapsed().as_secs_f64());
    // Warm-up: the first CUDA run pays kernel/graph setup.
    let started = Instant::now();
    decode(&recognizer, &vec![0.0; 16_000]);
    println!("warmup {:.2}s", started.elapsed().as_secs_f64());

    let mut total_audio = 0.0;
    let mut total_decode = 0.0;
    for wav in wavs {
        let wave = sherpa_onnx::Wave::read(wav).expect("wav");
        let seconds = wave.samples().len() as f64 / f64::from(wave.sample_rate());
        let started = Instant::now();
        let text = decode(&recognizer, wave.samples());
        let elapsed = started.elapsed().as_secs_f64();
        total_audio += seconds;
        total_decode += elapsed;
        println!(
            "{seconds:6.1}s audio  {elapsed:5.2}s decode  rtf {:.3}  {text}",
            elapsed / seconds
        );
    }
    if total_audio > 0.0 {
        println!(
            "TOTAL rtf {:.3} ({total_decode:.1}s / {total_audio:.1}s)",
            total_decode / total_audio
        );
    }

    // Segmented path on the longest file: stop-to-final latency.
    if let Some(wav) = wavs.first() {
        let wave = sherpa_onnx::Wave::read(wav).expect("wav");
        let recognizer =
            create_recognizer(family, Path::new(model_dir), &options).expect("recognizer");
        let vad = create_vad(Path::new(vad_model), 20.0).expect("vad");
        let mut segmented = SegmentedRecognizer::new(recognizer, vad, Duration::from_millis(500));
        let mut confirmed = 0;
        for chunk in wave.samples().chunks(512) {
            for event in segmented.accept(chunk) {
                if matches!(event, RealtimeEvent::Completed { .. }) {
                    confirmed += 1;
                }
            }
        }
        let started = Instant::now();
        let events = segmented.finish();
        let elapsed = started.elapsed().as_secs_f64();
        if let Some(RealtimeEvent::Finished { transcript }) = events.last() {
            println!("SEGMENTED {confirmed} segments before stop; stop->final {elapsed:.2}s");
            println!("  {transcript}");
        }
    }
}
