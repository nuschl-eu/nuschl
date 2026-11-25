// SPDX-License-Identifier: EUPL-1.2
//! WebRTC transport. Encoded media crosses this boundary; codecs and devices do not.
//!
//! The caller owns UDP, authenticated signaling and scheduling. Drain `poll` to
//! `WakeAt` after each input, then wake by that deadline. Never drain an unbounded
//! socket backlog before polling output: doing so can starve RTCP and the pacer.
//!
//! `offer_tracks` binds semantic purposes to MIDs in a structured description.
//! The caller must authenticate that entire value and provide the expected fresh
//! signaling ID at both acceptance steps. The legacy string API supports only
//! microphone plus optional camera; it never guesses roles for a second stream.
//! This is an encoded-media transport foundation, not a capture, calling,
//! ownership/consent, relay, or end-to-end media encryption implementation.

use nuschl_core::network::{Destination, PeerPolicy};
use nuschl_stream_quality::CAMERA;
use std::{
    io,
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, KeyframeRequestKind, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, Input, Output as RtcOutput, Rtc};
use thiserror::Error;

fn camera_share(video: u64) -> u64 {
    // The desktop camera's largest target plus transport overhead.
    (CAMERA[CAMERA.len() - 1].ceiling * 10)
        .div_ceil(9)
        .min(video / 2)
}

pub const MAX_SDP_BYTES: usize = 32 * 1024;
pub const MAX_DATAGRAM_BYTES: usize = 2048;
pub const MAX_OPUS_BYTES: usize = 1275;
pub const MAX_VIDEO_BYTES: usize = 256 * 1024;
const INITIAL_BITRATE: u64 = 2_000_000;
const CALL_INITIAL_BITRATE: u64 = 5_000_000;
const CALL_INITIAL_PROBE_CEILING: u64 = 50_000_000;
// The estimate includes packet overhead and leaves headroom for the video target.
const CALL_MAX_BITRATE: u64 = 150_000_000;
const CALL_VIDEO_WINDOW: Duration = Duration::from_millis(300);
const CALL_RTP_PACKETS: usize = 1024;
/// The microphone role's reservation, including the overhead `write` charges:
/// the largest frame the voice admission accepts at the highest selectable
/// target (640 bytes per 20 ms at 256 kb/s, `nuschl_core::bitrate`) plus
/// 80 bytes per packet, at 50 packets per second.
const MICROPHONE_RESERVE_BPS: u64 = (640 + 80) * 8 * 50;
mod descriptor;
mod pacing;
mod signaling;
use pacing::{Bucket, WINDOW};
pub use signaling::{Description, Purpose, SignalingId, TrackBinding};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Video,
}

impl Kind {
    fn codec(self, video: VideoCodec) -> Codec {
        match self {
            Self::Audio => Codec::Opus,
            Self::Video => video.codec(),
        }
    }
    fn limit(self, video_limit: usize) -> usize {
        match self {
            Self::Audio => MAX_OPUS_BYTES,
            Self::Video => video_limit,
        }
    }
}

/// The codec selected for a call before negotiating its tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    Av1,
    Vp9,
    Vp8,
}

impl VideoCodec {
    fn codec(self) -> Codec {
        match self {
            Self::Av1 => Codec::Av1,
            Self::Vp9 => Codec::Vp9,
            Self::Vp8 => Codec::Vp8,
        }
    }

    fn rtpmap(self) -> &'static str {
        match self {
            Self::Av1 => "AV1/90000",
            Self::Vp9 => "VP9/90000",
            Self::Vp8 => "VP8/90000",
        }
    }
}

/// A connection-local track identifier; do not persist it as a user identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track(Mid);

impl Track {
    pub fn mid(self) -> String {
        self.0.to_string()
    }
}

struct TrackState {
    track: Track,
    purpose: Purpose,
    bucket: Bucket,
    next: Instant,
    timestamp: Option<u64>,
    /// The bytes of a call video frame longer than the window while they drain.
    oversized: usize,
    last_keyframe: Option<Instant>,
}

#[derive(Debug)]
pub struct Frame {
    pub track: Track,
    pub kind: Kind,
    pub purpose: Purpose,
    /// Extended RTP clock ticks: 48 kHz audio, 90 kHz video.
    pub timestamp: u64,
    pub contiguous: bool,
    pub data: Arc<[u8]>,
}

#[derive(Debug)]
pub enum Event {
    Connected,
    Disconnected,
    Frame(Frame),
    /// The encoder/controller must act on this; estimating bandwidth alone does
    /// not adapt a video stream. This trial has no live video encoder yet.
    BandwidthEstimate(u64),
    RequestKeyframe(Track),
}

#[derive(Debug)]
pub enum Output {
    WakeAt(Instant),
    Datagram {
        source: SocketAddr,
        destination: SocketAddr,
        data: Vec<u8>,
    },
    Event(Event),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid signaling state; this trial supports one initial offer/answer")]
    SignalingState,
    #[error("invalid, oversized, or unsupported SDP")]
    Sdp,
    #[error("invalid, mismatched, or stale track description")]
    Description,
    #[error("invalid local UDP address")]
    Address,
    #[error("direct peers are disabled by the connection policy")]
    DirectDenied,
    #[error("could not bind the ICE socket: {0}")]
    Bind(#[source] io::Error),
    #[error("invalid or oversized datagram")]
    Datagram,
    #[error("invalid track, codec, encoded frame length, or RTP timestamp")]
    Frame,
    #[error("media transport is not connected")]
    NotConnected,
    #[error("drop or defer this frame: the bandwidth or latency budget is exhausted")]
    Backpressure,
    #[error("time must advance monotonically")]
    Clock,
    #[error(transparent)]
    Rtc(#[from] str0m::RtcError),
}

enum Signaling {
    New,
    Offering {
        pending: SdpPendingOffer,
        description: Description,
        legacy: bool,
        publish_update: bool,
    },
    Negotiated,
}

/// Native WebRTC state only. No codecs, microphone handles or session passwords.
pub struct Peer {
    rtc: Rtc,
    address: SocketAddr,
    reflexive: bool,
    video: VideoCodec,
    subscription: bool,
    publishing: bool,
    signaling: Signaling,
    tracks: Vec<TrackState>,
    now: Instant,
    closed: bool,
    bandwidth_bps: u64,
    call_video_limit: Option<usize>,
    total_budget: Bucket,
    video_budget: Bucket,
}

impl Peer {
    /// Open a nonblocking direct UDP leg on a selected local interface. An
    /// ephemeral port is chosen when `address` has port zero.
    pub fn bind_direct(
        policy: PeerPolicy,
        address: SocketAddr,
        now: Instant,
        video: VideoCodec,
    ) -> Result<(Peer, UdpSocket), Error> {
        if !policy.permits(Destination::DirectPeer) {
            return Err(Error::DirectDenied);
        }
        if address.ip().is_unspecified() || address.ip().is_multicast() {
            return Err(Error::Address);
        }
        let socket = UdpSocket::bind(address).map_err(Error::Bind)?;
        socket.set_nonblocking(true).map_err(Error::Bind)?;
        let peer = Self::with_video(socket.local_addr().map_err(Error::Bind)?, now, video)?;
        Ok((peer, socket))
    }

    /// Use an explicitly bound, reachable host address. TURN and interface
    /// discovery are separate future adapters, never implicit public services.
    pub fn new(address: SocketAddr, now: Instant) -> Result<Self, Error> {
        Self::with_video(address, now, VideoCodec::Vp8)
    }

    /// Configure the call's video codec while retaining Opus for audio. Socket
    /// adapters enforce the connection policy before creating a host candidate.
    pub fn with_video(address: SocketAddr, now: Instant, video: VideoCodec) -> Result<Self, Error> {
        Self::build(address, now, video, None, false, false)
    }

    /// Video-call legs carry bounded encrypted frames and let the RTP pacer
    /// spread a keyframe across the send interval of subsequent frames.
    /// SFU publishers can enable sustained upload measurement.
    pub fn for_call(
        address: SocketAddr,
        now: Instant,
        video: VideoCodec,
        video_limit: usize,
        relayed: bool,
        sustained_upload: bool,
    ) -> Result<Self, Error> {
        Self::build(
            address,
            now,
            video,
            Some(video_limit),
            relayed,
            sustained_upload,
        )
    }

    fn build(
        address: SocketAddr,
        now: Instant,
        video: VideoCodec,
        call_video_limit: Option<usize>,
        relayed: bool,
        sustained_upload: bool,
    ) -> Result<Self, Error> {
        if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
            return Err(Error::Address);
        }
        let (initial, reorder, send, window) = if let Some(limit) = call_video_limit {
            (
                CALL_INITIAL_BITRATE,
                CALL_RTP_PACKETS,
                // Retransmissions can reach every packet of the largest frame.
                CALL_RTP_PACKETS.max(limit / 1024),
                CALL_VIDEO_WINDOW,
            )
        } else {
            (INITIAL_BITRATE, 64, 512, WINDOW)
        };
        let mut rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .clear_codecs()
            .enable_opus(true, false)
            .enable_av1(video == VideoCodec::Av1)
            .enable_vp9(video == VideoCodec::Vp9)
            .enable_vp8(video == VideoCodec::Vp8)
            .set_bwe_sustained_delivery(sustained_upload)
            .set_fingerprint_verification(true)
            .set_mtu(1200..=1200)
            .set_reordering_size_audio(8)
            .set_reordering_timeout_audio(Some(Duration::from_millis(20)))
            .set_reordering_size_video(reorder)
            .set_send_buffer_audio(32)
            .set_send_buffer_video(send)
            .enable_bwe(Some(str0m::bwe::Bitrate::bps(initial)))
            .set_extension(descriptor::ID, descriptor::extension())
            .build(now);
        if call_video_limit.is_some() {
            rtc.bwe().set_bitrate(
                str0m::bwe::Bitrate::ZERO,
                str0m::bwe::Bitrate::bps(CALL_INITIAL_PROBE_CEILING),
            );
        }
        let candidate = if relayed {
            // The broker uses the allocation address as the RTC socket identity.
            Candidate::relayed(address, address, "udp")
        } else {
            Candidate::host(address, "udp")
        };
        rtc.add_local_candidate(candidate.map_err(|_| Error::Address)?)
            .ok_or(Error::Address)?;
        Ok(Self {
            rtc,
            address,
            reflexive: false,
            video,
            subscription: false,
            publishing: false,
            signaling: Signaling::New,
            tracks: Vec::with_capacity(4),
            now,
            closed: false,
            bandwidth_bps: initial,
            call_video_limit,
            total_budget: Bucket::new(now, window),
            video_budget: Bucket::new(now, window),
        })
    }

    /// Add the mapping observed by the configured STUN service to this call's host socket.
    pub fn add_call_reflexive(&mut self, address: SocketAddr) -> Result<(), Error> {
        if self.reflexive || !matches!(self.signaling, Signaling::New) || address == self.address {
            return Err(Error::Address);
        }
        self.rtc
            .add_local_candidate(
                Candidate::server_reflexive(address, self.address, "udp")
                    .map_err(|_| Error::Address)?,
            )
            .ok_or(Error::Address)?;
        self.reflexive = true;
        Ok(())
    }

    pub fn offer(&mut self, video: bool) -> Result<String, Error> {
        let purposes = if video {
            &[Purpose::Microphone, Purpose::Camera][..]
        } else {
            &[Purpose::Microphone][..]
        };
        // Compatibility API has no semantic manifest on its wire. It is
        // deliberately restricted to the original one-audio/one-video layout.
        let description = self.make_offer(
            purposes,
            SignalingId::new([1; 16])?,
            true,
            Direction::SendRecv,
        )?;
        Ok(description.sdp)
    }

    /// Add up to two audio and two video tracks. Authenticate the returned
    /// description as one value; assigning roles from bare SDP is forbidden.
    pub fn offer_tracks(
        &mut self,
        purposes: &[Purpose],
        id: SignalingId,
    ) -> Result<Description, Error> {
        self.make_offer(purposes, id, false, Direction::SendRecv)
    }

    /// The publish leg sends the caller's tracks to its configured SFU.
    pub fn offer_publish(
        &mut self,
        purposes: &[Purpose],
        id: SignalingId,
    ) -> Result<Description, Error> {
        let offer = self.make_offer(purposes, id, false, Direction::SendOnly)?;
        self.publishing = true;
        Ok(offer)
    }

    /// Retire removed and restarted sources. The remaining MIDs keep their RTP state.
    pub fn update_publish(
        &mut self,
        purposes: &[Purpose],
        restart: &[Purpose],
        id: SignalingId,
    ) -> Result<Description, Error> {
        if !self.publishing || self.closed || !matches!(self.signaling, Signaling::Negotiated) {
            return Err(Error::SignalingState);
        }
        if purposes.len() > 4
            || purposes
                .iter()
                .enumerate()
                .any(|(i, p)| purposes[..i].contains(p))
            || restart.iter().any(|p| !purposes.contains(p))
        {
            return Err(Error::Description);
        }
        let mut api = self.rtc.sdp_api();
        let mut bindings = Vec::with_capacity(purposes.len());
        for track in &self.tracks {
            if purposes.contains(&track.purpose) && !restart.contains(&track.purpose) {
                bindings.push(TrackBinding {
                    mid: track.track.mid(),
                    purpose: track.purpose,
                });
            } else {
                api.stop_media(track.track.0);
            }
        }
        for &purpose in purposes {
            if bindings.iter().any(|binding| binding.purpose == purpose) {
                continue;
            }
            let kind = match purpose.kind() {
                Kind::Audio => MediaKind::Audio,
                Kind::Video => MediaKind::Video,
            };
            let mid = api.add_media(kind, Direction::SendOnly, None, None, None);
            bindings.push(TrackBinding {
                mid: mid.to_string(),
                purpose,
            });
        }
        let (offer, pending) = api.apply().ok_or(Error::SignalingState)?;
        let description = Description {
            id,
            sdp: offer.to_sdp_string(),
            bindings,
        };
        signaling::validate_subscription(&description.sdp, &description.bindings)?;
        self.bind_tracks(&description.bindings);
        self.signaling = Signaling::Offering {
            pending,
            description: description.clone(),
            legacy: false,
            publish_update: true,
        };
        Ok(description)
    }

    fn make_offer(
        &mut self,
        purposes: &[Purpose],
        id: SignalingId,
        legacy: bool,
        direction: Direction,
    ) -> Result<Description, Error> {
        if self.closed || !matches!(self.signaling, Signaling::New) {
            return Err(Error::SignalingState);
        }
        if purposes.is_empty()
            || purposes.len() > 4
            || purposes
                .iter()
                .enumerate()
                .any(|(i, p)| purposes[..i].contains(p))
        {
            return Err(Error::Description);
        }
        let mut api = self.rtc.sdp_api();
        let mut bindings = Vec::with_capacity(purposes.len());
        for &purpose in purposes {
            let kind = match purpose.kind() {
                Kind::Audio => MediaKind::Audio,
                Kind::Video => MediaKind::Video,
            };
            let mid = api.add_media(kind, direction, None, None, None);
            bindings.push(TrackBinding {
                mid: mid.to_string(),
                purpose,
            });
        }
        let (offer, pending) = api.apply().ok_or(Error::SignalingState)?;
        let description = Description {
            id,
            sdp: offer.to_sdp_string(),
            bindings,
        };
        signaling::validate(&description, id, self.video)?;
        self.bind_tracks(&description.bindings);
        self.signaling = Signaling::Offering {
            pending,
            description: description.clone(),
            legacy,
            publish_update: false,
        };
        Ok(description)
    }

    /// SDP fingerprints must arrive over authenticated signaling before use in a
    /// real call. Successful DTLS verifies that fingerprint, not a user identity.
    pub fn accept_offer(&mut self, sdp: &str) -> Result<String, Error> {
        let bindings = signaling::legacy_bindings(sdp, self.video)?;
        let description = Description {
            id: SignalingId::new([1; 16])?,
            sdp: sdp.to_owned(),
            bindings,
        };
        Ok(self.accept_offer_tracks(&description, description.id)?.sdp)
    }

    /// The caller MUST authenticate the entire description and compare the
    /// expected fresh call ID before invoking this API. SDP labels carry no role.
    pub fn accept_offer_tracks(
        &mut self,
        description: &Description,
        expected: SignalingId,
    ) -> Result<Description, Error> {
        if self.closed || !matches!(self.signaling, Signaling::New) {
            return Err(Error::SignalingState);
        }
        signaling::validate(description, expected, self.video)?;
        let offer = SdpOffer::from_sdp_string(&description.sdp).map_err(|_| Error::Sdp)?;
        let answer = self.rtc.sdp_api().accept_offer(offer)?;
        let answer = Description {
            id: expected,
            sdp: answer.to_sdp_string(),
            bindings: description.bindings.clone(),
        };
        if let Err(error) = signaling::validate(&answer, expected, self.video) {
            self.close();
            return Err(error);
        }
        self.bind_tracks(&answer.bindings);
        self.signaling = Signaling::Negotiated;
        Ok(answer)
    }

    pub fn accept_answer(&mut self, sdp: &str) -> Result<(), Error> {
        let Signaling::Offering {
            description,
            legacy: true,
            ..
        } = &self.signaling
        else {
            return Err(Error::SignalingState);
        };
        let bindings = signaling::legacy_bindings(sdp, self.video)?;
        let answer = Description {
            id: description.id,
            sdp: sdp.to_owned(),
            bindings,
        };
        self.accept_answer_tracks(&answer, answer.id)
    }

    /// As for `accept_offer_tracks`, authentication is the caller's obligation.
    /// The answer must preserve the offered purpose for every MID and call ID.
    pub fn accept_answer_tracks(
        &mut self,
        description: &Description,
        expected: SignalingId,
    ) -> Result<(), Error> {
        if self.closed {
            return Err(Error::SignalingState);
        }
        let Signaling::Offering {
            description: offered,
            publish_update,
            ..
        } = &self.signaling
        else {
            return Err(Error::SignalingState);
        };
        if *publish_update {
            signaling::validate_subscription(&description.sdp, &description.bindings)?;
        } else {
            signaling::validate(description, expected, self.video)?;
        }
        if description.id != expected
            || offered.id != expected
            || offered.bindings.len() != description.bindings.len()
            || !offered
                .bindings
                .iter()
                .all(|b| description.bindings.contains(b))
        {
            return Err(Error::Description);
        }
        let answer = SdpAnswer::from_sdp_string(&description.sdp).map_err(|_| Error::Sdp)?;
        let Signaling::Offering { pending, .. } =
            std::mem::replace(&mut self.signaling, Signaling::Negotiated)
        else {
            return Err(Error::SignalingState);
        };
        if let Err(error) = self.rtc.sdp_api().accept_answer(pending, answer) {
            self.close();
            return Err(error.into());
        }
        Ok(())
    }

    /// Accept an authenticated SFU subscribe offer. Retired sections and its
    /// control data channel stay available to str0m without becoming media routes.
    pub fn accept_subscription(
        &mut self,
        sdp: &str,
        bindings: &[TrackBinding],
    ) -> Result<String, Error> {
        if self.closed || (!matches!(self.signaling, Signaling::New) && !self.subscription) {
            return Err(Error::SignalingState);
        }
        signaling::validate_subscription(sdp, bindings)?;
        let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| Error::Sdp)?;
        let answer = match self.rtc.sdp_api().accept_offer(offer) {
            Ok(answer) => answer,
            Err(error) => {
                self.close();
                return Err(error.into());
            }
        };
        self.bind_tracks(bindings);
        self.subscription = true;
        self.signaling = Signaling::Negotiated;
        Ok(answer.to_sdp_string())
    }

    pub fn track(&self, kind: Kind) -> Option<Track> {
        let mut tracks = self.tracks.iter().filter(|t| t.purpose.kind() == kind);
        let track = tracks.next()?.track;
        tracks.next().is_none().then_some(track)
    }

    pub fn track_for(&self, purpose: Purpose) -> Option<Track> {
        self.tracks
            .iter()
            .find(|t| t.purpose == purpose)
            .map(|t| t.track)
    }

    pub fn track_by_mid(&self, mid: &str) -> Option<Track> {
        let track = Track(Mid::from(mid));
        self.tracks
            .iter()
            .any(|state| state.track == track)
            .then_some(track)
    }

    pub fn purpose(&self, track: Track) -> Option<Purpose> {
        self.tracks
            .iter()
            .find(|t| t.track == track)
            .map(|t| t.purpose)
    }

    fn bind_tracks(&mut self, bindings: &[TrackBinding]) {
        let mut previous = std::mem::take(&mut self.tracks);
        self.tracks = bindings
            .iter()
            .map(|b| {
                let track = Track(Mid::from(b.mid.as_str()));
                if let Some(index) = previous
                    .iter()
                    .position(|old| old.track == track && old.purpose == b.purpose)
                {
                    return previous.swap_remove(index);
                }
                TrackState {
                    track,
                    purpose: b.purpose,
                    bucket: Bucket::new(
                        self.now,
                        if self.call_video_limit.is_some() && b.purpose.kind() == Kind::Video {
                            CALL_VIDEO_WINDOW
                        } else {
                            WINDOW
                        },
                    ),
                    next: self.now,
                    timestamp: None,
                    oversized: 0,
                    last_keyframe: None,
                }
            })
            .collect();
    }

    /// Request a keyframe for this video MID only; audio is never eligible.
    pub fn request_keyframe(&mut self, track: Track) -> Result<(), Error> {
        if !self.is_connected() {
            return Err(Error::NotConnected);
        }
        if self.purpose(track).is_none_or(|p| p.kind() != Kind::Video) {
            return Err(Error::Frame);
        }
        self.rtc
            .writer(track.0)
            .ok_or(Error::Frame)?
            .request_keyframe(None, KeyframeRequestKind::Pli)?;
        Ok(())
    }

    /// Drop queued RTP and retransmissions while keeping the negotiated transport warm.
    pub fn clear_call_send(&mut self) {
        for state in &self.tracks {
            let mid = state.track.0;
            let Some(direction) = self.rtc.media(mid).map(|media| media.direction()) else {
                continue;
            };
            if direction.is_sending() {
                let mut sdp = self.rtc.sdp_api();
                // Changing the local direction clears str0m's payloader and transmit
                // buffers. Restore the negotiated direction in the same operation.
                sdp.set_direction(
                    mid,
                    if direction.is_receiving() {
                        Direction::RecvOnly
                    } else {
                        Direction::Inactive
                    },
                );
                sdp.set_direction(mid, direction);
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        !self.closed && self.rtc.is_connected()
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.rtc.disconnect();
    }

    /// Submit an already encoded frame. Audio uses a 48 kHz clock, video 90 kHz.
    /// `descriptor` is a video frame's AV1 Dependency Descriptor, or empty.
    /// Timestamps increase independently per track. The caller bounds its capture
    /// queue and drops stale video on backpressure; a failed write consumes no
    /// frame budget or timestamp. Both audio roles have dedicated allocations,
    /// videos get equal shares, and the total admission budget follows BWE with
    /// transport headroom. An encoder must still adapt its rate/size to feedback.
    pub fn write(
        &mut self,
        track: Track,
        timestamp: u64,
        now: Instant,
        data: Arc<[u8]>,
        descriptor: &[u8],
    ) -> Result<(), Error> {
        self.write_frame(track, timestamp, now, data, descriptor, false)
    }

    /// Submit a call keyframe. The RTP pacer can send it using video credit
    /// that accrues within the queue's latency bound.
    pub fn write_keyframe(
        &mut self,
        track: Track,
        timestamp: u64,
        now: Instant,
        data: Arc<[u8]>,
        descriptor: &[u8],
    ) -> Result<(), Error> {
        self.write_frame(track, timestamp, now, data, descriptor, true)
    }

    fn write_frame(
        &mut self,
        track: Track,
        timestamp: u64,
        now: Instant,
        data: Arc<[u8]>,
        descriptor: &[u8],
        keyframe: bool,
    ) -> Result<(), Error> {
        if !self.is_connected() {
            return Err(Error::NotConnected);
        }
        let index = self
            .tracks
            .iter()
            .position(|t| t.track == track)
            .ok_or(Error::Frame)?;
        let kind = self.tracks[index].purpose.kind();
        if data.is_empty()
            || data.len() > self.frame_limit(kind)
            || descriptor.len() > usize::from(u8::MAX)
            || ((!descriptor.is_empty() || keyframe) && kind != Kind::Video)
            || self.tracks[index]
                .timestamp
                .is_some_and(|last| timestamp <= last)
        {
            return Err(Error::Frame);
        }
        self.clock(now)?;
        let (total, video, rate) = self.rates(self.tracks[index].purpose);
        // Include a conservative packet overhead estimate, on top of the 20%
        // transport headroom. Small audio packets must not bypass the budget.
        let bits = ((data.len() + data.len().div_ceil(1000) * 80) * 8) as f64;
        let call = self.call_video_limit.is_some();
        let state = &self.tracks[index];
        let window = rate as f64 * CALL_VIDEO_WINDOW.as_secs_f64();
        let recent_keyframe = state
            .last_keyframe
            .is_some_and(|at| now - at < CALL_VIDEO_WINDOW);
        let recovering = keyframe || recent_keyframe;
        // A call partitions its total allocation into video and audio shares.
        // Recovery and its dependent frames can use future video credit while
        // the pacer drains the bounded RTP queue.
        let funded = (call || self.total_budget.admits(now, total, bits))
            && state.bucket.admits(now, rate, bits)
            && (kind != Kind::Video || self.video_budget.admits(now, video, bits));
        let paced = call
            && kind == Kind::Video
            && !funded
            && bits <= window
            && recovering
            && state.bucket.admits_paced(now, rate, bits)
            && self.video_budget.admits_paced(now, video, bits);
        if rate == 0 || now < state.next || (!funded && !paced) {
            return Err(Error::Backpressure);
        }
        // Audio frames arrive on the encoder's cadence, and constrained VBR
        // varies their sizes around the target: the role's bucket bounds the
        // rate and the burst (WINDOW), and the floor bounds the packet rate.
        // Spacing each frame by its own size would refuse on-time frames.
        let interval = if kind == Kind::Audio {
            Duration::from_micros(2500)
        } else if call {
            let queued = self
                .rtc
                .direct_api()
                .stream_tx_by_mid(track.0, None)
                .and_then(|stream| {
                    stream
                        .queue_info()
                        .map(|q| (q.byte_size(), q.first_unsent()))
                });
            // A frame longer than the window, such as a large key frame, goes
            // out when the queue is empty; the frames behind it may queue for
            // as long as it takes, up to its size more.
            let state = &mut self.tracks[index];
            let (bytes, oldest) = queued.unwrap_or((0, None));
            if (bytes * 8) as f64 <= window {
                state.oversized = 0;
            }
            // Recovery uses the remaining bytes to bound the wait. An old
            // packet can linger in the queue while awaiting retransmission.
            let late = !recovering
                && oldest.is_some_and(|at| now.saturating_duration_since(at) >= CALL_VIDEO_WINDOW);
            let fits = bits + (bytes * 8) as f64 <= window + (state.oversized * 8) as f64;
            if bytes == 0 && bits > window {
                state.oversized = data.len();
            } else if !fits || (late && state.oversized == 0) {
                return Err(Error::Backpressure);
            }
            Duration::from_millis(1)
        } else {
            Duration::from_secs_f64(bits / rate as f64).max(Duration::from_millis(1))
        };
        if interval > WINDOW {
            return Err(Error::Backpressure);
        }
        let mut writer = self.rtc.writer(track.0).ok_or(Error::Frame)?;
        if !descriptor.is_empty() {
            writer = writer.user_extension_value(descriptor::Descriptor(Arc::from(descriptor)));
        }
        let params = *writer
            .payload_params()
            .find(|p| p.spec().codec == kind.codec(self.video))
            .ok_or(Error::Frame)?;
        writer.write(
            params.pt(),
            now,
            MediaTime::new(timestamp, params.spec().clock_rate),
            data,
        )?;
        if !call {
            self.total_budget.spend(now, total, bits, false);
        }
        if kind == Kind::Video {
            self.video_budget.spend(now, video, bits, paced);
        }
        let state = &mut self.tracks[index];
        state.bucket.spend(now, rate, bits, paced);
        state.next = now + interval;
        state.timestamp = Some(timestamp);
        if keyframe {
            state.last_keyframe = Some(now);
        }
        Ok(())
    }

    fn rates(&self, purpose: Purpose) -> (u64, u64, u64) {
        let maximum = if self.call_video_limit.is_some() {
            CALL_MAX_BITRATE
        } else {
            10_000_000
        };
        let total = self.bandwidth_bps.min(maximum) * 4 / 5;
        let microphone = if self.track_for(Purpose::Microphone).is_some() {
            MICROPHONE_RESERVE_BPS
        } else {
            0
        };
        let system = if self.track_for(Purpose::SystemAudio).is_some() {
            256_000
        } else {
            0
        };
        let audio = microphone + system;
        // Under severe congestion, both audio roles retain proportional shares;
        // video is shut off rather than borrowing either audio allocation.
        let video = total.saturating_sub(audio);
        let role_rate = match purpose {
            Purpose::Microphone => microphone * total.min(audio) / audio.max(1),
            Purpose::SystemAudio => system * total.min(audio) / audio.max(1),
            Purpose::Camera if self.track_for(Purpose::ScreenVideo).is_some() => {
                camera_share(video)
            }
            Purpose::ScreenVideo if self.track_for(Purpose::Camera).is_some() => {
                video - camera_share(video)
            }
            Purpose::Camera | Purpose::ScreenVideo => video,
        };
        (total, video, role_rate)
    }

    fn frame_limit(&self, kind: Kind) -> usize {
        kind.limit(self.call_video_limit.unwrap_or(MAX_VIDEO_BYTES))
    }

    /// Malformed datagrams return an error; a socket adapter may discard them
    /// rather than disconnect an otherwise authenticated peer.
    pub fn receive(&mut self, source: SocketAddr, now: Instant, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() || data.len() > MAX_DATAGRAM_BYTES {
            return Err(Error::Datagram);
        }
        let contents = data.try_into().map_err(|_| Error::Datagram)?;
        self.clock(now)?;
        self.rtc.handle_input(Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination: self.address,
                contents,
            },
        ))?;
        Ok(())
    }

    pub fn timeout(&mut self, now: Instant) -> Result<(), Error> {
        self.clock(now)?;
        self.rtc.handle_input(Input::Timeout(now))?;
        Ok(())
    }

    /// Returns one action at a time, avoiding an unbounded application queue.
    pub fn poll(&mut self) -> Result<Output, Error> {
        // Bound ignored events per call so a busy peer cannot starve its siblings.
        for _ in 0..64 {
            match self.rtc.poll_output()? {
                RtcOutput::Timeout(at) => return Ok(Output::WakeAt(at)),
                RtcOutput::Transmit(v) => {
                    return Ok(Output::Datagram {
                        source: v.source,
                        destination: v.destination,
                        data: v.contents.to_vec(),
                    });
                }
                RtcOutput::Event(event) => match event {
                    RtcEvent::Connected => return Ok(Output::Event(Event::Connected)),
                    RtcEvent::Closed => {
                        self.closed = true;
                        return Ok(Output::Event(Event::Disconnected));
                    }
                    RtcEvent::MediaAdded(media) => {
                        let kind = if media.kind == MediaKind::Audio {
                            Kind::Audio
                        } else {
                            Kind::Video
                        };
                        if self.purpose(Track(media.mid)).map(Purpose::kind) != Some(kind) {
                            if self.subscription {
                                continue;
                            }
                            self.close();
                            return Err(Error::Frame);
                        }
                    }
                    RtcEvent::MediaData(media) => {
                        let Some(purpose) = self.purpose(Track(media.mid)) else {
                            if self.subscription {
                                continue;
                            }
                            return Err(Error::Frame);
                        };
                        let kind = purpose.kind();
                        if media.params.spec().codec != kind.codec(self.video)
                            || media.data.is_empty()
                            || media.data.len() > self.frame_limit(kind)
                        {
                            if self.subscription {
                                continue;
                            }
                            return Err(Error::Frame);
                        }
                        return Ok(Output::Event(Event::Frame(Frame {
                            track: Track(media.mid),
                            kind,
                            purpose,
                            timestamp: media.time.numer(),
                            contiguous: media.contiguous,
                            data: media.data,
                        })));
                    }
                    RtcEvent::EgressBitrateEstimate(estimate) => {
                        let rate = match estimate {
                            str0m::bwe::BweKind::Twcc {
                                estimate,
                                can_probe,
                            } if can_probe || self.call_video_limit.is_none() => {
                                let rate = estimate.as_u64();
                                // str0m publishes its configured seed as soon as
                                // the path supports probing. Report changes from it.
                                if self.call_video_limit.is_some()
                                    && self.bandwidth_bps == CALL_INITIAL_BITRATE
                                    && rate == CALL_INITIAL_BITRATE
                                {
                                    continue;
                                }
                                rate
                            }
                            str0m::bwe::BweKind::Remb { estimate, .. } => estimate.as_u64(),
                            _ => continue,
                        };
                        // Only raise the probe target after the path has shown
                        // capacity for the next range.
                        let probe_upgrade = CALL_INITIAL_PROBE_CEILING * 4 / 5;
                        if self.call_video_limit.is_some()
                            && self.bandwidth_bps < probe_upgrade
                            && rate >= probe_upgrade
                        {
                            self.rtc.bwe().set_bitrate(
                                str0m::bwe::Bitrate::ZERO,
                                str0m::bwe::Bitrate::bps(CALL_MAX_BITRATE),
                            );
                        }
                        self.bandwidth_bps = rate;
                        return Ok(Output::Event(Event::BandwidthEstimate(rate)));
                    }
                    RtcEvent::KeyframeRequest(request) => {
                        let track = Track(request.mid);
                        match self.purpose(track) {
                            Some(purpose) if purpose.kind() == Kind::Video => {
                                return Ok(Output::Event(Event::RequestKeyframe(track)));
                            }
                            None if self.publishing => continue,
                            _ => return Err(Error::Frame),
                        }
                    }
                    _ => {}
                },
            }
        }
        Ok(Output::WakeAt(self.now))
    }

    fn clock(&mut self, now: Instant) -> Result<(), Error> {
        if now < self.now {
            return Err(Error::Clock);
        }
        self.now = now;
        Ok(())
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn a_camera_leaves_the_remaining_rate_for_a_screen() {
        assert_eq!(camera_share(2_000_000), 1_000_000);
        let total = CALL_MAX_BITRATE * 4 / 5;
        assert!(total - camera_share(total) >= 100_000_000);
    }
}
