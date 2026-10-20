mod common;

use common::{
    entries, new_log, CountingProcessor, FakeCapture, RecordingHistory, RecordingInjector,
    RecordingSink, ScriptedTransport,
};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use voiceless::coordinator::{run_session, ControlCommand, SessionPlan};
use voiceless::daemon::{ActivateOutcome, Gateway, SessionRunner};
use voiceless::realtime::RealtimeEvent;

/// Runner executing the real coordinator with scripted fakes; one prepared
/// session per start() call.
struct FixtureRunner {
    log: common::Log,
    history: RecordingHistory,
    events: std::collections::VecDeque<io::Result<RealtimeEvent>>,
    sessions_started: usize,
}

impl SessionRunner for FixtureRunner {
    fn start(
        &mut self,
        control: mpsc::Receiver<ControlCommand>,
    ) -> Pin<Box<dyn Future<Output = Result<i32, String>> + Send>> {
        self.sessions_started += 1;
        let log = self.log.clone();
        let transport = ScriptedTransport::new(log.clone());
        transport.before_finish(Ok(RealtimeEvent::Ready));
        transport.before_finish(Ok(RealtimeEvent::Completed {
            transcript: "会话文本".to_owned(),
        }));
        while let Some(event) = self.events.pop_front() {
            transport.after_finish(event);
        }
        let capture = FakeCapture::new(log.clone(), vec![vec![0; 64]], None);
        let injector = RecordingInjector::new(log.clone());
        let history = self.history.clone();
        let mut sink = RecordingSink::default();
        let plan = SessionPlan::new("cloud-realtime", true);
        Box::pin(async move {
            Ok(run_session(
                plan,
                capture,
                transport,
                CountingProcessor::noop(),
                injector,
                history,
                control,
                &mut sink,
            )
            .await)
        })
    }
}

fn runner(log: common::Log) -> FixtureRunner {
    FixtureRunner {
        history: RecordingHistory::new(),
        events: std::collections::VecDeque::new(),
        sessions_started: 0,
        log,
    }
}

async fn wait_idle(gateway: &mut Gateway<FixtureRunner>) {
    for _ in 0..400 {
        let _ = gateway.poll().await;
        if !gateway.is_active() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("gateway never returned to idle");
}

async fn wait_idle_code(gateway: &mut Gateway<FixtureRunner>) -> i32 {
    for _ in 0..400 {
        if let Some(code) = gateway.poll().await {
            return code;
        }
        if !gateway.is_active() {
            panic!("session vanished without an exit code");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("gateway never returned to idle");
}

#[tokio::test]
async fn activate_twice_runs_one_session_then_normal_stop_with_single_injection() {
    let log = new_log();
    let mut runner = runner(log.clone());
    let history = runner.history.clone();
    runner.events.push_back(Ok(RealtimeEvent::Finished {
        transcript: "会话文本".to_owned(),
    }));
    let mut gateway = Gateway::new(runner);

    assert_eq!(gateway.activate(), Ok(ActivateOutcome::Started));
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Second activation while recording triggers a normal stop.
    assert_eq!(gateway.activate(), Ok(ActivateOutcome::Stopping));
    wait_idle(&mut gateway).await;

    let log_entries = entries(&log);
    assert_eq!(
        log_entries
            .iter()
            .filter(|entry| entry.starts_with("injector.inject:"))
            .count(),
        1,
        "exactly one injection per stop cycle: {log_entries:?}"
    );
    assert_eq!(history.records().len(), 1);
}

#[tokio::test]
async fn activate_during_stop_flush_is_rejected_without_a_second_session() {
    let log = new_log();
    let runner = runner(log.clone());
    // No Finished event arrives: the stop flush waits, creating a window
    // where activate must be rejected instead of starting a second session.
    let mut gateway = Gateway::new(runner);

    assert_eq!(gateway.activate(), Ok(ActivateOutcome::Started));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(gateway.activate(), Ok(ActivateOutcome::Stopping));
    let rejected = gateway.activate();
    let Err(message) = &rejected else {
        panic!("expected rejection during stopping, got {rejected:?}");
    };
    assert!(message.contains("stopping"), "{message}");

    // Unblock the flush by cancelling; no second session may have started.
    gateway.cancel().await;
    let code = wait_idle_code(&mut gateway).await;
    assert_eq!(code, 130);
    assert_eq!(
        entries(&log)
            .iter()
            .filter(|entry| *entry == "capture.start")
            .count(),
        1,
        "exactly one capture lifecycle: {:?}",
        entries(&log)
    );
}

#[tokio::test]
async fn cancel_during_recording_yields_cancelled_exit_and_no_history() {
    let log = new_log();
    let runner = runner(log.clone());
    let history = runner.history.clone();
    let mut gateway = Gateway::new(runner);

    assert_eq!(gateway.activate(), Ok(ActivateOutcome::Started));
    tokio::time::sleep(Duration::from_millis(20)).await;
    gateway.cancel().await;
    let exit = wait_idle_code(&mut gateway).await;
    assert_eq!(exit, 130);
    assert!(history.records().is_empty(), "{:?}", history.records());
}
