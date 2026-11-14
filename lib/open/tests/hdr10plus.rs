#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_mp4::writer::{
    Edit, Entry, Handler, Sample, Table, TrackMoov, VIDEO_TIMESCALE, decode_times, ftyp, mdat_head,
    moov, video_head,
};
use pf_open::{Caps, Hdr10Plus, Kind, Opened, TrackKind, Want};
use profluens_core::fetch::Need;

/// A T.35 HDR10+ message, its payload's zeros escaped in an SEI.
const PLUS: [u8; 12] = [0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x01, 0x40, 0x00, 0x00, 0x00, 0x01];
/// A Dolby Vision RPU T.35 message.
const RPU: [u8; 9] = [0xB5, 0x00, 0x3B, 0x00, 0x00, 0x08, 0x00, 0x37, 0xCD];
const MIB: usize = 1 << 20;

/// An H.264 SEI NAL unit of one T.35 message.
fn sei(msg: &[u8]) -> Vec<u8> {
    let mut rbsp = vec![4, msg.len() as u8];
    rbsp.extend_from_slice(msg);
    rbsp.push(0x80);
    let mut nal = vec![0x06];
    profluens_video::bits::push_ebsp(&rbsp, &mut nal);
    nal
}

/// An IDR slice of `len` bytes.
fn idr(len: usize) -> Vec<u8> {
    let mut nal = vec![0x65];
    nal.extend((1..len).map(|i| (i % 251) as u8 | 1));
    nal
}

/// A key unit, its NAL units 4 octet length prefixed, an SEI of `msg` ahead of the slice.
fn key_unit(msg: Option<&[u8]>, slice: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in msg.map(sei).into_iter().chain([idr(slice)]) {
        out.extend_from_slice(&avc_frame(&nal));
    }
    out
}

fn video(o: &Opened) -> &pf_open::TrackDesc {
    o.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap()
}

/// What a Matroska track entry of `codec` states past its number.
struct MkvTrack<'a> {
    codec: &'a [u8],
    private: &'a [u8],
    /// A BlockAdditionMapping, its BlockAddIDValue and BlockAddIDType.
    mapping: Option<(u64, u64)>,
}

/// A Matroska file of one video track and one Cluster of `children`.
fn mkv(t: &MkvTrack<'_>, children: &[Vec<u8>]) -> (Vec<u8>, u64) {
    let ebml = el(&EBML_ID, &el(&[0x42, 0x82], b"matroska"));
    let info = el(&[0x15, 0x49, 0xA9, 0x66], &uel(&[0x2A, 0xD7, 0xB1], 1_000_000));
    let mut entry = [
        uel(&[0xD7], 1),
        uel(&[0x73, 0xC5], 101),
        uel(&[0x83], 1),
        el(&[0x86], t.codec),
        el(&[0xE0], &[uel(&[0xB0], 64), uel(&[0xBA], 64)].concat()),
    ]
    .concat();
    if !t.private.is_empty() {
        entry.extend(el(&[0x63, 0xA2], t.private));
    }
    if let Some((value, kind)) = t.mapping {
        let m = [uel(&[0x41, 0xF0], value), uel(&[0x41, 0xE7], kind)].concat();
        entry.extend(el(&[0x41, 0xE4], &m));
    }
    let tracks = el(&[0x16, 0x54, 0xAE, 0x6B], &el(&[0xAE], &entry));
    let cluster =
        el(&[0x1F, 0x43, 0xB6, 0x75], &[&uel(&[0xE7], 0)[..], &children.concat()].concat());
    let mut b = ebml;
    b.extend(SEGMENT_UNKNOWN);
    b.extend(info);
    b.extend(tracks);
    let first_cluster = b.len() as u64;
    b.extend(cluster);
    (b, first_cluster)
}

fn simple_block(track: u8, frame: &[u8]) -> Vec<u8> {
    el(&[0xA3], &block(track, 0, 0x80, frame))
}

/// A BlockGroup whose BlockAdditions hold `msg` at BlockAddID `id`.
fn group(frame: &[u8], id: u64, msg: &[u8]) -> Vec<u8> {
    let more = el(&[0xA6], &[uel(&[0xEE], id), el(&[0xA5], msg)].concat());
    el(&[0xA0], &[el(&[0xA1], &block(1, 0, 0, frame)), el(&[0x75, 0xA1], &more)].concat())
}

const AVC: MkvTrack<'static> = MkvTrack { codec: b"V_MPEG4/ISO/AVC", private: &[], mapping: None };

fn avc_track(rec: &[u8]) -> MkvTrack<'_> {
    MkvTrack { private: rec, ..AVC }
}

/// The first block's SEI marks an H.264 track, a block without one or a Dolby RPU does not, and
/// only an open that asks reads it.
#[test]
fn a_matroska_key_frame_sei_marks_the_track() {
    let rec = avcc(&SPS, &PPS);
    let (b, _) = mkv(&avc_track(&rec), &[simple_block(1, &key_unit(Some(&PLUS), 40))]);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!((video(&o).nal_length, video(&o).hdr10plus), (4, Hdr10Plus::Stream));
    assert_eq!(log.len(), 1, "the window holds the file");
    let (o, log) = open_with(&b, Want::TRACKS, Caps::default());
    assert!(!video(&o.unwrap()).hdr10plus.present(), "not asked");
    assert_eq!(log.len(), 1);
    for frame in [key_unit(None, 40), key_unit(Some(&RPU), 40)] {
        let (b, _) = mkv(&avc_track(&rec), &[simple_block(1, &frame)]);
        assert!(!video(&open(&b)).hdr10plus.present());
    }
    // the first block decides, a later one carrying it is not looked at
    let blocks =
        [simple_block(1, &key_unit(None, 40)), simple_block(1, &key_unit(Some(&PLUS), 40))];
    assert!(!video(&open(&mkv(&avc_track(&rec), &blocks).0)).hdr10plus.present());
}

/// A VP9 track's HDR10+ rides a BlockAdditional, at BlockAddID 4 unmapped as WebM writes it or at
/// the value a type 4 BlockAdditionMapping names.
#[test]
fn a_matroska_block_additional_marks_a_vp9_track() {
    let vp9 = MkvTrack { codec: b"V_VP9", private: &[], mapping: None };
    let frame = [0x82, 0x49, 0x83, 0x42, 0x00];
    let o = open(&mkv(&vp9, &[group(&frame, 4, &PLUS)]).0);
    assert_eq!((video(&o).family, video(&o).hdr10plus), ("vp9", Hdr10Plus::Addition));
    assert!(!video(&open(&mkv(&vp9, &[group(&frame, 4, &RPU)]).0)).hdr10plus.present(), "an RPU");
    assert!(
        !video(&open(&mkv(&vp9, &[group(&frame, 5, &PLUS)]).0)).hdr10plus.present(),
        "unmapped id"
    );
    let mapped = MkvTrack { mapping: Some((5, 4)), ..vp9 };
    let o = open(&mkv(&mapped, &[group(&frame, 5, &PLUS)]).0);
    assert_eq!(video(&o).hdr10plus, Hdr10Plus::Addition);
    assert!(
        !video(&open(&mkv(&vp9, &[simple_block(1, &frame)]).0)).hdr10plus.present(),
        "no additions"
    );
}

/// An AV1 track's metadata OBU carries it.
#[test]
fn a_matroska_av1_metadata_obu_marks_the_track() {
    let av1 = MkvTrack { codec: b"V_AV1", private: &[0x81, 0, 0x0C, 0], mapping: None };
    let obu = |kind: u8, p: &[u8]| [&[kind << 3 | 0x02, p.len() as u8][..], p].concat();
    let tu = |msg: &[u8]| {
        let meta = [&[4][..], msg, &[0x80]].concat();
        [obu(2, &[]), obu(5, &meta), obu(6, &[0x10, 0x20])].concat()
    };
    let o = open(&mkv(&av1, &[simple_block(1, &tu(&PLUS))]).0);
    assert_eq!(video(&o).hdr10plus, Hdr10Plus::Stream);
    // in the stream and in an addition both, the stream's, a copy keeps that
    let o = open(&mkv(&av1, &[group(&tu(&PLUS), 4, &PLUS)]).0);
    assert_eq!(video(&o).hdr10plus, Hdr10Plus::Stream);
    assert!(!video(&open(&mkv(&av1, &[simple_block(1, &tu(&RPU))]).0)).hdr10plus.present());
}

/// The first Cluster is walked an element head at a time, a read of at most 64 KiB where what is
/// held runs out. A block is read for its lead and its BlockAdditions alone, a large frame is
/// jumped over. A first block whose lead the window holds is no read.
#[test]
fn the_first_cluster_is_walked_in_bounded_reads() {
    let rec = avcc(&SPS, &PPS);
    let (b, first) = mkv(&avc_track(&rec), &[simple_block(1, &key_unit(Some(&PLUS), 40))]);
    let (o, log) = open_with(&b, Want::ALL, Caps { window: 64, ..Caps::default() });
    assert!(video(&o.unwrap()).hdr10plus.present());
    // the head machine peeks at the Cluster's element head
    let past: Vec<_> = log.iter().filter(|n| n.at + n.len as u64 > first && n.len > 12).collect();
    assert_eq!(past, [&Need { at: first, len: b.len() - first as usize }], "{log:?}");
    // a key frame of 1.5 MiB is decided on its lead, the window holds it
    let (big, _) = mkv(&avc_track(&rec), &[simple_block(1, &key_unit(Some(&PLUS), 3 * MIB / 2))]);
    let (o, log) = open_with(&big, Want::ALL, Caps::default());
    assert!(video(&o.unwrap()).hdr10plus.present(), "on its lead");
    let held = Caps::default().window as u64;
    assert!(log.iter().all(|n| n.at + n.len as u64 <= held), "{log:?}");
    // a VP9 frame of 1.5 MiB, its addition behind it is one small read at its head
    let vp9 = MkvTrack { codec: b"V_VP9", private: &[], mapping: None };
    let (b, _) = mkv(&vp9, &[group(&vec![0x82; 3 * MIB / 2], 4, &PLUS)]);
    let adds = b.windows(2).rposition(|w| w == [0x75, 0xA1]).unwrap() as u64;
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    assert_eq!(video(&o.unwrap()).hdr10plus, Hdr10Plus::Addition);
    let past: Vec<_> = log.iter().filter(|n| n.at + n.len as u64 > held).collect();
    assert_eq!(past, [&Need { at: adds, len: b.len() - adds as usize }], "{log:?}");
    // another track's block of 1 MiB ahead of the video's is jumped, one read at the video's head
    let blocks = [simple_block(2, &vec![0x82; MIB]), simple_block(1, &key_unit(Some(&PLUS), 40))];
    let (b, _) = mkv(&avc_track(&rec), &blocks);
    let video_at = b.len() - blocks[1].len();
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    assert_eq!(video(&o.unwrap()).hdr10plus, Hdr10Plus::Stream);
    let past: Vec<_> = log.iter().filter(|n| n.at + n.len as u64 > held).collect();
    assert_eq!(past, [&Need { at: video_at as u64, len: blocks[1].len() }], "{log:?}");
    // the first block whole in the window, a large one behind it unread
    let blocks = [simple_block(1, &key_unit(Some(&PLUS), 40)), simple_block(1, &idr(2 * MIB))];
    let (b, first) = mkv(&avc_track(&rec), &blocks);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    assert!(video(&o.unwrap()).hdr10plus.present());
    assert!(log.iter().all(|n| n.at + (n.len as u64) <= first.max(held)), "{log:?}");
    // a read cut short leaves the bit false and the open whole
    let (b, first) = mkv(&avc_track(&rec), &[simple_block(1, &key_unit(Some(&PLUS), 40))]);
    let cut = &b[..first as usize + 16];
    let caps = Caps { window: 64, ..Caps::default() };
    let mut m = pf_open::OpenAt::new(b.len() as u64, Want::ALL, caps);
    let mut log = Vec::new();
    let o = profluens_core::fetch::drive_slice_log(&mut m, cut, &mut log).unwrap();
    assert_eq!((o.kind, video(&o).hdr10plus.present()), (Kind::Mkv, false));
    assert_no_read_twice(&log);
    // bytes that are no element head end the walk, they are not read again
    let blocks =
        [vec![0u8; 64], simple_block(1, &key_unit(Some(&PLUS), 40)), simple_block(1, &idr(MIB))];
    let (b, _) = mkv(&avc_track(&rec), &blocks);
    let (o, log) = open_with(&b, Want::ALL, Caps { window: 64, ..Caps::default() });
    assert!(!video(&o.unwrap()).hdr10plus.present());
    assert_no_read_twice(&log);
    // a BlockGroup cut by the file's end decides on its Block's lead
    let group = el(
        &[0xA0],
        &[el(&[0xA1], &block(1, 0, 0, &key_unit(Some(&PLUS), 40))), uel(&[0xFB], 1)].concat(),
    );
    let (b, _) = mkv(&avc_track(&rec), &[group]);
    // inside the ReferenceBlock's head, an id and an 8 octet size
    let cut = &b[..b.len() - 12];
    let mut m = pf_open::OpenAt::new(b.len() as u64, Want::ALL, Caps::default());
    let o = profluens_core::fetch::drive_slice(&mut m, cut).unwrap();
    assert_eq!(video(&o).hdr10plus, Hdr10Plus::Stream);
}

/// No span read twice, a walk that cannot go on stops.
fn assert_no_read_twice(log: &[Need]) {
    for (i, n) in log.iter().enumerate() {
        assert!(!log[..i].contains(n), "{n:?} read again in {log:?}");
    }
}

/// A progressive MP4 of one H.264 track, its first sync sample `key` behind `lead` bytes of the
/// `mdat` no sample takes, then a P frame, the `moov` last.
fn mp4(key: &[u8], lead: usize) -> (Vec<u8>, u64) {
    let head = video_head(1, Entry::Avc(avcc(&SPS, &PPS)), 64, 64);
    let mut table = Table::new(1).unwrap();
    let mut file = Vec::new();
    ftyp(&mut file, false);
    let patch = mdat_head(&mut file);
    file.resize(file.len() + lead, 0);
    let at = file.len() as u64;
    let p = avc_frame(&[0x41, 0x9A, 0x02]);
    for (i, s) in [key, &p[..]].into_iter().enumerate() {
        let chunk = (i == 0).then_some(file.len() as u64);
        let sample = Sample { size: s.len() as u32, pts: i as i64 * 3600, hint: 0, sync: i == 0 };
        table.push(1, sample, chunk).unwrap();
        file.extend_from_slice(s);
    }
    let mdat_len = file.len() as u64 - (patch as u64 - 8);
    file[patch..patch + 8].copy_from_slice(&mdat_len.to_be_bytes());
    let mut dts = vec![0i64; table.len()];
    decode_times(Handler::Video, VIDEO_TIMESCALE, &table.pts, &table.hints, None, None, &mut dts);
    let track = TrackMoov {
        head: &head,
        edit: Edit::default(),
        table: Some(&table),
        dts: &dts,
        media_duration: 7200,
    };
    moov(&mut file, &[track], false);
    (file, at)
}

/// The first sync sample's SEI marks the track. One the window holds is no read, one past it is
/// one read of it, at most 64 KiB.
#[test]
fn an_mp4_first_sync_sample_marks_the_track() {
    let (b, _) = mp4(&key_unit(Some(&PLUS), 40), 0);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!((video(&o).nal_length, video(&o).hdr10plus), (4, Hdr10Plus::Stream));
    assert_eq!(log.len(), 1, "{log:?}");
    let (o, _) = open_with(&b, Want::TRACKS, Caps::default());
    assert!(!video(&o.unwrap()).hdr10plus.present());
    // asked for alone, the tables are folded for it
    let only = Want { hdr10plus: true, ..Want::TRACKS };
    let (o, _) = open_with(&b, only, Caps::default());
    let o = o.unwrap();
    assert!(video(&o).hdr10plus.present() && matches!(o.index, pf_open::Index::None));
    for key in [key_unit(None, 40), key_unit(Some(&RPU), 40)] {
        assert!(!video(&open(&mp4(&key, 0).0)).hdr10plus.present());
    }
    // behind the window
    let key = key_unit(Some(&PLUS), 40);
    let (b, at) = mp4(&key, 300 << 10);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    assert!(video(&o.unwrap()).hdr10plus.present());
    assert!(log.contains(&Need { at, len: key.len() }), "{log:?}");
    let (b, at) = mp4(&key_unit(Some(&PLUS), 3 * MIB / 2), 300 << 10);
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    assert!(video(&o.unwrap()).hdr10plus.present(), "on its prefix");
    assert!(log.contains(&Need { at, len: 64 << 10 }), "{log:?}");
}

/// A transport stream's first video payloads are scanned, no read past the open's.
#[test]
fn a_transport_streams_first_payload_marks_the_track() {
    let ts = |msg: Option<&[u8]>| {
        let (mut out, mut cc, mut vcc) = (Vec::new(), [0u8; 2], 0u8);
        tables(0x1000, &[(0x1B, 0x100)], &mut out, &mut cc);
        for f in 0..3u64 {
            let mut nals = vec![SPS.to_vec(), PPS.to_vec()];
            nals.extend(msg.filter(|_| f == 0).map(sei));
            nals.push(if f == 0 { idr(200) } else { vec![0x41, 0x9A, 0x02] });
            let unit: Vec<u8> = nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect();
            packets(0x100, &mut vcc, &pes(0xE0, Some(f * 3600), &unit), &mut out);
        }
        out
    };
    let b = ts(Some(&PLUS));
    let (o, log) = open_with(&b, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!((video(&o).family, video(&o).hdr10plus), ("h264/annexb", Hdr10Plus::Stream));
    assert_eq!(log.len(), 1);
    assert!(!video(&open_with(&b, Want::TRACKS, Caps::default()).0.unwrap()).hdr10plus.present());
    assert!(!video(&open(&ts(None))).hdr10plus.present());
    assert!(!video(&open(&ts(Some(&RPU)))).hdr10plus.present());
}
