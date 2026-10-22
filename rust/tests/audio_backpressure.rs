//! Bounded, lossless audio flow of `coordinator::run_session`: the queue
//! bound trips instead of dropping or reordering, a torn tail frame fails
//! the session before `finish`, and every chunk reaches the backend exactly
//! once in capture order. Fakes from `common`: `FakeCapture` (scripted
//! chunks and tail), `ScriptedTransport` (scripted events, append delay,
//! control triggers), `RecordingInjector`, `RecordingSink`.

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

/// Run one session with the fakes (5 s guard). The transport gets the
/// control sender so its scripted triggers can issue Stop/Cancel.
async fn run_fixture(
    plan: SessionPlan,
    capture: FakeCapture,
    transport: ScriptedTransport,
) -> (i32, Vec<OutputEvent>, Vec<String>) {
    let (control_tx, control_rx) = mpsc::channel(8);
    let log = transport.log.clone();
    let mut transport = transport;
    transport.control_tx = Some(control_tx.clone());
    let injector = RecordingInjector::new(log.clone());
    let mut sink = RecordingSink::default();
    let code = tokio::time::timeout(
        Duration::from_secs(5),
        run_session(
            plan,
            capture,
            transport,
            CountingProcessor::noop(),
            injector,
            RecordingHistory::new(),
            control_rx,
            &mut sink,
        ),
    )
    .await
    .expect("run_session must terminate");
    (code, sink.events, entries(&log))
}

#[tokio::test]
async fn default_capture_contract_is_16khz_mono_pcm16_32ms_chunks() {
    use voiceless::capture::{CHUNK_BYTES, SAMPLE_RATE};
    // 32 ms of 16 kHz mono PCM16 = 0.032 * 16000 * 2 bytes.
    assert_eq!(SAMPLE_RATE, 16_000);
    assert_eq!(CHUNK_BYTES, 1_024);
}

/// A 3-byte tail after stop: error + completed, no finish, no injection.
#[tokio::test]
async fn odd_byte_tail_frame_fails_without_final_or_injection() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.trigger(1, ControlCommand::Stop);
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: "不应出现".to_owned(),
    }));
    let capture = FakeCapture::new(log.clone(), vec![vec![0; 1024]], Some(vec![1, 2, 3]));
    let plan = SessionPlan::new("cloud-realtime", true);

    let (code, events, log_entries) = run_fixture(plan, capture, transport).await;

    assert_eq!(code, 1);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"error"), "{kinds:?}");
    assert_eq!(kinds.last().copied(), Some("completed"));
    assert!(!kinds.contains(&"final"), "{kinds:?}");
    assert!(!kinds.contains(&"finalized"), "{kinds:?}");
    assert!(
        !log_entries.iter().any(|entry| entry == "transport.finish"),
        "finish must not be sent after an incomplete tail: {log_entries:?}"
    );
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}

#[tokio::test]
async fn slow_transport_overrunning_the_bounded_queue_fails_the_session() {
    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.append_delay = Duration::from_millis(30);
    // Capture produces far faster than the transport drains; the bounded
    // queue (capacity 4 here) must trip instead of growing or dropping.
    let mut capture = FakeCapture::new(
        log.clone(),
        (0..64).map(|_| vec![0_u8; 1024]).collect(),
        None,
    );
    capture.chunk_interval = Duration::from_millis(2);
    let mut plan = SessionPlan::new("cloud-realtime", true);
    plan.queue_capacity = 4;
    plan.send_deadline = Duration::from_secs(2);

    let (code, events, log_entries) = run_fixture(plan, capture, transport).await;

    assert_eq!(code, 1);
    let kinds = event_kinds(&events);
    assert!(kinds.contains(&"error"), "{kinds:?}");
    assert_eq!(kinds.last().copied(), Some("completed"));
    assert!(!kinds.contains(&"final"), "{kinds:?}");
    let OutputEvent::Error(message) = events
        .iter()
        .find(|event| matches!(event, OutputEvent::Error(_)))
        .expect("error event")
    else {
        unreachable!()
    };
    assert!(
        message.contains("backlog") || message.contains("queue"),
        "{message}"
    );
    assert!(
        log_entries.iter().any(|entry| entry == "capture.abort"),
        "capture must be stopped on overrun: {log_entries:?}"
    );
    assert!(
        !log_entries
            .iter()
            .any(|entry| entry.starts_with("injector.inject:")),
        "{log_entries:?}"
    );
}

/// Capture EOF acts as the stop: three chunks and the tail are appended
/// once each, in order, then one final transcript.
#[tokio::test]
async fn chunks_are_delivered_in_capture_order_exactly_once() {
    let log = new_log();
    let transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: "完整".to_owned(),
    }));
    let chunks: Vec<Vec<u8>> = (0..3).map(|index| vec![index as u8 + 1; 64]).collect();
    let mut capture = FakeCapture::new(log.clone(), chunks, Some(vec![9; 32]));
    capture.eof_on_empty = true;
    let mut plan = SessionPlan::new("cloud-realtime", false);
    plan.queue_capacity = 8;

    let (code, events, log_entries) = run_fixture(plan, capture, transport).await;

    assert_eq!(code, 0);
    let appends: Vec<&String> = log_entries
        .iter()
        .filter(|entry| entry.starts_with("transport.append:"))
        .collect();
    assert_eq!(
        appends,
        vec![
            "transport.append:64",
            "transport.append:64",
            "transport.append:64",
            "transport.append:32",
        ],
        "every chunk and the tail must arrive once, in order: {log_entries:?}"
    );
    assert!(event_kinds(&events).contains(&"final"));
}
