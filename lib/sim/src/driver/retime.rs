//! What a fake retimer does with each buffer, as a small machine that the element and the oracles
//! both run, so where a rate starts follows from the seed.

use super::map::Rate;
use crate::prng::Prng;
use crate::time::{NONE, Ns};

pub const MAX_LAG: u8 = 5;

pub const MAX_OUT: u32 = 4;

pub const LATENCIES: [Ns; 3] = [0, 3_000_000, 20_000_007];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetimeDecl {
    pub on: bool,
    pub lag: u8,
    pub latency: Ns,
    pub seed: u64,
}

impl RetimeDecl {
    pub const OFF: Self = Self { on: false, lag: 0, latency: 0, seed: 0 };
}

pub fn plays(rate: Rate) -> bool {
    let (num, den) = (rate.num as u32, rate.den as u32);
    num <= MAX_OUT * den && den <= MAX_OUT * num
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Took {
    pub rate: Rate,
    pub out: u32,
    pub says: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retiming {
    decl: RetimeDecl,
    rate: Rate,
    asked: Rate,
    left: Option<u8>,
    asks: u64,
    owed: u32,
    restate: bool,
}

impl Retiming {
    pub const fn new(decl: RetimeDecl) -> Retiming {
        let one = Rate::ONE;
        Retiming { decl, rate: one, asked: one, left: None, asks: 0, owed: 0, restate: true }
    }

    fn lag(&self, n: u64) -> u8 {
        let mut rng = Prng::new(self.decl.seed).substream_named("retime.lag").substream(n);
        rng.below(self.decl.lag.min(MAX_LAG) as u64 + 1) as u8
    }

    /// Several asked before the switch are the last one, and the running count stands.
    pub fn ask(&mut self, rate: Rate) {
        let n = self.asks;
        self.asks += 1;
        if !plays(rate) {
            // The timeline restarts at that rate, so the next buffer tells it the rate that goes
            // on.
            self.restate = true;
            return;
        }
        self.asked = rate;
        self.left = match self.left {
            _ if self.asked == self.rate => None,
            None => Some(self.lag(n)),
            counting => counting,
        };
    }

    pub fn flush(&mut self) {
        (self.rate, self.left, self.owed, self.restate) = (self.asked, None, 0, true);
    }

    pub fn take(&mut self) -> Took {
        let mut says = std::mem::take(&mut self.restate);
        self.left = match self.left {
            Some(0) => {
                // A part of one buffer, counted in the old terms.
                let owed = self.owed as u64 * self.asked.num as u64 / self.rate.num.max(1) as u64;
                (self.rate, self.owed, says) = (self.asked, owed as u32, true);
                None
            }
            left => left.map(|n| n - 1),
        };
        self.owed += self.rate.den as u32;
        let out = self.owed / self.rate.num.max(1) as u32;
        self.owed -= out * self.rate.num as u32;
        Took { rate: self.rate, out, says }
    }
}

pub fn stamp(pts: Ns, duration: Ns, k: u32, n: u32) -> (Ns, Ns) {
    if pts == NONE || duration == NONE || n == 0 {
        return (pts, duration);
    }
    let part = |k: u32| (duration as u128 * k.min(n) as u128 / n as u128) as Ns;
    (pts.saturating_add(part(k)), part(k + 1) - part(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECL: RetimeDecl = RetimeDecl { on: true, lag: MAX_LAG, latency: 0, seed: 11 };

    fn rate(num: u16, den: u16) -> Rate {
        Rate { num, den }
    }

    fn run(r: &mut Retiming, n: u32) -> (u32, u32) {
        let took = (0..n).map(|_| r.take());
        took.fold((0, 0), |(out, said), t| (out + t.out, said + t.says as u32))
    }

    #[test]
    fn the_first_buffer_says_the_rate_and_at_one_each_goes_out_once() {
        let mut r = Retiming::new(DECL);
        assert_eq!(r.take(), Took { rate: Rate::ONE, out: 1, says: true });
        assert_eq!(run(&mut r, 50), (50, 0));
    }

    #[test]
    fn it_switches_a_seeded_number_of_buffers_after_it_is_asked_and_says_so_once() {
        let mut lags = [0u32; MAX_LAG as usize + 1];
        for seed in 0..200 {
            let mut r = Retiming::new(RetimeDecl { seed, ..DECL });
            r.take();
            r.ask(rate(2, 1));
            let mut again = r;
            let old = (0..=MAX_LAG).take_while(|_| r.take().rate == Rate::ONE).count();
            assert!(old <= MAX_LAG as usize, "seed {seed}");
            lags[old] += 1;
            // The same seed switches at the same buffer, and only that one speaks.
            let spoke: Vec<bool> = (0..=old).map(|_| again.take().says).collect();
            assert_eq!(spoke.iter().filter(|s| **s).count(), 1);
            assert_eq!(spoke.last(), Some(&true));
            assert_eq!(run(&mut again, 40), (20, 0), "half as many out at twice the rate");
        }
        assert!(lags.iter().all(|n| *n > 10), "every lag is drawn, {lags:?}");
    }

    #[test]
    fn buffers_out_follow_the_rate_with_the_remainder_carried() {
        let rates =
            [(1, 1, 120), (2, 1, 60), (1, 2, 240), (3, 2, 80), (4, 1, 30), (1, 4, 480), (5, 4, 96)];
        for (num, den, out) in rates {
            let mut r = Retiming::new(RetimeDecl { lag: 0, ..DECL });
            r.ask(rate(num, den));
            assert_eq!(run(&mut r, 120).0, out, "{num}/{den}");
        }
        let mut r = Retiming::new(RetimeDecl { lag: 0, ..DECL });
        r.ask(rate(1, 4));
        assert!((0..9).all(|_| r.take().out == MAX_OUT));
        let mut r = Retiming::new(RetimeDecl { lag: 0, ..DECL });
        r.ask(rate(65_535, 65_534));
        assert_eq!(run(&mut r, 1_000).0, 999);
        r.ask(rate(1, 4));
        assert!((0..9).all(|_| r.take().out == MAX_OUT));
        r.ask(rate(4, 1));
        assert_eq!(run(&mut r, 400).0, 100);
    }

    #[test]
    fn several_asked_for_before_the_switch_are_the_last_one() {
        let mut r = Retiming::new(RetimeDecl { lag: 3, seed: 1, ..DECL });
        r.take();
        r.ask(rate(2, 1));
        let mut alone = r;
        r.ask(rate(3, 2));
        let (mut a, mut b) = (alone.take(), r.take());
        for _ in 0..=3 {
            assert_eq!(a.says, b.says, "the count that was running stands");
            (a, b) = (alone.take(), r.take());
        }
        assert_eq!((a.rate, b.rate), (rate(2, 1), rate(3, 2)));
        let mut back = Retiming::new(DECL);
        back.take();
        back.ask(rate(2, 1));
        back.ask(Rate::ONE);
        assert_eq!(run(&mut back, 20), (20, 0));
    }

    #[test]
    fn a_flush_plays_the_rate_asked_from_its_first_buffer_and_a_refusal_restates() {
        let mut r = Retiming::new(DECL);
        r.take();
        r.ask(rate(1, 2));
        r.flush();
        assert_eq!(r.take(), Took { rate: rate(1, 2), out: 2, says: true });
        assert_eq!(run(&mut r, 10), (20, 0));
        assert!(
            !plays(rate(64, 1)) && !plays(rate(1, 64)) && plays(rate(4, 1)) && plays(rate(1, 4))
        );
        r.ask(rate(64, 1));
        assert_eq!(r.take(), Took { rate: rate(1, 2), out: 2, says: true });
        assert_eq!(run(&mut r, 10), (20, 0));
    }

    #[test]
    fn the_outputs_of_a_buffer_share_its_span() {
        assert_eq!(stamp(100, 40, 0, 1), (100, 40));
        let parts: Vec<(Ns, Ns)> = (0..3).map(|k| stamp(100, 40, k, 3)).collect();
        assert_eq!(parts, [(100, 13), (113, 13), (126, 14)]);
        assert_eq!(stamp(NONE, 40, 1, 2), (NONE, 40));
        assert_eq!(stamp(7, NONE, 1, 2), (7, NONE));
    }
}
