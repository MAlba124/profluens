//! ContentEncodings, DefaultDuration, codec heads, attachments and chapters through the demuxer.

// A test's own fixtures and files, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::{Arc, Mutex};

use pf_mkv::ebml::{self, id};
use pf_mkv::inflate::adler32;
use pf_mkv::{MatroskaReader, MkvDemux, MkvExtras, Refusal};
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::SinkPad;
use profluens_elements::io::FileSrc;

const MS: u64 = 1_000_000;

const HEAD: &[u8] = b"[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nStyle: Default,Gentium,48\n\n[Events]\nFormat: ReadOrder, Layer, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
const HEAD_Z: &[u8] = &[
    0x78, 0xda, 0x35, 0x8b, 0xc1, 0x0a, 0x83, 0x30, 0x10, 0x44, 0xef, 0xf9, 0x8a, 0xbd, 0xbb, 0x14,
    0x0f, 0x39, 0x94, 0x9c, 0x6b, 0x4b, 0xc1, 0xb6, 0xa0, 0xe2, 0x45, 0x7a, 0x08, 0xba, 0x29, 0x01,
    0x13, 0x25, 0x5d, 0xa5, 0xf9, 0xfb, 0xc6, 0x16, 0x2f, 0x33, 0xf3, 0xe0, 0x4d, 0x57, 0xf7, 0xc1,
    0xce, 0x0c, 0x57, 0x6f, 0xa6, 0xa7, 0xf8, 0x43, 0x13, 0x67, 0x52, 0xb0, 0xca, 0x43, 0x9e, 0x67,
    0x42, 0x74, 0xad, 0xcc, 0xa0, 0xe6, 0x38, 0xd2, 0x3b, 0x09, 0x5b, 0x2b, 0x38, 0x91, 0xd1, 0xcb,
    0xc8, 0x78, 0x21, 0xcf, 0x76, 0x71, 0x28, 0x8f, 0xc9, 0x2b, 0xd6, 0x44, 0xc9, 0x39, 0x4f, 0xc1,
    0x69, 0x56, 0x50, 0x91, 0x1e, 0x1e, 0x61, 0xa0, 0x80, 0x50, 0xea, 0xb8, 0xd5, 0xef, 0x8d, 0x70,
    0xd7, 0x2e, 0xe5, 0x4d, 0x87, 0x97, 0xf5, 0xe5, 0x3e, 0xaa, 0x7d, 0xb4, 0x08, 0x85, 0x31, 0xd4,
    0x33, 0x42, 0x43, 0x1f, 0x16, 0x5f, 0xfc, 0xc6, 0x35, 0x33,
];
const LINE: &[u8] =
    b"0,0,Default,,0,0,0,,{\\fnGentium}Hello there, this is the first line of the script.";
const LINE_Z: &[u8] = &[
    0x78, 0xda, 0x33, 0xd0, 0x31, 0xd0, 0x71, 0x49, 0x4d, 0x4b, 0x2c, 0xcd, 0x29, 0xd1, 0x01, 0x32,
    0x41, 0x50, 0xa7, 0x3a, 0x26, 0x2d, 0xcf, 0x3d, 0x35, 0xaf, 0x24, 0xb3, 0x34, 0xb7, 0xd6, 0x23,
    0x35, 0x27, 0x27, 0x5f, 0xa1, 0x24, 0x23, 0xb5, 0x28, 0x55, 0x07, 0x48, 0x65, 0x16, 0x2b, 0x00,
    0x11, 0x90, 0xab, 0x90, 0x96, 0x59, 0x54, 0x5c, 0xa2, 0x90, 0x93, 0x99, 0x97, 0xaa, 0x90, 0x9f,
    0x06, 0x16, 0x29, 0x4e, 0x2e, 0xca, 0x2c, 0x28, 0xd1, 0x03, 0x00, 0x53, 0xfb, 0x1b, 0xed,
];

const SYNC: [u8; 2] = [0x0B, 0x77];
const VOL: [u8; 8] = [0, 0, 1, 0x20, 0x08, 0xC8, 0x0D, 0x40];

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> =
        if data.is_empty() { vec![&[][..]] } else { data.chunks(65_535).collect() };
    for (k, c) in chunks.iter().enumerate() {
        out.push((k + 1 == chunks.len()) as u8);
        out.extend_from_slice(&(c.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        out.extend_from_slice(c);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn element(out: &mut Vec<u8>, element_id: &[u8], body: &[u8]) {
    ebml::write_binary(out, element_id, body);
}

fn master(out: &mut Vec<u8>, element_id: &[u8], fill: impl FnOnce(&mut Vec<u8>)) {
    let mut body = Vec::new();
    fill(&mut body);
    element(out, element_id, &body);
}

fn compression(t: &mut Vec<u8>, scope: u64, algo: u64, settings: &[u8]) {
    master(t, id::CONTENT_ENCODINGS, |e| {
        master(e, id::CONTENT_ENCODING, |c| {
            ebml::write_uint(c, id::CONTENT_ENCODING_ORDER, 0);
            ebml::write_uint(c, id::CONTENT_ENCODING_SCOPE, scope);
            ebml::write_uint(c, id::CONTENT_ENCODING_TYPE, 0);
            master(c, id::CONTENT_COMPRESSION, |z| {
                ebml::write_uint(z, id::CONTENT_COMP_ALGO, algo);
                if !settings.is_empty() {
                    ebml::write_binary(z, id::CONTENT_COMP_SETTINGS, settings);
                }
            });
        });
    });
}

fn track(b: &mut Vec<u8>, number: u64, kind: u64, codec: &str, fill: impl FnOnce(&mut Vec<u8>)) {
    master(b, id::TRACK_ENTRY, |t| {
        ebml::write_uint(t, id::TRACK_NUMBER, number);
        ebml::write_uint(t, id::TRACK_TYPE, kind);
        ebml::write_string(t, id::CODEC_ID, codec);
        fill(t);
    });
}

fn block(track: u8, rel: i16, flags: u8, lace: &[u8], frames: &[&[u8]]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend_from_slice(&rel.to_be_bytes());
    b.push(flags);
    b.extend_from_slice(lace);
    frames.iter().for_each(|f| b.extend_from_slice(f));
    b
}

fn ac3(seed: usize, n: usize) -> Vec<u8> {
    let mut f = SYNC.to_vec();
    f.extend((0..n - 2).map(|i| ((i * 31 + seed * 7) % 253) as u8));
    f
}

fn text_of(k: usize) -> Vec<u8> {
    format!("{k},0,Default,,0,0,0,,Line {k} of the script, said twice. Line {k} of the script, said twice.").into_bytes()
}

fn pcm_be(seed: i16, frames: usize) -> Vec<u8> {
    (0..frames * 2)
        .flat_map(|i| {
            seed.wrapping_mul(97).wrapping_add((i as i16).wrapping_mul(300)).to_be_bytes()
        })
        .collect()
}

fn film() -> (Vec<u8>, Vec<u8>) {
    let big: Vec<u8> = (0..40_000usize).map(|i| (i * 131 % 256) as u8).collect();
    let mut seg = Vec::new();
    let seek_at = seg.len();
    element(&mut seg, id::VOID, &[0; 60]);
    master(&mut seg, id::INFO, |b| {
        ebml::write_uint(b, id::TIMESTAMP_SCALE, 1_000_000);
        ebml::write_f64(b, id::DURATION, 2_000.0);
    });
    master(&mut seg, id::TRACKS, |b| {
        track(b, 1, 1, "V_MPEG4/ISO/ASP", |t| {
            ebml::write_binary(t, id::CODEC_PRIVATE, &VOL);
            ebml::write_uint(t, id::DEFAULT_DURATION, 40 * MS);
            master(t, id::VIDEO, |v| {
                ebml::write_uint(v, id::PIXEL_WIDTH, 320);
                ebml::write_uint(v, id::PIXEL_HEIGHT, 240);
            });
        });
        track(b, 2, 2, "A_AC3", |t| {
            ebml::write_uint(t, id::DEFAULT_DURATION, 32 * MS);
            compression(t, 1, 3, &SYNC);
            master(t, id::AUDIO, |a| {
                ebml::write_f64(a, id::SAMPLING_FREQUENCY, 48_000.0);
                ebml::write_uint(a, id::CHANNELS, 2);
            });
        });
        track(b, 3, 17, "S_TEXT/ASS", |t| {
            ebml::write_binary(t, id::CODEC_PRIVATE, HEAD_Z);
            compression(t, 3, 0, &[]);
        });
        track(b, 4, 2, "A_PCM/INT/BIG", |t| {
            master(t, id::AUDIO, |a| {
                ebml::write_f64(a, id::SAMPLING_FREQUENCY, 8_000.0);
                ebml::write_uint(a, id::CHANNELS, 2);
                ebml::write_uint(a, id::BIT_DEPTH, 16);
            });
        });
        track(b, 5, 1, "V_MS/VFW/FOURCC", |t| {
            let mut bih = vec![0u8; 40];
            bih[..4].copy_from_slice(&40u32.to_le_bytes());
            bih[16..20].copy_from_slice(b"XVID");
            bih.extend_from_slice(&VOL);
            ebml::write_binary(t, id::CODEC_PRIVATE, &bih);
        });
        track(b, 6, 2, "A_OPUS", |t| {
            master(t, id::CONTENT_ENCODINGS, |e| {
                master(e, id::CONTENT_ENCODING, |c| {
                    ebml::write_uint(c, id::CONTENT_ENCODING_TYPE, 1);
                    master(c, id::CONTENT_ENCRYPTION, |_| {});
                });
            });
        });
    });
    master(&mut seg, id::CHAPTERS, |c| {
        master(c, id::EDITION_ENTRY, |e| {
            ebml::write_uint(e, id::EDITION_UID, 1);
            for (uid, start, name) in [(10, 0, "Opening"), (11, 1_000, "Part A")] {
                master(e, id::CHAPTER_ATOM, |a| {
                    ebml::write_uint(a, id::CHAPTER_UID, uid);
                    ebml::write_uint(a, id::CHAPTER_TIME_START, start * MS);
                    master(a, id::CHAPTER_DISPLAY, |d| {
                        ebml::write_string(d, id::CHAP_STRING, name);
                        ebml::write_string(d, id::CHAP_LANGUAGE, "eng");
                    });
                });
            }
        });
    });
    for cluster in 0..2u64 {
        ebml::write_id(&mut seg, id::CLUSTER);
        ebml::write_unknown_size(&mut seg);
        ebml::write_uint(&mut seg, id::TIMESTAMP, cluster * 1_000);
        for k in 0..10usize {
            let n = cluster as usize * 10 + k;
            let rel = (k * 96) as i16;
            let key = if k == 0 { 0x80 } else { 0 };
            let vop = [&[0, 0, 1, 0xB6][..], &[n as u8; 300]].concat();
            element(&mut seg, id::SIMPLE_BLOCK, &block(1, rel, key, &[], &[&vop]));
            let frames: Vec<Vec<u8>> = (0..3).map(|j| ac3(n * 3 + j, 200 + j * 40)).collect();
            let stripped: Vec<&[u8]> = frames.iter().map(|f| &f[2..]).collect();
            let lace = [2u8, stripped[0].len() as u8, stripped[1].len() as u8];
            element(&mut seg, id::SIMPLE_BLOCK, &block(2, rel, 0x82, &lace, &stripped));
            let pcm = pcm_be(n as i16, 64);
            element(&mut seg, id::SIMPLE_BLOCK, &block(4, rel, 0x80, &[], &[&pcm]));
            element(&mut seg, id::SIMPLE_BLOCK, &block(5, rel, key, &[], &[&vop]));
            element(&mut seg, id::SIMPLE_BLOCK, &block(6, rel, 0x80, &[], &[b"sealed"]));
            if k % 3 == 0 {
                let z = match (cluster, k) {
                    (0, 0) => LINE_Z.to_vec(),
                    (0, 3) => b"\x78\x01\x01\xff".to_vec(),
                    _ => zlib_stored(&text_of(n)),
                };
                master(&mut seg, id::BLOCK_GROUP, |g| {
                    element(g, id::BLOCK, &block(3, rel + 10, 0, &[], &[&z]));
                    ebml::write_uint(g, id::BLOCK_DURATION, 900);
                });
            }
        }
        if cluster == 1 {
            element(&mut seg, id::SIMPLE_BLOCK, &block(3, 990, 0x80, &[], &[&zlib_stored(&big)]));
        }
    }
    let attachments_at = seg.len() as u64;
    master(&mut seg, id::ATTACHMENTS, |a| {
        for (name, mime, data, uid) in [
            ("Gentium.ttf", "application/x-truetype-font", vec![0u8, 1, 0, 0, 9, 9], 7),
            ("cover.jpg", "image/jpeg", vec![0xFF, 0xD8, 0xFF], 8),
        ] {
            master(a, id::ATTACHED_FILE, |f| {
                ebml::write_string(f, id::FILE_NAME, name);
                ebml::write_string(f, id::FILE_MEDIA_TYPE, mime);
                ebml::write_binary(f, id::FILE_DATA, &data);
                ebml::write_uint(f, id::FILE_UID, uid);
            });
        }
    });
    let mut head = Vec::new();
    master(&mut head, id::SEEK_HEAD, |s| {
        master(s, id::SEEK, |e| {
            ebml::write_binary(e, id::SEEK_ID, id::ATTACHMENTS);
            ebml::write_binary(e, id::SEEK_POSITION, &attachments_at.to_be_bytes());
        });
    });
    let void_len = 62 - head.len();
    element(&mut head, id::VOID, &vec![0; void_len - 2]);
    assert_eq!(head.len(), 62);
    seg.splice(seek_at..seek_at + 62, head);
    let mut out = Vec::new();
    master(&mut out, id::EBML, |b| ebml::write_string(b, id::DOC_TYPE, "matroska"));
    ebml::write_id(&mut out, id::SEGMENT);
    ebml::write_unknown_size(&mut out);
    out.extend_from_slice(&seg);
    (out, big)
}

fn header(file: &[u8]) -> Vec<u8> {
    let at = file.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
    file[..at].to_vec()
}

#[test]
fn the_reader_undoes_each_encoding_and_stamps_laced_frames_by_default_duration() {
    let (file, big) = film();
    let mut r = MatroskaReader::new();
    r.push(&file).expect("the file reads");
    let t = r.tracks();
    assert_eq!(t[2].codec_private, HEAD, "the private scope is undone at the header");
    assert_eq!(t[0].default_duration_ns, Some(40 * MS));
    assert_eq!(t[5].coding.refused, Some(Refusal::Encrypted));
    let mut by = vec![Vec::new(); 7];
    while let Some(f) = r.next_frame() {
        by[f.track_number as usize].push(f);
    }
    let undecoded = r.take_undecoded().map(|(track, _)| track);
    assert_eq!(undecoded, Some(3), "the broken zlib frame is dropped and said");
    let audio: Vec<(u64, Option<u64>)> =
        by[2].iter().take(6).map(|f| (f.pts_ns, f.duration_ns)).collect();
    let want: Vec<(u64, Option<u64>)> =
        [0, 32, 64, 96, 128, 160].iter().map(|ms| (ms * MS, Some(32 * MS))).collect();
    assert_eq!(audio, want);
    assert!(
        by[2].iter().enumerate().all(|(k, f)| f.data == ac3(k, 200 + (k % 3) * 40)),
        "sync words back on"
    );
    assert!(
        by[1].iter().all(|f| f.duration_ns == Some(40 * MS)),
        "a SimpleBlock lasts the default"
    );
    let text: Vec<&[u8]> = by[3].iter().map(|f| &f.data[..]).collect();
    let mut want: Vec<Vec<u8>> = vec![LINE.to_vec()];
    want.extend([6, 9, 10, 13, 16, 19].map(text_of));
    want.push(big);
    assert_eq!(text, want.iter().map(|w| &w[..]).collect::<Vec<_>>());
    assert_eq!(by[3][0].duration_ns, Some(900 * MS), "BlockDuration wins");
    assert_eq!(by[3].last().unwrap().duration_ns, None, "no duration, no default");
    assert!(by[6].iter().all(|f| f.data == b"sealed"), "a refused track's frames pass as stored");
}

#[test]
fn a_track_is_named_by_its_codec_and_what_its_encoding_allows() {
    let (file, _) = film();
    let mut r = MatroskaReader::new();
    r.push(&header(&file)).unwrap();
    let families: Vec<&str> = r.tracks().iter().map(pf_mkv::track_family).collect();
    assert_eq!(
        families,
        ["mpeg4/asp", "ac3", "subtitle/ass", "audio/raw", "mpeg4/asp", "encrypted"]
    );
    let firsts: Vec<&str> = r.tracks().iter().map(|t| pf_mkv::track_offers(t)[0].family).collect();
    assert_eq!(firsts, families, "a pad's first offer is its family");
}

static ANY: [OfferDesc; 1] = [OfferDesc::wildcard()];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY }];
static REC: ElementDesc = ElementDesc {
    name: "rec",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

type Buf = (Option<u64>, Option<u64>, Vec<u8>);
type Got = Arc<Mutex<Vec<Buf>>>;

struct Rec(Got);

impl Element for Rec {
    fn desc(&self) -> &'static ElementDesc {
        &REC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.0.lock().unwrap().push((
                buf.pts.nanos(),
                buf.duration.nanos(),
                buf.memory.data().to_vec(),
            ));
        }
        Ok(Flow::Ok)
    }
}

fn demux(name: &str, pull: bool) -> (Vec<Vec<Buf>>, Vec<&'static str>, MkvExtras) {
    demux_by(name, pull, MkvDemux::new)
}

fn demux_by(
    name: &str,
    pull: bool,
    make: impl FnOnce(Vec<u8>) -> MkvDemux,
) -> (Vec<Vec<Buf>>, Vec<&'static str>, MkvExtras) {
    let (file, _) = film();
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mkv_content");
    std::fs::create_dir_all(&dir).expect("a tmp dir");
    let path = dir.join(name);
    std::fs::write(&path, &file).expect("write the fixture");
    let mut p = pf_reactor::pipeline();
    let src = FileSrc::new(path.to_str().expect("utf-8"));
    let src = p.add(if pull { src } else { src.push_only() });
    let demux = make(header(&file));
    let extras = demux.extras();
    let demux = p.add(demux);
    p.link(src.src(FileSrc::OUT), demux.sink(MkvDemux::IN)).expect("link");
    let (mut got, mut families) = (Vec::new(), Vec::new());
    for ap in p.preroll().expect("preroll").srcs {
        families.push(
            p.src_families(ap.element, ap.pad).and_then(|f| f.first().copied()).unwrap_or(""),
        );
        let g: Got = Arc::default();
        let rec = p.add(Rec(Arc::clone(&g)));
        p.link(ap.element.src(ap.pad), rec.sink(SinkPad(0))).expect("link");
        got.push(g);
    }
    p.run().expect("run");
    (got.iter().map(|g| g.lock().unwrap().clone()).collect(), families, extras)
}

#[test]
fn the_demuxer_puts_out_decoded_frames_alike_pulled_and_pushed() {
    let (pushed, families, _) = demux("pushed.mkv", false);
    let (pulled, _, _) = demux("pulled.mkv", true);
    assert_eq!(
        families,
        ["mpeg4/asp", "ac3", "subtitle/ass", "audio/raw", "mpeg4/asp", "encrypted"]
    );
    assert_eq!(pushed, pulled, "pulled as pushed");
    assert_eq!(pushed[0][0].2, VOL);
    assert_eq!(pushed[4][0].2, VOL);
    assert_eq!(pushed[0][1].1, Some(40 * MS), "a picture lasts the DefaultDuration");
    let stamps: Vec<Option<u64>> = pushed[1].iter().take(4).map(|b| b.0).collect();
    assert_eq!(stamps, [Some(0), Some(32 * MS), Some(64 * MS), Some(96 * MS)]);
    assert!(pushed[1].iter().all(|b| b.2.starts_with(&SYNC)));
    assert_eq!(pushed[2][0].2, LINE);
    assert_eq!(pushed[2].len(), 8);
    assert_eq!(pushed[2].last().unwrap().2.len(), 40_000);
    let le: Vec<u8> = pcm_be(0, 64).chunks(2).flat_map(|s| [s[1], s[0]]).collect();
    assert_eq!(pushed[3][0].2, le);
}

/// The film with stream `left` given no pad, what the other pads took and the warnings said.
fn demux_without(name: &str, pull: bool, left: Option<u32>) -> (Vec<Vec<Buf>>, Vec<String>) {
    let (file, _) = film();
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mkv_content");
    std::fs::create_dir_all(&dir).expect("a tmp dir");
    let path = dir.join(name);
    std::fs::write(&path, &file).expect("write the fixture");
    let mut p = pf_reactor::pipeline();
    p.set_select_all(false);
    let src = FileSrc::new(path.to_str().expect("utf-8"));
    let src = p.add(if pull { src } else { src.push_only() });
    let demux = p.add(MkvDemux::new(header(&file)));
    p.link(src.src(FileSrc::OUT), demux.sink(MkvDemux::IN)).expect("link");
    p.preroll().expect("preroll");
    let mut got = Vec::new();
    for stream in (0..p.streams(demux).len() as u32).filter(|s| Some(*s) != left) {
        let pad = p.select(demux, stream).expect("a pad");
        let g: Got = Arc::default();
        let rec = p.add(Rec(Arc::clone(&g)));
        p.link(demux.src(pad), rec.sink(SinkPad(0))).expect("link");
        got.push(g);
    }
    p.run().expect("run");
    let mut warnings = Vec::new();
    while let Some(m) = p.bus().try_recv() {
        if let BusMessage::Warning { error, .. } = m {
            warnings.push(error.to_string());
        }
    }
    (got.iter().map(|g| g.lock().unwrap().clone()).collect(), warnings)
}

#[test]
fn a_coded_frame_of_a_track_no_one_takes_is_dropped_undecoded() {
    let (file, _) = film();
    let read = |takes: fn(u64) -> bool| {
        let mut r = MatroskaReader::new();
        r.push(&file).expect("the file reads");
        let frames: Vec<(u64, u64, Vec<u8>)> = std::iter::from_fn(|| r.next_frame_of(takes))
            .map(|f| (f.track_number, f.pts_ns, f.data))
            .collect();
        (frames, r.take_undecoded().map(|(track, _)| track))
    };
    let (all, said) = read(|_| true);
    assert_eq!(said, Some(3), "read, the broken zlib frame is said");
    let (rest, said) = read(|track| track != 3);
    assert_eq!(said, None, "not taken, it was never inflated");
    let want: Vec<_> = all.into_iter().filter(|f| f.0 != 3).collect();
    assert!(want.iter().any(|f| f.0 == 2), "the stripped AC-3 track is coded too and still read");
    assert_eq!(rest, want, "every other track reads alike");
}

#[test]
fn the_demuxer_inflates_no_frame_of_a_stream_without_a_pad() {
    const UNDONE: &str = "content encoding does not undo";
    for pull in [true, false] {
        let name = |what: &str| format!("{what}_{}.mkv", if pull { "pulled" } else { "pushed" });
        let (all, warnings) = demux_without(&name("every"), pull, None);
        assert_eq!(all.len(), 6);
        assert!(warnings.iter().any(|w| w.contains(UNDONE)), "linked, the broken frame is said");
        // stream 2 is the zlib captions, with one frame that does not inflate
        let (rest, warnings) = demux_without(&name("no_captions"), pull, Some(2));
        assert!(!warnings.iter().any(|w| w.contains(UNDONE)), "unlinked, nothing inflated it");
        let want: Vec<_> = all.into_iter().enumerate().filter(|(n, _)| *n != 2).collect();
        let want: Vec<Vec<Buf>> = want.into_iter().map(|(_, bufs)| bufs).collect();
        assert_eq!(rest, want, "the other pads play alike, pull {pull}");
    }
}

#[test]
fn chapters_come_from_the_header_and_attachments_by_range() {
    let (_, _, extras) = demux("extras_pulled.mkv", true);
    assert!(extras.settled());
    let chapters = extras.chapters();
    let titles: Vec<(&str, u64)> =
        chapters.chapters.iter().map(|c| (c.titles[0].text.as_str(), c.start_ns)).collect();
    assert_eq!(titles, [("Opening", 0), ("Part A", 1_000 * MS)]);
    let names: Vec<String> = extras.attachments().iter().map(|a| a.name.clone()).collect();
    assert_eq!(names, ["Gentium.ttf", "cover.jpg"]);
    let fonts = extras.fonts();
    assert_eq!((fonts.len(), fonts[0].uid, &fonts[0].data[..]), (1, 7, &[0u8, 1, 0, 0, 9, 9][..]));
    let (_, _, extras) = demux("extras_pushed.mkv", false);
    assert!(extras.settled());
    assert_eq!(extras.chapters().chapters.len(), 2);
    assert!(extras.attachments().is_empty());
}

#[test]
fn a_shared_head_without_extras_plays_alike_and_settles_empty() {
    let (want, families, _) = demux("plain_pulled.mkv", true);
    let shared = |h: Vec<u8>| MkvDemux::with_shared_head(h.into()).without_extras();
    let (got, got_families, extras) = demux_by("shared_pulled.mkv", true, shared);
    assert_eq!((got, got_families), (want, families));
    assert!(extras.settled(), "settled at preroll with nothing read");
    assert!(extras.chapters().chapters.is_empty() && extras.attachments().is_empty());
    let (_, _, extras) =
        demux_by("shared_pushed.mkv", false, |h| MkvDemux::with_shared_head(h.into()));
    assert_eq!(extras.chapters().chapters.len(), 2, "a shared head keeps its Chapters");
}

fn bits(out: &mut Vec<u8>, acc: &mut (u64, u32), value: u32, len: u32) {
    acc.0 |= u64::from(value) << acc.1;
    acc.1 += len;
    while acc.1 >= 8 {
        out.push(acc.0 as u8);
        (acc.0, acc.1) = (acc.0 >> 8, acc.1 - 8);
    }
}

/// A Huffman code goes out most significant bit first (RFC 1951 section 3.1.1).
fn code(out: &mut Vec<u8>, acc: &mut (u64, u32), value: u32, len: u32) {
    bits(out, acc, value.reverse_bits() >> (32 - len), len);
}

/// zlib of `1 + 258 k` zeros in one fixed Huffman block, a literal then `k` matches of 258 at
/// distance 1 (RFC 1951 section 3.2.6).
fn zlib_zeros(k: usize) -> (Vec<u8>, usize) {
    let (mut out, mut acc) = (vec![0x78, 0x01], (0u64, 0u32));
    bits(&mut out, &mut acc, 0b011, 3);
    code(&mut out, &mut acc, 0x30, 8);
    for _ in 0..k {
        code(&mut out, &mut acc, 0b1100_0101, 8);
        code(&mut out, &mut acc, 0, 5);
    }
    code(&mut out, &mut acc, 0, 7);
    if acc.1 > 0 {
        out.push(acc.0 as u8);
    }
    let n = 1 + 258 * k;
    out.extend_from_slice(&adler32(&vec![0; n]).to_be_bytes());
    (out, n)
}

#[test]
fn a_push_of_zlib_frames_holds_their_coded_size_not_what_they_inflate_to() {
    let (z, n) = zlib_zeros(4064);
    let mut seg = Vec::new();
    master(&mut seg, id::TRACKS, |b| {
        track(b, 1, 0x11, "S_TEXT/UTF8", |t| compression(t, 1, 0, &[]));
    });
    ebml::write_id(&mut seg, id::CLUSTER);
    ebml::write_unknown_size(&mut seg);
    ebml::write_uint(&mut seg, id::TIMESTAMP, 0);
    for k in 0..32 {
        element(&mut seg, id::SIMPLE_BLOCK, &block(1, k, 0x80, &[], &[&z]));
    }
    let mut file = Vec::new();
    master(&mut file, id::EBML, |b| ebml::write_string(b, id::DOC_TYPE, "matroska"));
    ebml::write_id(&mut file, id::SEGMENT);
    ebml::write_unknown_size(&mut file);
    file.extend_from_slice(&seg);
    let mut r = MatroskaReader::new();
    r.push(&file).expect("the file reads");
    assert!(r.held() < 2 * file.len(), "{} octets held for a {} octet push", r.held(), file.len());
    let mut frames = 0;
    while let Some(f) = r.next_frame() {
        assert!(f.data.len() == n && f.data.iter().all(|b| *b == 0), "frame {frames} inflates");
        assert_eq!(f.pts_ns, frames as u64 * MS);
        r.recycle(f.data);
        frames += 1;
    }
    assert_eq!((frames, r.take_undecoded()), (32, None));
}
