//! Untrusted-input gate. Seeded corruptions of well-formed MP4 must not panic, hang or grow heap
//! or output past a bound. `pf_sim::corrupt` documents the seed and replay variables.

// Test code builds its inputs on the heap.
#![allow(clippy::disallowed_methods)]

use pf_mp4::boxes::{self, boxtype};
use pf_mp4::codec::parse_stsd;
use pf_mp4::frag;
use pf_mp4::reader::{MAX_SAMPLES, MAX_TRACKS};
use pf_mp4::{
    Limits, MoovExtent, Mp4Demux, Mp4Error, Mp4Reader, locate_moov, parse_moov_tags,
    props_from_moov,
};
use pf_sim::corrupt::harness::{feed_bytes, input_buffer, pool_allowance};
use pf_sim::corrupt::{
    Case, Chunking, EXTREMES, Site, Tracking, Violation, check_heap, check_output, check_steps,
    gate, heap_growth, heap_mark, mutate, noise, prefix_then_garbage, sweep_gate,
};
use pf_sim::prng::Prng;
use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

#[global_allocator]
static HEAP: Tracking = Tracking;

static TINY_H264: &[u8] = include_bytes!("fixtures/tiny_h264.mp4");
static BFRAMES_H264: &[u8] = include_bytes!("fixtures/bframes_h264.mp4");
static TINY_AV: &[u8] = include_bytes!("fixtures/tiny_av.mp4");
static FRAG_AV: &[u8] = include_bytes!("fixtures/frag/frag_av.mp4");
static FRAG_SIDX: &[u8] = include_bytes!("fixtures/frag/frag_sidx.mp4");
static FRAG_HYBRID: &[u8] = include_bytes!("fixtures/frag/frag_hybrid.mp4");
static DASH_INIT: &[u8] = include_bytes!("fixtures/frag/dash_init-stream0.m4s");
static DASH_FIRST: &[u8] = include_bytes!("fixtures/frag/dash_stream0-00001.m4s");
static DASH_OTHER_INIT: &[u8] = include_bytes!("fixtures/frag/dash_init-stream1.m4s");
static DASH_OTHER: &[u8] = include_bytes!("fixtures/frag/dash_stream1-00002.m4s");

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![version, 0, 0, 0];
    payload.extend_from_slice(body);
    boxed(kind, &payload)
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn handmade_base() -> Vec<u8> {
    let ftyp = boxed(b"ftyp", b"isom\0\0\0\0isom");

    let tkhd = |id: u32| {
        let mut body = vec![0u8; 8];
        body.extend(be32(&[id, 0, 0]));
        body.extend([0u8; 52]);
        body.extend(be32(&[64 << 16, 48 << 16]));
        full(b"tkhd", 0, &body)
    };
    let hdlr = |kind: &[u8; 4]| {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(kind);
        body.extend([0u8; 13]);
        full(b"hdlr", 0, &body)
    };
    let mut mdhd_v1 = vec![0u8; 16];
    mdhd_v1.extend(be32(&[15]));
    mdhd_v1.extend(90u64.to_be_bytes());
    mdhd_v1.extend([0u8; 4]);
    let mut elst_v1 = be32(&[2]);
    for (duration, media_time) in [(10u64, -1i64), (600, 0)] {
        elst_v1.extend(duration.to_be_bytes());
        elst_v1.extend(media_time.to_be_bytes());
        elst_v1.extend(be32(&[0x0001_0000]));
    }
    let mut vp09 = vec![0u8; 78];
    vp09[7] = 1;
    vp09[24..28].copy_from_slice(&[0, 64, 0, 48]);
    let video_offset_at;
    let video_stbl = {
        let mut stsd = be32(&[1]);
        stsd.extend(boxed(b"vp09", &vp09));
        let mut stz2 = vec![0, 0, 0, 4];
        stz2.extend(be32(&[6]));
        stz2.extend([0x33, 0x33, 0x33]);
        let mut co64 = be32(&[2]);
        co64.extend([0u8; 16]);
        let mut ctts = be32(&[2, 3]);
        ctts.extend((-1i32).to_be_bytes());
        ctts.extend(be32(&[3, 2]));
        let mut stbl = full(b"stsd", 0, &stsd);
        stbl.extend(full(b"stts", 0, &be32(&[1, 6, 1])));
        stbl.extend(full(b"ctts", 1, &ctts));
        stbl.extend(full(b"stz2", 0, &stz2));
        stbl.extend(full(b"stsc", 0, &be32(&[1, 1, 3, 1])));
        video_offset_at = stbl.len() + 16;
        stbl.extend(full(b"co64", 0, &co64));
        stbl.extend(full(b"stss", 0, &be32(&[2, 1, 4])));
        stbl
    };

    let mut mp4a = vec![0u8; 28];
    mp4a[7] = 1;
    mp4a[16..18].copy_from_slice(&[0, 2]);
    mp4a[24..26].copy_from_slice(&48_000u16.to_be_bytes());
    let mut esds = vec![0x03, 22, 0, 1, 0, 0x04, 17, 0x40];
    esds.extend([0u8; 12]);
    esds.extend([0x05, 2, 0x11, 0x90]);
    mp4a.extend(full(b"esds", 0, &esds));
    let audio_offset_at;
    let audio_stbl = {
        let mut stsd = be32(&[1]);
        stsd.extend(boxed(b"mp4a", &mp4a));
        let mut stbl = full(b"stsd", 0, &stsd);
        stbl.extend(full(b"stts", 0, &be32(&[1, 4, 1024])));
        stbl.extend(full(b"stsz", 0, &be32(&[5, 4])));
        stbl.extend(full(b"stsc", 0, &be32(&[1, 1, 4, 1])));
        audio_offset_at = stbl.len() + 16;
        stbl.extend(full(b"stco", 0, &be32(&[1, 0])));
        stbl
    };
    let mut mdhd_v0 = vec![0u8; 8];
    mdhd_v0.extend(be32(&[48_000, 4096, 0]));

    let data = |kind: u32, payload: &[u8]| {
        let mut body = be32(&[kind, 0]);
        body.extend_from_slice(payload);
        boxed(b"data", &body)
    };
    let mut ilst = boxed(&[0xA9, b'n', b'a', b'm'], &data(1, b"a title"));
    ilst.extend(boxed(b"trkn", &data(0, &[0, 0, 0, 3, 0, 12, 0, 0])));
    ilst.extend(boxed(b"covr", &data(13, &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2])));
    ilst.extend(boxed(&[0xA9, b'd', b'a', b'y'], &data(21, &[0x07, 0xD1])));
    ilst.extend(boxed(
        b"----",
        &[
            full(b"mean", 0, b"com.apple.iTunes"),
            full(b"name", 0, b"replaygain_track_gain"),
            data(1, b"-6.5 dB"),
        ]
        .concat(),
    ));
    let udta = boxed(b"udta", &full(b"meta", 0, &[hdlr(b"mdir"), boxed(b"ilst", &ilst)].concat()));

    let mut mvhd = vec![0u8; 8];
    mvhd.extend(be32(&[600, 3600]));
    mvhd.extend([0u8; 80]);
    let assemble = |video_stbl: &[u8], audio_stbl: &[u8]| {
        let video = boxed(
            b"trak",
            &[
                tkhd(1),
                boxed(b"edts", &full(b"elst", 1, &elst_v1)),
                boxed(
                    b"mdia",
                    &[
                        full(b"mdhd", 1, &mdhd_v1),
                        hdlr(b"vide"),
                        boxed(b"minf", &boxed(b"stbl", video_stbl)),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );
        let audio = boxed(
            b"trak",
            &[
                tkhd(2),
                boxed(
                    b"mdia",
                    &[
                        full(b"mdhd", 0, &mdhd_v0),
                        hdlr(b"soun"),
                        boxed(b"minf", &boxed(b"stbl", audio_stbl)),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );
        let moov = boxed(b"moov", &[full(b"mvhd", 0, &mvhd), video, audio, udta.clone()].concat());
        [ftyp.clone(), moov].concat()
    };

    let mdat_data = assemble(&video_stbl, &audio_stbl).len() + 8;
    let mut video_stbl = video_stbl;
    let mut audio_stbl = audio_stbl;
    let chunk = |n: usize| (mdat_data + n) as u64;
    video_stbl[video_offset_at..video_offset_at + 8].copy_from_slice(&chunk(0).to_be_bytes());
    video_stbl[video_offset_at + 8..video_offset_at + 16].copy_from_slice(&chunk(29).to_be_bytes());
    audio_stbl[audio_offset_at..audio_offset_at + 4]
        .copy_from_slice(&(chunk(9) as u32).to_be_bytes());
    let mut file = assemble(&video_stbl, &audio_stbl);
    assert_eq!(file.len() + 8, mdat_data);
    file.extend(boxed(b"mdat", &(0..38u8).collect::<Vec<u8>>()));
    file
}

const CONTAINERS: [&[u8; 4]; 13] = [
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"edts", b"udta", b"ilst", b"dinf", b"mvex",
    b"moof", b"traf", b"mfra",
];

fn box_sites(data: &[u8]) -> Vec<Site> {
    fn walk(data: &[u8], base: usize, depth: usize, sites: &mut Vec<Site>) {
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let Ok(h) = boxes::read_box_header(data, at) else {
                return;
            };
            sites.push(Site::boundary(base + at));
            sites.push(Site::be(base + at, 4));
            let body = h.body(data);
            let body_at = base + h.body_start;
            let kind = &h.kind;
            let children_at = match kind {
                k if CONTAINERS.contains(&k) => Some(0),
                b"meta" => Some(4),
                b"stsd" => Some(8),
                b"avc1" | b"vp09" => Some(78),
                b"mp4a" => Some(28),
                _ if depth >= 4 && body.len() >= 16 && &body[4..8] == b"data" => Some(0),
                _ => None,
            };
            match children_at {
                Some(skip) if skip <= body.len() && depth < 10 => {
                    for field in (0..skip).step_by(4) {
                        sites.push(Site::be(body_at + field, 4));
                    }
                    walk(&body[skip..], body_at + skip, depth + 1, sites);
                }
                _ => {
                    for field in (0..body.len().min(40)).step_by(4) {
                        sites.push(Site::be(body_at + field, 4));
                    }
                }
            }
            at = h.end;
        }
    }
    let mut sites = Vec::new();
    walk(data, 0, 0, &mut sites);
    sites
}

#[derive(Clone)]
struct Base {
    name: &'static str,
    data: Vec<u8>,
    sites: Vec<Site>,
}

fn base(name: &'static str, data: Vec<u8>) -> Base {
    let sites = box_sites(&data);
    Base { name, data, sites }
}

fn file_bases() -> Vec<Base> {
    vec![
        base("handmade", handmade_base()),
        base("tiny_h264", TINY_H264.to_vec()),
        base("bframes_h264", BFRAMES_H264.to_vec()),
        base("tiny_av", TINY_AV.to_vec()),
    ]
}

fn fragment_bases() -> Vec<Base> {
    vec![
        base("frag_av", FRAG_AV.to_vec()),
        base("frag_sidx", FRAG_SIDX.to_vec()),
        base("frag_hybrid", FRAG_HYBRID.to_vec()),
        base("dash", [DASH_INIT, DASH_FIRST, DASH_OTHER_INIT, DASH_OTHER].concat()),
    ]
}

fn file_case(prng: &mut Prng, bases: &[Base]) -> Case {
    let pick = match prng.below(2) {
        0 => 0,
        _ => prng.below(bases.len() as u64) as usize,
    };
    let base = &bases[pick];
    let (data, ops) = match prng.below(20) {
        0 => noise(prng, 512),
        1 | 2 => prefix_then_garbage(prng, &base.data, &base.sites, 256),
        _ => mutate(prng, &base.data, &base.sites),
    };
    let chunking = Chunking::pick(prng, data.len());
    Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
}

fn pristine<'a>(bases: &'a [Base], name: &str) -> &'a [u8] {
    bases.iter().find(|b| b.name == name).map_or(&bases[0].data, |b| &b.data)
}

/// Densest table is `stz2` at two samples an octet, 32 octets each in the index plus merge and
/// sort.
const RESOLVE_HEAP_PER_BYTE: usize = 256;
/// A run-length table costs the file an octet a sample, 32 index octets each.
const RESOLVE_INDEX_PER_FILE_OCTET: usize = 32;
const RESOLVE_HEAP_PER_SAMPLE: usize = 128;
const RESOLVE_HEAP_SLACK: usize = 16 << 10;

/// The heap and the sample index together, the index lives in its own mapping.
fn check_resolved(mark: isize, index: usize, limit: usize) -> Result<(), Violation> {
    let grew = heap_growth(mark) + index;
    if grew > limit {
        return Err(Violation::Heap { grew, limit });
    }
    Ok(())
}

fn drive_reader(case: &Case, bases: &[Base]) -> Result<(), Violation> {
    let mark = heap_mark();
    let head = match case.knob & 1 {
        0 => &case.data[..],
        _ => pristine(bases, case.base),
    };
    let limits = Limits { file_len: Some(case.data.len() as u64), ..Limits::default() };
    let mut reader = match Mp4Reader::bounded(head, &[], limits) {
        Ok(reader) => reader,
        Err(_) => {
            return check_heap(mark, RESOLVE_HEAP_PER_BYTE * head.len() + RESOLVE_HEAP_SLACK);
        }
    };
    let samples = match reader.is_fragmented() {
        true => reader.samples().len() + case.data.len(),
        false => reader.samples().len(),
    };
    // Table rows hold at most two samples an octet, run-length ones one per file octet.
    check_output(reader.samples().len(), 2 * head.len() + case.data.len())?;
    check_output(reader.tracks().len(), MAX_TRACKS)?;
    check_resolved(
        mark,
        reader.tables().index_bytes(),
        RESOLVE_HEAP_PER_BYTE * head.len()
            + RESOLVE_INDEX_PER_FILE_OCTET * case.data.len()
            + RESOLVE_HEAP_SLACK,
    )?;

    reader.reset_stream();
    let retain = case.knob & 2 != 0;
    let (mut emitted, mut bytes) = (0u64, 0usize);
    let (act, before) = (case.knob >> 8 & 7, (case.knob >> 16) as usize % 8);
    let mut fed = 0u64;
    for (index, chunk) in case.chunking.chunks(&case.data).enumerate() {
        if reader.is_fragmented() && index == before {
            let stamp = Some(case.knob >> 24 & 0xFFFF_FFFF);
            match act {
                0 => reader.mark(false, None, None),
                1 => reader.mark(true, stamp, None),
                2 => reader.mark(true, stamp, Some(case.knob >> 32)),
                3 => drop(reader.seek_stream(fed, stamp, 0)),
                4 => reader.select(0, false),
                5 => reader.select(0, true),
                _ => {}
            }
        }
        fed += chunk.len() as u64;
        let _ = (reader.take_error(), reader.is_lost(), reader.init_epoch(), reader.shift_ns());
        match retain {
            true => reader.push(input_buffer(chunk).memory),
            false => reader.push_bytes(chunk),
        }
        while let Some(sample) = reader.next_sample() {
            emitted += 1;
            bytes += sample.payload.data().len();
            let _ = reader.ticks_to_ns(sample.track_index, sample.pts);
            let _ = reader.ticks_to_ns(sample.track_index, sample.dts);
        }
        check_steps(emitted, samples as u64)?;
    }
    check_output(bytes, case.data.len())?;
    check_heap(
        mark,
        RESOLVE_HEAP_PER_BYTE * head.len()
            + RESOLVE_HEAP_PER_SAMPLE * samples
            + 4 * case.data.len()
            + RESOLVE_HEAP_SLACK,
    )
}

#[test]
fn reader_survives_corrupt_files() {
    let bases = file_bases();
    gate("mp4 reader", 3_000, &|prng| file_case(prng, &bases), &|case| drive_reader(case, &bases));
}

#[test]
fn reader_survives_corrupt_fragments() {
    let bases = fragment_bases();
    gate("mp4 fragments", 3_000, &|prng| file_case(prng, &bases), &|case| {
        drive_reader(case, &bases)
    });
    for base in &bases {
        sweep_gate("mp4 fragments sweep", base.name, &base.data, &base.sites, 300, &|case| {
            drive_reader(case, &bases)
        });
    }
}

#[test]
fn reader_survives_every_truncation_and_extreme_field() {
    let bases = file_bases();
    for base in &bases {
        sweep_gate("mp4 reader sweep", base.name, &base.data, &base.sites, 400, &|case| {
            drive_reader(case, &bases)
        });
    }
}

/// A `stco` row grows from 4 to 8 octets and an `stz2` nibble to 4.
const TABLE_HEAP_PER_BYTE: usize = 16;

fn drive_boxes(case: &Case) -> Result<(), Violation> {
    let body = &case.data[..];
    let rows = |len: usize, min_row: usize| check_output(len, body.len() / min_row + 1);
    let mark = heap_mark();
    let _ = boxes::parse_mvhd(body);
    let _ = boxes::parse_tkhd(body);
    let _ = boxes::parse_mdhd(body);
    if let Ok(table) = boxes::parse_elst(body) {
        rows(table.len(), 12)?;
    }
    if let Ok(table) = boxes::parse_stts(body) {
        rows(table.len(), 8)?;
    }
    if let Ok(table) = boxes::parse_ctts(body) {
        rows(table.len(), 8)?;
    }
    if let Ok(boxes::SampleSizes::Table(table)) = boxes::parse_stsz(body) {
        rows(table.len(), 4)?;
    }
    if let Ok(boxes::SampleSizes::Table(table)) = boxes::parse_stz2(body) {
        check_output(table.len(), 2 * body.len())?;
    }
    if let Ok(table) = boxes::parse_stsc(body) {
        rows(table.len(), 12)?;
    }
    for is_64 in [false, true] {
        if let Ok(table) = boxes::parse_chunk_offsets(body, is_64) {
            rows(table.len(), 4)?;
        }
    }
    if let Ok(table) = boxes::parse_stss(body) {
        rows(table.len(), 4)?;
    }
    if let Ok(entry) = parse_stsd(body) {
        check_output(entry.codec_head.len() + entry.config_record.len(), 3 * body.len())?;
    }
    let _ = frag::parse_trex(body);
    let _ = frag::parse_tfhd(body);
    let _ = frag::parse_tfdt(body);
    let _ = frag::parse_mehd(body);
    let _ = frag::mfra_size(body);
    if let Ok(mut run) = frag::parse_trun(body) {
        for _ in 0..(run.count as usize).min(body.len() + 1) {
            if run.row().is_err() {
                break;
            }
        }
    }
    if let Ok(sidx) = frag::parse_sidx(body) {
        rows(sidx.refs.len(), 12)?;
        let mut fragments = Vec::new();
        frag::sidx_fragments(&sidx, case.knob, None, &mut fragments);
        check_output(fragments.len(), sidx.refs.len())?;
    }
    if let Ok((_, table)) = frag::parse_tfra(body) {
        rows(table.len(), 11)?;
    }
    check_heap(mark, TABLE_HEAP_PER_BYTE * body.len() + (4 << 10))?;

    let mut at = 0usize;
    for chunk in case.chunking.chunks(body).take(64) {
        let window = &body[at..];
        let mut children = 0usize;
        let _ = boxes::for_each_child(window, |_| {
            children += 1;
            Ok(())
        });
        check_steps(children as u64, window.len() as u64 / 8 + 1)?;
        let _ = boxes::find_child(window, boxtype::MOOV);
        let _ = boxes::read_box_header(body, at);
        at += chunk.len();
    }
    Ok(())
}

fn table_bases(files: &[Base]) -> Vec<Base> {
    fn collect(data: &[u8], depth: usize, out: &mut Vec<Base>) {
        let _ = boxes::for_each_child(data, |h| {
            let body = h.body(data);
            if CONTAINERS.contains(&&h.kind) && depth < 8 {
                collect(body, depth + 1, out);
                return Ok(());
            }
            let name = match &h.kind {
                b"mvhd" => "mvhd",
                b"tkhd" => "tkhd",
                b"mdhd" => "mdhd",
                b"elst" => "elst",
                b"stts" => "stts",
                b"ctts" => "ctts",
                b"stsz" => "stsz",
                b"stz2" => "stz2",
                b"stsc" => "stsc",
                b"stco" => "stco",
                b"co64" => "co64",
                b"stss" => "stss",
                b"stsd" => "stsd",
                b"trex" => "trex",
                b"mehd" => "mehd",
                b"tfhd" => "tfhd",
                b"tfdt" => "tfdt",
                b"trun" => "trun",
                b"sidx" => "sidx",
                b"tfra" => "tfra",
                _ => return Ok(()),
            };
            let sites = (0..body.len().min(48)).step_by(4).map(|at| Site::be(at, 4)).collect();
            out.push(Base { name, data: body.to_vec(), sites });
            Ok(())
        });
    }
    let mut out = Vec::new();
    for file in files {
        collect(&file.data, 0, &mut out);
    }
    out
}

#[test]
fn box_and_table_parsers_survive_corrupt_bodies() {
    let bases = table_bases(&[file_bases(), fragment_bases()].concat());
    assert!(bases.iter().any(|b| b.name == "stz2") && bases.iter().any(|b| b.name == "ctts"));
    for name in ["trex", "tfhd", "tfdt", "trun", "sidx", "tfra"] {
        assert!(bases.iter().any(|b| b.name == name), "no {name} among the bases");
    }
    let make = |prng: &mut Prng| {
        let base = &bases[prng.below(bases.len() as u64) as usize];
        let (data, ops) = match prng.below(12) {
            0 => noise(prng, 256),
            1 => prefix_then_garbage(prng, &base.data, &base.sites, 64),
            _ => mutate(prng, &base.data, &base.sites),
        };
        let chunking = Chunking::pick(prng, data.len());
        Case { base: base.name, ops, data, chunking, knob: 0 }
    };
    gate("mp4 boxes", 6_000, &make, &drive_boxes);
    for base in &bases {
        sweep_gate("mp4 boxes sweep", base.name, &base.data, &base.sites, 60, &drive_boxes);
    }
}

#[derive(Default)]
struct CountingSink {
    entries: usize,
    bytes: usize,
}

impl TagSink for CountingSink {
    fn text(&mut self, key: &str, value: &str) {
        self.entries += 1;
        self.bytes += key.len() + value.len();
    }

    fn picture(&mut self, mime: &str, data: &[u8]) {
        self.entries += 1;
        self.bytes += mime.len() + data.len();
    }
}

fn drive_scanner(case: &Case) -> Result<(), Violation> {
    let mark = heap_mark();
    let data = &case.data[..];
    let scratch = Arena::default();

    let lengths = [
        data.len() as u64,
        (data.len() as u64).saturating_add(case.knob >> 40),
        EXTREMES[(case.knob % EXTREMES.len() as u64) as usize],
    ];
    for file_len in lengths {
        let mut window = &data[..(case.knob >> 8) as usize % (data.len() + 1)];
        let mut reads = 0u64;
        loop {
            match locate_moov(window, file_len) {
                MoovExtent::InPrefix { offset, len } => {
                    let Some(moov) =
                        offset.checked_add(len).and_then(|end| window.get(offset..end))
                    else {
                        return Err(Violation::Invariant(
                            "locate_moov named a range outside the prefix",
                        ));
                    };
                    let mut sink = CountingSink::default();
                    parse_moov_tags(moov, &scratch, &mut sink);
                    check_output(sink.bytes, moov.len() + 160 * sink.entries)?;
                    check_output(sink.entries, moov.len() / 16 + 1)?;
                    let _ = props_from_moov(moov);
                    break;
                }
                MoovExtent::Beyond { offset, len } => {
                    let end = offset.saturating_add(len);
                    if len == 0 || end <= window.len() as u64 && offset == 0 {
                        return Err(Violation::Invariant(
                            "locate_moov asked for bytes already held",
                        ));
                    }
                    let start = (offset.min(data.len() as u64)) as usize;
                    let end = (end.min(data.len() as u64)) as usize;
                    window = &data[start..end.max(start)];
                    reads += 1;
                    if reads > 4 {
                        break;
                    }
                }
                MoovExtent::NotFound => break,
            }
        }
    }

    let mut sink = CountingSink::default();
    parse_moov_tags(data, &scratch, &mut sink);
    let _ = props_from_moov(data);
    check_heap(mark, 64 << 10)
}

fn scanner_bases() -> Vec<Base> {
    let mut bases = [file_bases(), fragment_bases()].concat();
    let mut moovs = Vec::new();
    for file in &bases {
        if let MoovExtent::InPrefix { offset, len } =
            locate_moov(&file.data, file.data.len() as u64)
        {
            moovs.push(base("moov", file.data[offset..offset + len].to_vec()));
        }
    }
    assert_eq!(moovs.len(), bases.len(), "every base has a moov in reach");
    bases.extend(moovs);
    bases
}

#[test]
fn scanner_parsers_survive_corrupt_files() {
    let bases = scanner_bases();
    gate("mp4 scanner", 4_000, &|prng| file_case(prng, &bases), &drive_scanner);
    for base in bases.iter().filter(|b| b.name == "moov" || b.name == "handmade") {
        sweep_gate("mp4 scanner sweep", base.name, &base.data, &base.sites, 400, &drive_scanner);
    }
}

fn drive_demux(case: &Case, bases: &[Base]) -> Result<(), Violation> {
    let mark = heap_mark();
    let head = match case.knob >> 4 & 1 {
        0 => case.data.clone(),
        _ => pristine(bases, case.base).to_vec(),
    };
    let head_len = head.len();
    let element = match case.knob >> 5 & 1 {
        0 => Mp4Demux::new(head),
        _ => Mp4Demux::passthrough(head),
    };
    let element = match case.knob >> 6 & 1 {
        0 => element.limits(Limits { file_len: Some(case.data.len() as u64), ..Limits::default() }),
        _ => element,
    };
    let fed = feed_bytes(element, case, None, 3 * head_len)?;
    check_output(fed.out_bytes, 3 * case.data.len() + 3 * head_len + 64)?;
    check_heap(
        mark,
        (RESOLVE_HEAP_PER_BYTE + 2) * head_len
            + (RESOLVE_HEAP_PER_SAMPLE + 8) * case.data.len()
            + pool_allowance(case.knob),
    )
}

#[test]
fn demux_element_survives_corrupt_files() {
    let bases = file_bases();
    gate("mp4 demux", 1_500, &|prng| file_case(prng, &bases), &|case| drive_demux(case, &bases));
}

#[test]
fn demux_element_survives_corrupt_fragments() {
    let bases = fragment_bases();
    gate("mp4 demux fragments", 1_500, &|prng| file_case(prng, &bases), &|case| {
        drive_demux(case, &bases)
    });
    for base in &bases {
        sweep_gate("mp4 demux fragments sweep", base.name, &base.data, &base.sites, 100, &|case| {
            drive_demux(case, &bases)
        });
    }
}

#[test]
fn the_driver_collects_what_the_element_puts_out() {
    for (name, data, samples) in [("tiny_av", TINY_AV, 15 + 48), ("frag_av", FRAG_AV, 45 + 142)] {
        let case = Case {
            base: name,
            ops: Vec::new(),
            data: data.to_vec(),
            chunking: Chunking::Fixed(3000),
            knob: 0,
        };
        let fed = feed_bytes(Mp4Demux::new(data.to_vec()), &case, None, 3 * data.len())
            .expect("in bounds");
        assert!(!fed.failed, "{name}");
        assert!(fed.out_buffers >= samples + 2, "{name}: {} buffers", fed.out_buffers);
        assert!(fed.out_bytes > data.len() * 3 / 4 && fed.out_bytes < data.len() + 256, "{name}");
    }
}

fn drive_expand(case: &Case, tracks: &[pf_mp4::Track]) -> Result<(), Violation> {
    let mark = heap_mark();
    let trex = vec![
        frag::Trex { track_id: 1, duration: 1, size: (case.knob & 3) as u32, flags: 0 };
        tracks.len()
    ];
    let movie = frag::Movie { tracks, trex: &trex };
    let moof = case.data.len() as u64;
    let room = case.knob >> 8 & 0xFFFF;
    let place = frag::Place { moof: case.knob >> 40, mdat: (moof, moof + room) };
    let mut next_dts = vec![0u64; tracks.len()];
    let mut out = Vec::new();
    let _ = frag::expand(&case.data, &movie, place, &mut next_dts, &mut out);
    check_output(out.len(), room as usize)?;
    check_heap(mark, 64 * room as usize + (4 << 10))
}

#[test]
fn a_moof_claims_no_more_than_its_mdat_holds() {
    let reader = Mp4Reader::new(DASH_INIT).expect("the init segment resolves");
    let mut bases = Vec::new();
    for file in fragment_bases() {
        let _ = boxes::for_each_child(&file.data, |h| {
            if h.kind == boxtype::MOOF {
                bases.push(base("moof", h.body(&file.data).to_vec()));
            }
            Ok(())
        });
    }
    assert!(bases.len() >= 8);
    let make = |prng: &mut Prng| {
        let base = &bases[prng.below(bases.len() as u64) as usize];
        let (data, ops) = match prng.below(12) {
            0 => noise(prng, 256),
            _ => mutate(prng, &base.data, &base.sites),
        };
        let chunking = Chunking::pick(prng, data.len());
        Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
    };
    gate("mp4 moof", 6_000, &make, &|case| drive_expand(case, reader.tracks()));
}

#[test]
fn demux_element_survives_every_truncation_and_extreme_field() {
    let bases = file_bases();
    for base in &bases {
        sweep_gate("mp4 demux sweep", base.name, &base.data, &base.sites, 150, &|case| {
            drive_demux(case, &bases)
        });
    }
}

#[test]
fn the_handmade_base_resolves_and_streams() {
    let file = handmade_base();
    let mut reader = Mp4Reader::new(&file).expect("the handmade base is a valid movie");
    assert_eq!((reader.tracks().len(), reader.samples().len()), (2, 10));
    assert_eq!(reader.tracks()[1].family(), "aac");
    reader.reset_stream();
    reader.push_bytes(&file);
    let mut bytes = Vec::new();
    while let Some(sample) = reader.next_sample() {
        bytes.extend_from_slice(sample.payload.data());
    }
    assert_eq!(bytes, (0..38u8).collect::<Vec<u8>>());
    let mut sink = CountingSink::default();
    let MoovExtent::InPrefix { offset, len } = locate_moov(&file, file.len() as u64) else {
        panic!("moov is in the file");
    };
    parse_moov_tags(&file[offset..offset + len], &Arena::default(), &mut sink);
    assert_eq!(sink.entries, 5, "title, track number, cover, date, freeform");
    let _: Option<Mp4Error> = None;
}

/// Table parsers reserved from the declared row count, 8 to 32 MiB for a 12 octet body.
#[test]
fn a_table_claiming_four_billion_rows_reserves_only_what_its_box_holds() {
    let counted = [0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
    let stsz = [0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
    let stz2 = [0, 0, 0, 0, 0, 0, 0, 4, 0xFF, 0xFF, 0xFF, 0xFF];
    let mark = heap_mark();
    assert!(boxes::parse_elst(&counted).is_err());
    assert!(boxes::parse_stts(&counted).is_err());
    assert!(boxes::parse_ctts(&counted).is_err());
    assert!(boxes::parse_stsc(&counted).is_err());
    assert!(boxes::parse_stss(&counted).is_err());
    assert!(boxes::parse_chunk_offsets(&counted, false).is_err());
    assert!(boxes::parse_chunk_offsets(&counted, true).is_err());
    assert!(boxes::parse_stsz(&stsz).is_err());
    assert!(boxes::parse_stz2(&stz2).is_err());
    assert!(heap_growth(mark) < 1024, "a declared row count was trusted for an allocation");
}

fn one_track_movie(tables: &[u8]) -> Vec<u8> {
    let mut stsd = be32(&[1]);
    stsd.extend(boxed(b"raw ", &[0u8; 8]));
    let mut mdhd = vec![0u8; 8];
    mdhd.extend(be32(&[1000, 0, 0]));
    let stbl = [full(b"stsd", 0, &stsd), tables.to_vec()].concat();
    let minf = boxed(b"minf", &boxed(b"stbl", &stbl));
    boxed(b"moov", &boxed(b"trak", &boxed(b"mdia", &[full(b"mdhd", 0, &mdhd), minf].concat())))
}

/// 76 octets of run-length tables can describe four billion samples, a 160 GiB index.
#[test]
fn run_length_tables_describing_four_billion_samples_are_refused() {
    let tables = [
        full(b"stts", 0, &be32(&[1, 0xFFFF_FFFF, 1])),
        full(b"stsz", 0, &be32(&[1, 0xFFFF_FFFF])),
        full(b"stsc", 0, &be32(&[1, 1, 0xFFFF_FFFF, 1])),
        full(b"stco", 0, &be32(&[1, 0])),
    ]
    .concat();
    let mark = heap_mark();
    assert!(matches!(
        Mp4Reader::new(&one_track_movie(&tables)),
        Err(Mp4Error::TooManySamples { limit: MAX_SAMPLES })
    ));
    assert!(heap_growth(mark) < 4096);
}

/// 76 octets of run-length tables declare `MAX_SAMPLES` one octet samples, a 512 MiB index
/// once folded. Held to the file length, the index takes only the samples the file holds.
#[test]
fn a_tiny_table_declaring_max_samples_commits_only_what_the_file_holds() {
    const FILE: usize = 64 << 10;
    let n = MAX_SAMPLES as u32;
    let tables = |chunks: &[u32]| {
        let spc = n / chunks.len() as u32;
        [
            full(b"stts", 0, &be32(&[1, n, 1])),
            full(b"stsz", 0, &be32(&[1, n])),
            full(b"stsc", 0, &be32(&[1, 1, spc, 1])),
            full(b"stco", 0, &be32(&[&[chunks.len() as u32][..], chunks].concat())),
        ]
        .concat()
    };
    let head = one_track_movie(&tables(&[0]));
    let known = Limits { file_len: Some(FILE as u64), ..Limits::default() };
    let mark = heap_mark();
    let start = std::time::Instant::now();
    let reader = Mp4Reader::bounded(&head, &[], known).expect("the samples in the file resolve");
    assert_eq!(reader.samples().len(), FILE, "the samples past the end are left out");
    assert_eq!(reader.tables().index_bytes(), 32 * FILE);
    assert!(heap_growth(mark) < 4096, "{} heap octets", heap_growth(mark));
    // The tail past the end is stepped over a run at a time, not a sample at a time.
    assert!(start.elapsed() < std::time::Duration::from_secs(1), "{:?}", start.elapsed());
    drop(reader);

    // 256 chunks over the same octets are an overlap, refused once the file is spent.
    let chunks = vec![0u32; 256];
    let overlapping = Mp4Reader::bounded(&one_track_movie(&tables(&chunks)), &[], known);
    assert!(matches!(overlapping, Err(Mp4Error::Inconsistent(_))));

    // The gate's budget, a sample a file octet, holds this input class.
    let case = Case {
        base: "max",
        ops: Vec::new(),
        data: head.clone(),
        chunking: Chunking::Fixed(64),
        knob: 0,
    };
    assert!(drive_reader(&case, &[]).is_ok());

    // Without the length an embedder's budget refuses the table before anything is folded.
    let budget = Limits { file_len: None, samples: 1 << 16 };
    assert!(matches!(
        Mp4Reader::bounded(&head, &[], budget),
        Err(Mp4Error::TooManySamples { limit: 65_536 })
    ));
}

/// Several samples naming the same bytes made output unbounded in the input.
#[test]
fn samples_pointing_at_the_same_bytes_are_refused() {
    let tables = |offsets: [u32; 3]| {
        [
            full(b"stts", 0, &be32(&[1, 3, 1])),
            full(b"stsz", 0, &be32(&[4, 3])),
            full(b"stsc", 0, &be32(&[1, 1, 1, 1])),
            full(b"stco", 0, &be32(&[3, offsets[0], offsets[1], offsets[2]])),
        ]
        .concat()
    };
    let disjoint = Mp4Reader::new(&one_track_movie(&tables([100, 104, 108])));
    assert_eq!(disjoint.map(|r| r.samples().len()).ok(), Some(3));
    let overlapping = Mp4Reader::new(&one_track_movie(&tables([100, 102, 108])));
    assert!(matches!(overlapping, Err(Mp4Error::Inconsistent(_))));
}

#[test]
fn more_tracks_than_the_reader_resolves_is_refused() {
    let movie = |tracks: u32| {
        let mut body = Vec::new();
        for n in 0..tracks {
            let tables = [
                full(b"stts", 0, &be32(&[1, 1, 1])),
                full(b"stsz", 0, &be32(&[1, 1])),
                full(b"stsc", 0, &be32(&[1, 1, 1, 1])),
                full(b"stco", 0, &be32(&[1, n])),
            ]
            .concat();
            body.extend_from_slice(&one_track_movie(&tables)[8..]);
        }
        boxed(b"moov", &body)
    };
    assert_eq!(Mp4Reader::new(&movie(256)).map(|r| r.tracks().len()).ok(), Some(256));
    assert!(matches!(
        Mp4Reader::new(&movie(257)),
        Err(Mp4Error::TooManyTracks { limit: MAX_TRACKS })
    ));
}
