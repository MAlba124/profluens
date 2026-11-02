//! What vaapih265enc codes decodes back to its input on moving pictures: its SPS states every
//! tool the hardware uses. VDEnc and the shader encoder code asymmetric motion partitions
//! whatever the sequence parameters say, an SPS without AMP made every moving picture decode
//! wrong (35 dB and falling against 50). Skips without H.264 decode and HEVC encode.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use pf_harness::Harness;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};
use pf_vaapi::{VaapiH264Dec, VaapiH265Dec, VaapiH265Enc};

use chain::{Codec, Collect, UnitSrc};

/// 80 pictures of a moving test pattern, then bars.
const MOVING: &[u8] = include_bytes!("fixtures/scenecut_320x240.h264");
const W: usize = 320;
const H: usize = 240;

/// What the decoder hands on, as NV12 through vaapipostproc, or as HEVC at QP 26, in order.
fn run(encode: bool) -> Vec<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(1 << 20, 16);
    let ch = GpuFrameChannel::new();
    let src = p.add(UnitSrc::new(Codec::H264.units(MOVING)));
    let dec = p.add(VaapiH264Dec::new_zerocopy(Arc::clone(&ch)));
    let sink = p.add(Collect(tx));
    let next = match encode {
        true => p.add(VaapiH265Enc::new_zerocopy(ch).with_fps(25, 1).with_gop(120).with_qp(26)),
        false => p.add(VaapiPostProc::new_zerocopy(
            Target { max_width: W as u32, max_height: H as u32, depth: 8 },
            ch,
        )),
    };
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), next.sink(SinkPad(0))).expect("decoder on");
    p.link(next.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.run().expect("the run ends");
    let w = chain::warnings(&p);
    assert!(w.is_empty(), "warned {w:?}");
    drop(p);
    let mut out: Vec<_> = rx.try_iter().collect();
    out.sort_by_key(|u| u.0);
    out.into_iter().map(|u| u.1).collect()
}

#[test]
fn moving_pictures_decode_back_to_what_the_encoder_took() {
    let ok = pf_vaapi::probe().is_some_and(|c| {
        c.supports("h264/annexb") && c.supports("h265/annexb") && c.supports_encode("h265/annexb")
    });
    if !ok {
        return eprintln!("skip: no H.264 decode, HEVC decode and encode");
    }
    let input = run(false);
    let coded = run(true);
    assert_eq!((input.len(), coded.len()), (80, 80), "every picture");
    let mut h = Harness::with_pool(VaapiH265Dec::new(), W * H * 2, 8);
    h.fix_sink_format(VaapiH265Dec::IN, "h265/annexb", &[]);
    let mut back = Vec::new();
    for au in &coded {
        let b = h.alloc(au);
        h.push(b).expect("push");
        // copied out, a held buffer pins its pool slot
        back.extend(h.drain_outputs().iter().map(|b| b.memory.data()[..W * H].to_vec()));
    }
    back.extend(h.eos().expect("eos").iter().map(|b| b.memory.data()[..W * H].to_vec()));
    assert_eq!(back.len(), 80, "every picture decodes");
    let mut worst = f64::INFINITY;
    for (i, (a, b)) in input.iter().zip(&back).enumerate() {
        let a = &a[..W * H];
        let mse =
            a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>()
                / (W * H) as f64;
        let db = if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() };
        worst = worst.min(db);
        assert!(db > 38.0, "picture {i} decodes {db:.1} dB from the encoder's input");
    }
    eprintln!("worst luma {worst:.1} dB over 80 pictures");
}
