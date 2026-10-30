use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::command::{Command, CommandError, Control};
use profluens_core::counters::{CounterSnapshot, LatencySnapshot};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, Scope};
use profluens_core::event::Event;
use profluens_core::feedback::{Feedback, Qos};
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::io::{Completion, Handle, IoResult, Reactor, Submission};
use profluens_core::level::{Fill, Limit};
use profluens_core::log::DebugSpec;
use profluens_core::memory::PoolStats;
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{BlockedOn, Cut, Outcome, Stepped};
use profluens_core::time::Timestamp;

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
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
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

static SRC: ElementDesc = desc("ssrc", &[], &ONE_OUT, SchedHint::Active);
static SRC2: ElementDesc = desc("ssrc2", &[], &TWO_OUT, SchedHint::Active);
static FWD: ElementDesc = desc("sfwd", &ONE_IN, &ONE_OUT, SchedHint::Active);
static INLINE: ElementDesc = desc("sinline", &ONE_IN, &ONE_OUT, SchedHint::Passive);
static TEE: ElementDesc = desc("stee", &ONE_IN, &TWO_OUT, SchedHint::Active);
static MERGE: ElementDesc = desc("smerge", &TWO_IN, &ONE_OUT, SchedHint::Active);
static SINK: ElementDesc = desc("ssink", &ONE_IN, &[], SchedHint::Active);
static LONE: ElementDesc = desc("slone", &[], &ONE_OUT, SchedHint::Active);
static REST: ElementDesc = desc("srest", &ONE_IN, &[], SchedHint::Passive);

const IN: SinkPad = SinkPad(0);
const IN_B: SinkPad = SinkPad(1);
const OUT: SrcPad = SrcPad(0);
const OUT_B: SrcPad = SrcPad(1);

const STEP: Timestamp = Timestamp::from_millis(20);

// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum What {
    Start,
    Stop,
    Paused,
    Resumed,
    Flush,
    Skip(Timestamp),
    Eos,
    Tick,
    Asked,
    Told(i64),
    Heard(Option<i64>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rec {
    Mark { element: u32, what: What, seek_gen: u64 },
    Row(Row),
    Dropped(Row),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Row {
    sink: u32,
    stream: u64,
    seq: u64,
    pts: Timestamp,
    flags: u32,
    made_in: u64,
    taken_in: u64,
    at: Timestamp,
    running: Timestamp,
}

type Tape = Arc<Mutex<Vec<Rec>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Source {
        count: u64,
    },
    Forward,
    Merge,
    Sink {
        timed: bool,
    },
    Screen,
    Stuck,
    Hoard,
    FailAt(u64),
    /// Fails as `FailAt`, scoped to its branch.
    FailBranchAt(u64),
    Late,
    PanicAt(u64),
    FailStart,
    Pump(Duration),
    WakePast,
    Meddle,
    Scribe,
}

#[derive(Debug)]
struct Broke;

impl std::fmt::Display for Broke {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the test element broke")
    }
}

impl std::error::Error for Broke {}

struct Node {
    desc: &'static ElementDesc,
    role: Role,
    tape: Tape,
    clock: MockClock,
    every: Timestamp,
    seek_gen: u64,
    next: u64,
    fresh: bool,
    taken: u64,
    stash: Vec<Buffer>,
    hoard: Vec<Buffer>,
    control: Arc<Control>,
    file: Option<Handle>,
    heard: Option<i64>,
}

fn late(seek_gen: u64, seq: u64) -> i64 {
    ((seek_gen << 32) | (seq + 1)) as i64
}

fn seek_to(start: u64) -> Command {
    Command::Seek { seek_gen: 0, to_time: Timestamp(start * STEP.0) }
}

impl Node {
    fn mark(&self, ctx: &Ctx, what: What) {
        let (element, seek_gen) = (ctx.element().0, self.seek_gen);
        self.tape.lock().unwrap().push(Rec::Mark { element, what, seek_gen });
    }

    fn stock(&mut self, ctx: &mut Ctx, want: usize) -> bool {
        while self.stash.len() < want {
            let Some(buf) = ctx.try_alloc() else {
                return false;
            };
            self.stash.push(buf);
        }
        true
    }

    fn emit(&mut self, ctx: &mut Ctx, count: u64) -> Result<Flow, Error> {
        let heard = ctx.feedback(OUT).qos.map(|q| q.lateness);
        if heard != self.heard {
            self.heard = heard;
            self.mark(ctx, What::Heard(heard));
        }
        if self.next >= count {
            return Ok(Flow::Eos);
        }
        if !self.stock(ctx, self.desc.srcs.len()) {
            return Ok(Flow::Ok);
        }
        let key = self.next.is_multiple_of(8);
        let mut flags = if key { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
        if std::mem::take(&mut self.fresh) {
            flags |= BufferFlags::DISCONT;
        }
        for (stream, mut buf) in self.stash.drain(..).enumerate() {
            buf.memory.as_mut_full()[..8].copy_from_slice(&self.next.to_le_bytes());
            buf.memory.set_len(8);
            buf.pts = Timestamp(self.next * self.every.0);
            buf.dts = Timestamp(self.seek_gen);
            buf.duration = Timestamp(stream as u64);
            buf.flags = flags;
            ctx.out(SrcPad(stream as u32)).push(buf);
        }
        self.next += 1;
        Ok(Flow::Ok)
    }

    fn forward(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        let copies = self.desc.srcs.len().saturating_sub(1);
        while inputs.front().is_some() {
            if !self.stock(ctx, copies) {
                break;
            }
            let Some(buf) = inputs.pop() else { break };
            self.taken += 1;
            match self.role {
                Role::FailAt(n) if n == self.taken => {
                    return Err(Error::element_failed(ctx.element(), Broke));
                }
                Role::FailBranchAt(n) if n == self.taken => {
                    return Err(Error::element_failed(ctx.element(), Broke).scoped(Scope::Branch));
                }
                Role::PanicAt(n) if n == self.taken => panic!("the test element panicked"),
                _ => {}
            }
            for (k, mut copy) in self.stash.drain(..).enumerate() {
                copy.memory.set_len(buf.memory.len());
                (copy.pts, copy.dts, copy.duration, copy.flags) =
                    (buf.pts, buf.dts, buf.duration, buf.flags);
                ctx.out(SrcPad(k as u32 + 1)).push(copy);
            }
            ctx.out(OUT).push(buf);
        }
        Ok(Flow::Ok)
    }

    fn ask(&mut self, ctx: &mut Ctx, buf: Buffer) {
        if let Some(file) = self.file {
            ctx.io().write(file, 0, buf, 0);
            self.mark(ctx, What::Asked);
        }
    }

    fn reap(&mut self, ctx: &mut Ctx) {
        while ctx.io().next_completion().is_some() {
            self.mark(ctx, What::Tick);
        }
    }

    fn merge(&mut self, ctx: &mut Ctx) {
        for pad in [IN, IN_B] {
            let mut batch = ctx.take_input_on(pad);
            while let Some(buf) = batch.pop_front() {
                ctx.out(OUT).push(buf);
            }
            ctx.recycle_input(batch);
        }
    }

    fn record(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>, timed: bool) {
        while let Some(front) = inputs.front() {
            let pts = front.pts;
            let before = ctx.before_presentation(pts, Timestamp::ZERO);
            let preroll = self.role == Role::Screen && self.fresh;
            if !before && timed && !preroll && !ctx.due(pts) {
                break;
            }
            let Some(buf) = inputs.pop() else { break };
            let row = Row {
                sink: ctx.element().0,
                stream: buf.duration.0,
                seq: buf.pts.0 / self.every.0,
                pts: buf.pts,
                flags: buf.flags.0,
                made_in: buf.dts.0,
                taken_in: self.seek_gen,
                at: self.clock.now(),
                running: ctx.now(),
            };
            if before {
                ctx.count_drop();
            } else {
                self.fresh = false;
            }
            let rec = if before { Rec::Dropped(row) } else { Rec::Row(row) };
            self.tape.lock().unwrap().push(rec);
            if self.role == Role::Late {
                let qos = Qos { lateness: late(self.seek_gen, row.seq), earliest: row.pts };
                ctx.publish_feedback(IN, Feedback::Qos(qos));
            }
        }
    }
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.role == Role::FailStart {
            return Err(Error::element_failed(ctx.element(), Broke));
        }
        if self.role == Role::Scribe {
            self.file = Some(Handle::new(0, 0));
        }
        self.mark(ctx, What::Start);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        match self.role {
            Role::Source { count } => return self.emit(ctx, count),
            Role::Forward
            | Role::FailAt(_)
            | Role::FailBranchAt(_)
            | Role::PanicAt(_)
            | Role::FailStart => {
                return self.forward(ctx, &mut inputs);
            }
            Role::Merge => self.merge(ctx),
            Role::Sink { timed } => self.record(ctx, &mut inputs, timed),
            Role::Late => self.record(ctx, &mut inputs, false),
            Role::Screen => self.record(ctx, &mut inputs, true),
            Role::Stuck => {}
            Role::Pump(interval) => {
                ctx.wake_within(interval);
                self.mark(ctx, What::Tick);
            }
            Role::WakePast => {
                ctx.wake_at(Timestamp::ZERO);
                self.mark(ctx, What::Tick);
            }
            Role::Hoard => {
                while let Some(buf) = inputs.pop() {
                    self.hoard.push(buf);
                }
            }
            Role::Meddle => {
                if std::mem::take(&mut self.fresh) {
                    self.control.append(Command::Pause).expect("room in the log");
                }
                self.mark(ctx, What::Tick);
            }
            Role::Scribe => {
                while let Some(buf) = inputs.pop() {
                    self.ask(ctx, buf);
                }
                self.reap(ctx);
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        let what = match event {
            Event::FlushStart => {
                let to = ctx.presentation_point().map_or(0, |t| t.0 / STEP.0);
                (self.seek_gen, self.next, self.fresh) = (ctx.applied_seek_gen(), to, true);
                assert!(self.seek_gen <= ctx.seek_gen(), "flushing for a seek nobody issued");
                What::Flush
            }
            Event::SkippedTo { to_time } => {
                assert_eq!(
                    ctx.presentation_point(),
                    Some(*to_time),
                    "told one point, holds another"
                );
                self.fresh |= self.role == Role::Screen;
                What::Skip(*to_time)
            }
            Event::Paused => What::Paused,
            Event::Resumed => What::Resumed,
            Event::Eos => What::Eos,
            Event::Feedback { feedback: Feedback::Qos(q), .. } => What::Told(q.lateness),
            _ => return Ok(()),
        };
        self.mark(ctx, what);
        if what == What::Eos && self.role == Role::Scribe {
            let buf = ctx.alloc();
            self.ask(ctx, buf);
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.reap(ctx);
        self.stash.clear();
        self.hoard.clear();
        self.mark(ctx, What::Stop);
    }
}

struct Rig {
    p: Pipeline,
    tape: Tape,
    clock: MockClock,
    every: Timestamp,
}

impl Rig {
    fn new(pool_slots: u32) -> Rig {
        let clock = MockClock::starting_at(Timestamp::from_secs(5));
        let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
        p.set_pool(64, pool_slots);
        Rig { p, tape: Tape::default(), clock, every: STEP }
    }

    fn add(&mut self, desc: &'static ElementDesc, role: Role) -> ElementId {
        self.p.add(Node {
            desc,
            role,
            tape: Arc::clone(&self.tape),
            clock: self.clock.clone(),
            every: self.every,
            seek_gen: 0,
            next: 0,
            fresh: true,
            taken: 0,
            stash: Vec::new(),
            hoard: Vec::new(),
            control: self.p.control(),
            file: None,
            heard: None,
        })
    }

    fn link(&mut self, src: (ElementId, SrcPad), sink: (ElementId, SinkPad)) {
        self.p.link(src, sink).expect("link");
    }

    fn step(&mut self) -> Stepped {
        self.p.step(Arc::new(self.clock.clone())).expect("step")
    }

    fn tape(&self) -> Vec<Rec> {
        self.tape.lock().unwrap().clone()
    }

    fn rows(&self, sink: ElementId) -> Vec<Row> {
        let of = |r: &Rec| match r {
            Rec::Row(row) if row.sink == sink.0 => Some(*row),
            _ => None,
        };
        self.tape.lock().unwrap().iter().filter_map(of).collect()
    }

    fn marks(&self, el: ElementId) -> Vec<What> {
        let of = |r: &Rec| match r {
            Rec::Mark { element, what, .. } if *element == el.0 => Some(*what),
            _ => None,
        };
        self.tape.lock().unwrap().iter().filter_map(of).collect()
    }

    fn count(&self, el: ElementId, what: What) -> usize {
        self.marks(el).iter().filter(|w| **w == what).count()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    Pause,
    Resume,
    Seek(u64),
    Skip(Timestamp),
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Did {
    Pass { step: u32, group: u32, outcome: Outcome },
    Jump { step: u32, to: Timestamp },
    Sent { step: u32, act: Act, taken: bool },
}

#[derive(Default, Debug)]
struct App {
    script: Vec<(u32, Act)>,
    sent: usize,
    paused: bool,
    stopped: bool,
    starts: Vec<u64>,
    skips: Vec<(u64, Timestamp)>,
    last_seq: Option<u64>,
}

impl App {
    fn send(&mut self, run: &Stepped, act: Act) -> Result<bool, String> {
        let cmd = match act {
            Act::Pause => Command::Pause,
            Act::Resume => Command::Resume,
            Act::Seek(start) => seek_to(start),
            Act::Skip(to_time) => Command::SkipTo { to_time },
            Act::Stop => Command::Stop,
        };
        let finished = run.control().is_finished();
        let seq = match run.control().append(cmd) {
            Ok(_) if finished => return Err(format!("a finished run took {act:?}")),
            Ok(seq) => seq,
            Err(CommandError::Finished) if finished => return Ok(false),
            Err(CommandError::Redundant)
                if !finished && !matches!(act, Act::Seek(_) | Act::Skip(_)) =>
            {
                return Ok(false);
            }
            Err(e) => return Err(format!("{act:?} was refused as {e:?}, finished {finished}")),
        };
        self.last_seq = Some(seq);
        match act {
            Act::Pause => self.paused = true,
            Act::Resume => self.paused = false,
            Act::Stop => self.stopped = true,
            Act::Seek(start) => self.starts.push(start),
            Act::Skip(to_time) => self.skips.push((self.starts.len() as u64 - 1, to_time)),
        }
        Ok(true)
    }
}

struct Loop {
    rng: Rng,
    spurious: u64,
    limit: u32,
    keep_trace: bool,
    print: bool,
    halves: bool,
    trace: Vec<Did>,
    passes: u64,
    jumps: u64,
}

impl Loop {
    fn new(seed: u64) -> Loop {
        let (spurious, limit, keep_trace) = (0, 200_000, true);
        let print = std::env::var_os("PROFLUENS_STEP_TRACE").is_some();
        let (trace, passes, jumps, halves) = (Vec::new(), 0, 0, false);
        Loop { rng: Rng(seed), spurious, limit, keep_trace, print, halves, trace, passes, jumps }
    }

    fn did(&mut self, did: Did) {
        if self.print {
            println!("{did:?}");
        }
        if self.keep_trace {
            self.trace.push(did);
        }
    }

    fn spurious_pick(&mut self, run: &Stepped) -> Option<usize> {
        if self.spurious == 0 || self.rng.below(self.spurious) != 0 {
            return None;
        }
        let waits = |g: &usize| run.outcome(*g) != Some(Outcome::Exit) && !run.ready(*g);
        let idle: Vec<usize> = (0..run.groups()).filter(waits).collect();
        (!idle.is_empty()).then(|| idle[self.rng.below(idle.len() as u64) as usize])
    }

    fn run(
        &mut self,
        run: &mut Stepped,
        clock: &MockClock,
        app: &mut App,
        mut check: impl FnMut(&Stepped, &App) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut ready = Vec::new();
        for step in 0..self.limit {
            while app.script.get(app.sent).is_some_and(|(at, _)| *at <= step) {
                let act = app.script[app.sent].1;
                app.sent += 1;
                let taken = app.send(run, act).map_err(|e| format!("at step {step}: {e}"))?;
                self.did(Did::Sent { step, act, taken });
            }
            ready.clear();
            ready.extend((0..run.groups()).filter(|g| run.ready(*g)));
            let group = if let Some(idle) = self.spurious_pick(run) {
                idle
            } else if !ready.is_empty() {
                ready[self.rng.below(ready.len() as u64) as usize]
            } else if let Some(to) = run.next_deadline() {
                let now = clock.now();
                if to <= now {
                    return Err(format!("deadline {to:?} is not ahead of the clock at {now:?}"));
                }
                clock.advance(Timestamp(to.0 - now.0));
                self.jumps += 1;
                self.did(Did::Jump { step, to });
                continue;
            } else if let Some(next) = app.script.get_mut(app.sent) {
                next.0 = step;
                continue;
            } else if app.paused && !app.stopped && !run.control().is_finished() {
                app.script.push((step, Act::Resume));
                continue;
            } else {
                return Ok(());
            };
            if self.halves {
                run.pass_to(group, Cut::Drained)
                    .map_err(|e| format!("top of group {group}: {e}"))?;
            }
            let outcome = run.pass(group).map_err(|e| format!("pass of group {group}: {e}"))?;
            self.passes += 1;
            self.did(Did::Pass { step, group: group as u32, outcome });
            check(run, app).map_err(|e| format!("after step {step}: {e}"))?;
        }
        Err(format!("no quiescence within {} steps", self.limit))
    }
}

fn no_check(_: &Stepped, _: &App) -> Result<(), String> {
    Ok(())
}

fn lone(rig: &mut Rig, role: Role) -> ElementId {
    let head = rig.add(&LONE, role);
    let rest = rig.add(&REST, Role::Sink { timed: false });
    rig.link(head.src(OUT), rest.sink(IN));
    head
}

fn group_of(run: &Stepped, el: ElementId) -> usize {
    (0..run.groups()).find(|g| run.elements(*g).contains(&el)).expect("an element of the run")
}

fn all_gone(run: &Stepped) -> bool {
    (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
}

fn stall_report(run: &Stepped) -> String {
    let line = |g| format!("group {g}: {:?}, holding {}", run.outcome(g), run.held(g));
    (0..run.groups()).map(line).collect::<Vec<_>>().join("\n")
}

fn check_lifecycle(tape: &[Rec], elements: u32) -> Result<(), String> {
    for el in 0..elements {
        let marks: Vec<What> = tape
            .iter()
            .filter_map(|r| match r {
                Rec::Mark { element, what, .. } if *element == el => Some(*what),
                _ => None,
            })
            .collect();
        let starts = marks.iter().filter(|w| **w == What::Start).count();
        let stops = marks.iter().filter(|w| **w == What::Stop).count();
        let framed = marks.first() == Some(&What::Start) && marks.last() == Some(&What::Stop);
        if starts != 1 || stops != 1 || !framed {
            return Err(format!("element {el} lifecycle {marks:?}"));
        }
        let mut paused = false;
        for what in marks {
            let flips = matches!(what, What::Paused | What::Resumed);
            if flips && (what == What::Paused) == paused {
                return Err(format!("element {el} got {what:?} while paused is {paused}"));
            }
            paused ^= flips;
        }
    }
    Ok(())
}

fn seq_at(to_time: Timestamp) -> u64 {
    to_time.0.div_ceil(STEP.0)
}

struct SinkCheck {
    sink: u32,
    next: Vec<[u64; 2]>,
    ended: Vec<u32>,
    point: Vec<Timestamp>,
    newest: u64,
    skipped: usize,
}

impl SinkCheck {
    fn feed(&mut self, rec: &Rec, app: &App) -> Result<(), String> {
        let starts = &app.starts;
        while self.next.len() < starts.len() {
            let start = starts[self.next.len()];
            self.next.push([start, start]);
            self.ended.push(0);
            self.point.push(Timestamp(start * STEP.0));
        }
        match *rec {
            Rec::Mark { element, what: What::Skip(to_time), seek_gen } if element == self.sink => {
                let left = &app.skips[self.skipped..];
                let Some(at) = left.iter().position(|s| *s == (seek_gen, to_time)) else {
                    return Err(format!("sink {element} got a skip to {to_time:?} nobody sent"));
                };
                self.skipped += at + 1;
                self.point[seek_gen as usize] = to_time;
            }
            Rec::Mark { element, what: What::Eos, seek_gen } if element == self.sink => {
                self.ended[seek_gen as usize] += 1;
                if self.ended[seek_gen as usize] > 1 {
                    return Err(format!("sink {element} got Eos twice in generation {seek_gen}"));
                }
            }
            Rec::Row(row) | Rec::Dropped(row) if row.sink == self.sink => {
                let dropped = matches!(rec, Rec::Dropped(_));
                if row.made_in != row.taken_in {
                    return Err(format!("a buffer crossed generations, {row:?}"));
                }
                if row.made_in < self.newest {
                    return Err(format!("a generation came back, {row:?}"));
                }
                self.newest = row.made_in;
                let (g, s) = (row.made_in as usize, row.stream as usize);
                if self.ended[g] != 0 {
                    return Err(format!("a buffer after end of stream, {row:?}"));
                }
                if dropped != (row.pts < self.point[g]) {
                    let did = if dropped { "dropped at or after" } else { "presented before" };
                    return Err(format!("{did} the point {:?}, {row:?}", self.point[g]));
                }
                if row.seq != self.next[g][s] || row.pts.0 != row.seq * STEP.0 {
                    return Err(format!("out of order, wanted {} in {row:?}", self.next[g][s]));
                }
                if (row.seq == starts[g]) != (row.flags & BufferFlags::DISCONT.0 != 0) {
                    return Err(format!("discontinuity flag on the wrong buffer, {row:?}"));
                }
                if !dropped && row.running != row.pts {
                    return Err(format!("taken off its deadline, {row:?}"));
                }
                self.next[g][s] += 1;
            }
            _ => {}
        }
        Ok(())
    }
}

struct Oracle {
    sinks: Vec<SinkCheck>,
    swept: usize,
}

impl Oracle {
    fn new(sinks: &[ElementId]) -> Oracle {
        let check = |sink: &ElementId| SinkCheck {
            sink: sink.0,
            next: Vec::new(),
            ended: Vec::new(),
            point: Vec::new(),
            newest: 0,
            skipped: 0,
        };
        Oracle { sinks: sinks.iter().map(check).collect(), swept: 0 }
    }

    fn sweep(&mut self, tape: &Tape, app: &App) -> Result<(), String> {
        let tape = tape.lock().unwrap();
        for rec in &tape[self.swept..] {
            for sink in &mut self.sinks {
                sink.feed(rec, app)?;
            }
        }
        self.swept = tape.len();
        Ok(())
    }
}

fn check_complete(
    tape: &[Rec],
    sink: ElementId,
    stream: u64,
    starts: &[u64],
    count: u64,
) -> Result<(), String> {
    let last = starts.len() as u64 - 1;
    let (mut next, mut ends) = (starts[last as usize], 0);
    for rec in tape {
        match *rec {
            Rec::Mark { element, what: What::Eos, seek_gen } => {
                ends += (element == sink.0 && seek_gen == last) as u32;
            }
            Rec::Row(row) | Rec::Dropped(row)
                if row.sink == sink.0 && row.made_in == last && row.stream == stream =>
            {
                if row.seq != next {
                    return Err(format!(
                        "sink {} generation {last}: {} for {next}",
                        sink.0, row.seq
                    ));
                }
                next += 1;
            }
            _ => {}
        }
    }
    if next < count {
        return Err(format!("sink {} generation {last}: ended at {next} of {count}", sink.0));
    }
    if ends != 1 {
        return Err(format!("sink {} generation {last}: {ends} end of stream marks", sink.0));
    }
    Ok(())
}

#[derive(Default)]
struct EndCheck {
    finishes: u32,
    finished: bool,
    gone: usize,
}

impl EndCheck {
    fn feed(
        &mut self,
        run: &Stepped,
        app: &App,
        tape: &Tape,
        sinks: &[ElementId],
    ) -> Result<(), String> {
        let gone = (0..run.groups()).filter(|g| run.outcome(*g) == Some(Outcome::Exit)).count();
        let over = self.finished || app.stopped;
        if over && gone != self.gone + 1 {
            return Err("a pass in a run that is over did not exit".into());
        }
        if !over && gone != 0 {
            return Err("a group left a run that is not over".into());
        }
        self.gone = gone;
        let finished = run.control().is_finished();
        if self.finished && !finished {
            return Err("the finish was taken back".into());
        }
        if finished && !self.finished {
            self.finishes += 1;
            let last = app.starts.len() as u64 - 1;
            let tape = tape.lock().unwrap();
            let ended = |sink: &ElementId| {
                tape.iter().any(|r| {
                    matches!(r, Rec::Mark { element, what: What::Eos, seek_gen }
                        if *element == sink.0 && *seek_gen == last)
                })
            };
            if let Some(sink) = sinks.iter().find(|sink| !ended(sink)) {
                return Err(format!("finished before sink {} ended generation {last}", sink.0));
            }
        }
        self.finished = finished;
        Ok(())
    }
}

type Histogram = (Vec<u64>, u64, u64, u64);

fn latency_key(s: LatencySnapshot) -> Histogram {
    (s.buckets.to_vec(), s.count, s.sum_ns, s.max_ns)
}

#[derive(PartialEq, Debug)]
struct Outcomes {
    trace: Vec<Did>,
    tape: Vec<Rec>,
    counters: Vec<CounterSnapshot>,
    latency: Vec<[Histogram; 3]>,
    pool: PoolStats,
    clock: Timestamp,
}

fn outcomes(rig: &Rig, trace: Vec<Did>, elements: u32) -> Outcomes {
    let tap = rig.p.tap_handle();
    let latency = |el| {
        let (process, queue, late) = tap.latency(ElementId(el)).expect("element");
        [latency_key(process), latency_key(queue), latency_key(late)]
    };
    Outcomes {
        trace,
        tape: rig.tape(),
        counters: (0..elements).map(|el| rig.p.counters(ElementId(el))).collect(),
        latency: (0..elements).map(latency).collect(),
        pool: tap.pool(),
        clock: rig.clock.now(),
    }
}

const COUNT: u64 = 24;
const HORIZON: u64 = 260;

struct Commanded {
    rig: Rig,
    sinks: [ElementId; 2],
    elements: u32,
}

fn commanded(pool_slots: u32) -> Commanded {
    let mut rig = Rig::new(pool_slots);
    let src = rig.add(&SRC2, Role::Source { count: COUNT });
    let fwd = rig.add(&FWD, Role::Forward);
    let inline = rig.add(&INLINE, Role::Forward);
    let a = rig.add(&SINK, Role::Sink { timed: true });
    let b = rig.add(&SINK, Role::Sink { timed: true });
    rig.link(src.src(OUT), fwd.sink(IN));
    rig.link(fwd.src(OUT), inline.sink(IN));
    rig.link(inline.src(OUT), a.sink(IN));
    rig.link(src.src(OUT_B), b.sink(IN));
    Commanded { rig, sinks: [a, b], elements: 5 }
}

fn script(rng: &mut Rng) -> Vec<(u32, Act)> {
    let n = 2 + rng.below(11);
    let mut script: Vec<(u32, Act)> = (0..n)
        .map(|_| {
            let act = match rng.below(128) {
                0 => Act::Stop,
                1..=40 => Act::Pause,
                41..=80 => Act::Resume,
                _ => Act::Seek(rng.below(COUNT + 2)),
            };
            (rng.below(HORIZON) as u32, act)
        })
        .collect();
    script.sort_by_key(|(at, _)| *at);
    script
}

fn add_skips(script: &mut Vec<(u32, Act)>, seed: u64) {
    let mut rng = Rng(seed ^ 0x5eed_0f5c_1b5e_ed00);
    for _ in 0..rng.below(4) {
        let to_time = Timestamp(rng.below(COUNT + 2) * STEP.0 + rng.below(2) * STEP.0 / 2);
        script.push((rng.below(HORIZON) as u32, Act::Skip(to_time)));
    }
    script.sort_by_key(|(at, _)| *at);
}

#[derive(Clone, Copy)]
struct Setup {
    tracing: bool,
    tight: bool,
    skips: bool,
    halves: bool,
    limited: bool,
}

const LIMIT: Timestamp = Timestamp::from_millis(40);

const PLAIN: Setup =
    Setup { tracing: false, tight: false, skips: true, halves: false, limited: false };

fn commanded_run(seed: u64, setup: Setup) -> Result<Outcomes, String> {
    let Setup { tracing, tight, skips, halves, limited } = setup;
    let mut rng = Rng(seed);
    let roomy = [6, 10, 24, 64][rng.below(4) as usize];
    let pool_slots = if tight { [2, 3, 4, 5][(seed % 4) as usize] } else { roomy };
    let Commanded { mut rig, sinks, elements } = commanded(pool_slots);
    rig.p.set_tracing(tracing);
    if limited {
        sinks.iter().for_each(|sink| rig.p.set_link_limit(*sink, Limit::time(LIMIT)));
    }
    let mut app = App { script: script(&mut rng), starts: vec![0], ..App::default() };
    if skips {
        add_skips(&mut app.script, seed);
    }
    let mut run = rig.step();
    let mut driver = Loop::new(rng.next());
    (driver.limit, driver.halves) = (20_000, halves);
    let tap = rig.p.tap_handle();
    let mut oracle = Oracle::new(&sinks);
    let mut end = EndCheck::default();
    let each_pass = |run: &Stepped, app: &App| {
        let pool = tap.pool();
        if pool.outstanding > pool.max_slots {
            return Err(format!("pool over its cap, {pool:?}"));
        }
        for sink in sinks.iter().filter(|_| limited) {
            let held = tap.queued(*sink, IN).map_or(Timestamp::ZERO, |held| held.time);
            if held >= Timestamp(LIMIT.0 + STEP.0) {
                return Err(format!(
                    "the link into {sink:?} holds {held:?}, its limit is {LIMIT:?}"
                ));
            }
        }
        oracle.sweep(&rig.tape, app)?;
        end.feed(run, app, &rig.tape, &sinks)
    };
    driver.run(&mut run, &rig.clock, &mut app, each_pass)?;
    if !all_gone(&run) {
        let pool = tap.pool();
        let report = stall_report(&run);
        return Err(format!(
            "quiescent but not finished, {pool_slots} slots and {} out\n{report}",
            pool.outstanding
        ));
    }
    if (0..run.groups()).any(|g| run.held(g) != 0) {
        return Err(format!("a finished run holds data\n{}", stall_report(&run)));
    }
    if end.finishes != !app.stopped as u32 || run.control().is_finished() == app.stopped {
        return Err(format!("{} finishes in a run with stopped {}", end.finishes, app.stopped));
    }
    run.finish().map_err(|e| format!("finish: {e}"))?;
    oracle.sweep(&rig.tape, &app)?;
    let tape = rig.tape();
    check_lifecycle(&tape, elements)?;
    for (stream, sink) in sinks.into_iter().enumerate() {
        if !app.stopped {
            check_complete(&tape, sink, stream as u64, &app.starts, COUNT)?;
        }
    }
    let pool = tap.pool();
    if pool.outstanding != 0 {
        return Err(format!("the pool is missing buffers, {pool:?}"));
    }
    if app.last_seq.is_some_and(|seq| !rig.p.control().applied_by_all(seq)) {
        return Err("a group that is gone still holds the command log".into());
    }
    let ends = std::iter::from_fn(|| rig.p.bus().try_recv()).collect::<Vec<_>>();
    if !matches!(ends[..], [BusMessage::Eos]) {
        return Err(format!("the bus carries {} messages at the end", ends.len()));
    }
    Ok(outcomes(&rig, driver.trace, elements))
}

const COMMAND_SEEDS: u64 = 10_000;

fn seeds() -> std::ops::Range<u64> {
    let var = |name| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    match var("PROFLUENS_STEP_SEED") {
        Some(one) => one..one + 1,
        None => {
            let from = var("PROFLUENS_STEP_FROM").unwrap_or(0);
            from..from + var("PROFLUENS_STEP_SEEDS").unwrap_or(0).max(COMMAND_SEEDS)
        }
    }
}

#[test]
fn a_seed_replays_exactly_with_its_counters() {
    let traced = Setup { tracing: true, ..PLAIN };
    for seed in scaled(200) {
        let first = commanded_run(seed, traced).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let second = commanded_run(seed, traced).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert!(first == second, "seed {seed} ran two ways");
    }
    let (a, b) = (commanded_run(1, traced).unwrap(), commanded_run(2, traced).unwrap());
    assert!(a.trace != b.trace, "the seed picks the interleaving");
}

#[derive(Default)]
struct Sweep {
    failed: Vec<(u64, String)>,
    runs: u64,
    steps: usize,
    seeks: usize,
    skips: usize,
    stops: u32,
    cpu: Duration,
}

fn on_cpu() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    stat.split_whitespace().next()?.parse().ok().map(Duration::from_nanos)
}

fn sweep_shard(seeds: impl Iterator<Item = u64>, setup: Setup) -> Sweep {
    let (mut sweep, wall, cpu) = (Sweep::default(), Instant::now(), on_cpu());
    for seed in seeds {
        sweep.runs += 1;
        match commanded_run(seed, setup) {
            Ok(out) => {
                let sent = |d: &&Did, want: fn(&Act) -> bool| matches!(d, Did::Sent { act, taken: true, .. } if want(act));
                sweep.steps += out.trace.len();
                sweep.stops += out.trace.iter().any(|d| sent(&d, |a| *a == Act::Stop)) as u32;
                sweep.seeks +=
                    out.trace.iter().filter(|d| sent(d, |a| matches!(a, Act::Seek(_)))).count();
                sweep.skips +=
                    out.trace.iter().filter(|d| sent(d, |a| matches!(a, Act::Skip(_)))).count();
            }
            Err(e) => sweep.failed.push((seed, e)),
        }
    }
    sweep.cpu = match (cpu, on_cpu()) {
        (Some(from), Some(to)) => to.saturating_sub(from),
        _ => wall.elapsed(),
    };
    sweep
}

fn sweep(range: std::ops::Range<u64>, setup: Setup) -> Sweep {
    let (tight, limited) = (setup.tight, setup.limited);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let var = std::env::var("PROFLUENS_STEP_WORKERS").ok().and_then(|s| s.parse::<u64>().ok());
    let workers = var.unwrap_or(cores.min(8)).max(1);
    let wall = Instant::now();
    let shards: Vec<Sweep> = std::thread::scope(|scope| {
        let shard = |k| {
            let seeds = range.clone().filter(move |seed| seed % workers == k);
            scope.spawn(move || sweep_shard(seeds, setup))
        };
        let handles: Vec<_> = (0..workers).map(shard).collect();
        handles.into_iter().map(|h| h.join().expect("a sweep worker")).collect()
    });
    let mut total = Sweep::default();
    for shard in shards {
        total.failed.extend(shard.failed);
        total.runs += shard.runs;
        total.steps += shard.steps;
        total.seeks += shard.seeks;
        total.skips += shard.skips;
        total.stops += shard.stops;
        total.cpu += shard.cpu;
    }
    total.failed.sort();
    let runs = total.runs.max(1);
    println!(
        "{} seeds, tight pools {tight}, limited links {limited}: {:.0} seeds per second on one core ({:.2} s of cpu, \
         {:.2} s of wall on {workers} workers), {} steps, {:.1} seeks and {:.1} skips per seed, \
         {} runs stopped",
        total.runs,
        total.runs as f64 / total.cpu.as_secs_f64(),
        total.cpu.as_secs_f64(),
        wall.elapsed().as_secs_f64(),
        total.steps / runs as usize,
        total.seeks as f64 / runs as f64,
        total.skips as f64 / runs as f64,
        total.stops,
    );
    for (seed, e) in total.failed.iter().take(10) {
        println!("seed {seed}: {e}");
    }
    total
}

#[test]
fn commands_at_random_steps_keep_every_invariant() {
    let range = seeds();
    let whole = range.end - range.start >= COMMAND_SEEDS;
    let total = sweep(range, PLAIN);
    let failed: Vec<u64> = total.failed.iter().map(|(seed, _)| *seed).collect();
    assert!(failed.is_empty(), "{} seeds failed, {failed:?}", failed.len());
    assert!(!whole || (total.stops > 0 && total.seeks > total.runs as usize), "stop and seek ran");
    assert!(!whole || total.skips > total.runs as usize, "and so did skip");
}

#[test]
fn commands_keep_every_invariant_with_a_pool_smaller_than_a_ring() {
    let total = sweep(seeds(), Setup { tight: true, ..PLAIN });
    let failed: Vec<u64> = total.failed.iter().map(|(seed, _)| *seed).collect();
    assert!(failed.is_empty(), "{} seeds failed, {failed:?}", failed.len());
}

#[test]
fn commands_keep_every_invariant_with_links_limited_in_time() {
    let total = sweep(seeds(), Setup { limited: true, ..PLAIN });
    let failed: Vec<u64> = total.failed.iter().map(|(seed, _)| *seed).collect();
    assert!(failed.is_empty(), "{} seeds failed, {failed:?}", failed.len());
    let tight = sweep(seeds(), Setup { limited: true, tight: true, ..PLAIN });
    let failed: Vec<u64> = tight.failed.iter().map(|(seed, _)| *seed).collect();
    assert!(failed.is_empty(), "{} seeds failed with tight pools, {failed:?}", failed.len());
}

#[test]
fn a_revived_source_with_one_consumer_gone_does_not_starve_the_other() {
    let mut rig = Rig::new(2);
    let src = rig.add(&SRC2, Role::Source { count: 8 });
    let gone = rig.add(&SINK, Role::Sink { timed: false });
    let left = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), gone.sink(IN));
    rig.link(src.src(OUT_B), left.sink(IN));
    let mut run = rig.step();
    for _ in 0..200 {
        for g in 0..2 {
            if run.ready(g) {
                run.pass(g).expect("pass");
            }
        }
        if rig.rows(left).len() < 8 {
            run.pass(2).expect("pass");
        }
    }
    assert!(matches!(run.outcome(1), Some(Outcome::Idle { .. })), "{}", stall_report(&run));
    assert_eq!((rig.count(run.elements(1)[0], What::Eos), rig.rows(left).len()), (1, 8));
    assert!(matches!(run.outcome(0), Some(Outcome::Idle { .. })), "resting past its end");
    run.control().append(seek_to(0)).expect("room in the log");
    for _ in 0..200 {
        for g in 0..3 {
            if run.ready(g) {
                run.pass(g).expect("pass");
            }
        }
    }
    assert!(all_gone(&run), "{}", stall_report(&run));
    assert!(run.control().is_finished());
    run.finish().expect("finish");
    for (stream, sink) in [gone, left].into_iter().enumerate() {
        check_complete(&rig.tape(), sink, stream as u64, &[0, 0], 8).expect("the seek played");
    }
    assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
}

struct Lossless {
    rig: Rig,
    tee_sinks: [ElementId; 2],
    merged: ElementId,
    elements: u32,
}

const LOSSLESS: u64 = 40;

fn lossless(pool_slots: u32) -> Lossless {
    let mut rig = Rig::new(pool_slots);
    let src = rig.add(&SRC, Role::Source { count: LOSSLESS });
    let fwd = rig.add(&FWD, Role::Forward);
    let tee = rig.add(&TEE, Role::Forward);
    rig.p.set_element_pool(tee, 64, 3);
    let a = rig.add(&SINK, Role::Sink { timed: false });
    let b = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), fwd.sink(IN));
    rig.link(fwd.src(OUT), tee.sink(IN));
    rig.link(tee.src(OUT), a.sink(IN));
    rig.link(tee.src(OUT_B), b.sink(IN));
    let src2 = rig.add(&SRC2, Role::Source { count: LOSSLESS });
    let left = rig.add(&FWD, Role::Forward);
    let inline = rig.add(&INLINE, Role::Forward);
    let merge = rig.add(&MERGE, Role::Merge);
    let merged = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src2.src(OUT), left.sink(IN));
    rig.link(left.src(OUT), inline.sink(IN));
    rig.link(inline.src(OUT), merge.sink(IN));
    rig.link(src2.src(OUT_B), merge.sink(IN_B));
    rig.link(merge.src(OUT), merged.sink(IN));
    Lossless { rig, tee_sinks: [a, b], merged, elements: 10 }
}

type Delivered = Vec<(u64, Timestamp, u32, u64)>;

fn delivered(rows: &[Row], stream: u64) -> Delivered {
    let of = |r: &&Row| r.stream == stream;
    rows.iter().filter(of).map(|r| (r.seq, r.pts, r.flags, r.made_in)).collect()
}

#[test]
fn every_interleaving_delivers_the_same_buffers_in_the_same_order() {
    let mut reference: Option<Vec<Delivered>> = None;
    for seed in scaled(300) {
        let pool_slots = [5, 9, 64][(seed % 3) as usize];
        let Lossless { mut rig, tee_sinks, merged, elements } = lossless(pool_slots);
        let mut run = rig.step();
        assert_eq!(run.groups(), 9);
        let mut driver = Loop::new(seed);
        driver.spurious = if seed % 2 == 0 { 0 } else { 7 };
        let mut app = App { starts: vec![0], ..App::default() };
        driver
            .run(&mut run, &rig.clock, &mut app, no_check)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert!(all_gone(&run), "seed {seed} stalled\n{}", stall_report(&run));
        run.finish().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let tape = rig.tape();
        check_lifecycle(&tape, elements).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let mut got = Vec::new();
        for sink in tee_sinks {
            let rows = rig.rows(sink);
            assert_eq!(rows.len() as u64, LOSSLESS, "seed {seed}");
            got.push(delivered(&rows, 0));
        }
        let rows = rig.rows(merged);
        assert_eq!(rows.len() as u64, 2 * LOSSLESS, "seed {seed}");
        got.push(delivered(&rows, 0));
        got.push(delivered(&rows, 1));
        for sink in tee_sinks.into_iter().chain([merged]) {
            assert_eq!(rig.count(sink, What::Eos), 1, "seed {seed}, end of stream exactly once");
            let streams = if sink == merged { 2 } else { 1 };
            for stream in 0..streams {
                check_complete(&tape, sink, stream, &[0], LOSSLESS)
                    .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            }
        }
        assert_eq!(rig.p.tap_handle().pool().outstanding, 0, "seed {seed}");
        match &reference {
            None => reference = Some(got),
            Some(want) => assert!(*want == got, "seed {seed} delivered something else"),
        }
    }
}

#[test]
fn the_threaded_driver_delivers_what_the_stepped_driver_does() {
    let got = |stepped: bool| {
        let Lossless { mut rig, tee_sinks, merged, elements } = lossless(9);
        if stepped {
            let mut run = rig.step();
            let mut app = App { starts: vec![0], ..App::default() };
            Loop::new(1).run(&mut run, &rig.clock, &mut app, no_check).expect("runs");
            run.finish().expect("finish");
        } else {
            rig.p.run().expect("run");
        }
        check_lifecycle(&rig.tape(), elements).expect("lifecycle");
        let both = rig.rows(merged);
        let tee = tee_sinks.map(|sink| delivered(&rig.rows(sink), 0));
        (tee, delivered(&both, 0), delivered(&both, 1), rig.p.tap_handle().pool().outstanding)
    };
    let (stepped, threaded) = (got(true), got(false));
    assert_eq!(stepped.0[0].len() as u64, LOSSLESS);
    assert!(stepped == threaded, "the drivers disagree");
}

#[test]
fn an_hour_of_timed_media_takes_no_wall_time_and_lands_on_every_deadline() {
    const EVERY: Timestamp = Timestamp::from_millis(40);
    const HOUR: u64 = 3600 * 1_000_000_000 / EVERY.0;
    let mut rig = Rig::new(16);
    rig.every = EVERY;
    let src = rig.add(&SRC, Role::Source { count: HOUR });
    let fwd = rig.add(&FWD, Role::Forward);
    let sink = rig.add(&SINK, Role::Sink { timed: true });
    rig.link(src.src(OUT), fwd.sink(IN));
    rig.link(fwd.src(OUT), sink.sink(IN));
    let start = rig.clock.now();
    let wall = Instant::now();
    let mut run = rig.step();
    let mut driver = Loop::new(7);
    (driver.limit, driver.keep_trace) = (8_000_000, false);
    let mut app = App { starts: vec![0], ..App::default() };
    driver.run(&mut run, &rig.clock, &mut app, no_check).expect("an hour runs to its end");
    assert!(all_gone(&run), "{}", stall_report(&run));
    let stats = run.control().scheduler_stats();
    run.finish().expect("finish");
    let wall = wall.elapsed();

    let rows = rig.rows(sink);
    assert_eq!(rows.len() as u64, HOUR);
    for (k, row) in rows.iter().enumerate() {
        let pts = Timestamp(k as u64 * EVERY.0);
        assert_eq!((row.pts, row.running), (pts, pts), "buffer {k} is taken at its deadline");
        assert_eq!(row.at, Timestamp(start.0 + pts.0), "buffer {k} on the clock");
    }
    let last = Timestamp((HOUR - 1) * EVERY.0);
    assert_eq!(rig.clock.now(), Timestamp(start.0 + last.0), "the clock stops at the last one");
    assert_eq!(last.0 + EVERY.0, Timestamp::from_secs(3600).0, "that was an hour of media");
    assert_eq!(driver.jumps, HOUR - 1, "one jump per deadline and no other");
    assert_eq!((stats.parks, stats.parked_ns, stats.tick_expiries), (0, 0, 0), "nothing slept");
    println!("an hour of media, {} passes and {} jumps, in {wall:?}", driver.passes, driver.jumps);
    assert!(wall.as_secs() < 60, "an hour of virtual media took {wall:?}");
}

#[test]
fn a_sink_that_never_consumes_is_a_stall_with_names() {
    let mut rig = Rig::new(64);
    let src = rig.add(&SRC, Role::Source { count: 1000 });
    let fwd = rig.add(&FWD, Role::Forward);
    let stuck = rig.add(&SINK, Role::Stuck);
    rig.link(src.src(OUT), fwd.sink(IN));
    rig.link(fwd.src(OUT), stuck.sink(IN));
    let mut run = rig.step();
    let mut driver = Loop::new(3);
    let mut app = App { starts: vec![0], ..App::default() };
    driver.run(&mut run, &rig.clock, &mut app, no_check).expect("a stall is quiescence");
    assert!(!all_gone(&run) && run.next_deadline().is_none());
    assert!((0..3).all(|g| !run.ready(g)), "{}", stall_report(&run));
    let ring = Outcome::Blocked { on: BlockedOn::Ring { pad: OUT } };
    assert_eq!(run.outcome(0), Some(ring), "the source waits on its full ring");
    assert_eq!(run.outcome(1), Some(ring), "and so does the transform");
    assert!(matches!(run.outcome(2), Some(Outcome::Idle { .. })), "the sink asks for nothing");
    assert!(run.held(2) > 0, "while it holds {} buffers", run.held(2));
    assert_eq!(rig.clock.now(), Timestamp::from_secs(5), "and no deadline moved the clock");
    assert_eq!((run.elements(0), run.elements(2)), (&[src][..], &[stuck][..]));
    run.finish().expect("finish");
    for el in [src, fwd, stuck] {
        assert_eq!(rig.marks(el), [What::Start, What::Stop]);
    }
    let stops: Vec<u32> = rig
        .tape()
        .iter()
        .filter_map(|r| match r {
            Rec::Mark { element, what: What::Stop, .. } => Some(*element),
            _ => None,
        })
        .collect();
    assert_eq!(stops, [stuck.0, fwd.0, src.0], "the last group stops first");
    assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
}

#[test]
fn an_exhausted_pool_is_a_stall_that_names_the_starved_element() {
    let mut rig = Rig::new(4);
    let src = rig.add(&SRC, Role::Source { count: 1000 });
    let hoard = rig.add(&SINK, Role::Hoard);
    rig.link(src.src(OUT), hoard.sink(IN));
    let mut run = rig.step();
    let mut driver = Loop::new(11);
    let mut app = App { starts: vec![0], ..App::default() };
    driver.run(&mut run, &rig.clock, &mut app, no_check).expect("a stall is quiescence");
    let pool = Outcome::Blocked { on: BlockedOn::Pool { element: src } };
    assert_eq!(run.outcome(0), Some(pool), "{}", stall_report(&run));
    assert!(matches!(run.outcome(1), Some(Outcome::Idle { .. })));
    assert_eq!(rig.p.tap_handle().pool().outstanding, 4);
    run.control().append(Command::Stop).expect("room in the log");
    assert!(run.ready(0) && run.ready(1));
    driver.run(&mut run, &rig.clock, &mut app, no_check).expect("stops");
    assert!(all_gone(&run));
    run.finish().expect("finish");
    assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
}

#[test]
fn an_element_error_comes_back_from_the_pass_and_stops_the_rest() {
    let mut rig = Rig::new(64);
    let src = rig.add(&SRC, Role::Source { count: 1000 });
    let bad = rig.add(&FWD, Role::FailAt(5));
    let sink = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), bad.sink(IN));
    rig.link(bad.src(OUT), sink.sink(IN));
    let far = rig.add(&SRC, Role::Source { count: 1000 });
    let stuck = rig.add(&SINK, Role::Stuck);
    rig.link(far.src(OUT), stuck.sink(IN));
    let mut run = rig.step();
    let (failing, apart) = (group_of(&run, bad), [group_of(&run, far), group_of(&run, stuck)]);
    for _ in 0..100 {
        let Some(g) = apart.into_iter().find(|g| run.ready(*g)) else {
            break;
        };
        run.pass(g).expect("pass");
    }
    assert!(!run.ready(apart[0]) && !run.ready(apart[1]), "{}", stall_report(&run));
    let mut driver = Loop::new(5);
    let mut app = App { starts: vec![0], ..App::default() };
    let err = driver.run(&mut run, &rig.clock, &mut app, no_check).expect_err("the pass fails");
    assert!(err.contains(&format!("pass of group {failing}")), "{err}");
    assert_eq!(run.outcome(failing), Some(Outcome::Exit), "the failed group is torn down");
    assert_eq!(rig.marks(bad).last(), Some(&What::Stop));
    for g in apart {
        assert!(run.ready(g), "the failure woke group {g}");
        assert_eq!(run.pass(g).expect("a stopped group exits"), Outcome::Exit);
    }
    let failed = run.finish().expect_err("the run failed");
    assert!(matches!(failed, Error::ElementFailed { element, .. } if element == bad), "{failed}");
    for el in [src, bad, sink, far, stuck] {
        assert_eq!((rig.count(el, What::Start), rig.count(el, What::Stop)), (1, 1));
    }
    let on_bus: Vec<BusMessage> = std::iter::from_fn(|| rig.p.bus().try_recv()).collect();
    assert!(matches!(on_bus[..], [BusMessage::Error { .. }]), "{} messages", on_bus.len());
    assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
}

#[test]
fn a_failed_group_on_threads_ends_no_stream_for_its_neighbours() {
    for round in 0..20 {
        let panics = round % 2 == 1;
        let mut rig = Rig::new(64);
        let src = rig.add(&SRC, Role::Source { count: 1000 });
        let bad = rig.add(&FWD, if panics { Role::PanicAt(5) } else { Role::FailAt(5) });
        let sink = rig.add(&SINK, Role::Sink { timed: false });
        rig.link(src.src(OUT), bad.sink(IN));
        rig.link(bad.src(OUT), sink.sink(IN));
        match rig.p.run().expect_err("the run failed") {
            Error::GroupPanicked { .. } => assert!(panics),
            Error::ElementFailed { element, .. } => assert!(!panics && element == bad),
            other => panic!("{other}"),
        }
        for el in [src, sink] {
            assert_eq!((rig.count(el, What::Start), rig.count(el, What::Stop)), (1, 1));
            assert_eq!(rig.count(el, What::Eos), 0, "a failure is not an end of stream");
        }
        assert!(!rig.p.control().is_finished(), "and not the finish either");
        assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
    }
}

#[test]
fn a_failed_start_ends_the_run_before_its_first_pass() {
    let mut rig = Rig::new(64);
    let src = rig.add(&SRC, Role::Source { count: 10 });
    let bad = rig.add(&FWD, Role::FailStart);
    let sink = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), bad.sink(IN));
    rig.link(bad.src(OUT), sink.sink(IN));
    let Err(err) = rig.p.step(Arc::new(rig.clock.clone())) else { panic!("the start fails") };
    assert!(matches!(err, Error::ElementFailed { element, .. } if element == bad), "{err}");
    assert_eq!(rig.marks(src), [What::Start, What::Stop], "the group before it is stopped");
    assert!(rig.marks(bad).is_empty() && rig.marks(sink).is_empty());
    for k in 0..200 {
        let cmd = if k % 2 == 0 { Command::Pause } else { Command::Resume };
        assert!(rig.p.control().append(cmd).is_ok(), "command {k}");
    }
}

#[test]
fn a_run_dropped_without_finish_still_stops_its_elements() {
    let mut rig = Rig::new(64);
    let src = rig.add(&SRC, Role::Source { count: 10 });
    let sink = rig.add(&SINK, Role::Stuck);
    rig.link(src.src(OUT), sink.sink(IN));
    let mut run = rig.step();
    run.pass(0).expect("pass");
    drop(run);
    assert_eq!(rig.marks(src), [What::Start, What::Stop]);
    assert_eq!(rig.marks(sink), [What::Start, What::Stop]);
    assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
}

#[test]
#[should_panic(expected = "the test element panicked")]
fn a_panicking_element_is_not_caught() {
    let mut rig = Rig::new(64);
    let src = rig.add(&SRC, Role::Source { count: 10 });
    let bad = rig.add(&FWD, Role::PanicAt(1));
    let sink = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), bad.sink(IN));
    rig.link(bad.src(OUT), sink.sink(IN));
    let mut run = rig.step();
    let mut driver = Loop::new(1);
    let mut app = App { starts: vec![0], ..App::default() };
    let _ = driver.run(&mut run, &rig.clock, &mut app, no_check);
}

#[test]
fn readiness_follows_signals_and_deadlines_and_nothing_else() {
    let mut rig = Rig::new(64);
    let src = rig.add(&SRC, Role::Source { count: 3 });
    let sink = rig.add(&SINK, Role::Sink { timed: true });
    rig.link(src.src(OUT), sink.sink(IN));
    let mut run = rig.step();
    let base = rig.clock.now();
    let at = |running: Timestamp| Some(Timestamp(base.0 + running.0));
    let pass = |run: &mut Stepped, g| run.pass(g).expect("pass");
    assert!(run.ready(0) && run.ready(1), "a group that has not run is ready");
    assert_eq!((run.outcome(0), run.next_deadline()), (None, None));
    assert!(matches!(pass(&mut run, 1), Outcome::Idle { until: Timestamp::NONE, .. }));
    assert!(!run.ready(1));
    for _ in 0..3 {
        assert_eq!(pass(&mut run, 0), Outcome::Progressed);
    }
    assert!(run.ready(1), "the push signaled the sink");
    assert_eq!(pass(&mut run, 1), Outcome::Progressed);
    assert_eq!(pass(&mut run, 1), Outcome::Idle { until: STEP, within: Duration::MAX });
    assert_eq!((run.ready(1), run.next_deadline()), (false, at(STEP)));
    rig.clock.advance(Timestamp(STEP.0 - 1));
    assert!(!run.ready(1), "one nanosecond early is early");
    rig.clock.advance(Timestamp(1));
    assert!(run.ready(1) && run.next_deadline().is_none(), "a ready group has no deadline ahead");
    assert_eq!(pass(&mut run, 1), Outcome::Progressed);
    let third = Timestamp(2 * STEP.0);
    assert_eq!(pass(&mut run, 1), Outcome::Idle { until: third, within: Duration::MAX });
    run.control().append(Command::Pause).expect("room in the log");
    assert!(run.ready(1));
    assert!(matches!(pass(&mut run, 1), Outcome::Idle { until: Timestamp::NONE, .. }));
    rig.clock.advance(Timestamp::from_secs(1));
    assert_eq!((run.ready(1), run.next_deadline()), (false, None));
    run.control().append(Command::Resume).expect("room in the log");
    assert!(run.ready(1));
    assert_eq!(pass(&mut run, 1), Outcome::Idle { until: third, within: Duration::MAX });
    let late = Timestamp(third.0 + Timestamp::from_secs(1).0);
    assert_eq!((run.ready(1), run.next_deadline()), (false, at(late)));
    rig.clock.advance(STEP);
    assert_eq!(pass(&mut run, 1), Outcome::Progressed);
    for _ in 0..8 {
        for g in 0..2 {
            if run.ready(g) {
                pass(&mut run, g);
            }
        }
    }
    assert!(all_gone(&run), "{}", stall_report(&run));
    assert_eq!(pass(&mut run, 1), Outcome::Exit, "a pass of a group that is gone");
    run.finish().expect("finish");
    let taken: Vec<Timestamp> = rig.rows(sink).iter().map(|r| r.running).collect();
    assert_eq!(taken, [Timestamp::ZERO, STEP, third], "each at its deadline");
    assert_eq!(rig.count(src, What::Paused), 1, "a group applies a command when it next runs");
}

#[test]
fn an_interval_wake_is_measured_on_the_clock() {
    let every = Timestamp::from_millis(15);
    let mut rig = Rig::new(64);
    let pump = lone(&mut rig, Role::Pump(Duration::from_millis(15)));
    let mut run = rig.step();
    let start = rig.clock.now();
    let idle = Outcome::Idle { until: Timestamp::NONE, within: Duration::from_millis(15) };
    assert_eq!(run.pass(0).expect("pass"), idle);
    assert_eq!((run.ready(0), run.next_deadline()), (false, Some(Timestamp(start.0 + every.0))));
    rig.clock.advance(Timestamp(every.0 - 1));
    assert!(!run.ready(0));
    rig.clock.advance(Timestamp(1));
    assert!(run.ready(0), "the interval ran out on the clock");
    assert_eq!(run.pass(0).expect("pass"), idle);
    assert_eq!(run.next_deadline(), Some(Timestamp(start.0 + 2 * every.0)), "from the last pass");
    assert_eq!(rig.count(pump, What::Tick), 2);
    run.finish().expect("finish");
}

#[test]
fn a_deadline_that_stays_ripe_is_served_once_per_signal() {
    let mut rig = Rig::new(64);
    let el = lone(&mut rig, Role::WakePast);
    let mut run = rig.step();
    let idle = Outcome::Idle { until: Timestamp::ZERO, within: Duration::MAX };
    assert_eq!(run.pass(0).expect("pass"), idle);
    assert!(run.ready(0), "the deadline is ripe");
    assert_eq!(run.pass(0).expect("pass"), idle);
    assert_eq!((run.ready(0), run.next_deadline()), (false, None));
    rig.clock.advance(Timestamp::from_secs(1));
    assert!(!run.ready(0));
    run.control().append(Command::Pause).expect("room in the log");
    assert!(run.ready(0));
    assert_eq!(run.pass(0).expect("pass"), idle);
    assert!(run.ready(0), "the deadline gets its one pass again");
    assert_eq!(run.pass(0).expect("pass"), idle);
    assert!(!run.ready(0));
    assert_eq!(rig.count(el, What::Tick), 4);
    run.finish().expect("finish");
}

fn drain(run: &mut Stepped) {
    for _ in 0..10_000 {
        let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
            return;
        };
        run.pass(g).expect("pass");
    }
    panic!("never came to rest\n{}", stall_report(run));
}

fn play_out(run: &mut Stepped, clock: &MockClock) {
    for _ in 0..10_000 {
        drain(run);
        let Some(to) = run.next_deadline() else {
            return;
        };
        clock.advance(Timestamp(to.0 - clock.now().0));
    }
    panic!("never came to rest\n{}", stall_report(run));
}

fn at(rig: &Rig, el: ElementId, what: What) -> usize {
    let is = |r: &Rec| match r {
        Rec::Mark { element, what: w, .. } => *element == el.0 && *w == what,
        Rec::Row(_) | Rec::Dropped(_) => false,
    };
    rig.tape().iter().position(is).unwrap_or_else(|| panic!("element {} had no {what:?}", el.0))
}

struct Uneven {
    rig: Rig,
    short: ElementId,
    long: ElementId,
    sources: [ElementId; 2],
}

const SHORT: u64 = 2;
const LONG: u64 = 6;

fn uneven() -> (Uneven, Stepped) {
    let mut rig = Rig::new(16);
    let a = rig.add(&SRC, Role::Source { count: SHORT });
    let short = rig.add(&SINK, Role::Sink { timed: false });
    let b = rig.add(&SRC, Role::Source { count: LONG });
    let long = rig.add(&SINK, Role::Sink { timed: true });
    rig.link(a.src(OUT), short.sink(IN));
    rig.link(b.src(OUT), long.sink(IN));
    let mut run = rig.step();
    drain(&mut run);
    let resting = matches!(run.outcome(group_of(&run, short)), Some(Outcome::Idle { .. }));
    assert!(resting && !all_gone(&run), "{}", stall_report(&run));
    assert_eq!(rig.marks(short), [What::Start, What::Eos], "ended, and not stopped");
    assert_eq!((rig.rows(short).len() as u64, rig.rows(long).len()), (SHORT, 1));
    assert!(!run.control().is_finished(), "one sink of two has ended");
    (Uneven { rig, short, long, sources: [a, b] }, run)
}

#[test]
fn two_sinks_that_end_at_different_times_both_rest_and_the_second_ends_the_run() {
    let (Uneven { rig, short, long, sources }, mut run) = uneven();
    for _ in 0..3 {
        assert!(matches!(run.pass(group_of(&run, short)), Ok(Outcome::Idle { .. })));
    }
    assert!(!run.control().is_finished());
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    assert_eq!(rig.rows(long).len() as u64, LONG);
    for el in [short, long].into_iter().chain(sources) {
        assert_eq!(rig.marks(el), [What::Start, What::Eos, What::Stop]);
        let (stop, end) = (at(&rig, el, What::Stop), at(&rig, long, What::Eos));
        assert!(stop > end, "element {} was stopped before the run was over", el.0);
    }
    let on_bus: Vec<BusMessage> = std::iter::from_fn(|| rig.p.bus().try_recv()).collect();
    assert!(matches!(on_bus[..], [BusMessage::Eos]), "{} messages", on_bus.len());
}

#[test]
fn a_seek_between_two_endings_revives_both_sinks_and_both_end_again() {
    let (Uneven { rig, short, long, sources }, mut run) = uneven();
    run.control().append(seek_to(0)).expect("the run is not over");
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    let tape = rig.tape();
    check_complete(&tape, short, 0, &[0, 0], SHORT).expect("the short sink played again");
    check_complete(&tape, long, 0, &[0, 0], LONG).expect("the long sink played from the target");
    assert_eq!(rig.marks(short), [What::Start, What::Eos, What::Flush, What::Eos, What::Stop]);
    assert_eq!(rig.marks(long), [What::Start, What::Flush, What::Eos, What::Stop]);
    for src in sources {
        assert_eq!(rig.count(src, What::Flush), 1);
    }
}

#[test]
fn a_sink_that_slept_through_the_seek_holds_the_finish_until_it_has_played_it() {
    let (Uneven { rig, short, long, .. }, mut run) = uneven();
    run.control().append(seek_to(0)).expect("the run is not over");
    let asleep = group_of(&run, short);
    for _ in 0..10_000 {
        if let Some(g) = (0..run.groups()).find(|g| *g != asleep && run.ready(*g)) {
            run.pass(g).expect("pass");
        } else if let Some(to) = run.next_deadline() {
            rig.clock.advance(Timestamp(to.0 - rig.clock.now().0));
        } else {
            break;
        }
    }
    assert_eq!(rig.marks(long), [What::Start, What::Flush, What::Eos], "played the seek out");
    assert_eq!(rig.marks(short), [What::Start, What::Eos], "and this one has not flushed");
    assert!(!run.control().is_finished() && !all_gone(&run), "{}", stall_report(&run));
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    check_complete(&rig.tape(), short, 0, &[0, 0], SHORT).expect("the short sink played again");
}

fn at_the_last_ask(count: u64) -> (Rig, ElementId, Stepped, usize) {
    let mut rig = Rig::new(16);
    let src = rig.add(&SRC, Role::Source { count });
    let sink = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), sink.sink(IN));
    let mut run = rig.step();
    let g = group_of(&run, sink);
    for _ in 0..200 {
        if rig.count(sink, What::Eos) == 1 {
            break;
        }
        let ready = (0..run.groups()).find(|g| run.ready(*g)).expect("a group to run");
        run.pass(ready).expect("pass");
    }
    assert_eq!(run.pass(g).expect("pass"), Outcome::Progressed);
    assert!(!run.control().is_finished(), "ended, and the decision is the next pass");
    (rig, sink, run, g)
}

#[test]
fn a_seek_appended_before_the_last_sink_asks_wins_and_the_run_goes_on() {
    let (rig, sink, mut run, g) = at_the_last_ask(4);
    run.control().append(seek_to(1)).expect("the seek is first");
    assert!(matches!(run.pass(g).expect("pass"), Outcome::Idle { .. }));
    assert!(!run.control().is_finished(), "the sink flushed for the seek and did not ask");
    assert_eq!(rig.marks(sink), [What::Start, What::Eos, What::Flush]);
    drain(&mut run);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    check_complete(&rig.tape(), sink, 0, &[0, 1], 4).expect("the seek played");
    assert_eq!(rig.count(sink, What::Eos), 2, "once per generation");
}

#[test]
fn an_append_after_the_finish_is_refused_as_finished() {
    let (rig, sink, mut run, g) = at_the_last_ask(4);
    assert!(matches!(run.pass(g).expect("pass"), Outcome::Idle { .. }));
    assert!(run.control().is_finished(), "the last sink asked first");
    assert!((0..run.groups()).all(|g| run.ready(g)), "the finish woke every group");
    let head = [seek_to(0), Command::Pause, Command::Resume, Command::Stop];
    for cmd in head {
        assert_eq!(run.control().append(cmd), Err(CommandError::Finished), "{cmd:?}");
    }
    rig.p.control().pause();
    let _ = rig
        .p
        .control()
        .seek_bytes(Timestamp::ZERO, &[(profluens_core::command::ALL, 0, Timestamp::ZERO)]);
    rig.p.control().stop();
    assert!(!rig.p.control().is_paused() && rig.p.control().is_stopped());
    drain(&mut run);
    assert!(all_gone(&run));
    run.finish().expect("finish");
    assert_eq!(rig.marks(sink), [What::Start, What::Eos, What::Stop], "no flush, the seek lost");
    assert_eq!(rig.rows(sink).len(), 4);
}

#[test]
fn a_paused_run_does_not_end_at_a_timed_sink_and_ends_after_the_resume() {
    let mut rig = Rig::new(16);
    let src = rig.add(&SRC, Role::Source { count: 3 });
    let sink = rig.add(&SINK, Role::Sink { timed: true });
    rig.link(src.src(OUT), sink.sink(IN));
    let mut run = rig.step();
    drain(&mut run);
    assert_eq!(rig.rows(sink).len(), 1, "the first buffer is due at once");
    run.control().append(Command::Pause).expect("room in the log");
    play_out(&mut run, &rig.clock);
    rig.clock.advance(Timestamp::from_secs(60));
    play_out(&mut run, &rig.clock);
    assert_eq!((rig.rows(sink).len(), rig.count(sink, What::Eos)), (1, 0));
    assert!(!run.control().is_finished() && !all_gone(&run));
    assert_eq!(rig.marks(src), [What::Start, What::Eos, What::Paused], "the source rests, paused");
    run.control().append(Command::Resume).expect("room in the log");
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    let taken: Vec<Timestamp> = rig.rows(sink).iter().map(|r| r.running).collect();
    assert_eq!(taken, [Timestamp::ZERO, STEP, Timestamp(2 * STEP.0)], "each at its deadline");
}

#[test]
fn a_sink_at_rest_applies_a_pause_and_a_resume_and_the_run_still_ends() {
    let (Uneven { rig, short, long, .. }, mut run) = uneven();
    run.control().append(Command::Pause).expect("room in the log");
    play_out(&mut run, &rig.clock);
    assert_eq!(rig.marks(short), [What::Start, What::Eos, What::Paused]);
    assert!(!run.control().is_finished(), "the long sink is paused short of its end");
    run.control().append(Command::Resume).expect("room in the log");
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    let marks = [What::Start, What::Eos, What::Paused, What::Resumed, What::Stop];
    assert_eq!(rig.marks(short), marks);
    assert_eq!(rig.rows(long).len() as u64, LONG);
}

#[test]
fn a_transform_with_no_linked_output_is_a_terminal_group_and_ends_the_run() {
    let mut rig = Rig::new(4);
    let src = rig.add(&SRC, Role::Source { count: 5 });
    let fwd = rig.add(&FWD, Role::Forward);
    rig.link(src.src(OUT), fwd.sink(IN));
    let mut run = rig.step();
    drain(&mut run);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    assert_eq!(rig.marks(fwd), [What::Start, What::Eos, What::Stop]);
    assert_eq!(rig.p.counters(fwd).drops, 5);
}

const SKIPPED: u64 = 40;

fn skip_rig(sink_role: Role) -> (Rig, [ElementId; 3]) {
    let mut rig = Rig::new(16);
    let src = rig.add(&SRC, Role::Source { count: SKIPPED });
    let fwd = rig.add(&FWD, Role::Forward);
    let sink = rig.add(&SINK, sink_role);
    rig.link(src.src(OUT), fwd.sink(IN));
    rig.link(fwd.src(OUT), sink.sink(IN));
    (rig, [src, fwd, sink])
}

fn play_until(run: &mut Stepped, rig: &Rig, sink: ElementId, rows: usize) {
    for _ in 0..100_000 {
        if rig.rows(sink).len() >= rows {
            return;
        }
        if let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) {
            run.pass(g).expect("pass");
        } else if let Some(to) = run.next_deadline() {
            rig.clock.advance(Timestamp(to.0 - rig.clock.now().0));
        } else {
            panic!("at rest with {} rows\n{}", rig.rows(sink).len(), stall_report(run));
        }
    }
    panic!("never took {rows} rows");
}

fn seqs(rows: &[Row]) -> Vec<u64> {
    rows.iter().map(|r| r.seq).collect()
}

#[test]
fn a_skip_presents_nothing_before_the_point_and_everything_from_it_once() {
    let half = Timestamp(STEP.0 / 2);
    let on = |seq: u64| Timestamp(seq * STEP.0);
    let between = |seq: u64| Timestamp(seq * STEP.0 - half.0);
    let targets = [on(6), between(8), on(13), between(21), on(39), on(40), on(45), on(2)];
    for to_time in targets {
        let (mut rig, [src, fwd, sink]) = skip_rig(Role::Sink { timed: true });
        let mut run = rig.step();
        play_until(&mut run, &rig, sink, 5);
        let waiting: usize = (0..run.groups()).map(|g| run.held(g)).sum();
        assert!(waiting >= 8, "the rings hold buffers the skip has to sort, {waiting}");
        run.control().append(Command::SkipTo { to_time }).expect("the run is on");
        play_out(&mut run, &rig.clock);
        assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
        run.finish().expect("finish");

        let first = seq_at(to_time).max(5);
        let want: Vec<u64> = (0..5).chain(first..SKIPPED).collect();
        let rows = rig.rows(sink);
        assert_eq!(seqs(&rows), want, "skip to {to_time:?}");
        for row in &rows {
            assert_eq!(row.running, row.pts, "taken off its deadline, {row:?}");
            assert_eq!((row.made_in, row.taken_in), (0, 0), "a skip is no generation");
        }
        let dropped = first.min(SKIPPED) - 5;
        assert_eq!(rig.p.counters(sink).drops, dropped, "skip to {to_time:?}, each drop counted");
        assert_eq!(rig.p.counters(sink).buffers_in, SKIPPED, "and all of them reached the sink");
        for el in [src, fwd, sink] {
            let marks = [What::Start, What::Skip(to_time), What::Eos, What::Stop];
            assert_eq!(rig.marks(el), marks, "skip to {to_time:?}");
        }
        assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
    }
}

#[test]
fn a_skip_at_any_step_loses_nothing_at_or_after_the_point() {
    for seed in scaled(400) {
        let mut rng = Rng(seed);
        let to_time = Timestamp(rng.below(SKIPPED + 2) * STEP.0 + rng.below(2) * STEP.0 / 2);
        let (mut rig, [_, _, sink]) = skip_rig(Role::Sink { timed: true });
        let mut run = rig.step();
        let script = vec![(rng.below(120) as u32, Act::Skip(to_time))];
        let mut app = App { script, starts: vec![0], ..App::default() };
        let mut driver = Loop::new(rng.next());
        driver.spurious = if seed % 2 == 0 { 0 } else { 5 };
        let mut oracle = Oracle::new(&[sink]);
        let each_pass = |_: &Stepped, app: &App| oracle.sweep(&rig.tape, app);
        driver
            .run(&mut run, &rig.clock, &mut app, each_pass)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert!(all_gone(&run), "seed {seed} stalled\n{}", stall_report(&run));
        run.finish().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        check_complete(&rig.tape(), sink, 0, &[0], SKIPPED)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let (rows, drops) = (rig.rows(sink).len() as u64, rig.p.counters(sink).drops);
        assert_eq!(rows + drops, SKIPPED, "seed {seed}");
        assert_eq!(rig.count(sink, What::Skip(to_time)), app.skips.len(), "seed {seed}");
        assert_eq!(rig.p.tap_handle().pool().outstanding, 0, "seed {seed}");
    }
}

#[test]
fn a_skip_while_paused_presents_exactly_the_frame_at_the_point() {
    let skip = Command::SkipTo { to_time: Timestamp(20 * STEP.0) };
    for (jump, flushes) in [(skip, 0), (seek_to(20), 1)] {
        let (mut rig, [_, _, sink]) = skip_rig(Role::Screen);
        let mut run = rig.step();
        play_until(&mut run, &rig, sink, 3);
        run.control().append(Command::Pause).expect("room in the log");
        play_out(&mut run, &rig.clock);
        assert_eq!(seqs(&rig.rows(sink)), [0, 1, 2], "paused, and nothing more is due");
        rig.clock.advance(Timestamp::from_secs(7));
        run.control().append(jump).expect("room in the log");
        play_out(&mut run, &rig.clock);
        rig.clock.advance(Timestamp::from_secs(60));
        play_out(&mut run, &rig.clock);
        let rows = rig.rows(sink);
        assert_eq!(seqs(&rows), [0, 1, 2, 20], "{jump:?}: the frame at the point, and one only");
        assert_eq!(rows[3].running, rows[3].pts, "running time stands on the point");
        assert_eq!(rig.count(sink, What::Flush), flushes);
        assert!(!run.control().is_finished(), "{}", stall_report(&run));

        run.control().append(Command::Resume).expect("room in the log");
        play_out(&mut run, &rig.clock);
        assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
        run.finish().expect("finish");
        let rows = rig.rows(sink);
        assert_eq!(seqs(&rows), (0..3).chain(20..SKIPPED).collect::<Vec<_>>());
        for row in &rows {
            assert_eq!(row.running, row.pts, "{jump:?}: the paused time is no running time");
        }
        assert_eq!(rig.count(sink, What::Eos), 1);
    }
}

#[test]
fn a_skip_past_the_end_ends_every_sink_once_and_the_run_with_them() {
    let (Uneven { rig, short, long, sources }, mut run) = uneven();
    let beyond = Timestamp::from_secs(3600);
    run.control().append(Command::SkipTo { to_time: beyond }).expect("the run is not over");
    let resting = group_of(&run, short);
    assert!(run.ready(resting), "the skip woke the sink at rest");
    assert!(matches!(run.pass(resting), Ok(Outcome::Idle { .. })));
    assert_eq!(rig.marks(short), [What::Start, What::Eos, What::Skip(beyond)]);
    assert!(!run.control().is_finished(), "the other sink has not ended");
    drain(&mut run);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    assert_eq!(run.next_deadline(), None, "nothing was waited for");
    run.finish().expect("finish");
    assert_eq!(rig.marks(short), [What::Start, What::Eos, What::Skip(beyond), What::Stop]);
    assert_eq!(rig.marks(long), [What::Start, What::Skip(beyond), What::Eos, What::Stop]);
    assert_eq!((rig.rows(long).len(), rig.p.counters(long).drops), (1, LONG - 1));
    for src in sources {
        assert_eq!(rig.count(src, What::Eos), 1, "no source was revived");
    }
    let on_bus: Vec<BusMessage> = std::iter::from_fn(|| rig.p.bus().try_recv()).collect();
    assert!(matches!(on_bus[..], [BusMessage::Eos]), "{} messages", on_bus.len());
}

/// A group applies the last jump of what it drains, so it was told only the second skip.
#[test]
fn skips_drained_together_reach_a_group_as_the_last_of_them() {
    let (mut rig, [src, fwd, sink]) = skip_rig(Role::Sink { timed: false });
    let mut run = rig.step();
    let [up, mid, down] = [src, fwd, sink].map(|el| group_of(&run, el));
    assert!(up != mid && mid != down, "three groups");
    let (far, back) = (Timestamp(30 * STEP.0), Timestamp(10 * STEP.0));
    run.control().append(Command::SkipTo { to_time: far }).expect("room in the log");
    run.pass(mid).expect("pass");
    run.control().append(Command::SkipTo { to_time: back }).expect("room in the log");
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    let told = |to: &[Timestamp]| {
        let skips = to.iter().map(|t| What::Skip(*t));
        [What::Start].into_iter().chain(skips).chain([What::Eos, What::Stop]).collect::<Vec<_>>()
    };
    assert_eq!(rig.marks(fwd), told(&[far, back]), "it ran between the two");
    assert_eq!(rig.marks(sink), told(&[back]), "it drained both in one pass");
    assert_eq!(seqs(&rig.rows(sink)), (10..SKIPPED).collect::<Vec<_>>());
    assert_eq!(rig.p.counters(sink).drops, 10, "judged by the point it was told");
}

#[test]
fn a_skip_back_behind_a_seek_plays_on_from_the_seek() {
    let (mut rig, [_, _, sink]) = skip_rig(Role::Sink { timed: false });
    let mut run = rig.step();
    play_until(&mut run, &rig, sink, 3);
    run.control().append(seek_to(30)).expect("room in the log");
    run.control()
        .append(Command::SkipTo { to_time: Timestamp(10 * STEP.0) })
        .expect("room in the log");
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    let after: Vec<u64> =
        rig.rows(sink).iter().filter(|r| r.taken_in == 1).map(|r| r.seq).collect();
    assert_eq!(after, (30..SKIPPED).collect::<Vec<_>>(), "from the seek's point");
    assert_eq!(rig.marks(sink)[1..3], [What::Flush, What::Skip(Timestamp(10 * STEP.0))]);
}

/// A group learns a point at the top of a pass, so one begun before a skip takes its input by the
/// old point.
#[test]
fn a_pass_begun_before_a_skip_takes_the_end_of_stream_ahead_of_it() {
    let (rig, _, sink, mut run, [up, down]) = chain(4);
    play_until(&mut run, &rig, sink, 4);
    for _ in 0..16 {
        if run.ready(up) {
            run.pass(up).expect("pass");
        }
    }
    assert_eq!(rig.marks(sink), [What::Start], "the end is in its input");
    assert_eq!(run.pass_to(down, Cut::Drained).expect("the top"), None);
    let beyond = Timestamp::from_secs(3600);
    run.control().append(Command::SkipTo { to_time: beyond }).expect("room in the log");
    run.pass(down).expect("the rest");
    assert_eq!(rig.marks(sink), [What::Start, What::Eos], "not told the skip yet");
    assert_eq!(seqs(&rig.rows(sink)), [0, 1, 2, 3], "judged by the old point");
    drain(&mut run);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    assert_eq!(
        rig.marks(sink),
        [What::Start, What::Eos, What::Skip(beyond), What::Stop],
        "ended once"
    );
}

#[test]
fn a_skip_appended_before_the_last_sink_asks_does_not_hold_the_finish() {
    let (rig, sink, mut run, g) = at_the_last_ask(4);
    let to_time = Timestamp(2 * STEP.0);
    run.control().append(Command::SkipTo { to_time }).expect("the skip is first");
    assert!(matches!(run.pass(g).expect("pass"), Outcome::Idle { .. }));
    assert!(run.control().is_finished(), "the sink applied the skip and asked");
    assert_eq!(rig.marks(sink), [What::Start, What::Eos, What::Skip(to_time)]);
    drain(&mut run);
    assert!(all_gone(&run));
    run.finish().expect("finish");
    assert_eq!((rig.rows(sink).len(), rig.count(sink, What::Eos)), (4, 1));
}

#[test]
fn the_seeds_that_found_the_revived_source_stall_pass() {
    for seed in [2104, 3829, 8238, 9215] {
        let as_found = Setup { skips: false, ..PLAIN };
        commanded_run(seed, as_found).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    }
}

#[test]
fn a_signal_from_inside_the_pass_makes_the_group_ready_again() {
    let mut rig = Rig::new(64);
    let el = lone(&mut rig, Role::Meddle);
    let mut run = rig.step();
    assert!(matches!(run.pass(0).expect("pass"), Outcome::Idle { .. }));
    assert_eq!(rig.marks(el), [What::Start, What::Tick]);
    assert!(run.ready(0), "the epoch was sampled before the pass, so the signal counts");
    run.pass(0).expect("pass");
    assert_eq!(rig.marks(el), [What::Start, What::Tick, What::Paused, What::Tick]);
    assert!(!run.ready(0));
    run.finish().expect("finish");
}

#[derive(Default)]
struct SlowReactor {
    flying: Vec<(u32, Submission)>,
}

impl Reactor for SlowReactor {
    fn submit(&mut self, subs: &mut Vec<Submission>, _names: &[u8]) {
        self.flying.extend(subs.drain(..).map(|s| (3, s)));
    }

    fn due_in(&self) -> Option<Duration> {
        (!self.flying.is_empty()).then_some(Duration::ZERO)
    }

    fn is_idle(&self) -> bool {
        self.flying.is_empty()
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        for (left, _) in &mut self.flying {
            *left -= 1;
        }
        while self.flying.first().is_some_and(|(left, _)| *left == 0) {
            let (_, s) = self.flying.remove(0);
            let result = IoResult::Ok(s.buf.as_ref().map_or(0, |b| b.memory.len()));
            out.push((s.element, Completion { op: s.op, user: s.user, result, buf: s.buf }));
        }
    }
}

#[test]
fn io_in_flight_is_ready_at_once_and_settled_at_teardown() {
    let mut rig = Rig::new(64);
    rig.p
        .set_reactor_factory(Arc::new(|| Ok(Box::new(SlowReactor::default()) as Box<dyn Reactor>)));
    let src = rig.add(&SRC, Role::Source { count: 6 });
    let scribe = rig.add(&SINK, Role::Scribe);
    rig.link(src.src(OUT), scribe.sink(IN));
    let mut run = rig.step();
    let g = group_of(&run, scribe);
    let mut driver = Loop::new(9);
    let mut app = App { starts: vec![0], ..App::default() };
    let waiting = Outcome::Idle { until: Timestamp::NONE, within: Duration::ZERO };
    let until_blocked = |run: &Stepped, _: &App| match run.outcome(g) == Some(waiting) {
        true => Err("blocked on io".to_string()),
        false => Ok(()),
    };
    let stopped = driver.run(&mut run, &rig.clock, &mut app, until_blocked).expect_err("io");
    assert!(stopped.ends_with("blocked on io"), "{stopped}");
    assert!(run.ready(g), "a deadline of zero is ripe, as the threaded driver retries");
    let until_eos = |_: &Stepped, _: &App| match rig.count(scribe, What::Eos) {
        0 => Ok(()),
        _ => Err("at its end".to_string()),
    };
    let stopped = driver.run(&mut run, &rig.clock, &mut app, until_eos).expect_err("eos");
    assert!(stopped.ends_with("at its end"), "{stopped}");
    run.control().append(Command::Stop).expect("room in the log");
    assert_eq!(run.pass(g).expect("pass"), Outcome::Exit);
    let marks = rig.marks(scribe);
    let (asked, answered) = (rig.count(scribe, What::Asked), rig.count(scribe, What::Tick));
    assert_eq!((asked, answered), (7, 7), "six buffers and the last word, {marks:?}");
    assert_eq!(marks[marks.len() - 4..], [What::Eos, What::Asked, What::Tick, What::Stop]);
    run.finish().expect("finish");
    assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
}

fn chain(count: u64) -> (Rig, ElementId, ElementId, Stepped, [usize; 2]) {
    let mut rig = Rig::new(16);
    let src = rig.add(&SRC, Role::Source { count });
    let sink = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), sink.sink(IN));
    let run = rig.step();
    let groups = [group_of(&run, src), group_of(&run, sink)];
    (rig, src, sink, run, groups)
}

#[test]
fn a_pass_cut_with_nothing_in_the_gap_is_the_same_pass() {
    for seed in scaled(100) {
        let run = |halves| {
            commanded_run(seed, Setup { tracing: true, tight: seed % 2 == 0, halves, ..PLAIN })
                .unwrap_or_else(|e| panic!("seed {seed}, halves {halves}: {e}"))
        };
        assert!(run(false) == run(true), "seed {seed} ran another way in halves");
    }
}

#[test]
fn a_batch_of_a_seek_appended_past_the_drain_is_taken_after_the_flush() {
    let (rig, _, sink, mut run, [up, down]) = chain(8);
    run.pass(up).expect("pass");
    run.pass(down).expect("pass");
    assert_eq!(rig.rows(sink).len(), 1);
    assert_eq!(run.pass_to(down, Cut::Drained).expect("the top"), None);
    run.control().append(seek_to(5)).expect("room in the log");
    assert_eq!(run.pass(up).expect("pass"), Outcome::Progressed);
    run.pass(down).expect("the rest");
    let rows = rig.rows(sink);
    let got: Vec<(u64, u64, u64)> = rows.iter().map(|r| (r.seq, r.made_in, r.taken_in)).collect();
    assert_eq!(got, [(0, 0, 0), (5, 1, 1)], "the sink was in the buffer's generation");
    let tape = rig.tape();
    let flushed = at(&rig, sink, What::Flush);
    assert!(matches!(tape[flushed + 1], Rec::Row(row) if row.seq == 5), "flush, then the buffer");
    drain(&mut run);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    check_complete(&rig.tape(), sink, 0, &[0, 5], 8).expect("the seek played");
    assert_eq!(rig.count(sink, What::Flush), 1, "once for the one seek");
}

#[test]
fn a_group_at_a_cut_is_ready_and_a_signal_in_the_gap_brings_it_back() {
    let (rig, _, sink, mut run, [_, down]) = chain(2);
    assert_eq!(run.pass_to(down, Cut::Drained).expect("the top"), None);
    assert!(run.ready(down), "a thread inside its pass waits for nothing");
    assert_eq!(run.outcome(down), None, "the outcome is the last whole pass's");
    run.control().append(Command::Pause).expect("room in the log");
    assert!(matches!(run.pass(down).expect("the rest"), Outcome::Idle { .. }));
    assert_eq!(rig.marks(sink), [What::Start], "the next drain applies it");
    assert!(run.ready(down), "the epoch was sampled before the top, so the append shows");
    assert!(matches!(run.pass(down).expect("pass"), Outcome::Idle { .. }));
    assert_eq!(rig.marks(sink), [What::Start, What::Paused]);
    assert!(!run.ready(down));
    assert_eq!(run.pass_to(down, Cut::Drained).expect("the top"), None);
    assert!(run.ready(down) && run.next_deadline().is_none());
    assert!(matches!(run.pass_to(down, Cut::Drained), Ok(Some(Outcome::Idle { .. }))));
    assert!(!run.ready(down));
}

#[test]
fn a_stop_ends_a_pass_at_its_top_and_a_pass_already_past_it_runs_out() {
    let (rig, src, sink, mut run, [up, down]) = chain(8);
    assert_eq!(run.pass_to(up, Cut::Drained).expect("the top"), None);
    run.control().append(Command::Stop).expect("room in the log");
    assert_eq!(run.pass_to(down, Cut::Drained).expect("the top"), Some(Outcome::Exit));
    assert_eq!(run.outcome(down), Some(Outcome::Exit));
    assert_eq!(rig.marks(sink), [What::Start, What::Stop], "torn down before any cut");
    assert!(!run.ready(down));
    assert_eq!(run.pass(up).expect("the rest"), Outcome::Progressed);
    assert_eq!(rig.marks(src), [What::Start]);
    assert_eq!(run.pass(up).expect("pass"), Outcome::Exit);
    run.finish().expect("finish");
    assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
    assert!(rig.rows(sink).is_empty());
}

#[test]
fn a_seek_appended_past_the_drain_of_the_last_sink_wins_over_its_ask() {
    let (rig, sink, mut run, g) = at_the_last_ask(4);
    assert_eq!(run.pass_to(g, Cut::Drained).expect("the top"), None);
    run.control().append(seek_to(1)).expect("the seek is first");
    assert!(matches!(run.pass(g).expect("the rest"), Outcome::Idle { .. }));
    assert!(!run.control().is_finished(), "the sink asked and was behind the newest seek");
    assert_eq!(rig.marks(sink), [What::Start, What::Eos], "and it has not flushed yet");
    drain(&mut run);
    assert!(all_gone(&run) && run.control().is_finished(), "{}", stall_report(&run));
    run.finish().expect("finish");
    check_complete(&rig.tape(), sink, 0, &[0, 1], 4).expect("the seek played");
}

#[test]
fn a_run_made_from_explicit_configuration_is_quiet() {
    let Commanded { mut rig, elements, .. } = commanded(16);
    let mut run = rig.step();
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run), "{}", stall_report(&run));
    run.finish().expect("finish");
    let tap = rig.p.tap_handle();
    for el in 0..elements {
        let (process, queue, _) = tap.latency(ElementId(el)).expect("element");
        assert_eq!((process.count, queue.count), (0, 0), "tracing is off");
    }
}

#[test]
fn the_environment_does_not_reach_a_run_made_from_explicit_configuration() {
    let quiet = "stepped::a_run_made_from_explicit_configuration_is_quiet";
    let child = std::process::Command::new(std::env::current_exe().expect("this binary"))
        .args(["--exact", quiet, "--nocapture"])
        .env("PROFLUENS_DEBUG", "trace")
        .output()
        .expect("the child ran");
    let (out, err) =
        (String::from_utf8_lossy(&child.stdout), String::from_utf8_lossy(&child.stderr));
    assert!(child.status.success() && out.contains("1 passed"), "{out}\n{err}");
    assert!(!err.contains("pipeline") && !err.contains("group"), "the child logged\n{err}");
}

fn scaled(n: u64) -> std::ops::Range<u64> {
    let scale = std::env::var("PF_SEED_SCALE").ok().and_then(|s| s.parse::<u64>().ok());
    0..n * scale.unwrap_or(1).max(1)
}

const LOW_MARK: Timestamp = Timestamp::from_millis(40);
const HIGH_MARK: Timestamp = Timestamp::from_millis(100);

fn marked(count: u64, timed: bool) -> (Rig, ElementId, Stepped, [usize; 2]) {
    let mut rig = Rig::new(32);
    let src = rig.add(&SRC, Role::Source { count });
    let sink = rig.add(&SINK, Role::Sink { timed });
    rig.link(src.src(OUT), sink.sink(IN));
    rig.p.set_queue_capacity(sink, 16);
    rig.p.set_link_marks(sink, Limit::time(LOW_MARK), Limit::time(HIGH_MARK));
    let run = rig.step();
    let groups = [group_of(&run, src), group_of(&run, sink)];
    (rig, sink, run, groups)
}

fn crossings(rig: &Rig, sink: ElementId) -> Vec<(Fill, Timestamp)> {
    let mut said = Vec::new();
    while let Some(msg) = rig.p.bus().try_recv() {
        if let BusMessage::Buffering { element, pad, fill, held } = msg {
            assert_eq!((element, pad), (sink, Some(IN)), "named by the consumer and its pad");
            said.push((fill, held.time));
        }
    }
    said
}

fn passes(run: &mut Stepped, group: usize, n: usize) {
    (0..n).for_each(|_| _ = run.pass(group).expect("pass"));
}

const fn ms(n: u64) -> Timestamp {
    Timestamp::from_millis(n)
}

#[test]
fn a_link_with_marks_says_each_crossing_once_and_its_end_reads_high() {
    let (rig, sink, mut run, [up, down]) = marked(12, true);
    passes(&mut run, up, 5);
    assert_eq!(crossings(&rig, sink), [], "under the high mark it says nothing going up");
    run.pass(up).expect("pass");
    assert_eq!(crossings(&rig, sink), [(Fill::High, ms(100))], "the feed that reached it");
    run.pass(up).expect("pass");
    run.pass(down).expect("pass");
    for _ in 0..2 {
        rig.clock.advance(STEP);
        run.pass(down).expect("pass");
    }
    assert_eq!(crossings(&rig, sink), [], "between the marks it stays high going down");
    rig.clock.advance(STEP);
    run.pass(down).expect("pass");
    assert_eq!(crossings(&rig, sink), [(Fill::Low, ms(40))], "the take that reached the low mark");
    passes(&mut run, up, 2);
    assert_eq!(crossings(&rig, sink), [], "between the marks it stays low going up");
    run.pass(up).expect("pass");
    assert_eq!(crossings(&rig, sink), [(Fill::High, ms(100))]);
    rig.clock.advance(Timestamp::from_secs(1));
    run.pass(down).expect("pass");
    assert_eq!(crossings(&rig, sink), [(Fill::Low, ms(0))]);
    passes(&mut run, up, 3);
    assert_eq!(crossings(&rig, sink), [], "the last two buffers, under the high mark");
    run.pass(up).expect("the end of stream");
    assert_eq!(crossings(&rig, sink), [(Fill::High, ms(40))], "nothing more comes, so it is high");
    play_out(&mut run, &rig.clock);
    assert!(all_gone(&run), "{}", stall_report(&run));
    assert_eq!(crossings(&rig, sink), [], "the consumer took it all and the end is not waited at");
    assert_eq!(rig.rows(sink).len(), 12);
}

#[test]
fn a_seek_says_no_crossing_between_its_flushes_and_the_new_stretch_is_judged() {
    for consumer_first in [true, false] {
        let (rig, sink, mut run, [up, down]) = marked(40, false);
        passes(&mut run, up, 6);
        assert_eq!(crossings(&rig, sink), [(Fill::High, ms(100))]);
        run.control().append(seek_to(20)).expect("room in the log");
        if consumer_first {
            run.pass(down).expect("pass");
            assert_eq!(rig.count(sink, What::Flush), 1);
        }
        passes(&mut run, up, 6);
        assert_eq!(
            crossings(&rig, sink),
            [],
            "consumer first {consumer_first}, the flush is no crossing"
        );
        run.pass(down).expect("pass");
        assert_eq!(rig.count(sink, What::Flush), 1);
        assert_eq!(crossings(&rig, sink), [(Fill::Low, ms(0))], "it took all of the new stretch");
        passes(&mut run, up, 5);
        assert_eq!(
            crossings(&rig, sink),
            [(Fill::High, ms(100))],
            "from where the new stretch was taken to"
        );
    }
}

#[test]
fn a_low_link_is_said_high_once_the_new_stretch_fills_before_its_consumer_flushes() {
    let (rig, sink, mut run, [up, down]) = marked(40, false);
    passes(&mut run, up, 6);
    run.pass(down).expect("pass");
    assert_eq!(crossings(&rig, sink), [(Fill::High, ms(100)), (Fill::Low, ms(0))]);
    run.control().append(seek_to(20)).expect("room in the log");
    passes(&mut run, up, 6);
    assert_eq!(rig.count(sink, What::Flush), 0, "the consumer is behind the seek");
    assert_eq!(
        crossings(&rig, sink),
        [(Fill::High, ms(100))],
        "a player paused on the low mark plays"
    );
    run.pass(down).expect("pass");
    assert_eq!(crossings(&rig, sink), [(Fill::Low, ms(0))]);
}

#[test]
fn a_link_with_no_marks_says_nothing_and_judging_allocates_nothing() {
    let run = |marks: Option<(Limit, Limit)>| {
        let mut rig = Rig::new(16);
        let src = rig.add(&SRC, Role::Source { count: 64 });
        let sink = rig.add(&SINK, Role::Sink { timed: false });
        rig.link(src.src(OUT), sink.sink(IN));
        rig.p.set_queue_capacity(sink, 16);
        if let Some((low, high)) = marks {
            rig.p.set_link_marks(sink, low, high);
        }
        let mut run = rig.step();
        let [up, down] = [group_of(&run, src), group_of(&run, sink)];
        let (mut allocs, mut fills) = (0, 0);
        for cycle in 0..8 {
            let before = crate::link_leak::alloc_calls();
            passes(&mut run, up, 6);
            run.pass(down).expect("pass");
            allocs += if cycle > 0 { crate::link_leak::alloc_calls() - before } else { 0 };
            fills += crossings(&rig, sink).len();
        }
        let during = fills;
        play_out(&mut run, &rig.clock);
        assert!(all_gone(&run), "{}", stall_report(&run));
        (allocs, during, fills + crossings(&rig, sink).len())
    };
    let (plain, _, fills) = run(None);
    assert_eq!(fills, 0, "a link with no marks says nothing");
    let (judged, fills, _) = run(Some((Limit::time(LOW_MARK), Limit::time(HIGH_MARK))));
    assert_eq!(fills, 16, "high and low each cycle");
    assert_eq!(judged, plain, "judging at every feed and take allocates nothing");
}

fn told(rig: &Rig, el: ElementId) -> Vec<i64> {
    rig.marks(el)
        .into_iter()
        .filter_map(|w| if let What::Told(l) = w { Some(l) } else { None })
        .collect()
}

fn heard(rig: &Rig, el: ElementId) -> Vec<Option<i64>> {
    rig.marks(el)
        .into_iter()
        .filter_map(|w| if let What::Heard(l) = w { Some(l) } else { None })
        .collect()
}

fn late_chain(pass: &'static ElementDesc) -> (Rig, [ElementId; 3]) {
    let mut rig = Rig::new(3);
    let src = rig.add(&SRC, Role::Source { count: 60 });
    let fwd = rig.add(pass, Role::Forward);
    let sink = rig.add(&SINK, Role::Late);
    rig.link(src.src(OUT), fwd.sink(IN));
    rig.link(fwd.src(OUT), sink.sink(IN));
    (rig, [src, fwd, sink])
}

#[test]
fn feedback_reaches_the_source_through_a_passthrough_and_the_latest_wins() {
    for (pass, groups) in [(&FWD, 3), (&INLINE, 2)] {
        for seed in scaled(40) {
            let (mut rig, [src, fwd, _]) = late_chain(pass);
            let mut run = rig.step();
            assert_eq!(run.groups(), groups);
            let mut app = App { starts: vec![0], ..App::default() };
            Loop::new(seed).run(&mut run, &rig.clock, &mut app, no_check).expect("the run ends");
            run.finish().expect("no failure");
            for (el, values) in [(src, told(&rig, src)), (fwd, told(&rig, fwd))] {
                assert!(!values.is_empty(), "seed {seed}: element {} was told nothing", el.0);
                assert!(
                    values.windows(2).all(|w| w[0] < w[1]),
                    "seed {seed}: went back {values:?}"
                );
                assert!(values.iter().all(|l| (1..=60).contains(l)), "seed {seed}: {values:?}");
            }
            let read = heard(&rig, src);
            assert!(
                read.iter().flatten().count() > 0 && read.windows(2).all(|w| w[0] < w[1]),
                "seed {seed}: {read:?}"
            );
        }
    }
}

#[test]
fn an_element_of_two_src_pads_forwards_nothing_by_itself() {
    let mut rig = Rig::new(3);
    let src = rig.add(&SRC, Role::Source { count: 30 });
    let tee = rig.add(&TEE, Role::Forward);
    let a = rig.add(&SINK, Role::Late);
    let b = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), tee.sink(IN));
    rig.link(tee.src(OUT), a.sink(IN));
    rig.link(tee.src(OUT_B), b.sink(IN));
    let mut run = rig.step();
    let mut app = App { starts: vec![0], ..App::default() };
    Loop::new(3).run(&mut run, &rig.clock, &mut app, no_check).expect("the run ends");
    run.finish().expect("no failure");
    assert!(!told(&rig, tee).is_empty(), "the tee is told on its pad");
    assert!(
        told(&rig, src).is_empty() && heard(&rig, src).is_empty(),
        "core holds no merge policy"
    );
}

#[test]
fn feedback_never_crosses_a_seek() {
    for seed in scaled(60) {
        let pass = if seed % 2 == 0 { &FWD } else { &INLINE };
        let (mut rig, [src, fwd, sink]) = late_chain(pass);
        let mut run = rig.step();
        let mut app = App { starts: vec![0], ..App::default() };
        let mut rng = Rng(seed);
        app.script = (0..3)
            .map(|k| (20 + 40 * k + rng.below(30) as u32, Act::Seek(rng.below(40))))
            .collect();
        let mut driver = Loop::new(seed);
        driver.halves = seed % 3 == 0;
        driver
            .run(&mut run, &rig.clock, &mut app, no_check)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        run.finish().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        for el in [src, fwd] {
            let tape = rig.tape();
            for rec in &tape {
                let Rec::Mark { element, what, seek_gen } = *rec else {
                    continue;
                };
                let said = match what {
                    What::Told(l) | What::Heard(Some(l)) if element == el.0 => l,
                    _ => continue,
                };
                assert_eq!(
                    (said as u64) >> 32,
                    seek_gen,
                    "seed {seed}: element {} read {what:?} of another seek",
                    el.0
                );
            }
        }
        check_complete(&rig.tape(), sink, 0, &app.starts, 60)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    }
}

#[test]
fn a_link_cut_by_a_seal_takes_its_feedback_with_it() {
    let mut rig = Rig::new(3);
    let src = rig.add(&SRC, Role::Source { count: 400 });
    let bad = rig.add(&FWD, Role::FailBranchAt(30));
    let sink = rig.add(&SINK, Role::Late);
    rig.link(src.src(OUT), bad.sink(IN));
    rig.link(bad.src(OUT), sink.sink(IN));
    let mut run = rig.step();
    let mut app = App { starts: vec![0], ..App::default() };
    Loop::new(9).run(&mut run, &rig.clock, &mut app, no_check).expect("the run ends");
    run.finish().expect("a sealed branch fails no run");
    let read = heard(&rig, src);
    assert!(read.iter().flatten().count() > 0, "{read:?}");
    assert_eq!(read.last(), Some(&None), "the slot went with its link");
}

const PLAYED: u64 = 80;

fn branches(role: Role) -> (Rig, [ElementId; 3]) {
    let mut rig = Rig::new(8);
    let src = rig.add(&SRC, Role::Source { count: PLAYED });
    let tee = rig.add(&TEE, Role::Forward);
    let main = rig.add(&SINK, Role::Sink { timed: false });
    let bad = rig.add(&FWD, role);
    let side = rig.add(&SINK, Role::Sink { timed: false });
    rig.link(src.src(OUT), tee.sink(IN));
    rig.link(tee.src(OUT), main.sink(IN));
    rig.link(tee.src(OUT_B), bad.sink(IN));
    rig.link(bad.src(OUT), side.sink(IN));
    (rig, [main, bad, side])
}

fn sealed(rig: &Rig) -> Vec<Error> {
    let on_bus: Vec<BusMessage> = std::iter::from_fn(|| rig.p.bus().try_recv()).collect();
    assert!(!on_bus.iter().any(|m| matches!(m, BusMessage::Error { .. })), "an error on the bus");
    on_bus
        .into_iter()
        .filter_map(
            |m| {
                if let BusMessage::BranchSealed { error, .. } = m { Some(error) } else { None }
            },
        )
        .collect()
}

#[test]
fn a_branch_failure_seals_its_group_and_the_main_branch_plays_to_the_end() {
    for seed in scaled(60) {
        let (mut rig, [main, bad, side]) = branches(Role::FailBranchAt(5));
        let mut run = rig.step();
        let mut app = App { starts: vec![0], ..App::default() };
        let mut driver = Loop::new(seed);
        (driver.halves, driver.spurious) = (seed % 2 == 0, if seed % 3 == 0 { 5 } else { 0 });
        driver
            .run(&mut run, &rig.clock, &mut app, no_check)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert!(all_gone(&run), "seed {seed}\n{}", stall_report(&run));
        run.finish().unwrap_or_else(|e| panic!("seed {seed}: a sealed branch failed the run, {e}"));
        assert!(rig.p.control().is_finished(), "seed {seed}: the run finished on its own");
        assert_eq!(rig.rows(main).len() as u64, PLAYED, "seed {seed}");
        assert!(rig.rows(side).len() <= 4, "seed {seed}: what passed before the failure");
        assert_eq!(rig.count(side, What::Eos), 1, "seed {seed}: the branch ends, it does not hang");
        assert_eq!(rig.marks(bad).last(), Some(&What::Stop), "seed {seed}");
        let errors = sealed(&rig);
        assert!(
            matches!(errors[..], [Error::ElementFailed { element, scope: Scope::Branch, .. }] if element == bad),
            "seed {seed}"
        );
    }
}

#[test]
fn a_junction_behind_a_sealed_branch_mixes_on_with_one_input() {
    for seed in scaled(40) {
        let mut rig = Rig::new(8);
        let (a, b) = (
            rig.add(&SRC, Role::Source { count: PLAYED }),
            rig.add(&SRC, Role::Source { count: PLAYED }),
        );
        let bad = rig.add(&FWD, Role::FailBranchAt(7));
        let merge = rig.add(&MERGE, Role::Merge);
        let sink = rig.add(&SINK, Role::Sink { timed: false });
        rig.link(a.src(OUT), merge.sink(IN));
        rig.link(b.src(OUT), bad.sink(IN));
        rig.link(bad.src(OUT), merge.sink(IN_B));
        rig.link(merge.src(OUT), sink.sink(IN));
        let mut run = rig.step();
        let mut app = App { starts: vec![0], ..App::default() };
        Loop::new(seed)
            .run(&mut run, &rig.clock, &mut app, no_check)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        run.finish().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let rows = rig.rows(sink).len() as u64;
        assert!(
            (PLAYED..=PLAYED + 6).contains(&rows),
            "seed {seed}: all of one input and what the other passed, {rows}"
        );
        assert_eq!(
            (rig.count(merge, What::Eos), rig.count(sink, What::Eos)),
            (1, 1),
            "seed {seed}"
        );
        assert_eq!(sealed(&rig).len(), 1, "seed {seed}");
    }
}

#[test]
fn a_seek_behind_a_sealed_branch_ends_it_again_and_plays_the_rest() {
    for seed in scaled(40) {
        let (mut rig, [main, _, side]) = branches(Role::FailBranchAt(5));
        let mut run = rig.step();
        let mut app = App {
            starts: vec![0],
            script: vec![(60 + (seed % 40) as u32, Act::Seek(10))],
            ..App::default()
        };
        Loop::new(seed)
            .run(&mut run, &rig.clock, &mut app, no_check)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        run.finish().unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let tape = rig.tape();
        check_complete(&tape, main, 0, &app.starts, PLAYED)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let last = app.starts.len() as u64 - 1;
        let ends = |el: ElementId| {
            tape.iter().filter(|r| matches!(r, Rec::Mark { element, what: What::Eos, seek_gen } if *element == el.0 && *seek_gen == last)).count()
        };
        assert_eq!((ends(main), ends(side)), (1, 1), "seed {seed}");
    }
}

#[test]
fn a_failure_of_pipeline_scope_still_fails_the_run() {
    let (mut rig, [_, bad, _]) = branches(Role::FailAt(5));
    let mut run = rig.step();
    let mut app = App { starts: vec![0], ..App::default() };
    let err =
        Loop::new(1).run(&mut run, &rig.clock, &mut app, no_check).expect_err("the pass fails");
    assert!(err.contains("the test element broke"), "{err}");
    let failed = run.finish().expect_err("the run failed");
    assert!(
        matches!(failed, Error::ElementFailed { element, scope: Scope::Pipeline, .. } if element == bad)
    );
}

#[test]
fn a_branch_failure_on_threads_seals_and_the_run_finishes() {
    for _ in 0..20 {
        let (mut rig, [main, _, side]) = branches(Role::FailBranchAt(5));
        rig.p.run().expect("a sealed branch fails no run");
        assert_eq!(rig.rows(main).len() as u64, PLAYED);
        assert!(rig.rows(side).len() <= 4);
        assert_eq!(rig.count(side, What::Eos), 1);
        assert_eq!(sealed(&rig).len(), 1);
        assert_eq!(rig.p.tap_handle().pool().outstanding, 0);
    }
}
