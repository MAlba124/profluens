//! One run, declared as plain data from its seed. Each component draws from its own named
//! substream, so a knob added later leaves every seed's other draws where they were.

use core::fmt;

use super::gop::{self, GopDecl, How, Pictures};
use super::net::{NetDecl, draw_net};
use super::offers::{self, Listed, Offers, Plan};
use super::retime::{LATENCIES, MAX_LAG, RetimeDecl};
use crate::action::{self, Action, ActionKind, ActionList, Anchor, GenParams};
use crate::media::{
    Cue, CueRange, Fault, MediaSpec, Pacing, SpecError, StreamFault, StreamKind, StreamSpec,
};
use crate::prng::Prng;
use crate::profile::{self, SinkProfile};
use crate::time::{MS, NONE, Ns, Rational, SEC, frame_pts, frames_in};

pub const MAX_STREAMS: usize = 2;
pub const MAX_SINKS: usize = 2;
pub const MAX_FAULTS: usize = 4;
pub const MAX_CUES: usize = 8;
pub const MAX_WINDOW: u8 = 3;
pub const MAX_DELAY: u8 = 3;
pub const MAX_SWING: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Topology {
    Line = 0,
    TwoStreams = 1,
    Transform = 2,
    Inline = 3,
    FanOut = 4,
    TwoStreamsTransform = 5,
}

impl Topology {
    pub const ALL: [Topology; 6] = [
        Topology::Line,
        Topology::TwoStreams,
        Topology::Transform,
        Topology::Inline,
        Topology::FanOut,
        Topology::TwoStreamsTransform,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::TwoStreams => "two-streams",
            Self::Transform => "transform",
            Self::Inline => "inline-transform",
            Self::FanOut => "fan-out",
            Self::TwoStreamsTransform => "two-streams-transform",
        }
    }

    pub const fn streams(self) -> usize {
        match self {
            Self::TwoStreams | Self::TwoStreamsTransform => 2,
            _ => 1,
        }
    }

    pub const fn sinks(self) -> usize {
        match self {
            Self::TwoStreams | Self::FanOut | Self::TwoStreamsTransform => 2,
            _ => 1,
        }
    }

    pub const fn has_transform(self) -> bool {
        !matches!(self, Self::Line | Self::TwoStreams)
    }

    pub const fn stream_of(self, sink: usize) -> u16 {
        match self {
            Self::TwoStreams | Self::TwoStreamsTransform => sink as u16,
            _ => 0,
        }
    }

    pub const fn through_transform(self, sink: usize) -> bool {
        match self {
            Self::Transform | Self::Inline | Self::FanOut => true,
            Self::TwoStreamsTransform => sink == 0,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceDecl {
    pub streams: [StreamSpec; MAX_STREAMS],
    pub stream_count: u8,
    pub cues: [Cue; MAX_CUES],
    pub cue_count: u8,
    pub faults: [StreamFault; MAX_FAULTS],
    pub fault_count: u8,
    /// Per stream, the buffer from which it carries its second format. `NONE` is one format.
    pub format_change: [u64; MAX_STREAMS],
    pub burst: u8,
    /// Per stream, how far ahead in file order it is served, as in a far-apart file.
    pub skew: [Ns; MAX_STREAMS],
    pub swing: u8,
    /// Describes its streams as a demuxer does, a text stream sparse with a gap every half second.
    pub described: bool,
}

impl SourceDecl {
    pub fn fill(&self, spec: &mut MediaSpec) -> Result<(), SpecError> {
        spec.reset();
        let streams = (self.stream_count as usize).min(MAX_STREAMS);
        for stream in &self.streams[..streams] {
            match stream.kind {
                StreamKind::Text { .. } => {
                    let cues = &self.cues[..(self.cue_count as usize).min(MAX_CUES)];
                    spec.add_text(cues, stream.pacing)?;
                }
                _ => {
                    spec.add_stream(*stream)?;
                }
            }
        }
        for entry in &self.faults[..(self.fault_count as usize).min(MAX_FAULTS)] {
            spec.add_fault(entry.stream, entry.fault)?;
        }
        Ok(())
    }

    pub fn add_fault(&mut self, stream: u16, fault: Fault) -> bool {
        let Some(slot) = self.faults.get_mut(self.fault_count as usize) else {
            return false;
        };
        *slot = StreamFault { stream, fault };
        self.fault_count += 1;
        true
    }

    pub fn buffers(&self, stream: usize) -> u64 {
        let Some(s) = self.streams.get(stream) else {
            return 0;
        };
        match s.kind {
            StreamKind::Video { fps, .. } => frames_in(s.duration, fps),
            StreamKind::Audio { packet } => s.duration.checked_div(packet).unwrap_or(0),
            StreamKind::Text { .. } => self.cue_count as u64,
        }
    }

    pub fn error_stream(&self) -> Option<u16> {
        self.faults[..(self.fault_count as usize).min(MAX_FAULTS)]
            .iter()
            .find_map(|entry| matches!(entry.fault, Fault::ErrorAt { .. }).then_some(entry.stream))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransformDecl {
    pub delay: u8,
    /// Buffers leave in seeded order within blocks of `window + 1`. Zero keeps the order.
    pub window: u8,
    /// Fail on taking this input buffer, counted from 1 over the whole run. `NONE` never fails.
    pub error_at: u64,
    pub seed: u64,
}

impl TransformDecl {
    pub const PLAIN: Self = Self { delay: 0, window: 0, error_at: NONE, seed: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SinkDecl {
    pub profile: SinkProfile,
    pub leaky: bool,
    pub queue: u8,
    /// What the link into the sink holds at most, in stream time. Zero is no limit.
    pub limit: Ns,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Told {
    Each,
    First,
    Second,
    All,
    Planned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeDecl {
    pub on: bool,
    pub retimer: RetimeDecl,
    pub two_sources: bool,
    pub forced: Option<Told>,
}

impl TimeDecl {
    pub const OFF: Self =
        Self { on: false, retimer: RetimeDecl::OFF, two_sources: false, forced: None };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scenario {
    pub seed: u64,
    pub topology: Topology,
    pub source: SourceDecl,
    pub transform: TransformDecl,
    pub sinks: [SinkDecl; MAX_SINKS],
    pub offers: Offers,
    pub gop: GopDecl,
    pub time: TimeDecl,
    pub net: NetDecl,
    pub pool_slots: u32,
    pub transform_queue: u8,
    pub spurious: u32,
    /// One pass in this many is cut after its command drain and finished later. Zero is never.
    pub split: u32,
    pub start_paused: bool,
    pub script: ActionList,
    pub step_limit: u32,
}

impl Scenario {
    pub fn sparse(&self, sink: usize) -> bool {
        let kind = self.source.streams[self.topology.stream_of(sink) as usize].kind;
        self.source.described && matches!(kind, StreamKind::Text { .. })
    }

    pub fn sink_count(&self) -> usize {
        self.topology.sinks()
    }

    pub fn plan(&self) -> Plan {
        Plan::new(self.topology, &self.offers, &self.source.format_change)
    }

    pub fn pictures(&self) -> Pictures {
        Pictures::of(&self.source.streams[0], &self.gop)
    }

    pub fn decoded(&self, sink: usize) -> bool {
        self.gop.on && self.topology.through_transform(sink)
    }

    pub fn how(&self, action: &Action) -> Option<How> {
        let seeks = matches!(action.kind, ActionKind::Seek { .. });
        (self.gop.on && seeks).then(|| gop::how(self.seed, &self.gop, action))
    }

    pub fn retimed(&self, sink: usize) -> bool {
        self.time.retimer.on && self.topology.through_transform(sink)
    }

    pub fn sink_step(&self, sink: usize) -> Option<Ns> {
        buffer_step(self.source.streams[self.topology.stream_of(sink) as usize].kind)
    }

    pub fn latency(&self, sink: usize) -> Ns {
        if self.retimed(sink) { self.time.retimer.latency } else { 0 }
    }

    pub fn told(&self, action: &Action) -> Option<Told> {
        let seeks = matches!(action.kind, ActionKind::Seek { .. });
        if !self.time.two_sources || !seeks {
            return None;
        }
        let key = gop::key_of(action);
        let draw = Prng::new(self.seed).substream_named("time.told").substream(key).below(100);
        let told = self.time.forced.unwrap_or(match draw {
            0..=39 => Told::Each,
            40..=54 => Told::Planned,
            55..=69 => Told::First,
            70..=84 => Told::Second,
            _ => Told::All,
        });
        // A text stream has no grid to index.
        let gridded =
            self.source.streams[..2].iter().all(|s| !matches!(s.kind, StreamKind::Text { .. }));
        Some(if told == Told::Planned && !gridded { Told::Each } else { told })
    }

    pub fn reads(&self, to: Ns) -> [(u64, Ns); MAX_STREAMS] {
        core::array::from_fn(|stream| {
            let grid = Pictures::of(&self.source.streams[stream], &GopDecl::OFF);
            let byte = grid.key_of(grid.at_or_before(to));
            (byte, grid.pts(byte))
        })
    }

    pub fn lossy(&self, sink: usize) -> bool {
        self.sinks.get(sink).is_some_and(|s| s.leaky)
    }

    pub fn window(&self, sink: usize) -> u8 {
        if self.topology.through_transform(sink) && self.topology != Topology::FanOut {
            self.transform.window.min(MAX_WINDOW)
        } else {
            0
        }
    }

    pub fn declares_error(&self) -> bool {
        let transform = self.topology.has_transform() && self.transform.error_at != NONE;
        transform || self.source.error_stream().is_some()
    }

    pub fn total_buffers(&self) -> u64 {
        (0..self.source.stream_count as usize).map(|s| self.source.buffers(s)).sum()
    }

    /// The step bound grows with the script and never shrinks, so a cut-down script keeps the
    /// bound it failed under.
    pub fn with_script(&self, actions: &[Action]) -> Scenario {
        let mut script = ActionList::new();
        for action in actions {
            let _ = script.push(*action);
        }
        let limit = step_limit(self.total_buffers(), script.len()).max(self.step_limit);
        Scenario { script, step_limit: limit, ..*self }
    }
}

const FILLER_STREAM: StreamSpec = StreamSpec {
    kind: StreamKind::Audio { packet: 20 * MS },
    duration: 0,
    size: 0,
    pacing: Pacing::AsFastAsPossible,
};
const FILLER_CUE: Cue = Cue { start: 0, end: 0, size: 0 };
const FILLER_FAULT: StreamFault = StreamFault { stream: 0, fault: Fault::LatePad { delay: 0 } };

fn video(rng: &mut Prng, duration: Ns) -> StreamSpec {
    const RATES: [Rational; 3] =
        [Rational::new(25, 1), Rational::new(30_000, 1_001), Rational::new(50, 1)];
    const KEYS: [u32; 4] = [1, 5, 12, 25];
    let fps = RATES[rng.below(RATES.len() as u64) as usize];
    let keyframe_interval = KEYS[rng.below(KEYS.len() as u64) as usize];
    let kind = StreamKind::Video { fps, keyframe_interval };
    StreamSpec { kind, duration, size: 4096, pacing: Pacing::AsFastAsPossible }
}

fn audio(rng: &mut Prng, duration: Ns) -> StreamSpec {
    const PACKETS: [Ns; 3] = [10 * MS, 20 * MS, 23_219_955];
    let packet = PACKETS[rng.below(PACKETS.len() as u64) as usize];
    let kind = StreamKind::Audio { packet };
    StreamSpec { kind, duration, size: 256, pacing: Pacing::AsFastAsPossible }
}

fn text(rng: &mut Prng, duration: Ns, source: &mut SourceDecl) -> StreamSpec {
    let count = rng.range(3, MAX_CUES as u64 + 1);
    let gap = (duration / count).max(1);
    for i in 0..count {
        let start = i * gap + rng.below(gap / 2 + 1);
        let end = start + rng.range(1, gap * 2);
        source.cues[i as usize] = Cue { start, end, size: 32 };
    }
    source.cue_count = count as u8;
    let kind = StreamKind::Text { cues: CueRange { first: 0, len: count as u32 } };
    StreamSpec { kind, duration, size: 0, pacing: Pacing::AsFastAsPossible }
}

fn draw_faults(rng: &mut Prng, source: &mut SourceDecl, stream: u16) {
    let buffers = source.buffers(stream as usize).max(1);
    let (stall, hold) = (rng.below(buffers), rng.range(5 * MS, 500 * MS));
    let (early, error) = (rng.below(buffers), rng.below(buffers));
    let which = rng.below(100);
    if which < 22 {
        source.add_fault(stream, Fault::StallAt { buffer: stall, hold });
    }
    if (15..27).contains(&which) {
        source.add_fault(stream, Fault::EndEarlyAt { buffer: early });
    }
    if (27..31).contains(&which) {
        source.add_fault(stream, Fault::ErrorAt { buffer: error });
    }
}

fn draw_source(root: &Prng, topology: Topology) -> SourceDecl {
    let mut source = SourceDecl {
        streams: [FILLER_STREAM; MAX_STREAMS],
        stream_count: topology.streams() as u8,
        cues: [FILLER_CUE; MAX_CUES],
        cue_count: 0,
        faults: [FILLER_FAULT; MAX_FAULTS],
        fault_count: 0,
        format_change: [NONE; MAX_STREAMS],
        burst: root.substream_named("burst").range(1, 5) as u8,
        skew: [0; MAX_STREAMS],
        swing: 0,
        described: false,
    };
    let mut media = root.substream_named("media");
    let duration = media.range(100 * MS, 1_200 * MS);
    source.streams[0] = match media.below(3) {
        0 => audio(&mut media, duration),
        _ => video(&mut media, duration),
    };
    if topology.streams() == 2 {
        source.streams[1] = match media.below(4) {
            0 => text(&mut media, duration, &mut source),
            _ => audio(&mut media, duration),
        };
    }
    for stream in 0..topology.streams() as u16 {
        let mut rng = root.substream_named("faults").substream(stream as u64);
        draw_faults(&mut rng, &mut source, stream);
        let mut rng = root.substream_named("format").substream(stream as u64);
        let at = rng.below(source.buffers(stream as usize).max(1));
        if rng.chance(25) {
            source.format_change[stream as usize] = at;
        }
    }
    source
}

fn draw_transform(root: &Prng, topology: Topology, buffers: u64) -> TransformDecl {
    let mut rng = root.substream_named("transform");
    let (delay, window) = (rng.below(MAX_DELAY as u64 + 1), rng.below(MAX_WINDOW as u64 + 1));
    let (reorders, fails) = (rng.chance(40), rng.chance(4));
    let error_at = rng.range(1, buffers.max(1) + 1);
    let plain = topology == Topology::FanOut;
    TransformDecl {
        delay: if plain { 0 } else { delay as u8 },
        window: if plain || !reorders { 0 } else { window as u8 },
        error_at: if fails { error_at } else { NONE },
        seed: rng.next_u64(),
    }
}

pub fn buffer_step(kind: StreamKind) -> Option<Ns> {
    match kind {
        StreamKind::Video { fps, .. } => Some(frame_pts(1, fps).max(1)),
        StreamKind::Audio { packet } => Some(packet.max(1)),
        StreamKind::Text { .. } => None,
    }
}

fn draw_sink(root: &Prng, seed: u64, sink: u64) -> SinkDecl {
    let mut rng = root.substream_named("sink").substream(sink);
    let device = profile::device(seed ^ sink.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let profile = match rng.below(10) {
        0..=2 => SinkProfile::IDEAL,
        3..=6 => SinkProfile { lateness_bound: NONE, ..device },
        _ => device,
    };
    let leaky = rng.chance(8);
    let queue = if rng.chance(50) { rng.range(1, 7) as u8 } else { 0 };
    SinkDecl { profile, leaky, queue, limit: 0 }
}

struct ScriptShape {
    sinks: u16,
    duration: Ns,
    buffers: u64,
    count: u32,
    weights: [u32; ActionKind::KINDS],
}

fn draw_script(root: &Prng, seed: u64, shape: &ScriptShape, moved_percent: u32) -> ActionList {
    let params = GenParams {
        count: shape.count,
        duration: shape.duration.saturating_add(shape.duration / 4),
        sinks: shape.sinks,
        max_buffers: shape.buffers.clamp(1, u32::MAX as u64) as u32,
        buffer_anchor_percent: 45,
        streams: 0,
        tracks: 0,
        weights: shape.weights,
    };
    let mut drawn = ActionList::new();
    action::generate(seed, &params, &mut drawn);
    let mut script = ActionList::new();
    let horizon = shape.buffers.saturating_mul(6).saturating_add(40);
    for (i, action) in drawn.as_slice().iter().enumerate() {
        let mut rng = root.substream_named("anchor").substream(i as u64);
        let (moved, which) = (rng.chance(moved_percent), rng.below(8));
        let sink = rng.below(shape.sinks.max(1) as u64) as u16;
        let (step, count) = (rng.below(horizon) as u32, rng.range(1, 4) as u32);
        let lag = rng.below(7) as u16;
        let anchor = match which {
            _ if !moved => action.anchor,
            0..=3 => Anchor::AtStep(step),
            4..=6 => Anchor::AfterEos { sink, count: count.min(2) as u16, lag },
            _ => Anchor::InFlush { sink, count },
        };
        // Cannot fail, both lists have the same capacity.
        let _ = script.push(Action { anchor, kind: action.kind });
    }
    script
}

/// Generous, since reaching it is a verdict. A cut pass takes two steps.
fn step_limit(buffers: u64, actions: usize) -> u32 {
    let per_generation = buffers.saturating_mul(48).saturating_add(2_000);
    per_generation.saturating_mul(actions as u64 + 2).min(u32::MAX as u64 / 2) as u32
}

const PLAYS: [u32; ActionKind::KINDS] = [45, 22, 22, 2, 0, 0, 0, 0, 0, 0];
const RATES_ONLY: [u32; ActionKind::KINDS] = [45, 22, 22, 2, 0, 0, 0, 40, 0, 0];
const TIMED: [u32; ActionKind::KINDS] = [30, 14, 14, 2, 0, 0, 0, 50, 14, 10];

const TIMED_PERCENT: u32 = 30;

pub fn draw(seed: u64) -> Scenario {
    draw_with(seed, true, true, true, true)
}

/// The draw a seed made before the source could be fed over a network.
pub fn draw_offline(seed: u64) -> Scenario {
    draw_with(seed, true, true, true, false)
}

/// The draw a seed made before there was a timeline.
pub fn draw_untimed(seed: u64) -> Scenario {
    draw_with(seed, true, true, false, false)
}

/// The draw a seed made before seeks went through the rule.
pub fn draw_undecoded(seed: u64) -> Scenario {
    draw_with(seed, true, false, false, false)
}

/// The draw a seed made before pads had offers.
pub fn draw_open(seed: u64) -> Scenario {
    draw_with(seed, false, false, false, false)
}

/// The buffering knobs on. Every other draw is the seed's in [`draw`].
pub fn draw_buffering(seed: u64) -> Scenario {
    let mut sc = draw(seed);
    let mut rng = Prng::new(seed).substream_named("driver.buffering");
    let duration = sc.source.streams[0].duration;
    if sc.topology.streams() == 2 && rng.chance(60) {
        sc.source.skew[rng.below(2) as usize] = rng.range(MS, 3 * duration);
    }
    if rng.chance(40) {
        sc.source.swing = rng.range(2, MAX_SWING as u64 + 1) as u8;
    }
    let limited = rng.chance(60);
    for n in 0..sc.sink_count() {
        let shuffled = sc.gop.on || (sc.transform.window > 0 && sc.topology.through_transform(n));
        let step = buffer_step(sc.source.streams[sc.topology.stream_of(n) as usize].kind);
        let buffers = rng.range(1, 6);
        if let (true, false, false, Some(step)) = (limited, shuffled, sc.sinks[n].leaky, step) {
            sc.sinks[n].limit = buffers * step;
        }
    }
    let plain = sc.offers.is_open() && !sc.gop.on && !sc.net.on && !sc.time.on;
    sc.source.described = plain && rng.chance(50);
    sc
}

fn draw_time(root: &Prng, topology: Topology, source: &SourceDecl, plain: bool) -> TimeDecl {
    let mut rng = root.substream_named("timeline");
    let on = rng.chance(TIMED_PERCENT);
    let (retimes, two) = (rng.chance(55), rng.chance(50));
    let lag = rng.below(MAX_LAG as u64 + 1) as u8;
    let latency = LATENCIES[rng.below(LATENCIES.len() as u64) as usize];
    let seed = rng.next_u64();
    let placed = topology.has_transform() && topology != Topology::FanOut;
    let streams = &source.streams[..topology.streams().min(MAX_STREAMS)];
    let text = streams.iter().any(|s| matches!(s.kind, StreamKind::Text { .. }));
    let retimer = match on && plain && retimes && placed {
        true => RetimeDecl { on: true, lag, latency, seed },
        false => RetimeDecl::OFF,
    };
    let two_sources = on && plain && two && topology == Topology::TwoStreams && !text;
    TimeDecl { on, retimer, two_sources, forced: None }
}

/// An offset names an element, which a sink does not know inside its flush, so it waits for
/// buffers.
fn offsets_outside_flushes(script: &ActionList) -> ActionList {
    let mut moved = ActionList::new();
    for action in script.as_slice() {
        let anchor = match (action.anchor, action.kind) {
            (Anchor::InFlush { sink, count }, ActionKind::SetOffset { .. }) => {
                Anchor::AfterBuffers { sink, count }
            }
            (other, _) => other,
        };
        // Cannot fail, both lists have the same capacity.
        let _ = moved.push(Action { anchor, kind: action.kind });
    }
    moved
}

fn draw_gop(root: &Prng, topology: Topology, offers: &Offers) -> GopDecl {
    let gop = gop::draw(root);
    let placed = topology.has_transform() && topology != Topology::FanOut && offers.is_open();
    GopDecl { on: gop.on && placed, ..gop }
}

/// A sink cannot ask the rule inside its flush, so with a decoder the action waits for buffers.
fn outside_flushes(script: &ActionList) -> ActionList {
    let mut moved = ActionList::new();
    for action in script.as_slice() {
        let anchor = match action.anchor {
            Anchor::InFlush { sink, count } => Anchor::AfterBuffers { sink, count },
            other => other,
        };
        // Cannot fail, both lists have the same capacity.
        let _ = moved.push(Action { anchor, kind: action.kind });
    }
    moved
}

fn draw_offers(root: &Prng, topology: Topology, source: &mut SourceDecl) -> Offers {
    let offers = offers::draw(root, topology);
    if offers.is_open() {
        return offers;
    }
    for stream in 0..topology.streams() {
        let mut rng = root.substream_named("offers.change").substream(stream as u64);
        let at = rng.below(source.buffers(stream).max(1));
        if rng.chance(60) {
            source.format_change[stream] = at;
        }
    }
    offers
}

fn draw_with(seed: u64, offered: bool, decoded: bool, timed: bool, networked: bool) -> Scenario {
    let root = Prng::new(seed).substream_named("driver.scenario");
    let pick = root.substream_named("topology").below(Topology::ALL.len() as u64);
    let topology = Topology::ALL[pick as usize];
    let mut source = draw_source(&root, topology);
    let offers = match offered {
        true => draw_offers(&root, topology, &mut source),
        false => Offers::OPEN,
    };
    let buffers: u64 = (0..topology.streams()).map(|s| source.buffers(s)).sum();
    let gop = match decoded {
        true => draw_gop(&root, topology, &offers),
        false => GopDecl::OFF,
    };
    let time = match timed {
        true => draw_time(&root, topology, &source, offers.is_open() && !gop.on),
        false => TimeDecl::OFF,
    };
    let transform = match topology.has_transform() && !gop.on && !time.retimer.on {
        true => draw_transform(&root, topology, buffers),
        false => TransformDecl::PLAIN,
    };
    let mut knobs = root.substream_named("knobs");
    const POOLS: [u32; 7] = [2, 3, 4, 5, 8, 16, 64];
    // What the transform holds back never comes out of the slots the rest needs.
    // Nor does what the decoder holds back in its place.
    let reorder = if gop.on { gop.reorder as u32 } else { 0 };
    let held = transform.delay as u32 + transform.window as u32 + reorder;
    let pool_slots = POOLS[knobs.below(POOLS.len() as u64) as usize] + held;
    // One source over a network, never two. A receive and the request each hold a slot.
    let two = time.two_sources && topology == Topology::TwoStreams;
    let net = if two || !networked { NetDecl::OFF } else { draw_net(&root) };
    let pool_slots = pool_slots + 2 * net.on as u32;
    let transform_queue = if knobs.chance(50) { knobs.range(1, 7) as u8 } else { 0 };
    let spurious = if knobs.chance(30) { knobs.range(3, 12) as u32 } else { 0 };
    let start_paused = knobs.chance(8);
    // Its own substream, so seeds that never cut a pass run as before.
    let mut cuts = root.substream_named("split");
    let split = if cuts.chance(50) { cuts.range(2, 9) as u32 } else { 0 };
    let shape = ScriptShape {
        sinks: topology.sinks() as u16,
        duration: source.streams[0].duration,
        buffers: source.buffers(0).max(1),
        count: root.substream_named("script").below(13) as u32,
        weights: match (time.on, gop.on) {
            (false, _) => PLAYS,
            (true, true) => RATES_ONLY,
            (true, false) => TIMED,
        },
    };
    let script = draw_script(&root, seed, &shape, 45);
    let script = if gop.on { outside_flushes(&script) } else { script };
    let script = if time.on { offsets_outside_flushes(&script) } else { script };
    let mut sinks = [draw_sink(&root, seed, 0), draw_sink(&root, seed, 1)];
    // A level counts by stamps, so only where they run in order a buffer apart. No decoder, no
    // shuffling transform, no cues.
    let mut limits = root.substream_named("limit");
    for (n, sink) in sinks.iter_mut().enumerate() {
        let (limited, buffers) = (limits.chance(40), limits.range(1, 6));
        let shuffled = gop.on || (transform.window > 0 && topology.through_transform(n));
        let step = buffer_step(source.streams[topology.stream_of(n) as usize].kind);
        if let (true, false, false, Some(step)) = (limited, shuffled, sink.leaky, step) {
            sink.limit = buffers * step;
        }
    }
    Scenario {
        seed,
        topology,
        source,
        transform,
        sinks,
        offers,
        gop,
        time,
        net,
        pool_slots,
        transform_queue,
        spurious,
        split,
        start_paused,
        step_limit: step_limit(buffers, script.len()),
        script,
    }
}

pub fn plain(seed: u64, topology: Topology, duration: Ns) -> Scenario {
    let video = StreamKind::Video { fps: Rational::new(25, 1), keyframe_interval: 25 };
    let audio = StreamKind::Audio { packet: 20 * MS };
    let stream = |kind, size| StreamSpec { kind, duration, size, pacing: Pacing::AsFastAsPossible };
    let source = SourceDecl {
        streams: [stream(video, 4096), stream(audio, 256)],
        stream_count: topology.streams() as u8,
        cues: [FILLER_CUE; MAX_CUES],
        cue_count: 0,
        faults: [FILLER_FAULT; MAX_FAULTS],
        fault_count: 0,
        format_change: [NONE; MAX_STREAMS],
        burst: 4,
        skew: [0; MAX_STREAMS],
        swing: 0,
        described: false,
    };
    let buffers: u64 = (0..topology.streams()).map(|s| source.buffers(s)).sum();
    let sink = SinkDecl { profile: SinkProfile::IDEAL, leaky: false, queue: 0, limit: 0 };
    Scenario {
        seed,
        topology,
        source,
        transform: TransformDecl::PLAIN,
        sinks: [sink; MAX_SINKS],
        offers: Offers::OPEN,
        gop: GopDecl::OFF,
        time: TimeDecl::OFF,
        net: NetDecl::OFF,
        pool_slots: 16,
        transform_queue: 0,
        spurious: 0,
        split: 0,
        start_paused: false,
        script: ActionList::new(),
        step_limit: step_limit(buffers, 0),
    }
}

/// The reference scenario of the speed budget.
pub fn reference(seed: u64) -> Scenario {
    let root = Prng::new(seed).substream_named("driver.reference");
    let mut sc = plain(seed, Topology::TwoStreams, 10 * SEC);
    for (n, sink) in sc.sinks.iter_mut().enumerate() {
        sink.profile = profile::device(seed ^ n as u64);
    }
    let shape = ScriptShape {
        sinks: 2,
        duration: 10 * SEC,
        buffers: 250,
        count: 10,
        weights: [40, 15, 15, 0, 0, 0, 0, 0, 0, 0],
    };
    let script = draw_script(&root, seed, &shape, 0);
    sc.with_script(script.as_slice())
}

struct Ts(Ns);

impl fmt::Display for Ts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            NONE => f.write_str("none"),
            ns => write!(f, "{}.{:06}ms", ns / MS, ns % MS),
        }
    }
}

struct Carried(Option<How>);

impl fmt::Display for Carried {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(How::Planned { keyframe }) => {
                let mode = if keyframe { "its keyframe" } else { "the time" };
                write!(f, " by the rule, to {mode}")
            }
            Some(How::RawSkip) => f.write_str(" as a skip, whatever the rule says"),
            Some(How::RawSeek { pick }) => {
                write!(f, " as a seek to a byte of its own, choice {pick}")
            }
            None => Ok(()),
        }
    }
}

pub struct Script<'a>(pub &'a [Action], pub Option<&'a Scenario>);

impl fmt::Display for Script<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("  (no action)\n");
        }
        for (i, action) in self.0.iter().enumerate() {
            write!(f, "  {i:2} ")?;
            match action.anchor {
                Anchor::At(t) => write!(f, "at {}", Ts(t))?,
                Anchor::AfterBuffers { sink, count } => {
                    write!(f, "after {count} buffers on sink {sink}")?
                }
                Anchor::AfterEos { sink, count, lag } => {
                    write!(f, "{lag} steps after end of stream {count} on sink {sink}")?
                }
                Anchor::AtStep(step) => write!(f, "before step {step}")?,
                Anchor::InFlush { sink, count } => {
                    write!(f, "inside flush {count} of sink {sink}")?
                }
            }
            let how = self.1.and_then(|sc| sc.how(action));
            let told = match self.1.and_then(|sc| sc.told(action)) {
                Some(Told::Each) => ", each source told its byte",
                Some(Told::First) => ", only the first source told its byte",
                Some(Told::Second) => ", only the second source told its byte",
                Some(Told::All) => ", one byte for both sources",
                Some(Told::Planned) => ", each source's byte planned from its index",
                None => "",
            };
            match action.kind {
                ActionKind::Seek { to } => {
                    writeln!(f, ", seek to {}{}{told}", Ts(to), Carried(how))?
                }
                ActionKind::Pause => writeln!(f, ", pause")?,
                ActionKind::Resume => writeln!(f, ", resume")?,
                ActionKind::Stop => writeln!(f, ", stop")?,
                ActionKind::SetRate { num, den } => writeln!(f, ", rate {num}/{den}")?,
                ActionKind::SetOffset { sink, by } => {
                    writeln!(f, ", offset sink {sink} by {by}ns")?
                }
                ActionKind::SkipTo { to } => writeln!(f, ", skip to {}", Ts(to))?,
                other => writeln!(f, ", {other:?} (not driven)")?,
            }
        }
        Ok(())
    }
}

fn write_stream(f: &mut fmt::Formatter<'_>, n: usize, source: &SourceDecl) -> fmt::Result {
    let s = &source.streams[n];
    write!(f, "  stream {n} ")?;
    match s.kind {
        StreamKind::Video { fps, keyframe_interval } => {
            write!(f, "video {}/{} fps, key every {keyframe_interval}", fps.num, fps.den)?
        }
        StreamKind::Audio { packet } => write!(f, "audio, packets of {}", Ts(packet))?,
        StreamKind::Text { cues } => write!(f, "text, {} cues", cues.len)?,
    }
    write!(f, ", {} long, {} buffers", Ts(s.duration), source.buffers(n))?;
    match source.format_change[n] {
        NONE => writeln!(f),
        at => writeln!(f, ", second format from buffer {at}"),
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "seed {} topology {} pool {} burst {} spurious {} split {} starts paused {} step \
             limit {}",
            self.seed,
            self.topology.name(),
            self.pool_slots,
            self.source.burst,
            self.spurious,
            self.split,
            self.start_paused,
            self.step_limit
        )?;
        let s = &self.source;
        if s.skew != [0; MAX_STREAMS] || s.swing > 1 || s.described {
            let (a, b, swing) = (Ts(s.skew[0]), Ts(s.skew[1]), s.swing);
            writeln!(f, "  skew {a} and {b} ahead, swing {swing}, described {}", s.described)?;
        }
        for n in 0..(self.source.stream_count as usize).min(MAX_STREAMS) {
            write_stream(f, n, &self.source)?;
        }
        for entry in &self.source.faults[..(self.source.fault_count as usize).min(MAX_FAULTS)] {
            writeln!(f, "  fault on stream {} {:?}", entry.stream, entry.fault)?;
        }
        if self.topology.has_transform() {
            let t = &self.transform;
            write!(f, "  transform delay {} window {}", t.delay, t.window)?;
            match t.error_at {
                NONE => writeln!(f, " queue {}", self.transform_queue)?,
                at => writeln!(f, " queue {} fails at input {at}", self.transform_queue)?,
            }
        }
        for (n, sink) in self.sinks[..self.sink_count().min(MAX_SINKS)].iter().enumerate() {
            let p = &sink.profile;
            writeln!(
                f,
                "  sink {n} paced {} lateness bound {} leaky {} queue {} limit {}",
                p.paced,
                Ts(p.lateness_bound),
                sink.leaky,
                sink.queue,
                Ts(if sink.limit == 0 { NONE } else { sink.limit })
            )?;
        }
        if !self.offers.is_open() {
            let (streams, sinks) = (self.topology.streams(), self.sink_count());
            Listed { offers: &self.offers, streams, sinks }.fmt(f)?;
        }
        if self.gop.on {
            let (g, p) = (&self.gop, self.pictures());
            writeln!(
                f,
                "  decoder, {} keyframes in {} pictures, one reference in {}, holds {}, raw \
                 seeks {}",
                p.keys(),
                p.count(),
                g.nonref.max(1),
                g.reorder,
                g.raw
            )?;
        }
        if self.time.retimer.on {
            let r = &self.time.retimer;
            writeln!(
                f,
                "  retimer, switches up to {} buffers after it is asked, declares {} of latency",
                r.lag,
                Ts(r.latency)
            )?;
        }
        if self.time.two_sources {
            writeln!(f, "  a source for each stream")?;
        }
        writeln!(f, "script")?;
        Script(self.script.as_slice(), Some(self)).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_draws_one_scenario() {
        for seed in 0..200u64 {
            assert_eq!(draw(seed), draw(seed));
            assert_eq!(reference(seed), reference(seed));
        }
        let distinct = (0..100u64).filter(|seed| draw(*seed) != draw(seed + 1)).count();
        assert_eq!(distinct, 100);
    }

    #[test]
    fn the_family_is_covered_and_every_draw_is_buildable() {
        let mut seen = [0u32; Topology::ALL.len()];
        let mut spec = MediaSpec::with_capacity(MAX_CUES, MAX_FAULTS);
        let (mut errors, mut leaky, mut reorders, mut inside, mut cut) = (0, 0, 0, 0, 0);
        for seed in 0..3_000u64 {
            let sc = draw(seed);
            seen[sc.topology as usize] += 1;
            sc.source.fill(&mut spec).unwrap();
            assert_eq!(spec.streams.len(), sc.topology.streams());
            assert!(sc.total_buffers() > 0, "seed {seed}");
            assert!(sc.pool_slots >= 2 + sc.transform.delay as u32 + sc.transform.window as u32);
            assert!(sc.step_limit > 2_000);
            errors += sc.declares_error() as u32;
            leaky += (0..sc.sink_count()).any(|s| sc.lossy(s)) as u32;
            reorders += (0..sc.sink_count()).any(|s| sc.window(s) > 0) as u32;
            let flush = |a: &Action| matches!(a.anchor, Anchor::InFlush { .. });
            inside += sc.script.as_slice().iter().any(flush) as u32;
            cut += (sc.split > 0) as u32;
        }
        assert!((1_200..1_800).contains(&cut), "half of the seeds cut passes, {cut} did");
        assert!(seen.iter().all(|n| *n > 300), "{seen:?}");
        assert!(errors > 30 && leaky > 100 && reorders > 100 && inside > 100);
    }

    #[test]
    fn the_reference_is_the_declared_one() {
        let sc = reference(3);
        assert_eq!(
            (sc.topology, sc.total_buffers(), sc.script.len()),
            (Topology::TwoStreams, 750, 10)
        );
        assert!(!sc.declares_error());
        assert!(sc.sinks.iter().all(|s| s.profile.paced && !s.leaky));
        let stops = |a: &Action| a.kind == ActionKind::Stop;
        assert!((0..200).all(|seed| !reference(seed).script.as_slice().iter().any(stops)));
    }

    #[test]
    fn formats_differ_by_stream_and_by_side_of_the_change() {
        let mut sc = draw_open(1);
        sc.source.format_change = [5, NONE];
        let plan = sc.plan();
        let f = |stream, seq| plan.format_of(stream, seq);
        assert_eq!((f(0, 4), f(0, 5), f(1, 0), f(1, 9_999)), (10, 20, 11, 11));
    }

    #[test]
    fn a_seed_with_its_pads_left_open_is_the_scenario_it_was() {
        let (mut same, mut refused) = (0, 0);
        for seed in 0..2_000u64 {
            let (now, then) = (draw_undecoded(seed), draw_open(seed));
            assert!(then.offers.is_open() && !then.plan().refused);
            let mut source = now.source;
            source.format_change = then.source.format_change;
            assert_eq!(Scenario { offers: then.offers, source, ..now }, then, "seed {seed}");
            same += (now == then) as u32;
            refused += now.plan().refused as u32;
        }
        assert!((1_400..1_600).contains(&same), "three seeds in four draw none, {same}");
        assert!(refused > 100, "{refused} seeds cannot be linked");
    }

    /// A link is limited only where stamps run in order, so draws that differ in a decoder or
    /// shuffling transform are compared without limits.
    fn unlimited(mut sc: Scenario) -> Scenario {
        sc.sinks.iter_mut().for_each(|sink| sink.limit = 0);
        sc
    }

    #[test]
    fn a_seed_draws_a_limit_where_the_stamps_run_in_order() {
        let (mut limited, mut sinks) = (0, 0);
        for seed in 0..3_000u64 {
            let sc = draw(seed);
            for (n, sink) in sc.sinks[..sc.sink_count()].iter().enumerate() {
                sinks += 1;
                let Some(step) = sc.sink_step(n).filter(|_| sink.limit > 0) else {
                    assert_eq!(sink.limit, 0, "seed {seed}: cues are not a buffer apart");
                    continue;
                };
                limited += 1;
                assert!(!sc.gop.on && !sink.leaky, "seed {seed}");
                assert!(
                    sc.transform.window == 0 || !sc.topology.through_transform(n),
                    "seed {seed}"
                );
                assert!(
                    sink.limit % step == 0 && (1..=5).contains(&(sink.limit / step)),
                    "seed {seed}"
                );
            }
        }
        assert!(limited * 5 > sinks && limited * 2 < sinks, "{limited} of {sinks} sinks");
    }

    #[test]
    fn a_seed_with_no_decoder_is_the_scenario_it_was() {
        let (mut decoded, mut raw, mut audio) = (0, 0, 0);
        for seed in 0..3_000u64 {
            let (now, then) = (unlimited(draw_untimed(seed)), unlimited(draw_undecoded(seed)));
            assert!(!then.gop.on && then.script.as_slice().iter().all(|a| then.how(a).is_none()));
            if !now.gop.on {
                assert_eq!(Scenario { gop: GopDecl::OFF, ..now }, then, "seed {seed}");
                continue;
            }
            decoded += 1;
            raw += now.gop.raw as u32;
            audio += matches!(now.source.streams[0].kind, StreamKind::Audio { .. }) as u32;
            assert!(now.offers.is_open() && now.topology != Topology::FanOut, "seed {seed}");
            assert!(now.topology.has_transform() && now.transform == TransformDecl::PLAIN);
            let flush = |a: &Action| matches!(a.anchor, Anchor::InFlush { .. });
            assert!(!now.script.as_slice().iter().any(flush), "seed {seed}");
            let (transform, script) = (then.transform, then.script);
            let held = then.transform.delay as u32 + then.transform.window as u32;
            let pool_slots = now.pool_slots - now.gop.reorder as u32 + held;
            let rest = Scenario { transform, script, pool_slots, gop: GopDecl::OFF, ..now };
            assert_eq!(Scenario { step_limit: then.step_limit, ..rest }, then, "seed {seed}");
        }
        assert!((250..600).contains(&decoded), "{decoded} seeds have a decoder");
        assert!(raw > 50 && raw < decoded && audio > 50, "{raw} raw, {audio} audio");
    }

    #[test]
    fn a_seed_with_no_network_is_the_scenario_it_was() {
        let mut networked = 0;
        for seed in 0..4_000u64 {
            let (now, then) = (draw(seed), draw_offline(seed));
            assert_eq!(then.net, NetDecl::OFF);
            if !now.net.on {
                assert_eq!(now, then, "seed {seed}");
                continue;
            }
            networked += 1;
            let rest = Scenario { net: NetDecl::OFF, pool_slots: now.pool_slots - 2, ..now };
            assert_eq!(rest, then, "seed {seed}");
            assert!(!(now.time.two_sources && now.topology == Topology::TwoStreams), "seed {seed}");
        }
        assert!((350..650).contains(&networked), "{networked} seeds have a network");
    }

    #[test]
    fn a_seed_with_no_timeline_is_the_scenario_it_was() {
        let (mut timed, mut retimed, mut two, mut decoded, mut held) = (0, 0, 0, 0, 0);
        let mut kinds = [0u32; ActionKind::KINDS];
        for seed in 0..4_000u64 {
            let (now, then) = (unlimited(draw_offline(seed)), unlimited(draw_untimed(seed)));
            assert_eq!(then.time, TimeDecl::OFF);
            let old = |a: &Action| a.kind.index() < 4;
            assert!(then.script.as_slice().iter().all(old), "seed {seed}");
            if !now.time.on {
                assert_eq!(now, then, "seed {seed}");
                continue;
            }
            timed += 1;
            retimed += now.time.retimer.on as u32;
            two += now.time.two_sources as u32;
            decoded += now.gop.on as u32;
            held += (now.time.retimer.on && now.time.retimer.latency > 0) as u32;
            for action in now.script.as_slice() {
                kinds[action.kind.index()] += 1;
                let shifts = matches!(action.kind, ActionKind::SetOffset { .. });
                assert!(!shifts || !matches!(action.anchor, Anchor::InFlush { .. }), "seed {seed}");
                assert!(!now.gop.on || action.kind.index() < 8, "seed {seed}");
            }
            let plain = now.offers.is_open() && !now.gop.on;
            assert!(plain || !(now.time.retimer.on || now.time.two_sources), "seed {seed}");
            assert!(!now.time.retimer.on || now.transform == TransformDecl::PLAIN);
            assert!(!now.time.two_sources || now.topology == Topology::TwoStreams);
            let held = then.transform.delay as u32 + then.transform.window as u32;
            let pool_slots = now.pool_slots + if now.time.retimer.on { held } else { 0 };
            let rest = Scenario { time: TimeDecl::OFF, script: then.script, pool_slots, ..now };
            assert_eq!(Scenario { transform: then.transform, ..rest }, then, "seed {seed}");
            assert_eq!(now.script.len(), then.script.len());
            let mut pairs = now.script.as_slice().iter().zip(then.script.as_slice());
            let kept = |(a, b): (&Action, &Action)| {
                a.anchor == b.anchor || matches!(a.kind, ActionKind::SetOffset { .. })
            };
            assert!(pairs.all(kept), "seed {seed}");
        }
        assert!((1_000..1_400).contains(&timed), "three seeds in ten, {timed}");
        assert!(
            retimed > 150 && two > 40 && decoded > 80 && held > 80,
            "{retimed} {two} {decoded} {held}"
        );
        assert!(kinds[7] > 1_000 && kinds[8] > 300 && kinds[9] > 200, "{kinds:?}");
    }

    #[test]
    fn two_sources_are_told_a_byte_each_on_the_grid_of_their_own_stream() {
        let mut sc = plain(1, Topology::TwoStreams, 2 * SEC);
        sc.time = TimeDecl { on: true, two_sources: true, ..TimeDecl::OFF };
        // 25 fps with a keyframe every second, and audio in packets of 20 ms.
        assert_eq!(sc.reads(1_234 * MS), [(25, SEC), (61, 1_220 * MS)]);
        assert_eq!(sc.reads(0), [(0, 0), (0, 0)]);
        assert_eq!(sc.reads(9 * SEC), [(25, SEC), (99, 1_980 * MS)], "past the end, the last one");
        let seek = Action { anchor: Anchor::AtStep(3), kind: ActionKind::Seek { to: 5 } };
        let pause = Action { anchor: Anchor::AtStep(3), kind: ActionKind::Pause };
        assert!(sc.told(&seek).is_some() && sc.told(&pause).is_none());
        assert_eq!(sc.told(&seek), sc.told(&seek));
        sc.time.forced = Some(Told::Second);
        assert_eq!(sc.told(&seek), Some(Told::Second));
        sc.time.two_sources = false;
        assert_eq!(sc.told(&seek), None);
    }

    #[test]
    fn a_scenario_prints_its_script() {
        let text = format!("{}", draw(7));
        assert!(text.contains("topology") && text.contains("script"), "{text}");
        let cut = draw(7).with_script(&[]);
        assert!(cut.script.is_empty());
        assert!(format!("{cut}").contains("(no action)"));
    }
}
