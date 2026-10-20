//! Audio retention lifecycle: each successful session mirrors its capture
//! to a WAV file linked from the history entry; cancel, failure and empty
//! sessions never leave a file behind.

mod common;

use std::path::PathBuf;

use common::{
    entries, new_log, CountingProcessor, FakeCapture, RecordingHistory, RecordingInjector,
    RecordingSink, ScriptedTransport,
};
use tempfile::tempdir;
use tokio::sync::mpsc;
use voiceless::batch::load_wav_pcm16;
use voiceless::capture::wav_header;
use voiceless::coordinator::{run_session, ControlCommand, SessionPlan};
use voiceless::realtime::RealtimeEvent;

fn plan(audio_dir: Option<PathBuf>) -> SessionPlan {
    let mut plan = SessionPlan::new("cloud-realtime", false);
    plan.mode_id = "quick".to_owned();
    plan.save_audio_dir = audio_dir;
    plan
}

/// Fixture: ready -> stop after the first append -> finished transcript.
async fn run_success(
    audio_dir: Option<PathBuf>,
    chunks: Vec<Vec<u8>>,
    tail: Option<Vec<u8>>,
) -> (i32, RecordingHistory, Vec<String>) {
    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.trigger(1, ControlCommand::Stop);
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: "夜声记录".to_owned(),
    }));
    let capture = FakeCapture::new(log.clone(), chunks, tail);
    let (control_tx, control_rx) = mpsc::channel(8);
    transport.control_tx = Some(control_tx);
    let history = RecordingHistory::new();
    let history_probe = history.clone();
    let mut sink = RecordingSink::default();
    let code = run_session(
        plan(audio_dir),
        capture,
        transport,
        CountingProcessor::noop(),
        RecordingInjector::new(log.clone()),
        history,
        control_rx,
        &mut sink,
    )
    .await;
    (code, history_probe, entries(&log))
}

#[tokio::test]
async fn successful_session_saves_a_readable_wav_linked_to_history() {
    let root = tempdir().expect("temporary root");
    let audio_dir = root.path().join("audio");
    let chunk_a = vec![1_u8; 1024];
    let chunk_b = vec![2_u8; 1024];
    let tail = vec![3_u8; 512];

    let (code, history, _) = run_success(
        Some(audio_dir.clone()),
        vec![chunk_a.clone(), chunk_b.clone()],
        Some(tail.clone()),
    )
    .await;

    assert_eq!(code, 0);
    let records = history.records();
    assert_eq!(records.len(), 1);
    let audio_path = records[0]
        .audio_path
        .as_ref()
        .expect("entry must link the recording");
    assert!(audio_path.ends_with(".wav"));

    let wav = std::fs::read(audio_path).expect("wav file exists");
    let mut expected_pcm = Vec::new();
    expected_pcm.extend_from_slice(&chunk_a);
    expected_pcm.extend_from_slice(&chunk_b);
    expected_pcm.extend_from_slice(&tail);
    assert_eq!(
        &wav[..44],
        &wav_header(expected_pcm.len() as u64, 16_000),
        "finalized header must carry the real data size"
    );
    assert_eq!(wav[44..], expected_pcm[..]);

    // The saved file must be consumable by Voiceless's own batch path.
    let decoded = load_wav_pcm16(PathBuf::from(audio_path).as_path()).expect("parse saved wav");
    assert_eq!(decoded, expected_pcm);

    let expected_seconds = expected_pcm.len() as f64 / 32_000.0;
    assert_eq!(records[0].duration_seconds, Some(expected_seconds));
}

#[tokio::test]
async fn cancelled_session_leaves_no_partial_recording() {
    let root = tempdir().expect("temporary root");
    let audio_dir = root.path().join("audio");

    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.trigger(1, ControlCommand::Cancel);
    let capture = FakeCapture::new(log.clone(), vec![vec![7_u8; 1024]], None);
    let (control_tx, control_rx) = mpsc::channel(8);
    transport.control_tx = Some(control_tx);
    let history = RecordingHistory::new();
    let history_probe = history.clone();
    let mut sink = RecordingSink::default();
    let code = run_session(
        plan(Some(audio_dir.clone())),
        capture,
        transport,
        CountingProcessor::noop(),
        RecordingInjector::new(log.clone()),
        history,
        control_rx,
        &mut sink,
    )
    .await;

    assert_eq!(code, 130);
    assert!(history_probe.records().is_empty());
    let leftovers: Vec<_> = std::fs::read_dir(&audio_dir)
        .map(|entries| entries.collect::<Result<Vec<_>, _>>().unwrap_or_default())
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "cancel must not leave wav or partial files: {leftovers:?}"
    );
}

#[tokio::test]
async fn disabled_saving_records_history_without_audio() {
    let root = tempdir().expect("temporary root");
    let (code, history, _) = run_success(None, vec![vec![9_u8; 1024]], None).await;

    assert_eq!(code, 0);
    let records = history.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].audio_path, None);
    assert!(records[0].duration_seconds.is_some());
    assert!(
        !root.path().join("audio").exists(),
        "disabled saving must not create the audio directory"
    );
}

#[tokio::test]
async fn empty_session_saves_no_file_even_when_saving_is_enabled() {
    let root = tempdir().expect("temporary root");
    let audio_dir = root.path().join("audio");

    let log = new_log();
    let mut transport = ScriptedTransport::new(log.clone());
    transport.before_finish(Ok(RealtimeEvent::Ready));
    transport.after_finish(Ok(RealtimeEvent::Finished {
        transcript: String::new(),
    }));
    let mut capture = FakeCapture::new(log.clone(), vec![], None);
    capture.eof_on_empty = true;
    let (control_tx, control_rx) = mpsc::channel::<ControlCommand>(8);
    transport.control_tx = Some(control_tx);
    let history = RecordingHistory::new();
    let history_probe = history.clone();
    let mut sink = RecordingSink::default();
    let code = run_session(
        plan(Some(audio_dir.clone())),
        capture,
        transport,
        CountingProcessor::noop(),
        RecordingInjector::new(log.clone()),
        history,
        control_rx,
        &mut sink,
    )
    .await;

    assert_eq!(code, 0);
    assert!(history_probe.records().is_empty(), "no text, no record");
    let leftovers: Vec<_> = std::fs::read_dir(&audio_dir)
        .map(|entries| entries.collect::<Result<Vec<_>, _>>().unwrap_or_default())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "no audio was captured: {leftovers:?}");
}
