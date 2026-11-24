// SPDX-License-Identifier: EUPL-1.2
//! The core: one task owns every room, every peer's WebRTC state (str0m in RTP
//! mode) and the one UDP socket. It forwards RTP packets from a publisher to
//! each subscriber with their payload and timestamp unchanged, in the codec the
//! publisher negotiated (AV1, VP9 or Opus); it never decrypts, depacketizes or
//! transcodes. A source whose packets carry the Dependency Descriptor reaches
//! each receiver in the decode target chosen for it (`quality`), with the
//! marker on the last packet of each temporal unit the receiver gets. Each
//! subscriber gets its own sequence numbers from zero: frames it skips leave no
//! gap.

use crate::{
    config::Config,
    protocol::{
        self, ClientMessage, Decoders, MAX_SCREEN_BITRATE, Mode, PeerInfo, ServerMessage, Track,
        TurnCredentials, VideoLimit, Welcome,
    },
    quality::{self, Layered, VideoAdmission},
    rooms::{self, Bucket, Join, Nonce, PeerId, Refusal, RoomId, Rooms},
    routing::Routing,
    traffic::Traffic,
};
use nuschl_stream_quality::{
    DecodeCapacity, Kind, QualityRequest, Throttle, budget,
    dd::{self, Choice},
};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, PacerReason, Reason, Rtc,
    bwe::{Bitrate, BweKind},
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    format::{Codec, CodecConfig, FormatParams, PayloadParams},
    media::{Direction, Frequency, KeyframeRequestKind, MediaKind, Mid, Pt, Rid},
    net::{Protocol, Receive},
    rtp::{RtpPacket, RtpWrite},
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
};

/// The payload types of the AV1 format with level-idx 9, unused by str0m's defaults.
const PT_AV1_LEVEL_9: u8 = 47;
const PT_AV1_LEVEL_9_RTX: u8 = 48;
/// One keyframe request per source in this interval reaches its publisher.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
/// Per-packet overhead counted against media budgets (IP, UDP, SRTP).
const PACKET_OVERHEAD: usize = 48;
/// The largest UDP datagram read; longer ones are dropped.
pub const MAX_DATAGRAM: usize = 2048;
/// A subscribe connection's bandwidth estimate before its first measurement.
const INITIAL_ESTIMATE: u64 = 2_000_000;
/// Immediate sends a connection may service before returning to socket and signalling I/O.
const PACER_BATCH: usize = 16;
/// Heap entries serviced before the event loop checks the other inputs again.
const TIMEOUT_BATCH: usize = 32;

pub enum Command {
    Join {
        id: PeerId,
        client: IpAddr,
        nonce: Nonce,
        join: Join,
        out: mpsc::Sender<ServerMessage>,
    },
    Message {
        id: PeerId,
        message: ClientMessage,
    },
    Leave {
        id: PeerId,
    },
    /// The creator's device renews the lease of `room` or ends it; the reply is
    /// the HTTP status.
    Lease {
        room: RoomId,
        host_key: rooms::Key,
        end: bool,
        reply: oneshot::Sender<u16>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Leg {
    Publish,
    Subscribe,
}

struct Source {
    mid: Mid,
    kind: MediaKind,
    /// Video sources only.
    video: Option<quality::Video>,
    /// The simulcast layer forwarded, fixed by the first packet.
    layer: Option<Option<Rid>>,
    keyframe_asked: Option<Instant>,
}

struct Forward {
    from: PeerId,
    source: Mid,
    kind: MediaKind,
    /// `camera`, `screen` or `audio`.
    label: &'static str,
    /// The receiver-side mid, once offered.
    mid: Option<Mid>,
    open: bool,
    stopping: bool,
    view: quality::View,
}

struct Peer {
    room: RoomId,
    decoders: Decoders,
    out: mpsc::Sender<ServerMessage>,
    publish: Option<Rtc>,
    subscribe: Rtc,
    pending: Option<SdpPendingOffer>,
    /// Whether the subscribe connection has offered its data channel, which keeps
    /// the transport up while no source is forwarded: a browser closes it when
    /// every media section is stopped.
    channel: bool,
    sources: Vec<Source>,
    forwards: Vec<Forward>,
    budget: Bucket,
    deadlines: [Option<Instant>; 2],
    /// The video mids of its publish connection that carry a screen.
    screens: Vec<Mid>,
    /// The modes its sender chose for its video mids.
    modes: Vec<(Mid, Mode)>,
    /// The subscribe connection's bandwidth estimate, bits per second.
    estimate: u64,
    decode: DecodeCapacity,
    decode_throttle: Throttle,
    video_limits: bool,
    screen_rate: bool,
}

impl Source {
    fn label(&self) -> &'static str {
        match &self.video {
            None => "audio",
            Some(video) if video.kind == Kind::Screen => "screen",
            Some(_) => "camera",
        }
    }
}

impl Peer {
    fn rtc(&mut self, leg: Leg) -> Option<&mut Rtc> {
        match leg {
            Leg::Publish => self.publish.as_mut(),
            Leg::Subscribe => Some(&mut self.subscribe),
        }
    }
}

pub struct Turn {
    pub urls: Vec<String>,
    pub secret: Vec<u8>,
    pub lifetime: Duration,
}

pub struct Core {
    config: Arc<Config>,
    socket: Arc<UdpSocket>,
    /// Host candidates: the media addresses with the bound port.
    local: Vec<SocketAddr>,
    crypto: Arc<str0m::crypto::CryptoProvider>,
    turn: Option<Turn>,
    rooms: Rooms,
    peers: HashMap<PeerId, Peer>,
    routing: Routing<(PeerId, Leg)>,
    timers: BinaryHeap<Reverse<(Instant, PeerId, u8)>>,
    dirty: Vec<(PeerId, Leg)>,
    kicks: Vec<(PeerId, &'static str)>,
    /// The origin of the quality logic's time.
    origin: Instant,
    traffic: Option<Traffic>,
    /// How many tiers the traffic budget lowers every receiver's caps.
    step: u64,
}

fn canonical(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, |v4| {
            SocketAddr::new(IpAddr::V4(v4), address.port())
        }),
        IpAddr::V4(_) => address,
    }
}

impl Core {
    pub fn new(
        config: Arc<Config>,
        socket: Arc<UdpSocket>,
        turn: Option<Turn>,
        traffic: Option<Traffic>,
    ) -> std::io::Result<Self> {
        let port = socket.local_addr()?.port();
        let local = config
            .media_addresses
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect();
        Ok(Self {
            rooms: Rooms::new(rooms::Limits {
                rooms: config.max_rooms,
                peers_per_room: config.max_peers_per_room,
                lifetime: Duration::from_secs(config.room_lifetime_secs),
                lease: Duration::from_secs(config.lease_secs),
                creations_per_hour: config.creations_per_hour,
                room_bitrate: config.room_bitrate,
            }),
            config,
            socket,
            local,
            crypto: Arc::new(str0m::crypto::from_feature_flags()),
            turn,
            peers: HashMap::new(),
            routing: Routing::default(),
            timers: BinaryHeap::new(),
            dirty: Vec::new(),
            kicks: Vec::new(),
            origin: Instant::now(),
            traffic,
            step: 0,
        })
    }

    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let socket = self.socket.clone();
        let mut buffer = vec![0; MAX_DATAGRAM];
        let mut expiry = tokio::time::interval(Duration::from_secs(10));
        let mut quality = tokio::time::interval(Throttle::INTERVAL);
        loop {
            let next = self.timers.peek().map_or_else(
                || Instant::now() + Duration::from_secs(1),
                |Reverse((at, ..))| *at,
            );
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command, Instant::now()),
                    None => return,
                },
                received = socket.recv_from(&mut buffer) => {
                    if let Ok((length, source)) = received
                        && length < buffer.len()
                    {
                        self.datagram(canonical(source), &buffer[..length], Instant::now());
                    }
                }
                _ = tokio::time::sleep_until(next.into()) => self.timeouts(Instant::now()),
                _ = quality.tick() => self.quality(Instant::now()),
                _ = expiry.tick() => {
                    self.routing.expire(Instant::now());
                    for id in self.rooms.expire(Instant::now()) {
                        self.kicks.push((id, "ended"));
                    }
                }
            }
            self.flush(Instant::now());
        }
    }

    fn command(&mut self, command: Command, now: Instant) {
        match command {
            Command::Join {
                id,
                client,
                nonce,
                join,
                out,
            } => self.join(id, client, &nonce, join, out, now),
            Command::Leave { id } => self.remove(id, None),
            Command::Lease {
                room,
                host_key,
                end,
                reply,
            } => {
                let status = match self.rooms.lease(&room, &host_key, end, now) {
                    Ok(peers) => {
                        for peer in peers {
                            self.kicks.push((peer, "ended"));
                        }
                        204
                    }
                    Err(Refusal::NoRoom) => 404,
                    Err(Refusal::Rate) => 429,
                    Err(_) => 403,
                };
                let _ = reply.send(status);
            }
            Command::Message { id, message } => {
                if let Err(reason) = self.message(id, message, now) {
                    self.kicks.push((id, reason));
                }
            }
        }
    }

    /// A connection for `route`, with its ufrag in the routing map.
    fn new_rtc(&mut self, route: (PeerId, Leg), now: Instant) -> Rtc {
        let mut builder = Rtc::builder();
        if route.1 == Leg::Subscribe {
            builder = builder.enable_bwe(Some(Bitrate::bps(INITIAL_ESTIMATE)));
        }
        let mut config = builder
            .set_crypto_provider(self.crypto.clone())
            .set_rtp_mode(true)
            .set_ice_lite(true)
            // Royalty-free codecs only: a call uses AV1 or VP9, and Opus for audio.
            .clear_codecs()
            .enable_av1(true)
            .enable_vp9(true)
            .enable_opus(true, false)
            .set_extension(quality::DESCRIPTOR_ID, quality::descriptor_extension());
        // str0m pairs AV1 formats only at equal levels. Chromium states none
        // (level-idx 5) and Firefox level-idx 9, so both are offered and accepted.
        config.codec_config().add_config(
            PT_AV1_LEVEL_9.into(),
            Some(PT_AV1_LEVEL_9_RTX.into()),
            Codec::Av1,
            Frequency::NINETY_KHZ,
            None,
            FormatParams {
                level_idx: Some(9),
                ..FormatParams::default()
            },
        );
        let mut rtc = config.build(now);
        for address in &self.local {
            if let Ok(candidate) = Candidate::host(*address, "udp") {
                rtc.add_local_candidate(candidate);
            }
        }
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        self.routing.add_ufrag(ufrag, route);
        rtc
    }

    /// The call codec of `room`'s peers.
    fn codec(&self, room: &RoomId) -> Option<protocol::Codec> {
        let room = self.rooms.get(room)?;
        protocol::call_codec(
            room.peers
                .iter()
                .filter_map(|(id, _)| Some(self.peers.get(id)?.decoders)),
        )
    }

    fn join(
        &mut self,
        id: PeerId,
        client: IpAddr,
        nonce: &Nonce,
        join: Join,
        out: mpsc::Sender<ServerMessage>,
        now: Instant,
    ) {
        if self.traffic.as_ref().is_some_and(Traffic::exhausted) {
            let _ = out.try_send(ServerMessage::error("traffic"));
            return;
        }
        let decoders = join.decoders;
        let role = match self.rooms.join(id, &join, nonce, client, now) {
            Ok(role) => role,
            Err(refusal) => {
                let _ = out.try_send(ServerMessage::error(refusal.reason()));
                return;
            }
        };
        let room = self.rooms.get(&join.room).expect("joined");
        let others: Vec<_> = room
            .peers
            .iter()
            .filter(|(other, _)| *other != id)
            .map(|&(id, role)| PeerInfo { id, role })
            .collect();
        let before = self.codec(&join.room).filter(|_| !others.is_empty());
        let others_decoders = others.iter().map(|other| self.peers[&other.id].decoders);
        let Some(codec) = protocol::call_codec(others_decoders.chain([decoders])) else {
            self.rooms.leave(&join.room, id);
            let _ = out.try_send(ServerMessage::error("codec"));
            return;
        };
        let turn = self.turn.as_ref().map(|turn| {
            let (username, credential) =
                rooms::turn_credentials(&turn.secret, turn.lifetime, SystemTime::now());
            TurnCredentials {
                urls: turn.urls.clone(),
                username,
                credential,
            }
        });
        let mut forwards = Vec::new();
        for other in &others {
            for source in &self.peers[&other.id].sources {
                forwards.push(forward(other.id, source.mid, source.kind, source.label()));
            }
        }
        let peer = Peer {
            room: join.room,
            decoders,
            out,
            publish: None,
            subscribe: self.new_rtc((id, Leg::Subscribe), now),
            pending: None,
            channel: false,
            sources: Vec::new(),
            forwards,
            budget: rooms::media_budget(self.config.receiver_bitrate, now),
            deadlines: [None; 2],
            screens: Vec::new(),
            modes: Vec::new(),
            estimate: INITIAL_ESTIMATE,
            decode: quality::full_capacity(),
            decode_throttle: Throttle::default(),
            video_limits: join.video_limits,
            screen_rate: join.screen_rate,
        };
        self.peers.insert(id, peer);
        // Configured egress budgets govern admission. Measured connection
        // capacity governs layer allocation after the viewer connects.
        let planned = self.plan().0;
        let rates = rates(&planned);
        let (receivers, streams) = self.streams(&planned, &rates);
        let limits = self.limits();
        if !budget::admits(&limits, receivers.len(), &streams) {
            let peer = self.peers.remove(&id).expect("inserted");
            self.rooms.leave(&peer.room, id);
            self.routing.remove(|(route, _)| route == id);
            let _ = peer.out.try_send(ServerMessage::error("bandwidth"));
            return;
        }
        // Give the estimator enough initial capacity to send the admitted
        // base layers. Their traffic lets it measure this connection.
        let base: u64 = planned
            .iter()
            .zip(&rates)
            .filter(|(source, _)| source.receiver == id)
            .filter_map(|(_, rates)| rates.first())
            .sum();
        let peer = self.peers.get_mut(&id).expect("admitted peer");
        if base > peer.estimate {
            peer.estimate = base;
            peer.subscribe.bwe().reset(Bitrate::bps(base));
        }
        self.send(
            id,
            ServerMessage::Welcome(Welcome {
                peer: id,
                role,
                peers: others.clone(),
                codec,
                turn,
            }),
        );
        for other in others {
            self.send(other.id, ServerMessage::Joined { peer: id, role });
            if before != Some(codec) {
                self.send(other.id, ServerMessage::Codec { codec });
            }
        }
        self.negotiate(id);
    }

    fn message(
        &mut self,
        id: PeerId,
        message: ClientMessage,
        now: Instant,
    ) -> Result<(), &'static str> {
        let room = self.peers.get(&id).ok_or("unknown")?.room;
        match message {
            ClientMessage::Join { .. } => return Err("joined"),
            ClientMessage::Offer { .. }
                if self.traffic.as_ref().is_some_and(Traffic::exhausted) =>
            {
                return Err("traffic");
            }
            ClientMessage::Offer { sdp, screens } => {
                let offer = SdpOffer::from_sdp_string(&sdp).map_err(|_| "sdp")?;
                let limit = self.config.max_sources_per_peer;
                self.peers.get_mut(&id).expect("exists").screens = screens
                    .iter()
                    .take(limit)
                    .map(|mid| Mid::from(mid.as_str()))
                    .collect();
                if self.peers[&id].publish.is_none() {
                    let rtc = self.new_rtc((id, Leg::Publish), now);
                    self.peers.get_mut(&id).expect("exists").publish = Some(rtc);
                }
                let rtc = self
                    .peers
                    .get_mut(&id)
                    .and_then(|p| p.publish.as_mut())
                    .expect("exists");
                let answer = rtc.sdp_api().accept_offer(offer).map_err(|_| "sdp")?;
                self.send(
                    id,
                    ServerMessage::Answer {
                        sdp: answer.to_sdp_string(),
                    },
                );
                self.dirty.push((id, Leg::Publish));
            }
            ClientMessage::Modes { modes } => {
                let limit = self.config.max_sources_per_peer;
                let peer = self.peers.get_mut(&id).expect("exists");
                peer.modes = modes
                    .into_iter()
                    .take(limit)
                    .map(|(mid, mode)| (Mid::from(mid.as_str()), mode))
                    .collect();
                for source in &mut peer.sources {
                    if let Some(video) = &mut source.video {
                        video.mode = mode(&peer.modes, source.mid, video.kind);
                    }
                }
            }
            ClientMessage::Answer { sdp } => {
                let answer = SdpAnswer::from_sdp_string(&sdp).map_err(|_| "sdp")?;
                let peer = self.peers.get_mut(&id).expect("exists");
                let pending = peer.pending.take().ok_or("unexpected answer")?;
                peer.subscribe
                    .sdp_api()
                    .accept_answer(pending, answer)
                    .map_err(|_| "sdp")?;
                let mut opened = Vec::new();
                for forward in peer
                    .forwards
                    .iter_mut()
                    .filter(|f| f.mid.is_some() && !f.open)
                {
                    forward.open = true;
                    if !forward.stopping {
                        opened.push((forward.from, forward.source));
                    }
                }
                // A layered source's selector asks once the receiver has a target.
                for (from, source) in opened {
                    let layered = self.peers.get(&from).is_some_and(|p| {
                        p.sources.iter().any(|s| {
                            s.mid == source && s.video.as_ref().is_some_and(|v| v.layers.layered())
                        })
                    });
                    if !layered {
                        self.keyframe(from, source, now);
                    }
                }
                self.dirty.push((id, Leg::Subscribe));
                self.negotiate(id);
            }
            ClientMessage::Quality { request } => {
                let request = protocol::from_hex::<{ QualityRequest::SIZE }>(&request)
                    .and_then(|bytes| QualityRequest::from_bytes(&bytes))
                    .ok_or("quality")?;
                let since = now.saturating_duration_since(self.origin);
                let publisher = self.peers.get(&request.publisher);
                let sources: Vec<Mid> = publisher
                    .map(|p| {
                        p.sources
                            .iter()
                            .filter(|s| s.video.as_ref().is_some_and(|v| v.kind == request.kind))
                            .map(|s| s.mid)
                            .collect()
                    })
                    .unwrap_or_default();
                let peer = self.peers.get_mut(&id).expect("exists");
                for forward in peer
                    .forwards
                    .iter_mut()
                    .filter(|f| f.from == request.publisher && sources.contains(&f.source))
                {
                    // A sooner request is dropped; the receiver sends its latest later.
                    if forward.view.throttle.allow(since) {
                        forward.view.request = Some(usize::from(request.tier));
                    }
                }
            }
            ClientMessage::Capacity { decode } => {
                let decode = protocol::from_hex::<{ DecodeCapacity::SIZE }>(&decode)
                    .and_then(|bytes| DecodeCapacity::from_bytes(&bytes))
                    .ok_or("capacity")?;
                let since = now.saturating_duration_since(self.origin);
                let peer = self.peers.get_mut(&id).expect("exists");
                if peer.decode_throttle.allow(since) {
                    peer.decode = decode;
                }
            }
            ClientMessage::Key { to, data } => {
                if !protocol::valid_key_message(&data) {
                    return Err("key");
                }
                if to != id && self.peers.get(&to).is_some_and(|peer| peer.room == room) {
                    self.send(to, ServerMessage::Key { from: id, data });
                }
            }
            ClientMessage::Remove { peer } => {
                if !self.rooms.is_host(&room, id) {
                    return Err("not_allowed");
                }
                if peer != id && self.peers.get(&peer).is_some_and(|p| p.room == room) {
                    self.kicks.push((peer, "removed"));
                }
            }
            ClientMessage::End => {
                let peers = self.rooms.end(&room, id).map_err(|r| r.reason())?;
                for peer in peers {
                    self.kicks.push((peer, "ended"));
                }
            }
        }
        Ok(())
    }

    /// Offers pending changes on `id`'s subscribe connection, one offer at a time.
    fn negotiate(&mut self, id: PeerId) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        if peer.pending.is_some() {
            return;
        }
        // Without a pending offer, every forward with a mid is open.
        let mut api = peer.subscribe.sdp_api();
        if !std::mem::replace(&mut peer.channel, true) {
            api.add_channel("sfu".into());
        }
        peer.forwards
            .retain_mut(|forward| match (forward.mid, forward.stopping) {
                (None, true) => false,
                (Some(mid), true) => {
                    api.stop_media(mid);
                    false
                }
                (None, false) => {
                    forward.mid = Some(api.add_media(
                        forward.kind,
                        Direction::SendOnly,
                        Some(forward.from.to_string()),
                        Some(forward.source.to_string()),
                        None,
                    ));
                    true
                }
                (Some(_), false) => true,
            });
        let Some((offer, pending)) = api.apply() else {
            return;
        };
        peer.pending = Some(pending);
        let tracks = peer
            .forwards
            .iter()
            .filter_map(|f| {
                Some(Track {
                    mid: f.mid?.to_string(),
                    peer: f.from,
                    source: f.source.to_string(),
                    kind: f.label.into(),
                })
            })
            .collect();
        self.send(
            id,
            ServerMessage::Offer {
                sdp: offer.to_sdp_string(),
                tracks,
            },
        );
    }

    fn send(&mut self, id: PeerId, message: ServerMessage) {
        if let Some(peer) = self.peers.get(&id)
            && peer.out.try_send(message).is_err()
        {
            // A peer that does not read its messages leaves.
            self.kicks.push((id, "slow"));
        }
    }

    fn remove(&mut self, id: PeerId, reason: Option<&'static str>) {
        let before = self.peers.get(&id).and_then(|peer| self.codec(&peer.room));
        let Some(peer) = self.peers.remove(&id) else {
            return;
        };
        if let Some(reason) = reason {
            let _ = peer.out.try_send(ServerMessage::error(reason));
        }
        self.rooms.leave(&peer.room, id);
        self.routing.remove(|(route, _)| route == id);
        let others: Vec<PeerId> = self
            .rooms
            .get(&peer.room)
            .map(|room| room.peers.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default();
        let codec = self
            .codec(&peer.room)
            .filter(|&codec| Some(codec) != before);
        for other in others {
            if let Some(codec) = codec {
                self.send(other, ServerMessage::Codec { codec });
            }
            if let Some(other_peer) = self.peers.get_mut(&other) {
                for forward in other_peer.forwards.iter_mut().filter(|f| f.from == id) {
                    forward.stopping = true;
                }
            }
            self.send(other, ServerMessage::Left { peer: id });
            self.negotiate(other);
        }
    }

    fn datagram(&mut self, source: SocketAddr, data: &[u8], now: Instant) {
        let Some((id, leg)) = self.routing.route(source, data) else {
            return;
        };
        let Some(&destination) = self.local.iter().find(|a| a.is_ipv4() == source.is_ipv4()) else {
            return;
        };
        let Ok(contents) = data.try_into() else {
            return;
        };
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination,
                contents,
            },
        );
        let Some(rtc) = self.peers.get_mut(&id).and_then(|p| p.rtc(leg)) else {
            return;
        };
        if rtc.handle_input(input).is_err() {
            self.kicks.push((id, "media"));
            return;
        }
        self.drain(id, leg, now);
    }

    fn timeouts(&mut self, now: Instant) {
        for _ in 0..TIMEOUT_BATCH {
            let Some(&Reverse((at, id, leg))) = self.timers.peek() else {
                break;
            };
            if at > now {
                break;
            }
            self.timers.pop();
            let leg = if leg == 0 {
                Leg::Publish
            } else {
                Leg::Subscribe
            };
            let Some(peer) = self.peers.get_mut(&id) else {
                continue;
            };
            if peer.deadlines[leg as usize] != Some(at) {
                continue;
            }
            peer.deadlines[leg as usize] = None;
            if let Some(rtc) = peer.rtc(leg)
                && rtc.handle_input(Input::Timeout(now)).is_err()
            {
                self.kicks.push((id, "media"));
                continue;
            }
            self.drain(id, leg, now);
        }
    }

    /// Drains the connections that changed and disconnects kicked peers.
    fn flush(&mut self, now: Instant) {
        loop {
            if let Some((id, leg)) = self.dirty.pop() {
                self.drain(id, leg, now);
            } else if let Some((id, reason)) = self.kicks.pop() {
                self.remove(id, Some(reason));
            } else {
                return;
            }
        }
    }

    /// Polls one connection until it waits: sends its datagrams and handles
    /// its events.
    fn drain(&mut self, id: PeerId, leg: Leg, mut now: Instant) {
        let mut immediate = 0;
        loop {
            let Some(peer) = self.peers.get_mut(&id) else {
                return;
            };
            let Some(rtc) = peer.rtc(leg) else {
                return;
            };
            if !rtc.is_alive() {
                self.kicks.push((id, "media"));
                return;
            }
            match rtc.poll_output() {
                Err(_) => {
                    self.kicks.push((id, "media"));
                    return;
                }
                Ok(Output::Timeout(at)) => {
                    // str0m asks for a new clock reading after each paced send.
                    // Service due work here because Tokio timers run in milliseconds.
                    if leg == Leg::Subscribe
                        && immediate < PACER_BATCH
                        && rtc.last_timeout_reason() == Reason::Pacer(PacerReason::Immediate)
                    {
                        let current = Instant::now();
                        if at <= current {
                            if rtc.handle_input(Input::Timeout(current)).is_err() {
                                self.kicks.push((id, "media"));
                                return;
                            }
                            now = current;
                            immediate += 1;
                            continue;
                        }
                    }
                    if peer.deadlines[leg as usize] != Some(at) {
                        peer.deadlines[leg as usize] = Some(at);
                        self.timers.push(Reverse((at, id, leg as u8)));
                    }
                    return;
                }
                Ok(Output::Transmit(transmit)) => {
                    self.routing.learn(transmit.destination, (id, leg), now);
                    if let Some(traffic) = &mut self.traffic {
                        // With the IPv4 and UDP headers.
                        traffic.add(transmit.contents.len() + 28);
                    }
                    let mut destination = transmit.destination;
                    if let (IpAddr::V4(v4), Ok(local)) =
                        (destination.ip(), self.socket.local_addr())
                        && local.is_ipv6()
                    {
                        destination =
                            SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), destination.port());
                    }
                    // A full socket buffer drops the datagram, as the network would.
                    let _ = self.socket.try_send_to(&transmit.contents, destination);
                }
                Ok(Output::Event(event)) => self.event(id, leg, event, now),
            }
        }
    }

    fn event(&mut self, id: PeerId, leg: Leg, event: Event, now: Instant) {
        match (leg, event) {
            (_, Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => {
                self.kicks.push((id, "media"));
            }
            (Leg::Publish, Event::MediaAdded(added)) if added.direction.is_receiving() => {
                self.add_source(id, added.mid, added.kind);
            }
            (Leg::Publish, Event::MediaChanged(changed)) if !changed.direction.is_receiving() => {
                self.remove_source(id, changed.mid);
            }
            (Leg::Publish, Event::RtpPacket(packet)) => self.forward(id, packet, now),
            (Leg::Subscribe, Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. })) => {
                if let Some(peer) = self.peers.get_mut(&id) {
                    peer.estimate = estimate.as_u64();
                }
            }
            (Leg::Subscribe, Event::KeyframeRequest(request)) => {
                let Some(peer) = self.peers.get(&id) else {
                    return;
                };
                if let Some(forward) = peer
                    .forwards
                    .iter()
                    .find(|f| f.mid == Some(request.mid) && !f.view.backlog.recovering())
                {
                    let (from, source) = (forward.from, forward.source);
                    self.keyframe(from, source, now);
                }
            }
            _ => {}
        }
    }

    fn add_source(&mut self, id: PeerId, mid: Mid, kind: MediaKind) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        if peer.sources.iter().any(|s| s.mid == mid) {
            return;
        }
        if peer.sources.len() >= self.config.max_sources_per_peer {
            self.kicks.push((id, "sources"));
            return;
        }
        let video = (kind == MediaKind::Video).then(|| {
            let kind = if peer.screens.contains(&mid) {
                Kind::Screen
            } else {
                Kind::Camera
            };
            quality::Video::new(kind, mode(&peer.modes, mid, kind))
        });
        peer.sources.push(Source {
            mid,
            kind,
            video,
            layer: None,
            keyframe_asked: None,
        });
        let label = peer.sources.last().expect("pushed").label();
        let room = peer.room;
        let others: Vec<PeerId> = self.rooms.get(&room).map_or_else(Vec::new, |r| {
            r.peers
                .iter()
                .map(|(p, _)| *p)
                .filter(|p| *p != id)
                .collect()
        });
        for other in others {
            if let Some(peer) = self.peers.get_mut(&other) {
                peer.forwards.push(forward(id, mid, kind, label));
                self.negotiate(other);
            }
        }
    }

    /// A publisher stopped a source: its forwards end on every subscribe connection.
    fn remove_source(&mut self, id: PeerId, mid: Mid) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        let count = peer.sources.len();
        peer.sources.retain(|source| source.mid != mid);
        if peer.sources.len() == count {
            return;
        }
        let others: Vec<PeerId> = self.peers.keys().copied().filter(|p| *p != id).collect();
        for other in others {
            let peer = self.peers.get_mut(&other).expect("listed");
            let mut stopped = false;
            for forward in peer
                .forwards
                .iter_mut()
                .filter(|f| f.from == id && f.source == mid)
            {
                forward.stopping = true;
                stopped = true;
            }
            if stopped {
                self.negotiate(other);
            }
        }
    }

    fn forward(&mut self, from: PeerId, packet: RtpPacket, now: Instant) {
        let Some(publisher) = self.peers.get_mut(&from) else {
            return;
        };
        let room = publisher.room;
        let Some(rtc) = publisher.publish.as_mut() else {
            return;
        };
        let Some((mid, rid)) = rtc
            .direct_api()
            .stream_rx(&packet.header.ssrc)
            .map(|stream| (stream.mid(), stream.rid()))
        else {
            return;
        };
        let Some(params) = rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| p.pt() == packet.header.payload_type)
            .copied()
        else {
            return;
        };
        let Some(source) = publisher.sources.iter_mut().find(|s| s.mid == mid) else {
            return;
        };
        let descriptor = packet
            .header
            .ext_vals
            .user_values
            .get::<quality::Descriptor>();
        // None: no descriptor, every receiver gets every packet. Some(None): a
        // descriptor this source's structure cannot read yet, which a key frame
        // brings.
        let since = now.saturating_duration_since(self.origin);
        let timestamp = packet.time.numer();
        let frame = match (&mut source.video, descriptor) {
            (Some(video), Some(descriptor)) => Some(video.layers.packet(
                &descriptor.0,
                packet.payload.len(),
                timestamp,
                since,
            )),
            _ => None,
        };
        // Q1 hook: per-receiver layer choice (docs/video-plan.md §5.3) replaces
        // this. Today every receiver gets the first layer a source sends, and
        // all of a single stream's packets, including every temporal layer.
        if *source.layer.get_or_insert(rid) != rid {
            return;
        }
        let video = source.kind == MediaKind::Video;
        let bound = source
            .video
            .as_ref()
            .map_or(quality::QUEUE_BOUND, quality::Video::bound);
        let payload = packet.payload.len();
        let cost = (payload + PACKET_OVERHEAD) as u64;
        let can_request = source
            .keyframe_asked
            .is_none_or(|at| now.saturating_duration_since(at) >= KEYFRAME_INTERVAL);
        let receivers: Vec<PeerId> = self.rooms.get(&room).map_or_else(Vec::new, |r| {
            r.peers
                .iter()
                .map(|(p, _)| *p)
                .filter(|p| *p != from)
                .collect()
        });
        let mut keyframe = matches!(frame, Some(None));
        for id in receivers {
            let Some(peer) = self.peers.get_mut(&id) else {
                continue;
            };
            let Some(forward) = peer
                .forwards
                .iter_mut()
                .find(|f| f.from == from && f.source == mid && f.open)
            else {
                continue;
            };
            let Some(out_mid) = forward.mid else {
                continue;
            };
            let Some(pt) = peer
                .subscribe
                .media(out_mid)
                .map(|media| media.remote_pts().to_vec())
                .and_then(|pts| receiver_pt(peer.subscribe.codec_config(), &pts, params))
            else {
                continue;
            };
            let mut api = peer.subscribe.direct_api();
            let Some(stream) = api.stream_tx_by_mid(out_mid, None) else {
                continue;
            };
            let view = &mut forward.view;
            let layered = frame.flatten();
            if video && view.backlog.dropped(timestamp) {
                view.rewrite.skip(*packet.seq_no);
                if let Some(frame) = &layered {
                    view.selector.dropped(frame);
                }
                keyframe |= view.selector.needs_keyframe();
                continue;
            }
            let marker = match &frame {
                None => Some(packet.header.marker),
                Some(None) => None,
                Some(Some(frame)) => view
                    .selector
                    .forward(frame, timestamp, packet.header.marker),
            };
            keyframe |= view.selector.needs_keyframe();
            let Some(marker) = marker else {
                view.rewrite.skip(*packet.seq_no);
                continue;
            };
            let full = if video {
                let queue = stream.queue_info();
                let bitrate = peer.estimate.min(self.config.receiver_bitrate);
                match view.backlog.admission(timestamp, queue, can_request, now) {
                    VideoAdmission::Regular => match &layered {
                        None => view.backlog.full(queue, bitrate, payload, bound, now),
                        Some(frame) => {
                            let discardable = view.selector.discardable(frame);
                            match view.backlog.layered(
                                frame,
                                timestamp,
                                discardable,
                                queue,
                                bitrate,
                                bound,
                                now,
                            ) {
                                Layered::Send => view.backlog.capped(queue, payload),
                                Layered::Skip => {
                                    view.rewrite.skip(*packet.seq_no);
                                    view.selector.dropped(frame);
                                    keyframe |= view.selector.needs_keyframe();
                                    continue;
                                }
                            }
                        }
                    },
                    VideoAdmission::Burst(bytes) => view.backlog.burst_full(queue, bytes + cost),
                    VideoAdmission::Pause => {
                        view.rewrite.skip(*packet.seq_no);
                        continue;
                    }
                    VideoAdmission::RequestKeyframe => {
                        view.rewrite.skip(*packet.seq_no);
                        keyframe = true;
                        continue;
                    }
                }
            } else {
                false
            };
            let room_budget = &mut self.rooms.get_mut(&room).expect("room").budget;
            let limit = if full {
                Some(VideoLimit::ForwardingBacklog)
            } else if !peer.budget.take(cost as f64, now) {
                Some(VideoLimit::SfuViewer)
            } else if !room_budget.take(cost as f64, now) {
                Some(VideoLimit::SfuRoom)
            } else {
                None
            };
            if let Some(limit) = limit {
                if video {
                    view.rewrite.skip(*packet.seq_no);
                    view.backlog
                        .shed(timestamp, frame.is_none().then_some(now), limit);
                    if let Some(frame) = &layered {
                        view.selector.dropped(frame);
                    }
                }
                keyframe |= view.selector.needs_keyframe();
                continue;
            }
            let Some(seq) = view.rewrite.forward(*packet.seq_no) else {
                if let Some(frame) = &layered {
                    view.selector.dropped(frame);
                }
                continue;
            };
            if video {
                view.backlog.enqueued(payload, cost, marker);
            }
            stream.write_rtp(
                RtpWrite::new(
                    pt,
                    seq.into(),
                    packet.header.timestamp,
                    packet.timestamp,
                    packet.payload.clone(),
                )
                .marker(marker)
                .ext_vals(quality::forwarded(&packet.header.ext_vals))
                .nackable(video),
            );
            self.dirty.push((id, Leg::Subscribe));
        }
        if keyframe {
            self.keyframe(from, mid, now);
        }
    }

    /// The decode targets and publisher demands for each video source.
    fn plan(&self) -> (Vec<Planned>, HashMap<(PeerId, Mid), Demand>) {
        let mut demands: HashMap<(PeerId, Mid), Demand> = HashMap::new();
        let mut planned = Vec::new();
        let limits = self.limits();
        let (host, rooms, viewers) = self.screen_shares();
        for (&id, peer) in &self.peers {
            for (index, forward) in peer.forwards.iter().enumerate() {
                if forward.stopping {
                    continue;
                }
                let Some(publisher) = self.peers.get(&forward.from) else {
                    continue;
                };
                let Some(video) = publisher
                    .sources
                    .iter()
                    .find(|s| s.mid == forward.source)
                    .and_then(|s| s.video.as_ref())
                else {
                    continue;
                };
                let (requested, mut limit) =
                    quality::tier(video.kind, &forward.view, peer.decode, self.step);
                let backlog = &forward.view.backlog;
                let tier = backlog.tier(requested);
                if let Some(active) = backlog
                    .limit()
                    .filter(|_| tier < requested || backlog.recovering())
                {
                    limit = active;
                }
                // A late queue takes away what the source's mode gives up; the
                // budgets lower the rest.
                let smooth = video.smooth();
                let mut choices =
                    video
                        .layers
                        .choices(video.kind, requested, smooth, backlog.steps());
                // Past the lowest tier the traffic budget leaves base layers only.
                if self.step > video.kind.top() as u64 && choices.len() > 1 {
                    choices.truncate(1);
                    limit = VideoLimit::TrafficBudget;
                }
                let demand = demands.entry((forward.from, forward.source)).or_default();
                let publisher_limit =
                    if limit == VideoLimit::WithinTarget && tier < video.kind.top() {
                        VideoLimit::ReceiverQuality
                    } else {
                        limit
                    };
                demand.request(tier, publisher_limit);
                demand.wanted = demand.wanted.max(requested);
                let raised = video.kind == Kind::Screen && publisher.screen_rate;
                let single_layer = (raised || backlog.limited())
                    && video
                        .layers
                        .choices(video.kind, video.kind.top(), smooth, 0)
                        .len()
                        <= 1;
                if backlog.limited() && single_layer {
                    let (configured, configured_limit) =
                        if self.config.receiver_bitrate <= self.config.room_bitrate {
                            (self.config.receiver_bitrate, VideoLimit::SfuViewer)
                        } else {
                            (self.config.room_bitrate, VideoLimit::SfuRoom)
                        };
                    let affordable = video.kind.affordable(configured);
                    let cap = if affordable < tier {
                        (affordable, configured_limit)
                    } else {
                        (tier, publisher_limit)
                    };
                    demand.shared_cap = demand.shared_cap.min(cap);
                }
                let mut probe = video.kind.ladder()[requested].ceiling;
                if raised {
                    let viewer = viewers[&id];
                    let (configured, configured_limit) = [
                        (host.rate(limits.host), VideoLimit::SfuHost),
                        (rooms[&peer.room].rate(limits.room), VideoLimit::SfuRoom),
                        (viewer.rate(limits.receiver), VideoLimit::SfuViewer),
                    ]
                    .into_iter()
                    .fold((MAX_SCREEN_BITRATE, VideoLimit::WithinTarget), |a, b| {
                        a.min(b)
                    });
                    let policy = if self.step > 0 {
                        let step = self.step.min(video.kind.top() as u64) as usize;
                        let index = video.kind.top() - step;
                        (
                            video.kind.ladder()[index].ceiling,
                            VideoLimit::TrafficBudget,
                        )
                    } else {
                        (MAX_SCREEN_BITRATE, VideoLimit::WithinTarget)
                    };
                    // Probe independently of payload and leave room for packet headers.
                    probe = configured.min(policy.0.saturating_mul(10).div_ceil(9));
                    let network = viewer.rate(peer.estimate).min(MAX_SCREEN_BITRATE);
                    let mut cap = (configured * 9 / 10, configured_limit)
                        .min((network * 9 / 10, VideoLimit::ReceiverBandwidth))
                        .min(policy);
                    // A screen with one picture for every viewer gets at most the
                    // ceiling of that picture's tier, or of the viewer's tier when lower.
                    let picture = video.layers.picture_tier(video.kind);
                    if backlog.limited() || single_layer || picture.is_some() {
                        let tier = picture.map_or(tier, |picture| picture.min(tier));
                        cap = cap.min((video.kind.ladder()[tier].ceiling, limit));
                    }
                    demand.screen_rate(cap, single_layer);
                    if limit == VideoLimit::WithinTarget {
                        limit = cap.1;
                    }
                }
                planned.push(Planned {
                    receiver: id,
                    forward: index,
                    choices,
                    measured: video.layers.measured(),
                    mode: video.mode,
                    ceiling: probe,
                    limit,
                });
            }
        }
        (planned, demands)
    }

    /// Split configured egress across simultaneously forwarded screens. Reserve
    /// enough for the ceiling of the call's camera and Opus sources.
    fn screen_shares(
        &self,
    ) -> (
        ScreenShare,
        HashMap<RoomId, ScreenShare>,
        HashMap<PeerId, ScreenShare>,
    ) {
        let mut host = ScreenShare::default();
        let mut rooms: HashMap<_, ScreenShare> = HashMap::new();
        let mut viewers = HashMap::new();
        for (&id, peer) in &self.peers {
            let mut usage = ScreenShare::default();
            for forward in peer.forwards.iter().filter(|f| !f.stopping) {
                match forward.label {
                    "screen" => usage.screens += 1,
                    "camera" => usage.reserved += Kind::Camera.ladder()[Kind::Camera.top()].ceiling,
                    _ => usage.reserved += 256_000,
                }
            }
            host.reserve(usage);
            rooms.entry(peer.room).or_default().reserve(usage);
            viewers.insert(id, usage);
        }
        (host, rooms, viewers)
    }

    fn limits(&self) -> budget::Limits {
        budget::Limits {
            host: self.config.upload_bitrate / 10 * 7,
            room: self.config.room_bitrate,
            receiver: self.config.receiver_bitrate,
        }
    }

    /// Q1's budget inputs for `planned`: each receiver's bandwidth estimate and
    /// one stream per receiver and source.
    fn streams<'a>(
        &self,
        planned: &[Planned],
        rates: &'a [Vec<u64>],
    ) -> (Vec<u64>, Vec<budget::Stream<'a>>) {
        let mut rooms = HashMap::new();
        let mut receivers = HashMap::new();
        let mut estimates = Vec::new();
        let mut streams = Vec::new();
        for (planned, rates) in planned.iter().zip(rates) {
            let peer = &self.peers[&planned.receiver];
            let next = rooms.len();
            let room = *rooms.entry(peer.room).or_insert(next);
            let receiver = *receivers.entry(planned.receiver).or_insert_with(|| {
                estimates.push(peer.estimate);
                estimates.len() - 1
            });
            let stepper = peer.forwards[planned.forward].view.stepper;
            streams.push(budget::Stream {
                room,
                receiver,
                rates,
                current: stepper.map_or(0, |s| s.current() + 1),
            });
        }
        (estimates, streams)
    }

    /// Once a second: shares the budgets (Q1's `allocate`) over every
    /// receiver's layered sources, moves each receiver's decode target toward
    /// its share at Q1's `Stepper` pace, asks each subscribe connection's
    /// bandwidth estimator to probe for the receiver's tiers, and tells each
    /// publisher the new demand for each of its sources that changed.
    fn quality(&mut self, now: Instant) {
        let since = now.saturating_duration_since(self.origin);
        if let Some(traffic) = &mut self.traffic {
            let step = traffic.step(unix_seconds());
            if step != self.step {
                tracing::info!(step, sent = traffic.sent(), "traffic budget step");
                self.step = step;
            }
            if let Err(error) = traffic.save() {
                tracing::warn!("saving the traffic count: {error}");
            }
        }
        for peer in self.peers.values_mut() {
            for forward in &mut peer.forwards {
                forward.view.backlog.tick(now);
            }
        }
        let (planned, demands) = self.plan();
        let rates = rates(&planned);
        let (receivers, streams) = self.streams(&planned, &rates);
        let shares = budget::allocate(&self.limits(), &receivers, &streams);
        let mut desired: HashMap<PeerId, u64> = HashMap::new();
        for (planned, share) in planned.into_iter().zip(shares) {
            *desired.entry(planned.receiver).or_default() += planned.ceiling;
            let peer = self.peers.get_mut(&planned.receiver).expect("listed");
            let forward = &mut peer.forwards[planned.forward];
            let view = &mut forward.view;
            let mut limit = match share.limited_by {
                Some(budget::Limit::Host) => VideoLimit::SfuHost,
                Some(budget::Limit::Room) => VideoLimit::SfuRoom,
                Some(budget::Limit::Viewer) => VideoLimit::SfuViewer,
                Some(budget::Limit::Network) => VideoLimit::ReceiverBandwidth,
                None => planned.limit,
            };
            if share.layers == 0 && !planned.choices.is_empty() {
                view.backlog.overloaded(limit);
            }
            let target = share.layers.checked_sub(1).map(|ceiling| {
                let smooth = planned.mode.prefer == protocol::Prefer::Smooth;
                let (point, target) = dd::pick(
                    &planned.choices,
                    ceiling,
                    smooth,
                    planned.measured,
                    &mut view.stepper,
                    since,
                );
                if limit == VideoLimit::WithinTarget && point < ceiling {
                    limit = VideoLimit::QualityRecovery;
                }
                target
            });
            if peer.video_limits
                && forward.open
                && !forward.stopping
                && view.reported_limit != Some(limit)
                && peer.out.capacity() > crate::OUTGOING / 2
                && let Some(mid) = forward.mid
                && view.limit_throttle.allow(since)
                && peer
                    .out
                    .try_send(ServerMessage::VideoLimit {
                        mid: mid.to_string(),
                        peer: forward.from,
                        limit,
                    })
                    .is_ok()
            {
                view.reported_limit = Some(limit);
            }
            if forward.open
                && !forward.stopping
                && view.reported_mode != Some(planned.mode)
                && peer.out.capacity() > crate::OUTGOING / 2
                && let Some(mid) = forward.mid
                && peer
                    .out
                    .try_send(ServerMessage::Mode {
                        mid: mid.to_string(),
                        peer: forward.from,
                        mode: planned.mode,
                    })
                    .is_ok()
            {
                view.reported_mode = Some(planned.mode);
            }
            view.selector.want(target);
        }
        for (id, desired) in desired {
            let peer = self.peers.get_mut(&id).expect("listed");
            let desired = desired.min(self.config.receiver_bitrate);
            peer.subscribe
                .bwe()
                .set_bitrate(Bitrate::bps(0), Bitrate::bps(desired));
            self.dirty.push((id, Leg::Subscribe));
        }
        let mut messages = Vec::new();
        for (&id, peer) in &mut self.peers {
            for source in &mut peer.sources {
                let Some(video) = &mut source.video else {
                    continue;
                };
                if let Some(demand) = demands.get(&(id, source.mid)) {
                    let (tier, bitrate, limit) = demand.cap(video.kind);
                    let limit = peer.video_limits.then_some(limit);
                    let (height, fps) = demand.picture(video.kind, tier, video.smooth());
                    let cap = Some((tier, height, fps, bitrate, limit));
                    if cap != video.demand && video.throttle.allow(since) {
                        video.demand = cap;
                        let request = QualityRequest {
                            publisher: id,
                            kind: video.kind,
                            tier: tier as u8,
                        };
                        messages.push((
                            id,
                            ServerMessage::Quality {
                                request: hex::encode(request.to_bytes()),
                                source: source.mid.to_string(),
                                max_height: height,
                                max_fps: fps,
                                max_bitrate: bitrate,
                                limit,
                            },
                        ));
                    }
                }
            }
        }
        for (id, message) in messages {
            self.send(id, message);
        }
    }

    /// Asks a publisher for a keyframe of a video source, at most once per
    /// [`KEYFRAME_INTERVAL`].
    fn keyframe(&mut self, from: PeerId, mid: Mid, now: Instant) {
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        let Some(source) = peer.sources.iter_mut().find(|s| s.mid == mid) else {
            return;
        };
        if source.kind != MediaKind::Video
            || source
                .keyframe_asked
                .is_some_and(|at| now.saturating_duration_since(at) < KEYFRAME_INTERVAL)
        {
            return;
        }
        source.keyframe_asked = Some(now);
        let rid = source.layer.flatten();
        if let Some(rtc) = peer.publish.as_mut()
            && let Some(stream) = rtc.direct_api().stream_rx_by_mid(mid, rid)
        {
            stream.request_keyframe(KeyframeRequestKind::Pli);
            self.dirty.push((from, Leg::Publish));
        }
    }
}

pub fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[derive(Clone, Copy, Default)]
struct ScreenShare {
    screens: u64,
    reserved: u64,
}

impl ScreenShare {
    fn reserve(&mut self, other: Self) {
        self.screens += other.screens;
        self.reserved += other.reserved;
    }

    fn rate(self, wire_budget: u64) -> u64 {
        wire_budget.saturating_sub(self.reserved * 10 / 9) / self.screens.max(1)
    }
}

/// One receiver's stream of one video source, for the budgets.
struct Planned {
    receiver: PeerId,
    /// An index into the receiver's forwards.
    forward: usize,
    /// The decode targets its tier allows, lowest first; none without a descriptor.
    choices: Vec<Choice>,
    /// Whether a full second has measured the source's rates.
    measured: bool,
    mode: Mode,
    /// The most its tier needs, bits per second.
    ceiling: u64,
    limit: VideoLimit,
}

/// Maximum receiver demand with a shared cap when the publisher sends one layer.
struct Demand {
    requested: (usize, VideoLimit),
    /// The highest tier a receiver asks for and decodes, before the budgets.
    wanted: usize,
    shared_cap: (usize, VideoLimit),
    screen_rate: Option<(u64, VideoLimit)>,
}

impl Demand {
    fn request(&mut self, tier: usize, limit: VideoLimit) {
        let (highest, current_limit) = self.requested;
        if tier > highest || tier == highest && limit < current_limit {
            self.requested = (tier, limit);
        }
    }

    fn screen_rate(&mut self, cap: (u64, VideoLimit), single_layer: bool) {
        self.screen_rate = Some(self.screen_rate.map_or(cap, |previous| {
            if single_layer {
                previous.min(cap)
            } else {
                previous.max(cap)
            }
        }));
    }

    /// The picture height and frame rate of `tier`. Below the tier receivers
    /// want, a Sharp source keeps that tier's height and a Smooth one its frame
    /// rate.
    fn picture(&self, kind: Kind, tier: usize, smooth: bool) -> (u16, u8) {
        let (wanted, cap) = (kind.ladder()[self.wanted.max(tier)], kind.ladder()[tier]);
        if smooth {
            (cap.height, wanted.fps)
        } else {
            (wanted.height, cap.fps)
        }
    }

    fn cap(&self, kind: Kind) -> (usize, u64, VideoLimit) {
        let (tier, mut limit) = if self.shared_cap.0 < self.requested.0 {
            self.shared_cap
        } else {
            self.requested
        };
        let mut bitrate = kind.ladder()[tier].ceiling;
        if let Some((rate, reason)) = self.screen_rate {
            // Round down to avoid retuning encoders for tiny estimate changes.
            bitrate = (rate / 100_000 * 100_000).max(64_000);
            if limit == VideoLimit::WithinTarget || reason == VideoLimit::TrafficBudget {
                limit = reason;
            }
        }
        (tier, bitrate, limit)
    }
}

impl Default for Demand {
    fn default() -> Self {
        Self {
            requested: (0, VideoLimit::ForwardingBacklog),
            wanted: 0,
            shared_cap: (usize::MAX, VideoLimit::WithinTarget),
            screen_rate: None,
        }
    }
}

fn rates(planned: &[Planned]) -> Vec<Vec<u64>> {
    planned
        .iter()
        .map(|p| p.choices.iter().map(|c| c.rate).collect())
        .collect()
}

/// The receiver's payload type, among its negotiated `pts`, for packets sent in
/// `sent`: the same format, else any AV1 format for AV1. An AV1 format's
/// level-idx states what its side receives (AV1 RTP §7.2), so it says nothing
/// about the stream a publisher sends.
fn receiver_pt(config: &CodecConfig, pts: &[Pt], sent: PayloadParams) -> Option<Pt> {
    let negotiated = |p: &&PayloadParams| pts.contains(&p.pt());
    let exact = config.match_params(sent).filter(negotiated);
    let av1 = || {
        let mut params = config.params().iter().filter(negotiated);
        params.find(|p| sent.spec().codec == Codec::Av1 && p.spec().codec == Codec::Av1)
    };
    exact.or_else(av1).map(|p| p.pt())
}

/// The mode a sender chose in `modes` for its video source at `mid`.
fn mode(modes: &[(Mid, Mode)], mid: Mid, kind: Kind) -> Mode {
    modes.iter().find(|(m, _)| *m == mid).map_or_else(
        || Mode::default_for(kind == Kind::Screen),
        |(_, mode)| *mode,
    )
}

fn forward(from: PeerId, source: Mid, kind: MediaKind, label: &'static str) -> Forward {
    Forward {
        from,
        source,
        kind,
        label,
        view: quality::View::default(),
        mid: None,
        open: false,
        stopping: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_reservations_leave_room_for_a_camera_and_two_active_shares() {
        let usage = ScreenShare {
            screens: 1,
            reserved: 4_000_000,
        };
        assert_eq!(usage.rate(25_000_000), 20_555_556);
        let mut room = usage;
        room.reserve(usage);
        assert_eq!(room.rate(25_000_000), 8_055_556);
        assert_eq!(room.rate(3_000_000), 0);
    }

    #[test]
    fn screen_geometry_and_path_rate_are_independent() {
        let mut shared = Demand::default();
        let mut layered = Demand::default();
        for (demand, single_layer) in [(&mut shared, true), (&mut layered, false)] {
            demand.request(2, VideoLimit::WithinTarget);
            assert_eq!(demand.cap(Kind::Screen).1, 8_000_000);
            demand.screen_rate((23_031_000, VideoLimit::SfuViewer), single_layer);
            assert_eq!(
                demand.cap(Kind::Screen),
                (2, 23_000_000, VideoLimit::SfuViewer)
            );
            demand.screen_rate((9_000_000, VideoLimit::ReceiverBandwidth), single_layer);
        }
        assert_eq!(
            shared.cap(Kind::Screen),
            (2, 9_000_000, VideoLimit::ReceiverBandwidth)
        );
        assert_eq!(
            layered.cap(Kind::Screen),
            (2, 23_000_000, VideoLimit::SfuViewer)
        );
    }

    #[test]
    fn a_lowered_demand_keeps_what_the_mode_keeps() {
        let mut demand = Demand::default();
        demand.request(0, VideoLimit::ForwardingBacklog);
        demand.wanted = 3;
        assert_eq!(demand.picture(Kind::Screen, 0, false), (2160, 15));
        assert_eq!(demand.picture(Kind::Screen, 0, true), (720, 60));
        demand.wanted = 0;
        assert_eq!(demand.picture(Kind::Screen, 0, false), (720, 15));
    }

    fn av1(pt: u8, profile: u8, level_idx: u8) -> PayloadParams {
        let format = FormatParams {
            profile: Some(profile),
            level_idx: Some(level_idx),
            ..FormatParams::default()
        };
        let spec = str0m::format::CodecSpec {
            codec: Codec::Av1,
            clock_rate: Frequency::NINETY_KHZ,
            channels: None,
            format,
        };
        PayloadParams::new(pt.into(), None, spec)
    }

    #[test]
    fn av1_goes_to_the_receivers_av1_format_at_any_level() {
        let config = CodecConfig::new_from_payload_params(vec![av1(45, 0, 5), av1(47, 0, 9)]);
        let (both, firefox) = ([45.into(), 47.into()], [47.into()]);
        assert_eq!(receiver_pt(&config, &both, av1(35, 0, 5)), Some(45.into()));
        assert_eq!(receiver_pt(&config, &both, av1(35, 0, 9)), Some(47.into()));
        assert_eq!(
            receiver_pt(&config, &firefox, av1(35, 0, 5)),
            Some(47.into())
        );
        assert_eq!(
            receiver_pt(&config, &[], av1(35, 0, 5)),
            None,
            "not negotiated"
        );
    }
}
