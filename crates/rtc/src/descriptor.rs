// SPDX-License-Identifier: EUPL-1.2
//! The AV1 Dependency Descriptor header extension on sent video. The encoder
//! writes one descriptor per frame with start and end of frame set; each
//! packet keeps start of frame only on the frame's first packet and end of
//! frame only on its last, as the SFU's layer selection reads them.

use std::sync::Arc;
use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues, PacketPosition};

/// The header extension ID, the one the SFU maps the descriptor to.
pub const ID: u8 = 12;

/// One frame's descriptor.
#[derive(Clone, Debug)]
pub struct Descriptor(pub Arc<[u8]>);

#[derive(Debug)]
struct Serializer;

impl ExtensionSerializer for Serializer {
    fn write_to(&self, buf: &mut [u8], ev: &ExtensionValues) -> usize {
        let Some(Descriptor(bytes)) = ev.user_values.get::<Descriptor>() else {
            return 0;
        };
        let Some(out) = buf.get_mut(..bytes.len()) else {
            return 0;
        };
        out.copy_from_slice(bytes);
        if let (Some(first), Some(position)) =
            (out.first_mut(), ev.user_values.get::<PacketPosition>())
        {
            if !position.first {
                *first &= !0x80;
            }
            if !position.last {
                *first &= !0x40;
            }
        }
        bytes.len()
    }

    fn parse_value(&self, _: &[u8], _: &mut ExtensionValues) -> bool {
        true
    }

    fn is_video(&self) -> bool {
        true
    }

    fn is_audio(&self) -> bool {
        false
    }

    /// A descriptor with its template structure is longer than 16 bytes.
    fn requires_two_byte_form(&self, ev: &ExtensionValues) -> bool {
        ev.user_values
            .get::<Descriptor>()
            .is_some_and(|descriptor| descriptor.0.len() > 16)
    }
}

pub fn extension() -> Extension {
    Extension::with_serializer(nuschl_stream_quality::dd::URI, Serializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_packet_marks_its_place_in_the_frame() {
        let mut values = ExtensionValues::default();
        values
            .user_values
            .set(Descriptor(Arc::from(&[0xc1, 0x02, 0x03][..])));
        let mut written = Vec::new();
        for (first, last) in [(true, false), (false, false), (false, true)] {
            values.user_values.set(PacketPosition { first, last });
            let mut buf = [0; 16];
            assert_eq!(Serializer.write_to(&mut buf, &values), 3);
            written.push(buf[0]);
        }
        assert_eq!(written, [0x81, 0x01, 0x41]);
        let mut short = [0; 2];
        assert_eq!(Serializer.write_to(&mut short, &values), 0);
    }
}
