//! `opusenc ! opusdec` round trip through the real scheduler, caps and EOS drain included.

#![allow(clippy::disallowed_methods)] // test fixtures, not a hot path

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::SrcPad;
use profluens_core::pipeline::Pipeline;
use profluens_elements::testing::TestSink;

use pf_opus::{OpusDec, OpusEnc};

const RATE: i64 = 48_000;
const CHANNELS: usize = 2;
const FRAMES: usize = 25;
const FRAME_SAMPLES: usize = 960;

static SRC_FIELDS: [FieldDesc; 3] = [
    FieldDesc {
        field: "rate",
        allowed: ConstraintDesc::Set(&[ValueDesc::Int(RATE)]),
        preferred: None,
    },
    FieldDesc {
        field: "channels",
        allowed: ConstraintDesc::Set(&[ValueDesc::Int(CHANNELS as i64)]),
        preferred: None,
    },
    FieldDesc {
        field: "sample",
        allowed: ConstraintDesc::Set(&[ValueDesc::Id("s16")]),
        preferred: None,
    },
];
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SRC_FIELDS }];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "tonesrc",
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

struct ToneSrc {
    bytes: Vec<u8>,
    pos: usize,
}

impl ToneSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new() -> Self {
        let n = FRAMES * FRAME_SAMPLES;
        let mut bytes = Vec::with_capacity(n * CHANNELS * 2);
        for i in 0..n {
            let t = i as f64 / RATE as f64;
            let l = (0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16384.0) as i16;
            let r = (0.3 * (2.0 * std::f64::consts::PI * 660.0 * t).sin() * 16384.0) as i16;
            bytes.extend_from_slice(&l.to_le_bytes());
            bytes.extend_from_slice(&r.to_le_bytes());
        }
        Self { bytes, pos: 0 }
    }
}

impl Element for ToneSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.pos = 0;
        ctx.announce_format(
            Self::OUT,
            "audio/raw",
            &[
                ("rate", ValueDesc::Int(RATE)),
                ("channels", ValueDesc::Int(CHANNELS as i64)),
                ("sample", ValueDesc::Id("s16")),
            ],
        );
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.pos >= self.bytes.len() {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else { return Ok(Flow::Ok) };
        let cap = buf.memory.capacity();
        let n = cap.min(self.bytes.len() - self.pos);
        buf.memory.as_mut_full()[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
        buf.memory.set_len(n);
        self.pos += n;
        ctx.out(Self::OUT).push(buf);
        Ok(Flow::Ok)
    }
}

#[test]
fn tonesrc_opusenc_opusdec_roundtrips_through_scheduler() {
    let (sink, stats) = TestSink::new();
    let mut p = Pipeline::new();
    let src = p.add(ToneSrc::new());
    let enc = p.add(OpusEnc::new());
    let dec = p.add(OpusDec::new());
    let snk = p.add(sink);
    p.link(src.src(ToneSrc::OUT), enc.sink(OpusEnc::IN)).expect("src->opusenc");
    p.link(enc.src(OpusEnc::OUT), dec.sink(OpusDec::IN)).expect("opusenc->opusdec");
    p.link(dec.src(OpusDec::OUT), snk.sink(TestSink::IN)).expect("opusdec->sink");
    p.run().expect("pipeline run");

    assert!(stats.is_done(), "sink saw EOS");
    let input_bytes = (FRAMES * FRAME_SAMPLES * CHANNELS * 2) as u64;
    let frame_bytes = (FRAME_SAMPLES * CHANNELS * 2) as u64;
    let out = stats.bytes();
    assert!(
        out >= input_bytes * 9 / 10 && out <= input_bytes + frame_bytes,
        "decoded {out} bytes vs input {input_bytes}, reframing/emit through the scheduler is off",
    );
}
