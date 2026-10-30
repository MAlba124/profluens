//! `growingfilesrc` playing a file another writer is still appending to.
// Fixture setup does blocking IO, which the lint only forbids in element code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_reactor::OsReactor;
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad};
use profluens_core::io::{Completion, IoError, IoResult, Reactor, Submission};
use profluens_core::pipeline::Pipeline;
use profluens_core::time::Timestamp;
use profluens_elements::io::{FileError, FrontierHandle, GrowingFileSrc};

fn tmp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pf_growing_{tag}_{}.bin", std::process::id()))
}

/// Position-dependent bytes, so a wrong offset shows as wrong bytes.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u64).wrapping_mul(0x9E37_79B9) as u8).collect()
}

fn create_empty(path: &Path) -> File {
    File::create(path).expect("create the growing file")
}

/// Append and sync before the caller advances the frontier.
fn append(path: &Path, bytes: &[u8]) {
    let mut f = OpenOptions::new().append(true).open(path).expect("open for append");
    f.write_all(bytes).expect("append");
    f.sync_data().expect("sync the append before publishing it");
}

fn settle(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..2000 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

static RECORD_DESC: ElementDesc = ElementDesc {
    name: "growingrecordsink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// Records every byte, append-only even across a seek. Post-seek bytes can land ahead
/// of the `FlushStart` that announces them, so seek tests check by position.
struct RecordSink {
    shared: Arc<Mutex<Vec<u8>>>,
}

impl RecordSink {
    const IN: SinkPad = SinkPad(0);

    fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let shared = Arc::new(Mutex::new(Vec::new()));
        (Self { shared: Arc::clone(&shared) }, shared)
    }
}

impl Element for RecordSink {
    fn desc(&self) -> &'static ElementDesc {
        &RECORD_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.shared.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
}

struct Rig {
    pipeline: Pipeline,
    src: ElementId,
    recorded: Arc<Mutex<Vec<u8>>>,
    frontier: FrontierHandle,
}

fn rig(path: &Path) -> Rig {
    let (src, frontier) = GrowingFileSrc::new(path);
    let (sink, recorded) = RecordSink::new();
    let mut pipeline = pf_reactor::pipeline();
    let s = pipeline.add(src);
    let k = pipeline.add(sink);
    pipeline.link(s.src(GrowingFileSrc::OUT), k.sink(RecordSink::IN)).expect("link");
    Rig { pipeline, src: s, recorded, frontier }
}

fn drain_bus(p: &Pipeline) -> Vec<BusMessage> {
    let mut out = Vec::new();
    while let Some(m) = p.bus().try_recv() {
        out.push(m);
    }
    out
}

fn file_error(e: &Error) -> Option<&FileError> {
    std::error::Error::source(e)?.downcast_ref::<FileError>()
}

fn has_message(
    msgs: &[BusMessage],
    who: ElementId,
    error: bool,
    want: impl Fn(&FileError) -> bool,
) -> bool {
    msgs.iter().any(|m| match m {
        BusMessage::Error { element, error: e } if error => {
            *element == who && file_error(e).is_some_and(&want)
        }
        BusMessage::Warning { element, error: e } if !error => {
            *element == who && file_error(e).is_some_and(&want)
        }
        _ => false,
    })
}

/// `BusMessage` is not `Debug`.
fn summarize(msgs: &[BusMessage]) -> String {
    let mut s = String::new();
    for m in msgs {
        match m {
            BusMessage::Error { element, error } => {
                s.push_str(&format!("Error({element:?}, {error:?}); "));
            }
            BusMessage::Warning { element, error } => {
                s.push_str(&format!("Warning({element:?}, {error:?}); "));
            }
            BusMessage::Eos => s.push_str("Eos; "),
            _ => s.push_str("<other>; "),
        }
    }
    s
}

// Chunks unrelated to the pool slot, so nearly every read straddles the frontier.
#[test]
fn growing_file_streams_byte_identically_while_the_writer_appends() {
    const CHUNKS: usize = 12;
    const CHUNK: usize = 40_000;
    let path = tmp("append");
    create_empty(&path);
    let data = pattern(CHUNKS * CHUNK);

    let r = rig(&path);
    let (frontier, writer_path, writer_data) = (r.frontier.clone(), path.clone(), data.clone());
    let writer = std::thread::spawn(move || {
        let mut written = 0usize;
        for i in 0..CHUNKS {
            append(&writer_path, &writer_data[i * CHUNK..(i + 1) * CHUNK]);
            written += CHUNK;
            frontier.advance(written as u64);
            std::thread::sleep(Duration::from_millis(5));
        }
        frontier.finish(written as u64);
    });

    let mut p = r.pipeline;
    p.run().expect("run");
    writer.join().expect("writer joined");

    assert_eq!(
        *r.recorded.lock().unwrap(),
        data,
        "the stream must be byte-identical to the final file, in order"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn finish_ends_the_stream_with_the_complete_file() {
    let path = tmp("finish");
    create_empty(&path);
    let data = pattern(300_000);
    append(&path, &data);

    let r = rig(&path);
    r.frontier.advance(data.len() as u64);
    r.frontier.finish(data.len() as u64);
    assert!(r.frontier.is_finished(), "the handle reports the finished state");
    assert_eq!(r.frontier.downloaded(), data.len() as u64, "finish implies a final advance");

    let mut p = r.pipeline;
    p.run().expect("run");

    assert_eq!(*r.recorded.lock().unwrap(), data, "every byte, exactly once");
    assert!(
        drain_bus(&p).iter().any(|m| matches!(m, BusMessage::Eos)),
        "a finished growing file ends the pipeline through the ordinary EOS path"
    );
    let _ = std::fs::remove_file(&path);
}

// While stalled the group must park, not spin.
#[test]
fn a_stalled_writer_leaves_the_group_parked_not_spinning() {
    const HALF: usize = 200_000;
    let path = tmp("stall");
    create_empty(&path);
    let data = pattern(2 * HALF);
    append(&path, &data[..HALF]);

    let r = rig(&path);
    r.frontier.advance(HALF as u64);
    let mut p = r.pipeline;
    let tap = p.tap_handle();
    let recorded = Arc::clone(&r.recorded);
    let run = std::thread::spawn(move || p.run());

    assert!(
        settle(|| recorded.lock().unwrap().len() == HALF),
        "the first half plays out while the writer is still holding the rest"
    );

    let before = tap.scheduler();
    std::thread::sleep(Duration::from_millis(500));
    let after = tap.scheduler();
    let parked_ms = (after.parked_ns - before.parked_ns) / 1_000_000;
    let parks = after.parks - before.parks;
    let ticks = after.tick_expiries - before.tick_expiries;

    assert_eq!(
        recorded.lock().unwrap().len(),
        HALF,
        "nothing beyond the frontier may be emitted while the writer is stalled"
    );
    // Two groups park through 500 ms. A spin leaves both deltas at zero.
    assert!(
        parked_ms >= 300,
        "the group spent only {parked_ms} ms parked across a 500 ms stall, spinning?"
    );
    assert!(parks >= 20, "only {parks} parks in a 500 ms stall (expect ~50/group at 10 ms)");
    assert!(
        ticks >= 20,
        "only {ticks} of {parks} parks ended on the backstop tick, the frontier wait has no \
         wake source, so the tick is what must be advancing it"
    );

    append(&path, &data[HALF..]);
    r.frontier.advance(data.len() as u64);
    r.frontier.finish(data.len() as u64);

    let result = run.join().expect("the run thread joined");
    result.expect("run");
    assert_eq!(*r.recorded.lock().unwrap(), data, "playback resumed and completed the file");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn seek_back_into_downloaded_territory_replays_from_the_target() {
    const TOTAL: usize = 400_000;
    const TARGET: usize = 137_000; // unaligned to the pool slot on purpose
    let path = tmp("seekback");
    create_empty(&path);
    let data = pattern(TOTAL);
    append(&path, &data);

    let r = rig(&path);
    r.frontier.advance(TOTAL as u64);
    let mut p = r.pipeline;
    let seek = p.control();
    let recorded = Arc::clone(&r.recorded);
    let run = std::thread::spawn(move || p.run());

    assert!(
        settle(|| recorded.lock().unwrap().len() == TOTAL),
        "the whole downloaded region plays"
    );
    let _ = seek.seek_bytes(
        Timestamp::ZERO,
        &[(profluens_core::command::ALL, TARGET as u64, Timestamp::ZERO)],
    );
    assert!(
        settle(|| recorded.lock().unwrap().len() == 2 * TOTAL - TARGET),
        "the source must re-read from the seek target up to the frontier"
    );
    r.frontier.finish(TOTAL as u64);

    run.join().expect("the run thread joined").expect("a backward seek must not fail the run");
    let mut expected = data.clone();
    expected.extend_from_slice(&data[TARGET..]);
    assert_eq!(
        *r.recorded.lock().unwrap(),
        expected,
        "the post-seek stream is the file from the target on, the right bytes, from the right \
         offset, exactly once"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn seek_beyond_the_frontier_waits_for_the_writer_instead_of_failing() {
    const QUARTER: usize = 100_000;
    const TARGET: usize = 3 * QUARTER;
    let path = tmp("seekahead");
    create_empty(&path);
    let data = pattern(4 * QUARTER);
    append(&path, &data[..QUARTER]);

    let r = rig(&path);
    r.frontier.advance(QUARTER as u64);
    let mut p = r.pipeline;
    let seek = p.control();
    let tap = p.tap_handle();
    let recorded = Arc::clone(&r.recorded);
    let run = std::thread::spawn(move || p.run());

    assert!(settle(|| recorded.lock().unwrap().len() == QUARTER), "the downloaded quarter plays");

    let _ = seek.seek_bytes(
        Timestamp::ZERO,
        &[(profluens_core::command::ALL, TARGET as u64, Timestamp::ZERO)],
    );
    let before = tap.scheduler();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !run.is_finished(),
        "a beyond-frontier seek must not end the run, the bytes are simply not there yet"
    );
    assert_eq!(
        recorded.lock().unwrap().len(),
        QUARTER,
        "not one byte may be emitted while the target is still beyond the watermark"
    );
    assert!(
        tap.scheduler().parked_ns > before.parked_ns,
        "the beyond-frontier wait is the ordinary idle park, not a spin"
    );

    append(&path, &data[QUARTER..]);
    r.frontier.advance(data.len() as u64);
    r.frontier.finish(data.len() as u64);

    run.join().expect("the run thread joined").expect("run");
    let mut expected = data[..QUARTER].to_vec();
    expected.extend_from_slice(&data[TARGET..]);
    assert_eq!(
        *r.recorded.lock().unwrap(),
        expected,
        "playback resumed at the seek target once the watermark covered it"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn abort_posts_an_error_and_winds_the_pipeline_down() {
    const HAVE: usize = 150_000;
    let path = tmp("abort");
    create_empty(&path);
    let data = pattern(HAVE);
    append(&path, &data);

    let r = rig(&path);
    r.frontier.advance(HAVE as u64);
    let p = r.pipeline;
    let recorded = Arc::clone(&r.recorded);
    let run = std::thread::spawn(move || {
        let mut p = p;
        let result = p.run();
        (result, p)
    });

    assert!(settle(|| recorded.lock().unwrap().len() == HAVE), "the downloaded prefix plays");
    r.frontier.abort();

    assert!(settle(|| run.is_finished()), "abort must end the run, not leave it at the frontier");
    let (result, p) = run.join().expect("the run thread joined");
    result.expect("abort winds down through EOS, so the run itself succeeds");

    let msgs = drain_bus(&p);
    assert!(
        has_message(&msgs, r.src, true, |e| matches!(e, FileError::WriterAborted { .. })),
        "abort must put one clear Error on the bus, attributed to the source: {}",
        summarize(&msgs)
    );
    assert_eq!(*r.recorded.lock().unwrap(), data, "the bytes that did arrive were not lost");
    let _ = std::fs::remove_file(&path);
}

// The writer advances past what it wrote. No garbage, no hot loop, and the stream
// resumes without another advance once the bytes land.
#[test]
fn an_over_promising_watermark_degrades_to_parked_retries_not_garbage() {
    const LEN: usize = 4096;
    let path = tmp("overpromise");
    create_empty(&path); // zero-length at start: legal, and the usual case
    let data = pattern(LEN);

    let r = rig(&path);
    let mut p = r.pipeline;
    let tap = p.tap_handle();
    let recorded = Arc::clone(&r.recorded);
    let run = std::thread::spawn(move || p.run());

    r.frontier.advance(LEN as u64);
    let before = tap.scheduler();
    std::thread::sleep(Duration::from_millis(250));
    let after = tap.scheduler();

    assert!(
        recorded.lock().unwrap().is_empty(),
        "a watermark ahead of the real bytes must never produce bytes"
    );
    let parked_ms = (after.parked_ns - before.parked_ns) / 1_000_000;
    assert!(
        parked_ms >= 100,
        "only {parked_ms} ms parked over a 250 ms over-promised stall, the empty reads are \
         looping hot instead of being paced by the park tick"
    );

    append(&path, &data);
    assert!(
        settle(|| recorded.lock().unwrap().len() == LEN),
        "the retry must pick the bytes up without a further advance()"
    );
    r.frontier.finish(LEN as u64);

    run.join().expect("the run thread joined").expect("run");
    assert_eq!(*r.recorded.lock().unwrap(), data, "and the bytes are the right ones");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_file_shorter_than_the_declared_total_warns_and_ends_cleanly() {
    const REAL: usize = 4096;
    const CLAIMED: u64 = 8192;
    let path = tmp("truncated");
    create_empty(&path);
    let data = pattern(REAL);
    append(&path, &data);

    let r = rig(&path);
    r.frontier.finish(CLAIMED); // twice what is there
    let mut p = r.pipeline;
    p.run().expect("a short file is a warning, not a failed run");

    assert_eq!(*r.recorded.lock().unwrap(), data, "exactly the bytes the file really had");
    let msgs = drain_bus(&p);
    assert!(
        has_message(&msgs, r.src, false, |e| matches!(
            e,
            FileError::Truncated { declared, readable, .. }
                if *declared == CLAIMED && *readable == data.len() as u64
        )),
        "the truncation must be reported on the bus: {}",
        summarize(&msgs)
    );
    let _ = std::fs::remove_file(&path);
}

/// Passes `pass` completions, then fails every read. Unlinking would not do it.
struct FailingReactor {
    inner: OsReactor,
    remaining: Arc<AtomicU32>,
}

impl Reactor for FailingReactor {
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
    fn set_waker(&mut self, waker: profluens_core::io::Waker) {
        self.inner.set_waker(waker)
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        self.inner.run_once(out);
        for (_, c) in out.iter_mut().filter(|(_, c)| matches!(c.result, IoResult::Ok(_))) {
            if self.remaining.load(Ordering::Relaxed) == 0 {
                if let Some(buf) = c.buf.as_mut() {
                    buf.memory.set_len(0);
                }
                c.result = IoResult::Err(IoError::NotFound);
            } else {
                self.remaining.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

#[test]
fn a_failing_read_reports_an_error_and_exits_without_hanging() {
    let path = tmp("readerror");
    create_empty(&path);
    let data = pattern(600_000);
    append(&path, &data);

    let r = rig(&path);
    r.frontier.advance(data.len() as u64);
    let mut p = r.pipeline;
    // Shared so each per-group reactor sees the same budget.
    let budget = Arc::new(AtomicU32::new(1));
    p.set_reactor_factory(Arc::new(move || {
        Ok(Box::new(FailingReactor { inner: OsReactor::new()?, remaining: Arc::clone(&budget) })
            as Box<dyn Reactor>)
    }));

    let run = std::thread::spawn(move || {
        let result = p.run();
        (result, p)
    });
    assert!(settle(|| run.is_finished()), "a failing read must end the run promptly");
    let (result, p) = run.join().expect("the run thread joined, no panic");

    let err = result.expect_err("a failed read must fail the run");
    assert!(
        matches!(file_error(&err), Some(FileError::Read { .. })),
        "the error must name the failing read: {err:?}"
    );
    let msgs = drain_bus(&p);
    assert!(
        has_message(&msgs, r.src, true, |e| matches!(e, FileError::Read { .. })),
        "the read failure must be attributed to the source on the bus: {}",
        summarize(&msgs)
    );
    assert!(
        r.recorded.lock().unwrap().len() < data.len(),
        "the stream was cut short, not silently completed"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn dropping_every_handle_ends_the_stream_instead_of_hanging() {
    let path = tmp("abandoned");
    create_empty(&path);
    let data = pattern(50_000);
    append(&path, &data);

    let r = rig(&path);
    r.frontier.advance(data.len() as u64);
    let Rig { pipeline, src, recorded, frontier } = r;
    let mut p = pipeline;
    let watcher = Arc::clone(&recorded);
    let run = std::thread::spawn(move || {
        let result = p.run();
        (result, p)
    });

    assert!(settle(|| watcher.lock().unwrap().len() == data.len()), "the published bytes play");
    drop(frontier); // the downloader is gone

    assert!(settle(|| run.is_finished()), "an abandoned download must not hang the pipeline");
    let (result, p) = run.join().expect("the run thread joined");
    result.expect("an abandoned download winds down through EOS");
    assert!(
        has_message(&drain_bus(&p), src, false, |e| matches!(e, FileError::WriterAbandoned { .. })),
        "the abandonment must be reported on the bus"
    );
    assert_eq!(*recorded.lock().unwrap(), data, "the bytes that did arrive were delivered");
    let _ = std::fs::remove_file(&path);
}
