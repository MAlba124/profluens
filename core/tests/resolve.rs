//! The re-solve of a live change against the pinned running edges.

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::{Error, LinkError, SolveError};
use profluens_core::event::Event;
use profluens_core::format::{
    Constraint, ConstraintDesc, ConstraintSpec, FieldDesc, FieldSpec, FixedFormat, OfferDesc,
    Value, ValueDesc, ValueSpec,
};
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_core::topology::Leave;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

type Menu = &'static [OfferDesc];

fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

fn rates(vs: &[i64]) -> Menu {
    let allowed = ConstraintDesc::Set(leak(vs.iter().map(|v| ValueDesc::Int(*v)).collect()));
    leak(vec![OfferDesc {
        family: "audio/raw",
        fields: leak(vec![FieldDesc { field: "rate", allowed, preferred: None }]),
    }])
}

fn any_rate() -> Menu {
    leak(vec![OfferDesc {
        family: "audio/raw",
        fields: leak(vec![FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Any,
            preferred: None,
        }]),
    }])
}

static WILD: [OfferDesc; 1] = [OfferDesc::wildcard()];
static RATE_PROP: [PropDesc; 1] =
    [PropDesc { name: "rate", allowed: Constraint::Any, live: false }];

/// What a node saw of its formats, the rate alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Seen {
    Start(Option<i64>),
    Format(Option<i64>),
    Out(Option<i64>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Source(u32),
    Stall(u32),
    Mute,
    Bad,
    Forward,
    Sink,
}

type Tape = Arc<Mutex<Vec<(u32, Seen)>>>;

struct Node {
    desc: &'static ElementDesc,
    role: Role,
    tape: Tape,
}

#[derive(Debug)]
struct Boom;

impl std::fmt::Display for Boom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("boom")
    }
}

impl std::error::Error for Boom {}

fn rate_of(ctx: &Ctx, f: Option<&FixedFormat>) -> Option<i64> {
    match f?.get(ctx.field_id("rate")?)? {
        Value::Int(r) => Some(r),
        _ => None,
    }
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.role == Role::Bad {
            return Err(Error::element_failed(ctx.element(), Boom));
        }
        if self.role == Role::Sink {
            let rate = rate_of(ctx, ctx.negotiated_sink(IN));
            self.tape.lock().unwrap().push((ctx.element().0, Seen::Start(rate)));
        }
        if self.role == Role::Forward {
            let rate = rate_of(ctx, ctx.negotiated_src(OUT));
            self.tape.lock().unwrap().push((ctx.element().0, Seen::Out(rate)));
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        match &mut self.role {
            Role::Source(0) => return Ok(Flow::Eos),
            Role::Stall(0) => {}
            Role::Stall(n) => {
                let Some(mut buf) = ctx.try_alloc() else {
                    return Ok(Flow::Ok);
                };
                buf.memory.set_len(8);
                *n -= 1;
                ctx.out(OUT).push(buf);
            }
            Role::Source(n) => {
                let Some(mut buf) = ctx.try_alloc() else {
                    return Ok(Flow::Ok);
                };
                buf.memory.set_len(8);
                *n -= 1;
                ctx.out(OUT).push(buf);
            }
            Role::Mute | Role::Bad => {}
            Role::Forward => {
                while let Some(buf) = inputs.pop() {
                    ctx.out(OUT).push(buf);
                }
            }
            Role::Sink => while inputs.pop().is_some() {},
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            let rate = rate_of(ctx, Some(f));
            self.tape.lock().unwrap().push((ctx.element().0, Seen::Format(rate)));
        }
        Ok(())
    }
}

struct Rig {
    p: Pipeline,
    tape: Tape,
    clock: MockClock,
}

impl Rig {
    fn new() -> Rig {
        let clock = MockClock::starting_at(Timestamp::from_secs(5));
        let mut p = Pipeline::with(
            profluens_core::log::DebugSpec::parse(""),
            false,
            Arc::new(clock.clone()),
        );
        p.set_pool(64, 24);
        Rig { p, tape: Tape::default(), clock }
    }

    fn add(
        &mut self,
        name: &'static str,
        (sinks, srcs): (&[Menu], &[Menu]),
        couplings: &[Coupling],
        role: Role,
    ) -> ElementId {
        self.add_with(name, (sinks, srcs), couplings, role, &[])
    }

    fn add_with(
        &mut self,
        name: &'static str,
        (sinks, srcs): (&[Menu], &[Menu]),
        couplings: &[Coupling],
        role: Role,
        props: &'static [PropDesc],
    ) -> ElementId {
        let names = ["sink", "sink_1", "src", "src_1"];
        let pads = |menus: &[Menu], at: usize| {
            leak(
                menus
                    .iter()
                    .enumerate()
                    .map(|(i, m)| PadDesc { name: names[at + i], offers: m })
                    .collect(),
            )
        };
        let desc = Box::leak(Box::new(ElementDesc {
            name,
            sinks: pads(sinks, 0),
            srcs: pads(srcs, 2),
            props,
            sched: SchedHint::Active,
            inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: leak(couplings.to_vec()),
        }));
        self.p.add(Node { desc, role, tape: Arc::clone(&self.tape) })
    }

    fn add_desc(&mut self, desc: &'static ElementDesc, role: Role) -> ElementId {
        self.p.add(Node { desc, role, tape: Arc::clone(&self.tape) })
    }

    fn step(&mut self) -> Stepped {
        self.p.step(Arc::new(self.clock.clone())).expect("step")
    }

    fn run(&self, run: &mut Stepped, passes: Option<u32>) -> bool {
        for k in 0..passes.unwrap_or(1_000_000) {
            run.service();
            let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
            if ready.is_empty() {
                let Some(to) = run.next_deadline() else { break };
                self.clock.advance(Timestamp(to.0.saturating_sub(self.clock.now().0)));
                continue;
            }
            run.pass(ready[k as usize % ready.len()]).expect("pass");
        }
        (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
    }

    fn seen(&self, el: ElementId) -> Vec<Seen> {
        self.tape.lock().unwrap().iter().filter(|(e, _)| *e == el.0).map(|(_, s)| *s).collect()
    }

    fn rate(&self, link: LinkId) -> Option<i64> {
        match self.p.negotiated(link)?.get(self.p.field_id("rate")?)? {
            Value::Int(r) => Some(r),
            _ => None,
        }
    }
}

fn bus(p: &Pipeline) -> Vec<BusMessage> {
    std::iter::from_fn(|| p.bus().try_recv()).collect()
}

/// `src {48k, 44.1k} -> join (through) -> sink {44.1k, 48k}`, running at 48k.
fn junction(r: &mut Rig) -> (ElementId, ElementId, LinkId, LinkId) {
    let src = r.add("src", (&[], &[rates(&[48_000, 44_100])]), &[], Role::Source(400));
    let join =
        r.add("join", (&[any_rate()], &[any_rate()]), &[Coupling::through(0, 0)], Role::Forward);
    let sink = r.add("sink", (&[rates(&[44_100, 48_000])], &[]), &[], Role::Sink);
    let into = r.p.link(src.src(OUT), join.sink(IN)).unwrap();
    let out = r.p.link(join.src(OUT), sink.sink(IN)).unwrap();
    (src, join, into, out)
}

#[test]
fn a_branch_joining_behind_a_running_tee_takes_the_running_format() {
    let mut r = Rig::new();
    let src = r.add("src", (&[], &[rates(&[44_100, 48_000])]), &[], Role::Source(400));
    let tee = r.add("tee", (&[&WILD], &[&WILD, &WILD]), &[], Role::Forward);
    let a = r.add("a", (&[rates(&[48_000])], &[]), &[], Role::Sink);
    let up = r.p.link(src.src(OUT), tee.sink(IN)).unwrap();
    r.p.link(tee.src(OUT), a.sink(IN)).unwrap();
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    assert_eq!(r.rate(up), Some(48_000), "the one branch demands 48k");
    let b = r.add("b", (&[rates(&[44_100, 48_000])], &[]), &[], Role::Sink);
    let joined = r.p.link(tee.src(SrcPad(1)), b.sink(IN)).unwrap();
    assert!(r.p.dump_dot().contains("provisional"), "link time is not the commit's solve");
    r.p.commit().expect("commit");
    assert!(r.p.solve_meets() > 0 && r.p.solve_meets() < 64, "{} meets", r.p.solve_meets());
    assert_eq!((r.rate(up), r.rate(joined)), (Some(48_000), Some(48_000)));
    assert!(!r.p.dump_dot().contains("provisional"), "{}", r.p.dump_dot());
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
    assert_eq!(
        r.seen(b),
        [Seen::Start(Some(48_000))],
        "the joined sink starts at the running format"
    );
}

#[test]
fn a_source_joining_a_running_junction_is_solved_against_its_running_format() {
    let mut r = Rig::new();
    let (src, join, _, out) = junction(&mut r);
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    assert_eq!(r.rate(out), Some(48_000));
    r.p.remove(src, Leave::Cut).unwrap();
    let dec = r.add("dec", (&[], &[rates(&[44_100, 48_000])]), &[], Role::Source(100));
    let into = r.p.link(dec.src(OUT), join.sink(IN)).unwrap();
    r.p.commit().expect("commit");
    assert_eq!((r.rate(into), r.rate(out)), (Some(48_000), Some(48_000)));
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
}

#[test]
fn a_source_that_misses_the_junction_is_refused_and_the_books_are_unchanged() {
    let mut r = Rig::new();
    let (src, join, into, out) = junction(&mut r);
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    let before = [r.rate(into), r.rate(out)];
    r.p.remove(src, Leave::Cut).unwrap();
    let low = r.add("low", (&[], &[rates(&[44_100])]), &[], Role::Source(100));
    let refused = r.p.link(low.src(OUT), join.sink(IN));
    assert!(matches!(refused, Err(Error::Link(LinkError::NoCommonFormat(_)))), "{refused:?}");
    r.p.abandon();
    // Met at link time and moved off by a later edit, refused at the commit.
    r.p.remove(src, Leave::Cut).unwrap();
    let dec = r.add("dec", (&[], &[rates(&[44_100, 48_000])]), &[], Role::Source(100));
    let rs = r.add_with(
        "resample",
        (&[any_rate()], &[any_rate()]),
        &[Coupling::prop(0, "rate", "rate")],
        Role::Forward,
        &RATE_PROP,
    );
    r.p.link(dec.src(OUT), rs.sink(IN)).unwrap();
    let late = r.p.link(rs.src(OUT), join.sink(IN)).unwrap();
    r.p.set(rs, "rate", Value::Int(44_100)).unwrap();
    let dry = r.p.solve_check();
    assert!(matches!(dry, Err(Error::Solve(SolveError::Empty(_)))), "a dry run sees it, {dry:?}");
    assert_eq!(r.rate(late), Some(48_000), "and writes nothing");
    let e = r.p.commit().unwrap_err();
    let Error::Solve(SolveError::Empty(x)) = &e else { panic!("{e:?}") };
    assert_eq!((x.link, x.field.as_deref()), (late, Some("rate")), "{e}");
    let rules: Vec<(&str, &str)> = x.path.iter().map(|n| (n.element_name, n.rule)).collect();
    assert!(
        rules.contains(&("resample", "prop")) && rules.contains(&("join", "running format")),
        "{e}"
    );
    assert!(r.p.negotiated(late).is_none(), "the edit was taken back");
    assert_eq!([r.rate(into), r.rate(out)], before, "the running edges are as they were");
    assert!(r.run(&mut run, None), "the run plays on and ends");
    run.finish().expect("finish");
    assert_eq!(r.seen(rs), [], "the refused element never started");
}

#[test]
fn a_relink_across_groups_keeps_its_filter_and_is_re_solved() {
    let mut r = Rig::new();
    let src = r.add("src", (&[], &[rates(&[44_100, 48_000])]), &[], Role::Source(400));
    let a = r.add("a", (&[rates(&[44_100, 48_000])], &[]), &[], Role::Sink);
    let pin = [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(48_000)),
        preferred: None,
    }];
    let link = r.p.link_filtered(src.src(OUT), a.sink(IN), &pin).unwrap();
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    let b = r.add("b", (&[rates(&[44_100, 48_000])], &[]), &[], Role::Sink);
    r.p.relink(link, b.sink(IN)).unwrap();
    r.p.remove(a, Leave::Cut).unwrap();
    r.p.commit().expect("commit");
    assert_eq!(r.rate(link), Some(48_000), "unfiltered it would be 44.1k");
    assert!(r.p.dump_dot().contains("[rate=48000]"));
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
    assert_eq!(r.seen(b), [Seen::Start(Some(48_000))]);
}

#[test]
fn a_change_cancelled_before_its_splice_leaves_no_solved_format_behind() {
    let mut r = Rig::new();
    let src = r.add("src", (&[], &[rates(&[48_000, 44_100])]), &[], Role::Source(400));
    let mix =
        r.add("mix", (&[rates(&[44_100, 48_000]), rates(&[44_100, 48_000])], &[]), &[], Role::Sink);
    let first = r.p.link(src.src(OUT), mix.sink(IN)).unwrap();
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    let mute = r.add("mute", (&[], &[rates(&[44_100])]), &[], Role::Mute);
    let waits = r.p.link(mute.src(OUT), mix.sink(SinkPad(1))).unwrap();
    let change = r.p.commit().expect("commit");
    assert_eq!(r.rate(waits), Some(44_100));
    assert!(!r.run(&mut run, Some(20)));
    assert_eq!(r.p.cancel(), Some(change));
    r.p.settle();
    assert!(r.p.negotiated(waits).is_none(), "the solved edge left with the change");
    assert_eq!(r.rate(first), Some(48_000));
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
}

#[test]
fn a_coupled_branch_that_fails_in_start_after_the_solve_rolls_back() {
    let mut r = Rig::new();
    let (src, join, into, out) = junction(&mut r);
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    r.p.remove(src, Leave::Cut).unwrap();
    let bad = r.add("bad", (&[], &[rates(&[44_100, 48_000])]), &[], Role::Bad);
    let joined = r.p.link(bad.src(OUT), join.sink(IN)).unwrap();
    let change = r.p.commit().expect("the solve takes it");
    assert_eq!(r.rate(joined), Some(48_000));
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
    r.p.settle();
    let failed =
        |m: &BusMessage| matches!(m, BusMessage::ChangeFailed { change: c, .. } if *c == change);
    assert!(bus(&r.p).iter().any(failed));
    assert!(r.p.negotiated(joined).is_none(), "its format went with its edge");
    assert_eq!(
        (r.rate(into), r.rate(out)),
        (Some(48_000), Some(48_000)),
        "the old edge is back as it was"
    );
}

#[test]
fn a_commit_solve_allocates_nothing_when_warm() {
    let mut r = Rig::new();
    let (src, join, _, out) = junction(&mut r);
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    r.p.remove(src, Leave::Cut).unwrap();
    let dec = r.add("dec", (&[], &[rates(&[44_100, 48_000])]), &[], Role::Source(100));
    let mut up = dec;
    let rules = [Coupling::through(0, 0), Coupling::same(0, 0, "rate"), Coupling::through(0, 0)];
    for (i, rule) in rules.into_iter().enumerate() {
        let t =
            r.add(["t0", "t1", "t2"][i], (&[any_rate()], &[any_rate()]), &[rule], Role::Forward);
        r.p.link(up.src(OUT), t.sink(IN)).unwrap();
        up = t;
    }
    let into = r.p.link(up.src(OUT), join.sink(IN)).unwrap();
    r.p.solve_check().unwrap();
    r.p.solve_check().unwrap();
    let before = crate::link_leak::alloc_calls();
    r.p.solve_check().unwrap();
    assert_eq!(crate::link_leak::alloc_calls() - before, 0, "a warm solve of a change allocates");
    r.p.commit().expect("commit");
    assert_eq!((r.rate(into), r.rate(out)), (Some(48_000), Some(48_000)));
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
}

#[test]
fn a_change_after_the_upstream_read_its_whole_stream_splices() {
    let mut r = Rig::new();
    let src = r.add("src", (&[], &[rates(&[48_000])]), &[], Role::Source(6));
    let mute = r.add("mute", (&[rates(&[48_000])], &[]), &[], Role::Mute);
    let first = r.p.link(src.src(OUT), mute.sink(IN)).unwrap();
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(200)), "the mute sink holds the run");
    r.p.remove(mute, Leave::Cut).unwrap();
    let sink = r.add("sink", (&[rates(&[48_000])], &[]), &[], Role::Sink);
    let into = r.p.link(src.src(OUT), sink.sink(IN)).unwrap();
    let change = r.p.commit().expect("commit");
    assert!(r.run(&mut run, None), "the run ends behind the change");
    r.p.settle();
    assert!(!r.p.changing(), "the change spliced");
    let changed = |m: &BusMessage| matches!(m, BusMessage::Changed { change: c } if *c == change);
    assert!(bus(&r.p).iter().any(changed));
    assert_eq!((r.rate(into), r.p.negotiated(first).is_none()), (Some(48_000), true));
    run.finish().expect("finish");
    assert_eq!(r.seen(sink), [Seen::Start(Some(48_000))], "the new sink started and took the end");
}

#[test]
fn a_branch_that_drains_learns_its_input_was_cut_while_another_change_waits() {
    let mut r = Rig::new();
    let src = r.add("src", (&[], &[rates(&[48_000])]), &[], Role::Stall(3));
    let fwd =
        r.add("fwd", (&[any_rate()], &[any_rate()]), &[Coupling::through(0, 0)], Role::Forward);
    let sink = r.add("sink", (&[rates(&[48_000])], &[]), &[], Role::Sink);
    let feed = r.add("feed", (&[], &[rates(&[48_000])]), &[], Role::Stall(3));
    let mix = r.add("mix", (&[rates(&[48_000]), rates(&[48_000])], &[]), &[], Role::Sink);
    r.p.link(src.src(OUT), fwd.sink(IN)).unwrap();
    r.p.link(fwd.src(OUT), sink.sink(IN)).unwrap();
    r.p.link(feed.src(OUT), mix.sink(IN)).unwrap();
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(200)));
    let groups = |run: &Stepped, el: ElementId| {
        (0..run.groups()).filter(|g| run.elements(*g).contains(&el)).collect::<Vec<_>>()
    };
    let (branch, head) = ([groups(&run, fwd), groups(&run, sink)].concat(), groups(&run, src)[0]);
    r.p.remove(fwd, Leave::Drain).unwrap();
    r.p.remove(sink, Leave::Drain).unwrap();
    r.p.commit().expect("the branch leaves, draining");
    // A change waiting on a mute source is in flight when the source cuts its link.
    for g in branch.iter().copied() {
        run.service();
        run.pass(g).expect("pass");
    }
    let mute = r.add("mute", (&[], &[rates(&[48_000])]), &[], Role::Mute);
    r.p.link(mute.src(OUT), mix.sink(SinkPad(1))).unwrap();
    r.p.commit().expect("the mute source joins");
    run.service();
    run.pass(head).expect("the source cuts");
    assert!(!r.run(&mut run, Some(400)), "the mute source holds the run");
    assert!(
        branch.iter().all(|g| run.outcome(*g) == Some(Outcome::Exit)),
        "the drained branch saw its input end and left"
    );
    assert!(r.p.cancel().is_some(), "the mute source's change still waits");
    r.p.settle();
    for el in [src, feed, mix] {
        r.p.remove(el, Leave::Cut).unwrap();
    }
    r.p.commit().expect("commit");
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
}

fn desc_of(
    name: &'static str,
    (sinks, srcs): (&[Menu], &[Menu]),
    couplings: &[Coupling],
) -> &'static ElementDesc {
    let pad = |name: &'static str, offers: &Menu| PadDesc { name, offers };
    Box::leak(Box::new(ElementDesc {
        name,
        sinks: leak(sinks.iter().map(|m| pad("sink", m)).collect()),
        srcs: leak(srcs.iter().map(|m| pad("src", m)).collect()),
        props: &[],
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: leak(couplings.to_vec()),
    }))
}

fn splice(r: &mut Rig, run: &mut Stepped, ready: &mut Vec<usize>) {
    for k in 0..100_000usize {
        if !r.p.changing() {
            return;
        }
        run.service();
        ready.clear();
        ready.extend(run.running().iter().copied().filter(|g| run.ready(*g)));
        match ready.is_empty() {
            true => {
                let to = run.next_deadline().unwrap_or(Timestamp(r.clock.now().0 + 1_000_000));
                r.clock.advance(Timestamp(to.0.saturating_sub(r.clock.now().0).max(1)));
            }
            false => drop(run.pass(ready[k % ready.len()]).expect("a pass")),
        }
    }
    panic!("the change did not splice");
}

fn median(mut xs: Vec<u64>) -> u64 {
    xs.sort_unstable();
    xs[xs.len() / 2]
}

#[test]
fn a_churn_commit_costs_what_changes_and_not_what_was_ever_added() {
    // What a whole round asks of the allocator must not grow with every element ever added.
    const ROUNDS: usize = 10_000;
    let mut r = Rig::new();
    let (src, join, _, _) = junction(&mut r);
    let source = desc_of("src", (&[], &[rates(&[48_000, 44_100])]), &[]);
    let forward = desc_of("t", (&[any_rate()], &[any_rate()]), &[Coupling::through(0, 0)]);
    let mut run = r.step();
    let mut ready = Vec::with_capacity(64);
    assert!(!r.run(&mut run, Some(40)));
    let mut old = vec![src];
    let (mut calls, mut bytes, mut open) =
        (Vec::with_capacity(ROUNDS), Vec::with_capacity(ROUNDS), Vec::with_capacity(ROUNDS));
    let mut chain = Vec::with_capacity(8);
    for _ in 0..ROUNDS {
        let (c, b) = (crate::link_leak::alloc_calls(), crate::link_leak::alloc_bytes());
        old.iter().for_each(|el| r.p.remove(*el, Leave::Cut).expect("the chain leaves"));
        chain.clear();
        chain.push(r.add_desc(source, Role::Stall(u32::MAX)));
        for i in 0..6 {
            chain.push(r.add_desc(forward, Role::Forward));
            r.p.link(chain[i].src(OUT), chain[i + 1].sink(IN)).expect("the chain links");
        }
        r.p.link(chain[6].src(OUT), join.sink(IN)).expect("into the junction");
        r.p.commit().expect("the change is taken");
        splice(&mut r, &mut run, &mut ready);
        while r.p.bus().try_recv().is_some() {}
        calls.push(crate::link_leak::alloc_calls() - c);
        bytes.push(crate::link_leak::alloc_bytes() - b);
        open.push(run.running().len() as u64);
        std::mem::swap(&mut old, &mut chain);
    }
    let early = |xs: &[u64]| median(xs[100..600].to_vec());
    let late = |xs: &[u64]| median(xs[ROUNDS - 500..].to_vec());
    assert!(
        late(&calls) <= early(&calls) + 2,
        "allocations a round {} then {}",
        early(&calls),
        late(&calls)
    );
    assert!(
        late(&bytes) * 20 <= early(&bytes) * 21,
        "bytes a round {} then {}",
        early(&bytes),
        late(&bytes)
    );
    let most = |xs: &[u64]| xs.iter().copied().max().unwrap_or(0);
    assert!(
        most(&open[ROUNDS - 500..]) <= most(&open[100..600]),
        "the groups the driver walks grew"
    );
    drop(run);
}

#[test]
fn churn_leaves_the_tables_at_the_size_of_the_graph_there_is() {
    // Each round's elements and links are freed once the change spliced.
    const ROUNDS: usize = 10_000;
    let mut r = Rig::new();
    let (src, join, _, _) = junction(&mut r);
    let source = desc_of("src", (&[], &[rates(&[48_000, 44_100])]), &[]);
    let forward = desc_of("t", (&[any_rate()], &[any_rate()]), &[Coupling::through(0, 0)]);
    let mut run = r.step();
    let mut ready = Vec::with_capacity(64);
    assert!(!r.run(&mut run, Some(40)));
    let (mut old, mut chain) = (vec![src], Vec::with_capacity(8));
    let (mut rows, mut seats) = (Vec::with_capacity(ROUNDS), Vec::with_capacity(ROUNDS));
    for _ in 0..ROUNDS {
        old.iter().for_each(|el| r.p.remove(*el, Leave::Cut).expect("the chain leaves"));
        chain.clear();
        chain.push(r.add_desc(source, Role::Stall(u32::MAX)));
        for i in 0..6 {
            chain.push(r.add_desc(forward, Role::Forward));
            r.p.link(chain[i].src(OUT), chain[i + 1].sink(IN)).expect("the chain links");
        }
        r.p.link(chain[6].src(OUT), join.sink(IN)).expect("into the junction");
        r.p.commit().expect("the change is taken");
        splice(&mut r, &mut run, &mut ready);
        r.p.settle();
        while r.p.bus().try_recv().is_some() {}
        rows.push(r.p.table_rows());
        seats.push((run.seat_rows(), run.running().len()));
        std::mem::swap(&mut old, &mut chain);
    }
    let (elements, free, links) = rows[ROUNDS - 1];
    assert_eq!((elements - free, links), (9, 8), "the graph there is");
    assert_eq!(elements, 9 + 7, "a slot per element there is and per element that joined");
    assert!(rows[1..].iter().all(|t| *t == rows[1]), "the tables never grew past the first round");
    assert_eq!(r.p.element_count(), 3 + 7 * ROUNDS, "ids are not given out twice");
    assert!(seats.iter().all(|((held, _), live)| held == live), "a seat per group that runs");
    let most = |s: &[((usize, usize), usize)]| {
        s.iter()
            .map(|((held, room), _)| (*held, *room))
            .fold((0, 0), |a, b| (a.0.max(b.0), a.1.max(b.1)))
    };
    let (early, late) = (most(&seats[1..600]), most(&seats[ROUNDS - 500..]));
    assert!(late.0 <= early.0 && late.1 <= early.1, "the seats grew, {early:?} then {late:?}");
    assert!(late.0 <= 32, "{} seats", late.0);
    assert!(run.groups() > ROUNDS, "{} handles", run.groups());
    let closed =
        (0..run.groups()).find(|g| !run.running().contains(g)).expect("a group that closed");
    assert_eq!(
        (run.outcome(closed), run.elements(closed), run.held(closed), run.ready(closed)),
        (Some(Outcome::Exit), &[][..], 0, false)
    );
    assert_eq!(run.pass(closed).expect("a pass"), Outcome::Exit);
    assert_eq!(run.outcome(run.groups()), None, "no group has that handle yet");
    drop(run);
}

fn rate_range(min: i64, max: i64) -> Menu {
    let allowed = ConstraintDesc::Range {
        min: ValueDesc::Int(min),
        max: ValueDesc::Int(max),
        step: ValueDesc::Int(1),
    };
    leak(vec![OfferDesc {
        family: "audio/raw",
        fields: leak(vec![FieldDesc { field: "rate", allowed, preferred: None }]),
    }])
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];

/// A link that joins a running pad carries the format it runs at.
#[test]
fn a_link_that_joins_a_running_pad_carries_its_running_format() {
    for how in ["beside", "after a cut", "relinked"] {
        let mut r = Rig::new();
        let src = r.add("src", (&[], &[rates(&[44_100, 48_000])]), &[], Role::Source(400));
        let a = r.add("a", (&[rates(&[48_000])], &[]), &[], Role::Sink);
        let running = r.p.link(src.src(OUT), a.sink(IN)).unwrap();
        let mut run = r.step();
        assert!(!r.run(&mut run, Some(40)));
        let low = r.add("low", (&[rates(&[44_100])], &[]), &[], Role::Sink);
        let b = r.add("b", (&[rates(&[44_100, 48_000])], &[]), &[], Role::Sink);
        let joined = match how {
            "beside" => {
                let refused = r.p.link(src.src(OUT), low.sink(IN));
                assert!(
                    matches!(refused, Err(Error::Link(LinkError::NoCommonFormat(_)))),
                    "{refused:?}"
                );
                r.p.link(src.src(OUT), b.sink(IN)).unwrap()
            }
            "after a cut" => {
                r.p.remove(a, Leave::Cut).unwrap();
                assert!(
                    r.p.link(src.src(OUT), low.sink(IN)).is_err(),
                    "the cut link's format holds the pad"
                );
                r.p.link(src.src(OUT), b.sink(IN)).unwrap()
            }
            _ => {
                assert!(r.p.relink(running, low.sink(IN)).is_err());
                r.p.relink(running, b.sink(IN)).unwrap();
                r.p.remove(a, Leave::Cut).unwrap();
                running
            }
        };
        assert_eq!(r.rate(joined), Some(48_000), "{how}: at link time");
        r.p.commit().expect("commit");
        assert_eq!(r.rate(joined), Some(48_000), "{how}: at the commit");
        assert!(r.run(&mut run, None), "{how}: the run ends");
        run.finish().expect("finish");
        assert_eq!(r.seen(b), [Seen::Start(Some(48_000))], "{how}: told the running rate alone");
    }
}

#[test]
fn a_running_range_pins_its_value_not_its_range() {
    let mut r = Rig::new();
    let src = r.add("src", (&[], &[rate_range(8_000, 96_000)]), &[], Role::Source(400));
    let a = r.add("a", (&[rates(&[48_000])], &[]), &[], Role::Sink);
    r.p.link(src.src(OUT), a.sink(IN)).unwrap();
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    let low = r.add("low", (&[rates(&[44_100])], &[]), &[], Role::Sink);
    assert!(
        r.p.link(src.src(OUT), low.sink(IN)).is_err(),
        "44.1 kHz is in the range, not what runs"
    );
    let b = r.add("b", (&[rate_range(40_000, 50_000)], &[]), &[], Role::Sink);
    let joined = r.p.link(src.src(OUT), b.sink(IN)).unwrap();
    r.p.commit().expect("commit");
    assert_eq!(r.rate(joined), Some(48_000));
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
    assert_eq!(r.seen(b), [Seen::Start(Some(48_000))]);
}

#[test]
fn a_tap_beside_a_running_decoder_takes_its_bytes_and_a_typed_one_its_rate() {
    let mut r = Rig::new();
    let file = r.add("file", (&[], &[&BYTES]), &[], Role::Source(400));
    let out = leak(vec![
        OfferDesc { family: "audio/raw", fields: any_rate()[0].fields },
        OfferDesc::any("bytes"),
    ]);
    let dec = r.add("dec", (&[&BYTES], &[out]), &[], Role::Forward);
    let a = r.add("a", (&[rates(&[44_100])], &[]), &[], Role::Sink);
    r.p.link(file.src(OUT), dec.sink(IN)).unwrap();
    let running = r.p.link(dec.src(OUT), a.sink(IN)).unwrap();
    let mut run = r.step();
    assert!(!r.run(&mut run, Some(40)));
    let tap = r.add("tap", (&[&BYTES], &[]), &[], Role::Sink);
    let tapped = r.p.link(dec.src(OUT), tap.sink(IN)).expect("a tap takes the bytes");
    assert_eq!(r.p.negotiated(tapped).map(|f| r.p.family_name(f.family)), Some(Some("bytes")));
    let high = r.add("high", (&[rates(&[48_000])], &[]), &[], Role::Sink);
    assert!(r.p.link(dec.src(OUT), high.sink(IN)).is_err(), "the running rate is 44.1 kHz");
    let b = r.add("b", (&[rates(&[44_100, 48_000])], &[]), &[], Role::Sink);
    let typed = r.p.link(dec.src(OUT), b.sink(IN)).unwrap();
    let unsettled: Vec<&str> = r.p.unsettled(typed).map(|f| r.p.field_name(f).unwrap()).collect();
    assert_eq!(
        (r.rate(typed), unsettled),
        (Some(44_100), vec!["rate"]),
        "open on the pad, open on the link"
    );
    r.p.commit().expect("commit");
    assert_eq!((r.rate(running), r.rate(typed)), (Some(44_100), Some(44_100)));
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
    assert_eq!(r.seen(b), [Seen::Start(Some(44_100))]);
    assert_eq!(
        r.seen(tap),
        [Seen::Start(None), Seen::Format(Some(44_100))],
        "the tap is told what the pad carries"
    );
}

#[test]
fn a_pad_with_a_bytes_tap_starts_at_its_typed_format() {
    let mut r = Rig::new();
    let file = r.add("file", (&[], &[&BYTES]), &[], Role::Source(20));
    let out = leak(vec![
        OfferDesc { family: "audio/raw", fields: any_rate()[0].fields },
        OfferDesc::any("bytes"),
    ]);
    let dec = r.add("dec", (&[&BYTES], &[out]), &[], Role::Forward);
    let a = r.add("a", (&[rates(&[44_100])], &[]), &[], Role::Sink);
    let tap = r.add("tap", (&[&BYTES], &[]), &[], Role::Sink);
    r.p.link(file.src(OUT), dec.sink(IN)).unwrap();
    r.p.link(dec.src(OUT), a.sink(IN)).unwrap();
    r.p.link(dec.src(OUT), tap.sink(IN)).unwrap();
    let mut run = r.step();
    assert!(r.run(&mut run, None), "the run ends");
    run.finish().expect("finish");
    assert_eq!(r.seen(dec), [Seen::Out(Some(44_100))]);
}
