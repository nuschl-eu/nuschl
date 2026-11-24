// SPDX-License-Identifier: EUPL-1.2
//! Step-down and step-up timing (§5.3). A [`Stepper`] moves along a list of
//! points, lowest first, grouped by what they keep and ordered by what gives way
//! within a group: an encoder's tiers ([`Encoder`]) or a receiver's decode
//! targets ([`crate::dd::Layers::choices`]), grouped by resolution, or by frame
//! rate for a Smooth source. It steps down at once within a group and changes
//! group at most once per 10 s; it steps up after the budget has covered 1.3
//! times the next point's floor for 10 s (within a group) or 30 s (to another
//! group).

use crate::{Kind, Tier};
use std::time::Duration;

const WITHIN_GROUP_UP: Duration = Duration::from_secs(10);
const GROUP_UP: Duration = Duration::from_secs(30);
const GROUP_INTERVAL: Duration = Duration::from_secs(10);
/// A receiver lowers its decode capacity after this long below 90 % of its
/// target frame rate.
const DECODE_SLOW: Duration = Duration::from_secs(3);

/// What a point above the current one costs: 1.3 times its floor.
pub fn step_up_cost(floor: u64) -> u64 {
    floor.saturating_mul(13) / 10
}

/// The highest point `available` allows from point `current`: points up to it
/// need their floor, higher ones [`step_up_cost`]. Point 0 when none fits.
pub fn ceiling(floors: &[u64], current: usize, available: u64) -> usize {
    (0..floors.len())
        .rev()
        .find(|&i| {
            let cost = if i > current {
                step_up_cost(floors[i])
            } else {
                floors[i]
            };
            cost <= available
        })
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug)]
pub struct Stepper {
    current: usize,
    up_since: Option<Duration>,
    group_changed: Option<Duration>,
}

impl Stepper {
    pub fn new(current: usize) -> Self {
        Self {
            current,
            up_since: None,
            group_changed: None,
        }
    }

    pub fn current(&self) -> usize {
        self.current
    }

    /// Moves toward `ceiling`, the highest point the budget allows now (for
    /// example from [`ceiling`] or [`crate::budget::allocate`]). `groups[i]` is
    /// point i's group, its resolution or frame rate; returns the point to use.
    pub fn update(&mut self, groups: &[u32], ceiling: usize, now: Duration) -> usize {
        let Some(top) = groups.len().checked_sub(1) else {
            return 0;
        };
        let ceiling = ceiling.min(top);
        self.current = self.current.min(top);
        let group = groups[self.current];
        let next = if ceiling < self.current {
            self.up_since = None;
            let lowest = groups
                .iter()
                .position(|&g| g == group)
                .unwrap_or(self.current);
            let may_change = self
                .group_changed
                .is_none_or(|at| now.saturating_sub(at) >= GROUP_INTERVAL);
            if ceiling >= lowest || may_change {
                ceiling
            } else {
                lowest
            }
        } else if ceiling > self.current {
            let next = self.current + 1;
            let hold = if groups[next] == group {
                WITHIN_GROUP_UP
            } else {
                GROUP_UP
            };
            let since = *self.up_since.get_or_insert(now);
            if now.saturating_sub(since) < hold {
                return self.current;
            }
            self.up_since = None;
            next
        } else {
            self.up_since = None;
            return self.current;
        };
        if groups[next] != group {
            self.group_changed = Some(now);
        }
        self.current = next;
        next
    }
}

/// An encoder's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    pub height: u16,
    pub fps: u8,
    pub bitrate: u64,
}

/// A publisher's encoder for one source. Its points are each tier at half and at
/// full frame rate, half the frame rate at half the floor, without the lowest
/// tier's half rate. As the budget falls, the bitrate follows it down to the
/// point's floor, then the frame rate halves, then the resolution drops.
#[derive(Clone, Copy, Debug)]
pub struct Encoder {
    kind: Kind,
    stepper: Stepper,
}

impl Encoder {
    /// Starts at `tier`'s full frame rate.
    pub fn new(kind: Kind, tier: usize) -> Self {
        Self {
            kind,
            stepper: Stepper::new(2 * tier.min(kind.top())),
        }
    }

    /// `top` is the highest tier: the user's choice and the largest demand.
    /// `available` is this source's share of the upload ([`crate::split`]).
    pub fn update(&mut self, top: usize, available: u64, now: Duration) -> Setting {
        let points: Vec<(Tier, u8, u64)> = self.kind.ladder()[..=top.min(self.kind.top())]
            .iter()
            .flat_map(|&t| [(t, t.fps / 2, t.floor / 2), (t, t.fps, t.floor)])
            .skip(1)
            .collect();
        let floors: Vec<u64> = points.iter().map(|p| p.2).collect();
        let resolutions: Vec<u32> = points.iter().map(|p| u32::from(p.0.height)).collect();
        let ceiling = ceiling(&floors, self.stepper.current(), available);
        let (tier, fps, _) = points[self.stepper.update(&resolutions, ceiling, now)];
        Setting {
            height: tier.height,
            fps,
            bitrate: available.min(tier.ceiling),
        }
    }
}

/// Tells a receiver when to lower its decode capacity by one tier: after 3 s
/// below 90 % of the target frame rate.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecodeWatch {
    slow_since: Option<Duration>,
}

impl DecodeWatch {
    /// `fps` is the rate the receiver decoded, `target` the rate of what it
    /// receives.
    pub fn update(&mut self, fps: f32, target: f32, now: Duration) -> bool {
        if fps >= target * 0.9 {
            self.slow_since = None;
            return false;
        }
        let since = *self.slow_since.get_or_insert(now);
        if now.saturating_sub(since) < DECODE_SLOW {
            return false;
        }
        self.slow_since = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn the_ceiling_needs_headroom_above_the_current_point() {
        let floors = [100, 200, 400];
        assert_eq!(ceiling(&floors, 0, 259), 0);
        assert_eq!(ceiling(&floors, 0, 260), 1);
        // Holding a point needs only its floor.
        assert_eq!(ceiling(&floors, 1, 200), 1);
        assert_eq!(ceiling(&floors, 2, 400), 2);
        assert_eq!(ceiling(&floors, 2, 399), 1);
        assert_eq!(ceiling(&floors, 2, 0), 0);
    }

    /// Two resolutions with two frame rates each.
    const RESOLUTIONS: [u32; 4] = [360, 360, 720, 720];

    #[test]
    fn steps_up_after_10_s_for_frame_rate_and_30_s_for_resolution() {
        let mut stepper = Stepper::new(0);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(0)), 0);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(9)), 0);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(10)), 1);
        // A new hold starts with the next update.
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(11)), 1);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(40)), 1);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(41)), 2);
        // The budget falling back resets the hold.
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(42)), 2);
        assert_eq!(stepper.update(&RESOLUTIONS, 2, secs(50)), 2);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(51)), 2);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(60)), 2);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(61)), 3);
    }

    #[test]
    fn steps_down_frame_rate_at_once_and_resolution_once_per_10_s() {
        let mut stepper = Stepper::new(3);
        assert_eq!(stepper.update(&RESOLUTIONS, 2, secs(0)), 2);
        assert_eq!(stepper.update(&RESOLUTIONS, 1, secs(1)), 1);
        // Within 10 s of that resolution change only the frame rate drops.
        assert_eq!(stepper.update(&RESOLUTIONS, 0, secs(2)), 0);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(2)), 0);
        let mut stepper = Stepper::new(3);
        assert_eq!(stepper.update(&RESOLUTIONS, 3, secs(0)), 3);
        stepper.group_changed = Some(secs(100));
        assert_eq!(stepper.update(&RESOLUTIONS, 0, secs(105)), 2);
        assert_eq!(stepper.update(&RESOLUTIONS, 0, secs(109)), 2);
        // One resolution change may cross several points.
        assert_eq!(stepper.update(&RESOLUTIONS, 0, secs(110)), 0);
    }

    #[test]
    fn a_smooth_source_drops_resolution_at_once_and_frame_rate_once_per_10_s() {
        // Smooth decode targets (spatial, temporal): (0, 0), (0, 1), (0, 2),
        // (1, 2), (2, 2), grouped by frame rate.
        let frame_rates = [0, 1, 2, 2, 2];
        let mut stepper = Stepper::new(4);
        assert_eq!(stepper.update(&frame_rates, 2, secs(0)), 2);
        assert_eq!(stepper.update(&frame_rates, 1, secs(1)), 1);
        assert_eq!(stepper.update(&frame_rates, 0, secs(2)), 1);
        assert_eq!(stepper.update(&frame_rates, 0, secs(11)), 0);
    }

    #[test]
    fn an_encoder_lowers_bitrate_then_frame_rate_then_resolution() {
        let mut encoder = Encoder::new(Kind::Camera, 2);
        let at = |encoder: &mut Encoder, available, t| encoder.update(3, available, secs(t));
        let full = Setting {
            height: 720,
            fps: 30,
            bitrate: 1_500_000,
        };
        assert_eq!(at(&mut encoder, 1_500_000, 0), full);
        // Bitrate down to the floor.
        assert_eq!(at(&mut encoder, 1_000_000, 1).fps, 30);
        // Then 720p15 at half the floor.
        let half = at(&mut encoder, 900_000, 2);
        assert_eq!((half.height, half.fps, half.bitrate), (720, 15, 900_000));
        // Then 360p30.
        let low = at(&mut encoder, 450_000, 3);
        assert_eq!((low.height, low.fps), (360, 30));
        // 720p15 needs 650 kbit/s for 30 s.
        assert_eq!(at(&mut encoder, 650_000, 4).height, 360);
        assert_eq!(at(&mut encoder, 650_000, 34).height, 720);
        // A lower demand caps the tier.
        let mut encoder = Encoder::new(Kind::Screen, 3);
        assert_eq!(encoder.update(1, 20_000_000, secs(0)).height, 1080);
        // The floor tier keeps its frame rate.
        let mut encoder = Encoder::new(Kind::Camera, 0);
        assert_eq!(encoder.update(3, 0, secs(0)).fps, 15);
    }

    #[test]
    fn decode_capacity_drops_after_3_s_below_90_percent() {
        let mut watch = DecodeWatch::default();
        assert!(!watch.update(26.0, 30.0, secs(0)));
        assert!(!watch.update(26.0, 30.0, secs(2)));
        // Recovering resets the timer.
        assert!(!watch.update(27.0, 30.0, secs(3)));
        assert!(!watch.update(20.0, 30.0, secs(4)));
        assert!(!watch.update(20.0, 30.0, secs(6)));
        assert!(watch.update(20.0, 30.0, secs(7)));
        // The next step needs another 3 s.
        assert!(!watch.update(20.0, 30.0, secs(8)));
        assert!(watch.update(20.0, 30.0, secs(11)));
    }
}
