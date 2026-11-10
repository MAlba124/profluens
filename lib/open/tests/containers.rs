#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_avi::Idx1At;
use pf_open::{Brand, Caps, Index, Kind, Meta, TrackKind, Want};
use pf_probe::head::{AviHeadAt, Mp4HeadAt};
use profluens_core::fetch::{Need, Window};

/// The MP4 opener today, 256 KiB then the head machine over it.
fn fhs_mp4(b: &[u8]) -> Vec<Need> {
    let mut log = Vec::new();
    let w = reads(&mut Window::new(0, (256 << 10).min(b.len())), b, &mut log).unwrap();
    reads(&mut Mp4HeadAt::new(&w, b.len() as u64), b, &mut log).unwrap();
    log
}

#[test]
fn a_faststart_mp4_head_is_the_window_cut_at_the_mdat() {
    let b = fixture("plugins/formats/mp4/tests/fixtures/tiny_av.mp4");
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(&o.head[..], &b[..1829], "through the free box, up to the mdat");
    assert_eq!(o.tracks.len(), 2);
    let v = &o.tracks[0];
    assert_eq!(
        (v.kind, v.family, v.width, v.height, v.nal_length),
        (TrackKind::Video, "h264/annexb", 128, 96, 4)
    );
    assert_eq!(v.depth, 8, "from the SPS");
    assert!(o.codec_head(v).starts_with(&[0, 0, 0, 1]));
    let a = &o.tracks[1];
    assert_eq!(
        (a.kind, a.family, a.channels, a.rate, a.depth),
        (TrackKind::Audio, "aac", 2, 48_000, 16)
    );
    assert!(a.bitrate > 0 && v.bitrate > 0);
    assert!(a.delay_ns > 0, "the edit's priming");
    assert_eq!((o.language(a), o.duration_ns), ("und", 1_000 * MS));
    assert!(matches!(o.meta, Meta::Moov(_)));
    let Index::Mp4(tables) = &o.index else { panic!("{:?}", o.index) };
    assert_eq!(tables.tracks().len(), 2);
    assert!(!tables.samples().is_empty() && tables.bytes() > tables.index_bytes());
    // tracks only, no sample folded
    let (tracks, _) = open_with(&b, Want::TRACKS, Caps::default());
    let tracks = tracks.unwrap();
    assert!(matches!(tracks.index, Index::None), "{:?}", tracks.index);
    assert_eq!((tracks.tracks, tracks.arena), (o.tracks.clone(), o.arena.clone()));
    // a window short of the moov reads the rest past it
    let (small, log) = open_with(&b, Want::ALL, Caps { window: 100, ..Caps::default() });
    let small = small.unwrap();
    assert_eq!((&small.head[..], &small.tracks), (&o.head[..], &o.tracks));
    assert!(log.iter().skip(1).all(|n| n.at >= 100), "{log:?}");
}

#[test]
fn a_moov_behind_the_mdat_is_one_more_read() {
    let small = fixture("plugins/formats/mp4/tests/fixtures/moov_last.mp4");
    let o = open(&small);
    assert_eq!(o.tracks.len(), 2);
    assert_eq!(o.duration_ns, 4_000 * MS);
    // the mdat grown past the window
    let mut b = small[..40].to_vec();
    let grow = 1 << 20;
    let mdat = 82510 + grow;
    b.extend_from_slice(&(mdat as u32).to_be_bytes());
    b.extend_from_slice(b"mdat");
    b.resize(40 + mdat, 0);
    b.extend_from_slice(&small[82550..]);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log.len(), fhs_mp4(&b).len());
    assert_eq!(&o.head[o.head.len() - 3049..], &small[82550..], "the moov appended");
    assert_eq!(o.tracks.len(), 2);
    let (o, _) = open_with(&b, Want::ALL, Caps { max_head: 1000, ..Caps::default() });
    assert_eq!(o.err(), Some(pf_open::OpenError::TooLarge));
}

/// The AVI opener today, the head a step at a time, the demuxer reading the idx1 at each run.
fn fhs_avi(b: &[u8]) -> Vec<Need> {
    let mut log = Vec::new();
    reads(&mut AviHeadAt::new(16 << 20), b, &mut log).unwrap();
    log
}

#[test]
fn an_avi_reads_its_idx1_once() {
    let b = fixture("plugins/formats/avi/tests/fixtures/tiny.avi");
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log.len(), 1, "the window holds the file and its idx1");
    assert_eq!(o.kind, Kind::Avi);
    assert!(o.head.ends_with(b"movi") && o.head.starts_with(b"RIFF"));
    assert_eq!(o.head.len(), 9984);
    let v = &o.tracks[0];
    assert_eq!(
        (v.id, v.stream, v.kind, v.family, v.width, v.height),
        (1, 0, TrackKind::Video, "mpeg4/asp", 160, 120)
    );
    assert!(v.frame_ns > 0);
    let a = &o.tracks[1];
    assert_eq!((a.id, a.stream, a.family, a.channels, a.rate), (2, 1, "ac3", 1, 44_100));
    assert_eq!(a.bitrate, 96_000);
    assert!((o.duration_ns as i64 - 3_030 * MS as i64).abs() < 50 * MS as i64, "{}", o.duration_ns);
    let Index::Avi(ix) = &o.index else { panic!("{:?}", o.index) };
    assert!(ix.whole);
    // the idx1 as the demuxer's machine reads it
    let movi = pf_avi::probe_header(&o.head).unwrap().1.unwrap();
    let mut want = Vec::new();
    let idx1 = reads(&mut Idx1At::new(movi.movi_end, b.len() as u64), &b, &mut want).unwrap();
    assert!(idx1.is_some());
    // the XviD VOL, in the first key chunk only, is the video track's head
    let vol = o.codec_head(v);
    let first = pf_avi::first_chunk_of(&b[movi.movi_data_start as usize..], 0).unwrap();
    assert_eq!(Some(vol), pf_avi::vol_prefix(first));
    assert!(vol.starts_with(&[0, 0, 1, 0xB0]), "{vol:x?}");
    let caps = Caps { window: 4096, ..Caps::default() };
    let (o2, log) = open_with(&b, Want { index: false, meta: true }, caps);
    assert_eq!(o2.unwrap().tracks, o.tracks, "the VOL read past the head too");
    let vol_read = log.iter().filter(|n| n.at == movi.movi_data_start).count();
    assert_eq!(vol_read, 1, "one read at movi for the VOL, {log:?}");
    assert!(log.len() <= fhs_avi(&b).len() + 2, "{log:?}");
    let (_, log) = open_with(&b, Want::ALL, caps);
    assert!(
        log.len() <= fhs_avi(&b).len() + 2 + want.len(),
        "the idx1 the demuxer read moves here, {log:?}"
    );
}

/// One second of 64x48 MJPEG beside a sine at `rate`, coded as `args` say last, made once by
/// ffmpeg, None without it.
fn ffmpeg_avi(name: &str, rate: u32, args: &[&str]) -> Option<Vec<u8>> {
    use std::sync::atomic::{AtomicU32, Ordering};
    let path = std::env::temp_dir().join(format!("pf_open_avi_{name}_v1.avi"));
    if let Ok(b) = std::fs::read(&path) {
        return Some(b);
    }
    // tests make them at once, each its own copy until the rename
    static MADE: AtomicU32 = AtomicU32::new(0);
    let n = MADE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{n}.avi", std::process::id()));
    let ok = std::process::Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=10:duration=1"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:sample_rate={rate}:duration=1"))
        .args(["-c:v", "mjpeg"])
        .args(args)
        .args(["-f", "avi"])
        .arg(&tmp)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: ffmpeg could not make {name}");
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    std::fs::rename(&tmp, &path).unwrap();
    Some(std::fs::read(&path).unwrap())
}

/// A WAVE_FORMAT_EXTENSIBLE track is described by its SubFormat, its valid bits and its mask,
/// as ffmpeg writes PCM past 16 bits or 48 kHz and AC-3 of more than two channels.
#[test]
fn an_avi_extensible_track_takes_its_subformat_valid_bits_and_speakers() {
    let made = |name, rate, codec, ac| ffmpeg_avi(name, rate, &["-c:a", codec, "-ac", ac]);
    let (Some(s24), Some(f32), Some(s16), Some(ac3)) = (
        made("s24x6", 48_000, "pcm_s24le", "6"),
        made("f32x2", 96_000, "pcm_f32le", "2"),
        made("s16x2", 48_000, "pcm_s16le", "2"),
        made("ac3x6", 48_000, "ac3", "6"),
    ) else {
        return;
    };
    let a = |b: &[u8]| {
        let t = open(b).tracks[1];
        (t.kind, t.family, t.channels, t.rate, t.depth, t.layout)
    };
    assert_eq!(a(&s24), (TrackKind::Audio, "audio/raw", 6, 48_000, 24, 0x3F));
    assert_eq!(open(&s24).tracks[1].bitrate, 48_000 * 18 * 8);
    // 20 bits valid in the 24 bit container
    let strf = s24.windows(4).position(|w| w == b"auds").unwrap();
    let strf = strf + s24[strf..].windows(4).position(|w| w == b"strf").unwrap();
    assert_eq!(&s24[strf + 8..strf + 10], &[0xFE, 0xFF], "WAVE_FORMAT_EXTENSIBLE");
    let mut short = s24.clone();
    short[strf + 26..strf + 28].copy_from_slice(&20u16.to_le_bytes());
    assert_eq!(a(&short), (TrackKind::Audio, "audio/raw", 6, 48_000, 20, 0x3F));
    assert_eq!(a(&f32), (TrackKind::Audio, "audio/raw", 2, 96_000, 32, 0x3));
    assert_eq!(a(&s16), (TrackKind::Audio, "audio/raw", 2, 48_000, 16, 0), "WAVEFORMATEX");
    let (kind, family, channels, rate, _, layout) = a(&ac3);
    assert_eq!((kind, family, channels, rate, layout), (TrackKind::Audio, "ac3", 6, 48_000, 0));
}

/// ffmpeg's global header puts the VOL in `strf` alone and counts it in `biSize`, the track's
/// head is that VOL whole, not what lies past `biSize`.
#[test]
fn an_avi_with_the_vol_in_its_strf_has_it_whole_as_the_head() {
    let mpeg4 = ["-c:v", "mpeg4", "-vtag", "XVID", "-flags", "+global_header", "-an"];
    let Some(b) = ffmpeg_avi("mpeg4_global", 48_000, &mpeg4) else { return };
    let o = open(&b);
    let v = &o.tracks[0];
    assert_eq!(v.family, "mpeg4/asp");
    let strf = b.windows(4).position(|w| w == b"strf").unwrap() + 8;
    let size = u32::from_le_bytes(b[strf..strf + 4].try_into().unwrap()) as usize;
    assert!(size > 40, "biSize {size} counts the extradata");
    let vol = o.codec_head(v);
    assert_eq!(vol, &b[strf + 40..strf + size]);
    assert_eq!(pf_avi::vol_prefix(vol), Some(vol), "{vol:x?}");
    let movi = b.windows(4).position(|w| w == b"movi").unwrap();
    let in_band = |w: &[u8]| w[..3] == [0, 0, 1] && w[3] & 0xF0 == 0x20;
    assert!(!b[movi..].windows(4).any(in_band), "no VOL in a chunk");
}

#[test]
fn an_flv_describes_what_its_head_states() {
    let b = fixture("plugins/formats/flv/tests/fixtures/h264_aac.flv");
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log, [Need { at: 0, len: b.len() }], "the HLS source read a prefix and the head");
    assert_eq!(o.kind, Kind::Flv);
    let v = &o.tracks[0];
    assert_eq!(
        (v.id, v.kind, v.family, v.width, v.height, v.nal_length),
        (1, TrackKind::Video, "h264/annexb", 128, 96, 4)
    );
    assert!(o.codec_head(v).starts_with(&[0, 0, 0, 1]));
    let a = &o.tracks[1];
    assert_eq!(
        (a.id, a.kind, a.family, a.channels, a.rate),
        (2, TrackKind::Audio, "aac", 2, 44_100)
    );
    assert!(!o.codec_head(a).is_empty());
    assert!(
        (o.duration_ns as i64 - 4_133 * MS as i64).abs() < 100 * MS as i64,
        "{}",
        o.duration_ns
    );
    assert_eq!(&o.head[..], &b[..], "a file under the demuxer's head whole");
    let o = open(&fixture("plugins/formats/flv/tests/fixtures/flv1_mp3.flv"));
    let fams: Vec<_> = o.tracks.iter().map(|t| (t.family, t.rate)).collect();
    assert_eq!(fams[0].0, "flv1");
    assert_eq!(fams[1].0, "mp3");
    assert!(fams[1].1 > 0);
    // a window short of the demuxer's head is grown to it, never read twice
    let (_, log) = open_with(&b, Want::ALL, Caps { window: 1000, ..Caps::default() });
    assert_eq!(log, [Need { at: 0, len: 1000 }, Need { at: 1000, len: b.len() - 1000 }]);
}

/// Every kind the sniff tells opens, none is refused as unsupported.
#[test]
fn every_kind_the_sniff_tells_opens() {
    for (rel, kind) in [
        ("plugins/formats/ts/tests/fixtures/h264_aac.ts", Kind::Ts),
        ("plugins/formats/ps/tests/fixtures/mpeg2_ac3.vob", Kind::Ps),
    ] {
        let (o, _) = open_with(&fixture(rel), Want::ALL, Caps::default());
        assert_eq!(o.map(|o| o.kind), Ok(kind), "{rel}");
    }
}

#[test]
fn an_open_states_its_brand() {
    assert_eq!(open(&own("tone.webm")).brand(), Brand::WebM);
    assert_eq!(open(&own("tagged.mkv")).brand(), Brand::Matroska);
    assert_eq!(open(&own("tone.m4a")).brand(), Brand::M4a);
    let mut b = fixture("plugins/formats/mp4/tests/fixtures/tiny_av.mp4");
    assert_eq!(open(&b).brand(), Brand::Mp4);
    // the same movie stated QuickTime, the major and every compatible brand
    for at in (8..32).step_by(4).filter(|&at| at != 12) {
        b[at..at + 4].copy_from_slice(b"qt  ");
    }
    let o = open(&b);
    assert_eq!((o.kind, o.brand(), o.tracks.len()), (Kind::Mp4, Brand::QuickTime, 2));
}

/// An MP4's AV1 and Opus tracks send no head, the open states their av1C and the OpusHead of
/// their dOps as Matroska does.
#[test]
fn an_mp4s_av1c_and_opus_head_are_its_tracks_heads() {
    let o = open(&own("av1.mp4"));
    let t = &o.tracks[0];
    assert_eq!(t.family, "av1");
    assert_eq!(o.codec_head(t)[..4], [0x81, 0, 0x0c, 0]);
    let o = open(&own("opus_5_1.mp4"));
    let t = &o.tracks[0];
    assert_eq!((t.family, t.channels), ("opus", 6));
    let mut want = b"OpusHead".to_vec();
    want.extend_from_slice(&[1, 6, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 1, 4, 2, 0, 4, 1, 2, 3, 5]);
    assert_eq!(o.codec_head(t), want);
    assert_eq!(t.layout.count_ones(), 6, "the mapping family's layout");
}
