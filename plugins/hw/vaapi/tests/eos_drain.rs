//! An encoder whose output pool is a slot or two sends every frame it staged before the end goes
//! downstream. Skipped without a VA device that encodes the family.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::{Settings, VaapiAv1Enc, VaapiH264Enc, VaapiH265Enc, VaapiVp8Enc, VaapiVp9Enc};

const W: u32 = 128;
const H: u32 = 64;
const FRAMES: u64 = 24;
const FRAME_NS: u64 = 40_000_000;

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    inputs: InputPolicy,
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc {
        field: "width",
        allowed: ConstraintDesc::Eq(ValueDesc::Int(W as i64)),
        preferred: None,
    },
    FieldDesc {
        field: "height",
        allowed: ConstraintDesc::Eq(ValueDesc::Int(H as i64)),
        preferred: None,
    },
    FieldDesc {
        field: "pixfmt",
        allowed: ConstraintDesc::Eq(ValueDesc::Id("nv12")),
        preferred: None,
    },
];
static RAW: [OfferDesc; 1] = [OfferDesc { family: "video/raw", fields: &RAW_FIELDS }];
static CODED: [OfferDesc; 5] = [
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc::any("vp8"),
    OfferDesc::any("vp9"),
    OfferDesc::any("av1"),
];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &RAW }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &CODED }];
static SRC_DESC: ElementDesc = desc("rawsrc", &[], &SRC_PADS, InputPolicy::None);
static SINK_DESC: ElementDesc = desc("countsink", &SINK_PADS, &[], InputPolicy::Single);

/// Every frame in one pass, so the encoder stages most of them behind its own small pool.
struct RawSrc(bool);

impl Element for RawSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if std::mem::replace(&mut self.0, true) {
            return Ok(Flow::Eos);
        }
        let len = (W * H * 3 / 2) as usize;
        for i in 0..FRAMES {
            let mut buf = ctx.alloc_exact(len).expect("exact");
            for (p, b) in buf.memory.as_mut_full()[..len].iter_mut().enumerate() {
                *b = ((p as u64 + i * 7) % 220 + 16) as u8;
            }
            buf.memory.set_len(len);
            buf.pts = Timestamp::from_nanos(i * FRAME_NS);
            buf.duration = Timestamp::from_nanos(FRAME_NS);
            ctx.out(SrcPad(0)).push(buf);
        }
        Ok(Flow::Ok)
    }
}

#[derive(Default)]
struct Seen {
    pts: Vec<Timestamp>,
    at_eos: Option<usize>,
}

struct CountSink(Arc<Mutex<Seen>>);

impl Element for CountSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut seen = self.0.lock().expect("seen");
        while let Some(b) = inputs.pop() {
            seen.pts.push(b.pts);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::Eos) {
            let mut seen = self.0.lock().expect("seen");
            seen.at_eos = Some(seen.pts.len());
        }
        Ok(())
    }
}

fn check(test: &str, family: &str, make: fn() -> Box<dyn Element>) {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return;
    };
    if !caps.supports_encode(family) {
        eprintln!("skip {test}: no {family} encode");
        return;
    }
    let want: Vec<Timestamp> = (0..FRAMES).map(|i| Timestamp::from_nanos(i * FRAME_NS)).collect();
    for slots in [1u32, 2, 3] {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut p = Pipeline::new();
        p.set_pool(256 * 1024, 16);
        let src = p.add(RawSrc(false));
        let enc = p.add_boxed(make());
        let sink = p.add(CountSink(Arc::clone(&seen)));
        p.link(src.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("src to encoder");
        p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
        p.set_element_pool(enc, 64 * 1024, slots);
        p.run().expect("the run ends");
        let seen = seen.lock().expect("seen");
        assert_eq!(seen.pts, want, "{test} every frame through {slots} slots, in order");
        assert_eq!(seen.at_eos, Some(FRAMES as usize), "{test} all of them ahead of the end");
        eprintln!("{test}: {FRAMES} frames through {slots} slots");
    }
}

#[test]
fn h264enc_sends_every_staged_frame_before_the_end() {
    check("h264enc_sends_every_staged_frame_before_the_end", "h264/annexb", || {
        Box::new(VaapiH264Enc::new())
    });
}

#[test]
fn h265enc_sends_every_staged_frame_before_the_end() {
    check("h265enc_sends_every_staged_frame_before_the_end", "h265/annexb", || {
        Box::new(VaapiH265Enc::new())
    });
}

#[test]
fn vp8enc_sends_every_staged_frame_before_the_end() {
    check("vp8enc_sends_every_staged_frame_before_the_end", "vp8", || Box::new(VaapiVp8Enc::new()));
}

#[test]
fn vp9enc_sends_every_staged_frame_before_the_end() {
    check("vp9enc_sends_every_staged_frame_before_the_end", "vp9", || {
        Box::new(VaapiVp9Enc::new(Settings::default()))
    });
}

#[test]
fn av1enc_sends_every_staged_frame_before_the_end() {
    check("av1enc_sends_every_staged_frame_before_the_end", "av1", || {
        Box::new(VaapiAv1Enc::new(Settings::default()))
    });
}
