//! End-to-end corruption gate, corrupted files on disk scanned through the reactor.
//! The per-format parsers are gated from memory in `src/gate.rs` with the same inputs.

// test fixtures and case files
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod inputs;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pf_reactor::OsReactor;
use pf_sim::corrupt::{
    CLASS_REGION_COST, Corpus, Input, Tracking, corpus_gate, corpus_gate_quiet, heap_bounded,
    heap_during, iterations,
};
use pf_tags::{
    DEFAULT_PREFIX, Format, Props, ScanConfig, ScanOutcome, Scanner, fixture, scan_parallel,
    scan_parallel_with,
};
use profluens_core::id::ElementId;
use profluens_core::io::{Completion, IoError, IoResult, Reactor, ReactorFactory, Submission};
use profluens_core::memory::{Pool, REGION_SLOTS};

#[global_allocator]
static HEAP: Tracking = Tracking;

/// Crates with their own corruption gates (see `src/gate.rs`).
const TOLERATE: &[&str] = &["mp3/src", "mkv/src", "mp4/src", "ogg/src"];

const MIB: usize = 1024 * 1024;

const MAX_METADATA: usize = 32 * MIB;

struct Dir(PathBuf);

impl Dir {
    fn new(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pf_tags_corruption_{test}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create case directory");
        Dir(dir)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).expect("write case file");
        path
    }

    fn write_sparse(&self, name: &str, head: &[u8], len: u64) -> PathBuf {
        let path = self.write(name, head);
        let file = File::options().write(true).open(&path).expect("open case file");
        file.set_len(len).expect("extend case file");
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Row {
    format: Format,
    len: u64,
    props: Props,
    text: Vec<(String, String)>,
    pictures: Vec<(String, usize)>,
}

fn row(out: &ScanOutcome<'_>) -> Row {
    let r = match &out.result {
        Ok(r) => r,
        Err(e) => panic!("{} failed to scan with {e:?}", out.path.display()),
    };
    let row = Row {
        format: r.format,
        len: r.file.len,
        props: r.props,
        text: r.tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        pictures: r.tags.pictures().iter().map(|p| (p.mime.to_string(), p.data.len())).collect(),
    };
    let entries = row.text.len() + row.pictures.len();
    let bytes: usize = row.text.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
        + row.pictures.iter().map(|(mime, len)| mime.len() + len).sum::<usize>();
    let read = 3 * row.len as usize;
    assert!(entries <= read, "{entries} tags out of a {} byte file", row.len);
    assert!(bytes <= 2 * read + 64 * entries, "{bytes} tag bytes out of a {} byte file", row.len);
    row
}

fn scan_all(scanner: &mut Scanner, paths: &[PathBuf]) -> HashMap<PathBuf, Row> {
    let mut rows = HashMap::new();
    scanner.scan(paths, |out| {
        let previous = rows.insert(out.path.to_path_buf(), row(&out));
        assert!(previous.is_none(), "{} was reported twice", out.path.display());
    });
    assert_reported(rows.keys(), paths);
    rows
}

fn assert_reported<'a>(reported: impl Iterator<Item = &'a PathBuf>, paths: &[PathBuf]) {
    let reported: std::collections::HashSet<&PathBuf> = reported.collect();
    for path in paths {
        assert!(reported.contains(path), "{} was never reported", path.display());
    }
    assert_eq!(reported.len(), paths.len(), "a path that was not asked for was reported");
}

fn scanner(slot: usize, slots: u32, tail_len: usize) -> Scanner {
    Scanner::with_pool(ScanConfig { in_flight: 16, tail_len }, Pool::bounded(slot, slots))
}

#[derive(Clone, Copy)]
enum Faults {
    Short,
    Failing,
}

struct Flaky {
    inner: OsReactor,
    faults: Faults,
    ops: u64,
}

impl Reactor for Flaky {
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]) {
        self.inner.submit(subs, names)
    }
    fn due_in(&self) -> Option<std::time::Duration> {
        self.inner.due_in()
    }
    fn is_idle(&self) -> bool {
        self.inner.is_idle()
    }
    fn park(&mut self, timeout: std::time::Duration) -> bool {
        self.inner.park(timeout)
    }
    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        self.inner.run_once(out);
        for (_, c) in out.iter_mut() {
            let (IoResult::Ok(n), Some(buf)) = (c.result, c.buf.as_mut()) else {
                continue;
            };
            self.ops += 1;
            let fate = match self.faults {
                Faults::Short => 0,
                Faults::Failing => self.ops.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60,
            };
            c.result = match fate {
                0..=3 if n > 1 => IoResult::Ok(n / 2),
                4 | 5 => IoResult::Ok(0),
                6 => IoResult::Err(IoError::Unsupported),
                7 => IoResult::Cancelled,
                _ => IoResult::Ok(n),
            };
            if let IoResult::Ok(kept) = c.result {
                buf.memory.set_len(kept);
            }
        }
    }
}

fn flaky(faults: Faults) -> ReactorFactory {
    Arc::new(move || {
        Ok(Box::new(Flaky { inner: OsReactor::new()?, faults, ops: 0 }) as Box<dyn Reactor>)
    })
}

#[test]
fn scanning_one_corrupt_file_survives() {
    let dir = Dir::new("single");
    let path = dir.0.join("case.bin");
    let fixtures = inputs::fixtures();
    let fields: Vec<Vec<usize>> =
        fixtures.iter().map(|f| f.fields.iter().copied().step_by(9).collect()).collect();
    let gate_inputs: Vec<Input<'_>> = fixtures
        .iter()
        .zip(&fields)
        .map(|(f, fields)| Input { name: f.name, bytes: &f.bytes, fields })
        .collect();
    let tokens = inputs::tokens();
    let gate = Corpus {
        name: "tags_scan_single",
        inputs: &gate_inputs,
        tokens: &tokens,
        iters: iterations(100),
        sweep: 6,
        tolerate: TOLERATE,
    };
    let paths = [path.clone()];
    let bounded = heap_bounded(ONE_FILE, 35, |bytes, _| {
        std::fs::write(&path, bytes).expect("write case file");
        for (slot, tail_len) in [(128 * 1024, 64 * 1024), (512, 300), (96, 32)] {
            let rows = scan_all(&mut scanner(slot, 8, tail_len), &paths);
            assert_eq!(rows[&path].len, bytes.len() as u64);
        }
    });
    corpus_gate(&gate, &bounded);
}

fn scan_batch(paths: &[PathBuf], good: &HashMap<PathBuf, Row>) {
    let check = |rows: &HashMap<PathBuf, Row>, how: &str| {
        for (path, reference) in good {
            assert_eq!(&rows[path], reference, "{} scanned {how}", path.display());
        }
    };
    check(&scan_all(&mut scanner(512, 256, 1024), paths), "by one scanner");
    check(&scan_all(&mut scanner(512, 3, 1024), paths), "from a starved pool");
    check(&scan_all(&mut Scanner::new(ScanConfig::default()), paths), "with default windows");

    let cfg = ScanConfig::default();
    let rows = Mutex::new(HashMap::new());
    let keep = |out: ScanOutcome<'_>| {
        let previous = rows.lock().expect("rows").insert(out.path.to_path_buf(), row(&out));
        assert!(previous.is_none(), "{} was reported twice", out.path.display());
    };
    scan_parallel(paths.to_vec(), 4, cfg, keep);
    let by_threads = std::mem::take(&mut *rows.lock().expect("rows"));
    assert_reported(by_threads.keys(), paths);
    check(&by_threads, "by four threads");

    scan_parallel_with(paths.to_vec(), 3, cfg, Some(flaky(Faults::Short)), keep);
    let short = std::mem::take(&mut *rows.lock().expect("rows"));
    assert_reported(short.keys(), paths);
    check(&short, "by three threads under short reads");

    let seen = Mutex::new(Vec::new());
    scan_parallel_with(paths.to_vec(), 2, cfg, Some(flaky(Faults::Failing)), |out| {
        seen.lock().expect("seen").push(out.path.to_path_buf());
    });
    let seen = seen.into_inner().expect("seen");
    assert_eq!(seen.len(), paths.len(), "a path was reported twice under failing reads");
    assert_reported(seen.iter(), paths);
}

#[test]
fn a_batch_of_corrupt_files_reports_every_path_once() {
    let dir = Dir::new("batch");
    let fixtures = inputs::fixtures();
    let gate_inputs: Vec<Input<'_>> =
        fixtures.iter().map(|f| Input { name: f.name, bytes: &f.bytes, fields: &[] }).collect();
    let tokens = inputs::tokens();

    let good_paths: Vec<PathBuf> =
        fixtures.iter().map(|f| dir.write(&format!("good_{}", f.name), &f.bytes)).collect();
    let mut good = HashMap::new();
    for (path, f) in good_paths.iter().zip(&fixtures) {
        let rows = scan_all(&mut Scanner::new(ScanConfig::default()), std::slice::from_ref(path));
        assert_eq!(rows[path].format, f.format, "{}", f.name);
        good.insert(path.clone(), rows[path].clone());
    }

    let total = iterations(160);
    let mut cases = 0;
    let mut round = 0;
    while cases < total {
        let name = format!("tags_scan_batch_{round}");
        let written: RefCell<Vec<PathBuf>> = RefCell::new(good_paths.clone());
        let writer = Corpus {
            name: &name,
            inputs: &gate_inputs,
            tokens: &tokens,
            iters: (total - cases).min(1024),
            sweep: if round == 0 { 6 } else { 0 },
            tolerate: &[],
        };
        let write = |bytes: &[u8], _| {
            let mut written = written.borrow_mut();
            let path = dir.write(&format!("r{round}_{}.bin", written.len()), bytes);
            written.push(path);
        };
        let summary = corpus_gate_quiet(&writer, &heap_bounded(4 * MIB, 16, write));
        cases += summary.cases.max(1);
        round += 1;

        let paths = written.into_inner();
        let batch = Corpus {
            name: "tags_scan_batch_run",
            inputs: &[],
            tokens: &[],
            iters: 1,
            sweep: 0,
            tolerate: TOLERATE,
        };
        // Three scanners with pools of at most 52 slots of 128 KiB, plus up to 8 MiB of parked
        // extents.
        corpus_gate_quiet(&batch, &heap_bounded(64 * MIB, 0, |_, _| scan_batch(&paths, &good)));
        for path in paths.iter().filter(|p| !good.contains_key(*p)) {
            std::fs::remove_file(path).ok();
        }
    }
    eprintln!("corruption gate tags_scan_batch passed {cases} cases in {round} batches");
}

#[test]
fn files_shorter_than_any_header_report_unknown_or_less() {
    let dir = Dir::new("short");
    let fixtures = inputs::fixtures();
    let mut paths = Vec::new();
    let mut whole = Vec::new();
    for f in &fixtures {
        for len in 0..48.min(f.bytes.len()) {
            paths.push(dir.write(&format!("{}_{len}", f.name), &f.bytes[..len]));
        }
        whole.push(dir.write(f.name, &f.bytes));
    }
    paths.extend(whole.iter().cloned());
    let rows = scan_all(&mut scanner(256, 64, 160), &paths);
    for (path, f) in whole.iter().zip(&fixtures) {
        assert_eq!(rows[path].format, f.format, "{}", f.name);
    }
    for row in rows.values().filter(|row| row.len == 0) {
        assert_eq!(row.format, Format::Unknown);
        assert!(row.text.is_empty() && row.pictures.is_empty());
    }
}

#[test]
fn the_backend_under_test_is_reported() {
    let kind = Scanner::new(ScanConfig::default()).reactor_kind();
    eprintln!("corruption gate tags_scan backend {kind:?}");
}

fn scan_sparse(dir: &Dir, name: &str, head: &[u8], len: u64) -> (Row, usize) {
    let paths = [dir.write_sparse(name, head, len)];
    let mut scanner = Scanner::new(ScanConfig::default());
    let (rows, grew) = heap_during(|| scan_all(&mut scanner, &paths));
    (rows[&paths[0]].clone(), grew)
}

const ONE_FILE: usize =
    REGION_SLOTS as usize * DEFAULT_PREFIX + 64 * 1024 + 64 * 1024 + CLASS_REGION_COST + 64 * 1024;

#[test]
fn a_declared_size_past_the_metadata_ceiling_is_not_read() {
    let dir = Dir::new("ceiling");

    let (row, grew) = scan_sparse(&dir, "id3.mp3", b"ID3\x04\x00\x00\x0f\x7f\x7f\x7f", 64 << 20);
    assert_eq!(row.format, Format::Mp3);
    assert!(grew <= ONE_FILE, "a 33.5 MiB ID3v2 claim cost {grew} bytes");

    let mut wav = fixture::riff(&[fixture::fmt_chunk(44_100, 2, 16)]);
    wav.extend_from_slice(b"LIST\xf0\xff\xff\x7fINFO");
    let (row, grew) = scan_sparse(&dir, "list.wav", &wav, 3 << 30);
    assert_eq!((row.format, row.props.sample_rate), (Format::Wav, Some(44_100)));
    assert!(grew <= ONE_FILE, "a 2 GiB LIST claim cost {grew} bytes");
    let mut aiff = fixture::form(b"AIFF", &[fixture::comm_chunk(48_000, 2, 16, 48_000, None)]);
    aiff.extend_from_slice(b"ID3 \x7f\xff\xff\xf0ID3\x04\x00\x00\x00\x00\x00\x00");
    let (row, grew) = scan_sparse(&dir, "id3.aiff", &aiff, 3 << 30);
    assert_eq!((row.format, row.props.sample_rate), (Format::Aiff, Some(48_000)));
    assert!(grew <= ONE_FILE, "a 2 GiB ID3 chunk claim cost {grew} bytes");

    let mut mp4 = fixture::mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00M4A mp42");
    mp4.extend_from_slice(b"\x00\x00\x00\x01moov\x00\x00\x00\x00\x40\x00\x00\x00");
    let (row, grew) = scan_sparse(&dir, "moov.m4a", &mp4, (1 << 30) + mp4.len() as u64);
    assert_eq!(row.format, Format::Mp4);
    let clamped = MAX_METADATA..=MAX_METADATA + ONE_FILE;
    assert!(clamped.contains(&grew), "a 1 GiB moov claim cost {grew} bytes");
    let mut mkv = fixture::mkv(48_000.0, 2, 3_000.0, &[("TITLE", "Claimed")], None);
    let tags_id = [0x12, 0x54, 0xC3, 0x67];
    let tags_at = mkv.windows(4).rposition(|w| w == tags_id).expect("a Tags master");
    mkv.truncate(tags_at + 4);
    mkv.extend_from_slice(&[0x01, 0, 0, 0, 0x40, 0, 0, 0]);
    let (row, grew) = scan_sparse(&dir, "tags.mkv", &mkv, 2 << 30);
    assert_eq!((row.format, row.props.sample_rate), (Format::Mkv, Some(48_000)));
    assert!(clamped.contains(&grew), "a 1 GiB Tags claim cost {grew} bytes");
}

#[test]
fn a_pool_with_fewer_buffers_than_files_in_flight_loses_no_path() {
    let dir = Dir::new("starved");
    let wav = fixture::wav(8_000, 1, 8, 64, &[("INAM", "Starved")], false);
    let paths: Vec<PathBuf> = (0..40).map(|i| dir.write(&format!("{i}.wav"), &wav)).collect();
    let rows = scan_all(&mut scanner(256, 2, 64), &paths);
    for row in rows.values() {
        assert_eq!(row.text, [("TITLE".to_string(), "Starved".to_string())]);
    }
}

#[test]
fn a_superseded_extent_is_released_before_the_next_is_read() {
    let dir = Dir::new("superseded");
    let mut flac = b"fLaC".to_vec();
    flac.extend_from_slice(&fixture::flac_block(
        false,
        0,
        &fixture::streaminfo(44_100, 2, 16, 44_100),
    ));
    flac.extend_from_slice(&[0x06, 0xFF, 0xFF, 0xFF]);
    let (row, grew) = scan_sparse(&dir, "picture.flac", &flac, 1 << 30);
    assert_eq!((row.format, row.props.sample_rate), (Format::Flac, Some(44_100)));
    let one_extent = 16 * MIB..=16 * MIB + 64 * 1024 + ONE_FILE;
    assert!(one_extent.contains(&grew), "a 16 MiB block cost {grew} bytes");
}

#[test]
fn a_scanner_on_epoll_costs_one_files_heap() {
    let dir = Dir::new("epoll");
    let factory: ReactorFactory = Arc::new(|| {
        let reactor = OsReactor::files(pf_reactor::Want::Epoll, 32)?;
        assert_eq!(reactor.backend(), pf_reactor::Backend::Epoll);
        Ok(Box::new(reactor) as Box<dyn Reactor>)
    });
    let cfg = ScanConfig { in_flight: 16, tail_len: 64 * 1024 };
    for f in inputs::fixtures() {
        let paths = [dir.write(f.name, &f.bytes)];
        let (rows, grew) = heap_during(|| {
            let mut scanner = Scanner::with_reactor(cfg, Pool::bounded(128 * 1024, 8), &factory);
            scan_all(&mut scanner, &paths)
        });
        assert_eq!(rows[&paths[0]].format, f.format, "{}", f.name);
        let limit = ONE_FILE + 35 * f.bytes.len();
        assert!(grew <= limit, "{} on epoll cost {grew} bytes, limit {limit}", f.name);
    }
}
