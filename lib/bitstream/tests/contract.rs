// Test fixtures.
#![allow(clippy::disallowed_methods)]

mod drive;

use drive::{Bytes, Tally};

/// Random bytes, xorshift from `seed`.
fn stream(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}

fn runs(drive: fn(&mut Bytes) -> Tally, seeds: u64) -> Tally {
    let mut all = Tally::default();
    for seed in 0..seeds {
        let bytes =
            stream(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15), 1 + (seed as usize * 37) % 600);
        let t = match std::panic::catch_unwind(|| drive(&mut Bytes::new(&bytes))) {
            Ok(t) => t,
            Err(_) => panic!("seed {seed} len {}: {bytes:?}", bytes.len()),
        };
        all.pictures += t.pictures;
        all.skipped += t.skipped;
        all.abandoned += t.abandoned;
        all.errors += t.errors;
        all.discards += t.discards;
    }
    all
}

#[test]
fn h264_slots_go_out_once_and_are_released_once_on_random_streams() {
    let t = runs(drive::h264, 10_000);
    assert!(t.pictures > 250_000, "{t:?}");
    assert!(t.skipped > 0 && t.abandoned > 0 && t.discards > 0, "{t:?}");
}

#[test]
fn h265_slots_go_out_once_and_are_released_once_on_random_streams() {
    let t = runs(drive::h265, 10_000);
    assert!(t.pictures > 250_000, "{t:?}");
    assert!(t.skipped > 0 && t.abandoned > 0 && t.errors > 0 && t.discards > 0, "{t:?}");
}
