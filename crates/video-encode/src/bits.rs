// SPDX-License-Identifier: EUPL-1.2
//! Bit writing for AV1 headers and the Dependency Descriptor, most significant bit
//! first, with the AV1 specification's descriptor names (§4.10).

#[derive(Default)]
pub struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Bits {
    /// `f(n)`: the low `n` bits of `value`.
    pub fn put(&mut self, value: u32, n: u32) {
        for bit in (0..n).rev() {
            if self.len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if value >> bit & 1 == 1
                && let Some(last) = self.bytes.last_mut()
            {
                *last |= 0x80 >> (self.len % 8);
            }
            self.len += 1;
        }
    }

    pub fn flag(&mut self, value: bool) {
        self.put(u32::from(value), 1);
    }

    /// `ns(n)`: `value` below `n` in as few bits as its range allows.
    pub fn ns(&mut self, value: u32, n: u32) {
        let width = u32::BITS - n.leading_zeros();
        let m = (1 << width) - n;
        if value < m {
            self.put(value, width - 1);
        } else {
            self.put((value + m) >> 1, width - 1);
            self.put((value + m) & 1, 1);
        }
    }

    /// `trailing_bits()`: a one, then zeros to the byte boundary.
    pub fn trailing(&mut self) {
        self.flag(true);
        self.align();
    }

    /// `byte_alignment()`: zeros to the byte boundary.
    pub fn align(&mut self) {
        self.len = self.bytes.len() * 8;
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_FRAME: u8 = 6;

/// An OBU with its size field and, in a layered stream, its temporal layer
/// (§5.3).
pub fn obu(kind: u8, temporal: Option<u8>, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(kind << 3 | u8::from(temporal.is_some()) << 2 | 1 << 1);
    if let Some(temporal) = temporal {
        out.push(temporal << 5);
    }
    let mut size = payload.len();
    loop {
        let byte = (size & 0x7f) as u8;
        size >>= 7;
        out.push(if size == 0 { byte } else { byte | 0x80 });
        if size == 0 {
            break;
        }
    }
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_msb_first_and_aligns() {
        let mut bits = Bits::default();
        bits.put(0b101, 3);
        bits.put(0x1ff, 9);
        bits.trailing();
        assert_eq!(bits.into_bytes(), [0b1011_1111, 0b1111_1000]);
    }

    #[test]
    fn ns_matches_the_specification() {
        // ns(5): three values in two bits, two in three bits.
        let written: Vec<_> = (0..5)
            .map(|value| {
                let mut bits = Bits::default();
                bits.ns(value, 5);
                bits.len
            })
            .collect();
        assert_eq!(written, [2, 2, 2, 3, 3]);
        let mut bits = Bits::default();
        bits.ns(0, 1);
        assert_eq!(bits.len, 0);
    }

    #[test]
    fn obu_sizes_use_leb128() {
        let payload = vec![0; 200];
        let out = obu(OBU_FRAME, Some(2), &payload);
        assert_eq!(&out[..4], [0x36, 0x40, 0xc8, 0x01]);
        assert_eq!(out.len(), 204);
        assert_eq!(obu(OBU_TEMPORAL_DELIMITER, None, &[]), [0x12, 0x00]);
    }
}
