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

use crate::{
    protocol::{Mode, Prefer, VideoLimit},
    rooms::Bucket,
};
use nuschl_stream_quality::{
    DecodeCapacity, Kind, Throttle,
    dd::{Frame, Layers, Pace, Selector, Wanted},
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

/// Tier, height, frame rate, bitrate and limit.
type PublisherDemand = (usize, u16, u8, u64, Option<VideoLimit>);

/// A video source: its kind, its sender's mode and the demand last sent to its
/// publisher.
pub struct Video {
    pub kind: Kind,
    pub mode: Mode,
    demand: Option<PublisherDemand>,
    throttle: Throttle,
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

    /// A material rate change or a new picture format, at most once per second.
    pub fn update_demand(&mut self, next: PublisherDemand, now: Duration) -> bool {
        let (tier, height, fps, bitrate, limit) = next;
        let changed =
            self.demand
                .is_none_or(|(old_tier, old_height, old_fps, old_rate, old_limit)| {
                    (tier, height, fps, limit) != (old_tier, old_height, old_fps, old_limit)
                        || (bitrate != old_rate && bitrate == 64_000)
                        || bitrate.abs_diff(old_rate) >= (old_rate / 20).max(100_000)
                });
        if changed && self.throttle.allow(now) {
            self.demand = Some(next);
            true
        } else {
            false
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

/// One packet of a source on its way to one receiver.
pub struct Arrival {
    /// `None` without a descriptor, `Some(None)` for a descriptor the source's
    /// structure cannot read yet.
    pub frame: Option<Option<Frame>>,
    pub video: bool,
    pub seq: u64,
    pub timestamp: u64,
    /// The publisher's marker.
    pub marker: bool,
    pub payload: usize,
    /// The payload with its packet overhead, for the budgets.
    pub cost: u64,
    pub bound: Duration,
    /// Whether the source may be asked for a key frame now.
    pub can_request: bool,
    /// The source's target list and its lowest target.
    pub targets: Option<(u32, Option<Wanted>)>,
}

impl View {
    /// Admits `packet` past the selector, the queue (`queue` at the
    /// receiver's `bitrate`) and the viewer, room and host budgets: the
    /// sequence number and marker it goes out with, or `None`; and whether
    /// the source is to send a key frame.
    pub fn admit(
        &mut self,
        packet: &Arrival,
        queue: Option<&dyn StreamTxQueueInfo>,
        bitrate: u64,
        budgets: [&mut Bucket; 2],
        host: Option<(&mut Bucket, VideoLimit)>,
        now: Instant,
    ) -> (Option<(u64, bool)>, bool) {
        let Arrival {
            frame,
            video,
            seq,
            timestamp,
            payload,
            cost,
            bound,
            ..
        } = *packet;
        // A new target list gives the indices other layers: the lowest target
        // until the next second's choice.
        if let Some((generation, lowest)) = packet.targets
            && self
                .generation
                .replace(generation)
                .is_some_and(|g| g != generation)
        {
            self.selector.meanwhile(None);
            self.selector.want(lowest);
        }
        let layered = frame.flatten();
        // A temporal unit with a packet out goes out whole past the queue's cap
        // and the budgets, up to MAX_KEY_BYTES: the rest of a frame that went
        // out, also late, a late frame that frames out reference, and the
        // unit's later frames.
        let whole = self.backlog.whole(timestamp, payload);
        let rest = whole
            && layered
                .as_ref()
                .is_some_and(|f| self.selector.sent(f) || self.selector.owes(f));
        let carried = layered.is_some() && self.backlog.out(timestamp);
        // Leaves the packet out; whether the receiver now waits for a key frame.
        let skip = |view: &mut Self, shed: Option<VideoLimit>, release: bool| {
            view.rewrite.skip(seq);
            if let Some(limit) = shed {
                view.backlog
                    .shed(timestamp, frame.is_none().then_some(now), limit);
            }
            if release {
                view.selector.release(timestamp);
            }
            if let Some(frame) = &layered {
                view.selector.dropped(frame);
            }
            view.selector.needs_keyframe()
        };
        // Past that the queue sheds the unit and releases its target.
        if carried && !whole {
            return (None, skip(self, Some(VideoLimit::ForwardingBacklog), true));
        }
        if video && self.backlog.dropped(timestamp) && !rest {
            return (None, skip(self, None, false));
        }
        let marker = match &frame {
            None => Some(packet.marker),
            Some(None) => None,
            Some(Some(frame)) => self.selector.forward(frame, timestamp, packet.marker),
        };
        let keyframe = self.selector.needs_keyframe();
        let Some(marker) = marker else {
            self.rewrite.skip(seq);
            return (None, keyframe);
        };
        let full = if video {
            match self
                .backlog
                .admission(timestamp, queue, packet.can_request, now)
            {
                VideoAdmission::Regular => match &layered {
                    None => self.backlog.full(queue, bitrate, payload, bound, now),
                    Some(frame) => {
                        let discardable = self.selector.discardable(frame);
                        let admitted = self.backlog.layered(
                            frame,
                            timestamp,
                            discardable,
                            queue,
                            bitrate,
                            bound,
                            now,
                        );
                        if admitted == Layered::Skip {
                            return (None, skip(self, None, false) || keyframe);
                        }
                        !rest && !carried && self.backlog.capped(queue, payload)
                    }
                },
                VideoAdmission::Burst(bytes) => self.backlog.burst_full(queue, bytes + cost),
                VideoAdmission::Pause => {
                    self.rewrite.skip(seq);
                    return (None, keyframe);
                }
                VideoAdmission::RequestKeyframe => {
                    self.rewrite.skip(seq);
                    return (None, true);
                }
            }
        } else {
            false
        };
        // The budgets lend what the unit carries, down to one burst, and each
        // unit at most one burst.
        let [viewer, room] = budgets;
        let previous = [*viewer, *room];
        let credit = viewer.burst().min(room.burst()).min(
            host.as_ref()
                .map_or(f64::INFINITY, |(bucket, _)| bucket.burst()),
        );
        let lend = (rest || carried) && self.backlog.lendable(timestamp, cost, credit);
        let charge = |bucket: &mut Bucket| {
            if lend {
                bucket.lend(cost as f64, now)
            } else {
                bucket.take(cost as f64, now)
            }
        };
        let limit = if full {
            Some(VideoLimit::ForwardingBacklog)
        } else if !charge(viewer) {
            Some(VideoLimit::SfuViewer)
        } else if !charge(room) {
            Some(VideoLimit::SfuRoom)
        } else {
            host.and_then(|(bucket, limit)| {
                if charge(bucket) {
                    None
                } else {
                    [*viewer, *room] = previous;
                    Some(limit)
                }
            })
        };
        if let Some(limit) = limit {
            if video {
                return (None, skip(self, Some(limit), rest || carried) || keyframe);
            }
            return (None, keyframe);
        }
        if lend {
            self.backlog.lent(timestamp, cost);
        }
        let Some(seq) = self.rewrite.forward(seq) else {
            if let Some(frame) = &layered {
                self.selector.dropped(frame);
            }
            return (None, keyframe);
        };
        if video {
            self.backlog.enqueued(payload, cost, marker, timestamp);
        }
        (Some((seq, marker)), keyframe)
    }
}

/// The video admitted to one receiver's send queue and its temporary quality limit.
#[derive(Default)]
pub struct Backlog {
    dropped_through: Option<u64>,
    /// The layered frame left out whole.
    skipping: Option<u16>,
    /// The latest temporal units with a packet out, by timestamp, their bytes
    /// out and the bytes the budgets lent them.
    units: VecDeque<(u64, u64, f64)>,
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
    /// Whether the packet at `timestamp` belongs to what the queue shed: the
    /// unit at `dropped_through` and the second before it.
    pub fn dropped(&self, timestamp: u64) -> bool {
        self.dropped_through
            .is_some_and(|last| timestamp <= last && last - timestamp <= 90_000)
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
                if (discardable || delay > bound * 2) && !self.out(timestamp) {
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
        self.units.iter().any(|&(unit, ..)| unit == timestamp)
    }

    /// Whether the temporal unit at `timestamp` takes `payload` more bytes past
    /// the queue's cap: at most [`MAX_KEY_BYTES`] in all.
    pub fn whole(&self, timestamp: u64, payload: usize) -> bool {
        let unit = self.units.iter().find(|&&(unit, ..)| unit == timestamp);
        unit.map_or(0, |&(_, bytes, _)| bytes) + payload as u64 <= MAX_KEY_BYTES
    }

    /// Whether the budgets lend the temporal unit at `timestamp` `cost` more,
    /// at most `credit` in all.
    pub fn lendable(&self, timestamp: u64, cost: u64, credit: f64) -> bool {
        let unit = self.units.iter().find(|&&(unit, ..)| unit == timestamp);
        unit.map_or(0.0, |&(.., lent)| lent) + cost as f64 <= credit
    }

    /// The unit at `timestamp`, which takes a packet: `payload` bytes, or a
    /// loan of `lent`.
    fn unit(&mut self, timestamp: u64, payload: usize, lent: f64) {
        match self.units.iter_mut().find(|(unit, ..)| *unit == timestamp) {
            Some((_, bytes, loan)) => {
                *bytes += payload as u64;
                *loan += lent;
            }
            None => {
                self.units.push_back((timestamp, payload as u64, lent));
                if self.units.len() > 8 {
                    self.units.pop_front();
                }
            }
        }
    }

    /// The budgets lent the temporal unit at `timestamp` `cost`.
    pub fn lent(&mut self, timestamp: u64, cost: u64) {
        self.unit(timestamp, 0, cost as f64);
    }

    /// A packet of the temporal unit at `timestamp` went out.
    pub fn enqueued(&mut self, payload: usize, cost: u64, marker: bool, timestamp: u64) {
        self.unit(timestamp, payload, 0.0);
        self.unsampled_bytes += payload;
        match self.key.1 {
            Some(key) if key == timestamp => {
                self.key = (self.key.0 + payload as u64, (!marker).then_some(key));
            }
            // A packet of a later unit, or of a new stream, ends the key
            // picture's unit, also without its marker.
            Some(key) if timestamp > key || key - timestamp > 90_000 => self.key.1 = None,
            _ => {}
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

/// The receiver's selected tier within its decode capacity.
pub fn tier(kind: Kind, view: &View, decode: DecodeCapacity) -> (usize, VideoLimit) {
    let top = kind.top();
    let requested = view.request.unwrap_or(top).min(top);
    let tier = requested.min(decode.get(kind));
    let limit = if tier == requested {
        VideoLimit::WithinTarget
    } else {
        VideoLimit::ReceiverDecode
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
    use crate::rooms;
    use std::collections::{HashMap, HashSet};

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

    #[test]
    fn a_key_allowance_ends_without_its_marker() {
        let now = Instant::now();
        let (key, delta) = key_and_delta();
        let mut backlog = Backlog::default();
        // 2 Mbit/s: 100 kB wait 400 ms. Each unit samples the queue anew.
        let admit = |backlog: &mut Backlog, frame: &Frame, unit, bytes| {
            let oldest = now + Duration::from_millis(unit);
            let queue = Queue { oldest, bytes };
            backlog.layered(frame, unit, true, Some(&queue), 2_000_000, QUEUE_BOUND, now)
        };
        // A 100 kB key picture at unit 1 whose last packet never goes out,
        // then delta units through an empty queue.
        assert_eq!(admit(&mut backlog, &key, 1, 0), Layered::Send);
        backlog.enqueued(100_000, 100_000, false, 1);
        for unit in 2..10 {
            assert_eq!(admit(&mut backlog, &delta, unit, 0), Layered::Send);
            backlog.enqueued(1000, 1000, true, unit);
        }
        // 600 ms queued is twice the bound.
        assert_eq!(admit(&mut backlog, &delta, 10, 150_000), Layered::Skip);
    }

    #[test]
    fn late_packets_and_a_jump_back_keep_the_units() {
        let second = 90_000;
        let mut backlog = Backlog::default();
        // A packet of a unit two seconds back, then one of a new stream from
        // time 0: each unit keeps its later packets.
        backlog.enqueued(1000, 1000, false, 10 * second);
        backlog.enqueued(1000, 1000, true, 8 * second);
        backlog.enqueued(1000, 1000, false, 0);
        for unit in [10 * second, 8 * second, 0] {
            assert!(backlog.out(unit), "{unit}");
        }
        // A shed takes its unit and the second before it.
        backlog.shed(10 * second, None, VideoLimit::ForwardingBacklog);
        assert!(backlog.dropped(10 * second));
        assert!(backlog.dropped(9 * second));
        assert!(!backlog.dropped(10 * second + 1));
        assert!(!backlog.dropped(8 * second));
        assert!(!backlog.dropped(0));
    }

    #[test]
    fn one_unit_takes_at_most_its_bound_past_the_cap_and_budgets() {
        let now = Instant::now();
        let queue = Queue {
            oldest: now,
            bytes: 0,
        };
        // A source stuck in one temporal unit: a key picture, then delta
        // frames of 1000 bytes at its timestamp, all at once.
        let run = |rate: u64| {
            let mut layers = Layers::default();
            let mut view = View::default();
            view.selector.want(Some((0, 0)));
            let mut viewer = crate::rooms::media_budget(rate, now);
            let mut room = crate::rooms::media_budget(rate, now);
            let mut sent = 0;
            for number in 1..20_000u16 {
                let [high, low] = number.to_be_bytes();
                let descriptor = if number == 1 {
                    vec![0xc0, high, low, 0x80, 0x00, 0x3a, 0x40, 0x00]
                } else {
                    vec![0xc1, high, low]
                };
                let frame = layers.packet(&descriptor, 1000, 3000, Duration::ZERO);
                let arrival = Arrival {
                    frame: Some(frame),
                    video: true,
                    seq: u64::from(number),
                    timestamp: 3000,
                    marker: true,
                    payload: 1000,
                    cost: 1000,
                    bound: QUEUE_BOUND,
                    can_request: true,
                    targets: None,
                };
                let budgets = [&mut viewer, &mut room];
                if view
                    .admit(&arrival, Some(&queue), u64::MAX, budgets, None, now)
                    .0
                    .is_some()
                {
                    sent += 1000;
                }
            }
            sent
        };
        // Wide budgets: the queue sheds the unit at MAX_KEY_BYTES.
        let wide = run(10_000_000_000);
        assert!(wide > MAX_KEY_BYTES / 2 && wide <= MAX_KEY_BYTES, "{wide}");
        // 2 Mbit/s: the budgets lend it one burst past what they hold.
        let burst = crate::rooms::media_budget(2_000_000, now).burst() as u64;
        let narrow = run(2_000_000);
        assert!(
            narrow > burst && narrow <= 2 * burst,
            "{narrow} for {burst}"
        );
    }

    /// A queue sampled at `at`, as str0m reports it between polls.
    struct Sample {
        at: Instant,
        oldest: Option<Instant>,
        bytes: usize,
    }

    impl StreamTxQueueInfo for Sample {
        fn created_at(&self) -> Instant {
            self.at
        }

        fn byte_size(&self) -> usize {
            self.bytes
        }

        fn packet_count(&self) -> usize {
            usize::from(self.bytes > 0)
        }

        fn first_unsent(&self) -> Option<Instant> {
            self.oldest
        }
    }

    /// Random layered streams through a queue that drains at the receiver's
    /// rate, sampled every few milliseconds, with late packets: a temporal
    /// unit with a packet out goes out whole, key pictures go out, a packet of
    /// a delta unit waits at most twice the bound beyond the key picture and
    /// two units, and a link of 2.5 times the source's rate leaves nothing out.
    #[test]
    fn the_queue_holds_its_bounds_over_random_streams() {
        let start = Instant::now();
        let mut skipped = 0;
        for seed in 1..=200u64 {
            let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut below = |n: u64| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state % n
            };
            let rate = [500_000, 2_000_000, 8_000_000, 20_000_000][below(4) as usize];
            let bound = [QUEUE_BOUND, LOW_LATENCY_BOUND][below(2) as usize];
            let source = rate * (1 + below(12)) / 5;
            let fps = [15, 30, 60][below(3) as usize];
            let delta = source / 8 / fps;
            let time = |bytes: u64| Duration::from_nanos(bytes * 8_000_000_000 / rate);
            let limit = bound * 2 + time(12 * delta) + time(3 * delta) * 2;
            let mut layers = Layers::default();
            let mut backlog = Backlog::default();
            // When each queued packet came and when it leaves.
            let mut queue: VecDeque<(Instant, Instant, usize)> = VecDeque::new();
            let mut sample = Sample {
                at: start,
                oldest: None,
                bytes: 0,
            };
            let mut held = None;
            let mut number = 0u16;
            let mut left = VecDeque::new();
            let mut keyed = 0;
            for unit in 0..10 * fps {
                // Key pictures at least 2 s apart, as the SFU asks for them.
                let key = unit == 0 || unit >= keyed + 2 * fps && below(50) == 0;
                if key {
                    keyed = unit;
                }
                let bytes = if key { 8 } else { 1 } * delta * (50 + below(100)) / 100;
                let timestamp = unit * 90_000 / fps;
                // The unit's packets: one or two frames, then maybe a late one
                // of the unit before.
                let mut packets = Vec::new();
                let frames = 1 + below(2);
                for index in 0..frames {
                    number = number.wrapping_add(1);
                    let size = (bytes / frames).max(1);
                    let count = size.div_ceil(1200);
                    for packet in 0..count {
                        let [high, low] = number.to_be_bytes();
                        let flags = u8::from(packet == 0) << 7 | u8::from(packet + 1 == count) << 6;
                        let first = key && index == 0;
                        let mut descriptor = vec![flags | u8::from(!first), high, low];
                        if unit == 0 && packet == 0 {
                            descriptor.extend([0x80, 0x00, 0x3a, 0x40, 0x00]);
                        }
                        let marker = index + 1 == frames && packet + 1 == count;
                        let payload = (size / count) as usize;
                        packets.push((descriptor, timestamp, payload, marker, key));
                    }
                }
                if let Some(late) = held.take() {
                    packets.insert(1, late);
                }
                if unit > 0 && below(10) == 0 {
                    held = packets.pop();
                }
                let at = start + Duration::from_nanos(unit * 1_000_000_000 / fps);
                for (index, (descriptor, timestamp, payload, marker, key)) in
                    packets.into_iter().enumerate()
                {
                    let now = at + Duration::from_micros(50 * index as u64);
                    while queue.front().is_some_and(|&(_, leaves, _)| leaves <= now) {
                        queue.pop_front();
                    }
                    if now >= sample.at + Duration::from_millis(1 + below(5)) {
                        sample = Sample {
                            at: now,
                            oldest: queue.front().map(|&(came, ..)| came),
                            bytes: queue.iter().map(|&(.., bytes)| bytes).sum(),
                        };
                    }
                    let frame = layers
                        .packet(&descriptor, payload, timestamp, now - start)
                        .expect("valid descriptor");
                    // As the selector: the rest of a frame left out stays out.
                    if left.contains(&frame.number) {
                        continue;
                    }
                    let discardable = !key && timestamp / (90_000 / fps) % 2 == 1;
                    let out = backlog.out(timestamp);
                    let admitted = backlog.layered(
                        &frame,
                        timestamp,
                        discardable,
                        Some(&sample),
                        rate,
                        bound,
                        now,
                    );
                    if frame.start && frame.references().next().is_none() {
                        assert_eq!(admitted, Layered::Send, "seed {seed}: a key picture");
                    }
                    let left_out = match admitted {
                        Layered::Send => !out && backlog.capped(Some(&sample), payload),
                        Layered::Skip => true,
                    };
                    assert!(
                        !(out && left_out),
                        "seed {seed}: a unit with a packet out loses one"
                    );
                    if left_out {
                        assert!(
                            5 * source > 2 * rate,
                            "seed {seed}: left out on a wide link"
                        );
                        nuschl_stream_quality::dd::push(&mut left, frame.number);
                        skipped += 1;
                        continue;
                    }
                    backlog.enqueued(payload, payload as u64, marker, timestamp);
                    let free = queue.back().map_or(now, |&(_, leaves, _)| leaves.max(now));
                    let leaves = free + time(payload as u64);
                    queue.push_back((now, leaves, payload));
                    if !key {
                        let waited = leaves - now;
                        assert!(
                            waited <= limit,
                            "seed {seed}: waited {waited:?} of {limit:?}"
                        );
                    }
                }
            }
        }
        assert!(skipped > 0);
    }

    /// One recorded frame of a fixture: its RTP timestamp increase, packets,
    /// payload bytes, the publisher's marker, and its first and later
    /// packets' descriptors.
    type Recorded = (u64, usize, usize, bool, Vec<u8>, Option<Vec<u8>>);

    fn recorded(text: &str) -> Vec<Recorded> {
        let hex = |text: &str| -> Vec<u8> {
            (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
                .collect()
        };
        let lines = text.lines().filter(|line| !line.starts_with('#'));
        lines
            .map(|line| {
                let f: Vec<&str> = line.split(' ').collect();
                let number = |i: usize| f[i].parse::<usize>().expect("a number");
                let rest = (f[6] != "-").then(|| hex(f[6]));
                (
                    number(1) as u64,
                    number(2),
                    number(3),
                    f[4] == "1",
                    hex(f[5]),
                    rest,
                )
            })
            .collect()
    }

    /// A replayed Chromium source: its fixture looped with frame numbers and
    /// timestamps running on, or numbered anew as from a reopened encoder,
    /// or stuck in one endless temporal unit.
    struct Replayed {
        frames: Vec<Recorded>,
        next: usize,
        offset: u16,
        timestamp: u64,
        kind: Kind,
        bound: Duration,
        frozen: bool,
        layers: Layers,
        /// Parses each frame's first packet in order, for its references.
        parser: Layers,
        /// The id of the latest frame with each number.
        ids: HashMap<u16, u64>,
    }

    /// A packet on its way to the SFU: its source, frame and index in the
    /// frame, its descriptor, timestamp, marker and payload, and when it was
    /// sent.
    #[derive(Clone)]
    struct Sent {
        source: usize,
        frame: u64,
        index: usize,
        descriptor: Vec<u8>,
        timestamp: u64,
        marker: bool,
        payload: usize,
        at: Instant,
    }

    /// The real selector, queue and budgets over random calls: replayed
    /// Chromium L3T3_KEY cameras and L2T2_KEY screens to viewers that share a
    /// room budget and get audio charged to the same budgets, with reordered,
    /// repaired and lost packets, key units that lose their marker packet,
    /// encoders that number frames anew, and sources stuck in one endless
    /// temporal unit. Each viewer gets every whole frame with its references,
    /// at most one marker per unit and at most MAX_KEY_BYTES per unit; the
    /// budgets lend at most one burst; and audio keeps flowing.
    #[test]
    fn views_hold_their_invariants_over_random_calls() {
        let camera = recorded(include_str!("../tests/fixtures/chrome-l3t3-key.txt"));
        let screen = recorded(include_str!("../tests/fixtures/chrome-l2t2-key.txt"));
        let start = Instant::now();
        let mut totals = [0usize; 3];
        for seed in 1..=120u64 {
            let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut below = |n: u64| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state % n
            };
            let mut sources: Vec<Replayed> = (0..1 + below(2))
                .map(|_| {
                    let (frames, kind) = if below(2) == 0 {
                        (camera.clone(), Kind::Camera)
                    } else {
                        (screen.clone(), Kind::Screen)
                    };
                    let bound = [QUEUE_BOUND, LOW_LATENCY_BOUND][below(2) as usize];
                    let (layers, parser) = (Layers::default(), Layers::default());
                    let ids = HashMap::new();
                    let (next, offset, timestamp, frozen) = (0, 0, 1 << 32, false);
                    Replayed {
                        frames,
                        next,
                        offset,
                        timestamp,
                        kind,
                        bound,
                        frozen,
                        layers,
                        parser,
                        ids,
                    }
                })
                .collect();
            let room_rate = [1_000_000, 2_000_000, 6_000_000, 50_000_000][below(4) as usize];
            let mut room = rooms::media_budget(room_rate, start);
            // Per viewer: its budget, link rate, views (the audio one last),
            // queues, and what it got.
            struct Viewer {
                budget: Bucket,
                rate: u64,
                views: Vec<View>,
                queues: Vec<VecDeque<(Instant, usize)>>,
                got: HashMap<u64, HashSet<usize>>,
                marks: HashMap<(usize, u64), HashSet<u64>>,
                bytes: HashMap<(usize, u64), u64>,
                seqs: HashSet<(usize, u64)>,
                audio: Option<Instant>,
            }
            let mut viewers: Vec<Viewer> = (0..1 + below(3))
                .map(|_| Viewer {
                    budget: rooms::media_budget(
                        [2_000_000, 6_000_000, 50_000_000][below(3) as usize],
                        start,
                    ),
                    rate: [500_000, 3_000_000, 20_000_000][below(3) as usize],
                    views: (0..=sources.len()).map(|_| View::default()).collect(),
                    queues: vec![VecDeque::new(); sources.len()],
                    got: HashMap::new(),
                    marks: HashMap::new(),
                    bytes: HashMap::new(),
                    seqs: HashSet::new(),
                    audio: None,
                })
                .collect();
            // Per frame: packets, references, and whether a packet was lost
            // upstream or unreadable at the SFU.
            let mut frames: HashMap<u64, (usize, Vec<u64>, bool)> = HashMap::new();
            let mut pending: Vec<(u64, Sent)> = Vec::new();
            let mut now = start;
            let mut second = start;
            let mut audio = start;
            for step in 0..3000u64 {
                let count_of_sources = sources.len() as u64;
                let index = (step % count_of_sources) as usize;
                let source = &mut sources[index];
                if source.next == source.frames.len() {
                    // The fixture again from its key picture: numbers run on,
                    // or start anew as from a reopened encoder.
                    let first = &source.frames[0].4;
                    let last = source.offset.wrapping_add(u16::from_be_bytes([
                        source.frames[source.frames.len() - 1].4[1],
                        source.frames[source.frames.len() - 1].4[2],
                    ]));
                    let back = if below(4) == 0 {
                        2 + below(100) as u16
                    } else {
                        0
                    };
                    source.offset = last
                        .wrapping_add(1)
                        .wrapping_sub(back)
                        .wrapping_sub(u16::from_be_bytes([first[1], first[2]]));
                    source.next = 0;
                    source.timestamp += 3000;
                }
                if !source.frozen && step > 1000 && below(3000) == 0 {
                    source.frozen = true;
                }
                let (dt, count, bytes, marker, first, rest) = source.frames[source.next].clone();
                source.next += 1;
                if !source.frozen {
                    source.timestamp += dt;
                }
                now += Duration::from_micros(dt * 100 / 9 / count_of_sources + 10);
                let number = u16::from_be_bytes([first[1], first[2]]).wrapping_add(source.offset);
                let id = step;
                let mut parsed = first.clone();
                parsed[1..3].copy_from_slice(&number.to_be_bytes());
                let parse = source
                    .parser
                    .packet(&parsed, 0, source.timestamp, now - start);
                let references = parse.map_or_else(Vec::new, |frame| {
                    frame
                        .references()
                        .filter_map(|r| source.ids.get(&r).copied())
                        .collect()
                });
                source.ids.insert(number, id);
                // A key unit loses its marker packet now and then.
                let key = references.is_empty() && dt == 0 || source.next == 1;
                let lost = marker && below(if key { 3 } else { 50 }) == 0;
                frames.insert(id, (count, references, lost));
                for packet in 0..count {
                    let (first_packet, last) = (packet == 0, packet + 1 == count);
                    let mut descriptor = match (&rest, first_packet) {
                        (_, true) => first.clone(),
                        (Some(rest), false) => rest.clone(),
                        (None, false) => first[..3].to_vec(),
                    };
                    descriptor[0] =
                        (descriptor[0] & 0x3f) | u8::from(first_packet) << 7 | u8::from(last) << 6;
                    descriptor[1..3].copy_from_slice(&number.to_be_bytes());
                    if lost && last {
                        continue;
                    }
                    let sent = Sent {
                        source: index,
                        frame: id,
                        index: packet,
                        descriptor,
                        timestamp: sources[index].timestamp,
                        marker: marker && last,
                        payload: (bytes / count).max(1),
                        at: now,
                    };
                    let order = step * 64 + packet as u64 + below(4) * 32;
                    if below(50) == 0 {
                        pending.push((order + 64 * (1 + below(30)), sent.clone()));
                    }
                    pending.push((order, sent));
                }
                pending.sort_by_key(|(order, _)| *order);
                let ready = pending.partition_point(|(order, _)| *order < step * 64);
                for (_, sent) in pending.drain(..ready).collect::<Vec<_>>() {
                    if now - sent.at > Duration::from_secs(2) {
                        continue;
                    }
                    let source = &mut sources[sent.source];
                    let frame = source.layers.packet(
                        &sent.descriptor,
                        sent.payload,
                        sent.timestamp,
                        now - start,
                    );
                    if frame.is_none() {
                        frames.get_mut(&sent.frame).expect("a frame").2 = true;
                    }
                    let arrival = Arrival {
                        frame: Some(frame),
                        video: true,
                        seq: sent.frame * 64 + sent.index as u64,
                        timestamp: sent.timestamp,
                        marker: sent.marker,
                        payload: sent.payload,
                        cost: sent.payload as u64 + 50,
                        bound: source.bound,
                        can_request: true,
                        targets: Some((source.layers.generation(), source.layers.lowest())),
                    };
                    for viewer in &mut viewers {
                        let queue = &mut viewer.queues[sent.source];
                        let rate = viewer.rate;
                        while queue.front().is_some_and(|&(leaves, _)| leaves <= now) {
                            queue.pop_front();
                        }
                        let sample = Sample {
                            at: now,
                            oldest: queue.front().map(|_| now),
                            bytes: queue.iter().map(|&(_, bytes)| bytes).sum(),
                        };
                        let budgets = [&mut viewer.budget, &mut room];
                        let view = &mut viewer.views[sent.source];
                        let (out, _) =
                            view.admit(&arrival, Some(&sample), rate, budgets, None, now);
                        for bucket in [&viewer.budget, &room] {
                            let (tokens, burst) = bucket.level();
                            assert!(tokens >= -burst - 1.0, "seed {seed}: {tokens} past {burst}");
                        }
                        // A repeated packet goes out with its first sequence
                        // number: the receiver drops the copy.
                        let Some((seq, marker)) = out else {
                            continue;
                        };
                        if !viewer.seqs.insert((sent.source, seq)) {
                            continue;
                        }
                        totals[0] += 1;
                        let free = queue.back().map_or(now, |&(leaves, _)| leaves.max(now));
                        let time = Duration::from_nanos(sent.payload as u64 * 8_000_000_000 / rate);
                        queue.push_back((free + time, sent.payload));
                        viewer.got.entry(sent.frame).or_default().insert(sent.index);
                        let unit = (sent.source, sent.timestamp);
                        let bytes = viewer.bytes.entry(unit).or_default();
                        *bytes += sent.payload as u64;
                        assert!(
                            *bytes <= MAX_KEY_BYTES,
                            "seed {seed}: unit of {bytes} bytes"
                        );
                        // A source stuck in one unit marks each of its
                        // frames' ends itself.
                        if marker && !sources[sent.source].frozen {
                            totals[1] += 1;
                            let marks = viewer.marks.entry(unit).or_default();
                            marks.insert(seq);
                            assert_eq!(marks.len(), 1, "seed {seed}: two markers in {unit:?}");
                        }
                    }
                }
                // Audio every 20 ms, charged to the same budgets.
                while audio <= now {
                    audio += Duration::from_millis(20);
                    let arrival = Arrival {
                        frame: None,
                        video: false,
                        seq: audio.duration_since(start).as_millis() as u64,
                        timestamp: 0,
                        marker: true,
                        payload: 120,
                        cost: 170,
                        bound: QUEUE_BOUND,
                        can_request: false,
                        targets: None,
                    };
                    for viewer in &mut viewers {
                        let view = viewer.views.last_mut().expect("audio");
                        let budgets = [&mut viewer.budget, &mut room];
                        if view
                            .admit(&arrival, None, viewer.rate, budgets, None, now)
                            .0
                            .is_some()
                        {
                            let gap = viewer.audio.map_or(Duration::ZERO, |at| now - at);
                            assert!(
                                gap <= Duration::from_secs(10),
                                "seed {seed}: audio stopped {gap:?}"
                            );
                            viewer.audio = Some(now);
                            totals[2] += 1;
                        }
                    }
                }
                // Once a second each viewer's targets, as the SFU plans them.
                if now >= second {
                    second = now + Duration::from_secs(1);
                    for viewer in &mut viewers {
                        for (source, view) in sources.iter().zip(&mut viewer.views) {
                            let tier = below(4) as usize;
                            let choices =
                                source.layers.choices(source.kind, tier, below(2) == 0, 0);
                            let choice = (!choices.is_empty() && below(8) != 0)
                                .then(|| choices[below(choices.len() as u64) as usize]);
                            let base = view.selector.current().and_then(|c| source.layers.base(c));
                            view.selector.meanwhile(base);
                            view.selector
                                .want(choice.map(|c| (c.target, c.layer.spatial)));
                            view.generation = Some(source.layers.generation());
                        }
                    }
                }
            }
            // Every whole frame a viewer got has its references whole, but for
            // a reference lost upstream or unreadable at the SFU.
            for viewer in &viewers {
                let whole = |id: &u64| {
                    let packets = viewer.got.get(id).map_or(0, HashSet::len);
                    packets == frames[id].0
                };
                for (id, (_, references, _)) in &frames {
                    if whole(id) {
                        for reference in references.iter().filter(|r| !frames[r].2) {
                            assert!(
                                whole(reference),
                                "seed {seed}: frame {id} lacks {reference}"
                            );
                        }
                    }
                }
            }
        }
        // Forwarded packets, markers and audio packets.
        assert!(totals.iter().all(|&n| n > 1000), "{totals:?}");
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
    fn publisher_demand_absorbs_small_budget_changes() {
        let mut video = Video::new(Kind::Screen, Mode::default_for(true));
        let demand = |rate| (3, 2160, 60, rate, Some(VideoLimit::TrafficBudget));
        assert!(video.update_demand(demand(100_000_000), Duration::ZERO));
        assert!(!video.update_demand(demand(80_000_000), Duration::from_millis(500)));
        assert!(!video.update_demand(demand(99_000_000), Duration::from_secs(1)));
        assert!(video.update_demand(demand(94_000_000), Duration::from_secs(2)));
        assert!(video.update_demand(demand(64_000), Duration::from_secs(3)));
    }

    #[test]
    fn viewer_quality_stays_within_decode_capacity() {
        let view = View::default();
        let full = full_capacity();
        let asked = View {
            request: Some(1),
            ..View::default()
        };
        assert_eq!(
            tier(Kind::Screen, &asked, full),
            (1, VideoLimit::WithinTarget)
        );
        let decode = DecodeCapacity { camera: 0, ..full };
        assert_eq!(
            tier(Kind::Camera, &view, decode),
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
