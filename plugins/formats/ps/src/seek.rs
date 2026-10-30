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

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test vectors
mod tests {
    use super::*;

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
