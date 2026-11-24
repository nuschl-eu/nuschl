// SPDX-License-Identifier: EUPL-1.2
//! The SFU binary on loopback with three str0m peers standing in for browsers.
//! A host creates the room; two participants join with the link key alone. The
//! host publishes AV1 and one participant VP9; every peer receives the others'
//! RTP payloads byte for byte with consecutive sequence numbers. Key messages
//! reach the named peer only. Wrong proofs, unknown rooms, oversized key
//! messages and host actions by participants are refused. The call codec
//! follows the peers' decoders. The creator's lease renews with `host_key`, and
//! ending it disconnects everyone. Chromium's layered AV1, recorded at the sender
//! (`tests/fixtures`), reaches each receiver as frames a decoder can use.

use futures_util::{SinkExt, StreamExt};
use hmac::{KeyInit, Mac};
use nuschl_stream_quality::{Kind, QualityRequest, dd::Layers};
use nuschl_stream_sfu::quality::{DESCRIPTOR_ID, Descriptor, descriptor_extension};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::UdpSocket,
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    format::Codec,
    media::{Direction, MediaKind, Mid},
    net::{Protocol, Receive},
    rtp::{ExtensionValues, RtpWrite},
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const ADMIT: [u8; 32] = [1; 32];
const HOST: [u8; 32] = [2; 32];
const ROOM: [u8; 16] = [3; 16];

fn start(settings: &str) -> nuschl_testkit::sfu::Sfu {
    nuschl_testkit::sfu::start(env!("CARGO_BIN_EXE_nuschl-stream-sfu"), settings)
}

/// `POST` or `DELETE /sfu/lease`; returns the status code.
fn lease(url: &str, method: &str, stream_id: [u8; 16], host_key: [u8; 32]) -> u16 {
    use std::io::{Read, Write};
    let address = url.trim_start_matches("ws://").trim_end_matches("/sfu");
    let body =
        json!({"stream_id": hex::encode(stream_id), "host_key": hex::encode(host_key)}).to_string();
    let mut stream = std::net::TcpStream::connect(address).expect("connect");
    write!(
        stream,
        "{method} /sfu/lease HTTP/1.1\r\nHost: sfu\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    response
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("code")
}

fn prove(key: &[u8; 32], label: &[u8], nonce: &[u8]) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("key");
    mac.update(label);
    mac.update(nonce);
    hex::encode(mac.finalize().into_bytes())
}

/// One WebRTC connection on its own loopback socket.
struct Leg {
    rtc: Rtc,
    socket: UdpSocket,
    /// Each event with the time its datagram reached the socket.
    events: Vec<(Instant, Event)>,
    arrived: Instant,
    /// Drops every nth incoming AV1 media packet; retransmissions pass.
    loss: Option<(u64, u64)>,
    link: Option<Link>,
}

/// A downlink in front of a test peer: `rate` bits per second, a drop-tail
/// buffer of `buffer` and `delay` one way.
struct Link {
    rate: f64,
    delay: Duration,
    buffer: Duration,
    free: Instant,
    /// Each datagram with the time it leaves the link and the time it arrived.
    queue: std::collections::VecDeque<(Instant, Instant, std::net::SocketAddr, Vec<u8>)>,
}

impl Link {
    fn new(rate: u64, delay: Duration, buffer: Duration) -> Self {
        Self {
            rate: rate as f64,
            delay,
            buffer,
            free: Instant::now(),
            queue: Default::default(),
        }
    }

    fn arrive(&mut self, now: Instant, source: std::net::SocketAddr, datagram: &[u8]) {
        let start = self.free.max(now);
        if start.saturating_duration_since(now) > self.buffer {
            return;
        }
        // With the IPv4 and UDP headers.
        self.free = start + Duration::from_secs_f64((datagram.len() + 28) as f64 * 8.0 / self.rate);
        self.queue
            .push_back((self.free + self.delay, now, source, datagram.to_vec()));
    }
}

impl Leg {
    fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("udp");
        socket.set_nonblocking(true).expect("nonblocking");
        let mut rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .set_rtp_mode(true)
            .set_extension(DESCRIPTOR_ID, descriptor_extension())
            .build(Instant::now());
        rtc.add_local_candidate(
            Candidate::host(socket.local_addr().expect("address"), "udp").expect("candidate"),
        );
        Self {
            rtc,
            socket,
            events: Vec::new(),
            arrived: Instant::now(),
            loss: None,
            link: None,
        }
    }

    fn lost(&mut self, datagram: &[u8]) -> bool {
        let Some((every, count)) = &mut self.loss else {
            return false;
        };
        let rtp = datagram.len() > 12 && datagram[0] >> 6 == 2;
        let pt = datagram.get(1).map(|b| b & 0x7f);
        let av1 = self
            .rtc
            .codec_config()
            .params()
            .iter()
            .any(|p| p.spec().codec == Codec::Av1 && Some(*p.pt()) == pt);
        if !rtp || !av1 {
            return false;
        }
        *count += 1;
        *count % *every == 0
    }

    fn poll(&mut self) {
        loop {
            match self.rtc.poll_output().expect("poll") {
                Output::Timeout(_) => return,
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Output::Event(event) => self.events.push((self.arrived, event)),
            }
        }
    }

    fn step(&mut self) {
        self.poll();
        let local = self.socket.local_addr().expect("address");
        let mut buffer = [0; 2048];
        while let Ok((length, source)) = self.socket.recv_from(&mut buffer) {
            if self.lost(&buffer[..length]) {
                continue;
            }
            if let Some(link) = &mut self.link {
                link.arrive(Instant::now(), source, &buffer[..length]);
                continue;
            }
            self.arrived = Instant::now();
            let input = Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents: buffer[..length].try_into().expect("datagram"),
                },
            );
            self.rtc.handle_input(input).expect("input");
            self.poll();
        }
        let now = Instant::now();
        while let Some(link) = &mut self.link
            && link.queue.front().is_some_and(|(due, ..)| *due <= now)
        {
            let (_, arrived, source, datagram) = link.queue.pop_front().expect("a datagram");
            self.arrived = arrived;
            let input = Input::Receive(
                now,
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents: datagram.as_slice().try_into().expect("datagram"),
                },
            );
            self.rtc.handle_input(input).expect("input");
            self.poll();
        }
        self.rtc
            .handle_input(Input::Timeout(Instant::now()))
            .expect("timeout");
        self.poll();
    }
}

/// A packet a subscriber received from one publisher.
struct Received {
    /// When it reached the subscriber's socket.
    at: Instant,
    seq: u64,
    pt: u8,
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
    descriptor: Option<Descriptor>,
}

/// One frame of a recorded source (`tests/fixtures/chrome-*.txt`).
struct Recorded {
    delay: Duration,
    timestamp: u32,
    packets: usize,
    bytes: usize,
    marker: bool,
    first: Vec<u8>,
    rest: Option<Vec<u8>>,
}

fn fixture(text: &str) -> Vec<Recorded> {
    let hex = |text: &str| hex::decode(text).expect("hex");
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let f: Vec<&str> = line.split(' ').collect();
            let number = |i: usize| f[i].parse::<u64>().expect("a number");
            Recorded {
                delay: Duration::from_millis(number(0)),
                timestamp: number(1) as u32,
                packets: number(2) as usize,
                bytes: number(3) as usize,
                marker: f[4] == "1",
                first: hex(f[5]),
                rest: (f[6] != "-").then(|| hex(f[6])),
            }
        })
        .collect()
}

/// A recorded source sent frame by frame at its recorded pace. The end of the
/// recording, and a key frame request after [`KEY_DELAY`], start it over at its
/// key picture; frame numbers and timestamps keep rising.
struct Replay {
    frames: Vec<Recorded>,
    next: usize,
    /// Added to the recorded frame numbers.
    offset: u16,
    number: u16,
    timestamp: u32,
    sent_at: Option<Instant>,
    restart: Option<Instant>,
    /// When each frame number went out.
    sent: HashMap<u16, Instant>,
}

/// How long a replayed publisher takes to answer a key frame request, like a
/// busy encoder: frames already on their way arrive first.
const KEY_DELAY: Duration = Duration::from_millis(250);

impl Replay {
    fn new(frames: Vec<Recorded>) -> Self {
        Self {
            frames,
            next: 0,
            offset: 0,
            number: 0,
            timestamp: 0,
            sent_at: None,
            restart: None,
            sent: HashMap::new(),
        }
    }
}

/// What a libwebrtc receiver can decode of `packets`: a frame is whole when its
/// packets from start to end have consecutive sequence numbers, and decodable
/// when every frame it references is. Every temporal unit of whole frames but
/// the last, which may still be arriving, must end with exactly one marker, on
/// its last packet. Returns the decodable frames' numbers and spatial layers,
/// and the whole frames that are not decodable.
fn decodable(packets: &[Received]) -> (Vec<(u16, u8)>, Vec<u16>) {
    // Retransmissions arrive late.
    let mut sorted: Vec<&Received> = packets.iter().collect();
    sorted.sort_by_key(|p| p.seq);
    #[derive(Default)]
    struct Frame {
        number: u16,
        start: Option<u64>,
        end: Option<u64>,
        packets: u64,
        spatial: u8,
        references: Vec<u16>,
    }
    let mut layers = Layers::default();
    let mut frames: Vec<Frame> = Vec::new();
    let mut indices = HashMap::new();
    for &packet in &sorted {
        let descriptor = packet.descriptor.as_ref().expect("a descriptor");
        let Some(frame) = layers.packet(&descriptor.0, 0, 0, Duration::ZERO) else {
            continue;
        };
        let index = *indices.entry(frame.number).or_insert_with(|| {
            frames.push(Frame {
                number: frame.number,
                spatial: frame.layer.spatial,
                ..Frame::default()
            });
            frames.len() - 1
        });
        let entry = &mut frames[index];
        entry.packets += 1;
        if frame.start {
            entry.start = Some(packet.seq);
            entry.references = frame.references().collect();
        }
        if frame.end {
            entry.end = Some(packet.seq);
        }
    }
    let whole = |frame: &Frame| {
        let (Some(start), Some(end)) = (frame.start, frame.end) else {
            return false;
        };
        end + 1 - start == frame.packets
    };
    // The last unit may still be arriving.
    let units: Vec<&[&Received]> = sorted.chunk_by(|a, b| a.timestamp == b.timestamp).collect();
    for unit in units.iter().rev().skip(1) {
        let numbers = unit
            .iter()
            .filter_map(|p| Some(p.descriptor.as_ref()?.0[1..3].to_vec()));
        if !numbers
            .map(|n| u16::from_be_bytes([n[0], n[1]]))
            .all(|n| indices.get(&n).is_some_and(|&i| whole(&frames[i])))
        {
            continue;
        }
        let markers: Vec<bool> = unit.iter().map(|p| p.marker).collect();
        assert_eq!(
            markers.iter().filter(|&&m| m).count(),
            1,
            "one marker per temporal unit {}",
            unit[0].timestamp
        );
        assert!(
            markers.last().expect("a packet"),
            "the marker ends the unit"
        );
    }
    let mut decoded: Vec<(u16, u8)> = Vec::new();
    let mut numbers = std::collections::HashSet::new();
    let mut broken = Vec::new();
    for frame in frames.iter().filter(|f| whole(f)) {
        if frame.references.iter().all(|r| numbers.contains(r)) {
            numbers.insert(frame.number);
            decoded.push((frame.number, frame.spatial));
        } else {
            broken.push(frame.number);
        }
    }
    (decoded, broken)
}

/// An L1T3 Dependency Descriptor with libwebrtc's decode target indications:
/// templates T0, T1, T2 and decode targets for a quarter, half and all of the
/// frame rate. T0 frames are switch points for all three, T1 frames for the
/// full rate.
fn l1t3(template: u32, number: u16, structure: bool) -> Vec<u8> {
    let mut bits = Vec::new();
    let mut put = |value: u32, n: u32| {
        for i in (0..n).rev() {
            bits.push((value >> i) & 1 == 1);
        }
    };
    put(0b11, 2); // start and end of frame
    put(template, 6);
    put(u32::from(number), 16);
    if structure {
        put(0b10000, 5);
        put(0, 6); // template id offset
        put(2, 5); // three decode targets
        put(0b01_01_11, 6); // templates T0, T1, T2
        put(0b10_10_10, 6); // T0: switch for all
        put(0b00_01_10, 6); // T1: discardable for half, switch for full
        put(0b00_00_01, 6); // T2: discardable for full
        put(0, 3); // no frame diffs
        put(0, 2); // no chains
        put(0, 1); // no resolutions
    }
    while bits.len() % 8 != 0 {
        bits.push(false);
    }
    bits.chunks(8)
        .map(|byte| byte.iter().fold(0, |a, &b| (a << 1) | u8::from(b)))
        .collect()
}

struct Peer {
    to_sfu: mpsc::UnboundedSender<String>,
    from_sfu: mpsc::UnboundedReceiver<Value>,
    id: u64,
    publish: Option<(Leg, Option<SdpPendingOffer>, Mid, Codec)>,
    subscribe: Option<Leg>,
    tracks: HashMap<String, u64>,
    received: HashMap<u64, Vec<Received>>,
    /// Payloads and descriptors sent.
    sent: Vec<(Vec<u8>, Option<Descriptor>)>,
    seq: u64,
    frame: u32,
    frame_packets: usize,
    last_frame: Option<Instant>,
    replay: Option<Replay>,
    keyframe_requests: usize,
    messages: Vec<Value>,
}

async fn connect(
    url: &str,
) -> (
    mpsc::UnboundedSender<String>,
    mpsc::UnboundedReceiver<Value>,
) {
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");
    let (to_sfu, mut outgoing) = mpsc::unbounded_channel::<String>();
    let (incoming, from_sfu) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                message = outgoing.recv() => match message {
                    Some(text) => if socket.send(Message::text(text)).await.is_err() { return },
                    None => return,
                },
                message = socket.next() => match message {
                    Some(Ok(Message::Text(text))) => {
                        let _ = incoming.send(serde_json::from_str(&text).expect("json"));
                    }
                    Some(Ok(_)) => {}
                    _ => {
                        let _ = incoming.send(json!({"type": "closed"}));
                        return;
                    }
                },
            }
        }
    });
    (to_sfu, from_sfu)
}

async fn next(from_sfu: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), from_sfu.recv())
        .await
        .expect("a message in time")
        .expect("open")
}

/// Joins with the admit proof, the host proof and the room's keys as asked,
/// decoding AV1 and VP9; returns the peer and the SFU's first answer.
async fn join(url: &str, admit: &[u8; 32], host: bool, create: bool) -> (Peer, Value) {
    join_decoding(url, admit, host, create, &["av1", "vp9"], false).await
}

async fn join_decoding(
    url: &str,
    admit: &[u8; 32],
    host: bool,
    create: bool,
    codecs: &[&str],
    video_limits: bool,
) -> (Peer, Value) {
    let (to_sfu, mut from_sfu) = connect(url).await;
    let hello = next(&mut from_sfu).await;
    assert_eq!(hello["type"], "hello");
    let nonce = hex::decode(hello["nonce"].as_str().expect("nonce")).expect("hex");
    let mut message = json!({
        "type": "join",
        "room": hex::encode(ROOM),
        "verifier": hex::encode(<sha2::Sha256 as sha2::Digest>::digest(HOST)),
        "proof": prove(admit, b"nuschl/stream/v2 sfu admit", &nonce),
        "codecs": codecs,
    });
    if video_limits {
        assert_eq!(hello["video_limits"], true);
        message["video_limits"] = true.into();
    }
    if host {
        message["host_proof"] = prove(&HOST, b"nuschl/stream/v2 sfu host", &nonce).into();
    }
    if create {
        message["create"] = json!({"admit_key": hex::encode(ADMIT), "host_key": hex::encode(HOST)});
    }
    to_sfu.send(message.to_string()).expect("send");
    let answer = next(&mut from_sfu).await;
    let id = answer["peer"].as_u64().unwrap_or(0);
    let peer = Peer {
        to_sfu,
        from_sfu,
        id,
        publish: None,
        subscribe: None,
        tracks: HashMap::new(),
        received: HashMap::new(),
        sent: Vec::new(),
        seq: 0,
        frame: 0,
        frame_packets: 1,
        last_frame: None,
        replay: None,
        keyframe_requests: 0,
        messages: Vec::new(),
    };
    (peer, answer)
}

impl Peer {
    /// Sends a message; a closed connection shows in the messages received.
    fn send(&self, message: Value) {
        let _ = self.to_sfu.send(message.to_string());
    }

    fn start_publishing(&mut self, codec: Codec) {
        self.publish_source(codec, false);
    }

    fn publish_source(&mut self, codec: Codec, screen: bool) {
        let mut leg = Leg::new();
        let mut api = leg.rtc.sdp_api();
        let mid = api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = api.apply().expect("offer");
        let screens: Vec<String> = if screen {
            vec![mid.to_string()]
        } else {
            Vec::new()
        };
        self.send(json!({"type": "offer", "sdp": offer.to_sdp_string(), "screens": screens}));
        self.publish = Some((leg, Some(pending), mid, codec));
    }

    /// Stops the published source with a new offer.
    fn stop_publishing(&mut self) {
        let (leg, pending, mid, _) = self.publish.as_mut().expect("publishing");
        let mut api = leg.rtc.sdp_api();
        api.stop_media(*mid);
        let (offer, next) = api.apply().expect("offer");
        *pending = Some(next);
        self.send(json!({"type": "offer", "sdp": offer.to_sdp_string()}));
    }

    fn step(&mut self) {
        while let Ok(message) = self.from_sfu.try_recv() {
            match message["type"].as_str() {
                Some("offer") => {
                    let leg = self.subscribe.get_or_insert_with(Leg::new);
                    let offer = SdpOffer::from_sdp_string(message["sdp"].as_str().expect("sdp"))
                        .expect("offer");
                    let answer = leg.rtc.sdp_api().accept_offer(offer).expect("accept");
                    self.tracks.clear();
                    for track in message["tracks"].as_array().expect("tracks") {
                        let mid = track["mid"].as_str().expect("mid").to_owned();
                        self.tracks
                            .insert(mid, track["peer"].as_u64().expect("peer"));
                    }
                    self.send(json!({"type": "answer", "sdp": answer.to_sdp_string()}));
                }
                Some("answer") => {
                    let (leg, pending, ..) = self.publish.as_mut().expect("publishing");
                    let answer = SdpAnswer::from_sdp_string(message["sdp"].as_str().expect("sdp"))
                        .expect("answer");
                    let pending = pending.take().expect("one offer");
                    leg.rtc
                        .sdp_api()
                        .accept_answer(pending, answer)
                        .expect("accept");
                }
                _ => self.messages.push(message),
            }
        }
        if let Some(leg) = &mut self.subscribe {
            leg.step();
            for (at, event) in leg.events.drain(..) {
                if let Event::RtpPacket(packet) = event {
                    let mid = leg
                        .rtc
                        .direct_api()
                        .stream_rx(&packet.header.ssrc)
                        .map(|s| s.mid().to_string())
                        .expect("a stream");
                    let from = self.tracks[&mid];
                    self.received.entry(from).or_default().push(Received {
                        at,
                        seq: *packet.seq_no,
                        pt: *packet.header.payload_type,
                        timestamp: packet.header.timestamp,
                        marker: packet.header.marker,
                        payload: packet.payload.to_vec(),
                        descriptor: packet
                            .header
                            .ext_vals
                            .user_values
                            .get::<Descriptor>()
                            .cloned(),
                    });
                }
            }
        }
        if let Some((leg, ..)) = &mut self.publish {
            leg.step();
            for (_, event) in leg.events.drain(..) {
                if let Event::KeyframeRequest(_) = event {
                    self.keyframe_requests += 1;
                    if let Some(replay) = &mut self.replay {
                        replay.restart.get_or_insert(Instant::now() + KEY_DELAY);
                    }
                }
            }
        }
    }

    /// Sends the recorded frames that are due, AV1 with their descriptors.
    fn send_replay(&mut self) {
        let (Some((leg, None, mid, _)), Some(replay)) = (&mut self.publish, &mut self.replay)
        else {
            return;
        };
        if !leg.rtc.is_connected() {
            return;
        }
        let media = leg.rtc.media(*mid).expect("media");
        let pt = leg
            .rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| p.spec().codec == Codec::Av1 && media.remote_pts().contains(&p.pt()))
            .expect("AV1")
            .pt();
        let mut api = leg.rtc.direct_api();
        let stream = api.stream_tx_by_mid(*mid, None).expect("stream");
        let now = Instant::now();
        loop {
            let at_unit_start =
                replay.next == replay.frames.len() || replay.frames[replay.next].timestamp != 0;
            let requested = replay.restart.is_some_and(|at| at <= now);
            if at_unit_start && (requested || replay.next == replay.frames.len()) {
                let recorded = |f: &Recorded| u16::from_be_bytes([f.first[1], f.first[2]]);
                replay.offset = replay
                    .number
                    .wrapping_add(1)
                    .wrapping_sub(recorded(&replay.frames[0]));
                replay.next = 0;
                replay.timestamp = replay.timestamp.wrapping_add(3000);
                replay.restart = None;
            }
            let frame = &replay.frames[replay.next];
            let due = replay.sent_at.map_or(now, |at| at + frame.delay);
            if due > now {
                return;
            }
            replay.sent_at = Some(due.max(now - Duration::from_millis(100)));
            replay.next += 1;
            replay.timestamp = replay.timestamp.wrapping_add(frame.timestamp);
            replay.number =
                u16::from_be_bytes([frame.first[1], frame.first[2]]).wrapping_add(replay.offset);
            replay.sent.insert(replay.number, now);
            let size = frame.bytes / frame.packets;
            for index in 0..frame.packets {
                let (first, last) = (index == 0, index + 1 == frame.packets);
                let mut descriptor = match (&frame.rest, first) {
                    (_, true) => frame.first.clone(),
                    (Some(rest), false) => rest.clone(),
                    (None, false) => frame.first[..3].to_vec(),
                };
                descriptor[0] =
                    (descriptor[0] & 0x3f) | (u8::from(first) << 7) | (u8::from(last) << 6);
                descriptor[1..3].copy_from_slice(&replay.number.to_be_bytes());
                self.seq += 1;
                let mut payload = vec![0x10; size.max(3)];
                payload[1..3].copy_from_slice(&replay.number.to_be_bytes());
                let mut values = ExtensionValues::default();
                values.user_values.set(Descriptor(descriptor));
                stream.write_rtp(
                    RtpWrite::new(pt, self.seq.into(), replay.timestamp, now, payload)
                        .marker(last && frame.marker)
                        .ext_vals(values)
                        .nackable(true),
                );
            }
        }
    }

    /// Sends a synthetic encrypted frame once connected, at 30 frames a second.
    fn send_frame(&mut self) {
        let Some((leg, None, mid, codec)) = &mut self.publish else {
            return;
        };
        if !leg.rtc.is_connected()
            || self
                .last_frame
                .is_some_and(|at| at.elapsed() < Duration::from_millis(33))
        {
            return;
        }
        self.last_frame = Some(Instant::now());
        let media = leg.rtc.media(*mid).expect("media");
        let pt = leg
            .rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| p.spec().codec == *codec && media.remote_pts().contains(&p.pt()))
            .expect("negotiated codec")
            .pt();
        // The one-packet fixture sends T0 T2 T1 T2. Multi-packet frames
        // all belong to the base target, like a high-rate single-layer share.
        let number = self.frame as u16;
        self.frame += 1;
        let template = if self.frame_packets == 1 {
            [0, 2, 1, 2][usize::from(number) % 4]
        } else {
            0
        };
        let timestamp = self.frame * 3000;
        let mut api = leg.rtc.direct_api();
        let stream = api.stream_tx_by_mid(*mid, None).expect("stream");
        for index in 0..self.frame_packets {
            self.seq += 1;
            let first = index == 0;
            let last = index + 1 == self.frame_packets;
            // An AV1 aggregation header with bytes standing in for SFrame ciphertext.
            let header = if first && number.is_multiple_of(10) {
                0x18
            } else {
                0x10
            };
            let mut payload = vec![header];
            payload.extend_from_slice(&self.id.to_be_bytes());
            payload.extend_from_slice(&self.seq.to_be_bytes());
            payload.extend((0..600).map(|i| (i as u64 * 31 + self.seq) as u8));
            let descriptor = (*codec == Codec::Av1).then(|| {
                let mut bytes = l1t3(template, number, template == 0 && first);
                bytes[0] = (bytes[0] & 0x3f) | (u8::from(first) << 7) | (u8::from(last) << 6);
                Descriptor(bytes)
            });
            let mut values = ExtensionValues::default();
            if let Some(descriptor) = &descriptor {
                values.user_values.set(descriptor.clone());
            }
            stream.write_rtp(
                RtpWrite::new(
                    pt,
                    self.seq.into(),
                    timestamp,
                    Instant::now(),
                    payload.clone(),
                )
                .marker(last)
                .ext_vals(values)
                .nackable(true),
            );
            self.sent.push((payload, descriptor));
        }
    }

    fn count(&self, from: u64) -> usize {
        self.received.get(&from).map_or(0, Vec::len)
    }

    /// The first message of `kind` received so far, or within ten seconds.
    async fn expect(&mut self, kind: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(index) = self.messages.iter().position(|m| m["type"] == kind) {
                return self.messages.remove(index);
            }
            assert!(Instant::now() < deadline, "no {kind}: {:?}", self.messages);
            self.step();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// The frame numbers a receiver got from `publisher`, read from the payloads.
fn frames(receiver: &Peer, publisher: &Peer) -> Vec<u64> {
    receiver
        .received
        .get(&publisher.id)
        .map_or_else(Vec::new, |packets| {
            packets
                .iter()
                .map(|p| u64::from_be_bytes(p.payload[9..17].try_into().expect("8 bytes")) - 1)
                .collect()
        })
}

fn quality_request(publisher: u64, tier: u8) -> Value {
    source_request(publisher, Kind::Camera, tier)
}

fn source_request(publisher: u64, kind: Kind, tier: u8) -> Value {
    let request = QualityRequest {
        publisher,
        kind,
        tier,
    };
    json!({"type": "quality", "request": hex::encode(request.to_bytes())})
}

fn assert_forwarded(receiver: &Peer, publisher: &Peer, codec: Codec) {
    let sent: HashMap<&Vec<u8>, &Option<Descriptor>> =
        publisher.sent.iter().map(|(p, d)| (p, d)).collect();
    let received = &receiver.received[&publisher.id];
    let leg = receiver.subscribe.as_ref().expect("subscribed");
    for (index, packet) in received.iter().enumerate() {
        let descriptor = sent
            .get(&packet.payload)
            .expect("payload changed or invented");
        assert_eq!(
            **descriptor, packet.descriptor,
            "the descriptor arrives as sent"
        );
        assert_eq!(
            packet.seq, index as u64,
            "consecutive sequence numbers from zero"
        );
        let params = leg
            .rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| *p.pt() == packet.pt);
        assert_eq!(params.map(|p| p.spec().codec), Some(codec));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn video_limit_is_source_scoped_and_can_clear_on_a_quiet_source() {
    use nuschl_stream_quality::DecodeCapacity;

    let sfu = start("");
    let url = &sfu.url;
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    let (mut receiver, _) = join_decoding(url, &ADMIT, false, false, &["av1"], true).await;
    let (mut legacy, _) = join(url, &ADMIT, false, false).await;
    host.start_publishing(Codec::Av1);
    host.expect("quality").await;
    let report = receiver.expect("video_limit").await;
    let mid = report["mid"].as_str().expect("mid");
    assert_eq!(receiver.tracks[mid], host.id);
    assert_eq!(report["peer"], host.id);
    assert_eq!(report["limit"], "within_target");
    assert!(receiver.received.is_empty());
    for (camera, expected) in [(0, "receiver_decode"), (3, "within_target")] {
        let decode = DecodeCapacity { camera, screen: 3 };
        receiver.send(json!({"type": "capacity", "decode": hex::encode(decode.to_bytes())}));
        let report = receiver.expect("video_limit").await;
        assert_eq!(
            report,
            json!({"type": "video_limit", "mid": mid, "peer": host.id, "limit": expected})
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    legacy.step();
    assert!(
        !legacy
            .messages
            .iter()
            .any(|message| message["type"] == "video_limit")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_media_and_key_messages_unchanged() {
    let sfu = start("");
    let url = sfu.url.as_str();

    // Refusals before the room exists, and creating it without P.
    let (_, refused) = join(url, &ADMIT, false, false).await;
    assert_eq!(refused, json!({"type": "error", "reason": "no_room"}));
    let (_, refused) = join(url, &ADMIT, false, true).await;
    assert_eq!(refused["reason"], "proof");

    let (mut host, welcome) = join(url, &ADMIT, true, true).await;
    assert_eq!(welcome["type"], "welcome");
    // The creator's device renews the lease with host_key.
    assert_eq!(lease(url, "POST", ROOM, HOST), 204);
    assert_eq!(lease(url, "POST", ROOM, HOST), 429, "at most once a second");
    assert_eq!(
        lease(url, "POST", ROOM, ADMIT),
        404,
        "another key names no room"
    );
    assert_eq!(lease(url, "POST", [4; 16], HOST), 404);
    assert_eq!(welcome["role"], "host");
    assert_eq!(welcome["codec"], "av1");
    let turn = &welcome["turn"];
    assert_eq!(turn["urls"], json!(["turn:turn.test:3478?transport=udp"]));
    assert!(turn["username"].as_str().expect("username").contains(':'));
    let (mut alice, welcome) = join(url, &ADMIT, false, false).await;
    assert_eq!(welcome["role"], "participant");
    assert_eq!(welcome["peers"], json!([{"id": host.id, "role": "host"}]));
    let (mut bob, _) = join(url, &ADMIT, false, false).await;
    let (_, refused) = join(url, &[9; 32], false, false).await;
    assert_eq!(refused["reason"], "proof", "a wrong link key");
    assert_eq!(host.expect("joined").await["peer"], alice.id);

    // The host publishes AV1, a participant VP9.
    host.start_publishing(Codec::Av1);
    alice.start_publishing(Codec::Vp9);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut alice, &mut bob] {
            peer.step();
        }
        host.send_frame();
        alice.send_frame();
        if [
            bob.count(host.id),
            bob.count(alice.id),
            alice.count(host.id),
            host.count(alice.id),
        ]
        .iter()
        .all(|&n| n >= 50)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "media did not arrive: {:?} {:?} {:?}",
            host.received.keys(),
            alice.received.keys(),
            bob.received.keys()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_forwarded(&bob, &host, Codec::Av1);
    assert_forwarded(&bob, &alice, Codec::Vp9);
    assert_forwarded(&alice, &host, Codec::Av1);
    assert_forwarded(&host, &alice, Codec::Vp9);
    assert!(!bob.received.contains_key(&bob.id) && !host.received.contains_key(&host.id));

    // The publisher hears the largest tier its receivers ask for.
    alice.send(quality_request(host.id, 0));
    bob.send(quality_request(host.id, 1));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let message = host.expect("quality").await;
        let bytes = hex::decode(message["request"].as_str().expect("hex")).expect("hex");
        let demand = QualityRequest::from_bytes(&bytes).expect("a request");
        assert_eq!((demand.publisher, demand.kind), (host.id, Kind::Camera));
        if demand.tier == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "no demand of tier 1");
    }

    // A publisher that stops its source leaves the subscribers' offers.
    alice.stop_publishing();
    let deadline = Instant::now() + Duration::from_secs(10);
    while bob.tracks.values().any(|&peer| peer == alice.id) {
        assert!(Instant::now() < deadline, "the stopped source stayed");
        for peer in [&mut host, &mut alice, &mut bob] {
            peer.step();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(bob.tracks.values().any(|&peer| peer == host.id));

    // A peer without AV1 switches the call to VP9 and back when it leaves; a
    // peer without VP9 cannot join then.
    let (vp9, welcome) = join_decoding(url, &ADMIT, false, false, &["vp9"], false).await;
    assert_eq!(welcome["codec"], "vp9");
    assert_eq!(bob.expect("codec").await["codec"], "vp9");
    let (_, refused) = join_decoding(url, &ADMIT, false, false, &["av1"], false).await;
    assert_eq!(refused, json!({"type": "error", "reason": "codec"}));
    let (_, refused) = join_decoding(url, &ADMIT, false, false, &[], false).await;
    assert_eq!(refused["reason"], "join");
    let vp9_id = vp9.id;
    drop(vp9);
    assert_eq!(bob.expect("codec").await["codec"], "av1");
    assert_eq!(host.expect("left").await["peer"], vp9_id);

    // Key messages reach the named peer, unchanged, with the sender's id.
    let data = "a5".repeat(16 * 1024);
    bob.send(json!({"type": "key", "to": host.id, "data": data}));
    let key = host.expect("key").await;
    assert_eq!(key, json!({"type": "key", "from": bob.id, "data": data}));
    assert!(!alice.messages.iter().any(|m| m["type"] == "key"));

    // Participants cannot act as hosts; the host removes a peer.
    alice.send(json!({"type": "remove", "peer": bob.id}));
    assert_eq!(alice.expect("error").await["reason"], "not_allowed");
    host.send(json!({"type": "remove", "peer": bob.id}));
    assert_eq!(bob.expect("error").await["reason"], "removed");
    assert_eq!(host.expect("left").await["peer"], alice.id);
    assert_eq!(host.expect("left").await["peer"], bob.id);

    // An oversized key message ends the sender's connection.
    let (mut carol, _) = join(url, &ADMIT, false, false).await;
    carol.send(json!({"type": "key", "to": host.id, "data": "a5".repeat(16 * 1024 + 1)}));
    assert_eq!(carol.expect("error").await["reason"], "key");

    // Ending the lease disconnects everyone and refuses new joins.
    assert_eq!(lease(url, "DELETE", ROOM, HOST), 204);
    assert_eq!(host.expect("error").await["reason"], "ended");
    assert_eq!(lease(url, "POST", ROOM, HOST), 404);
    let (_, refused) = join(url, &ADMIT, false, false).await;
    assert_eq!(refused["reason"], "ended");
}

/// One AV1 publisher with an L1T3 descriptor at 30 frames a second and two
/// receivers that join once a second has measured it. One asks for the 15 fps
/// camera tier and gets the T0 and T1 frames; the other gets every frame. Each
/// switches at switch points, and skipped frames leave no gaps in its sequence
/// numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_receiver_gets_the_layers_of_its_tier() {
    let sfu = start("");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    host.start_publishing(Codec::Av1);
    publish_for(&mut host, Duration::from_millis(1500)).await;
    let (mut low, _) = join(url, &ADMIT, false, false).await;
    let (mut full, _) = join(url, &ADMIT, false, false).await;
    let mut asked = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut low, &mut full] {
            peer.step();
        }
        host.send_frame();
        if !asked && !low.tracks.is_empty() {
            low.send(quality_request(host.id, 0));
            asked = true;
        }
        let tail = frames(&low, &host);
        let settled = tail.iter().rev().take_while(|n| *n % 2 == 0).count();
        if settled >= 20 && frames(&full, &host).len() >= 90 {
            break;
        }
        assert!(Instant::now() < deadline, "low {tail:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for receiver in [&low, &full] {
        let packets = &receiver.received[&host.id];
        assert!(
            packets.iter().enumerate().all(|(i, p)| p.seq == i as u64),
            "consecutive sequence numbers across skipped frames"
        );
    }
    // Every frame from the first one on, all layers.
    let all = frames(&full, &host);
    assert!(all.windows(2).all(|w| w[1] == w[0] + 1), "{all:?}");
    // T2 frames are the odd ones. The low receiver skips only those, and its
    // first skip follows a T0 frame, the switch point for the 15 fps target.
    let got = frames(&low, &host);
    let skipped: Vec<u64> = got.windows(2).flat_map(|w| w[0] + 1..w[1]).collect();
    assert!(
        !skipped.is_empty() && skipped.iter().all(|n| n % 2 == 1),
        "{got:?}"
    );
    let first_skip = got.windows(2).find(|w| w[1] > w[0] + 1).expect("a skip")[0];
    assert_eq!(first_skip % 4, 0, "{got:?}");
}

/// An L1T3 screen at 30 frames a second that starts while its viewer asks for
/// the 15 fps tier, as the native publisher's: the viewer gets the T0 frames
/// until a full second has measured the source, then the T0 and T1 frames,
/// and never a T2 frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_starting_screen_stays_within_its_viewers_tier() {
    let sfu = start("");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    let (mut viewer, _) = join(url, &ADMIT, false, false).await;
    host.publish_source(Codec::Av1, true);
    let deadline = Instant::now() + Duration::from_secs(10);
    while viewer.tracks.is_empty() {
        assert!(Instant::now() < deadline, "no track");
        host.step();
        viewer.step();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    viewer.send(source_request(host.id, Kind::Screen, 0));
    let asked = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < asked {
        host.step();
        viewer.step();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let sending = Instant::now() + Duration::from_secs(6);
    while Instant::now() < sending {
        host.step();
        viewer.step();
        host.send_frame();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let got = frames(&viewer, &host);
    assert!(got.iter().all(|n| n % 2 == 0), "a T2 frame: {got:?}");
    assert!(got.len() >= 60, "{got:?}");
    assert!(got.iter().any(|n| n % 4 == 2), "T1 frames: {got:?}");
}

/// A receiver whose budget covers the T0 and T1 frames but not the T2 frames
/// steps down to the 15 fps decode target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_receiver_budget_limits_its_layers() {
    // T0 and T1 together: 15 frames of 617 bytes a second, 74 kbit/s; with
    // the step-up allowance 96 kbit/s. All three: 148 kbit/s.
    let sfu = start("receiver_bitrate = 110000");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    host.start_publishing(Codec::Av1);
    publish_for(&mut host, Duration::from_millis(1500)).await;
    let (mut receiver, _) = join(url, &ADMIT, false, false).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut receiver] {
            peer.step();
        }
        host.send_frame();
        let got = frames(&receiver, &host);
        let settled: Vec<u64> = got
            .iter()
            .rev()
            .take_while(|n| *n % 2 == 0)
            .copied()
            .collect();
        if settled.len() >= 20 {
            // The T0 and T1 frames, each one.
            assert!(settled.windows(2).all(|w| w[0] == w[1] + 2), "{got:?}");
            break;
        }
        assert!(Instant::now() < deadline, "{got:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_overloaded_single_layer_publisher_receives_lower_encoder_limits() {
    let sfu = start("receiver_bitrate = 40000");
    let (mut host, _) = join(&sfu.url, &ADMIT, true, true).await;
    let (mut viewer, _) = join(&sfu.url, &ADMIT, false, false).await;
    // VP9 carries no dependency descriptor in this fixture.
    host.start_publishing(Codec::Vp9);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut began = false;
    loop {
        host.step();
        viewer.step();
        if began {
            host.send_frame();
        }
        if let Some(index) = host
            .messages
            .iter()
            .position(|msg| msg["type"] == "quality")
        {
            let message = host.messages.remove(index);
            let bytes = hex::decode(message["request"].as_str().expect("hex")).expect("request");
            let demand = QualityRequest::from_bytes(&bytes).expect("demand");
            assert_eq!((demand.publisher, demand.kind), (host.id, Kind::Camera));
            let (_, _, mid, _) = host.publish.as_ref().expect("publisher");
            assert_eq!(message["source"], mid.to_string());
            let tier = usize::from(demand.tier);
            let cap = demand.kind.ladder()[tier];
            assert_eq!(message["max_height"], cap.height);
            assert_eq!(message["max_fps"], cap.fps);
            assert_eq!(message["max_bitrate"], cap.ceiling);
            if began && tier < demand.kind.top() {
                assert!((1..host.sent.len()).contains(&viewer.count(host.id)));
                break;
            }
            assert_eq!(tier, demand.kind.top());
            began = true;
        }
        assert!(Instant::now() < deadline, "no lower encoder limits");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paced_video_sustains_small_packet_bursts() {
    const FRAMES: u32 = 120;
    const PACKETS: u32 = 48;
    const INTERVAL: Duration = Duration::from_millis(25);
    let sfu = start("");
    let (mut host, _) = join(&sfu.url, &ADMIT, true, true).await;
    let (mut viewer, _) = join(&sfu.url, &ADMIT, false, false).await;
    host.start_publishing(Codec::Vp9);
    let connect = Instant::now() + Duration::from_secs(5);
    loop {
        host.step();
        viewer.step();
        if host
            .publish
            .as_ref()
            .expect("publisher")
            .0
            .rtc
            .is_connected()
            && viewer
                .subscribe
                .as_ref()
                .is_some_and(|leg| leg.rtc.is_connected())
            && viewer.tracks.values().any(|&id| id == host.id)
        {
            break;
        }
        assert!(Instant::now() < connect, "media legs did not connect");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let (leg, _, mid, _) = host.publish.as_ref().expect("publisher");
    let remote = leg.rtc.media(*mid).expect("video").remote_pts();
    let pt = leg
        .rtc
        .codec_config()
        .params()
        .iter()
        .find(|params| params.spec().codec == Codec::Vp9 && remote.contains(&params.pt()))
        .expect("negotiated VP9")
        .pt();
    // This packet rate exercises the pacer while the small payload fits its
    // starting bitrate. Large RTP packets hit the same limit at about 10 Mb/s.
    let start = Instant::now();
    for frame in 0..FRAMES {
        let due = start + INTERVAL * frame;
        while Instant::now() < due {
            host.step();
            viewer.step();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let (leg, _, mid, _) = host.publish.as_mut().expect("publisher");
        let mut api = leg.rtc.direct_api();
        let stream = api.stream_tx_by_mid(*mid, None).expect("video sender");
        let now = Instant::now();
        for packet in 0..PACKETS {
            let seq = u64::from(frame * PACKETS + packet + 1);
            stream.write_rtp(
                RtpWrite::new(pt, seq.into(), (frame + 1) * 2250, now, vec![0x10; 64])
                    .marker(packet + 1 == PACKETS),
            );
        }
        host.step();
        viewer.step();
    }
    let minimum = (FRAMES * PACKETS * 4 / 5) as usize;
    let deadline = start + INTERVAL * FRAMES + Duration::from_secs(1);
    while viewer.count(host.id) < minimum && Instant::now() < deadline {
        host.step();
        viewer.step();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let received = viewer.count(host.id);
    assert!(
        received >= minimum,
        "received {received} of {} RTP packets",
        FRAMES * PACKETS
    );
}

/// Publishes about 3 Mbit/s in the base target and lets the SFU measure it.
async fn active_high_bitrate_host(url: &str) -> Peer {
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    host.frame_packets = 24;
    host.start_publishing(Codec::Av1);
    publish_for(&mut host, Duration::from_secs(3)).await;
    host
}

async fn publish_for(host: &mut Peer, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        host.step();
        host.send_frame();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Late subscribers receive an active base target above the congestion
/// estimator's 2 Mbit/s startup rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joins_an_active_high_bitrate_stream() {
    let sfu = start("");
    let url = sfu.url.as_str();
    let mut host = active_high_bitrate_host(url).await;
    let (mut viewer, welcome) = join(url, &ADMIT, false, false).await;
    assert_eq!(welcome["type"], "welcome", "{welcome}");
    let deadline = Instant::now() + Duration::from_secs(15);
    while viewer.count(host.id) < host.frame_packets * 60 {
        assert!(
            Instant::now() < deadline,
            "timed out after receiving {} RTP packets",
            viewer.count(host.id)
        );
        host.step();
        viewer.step();
        host.send_frame();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_forwarded(&viewer, &host, Codec::Av1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admits_a_base_layer_when_it_fits_the_room_budget() {
    let sfu = start("room_bitrate = 1800000");
    let mut host = active_high_bitrate_host(&sfu.url).await;
    let (_, refused) = join(&sfu.url, &ADMIT, false, false).await;
    assert_eq!(refused, json!({"type": "error", "reason": "bandwidth"}));

    host.frame_packets = 8;
    publish_for(&mut host, Duration::from_secs(3)).await;
    let (_, welcome) = join(&sfu.url, &ADMIT, false, false).await;
    assert_eq!(welcome["type"], "welcome", "{welcome}");
}

/// At its traffic budget the SFU refuses new peers, and the count is on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_traffic_budget_refuses_new_video() {
    let sfu = start("traffic_budget = 30000\nperiod = \"day\"\nstate_directory = \"{directory}\"");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    let (mut receiver, _) = join(url, &ADMIT, false, false).await;
    host.start_publishing(Codec::Av1);
    // About 20 kB a second reach the receiver; the count is saved each second.
    let count = sfu.directory.path().join("traffic");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for peer in [&mut host, &mut receiver] {
            peer.step();
        }
        host.send_frame();
        let sent = std::fs::read_to_string(&count)
            .ok()
            .and_then(|text| text.split_whitespace().nth(1)?.parse::<u64>().ok());
        if sent.is_some_and(|sent| sent >= 30_000) {
            break;
        }
        assert!(Instant::now() < deadline, "{sent:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (_, refused) = join(url, &ADMIT, false, false).await;
    assert_eq!(refused, json!({"type": "error", "reason": "traffic"}));
}

/// Replays `host`'s recording for two seconds without viewers, so a viewer joins
/// a running source.
async fn replay_alone(host: &mut Peer) {
    let running = Instant::now() + Duration::from_secs(2);
    while Instant::now() < running {
        host.step();
        host.send_replay();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Steps `host`, replaying its recording, and `viewer` until `done` accepts the
/// viewer's decodable frames, checked once a second; every whole frame the
/// viewer gets must be decodable.
async fn replay_until(
    host: &mut Peer,
    viewer: &mut Peer,
    limit: Duration,
    mut done: impl FnMut(&mut Peer, &[(u16, u8)]) -> bool,
) {
    let deadline = Instant::now() + limit;
    let mut checked = Instant::now();
    loop {
        host.step();
        viewer.step();
        host.send_replay();
        if checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            let packets = viewer.received.get(&host.id).map_or(&[][..], Vec::as_slice);
            let (decoded, broken) = decodable(packets);
            assert!(broken.is_empty(), "undecodable frames {broken:?}");
            if done(viewer, &decoded) {
                return;
            }
            let spatial: Vec<u8> = decoded.iter().map(|f| f.1).collect();
            assert!(Instant::now() < deadline, "decoded layers {spatial:?}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Chromium's L2T2_KEY screen: a viewer starts on the base layer and moves to the
/// full layer once its budget allows (after the 30 s step-up hold), at a key
/// picture: the full layer's delta frames are switch points only for receivers
/// that hold its chain. Every whole frame it gets stays decodable, and the SFU
/// asks the publisher for a key frame only where the viewer lacks a chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layered_screen_switches_up_at_a_key_picture() {
    let sfu = start("lease_secs = 3600");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    host.replay = Some(Replay::new(fixture(include_str!(
        "fixtures/chrome-l2t2-key.txt"
    ))));
    host.publish_source(Codec::Av1, true);
    replay_alone(&mut host).await;
    let (mut viewer, _) = join(url, &ADMIT, false, false).await;
    let capacity = |screen| {
        let decode = nuschl_stream_quality::DecodeCapacity { camera: 3, screen };
        json!({"type": "capacity", "decode": hex::encode(decode.to_bytes())})
    };
    let (mut asked, mut up) = (false, None);
    replay_until(
        &mut host,
        &mut viewer,
        Duration::from_secs(90),
        |viewer, decoded| {
            if !asked && !viewer.tracks.is_empty() {
                // The base layer first.
                viewer.send(capacity(0));
                asked = true;
            }
            let Some(at) = up else {
                if decoded.iter().rev().take_while(|f| f.1 == 0).count() >= 20 {
                    up = Some(decoded.len());
                }
                return false;
            };
            // The SFU takes one report a second; this repeats it until it passes.
            viewer.send(capacity(3));
            decoded[at..].iter().filter(|f| f.1 == 1).count() >= 60
        },
    )
    .await;
    // One for the viewer's start, one for the switch.
    assert_eq!(host.keyframe_requests, 2, "key frame requests");
}

/// Chromium's L3T3_KEY camera: a viewer on an upper layer that asks for the
/// lowest tier moves down at a key picture, since each spatial layer has its own
/// chain, and every whole frame it gets stays decodable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layered_camera_switches_down_at_a_key_picture() {
    let sfu = start("");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    let (mut viewer, _) = join(url, &ADMIT, false, false).await;
    host.replay = Some(Replay::new(fixture(include_str!(
        "fixtures/chrome-l3t3-key.txt"
    ))));
    host.publish_source(Codec::Av1, false);
    let publisher = host.id;
    let mut down: Option<usize> = None;
    replay_until(
        &mut host,
        &mut viewer,
        Duration::from_secs(30),
        |viewer, decoded| {
            let Some(at) = down else {
                if decoded.iter().filter(|f| f.1 > 0).count() >= 20 {
                    viewer.send(quality_request(publisher, 0));
                    down = Some(decoded.len());
                }
                return false;
            };
            decoded[at..].iter().rev().take_while(|f| f.1 == 0).count() >= 30
        },
    )
    .await;
}

/// A viewer that loses one in 30 AV1 packets on its downlink gets them again
/// from the SFU's retransmission buffer: every frame arrives whole and
/// decodable, and the publisher sends one key frame, for the viewer's start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_packets_return_through_nack() {
    let sfu = start("");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    host.replay = Some(Replay::new(fixture(include_str!(
        "fixtures/chrome-l3t3-key.txt"
    ))));
    host.publish_source(Codec::Av1, false);
    replay_alone(&mut host).await;
    let (mut viewer, _) = join(url, &ADMIT, false, false).await;
    let publisher = host.id;
    let frames = |viewer: &Peer| -> std::collections::HashSet<u16> {
        let packets = viewer.received.get(&publisher).into_iter().flatten();
        packets
            .filter_map(|p| {
                Some(u16::from_be_bytes(
                    p.descriptor.as_ref()?.0[1..3].try_into().ok()?,
                ))
            })
            .collect()
    };
    // Repairs take a round trip, so the frames are checked after the replay.
    let deadline = Instant::now() + Duration::from_secs(30);
    while frames(&viewer).len() < 300 {
        assert!(
            Instant::now() < deadline,
            "{} frames",
            frames(&viewer).len()
        );
        host.step();
        viewer.step();
        host.send_replay();
        if let Some(leg) = &mut viewer.subscribe {
            leg.loss.get_or_insert((30, 0));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let repaired = Instant::now() + Duration::from_secs(1);
    while Instant::now() < repaired {
        host.step();
        viewer.step();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let leg = viewer.subscribe.as_ref().expect("subscribed");
    assert!(
        leg.loss.is_some_and(|(_, count)| count >= 300),
        "{:?}",
        leg.loss
    );
    let (decoded, broken) = decodable(&viewer.received[&host.id]);
    assert!(broken.is_empty(), "undecodable frames {broken:?}");
    // The replay may stop inside a frame.
    let seen = frames(&viewer).len();
    assert!(
        decoded.len() + 3 >= seen,
        "{} of {seen} frames whole",
        decoded.len()
    );
    assert!(
        host.keyframe_requests <= 1,
        "{} key frame requests",
        host.keyframe_requests
    );
}

/// A camera viewer behind a 700 kbit/s downlink, about a third of the camera's
/// rate, with a buffer deep enough that the link loses nothing: the SFU lowers
/// its layers and leaves out whole frames it can do without. Every frame it
/// gets stays decodable and it keeps decoding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_throttled_viewer_gets_decodable_frames() {
    let sfu = start("");
    let url = sfu.url.as_str();
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    host.replay = Some(Replay::new(fixture(include_str!(
        "fixtures/chrome-l3t3-key.txt"
    ))));
    host.publish_source(Codec::Av1, false);
    replay_alone(&mut host).await;
    let (mut viewer, _) = join(url, &ADMIT, false, false).await;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        host.step();
        viewer.step();
        host.send_replay();
        if let Some(leg) = &mut viewer.subscribe {
            leg.link.get_or_insert_with(|| {
                Link::new(700_000, Duration::from_millis(20), Duration::from_secs(10))
            });
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let late = viewer.received[&host.id].len();
    let repaired = Instant::now() + Duration::from_secs(1);
    while Instant::now() < repaired {
        host.step();
        viewer.step();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let packets = &viewer.received[&host.id];
    let (decoded, broken) = decodable(packets);
    assert!(broken.is_empty(), "undecodable frames {broken:?}");
    // Still decoding in the last five seconds.
    let numbers: std::collections::HashSet<u16> = packets[late * 3 / 4..]
        .iter()
        .filter_map(|p| {
            Some(u16::from_be_bytes(
                p.descriptor.as_ref()?.0[1..3].try_into().ok()?,
            ))
        })
        .collect();
    let recent = decoded.iter().filter(|f| numbers.contains(&f.0)).count();
    let spatial: Vec<u8> = decoded.iter().map(|f| f.1).collect();
    assert!(recent >= 20, "{recent} recent frames, layers {spatial:?}");
}

/// A publisher replaying Chromium's L3T3_KEY camera in `mode`, and a viewer
/// that joins once the replay runs.
async fn camera_call(url: &str, mode: Value) -> (Peer, Peer) {
    let (mut host, _) = join(url, &ADMIT, true, true).await;
    host.replay = Some(Replay::new(fixture(include_str!(
        "fixtures/chrome-l3t3-key.txt"
    ))));
    host.publish_source(Codec::Av1, false);
    let mid = host.publish.as_ref().expect("publishing").2.to_string();
    host.send(json!({"type": "modes", "modes": {mid: mode}}));
    replay_alone(&mut host).await;
    let (viewer, _) = join(url, &ADMIT, false, false).await;
    (host, viewer)
}

/// Steps `host`, replaying, and `viewer` behind a `rate` downlink with 20 ms
/// of delay for `duration`.
async fn throttled(host: &mut Peer, viewer: &mut Peer, rate: u64, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        host.step();
        viewer.step();
        host.send_replay();
        if let Some(leg) = &mut viewer.subscribe {
            let link = leg.link.get_or_insert_with(|| {
                Link::new(rate, Duration::from_millis(20), Duration::from_secs(10))
            });
            link.rate = rate as f64;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn number(packet: &Received) -> Option<u16> {
    Some(u16::from_be_bytes(
        packet.descriptor.as_ref()?.0[1..3].try_into().ok()?,
    ))
}

/// The frame rate and the top spatial layer of the frames a viewer decoded
/// in the last five seconds of media.
fn recent_rate(packets: &[Received]) -> (f64, u8) {
    let (decoded, broken) = decodable(packets);
    assert!(broken.is_empty(), "undecodable frames {broken:?}");
    let timestamps: HashMap<u16, u32> = packets
        .iter()
        .filter_map(|p| Some((number(p)?, p.timestamp)))
        .collect();
    let last = decoded
        .iter()
        .map(|f| timestamps[&f.0])
        .max()
        .expect("decoded frames");
    let recent: Vec<_> = decoded
        .iter()
        .filter(|f| last.wrapping_sub(timestamps[&f.0]) < 5 * 90_000)
        .collect();
    let units: std::collections::HashSet<u32> = recent.iter().map(|f| timestamps[&f.0]).collect();
    let spatial = recent.iter().map(|f| f.1).max().expect("recent frames");
    (units.len() as f64 / 5.0, spatial)
}

/// Chromium's L3T3_KEY camera to a viewer behind a 1.5 Mbit/s downlink: the
/// top spatial layer fits at up to half the frame rate (about 1 Mbit/s), and
/// at the full rate of about 23 frames a second (1.4 Mbit/s and packet
/// overhead) it does not. A Sharp source keeps the top spatial layer and
/// loses frame rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sharp_stream_keeps_its_resolution() {
    let sfu = start("");
    let (mut host, mut viewer) = camera_call(&sfu.url, json!({"prefer": "sharp"})).await;
    throttled(&mut host, &mut viewer, 1_500_000, Duration::from_secs(20)).await;
    let (rate, spatial) = recent_rate(&viewer.received[&host.id]);
    assert_eq!(spatial, 2, "{rate} frames a second");
    assert!(rate <= 16.0, "{rate} frames a second");
}

/// The camera of [`a_sharp_stream_keeps_its_resolution`] as a Smooth source
/// keeps its frame rate on a lower spatial layer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_smooth_stream_keeps_its_frame_rate() {
    let sfu = start("");
    let (mut host, mut viewer) = camera_call(&sfu.url, json!({"prefer": "smooth"})).await;
    throttled(&mut host, &mut viewer, 1_500_000, Duration::from_secs(20)).await;
    let (rate, spatial) = recent_rate(&viewer.received[&host.id]);
    assert!(spatial < 2, "spatial layer {spatial}");
    assert!(rate >= 18.0, "{rate} frames a second");
}

/// A Low latency camera's viewer whose downlink falls from 3 to 0.7 Mbit/s
/// while a key picture arrives. Once its layer has followed, two seconds
/// later, its frames leave the SFU within the 50 ms budget, and the frames it
/// needs within a few times that; the 300 ms queue of other sources holds them
/// for hundreds of milliseconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_low_latency_stream_stays_within_its_budget() {
    let sfu = start("");
    let mode = json!({"prefer": "smooth", "low_latency": true});
    let (mut host, mut viewer) = camera_call(&sfu.url, mode).await;
    throttled(&mut host, &mut viewer, 3_000_000, Duration::from_secs(5)).await;
    let drop = Instant::now();
    host.replay.as_mut().expect("replay").restart = Some(drop);
    throttled(&mut host, &mut viewer, 700_000, Duration::from_secs(7)).await;
    // Each frame's wait at the SFU: until its first packet reaches the
    // viewer's socket, in front of the downlink.
    let sent = &host.replay.as_ref().expect("replay").sent;
    let mut first: HashMap<u16, Instant> = HashMap::new();
    for packet in &viewer.received[&host.id] {
        if let Some(number) = number(packet) {
            let at = first.entry(number).or_insert(packet.at);
            *at = (*at).min(packet.at);
        }
    }
    let settled = drop + Duration::from_secs(2);
    let mut waits: Vec<Duration> = first
        .iter()
        .filter(|(number, _)| sent.get(number).is_some_and(|at| *at >= settled))
        .map(|(number, at)| at.saturating_duration_since(sent[number]))
        .collect();
    waits.sort();
    assert!(waits.len() >= 50, "{} frames", waits.len());
    let p95 = waits[waits.len() * 95 / 100];
    let max = waits[waits.len() - 1];
    assert!(p95 <= Duration::from_millis(50), "95th percentile {p95:?}");
    assert!(max <= Duration::from_millis(200), "longest {max:?}");
    recent_rate(&viewer.received[&host.id]);
}
