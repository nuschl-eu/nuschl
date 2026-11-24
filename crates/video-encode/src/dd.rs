// SPDX-License-Identifier: EUPL-1.2
//! The AV1 Dependency Descriptor RTP header extension (AV1 RTP specification,
//! appendix A) for the reference structure in [`crate::layers`]. Keyframes carry
//! the template structure with the picture size; other frames name their template
//! in the three mandatory bytes.

use crate::bits::Bits;
use crate::layers::Layering;

/// The descriptor of a frame sent in one packet: start and end of frame set. A
/// packetizer clears bit 7 of byte 0 on all packets but the first and bit 6 on
/// all but the last.
pub fn descriptor(
    layering: Layering,
    template: u8,
    frame_number: u16,
    structure: Option<(u16, u16)>,
) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.flag(true); // start_of_frame
    bits.flag(true); // end_of_frame
    bits.put(u32::from(template), 6); // frame_dependency_template_id, offset 0
    bits.put(u32::from(frame_number), 16);
    let Some((width, height)) = structure else {
        return bits.into_bytes();
    };
    bits.flag(true); // template_dependency_structure_present_flag
    // active decode targets, custom DTIs, custom frame diffs, custom chains
    bits.put(0, 4);
    let templates = layering.templates();
    let targets = templates[0].dtis.len() as u32;
    bits.put(0, 6); // template_id_offset
    bits.put(targets - 1, 5); // dt_cnt_minus_one
    for pair in templates.windows(2) {
        // next_layer_idc: the same layer, or the next temporal layer
        bits.put(u32::from(pair[1].temporal > pair[0].temporal), 2);
    }
    bits.put(3, 2); // no more templates
    for template in templates {
        for &dti in template.dtis {
            bits.put(u32::from(dti), 2);
        }
    }
    for template in templates {
        if template.fdiff > 0 {
            bits.flag(true); // fdiff_follows_flag
            bits.put(u32::from(template.fdiff - 1), 4);
        }
        bits.flag(false);
    }
    bits.ns(1, targets + 1); // chain_cnt: one chain over T0
    for _ in 0..targets {
        bits.ns(0, 1); // decode_target_protected_by: that chain
    }
    for template in templates {
        bits.put(u32::from(template.chain), 4);
    }
    bits.flag(true); // resolutions_present_flag
    bits.put(u32::from(width.max(1) - 1), 16);
    bits.put(u32::from(height.max(1) - 1), 16);
    bits.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::Pattern;
    use nuschl_stream_quality::{
        Kind,
        dd::{Dti, Layers},
    };
    use std::time::Duration;

    /// The descriptors of `count` frames with a keyframe first.
    fn stream(layering: Layering, count: u16) -> Vec<(u8, Vec<u8>)> {
        let mut pattern = Pattern::new(layering);
        (0..count)
            .map(|number| {
                let plan = pattern.plan(false);
                let structure = plan.key.then_some((3440, 1440));
                let bytes = descriptor(layering, plan.template, number, structure);
                pattern.commit(plan, u64::from(number));
                (plan.temporal, bytes)
            })
            .collect()
    }

    #[test]
    fn the_sfu_parser_reads_each_mode() {
        for layering in [Layering::L1T1, Layering::L1T2, Layering::L1T3] {
            let mut layers = Layers::default();
            let mut previous_t0 = None;
            for (number, (temporal, bytes)) in (0u16..).zip(stream(layering, 24)) {
                assert_eq!(bytes.len() > 3, number == 0, "{layering:?}");
                let frame = layers
                    .packet(&bytes, 1000, Duration::ZERO)
                    .expect("valid descriptor");
                assert!(frame.start && frame.end);
                assert_eq!(frame.number, number);
                assert_eq!(frame.layer.temporal, temporal);
                assert_eq!(frame.layer.spatial, 0);
                for target in 0..usize::from(layering.layers()) {
                    // Target t holds temporal layers 0 to t.
                    assert_eq!(
                        frame.dti(target) != Dti::NotPresent,
                        usize::from(temporal) <= target,
                    );
                    let diff = previous_t0.map_or(0, |t0| number - t0);
                    assert_eq!(frame.chain_diff(target), Some(diff as u8));
                }
                if temporal == 0 {
                    previous_t0 = Some(number);
                }
            }
            assert!(layers.layered());
            assert_eq!(layers.picture_tier(Kind::Screen), Some(2));
        }
    }

    #[test]
    fn references_match_the_reference_structure() {
        let mut layers = Layers::default();
        let mut pattern = Pattern::new(Layering::L1T3);
        for number in 0..12u16 {
            let plan = pattern.plan(false);
            let bytes = descriptor(
                Layering::L1T3,
                plan.template,
                number,
                plan.key.then_some((640, 360)),
            );
            let frame = layers.packet(&bytes, 100, Duration::ZERO).expect("valid");
            let expected = (!plan.key)
                .then(|| pattern.slots[usize::from(plan.reference)])
                .flatten()
                .map(|id| id as u16);
            assert_eq!(frame.references().next(), expected);
            assert_eq!(frame.references().count(), usize::from(!plan.key));
            pattern.commit(plan, u64::from(number));
        }
    }
}
