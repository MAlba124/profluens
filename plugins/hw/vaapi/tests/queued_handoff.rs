//! A producer that hands its pictures on as queued, the dma-buf's fence ordering the consumer's
//! reads, codes what one that waits for each picture codes, byte for byte, with the encoder
//! keeping one to three pictures on the GPU, on its own thread and inline on the decoder's, with
//! and without vaapipostproc between. Skips without VA-API H.264 decode and encode.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::{FrameRx, GpuFrameChannel, GpuFrameHeader, Handoff, wait_readable};
use pf_vaapi::vpp::{Target, VaapiPostProc};
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc};

use chain::{Codec, Collect, UnitSrc};

const IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");
/// The fixture over again, so the surfaces and the in-flight ring cycle.
const LOOPS: usize = 6;

#[derive(Clone, Copy)]
struct Shape {
    handoff: Handoff,
    depth: usize,
    inline: bool,
    vpp: bool,
    /// A pipeline with a reactor, the encoder woken by each picture's fence instead of a timer.
    reactor: bool,
}

fn units() -> Vec<Vec<u8>> {
    let one = Codec::H264.units(IPB);
    (0..LOOPS).flat_map(|_| one.clone()).collect()
}

struct Out {
    frames: Vec<(Timestamp, Vec<u8>)>,
    dec_waits: u64,
    vpp_waits: u64,
    enc_waits: u64,
    enc_polls: u64,
    /// Coded buffers the encoder made, one a picture it keeps in flight, reused after.
    enc_coded_bufs: u64,
}

fn encode(s: Shape) -> Out {
    let (tx, rx) = mpsc::channel();
    let mut p = if s.reactor { pf_reactor::pipeline() } else { Pipeline::new() };
    p.set_pool(256 * 1024, 16);
    let (a, b) = (GpuFrameChannel::with_handoff(s.handoff), GpuFrameChannel::with_handoff(s.handoff));
    let src = p.add(UnitSrc::new(units()));
    let dec = VaapiH264Dec::new_zerocopy(Arc::clone(&a));
    let dec_stats = dec.stats();
    let dec = p.add(dec);
    let vpp = s.vpp.then(|| {
        let target = Target { max_width: 320, max_height: 240, depth: 8 };
        let v = VaapiPostProc::new_zerocopy(target, Arc::clone(&a))
            .with_fixed_size()
            .with_gpu_output(Arc::clone(&b));
        let st = v.stats();
        (p.add(v), st)
    });
    let enc_in = if s.vpp { b } else { a };
    let enc = VaapiH264Enc::new_zerocopy(enc_in).with_fps(25, 1).with_qp(26).with_gop(30).with_depth(s.depth);
    let enc_stats = enc.stats();
    let enc = p.add(enc);
    let sink = p.add(Collect(tx));
    if s.inline {
        if let Some((vpp, _)) = &vpp {
            p.set_element_sched(*vpp, SchedHint::Passive);
        }
        p.set_element_sched(enc, SchedHint::Passive);
    }
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    match &vpp {
        Some((vpp, _)) => {
            p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
            p.link(vpp.src(VaapiPostProc::OUT), enc.sink(SinkPad(0))).expect("vpp to encoder");
        }
        None => {
            p.link(dec.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("decoder to encoder");
        }
    }
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    if let Some((vpp, _)) = &vpp {
        p.set_element_pool(*vpp, 256 * 1024, 16);
    }
    p.set_element_pool(enc, 1 << 20, 8);
    p.run().expect("the run ends");
    let w = chain::warnings(&p);
    assert!(w.is_empty(), "warned {w:?}");
    let vpp_waits = vpp.as_ref().map_or(0, |(_, st)| st.snapshot().waits);
    drop(p);
    let enc = enc_stats.snapshot();
    Out {
        frames: rx.try_iter().collect(),
        dec_waits: dec_stats.snapshot().waits,
        vpp_waits,
        enc_waits: enc.waits,
        enc_polls: enc.polls,
        enc_coded_bufs: enc.coded_bufs,
    }
}

fn supported(test: &str) -> bool {
    let ok = pf_vaapi::probe()
        .is_some_and(|c| c.supports("h264/annexb") && c.supports_encode("h264/annexb"));
    if !ok {
        eprintln!("skip {test}: no VA-API H.264 decode and encode");
    }
    ok
}

fn check(test: &str, vpp: bool) {
    if !supported(test) {
        return;
    }
    let base = encode(Shape { handoff: Handoff::Done, depth: 1, inline: false, vpp, reactor: false });
    assert_eq!(base.frames.len(), units().len(), "{test} every picture");
    assert_eq!(base.enc_coded_bufs, 1, "{test} one picture in flight, one coded buffer");
    assert!(base.dec_waits > 0, "{test} the decoder waits for each picture it hands on");
    if vpp {
        assert!(base.vpp_waits > 0, "{test} vaapipostproc waits for each picture it hands on");
    }
    for inline in [false, true] {
        for depth in 1..=3 {
            let s = Shape { handoff: Handoff::Queued, depth, inline, vpp, reactor: false };
            let out = encode(s);
            assert_eq!(out.frames, base.frames, "{test} queued, depth {depth}, inline {inline}");
            assert_eq!((out.dec_waits, out.vpp_waits), (0, 0), "{test} nobody waited, depth {depth}");
            // the ring is read without waiting, only the end drains it waiting for each picture
            assert!(out.enc_waits <= depth as u64, "{test} the encoder waited {} times", out.enc_waits);
            let made = out.enc_coded_bufs;
            assert!(
                (1..=depth as u64).contains(&made),
                "{test} {made} coded buffers at depth {depth}"
            );
        }
    }
}

/// With a reactor the encoder is woken by each picture's fence, so it polls for almost none
/// of them. The pictures still code the same.
#[test]
fn a_reactor_wakes_the_encoder_on_each_fence() {
    let test = "fenced";
    if !supported(test) {
        return;
    }
    let base = encode(Shape { handoff: Handoff::Done, depth: 1, inline: false, vpp: false, reactor: false });
    for inline in [false, true] {
        let polled = encode(Shape { handoff: Handoff::Queued, depth: 2, inline, vpp: false, reactor: false });
        let fenced = encode(Shape { handoff: Handoff::Queued, depth: 2, inline, vpp: false, reactor: true });
        assert_eq!(fenced.frames, base.frames, "{test} inline {inline} codes the same");
        assert!(
            fenced.enc_polls * 4 < polled.enc_polls.max(4),
            "{test} inline {inline} polled {} times against {} on a timer",
            fenced.enc_polls,
            polled.enc_polls
        );
    }
}

#[test]
fn queued_decode_to_encode_codes_what_waiting_codes() {
    check("queued_dec_enc", false);
}

#[test]
fn queued_decode_through_vaapipostproc_codes_what_waiting_codes() {
    check("queued_dec_vpp_enc", true);
}

static GPU_TAKEN: [OfferDesc; 1] = [OfferDesc::any("video/gpu")];
static GPU_SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &GPU_TAKEN }];
static GPU_SINK_DESC: ElementDesc = ElementDesc {
    name: "gpucollect",
    sinks: &GPU_SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// Takes each frame off the channel and waits for it to be readable, the result sent on.
struct GpuCollect {
    channel: Arc<GpuFrameChannel>,
    frames: Option<FrameRx>,
    out: mpsc::Sender<(u64, Result<bool, i32>)>,
}

impl Element for GpuCollect {
    fn desc(&self) -> &'static ElementDesc {
        &GPU_SINK_DESC
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.frames = self.channel.take_rx();
        Ok(())
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(b) = inputs.pop() {
            let Some(h) = GpuFrameHeader::from_bytes(b.memory.data()) else { continue };
            if let Some(frame) = self.frames.as_mut().and_then(|f| f.take(h.token)) {
                let _ = self.out.send((h.token, wait_readable(&frame.fds, 1000)));
            }
        }
        Ok(Flow::Ok)
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        if let Some(f) = self.frames.take() {
            self.channel.put_rx(f);
        }
    }
}

/// A frame handed on as queued is readable once its writer is done, which the dma-buf says.
#[test]
fn a_queued_frame_becomes_readable() {
    let test = "queued_readable";
    if !pf_vaapi::probe().is_some_and(|c| c.supports("h264/annexb")) {
        eprintln!("skip {test}: no VA-API H.264 decode");
        return;
    }
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let ch = GpuFrameChannel::queued();
    let src = p.add(UnitSrc::new(Codec::H264.units(IPB)));
    let dec = p.add(VaapiH264Dec::new_zerocopy(Arc::clone(&ch)));
    let (tx, rx) = mpsc::channel();
    let sink = p.add(GpuCollect { channel: Arc::clone(&ch), frames: None, out: tx });
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("decoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.run().expect("the run ends");
    let w = chain::warnings(&p);
    assert!(w.is_empty(), "{test} warned {w:?}");
    drop(p);
    let seen: Vec<(u64, Result<bool, i32>)> = rx.try_iter().collect();
    assert_eq!(seen.len(), Codec::H264.units(IPB).len(), "{test} every picture");
    for (token, readable) in seen {
        assert_eq!(readable, Ok(true), "{test} frame {token} readable within a second");
    }
}
