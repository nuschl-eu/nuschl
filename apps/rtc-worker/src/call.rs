// SPDX-License-Identifier: EUPL-1.2
//! One call's transport state. The supervisor owns UDP and authenticates SDP.

use nuschl_ipc::{
    crypto::{
        call::MAX_MEDIA_PEERS,
        epoch::{self, Direction, Update},
    },
    rtc::{
        self as wire, Codec, Command, Description, Event, LegRole, Packet, Purpose, Reply, Track,
    },
};
use nuschl_rtc::{self as rtc, Output, Peer, SignalingId, TrackBinding, VideoCodec};
use nuschl_sframe::{FrameDecryptor, FrameEncryptor, MAX_ENCODINGS, encryptors};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::SocketAddr,
    sync::mpsc::{self, SyncSender},
    thread,
    time::Instant,
};
#[cfg(target_os = "linux")]
use std::{
    net::Shutdown,
    os::{fd::AsFd, unix::net::UnixStream},
};

const MAX_LEGS: usize = 4;
const MAX_WIRE_VIDEO: usize = wire::MAX_VIDEO + 64;

enum Input {
    Commands(Packet<Command>),
    Key(Update),
    End,
}

#[cfg(target_os = "linux")]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let keys = UnixStream::from(io::stdin().as_fd().try_clone_to_owned()?);
    let control = UnixStream::from(io::stdout().as_fd().try_clone_to_owned()?);
    let commands = control.try_clone()?;
    let revoke = control.try_clone()?;
    run_channel(commands, control, keys, move || {
        let _ = revoke.shutdown(Shutdown::Both);
    })
}

#[cfg(not(target_os = "linux"))]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    let keys = {
        use std::os::fd::AsFd as _;
        std::fs::File::from(io::stderr().as_fd().try_clone_to_owned()?)
    };
    #[cfg(windows)]
    let keys = {
        use std::os::windows::io::AsHandle as _;
        std::fs::File::from(io::stderr().as_handle().try_clone_to_owned()?)
    };
    run_channel(io::stdin(), io::stdout(), keys, || {})
}

fn run_channel(
    mut commands: impl Read + Send + 'static,
    mut output: impl Write,
    mut keys: impl Read + Send + 'static,
    revoke: impl FnOnce() + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(Packet {
        id,
        kind: Command::Start { generation },
        ..
    }) = wire::read_request(&mut commands)?
    else {
        return Err("expected call start".into());
    };
    let mut last_id = id;
    let (send, inbox) = mpsc::sync_channel(1);
    let command_events = send.clone();
    thread::Builder::new().spawn(move || {
        input(
            command_events,
            || {
                wire::read_request(&mut commands)
                    .ok()
                    .flatten()
                    .map(Input::Commands)
            },
            || {},
        )
    })?;
    thread::Builder::new().spawn(move || {
        input(
            send,
            || epoch::read(&mut keys).ok().flatten().map(Input::Key),
            revoke,
        )
    })?;
    reply(&mut output, id, Reply::Done, Vec::new())?;
    let mut call = Call::new(generation);
    while let Ok(event) = inbox.recv() {
        match event {
            Input::Commands(request) if request.id > last_id => {
                last_id = request.id;
                let (kind, data) = call
                    .handle(request.kind, request.data)
                    .unwrap_or((Reply::Rejected, Vec::new()));
                reply(&mut output, last_id, kind, data)?;
            }
            Input::Key(update) => call.install(update).map_err(|()| "invalid call epoch")?,
            Input::End => return Ok(()),
            _ => return Err("invalid request order".into()),
        }
    }
    Ok(())
}

fn input(events: SyncSender<Input>, mut next: impl FnMut() -> Option<Input>, end: impl FnOnce()) {
    while let Some(event) = next() {
        if events.send(event).is_err() {
            return;
        }
    }
    end();
    let _ = events.send(Input::End);
}

fn reply(
    output: &mut impl Write,
    id: u64,
    kind: Reply,
    data: Vec<u8>,
) -> Result<(), nuschl_ipc::Error> {
    wire::write_reply(output, &Packet { id, kind, data })
}

struct Leg {
    peer: Peer,
    role: LegRole,
    wake: Instant,
    local: SocketAddr,
    from: Vec<Incoming>,
}

struct Incoming {
    track: Track,
    discontinuous: bool,
}

struct Sending {
    publisher: u64,
    epoch: u32,
    keys: [FrameEncryptor; MAX_ENCODINGS as usize],
}

struct Call {
    generation: u64,
    origin: Instant,
    legs: BTreeMap<u32, Leg>,
    last_direct_leg: u32,
    opened_publish: bool,
    opened_subscribe: bool,
    sending: Option<Sending>,
    receiving: BTreeMap<u64, FrameDecryptor>,
    active: Vec<u32>,
    keyframe: [bool; MAX_ENCODINGS as usize],
}

impl Call {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            origin: Instant::now(),
            legs: BTreeMap::new(),
            last_direct_leg: 0,
            opened_publish: false,
            opened_subscribe: false,
            sending: None,
            receiving: BTreeMap::new(),
            active: Vec::new(),
            keyframe: [false; MAX_ENCODINGS as usize],
        }
    }

    fn install(&mut self, update: Update) -> Result<(), ()> {
        if update.generation != self.generation {
            return Err(());
        }
        let now = self.millis();
        match update.direction {
            Direction::Send => {
                if self
                    .sending
                    .as_ref()
                    .is_some_and(|s| s.publisher != update.peer || s.epoch >= update.epoch)
                {
                    return Err(());
                }
                self.sending = Some(Sending {
                    publisher: update.peer,
                    epoch: update.epoch,
                    keys: encryptors(update.epoch, &update.key),
                });
            }
            Direction::Receive => {
                if self.receiving.len() >= MAX_MEDIA_PEERS
                    && !self.receiving.contains_key(&update.peer)
                {
                    return Err(());
                }
                self.receiving
                    .entry(update.peer)
                    .or_default()
                    .install(update.epoch, &update.key, now)
                    .map_err(|_| ())?;
            }
            Direction::ForgetReceive => {
                self.receiving.remove(&update.peer);
            }
        }
        Ok(())
    }

    fn handle(&mut self, command: Command, data: Vec<u8>) -> Result<(Reply, Vec<u8>), ()> {
        let now = Instant::now();
        let kind = match command {
            Command::Start { .. } => return Err(()),
            Command::OpenLeg {
                leg,
                local,
                codec,
                role,
                relayed,
            } => {
                let fresh = match role {
                    LegRole::Direct => {
                        leg >= wire::FIRST_DIRECT_LEG && leg > self.last_direct_leg && !relayed
                    }
                    LegRole::SfuPublish => leg == wire::SFU_PUBLISH_LEG && !self.opened_publish,
                    LegRole::SfuSubscribe => {
                        leg == wire::SFU_SUBSCRIBE_LEG && !self.opened_subscribe
                    }
                };
                if !fresh || self.legs.len() == MAX_LEGS {
                    return Err(());
                }
                let codec = match codec {
                    Codec::Av1 => VideoCodec::Av1,
                    Codec::Vp9 => VideoCodec::Vp9,
                };
                let peer =
                    Peer::for_call(local, now, codec, MAX_WIRE_VIDEO, relayed).map_err(|_| ())?;
                self.legs.insert(
                    leg,
                    Leg {
                        peer,
                        role,
                        wake: now,
                        local,
                        from: Vec::new(),
                    },
                );
                match role {
                    LegRole::Direct => self.last_direct_leg = leg,
                    LegRole::SfuPublish => self.opened_publish = true,
                    LegRole::SfuSubscribe => self.opened_subscribe = true,
                }
                Reply::Done
            }
            Command::CloseLeg { leg } => {
                self.legs.remove(&leg).ok_or(())?;
                self.active.retain(|active| *active != leg);
                Reply::Done
            }
            Command::AddReflexive { leg, address } => {
                let live = self.legs.get_mut(&leg).ok_or(())?;
                if live.role != LegRole::Direct {
                    return Err(());
                }
                live.peer.add_call_reflexive(address).map_err(|_| ())?;
                Reply::Done
            }
            Command::Offer { leg, id, purposes } => {
                let live = self.legs.get_mut(&leg).ok_or(())?;
                let purposes: Vec<_> = purposes.into_iter().map(to_purpose).collect();
                let id = SignalingId::new(id).map_err(|_| ())?;
                let description = match live.role {
                    LegRole::Direct => live.peer.offer_tracks(&purposes, id),
                    LegRole::SfuPublish => live.peer.offer_publish(&purposes, id),
                    LegRole::SfuSubscribe => return Err(()),
                }
                .map_err(|_| ())?;
                if live.role == LegRole::SfuPublish {
                    self.keyframe = [false; MAX_ENCODINGS as usize];
                }
                Reply::Description {
                    leg,
                    description: from_description(description),
                }
            }
            Command::UpdatePublish {
                leg,
                id,
                purposes,
                restart,
            } => {
                let live = self.legs.get_mut(&leg).ok_or(())?;
                if live.role != LegRole::SfuPublish {
                    return Err(());
                }
                let purposes: Vec<_> = purposes.into_iter().map(to_purpose).collect();
                let restart: Vec<_> = restart.into_iter().map(to_purpose).collect();
                let id = SignalingId::new(id).map_err(|_| ())?;
                let video = [Purpose::Camera, Purpose::ScreenVideo];
                let before = video.map(|purpose| live.peer.track_for(to_purpose(purpose)));
                let description = live
                    .peer
                    .update_publish(&purposes, &restart, id)
                    .map_err(|_| ())?;
                for (purpose, previous) in video.into_iter().zip(before) {
                    if live.peer.track_for(to_purpose(purpose)) != previous {
                        self.keyframe[usize::from(encoding(purpose))] = false;
                    }
                }
                Reply::Description {
                    leg,
                    description: from_description(description),
                }
            }
            Command::AcceptOffer { leg, description } => {
                let live = self.legs.get_mut(&leg).ok_or(())?;
                let description = live.offer(description)?;
                Reply::Description { leg, description }
            }
            Command::AcceptAnswer { leg, description } => {
                self.legs.get_mut(&leg).ok_or(())?.answer(description)?;
                Reply::Done
            }
            Command::Datagram { leg, source } => {
                self.legs
                    .get_mut(&leg)
                    .ok_or(())?
                    .peer
                    .receive(source, now, &data)
                    .map_err(|_| ())?;
                Reply::Done
            }
            Command::Poll { leg } => return self.poll(leg, now),
            Command::SetSendLegs { legs } => {
                let usable = |index: usize, leg: &u32| {
                    !legs[..index].contains(leg)
                        && self.legs.get(leg).is_some_and(|live| {
                            live.peer.is_connected()
                                && match live.role {
                                    LegRole::Direct => true,
                                    LegRole::SfuPublish => legs.len() == 1,
                                    LegRole::SfuSubscribe => false,
                                }
                        })
                };
                if self.sending.is_none() || !legs.iter().enumerate().all(|(i, leg)| usable(i, leg))
                {
                    return Err(());
                }
                if self.active != legs {
                    for previous in std::mem::replace(&mut self.active, legs) {
                        if self.active.contains(&previous) {
                            continue;
                        }
                        match self.legs.get_mut(&previous) {
                            Some(old) if old.role == LegRole::SfuPublish => {
                                old.peer.clear_call_send()
                            }
                            _ => {
                                self.legs.remove(&previous);
                            }
                        }
                    }
                    self.keyframe = [false; MAX_ENCODINGS as usize];
                }
                Reply::Done
            }
            Command::Write {
                purpose,
                timestamp,
                key_frame,
                descriptor,
            } => {
                self.write(purpose, timestamp, key_frame, now, &data, &descriptor)?;
                Reply::Done
            }
            Command::RequestKeyframe {
                leg,
                publisher,
                purpose,
            } => {
                let live = self.legs.get_mut(&leg).ok_or(())?;
                let incoming = live
                    .from
                    .iter()
                    .find(|incoming| {
                        incoming.track.purpose == purpose
                            && incoming.track.publisher == Some(publisher)
                    })
                    .ok_or(())?;
                let track = live.peer.track_by_mid(&incoming.track.mid).ok_or(())?;
                live.peer.request_keyframe(track).map_err(|_| ())?;
                Reply::Done
            }
        };
        Ok((kind, Vec::new()))
    }

    fn write(
        &mut self,
        purpose: Purpose,
        timestamp: u64,
        key_frame: bool,
        now: Instant,
        data: &[u8],
        descriptor: &[u8],
    ) -> Result<(), ()> {
        let index = usize::from(encoding(purpose));
        if self.active.is_empty()
            || matches!(purpose, Purpose::Camera | Purpose::ScreenVideo)
                && !self.keyframe[index]
                && !key_frame
        {
            return Err(());
        }
        let sender = self.sending.as_mut().ok_or(())?;
        let encrypted: std::sync::Arc<[u8]> = sender.keys[index]
            .encrypt(key_frame, data)
            .map_err(|_| ())?
            .into();
        // Every direct receiver gets the same ciphertext. A leg that refuses the
        // frame needs a key frame, which the other legs receive too.
        let mut written = true;
        for leg in &self.active {
            let live = self.legs.get_mut(leg).ok_or(())?;
            let track = live.peer.track_for(to_purpose(purpose)).ok_or(())?;
            written &= live
                .peer
                .write(track, timestamp, now, encrypted.clone(), descriptor)
                .is_ok();
        }
        if !written {
            self.keyframe[index] = false;
            return Err(());
        }
        if key_frame {
            self.keyframe[index] = true;
        }
        Ok(())
    }

    fn poll(&mut self, id: u32, now: Instant) -> Result<(Reply, Vec<u8>), ()> {
        let millis = self.millis();
        let leg = self.legs.get_mut(&id).ok_or(())?;
        if now >= leg.wake {
            leg.peer.timeout(now).map_err(|_| ())?;
        }
        for _ in 0..32 {
            match leg.peer.poll().map_err(|_| ())? {
                Output::WakeAt(at) => {
                    leg.wake = at;
                    // str0m needs a fresh time sample after each paced packet.
                    // Service due work before returning a millisecond deadline.
                    let current = Instant::now();
                    if at <= current {
                        leg.peer.timeout(current).map_err(|_| ())?;
                        continue;
                    }
                    let after_millis = at
                        .duration_since(current)
                        .as_nanos()
                        .div_ceil(1_000_000)
                        .min(60_000) as u64;
                    return Ok((
                        Reply::Wake {
                            leg: id,
                            after_millis,
                        },
                        Vec::new(),
                    ));
                }
                Output::Datagram {
                    source,
                    destination,
                    data,
                } => {
                    if source != leg.local {
                        return Err(());
                    }
                    return Ok((
                        Reply::Datagram {
                            leg: id,
                            destination,
                        },
                        data,
                    ));
                }
                Output::Event(event) => {
                    let event = match event {
                        rtc::Event::Connected => Event::Connected,
                        rtc::Event::Disconnected => Event::Disconnected,
                        rtc::Event::BandwidthEstimate(bitrate) => {
                            Event::BandwidthEstimate { bitrate }
                        }
                        rtc::Event::RequestKeyframe(track) => {
                            let purpose = from_purpose(leg.peer.purpose(track).ok_or(())?);
                            Event::Keyframe { purpose }
                        }
                        rtc::Event::Frame(frame) => {
                            let purpose = from_purpose(frame.purpose);
                            let Some(incoming) = leg.from.iter_mut().find(|incoming| {
                                incoming.track.mid == frame.track.mid()
                                    && incoming.track.purpose == purpose
                            }) else {
                                continue;
                            };
                            let Some(publisher) = incoming.track.publisher else {
                                continue;
                            };
                            let Some(decryptor) = self.receiving.get_mut(&publisher) else {
                                continue;
                            };
                            let lost = &mut incoming.discontinuous;
                            let Ok((key_frame, data)) =
                                decryptor.decrypt_encoding(encoding(purpose), &frame.data, millis)
                            else {
                                *lost = true;
                                continue;
                            };
                            if data.is_empty() || data.len() > purpose.frame_limit() {
                                *lost = true;
                                continue;
                            }
                            let event = Event::Frame {
                                publisher,
                                purpose,
                                timestamp: frame.timestamp,
                                contiguous: !std::mem::take(lost) && frame.contiguous,
                                key_frame,
                            };
                            return Ok((Reply::Event { leg: id, event }, data));
                        }
                    };
                    return Ok((Reply::Event { leg: id, event }, Vec::new()));
                }
            }
        }
        Ok((
            Reply::Wake {
                leg: id,
                after_millis: 0,
            },
            Vec::new(),
        ))
    }

    fn millis(&self) -> u64 {
        self.origin
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

impl Leg {
    fn offer(&mut self, description: Description) -> Result<Description, ()> {
        let rtc = to_description(&description)?;
        if self.role == LegRole::SfuPublish
            || description.tracks.iter().any(|t| t.publisher.is_none())
        {
            return Err(());
        }
        let answer = match self.role {
            LegRole::Direct => self.peer.accept_offer_tracks(&rtc, rtc.id).map(|d| d.sdp),
            LegRole::SfuSubscribe => self.peer.accept_subscription(&rtc.sdp, &rtc.bindings),
            LegRole::SfuPublish => return Err(()),
        }
        .map_err(|_| ())?;
        self.set_tracks(&description.tracks);
        Ok(Description {
            id: description.id,
            sdp: answer,
            tracks: description
                .tracks
                .into_iter()
                .map(|mut track| {
                    track.publisher = None;
                    track
                })
                .collect(),
        })
    }

    fn answer(&mut self, description: Description) -> Result<(), ()> {
        let expected = match self.role {
            LegRole::Direct => true,
            LegRole::SfuPublish => false,
            LegRole::SfuSubscribe => return Err(()),
        };
        if !description
            .tracks
            .iter()
            .all(|t| t.publisher.is_some() == expected)
        {
            return Err(());
        }
        let rtc = to_description(&description)?;
        self.peer
            .accept_answer_tracks(&rtc, rtc.id)
            .map_err(|_| ())?;
        self.set_tracks(&description.tracks);
        Ok(())
    }

    fn set_tracks(&mut self, tracks: &[Track]) {
        let mut previous = std::mem::take(&mut self.from);
        for track in tracks {
            if self.from.iter().any(|incoming| {
                incoming.track.publisher == track.publisher
                    && incoming.track.purpose == track.purpose
            }) {
                continue;
            }
            let incoming = previous
                .iter()
                .position(|entry| entry.track == *track)
                .map(|index| previous.swap_remove(index))
                .unwrap_or_else(|| Incoming {
                    track: track.clone(),
                    discontinuous: false,
                });
            self.from.push(incoming);
        }
    }
}

fn to_description(description: &Description) -> Result<rtc::Description, ()> {
    Ok(rtc::Description {
        id: SignalingId::new(description.id).map_err(|_| ())?,
        sdp: description.sdp.clone(),
        bindings: description
            .tracks
            .iter()
            .map(|t| TrackBinding {
                mid: t.mid.clone(),
                purpose: to_purpose(t.purpose),
            })
            .collect(),
    })
}

fn from_description(description: rtc::Description) -> Description {
    Description {
        id: description.id.bytes(),
        sdp: description.sdp,
        tracks: description
            .bindings
            .into_iter()
            .map(|t| Track {
                mid: t.mid,
                purpose: from_purpose(t.purpose),
                publisher: None,
            })
            .collect(),
    }
}

fn encoding(purpose: Purpose) -> u8 {
    match purpose {
        Purpose::Camera => 0,
        Purpose::ScreenVideo => 1,
        Purpose::Microphone => 2,
        Purpose::SystemAudio => 3,
    }
}

fn to_purpose(purpose: Purpose) -> rtc::Purpose {
    match purpose {
        Purpose::Camera => rtc::Purpose::Camera,
        Purpose::ScreenVideo => rtc::Purpose::ScreenVideo,
        Purpose::Microphone => rtc::Purpose::Microphone,
        Purpose::SystemAudio => rtc::Purpose::SystemAudio,
    }
}

fn from_purpose(purpose: rtc::Purpose) -> Purpose {
    match purpose {
        rtc::Purpose::Camera => Purpose::Camera,
        rtc::Purpose::ScreenVideo => Purpose::ScreenVideo,
        rtc::Purpose::Microphone => Purpose::Microphone,
        rtc::Purpose::SystemAudio => Purpose::SystemAudio,
    }
}
