//! Microbench of the per-row paths and a whole ideal run, plain `Instant` timing.
//! `cargo bench -p pf-sim --bench sweep`

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use std::hint::black_box;
use std::time::Instant;

use pf_sim::action::{self, ActionList, GenParams};
use pf_sim::ideal;
use pf_sim::media::{MediaSpec, Pacing, StreamKind, StreamSpec};
use pf_sim::record::{Recording, Row, RowKind};
use pf_sim::sweep::{self, Expect};
use pf_sim::time::{MS, Rational, SEC};

#[path = "../tests/alloc_count.rs"]
mod alloc_count;

#[global_allocator]
static GLOBAL: alloc_count::Counting = alloc_count::Counting;

const TRIALS: usize = 15;

const SEEKS: [u32; pf_sim::action::ActionKind::KINDS] = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0];

fn params(duration: u64, weights: [u32; pf_sim::action::ActionKind::KINDS]) -> GenParams {
    GenParams {
        count: 10,
        duration,
        sinks: 2,
        max_buffers: 400,
        buffer_anchor_percent: 30,
        streams: 2,
        tracks: 1,
        weights,
    }
}

fn declare(spec: &mut MediaSpec, duration: u64) {
    spec.reset();
    let pacing = Pacing::Jitter { base: MS, bound: 8 * MS };
    let video = StreamKind::Video { fps: Rational::new(25, 1), keyframe_interval: 25 };
    spec.add_stream(StreamSpec { kind: video, duration, size: 4096, pacing }).unwrap();
    let audio = StreamKind::Audio { packet: 20 * MS };
    spec.add_stream(StreamSpec { kind: audio, duration, size: 256, pacing: Pacing::RealTime })
        .unwrap();
}

/// A valid log of `rows` rows. Every rule is a safety property, so its head is valid.
// Bench setup, built before anything is timed.
#[allow(clippy::disallowed_methods)]
fn valid_log(rows: usize) -> Vec<Row> {
    let mut spec = MediaSpec::with_capacity(0, 0);
    let duration = (rows as u64 / 75 + 2) * 2 * SEC;
    declare(&mut spec, duration);
    let mut actions = ActionList::new();
    action::generate(rows as u64, &params(duration / 2, SEEKS), &mut actions);
    let mut rec = Recording::with_capacity(rows);
    ideal::play(&spec, 1, actions.as_slice(), &mut rec).unwrap();
    assert_eq!(rec.len(), rows, "the run was too short to fill the log");
    rec.rows().to_vec()
}

fn median(mut samples: [f64; TRIALS]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[TRIALS / 2]
}

/// Median seconds per call over `TRIALS` trials. Panics if the measured calls allocate.
fn measure(name: &str, iters: usize, mut body: impl FnMut()) -> f64 {
    for _ in 0..iters.div_ceil(4) {
        body();
    }
    let before = alloc_count::allocations();
    let mut samples = [0.0; TRIALS];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..iters {
            body();
        }
        *sample = start.elapsed().as_secs_f64() / iters as f64;
    }
    let allocated = alloc_count::allocations() - before;
    assert_eq!(allocated, 0, "{name} allocated {allocated} times after warm-up");
    median(samples)
}

fn report(name: &str, rows: usize, seconds: f64) {
    let per_row = seconds / rows as f64;
    println!(
        "{name:<8} {rows:>7} rows  {:>8.2} ns/row  {:>8.1} M rows/s  {:>10.1} us/call",
        per_row * 1e9,
        1e-6 / per_row,
        seconds * 1e6,
    );
}

fn bench_rows(rows: usize) {
    let log = valid_log(rows);
    let sinks = ideal::expect();
    let expect = Expect { sinks: &sinks[..2], gaps: &[] };
    assert_eq!(sweep::all(&log, &expect), Ok(()));
    let iters = (2_000_000 / rows).max(8);

    let mut rec = Recording::with_capacity(rows);
    let template = Row { size: 4096, duration: 40 * MS, ..Row::new(RowKind::Buffer, 1, 1) };
    let push = measure("push", iters, || {
        rec.reset();
        for seq in 0..rows as u64 {
            let at = black_box(seq * 40 * MS);
            rec.push(Row { seq, pts: at, dts: at, at, ..template });
        }
        black_box(rec.rows());
    });
    assert_eq!((rec.len(), rec.overflow(), rec.buffers_on(1)), (rows, 0, rows as u32));
    report("push", rows, push);

    let copy = measure("copy", iters, || {
        rec.reset();
        for row in &log {
            rec.push(*row);
        }
        black_box(rec.rows());
    });
    report("copy", rows, copy);

    let swept = measure("sweep", iters, || {
        black_box(sweep::all(black_box(&log), &expect)).ok();
    });
    report("sweep", rows, swept);
}

fn bench_runs() {
    let mut spec = MediaSpec::with_capacity(0, 0);
    let mut actions = ActionList::new();
    let mut rec = Recording::with_capacity(16_384);
    let sinks = ideal::expect();
    let expect = Expect { sinks: &sinks[..2], gaps: &[] };
    let (mut seed, mut rows) = (0u64, 0usize);
    let seconds = measure("run", 2_000, || {
        seed += 1;
        declare(&mut spec, 10 * SEC);
        action::generate(seed, &params(10 * SEC, GenParams::PLAYBACK_WEIGHTS), &mut actions);
        let played = ideal::play(&spec, seed, actions.as_slice(), &mut rec).unwrap();
        assert!(!played.truncated);
        assert!(sweep::check(&rec, &expect).is_ok());
        rows += rec.len();
    });
    let mean_rows = rows / seed as usize;
    println!(
        "run      {mean_rows:>7} rows  {:>8.2} ns/row  {:>8.0} runs/s    {:>10.1} us/run  (ideal, edges only)",
        seconds / mean_rows as f64 * 1e9,
        1.0 / seconds,
        seconds * 1e6,
    );
}

fn main() {
    println!(
        "pf-sim core, median of {TRIALS} trials, zero allocations asserted in every measured loop"
    );
    println!("row size {} bytes", std::mem::size_of::<Row>());
    bench_rows(1_000);
    bench_rows(100_000);
    bench_runs();
}
