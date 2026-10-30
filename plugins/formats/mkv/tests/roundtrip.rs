//! Structural round-trip of the writer, read back with the test EBML reader in `common/`.
//! An `ffmpeg` oracle runs when the binary is present.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

mod common;

use common::{SimpleBlock, parse_simple_block, walk};
use pf_libflac::{FlacEncoder, SampleFormat};
use pf_mkv::ebml::id;
use pf_mkv::{MatroskaWriter, TrackConfig};

const MASTERS: &[&[u8]] =
    &[id::EBML, id::SEGMENT, id::INFO, id::TRACKS, id::TRACK_ENTRY, id::AUDIO, id::CLUSTER];

fn make_flac(
    sample_rate: u32,
    channels: u32,
    block: usize,
    n_frames: usize,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    let (mut enc, mut header) =
        FlacEncoder::new_streaming(sample_rate, channels, SampleFormat::S16, block as u32)
            .expect("encoder");
    let mut frames = Vec::new();
    let mut phase = 0i32;
    for _ in 0..n_frames {
        let mut pcm = Vec::with_capacity(block * channels as usize * 2);
        for _ in 0..block {
            for c in 0..channels {
                let v = ((phase.wrapping_mul(37) + c as i32 * 1000) & 0x3FFF) as i16 - 0x2000;
                pcm.extend_from_slice(&v.to_le_bytes());
                phase = phase.wrapping_add(1);
            }
        }
        enc.encode_frames(&pcm, &mut frames).expect("encode");
    }
    let body = enc.finish_frames(&mut frames);
    let off = pf_libflac::streaminfo_offset();
    header[off..off + body.len()].copy_from_slice(&body);
    (header, frames)
}

fn read_first_track(stream: &[u8]) -> (String, Vec<u8>) {
    let els = walk(stream, MASTERS);
    let mut codec_id = None;
    let mut codec_private = None;
    for (_depth, el) in &els {
        if el.id == id::CODEC_ID {
            let (s, e) = el.data.unwrap();
            codec_id = Some(String::from_utf8(stream[s..e].to_vec()).unwrap());
        } else if el.id == id::CODEC_PRIVATE {
            let (s, e) = el.data.unwrap();
            codec_private = Some(stream[s..e].to_vec());
        }
        if codec_id.is_some() && codec_private.is_some() {
            break;
        }
    }
    (codec_id.expect("CodecID present"), codec_private.expect("CodecPrivate present"))
}

fn read_blocks(stream: &[u8]) -> Vec<(i64, SimpleBlock)> {
    let els = walk(stream, MASTERS);
    let mut out = Vec::new();
    let mut cluster_base = 0i64;
    for (_depth, el) in &els {
        if el.id == id::TIMESTAMP {
            let (s, e) = el.data.unwrap();
            let mut v = 0i64;
            for &b in &stream[s..e] {
                v = (v << 8) | b as i64;
            }
            cluster_base = v;
        } else if el.id == id::SIMPLE_BLOCK {
            let (s, e) = el.data.unwrap();
            let blk = parse_simple_block(&stream[s..e]);
            out.push((cluster_base + blk.rel_ts as i64, blk));
        }
    }
    out
}

#[test]
fn single_flac_track_structural_roundtrip() {
    let (codec_private, frames) = make_flac(48_000, 2, 4096, 6);
    let track = TrackConfig::flac(1, codec_private.clone(), 48_000.0, 2, 16);

    let mut w = MatroskaWriter::new(vec![track]);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let dur_ns = 4096u64 * 1_000_000_000 / 48_000;
    for (i, f) in frames.iter().enumerate() {
        w.write_frame(&mut out, 1, i as u64 * dur_ns, f, true).unwrap();
    }
    w.finalize(&mut out);

    assert_eq!(&out[..4], id::EBML, "stream starts with the EBML Header ID");
    let els = walk(&out, MASTERS);
    assert!(els.iter().any(|(_, e)| e.id == id::EBML), "EBML Header element present");
    assert!(els.iter().any(|(_, e)| e.id == id::SEGMENT), "Segment present");
    assert!(els.iter().any(|(_, e)| e.id == id::TRACKS), "Tracks present");

    let track_entries = els.iter().filter(|(_, e)| e.id == id::TRACK_ENTRY).count();
    assert_eq!(track_entries, 1, "exactly one TrackEntry");
    let (cid, cpriv) = read_first_track(&out);
    assert_eq!(cid, "A_FLAC", "CodecID is A_FLAC");
    assert_eq!(cpriv, codec_private, "CodecPrivate is the exact fLaC+STREAMINFO blob");
    assert_eq!(&cpriv[..4], b"fLaC", "CodecPrivate begins with the fLaC marker");

    let blocks = read_blocks(&out);
    assert_eq!(blocks.len(), frames.len(), "one SimpleBlock per input frame");
    let mut last_ts = i64::MIN;
    for (i, ((abs_ts, blk), frame)) in blocks.iter().zip(frames.iter()).enumerate() {
        assert_eq!(blk.track, 1, "block {i} on track 1");
        assert!(blk.keyframe, "FLAC block {i} flagged keyframe");
        assert_eq!(&blk.frame, frame, "block {i} carries the exact FLAC frame bytes");
        assert!(*abs_ts >= last_ts, "block {i} timestamp monotonically non-decreasing");
        last_ts = *abs_ts;
    }
    assert!(els.iter().any(|(_, e)| e.id == id::CLUSTER), "at least one Cluster");
}

#[test]
fn two_track_library_roundtrip() {
    let (cp1, frames1) = make_flac(48_000, 2, 4096, 4);
    let (cp2, frames2) = make_flac(44_100, 1, 4096, 4);
    let tracks = vec![
        TrackConfig::flac(1, cp1.clone(), 48_000.0, 2, 16),
        TrackConfig::flac(2, cp2.clone(), 44_100.0, 1, 16),
    ];

    let mut w = MatroskaWriter::new(tracks);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let dur = 85_000_000u64; // ~one 48k block, ns
    for i in 0..4 {
        w.write_frame(&mut out, 1, i as u64 * dur, &frames1[i], true).unwrap();
        w.write_frame(&mut out, 2, i as u64 * dur, &frames2[i], true).unwrap();
    }
    w.finalize(&mut out);

    let els = walk(&out, MASTERS);
    let entries = els.iter().filter(|(_, e)| e.id == id::TRACK_ENTRY).count();
    assert_eq!(entries, 2, "two TrackEntries for two tracks");

    let mut cprivs = Vec::new();
    for (_d, el) in &els {
        if el.id == id::CODEC_PRIVATE {
            let (s, e) = el.data.unwrap();
            cprivs.push(out[s..e].to_vec());
        }
    }
    assert_eq!(cprivs.len(), 2, "two CodecPrivate blobs");
    assert!(cprivs.contains(&cp1), "track 1 CodecPrivate present");
    assert!(cprivs.contains(&cp2), "track 2 CodecPrivate present");

    let blocks = read_blocks(&out);
    let t1: Vec<&Vec<u8>> =
        blocks.iter().filter(|(_, b)| b.track == 1).map(|(_, b)| &b.frame).collect();
    let t2: Vec<&Vec<u8>> =
        blocks.iter().filter(|(_, b)| b.track == 2).map(|(_, b)| &b.frame).collect();
    assert_eq!(t1.len(), frames1.len(), "all track-1 frames muxed");
    assert_eq!(t2.len(), frames2.len(), "all track-2 frames muxed");
    for (got, want) in t1.iter().zip(frames1.iter()) {
        assert_eq!(*got, want, "track 1 frame bytes preserved");
    }
    for (got, want) in t2.iter().zip(frames2.iter()) {
        assert_eq!(*got, want, "track 2 frame bytes preserved");
    }
}

#[test]
fn ffmpeg_oracle_if_present() {
    use std::process::Command;

    let ffmpeg = match which_ffmpeg() {
        Some(p) => p,
        None => {
            eprintln!("ffmpeg not found, skipping the optional oracle cross-validation");
            return;
        }
    };

    let (codec_private, frames) = make_flac(48_000, 2, 4096, 8);
    let track = TrackConfig::flac(1, codec_private, 48_000.0, 2, 16);
    let mut w = MatroskaWriter::new(vec![track]);
    let mut out = Vec::new();
    w.write_header(&mut out).unwrap();
    let dur_ns = 4096u64 * 1_000_000_000 / 48_000;
    for (i, f) in frames.iter().enumerate() {
        w.write_frame(&mut out, 1, i as u64 * dur_ns, f, true).unwrap();
    }
    w.finalize(&mut out);

    let path = std::env::temp_dir().join(format!("pf-mkv-oracle-{}.mkv", std::process::id()));
    std::fs::write(&path, &out).expect("write temp mkv");

    let output = Command::new(&ffmpeg)
        .arg("-hide_banner")
        .arg("-i")
        .arg(&path)
        .output()
        .expect("run ffmpeg");
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if std::env::var_os("PF_MKV_KEEP").is_none() {
        let _ = std::fs::remove_file(&path);
    } else {
        eprintln!("PF_MKV_KEEP set, left oracle file at {}", path.display());
    }

    assert!(
        stderr.contains("matroska"),
        "ffmpeg identifies the container as Matroska; stderr was:\n{stderr}"
    );
    assert!(stderr.contains("flac"), "ffmpeg identifies a FLAC stream; stderr was:\n{stderr}");
}

fn which_ffmpeg() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("ffmpeg");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}
