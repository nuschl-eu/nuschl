// SPDX-License-Identifier: EUPL-1.2
//! Which playback streams the desktop audio share records.
//!
//! The share links application playback streams into its own capture node.
//! It never links Nuschl's streams: those of an application named Nuschl
//! (Nuschl's audio processes say so), of a process Nuschl started (its
//! sandbox runs every worker as `/worker`), or of this process's own
//! PipeWire connection (`pipewire.sec.pid`, set by the daemon). Audio flows
//! on from there: into sinks, filter chains and every recording made of
//! them. A stream is left out when it comes from a module's filter chain or
//! loopback (`node.link-group`; the streams it carries are linked
//! themselves), or when its application records anything other than an
//! unaffected source, because it may play back what it records. A node,
//! client or link whose description has not arrived counts against the
//! stream it touches.

use std::collections::{BTreeMap, BTreeSet};

pub const PLAYBACK: &str = "Stream/Output/Audio";
const RECORDING: &str = "Stream/Input/Audio";
const SOURCE: &str = "Audio/Source";

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub class: String,
    pub client: Option<u32>,
    pub link_group: Option<String>,
    pub application: String,
    pub binary: String,
    /// The node's full description has arrived.
    pub ready: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Client {
    pub pid: Option<String>,
    pub application: String,
    pub binary: String,
    pub ready: bool,
}

#[derive(Clone, Debug)]
pub struct Port {
    pub node: u32,
    pub output: bool,
    pub monitor: bool,
    pub channel: String,
}

#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub output: u32,
    pub input: u32,
    pub client: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: BTreeMap<u32, Node>,
    pub clients: BTreeMap<u32, Client>,
    pub ports: BTreeMap<u32, Port>,
    pub links: BTreeMap<u32, Link>,
}

fn named_nuschl(application: &str, binary: &str) -> bool {
    application == "Nuschl" || binary == "worker" || binary.starts_with("nuschl")
}

impl Graph {
    /// The capture node's input ports for the left and right channel.
    pub fn inputs(&self, capture: u32) -> Option<[u32; 2]> {
        let port = |channel| {
            self.ports.iter().find_map(|(&id, port)| {
                (port.node == capture && !port.output && port.channel == channel).then_some(id)
            })
        };
        Some([port("FL")?, port("FR")?])
    }

    /// The client of the capture node: this process's own connection.
    fn own(&self, capture: u32) -> Option<&Client> {
        self.clients.get(&self.nodes.get(&capture)?.client?)
    }

    fn nuschl(&self, node: &Node, own: Option<&str>) -> bool {
        let client = node.client.and_then(|id| self.clients.get(&id));
        named_nuschl(&node.application, &node.binary)
            || client.is_some_and(|client| {
                named_nuschl(&client.application, &client.binary)
                    || (own.is_some() && client.pid.as_deref() == own)
            })
    }

    /// Nodes that may carry Nuschl's audio.
    fn tainted(&self, capture: u32) -> BTreeSet<u32> {
        let own = self.own(capture).and_then(|client| client.pid.as_deref());
        let mut tainted: BTreeSet<u32> = self
            .nodes
            .iter()
            .filter(|(id, node)| **id == capture || self.nuschl(node, own))
            .map(|(id, _)| *id)
            .collect();
        loop {
            let before = tainted.len();
            for link in self.links.values() {
                if tainted.contains(&link.output) {
                    tainted.insert(link.input);
                }
            }
            let groups: BTreeSet<&str> = tainted
                .iter()
                .filter_map(|id| self.nodes.get(id)?.link_group.as_deref())
                .collect();
            for (id, node) in &self.nodes {
                if node
                    .link_group
                    .as_deref()
                    .is_some_and(|group| groups.contains(group))
                {
                    tainted.insert(*id);
                }
            }
            if tainted.len() == before {
                return tainted;
            }
        }
    }

    /// Clients that record a stream, a sink, Nuschl or an unknown node.
    fn replaying(&self, tainted: &BTreeSet<u32>) -> BTreeSet<u32> {
        self.links
            .values()
            .filter_map(|link| {
                let recorder = self.nodes.get(&link.input)?;
                let fed = self.nodes.get(&link.output);
                (recorder.class == RECORDING
                    && (tainted.contains(&link.output)
                        || fed.is_none_or(|fed| !fed.ready || fed.class != SOURCE)))
                .then_some(recorder.client)?
            })
            .collect()
    }

    fn eligible(&self, node: &Node, tainted: bool, replaying: &BTreeSet<u32>) -> bool {
        node.ready
            && node.class == PLAYBACK
            && node.link_group.is_none()
            && !tainted
            && node.client.is_some_and(|client| {
                !replaying.contains(&client)
                    && self.clients.get(&client).is_some_and(|client| client.ready)
            })
    }

    /// The links from playback ports to the capture node's `inputs` (left,
    /// right) that the share wants, as (output port, input port).
    pub fn wanted(&self, capture: u32, inputs: [u32; 2]) -> BTreeSet<(u32, u32)> {
        let tainted = self.tainted(capture);
        let replaying = self.replaying(&tainted);
        self.ports
            .iter()
            .filter(|(_, port)| port.output && !port.monitor)
            .filter(|(_, port)| {
                self.nodes.get(&port.node).is_some_and(|node| {
                    self.eligible(node, tainted.contains(&port.node), &replaying)
                })
            })
            .flat_map(|(&id, port)| {
                let sides: &[usize] = match port.channel.as_str() {
                    "FL" | "SL" | "RL" => &[0],
                    "FR" | "SR" | "RR" => &[1],
                    _ => &[0, 1],
                };
                sides.iter().map(move |&side| (id, inputs[side]))
            })
            .collect()
    }

    /// A link into the capture node that another client made.
    pub fn foreign(&self, capture: u32) -> Option<u32> {
        let own = self.nodes.get(&capture)?.client?;
        self.links
            .iter()
            .find(|(_, link)| link.input == capture && link.client != Some(own))
            .map(|(id, _)| *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: u32 = 10;
    const INPUTS: [u32; 2] = [11, 12];
    const OWN: u32 = 1;

    fn client(application: &str, binary: &str, pid: &str) -> Client {
        Client {
            pid: Some(pid.into()),
            application: application.into(),
            binary: binary.into(),
            ready: true,
        }
    }

    fn node(class: &str, client: u32, application: &str) -> Node {
        Node {
            class: class.into(),
            client: Some(client),
            application: application.into(),
            ready: true,
            ..Node::default()
        }
    }

    /// The share's own node, speakers and a microphone.
    fn desktop() -> Graph {
        let mut graph = Graph::default();
        graph
            .clients
            .insert(OWN, client("worker", "worker", "4242"));
        graph
            .clients
            .insert(2, client("WirePlumber", "wireplumber", "100"));
        graph.nodes.insert(CAPTURE, node(RECORDING, OWN, "Nuschl"));
        graph.nodes.insert(20, node("Audio/Sink", 2, "WirePlumber"));
        graph.nodes.insert(21, node(SOURCE, 2, "WirePlumber"));
        port(&mut graph, INPUTS[0], CAPTURE, false, "FL");
        port(&mut graph, INPUTS[1], CAPTURE, false, "FR");
        port(&mut graph, 22, 20, false, "FL");
        port(&mut graph, 23, 20, true, "FL");
        port(&mut graph, 24, 21, true, "MONO");
        graph
    }

    fn port(graph: &mut Graph, id: u32, node: u32, output: bool, channel: &str) {
        graph.ports.insert(
            id,
            Port {
                node,
                output,
                monitor: false,
                channel: channel.into(),
            },
        );
    }

    fn link(graph: &mut Graph, id: u32, output: u32, input: u32, client: u32) {
        graph.links.insert(
            id,
            Link {
                output,
                input,
                client: Some(client),
            },
        );
    }

    /// An application `id` with a stereo playback stream into the speakers.
    fn player(graph: &mut Graph, id: u32, application: &str, binary: &str, pid: &str) {
        graph.clients.insert(id, client(application, binary, pid));
        graph.nodes.insert(id * 10, node(PLAYBACK, id, application));
        port(graph, id * 10 + 1, id * 10, true, "FL");
        port(graph, id * 10 + 2, id * 10, true, "FR");
        link(graph, id * 10 + 3, id * 10, 20, 2);
    }

    fn wanted(graph: &Graph) -> BTreeSet<(u32, u32)> {
        graph.wanted(CAPTURE, graph.inputs(CAPTURE).unwrap())
    }

    fn stereo(node: u32) -> BTreeSet<(u32, u32)> {
        [(node + 1, INPUTS[0]), (node + 2, INPUTS[1])].into()
    }

    #[test]
    fn applications_are_linked_and_nuschls_streams_are_not() {
        let mut graph = desktop();
        player(&mut graph, 5, "Firefox", "firefox", "500");
        // Nuschl's voice, a sandboxed Nuschl chime, a development build, and
        // a stream from this process's own connection.
        player(&mut graph, 6, "Nuschl", "worker", "600");
        player(&mut graph, 7, "", "worker", "700");
        player(&mut graph, 8, "Nuschl", "nuschl-media-worker", "800");
        player(&mut graph, 9, "Shared audio", "player", "4242");
        // A stream that names itself Nuschl on its node only.
        player(&mut graph, 3, "Music", "music", "300");
        graph.nodes.get_mut(&30).unwrap().application = "Nuschl".into();
        assert_eq!(wanted(&graph), stereo(50));
    }

    #[test]
    fn new_streams_are_linked_once_described_and_departed_ones_unlinked() {
        let mut graph = desktop();
        player(&mut graph, 5, "Firefox", "firefox", "500");
        let before = wanted(&graph);
        player(&mut graph, 6, "mpv", "mpv", "600");
        graph.clients.get_mut(&6).unwrap().ready = false;
        assert_eq!(
            wanted(&graph),
            before,
            "its client's description is pending"
        );
        graph.clients.get_mut(&6).unwrap().ready = true;
        graph.nodes.get_mut(&60).unwrap().ready = false;
        assert_eq!(wanted(&graph), before, "its node's description is pending");
        graph.nodes.get_mut(&60).unwrap().ready = true;
        let after = wanted(&graph);
        assert_eq!(after, &before | &stereo(60));
        // The player quits: its node and ports go away, and so do its links.
        graph.nodes.remove(&50);
        graph.ports.retain(|_, port| port.node != 50);
        assert_eq!(wanted(&graph), stereo(60));
        assert!(before.is_disjoint(&wanted(&graph)));
    }

    #[test]
    fn mono_and_surround_reach_both_sides() {
        let mut graph = desktop();
        graph.clients.insert(5, client("Game", "game", "500"));
        graph.nodes.insert(50, node(PLAYBACK, 5, "Game"));
        port(&mut graph, 51, 50, true, "MONO");
        port(&mut graph, 52, 50, true, "RR");
        port(&mut graph, 53, 50, true, "LFE");
        graph.ports.insert(
            54,
            Port {
                node: 50,
                output: true,
                monitor: true,
                channel: "FL".into(),
            },
        );
        assert_eq!(
            wanted(&graph),
            [
                (51, INPUTS[0]),
                (51, INPUTS[1]),
                (52, INPUTS[1]),
                (53, INPUTS[0]),
                (53, INPUTS[1])
            ]
            .into()
        );
    }

    #[test]
    fn replayed_audio_and_filter_chains_are_left_out() {
        let mut graph = desktop();
        player(&mut graph, 5, "Firefox", "firefox", "500");
        player(&mut graph, 6, "Nuschl", "worker", "600");
        // OBS records the speakers (where Nuschl plays) and monitors it.
        player(&mut graph, 7, "OBS", "obs", "700");
        graph.nodes.insert(71, node(RECORDING, 7, "OBS"));
        link(&mut graph, 79, 20, 71, 2);
        // A call app records the microphone and plays the far end.
        player(&mut graph, 8, "Meet", "meet", "800");
        graph.nodes.insert(81, node(RECORDING, 8, "Meet"));
        link(&mut graph, 89, 21, 81, 2);
        // An echo canceller: Nuschl's voice reaches its sink, so its
        // playback carries Nuschl; its group shares one link-group.
        graph.clients.insert(3, client("pipewire", "pipewire", "1"));
        for (id, class) in [(90, "Audio/Sink"), (91, PLAYBACK), (92, SOURCE)] {
            let mut module = node(class, 3, "");
            module.link_group = Some("echo-cancel-1".into());
            graph.nodes.insert(id, module);
        }
        port(&mut graph, 93, 91, true, "FL");
        link(&mut graph, 94, 60, 90, 2);
        // An app that records the echo canceller's source.
        player(&mut graph, 4, "Talk", "talk", "400");
        graph.nodes.insert(41, node(RECORDING, 4, "Talk"));
        link(&mut graph, 49, 92, 41, 2);
        assert_eq!(wanted(&graph), &stereo(50) | &stereo(80));
        // A recording from a node not yet described counts as a replay.
        link(&mut graph, 88, 99, 81, 2);
        assert_eq!(wanted(&graph), stereo(50));
    }

    #[test]
    fn links_others_make_into_the_capture_node_are_found() {
        let mut graph = desktop();
        player(&mut graph, 5, "Firefox", "firefox", "500");
        link(&mut graph, 30, 50, CAPTURE, OWN);
        assert_eq!(graph.foreign(CAPTURE), None);
        // The session manager links the microphone.
        link(&mut graph, 31, 21, CAPTURE, 2);
        assert_eq!(graph.foreign(CAPTURE), Some(31));
    }
}
