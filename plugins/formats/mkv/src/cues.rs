//! A file's Cues packed a table per cued track, a column per field in place of a struct per
//! cue, the optional fields only on the tracks that state them.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use crate::ebml::{self, id};
use crate::reader::{Cue, cues_body, read_uint};

/// No column for a track, no value in a narrow column.
const NONE: u32 = u32::MAX;
/// The track's durations lie in `wide` behind its Clusters, one of them did not fit `u32`.
const WIDE: u32 = u32::MAX - 1;
/// No value in a wide duration column.
const NONE_WIDE: u64 = u64::MAX;
/// The most cues held, every column then starts below `WIDE`. Those past it are left out.
const MAX_POINTS: usize = (u32::MAX / 4) as usize;
/// Track numbers below this are found by a table, the rest by a tree.
const SMALL: usize = 256;

/// Every track's CuePoints, each track's ascending by time in the order the file states them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cues {
    /// Ascending by track number, none without a cue.
    tracks: Box<[TrackCues]>,
    /// The times in ns then the Cluster bytes of every track back to back, then its durations
    /// in ticks when one did not fit `narrow`.
    wide: Box<[u64]>,
    /// The relative positions then the durations in ticks of the tracks that have them.
    narrow: Box<[u32]>,
    /// Nanoseconds a duration tick.
    tick: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrackCues {
    track: u64,
    len: u32,
    /// Where its time column starts in `wide`, the Clusters `len` on.
    wide_at: u32,
    /// Where its columns start in `narrow`, `NONE` without, `WIDE` for durations in `wide`.
    relative_at: u32,
    duration_at: u32,
}

const _: () = assert!(size_of::<TrackCues>() == 24);

/// One track's cues, ascending by time.
#[derive(Clone, Copy, Debug)]
pub struct TrackView<'a> {
    track: u64,
    times: &'a [u64],
    clusters: &'a [u64],
    /// Empty without the column, as the two below.
    relative: &'a [u32],
    ticks: &'a [u32],
    ticks_wide: &'a [u64],
    tick: u64,
}

impl<'a> TrackView<'a> {
    pub fn track(&self) -> u64 {
        self.track
    }

    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    /// The CueTimes in ns.
    pub fn times(&self) -> &'a [u64] {
        self.times
    }

    /// Where each cue's Cluster starts in the file.
    pub fn clusters(&self) -> &'a [u64] {
        self.clusters
    }

    /// The CueRelativePosition of cue `i`, none for one past 4 GiB.
    pub fn relative(&self, i: usize) -> Option<u64> {
        self.relative.get(i).filter(|&&r| r != NONE).map(|&r| u64::from(r))
    }

    fn ticks(&self, i: usize) -> Option<u64> {
        let narrow = self.ticks.get(i).filter(|&&d| d != NONE).map(|&d| u64::from(d));
        narrow.or_else(|| self.ticks_wide.get(i).copied().filter(|&d| d != NONE_WIDE))
    }

    /// The CueDuration of cue `i`.
    pub fn duration_ns(&self, i: usize) -> Option<u64> {
        self.ticks(i).map(|d| d.saturating_mul(self.tick))
    }

    /// Every cue places its block.
    pub fn placed(&self) -> bool {
        !self.relative.is_empty() && self.relative.iter().all(|&r| r != NONE)
    }

    /// The index of the last cue at or before `t_ns`.
    pub fn floor(&self, t_ns: u64) -> Option<usize> {
        self.times.partition_point(|&t| t <= t_ns).checked_sub(1)
    }

    /// Cue `i`, which must be below [`TrackView::len`].
    pub fn cue(&self, i: usize) -> Cue {
        Cue {
            time_ns: self.times[i],
            track: self.track,
            byte: self.clusters[i],
            duration_ns: self.duration_ns(i),
            relative: self.relative(i),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Cue> + 'a {
        let v = *self;
        (0..v.len()).map(move |i| v.cue(i))
    }

    fn raw(&self, i: usize) -> Raw {
        Raw {
            time_ns: self.times[i],
            track: self.track,
            byte: self.clusters[i],
            ticks: self.ticks(i),
            relative: self.relative(i),
        }
    }
}

impl Cues {
    /// The Cues element `cues_bytes` with its header, empty for anything else. A cue is kept
    /// as [`crate::reader::parse_cue_tracks`] keeps it.
    pub fn parse(cues_bytes: &[u8], segment_data_start: u64, timestamp_scale: u64) -> Cues {
        let Some(body) = cues_body(cues_bytes) else {
            return Cues::default();
        };
        build(timestamp_scale, &|f| each_point(body, segment_data_start, timestamp_scale, f))
    }

    /// `cues` in any order, their durations held to the nanosecond.
    pub fn of(cues: &[Cue]) -> Cues {
        build(1, &|f| {
            for c in cues {
                let (time_ns, track, byte) = (c.time_ns, c.track, c.byte);
                f(Raw { time_ns, track, byte, ticks: c.duration_ns, relative: c.relative });
            }
        })
    }

    /// These without the cues of `track` whose time `dropped` names.
    pub fn without(&self, track: u64, dropped: &dyn Fn(u64) -> bool) -> Cues {
        build(self.tick, &|f| {
            for v in self.tracks() {
                let kept = |i: &usize| v.track != track || !dropped(v.times[*i]);
                (0..v.len()).filter(kept).for_each(|i| f(v.raw(i)));
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// The cues of every track.
    pub fn len(&self) -> usize {
        self.tracks.iter().map(|t| t.len as usize).sum()
    }

    /// The heap held.
    pub fn bytes(&self) -> usize {
        size_of_val(&*self.tracks) + size_of_val(&*self.wide) + size_of_val(&*self.narrow)
    }

    /// The cues of track number `track`, none when the Cues name it nowhere.
    pub fn track(&self, track: u64) -> Option<TrackView<'_>> {
        let at = self.tracks.binary_search_by_key(&track, |t| t.track).ok()?;
        Some(self.view(&self.tracks[at]))
    }

    /// The cued tracks, ascending by track number.
    pub fn tracks(&self) -> impl Iterator<Item = TrackView<'_>> {
        self.tracks.iter().map(|t| self.view(t))
    }

    /// Every cue ascending by time, then by track number.
    pub fn iter(&self) -> impl Iterator<Item = Cue> + '_ {
        let first = |(n, t): (usize, &TrackCues)| Reverse((self.wide[t.wide_at as usize], n, 0));
        let mut heads: BinaryHeap<_> = self.tracks.iter().enumerate().map(first).collect();
        std::iter::from_fn(move || {
            let Reverse((_, n, i)) = heads.pop()?;
            let v = self.view(&self.tracks[n]);
            if let Some(&next) = v.times.get(i + 1) {
                heads.push(Reverse((next, n, i + 1)));
            }
            Some(v.cue(i))
        })
    }

    fn view(&self, t: &TrackCues) -> TrackView<'_> {
        let (at, len) = (t.wide_at as usize, t.len as usize);
        let narrow = |at: u32| match at {
            NONE | WIDE => &[][..],
            at => &self.narrow[at as usize..at as usize + len],
        };
        let ticks_wide = match t.duration_at {
            WIDE => &self.wide[at + 2 * len..at + 3 * len],
            _ => &[][..],
        };
        TrackView {
            track: t.track,
            times: &self.wide[at..at + len],
            clusters: &self.wide[at + len..at + 2 * len],
            relative: narrow(t.relative_at),
            ticks: narrow(t.duration_at),
            ticks_wide,
            tick: self.tick,
        }
    }
}

/// A cue as the Cues state it, its duration in ticks.
#[derive(Clone, Copy)]
struct Raw {
    time_ns: u64,
    track: u64,
    byte: u64,
    ticks: Option<u64>,
    relative: Option<u64>,
}

/// What a track's durations take.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Width {
    None,
    Narrow,
    Wide,
}

/// A track while its table is counted and filled.
struct Slot {
    at: TrackCues,
    relative: bool,
    duration: Width,
    filled: u32,
    /// The last time filled, and that none before it was later.
    last: u64,
    sorted: bool,
}

/// Where each track's [`Slot`] lies, in the order the Cues first name them.
struct Slots {
    small: [u32; SMALL],
    large: BTreeMap<u64, u32>,
}

impl Slots {
    fn get(&self, track: u64) -> Option<usize> {
        let at = match self.small.get(track as usize).filter(|_| track < SMALL as u64) {
            Some(&at) => Some(at).filter(|&at| at != NONE),
            None => self.large.get(&track).copied(),
        };
        at.map(|at| at as usize)
    }

    /// The slot of `track`, `next` for one not seen before.
    fn of(&mut self, track: u64, next: usize) -> usize {
        if let Some(at) = self.get(track) {
            return at;
        }
        match track < SMALL as u64 {
            true => self.small[track as usize] = next as u32,
            false => {
                self.large.insert(track, next as u32);
            }
        }
        next
    }
}

/// Passes every cue to its argument, the same ones in the same order on each call.
type Each<'a> = &'a dyn Fn(&mut dyn FnMut(Raw));

/// The tables of the cues `each` passes, the same ones on each of its two runs. The first run
/// counts what each track takes, so the columns are allocated once at their size.
// Cold path, once per Cues.
#[allow(clippy::disallowed_methods)]
fn build(tick: u64, each: Each<'_>) -> Cues {
    let mut slots = Slots { small: [NONE; SMALL], large: BTreeMap::new() };
    let mut of: Vec<Slot> = Vec::new();
    let mut total = 0usize;
    each(&mut |c| {
        if total == MAX_POINTS {
            return;
        }
        total += 1;
        let n = slots.of(c.track, of.len());
        if n == of.len() {
            let at =
                TrackCues { track: c.track, len: 0, wide_at: 0, relative_at: 0, duration_at: 0 };
            of.push(Slot {
                at,
                relative: false,
                duration: Width::None,
                filled: 0,
                last: 0,
                sorted: true,
            });
        }
        let s = &mut of[n];
        s.at.len += 1;
        s.relative |= c.relative.is_some_and(|r| r < u64::from(NONE));
        s.duration = s.duration.max(match c.ticks {
            None => Width::None,
            Some(d) if d < u64::from(NONE) => Width::Narrow,
            Some(_) => Width::Wide,
        });
    });
    if of.is_empty() {
        return Cues::default();
    }
    let mut order: Vec<u32> = (0..of.len() as u32).collect();
    order.sort_unstable_by_key(|&n| of[n as usize].at.track);
    let (mut wide_len, mut narrow_len) = (0u32, 0u32);
    for &n in &order {
        let s = &mut of[n as usize];
        let len = s.at.len;
        let mut column = |on: bool| match on {
            true => {
                narrow_len += len;
                narrow_len - len
            }
            false => NONE,
        };
        s.at.relative_at = column(s.relative);
        s.at.duration_at = match s.duration {
            Width::Wide => WIDE,
            width => column(width == Width::Narrow),
        };
        s.at.wide_at = wide_len;
        wide_len += len * if s.duration == Width::Wide { 3 } else { 2 };
    }
    let mut wide = vec![0u64; wide_len as usize].into_boxed_slice();
    let mut narrow = vec![NONE; narrow_len as usize].into_boxed_slice();
    let mut left = total;
    each(&mut |c| {
        let Some(s) = slots.get(c.track).map(|n| &mut of[n]).filter(|_| left > 0) else {
            return;
        };
        left -= 1;
        if s.filled == s.at.len {
            return;
        }
        let (at, len, i) = (s.at.wide_at as usize, s.at.len as usize, s.filled as usize);
        s.filled += 1;
        s.sorted &= c.time_ns >= s.last;
        s.last = c.time_ns;
        wide[at + i] = c.time_ns;
        wide[at + len + i] = c.byte;
        if s.at.relative_at != NONE {
            let relative = c.relative.filter(|&r| r < u64::from(NONE));
            narrow[s.at.relative_at as usize + i] = relative.map_or(NONE, |r| r as u32);
        }
        match s.at.duration_at {
            NONE => {}
            WIDE => wide[at + 2 * len + i] = c.ticks.map_or(NONE_WIDE, |d| d.min(NONE_WIDE - 1)),
            ticks => narrow[ticks as usize + i] = c.ticks.map_or(NONE, |d| d as u32),
        }
    });
    let (mut by, mut wide_tmp, mut narrow_tmp) = (Vec::new(), Vec::new(), Vec::new());
    for s in of.iter().filter(|s| !s.sorted) {
        let (at, len) = (s.at.wide_at as usize, s.at.len as usize);
        by.clear();
        by.extend(0..s.at.len);
        by.sort_by_key(|&i| wide[at + i as usize]);
        let columns = if s.at.duration_at == WIDE { 3 } else { 2 };
        for column in 0..columns {
            permute(&mut wide[at + column * len..at + (column + 1) * len], &by, &mut wide_tmp);
        }
        for column in [s.at.relative_at, s.at.duration_at] {
            if column < WIDE {
                let column = column as usize;
                permute(&mut narrow[column..column + len], &by, &mut narrow_tmp);
            }
        }
    }
    let tracks = order.iter().map(|&n| of[n as usize].at).collect();
    Cues { tracks, wide, narrow, tick }
}

/// `column` in the order `by` names its entries.
fn permute<T: Copy>(column: &mut [T], by: &[u32], tmp: &mut Vec<T>) {
    tmp.clear();
    tmp.extend(by.iter().map(|&i| column[i as usize]));
    column.copy_from_slice(tmp);
}

/// The children of the element body `data` up to the first cut short or malformed.
fn children<'a>(data: &'a [u8], f: &mut dyn FnMut(&'a [u8], &'a [u8])) {
    let mut at = 0usize;
    while at < data.len() {
        let Ok(h) = ebml::read_element_header(data, at) else {
            break;
        };
        let end = match h.data_end() {
            Ok(Some(e)) if e <= data.len() => e,
            _ => break,
        };
        f(h.id, &data[h.data_start..end]);
        at = end;
    }
}

fn each_point(body: &[u8], segment_data_start: u64, scale: u64, f: &mut dyn FnMut(Raw)) {
    children(body, &mut |id, point| {
        if id != id::CUE_POINT {
            return;
        }
        // The CueTime may follow the positions it stamps.
        let mut time = None;
        children(point, &mut |id, v| {
            if id == id::CUE_TIME {
                time = Some(read_uint(v).saturating_mul(scale));
            }
        });
        let Some(time_ns) = time else { return };
        children(point, &mut |id, v| {
            if id == id::CUE_TRACK_POSITIONS
                && let Some(c) = position(v, segment_data_start, time_ns)
            {
                f(c);
            }
        });
    });
}

/// A CueTrackPositions body, none without its track or Cluster or with a malformed child.
fn position(data: &[u8], segment_data_start: u64, time_ns: u64) -> Option<Raw> {
    let (mut track, mut pos, mut ticks, mut relative) = (None, None, None, None);
    let mut at = 0usize;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if end > data.len() {
            return None;
        }
        let value = read_uint(&data[h.data_start..end]);
        if h.id == id::CUE_TRACK {
            track = Some(value);
        } else if h.id == id::CUE_CLUSTER_POSITION {
            pos = Some(value);
        } else if h.id == id::CUE_DURATION {
            ticks = Some(value);
        } else if h.id == id::CUE_RELATIVE_POSITION {
            relative = Some(value);
        }
        at = end;
    }
    let byte = segment_data_start.saturating_add(pos?);
    Some(Raw { time_ns, track: track?, byte, ticks, relative })
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::reader::parse_cue_tracks;

    const MS: u64 = 1_000_000;

    /// One CueTrackPositions as `(track, Cluster position, relative, duration ticks)`.
    type At = (u64, u64, Option<u64>, Option<u64>);

    fn cues_el(points: &[(u64, Vec<At>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (time, positions) in points {
            let mut point = Vec::new();
            ebml::write_uint(&mut point, id::CUE_TIME, *time);
            for &(track, pos, relative, ticks) in positions {
                let mut p = Vec::new();
                ebml::write_uint(&mut p, id::CUE_TRACK, track);
                ebml::write_uint(&mut p, id::CUE_CLUSTER_POSITION, pos);
                if let Some(r) = relative {
                    ebml::write_uint(&mut p, id::CUE_RELATIVE_POSITION, r);
                }
                if let Some(d) = ticks {
                    ebml::write_uint(&mut p, id::CUE_DURATION, d);
                }
                ebml::write_binary(&mut point, id::CUE_TRACK_POSITIONS, &p);
            }
            ebml::write_binary(&mut body, id::CUE_POINT, &point);
        }
        let mut cues = Vec::new();
        ebml::write_binary(&mut cues, id::CUES, &body);
        cues
    }

    /// What the tables must hold of the plain parse, a relative position past `u32` dropped,
    /// by time then track.
    fn oracle(bytes: &[u8], start: u64, scale: u64) -> Vec<Cue> {
        let mut all = parse_cue_tracks(bytes, start, scale);
        for c in &mut all {
            c.relative = c.relative.filter(|&r| r < u64::from(u32::MAX));
        }
        all.sort_by_key(|c| (c.time_ns, c.track));
        all
    }

    fn same(bytes: &[u8], start: u64, scale: u64) -> Cues {
        let (want, got) = (oracle(bytes, start, scale), Cues::parse(bytes, start, scale));
        assert_eq!(got.iter().collect::<Vec<_>>(), want);
        assert_eq!((got.len(), got.is_empty()), (want.len(), want.is_empty()));
        let mut numbers: Vec<u64> = want.iter().map(|c| c.track).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(got.tracks().map(|v| v.track()).collect::<Vec<_>>(), numbers);
        for n in numbers {
            let own: Vec<Cue> = want.iter().filter(|c| c.track == n).copied().collect();
            let v = got.track(n).expect("a cued track");
            assert_eq!(v.iter().collect::<Vec<_>>(), own, "track {n}");
            assert_eq!(v.placed(), own.iter().all(|c| c.relative.is_some()), "track {n}");
            assert!(v.times().is_sorted());
        }
        got
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn generated_cues_hold_what_the_plain_parse_reads() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..200 {
            let tracks = 1 + rng.below(6);
            let number = |rng: &mut Rng| match rng.below(8) {
                0 => 250 + rng.below(1_000),
                _ => 1 + rng.below(tracks),
            };
            let mut time = 0u64;
            let points: Vec<(u64, Vec<At>)> = (0..rng.below(120))
                .map(|_| {
                    time = match rng.below(10) {
                        0 => time.saturating_sub(rng.below(5_000)),
                        _ => time + rng.below(3_000),
                    };
                    let positions = (0..rng.below(4))
                        .map(|_| {
                            let track = number(&mut rng);
                            let relative = match (track % 3, rng.below(20)) {
                                (0, _) => None,
                                (_, 0) => Some(u64::from(u32::MAX) + rng.below(3) - 1),
                                _ => Some(rng.below(1 << 20)),
                            };
                            let ticks = match (track % 2, rng.below(20)) {
                                (0, _) => None,
                                (_, 0) => Some(u64::from(u32::MAX) + rng.below(3) - 1),
                                (_, 1) => None,
                                _ => Some(rng.below(10_000)),
                            };
                            (track, rng.below(1 << 40), relative, ticks)
                        })
                        .collect();
                    (time, positions)
                })
                .collect();
            let scale = [1, 1_000, MS, 1 << 40][round % 4];
            same(&cues_el(&points), rng.below(1 << 30), scale);
        }
    }

    #[test]
    fn a_cut_or_malformed_cues_holds_what_the_plain_parse_reads_of_it() {
        let whole = cues_el(&[
            (0, vec![(1, 100, Some(4), None), (3, 100, Some(40), Some(900))]),
            (7, vec![(1, 200, None, None)]),
            (5, vec![(3, 150, Some(9), Some(u64::MAX - 1))]),
        ]);
        for cut in 0..whole.len() {
            same(&whole[..cut], 50, MS);
            same(&whole[cut..], u64::MAX, u64::MAX);
            let mut broken = whole.clone();
            broken[cut] ^= 0x55;
            same(&broken, 50, MS);
        }
        assert_eq!(Cues::parse(&[], 0, MS), Cues::default());
        let body = ebml::read_element_header(&whole, 0).expect("a head").data_start;
        assert_eq!(Cues::parse(&whole[body..], 0, MS), Cues::default(), "a payload alone");
    }

    /// A CuePoint whose time follows its positions stamps them, one without a time is dropped.
    #[test]
    fn a_cue_time_behind_its_positions_stamps_them() {
        let position = |track: u64, pos: u64| {
            let mut p = Vec::new();
            ebml::write_uint(&mut p, id::CUE_TRACK, track);
            ebml::write_uint(&mut p, id::CUE_CLUSTER_POSITION, pos);
            let mut out = Vec::new();
            ebml::write_binary(&mut out, id::CUE_TRACK_POSITIONS, &p);
            out
        };
        let mut late = position(1, 10);
        ebml::write_uint(&mut late, id::CUE_TIME, 4);
        let (mut body, mut bytes) = (Vec::new(), Vec::new());
        ebml::write_binary(&mut body, id::CUE_POINT, &late);
        ebml::write_binary(&mut body, id::CUE_POINT, &position(1, 20));
        ebml::write_binary(&mut bytes, id::CUES, &body);
        let cues = same(&bytes, 100, MS);
        let v = cues.track(1).expect("cued");
        assert_eq!((v.times(), v.clusters()), (&[4 * MS][..], &[110][..]));
    }

    #[test]
    fn each_track_holds_only_the_columns_its_cues_state() {
        let video = |n: u64| (n * 2_000, vec![(1, n * 1_000, Some(n), None)]);
        let cues = same(&cues_el(&(0..100).map(video).collect::<Vec<_>>()), 0, MS);
        assert_eq!(cues.bytes(), 24 + 100 * 20, "time, Cluster and relative");
        let bare = |n: u64| (n * 2_000, vec![(1, n * 1_000, None, None)]);
        let cues = same(&cues_el(&(0..100).map(bare).collect::<Vec<_>>()), 0, MS);
        assert_eq!(cues.bytes(), 24 + 100 * 16, "time and Cluster");
        assert!(!cues.track(1).expect("cued").placed(), "no cue places its block");
        let text = |n: u64| (n * 2_000, vec![(3, n * 1_000, Some(n), Some(1_500))]);
        let cues = same(&cues_el(&(0..100).map(text).collect::<Vec<_>>()), 0, MS);
        assert_eq!(cues.bytes(), 24 + 100 * 24, "and a duration");
        let both = |n: u64| {
            (n * 2_000, vec![(1, n * 1_000, Some(n), None), (3, n * 1_000, Some(n), Some(9))])
        };
        let cues = same(&cues_el(&(0..100).map(both).collect::<Vec<_>>()), 0, MS);
        assert_eq!(cues.bytes(), 2 * 24 + 100 * (20 + 24), "the video pays for no duration");
        assert_eq!(cues.track(3).and_then(|v| v.duration_ns(7)), Some(9 * MS));
        assert_eq!(cues.track(1).and_then(|v| v.duration_ns(7)), None);
        assert!(cues.track(2).is_none());
    }

    #[test]
    fn a_relative_position_past_4_gib_is_none_and_the_track_not_placed() {
        let far = u64::from(u32::MAX);
        let points =
            [(0, vec![(3, 10, Some(5), None)]), (9, vec![(3, 20, Some(far), None)])].to_vec();
        let cues = same(&cues_el(&points), 0, MS);
        let v = cues.track(3).expect("cued");
        assert_eq!((v.relative(0), v.relative(1)), (Some(5), None));
        assert!(!v.placed());
        let near = [(0, vec![(3, 10, Some(far - 1), None)])].to_vec();
        let v = same(&cues_el(&near), 0, MS);
        let v = v.track(3).expect("cued");
        assert_eq!(v.relative(0), Some(far - 1));
        assert!(v.placed());
        let all_far = [(0, vec![(3, 10, Some(far + 7), None)])].to_vec();
        assert_eq!(same(&cues_el(&all_far), 0, MS).bytes(), 24 + 16, "no column for none");
    }

    /// A duration past `u32` ticks moves its track's durations to a wide column, the other
    /// tracks stay narrow.
    #[test]
    fn a_duration_past_u32_ticks_is_kept_whole() {
        let long = u64::from(u32::MAX);
        let points = [
            (0, vec![(3, 10, None, Some(long - 1)), (4, 10, None, Some(long))]),
            (5, vec![(3, 20, None, None), (4, 20, None, None)]),
            (9, vec![(3, 30, None, Some(7)), (4, 30, None, Some(1 << 50))]),
        ]
        .to_vec();
        let cues = same(&cues_el(&points), 0, 1);
        let ns = |track: u64| {
            let v = cues.track(track).expect("cued");
            [v.duration_ns(0), v.duration_ns(1), v.duration_ns(2)]
        };
        assert_eq!(ns(3), [Some(long - 1), None, Some(7)]);
        assert_eq!(ns(4), [Some(long), None, Some(1 << 50)]);
        assert_eq!(cues.bytes(), 2 * 24 + 3 * 20 + 3 * 24, "u32 for track 3, u64 for track 4");
        let cues = same(&cues_el(&points), 0, MS);
        let v = cues.track(4).expect("cued");
        assert_eq!(v.duration_ns(0), Some(long * MS));
        assert_eq!(v.duration_ns(2), Some(u64::MAX), "saturated, as the plain parse");
        // the one value a wide column cannot tell from none reads a tick short
        let max = [(0, vec![(3, 10, None, Some(u64::MAX))])].to_vec();
        let cues = Cues::parse(&cues_el(&max), 0, 1);
        assert_eq!(cues.track(3).and_then(|v| v.duration_ns(0)), Some(u64::MAX - 1));
        let cues = Cues::parse(&cues_el(&max), 0, 2);
        assert_eq!(cues.track(3).and_then(|v| v.duration_ns(0)), Some(u64::MAX));
    }

    /// A track's cues out of time order are sorted, those of one time in the order stated,
    /// every column with them.
    #[test]
    fn cues_out_of_time_order_are_sorted_a_track() {
        let points = [
            (9, vec![(3, 90, Some(1), Some(u64::from(u32::MAX))), (1, 90, Some(11), None)]),
            (2, vec![(3, 20, Some(2), Some(5)), (1, 20, Some(12), None)]),
            (9, vec![(3, 91, Some(3), None)]),
            (4, vec![(3, 40, Some(4), Some(6)), (5, 40, None, Some(8))]),
            (1, vec![(5, 10, None, Some(3))]),
        ]
        .to_vec();
        let cues = same(&cues_el(&points), 0, MS);
        let v = cues.track(3).expect("cued");
        assert_eq!(v.times(), [2 * MS, 4 * MS, 9 * MS, 9 * MS]);
        assert_eq!(v.clusters(), [20, 40, 90, 91]);
        assert_eq!((0..4).map(|i| v.relative(i)).collect::<Vec<_>>(), [2, 4, 1, 3].map(Some));
        let durations: Vec<_> = (0..4).map(|i| v.duration_ns(i)).collect();
        let long = u64::from(u32::MAX) * MS;
        assert_eq!(durations, [Some(5 * MS), Some(6 * MS), Some(long), None]);
        let v = cues.track(5).expect("cued");
        assert_eq!((v.clusters(), v.duration_ns(0)), (&[10, 40][..], Some(3 * MS)));
        assert_eq!(v.floor(0), None);
        assert_eq!((v.floor(MS), v.floor(3 * MS), v.floor(u64::MAX)), (Some(0), Some(0), Some(1)));
    }

    #[test]
    fn track_numbers_of_any_size_are_held_ascending() {
        let numbers = [u64::MAX, 255, 256, 1, 1 << 40, 0, 257, 256, 1];
        let points: Vec<(u64, Vec<At>)> =
            numbers.iter().zip(0..).map(|(&n, t)| (t, vec![(n, t * 10, None, None)])).collect();
        let cues = same(&cues_el(&points), 0, MS);
        let held: Vec<u64> = cues.tracks().map(|v| v.track()).collect();
        assert_eq!(held, [0, 1, 255, 256, 257, 1 << 40, u64::MAX]);
        assert_eq!(cues.track(256).map(|v| v.len()), Some(2));
        assert!(cues.track(2).is_none() && cues.track(258).is_none());
    }

    #[test]
    fn cues_given_by_value_and_cues_without_some_are_the_same_tables() {
        let points = [
            (0, vec![(1, 0, Some(1), None), (3, 0, Some(40), Some(u64::from(u32::MAX) + 5))]),
            (2_000, vec![(1, 500, Some(1), None)]),
            (2_500, vec![(3, 500, Some(90), Some(800))]),
            (4_000, vec![(1, 900, Some(1), None)]),
        ]
        .to_vec();
        let bytes = cues_el(&points);
        let cues = same(&bytes, 100, MS);
        let all = oracle(&bytes, 100, MS);
        let mut shuffled = all.clone();
        shuffled.reverse();
        assert_eq!(Cues::of(&shuffled).iter().collect::<Vec<_>>(), all, "by value, any order");
        assert_eq!(Cues::of(&[]), Cues::default());
        let kept = cues.without(1, &|t| t == 2_000 * MS);
        let want: Vec<Cue> =
            all.iter().filter(|c| (c.track, c.time_ns) != (1, 2_000 * MS)).copied().collect();
        assert_eq!(kept.iter().collect::<Vec<_>>(), want);
        assert_eq!(cues.without(9, &|_| true), cues, "another track's are kept");
        assert_eq!(cues.without(3, &|_| true).tracks().count(), 1, "a track left empty goes");
        let none = cues.without(1, &|_| true).without(3, &|_| true);
        assert_eq!(none, Cues::default());
    }

    /// The Cues of a file of many cued tracks take the bytes the plan gives them.
    #[test]
    fn a_heavily_cued_file_packs_to_under_half() {
        let points: Vec<(u64, Vec<At>)> = (0..2_000u64)
            .map(|n| {
                let text = (2..38).map(|t| (t, n * 4_096, Some(t * 100), Some(1_500)));
                let video = [(1, n * 4_096, Some(0), None)];
                (n * 1_000, video.into_iter().chain(text).collect())
            })
            .collect();
        let bytes = cues_el(&points);
        let cues = same(&bytes, 0, MS);
        let plain = cues.len() * size_of::<Cue>();
        assert_eq!(cues.len(), 2_000 * 37);
        assert_eq!(cues.bytes(), 37 * 24 + 2_000 * (20 + 36 * 24));
        assert!(cues.bytes() * 2 < plain, "{} of {plain}", cues.bytes());
    }
}
