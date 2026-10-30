//! Scripted actions as inline data, anchored to the run, never a wall clock. Any subset of a list
//! is a valid script, so a driver treats a resume with no pause as a no-op.

use crate::prng::Prng;
use crate::record::Recording;
use crate::time::Ns;

pub const MAX_ACTIONS: usize = 64;

/// `AfterEos::lag` reaches the few passes after an end of stream where a group changes phase.
/// `InFlush` is an app thread landing a command mid-pass, fired by the fake sink's flush handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    At(Ns),
    AfterBuffers { sink: u16, count: u32 },
    AfterEos { sink: u16, count: u16, lag: u16 },
    AtStep(u32),
    InFlush { sink: u16, count: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Seek {
        to: Ns,
    },
    Pause,
    Resume,
    Stop,
    /// Vocabulary only. Core's `Command` has no such variants, no family weights them, and the
    /// sink answers `NotDriven`. Stream selection against core is `topo`'s `Act::Switch`.
    SelectStream {
        stream: u16,
    },
    AttachTrack {
        track: u16,
    },
    DetachTrack {
        track: u16,
    },
    SetRate {
        num: u16,
        den: u16,
    },
    SetOffset {
        sink: u16,
        by: i64,
    },
    SkipTo {
        to: Ns,
    },
}

pub const RATES: [(u16, u16); 14] = [
    (1, 1),
    (1, 1),
    (2, 1),
    (1, 2),
    (3, 2),
    (2, 3),
    (3, 4),
    (5, 4),
    (4, 1),
    (1, 4),
    (21, 20),
    (64, 1),
    (1, 64),
    (65_535, 65_534),
];

pub const OFFSET_BOUND: Ns = 150_000_000;

const STRAY_PERCENT: u32 = 8;

impl ActionKind {
    pub const KINDS: usize = 10;

    pub const fn index(self) -> usize {
        match self {
            Self::Seek { .. } => 0,
            Self::Pause => 1,
            Self::Resume => 2,
            Self::Stop => 3,
            Self::SelectStream { .. } => 4,
            Self::AttachTrack { .. } => 5,
            Self::DetachTrack { .. } => 6,
            Self::SetRate { .. } => 7,
            Self::SetOffset { .. } => 8,
            Self::SkipTo { .. } => 9,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    pub anchor: Anchor,
    pub kind: ActionKind,
}

const _: () = assert!(core::mem::size_of::<Action>() == 32);

pub fn anchor_met(anchor: Anchor, now: Ns, recording: &Recording) -> bool {
    anchor_met_at(anchor, now, None, recording)
}

pub fn anchor_met_at(anchor: Anchor, now: Ns, step: Option<u32>, recording: &Recording) -> bool {
    match anchor {
        Anchor::At(t) => t <= now,
        Anchor::AfterBuffers { sink, count } => recording.buffers_on(sink) >= count,
        Anchor::AfterEos { sink, count, .. } => recording.ends_on(sink) >= count as u32,
        Anchor::AtStep(at) => step.is_some_and(|step| at <= step),
        Anchor::InFlush { .. } => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionError {
    Full,
}

impl core::fmt::Display for ActionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "an action list holds at most {MAX_ACTIONS} actions")
    }
}

impl std::error::Error for ActionError {}

#[derive(Clone, Copy, Debug)]
pub struct ActionList {
    items: [Action; MAX_ACTIONS],
    len: usize,
}

impl ActionList {
    pub const fn new() -> Self {
        let filler = Action { anchor: Anchor::At(0), kind: ActionKind::Pause };
        Self { items: [filler; MAX_ACTIONS], len: 0 }
    }

    pub fn from_slice(actions: &[Action]) -> Result<Self, ActionError> {
        let mut list = Self::new();
        for action in actions {
            list.push(*action)?;
        }
        Ok(list)
    }

    pub fn push(&mut self, action: Action) -> Result<(), ActionError> {
        let slot = self.items.get_mut(self.len).ok_or(ActionError::Full)?;
        *slot = action;
        self.len += 1;
        Ok(())
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn as_slice(&self) -> &[Action] {
        &self.items[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for ActionList {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for ActionList {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for ActionList {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenParams {
    pub count: u32,
    pub duration: Ns,
    pub sinks: u16,
    pub max_buffers: u32,
    pub buffer_anchor_percent: u32,
    pub streams: u16,
    pub tracks: u16,
    pub weights: [u32; ActionKind::KINDS],
}

impl GenParams {
    pub const PLAYBACK_WEIGHTS: [u32; ActionKind::KINDS] = [40, 15, 15, 2, 10, 9, 9, 0, 0, 0];
}

fn pick(weights: &[u32; ActionKind::KINDS], mut draw: u64) -> usize {
    for (index, weight) in weights.iter().enumerate() {
        if draw < *weight as u64 {
            return index;
        }
        draw -= *weight as u64;
    }
    0
}

/// Every field is drawn whether used or not, so one choice never shifts the draws behind it.
fn draw(rng: &mut Prng, p: &GenParams, weights: &[u32; ActionKind::KINDS], total: u64) -> Action {
    let wants_buffers = rng.chance(p.buffer_anchor_percent);
    let at = rng.below(p.duration.saturating_add(1));
    let sink = rng.below(p.sinks.max(1) as u64) as u16;
    let count = rng.range(1, p.max_buffers.max(1) as u64 + 1) as u32;
    let which = pick(weights, rng.below(total));
    let to = rng.below(p.duration.saturating_add(1));
    let stream = rng.below(p.streams.max(1) as u64) as u16;
    let track = rng.below(p.tracks.max(1) as u64) as u16;
    // Drawn behind everything older, so a script with none of these is as it was.
    let (num, den) = RATES[rng.below(RATES.len() as u64) as usize];
    let stray = rng.chance(STRAY_PERCENT);
    let by = rng.below(2 * OFFSET_BOUND + 1) as i64 - OFFSET_BOUND as i64;

    let anchor = if wants_buffers && p.sinks > 0 {
        Anchor::AfterBuffers { sink, count }
    } else {
        Anchor::At(at)
    };
    let kind = match which {
        0 => ActionKind::Seek { to },
        1 => ActionKind::Pause,
        2 => ActionKind::Resume,
        3 => ActionKind::Stop,
        4 => ActionKind::SelectStream { stream },
        5 => ActionKind::AttachTrack { track },
        6 => ActionKind::DetachTrack { track },
        7 => ActionKind::SetRate { num, den },
        8 => ActionKind::SetOffset { sink: if stray { p.sinks.max(1) } else { sink }, by },
        _ => ActionKind::SkipTo { to },
    };
    Action { anchor, kind }
}

/// Action `n` comes from its own substream, so more actions extend a script without changing it.
pub fn generate(seed: u64, p: &GenParams, out: &mut ActionList) {
    out.clear();
    let mut weights = p.weights;
    if p.streams == 0 {
        weights[4] = 0;
    }
    if p.tracks == 0 {
        weights[5] = 0;
        weights[6] = 0;
    }
    let total: u64 = weights.iter().map(|weight| *weight as u64).sum();
    if total == 0 {
        return;
    }
    let root = Prng::new(seed).substream_named("actions");
    for index in 0..(p.count as usize).min(MAX_ACTIONS) {
        let mut rng = root.substream(index as u64);
        let action = draw(&mut rng, p, &weights, total);
        // Cannot fail, the loop is bounded by the list's capacity.
        let _ = out.push(action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Row, RowKind};
    use crate::time::SEC;

    const PARAMS: GenParams = GenParams {
        count: 12,
        duration: 10 * SEC,
        sinks: 2,
        max_buffers: 200,
        buffer_anchor_percent: 40,
        streams: 3,
        tracks: 2,
        weights: GenParams::PLAYBACK_WEIGHTS,
    };

    fn generated(seed: u64, p: &GenParams) -> ActionList {
        let mut list = ActionList::new();
        generate(seed, p, &mut list);
        list
    }

    #[test]
    fn same_seed_same_script_and_other_seeds_differ() {
        assert_eq!(generated(5, &PARAMS), generated(5, &PARAMS));
        assert_eq!(generated(5, &PARAMS).len(), 12);
        let distinct =
            (0..50u64).filter(|seed| generated(*seed, &PARAMS) != generated(seed + 1, &PARAMS));
        assert_eq!(distinct.count(), 50);
    }

    #[test]
    fn a_longer_script_extends_a_shorter_one() {
        let short = generated(9, &GenParams { count: 5, ..PARAMS });
        let long = generated(9, &GenParams { count: 40, ..PARAMS });
        assert_eq!(short.as_slice(), &long.as_slice()[..5]);
    }

    #[test]
    fn a_weight_change_keeps_the_anchors() {
        let seeks_only = GenParams { weights: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0], ..PARAMS };
        let a = generated(3, &PARAMS);
        let b = generated(3, &seeks_only);
        for (x, y) in a.as_slice().iter().zip(b.as_slice()) {
            assert_eq!(x.anchor, y.anchor);
            assert!(matches!(y.kind, ActionKind::Seek { .. }));
        }
    }

    #[test]
    fn draws_respect_their_bounds() {
        let mut kinds = [0u32; ActionKind::KINDS];
        let mut buffer_anchors = 0;
        for seed in 0..400u64 {
            for action in generated(seed, &PARAMS).as_slice() {
                kinds[action.kind.index()] += 1;
                match action.anchor {
                    Anchor::At(t) => assert!(t <= PARAMS.duration),
                    Anchor::AfterBuffers { sink, count } => {
                        buffer_anchors += 1;
                        assert!(sink < PARAMS.sinks);
                        assert!((1..=PARAMS.max_buffers).contains(&count));
                    }
                    other => panic!("the generator draws no {other:?}"),
                }
                match action.kind {
                    ActionKind::Seek { to } => assert!(to <= PARAMS.duration),
                    ActionKind::SelectStream { stream } => assert!(stream < PARAMS.streams),
                    ActionKind::AttachTrack { track } | ActionKind::DetachTrack { track } => {
                        assert!(track < PARAMS.tracks)
                    }
                    _ => {}
                }
            }
        }
        assert!(kinds[..7].iter().all(|count| *count > 0), "{kinds:?}");
        assert!(kinds[0] > kinds[3] * 5, "seeks should dwarf stops, {kinds:?}");
        // 40 percent of 4800 anchors.
        assert!((1_600..2_240).contains(&buffer_anchors), "{buffer_anchors}");
    }

    #[test]
    fn disabled_kinds_and_anchors_are_never_drawn() {
        let bare = GenParams { sinks: 0, streams: 0, tracks: 0, ..PARAMS };
        for seed in 0..200u64 {
            for action in generated(seed, &bare).as_slice() {
                assert!(matches!(action.anchor, Anchor::At(_)));
                assert!(action.kind.index() < 4, "{action:?}");
            }
        }
        let timed = GenParams { weights: [0, 0, 0, 0, 0, 0, 0, 3, 2, 1], ..PARAMS };
        let (mut rates, mut strays, mut early) = (0, 0, 0);
        for seed in 0..200u64 {
            for action in generated(seed, &timed).as_slice() {
                match action.kind {
                    ActionKind::SetRate { num, den } => {
                        rates += 1;
                        assert!(RATES.contains(&(num, den)));
                    }
                    ActionKind::SetOffset { sink, by } => {
                        assert!(sink <= PARAMS.sinks && by.unsigned_abs() <= OFFSET_BOUND);
                        strays += (sink == PARAMS.sinks) as u32;
                        early += (by < 0) as u32;
                    }
                    ActionKind::SkipTo { to } => assert!(to <= PARAMS.duration),
                    other => panic!("the weights draw no {other:?}"),
                }
            }
        }
        assert!(rates > 1_000 && strays > 20 && early > 200, "{rates} {strays} {early}");
        for seed in 0..200u64 {
            for action in generated(seed, &PARAMS).as_slice() {
                assert!(action.kind.index() < 7, "{action:?}");
            }
        }
        let nothing = GenParams { weights: [0; ActionKind::KINDS], ..PARAMS };
        assert!(generated(1, &nothing).is_empty());
        let capped = GenParams { count: 1_000, ..PARAMS };
        assert_eq!(generated(1, &capped).len(), MAX_ACTIONS);
    }

    #[test]
    fn a_list_is_bounded_and_compares_by_content() {
        let pause = Action { anchor: Anchor::At(1), kind: ActionKind::Pause };
        let mut list = ActionList::new();
        for _ in 0..MAX_ACTIONS {
            list.push(pause).unwrap();
        }
        assert_eq!(list.push(pause), Err(ActionError::Full));
        assert_eq!(list.len(), MAX_ACTIONS);
        list.clear();
        assert!(list.is_empty());
        assert_eq!(list, ActionList::new());
        assert_eq!(ActionList::from_slice(&[pause]).unwrap().as_slice(), &[pause]);
    }

    #[test]
    fn anchors_hold_on_time_and_on_buffer_counts() {
        let mut rec = Recording::with_capacity(8);
        assert!(!anchor_met(Anchor::At(5), 4, &rec));
        assert!(anchor_met(Anchor::At(5), 5, &rec));
        let two_on_one = Anchor::AfterBuffers { sink: 1, count: 2 };
        rec.push(Row::new(RowKind::Buffer, 1, 0));
        rec.push(Row::new(RowKind::Buffer, 0, 0));
        assert!(!anchor_met(two_on_one, 0, &rec));
        rec.push(Row::new(RowKind::Buffer, 1, 0));
        assert!(anchor_met(two_on_one, 0, &rec));
    }

    #[test]
    fn anchors_hold_on_ends_and_steps_and_never_on_a_flush() {
        let mut rec = Recording::with_capacity(8);
        let ended = Anchor::AfterEos { sink: 1, count: 1, lag: 3 };
        assert!(!anchor_met(ended, 0, &rec));
        rec.push(Row::new(RowKind::Eos, 0, 0));
        assert!(!anchor_met(ended, 0, &rec));
        rec.push(Row::new(RowKind::Eos, 1, 0));
        assert!(anchor_met(ended, 0, &rec));
        assert!(!anchor_met(Anchor::AtStep(0), 0, &rec), "no passes, no step");
        assert!(!anchor_met_at(Anchor::AtStep(4), 0, Some(3), &rec));
        assert!(anchor_met_at(Anchor::AtStep(4), 0, Some(4), &rec));
        let inside = Anchor::InFlush { sink: 0, count: 1 };
        rec.push(Row::new(RowKind::FlushStart, 0, 0));
        assert!(!anchor_met_at(inside, u64::MAX, Some(u32::MAX), &rec));
    }
}
