//! The seek index `flacdec` publishes for a file read from its first byte. The SEEKTABLE when there
//! is one, else grown from the frames decoded.

use profluens_core::pipeline::IndexEntry;

use pf_tags::flacmeta::SeekPoint;

/// A second. A seek floors to the entry before its target and the decoder cuts the rest.
pub const GROWN_SPACING: u64 = 1_000_000_000;

pub const GROWN_EVERY: usize = 16;

pub const MAX_GROWN: usize = 1 << 18;

pub fn samples_ns(samples: u64, rate: u32) -> Option<u64> {
    let ns = u128::from(samples) * 1_000_000_000 / u128::from(rate).max(1);
    (rate > 0).then_some(ns.min(u128::from(u64::MAX)) as u64)
}

/// A point that does not move the map on is left out, so a corrupt table gives a shorter index.
// Cold, once per stream.
#[allow(clippy::disallowed_methods)]
pub fn table_entries(
    points: &[SeekPoint],
    start: u64,
    rate: u32,
    len: Option<u64>,
) -> Vec<IndexEntry> {
    let mut entries: Vec<IndexEntry> = Vec::with_capacity(points.len());
    for p in points {
        let (Some(time), Some(byte)) =
            (samples_ns(p.sample, rate), start.checked_add(p.byte_offset))
        else {
            continue;
        };
        if len.is_some_and(|len| byte >= len) {
            continue;
        }
        match entries.last() {
            Some(e) if time <= e.time_ns || byte < e.byte => {}
            _ => entries.push(IndexEntry::key(time, byte)),
        }
    }
    entries
}

#[derive(Default)]
pub struct Grown {
    pub entries: Vec<IndexEntry>,
    pub published: usize,
}

impl Grown {
    /// True when the index grew [`GROWN_EVERY`] entries past the last publication.
    pub fn frame(&mut self, time_ns: u64, byte: u64) -> bool {
        let at = self.entries.partition_point(|e| e.time_ns < time_ns);
        // Near one known, read again behind a seek, or out of order with the bytes.
        let next = self
            .entries
            .get(at)
            .is_some_and(|e| e.time_ns - time_ns < GROWN_SPACING || e.byte <= byte);
        let prev = at.checked_sub(1).map(|n| &self.entries[n]);
        let prev = prev.is_some_and(|e| time_ns - e.time_ns < GROWN_SPACING || e.byte >= byte);
        if next || prev || self.entries.len() >= MAX_GROWN {
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
// Fixtures are built on the heap.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn point(sample: u64, byte_offset: u64) -> SeekPoint {
        SeekPoint { sample, byte_offset, frame_samples: 4096 }
    }

    #[test]
    fn a_table_is_made_file_absolute_and_kept_ascending() {
        let points = [
            point(0, 0),
            point(44_100, 20_000),
            point(44_100, 30_000),
            point(88_200, 10_000),
            point(132_300, 60_500),
        ];
        let entries = table_entries(&points, 8_000, 44_100, Some(70_000));
        let got: Vec<(u64, u64)> = entries.iter().map(|e| (e.time_ns, e.byte)).collect();
        assert_eq!(got, [(0, 8_000), (S, 28_000), (3 * S, 68_500)]);
        assert!(
            table_entries(&points, 8_000, 44_100, Some(60_000)).iter().all(|e| e.byte < 60_000)
        );
        assert!(table_entries(&points, 0, 0, None).is_empty(), "no rate, no time");
    }

    #[test]
    fn frames_grow_an_entry_a_second_and_a_reread_adds_nothing() {
        let mut grown = Grown::default();
        let mut due = 0;
        for n in 0..400u64 {
            due += usize::from(grown.frame(n * S / 10, 4_000 + n * 1_000));
            if grown.entries.len() >= grown.published + GROWN_EVERY {
                grown.published = grown.entries.len();
            }
        }
        assert_eq!(grown.entries.len(), 40);
        assert_eq!(due, 2, "published at 16 and 32");
        assert!(
            grown
                .entries
                .iter()
                .enumerate()
                .all(|(i, e)| (e.time_ns, e.byte) == (i as u64 * S, 4_000 + i as u64 * 10_000))
        );
        let before = grown.entries.clone();
        for n in 100..200u64 {
            grown.frame(n * S / 10, 4_000 + n * 1_000);
        }
        assert_eq!(grown.entries, before);
        let mut sparse = Grown::default();
        sparse.frame(0, 100);
        sparse.frame(10 * S, 10_100);
        sparse.frame(5 * S, 20_000);
        assert_eq!(sparse.entries.len(), 2, "bytes out of order with the next");
        sparse.frame(5 * S, 5_100);
        assert_eq!(
            sparse.entries.iter().map(|e| e.time_ns).collect::<Vec<_>>(),
            [0, 5 * S, 10 * S]
        );
    }
}
