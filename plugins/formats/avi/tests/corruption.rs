// Test code builds its inputs on the heap.
#![allow(clippy::disallowed_methods)]

use pf_avi::riff::MAX_STREAMS;
use pf_avi::{AviDemux, MoviWalker, build_seek_index, codec, parse_idx1, probe_header};
use pf_sim::corrupt::harness::{feed_bytes, pool_allowance};
use pf_sim::corrupt::{
    Case, Chunking, Site, Tracking, Violation, check_heap, check_output, check_steps, gate,
    heap_growth, heap_mark, mutate, noise, prefix_then_garbage, sweep_gate,
};
use pf_sim::prng::Prng;

#[global_allocator]
static HEAP: Tracking = Tracking;

static TINY: &[u8] = include_bytes!("fixtures/tiny.avi");

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    chunk(b"LIST", &[kind.to_vec(), body.to_vec()].concat())
}

fn strh(
    kind: &[u8; 4],
    handler: &[u8; 4],
    scale: u32,
    rate: u32,
    length: u32,
    sample_size: u32,
) -> Vec<u8> {
    let mut body = vec![0u8; 56];
    body[0..4].copy_from_slice(kind);
    body[4..8].copy_from_slice(handler);
    body[20..24].copy_from_slice(&scale.to_le_bytes());
    body[24..28].copy_from_slice(&rate.to_le_bytes());
    body[32..36].copy_from_slice(&length.to_le_bytes());
    body[44..48].copy_from_slice(&sample_size.to_le_bytes());
    chunk(b"strh", &body)
}

fn wave_format(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let mut body = tag.to_le_bytes().to_vec();
    body.extend_from_slice(&channels.to_le_bytes());
    body.extend_from_slice(&rate.to_le_bytes());
    body.extend_from_slice(&[0u8; 6]);
    body.extend_from_slice(&bits.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    chunk(b"strf", &body)
}

fn handmade_base() -> Vec<u8> {
    let mut avih = vec![0u8; 56];
    avih[0..4].copy_from_slice(&40_000u32.to_le_bytes());
    avih[16..20].copy_from_slice(&4u32.to_le_bytes());
    let mut bitmap = vec![0u8; 40];
    bitmap[0..4].copy_from_slice(&40u32.to_le_bytes());
    bitmap[4..8].copy_from_slice(&160i32.to_le_bytes());
    bitmap[8..12].copy_from_slice(&(-120i32).to_le_bytes());
    bitmap[16..20].copy_from_slice(b"XVID");
    let hdrl = [
        chunk(b"avih", &avih),
        list(b"strl", &[strh(b"vids", b"XVID", 1, 25, 4, 0), chunk(b"strf", &bitmap)].concat()),
        list(
            b"strl",
            &[strh(b"auds", &[0; 4], 1, 48_000, 9, 0), wave_format(0x2000, 6, 48_000, 0)].concat(),
        ),
        list(
            b"strl",
            &[
                strh(b"auds", &[0; 4], 1, 8_000, 64, 4),
                wave_format(1, 2, 8_000, 16),
                chunk(b"JUNK", &[0; 5]),
            ]
            .concat(),
        ),
        list(b"odml", &chunk(b"dmlh", &4u32.to_le_bytes())),
    ]
    .concat();
    let movi = [
        chunk(b"00dc", &[0xD0; 21]),
        chunk(b"01wb", &[0xA1; 8]),
        chunk(b"JUNK", &[0; 7]),
        list(b"rec ", &[chunk(b"00dc", &[0xD1; 10]), chunk(b"02wb", &[0xA2; 16])].concat()),
        list(b"INFO", &chunk(b"ISFT", b"none")),
        chunk(b"00dc", &[]),
        chunk(b"02wb", &[0xA3; 12]),
        chunk(b"00db", &[0xD2; 33]),
        chunk(b"01wb", &[0xA4; 3]),
    ]
    .concat();
    let mut idx1 = Vec::new();
    for (id, flags, offset, size) in [
        (b"00dc", 0x10u32, 4u32, 21u32),
        (b"01wb", 0, 34, 8),
        (b"00dc", 0, 70, 10),
        (b"00dc", 0x10, 132, 0),
    ] {
        idx1.extend_from_slice(id);
        idx1.extend_from_slice(&flags.to_le_bytes());
        idx1.extend_from_slice(&offset.to_le_bytes());
        idx1.extend_from_slice(&size.to_le_bytes());
    }
    let body = [
        b"AVI ".to_vec(),
        list(b"hdrl", &hdrl),
        chunk(b"JUNK", &[0; 9]),
        list(b"movi", &movi),
        chunk(b"idx1", &idx1),
    ]
    .concat();
    chunk(b"RIFF", &body)
}

fn riff_sites(data: &[u8]) -> Vec<Site> {
    fn walk(data: &[u8], mut at: usize, end: usize, depth: usize, sites: &mut Vec<Site>) {
        while at + 8 <= end {
            let id = &data[at..at + 4];
            let size = u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]])
                as usize;
            sites.push(Site::boundary(at));
            sites.push(Site::le(at + 4, 4));
            let body_end = (at + 8).saturating_add(size).min(end);
            if (id == b"RIFF" || id == b"LIST") && depth < 6 {
                walk(data, at + 12, body_end, depth + 1, sites);
            } else if id == b"strh" || id == b"strf" || id == b"avih" || id == b"idx1" {
                for field in (0..(body_end - at - 8).min(48)).step_by(4) {
                    sites.push(Site::le(at + 8 + field, 4));
                }
            }
            at = body_end + (size & 1);
        }
    }
    let mut sites = Vec::new();
    walk(data, 0, data.len(), 0, &mut sites);
    sites
}

struct Base {
    name: &'static str,
    data: Vec<u8>,
    sites: Vec<Site>,
}

fn base(name: &'static str, data: Vec<u8>) -> Base {
    let mut sites = riff_sites(&data);
    if sites.len() > 600 {
        let tail: Vec<Site> = sites[200..].iter().copied().step_by(sites.len() / 400).collect();
        sites.truncate(200);
        sites.extend(tail);
    }
    Base { name, data, sites }
}

fn file_bases() -> Vec<Base> {
    vec![
        base("handmade", handmade_base()),
        base("tiny head", TINY[..12 * 1024].to_vec()),
        base("tiny", TINY.to_vec()),
    ]
}

fn file_case(prng: &mut Prng, bases: &[Base]) -> Case {
    let base = &bases[[0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 2][prng.below(16) as usize]];
    let (data, ops) = match prng.below(20) {
        0 => noise(prng, 512),
        1 | 2 => prefix_then_garbage(prng, &base.data, &base.sites, 256),
        _ => mutate(prng, &base.data, &base.sites),
    };
    let chunking = Chunking::pick(prng, data.len());
    Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
}

fn header_prefix(file: &[u8]) -> Vec<u8> {
    let end = file.windows(4).position(|w| w == b"movi").map_or(file.len(), |at| at + 4);
    file[..end].to_vec()
}

fn drive_probe(case: &Case) -> Result<(), Violation> {
    let data = &case.data[..];
    let mark = heap_mark();
    if let Ok((header, movi)) = probe_header(data) {
        check_output(header.streams.len(), MAX_STREAMS)?;
        let _ = header.duration_ns();
        for stream in &header.streams {
            let _ = (stream.sample_duration_ns(), codec::family_for(stream));
            let _ = codec::offers_for(stream).len();
            check_output(stream.strf.len(), data.len())?;
        }
        if let Some(movi) = movi
            && (movi.movi_data_start > data.len() as u64
                || movi.movi_chunk_start > movi.movi_data_start)
        {
            return Err(Violation::Invariant("movi located outside the prefix"));
        }
    }
    check_heap(mark, 4 * data.len() + 256 * MAX_STREAMS + (8 << 10))?;

    let mark = heap_mark();
    let mut offset = 0usize;
    for chunk in case.chunking.chunks(data).take(32) {
        let payload = &data[offset..];
        let index = parse_idx1(payload, case.knob >> 20, case.knob >> 8);
        check_output(index.entries.len(), payload.len() / 16)?;
        for entry in index.entries.iter().take(64) {
            let _ = (index.resolve_offset(entry), entry.stream_index());
        }
        let seek = index.build_seek_index((case.knob & 3) as usize, case.knob >> 30);
        check_output(seek.entries.len(), index.entries.len())?;
        if let Some(seek) = build_seek_index(data, payload, case.knob >> 8) {
            check_output(seek.entries.len(), payload.len() / 16)?;
        }
        offset += chunk.len();
    }
    check_heap(mark, 8 * data.len() + 256 * MAX_STREAMS + (8 << 10))
}

#[test]
fn header_probe_and_index_survive_corrupt_files() {
    let mut bases = file_bases();
    bases.truncate(2);
    let handmade = handmade_base();
    let idx1_at = handmade.windows(4).position(|w| w == b"idx1").expect("idx1") + 8;
    bases.push(base("idx1", handmade[idx1_at..].to_vec()));
    gate("avi probe", 1_500, &|prng| file_case(prng, &bases[..]), &drive_probe);
    for base in &bases {
        sweep_gate("avi probe sweep", base.name, &base.data, &base.sites, 150, &drive_probe);
    }
}

fn drive_walker(case: &Case) -> Result<(), Violation> {
    let chunks = case.chunking.split(&case.data);
    let mark = heap_mark();
    let mut walker = MoviWalker::new();
    let resync_at = (case.knob & 1 == 1).then(|| (case.knob >> 8) as usize % chunks.len());
    let (mut media, mut bytes) = (0u64, 0usize);
    for (index, chunk) in chunks.iter().enumerate() {
        if resync_at == Some(index) {
            walker.resync();
        }
        walker.push(chunk);
        while let Some(found) = walker.next_chunk() {
            media += 1;
            bytes += found.data.len();
            if found.stream_index > 99 {
                return Err(Violation::Invariant("a stream number past two digits"));
            }
        }
    }
    check_steps(media, case.data.len() as u64 / 8)?;
    check_output(bytes, case.data.len())?;
    check_heap(mark, 4 * case.data.len() + (16 << 10))
}

fn movi_bases() -> Vec<Base> {
    file_bases()
        .into_iter()
        .map(|b| {
            let start = header_prefix(&b.data).len();
            let data = b.data[start..].to_vec();
            let sites = b
                .sites
                .iter()
                .filter(|s| s.at >= start)
                .map(|s| Site { at: s.at - start, ..*s })
                .collect();
            Base { name: b.name, data, sites }
        })
        .collect()
}

#[test]
fn walker_survives_corrupt_media_lists() {
    let bases = movi_bases();
    gate("avi walker", 4_000, &|prng| file_case(prng, &bases), &drive_walker);
    for base in &bases {
        sweep_gate("avi walker sweep", base.name, &base.data, &base.sites, 500, &drive_walker);
    }
}

fn drive_demux(case: &Case, bases: &[Base]) -> Result<(), Violation> {
    let mark = heap_mark();
    let header = match case.knob >> 4 & 1 {
        0 => header_prefix(&case.data),
        _ => header_prefix(
            bases.iter().find(|b| b.name == case.base).map_or(&bases[0].data, |b| &b.data),
        ),
    };
    let header_len = header.len();
    let chunks = case.chunking.count(&case.data);
    let flush_before = (case.knob >> 5 & 3 == 0).then(|| (case.knob >> 8) as usize % chunks);
    let fed = feed_bytes(AviDemux::new(header), case, flush_before, 0)?;
    check_output(fed.out_bytes, case.data.len())?;
    check_heap(
        mark,
        6 * case.data.len() + 6 * header_len + 256 * MAX_STREAMS + pool_allowance(case.knob),
    )
}

#[test]
fn demux_element_survives_corrupt_files() {
    let bases = file_bases();
    gate("avi demux", 800, &|prng| file_case(prng, &bases), &|case| drive_demux(case, &bases));
}

#[test]
fn demux_element_survives_every_truncation_and_extreme_size() {
    let bases = file_bases();
    for base in &bases {
        let budget = if base.data.len() > 64 << 10 { 20 } else { 200 };
        sweep_gate("avi demux sweep", base.name, &base.data, &base.sites, budget, &|case| {
            drive_demux(case, &bases)
        });
    }
}

#[test]
fn the_handmade_base_probes_and_walks() {
    let file = handmade_base();
    let (header, movi) = probe_header(&file).expect("the handmade base is a valid AVI");
    assert_eq!(header.streams.len(), 3);
    assert!(header.has_odml);
    let movi = movi.expect("movi is in the file");
    let mut walker = MoviWalker::new();
    walker.push(&file[movi.movi_data_start as usize..]);
    let mut sizes = Vec::new();
    while let Some(found) = walker.next_chunk() {
        sizes.push((found.stream_index, found.data.len()));
    }
    assert_eq!(sizes, [(0, 21), (1, 8), (0, 10), (2, 16), (0, 0), (2, 12), (0, 33), (1, 3)]);
}

#[test]
fn many_small_chunks_are_walked_in_linear_time() {
    let chunks = [b'0', b'0', b'd', b'c', 0, 0, 0, 0].repeat(1 << 17);
    let mut walker = MoviWalker::new();
    let begin = std::time::Instant::now();
    walker.push(&chunks);
    let mut found = 0;
    while walker.next_chunk().is_some() {
        found += 1;
    }
    assert_eq!(found, 1 << 17);
    assert!(begin.elapsed().as_secs() < 5, "walking small chunks took {:?}", begin.elapsed());
}

#[test]
fn a_skipped_chunk_with_a_huge_declared_size_is_not_buffered() {
    let mut walker = MoviWalker::new();
    walker.push(&[b'0', b'0', b'd', b'c', 1, 0, 0, 0, 0x42, 0]);
    assert_eq!(walker.next_chunk().map(|c| c.data), Some(vec![0x42]));
    walker.push(&[b'J', b'U', b'N', b'K', 0xFE, 0xFF, 0xFF, 0x7F]);
    let mark = heap_mark();
    let filler = [0u8; 4096];
    for _ in 0..256 {
        walker.push(&filler);
        assert!(walker.next_chunk().is_none());
    }
    assert!(heap_growth(mark) < 16 << 10, "a megabyte of skipped octets was buffered");

    let mut walker = MoviWalker::new();
    walker.push(&[b'0', b'0', b'd', b'c', 0, 0, 0, 0, b'J', b'U', b'N', b'K', 3, 0, 0, 0, 9]);
    assert_eq!(walker.next_chunk().map(|c| c.data.len()), Some(0));
    assert!(walker.next_chunk().is_none());
    walker.push(&[9, 9, 0, b'0', b'1', b'w', b'b', 2, 0, 0, 0, 5, 6]);
    assert_eq!(walker.next_chunk().map(|c| (c.stream_index, c.data)), Some((1, vec![5, 6])));
}

#[test]
fn a_media_chunk_larger_than_any_frame_is_a_false_sync() {
    let mut walker = MoviWalker::new();
    walker.push(&[b'0', b'0', b'd', b'c', 0xF0, 0xFF, 0xFF, 0xFF]);
    walker.push(&[b'0', b'1', b'w', b'b', 2, 0, 0, 0, 7, 8]);
    assert_eq!(walker.next_chunk().map(|c| (c.stream_index, c.data)), Some((1, vec![7, 8])));
}

#[test]
fn a_chunk_header_straddling_two_pushes_survives_a_resync() {
    let mut walker = MoviWalker::new();
    walker.push(&[0xEE, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE, b'0', b'0', b'd', b'c', 2, 0]);
    assert!(walker.next_chunk().is_none());
    walker.push(&[0, 0, 0xAB, 0xCD]);
    assert_eq!(walker.next_chunk().map(|c| c.data), Some(vec![0xAB, 0xCD]));
}

#[test]
fn streams_past_the_two_digit_limit_are_not_discovered() {
    let stream = list(b"strl", &strh(b"auds", &[0; 4], 1, 8_000, 1, 0));
    let file = |count: usize| {
        let hdrl = list(b"hdrl", &stream.repeat(count));
        chunk(b"RIFF", &[b"AVI ".to_vec(), hdrl].concat())
    };
    assert_eq!(probe_header(&file(100)).map(|(h, _)| h.streams.len()).ok(), Some(100));
    assert_eq!(probe_header(&file(300)).map(|(h, _)| h.streams.len()).ok(), Some(100));
}
