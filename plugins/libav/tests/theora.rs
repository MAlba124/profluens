//! `libavtheoradec` against the system ffmpeg's decode of an Ogg Theora stream made with
//! libtheora, byte for byte. The fixture is made at test time, skipped where ffmpeg or libtheora
//! is missing:
//!
//! ```text
//! ffmpeg -f lavfi -i "testsrc2=size=160x120:rate=15:duration=1" -c:v libtheora -q:v 5 -f ogg tiny.ogv
//! ffmpeg -i tiny.ogv -fps_mode passthrough -f rawvideo -pix_fmt yuv420p tiny.ogv.yuv
//! ```
//!
//! 120 rows are not a whole number of 16-row macroblocks, so the decode crops as ffmpeg's does.

// Test code.
#![allow(clippy::disallowed_methods)]

mod support;

use pf_harness::Harness;
use pf_libav::LibavTheoraDec;
use profluens_core::bus::BusMessage;
use profluens_core::format::Value;
use profluens_core::time::Timestamp;

const SLOT: usize = 1024 * 1024;
const FRAME_NS: u64 = 1_000_000_000 / 15;
const FRAMES: usize = 15;
const FRAME: usize = 160 * 120 * 3 / 2;

fn fixture() -> Option<(Vec<Vec<u8>>, Vec<u8>)> {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x120:rate=15:duration=1",
        "-c:v",
        "libtheora",
        "-q:v",
        "5",
        "-f",
        "ogg",
    ];
    let ogg = support::make("tiny.ogv", &args)?;
    let yuv =
        support::oracle("tiny.ogv", "tiny.ogv.yuv", &["-f", "rawvideo", "-pix_fmt", "yuv420p"])?;
    let packets = pf_ogg::demux_all(&ogg).into_iter().map(|p| p.data).collect();
    Some((packets, yuv))
}

fn announced(h: &Harness, field: &str) -> Option<i64> {
    match h.announced()?.get(h.vocabulary().field_id(field)?)? {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

fn warnings(h: &mut Harness) -> usize {
    h.bus_messages().into_iter().filter(|m| matches!(m, BusMessage::Warning { .. })).count()
}

/// The pictures of `units`, a frame stamped by its place among the frames from `first` on.
fn decode(h: &mut Harness, units: &[Vec<u8>], first: usize) -> Vec<(Timestamp, Vec<u8>)> {
    let mut out = Vec::new();
    let mut frame = first;
    for u in units {
        let mut buf = h.alloc(u);
        if u.first().is_some_and(|b| b & 0x80 == 0) {
            buf.pts = Timestamp::from_nanos(frame as u64 * FRAME_NS);
            frame += 1;
        }
        h.push(buf).expect("push");
        out.extend(h.drain_outputs().into_iter().map(|b| (b.pts, b.memory.data().to_vec())));
    }
    out.extend(h.eos().expect("eos").into_iter().map(|b| (b.pts, b.memory.data().to_vec())));
    out
}

#[test]
fn in_band_headers_decode_as_ffmpeg_does() {
    let Some((units, want)) = fixture() else { return };
    assert_eq!([units[0][0], units[1][0], units[2][0]], [0x80, 0x81, 0x82]);
    assert_eq!(units.len(), 3 + FRAMES);
    assert_eq!(want.len(), FRAME * FRAMES, "the oracle");
    let mut h = Harness::with_slot_size(LibavTheoraDec::new(), SLOT);
    let got = decode(&mut h, &units, 0);
    assert_eq!(got.len(), FRAMES, "one picture a frame");
    for (k, (pts, bytes)) in got.iter().enumerate() {
        assert_eq!(*pts, Timestamp::from_nanos(k as u64 * FRAME_NS), "frame {k}");
        assert!(bytes[..] == want[k * FRAME..(k + 1) * FRAME], "frame {k} differs from ffmpeg");
    }
    assert_eq!((announced(&h, "width"), announced(&h, "height")), (Some(160), Some(120)));
    assert_eq!(warnings(&mut h), 0);
}

/// The headers again behind a flush, as the demuxer sends them, keep the open decoder.
#[test]
fn the_headers_outlast_a_flush_and_again_change_nothing() {
    let Some((units, want)) = fixture() else { return };
    for again in [false, true] {
        let mut h = Harness::with_slot_size(LibavTheoraDec::new(), SLOT);
        let _ = decode(&mut h, &units[..8], 0);
        h.seek(0, Timestamp::ZERO).expect("seek");
        let _ = h.drain_outputs();
        let mut input: Vec<Vec<u8>> = if again { units[..3].to_vec() } else { Vec::new() };
        input.extend_from_slice(&units[3..]);
        let got = decode(&mut h, &input, 0);
        assert_eq!(got.len(), FRAMES, "again {again}");
        for (k, (_, bytes)) in got.iter().enumerate() {
            assert!(bytes[..] == want[k * FRAME..(k + 1) * FRAME], "again {again}: frame {k}");
        }
        assert_eq!(warnings(&mut h), 0, "again {again}");
    }
}

#[test]
fn frames_ahead_of_the_headers_are_dropped() {
    let Some((units, _)) = fixture() else { return };
    let mut h = Harness::with_slot_size(LibavTheoraDec::new(), SLOT);
    assert!(decode(&mut h, &units[3..], 0).is_empty(), "no headers, no pictures");
    assert_eq!(warnings(&mut h), 0, "a drop is logged, never posted");

    let mut h = Harness::with_slot_size(LibavTheoraDec::new(), SLOT);
    let late: Vec<Vec<u8>> = units[3..6].iter().chain(&units[..3]).cloned().collect();
    let mut input = late;
    input.extend_from_slice(&units[3..]);
    assert_eq!(decode(&mut h, &input, 0).len(), FRAMES, "the headers open it late");
}

#[test]
fn hostile_headers_never_panic() {
    let Some((units, _)) = fixture() else { return };
    let (ident, setup) = (&units[0], &units[2]);
    let mut heads: Vec<[Vec<u8>; 2]> = Vec::new();
    for n in [7, 8, 20, 41] {
        heads.push([ident[..n].to_vec(), setup.clone()]);
    }
    for n in [7, 8, 40, setup.len() / 2, setup.len() - 1] {
        heads.push([ident.clone(), setup[..n].to_vec()]);
    }
    for at in [7, 9, 10, 14, 20, 22, 30] {
        let mut bad = ident.clone();
        bad[at] ^= 0xA5;
        heads.push([bad, setup.clone()]);
        let mut bad = setup.clone();
        bad[at * 3] ^= 0xA5;
        heads.push([ident.clone(), bad]);
    }
    for [i, s] in &heads {
        let mut h = Harness::with_slot_size(LibavTheoraDec::new(), SLOT);
        let mut input = vec![i.clone(), units[1].clone(), s.clone()];
        input.extend_from_slice(&units[3..]);
        let _ = decode(&mut h, &input, 0);
        let _ = warnings(&mut h);
    }
}

/// 2 s at 15 fps, a keyframe at most every 5 frames.
fn gop_fixture() -> Option<(Vec<Vec<u8>>, Vec<u8>)> {
    let args = [
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x120:rate=15:duration=2",
        "-c:v",
        "libtheora",
        "-q:v",
        "5",
        "-g",
        "5",
        "-f",
        "ogg",
    ];
    let ogg = support::make("gop.ogv", &args)?;
    let yuv =
        support::oracle("gop.ogv", "gop.ogv.yuv", &["-f", "rawvideo", "-pix_fmt", "yuv420p"])?;
    let packets = pf_ogg::demux_all(&ogg).into_iter().map(|p| p.data).collect();
    Some((packets, yuv))
}

/// Pushes `unit` stamped as frame `frame`, the pictures out so far.
fn push(h: &mut Harness, unit: &[u8], frame: usize) -> Vec<(Timestamp, Vec<u8>)> {
    let mut buf = h.alloc(unit);
    buf.pts = Timestamp::from_nanos(frame as u64 * FRAME_NS);
    h.push(buf).expect("push");
    h.drain_outputs().into_iter().map(|b| (b.pts, b.memory.data().to_vec())).collect()
}

/// An inter frame with no reference pictures, behind a flush or first after the headers, makes
/// no picture where vp3.c would decode it against one never written. The next intra frame and
/// every frame after decode as ffmpeg's decode of the whole stream has them, an intra frame
/// refreshing every reference.
#[test]
fn inter_frames_without_references_wait_for_an_intra_frame() {
    let Some((units, want)) = gop_fixture() else { return };
    let (heads, frames) = units.split_at(3);
    assert_eq!(want.len(), FRAME * frames.len(), "the oracle");
    // Theora I 7.1, the frame type in the bit after the packet type
    let intra = |u: &[u8]| u.first().is_some_and(|b| b & 0xC0 == 0);
    let key = (1..frames.len())
        .find(|&k| intra(&frames[k]) && !intra(&frames[k - 1]))
        .expect("an intra frame behind an inter one");
    for flushed in [true, false] {
        let mut h = Harness::with_slot_size(LibavTheoraDec::new(), SLOT);
        for u in heads {
            assert!(push(&mut h, u, 0).is_empty(), "a header makes no picture");
        }
        if flushed {
            let _ = decode(&mut h, &frames[..key - 1], 0);
            h.seek(0, Timestamp::ZERO).expect("seek");
            let _ = h.drain_outputs();
        }
        let what = format!("flushed {flushed}");
        assert!(push(&mut h, &frames[key - 1], key - 1).is_empty(), "{what}: the inter frame");
        let mut got = push(&mut h, &frames[key], key);
        for (i, u) in frames.iter().enumerate().skip(key + 1) {
            got.extend(push(&mut h, u, i));
        }
        got.extend(h.eos().expect("eos").into_iter().map(|b| (b.pts, b.memory.data().to_vec())));
        assert_eq!(got.len(), frames.len() - key, "{what}: a picture a frame from the intra one");
        for (i, (pts, bytes)) in got.iter().enumerate() {
            let k = key + i;
            assert_eq!(*pts, Timestamp::from_nanos(k as u64 * FRAME_NS), "{what}: frame {k}");
            assert!(bytes[..] == want[k * FRAME..(k + 1) * FRAME], "{what}: frame {k} differs");
        }
        assert_eq!(warnings(&mut h), 0, "{what}: a drop is logged, never posted");
    }
}
