// SPDX-License-Identifier: EUPL-1.2
//! AV1 sequence and frame headers (AV1 specification §5.5 and §5.9) for the
//! encoder's subset: 8-bit 4:2:0 Main profile, up to two tile rows, shown frames, single
//! reference prediction, coding tools a hardware encoder decides inside its frame.
//! The tool flags match what Mesa's radeonsi writes, so its rewritten headers and
//! a driver that sends ours describe the same stream.

use crate::bits::{
    Bits, OBU_FRAME, OBU_FRAME_HEADER, OBU_SEQUENCE_HEADER, OBU_TEMPORAL_DELIMITER, obu,
};

pub const ORDER_HINT_BITS: u32 = 8;
const KEY_FRAME: u32 = 0;
const INTER_FRAME: u32 = 1;
/// One tile covers at most 4096 pixels across and 4096 × 2304 pixels (Annex A).
const MAX_TILE_WIDTH_SB: u32 = 64;
const MAX_TILE_AREA_SB: u32 = 2304;

/// The parameters every frame of a coded video sequence shares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sequence {
    pub width: u16,
    pub height: u16,
    /// Temporal layers, each an operating point: 1 to 3.
    pub layers: u8,
    /// `seq_level_idx` (Annex A.3).
    pub level: u8,
    pub high_tier: bool,
    /// Width of the tile size field when a frame uses two rows.
    tile_size_bytes: Option<u8>,
    /// Whether frames choose screen content tools (palette, intra block copy).
    pub screen: bool,
}

impl Sequence {
    /// A sequence of `width` × `height` frames at up to `fps`, or `None` when a
    /// frame needs more than one tile.
    pub fn new(width: u16, height: u16, fps: u16, layers: u8, screen: bool) -> Option<Self> {
        let (columns, rows) = superblocks(width, height);
        if width == 0
            || height == 0
            || !(1..=3).contains(&layers)
            || columns > MAX_TILE_WIDTH_SB
            || columns * rows > MAX_TILE_AREA_SB
        {
            return None;
        }
        Some(Self {
            width,
            height,
            layers,
            level: level(width, height, fps, 0).0,
            high_tier: false,
            tile_size_bytes: None,
            screen,
        })
    }

    /// Widens the sequence's envelope as the capture rate rises. Returns
    /// whether the next frame needs a new sequence header.
    pub(crate) fn expand(&mut self, fps: u16, bitrate: u32, tile_size_bytes: Option<u8>) -> bool {
        let current = (self.level, self.high_tier);
        let next = current.max(level(self.width, self.height, fps, bitrate));
        let tiles =
            self.tile_size_bytes.or(tile_size_bytes
                .filter(|size| (1..=4).contains(size) && self.height.div_ceil(64) > 1));
        let changed = current != next || tiles != self.tile_size_bytes;
        (self.level, self.high_tier) = next;
        self.tile_size_bytes = tiles;
        changed
    }

    /// Superblock heights of the two uniform tile rows, when enabled.
    pub fn tile_rows(&self) -> Option<[u16; 2]> {
        let rows = self.height.div_ceil(64);
        let first = rows.div_ceil(2);
        self.tile_size_bytes.map(|_| [first, rows - first])
    }

    /// The sequence header OBU.
    pub fn obu(&self) -> Vec<u8> {
        let mut bits = Bits::default();
        bits.put(0, 3); // seq_profile: Main
        bits.flag(false); // still_picture
        bits.flag(false); // reduced_still_picture_header
        bits.flag(false); // timing_info_present_flag
        bits.flag(false); // initial_display_delay_present_flag
        bits.put(u32::from(self.layers - 1), 5); // operating_points_cnt_minus_1
        for point in 0..self.layers {
            // Operating point i decodes spatial layer 0 and temporal layers 0 to
            // layers - 1 - i; a single layer needs no scalability information.
            let idc = if self.layers == 1 {
                0
            } else {
                1 << 8 | ((1 << (self.layers - point)) - 1)
            };
            bits.put(idc, 12);
            bits.put(u32::from(self.level), 5);
            if self.level > 7 {
                bits.flag(self.high_tier); // seq_tier
            }
        }
        let width_bits = significant_bits(self.width - 1);
        let height_bits = significant_bits(self.height - 1);
        bits.put(width_bits - 1, 4);
        bits.put(height_bits - 1, 4);
        bits.put(u32::from(self.width - 1), width_bits);
        bits.put(u32::from(self.height - 1), height_bits);
        bits.flag(false); // frame_id_numbers_present_flag
        // 128 × 128 superblocks, filter intra, intra edge filter, inter-intra and
        // masked compound, warped motion, dual filter: off.
        bits.put(0, 7);
        bits.flag(true); // enable_order_hint
        bits.put(0, 2); // enable_jnt_comp, enable_ref_frame_mvs
        if self.screen {
            bits.flag(true); // seq_choose_screen_content_tools
            bits.flag(false); // seq_choose_integer_mv
            bits.flag(false); // seq_force_integer_mv
        } else {
            bits.flag(false); // seq_choose_screen_content_tools
            bits.flag(false); // seq_force_screen_content_tools
        }
        bits.put(ORDER_HINT_BITS - 1, 3);
        bits.flag(false); // enable_superres
        bits.flag(true); // enable_cdef
        bits.flag(false); // enable_restoration
        // color_config(): 8 bits, 4:2:0, BT.709 in limited range.
        bits.flag(false); // high_bitdepth
        bits.flag(false); // mono_chrome
        bits.flag(true); // color_description_present_flag
        for code in [1, 1, 1] {
            bits.put(code, 8); // primaries, transfer, matrix: BT.709
        }
        bits.flag(false); // color_range: limited
        bits.put(0, 2); // chroma_sample_position: unknown
        bits.flag(false); // separate_uv_delta_q
        bits.flag(false); // film_grain_params_present
        bits.trailing();
        obu(OBU_SEQUENCE_HEADER, None, &bits.into_bytes())
    }
}

/// One frame's header fields. Inter frames predict from one reference slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub key: bool,
    pub temporal_id: u8,
    pub order_hint: u8,
    /// The slot an inter frame predicts from.
    pub reference: u8,
    /// The slots an inter frame replaces; a keyframe replaces all.
    pub refresh: u8,
    /// `base_q_idx`, 1 to 255.
    pub qindex: u8,
    /// `allow_screen_content_tools`, in a sequence that lets frames choose.
    pub screen_content: bool,
    /// Luma vertical and horizontal, then U and V.
    pub loop_filter: [u8; 4],
    pub tx_mode_select: bool,
    pub disable_cdf_update: bool,
    /// The picture's size inside the coded frame.
    pub render: (u16, u16),
}

impl FrameHeader {
    /// A frame header OBU, or with `tiles` a frame OBU carrying the frame's one
    /// tile.
    pub fn obu(&self, sequence: &Sequence, tiles: Option<&[u8]>) -> Vec<u8> {
        let mut bits = Bits::default();
        self.write(sequence, &mut bits);
        let temporal = (sequence.layers > 1).then_some(self.temporal_id);
        match tiles {
            None => {
                bits.trailing();
                obu(OBU_FRAME_HEADER, temporal, &bits.into_bytes())
            }
            Some(tiles) => {
                bits.align();
                let mut payload = bits.into_bytes();
                payload.extend_from_slice(tiles);
                obu(OBU_FRAME, temporal, &payload)
            }
        }
    }

    fn write(&self, sequence: &Sequence, bits: &mut Bits) {
        bits.flag(false); // show_existing_frame
        bits.put(if self.key { KEY_FRAME } else { INTER_FRAME }, 2);
        bits.flag(true); // show_frame
        if !self.key {
            bits.flag(false); // error_resilient_mode; implied for shown keyframes
        }
        bits.flag(self.disable_cdf_update);
        if sequence.screen {
            bits.flag(self.screen_content); // allow_screen_content_tools
        }
        bits.flag(false); // frame_size_override_flag
        bits.put(u32::from(self.order_hint), ORDER_HINT_BITS);
        if !self.key {
            bits.put(0, 3); // primary_ref_frame: LAST, the reference
            bits.put(u32::from(self.refresh), 8);
            bits.flag(false); // frame_refs_short_signaling
            for _ in 0..7 {
                bits.put(u32::from(self.reference), 3); // ref_frame_idx
            }
        }
        // frame_size() follows from the sequence. render_size():
        bits.flag(true); // render_and_frame_size_different
        bits.put(u32::from(self.render.0.max(1) - 1), 16);
        bits.put(u32::from(self.render.1.max(1) - 1), 16);
        if self.key {
            if self.screen_content {
                bits.flag(false); // allow_intrabc
            }
        } else {
            bits.flag(false); // allow_high_precision_mv
            bits.flag(false); // is_filter_switchable
            bits.put(0, 2); // interpolation_filter: EIGHTTAP
            bits.flag(false); // is_motion_mode_switchable
        }
        if !self.disable_cdf_update {
            bits.flag(false); // disable_frame_end_update_cdf
        }
        // tile_info(): one full-width tile, or two uniform full-width tiles.
        let (columns, rows) = superblocks(sequence.width, sequence.height);
        bits.flag(true); // uniform_tile_spacing_flag
        if columns > 1 {
            bits.flag(false); // increment_tile_cols_log2
        }
        if let Some(size) = sequence.tile_size_bytes {
            bits.flag(true); // increment_tile_rows_log2
            if rows > 2 {
                bits.flag(false); // stop at two rows
            }
            bits.flag(false); // context_update_tile_id: first tile
            bits.put(u32::from(size - 1), 2); // tile_size_bytes_minus_1
        } else if rows > 1 {
            bits.flag(false); // increment_tile_rows_log2
        }
        // quantization_params()
        bits.put(u32::from(self.qindex.max(1)), 8);
        bits.put(0, 3); // no Y DC, U DC or U AC delta; V follows U
        bits.flag(false); // using_qmatrix
        bits.flag(false); // segmentation_enabled
        bits.flag(false); // delta_q_present
        // loop_filter_params(), coded since a nonzero base_q_idx is not lossless.
        let [vertical, horizontal, u, v] = self.loop_filter.map(|level| u32::from(level.min(63)));
        bits.put(vertical, 6);
        bits.put(horizontal, 6);
        if vertical != 0 || horizontal != 0 {
            bits.put(u, 6);
            bits.put(v, 6);
        }
        bits.put(0, 3); // loop_filter_sharpness
        bits.flag(false); // loop_filter_delta_enabled
        // cdef_params(): damping 3 and one strength of zero; hardware that picks
        // its own CDEF strengths writes them into its header.
        bits.put(0, 2 + 2 + 4 + 2 + 4 + 2);
        bits.flag(self.tx_mode_select);
        if !self.key {
            bits.flag(false); // reference_select
        }
        bits.flag(false); // reduced_tx_set
        if !self.key {
            bits.put(0, 7); // is_global, LAST to ALTREF
        }
    }
}

/// The temporal delimiter OBU that starts each temporal unit.
pub fn temporal_delimiter() -> Vec<u8> {
    obu(OBU_TEMPORAL_DELIMITER, None, &[])
}

/// libaom's loop filter level from the quantizer, a linear fit to its searched
/// levels for 8-bit video (`LPF_PICK_FROM_Q`).
pub fn loop_filter(qindex: u8, key: bool) -> [u8; 4] {
    let q = i64::from(ac_quantizer(qindex));
    let fit = if key {
        q * 17563 - 421_574
    } else {
        q * 12034 + 650_707
    };
    let level = ((fit + (1 << 17)) >> 18).clamp(0, 63) as u8;
    [level; 4]
}

/// The 8-bit AC quantizer step of each `base_q_idx` (§7.12.2, `Ac_Qlookup[0]`).
pub fn ac_quantizer(qindex: u8) -> u16 {
    AC_QLOOKUP[usize::from(qindex)]
}

#[rustfmt::skip]
const AC_QLOOKUP: [u16; 256] = [
    4, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
    31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54,
    55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78,
    79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101,
    102, 104, 106, 108, 110, 112, 114, 116, 118, 120, 122, 124, 126, 128, 130, 132, 134, 136, 138,
    140, 142, 144, 146, 148, 150, 152, 155, 158, 161, 164, 167, 170, 173, 176, 179, 182, 185, 188,
    191, 194, 197, 200, 203, 207, 211, 215, 219, 223, 227, 231, 235, 239, 243, 247, 251, 255, 260,
    265, 270, 275, 280, 285, 290, 295, 300, 305, 311, 317, 323, 329, 335, 341, 347, 353, 359, 366,
    373, 380, 387, 394, 401, 408, 416, 424, 432, 440, 448, 456, 465, 474, 483, 492, 501, 510, 520,
    530, 540, 550, 560, 571, 582, 593, 604, 615, 627, 639, 651, 663, 676, 689, 702, 715, 729, 743,
    757, 771, 786, 801, 816, 832, 848, 864, 881, 898, 915, 933, 951, 969, 988, 1007, 1026, 1046,
    1066, 1087, 1108, 1129, 1151, 1173, 1196, 1219, 1243, 1267, 1292, 1317, 1343, 1369, 1396, 1423,
    1451, 1479, 1508, 1537, 1567, 1597, 1628, 1660, 1692, 1725, 1759, 1793, 1828,
];

/// 64 × 64 superblock columns and rows.
fn superblocks(width: u16, height: u16) -> (u32, u32) {
    (
        u32::from(width).div_ceil(64),
        u32::from(height).div_ceil(64),
    )
}

fn significant_bits(value: u16) -> u32 {
    (u16::BITS - value.leading_zeros()).max(1)
}

/// The lowest level and tier that hold the sequence (AV1 Annex A.3).
fn level(width: u16, height: u16, fps: u16, bitrate: u32) -> (u8, bool) {
    // seq_level_idx, MaxPicSize, MaxHSize, MaxVSize, MaxDisplayRate, MainMbps, HighMbps
    const LEVELS: [(u8, u64, u16, u16, u64, u32, u32); 10] = [
        (0, 147_456, 2048, 1152, 4_423_680, 1, 0),
        (1, 278_784, 2816, 1584, 8_363_520, 3, 0),
        (4, 665_856, 4352, 2448, 19_975_680, 6, 0),
        (5, 1_065_024, 5504, 3096, 31_950_720, 10, 0),
        (8, 2_359_296, 6144, 3456, 70_778_880, 12, 30),
        (9, 2_359_296, 6144, 3456, 141_557_760, 20, 50),
        (12, 8_912_896, 8192, 4352, 267_386_880, 30, 100),
        (13, 8_912_896, 8192, 4352, 534_773_760, 40, 160),
        (14, 8_912_896, 8192, 4352, 1_069_547_520, 60, 240),
        (16, 35_651_584, 16384, 8704, 1_069_547_520, 60, 240),
    ];
    let picture = u64::from(width) * u64::from(height);
    LEVELS
        .iter()
        .find(|&&(_, size, max_width, max_height, rate, main, high)| {
            picture <= size
                && width <= max_width
                && height <= max_height
                && picture * u64::from(fps) <= rate
                && bitrate <= main.max(high) * 1_000_000
        })
        .map_or((31, false), |&(index, _, _, _, _, main, _)| {
            (index, bitrate > main * 1_000_000)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_size_and_rate() {
        assert_eq!(level(1280, 720, 30, 0), (5, false));
        assert_eq!(level(1920, 1080, 30, 0), (8, false));
        assert_eq!(level(1920, 1080, 60, 0), (9, false));
        assert_eq!(level(3440, 1440, 30, 0), (12, false));
        assert_eq!(level(3440, 1440, 60, 0), (13, false));
        assert_eq!(level(3440, 1440, 100, 50_000_000), (13, true));
        assert_eq!(level(3840, 2160, 60, 0), (13, false));
        assert_eq!(level(3840, 2160, 100, 100_000_000), (14, true));
        assert_eq!(level(3840, 2160, 120, 100_000_000), (14, true));
    }

    #[test]
    fn high_rate_sequence_stays_stable_when_the_budget_falls() {
        let mut sequence = Sequence::new(3840, 2160, 30, 3, true).unwrap();
        assert!(sequence.expand(120, 100_000_000, Some(4)));
        assert_eq!(sequence.level, 14);
        assert!(sequence.high_tier);
        assert_eq!(sequence.tile_rows(), Some([17, 17]));
        let high = sequence.obu();
        assert!(!sequence.expand(30, 4_000_000, None));
        assert_eq!(sequence.obu(), high);
        assert_eq!(sequence.tile_rows(), Some([17, 17]));
    }

    #[test]
    fn a_frame_fits_one_tile() {
        assert!(Sequence::new(3840, 2160, 60, 3, true).is_some());
        assert!(Sequence::new(4096, 2304, 30, 1, false).is_some());
        assert!(Sequence::new(4160, 1080, 30, 1, false).is_none());
        assert!(Sequence::new(4096, 2368, 30, 1, false).is_none());
        assert!(Sequence::new(64, 64, 30, 4, false).is_none());
    }

    #[test]
    fn loop_filter_rises_with_the_quantizer() {
        assert_eq!(loop_filter(1, true), [0; 4]);
        let low = loop_filter(60, false)[0];
        let high = loop_filter(200, false)[0];
        assert!(low < high && high <= 63, "{low} {high}");
        assert_eq!(ac_quantizer(255), 1828);
    }
}
