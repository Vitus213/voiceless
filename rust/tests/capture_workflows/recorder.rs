#![cfg(target_os = "linux")]

//! The dictation capture (`capture::RawCapture`) on the shared recorder,
//! with stand-ins for `pw-record` and `pw-dump` first on `PATH`: which
//! recorder runs, what it is told, what is kept, and what is said when its
//! stream cannot be verified or ends on its own.
//!
//! The stand-ins of one scenario live in a directory of their own that is
//! the whole `PATH` of the scenario, so that no real PipeWire tool can be
//! reached; the scenario runs in a child process, which is this test binary
//! told what to do by `VOICELESS_TEST_RECORDER`.

use super::script;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tokio::time::{sleep, timeout};
use voiceless::capture::{RawCapture, CHUNK_BYTES, SAMPLE_RATE};

/// The `--help` of `pw-record` on Ubuntu 24.04.
const PROPERTIES: &str = "  -P  --properties                      Set node properties";
/// What PipeWire 1.4 and later add to it.
const RAW: &str = "  -a, --raw                             RAW mode";
/// What `pw-record` on PipeWire before 1.4 prints after its last sample
/// when its stream ends on its own.
const LOST: &str = "stream node 7 unconnected";
/// The help of a `pw-record` without `--properties`, as on Ubuntu 22.04.
const NO_PROPERTIES: &str =
    "      --list-targets                    List available targets for --target";
const PIPEWIRE: &str =
    "pw-record is absent from PATH; install PipeWire (the pipewire-bin package on Debian and Ubuntu)";
const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The stand-ins of one scenario, in `<root>/bin`.
struct Tools {
    dir: PathBuf,
}

impl Tools {
    fn new(root: &Path) -> Self {
        let dir = root.join("bin");
        fs::create_dir_all(&dir).unwrap();
        // The stand-ins are shell scripts, and find the few tools they use
        // next to themselves.
        for tool in ["cat", "sed", "sleep", "touch"] {
            let found = std::env::var_os("PATH")
                .and_then(|paths| {
                    std::env::split_paths(&paths)
                        .map(|directory| directory.join(tool))
                        .find(|candidate| candidate.is_file())
                })
                .unwrap_or_else(|| panic!("{tool} is on PATH"));
            fs::remove_file(dir.join(tool)).ok();
            std::os::unix::fs::symlink(found, dir.join(tool)).unwrap();
        }
        // Two chunks of one sample.
        fs::write(dir.join("audio.pcm"), vec![1_u8; 2 * CHUNK_BYTES]).unwrap();
        Self { dir }
    }

    /// A `pw-dump` whose graph has `mic`, a `decoy`, and the process that
    /// runs it's dictation stream, fed by node `feeds`: 1 is `mic`, 2 the
    /// decoy. From its second read on it notes `verified`: the first read
    /// selects the microphone, the ones after it verify the link. Once
    /// `unlinked` is there the link is gone from the graph: the source left.
    fn graph(self, feeds: u64) -> Self {
        script(
            self.dir.join("pw-dump"),
            &format!(
                r##"#!/bin/sh
n=$(cat '{dir}/reads' 2>/dev/null || echo 0)
echo $((n + 1)) > '{dir}/reads'
[ "$n" -ge 1 ] && : > '{dir}/verified'
if [ -e '{dir}/unlinked' ]; then
cat <<GRAPH | sed 's/,{{"type":"PipeWire:Interface:Link".*\]$/]/'
{graph}
GRAPH
else
cat <<GRAPH
{graph}
GRAPH
fi
"##,
                graph = format!(
                    "[{{\"id\":1,\"type\":\"PipeWire:Interface:Node\",\"info\":{{\"props\":{{\"node.name\":\"mic\",\"media.class\":\"Audio/Source\",\"object.serial\":31}}}}}},\
                     {{\"id\":2,\"type\":\"PipeWire:Interface:Node\",\"info\":{{\"props\":{{\"node.name\":\"decoy\",\"media.class\":\"Audio/Source\",\"object.serial\":99}}}}}},\
                     {{\"id\":3,\"type\":\"PipeWire:Interface:Node\",\"info\":{{\"props\":{{\"node.name\":\"voiceless-capture-$PPID\",\"media.class\":\"Stream/Input/Audio\"}}}}}},\
                     {{\"type\":\"PipeWire:Interface:Link\",\"info\":{{\"output-node-id\":{feeds},\"input-node-id\":3,\"state\":\"active\"}}}}]"
                ),
                dir = self.dir.display()
            ),
        );
        self
    }

    /// A `pw-dump` that notes it was asked, and has no graph to give.
    fn no_graph(self) -> Self {
        script(
            self.dir.join("pw-dump"),
            &format!(
                "#!/bin/sh\ntouch '{}'\necho \"can't connect: Host is down\" >&2\nexit 1\n",
                self.dir.join("dumped").display()
            ),
        );
        self
    }

    /// A `pw-record` whose help is `help` and which does `body` to record,
    /// after noting its arguments and its pid.
    fn recorder(self, help: &str, body: &str) -> Self {
        let dir = self.dir.display();
        script(
            self.dir.join("pw-record"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in --help) printf '%s\\n' '{help}'; exit 0;; esac\n\
                 printf '%s\\n' \"$@\" > '{dir}/argv'\nprintf '%s\\n' \"$$\" > '{dir}/pid'\n{body}\n"
            ),
        );
        self
    }

    /// What streams `audio.pcm` and, on the SIGINT that ends a capture,
    /// prints two more bytes and ends as `pw-record` does.
    fn streaming(self) -> Self {
        let audio = self.dir.join("audio.pcm");
        self.recorder(
            &format!("{PROPERTIES}\n{RAW}"),
            &format!(
                "trap 'printf \"\\007\\010\"; exit 1' INT\ncat '{}'\nwhile :; do sleep 0.02; done\n",
                audio.display()
            ),
        )
    }

    /// GStreamer's recorder, for a `pw-record` without `--properties`: it
    /// streams like the other one and, once `unlinked` is there, is silent
    /// without ending, as `pipewiresrc` is once its source left.
    fn gstreamer(self) -> Self {
        let tools = self.recorder(NO_PROPERTIES, "exit 2\n");
        script(tools.dir.join("gst-inspect-1.0"), "#!/bin/sh\nexit 0\n");
        let audio = tools.dir.join("audio.pcm");
        let dir = tools.dir.display();
        script(
            tools.dir.join("gst-launch-1.0"),
            &format!(
                "#!/bin/sh\ntrap 'printf \"\\007\\010\"; exit 1' INT\nprintf '%s\\n' \"$@\" > '{dir}/argv'\n\
                 printf '%s\\n' \"$$\" > '{dir}/pid'\ncat '{}'\n\
                 while [ ! -e '{dir}/unlinked' ]; do sleep 0.02; done\nwhile :; do sleep 0.05; done\n",
                audio.display()
            ),
        );
        tools
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join(name)).unwrap()
    }

    /// The arguments the recorder was given, one per line.
    fn argv(&self) -> Vec<String> {
        self.read("argv").lines().map(str::to_owned).collect()
    }

    /// The recorder's pid, once it noted it.
    fn recorder_pid(&self) -> Option<i32> {
        fs::read_to_string(self.dir.join("pid"))
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
    }
}

/// This test binary running scenario `name`, with nothing on `PATH` but
/// the stand-ins of `root`.
fn spawned(root: &Path, name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "recorder::dictation_recorder_scenario",
            "--nocapture",
        ])
        .env("VOICELESS_TEST_RECORDER", name)
        .env("VOICELESS_TEST_RECORDER_DIR", root)
        .env("PATH", root.join("bin"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .stdin(Stdio::null());
    command
}

/// Scenario `name`, run to its end.
fn scenario(root: &Path, name: &str) -> Output {
    spawned(root, name).output().unwrap()
}

fn said(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn passed(output: &Output) {
    assert!(
        output.status.success(),
        "the scenario failed:\n{}",
        said(output)
    );
}

/// The pid of the process that ran a scenario: its recorders are named
/// after it.
fn owner(output: &Output) -> u32 {
    said(output)
        .lines()
        .find_map(|line| line.strip_prefix("owner ")?.parse().ok())
        .expect("the scenario says who it is")
}
/// Whether a process still exists.
fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The recorder's pid once it has noted it.
async fn started(tools: &Tools) -> i32 {
    timeout(WAIT, async {
        loop {
            if let Some(pid) = tools.recorder_pid() {
                return pid;
            }
            sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

fn properties(pid: u32) -> String {
    format!("node.name=voiceless-capture-{pid} node.dont-reconnect=true stream.capture.sink=false")
}

/// What every recorder is told after its stream properties: the format it records in.
fn format_arguments() -> Vec<String> {
    [
        "--rate".to_owned(),
        SAMPLE_RATE.to_string(),
        "--channels".to_owned(),
        "1".to_owned(),
        "--format".to_owned(),
        "s16".to_owned(),
        "--latency".to_owned(),
        "32ms".to_owned(),
        "-".to_owned(),
    ]
    .to_vec()
}

/// Why a capture could not start.
async fn refused(source: Option<&str>) -> std::io::Error {
    RawCapture::start(source)
        .await
        .err()
        .expect("the capture started")
}

/// The scenarios, each in a child process ([`scenario`]).
#[tokio::test]
async fn dictation_recorder_scenario() {
    let Some(name) = std::env::var_os("VOICELESS_TEST_RECORDER") else {
        return;
    };
    eprintln!("owner {}", std::process::id());
    let dir = std::env::var_os("VOICELESS_TEST_RECORDER_DIR").unwrap();
    let root = Path::new(&dir);
    match name.to_str().unwrap() {
        "selected" => {
            let mut capture = RawCapture::start(Some("mic")).await.unwrap();
            let mut audio = capture.next_chunk().await.unwrap().unwrap();
            audio.extend(capture.next_chunk().await.unwrap().unwrap());
            assert_eq!(audio, vec![1; 2 * CHUNK_BYTES]);
            let tail = capture.stop().await.unwrap();
            assert_eq!(tail, Some(vec![7, 8]));
        }
        // The owner goes away without stopping anything, as a killed daemon does.
        "held" => {
            let _capture = RawCapture::start(Some("mic")).await.unwrap();
            loop {
                sleep(std::time::Duration::from_secs(60)).await;
            }
        }
        "wrong-link" => {
            let error = refused(Some("mic")).await;
            eprintln!("{error}");
        }
        "denied" => {
            // The stream may be linked in the graph a moment before the
            // recorder reports the denial and ends: the denial is read
            // wherever it comes, at the start or with the first audio.
            let error = match RawCapture::start(Some("mic")).await {
                Err(error) => error,
                Ok(mut capture) => capture
                    .next_chunk()
                    .await
                    .expect_err("audio came from a stream that was not permitted"),
            };
            eprintln!("{error}|{:?}", error.kind());
        }
        "lost" => {
            let mut capture = RawCapture::start(Some("mic")).await.unwrap();
            let mut audio = capture.next_chunk().await.unwrap().unwrap();
            audio.extend(capture.next_chunk().await.unwrap().unwrap());
            eprintln!(
                "chunk {} {}",
                audio.len(),
                audio.iter().all(|byte| *byte == 1)
            );
            match capture.next_chunk().await {
                Err(error) => eprintln!("{error}|{:?}", error.kind()),
                Ok(_) => eprintln!("no error"),
            }
        }
        "default" => {
            let mut capture = RawCapture::start(None).await.unwrap();
            let mut audio = capture.next_chunk().await.unwrap().unwrap();
            audio.extend(capture.next_chunk().await.unwrap().unwrap());
            let tail = capture.stop().await.unwrap();
            eprintln!("{} {}", audio.len(), tail.unwrap().len());
        }
        selection @ ("lost-link" | "lost-default-link") => {
            let source = (selection == "lost-link").then_some("mic");
            let mut capture = RawCapture::start(source).await.unwrap();
            let mut audio = capture.next_chunk().await.unwrap().unwrap();
            audio.extend(capture.next_chunk().await.unwrap().unwrap());
            eprintln!(
                "chunk {} {}",
                audio.len(),
                audio.iter().all(|byte| *byte == 1)
            );
            // The source leaves the graph; the recorder does not end.
            fs::write(root.join("bin").join("unlinked"), b"").unwrap();
            let error = capture
                .next_chunk()
                .await
                .expect_err("audio kept coming from a source that left");
            eprintln!("{error}|{:?}", error.kind());
        }
        "gone" => {
            let error = refused(Some("gone")).await;
            eprintln!("{error}");
        }
        other => panic!("unknown scenario {other}"),
    }
}

#[tokio::test]
async fn dictation_capture_records_the_selected_microphone_and_keeps_the_tail() {
    let root = tempfile::tempdir().unwrap();
    let tools = Tools::new(root.path()).graph(1).streaming();
    let output = scenario(root.path(), "selected");
    passed(&output);
    // Pointed at the microphone's serial, named after its owner, never to
    // fall back, and told its format; `--raw`, because its help lists it.
    let mut expected = vec![
        "--target".to_owned(),
        "31".to_owned(),
        "--properties".to_owned(),
        properties(owner(&output)) + " node.dont-fallback=true",
        "--raw".to_owned(),
    ];
    expected.extend(format_arguments());
    assert_eq!(tools.argv(), expected);
    // The recorder was reaped with the capture.
    let pid: i32 = tools.read("pid").trim().parse().unwrap();
    assert!(!alive(pid), "the recorder {pid} outlived its capture");
}

#[tokio::test]
async fn dictation_recorder_is_a_group_of_its_own_that_ends_with_its_owner() {
    let root = tempfile::tempdir().unwrap();
    let tools = Tools::new(root.path()).graph(1).streaming();
    let mut owner = spawned(root.path(), "held")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let recorder = started(&tools).await;
    // A terminal's interrupt goes to the foreground process group. The
    // recorder is not in it, so it ends when its owner stops it, not before.
    let stat = fs::read_to_string(format!("/proc/{recorder}/stat")).unwrap();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    assert_eq!(fields[1].parse::<i32>().unwrap(), owner.id() as i32);
    assert_eq!(fields[2].parse::<i32>().unwrap(), recorder);
    // The owner dies without stopping anything: the recorder is told and
    // ends, so no microphone is left open.
    // SAFETY: the pid is the child this test spawned and has not reaped.
    assert_eq!(unsafe { libc::kill(owner.id() as i32, libc::SIGKILL) }, 0);
    owner.wait().unwrap();
    timeout(WAIT, async {
        while alive(recorder) {
            sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dictation_capture_refuses_a_stream_another_node_feeds_and_keeps_nothing() {
    let root = tempfile::tempdir().unwrap();
    Tools::new(root.path()).graph(2).streaming();
    let output = scenario(root.path(), "wrong-link");
    passed(&output);
    // The stream is the child's, so its name carries the child's pid.
    let said = said(&output);
    assert!(
        said.contains("requested \"mic\" (serial 31), \"voiceless-capture-")
            && said.contains("\" linked to node 2"),
        "{said}"
    );
}

#[tokio::test]
async fn dictation_capture_reports_a_denied_microphone_as_a_denial() {
    let root = tempfile::tempdir().unwrap();
    Tools::new(root.path()).graph(1).recorder(
        &format!("{PROPERTIES}\n{RAW}"),
        "echo 'error: failed to create stream: Permission denied' >&2\nexit 1\n",
    );
    let output = scenario(root.path(), "denied");
    passed(&output);
    // The recorder's own line is echoed as it comes; the capture's
    // complaint follows with where access is decided.
    let said = said(&output);
    assert!(
        said.contains(
            "error: failed to create stream: Permission denied; check the microphone privacy switch"
        ) && said.contains("|PermissionDenied"),
        "{said}"
    );
}

#[tokio::test]
async fn dictation_capture_names_why_the_microphone_left() {
    let root = tempfile::tempdir().unwrap();
    Tools::new(root.path()).graph(1).recorder(
        &format!("{PROPERTIES}\n{RAW}"),
        &format!(
            "while [ ! -e '{dir}/verified' ]; do sleep 0.01; done\n\
             cat '{dir}/audio.pcm'\nprintf '{LOST}\\n'\nexit 1\n",
            dir = root.path().join("bin").display()
        ),
    );
    let output = scenario(root.path(), "lost");
    passed(&output);
    // The audio before the line was audio; the line is the reason.
    assert!(
        said(&output).contains(&format!(
            "chunk {} true\nmicrophone disconnected or recorder stopped unexpectedly: {LOST}|BrokenPipe",
            2 * CHUNK_BYTES
        )),
        "{}",
        said(&output)
    );
}

#[tokio::test]
async fn dictation_capture_asks_the_default_of_nobody_and_reads_no_graph() {
    let root = tempfile::tempdir().unwrap();
    // No `--raw` in the help, so none on the command line.
    let tools = Tools::new(root.path()).no_graph().recorder(
        PROPERTIES,
        &format!(
            "trap 'printf \"\\007\\010\"; exit 1' INT\ncat '{}'\nwhile :; do sleep 0.02; done\n",
            root.path().join("bin").join("audio.pcm").display()
        ),
    );
    let output = scenario(root.path(), "default");
    passed(&output);
    assert!(
        said(&output).contains(&format!("{} 2", 2 * CHUNK_BYTES)),
        "{}",
        said(&output)
    );
    let mut expected = vec!["--properties".to_owned(), properties(owner(&output))];
    expected.extend(format_arguments());
    assert_eq!(tools.argv(), expected);
    assert!(!tools.dir.join("dumped").exists(), "the graph was read");
}

#[tokio::test]
async fn dictation_capture_names_a_microphone_that_left_and_a_recorder_that_is_absent() {
    let root = tempfile::tempdir().unwrap();
    Tools::new(root.path())
        .graph(1)
        .recorder(&format!("{PROPERTIES}\n{RAW}"), "exit 2\n");
    let output = scenario(root.path(), "gone");
    passed(&output);
    assert!(
        said(&output).contains(
            "selected microphone \"gone\" is not available; connect it or choose another microphone"
        ),
        "{}",
        said(&output)
    );
    // Nothing on `PATH` at all.
    let bare = tempfile::tempdir().unwrap();
    let output = scenario(bare.path(), "default");
    assert!(!output.status.success());
    assert!(said(&output).contains(PIPEWIRE), "{}", said(&output));
}

#[tokio::test]
async fn dictation_capture_ends_a_recorder_that_outlives_its_microphone() {
    let root = tempfile::tempdir().unwrap();
    let tools = Tools::new(root.path()).graph(1).gstreamer();
    let output = scenario(root.path(), "lost-link");
    passed(&output);
    let said = said(&output);
    assert!(
        said.contains(&format!(
            "chunk {} true\nmicrophone disconnected: the graph no longer links voiceless-capture-",
            2 * CHUNK_BYTES
        )) && said.contains("|BrokenPipe"),
        "{said}"
    );
    // The recorder was released with the capture.
    let pid: i32 = tools.read("pid").trim().parse().unwrap();
    assert!(!alive(pid), "the recorder {pid} outlived its capture");
    // It was GStreamer's recorder, told its stream in GStreamer's words.
    let argv = tools.argv();
    assert_eq!(
        argv[3],
        format!("client-name=voiceless-capture-{}", owner(&output))
    );
    assert!(argv
        .iter()
        .all(|arg| !arg.contains("--properties") && !arg.contains("--raw")));
}

#[tokio::test]
async fn dictation_reports_a_disconnected_system_default_on_gstreamer() {
    let root = tempfile::tempdir().unwrap();
    let tools = Tools::new(root.path()).graph(2).gstreamer();
    let output = scenario(root.path(), "lost-default-link");
    passed(&output);
    let diagnostic = said(&output);
    assert!(
        diagnostic.contains("microphone disconnected") && diagnostic.contains("|BrokenPipe"),
        "{diagnostic}"
    );
    assert!(!alive(tools.recorder_pid().unwrap()));
}
