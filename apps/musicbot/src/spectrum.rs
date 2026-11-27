//! Band energies of a song for a deck's scrolling waveform and spectrogram.
//!
//! [`Spectrum`] downmixes interleaved stereo at 48 kHz to mono, low-passes it with an
//! eighth-order Butterworth filter at 5.6 kHz (four RBJ biquads) and keeps every fourth
//! sample, giving 12 kHz. A bank of RBJ biquads (Direct Form I, f64) splits that stream into
//! bands; the mean square of each band over each column of 10 ms (120 samples at 12 kHz)
//! becomes a level in dB and then a byte through [`level_byte`].
//!
//! Coarse bands, each a cascade of Butterworth sections:
//! - low: 20-250 Hz (second-order high-pass at 20 Hz, fourth-order low-pass at 250 Hz),
//! - mid: 250-2500 Hz (fourth-order high-pass at 250 Hz, fourth-order low-pass at 2.5 kHz),
//! - high: 2.5-5.6 kHz (fourth-order high-pass at 2.5 kHz, bounded by the decimation filter).
//!
//! Fine bands: 24 constant-Q band-pass biquads with edges `40 Hz * (5500 / 40)^(k / 24)`,
//! each 0.296 octaves wide (Q 4.87). Their energies are scaled by the top band's bandwidth
//! over their own, so white noise reads flat and a sine reads 0 dB in the top band and up to
//! 21 dB more in the lowest band.
//!
//! Levels are dB relative to a full-scale sine: `10 * log10(2 * mean_square)`.

use biquad::{Biquad as _, Coefficients, DirectForm1, Hertz, Type};
use std::f64::consts::{LN_2, PI};

/// Columns per second of song time; column n covers [n/100 s, (n+1)/100 s).
pub const COLUMNS_PER_SECOND: usize = 100;
/// Low/mid/high for the coloured waveform, and log-spaced bands for the spectrogram.
pub const COARSE_BANDS: usize = 3;
pub const FINE_BANDS: usize = 24;
/// Level of byte 0. Byte `b` stands for `FLOOR_DB * (1 - b / 255)` dB: 0 dB is 255, each
/// byte step is 72/255 = 0.282 dB.
pub const FLOOR_DB: f32 = -72.0;

/// Input rate in Hz.
const RATE: usize = 48_000;
const DECIMATION: usize = 4;
const LOW_RATE: f64 = (RATE / DECIMATION) as f64;
/// Analysis samples per column.
const COLUMN_SAMPLES: usize = RATE / DECIMATION / COLUMNS_PER_SECOND;
/// Columns kept at most: 30 minutes. Later input is ignored.
const MAX_COLUMNS: usize = 30 * 60 * COLUMNS_PER_SECOND;
const ANTI_ALIAS_HZ: f64 = 5600.0;
const LOW_CUT_HZ: f64 = 20.0;
const LOW_MID_HZ: f64 = 250.0;
const MID_HIGH_HZ: f64 = 2500.0;
const FINE_LOW_HZ: f64 = 40.0;
const FINE_HIGH_HZ: f64 = 5500.0;
/// Filter outputs below this magnitude become 0, which keeps decaying filters out of
/// subnormal numbers.
const FLUSH: f64 = 1e-25;

/// Band energy in dB as a byte: 0 at [`FLOOR_DB`] (-72 dBFS) or below and for NaN, 255 at
/// 0 dBFS or above, `round((db + 72) * 255 / 72)` between.
pub fn level_byte(db: f32) -> u8 {
    if db.is_nan() || db <= FLOOR_DB {
        return 0;
    }
    ((db - FLOOR_DB) * 255.0 / -FLOOR_DB).round().min(255.0) as u8
}

/// A cookbook biquad (the `biquad` crate's coefficients) in direct form I,
/// with a gain after it.
#[derive(Clone, Copy)]
struct Biquad {
    filter: DirectForm1<f64>,
    gain: f64,
}

enum Kind {
    /// Low-pass.
    Low,
    /// High-pass.
    High,
    /// Band-pass with 0 dB peak gain and the bandwidth in octaves.
    Band(f64),
}

impl Biquad {
    fn new(kind: Kind, hz: f64, q: f64, rate: f64) -> Self {
        let (kind, q, gain) = match kind {
            Kind::Low => (Type::LowPass, q, 1.0),
            Kind::High => (Type::HighPass, q, 1.0),
            Kind::Band(octaves) => {
                // The crate's band-pass peaks at Q; dividing by Q gives 0 dB.
                let w0 = 2.0 * PI * hz / rate;
                let q = 1.0 / (2.0 * (LN_2 / 2.0 * octaves * w0 / w0.sin()).sinh());
                (Type::BandPass, q, 1.0 / q)
            }
        };
        let coefficients = match (Hertz::from_hz(rate), Hertz::from_hz(hz)) {
            (Ok(rate), Ok(hz)) => Coefficients::from_params(kind, rate, hz, q).ok(),
            _ => None,
        }
        .unwrap_or(Coefficients {
            a1: 0.0,
            a2: 0.0,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
        });
        Self {
            filter: DirectForm1::<f64>::new(coefficients),
            gain,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.filter.run(x) * self.gain;
        // Silence rests at exact zero, so decaying filters stay fast.
        if x == 0.0 && y.abs() < FLUSH {
            self.filter.reset_state();
            return 0.0;
        }
        y
    }
}

/// The Q of each biquad section of an even-order Butterworth filter.
fn butterworth(order: usize) -> impl Iterator<Item = f64> {
    (0..order / 2).map(move |k| 1.0 / (2.0 * (PI * (2 * k + 1) as f64 / (2 * order) as f64).cos()))
}

fn sections(kind: fn() -> Kind, hz: f64, order: usize, rate: f64) -> Vec<Biquad> {
    butterworth(order)
        .map(|q| Biquad::new(kind(), hz, q, rate))
        .collect()
}

/// Lower edge of fine band `k`; `fine_edge(FINE_BANDS)` is the top edge.
fn fine_edge(k: usize) -> f64 {
    FINE_LOW_HZ * (FINE_HIGH_HZ / FINE_LOW_HZ).powf(k as f64 / FINE_BANDS as f64)
}

/// Band energies of one song, one column per 10 ms of song time.
pub struct Spectrum {
    /// Columns kept at most.
    limit: usize,
    /// Left sample of a stereo pair split across pushes.
    pending: Option<f32>,
    /// Input frames since the last analysis sample.
    phase: usize,
    anti_alias: Vec<Biquad>,
    coarse_filters: [Vec<Biquad>; COARSE_BANDS],
    fine_filters: [Biquad; FINE_BANDS],
    /// Power factor per fine band: the top band's bandwidth over the band's.
    fine_gain: [f64; FINE_BANDS],
    coarse_energy: [f64; COARSE_BANDS],
    fine_energy: [f64; FINE_BANDS],
    samples: usize,
    coarse: Vec<u8>,
    fine: Vec<u8>,
}

impl Default for Spectrum {
    fn default() -> Self {
        Self::new()
    }
}

impl Spectrum {
    pub fn new() -> Self {
        Self::with_limit(MAX_COLUMNS)
    }

    fn with_limit(limit: usize) -> Self {
        let rate = LOW_RATE;
        let mut low = vec![Biquad::new(
            Kind::High,
            LOW_CUT_HZ,
            std::f64::consts::FRAC_1_SQRT_2,
            rate,
        )];
        low.extend(sections(|| Kind::Low, LOW_MID_HZ, 4, rate));
        let mut mid = sections(|| Kind::High, LOW_MID_HZ, 4, rate);
        mid.extend(sections(|| Kind::Low, MID_HIGH_HZ, 4, rate));
        let high = sections(|| Kind::High, MID_HIGH_HZ, 4, rate);
        let octaves = (FINE_HIGH_HZ / FINE_LOW_HZ).log2() / FINE_BANDS as f64;
        let centre = |k: usize| (fine_edge(k) * fine_edge(k + 1)).sqrt();
        Self {
            limit,
            pending: None,
            phase: 0,
            anti_alias: sections(|| Kind::Low, ANTI_ALIAS_HZ, 8, RATE as f64),
            coarse_filters: [low, mid, high],
            fine_filters: std::array::from_fn(|k| {
                Biquad::new(Kind::Band(octaves), centre(k), 0.0, rate)
            }),
            fine_gain: std::array::from_fn(|k| {
                (fine_edge(FINE_BANDS) - fine_edge(FINE_BANDS - 1))
                    / (fine_edge(k + 1) - fine_edge(k))
            }),
            coarse_energy: [0.0; COARSE_BANDS],
            fine_energy: [0.0; FINE_BANDS],
            samples: 0,
            coarse: Vec::new(),
            fine: Vec::new(),
        }
    }

    /// Interleaved stereo f32 at 48 kHz, consecutive from the song's start, any length per
    /// call. Non-finite samples count as 0.
    pub fn push(&mut self, stereo: &[f32]) {
        for &sample in stereo {
            match self.pending.take() {
                None => self.pending = Some(sample),
                Some(left) => self.frame(left, sample),
            }
        }
    }

    /// Complete columns so far.
    pub fn columns(&self) -> usize {
        self.coarse.len() / COARSE_BANDS
    }

    /// Bytes of columns [from, to) clamped to what exists: COARSE_BANDS bytes per column
    /// (low, mid, high).
    pub fn coarse(&self, from: usize, to: usize) -> &[u8] {
        let (from, to) = self.clamp(from, to);
        &self.coarse[from * COARSE_BANDS..to * COARSE_BANDS]
    }

    /// Bytes of columns [from, to) clamped to what exists: FINE_BANDS bytes per column
    /// (lowest band first).
    pub fn fine(&self, from: usize, to: usize) -> &[u8] {
        let (from, to) = self.clamp(from, to);
        &self.fine[from * FINE_BANDS..to * FINE_BANDS]
    }

    /// The whole song in `columns` coarse columns (max over each group), for an overview
    /// strip; `total` is the song's expected column count (columns not yet computed read 0).
    /// Group `i` covers song columns `[i * total / columns, (i + 1) * total / columns)`, at
    /// least one column.
    pub fn overview(&self, columns: usize, total: usize) -> Vec<u8> {
        let mut out = vec![0; columns * COARSE_BANDS];
        let have = self.columns();
        for (i, group) in out.as_chunks_mut::<COARSE_BANDS>().0.iter_mut().enumerate() {
            let start = i * total / columns;
            let end = ((i + 1) * total / columns)
                .max(start + 1)
                .min(have)
                .min(total);
            for column in self
                .coarse
                .as_chunks::<COARSE_BANDS>()
                .0
                .iter()
                .take(end)
                .skip(start)
            {
                for (max, &level) in group.iter_mut().zip(column) {
                    *max = (*max).max(level);
                }
            }
        }
        out
    }

    fn clamp(&self, from: usize, to: usize) -> (usize, usize) {
        let to = to.min(self.columns());
        (from.min(to), to)
    }

    fn frame(&mut self, left: f32, right: f32) {
        if self.columns() >= self.limit {
            return;
        }
        let mut x = 0.5 * (f64::from(finite(left)) + f64::from(finite(right)));
        for section in &mut self.anti_alias {
            x = section.process(x);
        }
        self.phase += 1;
        if self.phase < DECIMATION {
            return;
        }
        self.phase = 0;
        for (chain, energy) in self.coarse_filters.iter_mut().zip(&mut self.coarse_energy) {
            let y = chain.iter_mut().fold(x, |y, section| section.process(y));
            *energy += y * y;
        }
        for (filter, energy) in self.fine_filters.iter_mut().zip(&mut self.fine_energy) {
            let y = filter.process(x);
            *energy += y * y;
        }
        self.samples += 1;
        if self.samples == COLUMN_SAMPLES {
            self.samples = 0;
            for energy in &mut self.coarse_energy {
                self.coarse.push(energy_byte(*energy));
                *energy = 0.0;
            }
            for (energy, gain) in self.fine_energy.iter_mut().zip(self.fine_gain) {
                self.fine.push(energy_byte(*energy * gain));
                *energy = 0.0;
            }
        }
    }
}

fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// Byte of a column's summed squares relative to a full-scale sine.
fn energy_byte(sum: f64) -> u8 {
    let mean_square = sum / COLUMN_SAMPLES as f64;
    level_byte((10.0 * (2.0 * mean_square).log10()) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

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

    fn stereo(mono: &[f32]) -> Vec<f32> {
        mono.iter().flat_map(|&x| [x, x]).collect()
    }

    fn sine(hz: f64, amplitude: f64, seconds: f64) -> Vec<f32> {
        let len = (seconds * RATE as f64) as usize;
        (0..len)
            .map(|i| (amplitude * (TAU * hz * i as f64 / RATE as f64).sin()) as f32)
            .collect()
    }

    fn analyze(mono: &[f32]) -> Spectrum {
        let mut spectrum = Spectrum::new();
        spectrum.push(&stereo(mono));
        spectrum
    }

    /// Mean level in dB per band over columns `from..`, averaged as energy.
    fn mean_db(bytes: &[u8], bands: usize, from: usize) -> Vec<f64> {
        let rows: Vec<&[u8]> = bytes.chunks_exact(bands).skip(from).collect();
        (0..bands)
            .map(|b| {
                let energy: f64 = rows
                    .iter()
                    .map(|row| {
                        10f64.powf(f64::from(FLOOR_DB) * (1.0 - f64::from(row[b]) / 255.0) / 10.0)
                    })
                    .sum::<f64>()
                    / rows.len() as f64;
                10.0 * energy.log10()
            })
            .collect()
    }

    fn mean_bytes(bytes: &[u8], bands: usize, from: usize) -> Vec<f64> {
        let rows: Vec<&[u8]> = bytes.chunks_exact(bands).skip(from).collect();
        (0..bands)
            .map(|b| rows.iter().map(|row| f64::from(row[b])).sum::<f64>() / rows.len() as f64)
            .collect()
    }

    fn fine_band_of(hz: f64) -> usize {
        (0..FINE_BANDS)
            .find(|&k| fine_edge(k) <= hz && hz < fine_edge(k + 1))
            .unwrap()
    }

    fn strongest(levels: &[f64]) -> usize {
        levels
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0
    }

    #[test]
    fn level_byte_maps_dbfs_linearly() {
        assert_eq!(level_byte(0.0), 255);
        assert_eq!(level_byte(6.0), 255);
        assert_eq!(level_byte(-36.0), 128);
        assert_eq!(level_byte(-72.0), 0);
        assert_eq!(level_byte(-90.0), 0);
        assert_eq!(level_byte(f32::NEG_INFINITY), 0);
        assert_eq!(level_byte(f32::NAN), 0);
        assert_eq!(level_byte(-72.0 + 72.0 / 255.0), 1);
    }

    #[test]
    fn low_tone_lights_the_low_band() {
        // -34 dBFS keeps the bandwidth-scaled low fine bands below 0 dB.
        let spectrum = analyze(&sine(60.0, 0.02, 2.0));
        let coarse = mean_bytes(spectrum.coarse(0, usize::MAX), COARSE_BANDS, 20);
        println!("60 Hz coarse {coarse:.1?}");
        assert!(coarse[0] - coarse[1] >= 60.0 && coarse[0] - coarse[2] >= 60.0);
        let fine = mean_db(spectrum.fine(0, usize::MAX), FINE_BANDS, 20);
        println!("60 Hz fine {fine:.1?}");
        assert_eq!(strongest(&fine), fine_band_of(60.0));
    }

    #[test]
    fn high_tone_lights_the_high_band() {
        let spectrum = analyze(&sine(5000.0, 0.25, 2.0));
        let coarse = mean_bytes(spectrum.coarse(0, usize::MAX), COARSE_BANDS, 20);
        println!("5 kHz coarse {coarse:.1?}");
        assert!(coarse[2] - coarse[0] >= 60.0 && coarse[2] - coarse[1] >= 60.0);
        let fine = mean_db(spectrum.fine(0, usize::MAX), FINE_BANDS, 20);
        println!("5 kHz fine {fine:.1?}");
        assert_eq!(strongest(&fine), fine_band_of(5000.0));
    }

    #[test]
    fn full_scale_sines_read_near_255() {
        for (hz, coarse_band) in [(60.0, 0), (1000.0, 1), (4000.0, 2), (5000.0, 2)] {
            let spectrum = analyze(&sine(hz, 1.0, 1.0));
            let coarse = mean_bytes(spectrum.coarse(0, usize::MAX), COARSE_BANDS, 20);
            let fine = mean_bytes(spectrum.fine(0, usize::MAX), FINE_BANDS, 20);
            println!(
                "full-scale {hz} Hz: coarse {coarse:.1?} fine band {:.1}",
                fine[fine_band_of(hz)]
            );
            assert!(coarse[coarse_band] >= 250.0, "{hz} Hz {coarse:?}");
            assert!(fine[fine_band_of(hz)] >= 250.0, "{hz} Hz {fine:?}");
        }
    }

    #[test]
    fn white_noise_reads_flat_across_fine_bands() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let noise: Vec<f32> = (0..30 * RATE).map(|_| rng.next() * 0.1).collect();
        let fine = mean_db(analyze(&noise).fine(0, usize::MAX), FINE_BANDS, 50);
        let average = fine.iter().sum::<f64>() / FINE_BANDS as f64;
        let spread: Vec<f64> = fine.iter().map(|db| db - average).collect();
        println!("noise fine dB around {average:.1}: {spread:.2?}");
        assert!(spread.iter().all(|d| d.abs() <= 3.0), "{spread:?}");
    }

    #[test]
    fn click_lands_in_its_column() {
        let mut mono = vec![0.0f32; 2 * RATE];
        mono[(1.234 * RATE as f64).round() as usize] = 1.0;
        let spectrum = analyze(&mono);
        assert_eq!(spectrum.columns(), 200);
        assert!(spectrum.coarse(0, 123).iter().all(|&b| b == 0));
        assert!(spectrum.fine(0, 123).iter().all(|&b| b == 0));
        let coarse = spectrum.coarse(0, usize::MAX);
        for band in 0..COARSE_BANDS {
            let loudest = (0..200)
                .max_by_key(|&c| coarse[c * COARSE_BANDS + band])
                .unwrap();
            assert_eq!(loudest, 123, "band {band}");
        }
    }

    #[test]
    fn odd_pushes_match_one_push() {
        let mut rng = Rng(99);
        let mut mono: Vec<f32> = (0..3 * RATE).map(|_| rng.next() * 0.2).collect();
        for i in (0..mono.len()).step_by(7001) {
            mono[i] = 1.0;
        }
        let audio = stereo(&mono);
        let whole = analyze(&mono);
        let mut parts = Spectrum::new();
        let mut rest = &audio[..];
        for size in [1, 7, 960, 3, 1921, 2, 5].into_iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (head, tail) = rest.split_at(size.min(rest.len()));
            parts.push(head);
            rest = tail;
        }
        assert_eq!(parts.columns(), 300);
        assert_eq!(parts.coarse, whole.coarse);
        assert_eq!(parts.fine, whole.fine);
    }

    #[test]
    fn silence_and_non_finite_samples_give_zero() {
        let spectrum = analyze(&vec![0.0; 5 * RATE]);
        assert_eq!(spectrum.columns(), 500);
        assert!(spectrum.coarse(0, 500).iter().all(|&b| b == 0));
        assert!(spectrum.fine(0, 500).iter().all(|&b| b == 0));
        let spectrum = analyze(&vec![f32::NAN; RATE]);
        assert!(
            spectrum
                .coarse(0, 100)
                .iter()
                .chain(spectrum.fine(0, 100))
                .all(|&b| b == 0)
        );
    }

    #[test]
    fn ranges_clamp_to_existing_columns() {
        let spectrum = analyze(&vec![0.0; (1.234 * RATE as f64) as usize]);
        assert_eq!(spectrum.columns(), 123);
        assert_eq!(spectrum.coarse(100, 1000).len(), 23 * COARSE_BANDS);
        assert_eq!(spectrum.fine(100, 1000).len(), 23 * FINE_BANDS);
        assert!(spectrum.coarse(200, 300).is_empty());
        assert!(spectrum.fine(50, 10).is_empty());
    }

    #[test]
    fn overview_max_pools_and_pads() {
        let mut spectrum = Spectrum::new();
        spectrum.coarse = (0..10u8).flat_map(|c| [c, 20 - c, c % 3]).collect();
        // Groups of four song columns; columns 10 and 11 are missing.
        assert_eq!(spectrum.overview(3, 12), vec![3, 20, 2, 7, 16, 2, 9, 12, 2]);
        // Groups past the computed columns read 0.
        assert_eq!(
            spectrum.overview(4, 20),
            vec![4, 20, 2, 9, 15, 2, 0, 0, 0, 0, 0, 0]
        );
        // More overview columns than song columns repeat single columns.
        assert_eq!(
            spectrum.overview(4, 2),
            vec![0, 20, 0, 0, 20, 0, 1, 19, 1, 1, 19, 1]
        );
        assert!(spectrum.overview(0, 10).is_empty());
        assert_eq!(spectrum.overview(2, 0), vec![0; 6]);
    }

    #[test]
    fn columns_stop_at_the_limit() {
        let mut spectrum = Spectrum::with_limit(150);
        spectrum.push(&stereo(&sine(440.0, 0.5, 2.0)));
        assert_eq!(spectrum.columns(), 150);
        assert_eq!(spectrum.fine.len(), 150 * FINE_BANDS);
        assert_eq!(Spectrum::new().limit, MAX_COLUMNS);
    }

    #[test]
    fn throughput() {
        let mut rng = Rng(5);
        let audio = stereo(&(0..60 * RATE).map(|_| rng.next() * 0.3).collect::<Vec<_>>());
        let mut spectrum = Spectrum::new();
        let start = std::time::Instant::now();
        for chunk in audio.chunks(1920) {
            spectrum.push(chunk);
        }
        let elapsed = start.elapsed();
        println!(
            "60 s of audio in {elapsed:?}: {:.0}x real time, {:.3} % of a core",
            60.0 / elapsed.as_secs_f64(),
            elapsed.as_secs_f64() / 60.0 * 100.0
        );
        assert_eq!(spectrum.columns(), 6000);
    }
}
