// SPDX-License-Identifier: EUPL-1.2
//! Per-receiver video quality (docs/video-plan.md §5) on `nuschl-stream-quality`.
//!
//! The AV1 Dependency Descriptor travels through str0m as a user header
//! extension on both legs, as the publisher wrote it.
//!
//! A receiver gets the frames of one decode target per layered source from
//! Q1's `Selector`, which switches at switch points whose chains the receiver
//! holds and marks the end of each temporal unit it forwards, with sequence
//! numbers of its own ([`Rewrite`]) so intentional layer skips leave no gaps.
//! When a receiver falls behind, its queue leaves out whole frames that its
//! target can do without and lowers its target; other frames go out whole.
//!
//! A receiver asks for a tier per source (`QualityRequest`) and states the
//! tiers it decodes (`DecodeCapacity`), each at most once a second. The tier it
//! gets is the least of both; until it asks, it asks for the top tier. Once a
//! second the publisher hears the resulting demand for each source.

use crate::protocol::{Mode, Prefer, VideoLimit};
use nuschl_stream_quality::{
    DecodeCapacity, Kind, Throttle,
    dd::{Frame, Layers, Pace, Selector},
    receive,
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues, StreamTxQueueInfo};

const MIN_QUEUE_BYTES: u64 = 32 * 1024;
const MAX_QUEUE_BYTES: u64 = 2 * 1024 * 1024;
/// How long a frame may wait in a receiver's queue before the queue leaves
/// frames out.
pub const QUEUE_BOUND: Duration = Duration::from_millis(300);
/// [`QUEUE_BOUND`] for a Low latency source.
pub const LOW_LATENCY_BOUND: Duration = Duration::from_millis(50);
/// The largest frame a publisher sends, an 8 MiB screen frame: a key picture's
/// temporal unit, and a recovery picture, up to this size goes out whole.
pub const MAX_KEY_BYTES: u64 = 8 * 1024 * 1024;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// A video source: its kind, its sender's mode and the demand last sent to its
/// publisher.
pub struct Video {
    pub kind: Kind,
    pub mode: Mode,
    /// The tier, height, frame rate, bitrate and limit last asked of it.
    pub demand: Option<(usize, u16, u8, u64, Option<VideoLimit>)>,
    pub throttle: Throttle,
    pub layers: Layers,
}

impl Video {
    pub fn new(kind: Kind, mode: Mode) -> Self {
        Self {
            kind,
            mode,
            demand: None,
            throttle: Throttle::default(),
            layers: Layers::default(),
        }
    }

    pub fn smooth(&self) -> bool {
        self.mode.prefer == Prefer::Smooth
    }

    /// How long its frames may wait in a receiver's queue.
    pub fn bound(&self) -> Duration {
        if self.mode.low_latency {
            LOW_LATENCY_BOUND
        } else {
            QUEUE_BOUND
        }
    }
}

/// One receiver's view of one video source.
#[derive(Default)]
pub struct View {
    /// The tier asked for.
    pub request: Option<usize>,
    pub throttle: Throttle,
    pub selector: Selector,
    /// Paces moves toward the receiver's share of the budgets.
    pub pace: Option<Pace>,
    /// The source's target list its selector's targets belong to.
    pub generation: Option<u32>,
    pub rewrite: Rewrite,
    pub backlog: Backlog,
    pub reported_limit: Option<VideoLimit>,
    pub limit_throttle: Throttle,
    pub reported_mode: Option<Mode>,
}

/// The video admitted to one receiver's send queue and its temporary quality limit.
#[derive(Default)]
pub struct Backlog {
    dropped_through: Option<u64>,
    /// The layered frame left out whole.
    skipping: Option<u16>,
    /// The timestamp of the latest temporal unit with a packet out.
    unit: Option<u64>,
    /// The bytes of the last key picture's temporal unit, and its timestamp
    /// while more of it is to come: the queue's bound grows by the time they
    /// take.
    key: (u64, Option<u64>),
    sampled_at: Option<Instant>,
    unsampled_bytes: usize,
    overloaded: Option<VideoLimit>,
    limit: Option<VideoLimit>,
    steps: usize,
    recover_at: Option<Instant>,
    recovery: Recovery,
}

#[derive(Clone, Copy, Default)]
enum Recovery {
    #[default]
    Live,
    Paused(Instant),
    Requested(u64),
    Picture {
        timestamp: u64,
        bytes: u64,
    },
}

pub enum VideoAdmission {
    Regular,
    Pause,
    RequestKeyframe,
    Burst(u64),
}

/// What happens to a packet of a layered frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Layered {
    Send,
    /// Its frame is left out whole.
    Skip,
}

impl Backlog {
    pub fn dropped(&self, timestamp: u64) -> bool {
        self.dropped_through.is_some_and(|last| timestamp <= last)
    }

    pub fn shed(&mut self, timestamp: u64, pause: Option<Instant>, limit: VideoLimit) {
        self.dropped_through = Some(timestamp.max(self.dropped_through.unwrap_or(0)));
        self.overloaded(limit);
        self.recovery = pause.map_or(Recovery::Live, |now| {
            Recovery::Paused(now + Duration::from_secs(1))
        });
    }

    pub fn overloaded(&mut self, limit: VideoLimit) {
        self.overloaded = Some(limit);
    }

    pub fn limit(&self) -> Option<VideoLimit> {
        self.limit.filter(|_| self.limited() || self.recovering())
    }

    fn queued_bytes(&mut self, queue: Option<&dyn StreamTxQueueInfo>) -> usize {
        let sampled_at = queue.map(StreamTxQueueInfo::created_at);
        if self.sampled_at != sampled_at {
            self.sampled_at = sampled_at;
            self.unsampled_bytes = 0;
        }
        queue
            .map_or(0, StreamTxQueueInfo::byte_size)
            .saturating_add(self.unsampled_bytes)
    }

    /// Whether `payload` more bytes take the queue of a source without a
    /// descriptor past about `bound`, beyond the last recovery picture.
    pub fn full(
        &mut self,
        queue: Option<&dyn StreamTxQueueInfo>,
        bitrate: u64,
        payload: usize,
        bound: Duration,
        now: Instant,
    ) -> bool {
        let bytes = u128::from(bitrate) * bound.as_millis() / 8000;
        let limit = u64::try_from(bytes)
            .unwrap_or(u64::MAX)
            .clamp(MIN_QUEUE_BYTES, MAX_QUEUE_BYTES);
        let age = queue
            .and_then(StreamTxQueueInfo::first_unsent)
            .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        let queued = self.queued_bytes(queue) as u64;
        if queued <= limit && age < bound && self.key.1.is_none() {
            self.key.0 = 0;
        }
        let picture = Duration::from_secs_f64(self.key.0 as f64 * 8.0 / bitrate.max(1) as f64);
        queued + payload as u64 > limit + self.key.0 || age >= bound + picture
    }

    /// Admits a packet of the layered `frame` by how long a frame starting now
    /// waits in the queue, less the time the last key picture's temporal unit
    /// takes. Past `bound` the receiver's target drops a step, and a frame its
    /// target can do without (`discardable`) is left out whole; past twice
    /// `bound` every frame but a key picture is, and the receiver waits for the
    /// next switch point. A temporal unit (at `timestamp`) with a frame out
    /// keeps its later frames: its last one carries the marker.
    #[allow(clippy::too_many_arguments)]
    pub fn layered(
        &mut self,
        frame: &Frame,
        timestamp: u64,
        discardable: bool,
        queue: Option<&dyn StreamTxQueueInfo>,
        bitrate: u64,
        bound: Duration,
        now: Instant,
    ) -> Layered {
        if frame.start {
            self.skipping = None;
            let queued = self.queued_bytes(queue) as u64;
            let age = queue
                .and_then(StreamTxQueueInfo::first_unsent)
                .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
            let time =
                |bytes: u64| Duration::from_secs_f64(bytes as f64 * 8.0 / bitrate.max(1) as f64);
            let delay = age.max(time(queued));
            // The grown bound lasts until the delay falls to half the bound:
            // the packets behind the key picture wait for it too.
            if delay <= bound / 2 && self.key.1.is_none() {
                self.key.0 = 0;
            }
            let delay = delay.saturating_sub(time(self.key.0));
            let key = frame.references().next().is_none();
            if key {
                self.key = (0, Some(timestamp));
            } else if delay > bound {
                self.overloaded(VideoLimit::ForwardingBacklog);
                if (discardable || delay > bound * 2) && self.unit != Some(timestamp) {
                    self.skipping = Some(frame.number);
                }
            }
        }
        if self.skipping == Some(frame.number) {
            Layered::Skip
        } else {
            Layered::Send
        }
    }

    /// Whether `payload` more bytes would take the queue past
    /// [`MAX_QUEUE_BYTES`] beyond the last key picture's temporal unit.
    pub fn capped(&mut self, queue: Option<&dyn StreamTxQueueInfo>, payload: usize) -> bool {
        let cap = MAX_QUEUE_BYTES + self.key.0.min(MAX_KEY_BYTES);
        self.queued_bytes(queue).saturating_add(payload) as u64 > cap
    }

    /// Whether a packet of the temporal unit at `timestamp` is out: the
    /// unit's later packets go out too, the last one with the marker.
    pub fn out(&self, timestamp: u64) -> bool {
        self.unit == Some(timestamp)
    }

    /// A packet of the temporal unit at `timestamp` went out. A late packet
    /// of an earlier unit leaves the latest unit.
    pub fn enqueued(&mut self, payload: usize, cost: u64, marker: bool, timestamp: u64) {
        self.unit = self.unit.max(Some(timestamp));
        self.unsampled_bytes += payload;
        if self.key.1 == Some(timestamp) {
            self.key = (self.key.0 + payload as u64, (!marker).then_some(timestamp));
        }
        if let Recovery::Picture { timestamp, bytes } = self.recovery {
            self.recovery = if marker {
                Recovery::Live
            } else {
                Recovery::Picture {
                    timestamp,
                    bytes: bytes + cost,
                }
            };
        }
    }

    pub fn recovering(&self) -> bool {
        !matches!(self.recovery, Recovery::Live)
    }

    /// Reserve the next picture after a PLI while the video queue drains and
    /// the configured one-second media credit replenishes.
    pub fn admission(
        &mut self,
        timestamp: u64,
        queue: Option<&dyn StreamTxQueueInfo>,
        can_request: bool,
        now: Instant,
    ) -> VideoAdmission {
        let recovery = self.recovery;
        match recovery {
            Recovery::Live => VideoAdmission::Regular,
            Recovery::Paused(until)
                if now >= until && can_request && self.queued_bytes(queue) == 0 =>
            {
                self.recovery = Recovery::Requested(timestamp);
                VideoAdmission::RequestKeyframe
            }
            Recovery::Requested(previous) if timestamp > previous => {
                self.recovery = Recovery::Picture {
                    timestamp,
                    bytes: 0,
                };
                self.key = (0, Some(timestamp));
                VideoAdmission::Burst(0)
            }
            Recovery::Picture {
                timestamp: stamp,
                bytes,
            } if timestamp == stamp => VideoAdmission::Burst(bytes),
            Recovery::Picture {
                timestamp: stamp, ..
            } if timestamp > stamp => {
                let limit = self.limit.unwrap_or(VideoLimit::ForwardingBacklog);
                self.shed(stamp, Some(now), limit);
                VideoAdmission::Pause
            }
            _ => VideoAdmission::Pause,
        }
    }

    /// Whether a recovery picture of `cost` bytes so far outgrows
    /// [`MAX_KEY_BYTES`].
    pub fn burst_full(&mut self, queue: Option<&dyn StreamTxQueueInfo>, cost: u64) -> bool {
        cost > MAX_KEY_BYTES || self.queued_bytes(queue) as u64 > MAX_KEY_BYTES
    }

    pub fn tick(&mut self, now: Instant) {
        if let Some(limit) = self.overloaded.take() {
            self.limit = Some(limit);
            self.steps = (self.steps + 1).min(Kind::Camera.top().max(Kind::Screen.top()));
            self.recover_at = Some(now + RECOVERY_INTERVAL);
        } else if self.recover_at.is_some_and(|at| now >= at) {
            self.steps = self.steps.saturating_sub(1);
            self.recover_at = (self.steps > 0).then_some(now + RECOVERY_INTERVAL);
        }
    }

    pub fn tier(&self, tier: usize) -> usize {
        tier.saturating_sub(self.steps)
    }

    /// How many steps a late queue lowers the receiver's quality.
    pub fn steps(&self) -> usize {
        self.steps
    }

    pub fn limited(&self) -> bool {
        self.steps > 0
    }
}

/// How far back [`Rewrite`] remembers skipped packets.
const REWRITE_WINDOW: u64 = 2048;

/// One receiver's sequence numbers for one source: the publisher's, from zero,
/// less the packets skipped before them. A packet skipped after a later one was
/// forwarded keeps its number, and the receiver sees it as lost.
#[derive(Default)]
pub struct Rewrite {
    base: Option<u64>,
    highest: Option<u64>,
    skipped: VecDeque<u64>,
    pruned: u64,
}

impl Rewrite {
    pub fn skip(&mut self, seq: u64) {
        self.base.get_or_insert(seq);
        if self.highest.is_some_and(|highest| seq <= highest) {
            return;
        }
        self.highest = Some(seq);
        self.skipped.push_back(seq);
        while self
            .skipped
            .front()
            .is_some_and(|&old| old + REWRITE_WINDOW < seq)
        {
            self.skipped.pop_front();
            self.pruned += 1;
        }
    }

    /// The receiver's number for a forwarded packet; `None` for one from before
    /// the first packet or older than the window.
    pub fn forward(&mut self, seq: u64) -> Option<u64> {
        let base = *self.base.get_or_insert(seq);
        let highest = self.highest.unwrap_or(seq);
        if seq < base || seq + REWRITE_WINDOW < highest {
            return None;
        }
        self.highest = Some(highest.max(seq));
        let skipped = self.pruned + self.skipped.partition_point(|&s| s < seq) as u64;
        Some(seq - base - skipped)
    }
}

/// Every tier of both kinds.
pub fn full_capacity() -> DecodeCapacity {
    DecodeCapacity {
        camera: Kind::Camera.top() as u8,
        screen: Kind::Screen.top() as u8,
    }
}

/// The tier a receiver gets of a source of `kind`, with the caps lowered by
/// `step` tiers (`traffic`).
pub fn tier(kind: Kind, view: &View, decode: DecodeCapacity, step: u64) -> (usize, VideoLimit) {
    let top = kind.top();
    let requested = view.request.unwrap_or(top).min(top);
    let policy = top.saturating_sub(usize::try_from(step).unwrap_or(usize::MAX));
    let decode = decode.get(kind);
    let tier = receive(requested, decode, top, policy);
    let limit = if tier == requested {
        VideoLimit::WithinTarget
    } else if tier == decode {
        VideoLimit::ReceiverDecode
    } else {
        VideoLimit::TrafficBudget
    };
    (tier, limit)
}

/// The descriptor's extension id in the SFU's offers; an offer from a peer
/// keeps its own id.
pub const DESCRIPTOR_ID: u8 = 12;

/// One packet's Dependency Descriptor, as it arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor(pub Vec<u8>);

#[derive(Debug)]
struct DescriptorSerializer;

impl ExtensionSerializer for DescriptorSerializer {
    fn write_to(&self, buf: &mut [u8], ev: &ExtensionValues) -> usize {
        match ev.user_values.get::<Descriptor>() {
            Some(Descriptor(bytes)) if bytes.len() <= buf.len() => {
                buf[..bytes.len()].copy_from_slice(bytes);
                bytes.len()
            }
            _ => 0,
        }
    }

    fn parse_value(&self, buf: &[u8], ev: &mut ExtensionValues) -> bool {
        ev.user_values.set(Descriptor(buf.to_vec()));
        true
    }

    fn is_video(&self) -> bool {
        true
    }

    fn is_audio(&self) -> bool {
        false
    }

    /// A descriptor with its template structure is longer than 16 bytes.
    fn requires_two_byte_form(&self, ev: &ExtensionValues) -> bool {
        ev.user_values
            .get::<Descriptor>()
            .is_some_and(|d| d.0.len() > 16)
    }
}

pub fn descriptor_extension() -> Extension {
    Extension::with_serializer(nuschl_stream_quality::dd::URI, DescriptorSerializer)
}

/// Header extension values for a forwarded packet: the descriptor, if any.
pub fn forwarded(received: &ExtensionValues) -> ExtensionValues {
    let mut values = ExtensionValues::default();
    if let Some(descriptor) = received.user_values.get::<Descriptor>() {
        values.user_values.set(descriptor.clone());
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Queue {
        oldest: Instant,
        bytes: usize,
    }

    impl StreamTxQueueInfo for Queue {
        fn created_at(&self) -> Instant {
            self.oldest
        }

        fn byte_size(&self) -> usize {
            self.bytes
        }

        fn packet_count(&self) -> usize {
            1
        }

        fn first_unsent(&self) -> Option<Instant> {
            Some(self.oldest)
        }
    }

    #[test]
    fn video_queue_bounds_bytes_and_age() {
        let now = Instant::now();
        let mut backlog = Backlog::default();
        let sample = Queue {
            oldest: now,
            bytes: 74_000,
        };
        assert!(!backlog.full(Some(&sample), 2_000_000, 1000, QUEUE_BOUND, now));
        backlog.enqueued(1000, 1000, false, 0);
        assert!(backlog.full(Some(&sample), 2_000_000, 1, QUEUE_BOUND, now));
        let late = now + QUEUE_BOUND;
        assert!(backlog.full(Some(&sample), 15_000_000, 1000, QUEUE_BOUND, late));
        assert!(!backlog.full(None, 2_000_000, 1000, QUEUE_BOUND, now));
        // A Low latency source's queue holds 50 ms, and at least 32 KiB.
        let mut backlog = Backlog::default();
        let small = Queue {
            oldest: now,
            bytes: 30_000,
        };
        let wide = Queue {
            oldest: now,
            bytes: 80_000,
        };
        let mut low = |queue: &Queue, bitrate, payload, ms| {
            let at = now + Duration::from_millis(ms);
            backlog.full(Some(queue), bitrate, payload, LOW_LATENCY_BOUND, at)
        };
        assert!(!low(&small, 2_000_000, 1000, 49));
        assert!(low(&small, 2_000_000, 1000, 50));
        assert!(low(&small, 2_000_000, 4000, 0));
        assert!(!low(&wide, 20_000_000, 1000, 0));
        assert!(low(&wide, 20_000_000, 50_000, 0));
    }

    #[test]
    fn a_late_queue_leaves_out_whole_frames_and_lets_key_pictures_through() {
        let now = Instant::now();
        let mut backlog = Backlog::default();
        // One-packet frames of two templates: a key picture and a delta
        // frame referencing the frame before it.
        let mut layers = Layers::default();
        let mut frame = |number: u16, start: bool| {
            let [high, low] = number.to_be_bytes();
            let template = u8::from(number != 1);
            let mut descriptor = vec![0x40 | u8::from(start) << 7 | template, high, low];
            if number == 1 && start {
                descriptor.extend([0x80, 0x00, 0x3a, 0x40, 0x00]);
            }
            layers
                .packet(&descriptor, 1, 0, std::time::Duration::ZERO)
                .expect("valid")
        };
        let key = frame(1, true);
        let frames: Vec<Frame> = (2..8).map(|n| frame(n, true)).collect();
        let later = frame(2, false);
        // 2 Mbit/s: 100 kB wait 400 ms, 200 kB 800 ms.
        let queue = |bytes: usize| Queue { oldest: now, bytes };
        // Each frame starts a temporal unit of its own.
        let mut unit = 0;
        let mut admit = |backlog: &mut Backlog, frame: &Frame, discardable, bytes| {
            let queue = queue(bytes);
            unit += 1;
            backlog.layered(
                frame,
                unit,
                discardable,
                Some(&queue),
                2_000_000,
                QUEUE_BOUND,
                now,
            )
        };
        assert_eq!(admit(&mut backlog, &frames[0], true, 70_000), Layered::Send);
        // Past the bound a discardable frame stays out to its end, even once
        // the queue drains; a required one goes out.
        assert_eq!(
            admit(&mut backlog, &frames[0], true, 100_000),
            Layered::Skip
        );
        assert_eq!(admit(&mut backlog, &later, true, 0), Layered::Skip);
        assert_eq!(
            admit(&mut backlog, &frames[1], false, 100_000),
            Layered::Send
        );
        // Past twice the bound only a key picture goes out.
        assert_eq!(
            admit(&mut backlog, &frames[2], false, 200_000),
            Layered::Skip
        );
        assert_eq!(admit(&mut backlog, &key, false, 200_000), Layered::Send);
        // The key picture's 400 kB, 1.6 s, extend the bound for the frames
        // behind it: with 100 kB more they wait 400 ms past it.
        backlog.enqueued(400_000, 400_000, true, 6);
        let sample = |bytes| Queue {
            oldest: now + Duration::from_millis(1),
            bytes,
        };
        let mut layered = |frame: &Frame, discardable, bytes| {
            let queue = sample(bytes);
            backlog.layered(
                frame,
                u64::from(frame.number) + 100,
                discardable,
                Some(&queue),
                2_000_000,
                QUEUE_BOUND,
                now,
            )
        };
        assert_eq!(layered(&frames[3], true, 400_000), Layered::Send);
        assert_eq!(layered(&frames[4], false, 500_000), Layered::Send);
        assert_eq!(layered(&frames[5], true, 500_000), Layered::Skip);
        let cap = MAX_QUEUE_BYTES as usize + 400_000;
        assert!(!backlog.capped(Some(&sample(cap - 1000)), 1000));
        assert!(backlog.capped(Some(&sample(cap)), 1000));
        // The drops lowered the target.
        backlog.tick(now);
        assert_eq!(backlog.tier(3), 2);
        assert_eq!(backlog.limit(), Some(VideoLimit::ForwardingBacklog));
    }

    #[test]
    fn a_key_pictures_allowance_lasts_until_the_queue_catches_up() {
        let now = Instant::now();
        let (key, delta) = key_and_delta();
        let mut backlog = Backlog::default();
        // 2 Mbit/s: 100 kB wait 400 ms. Each unit samples the queue anew.
        let admit = |backlog: &mut Backlog, frame: &Frame, unit, bytes| {
            let oldest = now + Duration::from_millis(unit);
            let queue = Queue { oldest, bytes };
            backlog.layered(frame, unit, true, Some(&queue), 2_000_000, QUEUE_BOUND, now)
        };
        // A 100 kB key picture at unit 1, with a late marker of unit 0 between
        // its packets.
        assert_eq!(admit(&mut backlog, &key, 1, 0), Layered::Send);
        backlog.enqueued(50_000, 50_000, false, 1);
        backlog.enqueued(1000, 1000, true, 0);
        backlog.enqueued(50_000, 50_000, true, 1);
        // Its 400 ms extend the bound: 600 ms behind it pass.
        assert_eq!(admit(&mut backlog, &delta, 2, 150_000), Layered::Send);
        // Until the delay falls to half the bound.
        assert_eq!(admit(&mut backlog, &delta, 3, 60_000), Layered::Send);
        assert_eq!(admit(&mut backlog, &delta, 4, 150_000), Layered::Send);
        assert_eq!(admit(&mut backlog, &delta, 5, 30_000), Layered::Send);
        assert_eq!(admit(&mut backlog, &delta, 6, 150_000), Layered::Skip);
    }

    /// A key picture and a delta frame referencing the frame before it.
    fn key_and_delta() -> (Frame, Frame) {
        let mut layers = Layers::default();
        let key = layers
            .packet(
                &[0xc0, 0, 1, 0x80, 0x00, 0x3a, 0x40, 0x00],
                1,
                0,
                Duration::ZERO,
            )
            .expect("key picture");
        let delta = layers
            .packet(&[0xc1, 0, 2], 1, 0, Duration::ZERO)
            .expect("delta");
        (key, delta)
    }

    #[test]
    fn a_temporal_unit_with_a_frame_out_keeps_its_later_frames() {
        let now = Instant::now();
        let (key, delta) = key_and_delta();
        let mut backlog = Backlog::default();
        // 2 Mbit/s: 100 kB wait 400 ms.
        let queue = Queue {
            oldest: now,
            bytes: 100_000,
        };
        let admit = |backlog: &mut Backlog, frame: &Frame, unit| {
            let queue = Some(&queue as &dyn StreamTxQueueInfo);
            backlog.layered(frame, unit, true, queue, 2_000_000, QUEUE_BOUND, now)
        };
        assert_eq!(admit(&mut backlog, &delta, 1), Layered::Skip);
        assert_eq!(admit(&mut backlog, &key, 2), Layered::Send);
        backlog.enqueued(1000, 1048, false, 2);
        assert_eq!(admit(&mut backlog, &delta, 2), Layered::Send);
        assert_eq!(admit(&mut backlog, &delta, 3), Layered::Skip);
    }

    #[test]
    fn a_late_frame_of_an_earlier_unit_keeps_the_current_unit_whole() {
        let now = Instant::now();
        let mut layers = Layers::default();
        layers
            .packet(
                &[0xc0, 0, 1, 0x80, 0x00, 0x3a, 0x40, 0x00],
                1,
                0,
                Duration::ZERO,
            )
            .expect("key picture");
        let mut delta = |number| {
            let frame = layers.packet(&[0xc1, 0, number], 1, 0, Duration::ZERO);
            frame.expect("delta")
        };
        let (lower, late, top) = (delta(10), delta(8), delta(11));
        let mut backlog = Backlog::default();
        // 2 Mbit/s: 100 kB wait 400 ms, between the bound and twice it.
        let queue = Queue {
            oldest: now,
            bytes: 100_000,
        };
        let admit = |backlog: &mut Backlog, frame: &Frame, unit, discardable| {
            let queue = Some(&queue as &dyn StreamTxQueueInfo);
            backlog.layered(frame, unit, discardable, queue, 2_000_000, QUEUE_BOUND, now)
        };
        // Unit 200's lower frame goes out, then a repaired frame of unit 100.
        assert_eq!(admit(&mut backlog, &lower, 200, false), Layered::Send);
        backlog.enqueued(1000, 1048, false, 200);
        assert_eq!(admit(&mut backlog, &late, 100, false), Layered::Send);
        backlog.enqueued(1000, 1048, true, 100);
        // Unit 200's top frame, with the marker, follows.
        assert!(backlog.out(200));
        assert_eq!(admit(&mut backlog, &top, 200, true), Layered::Send);
    }

    #[test]
    fn a_low_latency_queue_leaves_out_frames_past_50_ms() {
        let now = Instant::now();
        let (key, delta) = key_and_delta();
        let mut backlog = Backlog::default();
        // 2 Mbit/s: 15 kB wait 60 ms.
        let queue = Queue {
            oldest: now,
            bytes: 15_000,
        };
        let mut admit = |frame: &Frame, unit, bound| {
            backlog.layered(frame, unit, true, Some(&queue), 2_000_000, bound, now)
        };
        assert_eq!(admit(&delta, 1, QUEUE_BOUND), Layered::Send);
        assert_eq!(admit(&delta, 2, LOW_LATENCY_BOUND), Layered::Skip);
        assert_eq!(admit(&key, 3, LOW_LATENCY_BOUND), Layered::Send);
    }

    #[test]
    fn video_shedding_covers_the_whole_remainder_and_recovers_slowly() {
        let now = Instant::now();
        let mut backlog = Backlog::default();
        backlog.shed(12, None, VideoLimit::ForwardingBacklog);
        assert!(backlog.dropped(11));
        assert!(backlog.dropped(12));
        assert!(!backlog.dropped(13));
        backlog.tick(now);
        assert_eq!(backlog.tier(3), 2);
        backlog.shed(14, None, VideoLimit::ForwardingBacklog);
        let latest = now + Duration::from_secs(1);
        backlog.tick(latest);
        assert_eq!(backlog.tier(3), 1);
        assert_eq!(backlog.limit(), Some(VideoLimit::ForwardingBacklog));
        backlog.tick(now + RECOVERY_INTERVAL);
        assert_eq!(backlog.tier(3), 1);
        assert_eq!(backlog.limit(), Some(VideoLimit::ForwardingBacklog));
        backlog.tick(latest + RECOVERY_INTERVAL);
        assert_eq!(backlog.tier(3), 2);
        backlog.tick(latest + RECOVERY_INTERVAL * 2);
        assert_eq!(backlog.tier(3), 3);
        assert_eq!(backlog.limit(), None);
    }

    #[test]
    fn unlayered_recovery_reserves_one_bounded_picture_after_pli() {
        let now = Instant::now();
        let mut backlog = Backlog::default();
        backlog.shed(12, Some(now), VideoLimit::ForwardingBacklog);
        assert!(matches!(
            backlog.admission(13, None, true, now),
            VideoAdmission::Pause
        ));
        let ready = now + Duration::from_secs(1);
        let queued = Queue {
            oldest: now,
            bytes: 500,
        };
        assert!(matches!(
            backlog.admission(14, Some(&queued), true, ready),
            VideoAdmission::Pause
        ));
        assert!(matches!(
            backlog.admission(14, None, false, ready),
            VideoAdmission::Pause
        ));
        assert!(matches!(
            backlog.admission(14, None, true, ready),
            VideoAdmission::RequestKeyframe
        ));
        assert!(matches!(
            backlog.admission(14, None, true, ready),
            VideoAdmission::Pause
        ));
        assert!(matches!(
            backlog.admission(15, None, true, ready),
            VideoAdmission::Burst(0)
        ));
        assert!(!backlog.burst_full(None, MAX_KEY_BYTES));
        assert!(backlog.burst_full(None, MAX_KEY_BYTES + 1));
        backlog.enqueued(1200, 1248, false, 15);
        assert!(matches!(
            backlog.admission(15, None, true, ready),
            VideoAdmission::Burst(1248)
        ));
        backlog.enqueued(1000, 1048, true, 15);
        assert!(matches!(
            backlog.admission(16, None, true, ready),
            VideoAdmission::Regular
        ));
    }

    #[test]
    fn traffic_steps_lower_the_tier() {
        let view = View::default();
        let full = full_capacity();
        let tiers: Vec<usize> = (0..5)
            .map(|step| tier(Kind::Camera, &view, full, step).0)
            .collect();
        assert_eq!(tiers, [3, 2, 1, 0, 0]);
        let asked = View {
            request: Some(1),
            ..View::default()
        };
        assert_eq!(
            tier(Kind::Screen, &asked, full, 1),
            (1, VideoLimit::WithinTarget)
        );
        assert_eq!(
            tier(Kind::Camera, &view, full, 1),
            (Kind::Camera.top() - 1, VideoLimit::TrafficBudget)
        );
        let decode = DecodeCapacity { camera: 0, ..full };
        assert_eq!(
            tier(Kind::Camera, &view, decode, 0),
            (0, VideoLimit::ReceiverDecode)
        );
    }

    #[test]
    fn skipped_packets_leave_no_gap() {
        let mut rewrite = Rewrite::default();
        assert_eq!(rewrite.forward(100), Some(0));
        rewrite.skip(101);
        rewrite.skip(102);
        assert_eq!(rewrite.forward(103), Some(1));
        // A packet lost on the way in keeps its gap; when it arrives late it
        // fills it.
        assert_eq!(rewrite.forward(105), Some(3));
        assert_eq!(rewrite.forward(104), Some(2));
        // A late packet the receiver skips leaves its number unused.
        rewrite.skip(104);
        assert_eq!(rewrite.forward(106), Some(4));
        assert_eq!(rewrite.forward(99), None, "before the first packet");
    }

    #[test]
    fn the_first_packets_may_be_skipped() {
        let mut rewrite = Rewrite::default();
        rewrite.skip(7);
        rewrite.skip(8);
        assert_eq!(rewrite.forward(9), Some(0));
        assert_eq!(rewrite.forward(10), Some(1));
    }

    #[test]
    fn old_skips_are_counted_once_pruned() {
        let mut rewrite = Rewrite::default();
        assert_eq!(rewrite.forward(0), Some(0));
        for seq in (1..10_000).step_by(2) {
            rewrite.skip(seq);
            assert_eq!(rewrite.forward(seq + 1), Some(seq.div_ceil(2)));
        }
        assert!(rewrite.skipped.len() as u64 <= REWRITE_WINDOW);
        assert_eq!(rewrite.forward(1), None, "older than the window");
    }
}
