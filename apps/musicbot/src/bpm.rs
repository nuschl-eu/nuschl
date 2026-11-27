//! Tempo (BPM) estimation and constant-tempo beat grids for decoded PCM.
//!
//! [`Onsets`] downmixes interleaved stereo at 48 kHz to mono, decimates it to 8 kHz and
//! computes a spectral-flux onset envelope at 100 Hz: a 256-point Hann STFT (32 ms window,
//! 10 ms hop) grouped into 15 log-spaced bands, each band compressed to `ln(1 + level /
//! LEVEL_REF)`, with the positive band differences averaged per frame.
//!
//! [`Analyzer::finish`] turns the first [`MAX_SECONDS`] of that envelope into a tempo:
//!
//! 1. The envelope is high-passed (minus a centred one-second mean) and smoothed with a
//!    three-tap kernel. A comb over its autocorrelation (lags of one to four beats) scores
//!    60-200 BPM in 0.25 BPM steps; the best score gives the pulse.
//! 2. The pulse and its octaves inside 60-200 BPM are refined in 0.05 BPM steps plus
//!    parabolic interpolation on the summed Fourier magnitudes of the envelope at the first
//!    four harmonics of the tempo.
//! 3. Metrical level: the fastest of those octaves whose autocorrelation at one beat reaches
//!    `LEVEL_RATIO` (0.7) of the strongest one wins.
//! 4. Octave preference: a tempo outside 80-170 BPM moves to its double or half inside that
//!    range when the envelope's Fourier magnitude at that tempo reaches `PREFER_RATIO` (0.5)
//!    of the magnitude at the chosen tempo.
//! 5. Confidence is the mean normalized autocorrelation at one to four beats of the result;
//!    `finish` gives `None` below `MIN_CONFIDENCE` (0.15).
//!
//! [`beat_grid`] fits a pulse train at a given tempo to the rectified, high-passed envelope
//! of a whole song. The drift of the Fourier phase at the tempo's strongest harmonic across
//! five-second blocks corrects the tempo by at most 0.5 %; a phase scan of a kernel made of
//! the first four harmonics then finds the beat phase in 0.5 ms steps with parabolic
//! refinement, shifted by the onset detector's calibrated latency. Confidence is the kernel's
//! score at that phase over its score for a perfectly aligned envelope (`GRID_MIN_CONFIDENCE`
//! 0.15).

use std::f64::consts::TAU;

/// Input: interleaved stereo f32 at 48 kHz, fed in frames of any even length (normally 960
/// samples per channel = 1920 values).
pub const RATE: u32 = 48_000;
/// The estimator reads at most this much audio; later input is ignored.
pub const MAX_SECONDS: f32 = 60.0;
/// Below this much audio `finish` gives None.
pub const MIN_SECONDS: f32 = 10.0;
/// The longest audio the onset envelope covers (bounded memory).
pub const MAX_GRID_SECONDS: f32 = 1800.0;

/// Decimation factor from [`RATE`] to the analysis rate.
const DECIMATION: usize = 6;
/// Analysis rate in Hz.
const LOW_RATE: usize = RATE as usize / DECIMATION;
/// STFT window length at the analysis rate.
const WIN: usize = 256;
/// STFT hop at the analysis rate.
const HOP: usize = 80;
/// Onset envelope rate in Hz.
const ENV_RATE: f32 = (LOW_RATE / HOP) as f32;
/// Growth of the band edges in FFT bins.
const BAND_GROWTH: f32 = 1.35;
/// Band level (relative to full scale) where the log compression turns from linear to
/// logarithmic.
const LEVEL_REF: f32 = 0.01;
const MAX_FRAMES: usize = MAX_SECONDS as usize * RATE as usize;
const MIN_FRAMES: usize = MIN_SECONDS as usize * RATE as usize;
const MAX_GRID_FRAMES: usize = MAX_GRID_SECONDS as usize * RATE as usize;
/// Half width in envelope frames of the moving mean the high-pass subtracts.
const HIGH_PASS_HALF: usize = 50;
/// Mean onset envelope below which the audio counts as steady (silence, a held tone).
const MIN_FLUX: f64 = 2e-3;

const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;
const PREFER_LOW: f32 = 80.0;
const PREFER_HIGH: f32 = 170.0;
const GRID_STEP: f32 = 0.25;
const REFINE_STEP: f32 = 0.05;
/// Beats the autocorrelation comb spans.
const COMB_BEATS: usize = 4;
/// Harmonics in the Fourier tempo strength and in the beat grid kernel.
const HARMONICS: usize = 4;
/// Longest autocorrelation lag: four beats at 55 BPM.
const MAX_LAG: usize = (COMB_BEATS as f32 * 60.0 * ENV_RATE / 55.0) as usize + 2;
/// A faster metrical level wins when its one-beat autocorrelation reaches this share of
/// the strongest level.
const LEVEL_RATIO: f64 = 0.7;
/// An octave inside 80-170 BPM replaces a tempo outside it when its Fourier magnitude
/// reaches this share of the tempo's.
const PREFER_RATIO: f64 = 0.5;
/// Tempo estimates below this confidence give None.
const MIN_CONFIDENCE: f32 = 0.15;

/// Tempo range `beat_grid` accepts.
const GRID_MIN_BPM: f32 = 30.0;
const GRID_MAX_BPM: f32 = 300.0;
/// Shortest audio `beat_grid` fits.
const GRID_MIN_SECONDS: f32 = 5.0;
/// Envelope frames per block of the tempo correction.
const GRID_BLOCK: usize = 500;
/// Largest relative tempo correction of `beat_grid`.
const GRID_TEMPO_RANGE: f32 = 0.005;
/// Phase scan step in envelope frames (0.5 ms).
const PHASE_STEP: f64 = 0.05;
/// Grids below this confidence give None.
const GRID_MIN_CONFIDENCE: f32 = 0.15;
/// Time from the start of envelope frame 0 to a transient that peaks the envelope at frame
/// 0: the onset detector's latency, calibrated on synthetic kicks and clicks.
const ONSET_OFFSET_MS: f64 = 17.4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    /// Tempo rounded to 0.1 BPM.
    pub bpm: f32,
    /// Periodicity of the onsets at that tempo, 0..1.
    pub confidence: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    /// Grid tempo: the requested tempo refined by at most 0.5 %.
    pub bpm: f32,
    /// Milliseconds of the first beat at or after 0, in `[0, 60000 / bpm)`.
    pub first_beat_ms: f64,
    /// Alignment of the onsets with the grid, 0..1.
    pub confidence: f32,
}

/// Onset envelope of any length of audio, from the song's start (same input format as
/// [`Analyzer`]). It covers at most [`MAX_GRID_SECONDS`] and ignores later input.
pub struct Onsets {
    /// Stereo frames this envelope reads at most.
    limit: usize,
    /// Stereo frames taken.
    frames: usize,
    acc: f32,
    acc_len: usize,
    ring: [f32; WIN],
    ring_pos: usize,
    low_len: usize,
    window: [f32; WIN],
    cos: [f32; WIN / 2],
    sin: [f32; WIN / 2],
    bitrev: [usize; WIN],
    bands: Vec<(usize, usize)>,
    prev: Vec<f32>,
    re: [f32; WIN],
    im: [f32; WIN],
    env: Vec<f32>,
}

impl Default for Onsets {
    fn default() -> Self {
        Self::new()
    }
}

impl Onsets {
    pub fn new() -> Self {
        Self::with_limit(MAX_GRID_FRAMES, 0)
    }

    fn with_limit(limit: usize, capacity: usize) -> Self {
        let mut window = [0.0; WIN];
        for (i, w) in window.iter_mut().enumerate() {
            *w = 0.5 - 0.5 * (TAU * i as f64 / WIN as f64).cos() as f32;
        }
        let mut cos = [0.0; WIN / 2];
        let mut sin = [0.0; WIN / 2];
        for k in 0..WIN / 2 {
            let angle = TAU * k as f64 / WIN as f64;
            cos[k] = angle.cos() as f32;
            sin[k] = -angle.sin() as f32;
        }
        let bits = WIN.trailing_zeros();
        let mut bitrev = [0; WIN];
        for (i, r) in bitrev.iter_mut().enumerate() {
            *r = i.reverse_bits() >> (usize::BITS - bits);
        }
        let mut bands = Vec::new();
        let mut lo = 1;
        while lo <= WIN / 2 {
            let hi = ((lo as f32 * BAND_GROWTH).round() as usize)
                .max(lo + 1)
                .min(WIN / 2 + 1);
            bands.push((lo, hi));
            lo = hi;
        }
        let prev = vec![0.0; bands.len()];
        Self {
            limit,
            frames: 0,
            acc: 0.0,
            acc_len: 0,
            ring: [0.0; WIN],
            ring_pos: 0,
            low_len: 0,
            window,
            cos,
            sin,
            bitrev,
            bands,
            prev,
            re: [0.0; WIN],
            im: [0.0; WIN],
            env: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, stereo: &[f32]) {
        for pair in stereo.as_chunks::<2>().0 {
            if self.frames >= self.limit {
                return;
            }
            self.frames += 1;
            self.acc += finite(pair[0]) + finite(pair[1]);
            self.acc_len += 1;
            if self.acc_len == DECIMATION {
                let sample = self.acc / (2 * DECIMATION) as f32;
                self.acc = 0.0;
                self.acc_len = 0;
                self.push_low(sample);
            }
        }
    }

    /// Audio taken so far in seconds.
    #[cfg(test)]
    pub fn seconds(&self) -> f32 {
        self.frames as f32 / RATE as f32
    }

    fn push_low(&mut self, sample: f32) {
        self.ring[self.ring_pos] = sample;
        self.ring_pos = (self.ring_pos + 1) % WIN;
        self.low_len += 1;
        if self.low_len >= WIN && (self.low_len - WIN).is_multiple_of(HOP) {
            self.frame();
        }
    }

    /// Computes one envelope value from the window in the ring.
    fn frame(&mut self) {
        for i in 0..WIN {
            let slot = self.bitrev[i];
            self.re[slot] = self.ring[(self.ring_pos + i) % WIN] * self.window[i];
            self.im[slot] = 0.0;
        }
        let mut len = 2;
        while len <= WIN {
            let half = len / 2;
            let step = WIN / len;
            for start in (0..WIN).step_by(len) {
                for k in 0..half {
                    let (wr, wi) = (self.cos[k * step], self.sin[k * step]);
                    let a = start + k;
                    let b = a + half;
                    let tr = self.re[b] * wr - self.im[b] * wi;
                    let ti = self.re[b] * wi + self.im[b] * wr;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
            }
            len *= 2;
        }
        let gain = WIN as f32 / 2.0;
        let mut flux = 0.0;
        for (band, &(lo, hi)) in self.bands.iter().enumerate() {
            let power: f32 = (lo..hi)
                .map(|k| self.re[k] * self.re[k] + self.im[k] * self.im[k])
                .sum::<f32>()
                / (hi - lo) as f32;
            let level = (1.0 + power.sqrt() / gain / LEVEL_REF).ln();
            if !self.env.is_empty() {
                flux += (level - self.prev[band]).max(0.0);
            }
            self.prev[band] = level;
        }
        self.env.push(flux / self.bands.len() as f32);
    }
}

/// Streaming tempo estimator over the first [`MAX_SECONDS`] of a song.
pub struct Analyzer {
    onsets: Onsets,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        Self {
            onsets: Onsets::with_limit(MAX_FRAMES, envelope_len(MAX_FRAMES)),
        }
    }

    pub fn push(&mut self, stereo: &[f32]) {
        self.onsets.push(stereo);
    }

    /// Audio taken so far in seconds, at most [`MAX_SECONDS`].
    #[cfg(test)]
    pub fn seconds(&self) -> f32 {
        self.onsets.seconds()
    }

    /// Whether [`MAX_SECONDS`] of audio are in.
    #[cfg(test)]
    pub fn full(&self) -> bool {
        self.onsets.frames >= MAX_FRAMES
    }

    /// The tempo, or None for less than [`MIN_SECONDS`] of audio or a confidence below the
    /// threshold.
    pub fn finish(&self) -> Option<Estimate> {
        if self.onsets.frames < MIN_FRAMES {
            return None;
        }
        estimate(&self.onsets.env)
    }
}

/// Constant-tempo grid at `bpm` (the library's BPM or the estimate): the phase that best
/// matches a pulse train at that tempo, refined to about 1 ms. The grid tempo is `bpm`
/// corrected by at most 0.5 % towards the onsets' tempo. None below a confidence threshold
/// (silence, noise, or onsets unrelated to the tempo).
pub fn beat_grid(onsets: &Onsets, bpm: f32) -> Option<Grid> {
    if !(GRID_MIN_BPM..=GRID_MAX_BPM).contains(&bpm) {
        return None;
    }
    let raw = &onsets.env;
    if (raw.len() as f32) < GRID_MIN_SECONDS * ENV_RATE || mean(raw) < MIN_FLUX {
        return None;
    }
    let env: Vec<f32> = high_pass(raw).into_iter().map(|x| x.max(0.0)).collect();
    let total: f64 = env.iter().map(|&x| f64::from(x)).sum();
    if total <= 0.0 {
        return None;
    }
    let mut tempo = bpm;
    for _ in 0..2 {
        tempo = correct_tempo(&env, tempo).clamp(
            bpm * (1.0 - GRID_TEMPO_RANGE),
            bpm * (1.0 + GRID_TEMPO_RANGE),
        );
    }
    let period = f64::from(lag(tempo));
    let z = harmonics(&env, 1.0 / period);
    let score = |phase: f64| -> f64 {
        z.iter()
            .enumerate()
            .map(|(h, &(re, im))| {
                let angle = TAU * (h + 1) as f64 * phase / period;
                re * angle.cos() - im * angle.sin()
            })
            .sum()
    };
    let steps = (period / PHASE_STEP).ceil() as usize;
    let scores: Vec<f64> = (0..steps).map(|i| score(i as f64 * PHASE_STEP)).collect();
    let (best, &peak) = scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))?;
    let offset = parabolic(
        scores[(best + steps - 1) % steps],
        peak,
        scores[(best + 1) % steps],
    );
    let phase = (best as f64 + offset) * PHASE_STEP;
    let confidence = (score(phase) / (HARMONICS as f64 * total)).clamp(0.0, 1.0) as f32;
    if confidence < GRID_MIN_CONFIDENCE {
        return None;
    }
    let period_ms = 60_000.0 / f64::from(tempo);
    let first_beat_ms =
        (phase * 1000.0 / f64::from(ENV_RATE) + ONSET_OFFSET_MS).rem_euclid(period_ms);
    Some(Grid {
        bpm: tempo,
        first_beat_ms,
        confidence,
    })
}

/// Envelope frames for `frames` stereo frames of audio.
const fn envelope_len(frames: usize) -> usize {
    let low = frames / DECIMATION;
    if low < WIN { 0 } else { (low - WIN) / HOP + 1 }
}

fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

fn mean(values: &[f32]) -> f64 {
    values.iter().map(|&x| f64::from(x)).sum::<f64>() / values.len().max(1) as f64
}

/// Beat period in envelope frames.
fn lag(bpm: f32) -> f32 {
    60.0 * ENV_RATE / bpm
}

/// The envelope minus its centred one-second mean, smoothed with a three-tap kernel.
fn high_pass(raw: &[f32]) -> Vec<f32> {
    let n = raw.len();
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0f64);
    for &x in raw {
        prefix.push(prefix[prefix.len() - 1] + f64::from(x));
    }
    let flat: Vec<f32> = (0..n)
        .map(|i| {
            let lo = i.saturating_sub(HIGH_PASS_HALF);
            let hi = (i + HIGH_PASS_HALF + 1).min(n);
            raw[i] - ((prefix[hi] - prefix[lo]) / (hi - lo) as f64) as f32
        })
        .collect();
    (0..n)
        .map(|i| {
            let left = flat[i.saturating_sub(1)];
            let right = flat[(i + 1).min(n - 1)];
            0.25 * left + 0.5 * flat[i] + 0.25 * right
        })
        .collect()
}

fn estimate(raw: &[f32]) -> Option<Estimate> {
    if raw.len() <= MAX_LAG * 2 || mean(raw) < MIN_FLUX {
        return None;
    }
    let mut env = high_pass(raw);
    let centre = mean(&env) as f32;
    for x in &mut env {
        *x -= centre;
    }
    let acf = autocorrelation(&env);
    if acf[0] <= 0.0 {
        return None;
    }
    let comb = |bpm: f32| -> f64 {
        (1..=COMB_BEATS)
            .map(|k| interpolate(&acf, k as f32 * lag(bpm)))
            .sum()
    };
    let steps = ((MAX_BPM - MIN_BPM) / GRID_STEP).round() as usize;
    let pulse = (0..=steps)
        .map(|i| MIN_BPM + i as f32 * GRID_STEP)
        .max_by(|&a, &b| comb(a).total_cmp(&comb(b)))?;

    let mut levels: Vec<(f32, f64)> = [pulse * 2.0, pulse, pulse / 2.0]
        .into_iter()
        .filter(|bpm| (MIN_BPM..=MAX_BPM).contains(bpm))
        .map(|bpm| {
            let bpm = refine(&env, bpm);
            (bpm, interpolate(&acf, lag(bpm)))
        })
        .collect();
    levels.sort_by(|a, b| b.0.total_cmp(&a.0));
    let strongest = levels.iter().map(|l| l.1).fold(f64::MIN, f64::max);
    let mut bpm = levels.iter().find(|l| l.1 >= LEVEL_RATIO * strongest)?.0;

    if !(PREFER_LOW..=PREFER_HIGH).contains(&bpm) {
        let octave = if bpm < PREFER_LOW {
            bpm * 2.0
        } else {
            bpm / 2.0
        };
        if (PREFER_LOW..=PREFER_HIGH).contains(&octave) {
            let octave = refine(&env, octave);
            if fundamental(&env, octave) >= PREFER_RATIO * fundamental(&env, bpm) {
                bpm = octave;
            }
        }
    }

    let periodicity: f64 = (1..=COMB_BEATS)
        .map(|k| interpolate(&acf, k as f32 * lag(bpm)).max(0.0))
        .sum::<f64>()
        / (COMB_BEATS as f64 * acf[0]);
    let confidence = periodicity.clamp(0.0, 1.0) as f32;
    if confidence < MIN_CONFIDENCE {
        return None;
    }
    Some(Estimate {
        bpm: (bpm * 10.0).round() / 10.0,
        confidence,
    })
}

/// Autocorrelation for lags `0..=MAX_LAG`, each normalized by its overlap length.
fn autocorrelation(env: &[f32]) -> Vec<f64> {
    let n = env.len();
    (0..=MAX_LAG)
        .map(|l| {
            let sum: f64 = env[..n - l]
                .iter()
                .zip(&env[l..])
                .map(|(&a, &b)| f64::from(a * b))
                .sum();
            sum / (n - l) as f64
        })
        .collect()
}

/// `values` at a fractional index by linear interpolation; 0 past the end.
fn interpolate(values: &[f64], at: f32) -> f64 {
    let i = at.floor() as usize;
    if i + 1 >= values.len() {
        return 0.0;
    }
    let frac = f64::from(at - i as f32);
    values[i] * (1.0 - frac) + values[i + 1] * frac
}

/// Complex Fourier sums of `env` at the first [`HARMONICS`] multiples of `cycles` per frame.
fn harmonics(env: &[f32], cycles: f64) -> [(f64, f64); HARMONICS] {
    let mut steps = [(0.0, 0.0); HARMONICS];
    for (h, step) in steps.iter_mut().enumerate() {
        let angle = TAU * cycles * (h + 1) as f64;
        *step = (angle.cos(), -angle.sin());
    }
    let mut rot = [(1.0f64, 0.0f64); HARMONICS];
    let mut sums = [(0.0f64, 0.0f64); HARMONICS];
    for &x in env {
        let x = f64::from(x);
        for h in 0..HARMONICS {
            let (r, i) = rot[h];
            sums[h].0 += x * r;
            sums[h].1 += x * i;
            let (sr, si) = steps[h];
            rot[h] = (r * sr - i * si, r * si + i * sr);
        }
    }
    sums
}

fn magnitude((re, im): (f64, f64)) -> f64 {
    re.hypot(im)
}

/// Fourier magnitude of the envelope at the tempo's fundamental.
fn fundamental(env: &[f32], bpm: f32) -> f64 {
    magnitude(harmonics(env, 1.0 / f64::from(lag(bpm)))[0])
}

/// Summed Fourier magnitudes at the tempo's first [`HARMONICS`] harmonics.
fn harmonic_strength(env: &[f32], bpm: f32) -> f64 {
    harmonics(env, 1.0 / f64::from(lag(bpm)))
        .into_iter()
        .map(magnitude)
        .sum()
}

/// The tempo near `bpm` with the strongest harmonics.
fn refine(env: &[f32], bpm: f32) -> f32 {
    let half = (bpm * 0.015).max(1.0);
    let steps = (2.0 * half / REFINE_STEP).round() as usize;
    let start = bpm - half;
    let scores: Vec<f64> = (0..=steps)
        .map(|i| harmonic_strength(env, start + i as f32 * REFINE_STEP))
        .collect();
    let Some((best, _)) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else {
        return bpm;
    };
    let offset = if best > 0 && best < steps {
        parabolic(scores[best - 1], scores[best], scores[best + 1]) as f32
    } else {
        0.0
    };
    start + (best as f32 + offset) * REFINE_STEP
}

/// Vertex offset in `-0.5..=0.5` of the parabola through three equally spaced points.
fn parabolic(left: f64, centre: f64, right: f64) -> f64 {
    let curve = left - 2.0 * centre + right;
    if curve >= 0.0 {
        return 0.0;
    }
    (0.5 * (left - right) / curve).clamp(-0.5, 0.5)
}

/// `bpm` corrected by the drift, across blocks of the envelope, of the Fourier phase at the
/// tempo's strongest harmonic.
fn correct_tempo(env: &[f32], bpm: f32) -> f32 {
    let blocks = env.len() / GRID_BLOCK;
    if blocks < 2 {
        return bpm;
    }
    let cycles = 1.0 / f64::from(lag(bpm));
    let harmonic = harmonics(env, cycles)
        .into_iter()
        .map(magnitude)
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(1, |(h, _)| h + 1) as f64;
    let angle = TAU * cycles * harmonic;
    let step = (angle.cos(), -angle.sin());
    let mut rot = (1.0f64, 0.0f64);
    let mut phases = Vec::with_capacity(blocks);
    for block in env.as_chunks::<GRID_BLOCK>().0 {
        let mut sum = (0.0, 0.0);
        for &x in block {
            let x = f64::from(x);
            sum.0 += x * rot.0;
            sum.1 += x * rot.1;
            rot = (
                rot.0 * step.0 - rot.1 * step.1,
                rot.0 * step.1 + rot.1 * step.0,
            );
        }
        phases.push((sum.1.atan2(sum.0), magnitude(sum)));
    }
    let mut unwrapped = Vec::with_capacity(blocks);
    let mut last = phases[0].0;
    let mut acc = last;
    for &(phase, weight) in &phases {
        let delta = (phase - last + TAU / 2.0).rem_euclid(TAU) - TAU / 2.0;
        acc += delta;
        last = phase;
        unwrapped.push((acc, weight));
    }
    let weight: f64 = unwrapped.iter().map(|u| u.1).sum();
    if weight <= 0.0 {
        return bpm;
    }
    let centre = |j: usize| (j as f64 + 0.5) * GRID_BLOCK as f64;
    let mean_x = unwrapped
        .iter()
        .enumerate()
        .map(|(j, u)| centre(j) * u.1)
        .sum::<f64>()
        / weight;
    let mean_y = unwrapped.iter().map(|u| u.0 * u.1).sum::<f64>() / weight;
    let (mut cov, mut var) = (0.0, 0.0);
    for (j, &(y, w)) in unwrapped.iter().enumerate() {
        let dx = centre(j) - mean_x;
        cov += w * dx * (y - mean_y);
        var += w * dx * dx;
    }
    if var <= 0.0 {
        return bpm;
    }
    let drift = cov / var / TAU / harmonic;
    ((cycles + drift) * 60.0 * f64::from(ENV_RATE)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU as TAU32;

    /// Deterministic xorshift64 noise in `[-1, 1)`.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        }
    }

    #[derive(Clone, Copy)]
    enum Voice {
        Click,
        SoftClick,
        Kick,
        Snare,
        Hat,
    }

    use Voice::{Click, Hat, Kick, Snare, SoftClick};

    const CLICKS: &[(f32, Voice)] = &[(0.0, Click), (1.0, Click), (2.0, Click), (3.0, Click)];
    const CLICK_EIGHTHS: &[(f32, Voice)] = &[
        (0.0, Click),
        (0.5, SoftClick),
        (1.0, Click),
        (1.5, SoftClick),
        (2.0, Click),
        (2.5, SoftClick),
        (3.0, Click),
        (3.5, SoftClick),
    ];
    /// Kick on 1 and 3, snare on 2 and 4, hats on eighths.
    const DRUMS: &[(f32, Voice)] = &[
        (0.0, Kick),
        (1.0, Snare),
        (2.0, Kick),
        (3.0, Snare),
        (0.0, Hat),
        (0.5, Hat),
        (1.0, Hat),
        (1.5, Hat),
        (2.0, Hat),
        (2.5, Hat),
        (3.0, Hat),
        (3.5, Hat),
    ];
    /// Half-time feel: kick on 1, snare on 3, hats on eighths.
    const HALF_TIME: &[(f32, Voice)] = &[
        (0.0, Kick),
        (2.0, Snare),
        (0.0, Hat),
        (0.5, Hat),
        (1.0, Hat),
        (1.5, Hat),
        (2.0, Hat),
        (2.5, Hat),
        (3.0, Hat),
        (3.5, Hat),
    ];

    fn add_voice(buf: &mut [f32], start: usize, voice: Voice, rng: &mut Rng) {
        let sr = RATE as f32;
        let seconds = match voice {
            Click | SoftClick => 0.03,
            Kick => 0.35,
            Snare => 0.25,
            Hat => 0.06,
        };
        let mut phase = 0.0f32;
        let mut prev = 0.0f32;
        for i in 0..(seconds * sr) as usize {
            let Some(slot) = buf.get_mut(start + i) else {
                break;
            };
            let t = i as f32 / sr;
            let value = match voice {
                Click | SoftClick => {
                    let gain = if matches!(voice, Click) { 1.0 } else { 0.4 };
                    gain * ((TAU32 * 1000.0 * t).sin() * (-t / 0.004).exp() * 0.6
                        + rng.next() * (-t / 0.001).exp() * 0.3)
                }
                Kick => {
                    let freq = 50.0 + 110.0 * (-t / 0.03).exp();
                    phase = (phase + TAU32 * freq / sr) % TAU32;
                    phase.sin() * (-t / 0.12).exp() * 0.9 + rng.next() * (-t / 0.002).exp() * 0.4
                }
                Snare => {
                    rng.next() * (-t / 0.06).exp() * 0.45
                        + (TAU32 * 185.0 * t).sin() * (-t / 0.04).exp() * 0.35
                }
                Hat => {
                    let white = rng.next();
                    let high = white - prev;
                    prev = white;
                    high * (-t / 0.015).exp() * 0.12
                }
            };
            *slot += value;
        }
    }

    struct Track {
        bpm: f32,
        seconds: f32,
        /// Silence before the first beat in seconds.
        intro: f32,
        pattern: &'static [(f32, Voice)],
        /// Delays off-beat eighths by a third of an eighth.
        swing: bool,
        /// White noise relative to the signal's RMS.
        noise_db: Option<f32>,
        seed: u64,
    }

    impl Track {
        fn new(bpm: f32, seconds: f32, pattern: &'static [(f32, Voice)]) -> Self {
            Self {
                bpm,
                seconds,
                intro: 0.0,
                pattern,
                swing: false,
                noise_db: None,
                seed: 0x9e37_79b9_7f4a_7c15 ^ bpm.to_bits() as u64,
            }
        }

        fn render(&self) -> Vec<f32> {
            let rate = f64::from(RATE);
            let len = (f64::from(self.seconds) * rate) as usize;
            let mut buf = vec![0.0; len];
            let mut rng = Rng(self.seed | 1);
            let beat = 60.0 / f64::from(self.bpm);
            for bar in 0.. {
                let bar_start = f64::from(self.intro) + f64::from(bar) * 4.0 * beat;
                if bar_start >= f64::from(self.seconds) {
                    break;
                }
                for &(pos, voice) in self.pattern {
                    let mut pos = f64::from(pos);
                    if self.swing && (pos.fract() - 0.5).abs() < 1e-6 {
                        pos += 1.0 / 6.0;
                    }
                    let start = ((bar_start + pos * beat) * rate).round() as usize;
                    if start < len {
                        add_voice(&mut buf, start, voice, &mut rng);
                    }
                }
            }
            if let Some(db) = self.noise_db {
                let rms = (buf.iter().map(|&x| f64::from(x * x)).sum::<f64>() / len as f64).sqrt();
                let scale = (rms * 10f64.powf(f64::from(db) / 20.0) * 3f64.sqrt()) as f32;
                for x in &mut buf {
                    *x += rng.next() * scale;
                }
            }
            buf
        }
    }

    /// Feeds mono audio as stereo frames of 960 samples per channel.
    fn feed(mono: &[f32], mut push: impl FnMut(&[f32])) {
        let mut chunk = Vec::with_capacity(1920);
        for block in mono.chunks(960) {
            chunk.clear();
            for &s in block {
                chunk.push(s);
                chunk.push(s);
            }
            push(&chunk);
        }
    }

    fn analyze(mono: &[f32]) -> Option<Estimate> {
        let mut analyzer = Analyzer::new();
        feed(mono, |chunk| analyzer.push(chunk));
        analyzer.finish()
    }

    fn onsets(mono: &[f32]) -> Onsets {
        let mut onsets = Onsets::new();
        feed(mono, |chunk| onsets.push(chunk));
        onsets
    }

    const TEMPOS: [f32; 6] = [85.0, 100.0, 124.0, 128.0, 140.0, 174.0];
    const DURATIONS: [f32; 3] = [30.0, 45.0, 60.0];

    /// Runs every tempo with the given patterns and variants and checks ±1 BPM.
    fn check_tempos(name: &str, straight: &'static [(f32, Voice)], swung: &'static [(f32, Voice)]) {
        let mut failures = Vec::new();
        for (i, &bpm) in TEMPOS.iter().enumerate() {
            for (variant, swing, noise) in [
                ("straight", false, false),
                ("swing", true, false),
                ("noise", false, true),
                ("swing+noise", true, true),
            ] {
                let seconds = DURATIONS[(i + usize::from(swing) + usize::from(noise)) % 3];
                let mut track = Track::new(bpm, seconds, if swing { swung } else { straight });
                track.swing = swing;
                track.noise_db = noise.then_some(-20.0);
                let result = analyze(&track.render());
                println!("{name} {bpm} {variant} {seconds}s: {result:?}");
                if !result.is_some_and(|e| (e.bpm - bpm).abs() <= 1.0) {
                    failures.push(format!("{name} {bpm} {variant}: {result:?}"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn click_tracks_within_one_bpm() {
        check_tempos("clicks", CLICKS, CLICK_EIGHTHS);
    }

    #[test]
    fn drum_patterns_within_one_bpm() {
        check_tempos("drums", DRUMS, DRUMS);
    }

    #[test]
    fn drum_and_bass_with_half_time_kicks_stays_174() {
        // Kicks and snares alternate on every 174 BPM beat, so the kick pulse runs at 87 BPM.
        // The 87 BPM octave has a weak fundamental and 174 stays.
        let result = analyze(&Track::new(174.0, 45.0, DRUMS).render()).unwrap();
        assert!((result.bpm - 174.0).abs() <= 1.0, "{result:?}");
    }

    #[test]
    fn half_time_pattern_reports_the_in_range_half() {
        // Kick on 1 and snare on 3 at 174 BPM: the accents form an 87 BPM pulse at least
        // as strong as the hats' 174 BPM pulse, so the result moves into 80-170.
        let result = analyze(&Track::new(174.0, 45.0, HALF_TIME).render()).unwrap();
        assert!((result.bpm - 87.0).abs() <= 1.0, "{result:?}");
    }

    #[test]
    fn slow_patterns_report_the_in_range_double() {
        // 70 BPM with eighth hats and a 65 BPM click track report 140 and 130.
        let drums = analyze(&Track::new(70.0, 45.0, DRUMS).render()).unwrap();
        assert!((drums.bpm - 140.0).abs() <= 1.0, "{drums:?}");
        let clicks = analyze(&Track::new(65.0, 45.0, CLICKS).render()).unwrap();
        assert!((clicks.bpm - 130.0).abs() <= 1.0, "{clicks:?}");
    }

    #[test]
    fn unpulsed_audio_gives_none() {
        let len = 30 * RATE as usize;
        assert_eq!(analyze(&vec![0.0; len]), None);
        let mut rng = Rng(12345);
        let noise: Vec<f32> = (0..len).map(|_| rng.next() * 0.3).collect();
        assert_eq!(analyze(&noise), None);
        for freq in [440.0, 100.0] {
            let sine: Vec<f32> = (0..len)
                .map(|i| (TAU32 * freq * i as f32 / RATE as f32).sin() * 0.5)
                .collect();
            assert_eq!(analyze(&sine), None, "{freq} Hz");
        }
    }

    #[test]
    fn short_audio_gives_none() {
        let audio = Track::new(128.0, MIN_SECONDS - 0.1, DRUMS).render();
        assert_eq!(analyze(&audio), None);
    }

    #[test]
    fn non_finite_samples_count_as_zero() {
        let mut audio = Track::new(128.0, 30.0, DRUMS).render();
        for (i, x) in audio.iter_mut().enumerate() {
            if i % 997 == 0 {
                *x = if i % 2 == 0 { f32::NAN } else { f32::INFINITY };
            }
        }
        let result = analyze(&audio).unwrap();
        assert!((result.bpm - 128.0).abs() <= 1.0, "{result:?}");
        assert_eq!(analyze(&vec![f32::NAN; 30 * RATE as usize]), None);
    }

    #[test]
    fn input_past_max_seconds_is_ignored() {
        let first = Track::new(128.0, MAX_SECONDS, DRUMS).render();
        let mut longer = first.clone();
        longer.extend(Track::new(97.0, 15.0, CLICKS).render());
        let mut analyzer = Analyzer::new();
        let capacity = analyzer.onsets.env.capacity();
        feed(&longer, |chunk| analyzer.push(chunk));
        assert!(analyzer.full());
        assert_eq!(analyzer.seconds(), MAX_SECONDS);
        assert_eq!(analyzer.onsets.env.len(), envelope_len(MAX_FRAMES));
        assert_eq!(analyzer.onsets.env.capacity(), capacity);
        assert_eq!(analyzer.finish(), analyze(&first));
    }

    #[test]
    fn onsets_stop_at_their_limit() {
        let frames = 20 * RATE as usize;
        let mut onsets = Onsets::with_limit(frames, 0);
        feed(&Track::new(128.0, 25.0, DRUMS).render(), |chunk| {
            onsets.push(chunk)
        });
        assert_eq!(onsets.seconds(), 20.0);
        assert_eq!(onsets.env.len(), envelope_len(frames));
        assert_eq!(Onsets::new().limit, MAX_GRID_FRAMES);
    }

    /// Distance in ms between two times on a circle of `period` ms.
    fn circular_ms(a: f64, b: f64, period: f64) -> f64 {
        let d = (a - b).rem_euclid(period);
        d.min(period - d)
    }

    fn check_grid(track: &Track, given_bpm: f32) -> Grid {
        let grid = beat_grid(&onsets(&track.render()), given_bpm).unwrap();
        let period = 60_000.0 / f64::from(track.bpm);
        let expected = (f64::from(track.intro) * 1000.0).rem_euclid(period);
        let error = circular_ms(grid.first_beat_ms, expected, period);
        println!(
            "grid {} given {given_bpm} swing {} noise {:?}: {grid:?} expected {expected:.1} error {error:.2} ms",
            track.bpm, track.swing, track.noise_db
        );
        assert!(error <= 10.0, "{grid:?} expected {expected}");
        assert!((grid.bpm - track.bpm).abs() <= 0.05, "{grid:?}");
        assert!((0.0..period).contains(&grid.first_beat_ms));
        grid
    }

    #[test]
    fn grid_finds_the_first_beat_after_an_intro() {
        for (bpm, intro, seconds) in [
            (124.0, 3.7, 40.0),
            (100.0, 1.234, 45.0),
            (128.0, 0.5, 30.0),
            (174.0, 2.1, 40.0),
        ] {
            for (swing, noise, pattern) in [
                (false, false, DRUMS),
                (true, false, DRUMS),
                (false, true, DRUMS),
                (false, false, CLICKS),
            ] {
                let mut track = Track::new(bpm, seconds, pattern);
                track.intro = intro;
                track.swing = swing;
                track.noise_db = noise.then_some(-20.0);
                check_grid(&track, bpm);
            }
        }
    }

    #[test]
    fn grid_corrects_a_rounded_tempo() {
        let mut track = Track::new(127.73, 120.0, DRUMS);
        track.intro = 0.81;
        check_grid(&track, 128.0);
        check_grid(&track, 127.7);
    }

    #[test]
    fn grid_rejects_unrelated_audio() {
        let len = 30 * RATE as usize;
        assert_eq!(beat_grid(&onsets(&vec![0.0; len]), 120.0), None);
        let mut rng = Rng(777);
        let noise: Vec<f32> = (0..len).map(|_| rng.next() * 0.3).collect();
        assert_eq!(beat_grid(&onsets(&noise), 120.0), None);
        let drums = onsets(&Track::new(128.0, 40.0, DRUMS).render());
        let wrong = beat_grid(&drums, 97.0);
        println!("128 BPM drums given 97: {wrong:?}");
        assert_eq!(wrong, None);
    }

    #[test]
    fn timing_of_sixty_seconds() {
        let audio = Track::new(128.0, 60.0, DRUMS).render();
        let start = std::time::Instant::now();
        let result = analyze(&audio);
        let analysis = start.elapsed();
        let start = std::time::Instant::now();
        let grid = beat_grid(&onsets(&audio), 128.0);
        println!(
            "60 s analysis {analysis:?} {result:?}; grid {:?} {grid:?}",
            start.elapsed()
        );
    }
}
