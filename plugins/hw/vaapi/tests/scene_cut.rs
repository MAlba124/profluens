//! A source's intra picture is a scene change the transcode keys on: the decoder flags it
//! INTRA, the encoder codes an IDR there inside its own GOP, H.264 and HEVC out. Skips without
//! VA-API H.264 decode and both encoders.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::element::Element;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc, VaapiH265Enc};

use chain::{Codec, Collect, UnitSrc};

/// 80 pictures, a cut to colour bars at 40, the source's IDRs at 0 and 40.
const CUT: &[u8] = include_bytes!("fixtures/scenecut_320x240.h264");

/// The indices of the access units the encoder codes as IDRs, HEVC out when `hevc`, a GOP of
/// 120 so its own period puts none past the first.
fn idrs(hevc: bool) -> Vec<usize> {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let ch = GpuFrameChannel::new();
    let src = p.add(UnitSrc::new(Codec::H264.units(CUT)));
    let dec = p.add(VaapiH264Dec::new_zerocopy(Arc::clone(&ch)));
    let enc: Box<dyn Element> = match hevc {
        true => Box::new(VaapiH265Enc::new_zerocopy(ch).with_fps(25, 1).with_gop(120).with_bitrate(1000)),
        false => Box::new(VaapiH264Enc::new_zerocopy(ch).with_fps(25, 1).with_gop(120).with_bitrate(1000)),
    };
    let enc = p.add_boxed(enc);
    let sink = p.add(Collect(tx));
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
    let mut units: Vec<_> = rx.try_iter().collect();
    units.sort_by_key(|u| u.0);
    assert_eq!(units.len(), 80, "every picture");
    let idr = |au: &[u8]| {
        au.windows(4).filter(|w| w[..3] == [0, 0, 1]).any(|w| match hevc {
            true => matches!((w[3] >> 1) & 0x3F, 19 | 20),
            false => w[3] & 0x1F == 5,
        })
    };
    units.iter().enumerate().filter(|(_, u)| idr(&u.1)).map(|(i, _)| i).collect()
}

fn device(test: &str, hevc: bool) -> bool {
    let ok = pf_vaapi::probe().is_some_and(|c| {
        c.supports("h264/annexb") && c.supports_encode(if hevc { "h265/annexb" } else { "h264/annexb" })
    });
    if !ok {
        eprintln!("skip {test}: no VA-API H.264 decode and the encoder");
    }
    ok
}

#[test]
fn h264_keys_on_the_sources_scene_change() {
    if device("h264", false) {
        assert_eq!(idrs(false), vec![0, 40]);
    }
}

#[test]
fn hevc_keys_on_the_sources_scene_change() {
    if device("hevc", true) {
        assert_eq!(idrs(true), vec![0, 40]);
    }
}
