//! A 10 bit HDR10 source decoded and encoded again on the GPU keeps its colour: vaapih265enc
//! states the source's primaries, transfer and matrix in the VUI and its mastering display and
//! light level as SEI on each IDR, and the decoder reads them back. Skips without HEVC Main 10
//! decode and encode.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::mpsc;

use pf_harness::Harness;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_video::color::{ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, StreamColor, TransferFn};

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::{VaapiH265Dec, VaapiH265Enc};

use chain::{Codec, Collect, UnitSrc};

const HDR10: &[u8] = include_bytes!("fixtures/hdr10_128x128.hevc");

/// The fixture's own colour, its mastering display SEI, no light level.
fn source() -> StreamColor {
    StreamColor {
        matrix: Some(Matrix::Bt2020),
        range: Some(Range::Limited),
        transfer: Some(TransferFn::Pq),
        primaries: Some(Primaries::Bt2020),
        hdr: HdrStatic {
            mastering: Mastering {
                primaries: [[34000, 16000], [13250, 34500], [7500, 3000]],
                white: [15635, 16450],
                max_luminance: 40_000_000,
                min_luminance: 50,
            },
            light: ContentLight::default(),
        },
        chroma: None,
        dovi: None,
    }
}

/// `dec ! enc` on dmabufs, the access units the encoder makes.
fn transcode(units: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let ch = GpuFrameChannel::new();
    let src = p.add(UnitSrc::new(units.to_vec()));
    let dec = p.add_boxed(Codec::H265.decoder(Some(&ch)));
    let enc = p.add_boxed(Codec::H265.encoder(Some(&ch)));
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), enc.sink(VaapiH265Enc::IN)).expect("decoder to encoder");
    p.link(enc.src(VaapiH265Enc::OUT), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(enc, 1 << 20, 8);
    p.run().expect("the run ends");
    let w = chain::warnings(&p);
    assert!(w.is_empty(), "warned {w:?}");
    drop(p);
    let mut out: Vec<_> = rx.try_iter().collect();
    out.sort_by_key(|f| f.0);
    out.into_iter().map(|f| f.1).collect()
}

#[test]
fn hdr10_keeps_its_colour_and_metadata_through_the_encoder() {
    let test = "hdr10_keeps_its_colour";
    let ok = pf_vaapi::probe().is_some_and(|c| {
        c.decodes_profile("HEVCMain10") && c.profiles.iter().any(|p| p.name == "HEVCMain10" && (p.enc || p.enc_lp))
    });
    if !ok {
        return eprintln!("skip {test}: no HEVC Main 10 decode and encode");
    }
    let units = Codec::H265.units(HDR10);
    let out = transcode(&units);
    assert_eq!(out.len(), units.len(), "{test} every picture encoded");
    // a prefix SEI after the parameter sets and before the IDR's slice
    let types: Vec<u8> = out[0].windows(4).filter(|w| w[..3] == [0, 0, 1]).map(|w| (w[3] >> 1) & 0x3F).collect();
    let at = |t: u8| types.iter().position(|&x| x == t);
    assert!(at(34) < at(39) && at(39) < types.iter().position(|&x| x < 32), "{test} order {types:?}");
    let mut h = Harness::with_pool(VaapiH265Dec::new(), 128 * 128 * 3, 8);
    h.fix_sink_format(VaapiH265Dec::IN, "h265/annexb", &[]);
    for (i, au) in out.iter().enumerate() {
        let mut buf = h.alloc(au);
        buf.pts = Timestamp::from_nanos(i as u64 * 40_000_000);
        h.push(buf).expect("push");
        h.drain_outputs();
    }
    h.eos().expect("eos");
    let f = h.announced().expect("a format");
    let got = StreamColor::read_vocabulary(h.vocabulary(), &f);
    assert_eq!(got, source(), "{test} the encoder's stream states the source's colour");
}
