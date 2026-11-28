// SPDX-License-Identifier: EUPL-1.2
//! Packet loss over time: what a connection's cumulative counters say, cut into
//! buckets, so a long call shows whether packets went missing now or an hour
//! ago.
use std::collections::VecDeque;

/// One bucket counts this long.
pub const SLOT_MS: u64 = 20_000;
/// The buckets one ring keeps: ten minutes of them.
pub const SLOTS: usize = 30;

/// What the last minute says about a direction's health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// No packet went missing.
    Good,
    /// Some did.
    Lossy,
    /// A tenth or more did.
    Poor,
}

impl Health {
    /// What a loss in percent says: a tenth or more is poor, anything else
    /// above nothing is lossy.
    pub fn of(percent: u8) -> Self {
        match percent {
            0 => Self::Good,
            1..=9 => Self::Lossy,
            _ => Self::Poor,
        }
    }
}

/// Cumulative packet counters of one direction, as the connection and the
/// server report them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub good: u32,
    pub late: u32,
    pub lost: u32,
}

impl Counts {
    pub fn new(good: u32, late: u32, lost: u32) -> Self {
        Self { good, late, lost }
    }

    /// Every packet counted, lost or not.
    pub fn packets(self) -> u32 {
        self.good
            .saturating_add(self.late)
            .saturating_add(self.lost)
    }
}

/// Both directions of one server's voice, over time: what arrived, and what
/// the server counted of ours.
#[derive(Default)]
pub struct Loss {
    /// The connection these rings count; a new one starts them over.
    generation: Option<u64>,
    pub received: Ring,
    pub sent: Ring,
}

impl Loss {
    /// Take both directions' cumulative counters at `now_ms`. A different
    /// connection than the rings last saw starts them over.
    pub fn sample(&mut self, generation: u64, received: Counts, sent: Counts, now_ms: u64) {
        if self.generation != Some(generation) {
            *self = Self {
                generation: Some(generation),
                ..Self::default()
            };
        }
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
    /// The packets of each finished bucket, oldest first.
    done: VecDeque<Counts>,
    /// When the bucket being counted started, and what it holds.
    since: Option<u64>,
    counted: Counts,
    /// The counters the last sample saw.
    last: Option<Counts>,
}

impl Ring {
    /// Take the cumulative counters at `now_ms`. Counters that fell back belong
    /// to a fresh key, so the sample becomes the new base and the buckets kept
    /// stay. What arrived since the last sample counts toward the bucket the
    /// sample falls in; a gap longer than a bucket leaves the buckets in
    /// between empty.
    pub fn sample(&mut self, counters: Counts, now_ms: u64) {
        let Some(last) = self.last else {
            self.since = Some(now_ms);
            self.last = Some(counters);
            return;
        };
        let Some(deltas) = counters.minus(last) else {
            self.last = Some(counters);
            return;
        };
        self.last = Some(counters);
        let mut since = *self.since.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) >= SLOT_MS {
            let elapsed = (now_ms - since) / SLOT_MS;
            self.close();
            for _ in 1..elapsed.min(SLOTS as u64) {
                self.done.push_back(Counts::default());
            }
            self.trim();
            since += elapsed * SLOT_MS;
        }
        self.since = Some(since);
        self.counted.good = self.counted.good.saturating_add(deltas.good);
        self.counted.late = self.counted.late.saturating_add(deltas.late);
        self.counted.lost = self.counted.lost.saturating_add(deltas.lost);
    }

    /// Finish the bucket being counted and keep it, dropping the oldest.
    fn close(&mut self) {
        self.done.push_back(self.counted);
        self.counted = Counts::default();
        self.trim();
    }

    fn trim(&mut self) {
        while self.done.len() > SLOTS {
            self.done.pop_front();
        }
    }

    /// The loss of each finished bucket, in percent, oldest first.
    #[cfg(test)]
    pub fn buckets(&self) -> impl Iterator<Item = u8> + '_ {
        self.done.iter().copied().map(loss)
    }

    /// The finished buckets followed by the one still counting, so a panel
    /// opened early shows the loss of this moment at its right edge.
    pub fn bars(&self) -> impl Iterator<Item = u8> + '_ {
        // The running bucket takes the oldest one's place, so the strip keeps
        // its width once the ring is full.
        let keep = SLOTS.saturating_sub(1).min(self.done.len());
        let running = (self.counted.packets() > 0).then(|| loss(self.counted));
        self.done
            .iter()
            .skip(self.done.len() - keep)
            .copied()
            .map(loss)
            .chain(running)
    }

    /// How long the finished buckets reach back, in whole minutes.
    pub fn minutes(&self) -> u64 {
        (self.done.len() as u64 * SLOT_MS / 60_000).max(1)
    }

    /// The loss of the last minute as one percentage: a single lost packet in a
    /// quiet bucket says nothing on its own, the minute it belongs to does.
    pub fn recent(&self) -> Option<u8> {
        let window = (60_000 / SLOT_MS) as usize;
        let finished = self
            .done
            .iter()
            .skip(self.done.len().saturating_sub(window));
        let counted = finished.fold(self.counted, |total, bucket| Counts {
            good: total.good.saturating_add(bucket.good),
            late: total.late.saturating_add(bucket.late),
            lost: total.lost.saturating_add(bucket.lost),
        });
        (counted.packets() > 0).then(|| loss(counted))
    }

    /// The worst loss of every bucket kept, in percent.
    pub fn worst(&self) -> Option<u8> {
        let running = (self.counted.packets() > 0).then(|| loss(self.counted));
        self.done.iter().copied().map(loss).chain(running).max()
    }

    /// What the last minute's loss says about this direction.
    pub fn health(&self) -> Health {
        self.recent().map_or(Health::Good, Health::of)
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

impl Counts {
    /// What this sample counted beyond `seen`, or nothing when the counters
    /// went backwards.
    fn minus(self, seen: Self) -> Option<Self> {
        Some(Self {
            good: self.good.checked_sub(seen.good)?,
            late: self.late.checked_sub(seen.late)?,
            lost: self.lost.checked_sub(seen.lost)?,
        })
    }
}

/// The loss of `counted` in percent, rounded up so one lost packet of a
/// thousand reads as 1% rather than nothing.
fn loss(counted: Counts) -> u8 {
    let packets = counted.packets();
    if packets == 0 || counted.lost == 0 {
        return 0;
    }
    ((u64::from(counted.lost) * 100)
        .div_ceil(u64::from(packets))
        .min(100)) as u8
}

#[cfg(test)]
mod tests {
    use super::{Counts, Health, Loss, Ring, SLOT_MS, SLOTS};

    /// A ring that saw `counters` at the start of each slot, closed by one more
    /// sample at the end of the last.
    fn ring_of(slots: &[(u32, u32, u32)]) -> Ring {
        let mut ring = Ring::default();
        for (index, counters) in slots.iter().enumerate() {
            ring.sample(
                Counts::new(counters.0, counters.1, counters.2),
                index as u64 * SLOT_MS,
            );
        }
        ring.sample(
            slots
                .last()
                .map_or(Counts::default(), |c| Counts::new(c.0, c.1, c.2)),
            slots.len() as u64 * SLOT_MS,
        );
        ring
    }

    #[test]
    fn each_slot_becomes_a_bucket_when_the_next_sample_closes_it() {
        let mut ring = Ring::default();
        // The first sample only sets the counters that the next ones compare to.
        ring.sample(Counts::new(0, 0, 0), 0);
        ring.sample(Counts::new(100, 0, 0), SLOT_MS);
        assert_eq!(ring.buckets().collect::<Vec<_>>(), [0]);
        // A hundred good packets, then a hundred with a tenth lost.
        ring.sample(Counts::new(190, 0, 10), 2 * SLOT_MS);
        ring.sample(Counts::new(190, 0, 10), 3 * SLOT_MS);
        assert_eq!(ring.buckets().collect::<Vec<_>>(), [0, 0, 10]);
        // The minute holds a hundred quiet packets too, so its loss is five
        // percent even though the bucket behind it lost a tenth.
        assert_eq!(ring.recent(), Some(5));
        assert_eq!(ring.worst(), Some(10));
        assert_eq!(ring.health(), Health::Lossy);
        assert_eq!(
            ring.label("Received"),
            "Received: 5% in the last minute, 10% at worst in the last minute."
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
    fn a_long_gap_leaves_empty_buckets_and_a_reset_keeps_them() {
        let mut ring = Ring::default();
        ring.sample(Counts::new(0, 0, 0), 0);
        ring.sample(Counts::new(10, 0, 5), SLOT_MS);
        // Five minutes without a sample: the buckets in between stay empty.
        ring.sample(Counts::new(10, 0, 5), 6 * SLOT_MS);
        assert_eq!(ring.buckets().collect::<Vec<_>>(), [0, 34, 0, 0, 0, 0]);
        // Counters that fell back are a fresh key: the buckets stay.
        ring.sample(Counts::new(1, 0, 0), 7 * SLOT_MS);
        assert_eq!(ring.buckets().count(), 6);
    }

    #[test]
    fn the_ring_keeps_ten_minutes() {
        let mut ring = Ring::default();
        for slot in 0..=SLOTS as u64 + 5 {
            ring.sample(Counts::new(slot as u32 * 10, 0, 0), slot * SLOT_MS);
        }
        assert_eq!(ring.buckets().count(), SLOTS);
        assert_eq!(ring.bars().count(), SLOTS);
        assert_eq!(ring.minutes(), 10);
        assert_eq!(
            ring.label("Received"),
            "Received: no loss in the last 10 minutes."
        );
    }

    #[test]
    fn the_bucket_still_counting_closes_the_strip() {
        let mut ring = Ring::default();
        ring.sample(Counts::new(0, 0, 0), 0);
        ring.sample(Counts::new(995, 0, 5), SLOT_MS / 2);
        assert_eq!(ring.bars().collect::<Vec<_>>(), [1]);
        assert_eq!(ring.recent(), Some(1));
        assert_eq!(ring.health(), Health::Lossy);
    }

    #[test]
    fn a_quarter_of_a_connection_is_one_from_the_other() {
        let mut ring = Ring::default();
        ring.sample(Counts::new(0, 0, 0), 0);
        ring.sample(Counts::new(2, 0, 0), SLOT_MS);
        // Two lost packets of nearly two thousand in the minute: a percent, not
        // the tenth a single quiet bucket would claim.
        ring.sample(Counts::new(1875, 0, 2), 2 * SLOT_MS);
        assert_eq!(ring.recent(), Some(1));
        assert_eq!(ring.health(), Health::Lossy);
    }

    #[test]
    fn the_worse_direction_decides_the_signal() {
        let mut loss = Loss::default();
        loss.sample(1, Counts::new(0, 0, 0), Counts::new(0, 0, 0), 0);
        loss.sample(
            1,
            Counts::new(900, 0, 100),
            Counts::new(500, 0, 0),
            2 * SLOT_MS,
        );
        assert_eq!(loss.received.health(), Health::Poor);
        assert_eq!(loss.sent.health(), Health::Good);
        assert_eq!(loss.health(), Health::Poor);
        // A new connection starts over.
        loss.sample(2, Counts::new(10, 0, 0), Counts::new(10, 0, 0), 3 * SLOT_MS);
        assert_eq!(loss.received.buckets().count(), 0);
        assert_eq!(loss.health(), Health::Good);
    }

    #[test]
    fn the_label_names_the_recent_and_the_worst_loss() {
        // A tenth lost a few slots back, nothing since.
        let ring = ring_of(&[(0, 0, 0), (90, 0, 10), (190, 0, 10), (290, 0, 10)]);
        assert_eq!(ring.worst(), Some(10));
        assert_eq!(
            ring.label("Sent"),
            "Sent: 4% in the last minute, 10% at worst in the last minute."
        );
        assert_eq!(
            Ring::default().label("Sent"),
            "Sent: no packets counted yet."
        );
    }
}
