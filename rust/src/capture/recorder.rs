//! The PipeWire recorder that the dictation capture (`super::RawCapture`)
//! and the meeting tracks (`meeting_capture::track`) both run.
//!
//! Programs. [`RecorderProfile`] names `pw-record` and `pw-dump` and says
//! which of two recorders `pw-record` is ([`RecorderBackend`]): `pw-record`
//! itself when its help lists `--properties` (PipeWire 0.3.52 onwards; from
//! 1.4 it also lists `--raw`, without which it writes a file header into
//! what it prints), else `gst-launch-1.0 pipewiresrc`, whose tools and
//! plugin then have to be installed too. Asking `pw-record --help`
//! connects to nothing. [`RecorderBackend::detect`] and
//! [`RecorderProfile::on_path`] block; [`RecorderProfile::detect`] is the
//! same off the runtime thread.
//!
//! Selection. A stream that names its target ([`Target::Node`]) is pointed
//! at that node's `object.serial`, read from a dump taken right before the
//! start, and asks `node.dont-reconnect`, with which the session manager
//! fails a target that left instead of linking the default device. That is
//! not trusted: [`RecorderProfile::verify`] waits for a dump to show an
//! `active` link into the stream that comes from that node and no other,
//! so a caller keeps no sample before it passed. [`selectable`] is what
//! may be a target: an `Audio/Source` node, or an `Audio/Sink` node
//! recorded through its monitor, that has an `object.serial`.
//!
//! End of a recorder. [`stopped_as_asked`] tells a stop from a failure,
//! [`Diagnostic`] says what the recorder reported, and [`split_output`]
//! finds the line `pw-record` prints after its last sample on PipeWire
//! before 1.4, with the audio.

use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::SAMPLE_RATE;

/// A tool that is there answers in a few milliseconds; one that hangs never does.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// A healthy session manager links a stream in well under a second.
const LINK_TIMEOUT: Duration = Duration::from_secs(5);
/// A present device may briefly have no active link while PipeWire relinks it.
const LINK_GRACE: Duration = Duration::from_secs(3);
/// How often a recording that watches its link looks at the graph.
pub(crate) const LINK_WATCH: Duration = Duration::from_secs(1);

const PW_RECORD: &str = "pw-record";
const GST_LAUNCH: &str = "gst-launch-1.0";
const GST_INSPECT: &str = "gst-inspect-1.0";
/// What installs the PipeWire tools, for a machine that lacks them.
const PIPEWIRE: &str = "PipeWire (the pipewire-bin package on Debian and Ubuntu)";
/// What installs `gst-launch-1.0` and `gst-inspect-1.0`.
const TOOLS: &str = "the GStreamer tools (the gstreamer1.0-tools package on Debian and Ubuntu)";
/// What installs the `pipewiresrc` element.
const PLUGIN: &str =
    "GStreamer's PipeWire plugin (the gstreamer1.0-pipewire package on Debian and Ubuntu)";
/// How much of the end of a `pw-record` output may be the lines it prints
/// after its last sample: `stream node `, ten digits, ` error: ` and a
/// reason, then the line saying the stream is gone.
pub(crate) const OUTPUT_TAIL: usize = 512;

// ---------------------------------------------------------------- programs

/// Which program records on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecorderBackend {
    /// `pw-record --properties … --target <serial>`. `raw`: its help lists
    /// `--raw` (PipeWire 1.4 onwards), which keeps it from writing a file
    /// header into its output.
    PwRecord { raw: bool },
    /// `gst-launch-1.0 pipewiresrc … fdsink`: where `pw-record` has no
    /// `--properties` (PipeWire before 0.3.52, Ubuntu 22.04 among them).
    Gstreamer,
}

impl RecorderBackend {
    /// Which recorder `record` is, from its `--help`. Blocks for at most
    /// [`ANSWER_TIMEOUT`]; fails when the help does not.
    pub(crate) fn detect(record: &Path) -> io::Result<Self> {
        let help = run(record, &["--help"])?;
        if !help.status.success() {
            return Err(io::Error::other(format!(
                "{} --help failed: {}",
                record.display(),
                complaint(&help)
            )));
        }
        let help = String::from_utf8_lossy(&help.stdout);
        Ok(if lists(&help, "--properties") {
            Self::PwRecord {
                raw: lists(&help, "--raw"),
            }
        } else {
            Self::Gstreamer
        })
    }
}

/// Whether a help text offers `option`, as a word of its own.
fn lists(help: &str, option: &str) -> bool {
    help.lines().any(|line| {
        line.split_whitespace()
            .any(|word| word.trim_matches(',') == option)
    })
}

/// The recorder programs of one use, each resolved to the file that runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecorderProfile {
    record: PathBuf,
    dump: PathBuf,
    backend: RecorderBackend,
    /// `gst-launch-1.0` for the GStreamer recorder.
    launcher: Option<PathBuf>,
}

impl RecorderProfile {
    /// `pw-record` and `pw-dump` as `PATH` names them: what dictation
    /// runs. Blocks for at most [`ANSWER_TIMEOUT`] per program; `doctor`
    /// calls it off the runtime thread.
    pub(crate) fn on_path() -> io::Result<Self> {
        Self::found(Path::new(PW_RECORD), Path::new("pw-dump"))
    }

    /// The programs a caller names, with what records through them. A bare
    /// name is looked up on `PATH`, like the program of a command, and so
    /// is a GStreamer tool it needs; a path is used where it is, and its
    /// GStreamer tools are the ones installed next to it, never another
    /// copy from `PATH`. Blocking, like [`Self::on_path`].
    pub(crate) fn found(record: &Path, dump: &Path) -> io::Result<Self> {
        // `pw-record` stays the name it was called by: when it is a link to
        // `pw-cat`, argv[0] says that it records.
        let named = record.components().count() == 1;
        let record_path = locate(record, PIPEWIRE)?;
        let dump = locate(dump, PIPEWIRE)?;
        let backend = RecorderBackend::detect(&record_path)?;
        let launcher = match backend {
            RecorderBackend::PwRecord { .. } => None,
            RecorderBackend::Gstreamer => Some(gstreamer(record, named)?),
        };
        Ok(Self {
            record: record_path,
            dump,
            backend,
            launcher,
        })
    }

    /// [`Self::on_path`] off the runtime thread.
    pub(crate) async fn detect_on_path() -> io::Result<Self> {
        Self::detect(
            Path::new(PW_RECORD).to_owned(),
            Path::new("pw-dump").to_owned(),
        )
        .await
    }

    /// [`Self::found`] off the runtime thread.
    pub(crate) async fn detect(record: PathBuf, dump: PathBuf) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || Self::found(&record, &dump))
            .await
            .map_err(|error| io::Error::other(format!("the recorder probe ended: {error}")))?
    }

    pub(crate) fn record(&self) -> &Path {
        &self.record
    }

    pub(crate) fn backend(&self) -> RecorderBackend {
        self.backend
    }

    /// One line for a check's detail: what records, and what reads the graph.
    pub(crate) fn describe(&self) -> String {
        let recorder = match self.backend {
            RecorderBackend::PwRecord { raw } => format!(
                "{} --properties{}",
                self.record.display(),
                if raw { " --raw" } else { "" }
            ),
            RecorderBackend::Gstreamer => format!(
                "{} pipewiresrc ({} has no --properties)",
                self.launcher
                    .as_deref()
                    .unwrap_or(Path::new(GST_LAUNCH))
                    .display(),
                self.record.display()
            ),
        };
        format!("{recorder}; {}", self.dump.display())
    }

    /// The recorder of the stream `stream` reading `target`: 16 kHz mono
    /// PCM16 on stdout, stdin closed and stderr left to the caller. The
    /// recorder runs in a process group of its own, so that a terminal's
    /// interrupt reaches its owner alone, and ends with the thread that
    /// started it.
    pub(crate) fn command(&self, stream: &str, target: Target<'_>) -> Command {
        let mut command = match self.backend {
            RecorderBackend::PwRecord { raw } => {
                let mut command = Command::new(&self.record);
                let mut properties = format!(
                    "node.name={stream} node.dont-reconnect=true stream.capture.sink={}",
                    target.monitor()
                );
                if let Some((_, serial)) = target.node() {
                    properties.push_str(" node.dont-fallback=true");
                    command.arg("--target").arg(serial.to_string());
                }
                command.arg("--properties").arg(properties);
                if raw {
                    command.arg("--raw");
                }
                command.args([
                    "--rate",
                    &SAMPLE_RATE.to_string(),
                    "--channels",
                    "1",
                    "--format",
                    "s16",
                    "--latency",
                    "32ms",
                    "-",
                ]);
                command
            }
            RecorderBackend::Gstreamer => {
                let mut command =
                    Command::new(self.launcher.as_deref().unwrap_or(Path::new(GST_LAUNCH)));
                command.args(["-q", "-e", "pipewiresrc"]);
                command.arg(format!("client-name={stream}"));
                // `PIPEWIRE_NODE` is how the source is told its target. It
                // takes the node's name on every PipeWire: 1.0 would also
                // take the serial, the older ones would read one as a node
                // id. `verify` checks the serial of what was linked.
                if let Some((name, _)) = target.node() {
                    command.env("PIPEWIRE_NODE", name);
                }
                let mut properties = String::from("props,node.dont-reconnect=true");
                if target.node().is_some() {
                    properties.push_str(",node.dont-fallback=true");
                }
                properties.push_str(&format!(",stream.capture.sink={}", target.monitor()));
                command.arg(format!("stream-properties={properties}"));
                command.args([
                    "!",
                    "audio/x-raw,format=S16LE,rate=16000,channels=1",
                    "!",
                    "fdsink",
                    "fd=1",
                ]);
                command
            }
        };
        command.stdin(Stdio::null()).stdout(Stdio::piped());
        end_with_owner(&mut command);
        command
    }

    /// Whether a recording with it has to watch its link: `gst-launch-1.0`
    /// keeps running with no audio once its source is gone, where
    /// `pw-record` ends and says why.
    pub(crate) fn watches_link(&self) -> bool {
        self.backend == RecorderBackend::Gstreamer
    }

    /// What the graph says now of the link into `stream`. `Err` when the
    /// graph could not be read just now. Blocking, for a thread that records.
    pub(crate) fn linked_now(
        &self,
        stream: &str,
        target: Target<'_>,
        pid: u32,
    ) -> Result<Fed, String> {
        let output = run(&self.dump, &[]).map_err(|error| error.to_string())?;
        Ok(fed(&decode(&self.dump, output)?, stream, target, Some(pid)))
    }

    /// [`Self::linked_now`] for a runtime.
    pub(crate) async fn linked(
        &self,
        stream: &str,
        target: Target<'_>,
        pid: Option<u32>,
    ) -> Result<Fed, String> {
        Ok(fed(&dump(&self.dump).await?, stream, target, pid))
    }

    /// Wait for the selected node to feed the stream. A missing or replaced
    /// node, a link from another source, or a recorder exit fails immediately.
    /// An inactive link may still start within [`LINK_TIMEOUT`].
    pub(crate) async fn verify(
        &self,
        stream: &str,
        target: Target<'_>,
        pid: Option<u32>,
        mut running: impl FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        let (name, serial) = target.node().expect("a node is verified");
        tokio::time::timeout(LINK_TIMEOUT, async {
            loop {
                running()?;
                let objects = dump(&self.dump).await.map_err(io::Error::other)?;
                match fed(&objects, stream, target, pid) {
                    Fed::Yes => return Ok(()),
                    Fed::No => {}
                    Fed::Denied => return Err(io::ErrorKind::PermissionDenied.into()),
                    Fed::Lost(reason) => return Err(io::Error::other(reason)),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no active link from {name:?} (serial {serial}) into {stream:?}"),
            )
        })?
    }
}

/// What a recording stream reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    /// Let the session manager choose. Any active source can feed this stream.
    Default,
    /// One node: the `node.name` it was selected by and the `object.serial`
    /// a fresh dump gave it. `monitor`: the monitor of an `Audio/Sink` node.
    Node {
        name: &'a str,
        serial: u64,
        monitor: bool,
    },
}

impl<'a> Target<'a> {
    /// The node's name and serial, when one is asked for.
    pub(crate) fn node(self) -> Option<(&'a str, u64)> {
        match self {
            Self::Default => None,
            Self::Node { name, serial, .. } => Some((name, serial)),
        }
    }

    fn monitor(self) -> bool {
        matches!(self, Self::Node { monitor: true, .. })
    }
}

/// The GStreamer tools of a recorder that needs them: `gst-launch-1.0`,
/// found as `record` was when `named` (on `PATH`) and otherwise installed
/// next to it, with `gst-inspect-1.0` showing that `pipewiresrc` is there.
fn gstreamer(record: &Path, named: bool) -> io::Result<PathBuf> {
    let tool = |name: &str| {
        locate(
            &if named {
                PathBuf::from(name)
            } else {
                record.with_file_name(name)
            },
            TOOLS,
        )
    };
    let launch = tool(GST_LAUNCH)?;
    let inspect = tool(GST_INSPECT)?;
    let element = run(&inspect, &["pipewiresrc"])?;
    if !element.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} has no pipewiresrc element: {}; install {PLUGIN}",
                inspect.display(),
                complaint(&element)
            ),
        ));
    }
    Ok(launch)
}

/// The file that runs the program its caller named: a bare name is looked
/// up in `PATH`, a path is taken as it is. `package` says what installs it.
fn locate(program: &Path, package: &str) -> io::Result<PathBuf> {
    let named = program.components().count() == 1;
    let path = if named {
        let found = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join(program))
                .find(|candidate| candidate.is_file())
        });
        found.unwrap_or_else(|| program.to_owned())
    } else {
        program.to_owned()
    };
    if path.is_file() {
        return Ok(path);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} is absent{}; install {package}",
            program.display(),
            if named { " from PATH" } else { "" }
        ),
    ))
}

/// Run `program` with `args`, and its output once it ended, which it has
/// to do within [`ANSWER_TIMEOUT`] or it is killed. Blocking.
fn run(program: &Path, args: &[&str]) -> io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot run {}: {error}", program.display()),
            )
        })?;
    // Drain while it runs, so that a talkative program cannot block on a full pipe.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} did not answer within {ANSWER_TIMEOUT:?}",
                        program.display()
                    ),
                ));
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Read a pipe to its end on a thread of its own; the thread's result is what was read.
fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// A program's own account of why it failed: the first line of its stderr,
/// else its exit status.
fn complaint(output: &Output) -> String {
    match String::from_utf8_lossy(&output.stderr)
        .trim()
        .lines()
        .next()
    {
        Some(line) => line.to_owned(),
        None => output.status.to_string(),
    }
}

/// Put the recorder in a process group of its own and end it with the
/// thread that starts it. The recorder gets SIGINT when that thread dies,
/// so a runtime that crashes or is killed by hand never leaves a
/// microphone open; a terminal's interrupt goes to the owner alone, which
/// then stops the recorder itself and keeps the tail.
fn end_with_owner(command: &mut Command) {
    const PR_SET_PDEATHSIG: libc::c_int = 1;
    // SAFETY: between fork and exec the closure makes two system calls, which are safe to make
    // there, and leaves through `_exit` if one fails.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 || libc::prctl(PR_SET_PDEATHSIG, libc::SIGINT) != 0 {
                libc::_exit(127);
            }
            Ok(())
        });
    }
}

// ------------------------------------------------------------------ graph

/// One `pw-dump` of the graph, or the reason it could not be read.
pub(crate) async fn dump(program: &Path) -> Result<Vec<Value>, String> {
    let output = tokio::time::timeout(
        ANSWER_TIMEOUT,
        tokio::process::Command::new(program)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) => decode(program, output),
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => Err(format!(
            "{} is absent; install {PIPEWIRE}",
            program.display()
        )),
        Ok(Err(error)) => Err(format!("cannot run {}: {error}", program.display())),
        Err(_) => Err(format!(
            "{} did not answer within {ANSWER_TIMEOUT:?}",
            program.display()
        )),
    }
}

/// The objects of a finished `pw-dump`, or its own complaint: `pw-dump`
/// says why on stderr when there is no session daemon to answer. An object
/// that leaves while the graph is printed is named on its own, as
/// `[{"id": N, "info": null}]` before or after the graph; it is not in it.
pub(crate) fn decode(program: &Path, output: Output) -> Result<Vec<Value>, String> {
    let name = program.display();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let status = output.status.to_string();
        let failure = stderr.trim().lines().next().unwrap_or(&status);
        return Err(format!("{name} failed: {failure}"));
    }
    let mut graph: Option<Vec<Value>> = None;
    let mut left: Vec<u64> = Vec::new();
    for value in serde_json::Deserializer::from_slice(&output.stdout).into_iter::<Value>() {
        let list = match value {
            Ok(Value::Array(list)) => list,
            Ok(_) if graph.is_none() => {
                return Err(format!(
                    "{name} printed something other than the PipeWire graph"
                ));
            }
            Err(error) if graph.is_none() && left.is_empty() => {
                return Err(format!("{name} printed invalid JSON: {error}"));
            }
            // Nothing whole after the graph began is not the graph.
            Ok(_) | Err(_) => break,
        };
        if let [object] = list.as_slice() {
            if object.get("type").is_none() && object.get("info").is_some_and(Value::is_null) {
                left.extend(object.get("id").and_then(Value::as_u64));
                continue;
            }
        }
        graph = Some(list);
    }
    let Some(mut objects) = graph else {
        return Err(match serde_json::from_slice::<Value>(&output.stdout) {
            Err(error) => format!("{name} printed invalid JSON: {error}"),
            Ok(_) => format!("{name} printed something other than the PipeWire graph"),
        });
    };
    objects.retain(|object| !object["id"].as_u64().is_some_and(|id| left.contains(&id)));
    Ok(objects)
}

/// What the graph says of a stream's link at one moment.
#[derive(Debug)]
pub(crate) enum Fed {
    /// An `active` link from the node feeds the stream.
    Yes,
    /// The selected device is still present, or the system chooses the device,
    /// but the stream has no active link.
    No,
    /// The server rejected the recorder's client before it could publish its stream.
    Denied,
    /// The selected node disappeared or another node feeds the stream.
    Lost(String),
}

impl Fed {
    /// Give a transient missing link time to recover. A different source or
    /// a missing device serial is conclusive on the first observation.
    pub(crate) fn loss(self, unlinked: &mut Option<Instant>, stream: &str) -> Option<io::Error> {
        match self {
            Self::Yes => {
                *unlinked = None;
                None
            }
            Self::No if unlinked.get_or_insert_with(Instant::now).elapsed() >= LINK_GRACE => {
                Some(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("the graph no longer links {stream} to an audio source"),
                ))
            }
            Self::No => None,
            Self::Denied => Some(io::ErrorKind::PermissionDenied.into()),
            Self::Lost(reason) => Some(io::Error::new(io::ErrorKind::BrokenPipe, reason)),
        }
    }
}

/// A node a stream can be pointed at.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Selectable<'a> {
    /// `node.name`.
    pub name: &'a str,
    /// `node.description`, else `node.nick`, else the name.
    pub label: String,
    /// `object.serial`: what a stream is pointed at with.
    pub serial: u64,
    /// An `Audio/Sink` node, recorded through its monitor; else an
    /// `Audio/Source` node.
    pub monitor: bool,
}

/// The node of a `pw-dump` object when a stream can be pointed at it and
/// it can be named: `Audio/Source` and `Audio/Sink` (and their subtypes, a
/// virtual source among them) with an `object.serial`. Streams, MIDI and
/// video sources and nodes without a serial select nothing.
pub(crate) fn selectable(object: &Value) -> Option<Selectable<'_>> {
    let props = object.pointer("/info/props")?;
    let name = props.get("node.name")?.as_str()?;
    let class = props.get("media.class")?.as_str()?;
    let monitor = if class == "Audio/Sink" || class.starts_with("Audio/Sink/") {
        true
    } else if class == "Audio/Source" || class.starts_with("Audio/Source/") {
        false
    } else {
        return None;
    };
    let serial = props
        .get("object.serial")?
        .as_u64()
        .filter(|serial| *serial != 0)?;
    // `node.description`, else `node.nick`; a node with neither is shown by its name.
    let label = ["node.description", "node.nick"]
        .iter()
        .filter_map(|key| props.get(*key).and_then(Value::as_str))
        .find(|value| !value.is_empty())
        .unwrap_or(name)
        .to_owned();
    Some(Selectable {
        name,
        label,
        serial,
        monitor,
    })
}

/// A node's `info.props` entry.
fn property<'a>(object: &'a Value, key: &str) -> Option<&'a Value> {
    object
        .get("info")
        .and_then(|info| info.get("props"))
        .and_then(|props| props.get(key))
}

/// Inspect every link into this stream. An explicitly selected source must
/// still have the same serial. The default microphone has no pinned identity.
fn fed(objects: &[Value], stream: &str, target: Target<'_>, recorder_pid: Option<u32>) -> Fed {
    let node = |id: u64| {
        objects.iter().find(|object| {
            object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node")
                && object.get("id").and_then(Value::as_u64) == Some(id)
        })
    };
    let matches_source = |source: &Value, name: &str, serial: u64| {
        property(source, "node.name").and_then(Value::as_str) == Some(name)
            && property(source, "object.serial").and_then(Value::as_u64) == Some(serial)
    };
    let mut active = false;
    for link in objects.iter().filter(|object| {
        object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Link")
    }) {
        let Some(info) = link.get("info") else {
            continue;
        };
        let input = info.get("input-node-id").and_then(Value::as_u64);
        let output = info.get("output-node-id").and_then(Value::as_u64);
        let Some((input, output)) = input.zip(output) else {
            continue;
        };
        if node(input)
            .is_none_or(|node| property(node, "node.name").and_then(Value::as_str) != Some(stream))
        {
            continue;
        }
        // A link whose source left the graph with it says nothing about ours.
        let Some(source) = node(output) else {
            continue;
        };
        if let Some((name, serial)) = target.node() {
            if !matches_source(source, name, serial) {
                return Fed::Lost(format!(
                    "requested {name:?} (serial {serial}), {stream:?} linked to node {output}"
                ));
            }
        }
        active |= info.get("state").and_then(Value::as_str) == Some("active");
    }
    if active {
        return Fed::Yes;
    }
    if let Some((name, serial)) = target.node() {
        if !objects.iter().any(|object| {
            object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node")
                && matches_source(object, name, serial)
        }) {
            return Fed::Lost(format!(
                "selected node {name:?} (serial {serial}) left the PipeWire graph"
            ));
        }
    }
    // A rejected client can remain alive without a stream. Match the PID
    // assigned by the server, since application.process.id is client-supplied.
    if recorder_pid.is_some_and(|pid| {
        objects.iter().any(|object| {
            object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Client")
                && property(object, "pipewire.access").and_then(Value::as_str) == Some("rejected")
                && property(object, "pipewire.sec.pid").and_then(Value::as_u64)
                    == Some(u64::from(pid))
        })
    }) {
        return Fed::Denied;
    }
    Fed::No
}

// ------------------------------------------------------- end of a recorder

/// Whether a recorder that was asked to stop ended as one does: with
/// success, from the SIGINT that asks for it, or as `pw-record` does after
/// it. PipeWire 1.6's signal handler quits the loop without setting
/// `drained`, which leaves EXIT_FAILURE even after a requested SIGINT.
pub(crate) fn stopped_as_asked(status: &ExitStatus, backend: RecorderBackend) -> bool {
    status.success()
        || status.signal() == Some(libc::SIGINT)
        || (matches!(backend, RecorderBackend::PwRecord { .. }) && status.code() == Some(1))
}

/// What a recorder reported when it ended.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Diagnostic {
    /// The first line that reports a fatal error.
    pub fatal: Option<String>,
    /// Some line says the stream was not permitted.
    pub permission: bool,
}

impl Diagnostic {
    /// The same, with these lines of the recorder taken into account after
    /// the ones it gave already: the first that reports a fatal error is
    /// the reason.
    pub(crate) fn with_lines(mut self, lines: impl IntoIterator<Item = String>) -> Self {
        for line in lines {
            self.absorb(&line);
        }
        self
    }

    /// The same, from what the recorder wrote to the file at `path`;
    /// nothing when there is no such file.
    pub(crate) fn with_log(self, path: &Path) -> Self {
        self.with_lines(
            std::fs::read(path)
                .map(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .lines()
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
    }

    /// The same, from what the recorder wrote to its output and nobody
    /// read: its last [`OUTPUT_TAIL`] bytes are looked at.
    pub(crate) fn with_output(self, tail: &[u8]) -> Self {
        self.with_lines(split_output(&tail[tail.len().saturating_sub(OUTPUT_TAIL)..]).1)
    }

    /// Take one line of the recorder into account.
    pub(crate) fn absorb(&mut self, line: &str) {
        let line = line.trim_end();
        if self.fatal.is_none() && recorder_fatal(line) {
            self.fatal = Some(line.to_owned());
        }
        self.permission |= is_permission(line);
    }

    /// A recorder that ended on its own: what it said comes first, else
    /// `observed`, which is what was seen of it. A denial is reported as
    /// one, with where access is decided.
    pub(crate) fn reported(self, observed: &io::Error) -> io::Error {
        let seen = observed.to_string();
        let reason = self.fatal.as_deref().unwrap_or(&seen);
        if self.permission || observed.kind() == io::ErrorKind::PermissionDenied {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{reason}; {}", remedy()),
            )
        } else {
            io::Error::new(observed.kind(), reason.to_owned())
        }
    }
}

/// Where access to an audio source is decided on this machine: a Flatpak
/// sandbox through its permissions, every other desktop through its
/// privacy switch, its sound settings or the session manager's rules.
pub(crate) fn remedy() -> &'static str {
    if Path::new("/.flatpak-info").exists() {
        "Voiceless has no audio access inside this Flatpak sandbox; allow audio input for the \
         application in its permissions and start again"
    } else {
        "check the microphone privacy switch and the sound settings, or the PipeWire access rules \
         of the session manager, and start again"
    }
}

/// Whether a recorder line reports a fatal error of its stream.
pub(crate) fn recorder_fatal(line: &str) -> bool {
    line.starts_with("error:")
        || line.starts_with("ERROR:")
        || line.starts_with("remote error:")
        || (line.starts_with("stream node ")
            && (line.contains(" error:") || line.ends_with(" unconnected")))
}

/// Whether a recorder line says the stream was not permitted: what
/// PipeWire answers for `EPERM` and `EACCES`, and what a portal or a policy
/// calls it.
pub(crate) fn is_permission(line: &str) -> bool {
    const DENIALS: [&str; 4] = [
        "permission denied",
        "operation not permitted",
        "not allowed",
        "access denied",
    ];
    let line = line.to_ascii_lowercase();
    DENIALS.iter().any(|denial| line.contains(denial))
}
/// Where the audio ends in `end`, the last bytes of a `pw-record` output:
/// its length when they are all audio, and the lines the recorder printed
/// after its last sample, in order, when there are. PipeWire before 1.4
/// prints them on stdout, after the audio, when its stream ends on its
/// own: `stream node <id> error: <reason>` and then `stream node <id>
/// unconnected` (Ubuntu 24.04, for a source that left: `stream node 56
/// error: target not found`, `stream node 4294967295 unconnected`). Text
/// longer than [`OUTPUT_TAIL`] is not told from audio.
pub(crate) fn split_output(end: &[u8]) -> (usize, Vec<String>) {
    const START: &[u8] = b"stream node ";
    let mut audio = end.len();
    let mut lines = Vec::new();
    // The end is tried first, line by line: the audio may hold anything.
    while let Some(at) = end[..audio]
        .windows(START.len())
        .rposition(|window| window == START)
    {
        let Some(line) = stream_line(&end[at..audio]) else {
            break;
        };
        audio = at;
        lines.push(line);
    }
    lines.reverse();
    (audio, lines)
}

/// The line in `bytes` when it is one `pw-record` prints for its stream.
fn stream_line(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rest = text.strip_prefix("stream node ")?.strip_suffix('\n')?;
    if rest.contains('\n') || !rest.is_ascii() {
        return None;
    }
    let (node, what) = rest.split_once(' ')?;
    let ended =
        what == "unconnected" || what.strip_prefix("error: ").is_some_and(|r| !r.is_empty());
    (ended && !node.is_empty() && node.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A profile with these programs, without running anything.
    fn profile(backend: RecorderBackend) -> RecorderProfile {
        RecorderProfile {
            record: PathBuf::from("/usr/bin/pw-record"),
            dump: PathBuf::from("/usr/bin/pw-dump"),
            backend,
            launcher: (backend == RecorderBackend::Gstreamer)
                .then(|| PathBuf::from("/usr/bin/gst-launch-1.0")),
        }
    }

    fn argv(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn environment(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    const HEADSET: &str = "alsa_input.usb-Headset-00.mono-fallback";
    const MICROPHONE: Target<'static> = Target::Node {
        name: HEADSET,
        serial: 31,
        monitor: false,
    };
    const SPEAKERS: Target<'static> = Target::Node {
        name: "alsa_output.pci-0000_0c_00_4.analog-stereo",
        serial: 42,
        monitor: true,
    };

    #[test]
    fn a_help_text_names_the_recorder_and_its_raw_option() {
        // Ubuntu 24.04, PipeWire 1.0.5.
        let noble = "pw-record [options] [<file>|-]\n  -h, --help   Show this help\n  \
                     -P  --properties                      Set node properties\n      \
                     --target                          Set node target serial or name\n";
        // PipeWire 1.4 and later, where `--raw` keeps the output free of a file header.
        let recent = format!("{noble}  -a, --raw                             RAW mode\n");
        assert!(lists(noble, "--properties"));
        assert!(!lists(noble, "--raw"));
        assert!(lists(&recent, "--raw"));
        // Ubuntu 22.04, PipeWire 0.3.48.
        assert!(!lists(
            "pw-record [options] <file>\n      --list-targets   List available targets for --target\n",
            "--properties"
        ));
        // An option is a word of its own.
        assert!(!lists(
            "  --properties-key   something else",
            "--properties"
        ));
        assert!(!lists("", "--raw"));
    }

    #[test]
    fn pw_record_is_told_its_stream_its_target_and_to_never_fall_back() {
        let recent = profile(RecorderBackend::PwRecord { raw: true });
        let command = recent.command("voiceless-capture-7", MICROPHONE);
        assert_eq!(command.get_program(), Path::new("/usr/bin/pw-record"));
        assert_eq!(
            argv(&command),
            [
                "--target",
                "31",
                "--properties",
                "node.name=voiceless-capture-7 node.dont-reconnect=true \
                 stream.capture.sink=false node.dont-fallback=true",
                "--raw",
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "s16",
                "--latency",
                "32ms",
                "-",
            ]
        );
        assert!(environment(&command).is_empty());
        // An output is recorded through its monitor.
        let monitor = argv(&recent.command("voiceless-meeting-7-system", SPEAKERS));
        assert_eq!(
            monitor[..4],
            [
                "--target",
                "42",
                "--properties",
                "node.name=voiceless-meeting-7-system node.dont-reconnect=true \
                 stream.capture.sink=true node.dont-fallback=true",
            ]
        );
        // Before 1.4 there is no `--raw` to give.
        let old = profile(RecorderBackend::PwRecord { raw: false });
        assert!(!argv(&old.command("s", MICROPHONE))
            .iter()
            .any(|arg| arg == "--raw"));
        // The system default is asked for by nobody: no target, so nothing to keep from falling back.
        let default = argv(&old.command("voiceless-capture-7", Target::Default));
        assert_eq!(
            default[..2],
            [
                "--properties",
                "node.name=voiceless-capture-7 node.dont-reconnect=true stream.capture.sink=false",
            ]
        );
        assert!(!default.iter().any(|arg| arg == "--target"));
    }

    #[test]
    fn gstreamer_is_told_the_same_in_its_own_vocabulary() {
        let gstreamer = profile(RecorderBackend::Gstreamer);
        let command = gstreamer.command("voiceless-meeting-7-system", SPEAKERS);
        assert_eq!(command.get_program(), Path::new("/usr/bin/gst-launch-1.0"));
        // The name, which every PipeWire reads as the node it is: an older
        // one would read a serial as the id of some other node.
        assert_eq!(
            environment(&command),
            [(
                "PIPEWIRE_NODE".to_owned(),
                Some("alsa_output.pci-0000_0c_00_4.analog-stereo".to_owned())
            )]
        );
        assert_eq!(
            argv(&command),
            [
                "-q",
                "-e",
                "pipewiresrc",
                "client-name=voiceless-meeting-7-system",
                "stream-properties=props,node.dont-reconnect=true,node.dont-fallback=true,\
                 stream.capture.sink=true",
                "!",
                "audio/x-raw,format=S16LE,rate=16000,channels=1",
                "!",
                "fdsink",
                "fd=1",
            ]
        );
        let default = gstreamer.command("voiceless-capture-7", Target::Default);
        assert!(environment(&default).is_empty());
        assert_eq!(
            argv(&default)[..5],
            [
                "-q",
                "-e",
                "pipewiresrc",
                "client-name=voiceless-capture-7",
                "stream-properties=props,node.dont-reconnect=true,stream.capture.sink=false",
            ]
        );
    }

    #[test]
    fn a_check_says_what_records_and_what_reads_the_graph() {
        assert_eq!(
            profile(RecorderBackend::PwRecord { raw: true }).describe(),
            "/usr/bin/pw-record --properties --raw; /usr/bin/pw-dump"
        );
        assert_eq!(
            profile(RecorderBackend::PwRecord { raw: false }).describe(),
            "/usr/bin/pw-record --properties; /usr/bin/pw-dump"
        );
        assert_eq!(
            profile(RecorderBackend::Gstreamer).describe(),
            "/usr/bin/gst-launch-1.0 pipewiresrc (/usr/bin/pw-record has no --properties); \
             /usr/bin/pw-dump"
        );
    }

    #[test]
    fn a_program_that_is_a_path_is_never_looked_up_elsewhere() {
        // A stand-in named by a test, with nothing installed next to it.
        let absent = locate(Path::new("/nonexistent/bin/pw-record"), PIPEWIRE).unwrap_err();
        assert_eq!(absent.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            absent.to_string(),
            "/nonexistent/bin/pw-record is absent; install PipeWire (the pipewire-bin package on \
             Debian and Ubuntu)"
        );
        let named = locate(Path::new("voiceless-no-such-recorder"), PIPEWIRE).unwrap_err();
        assert!(
            named
                .to_string()
                .starts_with("voiceless-no-such-recorder is absent from PATH; install PipeWire"),
            "{named}"
        );
    }

    /// What `selectable` makes of a node with these props, as owned data:
    /// its name, label, serial and whether it is recorded through its monitor.
    fn selected(props: Value) -> Option<(String, String, u64, bool)> {
        selectable(&json!({
            "id": 61, "type": "PipeWire:Interface:Node", "info": {"props": props}
        }))
        .map(|node| (node.name.to_owned(), node.label, node.serial, node.monitor))
    }

    #[test]
    fn sources_and_sinks_are_selectable_and_nothing_else_is() {
        const SPEAKERS: &str = "alsa_output.pci-0000_0c_00_4.analog-stereo";
        assert_eq!(
            selected(json!({
                "node.name": HEADSET, "media.class": "Audio/Source",
                "node.description": "Lian II Mono", "node.nick": "Lian II", "object.serial": 90,
            })),
            Some((HEADSET.to_owned(), "Lian II Mono".to_owned(), 90, false))
        );
        // A virtual source has a nick and no description.
        assert_eq!(
            selected(json!({
                "node.name": "echo-cancel-source", "media.class": "Audio/Source/Virtual",
                "node.nick": "Echo Cancel Source", "object.serial": 101,
            })),
            Some((
                "echo-cancel-source".to_owned(),
                "Echo Cancel Source".to_owned(),
                101,
                false
            ))
        );
        // A node with neither is shown by its name.
        assert_eq!(
            selected(json!({
                "node.name": "test-source", "media.class": "Audio/Source", "object.serial": 102,
            })),
            Some((
                "test-source".to_owned(),
                "test-source".to_owned(),
                102,
                false
            ))
        );
        // An output is recorded through its monitor, whatever its name says.
        assert_eq!(
            selected(json!({
                "node.name": SPEAKERS, "media.class": "Audio/Sink",
                "node.description": "Starship/Matisse Analog Stereo", "object.serial": 55,
            })),
            Some((
                SPEAKERS.to_owned(),
                "Starship/Matisse Analog Stereo".to_owned(),
                55,
                true
            ))
        );
        assert_eq!(
            selected(
                json!({"node.name": HEADSET, "media.class": "Audio/Sink", "object.serial": 7})
            ),
            Some((HEADSET.to_owned(), HEADSET.to_owned(), 7, true))
        );
        // Another client's stream, a MIDI or video source, a node without a
        // serial and one without a name select nothing.
        for props in [
            json!({"node.name": "audio-input-zoom", "media.class": "Stream/Input/Audio", "object.serial": 130}),
            json!({"node.name": "midi-in", "media.class": "Midi/Source", "object.serial": 3}),
            json!({"node.name": "camera", "media.class": "Video/Source", "object.serial": 4}),
            json!({"node.name": "serial-less", "media.class": "Audio/Source"}),
            json!({"node.name": "zero", "media.class": "Audio/Sink", "object.serial": 0}),
            json!({"media.class": "Audio/Source", "object.serial": 5}),
        ] {
            assert_eq!(selected(props.clone()), None, "{props}");
        }
        assert_eq!(
            selectable(&json!({"id": 1, "type": "PipeWire:Interface:Link"})),
            None
        );
    }

    /// A node as `pw-dump` prints it.
    fn node(id: u64, name: &str, serial: u64) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.name": name, "object.serial": serial,
        }}})
    }

    /// A link as `pw-dump` prints it, between one output and one input node.
    fn link(output: u64, input: u64, state: &str) -> Value {
        json!({"id": 70, "type": "PipeWire:Interface:Link", "info": {
            "output-node-id": output, "input-node-id": input, "state": state,
        }})
    }

    const STREAM: &str = "voiceless-capture-7";

    #[test]
    fn a_stream_reads_its_node_once_an_active_link_comes_from_it_and_only_it() {
        let graph = [
            node(61, HEADSET, 31),
            node(64, "alsa_input.pci-0000_0c_00_4.analog-stereo", 95),
            node(47, STREAM, 80),
            node(74, "audio-input-zoom", 130),
        ];
        let with = |links: &[Value]| {
            graph
                .iter()
                .cloned()
                .chain(links.iter().cloned())
                .collect::<Vec<_>>()
        };
        let headset = |links: &[Value]| fed(&with(links), STREAM, MICROPHONE, None);
        // A present device can reconnect a missing or paused link.
        assert!(matches!(headset(&[]), Fed::No));
        assert!(matches!(headset(&[link(61, 47, "paused")]), Fed::No));
        // Flowing from the headset, next to links of other streams and its own monitor's.
        let active = [
            link(64, 74, "active"),
            link(47, 74, "active"),
            link(61, 47, "active"),
        ];
        assert!(matches!(headset(&active), Fed::Yes));
        // Another node feeds the stream: the session manager fell back.
        let other = [link(64, 47, "active")];
        assert!(matches!(
            fed(&with(&other), STREAM, Target::Default, None),
            Fed::Yes
        ));
        let Fed::Lost(wrong) = headset(&other) else {
            panic!("the stream accepted a different microphone");
        };
        assert!(
            wrong.contains(&format!(
                "requested {HEADSET:?} (serial 31), {STREAM:?} linked to node 64"
            )),
            "{wrong}"
        );
        // Wrong before it flows, and wrong when the node's name now belongs to another serial.
        assert!(matches!(headset(&[link(64, 47, "paused")]), Fed::Lost(_)));
        let reused = [
            node(61, HEADSET, 97),
            node(47, STREAM, 80),
            link(61, 47, "active"),
        ];
        assert!(matches!(
            fed(&reused, STREAM, MICROPHONE, None),
            Fed::Lost(_)
        ));
        // A link pointing at a removed node cannot feed even a default recording.
        let gone = [
            node(47, STREAM, 80),
            link(61, 47, "active"),
            link(61, 47, "active"),
        ];
        assert!(matches!(fed(&gone, STREAM, MICROPHONE, None), Fed::Lost(_)));
        assert!(matches!(fed(&gone, STREAM, Target::Default, None), Fed::No));
    }

    #[test]
    fn a_denied_recorder_is_distinct_from_a_stream_waiting_for_its_link() {
        let mut graph = [
            node(61, HEADSET, 31),
            json!({"type": "PipeWire:Interface:Client", "info": {"props": {
                "pipewire.access": "rejected", "pipewire.sec.pid": 81, "application.process.id": 92
            }}}),
        ];
        assert!(matches!(
            fed(&graph, STREAM, MICROPHONE, Some(81)),
            Fed::Denied
        ));
        assert!(matches!(fed(&graph, STREAM, MICROPHONE, Some(92)), Fed::No));
        graph[1]["info"]["props"]["pipewire.access"] = json!("restricted");
        assert!(matches!(fed(&graph, STREAM, MICROPHONE, Some(81)), Fed::No));
    }

    /// A finished run with this wait status, as `pw-dump` leaves it.
    fn ran(raw_status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::from_raw(raw_status),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn a_dump_that_did_not_print_the_graph_is_rejected() {
        let program = Path::new("/bin/pw-dump");
        // No session daemon: the tool explains itself.
        let down = decode(
            program,
            ran(255 << 8, b"", b"can't connect: Host is down\n"),
        );
        assert_eq!(
            down,
            Err("/bin/pw-dump failed: can't connect: Host is down".to_owned())
        );
        // Killed: the exit status is all there is to say.
        let killed = decode(program, ran(9, b"", b""));
        assert_eq!(
            killed,
            Err("/bin/pw-dump failed: signal: 9 (SIGKILL)".to_owned())
        );
        let graph = decode(program, ran(0, b"[{\"id\": 0}]\n", b""));
        assert_eq!(graph, Ok(vec![json!({"id": 0})]));
        let broken = decode(program, ran(0, b"", b"")).unwrap_err();
        assert!(
            broken.starts_with("/bin/pw-dump printed invalid JSON: "),
            "{broken}"
        );
        // Well-formed, but not what `pw-dump` prints.
        let foreign = decode(program, ran(0, b"{\"id\": 0}", b"")).unwrap_err();
        assert_eq!(
            foreign,
            "/bin/pw-dump printed something other than the PipeWire graph"
        );
    }

    #[test]
    fn objects_that_left_while_the_graph_was_printed_are_not_in_it() {
        // What `pw-dump` on PipeWire 1.0.5 prints when objects leave under
        // it, as caught on Ubuntu 24.04: the graph, and each object that
        // left on its own, before or after it.
        let program = Path::new("/usr/bin/pw-dump");
        let graph = "[{\"id\":1,\"type\":\"PipeWire:Interface:Node\"},{\"id\":2,\"type\":\"PipeWire:Interface:Link\"},{\"id\":3,\"type\":\"PipeWire:Interface:Node\"}]";
        let left = |id: u8| format!("[{{\"id\":{id},\"info\":null}}]\n");
        let ids = |objects: &[Value]| {
            objects
                .iter()
                .map(|object| object["id"].as_u64())
                .collect::<Vec<_>>()
        };
        let after = format!("{graph}\n{}{}{}", left(2), left(3), left(9));
        assert_eq!(
            ids(&decode(program, ran(0, after.as_bytes(), b"")).unwrap()),
            [Some(1)]
        );
        let before = format!("{}{}{graph}\n", left(3), left(1));
        assert_eq!(
            ids(&decode(program, ran(0, before.as_bytes(), b"")).unwrap()),
            [Some(2)]
        );
        // Two graphs: the later one is the graph.
        let twice = format!("{graph}\n[{{\"id\":7,\"type\":\"PipeWire:Interface:Node\"}}]\n");
        assert_eq!(
            ids(&decode(program, ran(0, twice.as_bytes(), b"")).unwrap()),
            [Some(7)]
        );
        // Only what left, and no graph, is no graph.
        assert_eq!(
            decode(program, ran(0, left(1).as_bytes(), b"")).unwrap_err(),
            "/usr/bin/pw-dump printed something other than the PipeWire graph"
        );
    }

    #[test]
    fn the_lines_pw_record_prints_after_its_audio_are_not_kept_as_audio() {
        let audio = [0x00, 0x40, 0x00, 0xC0, 0x34, 0x12];
        let nothing: Vec<String> = Vec::new();
        assert_eq!(split_output(&audio), (6, nothing.clone()));
        assert_eq!(split_output(&[]), (0, nothing.clone()));
        for line in [
            "stream node 58 unconnected",
            "stream node 58 error: target not found",
            "stream node 4294967295 error: stream is suspended",
        ] {
            let mut end = audio.to_vec();
            end.extend_from_slice(line.as_bytes());
            end.push(b'\n');
            let (kept, said) = split_output(&end);
            assert_eq!(&end[..kept], audio);
            assert_eq!(said, [line.to_owned()]);
        }
        // What Ubuntu 24.04 prints for a source that left: why, and then
        // that the stream is gone. Both go; the audio stays.
        let mut end = audio.to_vec();
        end.extend_from_slice(
            b"stream node 56 error: target not found\nstream node 4294967295 unconnected\n",
        );
        let (kept, said) = split_output(&end);
        assert_eq!(&end[..kept], audio);
        assert_eq!(
            said,
            [
                "stream node 56 error: target not found".to_owned(),
                "stream node 4294967295 unconnected".to_owned(),
            ]
        );
        // Audio that happens to hold the words is audio, unless the very
        // end is whole lines.
        for end in [
            &b"stream node 58 unconnected"[..],
            &b"stream node x error: y\n"[..],
            &b"stream node 58 error: \n"[..],
            &b"stream node 58 streaming\n"[..],
            &b"stream node 58 unconnected\nstream node 58"[..],
            &b"stream node 58\n"[..],
        ] {
            assert_eq!(
                split_output(end),
                (end.len(), nothing.clone()),
                "{:?}",
                String::from_utf8_lossy(end)
            );
        }
        // The audio before the lines may hold the words too: `stream node 58 ` is
        // fifteen bytes of it.
        let both = b"stream node 58 stream node 7 unconnected\n";
        assert_eq!(
            split_output(both),
            (15, vec!["stream node 7 unconnected".to_owned()])
        );
        // Of lines longer than the window only what is in the window is
        // told: a caller gives the last `OUTPUT_TAIL` bytes.
        let last = "stream node 1 unconnected\n";
        let long = format!("stream node 58 error: {}\n{last}", "x".repeat(OUTPUT_TAIL));
        let end = long.as_bytes()[long.len() - OUTPUT_TAIL..].to_vec();
        assert_eq!(
            split_output(&end),
            (
                end.len() - last.len(),
                vec!["stream node 1 unconnected".to_owned()]
            )
        );
    }

    #[test]
    fn recorder_output_is_read_the_same_by_dictation_and_meetings() {
        assert!(recorder_fatal(
            "error: failed to create stream: no such source"
        ));
        assert!(recorder_fatal("remote error: destroyed"));
        assert!(recorder_fatal("stream node 41 unconnected"));
        assert!(recorder_fatal("stream node 41 error: not linked"));
        assert!(!recorder_fatal("stream node 41 streaming"));
        assert!(!recorder_fatal("recorder microphone: started"));

        assert!(is_permission(
            "error: failed to create stream: Permission denied"
        ));
        assert!(is_permission("remote error: not allowed"));
        assert!(is_permission("access denied: stream.capture.sink"));
        assert!(!is_permission("stream node 41 unconnected"));
        assert!(!is_permission("recorder microphone: started"));
    }

    #[test]
    fn what_a_recorder_said_names_a_denial_and_what_it_died_of() {
        let running = Diagnostic::default()
            .with_lines(["recording: started", "stream node 41 streaming"].map(String::from));
        assert_eq!(running, Diagnostic::default());
        // The first fatal line is the reason, on stderr as on the end of the output.
        let lost = Diagnostic::default().with_lines(["stream node 58 streaming"].map(String::from));
        assert_eq!(
            lost.clone()
                .with_output(b"\x01\x00stream node 58 unconnected\n"),
            Diagnostic {
                fatal: Some("stream node 58 unconnected".to_owned()),
                permission: false,
            }
        );
        assert_eq!(
            lost.clone().with_output(b"\x01\x00\x02\x00"),
            Diagnostic::default()
        );
        let denied = Diagnostic::default()
            .with_lines(["error: failed to create stream: Permission denied"].map(String::from));
        assert!(denied.permission);

        let observed = io::Error::other("recorder exited with exit status: 1");
        let unconnected = lost
            .clone()
            .with_output(b"stream node 58 error: target not found\n")
            .reported(&observed);
        assert_eq!(unconnected.kind(), io::ErrorKind::Other);
        assert_eq!(
            unconnected.to_string(),
            "stream node 58 error: target not found"
        );
        // Nothing was said: what was observed stands.
        assert_eq!(
            running.reported(&observed).to_string(),
            "recorder exited with exit status: 1"
        );
        // A denial says where it is decided.
        let refusal = denied.reported(&observed);
        assert_eq!(refusal.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            refusal.to_string(),
            format!(
                "error: failed to create stream: Permission denied; {}",
                remedy()
            )
        );
    }

    #[test]
    fn only_the_exits_a_recorder_is_known_for_count_as_a_stop() {
        let pw_record = RecorderBackend::PwRecord { raw: false };
        assert!(stopped_as_asked(&ExitStatus::from_raw(0), pw_record));
        // PipeWire 1.6 leaves EXIT_FAILURE even after a requested SIGINT,
        // and dies from the signal on other versions.
        assert!(stopped_as_asked(&ExitStatus::from_raw(1 << 8), pw_record));
        assert!(stopped_as_asked(
            &ExitStatus::from_raw(libc::SIGINT),
            pw_record
        ));
        assert!(!stopped_as_asked(&ExitStatus::from_raw(2 << 8), pw_record));
        assert!(!stopped_as_asked(
            &ExitStatus::from_raw(libc::SIGKILL),
            pw_record
        ));
        // `gst-launch-1.0` ends with success, or from a failure that is not a stop.
        assert!(stopped_as_asked(
            &ExitStatus::from_raw(libc::SIGINT),
            RecorderBackend::Gstreamer
        ));
        assert!(!stopped_as_asked(
            &ExitStatus::from_raw(1 << 8),
            RecorderBackend::Gstreamer
        ));
    }
}
