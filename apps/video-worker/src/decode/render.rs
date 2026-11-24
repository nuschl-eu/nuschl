// SPDX-License-Identifier: EUPL-1.2
//! The render size of an AV1 stream: the picture's size inside its coded
//! frame. Hardware encoders code aligned frames and send the picture's size
//! as the render size; VCN 4 codes 1920 × 1080 as 1920 × 1082. Decoders
//! output the whole frame, so the worker reads the render size from each
//! shown keyframe (AV1 sections 5.3, 5.5 and 5.9).

const SEQUENCE_HEADER: u8 = 1;
const FRAME_HEADER: u8 = 3;
const FRAME: u8 = 6;
/// `SELECT_SCREEN_CONTENT_TOOLS` and `SELECT_INTEGER_MV`.
const SELECT: u32 = 2;

/// The render size of the newest shown keyframe.
#[derive(Default)]
pub struct Render {
    sequence: Option<Sequence>,
    size: Option<(u16, u16)>,
}

impl Render {
    /// Reads one temporal unit's headers and returns the stream's render
    /// size, or `None` when its headers lack one this reader supports.
    pub fn update(&mut self, unit: &[u8]) -> Option<(u16, u16)> {
        for (kind, payload) in obus(unit) {
            match kind {
                SEQUENCE_HEADER => {
                    self.sequence = Sequence::read(payload);
                    self.size = None;
                }
                FRAME_HEADER | FRAME => {
                    if let Some(sequence) = &self.sequence {
                        let mut bits = Reader {
                            data: payload,
                            bit: 0,
                        };
                        if sequence.reduced || bits.shown_keyframe() == Some(true) {
                            self.size = sequence.render(&mut bits);
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        self.size
    }
}

/// The type and payload of each OBU in a temporal unit.
fn obus(mut data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        let (&header, rest) = data.split_first()?;
        let rest = rest.get(usize::from((header >> 2) & 1)..)?;
        let (payload, next) = if header & 2 == 0 {
            (rest, &[][..])
        } else {
            let (size, rest) = leb128(rest)?;
            rest.split_at_checked(size)?
        };
        data = next;
        Some(((header >> 3) & 15, payload))
    })
}

fn leb128(data: &[u8]) -> Option<(usize, &[u8])> {
    let mut value = 0u64;
    for (index, byte) in data.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Some((usize::try_from(value).ok()?, &data[index + 1..]));
        }
    }
    None
}

/// The sequence header fields a keyframe header's render size depends on.
struct Sequence {
    reduced: bool,
    width_bits: u32,
    height_bits: u32,
    width: u32,
    height: u32,
    frame_id_bits: u32,
    screen_content: u32,
    integer_mv: u32,
    order_hint_bits: u32,
    superres: bool,
}

impl Sequence {
    /// `None` for sequences with a decoder model, whose frame headers this
    /// reader skips.
    fn read(payload: &[u8]) -> Option<Self> {
        let mut bits = Reader {
            data: payload,
            bit: 0,
        };
        bits.skip(4)?; // seq_profile, still_picture
        let reduced = bits.flag()?;
        if reduced {
            bits.skip(5)?; // seq_level_idx[0]
        } else {
            if bits.flag()? {
                // timing_info()
                bits.skip(64)?;
                if bits.flag()? {
                    bits.uvlc()?; // num_ticks_per_picture_minus_1
                }
                if bits.flag()? {
                    return None; // decoder_model_info_present_flag
                }
            }
            let initial_display_delay = bits.flag()?;
            for _ in 0..=bits.f(5)? {
                bits.skip(12)?; // operating_point_idc
                if bits.f(5)? > 7 {
                    bits.skip(1)?; // seq_tier
                }
                if initial_display_delay && bits.flag()? {
                    bits.skip(4)?; // initial_display_delay_minus_1
                }
            }
        }
        let width_bits = bits.f(4)? + 1;
        let height_bits = bits.f(4)? + 1;
        let width = bits.f(width_bits)? + 1;
        let height = bits.f(height_bits)? + 1;
        let frame_id_bits = if !reduced && bits.flag()? {
            bits.f(4)? + 2 + bits.f(3)? + 1
        } else {
            0
        };
        bits.skip(3)?; // 128 × 128 superblocks, filter intra, intra edge filter
        let (mut screen_content, mut integer_mv, mut order_hint_bits) = (SELECT, SELECT, 0);
        if !reduced {
            bits.skip(4)?; // interintra and masked compound, warped motion, dual filter
            let order_hint = bits.flag()?;
            if order_hint {
                bits.skip(2)?; // jnt_comp, ref_frame_mvs
            }
            screen_content = if bits.flag()? { SELECT } else { bits.f(1)? };
            if screen_content > 0 {
                integer_mv = if bits.flag()? { SELECT } else { bits.f(1)? };
            }
            if order_hint {
                order_hint_bits = bits.f(3)? + 1;
            }
        }
        Some(Self {
            reduced,
            width_bits,
            height_bits,
            width,
            height,
            frame_id_bits,
            screen_content,
            integer_mv,
            order_hint_bits,
            superres: bits.flag()?,
        })
    }

    /// The render size from a shown keyframe's header, read up to its frame
    /// type.
    fn render(&self, bits: &mut Reader) -> Option<(u16, u16)> {
        bits.skip(1)?; // disable_cdf_update
        let screen_content = if self.screen_content == SELECT {
            bits.f(1)?
        } else {
            self.screen_content
        };
        if screen_content > 0 && self.integer_mv == SELECT {
            bits.skip(1)?; // force_integer_mv
        }
        bits.skip(self.frame_id_bits)?; // current_frame_id
        let override_size = !self.reduced && bits.flag()?;
        bits.skip(self.order_hint_bits)?;
        let (width, height) = if override_size {
            (bits.f(self.width_bits)? + 1, bits.f(self.height_bits)? + 1)
        } else {
            (self.width, self.height)
        };
        if self.superres && bits.flag()? {
            bits.skip(3)?; // coded_denom
        }
        let (width, height) = if bits.flag()? {
            (bits.f(16)? + 1, bits.f(16)? + 1)
        } else {
            (width, height)
        };
        Some((u16::try_from(width).ok()?, u16::try_from(height).ok()?))
    }
}

struct Reader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Reader<'_> {
    fn f(&mut self, bits: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..bits {
            let byte = self.data.get(self.bit / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.f(1)? == 1)
    }

    fn skip(&mut self, bits: u32) -> Option<()> {
        self.bit += bits as usize;
        (self.bit <= self.data.len() * 8).then_some(())
    }

    fn uvlc(&mut self) -> Option<()> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
        }
        if zeros < 32 {
            self.skip(zeros)?;
        }
        Some(())
    }

    /// `show_existing_frame` unset, `frame_type` KEY_FRAME and `show_frame`.
    fn shown_keyframe(&mut self) -> Option<bool> {
        Some(!self.flag()? && self.f(2)? == 0 && self.flag()?)
    }
}

#[cfg(test)]
mod tests {
    use super::Render;
    use nuschl_video_encode::{FrameHeader, Sequence, temporal_delimiter};

    fn unit(sequence: &Sequence, key: bool, render: (u16, u16)) -> Vec<u8> {
        let header = FrameHeader {
            key,
            temporal_id: u8::from(!key),
            order_hint: u8::from(!key),
            reference: 0,
            refresh: if key { 0xff } else { 1 },
            qindex: 100,
            screen_content: sequence.screen,
            loop_filter: [0; 4],
            tx_mode_select: true,
            disable_cdf_update: false,
            render,
        };
        let mut unit = temporal_delimiter();
        if key {
            unit.extend(sequence.obu());
        }
        unit.extend(header.obu(sequence, Some(&[0; 16])));
        unit
    }

    #[test]
    fn keyframes_set_the_render_size_inside_an_aligned_frame() {
        for (screen, layers, frame, picture) in [
            (true, 3, (1920, 1082), (1920, 1080)),
            (true, 1, (3456, 1440), (3440, 1440)),
            (false, 2, (1280, 720), (1280, 720)),
        ] {
            let sequence = Sequence::new(frame.0, frame.1, 30, layers, screen).unwrap();
            let mut render = Render::default();
            assert_eq!(
                render.update(&unit(&sequence, true, picture)),
                Some(picture)
            );
            // Inter frames keep the keyframe's size.
            assert_eq!(
                render.update(&unit(&sequence, false, (64, 64))),
                Some(picture)
            );
            let changed = (picture.0 - 2, picture.1 - 2);
            assert_eq!(
                render.update(&unit(&sequence, true, changed)),
                Some(changed)
            );
        }
    }

    #[test]
    fn rav1e_frames_show_their_frame_size() {
        let settings = nuschl_ipc::video::Settings {
            source: nuschl_ipc::video::Source::Camera,
            width: 160,
            height: 120,
            fps: 30,
            bitrate: 500_000,
        };
        let i420 = vec![128; settings.frame_bytes().unwrap()];
        let key = crate::codec::encode(&crate::codec::config(settings), settings, &i420).unwrap();
        assert_eq!(Render::default().update(&key), Some((160, 120)));
    }

    #[test]
    fn truncated_and_unknown_units_have_no_render_size() {
        let sequence = Sequence::new(1920, 1082, 30, 3, true).unwrap();
        let key = unit(&sequence, true, (1920, 1080));
        for end in 0..key.len() {
            assert_eq!(Render::default().update(&key[..end]), None, "{end}");
        }
        let mut render = Render::default();
        assert_eq!(render.update(&unit(&sequence, false, (64, 64))), None);
        render.update(&key);
        // A new sequence header drops the old size until its keyframe.
        assert_eq!(render.update(&key[..key.len() - 20]), None);
    }
}
