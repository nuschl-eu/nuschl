// SPDX-License-Identifier: EUPL-1.2
//! The reference structure: which slot each frame predicts from and replaces,
//! and its Dependency Descriptor template. Keyframes come only on request and
//! restart the pattern. A dropped frame leaves the pattern where it was, so the
//! next frame takes its place.
//!
//! L1T3 follows libwebrtc's pattern T0 T2 T1 T2: T0 predicts from the previous
//! T0, T1 from T0, and each T2 from the frame before it. Slot 0 holds the newest
//! T0 and slot 1 the newest T1; T2 frames replace nothing.

/// Temporal scalability modes for one spatial layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layering {
    L1T1,
    L1T2,
    L1T3,
}

/// Decode target indications (Dependency Descriptor table A.1).
pub const NOT_PRESENT: u8 = 0;
pub const DISCARDABLE: u8 = 1;
pub const SWITCH: u8 = 2;

/// A frame dependency template with one reference and one chain over T0.
pub struct Template {
    pub temporal: u8,
    /// One indication per decode target; target `t` holds temporal layers 0 to `t`.
    pub dtis: &'static [u8],
    /// The frame number distance to the referenced frame; 0 for none.
    pub fdiff: u8,
    /// The distance to the previous T0 frame.
    pub chain: u8,
}

const fn template(temporal: u8, dtis: &'static [u8], fdiff: u8, chain: u8) -> Template {
    Template {
        temporal,
        dtis,
        fdiff,
        chain,
    }
}

/// One frame of the repeating pattern.
struct Step {
    template: u8,
    reference: u8,
    refresh: u8,
}

const fn step(template: u8, reference: u8, refresh: u8) -> Step {
    Step {
        template,
        reference,
        refresh,
    }
}

const S: u8 = SWITCH;
const D: u8 = DISCARDABLE;
const N: u8 = NOT_PRESENT;

// Template 0 is the keyframe's in every mode. Templates are listed by temporal
// layer, as the descriptor codes them.
const L1T1: [Template; 2] = [template(0, &[S], 0, 0), template(0, &[S], 1, 1)];
const L1T2: [Template; 3] = [
    template(0, &[S, S], 0, 0),
    template(0, &[S, S], 2, 2),
    template(1, &[N, D], 1, 1),
];
const L1T3: [Template; 5] = [
    template(0, &[S, S, S], 0, 0),
    template(0, &[S, S, S], 4, 4),
    template(1, &[N, D, S], 2, 2),
    template(2, &[N, N, D], 1, 1),
    template(2, &[N, N, D], 1, 3),
];
const L1T1_STEPS: [Step; 1] = [step(1, 0, 0b01)];
const L1T2_STEPS: [Step; 2] = [step(1, 0, 0b01), step(2, 0, 0)];
const L1T3_STEPS: [Step; 4] = [
    step(1, 0, 0b01),
    step(3, 0, 0),
    step(2, 0, 0b10),
    step(4, 1, 0),
];

impl Layering {
    pub fn layers(self) -> u8 {
        match self {
            Self::L1T1 => 1,
            Self::L1T2 => 2,
            Self::L1T3 => 3,
        }
    }

    pub fn templates(self) -> &'static [Template] {
        match self {
            Self::L1T1 => &L1T1,
            Self::L1T2 => &L1T2,
            Self::L1T3 => &L1T3,
        }
    }

    fn steps(self) -> &'static [Step] {
        match self {
            Self::L1T1 => &L1T1_STEPS,
            Self::L1T2 => &L1T2_STEPS,
            Self::L1T3 => &L1T3_STEPS,
        }
    }

    /// Each layer's share of a frame's bits relative to the average frame:
    /// libwebrtc's split of 40, 20 and 40 % of the rate over T0, T1 and T2 (60
    /// and 40 % over two layers).
    pub fn weight(self, temporal: u8) -> f64 {
        match (self, temporal) {
            (Self::L1T2, 0) => 1.2,
            (Self::L1T2, _) => 0.8,
            (Self::L1T3, 0) => 1.6,
            (Self::L1T3, _) => 0.8,
            (Self::L1T1, _) => 1.0,
        }
    }
}

/// The next frame's place in the structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub key: bool,
    pub template: u8,
    pub temporal: u8,
    pub reference: u8,
    pub refresh: u8,
}

/// The pattern position and which frame each of the eight slots holds.
pub struct Pattern {
    layering: Layering,
    next: usize,
    pub slots: [Option<u64>; 8],
}

impl Pattern {
    pub fn new(layering: Layering) -> Self {
        Self {
            layering,
            next: 0,
            slots: [None; 8],
        }
    }

    /// The next frame, a keyframe when `key` or before the first keyframe.
    pub fn plan(&self, key: bool) -> Plan {
        if key || self.slots[0].is_none() {
            return Plan {
                key: true,
                template: 0,
                temporal: 0,
                reference: 0,
                refresh: 0xff,
            };
        }
        let steps = self.layering.steps();
        let step = &steps[self.next % steps.len()];
        Plan {
            key: false,
            template: step.template,
            temporal: self.layering.templates()[usize::from(step.template)].temporal,
            reference: step.reference,
            refresh: step.refresh,
        }
    }

    /// Records frame `id`, sent as `plan` describes.
    pub fn commit(&mut self, plan: Plan, id: u64) {
        for (slot, held) in self.slots.iter_mut().enumerate() {
            if plan.refresh >> slot & 1 == 1 {
                *held = Some(id);
            }
        }
        self.next = if plan.key { 1 } else { self.next + 1 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `count` frames with a keyframe first and returns, per frame, its
    /// temporal layer and the frame it references.
    fn run(layering: Layering, count: u64) -> Vec<(u8, Option<u64>)> {
        let mut pattern = Pattern::new(layering);
        (0..count)
            .map(|id| {
                let plan = pattern.plan(false);
                let reference = (!plan.key)
                    .then(|| pattern.slots[usize::from(plan.reference)])
                    .flatten();
                pattern.commit(plan, id);
                (plan.temporal, reference)
            })
            .collect()
    }

    #[test]
    fn l1t3_matches_its_templates() {
        let frames = run(Layering::L1T3, 9);
        let layers: Vec<_> = frames.iter().map(|f| f.0).collect();
        assert_eq!(layers, [0, 2, 1, 2, 0, 2, 1, 2, 0]);
        let references: Vec<_> = frames.iter().map(|f| f.1).collect();
        assert_eq!(
            references,
            [
                None,
                Some(0),
                Some(0),
                Some(2),
                Some(0),
                Some(4),
                Some(4),
                Some(6),
                Some(4)
            ]
        );
        // Each frame's template distance names the frame it predicts from.
        let mut pattern = Pattern::new(Layering::L1T3);
        for id in 0..9 {
            let plan = pattern.plan(false);
            let template = &Layering::L1T3.templates()[usize::from(plan.template)];
            assert_eq!(template.temporal, plan.temporal);
            if let Some(reference) = frames[id as usize].1 {
                assert_eq!(id - reference, u64::from(template.fdiff));
            }
            pattern.commit(plan, id);
        }
    }

    #[test]
    fn every_layer_predicts_only_from_its_own_or_lower_layers() {
        for layering in [Layering::L1T1, Layering::L1T2, Layering::L1T3] {
            let frames = run(layering, 40);
            for (layer, reference) in &frames {
                if let Some(reference) = reference {
                    assert!(frames[*reference as usize].0 <= *layer, "{layering:?}");
                }
            }
            // The weights keep the average frame at one share.
            let total: f64 = frames.iter().map(|f| layering.weight(f.0)).sum();
            assert!((total / 40.0 - 1.0).abs() < 0.05, "{layering:?} {total}");
        }
    }

    #[test]
    fn a_drop_repeats_the_step_and_a_keyframe_restarts() {
        let mut pattern = Pattern::new(Layering::L1T3);
        let key = pattern.plan(false);
        pattern.commit(key, 0);
        let first = pattern.plan(false);
        assert_eq!(first.temporal, 2);
        // Not committed: the next frame takes the same step.
        assert_eq!(pattern.plan(false), first);
        pattern.commit(first, 1);
        assert_eq!(pattern.plan(false).temporal, 1);
        let again = pattern.plan(true);
        assert!(again.key);
        pattern.commit(again, 2);
        assert_eq!(pattern.slots, [Some(2); 8]);
        assert_eq!(pattern.plan(false).temporal, 2);
    }
}
