//! A warm worker runs seed after seed without touching the heap.

use std::hint::black_box;

use pf_sim::action::{self, ActionKind, ActionList, GenParams};
use pf_sim::ideal;
use pf_sim::media::{self, Cue, Fault, MediaSpec, Pacing, Schedule, StreamKind, StreamSpec};
use pf_sim::profile;
use pf_sim::record::{Recording, Row, RowKind, checksum};
use pf_sim::shrink;
use pf_sim::sweep::{self, Expect, SweepError};
use pf_sim::time::{MS, Rational, SEC};

use crate::alloc_count::allocations;

struct Worker {
    spec: MediaSpec,
    schedule: Schedule,
    rec: Recording,
    probe: Recording,
    actions: ActionList,
}

fn declare(spec: &mut MediaSpec, seed: u64) {
    spec.reset();
    let pacing = Pacing::Jitter { base: MS, bound: 10 * MS };
    let video = StreamKind::Video { fps: Rational::new(30_000, 1_001), keyframe_interval: 30 };
    spec.add_stream(StreamSpec { kind: video, duration: 10 * SEC, size: 4096, pacing }).unwrap();
    let audio = StreamKind::Audio { packet: 20 * MS };
    spec.add_stream(StreamSpec {
        kind: audio,
        duration: 10 * SEC,
        size: 256,
        pacing: Pacing::RealTime,
    })
    .unwrap();
    let cues = [
        Cue { start: SEC, end: 3 * SEC, size: 30 },
        Cue { start: 5 * SEC, end: 6 * SEC, size: 12 },
    ];
    spec.add_text(&cues, Pacing::RealTime).unwrap();
    spec.add_fault(0, Fault::StallAt { buffer: seed % 200, hold: 80 * MS }).unwrap();
    spec.add_fault(1, Fault::LatePad { delay: 30 * MS }).unwrap();
}

fn run(w: &mut Worker, seed: u64) -> u64 {
    declare(&mut w.spec, seed);
    let params = GenParams {
        count: 10,
        duration: 10 * SEC,
        sinks: 3,
        max_buffers: 300,
        buffer_anchor_percent: 30,
        streams: 3,
        tracks: 1,
        weights: GenParams::PLAYBACK_WEIGHTS,
    };
    action::generate(seed, &params, &mut w.actions);

    let mut digest = 0;
    for stream in 0..3 {
        let summary = media::fill(&w.spec, stream, seed, &mut w.schedule).unwrap();
        digest ^= summary.end_at ^ w.schedule.slots().len() as u64;
    }

    let played = ideal::play(&w.spec, seed, w.actions.as_slice(), &mut w.rec).unwrap();
    assert!(!played.truncated);
    let sinks = ideal::expect();
    let expect = Expect { sinks: &sinks[..3], gaps: &[] };
    assert_eq!(sweep::check(&w.rec, &expect), Ok(()));
    digest ^= checksum(w.rec.rows());

    w.rec.push(Row::new(RowKind::Buffer, 9, 0));
    assert!(matches!(sweep::check(&w.rec, &expect), Err(SweepError::Violation(_))));

    let (spec, probe) = (&w.spec, &mut w.probe);
    let flushes =
        |rec: &Recording| rec.rows().iter().filter(|row| row.kind == RowKind::FlushStart).count();
    let shrunk = shrink::shrink(&mut w.actions, 500, |candidate| {
        ideal::play(spec, seed, candidate, probe).is_ok() && flushes(probe) >= 3
    });
    if shrunk.is_ok() {
        assert!(w.actions.as_slice().iter().all(|a| matches!(a.kind, ActionKind::Seek { .. })));
        digest ^= w.actions.len() as u64;
    }

    let device = profile::for_run(seed, 1, 50);
    digest ^ device.open_delay ^ device.ring_depth
}

#[test]
fn a_warm_worker_allocates_nothing() {
    let mut w = Worker {
        spec: MediaSpec::with_capacity(8, 8),
        schedule: Schedule::with_capacity(1_024),
        rec: Recording::with_capacity(16_384),
        probe: Recording::with_capacity(16_384),
        actions: ActionList::new(),
    };
    let mut digest = run(&mut w, 0);

    let before = allocations();
    let mut shrinks = 0;
    for seed in 1..=200 {
        digest ^= run(&mut w, seed);
        shrinks += u32::from(w.actions.len() < 10);
    }
    let allocated = allocations() - before;

    assert_eq!(allocated, 0, "200 warm runs allocated {allocated} times");
    assert!(shrinks > 20, "the shrinker ran on only {shrinks} of 200 seeds");
    black_box(digest);
}

#[test]
fn the_counter_sees_an_allocation() {
    let before = allocations();
    let boxed = black_box(Box::new(7u64));
    assert!(allocations() > before);
    drop(boxed);
}
