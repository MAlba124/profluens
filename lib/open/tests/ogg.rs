#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

mod common;

use common::*;
use pf_open::{Caps, Kind, Meta, OpenError, SkipWhy, Skipped, TrackKind, Want};
use profluens_core::fetch::Need;

/// What the tunnel opener reads today, 64 KiB of head and the last 64 KiB.
fn fhs_tunnel(len: usize) -> Vec<Need> {
    let head = Need { at: 0, len: len.min(64 << 10) };
    let at = len.saturating_sub(pf_ogg::OGG_TAIL_LEN);
    vec![head, Need { at: at as u64, len: len - at }]
}

fn pages(f: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < f.len() {
        let n = pf_ogg::PageHeader::parse(&f[at..]).unwrap().len();
        out.push(f[at..at + n].to_vec());
        at += n;
    }
    out
}

#[test]
fn opens_the_tracks_from_the_head_and_the_duration_from_the_tail() {
    let f = opus_file(7, 500, 312, 0);
    let (o, log) = open_with(&f, Want::ALL, Caps::default());
    let o = o.unwrap();
    assert_eq!(log, [Need { at: 0, len: f.len() }], "the window holds the file");
    assert_eq!(o.kind, Kind::Ogg);
    assert_eq!(o.tracks.len(), 1);
    let t = &o.tracks[0];
    assert_eq!((t.id, t.stream, t.kind, t.family), (1, 7, TrackKind::Audio, "opus"));
    assert_eq!((t.channels, t.rate, t.seek_preroll_ns), (2, 48_000, 80 * MS));
    assert_eq!(o.codec_head(t), opus_head(2, 312));
    assert_eq!(o.language(t), "nor");
    assert_eq!(t.delay_ns, 6_500_000, "pre-skip 312 samples");
    assert_eq!(o.duration_ns, 500 * 20 * MS - 6_500_000);
    assert_eq!(t.duration_ns, o.duration_ns);
    assert!(t.bitrate > 0);
    assert!(f.starts_with(&o.head));
    let Meta::Ogg(c) = o.meta else { panic!("{:?}", o.meta) };
    assert!(o.bytes(c).ends_with(b"LANGUAGE=nor"));
}

#[test]
fn a_file_longer_than_the_window_reads_only_its_ends() {
    let f = opus_file(7, 6_000, 0, 0);
    let window = Caps::default().window;
    assert!(f.len() > 2 * window);
    let (o, log) = open_with(&f, Want::ALL, Caps::default());
    assert_eq!(o.unwrap().duration_ns, 6_000 * 20 * MS);
    let tail = f.len() - pf_ogg::OGG_TAIL_LEN;
    assert_eq!(
        log,
        [Need { at: 0, len: window }, Need { at: tail as u64, len: pf_ogg::OGG_TAIL_LEN }]
    );
    assert_eq!(log.len(), fhs_tunnel(f.len()).len());
}

#[test]
fn every_stream_has_an_id_and_a_head_that_does_not_parse_is_skipped() {
    // the identification packet on a bos page of its own, RFC 3533 section 4
    let mut w = pf_ogg::OggWriter::new(2);
    let mut vorbis = Vec::new();
    for (p, g) in
        [(&b"\x01vorbis-ident"[..], 0), (b"\x03vorbis", 0), (b"\x05vorbis", 0), (b"x", 10)]
    {
        w.write_packet(&mut vorbis, p, g).unwrap();
        w.flush(&mut vorbis);
    }
    w.finish(&mut vorbis);
    let opus = opus_file(5, 4, 0, 0);
    let (v, o) = (pages(&vorbis), pages(&opus));
    let f = [&v[..1], &o[..1], &v[1..], &o[1..]].concat().concat();
    let o = open(&f);
    assert_eq!(o.tracks.iter().map(|t| (t.id, t.stream)).collect::<Vec<_>>(), [(2, 5)]);
    assert_eq!(o.skipped, [Skipped { id: 1, family: "vorbis", why: SkipWhy::BadHead }]);
}

#[test]
fn refusals() {
    let (o, _) = open_with(b"not an ogg file", Want::ALL, Caps::default());
    assert!(matches!(o, Err(OpenError::Unknown(_))));
    let bad = pf_ogg::mux_packets(1, &[(b"OpusHead", 0)]);
    let (o, _) = open_with(&bad, Want::ALL, Caps::default());
    assert_eq!(o.err(), Some(OpenError::NoStreams));
}

/// A cover in the comment packet past the window grows the head until the packet is in, only
/// when the tags are wanted.
#[test]
fn a_comment_past_the_window_grows_the_head() {
    let mut picture = b"METADATA_BLOCK_PICTURE=".to_vec();
    picture.resize(600 << 10, b'A');
    let tags = opus_tags(&[b"TITLE=Big", &picture]);
    let f = opus_file_tagged(9, 6_000, 0, 0, &tags);
    let window = Caps::default().window;
    let (o, log) = open_with(&f, Want::ALL, Caps::default());
    let o = o.unwrap();
    let Meta::Ogg(c) = o.meta else { panic!("{:?}", o.meta) };
    assert_eq!(o.bytes(c).len(), tags.len() - 8, "the body behind OpusTags");
    assert_eq!(o.title(&o.tracks[0]), "Big");
    let grown: Vec<_> = log.iter().map(|n| (n.at, n.len)).collect();
    let w = window as u64;
    assert_eq!(grown[..3], [(0, window), (w, window), (2 * w, 2 * window)], "{grown:?}");
    assert_eq!(o.head.len(), 4 * window);
    assert_eq!(o.duration_ns, 6_000 * 20 * MS);
    let (o, log) =
        open_with(&f, Want { index: true, meta: false, hdr10plus: false }, Caps::default());
    let o = o.unwrap();
    assert_eq!(log.len(), 2, "the window and the tail, {log:?}");
    assert_eq!((&o.meta, o.title(&o.tracks[0])), (&Meta::None, ""));
}

/// A Vorbis stream's head is its three header packets Xiph laced, the Matroska CodecPrivate.
#[test]
fn a_vorbis_head_laces_its_three_headers() {
    let o = open(&fixture("fixtures/out/ogg_vorbis.ogg"));
    let t = &o.tracks[0];
    assert_eq!(t.family, "vorbis");
    let h = o.codec_head(t);
    assert_eq!(h[0], 2);
    let mut at = 1;
    let mut lens = [0usize; 2];
    for l in &mut lens {
        while h[at] == 255 {
            *l += 255;
            at += 1;
        }
        *l += usize::from(h[at]);
        at += 1;
    }
    let (id, rest) = h[at..].split_at(lens[0]);
    let (comment, setup) = rest.split_at(lens[1]);
    for (p, k) in [(id, 1u8), (comment, 3), (setup, 5)] {
        assert_eq!((p[0], &p[1..7]), (k, &b"vorbis"[..]));
    }
    assert!(t.channels > 0 && t.rate > 0, "from the identification header");
}

#[test]
fn opus_fixtures() {
    let o = open(&own("tone.opus"));
    let t = &o.tracks[0];
    assert_eq!((t.family, t.channels, t.rate), ("opus", 2, 48_000));
    // the last granule less the pre-skip
    assert!((o.duration_ns as i64 - 4_000_000_000).abs() < MS as i64, "{}", o.duration_ns);
    let o = open(&fixture("plugins/formats/ogg/tests/fixtures/celt-fb-stereo-128kbps.opus"));
    assert!((o.duration_ns as i64 - 1_500_000_000).abs() < MS as i64, "{}", o.duration_ns);
}

/// A bos page behind a data packet starts a chained link, its streams are not the file's.
#[test]
fn a_chained_link_is_not_a_track() {
    let f = [opus_file(1, 20, 0, 0), opus_file(2, 20, 0, 0)].concat();
    let o = open(&f);
    assert_eq!(o.tracks.iter().map(|t| t.stream).collect::<Vec<_>>(), [1]);
}

/// A Theora stream states its picture size and frame rate (Theora I section 6.2), a head that
/// does not parse is skipped.
#[test]
fn a_theora_track_has_its_picture_size_and_frame_duration() {
    let mut ident = b"\x80theora\x03\x02\x01\0\x0a\0\x08\0\0\xa0\0\0\x78\0\0".to_vec();
    ident.extend_from_slice(&30_000u32.to_be_bytes());
    ident.extend_from_slice(&1_001u32.to_be_bytes());
    ident.extend_from_slice(&[0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 6 << 5]);
    let file = |ident: &[u8]| {
        let mut w = pf_ogg::OggWriter::new(3);
        let mut f = Vec::new();
        for (p, g) in [(ident, 0), (b"\x81theora", 0), (b"\x82theora", 0), (b"\x30", 1 << 6)] {
            w.write_packet(&mut f, p, g).unwrap();
            w.flush(&mut f);
        }
        w.finish(&mut f);
        f
    };
    let o = open(&file(&ident));
    let t = &o.tracks[0];
    assert_eq!((t.kind, t.family, t.width, t.height), (TrackKind::Video, "theora", 160, 120));
    assert_eq!(t.frame_ns, 33_366_666, "30000/1001");
    assert_eq!((t.duration_ns, o.duration_ns), (33_366_666, 33_366_666), "one frame, the last");
    ident[26..30].fill(0);
    let (o, _) = open_with(&file(&ident), Want::ALL, Caps::default());
    assert!(o.is_err_and(|e| matches!(e, OpenError::NoStreams)), "a zero frame rate");
}

/// Speex and Theora are timed by their last granule, the longest stream the file's duration.
#[test]
fn speex_and_theora_fixtures_are_timed() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/out");
    let want = [("ogg_speex.spx", 1_991_062_500), ("ogg_theora_speex.ogv", 2_000_000_000)];
    for (name, ns) in want {
        let Ok(f) = std::fs::read(root.join(name)) else {
            eprintln!("{name} not generated, skipped");
            continue;
        };
        let o = open(&f);
        assert_eq!(o.duration_ns, ns, "{name}");
        assert!(o.tracks.iter().all(|t| t.duration_ns > 0), "{name}: {:?}", o.tracks);
    }
}
