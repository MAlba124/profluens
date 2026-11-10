#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_bare::BareKind;
use pf_open::{Caps, Index, Kind, Meta, OpenError, Opened, TrackKind, Want};
use pf_probe::head::Mp4HeadAt;
use profluens_core::fetch::{Need, Window};

fn bare(name: &str) -> Vec<u8> {
    fixture(&format!("plugins/formats/bare/tests/fixtures/{name}"))
}

/// The head of the one track of a Matroska file.
fn mkv_head(bytes: &[u8]) -> Vec<u8> {
    let o = open(bytes);
    o.codec_head(&o.tracks[0]).to_vec()
}

/// What the tunnel opener reads of a bare file today.
fn fhs_bare(bytes: &[u8], kind: BareKind) -> Vec<Need> {
    let mut log = Vec::new();
    reads(&mut pf_bare::OpenAt::new(bytes.len() as u64, kind), bytes, &mut log).unwrap();
    log
}

/// The opens with a window too small for the head read no more than the tunnel's opener.
fn no_more_than_today(bytes: &[u8], kind: BareKind) -> Opened {
    let (o, log) = open_with(bytes, Want::ALL, Caps::default());
    assert_eq!(log.len(), 1, "the window holds the file, {log:?}");
    let caps = Caps { window: 64, ..Caps::default() };
    let (small, log) = open_with(bytes, Want::ALL, caps);
    let today = fhs_bare(bytes, kind);
    assert!(log.len() <= today.len() + 1, "{log:?} against {today:?}");
    let read: usize = log.iter().map(|n| n.len).sum();
    assert!(read <= today.iter().map(|n| n.len).sum::<usize>() + 64, "{log:?} against {today:?}");
    let (o, small) = (o.unwrap(), small.unwrap());
    assert_eq!(o.tracks, small.tracks);
    o
}

#[test]
fn flac_is_one_track_with_its_blocks() {
    let b = bare("tone.flac");
    let o = no_more_than_today(&b, BareKind::Flac);
    assert_eq!(o.kind, Kind::Flac);
    let t = &o.tracks[0];
    assert_eq!(
        (t.id, t.kind, t.family, t.channels, t.rate, t.depth),
        (1, TrackKind::Audio, "flac", 1, 48_000, 16)
    );
    assert!(t.bitrate > 50_000 && t.bitrate < 400_000, "{}", t.bitrate);
    assert_eq!(o.duration_ns, 2_000 * MS);
    let Index::Bare(h) = &o.index else { panic!("{:?}", o.index) };
    assert_eq!(o.codec_head(t), h.head);
    assert_eq!(
        o.codec_head(t),
        mkv_head(&bare("flac.mka")),
        "fLaC and STREAMINFO as Matroska holds them"
    );
    let Meta::Bare { raw, flac: true, .. } = &o.meta else { panic!("{:?}", o.meta) };
    assert_eq!((raw.comments.len(), raw.pictures.len()), (1, 1));
    assert!(o.head.is_empty(), "baresrc reads from the BareHead");
}

#[test]
fn mp3_is_one_track_with_its_id3_tag() {
    let b = bare("tone.mp3");
    let o = no_more_than_today(&b, BareKind::Mp3);
    let t = &o.tracks[0];
    assert_eq!((t.family, t.channels, t.rate, t.bitrate, t.depth), ("mp3", 2, 44_100, 128_000, 16));
    assert!(o.codec_head(t).is_empty(), "no head for MPEG audio");
    assert!((o.duration_ns as i64 - 78 * 26_122_448).abs() < MS as i64, "{}", o.duration_ns);
    let Meta::Bare { raw, flac: false, .. } = &o.meta else { panic!("{:?}", o.meta) };
    assert!(!raw.id3v2().is_empty());
}

#[test]
fn adts_is_one_track() {
    let b = bare("tone.aac");
    let o = no_more_than_today(&b, BareKind::Adts);
    let t = &o.tracks[0];
    assert_eq!((t.family, t.channels, t.rate, t.depth), ("aac", 1, 48_000, 16));
    assert_eq!(o.codec_head(t), mkv_head(&bare("aac.mka")), "the AudioSpecificConfig");
    assert!((o.duration_ns as i64 - 94 * 21_333_333).abs() < 50 * MS as i64, "{}", o.duration_ns);
}

#[test]
fn m4a_opens_its_audio_track_and_moov() {
    let b = own("tone.m4a");
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log, [Need { at: 0, len: b.len() }]);
    assert_eq!(o.kind, Kind::Mp4);
    let audio: Vec<_> = o.tracks.iter().filter(|t| t.kind == TrackKind::Audio).collect();
    assert_eq!(audio.len(), 1);
    let t = audio[0];
    assert_eq!((t.family, t.channels, t.rate, t.depth), ("aac", 1, 48_000, 16));
    assert_eq!(o.codec_head(t), mkv_head(&bare("aac.mka")), "the AudioSpecificConfig");
    assert!(t.bitrate > 50_000, "{}", t.bitrate);
    assert!((o.duration_ns as i64 - 2_005 * MS as i64).abs() < 2 * MS as i64, "{}", o.duration_ns);
    assert!(o.head.starts_with(&b[..8]));
    let Meta::Moov(r) = &o.meta else { panic!("{:?}", o.meta) };
    assert_eq!(&o.head[r.start + 4..r.start + 8], b"moov");
    // the tunnel read a window then the head machine over it
    let mut today = Vec::new();
    let w = reads(&mut Window::new(0, (256 << 10).min(b.len())), &b, &mut today).unwrap();
    reads(&mut Mp4HeadAt::new(&w, b.len() as u64), &b, &mut today).unwrap();
    assert!(log.len() <= today.len());
}

#[test]
fn broken_files_are_refused() {
    let junk = b"nothing like audio at all".repeat(100);
    assert!(matches!(open_with(&junk, Want::ALL, Caps::default()).0, Err(OpenError::Unknown(_))));
    let mut f = bare("tone.flac");
    f.truncate(30);
    let (o, _) = open_with(&f, Want::ALL, Caps::default());
    assert!(matches!(o, Err(OpenError::Bare(pf_bare::BareError::Malformed(_)))), "{o:?}");
}

/// The tracks alone leave the tag bytes, the open reads the same.
#[test]
fn the_meta_unwanted_is_none() {
    let (o, _) = open_with(&bare("tone.flac"), Want::TRACKS, Caps::default());
    let o = o.unwrap();
    assert_eq!(o.meta, Meta::None);
    assert!(matches!(o.index, Index::Bare(_)), "baresrc needs the head");
}
