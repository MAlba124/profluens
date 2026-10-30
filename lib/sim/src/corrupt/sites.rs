//! Site-directed corruption. Draws concentrate on the [`Site`]s a gate declares.

use super::op::{EXTREMES, Op, Site, apply, corrupt};
use crate::prng::Prng;

/// Half the draws land on or beside a site, where parsers keep their assumptions.
fn pick_offset(prng: &mut Prng, len: usize, sites: &[Site]) -> usize {
    if !sites.is_empty() && prng.chance(50) {
        let site = sites[prng.below(sites.len() as u64) as usize];
        let nudge = prng.below(5) as usize;
        return (site.at + nudge).saturating_sub(2).min(len);
    }
    prng.below(len as u64 + 1) as usize
}

fn pick_len(prng: &mut Prng, long: usize) -> usize {
    match prng.below(10) {
        0..=5 => prng.range(1, 9) as usize,
        6..=8 => prng.range(1, 65) as usize,
        _ => prng.range(1, long.max(2) as u64 + 1) as usize,
    }
}

fn pick_extreme(prng: &mut Prng, at: usize, sites: &[Site]) -> Op {
    let fields = sites.iter().filter(|s| s.width > 0).count() as u64;
    let value = EXTREMES[prng.below(EXTREMES.len() as u64) as usize];
    if fields > 0 && prng.chance(75) {
        let nth = prng.below(fields) as usize;
        let site = sites.iter().filter(|s| s.width > 0).nth(nth).copied();
        let site = site.unwrap_or(Site::boundary(at));
        return Op::Extreme { at: site.at, width: site.width, big_endian: site.big_endian, value };
    }
    Op::Extreme { at, width: 1 << prng.below(4), big_endian: prng.chance(50), value }
}

fn pick_op(prng: &mut Prng, len: usize, sites: &[Site]) -> Op {
    let at = pick_offset(prng, len, sites);
    match prng.below(11) {
        0 | 1 => Op::BitFlip { at, bit: prng.below(8) as u8 },
        2 => Op::Set { at, value: [0x00, 0x01, 0x7F, 0x80, 0xFF][prng.below(5) as usize] },
        3 => Op::Scramble { at, len: pick_len(prng, 512), seed: prng.next_u64() },
        4 => Op::Fill {
            at,
            len: pick_len(prng, 512),
            value: if prng.chance(50) { 0x00 } else { 0xFF },
        },
        5 => Op::Truncate { at },
        6 => Op::Drop { at, len: pick_len(prng, len) },
        7 => Op::Duplicate { at, len: pick_len(prng, len.min(4096)), times: 1 },
        8 => Op::Insert { at, len: pick_len(prng, 2048), seed: prng.next_u64() },
        _ => pick_extreme(prng, at, sites),
    }
}

pub fn mutate(prng: &mut Prng, base: &[u8], sites: &[Site]) -> (Vec<u8>, Vec<Op>) {
    let count = match prng.below(8) {
        0..=3 => 1,
        4 | 5 => 2,
        6 => 3,
        _ => prng.range(4, 7),
    };
    let mut data = base.to_vec();
    let mut ops = Vec::new();
    for _ in 0..count {
        let op = pick_op(prng, data.len(), sites);
        apply(&mut data, &op);
        ops.push(op);
    }
    (data, ops)
}

pub fn noise(prng: &mut Prng, max_len: usize) -> (Vec<u8>, Vec<Op>) {
    let len = prng.below(max_len as u64 + 1) as usize;
    let ops = vec![Op::Insert { at: 0, len, seed: prng.next_u64() }];
    (corrupt(&[], &ops), ops)
}

pub fn prefix_then_garbage(
    prng: &mut Prng,
    base: &[u8],
    sites: &[Site],
    max_garbage: usize,
) -> (Vec<u8>, Vec<Op>) {
    let keep = pick_offset(prng, base.len(), sites);
    let len = prng.below(max_garbage as u64 + 1) as usize;
    let ops = vec![Op::Truncate { at: keep }, Op::Insert { at: keep, len, seed: prng.next_u64() }];
    (corrupt(base, &ops), ops)
}

pub fn sweep(sites: &[Site]) -> Vec<Op> {
    let mut ops = Vec::new();
    for site in sites {
        for at in [site.at.saturating_sub(1), site.at, site.at + 1] {
            ops.push(Op::Truncate { at });
        }
        if site.width == 0 {
            continue;
        }
        ops.push(Op::Truncate { at: site.at + site.width as usize });
        let Site { at, width, big_endian } = *site;
        let extremes = super::op::extremes_for(width).into_iter();
        ops.extend(extremes.map(|value| Op::Extreme { at, width, big_endian, value }));
    }
    ops.dedup();
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITES: [Site; 3] = [Site::boundary(0), Site::be(4, 4), Site::le(12, 2)];

    #[test]
    fn the_same_seed_draws_the_same_bytes_and_ops() {
        let base: Vec<u8> = (0..200).collect();
        for seed in 0..500 {
            let left = mutate(&mut Prng::new(seed), &base, &SITES);
            let right = mutate(&mut Prng::new(seed), &base, &SITES);
            assert_eq!(left, right);
            // The printed ops alone rebuild the input.
            assert_eq!(corrupt(&base, &left.1), left.0);
        }
    }

    #[test]
    fn other_seeds_draw_other_inputs() {
        let base = [0x55u8; 64];
        let first = mutate(&mut Prng::new(1), &base, &SITES).0;
        let differ = (2..50).filter(|&s| mutate(&mut Prng::new(s), &base, &SITES).0 != first);
        assert!(differ.count() > 40);
    }

    #[test]
    fn every_generator_survives_empty_and_one_byte_bases() {
        for base in [&[][..], &[7]] {
            for sites in [&[][..], &SITES] {
                for seed in 0..2_000 {
                    let mut prng = Prng::new(seed);
                    let (data, ops) = mutate(&mut prng, base, sites);
                    assert_eq!(corrupt(base, &ops), data);
                    let (data, ops) = prefix_then_garbage(&mut prng, base, sites, 16);
                    assert!(data.len() <= base.len() + 16);
                    assert_eq!(corrupt(base, &ops), data);
                    let (data, ops) = noise(&mut prng, 16);
                    assert!(data.len() <= 16);
                    assert_eq!(corrupt(&[], &ops), data);
                }
            }
        }
    }

    #[test]
    fn the_sweep_cuts_at_every_site_and_forces_every_field() {
        let ops = sweep(&SITES);
        for at in [0, 1, 3, 4, 5, 8, 11, 12, 13, 14] {
            assert!(ops.contains(&Op::Truncate { at }), "no cut at {at}");
        }
        let extremes = ops.iter().filter(|op| matches!(op, Op::Extreme { .. })).count();
        let per_width = |w| super::super::op::extremes_for(w).len();
        assert_eq!(extremes, per_width(4) + per_width(2), "the values that fit each field");
        let signed_max = Op::Extreme { at: 12, width: 2, big_endian: false, value: 0x8000 };
        let below_max = Op::Extreme { at: 4, width: 4, big_endian: true, value: 0xFFFF_FFFE };
        assert!(ops.contains(&signed_max) && ops.contains(&below_max), "the width's own bounds");
        assert!(!ops.iter().any(|op| matches!(op, Op::Extreme { at: 0, .. })));
        assert!(sweep(&[]).is_empty());
    }
}
