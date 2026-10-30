//! `psdemux` in a pipeline: families, stamps against ffprobe, LPCM against ffmpeg, the wrap,
//! concatenated VOBs, the program stream map, junk, subpictures and decoding.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use std::process::Command;

use common::pipe::{self, Link, Out, run, run_with};
use common::{fixture, packets, streams};
use pf_libav::{LibavAudioDec, LibavMpeg2Dec};
use pf_ps::PsDemux;
use pf_ps::pack::{PACK, PRIVATE_1, PSM};
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::StreamKind;

fn families(out: &Out) -> Vec<(StreamKind, &str)> {
    out.iter().map(|(k, f, _)| (*k, *f)).collect()
}

fn ns(ticks: i64, origin: i64) -> u64 {
    (i128::from(ticks - origin) * 100_000 / 9).max(0) as u64
}

/// The earliest first decode time over the streams, as the element takes it.
fn origin(name: &str) -> i64 {
    let rows = packets(name);
    let first = |s: usize| rows.iter().find(|p| p.stream == s).and_then(|p| p.dts.or(p.pts));
    streams(name).iter().filter_map(|s| first(s.index)).min().expect("a stamp")
}

#[test]
fn each_stream_leaves_under_the_family_its_decoder_takes() {
    use StreamKind::{Audio, Video};
    let cases: [(&str, &[(StreamKind, &str)]); 8] = [
        ("mpeg2_ac3.vob", &[(Video, "mpeg2"), (Audio, "ac3")]),
        ("mpeg2_mp2.vob", &[(Video, "mpeg2"), (Audio, "mp3")]),
        ("mpeg1_mp2.mpg", &[(Video, "mpeg1"), (Audio, "mp3")]),
        ("lpcm16.vob", &[(Audio, "audio/raw")]),
        ("lpcm24.vob", &[(Audio, "audio/raw")]),
        ("dts.vob", &[(Audio, "dts")]),
        ("h264.vob", &[(Video, "h264/annexb")]),
        ("wrap.vob", &[(Video, "mpeg2"), (Audio, "ac3")]),
    ];
    for (name, want) in cases {
        let out = run(&fixture(name), 4096);
        assert_eq!(families(&out), want, "{name}");
        for (_, family, rec) in &out {
            assert!(!rec.units.is_empty(), "{name} {family}");
            assert_eq!(rec.formats.first().map(|f| f.0.as_str()), Some(*family), "{name}");
        }
    }
}

#[test]
fn stamps_start_at_zero_and_follow_ffprobe() {
    for name in ["mpeg2_ac3.vob", "mpeg2_mp2.vob", "mpeg1_mp2.mpg"] {
        let base = origin(name);
        let rows = packets(name);
        let probed = streams(name);
        let out = run(&fixture(name), 2048);
        let video = probed.iter().find(|s| s.codec.starts_with("mpeg")).expect("video");
        let rows: Vec<_> = rows.iter().filter(|p| p.stream == video.index).collect();
        let rec = &out[0].2;
        assert_eq!(rec.units.len(), rows.len(), "{name}");
        for (n, (u, row)) in rec.units.iter().zip(&rows).enumerate() {
            assert_eq!(u.1, row.size, "{name} unit {n}");
            if let (Some(at), Some(pts)) = (u.0, row.pts) {
                assert_eq!(at, ns(pts, base), "{name} unit {n}");
            }
            assert_eq!(u.2 & BufferFlags::KEYFRAME.0 != 0, row.key, "{name} unit {n}");
        }
        let first = out.iter().filter_map(|(_, _, r)| r.units.iter().find_map(|u| u.0)).min();
        assert!(first.is_some_and(|f| f < 100_000_000), "{name}: starts near zero, {first:?}");
    }
}

fn ffmpeg_pcm(name: &str, format: &str) -> Option<Vec<u8>> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-i", &path, "-f", format, "-"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(out.stdout)
}

#[test]
fn dvd_lpcm_leaves_as_ffmpeg_decodes_it() {
    let out = run(&fixture("lpcm16.vob"), 1000);
    let rec = &out[0].2;
    assert_eq!(rec.formats[0], ("audio/raw".into(), Some(48_000), Some(2), Some("s16".into())));
    if let Some(want) = ffmpeg_pcm("lpcm16.vob", "s16le") {
        assert!(
            rec.bytes == want,
            "16-bit differs from ffmpeg, {} {}",
            rec.bytes.len(),
            want.len()
        );
    }
    let out = run(&fixture("lpcm24.vob"), 777);
    let rec = &out[0].2;
    assert_eq!(rec.formats[0], ("audio/raw".into(), Some(48_000), Some(2), Some("s24".into())));
    if let Some(want) = ffmpeg_pcm("lpcm24.vob", "s32le") {
        let ours: Vec<u8> =
            rec.bytes.as_chunks::<3>().0.iter().flat_map(|s| [0, s[0], s[1], s[2]]).collect();
        assert!(ours == want, "24-bit differs from ffmpeg, {} {}", ours.len(), want.len());
    }
}

fn stamps(rec: &pipe::Recorded) -> Vec<u64> {
    rec.units.iter().filter_map(|u| u.0).collect()
}

#[test]
fn the_33_bit_wrap_reads_on() {
    let out = run(&fixture("wrap.vob"), 4096);
    let rows = packets("wrap.vob");
    assert!(rows.iter().any(|p| p.pts.is_some_and(|v| v < 0)), "starts ahead of the wrap");
    assert!(rows.iter().any(|p| p.pts.is_some_and(|v| v > 0)));
    for (_, family, rec) in &out {
        let s = stamps(rec);
        assert!(s.iter().all(|t| *t < 2_000_000_000), "{family}: {s:?}");
        assert!(s.iter().any(|t| *t > 1_000_000_000), "{family}: past the wrap");
    }
    let audio = stamps(&out[1].2);
    assert!(audio.windows(2).all(|w| w[1] > w[0]), "audio ascends across the wrap");
}

#[test]
fn concatenated_vobs_play_on_across_the_join() {
    let one = fixture("mpeg2_ac3.vob");
    let single = run(&one, 4096);
    let joined = [one.clone(), one].concat();
    let out = run(&joined, 4096);
    for ((_, family, a), (_, _, b)) in single.iter().zip(&out) {
        assert_eq!(b.units.len(), 2 * a.units.len(), "{family}: every unit of both");
        let n = a.units.len();
        let end = stamps(a).into_iter().max().expect("stamped");
        let next = b.units[n..].iter().find_map(|u| u.0).expect("stamped");
        assert!(next > end && next < end + 500_000_000, "{family}: {end} then {next}");
        assert!(b.units[n].2 & BufferFlags::DISCONT.0 != 0, "{family}: marked");
        let second: Vec<u64> = b.units[n..].iter().filter_map(|u| u.0).collect();
        assert!(second.iter().all(|t| *t >= end), "{family}: nothing goes back");
    }
}

fn psm(rows: &[(u8, u8)]) -> Vec<u8> {
    let mut b = vec![0, 0, 1, PSM, 0, 0, 0x80, 0xFF, 0, 0, 0, (rows.len() * 4) as u8];
    for &(t, id) in rows {
        b.extend_from_slice(&[t, id, 0, 0]);
    }
    let len = (b.len() - 6 + 4) as u16;
    b[4..6].copy_from_slice(&len.to_be_bytes());
    let crc = pf_mpegts::crc32(&b);
    b.extend_from_slice(&crc.to_be_bytes());
    b
}

/// `map` right behind the first pack header.
fn with_map(data: &[u8], map: &[u8]) -> Vec<u8> {
    let at = match pf_ps::pack::item_len(data) {
        pf_ps::pack::Need::Len(n) => n,
        _ => panic!("a pack first"),
    };
    assert_eq!(data[3], PACK);
    [&data[..at], map, &data[at..]].concat()
}

#[test]
fn a_program_stream_map_names_and_refuses_streams() {
    let data = with_map(&fixture("h264.vob"), &psm(&[(0x1B, 0xE2)]));
    assert_eq!(families(&run(&data, 4096)), [(StreamKind::Video, "h264/annexb")]);
    // MPEG-1 video declared, read by the same decoder.
    let data = with_map(&fixture("mpeg2_mp2.vob"), &psm(&[(0x01, 0xE0), (0x0F, 0xC0)]));
    assert_eq!(families(&run(&data, 4096)), [(StreamKind::Video, "mpeg1")], "AAC is refused");
    let data = with_map(&fixture("mpeg2_mp2.vob"), &psm(&[(0x10, 0xE0), (0x03, 0xC0)]));
    let out = run(&data, 4096);
    assert_eq!(families(&out), [(StreamKind::Video, "mpeg4/asp"), (StreamKind::Audio, "mp3")]);
}

#[test]
fn junk_in_the_middle_costs_only_what_it_hits() {
    let data = fixture("mpeg2_mp2.vob");
    let clean = run(&data, 4096);
    let mid = data.len() / 2;
    let mut x = 0x9E37_79B9u32;
    let junk: Vec<u8> = (0..3000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let dirty = [&data[..mid], &junk, &data[mid + 100..]].concat();
    let out = run(&dirty, 4096);
    for ((_, family, a), (_, _, b)) in clean.iter().zip(&out) {
        let lost = a.units.len() - b.units.len().min(a.units.len());
        assert!(lost <= a.units.len() / 3, "{family}: {lost} of {}", a.units.len());
        assert!(b.units.len() > a.units.len() / 2, "{family}");
    }
}

fn pes(id: u8, pts: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let mut fields = Vec::new();
    if let Some(v) = pts {
        fields = vec![
            0x21 | (((v >> 30) as u8 & 7) << 1),
            (v >> 22) as u8,
            (((v >> 15) as u8) << 1) | 1,
            (v >> 7) as u8,
            ((v as u8) << 1) | 1,
        ];
    }
    let len = (3 + fields.len() + payload.len()) as u16;
    let mut b = vec![0, 0, 1, id, (len >> 8) as u8, len as u8, 0x81];
    b.extend_from_slice(&[if pts.is_some() { 0x80 } else { 0 }, fields.len() as u8]);
    b.extend_from_slice(&fields);
    b.extend_from_slice(payload);
    b
}

fn pack(scr: u64) -> Vec<u8> {
    let v = 1u64 << 46
        | (scr >> 30 & 7) << 43
        | 1 << 42
        | (scr >> 15 & 0x7FFF) << 27
        | 1 << 26
        | (scr & 0x7FFF) << 11
        | 1 << 10
        | 1;
    let mut b = vec![0, 0, 1, PACK];
    b.extend_from_slice(&v.to_be_bytes()[2..]);
    b.extend_from_slice(&[0x01, 0x89, 0xC3, 0xF8]);
    b
}

#[test]
fn a_subpicture_unit_leaves_whole_and_decodes() {
    let spu = pf_text::vobsubdec::fixtures::minimal();
    let mut data = Vec::new();
    for at in [90_000u64, 180_000] {
        let (a, b) = spu.split_at(spu.len() / 2);
        data.extend(pack(at - 9000));
        data.extend(pes(PRIVATE_1, Some(at), &[&[0x20][..], a].concat()));
        data.extend(pack(at - 8000));
        data.extend(pes(PRIVATE_1, None, &[&[0x20][..], b].concat()));
    }
    let out = run(&data, 100);
    assert_eq!(families(&out), [(StreamKind::Text, "subtitle/vobsub")]);
    let rec = &out[0].2;
    assert_eq!(
        rec.units.iter().map(|u| (u.0, u.1)).collect::<Vec<_>>(),
        [(Some(0), spu.len()), (Some(1_000_000_000), spu.len())]
    );
    assert_eq!(rec.bytes, [spu.clone(), spu].concat());
    let out = run_with(&data, PsDemux::new(data.clone()), 100, &mut |_, p| {
        let dec = p.add(pf_text::VobSubDec::new());
        Link::Through(dec, profluens_core::id::SinkPad(0), profluens_core::id::SrcPad(0))
    });
    let pictures = &out[0].2;
    assert!(!pictures.units.is_empty(), "the decoder drew the unit");
}

fn ffmpeg_raw(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error"])
        .args(&args[..args.len() - 1])
        .args(["-i", &path])
        .args(args[args.len() - 1].split(' '))
        .arg("-")
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(out.stdout)
}

#[test]
fn mpeg1_and_mpeg2_video_and_layer_two_decode_as_ffmpeg_decodes_them() {
    for name in ["mpeg2_mp2.vob", "mpeg1_mp2.mpg"] {
        let data = fixture(name);
        // The mp3 element is passive, so each decoder gets a run of its own.
        let out = run_with(&data, PsDemux::new(data.clone()), 4096, &mut |n, p| match n {
            0 => Link::Through(p.add(LibavMpeg2Dec::new()), LibavMpeg2Dec::IN, LibavMpeg2Dec::OUT),
            _ => Link::Not,
        });
        let heard = run_with(&data, PsDemux::new(data.clone()), 4096, &mut |n, p| match n {
            1 => Link::Through(p.add(LibavAudioDec::mp3()), LibavAudioDec::IN, LibavAudioDec::OUT),
            _ => Link::Not,
        });
        let video =
            ffmpeg_raw(name, &["-map 0:v -fps_mode passthrough -f rawvideo -pix_fmt yuv420p"]);
        let audio = ffmpeg_raw(name, &["-c:a", "mp2float", "-map 0:a -f s16le"]);
        let (Some(video), Some(audio)) = (video, audio) else { return };
        let pictures = &out[0].2;
        assert!(pictures.units.len() >= 18, "{name}: {} pictures", pictures.units.len());
        assert!(pictures.bytes == video, "{name}: pictures differ from ffmpeg");
        let sound = &heard[1].2;
        assert_eq!(sound.formats[0].3.as_deref(), Some("s16"), "{name}");
        assert!(
            sound.bytes == audio,
            "{name}: sound differs, {} {}",
            sound.bytes.len(),
            audio.len()
        );
    }
}
