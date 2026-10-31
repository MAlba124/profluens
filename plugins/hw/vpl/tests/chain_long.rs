//! The Matroska HEVC fixture through three chain forms, every picture out of each.
//! A chain that stops short holds surfaces somewhere.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use std::path::Path;
use std::sync::mpsc;

use pf_mkv::{MatroskaReader, Reframer, nal_head_from_config};
use pf_vpl::{Settings, SurfaceChannel, Target, VplDec, VplEnc, VplVpp};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

use common::{Codec, Collect, UnitSrc, hw_decodes};

const FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/mkv_h265_aac.mkv");
const FRAMES: usize = 125;

fn read_hevc_aus() -> Option<Vec<Vec<u8>>> {
    if !Path::new(FIXTURE).exists() {
        return None;
    }
    let data = std::fs::read(FIXTURE).ok()?;
    let mut reader = MatroskaReader::new();
    reader.push(&data).ok()?;
    let track = reader.tracks().iter().find(|t| t.codec_id == "V_MPEGH/ISO/HEVC")?.clone();
    let (head, length_size) = nal_head_from_config(&track.codec_private, true).ok()?;
    let reframer = Reframer::Nal { length_size };
    let mut aus = Vec::new();
    let mut scratch = Vec::new();
    while let Some(frame) = reader.next_frame() {
        if frame.track_number != track.track_number {
            continue;
        }
        let annexb = reframer.reframe_into(&frame.data, &mut scratch).ok()?;
        let mut au = Vec::new();
        if aus.is_empty() {
            au.extend_from_slice(&head);
        }
        au.extend_from_slice(annexb);
        aus.push(au);
    }
    (!aus.is_empty()).then_some(aus)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
    Fixed,
    Bounded,
    Direct,
}

fn run(units: &[Vec<u8>], form: Form) -> (usize, Vec<String>) {
    let (tx, rx) = mpsc::channel();
    let (a, b) = (SurfaceChannel::new(), SurfaceChannel::new());
    let mut p = Pipeline::new();
    p.set_pool(256 * 1024, 16);
    let src = p.add(UnitSrc::new(Codec::H265, units.to_vec()));
    let dec = p.add(VplDec::new_surfaces(a.clone()));
    let target = Target { max_width: 320, max_height: 180, depth: 8 };
    let settings = Settings {
        bitrate_kbps: 600,
        gop: 50,
        bframes: 0,
        lookahead: 0,
        rc: pf_vpl::Rc::Vbr,
        hrd_ms: 1000,
        ..Settings::default()
    };
    let vpp = match form {
        Form::Fixed => Some(
            p.add(
                VplVpp::new_surfaces(target, a.clone())
                    .with_fixed_size()
                    .with_surface_output(b.clone()),
            ),
        ),
        Form::Bounded => {
            Some(p.add(VplVpp::new_surfaces(target, a.clone()).with_surface_output(b.clone())))
        }
        Form::Direct => None,
    };
    let enc_in = if vpp.is_some() { b.clone() } else { a.clone() };
    let enc =
        p.add(VplEnc::new_surfaces(enc_in).with_family("h264/annexb").with_settings(settings));
    let sink = p.add(Collect::only(tx, Codec::H264));
    p.preroll().expect("preroll");
    p.link(src.src(SrcPad(0)), dec.sink(SinkPad(0))).expect("src to decoder");
    match vpp {
        Some(v) => {
            p.link(dec.src(SrcPad(0)), v.sink(SinkPad(0))).expect("decoder to vpp");
            p.link(v.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("vpp to encoder");
            p.set_element_pool(v, 4096, 16);
        }
        None => {
            p.link(dec.src(SrcPad(0)), enc.sink(SinkPad(0))).expect("decoder to encoder");
        }
    }
    p.link(enc.src(SrcPad(0)), sink.sink(SinkPad(0))).expect("encoder to sink");
    p.set_element_pool(dec, 4096, 32);
    p.set_element_pool(enc, 4 << 20, 8);
    p.run().expect("the run ends");
    let warnings = common::warnings(&p);
    drop(p);
    (rx.try_iter().count(), warnings)
}

#[test]
fn the_long_clip_comes_out_whole_in_every_form() {
    let test = "the_long_clip_comes_out_whole_in_every_form";
    if hw_decodes(test, Codec::H265).is_none() {
        return;
    }
    let Some(units) = read_hevc_aus() else {
        return common::skip(test, &format!("{FIXTURE} absent or without an HEVC track"));
    };
    assert_eq!(units.len(), FRAMES, "{test}: the fixture's frames");
    let mut short = Vec::new();
    // PF_VPL_FORM=direct|bounded|fixed runs one form alone.
    let only = std::env::var("PF_VPL_FORM").ok();
    for form in [Form::Direct, Form::Bounded, Form::Fixed] {
        if only.as_deref().is_some_and(|o| !format!("{form:?}").eq_ignore_ascii_case(o)) {
            continue;
        }
        let (n, warnings) = run(&units, form);
        eprintln!("{test}: {form:?} {n} of {FRAMES} packets, warnings {warnings:?}");
        if n != FRAMES || !warnings.is_empty() {
            short.push((form, n, warnings));
        }
    }
    assert!(short.is_empty(), "{test}: short or warned {short:?}");
}
