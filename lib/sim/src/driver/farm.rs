//! Many seeds over worker threads, one single-threaded run per seed. A failure comes back as its
//! seed and verdict and replays alone, and [`shrink()`] cuts its script down.
//!
//! ```
//! use pf_sim::driver::{farm, run::Worker, scenario};
//!
//! assert!(Worker::new().run(&scenario::draw(42)).is_ok());
//! let report = farm::sweep(0..50, 2, scenario::draw);
//! assert_eq!((report.runs, report.failed), (50, 0), "{report}");
//! ```

use core::fmt;
use std::ops::Range;
use std::panic::{self, AssertUnwindSafe};
use std::time::{Duration, Instant};

use super::oracle::End;
use super::run::{Phases, Summary, Worker};
use super::scenario::{Scenario, Script};
use super::verdict::{Detail, Finding, Kind, Verdict};
use crate::action::{Action, MAX_ACTIONS};
use crate::shrink::{self, ShrinkError};

pub const KEPT_FAILURES: usize = 32;

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub runs: u64,
    pub failed: u64,
    pub failures: Vec<Verdict>,
    pub cpu: Duration,
    pub wall: Duration,
    pub workers: usize,
    pub steps: u64,
    pub passes: u64,
    pub cuts: u64,
    pub rows: u64,
    pub taken: u64,
    pub refused: u64,
    pub seeks: u64,
    pub skips: u64,
    pub rates: u64,
    pub offsets: u64,
    pub retimes: u64,
    pub finished: u64,
    pub stopped: u64,
    pub errored: u64,
    pub rejected: u64,
    pub unlinked: u64,
    pub bad_seeks: u64,
    /// Every run's checksum folded together, the same whatever the worker count.
    pub checksum: u64,
    pub phases: Option<Phases>,
}

impl Report {
    pub fn runs_per_cpu_second(&self) -> f64 {
        self.runs as f64 / self.cpu.as_secs_f64().max(1e-9)
    }

    fn add(&mut self, summary: &Summary) {
        self.steps += summary.steps as u64;
        self.passes += summary.passes as u64;
        self.cuts += summary.cuts as u64;
        self.rows += summary.rows as u64;
        self.taken += summary.taken as u64;
        self.refused += summary.refused as u64;
        self.seeks += summary.seeks as u64;
        self.skips += summary.skips as u64;
        self.rates += summary.rates as u64;
        self.offsets += summary.offsets as u64;
        self.retimes += summary.retimes as u64;
        match summary.end {
            End::Finished => self.finished += 1,
            End::Stopped => self.stopped += 1,
            End::Failed => self.errored += 1,
            End::Rejected => self.rejected += 1,
            End::Refused => self.unlinked += 1,
            End::BadSeek => self.bad_seeks += 1,
        }
        // A sum, so the order the workers finish in does not show.
        self.checksum = self.checksum.wrapping_add(summary.checksum);
    }

    fn merge(&mut self, shard: Report) {
        self.runs += shard.runs;
        self.failed += shard.failed;
        self.failures.extend(shard.failures);
        self.cpu += shard.cpu;
        self.steps += shard.steps;
        self.passes += shard.passes;
        self.cuts += shard.cuts;
        self.rows += shard.rows;
        self.taken += shard.taken;
        self.refused += shard.refused;
        self.seeks += shard.seeks;
        self.skips += shard.skips;
        self.rates += shard.rates;
        self.offsets += shard.offsets;
        self.retimes += shard.retimes;
        self.bad_seeks += shard.bad_seeks;
        self.finished += shard.finished;
        self.stopped += shard.stopped;
        self.errored += shard.errored;
        self.rejected += shard.rejected;
        self.unlinked += shard.unlinked;
        self.checksum = self.checksum.wrapping_add(shard.checksum);
        if let (Some(total), Some(part)) = (self.phases.as_mut(), shard.phases) {
            total.build += part.build;
            total.driver += part.driver;
            total.passes += part.passes;
            total.recording += part.recording;
            total.checks += part.checks;
            total.teardown += part.teardown;
            total.sweep += part.sweep;
            total.runs += part.runs;
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let runs = self.runs.max(1);
        writeln!(
            f,
            "{} seeds, {} failed. {:.0} runs per second per core ({:.2} s of cpu, {:.2} s of \
             wall on {} workers)",
            self.runs,
            self.failed,
            self.runs_per_cpu_second(),
            self.cpu.as_secs_f64(),
            self.wall.as_secs_f64(),
            self.workers,
        )?;
        writeln!(
            f,
            "per run {} steps, {} passes, {} of them cut, {} rows, {:.1} commands taken, {:.1} \
             refused, {:.1} seeks, {:.2} skips. {} finished, {} stopped, {} ended on a declared \
             error, {} on a format a sink turned away, {} on a seek the rule would not send, {} \
             were refused at link",
            self.steps / runs,
            self.passes / runs,
            self.cuts / runs,
            self.rows / runs,
            self.taken as f64 / runs as f64,
            self.refused as f64 / runs as f64,
            self.seeks as f64 / runs as f64,
            self.skips as f64 / runs as f64,
            self.finished,
            self.stopped,
            self.errored,
            self.rejected,
            self.bad_seeks,
            self.unlinked,
        )?;
        writeln!(
            f,
            "the log took {} rates and {} offsets, and a retimer told the timeline {} times",
            self.rates, self.offsets, self.retimes,
        )?;
        if let Some(p) = self.phases.filter(|p| p.runs > 0) {
            let us = |d: Duration| d.as_secs_f64() * 1e6 / p.runs as f64;
            writeln!(
                f,
                "microseconds per run, build {:.1}, driver loop {:.1}, passes {:.1}, recording \
                 {:.1}, checks {:.1}, teardown {:.1}, sweep {:.1}",
                us(p.build),
                us(p.driver),
                us(p.passes),
                us(p.recording),
                us(p.checks),
                us(p.teardown),
                us(p.sweep),
            )?;
        }
        for verdict in &self.failures {
            writeln!(f, "{verdict}")?;
        }
        Ok(())
    }
}

/// Thread CPU time from the kernel's accounting. The wall says little on a busy machine.
// A measurement around a shard of runs, never inside one.
#[allow(clippy::disallowed_methods)]
fn on_cpu() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    stat.split_whitespace().next()?.parse().ok().map(Duration::from_nanos)
}

// The farm times its own sweep. No run reads it.
#[allow(clippy::disallowed_methods)]
fn shard(
    seeds: Range<u64>,
    first: u64,
    stride: u64,
    scenario: fn(u64) -> Scenario,
    timed: bool,
) -> Report {
    let mut report = Report::default();
    let mut worker = Worker::new();
    worker.time_phases(timed);
    let (wall, cpu) = (Instant::now(), on_cpu());
    let mut seed = seeds.start.saturating_add(first);
    while seed < seeds.end {
        report.runs += 1;
        // A panic in core or a fake is a verdict with its seed, not a shard lost with every
        // seed it had run. The worker's state after one is unknown, so a fresh one goes on.
        let ran = panic::catch_unwind(AssertUnwindSafe(|| worker.run(&scenario(seed))));
        let ran = ran.unwrap_or_else(|_| {
            worker = Worker::new();
            worker.time_phases(timed);
            Err(Verdict::new(seed, 0, Finding::run(Kind::Panicked, Detail::None)))
        });
        match ran {
            Ok(summary) => report.add(&summary),
            Err(verdict) => {
                report.failed += 1;
                if report.failures.len() < KEPT_FAILURES {
                    report.failures.push(verdict);
                }
            }
        }
        let Some(next) = seed.checked_add(stride) else {
            break;
        };
        seed = next;
    }
    report.cpu = match (cpu, on_cpu()) {
        (Some(from), Some(to)) => to.saturating_sub(from),
        _ => wall.elapsed(),
    };
    report.phases = worker.phases();
    report
}

// The farm times its own sweep. No run reads it.
#[allow(clippy::disallowed_methods)]
fn sweep_with(
    seeds: Range<u64>,
    workers: usize,
    scenario: fn(u64) -> Scenario,
    timed: bool,
) -> Report {
    let workers = workers.max(1);
    let wall = Instant::now();
    let shards: Vec<Report> = std::thread::scope(|scope| {
        let spawn = |k: usize| {
            let seeds = seeds.clone();
            scope.spawn(move || shard(seeds, k as u64, workers as u64, scenario, timed))
        };
        let handles: Vec<_> = (0..workers).map(spawn).collect();
        // A shard catches its runs' panics, so a lost shard is the farm's own fault, and the
        // missing runs show in the total.
        handles.into_iter().filter_map(|handle| handle.join().ok()).collect()
    });
    let phases = timed.then(Phases::default);
    let mut total = Report { workers, phases, ..Report::default() };
    for shard in shards {
        total.merge(shard);
    }
    total.failures.sort_by_key(|verdict| verdict.seed);
    total.failures.truncate(KEPT_FAILURES);
    total.wall = wall.elapsed();
    total
}

pub fn sweep(seeds: Range<u64>, workers: usize, scenario: fn(u64) -> Scenario) -> Report {
    sweep_with(seeds, workers, scenario, false)
}

pub fn sweep_timed(seeds: Range<u64>, workers: usize, scenario: fn(u64) -> Scenario) -> Report {
    sweep_with(seeds, workers, scenario, true)
}

#[derive(Clone, Copy, Debug)]
pub struct Shrunk {
    pub scenario: Scenario,
    pub verdict: Verdict,
    pub tests: u32,
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShrinkFailure {
    NotReproduced,
}

impl fmt::Display for ShrinkFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the scenario does not fail with that kind of verdict when run again")
    }
}

impl std::error::Error for ShrinkFailure {}

pub fn shrink(
    worker: &mut Worker,
    sc: &Scenario,
    verdict: &Verdict,
    max_tests: u32,
) -> Result<Shrunk, ShrinkFailure> {
    let mut script = sc.script;
    let kind = verdict.kind;
    let fails = |actions: &[Action]| {
        let run = worker.run(&sc.with_script(actions));
        run.is_err_and(|found| found.kind == kind)
    };
    let done = match shrink::shrink(&mut script, max_tests, fails) {
        Ok(done) => done,
        Err(ShrinkError::InputPasses) => return Err(ShrinkFailure::NotReproduced),
    };
    let scenario = sc.with_script(script.as_slice());
    match worker.run(&scenario) {
        Err(found) if found.kind == kind => {
            Ok(Shrunk { scenario, verdict: found, tests: done.tests, exhausted: done.exhausted })
        }
        _ => Err(ShrinkFailure::NotReproduced),
    }
}

/// `worker` must have run `scenario` last.
pub struct Failure<'a> {
    pub worker: &'a Worker,
    pub scenario: &'a Scenario,
    pub verdict: &'a Verdict,
    pub trace_rows: usize,
}

impl fmt::Display for Failure<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.verdict)?;
        write!(f, "{}", self.scenario)?;
        writeln!(f, "answers")?;
        let script = self.scenario.script.as_slice();
        for (index, answer) in self.worker.answers().iter().enumerate().take(script.len()) {
            writeln!(f, "  {index:2} {}", answer.name())?;
        }
        writeln!(f, "trace, the last {} of {} rows", self.trace_rows, self.worker.trace().count())?;
        for row in self.worker.trace().tail(self.trace_rows) {
            writeln!(f, "  {row}")?;
        }
        Ok(())
    }
}

pub fn script_of(sc: &Scenario) -> Script<'_> {
    Script(&sc.script.as_slice()[..sc.script.len().min(MAX_ACTIONS)], Some(sc))
}
