//! The seek index `avidemux` publishes from `idx1`.

use std::collections::VecDeque;

use profluens_core::feedback::Bitrate;
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::rate::{self, ByteRate};
use profluens_core::time::Timestamp;

use crate::riff::{self, IDX1_ENTRY, Idx1Entry, MAX_STREAMS};

/// Chunks a keyframe restart may walk back over, about 30 s of film with one audio stream.
pub const RING: usize = 1024;

const NONE: u8 = u8::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track {
    /// The `NN` its chunks are named by.
    pub number: usize,
    /// `dwSampleSize`, zero for a chunk a tick.
    pub sample_size: u32,
    /// Nanoseconds a tick, zero for unknown timing.
    pub tick_ns: u64,
    /// The restart covers this stream's chunk at the keyframe.
    pub audio: bool,
    /// Nanoseconds the decoder needs ahead of a target, its pre-roll.
    pub lead_ns: u64,
}

/// The ticks a chunk of `size` bytes advances a stream's clock by.
pub fn ticks(sample_size: u32, size: usize) -> u64 {
    match (sample_size, size) {
        (_, 0) => 0,
        (0, _) => 1,
        (n, s) => (s as u64 / n as u64).max(1),
    }
}

#[derive(Clone, Copy)]
struct Step {
    place: u8,
    byte: u64,
    ticks: u64,
}

/// The `idx1` read so far, into index entries and the clocks at each restart.
pub struct Scan {
    tracks: Vec<Track>,
    slot: [u8; MAX_STREAMS],
    video: Option<usize>,
    movi_base: u64,
    file_len: u64,
    first_chunk: Option<u64>,
    relative: Option<bool>,
    clocks: Vec<u64>,
    back: Vec<u64>,
    ring: VecDeque<Step>,
    last: u64,
    pub entries: Vec<IndexEntry>,
    marks: Vec<u64>,
    mark_clocks: Vec<u64>,
    /// Keyframes whose audio lay further ahead than the ring, restarted at their byte.
    pub cut: u32,
    pub read: u64,
}

impl Scan {
    // Cold path, once per element at preroll.
    #[allow(clippy::disallowed_methods)]
    pub fn new(
        tracks: Vec<Track>,
        video: usize,
        movi_data_start: u64,
        first_chunk: Option<u64>,
        file_len: Option<u64>,
    ) -> Scan {
        let mut slot = [NONE; MAX_STREAMS];
        for (place, t) in tracks.iter().enumerate().take(NONE as usize) {
            if let Some(s) = slot.get_mut(t.number) {
                *s = place as u8;
            }
        }
        let n = tracks.len();
        Scan {
            video: (video < n && tracks[video].tick_ns > 0).then_some(video),
            tracks,
            slot,
            movi_base: movi_data_start.saturating_sub(4),
            file_len: file_len.unwrap_or(u64::MAX),
            first_chunk,
            relative: None,
            clocks: vec![0; n],
            back: vec![0; n],
            ring: VecDeque::new(),
            last: 0,
            entries: Vec::new(),
            marks: Vec::new(),
            mark_clocks: Vec::new(),
            cut: 0,
            read: 0,
        }
    }

    pub fn push(&mut self, body: &[u8]) {
        if self.ring.capacity() < RING {
            self.ring.reserve_exact(RING);
        }
        for b in body.as_chunks::<IDX1_ENTRY>().0 {
            self.entry(riff::idx1_entry(b));
        }
    }

    /// Every stream's clock at restart byte `byte`, where an entry restarts there.
    pub fn clocks_at(&self, byte: u64) -> Option<&[u64]> {
        let n = self.marks.partition_point(|m| *m < byte);
        (self.marks.get(n) == Some(&byte))
            .then(|| &self.mark_clocks[n * self.tracks.len()..][..self.tracks.len()])
    }

    fn entry(&mut self, e: Idx1Entry) {
        self.read += 1;
        let relative = match self.relative {
            Some(relative) => relative,
            None => {
                let relative =
                    riff::anchored(e.offset, self.movi_base, self.first_chunk, self.file_len);
                *self.relative.insert(relative)
            }
        };
        let Some(place) = e
            .stream_index()
            .and_then(|n| self.slot.get(n))
            .filter(|p| **p != NONE)
            .map(|p| *p as usize)
        else {
            return;
        };
        let byte = match relative {
            true => self.movi_base.saturating_add(e.offset as u64),
            false => e.offset as u64,
        };
        if byte < self.last {
            self.ring.clear();
        }
        self.last = byte;
        if e.keyframe && Some(place) == self.video {
            self.key(place, byte);
        }
        let inc = ticks(self.tracks[place].sample_size, e.size as usize);
        self.clocks[place] = self.clocks[place].saturating_add(inc);
        if self.ring.len() == RING {
            self.ring.pop_front();
        }
        self.ring.push_back(Step { place: place as u8, byte, ticks: inc });
    }

    fn key(&mut self, video: usize, byte: u64) {
        let time = self.clocks[video].saturating_mul(self.tracks[video].tick_ns);
        if self.entries.last().is_some_and(|e| e.time_ns >= time) {
            return;
        }
        self.back.copy_from_slice(&self.clocks);
        let mut at = byte;
        let mut found = covered(&self.tracks, &self.back, time);
        for step in self.ring.iter().rev() {
            if found {
                break;
            }
            let p = step.place as usize;
            self.back[p] = self.back[p].saturating_sub(step.ticks);
            at = step.byte;
            found = covered(&self.tracks, &self.back, time);
        }
        if !found {
            self.cut += 1;
            at = byte;
            self.back.copy_from_slice(&self.clocks);
        }
        let restart_ns = self
            .tracks
            .iter()
            .zip(&self.back)
            .filter(|(t, _)| t.audio && t.tick_ns > 0)
            .map(|(t, c)| c.saturating_mul(t.tick_ns))
            .fold(time, u64::min);
        self.entries.push(IndexEntry {
            time_ns: time,
            byte,
            restart: at,
            restart_ns: if at < byte { restart_ns } else { time },
        });
        if self.marks.last().is_none_or(|m| *m < at) {
            self.marks.push(at);
            self.mark_clocks.extend_from_slice(&self.back);
        }
    }
}

fn covered(tracks: &[Track], clocks: &[u64], time: u64) -> bool {
    tracks
        .iter()
        .zip(clocks)
        .filter(|(t, _)| t.audio && t.tick_ns > 0)
        .all(|(t, c)| c.saturating_mul(t.tick_ns) <= time.saturating_sub(t.lead_ns))
}

/// The bitrate the demuxer tells at stream time `at`.
pub fn rate_at(index: &Index, measured: &ByteRate, at: Timestamp) -> Option<Bitrate> {
    match index.entries.is_empty() {
        false => rate::of_index(index, at),
        true => measured.rate().or_else(|| rate::average(index)),
    }
}

#[cfg(test)]
mod tests;
