// SPDX-License-Identifier: EUPL-1.2
//! Per-speaker adaptive jitter policy with an explicit monotonic clock.
//! No device, codec, network, platform clock or external dependency is used.
//!
//! Mumble numbers voice frames in 10 ms units, so a 20 ms Opus packet advances
//! the sequence by two. Packets are held until a short, adaptive target delay is
//! buffered, then played in order. Gaps are filled by Opus in-band FEC when the
//! following packet carries it, otherwise by packet-loss concealment. The
//! native policy drops excess buffering. Browser playout adjusts PCM duration
//! against its percentile target.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::time::Duration;

pub const FRAME_MS: f32 = 10.;
const MIN_TARGET_MS: f32 = 40.;
const MAX_TARGET_MS: f32 = 200.;
/// Excess above the target that is dropped to catch up.
pub const CATCH_UP_MS: f32 = 120.;
/// Concealment beyond this ends playout until enough audio is buffered again.
const MAX_CONCEALED_FRAMES: u32 = 10;
const MAX_PACKETS: usize = 64;
/// Sequence numbers count 10 ms frames; this many take billions of years. Any
/// sender (a server relays them unchanged) picks them, and room above them
/// keeps the arithmetic on them from overflowing.
pub const MAX_SEQUENCE: u64 = u64::MAX / 2;

#[derive(Debug, PartialEq, Eq)]
pub enum Pull<P = Vec<u8>> {
    /// Decode this packet; it covers `frames` 10 ms frames.
    Packet { opus: P, end: bool },
    /// The packet for the next slot is missing; recover it from `fec` (the
    /// following packet's in-band copy) when available, else conceal.
    Lost { fec: Option<P>, frames: u64 },
    /// Nothing to play: waiting for the initial target or after an underrun.
    Idle,
}

/// What a buffer did with the packets it was given, taken by the caller that
/// reports diagnostics (issue #17) and then cleared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Packets accepted for playout.
    pub received: u32,
    /// Packets that arrived after their slot had played.
    pub late: u32,
    /// Packets whose slot was already filled.
    pub duplicate: u32,
    /// Packets dropped to catch up with the target delay.
    pub caught_up: u32,
}

struct Packet<P> {
    opus: P,
    end: bool,
}

pub struct JitterBuffer<P = Vec<u8>> {
    packets: BTreeMap<u64, Packet<P>>,
    next: Option<u64>,
    /// Origin of the packet just pulled, until its decoded duration is known.
    pulled: Option<u64>,
    playing: bool,
    concealed: u32,
    frames_per_packet: u64,
    delay: Delay,
    counters: Counters,
}

impl<P> Default for JitterBuffer<P> {
    fn default() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            pulled: None,
            playing: false,
            concealed: 0,
            frames_per_packet: 2,
            delay: Delay::Native {
                jitter_ms: 0.,
                last_arrival: None,
            },
            counters: Counters::default(),
        }
    }
}

enum Delay {
    Native {
        jitter_ms: f32,
        last_arrival: Option<(Duration, u64)>,
    },
    Adaptive(ArrivalDelay),
}

impl Delay {
    fn observe(&mut self, now: Duration, sequence: u64) {
        match self {
            Self::Native {
                jitter_ms,
                last_arrival,
            } => {
                if let Some((arrived, previous)) = *last_arrival
                    && sequence > previous
                {
                    // RFC 3550 interarrival jitter, in milliseconds.
                    let expected = (sequence - previous) as f32 * FRAME_MS;
                    let actual = now.saturating_sub(arrived).as_secs_f32() * 1000.;
                    let deviation = (actual - expected).abs().min(1000.);
                    *jitter_ms += (deviation - *jitter_ms) / 16.;
                }
                if last_arrival.is_none_or(|(_, previous)| sequence > previous) {
                    *last_arrival = Some((now, sequence));
                }
            }
            Self::Adaptive(delay) => delay.observe(now, sequence),
        }
    }

    fn reserve_ms(&self) -> f32 {
        match self {
            Self::Native { jitter_ms, .. } => 2.5 * jitter_ms,
            Self::Adaptive(delay) => delay.percentile_ms(),
        }
    }
}

const ARRIVAL_BUCKETS: usize = 17; // 0..160 ms above the arrival baseline.

#[derive(Default)]
struct ArrivalDelay {
    // Minimum clock offset in the current and previous one-second windows.
    baseline: Option<ArrivalBaseline>,
    buckets: [f32; ARRIVAL_BUCKETS],
    samples: u16,
}

struct ArrivalBaseline {
    second: u64,
    current_min: i128,
    previous_min: i128,
}

impl ArrivalDelay {
    fn observe(&mut self, now: Duration, sequence: u64) {
        let offset = now.as_micros() as i128 - i128::from(sequence) * 10_000;
        let second = now.as_secs();
        let baseline = self.baseline.get_or_insert(ArrivalBaseline {
            second,
            current_min: offset,
            previous_min: offset,
        });
        if second > baseline.second {
            baseline.previous_min = if second - baseline.second == 1 {
                baseline.current_min
            } else {
                offset
            };
            baseline.second = second;
            baseline.current_min = offset;
        }
        baseline.current_min = baseline.current_min.min(offset);
        let extra_us = offset - baseline.current_min.min(baseline.previous_min);
        // Round up so the estimate covers the upper edge of its bucket.
        let bucket = ((extra_us + 9_999) / 10_000).min((ARRIVAL_BUCKETS - 1) as i128) as usize;
        // The first samples have equal weight. Once established, the histogram
        // forgets 1/1000 per packet, with a 7–14 s half-life at 50–100 pps.
        self.samples = (self.samples + 1).min(1000);
        let weight = 1. / f32::from(self.samples);
        for count in &mut self.buckets {
            *count *= 1. - weight;
        }
        self.buckets[bucket] += weight;
    }

    fn percentile_ms(&self) -> f32 {
        let mut tail = 0.;
        for (bucket, count) in self.buckets.iter().enumerate().rev() {
            tail += count;
            if tail > 0.05 {
                return bucket as f32 * FRAME_MS;
            }
        }
        0.
    }
}

impl<P: Clone> JitterBuffer<P> {
    /// Percentile-based playout leaves catch-up to the caller's time stretcher.
    pub fn adaptive() -> Self {
        Self {
            delay: Delay::Adaptive(ArrivalDelay::default()),
            ..Self::default()
        }
    }

    pub fn target_ms(&self) -> f32 {
        (self.frames_per_packet as f32 * FRAME_MS + self.delay.reserve_ms() + 10.)
            .clamp(MIN_TARGET_MS, MAX_TARGET_MS)
    }

    /// Audio buffered ahead of the playout position, in milliseconds.
    pub fn buffered_ms(&self) -> f32 {
        let (Some(&first), Some(&last)) =
            (self.packets.keys().next(), self.packets.keys().next_back())
        else {
            return 0.;
        };
        let start = self.next.map_or(first, |next| next.min(first));
        (last + self.frames_per_packet).saturating_sub(start) as f32 * FRAME_MS
    }

    /// The decoder reports how many 10 ms frames a packet really carried.
    pub fn observe_frames(&mut self, frames: u64) {
        if (1..=12).contains(&frames) {
            self.frames_per_packet = frames;
            if let Some(origin) = self.pulled.take() {
                // pull() had to predict the next tick before decoding. Correct
                // from the packet origin, rather than inserting phantom PLC.
                self.next = Some(origin + frames);
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// The counters so far, cleared for the next window. Saturating, so a
    /// caller that forgets a window still reports the work that was done.
    pub fn take_counters(&mut self) -> Counters {
        std::mem::take(&mut self.counters)
    }

    /// Whether a continuous packet is queued after the current one.
    pub fn has_next_packet(&self) -> bool {
        self.next
            .is_some_and(|next| self.packets.contains_key(&next))
    }

    fn stop(&mut self) {
        self.playing = false;
        self.next = None;
        self.pulled = None;
        if let Delay::Adaptive(delay) = &mut self.delay {
            // A talkspurt can restart the sender clock after silence.
            delay.baseline = None;
        }
    }

    pub fn push(&mut self, sequence: u64, opus: P, end: bool, now: Duration) {
        if sequence > MAX_SEQUENCE {
            return;
        }
        if self.next.is_some_and(|next| sequence < next) {
            self.counters.late = self.counters.late.saturating_add(1);
            return;
        }
        if self.packets.contains_key(&sequence) {
            self.counters.duplicate = self.counters.duplicate.saturating_add(1);
            return;
        }
        self.counters.received = self.counters.received.saturating_add(1);
        // Packet spacing is the smallest gap to a neighbour; the decoder
        // corrects it from the actual packet duration via `observe_frames`.
        let before = self
            .packets
            .range(..sequence)
            .next_back()
            .map(|(previous, _)| sequence - previous);
        let after = self
            .packets
            .range(sequence + 1..)
            .next()
            .map(|(next, _)| next - sequence);
        if let Some(spacing) = before.into_iter().chain(after).min()
            && (1..=12).contains(&spacing)
            && spacing < self.frames_per_packet
        {
            self.frames_per_packet = spacing;
        }
        // A terminator may be sent immediately after the last voice packet.
        if !end || matches!(self.delay, Delay::Native { .. }) {
            self.delay.observe(now, sequence);
        }
        self.packets.insert(sequence, Packet { opus, end });
        while self.packets.len() > MAX_PACKETS {
            self.packets.pop_first();
        }
    }

    /// The next action for the playout clock, which asks whenever this
    /// speaker's decoded audio runs low.
    pub fn pull(&mut self) -> Pull<P> {
        self.pulled = None;
        if !self.playing {
            let end_seen = self.packets.values().any(|packet| packet.end);
            if self.packets.is_empty() || (self.buffered_ms() < self.target_ms() && !end_seen) {
                return Pull::Idle;
            }
            self.playing = true;
            self.concealed = 0;
            self.next = self
                .packets
                .first_key_value()
                .map(|(sequence, _)| *sequence);
        }
        // Native playout catches up by dropping excess queued packets.
        while matches!(self.delay, Delay::Native { .. })
            && self.buffered_ms() > self.target_ms() + CATCH_UP_MS
            && self.packets.len() > 1
        {
            if let Some((sequence, _)) = self.packets.pop_first() {
                self.next = Some(sequence + self.frames_per_packet);
                self.counters.caught_up = self.counters.caught_up.saturating_add(1);
            }
        }
        let Some(next) = self.next else {
            self.stop();
            return Pull::Idle;
        };
        if let Some(packet) = self.packets.remove(&next) {
            self.concealed = 0;
            self.next = Some(next + self.frames_per_packet);
            self.pulled = Some(next);
            if packet.end {
                self.stop();
            }
            return Pull::Packet {
                opus: packet.opus,
                end: packet.end,
            };
        }
        // Missing slot: conceal briefly, then wait to rebuffer.
        self.concealed += 1;
        if self.concealed > MAX_CONCEALED_FRAMES || (self.packets.is_empty() && self.concealed > 2)
        {
            self.stop();
            return Pull::Idle;
        }
        // Packet duration can change. Never conceal past the next known
        // packet merely because the preceding packet happened to be longer.
        let frames = self
            .packets
            .range(next + 1..)
            .next()
            .map_or(self.frames_per_packet, |(following, _)| {
                self.frames_per_packet.min(following - next)
            });
        self.next = Some(next + frames);
        let fec = self
            .packets
            .get(&(next + frames))
            .map(|packet| packet.opus.clone());
        Pull::Lost { fec, frames }
    }
}

/// The edges of an underrun (issue #206). When playout runs dry mid-waveform,
/// the caller conceals 10 ms more and fades it to silence; whatever plays next
/// fades in. Every other sample, concealment choice and gain is unchanged.
#[derive(Default)]
pub struct UnderrunFade {
    faded: bool,
}

impl UnderrunFade {
    /// Whether a pull that returned `Idle` ran dry mid-waveform, given whether
    /// the buffer was playing before it.
    pub fn ran_dry(&self, was_playing: bool) -> bool {
        was_playing && !self.faded
    }

    /// The 10 ms of concealment that continue a stream that ran dry.
    pub fn fade_out(&mut self, pcm: &mut [f32]) {
        ramp(pcm, false);
        self.faded = true;
    }

    /// Decoded or concealed stereo audio about to play.
    pub fn play(&mut self, pcm: &mut [f32]) {
        if std::mem::take(&mut self.faded) {
            ramp(pcm, true);
        }
    }

    /// Whether a terminator's audio plays: straight after an underrun it only
    /// ends audio that has already faded out.
    pub fn end(&mut self) -> bool {
        !std::mem::take(&mut self.faded)
    }
}

/// Raised-cosine ramp over the first 10 ms (480 frames at 48 kHz) of
/// interleaved stereo: up from silence when `rising`, else down to it.
fn ramp(pcm: &mut [f32], rising: bool) {
    for (index, frame) in pcm.as_chunks_mut::<2>().0.iter_mut().take(480).enumerate() {
        let up = 0.5 - 0.5 * (std::f32::consts::PI * (index as f32 + 0.5) / 480.).cos();
        let gain = if rising { up } else { 1. - up };
        frame[0] *= gain;
        frame[1] *= gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_packet_loss_stops_at_the_nearer_10_or_20_ms_future_packet() {
        for gap in [1, 2] {
            let mut buffer = JitterBuffer::default();
            // Arrival reorder must not change the exact missing sequence span.
            buffer.push(12 + gap, vec![20], true, Duration::ZERO);
            buffer.push(0, vec![120], false, Duration::from_millis(1));
            assert!(matches!(buffer.pull(), Pull::Packet { opus, .. } if opus == [120]));
            buffer.observe_frames(12);
            assert_eq!(
                buffer.pull(),
                Pull::Lost {
                    fec: Some(vec![20]),
                    frames: gap
                }
            );
            assert!(matches!(buffer.pull(), Pull::Packet { opus, end:true } if opus == [20]));
            buffer.observe_frames(2);
            assert_eq!(buffer.pull(), Pull::Idle);
        }
    }

    #[test]
    fn decoded_long_and_changing_durations_advance_from_the_packet_origin() {
        let mut buffer = JitterBuffer::default();
        let now = Duration::ZERO;
        buffer.push(0, vec![1], false, now);
        buffer.push(12, vec![2], false, now);
        buffer.push(14, vec![3], true, now);
        assert!(matches!(buffer.pull(), Pull::Packet { opus, .. } if opus == [1]));
        buffer.observe_frames(12);
        assert!(matches!(buffer.pull(), Pull::Packet { opus, .. } if opus == [2]));
        buffer.observe_frames(2);
        assert!(matches!(buffer.pull(), Pull::Packet { opus, end:true } if opus == [3]));
        buffer.observe_frames(12);
        assert_eq!(
            buffer.pull(),
            Pull::Idle,
            "terminator must not reopen next tick"
        );
    }

    #[test]
    fn sequence_numbers_near_the_end_of_their_range_are_ignored() {
        // Found writing the voice_playback fuzz target: the next slot and the
        // buffered length overflowed (a panic with overflow checks, a garbled
        // stream without).
        let mut buffer = JitterBuffer::default();
        let start = Duration::ZERO;
        for index in 0..4_u64 {
            let at = start + Duration::from_millis(20 * index);
            buffer.push(u64::MAX - 7 + 2 * index, vec![1], index == 3, at);
        }
        assert_eq!(buffer.buffered_ms(), 0.);
        assert_eq!(buffer.pull(), Pull::Idle);
        buffer.push(MAX_SEQUENCE, vec![2], true, start);
        assert!(matches!(buffer.pull(), Pull::Packet { end: true, .. }));
    }

    fn feed(buffer: &mut JitterBuffer, start: Duration, sequences: &[(u64, u64)]) {
        for (sequence, at_ms) in sequences {
            buffer.push(
                *sequence,
                vec![*sequence as u8],
                false,
                start + Duration::from_millis(*at_ms),
            );
        }
    }

    #[test]
    fn waits_for_the_target_then_plays_in_order() {
        let start = Duration::ZERO;
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(10, 0)]);
        assert_eq!(buffer.pull(), Pull::Idle, "prebuffering");
        feed(&mut buffer, start, &[(12, 20), (14, 40)]);
        for expected in [10, 12, 14] {
            assert_eq!(
                buffer.pull(),
                Pull::Packet {
                    opus: vec![expected],
                    end: false
                }
            );
        }
    }

    #[test]
    fn reorders_recovers_with_fec_and_conceals_gaps() {
        let start = Duration::ZERO;
        let mut buffer = JitterBuffer::default();
        // 4 arrives before 2; 6 is lost; 8 carries FEC for 6.
        feed(
            &mut buffer,
            start,
            &[(0, 0), (4, 20), (2, 25), (8, 60), (10, 80)],
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![0],
                end: false
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![2],
                end: false
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![4],
                end: false
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Lost {
                fec: Some(vec![8]),
                frames: 2
            }
        );
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![8],
                end: false
            }
        );
    }

    #[test]
    fn drops_late_and_duplicate_packets_and_stops_at_the_terminator() {
        let start = Duration::ZERO;
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(0, 0), (2, 20), (4, 40)]);
        buffer.push(6, vec![6], true, start + Duration::from_millis(60));
        assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        // A late packet and a duplicate (with different contents) are ignored.
        buffer.push(0, vec![0], false, start + Duration::from_millis(70));
        buffer.push(4, vec![44], false, start + Duration::from_millis(70));
        for expected in [2, 4] {
            assert_eq!(
                buffer.pull(),
                Pull::Packet {
                    opus: vec![expected],
                    end: false
                }
            );
        }
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![6],
                end: true
            }
        );
        assert!(!buffer.is_playing());
        assert_eq!(buffer.pull(), Pull::Idle);
    }

    #[test]
    fn underrun_rebuffers_instead_of_concealing_forever() {
        let start = Duration::ZERO;
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(0, 0), (2, 20), (4, 40)]);
        for _ in 0..3 {
            assert!(matches!(buffer.pull(), Pull::Packet { .. }));
        }
        let mut lost = 0;
        while let Pull::Lost { .. } = buffer.pull() {
            lost += 1;
        }
        assert!(lost <= 2 && !buffer.is_playing());
    }

    #[test]
    fn adaptive_target_learns_relative_delay_and_forgets_old_spikes() {
        let mut delay = ArrivalDelay::default();
        // Exercise the exact offset even near the maximum sequence, and a
        // baseline which follows the sender's steady clock.
        let base = MAX_SEQUENCE - 20_000;
        for tick in 0..1000 {
            delay.observe(Duration::from_millis(tick * 20), base + tick * 2);
        }
        assert_eq!(delay.percentile_ms(), 0.);
        for tick in 1000..1500 {
            let tardy = 15 * (tick % 2);
            delay.observe(Duration::from_millis(tick * 20 + tardy), base + tick * 2);
        }
        assert_eq!(delay.percentile_ms(), 20.);
        for tick in 1500..1600 {
            delay.observe(Duration::from_millis(tick * 20), base + tick * 2);
        }
        assert_eq!(delay.percentile_ms(), 20.);
        for tick in 1600..3500 {
            delay.observe(Duration::from_millis(tick * 20), base + tick * 2);
        }
        assert_eq!(delay.percentile_ms(), 0.);
    }

    #[test]
    fn adaptive_pull_keeps_every_packet_within_the_admission_bound() {
        let mut buffer = JitterBuffer::adaptive();
        for index in 0..50 {
            buffer.push(index * 2, index, index == 49, Duration::ZERO);
        }
        assert!(buffer.buffered_ms() > buffer.target_ms() + CATCH_UP_MS);
        for expected in 0..50 {
            assert_eq!(
                buffer.pull(),
                Pull::Packet {
                    opus: expected,
                    end: expected == 49,
                }
            );
        }
        assert_eq!(buffer.pull(), Pull::Idle);
    }

    #[test]
    fn jitter_raises_the_target_and_excess_is_dropped() {
        let start = Duration::ZERO;
        let mut steady: JitterBuffer = JitterBuffer::default();
        let mut jittery: JitterBuffer = JitterBuffer::default();
        for index in 0..50_u64 {
            steady.push(
                index * 2,
                vec![],
                false,
                start + Duration::from_millis(index * 20),
            );
            let wobble = if index % 2 == 0 { 0 } else { 35 };
            jittery.push(
                index * 2,
                vec![],
                false,
                start + Duration::from_millis(index * 20 + wobble),
            );
        }
        assert!(jittery.target_ms() > steady.target_ms() + 20.);
        // A burst of 50 packets (1 s) collapses to about target + catch-up window.
        let mut burst: JitterBuffer = JitterBuffer::default();
        for index in 0..50_u64 {
            burst.push(index * 2, vec![], false, start);
        }
        assert!(matches!(burst.pull(), Pull::Packet { .. }));
        assert!(burst.buffered_ms() <= burst.target_ms() + CATCH_UP_MS + 20.);
    }
    #[test]
    fn counters_separate_late_and_duplicate_packets_and_clear_on_take() {
        let start = Duration::ZERO;
        let mut buffer = JitterBuffer::default();
        feed(&mut buffer, start, &[(0, 0), (2, 20), (4, 40)]);
        assert_eq!(
            buffer.pull(),
            Pull::Packet {
                opus: vec![0],
                end: false
            }
        );
        // Slot 0 has played and 2 is queued: both are refused, differently.
        buffer.push(0, vec![9], false, start + Duration::from_millis(30));
        buffer.push(2, vec![9], false, start + Duration::from_millis(30));
        assert_eq!(
            buffer.take_counters(),
            Counters {
                received: 3,
                late: 1,
                duplicate: 1,
                caught_up: 0
            }
        );
        assert_eq!(buffer.take_counters(), Counters::default());
    }

    #[test]
    fn counters_report_the_packets_a_burst_drops_to_catch_up() {
        let mut buffer: JitterBuffer = JitterBuffer::default();
        for index in 0..50_u64 {
            buffer.push(index * 2, vec![], false, Duration::ZERO);
        }
        let mut caught_up = 0;
        while buffer.buffered_ms() > buffer.target_ms() + CATCH_UP_MS {
            assert!(matches!(buffer.pull(), Pull::Packet { .. }));
            caught_up += buffer.take_counters().caught_up;
        }
        assert!(caught_up > 0, "a burst above the target drops packets");
    }
}
