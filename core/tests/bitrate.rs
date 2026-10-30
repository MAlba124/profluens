// A demuxer tells its source the bitrate up its sink pad, and a seek's flush clears it.

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::clock::MockClock;
use profluens_core::command::ALL;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::feedback::{Bitrate, Feedback, RateKind};
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::rate;
use profluens_core::time::Timestamp;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static ONE_IN: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];
static ONE_OUT: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];

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

static SRC: ElementDesc = desc("bytesrc", &[], &ONE_OUT, SchedHint::Active);
static INLINE: ElementDesc = desc("demux", &ONE_IN, &[], SchedHint::Passive);
static GROUPED: ElementDesc = desc("demux", &ONE_IN, &[], SchedHint::Active);

const RATE: Bitrate = Bitrate { bits_per_second: 1_000_000, kind: RateKind::Local };

/// Per pass of the source, what `ctx.feedback` said of the rate, and the rates told it.
#[derive(Default)]
struct Heard {
    passes: Vec<Option<Bitrate>>,
    told: Vec<Bitrate>,
}

struct Src {
    heard: Arc<Mutex<Heard>>,
    left: u32,
    step: u64,
    again: bool,
}

impl Element for Src {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }
    fn process(&mut self, ctx: &mut Ctx, _: Inputs<'_>) -> Result<Flow, Error> {
        self.heard.lock().unwrap().passes.push(ctx.feedback(SrcPad(0)).bitrate);
        if self.left == 0 {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.set_len(8);
        buf.pts = Timestamp::from_millis(u64::from(40 - self.left) * self.step);
        ctx.out(SrcPad(0)).push(buf);
        self.left -= 1;
        Ok(Flow::Ok)
    }
    fn event(&mut self, _: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::Feedback { feedback: Feedback::Bitrate(b), .. } = event {
            self.heard.lock().unwrap().told.push(*b);
        }
        if matches!(event, Event::FlushStart) && self.again {
            self.left = 40;
        }
        Ok(())
    }
}

/// Tells the rate at the buffer `at` of each stretch, or with a teller when due.
struct Demux {
    desc: &'static ElementDesc,
    at: u32,
    seen: u32,
    teller: Option<rate::Teller>,
}

impl Element for Demux {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut newest = Timestamp::NONE;
        while let Some(buf) = inputs.pop() {
            self.seen += 1;
            newest = buf.pts;
            if self.seen == self.at && self.teller.is_none() {
                ctx.publish_feedback(SinkPad(0), Feedback::Bitrate(RATE));
            }
        }
        if let (Some(teller), true) = (self.teller.as_mut(), newest.is_some()) {
            let rate = |at: Timestamp| {
                Some(Bitrate {
                    bits_per_second: 1_000 + at.0 / 1_000_000_000,
                    kind: RateKind::Local,
                })
            };
            teller.tell_at(ctx, SinkPad(0), newest, rate);
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, _: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            self.seen = 0;
            self.teller.as_mut().map(rate::Teller::restart);
        }
        Ok(())
    }
}

/// The source into a demuxer that tells the rate at its third buffer, seeking at `seek_at` passes.
fn run(demux: &'static ElementDesc, seek_at: Option<usize>) -> Heard {
    run_with(demux, seek_at, None, 10)
}

fn run_with(
    demux: &'static ElementDesc,
    seek_at: Option<usize>,
    teller: Option<rate::Teller>,
    step: u64,
) -> Heard {
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    p.set_pool(64, 8);
    let heard = Arc::new(Mutex::new(Heard::default()));
    let again = teller.is_some();
    let src = p.add(Src { heard: Arc::clone(&heard), left: 40, step, again });
    let sink = p.add(Demux { desc: demux, at: 3, seen: 0, teller });
    p.link(src.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("link");
    let mut run = p.step(Arc::new(clock.clone())).expect("step");
    let mut sought = false;
    for _ in 0..100_000 {
        let passes = heard.lock().unwrap().passes.len();
        if seek_at.is_some_and(|at| passes >= at) && !sought {
            sought = true;
            run.control()
                .seek_bytes(Timestamp::ZERO, &[(ALL, 0, Timestamp::ZERO)])
                .expect("a seek");
        }
        let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) else {
            break;
        };
        run.pass(g).expect("pass");
    }
    drop(run);
    Arc::try_unwrap(heard).ok().expect("the run is gone").into_inner().unwrap()
}

#[test]
fn a_rate_told_inline_reaches_the_source_before_its_next_pass() {
    let heard = run(&INLINE, None);
    // The rate told at the source's third pass reaches its fourth.
    assert_eq!(heard.passes[..4], [None, None, None, Some(RATE)]);
    assert_eq!(heard.told, [RATE], "the event door says it once");
}

#[test]
fn a_rate_told_across_a_ring_is_read_at_the_top_of_the_sources_pass() {
    let heard = run(&GROUPED, None);
    let first = heard.passes.iter().position(Option::is_some).expect("the source heard it");
    assert!(heard.passes[first..].iter().all(|b| *b == Some(RATE)), "and keeps it");
    assert_eq!(heard.told, [RATE]);
}

#[test]
fn a_seek_clears_the_rate_until_the_demuxer_tells_it_again() {
    let heard = run(&INLINE, Some(10));
    let gaps = heard.passes.windows(2).filter(|w| w[0].is_some() && w[1].is_none()).count();
    assert_eq!(gaps, 1, "cleared once by the flush, {:?}", heard.passes);
    assert_eq!(heard.passes.last(), Some(&Some(RATE)), "told again behind it");
    assert_eq!(heard.told, [RATE, RATE], "a clear is no news, the rate told again is");
}

#[test]
fn a_teller_says_the_rate_at_the_start_every_two_seconds_and_again_behind_a_flush() {
    let rate = |secs: u64| Bitrate { bits_per_second: 1_000 + secs, kind: RateKind::Local };
    let heard = run_with(&INLINE, None, Some(rate::Teller::default()), 100);
    assert_eq!(heard.told, [rate(0), rate(2)]);
    let heard = run_with(&INLINE, Some(20), Some(rate::Teller::default()), 100);
    assert_eq!(heard.told[..2], [rate(0), rate(0)], "{:?}", heard.told);
}
