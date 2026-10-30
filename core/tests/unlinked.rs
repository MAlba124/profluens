use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, GraphError, LinkError};
use profluens_core::format::{OfferDesc, OfferSpec};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::Outcome;
use profluens_core::time::Timestamp;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static ONE_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static TWO_OUT: [PadDesc; 2] =
    [PadDesc { name: "a", offers: &BYTES }, PadDesc { name: "b", offers: &BYTES }];
static TWO_IN: [PadDesc; 2] =
    [PadDesc { name: "a", offers: &BYTES }, PadDesc { name: "b", offers: &BYTES }];
static PROBED_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];

static SRC: ElementDesc = desc("usrc", &[], &ONE_OUT);
static SRC2: ElementDesc = desc("usrc2", &[], &TWO_OUT);
static FWD: ElementDesc = desc("ufwd", &ONE_IN, &ONE_OUT);
static SINK: ElementDesc = desc("usink", &ONE_IN, &[]);
static JOIN: ElementDesc = desc("ujoin", &TWO_IN, &[]);
static DEVICE: ElementDesc = desc("udevice", &PROBED_IN, &[]);
static OTHER: [OfferDesc; 1] = [OfferDesc::any("video/other")];
static OTHER_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OTHER }];
static PICKY: ElementDesc = desc("upicky", &OTHER_IN, &ONE_OUT);
static LATE: ElementDesc = desc("ulate", &[], &[]);

#[derive(Default)]
struct Seen {
    prerolls: AtomicU32,
    clock_asks: AtomicU32,
    starts: AtomicU32,
    calls: AtomicU32,
    buffers: AtomicU32,
    stops: AtomicU32,
}

impl Seen {
    fn ran(&self) -> [u32; 4] {
        let get = |n: &AtomicU32| n.load(Ordering::Relaxed);
        [get(&self.starts), get(&self.calls), get(&self.stops), get(&self.clock_asks)]
    }
}

struct Node {
    desc: &'static ElementDesc,
    left: u32,
    offers: &'static str,
    seen: Arc<Seen>,
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.seen.prerolls.fetch_add(1, Ordering::Relaxed);
        if std::ptr::eq(self.desc, &LATE) {
            ctx.add_src_pad("late_0", &BYTES);
        }
        match std::ptr::eq(self.desc, &DEVICE) {
            true => ctx.offer_sink(IN, &[OfferSpec::any(self.offers)]),
            false => Ok(()),
        }
    }

    fn provide_clock(&mut self) -> Option<Arc<dyn Clock>> {
        self.seen.clock_asks.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.seen.starts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.seen.calls.fetch_add(1, Ordering::Relaxed);
        let srcs = self.desc.srcs.len() as u32;
        while let Some(buf) = inputs.pop() {
            self.seen.buffers.fetch_add(1, Ordering::Relaxed);
            if srcs > 0 {
                ctx.out(OUT).push(buf);
            }
        }
        if !self.desc.sinks.is_empty() {
            return Ok(Flow::Ok);
        }
        if self.left == 0 {
            return Ok(Flow::Eos);
        }
        for pad in 0..srcs {
            let buf = ctx.alloc();
            ctx.out(SrcPad(pad)).push(buf);
        }
        self.left = self.left.saturating_sub((self.left != u32::MAX) as u32);
        Ok(Flow::Ok)
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.seen.stops.fetch_add(1, Ordering::Relaxed);
    }
}

fn add(p: &mut Pipeline, desc: &'static ElementDesc, left: u32) -> (ElementId, Arc<Seen>) {
    offering(p, desc, left, "bytes")
}

fn offering(
    p: &mut Pipeline,
    desc: &'static ElementDesc,
    left: u32,
    offers: &'static str,
) -> (ElementId, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    (p.add(Node { desc, left, offers, seen: Arc::clone(&seen) }), seen)
}

fn chain(p: &mut Pipeline, buffers: u32) -> Arc<Seen> {
    let (src, _) = add(p, &SRC, buffers);
    let (sink, seen) = add(p, &SINK, 0);
    p.link(src.src(OUT), sink.sink(IN)).expect("link");
    seen
}

fn run(p: &mut Pipeline) -> Result<(), Error> {
    let (stop, done) = (p.control(), Arc::new(AtomicBool::new(false)));
    let over = Arc::clone(&done);
    let dog = std::thread::spawn(move || {
        for _ in 0..1_000 {
            if over.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.stop();
        true
    });
    let result = p.run();
    done.store(true, Ordering::Release);
    assert!(!dog.join().expect("watchdog"), "the run did not end by itself");
    result
}

fn left_out(p: &Pipeline) -> Vec<(ElementId, &'static str, bool)> {
    let mut out = Vec::new();
    while let Some(msg) = p.bus().try_recv() {
        let BusMessage::Warning { element, error } = msg else {
            continue;
        };
        let (named, name, starved) = match error {
            Error::Graph(GraphError::Unlinked { element, element_name }) => {
                (element, element_name, false)
            }
            Error::Graph(GraphError::Starved { element, element_name }) => {
                (element, element_name, true)
            }
            other => panic!("another warning, {other}"),
        };
        assert_eq!(element, named);
        out.push((element, name, starved));
    }
    out
}

fn warned(p: &Pipeline) -> Vec<(ElementId, &'static str)> {
    let named = left_out(p);
    assert!(named.iter().all(|(_, _, starved)| !starved), "{named:?}");
    named.into_iter().map(|(element, name, _)| (element, name)).collect()
}

fn kept(p: &Pipeline, el: ElementId) -> bool {
    !matches!(p.src_pad(el, "?"), Err(Error::Link(LinkError::UnknownElement { .. })))
}

#[test]
fn an_unlinked_sink_and_transform_take_no_part_and_the_run_ends_by_itself() {
    let mut p = Pipeline::new();
    let (src, _) = add(&mut p, &SRC, 3);
    let (lone_sink, a) = add(&mut p, &SINK, 0);
    let (sink, taken) = add(&mut p, &SINK, 0);
    let (lone_fwd, b) = add(&mut p, &FWD, 0);
    p.link(src.src(OUT), sink.sink(IN)).expect("link");

    run(&mut p).expect("the run ends when its one linked sink has ended");
    assert_eq!(taken.buffers.load(Ordering::Relaxed), 3);
    assert_eq!((a.ran(), b.ran()), ([0; 4], [0; 4]), "never started, stepped, stopped or asked");
    assert_eq!(warned(&p), [(lone_sink, "usink"), (lone_fwd, "ufwd")], "one warning each");
    assert!(kept(&p, lone_sink) && kept(&p, lone_fwd), "the run left them in the pipeline");
    assert!(!kept(&p, src) && !kept(&p, sink), "and took the ones that ran");
    assert_eq!(p.counters(lone_fwd).buffers_in, 0);
}

#[test]
fn the_stepped_driver_gives_an_unlinked_element_no_group() {
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p = Pipeline::new();
    let (lone, seen) = add(&mut p, &SINK, 0);
    let taken = chain(&mut p, 2);
    let mut run = p.step(Arc::new(clock)).expect("step");
    assert_eq!(run.groups(), 2, "the source and its sink");
    assert!((0..run.groups()).all(|g| !run.elements(g).contains(&lone)));
    assert_eq!(warned(&p), [(lone, "usink")]);
    for _ in 0..200 {
        let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
            break;
        };
        run.pass(g).expect("pass");
    }
    assert!((0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit)), "every group left");
    assert!(run.control().is_finished(), "the run finished without the lone sink");
    run.finish().expect("finish");
    assert_eq!((taken.buffers.load(Ordering::Relaxed), seen.ran()), (2, [0; 4]));
}

#[test]
fn a_source_with_no_linked_pad_does_not_run_and_the_run_does_not_wait_for_it() {
    let mut p = Pipeline::new();
    let (endless, a) = add(&mut p, &SRC, u32::MAX);
    let (finite, b) = add(&mut p, &SRC2, 5);
    let taken = chain(&mut p, 4);
    run(&mut p).expect("the chain ends the run");
    assert_eq!(taken.buffers.load(Ordering::Relaxed), 4);
    assert_eq!((a.ran(), b.ran()), ([0; 4], [0; 4]));
    assert_eq!(warned(&p), [(endless, "usrc"), (finite, "usrc2")]);
    assert_eq!(p.counters(finite).drops, 0, "it made nothing to drop");
}

#[test]
fn a_partly_linked_element_runs_as_before_and_its_unlinked_pad_drops() {
    let mut p = Pipeline::new();
    let (src, seen) = add(&mut p, &SRC2, 5);
    let (sink, taken) = add(&mut p, &SINK, 0);
    let (fwd, through) = add(&mut p, &FWD, 0);
    p.link(src.src(SrcPad(1)), fwd.sink(IN)).expect("link");
    let (tail, _) = add(&mut p, &FWD, 0);
    p.link(fwd.src(OUT), tail.sink(IN)).expect("link");
    run(&mut p).expect("run");
    assert_eq!(warned(&p), [(sink, "usink")], "only the element with no link at all");
    assert_eq!(seen.ran()[0], 1);
    assert_eq!(through.buffers.load(Ordering::Relaxed), 5);
    assert_eq!(p.counters(src).drops, 5, "the first pad has no link");
    assert_eq!(p.counters(tail).drops, 5, "a transform with no linked output ends the run");
    assert_eq!(taken.ran(), [0; 4]);
}

#[test]
fn a_probed_pad_left_pending_or_refused_leaves_its_element_out() {
    for prerolled in [false, true] {
        let mut p = Pipeline::new();
        let (src, _) = add(&mut p, &SRC, 3);
        let (device, seen) = offering(&mut p, &DEVICE, 0, "video/gpu");
        if prerolled {
            p.preroll().expect("preroll");
        }
        let refused = p.link(src.src(OUT), device.sink(IN)).expect_err("no link");
        let pending = matches!(refused, Error::Link(LinkError::OffersPending { .. }));
        let no_format = matches!(refused, Error::Link(LinkError::NoCommonFormat(_)));
        assert_eq!((pending, no_format), (!prerolled, prerolled), "{refused}");
        let (sink, taken) = add(&mut p, &SINK, 0);
        p.link(src.src(OUT), sink.sink(IN)).expect("link");
        run(&mut p).expect("the fallback plays and the run ends");
        assert_eq!(taken.buffers.load(Ordering::Relaxed), 3);
        assert_eq!(seen.ran(), [0; 4]);
        assert_eq!(seen.prerolls.load(Ordering::Relaxed), prerolled as u32);
        assert_eq!(warned(&p), [(device, "udevice")]);
        assert!(kept(&p, device));
    }
}

#[test]
fn an_element_whose_dynamic_pads_never_appeared_takes_no_part() {
    let mut p = Pipeline::new();
    let (late, seen) = add(&mut p, &LATE, 3);
    let taken = chain(&mut p, 2);
    run(&mut p).expect("run");
    assert_eq!((taken.buffers.load(Ordering::Relaxed), seen.ran()), (2, [0; 4]));
    assert_eq!(warned(&p), [(late, "ulate")]);

    let mut p = Pipeline::new();
    let (late, seen) = add(&mut p, &LATE, 3);
    let taken = chain(&mut p, 2);
    assert_eq!(p.preroll().expect("preroll").srcs.len(), 1);
    run(&mut p).expect("run");
    assert_eq!((taken.buffers.load(Ordering::Relaxed), seen.ran()), (2, [0; 4]));
    assert_eq!(warned(&p), [(late, "ulate")]);
}

#[test]
fn a_pipeline_with_nothing_linked_does_not_run_and_keeps_its_elements() {
    let mut p = Pipeline::new();
    let (src, a) = add(&mut p, &SRC, 3);
    let (sink, b) = add(&mut p, &SINK, 0);
    let empty = |r: Result<(), Error>| matches!(r, Err(Error::Graph(GraphError::Empty)));
    assert!(empty(run(&mut p)), "nothing to run is an error, not a run that played nothing");
    assert_eq!(warned(&p), [(src, "usrc"), (sink, "usink")]);
    let clock = MockClock::starting_at(Timestamp::ZERO);
    assert!(empty(p.step(Arc::new(clock)).map(|_| ())));
    assert_eq!(warned(&p), [(src, "usrc"), (sink, "usink")], "said again by every run");
    assert_eq!((a.ran(), b.ran()), ([0; 4], [0; 4]));
    assert!(kept(&p, src) && kept(&p, sink));

    p.link(src.src(OUT), sink.sink(IN)).expect("link");
    p.set_clock(Arc::new(profluens_core::clock::InstantClock::new()));
    run(&mut p).expect("run");
    assert_eq!(b.buffers.load(Ordering::Relaxed), 3);
    assert!(warned(&p).is_empty());
}

#[test]
fn elements_linked_to_each_other_and_fed_by_no_source_take_no_part() {
    let mut p = Pipeline::new();
    let (fwd, a) = add(&mut p, &FWD, 0);
    let (lone, b) = add(&mut p, &SINK, 0);
    p.link(fwd.src(OUT), lone.sink(IN)).expect("link");
    let taken = chain(&mut p, 3);
    run(&mut p).expect("the chain ends the run");
    assert_eq!(taken.buffers.load(Ordering::Relaxed), 3);
    assert_eq!((a.ran(), b.ran()), ([0; 4], [0; 4]));
    assert_eq!(left_out(&p), [(fwd, "ufwd", true), (lone, "usink", true)]);
    assert!(kept(&p, fwd) && kept(&p, lone), "the run left the island in the pipeline");
}

#[test]
fn a_chain_of_three_behind_a_missing_source_takes_no_part() {
    let mut p = Pipeline::new();
    let (sink, c) = add(&mut p, &SINK, 0);
    let (second, b) = add(&mut p, &FWD, 0);
    let (first, a) = add(&mut p, &FWD, 0);
    p.link(second.src(OUT), sink.sink(IN)).expect("link");
    p.link(first.src(OUT), second.sink(IN)).expect("link");
    let taken = chain(&mut p, 2);
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut stepped = p.step(Arc::new(clock)).expect("step");
    assert_eq!(stepped.groups(), 2, "the chain that has a source, and nothing else");
    let starved = [(sink, "usink", true), (second, "ufwd", true), (first, "ufwd", true)];
    assert_eq!(left_out(&p), starved, "each is named, in id order");
    for _ in 0..200 {
        let Some(g) = (0..stepped.groups()).find(|g| stepped.ready(*g)) else {
            break;
        };
        stepped.pass(g).expect("pass");
    }
    assert!(stepped.control().is_finished(), "the run finished without the three");
    stepped.finish().expect("finish");
    assert_eq!(taken.buffers.load(Ordering::Relaxed), 2);
    assert_eq!((a.ran(), b.ran(), c.ran()), ([0; 4], [0; 4], [0; 4]));
}

#[test]
fn an_island_alone_is_an_empty_pipeline() {
    let mut p = Pipeline::new();
    let (fwd, a) = add(&mut p, &FWD, 0);
    let (sink, b) = add(&mut p, &SINK, 0);
    p.link(fwd.src(OUT), sink.sink(IN)).expect("link");
    assert!(matches!(run(&mut p), Err(Error::Graph(GraphError::Empty))), "nothing can run");
    assert_eq!(left_out(&p), [(fwd, "ufwd", true), (sink, "usink", true)]);
    assert_eq!((a.ran(), b.ran()), ([0; 4], [0; 4]));
    let (src, _) = add(&mut p, &SRC, 4);
    p.link(src.src(OUT), fwd.sink(IN)).expect("link");
    run(&mut p).expect("run");
    assert_eq!(b.buffers.load(Ordering::Relaxed), 4);
    assert!(left_out(&p).is_empty());
}

#[test]
fn a_fan_in_with_one_input_fed_and_one_starved_runs_on_the_fed_one() {
    for fed_pad in [1, 0] {
        let mut p = Pipeline::new();
        let (join, taken) = add(&mut p, &JOIN, 0);
        let (dry, a) = add(&mut p, &FWD, 0);
        let (src, _) = add(&mut p, &SRC, 5);
        p.link(dry.src(OUT), join.sink(SinkPad(1 - fed_pad))).expect("link");
        p.link(src.src(OUT), join.sink(SinkPad(fed_pad))).expect("link");
        run(&mut p).expect("the fed input ends the run");
        assert_eq!(taken.buffers.load(Ordering::Relaxed), 5, "fed on pad {fed_pad}");
        assert_eq!(taken.ran()[0], 1, "the join ran");
        assert_eq!(a.ran(), [0; 4]);
        assert_eq!(left_out(&p), [(dry, "ufwd", true)], "only the transform with no input");
        assert!(kept(&p, dry) && !kept(&p, join));
    }
}

#[test]
fn a_generator_is_a_source_and_what_it_feeds_runs() {
    let mut p = Pipeline::new();
    let (src, _) = add(&mut p, &SRC2, 3);
    let (first, _) = add(&mut p, &FWD, 0);
    let (second, _) = add(&mut p, &FWD, 0);
    let (sink, taken) = add(&mut p, &SINK, 0);
    p.link(src.src(SrcPad(1)), first.sink(IN)).expect("link");
    p.link(first.src(OUT), second.sink(IN)).expect("link");
    p.link(second.src(OUT), sink.sink(IN)).expect("link");
    run(&mut p).expect("run");
    assert_eq!(taken.buffers.load(Ordering::Relaxed), 3);
    assert!(left_out(&p).is_empty());
}

#[test]
fn a_pair_that_linked_and_was_refused_the_pad_is_left_out_and_the_fallback_plays() {
    let mut p = Pipeline::new();
    let (src, _) = add(&mut p, &SRC, 3);
    let (decoder, a) = add(&mut p, &PICKY, 0);
    let (device, b) = add(&mut p, &SINK, 0);
    p.link(decoder.src(OUT), device.sink(IN)).expect("the pair agrees");
    let refused = p.link(src.src(OUT), decoder.sink(IN)).expect_err("another family");
    assert!(matches!(refused, Error::Link(LinkError::NoCommonFormat(_))), "{refused}");
    let (fallback, taken) = add(&mut p, &SINK, 0);
    p.link(src.src(OUT), fallback.sink(IN)).expect("link");
    run(&mut p).expect("the fallback plays and the run ends");
    assert_eq!(taken.buffers.load(Ordering::Relaxed), 3);
    assert_eq!((a.ran(), b.ran()), ([0; 4], [0; 4]));
    assert_eq!(left_out(&p), [(decoder, "upicky", true), (device, "usink", true)]);
}
