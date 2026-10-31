//! An H.264 encoder keyed by stamps opens an IDR at each stamp and nowhere else, on irregular
//! frame times with a scene cut between keys. Skips without the runtime.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use std::sync::Arc;

use pf_harness::Harness;
use pf_vpl::{Settings, VplEnc};
use profluens_core::buffer::BufferFlags;
use profluens_core::format::ValueDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

use common::hw;

const MS: u64 = 1_000_000;
const FRAMES: usize = 120;
/// Frame indexes keyed, two of them adjacent and one gap past the 48 frame plan GOP.
const KEYED: [usize; 7] = [0, 7, 19, 20, 41, 100, 113];
/// A hard cut no key is asked for, where an adaptive I frame would go.
const CUT: usize = 60;

fn i420(w: usize, h: usize, shade: u8) -> Vec<u8> {
    let mut f = vec![shade; w * h];
    for (i, px) in f.iter_mut().enumerate() {
        *px = px.wrapping_add((i % w) as u8 / 4);
    }
    f.resize(w * h + 2 * w.div_ceil(2) * h.div_ceil(2), 128);
    f
}

/// Irregular frame times, whole milliseconds so the 90 kHz stamps map back exactly.
fn stamps() -> Vec<u64> {
    let steps = [40, 33, 47, 20, 61, 25];
    let mut at = 0;
    (0..FRAMES)
        .map(|i| {
            let t = at;
            at += steps[i % steps.len()] * MS;
            t
        })
        .collect()
}

#[test]
fn keyed_stamps_open_an_idr_exactly_at_each_key() {
    let test = "keyed_stamps_open_an_idr_exactly_at_each_key";
    let Some(caps) = hw(test) else { return };
    if !caps.supports_encode("h264/annexb") {
        return common::skip(test, "the runtime does not encode h264");
    }
    let pts = stamps();
    // Keys off the frame times by less than the tolerance, as a copy's keys are.
    let keys: Arc<[i64]> =
        KEYED.iter().enumerate().map(|(n, &i)| (pts[i] + (n as u64 % 3) * MS) as i64).collect();
    let settings = Settings {
        bitrate_kbps: 800,
        max_kbps: 0,
        gop: 48,
        bframes: 0,
        lookahead: 0,
        rc: pf_vpl::Rc::Vbr,
        hrd_ms: 1000,
        ..Settings::default()
    };
    let enc = VplEnc::new()
        .with_family("h264/annexb")
        .with_settings(settings)
        .with_keys(keys, 5 * MS as i64);
    let mut e = Harness::with_pool(enc, 4 << 20, 16).real_wakes();
    let raw = [
        ("width", ValueDesc::Int(320)),
        ("height", ValueDesc::Int(240)),
        ("pixfmt", ValueDesc::Id("i420")),
        ("fps", ValueDesc::Rat(30, 1)),
    ];
    e.fix_sink_format(SinkPad(0), "video/raw", &raw);
    e.fix_src_format(
        SrcPad(0),
        "h264/annexb",
        &[("width", ValueDesc::Int(320)), ("height", ValueDesc::Int(240))],
    );
    e.start().expect("start");
    let (dark, bright) = (i420(320, 240, 30), i420(320, 240, 200));
    // what each packet says, its buffer goes back to the pool of 16
    let packet = |p: &profluens_core::buffer::Buffer| {
        let key = p.flags.contains(BufferFlags::KEYFRAME);
        (p.pts.0, key, key && pf_vpl::nal::has_sps(p.memory.data(), false))
    };
    let mut out = Vec::new();
    for (i, &t) in pts.iter().enumerate() {
        let mut b = e.alloc(if i < CUT { &dark } else { &bright });
        b.pts = Timestamp::from_nanos(t);
        b.duration = Timestamp::from_nanos(20 * MS);
        e.push(b).expect("push");
        while let Some(p) = e.pull(SrcPad(0)) {
            out.push(packet(&p));
        }
    }
    out.extend(e.eos().expect("eos").iter().map(packet));
    let warnings: Vec<String> = e
        .bus_messages()
        .iter()
        .filter_map(|m| match m {
            profluens_core::bus::BusMessage::Warning { error, .. }
            | profluens_core::bus::BusMessage::Error { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .collect();
    assert!(warnings.is_empty(), "{test}: warned {warnings:?}");
    assert_eq!(out.len(), FRAMES, "{test}: every frame");
    let mut got: Vec<u64> = out.iter().filter(|&&(_, key, _)| key).map(|&(pts, ..)| pts).collect();
    got.sort_unstable();
    let want: Vec<u64> = KEYED.iter().map(|&i| pts[i]).collect();
    assert_eq!(got, want, "{test}: keyframes exactly at the keyed frames");
    for &(pts, _, sps) in out.iter().filter(|&&(_, key, _)| key) {
        assert!(sps, "{test}: the IDR at {pts} carries its parameter sets");
    }
}
