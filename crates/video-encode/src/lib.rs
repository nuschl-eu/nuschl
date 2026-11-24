// SPDX-License-Identifier: EUPL-1.2
//! Nuschl's real-time AV1 encoder core. The platform-independent part makes every
//! real-time decision: whether to drop a frame, its quantizer from the sizes of
//! earlier frames ([`rate`]), which reference slot it predicts from and replaces
//! for temporal layers ([`Layering`]), its AV1 headers ([`Sequence`],
//! [`FrameHeader`]) and the Dependency Descriptor the SFU forwards layers by.
//!
//! A [`Backend`] drives the hardware: it codes one frame as a [`Frame`] describes
//! and returns the temporal unit. A backend that writes the headers itself after
//! coding (D3D12 reports its loop filter and CDEF choices) adjusts a copy of the
//! [`FrameHeader`] and writes the frame OBU with [`FrameHeader::obu`].

mod av1;
mod bits;
mod dd;
mod layers;
mod rate;

pub use av1::{FrameHeader, Sequence, temporal_delimiter};
pub use layers::Layering;

use av1::{ac_quantizer, loop_filter};
use std::ops::RangeInclusive;
use std::time::Duration;

/// The quantizer for a refinement step of a still picture: half the
/// reference's AC step.
fn finer(reference: u8) -> u8 {
    (0..reference)
        .rev()
        .find(|&q| 2 * ac_quantizer(q) <= ac_quantizer(reference))
        .unwrap_or(0)
}

/// One frame for a backend to code.
pub struct Frame<'a> {
    pub sequence: &'a Sequence,
    pub header: &'a FrameHeader,
    /// This frame's id. It occupies the slots `header.refresh` names once the
    /// encoder sends it; a later frame's `slots` show whether it did.
    pub id: u64,
    /// The id of the frame each of the eight reference slots holds.
    pub slots: [Option<u64>; 8],
}

/// Platform video encoding: hardware or a software codec.
pub trait Backend {
    type Input;
    type Error;

    /// Whether the encoder may drop a coded frame. VA-API takes each frame's
    /// references with the frame; libaom's reference slots follow every frame
    /// it codes, so the encoder codes each input once there and follows a
    /// frame it cannot send with a keyframe.
    const DISCARDS: bool = true;

    /// Codes `input` with the frame's quantizer, reference and refreshed slots,
    /// and returns the temporal unit: the temporal delimiter, the sequence header
    /// before a keyframe, and the frame. With `DISCARDS`, the encoder may code
    /// the same input again at a coarser quantizer.
    fn encode(&mut self, input: &Self::Input, frame: &Frame<'_>) -> Result<Vec<u8>, Self::Error>;
}

/// What the encoder produces, at what rate.
#[derive(Clone, Debug)]
pub struct Config {
    pub width: u16,
    pub height: u16,
    /// The highest frame rate the source delivers.
    pub fps: u16,
    pub bitrate: u32,
    pub layering: Layering,
    /// Screen content: frames may use palette and intra block copy.
    pub screen: bool,
    /// The largest frame a receiver accepts, in bytes.
    pub frame_bytes: usize,
    /// The largest frame as time at the bitrate: a frame above it is coded again
    /// coarser.
    pub window: Duration,
    /// The quantizer range; a lower maximum keeps detail and drops more frames.
    pub qindex: RangeInclusive<u8>,
}

/// One coded frame.
#[derive(Clone, Debug)]
pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub temporal_id: u8,
    pub qindex: u8,
    /// The Dependency Descriptor for the frame's packets, with start and end of
    /// frame set: a packetizer clears bit 7 of byte 0 on all packets but the
    /// first and bit 6 on all but the last.
    pub descriptor: Vec<u8>,
}

pub struct Encoder<B> {
    backend: B,
    sequence: Sequence,
    layering: Layering,
    screen: bool,
    frame_bytes: usize,
    pattern: layers::Pattern,
    rate: rate::Rate,
    keyframe: bool,
    /// The quantizer of the frame each slot holds.
    slot_qindex: [u8; 8],
    /// Whether the picture has stayed still since a frame repeated it.
    still: bool,
    /// Frames sent: the next frame's id, frame number and order hint.
    sent: u64,
}

impl<B: Backend> Encoder<B> {
    /// `None` when the picture needs more than one tile.
    pub fn new(backend: B, config: Config) -> Option<Self> {
        let sequence = Sequence::new(
            config.width,
            config.height,
            config.fps,
            config.layering.layers(),
            config.screen,
        )?;
        Some(Self {
            backend,
            sequence,
            layering: config.layering,
            screen: config.screen,
            frame_bytes: config.frame_bytes,
            pattern: layers::Pattern::new(config.layering),
            rate: rate::Rate::new(
                config.bitrate,
                config.fps,
                config.window,
                u32::from(config.width) * u32::from(config.height),
                config.qindex,
            ),
            keyframe: true,
            slot_qindex: [255; 8],
            still: false,
            sent: 0,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.keyframe = true;
    }

    /// The frames sent so far: an encoder opened for a new picture size
    /// continues from them ([`Encoder::continuing`]).
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Numbers frames on from an encoder that sent `sent`, so receivers read
    /// the new stream's frames as new ones.
    pub fn continuing(mut self, sent: u64) -> Self {
        self.sent = sent;
        self
    }

    /// Applies to the next frame; the stream continues without a keyframe.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.rate.set_bitrate(bitrate);
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Codes the picture captured at `now`, or drops it to hold the bitrate.
    pub fn encode(&mut self, input: &B::Input, now: Duration) -> Result<Option<Encoded>, B::Error> {
        let plan = self.pattern.plan(self.keyframe);
        let class = rate::class(plan.key, plan.temporal);
        let Some(target) = self
            .rate
            .target(now, plan.key, self.layering.weight(plan.temporal))
        else {
            return Ok(None);
        };
        // While the picture stays still, frames that replace the slot they
        // predict from refine it step by step; the others take the reference's
        // quantizer, so their loop filters keep its detail. A frame no other
        // frame references never codes finer than its reference: the detail
        // would be lost with it.
        let reference = self.slot_qindex[usize::from(plan.reference)];
        let mut qindex = self.rate.qindex(class, target);
        let persistent = plan.refresh >> plan.reference & 1 == 1;
        let refining = self.still && persistent && !plan.key;
        if !plan.key {
            qindex = match (self.still, persistent) {
                (true, true) => qindex.min(finer(reference)).max(self.rate.min_qindex()),
                (true, false) => reference,
                (false, _) if plan.refresh == 0 => qindex.max(reference),
                (false, _) => qindex,
            };
        }
        let mut retried = false;
        loop {
            let header = FrameHeader {
                key: plan.key,
                temporal_id: plan.temporal,
                order_hint: self.sent as u8,
                reference: plan.reference,
                refresh: plan.refresh,
                qindex,
                screen_content: self.screen,
                loop_filter: loop_filter(qindex, plan.key),
                tx_mode_select: true,
                disable_cdf_update: false,
                render: (self.sequence.width, self.sequence.height),
            };
            let frame = Frame {
                sequence: &self.sequence,
                header: &header,
                id: self.sent,
                slots: self.pattern.slots,
            };
            let data = self.backend.encode(input, &frame)?;
            let bits = data.len() as f64 * 8.0;
            let fits = data.len() <= self.frame_bytes;
            if !fits || bits > self.rate.frame_limit() {
                // A sudden change after small frames: code it again with the
                // measured model, once.
                if B::DISCARDS && !retried && qindex < self.rate.max_qindex() {
                    self.rate.oversized(class, qindex, bits);
                    qindex = self.rate.qindex(class, target).max(qindex + 1);
                    retried = true;
                    continue;
                }
                if !fits {
                    self.rate.oversized(class, qindex, bits);
                    // The backend's slots now hold the frame it cannot send.
                    self.keyframe |= !B::DISCARDS;
                    return Ok(None);
                }
            }
            // The picture stays still through refinement steps and through
            // frames within their target; a change exceeds it.
            let tiny = self.rate.sent(class, qindex, bits);
            self.still = tiny || (self.still && (refining || bits <= target));
            self.pattern.commit(plan, self.sent);
            for (slot, held) in self.slot_qindex.iter_mut().enumerate() {
                if plan.refresh >> slot & 1 == 1 {
                    *held = qindex;
                }
            }
            let descriptor = dd::descriptor(
                self.layering,
                plan.template,
                self.sent as u16,
                plan.key.then_some(header.render),
            );
            self.sent += 1;
            self.keyframe &= !plan.key;
            return Ok(Some(Encoded {
                data,
                keyframe: plan.key,
                temporal_id: plan.temporal,
                qindex,
                descriptor,
            }));
        }
    }
}

#[cfg(test)]
mod decode_tests;
#[cfg(test)]
mod rate_tests;
