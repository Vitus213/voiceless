use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tempfile::{tempdir, NamedTempFile};
use voiceless::transcription_jobs::{
    worker, Activity, ExportFormat, FailureCode, ImportRequest, ImportSource, JobFailure, JobQueue,
    PriorityResource, QueueOptions, RecognitionRequest, Segment, SharedResource, Stage, Status,
    Transport,
};

const SPEECH: &[u8] = include_bytes!("../../frontend/e2e/fixtures/jfk-16k-mono-s16le.pcm");

fn wav() -> Vec<u8> {
    let header = voiceless::capture::wav_header(SPEECH.len() as u64, 16_000);
    [header.as_slice(), SPEECH].concat()
}

fn upload(queue: &JobQueue, bytes: &[u8]) -> ImportSource {
    let mut file = NamedTempFile::new_in(queue.incoming_dir()).unwrap();
    file.write_all(bytes).unwrap();
    ImportSource::Upload(file)
}

fn submit(
    queue: &JobQueue,
    name: &str,
    source: ImportSource,
    transport: Transport,
    consent: bool,
) -> String {
    queue
        .submit(ImportRequest {
            name: Some(name.into()),
            source,
            recognition: RecognitionRequest {
                transport,
                cloud_authorized: consent,
                model: "sense-voice-int8".into(),
                language: None,
            },
        })
        .unwrap()
}

fn open(path: &Path) -> JobQueue {
    JobQueue::open(path, QueueOptions::default()).unwrap()
}

#[test]
fn import_independent_files_and_cloud_consent() {
    let root = tempdir().unwrap();
    let queue = open(root.path());
    let denied = submit(
        &queue,
        "private.wav",
        upload(&queue, SPEECH),
        Transport::Cloud,
        false,
    );
    let bad = submit(
        &queue,
        "broken.wav",
        upload(&queue, b"RIFF0000WAVE"),
        Transport::Local,
        false,
    );
    let good = submit(
        &queue,
        "speech.wav",
        upload(&queue, &wav()),
        Transport::Local,
        false,
    );
    let cloud = submit(
        &queue,
        "allowed.wav",
        upload(&queue, &wav()),
        Transport::Cloud,
        true,
    );
    assert_eq!(
        queue.get(&denied).unwrap().summary.error.unwrap().code,
        FailureCode::CloudNotAuthorized
    );
    assert!(queue.retry(&denied).is_err());
    for id in [&bad, &good, &cloud] {
        let claim = queue.claim(Stage::Import).unwrap();
        assert_eq!(claim.lease.id(), id);
        worker::import(&queue, claim);
    }
    assert_eq!(queue.get(&bad).unwrap().summary.status, Status::Failed);
    let local = queue.claim(Stage::Transcribe).unwrap();
    assert_eq!(local.lease.id(), good);
    assert_eq!(
        local.snapshot.duration_samples,
        Some(SPEECH.len() as u64 / 2)
    );
    assert!(!local.snapshot.request.cloud_authorized);
    let authorized = queue.claim(Stage::Transcribe).unwrap();
    assert_eq!(authorized.lease.id(), cloud);
    assert!(authorized.snapshot.request.cloud_authorized);
}

#[test]
fn import_measures_the_pcm_in_a_streamed_dashscope_wav() {
    let root = tempdir().unwrap();
    let queue = open(root.path());
    let mut recording = wav();
    recording[4..8].copy_from_slice(&0x7fff_ffbf_u32.to_le_bytes());
    recording[40..44].copy_from_slice(&0x7fff_ff9b_u32.to_le_bytes());
    let id = submit(
        &queue,
        "streamed.wav",
        upload(&queue, &recording),
        Transport::Local,
        false,
    );
    worker::import(&queue, queue.claim(Stage::Import).unwrap());
    let claim = queue.claim(Stage::Transcribe).unwrap();
    assert_eq!(claim.lease.id(), id);
    assert_eq!(
        claim.snapshot.duration_samples,
        Some(SPEECH.len() as u64 / 2)
    );
}

#[test]
fn import_refuses_a_wav_ending_inside_a_pcm_frame() {
    let root = tempdir().unwrap();
    let queue = open(root.path());
    let mut recording = voiceless::capture::wav_header(3, 16_000).to_vec();
    recording.extend_from_slice(&[0; 3]);
    let id = submit(
        &queue,
        "partial.wav",
        upload(&queue, &recording),
        Transport::Local,
        false,
    );
    worker::import(&queue, queue.claim(Stage::Import).unwrap());
    let job = queue.get(&id).unwrap();
    assert_eq!(job.summary.error.unwrap().code, FailureCode::Corrupt);
    assert!(queue.claim(Stage::Transcribe).is_none());
}

#[test]
fn recover_committed_boundary_after_restart_pause_retry_cancel_and_source_loss() {
    let root = tempdir().unwrap();
    let queue = open(root.path());
    let audio = wav();
    let id = submit(
        &queue,
        "speech.wav",
        upload(&queue, &audio),
        Transport::Local,
        false,
    );
    worker::import(&queue, queue.claim(Stage::Import).unwrap());
    let claim = queue.claim(Stage::Transcribe).unwrap();
    let first = Segment {
        start: 800,
        end: 16_000,
        text: "first".into(),
    };
    assert!(queue.commit(&claim.lease, std::slice::from_ref(&first), 16_000));
    drop(queue);

    let queue = open(root.path());
    assert_eq!(queue.recovery().resumed, std::slice::from_ref(&id));
    let resumed = queue.claim(Stage::Transcribe).unwrap();
    assert_eq!(resumed.snapshot.committed_until, first.end);
    queue.pause(&id).unwrap();
    assert!(!queue.complete(&resumed.lease));
    drop(queue);
    let queue = open(root.path());
    assert_eq!(queue.get(&id).unwrap().summary.status, Status::Paused);
    queue.resume(&id).unwrap();
    let run = queue.claim(Stage::Transcribe).unwrap();
    fs::remove_file(&run.snapshot.media).unwrap();
    let runtime = voiceless::transcription_jobs::RuntimeRecognition::new(|| None, Default::default);
    worker::transcribe(&queue, run, &runtime, &PriorityResource::new());
    assert_eq!(
        queue.get(&id).unwrap().summary.error.unwrap().code,
        FailureCode::SourceUnavailable
    );
    queue
        .reselect(&id, upload(&queue, b"another file"))
        .unwrap();
    worker::import(&queue, queue.claim(Stage::Import).unwrap());
    assert_eq!(
        queue.get(&id).unwrap().summary.error.unwrap().code,
        FailureCode::SourceMismatch
    );
    queue.reselect(&id, upload(&queue, &audio)).unwrap();
    worker::import(&queue, queue.claim(Stage::Import).unwrap());
    let run = queue.claim(Stage::Transcribe).unwrap();
    assert_eq!(run.snapshot.committed_until, first.end);
    assert!(queue.fail(
        &run.lease,
        JobFailure {
            code: FailureCode::Recognize,
            message: "interrupted".into(),
            remedy: None
        }
    ));
    queue.retry(&id).unwrap();
    let retry = queue.claim(Stage::Transcribe).unwrap();
    assert_eq!(retry.snapshot.committed_until, first.end);
    queue.cancel(&id).unwrap();
    let second = Segment {
        start: 32_000,
        end: 48_000,
        text: "second".into(),
    };
    assert!(!queue.commit(&retry.lease, std::slice::from_ref(&second), second.end));
    assert!(!queue.complete(&retry.lease));
    assert_eq!(queue.get(&id).unwrap().summary.status, Status::Cancelled);
    queue.retry(&id).unwrap();
    let final_run = queue.claim(Stage::Transcribe).unwrap();
    assert!(queue.commit(&final_run.lease, std::slice::from_ref(&second), second.end));
    assert!(queue.complete(&final_run.lease));
    drop(queue);
    let queue = open(root.path());
    let view = queue.get(&id).unwrap();
    assert_eq!(view.summary.progress.fraction, Some(1.0));
    assert_eq!(view.segments, [first, second]);
    assert_eq!(
        queue.export(&id, ExportFormat::Srt).unwrap().content,
        "1\n00:00:00,050 --> 00:00:01,000\nfirst\n\n2\n00:00:02,000 --> 00:00:03,000\nsecond\n\n"
    );
}

#[test]
fn scheduling_live_hold_drains_one_unit_and_cloud_can_continue() {
    let resource = PriorityResource::new();
    let gate: &dyn SharedResource = &resource;
    let first = gate.unit(&|| false).unwrap();
    let live = resource.hold();
    assert!(!live.wait_for_background(Duration::ZERO));
    drop(first);
    assert!(live.wait_for_background(Duration::ZERO));
    let cloud = gate.unit_for(Transport::Cloud, &|| false).unwrap();
    let (entered, waiting) = mpsc::channel();
    let (acquired, done) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _local = gate
                .unit(&|| {
                    entered.send(()).unwrap();
                    false
                })
                .unwrap();
            acquired.send(()).unwrap();
        });
        // The abort callback runs under the gate lock before testing admission.
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
        drop(live);
        done.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    drop(cloud);
}

#[test]
fn scheduling_worker_yields_after_in_flight_decode_and_resumes_from_committed_progress() {
    use std::sync::Mutex;
    use voiceless::transcription_jobs::{Recognition, SegmentRecognizer};

    struct Decoder {
        entered: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
        position: u64,
    }

    impl SegmentRecognizer for Decoder {
        fn push(&mut self, samples: &[f32]) -> Result<Vec<Segment>, JobFailure> {
            let start = self.position;
            if start == 0 {
                self.entered.send(()).unwrap();
                self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            self.position += samples.len() as u64;
            Ok(vec![Segment {
                start,
                end: self.position,
                text: start.to_string(),
            }])
        }

        fn finish(&mut self) -> Result<Vec<Segment>, JobFailure> {
            Ok(Vec::new())
        }
    }

    struct OneRun(Mutex<Option<Decoder>>);
    impl Recognition for OneRun {
        fn open(
            &self,
            _: &RecognitionRequest,
            origin: u64,
        ) -> Result<Box<dyn SegmentRecognizer>, JobFailure> {
            assert_eq!(origin, 0);
            Ok(Box::new(self.0.lock().unwrap().take().unwrap()))
        }
    }

    let root = tempdir().unwrap();
    let queue = open(root.path());
    let job = submit(
        &queue,
        "long.wav",
        upload(&queue, &wav()),
        Transport::Local,
        false,
    );
    worker::import(&queue, queue.claim(Stage::Import).unwrap());
    let run = queue.claim(Stage::Transcribe).unwrap();
    let resource = PriorityResource::new();
    let (entered, decoding) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let recognizer = OneRun(Mutex::new(Some(Decoder {
        entered,
        resume,
        position: 0,
    })));
    std::thread::scope(|scope| {
        scope.spawn(|| worker::transcribe(&queue, run, &recognizer, &resource));
        decoding.recv_timeout(Duration::from_secs(5)).unwrap();
        let live = resource.hold();
        assert!(!live.wait_for_background(Duration::ZERO));
        release.send(()).unwrap();
        assert!(live.wait_for_background(Duration::from_secs(5)));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let view = queue.get(&job).unwrap();
            if view.summary.activity == Some(Activity::WaitingForResource) {
                assert_eq!(view.segments.len(), 1);
                assert_eq!(view.segments[0].end, 16_000);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not report the live hold"
            );
            std::thread::yield_now();
        }
        drop(live);
    });
    let finished = queue.get(&job).unwrap();
    assert_eq!(finished.summary.status, Status::Completed);
    assert_eq!(
        finished.segments.last().unwrap().end,
        SPEECH.len() as u64 / 2
    );
    assert!(finished
        .segments
        .windows(2)
        .all(|pair| pair[0].end == pair[1].start));
}
