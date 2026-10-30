//! Chunk boundaries never show in events or counters, for every packet size and broken streams too.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{FIXTURES, Owned, Rng, SIZES, corrupt_framed, piece, reframe, run_with};
use pf_mpegts::{Config, Demux, Stats, framed_len};

fn events_with(data: &[u8], next_size: impl FnMut() -> usize) -> (Vec<Owned>, Stats) {
    let mut demux = Demux::new(Config::default()).unwrap();
    let mut events = Vec::new();
    run_with(&mut demux, data, next_size, &mut events);
    (events, *demux.stats())
}

#[test]
fn one_byte_pieces() {
    for f in FIXTURES {
        for size in SIZES {
            let data = reframe(f.data, size);
            let whole = events_with(&data, || usize::MAX);
            assert!(whole.0.len() > 10, "{} produced events", f.name);
            let bytes = events_with(&data, || 1);
            assert!(whole == bytes, "{} as {size:?} differs when pushed byte by byte", f.name);
        }
    }
}

#[test]
fn fixed_pieces_around_the_packet_size() {
    for f in FIXTURES {
        for size in SIZES {
            let data = reframe(f.data, size);
            let whole = events_with(&data, || usize::MAX);
            let n = framed_len(size);
            for pieces in [
                2,
                3,
                7,
                n - 1,
                n,
                n + 1,
                2 * n - 1,
                2 * n,
                2 * n + 1,
                5 * n,
                5 * n + 1,
                4096,
                65536,
            ] {
                let got = events_with(&data, || pieces);
                assert!(whole == got, "{} as {size:?} differs in pieces of {pieces}", f.name);
            }
        }
    }
}

#[test]
fn random_pieces() {
    for (n, f) in FIXTURES.iter().enumerate() {
        for size in SIZES {
            let data = reframe(f.data, size);
            let whole = events_with(&data, || usize::MAX);
            for seed in 0..40 {
                let mut rng = Rng::new(seed * 131 + n as u64 + framed_len(size) as u64 - 188);
                let pieces = events_with(&data, || piece(&mut rng));
                assert!(whole == pieces, "{} as {size:?} differs with seed {seed}", f.name);
            }
        }
    }
}

#[test]
fn damaged_input_in_any_pieces() {
    for (n, f) in FIXTURES.iter().enumerate() {
        for size in SIZES {
            let clean = reframe(f.data, size);
            for seed in 0..12 {
                let mut rng =
                    Rng::new(0xC0FFEE + seed * 977 + n as u64 + framed_len(size) as u64 - 188);
                let a = rng.below(clean.len() / 2);
                let b = a + 1 + rng.below(3000.min(clean.len() - a - 1));
                let data = corrupt_framed(&mut rng, &clean, a, b, size);
                let whole = events_with(&data, || usize::MAX);
                let bytes = events_with(&data, || 1);
                assert!(whole == bytes, "{} as {size:?} seed {seed} differs byte by byte", f.name);
                let pieces = events_with(&data, || piece(&mut rng));
                assert!(
                    whole == pieces,
                    "{} as {size:?} seed {seed} differs in random pieces",
                    f.name
                );
            }
        }
    }
}

#[test]
fn a_stream_that_changes_its_packet_size_in_any_pieces() {
    // Every size pair back to back, so the search widens at a position the pieces must not move.
    let f = &FIXTURES[0];
    for (n, first) in SIZES.into_iter().enumerate() {
        for second in SIZES {
            let mut data = reframe(f.data, first);
            data.extend_from_slice(&reframe(f.data, second));
            let whole = events_with(&data, || usize::MAX);
            let bytes = events_with(&data, || 1);
            assert!(whole == bytes, "{first:?} then {second:?} differs byte by byte");
            for seed in 0..10 {
                let mut rng = Rng::new(0x51 + seed * 31 + n as u64);
                let pieces = events_with(&data, || piece(&mut rng));
                assert!(whole == pieces, "{first:?} then {second:?} differs with seed {seed}");
            }
        }
    }
}
