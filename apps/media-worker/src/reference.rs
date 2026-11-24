// SPDX-License-Identifier: EUPL-1.2
//! The echo canceller's reference: what the output device actually plays, on
//! the same clock as the microphone.
//!
//! The reference is a recording of the output device (PipeWire sink capture,
//! PulseAudio monitor, WASAPI loopback, macOS process tap), so the echo of
//! every application is removed, not just Nuschl's voices. Where no such
//! recording exists, the output callback records what it hands to the device.
//!
//! Device callbacks stamp their samples on one clock (the worker's `Instant`
//! epoch: the device's own instants, corrected by the latency cpal reports,
//! see `StreamClock`), and the worker feeds the canceller each reference
//! frame once it has sounded, up to the microphone frame being processed.
//! The canceller then always sees the far end before its echo, at a steady
//! offset, whatever the callbacks' burst sizes.

use crate::dsp::{FRAME, RATE};
use rtrb::{Consumer, Producer, RingBuffer};
use speexdsp_resampler::State;
use std::time::{Duration, Instant};

/// Stamps per stream (one per device callback) kept for the worker.
const ANCHORS: usize = 256;
/// The reference may run this far behind before the microphone stops waiting.
pub const MAX_HOLD_FRAMES: usize = 4;
/// A reference frame is fed if it starts less than this after the microphone
/// frame: everything that sounded before the microphone frame ended (one
/// frame, 10 ms) has then reached the canceller ahead of its echo, with 15 ms
/// to spare for the devices' approximate latency reports.
pub const LEAD: Duration = Duration::from_millis(25);
/// Reference audio older than this is no use to the canceller (a stalled
/// stream resuming); it is dropped rather than fed in a burst.
const STALE: Duration = Duration::from_millis(300);
/// A stream lost audio when its stamps run this much ahead of its samples;
/// smaller differences are the stamps' own jitter (PulseAudio interpolates
/// its latency between polls).
const GAP: Duration = Duration::from_millis(10);
const FRAME_NANOS: u64 = FRAME as u64 * 1_000_000_000 / RATE as u64;

/// Nanoseconds since the worker's epoch.
#[derive(Clone, Copy)]
pub struct Clock {
    epoch: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    /// Now, moved `earlier` (by capture latency) or `later` (by playback
    /// latency). Callable from device callbacks: no allocation.
    pub fn now_shifted(&self, earlier: Duration, later: Duration) -> u64 {
        let elapsed = self.epoch.elapsed() + later;
        elapsed.saturating_sub(earlier).as_nanos() as u64
    }
}

/// One stream's device instants (cpal's callback and capture or playback
/// times) on the worker's clock. A callback runs some time after the device
/// instant it reports, so the smallest difference seen is the offset between
/// the two clocks: a late callback keeps the stamps of its audio.
pub struct StreamClock {
    clock: Clock,
    offset: Option<i128>,
}

impl StreamClock {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            offset: None,
        }
    }

    /// The device instant `callback` (in nanoseconds), moved `earlier` or
    /// `later` as in `Clock::now_shifted`. No allocation.
    pub fn now_shifted(&mut self, callback: u128, earlier: Duration, later: Duration) -> u64 {
        let device = callback as i128;
        let seen = i128::from(self.clock.now_shifted(Duration::ZERO, Duration::ZERO)) - device;
        let offset = *self
            .offset
            .insert(self.offset.map_or(seen, |offset| offset.min(seen)));
        (device + offset + later.as_nanos() as i128 - earlier.as_nanos() as i128).max(0) as u64
    }
}

/// The producer half of a stamped stream: samples and (frame, time) anchors.
pub struct Stamper {
    anchors: Producer<(u64, u64)>,
    frames: u64,
}

impl Stamper {
    /// Mark that the next frame pushed sounds (or was captured) at `nanos`.
    pub fn anchor(&mut self, nanos: u64) {
        // A full anchor ring only loses precision; the timeline extrapolates.
        let _ = self.anchors.push((self.frames, nanos));
    }

    /// Account frames that were pushed into the sample ring.
    pub fn advance(&mut self, frames: u64) {
        self.frames += frames;
    }
}

/// The consumer half: when a frame index sounded.
pub struct Timeline {
    anchors: Consumer<(u64, u64)>,
    last: Option<(u64, u64)>,
}

pub fn stamped() -> (Stamper, Timeline) {
    let (anchors, consumer) = RingBuffer::new(ANCHORS);
    (
        Stamper { anchors, frames: 0 },
        Timeline {
            anchors: consumer,
            last: None,
        },
    )
}

impl Timeline {
    /// The time of frame `index` (48 kHz frames since the stream started), from
    /// the newest stamp at or before it; `None` before the first stamp.
    pub fn time_of(&mut self, index: u64) -> Option<u64> {
        while let Ok(&(frame, nanos)) = self.anchors.peek() {
            if frame > index && self.last.is_some() {
                break;
            }
            self.last = Some((frame, nanos));
            let _ = self.anchors.pop();
        }
        let (frame, nanos) = self.last?;
        let offset = index.saturating_sub(frame) * 1_000_000_000 / u64::from(RATE);
        Some(nanos + offset)
    }
}

/// What the canceller's next reference frame is, for a microphone frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// It sounded no later than the microphone frame: give it to the canceller.
    Feed,
    /// It sounds later: process the microphone frame first.
    Later,
    /// It has sounded but not arrived: hold the microphone frame if allowed.
    Wait,
}

/// How much microphone audio was lost between a frame starting at `previous`
/// and the next one at `mic`: a skipped device cycle, or a full ring while the
/// worker was stalled.
pub fn lost(previous: u64, mic: u64) -> Option<u64> {
    let gap = mic.checked_sub(previous + FRAME_NANOS)?;
    (gap > GAP.as_nanos() as u64).then_some(gap)
}

/// The canceller's reference stream, and when the next frame for the
/// canceller starts.
///
/// AEC3 pairs render and capture by count, so the reference is fed one frame
/// per 10 ms of the microphone's clock, timed by the stamps: what the recording
/// lost is fed as silence, and what sounded while the microphone lost audio is
/// skipped.
pub struct Reference {
    samples: Consumer<f32>,
    timeline: Timeline,
    pub channels: usize,
    index: u64,
    end: Option<u64>,
    /// A gap is being bridged: audio is skipped up to the frame's start, or
    /// silence fed up to the next sample.
    bridging: bool,
}

impl Reference {
    pub fn new(samples: Consumer<f32>, timeline: Timeline, channels: usize) -> Self {
        Self {
            samples,
            timeline,
            channels,
            index: 0,
            end: None,
            bridging: false,
        }
    }

    /// Discard what is queued: it played before the microphone opened.
    pub fn skip(&mut self) {
        let frames = self.samples.slots() / self.channels;
        if let Ok(chunk) = self.samples.read_chunk(frames * self.channels) {
            chunk.commit_all();
        }
        self.index += frames as u64;
        self.end = None;
        let _ = self.timeline.time_of(self.index);
    }

    /// The microphone lost `nanos` of audio: skip the reference of that time.
    pub fn lose(&mut self, nanos: u64) {
        self.end = self.end.map(|end| end + nanos);
    }

    /// The next reference frame for a microphone frame starting at `mic`, into
    /// `frame` (`channels` interleaved): the 10 ms from where the previous one
    /// ended, as long as the stamps agree within `GAP`.
    pub fn next(&mut self, mic: u64, frame: &mut [f32]) -> Step {
        let frames = |nanos: u64| (nanos * u64::from(RATE) / 1_000_000_000) as usize;
        let Some(mut time) = self.timeline.time_of(self.index) else {
            return Step::Later;
        };
        // Audio older than `STALE` is no use to the canceller (a stalled stream
        // resuming): it is skipped.
        let start = self
            .end
            .unwrap_or(time)
            .max(mic.saturating_sub(STALE.as_nanos() as u64));
        if start >= mic + LEAD.as_nanos() as u64 {
            return Step::Later;
        }
        // Differences within `GAP` are the stamps' jitter. Audio from further
        // before `start` sounded while the microphone lost audio: skipped.
        let gap = GAP.as_nanos() as u64;
        self.bridging |= time + gap < start || time > start + gap;
        while self.bridging && time < start {
            let late = frames(start - time).clamp(1, FRAME);
            let Ok(chunk) = self.samples.read_chunk(late * self.channels) else {
                return Step::Wait;
            };
            chunk.commit_all();
            self.index += late as u64;
            time = self.timeline.time_of(self.index).unwrap_or(start);
        }
        // Audio from further after `start` follows what the recording lost:
        // silence up to it.
        let silent = if self.bridging {
            frames(time - start).min(FRAME)
        } else {
            0
        };
        let Ok(chunk) = self.samples.read_chunk((FRAME - silent) * self.channels) else {
            return Step::Wait;
        };
        self.bridging = time > start + FRAME_NANOS;
        let (silence, rest) = frame[..FRAME * self.channels].split_at_mut(silent * self.channels);
        let (first, second) = chunk.as_slices();
        silence.fill(0.);
        rest[..first.len()].copy_from_slice(first);
        rest[first.len()..].copy_from_slice(second);
        chunk.commit_all();
        self.index += (FRAME - silent) as u64;
        self.end = Some(start + FRAME_NANOS);
        Step::Feed
    }
}

/// Device-rate audio to 48 kHz with one or two channels, interleaved: two
/// keep stereo apart for the canceller (even device channels go left, odd
/// right). Runs on the device thread: no locks, allocation or blocking.
const REFERENCE_INPUT: usize = 128;
const REFERENCE_OUTPUT: usize = 6 * REFERENCE_INPUT;

struct FilteredReference {
    state: State,
    input: [[f32; REFERENCE_INPUT]; 2],
    output: [[f32; REFERENCE_OUTPUT]; 2],
}

impl FilteredReference {
    fn convert(&mut self, frames: usize, channels: usize, ring: &mut Producer<f32>) -> (u64, u64) {
        let (mut cursor, mut generated, mut buffered) = (0, 0, 0);
        while cursor < frames {
            let left =
                self.state
                    .process_float(0, &self.input[0][cursor..frames], &mut self.output[0]);
            let right = if channels == 2 {
                self.state
                    .process_float(1, &self.input[1][cursor..frames], &mut self.output[1])
            } else {
                left
            };
            let (Ok((read, written)), Ok(other)) = (left, right) else {
                break;
            };
            if (read, written) != other || read + written == 0 {
                break;
            }
            for i in 0..written {
                buffered += write_reference([self.output[0][i], self.output[1][i]], channels, ring);
            }
            generated += written as u64;
            cursor += read;
        }
        (generated, buffered)
    }
}

pub struct ReferenceConverter {
    channels: usize,
    outputs: usize,
    rate: u32,
    filtered: Option<Box<FilteredReference>>,
    filter_delay: Duration,
    accepted: u64,
    generated: u64,
}

fn write_reference(frame: [f32; 2], channels: usize, ring: &mut Producer<f32>) -> u64 {
    if ring.slots() < channels {
        return 0;
    }
    for value in frame.into_iter().take(channels) {
        let finite = if value.is_finite() {
            value.clamp(-1., 1.)
        } else {
            0.
        };
        let _ = ring.push(finite);
    }
    1
}

impl ReferenceConverter {
    pub fn new(channels: usize, rate: u32) -> Self {
        assert!((1..=32).contains(&channels) && (8_000..=384_000).contains(&rate));
        let outputs = channels.clamp(1, 2);
        let (filtered, filter_delay) = if rate == RATE {
            (None, Duration::ZERO)
        } else {
            let state = State::new(outputs, rate as usize, RATE as usize, 3)
                .expect("validated reference resampling configuration");
            let delay = Duration::from_secs_f64(state.get_input_latency() as f64 / f64::from(rate));
            (
                Some(Box::new(FilteredReference {
                    state,
                    input: [[0.; REFERENCE_INPUT]; 2],
                    output: [[0.; REFERENCE_OUTPUT]; 2],
                })),
                delay,
            )
        };
        Self {
            channels,
            outputs,
            rate,
            filtered,
            filter_delay,
            accepted: 0,
            generated: 0,
        }
    }

    pub fn outputs(&self) -> usize {
        self.outputs
    }

    /// The next converted output frame represents an EARLIER captured source
    /// time. Includes exact fractional clock phase, not rounded callback chunks.
    pub fn timestamp_delay(&self) -> Duration {
        let source = u128::from(self.accepted) * 1_000_000_000 / u128::from(self.rate);
        let output = u128::from(self.generated) * 1_000_000_000 / u128::from(RATE);
        let nanos = (self.filter_delay.as_nanos() + source).saturating_sub(output);
        Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
    }

    /// Actual captured input only: flush its final partial stage this callback.
    /// No speculative silence, waiting, callback allocation or clock rounding.
    pub fn push(&mut self, data: impl Iterator<Item = f32>, ring: &mut Producer<f32>) -> u64 {
        let mut frame = [0_f32; 32];
        let (mut filled, mut staged, mut pushed) = (0, 0, 0);
        for value in data {
            frame[filled] = if value.is_finite() { value } else { 0. };
            filled += 1;
            if filled < self.channels {
                continue;
            }
            filled = 0;
            let current = if self.outputs == 1 {
                [
                    frame[..self.channels].iter().sum::<f32>() / self.channels as f32,
                    0.,
                ]
            } else {
                let (mut left, mut right) = (0., 0.);
                for (i, sample) in frame[..self.channels].iter().enumerate() {
                    if i % 2 == 0 {
                        left += sample;
                    } else {
                        right += sample;
                    }
                }
                [
                    left / self.channels.div_ceil(2) as f32,
                    right / (self.channels / 2) as f32,
                ]
            }
            .map(|value| if value.is_finite() { value } else { 0. });
            self.accepted += 1;
            if let Some(filter) = &mut self.filtered {
                filter.input[0][staged] = current[0];
                filter.input[1][staged] = current[1];
                staged += 1;
                if staged == REFERENCE_INPUT {
                    let (generated, buffered) = filter.convert(staged, self.outputs, ring);
                    self.generated += generated;
                    pushed += buffered;
                    staged = 0;
                }
            } else {
                self.generated += 1;
                pushed += write_reference(current, self.outputs, ring);
            }
        }
        if staged > 0 {
            let (generated, buffered) =
                self.filtered
                    .as_mut()
                    .unwrap()
                    .convert(staged, self.outputs, ring);
            self.generated += generated;
            pushed += buffered;
        }
        pushed
    }
}

/// Interleaved stereo reference of what the output callback handed to the device, for
/// systems without an output recording (raw ALSA).
pub struct Played {
    pub samples: Producer<f32>,
    pub stamper: Stamper,
}

/// Capacity of a reference ring, in 48 kHz frames per channel.
pub const RING_FRAMES: usize = 40 * FRAME;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeline_maps_frames_through_the_newest_stamp() {
        let (mut stamper, mut timeline) = stamped();
        assert_eq!(timeline.time_of(0), None);
        stamper.anchor(1_000_000);
        stamper.advance(480);
        stamper.anchor(11_000_000);
        stamper.advance(480);
        assert_eq!(timeline.time_of(0), Some(1_000_000));
        assert_eq!(timeline.time_of(240), Some(6_000_000));
        // The second stamp corrects the clock (11 ms, not 10 ms).
        assert_eq!(timeline.time_of(480), Some(11_000_000));
        assert_eq!(timeline.time_of(960), Some(21_000_000));
    }

    #[test]
    fn a_late_callback_keeps_the_stamp_of_its_device_instant() {
        let mut clock = StreamClock::new(Clock::new());
        let first = clock.now_shifted(1_000_000_000, Duration::ZERO, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        // 10 ms of device time later, reported 20 ms later: the stamp moves 10 ms.
        let second = clock.now_shifted(1_010_000_000, Duration::ZERO, Duration::ZERO);
        assert_eq!(second - first, 10_000_000);
        let captured = clock.now_shifted(1_015_000_000, Duration::from_millis(3), Duration::ZERO);
        assert_eq!(captured - first, 12_000_000);
    }

    /// A reference of `frames` 48 kHz frames, value = its index, stamped
    /// from `start` (nanoseconds) with a gap of `gap` after frame `at`.
    fn recorded(frames: u64, start: u64, at: u64, gap: u64) -> Reference {
        let (mut ring, samples) = RingBuffer::new(frames as usize);
        let (mut stamper, timeline) = stamped();
        for frame in 0..frames {
            if frame == 0 || frame == at {
                let gap = if frame == at { gap } else { 0 };
                stamper.anchor(start + gap + frame * 1_000_000_000 / u64::from(RATE));
            }
            ring.push(frame as f32).unwrap();
            stamper.advance(1);
        }
        Reference::new(samples, timeline, 1)
    }

    #[test]
    fn reference_frames_are_fed_once_they_have_sounded() {
        let ms = 1_000_000;
        let lead = LEAD.as_nanos() as u64;
        let mut frame = [0.; 2 * FRAME];
        let mut reference = recorded(4 * FRAME as u64, 20 * ms + lead - 10 * ms, u64::MAX, 0);
        // The first frame starts before the lead runs out after the microphone frame.
        assert_eq!(reference.next(20 * ms, &mut frame), Step::Feed);
        assert_eq!(frame[0], 0.);
        assert_eq!(reference.next(20 * ms, &mut frame), Step::Later);
        assert_eq!(reference.next(30 * ms, &mut frame), Step::Feed);
        assert_eq!(frame[0], FRAME as f32);
        // Audio from over 300 ms before the microphone frame is skipped.
        assert_eq!(reference.next(400 * ms, &mut frame), Step::Wait);
        assert!(reference.samples.is_empty());
        let mut empty = Reference::new(RingBuffer::<f32>::new(1).1, stamped().1, 1);
        assert_eq!(empty.next(20 * ms, &mut frame), Step::Later);
    }

    #[test]
    fn audio_the_recording_lost_is_silence_and_audio_the_microphone_lost_is_skipped() {
        let ms = 1_000_000;
        let mut frame = [0.; 2 * FRAME];
        // 15 ms lost by the recording after its first frame: a silent frame,
        // then 5 ms (240 frames) of silence.
        let mut reference = recorded(3 * FRAME as u64, 15 * ms, FRAME as u64, 15 * ms);
        assert_eq!(reference.next(0, &mut frame), Step::Feed);
        assert_eq!(reference.next(10 * ms, &mut frame), Step::Feed);
        assert!(frame[..FRAME].iter().all(|&sample| sample == 0.));
        assert_eq!(reference.next(20 * ms, &mut frame), Step::Feed);
        assert!(frame[..240].iter().all(|&sample| sample == 0.));
        assert_eq!(frame[240], FRAME as f32);
        // Stamps 4 ms off are jitter: the frame follows on.
        let mut reference = recorded(3 * FRAME as u64, 15 * ms, FRAME as u64, 4 * ms);
        assert_eq!(reference.next(0, &mut frame), Step::Feed);
        assert_eq!(reference.next(10 * ms, &mut frame), Step::Feed);
        assert_eq!(frame[0], FRAME as f32);
        // The microphone lost 21 ms (about one 1024-frame device cycle): the
        // reference of that time is skipped.
        assert_eq!(lost(20 * ms, 51 * ms), Some(21 * ms));
        assert_eq!(lost(20 * ms, 39 * ms), None);
        let mut reference = recorded(5 * FRAME as u64, 15 * ms, u64::MAX, 0);
        assert_eq!(reference.next(0, &mut frame), Step::Feed);
        reference.lose(21 * ms);
        assert_eq!(reference.next(31 * ms, &mut frame), Step::Feed);
        assert_eq!(frame[0], (FRAME + 21 * 48) as f32);
    }

    #[test]
    fn stereo_stays_apart_and_surround_folds_into_two_channels() {
        let (mut ring, mut out) = RingBuffer::new(64);
        let mut stereo = ReferenceConverter::new(2, 48_000);
        assert_eq!(stereo.outputs(), 2);
        let pushed = stereo.push([0.5_f32, -0.25, 0.5, -0.25].into_iter(), &mut ring);
        assert_eq!(pushed, 2);
        let mut values = Vec::new();
        while let Ok(value) = out.pop() {
            values.push(value);
        }
        assert_eq!(values[2..], [0.5, -0.25]);
        // 5.1: FL FR FC LFE RL RR → left = mean(FL, FC, RL), right = mean(FR, LFE, RR).
        let mut surround = ReferenceConverter::new(6, 48_000);
        let frame = [0.3_f32, 0.0, 0.3, 0.0, 0.3, 0.0];
        surround.push(frame.into_iter().chain(frame), &mut ring);
        let mut last = [0.; 2];
        while let Ok(value) = out.pop() {
            last = [last[1], value];
        }
        assert!(
            (last[0] - 0.3).abs() < 1e-6 && last[1].abs() < 1e-6,
            "{last:?}"
        );
    }

    #[test]
    fn reference_filter_delay_maps_the_impulse_to_its_original_capture_instant() {
        let base = 1_000_000_000_u64;
        for rate in [
            8_000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000, 384_000,
        ] {
            let source_frames = rate as usize / 10 + 1;
            let mut input = vec![0_f32; source_frames * 2];
            input[..2].copy_from_slice(&[0.25, -0.125]);
            let (mut ring, mut output) = RingBuffer::new(12_000);
            let (mut stamper, mut timeline) = stamped();
            let mut converter = ReferenceConverter::new(2, rate);
            let (mut cursor, mut index) = (0, 0);
            let chunks = [1, 127, 37, 480, 256];
            while cursor < source_frames {
                let count = chunks[index % chunks.len()].min(source_frames - cursor);
                let capture_time = base + (cursor as u64 * 1_000_000_000 / u64::from(rate));
                // Capture-side compensation goes BACK, unlike playback delay.
                stamper.anchor(capture_time - converter.timestamp_delay().as_nanos() as u64);
                let produced = converter.push(
                    input[cursor * 2..(cursor + count) * 2].iter().copied(),
                    &mut ring,
                );
                stamper.advance(produced);
                cursor += count;
                index += 1;
            }
            let samples: Vec<_> = std::iter::from_fn(|| output.pop().ok()).collect();
            let peak = samples
                .as_chunks::<2>()
                .0
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a[0].abs().total_cmp(&b[0].abs()))
                .unwrap()
                .0;
            assert!(samples[peak * 2].abs() > 0.01);
            let represented = timeline.time_of(peak as u64).unwrap();
            assert!(
                represented.abs_diff(base) <= 1_000_000_000 / u64::from(RATE) + 2,
                "{rate}: reference impulse represented at {represented}, actual capture {base}"
            );
        }
    }

    #[test]
    fn non48_reference_clocks_and_irregular_callback_boundaries_stay_exact() {
        for rate in [11_025, 22_050, 44_100, 96_000] {
            let input: Vec<_> = (0..rate)
                .flat_map(|i| {
                    let value = (i as f32 * 0.03).sin() * 0.2;
                    [value, -0.5 * value]
                })
                .collect();
            let convert = |chunks: &[usize]| {
                let (mut ring, mut out) = RingBuffer::new(96_064);
                let mut converter = ReferenceConverter::new(2, rate);
                let mut cursor = 0;
                let mut count = 0;
                let mut index = 0;
                while cursor < input.len() {
                    let take = (chunks[index % chunks.len()] * 2).min(input.len() - cursor);
                    count +=
                        converter.push(input[cursor..cursor + take].iter().copied(), &mut ring);
                    cursor += take;
                    index += 1;
                }
                assert_eq!(count, 48_000, "one second remains one second at {rate}");
                let values: Vec<_> = std::iter::from_fn(|| out.pop().ok()).collect();
                assert_eq!(
                    values.len(),
                    96_000,
                    "reference timestamps count frames, not scalars"
                );
                values
            };
            assert_eq!(
                convert(&[rate as usize]),
                convert(&[1, 127, 480, 37, 256]),
                "{rate}"
            );
        }
    }

    #[test]
    fn mono_devices_give_a_mono_reference_at_48_khz() {
        let (mut ring, mut out) = RingBuffer::new(256);
        let mut converter = ReferenceConverter::new(1, 24_000);
        assert_eq!(converter.outputs(), 1);
        let pushed = converter.push(std::iter::repeat_n(0.25_f32, 50), &mut ring);
        assert_eq!(pushed, 100);
        let mut count = 0;
        while out.pop().is_ok() {
            count += 1;
        }
        assert_eq!(count, 100);
    }
}
