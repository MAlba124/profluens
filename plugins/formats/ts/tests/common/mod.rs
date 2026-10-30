//! Shared test support, owned events, a seeded generator, the fixtures and a small TS writer.

#![allow(dead_code, clippy::disallowed_methods)] // test support, each binary uses part of it

use pf_mpegts::{
    Anomaly, AnomalyKind, Config, Demux, Event, PacketSize, Program, Stats, StreamInfo, crc32,
    framed_len,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owned {
    Pat { transport_stream_id: u16, version: u8, programs: Vec<Program> },
    Pmt { program: Program, streams: Vec<StreamInfo> },
    Pcr { pid: u16, pcr: u64, discontinuity: bool, offset: u64, arrival: Option<u32> },
    Pes(Unit),
    Anomaly(Anomaly),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub pid: u16,
    pub stream_id: u8,
    pub flags: u8,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub offset: u64,
    pub arrival: Option<u32>,
    pub payload: Vec<u8>,
}

pub fn own(event: Event<'_>) -> Owned {
    match event {
        Event::Pat { transport_stream_id, version, programs } => {
            Owned::Pat { transport_stream_id, version, programs: programs.to_vec() }
        }
        Event::Pmt { program, streams } => {
            Owned::Pmt { program: *program, streams: streams.to_vec() }
        }
        Event::Pcr { pid, pcr, discontinuity, offset, arrival } => {
            Owned::Pcr { pid, pcr, discontinuity, offset, arrival }
        }
        Event::Pes(u) => Owned::Pes(Unit {
            pid: u.pid,
            stream_id: u.stream_id,
            flags: u.flags,
            pts: u.pts,
            dts: u.dts,
            offset: u.offset,
            arrival: u.arrival,
            payload: u.payload.to_vec(),
        }),
        Event::Anomaly(a) => Owned::Anomaly(a),
    }
}

pub fn run_with(
    demux: &mut Demux,
    data: &[u8],
    mut next_size: impl FnMut() -> usize,
    events: &mut Vec<Owned>,
) {
    let mut rest = data;
    while !rest.is_empty() {
        let n = next_size().clamp(1, rest.len());
        demux.push(&rest[..n], &mut |e| events.push(own(e)));
        rest = &rest[n..];
    }
    demux.flush(&mut |e| events.push(own(e)));
}

pub fn run(cfg: Config, data: &[u8]) -> (Vec<Owned>, Stats) {
    let mut demux = Demux::new(cfg).expect("valid config");
    let mut events = Vec::new();
    run_with(&mut demux, data, || usize::MAX, &mut events);
    (events, *demux.stats())
}

pub fn units(events: &[Owned]) -> Vec<&Unit> {
    events.iter().filter_map(|e| if let Owned::Pes(u) = e { Some(u) } else { None }).collect()
}

pub fn units_of(events: &[Owned], pid: u16) -> Vec<&Unit> {
    units(events).into_iter().filter(|u| u.pid == pid).collect()
}

pub fn anomalies(events: &[Owned]) -> Vec<Anomaly> {
    events.iter().filter_map(|e| if let Owned::Anomaly(a) = e { Some(*a) } else { None }).collect()
}

pub fn anomaly_kinds(events: &[Owned]) -> Vec<AnomalyKind> {
    anomalies(events).iter().map(|a| a.kind).collect()
}

pub fn pmts(events: &[Owned]) -> Vec<(&Program, &[StreamInfo])> {
    events
        .iter()
        .filter_map(|e| {
            if let Owned::Pmt { program, streams } = e {
                Some((program, streams.as_slice()))
            } else {
                None
            }
        })
        .collect()
}

pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

pub struct Fixture {
    pub name: &'static str,
    pub data: &'static [u8],
    pub packets: &'static str,
    pub streams: &'static str,
}

macro_rules! fixture {
    ($name:literal) => {
        Fixture {
            name: $name,
            data: include_bytes!(concat!("../fixtures/", $name, ".ts")),
            packets: include_str!(concat!("../fixtures/", $name, ".packets.csv")),
            streams: include_str!(concat!("../fixtures/", $name, ".streams.txt")),
        }
    };
}

pub const FIXTURES: &[Fixture] = &[
    fixture!("h264_aac"),
    fixture!("hevc_ac3"),
    fixture!("hevc_eac3_dvb"),
    fixture!("bframes"),
    fixture!("audio_mp2"),
    fixture!("aac_latm"),
    fixture!("mpeg2"),
    fixture!("wrap"),
];

pub fn fixture(name: &str) -> &'static Fixture {
    FIXTURES.iter().find(|f| f.name == name).expect("known fixture")
}

pub const AF_DISCONTINUITY: u8 = 0x80;
pub const AF_RANDOM_ACCESS: u8 = 0x40;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Adaptation,
    Payload,
}

pub fn build_packet(
    pid: u16,
    unit_start: bool,
    cc: u8,
    af_flags: u8,
    pcr: Option<u64>,
    payload: &[u8],
    fill: Fill,
) -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = (u8::from(unit_start) << 6) | ((pid >> 8) as u8 & 0x1F);
    p[2] = pid as u8;
    let content = usize::from(af_flags != 0 || pcr.is_some()) + if pcr.is_some() { 6 } else { 0 };
    let want_af = content > 0 || (payload.len() < 184 && fill == Fill::Adaptation);
    if !want_af {
        assert!(payload.len() <= 184);
        p[3] = 0x10 | (cc & 0x0F);
        p[4..4 + payload.len()].copy_from_slice(payload);
        return p;
    }
    let af_total = if fill == Fill::Adaptation { 184 - payload.len() } else { 1 + content };
    assert!(af_total > content && af_total + payload.len() <= 184, "payload does not fit");
    let has_payload = !payload.is_empty();
    p[3] = 0x20 | (u8::from(has_payload) << 4) | (cc & 0x0F);
    p[4] = (af_total - 1) as u8;
    if af_total > 1 {
        p[5] = af_flags | if pcr.is_some() { 0x10 } else { 0 };
    }
    if let Some(pcr) = pcr {
        let (base, ext) = (pcr / 300, pcr % 300);
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = (((base & 1) as u8) << 7) | 0x7E | (ext >> 8) as u8;
        p[11] = ext as u8;
    }
    let start = 4 + af_total;
    p[start..start + payload.len()].copy_from_slice(payload);
    p
}

fn stamp(prefix: u8, v: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((v >> 30) as u8 & 0x07) << 1) | 1,
        (v >> 22) as u8,
        (((v >> 15) as u8) << 1) | 1,
        (v >> 7) as u8,
        ((v as u8) << 1) | 1,
    ]
}

pub fn pes_bytes(
    stream_id: u8,
    pts: Option<u64>,
    dts: Option<u64>,
    payload: &[u8],
    bounded: bool,
) -> Vec<u8> {
    let mut optional = Vec::new();
    let flags = match (pts, dts) {
        (Some(p), Some(d)) => {
            optional.extend_from_slice(&stamp(0b0011, p));
            optional.extend_from_slice(&stamp(0b0001, d));
            0xC0
        }
        (Some(p), None) => {
            optional.extend_from_slice(&stamp(0b0010, p));
            0x80
        }
        _ => 0x00,
    };
    let length = if bounded { 3 + optional.len() + payload.len() } else { 0 };
    assert!(length <= 0xFFFF);
    let mut v = vec![
        0,
        0,
        1,
        stream_id,
        (length >> 8) as u8,
        length as u8,
        0x80,
        flags,
        optional.len() as u8,
    ];
    v.extend_from_slice(&optional);
    v.extend_from_slice(payload);
    v
}

fn finish_section(mut v: Vec<u8>) -> Vec<u8> {
    let length = v.len() - 3 + 4;
    v[1] = 0xB0 | (length >> 8) as u8;
    v[2] = length as u8;
    let crc = crc32(&v);
    v.extend_from_slice(&crc.to_be_bytes());
    v
}

pub fn pat_section(transport_stream_id: u16, version: u8, rows: &[(u16, u16)]) -> Vec<u8> {
    let mut v = vec![0x00, 0, 0, (transport_stream_id >> 8) as u8, transport_stream_id as u8];
    v.extend_from_slice(&[0xC1 | (version << 1), 0, 0]);
    for &(number, pid) in rows {
        v.extend_from_slice(&[
            (number >> 8) as u8,
            number as u8,
            0xE0 | (pid >> 8) as u8,
            pid as u8,
        ]);
    }
    finish_section(v)
}

pub type EsRow<'a> = (u8, u16, &'a [u8]);

pub fn pmt_section(
    program: u16,
    version: u8,
    pcr_pid: u16,
    program_info: &[u8],
    rows: &[EsRow<'_>],
) -> Vec<u8> {
    let mut v = vec![0x02, 0, 0, (program >> 8) as u8, program as u8, 0xC1 | (version << 1), 0, 0];
    v.extend_from_slice(&[0xE0 | (pcr_pid >> 8) as u8, pcr_pid as u8]);
    v.extend_from_slice(&[0xF0 | (program_info.len() >> 8) as u8, program_info.len() as u8]);
    v.extend_from_slice(program_info);
    for &(stream_type, pid, info) in rows {
        v.extend_from_slice(&[stream_type, 0xE0 | (pid >> 8) as u8, pid as u8]);
        v.extend_from_slice(&[0xF0 | (info.len() >> 8) as u8, info.len() as u8]);
        v.extend_from_slice(info);
    }
    finish_section(v)
}

pub struct Ts {
    pub out: Vec<u8>,
    cc: Vec<u8>,
}

impl Default for Ts {
    fn default() -> Self {
        Ts { out: Vec::new(), cc: vec![0; 8192] }
    }
}

impl Ts {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_cc(&mut self, pid: u16, has_payload: bool) -> u8 {
        let c = &mut self.cc[usize::from(pid)];
        let now = *c;
        if has_payload {
            *c = (*c + 1) & 0x0F;
        }
        now
    }

    pub fn peek_cc(&self, pid: u16) -> u8 {
        self.cc[usize::from(pid)]
    }

    pub fn skip_cc(&mut self, pid: u16, n: u8) {
        let c = &mut self.cc[usize::from(pid)];
        *c = (*c + n) & 0x0F;
    }

    pub fn packet(
        &mut self,
        pid: u16,
        unit_start: bool,
        af_flags: u8,
        pcr: Option<u64>,
        payload: &[u8],
        fill: Fill,
    ) -> usize {
        let cc = self.next_cc(pid, !payload.is_empty());
        let at = self.out.len();
        self.out
            .extend_from_slice(&build_packet(pid, unit_start, cc, af_flags, pcr, payload, fill));
        at
    }

    pub fn section(&mut self, pid: u16, section: &[u8]) {
        let mut first = vec![0u8];
        let n = section.len().min(183);
        first.extend_from_slice(&section[..n]);
        self.packet(pid, true, 0, None, &first, Fill::Payload);
        for chunk in section[n..].chunks(184) {
            self.packet(pid, false, 0, None, chunk, Fill::Payload);
        }
    }

    pub fn pes(&mut self, pid: u16, af_flags: u8, pes: &[u8]) -> usize {
        let room = if af_flags != 0 { 182 } else { 184 };
        let n = pes.len().min(room);
        let at = self.packet(pid, true, af_flags, None, &pes[..n], Fill::Adaptation);
        for chunk in pes[n..].chunks(184) {
            self.packet(pid, false, 0, None, chunk, Fill::Adaptation);
        }
        at
    }

    pub fn null(&mut self) {
        let mut p = [0xFFu8; 188];
        p[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
        self.out.extend_from_slice(&p);
    }

    pub fn tail(&mut self) {
        for _ in 0..8 {
            self.null();
        }
    }
}

pub fn start(rows: &[EsRow<'_>], pcr_pid: u16) -> Ts {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, 0x1000)]));
    ts.section(0x1000, &pmt_section(1, 0, pcr_pid, &[], rows));
    ts
}

pub const SIZES: [PacketSize; 3] = [PacketSize::Ts188, PacketSize::M2ts192, PacketSize::Fec204];

/// Bits 23 to 16 read 0x47 for packets 30 to 73, the case the sync search must see through.
pub fn arrival_of_packet(index: usize) -> u32 {
    (0x0047_0000 - 30 * 1500 + index as u32 * 1500) & 0x3FFF_FFFF
}

pub fn reframe(data: &[u8], size: PacketSize) -> Vec<u8> {
    let (packets, rest) = data.as_chunks::<188>();
    assert!(rest.is_empty(), "whole packets");
    let mut out = Vec::with_capacity(packets.len() * framed_len(size));
    for (index, packet) in packets.iter().enumerate() {
        if size == PacketSize::M2ts192 {
            out.extend_from_slice(&arrival_of_packet(index).to_be_bytes());
        }
        out.extend_from_slice(packet);
        if size == PacketSize::Fec204 {
            let mut rng = Rng::new(0xFEC0 + index as u64);
            out.extend_from_slice(&rng.next().to_le_bytes());
            out.extend_from_slice(&rng.next().to_le_bytes());
        }
    }
    out
}

pub fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add((i as u8).wrapping_mul(7))).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fnv(pub u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xCBF2_9CE4_8422_2325)
    }
}

impl Fnv {
    pub fn bytes(&mut self, data: &[u8]) {
        for &b in data {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }

    pub fn num(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
}

pub fn hash_of(data: &[u8]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64 ^ data.len() as u64;
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
    }
    for &b in rest {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitMark {
    pub pid: u16,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub len: usize,
    pub hash: u64,
    pub offset: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Digest {
    pub all: Fnv,
    pub events: u64,
    pub units: Vec<UnitMark>,
    pub skipped: u64,
    pub largest_unit: usize,
    pub most_programs: usize,
    pub most_streams: usize,
}

impl Digest {
    pub fn take(&mut self, event: Event<'_>) {
        self.events += 1;
        let h = &mut self.all;
        match event {
            Event::Pat { transport_stream_id, version, programs } => {
                h.num(1);
                h.num(u64::from(transport_stream_id));
                h.num(u64::from(version));
                for p in programs {
                    for v in
                        [p.number, p.pmt_pid, p.pcr_pid, u16::from(p.version), u16::from(p.mapped)]
                    {
                        h.num(u64::from(v));
                    }
                    h.num(u64::from(p.registration));
                }
                self.most_programs = self.most_programs.max(programs.len());
            }
            Event::Pmt { program, streams } => {
                h.num(2);
                h.num(u64::from(program.number));
                h.num(u64::from(program.version));
                h.num(u64::from(program.pcr_pid));
                for s in streams {
                    h.num(u64::from(s.pid));
                    h.num(u64::from(s.stream_type));
                    h.num(s.codec as u64);
                    h.num(s.encryption as u64);
                    h.num(u64::from(s.registration));
                    h.bytes(&s.language);
                    h.num(u64::from(s.flags));
                }
                self.most_streams = self.most_streams.max(streams.len());
            }
            Event::Pcr { pid, pcr, discontinuity, offset, arrival } => {
                h.num(3);
                h.num(u64::from(pid));
                h.num(pcr);
                h.num(u64::from(discontinuity));
                h.num(offset);
                h.num(arrival.map_or(u64::MAX, u64::from));
            }
            Event::Pes(u) => {
                let mark = UnitMark {
                    pid: u.pid,
                    pts: u.pts,
                    dts: u.dts,
                    len: u.payload.len(),
                    hash: hash_of(u.payload),
                    offset: u.offset,
                };
                h.num(4);
                h.num(u64::from(u.pid));
                h.num(u64::from(u.stream_id));
                h.num(u64::from(u.flags));
                h.num(u.pts.unwrap_or(u64::MAX));
                h.num(u.dts.unwrap_or(u64::MAX));
                h.num(u.offset);
                h.num(u.arrival.map_or(u64::MAX, u64::from));
                h.num(mark.hash);
                self.largest_unit = self.largest_unit.max(mark.len);
                self.units.push(mark);
            }
            Event::Anomaly(a) => {
                h.num(5);
                h.num(a.kind as u64);
                h.num(u64::from(a.pid));
                h.num(u64::from(a.detail));
                h.num(a.offset);
                if a.kind == AnomalyKind::SyncSkipped {
                    self.skipped += u64::from(a.detail);
                }
            }
        }
    }
}

pub fn digest_with(demux: &mut Demux, data: &[u8], mut next_size: impl FnMut() -> usize) -> Digest {
    let mut d = Digest::default();
    let mut rest = data;
    while !rest.is_empty() {
        let n = next_size().clamp(1, rest.len());
        demux.push(&rest[..n], &mut |e| d.take(e));
        rest = &rest[n..];
    }
    demux.flush(&mut |e| d.take(e));
    d
}

pub fn piece(rng: &mut Rng) -> usize {
    match rng.below(4) {
        0 => 1 + rng.below(8),
        1 => 180 + rng.below(17),
        2 => 1 + rng.below(700),
        _ => 1 + rng.below(5000),
    }
}

pub fn random_bytes(rng: &mut Rng, n: usize) -> Vec<u8> {
    (0..n).map(|_| rng.next() as u8).collect()
}

pub fn mangle(rng: &mut Rng, region: &mut Vec<u8>, phase: usize, size: usize) {
    for _ in 0..1 + rng.below(3) {
        let len = region.len();
        if len == 0 {
            return;
        }
        let first = (size - phase % size) % size;
        let packets = len.saturating_sub(first) / size;
        match rng.below(8) {
            0 => {
                for _ in 0..1 + rng.below(8) {
                    let at = rng.below(len);
                    region[at] ^= 1 << rng.below(8);
                }
            }
            1 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(400));
                region.drain(at..at + n);
            }
            2 => {
                let at = rng.below(len + 1);
                let n = 1 + rng.below(600);
                let junk = random_bytes(rng, n);
                region.splice(at..at, junk);
            }
            3 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(600));
                let junk = random_bytes(rng, n);
                region[at..at + n].copy_from_slice(&junk);
            }
            4 if packets >= 2 => {
                for _ in 0..1 + rng.below(4) {
                    let i = first + size * rng.below(packets);
                    let j = first + size * rng.below(packets);
                    for k in 0..size {
                        region.swap(i + k, j + k);
                    }
                }
            }
            5 if packets >= 1 => {
                let i = first + size * rng.below(packets);
                let copy = region[i..i + size].to_vec();
                region.splice(i..i, copy);
            }
            6 => {
                let at = rng.below(len);
                let n = 1 + rng.below((len - at).min(600));
                region[at..at + n].fill(0x47);
            }
            _ if packets >= 1 => {
                let i = first + size * rng.below(packets);
                region.drain(i..i + size);
            }
            _ => {}
        }
    }
}

pub fn corrupt(rng: &mut Rng, clean: &[u8], a: usize, b: usize) -> Vec<u8> {
    corrupt_framed(rng, clean, a, b, PacketSize::Ts188)
}

pub fn corrupt_framed(
    rng: &mut Rng,
    clean: &[u8],
    a: usize,
    b: usize,
    size: PacketSize,
) -> Vec<u8> {
    let mut region = clean[a..b].to_vec();
    mangle(rng, &mut region, a, framed_len(size));
    let mut out = Vec::with_capacity(clean.len() + 2048);
    out.extend_from_slice(&clean[..a]);
    out.extend_from_slice(&region);
    out.extend_from_slice(&clean[b..]);
    out
}
