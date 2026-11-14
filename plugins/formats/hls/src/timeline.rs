//! The segment grid of a rendition, where each segment starts on the media timeline. It is
//! fixed before any media is made, the playlist lists it and every job cuts on it, so a
//! segment is the same bytes whichever job made it.

const NS: i64 = 1_000_000_000;

/// Segments a grid holds at most, some 24 days of 2 s ones, so a hostile duration or rate makes
/// no grid rather than an endless one.
pub const SEGMENTS_MAX: i64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    /// Segment k starts at `starts[k]`, ns, ascending.
    starts: Vec<i64>,
    /// Where the last segment ends, ns.
    end: i64,
}

impl Timeline {
    /// Segments of `gop` frames at `fps_milli` frames a thousand seconds, the grid a
    /// transcode's encoder keys on. Segment k starts at frame `k * gop`.
    // Cold path, once a rendition.
    #[allow(clippy::disallowed_methods)]
    pub fn frames(duration_ns: i64, fps_milli: u32, gop: u32) -> Option<Timeline> {
        if duration_ns <= 0 || fps_milli == 0 || gop == 0 {
            return None;
        }
        let at = |k: i64| {
            ((i128::from(k) * i128::from(gop) * 1_000_000_000_000 + i128::from(fps_milli) / 2)
                / i128::from(fps_milli)) as i64
        };
        if at(1) <= 0 || duration_ns / at(1) >= SEGMENTS_MAX {
            return None;
        }
        let mut starts = Vec::new();
        let mut k = 0;
        while at(k) < duration_ns {
            starts.push(at(k));
            k += 1;
        }
        Some(Self::trimmed(starts, duration_ns, at(1)))
    }

    /// Segments that start on the keyframes `keys` (ns, ascending), each at least `seg_ns`
    /// long where the keys allow it. The first starts on the first key.
    pub fn keys(duration_ns: i64, keys: &[i64], seg_ns: i64) -> Option<Timeline> {
        let first = *keys.first()?;
        if duration_ns <= first || seg_ns <= 0 {
            return None;
        }
        let mut starts = vec![first];
        for &k in keys {
            if k >= duration_ns {
                break;
            }
            if k - starts[starts.len() - 1] >= seg_ns {
                starts.push(k);
            }
        }
        Some(Self::trimmed(starts, duration_ns, seg_ns))
    }

    /// Segments of `seg_ns` from zero, for a rendition with no video to follow.
    pub fn uniform(duration_ns: i64, seg_ns: i64) -> Option<Timeline> {
        if duration_ns <= 0 || seg_ns <= 0 || duration_ns / seg_ns >= SEGMENTS_MAX {
            return None;
        }
        let starts = (0..).map(|k| k * seg_ns).take_while(|&s| s < duration_ns).collect();
        Some(Self::trimmed(starts, duration_ns, seg_ns))
    }

    /// A last segment under a quarter of `seg_ns` joins the one before.
    fn trimmed(mut starts: Vec<i64>, end: i64, seg_ns: i64) -> Timeline {
        if starts.len() > 1 && end - starts[starts.len() - 1] < seg_ns / 4 {
            starts.pop();
        }
        Timeline { starts, end }
    }

    pub fn len(&self) -> u32 {
        self.starts.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Segment starts, ns, ascending.
    pub fn starts(&self) -> &[i64] {
        &self.starts
    }

    /// Where the last segment ends, ns.
    pub fn end(&self) -> i64 {
        self.end
    }

    pub fn start(&self, k: u32) -> Option<i64> {
        self.starts.get(k as usize).copied()
    }

    /// Where segment k ends, the next start or the end of the media.
    pub fn end_of(&self, k: u32) -> i64 {
        self.starts.get(k as usize + 1).copied().unwrap_or(self.end)
    }

    pub fn duration(&self, k: u32) -> i64 {
        self.start(k).map_or(0, |s| self.end_of(k) - s)
    }

    /// The segment holding `t`, None before the first.
    pub fn of(&self, t: i64) -> Option<u32> {
        let n = self.starts.partition_point(|&s| s <= t);
        n.checked_sub(1).map(|k| k as u32)
    }

    /// The longest segment, ns.
    pub fn longest(&self) -> i64 {
        (0..self.len()).map(|k| self.duration(k)).max().unwrap_or(0)
    }

    /// `EXT-X-TARGETDURATION`, the longest segment in whole seconds, rounded.
    pub fn target_secs(&self) -> u32 {
        (0..self.len()).map(|k| ((self.duration(k) + NS / 2) / NS) as u32).max().unwrap_or(1).max(1)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn a_frame_grid_starts_on_every_gop() {
        let t = Timeline::frames(10 * NS, 25_000, 50).expect("grid");
        assert_eq!(t.len(), 5);
        assert_eq!((t.start(1), t.start(4)), (Some(2 * NS), Some(8 * NS)));
        assert_eq!(t.end_of(4), 10 * NS);
        assert_eq!(t.target_secs(), 2);
    }

    #[test]
    fn ntsc_rates_land_on_the_nearest_ns_of_their_frame() {
        let t = Timeline::frames(60 * NS, 23_976, 48).expect("grid");
        // 48 frames of 1001/24000 s
        assert_eq!(t.start(1), Some(2_002_002_002));
        assert_eq!(t.start(10), Some(20_020_020_020));
    }

    #[test]
    fn a_short_tail_joins_the_segment_before() {
        let t = Timeline::frames(10 * NS + 100 * MS, 25_000, 50).expect("grid");
        assert_eq!(t.len(), 5, "0.1 s is under a quarter of 2 s");
        assert_eq!(t.duration(4), 2 * NS + 100 * MS);
        let t = Timeline::frames(10 * NS + 600 * MS, 25_000, 50).expect("grid");
        assert_eq!(t.len(), 6);
    }

    #[test]
    fn keys_closer_than_a_segment_are_skipped() {
        let keys = [0, 500 * MS, 2100 * MS, 3000 * MS, 4200 * MS, 9000 * MS];
        let t = Timeline::keys(10 * NS, &keys, 2 * NS).expect("grid");
        assert_eq!(
            (0..t.len()).filter_map(|k| t.start(k)).collect::<Vec<_>>(),
            vec![0, 2100 * MS, 4200 * MS, 9000 * MS]
        );
        assert_eq!(t.target_secs(), 5);
        assert_eq!(t.longest(), 4800 * MS);
    }

    #[test]
    fn of_finds_the_segment_and_nothing_before_the_first() {
        let keys = [40 * MS, 2040 * MS];
        let t = Timeline::keys(4 * NS, &keys, 2 * NS).expect("grid");
        assert_eq!(t.of(0), None);
        assert_eq!(t.of(40 * MS), Some(0));
        assert_eq!(t.of(2039 * MS), Some(0));
        assert_eq!(t.of(2040 * MS), Some(1));
        assert_eq!(t.of(100 * NS), Some(1), "past the end is the last");
    }

    #[test]
    fn degenerate_inputs_make_no_grid() {
        assert_eq!(Timeline::frames(0, 25_000, 50), None);
        assert_eq!(Timeline::keys(10 * NS, &[], 2 * NS), None);
        assert_eq!(Timeline::uniform(10 * NS, 0), None);
        assert_eq!(Timeline::uniform(3 * NS, 2 * NS).map(|t| t.len()), Some(2));
    }

    #[test]
    fn a_hostile_duration_or_rate_makes_no_grid() {
        assert_eq!(Timeline::uniform(i64::MAX, 1), None);
        assert_eq!(Timeline::frames(i64::MAX, 25_000, 50), None);
        assert_eq!(Timeline::frames(10 * NS, u32::MAX, 1), None, "43 M segments");
        let most = SEGMENTS_MAX - 1;
        assert_eq!(Timeline::uniform(most * 2 * NS, 2 * NS).map(|t| t.len() as i64), Some(most));
    }
}
