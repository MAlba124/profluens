//! The topology family. A seed draws a small player and a script that changes it while it
//! plays, held against what a change may do.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::Command;
use profluens_core::counters::TapHandle;
use profluens_core::couple::Coupling;
use profluens_core::ctx::{Ctx, Disposition, Stream, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, Scope, TopologyError};
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, FixedFormat, OfferDesc, Value, ValueDesc};
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::level::Limit;
use profluens_core::log::{DebugSpec, Level, Loggable};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::{Rate, Timestamp};
use profluens_core::topology::Leave;

use crate::prng::Prng;

pub const STEP: u64 = 20_000_000;
pub const STREAMS: u32 = 5;
pub const EXTRA: u32 = 9;
pub const HEAD: u32 = 50;
/// How late a paced sink may present a buffer, in buffers. A branch's first buffers are late by
/// about what the pool and rings hold, twenty odd at the smallest pool drawn.
const LATE: u64 = 32;
/// Pre-roll ahead of the position the source goes back to, in buffers. Read, not presented.
const LEAD: u64 = 3;
/// Passes a cut branch may take to stop after its splice. The loop picks among at most a
/// dozen ready groups.
const CUT_WITHIN: u32 = 600;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Oracle {
    Selected,
    AfterLeave,
    Leak,
    Timeline,
    Whole,
    Shared,
    Books,
    Ended,
    Stall,
    Error,
    Level,
    Format,
    JoinFormat,
    Meets,
}

pub const MEETS: u64 = 64;

#[derive(Clone, Debug)]
pub struct Verdict {
    pub seed: u64,
    pub oracle: Oracle,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub steps: u32,
    pub changes: u32,
    pub busy: u32,
    pub cancelled: u32,
    pub failed: u32,
    pub restarts: u32,
    pub joined: u32,
    pub left: u32,
    pub sealed: u32,
    pub gone: u32,
    pub formats: u32,
    pub meets: u64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Act {
    Switch { slot: u8, stream: u32, restart: bool, how: Leave, shape: u8 },
    Rewire { slot: u8, shape: u8 },
    Tap { stream: u32 },
    Untap,
    Share { stream: u32 },
    Unshare,
    Unplug { slot: u8 },
    AttachMute,
    Cancel,
    Fail { slot: u8 },
    Resink { slot: u8 },
    Fan { slot: u8 },
    Attach,
    Detach,
    Seek(u64),
    Skip(u64),
    Pause,
    Resume,
    Rate(i64, i64),
    Stop,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub seed: u64,
    pub version: u8,
    pub buffers: u64,
    pub paced: bool,
    pub pool: u32,
    pub logs: bool,
    pub limit: u8,
    pub script: Vec<(u32, Act)>,
    pub typed: Option<Typed>,
    pub copies: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Typed {
    pub slot: u8,
    pub fixed: i64,
    pub free: bool,
}

/// Each version adds actions drawn apart from the earlier ones, so a pinned seed keeps its run.
pub const VERSION: u8 = 8;

pub fn draw(seed: u64) -> Scenario {
    draw_at(seed, VERSION)
}

/// Draw the scenario of `seed` as the family drew it at `version`.
pub fn draw_at(seed: u64, version: u8) -> Scenario {
    let mut rng = Prng::new(seed ^ 0x7090_1097);
    let buffers = rng.range(30, 90);
    let actions = rng.range(2, 9);
    let mut script: Vec<(u32, Act)> =
        (0..actions).map(|_| (rng.range(4, 260) as u32, draw_act(&mut rng, buffers))).collect();
    let (paced, pool) = (rng.chance(50), rng.range(12, 28) as u32);
    // Drawn apart, so that a seed's script is what it was with these among it.
    let mut more = Prng::new(seed ^ 0x5a4e_d0a7);
    for _ in 0..if version >= 2 { more.range(0, 3) } else { 0 } {
        let act = match more.below(10) {
            0..=4 => Act::Share { stream: more.below(STREAMS as u64) as u32 },
            5..=7 => Act::Unshare,
            _ => Act::Unplug { slot: more.below(2) as u8 },
        };
        script.push((more.range(4, 300) as u32, act));
    }
    let logs = version >= 2 && more.below(8) == 0;
    let mut third = Prng::new(seed ^ 0xca9c_e1ed);
    for _ in 0..if version >= 3 { third.below(4) } else { 0 } {
        let act = if third.chance(60) { Act::Cancel } else { Act::AttachMute };
        // Half the time at the pass of an action, with no pass between, before its change splices.
        let beside = script[third.below(script.len() as u64) as usize].0;
        let at = if act == Act::Cancel && third.chance(50) {
            beside
        } else {
            third.range(4, 300) as u32
        };
        script.push((at, act));
    }
    script.sort_by_key(|(at, _)| *at);
    let mut fourth = Prng::new(seed ^ 0x11_417e_d11e);
    let limit = if version >= 4 && fourth.chance(50) { fourth.range(1, 6) as u8 } else { 0 };
    let mut fifth = Prng::new(seed ^ 0xfa11_5ea1);
    if version >= 5 && fifth.chance(40) {
        script.push((fifth.range(4, 300) as u32, Act::Fail { slot: fifth.below(2) as u8 }));
        // Stable, so what the script had keeps its order.
        script.sort_by_key(|(at, _)| *at);
    }
    let mut sixth = Prng::new(seed ^ 0x7e5d_f0a7);
    let typed = (version >= 6 && sixth.chance(70)).then(|| Typed {
        slot: sixth.below(2) as u8,
        fixed: sixth.range(1, 5) as i64,
        free: false,
    });
    for _ in 0..if version >= 6 { sixth.below(4) } else { 0 } {
        script.push((sixth.range(4, 300) as u32, Act::Resink { slot: sixth.below(2) as u8 }));
    }
    script.sort_by_key(|(at, _)| *at);
    let mut seventh = Prng::new(seed ^ 0xc0b1_e5ce);
    let copies = version >= 7 && seventh.chance(50);
    let mut eighth = Prng::new(seed ^ 0x5e7e_47f0);
    let typed = typed.map(|t| Typed { free: version >= 8 && eighth.chance(50), ..t });
    for _ in 0..if version >= 8 { eighth.below(4) } else { 0 } {
        script.push((eighth.range(4, 300) as u32, Act::Fan { slot: eighth.below(2) as u8 }));
    }
    script.sort_by_key(|(at, _)| *at);
    Scenario { seed, version, buffers, paced, pool, logs, limit, script, typed, copies }
}

fn draw_act(rng: &mut Prng, buffers: u64) -> Act {
    let how = if rng.chance(70) { Leave::Cut } else { Leave::Drain };
    let (slot, shape) = (rng.below(2) as u8, rng.below(3) as u8);
    match rng.below(100) {
        0..=33 => Act::Switch {
            slot,
            stream: rng.range(1, STREAMS as u64 - 1) as u32,
            restart: rng.chance(60),
            how,
            shape,
        },
        34..=41 => Act::Rewire { slot, shape },
        42..=51 => Act::Tap { stream: rng.range(1, STREAMS as u64 - 1) as u32 },
        52..=59 => Act::Untap,
        60..=67 => Act::Attach,
        68..=75 => Act::Detach,
        76..=85 => Act::Seek(rng.below(buffers)),
        86..=90 => Act::Skip(rng.below(buffers)),
        91..=93 => Act::Pause,
        94..=96 => Act::Resume,
        97..=98 => {
            Act::Rate(if rng.chance(50) { 2 } else { 1 }, if rng.chance(50) { 2 } else { 1 })
        }
        _ => Act::Stop,
    }
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];

const fn pad(name: &'static str) -> PadDesc {
    PadDesc { name, offers: &BYTES }
}

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    sched: SchedHint,
) -> ElementDesc {
    let inputs = if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single };
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched,
        inputs,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static ONE_IN: [PadDesc; 1] = [pad("sink")];
static TWO_IN: [PadDesc; 2] = [pad("a"), pad("b")];
static ONE_OUT: [PadDesc; 1] = [pad("src")];

static FEED: ElementDesc = desc("feed", &[], &ONE_OUT, SchedHint::Active);
static DEMUX: ElementDesc = desc("demux", &ONE_IN, &[], SchedHint::Passive);
static FWD: ElementDesc = desc("fwd", &ONE_IN, &ONE_OUT, SchedHint::Active);
static INLINE: ElementDesc = desc("inline", &ONE_IN, &ONE_OUT, SchedHint::Passive);
static SINK: ElementDesc = desc("sink", &ONE_IN, &[], SchedHint::Active);
static SHARE: ElementDesc = desc("share", &ONE_IN, &[], SchedHint::Active);
static MIX: ElementDesc = desc("mix", &TWO_IN, &[], SchedHint::Active);

static IDS: [ValueDesc; 4] =
    [ValueDesc::Int(1), ValueDesc::Int(2), ValueDesc::Int(3), ValueDesc::Int(4)];

const fn id(preferred: Option<ValueDesc>) -> [FieldDesc; 1] {
    [FieldDesc { field: "id", allowed: ConstraintDesc::Set(&IDS), preferred }]
}

static ID: [FieldDesc; 1] = id(None);
static ID_LIKED: [[FieldDesc; 1]; 4] =
    [id(Some(IDS[0])), id(Some(IDS[1])), id(Some(IDS[2])), id(Some(IDS[3]))];
pub const FAKE: &str = "fake";
static TYPED: [OfferDesc; 1] = [OfferDesc { family: FAKE, fields: &ID }];
static LIKED: [[OfferDesc; 1]; 4] = [
    [OfferDesc { family: FAKE, fields: &ID_LIKED[0] }],
    [OfferDesc { family: FAKE, fields: &ID_LIKED[1] }],
    [OfferDesc { family: FAKE, fields: &ID_LIKED[2] }],
    [OfferDesc { family: FAKE, fields: &ID_LIKED[3] }],
];
static T_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TYPED }];
static T_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &TYPED }];
static LIKES: [[PadDesc; 1]; 4] = [
    [PadDesc { name: "sink", offers: &LIKED[0] }],
    [PadDesc { name: "sink", offers: &LIKED[1] }],
    [PadDesc { name: "sink", offers: &LIKED[2] }],
    [PadDesc { name: "sink", offers: &LIKED[3] }],
];
static FIXES: [[Coupling; 1]; 4] = [
    [Coupling::fixed(0, "id", IDS[0])],
    [Coupling::fixed(0, "id", IDS[1])],
    [Coupling::fixed(0, "id", IDS[2])],
    [Coupling::fixed(0, "id", IDS[3])],
];
static THROUGH: [Coupling; 1] = [Coupling::through(0, 0)];

const fn typed(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    sched: SchedHint,
    couplings: &'static [Coupling],
) -> ElementDesc {
    ElementDesc { couplings, ..desc(name, sinks, srcs, sched) }
}

static TFIX: [ElementDesc; 4] = [
    typed("tfix", &ONE_IN, &T_OUT, SchedHint::Active, &FIXES[0]),
    typed("tfix", &ONE_IN, &T_OUT, SchedHint::Active, &FIXES[1]),
    typed("tfix", &ONE_IN, &T_OUT, SchedHint::Active, &FIXES[2]),
    typed("tfix", &ONE_IN, &T_OUT, SchedHint::Active, &FIXES[3]),
];
static TFWD: ElementDesc = typed("tfwd", &ONE_IN, &T_OUT, SchedHint::Active, &[]);
static TPASS: ElementDesc = typed("tpass", &T_IN, &T_OUT, SchedHint::Passive, &THROUGH);
static TSINK: [ElementDesc; 4] = [
    typed("tsink", &LIKES[0], &[], SchedHint::Active, &[]),
    typed("tsink", &LIKES[1], &[], SchedHint::Active, &[]),
    typed("tsink", &LIKES[2], &[], SchedHint::Active, &[]),
    typed("tsink", &LIKES[3], &[], SchedHint::Active, &[]),
];

const IN: SinkPad = SinkPad(0);
const IN_B: SinkPad = SinkPad(1);
const OUT: SrcPad = SrcPad(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    Start,
    Stop,
    Eos,
    Flush,
    Linked { pad: u32, on: bool },
    Row { stream: u32, seq: u64, late: u64 },
    Dropped { stream: u32, seq: u64 },
    Logs(bool),
    Format { typed: bool, id: i64 },
}

type Tape = Arc<Mutex<Vec<(u32, What)>>>;

type Doom = Arc<AtomicU32>;

#[derive(Debug)]
pub struct Doomed;

impl std::fmt::Display for Doomed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the branch was doomed to fail")
    }
}

impl std::error::Error for Doomed {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Feed { stream: u32 },
    Demux,
    Forward,
    Sink,
    Mute,
}

struct Node {
    desc: &'static ElementDesc,
    role: Role,
    tape: Tape,
    total: u64,
    next: u64,
    paced: bool,
    sent: [Option<u64>; STREAMS as usize],
    from: [u64; STREAMS as usize],
    read: Option<u64>,
    doom: Doom,
    copy: bool,
    greets: bool,
}

impl Node {
    fn note(&self, ctx: &Ctx, what: What) {
        self.tape.lock().unwrap_or_else(|e| e.into_inner()).push((ctx.element().0, what));
    }

    fn stamp(buf: &mut profluens_core::buffer::Buffer, stream: u32, seq: u64) {
        let bytes = buf.memory.as_mut_full();
        bytes[..8].copy_from_slice(&seq.to_le_bytes());
        bytes[8..12].copy_from_slice(&stream.to_le_bytes());
        buf.memory.set_len(12);
        (buf.pts, buf.duration) = (Timestamp(seq * STEP), Timestamp(STEP));
    }
}

fn told(ctx: &Ctx, format: Option<&FixedFormat>) -> Option<What> {
    let format = format?;
    let typed = ctx.family_name(format.family) == Some(FAKE);
    let id = match ctx.field_id("id").and_then(|f| format.get(f)) {
        Some(Value::Int(id)) => id,
        _ => 0,
    };
    Some(What::Format { typed, id })
}

fn read(buf: &profluens_core::buffer::Buffer) -> (u32, u64) {
    let b = buf.memory.data();
    let mut seq = [0u8; 8];
    seq.copy_from_slice(&b[..8]);
    (u32::from_le_bytes([b[8], b[9], b[10], b[11]]), u64::from_le_bytes(seq))
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        for n in 0..if self.role == Role::Demux { STREAMS } else { 0 } {
            ctx.add_stream(Stream {
                kind: if n == 0 { StreamKind::Video } else { StreamKind::Audio },
                codec: "bytes",
                language: "".into(),
                title: "".into(),
                default: n < 2,
                forced: false,
                disposition: Disposition::MAIN,
                channels: 0,
                duration: Timestamp::NONE,
                sparse: false,
                name: format!("src_{n}").into(),
                offers: &BYTES,
            });
        }
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.note(ctx, What::Start);
        if self.role == Role::Sink
            && let Some(told) = told(ctx, ctx.negotiated_sink(IN))
        {
            self.note(ctx, told);
        }
        if std::ptr::eq(self.desc, &SHARE) {
            self.note(ctx, What::Logs(Loggable::enabled(&*ctx, Level::Info)));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        match self.role {
            // Two a call where the demuxer takes one, so it has unread input when the source is
            // sent back.
            Role::Feed { stream } => {
                for _ in 0..2 {
                    if self.next >= self.total {
                        return Ok(Flow::Eos);
                    }
                    let Some(mut buf) = ctx.try_alloc() else {
                        return Ok(Flow::Ok);
                    };
                    Node::stamp(&mut buf, stream, self.next);
                    ctx.out(OUT).push(buf);
                    self.next += 1;
                }
            }
            Role::Mute => {}
            Role::Forward => {
                if inputs.front().is_some() && self.doom.load(Ordering::Relaxed) == ctx.element().0
                {
                    return Err(Error::element_failed(ctx.element(), Doomed).scoped(Scope::Branch));
                }
                while let Some(front) = inputs.front() {
                    let (stream, seq) = read(front);
                    let out = match self.copy {
                        true => ctx.try_alloc(),
                        false => inputs.pop(),
                    };
                    let Some(mut out) = out else {
                        ctx.await_recycle();
                        break;
                    };
                    if self.copy {
                        drop(inputs.pop());
                        Node::stamp(&mut out, stream, seq);
                    }
                    ctx.out(OUT).push(out);
                }
            }
            Role::Demux => {
                if let Some(buf) = inputs.pop() {
                    let (_, seq) = read(&buf);
                    for stream in 0..STREAMS {
                        let Some(pad) = ctx.stream_pad(stream).filter(|pad| ctx.pad_linked(*pad))
                        else {
                            continue;
                        };
                        let s = stream as usize;
                        if seq >= self.from[s] && self.sent[s].is_none_or(|sent| seq > sent) {
                            let Some(mut out) = ctx.alloc_exact(12) else {
                                return Err(ctx.exact_refused(12));
                            };
                            Node::stamp(&mut out, stream, seq);
                            ctx.out(pad).push(out);
                            self.sent[s] = Some(seq);
                        }
                    }
                    self.read = self.read.max(Some(seq));
                }
            }
            Role::Sink => {
                loop {
                    let front = inputs.front().map(|b| b.pts);
                    let fronts = [
                        front,
                        ctx.front_input_on(IN).map(|b| b.pts),
                        ctx.front_input_on(IN_B).map(|b| b.pts),
                    ];
                    let Some(at) = fronts.iter().position(Option::is_some) else {
                        break;
                    };
                    let pts = fronts[at].unwrap_or(Timestamp::NONE);
                    let before = ctx.before_presentation(pts, Timestamp(STEP));
                    if !before && self.paced && !ctx.due(pts) {
                        break;
                    }
                    let buf = match at {
                        0 => inputs.pop(),
                        1 => ctx.pop_input_on(IN),
                        _ => ctx.pop_input_on(IN_B),
                    };
                    let Some(buf) = buf else { break };
                    let (stream, seq) = read(&buf);
                    // In stream time, so a rate does not stretch it.
                    let late = ctx.stream_time(ctx.now()).0.saturating_sub(pts.0);
                    let late = if self.paced { late } else { 0 };
                    self.note(
                        ctx,
                        if before {
                            What::Dropped { stream, seq }
                        } else {
                            What::Row { stream, seq, late }
                        },
                    );
                }
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => self.note(ctx, What::Eos),
            Event::FlushStart => {
                self.note(ctx, What::Flush);
                if matches!(self.role, Role::Feed { .. }) {
                    self.next = ctx.seek_target().map_or(0, |t| t.to_byte);
                }
                (self.sent, self.from, self.read) =
                    ([None; STREAMS as usize], [0; STREAMS as usize], None);
            }
            Event::SrcLinked { pad, on: true, restart } => {
                let stream = (0..STREAMS).find(|s| ctx.stream_pad(*s) == Some(*pad));
                if let Some(s) = stream.map(|s| s as usize) {
                    self.sent[s] = None;
                    self.from[s] =
                        restart.map_or(self.read.map_or(0, |read| read + 1), |t| t.to_byte);
                }
            }
            Event::SrcJoined { pad } if self.greets => {
                let Some(mut out) = ctx.alloc_exact(12) else {
                    return Err(ctx.exact_refused(12));
                };
                Node::stamp(&mut out, HEAD, 0);
                ctx.out(*pad).push(out);
            }
            Event::SinkLinked { pad, on } => self.note(ctx, What::Linked { pad: pad.0, on: *on }),
            Event::FormatChange(format) if self.role == Role::Sink => {
                if let Some(told) = told(ctx, Some(format)) {
                    self.note(ctx, told);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.note(ctx, What::Stop);
    }
}

#[derive(Clone, Debug)]
struct Branch {
    elements: Vec<ElementId>,
    sink: ElementId,
    stream: u32,
    link: LinkId,
    joined: Option<(u64, bool)>,
    moved: bool,
    to_end: bool,
    inner: Option<LinkId>,
    format: Option<i64>,
    fan: Option<ElementId>,
}

#[derive(Clone, Debug)]
struct Left {
    sink: ElementId,
    elements: Vec<ElementId>,
    how: Leave,
    change: u32,
    spliced: Option<u32>,
}

struct App {
    p: Pipeline,
    clock: MockClock,
    tape: Tape,
    doom: Doom,
    paced: bool,
    limit: u8,
    grown: u64,
    buffers: u64,
    feed: ElementId,
    demux: ElementId,
    mix: ElementId,
    slots: [Option<Branch>; 2],
    tap: Option<Branch>,
    share: Option<Branch>,
    shares: Vec<Branch>,
    extra: Option<ElementId>,
    cut: Vec<ElementId>,
    sealed: Vec<ElementId>,
    made: Vec<Branch>,
    left: Vec<Left>,
    paused: bool,
    stopped: bool,
    summary: Summary,
    time: Option<TapHandle>,
    sought: u64,
    caught: Option<Timestamp>,
    undo: Option<Undo>,
    pending: Option<(u32, Undo)>,
    in_flight: bool,
    step: u32,
    fault: Option<(Oracle, String)>,
    typed: Option<Typed>,
    links: u32,
    books: Vec<(LinkId, FixedFormat)>,
    relinked: Option<LinkId>,
    over: bool,
    copies: bool,
    version: u8,
}

#[derive(Clone)]
struct Undo {
    slots: [Option<Branch>; 2],
    tap: Option<Branch>,
    share: Option<Branch>,
    extra: Option<ElementId>,
    made: usize,
    shares: usize,
    left: usize,
    cut: usize,
    first: usize,
    last: usize,
}

impl App {
    fn node(&self, desc: &'static ElementDesc, role: Role) -> Node {
        let (tape, total, paced) = (Arc::clone(&self.tape), self.buffers, self.paced);
        let (sent, from) = ([None; STREAMS as usize], [0; STREAMS as usize]);
        let copy = self.copies && role == Role::Forward;
        let greets = role == Role::Demux || self.version < 8;
        Node {
            desc,
            role,
            tape,
            total,
            next: 0,
            paced,
            sent,
            from,
            read: None,
            doom: Arc::clone(&self.doom),
            copy,
            greets,
        }
    }

    fn position(&self) -> u64 {
        match (&self.time, self.paced) {
            (Some(time), true) => time.now().nanos().map_or(self.sought, |ns| ns / STEP),
            _ => self.sought,
        }
    }

    fn limit(&mut self, sink: ElementId) {
        if self.limit > 0 {
            self.p.set_link_limit(sink, Limit::time(Timestamp(self.limit as u64 * STEP)));
        }
    }

    /// The sink whose link is over its limit in both time and buffers. Time alone misreads a joined
    /// link's header stamped zero, buffers alone misread the gap between a seek's two flushes.
    fn over_limit(&mut self) -> Option<(ElementId, u64)> {
        let time = self.time.as_ref().filter(|_| self.limit > 0)?;
        let sinks = || self.made.iter().map(|branch| branch.sink).chain([self.mix]);
        let stands =
            |sink| time.link_limit(sink, IN).and_then(|l| l.time.nanos()).map_or(0, |ns| ns / STEP);
        self.grown = sinks().map(stands).fold(self.grown, u64::max);
        let limit = (self.limit as u64).max(self.grown);
        let over = |sink: ElementId| {
            let held = time.queued(sink, IN)?;
            let (buffers, span) = (held.bytes / 12, held.time.nanos().unwrap_or(0));
            (buffers > limit + 2 && span > (limit + 1) * STEP).then_some((sink, buffers))
        };
        sinks().find_map(over)
    }

    fn branched(&self, stream: u32) -> bool {
        let on = |b: &Option<Branch>| b.as_ref().is_some_and(|b| b.stream == stream);
        stream == 0 || self.slots.iter().any(on) || on(&self.tap)
    }

    fn plays(&self, stream: u32) -> bool {
        self.branched(stream) || self.share.as_ref().is_some_and(|b| b.stream == stream)
    }

    // The list of a branch's elements, once a change.
    #[allow(clippy::disallowed_methods)]
    fn branch(
        &mut self,
        stream: u32,
        shape: u8,
        relink: Option<LinkId>,
        slot: Option<u8>,
    ) -> Result<Branch, Error> {
        let pad = self.p.select(self.demux, stream)?;
        let mut elements = Vec::new();
        let typed = self.typed.filter(|t| Some(t.slot) == slot && matches!(shape, 1 | 2));
        let ahead: &[(&'static ElementDesc, Role)] = match (shape, typed) {
            (0 | u8::MAX, _) => &[],
            (1, Some(t)) if t.free => &[(&TFWD, Role::Forward)],
            (1, Some(t)) => &[(&TFIX[t.fixed as usize - 1], Role::Forward)],
            (1, None) => &[(&FWD, Role::Forward)],
            (_, Some(_)) => &[(&TFWD, Role::Forward), (&TPASS, Role::Forward)],
            _ => &[(&FWD, Role::Forward), (&INLINE, Role::Forward)],
        };
        let liked = (self.p.element_count() + ahead.len()) % 4;
        let last: &'static ElementDesc = match (shape, typed) {
            (u8::MAX, _) => &SHARE,
            (_, Some(_)) => &TSINK[liked],
            _ => &SINK,
        };
        for (desc, role) in ahead.iter().chain(&[(last, Role::Sink)]) {
            let node = self.node(desc, *role);
            elements.push(self.p.add(node));
        }
        let link = match relink {
            Some(link) => self.p.relink(link, elements[0].sink(IN)).map(|()| link)?,
            None => self.p.link(self.demux.src(pad), elements[0].sink(IN))?,
        };
        self.relinked = relink;
        let mut inner = None;
        for pair in elements.windows(2) {
            inner = Some(self.p.link(pair[0].src(OUT), pair[1].sink(IN))?);
        }
        self.links = self.links.max(1 + inner.unwrap_or(link).0).max(1 + link.0);
        let sink = elements[elements.len() - 1];
        self.limit(sink);
        let format = typed.map(|t| if shape == 1 && !t.free { t.fixed } else { liked as i64 + 1 });
        let to_end = self.to_end();
        Ok(Branch {
            elements,
            sink,
            stream,
            link,
            joined: None,
            moved: false,
            to_end,
            inner,
            format,
            fan: None,
        })
    }

    /// Whether the demuxer has buffers left for a branch joining now. Behind a flush nothing says.
    fn to_end(&self) -> bool {
        let read = self.time.as_ref().and_then(|time| time.snapshot(self.demux)).map(|c| c.pts_out);
        match read {
            None => true,
            Some(read) => read.is_some_and(|read| read.0 / STEP + 4 < self.buffers),
        }
    }

    fn keep_books(&mut self) {
        self.books.clear();
        for link in (0..self.links).map(LinkId) {
            if let Some(f) = self.p.negotiated(link) {
                self.books.push((link, f.clone()));
            }
        }
    }

    fn leave(&mut self, branch: &Branch, how: Leave) -> Result<(), Error> {
        branch.elements.iter().chain(&branch.fan).try_for_each(|el| self.p.remove(*el, how))
    }

    /// `None` is a commit refused as busy, which leaves the books as they were.
    fn commit(&mut self, at: Option<(u64, bool)>, sparse: bool) -> Result<Option<u32>, Error> {
        let done = match (at, sparse) {
            (Some((seq, true)), _) => {
                let (from, to) = (seq.saturating_sub(LEAD), Timestamp(seq * STEP));
                let at = Timestamp(from * STEP);
                self.summary.restarts += 1;
                self.p.commit_at(to, &[(self.feed, from, at), (self.demux, from, at)])
            }
            (Some((seq, false)), _) => self.p.commit_at(Timestamp(seq * STEP), &[]),
            (None, true) => self.p.commit(),
            (None, false) => self.p.commit(),
        };
        match done {
            Ok(change) => {
                if self.over || self.stopped {
                    self.fault = Some((
                        Oracle::Books,
                        format!("the commit of change {change} was taken by a run that was over"),
                    ));
                }
                let meets = self.p.solve_meets();
                self.summary.meets = self.summary.meets.max(meets);
                if meets > MEETS {
                    self.fault = Some((
                        Oracle::Meets,
                        format!(
                            "the solve of change {change} took {meets} meets, more than {MEETS}"
                        ),
                    ));
                }
                // A link that ran before the commit runs at the format it had.
                for (link, was) in
                    self.books.iter().filter(|(link, _)| Some(*link) != self.relinked)
                {
                    if let Some(now) = self.p.negotiated(*link).filter(|now| *now != was) {
                        let detail = format!(
                            "change {change} moved running link {} from {:?} to {:?}",
                            link.0,
                            was.fields(),
                            now.fields()
                        );
                        self.fault = Some((Oracle::Format, detail));
                    }
                }
                self.keep_books();
                self.summary.changes += 1;
                let last = self.p.element_count();
                self.pending = self.undo.take().map(|undo| (change, Undo { last, ..undo }));
                self.in_flight = true;
                Ok(Some(change))
            }
            Err(Error::Topology(TopologyError::Busy)) => {
                self.summary.busy += 1;
                if !self.in_flight {
                    self.fault = Some((
                        Oracle::Books,
                        format!(
                            "a commit at step {} was refused as busy with no change on its way",
                            self.step
                        ),
                    ));
                }
                Ok(None)
            }
            Err(Error::Topology(TopologyError::NotRunning)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn gone(&mut self, branch: Branch, how: Leave, change: u32) {
        self.summary.left += 1;
        let mut elements = branch.elements;
        elements.extend(branch.fan);
        if how == Leave::Cut {
            self.cut.extend(&elements);
        }
        self.left.push(Left { sink: branch.sink, elements, how, change, spliced: None });
    }

    /// Change `change` failed before its splice. Its books are restored and what it added was cut.
    fn failed(&mut self, change: u32) {
        let Some((_, undo)) = self.pending.take_if(|(c, _)| *c == change) else {
            return;
        };
        self.in_flight = false;
        self.keep_books();
        (self.slots, self.tap, self.share, self.extra) =
            (undo.slots, undo.tap, undo.share, undo.extra);
        self.made.truncate(undo.made);
        self.shares.truncate(undo.shares);
        self.left.truncate(undo.left);
        self.cut.truncate(undo.cut);
        let elements: Vec<ElementId> =
            (undo.first..undo.last).map(|i| ElementId(i as u32)).collect();
        self.cut.extend(&elements);
        if let Some(sink) = elements.last().copied() {
            self.left.push(Left {
                sink,
                elements,
                how: Leave::Cut,
                change,
                spliced: Some(self.step),
            });
        }
    }

    // The books of an action, once an action.
    #[allow(clippy::disallowed_methods)]
    fn act(&mut self, run: &Stepped, act: Act) -> Result<(), Error> {
        // Brought up to date first, so a change that failed since is out of the books.
        self.p.settle();
        self.hear();
        self.over = run.control().is_finished();
        self.relinked = None;
        self.undo = Some(Undo {
            slots: self.slots.clone(),
            tap: self.tap.clone(),
            share: self.share.clone(),
            extra: self.extra,
            made: self.made.len(),
            shares: self.shares.len(),
            left: self.left.len(),
            cut: self.cut.len(),
            first: self.p.element_count(),
            last: 0,
        });
        match act {
            Act::Switch { slot, stream, restart, how, shape } if !self.plays(stream) => {
                let position = self.position();
                // The source goes back for a stream, never ahead, and for one stream at a time.
                // Sent elsewhere before it is back where it had come, the first
                // stream would miss what lies between.
                let read = self
                    .time
                    .as_ref()
                    .and_then(|time| time.snapshot(self.demux))
                    .and_then(|c| c.pts_out);
                let back = read.is_some_and(|read| {
                    position * STEP <= read.0 && self.caught.is_none_or(|high| read > high)
                });
                let restart = restart && back;
                if restart {
                    self.caught = read;
                }
                let old = self.slots[slot as usize].clone();
                if let Some(old) = &old {
                    self.leave(old, how)?;
                }
                let mut new = self.branch(stream, shape, None, Some(slot))?;
                new.joined = Some((position, restart));
                if let Some(change) = self.commit(Some((position, restart)), false)? {
                    self.summary.joined += 1;
                    self.made.push(new.clone());
                    self.slots[slot as usize] = Some(new);
                    old.into_iter().for_each(|old| self.gone(old, how, change));
                }
            }
            Act::Rewire { slot, shape } => {
                let Some(old) = self.slots[slot as usize].clone() else {
                    return Ok(());
                };
                let position = self.position();
                let mut new = self.branch(old.stream, shape, Some(old.link), Some(slot))?;
                self.leave(&old, Leave::Cut)?;
                new.joined = Some((position, false));
                if let Some(change) = self.commit(Some((position, false)), false)? {
                    self.summary.joined += 1;
                    self.made.push(new.clone());
                    self.slots[slot as usize] = Some(new);
                    self.gone(old, Leave::Cut, change);
                }
            }
            Act::Tap { stream } if self.tap.is_none() && !self.plays(stream) => {
                let mut new = self.branch(stream, 0, None, None)?;
                new.joined = Some((self.position(), false));
                if self.commit(None, true)?.is_some() {
                    self.summary.joined += 1;
                    self.made.push(new.clone());
                    self.tap = Some(new);
                }
            }
            Act::Untap => {
                let Some(old) = self.tap.clone() else {
                    return Ok(());
                };
                self.leave(&old, Leave::Drain)?;
                if let Some(change) = self.commit(None, false)? {
                    self.tap = None;
                    self.gone(old, Leave::Drain, change);
                }
            }
            Act::Share { stream } if self.share.is_none() && self.branched(stream) => {
                let mut new = self.branch(stream, u8::MAX, None, None)?;
                new.joined = Some((self.position(), false));
                if self.commit(None, true)?.is_some() {
                    self.summary.joined += 1;
                    self.shares.push(new.clone());
                    self.share = Some(new);
                }
            }
            Act::Unshare => {
                let Some(old) = self.share.clone() else {
                    return Ok(());
                };
                self.leave(&old, Leave::Drain)?;
                if let Some(change) = self.commit(None, false)? {
                    self.share = None;
                    self.gone(old, Leave::Drain, change);
                }
            }
            Act::Unplug { slot } => {
                let inner = self.slots[slot as usize].as_ref().and_then(|b| b.inner);
                let Some(inner) = inner else { return Ok(()) };
                self.p.unlink(inner)?;
                if self.commit(None, false)?.is_some()
                    && let Some(branch) = self.slots[slot as usize].as_mut()
                {
                    (branch.inner, branch.to_end) = (None, false);
                }
            }
            Act::Attach if self.extra.is_none() => {
                let mut node = self.node(&FEED, Role::Feed { stream: EXTRA });
                node.total = self.buffers / 2;
                let extra = self.p.add(node);
                self.p.set_element_pool(extra, 64, 6);
                let link = self.p.link(extra.src(OUT), self.mix.sink(IN_B))?;
                self.links = self.links.max(link.0 + 1);
                if self.commit(None, false)?.is_some() {
                    self.summary.joined += 1;
                    self.extra = Some(extra);
                }
            }
            // Its change waits for a first batch that never comes, until cancelled or the run ends.
            Act::AttachMute if self.extra.is_none() => {
                let extra = self.p.add(self.node(&FEED, Role::Mute));
                let link = self.p.link(extra.src(OUT), self.mix.sink(IN_B))?;
                self.links = self.links.max(link.0 + 1);
                if self.commit(None, false)?.is_some() {
                    self.extra = Some(extra);
                }
            }
            Act::Fail { slot } => {
                let ahead = self.slots[slot as usize].as_ref().filter(|b| b.elements.len() > 1);
                if let Some(first) = ahead.map(|b| b.elements[0]) {
                    self.doom.store(first.0, Ordering::Relaxed);
                }
            }
            Act::Resink { slot } => {
                let Some(old) = self.slots[slot as usize]
                    .clone()
                    .filter(|b| b.inner.is_some() && b.elements.len() > 1)
                else {
                    return Ok(());
                };
                let ahead = &old.elements[..old.elements.len() - 1];
                let doom = self.doom.load(Ordering::Relaxed);
                let sealed = self.version >= 7 && ahead.iter().any(|el| self.sealed.contains(el));
                if !sealed && ahead.iter().any(|el| el.0 == doom || self.sealed.contains(el)) {
                    return Ok(());
                }
                let (tail, position) = (ahead[ahead.len() - 1], self.position());
                let desc: &'static ElementDesc = match old.format {
                    Some(id) => &TSINK[id as usize % 4],
                    None => &SINK,
                };
                let sink = self.p.add(self.node(desc, Role::Sink));
                self.p.remove(old.sink, Leave::Cut)?;
                let inner = self.p.link(tail.src(OUT), sink.sink(IN))?;
                self.links = self.links.max(inner.0 + 1);
                self.limit(sink);
                let mut elements = ahead.to_vec();
                elements.push(sink);
                let to_end = self.to_end();
                let new = Branch {
                    elements,
                    sink,
                    inner: Some(inner),
                    joined: Some((position, false)),
                    moved: false,
                    to_end,
                    ..old.clone()
                };
                let committed = self.commit(Some((position, false)), false);
                if sealed {
                    match committed {
                        Err(Error::Topology(TopologyError::Gone { element }))
                            if element == tail =>
                        {
                            self.summary.gone += 1
                        }
                        Ok(Some(change)) => {
                            let detail = format!(
                                "change {change} took a link out of element {}, whose group is sealed",
                                tail.0
                            );
                            self.fault = Some((Oracle::Books, detail));
                        }
                        Ok(None) => {}
                        Err(e) => return Err(e),
                    }
                    return Ok(());
                }
                if let Some(change) = committed? {
                    self.summary.joined += 1;
                    self.made.push(new.clone());
                    self.slots[slot as usize] = Some(new);
                    self.gone(
                        Branch { elements: vec![old.sink], fan: None, ..old },
                        Leave::Cut,
                        change,
                    );
                }
            }
            Act::Fan { slot } => {
                let typed = |b: &Branch| {
                    b.format.is_some()
                        && b.inner.is_some()
                        && b.elements.len() > 1
                        && b.fan.is_none()
                };
                let Some(old) = self.slots[slot as usize].clone().filter(typed) else {
                    return Ok(());
                };
                let ahead = &old.elements[..old.elements.len() - 1];
                let doom = self.doom.load(Ordering::Relaxed);
                if ahead.iter().any(|el| el.0 == doom || self.sealed.contains(el)) {
                    return Ok(());
                }
                let id = old.format.unwrap_or(0);
                let sink = self.p.add(self.node(&TSINK[id as usize % 4], Role::Sink));
                let link = self.p.link(ahead[ahead.len() - 1].src(OUT), sink.sink(IN))?;
                self.links = self.links.max(link.0 + 1);
                self.limit(sink);
                if self.commit(None, false)?.is_some() {
                    self.summary.joined += 1;
                    let fan = Branch {
                        elements: vec![sink],
                        sink,
                        inner: None,
                        joined: None,
                        moved: false,
                        to_end: false,
                        fan: None,
                        ..old
                    };
                    self.made.push(fan);
                    if let Some(branch) = self.slots[slot as usize].as_mut() {
                        branch.fan = Some(sink);
                    }
                }
            }
            Act::Cancel => {
                if let Some(change) = self.p.cancel() {
                    self.summary.cancelled += 1;
                    self.failed(change);
                }
            }
            Act::Detach => {
                let Some(extra) = self.extra else {
                    return Ok(());
                };
                self.p.remove(extra, Leave::Cut)?;
                if self.commit(None, false)?.is_some() {
                    (self.extra, self.summary.left) = (None, self.summary.left + 1);
                    self.cut.push(extra);
                }
            }
            Act::Seek(seq) => {
                let to = Timestamp(seq * STEP);
                if run
                    .control()
                    .seek_bytes(to, &[(self.feed, seq, to), (self.demux, seq, to)])
                    .is_ok()
                {
                    (self.sought, self.caught) = (seq, None);
                    self.moved();
                }
            }
            // Ahead only. A skip back leaves every stream ahead of the clock, and a paced sink then
            // holds the demuxer that long.
            Act::Skip(seq) if seq >= self.position() => {
                if run.control().append(Command::SkipTo { to_time: Timestamp(seq * STEP) }).is_ok()
                {
                    self.sought = seq;
                    self.moved();
                }
            }
            Act::Pause => self.paused |= run.control().append(Command::Pause).is_ok(),
            Act::Resume => self.paused &= run.control().append(Command::Resume).is_err(),
            Act::Rate(num, den) => {
                if let Ok(rate) = Rate::new(num, den) {
                    let _ = run.control().append(Command::SetRate { rate });
                }
            }
            Act::Stop => self.stopped |= run.control().append(Command::Stop).is_ok(),
            _ => {}
        }
        Ok(())
    }

    fn hear(&mut self) {
        while let Some(message) = self.p.bus().try_recv() {
            match message {
                BusMessage::Changed { change } => {
                    let step = self.step;
                    self.left
                        .iter_mut()
                        .filter(|l| l.change == change)
                        .for_each(|l| l.spliced = Some(step));
                    if self.pending.take_if(|(c, _)| *c == change).is_some() {
                        self.in_flight = false;
                    }
                }
                BusMessage::ChangeFailed { change, .. } => {
                    self.summary.failed += 1;
                    self.failed(change);
                }
                BusMessage::BranchSealed { group, error } => {
                    self.sealed(ElementId(group.0), &error)
                }
                _ => {}
            }
        }
    }

    fn sealed(&mut self, head: ElementId, error: &Error) {
        self.summary.sealed += 1;
        let doomed = matches!(error, Error::ElementFailed { element, scope: Scope::Branch, .. } if *element == head);
        if !doomed || self.doom.load(Ordering::Relaxed) != head.0 {
            self.fault = Some((
                Oracle::Books,
                format!("group {} was sealed for {error}, and no branch of it was doomed", head.0),
            ));
            return;
        }
        // Out of the undo's reach, since a change that fails later does not unseal it.
        if let Some(branch) = self.made.iter().find(|b| b.elements[0] == head) {
            self.sealed.extend(&branch.elements[..branch.elements.len() - 1]);
        }
    }

    /// Every group applies a seek or skip, so a branch that left is moved too until its sink stops.
    fn moved(&mut self) {
        let tape = self.tape.lock().unwrap_or_else(|e| e.into_inner());
        let stopped = tape
            .iter()
            .filter(|(_, what)| *what == What::Stop)
            .map(|(el, _)| *el)
            .collect::<Vec<_>>();
        self.made.iter_mut().filter(|b| !stopped.contains(&b.sink.0)).for_each(|b| b.moved = true);
    }
}

fn fail<T>(seed: u64, oracle: Oracle, detail: String) -> Result<T, Verdict> {
    Err(Verdict { seed, oracle, detail })
}

pub fn run(sc: &Scenario) -> Result<Summary, Verdict> {
    play(sc, &Tape::default())
}

pub fn trace(sc: &Scenario) -> (Result<Summary, Verdict>, Vec<(u32, What)>) {
    let tape = Tape::default();
    let result = play(sc, &tape);
    let seen = tape.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (result, seen)
}

// The lists of a run and the words of a verdict, once a run.
#[allow(clippy::disallowed_methods)]
fn play(sc: &Scenario, tape: &Tape) -> Result<Summary, Verdict> {
    let seed = sc.seed;
    let error = |e: Error| Verdict { seed, oracle: Oracle::Error, detail: e.to_string() };
    let clock = MockClock::starting_at(Timestamp::from_secs(5));
    let debug = DebugSpec::parse(if sc.logs { "share:info" } else { "" });
    let mut p = Pipeline::with(debug, false, Arc::new(clock.clone()));
    p.set_pool(64, sc.pool);
    p.set_select_all(false);
    let tape = Arc::clone(tape);
    let doom: Doom = Arc::new(AtomicU32::new(u32::MAX));
    let node = |desc, role| {
        let (tape, total, paced) = (Arc::clone(&tape), sc.buffers, sc.paced);
        let (sent, from) = ([None; STREAMS as usize], [0; STREAMS as usize]);
        let greets = role == Role::Demux || sc.version < 8;
        Node {
            desc,
            role,
            tape,
            total,
            next: 0,
            paced,
            sent,
            from,
            read: None,
            doom: Arc::clone(&doom),
            copy: false,
            greets,
        }
    };
    let feed = p.add(node(&FEED, Role::Feed { stream: 0 }));
    let demux = p.add(node(&DEMUX, Role::Demux));
    let mix = p.add(node(&MIX, Role::Sink));
    p.link(feed.src(OUT), demux.sink(IN)).map_err(error)?;
    p.preroll().map_err(error)?;
    let video = p.select(demux, 0).map_err(error)?;
    p.link(demux.src(video), mix.sink(IN)).map_err(error)?;
    let (slots, summary) = ([None, None], Summary::default());
    let (made, left) = (Vec::new(), Vec::new());
    let mut app = App {
        p,
        clock,
        tape,
        doom,
        paced: sc.paced,
        limit: sc.limit,
        grown: 0,
        buffers: sc.buffers,
        feed,
        demux,
        mix,
        slots,
        tap: None,
        share: None,
        shares: Vec::new(),
        cut: Vec::new(),
        sealed: Vec::new(),
        undo: None,
        pending: None,
        in_flight: false,
        step: 0,
        fault: None,
        over: false,
        typed: sc.typed,
        links: 2,
        books: Vec::new(),
        relinked: None,
        copies: sc.copies,
        version: sc.version,
        extra: None,
        made,
        left,
        paused: false,
        stopped: false,
        summary,
        time: None,
        sought: 0,
        caught: None,
    };
    app.limit(mix);
    let first = app.branch(1, 1, None, Some(0)).map_err(error)?;
    app.made.push(first.clone());
    app.slots[0] = Some(first);
    app.time = Some(app.p.tap_handle());
    let mut run = app.p.step(Arc::new(app.clock.clone())).map_err(error)?;
    app.keep_books();

    let mut rng = Prng::new(seed ^ 0x5e_ed0f_9a55);
    let limit = 6_000 + 40 * sc.buffers as u32;
    let (mut next, mut step, mut ready) = (0usize, 0u32, Vec::new());
    loop {
        app.step = step;
        while sc.script.get(next).is_some_and(|(at, _)| *at <= step) {
            app.act(&run, sc.script[next].1).map_err(error)?;
            next += 1;
            if let Some((oracle, detail)) = app.fault.take() {
                return fail(seed, oracle, detail);
            }
        }
        run.service();
        app.hear();
        ready.clear();
        ready.extend((0..run.groups()).filter(|g| run.ready(*g)));
        if ready.is_empty() {
            match run.next_deadline() {
                Some(to) if to > app.clock.now() => {
                    app.clock.advance(Timestamp(to.0 - app.clock.now().0))
                }
                // A deadline at or behind the clock with no group ready is a wake that readied
                // nothing, which the next scripted action must not paper over.
                Some(to) => {
                    let now = app.clock.now();
                    let detail = format!("deadline {to:?} at or behind the clock {now:?}");
                    return fail(seed, Oracle::Stall, detail);
                }
                _ if next < sc.script.len() => step = sc.script[next].0,
                _ if app.paused && !app.stopped => {
                    app.paused = false;
                    let _ = run.control().append(Command::Resume);
                }
                _ => break,
            }
            continue;
        }
        let g = ready[rng.below(ready.len() as u64) as usize];
        run.pass(g).map_err(error)?;
        step += 1;
        if let Some((sink, held)) = app.over_limit() {
            let detail = format!(
                "the link into sink {} holds {held} buffers at step {step}, its limit is {}",
                sink.0, sc.limit
            );
            return fail(seed, Oracle::Level, detail);
        }
        for gone in app.left.iter().filter(|l| l.how == Leave::Cut) {
            let overdue = gone.spliced.is_some_and(|at| step > at + CUT_WITHIN);
            if overdue && !stopped(&app.tape, gone.sink) {
                return fail(
                    seed,
                    Oracle::Leak,
                    format!("sink {} cut at step {:?} runs on", gone.sink.0, gone.spliced),
                );
            }
        }
        if step > limit {
            let waits: Vec<_> = (0..run.groups()).map(|g| run.outcome(g)).collect();
            return fail(
                seed,
                Oracle::Stall,
                format!("{step} steps, the groups wait on {waits:?}"),
            );
        }
    }
    let over = (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit));
    if !over {
        let live = (0..run.groups()).filter(|g| run.outcome(*g) != Some(Outcome::Exit));
        let waits: Vec<_> =
            live.map(|g| (run.elements(g).to_vec(), run.outcome(g), run.held(g))).collect();
        return fail(
            seed,
            Oracle::Stall,
            format!("quiet at step {step} with the groups waiting on {waits:?}"),
        );
    }
    let dropped = app.p.counters(app.demux).drops;
    let report = run.finish().map_err(error)?;
    app.summary.steps = step;
    app.p.settle();
    app.hear();
    if app.pending.is_some() && app.in_flight {
        return fail(
            seed,
            Oracle::Books,
            "the run ended and nothing says how the last change went".into(),
        );
    }
    let Some(tap) = app.time.clone() else {
        return Ok(app.summary);
    };
    app.summary.formats = sweep(sc, &app, tap.pool().outstanding, dropped)?;
    books(sc, &app, &tap, report.buffers)?;
    Ok(app.summary)
}

// The lists of a run and the words of a verdict, once a run.
#[allow(clippy::disallowed_methods)]
fn books(sc: &Scenario, app: &App, tap: &TapHandle, reported: u64) -> Result<(), Verdict> {
    let seed = sc.seed;
    let tape = app.tape.lock().unwrap_or_else(|e| e.into_inner());
    let of = |el: ElementId| tape.iter().filter(move |(e, _)| *e == el.0).map(|(_, what)| *what);
    for gone in app.left.iter().filter(|l| l.spliced.is_some()) {
        if let Some(el) = gone.elements.iter().find(|el| tap.snapshot(**el).is_some()) {
            return fail(
                seed,
                Oracle::Books,
                format!("the tap reads element {}, which left", el.0),
            );
        }
    }
    for branch in app.slots.iter().flatten().chain(&app.tap).chain(&app.share) {
        let handed = of(branch.sink)
            .filter(|w| matches!(w, What::Row { .. } | What::Dropped { .. }))
            .count() as u64;
        let counted = tap.snapshot(branch.sink).map(|c| c.buffers_in);
        // What a flush or a cut took from its input was counted and not handed.
        let flushes = of(branch.sink).filter(|w| *w == What::Flush).count();
        let cut = of(branch.sink).any(|w| matches!(w, What::Linked { on: false, .. }));
        let exact = !app.stopped && !cut && flushes == usize::from(branch.joined.is_some());
        if counted.is_none_or(|counted| counted < handed || (exact && counted != handed)) {
            let detail = format!(
                "sink {} was handed {handed} buffers and the tap reads {counted:?}",
                branch.sink.0
            );
            return fail(seed, Oracle::Books, detail);
        }
    }
    for share in &app.shares {
        if of(share.sink).any(|w| w == What::Logs(!sc.logs)) {
            let detail = format!(
                "sink {} joined a run that logs {} and does not do the same",
                share.sink.0, sc.logs
            );
            return fail(seed, Oracle::Books, detail);
        }
    }
    // The report counts the joined source's pool buffers too, which left with it.
    let made = tap.snapshot(app.feed).map_or(0, |c| c.buffers_out);
    let joined = of(app.mix).filter(|w| {
        matches!(w, What::Row { stream: EXTRA, .. } | What::Dropped { stream: EXTRA, .. })
    });
    let joined = joined.count() as u64;
    if reported < made + joined {
        let detail = format!(
            "the report counts {reported} buffers, the feed made {made} and the joined source {joined}"
        );
        return fail(seed, Oracle::Books, detail);
    }
    let elements = tape.iter().map(|(el, _)| *el).max().map_or(0, |el| el + 1);
    for el in (0..elements)
        .map(ElementId)
        .filter(|el| !app.stopped && !app.cut.contains(el) && !app.sealed.contains(el))
    {
        let (started, ended) = (of(el).any(|w| w == What::Start), of(el).any(|w| w == What::Eos));
        if started && !ended {
            return fail(
                seed,
                Oracle::Ended,
                format!("the run ended and element {} had not ended its stream", el.0),
            );
        }
    }
    Ok(())
}

fn stopped(tape: &Tape, el: ElementId) -> bool {
    tape.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(e, what)| *e == el.0 && *what == What::Stop)
}

fn sweep(sc: &Scenario, app: &App, outstanding: u64, dropped: u64) -> Result<u32, Verdict> {
    let seed = sc.seed;
    let tape = app.tape.lock().unwrap_or_else(|e| e.into_inner());
    if outstanding != 0 {
        return fail(seed, Oracle::Leak, format!("{outstanding} pool slots are out at the end"));
    }
    if dropped != 0 {
        return fail(
            seed,
            Oracle::Selected,
            format!("the demuxer put out {dropped} buffers nobody plays"),
        );
    }
    let elements = tape.iter().map(|(el, _)| *el).max().map_or(0, |el| el + 1);
    for el in 0..elements {
        let of: Vec<What> = tape.iter().filter(|(e, _)| *e == el).map(|(_, what)| *what).collect();
        let count = |what: What| of.iter().filter(|w| **w == what).count();
        if count(What::Start) > 1 || count(What::Start) != count(What::Stop) {
            let detail = format!(
                "element {el} started {} times and stopped {}",
                count(What::Start),
                count(What::Stop)
            );
            return fail(seed, Oracle::Leak, detail);
        }
        let stop = of.iter().position(|w| *w == What::Stop).unwrap_or(of.len());
        if of[stop..].iter().any(|w| matches!(w, What::Row { .. } | What::Dropped { .. })) {
            return fail(
                seed,
                Oracle::AfterLeave,
                format!("element {el} was handed a buffer behind its stop"),
            );
        }
        let mut last = None;
        for what in &of {
            match what {
                What::Flush => last = None,
                What::Row { stream, seq, .. } | What::Dropped { stream, seq }
                    if *stream != EXTRA && *stream != HEAD =>
                {
                    if last.is_some_and(|last| *seq <= last) {
                        return fail(
                            seed,
                            Oracle::Whole,
                            format!("element {el} was handed {seq} behind {last:?}"),
                        );
                    }
                    last = Some(*seq);
                }
                _ => {}
            }
        }
        // The source that joins is a file from its start, so what is behind the position is old,
        // not late.
        let late = |w: &&What| matches!(w, What::Row { late, stream, .. } if *late > LATE * STEP && *stream != EXTRA);
        if let Some(row) = of.iter().find(late) {
            return fail(
                seed,
                Oracle::Timeline,
                format!("element {el} presented {row:?}, more than {LATE} buffers late"),
            );
        }
    }
    let mix: Vec<What> =
        tape.iter().filter(|(e, _)| *e == app.mix.0).map(|(_, what)| *what).collect();
    let mut joined = false;
    for what in &mix {
        match what {
            What::Linked { pad: 1, on } => joined = *on,
            What::Row { stream, .. } | What::Dropped { stream, .. }
                if *stream == EXTRA && !joined =>
            {
                return fail(
                    seed,
                    Oracle::AfterLeave,
                    "a buffer of the source that left, behind its leave".into(),
                );
            }
            What::Row { stream, .. } | What::Dropped { stream, .. }
                if *stream != 0 && *stream != EXTRA =>
            {
                return fail(
                    seed,
                    Oracle::Selected,
                    format!("the sink that always plays was handed stream {stream}"),
                );
            }
            _ => {}
        }
    }
    // A sink whose stream has ended is flushed once more when a source joins it.
    let handed = |w: &What| match w {
        What::Row { stream: 0, seq, .. } | What::Dropped { stream: 0, seq } => Some(*seq),
        _ => None,
    };
    let spans: Vec<Vec<u64>> = mix
        .split(|w| *w == What::Flush)
        .map(|span| span.iter().filter_map(handed).collect())
        .collect();
    // Every span between flushes is one run of the stream, not only the last.
    if let Some(gapped) = spans.iter().find(|s| s.windows(2).any(|w| w[1] != w[0] + 1)) {
        return fail(
            seed,
            Oracle::Whole,
            format!("the stream that always plays was handed {gapped:?} between flushes"),
        );
    }
    let seqs: Vec<u64> = spans.iter().rfind(|span| !span.is_empty()).cloned().unwrap_or_default();
    let whole = seqs.windows(2).all(|w| w[1] == w[0] + 1);
    if !whole || (!app.stopped && seqs.last() != Some(&(sc.buffers - 1))) {
        return fail(
            seed,
            Oracle::Whole,
            format!("the stream that always plays was handed {seqs:?}"),
        );
    }
    let sealed = |b: &&Branch| b.elements.iter().any(|el| app.sealed.contains(el));
    for branch in app
        .slots
        .iter()
        .flatten()
        .chain(&app.tap)
        .chain(&app.share)
        .filter(|b| b.to_end && !app.stopped && !sealed(b))
    {
        let last = tape.iter().rev().find_map(|(el, what)| match what {
            What::Row { seq, .. } | What::Dropped { seq, .. } if *el == branch.sink.0 => Some(*seq),
            _ => None,
        });
        if last != Some(sc.buffers - 1) {
            let detail = format!(
                "sink {} plays stream {} at the end and was last handed {last:?}",
                branch.sink.0, branch.stream
            );
            return fail(seed, Oracle::Whole, detail);
        }
    }
    for share in &app.shares {
        let of: Vec<What> =
            tape.iter().filter(|(e, _)| *e == share.sink.0).map(|(_, what)| *what).collect();
        let handed = |w: &What| match w {
            What::Row { stream, seq, .. } | What::Dropped { stream, seq } => Some((*stream, *seq)),
            _ => None,
        };
        let all: Vec<(u32, u64)> = of.iter().filter_map(handed).collect();
        let astray = all.iter().skip(1).any(|(stream, _)| *stream == HEAD);
        if astray || all.iter().any(|(stream, _)| *stream != HEAD && *stream != share.stream) {
            let detail = format!(
                "sink {} shares stream {} and was handed {all:?}",
                share.sink.0, share.stream
            );
            return fail(seed, Oracle::Shared, detail);
        }
        for span in of.split(|w| *w == What::Flush) {
            let seqs: Vec<u64> = span
                .iter()
                .filter_map(handed)
                .filter(|(s, _)| *s != HEAD)
                .map(|(_, seq)| seq)
                .collect();
            if seqs.windows(2).any(|w| w[1] != w[0] + 1) {
                let detail = format!(
                    "sink {} shares stream {} and was handed {seqs:?}",
                    share.sink.0, share.stream
                );
                return fail(seed, Oracle::Shared, detail);
            }
        }
    }
    for branch in &app.made {
        let of: Vec<What> =
            tape.iter().filter(|(e, _)| *e == branch.sink.0).map(|(_, what)| *what).collect();
        let stream_of = |w: &What| match w {
            What::Row { stream, .. } | What::Dropped { stream, .. } => Some(*stream),
            _ => None,
        };
        let handed: Vec<u32> = of.iter().filter_map(stream_of).collect();
        if handed.iter().any(|stream| *stream != branch.stream && *stream != HEAD) {
            let detail = format!(
                "sink {} plays stream {} and was handed another",
                branch.sink.0, branch.stream
            );
            return fail(seed, Oracle::Selected, detail);
        }
        if handed.iter().skip(1).any(|stream| *stream == HEAD) {
            let detail =
                format!("sink {} was handed a header behind its first buffer", branch.sink.0);
            return fail(seed, Oracle::Shared, detail);
        }
        let Some((position, restart)) = branch.joined.filter(|_| !branch.moved) else {
            continue;
        };
        if of.iter().filter(|w| **w == What::Flush).count() != 1 {
            continue;
        }
        let shown: Vec<u64> = of
            .iter()
            .filter_map(|w| match w {
                What::Row { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect();
        // A stream the source went back for was read again from the position, to the end of the
        // file.
        let plays = app.slots.iter().flatten().any(|b| b.sink == branch.sink);
        let cut_off =
            of.iter().any(|w| matches!(w, What::Linked { on: false, .. })) || sealed(&branch);
        if shown.is_empty() && restart && plays && !cut_off && !app.stopped && position < sc.buffers
        {
            let detail = format!(
                "sink {} joined at {position} with a restart and presented nothing",
                branch.sink.0
            );
            return fail(seed, Oracle::Timeline, detail);
        }
        let Some(first) = shown.first() else { continue };
        if *first < position {
            let detail =
                format!("sink {} joined at {position} and presented {first}", branch.sink.0);
            return fail(seed, Oracle::Timeline, detail);
        }
        if restart && *first != position {
            let detail = format!(
                "sink {} joined at {position} with a restart and presented {first} first",
                branch.sink.0
            );
            return fail(seed, Oracle::Timeline, detail);
        }
    }
    let mut formats = 0;
    for branch in &app.made {
        let want = What::Format { typed: branch.format.is_some(), id: branch.format.unwrap_or(0) };
        for told in tape
            .iter()
            .filter(|(el, w)| *el == branch.sink.0 && matches!(w, What::Format { .. }))
            .map(|(_, w)| *w)
        {
            formats += 1;
            if told != want {
                let detail = format!(
                    "sink {} of stream {} was told {told:?}, its branch carries {want:?}",
                    branch.sink.0, branch.stream
                );
                return fail(seed, Oracle::JoinFormat, detail);
            }
        }
    }
    Ok(formats)
}
