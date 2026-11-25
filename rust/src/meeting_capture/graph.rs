//! One `pw-dump` of the PipeWire graph, read as a meeting needs it: the
//! microphones, the outputs whose monitor carries what the system plays,
//! and which of each the system default names. What may be recorded is
//! `capture::recorder`'s to say, the same rule as the dictation settings'
//! (`desktop::audio`): a microphone is a node whose `media.class` is
//! `Audio/Source` or a subtype such as `Audio/Source/Virtual`, an output is
//! one whose class is `Audio/Sink` or a subtype, and only a node with an
//! `object.serial` can be recorded from. Nothing is inferred from names or
//! labels.
//!
//! The recorder is pointed at the `object.serial`, which lasts only for the
//! current PipeWire run, so the graph is read again right before every
//! start. A node that left since it was selected is reported as missing,
//! never replaced by another one.
//!
//! Failure behaviour: `read` fails with the reason the graph could not be
//! read (`pw-dump` absent, no PipeWire daemon, a daemon that does not
//! answer in five seconds, output that is not the graph). The tool is
//! `Tools::dump`, so tests name a stand-in and nothing here reads `PATH`.

use serde_json::Value;

use super::model::{SourceCatalog, SourceRef, Tools};
use crate::capture::recorder::{self, selectable};

/// The nodes a meeting can record, and the system defaults, of one dump.
pub(super) struct Graph {
    /// Microphones and outputs that can be targeted, in dump order.
    nodes: Vec<Node>,
    /// `node.name` the `default.audio.source` metadata entry names.
    default_microphone: Option<String>,
    /// `node.name` the `default.audio.sink` metadata entry names.
    default_output: Option<String>,
}

/// A node as a target and a label see it.
struct Node {
    name: String,
    label: String,
    serial: u64,
    /// An output (`Audio/Sink`); else a microphone.
    output: bool,
}

impl Graph {
    pub(super) fn resolve(
        &self,
        kind: super::TrackKind,
        node: String,
    ) -> Result<super::TrackSpec, super::Failure> {
        let selected = match kind {
            super::TrackKind::Microphone => self.microphone(&node),
            super::TrackKind::System => self.output(&node),
        };
        match selected {
            Some((serial, label)) => Ok(super::TrackSpec {
                kind,
                node,
                label,
                serial,
            }),
            None => Err(super::Failure::SourceMissing { source: kind, node }),
        }
    }

    /// Pick the nodes and the two default entries out of one dump's
    /// objects. An object that lacks the fields its kind needs is skipped:
    /// it names and connects nothing.
    pub(super) fn parse(objects: &[Value]) -> Self {
        let mut graph = Self {
            nodes: Vec::new(),
            default_microphone: None,
            default_output: None,
        };
        for object in objects {
            match object.get("type").and_then(Value::as_str) {
                Some("PipeWire:Interface:Node") => graph.nodes.extend(Node::parse(object)),
                Some("PipeWire:Interface:Metadata") if is_default(object) => {
                    graph.read_defaults(object);
                }
                _ => {}
            }
        }
        graph
    }

    /// `default.audio.source` and `default.audio.sink`, which the session
    /// manager keeps in the `default` metadata.
    fn read_defaults(&mut self, metadata: &Value) {
        for entry in metadata
            .get("metadata")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match entry.get("key").and_then(Value::as_str) {
                Some("default.audio.source") => {
                    self.default_microphone = named(entry.get("value"));
                }
                Some("default.audio.sink") => self.default_output = named(entry.get("value")),
                _ => {}
            }
        }
    }

    /// The serial to record `name` with and its label, when it is a
    /// microphone.
    pub(super) fn microphone(&self, name: &str) -> Option<(u64, String)> {
        self.node(name, false)
    }

    /// The serial to record the monitor of `name` with and its label, when
    /// it is an output.
    pub(super) fn output(&self, name: &str) -> Option<(u64, String)> {
        self.node(name, true)
    }

    fn node(&self, name: &str, output: bool) -> Option<(u64, String)> {
        self.nodes
            .iter()
            .find(|node| node.output == output && node.name == name)
            .map(|node| (node.serial, node.label.clone()))
    }

    /// What the settings show.
    pub(super) fn catalog(&self) -> SourceCatalog {
        SourceCatalog {
            microphones: self.listed(false),
            system_outputs: self.listed(true),
            default_microphone: self.default_microphone.clone(),
            default_system_output: self.default_output.clone(),
        }
    }

    /// The nodes of one kind, sorted by label so that the list keeps its
    /// order as devices come and go.
    fn listed(&self, output: bool) -> Vec<SourceRef> {
        let mut nodes: Vec<SourceRef> = self
            .nodes
            .iter()
            .filter(|node| node.output == output)
            .map(|node| SourceRef {
                node: node.name.clone(),
                label: node.label.clone(),
            })
            .collect();
        nodes.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.node.cmp(&b.node)));
        nodes
    }
}

impl Node {
    /// The node if it can be targeted and named.
    fn parse(object: &Value) -> Option<Self> {
        let node = selectable(object)?;
        Some(Self {
            name: node.name.to_owned(),
            label: node.label,
            serial: node.serial,
            output: node.monitor,
        })
    }
}

/// Whether this is the `default` metadata object. Metadata objects print
/// their props at the top level, not under `info` as nodes do.
fn is_default(metadata: &Value) -> bool {
    metadata
        .pointer("/props/metadata.name")
        .and_then(Value::as_str)
        == Some("default")
}

/// The `node.name` in a default entry's value: an object with `name` (the
/// session manager adds `force` once the user pinned the choice) or, from
/// older session managers, the bare name.
fn named(value: Option<&Value>) -> Option<String> {
    let name = match value? {
        Value::String(name) => name.as_str(),
        Value::Object(fields) => fields.get("name")?.as_str()?,
        _ => return None,
    };
    (!name.is_empty()).then(|| name.to_owned())
}

/// One `pw-dump` of the graph, or the reason it could not be read.
pub(super) async fn read(tools: &Tools) -> Result<Graph, String> {
    Ok(Graph::parse(&recorder::dump(&tools.dump).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HEADSET: &str = "alsa_input.usb-Fancy_Headset-00.mono-fallback";
    const SPEAKERS: &str = "alsa_output.pci-0000_0c_00_4.analog-stereo";

    fn node(id: u64, props: Value) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Node", "info": {"props": props}})
    }

    #[test]
    fn a_meeting_targets_microphones_and_the_monitor_of_outputs() {
        let headset = node(
            61,
            json!({
                "node.name": HEADSET,
                "media.class": "Audio/Source",
                "node.description": "Lian II Mono",
                "object.serial": 90,
            }),
        );
        // A virtual source has a nick and no description.
        let echo_cancel = node(
            70,
            json!({
                "node.name": "echo-cancel-source",
                "media.class": "Audio/Source/Virtual",
                "node.nick": "Echo Cancel Source",
                "object.serial": 101,
            }),
        );
        let speakers = node(
            55,
            json!({
                "node.name": SPEAKERS,
                "media.class": "Audio/Sink",
                "node.description": "Starship/Matisse Analog Stereo",
                "object.serial": 55,
            }),
        );
        // Another client's stream and a source without a serial are not targets.
        let stream = node(
            74,
            json!({
                "node.name": "audio-input-zoom",
                "media.class": "Stream/Input/Audio",
                "node.description": "ZoomMicrophone",
                "object.serial": 130,
            }),
        );
        let bare = node(
            75,
            json!({"node.name": "test-source", "media.class": "Audio/Source"}),
        );
        let defaults = json!({
            "id": 42,
            "type": "PipeWire:Interface:Metadata",
            "props": {"metadata.name": "default"},
            "metadata": [
                {"key": "default.audio.source", "value": {"name": HEADSET, "force": true}},
                {"key": "default.audio.sink", "value": SPEAKERS},
            ],
        });
        let saved = json!({
            "id": 43,
            "type": "PipeWire:Interface:Metadata",
            "props": {"metadata.name": "sm-settings"},
            "metadata": [{"key": "default.audio.source", "value": {"name": "echo-cancel-source"}}],
        });
        let graph = Graph::parse(&[
            stream,
            speakers,
            echo_cancel,
            bare,
            headset,
            defaults,
            saved,
        ]);

        assert_eq!(
            graph.microphone(HEADSET),
            Some((90, "Lian II Mono".to_owned()))
        );
        assert_eq!(
            graph.microphone("echo-cancel-source"),
            Some((101, "Echo Cancel Source".to_owned()))
        );
        assert_eq!(graph.microphone("audio-input-zoom"), None);
        assert_eq!(graph.microphone("test-source"), None);
        assert_eq!(graph.microphone(SPEAKERS), None);
        assert_eq!(
            graph.output(SPEAKERS),
            Some((55, "Starship/Matisse Analog Stereo".to_owned()))
        );
        assert_eq!(graph.output(HEADSET), None);

        assert_eq!(
            graph.catalog(),
            SourceCatalog {
                microphones: vec![
                    SourceRef {
                        node: "echo-cancel-source".to_owned(),
                        label: "Echo Cancel Source".to_owned(),
                    },
                    SourceRef {
                        node: HEADSET.to_owned(),
                        label: "Lian II Mono".to_owned(),
                    },
                ],
                system_outputs: vec![SourceRef {
                    node: SPEAKERS.to_owned(),
                    label: "Starship/Matisse Analog Stereo".to_owned(),
                }],
                // The `default` object is what is in effect; `sm-settings` only a saved preference.
                default_microphone: Some(HEADSET.to_owned()),
                default_system_output: Some(SPEAKERS.to_owned()),
            }
        );
    }
}
