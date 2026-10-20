mod common;

use common::{entries, new_log, FakeCapture, RecordingInjector, RecordingSink, ScriptedTransport};
use std::time::Duration;
use tokio::sync::mpsc;
use voiceless::coordinator::{run_session, ControlCommand, OutputEvent, SessionPlan};
use voiceless::realtime::RealtimeEvent;

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

async fn run_fixture(
    plan: SessionPlan,
    capture: FakeCapture,
    transport: ScriptedTransport,
    commands: Vec<ControlCommand>,
) -> (i32, Vec<OutputEvent>, Vec<String>) {
    let (control_tx, control_rx) = mpsc::channel(8);
    let log = transport.log.clone();
    let mut transport = transport;
    transport.control_tx = Some(control_tx.clone());
    let injector = RecordingInjector::new(log.clone());
    let mut sink = RecordingSink::default();
    for command in commands {
        control_tx.send(command).await.expect("send command");
    }
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        run_session(
            plan,
            capture,
            transport,
            common::CountingProcessor::noop(),
            injector,
            common::RecordingHistory::new(),
            control_rx,
            &mut sink,
        ),
    )
    .await
    .expect("run_session must terminate");
    (code, sink.events, entries(&log))
}

#[tokio::test]
async fn auth_failure_before_ready_never_starts_capture_or_injects() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "401 unauthorized",
    )));
    let capture = FakeCapture::new(log.clone(), vec![vec![0; 1024]], None);
    let plan = SessionPlan::new("cloud-realtime", true);

    let (code, events, log_entries) = run_fixture(plan, capture, transport, vec![]).await;

    assert_eq!(code, 1);
    assert_eq!(
        event_kinds(&events),
        vec!["error", "completed"],
        "{events:?}"
    );
    assert!(
        !log_entries.iter().any(|entry| entry == "capture.start"),
        "capture must not start on auth failure: {log_entries:?}"
    );
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("transport.append:")),
        "{log_entries:?}"
    );
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}

#[tokio::test]
async fn capture_start_failure_reports_error_without_injection() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    let mut capture = FakeCapture::new(log.clone(), vec![vec![0; 1024]], None);
    capture.start_error = Some("no default audio device".to_owned());
    let plan = SessionPlan::new("cloud-realtime", true);

    let (code, events, log_entries) = run_fixture(plan, capture, transport, vec![]).await;

    assert_eq!(code, 1);
    assert_eq!(
        event_kinds(&events),
        vec!["error", "completed"],
        "{events:?}"
    );
    let OutputEvent::Error(message) = &events[0] else {
        panic!("expected error event: {events:?}");
    };
    assert!(message.contains("no default audio device"), "{message}");
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}

#[tokio::test]
async fn second_stop_during_flush_cancels_without_finish_or_injection() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.before_finish(Ok(RealtimeEvent::Partial {
        text: "你好".to_owned(),
        stash: String::new(),
    }));
    // First stop comes from the test harness; the second stop arrives while
    // the tail frame is being flushed.
    transport.trigger(1, ControlCommand::Stop);
    transport.trigger(2, ControlCommand::Stop);
    let capture = FakeCapture::new(log.clone(), vec![vec![1; 1024]], Some(vec![2; 512]));
    let plan = SessionPlan::new("cloud-realtime", true);

    let (code, events, log_entries) = run_fixture(plan, capture, transport, vec![]).await;

    assert_eq!(code, 130);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"cancelled"), "{kinds:?}");
    assert_eq!(kinds.last().copied(), Some("completed"));
    assert!(!kinds.contains(&"final"), "{kinds:?}");
    assert!(!kinds.contains(&"finalized"), "{kinds:?}");
    assert!(
        !log_entries.iter().any(|entry| entry == "transport.finish"),
        "{log_entries:?}"
    );
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}

#[tokio::test]
async fn explicit_cancel_during_flush_cancels_without_injection() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.trigger(1, ControlCommand::Cancel);
    let capture = FakeCapture::new(log.clone(), vec![vec![1; 1024]], Some(vec![2; 512]));
    let plan = SessionPlan::new("cloud-realtime", true);

    let (code, events, log_entries) = run_fixture(plan, capture, transport, vec![]).await;

    assert_eq!(code, 130);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"cancelled"), "{kinds:?}");
    assert!(!kinds.contains(&"final"), "{kinds:?}");
    assert!(
        !log_entries.iter().any(|entry| entry == "transport.finish"),
        "{log_entries:?}"
    );
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}

#[tokio::test]
async fn disconnect_after_partial_fails_without_injecting_partial_text() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.before_finish(Ok(RealtimeEvent::Partial {
        text: "半句".to_owned(),
        stash: String::new(),
    }));
    transport.before_finish(Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "websocket closed mid-session",
    )));
    let capture = FakeCapture::new(log.clone(), vec![vec![0; 1024]], None);
    let plan = SessionPlan::new("cloud-realtime", true);

    let (code, events, log_entries) = run_fixture(plan, capture, transport, vec![]).await;

    assert_eq!(code, 1);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"transcript"), "{kinds:?}");
    assert!(kinds.contains(&"error"), "{kinds:?}");
    assert_eq!(kinds.last().copied(), Some("completed"));
    assert!(!kinds.contains(&"final"), "{kinds:?}");
    assert!(!kinds.contains(&"finalized"), "{kinds:?}");
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "partial text must never be injected: {log_entries:?}"
    );
}

#[tokio::test]
async fn finish_timeout_fails_the_session_without_injection() {
    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.trigger(1, ControlCommand::Stop);
    transport.hang_on_finish = true;
    let capture = FakeCapture::new(log.clone(), vec![vec![1; 1024]], None);
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.finish_timeout = Duration::from_millis(80);

    let (code, events, log_entries) = run_fixture(plan, capture, transport, vec![]).await;

    assert_eq!(code, 1);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"error"), "{kinds:?}");
    assert_eq!(kinds.last().copied(), Some("completed"));
    let OutputEvent::Error(message) = events
        .iter()
        .find(|event| matches!(event, OutputEvent::Error(_)))
        .expect("error event")
    else {
        unreachable!()
    };
    assert!(message.contains("timed out"), "{message}");
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}
