//! The app's side of a run. What the command log must answer to each action follows from the
//! facts kept here, so an answer it cannot explain is a verdict. Pure data, no core.

use super::map::{Map, Rate};
use crate::action::{ActionKind, MAX_ACTIONS};
use crate::time::{NONE, Ns};

pub const MAX_GENERATIONS: usize = MAX_ACTIONS + 1;

pub const MAX_JUMPS: usize = MAX_ACTIONS;

pub const MAX_SOURCES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Jump {
    pub seek_gen: u32,
    pub to_time: Ns,
    /// Chosen by the seek rule. One that was not may restart a decoder where it has no reference.
    pub planned: bool,
    pub from: Ns,
}

const NO_JUMP: Jump = Jump { seek_gen: 0, to_time: 0, planned: false, from: NONE };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shift {
    pub sink: u16,
    pub by: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Answer {
    Taken = 0,
    Full = 1,
    Redundant = 2,
    NotRunning = 3,
    Finished = 4,
    NotDriven = 5,
    Unfired = 6,
    NotASink = 7,
    /// Core did not know the element a command named. The driver's own hold-back is
    /// `NotDriven`, this one the model must explain, or it is a refusal.
    NoSuchElement = 8,
}

impl Answer {
    pub const fn from_code(code: u64) -> Answer {
        match code {
            0 => Answer::Taken,
            1 => Answer::Full,
            2 => Answer::Redundant,
            3 => Answer::NotRunning,
            4 => Answer::Finished,
            5 => Answer::NotDriven,
            7 => Answer::NotASink,
            8 => Answer::NoSuchElement,
            _ => Answer::Unfired,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Answer::Taken => "taken",
            Answer::Full => "refused, log full",
            Answer::Redundant => "refused, redundant",
            Answer::NotRunning => "refused, not running",
            Answer::Finished => "refused, finished",
            Answer::NotDriven => "not driven",
            Answer::Unfired => "never fired",
            Answer::NotASink => "refused, not a sink",
            Answer::NoSuchElement => "refused, no such element",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct App {
    pub paused: bool,
    pub stopped: bool,
    pub finished: bool,
    pub seek_gen: u32,
    pub taken: u32,
    pub rates: u32,
    asked: [u64; MAX_ACTIONS],
    pub map: Map,
    sinks: u16,
    targets: [Ns; MAX_GENERATIONS],
    bytes: [[u64; MAX_SOURCES]; MAX_GENERATIONS],
    jumps: [Jump; MAX_JUMPS],
    jump_count: u16,
    shifts: [Shift; MAX_ACTIONS],
    shift_count: u16,
    pub raw: bool,
}

impl App {
    pub const fn new() -> App {
        App::started(0, false, 1)
    }

    pub const fn started(now: Ns, retimer: bool, sinks: u16) -> App {
        App {
            paused: false,
            stopped: false,
            finished: false,
            seek_gen: 0,
            taken: 0,
            rates: 0,
            asked: [0; MAX_ACTIONS],
            map: Map::new(now, retimer),
            sinks,
            targets: [0; MAX_GENERATIONS],
            bytes: [[0; MAX_SOURCES]; MAX_GENERATIONS],
            jumps: [NO_JUMP; MAX_JUMPS],
            jump_count: 0,
            shifts: [Shift { sink: 0, by: 0 }; MAX_ACTIONS],
            shift_count: 0,
            raw: false,
        }
    }

    pub fn byte(&self, seek_gen: u32) -> u64 {
        self.byte_of(seek_gen, 0)
    }

    pub fn byte_of(&self, seek_gen: u32, source: usize) -> u64 {
        let named = self.bytes.get(seek_gen as usize);
        named.and_then(|bytes| bytes.get(source)).copied().unwrap_or(0)
    }

    pub fn jumps(&self) -> &[Jump] {
        &self.jumps[..(self.jump_count as usize).min(MAX_JUMPS)]
    }

    pub fn asked(&self) -> &[u64] {
        &self.asked[..(self.rates as usize).min(MAX_ACTIONS)]
    }

    pub fn shifts(&self) -> &[Shift] {
        &self.shifts[..(self.shift_count as usize).min(MAX_ACTIONS)]
    }

    fn jumped(&mut self, jump: Jump) {
        self.taken += 1;
        self.raw |= !jump.planned;
        if let Some(slot) = self.jumps.get_mut(self.jump_count as usize) {
            *slot = jump;
            self.jump_count += 1;
        }
    }

    pub fn sought(&mut self, now: Ns, to_time: Ns, byte: u64, planned: bool) {
        self.sought_each(now, to_time, [byte; MAX_SOURCES], planned);
    }

    pub fn sought_each(&mut self, now: Ns, to_time: Ns, bytes: [u64; MAX_SOURCES], planned: bool) {
        self.seek_gen += 1;
        let at = self.seek_gen as usize;
        if let (Some(target), Some(named)) = (self.targets.get_mut(at), self.bytes.get_mut(at)) {
            (*target, *named) = (to_time, bytes);
        }
        self.map.seek(now, to_time);
        self.jumped(Jump { seek_gen: self.seek_gen, to_time, planned, from: NONE });
    }

    pub fn skipped(&mut self, now: Ns, to_time: Ns, planned: bool) {
        self.map.skip(now, to_time);
        self.jumped(Jump { seek_gen: 0, to_time, planned, from: NONE });
    }

    pub fn skipped_from(&mut self, now: Ns, to_time: Ns, from: Ns) {
        self.map.skip(now, to_time);
        self.jumped(Jump { seek_gen: 0, to_time, planned: true, from });
    }

    pub fn target(&self, seek_gen: u32) -> Ns {
        self.targets.get(seek_gen as usize).copied().unwrap_or(0)
    }

    /// `Full` is the one answer the model cannot rule out, so [`Self::explains`] lets it through.
    pub fn expect(&self, kind: ActionKind) -> Answer {
        let redundant = match kind {
            ActionKind::Seek { .. } | ActionKind::SkipTo { .. } => false,
            ActionKind::Pause => self.paused,
            ActionKind::Resume => !self.paused,
            ActionKind::Stop => self.stopped,
            ActionKind::SetRate { num, den } => match Rate::new(num as u64, den as u64) {
                Some(rate) => rate == self.map.rate(),
                None => return Answer::NotDriven,
            },
            ActionKind::SetOffset { sink, .. } if sink >= self.sinks && !self.finished => {
                return Answer::NotASink;
            }
            ActionKind::SetOffset { .. } => false,
            _ => return Answer::NotDriven,
        };
        match (self.finished, redundant) {
            (true, _) => Answer::Finished,
            (false, true) => Answer::Redundant,
            (false, false) => Answer::Taken,
        }
    }

    pub fn explains(&self, kind: ActionKind, answer: Answer) -> bool {
        let expected = self.expect(kind);
        answer == expected || (answer == Answer::Full && expected == Answer::Taken)
    }

    pub fn room_for_seek(&self) -> bool {
        (self.seek_gen as usize) < MAX_GENERATIONS - 1
    }

    pub fn apply(&mut self, now: Ns, kind: ActionKind, answer: Answer) {
        if answer != Answer::Taken {
            return;
        }
        match kind {
            // A seek with no byte of its own is to the start of the file, which decodes.
            ActionKind::Seek { to } => return self.sought(now, to, 0, true),
            ActionKind::SkipTo { to } => return self.skipped(now, to, false),
            ActionKind::Pause => {
                self.paused = true;
                self.map.pause(now);
            }
            ActionKind::Resume => {
                self.paused = false;
                self.map.resume(now);
            }
            ActionKind::Stop => {
                self.stopped = true;
                self.map.touch(now);
            }
            ActionKind::SetRate { num, den } => {
                let Some(rate) = Rate::new(num as u64, den as u64) else {
                    return;
                };
                if let Some(slot) = self.asked.get_mut(self.rates as usize) {
                    *slot = rate.pack();
                }
                self.rates += 1;
                self.map.set_rate(now, rate);
            }
            ActionKind::SetOffset { sink, by } => {
                if let Some(slot) = self.shifts.get_mut(self.shift_count as usize) {
                    *slot = Shift { sink, by };
                    self.shift_count += 1;
                }
                self.map.touch(now);
            }
            _ => {}
        }
        self.taken += 1;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEEK: ActionKind = ActionKind::Seek { to: 7 };

    #[test]
    fn the_model_follows_what_the_log_took() {
        let mut app = App::new();
        assert_eq!(app.expect(ActionKind::Resume), Answer::Redundant);
        assert_eq!(app.expect(ActionKind::Pause), Answer::Taken);
        app.apply(0, ActionKind::Pause, Answer::Taken);
        assert_eq!(app.expect(ActionKind::Pause), Answer::Redundant);
        app.apply(0, SEEK, Answer::Taken);
        app.apply(0, ActionKind::Seek { to: 9 }, Answer::Full);
        assert_eq!((app.seek_gen, app.target(1), app.target(2), app.taken), (1, 7, 0, 2));
        app.apply(0, ActionKind::Stop, Answer::Taken);
        assert_eq!(app.expect(ActionKind::Stop), Answer::Redundant);
        assert_eq!(app.expect(SEEK), Answer::Taken, "a stopped run still takes a seek");
    }

    #[test]
    fn the_model_keeps_the_seeks_and_skips_in_the_order_the_log_took_them() {
        let mut app = App::new();
        app.sought(0, 40, 12, true);
        app.skipped(0, 90, true);
        app.apply(0, ActionKind::Pause, Answer::Taken);
        app.skipped(0, 20, false);
        app.apply(0, SEEK, Answer::Taken);
        let jump = |seek_gen, to_time, planned| Jump { seek_gen, to_time, planned, from: NONE };
        let want = [jump(1, 40, true), jump(0, 90, true), jump(0, 20, false), jump(2, 7, true)];
        assert_eq!(app.jumps(), want);
        assert_eq!((app.seek_gen, app.taken, app.raw), (2, 5, true));
        assert_eq!((app.target(1), app.byte(1), app.target(2), app.byte(2)), (40, 12, 7, 0));
        assert_eq!(app.byte(9), 0, "a generation the model never saw");
        app.sought_each(0, 3, [5, 8], true);
        assert_eq!((app.byte_of(3, 0), app.byte_of(3, 1), app.byte_of(3, 2)), (5, 8, 0));
    }

    #[test]
    fn a_finished_run_takes_nothing() {
        let app = App { finished: true, ..App::new() };
        let rate = ActionKind::SetRate { num: 2, den: 1 };
        let stray = ActionKind::SetOffset { sink: 4, by: 1 };
        for kind in [SEEK, ActionKind::Pause, ActionKind::Resume, ActionKind::Stop, rate, stray] {
            assert_eq!(app.expect(kind), Answer::Finished);
            assert!(!app.explains(kind, Answer::Taken));
            assert!(!app.explains(kind, Answer::Full));
        }
    }

    #[test]
    fn a_full_log_is_explained_only_where_the_command_would_have_gone_in() {
        let app = App::new();
        assert!(app.explains(SEEK, Answer::Full));
        assert!(!app.explains(ActionKind::Resume, Answer::Full));
        assert!(!app.explains(SEEK, Answer::Redundant));
        assert_eq!(app.expect(ActionKind::SelectStream { stream: 0 }), Answer::NotDriven);
    }

    #[test]
    fn a_rate_is_redundant_at_the_rate_last_asked_or_said_and_an_offset_needs_a_sink() {
        let mut app = App::started(0, true, 2);
        let rate = |num, den| ActionKind::SetRate { num, den };
        assert_eq!(app.expect(rate(1, 1)), Answer::Redundant);
        assert_eq!(app.expect(rate(3, 3)), Answer::Redundant, "reduced first");
        assert_eq!(app.expect(rate(0, 1)), Answer::NotDriven, "no rate at all");
        app.apply(5, rate(2, 1), Answer::Taken);
        assert_eq!((app.rates, app.taken, app.map.version), (1, 1, 1));
        assert_eq!(
            (app.expect(rate(4, 2)), app.expect(rate(1, 1))),
            (Answer::Redundant, Answer::Taken)
        );
        app.map.retime(6, 100, Rate::ONE, true);
        assert_eq!(
            (app.expect(rate(2, 1)), app.expect(rate(1, 1))),
            (Answer::Taken, Answer::Redundant)
        );
        let shift = |sink, by| ActionKind::SetOffset { sink, by };
        assert_eq!(
            (app.expect(shift(1, -3)), app.expect(shift(2, -3))),
            (Answer::Taken, Answer::NotASink)
        );
        app.apply(7, shift(1, -3), Answer::Taken);
        app.apply(7, shift(2, 9), Answer::NotASink);
        assert_eq!(app.shifts(), [Shift { sink: 1, by: -3 }]);
        assert_eq!(app.map.changed_at, 7);
    }

    #[test]
    fn answers_round_trip_through_their_codes() {
        for answer in [
            Answer::Taken,
            Answer::Full,
            Answer::Redundant,
            Answer::NotRunning,
            Answer::Finished,
            Answer::NotDriven,
            Answer::Unfired,
            Answer::NotASink,
        ] {
            assert_eq!(Answer::from_code(answer as u64), answer);
            assert!(!answer.name().is_empty());
        }
    }
}
