// SPDX-License-Identifier: EUPL-1.2
//! RTC worker commands and replies. The bounded worker envelope precedes the
//! optional media bytes. Epoch keys use the separate crypto-worker socket.

mod supervisor;
pub use supervisor::{Error as WorkerError, Worker};

use crate::{Envelope, Error, read_frame_async, read_frame_within, write_frame_within};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    net::SocketAddr,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

pub const MAX_SDP: usize = 32 * 1024;
pub const MAX_DATAGRAM: usize = 2048;
pub const MAX_VIDEO: usize = crate::video::SCREEN_FRAME_BYTES;
const MAX_METADATA: usize = MAX_SDP * 2 + 8192;
pub const MAX_TRACKS: usize = 48;

// The publish leg may first open after a direct receive leg has been used.
pub const SFU_SUBSCRIBE_LEG: u32 = 1;
pub const SFU_PUBLISH_LEG: u32 = 2;
pub const FIRST_DIRECT_LEG: u32 = 3;
/// A publisher sends each frame on the SFU leg or on up to this many direct legs.
pub const MAX_DIRECT_SEND_LEGS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Microphone,
    SystemAudio,
    Camera,
    ScreenVideo,
}

impl Purpose {
    pub fn frame_limit(self) -> usize {
        match self {
            Self::Microphone | Self::SystemAudio => 1275,
            Self::Camera | Self::ScreenVideo => MAX_VIDEO,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Av1,
    Vp9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegRole {
    Direct,
    SfuPublish,
    SfuSubscribe,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    pub id: [u8; 16],
    pub sdp: String,
    pub tracks: Vec<Track>,
}

impl Description {
    fn valid(&self) -> bool {
        self.id != [0; 16]
            && !self.sdp.is_empty()
            && self.sdp.len() <= MAX_SDP
            && self.tracks.len() <= MAX_TRACKS
            && self.tracks.iter().all(|track| track.valid())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    pub mid: String,
    pub purpose: Purpose,
    pub publisher: Option<u64>,
}

impl Track {
    fn valid(&self) -> bool {
        (1..=16).contains(&self.mid.len())
            && self
                .mid
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            && self.publisher != Some(0)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Start {
        generation: u64,
    },
    OpenLeg {
        leg: u32,
        local: SocketAddr,
        codec: Codec,
        role: LegRole,
        relayed: bool,
    },
    CloseLeg {
        leg: u32,
    },
    AddReflexive {
        leg: u32,
        address: SocketAddr,
    },
    Offer {
        leg: u32,
        id: [u8; 16],
        purposes: Vec<Purpose>,
    },
    UpdatePublish {
        leg: u32,
        id: [u8; 16],
        purposes: Vec<Purpose>,
        restart: Vec<Purpose>,
    },
    AcceptOffer {
        leg: u32,
        description: Description,
    },
    AcceptAnswer {
        leg: u32,
        description: Description,
    },
    Datagram {
        leg: u32,
        source: SocketAddr,
    },
    Poll {
        leg: u32,
    },
    /// Move media in one step to these legs: the SFU publish leg alone, or
    /// direct legs.
    SetSendLegs {
        legs: Vec<u32>,
    },
    Write {
        purpose: Purpose,
        timestamp: u64,
        key_frame: bool,
        /// The video frame's AV1 Dependency Descriptor for its RTP packets;
        /// empty for none.
        #[serde(with = "serde_bytes")]
        descriptor: Vec<u8>,
    },
    RequestKeyframe {
        leg: u32,
        publisher: u64,
        purpose: Purpose,
    },
}

impl Command {
    fn permits(&self, length: usize) -> bool {
        if length != 0 {
            return match self {
                Self::Datagram { leg, source } => {
                    *leg != 0 && endpoint(*source) && length <= MAX_DATAGRAM
                }
                Self::Write {
                    purpose,
                    descriptor,
                    ..
                } => {
                    length <= purpose.frame_limit()
                        && descriptor.len() <= crate::video::MAX_DESCRIPTOR
                        && (descriptor.is_empty()
                            || matches!(purpose, Purpose::Camera | Purpose::ScreenVideo))
                }
                _ => false,
            };
        }
        match self {
            Self::Start { generation } => *generation != 0,
            Self::OpenLeg { leg, local, .. } => *leg != 0 && endpoint(*local),
            Self::AddReflexive { leg, address } => *leg != 0 && endpoint(*address),
            Self::Offer { leg, id, purposes } => {
                *leg != 0 && *id != [0; 16] && (1..=4).contains(&purposes.len())
            }
            Self::UpdatePublish {
                leg,
                id,
                purposes,
                restart,
            } => {
                *leg != 0
                    && *id != [0; 16]
                    && purposes.len() <= 4
                    && restart.len() <= 4
                    && restart.iter().all(|purpose| purposes.contains(purpose))
            }
            Self::AcceptOffer { leg, description } | Self::AcceptAnswer { leg, description } => {
                *leg != 0 && description.valid()
            }
            Self::CloseLeg { leg } | Self::Poll { leg } => *leg != 0,
            Self::SetSendLegs { legs } => {
                (1..=MAX_DIRECT_SEND_LEGS).contains(&legs.len()) && !legs.contains(&0)
            }
            Self::RequestKeyframe { leg, publisher, .. } => *leg != 0 && *publisher != 0,
            Self::Datagram { .. } | Self::Write { .. } => false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Done,
    Rejected,
    Description { leg: u32, description: Description },
    Wake { leg: u32, after_millis: u64 },
    Datagram { leg: u32, destination: SocketAddr },
    Event { leg: u32, event: Event },
}

impl Reply {
    fn permits(&self, length: usize) -> bool {
        match self {
            Self::Done | Self::Rejected => length == 0,
            Self::Description { leg, description } => {
                *leg != 0 && description.valid() && length == 0
            }
            Self::Wake { leg, after_millis } => *leg != 0 && *after_millis <= 60_000 && length == 0,
            Self::Datagram { leg, destination } => {
                *leg != 0 && endpoint(*destination) && (1..=MAX_DATAGRAM).contains(&length)
            }
            Self::Event { leg, event } => {
                *leg != 0
                    && match event {
                        Event::Frame {
                            publisher, purpose, ..
                        } => *publisher > 0 && (1..=purpose.frame_limit()).contains(&length),
                        _ => length == 0,
                    }
            }
        }
    }

    fn matches(&self, command: &Command) -> bool {
        if matches!(self, Self::Rejected) {
            return true;
        }
        match (command, self) {
            (
                Command::Offer { leg, id, .. } | Command::UpdatePublish { leg, id, .. },
                Self::Description {
                    leg: answered,
                    description,
                },
            ) => leg == answered && *id == description.id,
            (
                Command::AcceptOffer {
                    leg,
                    description: offered,
                },
                Self::Description {
                    leg: answered,
                    description,
                },
            ) => leg == answered && offered.id == description.id,
            (
                Command::Poll { leg },
                Self::Wake { leg: answered, .. }
                | Self::Datagram { leg: answered, .. }
                | Self::Event { leg: answered, .. },
            ) => leg == answered,
            (
                Command::Offer { .. }
                | Command::UpdatePublish { .. }
                | Command::AcceptOffer { .. }
                | Command::Poll { .. },
                _,
            ) => false,
            (_, Self::Done) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Connected,
    Disconnected,
    BandwidthEstimate {
        bitrate: u64,
    },
    Keyframe {
        purpose: Purpose,
    },
    Frame {
        publisher: u64,
        purpose: Purpose,
        timestamp: u64,
        contiguous: bool,
        key_frame: bool,
    },
}

pub struct Packet<T> {
    pub id: u64,
    pub kind: T,
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata<T> {
    length: u32,
    kind: T,
}

fn endpoint(address: SocketAddr) -> bool {
    address.port() != 0 && !address.ip().is_unspecified() && !address.ip().is_multicast()
}

fn invalid() -> Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid RTC worker message").into()
}

fn frame<T>(
    envelope: Envelope<Metadata<T>>,
    permits: fn(&T, usize) -> bool,
) -> Result<Packet<T>, Error> {
    let Metadata { length, kind } = envelope.payload;
    if envelope.request_id == 0 || !permits(&kind, length as usize) {
        return Err(invalid());
    }
    Ok(Packet {
        id: envelope.request_id,
        kind,
        data: vec![0; length as usize],
    })
}

fn metadata<T: Serialize>(
    packet: &Packet<T>,
    permits: fn(&T, usize) -> bool,
) -> Result<Vec<u8>, Error> {
    if packet.id == 0 || !permits(&packet.kind, packet.data.len()) {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    write_frame_within(
        &mut bytes,
        &Envelope::new(
            packet.id,
            Metadata {
                kind: &packet.kind,
                length: u32::try_from(packet.data.len()).map_err(|_| Error::TooLarge)?,
            },
        ),
        MAX_METADATA,
    )?;
    Ok(bytes)
}

pub fn read_request(reader: &mut impl Read) -> Result<Option<Packet<Command>>, Error> {
    let Some(envelope) = read_frame_within(reader, MAX_METADATA)? else {
        return Ok(None);
    };
    let mut packet = frame(envelope, Command::permits)?;
    reader.read_exact(&mut packet.data)?;
    Ok(Some(packet))
}

pub fn write_reply(writer: &mut impl Write, packet: &Packet<Reply>) -> Result<(), Error> {
    writer.write_all(&metadata(packet, Reply::permits)?)?;
    writer.write_all(&packet.data)?;
    writer.flush()?;
    Ok(())
}

async fn read_reply(
    reader: &mut (impl AsyncRead + Unpin),
    request: &Packet<Command>,
) -> Result<Packet<Reply>, Error> {
    let envelope: Envelope<Metadata<Reply>> = read_frame_async(reader, MAX_METADATA).await?;
    if envelope.request_id != request.id || !envelope.payload.kind.matches(&request.kind) {
        return Err(invalid());
    }
    let mut packet = frame(envelope, Reply::permits)?;
    reader.read_exact(&mut packet.data).await?;
    Ok(packet)
}

async fn write_request(
    writer: &mut (impl AsyncWrite + Unpin),
    packet: &Packet<Command>,
) -> Result<(), Error> {
    writer
        .write_all(&metadata(packet, Command::permits)?)
        .await?;
    writer.write_all(&packet.data).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtc_data_lengths_are_validated_before_reading_the_payload() {
        let kind = Command::Datagram {
            leg: 1,
            source: SocketAddr::from(([127, 0, 0, 1], 1234)),
        };
        let mut bytes = metadata(
            &Packet {
                id: 1,
                kind: kind.clone(),
                data: vec![0xab; MAX_DATAGRAM],
            },
            Command::permits,
        )
        .unwrap();
        bytes.extend([0xab; MAX_DATAGRAM]);
        let packet = read_request(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(packet.id, 1);
        assert_eq!(packet.data, [0xab; MAX_DATAGRAM]);
        let mut bad = Vec::new();
        write_frame_within(
            &mut bad,
            &Envelope::new(
                1,
                Metadata {
                    kind,
                    length: (MAX_DATAGRAM + 1) as u32,
                },
            ),
            MAX_METADATA,
        )
        .unwrap();
        let Err(Error::Io(error)) = read_request(&mut bad.as_slice()) else {
            panic!("expected an invalid datagram length");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
