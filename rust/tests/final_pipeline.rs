//! Final-text pipeline after a successful finish: `quick` mode skips the
//! processor, other modes process the text and inject the result, a
//! processing failure keeps the recognized text with a warning, history is
//! written only for successful non-empty sessions, and `inject = false`
//! still processes and records. Fakes: `CountingProcessor` (counts calls,
//! fixed result), `RecordingHistory`, plus the capture/transport fakes from
//! `common`.

mod common;

use common::{
    entries, new_log, CountingProcessor, FakeCapture, RecordingHistory, RecordingInjector,
    RecordingSink, ScriptedTransport,
};
use std::time::Duration;
use tokio::sync::mpsc;
use voiceless::coordinator::{run_session, ControlCommand, OutputEvent, SessionPlan};
use voiceless::realtime::RealtimeEvent;

/// Flatten events to their kind names; a final snapshot counts as `final`.
fn event_kinds(events: &[OutputEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            OutputEvent::Ready => "ready",
            OutputEvent::Level(_) => "level",
            OutputEvent::Transcript(snapshot) if snapshot.is_final => "final",
            OutputEvent::Transcript(_) => "transcript",
            OutputEvent::Finalized { .. } => "finalized",
            OutputEvent::Warning(_) => "warning",
            OutputEvent::Error(_) => "error",
            OutputEvent::Cancelled => "cancelled",
            OutputEvent::Completed => "completed",
        })
        .collect()
}

/// Fixture: ready -> one completed segment -> stop after first append ->
/// finished with the same text. Drives the success final pipeline.
async fn run_success_session(
    plan: SessionPlan,
    processor: CountingProcessor,
    history: RecordingHistory,
) -> (
    i32,
    Vec<OutputEvent>,
    Vec<String>,
    CountingProcessor,
    RecordingHistory,
) {
    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.before_finish(Ok(RealtimeEvent::Completed {
        transcript: "原始识别文本".to_owned(),
    }));
    transport.trigger(1, ControlCommand::Stop);
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: "原始识别文本".to_owned(),
    }));
    let capture = FakeCapture::new(log.clone(), vec![vec![0; 1024]], None);
    let (control_tx, control_rx) = mpsc::channel(8);
    transport.control_tx = Some(control_tx);
    let injector = RecordingInjector::new(log.clone());
    let mut sink = RecordingSink::default();
    let processor_probe = processor.clone();
    let history_probe = history.clone();
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        run_session(
            plan, capture, transport, processor, injector, history, control_rx, &mut sink,
        ),
    )
    .await
    .expect("run_session must terminate");
    (
        code,
        sink.events,
        entries(&log),
        processor_probe,
        history_probe,
    )
}

#[tokio::test]
async fn quick_mode_never_calls_the_processor_and_injects_raw_text() {
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.mode_id = "quick".to_owned();
    let processor = CountingProcessor::ok("不应出现".to_owned());
    let history = RecordingHistory::new();

    let (code, events, log_entries, processor, history) =
        run_success_session(plan, processor, history).await;

    assert_eq!(code, 0);
    assert_eq!(processor.calls(), 0, "quick mode must not process");
    let injection = log_entries
        .iter()
        .find(|entry| entry.starts_with("injector.inject:"))
        .expect("injection must happen");
    assert_eq!(injection, "injector.inject:原始识别文本");
    let records = history.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].raw_text, "原始识别文本");
    assert_eq!(records[0].final_text, "原始识别文本");
    assert_eq!(records[0].processing_mode, "quick");
    assert!(event_kinds(&events).contains(&"finalized"));
}

#[tokio::test]
async fn custom_mode_processes_final_text_and_injects_the_result() {
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.mode_id = "translate-en".to_owned();
    let processor = CountingProcessor::ok("translated text".to_owned());
    let history = RecordingHistory::new();

    let (code, _events, log_entries, processor, history) =
        run_success_session(plan, processor, history).await;

    assert_eq!(code, 0);
    assert_eq!(processor.calls(), 1);
    assert_eq!(processor.last_input(), Some("原始识别文本".to_owned()));
    let injection = log_entries
        .iter()
        .find(|entry| entry.starts_with("injector.inject:"))
        .expect("injection must happen");
    assert_eq!(injection, "injector.inject:translated text");
    let records = history.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].processed_text.as_deref(),
        Some("translated text")
    );
    assert_eq!(records[0].final_text, "translated text");
    assert_eq!(records[0].processing_mode, "translate-en");
}

#[tokio::test]
async fn processing_failure_keeps_recognized_text_with_warning() {
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.mode_id = "voice-polish".to_owned();
    let processor = CountingProcessor::failing("上游服务超时".to_owned());
    let history = RecordingHistory::new();

    let (code, events, log_entries, processor, history) =
        run_success_session(plan, processor, history).await;

    assert_eq!(code, 0);
    assert_eq!(processor.calls(), 1);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"warning"), "{kinds:?}");
    let warning = events.iter().find_map(|event| match event {
        OutputEvent::Warning(message) => Some(message.clone()),
        _ => None,
    });
    assert!(
        warning.unwrap().contains("上游服务超时"),
        "warning must carry the processing failure reason"
    );
    let injection = log_entries
        .iter()
        .find(|entry| entry.starts_with("injector.inject:"))
        .expect("injection must still happen");
    assert_eq!(injection, "injector.inject:原始识别文本");
    let records = history.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].final_text, "原始识别文本");
    assert_eq!(records[0].status, "completed");
}

/// Two sessions: one cancelled after the first append, one finished with
/// an empty transcript; neither reaches the history recorder.
#[tokio::test]
async fn cancelled_and_empty_sessions_write_no_history() {
    // Cancelled session.
    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.trigger(1, ControlCommand::Cancel);
    let capture = FakeCapture::new(log.clone(), vec![vec![0; 1024]], None);
    let (control_tx, control_rx) = mpsc::channel(8);
    transport.control_tx = Some(control_tx);
    let injector = RecordingInjector::new(log.clone());
    let mut sink = RecordingSink::default();
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.mode_id = "quick".to_owned();
    let history = RecordingHistory::new();
    let history_probe = history.clone();
    let code = run_session(
        plan,
        capture,
        transport,
        CountingProcessor::noop(),
        injector,
        history,
        control_rx,
        &mut sink,
    )
    .await;
    assert_eq!(code, 130);
    assert!(
        history_probe.records().is_empty(),
        "{:?}",
        history_probe.records()
    );

    // Empty speech session: history must stay empty.
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.mode_id = "quick".to_owned();
    let log2 = new_log();
    let mut transport2 = ScriptedTransport::new(log2.clone());
    transport2.before_finish(Ok(RealtimeEvent::Ready));
    transport2.trigger(1, ControlCommand::Stop);
    transport2.after_finish(Ok(RealtimeEvent::Finished {
        transcript: String::new(),
    }));
    let capture2 = FakeCapture::new(log2.clone(), vec![vec![0; 1024]], None);
    let (tx2, rx2) = mpsc::channel(8);
    transport2.control_tx = Some(tx2);
    let history2 = RecordingHistory::new();
    let history2_probe = history2.clone();
    let mut sink2 = RecordingSink::default();
    let code2 = run_session(
        plan,
        capture2,
        transport2,
        CountingProcessor::noop(),
        RecordingInjector::new(log2.clone()),
        history2,
        rx2,
        &mut sink2,
    )
    .await;
    assert_eq!(code2, 0);
    assert!(
        history2_probe.records().is_empty(),
        "{:?}",
        history2_probe.records()
    );
}

#[tokio::test]
async fn injection_disabled_still_processes_and_records_history() {
    let mut plan = SessionPlan::new("cloud-realtime", false);
    plan.mode_id = "translate-en".to_owned();
    let processor = CountingProcessor::ok("translated".to_owned());
    let history = RecordingHistory::new();

    let (code, events, log_entries, processor, history) =
        run_success_session(plan, processor, history).await;

    assert_eq!(code, 0);
    assert_eq!(processor.calls(), 1);
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "no injection requested: {log_entries:?}"
    );
    assert_eq!(history.records().len(), 1);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"finalized"), "{kinds:?}");
}
