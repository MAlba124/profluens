//! End to end tests of the simulator over core's stepped driver.
//! `PF_SIM_SEEDS`, `PF_SIM_SEED`, `PF_SIM_WORKERS` and `PF_SIM_FROM` steer the sweeps.

// Tests build throwaway vectors.
#![allow(clippy::disallowed_methods)]

use std::ops::Range;
use std::sync::Arc;

use pf_sim::action::{Action, ActionKind, Anchor};
use pf_sim::driver::farm::{self, Failure, Report};
use pf_sim::driver::gop::{GopDecl, How, Mapped};
use pf_sim::driver::net::{Break, NetDecl, Slip};
use pf_sim::driver::offers::{Fixed, MAX_IDS, MAX_OFFERS, Offer, PadOffers};
use pf_sim::driver::oracle::{self, End};
use pf_sim::driver::rig::{self, BuildError};
use pf_sim::driver::run::Worker;
use pf_sim::driver::scenario::{self, MAX_CUES, MAX_FAULTS, Scenario, Topology, TransformDecl};
use pf_sim::driver::verdict::Kind;
use pf_sim::media::{Fault, MediaSpec};
use pf_sim::profile::SinkProfile;
use pf_sim::record::{Row, RowKind, flag};
use pf_sim::sweep::Rule;
use pf_sim::time::{MS, NONE, SEC};

const DEFAULT_SEEDS: u64 = 3_000;
/// Runs per second per core in release.
const BUDGET: f64 = 1_000.0;

fn var(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn seeds() -> Range<u64> {
    match var("PF_SIM_SEED") {
        Some(one) => one..one + 1,
        None => {
            let from = var("PF_SIM_FROM").unwrap_or(0);
            from..from + var("PF_SIM_SEEDS").unwrap_or(DEFAULT_SEEDS)
        }
    }
}

fn workers() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    var("PF_SIM_WORKERS").map_or(cores.min(8), |n| n as usize)
}

fn explain(sc: &Scenario) -> String {
    let mut worker = Worker::new();
    match worker.run(sc) {
        Ok(_) => "passes when run again".into(),
        Err(verdict) => {
            let failure =
                Failure { worker: &worker, scenario: sc, verdict: &verdict, trace_rows: 80 };
            format!("{failure}")
        }
    }
}

fn assert_clean(report: &Report, scenario: fn(u64) -> Scenario) {
    println!("{report}");
    let Some(first) = report.failures.first() else {
        assert_eq!(report.failed, 0, "failures past the kept ones");
        return;
    };
    let sc = scenario(first.seed);
    let mut worker = Worker::new();
    let shrunk = farm::shrink(&mut worker, &sc, first, 2_000);
    let cut = shrunk.map_or(sc, |s| s.scenario);
    let seeds: Vec<u64> = report.failures.iter().map(|v| v.seed).collect();
    panic!("{} seeds failed, the first of them {seeds:?}\n{}", report.failed, explain(&cut));
}

#[test]
fn every_seed_of_the_family_passes_every_oracle() {
    let range = seeds();
    let whole = range.end - range.start >= DEFAULT_SEEDS;
    let report = farm::sweep(range.clone(), workers(), scenario::draw);
    assert_eq!(report.runs, range.end - range.start, "every seed ran");
    assert_clean(&report, scenario::draw);
    if whole {
        assert!(report.finished > report.runs / 2, "most runs play to their end");
        assert!(report.stopped > 0 && report.errored > 0, "stops and declared errors ran");
        assert!(report.unlinked > 0 && report.rejected > 0, "offers refused and turned away");
        assert!(report.seeks > report.runs, "seeks ran");
        assert!(report.skips > 0 && report.bad_seeks > 0, "skips ran, and seeks no rule sends");
    }
}

/// First failing seeds of the 2026-10-10 scheduler bugs and early oracle errors, pinned.
#[test]
fn the_seeds_that_found_the_known_scheduler_bugs_pass() {
    let mut worker = Worker::new();
    let found = [1297, 3, 21, 36, 8741, 46, 9, 51, 278];
    let seeds = [50, 0, 14, 495, 1903, 450162, 565101, 784341];
    for seed in found.into_iter().chain(seeds) {
        let (sc, plain) = (scenario::draw(seed), scenario::draw_undecoded(seed));
        let (untimed, open) = (scenario::draw_untimed(seed), scenario::draw_open(seed));
        let offline = scenario::draw_offline(seed);
        for sc in [sc, offline, untimed, plain, open, Scenario { split: 0, ..open }] {
            let end = worker.run(&sc).unwrap_or_else(|_| panic!("{}", explain(&sc))).end;
            assert!(sc != open || !matches!(end, End::Refused | End::Rejected), "seed {seed}");
        }
    }
    assert!(scenario::draw(278).split > 0, "the seed that needs a cut still draws one");
    assert!(seeds.iter().all(|seed| scenario::draw_untimed(*seed).gop.on));
}

/// The first 32 failures of a billion seeds (2026-10-22), each an oracle bug.
fn pass_as_drawn(seeds: &[u64], shape: fn(&Scenario) -> bool) {
    let mut worker = Worker::new();
    for &seed in seeds {
        let sc = scenario::draw(seed);
        assert!(shape(&sc), "seed {seed} draws another run");
        if worker.run(&sc).is_err() {
            panic!("{}", explain(&sc));
        }
    }
}

#[test]
fn a_rate_behind_a_seek_and_a_skip_back_is_bounded_from_the_stretch() {
    let seeds = [
        1065713, 1165685, 3027915, 3322457, 3868915, 4031639, 4171853, 5093653, 11663231, 13683331,
        16776505, 18846385, 24603988, 30710423, 33468922, 36324106,
    ];
    pass_as_drawn(&seeds, |sc| sc.time.retimer.on);
}

#[test]
fn an_end_behind_a_point_only_the_decoder_was_told_owes_nothing_before_it() {
    let seeds = [15483426, 25388856, 26921069, 29389234, 32241901, 35832404, 36073350, 38053761];
    pass_as_drawn(&seeds, |sc| sc.gop.on);
}

#[test]
fn what_a_decoder_dropped_at_a_point_the_sink_was_told_over_is_not_missing() {
    let seeds = [8268225, 11266377, 13098911, 16770968, 16797102, 18266356, 22485012, 32356034];
    pass_as_drawn(&seeds, |sc| sc.gop.on);
}

#[test]
fn the_family_exercises_every_row_kind_and_every_end() {
    let mut worker = Worker::new();
    let mut kinds = [0u64; 28];
    let (mut ends, mut refused, mut inside) = ([0u64; 6], 0u64, 0u64);
    for seed in 0..3_000u64 {
        let sc = scenario::draw(seed);
        let summary = worker.run(&sc).unwrap_or_else(|_| panic!("{}", explain(&sc)));
        ends[summary.end as usize] += 1;
        refused += summary.refused as u64;
        for row in worker.rows().iter().chain(worker.decoded()) {
            kinds[row.kind as usize] += 1;
        }
        inside += worker.rows().iter().filter(|r| r.kind == RowKind::Command).count() as u64;
    }
    let seen = |kind: RowKind| kinds[kind as usize];
    for kind in [
        RowKind::Buffer,
        RowKind::Drop,
        RowKind::Format,
        RowKind::Segment,
        RowKind::Eos,
        RowKind::FlushStart,
        RowKind::Paused,
        RowKind::Resumed,
        RowKind::Start,
        RowKind::Stop,
        RowKind::Command,
        RowKind::Skip,
        RowKind::Before,
        RowKind::Decoded,
        RowKind::DecodeOnly,
        RowKind::Skipped,
        RowKind::Retimed,
        RowKind::Retime,
        RowKind::Offset,
    ] {
        assert!(seen(kind) > 100, "{} rows of kind {kind:?}", seen(kind));
    }
    assert!(seen(RowKind::RateAsked) > 50, "{} rates reached a retimer", seen(RowKind::RateAsked));
    assert!(seen(RowKind::Format) > seen(RowKind::Start), "formats change in mid stream");
    assert!(ends[..5].iter().all(|n| *n > 40), "every way a run ends, {ends:?}");
    assert!(ends[End::BadSeek as usize] > 2, "and on a seek no rule sends, {ends:?}");
    assert!(refused > 500 && inside > 100, "{refused} refusals, {inside} from inside a pass");
}

#[test]
fn the_reference_scenario_passes_every_oracle() {
    let count = (seeds().end - seeds().start).min(200);
    let report = farm::sweep(0..count.max(1), workers(), scenario::reference);
    assert_clean(&report, scenario::reference);
    assert_eq!(report.finished, report.runs, "the reference always plays to its end");
}

#[test]
fn one_seed_gives_one_run() {
    let mut worker = Worker::new();
    let mut other = Worker::new();
    let mut sums = Vec::new();
    for seed in 0..300u64 {
        let sc = scenario::draw(seed);
        let first = worker.run(&sc);
        let rows: Vec<Row> = worker.rows().to_vec();
        let second = other.run(&sc);
        assert_eq!(first, second, "seed {seed} ran two ways");
        assert!(rows == other.rows(), "seed {seed} recorded two logs");
        assert_eq!(worker.trace().digest(), other.trace().digest(), "seed {seed}");
        let ran = first.ok().filter(|summary| summary.end != End::Refused);
        sums.extend(ran.map(|summary| summary.checksum));
    }
    let ran = sums.len();
    sums.sort_unstable();
    sums.dedup();
    assert!(ran > 250 && sums.len() + 10 > ran, "{} distinct of {ran} runs", sums.len());
}

/// A scenario whose run panics, as core would on a seed it cannot take.
fn panicking(seed: u64) -> Scenario {
    assert!(seed != 7, "seed 7 is the one that panics");
    scenario::draw(seed)
}

#[test]
fn a_panic_in_a_run_is_a_verdict_naming_its_seed_and_the_sweep_goes_on() {
    let report = farm::sweep(0..20, 3, panicking);
    assert_eq!((report.runs, report.failed), (20, 1), "{report}");
    let verdict = report.failures.first().expect("the panic is kept");
    assert_eq!((verdict.seed, verdict.kind), (7, Kind::Panicked));
    assert_eq!(
        report.finished
            + report.stopped
            + report.errored
            + report.rejected
            + report.unlinked
            + report.bad_seeks,
        19,
        "the seeds after it ran"
    );
}

#[test]
fn a_sweep_adds_up_the_same_on_any_worker_count() {
    let one = farm::sweep(0..400, 1, scenario::draw);
    let many = farm::sweep(0..400, 5, scenario::draw);
    assert_eq!((one.runs, many.runs), (400, 400));
    assert_eq!(one.checksum, many.checksum);
    assert_eq!((one.steps, one.rows, one.seeks), (many.steps, many.rows, many.seeks));
}

/// A deliberate failure. The table holds three seek replays and not four, so the
/// shrinker has something to cut.
fn overflowing() -> (Worker, Scenario) {
    let replay = |count| seek(Anchor::AfterEos { sink: 0, count, lag: 0 }, 0);
    let pause = Action { anchor: Anchor::AtStep(3), kind: ActionKind::Pause };
    let resume = Action { anchor: Anchor::AtStep(9), kind: ActionKind::Resume };
    let script = [pause, replay(1), resume, replay(2), replay(3)];
    let sc = plain(Topology::Line).with_script(&script);
    let rows = |actions: &[Action]| {
        Worker::new().run(&sc.with_script(actions)).map_or(0, |summary| summary.rows as usize)
    };
    let (three, four) = (rows(&[replay(1), replay(2)]), rows(&script));
    assert!(three > 0 && four > three + 4, "{three} and {four} rows");
    (Worker::with_rows((three + four) / 2), sc)
}

#[test]
fn a_failing_seed_replays_alone_with_the_same_verdict() {
    let (mut worker, sc) = overflowing();
    let first = worker.run(&sc).expect_err("the recording is too small");
    assert_eq!(first.kind, Kind::RecordingOverflow);
    assert_eq!(first.seed, sc.seed);
    let again = worker.run(&sc).expect_err("it fails again");
    assert_eq!(first, again);
    let elsewhere = overflowing().0.run(&sc).expect_err("and on a fresh worker");
    assert_eq!(first, elsewhere);
    let text =
        format!("{}", Failure { worker: &worker, scenario: &sc, verdict: &again, trace_rows: 5 });
    assert!(text.contains("RecordingOverflow") && text.contains("seek to"), "{text}");
    assert!(text.contains("trace, the last 5"), "{text}");
}

#[test]
fn the_shrinker_cuts_a_script_to_what_the_verdict_needs() {
    let (mut worker, sc) = overflowing();
    let verdict = worker.run(&sc).expect_err("the recording is too small");
    let shrunk = farm::shrink(&mut worker, &sc, &verdict, 500).expect("shrinks");
    assert!(!shrunk.exhausted);
    assert_eq!(shrunk.verdict.kind, verdict.kind);
    // Three replays fill the table. The pause and the resume add nothing.
    let script = shrunk.scenario.script;
    assert_eq!(script.len(), 3, "{}", farm::script_of(&shrunk.scenario));
    assert!(script.as_slice().iter().all(|a| matches!(a.kind, ActionKind::Seek { .. })));
    for cut in 0..script.len() {
        let mut fewer = script.as_slice().to_vec();
        fewer.remove(cut);
        assert!(worker.run(&sc.with_script(&fewer)).is_ok(), "without action {cut}");
    }
    let passes = sc.with_script(&[]);
    let nothing = farm::shrink(&mut Worker::new(), &passes, &verdict, 10);
    assert_eq!(nothing.err(), Some(farm::ShrinkFailure::NotReproduced));
}

fn plain(topology: Topology) -> Scenario {
    scenario::plain(5, topology, 400 * MS)
}

fn seek(anchor: Anchor, to: u64) -> Action {
    Action { anchor, kind: ActionKind::Seek { to } }
}

fn played(sc: &Scenario) -> (Worker, Vec<Row>, End) {
    let mut worker = Worker::new();
    let summary = worker.run(sc).unwrap_or_else(|_| panic!("{}", explain(sc)));
    let rows = worker.rows().to_vec();
    (worker, rows, summary.end)
}

fn swept(sc: &Scenario, change: impl FnOnce(&mut Vec<Row>)) -> Option<Kind> {
    let (worker, mut rows, end) = played(sc);
    change(&mut rows);
    oracle::sweep(sc, worker.declared(), worker.app(), end, &rows).err().map(|found| found.kind)
}

type Change = fn(&mut Vec<Row>, usize);

fn cut(rows: &mut Vec<Row>, at: usize) {
    rows.remove(at);
}

fn after_seek(rows: &[Row], kind: RowKind, n: usize) -> usize {
    let found = rows.iter().enumerate().filter(|(_, r)| r.kind == kind && r.seek_gen == 1).nth(n);
    found.map(|(i, _)| i).unwrap_or_else(|| panic!("no {kind:?} number {n} after the seek"))
}

fn nth(rows: &[Row], kind: RowKind, n: usize) -> usize {
    let found = rows.iter().enumerate().filter(|(_, r)| r.kind == kind).nth(n);
    found.map(|(i, _)| i).unwrap_or_else(|| panic!("no {kind:?} number {n}"))
}

#[test]
fn a_played_run_has_the_declared_shape() {
    let sc = plain(Topology::Line)
        .with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 4 }, 200 * MS)]);
    let (worker, rows, end) = played(&sc);
    assert_eq!(end, End::Finished);
    let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
    // Ten frames at 25 fps. Four or more, then all ten after the seek to zero.
    let buffers = |seek_gen| {
        rows.iter().filter(|r| r.kind == RowKind::Buffer && r.seek_gen == seek_gen).count()
    };
    assert!((4..=10).contains(&buffers(0)), "{}", buffers(0));
    assert_eq!(buffers(1), 10);
    assert_eq!(kinds[..3], [RowKind::Start, RowKind::Segment, RowKind::Format]);
    assert_eq!(kinds[kinds.len() - 2..], [RowKind::Eos, RowKind::Stop]);
    let flush = &rows[nth(&rows, RowKind::FlushStart, 0)];
    assert_eq!((flush.seek_gen, flush.aux), (1, 200 * MS));
    let after = &rows[after_seek(&rows, RowKind::Buffer, 0)];
    assert_eq!((after.seq, after.seek_gen, after.format), (0, 1, 10));
    assert!(after.has(flag::DISCONT | flag::KEYFRAME));
    assert_eq!(worker.app().seek_gen, 1);
    assert!(rows.windows(2).all(|w| w[0].at <= w[1].at), "virtual time never goes back");
}

#[test]
fn the_sweep_catches_each_way_a_log_can_be_wrong() {
    let sc = plain(Topology::Line)
        .with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 4 }, 200 * MS)]);
    assert_eq!(swept(&sc, |_| {}), None);
    let cases: [(&str, Kind, Change); 12] = [
        ("a lost buffer", Kind::Missing, |rows, at| cut(rows, at)),
        ("a buffer twice", Kind::Repeated, |rows, at| rows.insert(at, rows[at])),
        ("a buffer of the old generation", Kind::Generation, |rows, at| rows[at].seek_gen = 0),
        ("another pts", Kind::Content, |rows, at| rows[at].pts += 1),
        ("another size", Kind::Content, |rows, at| rows[at].size += 1),
        ("a keyframe that is none", Kind::Content, |rows, at| rows[at].flags = flag::KEYFRAME),
        ("another format", Kind::Format, |rows, at| rows[at].format = 20),
        ("a tag that did not decode", Kind::Content, |rows, at| rows[at].stream = u16::MAX),
        ("a stray discontinuity", Kind::Content, |rows, at| rows[at].flags |= flag::DISCONT),
        ("an early drop", Kind::Paced, |rows, at| rows[at].kind = RowKind::Drop),
        ("a second start", Kind::Lifecycle, |rows, at| rows[at] = rows[0]),
        ("a resume while playing", Kind::Transport, |rows, at| rows[at].kind = RowKind::Resumed),
    ];
    for (what, kind, change) in cases {
        let got = swept(&sc, |rows| {
            let at = after_seek(rows, RowKind::Buffer, 2);
            change(rows, at)
        });
        assert_eq!(got, Some(kind), "{what}");
    }
}

#[test]
fn the_sweep_catches_what_is_wrong_around_a_seek_and_an_end() {
    let sc = plain(Topology::Line)
        .with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 4 }, 200 * MS)]);
    let got = |kind: RowKind, n: usize, change: Change| {
        swept(&sc, |rows| {
            let at = nth(rows, kind, n);
            change(rows, at)
        })
    };
    let remove: Change = cut;
    let first = |change: Change| {
        swept(&sc, |rows| {
            let at = after_seek(rows, RowKind::Buffer, 0);
            change(rows, at)
        })
    };
    assert_eq!(first(remove), Some(Kind::Missing));
    assert_eq!(first(|rows, at| rows[at].flags &= !flag::DISCONT), Some(Kind::Content));
    assert_eq!(got(RowKind::FlushStart, 0, |rows, at| rows[at].aux += 1), Some(Kind::FlushTarget));
    assert_eq!(
        got(RowKind::FlushStart, 0, |rows, at| rows[at].seek_gen = 2),
        Some(Kind::FlushOrder)
    );
    assert_eq!(got(RowKind::FlushStart, 0, remove), Some(Kind::Generation));
    assert_eq!(got(RowKind::Format, 0, remove), Some(Kind::Format));
    assert_eq!(got(RowKind::Eos, 0, |rows, at| rows.swap(at, at - 1)), Some(Kind::Incomplete));
    assert_eq!(got(RowKind::Eos, 0, remove), Some(Kind::NotEnded));
    let twice = got(RowKind::Eos, 0, |rows, at| rows.insert(at, rows[at]));
    assert_eq!(twice, Some(Kind::Rule(Rule::EosOnce)));
    assert_eq!(got(RowKind::Stop, 0, remove), Some(Kind::Lifecycle));
    assert_eq!(got(RowKind::Segment, 1, remove), Some(Kind::Rule(Rule::SegmentBeforeBuffer)));
}

#[test]
fn a_reordering_window_and_a_leaky_ring_loosen_the_sweep_by_what_they_allow() {
    let mut sc = plain(Topology::Transform);
    sc.transform = TransformDecl { delay: 2, window: 3, error_at: NONE, seed: 9 };
    sc.pool_slots = 16;
    let (_, rows, end) = played(&sc);
    assert_eq!(end, End::Finished);
    let seqs: Vec<u64> = rows.iter().filter(|r| r.kind == RowKind::Buffer).map(|r| r.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (0..10).collect::<Vec<u64>>(), "every buffer once");
    assert_ne!(seqs, sorted, "and not in order");
    assert!(seqs.iter().enumerate().all(|(i, seq)| seq.abs_diff(i as u64) <= 3), "{seqs:?}");
    let moved = swept(&sc, |rows| {
        let (from, to) = (nth(rows, RowKind::Buffer, 0), nth(rows, RowKind::Buffer, 9));
        rows.swap(from, to);
    });
    assert!(moved.is_some(), "a buffer three blocks from its place");
    let lost = swept(&sc, |rows| cut(rows, nth(rows, RowKind::Buffer, 5)));
    assert!(matches!(lost, Some(Kind::Missing | Kind::Incomplete)), "{lost:?}");

    let mut leaky = plain(Topology::Line);
    leaky.sinks[0].leaky = true;
    assert_eq!(swept(&leaky, |rows| cut(rows, nth(rows, RowKind::Buffer, 3))), None);
    let twice = swept(&leaky, |rows| {
        rows.insert(nth(rows, RowKind::Buffer, 3), rows[nth(rows, RowKind::Buffer, 3)])
    });
    assert_eq!(twice, Some(Kind::Repeated));
    let no_format = swept(&leaky, |rows| cut(rows, nth(rows, RowKind::Format, 0)));
    assert_eq!(no_format, Some(Kind::Format));
}

#[test]
fn declared_faults_end_a_run_the_way_they_say() {
    let mut early = plain(Topology::TwoStreams);
    assert!(early.source.add_fault(0, Fault::EndEarlyAt { buffer: 3 }));
    let (_, rows, end) = played(&early);
    assert_eq!(end, End::Finished);
    let on =
        |sink: u16| rows.iter().filter(|r| r.sink == sink && r.kind == RowKind::Buffer).count();
    assert_eq!((on(0), on(1)), (3, 20), "stream 0 ends early, stream 1 plays out");

    let mut error = plain(Topology::Line);
    assert!(error.source.add_fault(0, Fault::ErrorAt { buffer: 6 }));
    let (_, rows, end) = played(&error);
    assert_eq!(end, End::Failed);
    assert!(rows.iter().all(|r| r.kind != RowKind::Eos), "a failed stream has no end");
    assert_eq!(rows.last().map(|r| r.kind), Some(RowKind::Stop));

    let mut broken = plain(Topology::Transform);
    broken.transform.error_at = 4;
    assert_eq!(played(&broken).2, End::Failed);

    let mut stalled = plain(Topology::Line);
    assert!(stalled.source.add_fault(0, Fault::StallAt { buffer: 5, hold: 700 * MS }));
    let (_, rows, end) = played(&stalled);
    assert_eq!(end, End::Finished);
    let at =
        |seq: u64| rows.iter().find(|r| r.kind == RowKind::Buffer && r.seq == seq).map(|r| r.at);
    assert_eq!(at(4), Some(0));
    assert_eq!(at(5), Some(700 * MS), "the stalled buffer leaves when the hold is over");
}

#[test]
fn a_limited_link_is_held_to_its_limit_and_the_stream_is_whole() {
    let mut sc = plain(Topology::Line);
    sc.sinks[0].profile = SinkProfile { paced: true, ..SinkProfile::IDEAL };
    let step = sc.sink_step(0).expect("pictures are a frame apart");
    sc.sinks[0].limit = 2 * step;
    let (_, rows, end) = played(&sc);
    assert_eq!(end, End::Finished);
    let buffers: Vec<&Row> = rows.iter().filter(|r| r.kind == RowKind::Buffer).collect();
    assert_eq!(buffers.len(), 10);
    assert!(buffers.iter().all(|r| r.aux == r.pts && r.at == r.pts), "taken exactly when due");
}

#[test]
fn the_family_limits_links_and_asks_a_retimer_behind_one_for_rates() {
    let (mut limited, mut held) = (0, 0);
    for seed in 0..4_000 {
        let sc = scenario::draw(seed);
        let sinks = &sc.sinks[..sc.sink_count()];
        limited += sinks.iter().any(|sink| sink.limit > 0) as u32;
        let rates =
            sc.script.as_slice().iter().any(|a| matches!(a.kind, ActionKind::SetRate { .. }));
        let behind = |n: usize| sc.retimed(n) && sinks[n].limit > 0 && sinks[n].profile.paced;
        held += (rates && (0..sinks.len()).any(behind)) as u32;
    }
    assert!((1_200..2_000).contains(&limited) && held > 30, "{limited} limited, {held} held");
}

#[test]
fn a_paced_sink_takes_on_the_deadline_and_drops_past_its_bound() {
    let mut sc = plain(Topology::Line);
    sc.sinks[0].profile = SinkProfile { paced: true, ..SinkProfile::IDEAL };
    let (_, rows, _) = played(&sc);
    let buffers: Vec<&Row> = rows.iter().filter(|r| r.kind == RowKind::Buffer).collect();
    assert_eq!(buffers.len(), 10);
    assert!(buffers.iter().all(|r| r.aux == r.pts && r.at == r.pts), "taken exactly when due");

    sc.sinks[0].profile.lateness_bound = 10 * MS;
    assert!(sc.source.add_fault(0, Fault::StallAt { buffer: 2, hold: 200 * MS }));
    let (_, rows, end) = played(&sc);
    assert_eq!(end, End::Finished);
    let kind_of = |seq: u64| {
        rows.iter()
            .find(|r| r.kind.is(pf_sim::record::class::ARRIVAL) && r.seq == seq)
            .map(|r| r.kind)
    };
    assert_eq!(
        (kind_of(1), kind_of(2), kind_of(6)),
        (Some(RowKind::Buffer), Some(RowKind::Drop), Some(RowKind::Buffer))
    );
}

#[test]
fn an_action_inside_a_flush_is_applied_by_the_next_pass() {
    let inside = Anchor::InFlush { sink: 0, count: 1 };
    let script = [seek(Anchor::AfterBuffers { sink: 0, count: 3 }, 0), seek(inside, 200 * MS)];
    let sc = plain(Topology::TwoStreams).with_script(&script);
    let (worker, rows, end) = played(&sc);
    assert_eq!((end, worker.app().seek_gen), (End::Finished, 2));
    let flushes: Vec<(u16, u32)> = rows
        .iter()
        .filter(|r| r.kind == RowKind::FlushStart)
        .map(|r| (r.sink, r.seek_gen))
        .collect();
    // Sink 0 flushed for both seeks. Sink 1 may have met both at once.
    assert!(flushes.contains(&(0, 1)) && flushes.contains(&(0, 2)), "{flushes:?}");
    assert!(flushes.contains(&(1, 2)), "{flushes:?}");
    let command = &rows[nth(&rows, RowKind::Command, 0)];
    assert_eq!((command.sink, command.seq, command.aux, command.seek_gen), (0, 1, 0, 2));
}

#[test]
fn a_cut_pass_lets_the_rest_of_the_run_into_its_middle() {
    use pf_sim::driver::trace::Did;
    let script = [seek(Anchor::AfterBuffers { sink: 0, count: 3 }, 200 * MS)];
    let whole = plain(Topology::TwoStreamsTransform).with_script(&script);
    let (worker, rows, end) = played(&whole);
    let tops = worker.trace().tail(usize::MAX).filter(|r| matches!(r.did, Did::Top { .. })).count();
    assert_eq!((end, tops), (End::Finished, 0));
    let buffers = |rows: &[Row], sink: u16| {
        let last = |r: &&Row| r.sink == sink && r.kind == RowKind::Buffer && r.seek_gen == 1;
        rows.iter().filter(last).map(|r| r.seq).collect::<Vec<u64>>()
    };

    let mut seen = 0;
    for split in 1..=4 {
        let cut = Scenario { split, ..whole };
        let mut worker = Worker::new();
        let summary = worker.run(&cut).unwrap_or_else(|_| panic!("{}", explain(&cut)));
        assert!(summary.cuts > 0 && summary.end == End::Finished, "split {split}");
        assert!(summary.cuts <= summary.passes, "a pass is cut once");
        let trace: Vec<Did> = worker.trace().tail(usize::MAX).map(|r| r.did).collect();
        for (at, did) in trace.iter().enumerate() {
            let Did::Top { group } = *did else { continue };
            let rest = trace[at + 1..].iter().position(|d| match d {
                Did::Pass { group: g, .. } | Did::Top { group: g } => *g == group,
                _ => false,
            });
            let rest = rest.unwrap_or_else(|| panic!("split {split}: a pass was never finished"));
            assert!(matches!(trace[at + 1 + rest], Did::Pass { .. }), "split {split}");
            seen += (rest > 0) as u32;
        }
        for sink in 0..2 {
            assert_eq!(buffers(worker.rows(), sink), buffers(&rows, sink), "split {split}");
        }
    }
    assert!(seen > 20, "{seen} passes had something in their middle");
}

#[test]
fn what_is_left_of_a_script_fires_when_the_run_goes_quiet() {
    let late = Anchor::AtStep(1_000_000);
    let never = Anchor::AfterBuffers { sink: 0, count: 9_999 };
    let pause = |anchor| Action { anchor, kind: ActionKind::Pause };
    let sc = plain(Topology::Line).with_script(&[seek(late, 0), pause(late), pause(never)]);
    let (worker, rows, end) = played(&sc);
    assert_eq!(end, End::Finished);
    assert_eq!(worker.answers()[..3], [Answer::Finished, Answer::Finished, Answer::Unfired]);
    assert_eq!(rows.iter().filter(|r| r.kind == RowKind::FlushStart).count(), 0);

    // Paced, since an unpaced sink plays to its end through a pause.
    let mut stuck = plain(Topology::Line).with_script(&[pause(Anchor::AtStep(2))]);
    stuck.sinks[0].profile.paced = true;
    let (worker, rows, end) = played(&stuck);
    assert_eq!((end, worker.app().taken), (End::Finished, 2));
    assert_eq!(rows.iter().filter(|r| r.kind == RowKind::Resumed).count(), 1);
}

fn offering(family: u8, ids: &[u8]) -> PadOffers {
    let mut offer = Offer { family, ids: [0; MAX_IDS], id_count: ids.len() as u8, step: 0 };
    offer.ids[..ids.len()].copy_from_slice(ids);
    PadOffers { list: [offer; MAX_OFFERS], count: 1 }
}

fn offered(topology: Topology, src: PadOffers, sink: PadOffers) -> Scenario {
    let mut sc = plain(topology);
    sc.offers.srcs[0] = src;
    sc.offers.sinks[0] = sink;
    sc
}

fn told(rows: &[Row], sink: u16) -> Vec<u64> {
    let formats = rows.iter().filter(|r| r.sink == sink && r.kind == RowKind::Format);
    formats.map(|r| r.aux).collect()
}

#[test]
fn offers_with_no_common_format_are_refused_at_link_and_the_run_never_starts() {
    let line = |src, sink| offered(Topology::Line, src, sink);
    let mut second_of_two = plain(Topology::TwoStreams);
    second_of_two.offers.sinks[1] = offering(1, &[11]);
    let mut second_of_fan = offered(Topology::FanOut, offering(0, &[20]), offering(0, &[20]));
    second_of_fan.offers.sinks[1] = offering(0, &[10, 30]);
    let mut apart = offered(Topology::FanOut, offering(0, &[10, 20]), offering(0, &[10]));
    apart.offers.sinks[1] = offering(0, &[20]);
    let cases = [
        ("no id in common", line(offering(0, &[10, 20]), offering(0, &[30])), 0),
        ("another family", line(offering(1, &[10]), offering(0, &[10])), 0),
        ("a static menu against another family", line(offering(1, &[]), PadOffers::OPEN), 0),
        ("two outputs of the transform", apart, 2),
        (
            "behind an inlined transform",
            offered(Topology::Inline, PadOffers::OPEN, offering(1, &[])),
            1,
        ),
        ("the second of two streams", second_of_two, 1),
        ("the second sink of a fan-out", second_of_fan, 2),
    ];
    for (what, sc, made) in cases {
        let plan = sc.plan();
        assert!(plan.refused && plan.made == made, "{what}: the plan links {}", plan.made);
        let mut worker = Worker::new();
        let summary = worker.run(&sc).unwrap_or_else(|_| panic!("{what}\n{}", explain(&sc)));
        assert_eq!((summary.end, summary.steps, summary.passes), (End::Refused, 0, 0), "{what}");
        assert!(worker.rows().is_empty() && worker.trace().count() == 0, "{what}: nothing ran");
    }
}

#[test]
fn the_rig_holds_every_link_against_the_plan() {
    let spec = |sc: &Scenario| {
        let mut spec = MediaSpec::with_capacity(MAX_CUES, MAX_FAULTS);
        sc.source.fill(&mut spec).expect("a servable declaration");
        Arc::new(spec)
    };
    let twenty = offered(Topology::Line, offering(0, &[20, 10]), offering(0, &[10, 20]));
    let ten = offered(Topology::Line, offering(0, &[10, 20]), offering(0, &[10, 20]));
    let none = offered(Topology::Line, offering(0, &[20]), offering(0, &[10]));
    let build = |sc: &Scenario, under: &Scenario| rig::build(sc, &under.plan(), spec(sc));
    let (want, got) = (Fixed { family: 0, id: 10 }, Fixed { family: 0, id: 20 });

    assert!(matches!(build(&twenty, &twenty), Ok(Some(_))));
    assert!(matches!(build(&none, &none), Ok(None)), "refused as planned");
    let made = build(&twenty, &none);
    assert!(matches!(made, Err(BuildError::Made { link: 0, got: g }) if g == got));
    let refused = build(&none, &ten);
    assert!(matches!(refused, Err(BuildError::Refused { link: 0, want: w }) if w == want));
    let other = build(&twenty, &ten);
    let differs = |w, g| w == want && g == got;
    assert!(matches!(other, Err(BuildError::Format { link: 0, want: w, got: g }) if differs(w, g)));

    let mut carried = offered(Topology::FanOut, offering(0, &[20, 10]), offering(0, &[10]));
    carried.offers.sinks[1] = offering(0, &[20]);
    assert!(matches!(build(&carried, &carried), Ok(None)));
    let late = offered(Topology::Transform, offering(0, &[10, 20]), offering(0, &[10]));
    assert!(matches!(build(&carried, &late), Err(BuildError::Format { link: 0, .. })));
    let moved = offered(Topology::Transform, offering(0, &[20, 10]), offering(0, &[10]));
    assert!(matches!(build(&moved, &moved), Ok(Some(_))));
}

#[test]
fn a_sink_is_told_what_its_link_fixed_and_each_change_before_the_first_buffer_under_it() {
    for topology in [Topology::Line, Topology::Transform, Topology::Inline] {
        let mut sc = offered(topology, offering(0, &[30, 10]), offering(0, &[10, 30, 40]));
        (sc.offers.second[0], sc.source.format_change[0]) = (40, 4);
        let (_, rows, end) = played(&sc);
        assert_eq!(end, End::Finished, "{topology:?}");
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds[..2], [RowKind::Start, RowKind::Format], "{topology:?}");
        assert_eq!(told(&rows, 0), [30, 40], "{topology:?}");
        let buffers: Vec<(u64, u8)> =
            rows.iter().filter(|r| r.kind == RowKind::Buffer).map(|r| (r.seq, r.format)).collect();
        assert_eq!(buffers.len(), 10);
        assert!(buffers.iter().all(|(seq, format)| *format == if *seq < 4 { 30 } else { 40 }));
        let change = rows.iter().position(|r| r.kind == RowKind::Format && r.aux == 40);
        let first = rows.iter().position(|r| r.kind == RowKind::Buffer && r.format == 40);
        assert!(change < first, "{topology:?}: the change comes before the first buffer under it");
    }
    let anchor = Anchor::AfterBuffers { sink: 0, count: 7 };
    let mut sc = offered(Topology::Line, offering(0, &[30]), offering(0, &[30, 40]));
    (sc.offers.second[0], sc.source.format_change[0]) = (40, 4);
    let (_, rows, end) = played(&sc.with_script(&[seek(anchor, 0)]));
    assert_eq!((end, told(&rows, 0)), (End::Finished, vec![30, 40, 30, 40]));
}

#[test]
fn a_format_a_sink_does_not_offer_ends_the_run_as_the_typed_rejection() {
    let turned = |topology| {
        let mut sc = offered(topology, offering(0, &[30, 10]), offering(0, &[10, 30]));
        (sc.offers.second[0], sc.source.format_change[0]) = (40, 4);
        sc
    };
    for topology in [Topology::Line, Topology::Transform, Topology::Inline, Topology::FanOut] {
        let sc = turned(topology);
        assert!(!sc.plan().refused && sc.plan().turns_away(0, 40));
        let (_, rows, end) = played(&sc);
        assert_eq!(end, End::Rejected, "{topology:?}");
        assert_eq!(told(&rows, 0), [30], "{topology:?}: the format turned away never arrived");
        let on_sink = |r: &&Row| r.sink == 0 && r.kind == RowKind::Buffer;
        assert!(rows.iter().filter(on_sink).all(|r| r.format == 30 && r.seq < 4), "{topology:?}");
        assert!(rows.iter().all(|r| r.kind != RowKind::Eos), "a rejected run has no end of stream");
        assert_eq!(rows.last().map(|r| r.kind), Some(RowKind::Stop));
    }
    let mut fan = turned(Topology::FanOut);
    fan.offers.sinks = [offering(0, &[30, 40]), offering(0, &[30])];
    assert!(!fan.plan().turns_some_away(0) && fan.plan().turns_away(1, 40));
    assert_eq!(played(&fan).2, End::Rejected);

    for topology in [Topology::Transform, Topology::Line] {
        let told_30 = offered(topology, PadOffers::OPEN, offering(0, &[30]));
        assert!(
            told_30.plan().first(0) == 30 && !told_30.plan().turns_some_away(0),
            "{topology:?}"
        );
        let (_, rows, end) = played(&told_30);
        assert_eq!((end, told(&rows, 0)), (End::Finished, vec![30]), "{topology:?}");
    }
}

#[test]
fn the_sweep_holds_the_formats_of_a_log_against_the_offers() {
    let mut sc = offered(Topology::Line, offering(0, &[30, 10]), offering(0, &[10, 30, 40]));
    (sc.offers.second[0], sc.source.format_change[0]) = (40, 4);
    assert_eq!(swept(&sc, |_| {}), None);
    let got = |n: usize, change: Change| {
        swept(&sc, |rows| {
            let at = nth(rows, RowKind::Format, n);
            change(rows, at)
        })
    };
    assert_eq!(got(1, |rows, at| rows[at].aux = 20), Some(Kind::Unoffered), "20 is not offered");
    assert_eq!(got(1, |rows, at| rows[at].aux = 10), Some(Kind::Format), "offered, and not sent");
    assert_eq!(got(1, cut), Some(Kind::Format), "a buffer under a format nobody announced");
    let start = got(0, cut);
    assert!(
        matches!(start, Some(Kind::Format | Kind::Rule(Rule::FormatBeforeBuffer))),
        "{start:?}"
    );

    let (worker, rows, _) = played(&sc);
    let ended = |sc: &Scenario, end, rows: &[Row]| {
        oracle::sweep(sc, worker.declared(), worker.app(), end, rows).err().map(|found| found.kind)
    };
    let none = offered(Topology::Line, offering(0, &[20]), offering(0, &[10]));
    assert_eq!(ended(&sc, End::Refused, &[]), Some(Kind::LinkRefused));
    assert_eq!(ended(&none, End::Finished, &rows), Some(Kind::LinkMade));
    assert_eq!(ended(&none, End::Refused, &rows), Some(Kind::RowAfterEnd));
    assert_eq!(ended(&none, End::Refused, &[]), None);
}

fn decoded(how: How) -> Scenario {
    let mut sc = scenario::plain(5, Topology::Transform, 2 * SEC);
    sc.gop = GopDecl { on: true, forced: Some(how), ..GopDecl::OFF };
    sc.sinks[0].profile.paced = true;
    sc
}

const BY_THE_RULE: How = How::Planned { keyframe: false };

fn played_decoded(sc: &Scenario) -> (Worker, Vec<Row>, Vec<Row>, End) {
    let (worker, rows, end) = played(sc);
    let pictures = worker.decoded().to_vec();
    (worker, rows, pictures, end)
}

fn after_last(rows: &[Row], jump: RowKind, kind: RowKind) -> Vec<u64> {
    let from = rows.iter().rposition(|r| r.kind == jump).map_or(0, |at| at + 1);
    rows[from..].iter().filter(|r| r.kind == kind).map(|r| r.seq).collect()
}

fn count(rows: &[Row], kind: RowKind) -> usize {
    rows.iter().filter(|r| r.kind == kind).count()
}

#[test]
fn a_forward_seek_inside_the_gop_is_a_skip_that_decodes_no_picture_twice() {
    for (nonref, reorder) in [(0, 0), (3, 0), (0, 2), (2, 3)] {
        let mut sc = decoded(BY_THE_RULE);
        (sc.gop.nonref, sc.gop.reorder) = (nonref, reorder);
        sc.pool_slots = 4 + reorder as u32;
        // At 200 ms seek to 620 ms. Picture 15 is at 600 and 16 at 640.
        let sc = sc.with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 6 }, 620 * MS)]);
        let (worker, rows, pictures, end) = played_decoded(&sc);
        let summary = (end, worker.app().seek_gen, worker.app().jumps().len());
        assert_eq!(summary, (End::Finished, 0, 1), "one skip and no seek");
        assert_eq!(count(&rows, RowKind::FlushStart) + count(&pictures, RowKind::FlushStart), 0);
        let skip = &rows[nth(&rows, RowKind::Skip, 0)];
        assert_eq!((skip.aux, count(&rows, RowKind::Skip)), (620 * MS, 1));
        assert_eq!(
            after_last(&rows, RowKind::Skip, RowKind::Buffer),
            (16..50).collect::<Vec<u64>>()
        );
        assert!(rows.iter().all(|r| r.kind != RowKind::Before || r.pts < 620 * MS));
        let done = |r: &&Row| r.kind.is(pf_sim::record::class::PICTURE);
        let seqs: Vec<u64> = pictures.iter().filter(done).map(|r| r.seq).collect();
        assert_eq!(seqs, (0..50).collect::<Vec<u64>>(), "nonref {nonref} reorder {reorder}");
        let kept: Vec<&Row> = pictures.iter().filter(|r| r.kind != RowKind::Decoded).collect();
        let kept: Vec<&&Row> = kept.iter().filter(|r| done(r)).collect();
        assert!(!kept.is_empty() && kept.iter().all(|r| r.pts < 620 * MS), "{}", kept.len());
        let p = sc.pictures();
        assert!(kept.iter().all(|r| (r.kind == RowKind::Skipped) != p.is_reference(r.seq)));
        assert_eq!(count(&pictures, RowKind::Skipped) > 0, nonref > 0);
    }
}

#[test]
fn an_accurate_seek_restarts_at_the_keyframe_and_presents_from_the_time() {
    let sc = decoded(BY_THE_RULE)
        .with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 30 }, 620 * MS)]);
    let (worker, rows, pictures, end) = played_decoded(&sc);
    assert_eq!((end, worker.app().seek_gen, worker.app().byte(1)), (End::Finished, 1, 0));
    let flush = &rows[nth(&rows, RowKind::FlushStart, 0)];
    assert_eq!((flush.aux, count(&rows, RowKind::Skip)), (620 * MS, 0));
    assert_eq!((worker.app().jumps().len(), worker.app().taken), (1, 1), "one command");
    assert_eq!(
        after_last(&rows, RowKind::FlushStart, RowKind::Buffer),
        (16..50).collect::<Vec<u64>>()
    );
    assert_eq!(
        after_last(&pictures, RowKind::FlushStart, RowKind::DecodeOnly),
        (0..16).collect::<Vec<u64>>()
    );
    assert_eq!(
        after_last(&pictures, RowKind::FlushStart, RowKind::Decoded),
        (16..50).collect::<Vec<u64>>()
    );
    let key = How::Planned { keyframe: true };
    let sc =
        decoded(key).with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 30 }, 620 * MS)]);
    let (_, rows, pictures, _) = played_decoded(&sc);
    assert_eq!(rows[nth(&rows, RowKind::FlushStart, 0)].aux, 0);
    assert_eq!(
        after_last(&rows, RowKind::FlushStart, RowKind::Buffer),
        (0..50).collect::<Vec<u64>>()
    );
    assert_eq!(count(&pictures, RowKind::DecodeOnly) + count(&rows, RowKind::Skip), 0);
}

#[test]
fn a_target_whose_keyframe_has_not_left_the_source_is_a_seek() {
    let mut sc = decoded(BY_THE_RULE);
    sc.pool_slots = 3;
    // Two pictures in, to 1620 ms. Its keyframe is picture 25.
    let sc = sc.with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 2 }, 1_620 * MS)]);
    let (worker, rows, pictures, end) = played_decoded(&sc);
    assert_eq!((end, worker.app().seek_gen, worker.app().byte(1)), (End::Finished, 1, 25));
    assert_eq!(
        after_last(&rows, RowKind::FlushStart, RowKind::Buffer),
        (41..50).collect::<Vec<u64>>()
    );
    assert_eq!(
        after_last(&pictures, RowKind::FlushStart, RowKind::DecodeOnly),
        (25..41).collect::<Vec<u64>>()
    );
    assert!(pictures.iter().all(|r| !(10..25).contains(&r.seq) || r.kind == RowKind::FlushStart));
}

#[test]
fn the_sweep_catches_a_skip_that_decodes_the_rest_of_the_group_being_presented() {
    use pf_sim::driver::model::App;
    // Sixteen pictures in, to 1100 ms. A pool of eight holds the decoder short of keyframe 25.
    let mut sc = decoded(How::RawSkip);
    sc.pool_slots = 8;
    let sc = sc.with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 16 }, 1_100 * MS)]);
    let (worker, rows, pictures, end) = played_decoded(&sc);
    let between = after_last(&pictures, RowKind::Skip, RowKind::DecodeOnly);
    assert!(between.iter().any(|seq| *seq < 25), "{between:?}");
    let skip = nth(&rows, RowKind::Skip, 0);
    let shown = rows[..skip].iter().rev().find(|r| r.kind == RowKind::Buffer).map(|r| r.pts);
    let from = shown.expect("a picture shown before the skip");
    assert!(from < 1_000 * MS, "the play position is in the first group, {from}");
    let (mut by_the_rule, mut inside) = (App::new(), App::new());
    by_the_rule.skipped_from(0, 1_100 * MS, from);
    inside.skipped_from(0, 1_100 * MS, 1_000 * MS);
    let both = (&rows[..], &pictures[..]);
    let swept = |app: &App| {
        oracle::sweep_decoded(&sc, worker.declared(), app, end, both).err().map(|f| f.kind)
    };
    assert_eq!(swept(worker.app()), None, "an application's own skip is held to nothing");
    assert_eq!(swept(&by_the_rule), Some(Kind::Overdecoded));
    assert_eq!(swept(&inside), None);
}

#[test]
fn a_seek_the_rule_would_not_send_fails_the_decoder_as_declared() {
    let at = Anchor::AfterBuffers { sink: 0, count: 6 };
    let sc = decoded(How::RawSeek { pick: 0 }).with_script(&[seek(at, 620 * MS)]);
    let (worker, rows, pictures, end) = played_decoded(&sc);
    assert_eq!((end, worker.app().byte(1), worker.app().raw), (End::BadSeek, 1, true));
    assert_eq!(rows.last().map(|r| r.kind), Some(RowKind::Stop));
    assert!(rows.iter().all(|r| r.kind != RowKind::Eos), "a failed stream has no end");
    assert!(after_last(&pictures, RowKind::FlushStart, RowKind::Decoded).is_empty());

    let sc = decoded(How::RawSeek { pick: 1 }).with_script(&[seek(at, 620 * MS)]);
    let (_, rows, _, end) = played_decoded(&sc);
    assert_eq!(end, End::Finished);
    assert_eq!(
        after_last(&rows, RowKind::FlushStart, RowKind::Buffer),
        (25..50).collect::<Vec<u64>>()
    );

    let late = Anchor::AfterBuffers { sink: 0, count: 30 };
    let sc = decoded(How::RawSkip).with_script(&[seek(late, 620 * MS)]);
    let (worker, rows, pictures, end) = played_decoded(&sc);
    assert_eq!((end, worker.app().seek_gen, count(&rows, RowKind::Skip)), (End::Finished, 0, 1));
    assert_eq!(after_last(&rows, RowKind::Skip, RowKind::Buffer).first(), Some(&30));
    assert_eq!(count(&pictures, RowKind::Decoded), 50);
}

fn indexed(mapped: Mapped) -> Scenario {
    let mut sc = scenario::plain(5, Topology::Transform, 4 * SEC);
    sc.gop =
        GopDecl { on: true, forced: Some(How::Planned { keyframe: true }), mapped, ..GopDecl::OFF };
    sc.sinks[0].profile.paced = true;
    sc.pool_slots = 4;
    sc
}

fn landed(sc: &Scenario) -> (u64, u64, Vec<u64>) {
    let (worker, rows, _, end) = played_decoded(sc);
    assert_eq!((end, worker.app().seek_gen), (End::Finished, 1));
    let target = rows[nth(&rows, RowKind::FlushStart, 0)].aux;
    (target, worker.app().byte(1), after_last(&rows, RowKind::FlushStart, RowKind::Buffer))
}

#[test]
fn past_the_last_entry_of_an_incomplete_index_a_seek_is_an_estimate() {
    let early = Anchor::AfterBuffers { sink: 0, count: 6 };
    // 2620 ms is picture 65.5, so the estimate is byte 65 and the source restarts at 50.
    let sc = indexed(Mapped::Partial).with_script(&[seek(early, 2_620 * MS)]);
    assert_eq!(landed(&sc), (2_620 * MS, 50, (66..100).collect()));
    let sc = indexed(Mapped::Whole).with_script(&[seek(early, 2_620 * MS)]);
    assert_eq!(landed(&sc), (2_000 * MS, 50, (50..100).collect()), "the keyframe");
    let sc = indexed(Mapped::Partial).with_script(&[seek(early, 620 * MS)]);
    assert_eq!(landed(&sc), (0, 0, (0..100).collect()));
}

#[test]
fn an_index_that_grows_with_reads_is_exact_behind_what_was_read() {
    let early = Anchor::AfterBuffers { sink: 0, count: 6 };
    let sc = indexed(Mapped::Grows).with_script(&[seek(early, 2_620 * MS)]);
    assert_eq!(landed(&sc), (2_620 * MS, 50, (66..100).collect()), "nothing read there yet");
    let read = Anchor::AfterBuffers { sink: 0, count: 60 };
    let sc = indexed(Mapped::Grows).with_script(&[seek(read, 1_620 * MS)]);
    assert_eq!(landed(&sc), (1_000 * MS, 25, (25..100).collect()), "the keyframe it read");
    let sc = indexed(Mapped::Grows).with_script(&[seek(read, 3_620 * MS)]);
    assert_eq!(landed(&sc), (3_620 * MS, 75, (91..100).collect()), "past it, an estimate");
}

#[test]
fn a_seek_before_the_index_exists_is_not_sent_and_the_next_is_exact() {
    let mut sc = indexed(Mapped::Late { after: 20 });
    sc.gop.forced = Some(BY_THE_RULE);
    sc.pool_slots = 3;
    let first = seek(Anchor::AfterBuffers { sink: 0, count: 2 }, 2_620 * MS);
    let second = seek(Anchor::AfterBuffers { sink: 0, count: 30 }, 620 * MS);
    let sc = sc.with_script(&[first, second]);
    let (worker, rows, pictures, end) = played_decoded(&sc);
    assert_eq!(worker.answers()[..2], [Answer::NotDriven, Answer::Taken]);
    assert_eq!((end, worker.app().seek_gen, worker.app().byte(1)), (End::Finished, 1, 0));
    assert_eq!(rows[nth(&rows, RowKind::FlushStart, 0)].aux, 620 * MS);
    assert_eq!(
        after_last(&rows, RowKind::FlushStart, RowKind::Buffer),
        (16..100).collect::<Vec<u64>>()
    );
    assert_eq!(
        after_last(&pictures, RowKind::FlushStart, RowKind::DecodeOnly),
        (0..16).collect::<Vec<u64>>()
    );
}

#[test]
fn the_family_publishes_its_index_whole_late_growing_and_in_part() {
    let mut worker = Worker::new();
    let mut drawn = [0u32; 4];
    for seed in 0..3_000u64 {
        let sc = scenario::draw(seed);
        if !sc.gop.on {
            continue;
        }
        let mode = match sc.gop.mapped {
            Mapped::Whole => 0,
            Mapped::Late { .. } => 1,
            Mapped::Grows => 2,
            Mapped::Partial => 3,
        };
        drawn[mode] += 1;
        if mode > 0 {
            worker.run(&sc).unwrap_or_else(|_| panic!("{}", explain(&sc)));
        }
    }
    assert!(drawn[0] > 150 && drawn[1..].iter().all(|n| *n > 15), "{drawn:?}");
}

fn swept_decoded(sc: &Scenario, change: impl FnOnce(&mut Vec<Row>, &mut Vec<Row>)) -> Option<Kind> {
    let (worker, mut rows, mut pictures, end) = played_decoded(sc);
    change(&mut rows, &mut pictures);
    let both = (&rows[..], &pictures[..]);
    let swept = oracle::sweep_decoded(sc, worker.declared(), worker.app(), end, both);
    swept.err().map(|found| found.kind)
}

#[test]
fn the_sweep_catches_each_way_a_seek_to_a_time_can_go_wrong() {
    let at = Anchor::AfterBuffers { sink: 0, count: 6 };
    let mut skip = decoded(BY_THE_RULE).with_script(&[seek(at, 620 * MS)]);
    skip.pool_slots = 4;
    assert_eq!(swept_decoded(&skip, |_, _| {}), None);
    let shown = |rows: &[Row], n: usize| {
        let from = nth(rows, RowKind::Skip, 0);
        let found =
            rows[from..].iter().enumerate().filter(|(_, r)| r.kind == RowKind::Buffer).nth(n);
        from + found.map(|(i, _)| i).expect("a picture after the skip")
    };
    let of = |pictures: &[Row], seq: u64| {
        pictures.iter().position(|r| r.seq == seq && r.kind != RowKind::Skip).expect("a picture")
    };
    type Both<'a> = (&'a mut Vec<Row>, &'a mut Vec<Row>);
    type Wrong = fn(Both<'_>, usize, usize);
    let cases: [(&str, Kind, Wrong); 9] = [
        ("a picture decoded twice", Kind::DecodedTwice, |(_, p), _, d| p.insert(d, p[d])),
        ("the picture at the time never reaches the sink", Kind::Missing, |(r, _), s, _| {
            r.remove(s);
        }),
        ("a picture from before the time is presented", Kind::BeforePoint, |(r, _), s, _| {
            let before = Row { kind: RowKind::Buffer, ..r[s - 1] };
            r[s - 1] = before;
        }),
        ("a picture at the time is dropped as before it", Kind::BeforePoint, |(r, _), s, _| {
            r[s].kind = RowKind::Before
        }),
        (
            "a picture the decoder never put out reaches the sink",
            Kind::Undecoded,
            |(_, p), _, d| {
                p.remove(d);
            },
        ),
        ("a picture at the time is kept back", Kind::Withheld, |(_, p), _, d| {
            p[d + 1].kind = RowKind::DecodeOnly
        }),
        ("a reference is skipped", Kind::Withheld, |(_, p), _, d| p[d - 1].kind = RowKind::Skipped),
        ("a skip to a time nobody sent", Kind::SkipUnsent, |(r, _), s, _| {
            let told = r.iter().position(|row| row.kind == RowKind::Skip).unwrap_or(s);
            r[told].aux += 1;
        }),
        ("a skip the sink is told twice", Kind::SkipUnsent, |(r, _), s, _| {
            let told = r.iter().position(|row| row.kind == RowKind::Skip).unwrap_or(s);
            r.insert(told, r[told]);
        }),
    ];
    for (what, kind, change) in cases {
        let got = swept_decoded(&skip, |rows, pictures| {
            let (s, d) = (shown(rows, 0), of(pictures, 16));
            change((rows, pictures), s, d)
        });
        assert_eq!(got, Some(kind), "{what}");
    }
    let back = decoded(BY_THE_RULE)
        .with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 30 }, 620 * MS)]);
    let lost = swept_decoded(&back, |_, pictures| {
        pictures.retain(|r| r.kind != RowKind::FlushStart);
    });
    assert_eq!(lost, Some(Kind::Generation));
}

#[test]
fn the_sweep_catches_a_skip_that_decodes_groups_a_seek_would_jump_over() {
    use pf_sim::driver::model::App;
    use pf_sim::media::StreamKind;
    use pf_sim::time::Rational;
    let short = |how| {
        let mut sc = decoded(how);
        let kind = StreamKind::Video { fps: Rational::new(25, 1), keyframe_interval: 5 };
        (sc.source.streams[0].kind, sc.pool_slots) = (kind, 8);
        sc.with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 6 }, 1_620 * MS)])
    };
    let (worker, _, pictures, _) = played_decoded(&short(BY_THE_RULE));
    assert_eq!((worker.app().seek_gen, worker.app().byte(1)), (1, 40));
    assert_eq!(after_last(&pictures, RowKind::FlushStart, RowKind::DecodeOnly), [40]);

    let sc = short(How::RawSkip);
    let (worker, rows, pictures, end) = played_decoded(&sc);
    let between = after_last(&pictures, RowKind::Skip, RowKind::DecodeOnly);
    assert!(between.len() > 20 && between.contains(&25) && between.contains(&39), "{between:?}");
    let mut by_the_rule = App::new();
    by_the_rule.skipped(0, 1_620 * MS, true);
    let swept = |sc: &Scenario, app: &App| {
        let both = (&rows[..], &pictures[..]);
        oracle::sweep_decoded(sc, worker.declared(), app, end, both).err().map(|found| found.kind)
    };
    assert_eq!(swept(&sc, worker.app()), None, "an application's own skip is held to nothing");
    assert_eq!(swept(&sc, &by_the_rule), Some(Kind::Overdecoded));
    assert_eq!(swept(&Scenario { pool_slots: 64, ..sc }, &by_the_rule), None);
}

#[test]
fn the_family_plays_through_a_decoder_and_seeks_by_the_rule() {
    let mut worker = Worker::new();
    let (mut on, mut skips, mut seeks, mut audio, mut held, mut raw) = (0, 0u64, 0u64, 0, 0, 0);
    for seed in 0..3_000u64 {
        let sc = scenario::draw(seed);
        if !sc.gop.on {
            continue;
        }
        let summary = worker.run(&sc).unwrap_or_else(|_| panic!("{}", explain(&sc)));
        on += 1;
        skips += summary.skips as u64;
        seeks += summary.seeks as u64;
        audio += sc.pictures().spans() as u32;
        held += (sc.gop.reorder > 0) as u32;
        raw += worker.app().raw as u32;
        assert!(sc.gop.raw || !worker.app().raw, "seed {seed}: a raw seek nobody declared");
        assert!(summary.end != End::BadSeek || worker.app().raw, "seed {seed}");
    }
    assert!((250..600).contains(&on), "{on} seeds have a decoder");
    assert!(skips > 100 && seeks > 300, "{skips} skips and {seeks} seeks");
    assert!(audio > 50 && held > 100 && raw > 20, "{audio} audio, {held} hold back, {raw} raw");
}

use pf_sim::driver::map::{Map, Piece, Rate};
use pf_sim::driver::model::Answer;
use pf_sim::driver::oracle::Pacing;
use pf_sim::driver::retime::RetimeDecl;
use pf_sim::driver::scenario::{TimeDecl, Told};

fn paced(topology: Topology, duration: u64) -> Scenario {
    let mut sc = scenario::plain(5, topology, duration);
    for sink in sc.sinks.iter_mut() {
        sink.profile.paced = true;
    }
    sc.time.on = true;
    sc
}

fn at(anchor: Anchor, kind: ActionKind) -> Action {
    Action { anchor, kind }
}

fn rate(num: u16, den: u16) -> ActionKind {
    ActionKind::SetRate { num, den }
}

fn shown(rows: &[Row], sink: u16) -> Vec<(u64, u64)> {
    let on = |r: &&Row| r.sink == sink && r.kind == RowKind::Buffer;
    rows.iter().filter(on).map(|r| (r.pts, r.at)).collect()
}

#[test]
fn a_rate_with_no_retimer_starts_at_what_is_presented_and_both_sinks_follow_it() {
    let change = 400 * MS;
    let sc =
        paced(Topology::TwoStreams, 2 * SEC).with_script(&[at(Anchor::At(change), rate(2, 1))]);
    let (worker, rows, end) = played(&sc);
    assert_eq!((end, worker.app().rates), (End::Finished, 1));
    let want = Piece { stream_start: change, running_start: change, rate: Rate { num: 2, den: 1 } };
    assert_eq!(worker.app().map.pieces().last(), Some(&want));
    for sink in 0..2 {
        let rows = shown(&rows, sink);
        assert_eq!(rows.len(), if sink == 0 { 50 } else { 100 });
        for (pts, taken) in rows {
            let due = if pts < change { pts } else { change + (pts - change) / 2 };
            assert_eq!(taken, due, "sink {sink}, the buffer stamped {pts}");
        }
    }
    let last = rows.iter().rfind(|r| r.kind == RowKind::Buffer).map(|r| r.at);
    assert!(last < Some(1_200 * MS), "{last:?}");
}

fn retimed(lag: u8, latency: u64) -> Scenario {
    let mut sc = paced(Topology::Transform, 2 * SEC);
    let decl = RetimeDecl { on: true, lag, latency, seed: 0 };
    let waits = |seed: &u64| {
        let mut r = pf_sim::driver::retime::Retiming::new(RetimeDecl { seed: *seed, ..decl });
        r.take();
        r.ask(Rate { num: 2, den: 1 });
        (0..lag).all(|_| r.take().rate == Rate::ONE)
    };
    let seed = (0..500u64).find(waits).expect("some seed draws the whole lag");
    sc.time.retimer = RetimeDecl { seed, ..decl };
    sc
}

#[test]
fn a_retimer_switches_some_buffers_after_it_is_asked_and_the_map_changes_there() {
    let latency = 3 * MS;
    let asked = Anchor::AfterBuffers { sink: 0, count: 5 };
    let sc = retimed(4, latency).with_script(&[at(asked, rate(1, 2))]);
    let (worker, rows, end) = played(&sc);
    let inner = worker.decoded();
    assert_eq!((end, worker.app().rates), (End::Finished, 1));
    let told: Vec<&Row> = inner.iter().filter(|r| r.kind == RowKind::Retime).collect();
    let half = Rate { num: 1, den: 2 };
    assert_eq!(told.len(), 2);
    assert_eq!((told[0].pts, told[0].aux, told[1].aux), (0, Rate::ONE.pack(), half.pack()));
    let switch = told[1].pts;
    let asked_at = inner.iter().position(|r| r.kind == RowKind::RateAsked).expect("it was asked");
    let taken = |r: &&Row| r.kind == RowKind::Retimed;
    let before = inner[..asked_at].iter().filter(taken).count() as u64;
    assert_eq!(switch, (before + 4) * 40 * MS, "four buffers on from where it was asked");
    assert!(inner.iter().filter(taken).all(|r| r.format == if r.pts < switch { 1 } else { 2 }));
    let want = Piece { stream_start: switch, running_start: switch, rate: half };
    assert_eq!(
        worker.app().map.pieces(),
        [Piece { stream_start: 0, running_start: 0, rate: Rate::ONE }, want]
    );
    let shown = shown(&rows, 0);
    assert_eq!(shown.len() as u64, 50 + (2 * SEC - switch) / (40 * MS));
    for (pts, taken) in shown {
        let running = if pts < switch { pts } else { switch + (pts - switch) * 2 };
        assert_eq!(taken, latency + running, "the buffer stamped {pts}");
    }
}

#[test]
fn a_seek_a_skip_and_a_pause_land_in_the_middle_of_a_pending_change() {
    let asked = Anchor::AfterBuffers { sink: 0, count: 5 };
    let twice = Rate { num: 2, den: 1 };
    let seek = ActionKind::Seek { to: 800 * MS };
    let skip = ActionKind::SkipTo { to: 1_200 * MS };
    for kind in [seek, skip, ActionKind::Pause] {
        let sc = retimed(5, 0).with_script(&[at(asked, rate(2, 1)), at(asked, kind)]);
        let (worker, rows, end) = played(&sc);
        assert_eq!((end, worker.app().rates), (End::Finished, 1), "{kind:?}");
        let map = worker.app().map;
        assert_eq!(map.newest().rate, twice, "{kind:?}");
        let told: Vec<&Row> =
            worker.decoded().iter().filter(|r| r.kind == RowKind::Retime).collect();
        assert_eq!(told.last().map(|r| r.aux), Some(twice.pack()), "{kind:?}");
        match kind {
            ActionKind::Seek { to } => {
                let restart = Piece { stream_start: to, running_start: to, rate: twice };
                assert_eq!(map.pieces(), [restart]);
                assert_eq!(told.last().map(|r| (r.seek_gen, r.pts)), Some((1, 0)));
            }
            ActionKind::SkipTo { to } => {
                assert!(map.newest().stream_start >= to, "{:?}", map.pieces());
                assert_eq!(count(&rows, RowKind::FlushStart), 0);
            }
            _ => assert_eq!(count(&rows, RowKind::Paused), 1),
        }
    }
}

#[test]
fn an_offset_moves_its_sink_and_no_other() {
    let (change, by) = (200 * MS, 30 * MS as i64);
    let shift = |sink, by| ActionKind::SetOffset { sink, by };
    let script = [at(Anchor::At(change), shift(1, by)), at(Anchor::At(change), shift(2, by))];
    let sc = paced(Topology::TwoStreams, SEC).with_script(&script);
    let (worker, rows, end) = played(&sc);
    assert_eq!(end, End::Finished);
    assert_eq!(worker.answers()[..2], [Answer::Taken, Answer::NotASink]);
    let told: Vec<(u16, u64)> =
        rows.iter().filter(|r| r.kind == RowKind::Offset).map(|r| (r.sink, r.aux)).collect();
    assert_eq!(told, [(1, by as u64)]);
    assert!(shown(&rows, 0).iter().all(|(pts, taken)| taken == pts), "the picture stays");
    for (pts, taken) in shown(&rows, 1) {
        let due = if pts < change { pts } else { pts + by as u64 };
        assert_eq!(taken, due, "the sound stamped {pts}");
    }
    let early = [at(Anchor::At(change), shift(0, -(100 * MS as i64)))];
    let (_, rows, _) = played(&paced(Topology::TwoStreams, SEC).with_script(&early));
    for (pts, taken) in shown(&rows, 0) {
        let due = if pts < change { pts } else { (pts - 100 * MS).max(change) };
        assert_eq!(taken, due, "the picture stamped {pts}");
    }
    assert!(shown(&rows, 1).iter().all(|(pts, taken)| taken == pts), "the sound stays");
}

#[test]
fn two_sources_seek_to_their_own_bytes_and_present_one_time_together() {
    let to = 1_234 * MS;
    let first = |rows: &[Row], sink: u16| {
        let after = |r: &&Row| r.sink == sink && r.kind == RowKind::Buffer && r.seek_gen == 1;
        rows.iter().find(after).map(|r| r.seq)
    };
    for (told, bytes, starts) in [
        (Told::Each, [25, 61], [Some(25), Some(61)]),
        (Told::First, [25, 0], [Some(25), Some(0)]),
        (Told::Second, [0, 61], [Some(0), Some(61)]),
        (Told::All, [25, 25], [Some(25), Some(25)]),
        (Told::Planned, [25, 61], [Some(25), Some(61)]),
    ] {
        let mut sc = paced(Topology::TwoStreams, 2 * SEC);
        sc.time = TimeDecl { on: true, two_sources: true, forced: Some(told), ..TimeDecl::OFF };
        sc.pool_slots = 256;
        let sc = sc.with_script(&[seek(Anchor::AfterBuffers { sink: 0, count: 5 }, to)]);
        let (worker, rows, end) = played(&sc);
        let app = worker.app();
        assert_eq!((end, app.seek_gen), (End::Finished, 1), "{told:?}");
        assert_eq!([app.byte_of(1, 0), app.byte_of(1, 1)], bytes, "{told:?}");
        assert_eq!([first(&rows, 0), first(&rows, 1)], starts, "{told:?}");
        let flushed = rows[nth(&rows, RowKind::FlushStart, 0)].at;
        for sink in 0..2 {
            let after = |r: &&Row| r.sink == sink && r.kind == RowKind::Buffer && r.seek_gen == 1;
            for row in rows.iter().filter(after) {
                let due = flushed + row.pts.saturating_sub(to);
                assert_eq!(row.at, due, "{told:?} sink {sink}, the buffer stamped {}", row.pts);
            }
        }
    }
}

#[test]
fn a_buffer_off_the_map_is_caught_early_late_and_misjudged() {
    let mut map = Map::new(SEC, false);
    map.set_rate(SEC + 100 * MS, Rate { num: 2, den: 1 });
    let profile = SinkProfile { paced: true, lateness_bound: 5 * MS, ..SinkProfile::IDEAL };
    let sink = |offset, waited| Pacing { profile, latency: 3 * MS, offset, waited };
    // Stamped 300 ms. That is 200 ms of running time, 203 behind the latency.
    let taken = |at: u64| Row { pts: 300 * MS, aux: at, ..Row::new(RowKind::Buffer, 0, 0) };
    let held = |pacing: Pacing, at: u64| oracle::on_the_map(&map, &pacing, &taken(at), SEC + at);
    let off = |want: u64, got: u64| Err((Kind::OffTimeline, SEC + want, SEC + got));
    assert_eq!(held(sink(0, true), 203 * MS), Ok(()));
    assert_eq!(held(sink(0, false), 203 * MS - 1), off(203 * MS, 203 * MS - 1), "early");
    assert_eq!(
        held(sink(0, true), 203 * MS + 1),
        off(203 * MS, 203 * MS + 1),
        "late though it waited"
    );
    assert_eq!(held(sink(0, false), 203 * MS + 1), Ok(()), "one that arrived late is late");
    assert_eq!(held(sink(-(MS as i64), true), 202 * MS), Ok(()), "its own offset moves it");
    assert_eq!(held(sink(MS as i64, true), 203 * MS), off(204 * MS, 203 * MS));
    let mut moved = map;
    moved.touch(SEC + 204 * MS);
    assert_eq!(
        oracle::on_the_map(&moved, &sink(0, true), &taken(204 * MS), SEC + 204 * MS),
        Ok(())
    );
    let past = Err((Kind::Paced, 5 * MS, 7 * MS));
    assert_eq!(held(sink(0, false), 210 * MS), past, "kept past its bound");
    let drop = |late: u64| Row { kind: RowKind::Drop, aux: late, ..taken(0) };
    let dropped =
        |late: u64, at: u64| oracle::on_the_map(&map, &sink(0, false), &drop(late), SEC + at);
    assert_eq!(dropped(7 * MS, 210 * MS), Ok(()));
    assert_eq!(dropped(6 * MS, 210 * MS), Err((Kind::Paced, 7 * MS, 6 * MS)));
    assert_eq!(
        dropped(4 * MS, 207 * MS),
        Err((Kind::Paced, 4 * MS, 4 * MS)),
        "dropped inside its bound"
    );
    let wrong = Row { aux: 9, ..taken(0) };
    let unpaced = Pacing { profile: SinkProfile::IDEAL, ..sink(0, false) };
    assert_eq!(
        oracle::on_the_map(&map, &unpaced, &wrong, SEC + 50),
        Err((Kind::OffTimeline, 50, 9))
    );
}

#[test]
fn the_sweep_catches_each_way_a_retimed_run_and_an_offset_can_go_wrong() {
    let asked = Anchor::AfterBuffers { sink: 0, count: 5 };
    let sc = retimed(3, 0)
        .with_script(&[at(asked, rate(1, 2)), at(asked, ActionKind::SetOffset { sink: 0, by: 7 })]);
    assert_eq!(swept_decoded(&sc, |_, _| {}), None);
    let of = |rows: &[Row], kind: RowKind, n: usize| {
        let found = rows.iter().enumerate().filter(|(_, r)| r.kind == kind).nth(n);
        found.map(|(i, _)| i).unwrap_or_else(|| panic!("no {kind:?} number {n}"))
    };
    type Both<'a> = (&'a mut Vec<Row>, &'a mut Vec<Row>);
    type Wrong = fn(Both<'_>, &dyn Fn(&[Row], RowKind, usize) -> usize);
    let cases: [(&str, Kind, Wrong); 11] = [
        ("a buffer the retimer put out never arrives", Kind::Missing, |(r, _), of| {
            r.remove(of(r, RowKind::Buffer, 30));
        }),
        ("a buffer arrives twice", Kind::Missing, |(r, _), of| {
            let at = of(r, RowKind::Buffer, 30);
            r.insert(at, r[at]);
        }),
        ("a copy with the stamp of the buffer it copies", Kind::Missing, |(r, _), of| {
            let at = of(r, RowKind::Buffer, 60);
            r[at].pts = r[at - 1].pts;
        }),
        ("a buffer nothing was retimed from", Kind::Unretimed, |(r, _), of| {
            let at = of(r, RowKind::Eos, 0);
            r.insert(at, r[at - 1]);
        }),
        ("the retimer skips a buffer of its stream", Kind::Missing, |(_, p), of| {
            p.remove(of(p, RowKind::Retimed, 9));
        }),
        ("it plays another rate than its seed says", Kind::Retimed, |(_, p), of| {
            let at = of(p, RowKind::Retimed, 2);
            p[at].aux = Rate { num: 1, den: 2 }.pack();
        }),
        ("it puts out another number of buffers", Kind::Content, |(_, p), of| {
            let at = of(p, RowKind::Retimed, 2);
            p[at].format = 3;
        }),
        ("it switches and tells the timeline nothing", Kind::Retimed, |(_, p), of| {
            p.remove(of(p, RowKind::Retime, 1));
        }),
        ("it tells the timeline of a switch it did not make", Kind::Retimed, |(_, p), of| {
            let at = of(p, RowKind::Retime, 1);
            p.insert(at - 3, p[at]);
        }),
        ("it is told a rate nobody asked for", Kind::RateUnsent, |(_, p), of| {
            let at = of(p, RowKind::RateAsked, 0);
            p[at].aux = Rate { num: 3, den: 2 }.pack();
        }),
        ("a sink is told an offset nobody sent it", Kind::OffsetUnsent, |(r, _), of| {
            let at = of(r, RowKind::Offset, 0);
            r[at].aux += 1;
        }),
    ];
    for (what, kind, change) in cases {
        let got = swept_decoded(&sc, |rows, inner| change((rows, inner), &of));
        assert_eq!(got, Some(kind), "{what}");
    }
    let twice = swept_decoded(&sc, |rows, _| {
        let at = of(rows, RowKind::Offset, 0);
        rows.insert(at, rows[at]);
    });
    assert_eq!(twice, Some(Kind::OffsetUnsent));
}

#[test]
fn the_family_changes_rates_shifts_sinks_and_retimes() {
    let mut worker = Worker::new();
    let (mut timed, mut retimed, mut two, mut plain) = (0, 0, 0, 0);
    let (mut rates, mut offsets, mut retimes, mut skips, mut refused) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut pieces, mut told) = (0u64, 0u64);
    for seed in 0..6_000u64 {
        let sc = scenario::draw(seed);
        if !sc.time.on {
            continue;
        }
        let summary = worker.run(&sc).unwrap_or_else(|_| panic!("{}", explain(&sc)));
        timed += 1;
        retimed += sc.time.retimer.on as u32;
        two += sc.time.two_sources as u32;
        plain += (!sc.time.retimer.on && summary.rates > 0) as u32;
        rates += summary.rates as u64;
        offsets += summary.offsets as u64;
        retimes += summary.retimes as u64;
        skips += summary.skips as u64;
        pieces += (worker.app().map.pieces().len() > 1) as u64;
        let unplayed = |r: &&Row| {
            r.kind == RowKind::RateAsked
                && !Rate::unpack(r.aux).is_some_and(pf_sim::driver::retime::plays)
        };
        refused += worker.decoded().iter().filter(unplayed).count() as u64;
        told += (sc.time.two_sources && summary.seeks > 0) as u64;
        assert!(sc.time.retimer.on || summary.retimes == 0, "seed {seed}");
    }
    assert!((1_500..2_100).contains(&timed), "three seeds in ten, {timed}");
    assert!(
        retimed > 250 && two > 60 && plain > 300,
        "{retimed} retimed, {two} two, {plain} plain"
    );
    assert!(rates > 1_500 && offsets > 350 && skips > 250, "{rates} {offsets} {skips}");
    assert!(retimes > 400 && refused > 20 && pieces > 300, "{retimes} {refused} {pieces}");
    assert!(told > 40, "{told} runs sought with two sources");
}

/// `PF_SIM_SEEDS=100000 cargo test --release -p pf-sim --test driver -- --ignored tallied
/// --nocapture`
#[test]
#[ignore = "prints a tally of verdicts, run it with --ignored"]
fn the_verdicts_of_a_sweep_tallied_by_kind() {
    let count = var("PF_SIM_SEEDS").unwrap_or(100_000);
    let mut tally: Vec<(Kind, u64, u64)> = Vec::new();
    let (mut failed, mut worker) = (0u64, Worker::new());
    for seed in 0..count {
        let Err(verdict) = worker.run(&scenario::draw(seed)) else {
            continue;
        };
        failed += 1;
        match tally.iter_mut().find(|(known, ..)| *known == verdict.kind) {
            Some((_, seeds, _)) => *seeds += 1,
            None => tally.push((verdict.kind, 1, seed)),
        }
    }
    println!("TEETH {failed} of {count} seeds failed");
    for (kind, seeds, first) in tally {
        println!("TEETH {kind:?} {seeds} seeds, the first {first}");
    }
}

fn broken(seed: u64, breakage: Break, slip: Slip) -> Scenario {
    let mut sc = scenario::draw(seed);
    (sc.net.breakage, sc.net.slip) = (breakage, slip);
    sc
}

#[test]
fn the_family_serves_over_a_network_in_one_seed_in_eight() {
    let count = var("PF_SIM_SEEDS").unwrap_or(DEFAULT_SEEDS).min(40_000);
    let nets: Vec<NetDecl> =
        (0..count).map(|seed| scenario::draw(seed).net).filter(|n| n.on).collect();
    let share = nets.len() as f64 / count as f64;
    assert!((0.08..0.16).contains(&share), "{} of {count} seeds have a network", nets.len());
    let some = |what: fn(&NetDecl) -> bool| nets.iter().any(what);
    assert!(some(|n| n.faults.resets > 0) && some(|n| n.faults.refuse > 0), "resets and refusals");
    assert!(
        some(|n| n.faults.stalls > 0 && n.faults.stall_for == NONE),
        "a peer that stalls for good"
    );
    assert!(
        some(|n| n.faults.time_out > 0) && some(|n| n.faults.dns_fail > 0),
        "timeouts and lookups"
    );
    assert!(
        some(|n| n.faults.split)
            && some(|n| n.faults.short_writes)
            && some(|n| n.faults.latency.1 > 0)
    );
    assert!(
        some(|n| n.faults == pf_sim::driver::net::Faults { ..n.faults })
            && nets.iter().all(|n| n.breakage == Break::None && n.slip == Slip::None)
    );
    fn only_net(seed: u64) -> Scenario {
        let sc = scenario::draw(seed);
        if sc.net.on { sc } else { scenario::plain(seed, Topology::Line, 40 * MS) }
    }
    let report = farm::sweep(0..count, workers(), only_net);
    assert_clean(&report, only_net);
}

/// Each deliberate defect is caught within 100,000 seeds by its oracle.
#[test]
fn the_network_oracles_have_teeth() {
    type Family = fn(u64) -> Scenario;
    let cases: [(&str, Family, Kind); 6] = [
        (
            "a receive gives its bytes twice",
            |s| broken(s, Break::Duplicate, Slip::None),
            Kind::NetBytes,
        ),
        ("a receive drops a byte", |s| broken(s, Break::Skip, Slip::None), Kind::NetBytes),
        (
            "a cancelled receive completes",
            |s| broken(s, Break::CancelCompletes, Slip::None),
            Kind::NetStale,
        ),
        (
            "the source resumes a byte late",
            |s| broken(s, Break::None, Slip::ResumeShort),
            Kind::NetBytes,
        ),
        (
            "the source cancels nothing at a flush",
            |s| broken(s, Break::None, Slip::NoCancel),
            Kind::NetStale,
        ),
        ("the source has no watchdog", |s| broken(s, Break::None, Slip::NoWatchdog), Kind::Stall),
    ];
    for (what, family, want) in cases {
        let mut worker = Worker::new();
        let found = (0..100_000u64).find_map(|seed| worker.run(&family(seed)).err());
        let Some(verdict) = found else { panic!("{what}: no seed in 100,000 caught it") };
        println!("TEETH {what}: seed {} fails {:?}", verdict.seed, verdict.kind);
        assert_eq!(
            verdict.kind,
            want,
            "{what}: seed {}\n{}",
            verdict.seed,
            explain(&family(verdict.seed))
        );
    }
}

/// The seeds the tests above name, each pinned for what it once found.
const PINNED: [u64; 17] =
    [1297, 3, 21, 36, 8741, 46, 9, 51, 278, 50, 0, 14, 495, 1903, 450162, 565101, 784341];

/// `cargo test --release -p pf-sim --test driver -- --ignored digests --nocapture`
#[test]
#[ignore = "prints digests to compare across a change, run it with --ignored"]
fn the_digests_of_the_families() {
    let count = var("PF_SIM_SEEDS").unwrap_or(20_000);
    fn unlimited(seed: u64) -> Scenario {
        let mut sc = scenario::draw(seed);
        sc.sinks.iter_mut().for_each(|sink| sink.limit = 0);
        sc
    }
    type Family = (&'static str, fn(u64) -> Scenario);
    let families: [Family; 7] = [
        ("draw", scenario::draw),
        ("draw with no link limited", unlimited),
        ("draw_offline", scenario::draw_offline),
        ("draw_untimed", scenario::draw_untimed),
        ("draw_undecoded", scenario::draw_undecoded),
        ("draw_open", scenario::draw_open),
        ("reference", scenario::reference),
    ];
    let mut worker = Worker::new();
    let (mut limited, mut moved) = (0, 0);
    for seed in 0..count.min(5_000) {
        let sc = scenario::draw(seed);
        if sc.sinks[..sc.sink_count()].iter().any(|sink| sink.limit > 0) {
            limited += 1;
            let sums = [worker.run(&sc), worker.run(&unlimited(seed))]
                .map(|run| run.map(|summary| summary.checksum).ok());
            moved += (sums[0] != sums[1]) as u32;
        }
    }
    let pinned = PINNED
        .iter()
        .filter(|seed| scenario::draw(**seed).sinks.iter().any(|sink| sink.limit > 0))
        .count();
    println!(
        "DIGEST of {} seeds {limited} draw a limit, which moves the digest of {moved}. Of the pinned seeds {pinned} draw one",
        count.min(5_000)
    );
    for (name, family) in families {
        let report = farm::sweep(0..count, workers(), family);
        println!("DIGEST {name} {count} seeds {:#018x} failed {}", report.checksum, report.failed);
        for seed in PINNED {
            let sum = worker.run(&family(seed)).map(|summary| summary.checksum);
            println!("DIGEST {name} seed {seed} {sum:x?}");
        }
    }
}

/// `cargo test --release -p pf-sim --test driver -- --ignored speeds --nocapture`
#[test]
#[ignore = "a release-build measurement, run it with --ignored"]
fn the_speeds_of_the_family_with_and_without_a_network() {
    let count = var("PF_SIM_SEEDS").unwrap_or(100_000);
    for (name, family) in
        [("draw_offline", scenario::draw_offline as fn(u64) -> Scenario), ("draw", scenario::draw)]
    {
        let report = farm::sweep(0..count, 1, family);
        let (rate, steps) = (report.runs_per_cpu_second(), report.steps / report.runs.max(1));
        println!(
            "SPEED {name} {count} seeds {rate:.0} runs per second per core, {steps} steps a run, {} failed",
            report.failed
        );
    }
}

/// `cargo test --release -p pf-sim --test driver -- --ignored budget --nocapture`
#[test]
#[ignore = "a release-build benchmark, run it with --ignored"]
fn the_reference_scenario_meets_the_speed_budget() {
    let count = var("PF_SIM_SEEDS").unwrap_or(2_000);
    // Thread time on one worker, so other load costs little.
    let warm = farm::sweep(0..count / 10 + 1, 1, scenario::reference);
    assert_eq!(warm.failed, 0, "{warm}");
    let report = farm::sweep(0..count, 1, scenario::reference);
    println!("{report}");
    let split = farm::sweep_timed(0..count.min(500), 1, scenario::reference);
    println!("with the phases timed\n{split}");
    assert_eq!(report.failed, 0);
    let rate = report.runs_per_cpu_second();
    assert!(rate >= BUDGET, "{rate:.0} runs per second per core, the budget is {BUDGET}");
}

#[test]
fn every_seed_of_the_buffering_family_passes_every_oracle() {
    let report = farm::sweep(seeds(), workers(), scenario::draw_buffering);
    assert_clean(&report, scenario::draw_buffering);
}

/// Video and audio, twelve seconds each, stream `ahead` served `skew` ahead in the file.
fn far_apart(ahead: usize, skew: u64) -> Scenario {
    let mut sc = scenario::plain(7, Topology::TwoStreams, 12 * SEC);
    sc.source.skew[ahead] = skew;
    sc.pool_slots = 1024;
    for sink in sc.sinks.iter_mut() {
        sink.profile = SinkProfile { paced: true, lateness_bound: 40 * MS, ..SinkProfile::IDEAL };
        (sink.limit, sink.queue) = (100 * MS, 255);
    }
    sc
}

fn late(rows: &[Row], sink: u16) -> usize {
    rows.iter().filter(|r| r.sink == sink && r.kind == RowKind::Drop).count()
}

#[test]
fn a_far_apart_file_plays_both_tracks_from_the_start_under_the_cap() {
    for (ahead, skew) in [(0, 3 * SEC), (0, 4 * SEC), (1, 3 * SEC), (1, 4 * SEC)] {
        let (_, rows, end) = played(&far_apart(ahead, skew));
        assert_eq!(end, End::Finished, "stream {ahead} {skew} ahead");
        assert_eq!(
            (late(&rows, 0), late(&rows, 1)),
            (0, 0),
            "stream {ahead} {skew} ahead, nothing dropped as late"
        );
    }
}

#[test]
fn a_far_apart_file_past_the_cap_plays_to_its_end_late() {
    for ahead in [0, 1] {
        let (_, rows, end) = played(&far_apart(ahead, 8 * SEC));
        assert_eq!(end, End::Finished);
        let behind = 1 - ahead as u16;
        assert!(late(&rows, behind) > 0, "stream {behind} behind the cap comes late");
        assert_eq!(late(&rows, ahead as u16), 0);
    }
}

#[test]
fn a_far_apart_file_whose_buffers_swing_ten_to_one_plays_from_the_start() {
    let mut sc = far_apart(0, 3 * SEC);
    sc.source.swing = scenario::MAX_SWING;
    let (_, rows, end) = played(&sc);
    assert_eq!(end, End::Finished);
    assert_eq!((late(&rows, 0), late(&rows, 1)), (0, 0));
}

#[test]
fn a_far_apart_file_over_a_network_that_stalls_past_what_is_held_plays_to_its_end() {
    let mut sc = far_apart(0, 3 * SEC);
    sc.net = NetDecl {
        on: true,
        faults: pf_sim::driver::net::Faults {
            stalls: 1,
            stall_within: 2_000,
            stall_for: 2 * SEC,
            ..pf_sim::driver::net::Faults::NONE
        },
        key: 11,
        patience: 10 * SEC,
        backoff: 50 * MS,
        breakage: Break::None,
        slip: Slip::None,
    };
    sc.pool_slots += 2;
    let (_, _, end) = played(&sc);
    assert_eq!(end, End::Finished);
}

/// A minute of video and three sparse cues, muxed in place or all `first` as in an mp4.
fn subtitled(first: bool) -> Scenario {
    let mut sc = scenario::plain(9, Topology::TwoStreams, 60 * SEC);
    let cues = [0, 20, 40].map(|s| pf_sim::media::Cue {
        start: s * SEC + SEC,
        end: s * SEC + 3 * SEC,
        size: 32,
    });
    sc.source.cues[..3].copy_from_slice(&cues);
    sc.source.cue_count = 3;
    sc.source.streams[1].kind =
        pf_sim::media::StreamKind::Text { cues: pf_sim::media::CueRange { first: 0, len: 3 } };
    sc.source.described = true;
    sc.source.skew[1] = if first { 60 * SEC } else { 0 };
    sc.pool_slots = 64;
    for (n, sink) in sc.sinks.iter_mut().enumerate() {
        sink.profile = pf_sim::profile::device(9 ^ n as u64);
        sink.limit = 200 * MS;
    }
    sc
}

#[test]
fn a_sparse_subtitle_track_with_cues_far_apart_never_holds_the_video_back() {
    for first in [false, true] {
        let (_, rows, end) = played(&subtitled(first));
        assert_eq!(end, End::Finished);
        assert_eq!(late(&rows, 0), 0, "the video plays on time, cues first {first}");
        let gaps = rows.iter().filter(|r| r.sink == 1 && r.kind == RowKind::Gap).count();
        let cues = rows.iter().filter(|r| r.sink == 1 && r.kind == RowKind::Buffer).count();
        assert_eq!(cues, 3);
        assert!(first || gaps >= 70, "{gaps} gaps");
    }
}
