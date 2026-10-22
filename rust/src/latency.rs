//! End-to-end latency gate: real cloud + Wayland injection trials with
//! per-stage timestamps, p50/p95/p99 aggregation and hard thresholds.
//! Without real credentials, audio or an injection target the gate is
//! marked unverified, never passed.
//!
//! This module is pure aggregation. `latency_cmd` (the `voiceless
//! benchmark latency` command) runs the trials and records one
//! `TrialOutcome` each; `evaluate_gate` turns them into a `LatencyGate`
//! that is serialized verbatim as the report
//! (`scripts/asr_benchmark/reports/latency-<backend>-<timestamp>.json`)
//! and decides pass / fail / unverified.
//! `tests/latency_benchmark_contract.rs` pins the percentile math and the
//! gate rules.
//!
//! Stage timestamps are seconds since the trial started. The three
//! measured intervals are utterance start -> first partial (p95),
//! stop -> final transcript received (p50) and stop -> injection complete
//! (p99). Only trials in the `quick` processing mode count: LLM-backed
//! modes add processing time that the 1 s budget was never meant to cover.

use serde::Serialize;

/// Checkpoints recorded during one trial, in the order a trial reaches
/// them. Each is stored at most once with its elapsed time; a trial that
/// fails early simply lacks the later stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TrialStage {
    /// Trial clock zero; always present with value 0.0.
    Start,
    /// Realtime session connected and the server sent `Ready`.
    ConnectReady,
    /// First voiced 32 ms chunk sent; leading silence in the sample is
    /// excluded from the first-partial interval.
    UtteranceStart,
    /// First `Partial` event received.
    FirstPartial,
    /// Last audio chunk sent and paced: the "hotkey released" moment the
    /// stop-based intervals are measured from.
    Stop,
    /// Reserved for the tail-frame flush of a live session. `latency_cmd`
    /// never records it (replay has no tail frame), so it only appears in
    /// hand-built fixtures.
    TailSent,
    /// `finish` acknowledged by the session.
    FinishSent,
    /// `Finished` event (final transcript) received.
    FinalReceived,
    /// `inject_text` returned; only recorded when injection was requested.
    InjectionComplete,
}

/// Result of one trial as recorded by `latency_cmd::run_trial`.
#[derive(Debug, Clone, Serialize)]
pub struct TrialOutcome {
    /// Zero-based trial index.
    pub id: usize,
    /// Processing mode of the trial; only `quick` feeds the gate.
    pub mode: String,
    /// `(stage, seconds since Start)` pairs in recording order.
    pub stages: Vec<(TrialStage, f64)>,
    /// False when any step errored or no text was produced; a failed trial
    /// keeps the stages it reached.
    pub success: bool,
    /// Failure reason when `success` is false.
    pub error: Option<String>,
}

/// Budgets the gate enforces. `Default` is the project SLO from the README
/// and `docs/low-latency-rust-plan.md`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LatencyThresholds {
    /// Utterance start -> first partial, p95 (default 1.0 s).
    pub first_partial_p95_seconds: f64,
    /// Stop -> final transcript received, p50 (default 0.6 s).
    pub stop_to_final_p50_seconds: f64,
    /// Stop -> injection complete, p99 (default 1.0 s).
    pub stop_to_inject_p99_seconds: f64,
    /// Successful `quick` trials required before the gate may pass
    /// (default 100); fewer leave it `unverified`.
    pub min_trials: usize,
}

impl Default for LatencyThresholds {
    /// The documented SLO: first partial p95 1.0 s, stop->final p50 0.6 s,
    /// stop->inject p99 1.0 s, over at least 100 trials.
    fn default() -> Self {
        Self {
            first_partial_p95_seconds: 1.0,
            stop_to_final_p50_seconds: 0.6,
            stop_to_inject_p99_seconds: 1.0,
            min_trials: 100,
        }
    }
}

/// Gate verdict plus everything needed to audit it; serialized as the
/// latency report.
#[derive(Debug, Clone, Serialize)]
pub struct LatencyGate {
    /// `quick` trials considered (successful + failed); non-quick trials
    /// are not included.
    pub trials: usize,
    /// `quick` trials with `success == false`; any one blocks `passed`.
    pub failed_trials: usize,
    /// Trials dropped because their mode was not `quick`.
    pub excluded_non_quick: usize,
    /// Fewer than `thresholds.min_trials` successful quick trials: the
    /// numbers are informational and `passed` is false.
    pub unverified: bool,
    /// True only when verified, no trial failed and every measured
    /// percentile is within its budget.
    pub passed: bool,
    /// p95 of utterance start -> first partial over the trials that
    /// recorded both stages; `None` when none did.
    pub first_partial_p95: Option<f64>,
    /// p50 of stop -> final received; `None` when no trial recorded both.
    pub stop_to_final_p50: Option<f64>,
    /// p99 of stop -> injection complete. `None` when no trial recorded
    /// `InjectionComplete` (for example `--inject` was not given); a
    /// missing percentile does not block the gate, the `null` in the
    /// report is the only trace that it was not measured.
    pub stop_to_inject_p99: Option<f64>,
    /// Budgets the verdict was computed against.
    pub thresholds: LatencyThresholds,
    /// Every outcome passed in, failed and non-quick trials included, so
    /// the report is self-contained.
    pub samples: Vec<TrialOutcome>,
}

/// Linear-interpolation percentile over a copy of the samples. Returns NaN
/// for empty input.
///
/// `p` is in `0..=100`. The rank is `p / 100 * (n - 1)` and the result is
/// interpolated between the two neighbouring sorted samples (NumPy's
/// default `linear` method), so p50 of `1..=100` is 50.5 and p100 is the
/// maximum. NaN samples are not filtered; they compare as equal to their
/// neighbours during the sort.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = (lower + 1).min(sorted.len() - 1);
    let fraction = rank - lower as f64;
    sorted[lower] + fraction * (sorted[upper] - sorted[lower])
}

/// Aggregate trial outcomes into the latency gate. Only `quick`-mode
/// successful trials feed the percentile thresholds; failed trials and
/// insufficient sample counts keep the gate unpassed.
///
/// Rules, in order: a non-`quick` trial is excluded entirely; a failed
/// quick trial counts toward `trials` and blocks `passed`; each percentile
/// is computed over the successful trials that recorded both of its
/// stages, so a trial contributes to some intervals and not others; an
/// interval no trial recorded is `None` and treated as within budget;
/// `unverified` is set when successful quick trials are fewer than
/// `min_trials`. `passed` = verified, no failures, all present percentiles
/// within budget. Pure and never panics, including on empty input.
pub fn evaluate_gate(outcomes: &[TrialOutcome], thresholds: &LatencyThresholds) -> LatencyGate {
    let mut quick_success: Vec<&TrialOutcome> = Vec::new();
    let mut failed_trials = 0;
    let mut excluded_non_quick = 0;
    for outcome in outcomes {
        if outcome.mode != "quick" {
            excluded_non_quick += 1;
            continue;
        }
        if outcome.success {
            quick_success.push(outcome);
        } else {
            failed_trials += 1;
        }
    }

    let first_partials: Vec<f64> = quick_success
        .iter()
        .filter_map(|outcome| {
            stage_duration(
                outcome,
                TrialStage::UtteranceStart,
                TrialStage::FirstPartial,
            )
        })
        .collect();
    let stop_to_finals: Vec<f64> = quick_success
        .iter()
        .filter_map(|outcome| stage_duration(outcome, TrialStage::Stop, TrialStage::FinalReceived))
        .collect();
    let stop_to_injects: Vec<f64> = quick_success
        .iter()
        .filter_map(|outcome| {
            stage_duration(outcome, TrialStage::Stop, TrialStage::InjectionComplete)
        })
        .collect();

    let first_partial_p95 = optional_percentile(&first_partials, 95.0);
    let stop_to_final_p50 = optional_percentile(&stop_to_finals, 50.0);
    let stop_to_inject_p99 = optional_percentile(&stop_to_injects, 99.0);

    let trials = quick_success.len() + failed_trials;
    let unverified = quick_success.len() < thresholds.min_trials;
    let within_budgets = [
        (first_partial_p95, thresholds.first_partial_p95_seconds),
        (stop_to_final_p50, thresholds.stop_to_final_p50_seconds),
        (stop_to_inject_p99, thresholds.stop_to_inject_p99_seconds),
    ]
    .iter()
    .all(|(value, budget)| match value {
        Some(value) => *value <= *budget,
        // An interval nobody measured cannot fail its budget; the report
        // shows it as null so a reader can tell it was not verified.
        None => true,
    });
    let passed = !unverified && failed_trials == 0 && within_budgets;

    LatencyGate {
        trials,
        failed_trials,
        excluded_non_quick,
        unverified,
        passed,
        first_partial_p95,
        stop_to_final_p50,
        stop_to_inject_p99,
        thresholds: *thresholds,
        samples: outcomes.to_vec(),
    }
}

/// Seconds between two recorded stages of one trial (first occurrence of
/// each); `None` when either stage is missing.
fn stage_duration(outcome: &TrialOutcome, from: TrialStage, to: TrialStage) -> Option<f64> {
    let start = outcome.stages.iter().find(|(stage, _)| *stage == from)?.1;
    let end = outcome.stages.iter().find(|(stage, _)| *stage == to)?.1;
    Some(end - start)
}

/// `percentile` that reports an empty sample set as `None` instead of NaN,
/// so the report can serialize it as `null`.
fn optional_percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(percentile(values, p))
    }
}
