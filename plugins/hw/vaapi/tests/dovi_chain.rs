// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::{Arc, mpsc};

use pf_hdr::h26x::{H265_DOVI_EL, H265_DOVI_RPU, Scan, annexb_nals, scan_annexb};
use pf_hdr::rpu::{Sample, nal_form, sample};
use pf_hdr::side::{FrameMeta, Writer};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::VaapiH265Enc;
use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};

use chain::{Codec, Collect, FRAME_NS, UnitSrc};

const HDR10: &[u8] = include_bytes!("fixtures/hdr10_128x128.hevc");

fn bars(i: usize, scale: u32) -> Option<[u32; 4]> {
    Some([0, 0, (8 + i as u32) * scale, 16 * scale])
}

/// `unit` with an enhancement layer NAL and a two layer RPU behind its slices, as profile 7
/// has them, the RPU's bars those of a picture twice the size.
fn with_layers(unit: &[u8], i: usize) -> Vec<u8> {
    let rpu = nal_form(&sample(Sample { nlq: Some(true), area: bars(i, 2), ..Sample::P8 }));
    let el = [0, 0, 0, 1, H265_DOVI_EL << 1, 1, 0x40, 0x01, 0xAA];
    let head = [0, 0, 0, 1, H265_DOVI_RPU << 1, 1];
    [unit, &el[..], &head[..], &rpu[..]].concat()
}

fn kinds(au: &[u8]) -> Vec<u8> {
    annexb_nals(au).map(|n| n[0] >> 1 & 0x3F).collect()
}

fn rpu_of(au: &[u8]) -> Option<Vec<u8>> {
    let (mut side, mut scratch) = ([0u8; 2048], [0u8; 4096]);
    let mut scan = Scan::new(Writer::new(&mut side));
    scan_annexb(pf_hdr::h26x::Codec::H265, au, &mut scratch, &mut scan);
    let n = scan.side.len();
    let m = FrameMeta::read(&side[..n]).ok()?;
    assert!(!m.dovi_el(), "no enhancement layer comes out");
    m.dovi_rpu().map(|(_, b)| b.to_vec())
}

/// `dec ! vaapipostproc ! enc` on dma-bufs, the encoder asked for the RPUs of a source twice
/// the picture, what it made and what the run warned.
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
    let enc = p.add(
        VaapiH265Enc::new_zerocopy(Arc::clone(&b))
            .with_qp(26)
            .with_gop(30)
            .with_dovi((256, 256), 0),
    );
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
    units.iter().enumerate().map(|(i, u)| with_layers(u, i)).collect()
}

#[test]
fn each_picture_keeps_its_own_rpu_through_the_scaler_as_a_single_layers() {
    let test = "each_picture_keeps_its_own_rpu_through_the_scaler_as_a_single_layers";
    if !device(test) {
        return;
    }
    let units = sourced();
    let (out, warned) = run(&units, Target { max_width: 128, max_height: 128, depth: 10 });
    assert!(warned.is_empty(), "{test} warned {warned:?}");
    assert_eq!(out.len(), units.len(), "{test} every picture encoded");
    for (pts, au) in &out {
        let i = (pts.nanos().expect("a stamp") / FRAME_NS) as usize;
        // a full enhancement layer, so the mapping goes with it
        let want = nal_form(&sample(Sample { flat: true, area: bars(i, 1), ..Sample::P8 }));
        assert_eq!(rpu_of(au), Some(want), "{test} picture {i} its own");
        let k = kinds(au);
        assert_eq!(k.last(), Some(&H265_DOVI_RPU), "{test} picture {i} {k:?}");
        assert_eq!(k.iter().filter(|&&t| t >= H265_DOVI_RPU).count(), 1, "{test} {k:?}");
    }
}

#[test]
fn a_tone_mapped_output_carries_no_rpu() {
    let test = "a_tone_mapped_output_carries_no_rpu";
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
        assert_eq!(rpu_of(au), None, "{test} at {pts:?}");
        assert!(kinds(au).iter().all(|&t| t < H265_DOVI_RPU), "{test} at {pts:?}");
    }
}
