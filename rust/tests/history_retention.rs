mod common;

use std::fs;

use rusqlite::{params, Connection};
use tempfile::tempdir;
use time::OffsetDateTime;
use voiceless::capture::wav_header;
use voiceless::history::retention::{
    apply, preview, Applied, ExportFormat, FailedSession, RecoveryStore, RetentionPolicy,
};
use voiceless::history::{HistoryFilter, HistoryStore};

#[test]
fn audio_expires_before_searchable_text_and_a_changed_selection_requires_confirmation() {
    let root = tempdir().unwrap();
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).unwrap();
    let db = Connection::open(path).unwrap();
    let audio = root.path().join("audio");
    fs::create_dir(&audio).unwrap();
    let recording = audio.join("sample.wav");
    fs::write(&recording, b"recording").unwrap();
    let insert = |id: &str, wav: Option<&str>| {
        db.execute("INSERT INTO recognition_history(id,created_at,raw_text,final_text,status,character_count,audio_path) VALUES(?1,'2025-01-01T00:00:00Z','alpha','alpha','completed',5,?2)", params![id, wav]).unwrap();
    };
    insert("old", recording.to_str());
    let recovery = RecoveryStore::new(root.path().join("recovery"));
    let policy = RetentionPolicy {
        text_days: 30,
        audio_days: 7,
        ..Default::default()
    };
    let exact_cutoff =
        OffsetDateTime::from_unix_timestamp(1_735_689_600).unwrap() + time::Duration::days(7);
    let boundary = preview(&store, &recovery, &policy, &audio, exact_cutoff).unwrap();
    assert_eq!(
        boundary.audio.recordings, 0,
        "the recording is still inside its retention period at the cutoff"
    );
    let now = OffsetDateTime::from_unix_timestamp(1_736_467_200).unwrap(); // January 2025, after day seven.
    let first = preview(&store, &recovery, &policy, &audio, now).unwrap();
    assert_eq!((first.text.records, first.audio.recordings), (0, 1));
    assert!(recording.exists());
    assert!(
        matches!(apply(&store, &recovery, &first.snapshot, &audio, now).unwrap(), Applied::Removed(r) if r.recordings == 1)
    );
    assert!(!recording.exists());
    let filter = HistoryFilter {
        text: "alpha".into(),
        ..Default::default()
    };
    let result = store.search(&filter, 10, None).unwrap();
    assert_eq!(result.records[0].audio_path, None);
    assert_eq!(
        store
            .export(&filter, ExportFormat::Text, false, now)
            .unwrap()
            .bytes,
        b"alpha\n\n"
    );
    let later = now + time::Duration::days(30);
    let second = preview(&store, &recovery, &policy, &audio, later).unwrap();
    insert("another", None);
    let Applied::Changed(updated) =
        apply(&store, &recovery, &second.snapshot, &audio, later).unwrap()
    else {
        panic!("stale confirmation deleted records")
    };
    assert_eq!(updated.text.records, 2);
    assert_eq!(store.totals().unwrap().records, 2);
    assert!(
        matches!(apply(&store, &recovery, &updated.snapshot, &audio, later).unwrap(), Applied::Removed(r) if r.records == 2)
    );
    assert!(store.search(&filter, 10, None).unwrap().records.is_empty());
    assert!(store
        .export(&filter, ExportFormat::Text, false, later)
        .unwrap()
        .bytes
        .is_empty());
}

#[test]
fn failed_audio_is_private_and_expires_without_entering_successful_history() {
    let root = tempdir().unwrap();
    let store = HistoryStore::open(root.path().join("history.sqlite3")).unwrap();
    let area = RecoveryStore::new(root.path().join("recovery"));
    let capture = root.path().join("capture.wav");
    let mut wav = wav_header(4, 16_000).to_vec();
    wav.extend_from_slice(&[1, 0, 2, 0]);
    fs::write(&capture, &wav).unwrap();
    let saved = area
        .keep(
            &capture,
            FailedSession {
                reason: "transport disconnected",
                backend: "local-offline",
                mode: "quick",
                app_id: None,
            },
        )
        .unwrap();
    let path = area.audio_path(&saved.id).unwrap();
    assert_eq!(fs::read(&path).unwrap(), wav);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert_eq!(area.list().unwrap(), vec![saved.clone()]);
    let now = OffsetDateTime::parse(
        &saved.saved_at,
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
        + time::Duration::days(2);
    let audio = root.path().join("audio");
    let policy = RetentionPolicy {
        failed_audio_days: 1,
        ..Default::default()
    };
    let queued = preview(&store, &area, &policy, &audio, now).unwrap();
    assert_eq!((queued.text.records, queued.recovery.recordings), (0, 1));
    assert!(
        matches!(apply(&store, &area, &queued.snapshot, &audio, now).unwrap(), Applied::Removed(r) if r.failed_recordings == 1)
    );
    assert!(area.list().unwrap().is_empty());
    assert_eq!(store.totals().unwrap().records, 0);
}

#[tokio::test]
async fn opted_in_recovery_keeps_capture_failure_but_discards_user_cancellation() {
    use common::{
        new_log, CountingProcessor, FakeCapture, RecordingHistory, RecordingInjector,
        RecordingSink, ScriptedTransport,
    };
    use voiceless::coordinator::{run_session, ControlCommand, FailureRecovery, SessionPlan};
    use voiceless::realtime::RealtimeEvent;

    for cancel in [false, true] {
        let root = tempdir().unwrap();
        let directory = root.path().join("recovery");
        let mut plan = SessionPlan::new("local-offline", false);
        plan.failure_recovery = Some(FailureRecovery {
            directory: directory.clone(),
            app_id: None,
        });
        let log = new_log();
        let mut capture = FakeCapture::new(log.clone(), vec![vec![7; 1024]], None);
        let mut transport = ScriptedTransport::new(log.clone());
        transport.before_finish(Ok(RealtimeEvent::Ready));
        let (send, receive) = tokio::sync::mpsc::channel(8);
        transport.control_tx = Some(send);
        if cancel {
            transport.trigger(1, ControlCommand::Cancel);
        } else {
            capture.end_error = Some("microphone disconnected".into());
        }
        let history = RecordingHistory::new();
        let code = run_session(
            plan,
            capture,
            transport,
            CountingProcessor::noop(),
            RecordingInjector::new(log),
            history.clone(),
            receive,
            &mut RecordingSink::default(),
        )
        .await;
        assert!(history.records().is_empty());
        let area = RecoveryStore::new(directory);
        let saved = area.list().unwrap();
        if cancel {
            assert_eq!(code, 130);
            assert!(saved.is_empty());
        } else {
            assert_ne!(code, 0);
            assert_eq!(saved.len(), 1);
            assert!(saved[0].reason.contains("microphone disconnected"));
            let bytes = fs::read(area.audio_path(&saved[0].id).unwrap()).unwrap();
            assert_eq!(&bytes[44..], &[7; 1024]);
        }
    }
}
