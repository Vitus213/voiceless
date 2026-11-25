use std::time::Duration;
use voiceless::meeting_capture::{
    Failure, MeetingError, MeetingRuntime, MeetingState, Selection, TrackKind,
};

#[tokio::test]
async fn catalog_resolves_host_ids_and_rejects_a_missing_selection_before_recording() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = MeetingRuntime::new(dir.path());
    let catalog = runtime.sources().await.unwrap();
    println!("{}", serde_json::to_string(&catalog).unwrap());
    for source in catalog.microphones.iter().chain(&catalog.system_outputs) {
        let id = source.node.parse::<cpal::DeviceId>().unwrap();
        assert_eq!(id.0, cpal::default_host().id());
    }
    let missing = "voiceless-absent-test-device".to_owned();
    let selection = Selection {
        microphone: missing.clone(),
        system: None,
    };
    assert!(
        matches!(runtime.start(selection).await, Err(MeetingError::Failure(Failure::SourceMissing { source: TrackKind::Microphone, node })) if node == missing)
    );
    if let Some(mic) = catalog.microphones.first() {
        let selection = Selection {
            microphone: mic.node.clone(),
            system: Some(missing.clone()),
        };
        assert!(
            matches!(runtime.start(selection).await, Err(MeetingError::Failure(Failure::SourceMissing { source: TrackKind::System, node })) if node == missing)
        );
    }
    assert!(runtime.meetings().is_empty());
}

#[tokio::test]
#[ignore = "requires denied microphone access and a test-owned selected endpoint"]
async fn denied_fixture_microphone_identifies_source_and_rolls_back_start() {
    let microphone = std::env::var("VOICELESS_TEST_MEETING_MIC").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = MeetingRuntime::new(dir.path());
    let result = runtime
        .start(Selection {
            microphone: microphone.clone(),
            system: None,
        })
        .await;
    println!("{result:?}");
    assert!(
        matches!(result, Err(MeetingError::Failure(Failure::PermissionDenied { source: TrackKind::Microphone, node, .. })) if node == microphone)
    );
    assert!(runtime.meetings().is_empty());
    std::fs::File::open(dir.path().join("capture.lock"))
        .unwrap()
        .try_lock()
        .unwrap();
}

#[tokio::test]
#[ignore = "requires test-owned endpoints playing 330 Hz at the microphone and 880 Hz at the selected output"]
async fn selected_fixture_tracks_drain_and_release_the_meeting_lease() {
    let selection = Selection {
        microphone: std::env::var("VOICELESS_TEST_MEETING_MIC").unwrap(),
        system: Some(std::env::var("VOICELESS_TEST_MEETING_OUTPUT").unwrap()),
    };
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = MeetingRuntime::new(dir.path());
    let mut other = MeetingRuntime::new(dir.path());
    let meeting = runtime.start(selection.clone()).await.unwrap();
    assert_eq!(meeting.scope.microphone.node, selection.microphone);
    assert_eq!(
        meeting.scope.system.unwrap().node,
        selection.system.clone().unwrap()
    );
    assert!(matches!(
        other.start(selection).await,
        Err(MeetingError::CaptureBusy)
    ));
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            assert!(runtime.poll().is_none(), "capture interrupted");
            if runtime
                .meeting(&meeting.id)
                .unwrap()
                .tracks
                .iter()
                .all(|track| {
                    track.received_bytes >= 32000 && track.level.is_some_and(|rms| rms > 0.01)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture audio missing: {:?}", runtime.meeting(&meeting.id)));
    runtime.stop().unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(meeting) = runtime.poll() {
                break meeting;
            }
            runtime.changed().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(settled.state, MeetingState::Completed, "{settled:?}");
    // Each fixture plays its own sine. Detect a silent track or a loopback
    // that accidentally opened the microphone side of a duplex endpoint.
    for track in &settled.tracks {
        let wav = std::fs::read(track.path.as_ref().unwrap()).unwrap();
        let samples = wav[44..]
            .chunks_exact(2)
            .map(|s| f64::from(i16::from_le_bytes([s[0], s[1]])))
            .collect::<Vec<_>>();
        assert_eq!(samples.len() as u64 * 2, track.committed_bytes);
        let power = |hz: f64| {
            let (mut sin, mut cos) = (0.0, 0.0);
            for (i, sample) in samples.iter().enumerate() {
                let phase = std::f64::consts::TAU * hz * i as f64 / 16000.0;
                sin += sample * phase.sin();
                cos += sample * phase.cos();
            }
            sin.hypot(cos) / samples.len() as f64
        };
        let (mic, output) = (power(330.0), power(880.0));
        println!(
            "{:?}: {} samples, 330 Hz {mic:.2}, 880 Hz {output:.2}",
            track.kind,
            samples.len()
        );
        let (selected, other) = if track.kind == TrackKind::Microphone {
            (mic, output)
        } else {
            (output, mic)
        };
        assert!(selected > 10.0 && selected > 3.0 * other);
    }
    std::fs::File::open(dir.path().join("capture.lock"))
        .unwrap()
        .try_lock()
        .unwrap();
}
