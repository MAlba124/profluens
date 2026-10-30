//! Seeded corruption gate for pf-mp3. Tag parsers and the probe must not panic and must stay
//! within their step budget and heap limit.

// Test fixtures allocate freely.
#![allow(clippy::disallowed_methods)]

use pf_mp3::frame::Header;
use pf_mp3::id3::{ape_tail_len, parse_ape, parse_v1, parse_v2, v2_total_len};
use pf_mp3::{XingToc, probe_props};
use pf_sim::corrupt::{
    Chunking, Corpus, Input, Tracking, base_seed, corpus_gate, corpus_gate_quiet, heap_bounded,
    iterations,
};
use pf_sim::prng::Prng;
use profluens_core::event::TagSink;
use profluens_core::scratch::Arena;

#[global_allocator]
static HEAP: Tracking = Tracking;

const MONO: &[u8] = include_bytes!("fixtures/mono_128.mp3");
const JOINT_STEREO: &[u8] = include_bytes!("fixtures/js_128.mp3");
const MONO_ID3: &[u8] = include_bytes!("fixtures/mono_128_id3.mp3");

const PARSER_HEAP: usize = 4 * 64 * 1024;

fn synchsafe(n: u32) -> [u8; 4] {
    [((n >> 21) & 0x7F) as u8, ((n >> 14) & 0x7F) as u8, ((n >> 7) & 0x7F) as u8, (n & 0x7F) as u8]
}

fn v2_tag(major: u8, flags: u8, body: &[u8]) -> Vec<u8> {
    let mut out = b"ID3".to_vec();
    out.extend_from_slice(&[major, 0, flags]);
    out.extend_from_slice(&synchsafe(body.len() as u32));
    out.extend_from_slice(body);
    out
}

fn v2_frame(major: u8, id: &[u8], flags: [u8; 2], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    match major {
        2 => out.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]),
        3 => out.extend_from_slice(&(data.len() as u32).to_be_bytes()),
        _ => out.extend_from_slice(&synchsafe(data.len() as u32)),
    }
    if major > 2 {
        out.extend_from_slice(&flags);
    }
    out.extend_from_slice(data);
    out
}

fn unsync(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, &b) in src.iter().enumerate() {
        out.push(b);
        if b == 0xFF && src.get(i + 1).is_none_or(|&n| n == 0 || n & 0xE0 == 0xE0) {
            out.push(0);
        }
    }
    out
}

fn utf16(s: &str, big_endian: bool, bom: bool) -> Vec<u8> {
    let mut out = match (bom, big_endian) {
        (false, _) => Vec::new(),
        (true, true) => vec![0xFE, 0xFF],
        (true, false) => vec![0xFF, 0xFE],
    };
    for u in s.encode_utf16() {
        out.extend_from_slice(&if big_endian { u.to_be_bytes() } else { u.to_le_bytes() });
    }
    out
}

const ART: &[u8] =
    &[0xFF, 0xD8, 0xFF, 0x00, 0xFF, 0xE0, 0x00, 0x10, 0xFF, 0xFF, 0x00, 0x00, 0x42, 0xFF];

fn apic(image: &[u8]) -> Vec<u8> {
    let mut data = vec![0x00];
    data.extend_from_slice(b"image/jpeg\0");
    data.push(3);
    data.extend_from_slice(b"cover\0");
    data.extend_from_slice(image);
    data
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn tag_v24() -> Vec<u8> {
    let mut body = vec![0, 0, 0, 6, 1, 0];
    body.extend(v2_frame(4, b"TIT2", [0, 0], b"\x03Kitchen Sink"));
    body.extend(v2_frame(4, b"TPE1", [0, 0], b"\x03One\x00Two\x00Three"));
    body.extend(v2_frame(
        4,
        b"TALB",
        [0, 0],
        &cat(&[&[2], &utf16("Bl\u{e5}b\u{e6}r \u{1F3B5}", true, false)]),
    ));
    body.extend(v2_frame(4, b"TXXX", [0, 0], b"\x00replaygain_track_gain\x00-6.40 dB"));
    body.extend(v2_frame(
        4,
        b"TXXX",
        [0, 0],
        &cat(&[
            &[1],
            &utf16("mood", false, true),
            &[0, 0],
            &utf16("calm", false, true),
            &[0, 0],
            &utf16("slow", false, true),
        ]),
    ));
    body.extend(v2_frame(4, b"COMM", [0, 0], b"\x01eng\xFF\xFE\x00\x00\xFF\xFEh\x00i\x00"));
    body.extend(v2_frame(4, b"TCOM", [0, 0x01], &cat(&[&synchsafe(9), b"\x00Gr\xF8nn\xE5s"])));
    body.extend(v2_frame(4, b"APIC", [0, 0x02], &unsync(&apic(ART))));
    body.extend(v2_frame(4, b"TDRC", [0, 0], b"\x002019-04-01"));
    body.extend_from_slice(&[0u8; 8]);
    v2_tag(4, 0x40, &body)
}

fn tag_v23(whole_unsync: bool) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 6, 0, 0, 0, 0, 0, 0];
    body.extend(v2_frame(
        3,
        b"TIT2",
        [0, 0],
        &cat(&[&[1], &utf16("Sm\u{f8}rbr\u{f8}d", false, true)]),
    ));
    body.extend(v2_frame(3, b"TPE1", [0, 0], &cat(&[&[1], &utf16("\u{10348} hwair", true, true)])));
    body.extend(v2_frame(3, b"TALB", [0, 0], b"\x00\xC6sop\xFF fabler"));
    body.extend(v2_frame(3, b"TXXX", [0, 0], b"\x00REPLAYGAIN_ALBUM_PEAK\x000.977661"));
    body.extend(v2_frame(3, b"COMM", [0, 0], b"\x00nob\x00\xE5pen kommentar"));
    body.extend(v2_frame(3, b"TYER", [0, 0], b"\x001999"));
    body.extend(v2_frame(3, b"APIC", [0, 0], &apic(ART)));
    body.extend_from_slice(&[0u8; 6]);
    if whole_unsync { v2_tag(3, 0xC0, &unsync(&body)) } else { v2_tag(3, 0x40, &body) }
}

fn tag_v22() -> Vec<u8> {
    let mut body = v2_frame(2, b"TT2", [0, 0], b"\x00Tittel");
    body.extend(v2_frame(2, b"TP1", [0, 0], b"\x00Bj\xF8rk"));
    body.extend(v2_frame(2, b"TXX", [0, 0], b"\x00key\x00value"));
    body.extend(v2_frame(
        2,
        b"COM",
        [0, 0],
        &cat(&[b"\x01eng", &utf16("d", true, true), &[0, 0], &utf16("tekst", true, true)]),
    ));
    body.extend(v2_frame(2, b"PIC", [0, 0], &cat(&[b"\x00PNG\x03front\x00", ART])));
    body.extend(v2_frame(2, b"TRK", [0, 0], b"\x004/9"));
    v2_tag(2, 0x00, &body)
}

fn tag_fields(tag: &[u8]) -> Vec<usize> {
    let major = tag[3];
    let (id_len, size_len, hdr) = if major == 2 { (3, 3, 6) } else { (4, 4, 10) };
    let mut at = vec![0, 3, 5, 6, 10, 14];
    let mut pos = 10;
    if tag[5] & 0x40 != 0 && major > 2 {
        pos += if major == 4 { 6 } else { 10 };
    }
    while pos + hdr <= tag.len() && tag[pos] != 0 {
        let size = &tag[pos + id_len..pos + id_len + size_len];
        let size = match major {
            4 => size.iter().fold(0usize, |n, &b| (n << 7) | (b & 0x7F) as usize),
            _ => size.iter().fold(0usize, |n, &b| (n << 8) | b as usize),
        };
        at.extend([pos, pos + id_len, pos + hdr - 1, pos + hdr, pos + hdr + 1]);
        pos += hdr + size;
    }
    at.push(pos);
    at.retain(|&a| a < tag.len());
    at
}

fn v1_tag(
    title: &[u8],
    artist: &[u8],
    album: &[u8],
    year: &[u8],
    comment: &[u8],
    genre: u8,
) -> Vec<u8> {
    let mut out = b"TAG".to_vec();
    for (src, n) in [(title, 30), (artist, 30), (album, 30), (year, 4), (comment, 30)] {
        let mut field = vec![0u8; n];
        field[..src.len().min(n)].copy_from_slice(&src[..src.len().min(n)]);
        out.extend_from_slice(&field);
    }
    out.push(genre);
    out
}

fn ape_item(key: &str, value: &[u8], flags: u32) -> Vec<u8> {
    let mut out = (value.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(key.as_bytes());
    out.push(0);
    out.extend_from_slice(value);
    out
}

fn ape_tag(with_header: bool) -> (Vec<u8>, Vec<usize>) {
    let items = [
        ape_item("replaygain_track_gain", b"-6.40 dB", 0),
        ape_item("replaygain_track_peak", b"0.977661", 0),
        ape_item("Title", b"Corner", 0),
        ape_item("Year", b"2019", 0),
        ape_item("Track", b"4/9", 0),
        ape_item("Artist", b"Sofia\x00Emil", 0),
        ape_item("Cover Art (Front)", b"\xFF\xD8\xFF\xE0", 2),
    ];
    let body_len: usize = items.iter().map(Vec::len).sum();
    let block = |flags: u32| {
        let mut b = b"APETAGEX".to_vec();
        b.extend_from_slice(&2000u32.to_le_bytes());
        b.extend_from_slice(&((body_len + 32) as u32).to_le_bytes());
        b.extend_from_slice(&(items.len() as u32).to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b
    };
    let has_header = if with_header { 1u32 << 31 } else { 0 };
    let mut out = Vec::new();
    let mut fields = Vec::new();
    if with_header {
        out.extend_from_slice(&block(has_header | (1 << 29)));
        fields.extend([0, 8, 12, 16, 20]);
    }
    for item in &items {
        fields.extend([out.len(), out.len() + 4, out.len() + 8]);
        out.extend_from_slice(item);
    }
    let footer = out.len();
    fields.extend([footer, footer + 8, footer + 12, footer + 16, footer + 20, footer + 23]);
    out.extend_from_slice(&block(has_header));
    (out, fields)
}

fn stream_fields(bytes: &[u8], info: bool) -> Vec<usize> {
    let mut pos = v2_total_len(bytes).unwrap_or(0);
    let mut at = if pos > 0 { vec![6, pos] } else { Vec::new() };
    let block = bytes[pos..].windows(4).take(64).position(|w| w == b"Info").filter(|_| info);
    if let Some(block) = block.map(|b| b + pos) {
        at.extend([block, block + 4, block + 8, block + 12, block + 16, block + 116, block + 120]);
        at.extend([block + 120 + 0x15, block + 120 + 0x18]);
    }
    while let Some(len) = bytes.get(pos..).and_then(Header::parse).and_then(|h| h.frame_len()) {
        at.extend([pos + 2, pos + 4]);
        pos += len;
    }
    at.retain(|&a| a < bytes.len());
    at
}

fn head_frames(bytes: &[u8], frames: usize) -> &[u8] {
    let mut pos = v2_total_len(bytes).unwrap_or(0);
    for _ in 0..frames {
        match bytes.get(pos..).and_then(Header::parse).and_then(|h| h.frame_len()) {
            Some(len) if pos + len <= bytes.len() => pos += len,
            _ => break,
        }
    }
    &bytes[..pos]
}

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    fields: Vec<usize>,
}

fn inputs(fixtures: &[Fixture]) -> Vec<Input<'_>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

fn stream_fixtures(frames: usize, info: bool) -> Vec<Fixture> {
    let make = |name, bytes: &[u8]| {
        let bytes = head_frames(bytes, frames).to_vec();
        Fixture { name, fields: stream_fields(&bytes, info), bytes }
    };
    vec![make("mono_128", MONO), make("js_128", JOINT_STEREO), make("mono_128_id3", MONO_ID3)]
}

fn tag_fixtures() -> Vec<Fixture> {
    let make = |name, bytes: Vec<u8>| Fixture { name, fields: tag_fields(&bytes), bytes };
    let lavf = MONO_ID3[..v2_total_len(MONO_ID3).expect("fixture tag")].to_vec();
    vec![
        make("v24_kitchen_sink", tag_v24()),
        make("v23_extended", tag_v23(false)),
        make("v23_whole_unsync", tag_v23(true)),
        make("v22", tag_v22()),
        make("v24_lavf", lavf),
    ]
}

fn tail_fixtures() -> Vec<Fixture> {
    let v1 = v1_tag(b"Tittel", b"Bj\xF8rk", b"Album", b"1999", b"kommentar", 17);
    let mut v11 = v1_tag(b"T", b"A", b"L", b"2001", b"with a track number", 147);
    v11[125] = 0;
    v11[126] = 9;
    let (ape, ape_fields) = ape_tag(false);
    let (ape_headed, headed_fields) = ape_tag(true);
    let mut both = ape_headed.clone();
    both.extend_from_slice(&v1);
    let v1_fields = vec![0, 3, 33, 63, 93, 97, 125, 126, 127];
    let mut both_fields = headed_fields.clone();
    both_fields.extend(v1_fields.iter().map(|f| f + ape_headed.len()));
    vec![
        Fixture { name: "id3v1", bytes: v1, fields: v1_fields.clone() },
        Fixture { name: "id3v1_1", bytes: v11, fields: v1_fields },
        Fixture { name: "ape", bytes: ape, fields: ape_fields },
        Fixture { name: "ape_with_header", bytes: ape_headed, fields: headed_fields },
        Fixture { name: "ape_then_id3v1", bytes: both, fields: both_fields },
    ]
}

const TAG_TOKENS: &[&[u8]] = &[
    b"ID3",
    &[b'I', b'D', b'3', 4, 0, 0x80, 0x7F, 0x7F, 0x7F, 0x7F],
    b"TIT2",
    b"TXXX",
    b"APIC",
    b"COMM",
    b"TT2",
    b"PIC",
    b"TAG",
    b"APETAGEX",
    &[0x00],
    &[0x00, 0x00],
    &[0xFF, 0xFE],
    &[0xFE, 0xFF],
    &[0xFF, 0x00],
    &[0xD8, 0x00],
    &[0x00, 0xDC],
    &[0xC0, 0x80],
    &[0xED, 0xA0, 0x80],
    &[0xF4, 0x90, 0x80, 0x80],
    &[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, b'k', 0x00, b'v'],
];

const STREAM_TOKENS: &[&[u8]] = &[
    &[0xFF, 0xFB, 0x90, 0x00],
    &[0xFF, 0xFB, 0x92, 0xC0],
    &[0xFF, 0xFA, 0xE4, 0x40],
    &[0xFF, 0xF3, 0x14, 0xC0],
    &[0xFF, 0xE3, 0x18, 0x00],
    &[0xFF, 0xFD, 0x10, 0x00],
    &[0xFF, 0xFF, 0x18, 0x00],
    &[0xFF, 0xFB, 0x00, 0x00],
    &[0xFF, 0xE0],
    b"ID3",
    &[b'I', b'D', b'3', 4, 0, 0x10, 0x7F, 0x7F, 0x7F, 0x7F],
    &[b'I', b'D', b'3', 3, 0, 0, 0, 0, 0, 0],
    b"TAG",
    b"APETAGEX",
    b"Xing",
    b"Info",
    b"VBRI",
    b"LAME",
    &[0x00; 16],
    &[0xFF; 16],
];

fn synthetic_stream(seed: u64) -> Vec<u8> {
    let mut p = Prng::new(seed);
    let mut out = Vec::new();
    if p.chance(15) {
        let body = vec![0xFFu8; p.below(40) as usize];
        out.extend_from_slice(&v2_tag(4, 0, &body));
    }
    let version = [0b11u8, 0b11, 0b10, 0b00][p.below(4) as usize];
    let layer = if p.chance(85) { 0b01u8 } else { 0b10 + p.below(2) as u8 };
    let rate = p.below(3) as u8;
    let mode = p.below(4) as u8;
    for _ in 0..1 + p.below(6) {
        let stray = p.chance(12);
        let version = if stray { [0b11u8, 0b10, 0b00][p.below(3) as usize] } else { version };
        let mode = if stray { p.below(4) as u8 } else { mode };
        let protection = u8::from(!p.chance(10));
        let bitrate = if p.chance(60) { 1 + p.below(4) as u8 } else { 1 + p.below(14) as u8 };
        let head = [
            0xFF,
            0xE0 | (version << 3) | (layer << 1) | protection,
            (bitrate << 4) | (rate << 2) | ((p.below(2) as u8) << 1),
            (mode << 6) | ((p.below(4) as u8) << 4),
        ];
        let Some(len) = Header::parse(&head).and_then(|h| h.frame_len()) else {
            continue;
        };
        let start = out.len();
        out.extend_from_slice(&head);
        out.resize(start + len.max(4), 0);
        match p.below(4) {
            0 => {}
            1 => out[start + 4..].fill(0xFF),
            _ => {
                for b in &mut out[start + 4..] {
                    *b = p.next_u64() as u8;
                }
            }
        }
        if p.chance(50) && len > 5 {
            out[start + 4] = 0;
            out[start + 5] &= 0x7F;
        }
    }
    out
}

#[derive(Default)]
struct Counting {
    entries: usize,
    value_bytes: usize,
}

impl TagSink for Counting {
    fn text(&mut self, _key: &str, value: &str) {
        self.entries += 1;
        self.value_bytes += value.len();
    }
    fn picture(&mut self, mime: &str, data: &[u8]) {
        self.entries += 1;
        self.value_bytes += mime.len() + data.len();
    }
}

/// Single forward passes with no counter, so the step budget is on what they emit.
fn id3v2_target(bytes: &[u8], _: Chunking) {
    let total = v2_total_len(bytes);
    if let Some(total) = total {
        assert!((10..10 + (1 << 28) + 10).contains(&total), "tag length {total}");
    }
    let sized = &bytes[..total.unwrap_or(bytes.len()).min(bytes.len())];
    for window in [bytes, sized] {
        let arena = Arena::default();
        let mut sink = Counting::default();
        parse_v2(window, &arena, &mut sink);
        assert!(sink.entries <= window.len(), "{} values from {}", sink.entries, window.len());
        // Latin-1 doubles at worst (two UTF-8 bytes per byte), UTF-16 grows by half.
        assert!(
            sink.value_bytes <= 2 * window.len(),
            "{} bytes from {}",
            sink.value_bytes,
            window.len()
        );
    }
}

fn tail_target(bytes: &[u8], _: Chunking) {
    let arena = Arena::default();
    let mut v1 = Counting::default();
    parse_v1(bytes, &arena, &mut v1);
    assert!(
        v1.entries <= 7 && v1.value_bytes <= 2 * 128,
        "{} values, {} bytes",
        v1.entries,
        v1.value_bytes
    );

    let body = match bytes.len().checked_sub(128) {
        Some(cut) if bytes[cut..].starts_with(b"TAG") => &bytes[..cut],
        _ => bytes,
    };
    let sized = ape_tail_len(body).map(|total| {
        assert!((32..=u32::MAX as usize + 32).contains(&total), "APE length {total}");
        body.len().checked_sub(total).map_or(body, |start| &body[start..])
    });
    for window in [Some(bytes), sized].into_iter().flatten() {
        let mut ape = Counting::default();
        parse_ape(window, &mut ape);
        assert!(ape.entries <= window.len() && ape.value_bytes <= window.len());
    }
}

fn props_target(bytes: &[u8], _: Chunking) {
    for audio_len in [bytes.len() as u64, 0, u64::MAX] {
        let Some(props) = probe_props(bytes, audio_len) else {
            continue;
        };
        assert!(props.first_frame_offset + 4 <= bytes.len(), "offset {}", props.first_frame_offset);
        assert!((8_000..=48_000).contains(&props.sample_rate), "rate {}", props.sample_rate);
        assert!((1..=2).contains(&props.channels), "channels {}", props.channels);
        let Some(toc) = props.toc else { continue };
        let duration = props.duration_ns.unwrap_or(u64::MAX);
        for stream in [0, u64::from(props.stream_bytes.unwrap_or(0)), audio_len, u64::MAX] {
            for i in 0..XingToc::LEN + 2 {
                let (byte, time) = (toc.byte_at(i, stream), toc.time_at(i, duration));
                assert_eq!(byte.is_some(), i < XingToc::LEN);
                assert_eq!(time.is_some(), i < XingToc::LEN);
                assert!(byte.unwrap_or(0) <= stream && time.unwrap_or(0) <= duration);
            }
        }
    }
}

#[test]
fn id3v2_parser_survives_corruption() {
    let fixtures = tag_fixtures();
    let gate = Corpus {
        name: "mp3_id3v2",
        inputs: &inputs(&fixtures),
        tokens: TAG_TOKENS,
        iters: iterations(6000),
        sweep: 128,
        tolerate: &[],
    };
    // The arena holds the unsync copy (1x), the text rewrite (2x) and the key (1x), in 64 KiB
    // chunks.
    corpus_gate(&gate, &heap_bounded(PARSER_HEAP, 16, id3v2_target));
}

#[test]
fn id3v1_and_ape_parsers_survive_corruption() {
    let fixtures = tail_fixtures();
    let gate = Corpus {
        name: "mp3_id3v1_ape",
        inputs: &inputs(&fixtures),
        tokens: TAG_TOKENS,
        iters: iterations(6000),
        sweep: 128,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(2 * 64 * 1024, 0, tail_target));
}

#[test]
fn props_probe_survives_corruption() {
    let fixtures = stream_fixtures(3, true);
    let gate = Corpus {
        name: "mp3_props",
        inputs: &inputs(&fixtures),
        tokens: STREAM_TOKENS,
        iters: iterations(6000),
        sweep: 128,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(4096, 0, props_target));
}

#[test]
fn props_survive_valid_headers_around_random_payloads() {
    let count = iterations(96);
    let base = Prng::new(base_seed("mp3_synthetic_frames"));
    let mut cases = 0;
    let mut replay = false;
    let probed = std::cell::Cell::new(0usize);
    for first in (0..count).step_by(64) {
        let batch = first..(first + 64).min(count);
        let streams: Vec<Vec<u8>> =
            batch.clone().map(|i| synthetic_stream(base.substream(i).next_u64())).collect();
        let names: Vec<String> = batch.map(|i| format!("synthetic stream {i}")).collect();
        let inputs: Vec<Input<'_>> = streams
            .iter()
            .zip(&names)
            .map(|(bytes, name)| Input { name, bytes, fields: &[] })
            .collect();
        let gate = Corpus {
            name: "mp3_synthetic_frames",
            inputs: &inputs,
            tokens: STREAM_TOKENS,
            iters: 0,
            sweep: 1,
            tolerate: &[],
        };
        let bounded = heap_bounded(4096, 0, |bytes, chunking| {
            probed
                .set(probed.get() + usize::from(probe_props(bytes, bytes.len() as u64).is_some()));
            props_target(bytes, chunking);
        });
        let summary = corpus_gate_quiet(&gate, &bounded);
        cases += summary.cases;
        replay |= summary.replay;
    }
    eprintln!("corruption gate mp3_synthetic_frames passed {cases} cases");
    // The generator must get frames past the framer to be worth running.
    let enough = replay || probed.get() as u64 >= count / 8;
    assert!(enough, "only {} streams probed to a frame", probed.get());
}

#[test]
fn pure_noise_never_yields_more_than_it_holds() {
    let gate = Corpus {
        name: "mp3_noise",
        inputs: &[],
        tokens: STREAM_TOKENS,
        iters: iterations(1500),
        sweep: 0,
        tolerate: &[],
    };
    let bounded = heap_bounded(PARSER_HEAP, 16, |bytes, chunking| {
        id3v2_target(bytes, chunking);
        tail_target(bytes, chunking);
        props_target(bytes, chunking);
    });
    corpus_gate(&gate, &bounded);
}
