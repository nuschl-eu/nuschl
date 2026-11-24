// SPDX-License-Identifier: EUPL-1.2
//! Rate control: a quantizer for each frame from the sizes of earlier frames.
//!
//! A leaky bucket holds the encoder's credit: it fills at the bitrate as time
//! passes, up to [`CAP`] seconds of it, and each frame spends its size. A frame's
//! target is its share of the bitrate since the previous frames, scaled up while
//! the credit sits above [`OPTIMAL`] and down below it; frames are dropped while
//! the credit is spent. The quantizer comes from a model per frame class (the
//! keyframe and each temporal layer): bits = factor × pixels ÷ AC step, with the
//! factor corrected after every frame, as in libvpx's and libaom's real-time
//! controllers. A bitrate change applies to the next frame.

use crate::av1::ac_quantizer;
use std::ops::RangeInclusive;
use std::time::Duration;

/// Credit the bucket aims for, in seconds at the bitrate.
const OPTIMAL: f64 = 0.2;
/// The most credit the bucket holds.
const CAP: f64 = 2.0 * OPTIMAL;
/// A keyframe's target as a share of the largest frame.
const KEY_SHARE: f64 = 0.7;
/// Starting model factors, bits × AC step per pixel: keyframes, then inter
/// frames of each temporal layer.
const FACTORS: [f64; 4] = [150.0, 15.0, 15.0, 15.0];

pub struct Rate {
    bitrate: f64,
    window: f64,
    qindex: RangeInclusive<u8>,
    pixels: f64,
    credit: f64,
    previous: Option<Duration>,
    /// Average seconds between frames.
    interval: f64,
    factors: [f64; 4],
}

/// The frame classes the model keeps apart.
pub fn class(key: bool, temporal: u8) -> usize {
    if key {
        0
    } else {
        1 + usize::from(temporal.min(2))
    }
}

impl Rate {
    pub fn new(
        bitrate: u32,
        fps: u16,
        window: Duration,
        pixels: u32,
        qindex: RangeInclusive<u8>,
    ) -> Self {
        let bitrate = f64::from(bitrate);
        Self {
            bitrate,
            window: window.as_secs_f64(),
            qindex: (*qindex.start()).max(1)..=*qindex.end(),
            pixels: f64::from(pixels),
            credit: OPTIMAL * bitrate,
            previous: None,
            interval: 1.0 / f64::from(fps.max(1)),
            factors: FACTORS,
        }
    }

    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.bitrate = f64::from(bitrate);
        self.credit = self.credit.min(CAP * self.bitrate);
    }

    /// The largest frame, in bits.
    pub fn frame_limit(&self) -> f64 {
        self.bitrate * self.window
    }

    /// Credits the time since the previous frame and returns the target size of
    /// a frame at `now` in bits, or `None` to drop it. Keyframes are never
    /// dropped. `weight` is the frame's share relative to the average frame.
    pub fn target(&mut self, now: Duration, key: bool, weight: f64) -> Option<f64> {
        if let Some(previous) = self.previous {
            let elapsed = now.saturating_sub(previous).as_secs_f64().min(1.0);
            self.credit = (self.credit + self.bitrate * elapsed).min(CAP * self.bitrate);
            self.interval += (elapsed - self.interval) / 8.0;
        }
        self.previous = Some(now);
        if key {
            return Some(KEY_SHARE * self.frame_limit());
        }
        if self.credit < 0.0 {
            return None;
        }
        let optimal = OPTIMAL * self.bitrate;
        let scale = (1.0 + (self.credit - optimal) / (2.0 * optimal)).clamp(0.5, 1.5);
        Some((self.bitrate * self.interval * weight * scale).min(self.frame_limit()))
    }

    /// The quantizer the model expects to code a frame of `class` in `target` bits.
    pub fn qindex(&self, class: usize, target: f64) -> u8 {
        let step = self.factors[class] * self.pixels / target.max(1.0);
        let fine = (0..=255u8)
            .find(|&q| f64::from(ac_quantizer(q)) >= step)
            .unwrap_or(255);
        fine.clamp(*self.qindex.start(), *self.qindex.end())
    }

    pub fn max_qindex(&self) -> u8 {
        *self.qindex.end()
    }

    /// A sent frame: spends its `bits` and moves the model halfway, in the log
    /// domain, toward what the frame measured.
    pub fn sent(&mut self, class: usize, qindex: u8, bits: f64) {
        self.credit -= bits;
        let predicted = self.factors[class] * self.pixels / f64::from(ac_quantizer(qindex));
        let ratio = (bits.max(8.0) / predicted).clamp(1.0 / 16.0, 16.0);
        self.factors[class] *= ratio.sqrt();
    }

    /// A frame coded too large to send: the model takes what it measured.
    pub fn oversized(&mut self, class: usize, qindex: u8, bits: f64) {
        self.factors[class] = bits * f64::from(ac_quantizer(qindex)) / self.pixels;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_fills_with_time_and_caps() {
        let mut rate = Rate::new(
            1_000_000,
            30,
            Duration::from_millis(300),
            640 * 360,
            1..=255,
        );
        rate.target(Duration::ZERO, false, 1.0);
        rate.sent(1, 100, 400_000.0);
        // 0.2 s of credit minus 0.4 s spent: dropped until it recovers.
        assert_eq!(rate.target(Duration::from_millis(100), false, 1.0), None);
        assert!(
            rate.target(Duration::from_millis(300), false, 1.0)
                .is_some()
        );
        // Idle seconds leave at most the cap.
        rate.target(Duration::from_secs(10), false, 1.0);
        rate.target(Duration::from_secs(20), false, 1.0);
        assert_eq!(rate.credit, CAP * 1_000_000.0);
    }

    #[test]
    fn the_model_follows_measured_frames() {
        let mut rate = Rate::new(
            2_000_000,
            30,
            Duration::from_millis(300),
            1280 * 720,
            1..=255,
        );
        let start = rate.qindex(1, 50_000.0);
        // Frames twice the prediction push the quantizer up.
        for _ in 0..10 {
            let q = rate.qindex(1, 50_000.0);
            let predicted = rate.factors[1] * rate.pixels / f64::from(ac_quantizer(q));
            rate.sent(1, q, 2.0 * predicted);
        }
        assert!(rate.qindex(1, 50_000.0) > start);
        rate.oversized(0, 100, 4_000_000.0);
        assert_eq!(rate.factors[0], 4_000_000.0 * 112.0 / (1280.0 * 720.0));
    }
}
