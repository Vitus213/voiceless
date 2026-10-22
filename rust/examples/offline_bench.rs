//! Benchmark an offline model on WAV files: load time, whole-file decode
//! time, and the VAD-segmented path (stop-to-final latency, transcript with
//! segment context).
//!
//! cargo run --release [--features cuda] --example offline_bench -- \
//!     <model-id> <model-dir> <cpu|cuda> <threads> <silero_vad.onnx> <wav>...
//!
//! `model-id` is a catalog id; `OfflineFamily::for_model` picks the engine
//! from it (`*-gguf` through transcribe.cpp, other `qwen3-asr*` and
//! `sense-voice*` through Sherpa-ONNX). `model-dir` is the payload
//! directory, `cpu|cuda` the execution provider, `threads` the CPU thread
//! count. WAVs must be 16 kHz mono: the decoders assume that rate and do
//! not resample. Per file it prints audio seconds, decode seconds and RTF
//! (decode / audio), then a total. Afterwards the first file is re-run
//! through `SegmentedRecognizer` the way a live session consumes audio
//! (512-sample VAD windows, 500 ms partial interval, 20 s segment cap) and
//! the time spent in `finish` is reported as stop-to-final. Exit 2 on a
//! usage error; any other failure panics with its message. Dev tool only,
//! not part of the quality or latency gates.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use voiceless::offline_asr::{
    create_decoder, create_vad, Decoder, OfflineFamily, OfflineOptions, SegmentedRecognizer,
};
use voiceless::realtime::RealtimeEvent;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [model_id, model_dir, provider, threads, vad_model, wavs @ ..] = args.as_slice() else {
        eprintln!(
            "usage: offline_bench <model-id> <model-dir> <cpu|cuda> <threads> <vad> <wav>..."
        );
        std::process::exit(2);
    };
    let family = OfflineFamily::for_model(model_id).expect("family");
    let options = OfflineOptions {
        provider: provider.clone(),
        num_threads: threads.parse().expect("threads"),
        hotwords: Vec::new(),
    };

    let started = Instant::now();
    let decoder: Arc<dyn Decoder> =
        create_decoder(family, Path::new(model_dir), &options).expect("decoder");
    println!("load {:.2}s", started.elapsed().as_secs_f64());

    let mut total_audio = 0.0;
    let mut total_decode = 0.0;
    for wav in wavs {
        let wave = sherpa_onnx::Wave::read(wav).expect("wav");
        let seconds = wave.samples().len() as f64 / f64::from(wave.sample_rate());
        let started = Instant::now();
        let text = decoder.decode(wave.samples(), "");
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

    // Segmented path on the first file: what a live session produces.
    // 512 samples is one Silero VAD window; 20 s caps a single segment so
    // a pause-free recording still yields intermediate `Completed` events.
    if let Some(wav) = wavs.first() {
        let wave = sherpa_onnx::Wave::read(wav).expect("wav");
        let vad = create_vad(Path::new(vad_model), 20.0).expect("vad");
        let mut segmented =
            SegmentedRecognizer::new(decoder.clone(), vad, Duration::from_millis(500));
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
