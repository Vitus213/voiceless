//! Quality benchmark: replay versioned corpus audio through a realtime
//! backend, measure content CER per sample and aggregate a versioned JSON
//! report. Dev and test splits stay disjoint; cloud and local reports are
//! never merged.
//!
//! This module is pure computation: no network, no audio devices, no
//! configuration. It defines the corpus format, the normalization and the
//! CER metric, the report schema and the replay loop. `benchmark_cmd` (the
//! `voiceless benchmark asr` command) supplies the real backends, the file
//! paths under `scripts/asr_benchmark/` and the exit-code policy;
//! `latency_cmd` reuses `load_corpus`. The metric mirrors the "content"
//! mode of `scripts/asr_benchmark/normalize.py`, so Rust reports stay
//! comparable with the frozen Python baselines in
//! `scripts/asr_benchmark/README.md`. `tests/benchmark_contract.rs` pins
//! the normalization, the CER, the loader and the report shape.
//!
//! Corpus files are JSONL (`corpus/<split>.jsonl`), one `CorpusEntry` per
//! line, with the audio beside them as `audio/<id>.wav`. `corpus_version`
//! hashes ids and references so a report can be tied to the exact corpus
//! it was measured on.
//!
//! Failure behaviour: a backend error on one sample is recorded on that
//! sample (`error` set, CER NaN) and counted in `failures`; the run
//! continues and the aggregate CER is computed over the scored samples
//! only. The caller decides whether failures block the gate.

use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One corpus line: `{"id", "reference", "voice"}`.
///
/// `reference` is the text that was sent to TTS to synthesize
/// `audio/<id>.wav` (`scripts/asr_benchmark/generate.py`), so it is also
/// the ground truth the transcript is scored against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusEntry {
    /// Stable sample id; also the basename of the WAV file.
    pub id: String,
    /// Ground-truth text (TTS input).
    pub reference: String,
    /// TTS voice name; carried for reporting only, never used in scoring.
    pub voice: String,
}

/// Per-sample outcome inside a `BenchmarkReport`.
#[derive(Debug, Clone, Serialize)]
pub struct SampleResult {
    pub id: String,
    pub reference: String,
    /// Final transcript returned by the backend; empty when the replay
    /// failed.
    pub transcript: String,
    /// Content CER of `transcript` against `reference`. NaN (serialized as
    /// JSON `null`) when the replay failed, so failed samples never lower
    /// the aggregate.
    pub cer: f64,
    /// Wall-clock seconds spent in `BackendFactory::replay` for this sample
    /// (connection, paced audio and the final wait included). Informational;
    /// the latency gate is `latency_cmd`, not this number.
    pub latency_seconds: f64,
    /// Backend error message when the replay failed.
    pub error: Option<String>,
}

/// Versioned report written by `benchmark asr` as pretty JSON
/// (`reports/<backend>-<split>-<timestamp>.json`). The field names are the
/// contract checked by `tests/benchmark_contract.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkReport {
    /// Backend the audio was replayed through (`cloud-realtime` or
    /// `local-streaming`); reports of different backends are never merged.
    pub backend: String,
    /// Model identifier of that backend (cloud realtime model name or local
    /// catalog id).
    pub model: String,
    /// `corpus_version` of the scored entries; two reports with different
    /// versions are not comparable.
    pub corpus_version: String,
    /// Corpus split name (`dev`, `test`), or `unspecified` from
    /// `run_asr_benchmark`.
    pub split: String,
    /// One entry per corpus line, in corpus order, failed samples included.
    pub samples: Vec<SampleResult>,
    /// Number of samples whose replay returned an error.
    pub failures: usize,
    /// Arithmetic mean of the per-sample CER over the samples that scored
    /// (a macro average, not total edits over total reference length);
    /// 0.0 when nothing scored.
    pub content_cer: f64,
    /// UTC RFC 3339 timestamp of the aggregation.
    pub generated_at: String,
}

/// Backend boundary for benchmark replay: feed one sample's PCM and return
/// the final transcript.
///
/// Implementations own whatever session or recognizer they need per call
/// and must block until the transcript is final: the replay loop times the
/// whole call as the sample latency. Tests use scripted fakes;
/// `benchmark_cmd` provides the cloud and local implementations.
pub trait BackendFactory {
    /// Replay one sample and return its final transcript.
    ///
    /// `index` is the sample's position in the corpus (fakes use it to
    /// script per-sample outcomes); `pcm` is 16 kHz mono PCM16
    /// little-endian bytes and may be empty when no audio was supplied;
    /// `chunk_interval` is the pace at which a streaming backend should
    /// feed 32 ms chunks (`Duration::ZERO` in tests). The error string is
    /// stored verbatim on the sample.
    fn replay(
        &mut self,
        index: usize,
        pcm: &[u8],
        chunk_interval: Duration,
    ) -> Result<String, String>;
}

/// Content normalization: NFKC, lowercase, strip all punctuation and
/// whitespace while keeping CJK characters and digits.
///
/// Keeps exactly the characters for which `char::is_alphanumeric` holds,
/// so full-width letters fold to ASCII (NFKC) and `你好，世界！` compares
/// equal to `你好世界`. This is the "content" mode of `normalize.py`; it is
/// what makes the CER punctuation- and spacing-agnostic.
pub fn normalize_content(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    text.nfkc()
        .flat_map(|ch| ch.to_lowercase())
        .filter(|ch| ch.is_alphanumeric())
        .collect()
}

/// Character error rate: edit distance over reference length, computed on
/// normalized text. An empty reference has nothing to miss (0.0).
///
/// Both strings go through `normalize_content` first, and the length is
/// counted in `char`s after normalization. The value can exceed 1.0 when
/// the hypothesis is much longer than the reference.
pub fn cer(reference: &str, hypothesis: &str) -> f64 {
    let reference = normalize_content(reference);
    let hypothesis = normalize_content(hypothesis);
    if reference.is_empty() {
        return 0.0;
    }
    let distance = edit_distance(&reference, &hypothesis);
    distance as f64 / reference.chars().count() as f64
}

/// Levenshtein distance over `char`s with a two-row DP table (unit cost for
/// insert, delete and substitute).
fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    for (i, char_a) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, char_b) in b_chars.iter().enumerate() {
            let cost = if char_a == *char_b { 0 } else { 1 };
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + cost),
            );
        }
        previous = current;
    }
    previous[b_chars.len()]
}

/// Read a JSONL corpus file into entries, in file order.
///
/// Blank lines are skipped. Any malformed line (or one missing a field)
/// fails the whole load with `io::ErrorKind::InvalidData` and a
/// `path:line:` prefix, so a corrupt corpus can never be partially scored.
/// An unreadable file returns the underlying `io::Error`.
pub fn load_corpus(path: &Path) -> Result<Vec<CorpusEntry>, io::Error> {
    let raw = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for (line_number, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: CorpusEntry = serde_json::from_str(trimmed).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}:{}: {error}", path.display(), line_number + 1),
            )
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Corpus version: content hash over entry ids and references so report
/// comparisons can detect corpus drift.
///
/// Format: `corpus-` followed by the first 8 bytes (16 hex digits) of a
/// SHA-256 over `(id, reference)` pairs in corpus order, separated by the
/// ASCII unit and record separators. `voice` is excluded on purpose:
/// re-synthesizing with another voice does not change the ground truth.
pub fn corpus_version(entries: &[CorpusEntry]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.id.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(entry.reference.as_bytes());
        hasher.update(b"\x1e");
    }
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("corpus-{hex}")
}

/// Replay a corpus with the split recorded as `unspecified`; see
/// `run_asr_benchmark_split` for the semantics.
#[allow(clippy::too_many_arguments)]
pub fn run_asr_benchmark<F: BackendFactory>(
    entries: &[CorpusEntry],
    pcm_by_id: &[(String, Vec<u8>)],
    factory: &mut F,
    backend: &str,
    model: &str,
    corpus_version: &str,
    chunk_interval: Duration,
) -> BenchmarkReport {
    run_asr_benchmark_split(
        entries,
        pcm_by_id,
        factory,
        backend,
        model,
        corpus_version,
        "unspecified",
        chunk_interval,
    )
}

/// Replay every corpus entry through `factory` and aggregate the report.
///
/// `pcm_by_id` maps entry ids to PCM16 bytes; an entry without audio is
/// replayed with an empty buffer rather than skipped (the command layer
/// refuses to run with missing audio, so this only matters for callers
/// that bypass it). Samples are replayed sequentially in corpus order and
/// each call is timed as `SampleResult::latency_seconds`. Backend errors
/// are recorded per sample and the loop continues. Synchronous: it blocks
/// the calling thread for the whole corpus, which is why the production
/// backends in `benchmark_cmd` use `block_in_place` internally.
#[allow(clippy::too_many_arguments)]
pub fn run_asr_benchmark_split<F: BackendFactory>(
    entries: &[CorpusEntry],
    pcm_by_id: &[(String, Vec<u8>)],
    factory: &mut F,
    backend: &str,
    model: &str,
    corpus_version: &str,
    split: &str,
    chunk_interval: Duration,
) -> BenchmarkReport {
    let mut samples = Vec::new();
    let mut failures = 0;
    for (index, entry) in entries.iter().enumerate() {
        // Linear lookup is fine for a few dozen samples; a missing id
        // becomes an empty buffer so the report still lists the sample.
        let pcm = pcm_by_id
            .iter()
            .find(|(id, _)| id == &entry.id)
            .map(|(_, pcm)| pcm.clone())
            .unwrap_or_default();
        let started = std::time::Instant::now();
        let outcome = factory.replay(index, &pcm, chunk_interval);
        let latency = started.elapsed().as_secs_f64();
        match outcome {
            Ok(transcript) => {
                let sample_cer = cer(&entry.reference, &transcript);
                samples.push(SampleResult {
                    id: entry.id.clone(),
                    reference: entry.reference.clone(),
                    transcript,
                    cer: sample_cer,
                    latency_seconds: latency,
                    error: None,
                });
            }
            Err(message) => {
                failures += 1;
                samples.push(SampleResult {
                    id: entry.id.clone(),
                    reference: entry.reference.clone(),
                    transcript: String::new(),
                    cer: f64::NAN,
                    latency_seconds: latency,
                    error: Some(message),
                });
            }
        }
    }
    // Failed samples carry NaN; excluding them keeps the aggregate honest
    // and leaves `failures` to tell the caller the run is incomplete.
    let scored: Vec<f64> = samples
        .iter()
        .map(|sample| sample.cer)
        .filter(|value| value.is_finite())
        .collect();
    let content_cer = if scored.is_empty() {
        0.0
    } else {
        scored.iter().sum::<f64>() / scored.len() as f64
    };
    BenchmarkReport {
        backend: backend.to_owned(),
        model: model.to_owned(),
        corpus_version: corpus_version.to_owned(),
        split: split.to_owned(),
        samples,
        failures,
        content_cer,
        generated_at: now_rfc3339(),
    }
}

/// Current UTC time as RFC 3339; empty string if formatting fails.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
