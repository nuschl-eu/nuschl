// SPDX-License-Identifier: EUPL-1.2
//! The AV1 Dependency Descriptor RTP header extension (AV1 RTP specification,
//! appendix A) and per-receiver layer choice. [`Layers`] keeps a source's template
//! structure and measures the rate of each decode target; a [`Selector`] per
//! receiver forwards the frames of the decode target it was given and switches
//! targets only at switch points the receiver can decode from: a frame with a
//! switch indication whose chain the receiver holds (§A.4.2).

use crate::{Kind, adapt::Stepper};
use std::{collections::VecDeque, time::Duration};

pub const URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";

/// Templates per structure; the template id has six bits.
const MAX_TEMPLATES: usize = 64;
/// Decode targets per structure, and so chains; the count has five bits.
const MAX_TARGETS: usize = 32;
/// Frame numbers a receiver's [`Selector`] remembers; a chain's frame diff has
/// eight bits.
const HISTORY: usize = 256;
/// Frame diffs kept per frame; libwebrtc decodes frames with at most five.
const MAX_FDIFFS: usize = 8;
/// The window over which [`Layers`] measures rates.
const WINDOW: Duration = Duration::from_secs(1);
/// The RTP clock of video, ticks per second.
const CLOCK: u64 = 90_000;
/// Temporal units in a frame-rate window: active when they span at most a
/// second of media time.
const RATE_UNITS: usize = 8;
/// A source's frame rate until an active window measures it: the highest any
/// tier asks for.
const MAX_FPS: u64 = 60;

/// A frame's decode target indication (table A.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dti {
    NotPresent,
    Discardable,
    Switch,
    Required,
}

/// A decode target's layers: its spatial id and highest temporal id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub spatial: u8,
    pub temporal: u8,
}

/// Two bits per decode target, target 0 lowest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Dtis(u64);

impl Dtis {
    fn get(self, target: usize) -> Dti {
        match self.0.checked_shr(2 * target as u32).unwrap_or(0) & 3 {
            0 => Dti::NotPresent,
            1 => Dti::Discardable,
            2 => Dti::Switch,
            _ => Dti::Required,
        }
    }

    fn read(bits: &mut Bits, count: usize) -> Option<Self> {
        let mut dtis = 0;
        for target in 0..count {
            dtis |= u64::from(bits.read(2)?) << (2 * target);
        }
        Some(Self(dtis))
    }
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl Bits<'_> {
    /// `f(n)`, n ≤ 32.
    fn read(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data.get(self.position / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.read(1)? == 1)
    }

    /// `ns(n)`, a value below n.
    fn ns(&mut self, n: u32) -> Option<u32> {
        let width = u32::BITS - n.leading_zeros();
        let m = (1 << width) - n;
        let value = self.read(width - 1)?;
        if value < m {
            return Some(value);
        }
        Some((value << 1) - m + self.read(1)?)
    }
}

struct Template {
    layer: Target,
    dtis: Dtis,
    fdiffs: Fdiffs,
    /// Per chain, the frame diff to the previous frame of the chain.
    chains: [u8; MAX_TARGETS],
}

/// The frame diffs to the frames a frame references, the first [`MAX_FDIFFS`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fdiffs {
    diffs: [u16; MAX_FDIFFS],
    count: u8,
}

impl Fdiffs {
    fn push(&mut self, diff: u32) {
        if let Some(slot) = self.diffs.get_mut(usize::from(self.count)) {
            *slot = diff as u16;
            self.count += 1;
        }
    }
}

/// A template dependency structure, sent with key frames.
struct Structure {
    offset: u32,
    templates: Vec<Template>,
    targets: Vec<Target>,
    chains: usize,
    /// The chain protecting each decode target; empty without chains.
    protected_by: Vec<u8>,
    /// The shorter side of each spatial layer, when the publisher sent it.
    heights: Vec<u32>,
    active: u32,
}

impl Structure {
    fn read(bits: &mut Bits) -> Option<Self> {
        let offset = bits.read(6)?;
        let count = bits.read(5)? as usize + 1;
        let mut templates = Vec::new();
        let mut layer = Target::default();
        loop {
            if templates.len() == MAX_TEMPLATES {
                return None;
            }
            templates.push(Template {
                layer,
                dtis: Dtis::default(),
                fdiffs: Fdiffs::default(),
                chains: [0; MAX_TARGETS],
            });
            match bits.read(2)? {
                0 => {}
                1 => layer.temporal += 1,
                2 => {
                    layer = Target {
                        spatial: layer.spatial + 1,
                        temporal: 0,
                    }
                }
                _ => break,
            }
        }
        for template in &mut templates {
            template.dtis = Dtis::read(bits, count)?;
        }
        for template in &mut templates {
            while bits.flag()? {
                template.fdiffs.push(bits.read(4)? + 1);
            }
        }
        let chains = bits.ns(count as u32 + 1)?;
        let mut protected_by = Vec::new();
        if chains > 0 {
            for _ in 0..count {
                protected_by.push(bits.ns(chains)? as u8);
            }
            for template in &mut templates {
                for diff in &mut template.chains[..chains as usize] {
                    *diff = bits.read(4)? as u8;
                }
            }
        }
        let targets = (0..count)
            .map(|target| {
                let mut layers = templates
                    .iter()
                    .filter(|t| t.dtis.get(target) != Dti::NotPresent)
                    .map(|t| t.layer);
                let first = layers.next().unwrap_or_default();
                layers.fold(first, |a, b| Target {
                    spatial: a.spatial.max(b.spatial),
                    temporal: a.temporal.max(b.temporal),
                })
            })
            .collect();
        let mut heights = Vec::new();
        if bits.flag()? {
            for _ in 0..=layer.spatial {
                let width = bits.read(16)? + 1;
                heights.push(width.min(bits.read(16)? + 1));
            }
        }
        Some(Self {
            offset,
            templates,
            targets,
            chains: chains as usize,
            protected_by,
            heights,
            active: u32::MAX >> (32 - count),
        })
    }
}

/// One packet's descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub start: bool,
    pub end: bool,
    pub number: u16,
    pub layer: Target,
    /// Whether the packet came after a packet of a later frame, or repeats
    /// its frame's start.
    pub late: bool,
    dtis: Dtis,
    fdiffs: Fdiffs,
    /// Per decode target, the frame diff to the previous frame of the chain
    /// protecting it; `None` for a structure without chains.
    chains: Option<[u8; MAX_TARGETS]>,
}

impl Frame {
    pub fn dti(&self, target: usize) -> Dti {
        self.dtis.get(target)
    }

    /// The frame numbers of the frames this one references.
    pub fn references(&self) -> impl Iterator<Item = u16> + '_ {
        let fdiffs = &self.fdiffs.diffs[..usize::from(self.fdiffs.count)];
        fdiffs.iter().map(|&d| self.number.wrapping_sub(d))
    }

    /// The frame diff to the previous frame of the chain protecting `target`.
    pub fn chain_diff(&self, target: usize) -> Option<u8> {
        Some(*self.chains?.get(target)?)
    }
}

/// A decode target a receiver can be given, from [`Layers::choices`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The decode target index.
    pub target: usize,
    pub layer: Target,
    /// The measured rate of the target, its lower layers included.
    pub rate: u64,
}

impl Choice {
    /// What the choice keeps in its source's mode: its spatial layer for a
    /// Sharp source, its temporal layer for a `smooth` one.
    pub fn kept(&self, smooth: bool) -> u8 {
        if smooth {
            self.layer.temporal
        } else {
            self.layer.spatial
        }
    }
}

/// One source's descriptor state, the measured rate of each of its decode
/// targets over the last second, and its frame rate (docs/video-plan.md §5.3,
/// layer selection).
#[derive(Default)]
pub struct Layers {
    structure: Option<Structure>,
    /// When the current rate window started: at the source's first frame,
    /// then once a second.
    window: Option<Duration>,
    /// Whether a full window has measured the rates.
    measured: bool,
    /// When the latest packet arrived, and whether the current window holds
    /// a pause of over a second.
    last: Option<Duration>,
    paused: bool,
    bytes: [u64; 32],
    rates: [u64; 32],
    /// The RTP timestamps of the latest temporal units, oldest first.
    units: VecDeque<u64>,
    /// The median temporal-unit interval of the latest active window, in ticks.
    interval: Option<u64>,
    /// The median interval of the latest slow window, until an active one.
    slow: Option<u64>,
    /// The newest frame a packet came for, and whether its start came.
    newest: Option<(u16, bool)>,
}

impl Layers {
    /// Reads one packet's descriptor and counts its `size` bytes toward the
    /// decode targets the frame belongs to; `timestamp` is the packet's
    /// extended RTP timestamp. `None` for a malformed descriptor or one that
    /// needs a structure this source has not sent.
    pub fn packet(
        &mut self,
        descriptor: &[u8],
        size: usize,
        timestamp: u64,
        now: Duration,
    ) -> Option<Frame> {
        let mut frame = self.parse(descriptor)?;
        self.paused |= self
            .last
            .is_some_and(|last| now.saturating_sub(last) > WINDOW);
        self.last = Some(now);
        let elapsed = now.saturating_sub(*self.window.get_or_insert(now));
        if elapsed >= WINDOW {
            // A window across a pause keeps the rates it has.
            if !self.paused || !self.measured {
                let millis = elapsed.as_millis() as u64;
                for target in 0..32 {
                    self.rates[target] = self.bytes[target] * 8000 / millis;
                }
                self.measured = true;
            }
            self.bytes = [0; 32];
            self.window = Some(now);
            self.paused = false;
        }
        for target in 0..32 {
            if frame.dti(target) != Dti::NotPresent {
                self.bytes[target] += size as u64;
            }
        }
        // A jump back of over a second, as from a new stream on the same
        // source, starts the frame rate's units over; a late packet of an
        // earlier temporal unit leaves them.
        if self
            .units
            .back()
            .is_some_and(|&last| timestamp + CLOCK < last)
        {
            self.units.clear();
        }
        // A jump back of more than the history in frame numbers, as from a new
        // stream, starts them over.
        frame.late = self.newest.is_some_and(|(newest, started)| {
            let behind = newest.wrapping_sub(frame.number);
            (1..HISTORY as u16).contains(&behind) || behind == 0 && started && frame.start
        });
        if !frame.late {
            let started = self.newest == Some((frame.number, true));
            self.newest = Some((frame.number, frame.start || started));
        }
        if frame.start && self.units.back().is_none_or(|&last| timestamp > last) {
            self.units.push_back(timestamp);
            if self.units.len() > RATE_UNITS + 1 {
                self.units.pop_front();
            }
            if self.units.len() > RATE_UNITS {
                let mut intervals: Vec<u64> = (1..self.units.len())
                    .map(|i| self.units[i] - self.units[i - 1])
                    .collect();
                // The shorter median: a pause inside the window leaves it.
                intervals.sort_unstable();
                let median = Some(intervals[RATE_UNITS / 2 - 1]);
                if timestamp - self.units[0] <= CLOCK {
                    self.interval = median;
                } else if self.interval.is_none() {
                    self.slow = median;
                }
            }
        }
        Some(frame)
    }

    fn parse(&mut self, descriptor: &[u8]) -> Option<Frame> {
        let bits = &mut Bits {
            data: descriptor,
            position: 0,
        };
        let start = bits.flag()?;
        let end = bits.flag()?;
        let template_id = bits.read(6)?;
        let number = bits.read(16)? as u16;
        let (mut new, mut active) = (None, None);
        let (mut custom_dtis, mut custom_fdiffs, mut custom_chains) = (false, false, false);
        if descriptor.len() > 3 {
            let has_structure = bits.flag()?;
            let has_active = bits.flag()?;
            custom_dtis = bits.flag()?;
            custom_fdiffs = bits.flag()?;
            custom_chains = bits.flag()?;
            if has_structure {
                new = Some(Structure::read(bits)?);
            }
            if has_active {
                let structure = new.as_ref().or(self.structure.as_ref())?;
                active = Some(bits.read(structure.targets.len() as u32)?);
            }
        }
        let structure = new.as_ref().or(self.structure.as_ref())?;
        let template = structure
            .templates
            .get(((template_id + 64 - structure.offset) % 64) as usize)?;
        let dtis = if custom_dtis {
            Dtis::read(bits, structure.targets.len())?
        } else {
            template.dtis
        };
        let mut fdiffs = template.fdiffs;
        if custom_fdiffs {
            fdiffs = Fdiffs::default();
            loop {
                match bits.read(2)? {
                    0 => break,
                    size => fdiffs.push(bits.read(4 * size)? + 1),
                }
            }
        }
        let mut diffs = template.chains;
        if custom_chains {
            for diff in &mut diffs[..structure.chains] {
                *diff = bits.read(8)? as u8;
            }
        }
        let chains = (!structure.protected_by.is_empty()).then(|| {
            let mut targets = [0; MAX_TARGETS];
            for (target, &chain) in targets.iter_mut().zip(&structure.protected_by) {
                *target = diffs[usize::from(chain)];
            }
            targets
        });
        let frame = Frame {
            start,
            end,
            number,
            layer: template.layer,
            late: false,
            dtis,
            fdiffs,
            chains,
        };
        if let Some(new) = new {
            // Rates measured for other targets are stale.
            if self
                .structure
                .as_ref()
                .is_none_or(|old| old.targets != new.targets)
            {
                self.rates = [0; 32];
                self.bytes = [0; 32];
                self.measured = false;
                self.window = None;
            }
            self.structure = Some(new);
        }
        if let (Some(structure), Some(active)) = (&mut self.structure, active) {
            structure.active = active;
        }
        Some(frame)
    }

    /// Whether the source has sent a template structure.
    pub fn layered(&self) -> bool {
        self.structure.is_some()
    }

    /// Whether a full second of the source has measured its rates.
    pub fn measured(&self) -> bool {
        self.measured
    }

    /// How many layer pairs the source's active targets have.
    pub fn targets(&self) -> usize {
        let Some(structure) = &self.structure else {
            return 0;
        };
        let mut layers: Vec<Target> = (0..structure.targets.len())
            .filter(|t| structure.active >> t & 1 == 1)
            .map(|t| structure.targets[t])
            .collect();
        layers.sort_by_key(|l| (l.spatial, l.temporal));
        layers.dedup();
        layers.len()
    }

    /// The active target with `target`'s spatial layer and its lowest
    /// temporal layer.
    pub fn base(&self, target: usize) -> Option<Wanted> {
        let structure = self.structure.as_ref()?;
        let spatial = structure.targets.get(target)?.spatial;
        (0..structure.targets.len())
            .filter(|&t| structure.active >> t & 1 == 1 && structure.targets[t].spatial == spatial)
            .min_by_key(|&t| structure.targets[t].temporal)
            .map(|t| (t, spatial))
    }

    /// Whether `layer`'s frame rate is at most a quarter above `fps`. The
    /// source's frame rate halves with each temporal layer below the top.
    fn within(&self, layer: Target, top: u8, fps: u64) -> bool {
        let interval = self.interval.or(self.slow).unwrap_or(CLOCK / MAX_FPS);
        (4 * CLOCK) << layer.temporal <= (5 * fps * interval) << top
    }

    /// The tier of `kind`'s ladder that the source's picture belongs to, when the
    /// source sends one spatial layer and its size: every receiver gets that
    /// picture.
    pub fn picture_tier(&self, kind: Kind) -> Option<usize> {
        match self.structure.as_ref()?.heights[..] {
            [height] => Some(kind.tier_of(height)),
            _ => None,
        }
    }

    /// The active decode targets that fit tier `tier` of `kind`'s ladder, one per
    /// layer pair, lowest first; the lowest active target when none fits. A target
    /// fits when its spatial layer belongs to the tier or a lower one (when the
    /// publisher sent layer sizes) and its frame rate is at most a quarter above
    /// the tier's. These are the steps of [`crate::budget::Stream`] and
    /// [`crate::adapt::Stepper`]. Until a full second measures the rates, the
    /// lowest fitting target is the only choice.
    ///
    /// A Sharp source's targets go by spatial, then temporal layer: a step down
    /// drops temporal layers first. A `smooth` source's lowest spatial layer
    /// gains temporal layers first, then its full frame rate gains spatial
    /// layers: a step down drops spatial layers first. Each of `steps` removes
    /// the top choice, down to the lowest one of the top group ([`Choice::kept`]).
    pub fn choices(&self, kind: Kind, tier: usize, smooth: bool, steps: usize) -> Vec<Choice> {
        let Some(structure) = &self.structure else {
            return Vec::new();
        };
        let tier_fps = u64::from(kind.ladder()[tier.min(kind.top())].fps);
        // The temporal layers sent: those of the active targets.
        let active = |target: &usize| structure.active >> target & 1 == 1;
        let top = (0..structure.targets.len())
            .filter(active)
            .map(|t| structure.targets[t].temporal)
            .max();
        let mut all: Vec<_> = structure
            .targets
            .iter()
            .enumerate()
            .filter(|(target, _)| structure.active >> target & 1 == 1)
            .map(|(target, &layer)| Choice {
                target,
                layer,
                rate: self.rates[target],
            })
            .collect();
        all.sort_by_key(|c| (c.layer.spatial, c.layer.temporal));
        all.dedup_by_key(|c| c.layer);
        let fits = |c: &Choice| {
            structure
                .heights
                .get(usize::from(c.layer.spatial))
                .is_none_or(|&h| kind.tier_of(h) <= tier)
                && self.within(c.layer, top.unwrap_or(0), tier_fps)
        };
        if all.iter().any(fits) {
            all.retain(fits);
        } else {
            all.truncate(1);
        }
        if !self.measured {
            all.truncate(1);
        }
        if smooth
            && let (Some(low), Some(top)) =
                (all.first(), all.iter().map(|c| c.layer.temporal).max())
        {
            let low = low.layer.spatial;
            all.retain(|c| c.layer.spatial == low || c.layer.temporal == top);
            all.sort_by_key(|c| (c.layer.temporal, c.layer.spatial));
        }
        let group = all.last().map(|c| c.kept(smooth));
        let floor = all.iter().position(|c| Some(c.kept(smooth)) == group);
        all.truncate(
            all.len()
                .saturating_sub(steps)
                .max(floor.map_or(0, |i| i + 1)),
        );
        all
    }
}

/// A viewer's moves along its choices: the stepper and the layer it chose.
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    stepper: Stepper,
    layer: Target,
}

impl Pace {
    /// Its point in `choices`: its layer, else the highest choice at or below
    /// it in both layers, else the lowest.
    pub fn seat(&self, choices: &[Choice]) -> usize {
        let below = |c: &Choice| {
            c.layer.spatial <= self.layer.spatial && c.layer.temporal <= self.layer.temporal
        };
        choices.iter().rposition(below).unwrap_or(0)
    }
}

/// The point of nonempty `choices` a viewer wants, and its target, at most
/// `ceiling`, the budgets' choice. The `pace` moves along the choices, grouped
/// by what the source's mode keeps; it starts at `ceiling` once the source is
/// `measured`. When the choices change, it keeps its layer, or takes the
/// highest one below it.
pub fn pick(
    choices: &[Choice],
    ceiling: usize,
    smooth: bool,
    measured: bool,
    pace: &mut Option<Pace>,
    now: Duration,
) -> (usize, Wanted) {
    let groups: Vec<u32> = choices.iter().map(|c| u32::from(c.kept(smooth))).collect();
    let point = match pace {
        // A source whose rates are being measured again keeps the pace.
        _ if !measured => ceiling,
        Some(pace) => {
            if choices.get(pace.stepper.current()).map(|c| c.layer) != Some(pace.layer) {
                pace.stepper.seat(pace.seat(choices));
            }
            pace.stepper.update(&groups, ceiling, now)
        }
        None => {
            let mut stepper = Stepper::new(ceiling);
            let point = stepper.update(&groups, ceiling, now);
            let layer = choices[point].layer;
            *pace = Some(Pace { stepper, layer });
            point
        }
    };
    let choice = &choices[point.min(choices.len() - 1)];
    if let Some(pace) = pace.as_mut().filter(|_| measured) {
        pace.layer = choice.layer;
    }
    (point, (choice.target, choice.layer.spatial))
}

/// A decode target and its spatial layer, the highest in the target.
pub type Wanted = (usize, u8);

/// The spatial layer of a late frame that went out for no target.
const NO_LAYER: u8 = u8::MAX;

/// Keeps the latest eight distinct items in `recent`, as frame numbers or
/// temporal units.
pub fn push<T: PartialEq>(recent: &mut VecDeque<T>, item: T) {
    if !recent.contains(&item) {
        recent.push_back(item);
        if recent.len() > 8 {
            recent.pop_front();
        }
    }
}

/// One receiver's view of one source: forwards the frames of its decode target
/// and ends each temporal unit it forwards with the marker.
#[derive(Clone, Debug, Default)]
pub struct Selector {
    current: Option<Wanted>,
    wanted: Option<Wanted>,
    /// Per frame number modulo [`HISTORY`]: the number of a frame forwarded
    /// whole, the decode targets whose chains it kept intact, and the spatial
    /// layer of the target it went out for, until it is left out.
    history: Vec<(u16, u32, Option<u8>)>,
    newest: Option<u16>,
    /// The latest temporal units with a packet forwarded, by RTP timestamp,
    /// and the target each went out for: a unit's later frames go out for its
    /// target, and a switch waits for a unit with none.
    units: VecDeque<(u64, Wanted)>,
    /// Whether the receiver lacks the wanted target's chain.
    broken: bool,
    /// The target to forward while a switch to a lower spatial layer waits
    /// for a key picture.
    meanwhile: Option<Wanted>,
    /// The latest frame whose start came by, and recent frames with a packet
    /// left out before their start came.
    started: Option<u16>,
    early: VecDeque<u16>,
    /// Frames that forwarded frames reference and that are still to come:
    /// they go out when they come, late.
    owed: VecDeque<u16>,
    /// The frame the current target began at: earlier frames stay out.
    since: Option<u16>,
    /// Per frame number modulo [`HISTORY`], a frame whose start came by.
    seen: Vec<Option<u16>>,
}

impl Selector {
    /// Sets the decode target to forward, `None` to forward nothing. The switch
    /// happens at the next frame that starts a temporal unit for the receiver,
    /// is a switch point for the target and whose chain the receiver holds.
    pub fn want(&mut self, target: Option<Wanted>) {
        if target != self.wanted {
            self.broken = false;
        }
        self.wanted = target;
    }

    /// Sets the target to forward while a switch to a lower spatial layer
    /// waits for a key picture: the current spatial layer's lowest temporal
    /// layer ([`Layers::base`]), so the receiver gets fewer frames at once.
    pub fn meanwhile(&mut self, target: Option<Wanted>) {
        self.meanwhile = target;
    }

    pub fn current(&self) -> Option<usize> {
        self.current.map(|(target, _)| target)
    }

    /// Whether a switch waits for a switch point.
    pub fn waiting(&self) -> bool {
        self.wanted.is_some() && self.current != self.wanted
    }

    /// Whether the switch waits for a key picture: the receiver lacks a frame
    /// of the wanted target's chain.
    pub fn needs_keyframe(&self) -> bool {
        self.waiting() && self.broken
    }

    /// Whether the receiver holds every earlier frame of the chain protecting
    /// `target`. Without chains the switch indications alone count.
    fn intact(&self, frame: &Frame, target: usize) -> bool {
        let Some(diff) = frame.chain_diff(target) else {
            return true;
        };
        let number = frame.number.wrapping_sub(u16::from(diff));
        diff == 0
            || self
                .history
                .get(usize::from(number) % HISTORY)
                .is_some_and(|&(n, targets, _)| n == number && targets >> target & 1 == 1)
    }

    /// Whether to forward a packet of `frame` in the temporal unit at
    /// `timestamp`, and with which marker: `marker` is the publisher's, set at
    /// the end of its temporal unit.
    pub fn forward(&mut self, frame: &Frame, timestamp: u64, marker: bool) -> Option<bool> {
        let passed = self.passed(frame.number);
        if frame.start {
            if self.seen.is_empty() {
                self.seen = vec![None; HISTORY];
            }
            self.seen[usize::from(frame.number) % HISTORY] = Some(frame.number);
        }
        // The rest of a frame that went out goes out too, also late, repeated
        // and for an earlier target, with its unit's marker.
        if let Some(spatial) = self.went(frame) {
            return Some(Self::end(frame, marker, spatial));
        }
        // A late frame that forwarded frames reference goes out, with the
        // publisher's marker alone.
        if self.owed.contains(&frame.number) {
            if frame.start {
                self.owed.retain(|&n| n != frame.number);
                self.record(frame, NO_LAYER);
            }
            return Some(Self::end(frame, marker, NO_LAYER));
        }
        // A frame with a packet left out before its start would arrive
        // incomplete: it counts as left out.
        if frame.start && self.early.contains(&frame.number) {
            self.dropped(frame);
            return None;
        }
        // A frame whose start came and stayed out stays out, with its later
        // and repeated packets.
        if passed {
            return None;
        }
        // A temporal unit with a packet out keeps its target. A late or
        // repeated frame start starts no switch, nor does a frame in such a
        // unit.
        let unit = self
            .units
            .iter()
            .find(|&&(at, _)| at == timestamp)
            .map(|&(_, target)| target);
        if frame.start && !frame.late && unit.is_none() && self.current != self.wanted {
            match self.wanted {
                None => self.current = None,
                Some((target, spatial)) => {
                    let intact = self.intact(frame, target);
                    self.broken = !intact;
                    let switch =
                        |target| self.intact(frame, target) && frame.dti(target) == Dti::Switch;
                    if intact && switch(target) {
                        self.current = self.wanted;
                        self.since = Some(frame.number);
                    } else if let (Some(base), Some(current)) = (self.meanwhile, self.current)
                        && !intact
                        && spatial < current.1
                        && base.1 == current.1
                        && base != current
                        && switch(base.0)
                    {
                        self.current = Some(base);
                        self.since = Some(frame.number);
                    }
                }
            }
        }
        let ahead = |of: u16| frame.number.wrapping_sub(of).wrapping_sub(1) < 0x7fff;
        if frame.start && self.started.is_none_or(ahead) {
            self.started = Some(frame.number);
        }
        let before = self
            .since
            .is_some_and(|since| since != frame.number && !ahead(since));
        let target = unit
            .or(self.current.filter(|_| !before))
            .filter(|&(target, _)| frame.dti(target) != Dti::NotPresent);
        let Some(target @ (_, spatial)) = target else {
            if !frame.start && self.started.is_none_or(ahead) {
                push(&mut self.early, frame.number);
            }
            return None;
        };
        // A frame whose reference came by and stayed out cannot be decoded.
        if frame.start && frame.references().any(|r| self.passed(r)) {
            self.dropped(frame);
            return None;
        }
        if unit.is_none() {
            self.units.push_back((timestamp, target));
            if self.units.len() > 8 {
                self.units.pop_front();
            }
        }
        if frame.start {
            self.record(frame, spatial);
        }
        Some(Self::end(frame, marker, spatial))
    }

    /// Whether frame `number` came by and stayed out.
    fn passed(&self, number: u16) -> bool {
        let index = usize::from(number) % HISTORY;
        let sent = self
            .history
            .get(index)
            .is_some_and(|&(n, _, s)| n == number && s.is_some());
        !sent && self.seen.get(index).is_some_and(|&n| n == Some(number))
    }

    /// Owes the receiver the frames `frame` references that are still to come.
    fn owe(&mut self, frame: &Frame) {
        for reference in frame.references() {
            let entry = self.history.get(usize::from(reference) % HISTORY);
            if entry.is_none_or(|&(n, ..)| n != reference) && !self.owed.contains(&reference) {
                self.owed.push_back(reference);
                if self.owed.len() > 16 {
                    self.owed.pop_front();
                }
            }
        }
    }

    /// The marker of a forwarded packet: the publisher's `marker`, or the end
    /// of the frame on the target's top spatial layer.
    fn end(frame: &Frame, marker: bool, spatial: u8) -> bool {
        marker || frame.end && frame.layer.spatial == spatial
    }

    /// The spatial layer of the target `frame` went out for, when its start
    /// went out.
    fn went(&self, frame: &Frame) -> Option<u8> {
        let &(number, _, spatial) = self.history.get(usize::from(frame.number) % HISTORY)?;
        spatial.filter(|_| number == frame.number)
    }

    /// Whether `frame`'s start went out: its other packets go out too.
    pub fn sent(&self, frame: &Frame) -> bool {
        self.went(frame).is_some()
    }

    /// Whether forwarded frames reference `frame` and owe it to the receiver.
    pub fn owes(&self, frame: &Frame) -> bool {
        self.owed.contains(&frame.number)
    }

    fn record(&mut self, frame: &Frame, spatial: u8) {
        let targets = (0..MAX_TARGETS)
            .filter(|&target| self.intact(frame, target))
            .fold(0, |mask, target| mask | 1 << target);
        if self.history.is_empty() {
            self.history = vec![(0, 0, None); HISTORY];
        }
        // Numbers skipped since the newest frame lose their entries, so none
        // outlives the history.
        let skipped = self
            .newest
            .map_or(1, |newest| frame.number.wrapping_sub(newest));
        if skipped < 0x8000 {
            for n in 1..skipped.min(HISTORY as u16) {
                let number = frame.number.wrapping_sub(n);
                self.history[usize::from(number) % HISTORY] = (0, 0, None);
            }
            self.newest = Some(frame.number);
        }
        self.history[usize::from(frame.number) % HISTORY] = (frame.number, targets, Some(spatial));
        self.owe(frame);
    }

    /// Whether the decode target forwarded now can do without `frame`.
    pub fn discardable(&self, frame: &Frame) -> bool {
        self.current
            .is_some_and(|(target, _)| frame.dti(target) == Dti::Discardable)
    }

    /// `frame` misses at least one packet at the receiver: the SFU did not send
    /// it. Its chains break there, and when the current target needs the frame
    /// the receiver gets nothing more until it can switch again.
    pub fn dropped(&mut self, frame: &Frame) {
        if let Some(entry) = self.history.get_mut(usize::from(frame.number) % HISTORY)
            && entry.0 == frame.number
        {
            *entry = (frame.number, 0, None);
        }
        // A frame from before the current target began leaves it.
        let before = self.since.is_some_and(|since| {
            let behind = since.wrapping_sub(frame.number);
            behind != 0 && behind < 0x8000
        });
        if let Some((target, _)) = self.current
            && matches!(frame.dti(target), Dti::Switch | Dti::Required)
            && !before
        {
            self.current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// Descriptors of an L3T3 key-frame stream captured from Chrome, from
    /// LiveKit's dependency descriptor tests (Apache-2.0).
    const CHROME_L3T3: [&str; 7] = [
        "c1017280081485214eafffaaaa863cf0430c10c302afc0aaa0063c00430010c002a000a80006000040001d954926e082b04a0941b820ac1282503157f974000ca864330e222222eca8655304224230eca877530077004200ef008601df010d",
        "86017340fc",
        "46017340fc",
        "c3017540fc",
        "88017640fc",
        "48017640fc",
        "c2017840fc",
    ];

    fn chrome() -> (Layers, Vec<Frame>) {
        let mut layers = Layers::default();
        let frames = CHROME_L3T3
            .iter()
            .map(|h| {
                layers
                    .packet(&hex(h), 1000, 0, Duration::ZERO)
                    .expect("valid")
            })
            .collect();
        // As after a full second at 30 frames a second.
        layers.measured = true;
        layers.interval = Some(3000);
        (layers, frames)
    }

    fn dtis(frame: &Frame) -> String {
        (0..9)
            .map(|target| match frame.dti(target) {
                Dti::NotPresent => '-',
                Dti::Discardable => 'D',
                Dti::Switch => 'S',
                Dti::Required => 'R',
            })
            .collect()
    }

    #[test]
    fn reads_a_chrome_structure_and_its_frames() {
        let (layers, frames) = chrome();
        let structure = layers.structure.as_ref().expect("structure");
        // libwebrtc's full L3T3: 15 templates, nine decode targets, spatial-major.
        assert_eq!(structure.templates.len(), 15);
        let targets: Vec<_> = structure
            .targets
            .iter()
            .map(|t| (t.spatial, t.temporal))
            .collect();
        assert_eq!(
            targets,
            [
                (0, 0),
                (0, 1),
                (0, 2),
                (1, 0),
                (1, 1),
                (1, 2),
                (2, 0),
                (2, 1),
                (2, 2)
            ]
        );
        assert_eq!(structure.heights, [67, 135, 270]);
        // One chain per spatial layer.
        assert_eq!(structure.protected_by, [0, 0, 0, 1, 1, 1, 2, 2, 2]);
        // Each chain diff points at the latest T0 frame of its layer or below:
        // 0x0172, 0x0173 and the inactive S2's 0x0174.
        let chains: Vec<[u8; 3]> = frames
            .iter()
            .map(|f| [0, 3, 6].map(|t| f.chain_diff(t).expect("chains")))
            .collect();
        assert_eq!(
            chains,
            [
                [0, 0, 0],
                [1, 1, 1],
                [1, 1, 1],
                [3, 2, 1],
                [4, 3, 2],
                [4, 3, 2],
                [6, 5, 4]
            ]
        );
        // The later packets turn the top spatial layer off.
        assert_eq!(structure.active, 0b111111);
        let summary: Vec<_> = frames
            .iter()
            .map(|f| {
                (
                    f.start,
                    f.end,
                    f.number,
                    f.layer.spatial,
                    f.layer.temporal,
                    dtis(f),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (true, true, 0x0172, 0, 0, "SSSSSSSSS".into()),
                (true, false, 0x0173, 1, 0, "---SSSSSS".into()),
                (false, true, 0x0173, 1, 0, "---SSSSSS".into()),
                (true, true, 0x0175, 0, 2, "--D--R--R".into()),
                (true, false, 0x0176, 1, 2, "-----D--R".into()),
                (false, true, 0x0176, 1, 2, "-----D--R".into()),
                (true, true, 0x0178, 0, 1, "-DS-RR-RR".into()),
            ]
        );
    }

    #[test]
    fn needs_a_structure_and_rejects_malformed_descriptors() {
        let mut layers = Layers::default();
        assert_eq!(
            layers.packet(&hex(CHROME_L3T3[1]), 1, 0, Duration::ZERO),
            None
        );
        assert_eq!(layers.packet(&[0x80, 0x01], 1, 0, Duration::ZERO), None);
        let full = hex(CHROME_L3T3[0]);
        assert_eq!(layers.packet(&full[..20], 1, 0, Duration::ZERO), None);
        assert!(layers.structure.is_none());
        assert!(layers.packet(&full, 1, 0, Duration::ZERO).is_some());
        // A template id outside the structure.
        assert_eq!(layers.packet(&[0xff, 0, 0], 1, 0, Duration::ZERO), None);
    }

    /// A synthetic L1T3 stream with libwebrtc's decode target indications:
    /// templates T0, T1, T2 and decode targets for a quarter, half and all of the
    /// frame rate. T0 frames are switch points for all three, T1 frames for the
    /// full rate.
    fn l1t3(id: u32, number: u16, structure: bool) -> Vec<u8> {
        let mut bits = Vec::new();
        let mut put = |value: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push((value >> i) & 1 == 1);
            }
        };
        put(1, 1);
        put(1, 1);
        put(id, 6);
        put(u32::from(number), 16);
        if structure {
            put(0b10000, 5);
            put(0, 6); // offset
            put(2, 5); // three decode targets
            put(1, 2);
            put(1, 2);
            put(3, 2); // T0, T1, T2
            put(0b10_10_10, 6); // T0: S S S
            put(0b00_01_10, 6); // T1: - D S
            put(0b00_00_01, 6); // T2: - - D
            put(0, 3); // no frame diffs
            put(0, 2); // no chains: ns(4) of 0
            put(0, 1); // no sizes
        }
        while bits.len() % 8 != 0 {
            bits.push(false);
        }
        bits.chunks(8)
            .map(|byte| byte.iter().fold(0, |a, &b| (a << 1) | u8::from(b)))
            .collect()
    }

    #[test]
    fn reads_dtis_in_target_order() {
        let mut layers = Layers::default();
        let frame = layers
            .packet(&l1t3(0, 0, true), 1, 0, Duration::ZERO)
            .expect("valid");
        assert_eq!([frame.dti(0), frame.dti(1), frame.dti(2)], [Dti::Switch; 3]);
        let t1 = layers
            .packet(&l1t3(1, 1, false), 1, 0, Duration::ZERO)
            .expect("valid");
        assert_eq!(
            [t1.dti(0), t1.dti(1), t1.dti(2)],
            [Dti::NotPresent, Dti::Discardable, Dti::Switch]
        );
        let structure = layers.structure.as_ref().expect("structure");
        let targets: Vec<_> = structure.targets.iter().map(|t| t.temporal).collect();
        assert_eq!(targets, [0, 1, 2]);
    }

    #[test]
    fn a_receiver_gets_its_temporal_layers_and_switches_at_switch_points() {
        let mut layers = Layers::default();
        let mut low = Selector::default();
        let mut full = Selector::default();
        low.want(Some((0, 0)));
        full.want(Some((2, 0)));
        // T0 T2 T1 T2 repeated, 30 frames a second.
        let pattern = [0, 2, 1, 2];
        let mut forwarded = (0, 0);
        for number in 0..40u16 {
            let id = pattern[usize::from(number) % 4];
            let frame = layers
                .packet(&l1t3(id, number, number == 0), 1, 0, Duration::ZERO)
                .expect("valid");
            let at = u64::from(number);
            // One packet per frame: each forwarded one ends its temporal unit.
            forwarded.0 += u32::from(low.forward(&frame, at, true).is_some());
            forwarded.1 += u32::from(full.forward(&frame, at, true) == Some(true));
            if number == 21 {
                // Up at the next switch point, the T1 frame 22; without chains
                // the switch indication suffices.
                low.want(Some((2, 0)));
                assert!(low.waiting());
            }
            if number == 22 {
                assert!(!low.waiting());
            }
        }
        // Frames 0, 4, …, 20 at T0, then all frames from 22 on.
        assert_eq!(forwarded, (6 + 18, 40));
        // Down to nothing at once.
        low.want(None);
        let frame = layers
            .packet(&l1t3(1, 41, false), 1, 0, Duration::ZERO)
            .expect("valid");
        assert_eq!(low.forward(&frame, 41, true), None);
        assert_eq!(low.current(), None);
        assert!(!low.waiting());
    }

    /// libwebrtc's L2T2_KEY structure (scalability_structure_key_svc.cc): two
    /// spatial layers that reference each other only in key pictures, two
    /// temporal layers, one chain per spatial layer. Decode targets S0T0, S0T1,
    /// S1T0, S1T1.
    const L2T2_KEY: [(u8, u8, &str, [u8; 2]); 6] = [
        (0, 0, "SSSS", [0, 0]),
        (0, 0, "SS--", [4, 3]),
        (0, 1, "-D--", [2, 1]),
        (1, 0, "--SS", [1, 1]),
        (1, 0, "--SS", [1, 4]),
        (1, 1, "---D", [3, 2]),
    ];

    struct Writer(Vec<bool>);

    impl Writer {
        fn put(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                self.0.push((value >> i) & 1 == 1);
            }
        }

        fn ns(&mut self, value: u32, n: u32) {
            let width = u32::BITS - n.leading_zeros();
            let m = (1 << width) - n;
            if value < m {
                self.put(value, width - 1);
            } else {
                self.put((value + m) >> 1, width - 1);
                self.put((value + m) & 1, 1);
            }
        }

        fn bytes(mut self) -> Vec<u8> {
            while !self.0.len().is_multiple_of(8) {
                self.0.push(false);
            }
            self.0
                .chunks(8)
                .map(|byte| byte.iter().fold(0, |a, &b| (a << 1) | u8::from(b)))
                .collect()
        }
    }

    /// The descriptor of one L2T2_KEY frame in one packet, with the structure
    /// and layer sizes when `structure`.
    pub(super) fn l2t2_key(template: usize, number: u16, structure: bool) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.put(0b11, 2);
        w.put(template as u32, 6);
        w.put(u32::from(number), 16);
        if structure {
            w.put(0b10000, 5);
            w.put(0, 6);
            w.put(3, 5);
            let mut previous = (0, 0);
            for &(spatial, temporal, ..) in &L2T2_KEY[1..] {
                w.put(
                    match (spatial - previous.0, temporal) {
                        (0, t) if t == previous.1 => 0,
                        (0, _) => 1,
                        _ => 2,
                    },
                    2,
                );
                previous = (spatial, temporal);
            }
            w.put(3, 2);
            for &(_, _, dtis, _) in &L2T2_KEY {
                for c in dtis.chars() {
                    w.put("-DSR".find(c).expect("a DTI") as u32, 2);
                }
            }
            for _ in &L2T2_KEY {
                // Frame diffs, unused by the selector.
                w.put(0, 1);
            }
            w.ns(2, 5);
            for chain in [0, 0, 1, 1] {
                w.ns(chain, 2);
            }
            for &(.., chains) in &L2T2_KEY {
                for diff in chains {
                    w.put(u32::from(diff), 4);
                }
            }
            w.put(1, 1);
            for (width, height) in [(960, 540), (1920, 1080)] {
                w.put(width - 1, 16);
                w.put(height - 1, 16);
            }
        }
        w.bytes()
    }

    /// The templates of temporal unit `unit` after a key picture at unit 0:
    /// key, then T1 and T0 alternating.
    pub(super) fn l2t2_unit(unit: u16) -> [usize; 2] {
        match unit % 2 {
            _ if unit == 0 => [0, 3],
            1 => [2, 5],
            _ => [1, 4],
        }
    }

    /// Runs `units` temporal units of an L2T2_KEY stream with key pictures at
    /// `keys` (every one since the stream began) through `selector`, calling
    /// `step` at the start of each unit; returns the frame numbers forwarded and
    /// those with the marker.
    fn run_l2t2(
        layers: &mut Layers,
        selector: &mut Selector,
        units: std::ops::Range<u16>,
        keys: &[u16],
        mut step: impl FnMut(u16, &mut Selector),
    ) -> (Vec<u16>, Vec<u16>) {
        let (mut forwarded, mut marked) = (Vec::new(), Vec::new());
        for unit in units {
            step(unit, selector);
            let since_key = unit - keys.iter().filter(|&&k| k <= unit).max().expect("a key");
            for (spatial, template) in l2t2_unit(since_key).into_iter().enumerate() {
                let number = unit * 2 + spatial as u16;
                let descriptor = l2t2_key(template, number, template == 0);
                let frame = layers
                    .packet(&descriptor, 1, 0, Duration::ZERO)
                    .expect("valid");
                if let Some(marker) = selector.forward(&frame, u64::from(unit), spatial == 1) {
                    forwarded.push(number);
                    if marker {
                        marked.push(number);
                    }
                }
            }
        }
        (forwarded, marked)
    }

    #[test]
    fn reads_chains_per_decode_target() {
        let mut layers = Layers::default();
        let key = layers
            .packet(&l2t2_key(0, 0, true), 1, 0, Duration::ZERO)
            .expect("valid");
        assert_eq!(
            layers.structure.as_ref().expect("structure").protected_by,
            [0, 0, 1, 1]
        );
        assert_eq!(
            (0..4).map(|t| key.chain_diff(t)).collect::<Vec<_>>(),
            [Some(0); 4]
        );
        let delta = layers
            .packet(&l2t2_key(4, 5, false), 1, 0, Duration::ZERO)
            .expect("valid");
        let diffs: Vec<_> = (0..4)
            .map(|t| delta.chain_diff(t).expect("chains"))
            .collect();
        assert_eq!(diffs, [1, 1, 4, 4]);
        // Custom chains replace the template's: fdiffs 3 and 18, chains 7 and 9.
        let mut w = Writer(Vec::new());
        w.put(0b11, 2);
        w.put(4, 6);
        w.put(5, 16);
        w.put(0b00011, 5);
        w.put(1, 2);
        w.put(2, 4);
        w.put(2, 2);
        w.put(17, 8);
        w.put(0, 2);
        w.put(7, 8);
        w.put(9, 8);
        let custom = layers
            .packet(&w.bytes(), 1, 0, Duration::ZERO)
            .expect("valid");
        assert_eq!(custom.references().collect::<Vec<_>>(), [2, 65523]);
        assert_eq!(custom.chain_diff(0), Some(7));
        assert_eq!(custom.chain_diff(3), Some(9));
    }

    #[test]
    fn an_upswitch_across_spatial_layers_waits_for_the_key_picture() {
        let mut layers = Layers::default();
        let mut selector = Selector::default();
        selector.want(Some((1, 0)));
        // S0T1: the key picture's base, then every S0 frame, each one marked.
        let (forwarded, marked) = run_l2t2(&mut layers, &mut selector, 0..6, &[0], |_, _| {});
        assert_eq!(forwarded, [0, 2, 4, 6, 8, 10]);
        assert_eq!(marked, forwarded);
        // Up to S1T1: the S1T0 frames are switch points without the receiver's
        // chain, so the switch waits for the next key picture at unit 9.
        let (forwarded, marked) =
            run_l2t2(&mut layers, &mut selector, 6..12, &[0, 9], |unit, s| {
                if unit == 6 {
                    s.want(Some((3, 1)));
                }
                if (7..10).contains(&unit) {
                    assert!(s.needs_keyframe(), "unit {unit}");
                }
            });
        assert_eq!(forwarded, [12, 14, 16, 18, 19, 21, 23]);
        // The key picture's base is part of the receiver's temporal unit.
        assert_eq!(marked, [12, 14, 16, 19, 21, 23]);
        assert!(!selector.waiting());
    }

    /// An L2T2_KEY viewer on S1T1 whose wanted target drops one step at unit
    /// 20, without a key picture: the frames it gets over the next 19 units.
    fn after_a_drop(wanted: Wanted) -> (usize, bool) {
        let mut layers = Layers::default();
        let mut selector = Selector::default();
        selector.want(Some((3, 1)));
        let mut forwarded = 0;
        for unit in 0..40u16 {
            if unit == 20 {
                selector.meanwhile(selector.current().and_then(|c| layers.base(c)));
                selector.want(Some(wanted));
            }
            for (spatial, template) in l2t2_unit(unit).into_iter().enumerate() {
                let number = unit * 2 + spatial as u16;
                let descriptor = l2t2_key(template, number, template == 0);
                let frame = layers
                    .packet(&descriptor, 1, 0, Duration::ZERO)
                    .expect("valid");
                let sent = selector.forward(&frame, u64::from(unit), spatial == 1);
                forwarded += usize::from(sent.is_some() && unit > 20);
            }
        }
        (forwarded, selector.needs_keyframe())
    }

    #[test]
    fn a_spatial_downswitch_drops_temporal_layers_while_it_waits() {
        // Sharp, S1T0: every other unit at once.
        assert_eq!(after_a_drop((2, 1)), (9, false));
        // Smooth, S0T1: S1T0 until a key picture brings the S0 chain.
        assert_eq!(after_a_drop((1, 0)), (9, true));
    }

    #[test]
    fn a_downswitch_needs_the_lower_chain() {
        let mut layers = Layers::default();
        let mut selector = Selector::default();
        selector.want(Some((3, 1)));
        // From the key picture the receiver holds the S0 chain until the first
        // S0T0 delta frame (unit 2), which it can still switch down at.
        let (forwarded, _) = run_l2t2(&mut layers, &mut selector, 0..4, &[0], |unit, s| {
            if unit == 2 {
                s.want(Some((0, 0)));
            }
        });
        assert_eq!(forwarded, [0, 1, 3, 4]);
        // Later the S0 chain is broken for an S1 receiver: it stays at S1 and
        // needs a key picture.
        let mut late = Selector::default();
        late.want(Some((3, 1)));
        run_l2t2(&mut layers, &mut late, 4..8, &[0, 4], |unit, s| {
            if unit == 7 {
                s.want(Some((0, 0)));
            }
        });
        let (forwarded, _) = run_l2t2(&mut layers, &mut late, 8..10, &[0, 4], |_, _| {});
        assert_eq!(forwarded, [17, 19]);
        assert!(late.needs_keyframe());
    }

    #[test]
    fn a_temporal_switch_needs_no_key_picture() {
        let mut layers = Layers::default();
        let mut selector = Selector::default();
        selector.want(Some((2, 0)));
        let (forwarded, _) = run_l2t2(&mut layers, &mut selector, 0..8, &[0], |unit, s| {
            if unit == 3 {
                s.want(Some((3, 1)));
            }
            assert!(!s.needs_keyframe());
        });
        // S1T0 frames only, then from the next S1T0 frame (unit 4) all S1 frames.
        assert_eq!(forwarded, [0, 1, 5, 9, 11, 13, 15]);
    }

    #[test]
    fn a_dropped_frame_breaks_its_chain_and_a_discardable_one_nothing() {
        let mut layers = Layers::default();
        let mut selector = Selector::default();
        selector.want(Some((3, 1)));
        run_l2t2(&mut layers, &mut selector, 0..3, &[0], |_, _| {});
        // Unit 3 is T1: its S1 frame is discardable for S1T1.
        let t1 = layers
            .packet(&l2t2_key(5, 7, false), 1, 0, Duration::ZERO)
            .expect("valid");
        selector.dropped(&t1);
        assert_eq!(selector.current(), Some(3));
        // Unit 4's S1T0 frame is required: nothing more until a key picture.
        let (forwarded, _) = run_l2t2(&mut layers, &mut selector, 4..5, &[0], |_, _| {});
        assert_eq!(forwarded, [9]);
        let t0 = layers
            .packet(&l2t2_key(4, 9, false), 1, 0, Duration::ZERO)
            .expect("valid");
        selector.dropped(&t0);
        let (forwarded, _) = run_l2t2(&mut layers, &mut selector, 5..8, &[0, 7], |_, _| {});
        assert_eq!(forwarded, [14, 15]);
    }

    /// `descriptor` as a packet after the first of its frame.
    fn rest(descriptor: &[u8]) -> Vec<u8> {
        let mut rest = descriptor[..3].to_vec();
        rest[0] &= 0x7f;
        rest
    }

    #[test]
    fn layers_mark_late_and_repeated_packets() {
        let mut layers = Layers::default();
        let mut late = |descriptor: &[u8], timestamp: u64| {
            let frame = layers.packet(descriptor, 1, timestamp, Duration::ZERO);
            frame.expect("valid").late
        };
        assert!(!late(&l2t2_key(0, 0, true), 0));
        assert!(!late(&l2t2_key(3, 1, false), 0));
        // Frame 2's later packet before its start, then the start twice.
        let two = l2t2_key(2, 2, false);
        assert!(!late(&rest(&two), 3000));
        assert!(!late(&two, 3000));
        assert!(late(&two, 3000));
        assert!(!late(&rest(&two), 3000));
        // Frame 1 after frame 2.
        assert!(late(&l2t2_key(3, 1, false), 0));
        // A jump back of more than the history in frame numbers starts over.
        assert!(!late(&l2t2_key(1, 1002, false), 6000));
        assert!(!late(&l2t2_key(1, 2, false), 9000));
        assert!(late(&l2t2_key(1, 1, false), 12_000));
    }

    #[test]
    fn a_temporal_unit_goes_out_for_one_target() {
        let mut layers = Layers::default();
        let mut frame = |descriptor: Vec<u8>| layers.packet(&descriptor, 1, 0, Duration::ZERO);
        let mut selector = Selector::default();
        selector.want(Some((3, 1)));
        // An S1T1 viewer gets the key picture's S0 frame, then wants S0T1 and
        // switches at unit 2's S0T0 frame, before the key picture's S1 frame
        // comes: that frame still goes out, with its unit's marker.
        let key = frame(l2t2_key(0, 0, true)).expect("valid");
        assert_eq!(selector.forward(&key, 0, false), Some(false));
        selector.want(Some((1, 0)));
        let t0 = frame(l2t2_key(1, 4, false)).expect("valid");
        assert_eq!(selector.forward(&t0, 2, false), Some(true));
        assert_eq!(selector.current(), Some(1));
        let top = frame(l2t2_key(3, 1, false)).expect("valid");
        assert_eq!(selector.forward(&top, 0, true), Some(true));
        // A switch waits for a unit with no packet out.
        selector.want(Some((3, 1)));
        let s1 = frame(l2t2_key(4, 5, false)).expect("valid");
        assert_eq!(selector.forward(&s1, 2, true), None);
        assert_eq!(selector.current(), Some(1));
    }

    #[test]
    fn a_frame_that_stayed_out_stays_out_whole() {
        let mut layers = Layers::default();
        let mut frame = |descriptor: &[u8]| {
            let frame = layers.packet(descriptor, 1, 0, Duration::ZERO);
            frame.expect("valid")
        };
        let mut selector = Selector::default();
        // A key picture's S0 frame in two packets: its start passes a viewer
        // that wants nothing yet, and it wants S0T0 before the rest comes.
        let mut start = l2t2_key(0, 0, true);
        start[0] &= !0x40;
        let rest = rest(&start);
        assert_eq!(selector.forward(&frame(&start), 0, false), None);
        selector.want(Some((0, 0)));
        assert_eq!(selector.forward(&frame(&rest), 0, false), None);
        // A repeated start stays out too, and the S0T0 frames after it lack
        // their chain: the viewer waits for the next key picture.
        assert_eq!(selector.forward(&frame(&start), 0, false), None);
        assert_eq!(
            selector.forward(&frame(&l2t2_key(1, 2, false)), 1, true),
            None
        );
        assert!(selector.needs_keyframe());
        let key = frame(&l2t2_key(0, 4, false));
        assert_eq!(selector.forward(&key, 2, false), Some(true));
    }

    /// Frame `number` of an L1T3 source (T0 T2 T1 T2), 1000 bytes, at `millis`
    /// of media and wall time.
    fn l1t3_at(layers: &mut Layers, number: u16, millis: u64) -> Option<Frame> {
        let id = [0, 2, 1, 2][usize::from(number) % 4];
        let descriptor = l1t3(id, number, number == 0);
        layers.packet(
            &descriptor,
            1000,
            millis * 90,
            Duration::from_millis(millis),
        )
    }

    fn targets(layers: &Layers, kind: Kind, tier: usize) -> Vec<usize> {
        let choices = layers.choices(kind, tier, false, 0);
        choices.iter().map(|c| c.target).collect()
    }

    #[test]
    fn measures_rates_and_offers_the_targets_that_fit() {
        let mut layers = Layers::default();
        // Two seconds at 30 frames a second.
        for number in 0..61u16 {
            l1t3_at(&mut layers, number, u64::from(number) * 1000 / 30);
        }
        let choices = layers.choices(Kind::Camera, 3, false, 0);
        let rates: Vec<_> = choices.iter().map(|c| c.rate).collect();
        // Frames 30 to 59 in the second window: 7, 15 and 30 of 8000 bits.
        assert_eq!(rates, [56_000, 120_000, 240_000]);
        // A 15 fps tier takes the 7.5 and 15 fps targets.
        assert_eq!(targets(&layers, Kind::Camera, 0), [0, 1]);
    }

    #[test]
    fn a_source_offers_its_lowest_target_until_a_full_second_measures_it() {
        // 30 frames a second from 4 s on.
        let mut layers = Layers::default();
        for number in 0..30 {
            l1t3_at(&mut layers, number, 4000 + u64::from(number) * 1000 / 30);
        }
        assert!(!layers.measured());
        assert_eq!(
            targets(&layers, Kind::Screen, 3),
            [0],
            "within the first second"
        );
        assert_eq!(layers.targets(), 3, "three layers sent");
        l1t3_at(&mut layers, 30, 5000);
        assert!(layers.measured());
        // A 15 fps tier takes the quarter and the half rate.
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1]);
        // A window across a three-second pause keeps the measured rates.
        l1t3_at(&mut layers, 31, 8000);
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1]);
    }

    #[test]
    fn a_window_across_a_pause_keeps_the_rates() {
        let mut layers = Layers::default();
        let rate = |layers: &Layers| layers.choices(Kind::Screen, 3, false, 0)[2].rate;
        for number in 0..61u16 {
            l1t3_at(&mut layers, number, u64::from(number) * 1000 / 30);
        }
        assert_eq!(rate(&layers), 240_000);
        // A 1.5 s pause inside the next window: the rates stay.
        for number in 61..76u16 {
            l1t3_at(
                &mut layers,
                number,
                2000 + u64::from(number - 61) * 1000 / 30,
            );
        }
        l1t3_at(&mut layers, 76, 4000);
        assert_eq!(rate(&layers), 240_000);
        // A 0.6 s pause counts: 16 frames of 8000 bits over 1.1 s.
        for number in 77..92u16 {
            l1t3_at(
                &mut layers,
                number,
                4000 + u64::from(number - 76) * 1000 / 30,
            );
        }
        l1t3_at(&mut layers, 92, 5100);
        assert_eq!(rate(&layers), 16 * 8000 * 1000 / 1100);
    }

    #[test]
    fn frame_rates_come_from_rtp_timestamps() {
        let mut layers = Layers::default();
        // 5 frames a second. Before 9 units the source counts as 60 fps: a
        // 15 fps tier takes the quarter rate alone, a 30 fps tier the half
        // rate too.
        for number in 0..8 {
            l1t3_at(&mut layers, number, u64::from(number) * 200);
        }
        assert!(layers.measured());
        assert_eq!(targets(&layers, Kind::Screen, 0), [0]);
        assert_eq!(targets(&layers, Kind::Screen, 1), [0, 1]);
        // Then 9 units span 1.6 s: a slow window, 5 fps, without an active
        // one: the full rate fits the 15 fps tier.
        for number in 8..16 {
            l1t3_at(&mut layers, number, u64::from(number) * 200);
        }
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1, 2]);
        // Eight units 33 ms apart in media time, all arriving at once: 30 fps.
        for number in 16..25 {
            let id = [0, 2, 1, 2][usize::from(number) % 4];
            let media = (3200 + u64::from(number - 16) * 1000 / 30) * 90;
            let at = Duration::from_millis(3300);
            layers.packet(&l1t3(id, number, false), 1000, media, at);
        }
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1]);
        // A late packet of an earlier unit leaves the frame rate.
        layers.packet(
            &l1t3(2, 9, false),
            1000,
            1800 * 90,
            Duration::from_millis(3300),
        );
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1]);
    }

    #[test]
    fn a_slow_screen_reaches_a_15_fps_viewer_whole() {
        // A screen at 4 fps from its start, as while typing.
        let mut layers = Layers::default();
        for number in 0..40u16 {
            l1t3_at(&mut layers, number, u64::from(number) * 250);
        }
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1, 2]);
    }

    #[test]
    fn an_inactive_top_temporal_layer_leaves_the_frame_rate_cap() {
        // A 60 fps capture without its top temporal layer: units arrive at
        // 30 fps, T0 T1 T0 T1, so T1 runs at 30 fps.
        let mut layers = Layers::default();
        for number in 0..61u16 {
            let millis = u64::from(number) * 1000 / 30;
            let descriptor = l1t3(u32::from(number % 2), number, number == 0);
            layers.packet(
                &descriptor,
                1000,
                millis * 90,
                Duration::from_millis(millis),
            );
        }
        layers.structure.as_mut().expect("structure").active = 0b011;
        assert_eq!(targets(&layers, Kind::Screen, 0), [0]);
    }

    #[test]
    fn a_new_target_list_measures_the_rates_again() {
        let mut layers = Layers::default();
        for number in 0..61u16 {
            l1t3_at(&mut layers, number, u64::from(number) * 1000 / 30);
        }
        assert_eq!(targets(&layers, Kind::Screen, 1).len(), 3);
        let mut pace = None;
        let choices = layers.choices(Kind::Screen, 1, false, 0);
        let at = Duration::from_secs;
        assert_eq!(pick(&choices, 2, false, true, &mut pace, at(3)).0, 2);
        // The same structure again keeps the rates; another one does not.
        l1t3_at(&mut layers, 0, 2034);
        assert!(layers.measured());
        let descriptor = l2t2_key(0, 2, true);
        layers.packet(&descriptor, 1000, 2067 * 90, Duration::from_millis(2067));
        assert!(!layers.measured());
        // The pace waits for the new rates.
        let lowest = layers.choices(Kind::Screen, 1, false, 0);
        assert_eq!(pick(&lowest, 0, false, false, &mut pace, at(4)).0, 0);
        assert_eq!(pick(&choices, 2, false, true, &mut pace, at(5)).0, 2);
    }

    #[test]
    fn a_jump_back_in_time_measures_the_frame_rate_again() {
        // 30 fps late in a stream, then a new stream from time 0 at 15 fps.
        let mut layers = Layers::default();
        for number in 0..61u16 {
            l1t3_at(&mut layers, number, 100_000 + u64::from(number) * 1000 / 30);
        }
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1]);
        for number in 61..80u16 {
            let id = [0, 2, 1, 2][usize::from(number) % 4];
            let media = u64::from(number - 61) * 6000;
            let at = Duration::from_millis(102_100 + u64::from(number - 61) * 1000 / 15);
            layers.packet(&l1t3(id, number, false), 1000, media, at);
        }
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1, 2]);
    }

    #[test]
    fn a_static_source_keeps_its_frame_rate() {
        let mut layers = Layers::default();
        for number in 0..61u16 {
            l1t3_at(&mut layers, number, u64::from(number) * 1000 / 30);
        }
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1]);
        // Static: a frame every 600 ms for 12 s. The full rate does not
        // become eligible for the 15 fps tier.
        for number in 61..81u16 {
            l1t3_at(&mut layers, number, 2000 + u64::from(number - 60) * 600);
            assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1], "frame {number}");
        }
        // A source that slows down while active: 15 frames a second.
        for number in 81..100u16 {
            l1t3_at(
                &mut layers,
                number,
                15_000 + u64::from(number - 80) * 1000 / 15,
            );
        }
        assert_eq!(targets(&layers, Kind::Screen, 0), [0, 1, 2]);
    }

    #[test]
    fn a_late_queue_removes_choices_down_to_the_top_group() {
        let (mut layers, _) = chrome();
        layers.structure.as_mut().expect("structure").active = u32::MAX;
        let layers_of = |smooth, steps| -> Vec<(u8, u8)> {
            let choices = layers.choices(Kind::Camera, 3, smooth, steps);
            choices
                .iter()
                .map(|c| (c.layer.spatial, c.layer.temporal))
                .collect()
        };
        // Sharp keeps the top spatial layer: S2T2, then S2T1, then S2T0.
        assert_eq!(layers_of(false, 1).last(), Some(&(2, 1)));
        assert_eq!(layers_of(false, 9).last(), Some(&(2, 0)));
        // Smooth keeps the full frame rate: S2T2, then S1T2, then S0T2.
        assert_eq!(layers_of(true, 1).last(), Some(&(1, 2)));
        assert_eq!(layers_of(true, 9).last(), Some(&(0, 2)));
    }

    #[test]
    fn a_stepper_starts_once_the_source_is_measured() {
        let (layers, _) = chrome();
        let choices = layers.choices(Kind::Camera, 3, false, 0);
        let mut stepper = None;
        let at = Duration::from_secs(1);
        assert_eq!(
            pick(&choices, 5, false, false, &mut stepper, at),
            (5, (5, 1))
        );
        assert!(stepper.is_none());
        assert_eq!(pick(&choices, 5, false, true, &mut stepper, at).0, 5);
        // Up to another spatial layer after the 30 s hold.
        assert_eq!(pick(&choices, 5, false, true, &mut stepper, at).0, 5);
        let mut choices = choices;
        choices.push(Choice {
            target: 6,
            layer: Target {
                spatial: 2,
                temporal: 0,
            },
            rate: 0,
        });
        assert_eq!(pick(&choices, 6, false, true, &mut stepper, at).0, 5);
        let later = at + Duration::from_secs(30);
        assert_eq!(pick(&choices, 6, false, true, &mut stepper, later).0, 6);
    }

    fn layers_of(choices: &[Choice]) -> Vec<(u8, u8)> {
        let layer = |c: &Choice| (c.layer.spatial, c.layer.temporal);
        choices.iter().map(layer).collect()
    }

    /// Two seconds of an L2T2_KEY screen (960 × 540, 1920 × 1080) from media
    /// time `start`, a temporal unit every `interval` ticks, from unit
    /// `first`; returns the time after them.
    fn l2t2_screen(layers: &mut Layers, start: u64, interval: u64, first: u16) -> u64 {
        let mut timestamp = start;
        for unit in first..first + 2 * (CLOCK / interval) as u16 {
            for (spatial, template) in l2t2_unit(unit).into_iter().enumerate() {
                let number = unit * 2 + spatial as u16;
                let descriptor = l2t2_key(template, number, template == 0);
                let now = Duration::from_micros(timestamp * 1_000_000 / CLOCK);
                layers.packet(&descriptor, 1000, timestamp, now);
            }
            timestamp += interval;
        }
        timestamp
    }

    #[test]
    fn a_mode_change_keeps_the_viewers_target() {
        // A Smooth viewer of an L3T3 camera on S2T2; its sender switches to
        // Sharp, whose S2T2 is a later choice.
        let (mut layers, _) = chrome();
        layers.structure.as_mut().expect("structure").active = u32::MAX;
        let smooth = layers.choices(Kind::Camera, 3, true, 0);
        let sharp = layers.choices(Kind::Camera, 3, false, 0);
        let mut pace = None;
        let at = Duration::from_secs;
        let top = |choices: &[Choice]| choices.len() - 1;
        assert_eq!(
            pick(&smooth, top(&smooth), true, true, &mut pace, at(10)).1,
            (8, 2)
        );
        assert_eq!(
            pick(&sharp, top(&sharp), false, true, &mut pace, at(11)).1,
            (8, 2)
        );
    }

    #[test]
    fn a_tier_change_keeps_a_sharp_viewers_resolution() {
        // A Sharp viewer of a 60 fps L2T2_KEY screen on the 1440p60 tier gets
        // S1T1; its tile shrinks to the 1080p30 tier, S1T0, and grows back.
        let mut layers = Layers::default();
        l2t2_screen(&mut layers, 0, 1500, 0);
        let large = layers.choices(Kind::Screen, 2, false, 0);
        let small = layers.choices(Kind::Screen, 1, false, 0);
        assert_eq!(layers_of(&large), [(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(layers_of(&small), [(0, 0), (1, 0)]);
        let mut pace = None;
        let at = Duration::from_secs;
        assert_eq!(pick(&large, 3, false, true, &mut pace, at(10)).1.1, 1);
        assert_eq!(pick(&small, 1, false, true, &mut pace, at(11)).1.1, 1);
        // Back on S1T0, the step to S1T1 waits for its 10 s hold.
        assert_eq!(pick(&large, 3, false, true, &mut pace, at(12)), (2, (2, 1)));
        assert_eq!(pick(&large, 3, false, true, &mut pace, at(22)), (3, (3, 1)));
    }

    #[test]
    fn a_frame_rate_change_keeps_a_sharp_viewers_resolution() {
        // The 1080p30 tier of the screen at 30, 60 and 30 fps.
        let mut layers = Layers::default();
        let end = l2t2_screen(&mut layers, 0, 3000, 0);
        let mut pace = None;
        let at = Duration::from_secs;
        let at30 = layers.choices(Kind::Screen, 1, false, 0);
        assert_eq!(layers_of(&at30), [(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(pick(&at30, 3, false, true, &mut pace, at(10)).1.1, 1);
        let end = l2t2_screen(&mut layers, end, 1500, 60);
        let at60 = layers.choices(Kind::Screen, 1, false, 0);
        assert_eq!(layers_of(&at60), [(0, 0), (1, 0)]);
        assert_eq!(pick(&at60, 1, false, true, &mut pace, at(11)).1.1, 1);
        l2t2_screen(&mut layers, end, 3000, 240);
        let at30 = layers.choices(Kind::Screen, 1, false, 0);
        assert_eq!(pick(&at30, 3, false, true, &mut pace, at(12)).1.1, 1);
    }

    #[test]
    fn active_targets_and_layer_sizes_limit_the_choice() {
        let (mut layers, _) = chrome();
        let targets: Vec<_> = layers
            .choices(Kind::Camera, 3, false, 0)
            .iter()
            .map(|c| c.target)
            .collect();
        assert_eq!(targets, [0, 1, 2, 3, 4, 5]);
        let structure = layers.structure.as_mut().expect("structure");
        structure.active = u32::MAX;
        structure.heights = vec![180, 360, 720];
        let spatial = |tier| {
            let choices = layers.choices(Kind::Camera, tier, false, 0);
            choices
                .iter()
                .map(|c| c.layer.spatial)
                .max()
                .expect("a choice")
        };
        assert_eq!(spatial(3), 2);
        assert_eq!(spatial(2), 2);
        assert_eq!(spatial(1), 1);
        assert_eq!(spatial(0), 0);
        assert_eq!(layers.picture_tier(Kind::Camera), None, "three pictures");
        // A screen tier is taller than every layer.
        layers.structure.as_mut().expect("structure").heights = vec![1440, 2160, 4320];
        assert_eq!(layers.choices(Kind::Screen, 0, false, 0).len(), 1);
        layers.structure.as_mut().expect("structure").heights = vec![1080];
        assert_eq!(layers.picture_tier(Kind::Screen), Some(1));
        assert_eq!(Layers::default().picture_tier(Kind::Screen), None);
    }

    #[test]
    fn sharp_drops_temporal_layers_first_and_smooth_spatial_layers() {
        let (mut layers, _) = chrome();
        layers.structure.as_mut().expect("structure").active = u32::MAX;
        let order = |smooth| -> Vec<(u8, u8)> {
            layers
                .choices(Kind::Camera, 3, smooth, 0)
                .iter()
                .map(|c| (c.layer.spatial, c.layer.temporal))
                .collect()
        };
        // Each step down takes the point before it.
        assert_eq!(
            order(false),
            [
                (0, 0),
                (0, 1),
                (0, 2),
                (1, 0),
                (1, 1),
                (1, 2),
                (2, 0),
                (2, 1),
                (2, 2)
            ]
        );
        assert_eq!(order(true), [(0, 0), (0, 1), (0, 2), (1, 2), (2, 2)]);
    }

    #[test]
    fn an_active_mask_limits_the_choice() {
        let mut layers = Layers::default();
        layers.packet(&l1t3(0, 0, true), 1, 0, Duration::ZERO);
        // Extended fields with only an active mask, 0b011: targets 0 and 1.
        let mut packet = l1t3(1, 1, false);
        packet.extend([0x43, 0]);
        layers.packet(&packet, 1, 0, Duration::ZERO).expect("valid");
        layers.measured = true;
        layers.interval = Some(3000);
        let targets: Vec<_> = layers
            .choices(Kind::Camera, 3, false, 0)
            .iter()
            .map(|c| c.target)
            .collect();
        assert_eq!(targets, [0, 1]);
    }
}

/// The selection model of docs/video-plan.md §5.3 over seeded random calls
/// (proptest is not in the lock file). The source is an L1T2 or L1T3 screen,
/// or an L2T2_KEY or L3T3_KEY camera, with key pictures, new structures and
/// active targets, frame-rate changes and static stretches, frames of up to
/// three packets that arrive reordered, repaired late or repeated, frame
/// numbers that wrap and timestamps that jump back. Viewers join, leave, lose
/// frames at the SFU and change tier, budget, queue steps and mode. Every
/// frame, every second and the end of each run check the invariants.
#[cfg(test)]
mod model {
    use super::tests::{l2t2_key, l2t2_unit};
    use super::*;
    use crate::budget;
    use std::collections::HashSet;

    /// A seeded xorshift generator.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }

    #[derive(Default)]
    struct Viewer {
        selector: Selector,
        pace: Option<Pace>,
        tier: usize,
        budget: u64,
        steps: usize,
        wanted: Option<Wanted>,
        meanwhile: Option<Wanted>,
        /// The frames it got, for the chains.
        held: HashSet<u16>,
        /// The wanted layer, when it last changed, and when it last moved to
        /// another group of eligible choices.
        layer: Option<Target>,
        changed: Duration,
        crossed: Option<Duration>,
    }

    /// The camera's active frame rate.
    const FPS: u64 = 30;
    /// Shorter sides of the two layers (960 × 540 and 1920 × 1080).
    const HEIGHTS: [u32; 2] = [540, 1080];

    /// Whether `layer` fits camera tier `tier`: the invariant, from the
    /// generator's own frame rate.
    fn fits(layer: Target, tier: usize) -> bool {
        let cap = Kind::Camera.ladder()[tier];
        let fps = FPS >> (1 - layer.temporal);
        Kind::Camera.tier_of(HEIGHTS[usize::from(layer.spatial)]) <= tier
            && 4 * fps <= 5 * u64::from(cap.fps)
    }

    fn second(viewer: &mut Viewer, layers: &Layers, smooth: bool, now: Duration) {
        let choices = layers.choices(Kind::Camera, viewer.tier, smooth, viewer.steps);
        if !layers.measured() {
            assert!(choices.len() <= 1, "a choice before a full second");
        }
        for choice in choices.iter().skip(1) {
            assert!(
                fits(choice.layer, viewer.tier),
                "{choice:?} above tier {}",
                viewer.tier
            );
        }
        let rates: Vec<u64> = choices.iter().map(|c| c.rate).collect();
        let current = viewer.pace.map_or(0, |p| p.seat(&choices) + 1);
        let limits = budget::Limits::project(10_000_000_000);
        let stream = budget::Stream {
            room: 0,
            receiver: 0,
            rates: &rates,
            current,
        };
        let layers_given = budget::allocate(&limits, &[viewer.budget], &[stream])[0].layers;
        let wanted = layers_given.checked_sub(1).map(|ceiling| {
            let had = viewer.pace.is_some();
            let (point, wanted) = pick(
                &choices,
                ceiling,
                smooth,
                layers.measured(),
                &mut viewer.pace,
                now,
            );
            let layer = choices[point].layer;
            // A layer that is still a choice changes only by the stepper's
            // rules: down within a group at once, to another group at most
            // once per 10 s, up after the budget allowed it for 10 s within a
            // group or 30 s to another.
            let old = viewer
                .layer
                .and_then(|old| choices.iter().position(|c| c.layer == old));
            if had
                && let Some(old) = old
                && viewer.layer != Some(layer)
            {
                let crossed = choices[old].kept(smooth) != choices[point].kept(smooth);
                let since = now.saturating_sub(viewer.changed);
                if point > old {
                    let hold = Duration::from_secs(if crossed { 30 } else { 10 });
                    assert!(since >= hold, "up after {since:?}, not {hold:?}");
                } else if crossed {
                    let at = viewer
                        .crossed
                        .map_or(Duration::MAX, |at| now.saturating_sub(at));
                    assert!(at >= Duration::from_secs(10), "groups 10 s apart");
                }
                if crossed {
                    viewer.crossed = Some(now);
                }
            }
            if viewer.layer != Some(layer) {
                viewer.changed = now;
            }
            viewer.layer = Some(layer);
            wanted
        });
        viewer.wanted = wanted;
        viewer.meanwhile = viewer.selector.current().and_then(|c| layers.base(c));
        viewer.selector.meanwhile(viewer.meanwhile);
        viewer.selector.want(wanted);
    }

    #[test]
    fn invariants_hold_over_random_calls() {
        for seed in 1..=60u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut layers = Layers::default();
            let mut viewers: Vec<Viewer> = Vec::new();
            let (mut active, mut smooth) = (false, false);
            let (mut timestamp, mut last_key, mut next_second) = (0u64, None::<u64>, 0u64);
            let mut since_key = 0u16;
            for unit in 0..2400u16 {
                // The source: 30 fps with jitter, or static with long gaps.
                timestamp += if active {
                    CLOCK / FPS + rng.below(300)
                } else {
                    CLOCK / 2 + rng.below(CLOCK * 2)
                };
                if rng.chance(2) {
                    active = !active;
                }
                let wants_key = viewers.iter().any(|v| v.selector.needs_keyframe());
                let can_key = last_key.is_none_or(|at| timestamp - at >= 2 * CLOCK);
                if last_key.is_none() || (wants_key || rng.chance(1)) && can_key {
                    since_key = 0;
                    last_key = Some(timestamp);
                }
                let now = Duration::from_micros(timestamp * 1_000_000 / CLOCK);
                if timestamp >= next_second {
                    next_second = timestamp + CLOCK;
                    if rng.chance(10) {
                        smooth = !smooth;
                    }
                    if viewers.len() < 4 && rng.chance(20) {
                        viewers.push(Viewer::default());
                    }
                    if !viewers.is_empty() && rng.chance(3) {
                        viewers.swap_remove(rng.below(viewers.len() as u64) as usize);
                    }
                    for viewer in &mut viewers {
                        if rng.chance(15) {
                            viewer.tier = rng.below(4) as usize;
                        }
                        if rng.chance(15) || viewer.budget == 0 {
                            viewer.budget =
                                [100_000, 400_000, 1_500_000, 8_000_000][rng.below(4) as usize];
                        }
                        if rng.chance(10) {
                            viewer.steps = rng.below(4) as usize;
                        }
                        second(viewer, &layers, smooth, now);
                    }
                }
                let mut marks = vec![(0, false, false); viewers.len()];
                for (spatial, template) in l2t2_unit(since_key).into_iter().enumerate() {
                    let number = unit.wrapping_mul(2).wrapping_add(spatial as u16);
                    let descriptor = l2t2_key(template, number, template == 0);
                    let size = [1500, 6000][spatial] * if template < 2 { 4 } else { 1 };
                    let frame = layers
                        .packet(&descriptor, size, timestamp, now)
                        .expect("valid");
                    assert_eq!(frame.layer.spatial as usize, spatial);
                    for (viewer, mark) in viewers.iter_mut().zip(&mut marks) {
                        if rng.chance(1) {
                            viewer.selector.dropped(&frame);
                            mark.1 = true;
                            continue;
                        }
                        let before = viewer.selector.current();
                        let Some(marker) = viewer.selector.forward(&frame, timestamp, spatial == 1)
                        else {
                            continue;
                        };
                        let current = viewer.selector.current().expect("a target");
                        assert_ne!(frame.dti(current), Dti::NotPresent, "a frame of its target");
                        if before != Some(current) {
                            assert!(frame.start && frame.dti(current) == Dti::Switch);
                            // The wanted target, or the current spatial layer's
                            // base while a spatial downswitch waits.
                            let to = Some(current);
                            assert!(
                                to == viewer.wanted.map(|w| w.0)
                                    || to == viewer.meanwhile.map(|w| w.0)
                            );
                        }
                        // The viewer holds the frame's chain: it can decode it.
                        let chain = frame.chain_diff(current).unwrap_or(0);
                        let previous = number.wrapping_sub(u16::from(chain));
                        assert!(
                            chain == 0 || viewer.held.contains(&previous),
                            "a held chain"
                        );
                        viewer.held.insert(number);
                        *mark = (mark.0 + u8::from(marker), mark.1, marker);
                    }
                }
                for (count, lost, last) in marks {
                    // A unit without a loss ends with one marker, on its last
                    // forwarded frame.
                    if !lost && count > 0 {
                        assert!(count == 1 && last, "one marker per unit");
                    }
                }
                since_key = since_key.wrapping_add(1);
            }
        }
    }
}
