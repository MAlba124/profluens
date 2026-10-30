//! The demuxer and framers against the ffprobe tables recorded by `cargo xtask fixtures-ps`.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{fixture, key, packets, raw33, streams, units};
use pf_ps::Codec;

const FIXTURES: [&str; 8] = [
    "mpeg2_ac3.vob",
    "mpeg2_mp2.vob",
    "mpeg1_mp2.mpg",
    "lpcm16.vob",
    "lpcm24.vob",
    "dts.vob",
    "h264.vob",
    "wrap.vob",
];

fn expected(codec: &str) -> Option<Codec> {
    Some(match codec {
        "mpeg2video" => Codec::Mpeg2Video,
        "mpeg1video" => Codec::Mpeg1Video,
        "h264" => Codec::H264,
        "ac3" => Codec::Ac3,
        "mp2" => Codec::MpegAudio,
        "dts" => Codec::Dts,
        "pcm_dvd" => Codec::Lpcm,
        "dvd_nav_packet" => return None,
        other => panic!("no mapping for ffprobe codec {other}"),
    })
}

fn check(name: &str, chunk: usize) -> usize {
    let data = fixture(name);
    let theirs = packets(name);
    let mut checked = 0;
    for s in streams(name) {
        let Some(want) = expected(&s.codec) else { continue };
        let what = format!("{name} {} chunk {chunk}", s.codec);
        let (codec, ours) = units(&data, key(s.id), chunk);
        assert_eq!(codec, want, "{what}");
        let rows: Vec<_> = theirs.iter().filter(|p| p.stream == s.index).collect();
        match codec {
            Codec::Mpeg1Video | Codec::Mpeg2Video | Codec::H264 => {
                assert_eq!(ours.len(), rows.len(), "{what}: units");
                for (n, (u, row)) in ours.iter().zip(&rows).enumerate() {
                    assert_eq!(u.bytes.len(), row.size, "{what}: unit {n} size");
                    if let Some(pts) = u.pts {
                        let theirs = row.pts.map(raw33);
                        assert_eq!(Some(pts), theirs, "{what}: unit {n} pts");
                        assert_eq!(u.dts, row.dts.map(raw33), "{what}: unit {n} dts");
                    }
                    if codec != Codec::H264 {
                        assert_eq!(u.key, row.key, "{what}: unit {n} key");
                    }
                }
                assert!(ours[0].key && ours[0].pts.is_some(), "{what}: starts at a stamped key");
            }
            Codec::Lpcm => {
                assert_eq!(ours.len(), rows.len(), "{what}: units");
                for (u, row) in ours.iter().zip(&rows) {
                    assert_eq!(u.bytes.len() + 3, row.size, "{what}: the LPCM header is apart");
                    if u.pts.is_some() {
                        assert_eq!(u.pts, row.pts.map(raw33), "{what}");
                    }
                }
            }
            _ => {
                let bytes: usize = ours.iter().map(|u| u.bytes.len()).sum();
                assert_eq!(bytes, rows.iter().map(|p| p.size).sum::<usize>(), "{what}: bytes");
                let stamps: Vec<u64> = rows.iter().filter_map(|p| p.pts.map(raw33)).collect();
                for u in ours.iter().filter_map(|u| u.pts) {
                    assert!(stamps.contains(&u), "{what}: {u} is no frame's stamp");
                }
            }
        }
        checked += ours.len();
    }
    checked
}

#[test]
fn units_are_the_frames_ffprobe_lists() {
    for name in FIXTURES {
        let n = check(name, 1 << 20);
        assert!(n > 10, "{name}: {n} units");
        assert_eq!(check(name, 7), n, "{name}");
    }
}

#[test]
fn every_byte_is_read_and_no_anomaly_is_reported() {
    for name in FIXTURES {
        let data = fixture(name);
        let mut d = pf_ps::Demux::new();
        let mut anomalies = Vec::new();
        let mut sink = |e: pf_ps::Event<'_>| {
            if let pf_ps::Event::Anomaly(a) = e {
                anomalies.push(a);
            }
        };
        d.push(&data, &mut sink);
        d.flush(&mut sink);
        assert_eq!(anomalies, [], "{name}");
        let stats = d.stats();
        assert_eq!((stats.bytes, stats.junk), (data.len() as u64, 0), "{name}");
        assert!(stats.packs > 0, "{name}");
    }
}
