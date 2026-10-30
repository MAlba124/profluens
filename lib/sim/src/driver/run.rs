//! One run, from seed to verdict, over core's stepped driver. One thread, no sleep, no wall clock.
//! A worker owns a run's memory between runs. Core does not, so the pipeline is rebuilt every run.

use std::sync::Arc;
use std::time::{Duration, Instant};

use profluens_core::bus::BusMessage;
use profluens_core::clock::Clock;
use profluens_core::command::CommandError;
use profluens_core::command::{ALL, Command};
use profluens_core::counters::TapHandle;
use profluens_core::error::{Error, FormatError};
use profluens_core::pipeline::{SeekMode, SeekPlan};
use profluens_core::sched::{BlockedOn, Cut, Outcome, Stepped};
use profluens_core::time::Timestamp;
use profluens_core::weave::MAX_INTERLEAVE;

use super::gop::{self, How};
use super::map;
use super::model::{Answer, App};
use super::offers::{Fixed, Plan};
use super::oracle::{self, End, Pacing};
use super::pads::{FakeError, IN};
use super::rig::{self, BuildError, Rig, START};
use super::scenario::{MAX_CUES, MAX_FAULTS, MAX_SINKS, Scenario, Told};
use super::sink;
use super::trace::{AUTO, Did, Trace};
use super::verdict::{Detail, Finding, Kind, NO_ROW, Verdict, Waits};
use crate::action::{ActionKind, Anchor, MAX_ACTIONS, anchor_met_at};
use crate::media::MediaSpec;
use crate::prng::Prng;
use crate::record::{self, Recording, Row, RowKind, class};
use crate::time::NONE;

/// Groups the driver tracks. The family's widest topology has four.
const MAX_GROUPS: usize = 8;
const DEFAULT_ROWS: usize = 32_768;
const NO_GEN: u32 = u32::MAX;
const NO_STEP: u32 = u32::MAX;

fn lag_of(anchor: Anchor) -> u32 {
    match anchor {
        Anchor::AfterEos { lag, .. } => lag as u32,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Digest of the recording and the pass trace. One seed gives one value.
    pub checksum: u64,
    pub end: End,
    pub steps: u32,
    pub passes: u32,
    pub cuts: u32,
    pub jumps: u32,
    pub rows: u32,
    pub taken: u32,
    pub refused: u32,
    pub seeks: u32,
    pub skips: u32,
    pub rates: u32,
    pub offsets: u32,
    pub retimes: u32,
    /// Bit `n` set when action `n` fired.
    pub fired: u64,
}

/// Where a run's time went, summed since [`Worker::time_phases`]. Timing adds four clock reads a
/// pass, so read the split, not the total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Phases {
    pub build: Duration,
    pub driver: Duration,
    pub passes: Duration,
    pub recording: Duration,
    pub checks: Duration,
    pub teardown: Duration,
    pub sweep: Duration,
    pub runs: u64,
}

#[derive(Clone, Copy)]
struct SinkLive {
    seek_gen: u32,
    paused: bool,
    ended_gen: u32,
    /// The front buffer was not due when its group went idle, so it is taken on its deadline.
    waits: bool,
    offset: i64,
    shifts: u16,
    taken: u64,
}

struct Live {
    app: App,
    step: u32,
    fired: u64,
    answers: [Answer; MAX_ACTIONS],
    armed: [u32; MAX_ACTIONS],
    sinks: [SinkLive; MAX_SINKS],
    group_sinks: [u8; MAX_GROUPS],
    plan: Plan,
    failed: bool,
    rejected: bool,
    bad_seek: bool,
    meddled: bool,
    standing: [Option<Before>; MAX_GROUPS],
    rng: Prng,
    /// A stream of its own, so a scenario that cuts none picks its groups as it always did.
    cutter: Prng,
    passes: u32,
    cuts: u32,
    jumps: u32,
    refused: u32,
    shifts: [u16; MAX_SINKS],
    retimes: u32,
    asked: Option<(u64, u64)>,
    made: u64,
    probed: u32,
    limits: [u64; MAX_SINKS],
}

#[derive(Clone, Copy)]
struct Before {
    seek_gen: u32,
    paused: bool,
    over: bool,
    meddled: bool,
    shifts: [u16; MAX_SINKS],
}

pub struct Worker {
    rec: Recording,
    dec: Recording,
    steps: Box<[u32]>,
    /// Shared with the fake source of the run in flight, free again once it is torn down.
    spec: Arc<MediaSpec>,
    declared: MediaSpec,
    trace: Trace,
    answers: [Answer; MAX_ACTIONS],
    app: App,
    phases: Option<Phases>,
    lap_from: Option<Instant>,
}

fn waits_of(outcome: Option<Outcome>) -> Waits {
    match outcome {
        None => Waits::Unrun,
        Some(Outcome::Progressed) => Waits::Progressed,
        Some(Outcome::Idle { until, .. }) => Waits::Idle { until: until.0 },
        Some(Outcome::Blocked { on: BlockedOn::Ring { pad } }) => Waits::BlockedRing { pad: pad.0 },
        Some(Outcome::Blocked { on: BlockedOn::Pool { element } }) => {
            Waits::BlockedPool { element: element.0 }
        }
        Some(Outcome::Blocked { on: BlockedOn::Io }) => Waits::BlockedIo,
        Some(Outcome::Exit) => Waits::Exited,
    }
}

fn failure_of(error: &Error) -> Detail {
    if let Error::Format(FormatError::AnnouncementRejected(r)) = error {
        return Detail::Element { element: r.element.0, tap_full: false };
    }
    let Error::ElementFailed { element, source, .. } = error else {
        return Detail::None;
    };
    let tap_full = matches!(source.downcast_ref::<FakeError>(), Some(FakeError::TapFull { .. }));
    Detail::Element { element: element.0, tap_full }
}

fn announced_id(text: &str) -> Option<u8> {
    text.rsplit_once("id=")?.1.parse().ok()
}

/// `Some(true)` when the plan says that sink turns the format away and the error names it.
/// `Some(false)` is a rejection nothing explains.
fn rejection(sc: &Scenario, plan: &Plan, rig: &Rig, error: &Error) -> Option<bool> {
    let Error::Format(FormatError::AnnouncementRejected(r)) = error else {
        return None;
    };
    let sink = rig.sinks.iter().take(sc.sink_count()).position(|id| *id == r.element);
    Some(sink.is_some_and(|sink| {
        let named = r.pad == IN && r.offers.len() == sc.offers.sinks[sink].offers().len();
        named && announced_id(&r.announced).is_some_and(|id| plan.turns_away(sink, id))
    }))
}

fn unbuilt(error: &BuildError) -> Finding {
    let open = Fixed::OPEN;
    let about = |link: &u8, want: Fixed, got: Fixed| Detail::Link { link: *link, want, got };
    let (kind, detail) = match error {
        BuildError::Schedule(_) | BuildError::Core(_) => (Kind::BuildFailed, Detail::None),
        BuildError::Refused { link, want } => (Kind::LinkRefused, about(link, *want, open)),
        BuildError::Made { link, got } => (Kind::LinkMade, about(link, open, *got)),
        BuildError::Format { link, want, got } => (Kind::LinkFormat, about(link, *want, *got)),
        BuildError::Unnamed { link } => (Kind::RefusalUnnamed, about(link, open, open)),
    };
    Finding::run(kind, detail)
}

fn net_kind(error: &Error) -> Kind {
    let fake = match error {
        Error::ElementFailed { source, .. } => source.downcast_ref::<FakeError>().copied(),
        _ => None,
    };
    match fake {
        Some(FakeError::NetBytes { .. }) => Kind::NetBytes,
        Some(FakeError::NetStale) => Kind::NetStale,
        _ => Kind::UnexpectedError,
    }
}

fn missing_reference(error: &Error) -> bool {
    let Error::ElementFailed { source, .. } = error else {
        return false;
    };
    matches!(source.downcast_ref::<FakeError>(), Some(FakeError::MissingReference { .. }))
}

#[derive(Clone, Copy)]
enum Carry {
    Seek { to_time: u64, to_byte: u64, at: u64 },
    Skip { to_time: u64, from: u64 },
    Nothing,
}

fn declared(sc: &Scenario, error: &Error) -> bool {
    let Error::ElementFailed { source, .. } = error else {
        return false;
    };
    let fake = source.downcast_ref::<FakeError>();
    sc.declares_error() && fake.is_some_and(|why| why.declared())
}

impl Live {
    fn new(sc: &Scenario) -> Live {
        let (waits, offset, shifts) = (false, 0, 0);
        let sink = SinkLive {
            seek_gen: 0,
            paused: false,
            ended_gen: NO_GEN,
            waits,
            offset,
            shifts,
            taken: 0,
        };
        let mut app = App::started(START, sc.time.retimer.on, sc.sink_count() as u16);
        // A run that starts paused has that pause as the first command in its log.
        if sc.start_paused {
            app.apply(START, ActionKind::Pause, Answer::Taken);
        }
        Live {
            app,
            step: 0,
            fired: 0,
            answers: [Answer::Unfired; MAX_ACTIONS],
            armed: [NO_STEP; MAX_ACTIONS],
            sinks: [sink; MAX_SINKS],
            group_sinks: [0; MAX_GROUPS],
            plan: sc.plan(),
            failed: false,
            rejected: false,
            bad_seek: false,
            meddled: false,
            standing: [None; MAX_GROUPS],
            rng: Prng::new(sc.seed).substream_named("driver.pick"),
            cutter: Prng::new(sc.seed).substream_named("driver.cut"),
            passes: 0,
            cuts: 0,
            jumps: 0,
            refused: 0,
            shifts: [0; MAX_SINKS],
            retimes: 0,
            asked: None,
            made: NONE,
            probed: 0,
            limits: [0; MAX_SINKS],
        }
    }

    fn over(&self) -> bool {
        self.app.finished || self.app.stopped || self.failed
    }

    fn answered(
        &mut self,
        index: usize,
        kind: ActionKind,
        said: Answer,
        now: u64,
    ) -> Result<(), Finding> {
        self.explained(index, kind, said)?;
        self.app.apply(now, kind, said);
        if let (ActionKind::SetOffset { sink, .. }, Answer::Taken) = (kind, said)
            && let Some(count) = self.shifts.get_mut(sink as usize)
        {
            *count += 1;
        }
        self.mark(index, said);
        Ok(())
    }

    fn explained(&self, index: usize, kind: ActionKind, said: Answer) -> Result<(), Finding> {
        let detail = Detail::Action { index: index.min(u8::MAX as usize) as u8, answer: said };
        // What the driver itself held back was never put to the log.
        match said != Answer::NotDriven && !self.app.explains(kind, said) {
            true => Err(Finding::run(Kind::Refusal, detail)),
            false => Ok(()),
        }
    }

    fn mark(&mut self, index: usize, said: Answer) {
        self.refused += (said != Answer::Taken) as u32;
        if let Some(slot) = self.answers.get_mut(index) {
            *slot = said;
            self.fired |= 1 << index;
        }
    }

    fn note(
        &mut self,
        sc: &Scenario,
        (index, row): (usize, &Row),
        (over, now): (bool, u64),
    ) -> Result<Option<usize>, Finding> {
        if over && row.kind != RowKind::Stop {
            return Err(Finding::at_row(Kind::RowAfterEnd, index, row, 0, 0));
        }
        let sink = row.sink as usize;
        let (Some(live), Some(decl)) = (self.sinks.get_mut(sink), sc.sinks.get(sink)) else {
            let sinks = sc.sink_count() as u64;
            return Err(Finding::at_row(Kind::Lifecycle, index, row, sinks, sink as u64));
        };
        match row.kind {
            RowKind::FlushStart => {
                (live.seek_gen, live.waits, live.taken) = (row.seek_gen, false, 0)
            }
            RowKind::Eos => live.ended_gen = row.seek_gen,
            RowKind::Paused => live.paused = true,
            RowKind::Resumed => live.paused = false,
            RowKind::Offset => (live.offset, live.shifts) = (row.aux as i64, live.shifts + 1),
            RowKind::Command => return Ok(Some(row.seq as usize)),
            kind if kind.is(class::ARRIVAL) => {
                if row.pts != NONE {
                    live.taken = live.taken.max(row.pts);
                }
                // Only the buffer that was at the front had waited.
                let waited = std::mem::take(&mut live.waits);
                let (profile, latency) = (decl.profile, sc.latency(sink));
                let pacing = Pacing { profile, latency, offset: live.offset, waited };
                let off = |(kind, want, got)| Finding::at_row(kind, index, row, want, got);
                oracle::on_the_map(&self.app.map, &pacing, row, now).map_err(off)?;
            }
            _ => {}
        }
        Ok(None)
    }
}

impl Worker {
    pub fn new() -> Worker {
        Worker::with_rows(DEFAULT_ROWS)
    }

    pub fn with_rows(rows: usize) -> Worker {
        Worker {
            rec: Recording::with_capacity(rows),
            dec: Recording::with_capacity(rows),
            steps: vec![0u32; rows].into_boxed_slice(),
            spec: Arc::new(MediaSpec::with_capacity(MAX_CUES, MAX_FAULTS)),
            declared: MediaSpec::with_capacity(MAX_CUES, MAX_FAULTS),
            trace: Trace::new(),
            answers: [Answer::Unfired; MAX_ACTIONS],
            app: App::new(),
            phases: None,
            lap_from: None,
        }
    }

    pub fn rows(&self) -> &[Row] {
        self.rec.rows()
    }

    pub fn decoded(&self) -> &[Row] {
        self.dec.rows()
    }

    pub fn step_of(&self, row: usize) -> Option<u32> {
        (row < self.rec.len()).then(|| self.steps.get(row).copied()).flatten()
    }

    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn answers(&self) -> &[Answer; MAX_ACTIONS] {
        &self.answers
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn declared(&self) -> &MediaSpec {
        &self.declared
    }

    /// Timing reads the wall clock, so it is off unless a benchmark asks.
    pub fn time_phases(&mut self, on: bool) {
        self.phases = on.then(Phases::default);
    }

    pub fn phases(&self) -> Option<Phases> {
        self.phases
    }

    // The worker times its own phases when asked. No run reads it.
    #[allow(clippy::disallowed_methods)]
    fn lap(&mut self, phase: fn(&mut Phases) -> &mut Duration) {
        let Some(phases) = self.phases.as_mut() else {
            return;
        };
        let now = Instant::now();
        if let Some(from) = self.lap_from.replace(now) {
            *phase(phases) += now.duration_since(from);
        }
    }

    fn declare(&mut self, sc: &Scenario) -> Result<(), Finding> {
        let bad = |_| Finding::run(Kind::BadScenario, Detail::None);
        sc.source.fill(&mut self.declared).map_err(bad)?;
        // The last run's fake source is gone, so this is the only handle. If not, the run gets its
        // own table.
        if Arc::get_mut(&mut self.spec).is_none() {
            self.spec = Arc::new(MediaSpec::with_capacity(MAX_CUES, MAX_FAULTS));
        }
        match Arc::get_mut(&mut self.spec) {
            Some(spec) => sc.source.fill(spec).map_err(bad)?,
            None => return Err(Finding::run(Kind::BadScenario, Detail::None)),
        }
        Ok(())
    }

    fn drain_decoded(&mut self, sc: &Scenario, rig: &Rig, live: &mut Live) -> Result<(), Finding> {
        let Some(tap) = rig.decoded.as_ref() else {
            return Ok(());
        };
        let now = rig.clock.now().0;
        let at = now.saturating_sub(START);
        // A tap holds a fixed number of rows, which bounds the loop.
        for _ in 0..=tap.capacity() {
            let Some(row) = tap.try_pop() else { break };
            match row.kind {
                RowKind::RateAsked if sc.time.retimer.on => {
                    live.asked = control_bound(sc, live, now)
                }
                RowKind::FlushStart => (live.asked, live.made) = (None, NONE),
                RowKind::Retimed if live.made == NONE && row.format > 0 => live.made = row.pts,
                _ => {}
            }
            if row.kind == RowKind::Retime {
                // One from a group short of the newest seek speaks of the stream before it, and
                // core drops it.
                let current = row.seek_gen == live.app.seek_gen;
                let Some(rate) = map::Rate::unpack(row.aux) else {
                    let detail = Detail::Row { row, want: 0, got: row.aux };
                    return Err(Finding::run(Kind::Retimed, detail));
                };
                live.app.map.retime(now, row.pts, rate, current);
                live.retimes += 1;
                // What the retimer made when asked stands in the limited link, so the rate is heard
                // within the limit of where it was asked.
                if let Some((by, reach)) = live.asked.take() {
                    let first = if live.made == NONE { row.pts } else { live.made };
                    let by = by.max(first.saturating_add(reach));
                    if row.pts != NONE && row.pts > by {
                        return Err(Finding::numbers(Kind::ControlLate, by, row.pts));
                    }
                }
            }
            if !self.dec.push(Row { at, ..row }) {
                let capacity = self.dec.capacity() as u64;
                return Err(Finding::numbers(Kind::RecordingOverflow, capacity, capacity + 1));
            }
        }
        Ok(())
    }

    fn carry(
        &self,
        sc: &Scenario,
        (rig, run, tap): Seen<'_>,
        to: u64,
        how: How,
    ) -> (Carry, bool, Option<Answer>) {
        let keyframe = match how {
            How::RawSkip => {
                return (Carry::Skip { to_time: to, from: NONE }, false, None);
            }
            How::RawSeek { pick } => {
                let to_byte = gop::raw_byte(&sc.pictures(), pick, to);
                return (Carry::Seek { to_time: to, to_byte, at: to }, false, None);
            }
            How::Planned { keyframe } => keyframe,
        };
        let mode = if keyframe { SeekMode::Keyframe } else { SeekMode::Accurate };
        let position = tap.now();
        let known =
            tap.index(rig.source).is_some_and(|index| index.keyframe(Timestamp(to)).is_some());
        let (plan, said) = match run.control().seek(Timestamp(to), mode) {
            Ok(plan) => (plan, Answer::Taken),
            Err(e) => (
                run.control().plan_from(position, Timestamp(to), mode),
                sink::answer(Err::<u64, CommandError>(e)),
            ),
        };
        let carry = match plan {
            SeekPlan::Skip { to_time } => {
                Carry::Skip { to_time: to_time.0, from: if known { position.0 } else { NONE } }
            }
            SeekPlan::Seek { to_byte, key, to_time } => {
                Carry::Seek { to_time: to_time.0, to_byte, at: key.0 }
            }
            SeekPlan::Unseekable | SeekPlan::Deferred => Carry::Nothing,
        };
        (carry, true, Some(said))
    }

    fn jump(
        &mut self,
        sc: &Scenario,
        seen: Seen<'_>,
        live: &mut Live,
        (index, to, how): (usize, u64, How),
    ) -> Result<(), Finding> {
        let kind = ActionKind::Seek { to };
        let (carry, planned, sent) = match live.app.room_for_seek() {
            true => self.carry(sc, seen, to, how),
            false => (Carry::Nothing, true, None),
        };
        let (control, now) = (seen.1.control(), seen.0.clock.now().0);
        let said = match (carry, sent) {
            (Carry::Nothing, _) => {
                live.mark(index, Answer::NotDriven);
                return Ok(());
            }
            (_, Some(said)) => said,
            (Carry::Seek { to_time, to_byte, at }, None) => sink::answer(
                control.seek_bytes(Timestamp(to_time), &[(ALL, to_byte, Timestamp(at))]),
            ),
            (Carry::Skip { to_time, .. }, None) => {
                sink::answer(control.append(Command::SkipTo { to_time: Timestamp(to_time) }))
            }
        };
        self.trace.push(live.step, Did::Sent { action: index as u16, answer: said });
        live.explained(index, kind, said)?;
        match carry {
            Carry::Seek { to_time, to_byte, .. } if said == Answer::Taken => {
                let byte = if sc.gop.resyncs() { sc.pictures().resync(to_byte) } else { to_byte };
                live.app.sought(now, to_time, byte, planned)
            }
            Carry::Skip { to_time, from } if said == Answer::Taken && planned => {
                live.app.skipped_from(now, to_time, from)
            }
            Carry::Skip { to_time, .. } if said == Answer::Taken => {
                live.app.skipped(now, to_time, planned)
            }
            _ => {}
        }
        live.mark(index, said);
        Ok(())
    }

    fn tell(
        &mut self,
        sc: &Scenario,
        (rig, run, _): Seen<'_>,
        live: &mut Live,
        (index, to, told): (usize, u64, Told),
    ) -> Result<(), Finding> {
        let (kind, now, reads) = (ActionKind::Seek { to }, rig.clock.now().0, sc.reads(to));
        if !live.app.room_for_seek() {
            live.mark(index, Answer::NotDriven);
            return Ok(());
        }
        let (control, at) = (run.control(), Timestamp(to));
        let target = |n: usize| (rig.sources[n], reads[n].0, Timestamp(reads[n].1));
        // A source told nothing starts over from its first byte.
        let (said, bytes) = match told {
            Told::Each => {
                (control.seek_bytes(at, &[target(0), target(1)]), [reads[0].0, reads[1].0])
            }
            Told::First => (control.seek_bytes(at, &[target(0)]), [reads[0].0, 0]),
            Told::Second => (control.seek_bytes(at, &[target(1)]), [0, reads[1].0]),
            Told::All => (
                control.seek_bytes(at, &[(ALL, reads[0].0, Timestamp(reads[0].1))]),
                [reads[0].0; 2],
            ),
            // Never a skip, the model takes it as a seek of each source.
            Told::Planned => {
                let said = match control.seek_from(Timestamp(NONE), at, SeekMode::Accurate) {
                    Ok(SeekPlan::Seek { .. }) => Ok(0),
                    Ok(_) => Err(CommandError::NoSuchElement),
                    Err(e) => Err(e),
                };
                (said, [reads[0].0, reads[1].0])
            }
        };
        let said = sink::answer(said);
        self.trace.push(live.step, Did::Sent { action: index as u16, answer: said });
        live.explained(index, kind, said)?;
        if said == Answer::Taken {
            live.app.sought_each(now, to, bytes, true);
        }
        live.mark(index, said);
        Ok(())
    }

    fn drain(
        &mut self,
        sc: &Scenario,
        rig: &Rig,
        live: &mut Live,
        mask: u8,
        over: bool,
    ) -> Result<(), Finding> {
        if mask == 0 {
            return Ok(());
        }
        let now = rig.clock.now().0;
        let at = now.saturating_sub(START);
        for (sink, tap) in rig.taps.iter().enumerate() {
            let Some(tap) = tap.as_ref().filter(|_| mask & (1 << sink) != 0) else {
                continue;
            };
            for _ in 0..=tap.capacity() {
                let Some(row) = tap.try_pop() else { break };
                let row = Row { at, ..row };
                let index = self.rec.len();
                if !self.rec.push(row) {
                    let capacity = self.rec.capacity() as u64;
                    return Err(Finding::numbers(Kind::RecordingOverflow, capacity, capacity + 1));
                }
                if let Some(slot) = self.steps.get_mut(index) {
                    *slot = live.step;
                }
                let Some(fired) = live.note(sc, (index, &row), (over, now))? else {
                    continue;
                };
                let Some(action) = sc.script.as_slice().get(fired) else {
                    continue;
                };
                let said = Answer::from_code(row.aux);
                self.trace.push(live.step, Did::Sent { action: fired as u16, answer: said });
                live.meddled = true;
                live.answered(fired, action.kind, said, now)?;
            }
        }
        Ok(())
    }

    fn fire(
        &mut self,
        sc: &Scenario,
        seen: Seen<'_>,
        live: &mut Live,
        index: usize,
    ) -> Result<(), Finding> {
        let Some(action) = sc.script.as_slice().get(index) else {
            return Ok(());
        };
        match (action.kind, sc.how(action), sc.told(action)) {
            (ActionKind::Seek { to }, Some(how), _) => {
                self.jump(sc, seen, live, (index, to, how))?
            }
            (ActionKind::Seek { to }, None, Some(told)) => {
                self.tell(sc, seen, live, (index, to, told))?
            }
            (kind, ..) => self.send(sc, seen, live, (index, kind))?,
        }
        probe(seen, live)
    }

    fn send(
        &mut self,
        sc: &Scenario,
        (rig, run, _): Seen<'_>,
        live: &mut Live,
        (index, kind): (usize, ActionKind),
    ) -> Result<(), Finding> {
        let seek = matches!(kind, ActionKind::Seek { .. });
        let said = match kind {
            _ if seek && !live.app.room_for_seek() => Answer::NotDriven,
            ActionKind::SetOffset { sink, by } => {
                // A number past the last sink names the source, which presents nothing.
                let named =
                    rig.sinks.get(sink as usize).filter(|_| (sink as usize) < sc.sink_count());
                let element = named.copied().unwrap_or(rig.source);
                sink::answer(run.control().append(Command::SetOffset { element, by }))
            }
            _ => sink::send(run.control(), kind),
        };
        self.trace.push(live.step, Did::Sent { action: index as u16, answer: said });
        live.answered(index, kind, said, rig.clock.now().0)
    }

    fn fire_due(&mut self, sc: &Scenario, seen: Seen<'_>, live: &mut Live) -> Result<(), Finding> {
        let rig = seen.0;
        let script = sc.script.as_slice();
        let all = u64::MAX >> (MAX_ACTIONS - script.len().min(MAX_ACTIONS)).min(63);
        if script.is_empty() || live.fired & all == all {
            return Ok(());
        }
        let now = rig.clock.now().0.saturating_sub(START);
        for (index, action) in script.iter().enumerate() {
            let pending = live.fired & (1 << index) == 0;
            if !pending || !anchor_met_at(action.anchor, now, Some(live.step), &self.rec) {
                continue;
            }
            let armed = live.armed[index].min(live.step);
            live.armed[index] = armed;
            if live.step - armed >= lag_of(action.anchor) {
                self.fire(sc, seen, live, index)?;
            }
        }
        Ok(())
    }
}

impl Default for Worker {
    fn default() -> Self {
        Self::new()
    }
}

type Seen<'a> = (&'a Rig, &'a Stepped, &'a TapHandle);

fn probe((rig, _, tap): Seen<'_>, live: &mut Live) -> Result<(), Finding> {
    let map = &live.app.map;
    if std::mem::replace(&mut live.probed, map.version) == map.version {
        return Ok(());
    }
    let (want, got) = (map.position(rig.clock.now().0), tap.now().0);
    if want != got {
        return Err(Finding::numbers(Kind::Position, want, got));
    }
    let (new, old) =
        (map.newest().running_start, map.pieces().first().map_or(0, |p| p.running_start));
    let far = new.saturating_add(1_000_000_007);
    for running in [new.saturating_sub(1), new, new.saturating_add(1), far, old.saturating_sub(3)] {
        let (want, got) = (map.stream(running), tap.stream_time(Timestamp(running)).0);
        if want != got {
            return Err(Finding::numbers(Kind::Position, want, got));
        }
    }
    Ok(())
}

fn pick(sc: &Scenario, run: &Stepped, live: &mut Live) -> Option<usize> {
    let groups = run.groups().min(MAX_GROUPS);
    let (mut ready, mut idle) = ([0usize; MAX_GROUPS], [0usize; MAX_GROUPS]);
    let (mut ready_count, mut idle_count) = (0, 0);
    for g in 0..groups {
        if run.ready(g) {
            ready[ready_count] = g;
            ready_count += 1;
        } else if run.outcome(g) != Some(Outcome::Exit) {
            idle[idle_count] = g;
            idle_count += 1;
        }
    }
    if sc.spurious > 0 && live.rng.below(sc.spurious as u64) == 0 && idle_count > 0 {
        return Some(idle[live.rng.below(idle_count as u64) as usize]);
    }
    (ready_count > 0).then(|| ready[live.rng.below(ready_count as u64) as usize])
}

/// The checks after a pass of group `g`.
fn check_pass(
    run: &Stepped,
    tap: &TapHandle,
    live: &mut Live,
    before: Before,
    g: usize,
    (exited, failed): (bool, bool),
) -> Result<(), Finding> {
    let group = Detail::Group { group: g as u16, waits: waits_of(run.outcome(g)), held: 0 };
    if before.over && !exited {
        return Err(Finding::run(Kind::PassAfterEnd, group));
    }
    if !before.over && exited && !failed {
        return Err(Finding::run(Kind::LeftEarly, group));
    }
    check_finish(run, live)?;
    check_pool(run, tap)?;
    if exited {
        return Ok(());
    }
    check_applied(live, before, g)
}

/// The stamp by which a retimer asked for a rate at `now` has to start it. The bound is the limit,
/// the lag and eight buffers past the sink.
fn control_bound(sc: &Scenario, live: &Live, now: u64) -> Option<(u64, u64)> {
    let (lag, position) = (sc.time.retimer.lag as u64, live.app.map.position(now));
    let of = |sink: usize| {
        let (decl, step) = (&sc.sinks[sink], sc.sink_step(sink)?);
        let (p, early) = (&decl.profile, live.sinks[sink].offset.unsigned_abs());
        let stands =
            (position + p.ring_depth + p.device_latency + early).max(live.sinks[sink].taken);
        let limit = decl.limit.max(live.limits[sink]);
        let reach = limit + (lag + 2 * super::retime::MAX_OUT as u64) * step;
        (sc.retimed(sink) && p.paced && decl.limit > 0).then_some((stands + reach, reach))
    };
    (0..sc.sink_count()).filter_map(of).min()
}

/// How far over its limit the link into `sink` may be, in stream time. Inputs overshoot by a
/// buffer, a source by its burst, a holding transform by what it holds, a fast retimer by its
/// skips.
fn overshoot(sc: &Scenario, sink: usize) -> u64 {
    let held_back = if sc.topology.through_transform(sink) { sc.transform.delay as u64 } else { 0 };
    let apart = if sc.retimed(sink) { super::retime::MAX_OUT as u64 } else { 1 };
    (sc.source.burst as u64 + held_back).max(1) * apart * sc.sink_step(sink).unwrap_or(0)
}

fn check_levels(sc: &Scenario, rig: &Rig, tap: &TapHandle, live: &mut Live) -> Result<(), Finding> {
    for sink in 0..sc.sink_count() {
        let declared = sc.sinks[sink].limit;
        // A sparse link never rests its tail by time, so its limit bounds nothing.
        if declared == 0 || sc.sparse(sink) {
            continue;
        }
        let now =
            tap.link_limit(rig.sinks[sink], IN).and_then(|l| l.time.nanos()).unwrap_or(declared);
        let cap = declared.max(MAX_INTERLEAVE.0);
        if now > cap {
            let detail = Detail::Numbers { want: cap, got: now };
            return Err(Finding {
                kind: Kind::LinkOverCap,
                sink: sink as u16,
                row: NO_ROW,
                detail,
            });
        }
        live.limits[sink] = live.limits[sink].max(now);
        let limit = live.limits[sink];
        let held = tap.queued(rig.sinks[sink], IN).map_or(0, |held| held.time.nanos().unwrap_or(0));
        if held > limit + overshoot(sc, sink) {
            let detail = Detail::Numbers { want: limit, got: held };
            return Err(Finding {
                kind: Kind::LinkOverLimit,
                sink: sink as u16,
                row: NO_ROW,
                detail,
            });
        }
    }
    Ok(())
}

fn check_pool(run: &Stepped, tap: &TapHandle) -> Result<(), Finding> {
    // Read after every pass, so it reads two numbers a pool and not all seven.
    let (outstanding, max_slots, over) = tap.pool_use();
    if over || outstanding > max_slots {
        return Err(Finding::numbers(Kind::PoolOverCap, max_slots, outstanding));
    }
    let held: usize = (0..run.groups()).map(|g| run.held(g)).sum();
    if held as u64 > outstanding {
        return Err(Finding::numbers(Kind::PoolOverCap, outstanding, held as u64));
    }
    Ok(())
}

fn check_finish(run: &Stepped, live: &mut Live) -> Result<(), Finding> {
    let finished = run.control().is_finished();
    if live.app.finished && !finished {
        return Err(Finding::run(Kind::FinishTakenBack, Detail::None));
    }
    if !finished || live.app.finished {
        return Ok(());
    }
    if live.app.stopped {
        return Err(Finding::run(Kind::EndUnclear, Detail::None));
    }
    live.app.finished = true;
    let newest = live.app.seek_gen;
    for (sink, s) in live.sinks.iter().enumerate() {
        let in_run = live.group_sinks.iter().any(|mask| mask & (1 << sink) != 0);
        if in_run && s.ended_gen != newest {
            let detail = Detail::Numbers { want: newest as u64, got: s.ended_gen as u64 };
            return Err(Finding {
                kind: Kind::FinishedEarly,
                sink: sink as u16,
                row: NO_ROW,
                detail,
            });
        }
    }
    Ok(())
}

/// After a pass the group's sinks are in the generation of the newest seek logged before it. A
/// cut pass that met a seek in the gap applied the whole log, so it is at the newest instead.
fn check_applied(live: &Live, before: Before, g: usize) -> Result<(), Finding> {
    let mask = live.group_sinks.get(g).copied().unwrap_or(0);
    let top = (before.seek_gen, before.paused);
    let now = (live.app.seek_gen, live.app.paused);
    for (sink, s) in live.sinks.iter().enumerate() {
        if mask & (1 << sink) == 0 {
            continue;
        }
        let about = |want, got| Finding {
            kind: Kind::CommandNotApplied,
            sink: sink as u16,
            row: NO_ROW,
            detail: Detail::Numbers { want, got },
        };
        if s.seek_gen < before.seek_gen || s.seek_gen > live.app.seek_gen {
            return Err(about(before.seek_gen as u64, s.seek_gen as u64));
        }
        let is = (s.seek_gen, s.paused);
        let caught_up = is == now && now.0 > top.0;
        if !before.meddled && is != top && !caught_up {
            return Err(about(before.paused as u64, s.paused as u64));
        }
        let (least, most) = (before.shifts[sink], live.shifts[sink]);
        if s.shifts < least || s.shifts > most {
            return Err(about(least as u64, s.shifts as u64));
        }
    }
    Ok(())
}

fn idle(
    worker: &mut Worker,
    sc: &Scenario,
    seen: Seen<'_>,
    live: &mut Live,
) -> Result<bool, Finding> {
    let (rig, run, _) = seen;
    let now = rig.clock.now().0;
    let mut to = run.next_deadline().map_or(NONE, |t| t.0);
    let mut stepped: Option<(u32, usize)> = None;
    for (index, action) in sc.script.as_slice().iter().enumerate() {
        let at = match action.anchor {
            _ if live.fired & (1 << index) != 0 => continue,
            Anchor::At(t) => {
                to = to.min(START.saturating_add(t));
                continue;
            }
            Anchor::AtStep(at) => at,
            _ if live.armed[index] != NO_STEP => live.armed[index],
            _ => continue,
        };
        if stepped.is_none_or(|(least, _)| at < least) {
            stepped = Some((at, index));
        }
    }
    if to != NONE {
        if to <= now {
            return Err(Finding::numbers(Kind::DeadlineBehind, now + 1, to));
        }
        rig.clock.advance(Timestamp(to - now));
        live.jumps += 1;
        worker.trace.push(live.step, Did::Jump { to });
        return Ok(true);
    }
    if let Some((_, index)) = stepped {
        return worker.fire(sc, seen, live, index).map(|()| true);
    }
    if live.app.paused && !live.over() {
        let said = sink::send(run.control(), ActionKind::Resume);
        worker.trace.push(live.step, Did::Sent { action: AUTO, answer: said });
        live.answered(MAX_ACTIONS, ActionKind::Resume, said, now)?;
        return probe(seen, live).map(|()| true);
    }
    Ok(false)
}

fn settle(
    worker: &mut Worker,
    sc: &Scenario,
    (rig, run, tap): (&Rig, &Stepped, &TapHandle),
    live: &mut Live,
    (g, before): (usize, Before),
    result: Result<Option<Outcome>, Error>,
) -> Result<(), Finding> {
    let (failed, stands) = (result.is_err(), matches!(result, Ok(None)));
    let did = match stands {
        true => Did::Top { group: g as u16 },
        false => Did::Pass { group: g as u16, waits: waits_of(run.outcome(g)), failed },
    };
    worker.trace.push(live.step, did);
    if let Err(error) = &result {
        match rejection(sc, &live.plan, rig, error) {
            Some(true) => live.rejected = true,
            Some(false) => return Err(Finding::run(Kind::Rejection, failure_of(error))),
            None if declared(sc, error) => {}
            // Only a seek the rule would not send may leave the decoder like that.
            None if missing_reference(error) && live.app.raw => live.bad_seek = true,
            None if missing_reference(error) => {
                return Err(Finding::run(Kind::MissingReference, failure_of(error)));
            }
            None => return Err(Finding::run(net_kind(error), failure_of(error))),
        }
    }
    let mask = live.group_sinks.get(g).copied().unwrap_or(0);
    worker.drain(sc, rig, live, mask, before.over)?;
    worker.drain_decoded(sc, rig, live)?;
    worker.lap(|p| &mut p.recording);
    // A sink left idle until a deadline holds a buffer that is not due yet.
    let waits = matches!(run.outcome(g), Some(Outcome::Idle { until, .. }) if until.is_some());
    for (sink, s) in live.sinks.iter_mut().enumerate() {
        if !stands && mask & (1 << sink) != 0 {
            s.waits = waits;
        }
    }
    probe((rig, run, tap), live)?;
    // A failed group stops the pipeline, as a stop the app did not send.
    live.failed |= failed;
    live.app.stopped |= failed;
    let before = Before { meddled: before.meddled || live.meddled, ..before };
    if stands {
        live.cuts += 1;
        if let Some(slot) = live.standing.get_mut(g) {
            *slot = Some(before);
        }
        check_pool(run, tap)?;
    } else {
        live.passes += 1;
        let exited = !matches!(result, Ok(Some(outcome)) if outcome != Outcome::Exit);
        check_pass(run, tap, live, before, g, (exited, failed))?;
    }
    check_levels(sc, rig, tap, live)?;
    worker.lap(|p| &mut p.checks);
    Ok(())
}

fn drive(
    worker: &mut Worker,
    sc: &Scenario,
    rig: &Rig,
    run: &mut Stepped,
    live: &mut Live,
) -> Result<(), Finding> {
    let tap = rig.pipeline.tap_handle();
    for step in 0..sc.step_limit {
        live.step = step;
        worker.fire_due(sc, (rig, &*run, &tap), live)?;
        let Some(g) = pick(sc, run, live) else {
            if idle(worker, sc, (rig, &*run, &tap), live)? {
                continue;
            }
            return Ok(());
        };
        let standing = live.standing.get_mut(g).and_then(Option::take);
        let (seek_gen, paused, shifts) = (live.app.seek_gen, live.app.paused, live.shifts);
        let fresh = Before { seek_gen, paused, over: live.over(), meddled: false, shifts };
        let cut = standing.is_none() && sc.split > 0 && live.cutter.below(sc.split as u64) == 0;
        live.meddled = false;
        worker.lap(|p| &mut p.driver);
        let result = match cut {
            true => run.pass_to(g, Cut::Drained),
            false => run.pass(g).map(Some),
        };
        worker.lap(|p| &mut p.passes);
        let pass = (g, standing.unwrap_or(fresh));
        settle(worker, sc, (rig, &*run, &tap), live, pass, result)?;
    }
    Err(Finding::numbers(Kind::NoQuiescence, sc.step_limit as u64, sc.step_limit as u64))
}

fn check_quiet(run: &Stepped, live: &Live) -> Result<End, Finding> {
    for g in 0..run.groups() {
        let (waits, held) = (waits_of(run.outcome(g)), run.held(g) as u32);
        let detail = Detail::Group { group: g as u16, waits, held };
        if waits != Waits::Exited {
            return Err(Finding::run(Kind::Stall, detail));
        }
        if held != 0 {
            return Err(Finding::run(Kind::HeldAtEnd, detail));
        }
    }
    let end = match (live.failed, live.app.finished, live.app.stopped) {
        (true, false, _) if live.rejected => End::Rejected,
        (true, false, _) if live.bad_seek => End::BadSeek,
        (true, false, _) => End::Failed,
        (false, true, false) => End::Finished,
        (false, false, true) => End::Stopped,
        _ => return Err(Finding::run(Kind::EndUnclear, Detail::None)),
    };
    let last = live.app.taken.checked_sub(1);
    if last.is_some_and(|seq| !run.control().applied_by_all(seq as u64)) {
        return Err(Finding::run(Kind::LogHeld, Detail::None));
    }
    Ok(end)
}

fn check_bus(rig: &Rig, end: End) -> Result<(), Finding> {
    let (mut ends, mut errors) = (0u64, 0u64);
    // Drained whole, the bus is a ring of bounded capacity, so a second Eos or an Error behind a
    // crowd of other messages still counts.
    while let Some(message) = rig.pipeline.bus().try_recv() {
        match message {
            BusMessage::Eos => ends += 1,
            BusMessage::Error { .. } => errors += 1,
            _ => {}
        }
    }
    let failed = matches!(end, End::Failed | End::Rejected | End::BadSeek);
    if (failed && errors == 0) || (!failed && (ends != 1 || errors != 0)) {
        return Err(Finding::numbers(Kind::Bus, 1, ends));
    }
    Ok(())
}

impl Worker {
    fn seat(rig: &Rig, run: &Stepped, live: &mut Live, sinks: usize) {
        for g in 0..run.groups().min(MAX_GROUPS) {
            for sink in 0..sinks.min(MAX_SINKS) {
                if run.elements(g).contains(&rig.sinks[sink]) {
                    live.group_sinks[g] |= 1 << sink;
                }
            }
        }
    }

    fn play(&mut self, sc: &Scenario, live: &mut Live) -> Result<End, Finding> {
        self.declare(sc)?;
        let built = rig::build(sc, &live.plan, Arc::clone(&self.spec));
        let Some(mut rig) = built.map_err(|error| unbuilt(&error))? else {
            return Ok(End::Refused);
        };
        let clock: Arc<dyn Clock> = Arc::new(rig.clock.clone());
        let unstarted = |_| Finding::run(Kind::BuildFailed, Detail::None);
        let mut run = rig.pipeline.step(clock).map_err(unstarted)?;
        Worker::seat(&rig, &run, live, sc.sink_count());
        self.lap(|p| &mut p.build);
        self.drain(sc, &rig, live, u8::MAX, false)?;
        let driven = drive(self, sc, &rig, &mut run, live);
        let end = driven.and_then(|()| check_quiet(&run, live));
        self.lap(|p| &mut p.driver);
        let finished = run.finish();
        let drained = self.drain(sc, &rig, live, u8::MAX, false);
        let end = end?;
        drained?;
        self.drain_decoded(sc, &rig, live)?;
        if finished.is_err() != matches!(end, End::Failed | End::Rejected | End::BadSeek) {
            return Err(Finding::run(Kind::UnexpectedError, Detail::None));
        }
        check_bus(&rig, end)?;
        let pool = rig.pipeline.tap_handle().pool();
        if pool.outstanding != 0 {
            return Err(Finding::numbers(Kind::PoolLeak, 0, pool.outstanding));
        }
        drop(rig);
        self.lap(|p| &mut p.teardown);
        Ok(end)
    }

    // The worker times its own phases when asked. No run reads it.
    #[allow(clippy::disallowed_methods)]
    pub fn run(&mut self, sc: &Scenario) -> Result<Summary, Verdict> {
        self.rec.reset();
        self.dec.reset();
        self.trace.reset();
        self.lap_from = self.phases.is_some().then(Instant::now);
        let mut live = Live::new(sc);
        let played = self.play(sc, &mut live);
        (self.answers, self.app) = (live.answers, live.app);
        let swept = played.and_then(|end| {
            let rows = (self.rec.rows(), self.dec.rows());
            oracle::sweep_decoded(sc, &self.declared, &live.app, end, rows).map(|()| end)
        });
        self.lap(|p| &mut p.sweep);
        if let Some(phases) = self.phases.as_mut() {
            phases.runs += 1;
        }
        let end = swept.map_err(|found| {
            let step = self.step_of(found.row as usize).unwrap_or(live.step);
            Verdict::new(sc.seed, step, found)
        })?;
        let rows = record::checksum(self.rec.rows());
        // A run with no decoder and no retimer keeps the checksum it had before either existed.
        let inner = sc.gop.on || sc.time.retimer.on;
        let decoded = if inner { record::checksum(self.dec.rows()).rotate_left(7) } else { 0 };
        let skips = live.app.jumps().iter().filter(|jump| jump.seek_gen == 0).count();
        Ok(Summary {
            checksum: crate::prng::mix(self.trace.digest() ^ rows.rotate_left(1) ^ decoded),
            end,
            steps: live.step,
            passes: live.passes,
            cuts: live.cuts,
            jumps: live.jumps,
            rows: self.rec.len() as u32,
            taken: live.app.taken,
            refused: live.refused,
            seeks: live.app.seek_gen,
            skips: skips as u32,
            rates: live.app.rates,
            offsets: live.app.shifts().len() as u32,
            retimes: live.retimes,
            fired: live.fired,
        })
    }
}
