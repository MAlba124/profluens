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
    let caps = Caps { window: 4096, ..Caps::default() };
    let (o2, log) = open_with(&b, Want { index: false, meta: true }, caps);
    assert_eq!(o2.unwrap().tracks, o.tracks);
    assert!(log.len() <= fhs_avi(&b).len() + 1, "{log:?}");
    let (_, log) = open_with(&b, Want::ALL, caps);
    assert!(
        log.len() <= fhs_avi(&b).len() + 1 + want.len(),
        "the idx1 the demuxer read moves here, {log:?}"
    );
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

#[test]
fn what_is_not_opened_here_says_so() {
    let ts = fixture("plugins/formats/ts/tests/fixtures/h264_aac.ts");
    let (o, _) = open_with(&ts, Want::ALL, Caps::default());
    assert_eq!(o.err(), Some(pf_open::OpenError::Unsupported(Kind::Ts)));
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
