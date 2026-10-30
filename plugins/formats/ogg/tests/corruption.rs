//! The untrusted-input gate.

// Test code builds its inputs on the heap.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use pf_ogg::page::{HEADER_FIXED_LEN, PageHeader, page_crc};
use pf_ogg::{
    OggDemux, OggMux, OggReader, OggWriter, demux_all, last_granule, mux_packets, opus_duration_ns,
    parse_comment_body, parse_opus_head, parse_opus_tags, parse_speex_head, parse_vorbis_comments,
    parse_vorbis_ident, vorbis_duration_ns,
};
use pf_sim::corrupt::harness::{
    Fed, collect, feed_bytes, input_buffer, pool_allowance, pool_config, settle,
};
use pf_sim::corrupt::{
    Case, Chunking, EXTREMES, Op, Site, Tracking, Violation, apply, check_heap, check_output,
    check_steps, gate, heap_mark, mutate, noise, prefix_then_garbage, sweep_gate,
};
use pf_sim::prng::Prng;
use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

#[global_allocator]
static HEAP: Tracking = Tracking;

fn opus_head() -> Vec<u8> {
    let mut packet = b"OpusHead".to_vec();
    packet.extend_from_slice(&[1, 2, 0x38, 0x01]);
    packet.extend_from_slice(&48_000u32.to_le_bytes());
    packet.extend_from_slice(&[0, 0, 0]);
    packet
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let v = chunk.iter().fold(0u32, |v, &b| (v << 8) | b as u32) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            match i <= chunk.len() {
                true => out.push(ALPHABET[(v >> (18 - 6 * i) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

fn picture_block() -> Vec<u8> {
    let mut block = 3u32.to_be_bytes().to_vec();
    block.extend_from_slice(&10u32.to_be_bytes());
    block.extend_from_slice(b"image/jpeg");
    block.extend_from_slice(&5u32.to_be_bytes());
    block.extend_from_slice(b"cover");
    block.extend_from_slice(&[0u8; 16]);
    block.extend_from_slice(&7u32.to_be_bytes());
    block.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]);
    block
}

fn comment_body() -> Vec<u8> {
    let picture = format!("METADATA_BLOCK_PICTURE={}", base64(&picture_block()));
    let entries = [
        "title=a title",
        "ARTIST=one",
        "ARTIST=two",
        "a_rather_long_field_name_past_the_stack_buffer_0123456789=v",
        &picture,
    ];
    let mut body = 6u32.to_le_bytes().to_vec();
    body.extend_from_slice(b"vendor");
    body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        body.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        body.extend_from_slice(entry.as_bytes());
    }
    body
}

fn opus_tags() -> Vec<u8> {
    [b"OpusTags".to_vec(), comment_body()].concat()
}

fn single_base() -> Vec<u8> {
    let (head, tags) = (opus_head(), opus_tags());
    let sizes = [0usize, 1, 17, 254, 255, 256, 510, 700, 3, 40];
    let payloads: Vec<Vec<u8>> = sizes.iter().map(|&n| (0..n).map(|i| i as u8).collect()).collect();
    let mut packets: Vec<(&[u8], u64)> = vec![(&head, 0), (&tags, 0)];
    for (i, payload) in payloads.iter().enumerate() {
        packets.push((payload, 960 * (i as u64 + 1)));
    }
    mux_packets(0x1111, &packets)
}

fn multiplexed_base() -> Vec<u8> {
    let mut out = Vec::new();
    let mut a = OggWriter::new(0xAAAA);
    let mut b = OggWriter::new(0xBBBB);
    for round in 0..6u64 {
        for (writer, fill) in [(&mut a, 0xA0u8), (&mut b, 0xB0)] {
            let packet = vec![fill | round as u8; 20 + 90 * round as usize];
            writer.write_packet(&mut out, &packet, round * 100).expect("open stream");
            writer.flush(&mut out);
        }
    }
    a.finish(&mut out);
    let mut c = OggWriter::new(0xCCCC);
    c.write_packet(&mut out, &opus_head(), 0).expect("open stream");
    c.flush(&mut out);
    c.write_packet(&mut out, &[0xC0; 300], 960).expect("open stream");
    c.finish(&mut out);
    b.finish(&mut out);
    out
}

fn spanning_base() -> Vec<u8> {
    let big: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
    mux_packets(0x5555, &[(&opus_head(), 0), (&big, 960), (&[1, 2, 3], 1920)])
}

fn page_sites(data: &[u8]) -> Vec<Site> {
    let mut sites = Vec::new();
    let mut at = 0;
    while let Ok(page) = PageHeader::parse(&data[at..]) {
        sites.push(Site::boundary(at));
        sites.push(Site::le(at + 4, 1));
        sites.push(Site::le(at + 5, 1));
        sites.push(Site::le(at + 6, 8));
        sites.push(Site::le(at + 14, 4));
        sites.push(Site::le(at + 18, 4));
        sites.push(Site::le(at + 22, 4));
        sites.push(Site::le(at + 26, 1));
        for lacing in 0..page.n_segments().min(6) {
            sites.push(Site::le(at + HEADER_FIXED_LEN + lacing, 1));
        }
        sites.push(Site::boundary(at + HEADER_FIXED_LEN + page.n_segments()));
        at += page.len();
    }
    sites
}

struct Base {
    name: &'static str,
    data: Vec<u8>,
    sites: Vec<Site>,
}

fn stream_bases() -> Vec<Base> {
    [("single", single_base()), ("multiplexed", multiplexed_base()), ("spanning", spanning_base())]
        .into_iter()
        .map(|(name, data)| Base { name, sites: page_sites(&data), data })
        .collect()
}

fn repair_checksums(data: &mut Vec<u8>, ops: &mut Vec<Op>) {
    let mut at = 0;
    while at + HEADER_FIXED_LEN <= data.len() {
        let page_len = (data[at..at + 4] == *b"OggS")
            .then(|| {
                let table_end = at + HEADER_FIXED_LEN + data[at + 26] as usize;
                let table = data.get(at + HEADER_FIXED_LEN..table_end)?;
                let len = table_end - at + table.iter().map(|&b| b as usize).sum::<usize>();
                (at + len <= data.len()).then_some(len)
            })
            .flatten();
        let Some(len) = page_len else {
            at += 1;
            continue;
        };
        let crc = page_crc(&data[at..at + len]).to_le_bytes();
        if data[at + 22..at + 26] != crc {
            let op = Op::Splice { at: at + 22, remove: 4, bytes: crc.to_vec() };
            apply(data, &op);
            ops.push(op);
        }
        at += len;
    }
}

fn stream_case(prng: &mut Prng, bases: &[Base]) -> Case {
    let base = &bases[[0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 2][prng.below(15) as usize]];
    let (mut data, mut ops) = match prng.below(20) {
        0 => noise(prng, 512),
        1 | 2 => prefix_then_garbage(prng, &base.data, &base.sites, 256),
        _ => mutate(prng, &base.data, &base.sites),
    };
    if prng.chance(75) {
        repair_checksums(&mut data, &mut ops);
    }
    let chunking = Chunking::pick(prng, data.len());
    Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
}

fn repaired_sweep(
    name: &str,
    base: &Base,
    budget: u64,
    drive: &dyn Fn(&Case) -> Result<(), Violation>,
) {
    sweep_gate(name, base.name, &base.data, &base.sites, budget, &|case| {
        drive(case)?;
        let mut repaired = case.clone();
        repair_checksums(&mut repaired.data, &mut repaired.ops);
        drive(&repaired)
    });
}

const READER_HEAP_PER_BYTE: usize = 4;
const READER_HEAP_SLACK: usize = 96 << 10;

fn drive_reader(case: &Case) -> Result<(), Violation> {
    let chunks = case.chunking.split(&case.data);
    let mark = heap_mark();
    let mut reader = OggReader::new();
    let reset_at = (case.knob & 1 == 1).then(|| (case.knob >> 8) as usize % chunks.len());
    let recycle = case.knob & 2 != 0;
    let (mut packets, mut bytes) = (0u64, 0usize);
    let mut drain = |reader: &mut OggReader| {
        while let Some(packet) = reader.next_packet() {
            packets += 1;
            bytes += packet.data.len();
            if recycle {
                reader.recycle(packet.data);
            }
        }
    };
    for (index, chunk) in chunks.iter().enumerate() {
        if reset_at == Some(index) {
            reader.reset();
        }
        reader.push(chunk);
        drain(&mut reader);
    }
    let leftover = reader.finish();
    drain(&mut reader);
    check_output(leftover, case.data.len())?;
    check_steps(packets, case.data.len() as u64)?;
    check_steps(reader.pages_parsed(), case.data.len() as u64 / HEADER_FIXED_LEN as u64)?;
    check_steps(reader.resync_count(), case.data.len() as u64 + 1)?;
    check_output(bytes, case.data.len())?;
    check_heap(mark, READER_HEAP_PER_BYTE * case.data.len() + READER_HEAP_SLACK)?;

    if case.knob & 4 != 0 {
        let all = demux_all(&case.data);
        check_output(all.iter().map(|p| p.data.len()).sum(), case.data.len())?;
    }
    Ok(())
}

#[test]
fn reader_survives_corrupt_streams() {
    let bases = stream_bases();
    gate("ogg reader", 2_000, &|prng| stream_case(prng, &bases), &drive_reader);
}

#[test]
fn reader_survives_every_truncation_and_extreme_field() {
    for base in &stream_bases() {
        let budget = if base.data.len() > 16 << 10 { 30 } else { 400 };
        repaired_sweep("ogg reader sweep", base, budget, &drive_reader);
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

fn drive_slices(case: &Case) -> Result<(), Violation> {
    let mark = heap_mark();
    let scratch = Arena::default();
    let mut offsets = vec![0usize];
    for chunk in case.chunking.chunks(&case.data).take(31) {
        offsets.push(offsets[offsets.len() - 1] + chunk.len());
    }
    for &offset in &offsets {
        let window = &case.data[offset.min(case.data.len())..];
        if let Ok(page) = PageHeader::parse(window) {
            if page.len() > window.len() || page.segment_table().len() != page.n_segments() {
                return Err(Violation::Invariant("a page larger than its window"));
            }
            let fields = (page.header_type(), page.granule_position(), page.serial());
            let _ = (fields, page.sequence(), page.crc(), page.payload().len(), page.raw().len());
            let _ = (page.is_bos(), page.is_eos(), page.is_continued(), page.is_empty());
        }
        let serial = (case.knob & 1 == 1).then_some((case.knob >> 32) as u32);
        let granule = last_granule(window, serial);
        let _ = opus_duration_ns(granule.unwrap_or(case.knob), (case.knob >> 16) as u16);
        let _ = vorbis_duration_ns(granule.unwrap_or(case.knob), (case.knob >> 20) as u32);
        let _ = (parse_opus_head(window), parse_vorbis_ident(window), parse_speex_head(window));

        let mut sink = CountingSink::default();
        parse_comment_body(window, &scratch, &mut sink);
        let _ = parse_opus_tags(window, &scratch, &mut sink);
        let _ = parse_vorbis_comments(window, &scratch, &mut sink);
        check_output(sink.bytes, 3 * window.len())?;
        check_output(sink.entries, 3 * (window.len() / 4 + 1))?;
    }
    for extreme in EXTREMES {
        let _ = opus_duration_ns(extreme, extreme as u16);
        let _ = vorbis_duration_ns(extreme, extreme as u32);
    }
    check_heap(mark, 96 * case.data.len() + 3 * (64 << 10))
}

fn slice_bases() -> Vec<Base> {
    let mut vorbis_ident = b"\x01vorbis".to_vec();
    vorbis_ident.extend_from_slice(&[0, 0, 0, 0, 2]);
    vorbis_ident.extend_from_slice(&44_100u32.to_le_bytes());
    vorbis_ident.extend_from_slice(&[0u8; 14]);
    let mut speex = b"Speex   ".to_vec();
    speex.extend_from_slice(&[0u8; 28]);
    speex.extend_from_slice(&16_000u32.to_le_bytes());
    speex.extend_from_slice(&[0u8; 8]);
    speex.extend_from_slice(&1u32.to_le_bytes());
    speex.extend_from_slice(&[0u8; 28]);
    let comments = comment_body();
    let comment_sites = vec![Site::le(0, 4), Site::le(10, 4), Site::le(14, 4), Site::le(31, 4)];
    let shifted = |by: usize| comment_sites.iter().map(|s| Site::le(s.at + by, 4)).collect();
    let mut bases = vec![
        Base {
            name: "OpusHead",
            data: opus_head(),
            sites: vec![Site::le(8, 1), Site::le(9, 1), Site::le(12, 4)],
        },
        Base {
            name: "vorbis ident",
            data: vorbis_ident,
            sites: vec![Site::le(7, 4), Site::le(11, 1), Site::le(12, 4)],
        },
        Base { name: "Speex", data: speex, sites: vec![Site::le(36, 4), Site::le(48, 4)] },
        Base { name: "OpusTags", data: opus_tags(), sites: shifted(8) },
        Base {
            name: "vorbis comment",
            data: [b"\x03vorbis".to_vec(), comments.clone()].concat(),
            sites: shifted(7),
        },
        Base { name: "comment body", data: comments, sites: comment_sites },
        Base {
            name: "picture",
            data: picture_block(),
            sites: vec![Site::be(4, 4), Site::be(18, 4), Site::be(43, 4)],
        },
    ];
    bases.extend(stream_bases().into_iter().filter(|b| b.name != "spanning"));
    bases
}

#[test]
fn slice_parsers_survive_corrupt_headers() {
    let bases = slice_bases();
    let make = |prng: &mut Prng| {
        let base = &bases[prng.below(bases.len() as u64) as usize];
        let (mut data, mut ops) = match prng.below(12) {
            0 => noise(prng, 256),
            1 => prefix_then_garbage(prng, &base.data, &base.sites, 128),
            _ => mutate(prng, &base.data, &base.sites),
        };
        if prng.chance(50) {
            repair_checksums(&mut data, &mut ops);
        }
        let chunking = Chunking::pick(prng, data.len());
        Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
    };
    gate("ogg slices", 3_000, &make, &drive_slices);
    for base in &bases {
        repaired_sweep("ogg slices sweep", base, 80, &drive_slices);
    }
}

fn drive_demux(case: &Case) -> Result<(), Violation> {
    let mark = heap_mark();
    let chunks = case.chunking.count(&case.data);
    let flush_before = (case.knob >> 5 & 3 == 0).then(|| (case.knob >> 8) as usize % chunks);
    let fed = feed_bytes(OggDemux::new(), case, flush_before, 0)?;
    check_output(fed.out_bytes, case.data.len())?;
    check_output(fed.out_buffers, 2 * case.data.len() + 1)?;
    check_heap(mark, (READER_HEAP_PER_BYTE + 2) * case.data.len() + pool_allowance(case.knob))
}

#[test]
fn demux_element_survives_corrupt_streams() {
    let bases = stream_bases();
    gate("ogg demux", 500, &|prng| stream_case(prng, &bases), &drive_demux);
}

#[test]
fn demux_element_survives_every_truncation_and_extreme_field() {
    for base in &stream_bases() {
        let budget = if base.data.len() > 16 << 10 { 20 } else { 120 };
        repaired_sweep("ogg demux sweep", base, budget, &drive_demux);
    }
}

fn drive_mux(case: &Case) -> Result<(), Violation> {
    let mark = heap_mark();
    let mut harness =
        Harness::with_pool_config(OggMux::with_serial(case.knob as u32), pool_config(case.knob));
    let mut fed = Fed::default();
    let packets = case.chunking.split(&case.data);
    let mut stream = Vec::new();
    let mut take = |harness: &mut Harness, fed: &mut Fed| {
        for buffer in harness.drain_outputs() {
            fed.out_bytes += buffer.memory.len();
            stream.extend_from_slice(buffer.memory.data());
        }
    };
    let mut limit = 64u64;
    for packet in &packets {
        fed.steps += 1;
        if harness.push(input_buffer(packet)).is_err() {
            return Err(Violation::Invariant("oggmux failed on a plain packet"));
        }
        take(&mut harness, &mut fed);
        limit += 16 + packet.len() as u64;
        settle(&mut harness, &mut fed, limit)?;
    }
    match harness.eos() {
        Ok(tail) => {
            for buffer in tail {
                fed.out_bytes += buffer.memory.len();
                stream.extend_from_slice(buffer.memory.data());
            }
        }
        Err(_) => return Err(Violation::Invariant("oggmux failed at end of stream")),
    }
    collect(&mut harness, &mut fed);
    let n = case.data.len();
    let pages = packets.len() + n / (255 * 255) + 2;
    check_output(fed.out_bytes, n + n / 255 + packets.len() + 27 * pages)?;

    let back = demux_all(&stream);
    if back.len() != packets.len() || back.iter().zip(&packets).any(|(got, want)| got.data != *want)
    {
        return Err(Violation::Invariant("oggmux output did not demux to its input"));
    }
    check_heap(mark, 8 * n + 160 * packets.len() + pool_allowance(case.knob) + (64 << 10))
}

#[test]
fn mux_element_bounds_and_round_trips_arbitrary_packets() {
    let make = |prng: &mut Prng| {
        let (data, ops) = match prng.below(12) {
            0 => noise(prng, 70_000),
            _ => noise(prng, 2_000),
        };
        let chunking = match prng.below(4) {
            0 => Chunking::Random { max: 300, seed: prng.next_u64() },
            1 => Chunking::Random { max: 70_000, seed: prng.next_u64() },
            _ => Chunking::pick(prng, data.len()),
        };
        Case { base: "noise", ops, data, chunking, knob: prng.next_u64() }
    };
    gate("ogg mux", 400, &make, &drive_mux);
}
