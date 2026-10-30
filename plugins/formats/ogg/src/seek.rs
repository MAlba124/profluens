//! Where a seek lands in an Ogg file read by range, by bisecting the pages of one logical
//! bitstream.

use crate::duration::GranuleClock;
use crate::page::{CAPTURE_PATTERN, GRANULE_NONE, MAX_PAGE_SIZE, PageError, PageHeader};

/// The bytes of a probe in the middle of what is left, several pages of audio.
pub const PROBE_LEN: u32 = 16 << 10;
/// Probes a seek takes at most before landing on the best page found.
pub const PROBES_MAX: u32 = 48;

/// A page a seek lands on, its first byte, granule and that granule's stream time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landing {
    pub at: u64,
    pub granule: u64,
    pub ns: u64,
}

/// What a bisection wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Probe { at: u64, len: u32 },
    Land(Option<Landing>),
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
    probes: u32,
    whole: Option<u64>,
}

impl Bisect {
    /// The landing for stream time `target` ns in the stream of `serial` in a file `len` bytes
    /// long.
    pub fn new(serial: u32, clock: GranuleClock, target: u64, len: u64) -> Bisect {
        Bisect { serial, clock, target, len, lo: 0, hi: len, best: None, probes: 0, whole: None }
    }

    pub fn step(&self) -> Step {
        if self.lo >= self.hi || self.probes >= PROBES_MAX {
            return Step::Land(self.best);
        }
        let (at, len) = match self.whole {
            Some(at) => (at, MAX_PAGE_SIZE as u64),
            None if self.hi - self.lo <= PROBE_LEN as u64 => {
                (self.lo, self.hi - self.lo + PROBE_LEN as u64)
            }
            None => (self.lo + (self.hi - self.lo) / 2, PROBE_LEN as u64),
        };
        match len.min(self.len.saturating_sub(at)) {
            0 => Step::Land(self.best),
            n => Step::Probe { at, len: n as u32 },
        }
    }

    /// The bytes read at `at` for the last probe.
    pub fn served(&mut self, at: u64, data: &[u8]) {
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
                return;
            }
            found = true;
            self.best = Some(Landing { at: start, granule, ns });
            self.lo = self.lo.max(end);
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
