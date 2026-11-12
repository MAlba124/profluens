// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::{Arc, mpsc};

use pf_hdr::emit::{Body, Place, Unit, write_h265};
use pf_hdr::h26x::{H265_PREFIX_SEI, SEI_T35, Scan, annexb_nals, scan_annexb};
use pf_hdr::side::{FrameMeta, Writer};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};

use chain::{Codec, Collect, FRAME_NS, UnitSrc};

const HDR10: &[u8] = include_bytes!("fixtures/hdr10_128x128.hevc");

fn maxscl(i: usize) -> u32 {
    1000 + 100 * i as u32
}

fn first_slice(au: &[u8]) -> usize {
    (0..au.len().saturating_sub(3))
        .find(|&i| au[i..i + 3] == [0, 0, 1] && (au[i + 3] >> 1) & 0x3F < 32)
        .map_or(au.len(), |i| if i > 0 && au[i - 1] == 0 { i - 1 } else { i })
}

/// `unit` with an HDR10+ prefix SEI in front of its first slice.
fn with_hdr10plus(unit: &[u8], maxscl: u32) -> Vec<u8> {
    let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
    t35.extend(pf_hdr::hdr10plus::sample_payload([maxscl, 1, 1], 1000));
    let body = Body::Sei { payload_type: SEI_T35, payload: &t35 };
    let u = Unit { place: Place::BeforeSlices, nal_type: H265_PREFIX_SEI, body };
    let mut nal = [0u8; 1024];
    let n = write_h265(&u, &mut nal).expect("an SEI fits");
    let at = first_slice(unit);
    [&unit[..at], &nal[..n], &unit[at..]].concat()
}

fn hdr10plus_of(au: &[u8]) -> Option<u32> {
    let (mut side, mut scratch) = ([0u8; 2048], [0u8; 4096]);
    let mut scan = Scan::new(Writer::new(&mut side));
    scan_annexb(pf_hdr::h26x::Codec::H265, au, &mut scratch, &mut scan);
    let n = scan.side.len();
    let m = FrameMeta::read(&side[..n]).ok()?;
    Some(m.hdr10plus()?.ok()?.windows[0].maxscl[0])
}

fn kinds(au: &[u8]) -> Vec<u8> {
    annexb_nals(au).map(|n| n[0] >> 1 & 0x3F).collect()
}

/// `dec ! vaapipostproc ! enc` on dma-bufs, what the encoder made and what the run warned.
fn run(units: &[Vec<u8>], target: Target) -> (Vec<(Timestamp, Vec<u8>)>, Vec<String>) {
    let (tx, rx) = mpsc::channel();
    let (a, b) = (GpuFrameChannel::new(), GpuFrameChannel::new());
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.to_vec()));
    let dec = p.add_boxed(Codec::H265.decoder(Some(&a)));
    let vpp = p.add(
        VaapiPostProc::new_zerocopy(target, Arc::clone(&a))
            .with_fixed_size()
            .with_gpu_output(Arc::clone(&b)),
    );
    let enc = p.add_boxed(Codec::H265.encoder(Some(&b)));
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(vpp.src(VaapiPostProc::OUT), enc.sink(SinkPad(0))).expect("vpp to encoder");
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(vpp, 256 * 1024, 16);
    p.set_element_pool(enc, 1 << 20, 8);
    p.run().expect("the run ends");
    let warned = chain::warnings(&p);
    drop(p);
    (rx.try_iter().collect(), warned)
}

fn device(test: &str) -> bool {
    let ok = pf_vaapi::probe().is_some_and(|c| {
        c.video_proc
            && c.decodes_profile("HEVCMain10")
            && c.profiles.iter().any(|p| p.name == "HEVCMain10" && (p.enc || p.enc_lp))
    });
    if !ok {
        eprintln!("skip {test}: no HEVC Main 10 decode, encode and video processing");
    }
    ok
}

fn sourced() -> Vec<Vec<u8>> {
    let units = Codec::H265.units(HDR10);
    units.iter().enumerate().map(|(i, u)| with_hdr10plus(u, maxscl(i))).collect()
}

#[test]
fn each_picture_keeps_its_own_hdr10plus_through_the_scaler_and_the_encoder() {
    let test = "each_picture_keeps_its_own_hdr10plus_through_the_scaler_and_the_encoder";
    if !device(test) {
        return;
    }
    let units = sourced();
    let (out, warned) = run(&units, Target { max_width: 128, max_height: 128, depth: 10 });
    assert!(warned.is_empty(), "{test} warned {warned:?}");
    assert_eq!(out.len(), units.len(), "{test} every picture encoded");
    for (pts, au) in &out {
        let i = (pts.nanos().expect("a stamp") / FRAME_NS) as usize;
        assert_eq!(hdr10plus_of(au), Some(maxscl(i)), "{test} picture {i} its own");
        let k = kinds(au);
        let slice = k.iter().position(|&t| t < 32).expect("a slice");
        assert!(k[..slice].contains(&H265_PREFIX_SEI), "{test} picture {i} {k:?}");
        assert!(k[slice..].iter().all(|&t| t < 32), "{test} nothing after the slices {k:?}");
    }
}

#[test]
fn a_tone_mapped_output_carries_no_hdr10plus() {
    let test = "a_tone_mapped_output_carries_no_hdr10plus";
    if !device(test) {
        return;
    }
    let units = sourced();
    let (out, warned) = run(&units, Target { max_width: 128, max_height: 128, depth: 8 });
    if warned.iter().any(|w| w.contains("tone mapping")) {
        return eprintln!("skip {test}: the driver does not tone map, {warned:?}");
    }
    assert!(warned.is_empty(), "{test} warned {warned:?}");
    assert_eq!(out.len(), units.len(), "{test} every picture encoded");
    for (pts, au) in &out {
        assert_eq!(hdr10plus_of(au), None, "{test} at {pts:?}");
        assert!(!kinds(au).contains(&H265_PREFIX_SEI), "{test} no SEI at all at {pts:?}");
    }
}
