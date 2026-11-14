//! A track's sample tables held in the shape the file states them (ISO/IEC 14496-12 8.6, 8.7),
//! and the walk of every track's samples in file order. A sample is its track and its number
//! there. Its offset is its chunk's plus the sizes before it in the chunk, its times a search
//! of the runs, so the walk forward keeps a place a track and the tables cost what the boxes do.

use crate::boxes::{self, SampleSizes};
use crate::edit::{self, Edit, EditList};
use crate::reader::{MAX_SAMPLES, Mp4Error, PCM_GROUP, Room, Sample, Stbl, TrackKind};

/// A sample of the tables, ordered as the file stores them, by offset then track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SampleId {
    /// The offset above the track index, as [`Sample`] packs them.
    place: u64,
    /// Its number in its track's tables, a group's number for grouped PCM.
    n: u32,
}

impl SampleId {
    pub(crate) fn new(track: u16, offset: u64, n: u32) -> SampleId {
        SampleId { place: offset << Sample::TRACK_BITS | u64::from(track), n }
    }

    pub fn track(&self) -> usize {
        (self.place & ((1 << Sample::TRACK_BITS) - 1)) as usize
    }

    pub fn offset(&self) -> u64 {
        self.place >> Sample::TRACK_BITS
    }

    pub(crate) fn number(&self) -> u32 {
        self.n
    }
}

/// The chunk offsets, 32 bits wide where every one fits.
enum Offsets {
    Narrow(Box<[u32]>),
    Wide(Box<[u64]>),
}

impl Offsets {
    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    fn of(offsets: Vec<u64>) -> Offsets {
        match offsets.iter().all(|&o| o <= u64::from(u32::MAX)) {
            true => Offsets::Narrow(offsets.iter().map(|&o| o as u32).collect()),
            false => Offsets::Wide(offsets.into_boxed_slice()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Offsets::Narrow(o) => o.len(),
            Offsets::Wide(o) => o.len(),
        }
    }

    /// The offset of chunk `chunk`, which must be below [`Offsets::len`].
    fn get(&self, chunk: u32) -> u64 {
        match self {
            Offsets::Narrow(o) => u64::from(o[chunk as usize]),
            Offsets::Wide(o) => o[chunk as usize],
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Offsets::Narrow(o) => size_of_val(&**o),
            Offsets::Wide(o) => size_of_val(&**o),
        }
    }
}

/// The sample sizes, one for all, else 16 bits wide where every one fits.
enum Sizes {
    Constant(u32),
    Narrow(Box<[u16]>),
    Wide(Box<[u32]>),
}

impl Sizes {
    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    fn of(sizes: SampleSizes) -> Sizes {
        match sizes {
            SampleSizes::Constant { sample_size, .. } => Sizes::Constant(sample_size),
            SampleSizes::Table(t) if t.iter().all(|&s| s <= u32::from(u16::MAX)) => {
                Sizes::Narrow(t.iter().map(|&s| s as u16).collect())
            }
            SampleSizes::Table(t) => Sizes::Wide(t.into_boxed_slice()),
        }
    }

    /// The size of sample `n`, 0 past the table.
    fn get(&self, n: u32) -> u32 {
        match self {
            Sizes::Constant(size) => *size,
            Sizes::Narrow(t) => t.get(n as usize).map_or(0, |&s| u32::from(s)),
            Sizes::Wide(t) => t.get(n as usize).copied().unwrap_or(0),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Sizes::Constant(_) => 0,
            Sizes::Narrow(t) => size_of_val(&**t),
            Sizes::Wide(t) => size_of_val(&**t),
        }
    }
}

/// An `stsc` run with the entries before it. An entry is a sample, a frame of grouped PCM.
#[derive(Clone, Copy)]
struct ChunkRun {
    first_chunk: u32,
    per_chunk: u32,
    first_entry: u32,
}

/// An `stts` run with the entries and the time before it.
#[derive(Clone, Copy)]
struct TimeRun {
    first: u32,
    delta: u32,
    dts: u64,
}

/// A `ctts` run with the samples before it.
#[derive(Clone, Copy)]
struct OffsetRun {
    first: u32,
    offset: i32,
}

/// What a track's edit list makes of its samples.
enum Edits {
    None,
    /// The list presents no sample, every time moves back by its first media time.
    Shift(i64),
    Followed {
        edits: Box<[Edit]>,
        /// The sample numbers a video edit decodes from its sync sample to its last shown,
        /// ascending and apart. One of them outside every edit is hidden.
        needed: Box<[(u32, u32)]>,
        /// Audio keeps what lies before its first edit as priming.
        audio: bool,
        /// The sample numbers kept, from the first to past the last, when they are a run
        /// of them. None when the walk has to tell.
        kept: Option<(u32, u32)>,
    },
}

/// One track's tables. Empty for a track without them.
pub(crate) struct TrackTable {
    chunks: Offsets,
    /// The chunks ascending by offset, None for a track that states them so.
    by_offset: Option<Box<[u32]>>,
    /// Never empty for a track with samples, the first starts at chunk 0.
    runs: Box<[ChunkRun]>,
    /// The groups before each run of a grouped PCM track, else empty.
    groups: Box<[u32]>,
    sizes: Sizes,
    /// The bytes a frame of grouped PCM, 0 for a sample an entry.
    group: u32,
    times: Box<[TimeRun]>,
    /// Empty for no `ctts`, else to the last sample.
    offsets: Box<[OffsetRun]>,
    /// The sync samples numbered from 1, None when every one is.
    sync: Option<Box<[u32]>>,
    /// The chunks before `live` the file's end cuts, with the samples of each inside it,
    /// by chunk.
    cuts: Box<[(u32, u32)]>,
    /// Past the last chunk with a sample inside the file.
    live: u32,
    edits: Edits,
    /// The samples inside the file when they are the numbers below it, `prefix`. The end
    /// of a cut file takes the last of them, so they mostly are. Else the samples stated.
    total: u32,
    prefix: bool,
}

impl Default for TrackTable {
    fn default() -> Self {
        TrackTable {
            chunks: Offsets::Narrow(Box::default()),
            by_offset: None,
            runs: Box::default(),
            groups: Box::default(),
            sizes: Sizes::Constant(0),
            group: 0,
            times: Box::default(),
            offsets: Box::default(),
            sync: None,
            cuts: Box::default(),
            live: 0,
            edits: Edits::None,
            total: 0,
            prefix: true,
        }
    }
}

/// How many of `len` ascending keys are at or below `at`, tried at `hint` and the one behind it
/// before a search, so a walk forward costs no search. `hint` is left at the answer.
fn upto(len: usize, key: impl Fn(usize) -> u32, at: u32, hint: &mut u32) -> usize {
    let holds = |i: usize| (i == 0 || key(i - 1) <= at) && (i == len || key(i) > at);
    let mut i = (*hint as usize).min(len);
    if !holds(i) {
        i = match i < len && holds(i + 1) {
            true => i + 1,
            false => bisect(len, |i| key(i) <= at),
        };
    }
    *hint = i as u32;
    i
}

/// How many of `0..len` hold `before`, which holds for a prefix of them.
fn bisect(len: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match before(mid) {
            true => lo = mid + 1,
            false => hi = mid,
        }
    }
    lo
}

/// When a sample numbered `n` that the media presents at `m` is presented by `edits`, and
/// whether hidden, one a video edit decodes and does not show. None for one left out.
fn presented(
    edits: &[Edit],
    needed: &[(u32, u32)],
    audio: bool,
    n: u32,
    m: i64,
) -> Option<(i64, bool)> {
    let first = edits.first()?;
    let within = || {
        let behind = needed.partition_point(|r| r.1 < n);
        needed.get(behind).is_some_and(|r| r.0 <= n)
    };
    match edit::edit_of(edits, m) {
        Some(k) => Some((edits[k].start.saturating_add(m.saturating_sub(edits[k].media)), false)),
        None if within() => Some((edit::hidden_stamp(edits, m), true)),
        None if audio && m < first.media => {
            Some((first.start.saturating_add(m.saturating_sub(first.media)), false))
        }
        None => None,
    }
}

/// A chunk as a walk enters it.
#[derive(Clone, Copy, Default)]
struct ChunkAt {
    chunk: u32,
    /// The number of its first sample.
    first: u32,
    /// The number of its first entry, a frame of grouped PCM.
    entry: u32,
    /// The entries the tables give it.
    per: u32,
    /// Its samples inside the file.
    count: u32,
    offset: u64,
    /// The next chunk of the track that may hold a sample.
    next: u32,
}

/// A place in one track's samples in the order the file stores them.
#[derive(Clone, Copy, Default)]
pub(crate) struct Head {
    /// The chunk by offset, and the sample in it.
    ord: u32,
    k: u32,
    at: ChunkAt,
    /// Where sample `k` lies.
    offset: u64,
    /// Past the last sample.
    done: bool,
    /// The chunk by offset the walk last left by its end, and where its last sample lies.
    left: Option<(u32, u64)>,
    /// Where the runs were last found, see [`upto`].
    run: u32,
    time: u32,
    cts: u32,
    sync: u32,
}

impl TrackTable {
    /// The tables of `s` for track `track`, the samples past the end `room` knows left out and
    /// every one inside it charged. False when the edit list presents no sample.
    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn fold(
        track: u16,
        s: &Stbl<'_>,
        room: &mut Room,
    ) -> Result<(TrackTable, bool), Mp4Error> {
        let stts = boxes::parse_stts(s.stts)?;
        let sizes = if s.stz2 { boxes::parse_stz2(s.sizes)? } else { boxes::parse_stsz(s.sizes)? };
        let stsc = boxes::parse_stsc(s.stsc)?;
        let chunk_offsets = boxes::parse_chunk_offsets(s.offsets, s.co64)?;
        let ctts = match s.ctts {
            Some(body) => boxes::parse_ctts(body)?,
            None => Vec::new(),
        };
        // Absent, every sample is sync (8.6.2).
        let stss = s.stss.map(boxes::parse_stss).transpose()?;
        let group = match sizes {
            SampleSizes::Constant { sample_size: 1, .. } => s.pcm_frame,
            SampleSizes::Constant { sample_size, .. } if sample_size == s.pcm_frame => sample_size,
            _ => 0,
        };
        let entries = sizes.count();
        if entries == 0 {
            return Ok((TrackTable::default(), true));
        }
        if group == 0 && entries > MAX_SAMPLES {
            return Err(Mp4Error::TooManySamples { limit: MAX_SAMPLES });
        }
        let chunk_count = chunk_offsets.len();
        if chunk_count == 0 {
            return Err(Mp4Error::Inconsistent("no chunk offsets"));
        }
        if stsc.is_empty() {
            return Err(Mp4Error::Inconsistent("empty stsc"));
        }
        // The chunks before the first run hold nothing.
        let mut runs = vec![ChunkRun { first_chunk: 0, per_chunk: 0, first_entry: 0 }];
        let mut groups = vec![0u32];
        let (mut mapped, mut grouped) = (0u64, 0u64);
        for (i, run) in stsc.iter().enumerate() {
            // first_chunk is 1-based. 0 or past chunk_count is malformed.
            if run.first_chunk == 0 {
                return Err(Mp4Error::Inconsistent("stsc first_chunk == 0"));
            }
            let start = (run.first_chunk - 1) as usize;
            if start >= chunk_count {
                return Err(Mp4Error::Inconsistent("stsc first_chunk past chunk count"));
            }
            let end = match stsc.get(i + 1) {
                Some(next) if next.first_chunk <= run.first_chunk => {
                    return Err(Mp4Error::Inconsistent("stsc first_chunk not increasing"));
                }
                Some(next) => ((next.first_chunk - 1) as usize).min(chunk_count),
                None => chunk_count,
            };
            // The total is checked below, a run past it is never read.
            let (first_entry, first_group) = (mapped, grouped);
            let chunks = (end - start) as u64;
            mapped = mapped.saturating_add(chunks * u64::from(run.samples_per_chunk));
            grouped = grouped
                .saturating_add(chunks * u64::from(run.samples_per_chunk.div_ceil(PCM_GROUP)));
            let first_entry = first_entry.min(u64::from(u32::MAX)) as u32;
            if start == 0 {
                runs.clear();
                groups.clear();
            }
            runs.push(ChunkRun {
                first_chunk: start as u32,
                per_chunk: run.samples_per_chunk,
                first_entry,
            });
            groups.push(first_group.min(u64::from(u32::MAX)) as u32);
        }
        // The chunk map must describe exactly the entries, else reject.
        if mapped != entries as u64 {
            return Err(Mp4Error::Inconsistent("stsc/stco sample total != stsz sample count"));
        }
        let stated: u64 = stts.iter().map(|r| u64::from(r.count)).sum();
        let short = Mp4Error::Inconsistent("stts describes fewer samples than stsz");
        // Each sample is charged to the room as the walk in the track's order reaches it. A
        // sample past the end cuts its chunk there, offsets rise through a chunk.
        let mut cuts: Vec<(u32, u32)> = Vec::new();
        // past the last chunk with a sample in the file, and past that sample's number
        let (mut live, mut inside) = (0u32, 0u64);
        for (r, run) in runs.iter().enumerate() {
            let end = runs.get(r + 1).map_or(chunk_count as u32, |next| next.first_chunk);
            for chunk in run.first_chunk..end {
                let mut offset = chunk_offsets[chunk as usize];
                let entry = u64::from(run.first_entry)
                    + u64::from(chunk - run.first_chunk) * u64::from(run.per_chunk);
                let samples =
                    if group == 0 { run.per_chunk } else { run.per_chunk.div_ceil(PCM_GROUP) };
                let first = match group {
                    0 => entry,
                    _ => {
                        u64::from(groups[r])
                            + u64::from(chunk - run.first_chunk) * u64::from(samples)
                    }
                };
                let mut kept = samples;
                for k in 0..samples {
                    let (size, at) = match group {
                        0 => (sizes.get(entry as usize + k as usize), entry + u64::from(k)),
                        group => {
                            let frames = (run.per_chunk - k * PCM_GROUP).min(PCM_GROUP);
                            let size = frames
                                .checked_mul(group)
                                .ok_or(Mp4Error::Inconsistent("pcm group size overflow"))?;
                            (Some(size), entry + u64::from(k) * u64::from(PCM_GROUP))
                        }
                    };
                    let size =
                        size.ok_or(Mp4Error::Inconsistent("sample index past size table"))?;
                    if !room.admits(offset, size)? {
                        cuts.push((chunk, k));
                        kept = k;
                        break;
                    }
                    if at >= stated {
                        return Err(short);
                    }
                    Sample::new(track, offset, size, 0, 0, true)?;
                    offset = offset
                        .checked_add(u64::from(size))
                        .ok_or(Mp4Error::Inconsistent("chunk offset overflow"))?;
                }
                if kept > 0 {
                    (live, inside) = (chunk + 1, first + u64::from(kept));
                }
            }
        }
        if stated < entries as u64 {
            return Err(short);
        }
        // The chunks past the last one inside the file are left out as one. With no other
        // cut than of that last chunk, the samples inside are the numbers below `inside`.
        cuts.retain(|cut| cut.0 < live);
        let prefix = cuts.iter().all(|cut| cut.0 + 1 == live);
        let stated_samples = if group == 0 { entries as u64 } else { grouped };
        let total = if prefix { inside } else { stated_samples }.min(u64::from(u32::MAX)) as u32;
        let mut times: Vec<TimeRun> = Vec::new();
        let (mut first, mut dts) = (0u64, 0u64);
        for run in stts.iter().filter(|run| run.count != 0) {
            if first >= entries as u64 {
                break;
            }
            times.push(TimeRun { first: first as u32, delta: run.delta, dts });
            let count = u64::from(run.count).min(entries as u64 - first);
            first += count;
            dts = count
                .checked_mul(u64::from(run.delta))
                .and_then(|span| dts.checked_add(span))
                .ok_or(Mp4Error::Inconsistent("cumulative dts overflow"))?;
        }
        let mut offsets: Vec<OffsetRun> = Vec::new();
        let mut first = 0u64;
        for run in ctts.iter().filter(|run| run.count != 0) {
            if first >= entries as u64 {
                break;
            }
            offsets.push(OffsetRun { first: first as u32, offset: run.offset as i32 });
            first += u64::from(run.count);
        }
        // Past the table the offsets are zero.
        if !offsets.is_empty() && first < entries as u64 {
            offsets.push(OffsetRun { first: first as u32, offset: 0 });
        }
        let mut table = TrackTable {
            chunks: Offsets::of(chunk_offsets),
            by_offset: None,
            runs: runs.into_boxed_slice(),
            groups: if group == 0 { Box::default() } else { groups.into_boxed_slice() },
            sizes: Sizes::of(sizes),
            group,
            times: times.into_boxed_slice(),
            offsets: offsets.into_boxed_slice(),
            sync: stss.map(Vec::into_boxed_slice),
            cuts: cuts.into_boxed_slice(),
            live,
            edits: Edits::None,
            total,
            prefix,
        };
        // The edits go by the track's order, read before the chunks are put in the file's.
        let presented = match &s.edits {
            Some(list) if !table.first().done => table.follow(track, s.kind, list),
            _ => true,
        };
        let chunks = &table.chunks;
        let ascends = (1..live).all(|c| chunks.get(c - 1) <= chunks.get(c));
        if !ascends {
            // only the chunks with a sample inside the file
            let mut hint = 0;
            let holds = |c: &u32| table.chunk(*c, &mut hint).is_some_and(|at| at.count > 0);
            let mut by: Vec<u32> = (0..live).filter(holds).collect();
            by.sort_by_key(|&c| chunks.get(c));
            table.by_offset = Some(by.into_boxed_slice());
        }
        Ok((table, presented))
    }

    /// Takes the edit list `list` as [`edit::apply`] does. False when it presents no sample,
    /// the times then move back by its first media time.
    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    fn follow(&mut self, track: u16, kind: TrackKind, list: &EditList) -> bool {
        let edits = &list.edits;
        let first_media = edits.first().map(|e| e.media);
        let priming = |m: i64| kind == TrackKind::Audio && first_media.is_some_and(|f| m < f);
        let mut shown = vec![(u32::MAX, 0u32); edits.len()];
        // the samples an edit or the priming keeps, the first, the last and how many
        let mut kept = (u32::MAX, 0u32, 0u32);
        let mut see = |n: u32, m: i64| {
            let k = edit::edit_of(edits, m);
            if let Some(k) = k {
                let (first, last) = &mut shown[k];
                (*first, *last) = ((*first).min(n), (*last).max(n));
            }
            if k.is_some() || priming(m) {
                kept = (kept.0.min(n), kept.1.max(n), kept.2 + 1);
            }
        };
        // With every sample in the file and one an entry, the times are read off the runs.
        let direct = self.group == 0 && self.prefix;
        match direct {
            true => {
                let (mut time, mut cts) = (0, 0);
                for n in 0..self.total {
                    let raw =
                        i128::from(self.dts(n, &mut time)) + i128::from(self.cts(n, &mut cts));
                    see(n, raw.min(i128::from(i64::MAX)) as i64);
                }
            }
            false => {
                let mut h = self.first();
                while let Some((id, s)) = self.kept(track, &mut h) {
                    see(id.n, s.pts());
                    self.step(&mut h);
                }
            }
        }
        if shown.iter().all(|r| r.0 == u32::MAX) {
            self.edits = Edits::Shift(list.first_media());
            return false;
        }
        let mut needed: Vec<(u32, u32)> = Vec::new();
        if kind == TrackKind::Video {
            let mut firsts: Vec<(u32, u32)> =
                shown.iter().copied().filter(|r| r.0 != u32::MAX).collect();
            firsts.sort_unstable();
            match direct {
                // the sync sample at or before each first, the track's first sample without one
                true => needed.extend(firsts.iter().map(|&(first, last)| {
                    let key = match &self.sync {
                        None => first,
                        Some(sync) => {
                            let upto = sync.partition_point(|&s| s <= first + 1);
                            upto.checked_sub(1).map_or(0, |i| sync[i].saturating_sub(1))
                        }
                    };
                    (key, last)
                })),
                false => {
                    let mut h = self.first();
                    let (mut key, mut at) = (h.at.first + h.k, 0usize);
                    while let Some((id, s)) = self.kept(track, &mut h) {
                        if at == firsts.len() {
                            break;
                        }
                        if s.sync() {
                            key = id.n;
                        }
                        while firsts.get(at).is_some_and(|f| f.0 == id.n) {
                            needed.push((key, firsts[at].1));
                            at += 1;
                        }
                        self.step(&mut h);
                    }
                }
            }
            needed.sort_unstable();
            let mut merged: Vec<(u32, u32)> = Vec::with_capacity(needed.len());
            for (a, b) in needed {
                match merged.last_mut() {
                    Some(m) if a <= m.1.saturating_add(1) => m.1 = m.1.max(b),
                    _ => merged.push((a, b)),
                }
            }
            needed = merged;
        }
        // Video keeps what its edits decode, another track what they or the priming show.
        let run = match (kind, &needed[..]) {
            (TrackKind::Video, &[(from, to)]) => Some((from, to + 1)),
            (TrackKind::Video, _) => None,
            _ => (kept.2 == kept.1 - kept.0 + 1).then_some((kept.0, kept.1 + 1)),
        };
        self.edits = Edits::Followed {
            edits: edits.clone().into_boxed_slice(),
            needed: needed.into_boxed_slice(),
            audio: kind == TrackKind::Audio,
            kept: run.filter(|_| direct),
        };
        true
    }

    /// The heap held.
    pub(crate) fn bytes(&self) -> usize {
        let edits = match &self.edits {
            Edits::Followed { edits, needed, .. } => size_of_val(&**edits) + size_of_val(&**needed),
            _ => 0,
        };
        self.chunks.bytes()
            + self.by_offset.as_ref().map_or(0, |by| size_of_val(&**by))
            + size_of_val(&*self.runs)
            + size_of_val(&*self.groups)
            + self.sizes.bytes()
            + size_of_val(&*self.times)
            + size_of_val(&*self.offsets)
            + self.sync.as_ref().map_or(0, |sync| size_of_val(&**sync))
            + size_of_val(&*self.cuts)
            + edits
    }

    /// Chunk `chunk` of the track.
    fn chunk(&self, chunk: u32, hint: &mut u32) -> Option<ChunkAt> {
        if chunk >= self.live {
            return None;
        }
        let r = upto(self.runs.len(), |i| self.runs[i].first_chunk, chunk, hint).checked_sub(1)?;
        let run = self.runs[r];
        let into = chunk - run.first_chunk;
        // Inside the entries the tables state, so inside `u32`.
        let entry = run.first_entry.wrapping_add(into.wrapping_mul(run.per_chunk));
        let (first, stated) = match self.group {
            0 => (entry, run.per_chunk),
            _ => {
                let each = run.per_chunk.div_ceil(PCM_GROUP);
                (self.groups[r].wrapping_add(into.wrapping_mul(each)), each)
            }
        };
        let cut = self.cuts.binary_search_by_key(&chunk, |c| c.0).ok().map(|at| self.cuts[at].1);
        let count = cut.unwrap_or(stated);
        // a run of chunks without a sample is stepped over as one
        let next = match (run.per_chunk, self.runs.get(r + 1)) {
            (0, Some(next)) => next.first_chunk,
            _ => chunk + 1,
        };
        Some(ChunkAt {
            chunk,
            first,
            entry,
            per: run.per_chunk,
            count,
            offset: self.chunks.get(chunk),
            next,
        })
    }

    /// The chunks by offset a walk passes.
    fn ords(&self) -> usize {
        self.by_offset.as_ref().map_or(self.live as usize, |by| by.len())
    }

    /// The chunk the track stores `ord`th by offset.
    fn chunk_of(&self, ord: u32) -> u32 {
        match &self.by_offset {
            Some(by) => by.get(ord as usize).copied().unwrap_or(u32::MAX),
            None => ord,
        }
    }

    /// `h` at the first sample of the chunks from the `ord`th on.
    fn enter(&self, h: &mut Head, mut ord: u32) {
        loop {
            match self.chunk(self.chunk_of(ord), &mut h.run) {
                // a track in the file's order numbers its chunks as it stores them
                Some(at) if at.count == 0 && self.by_offset.is_none() => ord = at.next.max(ord + 1),
                Some(at) if at.count == 0 => ord += 1,
                Some(at) => {
                    (h.ord, h.k, h.at, h.offset, h.done) = (ord, 0, at, at.offset, false);
                    return;
                }
                None => {
                    (h.ord, h.k, h.done) = (self.ords() as u32, 0, true);
                    return;
                }
            }
        }
    }

    /// The track's first sample by offset.
    pub(crate) fn first(&self) -> Head {
        let mut h = Head::default();
        self.enter(&mut h, 0);
        h
    }

    /// The size of the sample `h` is at.
    fn size(&self, h: &Head) -> u32 {
        match self.group {
            0 => self.sizes.get(h.at.first + h.k),
            group => (h.at.per - h.k * PCM_GROUP).min(PCM_GROUP).wrapping_mul(group),
        }
    }

    /// `h` to the sample behind the one it is at.
    pub(crate) fn step(&self, h: &mut Head) {
        if h.done {
            return;
        }
        let at = h.offset;
        h.offset += u64::from(self.size(h));
        h.k += 1;
        if h.k >= h.at.count {
            h.left = Some((h.ord, at));
            self.enter(h, h.ord + 1);
        }
    }

    /// `h` to sample `k` of the chunk it is in, by the sizes between.
    fn within(&self, h: &mut Head, k: u32) {
        let constant = match (self.group, &self.sizes) {
            (0, Sizes::Constant(size)) => Some(u64::from(*size)),
            (0, _) => None,
            // every group before a chunk's last is whole
            (group, _) => Some(u64::from(PCM_GROUP) * u64::from(group)),
        };
        if let Some(size) = constant {
            (h.k, h.offset) = (k, h.at.offset + u64::from(k) * size);
            return;
        }
        while h.k < k {
            h.offset += u64::from(self.size(h));
            h.k += 1;
        }
        while h.k > k {
            h.k -= 1;
            h.offset -= u64::from(self.size(h));
        }
    }

    /// `h` at the track's first sample at or past `byte`. A place in the chunk `h` is in is
    /// walked to from where it is.
    pub(crate) fn to_byte(&self, h: &mut Head, byte: u64) {
        let before = bisect(self.ords(), |ord| self.chunks.get(self.chunk_of(ord as u32)) < byte);
        let Some(ord) = before.checked_sub(1).map(|ord| ord as u32) else {
            return self.enter(h, 0);
        };
        // A chunk whose last sample lies before the byte holds nothing at or past it, so a
        // walk back by a few samples never reads a chunk again from its start.
        if h.left.is_some_and(|(left, last)| left == ord && last < byte) {
            if h.done || h.ord != ord + 1 || h.k != 0 {
                self.enter(h, ord + 1);
            }
            return;
        }
        if h.done || h.ord != ord {
            self.enter(h, ord);
        }
        if h.done || h.ord != ord {
            return;
        }
        // back while the sample before is at or past it, then on while this one is before
        while h.k > 0 && h.offset >= byte {
            let k = h.k - 1;
            self.within(h, k);
        }
        while !h.done && h.ord == ord && h.offset < byte {
            self.step(h);
        }
    }

    /// `h` at sample number `n` of the track, the next inside the file when the end cut it.
    pub(crate) fn to_sample(&self, h: &mut Head, n: u32) {
        let first = |i: usize| match self.group {
            0 => self.runs[i].first_entry,
            _ => self.groups[i],
        };
        let mut hint = h.run;
        let Some(r) = upto(self.runs.len(), first, n, &mut hint).checked_sub(1) else {
            return self.enter(h, 0);
        };
        let run = self.runs[r];
        let each = if self.group == 0 { run.per_chunk } else { run.per_chunk.div_ceil(PCM_GROUP) };
        if each == 0 {
            (h.k, h.done) = (0, true);
            return;
        }
        let into = n - first(r);
        let (chunk, k) = (run.first_chunk.saturating_add(into / each), into % each);
        if h.done || h.at.chunk != chunk {
            let ord = match &self.by_offset {
                None => chunk,
                // a chunk past the file's end or without a sample is not among them
                Some(by) if (chunk as usize) < self.chunks.len() => {
                    let at = self.chunks.get(chunk);
                    let from = bisect(by.len(), |ord| self.chunks.get(by[ord]) < at);
                    let same = by[from..].iter().take_while(|&&c| self.chunks.get(c) == at);
                    same.clone().position(|&c| c == chunk).map_or(from, |i| from + i) as u32
                }
                Some(by) => by.len() as u32,
            };
            self.enter(h, ord);
            if h.done || h.at.chunk != chunk {
                return;
            }
        }
        match k < h.at.count {
            true => self.within(h, k),
            false => self.enter(h, h.ord + 1),
        }
    }

    fn dts(&self, entry: u32, hint: &mut u32) -> u64 {
        let i = upto(self.times.len(), |i| self.times[i].first, entry, hint);
        let Some(run) = i.checked_sub(1).map(|i| self.times[i]) else { return 0 };
        run.dts + u64::from(entry - run.first) * u64::from(run.delta)
    }

    fn cts(&self, n: u32, hint: &mut u32) -> i32 {
        let i = upto(self.offsets.len(), |i| self.offsets[i].first, n, hint);
        i.checked_sub(1).map_or(0, |i| self.offsets[i].offset)
    }

    fn is_sync(&self, n: u32, hint: &mut u32) -> bool {
        let Some(sync) = &self.sync else { return true };
        let i = upto(sync.len(), |i| sync[i], n + 1, hint);
        i.checked_sub(1).is_some_and(|i| sync[i] == n + 1)
    }

    /// The sample `h` is at with its edit taken, None for one the edit list leaves out.
    fn resolve(&self, track: u16, h: &mut Head) -> Option<Sample> {
        let n = h.at.first + h.k;
        let (size, entry, cts, sync) = match self.group {
            0 => (self.sizes.get(n), n, self.cts(n, &mut h.cts), self.is_sync(n, &mut h.sync)),
            _ => (self.size(h), h.at.entry + h.k * PCM_GROUP, 0, true),
        };
        let raw = self.dts(entry, &mut h.time);
        let dts = raw.min(i64::MAX as u64) as i64;
        let pts = (i128::from(raw) + i128::from(cts)).min(i128::from(i64::MAX)) as i64;
        let mut s = Sample::new(track, h.offset, size, dts, pts, sync).ok()?;
        match &self.edits {
            Edits::None => {}
            Edits::Shift(by) => s.back(*by),
            Edits::Followed { edits, needed, audio, .. } => {
                let (pts, hidden) = presented(edits, needed, *audio, n, s.pts())?;
                if hidden {
                    s.hide();
                }
                s.present_at(pts);
            }
        }
        Some(s)
    }

    /// The first sample from `h` on the edit list keeps, `h` left at it.
    pub(crate) fn kept(&self, track: u16, h: &mut Head) -> Option<(SampleId, Sample)> {
        while !h.done {
            if let Some(s) = self.resolve(track, h) {
                return Some((SampleId::new(track, h.offset, h.at.first + h.k), s));
            }
            self.step(h);
        }
        None
    }

    /// The sample numbers the track gives, from the first to past the last, when they are a
    /// run of them stored in the track's order and all inside the file. None when only the
    /// walk tells which samples there are.
    fn span(&self) -> Option<(u32, u32)> {
        let kept = match &self.edits {
            Edits::None | Edits::Shift(_) => Some((0, self.total)),
            Edits::Followed { kept, .. } => *kept,
        };
        kept.filter(|_| self.by_offset.is_none() && self.prefix)
    }

    /// The [`TrackTable::span`] of a track whose samples are presented in the order of their
    /// numbers, one without composition offsets or samples a video edit hides, so a time is
    /// found by a search.
    fn rising(&self) -> Option<(u32, u32)> {
        let shown = match &self.edits {
            Edits::Followed { needed, .. } => needed.is_empty(),
            _ => true,
        };
        self.span().filter(|_| shown && self.offsets.is_empty())
    }

    /// When sample `n` of a [`TrackTable::rising`] track is presented.
    fn pts_of(&self, n: u32) -> i64 {
        let entry = match self.group {
            0 => n,
            _ => {
                let groups = &self.groups;
                let Some(r) = upto(groups.len(), |i| groups[i], n, &mut 0).checked_sub(1) else {
                    return 0;
                };
                let (run, into) = (self.runs[r], n - groups[r]);
                let each = run.per_chunk.div_ceil(PCM_GROUP).max(1);
                let frames = (into / each).wrapping_mul(run.per_chunk);
                run.first_entry.wrapping_add(frames).wrapping_add(into % each * PCM_GROUP)
            }
        };
        let dts = self.dts(entry, &mut 0).min(i64::MAX as u64) as i64;
        match &self.edits {
            Edits::None => dts,
            Edits::Shift(by) => dts.saturating_sub(*by),
            Edits::Followed { edits, needed, audio, .. } => {
                presented(edits, needed, *audio, n, dts).map_or(dts, |at| at.0)
            }
        }
    }

    /// The sizes of the samples in the file's order without their walk, None for a track
    /// only the walk tells of.
    pub(crate) fn sizes(&self) -> Option<impl Iterator<Item = u32> + '_> {
        let (from, to) = self.span().filter(|_| self.group == 0)?;
        Some((from..to).map(|n| self.sizes.get(n)))
    }

    /// The numbers of the sync samples below `n`, the latest first.
    pub(crate) fn keys_before(&self, n: u32) -> impl Iterator<Item = u32> + '_ {
        let listed = self.sync.as_ref().map(|sync| {
            let below = sync.partition_point(|&s| s <= n);
            sync[..below].iter().rev().map(|&s| s - 1)
        });
        let all = self.sync.is_none().then(|| (0..n).rev());
        listed.into_iter().flatten().chain(all.into_iter().flatten())
    }
}

/// The byte runs a track's samples take chunk by chunk, see [`disjoint`].
struct Extents<'a> {
    table: &'a TrackTable,
    head: Head,
    /// Past the last sample.
    to: u32,
    next: Option<(u64, u64)>,
}

impl Extents<'_> {
    /// The run of the next chunk with a sample of some bytes, from its first such sample to
    /// the end of its last.
    fn read(&mut self) {
        let (table, head) = (self.table, &mut self.head);
        self.next = None;
        while self.next.is_none() && !head.done && head.at.first + head.k < self.to {
            let ord = head.ord;
            let (mut start, mut end) = (None, 0);
            while !head.done && head.ord == ord && head.at.first + head.k < self.to {
                let size = u64::from(table.size(head));
                if size != 0 {
                    start.get_or_insert(head.offset);
                    end = head.offset.saturating_add(size);
                }
                table.step(head);
            }
            self.next = start.map(|start| (start, end));
        }
    }
}

/// The samples of `tables` when no two of some bytes share a byte, an error when two do. By
/// the chunks, for tracks that each give a run of their samples in order. None when one does
/// not, or stores its chunks so that only the walk of every sample tells.
// Cold path, once per movie.
#[allow(clippy::disallowed_methods)]
pub(crate) fn disjoint(tables: &[TrackTable]) -> Option<Result<usize, Mp4Error>> {
    let mut count = 0usize;
    let mut runs = Vec::with_capacity(tables.len());
    for table in tables {
        let (from, to) = table.span()?;
        count += (to - from) as usize;
        let mut head = table.first();
        table.to_sample(&mut head, from);
        let mut extents = Extents { table, head, to, next: None };
        extents.read();
        runs.push(extents);
    }
    let mut covered = 0u64;
    loop {
        let least = runs.iter_mut().filter(|r| r.next.is_some()).min_by_key(|r| r.next);
        let Some(run) = least else { return Some(Ok(count)) };
        let (start, end) = run.next?;
        if start < covered {
            return Some(Err(Mp4Error::Inconsistent("samples overlap in the file")));
        }
        covered = end;
        run.read();
        // a track whose runs fall back is not merged by its chunks
        if run.next.is_some_and(|next| next.0 < start) {
            return None;
        }
    }
}

/// One track's samples from a place on, in the order the file stores them.
pub(crate) struct TrackIter<'a> {
    table: &'a TrackTable,
    track: u16,
    head: Head,
}

impl<'a> TrackIter<'a> {
    pub(crate) fn new(table: &'a TrackTable, track: usize) -> Self {
        TrackIter { table, track: track as u16, head: table.first() }
    }

    /// From the track's first sample at or past `byte`.
    pub(crate) fn at_byte(mut self, byte: u64) -> Self {
        self.table.to_byte(&mut self.head, byte);
        self
    }

    /// From sample number `n`.
    pub(crate) fn at_sample(mut self, n: u32) -> Self {
        self.table.to_sample(&mut self.head, n);
        self
    }
}

impl Iterator for TrackIter<'_> {
    type Item = (SampleId, Sample);

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.table.kept(self.track, &mut self.head)?;
        self.table.step(&mut self.head);
        Some(next)
    }
}

/// One track's sync samples in the order the file stores them, read off the list of them
/// where the track has one and stores its chunks in order, else by the walk of every sample.
pub(crate) struct Keys<'a> {
    table: &'a TrackTable,
    track: u16,
    head: Head,
    /// The next entry of the sync list, None for the walk.
    at: Option<usize>,
}

impl<'a> Keys<'a> {
    /// From the track's first sample at or past `floor`.
    pub(crate) fn new(table: &'a TrackTable, track: usize, floor: u64) -> Self {
        let mut head = table.first();
        table.to_byte(&mut head, floor);
        let from = head.at.first + head.k;
        let listed = table.sync.as_ref().filter(|_| table.by_offset.is_none() && !head.done);
        let at = listed.map(|sync| sync.partition_point(|&s| s <= from));
        Keys { table, track: track as u16, head, at }
    }

    /// How many are left when every sample is one and none is left out, None when only the
    /// walk tells.
    pub(crate) fn len(&self) -> Option<usize> {
        let all = self.table.sync.is_none() && !self.head.done;
        let (from, to) = self.table.span().filter(|_| all)?;
        Some(to.saturating_sub(from.max(self.head.at.first + self.head.k)) as usize)
    }
}

impl Iterator for Keys<'_> {
    type Item = (SampleId, Sample);

    fn next(&mut self) -> Option<Self::Item> {
        let (table, track) = (self.table, self.track);
        let Some(at) = &mut self.at else {
            loop {
                let next = table.kept(track, &mut self.head)?;
                table.step(&mut self.head);
                if next.1.sync() {
                    return Some(next);
                }
            }
        };
        let sync = table.sync.as_deref()?;
        while let Some(&key) = sync.get(*at) {
            *at += 1;
            // numbered from 1, one past the end or left out by an edit is no sample
            let Some(n) = key.checked_sub(1) else { continue };
            if table.prefix && n >= table.total {
                *at = sync.len();
                break;
            }
            table.to_sample(&mut self.head, n);
            if self.head.done || self.head.at.first + self.head.k != n {
                continue;
            }
            if let Some(s) = table.resolve(track, &mut self.head) {
                return Some((SampleId::new(track, self.head.offset, n), s));
            }
        }
        None
    }

    /// A step over samples that are all sync ones is a step of the walk's place.
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        if let Some((from, _)) = self.table.span().filter(|_| n > 0 && self.len().is_some()) {
            let at = from.max(self.head.at.first + self.head.k);
            let to = at.saturating_add(n.min(u32::MAX as usize) as u32);
            self.table.to_sample(&mut self.head, to);
            return self.next();
        }
        (0..n).try_for_each(|_| self.next().map(drop))?;
        self.next()
    }
}

/// How a [`Seeker`] finds a track's samples.
enum Seek<'a> {
    /// The sync samples one by one.
    Keys(std::iter::Peekable<Keys<'a>>),
    /// Every sample one by one.
    Every(std::iter::Peekable<TrackIter<'a>>),
    /// By a search of the times of the samples from `next` to `end`.
    Rising { head: Head, next: u32, end: u32 },
}

/// One track's samples a seek may land on, taken in order up to rising bounds. The sync
/// samples of video, every sample of another track.
pub(crate) struct Seeker<'a> {
    table: &'a TrackTable,
    track: u16,
    seek: Seek<'a>,
    /// The first sample seen.
    pub(crate) first: Option<(SampleId, Sample)>,
    /// The last sample taken.
    pub(crate) best: Option<(SampleId, Sample)>,
}

impl<'a> Seeker<'a> {
    /// From the track's first sample at or past `floor`, only its sync samples when `keys`.
    pub(crate) fn new(table: &'a TrackTable, track: usize, keys: bool, floor: u64) -> Self {
        let mut head = table.first();
        table.to_byte(&mut head, floor);
        let rising = table.rising().filter(|_| !keys && !head.done);
        let seek = match rising {
            Some((from, end)) => Seek::Rising { head, next: from.max(head.at.first + head.k), end },
            None if keys => Seek::Keys(Keys::new(table, track, floor).peekable()),
            None => Seek::Every(TrackIter { table, track: track as u16, head }.peekable()),
        };
        Seeker { table, track: track as u16, seek, first: None, best: None }
    }

    /// Takes the samples before the first whose presentation time is `past`. A sync sample
    /// `skip` names is no landing.
    pub(crate) fn advance(&mut self, past: &dyn Fn(i64) -> bool, skip: &dyn Fn(SampleId) -> bool) {
        let (table, track) = (self.table, self.track);
        match &mut self.seek {
            Seek::Keys(keys) => {
                while let Some(&(id, s)) = keys.peek() {
                    if !skip(id) {
                        self.first.get_or_insert((id, s));
                        if past(s.pts()) {
                            break;
                        }
                        self.best = Some((id, s));
                    }
                    keys.next();
                }
            }
            Seek::Every(every) => {
                while let Some(&(id, s)) = every.peek() {
                    self.first.get_or_insert((id, s));
                    if past(s.pts()) {
                        break;
                    }
                    self.best = Some((id, s));
                    every.next();
                }
            }
            Seek::Rising { head, next, end } => {
                let left = end.saturating_sub(*next);
                let mut at = |n: u32| {
                    table.to_sample(head, n);
                    let s = table.resolve(track, head)?;
                    Some((SampleId::new(track, head.offset, n), s))
                };
                if self.first.is_none() && left > 0 {
                    self.first = at(*next);
                }
                let taken = bisect(left as usize, |j| !past(table.pts_of(*next + j as u32))) as u32;
                if taken > 0 {
                    *next += taken;
                    self.best = at(*next - 1);
                }
            }
        }
    }
}

/// A track's place in the walk of the file and its next sample there.
#[derive(Clone, Copy, Default)]
struct Front {
    head: Head,
    next: Option<(SampleId, Sample)>,
}

/// The walk of every track's samples in file order, a merge of the tracks by offset. The
/// track that leads is kept while its samples stay below every other track's next.
#[derive(Clone, Default)]
pub(crate) struct Cursor {
    fronts: Vec<Front>,
    lead: usize,
    /// The least next sample of the tracks that do not lead.
    bound: Option<SampleId>,
}

impl Cursor {
    /// At the first sample of the file.
    // Cold path, once per reader and index.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new(tables: &[TrackTable]) -> Cursor {
        let front = |(track, table): (usize, &TrackTable)| {
            let mut head = table.first();
            Front { next: table.kept(track as u16, &mut head), head }
        };
        let mut cursor =
            Cursor { fronts: tables.iter().enumerate().map(front).collect(), lead: 0, bound: None };
        cursor.elect();
        cursor
    }

    fn elect(&mut self) {
        let id = |f: &Front| f.next.map(|next| next.0);
        let ids = || self.fronts.iter().enumerate().filter_map(|(t, f)| Some((id(f)?, t)));
        self.lead = ids().min().map_or(0, |least| least.1);
        self.bound = ids().filter(|&(_, t)| t != self.lead).map(|(id, _)| id).min();
    }

    /// The next sample of the file.
    pub(crate) fn peek(&self) -> Option<(SampleId, Sample)> {
        self.fronts.get(self.lead)?.next
    }

    /// The next sample of `track`.
    pub(crate) fn front(&self, track: usize) -> Option<(SampleId, Sample)> {
        self.fronts.get(track)?.next
    }

    /// One track's samples from its next on.
    pub(crate) fn rest<'a>(&self, tables: &'a [TrackTable], track: usize) -> Option<TrackIter<'a>> {
        let (table, front) = (tables.get(track)?, self.fronts.get(track)?);
        Some(TrackIter { table, track: track as u16, head: front.head })
    }

    /// Past the next sample of the file.
    pub(crate) fn advance(&mut self, tables: &[TrackTable]) {
        let (Some(front), Some(table)) = (self.fronts.get_mut(self.lead), tables.get(self.lead))
        else {
            return;
        };
        table.step(&mut front.head);
        front.next = table.kept(self.lead as u16, &mut front.head);
        let leads = match (front.next, self.bound) {
            (Some((id, _)), Some(bound)) => id < bound,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if !leads {
            self.elect();
        }
    }

    /// At the first sample of the file at or past `byte`.
    pub(crate) fn seek_byte(&mut self, tables: &[TrackTable], byte: u64) {
        for (track, (front, table)) in self.fronts.iter_mut().zip(tables).enumerate() {
            table.to_byte(&mut front.head, byte);
            front.next = table.kept(track as u16, &mut front.head);
        }
        self.elect();
    }

    /// At sample `id`, each other track at its first behind it in file order.
    pub(crate) fn seek_id(&mut self, tables: &[TrackTable], id: SampleId) {
        for (track, (front, table)) in self.fronts.iter_mut().zip(tables).enumerate() {
            match track.cmp(&id.track()) {
                std::cmp::Ordering::Equal => table.to_sample(&mut front.head, id.n),
                // a track before it at the same offset is before it
                std::cmp::Ordering::Less => {
                    table.to_byte(&mut front.head, id.offset().saturating_add(1))
                }
                std::cmp::Ordering::Greater => table.to_byte(&mut front.head, id.offset()),
            }
            front.next = table.kept(track as u16, &mut front.head);
        }
        self.elect();
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::boxes::ElstEntry;
    use crate::flat;
    use crate::reader::{Limits, Tables, flat_of, fold_tables};

    #[test]
    fn a_hint_finds_what_a_search_finds() {
        let keys = [0u32, 4, 4, 9, 20, 21, 50];
        for at in 0..60 {
            let want = keys.iter().filter(|&&k| k <= at).count();
            for start in 0..=keys.len() as u32 + 2 {
                let mut hint = start;
                assert_eq!(upto(keys.len(), |i| keys[i], at, &mut hint), want, "{at} from {start}");
                assert_eq!(hint as usize, want);
            }
        }
        let mut hint = 3;
        assert_eq!(upto(0, |_| 0, 7, &mut hint), 0);
        assert_eq!(bisect(0, |_| true), 0);
        assert_eq!(bisect(5, |i| i < 3), 3);
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }

        fn one_in(&mut self, n: u64) -> bool {
            self.below(n) == 0
        }
    }

    fn be32(words: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        words.iter().for_each(|w| b.extend_from_slice(&w.to_be_bytes()));
        b
    }

    /// The box bodies of one generated track.
    struct Boxes {
        count: usize,
        stts: Vec<u8>,
        sizes: Vec<u8>,
        stsc: Vec<u8>,
        offsets: Vec<u8>,
        co64: bool,
        pcm_frame: u32,
        ctts: Option<Vec<u8>>,
        stss: Option<Vec<u8>>,
        edits: Option<EditList>,
        kind: TrackKind,
    }

    impl Boxes {
        fn stbl(&self) -> Stbl<'_> {
            Stbl {
                count: self.count,
                stts: &self.stts,
                sizes: &self.sizes,
                stz2: false,
                stsc: &self.stsc,
                offsets: &self.offsets,
                co64: self.co64,
                pcm_frame: self.pcm_frame,
                ctts: self.ctts.as_deref(),
                stss: self.stss.as_deref(),
                edits: self.edits.clone(),
                kind: self.kind,
            }
        }
    }

    /// What a generated track is made of before its chunks are placed.
    struct Plan {
        kind: TrackKind,
        /// The entries each chunk holds.
        per: Vec<u32>,
        /// A size per entry, or one for all.
        sizes: Result<Vec<u32>, u32>,
        pcm_frame: u32,
        /// The order its chunks lie in the file.
        laid: Vec<usize>,
    }

    impl Plan {
        fn bytes(&self, chunk: usize) -> u64 {
            let first: usize = self.per[..chunk].iter().map(|&n| n as usize).sum();
            match &self.sizes {
                Ok(sizes) => {
                    sizes[first..first + self.per[chunk] as usize].iter().map(|&s| s as u64).sum()
                }
                Err(size) => {
                    u64::from(*size) * u64::from(self.per[chunk]) * u64::from(self.pcm_frame.max(1))
                }
            }
        }
    }

    fn plan(rng: &mut Rng) -> Plan {
        let kind = [TrackKind::Video, TrackKind::Audio, TrackKind::Other][rng.below(3) as usize];
        let pcm = kind == TrackKind::Audio && rng.one_in(4);
        let chunks = 1 + rng.below(30) as usize;
        let mut each = 1 + rng.below(6) as u32;
        let per: Vec<u32> = (0..chunks)
            .map(|_| {
                if rng.one_in(3) {
                    each = rng.below(7) as u32;
                }
                if pcm { each * (1 + rng.below(900) as u32) } else { each }
            })
            .collect();
        let entries: usize = per.iter().map(|&n| n as usize).sum();
        let sizes = match (pcm, rng.one_in(5)) {
            (true, _) => Err(1),
            (false, true) => Err(1 + rng.below(9) as u32),
            (false, false) => Ok((0..entries)
                .map(|_| match rng.below(60) {
                    0 => 70_000 + rng.below(9) as u32,
                    1..=6 => 0,
                    _ => 1 + rng.below(400) as u32,
                })
                .collect()),
        };
        let mut laid: Vec<usize> = (0..chunks).collect();
        if rng.one_in(4) {
            for i in (1..chunks).rev() {
                laid.swap(i, rng.below(i as u64 + 1) as usize);
            }
        }
        Plan { kind, per, sizes, pcm_frame: if pcm { 1 + rng.below(6) as u32 } else { 0 }, laid }
    }

    /// The edit list of a track of `ticks` media ticks, sometimes one that shows nothing.
    fn edits(rng: &mut Rng, ticks: u64) -> Option<EditList> {
        let mut entries = Vec::new();
        if rng.one_in(3) {
            let segment_duration = 1 + rng.below(40);
            entries.push(ElstEntry { segment_duration, media_time: -1, media_rate: 0x1_0000 });
        }
        let mut media = if rng.one_in(8) { ticks + 50 } else { rng.below(ticks / 2 + 1) };
        for _ in 0..1 + rng.below(3) {
            let segment_duration = if rng.one_in(6) { 0 } else { 1 + rng.below(ticks / 2 + 5) };
            let media_rate = if rng.one_in(12) { 0x2_0000 } else { 0x1_0000 };
            entries.push(ElstEntry { segment_duration, media_time: media as i64, media_rate });
            media += segment_duration + rng.below(30);
        }
        edit::follow(&entries, 1, 1)
    }

    /// A movie of up to four tracks, their chunks dealt into the file a byte or more apart,
    /// and the file's length when it is cut. With `collide` one chunk is moved onto another.
    fn movie(rng: &mut Rng, collide: bool) -> (Vec<Boxes>, Option<u64>) {
        let plans: Vec<Plan> = (0..1 + rng.below(4)).map(|_| plan(rng)).collect();
        let mut next = vec![0usize; plans.len()];
        let mut placed: Vec<Vec<u64>> = plans.iter().map(|p| vec![0; p.per.len()]).collect();
        let mut at = if rng.one_in(6) { 1 << 33 } else { 40 + rng.below(100) };
        while let Some(t) = {
            let open: Vec<usize> =
                (0..plans.len()).filter(|&t| next[t] < plans[t].laid.len()).collect();
            (!open.is_empty()).then(|| open[rng.below(open.len() as u64) as usize])
        } {
            let chunk = plans[t].laid[next[t]];
            next[t] += 1;
            placed[t][chunk] = at;
            at += plans[t].bytes(chunk) + 1 + rng.below(9);
        }
        if collide {
            let pick = |rng: &mut Rng| {
                let t = rng.below(placed.len() as u64) as usize;
                (t, rng.below(placed[t].len() as u64) as usize)
            };
            let ((t, chunk), (onto, other)) = (pick(rng), pick(rng));
            placed[t][chunk] = placed[onto][other] + rng.below(3);
        }
        let boxes = plans
            .iter()
            .zip(&placed)
            .map(|(p, offsets)| {
                let entries: usize = p.per.iter().map(|&n| n as usize).sum();
                let mut stts = Vec::new();
                let (mut timed, mut ticks) = (0u64, 0u64);
                let want =
                    if rng.one_in(25) { entries as u64 / 2 } else { entries as u64 + rng.below(4) };
                while timed < want {
                    let (count, delta) = (1 + rng.below(50), 1 + rng.below(40));
                    stts.extend([count as u32, delta as u32]);
                    (timed, ticks) = (timed + count, ticks + count * delta);
                }
                let sizes = match &p.sizes {
                    Ok(sizes) => be32(&[&[0, sizes.len() as u32][..], sizes].concat()),
                    Err(size) => be32(&[*size, entries as u32]),
                };
                let mut stsc: Vec<u32> = Vec::new();
                let skip = p.per.iter().take_while(|&&n| n == 0).count();
                let from = if rng.one_in(2) { skip.min(p.per.len() - 1) } else { 0 };
                for (c, &n) in p.per.iter().enumerate().skip(from) {
                    if c == from || p.per[c - 1] != n || rng.one_in(9) {
                        stsc.extend([c as u32 + 1, n, 1]);
                    }
                }
                let co64 = offsets.iter().any(|&o| o > u64::from(u32::MAX)) || rng.one_in(5);
                let mut chunk_offsets = be32(&[offsets.len() as u32]);
                for &o in offsets {
                    match co64 {
                        true => chunk_offsets.extend_from_slice(&o.to_be_bytes()),
                        false => chunk_offsets.extend_from_slice(&(o as u32).to_be_bytes()),
                    }
                }
                let video = p.kind == TrackKind::Video;
                let ctts = (video && !rng.one_in(3)).then(|| {
                    let (mut runs, mut covered) = (Vec::new(), 0u64);
                    let want = if rng.one_in(4) { entries as u64 / 2 } else { entries as u64 };
                    while covered < want {
                        let count = 1 + rng.below(3);
                        runs.extend([count as u32, (rng.below(80) as i32 - 20) as u32]);
                        covered += count;
                    }
                    be32(&[&[(runs.len() / 2) as u32][..], &runs].concat())
                });
                let stss = (video && !rng.one_in(4)).then(|| {
                    let keys: Vec<u32> =
                        (1..=entries as u32).filter(|&n| n == 1 || rng.one_in(5)).collect();
                    be32(&[&[keys.len() as u32][..], &keys].concat())
                });
                let count = match p.pcm_frame {
                    0 => entries,
                    _ => entries.min(entries.div_ceil(PCM_GROUP as usize) + p.per.len()),
                };
                Boxes {
                    count,
                    stts: be32(&[&[(stts.len() / 2) as u32][..], &stts].concat()),
                    sizes,
                    stsc: be32(&[&[(stsc.len() / 3) as u32][..], &stsc].concat()),
                    offsets: chunk_offsets,
                    co64,
                    pcm_frame: p.pcm_frame,
                    ctts,
                    stss,
                    edits: if rng.one_in(2) { edits(rng, ticks) } else { None },
                    kind: p.kind,
                }
            })
            .collect();
        let cut = rng.one_in(3).then(|| at.saturating_sub(rng.below(at.min(4_000) + 1)));
        (boxes, cut)
    }

    fn walked(stbl: &[TrackTable], cursor: &mut Cursor) -> Vec<(SampleId, Sample)> {
        let mut all = Vec::new();
        while let Some(next) = cursor.peek() {
            all.push(next);
            cursor.advance(stbl);
        }
        all
    }

    /// The tables hold what the flat index held, in its order, from any place. Of a movie
    /// with a chunk moved onto another, `collide`, only that both refuse it or hold the same.
    fn same(
        stbls: &[Option<Stbl<'_>>],
        limits: Limits,
        rng: &mut Rng,
        (seed, collide): (u64, bool),
    ) -> usize {
        let (flat, refused) = match (flat::fold(stbls, limits), fold_tables(stbls, limits)) {
            (Err(want), Err(got)) => {
                assert_eq!(std::mem::discriminant(&want), std::mem::discriminant(&got), "{seed}");
                return 0;
            }
            (Ok(_), Err(e)) => panic!("seed {seed}, only the tables refuse, {e}"),
            (Err(e), Ok(_)) => panic!("seed {seed}, only the flat index refuses, {e}"),
            (Ok(flat), Ok((stbl, count, unpresented))) => {
                let refused: Vec<usize> = flat.1.iter().map(|r| r.0).collect();
                assert_eq!(unpresented, refused, "seed {seed}");
                assert_eq!(count, flat.0.len(), "seed {seed}");
                (flat.0, stbl)
            }
        };
        let stbl = refused;
        let mut cursor = Cursor::new(&stbl);
        let all = walked(&stbl, &mut cursor);
        let samples: Vec<Sample> = all.iter().map(|a| a.1).collect();
        // Samples of no bytes at one offset went by their decode times, which an edit list
        // may turn. The tables keep the order they are stored in.
        let mut by_time = samples.clone();
        by_time.sort_by_key(|s| (s.place, s.dts));
        assert!(by_time == flat, "seed {seed}\n got {samples:?}\nwant {flat:?}");
        let flat = samples;
        if collide {
            return flat.len();
        }
        assert!(all.is_sorted_by_key(|a| a.0), "seed {seed}, the ids rise");
        let Some(last) = flat.last() else { return 0 };
        for _ in 0..6 {
            let byte = flat[0].offset().saturating_sub(3)
                + rng.below(last.offset() - flat[0].offset() + 9);
            let from = flat.partition_point(|s| s.offset() < byte);
            cursor.seek_byte(&stbl, byte);
            let rest: Vec<Sample> = walked(&stbl, &mut cursor).iter().map(|a| a.1).collect();
            assert!(rest == flat[from..], "seed {seed}, from byte {byte}");
            let at = rng.below(all.len() as u64) as usize;
            cursor.seek_id(&stbl, all[at].0);
            // a few on and back again, as a plan that asked too far
            (0..rng.below(6)).for_each(|_| cursor.advance(&stbl));
            cursor.seek_id(&stbl, all[at].0);
            assert!(walked(&stbl, &mut cursor) == all[at..], "seed {seed}, from sample {at}");
        }
        for (t, table) in stbl.iter().enumerate() {
            let own: Vec<(SampleId, Sample)> =
                all.iter().copied().filter(|a| a.1.track() == t).collect();
            assert!(TrackIter::new(table, t).collect::<Vec<_>>() == own, "seed {seed}, track {t}");
            let Some(&(id, s)) = own.get(rng.below(own.len() as u64 + 1) as usize) else {
                continue;
            };
            let from = own.iter().position(|a| a.0 == id).unwrap_or(0);
            let rest: Vec<_> = TrackIter::new(table, t).at_sample(id.number()).collect();
            assert!(rest == own[from..], "seed {seed}, track {t} from sample {}", id.number());
            let from = own.partition_point(|a| a.1.offset() < s.offset());
            let rest: Vec<_> = TrackIter::new(table, t).at_byte(s.offset()).collect();
            assert!(rest == own[from..], "seed {seed}, track {t} from byte {}", s.offset());
        }
        for (t, table) in stbl.iter().enumerate() {
            seeks(table, t, &all, rng, seed);
        }
        flat.len()
    }

    /// The sync samples, the sizes and the landings of a seek come out of the lists and the
    /// searches as they come out of the walk of every sample.
    fn seeks(table: &TrackTable, t: usize, all: &[(SampleId, Sample)], rng: &mut Rng, seed: u64) {
        let own: Vec<(SampleId, Sample)> =
            all.iter().copied().filter(|a| a.1.track() == t).collect();
        let floor = match own.get(rng.below(own.len() as u64 + 1) as usize) {
            Some((_, s)) if rng.one_in(2) => s.offset() + rng.below(3),
            _ => 0,
        };
        let from = own.partition_point(|a| a.1.offset() < floor);
        let keys: Vec<_> = own[from..].iter().copied().filter(|a| a.1.sync()).collect();
        assert!(
            Keys::new(table, t, floor).collect::<Vec<_>>() == keys,
            "seed {seed}, track {t} keys"
        );
        let stride = 1 + rng.below(5) as usize;
        let thinned: Vec<_> = keys.iter().copied().step_by(stride).collect();
        let stepped: Vec<_> = Keys::new(table, t, floor).step_by(stride).collect();
        assert!(stepped == thinned, "seed {seed}, track {t} every {stride}th key");
        if let Some(n) = Keys::new(table, t, floor).len() {
            assert_eq!(n, keys.len(), "seed {seed}, track {t}");
        }
        if let Some(sizes) = table.sizes() {
            let want: Vec<u32> = own.iter().map(|a| a.1.size()).collect();
            assert!(sizes.collect::<Vec<_>>() == want, "seed {seed}, track {t} sizes");
        }
        let skip = |id: SampleId| id.number() % 7 == 3;
        let (low, high) = own.iter().fold((0, 0), |(l, h), a| (l.min(a.1.pts()), h.max(a.1.pts())));
        let mut bounds: Vec<i64> =
            (0..5).map(|_| low - 2 + rng.below((high - low) as u64 + 5) as i64).collect();
        bounds.sort_unstable();
        for only_keys in [false, true] {
            let mut seeker = Seeker::new(table, t, only_keys, floor);
            let mut rest = own[from..].iter().copied().peekable();
            let (mut first, mut best) = (None, None);
            for &bound in &bounds {
                while let Some(&(id, s)) = rest.peek() {
                    if only_keys && (!s.sync() || skip(id)) {
                        rest.next();
                        continue;
                    }
                    first.get_or_insert((id, s));
                    if s.pts() > bound {
                        break;
                    }
                    best = Some((id, s));
                    rest.next();
                }
                seeker.advance(&|pts| pts > bound, &skip);
                let got = (seeker.first, seeker.best);
                assert!(
                    got == (first, best),
                    "seed {seed}, track {t}, keys {only_keys}, to {bound}"
                );
            }
        }
    }

    #[test]
    fn generated_tables_hold_what_the_flat_index_held() {
        let (mut total, mut refused) = (0usize, 0usize);
        let (mut rising, mut listed) = (0usize, 0usize);
        for seed in 1..=1_500u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let collide = seed % 6 == 0;
            let (boxes, file_len) = movie(&mut rng, collide);
            let stbls: Vec<Option<Stbl<'_>>> =
                boxes.iter().map(|b| (!rng.one_in(12)).then(|| b.stbl())).collect();
            let limits = Limits { file_len, ..Limits::default() };
            let n = same(&stbls, limits, &mut rng, (seed, collide));
            (total, refused) = (total + n, refused + usize::from(n == 0));
            if let Some((stbl, ..)) = fold_tables(&stbls, limits).ok().filter(|_| n > 0) {
                rising += stbl.iter().filter(|t| t.rising().is_some() && t.total > 0).count();
                listed += stbl.iter().filter(|t| t.sync.is_some() && t.by_offset.is_none()).count();
            }
        }
        assert!(total > 100_000 && refused < 800, "{total} samples, {refused} movies without");
        assert!(
            rising > 200 && listed > 200,
            "{rising} tracks searched, {listed} with listed keys"
        );
    }

    /// A plan that asked one sample too far walks back by it. In a file of two tracks stored
    /// one behind the other in a chunk each, that is a step from where each track stands and
    /// no read of a chunk from its start.
    #[test]
    fn a_step_back_in_a_huge_chunk_is_a_step() {
        const N: u32 = 120_000;
        let sizes: Vec<u32> = (0..N).map(|n| 1 + n % 7).collect();
        let bytes: u64 = sizes.iter().map(|&s| u64::from(s)).sum();
        let track = |at: u64| Boxes {
            count: N as usize,
            stts: be32(&[1, N, 1]),
            sizes: be32(&[&[0, N][..], &sizes].concat()),
            stsc: be32(&[1, 1, N, 1]),
            offsets: be32(&[1, at as u32]),
            co64: false,
            pcm_frame: 0,
            ctts: None,
            stss: None,
            edits: None,
            kind: TrackKind::Audio,
        };
        let boxes = [track(100), track(100 + bytes)];
        let stbls: Vec<Option<Stbl<'_>>> = boxes.iter().map(|b| Some(b.stbl())).collect();
        let (stbl, count, _) = fold_tables(&stbls, Limits::default()).expect("two tracks");
        assert_eq!(count, 2 * N as usize);
        let start = std::time::Instant::now();
        let mut cursor = Cursor::new(&stbl);
        let (mut seen, mut last) = (0usize, None);
        while let Some((id, _)) = cursor.peek() {
            cursor.advance(&stbl);
            // one too far, then back to it
            cursor.advance(&stbl);
            cursor.seek_id(&stbl, id);
            assert_eq!(cursor.peek().map(|next| next.0), Some(id));
            cursor.advance(&stbl);
            assert!(last < Some(id));
            (seen, last) = (seen + 1, Some(id));
        }
        assert_eq!(seen, count);
        // a chunk read again per step is ten thousand million sizes summed
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "{:?}", start.elapsed());
    }

    /// Every MP4 fixture of the repository, whole and cut at half its length.
    #[test]
    fn every_fixtures_tables_hold_what_the_flat_index_held() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let dirs = [
            "plugins/formats/mp4/tests/fixtures",
            "plugins/formats/mp4/tests/fixtures/frag",
            "lib/open/tests/fixtures",
            "plugins/video/hdr/tests/fixtures",
        ];
        let (mut files, mut samples) = (0, 0);
        for dir in dirs {
            let mut paths: Vec<_> = std::fs::read_dir(root.join(dir))
                .unwrap_or_else(|e| panic!("{dir}: {e}"))
                .map(|e| e.expect("an entry").path())
                .filter(|p| {
                    let ext = p.extension().and_then(|e| e.to_str());
                    matches!(ext, Some("mp4" | "m4a" | "mov" | "m4s"))
                })
                .collect();
            paths.sort();
            for path in paths {
                let file = std::fs::read(&path).expect("a fixture");
                let name = path.display();
                for file_len in [None, Some(file.len() as u64 / 2)] {
                    let limits = Limits { file_len, ..Limits::default() };
                    let want = flat_of(&file, limits);
                    let got = Tables::resolve(&file, &[], limits);
                    match (want, got) {
                        (Ok(want), Ok(got)) => {
                            assert_eq!(got.sample_count(), want.len(), "{name}");
                            assert!(got.samples().eq(want.iter().copied()), "{name}");
                            samples += want.len();
                        }
                        (Err(_), Err(_)) => {}
                        (want, got) => {
                            panic!("{name}, flat {:?}, tables {:?}", want.err(), got.err())
                        }
                    }
                }
                files += 1;
            }
        }
        assert!(files >= 15 && samples > 500, "{files} files, {samples} samples");
    }
}
