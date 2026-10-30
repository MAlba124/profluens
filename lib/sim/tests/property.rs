//! Ideal pipeline recordings must pass the sweep, and each corruption must fire its rule.

// Corruptions share one signature for their table, though only some resize the log.
#![allow(clippy::ptr_arg)]

use pf_sim::action::{self, ActionList, GenParams};
use pf_sim::ideal;
use pf_sim::media::{Cue, Fault, MediaSpec, Pacing, Recovery, StreamKind, StreamSpec};
use pf_sim::prng::Prng;
use pf_sim::record::{NO_STREAM, Recording, Row, RowKind, checksum, class, flag};
use pf_sim::sweep::{self, Expect, Rule};
use pf_sim::time::{MS, NONE, Rational, SEC, US};

const SEEDS: u64 = 2_000;

const RESET: u32 = class::FLUSH | class::bit(RowKind::Start);

fn draw_pacing(rng: &mut Prng) -> Pacing {
    match rng.below(3) {
        0 => Pacing::AsFastAsPossible,
        1 => Pacing::RealTime,
        _ => Pacing::Jitter { base: rng.below(20 * MS), bound: rng.below(30 * MS) },
    }
}

fn draw_fault(rng: &mut Prng) -> Fault {
    let buffer = rng.below(20);
    match rng.below(5) {
        0 => Fault::StallAt { buffer, hold: rng.range(MS, 300 * MS) },
        1 => Fault::ErrorAt { buffer },
        2 => Fault::EndEarlyAt { buffer },
        3 => Fault::LatePad { delay: rng.range(MS, 300 * MS) },
        _ => Fault::BufferingDip { buffer, recovery: Recovery::After(50 * MS) },
    }
}

fn draw_spec(rng: &mut Prng, spec: &mut MediaSpec) {
    const RATES: [Rational; 4] = [
        Rational::new(25, 1),
        Rational::new(30_000, 1_001),
        Rational::new(60, 1),
        Rational::new(24_000, 1_001),
    ];
    const PACKETS: [u64; 4] = [5 * MS, 10 * MS, 20 * MS, 21_333_333];
    spec.reset();
    for _ in 0..rng.range(1, 4) {
        let duration = rng.range(200 * MS, 2 * SEC);
        let pacing = draw_pacing(rng);
        let stream = match rng.below(3) {
            0 => {
                let fps = RATES[rng.below(4) as usize];
                let kind = StreamKind::Video { fps, keyframe_interval: rng.below(31) as u32 };
                spec.add_stream(StreamSpec { kind, duration, size: 4096, pacing }).unwrap()
            }
            1 => {
                let kind = StreamKind::Audio { packet: PACKETS[rng.below(4) as usize] };
                spec.add_stream(StreamSpec { kind, duration, size: 256, pacing }).unwrap()
            }
            _ => {
                let mut cues = [Cue { start: 0, end: 0, size: 0 }; 5];
                let count = rng.range(1, 6) as usize;
                let mut start = 0;
                for cue in &mut cues[..count] {
                    start += rng.below(400 * MS);
                    *cue = Cue {
                        start,
                        end: start + rng.range(MS, 500 * MS),
                        size: rng.range(1, 80) as u32,
                    };
                }
                spec.add_text(&cues[..count], pacing).unwrap()
            }
        };
        if rng.chance(30) {
            spec.add_fault(stream, draw_fault(rng)).unwrap();
        }
    }
}

fn prev_on_sink(rows: &[Row], mask: u32) -> Vec<Option<usize>> {
    let mut last = [None; 16];
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        out.push(last[row.sink as usize]);
        if row.kind.is(mask) {
            last[row.sink as usize] = Some(i);
        }
    }
    out
}

fn next_on_sink(rows: &[Row], mask: u32) -> Vec<Option<usize>> {
    let mut next = [None; 16];
    let mut out = vec![None; rows.len()];
    for (i, row) in rows.iter().enumerate().rev() {
        out[i] = next[row.sink as usize];
        if row.kind.is(mask) {
            next[row.sink as usize] = Some(i);
        }
    }
    out
}

fn is(rows: &[Row], index: Option<usize>, mask: u32) -> bool {
    index.is_some_and(|i| rows[i].kind.is(mask))
}

fn choose(rng: &mut Prng, len: usize, keep: impl Fn(usize) -> bool) -> Option<usize> {
    let candidates: Vec<usize> = (0..len).filter(|i| keep(*i)).collect();
    (!candidates.is_empty()).then(|| candidates[rng.below(candidates.len() as u64) as usize])
}

const BUFFER: u32 = class::bit(RowKind::Buffer);

fn drop_format(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let next = next_on_sink(rows, class::TIMED);
    let Some(i) = choose(rng, rows.len(), |i| rows[i].kind == RowKind::Format && next[i].is_some())
    else {
        return false;
    };
    rows.remove(i);
    true
}

fn drop_segment(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let next = next_on_sink(rows, class::TIMED | class::bit(RowKind::Segment) | RESET);
    let wanted = |i: usize| rows[i].kind == RowKind::Segment && is(rows, next[i], class::TIMED);
    let Some(i) = choose(rng, rows.len(), wanted) else {
        return false;
    };
    rows.remove(i);
    true
}

fn time_back(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let prev = prev_on_sink(rows, BUFFER | class::bit(RowKind::Segment) | RESET);
    let wanted = |i: usize| {
        rows[i].kind == RowKind::Buffer
            && prev[i].is_some_and(|p| rows[p].kind == RowKind::Buffer && rows[p].pts >= 1)
    };
    let Some(i) = choose(rng, rows.len(), wanted) else {
        return false;
    };
    let earlier = rows[prev[i].unwrap()].pts - 1;
    rows[i].pts = earlier;
    rows[i].dts = earlier;
    true
}

fn after_eos(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let Some(i) = choose(rng, rows.len(), |i| rows[i].kind == RowKind::Eos) else {
        return false;
    };
    let late = Row { kind: RowKind::Buffer, seq: 1 << 40, ..rows[i] };
    rows.insert(i + 1, late);
    true
}

fn double_eos(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let Some(i) = choose(rng, rows.len(), |i| rows[i].kind == RowKind::Eos) else {
        return false;
    };
    rows.insert(i + 1, rows[i]);
    true
}

fn stale_gen(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let prev = prev_on_sink(rows, class::FLUSH);
    let wanted = |i: usize| !rows[i].kind.is(class::OUT_OF_BAND) && prev[i].is_some();
    let Some(i) = choose(rng, rows.len(), wanted) else {
        return false;
    };
    rows[i].seek_gen = 0;
    true
}

fn clear_discont(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let prev = prev_on_sink(rows, class::ARRIVAL | RESET);
    let wanted = |i: usize| rows[i].kind == RowKind::Buffer && is(rows, prev[i], class::FLUSH);
    let Some(i) = choose(rng, rows.len(), wanted) else {
        return false;
    };
    rows[i].flags &= !flag::DISCONT;
    true
}

fn duplicate(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let Some(i) = choose(rng, rows.len(), |i| rows[i].kind == RowKind::Buffer) else {
        return false;
    };
    rows.insert(i + 1, rows[i]);
    true
}

fn misroute(rows: &mut Vec<Row>, rng: &mut Prng, sinks: usize) -> bool {
    match choose(rng, rows.len(), |i| rows[i].kind == RowKind::Buffer) {
        Some(i) if rng.chance(50) => rows[i].stream = (rows[i].stream + 1) % 16,
        Some(i) => rows[i].sink = sinks as u16,
        None => rows.push(Row::new(RowKind::Start, sinks as u16, NO_STREAM)),
    }
    true
}

fn lose_buffer(rows: &mut Vec<Row>, rng: &mut Prng, _: usize) -> bool {
    let prev = prev_on_sink(rows, class::ARRIVAL | RESET);
    let next = next_on_sink(rows, class::ARRIVAL | RESET);
    let wanted = |i: usize| {
        rows[i].kind == RowKind::Buffer && is(rows, prev[i], BUFFER) && is(rows, next[i], BUFFER)
    };
    let Some(i) = choose(rng, rows.len(), wanted) else {
        return false;
    };
    rows.remove(i);
    true
}

type Corruption = fn(&mut Vec<Row>, &mut Prng, usize) -> bool;

const CORRUPTIONS: [(Rule, Corruption); Rule::COUNT] = [
    (Rule::FormatBeforeBuffer, drop_format),
    (Rule::SegmentBeforeBuffer, drop_segment),
    (Rule::TimestampsMonotonic, time_back),
    (Rule::NothingAfterEos, after_eos),
    (Rule::EosOnce, double_eos),
    (Rule::NoStaleGeneration, stale_gen),
    (Rule::DiscontAfterFlush, clear_discont),
    (Rule::NoDuplicateSeq, duplicate),
    (Rule::NoMisrouting, misroute),
    (Rule::NoUndeclaredGap, lose_buffer),
];

fn corrupt(valid: &[Row], rng: &mut Prng, sinks: usize) -> (Rule, Vec<Row>) {
    let first = rng.below(Rule::COUNT as u64) as usize;
    for offset in 0..Rule::COUNT {
        let (rule, apply) = CORRUPTIONS[(first + offset) % Rule::COUNT];
        let mut rows = valid.to_vec();
        if apply(&mut rows, rng, sinks) {
            return (rule, rows);
        }
    }
    unreachable!("misroute always applies");
}

#[test]
fn the_sweep_accepts_ideal_runs_and_rejects_one_corruption() {
    let mut spec = MediaSpec::with_capacity(16, 8);
    let mut rec = Recording::with_capacity(1 << 16);
    let mut actions = ActionList::new();
    let sinks = ideal::expect();
    let mut caught = [0u32; Rule::COUNT];
    let (mut rows_seen, mut seeks_fired) = (0usize, 0u32);

    for seed in 0..SEEDS {
        let mut rng = Prng::new(seed).substream_named("property");
        draw_spec(&mut rng, &mut spec);
        let streams = spec.streams.len();
        let params = GenParams {
            count: rng.below(11) as u32,
            duration: 2 * SEC + 200 * MS,
            sinks: streams as u16,
            max_buffers: 60,
            buffer_anchor_percent: 30,
            streams: streams as u16,
            tracks: 2,
            weights: GenParams::PLAYBACK_WEIGHTS,
        };
        action::generate(seed, &params, &mut actions);

        let played = ideal::play(&spec, seed, actions.as_slice(), &mut rec).unwrap();
        assert!(!played.truncated, "seed {seed} overflowed the recording");
        let expect = Expect { sinks: &sinks[..streams], gaps: &[] };
        if let Err(error) = sweep::check(&rec, &expect) {
            panic!("seed {seed} is a valid run but was rejected, {error}");
        }
        let valid = rec.rows().to_vec();
        rows_seen += valid.len();
        seeks_fired +=
            valid.iter().filter(|row| row.kind == RowKind::FlushStart && row.sink == 0).count()
                as u32;

        ideal::play(&spec, seed, actions.as_slice(), &mut rec).unwrap();
        assert_eq!(checksum(rec.rows()), checksum(&valid), "seed {seed} did not replay");

        let (rule, rows) = corrupt(&valid, &mut rng, streams);
        let verdicts = sweep::each(&rows, &expect);
        assert!(verdicts[rule as usize].is_some(), "seed {seed}, {rule} missed its corruption");
        assert!(sweep::all(&rows, &expect).is_err(), "seed {seed}");
        caught[rule as usize] += 1;
    }

    assert!(rows_seen as u64 > SEEDS * 50, "{rows_seen} rows over {SEEDS} runs");
    assert!(seeks_fired as u64 > SEEDS / 2, "{seeks_fired} seeks over {SEEDS} runs");
    for (rule, count) in Rule::ALL.iter().zip(caught) {
        assert!(count as u64 >= SEEDS / 40, "{rule} was exercised only {count} times, {caught:?}");
    }
}

#[test]
fn seeds_reach_the_run() {
    let mut spec = MediaSpec::with_capacity(0, 0);
    let pacing = Pacing::Jitter { base: 500 * US, bound: 20 * MS };
    let video = StreamKind::Video { fps: Rational::new(25, 1), keyframe_interval: 12 };
    spec.add_stream(StreamSpec { kind: video, duration: SEC, size: 1, pacing }).unwrap();
    let mut rec = Recording::with_capacity(4_096);
    let mut digests = Vec::new();
    for seed in 0..64u64 {
        ideal::play(&spec, seed, &[], &mut rec).unwrap();
        assert!(rec.rows().iter().all(|row| row.at != NONE));
        digests.push(checksum(rec.rows()));
    }
    digests.sort_unstable();
    digests.dedup();
    assert_eq!(digests.len(), 64);
}
