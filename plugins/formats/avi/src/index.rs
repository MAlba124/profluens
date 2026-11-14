//! The seek index `avidemux` publishes from `idx1`.

use std::collections::VecDeque;

use profluens_core::feedback::Bitrate;
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::rate::{self, ByteRate};
use profluens_core::time::Timestamp;

use crate::codec;
use crate::idx1::Idx1Body;
use crate::riff::{
    self, AviHeader, Clock, IDX1_ENTRY, Idx1Entry, MAX_STREAMS, MoviLocation, StreamKind,
};

/// Chunks a keyframe restart may walk back over, about 30 s of film with one audio stream.
pub const RING: usize = 1024;

const NONE: u8 = u8::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track {
    /// The `NN` its chunks are named by.
    pub number: usize,
    /// `dwSampleSize`, zero for a chunk a tick.
    pub sample_size: u32,
    /// Its `dwScale/dwRate`, untimed for unknown timing.
    pub clock: Clock,
    /// `dwStart`, the ticks ahead of its first chunk.
    pub start: u64,
    /// The restart covers this stream's chunk at the keyframe.
    pub audio: bool,
    /// Nanoseconds the decoder needs ahead of a target, its pre-roll.
    pub lead_ns: u64,
}

/// The ticks a chunk of `size` bytes advances a stream's clock by. An empty chunk of a stream
/// without a sample size is a dropped frame, one tick as VfW and ffmpeg count it, for audio
/// (`frames` false) none, as ffmpeg counts its blocks.
pub fn ticks(sample_size: u32, frames: bool, size: usize) -> u64 {
    match (sample_size, size) {
        (0, 0) => u64::from(frames),
        (0, _) => 1,
        (_, 0) => 0,
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
    /// Every stream's clock at each entry's keyframe byte.
    key_clocks: Vec<u64>,
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
            video: (video < n && tracks[video].clock.timed()).then_some(video),
            clocks: tracks.iter().map(|t| t.start).collect(),
            tracks,
            slot,
            movi_base: movi_data_start.saturating_sub(4),
            file_len: file_len.unwrap_or(u64::MAX),
            first_chunk,
            relative: None,
            back: vec![0; n],
            ring: VecDeque::new(),
            last: 0,
            entries: Vec::new(),
            marks: Vec::new(),
            mark_clocks: Vec::new(),
            key_clocks: Vec::new(),
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

    /// The OpenDML standard index chunks `ix`, each whole from its header, one after another.
    /// Their chunks go in by file offset, each stream's index lies apart. False when one is no
    /// standard index.
    // Cold path, once per open.
    #[allow(clippy::disallowed_methods)]
    pub fn push_ix(&mut self, mut ix: &[u8]) -> bool {
        let mut all: Vec<(u64, u8, u32, bool)> = Vec::new();
        while let Some(head) = ix.get(..8) {
            let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
            let (chunk, rest) = ix.split_at(ix.len().min(8usize.saturating_add(size)));
            let Some((stream, entries)) = riff::ix_entries(chunk) else { return false };
            if let Some(&place) = self.slot.get(stream).filter(|p| **p != NONE) {
                all.extend(entries.map(|e| (e.byte, place, e.size, e.key)));
            }
            ix = rest;
        }
        all.sort_by_key(|e| e.0);
        if self.ring.capacity() < RING {
            self.ring.reserve_exact(RING);
        }
        for (byte, place, size, key) in all {
            self.read += 1;
            self.chunk(usize::from(place), byte, size, key);
        }
        true
    }

    /// Every stream's clock at `byte`, where an entry restarts there or has its keyframe.
    pub fn clocks_at(&self, byte: u64) -> Option<&[u64]> {
        let w = self.tracks.len();
        let n = self.marks.partition_point(|m| *m < byte);
        if self.marks.get(n) == Some(&byte) {
            return self.mark_clocks.get(n * w..(n + 1) * w);
        }
        let k = self.entries.binary_search_by_key(&byte, |e| e.byte).ok()?;
        self.key_clocks.get(k * w..(k + 1) * w)
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
        self.chunk(place, byte, e.size, e.keyframe);
    }

    /// The chunk of the track at `place` whose header is at `byte`.
    fn chunk(&mut self, place: usize, byte: u64, size: u32, keyframe: bool) {
        if byte < self.last {
            self.ring.clear();
        }
        self.last = byte;
        if keyframe && Some(place) == self.video {
            self.key(place, byte);
        }
        let t = &self.tracks[place];
        let inc = ticks(t.sample_size, !t.audio, size as usize);
        self.clocks[place] = self.clocks[place].saturating_add(inc);
        if self.ring.len() == RING {
            self.ring.pop_front();
        }
        self.ring.push_back(Step { place: place as u8, byte, ticks: inc });
    }

    fn key(&mut self, video: usize, byte: u64) {
        let time = self.tracks[video].clock.ns(self.clocks[video]);
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
            .filter(|(t, _)| t.audio && t.clock.timed())
            .map(|(t, c)| t.clock.ns(*c))
            .fold(time, u64::min);
        self.entries.push(IndexEntry {
            time_ns: time,
            byte,
            restart: at,
            restart_ns: if at < byte { restart_ns } else { time },
        });
        self.key_clocks.extend_from_slice(&self.clocks);
        if self.marks.last().is_none_or(|m| *m < at) {
            self.marks.push(at);
            self.mark_clocks.extend_from_slice(&self.back);
        }
    }
}

/// The [`Scan`] of a file whose header is `header`, None without a timed video stream.
// Cold path, once per open.
#[allow(clippy::disallowed_methods)]
pub(crate) fn scan_for(
    header: &AviHeader,
    movi: &MoviLocation,
    prefix: &[u8],
    len: Option<u64>,
) -> Option<Scan> {
    let tracks: Vec<Track> = header
        .streams
        .iter()
        .map(|s| {
            let audio = s.kind == StreamKind::Audio;
            let rate = u64::from(s.samples_per_sec);
            let lead_ns = if audio {
                profluens_audio::lead_of(codec::family_for(s), rate, 0, None).pre_roll
            } else {
                0
            };
            Track {
                number: s.index,
                sample_size: s.sample_size,
                clock: s.clock().unwrap_or_default(),
                start: u64::from(s.start),
                audio,
                lead_ns,
            }
        })
        .collect();
    let video = header
        .streams
        .iter()
        .zip(&tracks)
        .position(|(s, t)| s.kind == StreamKind::Video && t.clock.timed())?;
    let first = riff::first_chunk(prefix, movi.movi_data_start);
    Some(Scan::new(tracks, video, movi.movi_data_start, first, len))
}

/// The `idx1` entries of a file, read once and handed to each [`crate::AviDemux`] over it
/// through [`crate::AviDemux::with_index`].
pub struct AviIndex {
    pub scan: Scan,
    /// Every entry the `idx1` declares was read.
    pub whole: bool,
}

impl AviIndex {
    /// The index of a file of `len` bytes from its `header` through `movi` and the index
    /// [`crate::Idx1At`] read, an empty one for a file without. The OpenDML standard indexes
    /// when it read them, else `idx1`, which covers the first RIFF only, the index is then not
    /// whole when more follow. None for a header that does not parse or has no timed video, a
    /// demuxer reads no `idx1` for it.
    pub fn build(header: &[u8], idx1: Option<&Idx1Body>, len: Option<u64>) -> Option<AviIndex> {
        let (avi, movi) = riff::probe_header(header).ok()?;
        let movi = movi?;
        if let Some(i) = idx1.filter(|i| !i.ix.is_empty()) {
            let mut odml = scan_for(&avi, &movi, header, len)?;
            if odml.push_ix(&i.ix) && !odml.entries.is_empty() {
                return Some(AviIndex { scan: odml, whole: i.whole });
            }
        }
        let mut scan = scan_for(&avi, &movi, header, len)?;
        scan.push(idx1.map_or(&[][..], |i| &i.body));
        let whole = idx1.is_some_and(|i| i.whole) && !riff::segmented(header, &movi, len);
        Some(AviIndex { scan, whole })
    }
}

fn covered(tracks: &[Track], clocks: &[u64], time: u64) -> bool {
    tracks
        .iter()
        .zip(clocks)
        .filter(|(t, _)| t.audio && t.clock.timed())
        .all(|(t, c)| t.clock.ns(*c) <= time.saturating_sub(t.lead_ns))
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
