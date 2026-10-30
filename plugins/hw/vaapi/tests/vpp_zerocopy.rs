//! A VA decoder into vaapipostproc, readback and zero-copy, giving the same frames.

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
    frames: Vec<(Timestamp, Vec<u8>)>,
    warnings: Vec<String>,
    imports: u64,
    processed: u64,
}

fn run(
    codec: Codec,
    units: &[Vec<u8>],
    target: Target,
    channel: Option<&Arc<GpuFrameChannel>>,
) -> Run {
    let (tx, rx) = mpsc::channel();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(units.to_vec()));
    let dec = p.add_boxed(codec.decoder(channel));
    let vpp = match channel {
        Some(c) => VaapiPostProc::new_zerocopy(target, Arc::clone(c)),
        None => VaapiPostProc::new(target),
    };
    let stats = vpp.stats();
    let vpp = p.add(vpp);
    let sink = p.add(Collect(tx));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(VaapiPostProc::IN)).expect("decoder to vpp");
    p.link(vpp.src(VaapiPostProc::OUT), sink.sink(SinkPad(0))).expect("vpp to sink");
    p.set_element_pool(dec, 512 * 1024, 32);
    p.set_element_pool(vpp, 256 * 1024, 16);
    p.run().expect("the run ends");
    let warnings = chain::warnings(&p);
    drop(p);
    let mut frames: Vec<_> = rx.try_iter().collect();
    frames.sort_by_key(|f| f.0);
    Run {
        frames,
        warnings,
        imports: stats.imports.load(Ordering::Relaxed),
        processed: stats.frames.load(Ordering::Relaxed),
    }
}

fn device(codec: Codec, profile: Option<&str>, test: &str) -> bool {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip {test}: no VA-API device");
        return false;
    };
    let decodes = caps.supports(codec.family()) && profile.is_none_or(|p| caps.decodes_profile(p));
    if !decodes || !caps.video_proc {
        eprintln!(
            "skip {test}: no VA-API decode of {codec:?} {profile:?} or no VAEntrypointVideoProc"
        );
        return false;
    }
    true
}

fn check(test: &str, codec: Codec, profile: Option<&str>, stream: &[u8], target: Target) {
    if !device(codec, profile, test) {
        return;
    }
    let once = codec.units(stream);
    let units: Vec<Vec<u8>> = (0..LOOPS).flat_map(|_| once.iter().cloned()).collect();
    let read = run(codec, &units, target, None);
    let channel = GpuFrameChannel::new();
    let zero = run(codec, &units, target, Some(&channel));
    assert!(read.warnings.is_empty(), "{test} readback warned {:?}", read.warnings);
    assert!(zero.warnings.is_empty(), "{test} zero-copy warned {:?}", zero.warnings);
    assert_eq!(read.frames.len(), units.len(), "{test} every picture through readback");
    assert_eq!(zero.frames.len(), units.len(), "{test} every picture through zero-copy");
    assert_eq!(zero.processed, units.len() as u64);
    assert!(
        zero.imports > 0 && zero.imports < units.len() as u64,
        "{test} imported {} for {} frames",
        zero.imports,
        units.len()
    );
    let mut worst = 0u8;
    for ((pa, a), (pb, b)) in read.frames.iter().zip(&zero.frames) {
        assert_eq!(pa, pb, "{test} same pts");
        assert_eq!(a.len(), b.len(), "{test} same size");
        worst = worst.max(a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0));
    }
    assert!(worst <= 1, "{test} differs by {worst}");
    assert!(channel.take_tx().is_some() && channel.take_rx().is_some(), "{test} both ends back");
    eprintln!(
        "{test}: {} frames, {} imports, largest difference {worst}",
        units.len(),
        zero.imports
    );
}

#[test]
fn h264_through_dmabuf_is_the_same_as_through_readback() {
    let target = Target { max_width: 160, max_height: 120, depth: 8 };
    check(
        "h264_through_dmabuf_is_the_same_as_through_readback",
        Codec::H264,
        None,
        include_bytes!("fixtures/ipb_320x240.h264"),
        target,
    );
}

#[test]
fn h265_through_dmabuf_is_the_same_as_through_readback() {
    let target = Target { max_width: 160, max_height: 120, depth: 8 };
    check(
        "h265_through_dmabuf_is_the_same_as_through_readback",
        Codec::H265,
        None,
        include_bytes!("fixtures/ipb_320x240.hevc"),
        target,
    );
}

#[test]
fn main10_through_dmabuf_is_the_same_as_through_readback() {
    let target = Target { max_width: 64, max_height: 64, depth: 10 };
    check(
        "main10_through_dmabuf_is_the_same_as_through_readback",
        Codec::H265,
        Some("HEVCMain10"),
        include_bytes!("fixtures/hdr10_128x128.hevc"),
        target,
    );
}
