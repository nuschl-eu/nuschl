// SPDX-License-Identifier: EUPL-1.2
//! dav1d (rav1d) decodes streams with the encoder's sequence headers, frame
//! headers, OBU framing and temporal layers. A synthetic backend codes every
//! 64 × 64 superblock as one skipped DC-predicted block with the specification's
//! default CDFs, so each picture decodes to mid-grey.

use super::*;
use rav1d::{Decoder, PlanarImageComponent, Rav1dError, Settings};
use std::convert::Infallible;

/// libaom's range encoder (`od_ec_enc`) without CDF adaptation, which the
/// synthetic frames turn off.
struct Symbols {
    low: u64,
    range: u32,
    count: i32,
    precarry: Vec<u16>,
}

impl Symbols {
    fn new() -> Self {
        Self {
            low: 0,
            range: 0x8000,
            count: -9,
            precarry: Vec::new(),
        }
    }

    /// `cdf` holds the specification's cumulative probabilities of the symbols
    /// before the last, in 1/32768.
    fn write(&mut self, symbol: usize, cdf: &[u16]) {
        let last = cdf.len() as u32;
        let inverse = |i: usize| cdf.get(i).map_or(0, |&p| 32768 - u32::from(p));
        let s = symbol as u32;
        let r = self.range;
        let scaled = |f: u32, n: u32| (((r >> 8) * (f >> 6)) >> 1) + 4 * n;
        let (low, range) = if symbol > 0 {
            let u = scaled(inverse(symbol - 1), last + 1 - s);
            let v = scaled(inverse(symbol), last - s);
            (self.low + u64::from(r - u), u - v)
        } else {
            (self.low, r - scaled(inverse(0), last))
        };
        self.normalize(low, range);
    }

    fn normalize(&mut self, mut low: u64, range: u32) {
        let shift = 16 - (u32::BITS - range.leading_zeros()) as i32;
        let mut c = self.count;
        let mut s = c + shift;
        if s >= 0 {
            c += 16;
            let mut mask = (1u64 << c) - 1;
            if s >= 8 {
                self.precarry.push((low >> c) as u16);
                low &= mask;
                c -= 8;
                mask >>= 8;
            }
            self.precarry.push((low >> c) as u16);
            s = c + shift - 24;
            low &= mask;
        }
        self.low = low << shift;
        self.range = range << shift;
        self.count = s;
    }

    fn finish(mut self) -> Vec<u8> {
        let mut c = self.count;
        let mut s = c + 10;
        let mask = 0x3fffu64;
        let mut e = ((self.low + mask) & !mask) | (mask + 1);
        if s > 0 {
            let mut n = (1u64 << (c + 16)) - 1;
            while s > 0 {
                self.precarry.push((e >> (c + 16)) as u16);
                e &= n;
                s -= 8;
                c -= 8;
                n >>= 8;
            }
        }
        let mut out = vec![0; self.precarry.len()];
        let mut carry = 0u32;
        for (byte, &value) in out.iter_mut().zip(&self.precarry).rev() {
            carry += u32::from(value);
            *byte = carry as u8;
            carry >>= 8;
        }
        out
    }
}

// Default CDFs (AV1 specification §9.4).
const PARTITION_W64: [u16; 9] = [
    20137, 21547, 23078, 29566, 29837, 30261, 30524, 30892, 31724,
];
const SKIP: [[u16; 1]; 3] = [[31671], [16515], [4576]];
const IS_INTER: [[u16; 1]; 4] = [[806], [16662], [20186], [26538]];
const INTRA_FRAME_Y_MODE_DC_DC: [u16; 12] = [
    15588, 17027, 19338, 20218, 20682, 21110, 21825, 23244, 24189, 28165, 29093, 30466,
];
const Y_MODE_64X64: [u16; 12] = [
    20155, 21301, 22838, 23178, 23261, 23533, 23703, 24804, 25352, 26575, 27016, 28049,
];
const UV_MODE_DC_WITHOUT_CFL: [u16; 12] = [
    22631, 24152, 25378, 25661, 25986, 26520, 27055, 27923, 28244, 30059, 30941, 31961,
];
const PALETTE_Y_64X64: [u16; 1] = [32450];
const PALETTE_UV: [u16; 1] = [32461];

/// One tile of skipped 64 × 64 blocks predicted from DC.
fn grey_tile(sequence: &Sequence, header: &FrameHeader) -> Vec<u8> {
    assert!(sequence.width.is_multiple_of(64) && sequence.height.is_multiple_of(64));
    let mut symbols = Symbols::new();
    for row in 0..sequence.height / 64 {
        for column in 0..sequence.width / 64 {
            let (above, left) = (row > 0, column > 0);
            symbols.write(0, &PARTITION_W64); // PARTITION_NONE
            symbols.write(1, &SKIP[usize::from(above) + usize::from(left)]);
            if header.key {
                symbols.write(0, &INTRA_FRAME_Y_MODE_DC_DC);
            } else {
                let context = match (above, left) {
                    (true, true) => 3,
                    (false, false) => 0,
                    _ => 2,
                };
                symbols.write(0, &IS_INTER[context]); // an intra block
                symbols.write(0, &Y_MODE_64X64);
            }
            symbols.write(0, &UV_MODE_DC_WITHOUT_CFL);
            if header.screen_content {
                symbols.write(0, &PALETTE_Y_64X64);
                symbols.write(0, &PALETTE_UV);
            }
        }
    }
    symbols.finish()
}

struct Grey;

impl Backend for Grey {
    type Input = ();
    type Error = Infallible;

    fn encode(&mut self, _: &(), frame: &Frame<'_>) -> Result<Vec<u8>, Infallible> {
        let mut header = frame.header.clone();
        header.tx_mode_select = false;
        header.disable_cdf_update = true;
        let mut unit = temporal_delimiter();
        if header.key {
            unit.extend(frame.sequence.obu());
        }
        unit.extend(header.obu(frame.sequence, Some(&grey_tile(frame.sequence, &header))));
        Ok(unit)
    }
}

fn config(width: u16, height: u16, layering: Layering, screen: bool) -> Config {
    Config {
        width,
        height,
        fps: 30,
        bitrate: 1_000_000,
        layering,
        screen,
        frame_bytes: 1 << 20,
        window: Duration::from_secs(1),
        qindex: 1..=255,
    }
}

/// Temporal units and their layers, with a keyframe request before frame
/// `rekey`.
fn stream(config: Config, frames: u32, rekey: u32) -> Vec<(u8, Vec<u8>)> {
    let mut encoder = Encoder::new(Grey, config).expect("one tile");
    (0..frames)
        .map(|frame| {
            if frame == rekey {
                encoder.request_keyframe();
            }
            let now = Duration::from_millis(u64::from(frame) * 33);
            let encoded = encoder.encode(&(), now).unwrap().expect("not dropped");
            assert_eq!(encoded.keyframe, frame == 0 || frame == rekey);
            (encoded.temporal_id, encoded.data)
        })
        .collect()
}

/// Decodes every temporal unit at `operating_point` and returns the pictures'
/// sizes after checking they are grey.
fn decode(units: &[(u8, Vec<u8>)], operating_point: u8) -> Vec<(u32, u32)> {
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    settings.set_operating_point(operating_point);
    let mut decoder = Decoder::with_settings(&settings).expect("decoder");
    let mut sizes = Vec::new();
    for (_, unit) in units {
        let mut sent = decoder.send_data(unit.clone().into_boxed_slice(), None, None, None);
        loop {
            match decoder.get_picture() {
                Ok(picture) => {
                    let stride = picture.stride(PlanarImageComponent::Y) as usize;
                    let luma = picture.plane(PlanarImageComponent::Y);
                    for row in 0..picture.height() as usize {
                        let line = &luma[row * stride..][..picture.width() as usize];
                        assert!(line.iter().all(|&y| y == 128), "row {row}");
                    }
                    sizes.push((picture.width(), picture.height()));
                }
                Err(Rav1dError::TryAgain) => {}
                Err(error) => panic!("decoding failed: {error:?}"),
            }
            match sent {
                Err(Rav1dError::TryAgain) => sent = decoder.send_pending_data(),
                Ok(()) => break,
                Err(error) => panic!("decoding failed: {error:?}"),
            }
        }
    }
    sizes
}

#[test]
fn dav1d_decodes_each_operating_point_of_l1t3() {
    let units = stream(config(192, 128, Layering::L1T3, true), 24, 13);
    let layers: Vec<_> = units.iter().map(|unit| unit.0).collect();
    assert_eq!(&layers[..13], [0, 2, 1, 2, 0, 2, 1, 2, 0, 2, 1, 2, 0]);
    // The keyframe at 13 restarts the pattern.
    assert_eq!(&layers[13..18], [0, 2, 1, 2, 0]);
    for (point, highest) in [(0, 2), (1, 1), (2, 0)] {
        let sizes = decode(&units, point);
        let expected = layers.iter().filter(|&&layer| layer <= highest).count();
        assert_eq!(sizes.len(), expected, "operating point {point}");
        assert!(sizes.iter().all(|&size| size == (192, 128)));
    }
}

#[test]
fn dav1d_decodes_single_layer_and_two_layer_streams() {
    let units = stream(config(64, 64, Layering::L1T1, false), 10, 6);
    assert_eq!(decode(&units, 0).len(), 10);
    let units = stream(config(128, 64, Layering::L1T2, false), 10, 99);
    assert_eq!(decode(&units, 0).len(), 10);
    assert_eq!(decode(&units, 1).len(), 5);
}
