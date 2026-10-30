//! Scheduler regressions in pause and resume delivery, and stop latency.
//! Bounds are loose so they catch tick quantisation, not jitter.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

// Consumes one buffer per process() call.
static SLOW_PASSIVE_DESC: ElementDesc = ElementDesc {
    name: "slowpassive",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
static SLOW_ACTIVE_DESC: ElementDesc = ElementDesc {
    name: "slowactive",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct SlowSink {
    passive: bool,
    n: Arc<AtomicU64>,
}
impl SlowSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for SlowSink {
    fn desc(&self) -> &'static ElementDesc {
        if self.passive { &SLOW_PASSIVE_DESC } else { &SLOW_ACTIVE_DESC }
    }
    fn process(&mut self, _ctx: &mut Ctx, mut i: Inputs<'_>) -> Result<Flow, Error> {
        if i.pop().is_some() {
            self.n.fetch_add(1, Ordering::Release);
        }
        Ok(Flow::Ok)
    }
}

static IDLE_SRC_DESC: ElementDesc = ElementDesc {
    name: "idlesrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
struct IdleSrc(Arc<AtomicBool>);
impl IdleSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for IdleSrc {
    fn desc(&self) -> &'static ElementDesc {
        &IDLE_SRC_DESC
    }
    fn process(&mut self, _c: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
        self.0.store(true, Ordering::Release);
        Ok(Flow::Ok) // never produces: the group idles forever
    }
}

#[test]
fn stop_wakes_idle_parked_groups_at_once() {
    let ran = Arc::new(AtomicBool::new(false));
    let n = Arc::new(AtomicU64::new(0));
    let mut p = Pipeline::new();
    let src = p.add(IdleSrc(Arc::clone(&ran)));
    let snk = p.add(SlowSink { passive: false, n });
    p.link(src.src(IdleSrc::OUT), snk.sink(SlowSink::IN)).expect("link");
    let stop = p.control();
    let h = std::thread::spawn(move || p.run());
    while !ran.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(30)); // let everyone settle into park_idle
    let t0 = Instant::now();
    stop.stop();
    let _ = h.join().expect("join");
    let dt = t0.elapsed();
    println!("scheduler: stop -> run() returned in {dt:?}");
    // Under the 10 ms backstop tick proves the wake is event-driven.
    assert!(dt < Duration::from_millis(8), "stop took {dt:?} (park-tick quantised?)");
}

#[test]
fn stop_wakes_paused_groups_at_once() {
    let ran = Arc::new(AtomicBool::new(false));
    let n = Arc::new(AtomicU64::new(0));
    let mut p = Pipeline::new();
    let src = p.add(IdleSrc(Arc::clone(&ran)));
    let snk = p.add(SlowSink { passive: false, n });
    p.link(src.src(IdleSrc::OUT), snk.sink(SlowSink::IN)).expect("link");
    let stop = p.control();
    let pause = p.control();
    let h = std::thread::spawn(move || p.run());
    while !ran.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    pause.pause();
    std::thread::sleep(Duration::from_millis(150));
    let t0 = Instant::now();
    stop.stop();
    let _ = h.join().expect("join");
    let dt = t0.elapsed();
    println!("scheduler: stop-while-paused -> run() returned in {dt:?}");
    // The pause gate backstop is 100 ms.
    assert!(dt < Duration::from_millis(50), "stop-while-paused took {dt:?}");
}

static RESUME_SRC_DESC: ElementDesc = ElementDesc {
    name: "resumesrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
struct ResumeSrc {
    seen_resume: Arc<AtomicU64>,
    seen_paused: Arc<AtomicU64>,
    ran: Arc<AtomicBool>,
}
impl ResumeSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for ResumeSrc {
    fn desc(&self) -> &'static ElementDesc {
        &RESUME_SRC_DESC
    }
    fn process(&mut self, _c: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
        self.ran.store(true, Ordering::Release);
        Ok(Flow::Ok)
    }
    fn event(&mut self, _c: &mut Ctx, e: &Event) -> Result<(), Error> {
        match e {
            Event::Resumed => {
                self.seen_resume.fetch_add(1, Ordering::Release);
            }
            Event::Paused => {
                self.seen_paused.fetch_add(1, Ordering::Release);
            }
            _ => {}
        }
        Ok(())
    }
}

// Every delivered Paused needs exactly one Resumed.
#[test]
fn paused_and_resumed_deliveries_balance() {
    const CYCLES: u64 = 60;
    let seen_r = Arc::new(AtomicU64::new(0));
    let seen_p = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let n = Arc::new(AtomicU64::new(0));
    let mut p = Pipeline::new();
    let src = p.add(ResumeSrc {
        seen_resume: Arc::clone(&seen_r),
        seen_paused: Arc::clone(&seen_p),
        ran: Arc::clone(&ran),
    });
    let snk = p.add(SlowSink { passive: false, n });
    p.link(src.src(ResumeSrc::OUT), snk.sink(SlowSink::IN)).expect("link");
    let stop = p.control();
    let pause = p.control();
    let h = std::thread::spawn(move || p.run());
    while !ran.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    for i in 0..CYCLES {
        pause.pause();
        std::thread::sleep(Duration::from_micros(200 + (i % 30) * 500));
        pause.resume();
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(50));
    let (np, nr) = (seen_p.load(Ordering::Acquire), seen_r.load(Ordering::Acquire));
    stop.stop();
    let _ = h.join().expect("join");
    println!("scheduler: {CYCLES} pause/resume cycles -> {np} Paused, {nr} Resumed delivered");
    // Balance is the invariant. A pair inside one process() is legitimately coalesced.
    assert_eq!(np, nr, "every delivered Paused needs its Resumed (got {np} / {nr})");
    assert!(np > 0, "no pause was observed at all, the test exercised nothing");
}

#[test]
fn resume_is_delivered_promptly() {
    let seen = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let n = Arc::new(AtomicU64::new(0));
    let mut p = Pipeline::new();
    let src = p.add(ResumeSrc {
        seen_resume: Arc::clone(&seen),
        seen_paused: Arc::new(AtomicU64::new(0)),
        ran: Arc::clone(&ran),
    });
    let snk = p.add(SlowSink { passive: false, n });
    p.link(src.src(ResumeSrc::OUT), snk.sink(SlowSink::IN)).expect("link");
    let stop = p.control();
    let pause = p.control();
    let h = std::thread::spawn(move || p.run());
    while !ran.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut worst = Duration::ZERO;
    let mut over_1ms = 0;
    for i in 0..200u64 {
        pause.pause();
        std::thread::sleep(Duration::from_millis(3));
        let t0 = Instant::now();
        pause.resume();
        while seen.load(Ordering::Acquire) <= i {
            std::hint::spin_loop();
        }
        let dt = t0.elapsed();
        if dt > Duration::from_millis(1) {
            over_1ms += 1;
        }
        worst = worst.max(dt);
    }
    println!("scheduler: {over_1ms}/200 resumes took > 1 ms");
    stop.stop();
    let _ = h.join().expect("join");
    println!("scheduler: worst resume -> Event::Resumed latency {worst:?}");
    assert!(worst < Duration::from_millis(8), "resume latency {worst:?}");
}

// A resume landing in park_idle after a progressing pass skips the pause gate.
static BUSY_SRC_DESC: ElementDesc = ElementDesc {
    name: "busysrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
struct BusySrc;
impl BusySrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for BusySrc {
    fn desc(&self) -> &'static ElementDesc {
        &BUSY_SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
        let Some(mut b) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        b.memory.set_len(0);
        ctx.out(Self::OUT).push(b);
        Ok(Flow::Ok)
    }
}

static BUSY_SINK_DESC: ElementDesc = ElementDesc {
    name: "busysink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
struct BusySink {
    paused: bool,
    seen_p: Arc<AtomicU64>,
    seen_r: Arc<AtomicU64>,
    ran: Arc<AtomicBool>,
}
impl BusySink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for BusySink {
    fn desc(&self) -> &'static ElementDesc {
        &BUSY_SINK_DESC
    }
    fn process(&mut self, _c: &mut Ctx, mut i: Inputs<'_>) -> Result<Flow, Error> {
        self.ran.store(true, Ordering::Release);
        if self.paused {
            return Ok(Flow::Ok); // the pipewire-sink pattern: stage, don't render
        }
        while i.pop().is_some() {}
        Ok(Flow::Ok)
    }
    fn event(&mut self, _c: &mut Ctx, e: &Event) -> Result<(), Error> {
        match e {
            Event::Paused => {
                self.paused = true;
                self.seen_p.fetch_add(1, Ordering::Release);
            }
            Event::Resumed => {
                self.paused = false;
                self.seen_r.fetch_add(1, Ordering::Release);
            }
            _ => {}
        }
        Ok(())
    }
}

#[test]
fn paused_and_resumed_balance_on_a_busy_pipeline() {
    const CYCLES: u64 = 40;
    let seen_p = Arc::new(AtomicU64::new(0));
    let seen_r = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let mut p = Pipeline::new();
    p.set_pool(64, 32);
    let src = p.add(BusySrc);
    let snk = p.add(BusySink {
        paused: false,
        seen_p: Arc::clone(&seen_p),
        seen_r: Arc::clone(&seen_r),
        ran: Arc::clone(&ran),
    });
    p.link(src.src(BusySrc::OUT), snk.sink(BusySink::IN)).expect("link");
    let stop = p.control();
    let pause = p.control();
    let h = std::thread::spawn(move || p.run());
    while !ran.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    for i in 0..CYCLES {
        pause.pause();
        std::thread::sleep(Duration::from_micros(200 + (i % 40) * 400));
        pause.resume();
        std::thread::sleep(Duration::from_millis(3));
    }
    std::thread::sleep(Duration::from_millis(50));
    let (np, nr) = (seen_p.load(Ordering::Acquire), seen_r.load(Ordering::Acquire));
    stop.stop();
    let _ = h.join().expect("join");
    println!(
        "scheduler: {CYCLES} pause/resume cycles on a busy pipeline -> {np} Paused, {nr} Resumed"
    );
    assert_eq!(np, nr, "every delivered Paused needs its Resumed (got {np} / {nr})");
    assert!(np > 0, "no pause was observed at all, the test exercised nothing");
}

// A sink busy in a long process() when the resume lands.
static SLOWPROC_SINK_DESC: ElementDesc = ElementDesc {
    name: "slowprocsink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
struct SlowProcSink {
    seen_p: Arc<AtomicU64>,
    seen_r: Arc<AtomicU64>,
    ran: Arc<AtomicBool>,
}
impl SlowProcSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for SlowProcSink {
    fn desc(&self) -> &'static ElementDesc {
        &SLOWPROC_SINK_DESC
    }
    fn process(&mut self, _c: &mut Ctx, mut i: Inputs<'_>) -> Result<Flow, Error> {
        self.ran.store(true, Ordering::Release);
        if i.pop().is_some() {
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(8) {
                std::hint::spin_loop();
            }
        }
        Ok(Flow::Ok)
    }
    fn event(&mut self, _c: &mut Ctx, e: &Event) -> Result<(), Error> {
        match e {
            Event::Paused => {
                self.seen_p.fetch_add(1, Ordering::Release);
            }
            Event::Resumed => {
                self.seen_r.fetch_add(1, Ordering::Release);
            }
            _ => {}
        }
        Ok(())
    }
}

#[test]
fn resume_reaches_a_sink_busy_in_process() {
    const CYCLES: u64 = 30;
    let seen_p = Arc::new(AtomicU64::new(0));
    let seen_r = Arc::new(AtomicU64::new(0));
    let ran = Arc::new(AtomicBool::new(false));
    let mut p = Pipeline::new();
    p.set_pool(64, 32);
    let src = p.add(BusySrc);
    let snk = p.add(SlowProcSink {
        seen_p: Arc::clone(&seen_p),
        seen_r: Arc::clone(&seen_r),
        ran: Arc::clone(&ran),
    });
    p.link(src.src(BusySrc::OUT), snk.sink(SlowProcSink::IN)).expect("link");
    let stop = p.control();
    let pause = p.control();
    let h = std::thread::spawn(move || p.run());
    while !ran.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..CYCLES {
        pause.pause();
        std::thread::sleep(Duration::from_millis(25));
        pause.resume();
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(50));
    let (np, nr) = (seen_p.load(Ordering::Acquire), seen_r.load(Ordering::Acquire));
    stop.stop();
    let _ = h.join().expect("join");
    println!("scheduler: {CYCLES} pause/resume cycles, busy sink -> {np} Paused, {nr} Resumed");
    assert_eq!(np, nr, "every delivered Paused needs its Resumed (got {np} / {nr})");
    assert!(np > 0, "no pause was observed at all, the test exercised nothing");
}
