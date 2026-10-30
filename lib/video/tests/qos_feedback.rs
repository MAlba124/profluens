//! A sink too slow for the stream tells the decoder upstream, which skips what the sink would drop.

// Test code.
#![allow(clippy::disallowed_methods)]

use std::cell::Cell;
use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log::DebugSpec;
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_video::FAMILY;
use profluens_video::decoder::{self, Codec, Out, Picture, VideoDec};
use profluens_video::qos::FrameQos;

const REF: u8 = b'P';
const LEAF: u8 = b'b';

const PERIOD: Timestamp = Timestamp::from_millis(40);
// Half as long again as a frame lasts.
const COST: Timestamp = Timestamp::from_millis(60);
const GROUPS: u64 = 40;

thread_local! {
    static DECODED: Cell<u32> = const { Cell::new(0) };
    static DECODE_COST: std::cell::RefCell<Option<(MockClock, Timestamp)>> = const { std::cell::RefCell::new(None) };
}

struct Gop<const SKIP: bool> {
    window: Vec<Picture>,
}

static PIXFMTS: [ValueDesc; 1] = [ValueDesc::Id("i420")];
static SRC_FIELDS: [FieldDesc; 7] = decoder::src_fields(&PIXFMTS);
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }];
static GOP_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "gop", fields: &decoder::COLOR_FIELDS }];
static GOP_SINKS: [PadDesc; 1] = decoder::sink_pad(&GOP_OFFERS);
static GOP_SRCS: [PadDesc; 1] = decoder::src_pad(&SRC_OFFERS);
static GOP_DESC: ElementDesc =
    decoder::desc("gopdec", &GOP_SINKS, &GOP_SRCS, || Box::new(VideoDec::<Gop<true>>::new()));

impl<const SKIP: bool> Codec for Gop<SKIP> {
    const DESC: &'static ElementDesc = &GOP_DESC;

    fn new() -> Self {
        Self { window: Vec::new() }
    }

    fn reset(&mut self) {
        self.window.clear();
    }

    fn decode(&mut self, unit: &[u8], pts: Timestamp, duration: Timestamp, out: &mut Out<'_>) {
        DECODED.with(|d| d.set(d.get() + 1));
        DECODE_COST.with(|c| c.borrow().iter().for_each(|(clock, cost)| clock.advance(*cost)));
        let planes = [vec![unit[0]; 4], vec![1], vec![2]];
        self.window.push(Picture { width: 2, height: 2, pixfmt: "i420", planes, pts, duration });
        self.release(1, out);
    }

    fn finish(&mut self, out: &mut Out<'_>) {
        self.release(0, out);
    }

    fn disposable(&self, unit: &[u8]) -> bool {
        SKIP && unit[0] == LEAF
    }
}

impl<const SKIP: bool> Gop<SKIP> {
    fn release(&mut self, keep: usize, out: &mut Out<'_>) {
        while self.window.len() > keep {
            let first = (0..self.window.len()).min_by_key(|&i| self.window[i].pts).unwrap();
            let p = self.window.remove(first);
            if !out.decode_only(p.pts) {
                out.push(p);
            }
        }
    }
}

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

static UNIT_OFFERS: [OfferDesc; 1] = [OfferDesc::any("gop")];
static UNIT_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &UNIT_OFFERS }];
static RAW_OFFERS: [OfferDesc; 1] = [OfferDesc::any(FAMILY)];
static RAW_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &RAW_OFFERS }];
static SRC: ElementDesc = desc("unitsrc", &[], &UNIT_PADS);
static SINK: ElementDesc = desc("slowsink", &RAW_PADS, &[]);

fn units() -> Vec<(u8, u64)> {
    let mut v = vec![(REF, 0)];
    for g in 0..GROUPS {
        v.extend([(REF, 3 * g + 3), (LEAF, 3 * g + 1), (LEAF, 3 * g + 2)]);
    }
    v
}

struct UnitSrc {
    units: Vec<(u8, u64)>,
    next: usize,
}

impl Element for UnitSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(&(op, at)) = self.units.get(self.next) {
            let Some(mut buf) = ctx.try_alloc() else {
                return Ok(Flow::Ok);
            };
            buf.memory.as_mut_full()[0] = op;
            buf.memory.set_len(1);
            (buf.pts, buf.duration) = (Timestamp(at * PERIOD.0), PERIOD);
            ctx.out(SrcPad(0)).push(buf);
            self.next += 1;
        }
        Ok(Flow::Eos)
    }
}

struct SlowSink {
    clock: MockClock,
    cost: Timestamp,
    qos: FrameQos,
    preroll: bool,
    admitted: bool,
    shown: Arc<std::sync::atomic::AtomicU32>,
    dropped: Arc<std::sync::atomic::AtomicU32>,
}

impl Element for SlowSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        use std::sync::atomic::Ordering::Relaxed;
        while let Some(pts) = inputs.front().map(|b| b.pts) {
            if !self.preroll && !self.admitted && self.qos.late(ctx, pts, PERIOD.0).is_some() {
                inputs.pop();
                self.dropped.fetch_add(1, Relaxed);
                continue;
            }
            self.admitted = true;
            if !self.preroll && !ctx.due(pts) {
                break;
            }
            inputs.pop();
            self.admitted = false;
            self.clock.advance(self.cost);
            self.qos.shown(ctx, pts, std::mem::take(&mut self.preroll));
            self.shown.fetch_add(1, Relaxed);
        }
        Ok(Flow::Ok)
    }
}

fn play<const SKIP: bool>(render: Timestamp, decode: Timestamp) -> (u32, u32, u32) {
    use std::sync::atomic::Ordering::Relaxed;
    DECODED.with(|d| d.set(0));
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    DECODE_COST.with(|c| *c.borrow_mut() = Some((clock.clone(), decode)));
    let mut p = Pipeline::with(DebugSpec::default(), false, Arc::new(clock.clone()));
    p.set_pool(64, 64);
    let src = p.add(UnitSrc { units: units(), next: 0 });
    let dec = p.add(VideoDec::<Gop<SKIP>>::new());
    p.set_element_pool(dec, 64, 4);
    let (shown, dropped) = (Arc::default(), Arc::default());
    let sink = p.add(SlowSink {
        clock: clock.clone(),
        cost: render,
        qos: FrameQos::new(),
        preroll: true,
        admitted: false,
        shown: Arc::clone(&shown),
        dropped: Arc::clone(&dropped),
    });
    p.link(src.src(SrcPad(0)), dec.sink(VideoDec::<Gop<SKIP>>::IN)).expect("link");
    p.link(dec.src(VideoDec::<Gop<SKIP>>::OUT), sink.sink(SinkPad(0))).expect("link");
    let mut run = p.step(Arc::new(clock.clone())).expect("step");
    for _ in 0..100_000 {
        if let Some(g) = (0..run.groups()).find(|g| run.ready(*g)) {
            run.pass(g).expect("pass");
        } else if let Some(at) = run.next_deadline() {
            clock.advance(Timestamp(at.0.saturating_sub(clock.now().0).max(1)));
        } else {
            break;
        }
    }
    run.finish().expect("the run ends");
    (shown.load(Relaxed), dropped.load(Relaxed), DECODED.with(Cell::get))
}

#[test]
fn a_slow_sink_makes_the_decoder_skip_leaves_and_drops_less() {
    let total = units().len() as u32;
    let (shown_all, dropped_all, decoded_all) = play::<false>(COST, Timestamp::ZERO);
    assert_eq!(decoded_all, total, "a codec with nothing disposable decodes every unit");
    assert_eq!(shown_all + dropped_all, total, "every frame reached the sink");
    assert!(dropped_all > total / 5, "the sink is too slow, it dropped {dropped_all} of {total}");

    let (shown, dropped, decoded) = play::<true>(COST, Timestamp::ZERO);
    println!(
        "slow sink, no feedback {shown_all} shown {dropped_all} dropped, with {shown} shown {dropped} dropped {decoded} decoded"
    );
    assert!(decoded < total, "the decoder skipped no leaf");
    assert!(
        dropped * 2 < dropped_all,
        "the sink dropped {dropped}, {dropped_all} with no feedback"
    );
    assert!(shown * 3 > shown_all * 2, "the sink showed {shown}, {shown_all} with no feedback");
    assert_eq!(
        shown + dropped,
        decoded,
        "a skipped unit is no frame, a decoded one reaches the sink"
    );
}

#[test]
fn a_decoder_too_slow_for_the_stream_shows_more_when_it_skips_leaves() {
    let decode = Timestamp::from_millis(45);
    let (shown_all, dropped_all, _) = play::<false>(Timestamp::from_millis(1), decode);
    let (shown, dropped, decoded) = play::<true>(Timestamp::from_millis(1), decode);
    println!(
        "slow decoder, no feedback {shown_all} shown {dropped_all} dropped, with {shown} shown {dropped} dropped {decoded} decoded"
    );
    assert!(decoded < units().len() as u32, "the decoder skipped no leaf");
    assert!(shown > shown_all, "the sink showed {shown}, {shown_all} with no feedback");
    assert!(dropped < dropped_all, "the sink dropped {dropped}, {dropped_all} with no feedback");
}
