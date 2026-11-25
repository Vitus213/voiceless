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
use voiceless::coordinator::{
    run_session, ControlCommand, OutputEvent, SessionPhase, SessionPlan, TextProcessor,
};
use voiceless::realtime::RealtimeEvent;

/// Flatten events to their kind names; a final snapshot counts as `final`.
fn event_kinds(events: &[OutputEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            OutputEvent::Ready => "ready",
            OutputEvent::BackendSelected(_) => "backend-selected",
            OutputEvent::DevicesLoaded(_) => "devices-loaded",
            OutputEvent::Level(_) => "level",
            OutputEvent::Transcript(snapshot) if snapshot.is_final => "final",
            OutputEvent::Transcript(_) => "transcript",
            OutputEvent::Phase(_) => "phase",
            OutputEvent::Output(_) => "output",
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
async fn run_success_session<P: TextProcessor + Clone>(
    plan: SessionPlan,
    processor: P,
    history: RecordingHistory,
) -> (i32, Vec<OutputEvent>, Vec<String>, P, RecordingHistory) {
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

#[derive(Clone)]
struct RejectProcessing;

impl TextProcessor for RejectProcessing {
    fn cleanup(&mut self, _text: &str) -> String {
        panic!("raw dictation must skip text cleanup");
    }

    async fn process(&mut self, _mode: &str, _text: &str) -> Result<String, String> {
        panic!("raw dictation must skip remote processing");
    }
}

#[tokio::test]
async fn raw_mode_delivers_and_stores_the_authoritative_text() {
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.mode_id = "raw".to_owned();
    let (code, _, log, _, history) =
        run_success_session(plan, RejectProcessing, RecordingHistory::new()).await;

    assert_eq!(code, 0);
    assert_eq!(
        log.iter()
            .filter(|line| line.starts_with("injector.inject:"))
            .collect::<Vec<_>>(),
        ["injector.inject:原始识别文本"]
    );
    let records = history.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].final_text, records[0].raw_text);
    assert_eq!(records[0].processed_text, None);
    assert_eq!(records[0].processing_mode, "raw");
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
    assert!(sink2
        .events
        .iter()
        .any(|event| matches!(event, OutputEvent::Phase(SessionPhase::NoSpeech))));
    assert!(!entries(&log2)
        .iter()
        .any(|entry| entry.starts_with("injector.inject:")));
    assert!(
        history2_probe.records().is_empty(),
        "{:?}",
        history2_probe.records()
    );
}

#[tokio::test]
async fn cancellation_during_processing_releases_capture_and_skips_delivery() {
    struct PendingProcessor(Option<tokio::sync::oneshot::Sender<()>>);

    impl TextProcessor for PendingProcessor {
        async fn process(&mut self, _mode: &str, _text: &str) -> Result<String, String> {
            self.0.take().unwrap().send(()).unwrap();
            std::future::pending().await
        }
    }

    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.trigger(1, ControlCommand::Stop);
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: "原文".to_owned(),
    }));
    let capture = FakeCapture::new(log.clone(), vec![vec![1; 1024]], None);
    let (tx, rx) = mpsc::channel(8);
    transport.control_tx = Some(tx.clone());
    let (started, processing) = tokio::sync::oneshot::channel();
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.mode_id = "voice-polish".to_owned();
    let history = RecordingHistory::new();
    let mut sink = RecordingSink::default();
    let session = run_session(
        plan,
        capture,
        transport,
        PendingProcessor(Some(started)),
        RecordingInjector::new(log.clone()),
        history.clone(),
        rx,
        &mut sink,
    );
    let (code, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(session, async {
            processing.await.unwrap();
            tx.send(ControlCommand::Cancel).await.unwrap();
        })
    })
    .await
    .expect("cancel during processing must finish the session");
    assert_eq!(code, 130);
    assert!(history.records().is_empty());
    let effects = entries(&log);
    assert!(effects.iter().any(|entry| entry == "capture.abort"));
    assert!(!effects
        .iter()
        .any(|entry| entry.starts_with("injector.inject:")));
    assert!(sink
        .events
        .iter()
        .any(|event| matches!(event, OutputEvent::Cancelled)));
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
