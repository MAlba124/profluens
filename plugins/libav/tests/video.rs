//! The video elements against ffmpeg's own decode of the same streams, byte for byte.

// Test code.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libav::{
    LibavH264Dec, LibavH265Dec, LibavMpeg2Dec, LibavMpeg4Dec, LibavVp8Dec, LibavVp9Dec,
};
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::element::Element;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Timestamp;

const FRAME_NS: u64 = 40_000_000;
const SLOT: usize = 4 * 1024 * 1024;

fn at(i: usize) -> Timestamp {
    Timestamp::from_nanos(i as u64 * FRAME_NS)
}

struct Out {
    pts: Timestamp,
    bytes: Vec<u8>,
}

fn take(out: Vec<Buffer>) -> Vec<Out> {
    out.into_iter().map(|b| Out { pts: b.pts, bytes: b.memory.data().to_vec() }).collect()
}

fn feed(h: &mut Harness, units: &[Vec<u8>], range: std::ops::Range<usize>) -> Vec<Out> {
    let mut out = Vec::new();
    for i in range {
        let mut buf = h.alloc(&units[i]);
        buf.pts = at(i);
        buf.duration = Timestamp::from_nanos(FRAME_NS);
        h.push(buf).expect("push");
        out.extend(take(h.drain_outputs()));
    }
    out
}

fn decode_all(h: &mut Harness, units: &[Vec<u8>]) -> Vec<Out> {
    let mut out = feed(h, units, 0..units.len());
    out.extend(take(h.eos().expect("eos")));
    out
}

fn warnings(h: &mut Harness) -> usize {
    h.bus_messages().into_iter().filter(|m| matches!(m, BusMessage::Warning { .. })).count()
}

fn announced_int(h: &Harness, field: &str) -> Option<i64> {
    let f = h.announced()?;
    match f.get(h.vocabulary().field_id(field)?)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

fn announced_id(h: &Harness, field: &str) -> Option<String> {
    let f = h.announced()?;
    let Value::Id(v) = f.get(h.vocabulary().field_id(field)?)? else {
        return None;
    };
    h.vocabulary().value_name(v).map(str::to_string)
}

/// Units are stamped in decode order, so a B-frame stream's come out reordered.
fn check(name: &str, h: &mut Harness, out: &[Out], oracle: &[u8], frame: usize, frames: usize) {
    assert_eq!(oracle.len(), frame * frames, "{name}: oracle size");
    assert_eq!(out.len(), frames, "{name}: frames out");
    for (k, o) in out.iter().enumerate() {
        assert_eq!(o.bytes.len(), frame, "{name}: frame {k} size");
        assert!(
            o.bytes == oracle[k * frame..(k + 1) * frame],
            "{name}: frame {k} differs from ffmpeg"
        );
    }
    let mut stamps: Vec<Timestamp> = out.iter().map(|o| o.pts).collect();
    stamps.sort();
    stamps.dedup();
    assert_eq!(stamps.len(), frames, "{name}: one stamp per unit");
    assert_eq!(warnings(h), 0, "{name}: no warning on a clean stream");
}

fn i420(w: usize, h: usize) -> usize {
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

fn p010(w: usize, h: usize) -> usize {
    2 * w * h + 4 * w.div_ceil(2) * h.div_ceil(2)
}

fn stream(
    name: &str,
    size: &str,
    frames: usize,
    pix: &str,
    codec_args: &[&str],
    format: &str,
) -> Option<Vec<u8>> {
    let src = format!("testsrc2=size={size}:rate=25");
    let n = frames.to_string();
    let mut args = vec!["-f", "lavfi", "-i", &src, "-frames:v", &n, "-pix_fmt", pix];
    args.extend_from_slice(codec_args);
    args.extend_from_slice(&["-f", format]);
    support::make(name, &args)
}

struct Case {
    name: &'static str,
    size: (usize, usize),
    frames: usize,
    pix: &'static str,
    args: &'static [&'static str],
    format: &'static str,
}

fn run_case(c: &Case, element: impl Element + 'static) {
    let size = format!("{}x{}", c.size.0, c.size.1);
    let Some(bytes) = stream(c.name, &size, c.frames, c.pix, c.args, c.format) else {
        return;
    };
    let units = match c.format {
        "ivf" => support::ivf_units(&bytes),
        "hevc" => support::annexb_units(&bytes, true),
        "mpeg2video" | "mpeg1video" => support::mpeg2_units(&bytes),
        "m4v" => support::mpeg4_units(&bytes),
        _ => support::annexb_units(&bytes, false),
    };
    let ten = c.pix.contains("10");
    let (raw_pix, frame) = if ten {
        ("p010le", p010(c.size.0, c.size.1))
    } else {
        ("yuv420p", i420(c.size.0, c.size.1))
    };
    let Some(oracle) = support::oracle(
        c.name,
        &format!("{}.yuv", c.name),
        &["-f", "rawvideo", "-pix_fmt", raw_pix],
    ) else {
        return;
    };
    let mut h = Harness::with_slot_size(element, SLOT);
    let out = decode_all(&mut h, &units);
    check(c.name, &mut h, &out, &oracle, frame, c.frames);
    assert_eq!(announced_int(&h, "width"), Some(c.size.0 as i64), "{}: width", c.name);
    assert_eq!(announced_int(&h, "height"), Some(c.size.1 as i64), "{}: height", c.name);
    let pixfmt = if ten { "p010" } else { "i420" };
    assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some(pixfmt), "{}: pixfmt", c.name);
}

const X264: &[&str] = &["-c:v", "libx264", "-preset", "fast", "-bf", "3", "-g", "12"];
const X265: &[&str] =
    &["-c:v", "libx265", "-preset", "fast", "-x265-params", "bframes=3:keyint=12:log-level=none"];

#[test]
fn h264_high_with_b_frames_matches_ffmpeg() {
    let c = Case {
        name: "h264_high.h264",
        size: (320, 240),
        frames: 30,
        pix: "yuv420p",
        args: X264,
        format: "h264",
    };
    run_case(&c, LibavH264Dec::new());
}

#[test]
fn h264_odd_size_is_cropped_as_ffmpeg_crops_it() {
    let c = Case {
        name: "h264_odd.h264",
        size: (318, 178),
        frames: 10,
        pix: "yuv420p",
        args: X264,
        format: "h264",
    };
    run_case(&c, LibavH264Dec::new());
}

#[test]
fn h264_high10_comes_out_as_p010() {
    let args =
        &["-c:v", "libx264", "-preset", "fast", "-profile:v", "high10", "-bf", "2", "-g", "12"];
    let c = Case {
        name: "h264_high10.h264",
        size: (320, 240),
        frames: 20,
        pix: "yuv420p10le",
        args,
        format: "h264",
    };
    run_case(&c, LibavH264Dec::new());
}

#[test]
fn hevc_main_with_b_frames_matches_ffmpeg() {
    let c = Case {
        name: "hevc_main.hevc",
        size: (320, 240),
        frames: 30,
        pix: "yuv420p",
        args: X265,
        format: "hevc",
    };
    run_case(&c, LibavH265Dec::new());
}

#[test]
fn hevc_main10_comes_out_as_p010() {
    let c = Case {
        name: "hevc_main10.hevc",
        size: (322, 182),
        frames: 20,
        pix: "yuv420p10le",
        args: X265,
        format: "hevc",
    };
    run_case(&c, LibavH265Dec::new());
}

#[test]
fn mpeg2_with_b_frames_matches_ffmpeg() {
    let args = &["-c:v", "mpeg2video", "-q:v", "4", "-bf", "2", "-g", "12"];
    let c = Case {
        name: "mpeg2.m2v",
        size: (320, 240),
        frames: 30,
        pix: "yuv420p",
        args,
        format: "mpeg2video",
    };
    run_case(&c, LibavMpeg2Dec::new());
}

/// MPEG-1 through the MPEG-2 element, against ffmpeg's own mpeg1video decoder.
#[test]
fn mpeg1_with_b_frames_matches_ffmpeg() {
    let args = &["-c:v", "mpeg1video", "-q:v", "4", "-bf", "2", "-g", "12"];
    let c = Case {
        name: "mpeg1.m1v",
        size: (320, 240),
        frames: 30,
        pix: "yuv420p",
        args,
        format: "mpeg1video",
    };
    run_case(&c, LibavMpeg2Dec::new());
}

#[test]
fn mpeg4_asp_with_b_frames_matches_ffmpeg() {
    let args = &["-c:v", "mpeg4", "-q:v", "4", "-bf", "2", "-g", "12"];
    let c = Case {
        name: "mpeg4.m4v",
        size: (320, 240),
        frames: 30,
        pix: "yuv420p",
        args,
        format: "m4v",
    };
    run_case(&c, LibavMpeg4Dec::new());
}

#[test]
fn mpeg4_odd_size_matches_ffmpeg() {
    let args = &["-c:v", "mpeg4", "-q:v", "4", "-g", "12"];
    let c = Case {
        name: "mpeg4_odd.m4v",
        size: (318, 178),
        frames: 12,
        pix: "yuv420p",
        args,
        format: "m4v",
    };
    run_case(&c, LibavMpeg4Dec::new());
}

/// The vop_coding_type of an MPEG-4 Part 2 unit's VOP, 2 for B (14496-2 6.2.5).
fn vop_type(unit: &[u8]) -> Option<u8> {
    let at = unit.windows(4).position(|w| w == [0, 0, 1, 0xB6])?;
    unit.get(at + 4).map(|b| b >> 6)
}

/// AVI stamps in decode order. A unit in a B's place that makes no picture takes no stamp with
/// it, every picture after it keeps the one it has in a whole decode.
#[test]
fn mpeg4_a_unit_that_makes_no_picture_leaves_the_stamps_after_it() {
    let args = &["-c:v", "mpeg4", "-q:v", "4", "-bf", "2", "-g", "12"];
    let Some(bytes) = stream("mpeg4_gap.m4v", "320x240", 30, "yuv420p", args, "m4v") else {
        return;
    };
    let units = support::mpeg4_units(&bytes);
    assert_eq!(units.len(), 30);
    let mut h = Harness::with_slot_size(LibavMpeg4Dec::new(), SLOT);
    let whole = decode_all(&mut h, &units);
    assert_eq!(whole.len(), 30);
    let gap = (4..units.len()).find(|&i| vop_type(&units[i]) == Some(2)).expect("a B unit");
    // user data alone, a unit with no VOP that leaves the decoder as it was
    let mut cut = units.clone();
    cut[gap] = vec![0, 0, 1, 0xB2, b'p', b'f'];
    let mut h = Harness::with_slot_size(LibavMpeg4Dec::new(), SLOT);
    let out = decode_all(&mut h, &cut);
    assert_eq!(out.len(), 29, "the unit makes no picture");
    let mut last = None;
    for o in &out {
        let w = whole.iter().find(|w| w.bytes == o.bytes).expect("a picture of the whole decode");
        assert_eq!(o.pts, w.pts, "a picture stamped as another");
        assert!(last < Some(o.pts), "stamps in order");
        last = Some(o.pts);
    }
}

#[test]
fn vp8_matches_ffmpeg() {
    let args = &["-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-g", "10"];
    let c =
        Case { name: "vp8.ivf", size: (322, 182), frames: 25, pix: "yuv420p", args, format: "ivf" };
    run_case(&c, LibavVp8Dec::new());
}

#[test]
fn vp9_with_hidden_frames_matches_ffmpeg() {
    let args = &[
        "-c:v",
        "libvpx-vp9",
        "-deadline",
        "good",
        "-cpu-used",
        "8",
        "-auto-alt-ref",
        "1",
        "-lag-in-frames",
        "16",
        "-g",
        "20",
    ];
    let c =
        Case { name: "vp9.ivf", size: (320, 240), frames: 30, pix: "yuv420p", args, format: "ivf" };
    run_case(&c, LibavVp9Dec::new());
}

#[test]
fn vp9_profile2_comes_out_as_p010() {
    let args = &[
        "-c:v",
        "libvpx-vp9",
        "-deadline",
        "realtime",
        "-cpu-used",
        "8",
        "-profile:v",
        "2",
        "-g",
        "10",
    ];
    let c = Case {
        name: "vp9_10.ivf",
        size: (320, 240),
        frames: 12,
        pix: "yuv420p10le",
        args,
        format: "ivf",
    };
    run_case(&c, LibavVp9Dec::new());
}

/// An `m4v` stream as an mp4 or mkv demuxer gives it, its VOS, VO and VOL as one config unit and
/// each picture as its GOV and VOP alone.
fn config_and_vops(bytes: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut head = Vec::new();
    let mut units = Vec::new();
    for unit in support::mpeg4_units(bytes) {
        let starts: Vec<usize> = (0..unit.len().saturating_sub(3))
            .filter(|&i| unit[i..i + 3] == [0, 0, 1])
            .chain([unit.len()])
            .collect();
        let mut vop = Vec::new();
        for w in starts.windows(2) {
            let seg = &unit[w[0]..w[1]];
            if matches!(seg.get(3), Some(0xB3 | 0xB6)) {
                vop.extend_from_slice(seg);
            } else if units.is_empty() && vop.is_empty() {
                head.extend_from_slice(seg);
            }
        }
        units.push(vop);
    }
    assert!(head.windows(4).any(|w| w == [0, 0, 1, 0x20]), "a VOL in the head");
    assert!(units.iter().all(|u| !u.windows(4).any(|w| w == [0, 0, 1, 0x20])), "no VOL in band");
    (head, units)
}

fn push_header(h: &mut Harness, head: &[u8], pts: Timestamp) {
    let mut buf = h.alloc(head);
    buf.pts = pts;
    buf.flags = BufferFlags::HEADER;
    h.push(buf).expect("push header");
}

struct ConfigCase {
    head: Vec<u8>,
    units: Vec<Vec<u8>>,
    oracle: Vec<u8>,
}

fn mpeg4_config_case(name: &str, size: (usize, usize), frames: usize) -> Option<ConfigCase> {
    let args = &["-c:v", "mpeg4", "-q:v", "4", "-bf", "2", "-g", "12"];
    let bytes = stream(name, &format!("{}x{}", size.0, size.1), frames, "yuv420p", args, "m4v")?;
    let oracle =
        support::oracle(name, &format!("{name}.yuv"), &["-f", "rawvideo", "-pix_fmt", "yuv420p"])?;
    let (head, units) = config_and_vops(&bytes);
    Some(ConfigCase { head, units, oracle })
}

/// The VOL only in a header unit ahead of VOP-only units, as `mp4v` and `V_MPEG4/ISO/ASP` carry
/// it. Sent again before a keyframe, as after a seek, it changes nothing.
#[test]
fn mpeg4_with_its_vol_in_a_header_unit_matches_ffmpeg() {
    let Some(ConfigCase { head, units, oracle }) =
        mpeg4_config_case("mpeg4_dsi.m4v", (320, 240), 30)
    else {
        return;
    };
    assert_eq!(units.len(), 30);
    let mut h = Harness::with_slot_size(LibavMpeg4Dec::new(), SLOT);
    push_header(&mut h, &head, at(0));
    let mut out = feed(&mut h, &units, 0..12);
    push_header(&mut h, &head, at(12));
    out.extend(feed(&mut h, &units, 12..units.len()));
    out.extend(take(h.eos().expect("eos")));
    check("mpeg4_dsi.m4v", &mut h, &out, &oracle, i420(320, 240), 30);
    assert_eq!(announced_int(&h, "width"), Some(320));
    assert_eq!(announced_int(&h, "height"), Some(240));
}

/// Another header unit drains the decoder and reopens it, here on a new size.
#[test]
fn mpeg4_with_a_new_header_unit_reopens_on_it() {
    let Some(ConfigCase { head: head_a, units: units_a, oracle: oracle_a }) =
        mpeg4_config_case("mpeg4_dsi_a.m4v", (160, 120), 10)
    else {
        return;
    };
    let Some(ConfigCase { head: head_b, units: units_b, oracle: oracle_b }) =
        mpeg4_config_case("mpeg4_dsi_b.m4v", (128, 96), 10)
    else {
        return;
    };
    assert_ne!(head_a, head_b);
    let mut h = Harness::with_slot_size(LibavMpeg4Dec::new(), SLOT);
    push_header(&mut h, &head_a, at(0));
    let mut out = feed(&mut h, &units_a, 0..units_a.len());
    push_header(&mut h, &head_b, at(10));
    for (i, u) in units_b.iter().enumerate() {
        let mut buf = h.alloc(u);
        buf.pts = at(10 + i);
        buf.duration = Timestamp::from_nanos(FRAME_NS);
        h.push(buf).expect("push");
        out.extend(take(h.drain_outputs()));
    }
    out.extend(take(h.eos().expect("eos")));
    let (fa, fb) = (i420(160, 120), i420(128, 96));
    assert_eq!(out.len(), 20, "every picture of both");
    for (k, o) in out.iter().enumerate() {
        let want = if k < 10 {
            &oracle_a[k * fa..(k + 1) * fa]
        } else {
            &oracle_b[(k - 10) * fb..(k - 9) * fb]
        };
        assert!(o.bytes == want, "frame {k} differs from ffmpeg");
        // Stamped in decode order, so only which stream a picture came from is known.
        assert_eq!(o.pts < at(10), k < 10, "frame {k} from its own stream");
    }
    assert_eq!(warnings(&mut h), 0);
    assert_eq!(announced_int(&h, "width"), Some(128));
}

fn reduced(frame: &[u8], (w, h): (usize, usize), shift: (u32, u32), bits: u32) -> Vec<u8> {
    let size = if bits > 8 { 2 } else { 1 };
    let (cols, rows) = (w.div_ceil(1 << shift.0), h.div_ceil(1 << shift.1));
    let at = |base: usize, stride: usize, x: usize, y: usize| -> u32 {
        let i = base + size * (y * stride + x);
        match size {
            2 => u32::from(u16::from_le_bytes([frame[i], frame[i + 1]])),
            _ => u32::from(frame[i]),
        }
    };
    let put = |out: &mut Vec<u8>, v: u32| match size {
        2 => out.extend(((v << (16 - bits)) as u16).to_le_bytes()),
        _ => out.push(v as u8),
    };
    let mut out = Vec::new();
    for y in 0..h {
        for x in 0..w {
            put(&mut out, at(0, w, x, y));
        }
    }
    let (u, v) = (size * w * h, size * (w * h + cols * rows));
    let site = |base: usize, x: usize, y: usize| -> u32 {
        let taps = match shift.0 {
            0 => [(2 * x).saturating_sub(1), 2 * x, (2 * x + 1).min(cols - 1)],
            _ => [x; 3],
        };
        let lines = match shift.1 {
            0 => [2 * y, (2 * y + 1).min(rows - 1)],
            _ => [y; 2],
        };
        let sum: u32 = lines
            .iter()
            .map(|&r| {
                at(base, cols, taps[0], r)
                    + 2 * at(base, cols, taps[1], r)
                    + at(base, cols, taps[2], r)
            })
            .sum();
        (sum + 4) >> 3
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if size == 2 {
        for y in 0..ch {
            for x in 0..cw {
                put(&mut out, site(u, x, y));
                put(&mut out, site(v, x, y));
            }
        }
    } else {
        for base in [u, v] {
            for y in 0..ch {
                for x in 0..cw {
                    put(&mut out, site(base, x, y));
                }
            }
        }
    }
    out
}

fn run_reduced(c: &Case, element: impl Element + 'static, shift: (u32, u32), bits: u32) {
    let size = format!("{}x{}", c.size.0, c.size.1);
    let Some(bytes) = stream(c.name, &size, c.frames, c.pix, c.args, c.format) else {
        return;
    };
    let units = match c.format {
        "ivf" => support::ivf_units(&bytes),
        "hevc" => support::annexb_units(&bytes, true),
        "mpeg2video" => support::mpeg2_units(&bytes),
        "m4v" => support::mpeg4_units(&bytes),
        _ => support::annexb_units(&bytes, false),
    };
    let Some(native) =
        support::oracle(c.name, &format!("{}.yuv", c.name), &["-f", "rawvideo", "-pix_fmt", c.pix])
    else {
        return;
    };
    let (w, h) = c.size;
    let sample = if bits > 8 { 2 } else { 1 };
    let raw = sample * (w * h + 2 * w.div_ceil(1 << shift.0) * h.div_ceil(1 << shift.1));
    assert_eq!(native.len(), raw * c.frames, "{}: oracle size", c.name);
    let oracle: Vec<u8> =
        native.chunks_exact(raw).flat_map(|f| reduced(f, c.size, shift, bits)).collect();
    let (pixfmt, frame) = if bits > 8 { ("p010", p010(w, h)) } else { ("i420", i420(w, h)) };
    let mut h = Harness::with_slot_size(element, SLOT);
    let out = decode_all(&mut h, &units);
    check(c.name, &mut h, &out, &oracle, frame, c.frames);
    assert_eq!(announced_id(&h, "pixfmt").as_deref(), Some(pixfmt), "{}: pixfmt", c.name);
}

#[test]
fn h264_high444_at_10_bits_comes_out_as_p010_reduced_to_420() {
    let args =
        &["-c:v", "libx264", "-preset", "fast", "-profile:v", "high444", "-bf", "2", "-g", "12"];
    let c = Case {
        name: "h264_444_10.h264",
        size: (318, 178),
        frames: 12,
        pix: "yuv444p10le",
        args,
        format: "h264",
    };
    run_reduced(&c, LibavH264Dec::new(), (0, 0), 10);
}

#[test]
fn h264_high422_comes_out_as_i420() {
    let args =
        &["-c:v", "libx264", "-preset", "fast", "-profile:v", "high422", "-bf", "2", "-g", "12"];
    let c = Case {
        name: "h264_422.h264",
        size: (320, 240),
        frames: 12,
        pix: "yuv422p",
        args,
        format: "h264",
    };
    run_reduced(&c, LibavH264Dec::new(), (1, 0), 8);
}

#[test]
fn hevc_444_at_12_bits_comes_out_as_p010() {
    let c = Case {
        name: "hevc_444_12.hevc",
        size: (322, 182),
        frames: 12,
        pix: "yuv444p12le",
        args: X265,
        format: "hevc",
    };
    run_reduced(&c, LibavH265Dec::new(), (0, 0), 12);
}

#[test]
fn vp9_profile1_444_comes_out_as_i420() {
    let args = &[
        "-c:v",
        "libvpx-vp9",
        "-deadline",
        "realtime",
        "-cpu-used",
        "8",
        "-profile:v",
        "1",
        "-g",
        "10",
    ];
    let c = Case {
        name: "vp9_444.ivf",
        size: (320, 240),
        frames: 10,
        pix: "yuv444p",
        args,
        format: "ivf",
    };
    run_reduced(&c, LibavVp9Dec::new(), (0, 0), 8);
}

#[test]
fn the_in_tree_fixtures_match_ffmpeg() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    // ipb_novui.h264 is ipb.h264 with its SPS cut before the VUI, so no bitstream_restriction
    // states the reorder depth and the B pictures behind the first key come out of order.
    let cases: [(&str, bool, &str); 6] = [
        ("plugins/libav/tests/fixtures/ipb.h264", false, "yuv420p"),
        ("plugins/libav/tests/fixtures/ipb_novui.h264", false, "yuv420p"),
        ("plugins/libav/tests/fixtures/ipb.hevc", true, "yuv420p"),
        ("plugins/libav/tests/fixtures/opengop.hevc", true, "yuv420p"),
        ("plugins/libav/tests/fixtures/tiny_i.hevc", true, "yuv420p"),
        ("plugins/libav/tests/fixtures/main10_i.hevc", true, "p010le"),
    ];
    for (path, hevc, pix) in cases {
        let bytes = std::fs::read(format!("{root}/{path}")).expect("fixture");
        let name = path.rsplit('/').next().unwrap();
        std::fs::write(support::tmp(name), &bytes).expect("copy fixture");
        let Some(oracle) =
            support::oracle(name, &format!("{name}.yuv"), &["-f", "rawvideo", "-pix_fmt", pix])
        else {
            return;
        };
        let units = support::annexb_units(&bytes, hevc);
        let mut h = if hevc {
            Harness::with_slot_size(LibavH265Dec::new(), SLOT)
        } else {
            Harness::with_slot_size(LibavH264Dec::new(), SLOT)
        };
        let out = decode_all(&mut h, &units);
        let (w, hh) = (
            announced_int(&h, "width").unwrap() as usize,
            announced_int(&h, "height").unwrap() as usize,
        );
        let frame = if pix == "p010le" { p010(w, hh) } else { i420(w, hh) };
        let frames = oracle.len() / frame;
        check(name, &mut h, &out, &oracle, frame, frames);
    }
}

/// An H.264 mastering display colour volume SEI (D.1.29), BT.2020 primaries in G, B, R order
/// and D65, emulation prevented.
fn mastering_sei(max: u32, min: u32) -> Vec<u8> {
    let mut rbsp = vec![137u8, 24];
    for v in [8500u16, 39850, 6550, 2300, 35400, 14600, 15635, 16450] {
        rbsp.extend(v.to_be_bytes());
    }
    rbsp.extend(max.to_be_bytes());
    rbsp.extend(min.to_be_bytes());
    rbsp.push(0x80);
    let mut nal = vec![0x06];
    let mut zeros = 0;
    for b in rbsp {
        if zeros == 2 && b <= 3 {
            nal.push(3);
            zeros = 0;
        }
        nal.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    nal
}

/// Real releases state a 0 cd/m2 mastering minimum. FFmpeg's strict compliance drops such
/// luminance (h2645_sei.c), the decoder must not run strict.
#[test]
fn h264_mastering_luminance_with_a_zero_minimum_survives() {
    let mut units = support::annexb_units(&fixture("ipb.h264"), false);
    let mut sei = mastering_sei(10_000_000, 0);
    sei.extend_from_slice(&[0, 0, 0, 1]);
    // Every picture states it, one without would announce again without it.
    for unit in &mut units {
        let nals = profluens_video::h264parse::split_nals(unit);
        let slice = nals.iter().find(|n| matches!(n.unit_type, 1 | 5)).expect("a slice");
        let at = slice.start as usize;
        unit.splice(at..at, sei.iter().copied());
    }
    let mut h = Harness::with_slot_size(LibavH264Dec::new(), SLOT);
    let out = decode_all(&mut h, &units);
    assert_eq!(out.len(), units.len(), "every picture");
    assert_eq!(warnings(&mut h), 0);
    let lum = announced_int(&h, "mastering-luminance");
    assert_eq!(lum, Some(10_000_000 << 32), "1000 and 0 cd/m2");
    let rg = [35400u64, 14600, 8500, 39850].iter().fold(0, |a, w| a << 16 | w) as i64;
    assert_eq!(announced_int(&h, "mastering-rg"), Some(rg), "red and green primaries");
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).expect("fixture")
}

/// A 16x16 fixture's pictures on a link taking `takes`, every one, and what was announced.
fn decode_taking(
    element: impl Element + 'static,
    units: &[Vec<u8>],
    takes: &[&'static str],
) -> (Vec<Out>, Option<String>) {
    let mut h = Harness::with_slot_size(element, SLOT);
    if !takes.is_empty() {
        let values: Vec<ValueDesc> = takes.iter().map(|t| ValueDesc::Id(t)).collect();
        h.fix_src_takes(LibavH265Dec::OUT, "pixfmt", &values);
    }
    let out = decode_all(&mut h, units);
    assert_eq!(warnings(&mut h), 0, "no warning on a clean stream");
    (out, announced_id(&h, "pixfmt"))
}

/// A link that takes no p010 or gray8 gets the i420 it takes, narrowed or with neutral chroma,
/// and one taking i422 gets 4:2:2 as decoded. `main10_i.hevc`, `gray_ip.hevc` (x265 4.2, gray)
/// and `i422_ip.h264` (x264 core 165, high422) are committed 16x16 fixtures.
#[test]
fn what_the_link_does_not_take_goes_out_as_the_i420_it_does() {
    let units = support::annexb_units(&fixture("main10_i.hevc"), true);
    let (deep, pixfmt) = decode_taking(LibavH265Dec::new(), &units, &[]);
    assert_eq!(pixfmt.as_deref(), Some("p010"));
    let (out, pixfmt) = decode_taking(LibavH265Dec::new(), &units, &["i420"]);
    assert_eq!(pixfmt.as_deref(), Some("i420"));
    assert_eq!(out.len(), deep.len(), "every picture");
    assert!(!out.is_empty());
    let luma = 16 * 16;
    for (n, (o, d)) in out.iter().zip(&deep).enumerate() {
        assert_eq!(o.bytes.len(), i420(16, 16), "picture {n}: tight i420");
        let want: Vec<u8> = d.bytes[..2 * luma]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|w| ((u32::from(u16::from_le_bytes(*w) >> 6) + 2) >> 2).min(255) as u8)
            .collect();
        assert_eq!(o.bytes[..luma], want[..], "picture {n}: luma rounded to 8 bits");
    }

    let units = support::annexb_units(&fixture("gray_ip.hevc"), true);
    let (gray, pixfmt) = decode_taking(LibavH265Dec::new(), &units, &[]);
    assert_eq!(pixfmt.as_deref(), Some("gray8"));
    let (out, pixfmt) = decode_taking(LibavH265Dec::new(), &units, &["i420"]);
    assert_eq!(pixfmt.as_deref(), Some("i420"));
    assert_eq!(out.len(), 2, "every picture");
    for (n, (o, g)) in out.iter().zip(&gray).enumerate() {
        assert_eq!(o.bytes.len(), i420(16, 16), "picture {n}: tight i420");
        assert_eq!(o.bytes[..luma], g.bytes[..], "picture {n}: luma as decoded");
        assert!(o.bytes[luma..].iter().all(|c| *c == 128), "picture {n}: neutral chroma");
    }

    let units = support::annexb_units(&fixture("i422_ip.h264"), false);
    let (reduced, pixfmt) = decode_taking(LibavH264Dec::new(), &units, &[]);
    assert_eq!(pixfmt.as_deref(), Some("i420"));
    let (out, pixfmt) = decode_taking(LibavH264Dec::new(), &units, &["i420", "i422"]);
    assert_eq!(pixfmt.as_deref(), Some("i422"));
    assert_eq!(out.len(), 2, "every picture");
    for (n, (o, r)) in out.iter().zip(&reduced).enumerate() {
        assert_eq!(o.bytes.len(), luma + 2 * 8 * 16, "picture {n}: tight i422");
        assert_eq!(o.bytes[..luma], r.bytes[..luma], "picture {n}: the same luma");
    }
}

/// `ipb_left8.hevc` is `ipb.hevc` with an 8 pixel left conformance window (ffmpeg 9.0.2,
/// `hevc_metadata=crop_left=8`). Its pictures are the right 8 columns of the uncropped ones.
#[test]
fn a_left_conformance_window_crops_exactly() {
    let units = support::annexb_units(&fixture("ipb.hevc"), true);
    let mut h = Harness::with_slot_size(LibavH265Dec::new(), SLOT);
    let whole = decode_all(&mut h, &units);
    assert_eq!(announced_int(&h, "width"), Some(16));
    let units = support::annexb_units(&fixture("ipb_left8.hevc"), true);
    let mut h = Harness::with_slot_size(LibavH265Dec::new(), SLOT);
    let cropped = decode_all(&mut h, &units);
    assert_eq!(announced_int(&h, "width"), Some(8), "8 columns cropped off the left");
    assert_eq!(announced_int(&h, "height"), Some(16));
    assert_eq!(warnings(&mut h), 0);
    assert_eq!(cropped.len(), whole.len(), "every picture");
    assert!(!whole.is_empty());
    for (n, (c, w)) in cropped.iter().zip(&whole).enumerate() {
        assert_eq!(c.pts, w.pts, "picture {n} stamp");
        assert_eq!(c.bytes.len(), i420(8, 16), "picture {n}: tight i420");
        let mut want = Vec::new();
        (0..16).for_each(|r| want.extend_from_slice(&w.bytes[r * 16 + 8..r * 16 + 16]));
        for base in [256, 256 + 64] {
            (0..8)
                .for_each(|r| want.extend_from_slice(&w.bytes[base + r * 8 + 4..base + r * 8 + 8]));
        }
        assert!(c.bytes == want, "picture {n}: the right columns of the uncropped picture");
    }
}

/// A unit stamped without a duration gives a picture without one, not a zero length one.
#[test]
fn a_unit_without_a_duration_makes_a_picture_without_one() {
    let units = support::annexb_units(&fixture("ipb.hevc"), true);
    let run = |duration: Timestamp| {
        let mut h = Harness::with_slot_size(LibavH265Dec::new(), SLOT);
        let mut out = Vec::new();
        for (i, u) in units.iter().enumerate() {
            let mut buf = h.alloc(u);
            buf.pts = at(i);
            buf.duration = duration;
            h.push(buf).expect("push");
            out.extend(h.drain_outputs());
        }
        out.extend(h.eos().expect("eos"));
        assert_eq!(out.len(), units.len(), "every picture");
        out.iter().map(|b| b.duration).collect::<Vec<_>>()
    };
    assert!(run(Timestamp::NONE).iter().all(|d| d.is_none()), "unknown stays unknown");
    let known = Timestamp::from_nanos(FRAME_NS);
    assert!(run(known).iter().all(|d| *d == known), "a known duration rides along");
}

#[test]
fn a_dry_pool_holds_the_decoder_back_and_loses_nothing() {
    let c = Case {
        name: "h264_dry.h264",
        size: (320, 240),
        frames: 30,
        pix: "yuv420p",
        args: X264,
        format: "h264",
    };
    let Some(bytes) = stream(c.name, "320x240", c.frames, c.pix, c.args, c.format) else {
        return;
    };
    let Some(oracle) =
        support::oracle(c.name, "h264_dry.yuv", &["-f", "rawvideo", "-pix_fmt", "yuv420p"])
    else {
        return;
    };
    let units = support::annexb_units(&bytes, false);
    let mut h = Harness::with_pool(LibavH264Dec::new(), SLOT, 4);
    let mut out = Vec::new();
    let mut i = 0;
    let mut stalls = 0;
    while i < units.len() {
        if h.input_len() == 0 {
            let mut buf = h.alloc(&units[i]);
            buf.pts = at(i);
            buf.duration = Timestamp::from_nanos(FRAME_NS);
            h.push(buf).expect("push");
            i += 1;
        } else {
            h.crank().expect("crank");
            stalls += 1;
            assert!(stalls < 10_000, "the decoder never takes its input");
        }
        out.extend(take(h.drain_outputs()));
    }
    out.extend(take(h.eos().expect("eos")));
    check(c.name, &mut h, &out, &oracle, i420(320, 240), c.frames);
}

#[test]
fn a_flush_resumes_at_the_next_keyframe() {
    let c = Case {
        name: "h264_flush.h264",
        size: (160, 120),
        frames: 24,
        pix: "yuv420p",
        args: X264,
        format: "h264",
    };
    let Some(bytes) = stream(c.name, "160x120", c.frames, c.pix, c.args, c.format) else {
        return;
    };
    let Some(oracle) =
        support::oracle(c.name, "h264_flush.yuv", &["-f", "rawvideo", "-pix_fmt", "yuv420p"])
    else {
        return;
    };
    let units = support::annexb_units(&bytes, false);
    let mut h = Harness::with_slot_size(LibavH264Dec::new(), SLOT);
    let _ = feed(&mut h, &units, 0..7);
    h.seek(0, Timestamp::ZERO).expect("seek");
    let _ = h.drain_outputs();
    let out = decode_all(&mut h, &units);
    check(c.name, &mut h, &out, &oracle, i420(160, 120), c.frames);
}

#[test]
fn frames_before_the_presentation_point_are_not_put_out() {
    let args = &["-c:v", "libx264", "-preset", "fast", "-bf", "0", "-g", "12"];
    let c = Case {
        name: "h264_skip.h264",
        size: (160, 120),
        frames: 24,
        pix: "yuv420p",
        args,
        format: "h264",
    };
    let Some(bytes) = stream(c.name, "160x120", c.frames, c.pix, c.args, c.format) else {
        return;
    };
    let Some(oracle) =
        support::oracle(c.name, "h264_skip.yuv", &["-f", "rawvideo", "-pix_fmt", "yuv420p"])
    else {
        return;
    };
    let units = support::annexb_units(&bytes, false);
    let mut h = Harness::with_slot_size(LibavH264Dec::new(), SLOT);
    h.skip_to(at(10)).expect("skip");
    let out = decode_all(&mut h, &units);
    let frame = i420(160, 120);
    assert_eq!(out.len(), 14, "frames from the point on");
    for (k, o) in out.iter().enumerate() {
        assert_eq!(o.pts, at(10 + k));
        assert!(o.bytes == oracle[(10 + k) * frame..(11 + k) * frame], "frame {} differs", 10 + k);
    }
}

#[test]
fn garbage_warns_and_the_stream_goes_on() {
    let c = Case {
        name: "h264_garbage.h264",
        size: (160, 120),
        frames: 12,
        pix: "yuv420p",
        args: X264,
        format: "h264",
    };
    let Some(bytes) = stream(c.name, "160x120", c.frames, c.pix, c.args, c.format) else {
        return;
    };
    let mut units = support::annexb_units(&bytes, false);
    let junk: Vec<u8> = [0u8, 0, 0, 1, 0x65]
        .iter()
        .copied()
        .chain((0..4000u32).map(|i| (i * 7919 % 251) as u8))
        .collect();
    units.insert(5, junk);
    let mut h = Harness::with_slot_size(LibavH264Dec::new(), SLOT);
    let out = decode_all(&mut h, &units);
    assert!(out.len() >= 10, "the stream went on, {} frames", out.len());
}

/// Pictures of `units` before a seek after `cut` of them, then of the whole stream after it.
fn decode_seeked(
    element: impl Element + 'static,
    threads: i64,
    units: &[Vec<u8>],
    cut: usize,
) -> (Vec<Out>, Vec<Out>) {
    let mut h = Harness::with_slot_size(element, SLOT);
    assert!(h.set_prop("threads", Value::Int(threads)), "a declared property");
    let mut before = feed(&mut h, units, 0..cut);
    before.extend(take(h.drain_outputs()));
    h.seek(0, Timestamp::ZERO).expect("seek");
    let _ = h.drain_outputs();
    let after = decode_all(&mut h, units);
    assert_eq!(warnings(&mut h), 0, "{threads} threads: no warning");
    (before, after)
}

/// Frame threads put out what one thread does, each picture `threads - 1` units later, so fewer
/// before the seek's flush drops what is in flight.
fn threads_match<E: Element + 'static>(c: &Case, make: fn() -> E, threads: i64) {
    let size = format!("{}x{}", c.size.0, c.size.1);
    let Some(bytes) = stream(c.name, &size, c.frames, c.pix, c.args, c.format) else {
        return;
    };
    let units = match c.format {
        "m4v" => support::mpeg4_units(&bytes),
        "hevc" => support::annexb_units(&bytes, true),
        _ => support::annexb_units(&bytes, false),
    };
    let cut = units.len() / 2;
    let (one_before, one_after) = decode_seeked(make(), 1, &units, cut);
    let (many_before, many_after) = decode_seeked(make(), threads, &units, cut);
    let name = c.name;
    assert_eq!(one_after.len(), c.frames, "{name}: every picture after the seek");
    assert!(many_before.len() < one_before.len(), "{name}: frame threads delay");
    assert!(many_before.len() + threads as usize > one_before.len(), "{name}: by threads - 1");
    let pairs = one_before.iter().zip(&many_before).chain(one_after.iter().zip(&many_after));
    for (k, (a, b)) in pairs.enumerate() {
        assert_eq!(a.pts, b.pts, "{name}: picture {k} stamp");
        assert!(a.bytes == b.bytes, "{name}: picture {k} pixels");
    }
    assert_eq!(many_after.len(), one_after.len(), "{name}: pictures after the seek");
}

#[test]
fn h264_frame_threads_match_one_thread_across_a_seek_and_the_end() {
    let high10 =
        &["-c:v", "libx264", "-preset", "fast", "-profile:v", "high10", "-bf", "3", "-g", "12"];
    let high444 =
        &["-c:v", "libx264", "-preset", "fast", "-profile:v", "high444", "-bf", "2", "-g", "12"];
    let high422 =
        &["-c:v", "libx264", "-preset", "fast", "-profile:v", "high422", "-bf", "2", "-g", "12"];
    let cases = [
        Case {
            name: "h264_threads.h264",
            size: (320, 240),
            frames: 30,
            pix: "yuv420p",
            args: X264,
            format: "h264",
        },
        Case {
            name: "h264_threads_10.h264",
            size: (320, 240),
            frames: 30,
            pix: "yuv420p10le",
            args: high10,
            format: "h264",
        },
        Case {
            name: "h264_threads_444_10.h264",
            size: (318, 178),
            frames: 24,
            pix: "yuv444p10le",
            args: high444,
            format: "h264",
        },
        Case {
            name: "h264_threads_422.h264",
            size: (320, 240),
            frames: 24,
            pix: "yuv422p",
            args: high422,
            format: "h264",
        },
    ];
    for c in &cases {
        threads_match(c, LibavH264Dec::new, 4);
    }
}

/// MPEG-4 Part 2 takes its stamps in decode order, so the held ones cover the threads' delay too.
#[test]
fn mpeg4_frame_threads_keep_the_decode_order_stamps() {
    let c = Case {
        name: "mpeg4_threads.m4v",
        size: (320, 240),
        frames: 30,
        pix: "yuv420p",
        args: &["-c:v", "mpeg4", "-q:v", "4", "-bf", "2", "-g", "12"],
        format: "m4v",
    };
    threads_match(&c, LibavMpeg4Dec::new, 3);
}

#[test]
fn hevc_frame_threads_match_one_thread() {
    let c = Case {
        name: "hevc_threads.hevc",
        size: (320, 240),
        frames: 24,
        pix: "yuv420p",
        args: X265,
        format: "hevc",
    };
    threads_match(&c, LibavH265Dec::new, 4);
}

/// An MP4 or Matroska demuxer hands the parameter sets of its codec head as a unit of their own.
/// Frame threads would fail it on a later unit's call and drop that unit's report as its own.
#[test]
fn h264_frame_threads_take_a_unit_of_parameter_sets_alone() {
    let Some(bytes) = stream("h264_head.h264", "320x240", 12, "yuv420p", X264, "h264") else {
        return;
    };
    let units = support::annexb_units(&bytes, false);
    let first = &units[0];
    let idr = (3..first.len()).find(|&i| first[i - 3..i] == [0, 0, 1] && first[i] & 0x1F == 5);
    let slice = idr.expect("an IDR slice") - 4;
    let mut split = vec![first[..slice].to_vec(), first[slice..].to_vec()];
    split.extend(units[1..].iter().cloned());
    let run = |threads: i64| {
        let mut h = Harness::with_slot_size(LibavH264Dec::new(), SLOT);
        assert!(h.set_prop("threads", Value::Int(threads)));
        let out = decode_all(&mut h, &split);
        assert_eq!(warnings(&mut h), 0, "{threads} threads: no unit failed");
        out
    };
    let (one, four) = (run(1), run(4));
    assert_eq!(one.len(), 12);
    assert_eq!(four.len(), one.len());
    for (k, (a, b)) in one.iter().zip(&four).enumerate() {
        assert_eq!(a.pts, b.pts, "picture {k} stamp");
        assert!(a.bytes == b.bytes, "picture {k} pixels");
    }
}
