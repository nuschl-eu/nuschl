// SPDX-License-Identifier: EUPL-1.2
use std::time::{Duration, Instant};

pub(crate) const WINDOW: Duration = Duration::from_millis(120);

/// Tokens are committed only after the transport accepts a frame. Every bucket
/// has a finite burst window; a bandwidth reduction clamps accumulated credit.
pub(crate) struct Bucket {
    tokens: f64,
    updated: Instant,
    window: Duration,
}

impl Bucket {
    pub fn new(now: Instant, window: Duration) -> Self {
        Self {
            tokens: f64::INFINITY,
            updated: now,
            window,
        }
    }

    pub fn available(&self, now: Instant, rate: u64) -> f64 {
        let cap = rate as f64 * self.window.as_secs_f64();
        (self.tokens + now.saturating_duration_since(self.updated).as_secs_f64() * rate as f64)
            .min(cap)
    }

    /// Whether a frame of `bits` may go out: when the tokens cover it, or are
    /// full for a frame longer than the window.
    pub fn admits(&self, now: Instant, rate: u64, bits: f64) -> bool {
        self.available(now, rate) >= bits.min(rate as f64 * self.window.as_secs_f64())
    }

    /// A frame longer than the window takes the full window's tokens.
    pub fn spend(&mut self, now: Instant, rate: u64, bits: f64) {
        self.tokens = (self.available(now, rate) - bits).max(0.0);
        self.updated = now;
    }
}
