//! Contract of the `latency` module behind `benchmark latency`: percentile
//! interpolation, the three stage intervals, the quick-mode-only rule, the
//! "any failed trial blocks" rule, the 100-trial minimum (unverified,
//! never passed) and the serialized report field names. No real sessions:
//! every outcome is built by the `trial` fixture.

use voiceless::latency::{evaluate_gate, percentile, LatencyThresholds, TrialOutcome, TrialStage};

/// Successful `quick` trial with a full stage list derived from three
/// target intervals: utterance start (0.05) -> first partial at `partial`,
/// stop at `partial + 1.0`, final `stop_to_final` after stop, injection
/// `stop_to_inject` after stop. `TailSent` is included although the real
/// command never records it.
fn trial(partial: f64, stop_to_final: f64, stop_to_inject: f64) -> TrialOutcome {
    TrialOutcome {
        id: 0,
        mode: "quick".to_owned(),
        stages: vec![
            (TrialStage::Start, 0.0),
            (TrialStage::UtteranceStart, 0.05),
            (TrialStage::FirstPartial, partial),
            (TrialStage::Stop, partial + 1.0),
            (TrialStage::FinishSent, partial + 1.2),
            (TrialStage::FinalReceived, partial + 1.0 + stop_to_final),
            (
                TrialStage::InjectionComplete,
                partial + 1.0 + stop_to_inject,
            ),
        ],
        success: true,
        error: None,
    }
}

#[test]
fn percentile_interpolates_sorted_samples() {
    let values: Vec<f64> = (1..=100).map(|index| index as f64).collect();
    assert_eq!(percentile(&values, 50.0), 50.5);
    assert_eq!(percentile(&values, 95.0), 95.05);
    assert_eq!(percentile(&values, 99.0), 99.01);
    assert_eq!(
        percentile(&[3.0, 1.0, 2.0], 50.0),
        2.0,
        "unsorted input is sorted"
    );
    assert!(percentile(&[], 50.0).is_nan(), "empty input is undefined");
}

/// 100 successful quick trials with every interval inside its budget:
/// all three percentiles are present, nothing failed, the gate passes.
#[test]
fn gate_reports_p50_p95_p99_per_stage_group() {
    let outcomes: Vec<TrialOutcome> = (0..100)
        .map(|index| {
            let i = index as f64;
            trial(0.3 + i / 1000.0, 0.4 + i / 1000.0, 0.7 + i / 1000.0)
        })
        .collect();

    let gate = evaluate_gate(&outcomes, &LatencyThresholds::default());
    assert_eq!(gate.trials, 100);
    assert_eq!(gate.failed_trials, 0);
    assert!(gate.first_partial_p95.is_some());
    assert!(gate.stop_to_final_p50.is_some());
    assert!(gate.stop_to_inject_p99.is_some());
    assert!(gate.passed, "{gate:?}");
}

#[test]
fn gate_fails_when_any_threshold_is_exceeded() {
    let outcomes: Vec<TrialOutcome> = (0..100)
        .map(|index| {
            let i = index as f64;
            // stop-to-inject p99 lands above the 1.0s budget for the tail
            trial(0.3, 0.4, 0.7 + i / 200.0)
        })
        .collect();
    let gate = evaluate_gate(&outcomes, &LatencyThresholds::default());
    assert!(!gate.passed);
    assert!(gate.stop_to_inject_p99.unwrap() > 1.0);
}

#[test]
fn non_quick_modes_are_never_counted_toward_the_quick_gate() {
    let mut outcomes: Vec<TrialOutcome> = (0..100).map(|_index| trial(0.3, 0.4, 0.7)).collect();
    for outcome in outcomes.iter_mut().take(50) {
        outcome.mode = "translate-en".to_owned();
    }
    let gate = evaluate_gate(&outcomes, &LatencyThresholds::default());
    assert_eq!(
        gate.trials, 50,
        "only quick-mode trials may feed the 1s gate"
    );
    assert_eq!(gate.excluded_non_quick, 50);
}

#[test]
fn failed_trials_are_reported_and_never_pass_the_gate() {
    let mut outcomes: Vec<TrialOutcome> = (0..100).map(|_| trial(0.3, 0.4, 0.7)).collect();
    outcomes[0].success = false;
    outcomes[0].error = Some("cloud error".to_owned());
    let gate = evaluate_gate(&outcomes, &LatencyThresholds::default());
    assert_eq!(gate.failed_trials, 1);
    assert!(!gate.passed, "any failed trial must block the gate");
}

#[test]
fn insufficient_samples_mark_the_gate_unverified_not_passed() {
    let outcomes: Vec<TrialOutcome> = (0..99).map(|_| trial(0.3, 0.4, 0.7)).collect();
    let gate = evaluate_gate(&outcomes, &LatencyThresholds::default());
    assert!(!gate.passed);
    assert!(gate.unverified, "99 trials is below the 100 minimum");
    assert_eq!(gate.trials, 99);
}

#[test]
fn empty_environment_is_unverified_not_passed() {
    let gate = evaluate_gate(&[], &LatencyThresholds::default());
    assert!(!gate.passed);
    assert!(gate.unverified);
    assert!(gate.first_partial_p95.is_none());
}

/// The JSON report keeps the top-level verdict fields (counts, flags,
/// thresholds); a single trial is enough since only the shape is checked.
#[test]
fn gate_serializes_with_stage_timestamps_and_percentiles() {
    let outcomes = vec![trial(0.3, 0.4, 0.7)];
    let gate = evaluate_gate(&outcomes, &LatencyThresholds::default());
    let value = serde_json::to_value(&gate).expect("serialize");
    for field in [
        "trials",
        "failed_trials",
        "unverified",
        "passed",
        "thresholds",
    ] {
        assert!(value.get(field).is_some(), "missing field {field}");
    }
}

/// Trials that never recorded `InjectionComplete` (a run without
/// `--inject`) leave the stop-to-inject percentile unmeasured: the gate is
/// unverified and not passed, however good the other intervals are.
#[test]
fn unmeasured_injection_marks_the_gate_unverified_not_passed() {
    let outcomes: Vec<TrialOutcome> = (0..100)
        .map(|_| {
            let mut outcome = trial(0.3, 0.4, 0.7);
            outcome
                .stages
                .retain(|(stage, _)| *stage != TrialStage::InjectionComplete);
            outcome
        })
        .collect();
    let gate = evaluate_gate(&outcomes, &LatencyThresholds::default());
    assert!(gate.stop_to_inject_p99.is_none());
    assert!(gate.unverified, "{gate:?}");
    assert!(!gate.passed);
}
