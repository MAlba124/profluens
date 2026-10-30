//! AV1 decoded by dav1d, through vaapipostproc and the H.264 encoder, CPU vs dma-buf byte for byte.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::mpsc;

use pf_mkv::MatroskaReader;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::VaapiH264Enc;
use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};

use chain::{Collect, UnitSrc};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/mkv_av1.mkv");
const TARGET: Target = Target { max_width: 256, max_height: 144, depth: 8 };

fn temporal_units() -> Option<Vec<Vec<u8>>> {
    let data = std::fs::read(FIXTURE).ok()?;
    let mut reader = MatroskaReader::new();
    reader.push(&data).ok()?;
    let track = reader.tracks().iter().find(|t| t.codec_id == "V_AV1")?.clone();
    // av1C is four bytes of its own, then the sequence header OBU
    let config = track.codec_private.get(4..).unwrap_or(&[]).to_vec();
    let mut units = Vec::new();
    while let Some(frame) = reader.next_frame() {
        if frame.track_number != track.track_number {
            continue;
        }
        let mut tu = if units.is_empty() { config.clone() } else { Vec::new() };
        tu.extend_from_slice(&frame.data);
        units.push(tu);
    }
    (!units.is_empty()).then_some(units)
}

fn run(units: &[Vec<u8>], gpu: bool) -> (Vec<(Timestamp, Vec<u8>)>, Vec<String>) {
    let ch = GpuFrameChannel::new();
    let (vpp, enc) = match gpu {
        true => (
            VaapiPostProc::new(TARGET).with_gpu_output(Arc::clone(&ch)),
            VaapiH264Enc::new_zerocopy(Arc::clone(&ch)),
        ),
        false => (VaapiPostProc::new(TARGET), VaapiH264Enc::new()),
    };
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.to_vec()));
    let dec = p.add(pf_av1::Av1Dec::new());
    let vpp = p.add(vpp);
    let enc = p.add(enc.with_qp(26).with_gop(30));
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(vpp.src(VaapiPostProc::OUT), enc.sink(VaapiH264Enc::IN)).expect("vpp to encoder");
    p.link(enc.src(VaapiH264Enc::OUT), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 256 * 1024, 16);
    p.set_element_pool(vpp, 256 * 1024, 16);
    p.set_element_pool(enc, 256 * 1024, 16);
    p.run().expect("the run ends");
    let warnings = chain::warnings(&p);
    drop(p);
    if gpu {
        assert!(ch.take_tx().is_some() && ch.take_rx().is_some(), "both ends back");
    }
    (rx.try_iter().collect(), warnings)
}

#[test]
fn av1_through_dav1d_encodes_the_same_on_dmabufs_as_through_the_cpu() {
    let test = "av1_through_dav1d_encodes_the_same_on_dmabufs_as_through_the_cpu";
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return;
    };
    if !caps.supports_encode("h264/annexb") || !caps.video_proc {
        eprintln!("skip {test}: no VA-API H.264 encode or no VAEntrypointVideoProc");
        return;
    }
    let Some(units) = temporal_units() else {
        eprintln!("skip {test}: fixture {FIXTURE} absent (cargo xtask fixtures)");
        return;
    };
    let (cpu, cw) = run(&units, false);
    let (gpu, gw) = run(&units, true);
    assert!(cw.is_empty() && gw.is_empty(), "warned {cw:?} {gw:?}");
    assert_eq!(cpu.len(), units.len(), "every frame encoded");
    assert_eq!(cpu.len(), gpu.len());
    for (i, (a, b)) in cpu.iter().zip(&gpu).enumerate() {
        assert_eq!(a.0, b.0, "unit {i} pts");
        assert!(a.1 == b.1, "unit {i} differs");
    }
    let (pictures, warned) = chain::decode(chain::Codec::H264, &gpu);
    assert!(warned.is_empty(), "decoding the stream warned {warned:?}");
    assert_eq!(pictures.len(), gpu.len(), "every picture decodes");
    eprintln!(
        "{test}: {} AV1 frames to 256x144 H.264, the same both ways, decoded back",
        cpu.len()
    );
}
