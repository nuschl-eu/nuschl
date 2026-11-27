// SPDX-License-Identifier: EUPL-1.2
//! DJ mode's signal processing on decoded 48 kHz stereo PCM that the confined
//! decode child produced and the parent validated (finite samples, whole
//! 20 ms frames): crossfader curves, gain ramps, EQ and filter biquads (the
//! `biquad` crate), a variable-rate resampler for tempo and a peak limiter.
//! Safe Rust on plain sample arrays; parsing of untrusted media stays in the
//! worker.
use super::{CHANNELS, FRAME};
use biquad::{Biquad as _, Coefficients, DirectForm1, Hertz, Q_BUTTERWORTH_F64, Type};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Samples per second and channel.
pub const RATE: f64 = 48_000.0;
/// The EQ's range per band, in dB, like a club mixer's (−26 to +6 dB).
pub const EQ_MIN_DB: f32 = -26.0;
pub const EQ_MAX_DB: f32 = 6.0;
/// Corner frequencies of the low shelf, the mid peak and the high shelf.
const EQ_LOW_HZ: f64 = 250.0;
const EQ_MID_HZ: f64 = 1_000.0;
const EQ_MID_Q: f64 = 0.7;
/// Q of the shelves: Bristow-Johnson's shelf slope of 1.
const SHELF_Q: f64 = Q_BUTTERWORTH_F64;
const EQ_HIGH_HZ: f64 = 2_500.0;
/// The filter knob's ends: the low-pass falls to this cutoff on the left, the
/// high-pass rises to this one on the right. In the middle both sit where
/// their response is flat within 0.01 dB across the audible band.
const LOW_PASS_OPEN_HZ: f64 = 23_500.0;
const LOW_PASS_CLOSED_HZ: f64 = 60.0;
const HIGH_PASS_OPEN_HZ: f64 = 3.0;
const HIGH_PASS_CLOSED_HZ: f64 = 8_000.0;
/// The filter's resonance: flat (Butterworth) in the middle, rising to a
/// slight peak at the cutoff within the first fifth of the knob's travel.
const FILTER_Q_NEUTRAL: f64 = Q_BUTTERWORTH_F64;
const FILTER_Q_TURNED: f64 = 1.0;
/// Filter coefficients follow their parameters in blocks this long (0.67 ms).
const BLOCK: usize = 32;
/// How much of the way to a new control value each frame goes: about 60 ms
/// to settle, so 30 updates a second blend into one smooth movement.
const SMOOTHING: f32 = 0.35;
/// The cut curve fades each deck over this share of the crossfader's travel
/// at its own end.
const CUT_WIDTH: f32 = 0.04;
/// The limiter keeps the mix's peaks at this level and comes back to unity
/// gain over about a second.
const CEILING: f32 = 0.98;
const LIMITER_RELEASE: f32 = 0.02;
/// The limiter reaches a lower gain within this many samples (1 ms).
const LIMITER_ATTACK: usize = 48;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Curve {
    /// Constant power: each deck at −3 dB in the middle.
    #[default]
    Smooth,
    /// Both decks at full level, cut within the last few percent at each end.
    Cut,
}

/// The gains of decks A and B for a crossfader position from −1 (A) to 1 (B).
pub fn crossfade(position: f32, curve: Curve) -> [f32; 2] {
    let travel = (position.clamp(-1.0, 1.0) + 1.0) / 2.0;
    match curve {
        Curve::Smooth => {
            let angle = travel * std::f32::consts::FRAC_PI_2;
            [angle.cos().max(0.0), angle.sin().max(0.0)]
        }
        Curve::Cut => [
            ((1.0 - travel) / CUT_WIDTH).min(1.0),
            (travel / CUT_WIDTH).min(1.0),
        ],
    }
}

/// A channel fader's gain for a level in percent; squared, so the fader
/// follows loudness, like the queue's volume.
pub fn fader_gain(volume: u8) -> f32 {
    (f32::from(volume.min(100)) / 100.0).powi(2)
}

/// One frame's step of a control value towards its target.
pub fn approach(current: f32, target: f32) -> f32 {
    let next = current + (target - current) * SMOOTHING;
    if (target - next).abs() < 1e-4 {
        target
    } else {
        next
    }
}

/// Coefficients that pass the signal unchanged.
const PASS: Coefficients<f64> = Coefficients {
    a1: 0.0,
    a2: 0.0,
    b0: 1.0,
    b1: 0.0,
    b2: 0.0,
};

/// Bristow-Johnson cookbook coefficients from the `biquad` crate. The callers
/// clamp every frequency below Nyquist and every Q above zero, the only inputs
/// the crate refuses; a refusal would pass the signal unchanged.
fn coefficients(kind: Type<f64>, hz: f64, q: f64) -> Coefficients<f64> {
    match (Hertz::from_hz(RATE), Hertz::from_hz(hz)) {
        (Ok(rate), Ok(hz)) => Coefficients::from_params(kind, rate, hz, q).unwrap_or(PASS),
        _ => PASS,
    }
}

/// The magnitude of a biquad's response at `hz`.
#[cfg(test)]
fn response(c: &Coefficients<f64>, hz: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / RATE;
    let (c1, s1) = (w.cos(), -w.sin());
    let (c2, s2) = ((2.0 * w).cos(), -(2.0 * w).sin());
    let numerator = (c.b0 + c.b1 * c1 + c.b2 * c2, c.b1 * s1 + c.b2 * s2);
    let denominator = (1.0 + c.a1 * c1 + c.a2 * c2, c.a1 * s1 + c.a2 * s2);
    numerator.0.hypot(numerator.1) / denominator.0.hypot(denominator.1)
}

/// Below this level a frame counts as silent; a silent frame with silent
/// output resets the filters, so they rest at exact zero.
const SILENCE: f32 = 1e-9;

/// The EQ and filter settings of a deck.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    /// Low, mid and high, in dB.
    pub eq: [f32; 3],
    /// −1 (low-pass closed) to 1 (high-pass closed); 0 is neutral.
    pub filter: f32,
}

impl Tone {
    pub const NEUTRAL: Self = Self {
        eq: [0.0; 3],
        filter: 0.0,
    };

    fn approach(self, target: Self) -> Self {
        Self {
            eq: [0, 1, 2].map(|band| approach(self.eq[band], target.eq[band])),
            filter: approach(self.filter, target.filter),
        }
    }

    fn mix(self, to: Self, share: f32) -> Self {
        let blend = |from: f32, to: f32| from + (to - from) * share;
        Self {
            eq: [0, 1, 2].map(|band| blend(self.eq[band], to.eq[band])),
            filter: blend(self.filter, to.filter),
        }
    }

    fn coefficients(self) -> [Coefficients<f64>; 5] {
        let eq = self.eq.map(|db| f64::from(db.clamp(EQ_MIN_DB, EQ_MAX_DB)));
        let filter = f64::from(self.filter.clamp(-1.0, 1.0));
        let low_pass =
            LOW_PASS_OPEN_HZ * (LOW_PASS_CLOSED_HZ / LOW_PASS_OPEN_HZ).powf((-filter).max(0.0));
        let high_pass =
            HIGH_PASS_OPEN_HZ * (HIGH_PASS_CLOSED_HZ / HIGH_PASS_OPEN_HZ).powf(filter.max(0.0));
        let q =
            FILTER_Q_NEUTRAL + (FILTER_Q_TURNED - FILTER_Q_NEUTRAL) * (filter.abs() * 5.0).min(1.0);
        [
            coefficients(Type::LowShelf(eq[0]), EQ_LOW_HZ, SHELF_Q),
            coefficients(Type::PeakingEQ(eq[1]), EQ_MID_HZ, EQ_MID_Q),
            coefficients(Type::HighShelf(eq[2]), EQ_HIGH_HZ, SHELF_Q),
            coefficients(Type::LowPass, low_pass, q),
            coefficients(Type::HighPass, high_pass, q),
        ]
    }
}

/// Three EQ bands, then the filter's low-pass and high-pass, in series, each a
/// direct form I biquad per channel (that form tolerates coefficients changing
/// between blocks).
#[derive(Clone, Debug)]
pub struct ToneFilter {
    stages: [[DirectForm1<f64>; CHANNELS]; 5],
    tone: Tone,
}

impl Default for ToneFilter {
    fn default() -> Self {
        Self {
            stages: Tone::NEUTRAL
                .coefficients()
                .map(|coefficients| [DirectForm1::<f64>::new(coefficients); CHANNELS]),
            tone: Tone::NEUTRAL,
        }
    }
}

impl ToneFilter {
    pub fn reset(&mut self) {
        for filter in self.stages.iter_mut().flatten() {
            filter.reset_state();
        }
    }

    /// Filters one frame of interleaved stereo samples in place while the
    /// settings move one smoothing step towards `target`, block by block.
    pub fn process(&mut self, frame: &mut [f32], target: Tone) {
        let from = self.tone;
        let to = from.approach(target);
        self.tone = to;
        let samples = frame.len() / CHANNELS;
        let blocks = samples.div_ceil(BLOCK).max(1);
        let moving = from != to;
        let silent_input = frame.iter().all(|sample| sample.abs() < SILENCE);
        for (index, block) in frame.chunks_mut(BLOCK * CHANNELS).enumerate() {
            if moving || index == 0 {
                let share = (index + 1) as f32 / blocks as f32;
                let coefficients = from.mix(to, share).coefficients();
                for (stage, coefficients) in self.stages.iter_mut().zip(coefficients) {
                    for filter in stage {
                        filter.update_coefficients(coefficients);
                    }
                }
            }
            for samples in block.as_chunks_mut::<CHANNELS>().0 {
                for (channel, sample) in samples.iter_mut().enumerate() {
                    let mut value = f64::from(*sample);
                    for stage in &mut self.stages {
                        value = stage[channel].run(value);
                    }
                    *sample = value as f32;
                }
            }
        }
        // A silent frame whose filter tails have died away resets the
        // filters, which then rest at exact zero.
        if silent_input && frame.iter().all(|sample| sample.abs() < SILENCE) {
            self.reset();
            frame.fill(0.0);
        }
    }

    /// The magnitude of the whole chain's response at `hz` for `tone`.
    #[cfg(test)]
    fn response(tone: Tone, hz: f64) -> f64 {
        tone.coefficients()
            .iter()
            .map(|stage| response(stage, hz))
            .product()
    }
}

/// Plays decoded PCM faster or slower, with the pitch following the speed as
/// on a turntable. Keeps only the input samples it still needs; positions are
/// absolute sample indices of the song.
#[derive(Clone, Debug, Default)]
pub struct Resampler {
    samples: VecDeque<[f32; CHANNELS]>,
    /// The song sample index of `samples[0]`.
    start: u64,
    /// The song position of the next output sample, in input samples.
    position: f64,
    /// The input has ended; samples past it read as silence.
    ended: bool,
}

impl Resampler {
    /// Starts over with input that begins at sample `start` and output that
    /// begins at `position` (at or after `start`).
    pub fn reset(&mut self, start: u64, position: u64) {
        self.reset_at(start, position as f64);
    }

    /// [`Resampler::reset`] to a position between samples, as a loop's start.
    pub fn reset_at(&mut self, start: u64, position: f64) {
        self.samples.clear();
        self.start = start;
        self.position = position.max(start as f64);
        self.ended = false;
    }

    pub fn push(&mut self, frame: &[f32]) {
        self.samples.extend(
            frame
                .as_chunks::<CHANNELS>()
                .0
                .iter()
                .map(|pair| pair.map(|sample| sample.clamp(-1.0, 1.0))),
        );
    }

    pub fn end(&mut self) {
        self.ended = true;
    }

    /// Input samples remain to be played.
    pub fn unplayed(&self) -> bool {
        self.input_end() as f64 > self.position
    }

    /// Whether the input also holds the frame after the next, so a deck that
    /// moves away can still fade out what it was playing.
    pub fn ready_ahead(&self, from: f64, to: f64) -> bool {
        self.ended || self.input_end() >= self.needed(from, to) + (FRAME as f64 * to).ceil() as u64
    }

    /// The song position, in samples.
    pub fn position(&self) -> f64 {
        self.position
    }

    fn input_end(&self) -> u64 {
        self.start + self.samples.len() as u64
    }

    /// Every input sample has been played.
    pub fn drained(&self) -> bool {
        self.ended && self.position >= self.input_end() as f64
    }

    /// Whether the input holds every sample a frame at ratios `from` to `to` reads.
    pub fn ready(&self, from: f64, to: f64) -> bool {
        self.ended || self.input_end() >= self.needed(from, to)
    }

    fn needed(&self, from: f64, to: f64) -> u64 {
        let advance = FRAME as f64 * (from + to) / 2.0;
        // The interpolation reads two samples past the last position.
        (self.position + advance).floor() as u64 + 3
    }

    fn sample(&self, index: i64) -> [f32; CHANNELS] {
        let first = self.start as i64;
        let index = index.max(first);
        self.samples
            .get((index - first) as usize)
            .copied()
            .unwrap_or([0.0; CHANNELS])
    }

    /// Writes one frame, the speed moving linearly from ratio `from` to `to`.
    pub fn render(&mut self, from: f64, to: f64, frame: &mut [f32]) {
        let total = frame.len() / CHANNELS;
        self.render_part(from, to, total, 0, frame, None);
    }

    /// Writes samples `first..` of a frame of `total` samples into `out`, the
    /// speed moving linearly from `from` to `to` across the whole frame, and
    /// stops once the position reaches `limit`; returns the samples written.
    pub fn render_part(
        &mut self,
        from: f64,
        to: f64,
        total: usize,
        first: usize,
        out: &mut [f32],
        limit: Option<f64>,
    ) -> usize {
        let mut written = 0;
        for (offset, out) in out.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            if limit.is_some_and(|limit| self.position >= limit) {
                break;
            }
            let ratio = from + (to - from) * (first + offset + 1) as f64 / total.max(1) as f64;
            let whole = self.position.floor();
            let t = (self.position - whole) as f32;
            let at = whole as i64;
            let [before, current, next, after] =
                [at - 1, at, at + 1, at + 2].map(|index| self.sample(index));
            for channel in 0..CHANNELS {
                let (xm1, x0, x1, x2) = (
                    before[channel],
                    current[channel],
                    next[channel],
                    after[channel],
                );
                // Catmull-Rom: exact at whole positions, smooth in between.
                out[channel] = x0
                    + 0.5
                        * t
                        * (x1 - xm1
                            + t * (2.0 * xm1 - 5.0 * x0 + 4.0 * x1 - x2
                                + t * (3.0 * (x0 - x1) + x2 - xm1)));
            }
            self.position += ratio;
            written += 1;
        }
        // Keep one sample before the position for the next interpolation.
        let keep_from = (self.position.floor() as u64).saturating_sub(1);
        while self.start < keep_from && !self.samples.is_empty() {
            self.samples.pop_front();
            self.start += 1;
        }
        if self.samples.is_empty() {
            self.start = self.start.max(keep_from);
        }
        written
    }
}

/// A gain that moves towards its target by one smoothing step a frame and
/// ramps linearly within the frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ramp {
    pub current: f32,
}

impl Ramp {
    /// The gains at the start and end of the next frame.
    pub fn step(&mut self, target: f32) -> (f32, f32) {
        let from = self.current;
        self.current = approach(from, target);
        (from, self.current)
    }
}

/// Multiplies `frame` by a gain moving linearly from `from` to `to` and adds
/// it to `into`.
pub fn mix_into(into: &mut [f32], frame: &[f32], from: f32, to: f32) {
    let count = (frame.len() / CHANNELS).max(1) as f32;
    for (index, (out, samples)) in into
        .as_chunks_mut::<CHANNELS>()
        .0
        .iter_mut()
        .zip(frame.as_chunks::<CHANNELS>().0)
        .enumerate()
    {
        let gain = from + (to - from) * (index + 1) as f32 / count;
        for (out, sample) in out.iter_mut().zip(samples) {
            *out += sample * gain;
        }
    }
}

/// Scales `frame` by a gain moving linearly from `from` to `to`.
pub fn scale(frame: &mut [f32], from: f32, to: f32) {
    let count = (frame.len() / CHANNELS).max(1) as f32;
    for (index, samples) in frame.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
        let gain = from + (to - from) * (index + 1) as f32 / count;
        for sample in samples {
            *sample *= gain;
        }
    }
}

/// Keeps the summed decks within full scale: a lower gain within 1 ms of a
/// louder frame, back to unity over about a second.
#[derive(Clone, Copy, Debug)]
pub struct Limiter {
    gain: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Limiter {
    pub fn process(&mut self, frame: &mut [f32]) {
        let peak = frame
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let wanted = if peak > CEILING { CEILING / peak } else { 1.0 };
        let from = self.gain;
        let to = if wanted < from {
            wanted
        } else {
            let next = from + (wanted - from) * LIMITER_RELEASE;
            if (wanted - next).abs() < 1e-4 {
                wanted
            } else {
                next
            }
        };
        self.gain = to;
        let attack = if to < from { LIMITER_ATTACK } else { FRAME };
        for (index, samples) in frame.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            let share = ((index + 1) as f32 / attack as f32).min(1.0);
            let gain = from + (to - from) * share;
            for sample in samples {
                *sample = (*sample * gain).clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(gain: f64) -> f64 {
        20.0 * gain.log10()
    }

    #[test]
    fn smooth_crossfade_keeps_constant_power() {
        let [a, b] = crossfade(0.0, Curve::Smooth);
        assert!((a - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!((a - b).abs() < 1e-6);
        for step in -10..=10 {
            let [a, b] = crossfade(step as f32 / 10.0, Curve::Smooth);
            assert!((a * a + b * b - 1.0).abs() < 1e-5, "power at {step}");
        }
        assert_eq!(crossfade(-1.0, Curve::Smooth), [1.0, 0.0]);
        assert_eq!(crossfade(1.0, Curve::Smooth), [0.0, 1.0]);
        // Out-of-range positions read as the nearest end.
        assert_eq!(crossfade(-3.0, Curve::Smooth), [1.0, 0.0]);
    }

    #[test]
    fn cut_crossfade_switches_within_its_edges() {
        assert_eq!(crossfade(-1.0, Curve::Cut), [1.0, 0.0]);
        assert_eq!(crossfade(1.0, Curve::Cut), [0.0, 1.0]);
        assert_eq!(crossfade(0.0, Curve::Cut), [1.0, 1.0]);
        // Both decks are at full level once the fader leaves its edge.
        let edge = -1.0 + 2.0 * CUT_WIDTH + 1e-4;
        assert_eq!(crossfade(edge, Curve::Cut), [1.0, 1.0]);
        let [a, b] = crossfade(-1.0 + CUT_WIDTH, Curve::Cut);
        assert_eq!(a, 1.0);
        assert!((b - 0.5).abs() < 1e-5);
        let [a, b] = crossfade(1.0 - CUT_WIDTH, Curve::Cut);
        assert!((a - 0.5).abs() < 1e-5);
        assert_eq!(b, 1.0);
    }

    #[test]
    fn neutral_tone_is_transparent() {
        for hz in [20.0, 100.0, 1_000.0, 5_000.0, 10_000.0, 16_000.0] {
            let gain = ToneFilter::response(Tone::NEUTRAL, hz);
            assert!(db(gain).abs() < 0.01, "{hz} Hz: {} dB", db(gain));
        }
        // And on actual samples: a 1 kHz tone comes out as it went in.
        let mut filter = ToneFilter::default();
        let tone = |index: usize| {
            (std::f64::consts::TAU * 1_000.0 * index as f64 / RATE).sin() as f32 * 0.5
        };
        let mut largest: f32 = 0.0;
        for frame in 0..50 {
            let mut samples: Vec<f32> = (0..FRAME)
                .flat_map(|index| [tone(frame * FRAME + index); CHANNELS])
                .collect();
            let original = samples.clone();
            filter.process(&mut samples, Tone::NEUTRAL);
            if frame > 5 {
                for (out, input) in samples.iter().zip(&original) {
                    largest = largest.max((out - input).abs());
                }
            }
        }
        assert!(
            largest < 5e-3,
            "neutral filter changed samples by {largest}"
        );
    }

    #[test]
    fn filter_tails_die_away_and_silence_rests_at_zero() {
        // A loud low tone through a boosted low shelf and a turned filter,
        // then silence: the tails ring for a while, then frames are exactly zero.
        let tone = Tone {
            eq: [EQ_MAX_DB, 0.0, 0.0],
            filter: -0.5,
        };
        let mut filter = ToneFilter::default();
        for frame in 0..20 {
            let mut samples: Vec<f32> = (0..FRAME)
                .flat_map(|index| {
                    let t = (frame * FRAME + index) as f64 / RATE;
                    [(std::f64::consts::TAU * 80.0 * t).sin() as f32 * 0.9; CHANNELS]
                })
                .collect();
            filter.process(&mut samples, tone);
        }
        let mut silence = vec![0.0f32; FRAME * CHANNELS];
        filter.process(&mut silence, tone);
        assert!(
            silence.iter().any(|sample| *sample != 0.0),
            "the tail rings on"
        );
        let mut zero_from = None;
        for frame in 0..500 {
            let mut silence = vec![0.0f32; FRAME * CHANNELS];
            filter.process(&mut silence, tone);
            let zero = silence.iter().all(|sample| *sample == 0.0);
            match (zero, zero_from) {
                (true, None) => zero_from = Some(frame),
                (false, Some(_)) => panic!("silence started again at frame {frame}"),
                _ => {}
            }
        }
        assert!(zero_from.is_some_and(|frame| frame < 100), "{zero_from:?}");
    }

    #[test]
    fn eq_bands_cut_and_boost_their_frequencies() {
        let low = Tone {
            eq: [EQ_MIN_DB, 0.0, 0.0],
            filter: 0.0,
        };
        assert!(db(ToneFilter::response(low, 30.0)) < -24.0);
        assert!(db(ToneFilter::response(low, 10_000.0)).abs() < 0.5);
        let mid = Tone {
            eq: [0.0, EQ_MIN_DB, 0.0],
            filter: 0.0,
        };
        assert!((db(ToneFilter::response(mid, EQ_MID_HZ)) - f64::from(EQ_MIN_DB)).abs() < 0.1);
        assert!(db(ToneFilter::response(mid, 30.0)).abs() < 1.0);
        let high = Tone {
            eq: [0.0, 0.0, EQ_MAX_DB],
            filter: 0.0,
        };
        assert!((db(ToneFilter::response(high, 15_000.0)) - 6.0).abs() < 0.3);
        assert!(db(ToneFilter::response(high, 50.0)).abs() < 0.1);
    }

    #[test]
    fn filter_knob_sweeps_low_pass_left_and_high_pass_right() {
        let left = Tone {
            eq: [0.0; 3],
            filter: -1.0,
        };
        assert!(db(ToneFilter::response(left, 1_000.0)) < -40.0);
        assert!(db(ToneFilter::response(left, 30.0)) > -1.0);
        let right = Tone {
            eq: [0.0; 3],
            filter: 1.0,
        };
        assert!(db(ToneFilter::response(right, 200.0)) < -50.0);
        assert!(db(ToneFilter::response(right, 16_000.0)) > -1.0);
        let halfway = Tone {
            eq: [0.0; 3],
            filter: -0.5,
        };
        // The cutoff halfway sits near 1.2 kHz.
        assert!(db(ToneFilter::response(halfway, 300.0)) > -1.0);
        assert!(db(ToneFilter::response(halfway, 8_000.0)) < -20.0);
    }

    fn sine(hz: f64, index: u64) -> f32 {
        (std::f64::consts::TAU * hz * index as f64 / RATE).sin() as f32 * 0.5
    }

    #[test]
    fn resampler_frames_have_fixed_length_and_continue_the_input() {
        for tempo in [0.84, 0.92, 1.0, 1.08, 1.16] {
            let mut resampler = Resampler::default();
            resampler.reset(9_600, 9_600);
            let mut next_input = 9_600u64;
            let mut largest: f32 = 0.0;
            for _ in 0..100 {
                while !resampler.ready(tempo, tempo) {
                    let frame: Vec<f32> = (next_input..next_input + FRAME as u64)
                        .flat_map(|index| [sine(440.0, index); CHANNELS])
                        .collect();
                    next_input += FRAME as u64;
                    resampler.push(&frame);
                }
                let start = resampler.position();
                let mut out = vec![0.0; FRAME * CHANNELS];
                resampler.render(tempo, tempo, &mut out);
                assert_eq!(out.len(), FRAME * CHANNELS);
                assert!((resampler.position() - start - FRAME as f64 * tempo).abs() < 1e-6);
                for (index, pair) in out.as_chunks::<CHANNELS>().0.iter().enumerate() {
                    let at = start + index as f64 * tempo;
                    let expected = ((std::f64::consts::TAU * 440.0 * at / RATE).sin() * 0.5) as f32;
                    largest = largest.max((pair[0] - expected).abs());
                    assert_eq!(pair[0], pair[1]);
                }
                // Only a few frames of input are ever kept.
                assert!(resampler.samples.len() < 3 * FRAME);
            }
            // Below −60 dB of full scale at the slowest tempo.
            assert!(largest < 1e-3, "tempo {tempo}: error {largest}");
        }
    }

    #[test]
    fn unit_tempo_passes_samples_through_exactly() {
        let mut resampler = Resampler::default();
        resampler.reset(0, 0);
        let input: Vec<f32> = (0..3 * FRAME as u64)
            .flat_map(|index| [sine(1_234.0, index), -sine(1_234.0, index)])
            .collect();
        resampler.push(&input);
        let mut out = vec![0.0; FRAME * CHANNELS];
        resampler.render(1.0, 1.0, &mut out);
        assert_eq!(out, input[..FRAME * CHANNELS]);
        resampler.render(1.0, 1.0, &mut out);
        assert_eq!(out, input[FRAME * CHANNELS..2 * FRAME * CHANNELS]);
    }

    #[test]
    fn tempo_changes_glide_within_a_frame() {
        let mut resampler = Resampler::default();
        resampler.reset(0, 0);
        resampler.push(&vec![0.0; 4 * FRAME * CHANNELS]);
        let mut out = vec![0.0; FRAME * CHANNELS];
        resampler.render(1.0, 1.08, &mut out);
        let advance = resampler.position();
        assert!((advance - FRAME as f64 * 1.04).abs() < 0.1, "{advance}");
    }

    #[test]
    fn a_seek_starts_mid_frame_and_ended_input_drains_to_silence() {
        let mut resampler = Resampler::default();
        // The decoder skips whole frames; the cue point lies 100 samples in.
        resampler.reset(960, 1_060);
        let input: Vec<f32> = (960..1_920u64)
            .flat_map(|index| [index as f32 / 10_000.0; CHANNELS])
            .collect();
        resampler.push(&input);
        assert!(!resampler.ready(1.0, 1.0));
        resampler.end();
        assert!(resampler.ready(1.0, 1.0));
        let mut out = vec![1.0; FRAME * CHANNELS];
        resampler.render(1.0, 1.0, &mut out);
        assert_eq!(out[0], 0.106);
        assert_eq!(out[(859 - 1) * CHANNELS], 0.1918);
        assert!(out[860 * CHANNELS..].iter().all(|sample| *sample == 0.0));
        assert!(resampler.drained());
    }

    #[test]
    fn gain_ramps_settle_without_steps() {
        let mut ramp = Ramp::default();
        let mut frames = 0;
        let mut largest_step: f32 = 0.0;
        let mut previous = 0.0;
        while ramp.current != 1.0 {
            let (from, to) = ramp.step(1.0);
            let mut frame = vec![1.0; FRAME * CHANNELS];
            scale(&mut frame, from, to);
            for pair in frame.as_chunks::<CHANNELS>().0 {
                largest_step = largest_step.max((pair[0] - previous).abs());
                previous = pair[0];
            }
            frames += 1;
            assert!(frames < 40);
        }
        // A full jump of a fader settles within about half a second, in steps
        // below 0.04 % of full scale per sample.
        assert!(frames <= 25, "{frames} frames");
        assert!(largest_step < 4e-4, "{largest_step}");
        let mut into = vec![0.5; FRAME * CHANNELS];
        mix_into(&mut into, &vec![1.0; FRAME * CHANNELS], 0.0, 1.0);
        assert!((into[0] - (0.5 + 1.0 / FRAME as f32)).abs() < 1e-6);
        assert_eq!(*into.last().unwrap(), 1.5);
    }

    #[test]
    fn limiter_keeps_peaks_under_full_scale_and_recovers() {
        let mut limiter = Limiter::default();
        let mut quiet = vec![0.5; FRAME * CHANNELS];
        limiter.process(&mut quiet);
        assert!(quiet.iter().all(|sample| *sample == 0.5));
        let mut loud = vec![1.4; FRAME * CHANNELS];
        limiter.process(&mut loud);
        assert!(loud.iter().all(|sample| *sample <= 1.0));
        assert!((loud.last().unwrap() - CEILING).abs() < 1e-4);
        for _ in 0..600 {
            let mut frame = vec![0.1; FRAME * CHANNELS];
            limiter.process(&mut frame);
        }
        assert_eq!(limiter.gain, 1.0);
    }
}
