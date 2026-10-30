//! `vpldec ! vplvpp ! vplenc` on surfaces, decoded back in software and held to a PSNR floor.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use std::sync::Arc;
use std::sync::mpsc;

use pf_vpl::session::live_sessions;
use pf_vpl::{Settings, SurfaceChannel, Target, VplDec, VplEnc, VplGroup, VplVpp, nal};
use profluens_core::buffer::BufferFlags;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;

use common::{Codec, Collect, UnitSrc, decode_sw, hw_decodes, mean_psnr};

type Packet = (Timestamp, BufferFlags, Vec<u8>);

struct Run {
    units: Vec<Packet>,
    warnings: Vec<String>,
}

/// One chain in `p`, in `group` when given. The packets land on the receiver.
fn add_chain(
    p: &mut Pipeline,
    codec: Codec,
    out: Codec,
    units: &[Vec<u8>],
    target: Target,
    settings: Settings,
    group: Option<&Arc<VplGroup>>,
) -> (mpsc::Receiver<Packet>, [Arc<SurfaceChannel>; 2]) {
    let (tx, rx) = mpsc::channel();
    let (a, b) = (SurfaceChannel::new(), SurfaceChannel::new());
    let mut dec = VplDec::new_surfaces(a.clone());
    let mut vpp = VplVpp::new_surfaces(target, a.clone()).with_surface_output(b.clone());
    let mut enc = VplEnc::new_surfaces(b.clone()).with_settings(settings);
    if let Some(g) = group {
        dec = dec.with_group(Arc::clone(g));
        vpp = vpp.with_group(Arc::clone(g));
        enc = enc.with_group(Arc::clone(g));
    }
    let src = p.add(UnitSrc::new(codec, units.to_vec()));
    let dec = p.add(dec);
    let vpp = p.add(vpp);
    let enc = p.add(enc);
    let sink = p.add(Collect::only(tx, out));
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), vpp.sink(SinkPad(0))).expect("decoder to vpp");
    p.link(vpp.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("vpp to encoder");
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 4096, 32);
    p.set_element_pool(vpp, 4096, 16);
    p.set_element_pool(enc, 2 * 1024 * 1024, 8);
    (rx, [a, b])
}

fn channels_came_back(channels: &[Arc<SurfaceChannel>]) {
    for c in channels {
        assert!(
            c.take_tx().is_some() && c.take_rx().is_some(),
            "both ends of the surface channel came back"
        );
    }
}

fn run(codec: Codec, out: Codec, units: &[Vec<u8>], target: Target, settings: Settings) -> Run {
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    p.preroll().expect("preroll");
    let (rx, channels) = add_chain(&mut p, codec, out, units, target, settings, None);
    p.run().expect("the run ends");
    let warnings = common::warnings(&p);
    drop(p);
    channels_came_back(&channels);
    Run { units: rx.try_iter().collect(), warnings }
}

/// `chains` side by side, each in a group of its own, their packets in the same order.
fn run_grouped(
    chains: &[(Codec, Codec, &[Vec<u8>])],
    settings: Settings,
) -> (Vec<Vec<Packet>>, Vec<String>) {
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 32);
    p.preroll().expect("preroll");
    let target = Target { max_width: 0, max_height: 0, depth: 8 };
    let mut rxs = Vec::new();
    let mut all = Vec::new();
    for &(codec, out, units) in chains {
        let group = VplGroup::new().expect("a group on the probed runtime");
        let (rx, channels) = add_chain(&mut p, codec, out, units, target, settings, Some(&group));
        rxs.push(rx);
        all.extend(channels);
    }
    p.run().expect("the run ends");
    let warnings = common::warnings(&p);
    drop(p);
    channels_came_back(&all);
    (rxs.iter().map(|rx| rx.try_iter().collect()).collect(), warnings)
}

fn hevc_chain_settings() -> Settings {
    Settings { bitrate_kbps: 3000, gop: 30, bframes: 3, ..Settings::default() }
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, |d| d.count())
}

fn threads() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("Threads:")).and_then(|n| n.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn chain(test: &str, codec: Codec, out: Codec, stream: &[u8], target: Target, expect: (u32, u32)) {
    let Some(caps) = hw_decodes(test, codec) else {
        return;
    };
    if !caps.supports_encode(out.family()) {
        common::skip(test, &format!("the runtime does not encode {}", out.family()));
        return;
    }
    let units = Codec::H265.units(stream);
    let units = if codec == Codec::H264 { Codec::H264.units(stream) } else { units };
    let settings = Settings { bitrate_kbps: 3000, gop: 30, bframes: 3, ..Settings::default() };
    let r = run(codec, out, &units, target, settings);
    assert!(r.warnings.is_empty(), "{test}: the chain warned {:?}", r.warnings);
    eprintln!(
        "{test}: encoder delivered {} packets to the sink for {} inputs",
        r.units.len(),
        units.len()
    );
    assert_eq!(r.units.len(), units.len(), "{test}: every picture encoded");
    let hevc = out == Codec::H265;
    let first = &r.units[0];
    assert!(first.1.contains(BufferFlags::KEYFRAME), "{test}: the first packet is a keyframe");
    assert!(nal::has_sps(&first.2, hevc), "{test}: the first packet carries its parameter sets");
    assert!(nal::is_keyframe(&first.2, hevc), "{test}: the first packet opens with an IDR");
    let keys = r.units.iter().filter(|u| u.1.contains(BufferFlags::KEYFRAME)).count();
    let deltas = r.units.iter().filter(|u| u.1.contains(BufferFlags::DELTA)).count();
    assert_eq!(keys + deltas, r.units.len(), "{test}: every packet flagged");
    for (i, u) in r.units.iter().enumerate() {
        if u.1.contains(BufferFlags::KEYFRAME) {
            assert!(nal::has_sps(&u.2, hevc), "{test}: keyframe {i} carries its parameter sets");
        }
    }
    let bytes: usize = r.units.iter().map(|u| u.2.len()).sum();
    let back = decode_sw(out, &r.units.iter().map(|u| u.2.clone()).collect::<Vec<_>>());
    assert!(back.warnings.is_empty(), "{test}: decoding the output warned {:?}", back.warnings);
    assert_eq!(back.frames.len(), r.units.len(), "{test}: every packet decodes");
    assert_eq!((back.width, back.height), expect, "{test}: the output size");
    eprintln!(
        "{test}: {} packets, {bytes} bytes, {keys} keyframes, decoded back {}x{}",
        r.units.len(),
        back.width,
        back.height
    );
    if (back.width, back.height) == (320, 240) {
        let source = decode_sw(codec, &units);
        let (y, c) = mean_psnr(&source.frames, &back.frames, 320, 240);
        eprintln!("{test}: PSNR against the source luma {y:.1} dB chroma {c:.1} dB");
        assert!(y > 28.0, "{test}: luma PSNR {y:.1} collapsed");
    }
    let dir =
        std::env::var("PF_VPL_OUT").unwrap_or_else(|_| env!("CARGO_TARGET_TMPDIR").to_string());
    let path =
        std::path::Path::new(&dir).join(format!("{test}.{}", if hevc { "hevc" } else { "h264" }));
    match std::fs::write(
        &path,
        r.units.iter().flat_map(|u| u.2.iter().copied()).collect::<Vec<u8>>(),
    ) {
        Ok(()) => eprintln!("{test}: stream at {}", path.display()),
        Err(e) => eprintln!("{test}: stream not written to {} ({e})", path.display()),
    }
}

#[test]
fn hevc_decode_vpp_encode_at_the_source_size() {
    chain(
        "hevc_decode_vpp_encode_at_the_source_size",
        Codec::H265,
        Codec::H265,
        common::IPB_HEVC,
        Target { max_width: 0, max_height: 0, depth: 8 },
        (320, 240),
    );
}

#[test]
fn hevc_decode_vpp_encode_scaled_down() {
    chain(
        "hevc_decode_vpp_encode_scaled_down",
        Codec::H265,
        Codec::H265,
        common::IPB_HEVC,
        Target { max_width: 256, max_height: 192, depth: 8 },
        (256, 192),
    );
}

#[test]
fn h264_in_h264_out_through_the_chain() {
    chain(
        "h264_in_h264_out_through_the_chain",
        Codec::H264,
        Codec::H264,
        common::IPB_H264,
        Target { max_width: 0, max_height: 0, depth: 8 },
        (320, 240),
    );
}

#[test]
fn h264_in_hevc_out_scaled() {
    chain(
        "h264_in_hevc_out_scaled",
        Codec::H264,
        Codec::H265,
        common::P_H264,
        Target { max_width: 256, max_height: 192, depth: 8 },
        (256, 192),
    );
}

#[test]
fn the_decoder_hands_its_surfaces_to_the_encoder_without_the_vpp() {
    let test = "the_decoder_hands_its_surfaces_to_the_encoder_without_the_vpp";
    let Some(caps) = hw_decodes(test, Codec::H265) else {
        return;
    };
    if !caps.supports_encode("h265/annexb") {
        common::skip(test, "no HEVC encode");
        return;
    }
    let units = Codec::H265.units(common::P_HEVC);
    let (tx, rx) = mpsc::channel();
    let ch = SurfaceChannel::new();
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(Codec::H265, units.clone()));
    let dec = p.add(VplDec::new_surfaces(ch.clone()));
    // No B frames, reordering would pin decoder surfaces the direct handoff shares.
    let enc = p.add(VplEnc::new_surfaces(ch.clone()).with_settings(pf_vpl::Settings {
        bitrate_kbps: 2000,
        gop: 16,
        bframes: 0,
        ..pf_vpl::Settings::default()
    }));
    let sink = p.add(Collect::only(tx, Codec::H265));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    p.link(dec.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("decoder to encoder");
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 4096, 32);
    p.set_element_pool(enc, 2 * 1024 * 1024, 8);
    p.run().expect("the run ends");
    let warnings = common::warnings(&p);
    drop(p);
    let out: Vec<(Timestamp, BufferFlags, Vec<u8>)> = rx.try_iter().collect();
    assert!(warnings.is_empty(), "{test}: warned {warnings:?}");
    assert_eq!(out.len(), units.len(), "{test}: every picture encoded");
    let back = decode_sw(Codec::H265, &out.iter().map(|u| u.2.clone()).collect::<Vec<_>>());
    assert_eq!(back.frames.len(), out.len());
    let keys = out.iter().filter(|u| u.1.contains(BufferFlags::KEYFRAME)).count();
    eprintln!(
        "{test}: {} packets, {keys} keyframes at a gop of 16, decoded back {}x{}",
        out.len(),
        back.width,
        back.height
    );
    assert!(keys >= out.len() / 16, "{test}: a keyframe every 16 frames");
}

#[test]
fn two_chains_in_two_groups_match_each_alone() {
    let test = "two_chains_in_two_groups_match_each_alone";
    let Some(caps) = hw_decodes(test, Codec::H265) else {
        return;
    };
    if !caps.supports_encode("h265/annexb") {
        return common::skip(test, "no HEVC encode");
    }
    let ipb = Codec::H265.units(common::IPB_HEVC);
    let p = Codec::H265.units(common::P_HEVC);
    let settings = hevc_chain_settings();
    let target = Target { max_width: 0, max_height: 0, depth: 8 };
    let alone_ipb = run(Codec::H265, Codec::H265, &ipb, target, settings);
    let alone_p = run(Codec::H265, Codec::H265, &p, target, settings);
    assert!(alone_ipb.warnings.is_empty() && alone_p.warnings.is_empty(), "{test}: alone warned");
    let h = Codec::H265;
    let (both, warnings) = run_grouped(&[(h, h, &ipb), (h, h, &p)], settings);
    assert!(warnings.is_empty(), "{test}: side by side warned {warnings:?}");
    assert_eq!(both[0].len(), ipb.len(), "{test}: the first chain encoded every picture");
    assert_eq!(both[1].len(), p.len(), "{test}: the second chain encoded every picture");
    let bytes = |u: &[Packet]| u.iter().map(|p| p.2.clone()).collect::<Vec<_>>();
    assert_eq!(bytes(&both[0]), bytes(&alone_ipb.units), "{test}: the first chain's bytes");
    assert_eq!(bytes(&both[1]), bytes(&alone_p.units), "{test}: the second chain's bytes");
    assert_eq!(live_sessions(), 0, "{test}: every component session closed");
}

#[test]
fn a_chain_reopening_its_encoder_beside_a_running_chain() {
    let test = "a_chain_reopening_its_encoder_beside_a_running_chain";
    let Some(caps) = hw_decodes(test, Codec::H265) else {
        return;
    };
    if !caps.supports_encode("h265/annexb") {
        return common::skip(test, "no HEVC encode");
    }
    if !caps.supports_encode("h264/annexb") {
        return common::skip(test, "no H.264 encode");
    }
    // A size change midstream reopens the decoder, the VPP and the encoder, each joining its
    // group while the other chain runs. The grown chain encodes H.264, Gen12 HEVC encode refuses
    // 96 lines.
    let small: &[u8] = include_bytes!("../../vaapi/tests/fixtures/p_160x96.hevc");
    let mut grown = Codec::H265.units(small);
    let ipb = Codec::H265.units(common::IPB_HEVC);
    grown.extend(ipb.iter().cloned());
    let settings = hevc_chain_settings();
    let (h, avc) = (Codec::H265, Codec::H264);
    let (out, warnings) = run_grouped(&[(h, avc, &grown), (h, h, &ipb)], settings);
    assert!(warnings.is_empty(), "{test}: warned {warnings:?}");
    assert_eq!(out[0].len(), grown.len(), "{test}: every picture across the size change");
    assert_eq!(out[1].len(), ipb.len(), "{test}: the other chain lost nothing");
    let back = decode_sw(Codec::H264, &out[0].iter().map(|u| u.2.clone()).collect::<Vec<_>>());
    assert_eq!(back.frames.len(), grown.len(), "{test}: the reopened stream decodes");
    assert_eq!(live_sessions(), 0, "{test}: every component session closed");
}

#[test]
fn fifty_grouped_chains_in_a_row_leave_no_session_fd_or_thread() {
    let test = "fifty_grouped_chains_in_a_row_leave_no_session_fd_or_thread";
    let Some(caps) = hw_decodes(test, Codec::H265) else {
        return;
    };
    if !caps.supports_encode("h265/annexb") {
        return common::skip(test, "no HEVC encode");
    }
    let units = Codec::H265.units(common::P_HEVC);
    let settings = Settings { lookahead: 0, bframes: 0, gop: 16, ..hevc_chain_settings() };
    // The first chain warms the runtime, its threads and fds are the baseline.
    let h = Codec::H265;
    let (first, warnings) = run_grouped(&[(h, h, &units)], settings);
    assert!(warnings.is_empty() && first[0].len() == units.len(), "{test}: the first chain");
    let (fds, thr) = (open_fds(), threads());
    for i in 1..50 {
        let (out, warnings) = run_grouped(&[(h, h, &units)], settings);
        assert!(warnings.is_empty(), "{test}: chain {i} warned {warnings:?}");
        assert_eq!(out[0].len(), units.len(), "{test}: chain {i} encoded every picture");
        assert_eq!(live_sessions(), 0, "{test}: chain {i} left a session");
    }
    let (fds_after, thr_after) = (open_fds(), threads());
    eprintln!("{test}: fds {fds} -> {fds_after}, threads {thr} -> {thr_after}");
    assert!(fds_after <= fds + 2, "{test}: fds grew from {fds} to {fds_after}");
    assert!(thr_after <= thr + 2, "{test}: threads grew from {thr} to {thr_after}");
}
