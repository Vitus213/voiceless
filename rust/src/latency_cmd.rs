//! `voiceless benchmark latency`: run real cloud sessions with Wayland
//! injection and record per-stage timestamps. Each trial replays one corpus
//! sample at the 32 ms pace through `cloud-realtime`, stops normally,
//! injects the final text via the configured `[inject]` method and records
//! stage timestamps.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::{load_default_config, AppConfig};
use crate::latency::{LatencyThresholds, TrialOutcome, TrialStage};
use crate::realtime::{RealtimeEvent, RealtimeSession};
use crate::stream::inject_text;

const CHUNK_INTERVAL: Duration = Duration::from_millis(32);

#[derive(Debug, Clone)]
pub struct LatencyBenchmarkArgs {
    pub trials: usize,
    pub backend: String,
    pub mode: String,
    pub inject: bool,
    pub corpus_path: PathBuf,
    pub audio_dir: PathBuf,
    pub report_path: PathBuf,
    pub enforce: bool,
}

impl Default for LatencyBenchmarkArgs {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyBenchmarkArgs {
    pub fn new() -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let benchmark_root = root.join("scripts/asr_benchmark");
        Self {
            trials: 100,
            backend: "cloud-realtime".to_owned(),
            mode: "quick".to_owned(),
            inject: true,
            corpus_path: benchmark_root.join("corpus/test.jsonl"),
            audio_dir: benchmark_root.join("audio"),
            report_path: benchmark_root.join("reports"),
            enforce: false,
        }
    }
}

pub async fn run_latency(args: LatencyBenchmarkArgs) -> i32 {
    if args.backend != "cloud-realtime" {
        eprintln!("Voiceless: latency benchmark only supports cloud-realtime");
        return 2;
    }
    if args.mode != "quick" {
        eprintln!("Voiceless: non-quick modes must not feed the 1s latency gate; use --mode quick");
        return 2;
    }
    let config = match load_default_config() {
        Ok(config) if !config.cloud.api_key.trim().is_empty() => config,
        Ok(_) => {
            eprintln!("Voiceless: cloud.api_key missing; latency benchmark skipped (unverified)");
            return 2;
        }
        Err(error) => {
            eprintln!("Voiceless: {error}; latency benchmark skipped (unverified)");
            return 2;
        }
    };

    let entries = match crate::benchmark::load_corpus(&args.corpus_path) {
        Ok(entries) if !entries.is_empty() => entries,
        Ok(_) => {
            eprintln!(
                "Voiceless: corpus {} is empty; latency benchmark skipped (unverified)",
                args.corpus_path.display()
            );
            return 2;
        }
        Err(error) => {
            eprintln!("Voiceless: cannot read corpus: {error}; skipped (unverified)");
            return 2;
        }
    };
    let mut pcm_pool: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in &entries {
        let wav = args.audio_dir.join(format!("{}.wav", entry.id));
        match crate::batch::load_wav_pcm16(&wav) {
            Ok(pcm) => pcm_pool.push((entry.id.clone(), pcm)),
            Err(_) => {
                eprintln!(
                    "Voiceless: audio missing for {}; latency benchmark skipped (unverified)",
                    entry.id
                );
                return 2;
            }
        }
    }

    let mut outcomes = Vec::new();
    for trial_index in 0..args.trials {
        let (id, pcm) = &pcm_pool[trial_index % pcm_pool.len()];
        let outcome = run_trial(trial_index, &config, id, pcm, args.inject).await;
        let success = outcome.success;
        outcomes.push(outcome);
        if !success {
            eprintln!(
                "trial {trial_index} failed: {}",
                outcomes
                    .last()
                    .and_then(|last| last.error.clone())
                    .unwrap_or_default()
            );
        }
    }

    let thresholds = LatencyThresholds::default();
    let gate = crate::latency::evaluate_gate(&outcomes, &thresholds);

    std::fs::create_dir_all(&args.report_path).ok();
    let timestamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .replace([':', '+'], "-");
    let path = args
        .report_path
        .join(format!("latency-{}-{timestamp}.json", args.backend));
    match serde_json::to_string_pretty(&gate) {
        Ok(json) => {
            if let Err(error) = std::fs::write(&path, json) {
                eprintln!("Voiceless: cannot write report: {error}");
                return 1;
            }
            println!("report: {}", path.display());
        }
        Err(error) => {
            eprintln!("Voiceless: cannot serialize report: {error}");
            return 1;
        }
    }

    println!(
        "trials={} failed={} unverified={} first_partial_p95={:?} stop_to_final_p50={:?} stop_to_inject_p99={:?}",
        gate.trials,
        gate.failed_trials,
        gate.unverified,
        gate.first_partial_p95,
        gate.stop_to_final_p50,
        gate.stop_to_inject_p99
    );
    if gate.unverified {
        eprintln!(
            "Voiceless: fewer than {} successful trials; gate unverified",
            thresholds.min_trials
        );
        return 2;
    }
    if gate.passed {
        println!("Voiceless: latency gate passed");
        0
    } else {
        eprintln!("Voiceless: latency gate not passed");
        1
    }
}

async fn run_trial(
    id: usize,
    config: &AppConfig,
    sample_id: &str,
    pcm: &[u8],
    inject: bool,
) -> TrialOutcome {
    let mut stages = Vec::new();
    let start = Instant::now();
    stages.push((TrialStage::Start, 0.0));

    let mut session = match RealtimeSession::connect(
        &config.cloud.realtime_endpoint,
        &config.cloud.api_key,
        &config.cloud.realtime_model,
    )
    .await
    {
        Ok(session) => session,
        Err(error) => return failed(id, sample_id, &stages, start, error.to_string()),
    };
    match wait_ready(&session).await {
        Ok(()) => stages.push((TrialStage::ConnectReady, start.elapsed().as_secs_f64())),
        Err(error) => return failed(id, sample_id, &stages, start, error),
    }

    let mut confirmed: Vec<String> = Vec::new();
    let mut utterance_started = false;
    for chunk in pcm.chunks(crate::capture::CHUNK_BYTES) {
        // Anchor "start of speech" at the first voiced chunk; leading
        // silence in the corpus cannot count toward the user-facing SLO.
        if !utterance_started && chunk_is_voiced(chunk) {
            stages.push((TrialStage::UtteranceStart, start.elapsed().as_secs_f64()));
            utterance_started = true;
        }
        if let Err(error) = session.send_audio(chunk).await {
            return failed(id, sample_id, &stages, start, error.to_string());
        }
        // Pace capture while draining events immediately, matching the
        // production select loop where partials are consumed as they arrive.
        let pace_until = Instant::now() + CHUNK_INTERVAL;
        loop {
            let now = Instant::now();
            if now >= pace_until {
                break;
            }
            match tokio::time::timeout(pace_until - now, session.next_event()).await {
                Ok(Ok(event)) => collect(&mut stages, &mut confirmed, event, start),
                Ok(Err(error)) => {
                    return failed(id, sample_id, &stages, start, error.to_string());
                }
                Err(_) => break,
            }
        }
    }
    let stop_at = Instant::now();
    stages.push((
        TrialStage::Stop,
        stop_at.duration_since(start).as_secs_f64(),
    ));

    if let Err(error) = session.finish().await {
        return failed(id, sample_id, &stages, start, error.to_string());
    }
    stages.push((TrialStage::FinishSent, start.elapsed().as_secs_f64()));

    let mut final_text: Option<String> = None;
    loop {
        match session.next_event().await {
            Ok(RealtimeEvent::Finished { transcript }) => {
                if !transcript.is_empty() {
                    final_text = Some(transcript);
                }
                stages.push((TrialStage::FinalReceived, start.elapsed().as_secs_f64()));
                break;
            }
            Ok(event) => collect(&mut stages, &mut confirmed, event, start),
            Err(error) => {
                return failed(id, sample_id, &stages, start, error.to_string());
            }
        }
    }
    let text = final_text.unwrap_or_else(|| confirmed.join(""));
    if text.trim().is_empty() {
        return failed(id, sample_id, &stages, start, "no speech text".to_owned());
    }

    if inject {
        let result = inject_text(&config.inject, &text).await;
        stages.push((TrialStage::InjectionComplete, start.elapsed().as_secs_f64()));
        if !result.ok {
            return failed(
                id,
                sample_id,
                &stages,
                start,
                format!("injection: {}", result.message),
            );
        }
    }

    TrialOutcome {
        id,
        mode: "quick".to_owned(),
        stages,
        success: true,
        error: None,
    }
}

#[allow(clippy::ptr_arg)]
fn collect(
    stages: &mut Vec<(TrialStage, f64)>,
    confirmed: &mut Vec<String>,
    event: RealtimeEvent,
    start: Instant,
) {
    match event {
        RealtimeEvent::Partial { .. }
            if !stages
                .iter()
                .any(|(stage, _)| *stage == TrialStage::FirstPartial) =>
        {
            stages.push((TrialStage::FirstPartial, start.elapsed().as_secs_f64()));
        }
        RealtimeEvent::Completed { transcript } if !transcript.is_empty() => {
            confirmed.push(transcript);
        }
        _ => {}
    }
}

fn failed(
    id: usize,
    _sample_id: &str,
    stages: &[(TrialStage, f64)],
    start: Instant,
    message: String,
) -> TrialOutcome {
    let _ = start;
    TrialOutcome {
        id,
        mode: "quick".to_owned(),
        stages: stages.to_vec(),
        success: false,
        error: Some(message),
    }
}

/// 32 ms PCM16 chunk is voiced when its RMS amplitude crosses the speech
/// threshold; silence and noise floor do not start the SLO clock.
fn chunk_is_voiced(chunk: &[u8]) -> bool {
    const VOICE_RMS_THRESHOLD: f64 = 300.0;
    if chunk.len() < 2 {
        return false;
    }
    let sum: f64 = chunk
        .chunks_exact(2)
        .map(|bytes| {
            let sample = f64::from(i16::from_le_bytes([bytes[0], bytes[1]]));
            sample * sample
        })
        .sum();
    let rms = (sum / (chunk.len() / 2) as f64).sqrt();
    rms > VOICE_RMS_THRESHOLD
}

async fn wait_ready(session: &RealtimeSession) -> Result<(), String> {
    loop {
        match session.next_event().await {
            Ok(RealtimeEvent::Ready) => return Ok(()),
            Ok(_) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
}
