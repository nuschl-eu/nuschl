// SPDX-License-Identifier: EUPL-1.2
//! libaom's AV1 encoder in real-time mode, for the confined video worker's
//! software path (SECURITY.md). The caller plans each frame: its type,
//! temporal layer, reference slot, refreshed slots and quantizer. libaom codes
//! it and writes the headers, with an operating point per temporal layer.

#[allow(unsafe_code)]
mod ffi;

pub use ffi::Encoder;

/// An encoder's fixed parameters.
pub struct Config {
    /// Even, like the I420 pictures the encoder takes.
    pub width: u16,
    /// Even, like the I420 pictures the encoder takes.
    pub height: u16,
    pub fps: u16,
    /// Temporal layers, 1 to 3.
    pub layers: u8,
    /// Screen content: palette and libaom's screen tuning.
    pub screen: bool,
    pub threads: u8,
}

/// One frame as the caller plans it.
pub struct Frame {
    pub key: bool,
    pub temporal_id: u8,
    /// The slot an inter frame predicts from.
    pub reference: u8,
    /// The slots an inter frame replaces, one bit each; a keyframe replaces
    /// all.
    pub refresh: u8,
    /// `base_q_idx`. libaom codes in steps of 4, from 4 to 255: the frame
    /// takes the nearest step at or above it.
    pub qindex: u8,
}

/// A libaom call that failed, with its `aom_codec_err_t`.
#[derive(Debug)]
pub struct Error {
    pub call: &'static str,
    pub code: u32,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} failed with libaom error {}",
            self.call, self.code
        )
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
