//! Corruptions as plain data. A printed [`Op`] list replays without its seed.

use crate::prng::Prng;

pub const EXTREMES: [u64; 12] = [
    0,
    1,
    0x7F,
    0x80,
    0xFF,
    0x7FFF,
    0xFFFF,
    0x7FFF_FFFF,
    0x8000_0000,
    0xFFFF_FFFF,
    0x7FFF_FFFF_FFFF_FFFF,
    u64::MAX,
];

/// The values planned for a field of `width` bytes, the fixed extremes that fit it and the
/// bounds of the width itself, its signed and unsigned maximum and the one below and above.
// Cold path, planning.
#[allow(clippy::disallowed_methods)]
pub fn extremes_for(width: u8) -> Vec<u64> {
    let max = if width >= 8 { u64::MAX } else { (1u64 << (8 * u32::from(width))) - 1 };
    let mut values: Vec<u64> = EXTREMES.into_iter().filter(|v| *v <= max).collect();
    values.extend([max >> 1, (max >> 1) + 1, max - 1, max]);
    values.sort_unstable();
    values.dedup();
    values
}

/// `width` 0 is a bare boundary, anything else a size or count field of that many bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    pub at: usize,
    pub width: u8,
    pub big_endian: bool,
}

impl Site {
    pub const fn boundary(at: usize) -> Site {
        Site { at, width: 0, big_endian: true }
    }

    pub const fn be(at: usize, width: u8) -> Site {
        Site { at, width, big_endian: true }
    }

    pub const fn le(at: usize, width: u8) -> Site {
        Site { at, width, big_endian: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    BitFlip {
        at: usize,
        bit: u8,
    },
    Set {
        at: usize,
        value: u8,
    },
    Scramble {
        at: usize,
        len: usize,
        seed: u64,
    },
    Fill {
        at: usize,
        len: usize,
        value: u8,
    },
    Truncate {
        at: usize,
    },
    Drop {
        at: usize,
        len: usize,
    },
    Duplicate {
        at: usize,
        len: usize,
        times: usize,
    },
    Insert {
        at: usize,
        len: usize,
        seed: u64,
    },
    /// A field cut by the end of the input keeps its leading bytes.
    Extreme {
        at: usize,
        width: u8,
        big_endian: bool,
        value: u64,
    },
    Splice {
        at: usize,
        remove: usize,
        bytes: Vec<u8>,
    },
    Token {
        at: usize,
        times: usize,
        bytes: Vec<u8>,
    },
}

fn clamp(at: usize, n: usize, len: usize) -> (usize, usize) {
    let start = at.min(len);
    (start, start.saturating_add(n).min(len))
}

/// Every offset and length is clamped, so any op is safe on any input.
pub fn apply(data: &mut Vec<u8>, op: &Op) {
    let len = data.len();
    match *op {
        Op::BitFlip { at, bit } => {
            if let Some(byte) = data.get_mut(at) {
                *byte ^= 1 << (bit & 7);
            }
        }
        Op::Set { at, value } => {
            if let Some(byte) = data.get_mut(at) {
                *byte = value;
            }
        }
        Op::Scramble { at, len: n, seed } => {
            let mut prng = Prng::new(seed);
            let (start, end) = clamp(at, n, len);
            for byte in &mut data[start..end] {
                *byte = prng.next_u64() as u8;
            }
        }
        Op::Fill { at, len: n, value } => {
            let (start, end) = clamp(at, n, len);
            data[start..end].fill(value);
        }
        Op::Truncate { at } => data.truncate(at),
        Op::Drop { at, len: n } => {
            let (start, end) = clamp(at, n, len);
            data.drain(start..end);
        }
        Op::Duplicate { at, len: n, times } => {
            let (start, end) = clamp(at, n, len);
            let copy = data[start..end].repeat(times);
            data.splice(end..end, copy);
        }
        Op::Insert { at, len: n, seed } => {
            let mut prng = Prng::new(seed);
            let at = at.min(len);
            let garbage: Vec<u8> = (0..n).map(|_| prng.next_u64() as u8).collect();
            data.splice(at..at, garbage);
        }
        Op::Extreme { at, width, big_endian, value } => {
            let width = (width as usize).min(8);
            let (start, end) = clamp(at, width, len);
            let be = value.to_be_bytes();
            let le = value.to_le_bytes();
            let field = if big_endian { &be[8 - width..] } else { &le[..width] };
            data[start..end].copy_from_slice(&field[..end - start]);
        }
        Op::Splice { at, remove, ref bytes } => {
            let (start, end) = clamp(at, remove, len);
            data.splice(start..end, bytes.iter().copied());
        }
        Op::Token { at, times, ref bytes } => {
            let at = at.min(len);
            data.splice(at..at, bytes.repeat(times));
        }
    }
}

pub fn corrupt(base: &[u8], ops: &[Op]) -> Vec<u8> {
    let mut data = base.to_vec();
    for op in ops {
        apply(&mut data, op);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_op(at: usize) -> Vec<Op> {
        vec![
            Op::BitFlip { at, bit: 9 },
            Op::Set { at, value: 0x7F },
            Op::Scramble { at, len: 5, seed: 1 },
            Op::Fill { at, len: usize::MAX, value: 0xFF },
            Op::Truncate { at },
            Op::Drop { at, len: usize::MAX },
            Op::Duplicate { at, len: 3, times: 2 },
            Op::Insert { at, len: 4, seed: 2 },
            Op::Extreme { at, width: 8, big_endian: true, value: u64::MAX },
            Op::Extreme { at, width: 200, big_endian: false, value: 1 },
            Op::Splice { at, remove: usize::MAX, bytes: vec![1, 2, 3] },
            Op::Token { at, times: 3, bytes: vec![b'O', b'g'] },
        ]
    }

    #[test]
    fn every_op_stays_in_bounds_on_empty_and_tiny_inputs() {
        for base in [&[][..], &[0xAA], &[1, 2, 3, 4, 5, 6, 7, 8, 9]] {
            for at in [0, 1, base.len(), base.len() + 1, usize::MAX] {
                for op in every_op(at) {
                    let mut data = base.to_vec();
                    apply(&mut data, &op);
                    // Growth is bounded by what the op carries, never by its offsets.
                    assert!(data.len() <= base.len() * 3 + 6, "{op:?} on {base:?}");
                }
            }
        }
    }

    #[test]
    fn ops_that_only_overwrite_keep_the_length() {
        let base = [1u8, 2, 3, 4, 5];
        for at in [0, 2, 4, 5, 99] {
            for op in [
                Op::BitFlip { at, bit: 3 },
                Op::Set { at, value: 0 },
                Op::Scramble { at, len: 9, seed: 7 },
                Op::Fill { at, len: 9, value: 0 },
                Op::Extreme { at, width: 4, big_endian: true, value: 0x8000_0000 },
            ] {
                assert_eq!(corrupt(&base, std::slice::from_ref(&op)).len(), base.len(), "{op:?}");
            }
        }
    }

    #[test]
    fn each_op_does_what_it_says() {
        let base = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let run = |op: Op| corrupt(&base, &[op]);
        assert_eq!(run(Op::BitFlip { at: 1, bit: 7 })[1], 0x81);
        assert_eq!(run(Op::Truncate { at: 3 }), [0, 1, 2]);
        assert_eq!(run(Op::Drop { at: 1, len: 6 }), [0, 7]);
        assert_eq!(run(Op::Duplicate { at: 6, len: 2, times: 2 })[6..], [6, 7, 6, 7, 6, 7]);
        assert_eq!(run(Op::Fill { at: 6, len: 9, value: 9 }), [0, 1, 2, 3, 4, 5, 9, 9]);
        assert_eq!(
            run(Op::Extreme { at: 2, width: 2, big_endian: true, value: 0x7FFF }),
            [0, 1, 0x7F, 0xFF, 4, 5, 6, 7]
        );
        assert_eq!(
            run(Op::Extreme { at: 2, width: 2, big_endian: false, value: 0x7FFF }),
            [0, 1, 0xFF, 0x7F, 4, 5, 6, 7]
        );
        assert_eq!(run(Op::Extreme { at: 7, width: 2, big_endian: true, value: 0x7FFF })[7], 0x7F);
        assert_eq!(run(Op::Splice { at: 1, remove: 6, bytes: vec![9] }), [0, 9, 7]);
        assert_eq!(run(Op::Token { at: 8, times: 2, bytes: vec![b'a', b'b'] })[8..], *b"abab");
        assert_eq!(run(Op::Insert { at: 0, len: 3, seed: 5 })[3..], base);
    }

    #[test]
    fn seeded_ops_replay_the_same_bytes() {
        let base = [0u8; 64];
        let ops =
            [Op::Scramble { at: 3, len: 40, seed: 11 }, Op::Insert { at: 9, len: 17, seed: 12 }];
        assert_eq!(corrupt(&base, &ops), corrupt(&base, &ops));
        assert_ne!(corrupt(&base, &ops[..1]), base);
    }
}
