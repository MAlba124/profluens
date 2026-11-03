//! The MP4 demux engine (ISO/IEC 14496-12). [`Mp4Reader::new`] resolves the `moov` sample
//! tables up front, then the whole file is pushed and each sample is sliced out of the
//! retained input chunks by absolute offset. Fragmented movies walk `moof`/`mdat` instead.

use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use profluens_core::ctx::Disposition;
use profluens_core::memory::{Demand, IO_DEPTH_BYTES, Known, Memory};
pub use profluens_core::pipeline::CAPTION_REACH as CAPTION_REACH_NS;
use profluens_core::pipeline::{Caption, Index, IndexEntry};
use profluens_core::time::Timestamp;

use crate::boxes::{self, BoxError, BoxHeader};
use crate::codec::{self, Reframer, SampleEntry};
use crate::edit::{self, EditList, EditRefusal};
use crate::frag::{self, FragmentRef, MAX_FRAGMENT_HEAD, Trex};
use pf_mkv::ReframeError;
use stein_arena::{ArenaError, Grow};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Mp4Error {
    Box(BoxError),
    Missing(&'static str),
    FragmentTooLarge { limit: u64 },
    InitChanged,
    Inconsistent(&'static str),
    NoConfigRecord { entry: boxes::FourCc },
    NotPrerolled,
    SampleDropped { track: usize, cause: ReframeError },
    TooManySamples { limit: usize },
    TooManyTracks { limit: usize },
    Index(ArenaError),
    EditList { track: u32, refused: EditRefusal },
}

/// The most samples one file may describe, about 24 h of 60 fps video and two AAC tracks.
/// Run-length tables let a tiny head declare billions. At this limit the 32 octet per-sample
/// index is 512 MiB, so an embedder that knows the file length passes it in [`Limits`].
pub const MAX_SAMPLES: usize = 1 << 24;

/// What one reader may commit to its sample index, across all its tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The file length when known. Disjoint samples hold at most its octets, so each sample
    /// costs the file an octet and a 76 octet table cannot declare millions. A sample past it
    /// is a truncated file's tail and is left out.
    pub file_len: Option<u64>,
    /// The most samples, capped at [`MAX_SAMPLES`].
    pub samples: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { file_len: None, samples: MAX_SAMPLES }
    }
}

pub const MAX_TRACKS: usize = 256;

/// An audio track has a sync sample every packet, so its index is thinned to this.
pub const MAX_INDEX_ENTRIES: usize = 8_192;

impl std::fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mp4Error::Box(e) => write!(f, "mp4demux: malformed box, {e}"),
            Mp4Error::Missing(what) => write!(f, "mp4demux: required box/table missing, {what}"),
            Mp4Error::FragmentTooLarge { limit } => {
                write!(f, "mp4demux: a moof or an init segment is larger than {limit} octets")
            }
            Mp4Error::InitChanged => {
                f.write_str("mp4demux: an init segment in the stream describes other tracks")
            }
            Mp4Error::Inconsistent(what) => {
                write!(f, "mp4demux: inconsistent sample table, {what}")
            }
            Mp4Error::NoConfigRecord { entry } => write!(
                f,
                "mp4demux passthrough: '{}' entry has no avcC/hvcC record (in-band avc3/hev1 \
                 parameter sets), nothing to hand Matroska as CodecPrivate",
                entry.escape_ascii()
            ),
            Mp4Error::NotPrerolled => {
                f.write_str("mp4demux: started before preroll resolved the tables")
            }
            Mp4Error::SampleDropped { track, cause } => {
                write!(f, "mp4demux: dropping malformed sample on track {track}, {cause}")
            }
            Mp4Error::TooManySamples { limit } => {
                write!(f, "mp4demux: the sample tables describe more than {limit} samples")
            }
            Mp4Error::TooManyTracks { limit } => {
                write!(f, "mp4demux: the movie holds more than {limit} tracks")
            }
            Mp4Error::Index(e) => write!(f, "mp4demux: reserving the sample index, {e}"),
            Mp4Error::EditList { track, refused } => {
                write!(f, "mp4demux: track {track} edit list, {refused}")
            }
        }
    }
}

impl std::error::Error for Mp4Error {}

impl From<BoxError> for Mp4Error {
    fn from(e: BoxError) -> Self {
        Mp4Error::Box(e)
    }
}

/// 32 octets, the index costs that per sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Below [`MAX_TRACKS`].
    pub track_index: u16,
    pub offset: u64,
    pub size: u32,
    /// Decode time in media ticks, moved by the edit list. Signed, an edit that trims the
    /// composition lead makes the first DTS negative (the ffmpeg convention).
    pub dts: i64,
    /// Presentation time in media ticks on the edit list's timeline. Signed.
    pub pts: i64,
    pub sync: bool,
    /// Needed by referring samples but left out by the edit list. Leaves as `DECODE_ONLY`.
    pub hidden: bool,
}

const _: () = assert!(size_of::<Sample>() == 32);

impl Sample {
    pub fn track(&self) -> usize {
        usize::from(self.track_index)
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub track_id: u32,
    pub timescale: u32,
    pub duration_ns: u64,
    /// The first presented edit's `media_time` in media ticks (8.6.6). For audio this is the
    /// encoder's priming. A fragment's samples have it taken off.
    pub media_time: i64,
    pub edit_refused: Option<EditRefusal>,
    pub entry: SampleEntry,
    pub width: u32,
    pub height: u32,
    /// Degrees clockwise the picture is turned to show, from the track header's matrix.
    pub rotation: u16,
    pub language: [u8; 3],
    /// The `udta` name, else the `hdlr` name unless it is a muxer's generic handler word.
    pub title: String,
    pub language_tag: String,
    pub enabled: bool,
    pub alternate_group: i16,
    pub disposition: Disposition,
    pub forced: bool,
    /// Bits a second, the entry's stated avgBitrate, else its sample sizes summed over the
    /// media's duration. 0 for unknown, a fragmented track's samples are not in the head.
    pub bitrate: u64,
}

impl Track {
    pub fn family(&self) -> &'static str {
        self.entry.family
    }

    pub fn codec_head(&self) -> &[u8] {
        &self.entry.codec_head
    }

    pub fn reframer(&self) -> &Reframer {
        &self.entry.reframer
    }

    /// The delay an audio track declares, in media ticks, and zero for any other. Audio is
    /// stamped in media time with the priming declared as delay, like Matroska's `CodecDelay`
    /// (RFC 9559 5.1.4.1.25), and the decoder cuts what falls before zero.
    pub fn delay_ticks(&self) -> i64 {
        match self.entry.family {
            "aac" | "opus" | "ac3" | "eac3" | "mp3" | "flac" => self.media_time.max(0),
            _ => 0,
        }
    }

    pub fn stamp_ns(&self, pts: i64) -> u64 {
        ticks_to_ns(pts.saturating_add(self.delay_ticks()), self.timescale)
    }

    pub fn is_text(&self) -> bool {
        self.entry.family.starts_with("subtitle/")
    }

    pub fn kind(&self) -> TrackKind {
        track_kind(self.entry.family)
    }

    /// The decoder pre-roll ahead of a seek target in ns, from `profluens_audio::RESTARTS`.
    pub fn pre_roll_ns(&self) -> u64 {
        pre_roll_ns(self.entry.family, self.entry.sample_rate, self.timescale)
    }
}

/// At the sample entry's rate, else the media timescale, else 48 kHz, since `lead_of` takes a
/// rate of 0 for 1 Hz and an AAC track stating neither would restart 1024 s early.
fn pre_roll_ns(family: &str, sample_rate: u32, timescale: u32) -> u64 {
    let rate = [sample_rate, timescale].into_iter().find(|r| *r != 0).unwrap_or(48_000);
    profluens_audio::lead_of(family, u64::from(rate), 0, None).pre_roll
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemuxPool {
    pub bytes: u64,
    pub largest: u64,
    pub held: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Restart {
    pub to_byte: u64,
    pub at_ns: u64,
}

pub struct Tables {
    tracks: Vec<Track>,
    /// Every sample across all tracks, sorted by file offset, so streaming is a forward walk.
    samples: SampleIndex,
    rules: Vec<Rule>,
    frag: Option<FragTables>,
    /// Tracks left out for a missing sample table.
    skipped: u32,
}

struct FragTables {
    trex: Vec<Trex>,
    fragments: Vec<FragmentRef>,
    start: u64,
    duration_ns: u64,
}

impl std::fmt::Debug for Tables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tables {{ tracks: {}, samples: {} }}", self.tracks.len(), self.samples.len())
    }
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

/// The file octets left for samples. Disjoint samples inside a file hold at most its length.
#[derive(Clone, Copy)]
struct Room {
    end: u64,
    left: u64,
}

impl Default for Room {
    fn default() -> Self {
        Room { end: u64::MAX, left: u64::MAX }
    }
}

impl Room {
    fn of(file_len: Option<u64>) -> Self {
        file_len.map_or(Room::default(), |end| Room { end, left: end })
    }

    /// False for a sample past the end, a truncated file's tail. Past the octets left, the
    /// samples overlap, so the index never holds more samples than the file has octets.
    fn admits(&mut self, offset: u64, size: u32) -> Result<bool, Mp4Error> {
        if offset.saturating_add(u64::from(size)) > self.end {
            return Ok(false);
        }
        self.left = self
            .left
            .checked_sub(u64::from(size))
            .ok_or(Mp4Error::Inconsistent("samples overlap in the file"))?;
        Ok(true)
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

#[derive(Clone, Copy)]
struct Rule {
    kind: TrackKind,
    timescale: u32,
    delay_ticks: i64,
    pre_roll_ns: u64,
    media_time: i64,
}

#[derive(Clone, Copy)]
struct Pick {
    best: (u64, i64),
    first: (u64, i64),
    bound: i64,
    done: bool,
}

/// The largest tick count [`ticks_to_ns`] stamps at or before `ns`, compared in ticks
/// to keep the division out of a scan of every sample.
fn ticks_at_or_before(ns: u64, timescale: u32) -> i64 {
    if timescale == 0 {
        return ns.min(i64::MAX as u64) as i64;
    }
    let ticks = ((u128::from(ns) + 1) * u128::from(timescale) - 1) / 1_000_000_000;
    ticks.min(i64::MAX as u128) as i64
}

impl Tables {
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Tracks of the movie left out for a missing sample table, chapter and metadata ones often.
    pub fn skipped_tracks(&self) -> u32 {
        self.skipped
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// Octets the sample index has committed, off the heap in its own mapping.
    pub fn index_bytes(&self) -> usize {
        self.samples.committed()
    }

    // Cold path, once per open.
    #[allow(clippy::disallowed_methods)]
    pub fn demand(&self, track: usize, window: u32) -> Demand {
        if self.frag.is_some() {
            return Demand::unknown();
        }
        let k = window.max(1) as usize;
        let mut ring = vec![0u64; k];
        let (mut at, mut sum, mut peak, mut largest) = (0usize, 0u64, 0u64, 0u64);
        for s in self.samples.iter().filter(|s| s.track() == track) {
            let size = u64::from(s.size);
            sum = sum - ring[at] + size;
            ring[at] = size;
            at = (at + 1) % k;
            (peak, largest) = (peak.max(sum), largest.max(size));
        }
        Demand { known: Known::Samples, largest, peak, window: k as u32 }
    }

    /// A passthrough remuxer's demuxer pool for `tracks`, sized for the muxer's open Cluster
    /// and the range reads in flight, with a quarter extra for class rounding.
    pub fn remux_pool(&self, tracks: &[usize]) -> RemuxPool {
        let demand = tracks.iter().fold(Demand::unknown(), |d, &i| d.and(self.demand(i, 1)));
        let largest = demand.largest.max(1);
        let bytes = (pf_mkv::CLUSTER_BYTES_MAX + IO_DEPTH_BYTES + 2 * largest) * 5 / 4;
        let held = tracks.iter().map(|&i| self.most_within(i, bytes)).max().unwrap_or(0);
        RemuxPool { bytes, largest, held }
    }

    pub fn most_within(&self, track: usize, bytes: u64) -> u32 {
        let mut of = self.samples.iter().filter(|s| s.track() == track).map(|s| u64::from(s.size));
        let mut behind = of.clone();
        let (mut sum, mut n, mut most) = (0u64, 0u32, 0u32);
        for size in of.by_ref() {
            (sum, n) = (sum + size, n + 1);
            while sum > bytes {
                sum -= behind.next().unwrap_or(0);
                n -= 1;
            }
            most = most.max(n);
        }
        most
    }

    /// Where a seek to `point` restarts the file, video restarting at the sync sample at or
    /// before `video_from`, a caption track `on` at the sample up at `point`. The byte is the
    /// first any track needs.
    pub fn restart(&self, point: u64, video_from: u64, on: &dyn Fn(usize) -> bool) -> Restart {
        if let Some(frag) = &self.frag {
            return frag.restart(point, video_from, &self.rules);
        }
        let mut gates = [0u64; MAX_TRACKS];
        self.gates(Some(point), video_from, 0, on, &mut gates)
    }

    /// `track` is a caption track the restart goes back for.
    fn caption(&self, track: usize, on: &dyn Fn(usize) -> bool) -> bool {
        let text = |t: &Track| t.kind() == TrackKind::Other && t.is_text();
        self.tracks.get(track).is_some_and(text) && on(track)
    }

    /// A caption sample stamped `pts` is still up at `point`, within the reach.
    fn caption_up(&self, track: usize, point: u64, pts: i64) -> bool {
        let up = |t: &Track| point.saturating_sub(t.stamp_ns(pts)) <= CAPTION_REACH_NS;
        self.tracks.get(track).is_some_and(up)
    }

    pub fn is_fragmented(&self) -> bool {
        self.frag.is_some()
    }

    pub fn span(&self, index: usize) -> Option<i64> {
        let track = self.tracks.get(self.samples.get(index)?.track())?;
        if !track.is_text() {
            return None;
        }
        span_in(&self.samples, index)
    }

    pub fn fragments(&self) -> &[FragmentRef] {
        self.frag.as_ref().map_or(&[], |frag| &frag.fragments)
    }

    pub fn fragmented_duration_ns(&self) -> u64 {
        self.frag.as_ref().map_or(0, |frag| frag.duration_ns)
    }

    /// The time to byte map of the movie. Sync samples of the first
    /// video track, else the first audio track thinned to [`MAX_INDEX_ENTRIES`], or one entry
    /// per indexed fragment. An entry restarts ahead for audio pre-roll and for the sample up
    /// on a caption track `on`.
    // Cold path, once per open and link change.
    #[allow(clippy::disallowed_methods)]
    pub fn seek_index(&self, on: &dyn Fn(usize) -> bool) -> Index {
        let longest = self.tracks.iter().map(|t| t.duration_ns).max().unwrap_or(0);
        let ns = match (longest, &self.frag) {
            (0, Some(frag)) => frag.duration_ns,
            _ => longest,
        };
        let duration = if ns > 0 { Timestamp(ns) } else { Timestamp::NONE };
        let of = |kind| self.tracks.iter().position(|t| t.kind() == kind);
        let Some(track) = of(TrackKind::Video).or(of(TrackKind::Audio)) else {
            return Index { duration, ..Index::default() };
        };
        let (stream, grouped) = (Some(track as u32), self.tracks[track].kind() == TrackKind::Video);
        let captions = self.captions();
        let mut entries: Vec<IndexEntry> = Vec::new();
        let ascends = |entries: &[IndexEntry], time_ns: u64, byte: u64| {
            entries.last().is_none_or(|last| time_ns > last.time_ns && byte >= last.byte)
        };
        if let Some(frag) = &self.frag {
            let first =
                frag.fragments.is_empty().then_some(FragmentRef { time_ns: 0, offset: frag.start });
            for f in frag.fragments.iter().copied().chain(first) {
                if ascends(&entries, f.time_ns, f.offset) {
                    let r = frag.restart(f.time_ns, f.time_ns, &self.rules);
                    entries.push(IndexEntry {
                        time_ns: f.time_ns,
                        byte: f.offset,
                        restart: r.to_byte,
                        restart_ns: r.at_ns,
                    });
                }
            }
            return Index {
                complete: true,
                entries,
                file_len: None,
                duration,
                stream,
                grouped,
                captions,
                ..Index::default()
            };
        }
        let keys = |s: &&Sample| s.track() == track && s.sync;
        let stride = self.samples.iter().filter(keys).count().div_ceil(MAX_INDEX_ENTRIES).max(1);
        let mut sweep = Sweep { tables: self, cursors: vec![Swept::default(); self.tracks.len()] };
        for s in self.samples.iter().filter(keys).step_by(stride) {
            let time_ns = self.tracks[track].stamp_ns(s.pts);
            if ascends(&entries, time_ns, s.offset) {
                let r = sweep.restart(time_ns, on);
                entries.push(IndexEntry {
                    time_ns,
                    byte: s.offset,
                    restart: r.to_byte,
                    restart_ns: r.at_ns,
                });
            }
        }
        Index {
            complete: !entries.is_empty(),
            entries,
            file_len: None,
            duration,
            stream,
            grouped,
            captions,
            ..Index::default()
        }
    }

    /// Every sample of every text track as a caption up until the track's next, the stream its
    /// track index, ascending by time.
    // Cold path, once per open and link change.
    #[allow(clippy::disallowed_methods)]
    fn captions(&self) -> Vec<Caption> {
        let mut captions: Vec<Caption> = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            if t.kind() != TrackKind::Other || !t.is_text() {
                continue;
            }
            let own = self.samples.iter().filter(|s| s.track() == i);
            let first = captions.len();
            for s in own {
                let time_ns = t.stamp_ns(s.pts);
                let end_ns = time_ns.saturating_add(CAPTION_REACH_NS);
                if let Some(last) = captions.get_mut(first..).and_then(<[Caption]>::last_mut) {
                    last.end_ns = time_ns.max(last.time_ns);
                }
                captions.push(Caption { stream: i as u32, time_ns, end_ns, byte: s.offset });
            }
        }
        captions.sort_unstable_by_key(|c| c.time_ns);
        captions
    }

    pub fn track_restart(&self, track: usize, point: u64, on: &dyn Fn(usize) -> bool) -> Restart {
        if let Some(frag) = &self.frag {
            return frag.restart(point, point, &self.rules);
        }
        let mut gates = [0u64; MAX_TRACKS];
        self.gates(Some(point), point, 0, on, &mut gates);
        let to_byte = gates.get(track).copied().unwrap_or(0);
        let at = self.samples.partition_point(|s| s.offset < to_byte);
        let sample = self.samples[at..].iter().find(|s| s.track() == track);
        let stamp = sample.zip(self.tracks.get(track)).map(|(s, t)| t.stamp_ns(s.pts));
        Restart { to_byte, at_ns: stamp.unwrap_or(point) }
    }

    /// The offset each track's samples pass from, into `gates`, behind a source that
    /// restarted at `floor`. Video from the last sync sample at or before `video_from`,
    /// audio from its pre-roll ahead of `point`, a caption track `on` from its sample up at
    /// `point`, anything else whole. One walk in file order.
    fn gates(
        &self,
        point: Option<u64>,
        video_from: u64,
        floor: u64,
        on: &dyn Fn(usize) -> bool,
        gates: &mut [u64; MAX_TRACKS],
    ) -> Restart {
        const NONE: (u64, i64) = (u64::MAX, 0);
        let mut picks = [Pick { best: NONE, first: NONE, bound: 0, done: true }; MAX_TRACKS];
        let mut open = 0usize;
        for (i, (pick, rule)) in picks.iter_mut().zip(&self.rules).enumerate() {
            let bound_ns = match (rule.kind, point) {
                (TrackKind::Video, _) => video_from,
                (TrackKind::Audio, Some(point)) => point.saturating_sub(rule.pre_roll_ns),
                (TrackKind::Other, Some(point)) if self.caption(i, on) => point,
                _ => continue,
            };
            (pick.bound, pick.done) = (ticks_at_or_before(bound_ns, rule.timescale), false);
            open += 1;
        }
        let from = self.samples.partition_point(|s| s.offset < floor);
        for s in &self.samples[from..] {
            if open == 0 {
                break;
            }
            let (pick, rule) = (&mut picks[s.track()], &self.rules[s.track()]);
            if pick.done || (rule.kind == TrackKind::Video && !s.sync) {
                continue;
            }
            if pick.first == NONE {
                pick.first = (s.offset, s.pts);
            }
            // A stamp before zero is stamped zero.
            if s.pts.saturating_add(rule.delay_ticks).max(0) <= pick.bound {
                pick.best = (s.offset, s.pts);
            } else {
                pick.done = true;
                open -= 1;
            }
        }
        let mut restart = Restart { to_byte: u64::MAX, at_ns: u64::MAX };
        for (i, ((gate, pick), track)) in gates.iter_mut().zip(&picks).zip(&self.tracks).enumerate()
        {
            let gated = match (track.kind(), point) {
                (TrackKind::Other, Some(point)) => {
                    pick.best != NONE && self.caption_up(i, point, pick.best.1)
                }
                (TrackKind::Other, None) | (TrackKind::Audio, None) => false,
                _ => true,
            };
            let (offset, pts) = match (gated, pick.best != NONE) {
                (false, _) => (floor, None),
                (true, true) => (pick.best.0, Some(pick.best.1)),
                (true, false) => (pick.first.0, (pick.first != NONE).then_some(pick.first.1)),
            };
            *gate = offset;
            if let Some(pts) = pts {
                restart.to_byte = restart.to_byte.min(offset);
                restart.at_ns = restart.at_ns.min(track.stamp_ns(pts));
            }
        }
        if restart.to_byte == u64::MAX {
            restart = Restart { to_byte: floor, at_ns: point.unwrap_or(video_from) };
        }
        restart
    }
}

#[derive(Clone, Copy, Default)]
struct Swept {
    at: usize,
    best: Option<(u64, i64)>,
    first: Option<(u64, i64)>,
}

struct Sweep<'a> {
    tables: &'a Tables,
    cursors: Vec<Swept>,
}

impl Sweep<'_> {
    fn restart(&mut self, point: u64, on: &dyn Fn(usize) -> bool) -> Restart {
        let tables = self.tables;
        let mut restart = Restart { to_byte: u64::MAX, at_ns: u64::MAX };
        for (i, ((rule, track), cursor)) in
            tables.rules.iter().zip(&tables.tracks).zip(&mut self.cursors).enumerate()
        {
            let bound_ns = match rule.kind {
                TrackKind::Video => point,
                TrackKind::Audio => point.saturating_sub(rule.pre_roll_ns),
                TrackKind::Other if tables.caption(i, on) => point,
                TrackKind::Other => continue,
            };
            let bound = ticks_at_or_before(bound_ns, rule.timescale);
            while let Some(s) = tables.samples.get(cursor.at) {
                if s.track() != i || (rule.kind == TrackKind::Video && !s.sync) {
                    cursor.at += 1;
                    continue;
                }
                cursor.first.get_or_insert((s.offset, s.pts));
                // A stamp before zero is stamped zero.
                if s.pts.saturating_add(rule.delay_ticks).max(0) > bound {
                    break;
                }
                cursor.best = Some((s.offset, s.pts));
                cursor.at += 1;
            }
            // A caption is up from its sample on, a later one is not
            let pick = match rule.kind {
                TrackKind::Other => {
                    cursor.best.filter(|&(_, pts)| tables.caption_up(i, point, pts))
                }
                _ => cursor.best.or(cursor.first),
            };
            if let Some((offset, pts)) = pick {
                restart.to_byte = restart.to_byte.min(offset);
                restart.at_ns = restart.at_ns.min(track.stamp_ns(pts));
            }
        }
        if restart.to_byte == u64::MAX {
            restart = Restart { to_byte: 0, at_ns: point };
        }
        restart
    }
}

impl FragTables {
    fn restart(&self, point: u64, video_from: u64, rules: &[Rule]) -> Restart {
        let audio = rules.iter().filter(|rule| rule.kind == TrackKind::Audio);
        let lead = audio.map(|rule| rule.pre_roll_ns).max().unwrap_or(0);
        let bound = point.saturating_sub(lead).min(video_from);
        let behind = self.fragments.partition_point(|f| f.time_ns <= bound);
        match behind.checked_sub(1).and_then(|at| self.fragments.get(at)) {
            Some(f) => Restart { to_byte: f.offset, at_ns: f.time_ns },
            None => Restart { to_byte: self.start, at_ns: 0 },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    Open,
    Shut,
    NextSync,
    From { bound_ns: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Box,
    Gather { moov: bool, start: u64, end: u64 },
    Mdat { end: u64 },
    Lost,
}

/// The walk of a fragmented stream (ISO/IEC 14496-12 8.8). A `moof` is gathered whole and
/// waits for its `mdat`, since only the `mdat` bounds what the `moof` may claim.
struct Walk {
    step: Step,
    at: u64,
    head: Vec<u8>,
    moof: Option<u64>,
    run: Vec<Sample>,
    next: usize,
    next_dts: Vec<u64>,
    gates: Vec<Gate>,
    /// Nanoseconds taken off every stamp, so a segment's media time lands on the stream time
    /// its source named.
    shift: i64,
    anchor: Option<u64>,
    epoch: u32,
    marked: bool,
    error: Option<Mp4Error>,
}

pub struct Mp4Reader {
    tables: Arc<Tables>,
    next: usize,
    stream_base: u64,
    chunks: VecDeque<Chunk>,
    buffered: u64,
    gates: [u64; MAX_TRACKS],
    off: [bool; MAX_TRACKS],
    walk: Option<Walk>,
}

const SHUT: u64 = u64::MAX;

enum Chunk {
    Retained(Memory),
    Owned(Vec<u8>),
}

impl Chunk {
    fn len(&self) -> usize {
        match self {
            Chunk::Retained(m) => m.len(),
            Chunk::Owned(v) => v.len(),
        }
    }

    fn data(&self) -> &[u8] {
        match self {
            Chunk::Retained(m) => m.data(),
            Chunk::Owned(v) => v,
        }
    }
}

/// Smaller input chunks are copied instead of retained. Retaining pins the whole pool
/// slot, so a source dribbling tiny buffers would starve its pool.
const RETAIN_MIN: usize = 4096;

const OWNED_CHUNK_CAP: usize = 64 * 1024;

impl Mp4Reader {
    /// Resolve a movie from its head, which covers `ftyp` and the whole `moov`. A `moov`
    /// behind the `mdat` may be appended to the bytes before the `mdat`, as offsets are absolute.
    pub fn new(head: &[u8]) -> Result<Self, Mp4Error> {
        Self::bounded(head, &[], Limits::default())
    }

    pub fn indexed(head: &[u8], mfra: &[u8]) -> Result<Self, Mp4Error> {
        Self::bounded(head, mfra, Limits::default())
    }

    /// As [`Self::indexed`], the sample index held to `limits`.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn bounded(head: &[u8], mfra: &[u8], limits: Limits) -> Result<Self, Mp4Error> {
        let moov = find_moov(head)?.ok_or(Mp4Error::Missing("moov"))?;
        let movie = resolve_moov(moov.body(head), Some(limits))?;
        let rules = rules_of(&movie.tracks);
        let frag = movie.trex.map(|trex| {
            let mut fragments = Vec::new();
            let indexed_to = index_head(head, moov.end, &mut fragments);
            index_mfra(mfra, &movie.tracks, &mut fragments);
            fragments.sort_unstable();
            fragments.dedup_by_key(|f| f.offset);
            let duration_ns = if movie.duration_ns != 0 { movie.duration_ns } else { indexed_to };
            FragTables { trex, fragments, start: moov.end as u64, duration_ns }
        });
        let walk = frag.as_ref().map(|_| Walk::new(movie.tracks.len()));
        let skipped = movie.skipped;
        let tables = Tables { tracks: movie.tracks, samples: movie.samples, rules, frag, skipped };
        let mut reader = Self {
            tables: Arc::new(tables),
            next: 0,
            stream_base: 0,
            chunks: VecDeque::new(),
            buffered: 0,
            gates: [0; MAX_TRACKS],
            off: [false; MAX_TRACKS],
            walk,
        };
        reader.reset_stream();
        Ok(reader)
    }
    pub fn tracks(&self) -> &[Track] {
        &self.tables.tracks
    }

    pub fn samples(&self) -> &[Sample] {
        &self.tables.samples
    }

    pub fn tables(&self) -> Arc<Tables> {
        Arc::clone(&self.tables)
    }

    pub fn reset_stream(&mut self) {
        self.next = 0;
        self.stream_base = 0;
        self.chunks.clear();
        self.buffered = 0;
        self.gates = self.off.map(|off| if off { SHUT } else { 0 });
        let Self { walk, tables, off, .. } = self;
        if let Some(walk) = walk {
            walk.restart(0);
            (walk.shift, walk.anchor) = (0, None);
            for (gate, off) in walk.gates.iter_mut().zip(off.iter()) {
                *gate = if *off { Gate::Shut } else { Gate::Open };
            }
            // A movie may keep its first samples in its `moov` and the rest in fragments.
            let on = tables.samples.iter().filter(|s| !off[s.track()]);
            walk.run.extend(on.copied());
        }
    }

    /// Whether `track` is selected, every track is until deselected.
    pub fn is_on(&self, track: usize) -> bool {
        self.off.get(track).is_some_and(|off| !off)
    }

    pub fn select(&mut self, track: usize, on: bool) {
        let Some(off) = self.off.get_mut(track) else {
            return;
        };
        *off = !on;
        if let Some(gate) = self.walk.as_mut().and_then(|walk| walk.gates.get_mut(track)) {
            *gate = if on { Gate::NextSync } else { Gate::Shut };
            return;
        }
        let video = self.tables.rules.get(track).is_some_and(|rule| rule.kind == TrackKind::Video);
        let next = |s: &&Sample| s.track() == track && (s.sync || !video);
        let from = self.tables.samples.get(self.next..).and_then(|rest| rest.iter().find(next));
        self.gates[track] = if on { from.map_or(SHUT, |s| s.offset) } else { SHUT };
    }

    pub fn rejoin(&mut self, track: usize, to_byte: u64, point: u64) {
        if track >= MAX_TRACKS {
            return;
        }
        if self.walk.is_some() {
            return self.select(track, true);
        }
        let resume = self.tables.samples.get(self.next).map_or(SHUT, |s| s.offset);
        self.gates.iter_mut().for_each(|gate| *gate = (*gate).max(resume));
        let mut fresh = [0u64; MAX_TRACKS];
        let off = self.off;
        self.tables.gates(Some(point), point, to_byte, &|i| !off[i], &mut fresh);
        (self.gates[track], self.off[track]) = (fresh[track], false);
        self.chunks.clear();
        self.buffered = 0;
        self.stream_base = to_byte;
        self.next = self.tables.samples.partition_point(|s| s.offset < to_byte);
    }

    /// Follow a source that restarted at `to_byte` for a seek to `point`. Each track passes
    /// from its own sample, see [`Tables::restart`]. A fragmented stream gates by stamps as
    /// fragments arrive, and `to_byte` has to be the first byte of a box.
    pub fn seek_stream(&mut self, to_byte: u64, point: Option<u64>, video_from: u64) -> Restart {
        self.chunks.clear();
        self.buffered = 0;
        self.stream_base = to_byte;
        if let Some(walk) = self.walk.as_mut() {
            walk.restart(to_byte);
            for ((gate, rule), off) in walk.gates.iter_mut().zip(&self.tables.rules).zip(self.off) {
                *gate = match (rule.kind, point, off) {
                    (_, _, true) => Gate::Shut,
                    (TrackKind::Video, _, _) => {
                        Gate::From { bound_ns: point.unwrap_or(video_from) }
                    }
                    (TrackKind::Audio, Some(point), _) => {
                        Gate::From { bound_ns: point.saturating_sub(rule.pre_roll_ns) }
                    }
                    _ => Gate::Open,
                };
            }
            return Restart { to_byte, at_ns: point.unwrap_or(video_from) };
        }
        let off = self.off;
        let restart = self.tables.gates(point, video_from, to_byte, &|i| !off[i], &mut self.gates);
        for (gate, off) in self.gates.iter_mut().zip(self.off) {
            *gate = if off { SHUT } else { *gate };
        }
        self.next = self.tables.samples.partition_point(|s| s.offset < restart.to_byte);
        restart
    }

    pub fn is_fragmented(&self) -> bool {
        self.walk.is_some()
    }

    /// The next buffer starts a media segment. A walk not at a box boundary, or told to
    /// `reset`, drops what it holds. `media_ns` is the media time that lands on `stream_ns`.
    pub fn mark(&mut self, reset: bool, stream_ns: Option<u64>, media_ns: Option<u64>) {
        let end = self.stream_base + self.buffered;
        let Some(walk) = self.walk.as_mut() else {
            return;
        };
        let clean = walk.step == Step::Box && (walk.at == end || walk.at == u64::MAX);
        let clean = clean && walk.run.is_empty() && walk.moof.is_none();
        if !clean && !reset && walk.step != Step::Lost {
            walk.error.get_or_insert(Mp4Error::Inconsistent("a segment ended inside a box"));
        }
        self.chunks.clear();
        (self.stream_base, self.buffered) = (end, 0);
        match clean && !reset {
            true => walk.at = end,
            false => walk.restart(end),
        }
        walk.marked = true;
        if let Some(stream) = stream_ns {
            let to = i64::try_from(stream).unwrap_or(i64::MAX);
            match media_ns {
                Some(media) => {
                    walk.shift = i64::try_from(media).unwrap_or(i64::MAX).saturating_sub(to);
                    walk.anchor = None;
                }
                None => walk.anchor = Some(stream),
            }
        }
    }

    pub fn shift_ns(&self) -> i64 {
        self.walk.as_ref().map_or(0, |walk| walk.shift)
    }

    pub fn init_epoch(&self) -> u32 {
        self.walk.as_ref().map_or(0, |walk| walk.epoch)
    }

    pub fn take_error(&mut self) -> Option<Mp4Error> {
        self.walk.as_mut().and_then(|walk| walk.error.take())
    }

    pub fn is_lost(&self) -> bool {
        self.walk.as_ref().is_some_and(|walk| walk.step == Step::Lost && !walk.marked)
    }
    /// Feed the next chunk of the whole file. Chunks of at least `RETAIN_MIN` bytes are
    /// retained (a refcount move), smaller ones are copied.
    pub fn push(&mut self, chunk: Memory) {
        if chunk.is_empty() {
            return;
        }
        if chunk.len() >= RETAIN_MIN {
            self.buffered += chunk.len() as u64;
            self.chunks.push_back(Chunk::Retained(chunk));
        } else {
            self.push_bytes(chunk.data());
        }
    }

    // Cold path, small dribbles and tests only.
    #[allow(clippy::disallowed_methods)]
    pub fn push_bytes(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.buffered += data.len() as u64;
        match self.chunks.back_mut() {
            Some(Chunk::Owned(v)) if v.len() < OWNED_CHUNK_CAP => v.extend_from_slice(data),
            _ => self.chunks.push_back(Chunk::Owned(data.to_vec())),
        }
    }

    /// The next sample whose bytes are fully buffered. A sample inside one retained chunk is
    /// a zero-copy slice, one straddling a chunk boundary is gathered by copy.
    pub fn next_sample(&mut self) -> Option<ResolvedSample> {
        if self.walk.is_some() {
            return self.next_fragment_sample();
        }
        let samples = &self.tables.samples;
        while samples.get(self.next).is_some_and(|s| s.offset < self.gates[s.track()]) {
            self.next += 1;
        }
        let Some(s) = samples.get(self.next).copied() else {
            // Bytes behind the last sample, a trailing `moov`, would pin the source's slots.
            self.drop_before(self.stream_base + self.buffered);
            return None;
        };
        let mut sample = self.slice(s)?;
        sample.span = self.tables.span(self.next);
        self.next += 1;
        Some(sample)
    }

    pub fn span_at(&self, index: usize) -> Option<i64> {
        self.tables.span(index)
    }

    pub fn next_wanted(&mut self) -> Option<(usize, Sample)> {
        if self.walk.is_some() {
            return None;
        }
        let samples = &self.tables.samples;
        while samples.get(self.next).is_some_and(|s| s.offset < self.gates[s.track()]) {
            self.next += 1;
        }
        let s = *samples.get(self.next)?;
        self.next += 1;
        Some((self.next - 1, s))
    }

    pub fn rewind_to(&mut self, index: usize) {
        self.next = self.next.min(index);
    }

    fn drop_before(&mut self, start: u64) {
        while let Some(front) = self.chunks.front() {
            let flen = front.len() as u64;
            if self.stream_base + flen > start {
                break;
            }
            self.stream_base += flen;
            self.buffered -= flen;
            self.chunks.pop_front();
        }
    }

    /// The sample at `start` waits for bytes past the window. Once it spans two chunks they
    /// are copied out so a sample larger than the source's pool does not pin every slot.
    // Per input chunk of a sample wider than the window, every such sample in push mode.
    #[allow(clippy::disallowed_methods)]
    fn gather_from(&mut self, start: u64) {
        let retained = self.chunks.iter().any(|c| matches!(c, Chunk::Retained(_)));
        if self.chunks.len() < 2 || !retained || start < self.stream_base {
            return;
        }
        let skip = (start - self.stream_base) as usize;
        let mut owned = match self.chunks.pop_front() {
            Some(Chunk::Owned(mut v)) => {
                v.drain(..skip.min(v.len()));
                v
            }
            Some(Chunk::Retained(m)) => m.data().get(skip..).unwrap_or_default().to_vec(),
            None => return,
        };
        for chunk in self.chunks.drain(..) {
            owned.extend_from_slice(chunk.data());
        }
        (self.stream_base, self.buffered) = (start, owned.len() as u64);
        self.chunks.push_back(Chunk::Owned(owned));
    }

    fn peek(&self, at: u64, out: &mut [u8]) -> bool {
        let Some(end) = at.checked_add(out.len() as u64) else {
            return false;
        };
        if at < self.stream_base || end > self.stream_base + self.buffered {
            return false;
        }
        let (mut base, mut done) = (self.stream_base, 0usize);
        for chunk in &self.chunks {
            let clen = chunk.len() as u64;
            if at + (done as u64) < base + clen && done < out.len() {
                let lo = (at + done as u64 - base) as usize;
                let take = (out.len() - done).min(chunk.len() - lo);
                out[done..done + take].copy_from_slice(&chunk.data()[lo..lo + take]);
                done += take;
            }
            base += clen;
        }
        done == out.len()
    }

    fn slice(&mut self, s: Sample) -> Option<ResolvedSample> {
        let start = s.offset;
        let end = s.offset.checked_add(s.size as u64)?;
        self.drop_before(start);

        if start < self.stream_base || end > self.stream_base + self.buffered {
            self.gather_from(start);
            return None;
        }
        if s.size == 0 {
            // Zero-length sample. `Vec::new()` does not allocate.
            #[allow(clippy::disallowed_methods)]
            return Some(ResolvedSample {
                track_index: s.track(),
                pts: s.pts,
                dts: s.dts,
                sync: s.sync,
                hidden: s.hidden,
                span: None,
                payload: SamplePayload::Copied(Vec::new()),
            });
        }

        let mut base = self.stream_base;
        let mut idx = 0usize;
        while idx < self.chunks.len() {
            let clen = self.chunks[idx].len() as u64;
            if start < base + clen {
                break;
            }
            base += clen;
            idx += 1;
        }
        let rel = (start - base) as usize;
        let sliced = match &self.chunks[idx] {
            Chunk::Retained(m) => m.slice(rel, s.size as usize),
            Chunk::Owned(_) => None,
        };
        let payload = match sliced {
            Some(m) => SamplePayload::Slice(m),
            None => {
                // Cold path, only for a sample crossing retained chunk bounds.
                #[allow(clippy::disallowed_methods)]
                let mut v = Vec::with_capacity(s.size as usize);
                let (mut at, mut cbase, mut i) = (start, base, idx);
                while at < end {
                    let c = &self.chunks[i];
                    let lo = (at - cbase) as usize;
                    let take = ((end - at) as usize).min(c.len() - lo);
                    v.extend_from_slice(&c.data()[lo..lo + take]);
                    at += take as u64;
                    cbase += c.len() as u64;
                    i += 1;
                }
                SamplePayload::Copied(v)
            }
        };
        Some(ResolvedSample {
            track_index: s.track(),
            pts: s.pts,
            dts: s.dts,
            sync: s.sync,
            hidden: s.hidden,
            span: None,
            payload,
        })
    }

    fn next_fragment_sample(&mut self) -> Option<ResolvedSample> {
        loop {
            let (step, at) = self.walk.as_ref().map(|walk| (walk.step, walk.at))?;
            match step {
                Step::Lost => {
                    self.chunks.clear();
                    (self.stream_base, self.buffered) = (self.stream_base + self.buffered, 0);
                    return None;
                }
                Step::Mdat { end } => {
                    let walk = self.walk.as_mut()?;
                    match walk.run.get(walk.next).copied() {
                        Some(s) => {
                            let mut sample = self.slice(s)?;
                            let walk = self.walk.as_mut()?;
                            if self.tables.tracks.get(s.track()).is_some_and(Track::is_text) {
                                let media_time = self.tables.tracks[s.track()].media_time;
                                let end = walk
                                    .next_dts
                                    .get(s.track())
                                    .map(|n| (*n).min(i64::MAX as u64) as i64 - media_time);
                                sample.span =
                                    span_in(&walk.run, walk.next).or(end.map(|e| e - s.pts));
                            }
                            walk.next += 1;
                            return Some(sample);
                        }
                        None => {
                            walk.run.clear();
                            (walk.next, walk.at, walk.step) = (0, end, Step::Box);
                        }
                    }
                }
                Step::Box => {
                    self.drop_before(at);
                    let mut head = [0u8; 16];
                    if !self.peek(at, &mut head[..8]) {
                        return None;
                    }
                    let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
                    let kind = [head[4], head[5], head[6], head[7]];
                    let (header, size) = match size {
                        1 if !self.peek(at, &mut head) => return None,
                        1 => (
                            16u64,
                            u64::from_be_bytes([
                                head[8], head[9], head[10], head[11], head[12], head[13], head[14],
                                head[15],
                            ]),
                        ),
                        0 => (8, u64::MAX - at),
                        size => (8, size),
                    };
                    let Some(end) = at.checked_add(size).filter(|_| size >= header) else {
                        let small = BoxError::Malformed("box size smaller than its own header");
                        self.walk.as_mut()?.lose(Mp4Error::Box(small));
                        continue;
                    };
                    let moov = kind == boxes::boxtype::MOOV;
                    if (moov || kind == boxes::boxtype::MOOF) && size - header > MAX_FRAGMENT_HEAD {
                        self.walk
                            .as_mut()?
                            .lose(Mp4Error::FragmentTooLarge { limit: MAX_FRAGMENT_HEAD });
                    } else if moov || kind == boxes::boxtype::MOOF {
                        let walk = self.walk.as_mut()?;
                        walk.head.clear();
                        (walk.moof, walk.at) = (None, at + header);
                        walk.step = Step::Gather { moov, start: at, end };
                    } else if kind == boxes::boxtype::MDAT {
                        self.enter_mdat(at + header, end);
                    } else {
                        // `styp`, `sidx`, `free`, `emsg`, `prft` and unknown boxes are stepped over
                        // (4.2).
                        self.walk.as_mut()?.at = end;
                    }
                }
                Step::Gather { moov, start, end } => {
                    // Gather and release the chunks so a large `moof` does not pin the source's
                    // pool.
                    self.drop_before(at);
                    let have = (self.stream_base + self.buffered).min(end);
                    let Self { walk, chunks, stream_base, .. } = self;
                    let walk = walk.as_mut()?;
                    let mut base = *stream_base;
                    for chunk in chunks.iter() {
                        let clen = chunk.len() as u64;
                        if walk.at < base + clen && walk.at < have {
                            let lo = (walk.at - base) as usize;
                            let take = ((have - walk.at) as usize).min(chunk.len() - lo);
                            walk.head.extend_from_slice(&chunk.data()[lo..lo + take]);
                            walk.at += take as u64;
                        }
                        base += clen;
                    }
                    let whole = walk.at == end;
                    if whole {
                        walk.step = Step::Box;
                        walk.moof = (!moov).then_some(start);
                    }
                    self.drop_before(have);
                    if !whole {
                        return None;
                    }
                    if moov {
                        self.reinit();
                    }
                }
            }
        }
    }

    fn enter_mdat(&mut self, body: u64, end: u64) {
        let Self { walk, tables, .. } = self;
        let Some(walk) = walk.as_mut() else { return };
        walk.step = Step::Mdat { end };
        if let (Some(moof), Some(tables_frag)) = (walk.moof.take(), tables.frag.as_ref()) {
            walk.run.clear();
            walk.next = 0;
            let movie = frag::Movie { tracks: &tables.tracks, trex: &tables_frag.trex };
            let place = frag::Place { moof, mdat: (body, end) };
            let first =
                match frag::expand(&walk.head, &movie, place, &mut walk.next_dts, &mut walk.run) {
                    Ok(first) => first,
                    Err(e) => {
                        walk.error.get_or_insert(e);
                        None
                    }
                };
            walk.head.clear();
            if let (Some(stream), Some(first)) = (walk.anchor, first) {
                let (first, stream) = (
                    i64::try_from(first).unwrap_or(i64::MAX),
                    i64::try_from(stream).unwrap_or(i64::MAX),
                );
                (walk.shift, walk.anchor) = (first.saturating_sub(stream), None);
            }
            walk.gate(&tables.rules);
        } else {
            walk.run
                .retain(|s| s.offset >= body && s.offset.saturating_add(u64::from(s.size)) <= end);
        }
        // Disjoint offsets, so an unstable sort loses nothing and allocates nothing.
        walk.run.sort_unstable_by_key(|s| s.offset);
        let mut covered = body;
        for s in &walk.run {
            if s.offset < covered {
                walk.error.get_or_insert(Mp4Error::Inconsistent("samples overlap in the fragment"));
                walk.run.clear();
                break;
            }
            covered = s.offset + u64::from(s.size);
        }
    }

    // Cold path, once per init segment.
    #[allow(clippy::disallowed_methods)]
    fn reinit(&mut self) {
        let Some(walk) = self.walk.as_mut() else {
            return;
        };
        // The init's own samples are dropped below, so its tables are not folded.
        let movie = resolve_moov(&walk.head, None);
        walk.head.clear();
        let (movie, trex) = match movie.map(|m| (m.trex.clone(), m)) {
            Ok((Some(trex), movie)) => (movie, trex),
            Ok((None, _)) => return walk.lose(Mp4Error::InitChanged),
            Err(e) => return walk.lose(e),
        };
        let old = &self.tables.tracks;
        let same_shape = old.len() == movie.tracks.len()
            && old.iter().zip(&movie.tracks).all(|(a, b)| a.kind() == b.kind());
        if !same_shape {
            return walk.lose(Mp4Error::InitChanged);
        }
        let same = old.iter().zip(&movie.tracks).all(|(a, b)| {
            a.track_id == b.track_id
                && a.timescale == b.timescale
                && a.media_time == b.media_time
                && a.entry.kind == b.entry.kind
                && a.entry.config_record == b.entry.config_record
                && a.entry.codec_head == b.entry.codec_head
                && (a.width, a.height) == (b.width, b.height)
                && (a.entry.sample_rate, a.entry.channels)
                    == (b.entry.sample_rate, b.entry.channels)
        });
        let same = same && self.tables.frag.as_ref().is_some_and(|f| f.trex == trex);
        walk.next_dts.iter_mut().for_each(|next| *next = 0);
        if same {
            return;
        }
        walk.epoch = walk.epoch.wrapping_add(1);
        let rules = rules_of(&movie.tracks);
        let start = self.tables.frag.as_ref().map_or(0, |f| f.start);
        let frag =
            FragTables { trex, fragments: Vec::new(), start, duration_ns: movie.duration_ns };
        self.tables = Arc::new(Tables {
            tracks: movie.tracks,
            samples: SampleIndex::default(),
            rules,
            frag: Some(frag),
            skipped: movie.skipped,
        });
    }
    /// Media ticks to non-negative ns via i128. A negative time clamps to 0, a zero timescale
    /// passes the ticks through.
    pub fn ticks_to_ns(&self, track_index: usize, ticks: i64) -> u64 {
        let ts = self.tables.tracks.get(track_index).map(|t| t.timescale).unwrap_or(0);
        ticks_to_ns(ticks, ts)
    }
}

pub struct ResolvedSample {
    pub track_index: usize,
    pub pts: i64,
    pub dts: i64,
    pub sync: bool,
    pub hidden: bool,
    pub span: Option<i64>,
    pub payload: SamplePayload,
}

pub enum SamplePayload {
    Slice(Memory),
    Copied(Vec<u8>),
}

impl SamplePayload {
    pub fn data(&self) -> &[u8] {
        match self {
            SamplePayload::Slice(m) => m.data(),
            SamplePayload::Copied(v) => v,
        }
    }
}

/// Media ticks to non-negative ns via i128. Timescale 0 passes through.
pub(crate) fn ticks_to_ns(ticks: i64, timescale: u32) -> u64 {
    if timescale == 0 {
        return ticks.max(0) as u64;
    }
    let ns = (ticks as i128 * 1_000_000_000i128) / timescale as i128;
    ns.clamp(0, u64::MAX as i128) as u64
}

fn span_in(samples: &[Sample], at: usize) -> Option<i64> {
    let s = samples.get(at)?;
    let next = samples.get(at + 1..)?.iter().find(|n| n.track_index == s.track_index)?;
    Some(next.pts.saturating_sub(s.pts))
}

fn find_moov(head: &[u8]) -> Result<Option<BoxHeader>, BoxError> {
    let mut at = 0usize;
    while at + 8 <= head.len() {
        let h = boxes::read_box_header(head, at)?;
        if h.kind == boxes::boxtype::MOOV {
            return Ok(Some(h));
        }
        at = h.end;
    }
    Ok(None)
}

struct Movie {
    tracks: Vec<Track>,
    samples: SampleIndex,
    trex: Option<Vec<Trex>>,
    duration_ns: u64,
    skipped: u32,
}

/// `None` resolves the tracks and leaves the sample tables unfolded.
// Cold path, once per movie and per init segment.
#[allow(clippy::disallowed_methods)]
fn resolve_moov(moov_body: &[u8], limits: Option<Limits>) -> Result<Movie, Mp4Error> {
    // A fragmented movie carries `mvex` in `moov` (8.8.1).
    let mvex = boxes::find_child(moov_body, boxes::boxtype::MVEX)?;
    let movie_scale = match boxes::find_child(moov_body, boxes::boxtype::MVHD)? {
        Some(mvhd) => boxes::parse_mvhd(mvhd.body(moov_body))?.timescale,
        None => 0,
    };

    // `for_each_child`'s closure carries only BoxError, so tracks resolve outside it.
    let mut trak_boxes = Vec::new();
    boxes::for_each_child(moov_body, |child| {
        if child.kind == boxes::boxtype::TRAK {
            trak_boxes.push(*child);
        }
        Ok(())
    })?;

    let mut tracks = Vec::new();
    let mut stbls = Vec::new();
    let mut skipped = 0u32;
    for trak in &trak_boxes {
        let trak_body = trak.body(moov_body);
        let Some((track, stbl)) =
            resolve_track(trak_body, tracks.len(), mvex.is_some(), movie_scale)?
        else {
            skipped += 1;
            continue;
        };
        if tracks.len() == MAX_TRACKS {
            return Err(Mp4Error::TooManyTracks { limit: MAX_TRACKS });
        }
        tracks.push(track);
        stbls.push(stbl);
    }

    if tracks.is_empty() {
        return Err(Mp4Error::Missing("no resolvable track in moov"));
    }

    let mut samples = SampleIndex::default();
    if let Some(limits) = limits {
        let limit = limits.samples.min(MAX_SAMPLES);
        let total = stbls
            .iter()
            .flatten()
            .try_fold(0usize, |n, s| n.checked_add(s.count).filter(|&n| n <= limit))
            .ok_or(Mp4Error::TooManySamples { limit })?;
        samples = SampleIndex::reserve(total)?;
        samples.room = Room::of(limits.file_len);
        for (track_index, stbl) in stbls.iter().enumerate() {
            if let Some(s) = stbl
                && !fold_track(track_index as u16, s, &mut samples)?
            {
                let track = &mut tracks[track_index];
                track.edit_refused = Some(EditRefusal::NothingPresented);
            }
        }
    }

    // Unstable, a stable sort allocates half the index again as scratch.
    samples.sort_unstable_by_key(|s| (s.offset, s.track_index, s.dts));

    // Samples must be disjoint byte ranges. Overlapping ones would emit the same bytes per
    // sample, an output unbounded by the input. Empty samples are exempt.
    let mut covered = 0u64;
    for s in samples.iter().filter(|s| s.size != 0) {
        if s.offset < covered {
            return Err(Mp4Error::Inconsistent("samples overlap in the file"));
        }
        covered = s.offset.saturating_add(s.size as u64);
    }

    let Some(mvex) = mvex else {
        return Ok(Movie { tracks, samples, trex: None, duration_ns: 0, skipped });
    };
    let mvex_body = mvex.body(moov_body);
    let mut trex: Vec<Trex> =
        tracks.iter().map(|t| Trex { track_id: t.track_id, ..Trex::default() }).collect();
    let mut fragment_duration = 0u64;
    boxes::for_each_child(mvex_body, |child| {
        let body = child.body(mvex_body);
        if child.kind == boxes::boxtype::TREX {
            let parsed = frag::parse_trex(body)?;
            if let Some(slot) = trex.iter_mut().find(|slot| slot.track_id == parsed.track_id) {
                *slot = parsed;
            }
        } else if child.kind == boxes::boxtype::MEHD {
            fragment_duration = frag::parse_mehd(body)?;
        }
        Ok(())
    })?;
    let duration_ns = match movie_scale {
        0 => 0,
        scale => ticks_to_ns(fragment_duration.min(i64::MAX as u64) as i64, scale),
    };
    for track in tracks.iter_mut().filter(|track| track.duration_ns == 0) {
        track.duration_ns = duration_ns;
    }
    Ok(Movie { tracks, samples, trex: Some(trex), duration_ns, skipped })
}

// Cold path, once per movie.
#[allow(clippy::disallowed_methods)]
fn rules_of(tracks: &[Track]) -> Vec<Rule> {
    tracks
        .iter()
        .map(|track| Rule {
            kind: track.kind(),
            timescale: track.timescale,
            delay_ticks: track.delay_ticks(),
            pre_roll_ns: track.pre_roll_ns(),
            media_time: track.media_time,
        })
        .collect()
}

fn index_head(head: &[u8], mut at: usize, out: &mut Vec<FragmentRef>) -> u64 {
    let mut end_ns = 0u64;
    while let Ok(h) = boxes::read_box_header(head, at) {
        if h.kind == boxes::boxtype::MOOF || h.kind == boxes::boxtype::MDAT {
            break;
        }
        if let Some(sidx) =
            (h.kind == boxes::boxtype::SIDX).then(|| frag::parse_sidx(h.body(head)).ok()).flatten()
        {
            frag::sidx_fragments(&sidx, h.end as u64, out);
            let ticks = sidx
                .refs
                .iter()
                .fold(sidx.earliest, |t, r| t.saturating_add(u64::from(r.duration)));
            end_ns = end_ns.max(ticks_to_ns(ticks.min(i64::MAX as u64) as i64, sidx.timescale));
        }
        at = h.end;
    }
    end_ns
}

fn index_mfra(mfra: &[u8], tracks: &[Track], out: &mut Vec<FragmentRef>) {
    let Ok(h) = boxes::read_box_header(mfra, 0) else {
        return;
    };
    if h.kind != boxes::boxtype::MFRA {
        return;
    }
    let body = h.body(mfra);
    let of = tracks.iter().position(|t| t.kind() == TrackKind::Video).unwrap_or(0);
    let Some(track) = tracks.get(of) else { return };
    let _ = boxes::for_each_child(body, |child| {
        if child.kind != boxes::boxtype::TFRA {
            return Ok(());
        }
        let (id, rows) = frag::parse_tfra(child.body(body))?;
        if id == track.track_id {
            out.extend(rows.iter().map(|(time, offset)| {
                let pts = ((*time).min(i64::MAX as u64) as i64).saturating_sub(track.media_time);
                FragmentRef { time_ns: track.stamp_ns(pts), offset: *offset }
            }));
        }
        Ok(())
    });
}

impl Walk {
    // Cold path, once per reader of a fragmented movie.
    #[allow(clippy::disallowed_methods)]
    fn new(tracks: usize) -> Self {
        Walk {
            step: Step::Box,
            at: 0,
            head: Vec::new(),
            moof: None,
            run: Vec::new(),
            next: 0,
            next_dts: vec![0; tracks],
            gates: vec![Gate::Open; tracks],
            shift: 0,
            anchor: None,
            epoch: 0,
            marked: false,
            error: None,
        }
    }

    fn restart(&mut self, at: u64) {
        (self.step, self.at, self.moof, self.next) = (Step::Box, at, None, 0);
        self.head.clear();
        self.run.clear();
        self.next_dts.iter_mut().for_each(|next| *next = 0);
    }

    fn lose(&mut self, error: Mp4Error) {
        self.error.get_or_insert(error);
        self.restart(self.at);
        self.step = Step::Lost;
    }

    /// Let through the run as each track's gate says, in `moof` order. A sync sample comes from
    /// the sample flags (ISO/IEC 14496-12 8.8.3.1). A fragment ending before the bound is
    /// dropped whole and the gate stays.
    fn gate(&mut self, rules: &[Rule]) {
        if self.gates.iter().all(|gate| *gate == Gate::Open) {
            return;
        }
        let mut cut = [0usize; MAX_TRACKS];
        let shift = i128::from(self.shift);
        for (track, (gate, rule)) in self.gates.iter_mut().zip(rules).enumerate() {
            let video = rule.kind == TrackKind::Video;
            let stamp = |pts: i64| {
                let ns = ticks_to_ns(pts.saturating_add(rule.delay_ticks), rule.timescale);
                (i128::from(ns) - shift).clamp(0, i128::from(u64::MAX)) as u64
            };
            let of = self.run.iter().filter(|s| s.track() == track);
            cut[track] = match *gate {
                Gate::Open => 0,
                Gate::Shut => usize::MAX,
                Gate::NextSync => match of.clone().position(|s| s.sync || !video) {
                    Some(n) => {
                        *gate = Gate::Open;
                        n
                    }
                    None => usize::MAX,
                },
                Gate::From { bound_ns } => {
                    let (mut best, mut first) = (None, None);
                    let end =
                        self.next_dts.get(track).copied().unwrap_or(0).min(i64::MAX as u64) as i64;
                    let mut past = stamp(end.saturating_sub(rule.media_time)) > bound_ns;
                    for (n, s) in of.enumerate().filter(|(_, s)| s.sync || !video) {
                        first.get_or_insert(n);
                        match stamp(s.pts) <= bound_ns {
                            true => best = Some(n),
                            false => past = true,
                        }
                    }
                    let from = match (best, first, past) {
                        (Some(best), _, true) => Some(best),
                        (None, Some(first), _) => Some(first),
                        _ => None,
                    };
                    if from.is_some() {
                        *gate = Gate::Open;
                    }
                    from.unwrap_or(usize::MAX)
                }
            };
        }
        let mut seen = [0usize; MAX_TRACKS];
        self.run.retain(|s| {
            let n = seen[s.track()];
            seen[s.track()] += 1;
            n >= cut[s.track()]
        });
    }
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
fn resolve_track(
    trak_body: &[u8],
    track_index: usize,
    fragmented: bool,
    movie_scale: u32,
) -> Result<Option<(Track, Option<Stbl<'_>>)>, Mp4Error> {
    let tkhd = match boxes::find_child(trak_body, boxes::boxtype::TKHD)? {
        Some(h) => Some(boxes::parse_tkhd(h.body(trak_body))?),
        None => None,
    };

    let mdia =
        boxes::find_child(trak_body, boxes::boxtype::MDIA)?.ok_or(Mp4Error::Missing("mdia"))?;
    let mdia_body = mdia.body(trak_body);

    let mdhd =
        boxes::find_child(mdia_body, boxes::boxtype::MDHD)?.ok_or(Mp4Error::Missing("mdhd"))?;
    let mdhd = boxes::parse_mdhd(mdhd.body(mdia_body))?;

    let named = match boxes::find_child(trak_body, boxes::boxtype::UDTA)? {
        Some(udta) => {
            let udta_body = udta.body(trak_body);
            let name = boxes::find_child(udta_body, boxes::boxtype::NAME)?;
            name.and_then(|name| std::str::from_utf8(name.body(udta_body)).ok())
        }
        None => None,
    };
    let handler = match boxes::find_child(mdia_body, boxes::boxtype::HDLR)? {
        Some(hdlr) => boxes::parse_hdlr_name(hdlr.body(mdia_body)),
        None => "",
    };
    let title = named.map(|name| name.trim_end_matches('\0')).filter(|name| !name.is_empty());
    let title =
        title.unwrap_or(if handler.ends_with("Handler") { "" } else { handler }).to_string();
    let (role, forced) = kinds(trak_body)?;
    let language_tag = match boxes::find_child(mdia_body, boxes::boxtype::ELNG)? {
        Some(elng) => boxes::parse_elng(elng.body(mdia_body)).unwrap_or("").to_string(),
        None => String::new(),
    };

    let minf =
        boxes::find_child(mdia_body, boxes::boxtype::MINF)?.ok_or(Mp4Error::Missing("minf"))?;
    let minf_body = minf.body(mdia_body);

    let stbl =
        boxes::find_child(minf_body, boxes::boxtype::STBL)?.ok_or(Mp4Error::Missing("stbl"))?;
    let stbl_body = stbl.body(minf_body);

    let stsd =
        boxes::find_child(stbl_body, boxes::boxtype::STSD)?.ok_or(Mp4Error::Missing("stsd"))?;
    let entry = codec::parse_stsd(stsd.body(stbl_body))?;

    // A track missing a mandatory table is skipped, not an error. Chapter and metadata
    // tracks often carry an empty stbl.
    let stts = boxes::find_child(stbl_body, boxes::boxtype::STTS)?;
    let sizes_box = boxes::find_child(stbl_body, boxes::boxtype::STSZ)?
        .or(boxes::find_child(stbl_body, boxes::boxtype::STZ2)?);
    let stsc = boxes::find_child(stbl_body, boxes::boxtype::STSC)?;
    let chunk_offsets_box = boxes::find_child(stbl_body, boxes::boxtype::STCO)?
        .or(boxes::find_child(stbl_body, boxes::boxtype::CO64)?);

    let edits = edit_list(trak_body, mdhd.timescale, movie_scale)?;
    let media_time = edits.as_ref().map_or(0, EditList::first_media);
    let kind = track_kind(entry.family);

    let bytes =
        sizes_box.and_then(|b| sizes_total(b.body(stbl_body), b.kind == boxes::boxtype::STZ2));
    let bitrate = match entry.avg_bitrate {
        0 => bytes.map_or(0, |b| bits_per_second(b, mdhd.duration, mdhd.timescale)),
        avg => u64::from(avg),
    };
    let stbl = match (stts, sizes_box, stsc, chunk_offsets_box) {
        (Some(stts), Some(sizes_box), Some(stsc), Some(chunk_offsets_box)) => {
            let stz2 = sizes_box.kind == boxes::boxtype::STZ2;
            let sizes = sizes_box.body(stbl_body);
            let offsets = chunk_offsets_box.body(stbl_body);
            let pcm_frame = entry.pcm.map_or(0, |p| p.frame);
            let mut count = declared_count(sizes, stz2)?;
            if pcm_group(sizes, stz2, pcm_frame) != 0 {
                // Each chunk splits into at most one group more than its frames fill.
                let chunks =
                    offsets.get(4..8).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
                count = count.min(count.div_ceil(PCM_GROUP as usize) + chunks as usize);
            }
            Some(Stbl {
                count,
                stts: stts.body(stbl_body),
                sizes,
                stz2,
                stsc: stsc.body(stbl_body),
                offsets,
                co64: chunk_offsets_box.kind == boxes::boxtype::CO64,
                pcm_frame,
                ctts: boxes::find_child(stbl_body, boxes::boxtype::CTTS)?
                    .map(|h| h.body(stbl_body)),
                stss: boxes::find_child(stbl_body, boxes::boxtype::STSS)?
                    .map(|h| h.body(stbl_body)),
                edits: edits.clone(),
                kind,
            })
        }
        _ if fragmented => None,
        _ => return Ok(None),
    };

    let (track_id, tk_w, tk_h) =
        tkhd.map(|t| (t.track_id, t.width, t.height)).unwrap_or((track_index as u32 + 1, 0, 0));
    let width = if tk_w != 0 { tk_w } else { entry.width };
    let height = if tk_h != 0 { tk_h } else { entry.height };

    let mut duration_ns = ticks_to_ns(mdhd.duration as i64, mdhd.timescale);
    // The edit list's span is used where it differs from the media's by more than a movie
    // tick, its rounding.
    if let Some(list) = edits.as_ref().filter(|l| l.movie_len != 0 && movie_scale != 0) {
        let spans = ticks_to_ns(list.movie_len.min(i64::MAX as u64) as i64, movie_scale);
        if spans.abs_diff(duration_ns) > ticks_to_ns(1, movie_scale) {
            duration_ns = spans;
        }
    }
    let track = Track {
        track_id,
        timescale: mdhd.timescale,
        duration_ns,
        media_time,
        edit_refused: edits.as_ref().and_then(|l| l.refused),
        entry,
        width,
        height,
        rotation: tkhd.map_or(0, |t| t.rotation),
        language: mdhd.language,
        title,
        language_tag,
        enabled: tkhd.is_none_or(|t| t.enabled),
        alternate_group: tkhd.map_or(0, |t| t.alternate_group),
        disposition: role,
        forced,
        bitrate,
    };
    Ok(Some((track, stbl)))
}

/// The bytes an `stsz` or `stz2` body's samples sum to, walked in place. None for a table the
/// box does not hold whole.
fn sizes_total(body: &[u8], stz2: bool) -> Option<u64> {
    let be32 = |o: usize| body.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let (size, count) = (be32(4)?, u64::from(be32(8)?));
    if !stz2 && size != 0 {
        return Some(u64::from(size) * count);
    }
    let bits = if stz2 { u64::from(body[7]) } else { 32 };
    let table = body.get(12..)?;
    if !matches!(bits, 4 | 8 | 16 | 32) || (count * bits).div_ceil(8) > table.len() as u64 {
        return None;
    }
    let n = count as usize;
    Some(match bits {
        32 => table.as_chunks::<4>().0[..n].iter().map(|b| u64::from(u32::from_be_bytes(*b))).sum(),
        16 => table.as_chunks::<2>().0[..n].iter().map(|b| u64::from(u16::from_be_bytes(*b))).sum(),
        8 => table[..n].iter().map(|&b| u64::from(b)).sum(),
        _ => (0..n).map(|k| u64::from(table[k / 2] >> (4 * (1 - k % 2)) & 0x0F)).sum(),
    })
}

/// `bytes` over `duration` ticks of `timescale` a second, as bits a second.
fn bits_per_second(bytes: u64, duration: u64, timescale: u32) -> u64 {
    if duration == 0 || timescale == 0 {
        return 0;
    }
    let bits = u128::from(bytes) * 8 * u128::from(timescale) / u128::from(duration);
    u64::try_from(bits).unwrap_or(u64::MAX)
}

/// ISO/IEC 23009-1 5.8.5.5.
const ROLE_SCHEME: &[u8] = b"urn:mpeg:dash:role:2011";

fn kinds(trak_body: &[u8]) -> Result<(Disposition, bool), Mp4Error> {
    let (mut role, mut forced) = (Disposition::MAIN, false);
    let Some(udta) = boxes::find_child(trak_body, boxes::boxtype::UDTA)? else {
        return Ok((role, forced));
    };
    let udta_body = udta.body(trak_body);
    boxes::for_each_child(udta_body, |h| {
        let kind = (h.kind == boxes::boxtype::KIND)
            .then(|| boxes::parse_kind(h.body(udta_body)))
            .flatten();
        match kind {
            Some((ROLE_SCHEME, b"commentary")) => role.commentary = true,
            Some((ROLE_SCHEME, b"description")) => role.visual_impaired = true,
            Some((ROLE_SCHEME, b"caption" | b"enhanced-audio-intelligibility")) => {
                role.hearing_impaired = true
            }
            Some((ROLE_SCHEME, b"forced-subtitle")) => forced = true,
            _ => {}
        }
        Ok(())
    })?;
    Ok((role, forced))
}

struct Stbl<'a> {
    count: usize,
    stts: &'a [u8],
    sizes: &'a [u8],
    stz2: bool,
    stsc: &'a [u8],
    offsets: &'a [u8],
    co64: bool,
    pcm_frame: u32,
    ctts: Option<&'a [u8]>,
    stss: Option<&'a [u8]>,
    edits: Option<EditList>,
    kind: TrackKind,
}

fn track_kind(family: &str) -> TrackKind {
    match family {
        family if codec::is_video_family(family) => TrackKind::Video,
        family if pf_mkv::is_audio_family(family) => TrackKind::Audio,
        _ => TrackKind::Other,
    }
}

/// The frames one grouped PCM sample holds, 0 when the track folds one sample per entry. Only
/// a constant size of 1 or of the frame groups, a per-sample table never does.
fn pcm_group(sizes: &[u8], stz2: bool, pcm_frame: u32) -> u32 {
    if stz2 || pcm_frame == 0 {
        return 0;
    }
    let constant = sizes.get(4..8).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    match constant {
        1 => pcm_frame,
        size if size == pcm_frame => size,
        _ => 0,
    }
}

fn declared_count(body: &[u8], stz2: bool) -> Result<usize, Mp4Error> {
    let held = body.len().saturating_sub(12) as u64;
    if let (Some(&[a, b, c, d]), Some(&kind)) = (body.get(8..12), body.get(7)) {
        let count = u32::from_be_bytes([a, b, c, d]) as u64;
        let constant = !stz2 && body.get(4..8).is_some_and(|s| s != [0; 4]);
        let bits = if constant {
            0
        } else if stz2 {
            u64::from(kind)
        } else {
            32
        };
        if (count * bits).div_ceil(8) <= held && (!stz2 || matches!(kind, 4 | 8 | 16)) {
            return Ok(count as usize);
        }
    }
    let sizes = if stz2 { boxes::parse_stz2(body)? } else { boxes::parse_stsz(body)? };
    Ok(sizes.count())
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
                (sample.dts, sample.pts) =
                    (sample.dts.saturating_sub(shift), sample.pts.saturating_sub(shift));
            }
            Ok(false)
        }
    }
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
fn edit_list(
    trak_body: &[u8],
    media_scale: u32,
    movie_scale: u32,
) -> Result<Option<EditList>, Mp4Error> {
    let Some(edts) = boxes::find_child(trak_body, boxes::boxtype::EDTS)? else {
        return Ok(None);
    };
    let Some(elst) = boxes::find_child(edts.body(trak_body), boxes::boxtype::ELST)? else {
        return Ok(None);
    };
    let entries = boxes::parse_elst(elst.body(edts.body(trak_body)))?;
    Ok(edit::follow(&entries, media_scale, movie_scale))
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
                out.push(Sample {
                    track_index,
                    offset,
                    size,
                    dts,
                    pts: dts,
                    sync: true,
                    hidden: false,
                })?;
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

            out.push(Sample { track_index, offset, size, dts, pts, sync, hidden: false })?;

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

/// Uncompressed audio frames per sample, the samples of one chunk grouped as FFmpeg's mov does.
const PCM_GROUP: u32 = 1024;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{CttsEntry, SampleSizes, StscEntry, SttsEntry};

    #[test]
    fn error_messages_carry_the_variant_data() {
        let rows: [(Mp4Error, &[&str]); 7] = [
            (Mp4Error::Box(BoxError::Truncated("box runs past the buffer")), &["malformed box"]),
            (Mp4Error::Missing("moov"), &["missing", "moov"]),
            (Mp4Error::FragmentTooLarge { limit: 16 }, &["larger than 16 octets"]),
            (Mp4Error::Inconsistent("stsc past stco"), &["inconsistent", "stsc past stco"]),
            (Mp4Error::NoConfigRecord { entry: *b"avc3" }, &["passthrough", "'avc3'"]),
            (Mp4Error::NotPrerolled, &["before preroll"]),
            (
                Mp4Error::SampleDropped { track: 1, cause: ReframeError::Truncated("NAL length") },
                &["track 1", "NAL length"],
            ),
        ];
        for (e, needles) in rows {
            let s = format!("{e}");
            assert!(s.starts_with("mp4demux"), "{s}");
            assert!(!s.contains('\n') && !s.contains('\u{2014}'), "{s}");
            for n in needles {
                assert!(s.contains(n), "{s:?} should mention {n:?}");
            }
        }
    }

    #[test]
    fn the_tick_bound_is_the_stamp_bound() {
        for timescale in [0u32, 1, 24, 600, 1_000, 44_100, 48_000, 90_000, 1_000_000_000, u32::MAX]
        {
            for ticks in [0i64, 1, 2, 999, 1_000, 48_000, 123_456_789, 1 << 40] {
                let ns = ticks_to_ns(ticks, timescale);
                if ns == u64::MAX {
                    continue;
                }
                for bound in [ns.saturating_sub(1), ns, ns.saturating_add(1)] {
                    let at_or_before = ticks_at_or_before(bound, timescale);
                    assert!(
                        ticks_to_ns(at_or_before, timescale) <= bound,
                        "{timescale} {ticks} {bound}"
                    );
                    assert!(
                        ticks_to_ns(at_or_before + 1, timescale) > bound,
                        "{timescale} {ticks} {bound}"
                    );
                }
            }
        }
    }

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
    fn an_audio_track_with_no_rate_is_primed_as_at_48_khz() {
        assert_eq!(pre_roll_ns("aac", 0, 0), 21_333_334, "not 1024 s");
        assert_eq!(pre_roll_ns("aac", 0, 44_100), 23_219_955, "the media timescale");
        assert_eq!(pre_roll_ns("aac", 48_000, 1_000), 21_333_334, "the sample entry first");
    }

    #[test]
    fn ticks_to_ns_is_i128_safe() {
        assert_eq!(ticks_to_ns(0, 48_000), 0);
        assert_eq!(ticks_to_ns(48_000, 48_000), 1_000_000_000);
        let big = 1i64 << 40;
        let want = (big as i128 * 1_000_000_000 / 90_000) as u64;
        assert_eq!(ticks_to_ns(big, 90_000), want);
        assert_eq!(ticks_to_ns(1234, 0), 1234, "timescale 0 → passthrough");
        assert_eq!(ticks_to_ns(-9000, 90_000), 0, "negative presentation time → 0");
        assert_eq!(ticks_to_ns(-5, 0), 0, "timescale 0 + negative → 0");
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
        assert_eq!((out[0].offset, out[0].size, out[0].dts, out[0].pts), (1000, 10, 0, 0));
        assert_eq!((out[1].offset, out[1].size, out[1].dts, out[1].pts), (1010, 20, 512, 512));
        assert_eq!((out[2].offset, out[2].size, out[2].dts), (5000, 30, 1024));
        assert_eq!((out[3].offset, out[3].size, out[3].dts), (5030, 40, 1536));
        assert!(out.iter().all(|s| s.sync));
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
        assert_eq!((out[0].dts, out[0].pts), (-200, 0));
        assert_eq!((out[1].dts, out[1].pts), (-100, -100));
        assert_eq!((out[2].dts, out[2].pts), (0, 0));
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
    fn sample_sizes_are_summed_in_place() {
        let constant = [0, 0, 0, 0, 0, 0, 0, 5, 0, 0x0F, 0x42, 0x40];
        assert_eq!(sizes_total(&constant, false), Some(5_000_000));
        let two = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 7, 0, 1, 0, 9];
        assert_eq!(sizes_total(&two, false), Some(7 + 65_545));
        assert_eq!(sizes_total(&two[..19], false), None, "a table cut short");
        let nibbles = [0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 3, 0x12, 0x30];
        assert_eq!(sizes_total(&nibbles, true), Some(1 + 2 + 3));
        let bytes = [0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 2, 200, 100];
        assert_eq!(sizes_total(&bytes, true), Some(300));
        let wide = [0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 1, 1, 0];
        assert_eq!(sizes_total(&wide, true), Some(256));
        let odd = [0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 1, 1, 0];
        assert_eq!(sizes_total(&odd, true), None);
        assert_eq!(sizes_total(&[0; 8], false), None);
        assert_eq!(bits_per_second(1_000, 2, 1), 4_000);
        assert_eq!(bits_per_second(1_000, 0, 1), 0, "no duration, no rate");
        assert_eq!(bits_per_second(u64::MAX, 1, u32::MAX), u64::MAX, "saturates");
    }

    #[test]
    fn declared_counts_trust_only_what_the_box_holds() {
        let constant = [0, 0, 0, 0, 0, 0, 0, 5, 0, 0x0F, 0x42, 0x40];
        assert_eq!(declared_count(&constant, false).unwrap(), 1_000_000);
        let two = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 9];
        assert_eq!(declared_count(&two, false).unwrap(), 2);
        let mut three = two;
        three[11] = 3;
        assert!(declared_count(&three, false).is_err());
        let nibbles = [0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 3, 0x12, 0x30];
        assert_eq!(declared_count(&nibbles, true).unwrap(), 3);
        let mut odd = nibbles;
        odd[7] = 3;
        assert!(declared_count(&odd, true).is_err());
        assert!(declared_count(&[0, 0, 0], false).is_err());
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
        out.sort_by_key(|s| s.offset);
        let order: Vec<_> = out.iter().map(|s| (s.track_index, s.offset)).collect();
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
    fn only_a_constant_size_of_one_or_the_frame_groups_pcm() {
        let stsz = |size: u32| {
            let mut b = vec![0u8; 4];
            b.extend(size.to_be_bytes());
            b.extend(9u32.to_be_bytes());
            b
        };
        assert_eq!(pcm_group(&stsz(1), false, 4), 4);
        assert_eq!(pcm_group(&stsz(4), false, 4), 4);
        assert_eq!(pcm_group(&stsz(2), false, 4), 0, "another constant folds per entry");
        assert_eq!(pcm_group(&stsz(0), false, 4), 0, "a per-sample table folds per entry");
        assert_eq!(pcm_group(&stsz(1), true, 4), 0, "stz2 is never constant");
        assert_eq!(pcm_group(&stsz(1), false, 0), 0, "not pcm");
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
        assert_eq!((out[0].offset, out[0].size, out[0].dts), (64, 2048, 0));
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
        let got: Vec<_> = out.iter().map(|s| (s.offset, s.dts, s.pts)).collect();
        let want = [(0, 0, 0), (2, 1, 1), (4, 2, 2), (6, 3, 3)];
        let tail = [(10, 8, 17), (12, 9, 18), (14, 10, 19), (16, 11, 20)];
        assert_eq!(got, [&want[..], &tail[..]].concat());
        assert_eq!(out.committed(), 8 * size_of::<Sample>());
    }
}
