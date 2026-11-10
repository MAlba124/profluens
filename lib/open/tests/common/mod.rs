// Test fixtures allocate and read the disk freely, each test file uses part of the kit.
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::too_many_lines, dead_code)]

use pf_open::{Caps, OpenAt, OpenError, Opened, Want};
use profluens_core::fetch::{Fetch, Need, drive_slice_log};

pub const MS: u64 = 1_000_000;

/// A fixture of this crate or of another crate's tests, by its path from the workspace root.
pub fn fixture(path: &str) -> Vec<u8> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read(root.join(path)).unwrap()
}

pub fn own(name: &str) -> Vec<u8> {
    fixture(&format!("lib/open/tests/fixtures/{name}"))
}

/// `bytes` opened, with every read logged.
pub fn open_with(bytes: &[u8], want: Want, caps: Caps) -> (Result<Opened, OpenError>, Vec<Need>) {
    let mut log = Vec::new();
    let got = drive_slice_log(&mut OpenAt::new(bytes.len() as u64, want, caps), bytes, &mut log);
    (got, log)
}

pub fn open(bytes: &[u8]) -> Opened {
    open_with(bytes, Want::ALL, Caps::default()).0.unwrap()
}

/// What `m` reads of `bytes`, its result dropped.
pub fn reads<F: Fetch>(m: &mut F, bytes: &[u8], log: &mut Vec<Need>) -> Option<F::Out> {
    drive_slice_log(m, bytes, log).ok()
}

/// The OpusHead of a stream, RFC 7845 section 5.1, mapping family 0.
pub fn opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(channels);
    h.extend_from_slice(&pre_skip.to_le_bytes());
    h.extend_from_slice(&48_000u32.to_le_bytes());
    h.extend_from_slice(&0i16.to_le_bytes());
    h.push(0);
    h
}

/// Packet `i` of a synthetic stream, a 20 ms SILK TOC byte then `len - 1` bytes of filler.
pub fn opus_packet(serial: u32, i: usize, len: usize) -> Vec<u8> {
    let mut p = vec![0x08u8];
    p.extend((1..len.max(1)).map(|k| ((serial as usize * 131 + i * 31 + k * 7) % 251) as u8));
    p
}

pub fn opus_len(i: usize) -> usize {
    40 + (i * 37) % 160
}

/// An OpusTags packet of `comments`.
pub fn opus_tags(comments: &[&[u8]]) -> Vec<u8> {
    let mut b = b"OpusTags\x04\x00\x00\x00test".to_vec();
    b.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        b.extend_from_slice(&(c.len() as u32).to_le_bytes());
        b.extend_from_slice(c);
    }
    b
}

/// An Ogg Opus file of `packets` 20 ms packets, the granule starting at `start` samples, the
/// head and the tags on pages of their own (RFC 7845 section 3), then 200 ms pages.
pub fn opus_file(serial: u32, packets: usize, pre_skip: u16, start: u64) -> Vec<u8> {
    opus_file_tagged(serial, packets, pre_skip, start, &opus_tags(&[b"LANGUAGE=nor"]))
}

pub fn opus_file_tagged(
    serial: u32,
    packets: usize,
    pre_skip: u16,
    start: u64,
    tags: &[u8],
) -> Vec<u8> {
    let mut w = pf_ogg::OggWriter::new(serial);
    let mut out = Vec::new();
    for h in [&opus_head(2, pre_skip)[..], tags] {
        w.write_packet(&mut out, h, 0).unwrap();
        w.flush(&mut out);
    }
    for i in 0..packets {
        let p = opus_packet(serial, i, opus_len(i));
        w.write_packet(&mut out, &p, start + 960 * (i as u64 + 1)).unwrap();
        if i % 10 == 9 {
            w.flush(&mut out);
        }
    }
    w.finish(&mut out);
    out
}

/// An EBML element, the size always as an 8 byte vint.
pub fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.push(0x01);
    v.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
    v.extend_from_slice(payload);
    v
}

pub fn uel(id: &[u8], v: u64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

/// An avcC with one SPS and one PPS, 4 byte NAL lengths.
pub fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1);
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}

/// A block body, the track vint, the relative timestamp, the flags, then `rest`.
pub fn block(track: u8, rel: i16, flags: u8, rest: &[u8]) -> Vec<u8> {
    let mut v = vec![0x80 | track];
    v.extend_from_slice(&rel.to_be_bytes());
    v.push(flags);
    v.extend_from_slice(rest);
    v
}

pub fn avc_frame(nal: &[u8]) -> Vec<u8> {
    let mut v = (nal.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(nal);
    v
}

pub const SPS: [u8; 6] = [0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9];
pub const PPS: [u8; 4] = [0x68, 0xEB, 0xE3, 0xCB];
/// CodecDelay of both Opus tracks, 312 samples.
pub const MKV_DELAY_NS: u64 = 6_500_000;

pub const EBML_ID: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
pub const SEGMENT_UNKNOWN: [u8; 12] =
    [0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
pub const CUES_ID: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];
pub const TAGS_ID: [u8; 4] = [0x12, 0x54, 0xC3, 0x67];

/// A synthetic Matroska file and where its parts lie.
pub struct SynthMkv {
    pub bytes: Vec<u8>,
    /// Where each Cluster starts.
    pub clusters: Vec<u64>,
    pub cues_at: u64,
    pub tags_at: Option<u64>,
}

/// What [`mkv_with`] puts in a file.
#[derive(Clone, Copy)]
pub struct MkvParts {
    pub secs: usize,
    /// A Tags element after the Cues, with a BPS statistics tag for track 2.
    pub tags: bool,
    /// Bytes of filler in each video frame, the file grows with it.
    pub pad: usize,
    /// The Cues ahead of the Clusters, in the head.
    pub front: bool,
}

pub fn mkv_file(secs: usize) -> SynthMkv {
    mkv_with(MkvParts { secs, tags: false, pad: 0, front: false })
}

/// `secs` one second Clusters of 25 fps H.264, a key each second, and two Opus tracks in blocks
/// of five 20 ms frames, track 2 Xiph laced, track 3 EBML laced. A SeekHead points at the Cues
/// after the Clusters, one cue per Cluster for the video track, and at the Tags behind them.
pub fn mkv_with(parts: MkvParts) -> SynthMkv {
    let ebml = el(&EBML_ID, &el(&[0x42, 0x82], b"matroska"));
    let info = el(&[0x15, 0x49, 0xA9, 0x66], &uel(&[0x2A, 0xD7, 0xB1], 1_000_000));
    let vt = el(
        &[0xAE],
        &[
            uel(&[0xD7], 1),
            uel(&[0x73, 0xC5], 101),
            uel(&[0x83], 1),
            el(&[0x86], b"V_MPEG4/ISO/AVC"),
            el(&[0x63, 0xA2], &avcc(&SPS, &PPS)),
            uel(&[0x23, 0xE3, 0x83], 40_000_000),
            el(&[0xE0], &[uel(&[0xB0], 640), uel(&[0xBA], 360)].concat()),
        ]
        .concat(),
    );
    let at = |n: u64, lang: &[u8]| {
        el(
            &[0xAE],
            &[
                uel(&[0xD7], n),
                uel(&[0x73, 0xC5], 100 + n),
                uel(&[0x83], 2),
                el(&[0x86], b"A_OPUS"),
                el(&[0x63, 0xA2], &opus_head(2, 312)),
                uel(&[0x56, 0xAA], MKV_DELAY_NS),
                uel(&[0x56, 0xBB], 80_000_000),
                el(&[0x22, 0xB5, 0x9C], lang),
                el(&[0xE1], &uel(&[0x9F], 2)),
            ]
            .concat(),
        )
    };
    let tracks = el(&[0x16, 0x54, 0xAE, 0x6B], &[vt, at(2, b"eng"), at(3, b"nor")].concat());
    let mut bodies = Vec::new();
    for c in 0..parts.secs as i64 {
        let mut els: Vec<(i64, Vec<u8>)> = Vec::new();
        for k in 0..25i64 {
            let key = k == 0;
            let n = (c * 25 + k) as usize;
            let mut nal = vec![if key { 0x65 } else { 0x41 }];
            nal.extend((0..20 + n % 50 + parts.pad).map(|i| (n + i) as u8));
            let flags = if key { 0x80 } else { 0 };
            els.push((k * 40, el(&[0xA3], &block(1, (k * 40) as i16, flags, &avc_frame(&nal)))));
        }
        for b in 0..10i64 {
            for (ti, track) in [2u8, 3].into_iter().enumerate() {
                let frames: Vec<Vec<u8>> = (0..5)
                    .map(|f| {
                        let n = ((c * 10 + b) * 5 + f) as usize;
                        let mut p = vec![0x08];
                        p.extend((0..30 + (n * 7) % 60).map(|i| (n * 3 + i + ti * 101) as u8));
                        p
                    })
                    .collect();
                let lace = if track == 2 {
                    let mut l = vec![4u8];
                    l.extend(frames[..4].iter().map(|p| p.len() as u8));
                    l
                } else {
                    let mut l = vec![4u8];
                    let s: Vec<i64> = frames.iter().map(|p| p.len() as i64).collect();
                    l.extend((0x4000u16 | s[0] as u16).to_be_bytes());
                    for i in 1..4 {
                        l.extend((0x4000u16 | (s[i] - s[i - 1] + 8191) as u16).to_be_bytes());
                    }
                    l
                };
                let flags = 0x80 | if track == 2 { 0x02 } else { 0x06 };
                let body = [lace, frames.concat()].concat();
                els.push((b * 100, el(&[0xA3], &block(track, (b * 100) as i16, flags, &body))));
            }
        }
        els.sort_by_key(|e| e.0);
        let mut body = uel(&[0xE7], (c * 1000) as u64);
        for (_, e) in els {
            body.extend(e);
        }
        bodies.push(el(&[0x1F, 0x43, 0xB6, 0x75], &body));
    }
    let seek = |id: &[u8], pos: u64| {
        el(&[0x4D, 0xBB], &[el(&[0x53, 0xAB], id), uel(&[0x53, 0xAC], pos)].concat())
    };
    let seekhead = |cues: u64, tags: Option<u64>| {
        let mut b = seek(&CUES_ID, cues);
        if let Some(t) = tags {
            b.extend(seek(&TAGS_ID, t));
        }
        el(&[0x11, 0x4D, 0x9B, 0x74], &b)
    };
    let sh_len = seekhead(0, parts.tags.then_some(0)).len() as u64;
    let head_len = sh_len + info.len() as u64 + tracks.len() as u64;
    // a CuePoint's size holds whatever its values, every element is 8 octets of size
    let point = |c: u64, pos: u64| {
        let tp = el(&[0xB7], &[uel(&[0xF7], 1), uel(&[0xF1], pos)].concat());
        el(&[0xBB], &[uel(&[0xB3], c * 1000), tp].concat())
    };
    let cue_len =
        el(&CUES_ID, &(0..bodies.len() as u64).map(|c| point(c, 0)).collect::<Vec<_>>().concat())
            .len() as u64;
    let mut pos = head_len + if parts.front { cue_len } else { 0 };
    let seg_data = ebml.len() as u64 + 12;
    let mut clusters = Vec::new();
    let mut points = Vec::new();
    for (c, b) in bodies.iter().enumerate() {
        clusters.push(seg_data + pos);
        points.push(point(c as u64, pos));
        pos += b.len() as u64;
    }
    let cues = el(&CUES_ID, &points.concat());
    let cues_pos = if parts.front { head_len } else { pos };
    let tags_pos = parts.tags.then_some(pos + if parts.front { 0 } else { cues.len() as u64 });
    let tags = tags_el(&[(102, "BPS", "96000"), (101, "BPS", "2500000")]);
    let mut bytes = ebml;
    bytes.extend(SEGMENT_UNKNOWN);
    bytes.extend(seekhead(cues_pos, tags_pos));
    bytes.extend(info);
    bytes.extend(tracks);
    if parts.front {
        bytes.extend(&cues);
    }
    bodies.into_iter().for_each(|b| bytes.extend(b));
    if !parts.front {
        bytes.extend(cues);
    }
    if parts.tags {
        bytes.extend(tags);
    }
    SynthMkv {
        bytes,
        clusters,
        cues_at: seg_data + cues_pos,
        tags_at: tags_pos.map(|t| seg_data + t),
    }
}

/// A Tags element of one SimpleTag per track uid.
pub fn tags_el(simple: &[(u64, &str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (uid, name, value) in simple {
        let targets = el(&[0x63, 0xC0], &uel(&[0x63, 0xC5], *uid));
        let st = el(
            &[0x67, 0xC8],
            &[el(&[0x45, 0xA3], name.as_bytes()), el(&[0x44, 0x87], value.as_bytes())].concat(),
        );
        body.extend(el(&[0x73, 0x73], &[targets, st].concat()));
    }
    el(&TAGS_ID, &body)
}

/// `pat` replaced by `with` at its last match in `b`.
pub fn patch_last(b: &mut [u8], pat: &[u8], with: &[u8]) {
    let at = b.windows(pat.len()).rposition(|w| w == pat).unwrap();
    b[at..at + with.len()].copy_from_slice(with);
}

/// A sequence header of 720 by 576 at 25 frames, 16:9, its extension and a GOP header.
pub const SEQ: [u8; 28] = [
    0, 0, 1, 0xB3, 0x2D, 0x02, 0x40, 0x33, 0xFF, 0xFF, 0xE0, 0x18, 0, 0, 1, 0xB5, 0x14, 0x8A, 0, 1,
    0, 0, 0, 0, 1, 0xB8, 0, 8,
];

/// The five bytes of a PES stamp behind its `prefix` bits, H.222.0 2.4.3.7.
pub fn stamp(prefix: u8, t: u64) -> [u8; 5] {
    [
        prefix << 4 | ((t >> 29) & 0x0E) as u8 | 1,
        (t >> 22) as u8,
        ((t >> 14) & 0xFE) as u8 | 1,
        (t >> 7) as u8,
        ((t << 1) & 0xFE) as u8 | 1,
    ]
}

/// A PES packet of `id` stamped `pts`, its length unbounded for video.
pub fn pes(id: u8, pts: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let mut h = vec![0, 0, 1, id, 0, 0, 0x80];
    match pts {
        Some(t) => {
            h.extend_from_slice(&[0x80, 5]);
            h.extend_from_slice(&stamp(2, t));
        }
        None => h.extend_from_slice(&[0, 0]),
    }
    let len = h.len() - 6 + payload.len();
    if id & 0xF0 != 0xE0 || len <= 0xFFFF {
        h[4..6].copy_from_slice(&(len.min(0xFFFF) as u16).to_be_bytes());
    }
    h.extend_from_slice(payload);
    h
}

/// Transport packets of `pid` carrying `unit`, the first with PUSI, the last stuffed.
pub fn packets(pid: u16, cc: &mut u8, unit: &[u8], out: &mut Vec<u8>) {
    for (i, chunk) in unit.chunks(184).enumerate() {
        let mut p = vec![0x47, (pid >> 8) as u8 | if i == 0 { 0x40 } else { 0 }, pid as u8];
        let stuff = 184 - chunk.len();
        match stuff {
            0 => p.push(0x10 | *cc),
            1 => p.extend_from_slice(&[0x30 | *cc, 0]),
            n => {
                p.extend_from_slice(&[0x30 | *cc, (n - 1) as u8, 0]);
                p.resize(p.len() + n - 2, 0xFF);
            }
        }
        p.extend_from_slice(chunk);
        *cc = (*cc + 1) & 0xF;
        out.extend_from_slice(&p);
    }
}

/// A PSI section with its CRC, behind a zero pointer field.
pub fn section(table: u8, id: u16, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4;
    let mut s =
        vec![table, 0xB0 | (len >> 8) as u8, len as u8, (id >> 8) as u8, id as u8, 0xC1, 0, 0];
    s.extend_from_slice(body);
    let crc = pf_mpegts::crc32(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    [&[0u8][..], &s].concat()
}

/// A PAT naming program 1 at `pmt` and its PMT of `streams`, type and PID.
pub fn tables(pmt: u16, streams: &[(u8, u16)], out: &mut Vec<u8>, cc: &mut [u8; 2]) {
    packets(0, &mut cc[0], &section(0, 1, &[0, 1, 0xE0 | (pmt >> 8) as u8, pmt as u8]), out);
    let mut body = vec![0xE1, 0x00, 0xF0, 0];
    for &(kind, pid) in streams {
        body.extend_from_slice(&[kind, 0xE0 | (pid >> 8) as u8, pid as u8, 0xF0, 0]);
    }
    packets(pmt, &mut cc[1], &section(2, 1, &body), out);
}

/// A transport stream of MPEG-2 video at 25 frames and AC-3 at 48 kHz, `secs` long from
/// `start` ticks, the tables every 10 frames.
pub fn ts_file(secs: u64, start: u64) -> Vec<u8> {
    let (mut out, mut cc, mut vcc, mut acc) = (Vec::new(), [0u8; 2], 0u8, 0u8);
    let ac3 = pf_bare::fixture::ac3_frame(0, 192, false, 2);
    let mut audio = 0u64;
    for f in 0..secs * 25 {
        if f % 10 == 0 {
            tables(0x1000, &[(0x02, 0x100), (0x81, 0x101)], &mut out, &mut cc);
        }
        let t = start + f * 3600;
        let mut pic = if f % 12 == 0 { SEQ.to_vec() } else { Vec::new() };
        pic.extend_from_slice(&[0, 0, 1, 0, 0, if f % 12 == 0 { 0x0F } else { 0x17 }, 0xFF]);
        pic.resize(pic.len() + 900, 0x5A);
        packets(0x100, &mut vcc, &pes(0xE0, Some(t), &pic), &mut out);
        while audio * 2880 <= f * 3600 {
            let p = pes(0xBD, Some(start + audio * 2880), &ac3);
            packets(0x101, &mut acc, &p, &mut out);
            audio += 1;
        }
    }
    out
}

/// An MPEG-2 pack header of the clock `s`, extension 0, mux rate, no stuffing.
pub fn pack(out: &mut Vec<u8>, s: u64) {
    out.extend_from_slice(&[0, 0, 1, 0xBA]);
    out.push(0x44 | ((s >> 27) & 0x38) as u8 | ((s >> 28) & 3) as u8);
    out.push((s >> 20) as u8);
    out.push(((s >> 12) & 0xF8) as u8 | 4 | ((s >> 13) & 3) as u8);
    out.push((s >> 5) as u8);
    out.extend_from_slice(&[((s << 3) & 0xF8) as u8 | 4, 1, 0x01, 0x89, 0xC3, 0xF8]);
}

/// A VobSub `.sub` of `n` subtitles a second apart, each a 2048 byte pack of private stream 1
/// carrying subpicture substream `0x20 + n % 2`, a padding stream filling it.
pub fn vobsub_file(n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for k in 0..n {
        let at = out.len();
        let t = 90_000 + k * 90_000;
        pack(&mut out, t);
        // the SPU's size, its control sequence at 4, start display, the end
        let spu = [0, 10, 0, 4, 0, 0, 0, 4, 0x01, 0xFF];
        let mut payload = vec![0x20 + (k % 2) as u8];
        payload.extend_from_slice(&spu);
        out.extend_from_slice(&pes(0xBD, Some(t), &payload));
        let pad = 2048 - (out.len() - at) - 6;
        out.extend_from_slice(&[0, 0, 1, 0xBE, (pad >> 8) as u8, pad as u8]);
        out.resize(at + 2048, 0xFF);
    }
    out
}

/// A program stream of MPEG-2 video and AC-3 in private stream 1, packs of 2048 bytes.
pub fn ps_file(secs: u64, scr: bool, pts: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let ac3 = pf_bare::fixture::ac3_frame(0, 192, false, 7);
    let pack = |out: &mut Vec<u8>, t: u64| pack(out, if scr { t } else { 0 });
    let mut audio = 0u64;
    for f in 0..secs * 25 {
        let t = 90_000 + f * 3600;
        pack(&mut out, t);
        let mut pic = if f % 12 == 0 { SEQ.to_vec() } else { Vec::new() };
        pic.extend_from_slice(&[0, 0, 1, 0, 0, 0x0F, 0xFF]);
        pic.resize(pic.len() + 600, 0x5A);
        out.extend_from_slice(&pes(0xE0, pts.then_some(t), &pic));
        while audio * 2880 <= f * 3600 {
            pack(&mut out, t);
            let mut payload = vec![0x80, 1, 0, 1];
            payload.extend_from_slice(&ac3);
            out.extend_from_slice(&pes(0xBD, pts.then_some(90_000 + audio * 2880), &payload));
            audio += 1;
        }
    }
    out.extend_from_slice(&[0, 0, 1, 0xB9]);
    out
}
