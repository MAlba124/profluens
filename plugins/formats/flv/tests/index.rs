//! The seek index from `keyframes` and grown without it, and seeks to an index byte and to
//! a byte inside a tag, which a resync reads on from.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{MS, Packet, fixture, of, packets, run};
use pf_flv::FlvDemux;
use pf_harness::Harness;
use pf_sim::corrupt::harness::Shim;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::time::Timestamp;

#[test]
fn keyframes_metadata_is_the_index() {
    let data = fixture("h264_aac");
    let out = run(&data, &data, 4096);
    let index = out.index.expect("published at preroll");
    assert!(index.complete && index.grouped);
    assert_eq!((index.stream, index.file_len), (Some(0), Some(data.len() as u64)));
    assert_eq!(index.duration.nanos(), Some(4_133_000_000), "as onMetaData states");
    let keys: Vec<(u64, u64)> = of(&packets("h264_aac"), 0)
        .iter()
        .filter(|p| p.key)
        .map(|p| (p.dts as u64 * MS, p.pos))
        .collect();
    assert_eq!(keys.len(), 4);
    let got: Vec<(u64, u64)> = index.entries.iter().map(|e| (e.time_ns, e.byte)).collect();
    assert_eq!(got, keys, "the keyframes ffprobe lists, at their decode times");
}

#[test]
fn without_keyframes_the_index_grows_from_what_is_read() {
    let data = fixture("h264_mp3");
    let out = run(&data, &data, 4096);
    let index = out.index.expect("published at the end");
    assert!(!index.complete && index.grouped);
    assert_eq!(index.stream, Some(0));
    let keys: Vec<(u64, u64)> = of(&packets("h264_mp3"), 0)
        .iter()
        .filter(|p| p.key)
        .map(|p| (p.pts as u64 * MS, p.pos))
        .collect();
    let got: Vec<(u64, u64)> = index.entries.iter().map(|e| (e.time_ns, e.byte)).collect();
    assert_eq!(got, keys);

    let out = run(&fixture("aac"), &fixture("aac"), 4096);
    let index = out.index.expect("published");
    assert!(!index.grouped && !index.entries.is_empty());
    assert!(index.entries.windows(2).all(|w| w[1].time_ns - w[0].time_ns >= 1_000_000_000));
}

type Unit = (Option<u64>, usize, u32);

fn units(buffers: &[Buffer]) -> Vec<Unit> {
    buffers.iter().map(|b| (b.pts.nanos(), b.memory.len(), b.flags.0)).collect()
}

fn harness(data: &[u8]) -> Harness {
    let shim = Shim { inner: FlvDemux::new(data.to_vec()).with_len(data.len() as u64) };
    let mut h = Harness::with_pool(shim, 1 << 16, 256);
    h.start().expect("start");
    h
}

fn feed(h: &mut Harness, data: &[u8], out: &mut Vec<Unit>) {
    for piece in data.chunks(4096) {
        let buf = h.alloc(piece);
        h.push(buf).expect("push");
        out.extend(units(&h.drain_outputs()));
    }
}

fn reference(data: &[u8]) -> Vec<Unit> {
    let mut h = harness(data);
    let mut out = Vec::new();
    feed(&mut h, data, &mut out);
    out.extend(units(&h.eos().expect("eos")));
    out
}

/// Seeks to `to_byte` after reading a third of the file, what comes out behind it.
fn seek(data: &[u8], to_byte: u64) -> Vec<Unit> {
    let mut h = harness(data);
    let mut before = Vec::new();
    feed(&mut h, &data[..data.len() / 3], &mut before);
    assert!(!before.is_empty());
    h.seek(to_byte, Timestamp(0)).expect("seek");
    let mut after = Vec::new();
    feed(&mut h, &data[to_byte as usize..], &mut after);
    after.extend(units(&h.eos().expect("eos")));
    after
}

/// The video frames of `got`, known by stamp and length, and whether the first is a keyframe.
fn video_of(got: &[Unit], rows: &[Packet]) -> Vec<Unit> {
    let video = of(rows, 0);
    let is_video = |u: &Unit| video.iter().any(|p| (Some(p.pts as u64 * MS), p.size) == (u.0, u.1));
    got.iter().copied().filter(|u| u.2 & BufferFlags::HEADER.0 == 0 && is_video(u)).collect()
}

#[test]
fn a_seek_to_an_index_byte_starts_at_that_keyframe() {
    let data = fixture("h264_aac");
    let rows = packets("h264_aac");
    let whole = reference(&data);
    let key = of(&rows, 0).into_iter().filter(|p| p.key).nth(2).expect("a third keyframe");
    let after = seek(&data, key.pos);
    for u in after.iter().filter(|u| u.2 & BufferFlags::HEADER.0 == 0) {
        assert!(whole.contains(u), "{u:?} is not as read from the start");
    }
    let video = video_of(&after, &rows);
    assert_eq!(video[0].0, Some(key.pts as u64 * MS));
    assert!(video[0].2 & BufferFlags::KEYFRAME.0 != 0);
    let left = of(&rows, 0).iter().filter(|p| p.pos >= key.pos).count();
    assert_eq!(video.len(), left, "every frame from the keyframe on");
    assert!(after[0].2 & BufferFlags::HEADER.0 != 0, "the configuration is sent again");
}

#[test]
fn a_seek_into_the_middle_of_a_tag_scans_for_the_next_keyframe() {
    for name in ["h264_aac", "h264_mp3", "hevc_aac"] {
        let data = fixture(name);
        let rows = packets(name);
        let whole = reference(&data);
        let to = data.len() as u64 / 2 + 37;
        let after = seek(&data, to);
        assert!(!after.is_empty(), "{name}");
        for u in after.iter().filter(|u| u.2 & BufferFlags::HEADER.0 == 0) {
            assert!(whole.contains(u), "{name} {u:?} is not as read from the start");
        }
        let video = video_of(&after, &rows);
        let key = of(&rows, 0).into_iter().find(|p| p.key && p.pos >= to).expect("a keyframe");
        assert_eq!(video[0].0, Some(key.pts as u64 * MS), "{name}");
        assert!(video[0].2 & BufferFlags::KEYFRAME.0 != 0, "{name}");
        let left = of(&rows, 0).iter().filter(|p| p.pos >= key.pos).count();
        assert_eq!(video.len(), left, "{name}");
        let sound = of(&rows, 1).iter().filter(|p| p.pos > to).count();
        let frames = after.iter().filter(|u| u.2 & BufferFlags::HEADER.0 == 0).count();
        assert_eq!(frames - video.len(), sound, "{name} the sound from the next tag");
    }
}
