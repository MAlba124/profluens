//! A run's simulated network as data. The world's faults, deliberate defects and what a seed draws.

use crate::prng::{Prng, mix};
use crate::time::{MS, NONE, Ns, SEC};

pub fn pattern(key: u64, at: u64) -> u8 {
    mix(key ^ at.wrapping_mul(0x9E37_79B9_7F4A_7C15)) as u8
}

/// Each count is a budget spent as the fault fires, so a run under faults still ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Faults {
    pub latency: (Ns, Ns),
    pub split: bool,
    pub short_writes: bool,
    pub refuse: u8,
    pub time_out: u8,
    pub connect_timeout: Ns,
    pub resets: u8,
    pub reset_within: u64,
    pub stalls: u8,
    pub stall_within: u64,
    pub stall_for: Ns,
    pub dns_fail: u8,
    pub spare: u8,
    pub rate: [(Ns, u64); RATE_STEPS],
    /// The peer closes a connection idle this long (RFC 9110 section 9.2.2). [`NONE`] never
    /// closes.
    pub idle_close: Ns,
    pub piece: u64,
}

pub const RATE_STEPS: usize = 4;

pub const RATE_QUANTUM: Ns = 250 * MS;

impl Faults {
    pub const NONE: Faults = Faults {
        latency: (0, 0),
        split: false,
        short_writes: false,
        refuse: 0,
        time_out: 0,
        connect_timeout: 0,
        resets: 0,
        reset_within: 0,
        stalls: 0,
        stall_within: 0,
        stall_for: 0,
        dns_fail: 0,
        spare: 0,
        rate: [(0, 0), (NONE, 0), (NONE, 0), (NONE, 0)],
        idle_close: NONE,
        piece: 0,
    };

    pub const fn at_rate(rate: u64) -> Faults {
        Faults { rate: [(0, rate), (NONE, 0), (NONE, 0), (NONE, 0)], ..Faults::NONE }
    }

    pub fn rate_at(&self, now: Ns) -> u64 {
        let on = self.rate.iter().take_while(|step| step.0 != NONE && step.0 <= now).last();
        on.map_or(0, |step| step.1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Break {
    #[default]
    None,
    Duplicate,
    Skip,
    CancelCompletes,
    Lose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Slip {
    #[default]
    None,
    ResumeShort,
    NoCancel,
    NoWatchdog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetDecl {
    pub on: bool,
    pub faults: Faults,
    pub key: u64,
    pub patience: Ns,
    pub backoff: Ns,
    pub breakage: Break,
    pub slip: Slip,
}

impl NetDecl {
    pub const OFF: NetDecl = NetDecl {
        on: false,
        faults: Faults::NONE,
        key: 0,
        patience: 0,
        backoff: 0,
        breakage: Break::None,
        slip: Slip::None,
    };

    /// Another place for every generation, so bytes of the old place are never right at the new
    /// one.
    pub fn place(&self, seek_gen: u64) -> u64 {
        mix(self.key ^ seek_gen.wrapping_mul(0xD6E8_FEB8_6659_FD93)) >> 24
    }
}

/// One seed in eight, from its own substream, so the others run as before.
pub fn draw_net(root: &Prng) -> NetDecl {
    let mut rng = root.substream_named("net");
    if !rng.chance(12) {
        return NetDecl::OFF;
    }
    let slow = rng.chance(60);
    let faults = Faults {
        latency: if slow { (0, rng.range(1, 40) * MS) } else { (0, 0) },
        split: rng.chance(80),
        short_writes: rng.chance(50),
        refuse: if rng.chance(25) { rng.range(1, 3) as u8 } else { 0 },
        time_out: rng.chance(15) as u8,
        connect_timeout: 3 * SEC,
        resets: if rng.chance(40) { rng.range(1, 4) as u8 } else { 0 },
        reset_within: rng.range(1, 600),
        stalls: rng.chance(30) as u8,
        stall_within: rng.range(1, 600),
        stall_for: if rng.chance(50) { NONE } else { rng.range(1, 900) * MS },
        dns_fail: rng.chance(15) as u8,
        spare: 0,
        rate: Faults::NONE.rate,
        idle_close: NONE,
        piece: 0,
    };
    NetDecl {
        on: true,
        faults,
        key: rng.next_u64(),
        patience: rng.range(200, 2000) * MS,
        backoff: rng.range(1, 300) * MS,
        breakage: Break::None,
        slip: Slip::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_is_a_function_of_its_key_and_its_place() {
        let a: [u8; 64] = core::array::from_fn(|i| pattern(1, i as u64));
        let b: [u8; 64] = core::array::from_fn(|i| pattern(1, i as u64));
        let c: [u8; 64] = core::array::from_fn(|i| pattern(2, i as u64));
        let d: [u8; 64] = core::array::from_fn(|i| pattern(1, 1 + i as u64));
        assert_eq!(a, b);
        assert!(a != c && a != d, "another key or another place is another stream");
    }

    #[test]
    fn one_seed_in_eight_draws_a_network_and_each_seek_its_own_place() {
        let on = (0..4000).filter(|seed| draw_net(&Prng::new(*seed)).on).count();
        assert!((380..620).contains(&on), "{on} of 4000");
        let net = (0..).map(|seed| draw_net(&Prng::new(seed))).find(|n| n.on).expect("one is on");
        assert_eq!(
            net,
            draw_net(&Prng::new((0..).find(|s| draw_net(&Prng::new(*s)).on).unwrap_or(0)))
        );
        let places: std::collections::BTreeSet<u64> =
            (0..200).map(|seek_gen| net.place(seek_gen)).collect();
        assert_eq!(places.len(), 200);
        assert_eq!(draw_net(&Prng::new(1)).breakage, Break::None, "no seed draws a defect");
    }
}
