#![cfg(target_os = "linux")]

use super::script;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use tokio::time::{sleep, timeout};
use voiceless::{
    meeting_capture::{
        start, Failure, Limits, MeetingError, MeetingRuntime, MeetingService, MeetingState,
        Selection, Tools, TrackKind,
    },
    transcription_jobs::{JobQueue, QueueOptions, Transport},
};

/// What a stand-in recorder streams for a track before it idles: eight
/// 1 KiB chunks of one byte, which read as samples of `0x0101`.
const CHUNKS: usize = 8;
const CHUNK: usize = 1024;
/// What a track keeps back until the recorder is gone, so that the lines
/// `pw-record` may print after its last sample are never kept as audio
/// (`capture::recorder::OUTPUT_TAIL`).
const HELD: u64 = 512;
/// What a track commits while that streams: everything but what it keeps back.
const COMMITTED: u64 = CHUNKS as u64 * CHUNK as u64 - HELD;
const PIPEWIRE: &str = "install PipeWire (the pipewire-bin package on Debian and Ubuntu)";
const TOOLS: &str =
    "install the GStreamer tools (the gstreamer1.0-tools package on Debian and Ubuntu)";
const PLUGIN: &str =
    "install GStreamer's PipeWire plugin (the gstreamer1.0-pipewire package on Debian and Ubuntu)";

/// The `--help` of `pw-record` on Ubuntu 24.04.
const PROPERTIES: &str = "  -P  --properties                      Set node properties";

fn runtime(dir: &Path) -> MeetingRuntime {
    MeetingRuntime::new(dir)
        .with_tools(Tools {
            dump: dir.join("dump"),
            record: dir.join("record"),
        })
        .with_limits(Limits {
            commit_bytes: 4,
            ..Limits::default()
        })
}

/// The graph of a desktop with a microphone, an output and this process's
/// two meeting streams, each fed by the node it asked for.
fn graph(microphone_feeds: u64, system_feeds: u64) -> String {
    format!(
        r##"[{{"id":1,"type":"PipeWire:Interface:Node","info":{{"props":{{"node.name":"mic","media.class":"Audio/Source","object.serial":31}}}}}},{{"id":2,"type":"PipeWire:Interface:Node","info":{{"props":{{"node.name":"sink","media.class":"Audio/Sink","object.serial":42}}}}}},
{{"id":3,"type":"PipeWire:Interface:Node","info":{{"props":{{"node.name":"voiceless-meeting-$PPID-microphone"}}}}}},{{"id":4,"type":"PipeWire:Interface:Node","info":{{"props":{{"node.name":"voiceless-meeting-$PPID-system"}}}}}},
{{"type":"PipeWire:Interface:Link","info":{{"input-node-id":3,"output-node-id":{microphone_feeds},"state":"active"}}}},{{"type":"PipeWire:Interface:Link","info":{{"input-node-id":4,"output-node-id":{system_feeds},"state":"active"}}}}]
"##
    )
}

/// The same graph with nothing feeding the microphone's stream: its
/// source left it.
fn graph_without_microphone_link() -> String {
    graph(1, 2).replace(
        "{\"type\":\"PipeWire:Interface:Link\",\"info\":{\"input-node-id\":3,\"output-node-id\":1,\"state\":\"active\"}},",
        "",
    )
}

fn setup(dir: &Path) {
    pcm(dir, CHUNKS * CHUNK);
    script(
        dir.join("dump"),
        &format!("#!/bin/sh\ncat <<GRAPH\n{}\nGRAPH\n", graph(1, 2)),
    );
    recording(dir, PROPERTIES);
}

/// The audio the stand-ins stream: one byte per track, for `bytes` bytes.
fn pcm(dir: &Path, bytes: usize) {
    fs::write(dir.join("microphone.pcm"), vec![1_u8; bytes]).unwrap();
    fs::write(dir.join("system.pcm"), vec![2_u8; bytes]).unwrap();
}

/// A stand-in for `pw-record` whose help is `help`, and which streams each
/// track's audio and then idles until the SIGINT that ends a meeting.
fn recording(dir: &Path, help: &str) -> PathBuf {
    recorder(
        dir,
        help,
        &format!(
            r##"trap 'exit 0' INT
case "$*" in
    *"stream.capture.sink=true "*) cat '{dir}/system.pcm' ;;
    *"stream.capture.sink=false "*) cat '{dir}/microphone.pcm' ;;
    *) echo "unexpected recorder arguments: $*" >&2; exit 2 ;;
esac
while :; do sleep 0.05; done
"##,
            dir = dir.display()
        ),
    )
}

/// A `pw-dump` stand-in that notes `verified` from its second read on: the
/// first reads the graph to select the nodes, the ones after it verify the
/// link. A recorder that is to end by itself waits for it, so that it ends
/// once its stream was verified, as one does whose source leaves.
fn notes_verification(dir: &Path) {
    script(
        dir.join("dump"),
        &format!(
            "#!/bin/sh\nn=$(cat '{dir}/reads' 2>/dev/null || echo 0)\necho $((n + 1)) > '{dir}/reads'\n\
             [ \"$n\" -ge 1 ] && : > '{dir}/verified'\ncat <<GRAPH\n{}\nGRAPH\n",
            graph(1, 2),
            dir = dir.display()
        ),
    );
}

/// A stand-in for `pw-record`: it answers `--help` with `help` and does
/// `body` to record.
fn recorder(dir: &Path, help: &str, body: &str) -> PathBuf {
    script(
        dir.join("record"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in --help) printf '%s\\n' '{help}'; exit 0;; esac\n{body}\n"
        ),
    )
}

fn selection() -> Selection {
    Selection {
        microphone: "mic".into(),
        system: Some("sink".into()),
    }
}
fn queue(dir: &Path) -> Arc<JobQueue> {
    Arc::new(JobQueue::open(dir, QueueOptions::default()).unwrap())
}
const MODEL: &str = "sense-voice-int8";

async fn wait_committed(owner: &MeetingService, id: &str) {
    timeout(Duration::from_secs(5), async {
        while owner
            .get(id)
            .await
            .unwrap()
            .tracks
            .iter()
            .map(|t| t.committed_bytes)
            .collect::<Vec<_>>()
            != [COMMITTED; 2]
        {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// The audio of a track's WAV: `sample` all through, and nothing else.
fn track_audio(path: &Option<PathBuf>, sample: u8, bytes: usize) {
    let path = path.as_ref().expect("the track has audio");
    assert_eq!(
        &fs::read(path).unwrap()[44..],
        vec![sample; bytes],
        "{}",
        path.display()
    );
}

#[tokio::test]
async fn meeting_capture_dual_track_reaps_without_a_client_and_links_local_jobs_once() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    let jobs = queue(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let owner = start(runtime(dir.path()), jobs.clone(), MODEL.into(), move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let id = owner.record(selection()).await.unwrap().id;
    wait_committed(&owner, &id).await;
    assert!(owner.stop("other-meeting").await.is_err());
    owner.stop(&id).await.unwrap();
    timeout(Duration::from_secs(5), async {
        while let Some(e) = rx.recv().await {
            if e["state"] == "completed" {
                assert_eq!(e["meeting_id"], id);
                return;
            }
        }
        panic!("capture service exited before handoff");
    })
    .await
    .unwrap();
    let tracks = owner.retry(&id).await.unwrap().tracks;
    assert_eq!(jobs.list().len(), 2);
    // The end that was kept back joined the audio when the recorders were gone.
    for (track, sample) in tracks.iter().zip([1, 2]) {
        let job = jobs.get(track.job_id.as_ref().unwrap()).unwrap();
        assert_eq!(job.summary.transport, Transport::Local);
        assert_eq!(job.summary.model, MODEL);
        track_audio(&track.path, sample, CHUNKS * CHUNK);
    }
    // Recreate the crash window after the queue has committed and before the journal has its ID.
    drop(owner);
    let journal = dir.path().join("meetings").join(&id).join("meeting.json");
    let mut doc: serde_json::Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    doc["tracks"][1]["job_id"] = serde_json::Value::Null;
    fs::write(journal, serde_json::to_vec(&doc).unwrap()).unwrap();
    let restarted = start(runtime(dir.path()), jobs.clone(), MODEL.into(), |_| {}).unwrap();
    assert_eq!(restarted.get(&id).await.unwrap().tracks, tracks);
    assert_eq!(jobs.list().len(), 2);
    // Nothing of the recorders is left: their logs are gone with the tracks.
    assert!(!dir
        .path()
        .join("meetings")
        .join(&id)
        .join("microphone.log")
        .exists());
}

#[tokio::test]
async fn meeting_capture_crash_child() {
    let Some(path) = std::env::var_os("VOICELESS_TEST_CRASH_MEETING_DIR") else {
        return;
    };
    let mut rt = runtime(Path::new(&path));
    let id = rt.start(selection()).await.unwrap().id;
    timeout(Duration::from_secs(5), async {
        while rt
            .meeting(&id)
            .unwrap()
            .tracks
            .iter()
            .any(|t| t.committed_bytes != COMMITTED)
        {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Skip all destructors, as with a killed daemon.
    std::process::exit(73);
}

#[tokio::test]
async fn meeting_capture_restart_requires_recovery_before_submission() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "meeting_capture::meeting_capture_crash_child"])
        .env("VOICELESS_TEST_CRASH_MEETING_DIR", dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let jobs = queue(dir.path());
    let owner = start(runtime(dir.path()), jobs.clone(), MODEL.into(), |_| {}).unwrap();
    let records = runtime(dir.path()).recoveries();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].failure, Some(Failure::ProcessInterrupted));
    assert!(jobs.list().is_empty());
    let recovered = owner.recover(&records[0].id).await.unwrap();
    assert_eq!(recovered.state, MeetingState::Recovered);
    assert!(recovered.tracks.iter().all(|track| track.job_id.is_some()));
    owner.recover(&recovered.id).await.unwrap();
    assert_eq!(jobs.list().len(), 2);
}

#[tokio::test]
async fn meeting_capture_denied_source_is_identified_and_can_be_discarded() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    recorder(
        dir.path(),
        PROPERTIES,
        "echo 'error: failed to create stream: Permission denied' >&2\nexit 1\n",
    );
    let jobs = queue(dir.path());
    let mut rt = runtime(dir.path());
    let missing = rt
        .start(Selection {
            microphone: "gone".into(),
            system: None,
        })
        .await
        .unwrap_err();
    assert_eq!(missing.code(), "source-missing");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let owner = start(rt, jobs.clone(), MODEL.into(), move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let id = owner
        .record(Selection {
            microphone: "mic".into(),
            system: None,
        })
        .await
        .unwrap()
        .id;
    timeout(Duration::from_secs(5), async {
        loop {
            if rx.recv().await.unwrap()["state"] == "interrupted" {
                break;
            }
        }
    })
    .await
    .unwrap();
    let failure = owner.get(&id).await.unwrap().failure.unwrap();
    assert!(
        matches!(&failure, Failure::PermissionDenied { node, reason, remedy, .. } if node == "mic" && reason.contains("Permission denied") && !remedy.is_empty()),
        "{failure}"
    );
    assert_eq!(
        owner.get(&id).await.unwrap().tracks[0].committed_bytes,
        0,
        "a stream that was not permitted keeps no sample"
    );
    owner.discard(&id).await.unwrap();
    assert!(owner.get(&id).await.is_err());
    assert!(jobs.list().is_empty());
}

#[tokio::test]
async fn meeting_capture_wrong_link_keeps_zero_samples() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    // The session manager fell back: the microphone's stream is fed by the output.
    script(
        dir.path().join("dump"),
        &format!("#!/bin/sh\ncat <<GRAPH\n{}\nGRAPH\n", graph(2, 2)),
    );
    let mut rt = runtime(dir.path());
    rt.start(Selection {
        microphone: "mic".into(),
        system: None,
    })
    .await
    .unwrap();
    let ended = timeout(Duration::from_secs(5), async {
        loop {
            rt.changed().await;
            if let Some(meeting) = rt.poll() {
                break meeting;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(
            &ended.failure,
            Some(Failure::StreamUnconnected { node, reason, .. })
                if node == "mic" && reason.contains("requested \"mic\" (serial 31)") && reason.contains("linked to node 2")
        ),
        "{:?}",
        ended.failure
    );
    assert_eq!(ended.tracks[0].committed_bytes, 0);
    assert!(rt.recover(&ended.id).unwrap().tracks[0].path.is_none());
}

/// `pw-record` on PipeWire before 1.4 prints why its stream ended on
/// stdout, after its last sample: why, and then that the stream is gone.
/// The audio before those lines is the track's and the first is the reason
/// the track ended, wherever a read cut them; a sample their coming cut in
/// two is dropped, not held against the recorder.
#[tokio::test]
async fn meeting_capture_keeps_the_audio_before_the_line_pw_record_prints_and_uses_the_line() {
    // 8 KiB leaves the lines inside one read; 8 KiB + 1 is a sample the
    // stream's end cut in two; 32 KiB - 4 has a read end four bytes into
    // the lines.
    for bytes in [8 * 1024, 8 * 1024 + 1, 32 * 1024 - 4] {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path());
        pcm(dir.path(), bytes);
        notes_verification(dir.path());
        recorder(
            dir.path(),
            PROPERTIES,
            &format!(
                "while [ ! -e '{dir}/verified' ]; do sleep 0.01; done\n\
                 cat '{dir}/microphone.pcm'\nprintf 'stream node 41 error: target not found\\nstream node 4294967295 unconnected\\n'\nexit 1\n",
                dir = dir.path().display()
            ),
        );
        let mut rt = runtime(dir.path());
        rt.start(Selection {
            microphone: "mic".into(),
            system: None,
        })
        .await
        .unwrap();
        let ended = timeout(Duration::from_secs(5), async {
            loop {
                rt.changed().await;
                if let Some(meeting) = rt.poll() {
                    break meeting;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            ended.failure,
            Some(Failure::StreamEnded {
                source: TrackKind::Microphone,
                node: "mic".into(),
                reason: "stream node 41 error: target not found".into(),
            }),
            "{bytes} bytes"
        );
        let track = &rt.recover(&ended.id).unwrap().tracks[0];
        let kept = (bytes & !1) as u64;
        assert_eq!(track.committed_bytes, kept, "{bytes} bytes");
        track_audio(&track.path, 1, kept as usize);
    }
}

/// Where `pw-record` has no `--properties` (Ubuntu 22.04), `gst-launch-1.0`
/// records, found next to the `pw-record` a test names, and is told the
/// same things in GStreamer's words.
#[tokio::test]
async fn meeting_capture_records_through_gstreamer_where_pw_record_has_no_properties() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    recorder(
        dir.path(),
        "      --list-targets                    List available targets",
        "exit 2\n",
    );
    script(dir.path().join("gst-inspect-1.0"), "#!/bin/sh\nexit 0\n");
    script(
        dir.path().join("gst-launch-1.0"),
        &format!(
            r##"#!/bin/sh
printf 'PIPEWIRE_NODE=%s|%s\n' "${{PIPEWIRE_NODE:-}}" "$*" >> '{dir}/argv'
trap 'exit 0' INT
case "$*" in
    *"stream.capture.sink=true "*) cat '{dir}/system.pcm' ;;
    *"stream.capture.sink=false "*) cat '{dir}/microphone.pcm' ;;
    *) echo "unexpected recorder arguments: $*" >&2; exit 2 ;;
esac
while :; do sleep 0.05; done
"##,
            dir = dir.path().display()
        ),
    );
    let jobs = queue(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let owner = start(runtime(dir.path()), jobs.clone(), MODEL.into(), move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let id = owner.record(selection()).await.unwrap().id;
    wait_committed(&owner, &id).await;
    owner.stop(&id).await.unwrap();
    timeout(Duration::from_secs(5), async {
        while let Some(e) = rx.recv().await {
            if e["state"] == "completed" {
                return;
            }
        }
        panic!("capture service exited before handoff");
    })
    .await
    .unwrap();
    let mut lines = fs::read_to_string(dir.path().join("argv"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort();
    let pipeline = "! audio/x-raw,format=S16LE,rate=16000,channels=1 ! fdsink fd=1";
    assert_eq!(lines, [
        format!("PIPEWIRE_NODE=mic|-q -e pipewiresrc client-name=voiceless-meeting-{}-microphone stream-properties=props,node.dont-reconnect=true,node.dont-fallback=true,stream.capture.sink=false {pipeline}", std::process::id()),
        format!("PIPEWIRE_NODE=sink|-q -e pipewiresrc client-name=voiceless-meeting-{}-system stream-properties=props,node.dont-reconnect=true,node.dont-fallback=true,stream.capture.sink=true {pipeline}", std::process::id()),
    ]);
    for (track, sample) in owner.retry(&id).await.unwrap().tracks.iter().zip([1, 2]) {
        track_audio(&track.path, sample, CHUNKS * CHUNK);
    }
    // No `--raw` was there to give: the stand-in's help has none, and nothing else was asked of it.
    assert!(!fs::read_to_string(dir.path().join("argv"))
        .unwrap()
        .contains("--raw"));
}

/// What a meeting names when it cannot record: the program that is
/// missing, with the package that installs it. The programs a test names
/// are taken where they are, and what records with them is looked for next
/// to them, never elsewhere on `PATH`; nothing is left on disk.
#[tokio::test]
async fn meeting_capture_names_a_missing_recorder_and_its_package() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    /// What a meeting says when `tools` cannot record.
    async fn refused(dir: &Path, tools: Tools) -> (Failure, String) {
        let error = MeetingRuntime::new(dir)
            .with_tools(tools)
            .start(selection())
            .await
            .unwrap_err();
        let MeetingError::Failure(failure) = &error else {
            panic!("{error}")
        };
        (failure.clone(), error.to_string())
    }
    let at = |name: &str| dir.path().join(name);

    // `pw-record` itself.
    let (failure, _) = refused(
        dir.path(),
        Tools {
            dump: at("dump"),
            record: at("absent"),
        },
    )
    .await;
    assert!(
        matches!(&failure, Failure::NoRecorder { source: TrackKind::Microphone, reason } if reason == &format!("{} is absent; {PIPEWIRE}", at("absent").display())),
        "{failure}"
    );
    // The graph, before any recorder is looked for.
    let (failure, _) = refused(
        dir.path(),
        Tools {
            dump: at("no-dump"),
            record: at("record"),
        },
    )
    .await;
    assert!(
        matches!(&failure, Failure::GraphUnreadable { reason } if reason == &format!("{} is absent; {PIPEWIRE}", at("no-dump").display())),
        "{failure}"
    );
    // A `pw-record` without `--properties` records through GStreamer.
    recorder(
        dir.path(),
        "      --list-targets                    List available targets",
        "exit 2\n",
    );
    let (failure, _) = refused(
        dir.path(),
        Tools {
            dump: at("dump"),
            record: at("record"),
        },
    )
    .await;
    assert!(
        matches!(&failure, Failure::NoRecorder { reason, .. } if reason == &format!("{} is absent; {TOOLS}", at("gst-launch-1.0").display())),
        "{failure}"
    );
    script(at("gst-launch-1.0"), "#!/bin/sh\nexit 2\n");
    let (failure, _) = refused(
        dir.path(),
        Tools {
            dump: at("dump"),
            record: at("record"),
        },
    )
    .await;
    assert!(
        matches!(&failure, Failure::NoRecorder { reason, .. } if reason == &format!("{} is absent; {TOOLS}", at("gst-inspect-1.0").display())),
        "{failure}"
    );
    // The tools alone do not record: the plugin has to be there.
    script(
        at("gst-inspect-1.0"),
        "#!/bin/sh\necho \"No such element or plugin 'pipewiresrc'\" >&2\nexit 255\n",
    );
    let (failure, shown) = refused(
        dir.path(),
        Tools {
            dump: at("dump"),
            record: at("record"),
        },
    )
    .await;
    assert!(
        matches!(&failure, Failure::NoRecorder { reason, .. } if reason == &format!("{} has no pipewiresrc element: No such element or plugin 'pipewiresrc'; {PLUGIN}", at("gst-inspect-1.0").display())),
        "{failure}"
    );
    assert!(shown.contains("cannot start the recorder"), "{shown}");
    assert!(!dir.path().join("meetings").exists());
    // With the plugin there, the same tools record.
    script(at("gst-inspect-1.0"), "#!/bin/sh\nexit 0\n");
    script(
        at("gst-launch-1.0"),
        &format!(
            "#!/bin/sh\ntrap 'exit 0' INT\ncat '{}'\nwhile :; do sleep 0.05; done\n",
            at("microphone.pcm").display()
        ),
    );
    let mut rt = runtime(dir.path());
    let id = rt.start(selection()).await.unwrap().id;
    timeout(Duration::from_secs(5), async {
        while rt
            .meeting(&id)
            .unwrap()
            .tracks
            .iter()
            .any(|t| t.committed_bytes != COMMITTED)
        {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    rt.stop().unwrap();
    let ended = timeout(Duration::from_secs(5), async {
        loop {
            rt.changed().await;
            if let Some(meeting) = rt.poll() {
                break meeting;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ended.state, MeetingState::Completed, "{:?}", ended.failure);
    // Both tracks read the stand-in's one audio file.
    for track in &ended.tracks {
        track_audio(&track.path, 1, CHUNKS * CHUNK);
    }
}

/// `gst-launch-1.0` keeps running with no audio once its source is gone:
/// the graph is what tells. Gone from it, the source counts as lost: that
/// track ends and the meeting with it, and both keep their audio.
#[tokio::test]
async fn meeting_capture_ends_a_gstreamer_track_whose_source_left_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    recorder(
        dir.path(),
        "      --list-targets                    List available targets for --target",
        "exit 2\n",
    );
    script(dir.path().join("gst-inspect-1.0"), "#!/bin/sh\nexit 0\n");
    script(
        dir.path().join("dump"),
        &format!(
            "#!/bin/sh\nif [ -e '{dir}/unlinked' ]; then\n{left}\nelse\n{there}\nfi\n",
            dir = dir.path().display(),
            left = format_args!("cat <<GRAPH\n{}\nGRAPH\n", graph_without_microphone_link()),
            there = format_args!("cat <<GRAPH\n{}\nGRAPH\n", graph(1, 2)),
        ),
    );
    // Both recorders stream their track's audio, then are silent without
    // ending once `unlinked` is there, as `pipewiresrc` is once its source left.
    script(
        dir.path().join("gst-launch-1.0"),
        &format!(
            r##"#!/bin/sh
trap 'exit 0' INT
case "$*" in
    *"stream.capture.sink=true "*) cat '{dir}/system.pcm' ;;
    *"stream.capture.sink=false "*) cat '{dir}/microphone.pcm' ;;
esac
while [ ! -e '{dir}/unlinked' ]; do sleep 0.02; done
while :; do sleep 0.05; done
"##,
            dir = dir.path().display()
        ),
    );
    let mut rt = runtime(dir.path());
    rt.start(selection()).await.unwrap();
    timeout(Duration::from_secs(5), async {
        while rt.active_id().is_some_and(|id| {
            rt.meeting(id)
                .unwrap()
                .tracks
                .iter()
                .any(|track| track.committed_bytes != COMMITTED)
        }) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The microphone leaves the graph; only its link is gone from it.
    fs::write(dir.path().join("unlinked"), b"").unwrap();
    let ended = timeout(Duration::from_secs(10), async {
        loop {
            rt.changed().await;
            if let Some(meeting) = rt.poll() {
                break meeting;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the meeting did not end: {:?}",
            rt.active_id().map(|id| rt.meeting(id))
        )
    });
    assert!(
        matches!(
            &ended.failure,
            Some(Failure::StreamEnded { source: TrackKind::Microphone, node, reason })
                if node == "mic" && reason.contains("voiceless-meeting-") && reason.contains("no longer links")
        ),
        "{:?}",
        ended.failure
    );
    let recovered = rt.recover(&ended.id).unwrap();
    for (track, sample) in recovered.tracks.iter().zip([1, 2]) {
        assert_eq!(
            track.status,
            voiceless::meeting_capture::TrackStatus::Finalized
        );
        track_audio(&track.path, sample, CHUNKS * CHUNK);
    }
}
