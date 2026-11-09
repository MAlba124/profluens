#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_bare::fixture::{ac3_frame, dts_frame, eac3_frame, wav_file};

use pf_open::{Caps, Index, Kind, Meta, OpenError, TrackKind, Want};

/// A fixture, the family, width and rate of each track, and the milliseconds it lasts.
type Expect = (&'static str, &'static [(&'static str, u32, u32)], u64);

const TS: &str = "plugins/formats/ts/tests/fixtures";
const TSD: &str = "plugins/formats/tsdemux/tests/fixtures";
const PS: &str = "plugins/formats/ps/tests/fixtures";

#[test]
fn a_hand_built_transport_stream_opens_its_program() {
    let b = ts_file(8, 900_000);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(o.kind, Kind::Ts);
    let v = &o.tracks[0];
    assert_eq!((v.id, v.stream, v.kind, v.family), (0x100, 0x100, TrackKind::Video, "mpeg2"));
    assert_eq!((v.width, v.height, v.frame_ns, v.par), (720, 576, 40_000_000, (64, 45)));
    assert_eq!(o.codec_head(v), &SEQ[..22]);
    let a = &o.tracks[1];
    assert_eq!((a.id, a.family, a.rate, a.channels), (0x101, "ac3", 48_000, 3));
    assert!(v.default && a.default);
    // the last video frame starts 199 frames in, the last audio frame later
    let last_audio = (8 * 25 * 3600 - 3600) / 2880 * 2880;
    assert_eq!(o.duration_ns, last_audio.max(199 * 3600) * 100_000 / 9);
    assert_eq!(log[0], profluens_core::fetch::Need { at: 0, len: 256 << 10 });
    assert_eq!(log.len(), 2, "the window and the tail, {log:?}");
    assert_eq!(log[1].at + log[1].len as u64, b.len() as u64, "the tail to the end");
    assert_eq!(log[1].at, (b.len() as u64 - (256 << 10)).max(256 << 10));
    assert!(matches!(o.index, Index::None) && o.meta == Meta::None);
    assert_eq!(o.head.len(), 256 << 10);
    // a window short of the map grows to it, never past what it needs
    let (small, log) = open_with(&b, Want::ALL, Caps { window: 1000, ..Caps::default() });
    let small = small.unwrap();
    assert_eq!((small.tracks, small.duration_ns), (o.tracks.clone(), o.duration_ns));
    assert!(log.iter().all(|n| n.len <= 256 << 10), "{log:?}");
    assert!(small.head.len() < 256 << 10);
}

/// The stamps wrap the 33 bits half way, the duration reads across it.
#[test]
fn a_transport_stream_across_the_stamp_wrap_lasts_its_length() {
    let b = ts_file(6, (1 << 33) - 3 * 90_000);
    let o = open(&b);
    let last_audio = (6 * 25 * 3600 - 3600) / 2880 * 2880;
    assert_eq!(o.duration_ns, last_audio.max(149 * 3600) * 100_000 / 9);
}

#[test]
fn transport_stream_fixtures_open() {
    let want: [Expect; 8] = [
        ("h264_aac.ts", &[("h264/annexb", 128, 0), ("aac", 0, 48_000)], 1_954),
        ("hevc_ac3.ts", &[("h265/annexb", 128, 0), ("ac3", 0, 48_000)], 1_938),
        ("hevc_eac3_dvb.ts", &[("h265/annexb", 128, 0), ("eac3", 0, 48_000)], 1_938),
        ("mpeg2.ts", &[("mpeg2", 128, 0)], 2_000),
        ("audio_mp2.ts", &[("mp3", 0, 48_000)], 1_992),
        ("wrap.ts", &[("h264/annexb", 128, 0), ("aac", 0, 48_000)], 2_117),
        ("../../../tsdemux/tests/fixtures/mpeg4.ts", &[("mpeg4/asp", 128, 0)], 2_000),
        (
            "../../../tsdemux/tests/fixtures/dvbsub.ts",
            &[("mpeg2", 64, 0), ("subtitle/dvb", 0, 0)],
            6_040,
        ),
    ];
    for (name, tracks, ms) in want {
        let b = fixture(&format!("{TS}/{name}"));
        let o = open(&b);
        let got: Vec<_> = o.tracks.iter().map(|t| (t.family, t.width, t.rate)).collect();
        assert_eq!(got, tracks, "{name}");
        assert_eq!(o.duration_ns / MS, ms, "{name}");
        // a small window reads the same
        let (small, _) = open_with(&b, Want::ALL, Caps { window: 4096, ..Caps::default() });
        let small = small.unwrap();
        assert_eq!((&small.tracks, small.duration_ns), (&o.tracks, o.duration_ns), "{name}");
    }
    let o = open(&fixture(&format!("{TS}/h264_aac.ts")));
    assert_eq!(o.language(&o.tracks[1]), "eng");
    assert_eq!(o.codec_head(&o.tracks[1]), [0x11, 0x90], "the AudioSpecificConfig");
    assert_eq!(o.tracks[1].layout, 3, "stereo by its config");
    assert!(o.codec_head(&o.tracks[0]).starts_with(&[0, 0, 0, 1, 0x67]));
    assert_eq!(o.tracks[0].depth, 8, "from the SPS");
    let o = open(&fixture(&format!("{TSD}/mpeg1.ts")));
    assert_eq!(o.tracks[0].family, "mpeg1", "told by its sequence header");
    // AAC in LATM has no demuxer family
    let (latm, _) = open_with(&fixture(&format!("{TS}/aac_latm.ts")), Want::ALL, Caps::default());
    assert_eq!(latm.err(), Some(OpenError::NoStreams));
}

#[test]
fn a_hand_built_program_stream_opens_its_streams() {
    let b = ps_file(4, true, true);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(o.kind, Kind::Ps);
    let v = &o.tracks[0];
    assert_eq!((v.id, v.stream, v.family, v.width, v.height), (0xE000, 0xE000, "mpeg2", 720, 576));
    let a = &o.tracks[1];
    assert_eq!((a.id, a.family, a.rate, a.channels), (0xBD80, "ac3", 48_000, 6));
    let last_audio = (4 * 25 * 3600 - 3600) / 2880 * 2880;
    assert_eq!(o.duration_ns, last_audio.max(99 * 3600) * 100_000 / 9);
    assert_eq!(log, [profluens_core::fetch::Need { at: 0, len: b.len() }]);
    assert_eq!(&o.head[..], &b[..]);
    // a window short of the file grows to the head, the tail read for the last stamps
    let long = ps_file(40, true, true);
    let (o, log) = open_with(&long, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(o.head.len(), 1 << 20);
    assert_eq!(log.len(), 3, "{log:?}");
    let last_audio = (40 * 25 * 3600 - 3600) / 2880 * 2880;
    assert_eq!(o.duration_ns, last_audio.max(999 * 3600) * 100_000 / 9);
}

/// Packs without a clock and units without stamps still open, the duration unknown.
#[test]
fn a_program_stream_without_stamps_opens_untimed() {
    let o = open(&ps_file(2, false, false));
    assert_eq!((o.tracks.len(), o.duration_ns), (2, 0));
    let o = open(&ps_file(2, false, true));
    assert!(o.duration_ns > 1_800 * MS, "the stamps time it without a clock");
}

#[test]
fn program_stream_fixtures_open() {
    let want: [Expect; 8] = [
        ("dts.vob", &[("dts", 0, 48_000)], 245),
        ("h264.vob", &[("h264/annexb", 128, 0)], 1_133),
        ("lpcm16.vob", &[("audio/raw", 0, 48_000)], 491),
        ("lpcm24.vob", &[("audio/raw", 0, 48_000)], 245),
        ("mpeg2_ac3.vob", &[("mpeg2", 128, 0), ("ac3", 0, 48_000)], 1_200),
        ("mpeg2_mp2.vob", &[("mpeg2", 128, 0), ("mp3", 0, 48_000)], 1_200),
        ("wrap.vob", &[("mpeg2", 128, 0), ("ac3", 0, 48_000)], 1_200),
        ("mpeg1_mp2.mpg", &[("mpeg1", 128, 0), ("mp3", 0, 48_000)], 1_200),
    ];
    for (name, tracks, ms) in want {
        let b = fixture(&format!("{PS}/{name}"));
        let o = open(&b);
        let got: Vec<_> = o.tracks.iter().map(|t| (t.family, t.width, t.rate)).collect();
        assert_eq!(got, tracks, "{name}");
        assert_eq!(o.duration_ns / MS, ms, "{name}");
        let (small, _) = open_with(&b, Want::ALL, Caps { window: 4096, ..Caps::default() });
        assert_eq!(small.unwrap().tracks, o.tracks, "{name}");
    }
    let o = open(&fixture(&format!("{PS}/lpcm24.vob")));
    assert_eq!((o.tracks[0].channels, o.tracks[0].depth), (2, 24));
}

#[test]
fn elementary_audio_opens_through_baresrcs_head() {
    let ac3: Vec<u8> = (0..300).flat_map(|_| ac3_frame(0, 448, false, 7)).collect();
    let o = open(&ac3);
    assert_eq!(o.kind, Kind::Ac3);
    let t = &o.tracks[0];
    assert_eq!((t.id, t.family, t.rate, t.channels, t.bitrate), (1, "ac3", 48_000, 6, 448_000));
    assert_eq!(o.duration_ns, 9_600 * MS);
    assert!(matches!(o.index, Index::Bare(_)));
    let eac3: Vec<u8> = (0..100).flat_map(|_| eac3_frame(768, 0, 0)).collect();
    let o = open(&eac3);
    assert_eq!((o.kind, o.tracks[0].family, o.duration_ns), (Kind::Eac3, "eac3", 3_200 * MS));
    let dts: Vec<u8> = (0..100).flat_map(|_| dts_frame(2012)).collect();
    let o = open(&dts);
    let t = &o.tracks[0];
    assert_eq!((o.kind, t.family, t.channels, t.depth), (Kind::Dts, "dts", 6, 24));
    assert_eq!(o.duration_ns, 100 * 512 * 1_000_000_000 / 48_000);
}

#[test]
fn wav_opens_as_one_pcm_track() {
    let b = wav_file(44_100, 6, 24, false, 44_100 * 2, Some(0x60F), 100);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(o.kind, Kind::Wav);
    let t = &o.tracks[0];
    assert_eq!((t.family, t.rate, t.channels, t.depth), ("audio/raw", 44_100, 6, 24));
    assert_eq!((t.layout, t.bitrate, o.duration_ns), (0x60F, 44_100 * 6 * 24, 2_000 * MS));
    assert!(o.codec_head(t).is_empty() && o.head.is_empty());
    let Index::Bare(h) = &o.index else { panic!("{:?}", o.index) };
    assert_eq!(h.end - h.first, 44_100 * 2 * 18);
    let f = wav_file(48_000, 2, 32, true, 4800, None, 0);
    let o = open(&f);
    assert_eq!((o.tracks[0].depth, o.tracks[0].layout, o.duration_ns), (32, 0, 100 * MS));
}

/// Random bytes, cuts and bad lengths in streams that sniff as transport or program streams,
/// every open ends with an answer.
#[test]
fn hostile_streams_end_in_an_answer() {
    let mut seed = 0x853C_49E6_748F_EA9Bu64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // sync bytes over noise sniff as a transport stream and carry no map
    let mut noise: Vec<u8> = (0..188 * 200).map(|_| rnd() as u8).collect();
    for p in noise.chunks_mut(188) {
        p[0] = 0x47;
    }
    let (o, _) = open_with(&noise, Want::ALL, Caps::default());
    assert!(o.is_err(), "{:?}", o.map(|o| o.tracks));
    // a PAT whose program map is the PAT itself, and a map naming its own PID as a stream
    let (mut looped, mut cc) = (Vec::new(), [0u8; 2]);
    for _ in 0..50 {
        tables(0, &[(0x02, 0x100)], &mut looped, &mut cc);
        tables(0x1000, &[(0x02, 0x1000), (0x81, 0x1000)], &mut looped, &mut cc);
    }
    let _ = open_with(&looped, Want::ALL, Caps::default());
    let files = [ts_file(3, 0), ps_file(3, true, true)];
    for f in &files {
        for _ in 0..150 {
            let mut b = f.clone();
            for _ in 0..(rnd() % 60) {
                let at = (rnd() as usize) % b.len();
                b[at] = rnd() as u8;
            }
            // a PES or section length set huge
            let at = (rnd() as usize) % (b.len() - 8);
            if rnd() % 2 == 0 {
                b[at..at + 2].copy_from_slice(&[0xFF, 0xFF]);
            }
            b.truncate(b.len() - (rnd() as usize % (b.len() / 2)));
            let window = [256 << 10, 4096, 200][(rnd() % 3) as usize];
            let (o, _) = open_with(&b, Want::ALL, Caps { window, ..Caps::default() });
            if let Ok(o) = o {
                assert!(!o.tracks.is_empty());
                assert!(o.duration_ns < 3_600_000 * MS * 30, "{}", o.duration_ns);
            }
        }
    }
}

/// The corpus fixtures, when generated, open with the tracks ffmpeg wrote.
#[test]
fn generated_fixtures_open() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/out");
    let want: [(&str, Kind, &[&str]); 10] = [
        ("ts_h264_aac.ts", Kind::Ts, &["h264/annexb", "aac"]),
        ("ts_h265_ac3.ts", Kind::Ts, &["h265/annexb", "ac3"]),
        ("ps_mpeg1_mp2.mpg", Kind::Ps, &["mpeg1", "mp3"]),
        ("ps_mpeg2_ac3.vob", Kind::Ps, &["mpeg2", "ac3"]),
        ("audio.wav", Kind::Wav, &["audio/raw"]),
        ("audio_f32.wav", Kind::Wav, &["audio/raw"]),
        ("audio.ac3", Kind::Ac3, &["ac3"]),
        ("audio.eac3", Kind::Eac3, &["eac3"]),
        ("audio.dts", Kind::Dts, &["dts"]),
        ("audio_51.ac3", Kind::Ac3, &["ac3"]),
    ];
    for (name, kind, families) in want {
        let Ok(b) = std::fs::read(root.join(name)) else {
            eprintln!("{name} not generated, skipped");
            continue;
        };
        let o = open(&b);
        assert_eq!(o.kind, kind, "{name}");
        let got: Vec<_> = o.tracks.iter().map(|t| t.family).collect();
        assert_eq!(got, families, "{name}");
        // five seconds, a frame or a muxer's delay either way
        let ms = o.duration_ns / MS;
        assert!((4_850..=5_150).contains(&ms), "{name} lasts {ms} ms");
        assert!(o.tracks.iter().all(|t| t.kind != TrackKind::Audio || t.rate > 0), "{name}");
        let (small, _) = open_with(&b, Want::ALL, Caps { window: 4096, ..Caps::default() });
        assert_eq!(small.unwrap().tracks, o.tracks, "{name}");
    }
}
