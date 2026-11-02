//! An encoder whose session opens in `start` codes what one opened on the first picture codes,
//! and one opened at a size the pictures do not have reopens at theirs. H.264 and HEVC out.
//! Skips without VA-API H.264 decode and the encoder.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::element::Element;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc, VaapiH265Enc};

use chain::{Codec, Collect, UnitSrc};

const IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");

/// The fixture decoded and coded again, the session opened ahead at `ahead` when set.
fn encode(hevc: bool, ahead: Option<(u32, u32)>) -> Vec<(Timestamp, Vec<u8>)> {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let ch = GpuFrameChannel::new();
    let src = p.add(UnitSrc::new(Codec::H264.units(IPB)));
    let dec = p.add(VaapiH264Dec::new_zerocopy(Arc::clone(&ch)));
    let enc: Box<dyn Element> = match (hevc, ahead) {
        (true, Some((w, h))) => Box::new(VaapiH265Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000).with_ahead(w, h)),
        (true, None) => Box::new(VaapiH265Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000)),
        (false, Some((w, h))) => Box::new(VaapiH264Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000).with_ahead(w, h)),
        (false, None) => Box::new(VaapiH264Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000)),
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
    // in coding order, the fixture's B pictures leave the source's stamps out of order
    rx.try_iter().collect()
}

fn check(test: &str, hevc: bool) {
    let family = if hevc { "h265/annexb" } else { "h264/annexb" };
    if !pf_vaapi::probe().is_some_and(|c| c.supports("h264/annexb") && c.supports_encode(family)) {
        eprintln!("skip {test}: no VA-API H.264 decode and the encoder");
        return;
    }
    let lazy = encode(hevc, None);
    assert_eq!(lazy.len(), Codec::H264.units(IPB).len(), "{test} every picture");
    assert_eq!(encode(hevc, Some((320, 240))), lazy, "{test} the same bytes opened ahead");
    // reopened at the pictures' size, a new idr_pic_id the only difference in the bytes
    let codec = if hevc { Codec::H265 } else { Codec::H264 };
    let (want, _) = chain::decode(codec, &lazy);
    let (got, w) = chain::decode(codec, &encode(hevc, Some((640, 480))));
    assert!(w.is_empty(), "{test} the reopened stream decodes clean {w:?}");
    assert!(got == want, "{test} the same pictures after a reopen");
}

#[test]
fn h264_opened_ahead_codes_the_same() {
    check("h264_ahead", false);
}

#[test]
fn h265_opened_ahead_codes_the_same() {
    check("h265_ahead", true);
}
