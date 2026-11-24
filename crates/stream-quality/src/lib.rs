// SPDX-License-Identifier: EUPL-1.2
//! Call quality (docs/video-plan.md §5), shared by the SFU, the call pages and the
//! native driver: the camera and screen tier ladders, the fixed-size quality
//! requests and decode-capacity reports, per-receiver layer choice from the AV1
//! Dependency Descriptor ([`dd`]), the room and host budgets ([`budget`]) and the
//! step-down and step-up timing ([`adapt`]).
//!
//! Pure logic: callers pass bytes and the time, a [`Duration`] since an origin they
//! pick on a monotonic clock. Rates are bits per second. Each receiver gets
//! min(request, decode capacity, network capacity, policy) ([`receive`]).
//!
//! The SFU, for each receiver and source: [`dd::Layers::choices`] lists the decode
//! targets that fit the requested tier, [`budget::allocate`] over every stream
//! gives each its highest affordable choice, an [`adapt::Stepper`] moves toward
//! that choice in time, and a [`dd::Selector`] forwards it.

pub mod adapt;
pub mod budget;
pub mod dd;

use std::time::Duration;

/// The wire version of [`QualityRequest`] and [`DecodeCapacity`].
pub const VERSION: u8 = 1;

/// The two sources a participant publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Camera,
    Screen,
}

/// One rung of a ladder (§5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    /// The shorter side of the picture, in pixels.
    pub height: u16,
    pub fps: u8,
    /// The lowest bitrate the tier runs at.
    pub floor: u64,
    /// The bitrate the tier needs at most.
    pub ceiling: u64,
}

const fn tier(height: u16, fps: u8, floor_kbit: u64, ceiling_kbit: u64) -> Tier {
    Tier {
        height,
        fps,
        floor: floor_kbit * 1000,
        ceiling: ceiling_kbit * 1000,
    }
}

/// Camera tiers, lowest first: 180p15 (floor), 360p30 (tile), 720p30 (speaker),
/// 1080p30 (top).
pub const CAMERA: [Tier; 4] = [
    tier(180, 15, 100, 200),
    tier(360, 30, 300, 600),
    tier(720, 30, 1000, 2000),
    tier(1080, 30, 2000, 4000),
];

/// Screen tiers, lowest first: 720p15 (floor), 1080p30, 1440p60, 4K60.
pub const SCREEN: [Tier; 4] = [
    tier(720, 15, 400, 800),
    tier(1080, 30, 1500, 3000),
    tier(1440, 60, 3000, 8000),
    tier(2160, 60, 6000, 15000),
];

/// A user's camera or screen quality setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// The tile's size.
    Auto,
    /// The tile's size up to this tier index into the ladder.
    Tier(u8),
}

impl Kind {
    pub fn ladder(self) -> &'static [Tier] {
        match self {
            Self::Camera => &CAMERA,
            Self::Screen => &SCREEN,
        }
    }

    pub fn top(self) -> usize {
        self.ladder().len() - 1
    }

    /// The tier a picture whose shorter side is `height` pixels belongs to: the
    /// highest tier at most that tall, or the lowest.
    pub fn tier_of(self, height: u32) -> usize {
        self.ladder()
            .iter()
            .rposition(|t| u32::from(t.height) <= height)
            .unwrap_or(0)
    }

    /// The tier that covers a tile `height` device pixels tall: the lowest tier at
    /// least that tall, or the top one.
    pub fn fit(self, height: u32) -> usize {
        self.ladder()
            .iter()
            .position(|t| u32::from(t.height) >= height)
            .unwrap_or(self.top())
    }

    /// The highest tier whose floor `bps` covers, or the lowest.
    pub fn affordable(self, bps: u64) -> usize {
        self.ladder()
            .iter()
            .rposition(|t| t.floor <= bps)
            .unwrap_or(0)
    }

    /// The tier a viewer asks for: the tile's size, up to a chosen tier. Before
    /// the tile has a size (0), a chosen tier asks for itself.
    pub fn request(self, quality: Quality, tile_height: u32) -> usize {
        let fit = (tile_height > 0).then(|| self.fit(tile_height));
        match quality {
            Quality::Auto => self.fit(tile_height),
            Quality::Tier(tier) => usize::from(tier)
                .min(self.top())
                .min(fit.unwrap_or(usize::MAX)),
        }
    }
}

/// The tier a receiver gets: min(request, decode capacity, network capacity,
/// policy), each a tier index.
pub fn receive(request: usize, decode: usize, network: usize, policy: usize) -> usize {
    request.min(decode).min(network).min(policy)
}

/// A publisher's targets for its screen and its camera (§5.3): min(upload × 0.85,
/// largest demand), the screen first. A demand is the tier asked of that source,
/// `None` for a source it does not publish.
pub fn split(upload: u64, screen: Option<usize>, camera: Option<usize>) -> (u64, u64) {
    let mut left = upload / 100 * 85;
    let mut take = |kind: Kind, demand: Option<usize>| {
        let want = demand.map_or(0, |tier| kind.ladder()[tier.min(kind.top())].ceiling);
        let got = want.min(left);
        left -= got;
        got
    };
    let screen = take(Kind::Screen, screen);
    (screen, take(Kind::Camera, camera))
}

fn kind_byte(kind: Kind) -> u8 {
    match kind {
        Kind::Camera => 0,
        Kind::Screen => 1,
    }
}

/// A receiver's request for one source (§5.3), sent at most once a second
/// ([`Throttle`]). The SFU sends the largest demand per source back to its
/// publisher in the same form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualityRequest {
    /// The SFU's id of the publishing peer.
    pub publisher: u64,
    pub kind: Kind,
    /// A tier index into `kind`'s ladder.
    pub tier: u8,
}

impl QualityRequest {
    pub const SIZE: usize = 11;

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0; Self::SIZE];
        bytes[0] = VERSION;
        bytes[1..9].copy_from_slice(&self.publisher.to_be_bytes());
        bytes[9] = kind_byte(self.kind);
        bytes[10] = self.tier;
        bytes
    }

    /// Reads a request of this version with a tier inside its ladder.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::SIZE] = bytes.try_into().ok()?;
        let kind = match bytes[9] {
            0 => Kind::Camera,
            1 => Kind::Screen,
            _ => return None,
        };
        (bytes[0] == VERSION && usize::from(bytes[10]) <= kind.top()).then(|| Self {
            publisher: u64::from_be_bytes(bytes[1..9].try_into().expect("8 bytes")),
            kind,
            tier: bytes[10],
        })
    }
}

/// The highest tier a receiver decodes per kind (§5.3). The caller seals it under
/// the channel key; [`adapt::DecodeWatch`] tells when to lower it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeCapacity {
    pub camera: u8,
    pub screen: u8,
}

impl DecodeCapacity {
    pub const SIZE: usize = 3;

    pub fn get(self, kind: Kind) -> usize {
        usize::from(match kind {
            Kind::Camera => self.camera,
            Kind::Screen => self.screen,
        })
    }

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        [VERSION, self.camera, self.screen]
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let &[VERSION, camera, screen] = bytes else {
            return None;
        };
        (usize::from(camera) <= Kind::Camera.top() && usize::from(screen) <= Kind::Screen.top())
            .then_some(Self { camera, screen })
    }
}

/// Lets one message through per second: a sender keeps its latest request until
/// [`Throttle::next`], and the SFU drops requests that come sooner.
#[derive(Clone, Copy, Debug, Default)]
pub struct Throttle {
    last: Option<Duration>,
}

impl Throttle {
    pub const INTERVAL: Duration = Duration::from_secs(1);

    pub fn allow(&mut self, now: Duration) -> bool {
        if now < self.next() {
            return false;
        }
        self.last = Some(now);
        true
    }

    /// The earliest time [`Throttle::allow`] lets the next message through.
    pub fn next(&self) -> Duration {
        self.last
            .map_or(Duration::ZERO, |last| last + Self::INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladders_rise_and_auto_asks_for_the_tile() {
        for kind in [Kind::Camera, Kind::Screen] {
            assert!(kind.ladder().windows(2).all(|w| w[0].height < w[1].height
                && w[0].floor < w[1].floor
                && w[0].floor < w[0].ceiling));
        }
        // Grid tiles ask for 360p, the large view 720p or more.
        assert_eq!(CAMERA[Kind::Camera.request(Quality::Auto, 300)].height, 360);
        assert_eq!(CAMERA[Kind::Camera.request(Quality::Auto, 700)].height, 720);
        assert_eq!(Kind::Camera.request(Quality::Auto, 4000), 3);
        assert_eq!(Kind::Screen.request(Quality::Tier(9), 0), 3);
        assert_eq!(Kind::Screen.request(Quality::Tier(1), 2000), 1);
        assert_eq!(
            Kind::Screen.request(Quality::Tier(2), 700),
            0,
            "a tier limits the tile"
        );
        assert_eq!(Kind::Screen.tier_of(768), 0);
        assert_eq!(Kind::Screen.tier_of(1440), 2);
        assert_eq!(Kind::Camera.tier_of(90), 0);
        assert_eq!(Kind::Camera.affordable(1_200_000), 2);
        assert_eq!(Kind::Camera.affordable(0), 0);
    }

    #[test]
    fn a_receiver_gets_the_least_of_its_limits() {
        assert_eq!(receive(3, 2, 3, 3), 2);
        assert_eq!(receive(3, 3, 1, 3), 1);
        assert_eq!(receive(0, 3, 3, 3), 0);
        assert_eq!(receive(3, 3, 3, 2), 2);
    }

    #[test]
    fn the_screen_takes_the_upload_first() {
        // 10 Mbit/s upload: 8.5 to share; a 1440p60 screen wants 8.
        assert_eq!(split(10_000_000, Some(2), Some(3)), (8_000_000, 500_000));
        assert_eq!(split(10_000_000, None, Some(3)), (0, 4_000_000));
        assert_eq!(split(1_000_000, Some(3), Some(3)), (850_000, 0));
    }

    #[test]
    fn messages_have_fixed_sizes_and_valid_fields() {
        let request = QualityRequest {
            publisher: 0x0102_0304_0506_0708,
            kind: Kind::Screen,
            tier: 3,
        };
        let bytes = request.to_bytes();
        assert_eq!(QualityRequest::from_bytes(&bytes), Some(request));
        assert_eq!(QualityRequest::from_bytes(&bytes[..10]), None);
        for (index, value) in [(0, 2), (9, 2), (10, 4)] {
            let mut bad = bytes;
            bad[index] = value;
            assert_eq!(QualityRequest::from_bytes(&bad), None);
        }
        let capacity = DecodeCapacity {
            camera: 2,
            screen: 1,
        };
        assert_eq!(
            DecodeCapacity::from_bytes(&capacity.to_bytes()),
            Some(capacity)
        );
        assert_eq!(capacity.get(Kind::Camera), 2);
        assert_eq!(DecodeCapacity::from_bytes(&[VERSION, 4, 0]), None);
        assert_eq!(DecodeCapacity::from_bytes(&[2, 0, 0]), None);
        assert_eq!(DecodeCapacity::from_bytes(&[VERSION, 0]), None);
    }

    #[test]
    fn requests_pass_once_a_second() {
        let mut throttle = Throttle::default();
        let at = Duration::from_millis;
        assert!(throttle.allow(at(5000)));
        assert!(!throttle.allow(at(5999)));
        assert_eq!(throttle.next(), at(6000));
        assert!(throttle.allow(at(6000)));
        assert!(!throttle.allow(at(6500)));
        assert!(throttle.allow(at(9000)));
    }
}
