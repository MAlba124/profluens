//! The seek index, from the `keyframes` object of `onMetaData` where the muxer wrote one, else
//! grown from the keyframes read, published every [`GROWN_EVERY`] entries, at a flush and the end.

use profluens_core::pipeline::IndexEntry;

use crate::amf::{self, Meta};
use crate::tag::FILE_HEADER;

pub const GROWN_EVERY: usize = 64;

/// Some 72 hours at one a second, for either kind.
pub const MAX_KEYS: usize = 1 << 18;

const RESERVE: usize = 1024;

/// Chosen, not measured. A seek floors to the entry before and the decoder cuts the rest.
pub const AUDIO_SPACING: u64 = 1_000_000_000;

fn ns_of_ms(ms: u64) -> u64 {
    ms.saturating_mul(1_000_000)
}

/// The `keyframes` pairs less `origin_ms`, but those out of order, past the file or not numbers.
// Once at preroll.
#[allow(clippy::disallowed_methods)]
pub fn from_meta(
    body: &[u8],
    meta: &Meta,
    origin_ms: u32,
    file_len: Option<u64>,
) -> Vec<IndexEntry> {
    let (Some(pos), Some(times)) = (meta.filepositions, meta.times) else { return Vec::new() };
    let n = (pos.count.min(times.count) as usize).min(MAX_KEYS);
    let mut out: Vec<IndexEntry> = Vec::with_capacity(n);
    let pairs = amf::numbers(body, pos).zip(amf::numbers(body, times)).take(n);
    for (byte, time) in pairs {
        let (Some(byte), Some(time)) = (byte, time) else { continue };
        let fits = file_len.is_none_or(|len| byte < len as f64);
        if !(byte >= FILE_HEADER as f64 && byte < 2f64.powi(53) && fits) {
            continue;
        }
        if !(0.0..4.3e6).contains(&time) {
            continue;
        }
        let ms = (time * 1000.0).round() as u64;
        let e = IndexEntry::key(ns_of_ms(ms.saturating_sub(u64::from(origin_ms))), byte as u64);
        if out.last().is_none_or(|l| l.time_ns < e.time_ns && l.byte < e.byte) {
            out.push(e);
        }
    }
    out
}

pub struct Grown {
    pub entries: Vec<IndexEntry>,
    pub published: usize,
    pub audio: bool,
    /// Bytes are counted from somewhere unknown, nothing grows until the next seek.
    pub lost: bool,
}

impl Default for Grown {
    fn default() -> Self {
        Self::new()
    }
}

impl Grown {
    // Once per element, the one reservation.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Grown {
        Grown { entries: Vec::with_capacity(RESERVE), published: 0, audio: false, lost: false }
    }

    /// True when the index grew [`GROWN_EVERY`] entries past the last publication.
    pub fn keyframe(&mut self, time_ns: u64, byte: u64) -> bool {
        if self.lost || self.entries.len() >= MAX_KEYS {
            return false;
        }
        let spacing = if self.audio { AUDIO_SPACING } else { 0 };
        let at = self.entries.partition_point(|e| e.time_ns < time_ns);
        // Near one known, read again behind a seek, or out of order with the bytes.
        let next = self
            .entries
            .get(at)
            .is_some_and(|e| e.time_ns - time_ns < spacing.max(1) || e.byte <= byte);
        let prev = at.checked_sub(1).map(|n| &self.entries[n]);
        let prev = prev.is_some_and(|e| time_ns - e.time_ns < spacing || e.byte >= byte);
        if next || prev {
            return false;
        }
        self.entries.insert(at, IndexEntry::key(time_ns, byte));
        self.entries.len() >= self.published + GROWN_EVERY
    }

    pub fn unpublished(&self) -> bool {
        self.entries.len() != self.published
    }
}

#[cfg(test)]
mod tests {
    // Test bodies.
    #![allow(clippy::disallowed_methods)]

    use super::*;

    const S: u64 = 1_000_000_000;

    fn body(pos: &[f64], times: &[f64]) -> (Vec<u8>, Meta) {
        let mut b = Vec::new();
        let mut array = |vs: &[f64]| {
            let at = b.len();
            for v in vs {
                b.push(0);
                b.extend_from_slice(&v.to_be_bytes());
            }
            amf::Array { at, count: vs.len() as u32 }
        };
        let (p, t) = (array(pos), array(times));
        (b, Meta { filepositions: Some(p), times: Some(t), ..Meta::default() })
    }

    #[test]
    fn pairs_out_of_order_or_outside_the_file_are_left_out() {
        let (b, m) = body(
            &[701.0, 400.0, 20_760.0, 30_000.0, f64::NAN, 50_000.0, 90_000.0],
            &[0.0, 0.5, 1.0, 0.9, 3.0, 4.0, 5.0],
        );
        let got: Vec<(u64, u64)> =
            from_meta(&b, &m, 0, Some(80_000)).iter().map(|e| (e.time_ns, e.byte)).collect();
        assert_eq!(got, [(0, 701), (S, 20_760), (4 * S, 50_000)]);
        let shifted = from_meta(&b, &m, 500, None);
        assert_eq!(shifted[1].time_ns, S / 2, "less the origin");
        assert_eq!(shifted.last().map(|e| e.byte), Some(90_000), "no length, no bound");
        assert!(from_meta(&b, &Meta::default(), 0, None).is_empty());
    }

    #[test]
    fn a_keyframe_read_again_or_out_of_order_adds_nothing() {
        let mut g = Grown::new();
        g.keyframe(0, 13);
        g.keyframe(2 * S, 9_400);
        g.keyframe(2 * S, 9_400);
        g.keyframe(S, 9_500);
        g.keyframe(4 * S, 20_000);
        let t: Vec<u64> = g.entries.iter().map(|e| e.time_ns).collect();
        assert_eq!(t, [0, 2 * S, 4 * S]);
        g.lost = true;
        assert!(!g.keyframe(8 * S, 40_000) && g.entries.len() == 3);
    }

    #[test]
    fn audio_takes_one_a_second_and_the_index_is_due_every_64() {
        let mut g = Grown { audio: true, ..Grown::new() };
        for n in 0..300u64 {
            g.keyframe(n * 23 * S / 1000, n * 300);
        }
        assert!(g.entries.windows(2).all(|w| w[1].time_ns - w[0].time_ns >= AUDIO_SPACING));
        let mut g = Grown::new();
        let due: Vec<bool> = (0..130u64).map(|n| g.keyframe(n * S, n * 188)).collect();
        assert_eq!(due.iter().position(|d| *d), Some(GROWN_EVERY - 1));
        assert!(g.unpublished());
    }
}
