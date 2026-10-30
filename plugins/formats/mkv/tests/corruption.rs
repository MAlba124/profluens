//! The untrusted-input gate. Seeded corruptions of well-formed Matroska must not panic, must
//! return in bounded steps, and must not grow heap or output past a multiple of the input.

// Test code builds its inputs on the heap.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use pf_mkv::ebml::{self, id};
use pf_mkv::tags::{
    audio_format, element_extent, index_segment, info_duration_ns, parse_attachments, parse_tags,
};
use pf_mkv::{
    MatroskaReader, MatroskaWriter, MkvDemux, MkvMux, MkvMuxN, Reframer, TrackConfig,
    nal_head_from_config, parse_cues, parse_seek_head,
};
use pf_sim::corrupt::harness::{
    Fed, collect, feed_bytes, input_buffer, pool_allowance, pool_config, settle,
};
use pf_sim::corrupt::{
    Case, Chunking, EXTREMES, Op, Site, Tracking, Violation, check_heap, check_output, check_steps,
    corrupt, gate, heap_growth, heap_mark, mutate, noise, prefix_then_garbage, sweep_gate,
};
use pf_sim::prng::Prng;
use profluens_core::buffer::BufferFlags;
use profluens_core::event::{Event, TagSink};
use profluens_core::format::ValueDesc;
use profluens_core::time::Timestamp;

#[global_allocator]
static HEAP: Tracking = Tracking;

fn element(element_id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    ebml::write_id(&mut out, element_id);
    ebml::write_size(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

fn uint(element_id: &[u8], value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    ebml::write_uint(&mut out, element_id, value);
    out
}

fn text(element_id: &[u8], value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    ebml::write_string(&mut out, element_id, value);
    out
}

fn flac_head() -> Vec<u8> {
    let mut head = b"fLaC".to_vec();
    head.extend_from_slice(&[0x80, 0, 0, 34]);
    let mut info = [0u8; 34];
    info[..4].copy_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    info[10..14].copy_from_slice(&[0x0A, 0xC4, 0x42, 0xF0]);
    head.extend_from_slice(&info);
    head
}

fn avcc() -> Vec<u8> {
    let mut record = vec![1, 0x64, 0x00, 0x1F, 0xFF, 0xE1];
    record.extend_from_slice(&[0x00, 0x04, 0x67, 0x64, 0x00, 0x1F]);
    record.extend_from_slice(&[0x01, 0x00, 0x02, 0x68, 0xEE]);
    record
}

fn hvcc() -> Vec<u8> {
    let mut record = vec![1];
    let mut fixed = [0u8; 21];
    fixed[20] = 0xFF;
    record.extend_from_slice(&fixed);
    record.push(3);
    for (kind, nal) in
        [(32u8, &[0x40u8, 0x01, 0x0C][..]), (33, &[0x42, 0x01]), (34, &[0x44, 0x01, 0xC0])]
    {
        record.push(0x80 | kind);
        record.extend_from_slice(&1u16.to_be_bytes());
        record.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        record.extend_from_slice(nal);
    }
    record
}

fn nal_sample(tag: u8) -> Vec<u8> {
    vec![0, 0, 0, 3, 0x65, tag, 0x80, 0, 0, 0, 2, 0x41, tag]
}

fn written_base() -> Vec<u8> {
    let mut flac = TrackConfig::flac(2, flac_head(), 44_100.0, 2, 16);
    flac.codec_delay_ns = 7_200_000_000_000;
    let tracks = vec![
        TrackConfig::video(1, "V_MPEG4/ISO/AVC", avcc(), 320, 240),
        flac,
        TrackConfig::video(3, "V_MPEGH/ISO/HEVC", hvcc(), 320, 240),
        TrackConfig::vp8(4, 320, 240),
    ];
    let mut writer = MatroskaWriter::new(tracks);
    writer.enable_cues();
    writer.set_duration_ns(480_000_000);
    let mut out = Vec::new();
    writer.write_header(&mut out).expect("header");
    for i in 0..24u64 {
        let track = 1 + i % 4;
        let payload = match track {
            1 | 3 => nal_sample(i as u8),
            _ => vec![i as u8; 5 + i as usize],
        };
        writer.write_frame(&mut out, track, i * 20_000_000, &payload, i % 8 < 4).expect("frame");
    }
    writer.finalize(&mut out);
    out
}

fn block(track: u8, rel: i16, flags: u8, rest: &[u8]) -> Vec<u8> {
    let mut body = vec![0x80 | track];
    body.extend_from_slice(&rel.to_be_bytes());
    body.push(flags);
    body.extend_from_slice(rest);
    body
}

fn handmade_base() -> Vec<u8> {
    let mut out =
        element(id::EBML, &[uint(id::EBML_VERSION, 1), text(id::DOC_TYPE, "matroska")].concat());
    ebml::write_id(&mut out, id::SEGMENT);
    ebml::write_unknown_size(&mut out);

    let seek = |target: &[u8], pos: u64| {
        element(id::SEEK, &[element(id::SEEK_ID, target), uint(id::SEEK_POSITION, pos)].concat())
    };
    out.extend(element(
        id::SEEK_HEAD,
        &[seek(id::CUES, 400), seek(id::TAGS, 90), seek(id::INFO, 60), seek(id::SEEK_HEAD, 0)]
            .concat(),
    ));

    let mut duration = Vec::new();
    ebml::write_f64(&mut duration, id::DURATION, 1234.5);
    out.extend(element(id::INFO, &[uint(id::TIMESTAMP_SCALE, 1_000_000), duration].concat()));

    let mut rate = Vec::new();
    ebml::write_f64(&mut rate, id::SAMPLING_FREQUENCY, 44_100.0);
    let audio = element(
        id::TRACK_ENTRY,
        &[
            uint(id::TRACK_NUMBER, 1),
            uint(id::TRACK_TYPE, 2),
            text(id::CODEC_ID, "A_FLAC"),
            element(id::CODEC_PRIVATE, &flac_head()),
            element(id::AUDIO, &[rate, uint(id::CHANNELS, 2), uint(id::BIT_DEPTH, 16)].concat()),
        ]
        .concat(),
    );
    let colour = element(
        id::COLOUR,
        &[uint(id::MATRIX_COEFFICIENTS, 1), uint(id::COLOUR_RANGE, 1), uint(id::PRIMARIES, 1)]
            .concat(),
    );
    let video = element(
        id::TRACK_ENTRY,
        &[
            uint(id::TRACK_NUMBER, 2),
            uint(id::TRACK_TYPE, 1),
            text(id::CODEC_ID, "V_MPEG4/ISO/AVC"),
            element(id::CODEC_PRIVATE, &avcc()),
            element(
                id::VIDEO,
                &[uint(id::PIXEL_WIDTH, 320), uint(id::PIXEL_HEIGHT, 240), colour].concat(),
            ),
        ]
        .concat(),
    );
    out.extend(element(id::TRACKS, &[audio, video].concat()));

    let simple_tag = |name: &str, value: &str, nested: &[u8]| {
        element(
            id::SIMPLE_TAG,
            &[text(id::TAG_NAME, name), text(id::TAG_STRING, value), nested.to_vec()].concat(),
        )
    };
    let track_tag = element(
        id::TAG,
        &[
            element(id::TARGETS, &uint(id::TARGET_TYPE_VALUE, 30)),
            simple_tag("TITLE", "a title", &simple_tag("sort_with", "title, a", &[])),
            simple_tag("PART_NUMBER", "3", &[]),
        ]
        .concat(),
    );
    let album_tag = element(
        id::TAG,
        &[
            element(id::TARGETS, &uint(id::TARGET_TYPE_VALUE, 50)),
            simple_tag("TITLE", "an album", &[]),
            simple_tag("DATE_RELEASED", "2001", &[]),
        ]
        .concat(),
    );
    out.extend(element(id::TAGS, &[track_tag, album_tag].concat()));

    let attached = |name: &str, mime: &str, data: &[u8]| {
        element(
            id::ATTACHED_FILE,
            &[
                text(id::FILE_NAME, name),
                text(id::FILE_MEDIA_TYPE, mime),
                element(id::FILE_DATA, data),
            ]
            .concat(),
        )
    };
    out.extend(element(
        id::ATTACHMENTS,
        &[
            attached("font.ttf", "font/ttf", &[1, 2, 3]),
            attached("cover_land.JPG", "image/jpeg", &[0xFF, 0xD8, 0xFF, 0xE0, 9, 9]),
        ]
        .concat(),
    ));

    ebml::write_id(&mut out, id::CLUSTER);
    ebml::write_unknown_size(&mut out);
    out.extend(uint(id::TIMESTAMP, 10));
    out.extend(element(id::SIMPLE_BLOCK, &block(1, 0, 0x80, &[0xAA; 6])));
    out.extend(element(
        id::SIMPLE_BLOCK,
        &block(1, 1, 0x82, &[2, 3, 2, 1, 1, 1, 2, 2, 3, 3, 3, 3]),
    ));
    out.extend(element(
        id::SIMPLE_BLOCK,
        &block(1, 2, 0x86, &[2, 0x82, 0xC0, 4, 4, 5, 5, 5, 6, 6]),
    ));
    out.extend(element(id::SIMPLE_BLOCK, &block(1, 3, 0x84, &[2, 7, 7, 8, 8, 9, 9])));
    out.extend(element(id::SIMPLE_BLOCK, &block(2, 4, 0x80, &nal_sample(1))));
    out.extend(element(
        id::BLOCK_GROUP,
        &[
            element(id::BLOCK, &block(2, 5, 0x00, &nal_sample(2))),
            uint(id::BLOCK_DURATION, 40),
            element(id::REFERENCE_BLOCK, &[0xFF]),
        ]
        .concat(),
    ));

    let sized = [
        uint(id::TIMESTAMP, 100),
        element(id::SIMPLE_BLOCK, &block(2, 0, 0x80, &nal_sample(3))),
        element(id::BLOCK_GROUP, &element(id::BLOCK, &block(1, 1, 0x00, &[0xBB; 9]))),
    ]
    .concat();
    out.extend(element(id::CLUSTER, &sized));
    let cue = |time: u64, pos: u64| {
        element(
            id::CUE_POINT,
            &[
                uint(id::CUE_TIME, time),
                element(
                    id::CUE_TRACK_POSITIONS,
                    &[uint(id::CUE_TRACK, 2), uint(id::CUE_CLUSTER_POSITION, pos)].concat(),
                ),
            ]
            .concat(),
        )
    };
    out.extend(element(id::CUES, &[cue(10, 300), cue(100, 380)].concat()));
    out
}

fn coded_base() -> Vec<u8> {
    let mut out = element(id::EBML, &text(id::DOC_TYPE, "matroska"));
    ebml::write_id(&mut out, id::SEGMENT);
    ebml::write_unknown_size(&mut out);
    out.extend(element(id::INFO, &uint(id::TIMESTAMP_SCALE, 1_000_000)));
    let encoding = |scope: u64, algo: u64, settings: &[u8]| {
        let comp =
            [uint(id::CONTENT_COMP_ALGO, algo), element(id::CONTENT_COMP_SETTINGS, settings)]
                .concat();
        let one =
            [uint(id::CONTENT_ENCODING_SCOPE, scope), element(id::CONTENT_COMPRESSION, &comp)]
                .concat();
        element(id::CONTENT_ENCODINGS, &element(id::CONTENT_ENCODING, &one))
    };
    let hi = [0x78, 0x01, 0x01, 0x02, 0x00, 0xFD, 0xFF, b'h', b'i', 0x01, 0x3B, 0x00, 0xD2];
    let ac3 = element(
        id::TRACK_ENTRY,
        &[
            uint(id::TRACK_NUMBER, 1),
            uint(id::TRACK_TYPE, 2),
            text(id::CODEC_ID, "A_AC3"),
            uint(id::DEFAULT_DURATION, 32_000_000),
            encoding(1, 3, &[0x0B, 0x77]),
        ]
        .concat(),
    );
    let ass = element(
        id::TRACK_ENTRY,
        &[
            uint(id::TRACK_NUMBER, 2),
            uint(id::TRACK_TYPE, 17),
            text(id::CODEC_ID, "S_TEXT/ASS"),
            element(id::CODEC_PRIVATE, &hi),
            encoding(3, 0, &[]),
        ]
        .concat(),
    );
    out.extend(element(id::TRACKS, &[ac3, ass].concat()));
    let atom = |uid: u64, start: u64| {
        let display = element(
            id::CHAPTER_DISPLAY,
            &[text(id::CHAP_STRING, "a"), text(id::CHAP_LANGUAGE, "eng")].concat(),
        );
        element(
            id::CHAPTER_ATOM,
            &[uint(id::CHAPTER_UID, uid), uint(id::CHAPTER_TIME_START, start), display].concat(),
        )
    };
    let nested = element(id::CHAPTER_ATOM, &[uint(id::CHAPTER_UID, 3), atom(4, 5)].concat());
    out.extend(element(
        id::CHAPTERS,
        &element(id::EDITION_ENTRY, &[atom(1, 0), atom(2, 10), nested].concat()),
    ));
    ebml::write_id(&mut out, id::CLUSTER);
    ebml::write_unknown_size(&mut out);
    out.extend(uint(id::TIMESTAMP, 0));
    for k in 0..4i16 {
        out.extend(element(
            id::SIMPLE_BLOCK,
            &block(1, k * 96, 0x82, &[2, 3, 2, 1, 1, 1, 2, 2, 3, 3, 3, 3]),
        ));
        out.extend(element(
            id::BLOCK_GROUP,
            &[element(id::BLOCK, &block(2, k * 96 + 5, 0x00, &hi)), uint(id::BLOCK_DURATION, 40)]
                .concat(),
        ));
    }
    let attached = [
        text(id::FILE_NAME, "f.ttf"),
        text(id::FILE_MEDIA_TYPE, "font/ttf"),
        element(id::FILE_DATA, &[1, 2]),
    ];
    out.extend(element(id::ATTACHMENTS, &element(id::ATTACHED_FILE, &attached.concat())));
    out
}

const MASTERS: [&[u8]; 28] = [
    id::EBML,
    id::SEGMENT,
    id::INFO,
    id::TRACKS,
    id::TRACK_ENTRY,
    id::AUDIO,
    id::VIDEO,
    id::COLOUR,
    id::CLUSTER,
    id::BLOCK_GROUP,
    id::SEEK_HEAD,
    id::SEEK,
    id::CUES,
    id::CUE_POINT,
    id::CUE_TRACK_POSITIONS,
    id::TAGS,
    id::TAG,
    id::TARGETS,
    id::SIMPLE_TAG,
    id::ATTACHMENTS,
    id::ATTACHED_FILE,
    id::CONTENT_ENCODINGS,
    id::CONTENT_ENCODING,
    id::CONTENT_COMPRESSION,
    id::CHAPTERS,
    id::EDITION_ENTRY,
    id::CHAPTER_ATOM,
    id::CHAPTER_DISPLAY,
];

fn ebml_sites(data: &[u8]) -> Vec<Site> {
    fn walk(data: &[u8], mut at: usize, end: usize, depth: usize, sites: &mut Vec<Site>) {
        while at < end {
            let Ok(header) = ebml::read_element_header(data, at) else {
                return;
            };
            let id_len = header.id.len();
            sites.push(Site::boundary(at));
            sites.push(Site::be(at + id_len, (header.data_start - at - id_len) as u8));
            let data_end = match header.data_end() {
                Ok(Some(e)) => e.min(end),
                _ => end,
            };
            if MASTERS.contains(&header.id) && depth < 8 {
                walk(data, header.data_start, data_end, depth + 1, sites);
            } else if header.id == id::SIMPLE_BLOCK || header.id == id::BLOCK {
                sites.push(Site::be(header.data_start + 1, 2));
                sites.push(Site::be(header.data_start + 3, 1));
                sites.push(Site::be(header.data_start + 4, 1));
            }
            at = data_end.max(header.data_start);
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
    let sites = ebml_sites(&data);
    Base { name, data, sites }
}

fn stream_bases() -> Vec<Base> {
    vec![
        base("written", written_base()),
        base("handmade", handmade_base()),
        base("coded", coded_base()),
    ]
}

fn hostile_vint(prng: &mut Prng) -> Vec<u8> {
    match prng.below(4) {
        0 => vec![0xFF],
        1 => vec![0x00],
        2 => vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        _ => {
            let value = EXTREMES[prng.below(EXTREMES.len() as u64) as usize] & ((1 << 56) - 1);
            let mut vint = value.to_be_bytes();
            vint[0] = 0x01;
            vint.to_vec()
        }
    }
}

fn stream_case(prng: &mut Prng, bases: &[Base]) -> Case {
    let base = &bases[prng.below(bases.len() as u64) as usize];
    let (data, ops) = match prng.below(20) {
        0 => noise(prng, 512),
        1 | 2 => prefix_then_garbage(prng, &base.data, &base.sites, 256),
        3..=5 => {
            let fields: Vec<&Site> = base.sites.iter().filter(|s| s.width > 0).collect();
            let site = fields[prng.below(fields.len() as u64) as usize];
            let mut ops = vec![Op::Splice {
                at: site.at,
                remove: site.width as usize,
                bytes: hostile_vint(prng),
            }];
            let mut data = corrupt(&base.data, &ops);
            if prng.chance(30) {
                let (more, extra) = mutate(prng, &data, &base.sites);
                data = more;
                ops.extend(extra);
            }
            (data, ops)
        }
        _ => mutate(prng, &base.data, &base.sites),
    };
    let chunking = Chunking::pick(prng, data.len());
    Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
}

/// A queued frame costs its `Frame` plus a 64 byte minimum buffer, and a lace can carry one frame
/// per octet.
const READER_HEAP_PER_BYTE: usize = 192;
const READER_HEAP_SLACK: usize = 16 << 10;

fn drive_reader(case: &Case) -> Result<(), Violation> {
    let chunks = case.chunking.split(&case.data);
    let mark = heap_mark();
    let mut reader = MatroskaReader::new();
    let resync_at = (case.knob & 1 == 1).then(|| (case.knob >> 8) as usize % chunks.len());
    let recycle = case.knob & 2 != 0;
    let resync_on_error = case.knob & 4 != 0;
    let (mut frames, mut bytes) = (0usize, 0usize);
    for (index, chunk) in chunks.iter().enumerate() {
        if resync_at == Some(index) {
            reader.resync_streaming();
        }
        let failed = reader.push(chunk).is_err();
        while let Some(frame) = reader.next_frame() {
            frames += 1;
            bytes += frame.data.len();
            if recycle {
                reader.recycle(frame.data);
            }
        }
        check_steps(frames as u64, case.data.len() as u64 + 1)?;
        if failed {
            if !resync_on_error {
                break;
            }
            reader.resync_streaming();
        }
    }
    let _ = (reader.tracks().len(), reader.duration_ns(), reader.timestamp_scale());
    check_output(bytes, case.data.len())?;
    check_heap(mark, READER_HEAP_PER_BYTE * case.data.len() + READER_HEAP_SLACK)
}

#[test]
fn reader_survives_corrupt_streams() {
    let bases = stream_bases();
    gate("mkv reader", 4_000, &|prng| stream_case(prng, &bases), &drive_reader);
}

#[test]
fn reader_survives_every_truncation_and_extreme_size() {
    for base in stream_bases() {
        sweep_gate("mkv reader sweep", base.name, &base.data, &base.sites, 1_500, &drive_reader);
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
    let mut offsets = vec![0usize];
    for chunk in case.chunking.chunks(&case.data).take(63) {
        offsets.push(offsets[offsets.len() - 1] + chunk.len());
    }
    for &offset in &offsets {
        let window = &case.data[offset.min(case.data.len())..];
        if let Some(info) = parse_seek_head(window) {
            let _ = info.cues_pos;
        }
        let cues = parse_cues(window, case.knob, case.knob >> 7);
        check_output(cues.len(), window.len() / 6 + 1)?;
        if let Some(index) = index_segment(window) {
            let _ = index.lowest();
        }
        let _ = element_extent(window);
        let _ = info_duration_ns(window);
        let _ = audio_format(window);

        let mut sink = CountingSink::default();
        parse_tags(window, &mut sink);
        check_output(sink.bytes, window.len() + 16 * sink.entries)?;
        check_output(sink.entries, window.len() / 4 + 1)?;

        let mut sink = CountingSink::default();
        parse_attachments(window, &mut sink);
        check_output(sink.bytes, window.len())?;

        let files = pf_mkv::parse_attachments(window);
        check_output(files.iter().map(|a| a.data.len() + a.name.len()).sum(), window.len())?;
        let chapters = pf_mkv::parse_chapters(window);
        check_output(chapters.chapters.len(), window.len() / 4 + 1)?;
    }
    check_heap(mark, 8 * case.data.len() + (64 << 10))
}

fn slice_bases() -> Vec<Base> {
    let stream = handmade_base();
    let mut bases = stream_bases();
    let mut at = 0;
    while let Ok(header) = ebml::read_element_header(&stream, at) {
        if header.id == id::SEGMENT {
            at = header.data_start;
            continue;
        }
        let Ok(Some(end)) = header.data_end() else {
            break;
        };
        let name = match header.id {
            i if i == id::SEEK_HEAD => "seekhead",
            i if i == id::INFO => "info",
            i if i == id::TRACKS => "tracks",
            i if i == id::TAGS => "tags",
            i if i == id::ATTACHMENTS => "attachments",
            i if i == id::CUES => "cues",
            i if i == id::CHAPTERS => "chapters",
            _ => "",
        };
        if !name.is_empty() {
            bases.push(base(name, stream[at..end.min(stream.len())].to_vec()));
        }
        at = end;
        if at >= stream.len() {
            break;
        }
    }
    bases
}

#[test]
fn slice_parsers_survive_corrupt_elements() {
    let bases = slice_bases();
    assert!(bases.len() >= 7, "the handmade base lost a metadata master");
    gate("mkv slices", 4_000, &|prng| stream_case(prng, &bases), &drive_slices);
}

#[test]
fn slice_parsers_survive_every_truncation_and_extreme_size() {
    for base in slice_bases() {
        sweep_gate("mkv slices sweep", base.name, &base.data, &base.sites, 600, &drive_slices);
    }
}

fn drive_codec(case: &Case) -> Result<(), Violation> {
    let mark = heap_mark();
    for is_hevc in [false, true] {
        if let Ok((head, length_size)) = nal_head_from_config(&case.data, is_hevc) {
            check_output(head.len(), 2 * case.data.len())?;
            if !(1..=4).contains(&length_size) {
                return Err(Violation::Invariant("NAL length size outside 1..=4"));
            }
        }
    }
    let mut out = Vec::new();
    for length_size in 1..=4 {
        let reframer = Reframer::Nal { length_size };
        if let Ok(bytes) = reframer.reframe_into(&case.data, &mut out) {
            check_output(bytes.len(), 3 * case.data.len())?;
        }
    }
    check_heap(mark, 8 * case.data.len() + (64 << 10))
}

#[test]
fn config_records_and_reframing_survive_corruption() {
    let bases = [
        Base {
            name: "avcC",
            data: avcc(),
            sites: vec![
                Site::be(4, 1),
                Site::be(5, 1),
                Site::be(6, 2),
                Site::be(12, 1),
                Site::be(13, 2),
            ],
        },
        Base {
            name: "hvcC",
            data: hvcc(),
            sites: vec![Site::be(21, 1), Site::be(22, 1), Site::be(24, 2), Site::be(26, 2)],
        },
        Base { name: "sample", data: nal_sample(7), sites: vec![Site::be(0, 4), Site::be(7, 4)] },
    ];
    let make = |prng: &mut Prng| {
        let base = &bases[prng.below(bases.len() as u64) as usize];
        let (data, ops) = match prng.below(10) {
            0 => noise(prng, 128),
            1 => prefix_then_garbage(prng, &base.data, &base.sites, 64),
            _ => mutate(prng, &base.data, &base.sites),
        };
        Case { base: base.name, ops, data, chunking: Chunking::Whole, knob: 0 }
    };
    gate("mkv codec", 6_000, &make, &drive_codec);
    for base in &bases {
        sweep_gate("mkv codec sweep", base.name, &base.data, &base.sites, 400, &drive_codec);
    }
}

fn header_prefix(stream: &[u8]) -> Vec<u8> {
    let end = stream.windows(4).position(|w| w == id::CLUSTER).unwrap_or(stream.len());
    stream[..end].to_vec()
}

fn drive_demux(case: &Case, pristine_headers: &[(&'static str, Vec<u8>)]) -> Result<(), Violation> {
    let mark = heap_mark();
    let header = match case.knob >> 4 & 1 {
        0 => header_prefix(&case.data),
        _ => pristine_headers
            .iter()
            .find(|(name, _)| *name == case.base)
            .map_or_else(|| pristine_headers[0].1.clone(), |(_, header)| header.clone()),
    };
    let header_len = header.len();
    let chunks = case.chunking.count(&case.data);
    let flush_before = (case.knob >> 5 & 3 == 0).then(|| (case.knob >> 8) as usize % chunks);
    let fed = feed_bytes(MkvDemux::new(header), case, flush_before, 4 * header_len)?;
    // Reframing grows a block at most 2.5 times, and a codec head goes out at the start and after
    // the flush.
    check_output(fed.out_bytes, 3 * case.data.len() + 4 * header_len + 64)?;
    check_heap(
        mark,
        READER_HEAP_PER_BYTE * case.data.len() + 8 * header_len + pool_allowance(case.knob),
    )
}

#[test]
fn demux_element_survives_corrupt_streams() {
    let bases = stream_bases();
    let headers: Vec<(&'static str, Vec<u8>)> =
        bases.iter().map(|b| (b.name, header_prefix(&b.data))).collect();
    gate("mkv demux", 2_500, &|prng| stream_case(prng, &bases), &|case| {
        drive_demux(case, &headers)
    });
}

#[test]
fn demux_element_survives_every_truncation_and_extreme_size() {
    let bases = stream_bases();
    let headers: Vec<(&'static str, Vec<u8>)> =
        bases.iter().map(|b| (b.name, header_prefix(&b.data))).collect();
    for base in &bases {
        sweep_gate("mkv demux sweep", base.name, &base.data, &base.sites, 600, &|case| {
            drive_demux(case, &headers)
        });
    }
}

struct MuxBase {
    name: &'static str,
    family: &'static str,
    fields: &'static [(&'static str, ValueDesc)],
    head_len: usize,
    data: Vec<u8>,
    sites: Vec<Site>,
}

static AUDIO_FIELDS: [(&str, ValueDesc); 2] =
    [("rate", ValueDesc::Int(44_100)), ("channels", ValueDesc::Int(2))];
static VIDEO_FIELDS: [(&str, ValueDesc); 3] = [
    ("width", ValueDesc::Int(320)),
    ("height", ValueDesc::Int(240)),
    ("duration", ValueDesc::Int(1_000_000_000)),
];

fn mux_bases() -> Vec<MuxBase> {
    let with_frames = |head: Vec<u8>| {
        let mut data = head;
        for i in 0..6u8 {
            data.extend_from_slice(&nal_sample(i));
        }
        data
    };
    let flac = flac_head();
    let (avc, hevc) = (avcc(), hvcc());
    vec![
        MuxBase {
            name: "flac",
            family: "flac",
            fields: &AUDIO_FIELDS,
            head_len: flac.len(),
            sites: vec![Site::be(4, 1), Site::be(5, 3), Site::be(18, 4)],
            data: with_frames(flac),
        },
        MuxBase {
            name: "avcc",
            family: "h264/avcc",
            fields: &VIDEO_FIELDS,
            head_len: avc.len(),
            sites: vec![Site::be(0, 1), Site::be(6, 2)],
            data: with_frames(avc),
        },
        MuxBase {
            name: "hvcc",
            family: "h265/hvcc",
            fields: &VIDEO_FIELDS,
            head_len: hevc.len(),
            sites: vec![Site::be(0, 1), Site::be(22, 1)],
            data: with_frames(hevc),
        },
        MuxBase {
            name: "vp8",
            family: "vp8",
            fields: &VIDEO_FIELDS,
            head_len: 0,
            sites: Vec::new(),
            data: with_frames(Vec::new()),
        },
        MuxBase {
            name: "aac",
            family: "aac",
            fields: &AUDIO_FIELDS,
            head_len: 2,
            sites: vec![Site::be(0, 2)],
            data: with_frames(vec![0x12, 0x10]),
        },
    ]
}

fn hostile_pts(prng: &mut Prng, index: u64) -> Timestamp {
    match prng.below(16) {
        0 | 1 => Timestamp::NONE,
        2 => Timestamp::MAX,
        3 => Timestamp::from_nanos(EXTREMES[prng.below(EXTREMES.len() as u64) as usize] >> 1),
        4 => Timestamp::from_nanos(prng.next_u64() >> prng.below(40)),
        _ => Timestamp::from_nanos(index * 20_000_000),
    }
}

fn drive_mux(case: &Case, bases: &[MuxBase]) -> Result<(), Violation> {
    let mark = heap_mark();
    let Some(base) = bases.iter().find(|b| b.name == case.base) else {
        return Err(Violation::Invariant("unknown mux base"));
    };
    let multi = case.knob >> 4 & 1 == 1 || base.family == "aac";
    let mut harness = match multi {
        true => Harness::with_pool_config(MkvMuxN::new(), pool_config(case.knob)),
        false => Harness::with_pool_config(MkvMux::from_caps(), pool_config(case.knob)),
    };
    if multi {
        harness.fix_sink_format(MkvMuxN::INS[0], base.family, base.fields);
    }
    let mut fed = Fed::default();
    if harness.start().is_err() {
        return Ok(());
    }
    if case.knob >> 5 & 7 != 0 {
        let Some(format) = harness.vocabulary().build_fixed(base.family, base.fields) else {
            return Err(Violation::Invariant("mux base format did not resolve"));
        };
        fed.failed = harness.push_event(Event::FormatChange(format)).is_err();
    }

    let head_len = base.head_len.min(case.data.len());
    let (head, rest) = case.data.split_at(head_len);
    let mut buffers: Vec<&[u8]> = Vec::new();
    if !head.is_empty() && case.knob >> 8 & 3 != 0 {
        buffers.push(head);
        buffers.extend(case.chunking.split(rest));
    } else {
        buffers.extend(case.chunking.split(&case.data));
    }

    let mut prng = Prng::new(case.knob);
    let mut limit = 1024u64;
    for (index, bytes) in buffers.iter().enumerate() {
        if fed.failed {
            break;
        }
        let mut buffer = input_buffer(bytes);
        buffer.pts = hostile_pts(&mut prng, index as u64);
        buffer.flags = BufferFlags(prng.below(16) as u32);
        fed.steps += 1;
        fed.failed = harness.push(buffer).is_err();
        limit += 128 + 4 * bytes.len() as u64;
        settle(&mut harness, &mut fed, limit)?;
    }
    if !fed.failed
        && let Ok(tail) = harness.eos()
    {
        fed.out_bytes += tail.iter().map(|b| b.memory.len()).sum::<usize>();
    }
    collect(&mut harness, &mut fed);
    check_output(fed.out_bytes, case.data.len() + 96 * buffers.len() + 2 * head_len + 1024)?;
    check_heap(mark, 4 * case.data.len() + 512 * buffers.len() + pool_allowance(case.knob))
}

#[test]
fn mux_elements_survive_corrupt_heads_and_hostile_timestamps() {
    let bases = mux_bases();
    let make = |prng: &mut Prng| {
        let base = &bases[prng.below(bases.len() as u64) as usize];
        let (data, ops) = match prng.below(10) {
            0 => noise(prng, 256),
            1 => prefix_then_garbage(prng, &base.data, &base.sites, 64),
            2 | 3 => (base.data.clone(), Vec::new()),
            _ => mutate(prng, &base.data, &base.sites),
        };
        let chunking = Chunking::pick(prng, data.len());
        Case { base: base.name, ops, data, chunking, knob: prng.next_u64() }
    };
    gate("mkv mux", 2_500, &make, &|case| drive_mux(case, &bases));
}

// Regressions. Each is the smallest input that broke before its fix.

const STREAM_HEAD: [u8; 20] = [
    0x18, 0x53, 0x80, 0x67, 0xFF, // Segment, unknown size
    0x16, 0x54, 0xAE, 0x6B, 0x85, 0xAE, 0x83, 0xD7, 0x81, 0x01, // Tracks { TrackEntry { 1 } }
    0x1F, 0x43, 0xB6, 0x75, 0xFF, // Cluster, unknown size
];

fn streaming_reader() -> MatroskaReader {
    let mut reader = MatroskaReader::new();
    reader.push(&STREAM_HEAD).expect("the head parses");
    assert_eq!(reader.tracks().len(), 1);
    reader
}

#[test]
fn an_empty_fixed_lace_is_refused_instead_of_becoming_256_frames() {
    let block = [0xA3, 0x85, 0x81, 0x00, 0x00, 0x04, 0xFF];
    let mut reader = streaming_reader();
    reader.push(&block).expect("a bad block is skipped once the tracks are known");
    assert!(matches!(reader.take_damage(), Some(ebml::ReadError::Malformed(_))));
    assert!(reader.next_frame().is_none());
}

#[test]
fn empty_laced_frames_take_no_payload_buffer() {
    let block = [0xA3, 0x87, 0x81, 0x00, 0x00, 0x02, 0x02, 0x00, 0x00];
    let mut reader = streaming_reader();
    reader.push(&block).expect("a lace of empty frames parses");
    let mut frames = 0;
    while let Some(frame) = reader.next_frame() {
        assert_eq!((frame.data.len(), frame.data.capacity()), (0, 0));
        frames += 1;
    }
    assert_eq!(frames, 3);
}

#[test]
fn a_block_declared_past_the_send_cap_is_damage_not_a_buffer() {
    // A SimpleBlock declaring 100 MiB inside the unknown-size Cluster, one flipped size bit.
    let bad = [0xA3, 0x16, 0x40, 0x00, 0x00, 0x81, 0x00, 0x00, 0x80, 0x11, 0x22];
    let mut reader = streaming_reader();
    let mark = heap_mark();
    reader.push(&bad).expect("damage is skipped once the tracks are known");
    assert!(matches!(reader.take_damage(), Some(ebml::ReadError::Malformed(_))));
    assert_eq!(reader.damage_at(), STREAM_HEAD.len() as u64, "the block's offset is named");
    for _ in 0..64 {
        reader.push(&[0u8; 4096]).expect("what follows is scanned, not buffered");
    }
    assert!(heap_growth(mark) < 64 << 10, "the declared size was buffered");
    let good =
        [0x1F, 0x43, 0xB6, 0x75, 0xFF, 0xE7, 0x81, 0x00, 0xA3, 0x85, 0x81, 0x00, 0x00, 0x80, 0x42];
    reader.push(&good).expect("the next Cluster is found");
    assert_eq!(reader.next_frame().map(|f| f.data), Some(vec![0x42]));
}

#[test]
fn an_element_running_past_its_sized_segment_is_damage() {
    let mut file = vec![0x18, 0x53, 0x80, 0x67, 0x80 | 17]; // Segment ending at octet 22
    file.extend_from_slice(&STREAM_HEAD[5..]); // Tracks and an unknown-size Cluster, 15 octets
    file.extend_from_slice(&[0xA3, 0x85, 0x81, 0x00, 0x00, 0x80, 0x42]); // ends at 27
    let mut reader = MatroskaReader::new();
    reader.push(&file).expect("the head parses, the block is skipped");
    assert_eq!(reader.tracks().len(), 1);
    assert!(matches!(reader.take_damage(), Some(ebml::ReadError::Malformed(_))));
    assert_eq!(reader.damage_at(), 20);
    assert!(reader.next_frame().is_none(), "nothing past the Segment is a frame");
}

#[test]
fn an_opaque_element_with_a_huge_declared_size_is_skipped_unbuffered() {
    let void = [0xEC, 0x01, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00];
    let mut reader = streaming_reader();
    reader.push(&void).expect("the header of a skipped element");
    let mark = heap_mark();
    let filler = [0u8; 4096];
    for _ in 0..256 {
        reader.push(&filler).expect("skipped octets");
    }
    assert!(heap_growth(mark) < 16 << 10, "a megabyte of skipped octets was buffered");

    let mut reader = streaming_reader();
    let small_void = [0xEC, 0x83, 0xAA, 0xBB, 0xCC];
    reader.push(&small_void[..3]).expect("part of a small Void");
    reader.push(&small_void[3..]).expect("the rest of it");
    reader.push(&[0xA3, 0x85, 0x81, 0x00, 0x00, 0x80, 0x42]).expect("a block after the Void");
    assert_eq!(reader.next_frame().map(|f| f.data), Some(vec![0x42]));
}

#[test]
fn a_block_larger_than_the_reader_buffers_is_malformed() {
    let block = [0xA3, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
    let mut reader = streaming_reader();
    reader.push(&block).expect("the reader resyncs past it");
    assert!(matches!(reader.take_damage(), Some(ebml::ReadError::Malformed(_))));
    assert!(reader.next_frame().is_none());
}

#[test]
fn more_tracks_than_the_reader_accepts_is_malformed() {
    let entry = |n: u16| {
        let mut number = Vec::new();
        ebml::write_uint(&mut number, id::TRACK_NUMBER, n as u64);
        element(id::TRACK_ENTRY, &number)
    };
    let tracks = |count: u16| {
        let mut stream = STREAM_HEAD[..5].to_vec();
        let entries: Vec<u8> = (1..=count).flat_map(entry).collect();
        stream.extend(element(id::TRACKS, &entries));
        stream
    };
    let mut reader = MatroskaReader::new();
    reader.push(&tracks(256)).expect("256 tracks are accepted");
    assert_eq!(reader.tracks().len(), 256);
    let mut reader = MatroskaReader::new();
    assert!(matches!(reader.push(&tracks(257)), Err(ebml::ReadError::Malformed(_))));
}

#[test]
fn a_timestamp_near_the_maximum_does_not_overflow_tick_rounding() {
    for scale in [1u64, 1_000_000] {
        let mut writer =
            MatroskaWriter::with_timestamp_scale(vec![TrackConfig::vp8(1, 16, 16)], scale);
        let mut out = Vec::new();
        writer.write_header(&mut out).expect("header");
        writer.write_frame(&mut out, 1, 0, &[1], true).expect("a frame at zero");
        writer
            .write_frame(&mut out, 1, u64::MAX - 1, &[2], false)
            .expect("a frame at the end of time");
        writer.finalize(&mut out);
        let mut reader = MatroskaReader::new();
        reader.push(&out).expect("the result still parses");
    }
}

#[test]
fn a_pts_at_the_maximum_does_not_overflow_the_muxers_synth_clock() {
    for multi in [false, true] {
        let mut harness = match multi {
            true => Harness::new(MkvMuxN::new()),
            false => Harness::new(MkvMux::from_caps()),
        };
        if multi {
            harness.fix_sink_format(MkvMuxN::INS[0], "flac", &AUDIO_FIELDS);
        }
        let format = harness.vocabulary().build_fixed("flac", &AUDIO_FIELDS).expect("flac caps");
        harness.push_event(Event::FormatChange(format)).expect("announcement");
        harness.push(input_buffer(&flac_head())).expect("head");
        for pts in [Timestamp::MAX, Timestamp::NONE, Timestamp::MAX] {
            let mut frame = input_buffer(&[0xFF, 0xF8, 0x00]);
            frame.pts = pts;
            harness.push(frame).expect("a frame at the end of time");
        }
        harness.eos().expect("finalize");
    }
}

#[test]
fn a_frame_at_the_end_of_time_holds_none_behind_it() {
    let mut harness = Harness::new(MkvMuxN::new());
    harness.fix_sink_format(MkvMuxN::INS[0], "flac", &AUDIO_FIELDS);
    let format = harness.vocabulary().build_fixed("flac", &AUDIO_FIELDS).expect("flac caps");
    harness.push_event(Event::FormatChange(format)).expect("announcement");
    harness.push(input_buffer(&flac_head())).expect("head");
    let mut frame = input_buffer(&[0xFF, 0xF8, 0x00]);
    frame.pts = Timestamp::MAX;
    harness.push(frame).expect("a frame at the end of time");
    let mut out = 0;
    for _ in 0..200 {
        harness.push(input_buffer(&[0xFF, 0xF8, 0x00])).expect("an unstamped frame");
        out += harness.drain_outputs().iter().map(|b| b.memory.len()).sum::<usize>();
    }
    assert!(out >= 3 * 192, "{out} octets out of 200 frames before the end");
}

#[test]
fn a_flac_head_with_no_last_block_stops_at_the_ceiling() {
    let mut harness = Harness::new(MkvMux::from_caps());
    let format = harness.vocabulary().build_fixed("flac", &AUDIO_FIELDS).expect("flac caps");
    harness.push_event(Event::FormatChange(format)).expect("announcement");
    harness
        .push(input_buffer(&[b'f', b'L', b'a', b'C', 0x01, 0xFF, 0xFF, 0xFF]))
        .expect("the head so far");
    let filler = vec![0x01u8; 1 << 20];
    let mut refused_after = None;
    for megabytes in 1..=80 {
        if harness.push(input_buffer(&filler)).is_err() {
            refused_after = Some(megabytes);
            break;
        }
    }
    assert_eq!(refused_after, Some(64), "the 64 MiB ceiling on an absorbed FLAC head");
}
