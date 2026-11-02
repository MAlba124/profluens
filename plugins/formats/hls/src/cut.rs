//! Where a stream of frames splits into the timeline's segments. A segment is kept only when
//! it was seen from its start, a video keyframe on the segment's start or the first audio
//! frame at or after it, so a job that starts mid-segment, after a seek or a restart, makes
//! no partial segment. Nothing here holds samples, the caller does what the offer says.
//!
//! An open GOP puts leading pictures after the keyframe in decode order with stamps before
//! it. The frames right after a start that fall in the segment before, past every stamp seen
//! ahead of the start, belong to the new segment.

use crate::timeline::Timeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    /// Cut by pf-subparse's `vttwrite`, never by a cutter.
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Dropping frames until one starts a segment.
    Hunting,
    /// Filling segment k, true while the frames after its start may be leading pictures.
    Filling(u32, bool),
}

/// What to do with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Drop,
    /// The frame belongs to segment k, the first of it when nothing is pending.
    Append(u32),
    /// Segment `done` ended where this frame starts, the frame begins segment `next`.
    Cut {
        done: u32,
        next: u32,
    },
    /// Segment `done` ended where this frame starts, the frame starts no segment and is
    /// dropped, the cutter hunts for the next start.
    Close {
        done: u32,
    },
    /// The stream went back without a flush, what is pending is no segment.
    Discard,
}

#[derive(Debug, Clone)]
pub struct Cutter {
    kind: Kind,
    /// How far a video keyframe may sit from its segment's start, ns, half a frame and a little.
    tol: i64,
    state: State,
    /// The latest stamp offered since the reset, ns.
    seen: i64,
    /// The latest stamp offered ahead of the segment being filled, ns, a leading picture lies past
    /// it.
    floor: i64,
}

/// How late an audio track may start and still open the first segment, ns.
const LATE_START: i64 = 500_000_000;
/// How far past its segment's start a variable rate source's first frame may lie and still
/// start it after a restart, when no frame before it was seen, ns.
const MAX_STEP: i64 = 1_000_000_000;

impl Cutter {
    pub fn new(kind: Kind, tol: i64) -> Self {
        Self { kind, tol: tol.max(1), state: State::Hunting, seen: i64::MIN, floor: i64::MIN }
    }

    /// After a flush or a restart nothing pending is a segment.
    pub fn reset(&mut self) {
        self.state = State::Hunting;
        self.seen = i64::MIN;
    }

    pub fn filling(&self) -> Option<u32> {
        match self.state {
            State::Filling(k, _) => Some(k),
            State::Hunting => None,
        }
    }

    /// The segment a frame at `pts` falls in, a frame just short of a boundary in the one
    /// after, stamps from a sample count round down.
    fn segment(&self, tl: &Timeline, pts: i64) -> Option<u32> {
        tl.of(pts + self.tol)
    }

    /// Whether a frame at `pts` lasting `dur` is the first of segment k. A video keyframe
    /// within the tolerance of the start, or the first frame past it when the one before fell
    /// short, as the encoder keys a variable rate source (`profluens_video::keys::keyed`).
    /// Audio may open the first segment up to [`LATE_START`] late, a track that starts after
    /// the video.
    fn starts(&self, tl: &Timeline, k: u32, pts: i64, dur: i64, key: bool) -> bool {
        let Some(at) = tl.start(k) else { return false };
        match self.kind {
            Kind::Video => {
                let first_past = pts > at
                    && match self.seen {
                        i64::MIN => pts - at <= MAX_STEP,
                        prev => prev < at - self.tol,
                    };
                key && ((pts - at).abs() <= self.tol || first_past)
            }
            Kind::Audio => {
                let late = if k == 0 { LATE_START } else { 0 };
                pts >= at - self.tol && pts - at < dur.max(self.tol) + late
            }
            Kind::Text => false,
        }
    }

    pub fn offer(&mut self, tl: &Timeline, pts: i64, dur: i64, key: bool) -> Offer {
        let k = self.segment(tl, pts);
        let starts = k.is_some_and(|k| self.starts(tl, k, pts, dur, key));
        let video = self.kind == Kind::Video;
        let offer = match (self.state, k) {
            (State::Hunting, Some(k)) if starts => {
                self.state = State::Filling(k, video);
                self.floor = self.seen;
                Offer::Append(k)
            }
            (State::Hunting, _) => Offer::Drop,
            (State::Filling(cur, _), Some(k)) if k == cur => {
                self.state = State::Filling(cur, false);
                Offer::Append(cur)
            }
            (State::Filling(cur, true), Some(k)) if k + 1 == cur && pts > self.floor => {
                Offer::Append(cur)
            }
            (State::Filling(cur, _), Some(k)) if k > cur && starts => {
                self.state = State::Filling(k, video);
                self.floor = self.seen;
                Offer::Cut { done: cur, next: k }
            }
            (State::Filling(cur, _), Some(k)) if k > cur => {
                self.state = State::Hunting;
                Offer::Close { done: cur }
            }
            (State::Filling(..), _) => {
                self.state = State::Hunting;
                Offer::Discard
            }
        };
        self.seen = self.seen.max(pts);
        offer
    }

    /// At the end of the media the segment being filled is whole.
    pub fn end(&mut self) -> Option<u32> {
        let k = self.filling();
        self.state = State::Hunting;
        k
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn grid() -> Timeline {
        Timeline::frames(10_000 * MS, 25_000, 50).expect("grid")
    }

    /// Offers of 25 fps video from `from_ms`, a key every `gop` frames counted from zero.
    fn video(c: &mut Cutter, tl: &Timeline, from_ms: i64, n: i64, gop: i64) -> Vec<Offer> {
        (0..n)
            .map(|i| from_ms / 40 + i)
            .map(|f| c.offer(tl, f * 40 * MS + (f % 3) * 300_000, 40 * MS, f % gop == 0))
            .collect()
    }

    #[test]
    fn a_run_from_zero_cuts_on_every_boundary_key() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Video, 20 * MS));
        let o = video(&mut c, &tl, 0, 101, 50);
        assert_eq!(o[0], Offer::Append(0));
        assert_eq!(o[49], Offer::Append(0));
        assert_eq!(o[50], Offer::Cut { done: 0, next: 1 });
        assert_eq!(o[100], Offer::Cut { done: 1, next: 2 });
        assert_eq!(c.end(), Some(2));
    }

    #[test]
    fn a_start_mid_segment_hunts_for_the_next_boundary() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Video, 20 * MS));
        // a keyframe every 25 frames, the one at 1 s is no boundary
        let o = video(&mut c, &tl, 1000, 30, 25);
        assert!(o[..25].iter().all(|x| *x == Offer::Drop), "{o:?}");
        assert_eq!(o[25], Offer::Append(1));
    }

    /// 60 fps then 20 fps as a phone writes it, the frames around 6 s at 5983 and 6033 ms,
    /// both further from the boundary than half a 60 fps frame.
    #[test]
    fn a_frame_rate_drop_starts_the_segment_on_the_first_frame_past_it() {
        let tl = Timeline::frames(22_000 * MS, 60_000, 120).expect("grid");
        let tol = 9_333_333;
        let mut c = Cutter::new(Kind::Video, tol);
        let at = |ms: f64| (ms * MS as f64) as i64;
        assert_eq!(c.offer(&tl, at(4000.0), 50 * MS, true), Offer::Append(2));
        assert_eq!(c.offer(&tl, at(5983.333), 50 * MS, false), Offer::Append(2));
        assert_eq!(c.offer(&tl, at(6033.333), 50 * MS, true), Offer::Cut { done: 2, next: 3 });
        assert_eq!(c.offer(&tl, at(6083.333), 50 * MS, false), Offer::Append(3));
        // a restart at 6 s lands on the same frame
        c.reset();
        assert_eq!(c.offer(&tl, at(6033.333), 50 * MS, true), Offer::Append(3));
        // a key mid segment after a reset starts nothing
        c.reset();
        assert_eq!(c.offer(&tl, at(7300.0), 50 * MS, true), Offer::Drop);
    }

    #[test]
    fn stamps_rounded_to_the_millisecond_still_start_their_segment() {
        let tl = Timeline::frames(60_000 * MS, 23_976, 48).expect("grid");
        let mut c = Cutter::new(Kind::Video, 20 * MS);
        // frame 48 at 2002.002 ms, a Matroska stamp rounds it to 2002 ms
        assert_eq!(c.offer(&tl, 2002 * MS, 41 * MS, true), Offer::Append(1));
        // the frame after is in segment 1, the stamp did not reach back into segment 0
        assert_eq!(c.offer(&tl, 2043 * MS, 41 * MS, false), Offer::Append(1));
        assert_eq!(
            c.offer(&tl, 1960 * MS, 41 * MS, false),
            Offer::Discard,
            "no leading picture past the start"
        );
    }

    /// 250 frames at 25 fps in decode order, (frame, key), a key every 50 frames, P B pairs
    /// between. An open GOP sends the last two frames of a GOP after the next key, as its
    /// leading pictures.
    fn decode_order(open: bool) -> Vec<(i64, bool)> {
        let mut v = Vec::new();
        for g in (0..250).step_by(50) {
            v.push((g, true));
            let lead = open && g > 0;
            if lead {
                v.extend([(g - 2, false), (g - 1, false)]);
            }
            let end = if open && g + 50 < 250 { g + 48 } else { g + 50 };
            let mut j = g + 1;
            while j < end {
                if j + 1 < end {
                    v.extend([(j + 1, false), (j, false)]);
                } else {
                    v.push((j, false));
                }
                j += 2;
            }
        }
        v
    }

    /// The segments done and where each frame went, offered from decode position `from`.
    fn placed(open: bool, from: usize) -> (Vec<u32>, Vec<(i64, u32)>) {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Video, 20 * MS));
        let (mut done, mut at) = (Vec::new(), Vec::new());
        for &(f, key) in &decode_order(open)[from..] {
            match c.offer(&tl, f * 40 * MS, 40 * MS, key) {
                Offer::Append(k) => at.push((f, k)),
                Offer::Cut { done: d, next } => {
                    done.push(d);
                    at.push((f, next));
                }
                Offer::Drop => {}
                o => panic!("frame {f} {o:?}"),
            }
        }
        done.extend(c.end());
        (done, at)
    }

    /// The segment frame f belongs to, an open GOP's leading pictures in the next one.
    fn owner(open: bool, f: i64) -> u32 {
        (if open && f % 50 >= 48 && f < 200 { f / 50 + 1 } else { f / 50 }) as u32
    }

    #[test]
    fn closed_and_open_gops_cut_every_segment_and_place_every_frame() {
        for open in [false, true] {
            let (done, at) = placed(open, 0);
            assert_eq!(done, [0, 1, 2, 3, 4], "open {open}");
            let mut frames: Vec<i64> = at.iter().map(|x| x.0).collect();
            frames.sort_unstable();
            assert_eq!(frames, (0..250).collect::<Vec<_>>(), "open {open}, each frame once");
            for &(f, k) in &at {
                assert_eq!(k, owner(open, f), "open {open} frame {f}");
            }
        }
    }

    #[test]
    fn a_restart_at_an_open_gop_key_keeps_its_leading_pictures() {
        let order = decode_order(true);
        let key = |f| order.iter().position(|&x| x == (f, true)).expect("key");
        // a run that starts on the key of segment 2, as a demuxer seeking to it does, and one
        // that starts mid GOP and hunts
        for from in [key(100), key(50) + 20] {
            let (done, at) = placed(true, from);
            assert_eq!(done, [2, 3, 4], "from {from}");
            assert!(at.contains(&(98, 2)) && at.contains(&(99, 2)), "from {from} {at:?}");
            assert!(at.iter().all(|&(f, k)| f >= 98 && k == owner(true, f)), "from {from}");
        }
    }

    #[test]
    fn a_jump_back_after_a_cut_still_discards() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Video, 20 * MS));
        for f in 0..51 {
            c.offer(&tl, f * 40 * MS, 40 * MS, f % 50 == 0);
        }
        assert_eq!(c.filling(), Some(1));
        // a frame segment 0 already holds is no leading picture
        assert_eq!(c.offer(&tl, 30 * 40 * MS, 40 * MS, false), Offer::Discard);
        // two segments back right after a cut
        let mut c = Cutter::new(Kind::Video, 20 * MS);
        for f in (0..101).filter(|f| *f < 3 || *f >= 50) {
            c.offer(&tl, f * 40 * MS, 40 * MS, f % 50 == 0);
        }
        assert_eq!(c.offer(&tl, 10 * 40 * MS, 40 * MS, false), Offer::Discard);
    }

    #[test]
    fn a_boundary_without_a_key_closes_the_segment_and_hunts() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Video, 20 * MS));
        let o = video(&mut c, &tl, 0, 151, 75);
        assert_eq!(o[50], Offer::Close { done: 0 }, "frame 50 is no key");
        assert!(o[51..100].iter().all(|x| *x == Offer::Drop));
        // the key at 75 is no boundary, the one at 150 is
        assert_eq!(o[150], Offer::Append(3));
    }

    #[test]
    fn audio_starts_on_the_first_frame_at_or_after_the_boundary() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Audio, MS));
        let f = 21_333_333;
        // AAC frames from 1.97 s, the one at 1.991 s straddles 2 s and stays in segment 0
        let pts: Vec<i64> = (0..4).map(|i| 1_970 * MS + i * f).collect();
        let o: Vec<Offer> = pts.iter().map(|&p| c.offer(&tl, p, f, true)).collect();
        assert_eq!(o, vec![Offer::Drop, Offer::Drop, Offer::Append(1), Offer::Append(1)]);
        // run on to the next boundary
        let mut p = pts[3];
        let mut cut = None;
        while cut.is_none() {
            p += f;
            if let Offer::Cut { done, next } = c.offer(&tl, p, f, true) {
                cut = Some((done, next, p));
            }
        }
        let (done, next, at) = cut.expect("cut");
        assert_eq!((done, next), (1, 2));
        assert!(at >= 4_000 * MS && at - 4_000 * MS < f);
    }

    #[test]
    fn an_audio_frame_a_rounding_short_of_the_boundary_starts_the_segment() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Audio, MS));
        let f = 21_333_333;
        // a job moved to segment 2 whose first aligned frame was stamped 1 ns short of it
        let at = tl.start(2).expect("start");
        assert_eq!(c.offer(&tl, at - 1, f, true), Offer::Append(2));
        assert_eq!(c.offer(&tl, at - 1 + f, f, true), Offer::Append(2));
    }

    #[test]
    fn audio_that_starts_late_opens_the_first_segment() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Audio, MS));
        let f = 21_333_333;
        // E-AC-3 from 30 ms, its first AAC frame stamped a frame in
        assert_eq!(c.offer(&tl, f, f, true), Offer::Append(0));
        assert_eq!(c.offer(&tl, 2 * f, f, true), Offer::Append(0));
        // past the first segment a late frame is still no start
        let mut c = Cutter::new(Kind::Audio, MS);
        let at = tl.start(1).expect("start");
        assert_eq!(c.offer(&tl, at + f, f, true), Offer::Drop);
    }

    #[test]
    fn a_reset_drops_what_was_filling() {
        let (tl, mut c) = (grid(), Cutter::new(Kind::Video, 20 * MS));
        assert_eq!(c.offer(&tl, 0, 40 * MS, true), Offer::Append(0));
        c.reset();
        assert_eq!(c.filling(), None);
        assert_eq!(c.end(), None);
        assert_eq!(c.offer(&tl, 4_000 * MS, 40 * MS, true), Offer::Append(2));
    }
}
