#![allow(dead_code)]

use pf_harness::Harness;
use pf_sim::corrupt::Chunking;
use pf_sim::corrupt::harness::{Fed, collect, drive, drive_pieces, input_buffer};
use profluens_core::buffer::Buffer;
use profluens_core::time::Timestamp;

/// What a case does with the presentation point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    None,
    Seek(Timestamp),
    Skip { before: usize, to: Timestamp },
}

impl Point {
    /// Drawn from the case's bytes, so a case replays as it ran.
    pub fn of(bytes: &[u8], pieces: usize, span: Timestamp) -> Point {
        let ends = bytes.iter().take(8).chain(bytes.iter().rev().take(8));
        let hash = ends.fold(0xcbf2_9ce4_8422_2325 ^ bytes.len() as u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        let seed = (hash ^ (hash >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
        let seed = seed ^ (seed >> 33);
        let to = Timestamp(span.0 / 15 * ((seed >> 8) % 16));
        match seed % 3 {
            0 => Point::None,
            1 => Point::Seek(to),
            _ => Point::Skip { before: (seed >> 16) as usize % pieces.max(1), to },
        }
    }

    pub fn open(self, harness: &mut Harness) -> bool {
        match self {
            Point::Seek(to) => harness.seek(0, to).is_ok(),
            _ => true,
        }
    }
}

fn push_dry(harness: &mut Harness, buffer: Buffer, fed: &mut Fed, budget: u64) -> bool {
    let mut flow = harness.push(buffer);
    loop {
        fed.steps += 1;
        assert!(fed.steps <= budget, "element still producing after {budget} process calls");
        if flow.is_err() {
            return false;
        }
        if collect(harness, fed) == 0 {
            return true;
        }
        flow = harness.crank();
    }
}

/// Feed `pieces` one per buffer with a skip to `to` ahead of buffer `before`, then end of stream.
pub fn drive_skipping<'a>(
    harness: &mut Harness,
    pieces: &mut dyn Iterator<Item = &'a [u8]>,
    (count, total): (usize, usize),
    (before, to): (usize, Timestamp),
    stamp: &dyn Fn(usize, &mut Buffer),
) -> Fed {
    let mut fed = Fed::default();
    let budget = (2 * count + total + 64) as u64;
    for (index, piece) in pieces.enumerate() {
        if index == before && harness.skip_to(to).is_err() {
            fed.failed = true;
        }
        let mut buffer = input_buffer(piece);
        stamp(index, &mut buffer);
        if fed.failed || !push_dry(harness, buffer, &mut fed, budget) {
            fed.failed = true;
            break;
        }
    }
    if !fed.failed {
        fed.failed =
            harness.eos().map(|tail| fed.out_bytes += bytes(&tail, &mut fed.out_buffers)).is_err();
    }
    collect(harness, &mut fed);
    fed
}

pub fn drive_bytes(
    harness: &mut Harness,
    bytes: &[u8],
    chunking: Chunking,
    flush_before: Option<usize>,
    span: Timestamp,
) -> Fed {
    let count = chunking.count(bytes);
    let point = Point::of(bytes, count, span);
    if !point.open(harness) {
        return Fed { failed: true, ..Fed::default() };
    }
    let Point::Skip { before, to } = point else {
        return drive(harness, bytes, chunking, flush_before);
    };
    let mut pieces = chunking.pieces(bytes);
    drive_skipping(harness, &mut pieces, (count, bytes.len()), (before, to), &|_, _| {})
}

/// [`drive_pieces`] with the point the case draws from `bytes`.
pub fn drive_packets(
    harness: &mut Harness,
    bytes: &[u8],
    pieces: &[&[u8]],
    flush_before: Option<usize>,
    span: Timestamp,
    stamp: &dyn Fn(usize, &mut Buffer),
) -> Fed {
    let point = Point::of(bytes, pieces.len(), span);
    if !point.open(harness) {
        return Fed { failed: true, ..Fed::default() };
    }
    let Point::Skip { before, to } = point else {
        return drive_pieces(harness, pieces, flush_before, stamp);
    };
    let total = pieces.iter().map(|piece| piece.len()).sum();
    let mut each = pieces.iter().copied();
    drive_skipping(harness, &mut each, (pieces.len(), total), (before, to), stamp)
}

fn bytes(tail: &[Buffer], buffers: &mut usize) -> usize {
    *buffers += tail.len();
    tail.iter().map(|buffer| buffer.memory.len()).sum()
}
