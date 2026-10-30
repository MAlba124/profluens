//! Topology family tests, see `pf_sim::driver::topo`. `PF_SIM_SEEDS`, `PF_SIM_SEED`,
//! `PF_SIM_WORKERS`, `PF_SIM_VERSION` and `PF_SIM_FROM` steer the sweep.

// Tests own their staging `Vec`s and `format!` in assertions.
#![allow(clippy::disallowed_methods)]

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pf_sim::driver::topo::{self, Act, Oracle, Scenario, Summary, Verdict};
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, TopologyError};
use profluens_core::format::OfferDesc;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::topology::Leave;

const DEFAULT_SEEDS: u64 = 4_000;

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

fn drawn(seed: u64) -> Scenario {
    topo::draw_at(seed, var("PF_SIM_VERSION").map_or(topo::VERSION, |v| v as u8))
}

fn workers() -> u64 {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    var("PF_SIM_WORKERS").unwrap_or(cores.min(8)).max(1)
}

fn sweep(range: Range<u64>, workers: u64) -> (Summary, f64, Vec<Verdict>) {
    let next = AtomicU64::new(range.start);
    let all = Mutex::new((Summary::default(), 0f64, Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let (mut sum, mut failed) = (Summary::default(), Vec::new());
                let begun = cpu_seconds();
                loop {
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= range.end {
                        break;
                    }
                    match topo::run(&drawn(seed)) {
                        Ok(s) => {
                            sum.steps += s.steps;
                            sum.changes += s.changes;
                            sum.busy += s.busy;
                            sum.cancelled += s.cancelled;
                            sum.failed += s.failed;
                            sum.restarts += s.restarts;
                            sum.joined += s.joined;
                            sum.left += s.left;
                            sum.sealed += s.sealed;
                            sum.gone += s.gone;
                            sum.formats += s.formats;
                            sum.meets = sum.meets.max(s.meets);
                        }
                        Err(verdict) => failed.push(verdict),
                    }
                }
                let spent = cpu_seconds() - begun;
                let mut all = all.lock().unwrap();
                all.0.steps += sum.steps;
                all.0.changes += sum.changes;
                all.0.busy += sum.busy;
                all.0.cancelled += sum.cancelled;
                all.0.failed += sum.failed;
                all.0.restarts += sum.restarts;
                all.0.joined += sum.joined;
                all.0.left += sum.left;
                all.0.sealed += sum.sealed;
                all.0.gone += sum.gone;
                all.0.formats += sum.formats;
                all.0.meets = all.0.meets.max(sum.meets);
                all.1 += spent;
                all.2.extend(failed);
            });
        }
    });
    let (sum, spent, mut failed) = all.into_inner().unwrap();
    failed.sort_by_key(|v| v.seed);
    (sum, spent, failed)
}

fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").unwrap_or_default();
    // Fields 14 and 15, behind the command's closing bracket, in clock ticks of 10 ms.
    let rest = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
    let ticks: Vec<f64> =
        rest.split_whitespace().skip(11).take(2).filter_map(|t| t.parse().ok()).collect();
    ticks.iter().sum::<f64>() / 100.0
}

fn explain(verdict: &Verdict) -> String {
    let sc = drawn(verdict.seed);
    let (_, tape) = topo::trace(&sc);
    let buffer =
        |what: &topo::What| matches!(what, topo::What::Row { .. } | topo::What::Dropped { .. });
    let (mut lines, mut last): (String, Vec<Option<topo::What>>) = (String::new(), Vec::new());
    for (el, what) in &tape {
        let el = *el as usize;
        last.resize(last.len().max(el + 1), None);
        let was = last[el].filter(buffer);
        if buffer(what) && was.is_some() {
            last[el] = Some(*what);
            continue;
        }
        if let Some(held) = was {
            lines.push_str(&format!("  {el}: .. {held:?}\n"));
        }
        lines.push_str(&format!("  {el}: {what:?}\n"));
        last[el] = Some(*what);
    }
    format!("seed {} fails {:?}, {}\n{sc:?}\n{lines}", verdict.seed, verdict.oracle, verdict.detail)
}

#[test]
fn every_seed_of_the_family_passes() {
    let range = seeds();
    let count = range.end - range.start;
    let (sum, spent, failed) = sweep(range, workers());
    if let Some(first) = failed.first() {
        let all: Vec<(u64, Oracle)> = failed.iter().map(|v| (v.seed, v.oracle)).collect();
        panic!("{} of {count} seeds failed, {all:?}, the first\n{}", failed.len(), explain(first));
    }
    let rate = count as f64 / spent.max(1e-9);
    println!(
        "TOPOLOGY {count} seeds, {rate:.0} runs per second per core, {} steps a run, {} changes, {} refused as busy, {} restarts, {} joins, {} leaves, {} cancelled, {} failed, {} sealed, {} links refused as gone, {} formats held, at most {} meets a commit",
        u64::from(sum.steps) / count.max(1),
        sum.changes,
        sum.busy,
        sum.restarts,
        sum.joined,
        sum.left,
        sum.cancelled,
        sum.failed,
        sum.sealed,
        sum.gone,
        sum.formats,
        sum.meets
    );
}

#[test]
fn the_family_draws_every_action_and_changes_most_runs() {
    let count = seeds().end.min(4_000);
    let scenarios: Vec<Scenario> = (0..count).map(topo::draw).collect();
    let any = |fits: fn(&Act) -> bool| {
        scenarios.iter().filter(|sc| sc.script.iter().any(|(_, act)| fits(act))).count()
    };
    let share = |n: usize| n as f64 / count as f64;
    assert!(share(any(|a| matches!(a, Act::Switch { restart: true, .. }))) > 0.4);
    assert!(share(any(|a| matches!(a, Act::Switch { how: Leave::Drain, .. }))) > 0.2);
    assert!(share(any(|a| matches!(a, Act::Rewire { .. }))) > 0.2);
    assert!(
        share(any(|a| matches!(a, Act::Tap { .. }))) > 0.2
            && share(any(|a| matches!(a, Act::Untap))) > 0.2
    );
    assert!(
        share(any(|a| matches!(a, Act::Attach))) > 0.2
            && share(any(|a| matches!(a, Act::Detach))) > 0.2
    );
    assert!(
        share(any(|a| matches!(a, Act::Seek(_)))) > 0.2
            && share(any(|a| matches!(a, Act::Skip(_)))) > 0.1
    );
    assert!(
        share(any(|a| matches!(a, Act::Pause))) > 0.05
            && share(any(|a| matches!(a, Act::Rate(..)))) > 0.03
    );
    assert!(share(any(|a| matches!(a, Act::Stop))) > 0.02);
    assert!(
        share(any(|a| matches!(a, Act::Share { .. }))) > 0.3
            && share(any(|a| matches!(a, Act::Unshare))) > 0.2
    );
    assert!(share(any(|a| matches!(a, Act::Unplug { .. }))) > 0.1);
    assert!(
        share(any(|a| matches!(a, Act::Cancel))) > 0.3
            && share(any(|a| matches!(a, Act::AttachMute))) > 0.2
    );
    assert!(share(any(|a| matches!(a, Act::Fail { .. }))) > 0.3);
    assert!(
        share(any(|a| matches!(a, Act::Resink { .. }))) > 0.5
            && share(any(|a| matches!(a, Act::Fan { .. }))) > 0.5
    );
    assert!(
        scenarios.iter().filter(|sc| sc.typed.is_some_and(|t| t.free)).count() > count as usize / 4
    );
    assert!(scenarios.iter().filter(|sc| sc.typed.is_some()).count() > count as usize / 2);
    assert!(scenarios.iter().filter(|sc| sc.paced).count() > count as usize / 3);
    assert!(scenarios.iter().filter(|sc| sc.copies).count() > count as usize / 3);
    let (sum, _, failed) = sweep(0..count, workers());
    assert!(failed.is_empty(), "{}", explain(&failed[0]));
    assert!(u64::from(sum.changes) > count, "{} changes in {count} runs", sum.changes);
    assert!(u64::from(sum.restarts) > count / 4 && sum.busy > 0, "{sum:?}");
    assert!(u64::from(sum.joined) > count && u64::from(sum.left) > count / 2, "{sum:?}");
    assert!(u64::from(sum.cancelled) > count / 20 && sum.failed > sum.cancelled, "{sum:?}");
    assert!(u64::from(sum.sealed) > count / 20 && sum.gone > 0, "{sum:?}");
    assert!(u64::from(sum.formats) > count && sum.meets <= topo::MEETS, "{sum:?}");
}

/// (version, seed, why). Each is the first of 100,000 seeds to fail with that break put into core.
const PINNED: [(u8, u64, &str); 17] = [
    (3, 0, "a handle reads what left, the run waits at its end on a change that never splices"),
    (
        3,
        2,
        "a cut pad keeps its input, a join at the start of the stream, a cursor that stays, \
            a pad not told it is linked, a ring kept behind a cut, a handle that misses what joined",
    ),
    (3, 5, "a cut branch that is not ended, a commit taken by a run that is over"),
    (3, 6, "a restart that keeps the demuxer's unread input, a cancel that leaves its groups"),
    (3, 14, "a head that keeps the verdict of several links"),
    (3, 20, "a shared batch to one link, a header to every link, a joined element with no log"),
    (3, 23, "a revived consumer with its ended inputs live, a revived stream still ended"),
    (3, 24, "a pool that left and is out of the report"),
    (3, 30, "a source at its end that is not read again"),
    (3, 39, "a cut that takes every link of its pad"),
    (3, 60, "a draining group that loses its end to a seek"),
    (3, 306, "a consumer flushed by a join while it still plays"),
    (3, 2_283, "a cancel that leaves the books as the change made them"),
    (3, 10_279, "a run that ends while a change is on its way"),
    (
        6,
        10,
        "a commit that solves a running link again, a join solved without its running boundary",
    ),
    (7, 24, "a branch that drains with its pools retired at the splice"),
    (7, 26, "a link out of a sealed group taken by a commit"),
];

#[test]
fn the_pinned_seeds_pass() {
    for (version, seed, what) in PINNED {
        if let Err(verdict) = topo::run(&topo::draw_at(seed, version)) {
            panic!("pinned for {what}: {}", explain(&verdict));
        }
    }
}

/// Version 8 seeds a 300M sweep failed with `Timeline`, a skip reaching a branch that had left.
const SKIPPED_ON_THE_WAY_OUT: [u64; 252] = [
    1_337_301,
    3_194_104,
    4_148_026,
    7_423_386,
    9_059_269,
    9_386_021,
    10_742_319,
    11_692_817,
    14_710_749,
    14_968_544,
    19_075_650,
    19_291_097,
    20_581_702,
    21_116_222,
    21_794_700,
    22_235_190,
    22_448_264,
    23_037_739,
    23_038_074,
    23_925_718,
    23_966_959,
    24_068_987,
    25_397_323,
    25_501_317,
    25_604_996,
    25_938_294,
    27_418_593,
    27_620_833,
    28_580_854,
    31_260_674,
    31_601_424,
    31_640_302,
    31_750_967,
    35_468_021,
    37_386_308,
    37_764_108,
    37_857_936,
    38_384_808,
    39_884_627,
    41_750_340,
    42_407_701,
    43_411_295,
    43_753_919,
    46_820_773,
    47_605_091,
    47_945_242,
    49_152_736,
    51_385_232,
    52_202_448,
    53_118_093,
    54_589_328,
    54_670_475,
    55_288_905,
    55_637_364,
    56_311_982,
    56_616_342,
    57_753_046,
    58_828_916,
    62_269_393,
    64_559_877,
    64_607_982,
    65_642_389,
    65_830_897,
    66_416_418,
    67_952_359,
    68_625_001,
    69_661_658,
    70_353_069,
    70_909_989,
    70_934_115,
    74_892_057,
    75_238_434,
    78_439_860,
    78_618_245,
    81_070_032,
    84_301_484,
    84_755_301,
    86_519_855,
    87_027_757,
    89_922_932,
    90_483_914,
    90_743_362,
    90_797_945,
    91_382_151,
    91_897_598,
    92_666_384,
    98_223_821,
    98_265_347,
    98_660_734,
    99_336_420,
    103_022_590,
    103_889_890,
    104_028_829,
    104_637_940,
    106_751_262,
    106_779_083,
    107_895_959,
    109_800_773,
    111_279_081,
    111_841_512,
    112_718_589,
    117_454_079,
    119_477_587,
    120_987_422,
    124_091_997,
    126_535_182,
    127_619_714,
    127_897_426,
    128_942_424,
    129_495_439,
    129_627_369,
    129_834_576,
    130_684_985,
    130_873_508,
    131_417_860,
    132_599_123,
    133_239_286,
    133_625_654,
    135_593_038,
    135_626_199,
    136_550_154,
    136_674_210,
    136_999_968,
    137_869_207,
    139_740_054,
    140_609_090,
    142_112_840,
    144_718_390,
    148_426_248,
    149_357_464,
    152_127_616,
    153_826_525,
    155_655_856,
    155_728_915,
    157_439_119,
    157_694_071,
    157_975_277,
    159_894_093,
    160_076_259,
    160_371_109,
    161_043_496,
    161_399_287,
    161_500_763,
    161_807_595,
    161_982_695,
    165_098_963,
    165_199_788,
    166_543_230,
    169_683_226,
    169_703_065,
    172_628_912,
    173_783_241,
    174_079_985,
    174_306_698,
    175_105_509,
    175_485_921,
    176_189_376,
    176_905_437,
    181_358_622,
    181_671_720,
    181_794_159,
    181_855_929,
    182_858_637,
    188_162_747,
    188_728_390,
    189_367_514,
    190_260_972,
    190_970_729,
    192_837_678,
    194_590_555,
    195_482_519,
    196_894_002,
    199_144_707,
    200_055_472,
    200_269_547,
    201_155_729,
    201_559_295,
    206_193_251,
    207_115_907,
    207_991_302,
    208_800_866,
    208_825_399,
    209_562_799,
    210_320_520,
    210_721_692,
    210_864_237,
    212_675_211,
    215_972_786,
    217_717_020,
    218_559_998,
    218_786_130,
    220_325_171,
    220_473_200,
    220_926_803,
    221_539_895,
    222_981_293,
    224_601_619,
    225_010_800,
    225_864_950,
    226_199_483,
    226_503_062,
    226_559_235,
    228_453_521,
    230_440_583,
    233_599_600,
    233_611_331,
    234_849_933,
    241_750_744,
    242_671_530,
    243_441_719,
    245_684_989,
    248_649_562,
    252_224_832,
    253_231_624,
    254_102_790,
    256_891_193,
    258_219_018,
    258_555_108,
    261_275_672,
    261_416_552,
    261_491_255,
    262_111_383,
    263_900_087,
    264_518_161,
    266_168_281,
    268_873_837,
    269_197_545,
    270_073_210,
    270_188_744,
    270_824_093,
    271_563_075,
    274_284_001,
    274_726_889,
    276_393_420,
    277_922_062,
    281_099_786,
    281_933_766,
    283_280_645,
    284_715_262,
    285_192_190,
    286_654_359,
    289_232_892,
    289_590_106,
    290_173_601,
    291_808_306,
    294_475_452,
    294_648_532,
    296_388_853,
    297_711_011,
    298_211_077,
    299_622_046,
    299_950_588,
];

#[test]
fn a_branch_on_its_way_out_presents_from_a_skip() {
    let failed: Vec<Verdict> = SKIPPED_ON_THE_WAY_OUT
        .iter()
        .filter_map(|seed| topo::run(&topo::draw_at(*seed, 8)).err())
        .collect();
    if let Some(first) = failed.first() {
        panic!(
            "{} of {} seeds failed, the first\n{}",
            failed.len(),
            SKIPPED_ON_THE_WAY_OUT.len(),
            explain(first)
        );
    }
}

#[test]
fn a_seed_replays() {
    for seed in [0, 7, 191, 4_242] {
        let (a, b) = (topo::run(&topo::draw(seed)), topo::run(&topo::draw(seed)));
        match (a, b) {
            (Ok(a), Ok(b)) => assert_eq!(
                (a.steps, a.changes, a.busy),
                (b.steps, b.changes, b.busy),
                "seed {seed}"
            ),
            (a, b) => panic!("seed {seed}: {a:?} and {b:?}"),
        }
    }
}

/// `PF_SIM_SEEDS=100000 cargo test --release -p pf-sim --test topology -- --ignored tallied
/// --nocapture`
#[test]
#[ignore = "prints a tally of verdicts, run it with --ignored"]
fn the_verdicts_of_a_sweep_tallied_by_oracle() {
    let count = var("PF_SIM_SEEDS").unwrap_or(100_000);
    let from = var("PF_SIM_FROM").unwrap_or(0);
    let (_, _, failed) = sweep(from..from + count, workers());
    println!("TEETH {} of {count} seeds failed", failed.len());
    let mut tally: Vec<(Oracle, u64, u64, String)> = Vec::new();
    for verdict in failed {
        println!("FAILED seed {} {:?}, {}", verdict.seed, verdict.oracle, verdict.detail);
        match tally.iter_mut().find(|(oracle, ..)| *oracle == verdict.oracle) {
            Some((_, seeds, ..)) => *seeds += 1,
            None => tally.push((verdict.oracle, 1, verdict.seed, verdict.detail)),
        }
    }
    for (oracle, seeds, first, detail) in &tally {
        println!("TEETH {oracle:?} {seeds} seeds, the first {first}, {detail}");
    }
    // The remote sweeps run this test, so its rc must say what the tally says.
    assert!(tally.is_empty(), "{} oracles failed seeds, see the TEETH lines", tally.len());
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static OUT_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static IN_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
    let inputs = if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single };
    let (sched, latency) = (SchedHint::Active, LatencyDesc::NONE);
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched,
        inputs,
        latency,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static SOURCE: ElementDesc = desc("stress-src", &[], &OUT_PADS);
static COUNTER: ElementDesc = desc("stress-sink", &IN_PADS, &[]);

struct Source {
    next: u64,
    end: Arc<AtomicU64>,
    made: Arc<AtomicU64>,
}

impl Element for Source {
    fn desc(&self) -> &'static ElementDesc {
        &SOURCE
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.end.load(Ordering::Acquire) != 0 {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.as_mut_full()[..8].copy_from_slice(&self.next.to_le_bytes());
        buf.memory.set_len(8);
        ctx.out(SrcPad(0)).push(buf);
        self.next += 1;
        self.made.store(self.next, Ordering::Release);
        Ok(Flow::Ok)
    }
}

struct Counter {
    taken: Arc<AtomicU64>,
    last: Option<u64>,
    lives: Arc<AtomicU64>,
    whole: bool,
}

impl Element for Counter {
    fn desc(&self) -> &'static ElementDesc {
        &COUNTER
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.lives.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let mut seq = [0u8; 8];
            seq.copy_from_slice(&buf.memory.data()[..8]);
            let seq = u64::from_le_bytes(seq);
            assert!(self.last.is_none_or(|last| seq > last), "{seq} behind {:?}", self.last);
            assert!(
                !self.whole || seq == self.last.map_or(0, |last| last + 1),
                "{seq} behind {:?}",
                self.last
            );
            self.last = Some(seq);
            self.taken.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Flow::Ok)
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.lives.fetch_sub(1, Ordering::AcqRel);
    }
}

#[test]
fn thousands_of_relinks_under_load_leave_nothing_behind() {
    let relinks = var("PF_SIM_RELINKS").unwrap_or(3_000);
    let (end, taken, lives) =
        (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let mut p = Pipeline::new();
    p.set_pool(64, 32);
    let src = p.add(Source { next: 0, end: Arc::clone(&end), made: Arc::default() });
    let counter = |taken: &Arc<AtomicU64>, lives: &Arc<AtomicU64>| Counter {
        taken: Arc::clone(taken),
        last: None,
        lives: Arc::clone(lives),
        whole: false,
    };
    let first = p.add(counter(&taken, &lives));
    let link = p.link(src.src(SrcPad(0)), first.sink(SinkPad(0))).expect("link");
    let (handle, tap) = (p.control(), p.tap_handle());
    let editor = {
        let (end, taken, lives) = (Arc::clone(&end), Arc::clone(&taken), Arc::clone(&lives));
        std::thread::spawn(move || {
            let (mut old, mut done, mut refused): (ElementId, u64, u64) = (first, 0, 0);
            while done < relinks {
                let made = counter(&taken, &lives);
                let edit = handle.edit(move |p: &mut Pipeline| {
                    // Asked first, so a refusal costs the books nothing.
                    if p.changing() {
                        return Err(Error::Topology(TopologyError::Busy));
                    }
                    let new = p.add(made);
                    p.relink(link, new.sink(SinkPad(0)))?;
                    p.remove(old, Leave::Cut)?;
                    p.commit().map(|_| new)
                });
                match edit {
                    Ok(Ok(new)) => (old, done) = (new, done + 1),
                    Ok(Err(Error::Topology(TopologyError::Busy))) => {
                        refused += 1;
                        std::thread::yield_now();
                    }
                    Ok(Err(e)) => panic!("relink {done} refused, {e}"),
                    Err(_) => std::thread::yield_now(),
                }
            }
            end.store(1, Ordering::Release);
            (done, refused)
        })
    };
    p.run().expect("the run ends cleanly");
    let (done, refused) = editor.join().expect("the editor");
    println!(
        "STRESS {done} relinks, {refused} commits refused as busy, {} buffers",
        taken.load(Ordering::Relaxed)
    );
    assert_eq!(done, relinks);
    assert_eq!(lives.load(Ordering::Acquire), 0, "every sink that started was stopped");
    assert_eq!(tap.pool().outstanding, 0, "no pool slot is left out");
    assert!(taken.load(Ordering::Relaxed) > relinks, "the stream ran through it");
}

#[test]
fn thousands_of_links_join_and_leave_a_pad_that_plays() {
    let shares = var("PF_SIM_RELINKS").unwrap_or(3_000);
    let (end, made) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (kept, shared, lives) =
        (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let mut p = Pipeline::new();
    p.set_pool(64, 32);
    let src = p.add(Source { next: 0, end: Arc::clone(&end), made: Arc::clone(&made) });
    let keeper = p.add(Counter {
        taken: Arc::clone(&kept),
        last: None,
        lives: Arc::clone(&lives),
        whole: true,
    });
    p.link(src.src(SrcPad(0)), keeper.sink(SinkPad(0))).expect("link");
    let (handle, tap) = (p.control(), p.tap_handle());
    let editor = {
        let (end, shared, lives) = (Arc::clone(&end), Arc::clone(&shared), Arc::clone(&lives));
        std::thread::spawn(move || {
            let (mut beside, mut done, mut refused): (Option<ElementId>, u64, u64) = (None, 0, 0);
            while done < shares {
                let joins = Counter {
                    taken: Arc::clone(&shared),
                    last: None,
                    lives: Arc::clone(&lives),
                    whole: false,
                };
                let edit = handle.edit(move |p: &mut Pipeline| {
                    if p.changing() {
                        return Err(Error::Topology(TopologyError::Busy));
                    }
                    if let Some(old) = beside {
                        p.remove(old, Leave::Cut)?;
                    }
                    let new = p.add(joins);
                    p.link(src.src(SrcPad(0)), new.sink(SinkPad(0)))?;
                    p.commit().map(|_| new)
                });
                match edit {
                    Ok(Ok(new)) => (beside, done) = (Some(new), done + 1),
                    Ok(Err(Error::Topology(TopologyError::Busy))) => {
                        refused += 1;
                        std::thread::yield_now();
                    }
                    Ok(Err(e)) => panic!("share {done} refused, {e}"),
                    Err(_) => std::thread::yield_now(),
                }
            }
            end.store(1, Ordering::Release);
            (done, refused)
        })
    };
    p.run().expect("the run ends cleanly");
    let (done, refused) = editor.join().expect("the editor");
    let (made, kept, shared) = (
        made.load(Ordering::Acquire),
        kept.load(Ordering::Relaxed),
        shared.load(Ordering::Relaxed),
    );
    println!(
        "STRESS {done} links joined a pad that played, {refused} commits refused as busy, {kept} buffers played and {shared} shared"
    );
    assert_eq!(done, shares);
    assert_eq!(kept, made, "the sink that stays was handed every buffer the source made");
    assert!(shared > 0, "the links that joined were fed");
    assert_eq!(lives.load(Ordering::Acquire), 0, "every sink that started was stopped");
    assert_eq!(tap.pool().outstanding, 0, "no pool slot is left out");
}
