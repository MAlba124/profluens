//! An encoder on its decoder's thread (Passive), each picture read back on the pass after it
//! went to the GPU, codes what one on its own thread codes, every picture once and in order.
//! Skips without VA-API H.264 decode and the encoder.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::element::{Element, SchedHint};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc, VaapiH265Enc};

use chain::{Codec, Collect, UnitSrc};

const IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");

fn encode(hevc: bool, inline: bool) -> Vec<(Timestamp, Vec<u8>)> {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let ch = GpuFrameChannel::new();
    let src = p.add(UnitSrc::new(Codec::H264.units(IPB)));
    let dec = p.add(VaapiH264Dec::new_zerocopy(Arc::clone(&ch)));
    let enc: Box<dyn Element> = match hevc {
        true => Box::new(VaapiH265Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000)),
        false => Box::new(VaapiH264Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000)),
    };
    let enc = p.add_boxed(enc);
    let sink = p.add(Collect(tx));
    if inline {
        p.set_element_sched(enc, SchedHint::Passive);
    }
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("decoder to encoder");
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(enc, 1 << 20, 8);
    p.run().expect("the run ends");
    let w = chain::warnings(&p);
    assert!(w.is_empty(), "warned {w:?}");
    drop(p);
    rx.try_iter().collect()
}

fn check(test: &str, hevc: bool) {
    let family = if hevc { "h265/annexb" } else { "h264/annexb" };
    if !pf_vaapi::probe().is_some_and(|c| c.supports("h264/annexb") && c.supports_encode(family)) {
        eprintln!("skip {test}: no VA-API H.264 decode and the encoder");
        return;
    }
    let threaded = encode(hevc, false);
    assert_eq!(threaded.len(), Codec::H264.units(IPB).len(), "{test} every picture");
    assert_eq!(encode(hevc, true), threaded, "{test} inline codes the same, in order");
}

#[test]
fn h264_inline_codes_what_its_own_thread_codes() {
    check("h264_inline", false);
}

#[test]
fn h265_inline_codes_what_its_own_thread_codes() {
    check("h265_inline", true);
}
