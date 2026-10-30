//! The committed fixtures, their ffprobe tables and the demuxer and framers run over them.

#![allow(clippy::disallowed_methods, dead_code)] // test fixtures

pub mod pipe;

use pf_ps::es::{Au, Framer, Spu, Stamp};
use pf_ps::{Codec, Demux, Event, scan_head, streams};

pub fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn table(name: &str, suffix: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}.{suffix}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[derive(Clone, Debug)]
pub struct Probed {
    pub index: usize,
    pub id: u16,
    pub codec: String,
}

/// `id` as ffprobe gives it, the stream id, or 0xBD sub-stream ids alone.
pub fn streams(name: &str) -> Vec<Probed> {
    table(name, "streams.txt")
        .lines()
        .filter(|l| l.starts_with("stream|"))
        .map(|line| {
            let field =
                |key: &str| line.split('|').find_map(|f| f.strip_prefix(key)?.strip_prefix('='));
            let id = field("id").expect("id").trim_start_matches("0x");
            Probed {
                index: field("index").expect("index").parse().expect("index"),
                id: u16::from_str_radix(id, 16).expect("hex id"),
                codec: field("codec_name").expect("codec").to_string(),
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub struct Packet {
    pub stream: usize,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub size: usize,
    pub pos: Option<u64>,
    pub key: bool,
}

pub fn packets(name: &str) -> Vec<Packet> {
    table(name, "packets.csv")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            Packet {
                stream: f[0].parse().expect("stream"),
                pts: f[1].parse().ok(),
                dts: f[2].parse().ok(),
                size: f[3].parse().expect("size"),
                pos: f[4].parse().ok(),
                key: f[5].starts_with('K'),
            }
        })
        .collect()
}

/// The demuxer key of an ffprobe stream id.
pub fn key(id: u16) -> u16 {
    match id {
        0x100..=0x1FF => id << 8,
        sub => 0xBD00 | sub,
    }
}

pub fn raw33(v: i64) -> u64 {
    v.rem_euclid(1 << 33) as u64
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Got {
    pub bytes: Vec<u8>,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub key: bool,
    pub offset: u64,
}

fn got(au: Au<'_>) -> Got {
    Got {
        bytes: au.bytes.to_vec(),
        pts: au.stamp.map(|s| s.pts),
        dts: au.stamp.and_then(|s| s.dts),
        key: au.key,
        offset: au.offset,
    }
}

/// The units of the stream of `key` as the element cuts them, fed `chunk` bytes at a time.
pub fn units(data: &[u8], key: u16, chunk: usize) -> (Codec, Vec<Got>) {
    let head = scan_head(data);
    let psm_type = head.psm[usize::from(key >> 8)];
    let seen = head.seen.iter().find(|s| s.key == key).expect("the stream is in the head");
    let codec = streams::codec_of(key, psm_type, &seen.sniff, head.mpeg2).expect("read here");
    let mut framer = streams::video_of(codec).map(|v| Framer::new(v, 0, 8 << 20));
    let mut spu = (codec == Codec::Subpicture).then(Spu::new);
    let mut out = Vec::new();
    let mut d = Demux::new();
    let mut sink = |e: Event<'_>| {
        let Event::Unit(u) = e else { return };
        if u.key != key {
            return;
        }
        let stamp = u.pts.map(|pts| Stamp { pts, dts: u.dts });
        let mut emit = |au: Au<'_>| out.push(got(au));
        match (&mut framer, &mut spu) {
            (Some(f), _) => f.push(u.payload, stamp, u.offset, &mut emit),
            (_, Some(s)) => s.push(u.payload, stamp, u.offset, &mut emit),
            _ => emit(Au { bytes: u.payload, stamp, key: true, offset: u.offset }),
        }
    };
    for c in data.chunks(chunk) {
        d.push(c, &mut sink);
    }
    d.flush(&mut sink);
    if let Some(f) = &mut framer {
        f.flush(&mut |au| out.push(got(au)));
    }
    (codec, out)
}
