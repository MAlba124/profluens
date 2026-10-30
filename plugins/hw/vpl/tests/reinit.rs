//! Changes midstream. A decoder meeting a larger sequence, an encoder fed a new size, a forced
//! keyframe. Every test skips without the runtime.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

use pf_harness::Harness;
use pf_vpl::session::live_sessions;
use pf_vpl::{Settings, VplEnc};
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::event::Event;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

use common::{Codec, FRAME_NS, decode_hw, hw, hw_decodes};

const SMALL_HEVC: &[u8] = include_bytes!("../../vaapi/tests/fixtures/p_160x96.hevc");

fn i420(w: usize, h: usize, shade: u8) -> Vec<u8> {
    let mut f = vec![shade; w * h];
    f.resize(w * h + 2 * w.div_ceil(2) * h.div_ceil(2), 128);
    f
}

fn raw_fields(w: i64, h: i64) -> [(&'static str, ValueDesc); 4] {
    [
        ("width", ValueDesc::Int(w)),
        ("height", ValueDesc::Int(h)),
        ("pixfmt", ValueDesc::Id("i420")),
        ("fps", ValueDesc::Rat(25, 1)),
    ]
}

fn encoder(settings: Settings, w: i64, h: i64) -> Harness {
    let mut e = Harness::with_pool(VplEnc::new().with_settings(settings), 4 << 20, 16).real_wakes();
    e.fix_sink_format(SinkPad(0), "video/raw", &raw_fields(w, h));
    e.fix_src_format(
        SrcPad(0),
        "h265/annexb",
        &[("width", ValueDesc::Int(w)), ("height", ValueDesc::Int(h))],
    );
    e.start().expect("start");
    e
}

fn push_frames(e: &mut Harness, out: &mut Vec<Buffer>, frame: &[u8], from: u64, n: u64) {
    for i in from..from + n {
        let mut b = e.alloc(frame);
        b.pts = Timestamp::from_nanos(i * FRAME_NS);
        b.duration = Timestamp::from_nanos(FRAME_NS);
        e.push(b).expect("push");
        while let Some(p) = e.pull(SrcPad(0)) {
            out.push(p);
        }
    }
}

fn said(e: &mut Harness) -> Vec<String> {
    e.bus_messages()
        .iter()
        .filter_map(|m| match m {
            profluens_core::bus::BusMessage::Warning { error, .. }
            | profluens_core::bus::BusMessage::Error { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_larger_sequence_midstream_loses_no_picture_on_either_side() {
    let test = "a_larger_sequence_midstream_loses_no_picture_on_either_side";
    if hw_decodes(test, Codec::H265).is_none() {
        return;
    }
    let small = Codec::H265.units(SMALL_HEVC);
    let large = Codec::H265.units(common::IPB_HEVC);
    let (a, b) = (decode_hw(Codec::H265, &small), decode_hw(Codec::H265, &large));
    assert!(a.warnings.is_empty() && b.warnings.is_empty(), "{test}: alone");
    let both: Vec<Vec<u8>> = small.iter().chain(&large).cloned().collect();
    let d = decode_hw(Codec::H265, &both);
    eprintln!(
        "{test}: {} + {} pictures alone, {} across the change",
        a.frames.len(),
        b.frames.len(),
        d.frames.len()
    );
    assert!(d.warnings.is_empty(), "{test}: warned {:?}", d.warnings);
    assert_eq!(d.frames.len(), a.frames.len() + b.frames.len(), "{test}: every picture");
    let (small_len, large_len) = (160 * 96 * 3 / 2, 320 * 240 * 3 / 2);
    for (i, f) in d.frames.iter().enumerate() {
        let want = if i < a.frames.len() { small_len } else { large_len };
        assert_eq!(f.len(), want, "{test}: picture {i} at its sequence's size");
    }
    assert_eq!(&d.frames[..a.frames.len()], &a.frames[..], "{test}: the small tail intact");
}

#[test]
fn an_encoder_fed_a_new_size_drains_the_old_session_and_closes_it() {
    let test = "an_encoder_fed_a_new_size_drains_the_old_session_and_closes_it";
    let Some(caps) = hw(test) else { return };
    if !caps.supports_encode("h265/annexb") {
        return common::skip(test, "the runtime does not encode h265");
    }
    let base = live_sessions();
    let settings = Settings { gop: 30, bframes: 3, ..Settings::default() };
    let mut e = encoder(settings, 320, 240);
    let mut out = Vec::new();
    push_frames(&mut e, &mut out, &i420(320, 240, 90), 0, 12);
    // Gen12 HEVC encode wants 128 lines or more, 160x96 is refused.
    e.change_sink_format(SinkPad(0), "video/raw", &raw_fields(256, 192)).expect("format");
    push_frames(&mut e, &mut out, &i420(256, 192, 160), 12, 12);
    out.extend(e.eos().expect("eos"));
    let warnings = said(&mut e);
    assert!(warnings.is_empty(), "{test}: warned {warnings:?}");
    assert_eq!(out.len(), 24, "{test}: every frame of both sizes encoded");
    let keys: Vec<u64> = out
        .iter()
        .filter(|p| p.flags.contains(BufferFlags::KEYFRAME))
        .map(|p| p.pts.0 / FRAME_NS)
        .collect();
    assert!(keys.contains(&0) && keys.contains(&12), "{test}: each size opens on an IDR {keys:?}");
    let mut pts: Vec<u64> = out.iter().map(|p| p.pts.0 / FRAME_NS).collect();
    pts.sort_unstable();
    assert_eq!(pts, (0..24).collect::<Vec<_>>(), "{test}: each frame once");
    assert_eq!(live_sessions() - base, 1, "{test}: only the open session is left");
    e.stop();
    assert_eq!(live_sessions(), base, "{test}: stop closes it");
}

#[test]
fn a_forced_keyframe_lands_on_the_next_frame() {
    let test = "a_forced_keyframe_lands_on_the_next_frame";
    let Some(caps) = hw(test) else { return };
    if !caps.supports_encode("h265/annexb") {
        return common::skip(test, "the runtime does not encode h265");
    }
    let settings = Settings { gop: 300, bframes: 0, lookahead: 0, ..Settings::default() };
    let mut e = encoder(settings, 320, 240);
    let mut out = Vec::new();
    push_frames(&mut e, &mut out, &i420(320, 240, 90), 0, 15);
    e.push_event(Event::PropChanged { name: "force-keyframe", value: Value::Int(1) })
        .expect("force");
    push_frames(&mut e, &mut out, &i420(320, 240, 90), 15, 15);
    out.extend(e.eos().expect("eos"));
    let warnings = said(&mut e);
    assert!(warnings.is_empty(), "{test}: warned {warnings:?}");
    assert_eq!(out.len(), 30, "{test}: every frame");
    let keys: Vec<u64> = out
        .iter()
        .filter(|p| p.flags.contains(BufferFlags::KEYFRAME))
        .map(|p| p.pts.0 / FRAME_NS)
        .collect();
    assert_eq!(keys, [0, 15], "{test}: the stream opens on an IDR and the forced one follows");
    let forced = out.iter().find(|p| p.pts.0 == 15 * FRAME_NS).expect("frame 15");
    assert!(
        pf_vpl::nal::has_sps(forced.memory.data(), true),
        "{test}: the forced IDR carries its parameter sets"
    );
}
