#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_mkv::CuesAt;
use pf_mkv::walk::MetaAt;
use pf_open::{Caps, Index, Kind, Meta, OpenError, Opened, TrackKind, Want};
use pf_probe::head::{MkvHeadAt, MkvHeadError};
use profluens_core::fetch::Need;

/// What the tunnel opener reads today, the head a top level element head at a time, the Cues and
/// the Tags with the attached pictures, each through its own machine.
fn fhs_tunnel(bytes: &[u8]) -> Vec<Need> {
    let mut log = Vec::new();
    let h = reads(&mut MkvHeadAt::new(64 << 20), bytes, &mut log).unwrap();
    reads(&mut CuesAt::new(&h.head, 32 << 20), bytes, &mut log);
    reads(&mut MetaAt::new(&h.head, 4 << 20, 16 << 20), bytes, &mut log);
    log
}

/// The HLS source, the sniff prefix, the head under 8 MB and the Cues.
fn fhs_hls(bytes: &[u8]) -> Vec<Need> {
    let mut log = vec![Need { at: 0, len: pf_probe::PREFIX_LEN }];
    let h = reads(&mut MkvHeadAt::new(8 << 20), bytes, &mut log).unwrap();
    reads(&mut CuesAt::new(&h.head, 32 << 20), bytes, &mut log);
    log
}

fn cues(o: &Opened) -> (u64, Vec<(u64, u64)>, Option<u64>) {
    let Index::Mkv { first_cluster, cues, at } = &o.index else { panic!("{:?}", o.index) };
    let video = cues.iter().filter(|c| c.track == 1).map(|c| (c.time_ns, c.byte)).collect();
    (*first_cluster, video, *at)
}

/// A file the window holds whole is one read, the tracks, the Cues and the meta all from it.
#[test]
fn a_file_the_window_holds_is_one_read() {
    let f = mkv_with(MkvParts { secs: 4, tags: true, pad: 0, front: false });
    assert!(f.bytes.len() < Caps::default().window);
    let (o, log) = open_with(&f.bytes, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log, [Need { at: 0, len: f.bytes.len() }]);
    assert_eq!((o.kind, o.len), (Kind::Mkv, f.bytes.len() as u64));
    assert_eq!(
        &o.head[..],
        &f.bytes[..f.clusters[0] as usize],
        "the bytes before the first Cluster"
    );
    let (first, video, at) = cues(&o);
    assert_eq!(first, f.clusters[0]);
    let want: Vec<_> =
        f.clusters.iter().enumerate().map(|(c, &b)| (c as u64 * 1000 * MS, b)).collect();
    assert_eq!((video, at), (want, Some(f.cues_at)));
    assert_eq!(o.cue_before(1, 2500 * MS), Some(f.clusters[2]));
    assert_eq!(o.cue_before(1, 0), Some(f.clusters[0]));
    assert_eq!(o.cue_before(9, 1500 * MS), Some(f.clusters[1]), "a track without cues takes all");
    assert_eq!(o.tracks.len(), 3);
    let v = &o.tracks[0];
    assert_eq!(
        (v.id, v.kind, v.family, v.width, v.height),
        (1, TrackKind::Video, "h264/annexb", 640, 360)
    );
    assert_eq!((v.nal_length, v.frame_ns, v.depth, v.par), (4, 40 * MS, 8, (1, 1)));
    assert_eq!(o.codec_head(v), [&[0, 0, 0, 1][..], &SPS, &[0, 0, 0, 1], &PPS].concat());
    let a = &o.tracks[1];
    // no SamplingFrequency element, its default
    assert_eq!((a.kind, a.family, a.channels, a.rate), (TrackKind::Audio, "opus", 2, 8_000));
    assert_eq!((a.delay_ns, a.seek_preroll_ns), (MKV_DELAY_NS, 80 * MS));
    assert_eq!(o.codec_head(a), opus_head(2, 312));
    assert_eq!((o.language(a), o.language(&o.tracks[2])), ("eng", "nor"));
    // the statistics tags behind the Cues name tracks 1 and 2 by their uids
    assert_eq!((v.bitrate, a.bitrate, o.tracks[2].bitrate), (2_500_000, 96_000, 0));
    let Meta::Mkv(m) = &o.meta else { panic!("{:?}", o.meta) };
    assert!(m.error.is_none() && m.pictures.is_empty());
    assert_eq!(m.tags.len() as u64, f.bytes.len() as u64 - f.tags_at.unwrap());
    assert!(o.skipped.is_empty());
}

fn big(tags: bool) -> SynthMkv {
    let f = mkv_with(MkvParts { secs: 40, tags, pad: 2000, front: false });
    assert!(f.bytes.len() > 4 * Caps::default().window);
    f
}

/// A head in the window and Cues and Tags at the end are two reads, where the tunnel opener read
/// every top level element head, the head, the Cues and the Tags apart.
#[test]
fn the_cues_and_tags_at_the_end_are_one_read() {
    let f = big(true);
    let (o, log) = open_with(&f.bytes, Want::ALL, Caps::default());
    let o = o.unwrap();
    let len = f.bytes.len() as u64;
    let tail = Need { at: f.cues_at, len: (len - f.cues_at) as usize };
    assert_eq!(log, [Need { at: 0, len: Caps::default().window }, tail]);
    let today = fhs_tunnel(&f.bytes);
    assert!(today.len() >= 9, "{today:?}");
    assert_eq!(cues(&o).1.len(), 40);
    assert_eq!(cues(&o).2, Some(f.cues_at));
    assert_eq!(o.tracks[0].bitrate, 2_500_000);
    let Meta::Mkv(m) = &o.meta else { panic!() };
    assert!(!m.tags.is_empty());
    // without the meta the same, the Cues alone at the end
    let (_, log) = open_with(&f.bytes, Want { index: true, meta: false }, Caps::default());
    assert_eq!(log, [Need { at: 0, len: Caps::default().window }, tail]);
    assert!(log.len() <= fhs_hls(&f.bytes).len());
    let (o, log) = open_with(&f.bytes, Want::TRACKS, Caps::default());
    assert_eq!(log.len(), 1, "the tracks alone are the window");
    assert!(matches!(o.unwrap().index, Index::Mkv { at: None, .. }));
}

/// Cues past the tail cap are read by their own machine, its head then its body.
#[test]
fn cues_past_the_tail_cap_are_read_apart() {
    let f = big(false);
    let caps = Caps { max_tail: 64, ..Caps::default() };
    let (o, log) = open_with(&f.bytes, Want::ALL, caps);
    let o = o.unwrap();
    let cue_len = f.bytes.len() - f.cues_at as usize;
    let window = Need { at: 0, len: caps.window };
    assert_eq!(
        log,
        [window, Need { at: f.cues_at, len: 12 }, Need { at: f.cues_at, len: cue_len }]
    );
    assert_eq!(cues(&o).1.len(), 40);
    assert!(log.len() < fhs_tunnel(&f.bytes).len());
}

/// A window short of the head reads the element heads past it and the rest of the head, never
/// the bytes it holds again.
#[test]
fn a_head_past_the_window_reads_only_past_it() {
    let f = big(true);
    let caps = Caps { window: 100, ..Caps::default() };
    let (o, log) = open_with(&f.bytes, Want::ALL, caps);
    let o = o.unwrap();
    assert_eq!(&o.head[..], &f.bytes[..f.clusters[0] as usize]);
    assert_eq!(log[0], Need { at: 0, len: 100 });
    let head_reads: Vec<_> = log[1..].iter().filter(|n| n.at < f.clusters[0]).collect();
    assert!(head_reads.iter().all(|n| n.at >= 100), "{log:?}");
    assert!(log.len() <= fhs_tunnel(&f.bytes).len(), "{log:?}");
    assert_eq!(o.tracks.len(), 3);
}

#[test]
fn refusals() {
    let (o, log) = open_with(b"not a matroska file", Want::ALL, Caps::default());
    assert!(matches!(o, Err(OpenError::Unknown(_))), "{o:?}");
    assert_eq!(log.len(), 1);
    let f = mkv_file(1);
    let (o, _) = open_with(&f.bytes[..200], Want::ALL, Caps::default());
    assert!(o.is_err(), "a file cut in the head");
    let (o, _) = open_with(&[], Want::ALL, Caps::default());
    assert!(matches!(o, Err(OpenError::Unknown(_))));
}

#[test]
fn a_cues_position_past_u64_is_no_index() {
    let mut f = mkv_file(2).bytes;
    let mut pos = vec![0x53, 0xAC, 0x01, 0, 0, 0, 0, 0, 0, 0x08];
    pos.extend(u64::MAX.to_be_bytes());
    patch_last(&mut f, &[0x53, 0xAC, 0x01, 0, 0, 0, 0, 0, 0, 0x08], &pos);
    let o = open(&f);
    assert_eq!(cues(&o).1, []);
    assert_eq!(cues(&o).2, None);
}

#[test]
fn huge_cues_are_not_read() {
    for window in [256 << 10, 64] {
        let mut f = mkv_file(2).bytes;
        patch_last(
            &mut f,
            &[0x1C, 0x53, 0xBB, 0x6B, 0x01],
            &[0x1C, 0x53, 0xBB, 0x6B, 0x01, 0x10, 0, 0, 0, 0, 0, 0],
        );
        let caps = Caps { window, max_tail: 0, ..Caps::default() };
        let (o, log) = open_with(&f, Want::ALL, caps);
        let o = o.unwrap();
        assert_eq!(cues(&o).1, []);
        assert!(log.iter().all(|n| n.len <= f.len()), "nothing of the 2^52 bytes asked: {log:?}");
    }
}

#[test]
fn a_head_past_the_cap_is_refused_before_it_is_read() {
    let mut f = el(&EBML_ID, &el(&[0x42, 0x82], b"matroska"));
    f.extend(SEGMENT_UNKNOWN);
    // a Void of 2^40 bytes, the Cluster behind it
    f.extend([0xEC, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let (o, log) = open_with(&f, Want::ALL, Caps::default());
    assert!(matches!(o, Err(OpenError::MkvHead(MkvHeadError::TooLarge { .. }))), "{o:?}");
    assert!(log.iter().all(|n| n.at < f.len() as u64), "{log:?}");
    let mut f = el(&EBML_ID, &el(&[0x42, 0x82], b"matroska"));
    f.extend(SEGMENT_UNKNOWN);
    for _ in 0..5000 {
        f.extend([0xEC, 0x80]);
    }
    let (o, _) = open_with(&f, Want::ALL, Caps::default());
    assert!(matches!(o, Err(OpenError::MkvHead(MkvHeadError::TooMany { .. }))), "{o:?}");
}

/// The HLS cap ends a head that crosses it once Tracks is read, the tunnel's holds it.
#[test]
fn the_head_cap_is_the_callers() {
    let f = mkv_file(2);
    let cut = f.clusters[0] as usize;
    let size: u64 = 9 << 20;
    let mut att = vec![0x19, 0x41, 0xA4, 0x69, 0x01];
    att.extend_from_slice(&size.to_be_bytes()[1..]);
    let rest_at = (cut + att.len()) as u64 + size;
    let mut file = f.bytes[..cut].to_vec();
    file.extend_from_slice(&att);
    file.resize(rest_at as usize, 0);
    file.extend_from_slice(&f.bytes[cut..]);
    let hls = Caps { max_head: 8 << 20, ..Caps::default() };
    let o = open_with(&file, Want::TRACKS, hls).0.unwrap();
    assert_eq!((o.head.len(), cues(&o).0), (cut, rest_at), "cut before the Attachments");
    let o = open_with(&file, Want::TRACKS, Caps::default()).0.unwrap();
    assert_eq!((o.head.len() as u64, cues(&o).0), (rest_at, rest_at), "the Attachments held");
}

/// Fonts attached past the head cap behind Tracks. The head stops before the Attachments, the
/// first Cluster is found behind them, the fonts are never read.
#[test]
fn attachments_past_the_cap_end_the_head_before_them() {
    let f = mkv_file(2);
    let cut = f.clusters[0] as usize;
    let size: u64 = 80 << 20;
    let mut att = vec![0x19, 0x41, 0xA4, 0x69, 0x01];
    att.extend_from_slice(&size.to_be_bytes()[1..]);
    let body_at = (cut + att.len()) as u64;
    let rest_at = body_at + size;
    let mut file = Vec::with_capacity(rest_at as usize + f.bytes.len() - cut);
    file.extend_from_slice(&f.bytes[..cut]);
    file.extend_from_slice(&att);
    file.resize(rest_at as usize, 0);
    file.extend_from_slice(&f.bytes[cut..]);
    let (o, log) = open_with(&file, Want::ALL, Caps::default());
    let o = o.unwrap();
    let window = Caps::default().window as u64;
    let fonts =
        |n: &Need| n.at + n.len as u64 > window.max(body_at) && n.at < rest_at && n.len > 12;
    assert!(!log.iter().any(fonts), "the fonts left unread: {log:?}");
    assert_eq!(&o.head[..], &f.bytes[..cut], "the head ends where the Attachments start");
    assert_eq!(cues(&o).0, rest_at);
    assert_eq!(o.tracks.len(), 3);
    assert!(log.len() <= fhs_tunnel(&file).len(), "{log:?}");
}

/// A release's tags at the end and a cover beside a font in the head, the cover kept and the
/// font left, in fewer reads than the tunnel's three machines.
#[test]
fn a_tagged_release_reads_its_meta_with_its_head() {
    let b = own("tagged.mkv");
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    let Meta::Mkv(m) = &o.meta else { panic!("{:?}", o.meta) };
    assert!(m.error.is_none());
    assert!(!m.tags.is_empty());
    assert_eq!(m.pictures.len(), 1, "the cover, not the font");
    assert!(m.pictures[0].media_type.starts_with("image/"));
    assert_eq!(log.len(), 1, "the window holds the file");
    let caps = Caps { window: 4096, ..Caps::default() };
    let (small, log) = open_with(&b, Want::ALL, caps);
    let small = small.unwrap();
    assert_eq!(small.meta, o.meta);
    assert_eq!(small.tracks, o.tracks);
    assert!(log.len() <= fhs_tunnel(&b).len(), "{log:?} against {:?}", fhs_tunnel(&b));
    let a = &o.tracks[0];
    assert_eq!((a.family, a.channels, a.rate), ("ac3", 3, 48_000));
    assert_eq!(o.duration_ns, 1_000 * MS);
}

/// An Opus WebM, its CodecDelay and SeekPreRoll as stated.
#[test]
fn a_webm_opus_track() {
    let o = open(&own("tone.webm"));
    assert_eq!(o.tracks.len(), 1);
    let t = &o.tracks[0];
    assert_eq!((t.family, t.channels, t.rate, t.kind), ("opus", 2, 48_000, TrackKind::Audio));
    assert!(t.seek_preroll_ns > 0 && t.delay_ns > 0, "{t:?}");
    assert!(
        (o.duration_ns as i64 - 12_008 * MS as i64).abs() < 10 * MS as i64,
        "{}",
        o.duration_ns
    );
}

/// An avcC whose NAL lengths are 2 octets is described with them, the tunnel leaves it out.
#[test]
fn a_two_octet_nal_length_is_stated() {
    let f = mkv_file(1);
    let mut b = f.bytes.clone();
    // lengthSizeMinusOne in the avcC's fifth octet
    let rec = avcc(&SPS, &PPS);
    let at = b.windows(rec.len()).position(|w| w == rec).unwrap();
    b[at + 4] = 0xFD;
    let o = open(&b);
    assert_eq!(o.tracks[0].nal_length, 2);
    // a record that does not parse is skipped, the rest described
    let mut b = f.bytes.clone();
    b[at] = 9;
    let o = open(&b);
    assert_eq!(o.tracks.len(), 2);
    assert_eq!(
        o.skipped,
        [pf_open::Skipped { id: 1, family: "h264/annexb", why: pf_open::SkipWhy::BadHead }]
    );
}

/// Cues ahead of the Clusters are in the head, read with it and never again, whether the window
/// holds them or the head machine read past it.
#[test]
fn cues_in_the_head_are_not_read_again() {
    let f = mkv_with(MkvParts { secs: 40, tags: false, pad: 2000, front: true });
    assert!(f.cues_at < f.clusters[0]);
    for window in [256 << 10, (f.cues_at + 20) as usize] {
        let caps = Caps { window, ..Caps::default() };
        let (o, log) = open_with(&f.bytes, Want::ALL, caps);
        let o = o.unwrap();
        assert_eq!(cues(&o).1.len(), 40, "window {window}");
        assert_eq!(cues(&o).2, Some(f.cues_at));
        assert_eq!(&o.head[..], &f.bytes[..f.clusters[0] as usize]);
        let past = &log[1..];
        assert!(past.iter().all(|n| n.at + n.len as u64 <= f.clusters[0] + 12), "{log:?}");
        let again = log.iter().filter(|n| n.at < window as u64).count();
        assert_eq!(again, 1, "the window alone touches its bytes, {log:?}");
    }
}
