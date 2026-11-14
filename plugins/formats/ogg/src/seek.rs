//! Where a seek lands in an Ogg file read by range, by bisecting the pages of one logical
//! bitstream.

use crate::duration::GranuleClock;
use crate::page::{CAPTURE_PATTERN, GRANULE_NONE, MAX_PAGE_SIZE, PageError, PageHeader};

/// The bytes of a probe, at least a page of the largest size, so a probe mostly holds a whole one
/// and is seldom read again. On a disk the seek costs more than the bytes.
pub const PROBE_LEN: u32 = 64 << 10;
/// Probes a seek takes at most before landing on the best page found.
pub const PROBES_MAX: u32 = 48;

/// A page a seek lands on, its first byte, granule and that granule's stream time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landing {
    pub at: u64,
    pub granule: u64,
    pub ns: u64,
}

impl Landing {
    /// The start of the file, where a landing on a header page goes instead. Read from its
    /// header page on, another stream's header packets behind it would go out as media, from
    /// byte 0 every bos page re-arms its stream's header count.
    pub const HEAD: Landing = Landing { at: 0, granule: 0, ns: 0 };
}

/// What a bisection wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Probe { at: u64, len: u32 },
    Land(Option<Landing>),
}

/// What a bisection looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The last page ending by the target.
    Target,
    /// The last page ending before the keyframe the frame after `found` decodes from, `found`
    /// landed on where none is read.
    Keyframe { found: Landing },
}

/// A seek's search for its landing page.
#[derive(Clone, Debug)]
pub struct Bisect {
    serial: u32,
    clock: GranuleClock,
    target: u64,
    len: u64,
    lo: u64,
    hi: u64,
    best: Option<Landing>,
    /// The stream time at `lo`, at most.
    lo_ns: u64,
    /// The stream time at `hi`, at least, `None` until a page past the target or the file's end
    /// time is known.
    hi_ns: Option<u64>,
    /// The next probe halves the bracket, the last guess having failed to.
    halve: bool,
    /// The bracket's width before the last probe.
    span: u64,
    probes: u32,
    whole: Option<u64>,
    phase: Phase,
    /// Where the stream's pages start, a chained link's first byte.
    floor: u64,
}

impl Bisect {
    /// The landing for stream time `target` ns in the stream of `serial` in a file `len` bytes
    /// long. A clock of keyframes lands ahead of the keyframe the target's frame decodes from.
    pub fn new(serial: u32, clock: GranuleClock, target: u64, len: u64) -> Bisect {
        Bisect {
            serial,
            clock,
            target,
            len,
            lo: 0,
            hi: len,
            best: None,
            lo_ns: 0,
            hi_ns: None,
            halve: false,
            span: len,
            probes: 0,
            whole: None,
            phase: Phase::Target,
            floor: 0,
        }
    }

    /// Searches bytes `lo` to `hi` alone, the link of a chained file the stream is in, its
    /// stream time 0 at `lo`.
    pub fn within(mut self, lo: u64, hi: u64) -> Bisect {
        let hi = hi.min(self.len).max(lo);
        (self.lo, self.hi, self.span, self.floor) = (lo, hi, hi - lo, lo);
        self
    }

    /// The stream time `ns` the file's last page ends at, its duration, so the first probe
    /// interpolates instead of halving.
    pub fn ending(mut self, ns: Option<u64>) -> Bisect {
        self.hi_ns = ns;
        self
    }

    pub fn step(&self) -> Step {
        match self.probe() {
            Some((at, len)) => Step::Probe { at, len },
            None => {
                let landing = match self.phase {
                    Phase::Target => self.best,
                    Phase::Keyframe { found } => self.best.or(Some(found)),
                };
                // granule 0 names no sample or frame past a header, Theora before 3.2.1 its
                // first frame, which the start of the file reaches as well
                Step::Land(landing.map(|l| if l.granule == 0 { Landing::HEAD } else { l }))
            }
        }
    }

    fn probe(&self) -> Option<(u64, u32)> {
        if self.lo >= self.hi || self.probes >= PROBES_MAX {
            return None;
        }
        let (at, len) = match self.whole {
            Some(at) => (at, MAX_PAGE_SIZE as u64),
            None if self.hi - self.lo <= PROBE_LEN as u64 => {
                (self.lo, self.hi - self.lo + PROBE_LEN as u64)
            }
            None => (self.aim(), PROBE_LEN as u64),
        };
        match len.min(self.len.saturating_sub(at)) {
            0 => None,
            n => Some((at, n as u32)),
        }
    }

    /// Where a probe of what is left starts, centred on where the target falls on a line of
    /// stream time against bytes through the bounds, or through the file's start and `lo` with
    /// no time at `hi` yet. A guess that left more than half the bracket is followed by a
    /// halving, so a file whose bitrate swings still takes O(log n) probes.
    fn aim(&self) -> u64 {
        let (lo, hi) = (self.lo, self.hi);
        let (b0, t0, b1, t1) = match self.hi_ns {
            Some(ns) => (lo, self.lo_ns, hi, ns),
            None => (self.floor, 0, lo, self.lo_ns),
        };
        if self.halve || t1 <= t0 || b1 <= b0 {
            return lo + (hi - lo) / 2;
        }
        let along =
            u128::from(self.target.saturating_sub(t0)) * u128::from(b1 - b0) / u128::from(t1 - t0);
        let guess = u64::try_from(u128::from(b0) + along).unwrap_or(u64::MAX);
        // a probe up against `hi` would narrow it by its few bytes
        let last = hi.saturating_sub(PROBE_LEN as u64);
        guess.saturating_sub(PROBE_LEN as u64 / 2).min(last).max(lo)
    }

    /// The bytes read at `at` for the last probe.
    pub fn served(&mut self, at: u64, data: &[u8]) {
        if self.whole.is_none() {
            self.span = self.hi.saturating_sub(self.lo);
        }
        self.read(at, data);
        // a page read again finishes the probe it was cut from
        if self.whole.is_none() {
            self.halve = !self.halve && self.hi.saturating_sub(self.lo) > self.span / 2;
        }
        self.rewind();
    }

    /// Once the target's page is found, searches on within the probes left for the page before
    /// the keyframe the frame after it decodes from. A decoder fed from the target's page would
    /// have no reference picture until the next keyframe.
    fn rewind(&mut self) {
        let (Phase::Target, None, Some(found)) = (self.phase, self.probe(), self.best) else {
            return;
        };
        let Some(key) = self.clock.keyframe_ns(found.granule) else {
            return;
        };
        self.phase = Phase::Keyframe { found };
        (self.target, self.lo, self.hi, self.best, self.whole) =
            (key, self.floor, found.at, None, None);
        (self.lo_ns, self.hi_ns, self.halve) = (0, Some(found.ns), false);
    }

    fn read(&mut self, at: u64, data: &[u8]) {
        self.probes += 1;
        let reread = self.whole.take();
        let sweeps = at <= self.lo;
        let mut found = false;
        let mut covered = at + data.len() as u64 >= self.hi;
        let mut from = 0;
        while let Some(c) = capture(data, from) {
            let start = at + c as u64;
            if start >= self.hi {
                covered = true;
                break;
            }
            let page = match PageHeader::parse(&data[c..]) {
                Ok(page) => page,
                // a page by the target is in, the cut one is left to a later probe. Read again
                // now, the read is cut in turn and a target far past creeps on a read a probe.
                Err(PageError::NeedMoreHeader | PageError::NeedMoreBody) if found => return,
                Err(PageError::NeedMoreHeader | PageError::NeedMoreBody) => {
                    match reread == Some(start) {
                        true => self.hi = self.hi.min(start),
                        false => self.whole = Some(start),
                    }
                    return;
                }
                Err(_) => {
                    from = c + 1;
                    continue;
                }
            };
            let end = start + page.len() as u64;
            from = c + page.len();
            if sweeps {
                self.lo = self.lo.max(end);
            }
            let granule = page.granule_position();
            if page.serial() != self.serial || granule == GRANULE_NONE {
                continue;
            }
            let Some(ns) = self.clock.ns(granule) else {
                continue;
            };
            if ns > self.target {
                self.hi = self.hi.min(if found { start } else { at });
                self.hi_ns = Some(self.hi_ns.map_or(ns, |h| h.min(ns)));
                return;
            }
            found = true;
            self.best = Some(Landing { at: start, granule, ns });
            (self.lo, self.lo_ns) = (self.lo.max(end), ns);
        }
        if !found {
            match covered || reread == Some(at) || data.len() >= MAX_PAGE_SIZE {
                true => self.hi = self.hi.min(at),
                false => self.whole = Some(at),
            }
        }
    }
}

fn capture(data: &[u8], from: usize) -> Option<usize> {
    let tail = data.get(from..)?;
    tail.windows(CAPTURE_PATTERN.len()).position(|w| w == CAPTURE_PATTERN).map(|i| from + i)
}

#[cfg(test)]
mod tests;
