//! Test elements. Seedable byte patterns, hashing sinks and clock-aware variants.

mod testsink;
mod testsrc;
mod timed;

pub use testsink::{TestSink, TestSinkStats};
pub use testsrc::TestSrc;
pub use timed::{Render, TimedSinkStats, TimedTestSink, TimedTestSrc};

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One FNV-1a folding step.
#[inline]
pub fn fold(hash: u64, byte: u8) -> u64 {
    (hash ^ byte as u64).wrapping_mul(FNV_PRIME)
}

/// The byte [`TestSrc`] produces at stream position `index`.
#[inline]
pub fn pattern_byte(index: u64) -> u8 {
    (index.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(23) >> 24) as u8
}
