//! The demuxer against ffprobe tables recorded by `cargo xtask fixtures-ts`. A unit covers a run
//! of oracle packets since ffprobe splits frames, video runs have length one.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{FIXTURES, Fixture, Owned, anomalies, fixture, pmts, run, units_of};
use pf_mpegts::time::{Step, TS_MODULUS, Unwrapper};
use pf_mpegts::{Codec, Config, UF_RANDOM_ACCESS, language_str};

struct OracleStream {
    index: usize,
    pid: u16,
    codec: String,
    video: bool,
    language: Option<String>,
}

struct OraclePacket {
    stream: usize,
    pts: i64,
    dts: i64,
    size: usize,
    pos: Option<u64>,
    key: bool,
}

fn parse_streams(text: &str) -> Vec<OracleStream> {
    let mut out: Vec<OracleStream> = Vec::new();
    for line in text.lines().filter(|l| l.contains("stream|")) {
        let field = |key: &str| {
            line.split('|').find_map(|f| f.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        };
        let index: usize = field("index").expect("index").parse().expect("index number");
        let id = field("id").expect("id").trim_start_matches("0x");
        let stream = OracleStream {
            index,
            pid: u16::from_str_radix(id, 16).expect("hex id"),
            codec: field("codec_name").expect("codec_name").to_string(),
            video: field("codec_type") == Some("video"),
            language: field("tag:language").map(str::to_string),
        };
        match out.iter_mut().find(|s| s.index == index) {
            Some(known) => *known = stream,
            None => out.push(stream),
        }
    }
    out
}

fn parse_packets(text: &str) -> Vec<OraclePacket> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            OraclePacket {
                stream: f[0].parse().expect("stream index"),
                pts: f[1].parse().expect("pts"),
                dts: f[2].parse().expect("dts"),
                size: f[3].parse().expect("size"),
                pos: f[4].parse().ok(),
                key: f[5].starts_with('K'),
            }
        })
        .collect()
}

fn expected_codec(name: &str) -> Codec {
    match name {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "mpeg2video" => Codec::Mpeg2Video,
        "aac" => Codec::AacAdts,
        "aac_latm" => Codec::AacLatm,
        "ac3" => Codec::Ac3,
        "eac3" => Codec::Eac3,
        "mp2" => Codec::MpegAudio,
        other => panic!("no mapping for ffprobe codec {other}"),
    }
}

/// ffprobe moves pre-rollover timestamps below zero, the stream carries them modulo 2^33.
fn raw33(v: i64) -> u64 {
    v.rem_euclid(TS_MODULUS as i64) as u64
}

fn check(f: &Fixture) -> (usize, usize) {
    let (events, stats) = run(Config::default(), f.data);
    assert_eq!(anomalies(&events), [], "{} is a clean stream", f.name);
    assert_eq!(stats.bytes, f.data.len() as u64);
    assert_eq!(stats.packets, (f.data.len() / 188) as u64);

    let oracle_streams = parse_streams(f.streams);
    let oracle_packets = parse_packets(f.packets);
    let maps = pmts(&events);
    assert_eq!(maps.len(), 1, "{} has one program map", f.name);
    let (_, streams) = maps[0];
    assert_eq!(streams.len(), oracle_streams.len(), "{} stream count", f.name);

    let mut unit_total = 0;
    for os in &oracle_streams {
        let info = streams.iter().find(|s| s.pid == os.pid).expect("stream listed in the map");
        assert_eq!(info.codec, expected_codec(&os.codec), "{} pid {:#x} codec", f.name, os.pid);
        assert_eq!(
            language_str(info),
            os.language.as_deref(),
            "{} pid {:#x} language",
            f.name,
            os.pid
        );

        let packets: Vec<&OraclePacket> =
            oracle_packets.iter().filter(|p| p.stream == os.index).collect();
        let ours = units_of(&events, os.pid);
        let mut at = 0;
        for (n, unit) in ours.iter().enumerate() {
            let ctx = format!("{} pid {:#x} unit {n}", f.name, os.pid);
            let first = packets.get(at).unwrap_or_else(|| panic!("{ctx} has no oracle packet"));
            let pts = unit.pts.unwrap_or_else(|| panic!("{ctx} has no pts"));
            assert_eq!(pts, raw33(first.pts), "{ctx} pts");
            assert_eq!(unit.dts.unwrap_or(pts), raw33(first.dts), "{ctx} dts");
            assert_eq!(Some(unit.offset), first.pos, "{ctx} position");
            let mut size = 0;
            let mut run_len = 0;
            while size < unit.payload.len() {
                let p = packets.get(at).unwrap_or_else(|| panic!("{ctx} runs past the oracle"));
                size += p.size;
                at += 1;
                run_len += 1;
            }
            assert_eq!(size, unit.payload.len(), "{ctx} size");
            if os.video {
                assert_eq!(run_len, 1, "{ctx} holds one access unit");
                assert_eq!(unit.flags & UF_RANDOM_ACCESS != 0, first.key, "{ctx} key flag");
            }
            assert_eq!(
                unit.flags & !UF_RANDOM_ACCESS & !pf_mpegts::UF_DATA_ALIGNED,
                0,
                "{ctx} flags"
            );
        }
        assert_eq!(at, packets.len(), "{} pid {:#x} covers every oracle packet", f.name, os.pid);
        if os.video {
            assert_eq!(ours.len(), packets.len());
        }
        unit_total += ours.len();
    }
    let all = events.iter().filter(|e| matches!(e, Owned::Pes(_))).count();
    assert_eq!(all, unit_total, "{} units outside the oracle streams", f.name);
    assert_eq!(stats.pes_units, unit_total as u64);
    (unit_total, oracle_packets.len())
}

#[test]
fn every_fixture_matches_ffprobe() {
    for f in FIXTURES {
        let (units, packets) = check(f);
        println!("{:<14} {units:>4} units cover {packets:>4} ffprobe packets", f.name);
    }
}

#[test]
fn b_frames_have_distinct_pts_and_dts() {
    let f = fixture("bframes");
    let (events, _) = run(Config::default(), f.data);
    let video = units_of(&events, 0x100);
    assert!(video.iter().any(|u| u.dts.is_some_and(|d| Some(d) != u.pts)));
    let dts: Vec<u64> = video.iter().map(|u| u.dts.or(u.pts).unwrap()).collect();
    assert!(dts.windows(2).all(|w| w[0] < w[1]));
    let pts: Vec<u64> = video.iter().map(|u| u.pts.unwrap()).collect();
    assert!(pts.windows(2).any(|w| w[0] > w[1]));
}

#[test]
fn audio_units_pack_several_frames() {
    let f = fixture("h264_aac");
    let (events, _) = run(Config::default(), f.data);
    let frames = parse_packets(f.packets).iter().filter(|p| p.stream == 1).count();
    let units = units_of(&events, 0x101).len();
    assert!(units > 0 && units < frames, "{units} units for {frames} frames");
}

#[test]
fn pcr_follows_the_first_stream() {
    let f = fixture("h264_aac");
    let (events, _) = run(Config::default(), f.data);
    let (program, _) = pmts(&events)[0];
    assert_eq!(program.pcr_pid, 0x100);
    let pcrs: Vec<u64> = events
        .iter()
        .filter_map(|e| if let Owned::Pcr { pid: 0x100, pcr, .. } = e { Some(*pcr) } else { None })
        .collect();
    assert!(pcrs.len() > 10);
    assert!(pcrs.windows(2).all(|w| w[0] < w[1]));
    let first_dts = units_of(&events, 0x100)[0].dts.or(units_of(&events, 0x100)[0].pts).unwrap();
    assert!(pcrs[0] / 300 <= first_dts);
}

#[test]
fn rollover_unwraps_to_the_oracle_timeline() {
    let f = fixture("wrap");
    let (events, _) = run(Config::default(), f.data);
    let oracle = parse_packets(f.packets);
    for (index, pid) in [(0usize, 0x100u16), (1, 0x101)] {
        let packets: Vec<&OraclePacket> = oracle.iter().filter(|p| p.stream == index).collect();
        let ours = units_of(&events, pid);
        assert_eq!(ours.len(), packets.len(), "one frame per unit in this fixture");
        let raw: Vec<u64> = ours.iter().map(|u| u.dts.or(u.pts).unwrap()).collect();
        assert!(raw.windows(2).any(|w| w[1] + TS_MODULUS / 2 < w[0]), "pid {pid:#x} wraps");

        let mut dts_line = Unwrapper::for_timestamps();
        let mut pts_line = Unwrapper::for_timestamps();
        let mut rollovers = 0;
        let mut origin = None;
        for (unit, p) in ours.iter().zip(&packets) {
            let d = dts_line.unwrap(unit.dts.or(unit.pts).unwrap());
            let t = pts_line.unwrap(unit.pts.unwrap());
            assert_ne!(d.step, Step::Discontinuity);
            assert_ne!(t.step, Step::Discontinuity);
            assert_ne!(d.step, Step::Backward, "DTS never steps back");
            rollovers += usize::from(d.step == Step::Rollover);
            let (od, op) = *origin.get_or_insert((d.ticks - p.dts, t.ticks - p.pts));
            assert_eq!(d.ticks - p.dts, od);
            assert_eq!(t.ticks - p.pts, op);
        }
        assert_eq!(rollovers, 1, "pid {pid:#x} rolls over once");
    }
}

#[test]
fn one_shared_timeline_for_a_whole_program() {
    let f = fixture("wrap");
    let (events, _) = run(Config::default(), f.data);
    let mut line = Unwrapper::for_timestamps();
    let mut low = i64::MAX;
    let mut high = i64::MIN;
    for e in &events {
        let values: [Option<u64>; 2] = match e {
            Owned::Pes(u) => [u.dts, u.pts],
            Owned::Pcr { pcr, .. } => [Some(pcr / 300), None],
            _ => [None, None],
        };
        for v in values.into_iter().flatten() {
            let r = line.unwrap(v);
            assert_ne!(r.step, Step::Discontinuity);
            low = low.min(r.ticks);
            high = high.max(r.ticks);
        }
    }
    assert!(high - low < 4 * 90_000, "span {}", high - low);
    assert!(low < TS_MODULUS as i64 && high > TS_MODULUS as i64);
}
