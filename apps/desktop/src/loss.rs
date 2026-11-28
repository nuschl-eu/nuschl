// SPDX-License-Identifier: EUPL-1.2
//! Packet loss over time: what a connection's cumulative counters say, cut into
//! buckets, so a long call shows whether packets went missing now or an hour
//! ago.
use std::collections::VecDeque;

/// One bucket counts this long.
pub const SLOT_MS: u64 = 20_000;
/// The buckets one ring keeps: ten minutes of them.
pub const SLOTS: usize = 30;

/// What the buckets of the last minute say about a direction's health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// No packet went missing.
    Good,
    /// Some did.
    Lossy,
    /// A tenth or more did.
    Poor,
}

/// Both directions of one server's voice, over time: what arrived, and what
/// the server counted of ours.
#[derive(Default)]
pub struct Loss {
    pub received: Ring,
    pub sent: Ring,
}

impl Loss {
    /// Take both directions' cumulative counters at `now_ms`.
    pub fn sample(&mut self, received: (u32, u32, u32), sent: (u32, u32, u32), now_ms: u64) {
        self.received.sample(received, now_ms);
        self.sent.sample(sent, now_ms);
    }

    /// The signal the voice card shows: the worse of the two directions.
    pub fn health(&self) -> Health {
        match (self.received.health(), self.sent.health()) {
            (Health::Poor, _) | (_, Health::Poor) => Health::Poor,
            (Health::Lossy, _) | (_, Health::Lossy) => Health::Lossy,
            _ => Health::Good,
        }
    }

    /// What both directions say, for screen readers and a tooltip.
    pub fn label(&self) -> String {
        format!(
            "{} {}",
            self.received.label("Received"),
            self.sent.label("Sent")
        )
    }
}

/// A direction's loss over time.
#[derive(Default)]
pub struct Ring {
    /// The loss of each finished bucket, in percent, oldest first.
    done: VecDeque<u8>,
    /// When the bucket being counted started, and its packets.
    since: Option<u64>,
    packets: (u32, u32, u32),
    /// The counters the last sample saw.
    last: Option<(u32, u32, u32)>,
}

impl Ring {
    /// Take the cumulative counters `(good, late, lost)` at `now_ms`. A counter
    /// that went backwards belongs to a fresh connection, so the ring starts
    /// over there. What arrived since the last sample counts toward the bucket
    /// the sample falls in; a gap longer than a bucket leaves the buckets in
    /// between empty.
    pub fn sample(&mut self, counters: (u32, u32, u32), now_ms: u64) {
        let (good, late, lost) = counters;
        let deltas = match self.last {
            Some((seen_good, seen_late, seen_lost))
                if good >= seen_good && late >= seen_late && lost >= seen_lost =>
            {
                (good - seen_good, late - seen_late, lost - seen_lost)
            }
            Some(_) => {
                *self = Self::default();
                return self.sample(counters, now_ms);
            }
            None => {
                self.since = Some(now_ms);
                self.last = Some(counters);
                return;
            }
        };
        self.last = Some(counters);
        let mut since = *self.since.get_or_insert(now_ms);
        while now_ms.saturating_sub(since) >= SLOT_MS {
            self.done.push_back(percent(self.packets.2, self.packets));
            self.packets = (0, 0, 0);
            since += SLOT_MS;
        }
        self.since = Some(since);
        self.packets.0 = self.packets.0.saturating_add(deltas.0);
        self.packets.1 = self.packets.1.saturating_add(deltas.1);
        self.packets.2 = self.packets.2.saturating_add(deltas.2);
        while self.done.len() > SLOTS {
            self.done.pop_front();
        }
    }

    /// The loss of each finished bucket, in percent, oldest first.
    #[cfg(test)]
    pub fn buckets(&self) -> impl Iterator<Item = u8> + '_ {
        self.done.iter().copied()
    }

    /// The finished buckets followed by the one still counting, so a panel
    /// opened early shows the loss of this moment at its right edge.
    pub fn bars(&self) -> impl Iterator<Item = u8> + '_ {
        let running = (self.packets.0 + self.packets.1 + self.packets.2 > 0)
            .then(|| percent(self.packets.2, self.packets));
        self.done.iter().copied().chain(running)
    }

    /// How long the finished buckets reach back, in whole minutes.
    pub fn minutes(&self) -> u64 {
        (self.done.len() as u64 * SLOT_MS / 60_000).max(1)
    }

    /// The worst loss of the buckets in the last minute, in percent.
    pub fn recent(&self) -> Option<u8> {
        let window = (60_000 / SLOT_MS) as usize;
        let skip = self.bars().count().saturating_sub(window);
        self.bars().skip(skip).max()
    }

    /// The worst loss of every bucket kept, in percent.
    pub fn worst(&self) -> Option<u8> {
        self.bars().max()
    }

    /// What the last minute's loss says about this direction.
    pub fn health(&self) -> Health {
        match self.recent() {
            Some(percent) if percent >= 10 => Health::Poor,
            Some(percent) if percent > 0 => Health::Lossy,
            _ => Health::Good,
        }
    }

    /// The ring as a sentence, for screen readers and a tooltip.
    pub fn label(&self, direction: &str) -> String {
        let Some(recent) = self.recent() else {
            return format!("{direction}: no packets counted yet.");
        };
        let minutes = match self.minutes() {
            1 => "minute".to_string(),
            minutes => format!("{minutes} minutes"),
        };
        match self.worst().unwrap_or(0) {
            0 => format!("{direction}: no loss in the last {minutes}."),
            worst => format!(
                "{direction}: {recent}% in the last minute, {worst}% at worst in the last {minutes}."
            ),
        }
    }
}

/// `lost` of `packets` as a percentage, rounded up so one lost packet of a
/// thousand reads as 1% rather than nothing.
fn percent(lost: u32, packets: (u32, u32, u32)) -> u8 {
    let total = packets.0.saturating_add(packets.1).saturating_add(lost);
    if total == 0 || lost == 0 {
        return 0;
    }
    ((f64::from(lost) * 100. / f64::from(total)).ceil() as u8).min(100)
}

#[cfg(test)]
mod tests {
    use super::{Health, Ring, SLOT_MS, SLOTS};

    /// A ring that saw `counters` at the start of each slot, closed by one more
    /// sample at the end of the last.
    fn ring_of(slots: &[(u32, u32, u32)]) -> Ring {
        let mut ring = Ring::default();
        for (index, counters) in slots.iter().enumerate() {
            ring.sample(*counters, index as u64 * SLOT_MS);
        }
        ring.sample(
            *slots.last().unwrap_or(&(0, 0, 0)),
            slots.len() as u64 * SLOT_MS,
        );
        ring
    }

    #[test]
    fn each_slot_becomes_a_bucket_when_the_next_sample_closes_it() {
        let mut ring = Ring::default();
        // The first sample only sets the counters that the next ones compare to.
        ring.sample((0, 0, 0), 0);
        ring.sample((100, 0, 0), SLOT_MS);
        assert_eq!(ring.buckets().collect::<Vec<_>>(), [0]);
        // A hundred good packets, then a hundred with a tenth lost.
        ring.sample((190, 0, 10), 2 * SLOT_MS);
        ring.sample((190, 0, 10), 3 * SLOT_MS);
        assert_eq!(ring.buckets().collect::<Vec<_>>(), [0, 0, 10]);
        assert_eq!(ring.recent(), Some(10));
        assert_eq!(ring.worst(), Some(10));
        assert_eq!(ring.health(), Health::Poor);
        assert_eq!(
            ring.label("Received"),
            "Received: 10% in the last minute, 10% at worst in the last minute."
        );
        // One lost packet of a thousand rounds up to one percent.
        let ring = ring_of(&[(0, 0, 0), (999, 0, 1)]);
        assert_eq!(ring.buckets().last(), Some(1));
        assert_eq!(ring.health(), Health::Lossy);
        // Nothing lost reads as good health.
        let ring = ring_of(&[(0, 0, 0), (500, 0, 0), (900, 0, 0)]);
        assert_eq!(ring.health(), Health::Good);
    }

    #[test]
    fn a_long_gap_leaves_empty_buckets_and_a_reset_starts_over() {
        let mut ring = Ring::default();
        ring.sample((0, 0, 0), 0);
        ring.sample((10, 0, 5), SLOT_MS);
        // Five minutes without a sample: the buckets in between stay empty.
        ring.sample((10, 0, 5), 6 * SLOT_MS);
        assert_eq!(ring.buckets().collect::<Vec<_>>(), [0, 34, 0, 0, 0, 0]);
        // A counter that went backwards is a fresh connection.
        ring.sample((1, 0, 0), 7 * SLOT_MS);
        assert!(ring.buckets().next().is_none());
        assert_eq!(ring.recent(), None);
    }

    #[test]
    fn the_ring_keeps_ten_minutes() {
        let mut ring = Ring::default();
        for slot in 0..=SLOTS as u64 + 5 {
            ring.sample((slot as u32 * 10, 0, 0), slot * SLOT_MS);
        }
        assert_eq!(ring.buckets().count(), SLOTS);
        assert_eq!(ring.minutes(), 10);
        assert_eq!(
            ring.label("Received"),
            "Received: no loss in the last 10 minutes."
        );
    }

    #[test]
    fn the_bucket_still_counting_closes_the_strip() {
        let mut ring = Ring::default();
        ring.sample((0, 0, 0), 0);
        ring.sample((995, 0, 5), SLOT_MS / 2);
        assert_eq!(ring.bars().collect::<Vec<_>>(), [1]);
        assert_eq!(ring.recent(), Some(1));
        assert_eq!(ring.health(), Health::Lossy);
    }

    #[test]
    fn the_label_names_the_recent_and_the_worst_loss() {
        // A tenth lost a few slots back, nothing since.
        let ring = ring_of(&[(0, 0, 0), (90, 0, 10), (190, 0, 10), (290, 0, 10)]);
        assert_eq!(ring.worst(), Some(10));
        assert_eq!(
            ring.label("Sent"),
            "Sent: 10% in the last minute, 10% at worst in the last minute."
        );
        assert_eq!(
            Ring::default().label("Sent"),
            "Sent: no packets counted yet."
        );
    }
}
