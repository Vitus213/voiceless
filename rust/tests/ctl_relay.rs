use serde_json::json;
use voiceless::ctl::RelayedSession;

fn run(events: &[serde_json::Value]) -> Option<i32> {
    let mut session = RelayedSession::default();
    events.iter().find_map(|event| session.observe(event))
}

fn state(name: &str) -> serde_json::Value {
    json!({"type": "state", "state": name, "mode": "quick"})
}

#[test]
fn relayed_session_maps_daemon_events_to_stream_exit_codes() {
    let completed = [
        state("recording"),
        json!({"type": "finalized"}),
        json!({"type": "completed"}),
        state("stopping"),
        state("idle"),
    ];
    assert_eq!(run(&completed), Some(0));

    let cancelled = [
        state("recording"),
        json!({"type": "cancelled"}),
        json!({"type": "completed"}),
        state("idle"),
    ];
    assert_eq!(run(&cancelled), Some(130));

    let failed = [
        state("recording"),
        json!({"type": "error", "message": "out of memory"}),
        json!({"type": "completed"}),
        state("idle"),
    ];
    assert_eq!(run(&failed), Some(1));

    // The session task died before emitting any session event.
    assert_eq!(run(&[state("recording"), state("idle")]), Some(1));
}

#[test]
fn relayed_session_waits_for_its_own_start_and_end() {
    // An idle left over from before this session started is not its end.
    assert_eq!(run(&[state("idle")]), None);
    assert_eq!(run(&[state("recording"), state("stopping")]), None);
}

#[test]
fn only_stream_events_are_printed() {
    for kind in [
        "ready",
        "transcript",
        "finalized",
        "warning",
        "error",
        "cancelled",
        "completed",
    ] {
        assert!(
            RelayedSession::is_session_event(&json!({"type": kind})),
            "{kind}"
        );
    }
    for kind in ["state", "log", "session", "level"] {
        assert!(
            !RelayedSession::is_session_event(&json!({"type": kind})),
            "{kind}"
        );
    }
}
