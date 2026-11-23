// SPDX-License-Identifier: EUPL-1.2
//! Encoded media for a native call. The WSS connection owns
//! signaling, and the isolated RTC worker owns ICE, DTLS, SRTP and SFrame.

use super::turn::NetworkLeg;
use super::{
    Action, ClientMessage, Codec, Connection, Error, HANDSHAKE, Outcome, Quality, ServerMessage,
    VideoKind, write,
};
use nuschl_core::network::Destination;
use nuschl_ipc::rtc::{
    self, Command, Description, Event, LegRole, Reply, SFU_PUBLISH_LEG as PUBLISH,
    SFU_SUBSCRIBE_LEG as SUBSCRIBE, Track, Worker,
};
use nuschl_protocol::sfu::VideoLimit;
use nuschl_stream_quality::{QualityRequest, Throttle};
use std::{
    collections::{HashMap, HashSet},
    future::pending,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, sleep, sleep_until, timeout},
};

mod direct;
use direct::{Candidates, Direct, Role, Signal};

pub use rtc::Purpose;

const UDP_BURST: usize = 64;
const MAX_PENDING_FRAME_AGE: Duration = Duration::from_millis(150);
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);
// A subscription accepts 48 SDP sections, including its control channel and first source.
const MAX_SOURCE_CHANGES: u8 = 46;
const VIDEO: [Purpose; 2] = [Purpose::Camera, Purpose::ScreenVideo];
/// Shared desktop audio packets queued for the RTC worker, and received ones.
const AUDIO_QUEUE: usize = 8;
/// Direct admission counts the system-audio reservation of `nuschl_rtc`.
const AUDIO_BITRATE: u64 = 256_000;

pub struct MediaOptions {
    /// Path of the installed `nuschl-rtc-worker`.
    pub executable: PathBuf,
    /// UDP port on the same authenticated SFU address as WSS (default 3479).
    pub sfu_port: u16,
    /// The initial source to publish, or `None` to start with capture off.
    pub publish: Option<Purpose>,
}

pub enum MediaEvent {
    SourceReady {
        purpose: Purpose,
    },
    BandwidthEstimate {
        bitrate: u64,
    },
    SourceLimits {
        purpose: Purpose,
        height: u16,
        fps: u8,
        bitrate: u64,
        limit: Option<VideoLimit>,
    },
    VideoLimit {
        publisher: u64,
        purpose: Purpose,
        limit: VideoLimit,
    },
    SourceKeyframeRequired {
        purpose: Purpose,
    },
    /// A remote track is available after signaling has accepted its SDP.
    SourceAvailable {
        publisher: u64,
        purpose: Purpose,
    },
    /// The path of this endpoint's video changed. The SFU stays available
    /// for fallback.
    Route(Route),
    /// A negotiated video source was removed while the publisher stayed in the room.
    SourceEnded {
        publisher: u64,
        purpose: Purpose,
    },
    Frame {
        publisher: u64,
        purpose: Purpose,
        timestamp: u64,
        contiguous: bool,
        key_frame: bool,
        data: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Direct,
    Sfu(Reason),
}

/// Why video stays on the SFU (docs/video-plan.md §5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// This endpoint is in relay-only mode.
    RelayOnly,
    /// This endpoint sends and receives video.
    TwoWay,
    /// More than two people receive this publisher's video.
    Viewers,
    /// This viewer receives video from several publishers.
    Senders,
    /// A viewer has not asked for a direct leg: it is a browser, relay-only or
    /// still starting, or nobody watches.
    Viewer,
    /// The publisher has not offered a direct leg.
    Sender,
    /// No fresh probe of the upload exists yet.
    Measuring,
    /// 80 % of the probed upload is below the stream rate for every viewer.
    Upload,
    Connecting,
    /// A direct leg failed to connect or lost its connection.
    Failed,
}

enum QueuedEvent {
    Call(MediaEvent),
    SourceReady { index: usize, revision: u8 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct EncoderCaps {
    revision: u8,
    height: u16,
    fps: u8,
    bitrate: u64,
    limit: Option<VideoLimit>,
}

type SourceCaps = [Option<EncoderCaps>; 2];

#[derive(Clone, Copy, Default)]
struct Sources {
    tracks: [Option<u8>; 2],
    /// The shared desktop audio track's revision.
    audio: Option<u8>,
    revision: u8,
}

impl Sources {
    fn active(self) -> Vec<Purpose> {
        self.tracks
            .iter()
            .zip(VIDEO)
            .filter_map(|(revision, purpose)| revision.map(|_| purpose))
            .chain(self.audio.map(|_| Purpose::SystemAudio))
            .collect()
    }

    fn update(&mut self, tracks: [bool; 2], audio: bool) -> Result<(), Error> {
        if self.tracks.map(|slot| slot.is_some()) == tracks && self.audio.is_some() == audio {
            return Ok(());
        }
        if self.revision == MAX_SOURCE_CHANGES {
            return Err(Error::Protocol("video source limit reached for this call"));
        }
        self.revision += 1;
        for (slot, active) in self.tracks.iter_mut().zip(tracks) {
            *slot = active.then(|| slot.unwrap_or(self.revision));
        }
        self.audio = audio.then(|| self.audio.unwrap_or(self.revision));
        Ok(())
    }
}

enum Input {
    RequestKeyframe(Option<u64>, Purpose),
    #[cfg(test)]
    LoseDirect,
    #[cfg(test)]
    ThrottleDirect(u64),
}

struct Audio {
    revision: u8,
    queued: Instant,
    timestamp: u64,
    data: Vec<u8>,
}

struct Video {
    sequence: u64,
    revision: u8,
    queued: Instant,
    timestamp: u64,
    key_frame: bool,
    data: Vec<u8>,
}

pub struct Media {
    input: mpsc::Sender<Input>,
    frames: [mpsc::Sender<Video>; 2],
    audio: mpsc::Sender<Audio>,
    audio_frames: mpsc::Receiver<MediaEvent>,
    sequence: [AtomicU64; 2],
    quality: watch::Sender<QualityRequests>,
    sources: watch::Sender<Sources>,
    events: mpsc::Receiver<Result<QueuedEvent, Error>>,
    keyframes: [mpsc::Receiver<()>; 2],
    limits: watch::Receiver<SourceCaps>,
    reported_limits: SourceCaps,
    bandwidth: watch::Receiver<Option<u64>>,
    task: JoinHandle<()>,
}

impl Media {
    #[cfg(test)]
    pub(super) fn send_keyframe(&self, timestamp: u64, data: Vec<u8>) -> Result<bool, Error> {
        self.send_frame(timestamp, true, data)
    }

    #[cfg(test)]
    pub(super) fn send_frame(
        &self,
        timestamp: u64,
        key_frame: bool,
        data: Vec<u8>,
    ) -> Result<bool, Error> {
        let sources = self.sources.borrow().active();
        let [purpose] = sources.as_slice() else {
            return Err(Error::Protocol("select an active video source"));
        };
        self.send_source_frame(*purpose, timestamp, key_frame, data)
    }

    /// Submit a frame on a camera or screen track after its `SourceReady` event.
    /// Each source has a separate one-frame queue and a 90 kHz timestamp clock.
    /// Shared desktop audio takes Opus packets on a 48 kHz clock, which wait
    /// in a short queue of their own; the publisher drops them until it sends.
    pub fn send_source_frame(
        &self,
        purpose: Purpose,
        timestamp: u64,
        key_frame: bool,
        data: Vec<u8>,
    ) -> Result<bool, Error> {
        if purpose == Purpose::SystemAudio {
            let revision = self
                .sources
                .borrow()
                .audio
                .ok_or(Error::Protocol("shared audio is off"))?;
            if data.is_empty() || data.len() > purpose.frame_limit() {
                return Err(Error::Protocol("invalid encoded frame size"));
            }
            let audio = Audio {
                revision,
                queued: Instant::now(),
                timestamp,
                data,
            };
            return match self.audio.try_send(audio) {
                Ok(()) => Ok(true),
                Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
                Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
            };
        }
        let index = video_index(purpose)?;
        let revision =
            self.sources.borrow().tracks[index].ok_or(Error::Protocol("video source is off"))?;
        if data.is_empty() || data.len() > purpose.frame_limit() {
            return Err(Error::Protocol("invalid encoded frame size"));
        }
        let sequence = self.sequence[index]
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let frame = Video {
            sequence,
            revision,
            queued: Instant::now(),
            timestamp,
            key_frame,
            data,
        };
        match self.frames[index].try_send(frame) {
            Ok(()) => Ok(true),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
        }
    }

    /// A live call can publish while receiving remote tracks.
    pub fn can_publish(&self) -> bool {
        !self.sources.is_closed()
    }

    /// Change one granted source while keeping the other sources and signaling live.
    pub fn set_source(&mut self, purpose: Purpose, active: bool) -> Result<(), Error> {
        let current = *self.sources.borrow();
        let mut tracks = current.tracks.map(|slot| slot.is_some());
        let mut audio = current.audio.is_some();
        if purpose == Purpose::SystemAudio {
            audio = active;
        } else {
            tracks[video_index(purpose)?] = active;
        }
        let mut sources = current;
        sources.update(tracks, audio)?;
        self.sources
            .send(sources)
            .map_err(|_| Error::Protocol("media ended"))
    }

    #[cfg(test)]
    pub(super) fn switch_source(&mut self, purpose: Purpose) -> Result<(), Error> {
        let index = video_index(purpose)?;
        self.set_sources(std::array::from_fn(|i| i == index))
    }

    #[cfg(test)]
    fn set_sources(&self, tracks: [bool; 2]) -> Result<(), Error> {
        let mut sources = *self.sources.borrow();
        sources.update(tracks, sources.audio.is_some())?;
        self.sources
            .send(sources)
            .map_err(|_| Error::Protocol("media ended"))
    }

    /// Set the ceiling for a subscribed video source. Auto fits the tile's
    /// device-pixel height. The latest choice for each source takes effect when
    /// the SFU accepts its next request.
    pub fn set_quality(
        &self,
        publisher: u64,
        purpose: Purpose,
        quality: Quality,
        tile_height: u32,
    ) -> Result<(), Error> {
        let kind = video_kind(purpose).ok_or(Error::Protocol("invalid video source"))?;
        if publisher == 0 {
            return Err(Error::Protocol("invalid video subscription"));
        }
        if self.quality.is_closed() {
            return Err(Error::Protocol("media ended"));
        }
        if quality == Quality::Auto && tile_height == 0 {
            return Ok(());
        }
        let request = QualityRequest {
            publisher,
            kind,
            tier: kind.request(quality, tile_height) as u8,
        };
        self.quality.send_if_modified(|latest| {
            if let Some(slot) = latest
                .iter_mut()
                .find(|slot| slot.publisher == publisher && slot.kind == kind)
            {
                if *slot == request {
                    return false;
                }
                *slot = request;
            } else {
                if latest.len() == rtc::MAX_TRACKS {
                    latest.remove(0);
                }
                latest.push(request);
            }
            true
        });
        Ok(())
    }

    /// Recover decoding for the active subscription with a fresh keyframe.
    /// Subsequent requests share the transport's keyframe cooldown.
    pub fn request_keyframe(&self, purpose: Purpose) -> Result<(), Error> {
        self.request_video_keyframe(None, purpose)
    }

    pub fn request_source_keyframe(&self, publisher: u64, purpose: Purpose) -> Result<(), Error> {
        self.request_video_keyframe(Some(publisher), purpose)
    }

    fn request_video_keyframe(
        &self,
        publisher: Option<u64>,
        purpose: Purpose,
    ) -> Result<(), Error> {
        if publisher == Some(0) || video_kind(purpose).is_none() {
            return Err(Error::Protocol("invalid video subscription"));
        }
        match self
            .input
            .try_send(Input::RequestKeyframe(publisher, purpose))
        {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Protocol("media ended")),
        }
    }

    #[cfg(test)]
    pub(super) async fn lose_direct(&self) {
        assert!(self.input.send(Input::LoseDirect).await.is_ok());
    }

    #[cfg(test)]
    pub(super) async fn throttle_direct(&self, bytes_per_second: u64) {
        assert!(
            self.input
                .send(Input::ThrottleDirect(bytes_per_second))
                .await
                .is_ok()
        );
    }

    pub async fn next(&mut self) -> Option<Result<MediaEvent, Error>> {
        loop {
            let limits = *self.limits.borrow_and_update();
            for (index, caps) in limits.into_iter().enumerate() {
                if self.reported_limits[index] != caps {
                    self.reported_limits[index] = caps;
                    if let Some(EncoderCaps {
                        revision,
                        height,
                        fps,
                        bitrate,
                        limit,
                    }) = caps
                        && Some(revision) == self.sources.borrow().tracks[index]
                    {
                        return Some(Ok(MediaEvent::SourceLimits {
                            purpose: VIDEO[index],
                            height,
                            fps,
                            bitrate,
                            limit,
                        }));
                    }
                }
            }
            let event = tokio::select! {
                biased;
                event = self.events.recv() => event,
                Some(frame) = self.audio_frames.recv() => return Some(Ok(frame)),
                (index, requested) = next_source(&mut self.keyframes) => {
                    if requested.is_some() {
                        return Some(Ok(MediaEvent::SourceKeyframeRequired { purpose: VIDEO[index] }));
                    }
                    self.events.recv().await
                },
                changed = self.limits.changed() => {
                    if changed.is_err() {
                        self.events.recv().await
                    } else {
                        continue;
                    }
                }
                changed = self.bandwidth.changed() => {
                    if changed.is_err() {
                        self.events.recv().await
                    } else {
                        if let Some(bitrate) = *self.bandwidth.borrow_and_update() {
                            return Some(Ok(MediaEvent::BandwidthEstimate { bitrate }));
                        }
                        continue;
                    }
                }
            };
            match event? {
                Ok(QueuedEvent::Call(event)) => return Some(Ok(event)),
                Ok(QueuedEvent::SourceReady { index, revision })
                    if Some(revision) == self.sources.borrow().tracks[index] =>
                {
                    return Some(Ok(MediaEvent::SourceReady {
                        purpose: VIDEO[index],
                    }));
                }
                Err(error) => return Some(Err(error)),
                _ => {}
            }
        }
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.input);
        drop(self.frames);
        drop(self.audio);
        drop(self.events);
        drop(self.keyframes);
        self.task
            .await
            .map_err(|_| Error::Protocol("media task stopped"))
    }
}

impl Connection {
    /// Start native encoded video over the configured SFU. Local sources and
    /// subscriptions share the signaling connection until stop.
    pub async fn into_media(mut self, options: MediaOptions) -> Result<Media, Error> {
        if self.policy.relay_only && self.welcome.turn.is_none() {
            return Err(Error::Protocol(
                "native media requires TURN in relay-only mode",
            ));
        }
        if options.sfu_port == 0
            || options
                .publish
                .is_some_and(|kind| matches!(kind, Purpose::Microphone | Purpose::SystemAudio))
        {
            return Err(Error::Protocol("invalid video source or SFU port"));
        }
        let mut destination = self.socket.get_ref().get_ref().0.peer_addr()?;
        destination.set_port(options.sfu_port);
        let mut worker = self.scope.start_media(options.executable).await?;
        let codec = match self.welcome.codec {
            Codec::Av1 => rtc::Codec::Av1,
            Codec::Vp9 => rtc::Codec::Vp9,
        };
        let initial = Sources {
            tracks: VIDEO.map(|purpose| (options.publish == Some(purpose)).then_some(0)),
            audio: None,
            revision: 0,
        };
        let (input, control) = mpsc::channel(rtc::MAX_TRACKS);
        let ([camera, screen], frames) = frame_channels();
        let (quality, wanted) = watch::channel(Vec::new());
        let (sources, changes) = watch::channel(initial);
        let (output, events) = mpsc::channel(4);
        let (requests, keyframes) = frame_channels();
        let (audio, audio_in) = mpsc::channel(AUDIO_QUEUE);
        let (received_audio, audio_frames) = mpsc::channel(AUDIO_QUEUE);
        let (encoder_caps, limits) = watch::channel([None; 2]);
        let (upload_estimate, bandwidth) = watch::channel(None);
        let task = tokio::spawn(async move {
            let mut connection = self;
            let result = run(
                &mut connection,
                &mut worker,
                destination,
                codec,
                Inputs {
                    control,
                    frames,
                    audio: audio_in,
                    changes,
                    wanted,
                },
                Output {
                    events: &output,
                    keyframes: &requests,
                    audio: &received_audio,
                    limits: &encoder_caps,
                    bandwidth: &upload_estimate,
                },
            )
            .await;
            let _ = connection.close().await;
            let _ = worker.stop().await;
            if let Err(error) = result {
                let _ = output.send(Err(error)).await;
            }
        });
        Ok(Media {
            input,
            frames: [camera, screen],
            audio,
            audio_frames,
            sequence: Default::default(),
            quality,
            sources,
            events,
            keyframes,
            limits,
            reported_limits: [None; 2],
            bandwidth,
            task,
        })
    }
}

enum Network {
    Sfu(Box<NetworkLeg>),
    Direct {
        socket: tokio::net::UdpSocket,
        remote: Option<Candidates>,
    },
}

impl Network {
    fn address(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Sfu(leg) => Ok(leg.address()),
            Self::Direct { socket, .. } => socket.local_addr(),
        }
    }

    fn permits(&self, address: SocketAddr) -> std::io::Result<bool> {
        match self {
            Self::Sfu(leg) => Ok(leg.peer()? == address),
            Self::Direct { remote, .. } => {
                Ok(remote.is_some_and(|remote| remote.contains(address)))
            }
        }
    }

    async fn receive(&mut self, data: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match self {
            Self::Sfu(leg) => Ok((leg.recv(data).await?, leg.peer()?)),
            Self::Direct { socket, .. } => socket.recv_from(data).await,
        }
    }

    async fn send(&mut self, data: &[u8], destination: SocketAddr) -> std::io::Result<()> {
        if self.permits(destination)? {
            match self {
                Self::Sfu(leg) => leg.send(data).await?,
                Self::Direct { socket, .. } => {
                    socket.send_to(data, destination).await?;
                }
            }
        }
        Ok(())
    }
}

struct Leg {
    id: u32,
    network: Network,
    wake: Option<Instant>,
    connected: bool,
    selected: bool,
    bandwidth: Option<(Instant, u64)>,
    last_packet: Option<Instant>,
    video_gates: HashMap<(u64, Purpose), FrameGate>,
    /// Publishers whose shared desktop audio this leg receives.
    audio: HashSet<u64>,
    #[cfg(test)]
    throttle: Option<Shaper>,
}

/// A bottleneck for a leg's outgoing datagrams: a queue drained at a byte
/// rate that drops what would wait longer than a second.
#[cfg(test)]
struct Shaper {
    rate: f64,
    free: Instant,
    queue: std::collections::VecDeque<(Instant, Vec<u8>, SocketAddr)>,
}

#[cfg(test)]
impl Shaper {
    fn new(rate: u64) -> Self {
        Self {
            rate: rate as f64,
            free: Instant::now(),
            queue: Default::default(),
        }
    }

    fn push(&mut self, data: Vec<u8>, destination: SocketAddr) {
        let now = Instant::now();
        let start = self.free.max(now);
        if start <= now + Duration::from_secs(1) {
            self.free = start + Duration::from_secs_f64(data.len() as f64 / self.rate);
            self.queue.push_back((self.free, data, destination));
        }
    }

    fn due(&mut self) -> Option<(Vec<u8>, SocketAddr)> {
        let (at, ..) = self.queue.front()?;
        (*at <= Instant::now())
            .then(|| self.queue.pop_front())
            .flatten()
            .map(|(_, data, destination)| (data, destination))
    }
}

impl Leg {
    /// Gate each video track on its keyframes, and admit shared audio tracks.
    fn set_video_tracks(&mut self, tracks: &[Track]) {
        self.audio = tracks
            .iter()
            .filter(|track| track.purpose == Purpose::SystemAudio)
            .filter_map(|track| track.publisher)
            .collect();
        self.video_gates.retain(|&(publisher, purpose), _| {
            tracks
                .iter()
                .any(|track| track.publisher == Some(publisher) && track.purpose == purpose)
        });
        for track in tracks {
            if let Some(publisher) = track.publisher
                && video_kind(track.purpose).is_some()
            {
                self.video_gates
                    .entry((publisher, track.purpose))
                    .or_insert_with(FrameGate::new);
            }
        }
    }

    async fn request_keyframe(
        &mut self,
        worker: &mut Worker,
        publisher: u64,
        purpose: Purpose,
    ) -> Result<(), Error> {
        if let Some(gate) = self.video_gates.get_mut(&(publisher, purpose)) {
            gate.awaiting_key = true;
            if self.connected && gate.request() {
                worker
                    .exchange(
                        Command::RequestKeyframe {
                            leg: self.id,
                            publisher,
                            purpose,
                        },
                        Vec::new(),
                    )
                    .await?;
                self.wake = Some(Instant::now());
            }
        }
        Ok(())
    }

    async fn start(
        worker: &mut Worker,
        id: u32,
        network: Network,
        codec: rtc::Codec,
        role: LegRole,
    ) -> Result<Self, Error> {
        done(
            worker,
            Command::OpenLeg {
                leg: id,
                local: network.address()?,
                codec,
                role,
                relayed: matches!(&network, Network::Sfu(leg) if leg.is_relayed()),
            },
        )
        .await?;
        Ok(Self {
            id,
            network,
            wake: None,
            connected: false,
            selected: false,
            bandwidth: None,
            last_packet: None,
            video_gates: HashMap::new(),
            audio: HashSet::new(),
            #[cfg(test)]
            throttle: None,
        })
    }

    async fn datagram(
        &mut self,
        worker: &mut Worker,
        source: SocketAddr,
        data: &[u8],
    ) -> Result<(), Error> {
        if data.is_empty() || data.len() > rtc::MAX_DATAGRAM || !self.network.permits(source)? {
            return Ok(());
        }
        let reply = worker
            .exchange(
                Command::Datagram {
                    leg: self.id,
                    source,
                },
                data.to_vec(),
            )
            .await?;
        let now = Instant::now();
        if matches!(reply.kind, Reply::Done) && matches!(self.network, Network::Direct { .. }) {
            self.last_packet = Some(now);
        }
        self.wake = Some(now);
        Ok(())
    }
}

async fn receive(leg: Option<&mut Leg>, bytes: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
    match leg {
        Some(leg) => leg.network.receive(bytes).await,
        None => pending().await,
    }
}

async fn wake(until: Option<Instant>) {
    match until {
        Some(until) if until > Instant::now() => sleep_until(until).await,
        Some(_) => {}
        None => pending().await,
    }
}

async fn done(worker: &mut Worker, command: Command) -> Result<(), Error> {
    match worker.exchange(command, Vec::new()).await?.kind {
        Reply::Done => Ok(()),
        _ => Err(Error::Protocol("RTC worker rejected media setup")),
    }
}

async fn describe(worker: &mut Worker, command: Command) -> Result<Description, Error> {
    match worker.exchange(command, Vec::new()).await?.kind {
        Reply::Description { description, .. } => Ok(description),
        _ => Err(Error::Protocol("RTC worker rejected media description")),
    }
}

struct Output<'a> {
    events: &'a mpsc::Sender<Result<QueuedEvent, Error>>,
    keyframes: &'a [mpsc::Sender<()>; 2],
    audio: &'a mpsc::Sender<MediaEvent>,
    // Feedback updates carry the latest value without holding up media I/O.
    limits: &'a watch::Sender<SourceCaps>,
    bandwidth: &'a watch::Sender<Option<u64>>,
}

impl Output<'_> {
    fn keyframe(&self, index: usize) {
        let _ = self.keyframes[index].try_send(());
    }

    fn upload(&self, estimate: Option<u64>) {
        if let Some(rate) = estimate {
            self.bandwidth
                .send_if_modified(|current| current.replace(rate) != Some(rate));
        }
    }

    async fn call(&self, event: MediaEvent) -> bool {
        self.events.send(Ok(QueuedEvent::Call(event))).await.is_ok()
    }
}

struct FrameGate {
    awaiting_key: bool,
    requested: Option<Instant>,
}

impl FrameGate {
    fn new() -> Self {
        Self {
            awaiting_key: true,
            requested: None,
        }
    }

    fn request(&mut self) -> bool {
        let now = Instant::now();
        if self
            .requested
            .is_some_and(|at| now - at < KEYFRAME_INTERVAL)
        {
            return false;
        }
        self.requested = Some(now);
        true
    }

    fn arrived(&mut self, key_frame: bool) {
        if key_frame {
            self.awaiting_key = false;
            self.requested = None;
        }
    }
}

fn signaling_id(connection: &Connection, leg: u32) -> [u8; 16] {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&connection.scope.generation.to_be_bytes());
    id[8..12].copy_from_slice(&leg.to_be_bytes());
    id
}

type QualityRequests = Vec<QualityRequest>;

struct Inputs {
    control: mpsc::Receiver<Input>,
    frames: [mpsc::Receiver<Video>; 2],
    audio: mpsc::Receiver<Audio>,
    changes: watch::Receiver<Sources>,
    wanted: watch::Receiver<QualityRequests>,
}

fn frame_channels<T>() -> ([mpsc::Sender<T>; 2], [mpsc::Receiver<T>; 2]) {
    let (camera, camera_in) = mpsc::channel(1);
    let (screen, screen_in) = mpsc::channel(1);
    ([camera, screen], [camera_in, screen_in])
}

async fn next_source<T>(channels: &mut [mpsc::Receiver<T>; 2]) -> (usize, Option<T>) {
    let [camera, screen] = channels;
    tokio::select! {
        frame = camera.recv() => (0, frame),
        frame = screen.recv() => (1, frame),
    }
}

#[derive(Default)]
struct ReceiveQuality {
    wanted: QualityRequests,
    sent: HashMap<(u64, VideoKind), (QualityRequest, Instant)>,
}

impl ReceiveQuality {
    fn next_request(&self, tracks: &[Track]) -> Option<(Instant, QualityRequest)> {
        self.wanted
            .iter()
            .filter_map(|&request| {
                let key = (request.publisher, request.kind);
                let sent = self.sent.get(&key);
                if sent.is_some_and(|(previous, _)| *previous == request)
                    || !tracks.iter().any(|track| {
                        track.publisher == Some(request.publisher)
                            && video_kind(track.purpose) == Some(request.kind)
                    })
                {
                    return None;
                }
                Some((sent.map_or_else(Instant::now, |(_, at)| *at), request))
            })
            .min_by_key(|(at, _)| *at)
    }

    fn sent(&mut self, request: QualityRequest) {
        // Give signaling transit jitter the same 100 ms margin as the call page.
        let next = Instant::now() + Throttle::INTERVAL + Duration::from_millis(100);
        self.sent
            .insert((request.publisher, request.kind), (request, next));
    }

    fn source_ended(&mut self, publisher: u64, purpose: Purpose) {
        if let Some(kind) = video_kind(purpose) {
            self.sent.remove(&(publisher, kind));
        }
    }
}

fn video_kind(purpose: Purpose) -> Option<VideoKind> {
    match purpose {
        Purpose::Camera => Some(VideoKind::Camera),
        Purpose::ScreenVideo => Some(VideoKind::Screen),
        _ => None,
    }
}

fn quality_index(kind: VideoKind) -> usize {
    match kind {
        VideoKind::Camera => 0,
        VideoKind::Screen => 1,
    }
}

fn video_index(purpose: Purpose) -> Result<usize, Error> {
    video_kind(purpose)
        .map(quality_index)
        .ok_or(Error::Protocol("invalid video source"))
}

async fn run(
    connection: &mut Connection,
    worker: &mut Worker,
    destination: SocketAddr,
    codec: rtc::Codec,
    mut inputs: Inputs,
    output: Output<'_>,
) -> Result<(), Error> {
    let mut peers: Vec<_> = connection
        .welcome
        .peers
        .iter()
        .map(|peer| peer.id)
        .collect();
    let mut remotes = HashSet::new();
    let mut role = Err(Reason::Sender);
    let mut upload = None;
    let mut reported = None;
    let mut key_tick = interval(Duration::from_millis(250));
    key_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut offered: Option<Description> = None;
    let mut publish_tracks = Vec::<Track>::new();
    let mut source_change_deadline = None;
    let mut pending_source = Some(*inputs.changes.borrow_and_update());
    let mut source = Sources::default();
    let turn = connection
        .welcome
        .turn
        .clone()
        .filter(|_| connection.policy.relay_only);
    let subscribe_path = NetworkLeg::open(destination, turn.as_ref()).await?;
    let mut publish: Option<Leg> = None;
    let mut subscribe = Leg::start(
        worker,
        SUBSCRIBE,
        Network::Sfu(Box::new(subscribe_path)),
        codec,
        LegRole::SfuSubscribe,
    )
    .await?;
    let mut direct = if connection.policy.permits(Destination::DirectPeer) {
        Some(Direct::new(
            connection.socket.get_ref().get_ref().0.local_addr()?.ip(),
            connection
                .welcome
                .turn
                .as_ref()
                .map_or(&[], |turn| turn.urls.as_slice()),
        ))
    } else {
        None
    };
    let mut discovery = None;
    let mut desired = [None::<u64>; 2];
    let mut direct_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut subscribed = Vec::<Track>::new();
    let mut quality = ReceiveQuality::default();
    let mut pub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut sub_buffer = [0; rtc::MAX_DATAGRAM + 1];
    let mut sending = [None; 2];
    let mut audio_sending = None;
    let mut send_gates = std::array::from_fn::<_, 2, _>(|_| FrameGate::new());
    let mut sequence = [0_u64; 2];
    loop {
        let route = direct
            .as_ref()
            .map_or(Route::Sfu(Reason::RelayOnly), |direct| {
                direct.route(&role, upload)
            });
        if reported.replace(route) != Some(route) && !output.call(MediaEvent::Route(route)).await {
            return Ok(());
        }
        let selected = direct.as_ref().is_some_and(Direct::selected);
        let sending_direct = selected && matches!(role, Ok(Role::Send(_)));
        let receiving_direct = selected && matches!(role, Ok(Role::Receive(_)));
        let attempting_direct = direct
            .as_ref()
            .is_some_and(|direct| !direct.legs.is_empty())
            || discovery.is_some();
        let direct_wake = direct.as_ref().and_then(Direct::wake);
        let direct_deadline = direct.as_ref().and_then(Direct::deadline);
        let publish_wake = publish.as_ref().and_then(|leg| leg.wake);
        let subscribe_wake = subscribe.wake;
        let next_quality = quality.next_request(&subscribed);
        let mut retreat = false;
        let mut cause = Some(Reason::Failed);
        tokio::select! {
            message = connection.next() => match message? {
                ServerMessage::Joined { peer, .. } => {
                    if peer == 0 || peer == connection.welcome.peer {
                        return Err(Error::Protocol("invalid call peer"));
                    }
                    if !peers.contains(&peer) {
                        if peers.len() >= 256 { return Err(Error::Protocol("invalid call roster")); }
                        peers.push(peer);
                    }
                }
                ServerMessage::Left { peer } => {
                    peers.retain(|candidate| *candidate != peer);
                    remotes.remove(&peer);
                    if let Some(direct) = &mut direct {
                        direct.ready.retain(|ready| *ready != peer);
                    }
                    connection.signal(Action::Left { peer }).await?;
                    let next = subscribed.iter().filter(|track| track.publisher != Some(peer)).cloned().collect();
                    if !set_subscriptions(&mut subscribe, &mut quality, &mut subscribed, next, &output).await {
                        return Ok(());
                    }
                }
                ServerMessage::Offer { sdp, tracks } => {
                    if sdp.len() > rtc::MAX_SDP || tracks.len() > rtc::MAX_TRACKS { return Err(Error::Protocol("invalid native SFU offer")); }
                    let tracks = tracks.into_iter().map(|track| {
                        let purpose = match track.kind.as_ref() {
                            "camera" => Purpose::Camera,
                            "screen" => Purpose::ScreenVideo,
                            "audio" => Purpose::SystemAudio,
                            _ => return Err(Error::Protocol("invalid media source")),
                        };
                        if track.peer == 0 || !peers.contains(&track.peer) {
                            return Err(Error::Protocol("invalid media publisher"));
                        }
                        Ok(Track { mid: track.mid, purpose, publisher: Some(track.peer) })
                    }).collect::<Result<Vec<_>, _>>()?;
                    let mut seen = HashSet::new();
                    let next = tracks.iter().filter(|track| {
                        (video_kind(track.purpose).is_some() || track.purpose == Purpose::SystemAudio)
                            && seen.insert((track.publisher, track.purpose))
                    }).cloned().collect::<Vec<_>>();
                    let description = Description { id: signaling_id(connection, SUBSCRIBE), sdp, tracks };
                    let answer = describe(worker, Command::AcceptOffer { leg: SUBSCRIBE, description }).await?;
                    write(&mut connection.socket, &ClientMessage::Answer { sdp: answer.sdp }).await?;
                    for publisher in next.iter().filter_map(|track| track.publisher) {
                        if remotes.insert(publisher) {
                            connection.signal(Action::Subscribe { peer: publisher }).await?;
                        }
                    }
                    retreat |= attempting_direct && subscribed.iter().any(|track| !next.contains(track));
                    if !set_subscriptions(&mut subscribe, &mut quality, &mut subscribed, next, &output).await {
                        return Ok(());
                    }
                    subscribe.wake = Some(Instant::now());
                }
                ServerMessage::Answer { sdp } => {
                    let mut description = offered.take().ok_or(Error::Protocol("unexpected publish answer"))?;
                    description.sdp = sdp;
                    done(worker, Command::AcceptAnswer { leg: PUBLISH, description }).await?;
                    source_change_deadline = None;
                    if let Some(leg) = &mut publish { leg.wake = Some(Instant::now()); }
                }
                ServerMessage::Key { from, data } if peers.contains(&from) => {
                    if let Ok(data) = hex::decode(data)
                        && let Ok(Outcome::Signal { publisher, data, .. }) =
                            connection.signal(Action::Key { peer: from, data }).await
                        && let Some(direct) = &mut direct
                        && let Ok(signal) = serde_json::from_slice(&data)
                    {
                        // `publisher` names this endpoint's authenticated side of the pair.
                        let paired = match &role {
                            Ok(Role::Send(viewers)) => publisher && viewers.contains(&from),
                            Ok(Role::Receive(sender)) => !publisher && *sender == from,
                            Err(_) => false,
                        };
                        let result = match signal {
                            Signal::Ready if publisher => {
                                if !direct.ready.contains(&from) {
                                    direct.ready.push(from);
                                }
                                Ok(())
                            }
                            Signal::Offer { candidates, description }
                                if paired && direct.can_accept() && discovery.is_none() =>
                            {
                                discovery = direct.accept(from, candidates, description, &subscribed);
                                Ok(())
                            }
                            Signal::Answer { candidates, description } if paired && publisher => {
                                direct.answer(worker, from, candidates, description).await
                            }
                            Signal::Connected { id } if paired && publisher => {
                                if let Some(attempt) = direct.legs.iter_mut().find(|attempt| attempt.peer == from && attempt.id == id) {
                                    attempt.confirmed = true;
                                }
                                Ok(())
                            }
                            Signal::Close { id } => {
                                retreat = direct.legs.iter().any(|attempt| attempt.peer == from && attempt.id == id);
                                if !publisher {
                                    cause = Some(Reason::Sender);
                                }
                                Ok(())
                            }
                            _ => Ok(()),
                        };
                        retreat |= result.is_err();
                    }
                }
                ServerMessage::Quality { source: mid, max_height, max_fps, max_bitrate, limit, .. }
                    if (16..=2160).contains(&max_height) && (1..=120).contains(&max_fps) && (64_000..=nuschl_protocol::sfu::MAX_SCREEN_BITRATE).contains(&max_bitrate) => {
                    if let Some(track) = publish_tracks.iter().find(|track| track.mid == mid)
                        && let Ok(index) = video_index(track.purpose)
                        && let Some(revision) = source.tracks[index]
                    {
                        // Direct admission is based on the selected tier. The
                        // SFU path's headroom can move during setup or handoff.
                        desired[index] = Some(if video_kind(track.purpose) == Some(VideoKind::Screen) {
                            VideoKind::Screen.ladder()[VideoKind::Screen.tier_of(u32::from(max_height))].ceiling
                        } else {
                            max_bitrate
                        });
                        output.limits.send_modify(|limits| limits[index] = Some(EncoderCaps {
                            revision, height: max_height, fps: max_fps, bitrate: max_bitrate, limit,
                        }));
                    }
                }
                ServerMessage::VideoLimit { mid, peer: publisher, limit } => {
                    if let Some(track) = subscribed.iter().find(|track| {
                        track.mid == mid && track.publisher == Some(publisher) && video_kind(track.purpose).is_some()
                    }) && !output.call(MediaEvent::VideoLimit { publisher, purpose: track.purpose, limit }).await {
                        return Ok(());
                    }
                }
                ServerMessage::Codec { codec: changed } if changed != connection.welcome.codec => return Err(Error::Protocol("native call codec changed")),
                ServerMessage::Error { reason } => return Err(Error::Refused(reason.into_owned())),
                _ => {}
            },
            changed = inputs.changes.changed() => {
                if changed.is_err() { return Ok(()); }
                pending_source = Some(*inputs.changes.borrow_and_update());
            }
            changed = inputs.wanted.changed() => {
                if changed.is_err() { return Ok(()); }
                quality.wanted.clone_from(&inputs.wanted.borrow_and_update());
            }
            () = wake(next_quality.map(|(at, _)| at)) => {
                quality.wanted.clone_from(&inputs.wanted.borrow_and_update());
                if let Some((at, request)) = quality.next_request(&subscribed)
                    && at <= Instant::now()
                {
                    write(&mut connection.socket, &ClientMessage::Quality {
                        request: hex::encode(request.to_bytes()),
                    }).await?;
                    quality.sent(request);
                }
            }
            input = inputs.control.recv() => match input {
                Some(Input::RequestKeyframe(publisher, purpose)) => {
                    if let Some(publisher) = subscribed.iter().find(|track| {
                        track.purpose == purpose && publisher.is_none_or(|peer| track.publisher == Some(peer))
                    }).and_then(|track| track.publisher) {
                        let active = direct.as_mut()
                            .and_then(|direct| direct.legs.iter_mut().find(|attempt| attempt.transport.selected && attempt.peer == publisher))
                            .map_or(&mut subscribe, |attempt| &mut attempt.transport);
                        active.request_keyframe(worker, publisher, purpose).await?;
                    }
                }
                #[cfg(test)]
                Some(Input::ThrottleDirect(rate)) => {
                    for attempt in direct.iter_mut().flat_map(|direct| &mut direct.legs) {
                        attempt.transport.throttle = Some(Shaper::new(rate));
                    }
                }
                #[cfg(test)]
                Some(Input::LoseDirect) => {
                    for attempt in direct.iter_mut().flat_map(|direct| &mut direct.legs) {
                        if let Network::Direct { remote, .. } = &mut attempt.transport.network {
                            *remote = None;
                        }
                    }
                }
                None => return Ok(()),
            },
            frame = inputs.audio.recv() => {
                let Some(frame) = frame else { return Ok(()); };
                if Some(frame.revision) == inputs.changes.borrow().audio
                    && Some(frame.revision) == audio_sending
                    && frame.queued.elapsed() <= MAX_PENDING_FRAME_AGE
                    && (sending_direct || publish.is_some())
                {
                    let command = Command::Write { purpose: Purpose::SystemAudio, timestamp: frame.timestamp, key_frame: false };
                    match worker.exchange(command, frame.data).await?.kind {
                        Reply::Done => {
                            let now = Some(Instant::now());
                            if sending_direct {
                                for attempt in direct.iter_mut().flat_map(|direct| &mut direct.legs) {
                                    attempt.transport.wake = now;
                                }
                            } else if let Some(leg) = &mut publish {
                                leg.wake = now;
                            }
                        }
                        Reply::Rejected => {}
                        _ => return Err(Error::Protocol("invalid RTC write reply")),
                    }
                }
            }
            (index, frame) = next_source(&mut inputs.frames) => {
                let Some(frame) = frame else { return Ok(()); };
                let allowed = Some(frame.revision) == inputs.changes.borrow().tracks[index];
                if allowed && Some(frame.revision) == sending[index] && (sending_direct || publish.is_some()) {
                    let gate = &mut send_gates[index];
                    if frame.sequence != sequence[index].wrapping_add(1) {
                        gate.awaiting_key = true;
                    }
                    sequence[index] = frame.sequence;
                    if frame.queued.elapsed() <= MAX_PENDING_FRAME_AGE && (!gate.awaiting_key || frame.key_frame) {
                        let command = Command::Write { purpose: VIDEO[index], timestamp: frame.timestamp, key_frame: frame.key_frame };
                        match worker.exchange(command, frame.data).await?.kind {
                            Reply::Done => {
                                gate.arrived(frame.key_frame);
                                let now = Some(Instant::now());
                                if sending_direct {
                                    for attempt in direct.iter_mut().flat_map(|direct| &mut direct.legs) {
                                        attempt.transport.wake = now;
                                    }
                                } else if let Some(leg) = &mut publish {
                                    leg.wake = now;
                                }
                                continue;
                            }
                            Reply::Rejected => {}
                            _ => return Err(Error::Protocol("invalid RTC write reply")),
                        }
                    }
                    gate.awaiting_key = true;
                    if gate.request() {
                        output.keyframe(index);
                    }
                }
            }
            received = receive(publish.as_mut(), &mut pub_buffer) => {
                if let Some(leg) = &mut publish {
                    let (size, source) = received?;
                    leg.datagram(worker, source, &pub_buffer[..size]).await?;
                }
            }
            received = subscribe.network.receive(&mut sub_buffer) => {
                let (size, source) = received?;
                subscribe.datagram(worker, source, &sub_buffer[..size]).await?;
            }
            (index, received) = direct::receive(direct.as_ref().map_or(&[], |direct| direct.legs.as_slice()), &mut direct_buffer) => {
                if let Some(attempt) = direct.as_mut().and_then(|direct| direct.legs.get_mut(index)) {
                    retreat = match received {
                        Ok((size, source)) => attempt.transport.datagram(worker, source, &direct_buffer[..size]).await.is_err(),
                        Err(_) => true,
                    };
                }
            }
            result = direct::next_prepared(&mut discovery) => {
                discovery = None;
                if let (Some(direct), Ok(role)) = (&mut direct, &role) {
                    let publisher = matches!(role, Role::Send(_));
                    retreat = match result {
                        Ok(prepared) => match direct.complete(worker, codec, prepared).await {
                            Ok((peer, signal)) => direct::send(connection, peer, publisher, &signal).await.is_err(),
                            Err(_) => true,
                        },
                        Err(_) => true,
                    };
                }
            }
            () = wake(publish_wake) => {
                if let Some(leg) = &mut publish {
                    drain(worker, leg, false, !sending_direct, &output).await?;
                    if !sending_direct {
                        output.upload(leg.bandwidth.map(|(_, rate)| rate));
                    }
                }
            }
            () = wake(subscribe_wake) => { drain(worker, &mut subscribe, !receiving_direct, false, &output).await?; }
            () = wake(direct_wake) => {
                let receiving = matches!(role, Ok(Role::Receive(_)));
                let now = Instant::now();
                for attempt in direct.iter_mut().flat_map(|direct| &mut direct.legs) {
                    if attempt.answered && attempt.transport.wake.is_some_and(|at| at <= now) {
                        retreat |= drain(worker, &mut attempt.transport, receiving, sending_direct, &output).await.is_err();
                    }
                }
                // One encoding serves every direct viewer.
                if sending_direct {
                    output.upload(direct.iter().flat_map(|direct| &direct.legs).map(|attempt| attempt.transport.bandwidth.map(|(_, rate)| rate)).min().flatten());
                }
            }
            () = wake(direct_deadline) => retreat = true,
            () = wake(source_change_deadline) => return Err(Error::Timeout),
            _ = key_tick.tick(), if publish.is_some() => { connection.signal(Action::Tick).await?; }
            () = std::future::ready(()), if offered.is_none() && pending_source.is_some() => {}
        }
        if offered.is_none()
            && let Some(next) = pending_source.take()
            && (source.tracks, source.audio) != (next.tracks, next.audio)
        {
            let update = publish.is_some();
            if publish.is_none() {
                connection
                    .signal(Action::Publish {
                        peer: connection.welcome.peer,
                    })
                    .await?;
                let path = NetworkLeg::open(destination, turn.as_ref()).await?;
                publish = Some(
                    Leg::start(
                        worker,
                        PUBLISH,
                        Network::Sfu(Box::new(path)),
                        codec,
                        LegRole::SfuPublish,
                    )
                    .await?,
                );
            }
            let leg = publish
                .as_mut()
                .ok_or(Error::Protocol("call has no publisher"))?;
            retreat |= attempting_direct;
            let description = offer_publish(connection, worker, source, next, update).await?;
            publish_tracks.clone_from(&description.tracks);
            offered = Some(description);
            source_change_deadline = Some(Instant::now() + HANDSHAKE);
            leg.wake = Some(Instant::now());
            for index in 0..VIDEO.len() {
                if source.tracks[index] != next.tracks[index] {
                    desired[index] = None;
                    sending[index] = None;
                    send_gates[index] = FrameGate::new();
                    output.limits.send_modify(|limits| limits[index] = None);
                }
            }
            if source.audio != next.audio {
                audio_sending = None;
            }
            source = next;
        }
        if offered.is_none()
            && (source.tracks != sending || source.audio != audio_sending)
            && publish.as_ref().is_some_and(|leg| leg.connected)
        {
            // The crypto worker delivers the epoch on a separate private channel.
            timeout(Duration::from_secs(1), async {
                loop {
                    if matches!(
                        worker
                            .exchange(
                                Command::SetSendLegs {
                                    legs: vec![PUBLISH]
                                },
                                Vec::new()
                            )
                            .await?
                            .kind,
                        Reply::Done
                    ) {
                        return Ok::<(), Error>(());
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| Error::Timeout)??;
            for (index, revision) in source.tracks.into_iter().enumerate() {
                if let Some(revision) = revision
                    && sending[index] != Some(revision)
                    && output
                        .events
                        .send(Ok(QueuedEvent::SourceReady { index, revision }))
                        .await
                        .is_err()
                {
                    return Ok(());
                }
            }
            sending = source.tracks;
            audio_sending = source.audio;
        }

        let Some(direct) = &mut direct else {
            continue;
        };
        let publishing = source.tracks.iter().any(Option::is_some) || source.audio.is_some();
        let next_role = direct::role(&peers, publishing, &subscribed);
        if next_role != role {
            retreat = true;
            cause = None;
        }
        let desired = source
            .tracks
            .iter()
            .zip(desired)
            .try_fold(0_u64, |sum, (active, desired)| {
                if active.is_some() {
                    desired.map(|desired| sum + desired)
                } else {
                    Some(sum)
                }
            })
            .map(|sum| sum + source.audio.map_or(0, |_| AUDIO_BITRATE))
            .filter(|desired| *desired != 0);
        // Each direct leg carries the whole stream.
        if let Some(desired) = desired
            && direct.selected()
        {
            let slow = direct.legs.iter().any(|attempt| {
                attempt
                    .transport
                    .bandwidth
                    .is_some_and(|(_, rate)| !direct::sustains(rate, desired))
            });
            if slow {
                retreat = true;
                cause = Some(Reason::Upload);
            }
        }
        if retreat {
            discovery = None;
            if let Ok(current) = &role {
                let sender = matches!(current, Role::Send(_));
                let was_selected = direct.selected();
                let closed = direct
                    .close(worker, publish.as_mut().filter(|_| sender), cause)
                    .await?;
                for (peer, id) in closed {
                    let _ = direct::send(connection, peer, sender, &Signal::Close { id }).await;
                }
                if was_selected {
                    if sender {
                        output.bandwidth.send_replace(None);
                        for (index, gate) in send_gates.iter_mut().enumerate() {
                            *gate = FrameGate::new();
                            if sending[index].is_some() {
                                output.keyframe(index);
                            }
                        }
                    } else {
                        for track in &subscribed {
                            if let Some(publisher) = track.publisher {
                                subscribe
                                    .request_keyframe(worker, publisher, track.purpose)
                                    .await?;
                            }
                        }
                    }
                }
            }
        }
        if role != next_role {
            role = next_role;
            direct.asked = false;
        }
        upload = match (&role, &publish, desired) {
            (Ok(Role::Send(viewers)), Some(publish), Some(desired)) => {
                direct::upload(publish.bandwidth, desired, viewers.len())
            }
            _ => None,
        };
        match &role {
            // Verify that the fallback can deliver this publisher's media before
            // requesting a direct route. A grant can arrive before the sender rotates.
            Ok(Role::Receive(publisher)) => {
                if !direct.asked
                    && subscribe
                        .video_gates
                        .iter()
                        .any(|(&(from, _), gate)| from == *publisher && !gate.awaiting_key)
                {
                    direct.asked = true;
                    let _ = direct::send(connection, *publisher, false, &Signal::Ready).await;
                }
                if let Some(attempt) = direct.legs.first_mut()
                    && attempt.transport.connected
                    && !attempt.confirmed
                {
                    attempt.confirmed = true;
                    let id = attempt.id;
                    if direct::send(connection, *publisher, false, &Signal::Connected { id })
                        .await
                        .is_err()
                    {
                        direct.close(worker, None, Some(Reason::Failed)).await?;
                    }
                }
            }
            Ok(Role::Send(viewers)) => {
                // 80 % of the probed upload carries the stream to every viewer.
                if direct.selected()
                    || upload != Some(true)
                    || !sending.iter().any(Option::is_some)
                    || !viewers.iter().all(|viewer| direct.ready.contains(viewer))
                {
                    continue;
                }
                if let Some(&viewer) = viewers
                    .iter()
                    .find(|viewer| !direct.legs.iter().any(|attempt| attempt.peer == **viewer))
                {
                    if direct.can_offer() && discovery.is_none() {
                        discovery =
                            Some(direct.discover(viewer, direct::Setup::Offer(source.active())));
                    }
                    continue;
                }
                if !direct
                    .legs
                    .iter()
                    .all(|attempt| attempt.transport.connected && attempt.confirmed)
                {
                    continue;
                }
                let legs = direct
                    .legs
                    .iter()
                    .map(|attempt| attempt.transport.id)
                    .collect();
                if !matches!(
                    worker
                        .exchange(Command::SetSendLegs { legs }, Vec::new())
                        .await?
                        .kind,
                    Reply::Done,
                ) {
                    for (peer, id) in direct.close(worker, None, Some(Reason::Failed)).await? {
                        let _ = direct::send(connection, peer, true, &Signal::Close { id }).await;
                    }
                    continue;
                }
                output.bandwidth.send_replace(None);
                let now = Some(Instant::now());
                for attempt in &mut direct.legs {
                    attempt.transport.selected = true;
                    attempt.transport.wake = now;
                }
                for (index, gate) in send_gates.iter_mut().enumerate() {
                    *gate = FrameGate::new();
                    if sending[index].is_some() {
                        output.keyframe(index);
                    }
                }
            }
            Err(_) => {}
        }
    }
}

async fn set_subscriptions(
    leg: &mut Leg,
    quality: &mut ReceiveQuality,
    current: &mut Vec<Track>,
    next: Vec<Track>,
    output: &Output<'_>,
) -> bool {
    for track in current.iter().filter(|track| !next.contains(track)) {
        if let Some(publisher) = track.publisher {
            leg.video_gates.remove(&(publisher, track.purpose));
            quality.source_ended(publisher, track.purpose);
            if !output
                .call(MediaEvent::SourceEnded {
                    publisher,
                    purpose: track.purpose,
                })
                .await
            {
                return false;
            }
        }
    }
    for track in next.iter().filter(|track| !current.contains(track)) {
        if let Some(publisher) = track.publisher
            && !output
                .call(MediaEvent::SourceAvailable {
                    publisher,
                    purpose: track.purpose,
                })
                .await
        {
            return false;
        }
    }
    leg.set_video_tracks(&next);
    *current = next;
    true
}

async fn offer_publish(
    connection: &mut Connection,
    worker: &mut Worker,
    previous: Sources,
    next: Sources,
    update: bool,
) -> Result<Description, Error> {
    let mut id = signaling_id(connection, PUBLISH);
    id[15] = next.revision;
    let purposes = next.active();
    let command = if update {
        let restart = VIDEO
            .into_iter()
            .enumerate()
            .filter_map(|(index, purpose)| {
                (previous.tracks[index].is_some()
                    && next.tracks[index].is_some()
                    && previous.tracks[index] != next.tracks[index])
                    .then_some(purpose)
            })
            .collect();
        Command::UpdatePublish {
            leg: PUBLISH,
            id,
            purposes: purposes.clone(),
            restart,
        }
    } else {
        Command::Offer {
            leg: PUBLISH,
            id,
            purposes: purposes.clone(),
        }
    };
    let mut description = describe(worker, command).await?;
    if description.tracks.len() != purposes.len()
        || purposes.iter().any(|purpose| {
            !description
                .tracks
                .iter()
                .any(|track| track.purpose == *purpose && track.publisher.is_none())
        })
    {
        return Err(Error::Protocol("invalid publish tracks"));
    }
    let screens = description
        .tracks
        .iter()
        .filter(|track| track.purpose == Purpose::ScreenVideo)
        .map(|track| track.mid.clone())
        .collect();
    let sdp = std::mem::take(&mut description.sdp);
    write(
        &mut connection.socket,
        &ClientMessage::Offer { sdp, screens },
    )
    .await?;
    Ok(description)
}

async fn drain(
    worker: &mut Worker,
    leg: &mut Leg,
    receiving: bool,
    sending: bool,
    output: &Output<'_>,
) -> Result<(), Error> {
    #[cfg(test)]
    while let Some((data, destination)) = leg.throttle.as_mut().and_then(Shaper::due) {
        leg.network.send(&data, destination).await?;
    }
    for _ in 0..UDP_BURST {
        let packet = worker
            .exchange(Command::Poll { leg: leg.id }, Vec::new())
            .await?;
        match packet.kind {
            Reply::Wake { after_millis, .. } => {
                leg.wake = Some(Instant::now() + Duration::from_millis(after_millis));
                #[cfg(test)]
                if let Some((at, ..)) = leg
                    .throttle
                    .as_ref()
                    .and_then(|shaper| shaper.queue.front())
                {
                    leg.wake = leg.wake.min(Some(*at));
                }
                return Ok(());
            }
            Reply::Datagram { destination, .. } => {
                #[cfg(test)]
                if let Some(shaper) = &mut leg.throttle {
                    shaper.push(packet.data, destination);
                    continue;
                }
                leg.network.send(&packet.data, destination).await?;
            }
            Reply::Event { event, .. } => match event {
                Event::Connected => leg.connected = true,
                Event::Disconnected => {
                    return Err(Error::Protocol("native media transport disconnected"));
                }
                Event::BandwidthEstimate { bitrate } => {
                    leg.bandwidth = Some((Instant::now(), bitrate));
                }
                Event::Keyframe { purpose } if sending => {
                    if let Ok(index) = video_index(purpose) {
                        output.keyframe(index);
                    }
                }
                Event::Frame {
                    publisher,
                    purpose: Purpose::SystemAudio,
                    timestamp,
                    contiguous,
                    ..
                } if receiving => {
                    if leg.audio.contains(&publisher) {
                        let _ = output.audio.try_send(MediaEvent::Frame {
                            publisher,
                            purpose: Purpose::SystemAudio,
                            timestamp,
                            contiguous,
                            key_frame: false,
                            data: packet.data,
                        });
                    }
                }
                Event::Frame {
                    publisher,
                    purpose,
                    timestamp,
                    contiguous,
                    key_frame,
                } if receiving => {
                    let Some(gate) = leg.video_gates.get_mut(&(publisher, purpose)) else {
                        continue;
                    };
                    if (key_frame || (contiguous && !gate.awaiting_key))
                        && output
                            .events
                            .try_send(Ok(QueuedEvent::Call(MediaEvent::Frame {
                                publisher,
                                purpose,
                                timestamp,
                                contiguous: contiguous && !gate.awaiting_key,
                                key_frame,
                                data: packet.data,
                            })))
                            .is_ok()
                    {
                        gate.arrived(key_frame);
                        // A delivered frame verifies a direct leg.
                        leg.selected |= leg.id >= rtc::FIRST_DIRECT_LEG;
                    } else {
                        leg.request_keyframe(worker, publisher, purpose).await?;
                    }
                }
                _ => {}
            },
            _ => return Err(Error::Protocol("invalid RTC worker output")),
        }
    }
    leg.wake = Some(Instant::now());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_accepted_keyframe_starts_a_fresh_recovery_window() {
        let mut gate = FrameGate::new();
        let request = Instant::now();
        gate.requested = Some(request);

        gate.arrived(false);
        assert!(gate.awaiting_key);
        assert_eq!(gate.requested, Some(request));

        gate.arrived(true);
        assert!(!gate.awaiting_key);
        assert_eq!(gate.requested, None);
        assert!(gate.request());
    }
}
