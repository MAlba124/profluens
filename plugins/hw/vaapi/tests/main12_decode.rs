//! HEVC Main 12 decoded zero-copy leaves as P010 twins from the decode call: the picture
//! ffmpeg makes of it at 10 bits, and shrunk the same as vaapipostproc shrinks it. Skips without
//! Main 12 decode processing.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::VaapiH265Dec;
use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};

use chain::{Codec, Collect, UnitSrc};

const MAIN12: &[u8] = include_bytes!("fixtures/main12_128x128.hevc");
/// Its first picture as ffmpeg decodes it, P010.
const FIRST: &[u8] = include_bytes!("fixtures/main12_128x128_f0.p010");

fn run(shrink: Option<Target>, target: Target) -> (Vec<(Timestamp, Vec<u8>)>, Vec<String>) {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(Codec::H265.units(MAIN12)));
    let ch = GpuFrameChannel::new();
    let dec = VaapiH265Dec::new_zerocopy(Arc::clone(&ch));
    let dec = p.add(match shrink {
        Some(t) => dec.with_shrink(t, true),
        None => dec,
    });
    let vpp = p.add(VaapiPostProc::new_zerocopy(target, ch).with_fixed_size());
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(vpp.src(VaapiPostProc::OUT), sink.sink(SinkPad(0))).expect("vpp to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(vpp, 256 * 1024, 16);
    p.run().expect("the run ends");
    let w = chain::warnings(&p);
    drop(p);
    let mut frames: Vec<_> = rx.try_iter().collect();
    frames.sort_by_key(|f| f.0);
    (frames, w)
}

fn psnr10(a: &[u8], b: &[u8], n: usize) -> f64 {
    let s = |x: &[u8], i: usize| f64::from(u16::from_le_bytes([x[2 * i], x[2 * i + 1]]) >> 6);
    let mse = (0..n).map(|i| (s(a, i) - s(b, i)).powi(2)).sum::<f64>() / n as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (1023.0f64 * 1023.0 / mse).log10() }
}

fn main12(test: &str) -> bool {
    let ok = pf_vaapi::probe()
        .is_some_and(|c| c.decodes_profile("HEVCMain12") && c.shrinks("h265/annexb"));
    if !ok {
        eprintln!("skip {test}: no HEVC Main 12 decode processing");
    }
    ok
}

#[test]
fn main12_decodes_to_the_picture_ffmpeg_makes() {
    if !main12("main12_decodes") {
        return;
    }
    let (frames, w) = run(None, Target { max_width: 128, max_height: 128, depth: 10 });
    assert!(w.is_empty(), "warned {w:?}");
    assert_eq!(frames.len(), 8, "every picture");
    let db = psnr10(&frames[0].1, FIRST, 128 * 128);
    eprintln!("main12 first picture against ffmpeg at 10 bits, luma {db:.1} dB");
    assert!(db > 45.0, "{db:.1} dB");
}

#[test]
fn main12_shrunk_in_the_decoder_is_the_vpps_picture() {
    if !main12("main12_shrunk") {
        return;
    }
    let t = Target { max_width: 64, max_height: 64, depth: 10 };
    let (vpp, _) = run(None, t);
    let (dec, w) = run(Some(t), t);
    assert!(w.is_empty(), "warned {w:?}");
    assert_eq!((vpp.len(), dec.len()), (8, 8));
    let worst = vpp
        .iter()
        .zip(&dec)
        .map(|(a, b)| psnr10(&a.1, &b.1, 64 * 64))
        .fold(f64::INFINITY, f64::min);
    eprintln!("main12 shrunk by the decoder against the vpp, worst {worst:.1} dB");
    assert!(worst > 40.0, "{worst:.1} dB");
}
