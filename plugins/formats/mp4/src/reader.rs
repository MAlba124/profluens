//! The MP4 demux engine (ISO/IEC 14496-12). [`Mp4Reader::new`] resolves the `moov` sample
//! tables up front and keeps them in the file's shape ([`crate::stbl`]), then the whole file
//! is pushed and each sample is sliced out of the retained input chunks by absolute offset.
//! Fragmented movies walk `moof`/`mdat` instead.

use std::collections::VecDeque;
use std::sync::Arc;

use profluens_core::ctx::Disposition;
use profluens_core::memory::{Demand, IO_DEPTH_BYTES, Known, Memory};
pub use profluens_core::pipeline::CAPTION_REACH as CAPTION_REACH_NS;
use profluens_core::pipeline::{Caption, Index, IndexEntry};
use profluens_core::time::Timestamp;
use profluens_video::rap::{self, NalSyntax};

use crate::boxes::{self, BoxError, BoxHeader};
use crate::codec::{self, Reframer, SampleEntry};
use crate::edit::{self, EditList, EditRefusal};
use crate::frag::{self, FragmentRef, MAX_FRAGMENT_HEAD, Trex};
pub use crate::stbl::SampleId;
use crate::stbl::{Cursor, Keys, Seeker, TrackIter, TrackTable};
use pf_mkv::ReframeError;
use stein_arena::ArenaError;

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
/// Run-length tables let a tiny head declare billions. The tables are held as stated and cost
/// what their boxes do, the limit bounds the walk of every sample an open checks them by, so
/// an embedder that knows the file length passes it in [`Limits`].
pub const MAX_SAMPLES: usize = 1 << 24;

/// What one reader may take into its sample tables, across all its tracks.
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

/// 24 octets, a sample as the tables give it. Only a fragment's run holds them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// The file offset above the track index in the low octet, a track index is below
    /// [`MAX_TRACKS`]. Ascending is file order, then track.
    pub(crate) place: u64,
    pub(crate) dts: i64,
    /// `pts - dts`, the composition offset.
    cts: i32,
    /// The size in the low 30 bits, sync and hidden above.
    size_flags: u32,
}

const _: () = assert!(size_of::<Sample>() == 24);
const _: () = assert!(MAX_TRACKS <= 1 << Sample::TRACK_BITS);

impl Sample {
    pub(crate) const TRACK_BITS: u32 = 8;
    /// The sizes held, as FFmpeg's index entry holds them.
    pub const MAX_SIZE: u32 = (1 << 30) - 1;
    /// The offsets held.
    pub const MAX_OFFSET: u64 = (1 << (64 - Self::TRACK_BITS)) - 1;
    const SYNC: u32 = 1 << 30;
    const HIDDEN: u32 = 1 << 31;

    /// A sample of track `track_index`, below [`MAX_TRACKS`], its times in media ticks.
    /// Refused past [`Sample::MAX_SIZE`] or [`Sample::MAX_OFFSET`], and with times further
    /// apart than a composition offset states.
    pub fn new(
        track_index: u16,
        offset: u64,
        size: u32,
        dts: i64,
        pts: i64,
        sync: bool,
    ) -> Result<Sample, Mp4Error> {
        if size > Self::MAX_SIZE {
            return Err(Mp4Error::Inconsistent("a sample of 1 GiB or more"));
        }
        if offset > Self::MAX_OFFSET {
            return Err(Mp4Error::Inconsistent("a sample offset past 2^56"));
        }
        if usize::from(track_index) >= MAX_TRACKS {
            return Err(Mp4Error::Inconsistent("a sample of a track past the most resolved"));
        }
        let cts = pts.checked_sub(dts).and_then(|by| i32::try_from(by).ok());
        let cts = cts.ok_or(Mp4Error::Inconsistent("a composition offset past 32 bits"))?;
        let place = offset << Self::TRACK_BITS | u64::from(track_index);
        Ok(Sample { place, dts, cts, size_flags: size | if sync { Self::SYNC } else { 0 } })
    }

    pub fn track(&self) -> usize {
        (self.place & ((1 << Self::TRACK_BITS) - 1)) as usize
    }

    pub fn track_index(&self) -> u16 {
        self.track() as u16
    }

    pub fn offset(&self) -> u64 {
        self.place >> Self::TRACK_BITS
    }

    pub fn size(&self) -> u32 {
        self.size_flags & Self::MAX_SIZE
    }

    /// Decode time in media ticks, moved by the edit list. Signed, an edit that trims the
    /// composition lead makes the first DTS negative (the ffmpeg convention).
    pub fn dts(&self) -> i64 {
        self.dts
    }

    /// Presentation time in media ticks on the edit list's timeline. Signed.
    pub fn pts(&self) -> i64 {
        self.dts.saturating_add(i64::from(self.cts))
    }

    pub fn sync(&self) -> bool {
        self.size_flags & Self::SYNC != 0
    }

    /// Needed by referring samples but left out by the edit list. Leaves as `DECODE_ONLY`.
    pub fn hidden(&self) -> bool {
        self.size_flags & Self::HIDDEN != 0
    }

    pub(crate) fn hide(&mut self) {
        self.size_flags |= Self::HIDDEN;
    }

    /// Both times `by` earlier.
    pub(crate) fn back(&mut self, by: i64) {
        self.dts = self.dts.saturating_sub(by);
    }

    /// Presents at `pts`, the decode time moved as far.
    pub(crate) fn present_at(&mut self, pts: i64) {
        self.dts = self.dts.saturating_add(pts.saturating_sub(self.pts()));
    }
}

impl std::fmt::Debug for Sample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sample")
            .field("track_index", &self.track_index())
            .field("offset", &self.offset())
            .field("size", &self.size())
            .field("dts", &self.dts())
            .field("pts", &self.pts())
            .field("sync", &self.sync())
            .field("hidden", &self.hidden())
            .finish()
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
    /// The sample delta most samples take in media ticks, the `stts` one, else a fragmented
    /// movie's `trex` default. 0 for unknown.
    pub frame_ticks: u32,
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
        // an SBR frame is 1024 samples of the core rate, the entry states the output rate
        let core = match self.entry.family {
            "aac" => profluens_audio::asc::parse(&self.entry.config_record).map(|a| a.rate),
            _ => None,
        };
        pre_roll_ns(self.entry.family, core.unwrap_or(self.entry.sample_rate), self.timescale)
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
    /// Each track's sample tables as the file states them, see [`crate::stbl`]. A sample is
    /// read out of them, none is held.
    stbl: Vec<TrackTable>,
    /// The samples the tables hold, those an edit list leaves out and those past the end of
    /// the file not counted.
    count: usize,
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
        write!(f, "Tables {{ tracks: {}, samples: {} }}", self.tracks.len(), self.count)
    }
}

/// The file octets left for samples. Disjoint samples inside a file hold at most its length.
#[derive(Clone, Copy)]
pub(crate) struct Room {
    end: u64,
    left: u64,
}

impl Default for Room {
    fn default() -> Self {
        Room { end: u64::MAX, left: u64::MAX }
    }
}

impl Room {
    pub(crate) fn of(file_len: Option<u64>) -> Self {
        file_len.map_or(Room::default(), |end| Room { end, left: end })
    }

    /// False for a sample past the end, a truncated file's tail. Past the octets left, the
    /// samples overlap, so the index never holds more samples than the file has octets.
    pub(crate) fn admits(&mut self, offset: u64, size: u32) -> Result<bool, Mp4Error> {
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

#[derive(Clone, Copy)]
struct Rule {
    kind: TrackKind,
    timescale: u32,
    delay_ticks: i64,
    pre_roll_ns: u64,
    media_time: i64,
    /// The NAL syntax and length size a sample's random access is read by.
    rap: Option<(NalSyntax, u8)>,
}

/// Whether sample `id` is a sync sample its NAL types said is no random access point.
fn false_key(false_keys: &[SampleId], id: SampleId) -> bool {
    false_keys.binary_search(&id).is_ok()
}

/// Every sample of the tables in file order, see [`Tables::samples`].
pub struct Samples<'a> {
    stbl: &'a [TrackTable],
    cursor: Cursor,
    left: usize,
}

impl Iterator for Samples<'_> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let (_, s) = self.cursor.peek()?;
        self.cursor.advance(self.stbl);
        self.left = self.left.saturating_sub(1);
        Some(s)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for Samples<'_> {}

#[derive(Clone, Copy)]
struct Pick {
    best: (u64, i64),
    first: (u64, i64),
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
    /// The tracks and the sample tables of a head through `moov`, see [`Mp4Reader::bounded`].
    // Cold path, once per file.
    #[allow(clippy::disallowed_methods)]
    pub fn resolve(head: &[u8], mfra: &[u8], limits: Limits) -> Result<Tables, Mp4Error> {
        let moov = find_moov(head)?.ok_or(Mp4Error::Missing("moov"))?;
        let mut movie = resolve_moov(moov.body(head), Some(limits))?;
        if let Some(trex) = &movie.trex {
            fragment_rates(head, moov.end, &mut movie.tracks, trex);
        }
        let rules = rules_of(&movie.tracks);
        let frag = movie.trex.map(|trex| {
            let mut fragments = Vec::new();
            let indexed_to = index_head(head, moov.end, &movie.tracks, &mut fragments);
            index_mfra(mfra, &movie.tracks, &mut fragments);
            fragments.sort_unstable();
            fragments.dedup_by_key(|f| f.offset);
            let duration_ns = if movie.duration_ns != 0 { movie.duration_ns } else { indexed_to };
            FragTables { trex, fragments, start: moov.end as u64, duration_ns }
        });
        let skipped = movie.skipped;
        let (stbl, count) = (movie.stbl, movie.count);
        Ok(Tables { tracks: movie.tracks, stbl, count, rules, frag, skipped })
    }

    /// The tracks of a head through `moov` with no sample folded, for a caller that only
    /// describes them. A table the fold refuses is not seen here.
    pub fn tracks_of(head: &[u8]) -> Result<Vec<Track>, Mp4Error> {
        let moov = find_moov(head)?.ok_or(Mp4Error::Missing("moov"))?;
        let mut movie = resolve_moov(moov.body(head), None)?;
        if let Some(trex) = &movie.trex {
            fragment_rates(head, moov.end, &mut movie.tracks, trex);
        }
        Ok(movie.tracks)
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Tracks of the movie left out for a missing sample table, chapter and metadata ones often.
    pub fn skipped_tracks(&self) -> u32 {
        self.skipped
    }

    /// Every sample in file order, each read out of its track's tables as the walk reaches it.
    pub fn samples(&self) -> Samples<'_> {
        Samples { stbl: &self.stbl, cursor: Cursor::new(&self.stbl), left: self.count }
    }

    /// The samples [`Tables::samples`] gives.
    pub fn sample_count(&self) -> usize {
        self.count
    }

    /// Octets the sample tables hold, what the `stbl` boxes stated and little over.
    pub fn index_bytes(&self) -> usize {
        size_of_val(self.stbl.as_slice()) + self.stbl.iter().map(TrackTable::bytes).sum::<usize>()
    }

    /// Track `track`'s samples in the order the file stores them.
    fn of_track(&self, track: usize) -> TrackIter<'_> {
        TrackIter::new(self.table(track), track)
    }

    /// Track `track`'s tables, empty ones for a track there is none of.
    fn table(&self, track: usize) -> &TrackTable {
        static NONE: std::sync::LazyLock<TrackTable> =
            std::sync::LazyLock::new(TrackTable::default);
        self.stbl.get(track).unwrap_or(&NONE)
    }

    /// The sizes of track `track`'s samples in the order the file stores them.
    fn sizes_of(&self, track: usize) -> impl Iterator<Item = u64> + '_ {
        let listed = self.table(track).sizes();
        let walked = listed.is_none().then(|| self.of_track(track).map(|(_, s)| s.size()));
        listed.into_iter().flatten().chain(walked.into_iter().flatten()).map(u64::from)
    }

    /// About all the tables hold, the sample tables and the tracks with their buffers by length.
    pub fn bytes(&self) -> usize {
        let tracks: usize = self
            .tracks
            .iter()
            .map(|t| {
                let e = &t.entry;
                e.codec_head.len() + e.config_record.len() + t.title.len() + t.language_tag.len()
            })
            .sum();
        let frag = self
            .frag
            .as_ref()
            .map_or(0, |f| size_of_val(f.trex.as_slice()) + size_of_val(f.fragments.as_slice()));
        self.index_bytes()
            + size_of_val(self.tracks.as_slice())
            + tracks
            + size_of_val(self.rules.as_slice())
            + frag
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
        for size in self.sizes_of(track) {
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
        let mut of = self.sizes_of(track);
        let mut behind = self.sizes_of(track);
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
        self.gates(Some(point), video_from, 0, on, &[], &mut gates)
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

    /// Where caption track `track` joining at `point` reads from wherever it is stored, its
    /// sample up there, else its first behind the point. None for any other track.
    fn caption_from(&self, track: usize, point: u64) -> Option<u64> {
        let rule = self.rules.get(track).filter(|_| self.caption(track, &|_| true))?;
        let bound = ticks_at_or_before(point, rule.timescale);
        let mut up = None;
        for (_, s) in self.of_track(track) {
            if s.pts().saturating_add(rule.delay_ticks).max(0) > bound {
                return up.or(Some(s.offset()));
            }
            up = self.caption_up(track, point, s.pts()).then_some(s.offset());
        }
        up
    }

    /// The last sync sample of `track` before sample `from`, in the tables and in the file,
    /// that `false_keys` does not name.
    fn key_before(
        &self,
        track: usize,
        from: SampleId,
        false_keys: &[SampleId],
    ) -> Option<(SampleId, Sample)> {
        self.stbl.get(track)?.keys_before(from.number()).find_map(|n| {
            let (id, s) = self.of_track(track).at_sample(n).next()?;
            let key = id.number() == n && id < from && s.sync() && !false_key(false_keys, id);
            key.then_some((id, s))
        })
    }

    pub fn is_fragmented(&self) -> bool {
        self.frag.is_some()
    }

    /// The ticks text sample `id` is up, to its track's next sample. None for another kind
    /// and for a track's last.
    pub fn span(&self, id: SampleId) -> Option<i64> {
        let track = self.tracks.get(id.track())?;
        if !track.is_text() {
            return None;
        }
        let mut own = self.of_track(id.track()).at_sample(id.number());
        let (at, s) = own.next()?;
        let (_, next) = own.next().filter(|_| at == id)?;
        Some(next.pts().saturating_sub(s.pts()))
    }

    pub fn fragments(&self) -> &[FragmentRef] {
        self.frag.as_ref().map_or(&[], |frag| &frag.fragments)
    }

    pub fn fragmented_duration_ns(&self) -> u64 {
        self.frag.as_ref().map_or(0, |frag| frag.duration_ns)
    }

    /// Raises a fragmented movie's length and each track's to where the `moof` atom `moof`, the
    /// file's last, ends them. See [`frag::TailAt`].
    // Cold path, once per open.
    #[allow(clippy::disallowed_methods)]
    pub fn last_fragment(&mut self, moof: &[u8]) {
        let Some(frag) = self.frag.as_mut() else { return };
        let Some(h) =
            boxes::read_box_header(moof, 0).ok().filter(|h| h.kind == boxes::boxtype::MOOF)
        else {
            return;
        };
        let mut ends = vec![None; self.tracks.len()];
        let mut deltas = vec![Tally::NEW; self.tracks.len()];
        let movie = frag::Movie { tracks: &self.tracks, trex: &frag.trex };
        if frag::summarize(h.body(moof), &movie, &mut ends, &mut deltas).is_err() {
            return;
        }
        for (track, end) in self.tracks.iter_mut().zip(ends) {
            let Some(end) = end else { continue };
            let ns = track.stamp_ns(end.saturating_sub(track.media_time));
            track.duration_ns = track.duration_ns.max(ns);
            frag.duration_ns = frag.duration_ns.max(ns);
        }
    }

    /// Adds the fragments the `sidx` atoms a [`frag::TailAt`] walk reached index, and raises
    /// the movie's length and each track's to where they end. A walk stopped at its cap at
    /// byte `at` of a file of `len` scales the movie's length by bytes and leaves the tracks'.
    // Cold path, once per open.
    #[allow(clippy::disallowed_methods)]
    pub fn sidx_chain(&mut self, tail: &frag::Tail, len: u64) {
        let Tables { tracks, frag: Some(frag), .. } = self else { return };
        let (mut at, mut walked) = (0usize, 0u64);
        for &end in &tail.sidx_ends {
            let Ok(h) = boxes::read_box_header(&tail.sidx, at) else { break };
            at = h.end;
            let Ok(sidx) = frag::parse_sidx(h.body(&tail.sidx)) else { continue };
            let index = tracks.iter().position(|t| t.track_id == sidx.reference_id);
            frag::sidx_fragments(&sidx, end, index.map(|i| &tracks[i]), &mut frag.fragments);
            let ns = frag::sidx_ns(&sidx, index.map(|i| &tracks[i]), frag::sidx_end(&sidx));
            walked = walked.max(ns);
            if let (None, Some(i)) = (tail.capped_at, index) {
                tracks[i].duration_ns = tracks[i].duration_ns.max(ns);
            }
        }
        frag.fragments.sort_unstable();
        frag.fragments.dedup_by_key(|f| f.offset);
        let ns = match tail.capped_at {
            Some(at) if at > 0 => (u128::from(walked) * u128::from(len) / u128::from(at))
                .min(u128::from(u64::MAX)) as u64,
            _ => walked,
        };
        frag.duration_ns = frag.duration_ns.max(ns);
    }

    /// The time to byte map of the movie. Sync samples of the first
    /// video track, else the first audio track thinned to [`MAX_INDEX_ENTRIES`], or one entry
    /// per indexed fragment. An entry restarts ahead for audio pre-roll and for the sample up
    /// on a caption track `on`.
    pub fn seek_index(&self, on: &dyn Fn(usize) -> bool) -> Index {
        self.index_without(on, &[])
    }

    /// [`Self::seek_index`] without the sync samples `false_keys` names, see
    /// [`Mp4Reader::keyed`].
    // Cold path, once per open, link change and false key found.
    #[allow(clippy::disallowed_methods)]
    fn index_without(&self, on: &dyn Fn(usize) -> bool, false_keys: &[SampleId]) -> Index {
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
        let keys = || {
            let true_key = |(id, _): &(SampleId, Sample)| !false_key(false_keys, *id);
            Keys::new(self.table(track), track, 0).filter(true_key)
        };
        // With every sample a key and none left out, their count is known and a step over
        // them is a step of the walk's place.
        let listed = Keys::new(self.table(track), track, 0).len().filter(|_| false_keys.is_empty());
        let count = listed.unwrap_or_else(|| keys().count());
        let stride = count.div_ceil(MAX_INDEX_ENTRIES).max(1);
        let seeker = |(i, rule): (usize, &Rule)| {
            Seeker::new(self.table(i), i, rule.kind == TrackKind::Video, 0)
        };
        let cursors = self.rules.iter().enumerate().map(seeker).collect();
        let mut sweep = Sweep { tables: self, cursors, false_keys };
        let every = listed.map(|_| Keys::new(self.table(track), track, 0).step_by(stride));
        let thinned = listed.is_none().then(|| keys().step_by(stride));
        for (_, s) in every.into_iter().flatten().chain(thinned.into_iter().flatten()) {
            let time_ns = self.tracks[track].stamp_ns(s.pts());
            if ascends(&entries, time_ns, s.offset()) {
                let r = sweep.restart(time_ns, on);
                entries.push(IndexEntry {
                    time_ns,
                    byte: s.offset(),
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
            let first = captions.len();
            for (_, s) in self.of_track(i) {
                let time_ns = t.stamp_ns(s.pts());
                let end_ns = time_ns.saturating_add(CAPTION_REACH_NS);
                if let Some(last) = captions.get_mut(first..).and_then(<[Caption]>::last_mut) {
                    last.end_ns = time_ns.max(last.time_ns);
                }
                captions.push(Caption { stream: i as u32, time_ns, end_ns, byte: s.offset() });
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
        self.gates(Some(point), point, 0, on, &[], &mut gates);
        let to_byte = gates.get(track).copied().unwrap_or(0);
        let sample = self.of_track(track).at_byte(to_byte).next();
        let stamp = sample.zip(self.tracks.get(track)).map(|((_, s), t)| t.stamp_ns(s.pts()));
        Restart { to_byte, at_ns: stamp.unwrap_or(point) }
    }

    /// The offset each track's samples pass from, into `gates`, behind a source that
    /// restarted at `floor`. Video from the last sync sample at or before `video_from`,
    /// audio from its pre-roll ahead of `point`, a caption track `on` from its sample up at
    /// `point`, anything else whole. Each track is walked from the floor to its own bound.
    fn gates(
        &self,
        point: Option<u64>,
        video_from: u64,
        floor: u64,
        on: &dyn Fn(usize) -> bool,
        false_keys: &[SampleId],
        gates: &mut [u64; MAX_TRACKS],
    ) -> Restart {
        const NONE: (u64, i64) = (u64::MAX, 0);
        let mut picks = [Pick { best: NONE, first: NONE }; MAX_TRACKS];
        for (i, (pick, rule)) in picks.iter_mut().zip(&self.rules).enumerate() {
            let bound_ns = match (rule.kind, point) {
                (TrackKind::Video, _) => video_from,
                (TrackKind::Audio, Some(point)) => point.saturating_sub(rule.pre_roll_ns),
                (TrackKind::Other, Some(point)) if self.caption(i, on) => point,
                _ => continue,
            };
            let bound = ticks_at_or_before(bound_ns, rule.timescale);
            let mut seeker = Seeker::new(self.table(i), i, rule.kind == TrackKind::Video, floor);
            // A stamp before zero is stamped zero.
            let past = |pts: i64| pts.saturating_add(rule.delay_ticks).max(0) > bound;
            seeker.advance(&past, &|id| false_key(false_keys, id));
            let place = |(_, s): (SampleId, Sample)| (s.offset(), s.pts());
            pick.first = seeker.first.map_or(NONE, place);
            pick.best = seeker.best.map_or(NONE, place);
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

/// The restarts of rising points, each track's samples taken up to the last point's bound.
struct Sweep<'a> {
    tables: &'a Tables,
    cursors: Vec<Seeker<'a>>,
    false_keys: &'a [SampleId],
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
            // A stamp before zero is stamped zero.
            let past = |pts: i64| pts.saturating_add(rule.delay_ticks).max(0) > bound;
            cursor.advance(&past, &|id| false_key(self.false_keys, id));
            let place = |(_, s): (SampleId, Sample)| (s.offset(), s.pts());
            let (best, first) = (cursor.best.map(place), cursor.first.map(place));
            // A caption is up from its sample on, a later one is not
            let pick = match rule.kind {
                TrackKind::Other => best.filter(|&(_, pts)| tables.caption_up(i, point, pts)),
                _ => best.or(first),
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
    // From the first sample at or past `byte`, what lies before went out before a restart.
    Past { byte: u64 },
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
    /// The walk of the tables' samples, at the next one out.
    cursor: Cursor,
    /// The sample of the tables given last.
    last: Option<SampleId>,
    stream_base: u64,
    chunks: VecDeque<Chunk>,
    buffered: u64,
    gates: [u64; MAX_TRACKS],
    off: [bool; MAX_TRACKS],
    walk: Option<Walk>,
    /// The sync samples whose NAL types said no random access point, ascending. The tables are
    /// shared and stay as read.
    false_keys: Vec<SampleId>,
    landing: [Land; MAX_TRACKS],
    /// A false key was found since [`Self::keys_changed`] last said so.
    found: bool,
}

const SHUT: u64 = u64::MAX;

/// Where a track stands on the random access point its next sample out must be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Land {
    Off,
    /// A restart landed, its first sample is read for one.
    On,
    /// Its first sample was none and nothing earlier is read, it waits for the next.
    Waiting,
}

/// What the demuxer does with a sample whose NAL types were read, see [`Mp4Reader::keyed`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keyed {
    /// Put out, a random access point or not.
    Put { key: bool },
    /// Not put out, its track restarts at the sync sample before it, read next.
    Back,
    /// Not put out, its track waits for a random access point.
    Skip,
}

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

    /// As [`Self::indexed`], the sample tables held to `limits`.
    pub fn bounded(head: &[u8], mfra: &[u8], limits: Limits) -> Result<Self, Mp4Error> {
        Ok(Self::shared(Arc::new(Tables::resolve(head, mfra, limits)?)))
    }

    /// A reader over tables resolved once and shared, so another run over the file folds
    /// nothing.
    // Cold path, once at construction.
    #[allow(clippy::disallowed_methods)]
    pub fn shared(tables: Arc<Tables>) -> Self {
        let walk = tables.frag.as_ref().map(|_| Walk::new(tables.tracks.len()));
        let cursor = Cursor::new(&tables.stbl);
        let mut reader = Self {
            tables,
            cursor,
            last: None,
            stream_base: 0,
            chunks: VecDeque::new(),
            buffered: 0,
            gates: [0; MAX_TRACKS],
            off: [false; MAX_TRACKS],
            walk,
            false_keys: Vec::new(),
            landing: [Land::Off; MAX_TRACKS],
            found: false,
        };
        reader.reset_stream();
        reader
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tables.tracks
    }

    pub fn samples(&self) -> Samples<'_> {
        self.tables.samples()
    }

    pub fn tables(&self) -> Arc<Tables> {
        Arc::clone(&self.tables)
    }

    pub fn reset_stream(&mut self) {
        self.cursor.seek_byte(&self.tables.stbl, 0);
        self.last = None;
        self.stream_base = 0;
        self.chunks.clear();
        self.buffered = 0;
        self.gates = self.off.map(|off| if off { SHUT } else { 0 });
        self.landing = [Land::Off; MAX_TRACKS];
        let Self { walk, tables, off, .. } = self;
        if let Some(walk) = walk {
            walk.restart(0);
            (walk.shift, walk.anchor) = (0, None);
            for (gate, off) in walk.gates.iter_mut().zip(off.iter()) {
                *gate = if *off { Gate::Shut } else { Gate::Open };
            }
            // A movie may keep its first samples in its `moov` and the rest in fragments.
            walk.run.extend(tables.samples().filter(|s| !off[s.track()]));
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
        self.land(track, on);
        if let Some(gate) = self.walk.as_mut().and_then(|walk| walk.gates.get_mut(track)) {
            *gate = if on { Gate::NextSync } else { Gate::Shut };
            return;
        }
        let video = self.tables.rules.get(track).is_some_and(|rule| rule.kind == TrackKind::Video);
        let keys = &self.false_keys;
        let next = |(id, s): &(SampleId, Sample)| (s.sync() && !false_key(keys, *id)) || !video;
        let from = self.cursor.rest(&self.tables.stbl, track).and_then(|mut rest| rest.find(next));
        self.gates[track] = if on { from.map_or(SHUT, |(_, s)| s.offset()) } else { SHUT };
    }

    /// `track`'s next sample out is read for a random access point, where its samples can say.
    fn land(&mut self, track: usize, on: bool) {
        let read = self.tables.rules.get(track).is_some_and(|rule| rule.rap.is_some());
        if let Some(land) = self.landing.get_mut(track) {
            *land = if on && read { Land::On } else { Land::Off };
        }
    }

    /// Whether a track waits to read its first sample out for a random access point.
    pub fn is_landing(&self) -> bool {
        self.landing.iter().any(|land| *land != Land::Off)
    }

    /// No track waits for a random access point, nothing is left to read for one.
    pub fn landed(&mut self) {
        self.landing = [Land::Off; MAX_TRACKS];
    }

    pub fn rejoin(&mut self, track: usize, to_byte: u64, point: u64) {
        if track >= MAX_TRACKS {
            return;
        }
        if let Some(walk) = self.walk.as_mut() {
            let resume = walk.resume();
            walk.restart(to_byte);
            for gate in walk.gates.iter_mut().filter(|gate| **gate == Gate::Open) {
                *gate = Gate::Past { byte: resume };
            }
            let rule = self.tables.rules.get(track);
            if let Some(gate) = walk.gates.get_mut(track) {
                *gate = match rule.map(|rule| (rule.kind, rule.pre_roll_ns)) {
                    Some((TrackKind::Video, _)) => Gate::From { bound_ns: point },
                    Some((TrackKind::Audio, lead)) => {
                        Gate::From { bound_ns: point.saturating_sub(lead) }
                    }
                    _ => Gate::Open,
                };
            }
            self.off[track] = false;
            self.land(track, true);
            self.chunks.clear();
            (self.buffered, self.stream_base) = (0, to_byte);
            return;
        }
        let resume = self.cursor.peek().map_or(SHUT, |(_, s)| s.offset());
        self.gates.iter_mut().for_each(|gate| *gate = (*gate).max(resume));
        let mut fresh = [0u64; MAX_TRACKS];
        let off = self.off;
        let keys = &self.false_keys;
        self.tables.gates(Some(point), point, to_byte, &|i| !off[i], keys, &mut fresh);
        (self.gates[track], self.off[track]) = (fresh[track], false);
        self.land(track, true);
        self.chunks.clear();
        self.buffered = 0;
        self.stream_base = to_byte;
        self.cursor.seek_byte(&self.tables.stbl, to_byte);
    }

    /// Caption track `track`, rejoined, reads from its sample up at `point` where that lies
    /// before its gate. Each sample is asked alone, so only a pulled source may go back for it.
    /// The other tracks keep their gates and put nothing out twice.
    pub fn rejoin_up(&mut self, track: usize, point: u64) {
        if self.walk.is_some() || track >= MAX_TRACKS || self.off[track] {
            return;
        }
        let Some(from) = self.tables.caption_from(track, point) else { return };
        if from < self.gates[track] {
            self.gates[track] = from;
            if self.cursor.peek().is_none_or(|(_, s)| from <= s.offset()) {
                self.cursor.seek_byte(&self.tables.stbl, from);
            }
        }
    }

    /// Follow a source that restarted at `to_byte` for a seek to `point`. Each track passes
    /// from its own sample, see [`Tables::restart`]. A fragmented stream gates by stamps as
    /// fragments arrive, and `to_byte` has to be the first byte of a box.
    pub fn seek_stream(&mut self, to_byte: u64, point: Option<u64>, video_from: u64) -> Restart {
        self.chunks.clear();
        self.buffered = 0;
        self.stream_base = to_byte;
        for track in 0..self.tables.rules.len().min(MAX_TRACKS) {
            self.land(track, !self.off[track]);
        }
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
        let keys = &self.false_keys;
        let restart =
            self.tables.gates(point, video_from, to_byte, &|i| !off[i], keys, &mut self.gates);
        for (gate, off) in self.gates.iter_mut().zip(self.off) {
            *gate = if off { SHUT } else { *gate };
        }
        self.cursor.seek_byte(&self.tables.stbl, restart.to_byte);
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
        let Some((id, s)) = self.wanted() else {
            // Bytes behind the last sample, a trailing `moov`, would pin the source's slots.
            self.drop_before(self.stream_base + self.buffered);
            return None;
        };
        let mut sample = self.slice(s)?;
        self.cursor.advance(&self.tables.stbl);
        if self.tables.tracks.get(s.track()).is_some_and(Track::is_text) {
            // up until its track's next, which the walk now stands at
            let next = self.cursor.front(s.track());
            sample.span = next.map(|(_, next)| next.pts().saturating_sub(s.pts()));
        }
        self.last = Some(id);
        Some(sample)
    }

    /// The next sample a gate lets out, the walk left at it.
    fn wanted(&mut self) -> Option<(SampleId, Sample)> {
        let Self { tables, cursor, gates, .. } = self;
        while cursor.peek().is_some_and(|(_, s)| s.offset() < gates[s.track()]) {
            cursor.advance(&tables.stbl);
        }
        cursor.peek()
    }

    pub fn span_at(&self, id: SampleId) -> Option<i64> {
        self.tables.span(id)
    }

    /// The sample [`Self::next_sample`] gave last, None for a fragment's.
    pub(crate) fn last_index(&self) -> Option<SampleId> {
        self.last.filter(|_| self.walk.is_none())
    }

    /// Whether sample `index` of `track`, `sync` by its tables, goes out as a keyframe, by the
    /// NAL types of its `data` for H.264 and H.265. A sync sample that is no random access point
    /// leaves the index [`Self::seek_index`] gives. The first a restart lands on steps its
    /// track back to the sync sample before it where the bytes before can be read again,
    /// `back`, else the track waits for the next sync sample.
    pub fn keyed(
        &mut self,
        track: usize,
        index: Option<SampleId>,
        sync: bool,
        data: &[u8],
        back: bool,
    ) -> Keyed {
        let Some((syntax, len)) = self.tables.rules.get(track).and_then(|rule| rule.rap) else {
            return Keyed::Put { key: sync };
        };
        let read = rap::random_access(syntax, len, data);
        let key = read.unwrap_or(sync);
        let false_sync = sync && read == Some(false);
        if let Some(i) = index.filter(|_| false_sync)
            && let Err(at) = self.false_keys.binary_search(&i)
        {
            self.false_keys.insert(at, i);
            self.found = true;
        }
        let land = self.landing.get(track).copied().unwrap_or(Land::Off);
        let step = match (land, key, false_sync) {
            (Land::Off, ..) => return Keyed::Put { key },
            (Land::On, false, true) if back => {
                match index.is_some_and(|i| self.step_back(track, i)) {
                    true => return Keyed::Back,
                    false => Keyed::Put { key },
                }
            }
            (Land::On, false, true) => {
                self.landing[track] = Land::Waiting;
                return Keyed::Skip;
            }
            (Land::Waiting, false, false) => return Keyed::Skip,
            _ => Keyed::Put { key },
        };
        self.landing[track] = Land::Off;
        step
    }

    /// Restarts `track` at the last sync sample before sample `from` its NAL types did not
    /// refuse, the other tracks going on past `from`. False when there is none.
    fn step_back(&mut self, track: usize, from: SampleId) -> bool {
        if self.walk.is_some() {
            return false;
        }
        let Some((id, s)) = self.tables.key_before(track, from, &self.false_keys) else {
            return false;
        };
        let past = from.offset().saturating_add(1);
        self.gates.iter_mut().for_each(|gate| *gate = (*gate).max(past));
        self.gates[track] = s.offset();
        self.cursor.seek_id(&self.tables.stbl, id);
        true
    }

    /// Whether a false key was found since last asked, so the index is published again.
    pub fn keys_changed(&mut self) -> bool {
        std::mem::take(&mut self.found)
    }

    /// The seek index of the tables without the false keys found so far.
    pub fn seek_index(&self, on: &dyn Fn(usize) -> bool) -> Index {
        self.tables.index_without(on, &self.false_keys)
    }

    pub fn next_wanted(&mut self) -> Option<(SampleId, Sample)> {
        if self.walk.is_some() {
            return None;
        }
        let next = self.wanted()?;
        self.cursor.advance(&self.tables.stbl);
        Some(next)
    }

    /// The walk back at sample `id`, when it is past it.
    pub fn rewind_to(&mut self, id: SampleId) {
        if self.cursor.peek().is_none_or(|(next, _)| id < next) {
            self.cursor.seek_id(&self.tables.stbl, id);
        }
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
        let start = s.offset();
        let end = s.offset().checked_add(s.size() as u64)?;
        self.drop_before(start);

        // an empty sample needs no byte, one behind the window does not wait for it
        let gone = start < self.stream_base && s.size() != 0;
        if gone || end > self.stream_base + self.buffered {
            self.gather_from(start);
            return None;
        }
        if s.size() == 0 {
            // Zero-length sample. `Vec::new()` does not allocate.
            #[allow(clippy::disallowed_methods)]
            return Some(ResolvedSample {
                track_index: s.track(),
                pts: s.pts(),
                dts: s.dts,
                sync: s.sync(),
                hidden: s.hidden(),
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
            Chunk::Retained(m) => m.slice(rel, s.size() as usize),
            Chunk::Owned(_) => None,
        };
        let payload = match sliced {
            Some(m) => SamplePayload::Slice(m),
            None => {
                // Cold path, only for a sample crossing retained chunk bounds.
                #[allow(clippy::disallowed_methods)]
                let mut v = Vec::with_capacity(s.size() as usize);
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
            pts: s.pts(),
            dts: s.dts,
            sync: s.sync(),
            hidden: s.hidden(),
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
                                    span_in(&walk.run, walk.next).or(end.map(|e| e - s.pts()));
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
            walk.run.retain(|s| {
                s.offset() >= body && s.offset().saturating_add(u64::from(s.size())) <= end
            });
        }
        // Disjoint offsets, so an unstable sort loses nothing and allocates nothing.
        walk.run.sort_unstable_by_key(|s| s.offset());
        let mut covered = body;
        for s in &walk.run {
            if s.offset() < covered {
                walk.error.get_or_insert(Mp4Error::Inconsistent("samples overlap in the fragment"));
                walk.run.clear();
                break;
            }
            covered = s.offset() + u64::from(s.size());
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
        let stbl: Vec<TrackTable> = movie.tracks.iter().map(|_| TrackTable::default()).collect();
        self.cursor = Cursor::new(&stbl);
        self.tables = Arc::new(Tables {
            tracks: movie.tracks,
            stbl,
            count: 0,
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
    let next = samples.get(at + 1..)?.iter().find(|n| n.track_index() == s.track_index())?;
    Some(next.pts().saturating_sub(s.pts()))
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
    stbl: Vec<TrackTable>,
    count: usize,
    trex: Option<Vec<Trex>>,
    duration_ns: u64,
    skipped: u32,
}

/// The tracks of a `moov` that resolve, each with its sample tables as the boxes hold them.
struct Traks<'a> {
    tracks: Vec<Track>,
    stbls: Vec<Option<Stbl<'a>>>,
    skipped: u32,
    /// A fragmented movie carries `mvex` in `moov` (8.8.1).
    mvex: Option<BoxHeader>,
    movie_scale: u32,
}

// Cold path, once per movie and per init segment.
#[allow(clippy::disallowed_methods)]
fn resolve_traks(moov_body: &[u8]) -> Result<Traks<'_>, Mp4Error> {
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
    // A trak that does not resolve is skipped like one with no tables, its error said when no
    // track is left.
    let mut first_error = None;
    for trak in &trak_boxes {
        let trak_body = trak.body(moov_body);
        let resolved = resolve_track(trak_body, tracks.len(), mvex.is_some(), movie_scale);
        let (track, stbl) = match resolved {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                skipped += 1;
                continue;
            }
            Err(e) => {
                (skipped, first_error) = (skipped + 1, first_error.or(Some(e)));
                continue;
            }
        };
        if tracks.len() == MAX_TRACKS {
            return Err(Mp4Error::TooManyTracks { limit: MAX_TRACKS });
        }
        tracks.push(track);
        stbls.push(stbl);
    }

    if tracks.is_empty() {
        return Err(first_error.unwrap_or(Mp4Error::Missing("no resolvable track in moov")));
    }
    Ok(Traks { tracks, stbls, skipped, mvex, movie_scale })
}

/// The samples of a head as the flat index held them, see [`crate::flat`].
#[cfg(test)]
pub(crate) fn flat_of(head: &[u8], limits: Limits) -> Result<Vec<Sample>, Mp4Error> {
    let moov = find_moov(head)?.ok_or(Mp4Error::Missing("moov"))?;
    let traks = resolve_traks(moov.body(head))?;
    Ok(crate::flat::fold(&traks.stbls, limits)?.0)
}

/// Each track's tables of `stbls` held to `limits`, the samples they hold, and the tracks
/// whose edit list presents none.
// Cold path, once per movie.
#[allow(clippy::disallowed_methods)]
pub(crate) fn fold_tables(
    stbls: &[Option<Stbl<'_>>],
    limits: Limits,
) -> Result<(Vec<TrackTable>, usize, Vec<usize>), Mp4Error> {
    let limit = limits.samples.min(MAX_SAMPLES);
    stbls
        .iter()
        .flatten()
        .try_fold(0usize, |n, s| n.checked_add(s.count).filter(|&n| n <= limit))
        .ok_or(Mp4Error::TooManySamples { limit })?;
    let mut stbl: Vec<TrackTable> = stbls.iter().map(|_| TrackTable::default()).collect();
    let mut unpresented = Vec::new();
    let mut room = Room::of(limits.file_len);
    // The track of the fewest samples first, the order the room is charged in.
    let mut order: Vec<usize> = (0..stbls.len()).collect();
    order.sort_by_key(|&i| stbls[i].as_ref().map_or(0, |s| s.count));
    for track_index in order {
        let Some(s) = &stbls[track_index] else { continue };
        let (table, presented) = TrackTable::fold(track_index as u16, s, &mut room)?;
        if !presented {
            unpresented.push(track_index);
        }
        stbl[track_index] = table;
    }
    // Samples must be disjoint byte ranges. Overlapping ones would emit the same bytes per
    // sample, an output unbounded by the input. Empty samples are exempt.
    if let Some(count) = crate::stbl::disjoint(&stbl) {
        return Ok((stbl, count?, unpresented));
    }
    let (mut covered, mut count) = (0u64, 0usize);
    let mut walk = Cursor::new(&stbl);
    while let Some((_, s)) = walk.peek() {
        if s.size() != 0 && s.offset() < covered {
            return Err(Mp4Error::Inconsistent("samples overlap in the file"));
        }
        covered = covered.max(s.offset().saturating_add(u64::from(s.size())));
        count += 1;
        walk.advance(&stbl);
    }
    Ok((stbl, count, unpresented))
}

/// `None` resolves the tracks and leaves the sample tables unfolded.
// Cold path, once per movie and per init segment.
#[allow(clippy::disallowed_methods)]
fn resolve_moov(moov_body: &[u8], limits: Option<Limits>) -> Result<Movie, Mp4Error> {
    let Traks { mut tracks, stbls, skipped, mvex, movie_scale } = resolve_traks(moov_body)?;

    let (mut stbl, mut count) = (tracks.iter().map(|_| TrackTable::default()).collect(), 0);
    if let Some(limits) = limits {
        let unpresented;
        (stbl, count, unpresented) = fold_tables(&stbls, limits)?;
        for track_index in unpresented {
            tracks[track_index].edit_refused = Some(EditRefusal::NothingPresented);
        }
    }

    let Some(mvex) = mvex else {
        return Ok(Movie { tracks, stbl, count, trex: None, duration_ns: 0, skipped });
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
    Ok(Movie { tracks, stbl, count, trex: Some(trex), duration_ns, skipped })
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
            rap: rap_of(track),
        })
        .collect()
}

fn rap_of(track: &Track) -> Option<(NalSyntax, u8)> {
    let syntax = match track.entry.family {
        "h264/annexb" => NalSyntax::Avc,
        "h265/annexb" => NalSyntax::Hevc,
        _ => return None,
    };
    Some((syntax, rap::length_size(syntax, &track.entry.config_record)))
}

/// The fragments the `sidx` atoms behind the `moov` index, and the end of the last.
fn index_head(head: &[u8], mut at: usize, tracks: &[Track], out: &mut Vec<FragmentRef>) -> u64 {
    let mut end_ns = 0u64;
    while let Ok(h) = boxes::read_box_header(head, at) {
        if h.kind == boxes::boxtype::MOOF || h.kind == boxes::boxtype::MDAT {
            break;
        }
        if let Some(sidx) =
            (h.kind == boxes::boxtype::SIDX).then(|| frag::parse_sidx(h.body(head)).ok()).flatten()
        {
            let track = tracks.iter().find(|t| t.track_id == sidx.reference_id);
            frag::sidx_fragments(&sidx, h.end as u64, track, out);
            end_ns = end_ns.max(frag::sidx_ns(&sidx, track, frag::sidx_end(&sidx)));
        }
        at = h.end;
    }
    end_ns
}

/// A fragmented track's frame delta, its `stts`'s, else the one most rows of the first `moof`
/// behind the `moov` take, else its `trex` default.
// Cold path, once per movie.
#[allow(clippy::disallowed_methods)]
fn fragment_rates(head: &[u8], mut at: usize, tracks: &mut [Track], trex: &[Trex]) {
    let mut deltas = vec![Tally::NEW; tracks.len()];
    while let Ok(h) = boxes::read_box_header(head, at) {
        if h.kind == boxes::boxtype::MDAT {
            break;
        }
        if h.kind == boxes::boxtype::MOOF {
            let mut ends = vec![None; tracks.len()];
            let movie = frag::Movie { tracks, trex };
            let _ = frag::summarize(h.body(head), &movie, &mut ends, &mut deltas);
            break;
        }
        at = h.end;
    }
    for ((track, tally), x) in tracks.iter_mut().zip(&deltas).zip(trex) {
        if track.frame_ticks == 0 {
            track.frame_ticks = match tally.most() {
                0 => x.duration,
                most => most,
            };
        }
    }
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

    /// The first byte of what the walk has not let out, the run is in file order.
    fn resume(&self) -> u64 {
        match self.step {
            Step::Mdat { end } => self.run.get(self.next).map_or(end, |s| s.offset()),
            Step::Gather { start, .. } => start,
            Step::Box | Step::Lost => self.at,
        }
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
                Gate::NextSync => match of.clone().position(|s| s.sync() || !video) {
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
                    for (n, s) in of.enumerate().filter(|(_, s)| s.sync() || !video) {
                        first.get_or_insert(n);
                        match stamp(s.pts()) <= bound_ns {
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
                Gate::Past { byte } => match of.clone().position(|s| s.offset() >= byte) {
                    Some(n) => {
                        *gate = Gate::Open;
                        n
                    }
                    None => usize::MAX,
                },
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
    let frame_ticks = stts.map_or(0, |b| frame_ticks(b.body(stbl_body)));
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
    // the entry's is the coded size, the tkhd's the shown one a `pasp` stretches
    let width = if entry.width != 0 { entry.width } else { tk_w };
    let height = if entry.height != 0 { entry.height } else { tk_h };

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
        frame_ticks,
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

/// Distinct deltas a [`Tally`] keeps, a table with more takes the mean.
const DELTAS: usize = 16;

/// The delta most samples take, of up to [`DELTAS`] distinct ones, the mean past them.
#[derive(Clone, Copy)]
pub(crate) struct Tally {
    seen: [(u32, u64); DELTAS],
    used: usize,
    over: bool,
    ticks: u128,
    samples: u64,
}

impl Tally {
    pub(crate) const NEW: Tally =
        Tally { seen: [(0, 0); DELTAS], used: 0, over: false, ticks: 0, samples: 0 };

    pub(crate) fn add(&mut self, delta: u32, count: u64) {
        if count == 0 || delta == 0 {
            return;
        }
        self.ticks += u128::from(count) * u128::from(delta);
        self.samples = self.samples.saturating_add(count);
        match self.seen[..self.used].iter_mut().find(|t| t.0 == delta) {
            Some(t) => t.1 += count,
            None if self.used < DELTAS => {
                self.seen[self.used] = (delta, count);
                self.used += 1;
            }
            None => self.over = true,
        }
    }

    /// 0 for no timed sample.
    pub(crate) fn most(&self) -> u32 {
        if self.samples == 0 {
            return 0;
        }
        if self.over {
            return u32::try_from(self.ticks / u128::from(self.samples)).unwrap_or(0);
        }
        // the first of equal counts, a table's leading run
        let mut best = self.seen[0];
        for t in &self.seen[1..self.used] {
            if t.1 > best.1 {
                best = *t;
            }
        }
        best.0
    }
}

/// The delta most samples of an `stts` body take, walked in place, see [`Tally`].
fn frame_ticks(body: &[u8]) -> u32 {
    let be32 = |o: usize| body.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let Some(n) = be32(4) else { return 0 };
    let mut tally = Tally::NEW;
    for i in 0..n as usize {
        let at = 8 + i * 8;
        let (Some(count), Some(delta)) = (be32(at), be32(at + 4)) else { break };
        tally.add(delta, u64::from(count));
    }
    tally.most()
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

pub(crate) struct Stbl<'a> {
    pub(crate) count: usize,
    pub(crate) stts: &'a [u8],
    pub(crate) sizes: &'a [u8],
    pub(crate) stz2: bool,
    pub(crate) stsc: &'a [u8],
    pub(crate) offsets: &'a [u8],
    pub(crate) co64: bool,
    pub(crate) pcm_frame: u32,
    pub(crate) ctts: Option<&'a [u8]>,
    pub(crate) stss: Option<&'a [u8]>,
    pub(crate) edits: Option<EditList>,
    pub(crate) kind: TrackKind,
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

/// Uncompressed audio frames per sample, the samples of one chunk grouped as FFmpeg's mov does.
pub(crate) const PCM_GROUP: u32 = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    /// An `stts` body of `runs`, count and delta each.
    fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        for (c, d) in runs {
            b.extend_from_slice(&c.to_be_bytes());
            b.extend_from_slice(&d.to_be_bytes());
        }
        b
    }

    /// The delta most samples take, a short last sample left out. Past `DELTAS` distinct ones the
    /// mean, an empty or cut table is 0 or what it holds.
    #[test]
    fn the_frame_delta_is_the_one_most_samples_take() {
        assert_eq!(frame_ticks(&stts(&[(299, 1001), (1, 500)])), 1001);
        assert_eq!(frame_ticks(&stts(&[(10, 3000), (50, 1500), (10, 3000), (5, 1500)])), 1500);
        assert_eq!(frame_ticks(&stts(&[(4, 1000), (4, 2000)])), 1000, "a tie takes the first");
        let vfr: Vec<(u32, u32)> = (0..40).map(|i| (1, 990 + i)).collect();
        assert_eq!(frame_ticks(&stts(&vfr)), 1009, "the mean of 990 to 1029");
        assert_eq!(frame_ticks(&stts(&[])), 0);
        assert_eq!(frame_ticks(&stts(&[(5, 0), (0, 40)])), 0, "no timed sample");
        let mut cut = stts(&[(3, 512), (9, 256)]);
        cut.truncate(cut.len() - 4);
        assert_eq!(frame_ticks(&cut), 512, "the runs the body holds");
        assert_eq!(frame_ticks(&[0, 0]), 0);
    }

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

    /// Every field a packed sample is made of reads back, at the ends of what it holds too.
    #[test]
    fn a_packed_sample_reads_back_what_it_was_made_of() {
        let tracks = [0u16, 1, 7, (MAX_TRACKS - 1) as u16];
        let offsets = [0u64, 1, 4096, u64::from(u32::MAX) + 9, Sample::MAX_OFFSET];
        let sizes = [0u32, 1, 1500, Sample::MAX_SIZE];
        let times = [i64::MIN + (1 << 32), -3, 0, 90_000, i64::MAX - (1 << 32)];
        let offs = [i64::from(i32::MIN), -1001, 0, 2, i64::from(i32::MAX)];
        let mut n = 0u32;
        for (&track, &offset) in tracks.iter().flat_map(|t| offsets.iter().map(move |o| (t, o))) {
            for (&size, &dts) in sizes.iter().flat_map(|s| times.iter().map(move |t| (s, t))) {
                for &cts in &offs {
                    n += 1;
                    let (pts, sync) = (dts + cts, n.is_multiple_of(3));
                    let mut s = Sample::new(track, offset, size, dts, pts, sync).expect("held");
                    let read = |s: &Sample| {
                        (s.track_index(), s.track(), s.offset(), s.size(), s.dts(), s.pts())
                    };
                    assert_eq!(read(&s), (track, usize::from(track), offset, size, dts, pts));
                    assert_eq!((s.sync(), s.hidden()), (sync, false));
                    s.hide();
                    assert_eq!((read(&s), s.sync(), s.hidden()), (read(&s), sync, true));
                    s.present_at(pts - 40);
                    assert_eq!((s.dts(), s.pts(), s.size()), (dts - 40, pts - 40, size));
                    assert!(s.hidden() && s.sync() == sync && s.offset() == offset);
                }
            }
        }
    }
}
