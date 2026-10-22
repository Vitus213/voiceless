//! `voiceless benchmark` subcommands. The ASR quality gate replays versioned
//! corpus audio through the selected realtime backend at the real 32 ms
//! pace and writes a versioned JSON report; `--enforce` applies the
//! cloud-realtime CER threshold. Missing credentials, corpus or audio skip
//! loudly (exit 2) instead of producing a pass.
//!
//! Command: `voiceless benchmark asr [--split test] [--backend
//! cloud-realtime] [--enforce]`; `main.rs` builds `AsrBenchmarkArgs::new`
//! from the first two flags, sets `enforce` and awaits `run_asr`. The
//! metric and the report schema live in `benchmark`; this module adds the
//! real backends (`realtime::RealtimeSession` for DashScope,
//! `local_asr::LocalStreamingRecognizer` for the Sherpa-ONNX streaming
//! model), file discovery and the exit-code policy. The latency gate is a
//! separate command (`latency_cmd`).
//!
//! Files: `scripts/asr_benchmark/corpus/<split>.jsonl`,
//! `scripts/asr_benchmark/audio/<id>.wav` and
//! `scripts/asr_benchmark/reports/<backend>-<split>-<timestamp>.json`, all
//! resolved from the current working directory, so run it from the
//! repository root. Config: `cloud.api_key`, `cloud.realtime_endpoint` and
//! `cloud.realtime_model` from the XDG config file; the local backend
//! needs the `streaming-paraformer-bilingual-zh-en` model installed and
//! verified under the XDG data dir (`models.rs`).
//!
//! Exit codes: 0 when the report was written and the gate passed (or the
//! backend is local, whose CER is informational); 1 when a sample failed,
//! the enforced CER threshold was exceeded or the report could not be
//! written; 2 when skipped as unverified (missing corpus, audio, config,
//! credentials or model, or an unknown backend). A skip is never a pass.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::benchmark::{
    corpus_version, load_corpus, run_asr_benchmark_split, BackendFactory, BenchmarkReport,
};
use crate::config::{load_default_config, AppConfig};
use crate::realtime::{RealtimeEvent, RealtimeSession};

/// Pace between 32 ms chunks (1024 bytes at 16 kHz mono PCM16), the same
/// as live capture, so server-side endpointing behaves as in production.
const CHUNK_INTERVAL: Duration = Duration::from_millis(32);
/// Content CER budget for `cloud-realtime` under `--enforce` (README:
/// "CER <= 0.02", aligned with the measured 0.011 of qwen3-asr-flash).
/// Local backends have no budget; their CER is reported, never enforced.
pub const CLOUD_CER_THRESHOLD: f64 = 0.02;

/// Parameters of one `benchmark asr` run. `new` fills the repository
/// layout; `main.rs` overrides `enforce` from the CLI.
#[derive(Debug, Clone)]
pub struct AsrBenchmarkArgs {
    /// Corpus split name: selects `corpus_dir/<split>.jsonl` and is
    /// recorded in the report.
    pub split: String,
    /// `cloud-realtime` or `local-streaming`; anything else exits 2.
    pub backend: String,
    /// Directory holding `<split>.jsonl`.
    pub corpus_dir: PathBuf,
    /// Directory of `<id>.wav` files, one per corpus entry; any missing or
    /// undecodable file skips the whole run.
    pub audio_dir: PathBuf,
    /// Directory the JSON report is written into (created on demand).
    pub report_path: PathBuf,
    /// Apply `CLOUD_CER_THRESHOLD` (cloud backend only). Without it the CER
    /// is reported but never fails the run; failed samples still do.
    pub enforce: bool,
}

impl AsrBenchmarkArgs {
    /// Defaults rooted at `<cwd>/scripts/asr_benchmark` with `enforce`
    /// off; falls back to `.` when the working directory is unreadable.
    pub fn new(split: String, backend: String) -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let benchmark_root = root.join("scripts/asr_benchmark");
        Self {
            split,
            backend,
            corpus_dir: benchmark_root.join("corpus"),
            audio_dir: benchmark_root.join("audio"),
            report_path: benchmark_root.join("reports"),
            enforce: false,
        }
    }
}

/// Run the ASR quality benchmark and return the process exit code.
///
/// Checks run in this order and each skips with exit 2 before any network
/// or model work: corpus readable and non-empty; every sample's WAV present
/// and decodable as 16 kHz mono PCM16; config loadable; backend
/// prerequisites (`cloud.api_key` for the cloud, an installed and
/// checksum-verified streaming model for local). Then every sample is
/// replayed sequentially (a fresh session or recognizer per sample), the
/// report is written, a one-line summary goes to stdout and the gate is
/// evaluated: any failed sample is exit 1; the CER threshold applies only
/// with `--enforce` on `cloud-realtime`. Must run on a multi-thread tokio
/// runtime because `Backend::replay` uses `block_in_place`.
pub async fn run_asr(args: AsrBenchmarkArgs) -> i32 {
    let corpus_path = args.corpus_dir.join(format!("{}.jsonl", args.split));
    let entries = match load_corpus(&corpus_path) {
        Ok(entries) if !entries.is_empty() => entries,
        Ok(_) => {
            eprintln!(
                "Voiceless: corpus {} is empty; benchmark skipped (unverified)",
                corpus_path.display()
            );
            return 2;
        }
        Err(error) => {
            eprintln!(
                "Voiceless: cannot read corpus {}: {error}; benchmark skipped (unverified)",
                corpus_path.display()
            );
            return 2;
        }
    };

    let mut pcm_by_id = Vec::new();
    let mut missing_audio = Vec::new();
    for entry in &entries {
        let wav_path = args.audio_dir.join(format!("{}.wav", entry.id));
        // An undecodable WAV counts as missing: the gate must never score a
        // partial corpus, so all ids are collected and reported at once.
        match crate::batch::load_wav_pcm16(&wav_path) {
            Ok(pcm) => pcm_by_id.push((entry.id.clone(), pcm)),
            Err(_) => missing_audio.push(entry.id.clone()),
        }
    }
    if !missing_audio.is_empty() {
        eprintln!(
            "Voiceless: {} audio files missing from {} ({}); benchmark skipped (unverified)",
            missing_audio.len(),
            args.audio_dir.display(),
            missing_audio.join(", ")
        );
        return 2;
    }

    let config = match load_default_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Voiceless: {error}; benchmark skipped (unverified)");
            return 2;
        }
    };

    let version = corpus_version(&entries);
    let factory = match args.backend.as_str() {
        "cloud-realtime" => {
            if config.cloud.api_key.trim().is_empty() {
                eprintln!("Voiceless: cloud.api_key missing; benchmark skipped (unverified)");
                return 2;
            }
            Backend::Cloud {
                config: Box::new(config.clone()),
            }
        }
        "local-streaming" => {
            let spec = crate::models::streaming_paraformer_spec();
            let manager = crate::models::ModelManager::new(
                &crate::models::default_data_dir(),
                &crate::models::default_cache_dir(),
            );
            let payload = match manager.resolve(&spec.id) {
                Ok(Some(payload)) => payload,
                Ok(None) => {
                    eprintln!(
                        "Voiceless: streaming model not installed; benchmark skipped (unverified)"
                    );
                    return 2;
                }
                Err(error) => {
                    eprintln!("Voiceless: {error}; benchmark skipped (unverified)");
                    return 2;
                }
            };
            // Checksum-verify the installed payload first: a corrupted
            // model would produce a misleading CER, not a clean failure.
            match manager.check(&spec) {
                Ok((_, report)) if report.ok() => {}
                Ok((_, report)) => {
                    eprintln!(
                        "Voiceless: installed model is corrupted ({:?}); benchmark skipped (unverified)",
                        report
                    );
                    return 2;
                }
                Err(error) => {
                    eprintln!("Voiceless: {error}; benchmark skipped (unverified)");
                    return 2;
                }
            }
            Backend::Local { payload }
        }
        other => {
            eprintln!("Voiceless: unknown benchmark backend: {other}");
            return 2;
        }
    };

    let model = match args.backend.as_str() {
        "cloud-realtime" => config.cloud.realtime_model.clone(),
        _ => crate::models::streaming_paraformer_spec().id,
    };

    let mut factory = factory;
    let report = run_asr_benchmark_split(
        &entries,
        &pcm_by_id,
        &mut factory,
        &args.backend,
        &model,
        &version,
        &args.split,
        CHUNK_INTERVAL,
    );

    if let Err(error) = write_report(&args.report_path, &args.split, &args.backend, &report) {
        eprintln!("Voiceless: cannot write report: {error}");
        return 1;
    }

    println!(
        "backend={} split={} samples={} failures={} content_cer={:.4}",
        report.backend,
        report.split,
        report.samples.len(),
        report.failures,
        report.content_cer
    );

    // Failed samples block regardless of --enforce or backend: a sample
    // that was never transcribed is unverified, not a pass.
    if report.failures > 0 {
        eprintln!(
            "Voiceless: {} samples failed; gate not passed",
            report.failures
        );
        return 1;
    }
    if args.enforce && args.backend == "cloud-realtime" && report.content_cer > CLOUD_CER_THRESHOLD
    {
        eprintln!(
            "Voiceless: content CER {:.4} exceeds threshold {CLOUD_CER_THRESHOLD}; gate not passed",
            report.content_cer
        );
        return 1;
    }
    if args.backend != "cloud-realtime" {
        // Local reports are informational: no cross-backend threshold applies
        // and they never merge with the cloud gate.
        println!("Voiceless: local CER reported separately; no gate enforced");
    } else if args.enforce {
        println!("Voiceless: quality gate passed");
    }
    0
}

/// Write the pretty-printed report as
/// `<report_dir>/<backend>-<split>-<timestamp>.json` and print its path.
/// `:` and `+` in the RFC 3339 timestamp are replaced with `-` so the name
/// is valid everywhere; the report's own `generated_at` keeps the exact
/// form. Errors are I/O errors of the directory or file.
fn write_report(
    report_dir: &Path,
    split: &str,
    backend: &str,
    report: &BenchmarkReport,
) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(report_dir)?;
    let timestamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .replace([':', '+'], "-");
    let path = report_dir.join(format!("{backend}-{split}-{timestamp}.json"));
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(&path, json)?;
    println!("report: {}", path.display());
    Ok(())
}

/// Production replay backend.
///
/// `Cloud` opens one `RealtimeSession` per sample from the loaded config;
/// `Local` builds one `LocalStreamingRecognizer` per sample from the
/// resolved model payload directory, so samples never share decoder state.
enum Backend {
    /// Boxed to keep the enum small; cloned once per replay.
    Cloud { config: Box<AppConfig> },
    /// Model directory from `ModelManager::resolve`.
    Local { payload: std::path::PathBuf },
}

impl BackendFactory for Backend {
    /// Blocking adapter over the async backends. The replay loop in
    /// `benchmark` is synchronous while `run_asr` runs on the tokio
    /// runtime, so the cloud path re-enters the runtime with
    /// `block_in_place` + `block_on`, and the local path moves the
    /// CPU-bound decode off the async worker the same way. Both need the
    /// multi-thread runtime (`#[tokio::main]` default); `block_in_place`
    /// panics on a current-thread runtime.
    fn replay(
        &mut self,
        _index: usize,
        pcm: &[u8],
        chunk_interval: Duration,
    ) -> Result<String, String> {
        match self {
            Backend::Cloud { config } => {
                let config = config.as_ref().clone();
                let pcm = pcm.to_vec();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(async move { replay_cloud(&config, &pcm, chunk_interval).await })
                })
            }
            Backend::Local { payload } => {
                let payload = payload.clone();
                let pcm = pcm.to_vec();
                tokio::task::block_in_place(move || replay_local(&payload, &pcm))
            }
        }
    }
}

/// Feed the whole buffer in 1024-byte (32 ms) chunks through a fresh
/// Sherpa-ONNX streaming recognizer, unpaced: the decoder is synchronous,
/// so sleeping between chunks would only add idle time. Non-empty
/// `Completed` segments are concatenated; a non-empty `Finished`
/// transcript from `finish` replaces that join. Any recognizer error
/// (including a trailing odd byte) fails the sample.
fn replay_local(payload: &std::path::Path, pcm: &[u8]) -> Result<String, String> {
    let mut recognizer = crate::local_asr::LocalStreamingRecognizer::new(payload)
        .map_err(|error| error.to_string())?;
    let mut confirmed: Vec<String> = Vec::new();
    for chunk in pcm.chunks(crate::capture::CHUNK_BYTES) {
        for event in recognizer
            .accept_pcm(chunk)
            .map_err(|error| error.to_string())?
        {
            if let RealtimeEvent::Completed { transcript } = event {
                if !transcript.is_empty() {
                    confirmed.push(transcript);
                }
            }
        }
    }
    let mut final_text = confirmed.join("");
    for event in recognizer.finish().map_err(|error| error.to_string())? {
        if let RealtimeEvent::Finished { transcript } = event {
            if !transcript.is_empty() {
                final_text = transcript;
            }
        }
    }
    Ok(final_text)
}

/// One DashScope realtime session for one sample: connect, wait for
/// `Ready`, send 32 ms chunks sleeping `chunk_interval` between them, drain
/// whatever events are already buffered after each chunk, then `finish`
/// and wait for `Finished`.
///
/// Result precedence: a non-empty `Finished` transcript wins; otherwise the
/// concatenation of the non-empty `Completed` segments. A transport error
/// after `finish` is tolerated when segments were already confirmed (the
/// joined text is returned) and fails the sample otherwise. Errors before
/// `finish` always fail the sample.
async fn replay_cloud(
    config: &AppConfig,
    pcm: &[u8],
    chunk_interval: Duration,
) -> Result<String, String> {
    let mut session = RealtimeSession::connect(
        &config.cloud.realtime_endpoint,
        &config.cloud.api_key,
        &config.cloud.realtime_model,
    )
    .await
    .map_err(|error| error.to_string())?;

    loop {
        match session.next_event().await {
            Ok(RealtimeEvent::Ready) => break,
            Ok(_) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }

    let mut confirmed: Vec<String> = Vec::new();
    for chunk in pcm.chunks(crate::capture::CHUNK_BYTES) {
        session
            .send_audio(chunk)
            .await
            .map_err(|error| error.to_string())?;
        tokio::time::sleep(chunk_interval).await;
        while let Ok(Some(event)) = next_available(&session).await {
            if let RealtimeEvent::Completed { transcript } = event {
                if !transcript.is_empty() {
                    confirmed.push(transcript);
                }
            }
        }
    }

    session.finish().await.map_err(|error| error.to_string())?;
    loop {
        match session.next_event().await {
            Ok(RealtimeEvent::Finished { transcript }) => {
                if !transcript.is_empty() {
                    return Ok(transcript);
                }
                return Ok(confirmed.join(""));
            }
            Ok(RealtimeEvent::Completed { transcript }) => {
                if !transcript.is_empty() {
                    confirmed.push(transcript);
                }
            }
            Ok(_) => continue,
            Err(error) => {
                if !confirmed.is_empty() {
                    return Ok(confirmed.join(""));
                }
                return Err(error.to_string());
            }
        }
    }
}

/// Drain already-buffered events without blocking on the network.
///
/// A 1 ms timeout polls the session's event channel; cancelling
/// `next_event` is safe because it only awaits a tokio mutex and an mpsc
/// receiver, so no event is lost. `Ok(None)` means nothing is queued right
/// now; a transport error is surfaced as `Err`.
async fn next_available(session: &RealtimeSession) -> Result<Option<RealtimeEvent>, String> {
    match tokio::time::timeout(Duration::from_millis(1), session.next_event()).await {
        Ok(Ok(event)) => Ok(Some(event)),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Ok(None),
    }
}
