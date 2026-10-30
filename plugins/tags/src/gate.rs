//! Parser-level corruption gate over in-memory scans. `tests/corruption.rs` runs the same
//! inputs through the reactor.

// test fixtures
#![allow(clippy::disallowed_methods)]

use pf_sim::corrupt::{
    CLASS_REGION_COST, Chunking, Corpus, Input, Tracking, corpus_gate, heap_bounded, heap_during,
    iterations,
};
use pf_sim::prng::Prng;
use profluens_core::memory::Pool;
use profluens_core::scratch::Arena;

use crate::inputs::{self, Fixture};
use crate::scanner::{Held, Next};
use crate::sniff::sniff;
use crate::store::{ArenaSink, MAX_PICTURES, MAX_TEXT_TAGS};
use crate::{DEFAULT_PREFIX, Format, Props, ScanConfig, fixture};

#[global_allocator]
static HEAP: Tracking = Tracking;

const MAX_READS: usize = 5;

const MAX_READ_BYTES: usize = 32 << 20;

const ALL_FORMATS: [Format; 14] = [
    Format::Flac,
    Format::Mp3,
    Format::Mp4,
    Format::OggOpus,
    Format::OggVorbis,
    Format::OggFlac,
    Format::OggSpeex,
    Format::Wav,
    Format::Aiff,
    Format::Ape,
    Format::WavPack,
    Format::Musepack,
    Format::Mkv,
    Format::Unknown,
];

const OGG_FORMATS: [Format; 4] =
    [Format::OggOpus, Format::OggVorbis, Format::OggFlac, Format::OggSpeex];

#[derive(Clone, Copy, Debug)]
struct Shape {
    prefix: usize,
    tail_len: usize,
    claimed: u64,
    slack: usize,
    force: Option<Format>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Scanned {
    format: Format,
    props: Props,
    reads: usize,
    read_bytes: usize,
    text: Vec<(String, String)>,
    pictures: Vec<(String, usize)>,
}

fn read(file: &[u8], at: u64, want: usize, slack: usize) -> &[u8] {
    let Some(rest) = usize::try_from(at).ok().and_then(|at| file.get(at..)) else {
        return &[];
    };
    &rest[..rest.len().min(want.saturating_add(slack))]
}

/// `Scanner::scan` without the reactor. Mirrors `Scanner::{admit, plan, start_extent, start_tail}`.
fn scan_in_memory(file: &[u8], shape: Shape, pool: &Pool) -> Scanned {
    let mut out = Scanned::default();
    if shape.claimed == 0 {
        return out;
    }
    let cfg = ScanConfig { in_flight: 1, tail_len: shape.tail_len };
    let want = shape.prefix.min(usize::try_from(shape.claimed).unwrap_or(usize::MAX));
    let prefix = read(file, 0, want, shape.slack);
    let sniffed = sniff(prefix);
    let mut held = Held {
        format: shape.force.unwrap_or(sniffed.format),
        body: sniffed.body,
        file_len: shape.claimed,
        prefix: Some(prefix),
        extent: None,
        tail: None,
        extents: 0,
        tail_done: false,
    };
    loop {
        let window = match held.next_read(&cfg) {
            Next::Extent { at, len } => {
                let room = usize::try_from(held.file_len.saturating_sub(at)).unwrap_or(usize::MAX);
                let want = len.min(room);
                if want == 0 {
                    break;
                }
                held.extent = Some((read(file, at, want, shape.slack), at));
                held.extents += 1;
                want
            }
            Next::Tail => {
                held.tail_done = true;
                let want = (cfg.tail_len as u64).min(held.file_len);
                if want == 0 {
                    break;
                }
                let at = held.file_len - want;
                held.tail = Some((read(file, at, want as usize, shape.slack), at));
                want as usize
            }
            Next::Done => break,
        };
        out.reads += 1;
        assert!(out.reads <= MAX_READS, "read {} of one file, {shape:?}", out.reads);
        assert!(
            window <= MAX_READ_BYTES.max(cfg.tail_len),
            "a read of {window} bytes was planned, {shape:?}"
        );
    }
    out.read_bytes = held.prefix.map_or(0, <[u8]>::len)
        + held.extent.map_or(0, |(e, _)| e.len())
        + held.tail.map_or(0, |(t, _)| t.len());

    let arena = Arena::default();
    let mut sink = ArenaSink::new(&arena, pool, None);
    out.format = held.format;
    out.props = held.parse(&arena, &mut sink);
    let tags = sink.finish();
    out.text = tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    out.pictures = tags.pictures().iter().map(|p| (p.mime.to_string(), p.data.len())).collect();
    out
}

fn check(scanned: &Scanned, shape: Shape) {
    let entries = scanned.text.len() + scanned.pictures.len();
    let bytes: usize = scanned.text.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
        + scanned.pictures.iter().map(|(mime, len)| mime.len() + len).sum::<usize>();
    assert!(
        entries <= scanned.read_bytes,
        "{entries} tags out of {} bytes, {shape:?}",
        scanned.read_bytes
    );
    assert!(scanned.text.len() <= MAX_TEXT_TAGS && scanned.pictures.len() <= MAX_PICTURES);
    // A value is window bytes at most doubled by Latin-1 to UTF-8. 64 bytes covers any key.
    assert!(
        bytes <= 2 * scanned.read_bytes + 64 * entries,
        "{bytes} tag bytes in {entries} tags out of {} bytes, {shape:?}",
        scanned.read_bytes
    );
}

fn scan_every_way(bytes: &[u8], chunking: Chunking, forced: &[Format], pool: &Pool) {
    let mut p = Prng::new(match chunking {
        Chunking::Whole => 0,
        Chunking::Fixed(n) => n as u64,
        Chunking::Random { max, seed } => seed ^ max as u64,
    });
    let len = bytes.len() as u64;
    let whole =
        Shape { prefix: DEFAULT_PREFIX, tail_len: 64 * 1024, claimed: len, slack: 0, force: None };
    let prefix = match p.below(4) {
        0 => [12, 16, 27, 28, 32, 64][p.below(6) as usize],
        1 => 1 + p.below(256) as usize,
        2 => 256 + p.below(2048) as usize,
        _ => [128, 512, 1024, 4096][p.below(4) as usize],
    };
    let tail_len = [0, 31, 32, 128, 160, 300, 1024, 64 * 1024][p.below(8) as usize];
    let slack = [0, 0, 0, 1, 64, 4096][p.below(6) as usize];
    let small = Shape { prefix, tail_len, slack, ..whole };
    let claimed = [
        len.saturating_sub(1),
        len / 2,
        len + 1,
        len + 129,
        len + 70_000,
        len | 1 << 32,
        i64::MAX as u64,
        u64::MAX,
        1,
        11,
    ][p.below(10) as usize];
    let lying = Shape { claimed, ..small };

    for shape in [whole, small, lying] {
        check(&scan_in_memory(bytes, shape, pool), shape);
    }
    for (i, &format) in forced.iter().enumerate() {
        let shape = Shape { force: Some(format), ..if i % 2 == 0 { small } else { lying } };
        check(&scan_in_memory(bytes, shape, pool), shape);
    }
}

fn inputs_of<'a>(fixtures: &[&'a Fixture]) -> Vec<Input<'a>> {
    fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &f.fields }).collect()
}

fn gate_formats(name: &str, formats: &[Format], iters: u64, sweep: usize, ogg_crc: bool) {
    let all = inputs::fixtures();
    let fixtures = inputs::of(&all, formats);
    assert!(!fixtures.is_empty(), "no fixture for {formats:?}");
    let tokens = inputs::tokens();
    let pool = Pool::bounded(4096, 8);
    let gate = Corpus {
        name,
        inputs: &inputs_of(&fixtures),
        tokens: &tokens,
        iters: iterations(iters),
        sweep,
        tolerate: TOLERATE,
    };
    let bounded = heap_bounded(HEAP_BASE, HEAP_PER_BYTE, |bytes, chunking| {
        scan_every_way(bytes, chunking, formats, &pool);
        if ogg_crc {
            let mut fixed = bytes.to_vec();
            inputs::fix_ogg_crcs(&mut fixed);
            if fixed != bytes {
                scan_every_way(&fixed, chunking, formats, &pool);
            }
        }
    });
    corpus_gate(&gate, &bounded);
}

const HEAP_BASE: usize = 128 * 1024 + CLASS_REGION_COST;

/// A tag takes at least five input bytes and about 150 heap bytes in the arena and gate row.
const HEAP_PER_BYTE: usize = 32;

/// Crates with their own corruption gates. Panics in them are counted, not failed.
const TOLERATE: &[&str] = &["mp3/src", "mkv/src", "mp4/src", "ogg/src"];

#[test]
fn wav_survives_corruption() {
    gate_formats("tags_wav", &[Format::Wav], 1000, 64, false);
}

#[test]
fn aiff_survives_corruption() {
    gate_formats("tags_aiff", &[Format::Aiff], 1000, 64, false);
}

#[test]
fn flac_survives_corruption() {
    gate_formats("tags_flac", &[Format::Flac], 1000, 64, false);
}

#[test]
fn mp3_survives_corruption() {
    gate_formats("tags_mp3", &[Format::Mp3], 1000, 64, false);
}

#[test]
fn mp4_survives_corruption() {
    gate_formats("tags_mp4", &[Format::Mp4], 1000, 64, false);
}

#[test]
fn ogg_survives_corruption() {
    gate_formats("tags_ogg", &OGG_FORMATS, 500, 32, true);
}

#[test]
fn mkv_survives_corruption() {
    gate_formats("tags_mkv", &[Format::Mkv], 1000, 64, false);
}

#[test]
fn ape_family_survives_corruption() {
    gate_formats(
        "tags_ape_family",
        &[Format::Ape, Format::WavPack, Format::Musepack],
        1000,
        64,
        false,
    );
}

#[test]
fn every_format_survives_every_input() {
    gate_formats("tags_cross_format", &ALL_FORMATS, 250, 8, true);
}

#[test]
fn pure_noise_yields_nothing_it_does_not_hold() {
    let tokens = inputs::tokens();
    let pool = Pool::bounded(4096, 8);
    let gate = Corpus {
        name: "tags_noise",
        inputs: &[],
        tokens: &tokens,
        iters: iterations(1000),
        sweep: 0,
        tolerate: TOLERATE,
    };
    let bounded = heap_bounded(HEAP_BASE, HEAP_PER_BYTE, |bytes, chunking| {
        scan_every_way(bytes, chunking, &ALL_FORMATS, &pool)
    });
    corpus_gate(&gate, &bounded);
}

#[test]
fn fixtures_scan_the_same_whole_and_windowed() {
    let pool = Pool::bounded(4096, 8);
    for f in inputs::fixtures() {
        let len = f.bytes.len() as u64;
        let whole = Shape {
            prefix: DEFAULT_PREFIX,
            tail_len: 64 * 1024,
            claimed: len,
            slack: 0,
            force: None,
        };
        let reference = scan_in_memory(&f.bytes, whole, &pool);
        assert_eq!(reference.format, f.format, "{}", f.name);
        assert_eq!(reference.reads, 0, "{} fits the default prefix", f.name);
        assert!(reference.props.sample_rate.is_some(), "{} has no sample rate", f.name);
        if !["mp3_bare", "ogg_opus_dropped"].contains(&f.name) {
            assert!(!reference.text.is_empty(), "{} has no tags", f.name);
        }
        let step = if std::env::var_os("PF_FUZZ_ITERS").is_some() { 1 } else { 7 };
        let prefixes = (160..700).step_by(step).chain((700..f.bytes.len() + 2).step_by(61));
        for prefix in prefixes {
            for slack in [0, 3] {
                let shape = Shape { prefix, slack, ..whole };
                let windowed = scan_in_memory(&f.bytes, shape, &pool);
                let same = (&windowed.text, &windowed.pictures, windowed.props)
                    == (&reference.text, &reference.pictures, reference.props);
                assert!(same, "{} through {shape:?}\n{windowed:?}\n{reference:?}", f.name);
            }
        }
    }
}

fn whole_and_windowed(bytes: &[u8], prefix: usize) -> (Scanned, Scanned) {
    let pool = Pool::bounded(4096, 8);
    let whole = Shape {
        prefix: DEFAULT_PREFIX,
        tail_len: 64 * 1024,
        claimed: bytes.len() as u64,
        slack: 0,
        force: None,
    };
    (scan_in_memory(bytes, whole, &pool), scan_in_memory(bytes, Shape { prefix, ..whole }, &pool))
}

#[test]
fn a_matroska_cover_cut_by_the_prefix_is_read_from_the_extent() {
    let art = [0x89u8; 700];
    let file = fixture::mkv(48_000.0, 2, 3_000.0, &[("TITLE", "Cut")], Some(("image/png", &art)));
    let id = [0x19, 0x41, 0xA4, 0x69];
    let attachments = file.windows(4).rposition(|w| w == id).expect("an Attachments master");
    let (whole, windowed) = whole_and_windowed(&file, attachments + 64);
    assert_eq!(whole.pictures, [("image/png".to_string(), 700)]);
    assert_eq!(windowed.reads, 1);
    assert_eq!(windowed.pictures, whole.pictures);
    assert_eq!(windowed.text, whole.text);
}

#[test]
fn a_wav_data_chunk_past_the_prefix_still_states_its_duration() {
    let file = fixture::riff(&[
        fixture::fmt_chunk(8_000, 1, 8),
        fixture::riff_chunk(b"JUNK", &[0; 300]),
        fixture::riff_chunk(b"data", &[0; 8_000]),
    ]);
    let (whole, windowed) = whole_and_windowed(&file, 128);
    assert_eq!(whole.props.duration_ns, Some(1_000_000_000));
    assert_eq!(windowed.reads, 1);
    assert_eq!(windowed.props, whole.props);
}

#[test]
fn a_file_of_nothing_but_tiny_tags_stops_at_the_tag_ceiling() {
    let comments = vec!["a=b"; 200_000];
    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&fixture::flac_block(false, 0, &fixture::streaminfo(44_100, 2, 16, 1)));
    file.extend_from_slice(&fixture::flac_block(false, 4, &fixture::vorbis_comment(&comments)));
    let picture = fixture::picture_block("", &[]);
    for _ in 0..2_000 {
        file.extend_from_slice(&fixture::flac_block(false, 6, &picture));
    }
    let pool = Pool::bounded(4096, 8);
    let arena = Arena::default();
    let held = Held {
        format: Format::Flac,
        body: 0,
        file_len: file.len() as u64,
        prefix: Some(&file),
        extent: None,
        tail: None,
        extents: 0,
        tail_done: false,
    };
    let (kept, grew) = heap_during(|| {
        let mut sink = ArenaSink::new(&arena, &pool, None);
        held.parse(&arena, &mut sink);
        let tags = sink.finish();
        (tags.iter().count(), tags.pictures().len())
    });
    assert_eq!(kept, (MAX_TEXT_TAGS, MAX_PICTURES));
    assert!(grew <= 6 << 20, "{} bytes of tiny tags grew the heap by {grew}", file.len());
}
