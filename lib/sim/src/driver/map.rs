//! The timeline as the oracles model it, written by core's control block rules from the commands
//! the log took. Pure integers, no core.

use crate::time::{NONE, Ns};

pub const REACH: usize = 7;

const MAX: Ns = NONE - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    pub num: u16,
    pub den: u16,
}

impl Rate {
    pub const ONE: Rate = Rate { num: 1, den: 1 };
    pub const LIMIT: u64 = 64;

    /// `None` for zero, a rate outside the limit, or terms that do not fit sixteen bits once
    /// reduced.
    pub fn new(num: u64, den: u64) -> Option<Rate> {
        let (mut a, mut b) = (num, den);
        // Euclid's algorithm, 93 steps at most for terms of 64 bits.
        for _ in 0..96 {
            if b == 0 {
                break;
            }
            (a, b) = (b, a % b);
        }
        let (n, d) = (num / a.max(1), den / a.max(1));
        let within =
            n != 0 && d != 0 && n.div_ceil(Self::LIMIT) <= d && d.div_ceil(Self::LIMIT) <= n;
        match (u16::try_from(n), u16::try_from(d)) {
            (Ok(num), Ok(den)) if within => Some(Rate { num, den }),
            _ => None,
        }
    }

    pub const fn pack(self) -> u64 {
        self.num as u64 | (self.den as u64) << 16
    }

    /// `None` for a zero term, which no rate has and a row must not carry.
    pub const fn unpack(word: u64) -> Option<Rate> {
        let (num, den) = (word as u16, (word >> 16) as u16);
        if num == 0 || den == 0 {
            return None;
        }
        Some(Rate { num, den })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub stream_start: Ns,
    pub running_start: Ns,
    pub rate: Rate,
}

/// `at + d * mul / div`, floored toward minus infinity. The one rounding of the map.
fn scale(at: Ns, d: i128, mul: u16, div: u16) -> i128 {
    at as i128 + (d * mul as i128).div_euclid(div.max(1) as i128)
}

fn clamp(t: i128) -> Ns {
    t.clamp(0, MAX as i128) as Ns
}

impl Piece {
    pub fn running(&self, t: Ns) -> i128 {
        scale(
            self.running_start,
            t as i128 - self.stream_start as i128,
            self.rate.den,
            self.rate.num,
        )
    }

    pub fn stream(&self, r: Ns) -> i128 {
        scale(
            self.stream_start,
            r as i128 - self.running_start as i128,
            self.rate.num,
            self.rate.den,
        )
    }
}

const START: Piece = Piece { stream_start: 0, running_start: 0, rate: Rate::ONE };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Map {
    pieces: [Piece; REACH],
    len: usize,
    base: i64,
    paused: bool,
    frozen: Ns,
    rate: Rate,
    retimer: bool,
    point: Ns,
    /// The clock at the last thing that moved a deadline. A sink that waited may be late only
    /// here.
    pub changed_at: Ns,
    pub version: u32,
}

impl Map {
    pub const fn new(now: Ns, retimer: bool) -> Map {
        let base = if now > i64::MAX as u64 { i64::MAX } else { now as i64 };
        Map {
            pieces: [START; REACH],
            len: 1,
            base,
            paused: false,
            frozen: 0,
            rate: Rate::ONE,
            retimer,
            point: 0,
            changed_at: NONE,
            version: 0,
        }
    }

    pub const fn rate(&self) -> Rate {
        self.rate
    }

    pub const fn base(&self) -> i64 {
        self.base
    }

    pub fn pieces(&self) -> &[Piece] {
        &self.pieces[..self.len.clamp(1, REACH)]
    }

    pub fn newest(&self) -> Piece {
        self.pieces().last().copied().unwrap_or(START)
    }

    fn changed(&mut self, now: Ns) {
        (self.changed_at, self.version) = (now, self.version.wrapping_add(1));
    }

    pub fn touch(&mut self, now: Ns) {
        self.changed(now);
    }

    fn find(&self, at: Ns, by_stream: bool) -> Piece {
        let pieces = self.pieces();
        let start = |p: &Piece| {
            if by_stream { p.stream_start } else { p.running_start }
        };
        let found = pieces.iter().rev().find(|p| start(p) <= at);
        found.or(pieces.first()).copied().unwrap_or(START)
    }

    fn push(&mut self, piece: Piece, restart: bool) {
        if restart {
            self.len = 0;
        }
        if self.len >= REACH {
            self.pieces.copy_within(1.., 0);
            self.len = REACH - 1;
        }
        self.pieces[self.len] = piece;
        self.len += 1;
    }

    pub fn running(&self, t: Ns) -> i128 {
        self.find(t, true).running(t)
    }

    pub fn stream(&self, r: Ns) -> Ns {
        if r == NONE { r } else { clamp(self.find(r, false).stream(r)) }
    }

    pub fn running_now(&self, now: Ns) -> Ns {
        if self.paused { self.frozen } else { clamp(now as i128 - self.base as i128) }
    }

    pub fn position(&self, now: Ns) -> Ns {
        self.stream(self.running_now(now))
    }

    pub fn due_at(&self, pts: Ns, latency: Ns, offset: i64) -> Ns {
        if pts == NONE {
            return NONE;
        }
        let at = clamp(self.running(pts)).checked_add(latency).map_or(MAX, |sum| sum.min(MAX));
        clamp(at as i128 + offset as i128)
    }

    pub fn deadline(&self, at: Ns) -> Ns {
        if at == NONE { NONE } else { clamp(self.base as i128 + at as i128) }
    }

    fn rebase(&mut self, now: Ns, target: Ns) {
        if self.paused {
            self.frozen = target;
        }
        let ns = |t: Ns| t.min(i64::MAX as u64) as i64;
        self.base = ns(now) - ns(target);
    }

    /// Never before the newest piece or the presentation point, and the same rate writes nothing.
    fn extend(&mut self, start: Ns, rate: Rate) {
        let last = self.newest();
        let start = start.max(last.stream_start).max(self.point);
        if rate != last.rate && start != NONE {
            let running_start = clamp(last.running(start));
            self.push(Piece { stream_start: start, running_start, rate }, false);
        }
    }

    pub fn seek(&mut self, now: Ns, to: Ns) {
        self.rebase(now, to);
        self.point = to;
        self.push(Piece { stream_start: to, running_start: to, rate: self.rate }, true);
        self.changed(now);
    }

    pub fn skip(&mut self, now: Ns, to: Ns) {
        let running = clamp(self.running(to));
        self.rebase(now, running);
        self.point = to;
        self.changed(now);
    }

    pub fn set_rate(&mut self, now: Ns, rate: Rate) {
        self.rate = rate;
        if !self.retimer {
            self.extend(self.position(now), rate);
        }
        self.changed(now);
    }

    pub fn pause(&mut self, now: Ns) {
        (self.frozen, self.paused) = (clamp(now as i128 - self.base as i128), true);
        self.changed(now);
    }

    pub fn resume(&mut self, now: Ns) {
        self.rebase(now, self.frozen);
        self.paused = false;
        self.changed(now);
    }

    /// `current` is whether its group had flushed for the newest seek. One that had not speaks of
    /// the stream before, and the restart stands.
    pub fn retime(&mut self, now: Ns, start: Ns, rate: Rate, current: bool) {
        if !current {
            return;
        }
        self.rate = rate;
        self.extend(start, rate);
        self.changed(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{MS, SEC};

    fn rate(num: u64, den: u64) -> Rate {
        Rate::new(num, den).expect("a rate the timeline takes")
    }

    #[test]
    fn a_rate_is_reduced_and_held_to_the_limits() {
        assert_eq!(rate(150, 100), Rate { num: 3, den: 2 });
        assert_eq!(rate(64, 1).num, 64);
        assert_eq!(rate(1, 64).den, 64);
        for (num, den) in [(0, 1), (1, 0), (65, 1), (1, 65), (65_537, 65_536)] {
            assert_eq!(Rate::new(num, den), None, "{num}/{den}");
        }
        assert_eq!(Rate::unpack(rate(21, 20).pack()), Some(rate(21, 20)));
        assert_eq!(Rate::unpack(0), None);
        assert_eq!(Rate::unpack(rate(21, 20).pack() & 0xFFFF), None, "a zero denominator");
    }

    #[test]
    fn a_run_starts_at_rate_one_with_the_clock_as_its_base() {
        let map = Map::new(SEC, false);
        assert_eq!((map.running(0), map.running(7 * MS)), (0, 7 * MS as i128));
        assert_eq!((map.position(SEC), map.position(SEC + 40)), (0, 40));
        assert_eq!(map.deadline(map.due_at(40 * MS, 0, 0)), SEC + 40 * MS);
        assert_eq!(map.deadline(map.due_at(40 * MS, 5 * MS, -(MS as i64))), SEC + 44 * MS);
        assert_eq!(map.due_at(NONE, 0, 0), NONE, "no pts is never due");
        assert_eq!(map.due_at(MS, 0, -(SEC as i64)), 0, "an offset stops at zero");
    }

    #[test]
    fn a_rate_with_no_retimer_starts_at_what_is_presented_and_is_continuous() {
        let mut map = Map::new(SEC, false);
        map.set_rate(2 * SEC, rate(2, 1));
        assert_eq!(map.newest(), Piece { stream_start: SEC, running_start: SEC, rate: rate(2, 1) });
        assert_eq!((map.running(SEC), map.running(3 * SEC)), (SEC as i128, 2 * SEC as i128));
        assert_eq!(map.running(SEC / 2), (SEC / 2) as i128, "what was presented keeps its time");
        map.set_rate(3 * SEC, rate(3, 4));
        assert_eq!(map.newest().stream_start, 3 * SEC);
        assert_eq!(map.running(3 * SEC + 300), 2 * SEC as i128 + 400);
        assert_eq!(map.position(3 * SEC + 400), 3 * SEC + 300);
        let before = map;
        map.set_rate(4 * SEC, rate(3, 4));
        assert_eq!(map.pieces(), before.pieces());
        assert_eq!((map.changed_at, map.version), (4 * SEC, before.version + 1));
    }

    #[test]
    fn a_seek_restarts_at_the_rate_asked_and_a_skip_keeps_the_pieces() {
        let mut map = Map::new(0, false);
        map.set_rate(SEC, rate(3, 2));
        map.seek(2 * SEC, 10 * SEC);
        assert_eq!(
            map.pieces(),
            [Piece { stream_start: 10 * SEC, running_start: 10 * SEC, rate: rate(3, 2) }]
        );
        assert_eq!((map.position(2 * SEC), map.base()), (10 * SEC, -8 * SEC as i64));
        assert_eq!(map.running(7 * SEC), 8 * SEC as i128);
        assert_eq!(map.deadline(map.due_at(13 * SEC, 0, 0)), 4 * SEC);
        let pieces = *map.pieces().first().unwrap();
        map.skip(3 * SEC, 16 * SEC);
        assert_eq!(map.pieces(), [pieces]);
        assert_eq!(map.position(3 * SEC), 16 * SEC);
        assert_eq!(map.deadline(map.due_at(19 * SEC, 0, 0)), 5 * SEC);
    }

    #[test]
    fn a_pause_freezes_running_time_and_a_resume_goes_on_from_there() {
        let mut map = Map::new(0, false);
        map.pause(SEC);
        assert_eq!(
            (map.running_now(SEC), map.running_now(4 * SEC), map.position(4 * SEC)),
            (SEC, SEC, SEC)
        );
        map.set_rate(3 * SEC, rate(1, 2));
        assert_eq!(map.newest().stream_start, SEC);
        map.resume(5 * SEC);
        assert_eq!(map.position(5 * SEC), SEC);
        assert_eq!(map.deadline(map.due_at(2 * SEC, 0, 0)), 7 * SEC);
        map.pause(6 * SEC);
        map.seek(8 * SEC, 0);
        map.resume(9 * SEC);
        assert_eq!(map.position(9 * SEC), 0);
    }

    #[test]
    fn a_retimer_writes_the_pieces_and_the_command_writes_none() {
        let mut map = Map::new(0, true);
        map.set_rate(SEC, rate(2, 1));
        assert_eq!((map.pieces().len(), map.rate()), (1, rate(2, 1)));
        map.retime(SEC, 1_500 * MS, rate(2, 1), true);
        assert_eq!(map.newest().running_start, 1_500 * MS);
        assert_eq!(map.running(2_500 * MS), 2 * SEC as i128);
        map.retime(SEC, SEC, rate(1, 1), true);
        assert_eq!(map.newest().stream_start, 1_500 * MS);
        map.skip(SEC, 4 * SEC);
        map.retime(SEC, 2 * SEC, rate(4, 1), true);
        assert_eq!(map.newest().stream_start, 4 * SEC);
        let before = map;
        map.retime(SEC, 9 * SEC, rate(1, 4), false);
        assert_eq!(map, before);
    }

    #[test]
    fn a_time_out_of_reach_maps_through_the_oldest_piece_kept() {
        let mut map = Map::new(0, true);
        for k in 1..=REACH as u64 + 1 {
            map.retime(0, k * SEC, rate(1 + k % 2, 1), true);
        }
        assert_eq!(map.pieces().len(), REACH);
        let oldest = map.pieces()[0];
        assert_eq!(oldest.stream_start, 2 * SEC, "the start and the first change are gone");
        assert_eq!(map.running(0), oldest.running(0));
        assert_eq!(map.stream(0), clamp(oldest.stream(0)));
    }
}
