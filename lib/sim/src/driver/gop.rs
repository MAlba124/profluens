//! Groups of pictures, as a fake decoder and the seek rule see stream 0. A buffer's sequence
//! number is also the byte a seek names.

use crate::action::{Action, ActionKind, Anchor};
use crate::media::{StreamKind, StreamSpec};
use crate::prng::{Prng, mix};
use crate::time::{NONE, Ns, frame_at, frame_pts, frames_in};

pub const MAX_REORDER: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GopDecl {
    pub on: bool,
    pub nonref: u8,
    pub reorder: u8,
    pub raw: bool,
    pub forced: Option<How>,
    pub mapped: Mapped,
}

impl GopDecl {
    pub const OFF: Self =
        Self { on: false, nonref: 0, reorder: 0, raw: false, forced: None, mapped: Mapped::Whole };

    /// The source restarts a non-keyframe byte at its group's keyframe, as a demuxer handed an
    /// estimate does.
    pub const fn resyncs(&self) -> bool {
        !matches!(self.mapped, Mapped::Whole)
    }
}

/// How the source publishes its index of stream 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapped {
    Whole,
    Late { after: u8 },
    Grows,
    Partial,
}

pub fn draw(root: &Prng) -> GopDecl {
    const NONREF: [u8; 5] = [0, 0, 2, 3, 4];
    let mut rng = root.substream_named("gop");
    let (on, nonref) = (rng.chance(35), NONREF[rng.below(NONREF.len() as u64) as usize]);
    let (reorders, depth) = (rng.chance(50), rng.range(1, MAX_REORDER as u64 + 1) as u8);
    let raw = rng.chance(35);
    // A stream of its own, so the other knobs draw as they did.
    let mut index = root.substream_named("gop.mapped");
    let mapped = match index.below(10) {
        0 => Mapped::Late { after: index.range(1, 40) as u8 },
        1 => Mapped::Grows,
        2 => Mapped::Partial,
        _ => Mapped::Whole,
    };
    GopDecl { on, nonref, reorder: if reorders { depth } else { 0 }, raw, forced: None, mapped }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pictures {
    kind: StreamKind,
    interval: u64,
    count: u64,
    nonref: u64,
}

impl Pictures {
    pub fn of(stream: &StreamSpec, gop: &GopDecl) -> Pictures {
        let (interval, count) = match stream.kind {
            StreamKind::Video { fps, keyframe_interval } => {
                (keyframe_interval.max(1) as u64, frames_in(stream.duration, fps))
            }
            StreamKind::Audio { packet } => (1, stream.duration.checked_div(packet).unwrap_or(0)),
            StreamKind::Text { cues } => (1, cues.len as u64),
        };
        Pictures { kind: stream.kind, interval, count, nonref: gop.nonref as u64 }
    }

    pub const fn count(&self) -> u64 {
        self.count
    }

    pub const fn spans(&self) -> bool {
        !matches!(self.kind, StreamKind::Video { .. })
    }

    pub fn pts(&self, seq: u64) -> Ns {
        match self.kind {
            StreamKind::Video { fps, .. } => frame_pts(seq, fps),
            StreamKind::Audio { packet } => seq.saturating_mul(packet),
            StreamKind::Text { .. } => NONE,
        }
    }

    pub fn at_or_after(&self, t: Ns) -> u64 {
        let at_or_before = match self.kind {
            StreamKind::Video { fps, .. } => frame_at(t, fps),
            StreamKind::Audio { packet } => t.checked_div(packet).unwrap_or(0),
            StreamKind::Text { .. } => return self.count,
        };
        let first = at_or_before + (self.pts(at_or_before) < t) as u64;
        first.min(self.count)
    }

    pub const fn key_of(&self, seq: u64) -> u64 {
        seq - seq % self.interval
    }

    pub const fn is_key(&self, seq: u64) -> bool {
        seq.is_multiple_of(self.interval)
    }

    pub const fn is_reference(&self, seq: u64) -> bool {
        self.nonref < 2 || (seq % self.interval).is_multiple_of(self.nonref)
    }

    pub const fn needs(&self, seq: u64) -> Option<u64> {
        let place = seq % self.interval;
        if place == 0 {
            return None;
        }
        let before = place - 1;
        let reference = if self.nonref < 2 { before } else { before - before % self.nonref };
        Some(seq - place + reference)
    }

    pub const fn resync(&self, seq: u64) -> u64 {
        if seq >= self.count { seq } else { self.key_of(seq) }
    }

    pub const fn keys(&self) -> u64 {
        self.count.div_ceil(self.interval)
    }

    pub fn key(&self, n: u64) -> (Ns, u64) {
        let seq = n.saturating_mul(self.interval);
        (self.pts(seq), seq)
    }

    pub fn at_or_before(&self, t: Ns) -> u64 {
        let first = self.at_or_after(t);
        let exact = first < self.count && self.pts(first) == t;
        if exact { first } else { first.saturating_sub(1) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    Planned { keyframe: bool },
    RawSkip,
    RawSeek { pick: u8 },
}

/// Names an action by what it is, so it is carried out the same way in a shrunk script.
pub(super) fn key_of(action: &Action) -> u64 {
    let anchor = match action.anchor {
        Anchor::At(t) => t,
        Anchor::AfterBuffers { sink, count } => 1 << 60 | (sink as u64) << 32 | count as u64,
        Anchor::AfterEos { sink, count, lag } => {
            2 << 60 | (sink as u64) << 32 | (count as u64) << 16 | lag as u64
        }
        Anchor::AtStep(step) => 3 << 60 | step as u64,
        Anchor::InFlush { sink, count } => 4 << 60 | (sink as u64) << 32 | count as u64,
    };
    let to = match action.kind {
        ActionKind::Seek { to } => to,
        _ => 0,
    };
    mix(anchor) ^ mix(to.rotate_left(17))
}

pub fn how(seed: u64, gop: &GopDecl, action: &Action) -> How {
    if let Some(forced) = gop.forced {
        return forced;
    }
    let mut rng = Prng::new(seed).substream_named("gop.how").substream(key_of(action));
    let (which, pick) = (rng.below(100), rng.below(3) as u8);
    let keyframe = rng.chance(20);
    match which {
        0..=9 if gop.raw => How::RawSkip,
        10..=19 if gop.raw => How::RawSeek { pick },
        _ => How::Planned { keyframe },
    }
}

pub fn raw_byte(pictures: &Pictures, pick: u8, to: Ns) -> u64 {
    let at = pictures.at_or_before(to);
    let key = pictures.key_of(at);
    match pick {
        0 => key + 1,
        1 => key + pictures.interval,
        _ => at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Pacing;
    use crate::time::{MS, Rational, SEC};

    fn video(interval: u32, nonref: u8) -> Pictures {
        let kind = StreamKind::Video { fps: Rational::new(25, 1), keyframe_interval: interval };
        let stream =
            StreamSpec { kind, duration: 2 * SEC, size: 1, pacing: Pacing::AsFastAsPossible };
        Pictures::of(&stream, &GopDecl { nonref, ..GopDecl::OFF })
    }

    #[test]
    fn a_picture_is_decoded_from_the_reference_before_it_in_its_group() {
        let all = video(12, 0);
        assert_eq!((all.count(), all.keys()), (50, 5));
        assert_eq!(
            (all.needs(0), all.needs(1), all.needs(11), all.needs(12)),
            (None, Some(0), Some(10), None)
        );
        assert!((0..50).all(|seq| all.is_reference(seq)));
        // Two of three are pictures nothing refers to, as with two B-frames.
        let ibbp = video(12, 3);
        let refs: Vec<u64> = (0..12).filter(|seq| ibbp.is_reference(*seq)).collect();
        assert_eq!(refs, [0, 3, 6, 9]);
        let needs: Vec<Option<u64>> = (12..19).map(|seq| ibbp.needs(seq)).collect();
        let want = [None, Some(12), Some(12), Some(12), Some(15), Some(15), Some(15)];
        assert_eq!(needs, want);
        for (interval, nonref) in [(1, 0), (5, 2), (25, 4), (12, 3)] {
            let p = video(interval, nonref);
            for seq in 0..p.count() {
                let Some(need) = p.needs(seq) else {
                    assert!(p.is_key(seq));
                    continue;
                };
                assert!(need < seq && need >= p.key_of(seq) && p.is_reference(need), "{seq}");
            }
        }
    }

    #[test]
    fn the_index_has_one_entry_per_keyframe_and_times_map_to_pictures() {
        let p = video(12, 0);
        let entries: Vec<(Ns, u64)> = (0..p.keys()).map(|n| p.key(n)).collect();
        assert_eq!(
            entries,
            [(0, 0), (480 * MS, 12), (960 * MS, 24), (1_440 * MS, 36), (1_920 * MS, 48)]
        );
        assert_eq!((p.at_or_after(0), p.at_or_after(1), p.at_or_after(40 * MS)), (0, 1, 1));
        assert_eq!((p.at_or_after(1_960 * MS), p.at_or_after(1_961 * MS)), (49, 50));
        assert_eq!(
            (p.at_or_before(0), p.at_or_before(79 * MS), p.at_or_before(9 * SEC)),
            (0, 1, 49)
        );
        let kind = StreamKind::Audio { packet: 20 * MS };
        let stream = StreamSpec { kind, duration: SEC, size: 1, pacing: Pacing::AsFastAsPossible };
        let audio = Pictures::of(&stream, &GopDecl::OFF);
        assert_eq!((audio.count(), audio.keys(), audio.needs(7)), (50, 50, None));
        assert_eq!((audio.at_or_after(30 * MS), audio.key(3)), (2, (60 * MS, 3)));
    }

    #[test]
    fn a_raw_byte_is_one_the_rule_does_not_send() {
        let p = video(12, 0);
        // 600 ms is picture 15, in the group of keyframe 12.
        assert_eq!((raw_byte(&p, 0, 600 * MS), raw_byte(&p, 1, 600 * MS)), (13, 24));
        assert_eq!(raw_byte(&p, 2, 600 * MS), 15);
    }

    #[test]
    fn an_action_is_carried_out_one_way_whatever_else_is_in_the_script() {
        let gop = GopDecl { on: true, raw: true, ..GopDecl::OFF };
        let seek = |to| Action { anchor: Anchor::AtStep(7), kind: ActionKind::Seek { to } };
        let (mut raw, mut keyframes) = (0, 0);
        for to in 0..2_000u64 {
            let got = how(9, &gop, &seek(to));
            assert_eq!(got, how(9, &gop, &seek(to)));
            match got {
                How::Planned { keyframe } => keyframes += keyframe as u32,
                _ => raw += 1,
            }
            let tame = how(9, &GopDecl { raw: false, ..gop }, &seek(to));
            assert!(matches!(tame, How::Planned { .. }), "no raw seek unless declared");
        }
        assert!((300..500).contains(&raw) && keyframes > 200, "{raw} {keyframes}");
    }
}
