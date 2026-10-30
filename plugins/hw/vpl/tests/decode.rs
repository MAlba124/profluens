//! `vpldec` against the software decoders at a PSNR floor, HEVC Main 10 as p010.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use common::{Codec, decode_hw, decode_sw, hw, hw_decodes, mean_psnr, psnr16};

/// Exact up to inverse transform rounding.
const MIN_PSNR_DB: f64 = 45.0;

fn held(test: &str, codec: Codec, stream: &[u8], loops: usize) {
    let Some(caps) = hw_decodes(test, codec) else {
        return;
    };
    let once = codec.units(stream);
    let units: Vec<Vec<u8>> = (0..loops).flat_map(|_| once.iter().cloned()).collect();
    let sw = decode_sw(codec, &units);
    let hw = decode_hw(codec, &units);
    eprintln!(
        "{test}: {} units, sw {} frames {}x{} {}, hw {} frames {}x{} {}, api {}.{}",
        units.len(),
        sw.frames.len(),
        sw.width,
        sw.height,
        sw.pixfmt,
        hw.frames.len(),
        hw.width,
        hw.height,
        hw.pixfmt,
        caps.api.0,
        caps.api.1
    );
    assert!(hw.warnings.is_empty(), "{test}: the hardware decoder warned {:?}", hw.warnings);
    assert_eq!(hw.frames.len(), sw.frames.len(), "{test}: every picture out");
    assert_eq!((hw.width, hw.height), (sw.width, sw.height), "{test}: the announced size");
    assert_eq!(hw.pixfmt, "i420", "{test}: an 8 bit stream reads back as i420");
    let (w, h) = (sw.width as usize, sw.height as usize);
    for (i, (a, b)) in sw.frames.iter().zip(&hw.frames).enumerate() {
        assert_eq!(a.len(), b.len(), "{test}: picture {i} of one length");
    }
    let (y, c) = mean_psnr(&sw.frames, &hw.frames, w, h);
    eprintln!(
        "{test}: mean PSNR luma {y:.1} dB chroma {c:.1} dB over {} pictures",
        hw.frames.len()
    );
    assert!(
        y >= MIN_PSNR_DB && c >= MIN_PSNR_DB,
        "{test}: PSNR luma {y:.1} chroma {c:.1} under {MIN_PSNR_DB}"
    );
}

#[test]
fn probe_reports_the_runtime() {
    let Some(caps) = hw("probe_reports_the_runtime") else {
        return;
    };
    eprint!("{}", pf_vpl::probe::describe(caps));
    assert!(caps.api.0 >= 2, "a 2.x runtime");
}

#[test]
fn hevc_ipb_matches_the_software_decoder() {
    held("hevc_ipb_matches_the_software_decoder", Codec::H265, common::IPB_HEVC, 2);
}

#[test]
fn hevc_refs6_matches_the_software_decoder() {
    held("hevc_refs6_matches_the_software_decoder", Codec::H265, common::REFS6_HEVC, 1);
}

#[test]
fn h264_ipb_matches_the_software_decoder() {
    held("h264_ipb_matches_the_software_decoder", Codec::H264, common::IPB_H264, 2);
}

#[test]
fn h264_p_matches_the_software_decoder() {
    held("h264_p_matches_the_software_decoder", Codec::H264, common::P_H264, 1);
}

#[test]
fn vp9_features_match_the_software_decoder() {
    held("vp9_features_match_the_software_decoder", Codec::Vp9, common::FEATURES_VP9, 1);
}

#[test]
fn av1_tiles_match_the_software_decoder() {
    held("av1_tiles_match_the_software_decoder", Codec::Av1, common::TILES_AV1, 1);
}

#[test]
fn av1_inter_matches_the_software_decoder() {
    held("av1_inter_matches_the_software_decoder", Codec::Av1, common::INTER_AV1, 1);
}

#[test]
fn hevc_main10_reads_back_as_p010() {
    let test = "hevc_main10_reads_back_as_p010";
    let Some(caps) = hw_decodes(test, Codec::H265) else {
        return;
    };
    if !caps.decode_main10 {
        common::skip(test, "no HEVC Main 10 decode");
        return;
    }
    // No software Main 10 decoder, the VA-API decoder is the reference.
    if !pf_vaapi::probe().is_some_and(|c| c.decodes_profile("HEVCMain10")) {
        common::skip(test, "no VA-API HEVC Main 10 decode to hold against");
        return;
    }
    let units = Codec::H265.units(common::HDR10_HEVC);
    let va = common::decode_with(Box::new(pf_vaapi::VaapiH265Dec::new()), "h265/annexb", &units);
    let hw = decode_hw(Codec::H265, &units);
    eprintln!(
        "{test}: vaapi {} frames {} vpl {} frames {} {}x{}",
        va.frames.len(),
        va.pixfmt,
        hw.frames.len(),
        hw.pixfmt,
        hw.width,
        hw.height
    );
    assert!(hw.warnings.is_empty(), "{test}: warned {:?}", hw.warnings);
    assert_eq!(hw.pixfmt, "p010");
    assert_eq!(hw.frames.len(), va.frames.len());
    let y_len = (hw.width * hw.height * 2) as usize;
    let mut total = 0.0;
    for (a, b) in va.frames.iter().zip(&hw.frames) {
        assert_eq!(a.len(), b.len(), "{test}: p010 of one length");
        total += psnr16(&a[..y_len], &b[..y_len]).min(100.0);
    }
    let mean = total / hw.frames.len() as f64;
    eprintln!("{test}: mean luma PSNR {mean:.1} dB against vaapih265dec");
    assert!(mean >= MIN_PSNR_DB, "{test}: {mean:.1} dB");
}

#[test]
fn a_flush_resumes_at_the_next_keyframe() {
    let test = "a_flush_resumes_at_the_next_keyframe";
    if hw_decodes(test, Codec::H265).is_none() {
        return;
    }
    let units = Codec::H265.units(common::IPB_HEVC);
    let mut h =
        pf_harness::Harness::with_slot_size(pf_vpl::VplDec::new(), common::SLOT).real_wakes();
    h.fix_sink_format(profluens_core::id::SinkPad(0), "h265/annexb", &[]);
    h.start().expect("start");
    let mut before = 0;
    for u in &units[..units.len() / 2] {
        let b = h.alloc(u);
        h.push(b).expect("push");
        while h.pull(profluens_core::id::SrcPad(0)).is_some() {
            before += 1;
        }
    }
    h.push_event(profluens_core::event::Event::FlushStart).expect("flush");
    let mut after = 0;
    for u in &units {
        let b = h.alloc(u);
        h.push(b).expect("push");
        while h.pull(profluens_core::id::SrcPad(0)).is_some() {
            after += 1;
        }
    }
    after += h.eos().expect("eos").len();
    eprintln!("{test}: {before} pictures before the flush, {after} after");
    assert!(after >= units.len() - 2, "{test}: the stream decodes again after a flush");
}
