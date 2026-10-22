//! Contract of `ctl::RelayedSession`, the pure tracker behind `voiceless
//! stream` delegating to the daemon: daemon `Event` lines map to the
//! standalone `stream` exit codes (`0`, `1`, `130`), the session boundary is
//! our own `recording` -> `idle`, and only coordinator events are echoed to
//! the terminal. No D-Bus involved: events are hand-built JSON values.

use serde_json::json;
use voiceless::ctl::RelayedSession;

/// Feed `events` in order and return the first exit code produced.
fn run(events: &[serde_json::Value]) -> Option<i32> {
    let mut session = RelayedSession::default();
    events.iter().find_map(|event| session.observe(event))
}

/// A daemon `state` event as `serve_with_gateway` publishes it.
fn state(name: &str) -> serde_json::Value {
    json!({"type": "state", "state": name, "mode": "quick"})
}

/// completed -> 0, cancelled -> 130, error -> 1, and an idle without any
/// session event (task died early) -> 1.
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

/// A stale idle before our start, or a stopping without idle, yields no
/// code yet.
#[test]
fn relayed_session_waits_for_its_own_start_and_end() {
    // An idle left over from before this session started is not its end.
    assert_eq!(run(&[state("idle")]), None);
    assert_eq!(run(&[state("recording"), state("stopping")]), None);
}

/// Coordinator event kinds are printable; the daemon's own `state`, `log`,
/// `session` and `level` lines are not.
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
