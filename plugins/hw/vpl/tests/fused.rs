//! The fused decode and VPP, its fallback, and its cost against decode then `vplvpp`.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use std::sync::mpsc;
use std::time::Instant;

use pf_vpl::{SurfaceChannel, Target, VplDec, VplVpp};
use profluens_core::buffer::BufferFlags;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use common::{Codec, Collect, UnitSrc, hw_decodes};

type Out = Vec<(Timestamp, BufferFlags, Vec<u8>)>;

fn fused_run(units: &[Vec<u8>], targets: &[Target]) -> (Vec<Out>, Vec<String>, Option<bool>, f64) {
    let mut dec = VplDec::new();
    for t in targets {
        dec = dec.with_channel(*t, None);
    }
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(Codec::H265, units.to_vec()));
    let dec_id = p.add(dec);
    let mut rxs = Vec::new();
    let mut sinks = Vec::new();
    for _ in 0..=targets.len() {
        let (tx, rx) = mpsc::channel();
        sinks.push(p.add(Collect(tx, None)));
        rxs.push(rx);
    }
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec_id.sink(SinkPad(0))).expect("src to decoder");
    for (i, s) in sinks.iter().enumerate() {
        p.link(dec_id.src(SrcPad(i as u32)), s.sink(SinkPad(0))).expect("decoder pad to sink");
    }
    p.set_element_pool(dec_id, common::SLOT, 8 * (targets.len() as u32 + 1));
    let t0 = Instant::now();
    p.run().expect("the run ends");
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let warnings = common::warnings(&p);
    drop(p);
    let outs: Vec<Out> = rxs.iter().map(|r| r.try_iter().collect()).collect();
    (outs, warnings, None, ms)
}

fn separate_run(units: &[Vec<u8>], target: Target) -> (Out, Vec<String>, f64) {
    let (tx, rx) = mpsc::channel();
    let ch = SurfaceChannel::new();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(Codec::H265, units.to_vec()));
    let dec = p.add(VplDec::new_surfaces(ch.clone()));
    let vpp = p.add(VplVpp::new_surfaces(target, ch.clone()));
    let sink = p.add(Collect(tx, None));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(SinkPad(0))).expect("decoder to vpp");
    p.link(vpp.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("vpp to sink");
    p.set_element_pool(dec, 4096, 32);
    p.set_element_pool(vpp, common::SLOT, 8);
    let t0 = Instant::now();
    p.run().expect("the run ends");
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let warnings = common::warnings(&p);
    drop(p);
    (rx.try_iter().collect(), warnings, ms)
}

#[test]
fn fused_channels_put_out_the_scaled_pictures() {
    let test = "fused_channels_put_out_the_scaled_pictures";
    let Some(caps) = hw_decodes(test, Codec::H265) else {
        return;
    };
    let units = Codec::H265.units(common::IPB_HEVC);
    let targets = [
        Target { max_width: 160, max_height: 120, depth: 8 },
        Target { max_width: 64, max_height: 48, depth: 8 },
    ];
    let (outs, warnings, _, ms) = fused_run(&units, &targets);
    let fused_said = warnings.iter().any(|w| w.contains("FusedInit"));
    let others: Vec<&String> = warnings.iter().filter(|w| !w.contains("FusedInit")).collect();
    assert!(others.is_empty(), "{test}: warned {others:?}");
    eprintln!(
        "{test}: fused init {} (probe said {}), {:.1} ms for {} units",
        if fused_said { "refused, separate VPPs fed the channels" } else { "taken" },
        pf_vpl::MfxError { status: caps.fused, call: "" }.name(),
        ms,
        units.len()
    );
    assert_eq!(outs[0].len(), units.len(), "{test}: every picture on the main pad");
    let sizes = [(320, 240), (160, 120), (64, 48)];
    for (i, out) in outs.iter().enumerate() {
        let (w, h) = sizes[i];
        assert_eq!(out.len(), units.len(), "{test}: every picture on pad {i}");
        assert!(
            out.iter().all(|u| u.2.len() == w * h * 3 / 2),
            "{test}: pad {i} pictures of {w}x{h} i420"
        );
    }
    // A scaled picture keeps its mean luma within a few levels.
    let mean =
        |f: &[u8], n: usize| f[..n].iter().map(|&b| u64::from(b)).sum::<u64>() as f64 / n as f64;
    for (i, (big, scaled)) in outs[0].iter().zip(&outs[1]).enumerate() {
        let full = mean(&big.2, 320 * 240);
        let small = mean(&scaled.2, 160 * 120);
        assert!(
            (full - small).abs() < 6.0,
            "{test}: picture {i} mean luma full {full:.1} scaled {small:.1}"
        );
    }
}

#[test]
fn fused_against_separate_cost_a_frame() {
    let test = "fused_against_separate_cost_a_frame";
    if hw_decodes(test, Codec::H265).is_none() {
        return;
    }
    let once = Codec::H265.units(common::IPB_HEVC);
    let units = once.clone();
    let target = Target { max_width: 160, max_height: 120, depth: 8 };
    eprintln!("{test}: fused run start ({} frames)", units.len());
    let (outs, fw, _, fused_ms) = fused_run(&units, &[target]);
    eprintln!("{test}: fused done in {fused_ms:.0} ms, separate run start");
    let (sep, sw, sep_ms) = separate_run(&units, target);
    eprintln!("{test}: separate done in {sep_ms:.0} ms");
    let fused_refused = fw.iter().any(|w| w.contains("FusedInit"));
    assert_eq!(outs[1].len(), units.len(), "{test}: fused channel pictures");
    assert_eq!(sep.len(), units.len(), "{test}: separate pictures, warnings {sw:?}");
    let n = units.len() as f64;
    eprintln!(
        "{test}: {} frames 320x240 to 160x120, fused{} {:.2} ms/frame, decode then vplvpp {:.2} ms/frame (both read back to the CPU, wall time of the whole pipeline run)",
        units.len(),
        if fused_refused { " (refused, fallback VPP)" } else { "" },
        fused_ms / n,
        sep_ms / n
    );
}
