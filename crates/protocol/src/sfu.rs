// SPDX-License-Identifier: EUPL-1.2
//! Signalling: one JSON object per WebSocket text message, tagged by `type`.
//! Binary values are lowercase hex.
//!
//! Each peer has up to two WebRTC connections. On its publish connection the
//! peer offers and the SFU answers (`offer` → `answer`); on its subscribe
//! connection the SFU offers and the peer answers. Each side is always the
//! offerer on its own connection, so offers never cross. The subscribe
//! connection also carries an unused data channel, `sfu`, which keeps its
//! transport up while no source is forwarded.
//!
//! ```text
//! SFU → peer  hello {nonce, video_limits?, screen_rate?}
//! peer → SFU  join {room, verifier, proof, host_proof?, create?: {admit_key, host_key}, codecs, video_limits?, screen_rate?}
//! SFU → peer  welcome {peer, role, peers: [{id, role}], codec, turn?: {urls, username, credential}}
//!             | error {reason}
//! peer → SFU  offer {sdp, screens?: [mid]} publish connection; screens names
//!                                          the video mids that carry a screen
//! peer → SFU  modes {modes: {mid: mode}}   each video mid's mode, replacing the last
//! SFU → peer  answer {sdp}
//! SFU → peer  offer {sdp, tracks: [{mid, peer, source, kind}]}   subscribe connection
//! peer → SFU  answer {sdp}
//! peer → SFU  key {to, data}               relayed as key {from, data}
//! peer → SFU  quality {request}            hex QualityRequest, per source, ≤ 1/s
//! peer → SFU  capacity {decode}            hex DecodeCapacity, ≤ 1/s
//! SFU → peer  quality {request, source, max_height, max_fps, max_bitrate, limit?}
//!                                          encoder limits for the publisher's mid
//! SFU → peer  video_limit {mid, peer, limit} per-source limit for the receiver
//! SFU → peer  mode {mid, peer, mode}       a forwarded video source's mode, ≤ 1/s
//! peer → SFU  remove {peer} | end          hosts only
//! SFU → peer  joined {peer, role} | left {peer}
//! SFU → peer  codec {codec}
//! ```
//!
//! A later publish offer that stops or deactivates a mid ends that source:
//! its tracks leave the subscribers' next offers.
//! Peers that want video limit reports echo `video_limits: true` when the
//! server advertises it. The `video_limit` mid belongs to the receiver's
//! subscribe connection. The `quality` mid belongs to the publish connection.
//!
//! **Modes.** A mode is `{prefer: "sharp" | "smooth", low_latency?: true}`.
//! Sharp keeps a source's resolution and lowers its frame rate when bandwidth
//! or CPU is short; Smooth keeps the frame rate and lowers the resolution. Low
//! latency bounds each viewer's queue at the SFU to about 50 ms, and receivers
//! render its frames at once. A video mid without a mode is Sharp for a screen
//! and Smooth for a camera. The SFU sends each receiver the mode of each video
//! source it forwards, again when the publisher changes it.
//!
//! **One codec per call** (docs/video-plan.md §6). `codecs` lists the video
//! codecs the peer decodes in WebRTC, `"av1"` and `"vp9"`, at least one. The
//! call codec is AV1 while every peer decodes it, otherwise VP9; a join that
//! leaves no codec every peer decodes fails with `error {reason: "codec"}`.
//! `welcome` carries the call codec and `codec` each change after a join or a
//! leave; publishers encode in it.
//!
//! **Keys and proofs.** S and P are the link's two 32-byte secrets (`crates/stream`):
//!
//! ```text
//! room       = HKDF-Expand(HKDF-Extract("nuschl/stream/v2", S), "nuschl/stream/v2 stream id", 16)
//! admit_key  = HKDF-Expand(HKDF-Extract("nuschl/stream/v2", S), "nuschl/stream/v2 admit", 32)
//! host_key   = HKDF-Expand(HKDF-Extract("nuschl/stream/v2", P), "nuschl/stream/v2 host", 32)
//! verifier   = SHA-256(host_key)                                      // V
//! proof      = HMAC-SHA256(admit_key, "nuschl/stream/v2 sfu admit" || nonce)
//! host_proof = HMAC-SHA256(host_key,  "nuschl/stream/v2 sfu host"  || nonce)
//! ```
//!
//! HKDF-Extract's first argument is the salt; `nonce` is the 32 bytes of `hello`.
//! `crates/stream` provides these as `stream_id`, `admit_key`, `host_key` and
//! `room_check` (V). Every link carries V, in the same unpadded base64url as S
//! and P: `/call#v=1&k=<S>&h=<V>`, and the creator's link adds `&p=<P>`. A room is named
//! by `(room, verifier)`. `create` succeeds only with a `host_key` that hashes
//! to `verifier`, so a holder of S alone cannot create the room a link names.
//!
//! ```text
//! P        = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
//! host_key = 714cce84239301bcc3f4fcaaf0d59ca0d242f7b0bc9cf276ab9a3568bf9a873f
//! V        = 078384c474855fcaa97ecbb155d67f03add025efa0c126d8a23c30e71d6c4c37
//! h=         B4OExHSFX8qpfsuxVdZ_A63QJe-gwSbYojww5x1sTDc
//! ```
//!
//! **Lease.** The creator's device keeps the room alive over plain HTTP on the
//! same origin; P stays on the device, the SFU receives `host_key`:
//!
//! ```text
//! POST   /sfu/lease  {"stream_id": "<hex>", "host_key": "<hex>"}   renew, every 10 s
//! DELETE /sfu/lease  {"stream_id": "<hex>", "host_key": "<hex>"}   end the room
//! → 204 done | 400 malformed | 404 no room for this stream_id and SHA-256(host_key)
//!   | 429 renewed less than a second ago, or too many connections from the address
//! ```
//!
//! The room `(stream_id, SHA-256(host_key))` ends 30 s (`lease_secs`) after its
//! creation or its last renewal; ending disconnects every peer with `error
//! {reason: "ended"}` and refuses joins until the room expires. A body is at
//! most 1 KiB with a `Content-Length`, a request head at most 8 KiB, and each
//! request uses one connection of the per-address limit.

use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap, fmt};

pub type PeerId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Proved S: publishes its own sources, subscribes, exchanges key messages.
    Participant,
    /// Also proved P: removes peers and ends the room.
    Host,
}

/// Standard maximum for one WebSocket signaling message.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// The largest key message (docs/video-plan.md §4), before hex encoding. Key
/// messages are `nuschl-stream`'s envelopes between a Publisher and its
/// Subscribers (a key request is 1,272 bytes); the SFU relays them unread.
pub const MAX_KEY_MESSAGE: usize = 16 * 1024;

/// Maximum bounded encoder target for a negotiated screen rate, in bits per second.
pub const MAX_SCREEN_BITRATE: u64 = 50_000_000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Join {
        room: String,
        verifier: String,
        proof: String,
        #[serde(default)]
        host_proof: Option<String>,
        #[serde(default)]
        create: Option<Create>,
        codecs: Vec<Codec>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        video_limits: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        screen_rate: bool,
    },
    Offer {
        sdp: String,
        #[serde(default)]
        screens: Vec<String>,
    },
    Answer {
        sdp: String,
    },
    Key {
        to: PeerId,
        data: String,
    },
    Modes {
        modes: BTreeMap<String, Mode>,
    },
    Quality {
        request: String,
    },
    Capacity {
        decode: String,
    },
    Remove {
        peer: PeerId,
    },
    End,
}

/// A video codec a call can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Av1,
    Vp9,
}

/// The video codecs a peer decodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decoders {
    pub av1: bool,
    pub vp9: bool,
}

impl Decoders {
    /// `None` for an empty list.
    pub fn new(codecs: &[Codec]) -> Option<Self> {
        let decoders = Self {
            av1: codecs.contains(&Codec::Av1),
            vp9: codecs.contains(&Codec::Vp9),
        };
        (decoders != Self::default()).then_some(decoders)
    }
}

/// The call codec of peers that decode `peers`: AV1 when all of them do, else
/// VP9 when all of them do.
pub fn call_codec(peers: impl IntoIterator<Item = Decoders>) -> Option<Codec> {
    let (av1, vp9) = peers
        .into_iter()
        .fold((true, true), |(av1, vp9), d| (av1 && d.av1, vp9 && d.vp9));
    match (av1, vp9) {
        (true, _) => Some(Codec::Av1),
        (false, true) => Some(Codec::Vp9),
        (false, false) => None,
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub admit_key: String,
    pub host_key: String,
}

impl fmt::Debug for Create {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Create").finish_non_exhaustive()
    }
}

/// What gives way when bandwidth or CPU is short.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Prefer {
    /// Keeps the resolution and lowers the frame rate.
    Sharp,
    /// Keeps the frame rate and lowers the resolution.
    Smooth,
}

/// A video source's Sharp/Smooth and Low latency settings, chosen by its sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    pub prefer: Prefer,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub low_latency: bool,
}

impl Mode {
    /// The mode of a video source whose sender chose none: Sharp for a screen,
    /// Smooth for a camera.
    pub fn default_for(screen: bool) -> Self {
        Self {
            prefer: if screen {
                Prefer::Sharp
            } else {
                Prefer::Smooth
            },
            low_latency: false,
        }
    }
}

/// The quality constraint picked by the SFU for a publisher or a receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoLimit {
    WithinTarget,
    ReceiverQuality,
    ReceiverDecode,
    ReceiverBandwidth,
    SfuHost,
    SfuRoom,
    SfuViewer,
    TrafficBudget,
    ForwardingBacklog,
    QualityRecovery,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        nonce: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        video_limits: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        screen_rate: bool,
    },
    Welcome(Welcome),
    Joined {
        peer: PeerId,
        role: Role,
    },
    Left {
        peer: PeerId,
    },
    Codec {
        codec: Codec,
    },
    Offer {
        sdp: String,
        tracks: Vec<Track>,
    },
    Answer {
        sdp: String,
    },
    Key {
        from: PeerId,
        data: String,
    },
    Quality {
        request: String,
        source: String,
        max_height: u16,
        max_fps: u8,
        max_bitrate: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<VideoLimit>,
    },
    VideoLimit {
        mid: String,
        peer: PeerId,
        limit: VideoLimit,
    },
    Mode {
        mid: String,
        peer: PeerId,
        mode: Mode,
    },
    Error {
        reason: Cow<'static, str>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Welcome {
    pub peer: PeerId,
    pub role: Role,
    pub peers: Vec<PeerInfo>,
    pub codec: Codec,
    pub turn: Option<TurnCredentials>,
}

impl ServerMessage {
    pub fn error(reason: &'static str) -> Self {
        Self::Error {
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeerInfo {
    pub id: PeerId,
    pub role: Role,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct TurnCredentials {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl fmt::Debug for TurnCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnCredentials").finish_non_exhaustive()
    }
}

/// A forwarded source: `mid` on the receiver's subscribe connection carries
/// the source with `source` as its mid on the publisher's connection.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Track {
    pub mid: String,
    pub peer: PeerId,
    pub source: String,
    /// `camera`, `screen` or `audio`.
    pub kind: Cow<'static, str>,
}

pub fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    hex::decode_to_slice(text, &mut bytes).ok()?;
    Some(bytes)
}

/// A key message is relayed as it came: hex of at most [`MAX_KEY_MESSAGE`] bytes.
pub fn valid_key_message(data: &str) -> bool {
    !data.is_empty()
        && data.len() <= 2 * MAX_KEY_MESSAGE
        && data.len().is_multiple_of(2)
        && data.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_messages() {
        let quality: ClientMessage =
            serde_json::from_str(r#"{"type":"quality","request":"0100"}"#).expect("quality");
        assert!(matches!(quality, ClientMessage::Quality { request } if request == "0100"));
        let offer: ClientMessage =
            serde_json::from_str(r#"{"type":"offer","sdp":"","screens":["1"]}"#).expect("offer");
        assert!(matches!(offer, ClientMessage::Offer { screens, .. } if screens == ["1"]));
        let modes: ClientMessage = serde_json::from_str(
            r#"{"type":"modes","modes":{"1":{"prefer":"smooth","low_latency":true},"2":{"prefer":"sharp"}}}"#,
        )
        .expect("modes");
        let ClientMessage::Modes { modes } = modes else {
            panic!("modes");
        };
        assert_eq!(
            modes["1"],
            Mode {
                prefer: Prefer::Smooth,
                low_latency: true
            }
        );
        assert_eq!(modes["2"], Mode::default_for(true));
        let mode = ServerMessage::Mode {
            mid: "3".into(),
            peer: 4,
            mode: Mode::default_for(false),
        };
        assert_eq!(
            serde_json::to_value(mode).expect("json"),
            serde_json::json!({"type": "mode", "mid": "3", "peer": 4, "mode": {"prefer": "smooth"}})
        );
        let demand = ServerMessage::Quality {
            request: "01".into(),
            source: "1".into(),
            max_height: 720,
            max_fps: 15,
            max_bitrate: 800_000,
            limit: None,
        };
        assert_eq!(
            serde_json::to_value(demand).expect("json"),
            serde_json::json!({
                "type": "quality", "request": "01", "source": "1",
                "max_height": 720, "max_fps": 15, "max_bitrate": 800_000,
            })
        );
    }

    #[test]
    fn negotiates_a_bounded_video_limit_report() {
        let hello: ServerMessage =
            serde_json::from_str(r#"{"type":"hello","nonce":"00"}"#).expect("hello");
        assert!(matches!(
            hello,
            ServerMessage::Hello {
                video_limits: false,
                screen_rate: false,
                ..
            }
        ));
        let report = serde_json::json!({
            "type": "video_limit", "mid": "1", "peer": 2, "limit": "sfu_viewer",
        });
        let message: ServerMessage = serde_json::from_value(report.clone()).expect("report");
        assert!(matches!(
            message,
            ServerMessage::VideoLimit {
                limit: VideoLimit::SfuViewer,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(message).expect("json"), report);
        assert!(
            serde_json::from_value::<ServerMessage>(serde_json::json!({
                "type": "video_limit", "mid": "1", "peer": 2, "limit": "unknown",
            }))
            .is_err()
        );
    }

    #[test]
    fn wire_format() {
        let join: ClientMessage = serde_json::from_str(
            r#"{"type":"join","room":"00","verifier":"44","proof":"11","create":{"admit_key":"22","host_key":"33"},"codecs":["vp9","av1"]}"#,
        )
        .expect("join");
        assert!(matches!(
            join,
            ClientMessage::Join {
                host_proof: None,
                create: Some(_),
                video_limits: false,
                screen_rate: false,
                ref codecs,
                ..
            } if codecs == &[Codec::Vp9, Codec::Av1]
        ));
        assert!(
            serde_json::from_str::<ClientMessage>(
                r#"{"type":"join","room":"00","verifier":"44","proof":"11","codecs":["h264"]}"#
            )
            .is_err()
        );
        assert!(matches!(
            serde_json::from_str(r#"{"type":"end"}"#).expect("end"),
            ClientMessage::End
        ));
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"bogus"}"#).is_err());
        let welcome = serde_json::to_string(&ServerMessage::Welcome(Welcome {
            peer: 3,
            role: Role::Host,
            peers: vec![PeerInfo {
                id: 1,
                role: Role::Participant,
            }],
            codec: Codec::Vp9,
            turn: None,
        }))
        .expect("json");
        assert_eq!(
            welcome,
            r#"{"type":"welcome","peer":3,"role":"host","peers":[{"id":1,"role":"participant"}],"codec":"vp9","turn":null}"#
        );
    }

    #[test]
    fn one_codec_per_call() {
        let decoders = |codecs: &[Codec]| Decoders::new(codecs).expect("a codec");
        let both = decoders(&[Codec::Av1, Codec::Vp9]);
        let vp9 = decoders(&[Codec::Vp9]);
        let av1 = decoders(&[Codec::Av1]);
        assert_eq!(Decoders::new(&[]), None);
        assert_eq!(call_codec([both, both]), Some(Codec::Av1));
        assert_eq!(call_codec([both, vp9]), Some(Codec::Vp9));
        assert_eq!(call_codec([av1, both]), Some(Codec::Av1));
        assert_eq!(call_codec([av1, vp9]), None);
        assert_eq!(call_codec([vp9]), Some(Codec::Vp9));
    }

    #[test]
    fn hex_and_key_bounds() {
        assert_eq!(from_hex::<2>("0aff"), Some([0x0a, 0xff]));
        assert_eq!(from_hex::<2>("0aff00"), None);
        assert_eq!(from_hex::<2>("zz00"), None);
        assert!(valid_key_message(&"ab".repeat(MAX_KEY_MESSAGE)));
        assert!(!valid_key_message(&"ab".repeat(MAX_KEY_MESSAGE + 1)));
        assert!(!valid_key_message("abc"));
        assert!(!valid_key_message("xy"));
        assert!(!valid_key_message(""));
    }
}
