//! The seek index `mkvdemux` publishes, from the Cues or grown from the Clusters read past.
//! An entry restarts for its worst target, one on the keyframe.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

pub use profluens_core::pipeline::CAPTION_REACH;
use profluens_core::pipeline::IndexEntry;

use crate::codec::{family_for, is_audio_family, is_video_family, lead_of};
use crate::reader::{Cue, Track};

/// A caption track's cue as `(track, start, end, byte)`.
pub type CaptionSpan = (u64, u64, u64, u64);

/// The longest pre-roll an audio track may go without beside linked video, its restart then the
/// video keyframe's own Cluster: AC3, E-AC3, AAC, DTS and FLAC under it, MP3, Vorbis and Opus
/// over. Audio may then start up to a packet after the picture, the read saved is the whole group
/// before (megabytes, a cold disk seek). An audio only run keeps every lead.
pub const SHORT_PRE_ROLL: u64 = 25_000_000;

/// The first video track the Cues name (RFC 9559 section 22.1), else the first audio track.
pub fn key_track(cues: &[Cue], tracks: &[Track]) -> Option<u64> {
    let cued = |t: &&Track| cues.iter().any(|c| c.track == t.track_number);
    let of =
        |is: fn(&str) -> bool| tracks.iter().filter(|t| is(family_for(&t.codec_id))).find(cued);
    of(is_video_family).or_else(|| of(is_audio_family)).map(|t| t.track_number)
}

pub fn audio_lead(track: &Track, tick: u64) -> u64 {
    let lead = lead_of(track);
    lead.pre_roll.saturating_add(lead.packet).saturating_add(tick)
}

// Cold path, once per stream and link change.
#[allow(clippy::disallowed_methods)]
pub fn cue_entries(
    cues: &[Cue],
    tracks: &[Track],
    linked: &dyn Fn(u64) -> bool,
    tick: u64,
) -> Vec<IndexEntry> {
    let key = key_track(cues, tracks);
    let mut keys: Vec<(u64, u64)> = cues
        .iter()
        .filter(|c| key.is_none_or(|k| c.track == k))
        .map(|c| (c.time_ns, c.byte))
        .collect();
    keys.dedup_by_key(|k| k.0);
    let on = |t: &&Track| linked(t.track_number);
    let video = tracks.iter().filter(on).any(|t| is_video_family(family_for(&t.codec_id)));
    let audio: Vec<(Vec<(u64, u64)>, u64)> = tracks
        .iter()
        .filter(on)
        .filter(|t| is_audio_family(family_for(&t.codec_id)))
        .filter(|t| !video || lead_of(t).pre_roll > SHORT_PRE_ROLL)
        .map(|t| {
            let own: Vec<(u64, u64)> = cues
                .iter()
                .filter(|c| c.track == t.track_number)
                .map(|c| (c.time_ns, c.byte))
                .collect();
            (own, audio_lead(t, tick))
        })
        .collect();
    let (spans, left_out) = captions(cues, tracks);
    let mut captions: Vec<(u64, u64, u64)> = spans
        .iter()
        .filter(|c| linked(c.0))
        .map(|&(_, start, end, byte)| (start, end, byte))
        .collect();
    captions.sort_unstable();
    // A linked caption track the Cues leave out may have a caption up at any key, so every
    // entry goes back the reach for it.
    let uncued = left_out.iter().any(|&t| linked(t));
    let floor =
        |list: &[(u64, u64)], ns: u64| list[..list.partition_point(|p| p.0 <= ns)].last().copied();
    // A caption up at a key, or starting before the next key, restarts at the key below its
    // block. A remux writes a bitmap block by its decode time, ahead of the key of the time it
    // shows at, so the one up at a target past the key may lie before the key's Cluster. The
    // keys are also held ascending by byte, and the captions are swept once in key time order.
    // Both were a scan per key, minutes on a Cues of a few hundred thousand points.
    let mut by_byte: Vec<(u64, u64)> = keys.iter().map(|&(t, b)| (b, t)).collect();
    by_byte.sort_unstable();
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|&i| keys[i].0);
    let mut text: Vec<Option<(u64, u64)>> = vec![None; keys.len()];
    let mut up: BinaryHeap<Reverse<(u64, u64, u64)>> = BinaryHeap::new();
    let mut next = 0;
    for (n, &i) in order.iter().enumerate() {
        let time = keys[i].0;
        let until = order.get(n + 1).map_or(u64::MAX, |&k| keys[k].0);
        while let Some(&(start, end, byte)) = captions.get(next).filter(|c| c.0 < until) {
            up.push(Reverse((byte, start, end)));
            next += 1;
        }
        let reach = time.saturating_sub(CAPTION_REACH);
        // Gone or out of reach stays so, the keys only move forward.
        while let Some(Reverse((_, start, end))) = up.peek()
            && (*start < reach || *end <= time)
        {
            up.pop();
        }
        text[i] = up.peek().and_then(|Reverse((block, _, _))| {
            let below = by_byte.partition_point(|k| k.0 <= *block);
            by_byte[..below].last().map(|&(b, t)| (t, b))
        });
    }
    keys.iter()
        .zip(text)
        .map(|(&(time, byte), text)| {
            let mut earliest = (time, byte);
            for (own, lead) in &audio {
                let list = if own.is_empty() { &keys[..] } else { &own[..] };
                if let Some(at) = floor(list, time.saturating_sub(*lead)) {
                    earliest = earliest.min(at);
                }
            }
            if let Some(at) = text {
                earliest = earliest.min(at);
            }
            if uncued && let Some(at) = floor(&keys, time.saturating_sub(CAPTION_REACH)) {
                earliest = earliest.min(at);
            }
            let (restart_ns, restart) = if earliest.0 < time { earliest } else { (time, byte) };
            IndexEntry { time_ns: time, byte, restart, restart_ns }
        })
        .collect()
}

fn is_caption(t: &Track) -> bool {
    !is_audio_family(family_for(&t.codec_id)) && !is_video_family(family_for(&t.codec_id))
}

/// Every cue of every caption track as `(track, start, end, byte)` in cue order, and the
/// caption tracks the Cues leave out. A cue without a duration, a bitmap composition, is up
/// until its track's next cue.
// Cold path, once per stream and link change.
#[allow(clippy::disallowed_methods)]
pub fn captions(cues: &[Cue], tracks: &[Track]) -> (Vec<CaptionSpan>, Vec<u64>) {
    let (mut spans, mut left_out) = (Vec::new(), Vec::new());
    for t in tracks.iter().filter(|t| is_caption(t)) {
        let own: Vec<&Cue> = cues.iter().filter(|c| c.track == t.track_number).collect();
        if own.is_empty() {
            left_out.push(t.track_number);
        }
        for (i, c) in own.iter().enumerate() {
            let end = match c.duration_ns {
                Some(d) => c.time_ns.saturating_add(d),
                None => {
                    own.get(i + 1).map_or(c.time_ns.saturating_add(CAPTION_REACH), |n| n.time_ns)
                }
            };
            spans.push((t.track_number, c.time_ns, end.max(c.time_ns), c.byte));
        }
    }
    (spans, left_out)
}

pub(crate) fn segment_end(header: &[u8]) -> Option<u64> {
    let mut at = 0usize;
    loop {
        let h = crate::ebml::read_element_header(header, at).ok()?;
        if h.id == crate::ebml::id::SEGMENT {
            return h.size.and_then(|size| size.checked_add(h.data_start as u64));
        }
        at = h.data_end().ok()??.max(at + 1);
    }
}

pub const MAX_GROWN: usize = 1 << 18;

#[derive(Default)]
pub(crate) struct Grown {
    pub(crate) key: Option<u64>,
    cluster: Option<u64>,
    pub(crate) entries: Vec<IndexEntry>,
    pub(crate) published: usize,
}

pub(crate) const GROWN_EVERY: usize = 64;

impl Grown {
    pub(crate) fn cluster(&mut self, byte: u64) {
        self.cluster = Some(byte);
    }

    pub(crate) fn block(&mut self, track: u64, pts_ns: u64, keyframe: bool) -> bool {
        if !keyframe || self.key != Some(track) {
            return false;
        }
        let Some(byte) = self.cluster.take() else {
            return false;
        };
        let at = self.entries.partition_point(|e| e.time_ns < pts_ns);
        let after = self.entries.get(at).is_some_and(|e| e.time_ns == pts_ns || e.byte <= byte);
        let before =
            at.checked_sub(1).and_then(|n| self.entries.get(n)).is_some_and(|e| e.byte >= byte);
        if after || before || self.entries.len() >= MAX_GROWN {
            return false;
        }
        self.entries.insert(at, IndexEntry::key(pts_ns, byte));
        // Every publish clones the whole index, so the step grows with it, or a long walk of tiny
        // Clusters copies it quadratically.
        self.entries.len() >= self.published + GROWN_EVERY.max(self.published / 2)
    }

    pub(crate) fn unpublished(&self) -> bool {
        self.entries.len() != self.published
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn track(track_number: u64, codec: &str) -> Track {
        Track {
            track_number,
            codec_id: codec.into(),
            codec_private: Vec::new(),
            codec_delay_ns: 0,
            seek_pre_roll_ns: 0,
            language: String::new(),
            name: String::new(),
            flag_default: true,
            flag_forced: false,
            disposition: Default::default(),
            sampling_frequency: 48_000.0,
            channels: 2,
            bit_depth: 0,
            pixel_width: 0,
            pixel_height: 0,
            display_width: 0,
            display_height: 0,
            display_unit: 0,
            rotation: 0,
            uid: 0,
            bitrate: 0,
            colour_matrix: 2,
            colour_range: 0,
            colour_transfer: 2,
            colour_primaries: 2,
            hdr: Default::default(),
            chroma_siting: (0, 0),
            dovi: None,
            t35_add_id: 4,
            default_duration_ns: None,
            coding: Default::default(),
        }
    }

    fn cue(track: u64, time_ns: u64, byte: u64, duration_ns: Option<u64>) -> Cue {
        Cue { time_ns, track, byte, duration_ns, relative: None }
    }

    fn film(audio_cued: bool) -> (Vec<Cue>, Vec<Track>) {
        let mut cues: Vec<Cue> =
            (0..8).map(|n| cue(1, n * 2 * S, 1_000 + n * 1_000, None)).collect();
        if audio_cued {
            cues.extend((0..16).map(|n| cue(2, n * S, 1_000 + n * 500, None)));
        }
        cues.sort_by_key(|c| c.time_ns);
        (cues, vec![track(1, "V_VP9"), track(2, "A_OPUS")])
    }

    fn restarts(entries: &[IndexEntry]) -> Vec<(u64, u64)> {
        entries.iter().map(|e| (e.restart, e.restart_ns / (S / 10))).collect()
    }

    #[test]
    fn the_entries_are_the_video_keyframes_and_the_audio_restarts_the_entry_before() {
        let (cues, tracks) = film(false);
        let lead = audio_lead(&tracks[1], 1_000_000);
        assert!(lead > 0 && lead < S, "{lead}");
        let entries = cue_entries(&cues, &tracks, &|n| n == 2, 1_000_000);
        let keys: Vec<(u64, u64)> = entries.iter().map(|e| (e.time_ns, e.byte)).collect();
        assert_eq!(keys, (0..8).map(|n| (n * 2 * S, 1_000 + n * 1_000)).collect::<Vec<_>>());
        let want: Vec<(u64, u64)> = (0..8u64)
            .map(|n| (1_000 + n.saturating_sub(1) * 1_000, n.saturating_sub(1) * 20))
            .collect();
        assert_eq!(restarts(&entries), want);
        let silent = cue_entries(&cues, &tracks, &|_| false, 1_000_000);
        assert!(
            silent.iter().all(|e| (e.restart, e.restart_ns) == (e.byte, e.time_ns)),
            "{silent:?}"
        );
    }

    /// Beside linked video an audio track with a short pre-roll restarts at the keyframe, one
    /// with a long one and an audio only run go back for their lead.
    #[test]
    fn short_pre_roll_audio_beside_video_restarts_at_the_keyframe() {
        let (cues, mut tracks) = film(false);
        let keys: Vec<(u64, u64)> = (0..8u64).map(|n| (1_000 + n * 1_000, n * 20)).collect();
        for codec in ["A_AC3", "A_EAC3", "A_AAC", "A_DTS", "A_FLAC"] {
            tracks[1] = track(2, codec);
            let entries = cue_entries(&cues, &tracks, &|_| true, 1_000_000);
            assert_eq!(restarts(&entries), keys, "{codec} beside video");
            let alone = cue_entries(&cues, &tracks, &|n| n == 2, 1_000_000);
            assert_ne!(restarts(&alone), keys, "{codec} alone keeps its lead");
        }
        for codec in ["A_OPUS", "A_MPEG/L3", "A_VORBIS"] {
            tracks[1] = track(2, codec);
            let entries = cue_entries(&cues, &tracks, &|_| true, 1_000_000);
            assert_ne!(restarts(&entries), keys, "{codec} keeps its pre-roll");
        }
    }

    #[test]
    fn an_audio_track_with_cues_of_its_own_restarts_at_its_cue() {
        let (cues, tracks) = film(true);
        let entries = cue_entries(&cues, &tracks, &|n| n == 2, 1_000_000);
        assert_eq!(entries.len(), 8, "the audio's cues are no entries");
        let want: Vec<(u64, u64)> = (0..8u64)
            .map(|n| match n {
                0 => (1_000, 0),
                n => (1_000 + (2 * n - 1) * 500, (2 * n - 1) * 10),
            })
            .collect();
        assert_eq!(restarts(&entries), want);
    }

    /// The Cues say nothing of what is up on a caption track they leave out, so every entry
    /// goes back the reach for it, and none while the track is not linked.
    #[test]
    fn an_uncued_caption_track_restarts_the_reach_before_every_entry() {
        let cues: Vec<Cue> = (0..20).map(|n| cue(1, n * 2 * S, 1_000 + n * 1_000, None)).collect();
        let tracks = vec![track(1, "V_VP9"), track(2, "A_OPUS"), track(3, "S_TEXT/UTF8")];
        let entries = cue_entries(&cues, &tracks, &|n| n == 3, 1_000_000);
        let want: Vec<(u64, u64)> = (0..20u64)
            .map(|n| {
                let k = (n * 2 * S).saturating_sub(CAPTION_REACH) / (2 * S);
                (1_000 + k * 1_000, k * 20)
            })
            .collect();
        assert_eq!(restarts(&entries), want);
        let unlinked = cue_entries(&cues, &tracks, &|n| n == 1, 1_000_000);
        assert!(unlinked.iter().all(|e| (e.restart, e.restart_ns) == (e.byte, e.time_ns)));
    }

    #[test]
    fn a_caption_on_screen_restarts_at_the_entry_below_its_block() {
        let (mut cues, mut tracks) = film(false);
        tracks.push(track(3, "S_TEXT/UTF8"));
        cues.push(cue(3, 55 * S / 10, 3_400, Some(4 * S)));
        cues.push(cue(3, 11 * S, 6_100, Some(60 * S)));
        cues.sort_by_key(|c| c.time_ns);
        let entries = cue_entries(&cues, &tracks, &|n| n == 3, 1_000_000);
        assert_eq!(entries.len(), 8, "the captions' cues are no entries");
        let at = |n: usize| (entries[n].restart, entries[n].restart_ns / (S / 10));
        assert_eq!(at(2), (3_000, 40), "4 s, nothing up");
        assert_eq!(at(3), (3_000, 40), "6 s, the caption from 5.5 is up");
        assert_eq!(at(4), (3_000, 40), "8 s, still up");
        assert_eq!(at(5), (6_000, 100), "10 s, gone, the keyframe's own");
        assert_eq!(
            at(6),
            (6_000, 100),
            "12 s, the long one from 11 is up, in the Cluster of the keyframe at ten"
        );
        let hidden = cue_entries(&cues, &tracks, &|_| false, 1_000_000);
        assert_eq!((hidden[3].restart, hidden[3].restart_ns), (4_000, 6 * S));
    }

    /// A caption cue without a duration, a bitmap composition, is up until its track's next cue,
    /// the last one for the reach.
    #[test]
    fn a_caption_cue_without_a_duration_is_up_until_the_next() {
        let (mut cues, mut tracks) = film(false);
        tracks.push(track(3, "S_HDMV/PGS"));
        cues.push(cue(3, 55 * S / 10, 3_400, None));
        cues.push(cue(3, 9 * S, 5_600, None));
        cues.sort_by_key(|c| c.time_ns);
        let entries = cue_entries(&cues, &tracks, &|n| n == 3, 1_000_000);
        let at = |n: usize| (entries[n].restart, entries[n].restart_ns / (S / 10));
        assert_eq!(at(2), (3_000, 40), "4 s, nothing up");
        assert_eq!(at(3), (3_000, 40), "6 s, the composition from 5.5 is up");
        assert_eq!(at(4), (3_000, 40), "8 s, still up");
        assert_eq!(at(5), (5_000, 80), "10 s, the one from 9 replaced it, under the key at 8");
        assert_eq!(at(7), (5_000, 80), "14 s, the last one is up for the reach");
    }

    /// A remux stores a bitmap block ahead of its time, so a caption starting between a key and
    /// the next may lie before the key's Cluster, and is gone back for from that key on.
    #[test]
    fn a_caption_starting_before_the_next_key_restarts_at_the_key_below_its_block() {
        let (mut cues, mut tracks) = film(false);
        tracks.push(track(3, "S_HDMV/PGS"));
        cues.push(cue(3, 65 * S / 10, 3_400, Some(3 * S)));
        cues.sort_by_key(|c| c.time_ns);
        let entries = cue_entries(&cues, &tracks, &|n| n == 3, 1_000_000);
        let at = |n: usize| (entries[n].restart, entries[n].restart_ns / (S / 10));
        assert_eq!(at(2), (3_000, 40), "4 s, the key below the block is its own");
        assert_eq!(at(3), (3_000, 40), "6 s, the caption from 6.5 is stored before this key");
        assert_eq!(at(4), (3_000, 40), "8 s, still up");
        assert_eq!(at(5), (6_000, 100), "10 s, gone");
    }

    #[test]
    fn a_caption_up_longer_than_the_reach_is_not_gone_back_for() {
        let (mut cues, mut tracks) = film(false);
        tracks.push(track(3, "S_TEXT/UTF8"));
        cues.extend((8..20).map(|n| cue(1, n * 2 * S, 1_000 + n * 1_000, None)));
        cues.push(cue(3, S, 1_500, Some(60 * S)));
        cues.sort_by_key(|c| c.time_ns);
        let entries = cue_entries(&cues, &tracks, &|n| n == 3, 1_000_000);
        assert_eq!(entries[10].restart, 1_000, "20 s up, the reach");
        assert_eq!(entries[11].restart, entries[11].byte, "22 s up, past it");
    }

    /// What a scan per key would take minutes over, half keys and half captions up for long
    /// inside one reach, with the restart checked against the rule stated plainly.
    #[test]
    fn a_cues_of_many_keys_and_captions_indexes_in_one_sweep() {
        const N: u64 = 60_000;
        let mut cues: Vec<Cue> =
            (0..N).map(|n| cue(1, n * S / 10, 1_000 + n * 1_000, None)).collect();
        // Every caption starts within the reach of every later key for a while, bytes shuffled.
        cues.extend((0..N).map(|n| {
            let byte = 1_000 + ((n * 7_919) % N) * 1_000;
            cue(3, n * S / 10 + 1, byte, Some(15 * S))
        }));
        cues.sort_by_key(|c| c.time_ns);
        let tracks = vec![track(1, "V_VP9"), track(3, "S_TEXT/UTF8")];
        let entries = cue_entries(&cues, &tracks, &|n| n == 3, 1_000_000);
        assert_eq!(entries.len(), N as usize);
        let check = |n: u64| {
            let e = &entries[n as usize];
            let time = e.time_ns;
            let reach = time.saturating_sub(CAPTION_REACH);
            let up = cues
                .iter()
                .filter(|c| c.track == 3 && c.time_ns >= reach && c.time_ns <= time)
                .filter(|c| time < c.time_ns + 15 * S)
                .map(|c| c.byte)
                .min();
            let want = up.map_or(e.byte, |block| {
                cues.iter()
                    .filter(|c| c.track == 1 && c.byte <= block)
                    .map(|c| c.byte)
                    .max()
                    .unwrap_or(e.byte)
            });
            assert_eq!(e.restart, want.min(e.byte), "key {n}");
        };
        [0, 1, 99, 1_000, 12_345, N / 2, N - 151, N - 1].into_iter().for_each(check);
    }

    #[test]
    fn audio_alone_is_its_own_index_and_restarts_its_pre_roll_ahead() {
        let tracks = vec![track(2, "A_OPUS")];
        let cues: Vec<Cue> = (0..4).map(|n| cue(2, n * S, 100 + n * 100, None)).collect();
        assert_eq!(key_track(&cues, &tracks), Some(2));
        let entries = cue_entries(&cues, &tracks, &|_| true, 1_000_000);
        assert_eq!(restarts(&entries), [(100, 0), (100, 0), (200, 10), (300, 20)]);
    }

    #[test]
    fn a_grown_index_takes_the_first_keyframe_of_each_cluster_once() {
        let mut g = Grown { key: Some(1), ..Grown::default() };
        g.cluster(100);
        assert!(!g.block(2, 0, true), "another track");
        assert!(!g.block(1, 0, true));
        assert!(!g.block(1, 40, true), "the second keyframe of the Cluster");
        g.cluster(900);
        assert!(!g.block(1, 80, false), "no keyframe yet");
        assert!(!g.block(1, 120, true));
        g.cluster(100);
        assert!(!g.block(1, 0, true));
        assert_eq!(g.entries, [IndexEntry::key(0, 100), IndexEntry::key(120, 900)]);
        g.cluster(5_000);
        g.block(1, 4_000, true);
        g.cluster(2_000);
        g.block(1, 1_000, true);
        let times: Vec<u64> = g.entries.iter().map(|e| e.time_ns).collect();
        assert_eq!(times, [0, 120, 1_000, 4_000]);
        assert!(g.unpublished());
    }

    #[test]
    fn a_grown_index_says_when_to_publish() {
        let mut g = Grown { key: Some(1), ..Grown::default() };
        let said: Vec<bool> = (0..130u64)
            .map(|n| {
                g.cluster(n * 10);
                g.block(1, n, true)
            })
            .collect();
        assert_eq!(
            said.iter().filter(|s| **s).count(),
            67,
            "every one from the 64th on, until published"
        );
        g.published = g.entries.len();
        assert!(!g.unpublished());
        g.published = 100_000;
        g.entries.extend((130..149_998u64).map(|n| IndexEntry::key(n, n * 10)));
        g.cluster(1_500_000);
        assert!(!g.block(1, 150_000, true), "a step of half the published, not 64");
        g.cluster(1_500_010);
        assert!(g.block(1, 150_001, true), "the step reached");
    }
}
