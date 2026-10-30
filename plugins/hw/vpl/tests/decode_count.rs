//! `vpldec` picture count against the fixture frame count. Run with `--nocapture` to probe a
//! runtime.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::Path;

use pf_harness::Harness;
use pf_mkv::{MatroskaReader, Reframer, nal_head_from_config};
use pf_vpl::VplDec;
use profluens_core::bus::BusMessage;
use profluens_core::time::Timestamp;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/mkv_h265_aac.mkv");
const FRAMES: usize = 125;
const FRAME_NS: u64 = 40_000_000;

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

#[test]
fn every_access_unit_of_the_fixture_comes_out_as_a_picture() {
    let test = "every_access_unit_of_the_fixture_comes_out_as_a_picture";
    if pf_vpl::probe().is_none() {
        return pf_vpl::probe::skip(test, "no VPL runtime");
    }
    let Some(aus) = read_hevc_aus() else {
        return pf_vpl::probe::skip(test, &format!("{FIXTURE} absent or without an HEVC track"));
    };
    let mut h = Harness::with_pool(VplDec::new(), 2 << 20, 16).real_wakes();
    h.fix_sink_format(VplDec::IN, "h265/annexb", &[]);
    h.start().expect("start");
    let mut out = 0usize;
    for (i, au) in aus.iter().enumerate() {
        let mut buf = h.alloc(au);
        buf.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(buf).expect("push");
        while h.pull(VplDec::OUT).is_some() {
            out += 1;
        }
    }
    out += h.eos().expect("eos").len();
    let warnings: Vec<String> = h
        .bus_messages()
        .iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .collect();
    eprintln!("{test}: {} access units in, {out} pictures out, warnings {warnings:?}", aus.len());
    assert!(warnings.is_empty(), "{test}: warned {warnings:?}");
    assert_eq!(aus.len(), FRAMES, "{test}: the fixture's frames");
    assert_eq!(out, FRAMES, "{test}: every picture");
}
