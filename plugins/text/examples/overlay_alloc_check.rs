//! Steady-state allocation + copy-on-write audit for the **subtitle overlay** path.

#![allow(clippy::disallowed_methods)] // one-shot measurement harness

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::flow::{Queue, Tee};
use profluens_video::format::PixelFormat;
use profluens_video::geometry::frame_size;

use pf_text::SubtitleOverlay;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size(), Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new, Ordering::Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const FRAME_W: u32 = 1920;
const FRAME_H: u32 = 1080;
const N_FRAMES: u64 = 100;

fn main() {
    // One configuration per process, pipeline construction interns into process-global state.
    let cfgs: [(&str, Fan); 4] = [
        ("video ! suboverlay ! sink", Fan::None),
        ("video ! tee ! (overlay | drain)", Fan::TeeDrain),
        ("video ! tee ! (overlay | HOLD)", Fan::TeeRetain),
        ("video ! suboverlay ! sink  [src holds a DPB ref]", Fan::SrcHold),
    ];
    let arg: Vec<String> = std::env::args().collect();
    let Some(sel) = arg.get(1).and_then(|a| a.parse::<usize>().ok()) else {
        for i in 0..cfgs.len() * 2 {
            let out =
                std::process::Command::new(&arg[0]).arg(i.to_string()).output().expect("re-exec");
            print!("{}", String::from_utf8_lossy(&out.stdout));
        }
        println!(
            "\nframe = {} bytes I420 {}x{}",
            frame_size(PixelFormat::I420, FRAME_W, FRAME_H),
            FRAME_W,
            FRAME_H
        );
        return;
    };
    let (label, tee) = cfgs[sel / 2];
    let caption = sel % 2 == 1;
    let r = run(tee, caption);
    let frames = r.sink_ptrs.len().max(1);
    println!(
        "{label:50}  caption={caption:5}  frames={:4}  allocs={:5} ({:5.1}/frame)  \
         heap B/frame={:8.0}  painted={:4}  CoW={:4} ({:5.1}%)  {:7.2} ms/frame",
        r.sink_ptrs.len(),
        r.allocs,
        r.allocs as f64 / frames as f64,
        r.bytes as f64 / frames as f64,
        r.painted,
        r.cow,
        100.0 * r.cow as f64 / frames as f64,
        r.ms / frames as f64,
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fan {
    None,
    TeeDrain,
    TeeRetain,
    SrcHold,
}

#[derive(Default)]
struct Report {
    allocs: usize,
    bytes: usize,
    cow: usize,
    ms: f64,
    painted: usize,
    sink_ptrs: Vec<usize>,
}

fn run(with_tee: Fan, caption: bool) -> Report {
    let probe: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_ptrs: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));

    let mut p = Pipeline::new();
    p.set_pool(frame_size(PixelFormat::I420, FRAME_W, FRAME_H), 16);
    let videosrc = p.add(VideoSrc::new(Arc::clone(&probe), with_tee == Fan::SrcHold));
    let overlay = p.add(SubtitleOverlay::new());
    let painted = Arc::new(AtomicUsize::new(0));
    let snk = p.add(PtrSink { seen: Arc::clone(&sink_ptrs), painted: Arc::clone(&painted) });

    if matches!(with_tee, Fan::TeeDrain | Fan::TeeRetain) {
        let tee = p.add(Tee::new(2));
        let q = p.add(Queue::new());
        let drain = p.add(NullSink {
            retain: if with_tee == Fan::TeeRetain { Some(Vec::new()) } else { None },
        });
        p.link(videosrc.src(VideoSrc::OUT), tee.sink(Tee::IN)).expect("src->tee");
        p.link(tee.src(Tee::OUTS[0]), overlay.sink(SubtitleOverlay::VIDEO))
            .expect("tee->overlay.video");
        p.link(tee.src(Tee::OUTS[1]), q.sink(Queue::IN)).expect("tee->queue");
        p.link(q.src(Queue::OUT), drain.sink(NullSink::IN)).expect("queue->null");
    } else {
        p.link(videosrc.src(VideoSrc::OUT), overlay.sink(SubtitleOverlay::VIDEO))
            .expect("src->overlay.video");
    }
    let textsrc = p.add(TextSrc::new(caption));
    let tq = p.add(Queue::new());
    p.link(textsrc.src(TextSrc::OUT), tq.sink(Queue::IN)).expect("text->queue");
    p.link(tq.src(Queue::OUT), overlay.sink(SubtitleOverlay::TEXT)).expect("queue->overlay.text");
    p.link(overlay.src(SubtitleOverlay::OUT), snk.sink(PtrSink::IN)).expect("overlay->sink");

    let base_a = ALLOCS.load(Ordering::Relaxed);
    let base_b = BYTES.load(Ordering::Relaxed);
    let t0 = std::time::Instant::now();
    p.run().expect("run");
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let allocs = ALLOCS.load(Ordering::Relaxed) - base_a;
    let bytes = BYTES.load(Ordering::Relaxed) - base_b;

    let before = probe.lock().unwrap().clone();
    let after = sink_ptrs.lock().unwrap().clone();
    let cow = before.iter().zip(after.iter()).filter(|(a, b)| a != b).count();
    Report { allocs, bytes, cow, ms, painted: painted.load(Ordering::Relaxed), sink_ptrs: after }
}

static PIXFMTS: [ValueDesc; 1] = [ValueDesc::Id("i420")];
static VIDEO_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Set(&PIXFMTS), preferred: None },
];
static VIDEO_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &VIDEO_FIELDS }];

static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &VIDEO_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "videosrc",
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

struct VideoSrc {
    n: u64,
    seen: Arc<Mutex<Vec<usize>>>,
    dpb: Option<Vec<profluens_core::memory::Memory>>,
}
impl VideoSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new(seen: Arc<Mutex<Vec<usize>>>, hold: bool) -> Self {
        Self { n: 0, seen, dpb: if hold { Some(Vec::new()) } else { None } }
    }
}
impl Element for VideoSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.n = 0;
        ctx.announce_format(
            Self::OUT,
            "video/raw",
            &[
                ("width", ValueDesc::Int(FRAME_W as i64)),
                ("height", ValueDesc::Int(FRAME_H as i64)),
                ("pixfmt", ValueDesc::Id("i420")),
            ],
        );
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.n >= N_FRAMES {
            return Ok(Flow::Eos);
        }
        let need = frame_size(PixelFormat::I420, FRAME_W, FRAME_H);
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        if buf.memory.capacity() < need {
            buf.memory.set_len(0);
            ctx.out(Self::OUT).push(buf);
            return Ok(Flow::Ok);
        }
        buf.memory.as_mut_full()[..need].fill(128);
        buf.memory.set_len(need);
        buf.pts = Timestamp::from_nanos(self.n * 40_000_000);
        self.seen.lock().unwrap().push(buf.memory.data().as_ptr() as usize);
        if let Some(dpb) = self.dpb.as_mut() {
            dpb.push(buf.memory.clone());
            if dpb.len() > 8 {
                dpb.remove(0);
            }
        }
        ctx.out(Self::OUT).push(buf);
        self.n += 1;
        Ok(Flow::Ok)
    }
}

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &VIDEO_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "ptrsink",
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

struct PtrSink {
    seen: Arc<Mutex<Vec<usize>>>,
    painted: Arc<AtomicUsize>,
}
impl PtrSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for PtrSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.seen.lock().unwrap().push(buf.memory.data().as_ptr() as usize);
            let y = &buf.memory.data()[..(FRAME_W * FRAME_H) as usize];
            if y[y.len() * 3 / 4..].iter().any(|&p| p != 128) {
                self.painted.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(Flow::Ok)
    }
}

static NULL_DESC: ElementDesc = ElementDesc {
    name: "nullsink",
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
struct NullSink {
    retain: Option<Vec<profluens_core::buffer::Buffer>>,
}
impl NullSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for NullSink {
    fn desc(&self) -> &'static ElementDesc {
        &NULL_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = inputs.pop() {
            if let Some(held) = self.retain.as_mut() {
                held.push(b);
                if held.len() > 3 {
                    held.remove(0);
                }
            }
        }
        Ok(Flow::Ok)
    }
}

static TEXT_OFFERS: [OfferDesc; 1] = [OfferDesc::any(pf_text::CUE_IR_FAMILY)];
static TEXT_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &TEXT_OFFERS }];
static TEXT_DESC: ElementDesc = ElementDesc {
    name: "textsrc",
    sinks: &[],
    srcs: &TEXT_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};
struct TextSrc {
    sent: bool,
    emit: bool,
}
impl TextSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new(emit: bool) -> Self {
        Self { sent: false, emit }
    }
}
impl Element for TextSrc {
    fn desc(&self) -> &'static ElementDesc {
        &TEXT_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.sent = false;
        ctx.announce_format(Self::OUT, pf_text::CUE_IR_FAMILY, &[]);
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.sent || !self.emit {
            return Ok(Flow::Eos);
        }
        let mut b = pf_subparse::ir::CueBuilder::new();
        b.plain_text("A caption that stays on screen");
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        let n = b.finish(buf.memory.as_mut_full()).expect("a short cue fits");
        buf.memory.set_len(n);
        buf.pts = Timestamp::from_nanos(0);
        buf.duration = Timestamp::from_nanos(N_FRAMES * 40_000_000 + 1_000_000_000);
        ctx.out(Self::OUT).push(buf);
        self.sent = true;
        Ok(Flow::Ok)
    }
}
