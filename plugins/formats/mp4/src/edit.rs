//! Edit lists (`elst`, ISO/IEC 14496-12 8.6.6) folded onto each sample's times at resolution.
//! Samples outside every edit that a video edit needs are kept hidden, audio priming is kept as
//! delay.

use crate::boxes::ElstEntry;
use crate::reader::{Sample, TrackKind};

/// The most edits of one track the reader follows. A real list has a handful.
pub const MAX_EDITS: usize = 1024;

/// The 16.16 `media_rate` of an edit at the normal rate.
const RATE_ONE: i32 = 0x1_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditRefusal {
    Rate,
    Backwards,
    NothingPresented,
    TooMany,
}

impl std::fmt::Display for EditRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EditRefusal::Rate => {
                "an edit at a rate other than 1 or 0 and those after it are not followed"
            }
            EditRefusal::Backwards => {
                "an edit that goes back in media time and those after it are not followed"
            }
            EditRefusal::NothingPresented => {
                "the edits present no sample, only the first media time is taken off"
            }
            EditRefusal::TooMany => "the edits past the most the reader follows are not followed",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub start: i64,
    pub media: i64,
    pub len: i64,
}

impl Edit {
    fn end_media(&self) -> i64 {
        self.media.saturating_add(self.len)
    }

    fn end(&self) -> i64 {
        self.start.saturating_add(self.len)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditList {
    pub edits: Vec<Edit>,
    pub refused: Option<EditRefusal>,
    /// The movie ticks the list spans, 0 where it states no length, as empty edits alone or an
    /// edit open to the end of the media, the form a fragmented movie uses.
    pub movie_len: u64,
}

impl EditList {
    pub fn first_media(&self) -> i64 {
        self.edits.first().map_or(0, |e| e.media)
    }
}

/// Rounded up, so a sample inside a segment rounded down into the coarse movie timescale stays
/// inside.
fn to_media(ticks: u64, media: u32, movie: u32) -> i64 {
    if movie == 0 {
        return i64::MAX;
    }
    let t = (u128::from(ticks) * u128::from(media)).div_ceil(u128::from(movie));
    t.min(i64::MAX as u128) as i64
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
pub fn follow(entries: &[ElstEntry], media: u32, movie: u32) -> Option<EditList> {
    if entries.is_empty() {
        return None;
    }
    let mut list = EditList::default();
    // Movie ticks so the starts do not drift by rounding.
    let (mut at, mut open) = (0u64, false);
    for (n, e) in entries.iter().enumerate() {
        let seg = e.segment_duration;
        let start = if movie == 0 { 0 } else { to_media(at, media, movie) };
        at = at.saturating_add(seg);
        if e.media_time == -1 || e.media_rate == 0 {
            continue;
        }
        if list.refused.is_some() {
            continue;
        }
        if e.media_rate != RATE_ONE {
            list.refused = Some(EditRefusal::Rate);
            continue;
        }
        if n >= MAX_EDITS {
            list.refused = Some(EditRefusal::TooMany);
            continue;
        }
        // A segment of zero runs to the end of the media, the form a fragmented movie uses.
        let len = if seg == 0 || movie == 0 { i64::MAX } else { to_media(seg, media, movie) };
        let edit = Edit { start, media: e.media_time, len };
        if e.media_time < 0 || list.edits.last().is_some_and(|last| edit.media < last.end_media()) {
            list.refused = Some(EditRefusal::Backwards);
            continue;
        }
        open |= len == i64::MAX;
        list.edits.push(edit);
    }
    list.movie_len = if open || list.edits.is_empty() { 0 } else { at };
    Some(list)
}

fn edit_of(edits: &[Edit], m: i64) -> Option<usize> {
    let after = edits.partition_point(|e| e.media <= m);
    let k = after.checked_sub(1)?;
    (m < edits[k].end_media()).then_some(k)
}

/// The start of the first edit presenting media past `m`, or the end of the last edit.
fn hidden_stamp(edits: &[Edit], m: i64) -> i64 {
    let after = edits.partition_point(|e| e.media <= m);
    match edits.get(after) {
        Some(next) => next.start,
        None => edits.last().map_or(0, Edit::end),
    }
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
pub fn apply(samples: &mut [Sample], kind: TrackKind, list: &EditList) -> Option<usize> {
    let edits = &list.edits;
    let mut shown = vec![(usize::MAX, 0usize); edits.len()];
    let mut any = false;
    for (i, s) in samples.iter().enumerate() {
        if let Some(k) = edit_of(edits, s.pts) {
            let (first, last) = &mut shown[k];
            (*first, *last, any) = ((*first).min(i), (*last).max(i), true);
        }
    }
    if !any {
        return None;
    }
    let mut needed: Vec<(usize, usize)> = Vec::new();
    if kind == TrackKind::Video {
        let mut firsts: Vec<(usize, usize)> =
            shown.iter().copied().filter(|r| r.0 != usize::MAX).collect();
        firsts.sort_unstable();
        let (mut key, mut at) = (0usize, 0usize);
        for (i, s) in samples.iter().enumerate() {
            if at == firsts.len() {
                break;
            }
            if s.sync {
                key = i;
            }
            while firsts.get(at).is_some_and(|f| f.0 == i) {
                needed.push((key, firsts[at].1));
                at += 1;
            }
        }
        needed.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(needed.len());
        for (a, b) in needed {
            match merged.last_mut() {
                Some(m) if a <= m.1.saturating_add(1) => m.1 = m.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        needed = merged;
    }
    let first = edits[0];
    let (mut kept, mut range) = (0usize, 0usize);
    for i in 0..samples.len() {
        let mut s = samples[i];
        let m = s.pts;
        while needed.get(range).is_some_and(|r| r.1 < i) {
            range += 1;
        }
        let pts = match edit_of(edits, m) {
            Some(k) => Some(edits[k].start.saturating_add(m - edits[k].media)),
            None if needed.get(range).is_some_and(|r| r.0 <= i) => {
                s.hidden = true;
                Some(hidden_stamp(edits, m))
            }
            None if kind == TrackKind::Audio && m < first.media => {
                Some(first.start.saturating_add(m - first.media))
            }
            None => None,
        };
        let Some(pts) = pts else { continue };
        s.dts = s.dts.saturating_add(pts.saturating_sub(m));
        s.pts = pts;
        samples[kept] = s;
        kept += 1;
    }
    Some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(segment_duration: u64, media_time: i64) -> ElstEntry {
        ElstEntry { segment_duration, media_time, media_rate: RATE_ONE }
    }

    fn track(rows: &[(i64, bool)]) -> Vec<Sample> {
        rows.iter()
            .enumerate()
            .map(|(i, &(pts, sync))| Sample {
                track_index: 0,
                offset: i as u64 * 10,
                size: 10,
                dts: i as i64,
                pts,
                sync,
                hidden: false,
            })
            .collect()
    }

    fn times(samples: &[Sample]) -> Vec<(i64, bool)> {
        samples.iter().map(|s| (s.pts, s.hidden)).collect()
    }

    #[test]
    fn a_list_is_followed_in_media_ticks() {
        let list = follow(&[entry(500, -1), entry(1000, 0)], 90_000, 1000).unwrap();
        assert_eq!(list.edits, [Edit { start: 45_000, media: 0, len: 90_000 }]);
        assert_eq!((list.movie_len, list.refused), (1500, None));
        let dwell = ElstEntry { media_rate: 0, ..entry(100, 3000) };
        let list = follow(&[entry(100, 0), dwell, entry(100, 9000)], 600, 1000).unwrap();
        assert_eq!(
            list.edits.iter().map(|e| (e.start, e.media)).collect::<Vec<_>>(),
            [(0, 0), (120, 9000)]
        );
        let fast = ElstEntry { media_rate: 2 * RATE_ONE, ..entry(100, 500) };
        let list = follow(&[entry(100, 0), fast, entry(100, 900)], 1000, 1000).unwrap();
        assert_eq!((list.edits.len(), list.refused), (1, Some(EditRefusal::Rate)));
        let list = follow(&[entry(100, 500), entry(100, 0)], 1000, 1000).unwrap();
        assert_eq!((list.edits.len(), list.refused), (1, Some(EditRefusal::Backwards)));
        assert_eq!(follow(&[entry(1, 0)], 44_100, 1000).unwrap().edits[0].len, 45);
        assert_eq!(follow(&[entry(0, 7)], 1000, 1000).unwrap().edits[0].len, i64::MAX);
        assert!(follow(&[], 1000, 1000).is_none());
    }

    #[test]
    fn an_empty_edit_or_one_open_to_the_end_states_no_length() {
        // ffmpeg's fragmented form, 80 ms empty, then the media to its end
        let list = follow(&[entry(80, -1), entry(0, 1024)], 12_800, 1000).unwrap();
        assert_eq!(list.edits, [Edit { start: 1024, media: 1024, len: i64::MAX }]);
        assert_eq!(list.movie_len, 0);
        assert_eq!(follow(&[entry(80, -1)], 12_800, 1000).unwrap().movie_len, 0);
        assert_eq!(follow(&[entry(80, -1), entry(920, 0)], 12_800, 1000).unwrap().movie_len, 1000);
    }

    #[test]
    fn video_hides_what_an_edit_decodes_and_drops_the_rest() {
        let rows: Vec<(i64, bool)> = (0..10).map(|i| (i * 10, i % 5 == 0)).collect();
        let list = follow(&[entry(20, 0), entry(30, 70)], 1, 1).unwrap();
        let mut v = track(&rows);
        let kept = apply(&mut v, TrackKind::Video, &list).unwrap();
        let want = [
            (0, false),
            (10, false),
            (20, true),
            (20, true),
            (20, false),
            (30, false),
            (40, false),
        ];
        assert_eq!(times(&v[..kept]), want);
        assert_eq!(v[2].offset, 50, "the sync sample the second edit decodes from");
    }

    #[test]
    fn a_reference_presented_past_the_edit_is_hidden() {
        // I0 P30 B10 B20 P60 B40 B50, decode order.
        let rows = [
            (0, true),
            (30, false),
            (10, false),
            (20, false),
            (60, false),
            (40, false),
            (50, false),
        ];
        let list = follow(&[entry(25, 0)], 1, 1).unwrap();
        let mut v = track(&rows);
        let kept = apply(&mut v, TrackKind::Video, &list).unwrap();
        assert_eq!(times(&v[..kept]), [(0, false), (25, true), (10, false), (20, false)]);
    }

    #[test]
    fn audio_keeps_its_priming_and_text_only_what_is_shown() {
        let rows: Vec<(i64, bool)> = (0..8).map(|i| (i * 10, true)).collect();
        let list = follow(&[entry(20, 15), entry(20, 60)], 1, 1).unwrap();
        let mut a = track(&rows);
        let kept = apply(&mut a, TrackKind::Audio, &list).unwrap();
        assert_eq!(
            times(&a[..kept]),
            [(-15, false), (-5, false), (5, false), (15, false), (20, false), (30, false)]
        );
        let mut t = track(&rows);
        let kept = apply(&mut t, TrackKind::Other, &list).unwrap();
        assert_eq!(times(&t[..kept]), [(5, false), (15, false), (20, false), (30, false)]);
    }

    #[test]
    fn a_list_that_shows_nothing_is_not_applied() {
        let list = follow(&[entry(20, 1000)], 1, 1).unwrap();
        let mut v = track(&[(0, true), (10, false)]);
        assert!(apply(&mut v, TrackKind::Video, &list).is_none());
        assert_eq!(times(&v), [(0, false), (10, false)]);
    }
}
