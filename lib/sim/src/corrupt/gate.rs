//! Runs cases against a driver and fails on the first panic, [`Violation`] or hang, with a replay.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::chunk::Chunking;
use super::corpus::{self, Corpus};
use super::env::{Env, corpus_case_seed, site_case_seed};
use super::heap::{heap_growth, heap_mark};
use super::op::{Op, Site, corrupt};
use super::sites::sweep;
use crate::prng::{Prng, mix};

#[derive(Clone, Debug)]
pub struct Case {
    pub base: &'static str,
    pub ops: Vec<Op>,
    pub data: Vec<u8>,
    pub chunking: Chunking,
    pub knob: u64,
}

/// A bound the code under test broke without panicking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    Steps { taken: u64, limit: u64 },
    Heap { grew: usize, limit: usize },
    Output { bytes: usize, limit: usize },
    Invariant(&'static str),
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Steps { taken, limit } => write!(f, "{taken} steps taken, limit {limit}"),
            Violation::Heap { grew, limit } => {
                write!(f, "the heap grew {grew} bytes, limit {limit}")
            }
            Violation::Output { bytes, limit } => {
                write!(f, "{bytes} emitted or buffered, limit {limit}")
            }
            Violation::Invariant(what) => f.write_str(what),
        }
    }
}

impl std::error::Error for Violation {}

pub fn check_steps(taken: u64, limit: u64) -> Result<(), Violation> {
    if taken > limit {
        return Err(Violation::Steps { taken, limit });
    }
    Ok(())
}

pub fn check_heap(mark: isize, limit: usize) -> Result<(), Violation> {
    let grew = heap_growth(mark);
    if grew > limit {
        return Err(Violation::Heap { grew, limit });
    }
    Ok(())
}

pub fn check_output(bytes: usize, limit: usize) -> Result<(), Violation> {
    if bytes > limit {
        return Err(Violation::Output { bytes, limit });
    }
    Ok(())
}

/// Fails with `Heap` if the heap peaked over `base + per_byte * input length` above entry.
pub fn heap_bounded(
    base: usize,
    per_byte: usize,
    target: impl Fn(&[u8], Chunking),
) -> impl Fn(&Case) -> Result<(), Violation> {
    move |case| {
        let mark = heap_mark();
        target(&case.data, case.chunking);
        check_heap(mark, base + per_byte * case.data.len())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub cases: u64,
    pub tolerated: u64,
    pub tolerated_at: Vec<String>,
    /// `PF_FUZZ_CASE` narrowed the run to one case. A gate asserting a count over a run skips it.
    pub replay: bool,
    elapsed: Duration,
    slowest: Option<Slowest>,
}

#[derive(Clone, Copy, Debug)]
struct Slowest {
    took: Duration,
    seed: Option<u64>,
    len: usize,
    chunking: Chunking,
}

impl Summary {
    pub fn print(&self, name: &str) {
        let slowest = match self.slowest {
            Some(Slowest { took, seed, len, chunking }) => {
                let seed = seed.map_or("a sweep case".to_string(), |s| format!("seed {s:#X}"));
                format!(", slowest {} ms at {seed} ({len} bytes, {chunking:?})", took.as_millis())
            }
            None => String::new(),
        };
        let ms = self.elapsed.as_millis();
        eprintln!("corruption gate `{name}` passed {} cases in {ms} ms{slowest}", self.cases);
        if self.tolerated > 0 {
            eprintln!(
                "corruption gate `{name}`, {} of {} cases panicked inside a tolerated \
                 dependency at {:?}",
                self.tolerated, self.cases, self.tolerated_at
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Capture {
    Off,
    Quiet,
    Loud,
}

thread_local! {
    static CAPTURE: Cell<Capture> = const { Cell::new(Capture::Off) };
    static LAST_PANIC: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

fn install_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let capture = CAPTURE.try_with(Cell::get).unwrap_or(Capture::Off);
            if capture != Capture::Quiet {
                previous(info);
            }
            if capture == Capture::Off {
                return;
            }
            let payload = info.payload();
            let message = match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
                (Some(text), _) => (*text).to_string(),
                (_, Some(text)) => text.clone(),
                _ => "a panic payload that is not text".to_string(),
            };
            let location = match info.location() {
                Some(l) => format!("{}:{}:{}", l.file(), l.line(), l.column()),
                None => "unknown location".to_string(),
            };
            let _ = LAST_PANIC.try_with(|slot| *slot.borrow_mut() = Some((message, location)));
        }));
    });
}

enum Verdict {
    Pass,
    Violation(Violation),
    Panic { message: String, location: String },
}

const UNKNOWN_LOCATION: &str = "unknown location";

impl Verdict {
    fn same(&self, other: &Verdict) -> bool {
        match (self, other) {
            // With no location captured, a panic from a thread the driver spawned, the message
            // is all that tells two bugs apart.
            (
                Verdict::Panic { location: a, message: ma },
                Verdict::Panic { location: b, message: mb },
            ) => a == b && (a != UNKNOWN_LOCATION || ma == mb),
            (
                Verdict::Violation(Violation::Invariant(a)),
                Verdict::Violation(Violation::Invariant(b)),
            ) => a == b,
            (Verdict::Violation(a), Verdict::Violation(b)) => {
                std::mem::discriminant(a) == std::mem::discriminant(b)
            }
            _ => false,
        }
    }
}

type Drive<'a> = &'a dyn Fn(&Case) -> Result<(), Violation>;

fn judge(drive: Drive, case: &Case, capture: Capture) -> Verdict {
    CAPTURE.with(|c| c.set(capture));
    LAST_PANIC.with(|slot| slot.borrow_mut().take());
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| drive(case)));
    CAPTURE.with(|c| c.set(Capture::Off));
    match outcome {
        Ok(Ok(())) => Verdict::Pass,
        Ok(Err(violation)) => Verdict::Violation(violation),
        Err(_) => {
            // Empty when the panic crossed over from a thread the driver spawned.
            let (message, location) = LAST_PANIC
                .with(|slot| slot.borrow_mut().take())
                .unwrap_or_else(|| ("panic".to_string(), UNKNOWN_LOCATION.to_string()));
            Verdict::Panic { message, location }
        }
    }
}

struct Drawn<'a> {
    seed: Option<u64>,
    origin: &'a str,
    case: Case,
}

const OPS_SHOWN: usize = 2_000;

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 6);
    for (i, byte) in data.iter().enumerate() {
        let sep = if i == 0 { "" } else { ", " };
        let _ = fmt::Write::write_fmt(&mut out, format_args!("{sep}0x{byte:02X}"));
    }
    out
}

fn describe(headline: &str, drawn: &Drawn) -> String {
    let case = &drawn.case;
    let seed = match drawn.seed {
        Some(seed) => format!("{seed:#018X} (replay with PF_FUZZ_CASE={seed:#X})"),
        None => "none, a deterministic sweep case".to_string(),
    };
    // Debug output of ops is ASCII, so any byte offset is a character boundary.
    let mut ops = format!("{:?}", case.ops);
    if ops.len() > OPS_SHOWN {
        ops.truncate(OPS_SHOWN);
        ops.push_str(" and more, cut here");
    }
    let mut text = format!(
        "\n{headline}\n  seed       {seed}\n  base       {}\n  corruption {ops}\n  \
         chunking   {:?}\n  knob       {}\n  input      {} bytes",
        drawn.origin,
        case.chunking,
        case.knob,
        case.data.len(),
    );
    if case.data.len() <= 256 {
        text.push_str(&format!("\n  bytes      [{}]", hex(&case.data)));
    }
    text
}

fn file_stem(gate: &str) -> String {
    let keep = |c: char| {
        if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }
    };
    gate.chars().map(keep).collect()
}

/// Written before each judge, so a crash that takes the process, a stack overflow, an abort or a
/// fault in a C decoder, leaves the case it was on in the dump directory.
fn breadcrumb(gate: &Gate, drawn: &Drawn, index: u64) {
    let Some(dir) = &gate.env.dump else {
        return;
    };
    let which = match drawn.seed {
        Some(seed) => format!("seed {seed:#X} (replay with PF_FUZZ_CASE={seed:#X})"),
        None => format!("sweep case {index}"),
    };
    let text = format!("corruption gate `{}` running case {index}, {which}\n", gate.name);
    let _ = std::fs::write(format!("{dir}/{}.current", file_stem(gate.name)), text);
}

fn dump(env: &Env, gate: &str, drawn: &Drawn, index: u64, suffix: &str, bytes: &[u8]) -> String {
    let Some(dir) = &env.dump else {
        return String::new();
    };
    let gate = file_stem(gate);
    let which = match drawn.seed {
        Some(seed) => format!("{seed:016x}"),
        None => format!("sweep-{index}"),
    };
    let path = format!("{dir}/{gate}-{which}.{suffix}");
    match std::fs::write(&path, bytes) {
        Ok(()) => format!("\n  dumped     {path}"),
        Err(e) => format!("\n  dump failed, {e}"),
    }
}

const SHRINK_RUNS: u32 = 4_000;
const SHRINK_TIME: Duration = Duration::from_secs(20);

fn shrink_pass(
    probe: &mut Case,
    block: usize,
    budget: &mut u32,
    deadline: Instant,
    fails: &dyn Fn(&Case) -> bool,
) -> bool {
    let (mut at, mut progressed) = (0, false);
    while at < probe.data.len() && *budget > 0 && Instant::now() < deadline {
        let end = (at + block).min(probe.data.len());
        let cut: Vec<u8> = probe.data.drain(at..end).collect();
        *budget -= 1;
        if fails(probe) {
            progressed = true;
        } else {
            probe.data.splice(at..at, cut);
            at += block;
        }
    }
    progressed
}

fn shrink(probe: &mut Case, fails: &dyn Fn(&Case) -> bool) {
    let deadline = Instant::now() + SHRINK_TIME;
    let mut budget = SHRINK_RUNS;
    let mut block = (probe.data.len().next_power_of_two() / 2).max(1);
    while budget > 0 && Instant::now() < deadline {
        let progressed = shrink_pass(probe, block, &mut budget, deadline, fails);
        if block == 1 && !progressed {
            break;
        }
        block = (block / 2).max(1);
    }
}

fn reproduce(gate: &Gate, drawn: &Drawn, index: u64, verdict: &Verdict) -> String {
    let fails = |probe: &Case| judge(gate.drive, probe, Capture::Quiet).same(verdict);
    let mut probe = Case { ops: Vec::new(), chunking: Chunking::Whole, ..drawn.case.clone() };
    if !fails(&probe) {
        probe.chunking = drawn.case.chunking;
        if !fails(&probe) {
            // State left by an earlier case, or code under test that is not deterministic.
            // The bytes above are no reproducer and are not called one.
            return "\n  rerun      the same case passed when run again, the failure is flaky or \
                    depends on the cases before it"
                .to_string()
                + &dump(gate.env, gate.name, drawn, index, "bin", &drawn.case.data);
        }
    }
    shrink(&mut probe, &fails);
    let mut text =
        format!("\n  minimized  {} bytes, fed as {:?}", probe.data.len(), probe.chunking);
    if probe.data.len() <= 512 {
        text.push_str(&format!("\n  [{}]", hex(&probe.data)));
    } else if gate.env.dump.is_none() {
        text.push_str(" (set PF_FUZZ_DUMP to a directory to save them)");
    }
    text.push_str(&dump(gate.env, gate.name, drawn, index, "bin", &drawn.case.data));
    text.push_str(&dump(gate.env, gate.name, drawn, index, "min.bin", &probe.data));
    text
}

struct Watch {
    current: AtomicU64,
    started: AtomicU64,
    done: AtomicBool,
    origin: Instant,
}

impl Watch {
    fn new() -> Watch {
        Watch {
            current: AtomicU64::new(0),
            started: AtomicU64::new(0),
            done: AtomicBool::new(false),
            origin: Instant::now(),
        }
    }

    fn begin(&self, index: u64) {
        self.current.store(index, Ordering::Release);
        self.started.store(self.origin.elapsed().as_millis() as u64 + 1, Ordering::Release);
    }

    fn end(&self) {
        self.started.store(0, Ordering::Release);
    }

    fn overdue(&self, timeout: Duration) -> Option<u64> {
        let begun = self.started.load(Ordering::Acquire);
        if begun == 0 {
            return None;
        }
        let index = self.current.load(Ordering::Acquire);
        let ran = self.origin.elapsed().saturating_sub(Duration::from_millis(begun - 1));
        // Re-read both, so a case that ended meanwhile is not blamed.
        let same = self.started.load(Ordering::Acquire) == begun
            && self.current.load(Ordering::Acquire) == index;
        (same && ran > timeout).then_some(index)
    }
}

fn watchdog(watch: &Watch, timeout: Duration, report: &dyn Fn(u64) -> String) {
    while !watch.done.load(Ordering::Acquire) {
        std::thread::park_timeout(Duration::from_millis(250));
        let Some(index) = watch.overdue(timeout) else {
            continue;
        };
        // Straight to the fd. The harness captures `eprintln!` and an abort would discard it.
        let text = report(index) + "\n";
        let _ = std::io::stderr().write_all(text.as_bytes());
        std::process::abort();
    }
}

/// `case_at` is `Sync` because the watchdog thread rebuilds the case it reports.
struct Gate<'a> {
    name: &'a str,
    tolerate: &'a [&'a str],
    count: u64,
    case_at: &'a (dyn Fn(u64) -> Drawn<'a> + Sync),
    drive: Drive<'a>,
    env: &'a Env,
}

fn tolerate(gate: &Gate, drawn: &Drawn, index: u64, verdict: &Verdict, summary: &mut Summary) {
    let Verdict::Panic { message, location } = verdict else {
        return;
    };
    summary.tolerated += 1;
    if summary.tolerated_at.contains(location) || summary.tolerated_at.len() >= 64 {
        return;
    }
    if gate.env.show_tolerated {
        let headline = format!(
            "tolerated dependency panic at {location}\n  panic      {message}\n  gate       {}",
            gate.name
        );
        eprintln!("{}{}", describe(&headline, drawn), reproduce(gate, drawn, index, verdict));
    } else {
        // Each new place once, with its seed, so a panic in a shipping demuxer the gate only
        // tolerates is still on record and replays.
        let seed = match drawn.seed {
            Some(seed) => format!("PF_FUZZ_CASE={seed:#X}"),
            None => format!("sweep case {index}"),
        };
        eprintln!(
            "corruption gate `{}` tolerated a panic at {location}, {message}, first at {seed} \
             (PF_FUZZ_SHOW_TOLERATED=1 prints the case)",
            gate.name
        );
    }
    summary.tolerated_at.push(location.clone());
}

fn conclude(
    gate: &Gate,
    drawn: &Drawn,
    index: u64,
    verdict: &Verdict,
    summary: &mut Summary,
) -> Option<String> {
    let failure = match verdict {
        Verdict::Pass => return None,
        Verdict::Violation(violation) => violation.to_string(),
        Verdict::Panic { location, .. } if gate.tolerate.iter().any(|f| location.contains(f)) => {
            tolerate(gate, drawn, index, verdict, summary);
            return None;
        }
        Verdict::Panic { message, location } => {
            format!("it panicked\n  panic      {message}\n  at         {location}")
        }
    };
    let headline = format!("corruption gate `{}` failed, {failure}", gate.name);
    let described = describe(&headline, drawn);
    // Straight to the fd before minimizing, so a hang or crash while reproducing, which the
    // watchdog aborts, still shows what failed and its seed.
    let _ = std::io::stderr().write_all(format!("{described}\n").as_bytes());
    Some(described + &reproduce(gate, drawn, index, verdict))
}

fn run_case(gate: &Gate, watch: &Watch, index: u64, summary: &mut Summary) -> Option<String> {
    let drawn = (gate.case_at)(index);
    let capture = if gate.env.case.is_some() { Capture::Loud } else { Capture::Quiet };
    breadcrumb(gate, &drawn, index);
    watch.begin(index);
    let begin = Instant::now();
    let verdict = judge(gate.drive, &drawn.case, capture);
    let took = begin.elapsed();
    summary.cases += 1;
    if summary.slowest.is_none_or(|slowest| took > slowest.took) {
        let Case { ref data, chunking, .. } = drawn.case;
        summary.slowest = Some(Slowest { took, seed: drawn.seed, len: data.len(), chunking });
    }
    // Still watched, so a hang while minimizing aborts with this case's report, but on a window
    // of its own, so a slow case plus its shrinking is not read as a hang.
    watch.begin(index);
    let failure = conclude(gate, &drawn, index, &verdict, summary);
    watch.end();
    failure
}

fn run(gate: &Gate) -> Summary {
    install_hook();
    let watch = Watch::new();
    let timeout = Duration::from_secs(gate.env.timeout_secs);
    let (name, case_at, env) = (gate.name, gate.case_at, gate.env);
    let overdue = |index: u64| {
        let drawn = case_at(index);
        let headline =
            format!("corruption gate `{name}` failed, a case ran for over {} s", timeout.as_secs());
        describe(&headline, &drawn) + &dump(env, name, &drawn, index, "bin", &drawn.case.data)
    };
    let mut summary = Summary::default();
    let failure = std::thread::scope(|scope| {
        let dog = scope.spawn(|| watchdog(&watch, timeout, &overdue));
        let failure = (0..gate.count).find_map(|index| run_case(gate, &watch, index, &mut summary));
        watch.done.store(true, Ordering::Release);
        dog.thread().unpark();
        failure
    });
    summary.elapsed = watch.origin.elapsed();
    summary.replay = gate.env.case.is_some();
    if let Some(report) = failure {
        panic!("{report}");
    }
    summary
}

type Make<'a> = &'a (dyn Fn(&mut Prng) -> Case + Sync);

fn site_gate(env: &Env, name: &str, default_iters: u64, make: Make, drive: Drive) -> Summary {
    let count = if env.case.is_some() { 1 } else { env.iters.unwrap_or(default_iters) };
    let case_at = |index: u64| {
        let seed = env.case.unwrap_or_else(|| site_case_seed(env, name, index));
        let case = make(&mut Prng::new(seed));
        Drawn { seed: Some(seed), origin: case.base, case }
    };
    if env.case.is_some() {
        eprintln!("{}", describe(&format!("corruption gate `{name}` replaying"), &case_at(0)));
    }
    run(&Gate { name, tolerate: &[], count, case_at: &case_at, drive, env })
}

fn site_sweep(
    env: &Env,
    name: &str,
    base_name: &'static str,
    base: &[u8],
    sites: &[Site],
    default_budget: u64,
    drive: Drive,
) -> Summary {
    let ops = sweep(sites);
    let total = ops.len() as u64 * 2;
    let count = match (env.case, env.soak()) {
        (Some(_), _) => 0,
        (None, true) => total,
        (None, false) => total.min(default_budget.max(1)),
    };
    // Chunks stay small against the base, so a large one is not fed 5 octets at a time.
    let max = (base.len() / 64).clamp(5, 4096);
    let case_at = |nth: u64| {
        let i = (nth as u128 * total as u128 / count.max(1) as u128) as u64;
        let op = ops[(i / 2) as usize].clone();
        let chunking = match i % 2 {
            0 => Chunking::Whole,
            _ => Chunking::Random { max, seed: i },
        };
        let data = corrupt(base, std::slice::from_ref(&op));
        // Mixed, else the pool shape of `i & 3` follows the chunking of `i % 2`.
        let case = Case { base: base_name, ops: vec![op], data, chunking, knob: mix(i) };
        Drawn { seed: None, origin: base_name, case }
    };
    run(&Gate { name, tolerate: &[], count, case_at: &case_at, drive, env })
}

fn corpus_run(env: &Env, corpus: &Corpus, drive: Drive) -> Summary {
    let replay = env.case.is_some();
    let plan =
        if replay { Vec::new() } else { corpus::plan(corpus.inputs, corpus.sweep, env.soak()) };
    let planned = plan.len() as u64;
    let name_of = |input: Option<usize>| input.map_or("random bytes", |i| corpus.inputs[i].name);
    let case_at = |index: u64| {
        if let Some((input, op, chunking)) = plan.get(index as usize) {
            let data = corrupt(corpus.inputs[*input].bytes, std::slice::from_ref(op));
            let (ops, chunking) = (vec![op.clone()], *chunking);
            let case = Case { base: "corpus", ops, data, chunking, knob: mix(index) };
            return Drawn { seed: None, origin: name_of(Some(*input)), case };
        }
        let seed = env.case.unwrap_or_else(|| corpus_case_seed(env, corpus.name, index - planned));
        let mut prng = Prng::new(seed);
        let draft = corpus::draw(&mut prng, corpus.inputs, corpus.tokens);
        let corpus::Draft { input, ops, data, chunking } = draft;
        // Drawn after the case, so the cases drawn before the knob stay what they were.
        let case = Case { base: "corpus", ops, data, chunking, knob: prng.next_u64() };
        Drawn { seed: Some(seed), origin: name_of(input), case }
    };
    if replay {
        let headline = format!("corruption gate `{}` replaying", corpus.name);
        eprintln!("{}", describe(&headline, &case_at(0)));
    }
    let count = if replay { 1 } else { planned + corpus.iters };
    let Corpus { name, tolerate, .. } = *corpus;
    run(&Gate { name, tolerate, count, case_at: &case_at, drive, env })
}

/// Run a seeded gate. `PF_FUZZ_ITERS` replaces `default_iters`, `PF_FUZZ_CASE` runs one seed.
/// Panics with a minimized reproducer on a panic or violation. A hang aborts the process.
pub fn gate(
    name: &str,
    default_iters: u64,
    make: &(dyn Fn(&mut Prng) -> Case + Sync),
    drive: &dyn Fn(&Case) -> Result<(), Violation>,
) -> Summary {
    let summary = site_gate(&Env::read(), name, default_iters, make, drive);
    summary.print(name);
    summary
}

/// Every op of [`sweep`] over `sites`, fed whole and chunked. A soak runs all cases, a replay none.
pub fn sweep_gate(
    name: &str,
    base_name: &'static str,
    base: &[u8],
    sites: &[Site],
    default_budget: u64,
    drive: &dyn Fn(&Case) -> Result<(), Violation>,
) -> Summary {
    let summary = site_sweep(&Env::read(), name, base_name, base, sites, default_budget, drive);
    summary.print(name);
    summary
}

/// Corpus sweeps first, then `corpus.iters` seeded cases. `PF_FUZZ_CASE` runs one seed.
pub fn corpus_gate(corpus: &Corpus, drive: &dyn Fn(&Case) -> Result<(), Violation>) -> Summary {
    let summary = corpus_gate_quiet(corpus, drive);
    summary.print(corpus.name);
    summary
}

/// [`corpus_gate`] without the closing line.
pub fn corpus_gate_quiet(
    corpus: &Corpus,
    drive: &dyn Fn(&Case) -> Result<(), Violation>,
) -> Summary {
    corpus_run(&Env::read(), corpus, drive)
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
