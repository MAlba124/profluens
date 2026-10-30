use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64 as Plain};
use std::sync::{Arc, TryLockError};

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(loom)]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::{Mutex, MutexGuard};

use crate::clock::Clock;
use crate::counters::Roster;
use crate::ctx::SeekTarget;
use crate::id::ElementId;
use crate::pipeline::{Pipeline, SchedulerStats, SeekMode, SeekPlan, Seekable};
use crate::station::Station;
use crate::time::{Rate, Timestamp};
use crate::timeline::{Piece, Timeline, clamp};
use crate::topology::Topology;

/// Slots in the command log, a power of two.
pub const COMMAND_LOG_CAP: usize = 64;

/// Where one element reads behind a seek, `(element, to_byte, at)`.
pub type Target = (ElementId, u64, Timestamp);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// Flush and present from `to_time`. The log assigns `seek_gen`.
    Seek {
        seek_gen: u64,
        to_time: Timestamp,
    },
    /// Move the presentation point to `to_time` without a flush.
    SkipTo {
        to_time: Timestamp,
    },
    SetRate {
        rate: Rate,
    },
    /// Present what sink `element` shows `by` ns later, earlier when negative.
    SetOffset {
        element: ElementId,
        by: i64,
    },
    Pause,
    Resume,
    Stop,
    /// Apply step `step` of a topology change.
    Splice {
        step: u32,
    },
}

/// Why a command did not enter the log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandError {
    Full,
    Redundant,
    NotRunning,
    Finished,
    NotASink,
    NoSuchElement,
    OnRunThread,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CommandError::Full => "command log full, a group has not caught up",
            CommandError::Redundant => "the pipeline is already in that state",
            CommandError::NotRunning => "seek, skip or rate before the first run",
            CommandError::Finished => "the run has finished",
            CommandError::NotASink => "an offset for an element that is not a sink of this run",
            CommandError::NoSuchElement => "a seek target for an element that is not in this run",
            CommandError::OnRunThread => "an edit from the thread in run(), which runs the edits",
        })
    }
}

impl std::error::Error for CommandError {}

const SEEK: u64 = 0;
const PAUSE: u64 = 1;
const RESUME: u64 = 2;
const STOP: u64 = 3;
const SKIP: u64 = 4;
const RATE: u64 = 5;
const OFFSET: u64 = 6;
const SPLICE: u64 = 7;

const RETIRED: u64 = u64::MAX;

const FOR_EVERY_SEEK: u64 = u64::MAX;

pub(crate) const PLAYING: u64 = u64::MAX;

#[repr(align(64))]
pub(crate) struct Cursor {
    applied: AtomicU64,
    ended: AtomicU64,
    terminal: AtomicBool,
}

impl Cursor {
    fn new(applied: u64, terminal: bool) -> Cursor {
        let terminal = AtomicBool::new(terminal);
        Cursor { applied: AtomicU64::new(applied), ended: AtomicU64::new(PLAYING), terminal }
    }

    pub(crate) fn get(&self) -> u64 {
        self.applied.load(Ordering::Acquire)
    }

    pub(crate) fn end(&self, seek_gen: u64) {
        self.ended.store(seek_gen, Ordering::Release);
    }

    pub(crate) fn set_terminal(&self, terminal: bool) {
        self.terminal.store(terminal, Ordering::Release);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Finish {
    Done,
    NotYet,
    Busy,
}

fn min_applied(cursors: &[Arc<Cursor>]) -> u64 {
    cursors.iter().map(|c| c.get()).min().unwrap_or(RETIRED)
}

pub(crate) struct CommandLog<const N: usize> {
    head: AtomicU64,
    slots: [[AtomicU64; 4]; N],
    room: Station,
}

impl<const N: usize> CommandLog<N> {
    pub(crate) fn new() -> Self {
        const { assert!(N.is_power_of_two()) };
        let slots = std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0)));
        Self { head: AtomicU64::new(0), slots, room: Station::new() }
    }

    pub(crate) fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    pub(crate) fn read(&self, seq: u64) -> Command {
        let slot = &self.slots[seq as usize & (N - 1)];
        let [kind, seek_gen, to_byte, to_time] = slot.each_ref().map(|w| w.load(Ordering::Relaxed));
        match kind {
            SEEK => Command::Seek { seek_gen, to_time: Timestamp(to_time) },
            SKIP => Command::SkipTo { to_time: Timestamp(to_time) },
            RATE => Command::SetRate { rate: Rate::unpack(to_byte) },
            OFFSET => Command::SetOffset { element: ElementId(to_byte as u32), by: to_time as i64 },
            PAUSE => Command::Pause,
            RESUME => Command::Resume,
            SPLICE => Command::Splice { step: to_byte as u32 },
            _ => Command::Stop,
        }
    }

    pub(crate) fn advance(&self, cursor: &Cursor, applied: u64) {
        // Stored after the reader last read the slots, which frees them for the writer.
        cursor.applied.store(applied, Ordering::Release);
        self.room.signal();
    }

    fn has_room(&self, cursors: &[Arc<Cursor>]) -> bool {
        // A slot is reused only once every cursor has passed it.
        self.head.load(Ordering::Relaxed).saturating_sub(min_applied(cursors)) < N as u64
    }

    fn publish(&self, cmd: Command) -> u64 {
        let seq = self.head.load(Ordering::Relaxed);
        let words = match cmd {
            Command::Seek { seek_gen, to_time } => [SEEK, seek_gen, 0, to_time.0],
            Command::SkipTo { to_time } => [SKIP, 0, 0, to_time.0],
            Command::SetRate { rate } => [RATE, 0, rate.pack(), 0],
            Command::SetOffset { element, by } => [OFFSET, 0, element.0 as u64, by as u64],
            Command::Pause => [PAUSE, 0, 0, 0],
            Command::Resume => [RESUME, 0, 0, 0],
            Command::Stop => [STOP, 0, 0, 0],
            Command::Splice { step } => [SPLICE, 0, step as u64, 0],
        };
        for (slot, word) in self.slots[seq as usize & (N - 1)].iter().zip(words) {
            slot.store(word, Ordering::Relaxed);
        }
        // Release after the slot words, so a reader that sees head reads them whole.
        self.head.store(seq + 1, Ordering::Release);
        seq
    }
}

/// The pipeline's control block, the handle an app drives a run through.
pub struct Control {
    pub(crate) log: CommandLog<COMMAND_LOG_CAP>,
    writer: Mutex<Writer>,
    seek_gen: Plain,
    paused: AtomicBool,
    frozen: AtomicU64,
    stopped: AtomicBool,
    finished: AtomicBool,
    base: Arc<AtomicI64>,
    pub(crate) timeline: Timeline,
    stations: Mutex<Stations>,
    pub(crate) topology: Topology,
    view: std::sync::OnceLock<Roster>,
    /// A seek held until an index lands. Taken before the writer lock, never after.
    pending: std::sync::Mutex<Option<Held>>,
    /// Mirrors `pending`, so a group's pass checks for a held seek without the lock.
    holding: AtomicBool,
    over_at: Plain,
}

/// A seek held for an index, `from` the position it planned from or `None` for the pipeline's.
#[derive(Clone, Copy)]
struct Held {
    from: Option<Timestamp>,
    to: Timestamp,
    mode: SeekMode,
}

struct Writer {
    clock: Option<Arc<dyn Clock>>,
    cursors: Vec<Arc<Cursor>>,
    rate: Rate,
    retimer: Option<ElementId>,
    origin: Timestamp,
    presentation_point: Timestamp,
    seek_applied_at: u64,
    preview_next: bool,
    preview_gen: u64,
    sinks: Vec<u32>,
    known: usize,
    /// Each element's seek target and its generation by id, sorted.
    targets: Vec<(u32, (u64, SeekTarget))>,
    all: (u64, SeekTarget),
}

const START: SeekTarget = SeekTarget { to_byte: 0, at: Timestamp::ZERO };

fn slot(targets: &mut Vec<(u32, (u64, SeekTarget))>, el: u32) -> &mut (u64, SeekTarget) {
    let at = match targets.binary_search_by_key(&el, |t| t.0) {
        Ok(at) => at,
        Err(at) => {
            targets.insert(at, (el, (0, START)));
            at
        }
    };
    &mut targets[at].1
}

/// The target element for every element without a target of its own.
pub const ALL: ElementId = ElementId(u32::MAX);

struct Stations {
    live: Vec<Arc<Station>>,
    retired: SchedulerStats,
}

fn relock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Control {
    // One per pipeline.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new(base: Arc<AtomicI64>) -> Arc<Self> {
        let (zero, rate) = (Timestamp::ZERO, Rate::ONE);
        let (clock, cursors, retimer, sinks) = (None, Vec::new(), None, Vec::new());
        let (targets, all) = (Vec::new(), (0, START));
        let (origin, presentation_point, seek_applied_at) = (zero, zero, 0);
        let writer = Writer {
            clock,
            cursors,
            rate,
            retimer,
            origin,
            presentation_point,
            seek_applied_at,
            preview_next: false,
            preview_gen: 0,
            sinks,
            known: 0,
            targets,
            all,
        };
        Arc::new(Self {
            log: CommandLog::new(),
            writer: Mutex::new(writer),
            seek_gen: Plain::new(0),
            paused: AtomicBool::new(false),
            frozen: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            base,
            timeline: Timeline::new(),
            stations: Mutex::new(Stations { live: Vec::new(), retired: SchedulerStats::default() }),
            topology: Topology::new(),
            view: std::sync::OnceLock::new(),
            pending: std::sync::Mutex::new(None),
            holding: AtomicBool::new(false),
            over_at: Plain::new(0),
        })
    }

    pub(crate) fn set_view(&self, view: Roster) {
        let _ = self.view.set(view);
    }

    /// Append `cmd` and wake every group, or say why the log refused it.
    pub fn append(&self, cmd: Command) -> Result<u64, CommandError> {
        self.append_with(cmd, &[])
    }

    /// Seek to `to` in stream time, by a seek, a skip or held until an index lands. Never blocks.
    pub fn seek(&self, to: Timestamp, mode: SeekMode) -> Result<SeekPlan, CommandError> {
        self.seek_at(None, to, mode)
    }

    /// [`seek`](Self::seek) from a play position the caller keeps. `NONE` never skips.
    pub fn seek_from(
        &self,
        position: Timestamp,
        to: Timestamp,
        mode: SeekMode,
    ) -> Result<SeekPlan, CommandError> {
        self.seek_at(Some(position), to, mode)
    }

    /// A held seek is planned again from the position its request was.
    fn seek_at(
        &self,
        from: Option<Timestamp>,
        to: Timestamp,
        mode: SeekMode,
    ) -> Result<SeekPlan, CommandError> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let planned = self.planned(from.unwrap_or_else(|| self.position()), to, mode);
        if planned.plan == SeekPlan::Deferred && relock(&self.writer).clock.is_none() {
            return Err(CommandError::NotRunning);
        }
        *pending = (planned.plan == SeekPlan::Deferred).then_some(Held { from, to, mode });
        self.holding.store(pending.is_some(), Ordering::Release);
        self.carry(&planned, mode == SeekMode::Preview)?;
        Ok(planned.plan)
    }

    /// What [`seek`](Self::seek) would carry out now, sending nothing.
    pub fn plan(&self, to: Timestamp, mode: SeekMode) -> SeekPlan {
        self.planned(self.position(), to, mode).plan
    }

    /// What [`seek_from`](Self::seek_from) would carry out now, sending nothing.
    pub fn plan_from(&self, position: Timestamp, to: Timestamp, mode: SeekMode) -> SeekPlan {
        self.planned(position, to, mode).plan
    }

    // An empty Vec allocates nothing.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn planned(
        &self,
        position: Timestamp,
        to: Timestamp,
        mode: SeekMode,
    ) -> crate::seeker::Planned {
        let Some(view) = self.view.get() else {
            return crate::seeker::Planned { plan: SeekPlan::Unseekable, targets: Vec::new() };
        };
        // Settled before the counters are read, see the planner's docs.
        let settled = self.settled();
        view.read(|v| crate::seeker::plan(v, settled, position, to, mode))
    }

    fn carry(&self, planned: &crate::seeker::Planned, preview: bool) -> Result<(), CommandError> {
        match planned.plan {
            SeekPlan::Skip { to_time } => self.append(Command::SkipTo { to_time }).map(drop),
            SeekPlan::Seek { to_time, .. } => {
                self.append_seek(to_time, &planned.targets, preview).map(drop)
            }
            SeekPlan::Deferred | SeekPlan::Unseekable => Ok(()),
        }
    }

    /// Plan the held seek again on what was just published, and send it once no longer held.
    pub(crate) fn resolve_pending(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let Some(Held { from, to, mode }) = *pending else {
            return;
        };
        if self.is_stopped() {
            *pending = None;
            self.holding.store(false, Ordering::Release);
            return;
        }
        let planned = self.planned(from.unwrap_or_else(|| self.position()), to, mode);
        if planned.plan == SeekPlan::Deferred {
            return;
        }
        if !matches!(
            self.carry(&planned, mode == SeekMode::Preview),
            Err(CommandError::Full | CommandError::NotRunning)
        ) {
            *pending = None;
            self.holding.store(false, Ordering::Release);
        }
    }

    /// A seek held back by a full log or an index is tried again, from a group that made room.
    pub(crate) fn retry_held(&self) {
        if self.holding.load(Ordering::Acquire) {
            self.resolve_pending();
        }
    }

    /// The seek held for an index, if one is.
    pub fn pending(&self) -> Option<(Timestamp, SeekMode)> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).map(|h| (h.to, h.mode))
    }

    /// The stream time being presented, held at the last one once the run is over.
    pub fn position(&self) -> Timestamp {
        let clock = || self.view.get().map(|view| view.read(|v| Arc::clone(&v.clock)));
        let now = || self.presented_on(|| clock().map_or(Timestamp::NONE, |clock| clock.now()));
        self.held().unwrap_or_else(now)
    }

    fn held(&self) -> Option<Timestamp> {
        self.over_at.load(Ordering::Acquire).checked_sub(1).map(Timestamp)
    }

    fn hold_position(&self, w: &Writer) {
        if self.over_at.load(Ordering::Acquire) == 0 {
            let now = w.clock.as_ref().map_or(Timestamp::ZERO, |c| c.now());
            let at = self.presented(now);
            if at.is_some() {
                self.over_at.store(at.0.saturating_add(1), Ordering::Release);
            }
        }
    }

    /// The longest duration any element or index states.
    pub fn duration(&self) -> Option<Timestamp> {
        self.view.get()?.read(crate::seeker::duration)
    }

    /// Whether a skip would leave every sink's streams together now.
    pub fn can_skip(&self) -> bool {
        self.view.get().is_some_and(|view| view.read(crate::seeker::can_skip))
    }

    /// What a seek can do now.
    pub fn seekable(&self) -> Seekable {
        self.view.get().map_or(Seekable::Unknown, |view| view.read(crate::seeker::seekable))
    }

    /// The byte level of [`seek`](Self::seek), with where each source reads.
    pub fn seek_bytes(&self, to_time: Timestamp, targets: &[Target]) -> Result<u64, CommandError> {
        self.append_seek(to_time, targets, false)
    }

    fn append_seek(
        &self,
        to_time: Timestamp,
        targets: &[Target],
        preview: bool,
    ) -> Result<u64, CommandError> {
        self.append_with_preview(Command::Seek { seek_gen: 0, to_time }, targets, preview)
    }

    pub(crate) fn preview(&self, seek_gen: u64) -> bool {
        let w = relock(&self.writer);
        w.preview_gen != 0 && w.preview_gen == seek_gen
    }

    pub fn skip_to(&self, to_time: Timestamp) {
        self.send(Command::SkipTo { to_time });
    }

    /// Freeze running time.
    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    /// Continue where running time stopped.
    pub fn resume(&self) {
        self.send(Command::Resume);
    }

    /// Flip between paused and playing, returning whether now paused.
    pub fn toggle(&self) -> bool {
        let pause = !self.is_paused();
        self.send(if pause { Command::Pause } else { Command::Resume });
        pause
    }

    /// Stop the run without waiting, even on a full log. A held seek is dropped.
    pub fn stop(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.holding.store(false, Ordering::Release);
        if self.append(Command::Stop).is_err() {
            self.abort();
        }
    }

    /// Has every group run a pass. A wake during a pass ends the park that follows it.
    pub fn wake(&self) {
        self.signal_all();
    }

    /// Run `edit` on the pipeline on the `run()` thread and wait for its result.
    // One box per edit, off the media path.
    #[allow(clippy::disallowed_methods)]
    pub fn edit<T: Send + 'static>(
        &self,
        edit: impl FnOnce(&mut Pipeline) -> T + Send + 'static,
    ) -> Result<T, CommandError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.topology.push_edit(Box::new(move |p| drop(tx.send(edit(p)))))?;
        rx.recv().map_err(|_| CommandError::Finished)
    }

    pub(crate) fn append_with(
        &self,
        cmd: Command,
        targets: &[Target],
    ) -> Result<u64, CommandError> {
        self.append_with_preview(cmd, targets, false)
    }

    /// [`append_with`](Self::append_with), a seek marked as a preview under the same lock that
    /// commits it, so a refused append leaves no flag behind.
    fn append_with_preview(
        &self,
        cmd: Command,
        targets: &[Target],
        preview: bool,
    ) -> Result<u64, CommandError> {
        let mut w = relock(&self.writer);
        if targets.iter().any(|(element, ..)| *element != ALL && element.0 as usize >= w.known) {
            return Err(CommandError::NoSuchElement);
        }
        if self.is_finished() {
            return Err(CommandError::Finished);
        }
        let redundant = match cmd {
            Command::Pause => self.is_paused(),
            Command::Resume => !self.is_paused(),
            Command::Stop => self.is_stopped(),
            _ if w.clock.is_none() => return Err(CommandError::NotRunning),
            Command::SetRate { rate } => rate == w.rate,
            Command::SetOffset { element, .. } if w.sinks.binary_search(&element.0).is_err() => {
                return Err(CommandError::NotASink);
            }
            Command::Seek { .. } | Command::SkipTo { .. } | Command::SetOffset { .. } => false,
            Command::Splice { .. } => false,
        };
        if redundant {
            return Err(CommandError::Redundant);
        }
        if !self.log.has_room(&w.cursors) {
            return Err(CommandError::Full);
        }
        let seek_gen = self.seek_gen() + 1;
        for (element, to_byte, at) in targets {
            let target = (seek_gen, SeekTarget { to_byte: *to_byte, at: *at });
            match *element == ALL {
                true => {
                    w.all = target;
                    // A source pinned at its join yields to a seek aimed at every source.
                    w.targets
                        .iter_mut()
                        .filter(|t| t.1.0 == FOR_EVERY_SEEK)
                        .for_each(|t| t.1.0 = 0);
                }
                false => *slot(&mut w.targets, element.0) = target,
            }
        }
        w.preview_next = preview;
        Ok(self.commit(&mut w, cmd))
    }

    pub(crate) fn target(&self, element: ElementId, seek_gen: u64) -> Option<SeekTarget> {
        let w = relock(&self.writer);
        let own = |el: u32| {
            w.targets.binary_search_by_key(&el, |t| t.0).map_or((0, START), |k| w.targets[k].1)
        };
        let own = (element != ALL && (element.0 as usize) < w.known).then(|| own(element.0));
        own.into_iter().chain([w.all]).find(|(of, _)| *of >= seek_gen).map(|(_, target)| target)
    }

    /// Whether every live group has applied command `seq`.
    pub fn applied_by_all(&self, seq: u64) -> bool {
        min_applied(&relock(&self.writer).cursors) > seq
    }

    /// Whether every live group has applied every command in the log.
    pub fn settled(&self) -> bool {
        min_applied(&relock(&self.writer).cursors) >= self.log.head()
    }

    pub(crate) fn send(&self, cmd: Command) {
        self.send_with(cmd, &[]);
    }

    pub(crate) fn send_with(&self, cmd: Command, targets: &[Target]) {
        loop {
            let seen = self.log.room.epoch();
            match self.append_with(cmd, targets) {
                Err(CommandError::Full) => {}
                Err(_) => return self.signal_all(),
                Ok(_) => return,
            }
            // Parks without the writer lock, so a stop never queues behind a wedged group.
            self.log.room.park(seen, Some(crate::sched::PARK_BACKSTOP), None);
        }
    }

    fn commit(&self, w: &mut Writer, mut cmd: Command) -> u64 {
        let now = w.clock.as_ref().map_or(Timestamp::ZERO, |c| c.now());
        match &mut cmd {
            Command::Seek { seek_gen, to_time, .. } => {
                *seek_gen = self.seek_gen() + 1;
                w.preview_gen = if std::mem::take(&mut w.preview_next) { *seek_gen } else { 0 };
                self.rebase(now, *to_time);
                self.restart(w, *to_time);
                w.seek_applied_at = self.log.head() + 1;
            }
            Command::SkipTo { to_time } => {
                let running = self.timeline.running(*to_time);
                self.rebase(now, clamp(running));
                w.presentation_point = *to_time;
            }
            Command::SetRate { rate } => {
                w.rate = *rate;
                if w.retimer.is_none() {
                    self.extend(w, self.presented(now), *rate);
                }
            }
            Command::Pause => {
                // Frozen before the flag, so a reader that sees the flag reads this pause's time.
                let live = crate::time::running_time(now.0, self.base.load(Ordering::Acquire));
                self.frozen.store(live.0, Ordering::Release);
                self.paused.store(true, Ordering::Release);
            }
            Command::Resume => {
                // The base before the flag, as for a pause.
                if self.base.load(Ordering::Acquire) != crate::time::BASE_UNSET {
                    self.rebase(now, Timestamp(self.frozen.load(Ordering::Acquire)));
                }
                self.paused.store(false, Ordering::Release);
            }
            Command::SetOffset { .. } | Command::Splice { .. } => {}
            Command::Stop => {
                self.hold_position(w);
                self.stopped.store(true, Ordering::Release);
            }
        }
        // Effects before the slot, so a group that reads the command reads the base it moved.
        let seq = self.log.publish(cmd);
        if let Command::Seek { seek_gen, .. } = cmd {
            // After head, so a writer that sees it move returns to a pass that finds the command.
            self.seek_gen.store(seek_gen, Ordering::Release);
        }
        self.signal_all();
        seq
    }

    pub(crate) fn running(&self, clock: Timestamp) -> Timestamp {
        self.running_on(|| clock)
    }

    /// [`running`](Self::running) on `clock`, read after the base. A run publishes its clock before
    /// its base.
    pub(crate) fn running_on(&self, clock: impl FnOnce() -> Timestamp) -> Timestamp {
        // The flag before the base, which a resume writes ahead of the flag.
        let frozen = self.is_paused().then(|| Timestamp(self.frozen.load(Ordering::Acquire)));
        match self.base.load(Ordering::Acquire) {
            crate::time::BASE_UNSET => Timestamp::NONE,
            base => frozen.unwrap_or_else(|| match clock() {
                now if now.is_none() => Timestamp::NONE,
                now => crate::time::running_time(now.0, base),
            }),
        }
    }

    pub(crate) fn presented(&self, clock: Timestamp) -> Timestamp {
        self.timeline.stream(self.running(clock))
    }

    /// [`presented`](Self::presented) on `clock`, read after the base.
    pub(crate) fn presented_on(&self, clock: impl FnOnce() -> Timestamp) -> Timestamp {
        self.timeline.stream(self.running_on(clock))
    }

    fn restart(&self, w: &mut Writer, at: Timestamp) {
        (w.origin, w.presentation_point) = (at, at);
        self.timeline.push(Piece { stream_start: at, running_start: at, rate: w.rate }, true);
    }

    fn extend(&self, w: &Writer, start: Timestamp, rate: Rate) {
        let last = self.timeline.find(Timestamp::MAX, true);
        let start = start.max(last.stream_start).max(w.presentation_point);
        if rate != last.rate && start.is_some() {
            let running_start = clamp(last.running(start.0));
            self.timeline.push(Piece { stream_start: start, running_start, rate }, false);
        }
    }

    pub(crate) fn claim(&self, element: ElementId) -> Result<(), ElementId> {
        let mut w = relock(&self.writer);
        if let Some(first) = w.retimer.filter(|first| *first != element) {
            return Err(first);
        }
        if w.retimer.replace(element).is_none() && w.rate != Rate::ONE {
            let (origin, asked) = (w.origin, std::mem::replace(&mut w.rate, Rate::ONE));
            self.restart(&mut w, origin);
            w.rate = asked;
        }
        Ok(())
    }

    pub(crate) fn retime(&self, cursor: &Cursor, start: Timestamp, rate: Rate) {
        let mut w = relock(&self.writer);
        if cursor.get() < w.seek_applied_at {
            return;
        }
        w.rate = rate;
        self.extend(&w, start, rate);
        drop(w);
        self.signal_all();
    }

    fn rebase(&self, now: Timestamp, target: Timestamp) {
        if self.is_paused() {
            self.frozen.store(target.0, Ordering::Release);
        }
        let ns = |t: Timestamp| t.0.min(i64::MAX as u64) as i64;
        self.base.store(ns(now) - ns(target), Ordering::Release);
    }

    // Once per run.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn begin_run(
        &self,
        clock: Arc<dyn Clock>,
        stations: Vec<Arc<Station>>,
        terminal: &[bool],
        hold: bool,
    ) -> (Vec<Arc<Cursor>>, u64) {
        debug_assert_eq!(stations.len(), terminal.len(), "one flag per group");
        // A hold of the run before is not this one's, and the pending lock is taken first.
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.holding.store(false, Ordering::Release);
        let mut w = relock(&self.writer);
        let head = self.log.head();
        w.cursors = terminal.iter().map(|t| Arc::new(Cursor::new(head, *t))).collect();
        w.clock = Some(clock);
        (w.rate, w.retimer) = (Rate::ONE, None);
        self.restart(&mut w, Timestamp::ZERO);
        self.over_at.store(0, Ordering::Release);
        self.finished.store(false, Ordering::Release);
        self.install_stations(stations);
        let seek_gen = self.seek_gen();
        if hold || self.is_paused() {
            self.commit(&mut w, Command::Pause);
        }
        (w.cursors.clone(), seek_gen)
    }

    // Once per run.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn set_sinks(&self, sinks: Vec<u32>, known: usize) {
        let mut w = relock(&self.writer);
        w.targets.clear();
        (w.sinks, w.known, w.all) = (sinks, known, (0, START));
    }

    // Once per change.
    #[allow(clippy::disallowed_methods)]
    /// A change joins the run, `gone` says which elements left.
    pub(crate) fn join(
        &self,
        stations: &[Arc<Station>],
        terminal: &[bool],
        (sinks, known): (Vec<u32>, usize),
        targets: &[Target],
        sources: &[ElementId],
        gone: impl Fn(u32) -> bool,
    ) -> (Vec<Arc<Cursor>>, u64, bool, Rate, Timestamp) {
        let mut w = relock(&self.writer);
        let head = self.log.head();
        let cursors: Vec<_> = terminal.iter().map(|t| Arc::new(Cursor::new(head, *t))).collect();
        w.cursors.extend(cursors.iter().cloned());
        let seek_gen = self.seek_gen();
        w.targets.retain(|(el, _)| !gone(*el));
        let added = (w.known as u32..known as u32).map(|el| (el, (seek_gen, START)));
        w.targets.extend(added);
        w.known = w.known.max(known);
        for element in sources {
            slot(&mut w.targets, element.0).0 = FOR_EVERY_SEEK;
        }
        for (element, to_byte, at) in targets.iter().filter(|(el, ..)| (el.0 as usize) < known) {
            *slot(&mut w.targets, element.0) =
                (seek_gen, SeekTarget { to_byte: *to_byte, at: *at });
        }
        w.sinks = sinks;
        relock(&self.stations).live.extend(stations.iter().cloned());
        let now = w.clock.as_ref().map_or(Timestamp::ZERO, |c| c.now());
        (cursors, seek_gen, self.is_paused(), w.rate, self.presented(now).max(w.presentation_point))
    }

    pub(crate) fn position_now(&self) -> Timestamp {
        let w = relock(&self.writer);
        let now = w.clock.as_ref().map_or(Timestamp::ZERO, |c| c.now());
        self.presented(now).max(w.presentation_point)
    }

    pub(crate) fn retire(&self, cursor: &Arc<Cursor>, station: &Arc<Station>, head: ElementId) {
        self.log.advance(cursor, RETIRED);
        relock(&self.writer).cursors.retain(|c| !Arc::ptr_eq(c, cursor));
        let mut st = relock(&self.stations);
        if let Some(at) = st.live.iter().position(|s| Arc::ptr_eq(s, station)) {
            let stats = st.live.swap_remove(at).stats();
            st.retired.add(stats);
        }
        drop(st);
        self.topology.forget(head);
        self.signal_all();
    }

    pub(crate) fn abort(&self) {
        let at = self.position();
        if self.over_at.load(Ordering::Acquire) == 0 && at.is_some() {
            self.over_at.store(at.0.saturating_add(1), Ordering::Release);
        }
        self.stopped.store(true, Ordering::Release);
        self.signal_all();
    }

    pub(crate) fn try_finish(&self) -> Finish {
        // Under the writer lock like append, so a seek and the finish are ordered.
        let w = match self.writer.try_lock() {
            Ok(w) => w,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return Finish::Busy,
        };
        let seek_gen = self.seek_gen();
        let terminal = |c: &Arc<Cursor>| c.terminal.load(Ordering::Acquire);
        let ended = |c: &Arc<Cursor>| !terminal(c) || c.ended.load(Ordering::Acquire) == seek_gen;
        if self.is_stopped() || self.topology.changing() || !w.cursors.iter().all(ended) {
            return Finish::NotYet;
        }
        self.hold_position(&w);
        self.finished.store(true, Ordering::Release);
        self.stopped.store(true, Ordering::Release);
        self.signal_all();
        Finish::Done
    }

    pub(crate) fn over_but(&self, joining: &[Arc<Cursor>]) -> bool {
        let (w, seek_gen) = (relock(&self.writer), self.seek_gen());
        let ended = |c: &Arc<Cursor>| {
            !c.terminal.load(Ordering::Acquire) || c.ended.load(Ordering::Acquire) == seek_gen
        };
        let waits = |c: &Arc<Cursor>| joining.iter().any(|j| Arc::ptr_eq(j, c));
        !self.is_stopped() && w.cursors.iter().all(|c| waits(c) || ended(c))
    }

    /// Whether the run ended on its own, every terminal group at end of stream.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub(crate) fn clear_stop(&self) {
        self.stopped.store(false, Ordering::Release);
    }

    /// Whether the run is over, by a stop, a failure or its finish.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// The newest seek's generation, zero before any seek.
    pub fn seek_gen(&self) -> u64 {
        self.seek_gen.load(Ordering::Acquire)
    }

    pub(crate) fn install_stations(&self, live: Vec<Arc<Station>>) {
        let mut st = relock(&self.stations);
        let old = std::mem::replace(&mut st.live, live);
        for station in &old {
            st.retired.add(station.stats());
        }
    }

    pub(crate) fn signal_all(&self) {
        for station in &relock(&self.stations).live {
            station.signal();
        }
    }

    pub fn scheduler_stats(&self) -> SchedulerStats {
        let st = relock(&self.stations);
        let mut total = st.retired;
        for station in &st.live {
            total.add(station.stats());
        }
        total
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use std::time::Duration;

    const CAP: u64 = COMMAND_LOG_CAP as u64;
    const MS: fn(u64) -> Timestamp = Timestamp::from_millis;

    fn seek(k: u64) -> Command {
        Command::Seek { seek_gen: 0, to_time: Timestamp(k * 10) }
    }

    fn running(groups: usize) -> (Arc<Control>, Vec<Arc<Cursor>>, MockClock) {
        let control = Control::new(Arc::new(AtomicI64::new(0)));
        let clock = MockClock::new();
        let stations = (0..groups).map(|_| Arc::new(Station::new())).collect();
        let terminal = vec![true; groups];
        let (cursors, _) = control.begin_run(Arc::new(clock.clone()), stations, &terminal, false);
        (control, cursors, clock)
    }

    fn drain(control: &Control, cursor: &Cursor) -> Vec<Command> {
        let head = control.log.head();
        let got = (cursor.get()..head).map(|seq| control.log.read(seq)).collect();
        control.log.advance(cursor, head);
        got
    }

    #[test]
    fn commands_come_back_in_append_order_with_their_payload() {
        let (control, cursors, _) = running(1);
        let skip = Command::SkipTo { to_time: Timestamp(55) };
        let sent = [seek(7), Command::Pause, skip, seek(9), Command::Resume, Command::Stop];
        for (seq, cmd) in sent.into_iter().enumerate() {
            assert_eq!(control.append(cmd), Ok(seq as u64));
        }
        let want = [
            Command::Seek { seek_gen: 1, to_time: Timestamp(70) },
            Command::Pause,
            skip,
            Command::Seek { seek_gen: 2, to_time: Timestamp(90) },
            Command::Resume,
            Command::Stop,
        ];
        assert_eq!(drain(&control, &cursors[0]), want, "the log assigned the generations");
        assert_eq!(control.seek_gen(), 2);
        assert!(drain(&control, &cursors[0]).is_empty());
    }

    #[test]
    fn the_log_wraps_past_its_capacity_behind_a_reader() {
        let (control, cursors, _) = running(1);
        for k in 0..3 * CAP + 5 {
            assert_eq!(control.append(seek(k)), Ok(k));
            let want = Command::Seek { seek_gen: k + 1, to_time: Timestamp(k * 10) };
            assert_eq!(drain(&control, &cursors[0]), [want]);
        }
    }

    #[test]
    fn the_log_is_full_while_one_cursor_lags_a_whole_capacity() {
        let (control, cursors, _) = running(2);
        for k in 0..CAP {
            assert_eq!(control.append(seek(k)), Ok(k));
            assert_eq!(drain(&control, &cursors[0]).len(), 1);
        }
        assert_eq!(control.append(seek(CAP)), Err(CommandError::Full));
        assert_eq!(control.seek_gen(), CAP, "a refused seek takes no generation");
        assert!(!control.applied_by_all(0), "the laggard has applied nothing");
        let missed = drain(&control, &cursors[1]);
        let want: Vec<Command> = (0..CAP)
            .map(|k| Command::Seek { seek_gen: k + 1, to_time: Timestamp(k * 10) })
            .collect();
        assert_eq!(missed, want);
        assert!(control.applied_by_all(CAP - 1));
        assert_eq!(control.append(seek(CAP)), Ok(CAP));
    }

    #[test]
    fn a_group_that_left_does_not_hold_the_log_back() {
        let (control, cursors, _) = running(2);
        control.retire(&cursors[1], &Arc::new(Station::new()), ALL);
        for k in 0..2 * CAP {
            assert_eq!(control.append(seek(k)), Ok(k));
            drain(&control, &cursors[0]);
        }
        assert!(control.applied_by_all(2 * CAP - 1));
        control.retire(&cursors[0], &Arc::new(Station::new()), ALL);
        for k in 0..2 * CAP {
            assert_eq!(control.append(seek(k)), Ok(2 * CAP + k));
        }
        assert!(control.applied_by_all(4 * CAP - 1));
    }

    #[test]
    fn the_log_is_settled_when_every_live_group_has_applied_all_of_it() {
        let control = Control::new(Arc::new(AtomicI64::new(0)));
        assert!(control.settled(), "before a run there is nothing to apply");
        let (control, cursors, _) = running(2);
        assert!(control.settled());
        assert_eq!(control.append(seek(1)), Ok(0));
        assert!(!control.settled());
        drain(&control, &cursors[0]);
        assert!(!control.settled(), "one group has not flushed for it");
        drain(&control, &cursors[1]);
        assert!(control.settled() && control.applied_by_all(0));
        assert_eq!(control.append(Command::SkipTo { to_time: MS(5) }), Ok(1));
        assert_eq!(control.append(Command::Pause), Ok(2));
        drain(&control, &cursors[0]);
        assert!(!control.settled());
        control.retire(&cursors[1], &Arc::new(Station::new()), ALL);
        assert!(control.settled());
    }

    #[test]
    fn a_command_that_changes_nothing_is_not_appended() {
        let (control, _, _) = running(1);
        assert_eq!(control.append(Command::Resume), Err(CommandError::Redundant));
        assert_eq!(control.append(Command::Pause), Ok(0));
        assert_eq!(control.append(Command::Pause), Err(CommandError::Redundant));
        assert!(control.is_paused());
        assert_eq!(control.append(Command::Resume), Ok(1));
        assert_eq!(control.append(Command::Stop), Ok(2));
        assert_eq!(control.append(Command::Stop), Err(CommandError::Redundant));
        assert!(control.is_stopped());
        control.send(Command::Stop);
        control.send(Command::Resume);
        assert_eq!(control.log.head(), 3);
    }

    #[test]
    fn a_handle_call_that_changes_nothing_still_wakes_the_groups() {
        let control = Control::new(Arc::new(AtomicI64::new(0)));
        let station = Arc::new(Station::new());
        control.begin_run(Arc::new(MockClock::new()), vec![Arc::clone(&station)], &[true], false);
        let pause = Arc::clone(&control);
        let stop = Arc::clone(&control);
        let seen = station.epoch();
        assert_eq!(control.append(Command::Resume), Err(CommandError::Redundant));
        assert_eq!(station.epoch(), seen);
        pause.resume();
        assert_eq!((control.log.head(), station.epoch()), (0, seen + 1));
        pause.pause();
        pause.pause();
        assert_eq!(
            (control.log.head(), station.epoch()),
            (1, seen + 3),
            "appended once, woken twice"
        );
        stop.stop();
        stop.stop();
        assert_eq!((control.log.head(), station.epoch()), (2, seen + 5));
        assert!(stop.is_stopped() && pause.is_paused());
    }

    #[test]
    fn a_reading_takes_the_base_before_the_clock() {
        let base = Arc::new(AtomicI64::new(crate::time::BASE_UNSET));
        let control = Control::new(Arc::clone(&base));
        let old_clock = || {
            base.store(0, Ordering::Release);
            MS(14)
        };
        let read = control.presented_on(old_clock);
        assert!(read.is_none(), "the old clock's {read:?} read against the new base");
        base.store(0, Ordering::Release);
        assert_eq!(control.presented_on(|| MS(0)), MS(0), "the new clock against the new base");
    }

    #[test]
    fn before_the_first_run_a_seek_is_refused_and_a_pause_latches_for_the_run() {
        let base = Arc::new(AtomicI64::new(crate::time::BASE_UNSET));
        let control = Control::new(Arc::clone(&base));
        assert_eq!(control.append(seek(1)), Err(CommandError::NotRunning));
        let skip = Command::SkipTo { to_time: MS(1) };
        assert_eq!(control.append(skip), Err(CommandError::NotRunning), "nothing to skip in");
        assert_eq!((control.seek_gen(), control.log.head()), (0, 0));
        assert_eq!(control.append(Command::Pause), Ok(0));
        assert!(control.applied_by_all(0), "no group is live to apply it");
        assert_eq!(base.load(Ordering::Acquire), crate::time::BASE_UNSET, "no base to shift yet");

        let clock = MockClock::starting_at(MS(5));
        base.store(crate::time::base_from_clock(clock.now()), Ordering::Release);
        let stations = vec![Arc::new(Station::new())];
        let (cursors, seek_gen) =
            control.begin_run(Arc::new(clock.clone()), stations, &[true], false);
        assert_eq!((cursors[0].get(), seek_gen), (1, 0));
        assert_eq!(drain(&control, &cursors[0]), [Command::Pause]);
        clock.advance(MS(20));
        assert_eq!(control.append(Command::Resume), Ok(2));
        assert_eq!(base.load(Ordering::Acquire), 25_000_000);
    }

    #[test]
    fn a_held_start_is_a_pause_the_new_groups_find() {
        let control = Control::new(Arc::new(AtomicI64::new(0)));
        let stations = vec![Arc::new(Station::new()), Arc::new(Station::new())];
        let seen = stations[1].epoch();
        let clock = Arc::new(MockClock::new());
        let (cursors, _) = control.begin_run(clock, stations.clone(), &[false, true], true);
        assert!(control.is_paused());
        assert_ne!(stations[1].epoch(), seen, "appending signals the run's stations");
        for cursor in &cursors {
            assert_eq!(drain(&control, cursor), [Command::Pause]);
        }
    }

    #[test]
    fn a_seek_rebases_running_time_and_a_resume_excises_the_pause() {
        let base = Arc::new(AtomicI64::new(0));
        let control = Control::new(Arc::clone(&base));
        let clock = MockClock::new();
        control.begin_run(Arc::new(clock.clone()), Vec::new(), &[], false);
        let seek_to = |to_time| Command::Seek { seek_gen: 0, to_time };
        let ms = |v: i64| v * 1_000_000;
        clock.advance(MS(100));
        assert_eq!(control.append(seek_to(MS(30))), Ok(0));
        assert_eq!(base.load(Ordering::Acquire), ms(70), "running time reads 30 ms now");
        assert_eq!(control.append(Command::Pause), Ok(1));
        clock.advance(MS(40));
        assert_eq!(control.append(seek_to(MS(500))), Ok(2));
        assert_eq!(base.load(Ordering::Acquire), ms(140 - 500), "signed, the target is ahead");
        clock.advance(MS(10));
        assert_eq!(control.append(Command::Resume), Ok(3));
        assert_eq!(base.load(Ordering::Acquire), ms(150 - 500), "running time reads 500 ms");
    }

    #[test]
    fn running_time_stands_still_through_a_pause_whatever_the_clock_does() {
        let (control, _cursors, clock) = running(1);
        let now = || (control.running(clock.now()), control.presented(clock.now()));
        clock.advance(MS(100));
        assert_eq!(control.append(Command::Pause), Ok(0));
        clock.advance(MS(900));
        assert_eq!(now(), (MS(100), MS(100)), "what it read at the pause");
        assert_eq!(control.append(Command::Seek { seek_gen: 0, to_time: MS(5_000) }), Ok(1));
        clock.advance(MS(50));
        assert_eq!(now(), (MS(5_000), MS(5_000)));
        assert_eq!(control.append(Command::Resume), Ok(2));
        assert_eq!(now().0, MS(5_000), "and it goes on from there, with no step");
        clock.advance(MS(7));
        assert_eq!(now(), (MS(5_007), MS(5_007)));
        assert_eq!(control.append(Command::Pause), Ok(3));
        assert_eq!(control.append(Command::Resume), Ok(4));
        assert_eq!(now().0, MS(5_007));
        let fresh = Control::new(Arc::new(AtomicI64::new(crate::time::BASE_UNSET)));
        assert_eq!(fresh.append(Command::Pause), Ok(0));
        assert!(fresh.running(MS(3)).is_none() && fresh.presented(MS(3)).is_none());
        assert_eq!(fresh.append(Command::Resume), Ok(1));
        assert!(fresh.running(MS(3)).is_none());
    }

    #[test]
    fn a_skip_rebases_as_a_seek_does_and_takes_no_generation() {
        let base = Arc::new(AtomicI64::new(0));
        let control = Control::new(Arc::clone(&base));
        let clock = MockClock::new();
        let station = Arc::new(Station::new());
        let stations = vec![Arc::clone(&station)];
        let (cursors, _) = control.begin_run(Arc::new(clock.clone()), stations, &[true], false);
        let skip = |to_time| Command::SkipTo { to_time };
        let ms = |v: i64| v * 1_000_000;
        clock.advance(MS(100));
        let seen = station.epoch();
        assert_eq!(control.append(skip(MS(30))), Ok(0));
        assert_eq!(base.load(Ordering::Acquire), ms(70), "running time reads 30 ms now");
        assert_eq!((control.seek_gen(), station.epoch()), (0, seen + 1), "no generation, one wake");
        assert_eq!(drain(&control, &cursors[0]), [skip(MS(30))]);
        assert_eq!(control.append(Command::Pause), Ok(1));
        clock.advance(MS(40));
        assert_eq!(control.append(skip(MS(500))), Ok(2));
        assert_eq!(base.load(Ordering::Acquire), ms(140 - 500), "signed, the point is ahead");
        clock.advance(MS(10));
        assert_eq!(control.append(Command::Resume), Ok(3));
        assert_eq!(base.load(Ordering::Acquire), ms(150 - 500), "running time reads 500 ms");
        for k in 4..=CAP {
            assert_eq!(control.append(skip(MS(500))), Ok(k));
        }
        clock.advance(MS(10));
        assert_eq!(control.append(skip(MS(900))), Err(CommandError::Full));
        assert_eq!(base.load(Ordering::Acquire), ms(150 - 500), "a refused skip rebases nothing");
        assert_eq!(control.seek_gen(), 0);
    }

    #[test]
    fn a_skip_leaves_the_ended_slots_current_and_the_finish_is_unaffected() {
        let (control, cursors, _) = running(2);
        cursors[0].end(0);
        assert_eq!(control.append(Command::SkipTo { to_time: MS(5) }), Ok(0));
        assert_eq!(control.try_finish(), Finish::NotYet, "one sink still plays");
        cursors[1].end(0);
        assert_eq!(control.try_finish(), Finish::Done, "with the skip applied by nobody");
        assert_eq!(control.append(Command::SkipTo { to_time: MS(9) }), Err(CommandError::Finished));
    }

    #[test]
    fn a_full_log_makes_a_handle_wait_for_the_slowest_group() {
        let (control, cursors, _) = running(1);
        for k in 0..CAP {
            assert_eq!(control.append(seek(k)), Ok(k));
        }
        let waiter = {
            let control = Arc::clone(&control);
            std::thread::spawn(move || control.send(seek(CAP)))
        };
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(control.log.head(), CAP, "still waiting");
        control.log.advance(&cursors[0], 1);
        waiter.join().unwrap();
        assert_eq!((control.log.head(), control.seek_gen()), (CAP + 1, CAP + 1));
    }

    #[test]
    fn a_stop_gets_past_a_handle_that_waits_for_room() {
        let (control, cursors, _) = running(1);
        for k in 0..CAP {
            assert_eq!(control.append(seek(k)), Ok(k));
        }
        let waiter = {
            let control = Arc::clone(&control);
            std::thread::spawn(move || control.send(seek(CAP)))
        };
        std::thread::sleep(Duration::from_millis(30));
        let (done_tx, done) = std::sync::mpsc::channel();
        let stop = Arc::clone(&control);
        std::thread::spawn(move || {
            stop.stop();
            let _ = done_tx.send(());
        });
        let returned = done.recv_timeout(Duration::from_secs(5));
        assert!(returned.is_ok(), "the stop waited behind a handle that waits for room");
        assert!(control.is_stopped() && control.log.head() == CAP, "by the flag, the log is full");
        assert!(!control.applied_by_all(0), "and a query is not held up either");
        control.retire(&cursors[0], &Arc::new(Station::new()), ALL);
        waiter.join().unwrap();
        assert_eq!(control.log.head(), CAP + 1);
    }

    #[test]
    fn a_stop_on_a_full_log_does_not_wait_and_raises_the_flag() {
        let (control, _cursors, _) = running(1);
        for k in 0..CAP {
            assert_eq!(control.append(seek(k)), Ok(k));
        }
        let handle = Arc::clone(&control);
        assert!(!handle.is_stopped());
        handle.stop();
        assert!(handle.is_stopped() && control.log.head() == CAP, "by the flag, not the log");
        control.clear_stop();
        assert!(!handle.is_stopped());
    }

    fn rate(num: i64, den: i64) -> Rate {
        Rate::new(num, den).unwrap()
    }

    fn set_rate(num: i64, den: i64) -> Command {
        Command::SetRate { rate: rate(num, den) }
    }

    fn newest(control: &Control) -> (u64, u64, Rate) {
        let p = control.timeline.find(Timestamp::MAX, true);
        (p.stream_start.0 / 1_000_000, p.running_start.0 / 1_000_000, p.rate)
    }

    fn position(control: &Control, ms: u64) -> u64 {
        control.presented(MS(ms)).0
    }

    const NS: u64 = 1_000_000;
    const RETIMER: ElementId = ElementId(4);

    #[test]
    fn a_rate_with_no_retimer_starts_a_piece_at_what_is_being_presented() {
        let fresh = Control::new(Arc::new(AtomicI64::new(0)));
        assert_eq!(fresh.append(set_rate(2, 1)), Err(CommandError::NotRunning));
        let (control, cursors, clock) = running(1);
        assert_eq!(control.append(set_rate(1, 1)), Err(CommandError::Redundant), "it plays at one");
        clock.advance(MS(100));
        assert_eq!(control.append(set_rate(3, 2)), Ok(0));
        assert_eq!(drain(&control, &cursors[0]), [set_rate(3, 2)], "whole through the log");
        assert_eq!(newest(&control), (100, 100, rate(3, 2)));
        assert_eq!(control.append(set_rate(6, 4)), Err(CommandError::Redundant), "the same rate");
        assert_eq!(position(&control, 100), 100 * NS);
        assert_eq!(position(&control, 160), 190 * NS);
        assert_eq!(control.timeline.running(MS(190)), 160 * NS as i128);
        assert_eq!(control.timeline.running(MS(40)), 40 * NS as i128, "what was presented stands");
        clock.advance(MS(60));
        assert_eq!(control.append(set_rate(3, 4)), Ok(1));
        assert_eq!(newest(&control), (190, 160, rate(3, 4)));
        assert_eq!(position(&control, 200), 220 * NS);
        cursors[0].end(0);
        assert_eq!(control.try_finish(), Finish::Done);
        assert_eq!(control.append(set_rate(2, 1)), Err(CommandError::Finished));
    }

    #[test]
    fn a_rate_while_paused_starts_at_what_the_pause_froze() {
        let (control, _cursors, clock) = running(1);
        clock.advance(MS(100));
        assert_eq!(control.append(Command::Pause), Ok(0));
        clock.advance(MS(40));
        assert_eq!(control.append(set_rate(2, 1)), Ok(1));
        assert_eq!(newest(&control), (100, 100, rate(2, 1)));
        clock.advance(MS(10));
        assert_eq!(control.append(Command::Resume), Ok(2));
        assert_eq!(position(&control, 150), 100 * NS, "the pause is excised");
        assert_eq!(position(&control, 160), 120 * NS, "and it plays at two from there");
    }

    #[test]
    fn a_seek_restarts_the_timeline_at_one_piece_at_the_rate_last_asked_for() {
        let (control, _cursors, clock) = running(1);
        clock.advance(MS(100));
        assert_eq!(control.append(set_rate(2, 1)), Ok(0));
        clock.advance(MS(50));
        assert_eq!(control.append(Command::Seek { seek_gen: 0, to_time: MS(500) }), Ok(1));
        assert_eq!(newest(&control), (500, 500, rate(2, 1)));
        assert_eq!(position(&control, 150), 500 * NS, "the target is presented now");
        assert_eq!(position(&control, 160), 520 * NS);
        assert_eq!(control.timeline.running(MS(480)), 490 * NS as i128);
        assert_eq!(control.append(Command::Pause), Ok(2));
        clock.advance(MS(30));
        assert_eq!(control.append(Command::Seek { seek_gen: 0, to_time: MS(40) }), Ok(3));
        clock.advance(MS(20));
        assert_eq!(control.append(Command::Resume), Ok(4));
        assert_eq!((newest(&control), position(&control, 200)), ((40, 40, rate(2, 1)), 40 * NS));
        assert_eq!(position(&control, 205), 50 * NS);
    }

    #[test]
    fn a_skip_keeps_the_pieces_and_moves_the_base_to_where_its_time_maps() {
        let (control, _cursors, clock) = running(1);
        clock.advance(MS(100));
        assert_eq!(control.append(set_rate(2, 1)), Ok(0));
        clock.advance(MS(50));
        assert_eq!(control.append(Command::SkipTo { to_time: MS(300) }), Ok(1));
        assert_eq!(newest(&control), (100, 100, rate(2, 1)), "no piece was written");
        assert_eq!(position(&control, 150), 300 * NS);
        assert_eq!(position(&control, 160), 320 * NS);
        assert_eq!(control.append(Command::SkipTo { to_time: MS(60) }), Ok(2));
        assert_eq!(position(&control, 150), 60 * NS);
        assert_eq!(position(&control, 190), 100 * NS, "at one up to the join");
        assert_eq!(position(&control, 200), 120 * NS, "and at two behind it");
    }

    #[test]
    fn with_a_retimer_a_rate_writes_nothing_until_the_retimer_says_where() {
        let (control, cursors, clock) = running(1);
        assert_eq!(control.claim(RETIMER), Ok(()));
        assert_eq!(control.claim(RETIMER), Ok(()), "the same element again");
        assert_eq!(control.claim(ElementId(9)), Err(RETIMER), "one timeline, one retimer");
        clock.advance(MS(100));
        for (seq, r) in [(3, 2), (2, 1), (3, 4)].into_iter().enumerate() {
            assert_eq!(control.append(set_rate(r.0, r.1)), Ok(seq as u64));
        }
        assert_eq!(newest(&control), (0, 0, Rate::ONE));
        assert_eq!(position(&control, 250), 250 * NS, "it still plays at one");
        drain(&control, &cursors[0]);
        control.retime(&cursors[0], MS(300), rate(3, 4));
        assert_eq!(newest(&control), (300, 300, rate(3, 4)));
        assert_eq!((position(&control, 300), position(&control, 340)), (300 * NS, 330 * NS));
        assert_eq!(control.append(set_rate(1, 1)), Ok(3));
        control.retime(&cursors[0], MS(390), Rate::ONE);
        assert_eq!(newest(&control), (390, 420, Rate::ONE));
        control.retime(&cursors[0], MS(500), Rate::ONE);
        control.retime(&cursors[0], MS(200), rate(2, 1));
        assert_eq!(newest(&control), (390, 420, rate(2, 1)));
        control.retime(&cursors[0], Timestamp::NONE, rate(3, 2));
        assert_eq!(newest(&control), (390, 420, rate(2, 1)), "no stamp, no place");
    }

    #[test]
    fn a_retimers_piece_from_before_a_seek_is_dropped_and_the_restart_stands() {
        let (control, cursors, clock) = running(1);
        assert_eq!(control.claim(RETIMER), Ok(()));
        assert_eq!(control.append(set_rate(2, 1)), Ok(0));
        drain(&control, &cursors[0]);
        clock.advance(MS(100));
        assert_eq!(control.append(Command::Seek { seek_gen: 0, to_time: MS(700) }), Ok(1));
        assert_eq!(newest(&control), (700, 700, rate(2, 1)));
        control.retime(&cursors[0], MS(140), rate(2, 1));
        control.retime(&cursors[0], MS(140), rate(3, 2));
        assert_eq!(newest(&control), (700, 700, rate(2, 1)));
        assert_eq!(position(&control, 110), 720 * NS);
        drain(&control, &cursors[0]);
        control.retime(&cursors[0], MS(700), rate(2, 1));
        assert_eq!(control.timeline.find(Timestamp::ZERO, true).stream_start, MS(700), "one piece");
        control.retime(&cursors[0], MS(650), rate(3, 4));
        assert_eq!(newest(&control), (700, 700, rate(3, 4)));
        assert_eq!(position(&control, 110), 707_500_000);
    }

    #[test]
    fn a_skip_while_a_change_is_pending_leaves_one_consistent_map() {
        let (control, cursors, clock) = running(1);
        assert_eq!(control.claim(RETIMER), Ok(()));
        clock.advance(MS(100));
        assert_eq!(control.append(set_rate(2, 1)), Ok(0));
        assert_eq!(control.append(Command::SkipTo { to_time: MS(900) }), Ok(1));
        assert_eq!(position(&control, 100), 900 * NS);
        control.retime(&cursors[0], MS(250), rate(2, 1));
        assert_eq!(newest(&control), (900, 900, rate(2, 1)), "the piece is at the point");
        assert_eq!(position(&control, 100), 900 * NS, "which is still presented now");
        assert_eq!(position(&control, 130), 960 * NS);
        clock.advance(MS(30));
        assert_eq!(control.append(set_rate(1, 2)), Ok(2));
        assert_eq!(control.append(Command::SkipTo { to_time: MS(920) }), Ok(3));
        control.retime(&cursors[0], MS(1000), rate(1, 2));
        assert_eq!(newest(&control), (1000, 950, rate(1, 2)));
        assert_eq!(position(&control, 130), 920 * NS, "the skip's instant");
        assert_eq!(position(&control, 170), 1000 * NS, "40 ms on, 80 ms of stream at two");
        assert_eq!(position(&control, 190), 1010 * NS, "then at a half");
    }

    #[test]
    fn a_pause_while_a_change_is_pending_leaves_one_consistent_map() {
        let (control, cursors, clock) = running(1);
        assert_eq!(control.claim(RETIMER), Ok(()));
        clock.advance(MS(100));
        assert_eq!(control.append(set_rate(2, 1)), Ok(0));
        assert_eq!(control.append(Command::Pause), Ok(1));
        clock.advance(MS(500));
        control.retime(&cursors[0], MS(160), rate(2, 1));
        assert_eq!(control.append(Command::Resume), Ok(2));
        assert_eq!(position(&control, 600), 100 * NS, "the pause is excised");
        assert_eq!(position(&control, 660), 160 * NS);
        assert_eq!(position(&control, 670), 180 * NS);
    }

    #[test]
    fn a_rate_the_retimer_refuses_is_not_what_a_seek_restarts_at() {
        let (control, cursors, clock) = running(1);
        assert_eq!(control.claim(RETIMER), Ok(()));
        clock.advance(MS(100));
        assert_eq!(control.append(set_rate(64, 1)), Ok(0));
        drain(&control, &cursors[0]);
        control.retime(&cursors[0], MS(120), Rate::ONE);
        assert_eq!(newest(&control), (0, 0, Rate::ONE));
        assert_eq!(control.append(Command::Seek { seek_gen: 0, to_time: MS(300) }), Ok(1));
        assert_eq!(newest(&control), (300, 300, Rate::ONE));
        assert_eq!(control.append(set_rate(64, 1)), Ok(2), "and asking again is no repeat");
    }

    #[test]
    fn a_rate_written_before_the_retimer_claimed_is_taken_back_to_the_restart() {
        let (control, cursors, clock) = running(1);
        clock.advance(MS(3));
        assert_eq!(control.append(set_rate(2, 1)), Ok(0));
        assert_eq!(newest(&control), (3, 3, rate(2, 1)));
        assert_eq!(control.claim(RETIMER), Ok(()));
        assert_eq!(newest(&control), (0, 0, Rate::ONE));
        assert_eq!(control.append(set_rate(2, 1)), Err(CommandError::Redundant), "still asked for");
        assert_eq!(drain(&control, &cursors[0]), [set_rate(2, 1)]);
        control.retime(&cursors[0], Timestamp::ZERO, rate(2, 1));
        assert_eq!(newest(&control), (0, 0, rate(2, 1)));
        control.clear_stop();
        control.begin_run(Arc::new(clock), vec![Arc::new(Station::new())], &[true], false);
        assert_eq!(newest(&control), (0, 0, Rate::ONE));
        assert_eq!(control.claim(ElementId(9)), Ok(()));
    }

    impl Control {
        pub(crate) fn while_appending<R>(&self, f: impl FnOnce() -> R) -> R {
            let _w = relock(&self.writer);
            f()
        }
    }

    #[test]
    fn the_run_finishes_when_the_last_terminal_group_has_ended() {
        let base = Arc::new(AtomicI64::new(0));
        let control = Control::new(Arc::clone(&base));
        let stations: Vec<_> = (0..3).map(|_| Arc::new(Station::new())).collect();
        let clock = Arc::new(MockClock::new());
        let (cursors, seek_gen) =
            control.begin_run(clock, stations.clone(), &[false, true, true], false);
        let seen = stations[0].epoch();
        assert_eq!(control.try_finish(), Finish::NotYet, "nobody has ended");
        cursors[1].end(seek_gen);
        assert_eq!(control.try_finish(), Finish::NotYet, "one of two");
        assert!(!control.is_finished() && !control.is_stopped());
        assert_eq!(stations[0].epoch(), seen, "and asking wakes nobody");
        cursors[2].end(seek_gen);
        assert_eq!(control.try_finish(), Finish::Done);
        assert!(control.is_finished() && control.is_stopped());
        assert_eq!(stations[0].epoch(), seen + 1, "the group that is not terminal hears it too");
        assert_eq!(control.try_finish(), Finish::NotYet, "decided once");
        let skip = Command::SkipTo { to_time: MS(1) };
        for cmd in [seek(1), skip, Command::Pause, Command::Resume, Command::Stop] {
            assert_eq!(control.append(cmd), Err(CommandError::Finished), "{cmd:?}");
        }
        assert_eq!((control.log.head(), control.seek_gen(), control.is_paused()), (0, 0, false));
        assert_eq!(base.load(Ordering::Acquire), 0, "and a refused skip rebased nothing");
        control.send(seek(1));
        assert_eq!((control.log.head(), stations[0].epoch()), (0, seen + 2));
    }

    #[test]
    fn a_seek_leaves_every_ended_slot_behind_and_the_finish_waits_for_them_all() {
        let (control, cursors, _) = running(2);
        cursors[0].end(0);
        cursors[1].end(0);
        assert_eq!(control.append(seek(3)), Ok(0));
        assert_eq!(control.try_finish(), Finish::NotYet, "both ended, a generation ago");
        cursors[0].end(1);
        assert_eq!(control.try_finish(), Finish::NotYet, "the other has yet to end again");
        cursors[1].end(1);
        assert_eq!(control.try_finish(), Finish::Done);
    }

    #[test]
    fn a_held_lock_is_busy_and_a_stopped_run_does_not_finish() {
        let (control, cursors, clock) = running(1);
        cursors[0].end(0);
        assert_eq!(control.while_appending(|| control.try_finish()), Finish::Busy);
        assert!(!control.is_finished() && !control.is_stopped(), "nothing was decided");
        assert_eq!(control.append(Command::Stop), Ok(0));
        assert_eq!(control.try_finish(), Finish::NotYet);
        assert!(!control.is_finished(), "it was stopped first");
        assert_eq!(control.append(seek(1)), Ok(1), "a stopped run is not a finished one");
        control.clear_stop();
        let clock = Arc::new(clock);
        let station = || vec![Arc::new(Station::new())];
        let (cursors, seek_gen) = control.begin_run(clock.clone(), station(), &[true], false);
        cursors[0].end(seek_gen);
        assert_eq!(control.try_finish(), Finish::Done);
        control.clear_stop();
        control.begin_run(clock, station(), &[true], false);
        assert!(!control.is_finished());
        assert_eq!(control.try_finish(), Finish::NotYet, "the old run's slot is not this run's");
        assert_eq!(control.append(Command::Pause), Ok(2));
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    const THREE_THREAD_PREEMPTIONS: usize = 3;

    fn cmd(k: u64) -> Command {
        Command::Seek { seek_gen: k + 1, to_time: Timestamp(k * 7) }
    }

    fn cursors(n: usize) -> Vec<Arc<Cursor>> {
        (0..n).map(|_| Arc::new(Cursor::new(0, true))).collect()
    }

    fn drain<const N: usize>(log: &CommandLog<N>, cursor: &Cursor, applied: &mut u64) {
        let head = log.head();
        if head == *applied {
            return;
        }
        while *applied < head {
            assert_eq!(log.read(*applied), cmd(*applied), "in order, whole, and once");
            *applied += 1;
        }
        log.advance(cursor, *applied);
    }

    fn read_all<const N: usize>(log: &CommandLog<N>, cursor: &Cursor, total: u64) {
        let mut applied = 0;
        while applied < total {
            drain(log, cursor, &mut applied);
            loom::thread::yield_now();
        }
    }

    #[test]
    fn loom_log_two_readers_see_every_command_in_order() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(THREE_THREAD_PREEMPTIONS);
        model.check(|| {
            let log = Arc::new(CommandLog::<2>::new());
            let cursors = cursors(2);
            let readers: Vec<_> = cursors
                .iter()
                .map(|cursor| {
                    let (log, cursor) = (Arc::clone(&log), Arc::clone(cursor));
                    loom::thread::spawn(move || {
                        let mut applied = 0;
                        drain(&log, &cursor, &mut applied);
                        drain(&log, &cursor, &mut applied);
                        applied
                    })
                })
                .collect();
            let mut published = 0;
            for _ in 0..4 {
                if log.has_room(&cursors) {
                    assert_eq!(log.publish(cmd(published)), published);
                    published += 1;
                }
            }
            assert!(published >= 2, "two slots take two commands whatever the readers do");
            for (reader, cursor) in readers.into_iter().zip(&cursors) {
                let mut applied = reader.join().unwrap();
                drain(&log, cursor, &mut applied);
                assert_eq!(applied, published, "every command, none lost under a lagging cursor");
            }
        });
    }

    #[test]
    fn loom_full_log_writer_is_woken_by_the_cursor_store() {
        loom::model(|| {
            let log = Arc::new(CommandLog::<1>::new());
            let cursors = cursors(1);
            let reader = {
                let (log, cursor) = (Arc::clone(&log), Arc::clone(&cursors[0]));
                loom::thread::spawn(move || read_all(&log, &cursor, 2))
            };
            for k in 0..2 {
                loop {
                    let seen = log.room.epoch();
                    if log.has_room(&cursors) {
                        break;
                    }
                    log.room.park(seen, None, None);
                }
                assert_eq!(log.publish(cmd(k)), k);
            }
            reader.join().unwrap();
        });
    }

    fn two_sinks() -> (Arc<Control>, Vec<Arc<Cursor>>) {
        let control = Control::new(Arc::new(AtomicI64::new(0)));
        let stations = vec![Arc::new(Station::new()), Arc::new(Station::new())];
        let clock = Arc::new(crate::clock::MockClock::new());
        let (cursors, _) = control.begin_run(clock, stations, &[true, true], false);
        (control, cursors)
    }

    fn end_and_ask(control: &Control, cursor: &Cursor, seek_gen: u64) -> Finish {
        cursor.end(seek_gen);
        loop {
            match control.try_finish() {
                Finish::Busy => loom::thread::yield_now(),
                found => return found,
            }
        }
    }

    fn sink(control: &Arc<Control>, cursor: &Arc<Cursor>) -> loom::thread::JoinHandle<Finish> {
        let (control, cursor) = (Arc::clone(control), Arc::clone(cursor));
        loom::thread::spawn(move || end_and_ask(&control, &cursor, 0))
    }

    #[test]
    fn loom_two_sinks_ending_together_finish_the_run_exactly_once() {
        loom::model(|| {
            let (control, cursors) = two_sinks();
            let other = sink(&control, &cursors[1]);
            let mine = end_and_ask(&control, &cursors[0], 0);
            let theirs = other.join().unwrap();
            assert!(control.is_finished(), "both ended and both asked");
            assert_eq!((mine == Finish::Done) as u32 + (theirs == Finish::Done) as u32, 1);
        });
    }

    #[test]
    fn loom_a_seek_and_the_finish_are_one_before_the_other() {
        loom::model(|| {
            let (control, cursors) = two_sinks();
            cursors[0].end(0);
            let last = sink(&control, &cursors[1]);
            let seek = control.append(cmd(0));
            let finish = last.join().unwrap();
            let Ok(seq) = seek else {
                assert_eq!(seek, Err(CommandError::Finished));
                assert!(finish == Finish::Done && control.is_finished(), "the finish was first");
                assert_eq!((control.log.head(), control.seek_gen()), (0, 0), "nothing appended");
                return;
            };
            assert_eq!((seq, control.log.head(), control.seek_gen()), (0, 1, 1));
            assert!(finish == Finish::NotYet && !control.is_finished(), "the seek was first");
            assert_eq!(end_and_ask(&control, &cursors[0], 1), Finish::NotYet, "one sink revived");
            assert_eq!(end_and_ask(&control, &cursors[1], 1), Finish::Done);
        });
    }

    #[test]
    fn loom_two_sinks_ending_against_a_seek_never_split_the_run() {
        let mut model = loom::model::Builder::new();
        model.preemption_bound = Some(THREE_THREAD_PREEMPTIONS);
        model.check(|| {
            let (control, cursors) = two_sinks();
            let sinks = [sink(&control, &cursors[0]), sink(&control, &cursors[1])];
            let seek = control.append(cmd(0));
            let done = sinks.map(|s| (s.join().unwrap() == Finish::Done) as u32);
            assert_eq!(done[0] + done[1], seek.is_err() as u32);
            assert_eq!(control.is_finished(), seek.is_err());
            assert_eq!(control.log.head(), seek.is_ok() as u64);
        });
    }
}
