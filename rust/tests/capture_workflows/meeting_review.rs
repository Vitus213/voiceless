use serde_json::json;
use std::{fs, io::Write};
use voiceless::{
    capture::wav_header,
    meeting_capture::MeetingRuntime,
    meeting_review::{local_highlights, Error, Review, SpeakerSource},
    transcription_jobs::{
        worker, ImportRequest, ImportSource, JobQueue, QueueOptions, RecognitionRequest, Segment,
        Stage, Transport,
    },
};

#[test]
fn meeting_review_tracks_queue_timing_pitch_and_manual_provenance() {
    let root = tempfile::tempdir().unwrap();
    let id = "01234567-89ab-cdef-0123-456789abcdef";
    let dir = root.path().join("meetings").join(id);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("microphone.wav");
    let frequencies = [125.0, 235.0, 125.0, 0.0];
    let samples = 16000 * frequencies.len();
    let mut wav = fs::File::create(&path).unwrap();
    wav.write_all(&wav_header((samples * 2) as u64, 16000))
        .unwrap();
    for hz in frequencies {
        let mut pcm = Vec::with_capacity(32000);
        let mut phase = 0.0;
        for i in 0..16000 {
            let intonation = 0.3 * (std::f64::consts::TAU * f64::from(i) / 8000.).sin();
            phase += std::f64::consts::TAU * hz * intonation.exp2() / 16000.;
            pcm.extend_from_slice(
                &((phase.sin() * 6000. + (2. * phase).sin() * 1800.) as i16).to_le_bytes(),
            );
        }
        wav.write_all(&pcm).unwrap();
    }
    drop(wav);
    fs::write(dir.join("meeting.json"),json!({"version":1,"id":id,"state":"completed","started_at":"2026-01-01T00:00:00Z","pid":1,"tracks":[{"kind":"microphone","node":"mic","label":"Mic","status":"finalized","committed_bytes":samples*2}]}).to_string()).unwrap();
    let meeting = MeetingRuntime::new(root.path()).meeting(id).unwrap();
    let queue = JobQueue::open(root.path(), QueueOptions::default()).unwrap();
    let job = queue
        .submit(ImportRequest {
            source: ImportSource::Path(path),
            name: None,
            recognition: RecognitionRequest {
                model: "fixture".into(),
                transport: Transport::Local,
                language: None,
                cloud_authorized: false,
            },
        })
        .unwrap();
    let mut review = Review::load(&meeting).unwrap();
    assert!(review.attach(&meeting, queue.get(&job).unwrap()).is_err());
    worker::import(&queue, queue.claim(Stage::Import).unwrap());
    let lease = queue.claim(Stage::Transcribe).unwrap().lease;
    let paragraphs: Vec<_> = [
        "first voice",
        "second voice",
        "first voice again",
        "silence",
    ]
    .iter()
    .enumerate()
    .map(|(i, text)| Segment {
        start: i as u64 * 16000,
        end: (i as u64 + 1) * 16000,
        text: (*text).into(),
    })
    .collect();
    assert!(queue.commit(&lease, &paragraphs, 64000));
    assert!(queue.complete(&lease));
    review.attach(&meeting, queue.get(&job).unwrap()).unwrap();
    review.separate(&meeting).unwrap();
    assert!(review.paragraphs[0].speaker_source == SpeakerSource::LocalPitch);
    assert!(review.paragraphs[1].speaker_source == SpeakerSource::LocalPitch);
    assert_ne!(review.paragraphs[0].speaker, review.paragraphs[1].speaker);
    assert_eq!(review.paragraphs[0].speaker, review.paragraphs[2].speaker);
    assert!(review.paragraphs[3].speaker.is_none());
    let second = &mut review.paragraphs[1];
    assert_eq!((second.start, second.end), (16000, 32000));
    second.text = "corrected decision".into();
    second.speaker = Some("Robin".into());
    second.speaker_source = SpeakerSource::Manual;
    review.separate(&meeting).unwrap();
    let mut stale = Review::load(&meeting).unwrap();
    review.save(&meeting).unwrap();
    assert!(matches!(stale.save(&meeting), Err(Error::Conflict)));
    let reopened = Review::load(&meeting).unwrap();
    assert_eq!(reopened.paragraphs[1].speaker.as_deref(), Some("Robin"));
    assert_eq!(reopened.paragraphs[1].original, "second voice");
    let ids = [reopened.paragraphs[1].id.clone()];
    assert_eq!(
        local_highlights(&reopened.selected(&ids).unwrap()),
        "• corrected decision"
    );
    assert!(reopened.selected(&["not-a-segment".into()]).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.join("review.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
