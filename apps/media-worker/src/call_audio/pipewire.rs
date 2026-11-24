// SPDX-License-Identifier: EUPL-1.2
//! The desktop audio share on PipeWire: a capture node of this worker's,
//! unlinked by the session manager, and the links into it that the policy
//! wants, made and removed as streams come and go. The links are this
//! connection's own, so PipeWire removes them when the share ends. Another
//! client's link into the node ends the share.

use super::policy::{self, Graph};
use pipewire as pw;
use pw::{properties::properties, spa, types::ObjectType};
use rtrb::Producer;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    rc::Rc,
    sync::mpsc,
    thread::{self, JoinHandle},
};

/// Objects a desktop's audio graph holds, well beyond real ones.
const MAX_OBJECTS: usize = 4096;
/// Links the share makes at most.
const MAX_LINKS: usize = 512;

pub struct Capture {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Start sharing into `samples` (interleaved stereo at 48 kHz). `failed`
    /// is called once if the share ends by itself.
    pub fn start(
        samples: Producer<f32>,
        failed: impl FnOnce(String) + Send + 'static,
    ) -> Result<Self, String> {
        let (stop, stopped) = pw::channel::channel();
        let (started, ready) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("share-audio".into())
            .spawn(move || {
                let mut started = Some(started);
                if let Err(reason) = run(samples, stopped, &mut started) {
                    match started {
                        Some(started) => {
                            let _ = started.send(Err(reason));
                        }
                        None => failed(reason),
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(reason)) => {
                let _ = thread.join();
                Err(reason)
            }
            Err(_) => {
                let _ = thread.join();
                Err("Audio sharing stopped".into())
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct State {
    graph: Graph,
    nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    clients: HashMap<u32, (pw::client::Client, pw::client::ClientListener)>,
    links: BTreeMap<(u32, u32), pw::link::Link>,
    /// Every link's (output port, input port), by its global id.
    link_ports: HashMap<u32, (u32, u32)>,
    failure: Option<String>,
}

impl State {
    fn fail(&mut self, reason: &str) {
        self.failure.get_or_insert_with(|| reason.into());
    }

    fn full(&self) -> bool {
        let graph = &self.graph;
        graph.nodes.len() + graph.clients.len() + graph.ports.len() + graph.links.len()
            >= MAX_OBJECTS
    }
}

fn text(props: &spa::utils::dict::DictRef, key: &str) -> String {
    props.get(key).unwrap_or_default().to_owned()
}

fn number(props: &spa::utils::dict::DictRef, key: &str) -> Option<u32> {
    props.get(key)?.parse().ok()
}

/// Make and remove links until the share has exactly the links it wants.
fn reconcile(
    state: &mut State,
    core: &pw::core::CoreRc,
    registry: &pw::registry::RegistryRc,
    capture: u32,
) {
    if state.failure.is_some() || capture == pw::constants::ID_ANY {
        return;
    }
    if let Some(link) = state.graph.foreign(capture) {
        let _ = registry.destroy_global(link);
        state.fail("Audio sharing stopped: another program connected to it");
        return;
    }
    let Some(inputs) = state.graph.inputs(capture) else {
        return;
    };
    let wanted = state.graph.wanted(capture, inputs);
    if wanted.len() > MAX_LINKS {
        state.fail("Audio sharing stopped: too many audio streams");
        return;
    }
    // Dropping a link's proxy removes the link (it does not linger).
    state.links.retain(|ports, _| wanted.contains(ports));
    for &(output, input) in &wanted {
        if state.links.contains_key(&(output, input)) {
            continue;
        }
        let Some(node) = state.graph.ports.get(&output).map(|port| port.node) else {
            continue;
        };
        let link = core.create_object::<pw::link::Link>(
            "link-factory",
            &properties! {
                "link.output.node" => node.to_string(),
                "link.output.port" => output.to_string(),
                "link.input.node" => capture.to_string(),
                "link.input.port" => input.to_string(),
                "object.linger" => "false",
            },
        );
        match link {
            Ok(link) => {
                state.links.insert((output, input), link);
            }
            Err(_) => state.fail("Audio sharing stopped: a stream could not be linked"),
        }
    }
}

/// Record what the registry announces, binding nodes and clients to read
/// their full descriptions.
fn announce(
    state: &Rc<RefCell<State>>,
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    update: &Rc<dyn Fn()>,
) {
    let Some(props) = global.props else {
        return;
    };
    let id = global.id;
    let described = |apply: fn(&mut State, u32, &spa::utils::dict::DictRef)| {
        let state = Rc::downgrade(state);
        let update = Rc::downgrade(update);
        move |props: Option<&spa::utils::dict::DictRef>| {
            let (Some(state), Some(update), Some(props)) =
                (state.upgrade(), update.upgrade(), props)
            else {
                return;
            };
            apply(&mut state.borrow_mut(), id, props);
            update();
        }
    };
    match global.type_ {
        ObjectType::Node => {
            let node = policy::Node {
                class: text(props, "media.class"),
                client: number(props, "client.id"),
                application: text(props, "application.name"),
                ..policy::Node::default()
            };
            let bound = registry
                .bind::<pw::node::Node, _>(global)
                .ok()
                .map(|proxy| {
                    let info = described(|state, id, props| {
                        if let Some(node) = state.graph.nodes.get_mut(&id) {
                            node.class = text(props, "media.class");
                            node.client = number(props, "client.id");
                            node.link_group = props.get("node.link-group").map(str::to_owned);
                            node.application = text(props, "application.name");
                            node.binary = text(props, "application.process.binary");
                            node.ready = true;
                        }
                    });
                    let listener = proxy
                        .add_listener_local()
                        .info(move |described| {
                            // An update without the properties leaves them as they were.
                            if described
                                .change_mask()
                                .contains(pw::node::NodeChangeMask::PROPS)
                            {
                                info(described.props());
                            }
                        })
                        .register();
                    (proxy, listener)
                });
            let mut state = state.borrow_mut();
            state.graph.nodes.insert(id, node);
            state.nodes.extend(bound.map(|bound| (id, bound)));
        }
        ObjectType::Client => {
            let client = policy::Client {
                pid: props.get("pipewire.sec.pid").map(str::to_owned),
                application: text(props, "application.name"),
                ..policy::Client::default()
            };
            let bound = registry
                .bind::<pw::client::Client, _>(global)
                .ok()
                .map(|proxy| {
                    let info = described(|state, id, props| {
                        if let Some(client) = state.graph.clients.get_mut(&id) {
                            client.application = text(props, "application.name");
                            client.binary = text(props, "application.process.binary");
                            client.ready = true;
                        }
                    });
                    let listener = proxy
                        .add_listener_local()
                        .info(move |described| {
                            // An update without the properties leaves them as they were.
                            if described
                                .change_mask()
                                .contains(pw::client::ClientChangeMask::PROPS)
                            {
                                info(described.props());
                            }
                        })
                        .register();
                    (proxy, listener)
                });
            let mut state = state.borrow_mut();
            state.graph.clients.insert(id, client);
            state.clients.extend(bound.map(|bound| (id, bound)));
        }
        ObjectType::Port => {
            let Some(node) = number(props, "node.id") else {
                return;
            };
            state.borrow_mut().graph.ports.insert(
                id,
                policy::Port {
                    node,
                    output: props.get("port.direction") == Some("out"),
                    monitor: props.get("port.monitor") == Some("true"),
                    channel: text(props, "audio.channel"),
                },
            );
        }
        ObjectType::Link => {
            let (Some(output), Some(input)) = (
                number(props, "link.output.node"),
                number(props, "link.input.node"),
            ) else {
                return;
            };
            let mut state = state.borrow_mut();
            state.graph.links.insert(
                id,
                policy::Link {
                    output,
                    input,
                    client: number(props, "client.id"),
                },
            );
            if let (Some(output), Some(input)) = (
                number(props, "link.output.port"),
                number(props, "link.input.port"),
            ) {
                state.link_ports.insert(id, (output, input));
            }
        }
        _ => return,
    }
    update();
}

fn forget(state: &mut State, id: u32) {
    state.graph.nodes.remove(&id);
    state.graph.clients.remove(&id);
    state.graph.ports.remove(&id);
    state.graph.links.remove(&id);
    state.nodes.remove(&id);
    state.clients.remove(&id);
    // A link of the share's that went away is made again if still wanted.
    if let Some(ports) = state.link_ports.remove(&id) {
        state.links.remove(&ports);
    }
    state
        .links
        .retain(|&(output, input), _| output != id && input != id);
}

fn format() -> Result<Vec<u8>, String> {
    let mut audio = spa::param::audio::AudioInfoRaw::new();
    audio.set_format(spa::param::audio::AudioFormat::F32LE);
    audio.set_rate(48_000);
    audio.set_channels(2);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(bytes, _)| bytes.into_inner())
    .map_err(|_| "Audio sharing unavailable".to_owned())
}

type Started = Option<mpsc::Sender<Result<(), String>>>;

fn run(
    samples: Producer<f32>,
    stopped: pw::channel::Receiver<()>,
    started: &mut Started,
) -> Result<(), String> {
    let unavailable = |_| "Audio sharing needs PipeWire".to_owned();
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(unavailable)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(unavailable)?;
    let core = context.connect_rc(None).map_err(unavailable)?;
    let registry = core.get_registry_rc().map_err(unavailable)?;
    let state = Rc::new(RefCell::new(State::default()));
    let _stop = stopped.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "Shared audio",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_NAME => "Shared audio",
            *pw::keys::NODE_NAME => "nuschl-shared-audio",
            *pw::keys::APP_NAME => "Nuschl",
            "node.autoconnect" => "false",
            "node.dont-reconnect" => "true",
        },
    )
    .map_err(unavailable)?;
    let quit = {
        let mainloop = mainloop.clone();
        let state = state.clone();
        move |reason: &str| {
            state.borrow_mut().fail(reason);
            mainloop.quit();
        }
    };
    let _stream_listener = stream
        .add_local_listener_with_user_data(samples)
        .state_changed({
            let quit = quit.clone();
            move |_, _, _, new| {
                if let pw::stream::StreamState::Error(_) = new {
                    quit("Audio sharing stopped: the audio server refused it");
                }
            }
        })
        .process(|stream, samples| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
            if let Some(bytes) = data.data()
                && let Some(bytes) = bytes.get(offset..offset.saturating_add(size))
            {
                for sample in bytes.as_chunks::<4>().0 {
                    // A full ring (a stalled encoder) drops the newest audio.
                    let _ = samples.push(f32::from_le_bytes(*sample));
                }
            }
        })
        .register()
        .map_err(unavailable)?;
    let format = format()?;
    let format = spa::pod::Pod::from_bytes(&format).ok_or("Audio sharing unavailable")?;
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [format],
        )
        .map_err(unavailable)?;

    let update: Rc<dyn Fn()> = Rc::new({
        let state = state.clone();
        let core = core.clone();
        let registry = registry.clone();
        let stream = stream.clone();
        let quit = quit.clone();
        move || {
            let mut guard = state.borrow_mut();
            if guard.full() {
                guard.fail("Audio sharing stopped: too many audio objects");
            }
            reconcile(&mut guard, &core, &registry, stream.node_id());
            let failure = guard.failure.clone();
            drop(guard);
            if let Some(reason) = failure {
                quit(&reason);
            }
        }
    });
    let _core_listener = core
        .add_listener_local()
        .error({
            let quit = quit.clone();
            move |id, _, res, _| {
                // A broken connection (EPIPE). Errors about single objects,
                // such as a link to a port that just went away, are expected.
                if id == pw::core::PW_ID_CORE && res == -32 {
                    quit("Audio sharing stopped: the audio server went away");
                }
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let registry = registry.downgrade();
            let update = update.clone();
            move |global| {
                if let Some(registry) = registry.upgrade() {
                    announce(&state, &registry, global, &update);
                }
            }
        })
        .global_remove({
            let state = state.clone();
            let update = update.clone();
            move |id| {
                forget(&mut state.borrow_mut(), id);
                update();
            }
        })
        .register();
    if let Some(started) = started.take() {
        let _ = started.send(Ok(()));
    }
    mainloop.run();
    let failure = state.borrow_mut().failure.take();
    failure.map_or(Ok(()), Err)
}
