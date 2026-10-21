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
        _mode: &str,
        _inject: Option<bool>,
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

#[tokio::test]
async fn hold_to_talk_start_and_stop_run_one_session_and_reject_out_of_order_calls() {
    let log = new_log();
    let mut runner = runner(log.clone());
    let history = runner.history.clone();
    runner.events.push_back(Ok(RealtimeEvent::Finished {
        transcript: "按住说话".to_owned(),
    }));
    let mut gateway = Gateway::new(runner);

    // Release before press (e.g. a key-repeat artefact) is a no-op error.
    assert!(gateway.stop().is_err());
    assert_eq!(gateway.start("quick"), Ok(ActivateOutcome::Started));
    // A repeated press while recording must not start a second session.
    let Err(message) = gateway.start("quick") else {
        panic!("second start must be rejected");
    };
    assert!(message.contains("recording"), "{message}");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(gateway.stop(), Ok(ActivateOutcome::Stopping));
    assert!(gateway.stop().is_err(), "stop while stopping is rejected");
    wait_idle(&mut gateway).await;

    assert_eq!(gateway.state(), voiceless::daemon::State::Idle);
    assert_eq!(history.records().len(), 1);
    assert_eq!(
        entries(&log)
            .iter()
            .filter(|entry| entry.starts_with("injector.inject:"))
            .count(),
        1
    );
}

mod hotkey_machine {
    use std::time::{Duration, Instant};
    use voiceless::daemon::{HotkeyMachine, KeyAction, State};

    fn machine() -> HotkeyMachine {
        HotkeyMachine::new(
            Duration::from_millis(350),
            "quick".to_owned(),
            "prompt-optimize".to_owned(),
        )
    }

    #[test]
    fn holding_past_the_threshold_stops_on_release() {
        let mut keys = machine();
        let t0 = Instant::now();
        assert_eq!(
            keys.on_press(State::Idle, false, t0),
            KeyAction::Start("quick".to_owned())
        );
        assert_eq!(
            keys.on_release(State::Recording, t0 + Duration::from_millis(900)),
            KeyAction::Stop
        );
    }

    #[test]
    fn a_tap_latches_and_the_next_press_stops() {
        let mut keys = machine();
        let t0 = Instant::now();
        keys.on_press(State::Idle, false, t0);
        assert_eq!(
            keys.on_release(State::Recording, t0 + Duration::from_millis(120)),
            KeyAction::Nothing,
            "a tap keeps recording"
        );
        assert_eq!(
            keys.on_press(State::Recording, false, t0 + Duration::from_secs(5)),
            KeyAction::Stop
        );
        // The release of that stopping press does nothing more.
        assert_eq!(
            keys.on_release(State::Stopping, t0 + Duration::from_secs(6)),
            KeyAction::Nothing
        );
    }

    #[test]
    fn ctrl_selects_the_organize_mode_and_stopping_ignores_keys() {
        let mut keys = machine();
        let t0 = Instant::now();
        assert_eq!(
            keys.on_press(State::Idle, true, t0),
            KeyAction::Start("prompt-optimize".to_owned())
        );
        assert_eq!(
            keys.on_press(State::Stopping, false, t0),
            KeyAction::Nothing
        );
    }

    #[test]
    fn a_press_stops_a_session_started_elsewhere() {
        // e.g. started by `voiceless ctl toggle`: no press is pending.
        let mut keys = machine();
        let t0 = Instant::now();
        assert_eq!(keys.on_press(State::Recording, false, t0), KeyAction::Stop);
        assert_eq!(keys.on_release(State::Stopping, t0), KeyAction::Nothing);
    }
}

#[test]
fn hotkey_and_audio_config_are_validated() {
    use voiceless::config::AppConfig;
    let config = AppConfig::from_toml("[hotkey]\nkey = \"KEY_SCROLLLOCK\"\n").unwrap();
    assert_eq!(config.hotkey.hold_threshold_ms, 350);
    assert!(config.audio.pause_media);
    assert!(!config.audio.cues);
    assert!(AppConfig::from_toml("[hotkey]\nkey = \"KEY_NOPE\"\n").is_err());
}
