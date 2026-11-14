//! The flat sample index the reader held before its tables kept the file's shape, a struct
//! per sample in file order. Test only, the oracle the tables are compared against.

// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use std::ops::{Deref, DerefMut, Range};

use stein_arena::Grow;

use crate::boxes;
use crate::edit::{self, EditRefusal};
use crate::reader::{Limits, MAX_SAMPLES, Mp4Error, PCM_GROUP, Room, Sample, Stbl};

/// The samples in file order, and the tracks whose edit list presented nothing.
pub(crate) type Folded = (Vec<Sample>, Vec<(usize, EditRefusal)>);

/// The samples of `stbls` in file order, and the tracks whose edit list presented nothing.
pub(crate) fn fold(stbls: &[Option<Stbl<'_>>], limits: Limits) -> Result<Folded, Mp4Error> {
    let limit = limits.samples.min(MAX_SAMPLES);
    let total = stbls
        .iter()
        .flatten()
        .try_fold(0usize, |n, s| n.checked_add(s.count).filter(|&n| n <= limit))
        .ok_or(Mp4Error::TooManySamples { limit })?;
    let mut samples = SampleIndex::reserve(total)?;
    samples.room = Room::of(limits.file_len);
    // The largest run folds last, the merge keeps it in place.
    let mut order: Vec<usize> = (0..stbls.len()).collect();
    order.sort_by_key(|&i| stbls[i].as_ref().map_or(0, |s| s.count));
    let (mut runs, mut refused) = (Vec::with_capacity(order.len()), Vec::new());
    for track_index in order {
        let Some(s) = &stbls[track_index] else { continue };
        let from = samples.len();
        if !fold_track(track_index as u16, s, &mut samples)? {
            refused.push((track_index, EditRefusal::NothingPresented));
        }
        runs.push(from..samples.len());
    }
    merge_runs(&mut samples, &runs);
    let mut covered = 0u64;
    for s in samples.iter().filter(|s| s.size() != 0) {
        if s.offset() < covered {
            return Err(Mp4Error::Inconsistent("samples overlap in the file"));
        }
        covered = s.offset().saturating_add(s.size() as u64);
    }
    Ok((samples.to_vec(), refused))
}

/// One reservation of exactly the declared sample count, written once and never moved. The
/// mapping commits as it is written, `peak` is the most ever written.
#[derive(Default)]
struct SampleIndex {
    grow: Option<Grow<Sample>>,
    peak: usize,
    room: Room,
}

impl SampleIndex {
    fn reserve(n: usize) -> Result<Self, Mp4Error> {
        if n == 0 {
            return Ok(Self::default());
        }
        let grow = Grow::reserve(n).map_err(Mp4Error::Index)?;
        Ok(Self { grow: Some(grow), ..Self::default() })
    }

    fn push(&mut self, s: Sample) -> Result<(), Mp4Error> {
        match &mut self.grow {
            Some(g) => g.push(s).map_err(Mp4Error::Index),
            None => Err(Mp4Error::Inconsistent("more samples than the tables declare")),
        }
    }

    fn truncate(&mut self, n: usize) {
        if let Some(g) = &mut self.grow {
            self.peak = self.peak.max(g.len());
            g.truncate(n);
        }
    }

    fn committed(&self) -> usize {
        self.peak.max(self.len()) * size_of::<Sample>()
    }
}

impl Deref for SampleIndex {
    type Target = [Sample];
    fn deref(&self) -> &[Sample] {
        self.grow.as_deref().unwrap_or(&[])
    }
}

impl DerefMut for SampleIndex {
    fn deref_mut(&mut self) -> &mut [Sample] {
        self.grow.as_deref_mut().unwrap_or(&mut [])
    }
}

/// Past this many runs the index is sorted whole, the merge scans every run's head per stretch.
const MERGE_RUNS: usize = 16;

fn order_key(s: &Sample) -> (u64, i64) {
    (s.place, s.dts)
}

/// [`order_key`] of the `n`th sample of a sorted run in one integer, the place in the run for
/// the DTS, so the merge picks a head without a branch.
fn packed_key(s: &Sample, n: usize) -> u128 {
    u128::from(s.place) << 64 | n as u128
}

/// Above every [`packed_key`], a run holds at most [`MAX_SAMPLES`].
const DONE: u128 = u128::MAX;

/// `samples` into file order from `runs`, the contiguous per track ranges that cover it in
/// fold order. A track's run is in order unless its chunk offsets fall back. The runs but the
/// last go to scratch and merge in front of the last one's unread samples, which never get
/// overwritten, so the scratch is the size of the others.
// Cold path, once per movie.
#[allow(clippy::disallowed_methods)]
pub(crate) fn merge_runs(samples: &mut [Sample], runs: &[Range<usize>]) {
    if runs.len() > MERGE_RUNS {
        // Unstable, a stable sort allocates half the index again as scratch.
        samples.sort_unstable_by_key(order_key);
        return;
    }
    for r in runs {
        let run = &mut samples[r.clone()];
        if !run.is_sorted_by_key(order_key) {
            run.sort_unstable_by_key(order_key);
        }
    }
    let Some((last, rest)) = runs.split_last() else { return };
    if rest.is_empty() {
        return;
    }
    let scratch = samples[..last.start].to_vec();
    let (k, tail) = (runs.len(), rest.len());
    let (mut at, mut start, mut end) = ([0; MERGE_RUNS], [0; MERGE_RUNS], [0; MERGE_RUNS]);
    // each run's head, cached so a stretch reads only its own run
    let mut head = [DONE; MERGE_RUNS];
    for (i, r) in runs.iter().enumerate().filter(|(_, r)| !r.is_empty()) {
        (at[i], start[i], end[i]) = (r.start, r.start, r.end);
        head[i] = packed_key(if i == tail { &samples[r.start] } else { &scratch[r.start] }, 0);
    }
    let mut w = 0;
    loop {
        let m = (0..k).fold(0, |m, i| if head[i] < head[m] { i } else { m });
        let bound = (0..k).filter(|&i| i != m).fold(DONE, |b, i| b.min(head[i]));
        if head[m] == DONE || (m == tail && bound == DONE) {
            // what is left of the last run is in place
            break;
        }
        // the leading run's stretch below every other head, copied forward. The write trails
        // the last run's next unread sample by the scratch left unread, so never overwrites it.
        let mut i = at[m];
        head[m] = loop {
            samples[w] = if m == tail { samples[i] } else { scratch[i] };
            (w, i) = (w + 1, i + 1);
            if i == end[m] {
                break DONE;
            }
            let next = if m == tail { &samples[i] } else { &scratch[i] };
            let next = packed_key(next, i - start[m]);
            if next >= bound {
                break next;
            }
        };
        at[m] = i;
    }
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
fn fold_track(track_index: u16, s: &Stbl<'_>, out: &mut SampleIndex) -> Result<bool, Mp4Error> {
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
        boxes::SampleSizes::Constant { sample_size: 1, .. } => s.pcm_frame,
        boxes::SampleSizes::Constant { sample_size, .. } if sample_size == s.pcm_frame => {
            sample_size
        }
        _ => 0,
    };
    let from = out.len();
    build_samples(
        track_index,
        sizes.count(),
        &sizes,
        &stts,
        &ctts,
        &stsc,
        &chunk_offsets,
        stss.as_deref(),
        0,
        group,
        out,
    )?;
    let Some(list) = s.edits.as_ref().filter(|_| out.len() > from) else {
        return Ok(true);
    };
    match edit::apply(&mut out[from..], s.kind, list) {
        Some(kept) => {
            out.truncate(from + kept);
            Ok(true)
        }
        None => {
            let shift = list.first_media();
            for sample in &mut out[from..] {
                // the composition offset stays, so the presentation moves as far
                sample.dts = sample.dts.saturating_sub(shift);
            }
            Ok(false)
        }
    }
}

/// Fold the `stbl` tables onto `out` (8.6.1.2, 8.6.1.3, 8.7.3, 8.7.4, 8.7.5).
// Cold path, once per track.
#[allow(clippy::too_many_arguments, clippy::disallowed_methods)]
fn build_samples(
    track_index: u16,
    sample_count: usize,
    sizes: &boxes::SampleSizes,
    stts: &[boxes::SttsEntry],
    ctts: &[boxes::CttsEntry],
    stsc: &[boxes::StscEntry],
    chunk_offsets: &[u64],
    stss: Option<&[u32]>,
    media_time_shift: i64,
    group: u32,
    out: &mut SampleIndex,
) -> Result<(), Mp4Error> {
    if sample_count == 0 {
        return Ok(());
    }
    if group == 0 && sample_count > MAX_SAMPLES {
        return Err(Mp4Error::TooManySamples { limit: MAX_SAMPLES });
    }
    let chunk_count = chunk_offsets.len();
    if chunk_count == 0 {
        return Err(Mp4Error::Inconsistent("no chunk offsets"));
    }
    let mut samples_per_chunk = vec![0u32; chunk_count];
    if stsc.is_empty() {
        return Err(Mp4Error::Inconsistent("empty stsc"));
    }
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
            Some(next) => {
                if next.first_chunk <= run.first_chunk {
                    return Err(Mp4Error::Inconsistent("stsc first_chunk not increasing"));
                }
                ((next.first_chunk - 1) as usize).min(chunk_count)
            }
            None => chunk_count,
        };
        for spc in samples_per_chunk.iter_mut().take(end).skip(start) {
            *spc = run.samples_per_chunk;
        }
    }

    // The chunk map must describe exactly sample_count samples, else reject.
    let mapped: u64 = samples_per_chunk.iter().map(|&n| n as u64).sum();
    if mapped != sample_count as u64 {
        return Err(Mp4Error::Inconsistent("stsc/stco sample total != stsz sample count"));
    }

    let mut stts_iter = RunLengthTimes::new(stts);
    let mut ctts_iter = RunLengthOffsets::new(ctts);

    let sync_lookup = SyncLookup::new(stss);

    let mut sample_no = 0usize; // 0-based across the whole track
    let mut dts_raw: u64 = 0;

    for (chunk_idx, &chunk_off) in chunk_offsets.iter().enumerate() {
        let spc = samples_per_chunk[chunk_idx];
        let mut offset = chunk_off;
        if group != 0 {
            let mut left = spc;
            while left > 0 {
                let n = left.min(PCM_GROUP);
                let size = n
                    .checked_mul(group)
                    .ok_or(Mp4Error::Inconsistent("pcm group size overflow"))?;
                if !out.room.admits(offset, size)? {
                    // Offsets rise through a chunk, so the rest of it is past the end too.
                    dts_raw = skip_past_end(&mut stts_iter, &mut ctts_iter, left, dts_raw)?;
                    sample_no += left as usize;
                    break;
                }
                // Summed run by run, a track of billions of frames costs its groups.
                let span = stts_iter
                    .take(n)
                    .ok_or(Mp4Error::Inconsistent("stts describes fewer samples than stsz"))?;
                let dts = (dts_raw as i128 - media_time_shift as i128)
                    .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                out.push(Sample::new(track_index, offset, size, dts, dts, true)?)?;
                dts_raw = dts_raw
                    .checked_add(span)
                    .ok_or(Mp4Error::Inconsistent("cumulative dts overflow"))?;
                offset = offset
                    .checked_add(u64::from(size))
                    .ok_or(Mp4Error::Inconsistent("chunk offset overflow"))?;
                sample_no += n as usize;
                left -= n;
            }
            continue;
        }
        for k in 0..spc {
            if sample_no >= sample_count {
                return Err(Mp4Error::Inconsistent("more chunk samples than sizes"));
            }
            let size = sizes
                .get(sample_no)
                .ok_or(Mp4Error::Inconsistent("sample index past size table"))?;
            if !out.room.admits(offset, size)? {
                // Offsets rise through a chunk, so the rest of it is past the end too.
                dts_raw = skip_past_end(&mut stts_iter, &mut ctts_iter, spc - k, dts_raw)?;
                sample_no += (spc - k) as usize;
                break;
            }
            let delta = stts_iter
                .next_delta()
                .ok_or(Mp4Error::Inconsistent("stts describes fewer samples than stsz"))?;
            let ctts_off = ctts_iter.next_offset();

            // The edit shift comes off both dts and pts (the ffmpeg convention).
            let dts_i = dts_raw as i128 - media_time_shift as i128;
            let pts_i = dts_raw as i128 + ctts_off as i128 - media_time_shift as i128;
            let dts = dts_i.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            let pts = pts_i.clamp(i64::MIN as i128, i64::MAX as i128) as i64;

            let sync = sync_lookup.is_sync(sample_no);

            out.push(Sample::new(track_index, offset, size, dts, pts, sync)?)?;

            dts_raw = dts_raw
                .checked_add(delta as u64)
                .ok_or(Mp4Error::Inconsistent("cumulative dts overflow"))?;
            offset = offset
                .checked_add(size as u64)
                .ok_or(Mp4Error::Inconsistent("chunk offset overflow"))?;
            sample_no += 1;
        }
    }

    if sample_no != sample_count {
        return Err(Mp4Error::Inconsistent("chunk map produced wrong sample count"));
    }
    Ok(())
}

/// Steps the times over `n` samples left out, a run at a time.
fn skip_past_end(
    stts: &mut RunLengthTimes<'_>,
    ctts: &mut RunLengthOffsets<'_>,
    n: u32,
    dts_raw: u64,
) -> Result<u64, Mp4Error> {
    ctts.skip(n);
    stts.take(n)
        .and_then(|span| dts_raw.checked_add(span))
        .ok_or(Mp4Error::Inconsistent("stts describes fewer samples than stsz"))
}

struct RunLengthTimes<'a> {
    runs: &'a [boxes::SttsEntry],
    run: usize,
    left: u32,
}

impl<'a> RunLengthTimes<'a> {
    fn new(runs: &'a [boxes::SttsEntry]) -> Self {
        Self { runs, run: 0, left: runs.first().map(|e| e.count).unwrap_or(0) }
    }

    fn next_delta(&mut self) -> Option<u32> {
        while self.left == 0 {
            self.run += 1;
            self.left = self.runs.get(self.run)?.count;
        }
        self.left -= 1;
        Some(self.runs[self.run].delta)
    }

    /// The sum of the next `n` deltas, consumed a run at a time, `None` short of `n`.
    fn take(&mut self, n: u32) -> Option<u64> {
        let mut want = n;
        let mut sum = 0u64;
        while want > 0 {
            while self.left == 0 {
                self.run += 1;
                self.left = self.runs.get(self.run)?.count;
            }
            let got = self.left.min(want);
            sum = sum.saturating_add(u64::from(got) * u64::from(self.runs[self.run].delta));
            self.left -= got;
            want -= got;
        }
        Some(sum)
    }
}

struct RunLengthOffsets<'a> {
    runs: &'a [boxes::CttsEntry],
    run: usize,
    left: u32,
}

impl<'a> RunLengthOffsets<'a> {
    fn new(runs: &'a [boxes::CttsEntry]) -> Self {
        Self { runs, run: 0, left: runs.first().map(|e| e.count).unwrap_or(0) }
    }

    fn next_offset(&mut self) -> i64 {
        while self.left == 0 {
            self.run += 1;
            match self.runs.get(self.run) {
                Some(e) => self.left = e.count,
                None => return 0, // exhausted, remaining samples have zero offset
            }
        }
        self.left -= 1;
        self.runs[self.run].offset
    }

    fn skip(&mut self, mut n: u32) {
        while n > 0 {
            while self.left == 0 {
                self.run += 1;
                match self.runs.get(self.run) {
                    Some(e) => self.left = e.count,
                    None => return,
                }
            }
            let got = self.left.min(n);
            (self.left, n) = (self.left - got, n - got);
        }
    }
}

struct SyncLookup<'a> {
    list: Option<&'a [u32]>,
}

impl<'a> SyncLookup<'a> {
    fn new(list: Option<&'a [u32]>) -> Self {
        Self { list }
    }

    fn is_sync(&self, i: usize) -> bool {
        match self.list {
            None => true,
            Some(list) => list.binary_search(&((i + 1) as u32)).is_ok(),
        }
    }
}

mod tests {
    use super::*;
    use crate::boxes::{CttsEntry, SampleSizes, StscEntry, SttsEntry};
    use crate::reader::MAX_TRACKS;

    #[test]
    fn run_length_times_walks_deltas() {
        let stts = [SttsEntry { count: 2, delta: 100 }, SttsEntry { count: 1, delta: 40 }];
        let mut it = RunLengthTimes::new(&stts);
        assert_eq!(it.next_delta(), Some(100));
        assert_eq!(it.next_delta(), Some(100));
        assert_eq!(it.next_delta(), Some(40));
        assert_eq!(it.next_delta(), None);
    }

    #[test]
    fn run_length_offsets_exhaust_to_zero() {
        let ctts = [CttsEntry { count: 1, offset: 200 }, CttsEntry { count: 1, offset: -50 }];
        let mut it = RunLengthOffsets::new(&ctts);
        assert_eq!(it.next_offset(), 200);
        assert_eq!(it.next_offset(), -50);
        assert_eq!(it.next_offset(), 0, "past the table → zero");
        let mut it = RunLengthOffsets::new(&[]);
        assert_eq!(it.next_offset(), 0);
    }

    #[test]
    fn sync_lookup_absent_is_all_sync() {
        let s = SyncLookup::new(None);
        assert!(s.is_sync(0) && s.is_sync(7));
        let list = [1u32, 4];
        let s = SyncLookup::new(Some(&list));
        assert!(s.is_sync(0), "sample 1 (0-based 0) is sync");
        assert!(!s.is_sync(1));
        assert!(s.is_sync(3), "sample 4 (0-based 3) is sync");
    }

    #[test]
    fn build_samples_folds_offsets_and_times() {
        let sizes = SampleSizes::Table(vec![10, 20, 30, 40]);
        let stts = [SttsEntry { count: 4, delta: 512 }];
        let ctts = [CttsEntry { count: 4, offset: 0 }];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 2, sample_description_index: 1 }];
        let chunk_offsets = [1000u64, 5000];
        let mut out = SampleIndex::reserve(4).unwrap();
        build_samples(0, 4, &sizes, &stts, &ctts, &stsc, &chunk_offsets, None, 0, 0, &mut out)
            .unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!((out[0].offset(), out[0].size(), out[0].dts, out[0].pts()), (1000, 10, 0, 0));
        assert_eq!(
            (out[1].offset(), out[1].size(), out[1].dts, out[1].pts()),
            (1010, 20, 512, 512)
        );
        assert_eq!((out[2].offset(), out[2].size(), out[2].dts), (5000, 30, 1024));
        assert_eq!((out[3].offset(), out[3].size(), out[3].dts), (5030, 40, 1536));
        assert!(out.iter().all(|s| s.sync()));
    }

    #[test]
    fn build_samples_applies_ctts_and_edit_shift() {
        let sizes = SampleSizes::Constant { sample_size: 5, sample_count: 3 };
        let stts = [SttsEntry { count: 3, delta: 100 }];
        let ctts = [CttsEntry { count: 1, offset: 200 }, CttsEntry { count: 2, offset: 0 }];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 3, sample_description_index: 1 }];
        let chunk_offsets = [100u64];
        let mut out = SampleIndex::reserve(3).unwrap();
        build_samples(0, 3, &sizes, &stts, &ctts, &stsc, &chunk_offsets, None, 200, 0, &mut out)
            .unwrap();
        // dts_raw 0, dts -200, pts 0+200-200 = 0.
        assert_eq!((out[0].dts, out[0].pts()), (-200, 0));
        assert_eq!((out[1].dts, out[1].pts()), (-100, -100));
        assert_eq!((out[2].dts, out[2].pts()), (0, 0));
    }

    #[test]
    fn build_samples_rejects_inconsistent_tables() {
        let sizes = SampleSizes::Constant { sample_size: 5, sample_count: 4 };
        let stts = [SttsEntry { count: 4, delta: 100 }];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 2, sample_description_index: 1 }];
        let chunk_offsets = [100u64];
        let mut out = SampleIndex::reserve(4).unwrap();
        assert!(
            build_samples(0, 4, &sizes, &stts, &[], &stsc, &chunk_offsets, None, 0, 0, &mut out)
                .is_err()
        );
    }

    #[test]
    fn tracks_share_one_exact_index() {
        let stts = [SttsEntry { count: 2, delta: 1 }];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 2, sample_description_index: 1 }];
        let a = SampleSizes::Constant { sample_size: 4, sample_count: 2 };
        let b = SampleSizes::Table(vec![8, 8]);
        let mut out = SampleIndex::reserve(4).unwrap();
        build_samples(0, 2, &a, &stts, &[], &stsc, &[100], None, 0, 0, &mut out).unwrap();
        build_samples(1, 2, &b, &stts, &[], &stsc, &[0], None, 0, 0, &mut out).unwrap();
        out.sort_by_key(|s| s.offset());
        let order: Vec<_> = out.iter().map(|s| (s.track_index(), s.offset())).collect();
        assert_eq!(order, [(1, 0), (1, 8), (0, 100), (0, 104)]);
        let mut none = SampleIndex::reserve(0).unwrap();
        assert!(none.is_empty());
        assert!(build_samples(0, 2, &a, &stts, &[], &stsc, &[100], None, 0, 0, &mut none).is_err());
    }

    #[test]
    fn run_length_times_take_sums_a_run_at_a_time() {
        let stts = [SttsEntry { count: 3, delta: 10 }, SttsEntry { count: 2, delta: 7 }];
        let mut it = RunLengthTimes::new(&stts);
        assert_eq!(it.take(2), Some(20));
        assert_eq!(it.take(2), Some(17), "one of each run");
        assert_eq!(it.take(1), Some(7));
        assert_eq!(it.take(1), None, "short of the table");
        let huge = [SttsEntry { count: u32::MAX, delta: u32::MAX }];
        let mut it = RunLengthTimes::new(&huge);
        assert_eq!(it.take(u32::MAX), Some(u64::from(u32::MAX) * u64::from(u32::MAX)));
        assert_eq!(it.take(1), None);
    }

    #[test]
    // A wall clock bound on the fold, test only.
    #[allow(clippy::disallowed_methods)]
    fn pcm_groups_fold_a_billion_frames_by_their_stts_runs() {
        const FRAMES: u32 = 1 << 30;
        let sizes = SampleSizes::Constant { sample_size: 1, sample_count: FRAMES };
        let stts =
            [SttsEntry { count: FRAMES / 2, delta: 1 }, SttsEntry { count: FRAMES / 2, delta: 3 }];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: FRAMES, sample_description_index: 1 }];
        let groups = (FRAMES / PCM_GROUP) as usize;
        let mut out = SampleIndex::reserve(groups).unwrap();
        let started = std::time::Instant::now();
        build_samples(0, FRAMES as usize, &sizes, &stts, &[], &stsc, &[64], None, 0, 2, &mut out)
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(8), "walked per frame");
        assert_eq!(out.len(), groups);
        assert_eq!((out[0].offset(), out[0].size(), out[0].dts), (64, 2048, 0));
        assert_eq!(out[1].dts, 1024);
        let last = out[groups - 1];
        assert_eq!(last.dts, i64::from(FRAMES / 2) + i64::from(FRAMES / 2 - 1024) * 3);
    }

    #[test]
    fn a_sample_past_the_file_octets_is_an_overlap_refused_as_it_is_reached() {
        let sizes = SampleSizes::Constant { sample_size: 1, sample_count: 64 };
        let stts = [SttsEntry { count: 64, delta: 1 }];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 16, sample_description_index: 1 }];
        let mut out = SampleIndex::reserve(64).unwrap();
        out.room = Room::of(Some(16));
        let chunks = [0, 0, 0, 0];
        let got = build_samples(0, 64, &sizes, &stts, &[], &stsc, &chunks, None, 0, 0, &mut out);
        assert!(matches!(got, Err(Mp4Error::Inconsistent(_))));
        assert_eq!(out.len(), 16, "the index stops at the file's octets");
    }

    #[test]
    fn a_chunk_past_the_end_is_left_out_and_its_times_stepped_over() {
        let sizes = SampleSizes::Constant { sample_size: 2, sample_count: 12 };
        let stts = [SttsEntry { count: 12, delta: 1 }];
        let ctts = [
            CttsEntry { count: 4, offset: 0 },
            CttsEntry { count: 4, offset: 5 },
            CttsEntry { count: 4, offset: 9 },
        ];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 4, sample_description_index: 1 }];
        let mut out = SampleIndex::reserve(12).unwrap();
        out.room = Room::of(Some(50));
        build_samples(0, 12, &sizes, &stts, &ctts, &stsc, &[0, 49, 10], None, 0, 0, &mut out)
            .unwrap();
        let got: Vec<_> = out.iter().map(|s| (s.offset(), s.dts, s.pts())).collect();
        let want = [(0, 0, 0), (2, 1, 1), (4, 2, 2), (6, 3, 3)];
        let tail = [(10, 8, 17), (12, 9, 18), (14, 10, 19), (16, 11, 20)];
        assert_eq!(got, [&want[..], &tail[..]].concat());
        assert_eq!(out.committed(), 8 * size_of::<Sample>());
    }

    /// Per track runs of an interleaved file, chunks of 1 to 20 samples dealt to the tracks at
    /// random, empty samples among them, the chunks of `shuffled` stored out of file order.
    // Test setup.
    #[allow(clippy::disallowed_methods)]
    fn interleaved(
        tracks: usize,
        chunks: usize,
        seed: u64,
        shuffled: Option<usize>,
    ) -> Vec<Sample> {
        let mut x = seed | 1;
        let mut next = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let mut per: Vec<Vec<Vec<Sample>>> = vec![Vec::new(); tracks];
        // a negative first DTS, as an edit that trims the composition lead leaves
        let (mut offset, mut dts) = (4096u64, vec![-3i64; tracks]);
        for _ in 0..chunks {
            let t = next(tracks as u64) as usize;
            let mut chunk = Vec::new();
            for _ in 0..1 + next(20) {
                let size = next(4) as u32 * next(3000) as u32;
                let sync = next(10) == 0;
                let s = Sample::new(t as u16, offset, size, dts[t], dts[t] + 2, sync);
                chunk.push(s.expect("a sample"));
                (offset, dts[t]) = (offset + u64::from(size), dts[t] + 1 + next(3) as i64);
            }
            per[t].push(chunk);
        }
        if let Some(t) = shuffled.filter(|&t| per[t].len() > 2) {
            let n = per[t].len();
            per[t].swap(0, n - 1);
            per[t].swap(1, n / 2);
        }
        // largest last, as the fold orders them
        per.sort_by_key(|c| c.iter().map(Vec::len).sum::<usize>());
        per.into_iter().flatten().flatten().collect()
    }

    // Test setup.
    #[allow(clippy::disallowed_methods)]
    fn runs_of(samples: &[Sample]) -> Vec<Range<usize>> {
        let mut runs: Vec<Range<usize>> = Vec::new();
        for (i, s) in samples.iter().enumerate() {
            match runs.last_mut() {
                Some(r) if samples[r.start].track_index() == s.track_index() => r.end = i + 1,
                _ => runs.push(i..i + 1),
            }
        }
        runs
    }

    /// The merge of the per track runs is the order the whole index sort gave, past the merge's
    /// run count too, and for a track whose chunks fall back in the file.
    #[test]
    fn the_merge_of_the_runs_is_the_old_sort() {
        for (tracks, chunks, shuffled) in [
            (1, 50, None),
            (2, 400, None),
            (3, 2000, None),
            (3, 2000, Some(1)),
            (5, 3000, Some(0)),
            (MERGE_RUNS, 4000, None),
            (MERGE_RUNS + 3, 4000, Some(2)),
        ] {
            for seed in [1, 7, 0x9E37_79B9] {
                let mut got = interleaved(tracks, chunks, seed, shuffled);
                let mut want = got.clone();
                want.sort_unstable_by_key(|s| (s.offset(), s.track_index(), s.dts));
                let runs = runs_of(&got);
                merge_runs(&mut got, &runs);
                assert!(got == want, "{tracks} tracks, {chunks} chunks, seed {seed}");
            }
        }
        let mut empty: [Sample; 0] = [];
        merge_runs(&mut empty, &[]);
    }

    /// File order is the offset, then the track, then the decode time, as the fields compared.
    #[test]
    fn packed_samples_order_as_their_fields() {
        let of = |offset: u64, track: u16, dts: i64| {
            Sample::new(track, offset, 0, dts, dts, true).expect("held")
        };
        let rows = [(0, 255, 9), (1, 0, -5), (1, 0, 3), (1, 1, -9), (2, 0, 0), (1 << 40, 3, 1)];
        let mut samples: Vec<Sample> = rows.iter().rev().map(|&(o, t, d)| of(o, t, d)).collect();
        samples.sort_unstable_by_key(order_key);
        let got: Vec<_> = samples.iter().map(|s| (s.offset(), s.track_index(), s.dts())).collect();
        assert_eq!(got, rows);
        assert!(packed_key(&of(1, 0, 0), usize::MAX >> 1) < packed_key(&of(1, 1, 0), 0));
        assert!(packed_key(&of(Sample::MAX_OFFSET, 255, 0), MAX_SAMPLES) < DONE);
    }

    #[test]
    fn a_sample_past_what_the_index_holds_is_refused() {
        let refused = |s: Result<Sample, Mp4Error>| matches!(s, Err(Mp4Error::Inconsistent(_)));
        assert!(refused(Sample::new(0, 0, 1 << 30, 0, 0, true)), "1 GiB");
        assert!(refused(Sample::new(0, 0, u32::MAX, 0, 0, true)));
        assert!(refused(Sample::new(0, 1 << 56, 1, 0, 0, true)), "an offset past 2^56");
        assert!(refused(Sample::new(MAX_TRACKS as u16, 0, 1, 0, 0, true)));
        assert!(refused(Sample::new(0, 0, 1, 0, i64::from(i32::MAX) + 1, true)), "past i32");
        assert!(refused(Sample::new(0, 0, 1, 0, i64::from(i32::MIN) - 1, true)));
        assert!(refused(Sample::new(0, 0, 1, i64::MIN, i64::MAX, true)), "the offset overflows");
        // the fold refuses the movie
        let sizes = SampleSizes::Table(vec![4, 1 << 30]);
        let stts = [SttsEntry { count: 2, delta: 1 }];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 2, sample_description_index: 1 }];
        let mut out = SampleIndex::reserve(2).unwrap();
        let got = build_samples(0, 2, &sizes, &stts, &[], &stsc, &[64], None, 0, 0, &mut out);
        assert!(matches!(got, Err(Mp4Error::Inconsistent(_))), "{got:?}");
        let sizes = SampleSizes::Table(vec![4, 4]);
        let mut out = SampleIndex::reserve(2).unwrap();
        let far = [(1 << 56) - 4];
        let got = build_samples(0, 2, &sizes, &stts, &[], &stsc, &far, None, 0, 0, &mut out);
        assert!(matches!(got, Err(Mp4Error::Inconsistent(_))), "{got:?}");
        assert_eq!(out.len(), 1, "the one below 2^56 was held");
    }

    /// A composition offset is 32 bits in either version of the box, so every one is held.
    #[test]
    fn the_widest_composition_offsets_are_held() {
        let sizes = SampleSizes::Constant { sample_size: 2, sample_count: 3 };
        let stts = [SttsEntry { count: 3, delta: 10 }];
        let ctts = [
            CttsEntry { count: 1, offset: i64::from(i32::MAX) },
            CttsEntry { count: 1, offset: i64::from(i32::MIN) },
            CttsEntry { count: 1, offset: 7 },
        ];
        let stsc =
            [StscEntry { first_chunk: 1, samples_per_chunk: 3, sample_description_index: 1 }];
        let mut out = SampleIndex::reserve(3).unwrap();
        build_samples(0, 3, &sizes, &stts, &ctts, &stsc, &[0], None, 5, 0, &mut out).unwrap();
        let got: Vec<_> = out.iter().map(|s| (s.dts(), s.pts())).collect();
        let want = [(-5, i64::from(i32::MAX) - 5), (5, i64::from(i32::MIN) + 5), (15, 22)];
        assert_eq!(got, want);
    }
}
