// SPDX-License-Identifier: EUPL-1.2
//! Rooms and admission (docs/video-plan.md §4). A room is named by the link's
//! `stream_id` and its host verifier `V = SHA-256(host_key)`, and holds two keys
//! its creator hands over: `admit_key`, which every link holder can derive from
//! S, and `host_key`, which only holders of P can derive. Creating a room needs
//! a `host_key` that hashes to V, so a link names exactly one room. A proof of `admit_key` lets a peer publish its own sources and
//! subscribe; a proof of `host_key` also lets it create the room, remove peers
//! and end the room. A peer proves a key with an HMAC over the nonce the SFU
//! sent on its connection, so a proof cannot be replayed on another connection.
//!
//! A room keeps its keys until it expires, including while it is empty, so the
//! room id names the same keys for the whole life of its link. A room also
//! ends when its lease lapses: the creator's device renews it with `host_key`.

pub use crate::protocol::{PeerId, Role};
use hmac::{KeyInit, Mac};
use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant, SystemTime},
};

/// `(stream_id, V)`.
pub type RoomId = ([u8; 16], [u8; 32]);
pub type Key = [u8; 32];
pub type Nonce = [u8; 32];

/// Labels of the two proofs: `HMAC-SHA256(key, label || nonce)`.
pub const ADMIT_LABEL: &[u8] = b"nuschl/stream/v2 sfu admit";
pub const HOST_LABEL: &[u8] = b"nuschl/stream/v2 sfu host";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Proof,
    NoRoom,
    Ended,
    Full,
    Rooms,
    Rate,
    NotAllowed,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Proof => "proof",
            Self::NoRoom => "no_room",
            Self::Ended => "ended",
            Self::Full => "full",
            Self::Rooms => "busy",
            Self::Rate => "rate",
            Self::NotAllowed => "not_allowed",
        }
    }
}

pub struct Join {
    pub room: RoomId,
    pub proof: [u8; 32],
    pub host_proof: Option<[u8; 32]>,
    /// `(admit_key, host_key)`: creates the room, or joins it if it exists
    /// with the same keys.
    pub create: Option<(Key, Key)>,
    pub decoders: crate::protocol::Decoders,
    pub video_limits: bool,
    pub screen_rate: bool,
}

pub fn prove(key: &Key, label: &[u8], nonce: &Nonce) -> [u8; 32] {
    mac(key, label, nonce).finalize().into_bytes().into()
}

fn mac(key: &Key, label: &[u8], nonce: &Nonce) -> hmac::Hmac<sha2::Sha256> {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("any key length");
    mac.update(label);
    mac.update(nonce);
    mac
}

/// The host verifier the link carries: `V = SHA-256(host_key)`.
pub fn verifier(host_key: &Key) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(host_key).into()
}

fn verify(key: &Key, label: &[u8], nonce: &Nonce, proof: &[u8; 32]) -> bool {
    mac(key, label, nonce).verify_slice(proof).is_ok()
}

/// A token bucket: `rate` units per second, `burst` at most.
#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    updated: Instant,
}

impl Bucket {
    pub fn new(rate: f64, burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            rate,
            burst,
            updated: now,
        }
    }

    pub fn take(&mut self, amount: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.updated = now;
        if self.tokens < amount {
            return false;
        }
        self.tokens -= amount;
        true
    }
}

/// A media budget in bytes for `bits_per_second`, with a burst of one second,
/// or of the largest frame a publisher sends up to three seconds.
pub fn media_budget(bits_per_second: u64, now: Instant) -> Bucket {
    let bytes = bits_per_second as f64 / 8.0;
    let key = (crate::quality::MAX_KEY_BYTES as f64).min(3.0 * bytes);
    Bucket::new(bytes, bytes.max(key), now)
}

pub struct Room {
    admit_key: Key,
    host_key: Key,
    expires: Instant,
    /// The room ends when this passes without a renewal.
    lease: Instant,
    renewed: Option<Instant>,
    ended: bool,
    pub peers: Vec<(PeerId, Role)>,
    pub budget: Bucket,
}

pub struct Limits {
    pub rooms: usize,
    pub peers_per_room: usize,
    pub lifetime: Duration,
    pub lease: Duration,
    pub creations_per_hour: u32,
    pub room_bitrate: u64,
}

pub struct Rooms {
    limits: Limits,
    rooms: HashMap<RoomId, Room>,
    /// Room creations per client network in the current hour.
    creations: HashMap<IpAddr, (Instant, u32)>,
}

/// IPv4 addresses as they are, IPv6 addresses by their /64.
pub fn network(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
        },
        v4 => v4,
    }
}

const HOUR: Duration = Duration::from_secs(3600);

impl Rooms {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            rooms: HashMap::new(),
            creations: HashMap::new(),
        }
    }

    pub fn get(&self, room: &RoomId) -> Option<&Room> {
        self.rooms.get(room)
    }

    pub fn get_mut(&mut self, room: &RoomId) -> Option<&mut Room> {
        self.rooms.get_mut(room)
    }

    pub fn join(
        &mut self,
        peer: PeerId,
        join: &Join,
        nonce: &Nonce,
        client: IpAddr,
        now: Instant,
    ) -> Result<Role, Refusal> {
        let (admit_key, host_key) = match (self.rooms.get(&join.room), join.create) {
            (Some(room), _) if room.ended => return Err(Refusal::Ended),
            (Some(room), Some(keys)) if (room.admit_key, room.host_key) != keys => {
                return Err(Refusal::Proof);
            }
            (Some(room), _) => (room.admit_key, room.host_key),
            (None, Some(keys)) if verifier(&keys.1) == join.room.1 => keys,
            (None, Some(_)) => return Err(Refusal::Proof),
            (None, None) => return Err(Refusal::NoRoom),
        };
        if !verify(&admit_key, ADMIT_LABEL, nonce, &join.proof) {
            return Err(Refusal::Proof);
        }
        let role = match &join.host_proof {
            None => Role::Participant,
            Some(proof) if verify(&host_key, HOST_LABEL, nonce, proof) => Role::Host,
            Some(_) => return Err(Refusal::Proof),
        };
        if !self.rooms.contains_key(&join.room) {
            if role != Role::Host {
                return Err(Refusal::Proof);
            }
            self.create(join.room, admit_key, host_key, client, now)?;
        }
        let room = self.rooms.get_mut(&join.room).expect("exists");
        if room.peers.len() >= self.limits.peers_per_room {
            return Err(Refusal::Full);
        }
        room.peers.push((peer, role));
        Ok(role)
    }

    fn create(
        &mut self,
        id: RoomId,
        admit_key: Key,
        host_key: Key,
        client: IpAddr,
        now: Instant,
    ) -> Result<(), Refusal> {
        if self.rooms.len() >= self.limits.rooms {
            return Err(Refusal::Rooms);
        }
        // The table holds at most one entry per room slot; older ones go first.
        if self.creations.len() >= self.limits.rooms {
            self.creations
                .retain(|_, (start, _)| now.saturating_duration_since(*start) < HOUR);
            if self.creations.len() >= self.limits.rooms {
                return Err(Refusal::Rate);
            }
        }
        let entry = self.creations.entry(network(client)).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= HOUR {
            *entry = (now, 0);
        }
        if entry.1 >= self.limits.creations_per_hour {
            return Err(Refusal::Rate);
        }
        entry.1 += 1;
        self.rooms.insert(
            id,
            Room {
                admit_key,
                host_key,
                expires: now + self.limits.lifetime,
                lease: now + self.limits.lease,
                renewed: None,
                ended: false,
                peers: Vec::new(),
                budget: media_budget(self.limits.room_bitrate, now),
            },
        );
        Ok(())
    }

    pub fn leave(&mut self, room: &RoomId, peer: PeerId) {
        if let Some(room) = self.rooms.get_mut(room) {
            room.peers.retain(|(id, _)| *id != peer);
        }
    }

    /// Whether `peer` may remove peers and end the room.
    pub fn is_host(&self, room: &RoomId, peer: PeerId) -> bool {
        self.rooms
            .get(room)
            .is_some_and(|room| room.peers.contains(&(peer, Role::Host)))
    }

    /// A host ends the room; it refuses joins until it expires. Returns the
    /// peers to disconnect.
    pub fn end(&mut self, room: &RoomId, by: PeerId) -> Result<Vec<PeerId>, Refusal> {
        if !self.is_host(room, by) {
            return Err(Refusal::NotAllowed);
        }
        let room = self.rooms.get_mut(room).ok_or(Refusal::NoRoom)?;
        room.ended = true;
        Ok(room.peers.drain(..).map(|(id, _)| id).collect())
    }

    /// The creator's device renews the room's lease, at most once a second,
    /// or ends the room. Returns the peers to disconnect.
    pub fn lease(
        &mut self,
        room: &RoomId,
        host_key: &Key,
        end: bool,
        now: Instant,
    ) -> Result<Vec<PeerId>, Refusal> {
        let lease = self.limits.lease;
        let room = self
            .rooms
            .get_mut(room)
            .filter(|r| !r.ended)
            .ok_or(Refusal::NoRoom)?;
        if room.host_key != *host_key {
            return Err(Refusal::Proof);
        }
        if end {
            room.ended = true;
            return Ok(room.peers.drain(..).map(|(id, _)| id).collect());
        }
        if room
            .renewed
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1))
        {
            return Err(Refusal::Rate);
        }
        room.renewed = Some(now);
        room.lease = now + lease;
        Ok(Vec::new())
    }

    /// Ends rooms whose lease lapsed, removes expired rooms, and returns the
    /// peers of both to disconnect.
    pub fn expire(&mut self, now: Instant) -> Vec<PeerId> {
        let mut peers = Vec::new();
        self.rooms.retain(|_, room| {
            let keep = room.expires > now;
            if !keep || room.lease <= now {
                room.ended = true;
                peers.extend(room.peers.drain(..).map(|(id, _)| id));
            }
            keep
        });
        peers
    }
}

/// TURN REST credentials: `expiry:random` and base64(HMAC-SHA1(secret, username)).
pub fn turn_credentials(secret: &[u8], lifetime: Duration, now: SystemTime) -> (String, String) {
    use base64::Engine as _;
    let expiry = (now + lifetime)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let username = format!("{expiry}:{}", hex::encode(random::<8>()));
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(secret).expect("any key length");
    mac.update(username.as_bytes());
    let password = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    (username, password)
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the OS random number generator");
    bytes
}

#[cfg(test)]
mod tests;
