use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::{ALL, Command};
use profluens_core::ctx::{Ctx, Disposition, StopReason, Stream, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::PropError;
use profluens_core::error::{Error, GraphError, Scope, TopologyError};
use profluens_core::event::Event;
use profluens_core::format::{OfferDesc, Value};
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::level::Limit;
use profluens_core::log::{DebugSpec, Level, LogRecord, LogWriter, Loggable, Logs, with_logs};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{Cut, Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_core::topology::Leave;

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
static TWO_OUT: [PadDesc; 2] = [pad("a"), pad("b")];

static SRC: ElementDesc = desc("tsrc", &[], &ONE_OUT, SchedHint::Active);
static SRC2: ElementDesc = desc("tsrc2", &[], &TWO_OUT, SchedHint::Active);
static FWD: ElementDesc = desc("tfwd", &ONE_IN, &ONE_OUT, SchedHint::Active);
static INLINE: ElementDesc = desc("tinline", &ONE_IN, &ONE_OUT, SchedHint::Passive);
static MERGE: ElementDesc = desc("tmerge", &TWO_IN, &ONE_OUT, SchedHint::Active);
static SINK: ElementDesc = desc("tsink", &ONE_IN, &[], SchedHint::Active);
static SINK2: ElementDesc = desc("tsink2", &TWO_IN, &[], SchedHint::Active);
static DEMUX: ElementDesc = desc("tdemux", &ONE_IN, &[], SchedHint::Passive);
static IO_SINK: ElementDesc =
    ElementDesc { io: true, ..desc("tiosink", &ONE_IN, &[], SchedHint::Active) };

const IN: SinkPad = SinkPad(0);
const IN_B: SinkPad = SinkPad(1);
const OUT: SrcPad = SrcPad(0);
const OUT_B: SrcPad = SrcPad(1);
const STEP: Timestamp = Timestamp::from_millis(20);

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % n
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum What {
    Start,
    Stop,
    Eos,
    Flush,
    Paused,
    Row(u32, u64),
    Dropped(u32, u64),
    SrcLinked(u32, bool, Option<u64>),
    SrcJoined(u32),
    SinkLinked(u32, bool),
    Logs(bool),
    Latency(u64),
}

const HEADER: u32 = 99;

type Tape = Arc<Mutex<Vec<(u32, What)>>>;
type Reasons = Arc<Mutex<Vec<(u32, StopReason)>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Source,
    Forward,
    Sink,
    Merge,
    Demux,
    Mute,
    Sparse,
    Copy,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    Never,
    Start,
    Process(u64),
}

struct Node {
    desc: &'static ElementDesc,
    role: Role,
    tape: Tape,
    total: u64,
    next: u64,
    calls: u64,
    fail: Fail,
    paced: bool,
    high: u64,
    catching: Option<SrcPad>,
    /// Fails with `Branch` scope at the next process once set.
    doom: Option<Arc<AtomicBool>>,
    /// 0 waits in `process` once, 1 says it is there, 2 lets it go.
    hold: Option<Arc<AtomicU8>>,
    told: u64,
    reasons: Reasons,
}

#[derive(Debug)]
struct Boom;

impl std::fmt::Display for Boom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("boom")
    }
}

impl std::error::Error for Boom {}

impl Node {
    fn note(&self, ctx: &Ctx, what: What) {
        self.tape.lock().unwrap().push((ctx.element().0, what));
    }

    fn emit(&self, ctx: &mut Ctx, pad: SrcPad, stream: u32, seq: u64) -> bool {
        let Some(mut buf) = ctx.try_alloc() else {
            return false;
        };
        let bytes = buf.memory.as_mut_full();
        bytes[..8].copy_from_slice(&seq.to_le_bytes());
        bytes[8..12].copy_from_slice(&stream.to_le_bytes());
        buf.memory.set_len(12);
        (buf.pts, buf.duration) = (Timestamp(seq * STEP.0), STEP);
        ctx.out(pad).push(buf);
        true
    }
}

fn read(buf: &profluens_core::buffer::Buffer) -> (u32, u64) {
    let b = buf.memory.data();
    (
        u32::from_le_bytes(b[8..12].try_into().unwrap()),
        u64::from_le_bytes(b[..8].try_into().unwrap()),
    )
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        for n in 0..if self.role == Role::Demux { 3 } else { 0 } {
            ctx.add_stream(Stream {
                kind: if n == 0 { StreamKind::Video } else { StreamKind::Audio },
                codec: "bytes",
                language: if n == 2 { "nb".into() } else { "en".into() },
                title: format!("stream {n}").into(),
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
        if self.fail == Fail::Start {
            return Err(Error::element_failed(ctx.element(), Boom));
        }
        self.note(ctx, What::Start);
        self.note(ctx, What::Logs(Loggable::enabled(&*ctx, Level::Info)));
        profluens_core::log!(&*ctx, Level::Info, "started");
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.calls += 1;
        if self.fail == Fail::Process(self.calls) {
            return Err(Error::element_failed(ctx.element(), Boom));
        }
        if self.doom.as_ref().is_some_and(|d| d.load(Ordering::Acquire)) {
            return Err(Error::element_failed(ctx.element(), Boom).scoped(Scope::Branch));
        }
        if let Some(hold) = self.hold.take() {
            hold.store(1, Ordering::Release);
            while hold.load(Ordering::Acquire) != 2 {
                std::thread::yield_now();
            }
        }
        if ctx.path_latency().0 != self.told {
            self.told = ctx.path_latency().0;
            self.note(ctx, What::Latency(self.told));
        }
        match self.role {
            Role::Source => {
                if self.next >= self.total {
                    return Ok(Flow::Eos);
                }
                let pads = self.desc.srcs.len() as u32;
                let linked: Vec<u32> = (0..pads).filter(|p| ctx.pad_linked(SrcPad(*p))).collect();
                if ctx.pool_stats().outstanding as usize + linked.len()
                    > ctx.pool_stats().max_slots as usize
                {
                    ctx.await_recycle();
                    return Ok(Flow::Ok);
                }
                for p in linked {
                    self.emit(ctx, SrcPad(p), p, self.next);
                }
                self.next += 1;
            }
            Role::Mute => {}
            Role::Sparse => {
                if self.next > 0 {
                    return Ok(Flow::Eos);
                }
                let pads = self.desc.srcs.len() as u32;
                let linked: Vec<u32> = (0..pads).filter(|p| ctx.pad_linked(SrcPad(*p))).collect();
                for p in linked {
                    ctx.push_event(SrcPad(p), Event::Gap { until: Timestamp::from_secs(3_600) });
                }
                self.next = 1;
            }
            Role::Forward => {
                while let Some(buf) = inputs.pop() {
                    ctx.out(OUT).push(buf);
                }
            }
            Role::Copy => {
                while inputs.front().is_some() {
                    let Some(mut out) = ctx.try_alloc() else {
                        ctx.await_recycle();
                        break;
                    };
                    let Some(buf) = inputs.pop() else { break };
                    out.memory.as_mut_full()[..12].copy_from_slice(buf.memory.data());
                    out.memory.set_len(12);
                    (out.pts, out.duration) = (buf.pts, buf.duration);
                    ctx.out(OUT).push(out);
                }
            }
            Role::Demux => {
                while let Some(buf) = inputs.pop() {
                    let (_, seq) = read(&buf);
                    for stream in 0..3 {
                        let Some(pad) = ctx.stream_pad(stream).filter(|pad| ctx.pad_linked(*pad))
                        else {
                            continue;
                        };
                        if seq > self.high || self.high == 0 || self.catching == Some(pad) {
                            let mut out = ctx.alloc_exact(12).expect("exact");
                            out.memory.as_mut_full()[..8].copy_from_slice(&seq.to_le_bytes());
                            out.memory.as_mut_full()[8..12].copy_from_slice(&stream.to_le_bytes());
                            out.memory.set_len(12);
                            (out.pts, out.duration) = (Timestamp(seq * STEP.0), STEP);
                            ctx.out(pad).push(out);
                        }
                    }
                    if seq >= self.high {
                        (self.high, self.catching) = (seq, None);
                    }
                }
            }
            Role::Sink | Role::Merge => {
                let merge = self.role == Role::Merge;
                let mut took = Vec::new();
                while let Some(front) = inputs.front() {
                    if self.paced && !ctx.due(front.pts) {
                        break;
                    }
                    took.extend(inputs.pop());
                }
                for pad in [IN, IN_B] {
                    while let Some(buf) = ctx.pop_input_on(pad) {
                        took.push(buf);
                    }
                }
                for buf in took {
                    let (stream, seq) = read(&buf);
                    let early = ctx.before_presentation(buf.pts, buf.duration);
                    self.note(
                        ctx,
                        if early { What::Dropped(stream, seq) } else { What::Row(stream, seq) },
                    );
                    if merge {
                        ctx.out(OUT).push(buf);
                    }
                }
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => self.note(ctx, What::Eos),
            Event::Paused => self.note(ctx, What::Paused),
            Event::FlushStart => {
                self.note(ctx, What::Flush);
                if self.role == Role::Source {
                    self.next = ctx.seek_target().map_or(0, |t| t.to_byte);
                }
                if self.role == Role::Demux {
                    (self.high, self.catching) = (0, None);
                }
            }
            Event::SrcLinked { pad, on, restart } => {
                self.note(ctx, What::SrcLinked(pad.0, *on, restart.map(|t| t.to_byte)));
                if restart.is_some() {
                    self.catching = Some(*pad);
                }
            }
            Event::SrcJoined { pad } => {
                self.note(ctx, What::SrcJoined(pad.0));
                self.emit(ctx, *pad, HEADER, self.next);
            }
            Event::SinkLinked { pad, on } => self.note(ctx, What::SinkLinked(pad.0, *on)),
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.reasons.lock().unwrap().push((ctx.element().0, ctx.stop_reason()));
        self.note(ctx, What::Stop);
    }
}

struct Rig {
    p: Pipeline,
    tape: Tape,
    reasons: Reasons,
    clock: MockClock,
}

impl Node {
    fn sink(desc: &'static ElementDesc, tape: Tape) -> Node {
        Node {
            desc,
            role: Role::Sink,
            tape,
            total: 0,
            next: 0,
            calls: 0,
            fail: Fail::Never,
            paced: false,
            high: 0,
            catching: None,
            doom: None,
            hold: None,
            told: 0,
            reasons: Reasons::default(),
        }
    }
}

impl Rig {
    fn new() -> Rig {
        Rig::logging("")
    }

    fn logging(debug: &str) -> Rig {
        let clock = MockClock::starting_at(Timestamp::from_secs(5));
        let mut p = Pipeline::with(DebugSpec::parse(debug), false, Arc::new(clock.clone()));
        p.set_pool(64, 24);
        Rig { p, tape: Tape::default(), reasons: Reasons::default(), clock }
    }

    fn node(&self, desc: &'static ElementDesc, role: Role) -> Node {
        let (tape, total, reasons) = (Arc::clone(&self.tape), 400, Arc::clone(&self.reasons));
        Node {
            desc,
            role,
            tape,
            total,
            next: 0,
            calls: 0,
            fail: Fail::Never,
            paced: false,
            high: 0,
            catching: None,
            doom: None,
            hold: None,
            told: 0,
            reasons,
        }
    }

    fn add(&mut self, desc: &'static ElementDesc, role: Role) -> ElementId {
        let node = self.node(desc, role);
        self.p.add(node)
    }

    fn link(&mut self, src: (ElementId, SrcPad), sink: (ElementId, SinkPad)) -> LinkId {
        self.p.link(src, sink).expect("link")
    }

    fn step(&mut self) -> Stepped {
        self.p.step(Arc::new(self.clock.clone())).expect("step")
    }

    fn of(&self, el: ElementId) -> Vec<What> {
        self.tape.lock().unwrap().iter().filter(|(e, _)| *e == el.0).map(|(_, w)| *w).collect()
    }

    fn rows(&self, el: ElementId) -> Vec<u64> {
        let seq = |w: What| match w {
            What::Row(_, seq) => Some(seq),
            _ => None,
        };
        self.of(el).into_iter().filter_map(seq).collect()
    }

    fn reasons(&self, el: ElementId) -> Vec<StopReason> {
        self.reasons.lock().unwrap().iter().filter(|(e, _)| *e == el.0).map(|(_, r)| *r).collect()
    }

    fn count(&self, el: ElementId, what: What) -> usize {
        self.of(el).iter().filter(|w| **w == what).count()
    }

    fn run(&self, run: &mut Stepped, rng: &mut Rng, passes: Option<u64>) -> bool {
        for _ in 0..passes.unwrap_or(2_000_000) {
            run.service();
            let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
            if ready.is_empty() {
                let Some(to) = run.next_deadline() else { break };
                self.clock.advance(Timestamp(to.0 - self.clock.now().0));
                continue;
            }
            run.pass(ready[rng.below(ready.len() as u64) as usize]).expect("pass");
        }
        (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
    }

    /// Passes of the groups ready now, the clock standing still, until none is or `passes` ran.
    /// How many ran.
    fn run_still(&self, run: &mut Stepped, rng: &mut Rng, passes: u64) -> u64 {
        for n in 0..passes {
            run.service();
            let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
            if ready.is_empty() {
                return n;
            }
            run.pass(ready[rng.below(ready.len() as u64) as usize]).expect("pass");
        }
        passes
    }
}

fn contiguous(rows: &[u64]) -> bool {
    rows.windows(2).all(|w| w[1] == w[0] + 1)
}

fn topology(result: Result<u32, Error>) -> TopologyError {
    match result {
        Err(Error::Topology(e)) => e,
        other => panic!("expected a topology error, got {other:?}"),
    }
}

fn bus(p: &Pipeline) -> Vec<BusMessage> {
    std::iter::from_fn(|| p.bus().try_recv()).collect()
}

fn fan_out() -> (Rig, ElementId, ElementId) {
    let mut r = Rig::new();
    let src = r.add(&SRC2, Role::Source);
    let a = r.add(&SINK, Role::Sink);
    r.link(src.src(OUT), a.sink(IN));
    (r, src, a)
}

#[test]
fn a_branch_joins_a_running_source_and_gets_what_follows() {
    for seed in scaled(40) {
        let (mut r, src, a) = fan_out();
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(60)));
        let b = r.add(&SINK, Role::Sink);
        r.link(src.src(OUT_B), b.sink(IN));
        r.p.commit().expect("commit");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(r.rows(a), (0..400).collect::<Vec<_>>(), "seed {seed}");
        let got = r.rows(b);
        assert!(!got.is_empty() && got[0] > 0, "seed {seed}: from the join on, {got:?}");
        assert!(contiguous(&got) && got.last() == Some(&399), "seed {seed}: {got:?}");
        assert_eq!((r.count(b, What::Start), r.count(b, What::Stop)), (1, 1), "seed {seed}");
        assert_eq!(r.count(b, What::Flush), 1, "seed {seed}");
        assert_eq!(
            r.of(src).iter().filter(|w| matches!(w, What::SrcLinked(1, true, None))).count(),
            1
        );
    }
}

#[test]
fn a_branch_that_is_cut_stops_and_takes_nothing_more() {
    for seed in scaled(40) {
        let (mut r, src, a) = fan_out();
        let b = r.add(&SINK, Role::Sink);
        r.link(src.src(OUT_B), b.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(60)));
        r.p.remove(b, Leave::Cut).expect("remove");
        r.p.commit().expect("commit");
        let before = r.rows(b).len();
        for g in 0..run.groups() {
            run.pass(g).expect("pass");
        }
        let at_cut = r.rows(b).len();
        assert!(at_cut == before, "seed {seed}: nothing is taken in the pass that applies the cut");
        assert_eq!(r.of(b).last(), Some(&What::Stop), "seed {seed}");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(r.rows(a), (0..400).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(r.rows(b).len(), at_cut, "seed {seed}");
        assert_eq!((r.count(b, What::Eos), r.count(b, What::Stop)), (0, 1), "seed {seed}");
        assert!(r.of(src).contains(&What::SrcLinked(1, false, None)), "seed {seed}");
        let removed =
            |m: &BusMessage| matches!(m, BusMessage::ElementRemoved { element } if *element == b);
        assert_eq!(bus(&r.p).iter().filter(|m| removed(m)).count(), 1, "seed {seed}");
        r.p.settle();
        assert!(
            matches!(r.p.remove(b, Leave::Cut), Err(Error::Link(_))),
            "seed {seed}: it is gone"
        );
    }
}

#[test]
fn a_branch_that_drains_gets_its_end_of_stream_in_order_and_then_stops() {
    for seed in scaled(40) {
        let mut r = Rig::new();
        let src = r.add(&SRC2, Role::Source);
        let a = r.add(&SINK, Role::Sink);
        let (fwd, inline, b) =
            (r.add(&FWD, Role::Forward), r.add(&INLINE, Role::Forward), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), a.sink(IN));
        r.link(src.src(OUT_B), fwd.sink(IN));
        r.link(fwd.src(OUT), inline.sink(IN));
        r.link(inline.src(OUT), b.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(80)));
        for el in [fwd, inline, b] {
            r.p.remove(el, Leave::Drain).expect("remove");
        }
        r.p.commit().expect("commit");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(r.rows(a), (0..400).collect::<Vec<_>>(), "seed {seed}");
        let got = r.rows(b);
        assert!(contiguous(&got) && got.len() < 400, "seed {seed}: a prefix, {got:?}");
        for el in [fwd, inline, b] {
            let saw = r.of(el);
            let tail: Vec<What> = saw.iter().rev().take(2).rev().copied().collect();
            assert_eq!(tail, [What::Eos, What::Stop], "seed {seed}: element {}", el.0);
        }
        let tape = r.of(b);
        let last_row = tape.iter().rposition(|w| matches!(w, What::Row(..)));
        let eos = tape.iter().position(|w| *w == What::Eos);
        assert!(last_row < eos, "seed {seed}");
    }
}

#[test]
fn a_relink_moves_the_stream_to_another_sink_at_a_batch_boundary() {
    for seed in scaled(40) {
        let mut r = Rig::new();
        let (src, a, b) =
            (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink), r.add(&SINK, Role::Sink));
        let link = r.link(src.src(OUT), a.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(50)));
        r.p.relink(link, b.sink(IN)).expect("relink");
        r.p.commit().expect("commit");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        let (first, second) = (r.rows(a), r.rows(b));
        assert!(contiguous(&first) && first.first() == Some(&0), "seed {seed}: {first:?}");
        assert!(contiguous(&second) && second.last() == Some(&399), "seed {seed}: {second:?}");
        assert!(first.last() < second.first(), "seed {seed}: nothing is presented twice");
        assert!(r.of(a).contains(&What::SinkLinked(0, false)), "seed {seed}");
        assert_eq!((r.count(a, What::Eos), r.count(a, What::Stop)), (1, 1), "seed {seed}");
        assert_eq!(r.count(src, What::SrcLinked(0, true, None)), 1, "seed {seed}");
    }
}

#[test]
fn a_source_joins_a_running_aggregator_once_it_has_prerolled() {
    for seed in scaled(40) {
        let mut r = Rig::new();
        let (src, merge, sink) =
            (r.add(&SRC, Role::Source), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), merge.sink(IN));
        r.link(merge.src(OUT), sink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(50)));
        let mut second = r.node(&SRC2, Role::Source);
        second.total = 30;
        let second = r.p.add(second);
        r.link(second.src(OUT_B), merge.sink(IN_B));
        let change = r.p.commit().expect("commit");
        assert!(!r.of(merge).contains(&What::SinkLinked(1, true)), "seed {seed}");
        assert!(!bus(&r.p).iter().any(|m| matches!(m, BusMessage::Changed { .. })), "seed {seed}");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert!(
            bus(&r.p)
                .iter()
                .any(|m| matches!(m, BusMessage::Changed { change: c } if *c == change))
        );
        let tape = r.of(merge);
        let told =
            tape.iter().position(|w| *w == What::SinkLinked(1, true)).expect("told of its new pad");
        let first =
            tape.iter().position(|w| matches!(w, What::Row(1, _))).expect("took the new stream");
        assert!(told < first, "seed {seed}");
        let of = |stream: u32| -> Vec<u64> {
            let seq = |w: &What| match w {
                What::Row(s, seq) if *s == stream => Some(*seq),
                _ => None,
            };
            tape.iter().filter_map(seq).collect()
        };
        assert_eq!(of(0), (0..400).collect::<Vec<_>>(), "seed {seed}: the first stream is whole");
        assert_eq!(
            of(1),
            (0..30).collect::<Vec<_>>(),
            "seed {seed}: and so is the one that joined"
        );
        assert_eq!(r.rows(sink).len(), 430, "seed {seed}");
    }
}

#[test]
fn a_sparse_branch_is_spliced_on_the_gap_it_says() {
    for (role, spliced) in [(Role::Sparse, true), (Role::Mute, false)] {
        let mut r = Rig::new();
        let (src, merge, sink) =
            (r.add(&SRC, Role::Source), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), merge.sink(IN));
        r.link(merge.src(OUT), sink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(4));
        assert!(!r.run(&mut run, &mut rng, Some(50)));
        let second = r.p.add(r.node(&SRC2, role));
        r.link(second.src(OUT_B), merge.sink(IN_B));
        let change = r.p.commit().expect("commit");
        assert!(!r.run(&mut run, &mut rng, Some(60)));
        let changed = bus(&r.p)
            .iter()
            .any(|m| matches!(m, BusMessage::Changed { change: c } if *c == change));
        assert_eq!(
            changed,
            spliced,
            "{}",
            if spliced { "the gap is what it waited for" } else { "nothing was said" }
        );
        assert_eq!(r.of(merge).contains(&What::SinkLinked(1, true)), spliced);
        if !spliced {
            assert_eq!(r.p.cancel(), Some(change));
        }
        assert!(r.run(&mut run, &mut rng, None), "the run ends");
        run.finish().expect("finish");
        assert_eq!(r.rows(sink).len(), 400, "the stream that played is whole");
    }
}

#[test]
fn a_cap_on_the_pools_together_cuts_each_and_the_run_is_whole() {
    for (cap, slots) in [(64 * 24, 12), (64 * 6, 3), (1, 3), (u64::MAX, 72)] {
        let mut r = Rig::new();
        let (src, fwd, sink) =
            (r.add(&SRC, Role::Source), r.add(&FWD, Role::Forward), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), fwd.sink(IN));
        r.link(fwd.src(OUT), sink.sink(IN));
        r.p.set_pool_cap(cap);
        let tap = r.p.tap_handle();
        let (mut run, mut rng) = (r.step(), Rng(cap));
        assert_eq!(tap.pool().max_slots, slots, "three pools of 24 slots of 64 bytes under {cap}");
        assert!(r.run(&mut run, &mut rng, None), "the run ends under {cap}");
        let report = run.finish().expect("finish");
        assert_eq!(r.rows(sink).len(), 400, "every buffer under {cap}");
        assert!(
            report.pool_high_water <= slots,
            "{} slots out under {cap}",
            report.pool_high_water
        );
    }
}

static LATE: ElementDesc = ElementDesc {
    latency: LatencyDesc {
        min: Timestamp::from_millis(30),
        max: Timestamp::from_millis(30),
        is_live: true,
        jitter: Timestamp::ZERO,
    },
    ..desc("tlate", &[], &TWO_OUT, SchedHint::Active)
};

#[test]
fn a_change_that_raises_the_latency_grows_it_or_holds_it_as_asked() {
    use profluens_core::pipeline::LatencyPolicy;
    let ms = Timestamp::from_millis;
    for policy in [LatencyPolicy::Grow, LatencyPolicy::Hold] {
        let mut r = Rig::new();
        let (src, merge, sink) =
            (r.add(&SRC, Role::Source), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), merge.sink(IN));
        r.link(merge.src(OUT), sink.sink(IN));
        r.p.set_latency_policy(policy);
        let (mut run, mut rng) = (r.step(), Rng(9));
        assert!(!r.run(&mut run, &mut rng, Some(50)));
        let mut late = r.node(&LATE, Role::Source);
        late.total = 30;
        let late = r.p.add(late);
        r.link(late.src(OUT_B), merge.sink(IN_B));
        r.p.commit().expect("commit");
        assert!(r.run(&mut run, &mut rng, None), "the run ends");
        run.finish().expect("finish");
        let said = bus(&r.p);
        let grown = said.iter().any(|m| matches!(m, BusMessage::LatencyChanged { old, new } if (*old, *new) == (ms(0), ms(30))));
        let held = said.iter().any(|m| matches!(m, BusMessage::LatencyHeld { kept, asked } if (*kept, *asked) == (ms(0), ms(30))));
        assert_eq!(
            (grown, held),
            (policy == LatencyPolicy::Grow, policy == LatencyPolicy::Hold),
            "{policy:?}"
        );
        let first = r.of(merge).iter().filter(|w| matches!(w, What::Row(0, _))).count();
        assert_eq!(first, 400, "{policy:?}: the stream that played is whole");
        let joined = r.of(merge).iter().filter(|w| matches!(w, What::Row(1, _))).count();
        match policy {
            LatencyPolicy::Grow => {
                assert_eq!(joined, 30, "grown, nothing of what joined is dropped")
            }
            LatencyPolicy::Hold => assert!(joined <= 30),
        }
    }
}

#[test]
fn a_branch_that_fails_before_its_splice_never_was() {
    for (seed, fail) in
        (0..40).map(|s| (s, if s % 2 == 0 { Fail::Start } else { Fail::Process(1) }))
    {
        let mut r = Rig::new();
        let (src, merge, sink) =
            (r.add(&SRC, Role::Source), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), merge.sink(IN));
        r.link(merge.src(OUT), sink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(50)));
        let mut bad = r.node(&FWD, Role::Forward);
        bad.fail = fail;
        let (feeder, bad) = (r.add(&SRC, Role::Source), r.p.add(bad));
        r.link(feeder.src(OUT), bad.sink(IN));
        let link = r.link(bad.src(OUT), merge.sink(IN_B));
        let change = r.p.commit().expect("commit");
        assert_eq!(
            topology(r.p.commit()),
            TopologyError::Busy,
            "seed {seed}: one change at a time"
        );
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        let failed = |m: &BusMessage| matches!(m, BusMessage::ChangeFailed { change: c, .. } if *c == change);
        let messages = bus(&r.p);
        assert!(messages.iter().any(failed), "seed {seed}");
        assert!(!messages.iter().any(|m| matches!(m, BusMessage::Error { .. })), "seed {seed}");
        assert!(!r.of(merge).iter().any(|w| matches!(w, What::SinkLinked(..))), "seed {seed}");
        assert_eq!(r.rows(sink), (0..400).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(r.count(feeder, What::Start), r.count(feeder, What::Stop), "seed {seed}");
        assert_eq!(r.count(bad, What::Start), r.count(bad, What::Stop), "seed {seed}");
        r.p.settle();
        assert!(r.p.negotiated(link).is_none(), "seed {seed}");
    }
}

#[test]
fn a_change_the_run_cannot_take_is_refused_with_its_reason() {
    let mut r = Rig::new();
    let (src, inline, sink) =
        (r.add(&SRC2, Role::Source), r.add(&INLINE, Role::Forward), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), inline.sink(IN));
    let last = r.link(inline.src(OUT), sink.sink(IN));
    let late = r.add(&SINK, Role::Sink);
    assert_eq!(topology(r.p.commit()), TopologyError::NotRunning);
    let mut run = r.step();
    r.link(src.src(OUT_B), late.sink(IN));
    assert_eq!(
        r.p.commit().unwrap_err().to_string(),
        Error::Graph(GraphError::NonTailBranch { src, sink: late }).to_string()
    );
    r.link(src.src(OUT_B), late.sink(IN));
    assert!(r.p.commit().is_err());
    r.p.remove(inline, Leave::Cut).expect("remove");
    assert_eq!(
        topology(r.p.commit()),
        TopologyError::PartialGroup { element: src, removed: inline }
    );
    r.p.unlink(LinkId(0)).expect("unlink");
    assert_eq!(topology(r.p.commit()), TopologyError::Inline { src, sink: inline });
    let other = r.add(&SRC, Role::Source);
    r.link(other.src(OUT), sink.sink(IN));
    assert_eq!(
        topology(r.p.commit()),
        TopologyError::PadLinked { element: sink, src: false, pad: 0 }
    );
    assert_eq!(
        topology(r.p.unlink(LinkId(99)).and(Ok(0))),
        TopologyError::NoSuchLink { link: LinkId(99) }
    );
    assert_eq!(
        topology(r.p.select(src, 0).and(Ok(0))),
        TopologyError::NoSuchStream { element: src, stream: 0 }
    );
    assert_eq!(
        topology(r.p.commit_at(Timestamp::NONE, &[(inline, 0, Timestamp::ZERO)])),
        TopologyError::RestartAtRing { element: inline }
    );
    assert!(r.p.negotiated(last).is_some(), "the run's links are as they were");
    let mut rng = Rng(1);
    assert!(r.run(&mut run, &mut rng, None));
    run.finish().expect("finish");
    assert_eq!(r.rows(sink), (0..400).collect::<Vec<_>>());
}

#[test]
fn an_io_element_joining_a_run_with_no_reactor_factory_is_refused() {
    let (mut r, src, a) = fan_out();
    let (mut run, mut rng) = (r.step(), Rng(3));
    assert!(!r.run(&mut run, &mut rng, Some(40)));
    let late = r.add(&IO_SINK, Role::Sink);
    r.link(src.src(OUT_B), late.sink(IN));
    let err = r.p.commit().unwrap_err();
    assert!(
        matches!(err, Error::NoReactor { element, element_name: "tiosink" } if element == late),
        "{err:?}"
    );
    assert!(r.run(&mut run, &mut rng, None), "the run goes on without it");
    run.finish().expect("finish");
    assert_eq!(r.rows(a), (0..400).collect::<Vec<_>>());
    assert!(r.of(late).is_empty(), "the refused element never started");
}

#[derive(Clone, Copy, Debug)]
enum Act {
    Join,
    Leave,
    Command(Command),
}

#[test]
fn every_command_meets_a_join_and_a_leave_in_flight_in_either_order() {
    for seed in scaled(600) {
        let (mut r, src, a) = fan_out();
        let (mut run, mut rng) = (r.step(), Rng(seed ^ 0x51ed));
        let commands = [
            Command::Seek { seek_gen: 0, to_time: Timestamp(100 * STEP.0) },
            Command::SkipTo { to_time: Timestamp(300 * STEP.0) },
            Command::Pause,
            Command::Resume,
            Command::SetRate { rate: profluens_core::time::Rate::new(2, 1).expect("rate") },
            Command::Stop,
        ];
        let mut script: Vec<(u64, Act)> = Vec::new();
        script.push((20 + rng.below(40), Act::Join));
        script.push((70 + rng.below(40), Act::Leave));
        for (k, c) in commands.iter().enumerate() {
            if k < 5 || seed % 2 == 0 {
                script.push((10 + rng.below(110), Act::Command(*c)));
            }
        }
        script.sort_by_key(|(at, _)| *at);
        let (mut joined, mut stopped, mut paused) = (None, false, false);
        let leave = if seed % 3 == 0 { Leave::Drain } else { Leave::Cut };
        let mut at = 0;
        for (when, act) in script {
            r.run(&mut run, &mut rng, Some(when - at));
            at = when;
            match act {
                Act::Join => {
                    let b = r.add(&SINK, Role::Sink);
                    r.link(src.src(OUT_B), b.sink(IN));
                    joined = r.p.commit().ok().map(|_| b);
                }
                Act::Leave => {
                    if let Some(b) = joined {
                        r.p.remove(b, leave).expect("remove");
                        if r.p.commit().is_err() {
                            joined = None;
                        }
                    }
                }
                Act::Command(Command::Seek { to_time, .. }) => {
                    let _ =
                        run.control().seek_bytes(to_time, &[(src, to_time.0 / STEP.0, to_time)]);
                }
                Act::Command(c) => {
                    let taken = run.control().append(c).is_ok();
                    stopped |= taken && c == Command::Stop;
                    paused = match c {
                        Command::Pause if taken => true,
                        Command::Resume if taken => false,
                        _ => paused,
                    };
                }
            }
        }
        if paused && !stopped {
            run.control().append(Command::Resume).expect("resume");
        }
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        let tape = r.tape.lock().unwrap().clone();
        for el in 0..4u32 {
            let count = |what: What| tape.iter().filter(|(e, w)| *e == el && *w == what).count();
            assert_eq!(count(What::Start), count(What::Stop), "seed {seed}: element {el}");
            assert!(count(What::Start) <= 1, "seed {seed}: element {el}");
            let stop = tape.iter().position(|(e, w)| *e == el && *w == What::Stop);
            let after = |(i, (e, w)): (usize, &(u32, What))| {
                *e == el && Some(i) > stop && matches!(w, What::Row(..))
            };
            assert!(
                !tape.iter().enumerate().any(after),
                "seed {seed}: element {el} took a buffer after its stop"
            );
        }
        if !stopped {
            let rows = r.of(a);
            let flush = rows.iter().rposition(|w| *w == What::Flush).map_or(0, |i| i + 1);
            let handed = |w: &What| match w {
                What::Row(_, seq) | What::Dropped(_, seq) => Some(*seq),
                _ => None,
            };
            let seqs: Vec<u64> = rows[flush..].iter().filter_map(handed).collect();
            assert!(contiguous(&seqs) && seqs.last() == Some(&399), "seed {seed}: {seqs:?}");
            assert!(
                matches!(rows.iter().rfind(|w| handed(w).is_some()), Some(What::Row(_, 399))),
                "seed {seed}"
            );
        }
    }
}

fn demuxed() -> (Rig, ElementId, ElementId) {
    let mut r = Rig::new();
    let (src, demux) = (r.add(&SRC, Role::Source), r.add(&DEMUX, Role::Demux));
    r.link(src.src(OUT), demux.sink(IN));
    r.p.set_select_all(false);
    (r, src, demux)
}

#[test]
fn a_stream_has_no_pad_until_it_is_selected() {
    let (mut r, _, demux) = demuxed();
    let pads = r.p.preroll().expect("preroll");
    assert!(pads.srcs.is_empty(), "describing streams adds no pad");
    let streams = r.p.streams(demux);
    assert_eq!(streams.len(), 3);
    assert_eq!(
        (streams[0].kind, &*streams[2].language, &*streams[1].name),
        (StreamKind::Video, "nb", "src_1")
    );
    assert_eq!(r.p.stream_pad(demux, 1), None);
    let pad = r.p.select(demux, 1).expect("select");
    assert_eq!((pad, r.p.stream_pad(demux, 1)), (SrcPad(0), Some(SrcPad(0))));
    assert_eq!(r.p.select(demux, 1).expect("select again"), pad, "a stream keeps its pad");
    let sink = r.add(&SINK, Role::Sink);
    r.link(demux.src(pad), sink.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(3));
    assert!(r.run(&mut run, &mut rng, None));
    run.finish().expect("finish");
    let tape = r.of(sink);
    assert!(
        tape.iter().all(|w| !matches!(w, What::Row(s, _) if *s != 1)),
        "only the selected stream"
    );
    assert_eq!(r.rows(sink), (0..400).collect::<Vec<_>>());
    assert_eq!(r.p.counters(demux).drops, 0);
}

#[test]
fn the_default_preroll_selects_every_stream_as_a_pad() {
    let mut r = Rig::new();
    let (src, demux) = (r.add(&SRC, Role::Source), r.add(&DEMUX, Role::Demux));
    r.link(src.src(OUT), demux.sink(IN));
    let pads = r.p.preroll().expect("preroll");
    let names: Vec<&str> = pads.srcs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["src_0", "src_1", "src_2"]);
    assert_eq!(r.p.src_pad(demux, "src_2").expect("by name"), SrcPad(2));
}

#[test]
fn a_switch_restarts_the_new_stream_at_its_target_and_the_others_play_on() {
    for seed in scaled(40) {
        let (mut r, src, demux) = demuxed();
        r.p.preroll().expect("preroll");
        let (video, old) =
            (r.p.select(demux, 0).expect("select"), r.p.select(demux, 1).expect("select"));
        let (vsink, asink) = (r.add(&SINK, Role::Sink), r.add(&SINK, Role::Sink));
        r.link(demux.src(video), vsink.sink(IN));
        r.link(demux.src(old), asink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(90)));
        let position = *r.rows(asink).last().expect("played some");
        let new = r.p.select(demux, 2).expect("select");
        let bsink = r.add(&SINK, Role::Sink);
        r.link(demux.src(new), bsink.sink(IN));
        r.p.remove(asink, Leave::Cut).expect("remove");
        let at = Timestamp(position * STEP.0);
        let targets = [(src, position, at), (demux, position, at)];
        r.p.commit_at(at, &targets).expect("commit");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(r.rows(vsink), (0..400).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(r.rows(bsink), (position..400).collect::<Vec<_>>(), "seed {seed}");
        assert!(r.rows(asink).len() as u64 <= position + 1, "seed {seed}: the old branch is gone");
        assert_eq!(r.count(src, What::Flush), 1, "seed {seed}: the source went back");
        assert_eq!(r.count(demux, What::Flush), 0, "seed {seed}: the demuxer was not flushed");
        assert!(r.of(demux).contains(&What::SrcLinked(new.0, true, Some(position))), "seed {seed}");
        assert_eq!(r.count(vsink, What::Flush), 0, "seed {seed}");
    }
}

#[test]
fn a_restart_runs_while_the_tails_ring_is_full() {
    // A limit inside the ring's four batches rests the group on the level, one past them stages
    // a batch at the ring's capacity. The sink holds what is not due, and the clock stands.
    for (seed, limit) in scaled(20).flat_map(|seed| [(seed, 5), (seed, 50)]) {
        let (mut r, src, demux) = demuxed();
        r.p.preroll().expect("preroll");
        let video = r.p.select(demux, 0).expect("select");
        let vsink = r.p.add(Node { paced: true, ..r.node(&SINK, Role::Sink) });
        r.p.set_link_limit(vsink, Limit::time(Timestamp(limit * STEP.0)));
        r.link(demux.src(video), vsink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(r.run_still(&mut run, &mut rng, 10_000) < 10_000, "seed {seed}: the ring filled");
        let group = (0..run.groups()).find(|g| run.elements(*g).contains(&demux)).expect("group");
        assert!(matches!(run.outcome(group), Some(Outcome::Blocked { .. })), "seed {seed}");
        let (before, made) = (r.rows(vsink), r.p.counters(demux).buffers_out);
        let new = r.p.select(demux, 2).expect("select");
        let bsink = r.add(&SINK, Role::Sink);
        r.link(demux.src(new), bsink.sink(IN));
        let targets = [(src, 0, Timestamp::ZERO), (demux, 0, Timestamp::ZERO)];
        r.p.commit_at(Timestamp::NONE, &targets).expect("commit");
        assert!(r.run_still(&mut run, &mut rng, 10_000) < 10_000, "seed {seed}: it rests again");
        let got = r.rows(bsink);
        assert!(got.len() > 1 && got[0] == 0 && contiguous(&got), "seed {seed} {limit}: {got:?}");
        assert_eq!(r.rows(vsink), before, "seed {seed}: the video ring was not drained");
        assert!(
            got.len() as u64 <= made + 1,
            "seed {seed}: caught up, the first new video row rests the group, {got:?}"
        );
        assert!(matches!(run.outcome(group), Some(Outcome::Blocked { .. })), "seed {seed}");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(r.rows(vsink), (0..400).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(r.rows(bsink), (0..400).collect::<Vec<_>>(), "seed {seed}");
    }
}

#[test]
fn a_join_lands_on_the_timeline_at_what_is_presented() {
    let mut r = Rig::new();
    let mut paced = r.node(&SINK, Role::Sink);
    paced.paced = true;
    let (src, a) = (r.add(&SRC2, Role::Source), r.p.add(paced));
    r.link(src.src(OUT), a.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(9));
    while r.rows(a).len() < 100 {
        r.run(&mut run, &mut rng, Some(1));
    }
    let position = Timestamp(r.rows(a).last().expect("played") * STEP.0);
    let mut late = r.node(&SINK, Role::Sink);
    late.paced = true;
    let b = r.p.add(late);
    r.link(src.src(OUT_B), b.sink(IN));
    r.p.commit().expect("commit");
    assert!(r.run(&mut run, &mut rng, None));
    run.finish().expect("finish");
    let got = r.rows(b);
    let first = *got.first().expect("the joined sink presented");
    assert!(
        Timestamp(first * STEP.0).saturating_add(STEP) > position,
        "first {first}, position {position:?}"
    );
    assert!(contiguous(&got) && got.last() == Some(&399), "{got:?}");
    assert_eq!(r.rows(a), (0..400).collect::<Vec<_>>());
}

#[test]
fn a_running_pipeline_is_edited_from_another_thread() {
    let mut r = Rig::new();
    let mut long = r.node(&SRC2, Role::Source);
    long.total = 300_000;
    let (src, a) = (r.p.add(long), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), a.sink(IN));
    let handle = r.p.control();
    let tape = Arc::clone(&r.tape);
    let editor = std::thread::spawn(move || {
        let node = |desc| Node {
            desc,
            role: Role::Sink,
            tape: Arc::clone(&tape),
            total: 0,
            next: 0,
            calls: 0,
            fail: Fail::Never,
            paced: false,
            high: 0,
            catching: None,
            doom: None,
            hold: None,
            told: 0,
            reasons: Reasons::default(),
        };
        let mut joined = Vec::new();
        for _ in 0..1_000_000 {
            if joined.len() == 60 {
                break;
            }
            let made = node(&SINK);
            let edit = handle.edit(move |p: &mut Pipeline| {
                let b = p.add(made);
                p.link(src.src(OUT_B), b.sink(IN))?;
                p.commit().map(|_| b)
            });
            let b = match edit {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => panic!("join refused: {e}"),
                Err(_) if joined.is_empty() => {
                    std::thread::yield_now();
                    continue;
                }
                Err(_) => break,
            };
            joined.push(b);
            loop {
                let left = handle.edit(move |p: &mut Pipeline| {
                    p.remove(b, Leave::Cut).and_then(|()| p.commit())
                });
                match left {
                    Ok(Err(Error::Topology(TopologyError::Busy))) => std::thread::yield_now(),
                    Ok(Err(e)) => panic!("leave refused: {e}"),
                    Ok(Ok(_)) | Err(_) => break,
                }
            }
        }
        joined
    });
    r.p.run().expect("run");
    let joined = editor.join().expect("editor");
    assert_eq!(joined.len(), 60, "the run outlasted the edits");
    assert!(contiguous(&r.rows(a)) && r.rows(a).len() == 300_000);
    for b in joined {
        assert_eq!((r.count(b, What::Start), r.count(b, What::Stop)), (1, 1), "element {}", b.0);
    }
}

fn taken(r: &Rig, el: ElementId) -> Vec<(u32, u64)> {
    let row = |w: What| match w {
        What::Row(stream, seq) => Some((stream, seq)),
        _ => None,
    };
    r.of(el).into_iter().filter_map(row).collect()
}

#[test]
fn a_second_link_on_a_pad_that_plays_shares_its_buffers_and_leaves_it_untouched() {
    // Not scaled. The seed is how many steps the first link plays alone.
    for seed in 0..60 {
        let mut r = Rig::new();
        let (src, a) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), a.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(50 + seed)));
        let b = r.add(&SINK, Role::Sink);
        let link = r.link(src.src(OUT), b.sink(IN));
        r.p.commit().expect("a pad that plays takes another link");
        assert!(!r.run(&mut run, &mut rng, Some(120)));
        let joined = taken(&r, b);
        assert!(joined.len() > 1, "seed {seed}: the second link is fed, {joined:?}");
        assert_eq!(joined[0].0, HEADER, "seed {seed}: the header comes first, {joined:?}");
        r.p.unlink(link).expect("unlink");
        r.p.remove(b, Leave::Drain).expect("remove");
        r.p.commit().expect("commit");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");

        assert_eq!(taken(&r, a), (0..400).map(|n| (0, n)).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(
            r.count(a, What::Flush) + r.count(a, What::SinkLinked(0, true)),
            0,
            "seed {seed}"
        );
        let shared = taken(&r, b);
        let (header, rest) = shared.split_first().expect("rows");
        assert_eq!(header.1, rest[0].1, "seed {seed}: the header names the buffer that follows it");
        let numbers: Vec<u64> = rest.iter().map(|(_, n)| *n).collect();
        assert!(
            rest.iter().all(|(stream, _)| *stream == 0) && contiguous(&numbers),
            "seed {seed}: {shared:?}"
        );
        assert!(
            numbers[0] > 0 && numbers.len() < 400,
            "seed {seed}: a span of the middle, {numbers:?}"
        );
        let told = r.of(src);
        assert_eq!(
            told.iter().filter(|w| matches!(w, What::SrcJoined(0))).count(),
            1,
            "seed {seed}"
        );
        assert!(!told.iter().any(|w| matches!(w, What::SrcLinked(..))), "seed {seed}: {told:?}");
        assert_eq!(
            (r.count(b, What::Eos), r.count(b, What::Stop)),
            (1, 1),
            "seed {seed}: it left drained"
        );
    }
}

#[test]
fn a_pad_linked_twice_before_the_run_feeds_both() {
    for seed in scaled(20) {
        let mut r = Rig::new();
        let (src, a, b) =
            (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), a.sink(IN));
        r.link(src.src(OUT), b.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        let report = run.finish().expect("finish");
        assert_eq!(r.rows(a), (0..400).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(r.rows(b), (0..400).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(report.buffers, 400, "seed {seed}: one slot a buffer, shared by both links");
    }
}

#[test]
fn a_shared_pad_is_told_it_lost_its_link_only_with_the_last_one() {
    for seed in scaled(40) {
        let mut r = Rig::new();
        let (src, a) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
        let first = r.link(src.src(OUT), a.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(40)));
        let b = r.add(&SINK, Role::Sink);
        let second = r.link(src.src(OUT), b.sink(IN));
        r.p.commit().expect("commit");
        assert!(!r.run(&mut run, &mut rng, Some(80)));
        r.p.unlink(first).expect("unlink");
        r.p.commit().expect("commit");
        assert!(!r.run(&mut run, &mut rng, Some(80)));
        assert!(
            !r.of(src).iter().any(|w| matches!(w, What::SrcLinked(0, false, _))),
            "seed {seed}"
        );
        let before = r.rows(b).len();
        assert!(!r.run(&mut run, &mut rng, Some(80)));
        assert!(r.rows(b).len() > before, "seed {seed}: the link that joined plays on");
        r.p.unlink(second).expect("unlink");
        r.p.commit().expect("commit");
        assert!(!r.run(&mut run, &mut rng, Some(60)));
        run.control().append(Command::Stop).expect("stop");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(r.count(src, What::SrcLinked(0, false, None)), 1, "seed {seed}");
        let (one, other) = (r.rows(a), r.rows(b));
        assert!(contiguous(&one) && one.first() == Some(&0), "seed {seed}: {one:?}");
        let numbers: Vec<u64> =
            taken(&r, b).iter().filter(|(s, _)| *s == 0).map(|(_, n)| *n).collect();
        assert!(contiguous(&numbers), "seed {seed}: {other:?}");
    }
}

#[test]
fn the_handles_taken_before_a_change_reach_what_it_added_and_not_what_left() {
    let mut r = Rig::new();
    let (src, a) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), a.sink(IN));
    let (props, tap) = (r.p.prop_handle(), r.p.tap_handle());
    let (mut run, mut rng) = (r.step(), Rng(7));
    assert!(!r.run(&mut run, &mut rng, Some(60)));

    let (src_b, b) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
    r.p.set_element_pool(src_b, 64, 8);
    r.link(src_b.src(OUT), b.sink(IN));
    let unknown = |el: ElementId| {
        matches!(
            props.set(el, "nothing", Value::Int(1)),
            Err(Error::Prop(PropError::UnknownElement { .. }))
        )
    };
    let known = |el: ElementId| {
        matches!(
            props.set(el, "nothing", Value::Int(1)),
            Err(Error::Prop(PropError::UnknownProperty { .. }))
        )
    };
    assert!(
        known(b) && known(src_b),
        "an element is the handle's as soon as it is in the pipeline"
    );
    assert!(unknown(ElementId(40)));
    r.p.commit().expect("commit");
    assert!(!r.run(&mut run, &mut rng, Some(200)));
    let fed = tap.snapshot(b).expect("the tap reads the sink that joined").buffers_in;
    assert!(fed > 0 && fed == r.rows(b).len() as u64, "{fed} against {:?}", r.rows(b));
    assert_eq!(tap.element_name(b), Some("tsink"));
    let made = tap.snapshot(src_b).expect("and its source").buffers_out;
    assert_eq!(tap.pool().max_slots, 2 * 24 + 8 + 24, "the pools that joined count as headroom");

    for el in [src_b, b] {
        r.p.remove(el, Leave::Cut).expect("remove");
    }
    r.p.commit().expect("commit");
    assert!(!r.run(&mut run, &mut rng, Some(60)));
    r.p.settle();
    assert!(
        tap.snapshot(b).is_none() && tap.snapshot(src_b).is_none(),
        "what left is not reported"
    );
    assert!(unknown(b) && unknown(src_b) && known(a), "nor reached by a property");
    assert_eq!(tap.len(), 4, "ids are not given out twice");
    assert_eq!(tap.pool().max_slots, 2 * 24, "a pool that left is no headroom");
    assert!(r.run(&mut run, &mut rng, None));
    let report = run.finish().expect("finish");
    let own = tap.snapshot(src_b);
    assert!(own.is_none());
    assert!(
        made > 0 && report.buffers >= 400 + made,
        "{} buffers, {made} of the pool that left",
        report.buffers
    );
}

#[test]
fn an_element_that_joins_logs_through_the_gate_of_the_run() {
    for (debug, logs) in [("", false), ("tsink:info", true), ("info", true)] {
        let mut r = Rig::logging(debug);
        let (src, a) = (r.add(&SRC2, Role::Source), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), a.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(3));
        assert!(!r.run(&mut run, &mut rng, Some(40)));
        let b = r.add(&SINK, Role::Sink);
        r.link(src.src(OUT_B), b.sink(IN));
        r.p.commit().expect("commit");
        assert!(r.run(&mut run, &mut rng, None));
        run.finish().expect("finish");
        assert_eq!(r.count(a, What::Logs(logs)), 1, "{debug:?}: the sink of the run");
        assert_eq!(r.count(b, What::Logs(logs)), 1, "{debug:?}: the sink that joined");
    }
    let mut r = Rig::logging("tsink:info");
    let mut long = r.node(&SRC2, Role::Source);
    long.total = 200_000;
    let (src, a) = (r.p.add(long), r.add(&FWD, Role::Forward));
    r.link(src.src(OUT), a.sink(IN));
    let (handle, stop) = (r.p.control(), r.p.control());
    let made = r.node(&SINK, Role::Sink);
    let editor = std::thread::spawn(move || {
        let mut made = Some(made);
        loop {
            let node = made.take().expect("one join");
            let joined = handle.edit(move |p: &mut Pipeline| {
                let b = p.add(node);
                p.link(src.src(OUT_B), b.sink(IN)).and_then(|_| p.commit()).map(|_| b)
            });
            match joined {
                Ok(joined) => return joined.expect("the join"),
                Err(_) => std::thread::yield_now(),
            }
            made = Some(Node {
                desc: &SINK,
                role: Role::Sink,
                tape: Tape::default(),
                total: 0,
                next: 0,
                calls: 0,
                fail: Fail::Never,
                paced: false,
                high: 0,
                catching: None,
                doom: None,
                hold: None,
                told: 0,
                reasons: Reasons::default(),
            });
        }
    });
    let run = std::thread::spawn(move || {
        r.p.run().expect("run");
        r
    });
    let b = editor.join().expect("editor");
    stop.stop();
    let r = run.join().expect("the run thread");
    let _ = b;
    assert_eq!(r.count(a, What::Logs(false)), 1, "the forwarder is not named by the rule");
}

#[test]
fn the_logs_a_change_loses_are_told_on_the_bus() {
    let mut r = Rig::logging("info");
    let (src, a) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), a.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(11));
    assert!(!r.run(&mut run, &mut rng, Some(40)));
    let lost = |messages: &[BusMessage]| -> Vec<u32> {
        let count = |m: &BusMessage| match m {
            BusMessage::Warning {
                error: Error::Topology(TopologyError::LogsLost { count }),
                ..
            } => Some(*count),
            _ => None,
        };
        messages.iter().filter_map(count).collect()
    };
    let mut joined = Vec::new();
    for _ in 0..35 {
        let (src, sink) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), sink.sink(IN));
        joined.extend([src, sink]);
    }
    r.p.commit().expect("commit");
    assert_eq!(lost(&bus(&r.p)), [6], "70 joined and 64 drains wait");
    assert!(r.run(&mut run, &mut rng, None));
    run.finish().expect("finish");
    let logging =
        |logs: bool| joined.iter().filter(|el| r.count(**el, What::Logs(logs)) == 1).count();
    assert_eq!(
        (logging(true), logging(false)),
        (64, 6),
        "each started, the last six with no channel"
    );
    let mut r = Rig::logging("info");
    let (src, a) = (r.add(&SRC2, Role::Source), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), a.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(11));
    assert!(!r.run(&mut run, &mut rng, Some(40)));
    let b = r.add(&SINK, Role::Sink);
    r.link(src.src(OUT_B), b.sink(IN));
    r.p.commit().expect("commit");
    assert!(lost(&bus(&r.p)).is_empty());
    assert!(r.run(&mut run, &mut rng, None));
    run.finish().expect("finish");
}

struct Watch {
    made: u64,
    last: std::time::Instant,
}

impl Element for Watch {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.made == 5 {
            return Ok(Flow::Eos);
        }
        if self.last.elapsed() >= std::time::Duration::from_millis(30)
            && let Some(mut buf) = ctx.try_alloc()
        {
            buf.memory.as_mut_full()[..12].fill(0);
            buf.memory.set_len(12);
            ctx.out(OUT).push(buf);
            (self.made, self.last) = (self.made + 1, std::time::Instant::now());
        }
        Ok(Flow::Ok)
    }
}

#[test]
fn work_nobody_signaled_is_a_late_wake() {
    let mut r = Rig::new();
    let src = r.p.add(Watch { made: 0, last: std::time::Instant::now() });
    let sink = r.add(&SINK, Role::Sink);
    r.link(src.src(OUT), sink.sink(IN));
    let tap = r.p.tap_handle();
    r.p.run().expect("run");
    let stats = tap.scheduler();
    assert!(
        stats.late_wakes >= 4,
        "five buffers a watch made, each found at a backstop, {stats:?}"
    );
}

fn waiting(seed: u64) -> (Rig, Stepped, Rng, [ElementId; 4], u32) {
    let mut r = Rig::new();
    let (src, merge, sink) =
        (r.add(&SRC, Role::Source), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), merge.sink(IN));
    r.link(merge.src(OUT), sink.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(seed));
    assert!(!r.run(&mut run, &mut rng, Some(50)));
    let mute = r.add(&SRC, Role::Mute);
    r.link(mute.src(OUT), merge.sink(IN_B));
    let change = r.p.commit().expect("commit");
    assert!(!r.run(&mut run, &mut rng, Some(80)));
    assert!(r.p.changing(), "seed {seed}: the change waits for a batch that does not come");
    (r, run, rng, [src, merge, sink, mute], change)
}

fn cancelled(messages: &[BusMessage], change: u32) -> usize {
    let is = |m: &&BusMessage| matches!(m, BusMessage::ChangeFailed { change: c, error: Error::Topology(TopologyError::Cancelled { change: e }) } if *c == change && *e == change);
    messages.iter().filter(is).count()
}

#[test]
fn a_change_that_waits_is_cancelled_and_the_next_one_is_taken() {
    for seed in scaled(40) {
        let (mut r, mut run, mut rng, [src, merge, sink, mute], change) = waiting(seed);
        let b = r.add(&SINK, Role::Sink);
        r.link(src.src(OUT), b.sink(IN));
        assert_eq!(topology(r.p.commit()), TopologyError::Busy, "seed {seed}");

        assert_eq!(r.p.cancel(), Some(change), "seed {seed}");
        assert_eq!(r.p.cancel(), None, "seed {seed}: nothing waits any more");
        assert!(!r.p.changing(), "seed {seed}");
        assert_eq!(cancelled(&bus(&r.p), change), 1, "seed {seed}");
        assert!(
            matches!(r.p.remove(mute, Leave::Cut), Err(Error::Link(_))),
            "seed {seed}: the books forgot it"
        );

        let b = r.add(&SINK, Role::Sink);
        r.link(src.src(OUT), b.sink(IN));
        let next = r.p.commit().expect("the commit behind a cancel is taken");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert!(
            bus(&r.p).iter().any(|m| matches!(m, BusMessage::Changed { change: c } if *c == next)),
            "seed {seed}"
        );
        r.p.settle();
        assert_eq!(r.p.cancel(), None, "seed {seed}: a change that has spliced is not cancelled");

        assert_eq!(
            (r.count(mute, What::Start), r.count(mute, What::Stop), r.count(mute, What::Eos)),
            (1, 1, 0),
            "seed {seed}"
        );
        assert!(
            !r.of(merge).iter().any(|w| matches!(w, What::SinkLinked(1, _))),
            "seed {seed}: the aggregator never heard of it"
        );
        assert_eq!(r.rows(sink), (0..400).collect::<Vec<_>>(), "seed {seed}: what plays is whole");
        let shared: Vec<u64> =
            taken(&r, b).iter().filter(|(stream, _)| *stream == 0).map(|(_, n)| *n).collect();
        assert!(contiguous(&shared) && shared.last() == Some(&399), "seed {seed}: {shared:?}");
    }
}

#[test]
fn the_end_of_the_run_takes_back_a_change_that_still_waits() {
    for seed in scaled(40) {
        let (mut r, mut run, mut rng, [_, merge, sink, mute], change) = waiting(seed);
        assert!(
            r.run(&mut run, &mut rng, None),
            "seed {seed}: the run ends with the change unspliced"
        );
        run.finish().expect("finish");
        assert_eq!(cancelled(&bus(&r.p), change), 1, "seed {seed}");
        r.p.settle();
        assert_eq!(r.p.failed_change(), Some(change), "seed {seed}");
        assert_eq!(
            (r.count(mute, What::Start), r.count(mute, What::Stop), r.count(mute, What::Eos)),
            (1, 1, 0),
            "seed {seed}"
        );
        assert!(!r.of(merge).iter().any(|w| matches!(w, What::SinkLinked(1, _))), "seed {seed}");
        assert_eq!(r.rows(sink), (0..400).collect::<Vec<_>>(), "seed {seed}");
        assert_eq!(r.count(sink, What::Eos), 1, "seed {seed}: the run ended at its streams' end");
    }
}

#[test]
fn a_stop_takes_back_a_change_that_still_waits() {
    for seed in scaled(20) {
        let (mut r, mut run, mut rng, [.., mute], change) = waiting(seed);
        run.control().append(Command::Stop).expect("stop");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(cancelled(&bus(&r.p), change), 1, "seed {seed}");
        r.p.settle();
        assert!(
            matches!(r.p.remove(mute, Leave::Cut), Err(Error::Link(_))),
            "seed {seed}: the books forgot it"
        );
        assert_eq!((r.count(mute, What::Start), r.count(mute, What::Stop)), (1, 1), "seed {seed}");
    }
}

#[test]
fn a_change_that_waits_is_cancelled_on_threads_and_the_run_ends_past_another() {
    let mut r = Rig::new();
    let mut long = r.node(&SRC, Role::Source);
    long.total = 150_000;
    let (src, merge, sink) = (r.p.add(long), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), merge.sink(IN));
    r.link(merge.src(OUT), sink.sink(IN));
    let handle = r.p.control();
    let mutes = [r.node(&SRC, Role::Mute), r.node(&SRC, Role::Mute)];
    let shared = r.node(&SINK, Role::Sink);
    let editor = std::thread::spawn(move || {
        let [first, second] = mutes;
        let join = |node: Node| {
            while !matches!(handle.edit(|p: &mut Pipeline| p.changing()), Ok(false)) {
                std::thread::yield_now();
            }
            let edit = handle.edit(move |p: &mut Pipeline| {
                let mute = p.add(node);
                p.link(mute.src(OUT), merge.sink(IN_B))?;
                p.commit().map(|change| (mute, change))
            });
            edit.expect("the run is up").expect("the join is taken")
        };
        let (mute, change) = join(first);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let busy = handle.edit(move |p: &mut Pipeline| p.commit()).expect("the run is up");
        assert_eq!(topology(busy), TopologyError::Busy);
        let done = handle.edit(|p: &mut Pipeline| p.cancel()).expect("the run is up");
        assert_eq!(done, Some(change));
        let b = handle
            .edit(move |p: &mut Pipeline| {
                let b = p.add(shared);
                p.link(src.src(OUT), b.sink(IN)).and_then(|_| p.commit()).map(|_| b)
            })
            .expect("the run is up")
            .expect("the commit behind a cancel is taken");
        let last = join(second);
        (mute, change, b, last)
    });
    let (ended, said) = std::sync::mpsc::channel();
    let run = std::thread::spawn(move || {
        r.p.run().expect("run");
        let _ = ended.send(());
        r
    });
    let (mute, change, b, (_, last)) = editor.join().expect("editor");
    said.recv_timeout(std::time::Duration::from_secs(60))
        .expect("the run ends past a change that waits");
    let r = run.join().expect("the run thread");
    let messages = bus(&r.p);
    assert_eq!(
        (cancelled(&messages, change), cancelled(&messages, last)),
        (1, 1),
        "both are said to have failed"
    );
    assert_eq!(
        (r.count(mute, What::Start), r.count(mute, What::Stop), r.count(mute, What::Eos)),
        (1, 1, 0)
    );
    assert_eq!(r.rows(sink).len(), 150_000);
    let shared: Vec<u64> =
        taken(&r, b).iter().filter(|(stream, _)| *stream == 0).map(|(_, n)| *n).collect();
    assert!(contiguous(&shared) && shared.last() == Some(&149_999));
}

#[derive(Default)]
struct Kept {
    records: Mutex<Vec<(Level, &'static str, &'static str)>>,
    sessions: Mutex<Vec<u64>>,
    lines: Mutex<Vec<(Level, String)>>,
}

impl LogWriter for Kept {
    fn record(&self, rec: &LogRecord) {
        self.records.lock().unwrap().push((rec.level, rec.name, rec.event));
        self.sessions.lock().unwrap().push(rec.session);
    }

    fn line(&self, level: Level, msg: std::fmt::Arguments<'_>) {
        self.lines.lock().unwrap().push((level, msg.to_string()));
    }
}

impl Kept {
    fn started(&self) -> Vec<&'static str> {
        let records = self.records.lock().unwrap();
        let mut names: Vec<_> = records
            .iter()
            .filter(|r| r.0 == Level::Info && r.2 == "started")
            .map(|r| r.1)
            .collect();
        names.sort();
        names
    }

    fn says(&self, level: Level, word: &str) -> bool {
        self.lines.lock().unwrap().iter().any(|(l, line)| *l == level && line.contains(word))
    }
}

#[test]
fn a_stepped_run_hands_its_records_and_lines_to_the_writer() {
    let kept = Arc::new(Kept::default());
    let mut r = Rig::logging("");
    r.p.set_log_writer(Arc::clone(&kept) as Arc<dyn LogWriter>);
    r.p.set_log_session(42);
    r.p.set_debug(DebugSpec::parse("info"));
    let (src, a) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), a.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(5));
    assert!(r.run(&mut run, &mut rng, None));
    run.finish().expect("finish");
    assert_eq!(kept.started(), ["tsink", "tsrc"]);
    let sessions = kept.sessions.lock().unwrap();
    assert!(!sessions.is_empty() && sessions.iter().all(|s| *s == 42), "every record is stamped");
    drop(sessions);
    for word in ["add tsrc#0", "link tsrc#0.src -> tsink#1.sink", "clock <-", "scheduler parks="] {
        assert!(kept.says(Level::Info, word), "{word:?} in {:?}", kept.lines.lock().unwrap());
    }
    assert!(!kept.says(Level::Debug, "negotiate"), "debug lines stay below info");
}

#[test]
fn a_threaded_run_hands_its_records_to_the_writer() {
    let kept = Arc::new(Kept::default());
    let mut r = Rig::logging("tsink:info");
    r.p.set_log_writer(Arc::clone(&kept) as Arc<dyn LogWriter>);
    let (src, a) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), a.sink(IN));
    r.p.run().expect("run");
    assert_eq!(kept.started(), ["tsink"], "only the element the rule names");
    assert!(kept.lines.lock().unwrap().is_empty(), "no pipeline level, no lines");
    assert_eq!(r.count(a, What::Logs(true)), 1);
}

#[test]
fn set_debug_raises_and_silences_a_pipeline_made_from_explicit_configuration() {
    for (spec, logs) in [("info", true), ("off", false)] {
        let kept = Arc::new(Kept::default());
        let mut r = Rig::logging("info");
        r.p.set_log_writer(Arc::clone(&kept) as Arc<dyn LogWriter>);
        r.p.set_debug(DebugSpec::parse(spec));
        let (src, a) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), a.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(7));
        assert!(r.run(&mut run, &mut rng, None));
        run.finish().expect("finish");
        assert_eq!(!kept.started().is_empty(), logs, "{spec}");
        assert_eq!(kept.says(Level::Info, "add tsrc#0"), logs, "{spec}");
    }
}

/// Run in a child by the test below. Prints how many info lines a scoped `Pipeline::new` wrote.
#[test]
#[ignore = "run as a child of the_environment_wins_over_the_scoped_levels"]
fn scoped_pipeline_child() {
    let kept = Arc::new(Kept::default());
    let logs = Logs {
        writer: Some(Arc::clone(&kept) as Arc<dyn LogWriter>),
        debug: Some(DebugSpec::parse("info")),
    };
    let mut p = with_logs(&logs, Pipeline::new);
    p.set_debug(DebugSpec::parse("info"));
    let r = Rig::new();
    let (src, sink) = (p.add(r.node(&SRC, Role::Source)), p.add(r.node(&SINK, Role::Sink)));
    p.link(src.src(OUT), sink.sink(IN)).expect("link");
    let info = kept.lines.lock().unwrap().iter().filter(|(l, _)| *l == Level::Info).count();
    println!("info_lines={info}");
}

#[test]
fn the_environment_wins_over_the_scoped_levels() {
    let child = "topology::scoped_pipeline_child";
    let lines = |debug: Option<&str>| {
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("this binary"));
        cmd.args(["--exact", child, "--ignored", "--nocapture"]);
        match debug {
            Some(d) => cmd.env("PROFLUENS_DEBUG", d),
            None => cmd.env_remove("PROFLUENS_DEBUG"),
        };
        let out = cmd.output().expect("the child ran");
        let (stdout, stderr) =
            (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && stdout.contains("1 passed"), "{stdout}\n{stderr}");
        assert!(!stderr.contains("pipeline"), "a scoped pipeline writes no stderr\n{stderr}");
        let n = stdout
            .split("info_lines=")
            .nth(1)
            .and_then(|t| t.split_whitespace().next()?.parse::<u32>().ok());
        n.expect("the child counted")
    };
    assert_eq!(lines(None), 3, "two adds and a link at the scope's info");
    assert_eq!(lines(Some("warn")), 0, "the environment's warn wins over the scope and set_debug");
    assert_eq!(lines(Some("debug")), 3);
}

#[test]
fn with_logs_nests_and_restores() {
    let outer = Logs { writer: None, debug: Some(DebugSpec::parse("warn")) };
    let inner = Logs { writer: None, debug: Some(DebugSpec::parse("trace")) };
    assert!(profluens_core::log::current_logs().debug.is_none());
    with_logs(&outer, || {
        with_logs(&inner, || assert_eq!(profluens_core::log::current_logs().debug, inner.debug));
        assert_eq!(profluens_core::log::current_logs().debug, outer.debug);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_logs(&inner, || panic!("unwinds"))
        }));
        assert!(caught.is_err());
        assert_eq!(profluens_core::log::current_logs().debug, outer.debug, "restored on unwind");
    });
    assert!(profluens_core::log::current_logs().debug.is_none());
}

fn scaled(n: u64) -> std::ops::Range<u64> {
    let scale = std::env::var("PF_SEED_SCALE").ok().and_then(|s| s.parse::<u64>().ok());
    0..n * scale.unwrap_or(1).max(1)
}

#[test]
fn a_drained_branch_hands_its_sink_over_to_the_one_that_replaces_it() {
    for seed in scaled(60) {
        let mut r = Rig::new();
        let src = r.add(&SRC, Role::Source);
        let (old, sink) = (r.add(&FWD, Role::Forward), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), old.sink(IN));
        r.link(old.src(OUT), sink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        let passes = 40 + rng.below(80);
        assert!(!r.run(&mut run, &mut rng, Some(passes)));
        r.p.remove(old, Leave::Drain).expect("remove");
        let new = r.add(&FWD, Role::Forward);
        r.link(src.src(OUT), new.sink(IN));
        r.link(new.src(OUT), sink.sink(IN));
        r.p.commit().expect("commit");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(
            r.rows(sink),
            (0..400).collect::<Vec<_>>(),
            "seed {seed}: every buffer once, in order"
        );
        assert_eq!(
            r.count(sink, What::SinkLinked(0, true)) + r.count(sink, What::Flush),
            0,
            "seed {seed}"
        );
        assert_eq!(
            (r.count(old, What::Eos), r.count(old, What::Stop)),
            (1, 1),
            "seed {seed}: the old one drained"
        );
        assert_eq!(
            r.count(sink, What::Eos),
            1,
            "seed {seed}: the end of the stream is the new one's"
        );
    }
}

#[test]
fn a_seek_during_a_hand_over_plays_the_new_branch_from_the_point() {
    for seed in scaled(40) {
        let mut r = Rig::new();
        let src = r.add(&SRC, Role::Source);
        let (old, sink) = (r.add(&FWD, Role::Forward), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), old.sink(IN));
        r.link(old.src(OUT), sink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(60)));
        r.p.remove(old, Leave::Drain).expect("remove");
        let new = r.add(&FWD, Role::Forward);
        r.link(src.src(OUT), new.sink(IN));
        r.link(new.src(OUT), sink.sink(IN));
        r.p.commit().expect("commit");
        let passes = rng.below(6);
        let _ = r.run(&mut run, &mut rng, Some(passes));
        let to_time = Timestamp(300 * STEP.0);
        run.control().seek_bytes(to_time, &[(src, 300, to_time)]).expect("seek");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        let rows = r.rows(sink);
        let at = rows
            .iter()
            .rposition(|seq| *seq == 300)
            .unwrap_or_else(|| panic!("seed {seed}: the point is played, {rows:?}"));
        assert_eq!(
            rows[at..],
            (300..400).collect::<Vec<_>>()[..],
            "seed {seed}: from the point on, once"
        );
    }
}

#[test]
fn a_seek_after_a_source_swap_restarts_the_new_source_at_the_target() {
    for seed in scaled(20) {
        let mut r = Rig::new();
        let (old, sink) = (r.add(&SRC, Role::Source), r.add(&SINK, Role::Sink));
        r.link(old.src(OUT), sink.sink(IN));
        let (mut run, mut rng) = (r.step(), Rng(seed));
        assert!(!r.run(&mut run, &mut rng, Some(60)));
        r.p.remove(old, Leave::Cut).expect("remove");
        let new = r.add(&SRC, Role::Source);
        r.link(new.src(OUT), sink.sink(IN));
        r.p.commit().expect("commit");
        while r.p.changing() {
            assert!(!r.run(&mut run, &mut rng, Some(1)), "seed {seed}: the change splices");
        }
        let passes = rng.below(30);
        let _ = r.run(&mut run, &mut rng, Some(passes));
        let to_time = Timestamp(300 * STEP.0);
        run.control().seek_bytes(to_time, &[(ALL, 300, to_time)]).expect("seek");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        let rows = r.rows(sink);
        let at = rows.iter().position(|seq| *seq == 300).expect("the point is played");
        assert_eq!(rows[at..], (300..400).collect::<Vec<_>>()[..], "seed {seed}: from the point");
        let dropped = r.of(sink).iter().filter(|w| matches!(w, What::Dropped(..))).count();
        assert_eq!(dropped, 0, "seed {seed}: the new source restarts at the target, not at 0");
    }
}

#[test]
fn a_splice_applied_under_a_fan_in_batch_feeds_the_links_that_remain() {
    let mut r = Rig::new();
    let (a, b) = (r.add(&SRC, Role::Source), r.add(&SRC2, Role::Source));
    let (merge, sink) = (r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
    r.link(a.src(OUT), merge.sink(IN));
    r.link(b.src(OUT_B), merge.sink(IN_B));
    r.link(merge.src(OUT), sink.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(5));
    assert!(!r.run(&mut run, &mut rng, Some(60)));
    let group = |run: &Stepped, el: ElementId| {
        (0..run.groups()).find(|g| run.elements(*g).contains(&el)).expect("a group")
    };
    let (gb, gm) = (group(&run, b), group(&run, merge));
    // The merge has applied every command, then a seek and a cut land behind its back.
    assert_eq!(run.pass_to(gm, Cut::Drained).expect("the top"), None);
    let to_time = Timestamp(300 * STEP.0);
    run.control().seek_bytes(to_time, &[(ALL, 300, to_time)]).expect("seek");
    r.p.remove(a, Leave::Cut).expect("remove");
    r.p.commit().expect("commit");
    run.service();
    for _ in 0..4 {
        run.pass(gb).expect("b applies the seek and pushes");
    }
    // The rest of the merge's pass ingests b's batch, which applies the seek and the cut.
    run.pass(gm).expect("the merge survives its links changing under a batch");
    assert!(r.run(&mut run, &mut rng, None), "the run ends");
    run.finish().expect("finish");
    let rows: Vec<u64> = r
        .of(merge)
        .into_iter()
        .filter_map(|w| match w {
            What::Row(1, seq) => Some(seq),
            _ => None,
        })
        .collect();
    let at = rows.iter().position(|seq| *seq == 300).expect("the point is played");
    assert_eq!(rows[at..], (300..400).collect::<Vec<_>>()[..], "b plays on from the point");
    assert_eq!(r.count(merge, What::SinkLinked(0, false)), 1, "a's pad was cut");
}

#[test]
fn a_splice_held_by_a_full_log_posts_once_a_group_catches_up() {
    let mut r = Rig::new();
    let hold = Arc::new(AtomicU8::new(0));
    let mut src = r.node(&SRC2, Role::Source);
    (src.total, src.hold) = (2_000, Some(Arc::clone(&hold)));
    let (src, sink) = (r.p.add(src), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), sink.sink(IN));
    let handle = r.p.control();
    let tape = Arc::clone(&r.tape);
    let editor = std::thread::spawn(move || {
        while hold.load(Ordering::Acquire) != 1 {
            std::thread::yield_now();
        }
        let b = Node { total: 0, ..Node::sink(&SINK, tape) };
        let joined = handle.edit(move |p: &mut Pipeline| {
            let b = p.add(b);
            p.link(src.src(OUT_B), b.sink(IN))?;
            p.commit().map(|_| b)
        });
        let b = joined.expect("edited").expect("joined");
        // The attach posted, and every slot left fills while the source sits in `process`.
        let mut sent = 0;
        while handle.append(Command::SetOffset { element: sink, by: sent }).is_ok() {
            sent += 1;
        }
        assert!(sent > 0, "the log fills");
        hold.store(2, Ordering::Release);
        b
    });
    let run = std::thread::spawn(move || {
        r.p.run().expect("run");
        r
    });
    let started = std::time::Instant::now();
    while !run.is_finished() {
        assert!(started.elapsed() < std::time::Duration::from_secs(30), "the run hangs");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let (r, b) = (run.join().expect("run"), editor.join().expect("editor"));
    assert_eq!((r.count(b, What::Start), r.count(b, What::Stop)), (1, 1));
    assert!(!r.rows(b).is_empty(), "the joined sink played");
}

fn quiesce(run: &mut Stepped) {
    for _ in 0..10_000 {
        run.service();
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            return;
        }
        for g in ready {
            run.pass(g).expect("pass");
        }
    }
    panic!("the run never settled");
}

#[test]
fn a_producer_that_leaves_hands_on_the_batch_it_staged_for_a_cut_link() {
    let rows = |second: bool| {
        let mut r = Rig::new();
        let src = r.add(&SRC, Role::Source);
        let fwd = r.add(&FWD, Role::Forward);
        let sink = r.p.add(Node { paced: true, ..r.node(&SINK, Role::Sink) });
        r.p.set_queue_capacity(sink, 1);
        r.link(src.src(OUT), fwd.sink(IN));
        r.link(fwd.src(OUT), sink.sink(IN));
        let mut run = r.step();
        quiesce(&mut run);
        let before = r.rows(sink).len();
        r.p.remove(sink, Leave::Drain).expect("remove");
        r.p.commit().expect("commit");
        quiesce(&mut run);
        if second {
            r.p.remove(fwd, Leave::Drain).expect("remove");
            r.p.commit().expect("commit");
            quiesce(&mut run);
        }
        assert!(r.run(&mut run, &mut Rng(0), None), "second {second}: the run ends");
        run.finish().expect("finish");
        (before, r.rows(sink))
    };
    let (before, kept) = rows(false);
    assert!(
        contiguous(&kept) && kept.len() > before + 1,
        "the ring and the staged batch, {before} then {kept:?}"
    );
    assert_eq!(rows(true).1, kept, "the producer that left handed on what it staged");
}

#[test]
fn a_branch_that_drains_presents_from_a_skip() {
    let mut r = Rig::new();
    let src = r.add(&SRC2, Role::Source);
    let fwd = r.add(&FWD, Role::Forward);
    let sink = r.p.add(Node { paced: true, ..r.node(&SINK, Role::Sink) });
    let stays = r.p.add(Node { paced: true, ..r.node(&SINK, Role::Sink) });
    r.link(src.src(OUT), fwd.sink(IN));
    r.link(fwd.src(OUT), sink.sink(IN));
    r.link(src.src(OUT_B), stays.sink(IN));
    let mut run = r.step();
    quiesce(&mut run);
    let before = r.rows(sink).len() as u64;
    for el in [fwd, sink] {
        r.p.remove(el, Leave::Drain).expect("remove");
    }
    r.p.commit().expect("commit");
    quiesce(&mut run);
    assert_eq!(r.count(sink, What::Stop), 0, "it holds what is not due");
    let to = before + 3;
    run.control().append(Command::SkipTo { to_time: Timestamp(to * STEP.0) }).expect("skip");
    assert!(r.run(&mut run, &mut Rng(0), None), "the run ends, {:?}", outcomes(&run));
    run.finish().expect("finish");
    let dropped: Vec<u64> = r
        .of(sink)
        .iter()
        .filter_map(|w| if let What::Dropped(_, seq) = w { Some(*seq) } else { None })
        .collect();
    let rows = r.rows(sink);
    assert_eq!(
        dropped,
        (before..to).collect::<Vec<_>>(),
        "what it held before the point, {rows:?}"
    );
    let after = &rows[before as usize..];
    assert!(after.len() > 1 && after[0] == to && contiguous(after), "from the point on, {rows:?}");
    assert_eq!(
        (r.count(sink, What::Flush), r.count(sink, What::Eos), r.count(sink, What::Stop)),
        (0, 1, 1)
    );
}

fn outcomes(run: &Stepped) -> Vec<(u32, Option<Outcome>)> {
    (0..run.groups())
        .map(|g| (run.elements(g).first().map_or(u32::MAX, |e| e.0), run.outcome(g)))
        .collect()
}

fn group_of(run: &Stepped, el: ElementId) -> usize {
    (0..run.groups()).find(|g| run.elements(*g).contains(&el)).expect("a group runs it")
}

fn sealed(messages: &[BusMessage]) -> usize {
    messages.iter().filter(|m| matches!(m, BusMessage::BranchSealed { .. })).count()
}

fn keeper(r: &mut Rig) -> ElementId {
    let src = r.add(&SRC, Role::Source);
    let sink = r.p.add(Node { paced: true, ..r.node(&SINK, Role::Sink) });
    r.link(src.src(OUT), sink.sink(IN));
    sink
}

#[test]
fn a_branch_that_drains_allocates_from_its_pools_until_it_is_out() {
    let mut r = Rig::new();
    let src = r.add(&SRC2, Role::Source);
    let (copier, sink, a) =
        (r.add(&FWD, Role::Copy), r.add(&SINK, Role::Sink), r.add(&SINK, Role::Sink));
    r.p.set_element_pool(src, 64, 80);
    r.p.set_queue_capacity(copier, 32);
    r.p.set_queue_capacity(a, 32);
    r.link(src.src(OUT), copier.sink(IN));
    r.link(copier.src(OUT), sink.sink(IN));
    r.link(src.src(OUT_B), a.sink(IN));
    let tap = r.p.tap_handle();
    let mut run = r.step();
    let gs = group_of(&run, src);
    for _ in 0..80 {
        run.pass(gs).expect("pass");
    }
    let made = tap.snapshot(src).expect("the source").buffers_out / 2;
    assert_eq!(made, 33, "the ring and the batch staged");
    for el in [copier, sink] {
        r.p.remove(el, Leave::Drain).expect("remove");
    }
    r.p.commit().expect("commit");
    assert!(!r.p.changing(), "spliced at once");
    assert!(r.run(&mut run, &mut Rng(3), None), "the run ends, {:?}", outcomes(&run));
    assert_eq!(r.rows(sink), (0..made).collect::<Vec<_>>(), "what reached the ring is copied out");
    assert_eq!(r.rows(a).len(), 400);
    r.p.changing();
    assert!(tap.pool_of(copier).is_none() && tap.pool_of(sink).is_none(), "out, its buffers back");
    run.finish().expect("finish");
}

#[test]
fn a_link_out_of_or_into_a_sealed_group_is_refused() {
    for into in [false, true] {
        let mut r = Rig::new();
        keeper(&mut r);
        let doom = Some(Arc::new(AtomicBool::new(true)));
        let bad = match into {
            false => {
                let bad = r.p.add(Node { doom, ..r.node(&SRC2, Role::Source) });
                let a = r.add(&SINK, Role::Sink);
                r.link(bad.src(OUT), a.sink(IN));
                bad
            }
            true => {
                let (src, a) = (r.add(&SRC2, Role::Source), r.add(&SINK, Role::Sink));
                let bad = r.p.add(Node { doom, ..r.node(&SINK2, Role::Sink) });
                r.link(src.src(OUT), bad.sink(IN));
                r.link(src.src(OUT_B), a.sink(IN));
                bad
            }
        };
        let mut run = r.step();
        quiesce(&mut run);
        assert_eq!(sealed(&bus(&r.p)), 1, "into {into}");
        let (fresh, link) = match into {
            false => {
                let b = r.add(&SINK, Role::Sink);
                (b, r.link(bad.src(OUT_B), b.sink(IN)))
            }
            true => {
                let b = r.add(&SRC, Role::Source);
                (b, r.link(b.src(OUT), bad.sink(IN_B)))
            }
        };
        assert_eq!(topology(r.p.commit()), TopologyError::Gone { element: bad }, "into {into}");
        assert!(r.p.negotiated(link).is_none(), "into {into}: the books are as they were");
        assert!(r.p.element_desc(fresh).is_none(), "into {into}");
        assert!(!r.p.changing(), "into {into}");
        assert!(
            r.run(&mut run, &mut Rng(1), None),
            "into {into}: the run ends, {:?}",
            outcomes(&run)
        );
        run.finish().expect("finish");
    }
}

#[test]
fn a_link_into_a_group_that_seals_before_its_splice_is_cut_by_its_producer() {
    let mut r = Rig::new();
    keeper(&mut r);
    let (src, a) = (r.add(&SRC2, Role::Source), r.add(&SINK, Role::Sink));
    let doom = Arc::new(AtomicBool::new(false));
    let bad =
        r.p.add(Node { doom: Some(Arc::clone(&doom)), paced: true, ..r.node(&SINK2, Role::Sink) });
    r.link(src.src(OUT), bad.sink(IN));
    r.link(src.src(OUT_B), a.sink(IN));
    let mut run = r.step();
    quiesce(&mut run);
    let (b, c) = (r.add(&SRC2, Role::Source), r.add(&SINK, Role::Sink));
    r.link(b.src(OUT), bad.sink(IN_B));
    r.link(b.src(OUT_B), c.sink(IN));
    r.p.commit().expect("commit");
    doom.store(true, Ordering::Release);
    let g = group_of(&run, bad);
    assert_eq!(run.pass(g).expect("sealed"), Outcome::Exit);
    assert_eq!(sealed(&bus(&r.p)), 1);
    quiesce(&mut run);
    assert!(r.of(b).contains(&What::SrcLinked(0, false, None)), "{:?}", r.of(b));
    let gb = group_of(&run, b);
    assert!(!matches!(run.outcome(gb), Some(Outcome::Blocked { .. })), "{:?}", run.outcome(gb));
    assert!(r.run(&mut run, &mut Rng(2), None), "the run ends, {:?}", outcomes(&run));
    assert_eq!(r.rows(c).len(), 400);
    run.finish().expect("finish");
}

#[test]
fn a_producer_that_ended_cuts_a_sealed_link_and_leaves_past_a_ring_nobody_reads() {
    for seal in [true, false] {
        let mut r = Rig::new();
        keeper(&mut r);
        let src = r.p.add(Node { total: 9, ..r.node(&SRC, Role::Source) });
        let doom = Arc::new(AtomicBool::new(false));
        let sink = r.p.add(Node { doom: Some(Arc::clone(&doom)), ..r.node(&SINK, Role::Mute) });
        r.p.set_queue_capacity(sink, 1);
        r.link(src.src(OUT), sink.sink(IN));
        let mut run = r.step();
        quiesce(&mut run);
        let (gs, gk) = (group_of(&run, src), group_of(&run, sink));
        assert_eq!(
            run.outcome(gs),
            Some(Outcome::Blocked { on: profluens_core::sched::BlockedOn::Ring { pad: OUT } })
        );
        if seal {
            doom.store(true, Ordering::Release);
            assert_eq!(run.pass(gk).expect("sealed"), Outcome::Exit);
            quiesce(&mut run);
            assert!(r.of(src).contains(&What::SrcLinked(0, false, None)), "{:?}", r.of(src));
            assert!(matches!(run.outcome(gs), Some(Outcome::Idle { .. })), "{:?}", run.outcome(gs));
        }
        r.p.remove(src, Leave::Drain).expect("remove");
        r.p.remove(sink, if seal { Leave::Drain } else { Leave::Cut }).expect("remove");
        r.p.commit().expect("commit");
        quiesce(&mut run);
        assert_eq!(run.outcome(gs), Some(Outcome::Exit), "seal {seal}");
        assert_eq!(run.outcome(gk), Some(Outcome::Exit), "seal {seal}");
        assert!(r.run(&mut run, &mut Rng(4), None), "seal {seal}: the run ends");
        run.finish().expect("finish");
    }
}

#[test]
fn a_change_that_fails_before_its_splice_leaves_the_told_latency_as_it_was() {
    use profluens_core::pipeline::LatencyPolicy;
    let ms = Timestamp::from_millis;
    for policy in [LatencyPolicy::Grow, LatencyPolicy::Hold] {
        let mut r = Rig::new();
        let (src, merge, sink) =
            (r.add(&SRC, Role::Source), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
        r.link(src.src(OUT), merge.sink(IN));
        r.link(merge.src(OUT), sink.sink(IN));
        r.p.set_latency_policy(policy);
        let (mut run, mut rng) = (r.step(), Rng(5));
        assert!(!r.run(&mut run, &mut rng, Some(50)));
        let bad = r.p.add(Node { total: 30, fail: Fail::Start, ..r.node(&LATE, Role::Source) });
        r.link(bad.src(OUT_B), merge.sink(IN_B));
        let change = r.p.commit().expect("commit");
        assert!(!r.run(&mut run, &mut rng, Some(20)));
        assert!(!r.p.changing(), "{policy:?}");
        assert_eq!(r.p.failed_change(), Some(change), "{policy:?}");
        drop(bus(&r.p));
        if policy == LatencyPolicy::Hold {
            let plain = r.add(&SRC, Role::Source);
            r.p.set_element_pool(plain, 64, 64);
            let link = r.link(plain.src(OUT), merge.sink(IN_B));
            r.p.commit().expect("commit");
            run.service();
            let g = group_of(&run, plain);
            for _ in 0..60 {
                run.pass(g).expect("pass");
            }
            let blocked =
                Outcome::Blocked { on: profluens_core::sched::BlockedOn::Ring { pad: OUT } };
            assert_eq!(run.outcome(g), Some(blocked), "the held latency's leak was taken back");
            assert!(!r.run(&mut run, &mut rng, Some(20)));
            r.p.remove(plain, Leave::Cut).expect("remove");
            r.p.commit().expect("commit");
            assert!(!r.run(&mut run, &mut rng, Some(20)));
            assert!(!r.p.changing() && r.p.negotiated(link).is_none());
            drop(bus(&r.p));
        }
        let late = r.p.add(Node { total: 30, ..r.node(&LATE, Role::Source) });
        r.link(late.src(OUT_B), merge.sink(IN_B));
        r.p.commit().expect("commit");
        let said = bus(&r.p);
        let grown = said.iter().any(|m| matches!(m, BusMessage::LatencyChanged { old, new } if (*old, *new) == (ms(0), ms(30))));
        let held = said.iter().any(|m| matches!(m, BusMessage::LatencyHeld { kept, asked } if (*kept, *asked) == (ms(0), ms(30))));
        assert_eq!(
            (grown, held),
            (policy == LatencyPolicy::Grow, policy == LatencyPolicy::Hold),
            "{policy:?}"
        );
        assert!(r.run(&mut run, &mut rng, None), "{policy:?}: the run ends");
        run.finish().expect("finish");
        if policy == LatencyPolicy::Grow {
            for el in [merge, sink] {
                assert!(
                    r.of(el).contains(&What::Latency(ms(30).0)),
                    "{policy:?}: {} is told",
                    el.0
                );
            }
        }
    }
}

#[test]
fn an_inserted_element_is_marked_with_the_change_that_inserted_it() {
    let inserted =
        |p: &Pipeline| bus(p).iter().filter(|m| matches!(m, BusMessage::Inserted { .. })).count();
    let mut r = Rig::new();
    let (src, a) = (r.add(&SRC2, Role::Source), r.add(&SINK, Role::Sink));
    r.link(src.src(OUT), a.sink(IN));
    let (mut run, mut rng) = (r.step(), Rng(6));
    assert!(!r.run(&mut run, &mut rng, Some(30)));
    let join = |r: &mut Rig, fail: Fail| {
        let (fwd, b) =
            (r.p.add(Node { fail, ..r.node(&FWD, Role::Forward) }), r.add(&SINK, Role::Sink));
        let link = r.link(src.src(OUT_B), fwd.sink(IN));
        r.link(fwd.src(OUT), b.sink(IN));
        r.p.mark_auto(fwd, link);
        (fwd, b)
    };
    let (fwd, _) = join(&mut r, Fail::Never);
    let running = r.p.link_into(a, IN).expect("fed").0;
    r.p.mark_auto(a, running);
    assert!(r.p.is_auto(fwd) && r.p.is_auto(a));
    r.p.abandon();
    assert!(!r.p.is_auto(fwd) && !r.p.is_auto(a), "abandoned");
    let (fwd, _) = join(&mut r, Fail::Start);
    r.p.commit().expect("commit");
    assert!(!r.run(&mut run, &mut rng, Some(30)));
    assert!(!r.p.changing() && r.p.failed_change().is_some());
    assert!(!r.p.is_auto(fwd), "failed before its splice");
    assert_eq!(inserted(&r.p), 1, "said with the commit");
    let (fwd, b) = join(&mut r, Fail::Never);
    r.p.commit().expect("commit");
    assert_eq!(inserted(&r.p), 1);
    assert!(!r.run(&mut run, &mut rng, Some(60)));
    assert!(!r.p.changing() && r.p.is_auto(fwd));
    for el in [fwd, b] {
        r.p.remove(el, Leave::Cut).expect("remove");
    }
    r.p.commit().expect("commit");
    assert!(!r.run(&mut run, &mut rng, Some(60)));
    assert!(!r.p.changing() && !r.p.is_auto(fwd), "left");
    assert!(r.run(&mut run, &mut rng, None));
    run.finish().expect("finish");
}

#[test]
fn a_stop_says_whether_a_change_removed_the_element_or_was_taken_back() {
    use StopReason::{Removed, RolledBack, RunEnd};
    for seed in scaled(20) {
        for how in [Leave::Cut, Leave::Drain] {
            let (mut r, src, a) = fan_out();
            let b = r.add(&SINK, Role::Sink);
            r.link(src.src(OUT_B), b.sink(IN));
            let (mut run, mut rng) = (r.step(), Rng(seed));
            assert!(!r.run(&mut run, &mut rng, Some(60)));
            r.p.remove(b, how).expect("remove");
            r.p.commit().expect("commit");
            assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
            run.finish().expect("finish");
            assert_eq!(r.reasons(b), [Removed], "seed {seed} {how:?}");
            assert_eq!(
                (r.reasons(a), r.reasons(src)),
                (vec![RunEnd], vec![RunEnd]),
                "seed {seed} {how:?}"
            );
        }
        for fail in [Fail::Start, Fail::Process(1)] {
            let mut r = Rig::new();
            let (src, merge, sink) =
                (r.add(&SRC, Role::Source), r.add(&MERGE, Role::Merge), r.add(&SINK, Role::Sink));
            r.link(src.src(OUT), merge.sink(IN));
            r.link(merge.src(OUT), sink.sink(IN));
            let (mut run, mut rng) = (r.step(), Rng(seed));
            assert!(!r.run(&mut run, &mut rng, Some(50)));
            let (feeder, fwd) = (r.add(&SRC, Role::Source), r.add(&FWD, Role::Forward));
            let bad = r.p.add(Node { fail, ..r.node(&INLINE, Role::Forward) });
            r.link(feeder.src(OUT), fwd.sink(IN));
            r.link(fwd.src(OUT), bad.sink(IN));
            r.link(bad.src(OUT), merge.sink(IN_B));
            r.p.commit().expect("commit");
            assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
            run.finish().expect("finish");
            assert_eq!(
                (r.reasons(feeder), r.reasons(fwd)),
                (vec![RolledBack], vec![RolledBack]),
                "seed {seed}"
            );
            let bad_stopped = if fail == Fail::Start { vec![] } else { vec![RolledBack] };
            assert_eq!(r.reasons(bad), bad_stopped, "seed {seed}");
            assert_eq!(r.reasons(sink), [RunEnd], "seed {seed}");
        }
        let (mut r, mut run, mut rng, [_, _, sink, mute], _) = waiting(seed);
        r.p.cancel().expect("cancelled");
        assert!(r.run(&mut run, &mut rng, None), "seed {seed}: the run ends");
        run.finish().expect("finish");
        assert_eq!(
            (r.reasons(mute), r.reasons(sink)),
            (vec![RolledBack], vec![RunEnd]),
            "seed {seed}"
        );
    }
}
