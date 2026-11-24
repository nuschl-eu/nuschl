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

use crate::protocol::VideoLimit;
use nuschl_stream_quality::{
    DecodeCapacity, Kind, Throttle,
    adapt::Stepper,
    dd::{Frame, Layers, Selector},
    receive,
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues, StreamTxQueueInfo};

const QUEUE_AGE: Duration = Duration::from_millis(300);
const MIN_QUEUE_BYTES: u64 = 32 * 1024;
const MAX_QUEUE_BYTES: u64 = 2 * 1024 * 1024;
/// How long a layered frame may wait in a receiver's queue before the queue
/// leaves frames out.
pub const QUEUE_BOUND: Duration = Duration::from_millis(300);
/// The largest frame a publisher sends, an 8 MiB screen frame: a key picture's
/// temporal unit, and a recovery picture, up to this size goes out whole.
pub const MAX_KEY_BYTES: u64 = 8 * 1024 * 1024;
const RECOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// A video source: its kind and the demand last sent to its publisher.
pub struct Video {
    pub kind: Kind,
    pub demand: Option<(usize, u64, Option<VideoLimit>)>,
    pub throttle: Throttle,
    pub layers: Layers,
}

impl Video {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            demand: None,
            throttle: Throttle::default(),
            layers: Layers::default(),
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
    pub stepper: Option<Stepper>,
    pub rewrite: Rewrite,
    pub backlog: Backlog,
    pub reported_limit: Option<VideoLimit>,
    pub limit_throttle: Throttle,
}

/// The video admitted to one receiver's send queue and its temporary quality limit.
#[derive(Default)]
pub struct Backlog {
    dropped_through: Option<u64>,
    /// The layered frame left out whole.
    skipping: Option<u16>,
    /// The bytes of the last key picture's temporal unit, and whether more of
    /// it is to come: the queue's bound grows by the time they take.
    key: (u64, bool),
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
    /// descriptor past about 300 ms, beyond the last recovery picture.
    pub fn full(
        &mut self,
        queue: Option<&dyn StreamTxQueueInfo>,
        bitrate: u64,
        payload: usize,
        now: Instant,
    ) -> bool {
        let limit = (bitrate / 8 * 3 / 10).clamp(MIN_QUEUE_BYTES, MAX_QUEUE_BYTES);
        let age = queue
            .and_then(StreamTxQueueInfo::first_unsent)
            .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        let queued = self.queued_bytes(queue) as u64;
        if queued <= limit && age < QUEUE_AGE && !self.key.1 {
            self.key.0 = 0;
        }
        let picture = Duration::from_secs_f64(self.key.0 as f64 * 8.0 / bitrate.max(1) as f64);
        queued + payload as u64 > limit + self.key.0 || age >= QUEUE_AGE + picture
    }

    /// Admits a packet of the layered `frame` by how long a frame starting now
    /// waits in the queue, less the time the last key picture's temporal unit
    /// takes. Past `bound` the receiver's target drops a step, and a frame its
    /// target can do without (`discardable`) is left out whole; past twice
    /// `bound` every frame but a key picture is, and the receiver waits for the
    /// next switch point.
    pub fn layered(
        &mut self,
        frame: &Frame,
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
            if delay <= bound && !self.key.1 {
                self.key.0 = 0;
            }
            let delay = delay.saturating_sub(time(self.key.0));
            let key = frame.references().next().is_none();
            if key {
                self.key = (0, true);
            } else if delay > bound {
                self.overloaded(VideoLimit::ForwardingBacklog);
                if discardable || delay > bound * 2 {
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

    pub fn enqueued(&mut self, payload: usize, cost: u64, marker: bool) {
        self.unsampled_bytes += payload;
        if self.key.1 {
            self.key = (self.key.0 + payload as u64, !marker);
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
                self.key = (0, true);
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
        assert!(!backlog.full(Some(&sample), 2_000_000, 1000, now));
        backlog.enqueued(1000, 1000, false);
        assert!(backlog.full(Some(&sample), 2_000_000, 1, now));
        assert!(backlog.full(Some(&sample), 15_000_000, 1000, now + QUEUE_AGE));
        assert!(!backlog.full(None, 2_000_000, 1000, now));
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
                .packet(&descriptor, 1, std::time::Duration::ZERO)
                .expect("valid")
        };
        let key = frame(1, true);
        let frames: Vec<Frame> = (2..8).map(|n| frame(n, true)).collect();
        let later = frame(2, false);
        // 2 Mbit/s: 100 kB wait 400 ms, 200 kB 800 ms.
        let queue = |bytes: usize| Queue { oldest: now, bytes };
        let admit = |backlog: &mut Backlog, frame: &Frame, discardable, bytes| {
            let queue = queue(bytes);
            backlog.layered(
                frame,
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
        backlog.enqueued(400_000, 400_000, true);
        let sample = |bytes| Queue {
            oldest: now + Duration::from_millis(1),
            bytes,
        };
        let mut layered = |frame: &Frame, discardable, bytes| {
            let queue = sample(bytes);
            backlog.layered(
                frame,
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
        backlog.enqueued(1200, 1248, false);
        assert!(matches!(
            backlog.admission(15, None, true, ready),
            VideoAdmission::Burst(1248)
        ));
        backlog.enqueued(1000, 1048, true);
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
