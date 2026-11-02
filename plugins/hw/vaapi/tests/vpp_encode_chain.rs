//! Decode, vaapipostproc and encode through the CPU and on dma-bufs. At a fixed quantizer both
//! give the same stream byte for byte. Skipped without a capable VA device.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/chain.rs"]
mod chain;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::GpuFrameChannel;
use pf_vaapi::vpp::{Target, VaapiPostProc};

use chain::{Codec, Collect, UnitSrc};

const LOOPS: usize = 3;

struct Run {
    units: Vec<(Timestamp, Vec<u8>)>,
    warnings: Vec<String>,
    vpp_imports: u64,
}

fn run(
    codec: Codec,
    units: &[Vec<u8>],
    target: Target,
    channels: Option<(&Arc<GpuFrameChannel>, &Arc<GpuFrameChannel>)>,
) -> Run {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.to_vec()));
    let vpp = match channels {
        Some((a, b)) => {
            VaapiPostProc::new_zerocopy(target, Arc::clone(a)).with_gpu_output(Arc::clone(b))
        }
        None => VaapiPostProc::new(target),
    };
    let stats = vpp.stats();
    let dec = p.add_boxed(codec.decoder(channels.map(|c| c.0)));
    let vpp = p.add(vpp);
    let enc = p.add_boxed(codec.encoder(channels.map(|c| c.1)));
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(vpp.src(VaapiPostProc::OUT), enc.sink(SinkPad(0))).expect("vpp to encoder");
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(vpp, 256 * 1024, 16);
    p.set_element_pool(enc, 256 * 1024, 16);
    p.run().expect("the run ends");
    let warnings = chain::warnings(&p);
    drop(p);
    Run {
        units: rx.try_iter().collect(),
        warnings,
        vpp_imports: stats.imports.load(Ordering::Relaxed),
    }
}

fn device(codec: Codec, profile: Option<&str>, test: &str) -> bool {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return false;
    };
    let family = codec.family();
    let both = caps.supports(family) && caps.supports_encode(family);
    let named = profile.is_none_or(|n| caps.profiles.iter().any(|p| p.name == n && p.vld && p.enc));
    if !both || !named || !caps.video_proc {
        eprintln!(
            "skip {test}: no VA-API {codec:?} {profile:?} decode and encode, or no VAEntrypointVideoProc"
        );
        return false;
    }
    true
}

fn check(
    test: &str,
    codec: Codec,
    profile: Option<&str>,
    stream: &[u8],
    target: Target,
    picture: usize,
) {
    if !device(codec, profile, test) {
        return;
    }
    let once = codec.units(stream);
    let all: Vec<Vec<u8>> = (0..LOOPS).flat_map(|_| once.iter().cloned()).collect();
    let cpu = run(codec, &all, target, None);
    let (a, b) = (GpuFrameChannel::new(), GpuFrameChannel::new());
    let gpu = run(codec, &all, target, Some((&a, &b)));
    assert!(cpu.warnings.is_empty(), "{test} through the cpu warned {:?}", cpu.warnings);
    assert!(gpu.warnings.is_empty(), "{test} on dma-bufs warned {:?}", gpu.warnings);
    assert_eq!(cpu.units.len(), all.len(), "{test} every picture encoded through the cpu");
    assert_eq!(gpu.units.len(), all.len(), "{test} every picture encoded on dma-bufs");
    assert!(
        gpu.vpp_imports > 0 && gpu.vpp_imports < all.len() as u64,
        "{test} vpp imported {} for {} frames",
        gpu.vpp_imports,
        all.len()
    );
    for (i, ((pa, x), (pb, y))) in cpu.units.iter().zip(&gpu.units).enumerate() {
        assert_eq!(pa, pb, "{test} unit {i} pts");
        assert!(x == y, "{test} unit {i} differs, {} and {} bytes", x.len(), y.len());
    }
    for c in [&a, &b] {
        assert!(c.take_tx().is_some() && c.take_rx().is_some(), "{test} both ends back");
    }
    let (pictures, warned) = chain::decode(codec, &gpu.units);
    assert!(warned.is_empty(), "{test} decoding the stream warned {warned:?}");
    assert_eq!(pictures.len(), gpu.units.len(), "{test} every picture decodes");
    assert!(pictures.iter().all(|p| p.len() == picture), "{test} pictures of {picture} bytes");
    let ext = if codec == Codec::H264 { "h264" } else { "hevc" };
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}.{ext}"));
    let bytes: Vec<u8> = gpu.units.iter().flat_map(|u| u.1.iter().copied()).collect();
    std::fs::write(&path, &bytes).expect("the stream written");
    eprintln!(
        "{test}: {} frames to {}x{} at {} bits, {} bytes, the same both ways, vpp imported {}, decoded back, {}",
        all.len(),
        target.max_width,
        target.max_height,
        target.depth,
        bytes.len(),
        gpu.vpp_imports,
        path.display()
    );
}

#[test]
fn h264_on_dmabufs_encodes_what_the_cpu_path_does() {
    let target = Target { max_width: 256, max_height: 192, depth: 8 };
    let picture = 256 * 192 * 3 / 2;
    check(
        "h264_on_dmabufs_encodes_what_the_cpu_path_does",
        Codec::H264,
        None,
        include_bytes!("fixtures/ipb_320x240.h264"),
        target,
        picture,
    );
}

#[test]
fn hevc_on_dmabufs_encodes_what_the_cpu_path_does() {
    let target = Target { max_width: 256, max_height: 192, depth: 8 };
    let picture = 256 * 192 * 3 / 2;
    check(
        "hevc_on_dmabufs_encodes_what_the_cpu_path_does",
        Codec::H265,
        None,
        include_bytes!("fixtures/ipb_320x240.hevc"),
        target,
        picture,
    );
}

#[test]
fn main10_on_dmabufs_encodes_what_the_cpu_path_does_and_decodes_to_ten_bits() {
    // iHD on Gen12 encodes no HEVC picture under 128 wide on either entrypoint
    let target = Target { max_width: 128, max_height: 128, depth: 10 };
    let picture = 128 * 128 * 3;
    let test = "main10_on_dmabufs_encodes_what_the_cpu_path_does_and_decodes_to_ten_bits";
    check(
        test,
        Codec::H265,
        Some("HEVCMain10"),
        include_bytes!("fixtures/hdr10_128x128.hevc"),
        target,
        picture,
    );
}
