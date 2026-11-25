//! PipeWire microphones for the desktop settings: which ones exist, which
//! one the system default names and which node actually feeds this
//! process's capture stream.
//!
//! Everything comes from one `pw-dump` of the PipeWire graph, and nothing
//! is inferred from names or labels. What a microphone is, and which node a
//! stream may be pointed at, is `capture::recorder`'s to say, the same for
//! these settings, the dictation capture and the meeting tracks: the
//! microphones are the `Audio/Source` nodes (the `Virtual` subtype among
//! them) that can be targeted. The system default is the
//! `default.audio.source` entry of the `default` metadata, as the session
//! manager keeps it. The capture source is the node on the far end of a
//! link in state `active` that enters the stream named by
//! [`capture_node_name`]; a sink monitor or another client's stream is
//! reported under its own name, like a microphone would be. A stream that
//! is absent, unlinked or paused reports nothing.
//!
//! `Microphone::id` is the `node.name` stored in `[audio] source`.
//! `object.serial` is what `pw-record --target` takes. It lasts only for
//! the current PipeWire run, so [`require_source`] reads the graph again
//! before capture and apply.
//!
//! Failure behaviour: both fail with the reason `pw-dump` could not read
//! the graph (absent, no PipeWire daemon, timeout, unparsable output). A
//! microphone that has disappeared is never replaced by another one:
//! [`require_source`] names it instead.

use serde::Serialize;
use serde_json::Value;

use crate::capture::recorder::{self, Selectable};

/// CLI that prints the PipeWire graph as JSON. It ships with `pw-record`.
const PW_DUMP: &str = "pw-dump";
/// Start of every reason the graph could not be read.
const UNAVAILABLE: &str = "microphones are unavailable: ";
/// What to say about a microphone that cannot be captured from.
const MISSING: &str = "is not available; connect it or choose another microphone";

/// The microphones PipeWire offers and which one a capture reads.
///
/// Serialized as `{"microphones": [{"id": "…", "label": "…"}],
/// "system_default": "…", "capturing": "…"}`, the last two `null` when
/// there is none.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AudioDevices {
    /// Every selectable source node, sorted by label.
    pub microphones: Vec<Microphone>,
    /// `id` of the microphone the system default names, `None` when
    /// PipeWire has no default audio source. It is what the metadata says,
    /// so it can name a microphone that has just left.
    pub system_default: Option<String>,
    /// `node.name` of the node an active link feeds this process's capture
    /// stream from: a microphone, a virtual source or a sink's monitor.
    /// `None` while nothing is captured.
    pub capturing: Option<String>,
}

/// One selectable microphone.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Microphone {
    /// PipeWire `node.name`, the same across daemon restarts and replugging.
    pub id: String,
    /// `node.description`, else `node.nick`, else the id.
    pub label: String,
}

/// `node.name` this process gives its `pw-record` stream, so that the graph
/// tells that stream apart from every other client's.
pub(crate) fn capture_node_name() -> String {
    format!("voiceless-capture-{}", std::process::id())
}

/// The microphones, the system default and what the capture stream reads.
pub(crate) async fn audio_devices() -> Result<AudioDevices, String> {
    let objects = dump_graph().await?;
    let stream = capture_node_name();
    Ok(Graph::parse(&objects).devices(&stream))
}

/// The serial to point `pw-record --target` at for the selected node name.
///
/// Reading the graph again is the point: a microphone that has disappeared
/// since it was selected is an error naming it, never a move to another one.
pub(crate) async fn require_source(device: &str) -> Result<u64, String> {
    let objects = dump_graph().await?;
    Graph::parse(&objects).source_serial(device)
}

/// One `pw-dump` of the graph, or the reason it could not be read.
async fn dump_graph() -> Result<Vec<Value>, String> {
    crate::capture::recorder::dump(PW_DUMP.as_ref())
        .await
        .map_err(unavailable)
}

/// The reason the graph could not be read, as the settings tell it.
fn unavailable(reason: String) -> String {
    format!("{UNAVAILABLE}{reason}")
}

/// The nodes, links and default-source entry of one dump.
struct Graph<'a> {
    /// Every named node, in dump order.
    nodes: Vec<Node<'a>>,
    /// Every link, in dump order.
    links: Vec<Link>,
    /// `node.name` the `default.audio.source` metadata names, when set.
    default_source: Option<&'a str>,
}

/// A node as links see it, and as a recording could be pointed at it.
struct Node<'a> {
    /// Global object `id`: what links refer to. It is reused once the node
    /// is gone, which is why nothing user-facing keeps it.
    id: u64,
    /// `node.name`.
    name: &'a str,
    /// The node as a stream's target: `Some` for a source or a sink that
    /// has an `object.serial`, `None` for streams and for nodes without one.
    selectable: Option<Selectable<'a>>,
}

/// A link between two nodes' ports, by global node `id`.
struct Link {
    /// The node the audio leaves.
    output: u64,
    /// The node the audio enters.
    input: u64,
    /// State `active`: the only one in which audio flows.
    active: bool,
}

impl<'a> Graph<'a> {
    /// Pick the nodes, links and default out of one `pw-dump` list. An
    /// object that lacks the fields its kind needs is skipped, not an
    /// error: it names and connects nothing.
    fn parse(objects: &'a [Value]) -> Self {
        let mut graph = Self {
            nodes: Vec::new(),
            links: Vec::new(),
            default_source: None,
        };
        for object in objects {
            match object.get("type").and_then(Value::as_str) {
                Some("PipeWire:Interface:Node") => graph.nodes.extend(Node::parse(object)),
                Some("PipeWire:Interface:Link") => graph.links.extend(Link::parse(object)),
                Some("PipeWire:Interface:Metadata") if is_default_metadata(object) => {
                    graph.default_source = default_source(object);
                }
                _ => {}
            }
        }
        graph
    }

    fn devices(&self, stream: &str) -> AudioDevices {
        AudioDevices {
            microphones: self.microphones(),
            system_default: self.default_source.map(str::to_owned),
            capturing: self.capturing(stream),
        }
    }

    /// Every source node that can also be targeted, sorted by label so that
    /// the settings list keeps its order as devices come and go.
    fn microphones(&self) -> Vec<Microphone> {
        let mut microphones: Vec<_> = self
            .nodes
            .iter()
            .filter_map(Node::source)
            .map(|node| Microphone {
                id: node.name.to_owned(),
                label: node.label.clone(),
            })
            .collect();
        microphones.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));
        microphones
    }

    /// The node an active link feeds the stream called `stream` from.
    fn capturing(&self, stream: &str) -> Option<String> {
        self.links
            .iter()
            .filter(|link| link.active && self.enters(link, stream))
            .find_map(|link| self.nodes.iter().find(|node| node.id == link.output))
            .map(|node| node.name.to_owned())
    }

    /// Whether `link` enters the node called `stream`.
    fn enters(&self, link: &Link, stream: &str) -> bool {
        self.nodes
            .iter()
            .any(|node| node.id == link.input && node.name == stream)
    }

    fn source_serial(&self, device: &str) -> Result<u64, String> {
        self.nodes
            .iter()
            .filter(|node| node.name == device)
            .find_map(Node::source)
            .map(|node| node.serial)
            .ok_or_else(|| format!("selected microphone {device:?} {MISSING}"))
    }
}

impl<'a> Node<'a> {
    /// The node if links can point at it and it can be named.
    fn parse(object: &'a Value) -> Option<Self> {
        Some(Self {
            id: object.get("id")?.as_u64()?,
            name: object.pointer("/info/props/node.name")?.as_str()?,
            selectable: recorder::selectable(object),
        })
    }

    /// The node as a microphone: an `Audio/Source` that can be targeted.
    /// A sink is a target of its own kind, the one a meeting records
    /// through its monitor, and is not a microphone.
    fn source(&self) -> Option<&Selectable<'a>> {
        self.selectable.as_ref().filter(|node| !node.monitor)
    }
}

impl Link {
    /// The link if it names both of its nodes.
    fn parse(object: &Value) -> Option<Self> {
        let info = object.get("info")?;
        Some(Self {
            output: info.get("output-node-id")?.as_u64()?,
            input: info.get("input-node-id")?.as_u64()?,
            active: info.get("state")?.as_str()? == "active",
        })
    }
}

/// Whether this is the `default` metadata object, where the session manager
/// keeps the default sink and source. Metadata objects print their props at
/// the top level, not under `info` as nodes do.
fn is_default_metadata(metadata: &Value) -> bool {
    metadata
        .pointer("/props/metadata.name")
        .and_then(Value::as_str)
        == Some("default")
}

/// The node the `default.audio.source` entry of the `default` metadata names.
fn default_source(metadata: &Value) -> Option<&str> {
    metadata
        .get("metadata")?
        .as_array()?
        .iter()
        .filter(|entry| entry.get("key").and_then(Value::as_str) == Some("default.audio.source"))
        .find_map(|entry| named_node(entry.get("value")?))
}

/// The `node.name` in a `default.audio.source` value: an object with `name`
/// (the session manager adds `force` once the user pinned the choice) or,
/// from older session managers, the bare name.
fn named_node(value: &Value) -> Option<&str> {
    let name = match value {
        Value::String(_) => value.as_str(),
        Value::Object(fields) => fields.get("name").and_then(Value::as_str),
        _ => None,
    }?;
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HEADSET: &str = "alsa_input.usb-Fancy_Headset-00.mono-fallback";
    const MAINBOARD: &str = "alsa_input.pci-0000_0c_00.4.analog-stereo";
    /// The mainboard's output: the same label as its input, not a microphone.
    const SPEAKERS: &str = "alsa_output.pci-0000_0c_00.4.analog-stereo";
    const STREAM: &str = "voiceless-capture-4242";
    const HEADSET_SERIAL: u64 = 90;
    const MAINBOARD_SERIAL: u64 = 95;

    /// A node as `pw-dump` prints it: `props` is its `info.props`.
    fn node(id: u64, props: Value) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Node", "info": {"props": props}})
    }

    fn headset() -> Value {
        let props = json!({
            "node.name": HEADSET,
            "media.class": "Audio/Source",
            "node.description": "Lian II Mono",
            "node.nick": "Lian II",
            "object.serial": HEADSET_SERIAL,
        });
        node(61, props)
    }

    fn mainboard() -> Value {
        let props = json!({
            "node.name": MAINBOARD,
            "media.class": "Audio/Source",
            "node.description": "Starship/Matisse Analog Stereo",
            "object.serial": MAINBOARD_SERIAL,
        });
        node(64, props)
    }

    fn speakers() -> Value {
        let props = json!({
            "node.name": SPEAKERS,
            "media.class": "Audio/Sink",
            "node.description": "Starship/Matisse Analog Stereo",
            "object.serial": 55,
        });
        node(55, props)
    }

    /// A virtual source that has a nick but no description.
    fn echo_cancel() -> Value {
        let props = json!({
            "node.name": "echo-cancel-source",
            "media.class": "Audio/Source/Virtual",
            "node.nick": "Echo Cancel Source",
            "object.serial": 101,
        });
        node(70, props)
    }

    /// A source that has neither a description nor a nick.
    fn bare_source() -> Value {
        let props = json!({
            "node.name": "test-source",
            "media.class": "Audio/Source",
            "object.serial": 102,
        });
        node(71, props)
    }

    /// Another client's capture stream.
    fn other_stream() -> Value {
        let props = json!({
            "node.name": "audio-input-zoom",
            "media.class": "Stream/Input/Audio",
            "node.description": "ZoomMicrophone",
            "object.serial": 130,
        });
        node(74, props)
    }

    /// This process's capture stream, as `pw-record` names it.
    fn capture_stream() -> Value {
        let props = json!({
            "node.name": STREAM,
            "media.class": "Stream/Input/Audio",
            "object.serial": 80,
        });
        node(64, props)
    }

    /// A metadata object as `pw-dump` prints it, with one entry.
    fn metadata(name: &str, key: &str, value: Value) -> Value {
        json!({
            "id": 42,
            "type": "PipeWire:Interface:Metadata",
            "props": {"metadata.name": name},
            "metadata": [{"subject": 0, "key": key, "type": "Spa:String:JSON", "value": value}],
        })
    }

    /// The `default` metadata object with one `default.audio.source` entry.
    fn default_metadata(value: Value) -> Value {
        metadata("default", "default.audio.source", value)
    }

    /// A link as `pw-dump` prints it, between one output and one input port.
    fn link(output: u64, input: u64, state: &str) -> Value {
        json!({
            "id": 70,
            "type": "PipeWire:Interface:Link",
            "info": {
                "output-node-id": output,
                "output-port-id": output,
                "input-node-id": input,
                "input-port-id": input,
                "state": state,
                "error": null,
            },
        })
    }

    #[test]
    fn microphones_are_the_source_nodes_sorted_by_label() {
        // A source without a serial cannot be targeted, so it is not offered.
        let serial_less = node(
            75,
            json!({"node.name": "serial-less", "media.class": "Audio/Source"}),
        );
        let objects = [
            node(31, json!({"node.name": "Dummy-Driver"})),
            speakers(),
            headset(),
            mainboard(),
            echo_cancel(),
            bare_source(),
            other_stream(),
            serial_less,
        ];
        let graph = Graph::parse(&objects);
        let expected = json!({
            "microphones": [
                {"id": "echo-cancel-source", "label": "Echo Cancel Source"},
                {"id": HEADSET, "label": "Lian II Mono"},
                {"id": MAINBOARD, "label": "Starship/Matisse Analog Stereo"},
                {"id": "test-source", "label": "test-source"},
            ],
            "system_default": null,
            "capturing": null,
        });
        let devices = serde_json::to_value(graph.devices(STREAM)).unwrap();
        assert_eq!(devices, expected);
    }

    #[test]
    fn system_default_is_the_default_metadata_entry() {
        let default_of = |objects: &[Value]| Graph::parse(objects).devices(STREAM).system_default;
        // The session manager writes an object, with `force` next to the
        // name once the user has pinned the default.
        let pinned = default_metadata(json!({"name": MAINBOARD, "force": true}));
        assert_eq!(default_of(&[pinned]), Some(MAINBOARD.to_owned()));
        // Older session managers stored the bare name.
        let bare = default_metadata(json!(HEADSET));
        assert_eq!(default_of(&[bare]), Some(HEADSET.to_owned()));
        // Only the `default` object counts: this is the saved preference,
        // not what is in effect.
        let saved = metadata("sm-settings", "default.audio.source", json!(HEADSET));
        assert_eq!(default_of(&[saved]), None);
        // A default sink alone leaves no default microphone.
        let sink = metadata("default", "default.audio.sink", json!({"name": SPEAKERS}));
        assert_eq!(default_of(&[sink]), None);
    }

    #[test]
    fn the_selected_microphone_is_targeted_whatever_the_default_is() {
        let objects = [
            headset(),
            mainboard(),
            default_metadata(json!({"name": MAINBOARD})),
        ];
        let graph = Graph::parse(&objects);
        assert_eq!(graph.source_serial(HEADSET), Ok(HEADSET_SERIAL));
        assert_eq!(graph.source_serial(MAINBOARD), Ok(MAINBOARD_SERIAL));
    }

    #[test]
    fn an_unplugged_selected_microphone_is_reported_not_replaced() {
        // The headset left the graph; the mainboard is still the default.
        let objects = [mainboard(), default_metadata(json!({"name": MAINBOARD}))];
        let graph = Graph::parse(&objects);
        let gone = format!("selected microphone {HEADSET:?} {MISSING}");
        assert_eq!(graph.source_serial(HEADSET).unwrap_err(), gone);
    }

    #[test]
    fn capturing_is_what_an_active_link_feeds_the_named_stream_from() {
        let capture = capture_stream();
        // `capturing` for a graph made of the capture stream, `nodes`, `links`.
        let capturing = |nodes: &[Value], links: &[Value]| {
            let objects = [capture.clone()]
                .into_iter()
                .chain(nodes.iter().cloned())
                .chain(links.iter().cloned())
                .collect::<Vec<_>>();
            Graph::parse(&objects).devices(STREAM).capturing
        };
        // Unlinked (`pw-record --target 0`), which is also what is left
        // when the source leaves mid-capture: its links go with it.
        assert_eq!(capturing(&[], &[]), None);
        // Linked, but not flowing yet.
        assert_eq!(capturing(&[headset()], &[link(61, 64, "paused")]), None);
        // Flowing from the headset.
        let active = [link(61, 64, "active")];
        assert_eq!(capturing(&[headset()], &active), Some(HEADSET.to_owned()));
        // A sink's monitor keeps its own name.
        let sink = [link(55, 64, "active")];
        assert_eq!(capturing(&[speakers()], &sink), Some(SPEAKERS.to_owned()));
        // A link whose source is missing from the dump names nothing.
        assert_eq!(capturing(&[], &active), None);
        // Another client's stream, and the monitor ports of our own, are
        // not this stream's input.
        let stray = [link(61, 74, "active"), link(64, 74, "active")];
        assert_eq!(capturing(&[headset(), other_stream()], &stray), None);
    }

    /// A finished run with this wait status, as `pw-dump` leaves it.
    #[cfg(unix)]
    fn ran(raw_status: i32, stdout: &[u8], stderr: &[u8]) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(raw_status),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    /// What `dump_graph` makes of a finished `pw-dump`.
    fn decoded(output: std::process::Output) -> Result<Vec<Value>, String> {
        crate::capture::recorder::decode(PW_DUMP.as_ref(), output).map_err(unavailable)
    }

    #[test]
    #[cfg(unix)]
    fn a_dump_that_did_not_print_the_graph_is_rejected() {
        // No session daemon: the tool explains itself.
        let down = decoded(ran(255 << 8, b"", b"can't connect: Host is down\n"));
        let refused = format!("{UNAVAILABLE}pw-dump failed: can't connect: Host is down");
        assert_eq!(down, Err(refused));
        // Killed: the exit status is all there is to say.
        let killed = decoded(ran(9, b"", b""));
        assert_eq!(
            killed,
            Err("microphones are unavailable: pw-dump failed: signal: 9 (SIGKILL)".to_owned())
        );
        let graph = decoded(ran(0, b"[{\"id\": 0}]\n", b""));
        assert_eq!(graph, Ok(vec![json!({"id": 0})]));
        let broken = decoded(ran(0, b"", b"")).unwrap_err();
        assert!(
            broken.starts_with("microphones are unavailable: pw-dump printed invalid JSON: "),
            "{broken}"
        );
        // Well-formed, but not what `pw-dump` prints.
        let foreign = decoded(ran(0, b"{\"id\": 0}", b""));
        let shape = format!("{UNAVAILABLE}pw-dump printed something other than the PipeWire graph");
        assert_eq!(foreign, Err(shape));
    }
}
