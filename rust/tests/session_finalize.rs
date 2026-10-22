//! `session::RecognitionSession` in isolation: partials and confirmed
//! segments accumulate, the first stop finishes and the second cancels,
//! `Finished` while stopping exposes the injection text exactly once, and
//! error or cancel paths never expose it. No fakes: the state machine is
//! pure data.

use voiceless::realtime::RealtimeEvent;
use voiceless::session::{RecognitionSession, SessionAction, SessionState, SessionUpdate};

/// Partial (text + stash) then a confirmed segment; stop -> Finish, a
/// second stop -> Cancel.
#[test]
fn normal_stop_flushes_one_authoritative_transcript_and_one_injection() {
    let mut session = RecognitionSession::new("cloud-realtime");

    assert!(matches!(
        session.apply(RealtimeEvent::Partial {
            text: "你好".to_owned(),
            stash: "世界".to_owned(),
        }),
        SessionUpdate::Transcript(snapshot) if snapshot.partial_text == "你好世界"
    ));
    assert!(matches!(
        session.apply(RealtimeEvent::Completed {
            transcript: "你好世界".to_owned(),
        }),
        SessionUpdate::Transcript(snapshot)
            if snapshot.confirmed_segments == vec!["你好世界"]
                && snapshot.partial_text.is_empty()
    ));

    assert_eq!(session.request_stop(), SessionAction::Finish);
    assert_eq!(session.state(), SessionState::Stopping);
    assert_eq!(session.request_stop(), SessionAction::Cancel);
    assert_eq!(session.state(), SessionState::Cancelled);
}

/// `Finished` while stopping completes the session; the text can be
/// taken once, then it is gone.
#[test]
fn finished_event_allows_exactly_one_injection() {
    let mut session = RecognitionSession::new("cloud-realtime");
    assert_eq!(session.request_stop(), SessionAction::Finish);

    let update = session.apply(RealtimeEvent::Finished {
        transcript: "最终文本".to_owned(),
    });
    assert!(
        matches!(update, SessionUpdate::Final(snapshot) if snapshot.authoritative_text == "最终文本")
    );
    assert_eq!(session.state(), SessionState::Completed);
    assert_eq!(session.take_injection_text().as_deref(), Some("最终文本"));
    assert_eq!(session.take_injection_text(), None);
}

/// A backend error clears the text; a cancelled session ignores a late
/// `Finished`.
#[test]
fn errors_and_cancellation_never_expose_injection_text() {
    let mut failed = RecognitionSession::new("cloud-realtime");
    assert!(matches!(
        failed.apply(RealtimeEvent::Error("network down".to_owned())),
        SessionUpdate::Error(message) if message == "network down"
    ));
    assert_eq!(failed.take_injection_text(), None);

    let mut cancelled = RecognitionSession::new("cloud-realtime");
    assert_eq!(cancelled.request_stop(), SessionAction::Finish);
    assert_eq!(cancelled.request_stop(), SessionAction::Cancel);
    assert!(matches!(
        cancelled.apply(RealtimeEvent::Finished {
            transcript: "不要注入".to_owned(),
        }),
        SessionUpdate::Ignored
    ));
    assert_eq!(cancelled.take_injection_text(), None);
}

mod common;

use common::{entries, new_log, FakeCapture, RecordingInjector, RecordingSink, ScriptedTransport};
use tokio::sync::mpsc;
use voiceless::coordinator::{run_session, ControlCommand, OutputEvent, SessionPlan};

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

async fn run_cloud_fixture(
    capture: FakeCapture,
    mut transport: ScriptedTransport,
    plan: SessionPlan,
) -> (i32, Vec<OutputEvent>, Vec<String>) {
    let (control_tx, control_rx) = mpsc::channel(4);
    let injector = RecordingInjector::new(transport.log.clone());
    let mut sink = RecordingSink::default();
    let log = transport.log.clone();
    transport.control_tx = Some(control_tx);
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(5),
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
async fn stop_delivers_tail_and_finish_before_single_final_and_injection() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.before_finish(Ok(RealtimeEvent::Partial {
        text: "你好".to_owned(),
        stash: "世界".to_owned(),
    }));
    transport.before_finish(Ok(RealtimeEvent::Completed {
        transcript: "你好世界".to_owned(),
    }));
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: "你好世界".to_owned(),
    }));
    transport.trigger(1, ControlCommand::Stop);

    let capture = FakeCapture::new(log.clone(), vec![vec![1; 1024]], Some(vec![2; 512]));
    let plan = SessionPlan::new("cloud-realtime", true);

    let (code, events, log_entries) = run_cloud_fixture(capture, transport, plan).await;

    assert_eq!(code, 0);
    // Stop order: every captured chunk and the tail frame reach the backend,
    // then exactly one finish, before any final handling.
    let tail_index = log_entries
        .iter()
        .position(|entry| entry == "transport.append:512")
        .expect("tail frame must be appended exactly once");
    let finish_index = log_entries
        .iter()
        .position(|entry| entry == "transport.finish")
        .expect("finish must be sent");
    assert!(tail_index < finish_index, "{log_entries:?}");
    assert_eq!(
        log_entries
            .iter()
            .filter(|entry| entry.starts_with("transport.append:"))
            .count(),
        2,
        "{log_entries:?}"
    );
    assert_eq!(
        log_entries
            .iter()
            .filter(|entry| *entry == "transport.finish")
            .count(),
        1,
        "{log_entries:?}"
    );

    assert_eq!(
        event_kinds(&events),
        vec![
            "ready",
            "transcript",
            "transcript",
            "final",
            "finalized",
            "completed"
        ],
        "{events:?}"
    );
    let finals: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, OutputEvent::Transcript(s) if s.is_final))
        .collect();
    assert_eq!(finals.len(), 1, "{events:?}");
    let injection = events.iter().find_map(|event| match event {
        OutputEvent::Finalized { injection } => Some(injection.clone()),
        _ => None,
    });
    assert_eq!(injection, Some(Some(common::ok_fake_injection())));
    assert_eq!(
        log_entries
            .iter()
            .filter(|entry| entry.starts_with("injector.inject:"))
            .collect::<Vec<_>>(),
        vec!["injector.inject:你好世界"],
        "{log_entries:?}"
    );
}

#[tokio::test]
async fn stop_without_speech_completes_without_injection_and_is_diagnosable() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: String::new(),
    }));
    let capture = FakeCapture::new(log.clone(), vec![vec![0; 1024]], None);
    let plan = SessionPlan::new("cloud-realtime", true);

    let (control_tx, control_rx) = mpsc::channel(4);
    let injector = RecordingInjector::new(log.clone());
    let mut sink = RecordingSink::default();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = control_tx.send(ControlCommand::Stop).await;
    });
    let code = tokio::time::timeout(
        std::time::Duration::from_secs(5),
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

    assert_eq!(code, 0);
    let kinds = event_kinds(&sink.events);
    assert!(kinds.contains(&"warning"), "{kinds:?}");
    assert_eq!(kinds.last().copied(), Some("completed"));
    assert!(
        !kinds.contains(&"final"),
        "no speech must not produce a final transcript: {kinds:?}"
    );
    let finalized = sink.events.iter().find_map(|event| match event {
        OutputEvent::Finalized { injection } => Some(injection.clone()),
        _ => None,
    });
    assert_eq!(finalized, Some(None), "{:?}", sink.events);
    let log_entries = entries(&log);
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}
