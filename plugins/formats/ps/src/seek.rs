//! Where a seek lands in a program stream read by range, by bisecting the pack headers on their
//! system clock reference. The landing is the last pack at or before the target.

use crate::pack::{self, Need, PACK};

/// The bytes of a probe, some 30 DVD packs.
pub const PROBE_LEN: u32 = 64 << 10;
/// Probes a seek takes at most before landing on the best pack found.
pub const PROBES_MAX: u32 = 48;

const MODULUS: i64 = 1 << 33;

/// Maps a raw 33-bit clock to 90 kHz ticks since the file's first, the congruent value at or past
/// `slack` ticks ahead of the start, so a file starting near the wrap reads on across it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base {
    pub origin: i64,
    pub slack: i64,
}

impl Base {
    pub fn ticks(&self, raw: u64) -> i64 {
        let floor = self.origin - self.slack;
        floor + (raw as i64 - floor).rem_euclid(MODULUS) - self.origin
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landing {
    pub at: u64,
    pub ticks: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Probe { at: u64, len: u32 },
    Land(Option<Landing>),
}

#[derive(Clone, Debug)]
pub struct Bisect {
    base: Base,
    target: i64,
    len: u64,
    lo: u64,
    hi: u64,
    best: Option<Landing>,
    probes: u32,
}

fn next_pack(data: &[u8], mut from: usize) -> Option<(usize, u64)> {
    while let Some(c) = data.get(from..)?.windows(4).position(|w| w == [0, 0, 1, PACK]) {
        let c = from + c;
        if let (Need::Len(n), Ok(p)) = (pack::item_len(&data[c..]), pack::parse_pack(&data[c..]))
            && c + n <= data.len()
        {
            return Some((c, p.scr));
        }
        from = c + 1;
    }
    None
}

impl Bisect {
    /// The landing for `target` ticks since the file's first in a file `len` bytes long.
    pub fn new(base: Base, target: i64, len: u64) -> Bisect {
        Bisect { base, target, len, lo: 0, hi: len, best: None, probes: 0 }
    }

    pub fn step(&self) -> Step {
        if self.lo >= self.hi || self.probes >= PROBES_MAX {
            return Step::Land(self.best);
        }
        let probe = u64::from(PROBE_LEN);
        let (at, len) = match self.hi - self.lo <= probe {
            true => (self.lo, self.hi - self.lo + 32),
            false => (self.lo + (self.hi - self.lo) / 2, probe),
        };
        match len.min(probe).min(self.len.saturating_sub(at)) {
            0 => Step::Land(self.best),
            n => Step::Probe { at, len: n as u32 },
        }
    }

    pub fn served(&mut self, at: u64, data: &[u8]) {
        self.probes += 1;
        let mut found = false;
        let mut from = 0;
        while let Some((c, scr)) = next_pack(data, from) {
            from = c + 1;
            let start = at + c as u64;
            if start >= self.hi {
                break;
            }
            if start < self.lo {
                continue;
            }
            let ticks = self.base.ticks(scr);
            if ticks > self.target {
                self.hi = if found { start } else { at.max(self.lo) };
                return;
            }
            found = true;
            self.best = Some(Landing { at: start, ticks });
            self.lo = start + 1;
        }
        if !found {
            self.hi = self.hi.min(at.max(self.lo));
        }
    }
}

/// One time base in a window of a file, first decode and last presentation time in ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub first: i64,
    pub last: i64,
}

/// Runs a window keeps at most, a hostile one breaks every unit.
pub const RUNS_MAX: usize = 64;

/// The time bases of a window of a file in the order its stamps are read, a 10 s step starting a
/// new one.
#[derive(Clone, Debug)]
pub struct Line {
    unwrap: pf_mpegts::Unwrapper,
    runs: Vec<Run>,
    /// More runs than [`RUNS_MAX`] were read.
    lost: bool,
}

impl Default for Line {
    #[allow(clippy::disallowed_methods)] // once per scanned window
    fn default() -> Self {
        Line { unwrap: pf_mpegts::Unwrapper::for_timestamps(), runs: Vec::new(), lost: false }
    }
}

impl Line {
    pub fn note(&mut self, dts: u64, pts: u64) {
        let d = self.unwrap.unwrap(dts);
        let p = self.unwrap.unwrap(pts).ticks.max(d.ticks);
        self.unwrap.prime(d.ticks);
        let full = self.runs.len() >= RUNS_MAX;
        match self.runs.last_mut() {
            Some(r) if d.step != pf_mpegts::Step::Discontinuity => r.last = r.last.max(p),
            _ if full => self.lost = true,
            _ => self.runs.push(Run { first: d.ticks, last: p }),
        }
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }
}

/// Bytes against clock ticks over the stretches of the windows read on one base, the file's
/// mean rate when they are long enough.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rate {
    pub bytes: u64,
    pub ticks: i64,
    last: Option<(u64, i64)>,
}

/// A second of clock, less tells little of a VBR mean.
const RATE_TICKS_MIN: i64 = 90_000;
/// A file this much longer than the rate of its windows says is taken for one whose clock was set
/// back, read as a wrap, or jumped ahead. Shorter is no sign, quiet windows say too long.
const WRAP_FACTOR: i64 = 4;
const JUMP_FACTOR: i64 = 8;

impl Rate {
    /// A clock reference read at byte `at`, 90 kHz. A step back or past 10 s starts a stretch.
    pub fn note(&mut self, at: u64, ticks: i64) {
        if let Some((from, then)) = self.last
            && at > from
            && (0..=10 * 90_000).contains(&(ticks - then))
        {
            (self.bytes, self.ticks) = (self.bytes + (at - from), self.ticks + (ticks - then));
        }
        self.last = Some((at, ticks));
    }

    /// A new window, its first reference no stretch on from the last one's.
    pub fn restart(&mut self) {
        self.last = None;
    }

    /// Ticks a file `len` bytes long lasts at this rate, `None` short of a second measured.
    pub fn lasts(&self, len: u64) -> Option<i64> {
        (self.ticks >= RATE_TICKS_MIN && self.bytes > 0)
            .then(|| (i128::from(len) * i128::from(self.ticks) / i128::from(self.bytes)) as i64)
    }
}

/// Ticks from `least`, the head's earliest decode time, to the tail's last stamp, the runs of
/// both summed and the tail's first going on from the head's last. `lasts`, the file at its
/// windows' rate, rules out a join far longer than it. One that reads as the clock wrapping, which
/// a clock set back reads the same, is held to a tighter bound, and with no rate `trust_wrap`
/// says. `None` when ruled out or more runs than kept were read.
pub fn extent(
    head: &Line,
    tail: &Line,
    least: i64,
    lasts: Option<i64>,
    trust_wrap: bool,
) -> Option<i64> {
    if head.lost || tail.lost {
        return None;
    }
    let (end, done) = head.runs().split_last()?;
    let start = |n: usize, r: &Run| if n == 0 { r.first.min(least) } else { r.first };
    let mut ticks: i64 = done.iter().enumerate().map(|(n, r)| r.last - start(n, r)).sum();
    let open = start(done.len(), end);
    let Some((t1, rest)) = tail.runs().split_first() else {
        return Some((ticks + end.last - open).max(0));
    };
    let floor = end.first - ORIGIN_SLACK;
    let on = pf_mpegts::congruent(floor, t1.first.rem_euclid(MODULUS) as u64) - t1.first;
    let wrapped = end.first.rem_euclid(MODULUS) + (t1.first + on - end.first) >= MODULUS;
    ticks += (t1.last + on).max(end.last) - open;
    ticks += rest.iter().map(|r| r.last - r.first).sum::<i64>();
    let plausible = match (wrapped, lasts) {
        (false, Some(lasts)) => ticks <= lasts.saturating_mul(JUMP_FACTOR),
        (true, Some(lasts)) => ticks <= lasts.saturating_mul(WRAP_FACTOR),
        (false, None) => true,
        (true, None) => trust_wrap,
    };
    plausible.then_some(ticks.max(0))
}

/// Slack below a time base's first decode time for a stamp read past it, as the demuxers take it.
const ORIGIN_SLACK: i64 = 10 * 90_000;

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test vectors
mod tests {
    use super::*;

    const S: u64 = 90_000;

    fn line(stamps: &[u64]) -> Line {
        let mut l = Line::default();
        stamps.iter().for_each(|&s| l.note(s % (1 << 33), s % (1 << 33)));
        l
    }

    fn secs(from: u64, to: u64) -> Vec<u64> {
        (from..=to).map(|s| s * S).collect()
    }

    #[test]
    fn the_extent_sums_the_runs_the_windows_show() {
        let hour = 3600 * S;
        // One base, the tail 2 h on.
        let (head, tail) = (line(&secs(10, 20)), line(&secs(7190, 7210)));
        assert_eq!(extent(&head, &tail, 10 * S as i64, None, false), Some(7200 * S as i64));
        // The head restarts at 0 after 10 s, the tail is 1 h on from there.
        let head = line(&[secs(36_000, 36_010), secs(0, 5)].concat());
        let tail = line(&secs(3590, 3600));
        let got = extent(&head, &tail, (36_000 * S) as i64, None, false);
        assert_eq!(got, Some((10 + 3600) * S as i64), "summed across the head's break");
        // The tail restarts, 2 h in before it, 30 s after.
        let tail = line(&[secs(7190, 7200), secs(100, 130)].concat());
        let got = extent(&line(&secs(0, 10)), &tail, 0, None, false);
        assert_eq!(got, Some((7200 + 30) * S as i64), "summed across the tail's break");
        // 10 h, then 1 h somewhere between, taken for a wrap only when nothing says otherwise.
        let (head, tail) = (line(&secs(10 * 3600, 10 * 3600 + 5)), line(&secs(3600, 3605)));
        let least = (10 * hour) as i64;
        let wrapped = (1 << 33) - 9 * hour as i64 + 5 * S as i64;
        assert_eq!(extent(&head, &tail, least, None, true), Some(wrapped));
        assert_eq!(extent(&head, &tail, least, None, false), None);
        let two_hours = Some(2 * hour as i64);
        assert_eq!(extent(&head, &tail, least, two_hours, true), None, "17.5 h against 2 h");
        assert_eq!(extent(&head, &tail, least, Some(wrapped / 2), false), Some(wrapped));
        // 10 s, then 10 h on, told by the rate only
        let (head, tail) = (line(&secs(0, 10)), line(&secs(36_000, 36_005)));
        assert_eq!(extent(&head, &tail, 0, None, false), Some(36_005 * S as i64));
        assert_eq!(extent(&head, &tail, 0, Some(60 * S as i64), false), None, "a jump ahead");
        let quiet = Some(10 * 36_005 * S as i64);
        assert_eq!(extent(&head, &tail, 0, quiet, false), Some(36_005 * S as i64), "quiet windows");
        // A real wrap, 1 h on from 30 min short of the modulus.
        let first = (1 << 33) - 1800 * S;
        let (head, tail) = (line(&[first, first + S]), line(&[first + 3600 * S]));
        let rate = Some(3500 * S as i64);
        assert_eq!(extent(&head, &tail, first as i64, rate, false), Some(3600 * S as i64));
        // A file inside its head, and too many runs.
        let inside = extent(&line(&secs(4, 9)), &Line::default(), 4 * S as i64, None, false);
        assert_eq!(inside, Some(5 * S as i64));
        let broken: Vec<u64> = (0..2 * RUNS_MAX as u64).map(|n| n * 20 * S).collect();
        assert_eq!(extent(&line(&broken), &tail, 0, None, true), None);
    }

    #[test]
    fn the_rate_counts_stretches_on_one_clock() {
        let mut r = Rate::default();
        assert_eq!(r.lasts(1000), None);
        for n in 0..=10u64 {
            r.note(n * 1000, (n * S / 10) as i64);
        }
        assert_eq!((r.bytes, r.ticks), (10_000, S as i64));
        r.note(11_000, 50);
        r.note(12_000, 50 + S as i64 / 10);
        r.restart();
        r.note(500_000, 7 * S as i64);
        r.note(501_000, 7 * S as i64 + S as i64 / 10);
        assert_eq!((r.bytes, r.ticks), (12_000, 12 * S as i64 / 10));
        assert_eq!(r.lasts(120_000), Some(12 * S as i64));
    }

    fn pack(scr: u64) -> Vec<u8> {
        let v = 1u64 << 46
            | (scr >> 30 & 7) << 43
            | 1 << 42
            | (scr >> 15 & 0x7FFF) << 27
            | 1 << 26
            | (scr & 0x7FFF) << 11
            | 1 << 10
            | 1;
        let mut b = vec![0, 0, 1, PACK];
        b.extend_from_slice(&v.to_be_bytes()[2..]);
        b.extend_from_slice(&[0x01, 0x89, 0xC3, 0xF8]);
        b
    }

    fn file(packs: u64, first: u64) -> Vec<u8> {
        let mut f = Vec::new();
        for n in 0..packs {
            let mut p = pack((first + n * 1800) % (1 << 33));
            p.resize(2048, 0xAB);
            f.extend(p);
        }
        f
    }

    fn land(f: &[u8], base: Base, target: i64) -> (Option<Landing>, u32) {
        let mut b = Bisect::new(base, target, f.len() as u64);
        loop {
            match b.step() {
                Step::Probe { at, len } => {
                    let end = (at + u64::from(len)).min(f.len() as u64);
                    b.served(at, &f[at as usize..end as usize]);
                }
                Step::Land(l) => return (l, b.probes),
            }
        }
    }

    #[test]
    fn lands_on_the_last_pack_at_or_before_the_target() {
        let f = file(3000, 50_000);
        let base = Base { origin: 50_000, slack: 90_000 };
        for target in [0, 1, 1799, 1800, 1_000_000, 2999 * 1800, 4_000_000_000] {
            let (l, probes) = land(&f, base, target);
            let n = (target / 1800).min(2999) as u64;
            assert_eq!(
                l,
                Some(Landing { at: n * 2048, ticks: n as i64 * 1800 }),
                "target {target}"
            );
            assert!(probes < 20, "{probes} probes");
        }
        assert_eq!(land(&f, base, -1).0, None, "ahead of the first pack");
    }

    #[test]
    fn the_clock_reads_on_across_the_wrap() {
        let first = (1 << 33) - 900_000;
        let f = file(2000, first);
        let base = Base { origin: first as i64, slack: 90_000 };
        assert_eq!(base.ticks(first), 0);
        assert_eq!(base.ticks(100), 900_100);
        let (l, _) = land(&f, base, 1_500_000);
        assert_eq!(l.map(|l| l.at), Some(833 * 2048));
    }
}
