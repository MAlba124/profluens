//! Corpus-directed corruption. About one case in twelve is random bytes with no base.

use super::chunk::Chunking;
use super::op::{EXTREMES, Op, apply};
use crate::prng::Prng;

/// `fields` are offsets of length, count and size fields and boundaries. Edits cluster there.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub fields: &'a [usize],
}

#[derive(Clone, Copy, Debug)]
pub struct Corpus<'a> {
    pub name: &'a str,
    pub inputs: &'a [Input<'a>],
    pub tokens: &'a [&'a [u8]],
    /// Random cases. Pass [`iterations`](super::iterations) to let `PF_FUZZ_ITERS` scale it.
    pub iters: u64,
    /// Truncation points per input beyond declared fields. Twice as many extremes are sampled.
    pub sweep: usize,
    /// Path fragments of crates whose own panics are counted, not failed.
    pub tolerate: &'a [&'a str],
}

pub(super) struct Draft {
    pub input: Option<usize>,
    pub ops: Vec<Op>,
    pub data: Vec<u8>,
    pub chunking: Chunking,
}

const WIDTHS: [u8; 5] = [1, 2, 3, 4, 8];

fn size_cap(original: usize) -> usize {
    original * 2 + 256 * 1024
}

fn random_bytes(p: &mut Prng, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    for chunk in out.chunks_mut(8) {
        let word = p.next_u64().to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    out
}

fn draw_pos(p: &mut Prng, len: usize, fields: &[usize]) -> usize {
    if len == 0 {
        return 0;
    }
    let roll = p.below(100);
    let at = if !fields.is_empty() && roll < 45 {
        let field = fields[p.below(fields.len() as u64) as usize];
        (field + p.below(9) as usize).saturating_sub(4)
    } else if roll < 65 {
        p.below(len.min(64) as u64) as usize
    } else {
        p.below(len as u64) as usize
    };
    at.min(len - 1)
}

fn draw_len(p: &mut Prng, max: usize) -> usize {
    let cap = match p.below(4) {
        0 => 1,
        1 => 8,
        2 => 64,
        _ => 4096,
    };
    1 + p.below(cap.min(max.max(1)) as u64) as usize
}

fn room(cap: usize, len: usize, piece: usize) -> usize {
    cap.saturating_sub(len).checked_div(piece).unwrap_or(0)
}

fn draw_token(p: &mut Prng, at: usize, len: usize, tokens: &[&[u8]], cap: usize) -> Op {
    let token = tokens[p.below(tokens.len() as u64) as usize];
    let times = match p.below(10) {
        0..=6 => 1,
        7 | 8 => 2 + p.below(63) as usize,
        _ => 1000 + p.below(20_000) as usize,
    };
    Op::Token { at, times: times.min(room(cap, len, token.len())), bytes: token.to_vec() }
}

fn draw_op(p: &mut Prng, len: usize, fields: &[usize], tokens: &[&[u8]], cap: usize) -> Op {
    let at = draw_pos(p, len, fields);
    let rest = len - at.min(len);
    let kinds = if tokens.is_empty() { 10 } else { 12 };
    match p.below(kinds) {
        0 | 1 => Op::BitFlip { at, bit: p.below(8) as u8 },
        2 => {
            let remove = draw_len(p, rest).min(rest);
            Op::Splice { at, remove, bytes: random_bytes(p, remove) }
        }
        3 => Op::Fill { at, len: draw_len(p, rest), value: if p.chance(50) { 0x00 } else { 0xFF } },
        4 => Op::Truncate { at: if p.chance(50) { at } else { p.below(len as u64 + 1) as usize } },
        5 => Op::Drop { at, len: draw_len(p, rest) },
        6 => {
            let n = draw_len(p, rest);
            let most = if p.chance(80) { 3 } else { 64 };
            let times = 1 + p.below(most) as usize;
            Op::Duplicate { at, len: n, times: times.min(room(cap, len, n.min(rest))) }
        }
        7 => {
            let n = draw_len(p, 4096).min(cap.saturating_sub(len));
            Op::Splice { at, remove: 0, bytes: random_bytes(p, n) }
        }
        8 | 9 => {
            let width = WIDTHS[p.below(WIDTHS.len() as u64) as usize];
            let value = EXTREMES[p.below(EXTREMES.len() as u64) as usize];
            let big_endian = !p.chance(50);
            // Narrowed to what is left, so the field keeps its low bytes at the end.
            Op::Extreme { at, width: (width as usize).min(rest) as u8, big_endian, value }
        }
        _ => draw_token(p, at, len, tokens, cap),
    }
}

fn draw_noise(p: &mut Prng, inputs: &[Input]) -> Vec<Op> {
    let max = match p.below(3) {
        0 => 16,
        1 => 256,
        _ => 8192,
    };
    let len = p.below(max + 1) as usize;
    let mut ops = vec![Op::Splice { at: 0, remove: 0, bytes: random_bytes(p, len) }];
    if !inputs.is_empty() && p.chance(50) {
        let donor = inputs[p.below(inputs.len() as u64) as usize].bytes;
        let head = p.below(donor.len().min(256) as u64 + 1) as usize;
        ops.push(Op::Splice { at: 0, remove: 0, bytes: donor[..head].to_vec() });
    }
    ops
}

pub(super) fn draw(p: &mut Prng, inputs: &[Input], tokens: &[&[u8]]) -> Draft {
    let noise = inputs.is_empty() || p.chance(8);
    let (input, mut data, mut ops, fields): (_, Vec<u8>, _, &[usize]) = if noise {
        let ops = draw_noise(p, inputs);
        let mut data = Vec::new();
        ops.iter().for_each(|op| apply(&mut data, op));
        (None, data, ops, &[])
    } else {
        let i = p.below(inputs.len() as u64) as usize;
        (Some(i), inputs[i].bytes.to_vec(), Vec::new(), inputs[i].fields)
    };
    let cap = size_cap(data.len());
    let count = if noise { p.below(2) } else { 1 + p.below(4) };
    for _ in 0..count {
        let op = draw_op(p, data.len(), fields, tokens, cap);
        apply(&mut data, &op);
        ops.push(op);
    }
    Draft { input, ops, data, chunking: draw_chunking(p) }
}

fn draw_chunking(p: &mut Prng) -> Chunking {
    match p.below(8) {
        0 | 1 => Chunking::Whole,
        2 => Chunking::Fixed(1 + p.below(8) as usize),
        3 => Chunking::Fixed(1 + p.below(4096) as usize),
        4 | 5 => Chunking::Random { max: 1 + p.below(16) as usize, seed: p.next_u64() },
        6 => Chunking::Random { max: 1 + p.below(512) as usize, seed: p.next_u64() },
        _ => Chunking::Random { max: 1 + p.below(8192) as usize, seed: p.next_u64() },
    }
}

pub(super) type Planned = (usize, Op, Chunking);

fn plan_cuts(plan: &mut Vec<Planned>, index: usize, input: &Input, sweep: usize) {
    let len = input.bytes.len();
    let near = |&field: &usize| [field.saturating_sub(1), field, field + 1];
    let mut cuts: Vec<usize> = input.fields.iter().flat_map(near).map(|at| at.min(len)).collect();
    cuts.extend((0..len).step_by((len / sweep.max(1)).max(1)));
    cuts.push(len);
    cuts.sort_unstable();
    cuts.dedup();
    for (n, &cut) in cuts.iter().enumerate() {
        let chunking = match n % 3 {
            0 => Chunking::Whole,
            1 => Chunking::Fixed(1 + n % 7),
            _ => Chunking::Random { max: 64, seed: n as u64 },
        };
        plan.push((index, Op::Truncate { at: cut }, chunking));
    }
}

fn extremes_at(out: &mut Vec<Op>, at: usize, width: u8, len: usize) {
    let fits = |value: &u64| width == 8 || value >> (8 * width as u32) == 0;
    let left = (len - at.min(len)) as u64;
    let narrowed = left.min(width as u64) as u8;
    // The width's own bounds, and what is left of the input and one either side of it, so a
    // length field is also planned at exactly, just short of and just past the end.
    let mut values = super::op::extremes_for(width);
    values.extend([left.saturating_sub(1), left, left + 1].into_iter().filter(fits));
    values.sort_unstable();
    values.dedup();
    for value in values {
        for big_endian in [true, false] {
            out.push(Op::Extreme { at, width: narrowed, big_endian, value });
        }
    }
}

fn plan_extremes(plan: &mut Vec<Planned>, index: usize, input: &Input, sweep: usize, soak: bool) {
    let mut all = Vec::new();
    for &field in input.fields {
        for width in WIDTHS {
            extremes_at(&mut all, field, width, input.bytes.len());
        }
    }
    let wanted = if soak { all.len() } else { sweep * 2 };
    // An odd stride walks every width and value over the run of fields.
    let stride = if soak { 1 } else { (all.len() / wanted.max(1)).max(1) | 1 };
    for (n, op) in all.into_iter().enumerate().step_by(stride) {
        // Values come in [big, little] endian pairs, so the pair index picks the chunking, else
        // every big-endian extreme would be fed whole.
        let chunking = if (n / 2) % 2 == 0 { Chunking::Whole } else { Chunking::Fixed(5) };
        plan.push((index, op, chunking));
    }
}

pub(super) fn plan(inputs: &[Input], sweep: usize, soak: bool) -> Vec<Planned> {
    let mut plan = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        plan_cuts(&mut plan, index, input, sweep);
        plan_extremes(&mut plan, index, input, sweep, soak);
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::super::op::corrupt;
    use super::*;

    const TOKENS: [&[u8]; 3] = [b"fLaC", b"OggS", b"\xFF\xF8"];

    fn inputs<'a>(valid: &'a [u8], tiny: &'a [u8]) -> [Input<'a>; 3] {
        [
            Input { name: "valid", bytes: valid, fields: &[0, 4, 8, 190, 199] },
            Input { name: "tiny", bytes: tiny, fields: &[0, 1, 9] },
            Input { name: "empty", bytes: &[], fields: &[0, 3] },
        ]
    }

    #[test]
    fn the_same_seed_draws_the_same_case() {
        let valid: Vec<u8> = (0..200).collect();
        let inputs = inputs(&valid, &[7]);
        for seed in 0..2_000 {
            let left = draw(&mut Prng::new(seed), &inputs, &TOKENS);
            let right = draw(&mut Prng::new(seed), &inputs, &TOKENS);
            assert_eq!((&left.data, &left.ops), (&right.data, &right.ops));
            assert_eq!(left.chunking, right.chunking);
        }
    }

    #[test]
    fn the_ops_alone_rebuild_every_case_within_the_size_cap() {
        let valid: Vec<u8> = (0..200).collect();
        let inputs = inputs(&valid, &[7]);
        let mut noise = 0;
        for seed in 0..4_000 {
            let draft = draw(&mut Prng::new(seed), &inputs, &TOKENS);
            let base = draft.input.map_or(&[][..], |i| inputs[i].bytes);
            assert_eq!(corrupt(base, &draft.ops), draft.data, "seed {seed}");
            let start = if draft.input.is_some() { base.len() } else { 8192 + 256 };
            assert!(draft.data.len() <= size_cap(start), "seed {seed}");
            noise += u32::from(draft.input.is_none());
        }
        assert!((200..450).contains(&noise), "{noise} noise cases");
    }

    #[test]
    fn a_corpus_of_nothing_is_all_noise_and_no_plan() {
        for seed in 0..500 {
            let draft = draw(&mut Prng::new(seed), &[], &[]);
            assert!(draft.input.is_none() && draft.data.len() <= size_cap(8192));
        }
        assert!(plan(&[], 8, true).is_empty());
    }

    #[test]
    fn the_plan_cuts_at_every_field_and_keeps_the_untouched_input() {
        let valid: Vec<u8> = (0..200).collect();
        let inputs = inputs(&valid, &[7]);
        let plan = plan(&inputs, 4, false);
        let cuts = |index: usize| {
            let of = move |p: &Planned| match p {
                (i, Op::Truncate { at }, _) if *i == index => Some(*at),
                _ => None,
            };
            plan.iter().filter_map(of).collect::<Vec<usize>>()
        };
        for at in [0, 1, 3, 4, 5, 7, 8, 9, 189, 190, 191, 198, 199, 200, 50, 100, 150] {
            assert!(cuts(0).contains(&at), "no cut at {at}");
        }
        assert_eq!(cuts(1), [0, 1]);
        assert_eq!(cuts(2), [0]);
        for (index, op, _) in &plan {
            let base = inputs[*index].bytes;
            assert!(corrupt(base, std::slice::from_ref(op)).len() <= base.len());
        }
    }

    #[test]
    fn a_soak_plans_every_extreme_and_a_default_run_samples_them() {
        let valid: Vec<u8> = (0..200).collect();
        let inputs = &inputs(&valid, &[7])[..1];
        let extremes = |sweep, soak| {
            let plan = plan(inputs, sweep, soak);
            plan.iter().filter(|p| matches!(p.1, Op::Extreme { .. })).count()
        };
        // Per width, 6, 9, 11, 11 and 14 values fit 1, 2, 3, 4 and 8 bytes, plus what is left of
        // the input and one either side, 3 more at four of the fields and 1 (the value 2) at the
        // field one octet before the end, each in two orders.
        let widths = 6 + 9 + 11 + 11 + 14;
        assert_eq!(extremes(4, true), 2 * (4 * (widths + 5 * 3) + widths + 5));
        assert!((8..=16).contains(&extremes(4, false)));
        let planned = plan(inputs, 4, true);
        let has = |width: u8, value: u64| {
            planned.iter().any(|p| {
                matches!(p.1, Op::Extreme { width: w, value: v, .. } if w == width && v == value)
            })
        };
        let bounds = [
            (1, 0xFE),
            (2, 0x8000),
            (2, 0xFFFE),
            (3, 0x7F_FFFF),
            (3, 0x80_0000),
            (3, 0xFF_FFFF),
            (4, 0xFFFF_FFFE),
            (8, 0x8000_0000_0000_0000),
            (8, u64::MAX - 1),
            (1, 199),
            (1, 201),
            (2, 193),
        ];
        for (width, value) in bounds {
            assert!(has(width, value), "{width} bytes at {value:#x}");
        }
    }
}
