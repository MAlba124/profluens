// A tail with two time-limited tracks far apart in its file. The track behind is not held back.

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, GraphError};
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::level::Limit;
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::Stepped;
use profluens_core::time::Timestamp;
use profluens_core::weave::MAX_INTERLEAVE;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static TWO: [PadDesc; 2] =
    [PadDesc { name: "a", offers: &BYTES }, PadDesc { name: "b", offers: &BYTES }];

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

static DEMUX: ElementDesc = desc("skewed", &[], &TWO);
static SINK: ElementDesc = desc("paced", &ONE, &[]);

const STEP: Timestamp = Timestamp::from_millis(20);
const ROWS: u64 = 600;

/// Track 0 runs `ahead` rows ahead of track 1 in file order, one row a pass.
struct Skewed {
    ahead: u64,
    k: u64,
}

impl Skewed {
    fn row(&self) -> (u32, u64) {
        match self.k.checked_sub(self.ahead) {
            None => (0, self.k),
            Some(j) if j.is_multiple_of(2) => (1, j / 2),
            Some(j) => (0, self.ahead + j / 2),
        }
    }
}

impl Element for Skewed {
    fn desc(&self) -> &'static ElementDesc {
        &DEMUX
    }
    fn process(&mut self, ctx: &mut Ctx, _: Inputs<'_>) -> Result<Flow, Error> {
        // The track ahead is done, the other one goes on alone.
        while self.row().1 >= ROWS && self.k < self.ahead + 2 * ROWS {
            self.k += 1;
        }
        if self.k >= self.ahead + 2 * ROWS {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let (pad, n) = self.row();
        buf.memory.set_len(8);
        (buf.pts, buf.duration) = (Timestamp(n * STEP.0), STEP);
        ctx.out(SrcPad(pad)).push(buf);
        self.k += 1;
        Ok(Flow::Ok)
    }
}

#[derive(Default)]
struct Seen {
    first: Option<Timestamp>,
    rows: u64,
}

struct Paced {
    seen: Arc<Mutex<Seen>>,
    clock: MockClock,
}

impl Element for Paced {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(front) = inputs.front() {
            if !ctx.due(front.pts) {
                break;
            }
            inputs.pop();
            let mut seen = self.seen.lock().unwrap();
            seen.first.get_or_insert(self.clock.now());
            seen.rows += 1;
        }
        Ok(Flow::Ok)
    }
}

struct Outcome {
    first: [Timestamp; 2],
    rows: [u64; 2],
    warned: usize,
}

fn drive(run: &mut Stepped, clock: &MockClock) {
    for _ in 0..2_000_000 {
        let ready = (0..run.groups()).find(|g| run.ready(*g));
        match (ready, run.next_deadline()) {
            (Some(g), _) => drop(run.pass(g).expect("pass")),
            (None, Some(to)) => clock.advance(Timestamp(to.0.saturating_sub(clock.now().0).max(1))),
            (None, None) => return,
        }
    }
    panic!("no quiescence");
}

fn play(ahead: Timestamp, limit: Timestamp) -> Outcome {
    let clock = MockClock::starting_at(Timestamp::from_secs(5));
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    p.set_pool(64, 1024);
    let src = p.add(Skewed { ahead: ahead.0 / STEP.0, k: 0 });
    let seen: [Arc<Mutex<Seen>>; 2] = Default::default();
    let sinks: Vec<ElementId> =
        seen.iter().map(|s| p.add(Paced { seen: Arc::clone(s), clock: clock.clone() })).collect();
    for (pad, sink) in sinks.iter().enumerate() {
        p.link(src.src(SrcPad(pad as u32)), sink.sink(SinkPad(0))).expect("link");
        p.set_queue_capacity(*sink, 512);
        p.set_link_limit(*sink, Limit::time(limit));
    }
    let mut run = p.step(Arc::new(clock.clone())).expect("step");
    drive(&mut run, &clock);
    run.finish().expect("finish");
    let warned = std::iter::from_fn(|| p.bus().try_recv())
        .filter(|m| {
            matches!(
                m,
                BusMessage::Warning { error: Error::Graph(GraphError::Interleaved { .. }), .. }
            )
        })
        .count();
    let (a, b) = (seen[0].lock().unwrap(), seen[1].lock().unwrap());
    Outcome {
        first: [a.first.expect("track 0 played"), b.first.expect("track 1 played")],
        rows: [a.rows, b.rows],
        warned,
    }
}

#[test]
fn a_track_far_behind_in_the_file_starts_with_the_one_ahead() {
    let limit = Timestamp::from_millis(100);
    for ahead in [200, 1_000, 3_000].map(Timestamp::from_millis) {
        let out = play(ahead, limit);
        assert_eq!(out.rows, [ROWS, ROWS], "ahead {ahead:?}");
        let late = out.first[1].saturating_sub(out.first[0]);
        assert!(late <= STEP, "ahead {ahead:?}, track 1 started {late:?} after track 0");
        assert_eq!(out.warned, 0, "ahead {ahead:?}");
    }
}

#[test]
fn a_track_past_the_cap_starts_late_and_the_tail_warns_once() {
    let ahead = MAX_INTERLEAVE.saturating_add(Timestamp::from_secs(2));
    let out = play(ahead, Timestamp::from_millis(100));
    assert_eq!(out.rows, [ROWS, ROWS]);
    let late = out.first[1].saturating_sub(out.first[0]);
    assert!(late >= Timestamp::from_secs(1), "the cap holds it back, {late:?}");
    assert_eq!(out.warned, 1);
}

#[test]
fn the_latency_report_holds_the_residency_of_each_path() {
    let clock = MockClock::starting_at(Timestamp::from_secs(5));
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    let src = p.add(Skewed { ahead: 0, k: 0 });
    let seen = Arc::new(Mutex::new(Seen::default()));
    let sinks: Vec<ElementId> =
        (0..2).map(|_| p.add(Paced { seen: Arc::clone(&seen), clock: clock.clone() })).collect();
    for (pad, sink) in sinks.iter().enumerate() {
        p.link(src.src(SrcPad(pad as u32)), sink.sink(SinkPad(0))).expect("link");
    }
    let second = Timestamp::from_secs(1);
    p.set_link_limit(sinks[0], Limit::time(second));
    let residency = |p: &Pipeline, sink: ElementId| {
        p.latency_report().paths.iter().find(|path| path.sink == sink).expect("a path").residency
    };
    assert_eq!(
        (residency(&p, sinks[0]), residency(&p, sinks[1])),
        (second, Timestamp::ZERO),
        "one timed link"
    );
    p.set_link_limit(sinks[1], Limit::bytes(1 << 20));
    assert_eq!(residency(&p, sinks[1]), Timestamp::ZERO, "a limit in bytes says no time");
    p.set_link_limit(sinks[1], Limit::time(second));
    assert_eq!(
        (residency(&p, sinks[0]), residency(&p, sinks[1])),
        (MAX_INTERLEAVE, MAX_INTERLEAVE),
        "two grow to the cap"
    );
}
