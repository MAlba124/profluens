//! The parameter sets an encoder's head is known by before a session opens are the ones the
//! session writes, H.264 and HEVC 8 and 10 bit, the colour pinned. Skips without VA-API H.264
//! and HEVC Main 10 decode and the encoder.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use profluens_core::element::Element;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_video::color::{Matrix, Primaries, Range, StreamColor, TransferFn};

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::{VaapiH264Dec, VaapiH264Enc, VaapiH265Dec, VaapiH265Enc, h264enc, h265enc};

use chain::{Codec, Collect, UnitSrc};

const IPB: &[u8] = include_bytes!("fixtures/ipb_320x240.h264");
const PQ: &[u8] = include_bytes!("fixtures/hdr10_128x128.hevc");

/// BT.709 limited, what a tone mapped transcode states, not what the fixtures' decoders say.
fn bt709() -> StreamColor {
    StreamColor {
        matrix: Matrix::from_h273(1),
        primaries: Primaries::from_h273(1),
        transfer: TransferFn::from_h273(1),
        range: Some(Range::Limited),
        ..StreamColor::default()
    }
}

/// The NAL units of an Annex B stream, start codes cut.
fn nals(au: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends = starts.iter().skip(1).map(|&s| s - 3).chain([au.len()]);
    // a four byte start code leaves a zero on the unit before
    starts.iter().zip(ends).map(|(&a, b)| au[a..b].strip_suffix(&[0]).unwrap_or(&au[a..b])).collect()
}

/// The parameter set units of `au`, H.264 SPS and PPS or HEVC VPS, SPS and PPS.
fn sets(au: &[u8], hevc: bool) -> Vec<Vec<u8>> {
    nals(au)
        .into_iter()
        .filter(|n| match hevc {
            true => matches!((n[0] >> 1) & 0x3F, 32..=34),
            false => matches!(n[0] & 0x1F, 7 | 8),
        })
        .map(<[u8]>::to_vec)
        .collect()
}

/// The first coded unit of `stream` decoded and coded again by `enc`.
fn first_unit(source: Codec, stream: &[u8], enc: impl FnOnce(Arc<GpuFrameChannel>) -> Box<dyn Element>) -> Vec<u8> {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let ch = GpuFrameChannel::new();
    let src = p.add(UnitSrc::new(source.units(stream)));
    let dec: Box<dyn Element> = match source {
        Codec::H264 => Box::new(VaapiH264Dec::new_zerocopy(Arc::clone(&ch))),
        Codec::H265 => Box::new(VaapiH265Dec::new_zerocopy(Arc::clone(&ch))),
    };
    let dec = p.add_boxed(dec);
    let enc = p.add_boxed(enc(ch));
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
    rx.try_iter().next().expect("a unit").1
}

fn device(test: &str, decode: &str, encode: &str) -> bool {
    let ok = pf_vaapi::probe().is_some_and(|c| c.decodes_profile(decode) && c.supports_encode(encode));
    if !ok {
        eprintln!("skip {test}: no VA-API {decode} decode and {encode} encode");
    }
    ok
}

#[test]
fn the_h264_head_is_the_sessions_sets() {
    if !device("h264_head", "H264High", "h264/annexb") {
        return;
    }
    let au = first_unit(Codec::H264, IPB, |ch| {
        Box::new(VaapiH264Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000).with_low_power(true).with_colour(bt709()))
    });
    let (sps, pps) = h264enc::parameter_sets(320, 240, (25, 1), h264enc::DEFAULT_QP, &bt709());
    assert_eq!(sets(&au, false).len(), 2, "SPS and PPS in band");
    assert_eq!(sets(&au, false), sets(&[sps, pps].concat(), false));
    let (unpinned, _) = h264enc::parameter_sets(320, 240, (25, 1), h264enc::DEFAULT_QP, &StreamColor::default());
    assert_ne!(sets(&au, false)[0], sets(&unpinned, false)[0], "the pinned colour is in the SPS");
}

#[test]
fn the_hevc_head_is_the_sessions_sets() {
    if !device("hevc_head", "H264High", "h265/annexb") {
        return;
    }
    let au = first_unit(Codec::H264, IPB, |ch| {
        Box::new(VaapiH265Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000).with_colour(bt709()))
    });
    let head = h265enc::parameter_sets(320, 240, (25, 1), false, h265enc::DEFAULT_QP, true, true, &bt709()).expect("a head");
    assert_eq!(sets(&au, true).len(), 3, "VPS, SPS and PPS in band");
    assert_eq!(sets(&au, true), sets(&head, true));
}

#[test]
fn the_hevc_main10_head_is_the_sessions_sets() {
    if !device("main10_head", "HEVCMain10", "h265/annexb") {
        return;
    }
    let pq = StreamColor { transfer: TransferFn::from_h273(16), primaries: Primaries::from_h273(9), matrix: Matrix::from_h273(9), ..bt709() };
    let au = first_unit(Codec::H265, PQ, |ch| {
        Box::new(VaapiH265Enc::new_zerocopy(ch).with_fps(25, 1).with_bitrate(1000).with_colour(pq))
    });
    let head = h265enc::parameter_sets(128, 128, (25, 1), true, h265enc::DEFAULT_QP, true, true, &pq).expect("a head");
    assert_eq!(sets(&au, true).len(), 3, "VPS, SPS and PPS in band");
    assert_eq!(sets(&au, true), sets(&head, true));
}
