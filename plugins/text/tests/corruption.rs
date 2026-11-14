//! The seeded corruption gate for pf-text, over every decoder and element that reads untrusted
//! input.

// Test code, so fixtures and case buffers live on the heap.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use pf_sim::corrupt::harness::{drive, drive_pieces};
use pf_sim::corrupt::{
    Chunking, Corpus, Input, Tracking, corpus_gate, heap_bounded, heap_during, iterations,
};
use pf_subparse::ir::CueBuilder;
use pf_text::bitmap::{BITMAP_HEADER_LEN, BITMAP_RECT_LEN, BitmapBuilder, BitmapRef, payload_len};
use pf_text::dvbsubdec::PIXEL_BUDGET;
use pf_text::pgs::{self, DisplaySet, PgsError};
use pf_text::vobsubdec::MAX_BITMAP_BYTES as VOBSUB_MAX_BITMAP;
use pf_text::{DvbSubDec, PgsDec, SubtitleOverlay, VobSubDec};
use profluens_core::buffer::Buffer;
use profluens_core::bus::Bus;
use profluens_core::ctx::Ctx;
use profluens_core::element::Element;
use profluens_core::event::Event;
use profluens_core::id::{ElementId, FormatId};
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;
use profluens_video::format::PixelFormat;
use profluens_video::geometry::frame_size;

#[global_allocator]
static HEAP: Tracking = Tracking;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

fn text_fields(bytes: &[u8]) -> Vec<usize> {
    let mut at = vec![0];
    for (i, w) in bytes.windows(3).enumerate() {
        if w == b"-->" {
            at.extend([i.saturating_sub(13), i.saturating_sub(4), i, i + 4, i + 13]);
        }
        if w[0] == b'\n' || w[0] == b'\r' {
            at.push(i + 1);
        }
    }
    at.retain(|&a| a < bytes.len());
    at.sort_unstable();
    at.dedup();
    at
}

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    fields: Vec<usize>,
}

fn inputs(fixtures: &[Fixture]) -> Vec<Input<'_>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

const TEXT_TOKENS: &[&[u8]] = &[
    b"\x80",
    b"\xC3",
    b"\xE6\x97",
    b"\xF0\x9F\x98",
    b"\xC0\xAF",
    b"\xE0\x80\xAF",
    b"\xF0\x82\x82\xAC",
    b"\xED\xA0\x80",
    b"\xF4\x90\x80\x80",
    b"\xFF",
    b"A",
    b"<",
    b">",
    b"{",
    b"}",
    b"<b>",
    b"<i>",
    b"<font color=\"#ff0000\">",
    b"<c.loud>",
    b"<v Speaker>",
    b"<00:00:01.000>",
    b"{\\b1}",
    b"{\\t(",
    b"\\N",
    b"\\h",
    b"\\",
    b"99999999999:99:99,999 --> 99999999999:99:99.999\n",
    b"-00:00:01,000 --> -00:00:02,000\n",
    b"00:00:05,000 --> 00:00:01,000\n",
    b"00:00:05.000 --> 00:00:01.000\n",
    b"00:00:01 --> \n",
    b" --> ",
    b"-->-->-->",
    b"00:00:00,18446744073709551616 --> 00:00:00,99999999999999999999999\n",
    b"18446744073709551615:00:00.00",
    b"0:00:00.1844674407370955162,",
    b"6000000:00:00,000",
    b":",
    b",",
    b".",
    b"9",
    b"\r\n",
    b"\r",
    b"\n\n",
    b"\xEF\xBB\xBF",
    b"\0",
    b"[Events]\n",
    b"Format: Text, Start, End\n",
    b"Format:",
    b"Dialogue: ",
    b"Dialogue: 0,0:00:00.00,0:00:01.00,D,,0,0,0,,x\n",
    b"WEBVTT\n\n",
    b"NOTE ",
    b"STYLE\n",
    b"REGION\n",
    b"1\n00:00:00,000 --> 00:00:01,000\nx\n\n",
];

fn cue_payload(text: &str) -> Vec<u8> {
    let mut b = CueBuilder::new();
    // Hostile cue fixtures exceed CUE_LIMIT on purpose
    b.set_limit(text.len() * 32 + 1024);
    b.plain_text(text);
    b.to_vec()
}

const MAX_PIECES: usize = 4096;

fn coarse(bytes: &[u8], chunking: Chunking) -> Chunking {
    if chunking.pieces(bytes).count() <= MAX_PIECES {
        chunking
    } else {
        Chunking::Fixed(bytes.len().div_ceil(MAX_PIECES))
    }
}

const SEG_PDS: u8 = 0x14;
const SEG_ODS: u8 = 0x15;
const SEG_PCS: u8 = 0x16;
const SEG_WDS: u8 = 0x17;
const SEG_END: u8 = 0x80;

const MAX_RUN: usize = 16_383;

struct Segment {
    kind: u8,
    body: Vec<u8>,
    fields: &'static [usize],
}

fn rle_row(row: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < row.len() {
        let colour = row[i];
        let mut n = 1;
        while i + n < row.len() && row[i + n] == colour && n < MAX_RUN {
            n += 1;
        }
        match (colour, n) {
            (0, 1..=63) => out.extend([0, n as u8]),
            (0, _) => out.extend([0, 0x40 | (n >> 8) as u8, n as u8]),
            (_, 1 | 2) => out.extend(std::iter::repeat_n(colour, n)),
            (_, 3..=63) => out.extend([0, 0x80 | n as u8, colour]),
            (_, _) => out.extend([0, 0xC0 | (n >> 8) as u8, n as u8, colour]),
        }
        i += n;
    }
    out.extend([0, 0]);
}

fn pattern(width: usize, height: usize) -> Vec<u8> {
    let mut px = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            px.push(if x < width / 2 {
                0
            } else if x < width - width / 8 {
                1 + (y % 3) as u8
            } else {
                (x % 5) as u8
            });
        }
    }
    px
}

fn pcs(video: (u16, u16), objects: &[(u16, bool, u16, u16)]) -> Segment {
    let mut body = Vec::new();
    body.extend_from_slice(&video.0.to_be_bytes());
    body.extend_from_slice(&video.1.to_be_bytes());
    body.extend_from_slice(&[0x10, 0, 1, 0x80, 0, 0, objects.len() as u8]);
    for &(id, cropped, x, y) in objects {
        body.extend_from_slice(&id.to_be_bytes());
        // Bit 7 is object_cropped_flag.
        body.extend_from_slice(&[0, if cropped { 0x80 } else { 0 }]);
        body.extend_from_slice(&x.to_be_bytes());
        body.extend_from_slice(&y.to_be_bytes());
        if cropped {
            body.extend_from_slice(&[0, 1, 0, 1, 0, 4, 0, 2]);
        }
    }
    Segment { kind: SEG_PCS, body, fields: &[0, 2, 10, 11, 14, 15, 17] }
}

fn wds() -> Segment {
    let body = vec![1, 0, 0, 100, 0, 200, 0, 64, 0, 16];
    Segment { kind: SEG_WDS, body, fields: &[0, 2, 6] }
}

fn pds() -> Segment {
    let mut body = vec![0, 0];
    let entries: [[u8; 4]; 4] =
        [[1, 235, 128, 128], [2, 82, 240, 90], [3, 145, 54, 34], [4, 41, 110, 240]];
    for [id, y, cr, cb] in entries {
        body.extend_from_slice(&[id, y, cr, cb, 255]);
    }
    Segment { kind: SEG_PDS, body, fields: &[0, 2, 7] }
}

fn ods(id: u16, width: u16, height: u16, pieces: usize) -> Vec<Segment> {
    let px = pattern(width as usize, height as usize);
    let mut rle = Vec::new();
    for row in px.chunks(width as usize) {
        rle_row(row, &mut rle);
    }
    let data_len = ((4 + rle.len()) as u32).to_be_bytes();
    let step = rle.len().div_ceil(pieces).max(1);
    let last = rle.chunks(step).count() - 1;
    let mut out = Vec::new();
    for (i, part) in rle.chunks(step).enumerate() {
        let sequence = (if i == 0 { 0x80 } else { 0 }) | (if i == last { 0x40 } else { 0 });
        let mut body = Vec::new();
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&[0, sequence]);
        body.extend_from_slice(&data_len[1..]);
        if i == 0 {
            body.extend_from_slice(&width.to_be_bytes());
            body.extend_from_slice(&height.to_be_bytes());
        }
        body.extend_from_slice(part);
        let fields: &[usize] = if i == 0 { &[0, 3, 4, 7, 9, 11, 13] } else { &[0, 3, 4, 7] };
        out.push(Segment { kind: SEG_ODS, body, fields });
    }
    out
}

fn end() -> Segment {
    Segment { kind: SEG_END, body: Vec::new(), fields: &[] }
}

fn frame(name: &'static str, segments: &[Segment], sup: bool) -> Fixture {
    let (mut bytes, mut fields) = (Vec::new(), Vec::new());
    for (n, seg) in segments.iter().enumerate() {
        if sup {
            fields.extend([bytes.len(), bytes.len() + 2]);
            bytes.extend_from_slice(b"PG");
            bytes.extend_from_slice(&(90_000 * n as u32).to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
        }
        let at = bytes.len();
        fields.extend([at, at + 1]);
        let inner = seg.fields.iter().filter(|&&o| o < seg.body.len());
        fields.extend(inner.map(|o| at + 3 + o));
        bytes.push(seg.kind);
        bytes.extend_from_slice(&(seg.body.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&seg.body);
    }
    Fixture { name, bytes, fields }
}

fn caption(width: u16, height: u16, pieces: usize) -> Vec<Segment> {
    let mut segments = vec![pcs((1920, 1080), &[(0, false, 100, 200)]), wds(), pds()];
    segments.extend(ods(0, width, height, pieces));
    segments.push(end());
    segments
}

fn pgs_fixtures() -> Vec<Fixture> {
    let mut two = vec![pcs((1280, 720), &[(7, true, 10, 20), (8, false, 30, 40)]), pds()];
    two.extend(ods(8, 12, 3, 1));
    two.extend(ods(7, 20, 2, 2));
    two.push(end());
    vec![
        frame("caption_24x4", &caption(24, 4, 1), false),
        frame("fragmented_200x3", &caption(200, 3, 3), false),
        frame("two_objects_cropped", &two, false),
        frame("clear", &[pcs((1920, 1080), &[]), end()], false),
    ]
}

const PGS_TOKENS: &[&[u8]] = &[
    &[SEG_END, 0, 0],
    &[SEG_PCS, 0, 11, 0x07, 0x80, 0x04, 0x38, 0x10, 0, 0, 0x80, 0, 0, 0],
    &[
        SEG_PCS, 0, 19, 0xFF, 0xFF, 0xFF, 0xFF, 0x10, 0, 0, 0x80, 0, 0, 1, 0, 0, 0, 0, 0xFF, 0xFF,
        0xFF, 0xFF,
    ],
    &[SEG_PDS, 0, 7, 0, 0, 0, 255, 255, 255, 255],
    &[SEG_ODS, 0xFF, 0xFF],
    &[SEG_ODS, 0, 11, 0, 0, 0, 0xC0, 0, 0, 4, 0xFF, 0xFF, 0xFF, 0xFF],
    &[SEG_ODS, 0, 11, 0, 0, 0, 0xC0, 0, 0, 4, 0x10, 0x00, 0x08, 0x70],
    &[SEG_ODS, 0, 11, 0, 0, 0, 0x80, 0, 0, 4, 0x00, 0x40, 0x00, 0x40],
    &[SEG_ODS, 0, 7, 0, 0, 0, 0x00, 0, 0, 0],
    &[SEG_ODS, 0, 7, 0, 0, 0, 0x40, 0, 0, 0],
    &[SEG_WDS, 0, 1, 0],
    &[0x00, 0x7F, 0xFF],
    &[0x00, 0xFF, 0xFF, 0x01],
    &[0x00, 0x00],
    &[0x00],
    &[0x00, 0xBF, 0x02],
    b"PG",
    b"PG\0\0\0\0\0\0\0\0\x80\0\0",
    b"BSUB",
];

fn check_display_set(ds: &DisplaySet, coded: usize) {
    let pixels = ds.width as usize * ds.height as usize;
    assert_eq!(ds.rgba.len(), pixels * 4, "{}x{}", ds.width, ds.height);
    assert!(
        pixels <= MAX_RUN * coded.div_ceil(3),
        "{}x{} pixels out of {coded} bytes",
        ds.width,
        ds.height
    );
}

fn pgs_target(bytes: &[u8], _: Chunking) {
    match pgs::parse_display_set(bytes) {
        Ok(ds) => check_display_set(&ds, bytes.len()),
        Err(e) => assert!(!e.reason().is_empty()),
    }
    let sets = pgs::split_sup(bytes);
    let peeled: usize = sets.iter().map(|(_, _, set)| set.len()).sum();
    assert!(peeled <= bytes.len() && sets.len() <= bytes.len() / 26 + 1);
    for (_, _, set) in &sets {
        if let Ok(ds) = pgs::parse_display_set(set) {
            check_display_set(&ds, set.len());
        }
    }
    if let Some((_, segment)) = pgs::strip_sup_header(bytes) {
        assert!(segment.len() + 10 <= bytes.len());
    }
    if let Ok(image) = BitmapRef::parse(bytes) {
        let rects = image.rects().len();
        let pixels: usize = image.rects().iter().map(|r| image.rgba(r).len()).sum();
        assert!(BITMAP_HEADER_LEN + rects * BITMAP_RECT_LEN + pixels <= bytes.len());
        if rects <= 1 {
            let ds = pgs::decode_bitmap(bytes).expect("a validated payload decodes");
            assert_eq!(pgs::encode_bitmap(&ds), bytes, "one rect re-encodes to itself");
        }
    }
}

#[test]
fn pgs_parsers_survive_corruption() {
    let mut fixtures = pgs_fixtures();
    fixtures.push(frame("sup_caption_24x4", &caption(24, 4, 2), true));
    let wire = pgs::parse_display_set(&fixtures[0].bytes).expect("fixture decodes");
    let fields: Vec<usize> = (0..BITMAP_HEADER_LEN + BITMAP_RECT_LEN).step_by(4).collect();
    fixtures.push(Fixture { name: "bitmap_wire", bytes: pgs::encode_bitmap(&wire), fields });
    let gate = Corpus {
        name: "pgs_parsers",
        inputs: &inputs(&fixtures),
        tokens: PGS_TOKENS,
        iters: iterations(3000),
        sweep: 48,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(5 * MAX_RUN + 64 * KIB, 5 * MAX_RUN / 3 + 16, pgs_target));
}

fn pgsdec_target(bytes: &[u8], chunking: Chunking) {
    let pieces: Vec<&[u8]> = coarse(bytes, chunking).pieces(bytes).collect();
    let group = 1 + bytes.len() % 3;
    for flush in [None, Some(1 + bytes.len() % 3)] {
        let mut h = Harness::new(PgsDec::new());
        let driven = drive_pieces(&mut h, &pieces, flush, &|i, buf| {
            buf.pts = Timestamp::from_millis((i / group) as u64 * 500);
            buf.duration = Timestamp::from_millis(400);
        });
        assert!(!driven.failed, "pgsdec drops a bad set, it does not fail");
        assert!(driven.out_buffers <= pieces.len());
        let per_set = BITMAP_HEADER_LEN + BITMAP_RECT_LEN + 4 * MAX_RUN;
        let limit = pieces.len() * per_set + 4 * MAX_RUN * (bytes.len() / 3);
        assert!(driven.out_bytes <= limit, "{} bytes out of {}", driven.out_bytes, bytes.len());
    }
}

#[test]
fn pgsdec_element_survives_corruption() {
    let fixtures = pgs_fixtures();
    let gate = Corpus {
        name: "pgsdec_element",
        inputs: &inputs(&fixtures),
        tokens: PGS_TOKENS,
        iters: iterations(400),
        sweep: 16,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(13 * MAX_RUN + 4 * MIB, 13 * MAX_RUN / 3 + 16, pgsdec_target));
}

fn dvb_fields(bytes: &[u8]) -> Vec<usize> {
    let mut fields = vec![0];
    let mut at = usize::from(bytes.starts_with(&[0x20, 0x00])) * 2;
    while at + 6 <= bytes.len() && bytes[at] == 0x0F {
        let len = usize::from(u16::from_be_bytes([bytes[at + 4], bytes[at + 5]]));
        fields.extend([at, at + 1, at + 2, at + 4]);
        fields.extend((at + 6..at + 6 + len.min(12)).filter(|&o| o < bytes.len()));
        at += 6 + len;
    }
    fields
}

fn dvb_fixtures() -> Vec<Fixture> {
    use pf_text::dvbsubdec::fixtures::*;
    let two = block(&[
        display(1920, 1080, Some((16, 1903, 8, 1071))),
        page(5, 2, &[(1, 10, 20), (2, 300, 400)]),
        region(1, 16, 4, 8, 1, 0, true, &[(5, 0, 0)]),
        region(2, 8, 2, 2, 2, 1, true, &[(6, 2, 0)]),
        clut(1, &[(1, 235, 128, 128, 0), (0x42, 81, 240, 90, 0)]),
        object(
            5,
            &[vec![0x21, 0, 0x42, 0, 0], two_bit(&[(1, 9)]), eol(), eight_bit(&[(0x42, 16)])]
                .concat(),
            &[],
        ),
        object_with(
            6,
            true,
            &[vec![0x22], [0x11; 16].to_vec(), four_bit(&[(1, 3), (2, 30)])].concat(),
            &two_bit(&[(3, 6)]),
        ),
        end(),
    ]);
    let make = |name, bytes: Vec<u8>| Fixture { name, fields: dvb_fields(&bytes), bytes };
    vec![
        make("dvb_minimal", minimal()),
        make("dvb_two_regions_maps", two),
        make("dvb_pes_field", [vec![0x20, 0x00], minimal(), vec![0xFF]].concat()),
        make("dvb_clear", block(&[page(5, 0, &[]), end()])),
    ]
}

const DVB_TOKENS: &[&[u8]] = &[
    &[0x0F, 0x10, 0, 1, 0xFF, 0xFF],
    &[0x0F, 0x80, 0, 1, 0, 0],
    &[0x0F, 0x10, 0, 1, 0, 8, 5, 0x08, 1, 0, 0, 0, 0, 0],
    &[0x0F, 0x11, 0, 1, 0, 10, 1, 0x08, 0x04, 0x00, 0x04, 0x00, 0x0C, 1, 0x55, 0],
    &[0x0F, 0x12, 0, 1, 0, 8, 1, 0, 0xFF, 0xE1, 235, 128, 128, 0],
    &[0x0F, 0x13, 0, 1, 0, 7, 0, 5, 0, 0xFF, 0xFF, 0, 0],
    &[0x0F, 0x14, 0, 1, 0, 5, 0, 0x0F, 0xFF, 0x0F, 0xFF],
    &[0x10],
    &[0x11],
    &[0x12],
    &[0x20, 0xFF, 0xFF],
    &[0x21, 1, 2, 3, 4],
    &[0x22],
    &[0xF0],
    &[0x00, 0x00],
    &[0xFF],
    &[0x20, 0x00],
];

fn dvbsubdec_target(bytes: &[u8], chunking: Chunking) {
    let pieces: Vec<&[u8]> = coarse(bytes, chunking).pieces(bytes).collect();
    let group = 1 + bytes.len() % 3;
    for flush in [None, Some(1 + bytes.len() % 3)] {
        let mut h = Harness::new(DvbSubDec::new());
        let driven = drive_pieces(&mut h, &pieces, flush, &|i, buf| {
            buf.pts = Timestamp::from_millis((i / group) as u64 * 500);
            buf.duration = Timestamp::from_millis(400);
        });
        assert!(!driven.failed, "dvbsubdec drops a bad set, it does not fail");
        assert!(driven.out_buffers <= pieces.len() + bytes.len() / 6 + 1);
    }
}

#[test]
fn dvbsubdec_element_survives_corruption() {
    let fixtures = dvb_fixtures();
    let gate = Corpus {
        name: "dvbsubdec_element",
        inputs: &inputs(&fixtures),
        tokens: DVB_TOKENS,
        iters: iterations(400),
        sweep: 16,
        tolerate: &[],
    };
    let bound = PIXEL_BUDGET + 4 * PIXEL_BUDGET + 4 * MIB + 4 * MIB;
    corpus_gate(&gate, &heap_bounded(bound, 64, dvbsubdec_target));
}

fn vobsub_fields(bytes: &[u8]) -> Vec<usize> {
    let control = bytes.get(2..4).map_or(0, |b| usize::from(u16::from_be_bytes([b[0], b[1]])));
    let mut fields = vec![0, 2, 4];
    fields.extend((control..control + 24).filter(|&o| o < bytes.len()));
    fields
}

fn vobsub_fixtures() -> Vec<Fixture> {
    use pf_text::vobsubdec::fixtures::*;
    let (top, bottom) = (field(&[&[(1, 3), (2, 0)], &[(3, 10)]]), field(&[&[(2, 0)], &[(1, 0)]]));
    let chain = [
        Dcsq {
            delay: 0,
            commands: [
                setup(&top, [15, 15, 15, 15], (8, 8, 17, 11)),
                colour_change(&[
                    0, 0, 0x10, 1, 0, 2, 0x30, 0x10, 0xFF, 0xF0, 0x0F, 0xFF, 0xFF, 0xFF,
                ]),
                show(),
            ]
            .concat(),
        },
        Dcsq { delay: 90, commands: hide() },
    ];
    let make = |name, bytes: Vec<u8>| Fixture { name, fields: vobsub_fields(&bytes), bytes };
    vec![
        make("vobsub_minimal", minimal()),
        make("vobsub_never_hidden", never_hidden()),
        make("vobsub_lines_and_change", unit_with(&top, &bottom, &chain, Last::Itself)),
        Fixture { name: "vobsub_idx", bytes: IDX.to_vec(), fields: text_fields(IDX) },
    ]
}

const VOBSUB_TOKENS: &[&[u8]] = &[
    &[0xFF, 0xFF],
    &[0, 0],
    &[0x00, 0x04, 0x00, 0x04],
    &[0x05, 0, 0, 0x3F, 0xF0, 0x03, 0xFF],
    &[0x06, 0xFF, 0xFF, 0xFF, 0xFF],
    &[0x07, 0xFF, 0xFF],
    &[0x01],
    &[0x02],
    &[0xFF],
    b"palette: ",
    b"size: 4096x4096\n",
    b"ffffff, ",
];

fn vobsubdec_target(bytes: &[u8], chunking: Chunking) {
    let pieces: Vec<&[u8]> = coarse(bytes, chunking).pieces(bytes).collect();
    for flush in [None, Some(1 + bytes.len() % 3)] {
        let mut h = Harness::new(VobSubDec::new());
        let driven = drive_pieces(&mut h, &pieces, flush, &|i, buf| {
            buf.pts = Timestamp::from_millis(i as u64 * 500);
            buf.duration = Timestamp::from_millis(400);
        });
        assert!(!driven.failed, "vobsubdec drops a bad unit, it does not fail");
        assert!(driven.out_buffers <= pieces.len(), "one picture a unit at most");
        assert!(
            driven.out_bytes
                <= pieces.len() * (BITMAP_HEADER_LEN + BITMAP_RECT_LEN + VOBSUB_MAX_BITMAP)
        );
    }
}

#[test]
fn vobsubdec_element_survives_corruption() {
    let fixtures = vobsub_fixtures();
    let gate = Corpus {
        name: "vobsubdec_element",
        inputs: &inputs(&fixtures),
        tokens: VOBSUB_TOKENS,
        iters: iterations(400),
        sweep: 16,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(VOBSUB_MAX_BITMAP + 4 * MIB, 64, vobsubdec_target));
}

const FRAMES: [(u32, u32); 6] = [(64, 48), (1, 1), (2, 2), (17, 9), (96, 54), (3, 120)];

const MAX_COMPOSITES: usize = 10;

const RING_LIMIT: usize = 4096;

fn record(script: &mut Vec<u8>, kind: u8, pts: u8, duration: u8, payload: &[u8]) -> usize {
    let at = script.len();
    script.extend_from_slice(&[kind, pts, duration]);
    script.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    script.extend_from_slice(payload);
    at
}

fn stamp(byte: u8) -> Timestamp {
    if byte == 255 { Timestamp::NONE } else { Timestamp::from_millis(byte as u64 * 100) }
}

fn bitmap(width: u32, height: u32, at: (u32, u32), video: (u32, u32)) -> Vec<u8> {
    let rgba = (0..width * height * 4).map(|i| (i * 37 % 256) as u8).collect();
    pgs::encode_bitmap(&DisplaySet {
        rgba,
        width,
        height,
        x: at.0,
        y: at.1,
        video_width: video.0,
        video_height: video.1,
    })
}

fn script(size: u8, pixfmt: u8, records: &[(u8, u8, u8, Vec<u8>)]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = vec![size, pixfmt, 0];
    let mut fields = vec![0, 1];
    for (kind, pts, duration, payload) in records {
        let at = record(&mut bytes, *kind, *pts, *duration, payload);
        fields.push(at + 3);
        if *kind == 1 {
            fields.extend((0..6).map(|n| at + 6 + 4 + 4 * n));
        }
    }
    (bytes, fields)
}

fn overlay_fixtures() -> Vec<Fixture> {
    let text = cue_payload;
    let captions = vec![
        (0, 0, 20, text("Hello\nworld")),
        (2, 5, 0, Vec::new()),
        (1, 10, 20, bitmap(10, 6, (20, 30), (64, 48))),
        (2, 12, 0, Vec::new()),
        (0, 12, 30, text("a second cue, wider than the frame it is drawn on")),
        (2, 15, 0, Vec::new()),
        (1, 20, 0, pgs::encode_bitmap(&DisplaySet::clear(64, 48))),
        (2, 50, 0, Vec::new()),
    ];
    let oversized = vec![
        (1, 0, 50, bitmap(24, 16, (10, 5), (17, 9))),
        (2, 1, 0, Vec::new()),
        (1, 2, 50, bitmap(8, 8, (1900, 1070), (1920, 1080))),
        (2, 3, 0, Vec::new()),
        (1, 4, 50, bitmap(12, 10, (0, 0), (4, 3))),
        (2, 5, 0, Vec::new()),
        (1, 6, 50, bitmap(3, 40, (1, 0), (0, 0))),
        (2, 7, 0, Vec::new()),
    ];
    let hostile = vec![
        (0, 0, 90, text("tab\there \u{7f} \u{1} \u{65e5}\u{672c}\u{8a9e} \u{1f600}")),
        (0, 0, 90, text("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14")),
        (0, 255, 10, text("no timestamp")),
        (0, 1, 255, text("no duration")),
        (2, 2, 0, Vec::new()),
        (2, 255, 0, Vec::new()),
        (3, 0, 0, Vec::new()),
        (0, 3, 20, text("after the flush")),
        (2, 3, 0, Vec::new()),
    ];
    let mut out = Vec::new();
    for (name, size, pixfmt, records) in [
        ("captions_64x48_i420", 0, 0, &captions),
        ("captions_96x54_nv12", 4, 1, &captions),
        ("oversized_bitmaps_17x9", 3, 0, &oversized),
        ("oversized_bitmaps_2x2_nv12", 2, 1, &oversized),
        ("hostile_text_64x48", 0, 1, &hostile),
        ("hostile_text_3x120", 5, 0, &hostile),
        ("hostile_text_1x1", 1, 0, &hostile),
    ] {
        let (bytes, fields) = script(size, pixfmt, records);
        out.push(Fixture { name, bytes, fields });
    }
    out
}

/// The payload's pixels under another geometry, one rect of extreme placement.
fn reshaped(payload: &[u8], shape: usize) -> Option<Vec<u8>> {
    let pixels = (payload.len().checked_sub(BITMAP_HEADER_LEN + BITMAP_RECT_LEN)? / 4) as u32;
    if pixels == 0 {
        return None;
    }
    let (width, height, x, y, video) = match shape {
        0 => (pixels, 1, 0, 0, (0, 0)),
        1 => (1, pixels, u32::MAX, u32::MAX, (1, 1)),
        2 => (pixels, 1, u32::MAX / 2, 7, (u32::MAX, u32::MAX)),
        _ => (1, pixels, 0, u32::MAX, (65_535, 1)),
    };
    let mut out = vec![0u8; payload_len(&[(width, height)])?];
    let mut b = BitmapBuilder::new(&mut out, video.0, video.1, 1).ok()?;
    let px = b.rect(x, y, width, height).ok()?;
    px.copy_from_slice(&payload[BITMAP_HEADER_LEN + BITMAP_RECT_LEN..][..pixels as usize * 4]);
    b.finish().ok()?;
    Some(out)
}

fn overlay_target(bytes: &[u8], _: Chunking) {
    let Some((&[size, pixfmt, _], mut rest)) = bytes.split_first_chunk::<3>() else {
        return;
    };
    let (w, h) = FRAMES[size as usize % FRAMES.len()];
    let pixfmt = if pixfmt & 1 == 0 { PixelFormat::I420 } else { PixelFormat::Nv12 };
    let mut frame = vec![128u8; frame_size(pixfmt, w, h)];
    let mut overlay = SubtitleOverlay::new();
    let (bus, _messages) = Bus::channel();
    let mut ctx = Ctx::new(Pool::bounded(64, 1), bus, ElementId(0), FormatId(0), 1);
    overlay.start(&mut ctx).expect("start");
    let mut composites = 0u64;
    let mut paint = |overlay: &mut SubtitleOverlay, pts: Timestamp| {
        if composites < MAX_COMPOSITES as u64 {
            composites += 1;
            overlay.composite_frame(&mut frame, w, h, pixfmt, pts);
        }
    };
    while let Some((&[kind, pts, duration, a, b, c], tail)) = rest.split_first_chunk::<6>() {
        let len = (u32::from_be_bytes([0, a, b, c]) as usize).min(tail.len());
        let (payload, next) = tail.split_at(len);
        rest = next;
        let (pts, duration) = (stamp(pts), stamp(duration));
        // The views want a pool slot's alignment, the script packs payloads byte-tight.
        let payload = &payload.to_vec();
        match kind % 4 {
            0 => overlay.ingest_text(payload, pts, duration),
            1 => {
                overlay.ingest_bitmap(payload, pts, duration);
                paint(&mut overlay, pts);
                let first = payload.len() / 4 % 4;
                for shape in [first, (first + 1) % 4] {
                    if let Some(wire) = reshaped(payload, shape) {
                        overlay.ingest_bitmap(&wire, pts, duration);
                        paint(&mut overlay, pts);
                    }
                }
            }
            2 => paint(&mut overlay, pts),
            _ => {
                overlay.event(&mut ctx, &Event::FlushStart).expect("flush");
                assert_eq!(overlay.pending_cues(), 0, "a flush drops the timeline");
            }
        }
        assert!(overlay.pending_cues() <= RING_LIMIT, "{} cues held", overlay.pending_cues());
    }
    let mut short = vec![128u8; frame.len() / 2];
    overlay.composite_frame(&mut short, w, h, pixfmt, Timestamp::ZERO);
    assert!(short.iter().all(|&p| p == 128));
    assert!(overlay.with_caption <= composites + 1 && overlay.with_bitmap <= composites + 1);
}

#[test]
fn overlay_survives_hostile_cues_and_bitmaps() {
    let fixtures = overlay_fixtures();
    let gate = Corpus {
        name: "overlay",
        inputs: &inputs(&fixtures),
        tokens: TEXT_TOKENS,
        iters: iterations(300),
        sweep: 12,
        tolerate: &[],
    };
    corpus_gate(&gate, &heap_bounded(256 * KIB, 24, overlay_target));
}

#[test]
fn fixtures_are_valid() {
    // The object's size, then the crop of it drawn, the third crops (1, 1, 4, 2) to 4x1.
    let sizes = [((24, 4), (0, 0, 24, 4)), ((200, 3), (0, 0, 200, 3)), ((20, 2), (1, 1, 4, 1))];
    for (fixture, ((w, h), (cx, cy, cw, ch))) in pgs_fixtures().iter().zip(sizes) {
        let ds = pgs::parse_display_set(&fixture.bytes).expect(fixture.name);
        assert_eq!((ds.width, ds.height), (cw as u32, ch as u32), "{}", fixture.name);
        let want = pattern(w, h);
        for (i, px) in ds.rgba.chunks(4).enumerate() {
            let index = want[(cy + i / cw) * w + cx + i % cw];
            assert_eq!(px[3], if index == 0 { 0 } else { 255 }, "{}", fixture.name);
        }
    }
    let mut state = pgs::PgsState::new();
    let two = pgs::parse_display_set_in(&pgs_fixtures()[2].bytes, &mut state).expect("two");
    let rects: Vec<_> = two.rects().collect();
    assert_eq!(rects, [(10, 20, 4, 1), (30, 40, 12, 3)], "both objects, the first cropped");
    let clear = pgs::parse_display_set(&pgs_fixtures()[3].bytes).expect("clear");
    assert!(clear.is_clear());
    let sup = frame("sup", &caption(24, 4, 2), true);
    let sets = pgs::split_sup(&sup.bytes);
    assert_eq!(sets.len(), 1);
    assert_eq!(pgs::parse_display_set(&sets[0].2).map(|ds| ds.width), Ok(24));

    let set = &pgs_fixtures()[1].bytes;
    let pieces: Vec<&[u8]> = set.chunks(40).collect();
    let mut h = Harness::new(PgsDec::new());
    let stamp_all = |_: usize, buf: &mut Buffer| buf.pts = Timestamp::from_millis(40);
    let driven = drive_pieces(&mut h, &pieces, None, &stamp_all);
    assert_eq!(
        (driven.out_buffers, driven.out_bytes),
        (1, BITMAP_HEADER_LEN + BITMAP_RECT_LEN + 200 * 3 * 4)
    );

    for fixture in overlay_fixtures().iter().take(2) {
        let (mut o, mut painted) = (SubtitleOverlay::new(), 0);
        let (w, h) = FRAMES[fixture.bytes[0] as usize];
        let pixfmt = if fixture.bytes[1] == 0 { PixelFormat::I420 } else { PixelFormat::Nv12 };
        let mut rest = &fixture.bytes[3..];
        while let Some((&[kind, pts, duration, _, b, c], tail)) = rest.split_first_chunk::<6>() {
            let (payload, next) = tail.split_at(u16::from_be_bytes([b, c]) as usize);
            rest = next;
            let payload = &payload.to_vec();
            match kind {
                0 => o.ingest_text(payload, stamp(pts), stamp(duration)),
                1 => o.ingest_bitmap(payload, stamp(pts), stamp(duration)),
                _ => {
                    let mut frame = vec![128u8; frame_size(pixfmt, w, h)];
                    o.composite_frame(&mut frame, w, h, pixfmt, stamp(pts));
                    painted += usize::from(frame.iter().any(|&p| p != 128));
                }
            }
        }
        assert_eq!((painted, o.with_caption, o.with_bitmap), (3, 3, 2), "{}", fixture.name);
    }
}

const PCS_ONE_OBJECT: [u8; 22] = [
    0x16, 0x00, 0x13, 0x07, 0x80, 0x04, 0x38, 0x10, 0x00, 0x01, 0x80, 0x00, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

#[test]
fn pgs_object_size_is_bounded_by_its_coded_data() {
    let mut set = PCS_ONE_OBJECT.to_vec();
    set.extend_from_slice(&[
        0x15, 0x00, 0x0D, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x06, 0x10, 0x00, 0x08, 0x70, 0x00,
        0x00,
    ]);
    set.extend_from_slice(&[SEG_END, 0, 0]);
    let (parsed, grew) = heap_during(|| pgs::parse_display_set(&set));
    assert_eq!(
        parsed.map(|ds| (ds.width, ds.height)),
        Err(PgsError::Malformed("object size exceeds what its coded data can hold"))
    );
    assert!(grew < 64 * KIB, "refusing 41 bytes grew the heap by {grew} bytes");

    let mut h = Harness::new(PgsDec::new());
    let (driven, grew) = heap_during(|| drive(&mut h, &set, Chunking::Whole, None));
    assert_eq!((driven.out_buffers, driven.failed), (0, false), "dropped, not fatal");
    assert!(grew < 4 * MIB, "pgsdec grew the heap by {grew} bytes on 41 bytes");
}

#[test]
fn pgs_object_larger_than_its_video_is_refused() {
    // 2000x2000 with 800 coded bytes passes the coded-data bound and fails the video bound.
    let mut set = PCS_ONE_OBJECT.to_vec();
    set.extend_from_slice(&[0x15, 0x03, 0x2B, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x03, 0x24]);
    set.extend_from_slice(&[0x07, 0xD0, 0x07, 0xD0]);
    set.extend_from_slice(&[0u8; 800]);
    set.extend_from_slice(&[SEG_END, 0, 0]);
    let (parsed, grew) = heap_during(|| pgs::parse_display_set(&set));
    assert_eq!(
        parsed.map(|ds| (ds.width, ds.height)),
        Err(PgsError::Malformed("object larger than its video"))
    );
    assert!(grew < 64 * KIB, "refusing the object grew the heap by {grew} bytes");
}

fn many_objects(count: u16) -> Vec<u8> {
    let mut set = PCS_ONE_OBJECT.to_vec();
    set.extend_from_slice(&[SEG_PDS, 0, 7, 0, 0, 1, 235, 128, 128, 255]);
    for id in 0..count {
        set.extend_from_slice(&[SEG_ODS, 0x00, 0x0E]);
        set.extend_from_slice(&id.to_be_bytes());
        set.extend_from_slice(&[0x00, 0xC0, 0x00, 0x00, 0x07]);
        set.extend_from_slice(&[0x00, 0x01, 0x00, 0x01, 0x01, 0x00, 0x00]);
    }
    set.extend_from_slice(&[SEG_END, 0, 0]);
    set
}

#[test]
fn pgs_display_set_with_too_many_objects_is_refused() {
    assert_eq!(
        pgs::parse_display_set(&many_objects(65)),
        Err(PgsError::Malformed("display set defines too many objects"))
    );
    let ds = pgs::parse_display_set(&many_objects(64)).expect("64 objects");
    assert_eq!((ds.width, ds.height), (1, 1));
}

fn composite_64x48(overlay: &mut SubtitleOverlay) -> Vec<u8> {
    let mut frame = vec![128u8; frame_size(PixelFormat::I420, 64, 48)];
    overlay.composite_frame(&mut frame, 64, 48, PixelFormat::I420, Timestamp::ZERO);
    frame
}

#[test]
fn a_caption_wider_than_the_frame_is_rasterised_to_the_frame() {
    let line = cue_payload(&"W".repeat(100_000));
    let mut overlay = SubtitleOverlay::new();
    let (frame, grew) = heap_during(|| {
        overlay.ingest_text(&line, Timestamp::ZERO, Timestamp::from_secs(1));
        composite_64x48(&mut overlay)
    });
    assert!(frame[..64 * 48].iter().any(|&p| p > 200), "the caption is drawn");
    assert!(grew < MIB, "a 100 KB cue grew the heap by {grew} bytes");
}

#[test]
fn a_caption_of_more_lines_than_fit_keeps_the_ones_that_fit() {
    let lines = cue_payload(&"W\n".repeat(50_000));
    let mut overlay = SubtitleOverlay::new();
    let (frame, grew) = heap_during(|| {
        overlay.ingest_text(&lines, Timestamp::ZERO, Timestamp::from_secs(1));
        composite_64x48(&mut overlay)
    });
    assert!(frame[..64 * 8].iter().all(|&p| p == 128), "nothing is piled at the top edge");
    assert!(frame[64 * 20..64 * 40].iter().any(|&p| p > 200), "the last lines are drawn");
    assert!(grew < MIB, "a 100 KB cue grew the heap by {grew} bytes");
}

#[test]
fn the_cue_timeline_is_bounded() {
    let mut overlay = SubtitleOverlay::new();
    for n in 0..10_000u32 {
        let start = Timestamp::from_millis(10_000 - n as u64);
        overlay.ingest_text(&cue_payload(&format!("cue {n}")), start, Timestamp::from_secs(1));
    }
    assert_eq!(overlay.pending_cues(), RING_LIMIT);
}

#[test]
fn a_geometry_the_frame_cannot_hold_is_refused_before_plane_sizes_are_summed() {
    let mut overlay = SubtitleOverlay::new();
    overlay.ingest_text(&cue_payload("caption"), Timestamp::ZERO, Timestamp::from_secs(1));
    let mut frame = [128u8; 16];
    overlay.composite_frame(&mut frame, u32::MAX, u32::MAX, PixelFormat::I420, Timestamp::ZERO);
    assert_eq!(frame, [128u8; 16]);
    assert_eq!(overlay.with_caption, 0);
}
