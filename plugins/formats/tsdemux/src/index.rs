//! Seek index grown from keyframes read. Entries ascend in time and bytes, it is published every
//! [`GROWN_EVERY`] entries, at a flush and the end. Past its last entry the keyframe is unknown
//! until it is `complete`, the end read with no stretch skipped since the start.

use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::time::Timestamp;

pub const GROWN_EVERY: usize = 64;

/// Some 72 hours at one a second.
pub const MAX_GROWN: usize = 1 << 18;

/// A quarter hour at one a second, the vector doubles past it.
pub const RESERVE: usize = 1024;

/// Chosen, not measured. A seek floors to the entry before and the decoder cuts the rest.
pub const AUDIO_SPACING: u64 = 1_000_000_000;

/// Whether the counted bytes are file bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    File,
    /// Behind a flush with no target or a break in the input, until the next flush.
    Lost,
    Segments,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub pid: u16,
    pub stream: u32,
    pub video: bool,
}

pub struct Grown {
    pub key: Option<Key>,
    pub placed: Placed,
    pub entries: Vec<IndexEntry>,
    pub published: usize,
    pub file_len: Option<u64>,
    /// What the opener of the file found it lasts, for the estimate past the last entry.
    pub duration: Option<u64>,
    /// Every keyframe from the start on is in `entries`, none skipped over or refused.
    pub gapless: bool,
    /// The furthest byte read while gapless.
    pub reached: u64,
    pub complete: bool,
}

impl Grown {
    #[allow(clippy::disallowed_methods)] // once per element, the one reservation
    pub fn new() -> Self {
        Grown {
            key: None,
            placed: Placed::File,
            entries: Vec::with_capacity(RESERVE),
            published: 0,
            file_len: None,
            duration: None,
            gapless: true,
            reached: 0,
            complete: false,
        }
    }

    /// A run reads the file from its start.
    pub fn open(&mut self) {
        (self.gapless, self.reached) = (true, 0);
    }

    /// Reading goes on at `to` from `at`, `to` being `None` for no file byte. Ahead of what was
    /// read it skips a stretch.
    pub fn jump(&mut self, at: u64, to: Option<u64>) {
        self.reached = self.reached.max(at);
        self.gapless &= to.is_some_and(|to| to <= self.reached);
    }

    /// At the end of the input, true when the index is due again.
    pub fn end(&mut self) -> bool {
        let whole = self.gapless && self.placed == Placed::File && self.key.is_some();
        let due = self.unpublished() || (whole && !self.complete);
        self.complete |= whole;
        due
    }

    pub fn keys(&self, pid: u16) -> bool {
        self.placed == Placed::File && self.key.is_some_and(|key| key.pid == pid)
    }

    /// What was grown belongs to the old key and goes, the new one's keyframes read so far are
    /// not in it.
    pub fn rekey(&mut self, key: Option<Key>) {
        if self.key != key {
            (self.key, self.published) = (key, 0);
            (self.gapless, self.complete) = (false, false);
            self.entries.clear();
        }
    }

    /// True when the index grew [`GROWN_EVERY`] entries past the last publication.
    pub fn keyframe(&mut self, time_ns: u64, byte: u64) -> bool {
        let spacing = match self.key {
            Some(Key { video: false, .. }) => AUDIO_SPACING,
            _ => 0,
        };
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
        if self.entries.len() >= MAX_GROWN {
            self.gapless = false;
            return false;
        }
        self.entries.insert(at, IndexEntry::key(time_ns, byte));
        self.entries.len() >= self.published + GROWN_EVERY
    }

    pub fn unpublished(&self) -> bool {
        self.placed != Placed::Segments && self.entries.len() != self.published
    }

    /// Counts as published.
    #[allow(clippy::disallowed_methods)] // one copy per publication
    pub fn index(&mut self) -> Index {
        self.published = self.entries.len();
        Index {
            entries: self.entries.clone(),
            file_len: self.file_len,
            duration: self.duration.map_or(Timestamp::NONE, Timestamp),
            complete: self.complete,
            stream: self.key.map(|key| key.stream),
            grouped: self.key.is_some_and(|key| key.video),
            ..Index::default()
        }
    }
}

impl Default for Grown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn video() -> Grown {
        let mut g = Grown::new();
        g.rekey(Some(Key { pid: 256, stream: 0, video: true }));
        g
    }

    fn times(g: &Grown) -> Vec<u64> {
        g.entries.iter().map(|e| e.time_ns).collect()
    }

    #[test]
    fn a_keyframe_read_again_or_out_of_order_adds_nothing() {
        let mut g = video();
        assert!(g.keys(256) && !g.keys(257));
        g.keyframe(0, 0);
        g.keyframe(2 * S, 9_400);
        g.keyframe(0, 0);
        g.keyframe(2 * S, 9_400);
        assert_eq!(times(&g), [0, 2 * S]);
        g.keyframe(8 * S, 40_000);
        g.keyframe(4 * S, 20_000);
        assert_eq!(times(&g), [0, 2 * S, 4 * S, 8 * S]);
        g.keyframe(5 * S, 19_000);
        g.keyframe(3 * S, 30_000);
        g.keyframe(3 * S, 9_400);
        assert_eq!(times(&g), [0, 2 * S, 4 * S, 8 * S]);
        assert!(g.entries.windows(2).all(|w| w[0].byte < w[1].byte));
        assert_eq!(
            g.entries[1],
            IndexEntry { time_ns: 2 * S, byte: 9_400, restart: 9_400, restart_ns: 2 * S }
        );
    }

    #[test]
    fn audio_takes_one_unit_a_second() {
        let mut g = Grown::new();
        g.rekey(Some(Key { pid: 257, stream: 1, video: false }));
        for n in 0..300u64 {
            g.keyframe(n * 64 * S / 3_000, n * 376);
        }
        let t = times(&g);
        assert_eq!(t.len(), 7, "{t:?}");
        assert!(t.windows(2).all(|w| w[1] - w[0] >= AUDIO_SPACING));
        let index = g.index();
        assert_eq!((index.stream, index.grouped, index.complete), (Some(1), false, false));
    }

    #[test]
    fn the_index_is_due_every_64_entries() {
        let mut g = video();
        let said: Vec<bool> = (0..130u64).map(|n| g.keyframe(n * S, n * 188)).collect();
        assert_eq!(said.iter().position(|s| *s), Some(GROWN_EVERY - 1));
        assert_eq!(
            said.iter().filter(|s| **s).count(),
            67,
            "every one from the 64th on, until published"
        );
        let index = g.index();
        assert_eq!((index.entries.len(), index.stream, index.grouped), (130, Some(0), true));
        assert!(!g.unpublished());
        assert!(!g.keyframe(130 * S, 130 * 188) && g.unpublished());
    }

    #[test]
    fn the_end_read_with_nothing_skipped_since_the_start_is_complete() {
        let read = |jumps: &[(u64, Option<u64>)]| {
            let mut g = video();
            g.open();
            g.keyframe(0, 0);
            jumps.iter().for_each(|&(at, to)| g.jump(at, to));
            g.keyframe(S, 9_400);
            let due = g.end();
            (due, g.index().complete)
        };
        assert_eq!(read(&[]), (true, true));
        assert_eq!(read(&[(9_000, Some(400)), (2_000, Some(8_000))]), (true, true), "back");
        assert_eq!(read(&[(9_000, Some(9_001))]), (true, false), "ahead of the read");
        assert_eq!(read(&[(9_000, None)]), (true, false), "no file byte");
        let mut g = video();
        g.open();
        g.keyframe(0, 0);
        assert!(g.end() && g.index().complete);
        assert!(!g.end(), "published whole already");
        g.rekey(Some(Key { pid: 257, stream: 1, video: false }));
        assert!(!g.end() && !g.index().complete, "the new key's keyframes read are not in it");
        let mut g = video();
        g.open();
        g.placed = Placed::Lost;
        assert!(!g.end() && !g.index().complete);
    }

    #[test]
    fn bytes_that_are_no_file_bytes_grow_and_publish_nothing() {
        let mut g = video();
        g.placed = Placed::Lost;
        assert!(!g.keys(256));
        g.keyframe(0, 0);
        g.placed = Placed::Segments;
        assert!(!g.keys(256) && !g.unpublished());
        let mut g = video();
        g.keyframe(0, 0);
        g.rekey(Some(Key { pid: 257, stream: 1, video: false }));
        assert!(g.entries.is_empty() && !g.unpublished());
    }
}
