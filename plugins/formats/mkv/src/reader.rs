//! Incremental Matroska structure reader (RFC 9559, RFC 8794). It buffers partial elements
//! across pushes and never panics on untrusted input.

use std::collections::VecDeque;

use crate::ebml::{self, ReadError, id};
use crate::encoding::{self, Coding};
use crate::inflate::InflateError;
use profluens_core::ctx::Disposition;
use profluens_core::fetch::{Need, Step};
use profluens_core::memory::{Demand, Known};

/// A track discovered in the Tracks master.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub track_number: u64,
    pub codec_id: String,
    pub codec_private: Vec<u8>,
    /// `CodecDelay` in ns (RFC 9559 section 5.1.4.1.25). Block timestamps stay as written
    /// (section 11.2), the decoder takes the delay off.
    pub codec_delay_ns: u64,
    /// `SeekPreRoll` in ns (RFC 9559 section 5.1.4.1.26).
    pub seek_pre_roll_ns: u64,
    /// `LanguageBCP47`, else `Language`, whose default is `eng` (RFC 9559 section 5.1.4.1.19).
    pub language: String,
    pub name: String,
    pub flag_default: bool,
    pub flag_forced: bool,
    pub disposition: Disposition,
    pub sampling_frequency: f64,
    pub channels: u32,
    pub bit_depth: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// `DisplayWidth` and `DisplayHeight` in `display_unit`, 0 where absent, their default is
    /// then the cropped pixel size (RFC 9559 section 5.1.4.1.28.12).
    pub display_width: u32,
    pub display_height: u32,
    /// `DisplayUnit`, 0 pixels, 3 an aspect ratio.
    pub display_unit: u8,
    /// Degrees clockwise the picture is turned to show, a quarter turn, from a rectangular
    /// `Projection`'s `ProjectionPoseRoll` (RFC 9559 section 15.2). 0 for none or any other pose.
    pub rotation: u16,
    /// `TrackUID`, what Tags name a track by, 0 where absent.
    pub uid: u64,
    /// Bits a second from the `BPS` statistics tag, 0 for unknown. Set from Tags the head
    /// holds, Tags read apart give it through [`crate::tags::bitrates`].
    pub bitrate: u64,
    /// H.273 section 8.3 code point. Defaults to `2` (unspecified) since `0` is valid (identity).
    pub colour_matrix: u8,
    pub colour_range: u8,
    pub colour_transfer: u8,
    pub colour_primaries: u8,
    pub hdr: profluens_video::color::HdrStatic,
    pub chroma_siting: (u8, u8),
    pub dovi: Option<profluens_video::color::DoviConfig>,
    /// `BlockAddIDValue` of a type 4 mapping, else 4, what WebM uses unmapped for HDR10+.
    pub t35_add_id: u64,
    pub default_duration_ns: Option<u64>,
    pub coding: Coding,
}

/// The stamp of a frame whose time the Block leaves undetermined, `Timestamp::NONE`'s value.
pub const PTS_NONE: u64 = u64::MAX;

/// One decoded frame. A laced block yields several, frame `k` stamped `k` DefaultDurations
/// after the block's, else [`PTS_NONE`] past the first (RFC 9559 section 10.3.5).
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub track_number: u64,
    pub pts_ns: u64,
    /// `BlockDuration` scaled to ns (RFC 9559 section 5.1.3.6), else the track's
    /// `DefaultDuration`.
    pub duration_ns: Option<u64>,
    /// A Block with no ReferenceBlock is a keyframe (RFC 9559 section 12.8).
    pub keyframe: bool,
    pub data: Vec<u8>,
    pub t35: Vec<u8>,
    /// `DiscardPadding` in ns on a Block's last frame, the silence to drop at its end (RFC 9559
    /// section 5.1.3.5.7). Zero for none or for padding at the start.
    pub discard_ns: u64,
}

/// A queued frame. A coded one is undone as it leaves, so a push of zlib frames holds their
/// coded size and not what they inflate to.
struct Queued {
    frame: Frame,
    coded: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Element by element, the head and then the Clusters.
    Walking,
    /// Resuming mid-Segment after a seek or damage, scanning for the next Cluster ID.
    Resyncing,
}

const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;

/// Largest element buffered whole. A declared size is untrusted.
pub(crate) const MAX_HELD_ELEMENT: u64 = 256 << 20;

/// Largest block taken, what the pool sends on mid-stream. Inside an unknown-size Cluster one
/// flipped size bit would otherwise buffer up to [`MAX_HELD_ELEMENT`] of the media after it.
pub(crate) const MAX_BLOCK: u64 = profluens_core::memory::SIZED_TOP as u64;

/// A block's 1-octet track VINT reaches 126, so this is well above any real file.
const MAX_TRACKS: usize = 256;

/// The chain scratch keeps its peak capacity, so past this it is given back after use.
const SCRATCH_KEEP: usize = 1 << 20;

const CLASS_MIN_SHIFT: u32 = 6;
const CLASS_MAX_SHIFT: u32 = 26;
/// Quarter-octave size classes, so a class is at most 25 percent larger than its frame.
const CLASSES: usize = ((CLASS_MAX_SHIFT - CLASS_MIN_SHIFT) * 4 + 1) as usize;

const MIN_FREE_BYTES: usize = 256 << 10;
/// Hard ceiling, so an adversarial huge Cluster cannot turn the pool into a memory bomb.
const MAX_FREE_BYTES: usize = 32 << 20;

/// Recycled frame-payload buffers bucketed by size class and bounded by retained bytes.
/// One push decodes thousands of frames before any drain, so a count cap would not hold.
struct FreePool {
    class: [Vec<Vec<u8>>; CLASSES],
    bytes: usize,
}

/// Round `n` up to its quarter-octave class. Past the pooled range it is returned unrounded.
fn class_size(n: usize) -> usize {
    let min = 1usize << CLASS_MIN_SHIFT;
    if n <= min {
        return min;
    }
    if n > 1usize << CLASS_MAX_SHIFT {
        return n;
    }
    let step = 1usize << (n.ilog2() - 2);
    n.div_ceil(step) * step
}

/// The class a capacity can serve, rounded down so it never promises room it lacks.
fn class_index(cap: usize) -> usize {
    let k = cap.ilog2();
    let quarter = (cap >> (k - 2)) - 4;
    ((k - CLASS_MIN_SHIFT) * 4) as usize + quarter
}

impl FreePool {
    // Cold path, empty stacks, `Vec::new` does not allocate.
    #[allow(clippy::disallowed_methods)]
    const fn new() -> Self {
        Self { class: [const { Vec::new() }; CLASSES], bytes: 0 }
    }

    // Cold miss path, steady state is a `pop`.
    #[allow(clippy::disallowed_methods)]
    fn take(&mut self, needed: usize) -> Vec<u8> {
        let size = class_size(needed);
        if size > (1usize << CLASS_MAX_SHIFT) {
            return Vec::with_capacity(needed); // unpooled freak frame; dropped on recycle
        }
        let first = class_index(size);
        for c in first..(first + 5).min(CLASSES) {
            if let Some(b) = self.class[c].pop() {
                self.bytes -= b.capacity();
                return b;
            }
        }
        Vec::with_capacity(size)
    }

    fn put(&mut self, buf: Vec<u8>, budget: usize) {
        let cap = buf.capacity();
        if !((1usize << CLASS_MIN_SHIFT)..=(1usize << CLASS_MAX_SHIFT)).contains(&cap)
            || self.bytes + cap > budget
        {
            return;
        }
        self.bytes += cap;
        self.class[class_index(cap)].push(buf);
    }
}

pub struct MatroskaReader {
    buf: Vec<u8>,
    pos: usize,
    phase: Phase,
    timestamp_scale: u64,
    tracks: Vec<Track>,
    tracks_ready: bool,
    duration_ns: Option<u64>,
    cluster_base_tick: i64,
    pending: VecDeque<Queued>,
    /// Payload octets in `pending`, kept as frames come and go.
    queued: usize,
    free: FreePool,
    lace_sizes: Vec<usize>,
    coding_scratch: Vec<u8>,
    undecoded: Option<(u64, InflateError)>,
    skip: u64,
    base: Option<u64>,
    marked: Option<u64>,
    cluster_at: Option<u64>,
    marks: Vec<(u64, u64)>,
    /// Octets ever dropped from the front of `buf`, so `gone + pos` is a stream offset.
    gone: u64,
    /// End of the sized Cluster being read, in `gone + pos` terms.
    cluster_end: Option<u64>,
    /// End of a sized Segment, in `gone + pos` terms, past which no element of it runs.
    segment_end: Option<u64>,
    damage: Option<ReadError>,
    /// Stream offset of the last damage.
    damage_at: u64,
}

impl Default for MatroskaReader {
    fn default() -> Self {
        Self::new()
    }
}

impl MatroskaReader {
    // Cold path, construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            phase: Phase::Walking,
            timestamp_scale: DEFAULT_TIMESTAMP_SCALE,
            tracks: Vec::new(),
            tracks_ready: false,
            duration_ns: None,
            cluster_base_tick: 0,
            pending: VecDeque::new(),
            queued: 0,
            free: FreePool::new(),
            lace_sizes: Vec::new(),
            coding_scratch: Vec::new(),
            undecoded: None,
            skip: 0,
            base: Some(0),
            marked: None,
            cluster_at: None,
            marks: Vec::new(),
            gone: 0,
            cluster_end: None,
            segment_end: None,
            damage: None,
            damage_at: 0,
        }
    }

    /// Tracks, scale and duration from a probe of the head, so a resume at any byte resyncs.
    // Cold path, once a run.
    #[allow(clippy::disallowed_methods)]
    pub fn seed(&mut self, tracks: &[Track], timestamp_scale: u64, duration_ns: Option<u64>) {
        (self.tracks, self.tracks_ready) = (tracks.to_vec(), !tracks.is_empty());
        (self.timestamp_scale, self.duration_ns) = (timestamp_scale.max(1), duration_ns);
    }

    pub fn mark(&mut self, track: Option<u64>) {
        self.marked = track;
    }

    pub fn marks(&mut self) -> std::vec::Drain<'_, (u64, u64)> {
        self.marks.drain(..)
    }

    pub fn restart_at(&mut self, byte: Option<u64>) {
        self.base = byte;
    }

    fn advance(&mut self, n: usize) {
        self.base = self.base.map(|b| b + n as u64);
        self.gone += n as u64;
    }

    fn mark_from(&mut self, from: usize) {
        let (Some(track), Some(at)) = (self.marked, self.cluster_at) else {
            return;
        };
        let mut queued = self.pending.range(from..).map(|q| &q.frame);
        if let Some(f) = queued.find(|f| f.track_number == track && f.keyframe) {
            self.marks.push((f.pts_ns, at));
            self.cluster_at = None;
        }
    }

    /// A copy of `data` in a recycled payload buffer, so a held frame costs no heap.
    pub(crate) fn copy(&mut self, data: &[u8]) -> Vec<u8> {
        pooled(&mut self.free, Some(data))
    }

    pub fn recycle(&mut self, buf: Vec<u8>) {
        self.free.put(buf, self.free_budget());
    }

    /// The reader's input buffer capacity, clamped to [`MIN_FREE_BYTES`]..=[`MAX_FREE_BYTES`].
    fn free_budget(&self) -> usize {
        self.buf.capacity().clamp(MIN_FREE_BYTES, MAX_FREE_BYTES)
    }

    pub fn timestamp_scale(&self) -> u64 {
        self.timestamp_scale
    }

    pub fn duration_ns(&self) -> Option<u64> {
        self.duration_ns
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn tracks_ready(&self) -> bool {
        self.tracks_ready
    }

    pub fn push(&mut self, mut data: &[u8]) -> Result<(), ReadError> {
        if self.skip > 0 {
            let gone = (data.len() as u64).min(self.skip) as usize;
            self.skip -= gone as u64;
            self.advance(gone);
            data = &data[gone..];
            if data.is_empty() {
                return Ok(());
            }
        }
        self.buf.extend_from_slice(data);
        self.parse()
    }

    pub fn take_undecoded(&mut self) -> Option<(u64, InflateError)> {
        self.undecoded.take()
    }

    /// The last damage skipped since asked, the reader resynced past it.
    pub fn take_damage(&mut self) -> Option<ReadError> {
        self.damage.take()
    }

    /// Where the last damage was, a file offset when the element placed the reader, else an
    /// offset into what was pushed.
    pub fn damage_at(&self) -> u64 {
        self.damage_at
    }

    fn offset(&self, pos: usize) -> u64 {
        self.base.map_or(self.gone + pos as u64, |b| b + pos as u64)
    }

    fn trim_scratch(&mut self) {
        if self.coding_scratch.capacity() > SCRATCH_KEEP {
            self.coding_scratch.clear();
            self.coding_scratch.shrink_to_fit();
        }
    }

    pub(crate) fn decode_frame(
        &mut self,
        track: u64,
        steps: &[encoding::Step],
        src: &[u8],
    ) -> Option<Vec<u8>> {
        let mut buf = self.free.take(encoding::decoded_hint(steps, src.len()));
        let scratch = &mut self.coding_scratch;
        let done = encoding::decode(steps, src, &mut buf, scratch, encoding::MAX_DECODED);
        self.trim_scratch();
        match done {
            Ok(()) => Some(buf),
            Err(e) => {
                self.undecoded = Some((track, e));
                self.recycle(buf);
                None
            }
        }
    }

    pub fn held(&self) -> usize {
        self.buf.len().saturating_sub(self.pos) + self.queued
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        let budget = self.free_budget();
        while let Some(Queued { mut frame, coded }) = self.pending.pop_front() {
            self.queued -= frame.data.len();
            if !coded {
                return Some(frame);
            }
            let number = frame.track_number;
            let track = self.tracks.iter().find(|t| t.track_number == number);
            let Some(steps) = track.map(|t| &t.coding.frames) else {
                self.free.put(frame.data, budget);
                self.free.put(frame.t35, budget);
                continue;
            };
            let mut buf = self.free.take(encoding::decoded_hint(steps, frame.data.len()));
            let scratch = &mut self.coding_scratch;
            let done =
                encoding::decode(steps, &frame.data, &mut buf, scratch, encoding::MAX_DECODED);
            self.trim_scratch();
            let coded = std::mem::replace(&mut frame.data, buf);
            self.free.put(coded, budget);
            match done {
                Ok(()) => return Some(frame),
                Err(e) => {
                    self.undecoded = Some((number, e));
                    self.free.put(frame.data, budget);
                    self.free.put(frame.t35, budget);
                }
            }
        }
        None
    }

    pub fn unread(&mut self, frame: Frame) {
        self.queued += frame.data.len();
        self.pending.push_front(Queued { frame, coded: false });
    }

    fn compact(&mut self) {
        if self.pos > 0 && (self.pos * 2 >= self.buf.len() || self.pos == self.buf.len()) {
            self.buf.drain(..self.pos);
            self.advance(self.pos);
            self.pos = 0;
        }
    }

    fn parse(&mut self) -> Result<(), ReadError> {
        loop {
            match self.step() {
                Ok(true) => self.compact(),
                Ok(false) | Err(ReadError::Incomplete) => {
                    self.compact();
                    return Ok(());
                }
                // Once the tracks are known damage is skipped and the next Cluster sought.
                Err(e) if self.tracks_ready => self.lost(e),
                Err(e) => return Err(e),
            }
        }
    }

    fn lost(&mut self, e: ReadError) {
        (self.damage, self.damage_at) = (Some(e), self.offset(self.pos));
        self.pos = (self.pos + 1).min(self.buf.len());
        (self.phase, self.cluster_end) = (Phase::Resyncing, None);
    }

    fn step(&mut self) -> Result<bool, ReadError> {
        if self.phase == Phase::Resyncing {
            return self.step_resync();
        }
        if self.pos >= self.buf.len() {
            return Ok(false);
        }
        let at = self.gone + self.pos as u64;
        if self.cluster_end.is_some_and(|end| at >= end) {
            self.cluster_end = None;
        }
        let header = ebml::read_element_header(&self.buf, self.pos)?;
        // Stack copy of the ID, the hottest allocation site otherwise. IDs are 1 to 4 octets (RFC
        // 8794 section 5).
        let mut id_buf = [0u8; 4];
        let id_len = header.id.len();
        if id_len > id_buf.len() {
            return Err(ReadError::Malformed("EBML ID longer than 4 octets"));
        }
        id_buf[..id_len].copy_from_slice(header.id);
        let id = &id_buf[..id_len];
        let hdr_end = header.data_start;

        if id == id::SEGMENT {
            let data = self.gone + hdr_end as u64;
            self.segment_end = header.size.map(|size| data.saturating_add(size));
            self.pos = hdr_end;
            return Ok(true);
        }
        if id == id::CLUSTER {
            self.cluster_at = self.base.map(|b| b + self.pos as u64);
            let data = self.gone + hdr_end as u64;
            self.cluster_end = header.size.map(|size| data.saturating_add(size));
            self.pos = hdr_end;
            self.cluster_base_tick = 0;
            return Ok(true);
        }

        let data_end = match header.data_end()? {
            Some(e) => e,
            None => {
                return Err(ReadError::Malformed("unexpected unknown-size element"));
            }
        };
        let elem_end = self.gone.saturating_add(data_end as u64);
        if self.cluster_end.is_some_and(|end| elem_end > end) {
            return Err(ReadError::Malformed("element runs past its Cluster"));
        }
        if self.segment_end.is_some_and(|end| elem_end > end) {
            return Err(ReadError::Malformed("element runs past its Segment"));
        }
        let block = id == id::SIMPLE_BLOCK || id == id::BLOCK_GROUP;
        if block && header.size.is_some_and(|size| size > MAX_BLOCK) {
            return Err(ReadError::Malformed("block larger than any the reader sends on"));
        }
        if data_end > self.buf.len() {
            let parsed = id == id::INFO || id == id::TRACKS || id == id::TIMESTAMP || block;
            if !parsed {
                // Opaque, discarded as it arrives so its declared size costs no memory.
                self.skip = (data_end - self.buf.len()) as u64;
                self.pos = self.buf.len();
                return Ok(true);
            }
            if header.size.is_some_and(|size| size > MAX_HELD_ELEMENT) {
                return Err(ReadError::Malformed("element larger than the reader will buffer"));
            }
            return Err(ReadError::Incomplete); // not all here yet, buffer more
        }
        let (ds, de) = (header.data_start, data_end);
        // A bad body costs that element alone, its bounds are sound.
        if let Err(e) = self.body(id, ds, de) {
            if !self.tracks_ready {
                return Err(e);
            }
            (self.damage, self.damage_at) = (Some(e), at);
        }
        self.pos = de;
        Ok(true)
    }

    fn body(&mut self, id: &[u8], ds: usize, de: usize) -> Result<(), ReadError> {
        if id == id::INFO {
            let (scale, duration_ticks) = parse_info(&self.buf[ds..de])?;
            if let Some(scale) = scale {
                self.timestamp_scale = scale;
            }
            self.duration_ns =
                duration_ticks.map(|t| (t * self.timestamp_scale as f64).round() as u64);
        } else if id == id::TRACKS {
            let tracks = parse_tracks(&self.buf[ds..de])?;
            if tracks.is_empty() {
                // Tracks holds at least one TrackEntry (RFC 9559 section 5.1.4), none would
                // preroll with no pads.
                return Err(ReadError::Malformed("Tracks with no TrackEntry"));
            }
            // A later Tracks, a chained Segment or a resent header, replaces the last.
            (self.tracks, self.tracks_ready) = (tracks, true);
        } else if id == id::TAGS {
            crate::tags::bitrates_in(&self.buf[ds..de], &mut self.tracks);
        } else if id == id::TIMESTAMP {
            self.cluster_base_tick = read_uint(&self.buf[ds..de]) as i64;
        } else if id == id::SIMPLE_BLOCK || id == id::BLOCK_GROUP {
            let (base, scale, before) =
                (self.cluster_base_tick, self.timestamp_scale, self.pending.len());
            let mut q = Queue {
                out: &mut self.pending,
                queued: &mut self.queued,
                free: &mut self.free,
                lace: &mut self.lace_sizes,
                tracks: &self.tracks,
            };
            match id == id::SIMPLE_BLOCK {
                true => decode_simple_block(&self.buf[ds..de], base, scale, &mut q)?,
                false => decode_block_group(&self.buf[ds..de], base, scale, &mut q)?,
            }
            self.mark_from(before);
        }
        Ok(())
    }

    /// Scan for the next Cluster ID after a seek. A false positive with no valid size resumes the
    /// scan.
    fn step_resync(&mut self) -> Result<bool, ReadError> {
        const CID: &[u8] = id::CLUSTER; // [0x1F, 0x43, 0xB6, 0x75]
        let mut search_from = self.pos;
        loop {
            let hay = &self.buf[search_from..];
            let Some(rel) = find_subslice(hay, CID) else {
                break;
            };
            let cand = search_from + rel;
            let led =
                ebml::read_element_header(&self.buf, cand).map(|_| cluster_led(&self.buf, cand));
            match led {
                Ok(Some(false)) => search_from = cand + 1,
                Ok(Some(true)) => {
                    self.pos = cand;
                    self.phase = Phase::Walking;
                    self.cluster_base_tick = 0;
                    self.compact();
                    return Ok(true);
                }
                Ok(None) | Err(ReadError::Incomplete) => {
                    self.buf.drain(..cand);
                    self.advance(cand);
                    self.pos = 0;
                    return Ok(false);
                }
                Err(_) => {
                    search_from = cand + 1;
                }
            }
        }
        // Keep the last 3 bytes, a Cluster ID may be split across pushes.
        let keep = CID.len() - 1;
        let end = self.buf.len();
        let drop_to = end.saturating_sub(keep);
        if drop_to > 0 {
            self.buf.drain(..drop_to);
            self.advance(drop_to);
        }
        self.pos = 0;
        Ok(false)
    }

    /// Mid-Segment resume after a seek. Keeps tracks and scale, the source does not resend the
    /// header.
    pub fn resync_streaming(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.skip = 0;
        // Pre-seek payload buffers go to the recycle pool so a scrubbing consumer stays warm.
        let budget = self.free_budget();
        self.queued = 0;
        while let Some(q) = self.pending.pop_front() {
            self.free.put(q.frame.data, budget);
        }
        self.cluster_base_tick = 0;
        (self.base, self.cluster_at, self.cluster_end, self.segment_end) = (None, None, None, None);
        self.phase = if self.tracks_ready { Phase::Resyncing } else { Phase::Walking };
    }
}

// Free functions so a borrowed `buf` slice and `&mut pending` coexist without a copy.

pub(crate) fn parse_info(data: &[u8]) -> Result<(Option<u64>, Option<f64>), ReadError> {
    let mut at = 0;
    let mut scale = None;
    let mut duration_ticks = None;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in Info"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("Info child runs past its master"));
        }
        if h.id == id::TIMESTAMP_SCALE {
            let v = read_uint(&data[h.data_start..end]);
            if v != 0 {
                scale = Some(v);
            }
        } else if h.id == id::DURATION {
            let v = read_float(&data[h.data_start..end])?;
            if v.is_finite() && v > 0.0 {
                duration_ticks = Some(v);
            }
        }
        at = end;
    }
    Ok((scale, duration_ticks))
}

// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
fn parse_tracks(data: &[u8]) -> Result<Vec<Track>, ReadError> {
    let mut tracks: Vec<Track> = Vec::new();
    // Undone CodecPrivate octets so far, a bound on what a header of zlib bombs costs.
    let (mut undone, mut entries) = (0usize, 0usize);
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in Tracks"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("Tracks child runs past its master"));
        }
        if h.id == id::TRACK_ENTRY {
            // Refused before the entry is parsed, so the count bounds the parse as well.
            if entries == MAX_TRACKS {
                return Err(ReadError::Malformed("more tracks than the reader accepts"));
            }
            entries += 1;
            let track = parse_track_entry(&data[h.data_start..end], &mut undone)?;
            // The first entry of a number is the track, a repeat would split one stream in two
            // and leave the second pad silent.
            if !tracks.iter().any(|t| t.track_number == track.track_number) {
                tracks.push(track);
            }
        }
        at = end;
    }
    Ok(tracks)
}

// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
fn parse_track_entry(data: &[u8], undone: &mut usize) -> Result<Track, ReadError> {
    let mut track = Track {
        track_number: 0,
        codec_id: String::new(),
        codec_private: Vec::new(),
        codec_delay_ns: 0,
        seek_pre_roll_ns: 0,
        language: String::from("eng"),
        name: String::new(),
        flag_default: true,
        flag_forced: false,
        disposition: Disposition::MAIN,
        sampling_frequency: 0.0,
        channels: 0,
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
        t35_add_id: T35_ADD_ID,
        default_duration_ns: None,
        coding: Coding::default(),
    };
    let mut bcp47 = false;
    let mut private_steps = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in TrackEntry"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("TrackEntry child runs past its master"));
        }
        let body = &data[h.data_start..end];
        if h.id == id::TRACK_NUMBER {
            track.track_number = read_uint(body);
        } else if h.id == id::TRACK_UID {
            track.uid = read_uint(body);
        } else if h.id == id::CODEC_ID {
            track.codec_id = String::from_utf8_lossy(ebml::read_string(body)).into_owned();
        } else if h.id == id::CODEC_PRIVATE {
            track.codec_private = body.to_vec();
        } else if h.id == id::CODEC_DELAY {
            track.codec_delay_ns = read_uint(body);
        } else if h.id == id::SEEK_PRE_ROLL {
            track.seek_pre_roll_ns = read_uint(body);
        } else if h.id == id::LANGUAGE_BCP47 {
            (track.language, bcp47) =
                (String::from_utf8_lossy(ebml::read_string(body)).into_owned(), true);
        } else if h.id == id::LANGUAGE && !bcp47 {
            track.language = String::from_utf8_lossy(ebml::read_string(body)).into_owned();
        } else if h.id == id::NAME {
            track.name = String::from_utf8_lossy(ebml::read_string(body)).into_owned();
        } else if h.id == id::FLAG_DEFAULT {
            track.flag_default = read_uint(body) != 0;
        } else if h.id == id::FLAG_FORCED {
            track.flag_forced = read_uint(body) != 0;
        } else if h.id == id::FLAG_ENABLED {
            track.disposition.enabled = read_uint(body) != 0;
        } else if h.id == id::FLAG_COMMENTARY {
            track.disposition.commentary = read_uint(body) != 0;
        } else if h.id == id::FLAG_HEARING_IMPAIRED {
            track.disposition.hearing_impaired = read_uint(body) != 0;
        } else if h.id == id::FLAG_VISUAL_IMPAIRED {
            track.disposition.visual_impaired = read_uint(body) != 0;
        } else if h.id == id::FLAG_TEXT_DESCRIPTIONS {
            track.disposition.descriptions = read_uint(body) != 0;
        } else if h.id == id::FLAG_ORIGINAL {
            track.disposition.original = read_uint(body) != 0;
        } else if h.id == id::DEFAULT_DURATION {
            track.default_duration_ns = Some(read_uint(body)).filter(|d| *d > 0);
        } else if h.id == id::CONTENT_ENCODINGS {
            (track.coding, private_steps) = encoding::parse(body)?;
        } else if h.id == id::AUDIO {
            parse_audio(body, &mut track)?;
        } else if h.id == id::VIDEO {
            parse_video(body, &mut track)?;
        } else if h.id == id::BLOCK_ADDITION_MAPPING {
            parse_addition_mapping(body, &mut track)?;
        }
        at = end;
    }
    if track.track_number == 0 {
        // TrackNumber MUST NOT be 0 (RFC 9559 section 5.1.4.1.1).
        return Err(ReadError::Malformed("TrackEntry with a zero/absent TrackNumber"));
    }
    if !private_steps.is_empty() && track.coding.refused.is_none() {
        let (mut out, mut scratch) = (Vec::new(), Vec::new());
        // Past either bound the record is dropped like one that does not inflate, the track
        // stays and the decoder finds its parameters in band or not at all.
        let limit = encoding::MAX_PRIVATE.min(encoding::MAX_PRIVATE_TOTAL - *undone);
        let decoded =
            encoding::decode(&private_steps, &track.codec_private, &mut out, &mut scratch, limit);
        track.codec_private = if decoded.is_ok() { out } else { Vec::new() };
        *undone += track.codec_private.len();
    }
    Ok(track)
}

pub(crate) const T35_ADD_ID: u64 = 4;
pub(crate) const ADD_ID_TYPE_DVCC: u64 = u32::from_be_bytes(*b"dvcC") as u64;
pub(crate) const ADD_ID_TYPE_DVVC: u64 = u32::from_be_bytes(*b"dvvC") as u64;

fn parse_addition_mapping(data: &[u8], track: &mut Track) -> Result<(), ReadError> {
    let (mut value, mut kind, mut extra) = (0u64, 0u64, &[][..]);
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h
            .data_end()?
            .ok_or(ReadError::Malformed("unknown-size child in BlockAdditionMapping"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("BlockAdditionMapping child runs past its master"));
        }
        let body = &data[h.data_start..end];
        if h.id == id::BLOCK_ADD_ID_VALUE {
            value = read_uint(body);
        } else if h.id == id::BLOCK_ADD_ID_TYPE {
            kind = read_uint(body);
        } else if h.id == id::BLOCK_ADD_ID_EXTRA_DATA {
            extra = body;
        }
        at = end;
    }
    match kind {
        T35_ADD_ID if value >= 2 => track.t35_add_id = value,
        ADD_ID_TYPE_DVCC | ADD_ID_TYPE_DVVC => {
            track.dovi = profluens_video::color::DoviConfig::from_record(extra).ok()
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn block_addition(data: &[u8], want: u64) -> Option<std::ops::Range<usize>> {
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if end > data.len() {
            return None;
        }
        if h.id == id::BLOCK_MORE {
            let (mut add_id, mut bytes) = (1u64, None);
            let mut inner = h.data_start;
            while inner < end {
                let c = ebml::read_element_header(data, inner).ok()?;
                let c_end = c.data_end().ok()??;
                if c_end > end {
                    return None;
                }
                if c.id == id::BLOCK_ADD_ID {
                    add_id = read_uint(&data[c.data_start..c_end]);
                } else if c.id == id::BLOCK_ADDITIONAL {
                    bytes = Some(c.data_start..c_end);
                }
                inner = c_end;
            }
            if add_id == want
                && let Some(r) = bytes
            {
                return Some(r);
            }
        }
        at = end;
    }
    None
}

fn parse_audio(data: &[u8], track: &mut Track) -> Result<(), ReadError> {
    // The defaults where the Audio master leaves them out (RFC 9559 sections 5.1.4.1.29.1 and
    // 5.1.4.1.29.3).
    (track.sampling_frequency, track.channels) = (8_000.0, 1);
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in Audio"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("Audio child runs past its master"));
        }
        let body = &data[h.data_start..end];
        if h.id == id::SAMPLING_FREQUENCY {
            // NaN, zero or negative would ride the format as an Int rate, the default stands.
            let v = read_float(body)?;
            if v.is_finite() && v > 0.0 {
                track.sampling_frequency = v;
            }
        } else if h.id == id::CHANNELS {
            track.channels = read_uint(body) as u32;
        } else if h.id == id::BIT_DEPTH {
            track.bit_depth = read_uint(body) as u32;
        }
        at = end;
    }
    Ok(())
}

fn parse_video(data: &[u8], track: &mut Track) -> Result<(), ReadError> {
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in Video"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("Video child runs past its master"));
        }
        let body = &data[h.data_start..end];
        if h.id == id::PIXEL_WIDTH {
            track.pixel_width = read_uint(body) as u32;
        } else if h.id == id::PIXEL_HEIGHT {
            track.pixel_height = read_uint(body) as u32;
        } else if h.id == id::DISPLAY_WIDTH {
            track.display_width = read_uint(body).min(u64::from(u32::MAX)) as u32;
        } else if h.id == id::DISPLAY_HEIGHT {
            track.display_height = read_uint(body).min(u64::from(u32::MAX)) as u32;
        } else if h.id == id::DISPLAY_UNIT {
            track.display_unit = read_uint(body).min(u64::from(u8::MAX)) as u8;
        } else if h.id == id::PROJECTION {
            // a pose that does not parse is no turn, the track plays as stored
            track.rotation = projection_turn(body).unwrap_or(0);
        } else if h.id == id::COLOUR {
            parse_colour(body, track)?;
        }
        at = end;
    }
    Ok(())
}

/// The quarter turn clockwise a rectangular Projection's pose shows the picture at, its roll
/// counter-clockwise (RFC 9559 section 5.1.4.1.28.46). 0 for a yaw or pitch, a flip or a sphere.
fn projection_turn(data: &[u8]) -> Result<u16, ReadError> {
    let (mut kind, mut pose) = (0, [0.0f64; 3]);
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in Projection"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("Projection child runs past its master"));
        }
        let body = &data[h.data_start..end];
        let axes = [id::PROJECTION_POSE_YAW, id::PROJECTION_POSE_PITCH, id::PROJECTION_POSE_ROLL];
        if h.id == id::PROJECTION_TYPE {
            kind = read_uint(body);
        } else if let Some(axis) = axes.iter().position(|a| h.id == *a) {
            pose[axis] = read_float(body)?;
        }
        at = end;
    }
    let [yaw, pitch, roll] = pose;
    if kind != 0 || yaw != 0.0 || pitch != 0.0 || !roll.is_finite() {
        return Ok(0);
    }
    let turn = (-roll).rem_euclid(360.0);
    let quarter = (turn / 90.0).round();
    Ok(match (turn - quarter * 90.0).abs() < 0.5 {
        true => (quarter as u16 % 4) * 90,
        false => 0,
    })
}

fn parse_colour(data: &[u8], track: &mut Track) -> Result<(), ReadError> {
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in Colour"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("Colour child runs past its master"));
        }
        let body = &data[h.data_start..end];
        if h.id == id::MATRIX_COEFFICIENTS {
            track.colour_matrix = read_uint(body) as u8;
        } else if h.id == id::COLOUR_RANGE {
            track.colour_range = read_uint(body) as u8;
        } else if h.id == id::TRANSFER_CHARACTERISTICS {
            track.colour_transfer = read_uint(body) as u8;
        } else if h.id == id::PRIMARIES {
            track.colour_primaries = read_uint(body) as u8;
        } else if h.id == id::MAX_CLL {
            track.hdr.light.max_cll = read_uint(body).min(u64::from(u16::MAX)) as u16;
        } else if h.id == id::MAX_FALL {
            track.hdr.light.max_fall = read_uint(body).min(u64::from(u16::MAX)) as u16;
        } else if h.id == id::MASTERING_METADATA {
            parse_mastering(body, &mut track.hdr.mastering)?;
        } else if h.id == id::CHROMA_SITING_HORZ {
            track.chroma_siting.0 = read_uint(body).min(3) as u8;
        } else if h.id == id::CHROMA_SITING_VERT {
            track.chroma_siting.1 = read_uint(body).min(3) as u8;
        }
        at = end;
    }
    Ok(())
}

/// Stored in H.265 mastering display SEI units (D.3.28) so container and bitstream compare equal.
fn parse_mastering(
    data: &[u8],
    m: &mut profluens_video::color::Mastering,
) -> Result<(), ReadError> {
    let xy = |v: f64| (v * 50_000.0).round().clamp(0.0, 50_000.0) as u16;
    let nits = |v: f64| (v * 10_000.0).round().clamp(0.0, u32::MAX as f64) as u32;
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end =
            h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in MasteringMetadata"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("MasteringMetadata child runs past its master"));
        }
        let float =
            || read_float(&data[h.data_start..end]).map(|v| if v.is_finite() { v } else { 0.0 });
        let slot = match h.id {
            x if x == id::PRIMARY_R_X => Some((0, 0)),
            x if x == id::PRIMARY_R_Y => Some((0, 1)),
            x if x == id::PRIMARY_G_X => Some((1, 0)),
            x if x == id::PRIMARY_G_Y => Some((1, 1)),
            x if x == id::PRIMARY_B_X => Some((2, 0)),
            x if x == id::PRIMARY_B_Y => Some((2, 1)),
            x if x == id::WHITE_POINT_X => Some((3, 0)),
            x if x == id::WHITE_POINT_Y => Some((3, 1)),
            _ => None,
        };
        match slot {
            Some((3, c)) => m.white[c] = xy(float()?),
            Some((p, c)) => m.primaries[p][c] = xy(float()?),
            None if h.id == id::LUMINANCE_MAX => m.max_luminance = nits(float()?),
            None if h.id == id::LUMINANCE_MIN => m.min_luminance = nits(float()?),
            None => {}
        }
        at = end;
    }
    Ok(())
}

fn decode_block_group(
    data: &[u8],
    cluster_base_tick: i64,
    scale: u64,
    q: &mut Queue<'_>,
) -> Result<(), ReadError> {
    let parts = block_group_parts(data, scale)?;
    if let Some((s, e)) = parts.block {
        let additions = parts.additions.map_or(&[][..], |(a, b)| &data[a..b]);
        let timing = (cluster_base_tick, scale, parts.duration_ns, parts.discard_ns);
        decode_block(&data[s..e], parts.keyframe, timing, additions, q)?;
    }
    Ok(())
}

struct Queue<'a> {
    out: &'a mut VecDeque<Queued>,
    queued: &'a mut usize,
    free: &'a mut FreePool,
    lace: &'a mut Vec<usize>,
    tracks: &'a [Track],
}

pub(crate) struct GroupParts {
    pub(crate) block: Option<(usize, usize)>,
    pub(crate) keyframe: bool,
    pub(crate) duration_ns: Option<u64>,
    pub(crate) additions: Option<(usize, usize)>,
    pub(crate) discard_ns: u64,
}

pub(crate) fn block_group_parts(data: &[u8], scale: u64) -> Result<GroupParts, ReadError> {
    let mut at = 0;
    let mut has_reference = false;
    let mut block: Option<(usize, usize)> = None;
    let mut duration_ticks: Option<u64> = None;
    let mut additions = None;
    let mut discard_ns = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at)?;
        let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size child in BlockGroup"))?;
        if end > data.len() {
            return Err(ReadError::Malformed("BlockGroup child runs past its master"));
        }
        if h.id == id::BLOCK {
            block = Some((h.data_start, end));
        } else if h.id == id::REFERENCE_BLOCK {
            has_reference = true;
        } else if h.id == id::BLOCK_DURATION {
            duration_ticks = Some(read_uint(&data[h.data_start..end]));
        } else if h.id == id::BLOCK_ADDITIONS {
            additions = Some((h.data_start, end));
        } else if h.id == id::DISCARD_PADDING {
            // Signed, so a negative value is padding at the start, not cut here.
            discard_ns = read_int(&data[h.data_start..end]).max(0) as u64;
        }
        at = end;
    }
    let duration_ns = duration_ticks.map(|t| t.saturating_mul(scale));
    Ok(GroupParts { block, keyframe: !has_reference, duration_ns, additions, discard_ns })
}

fn decode_simple_block(
    data: &[u8],
    cluster_base_tick: i64,
    scale: u64,
    q: &mut Queue<'_>,
) -> Result<(), ReadError> {
    let (_track, _rel, flags, _start) = parse_block_header(data)?;
    let keyframe = flags & 0x80 != 0;
    decode_block(data, keyframe, (cluster_base_tick, scale, None, 0), &[], q)
}

fn decode_block(
    data: &[u8],
    keyframe: bool,
    (cluster_base_tick, scale, duration_ns, discard_ns): (i64, u64, Option<u64>, u64),
    additions: &[u8],
    q: &mut Queue<'_>,
) -> Result<(), ReadError> {
    let Queue { out, queued, free, lace, tracks } = q;
    block_frames(data, cluster_base_tick, scale, lace, &mut |track, pts_ns, k, n, r| {
        let t = tracks.iter().find(|t| t.track_number == track);
        let (pts_ns, duration_ns) =
            lace_stamp(pts_ns, k, n, duration_ns, t.and_then(|t| t.default_duration_ns));
        let t35 = match (k, t) {
            (0, Some(t)) if !additions.is_empty() => {
                block_addition(additions, t.t35_add_id).map(|r| &additions[r])
            }
            _ => None,
        };
        let coded = t.is_some_and(|t| t.coding.transforms());
        let discard_ns = if k + 1 == n { discard_ns } else { 0 };
        let stamp = Stamp { pts_ns, duration_ns, keyframe, discard_ns };
        **queued += r.len();
        push_frame(out, free, track, stamp, &data[r], t35, coded);
    })
}

/// A copy of `data` in a recycled buffer.
fn pooled(free: &mut FreePool, data: Option<&[u8]>) -> Vec<u8> {
    match data {
        Some(b) if !b.is_empty() => {
            let mut v = free.take(b.len());
            v.clear();
            v.extend_from_slice(b);
            v
        }
        // `Vec::new` does not allocate.
        #[allow(clippy::disallowed_methods)]
        _ => Vec::new(),
    }
}

pub(crate) fn lace_stamp(
    pts_ns: u64,
    k: usize,
    n: usize,
    block_ns: Option<u64>,
    default_ns: Option<u64>,
) -> (u64, Option<u64>) {
    if n == 1 {
        return (pts_ns, block_ns.or(default_ns));
    }
    match default_ns {
        Some(d) => (pts_ns.saturating_add(d.saturating_mul(k as u64)).min(PTS_NONE - 1), Some(d)),
        // Past the first a laced frame's time is undetermined, and BlockDuration spans the
        // whole Block (RFC 9559 sections 10.3.5 and 5.1.3.5.3).
        None => (if k == 0 { pts_ns } else { PTS_NONE }, block_ns.map(|b| b / n as u64)),
    }
}

pub(crate) fn block_frames(
    data: &[u8],
    cluster_base_tick: i64,
    scale: u64,
    lace_buf: &mut Vec<usize>,
    frame: &mut dyn FnMut(u64, u64, usize, usize, std::ops::Range<usize>),
) -> Result<(), ReadError> {
    let (track, rel_ts, flags, frames_start) = parse_block_header(data)?;
    let pts_ns = block_pts_ns(cluster_base_tick, rel_ts, scale);
    let lacing = (flags >> 1) & 0x03; // bits 0x06 are LACING (section 10.1)
    let payload =
        data.get(frames_start..).ok_or(ReadError::Malformed("block payload truncated"))?;
    lace_buf.clear();
    let header = match lacing {
        0b00 => {
            frame(track, pts_ns, 0, 1, frames_start..data.len());
            return Ok(());
        }
        0b01 => xiph_lace_sizes(payload, lace_buf)?,
        0b11 => ebml_lace_sizes(payload, lace_buf)?,
        0b10 => fixed_lace_sizes(payload, lace_buf)?,
        _ => unreachable!("2-bit lacing field"),
    };
    // Every size is checked before a frame goes out, so a bad lace costs its block alone.
    let sized = lace_buf.iter().try_fold(header, |at, &sz| at.checked_add(sz));
    match sized {
        None => return Err(ReadError::Malformed("laced frame size overflow")),
        Some(end) if end > payload.len() => {
            return Err(ReadError::Malformed("laced frame runs past the block"));
        }
        Some(_) => {}
    }
    let (mut off, n) = (header, lace_buf.len() + 1);
    for (k, &sz) in lace_buf.iter().enumerate() {
        frame(track, pts_ns, k, n, frames_start + off..frames_start + off + sz);
        off += sz;
    }
    frame(track, pts_ns, n - 1, n, frames_start + off..data.len());
    Ok(())
}

pub(crate) fn parse_block_header(data: &[u8]) -> Result<(u64, i16, u8, usize), ReadError> {
    let (track, track_len, _ao) = ebml::read_vint(data, 0)?;
    let after_track = track_len;
    if after_track + 3 > data.len() {
        return Err(ReadError::Malformed("block header truncated (timestamp/flags)"));
    }
    let rel_ts = i16::from_be_bytes([data[after_track], data[after_track + 1]]);
    let flags = data[after_track + 2];
    Ok((track, rel_ts, flags, after_track + 3))
}

pub(crate) fn block_pts_ns(cluster_base_tick: i64, rel_ts: i16, scale: u64) -> u64 {
    let abs_tick = cluster_base_tick.saturating_add(rel_ts as i64).max(0) as u64;
    abs_tick.saturating_mul(scale).min(PTS_NONE - 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuedBlock {
    pub track: u64,
    pub pts_ns: u64,
    pub duration_ns: Option<u64>,
    pub keyframe: bool,
    pub payload: std::ops::Range<usize>,
}

pub fn cluster_timestamp(data: &[u8]) -> Option<i64> {
    let mut at = 0;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if h.id == id::TIMESTAMP {
            return Some(read_uint(data.get(h.data_start..end)?).min(i64::MAX as u64) as i64);
        }
        if h.id == id::SIMPLE_BLOCK || h.id == id::BLOCK_GROUP {
            return None;
        }
        at = end;
    }
    None
}

pub fn cued_block(el: &[u8], cluster_tick: i64, scale: u64) -> Result<CuedBlock, ReadError> {
    let h = ebml::read_element_header(el, 0)?;
    let end = h.data_end()?.ok_or(ReadError::Malformed("unknown-size block"))?;
    if end > el.len() {
        return Err(ReadError::Incomplete);
    }
    let (body, keyframe, duration_ns) = if h.id == id::SIMPLE_BLOCK {
        ((h.data_start, end), None, None)
    } else if h.id == id::BLOCK_GROUP {
        let g = block_group_parts(&el[h.data_start..end], scale)?;
        let (a, b) = g.block.ok_or(ReadError::Malformed("BlockGroup without a Block"))?;
        ((h.data_start + a, h.data_start + b), Some(g.keyframe), g.duration_ns)
    } else {
        return Err(ReadError::Malformed("no block at the cued place"));
    };
    let (track, rel, flags, start) = parse_block_header(&el[body.0..body.1])?;
    if flags & 0x06 != 0 {
        return Err(ReadError::Malformed("a laced block at a cued place"));
    }
    Ok(CuedBlock {
        track,
        pts_ns: block_pts_ns(cluster_tick, rel, scale),
        duration_ns,
        keyframe: keyframe.unwrap_or(flags & 0x80 != 0),
        payload: body.0 + start..body.1,
    })
}

/// A frame's timing as the Block gives it.
struct Stamp {
    pts_ns: u64,
    duration_ns: Option<u64>,
    keyframe: bool,
    discard_ns: u64,
}

fn push_frame(
    out: &mut VecDeque<Queued>,
    free: &mut FreePool,
    track: u64,
    Stamp { pts_ns, duration_ns, keyframe, discard_ns }: Stamp,
    data: &[u8],
    t35: Option<&[u8]>,
    coded: bool,
) {
    // `Vec::new` does not allocate, so empty laced frames cost no pooled octets.
    #[allow(clippy::disallowed_methods)]
    let mut buf = if data.is_empty() { Vec::new() } else { free.take(data.len()) };
    buf.clear();
    buf.extend_from_slice(data);
    let t35 = pooled(free, t35);
    let frame =
        Frame { track_number: track, pts_ns, duration_ns, keyframe, data: buf, t35, discard_ns };
    out.push_back(Queued { frame, coded });
}

/// Xiph lacing sizes (RFC 9559 section 10.3.2).
fn xiph_lace_sizes(payload: &[u8], sizes: &mut Vec<usize>) -> Result<usize, ReadError> {
    let count_minus_one =
        *payload.first().ok_or(ReadError::Malformed("Xiph lace: missing frame count"))?;
    let n_coded = count_minus_one as usize; // sizes stored for all but the last frame
    let mut at = 1usize;
    for _ in 0..n_coded {
        let mut sz = 0usize;
        loop {
            let b = *payload
                .get(at)
                .ok_or(ReadError::Malformed("Xiph lace: size runs past the block"))?;
            at += 1;
            sz = sz
                .checked_add(b as usize)
                .ok_or(ReadError::Malformed("Xiph lace: size overflow"))?;
            if b != 0xFF {
                break; // a value < 0xFF terminates this frame's size (section 10.3.2)
            }
        }
        sizes.push(sz);
    }
    Ok(at)
}

/// EBML lacing sizes (RFC 9559 section 10.3.3), signed VINT deltas after the first.
fn ebml_lace_sizes(payload: &[u8], sizes: &mut Vec<usize>) -> Result<usize, ReadError> {
    let count_minus_one =
        *payload.first().ok_or(ReadError::Malformed("EBML lace: missing frame count"))?;
    let n_coded = count_minus_one as usize; // sizes stored for all but the last frame
    let mut at = 1usize;
    if n_coded == 0 {
        return Ok(at);
    }
    let (first, len, _ao) = ebml::read_vint(payload, at)?;
    at += len;
    let mut prev = first as i64;
    sizes.push(
        usize::try_from(prev)
            .map_err(|_| ReadError::Malformed("EBML lace: negative first size"))?,
    );
    for _ in 1..n_coded {
        let (raw, len, _ao) = ebml::read_vint(payload, at)?;
        let bias = (1i64 << (7 * len - 1)) - 1; // subtract 2^((7n)-1)-1, section 10.3.3
        let delta = raw as i64 - bias;
        prev = prev
            .checked_add(delta)
            .ok_or(ReadError::Malformed("EBML lace: size delta overflow"))?;
        if prev < 0 {
            return Err(ReadError::Malformed("EBML lace: negative frame size"));
        }
        sizes.push(prev as usize);
        at += len;
    }
    Ok(at)
}

/// Fixed-size lacing sizes (RFC 9559 section 10.3.4).
fn fixed_lace_sizes(payload: &[u8], sizes: &mut Vec<usize>) -> Result<usize, ReadError> {
    let count_minus_one =
        *payload.first().ok_or(ReadError::Malformed("fixed lace: missing frame count"))?;
    let count = count_minus_one as usize + 1;
    let body = payload.len() - 1; // frames follow the 1-octet header
    if !body.is_multiple_of(count) {
        return Err(ReadError::Malformed("fixed lace: block size not divisible by frame count"));
    }
    let each = body / count;
    // An empty body would otherwise turn 7 octets into 256 queued frames.
    if each == 0 && count > 1 {
        return Err(ReadError::Malformed("fixed lace: frames are empty"));
    }
    sizes.extend(std::iter::repeat_n(each, count - 1));
    Ok(1)
}

/// Whether the Cluster header at `at` leads with its Timestamp, which comes first or after a
/// CRC-32, Position or PrevSize (RFC 9559 section 5.1.3.1). A Cluster ID found by a scan is
/// otherwise taken for four octets of a payload. `None` while the octets run out.
pub(crate) fn cluster_led(data: &[u8], at: usize) -> Option<bool> {
    const CRC_32: &[u8] = &[0xBF];
    const POSITION: &[u8] = &[0xA7];
    const PREV_SIZE: &[u8] = &[0xAB];
    let mut child = match ebml::read_element_header(data, at) {
        Ok(h) => h.data_start,
        Err(ReadError::Incomplete) => return None,
        Err(_) => return Some(false),
    };
    for _ in 0..4 {
        let h = match ebml::read_element_header(data, child) {
            Ok(h) => h,
            Err(ReadError::Incomplete) => return None,
            Err(_) => return Some(false),
        };
        if h.id == id::TIMESTAMP {
            return Some(h.size.is_some_and(|size| size <= 8));
        }
        let small = h.size.filter(|size| *size <= 8);
        match small {
            Some(size) if h.id == CRC_32 || h.id == POSITION || h.id == PREV_SIZE => {
                child = h.data_start + size as usize;
            }
            _ => return Some(false),
        }
    }
    Some(false)
}

pub(crate) fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Cues position and geometry from a front SeekHead and Info (RFC 9559 section 5.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekHeadInfo {
    /// Segment Positions (RFC 9559 section 4) are measured from here.
    pub segment_data_start: u64,
    pub cues_pos: Option<u64>,
    /// A SeekHead past the header that a front one points to, to look in for Cues.
    pub next_head: Option<u64>,
    pub timestamp_scale: u64,
}

/// Extract the front SeekHead and TimestampScale from the header bytes. `None` when no Segment is
/// found.
pub fn parse_seek_head(header: &[u8]) -> Option<SeekHeadInfo> {
    let mut at = 0usize;
    let segment_data_start = loop {
        let h = ebml::read_element_header(header, at).ok()?;
        if h.id == id::SEGMENT {
            break h.data_start as u64;
        }
        let end = h.data_end().ok()??;
        if end <= at || end > header.len() {
            return None; // truncated before the Segment, or a malformed size
        }
        at = end;
    };

    let (mut cues_pos, mut next_head) = (None, None);
    let mut timestamp_scale = DEFAULT_TIMESTAMP_SCALE;
    let mut at = segment_data_start as usize;
    while at < header.len() {
        let Ok(h) = ebml::read_element_header(header, at) else {
            break;
        };
        if h.id == id::CLUSTER {
            break; // frames begin, nothing more of interest in the header
        }
        let Ok(Some(end)) = h.data_end() else { break };
        if end > header.len() {
            break;
        }
        if h.id == id::SEEK_HEAD {
            let body = &header[h.data_start..end];
            cues_pos = seek_head_pos(body, id::CUES).or(cues_pos);
            let beyond = |p: &u64| segment_data_start.saturating_add(*p) >= header.len() as u64;
            next_head = seek_head_pos(body, id::SEEK_HEAD).filter(beyond).or(next_head);
        } else if h.id == id::INFO
            && let Ok((Some(scale), _)) = parse_info(&header[h.data_start..end])
        {
            timestamp_scale = scale;
        }
        at = end;
    }

    let next_head = next_head.filter(|_| cues_pos.is_none());
    Some(SeekHeadInfo { segment_data_start, cues_pos, next_head, timestamp_scale })
}

/// The Segment Position a SeekHead body gives the element `target`.
pub fn seek_head_pos(data: &[u8], target: &[u8]) -> Option<u64> {
    let mut at = 0usize;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if end > data.len() {
            return None;
        }
        if h.id == id::SEEK
            && let Some(p) = seek_entry_pos(&data[h.data_start..end], target)
        {
            return Some(p);
        }
        at = end;
    }
    None
}

fn seek_entry_pos(data: &[u8], target: &[u8]) -> Option<u64> {
    let mut at = 0usize;
    let mut is_target = false;
    let mut pos = None;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if end > data.len() {
            return None;
        }
        let body = &data[h.data_start..end];
        if h.id == id::SEEK_ID {
            is_target = body == target;
        } else if h.id == id::SEEK_POSITION {
            pos = Some(read_uint(body));
        }
        at = end;
    }
    if is_target { pos } else { None }
}

/// Parse a `Cues` element (with its header) into sorted `(time_ns, absolute_byte)` (RFC 9559
/// section 5.1.5).
// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
pub fn parse_cues(
    cues_bytes: &[u8],
    segment_data_start: u64,
    timestamp_scale: u64,
) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let Some(body) = cues_body(cues_bytes) else {
        return out;
    };

    let mut at = 0usize;
    while at < body.len() {
        let Ok(cp) = ebml::read_element_header(body, at) else {
            break;
        };
        let cp_end = match cp.data_end() {
            Ok(Some(e)) if e <= body.len() => e,
            _ => break, // truncated CuePoint, stop rather than mis-slice
        };
        if cp.id == id::CUE_POINT
            && let Some((ticks, pos)) = parse_cue_point(&body[cp.data_start..cp_end])
        {
            let time_ns = ticks.saturating_mul(timestamp_scale);
            let abs = segment_data_start.saturating_add(pos);
            out.push((time_ns, abs));
        }
        at = cp_end;
    }
    out.sort_by_key(|&(t, _)| t);
    out
}

fn cues_body(cues_bytes: &[u8]) -> Option<&[u8]> {
    let h = ebml::read_element_header(cues_bytes, 0).ok()?;
    if h.id != id::CUES {
        return None;
    }
    let end = match h.data_end() {
        Ok(Some(e)) => e.min(cues_bytes.len()),
        _ => cues_bytes.len(),
    };
    cues_bytes.get(h.data_start..end)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cue {
    pub time_ns: u64,
    pub track: u64,
    pub byte: u64,
    pub duration_ns: Option<u64>,
    pub relative: Option<u64>,
}

/// Every position of every CuePoint with its track, ascending by time.
// Cold path, once per stream.
#[allow(clippy::disallowed_methods)]
pub fn parse_cue_tracks(
    cues_bytes: &[u8],
    segment_data_start: u64,
    timestamp_scale: u64,
) -> Vec<Cue> {
    let mut out = Vec::new();
    let Some(body) = cues_body(cues_bytes) else {
        return out;
    };
    let mut at = 0usize;
    while at < body.len() {
        let Ok(cp) = ebml::read_element_header(body, at) else {
            break;
        };
        let cp_end = match cp.data_end() {
            Ok(Some(e)) if e <= body.len() => e,
            _ => break,
        };
        if cp.id == id::CUE_POINT {
            let point = &body[cp.data_start..cp_end];
            cue_point_tracks(point, segment_data_start, timestamp_scale, &mut out);
        }
        at = cp_end;
    }
    out.sort_by_key(|c| c.time_ns);
    out
}

/// A file's Cues as read once, every track's, for every demuxer of it to take.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CueIndex {
    pub cues: Vec<Cue>,
    /// Where the Cues element starts, `None` when none were read.
    pub at: Option<u64>,
}

/// The bytes an element header takes at most, a 4 octet ID and an 8 octet size.
pub(crate) const ELEMENT_HEAD: usize = 12;

/// The read in flight of [`CuesAt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CuesState {
    Start,
    /// The Cues element's head.
    Head,
    /// The Cues whole.
    Body,
}

/// The Cues the front SeekHead of a header names, every track's, their element head read and
/// then the whole of them. Empty for a file without them, for a position that holds no Cues and
/// for Cues past `max` bytes, which are not read.
#[derive(Debug)]
pub struct CuesAt {
    /// Where the Cues start, None when the header names none.
    at: Option<u64>,
    segment_data_start: u64,
    timestamp_scale: u64,
    max: u64,
    state: CuesState,
    hd: [u8; ELEMENT_HEAD],
    body: Vec<u8>,
    got: usize,
}

// Cold path, once per file.
#[allow(clippy::disallowed_methods)]
impl CuesAt {
    pub fn new(header: &[u8], max: u64) -> Self {
        let sh = parse_seek_head(header);
        let at = sh.as_ref().and_then(|sh| sh.cues_pos?.checked_add(sh.segment_data_start));
        CuesAt {
            at,
            segment_data_start: sh.as_ref().map_or(0, |sh| sh.segment_data_start),
            timestamp_scale: sh.as_ref().map_or(0, |sh| sh.timestamp_scale),
            max,
            state: CuesState::Start,
            hd: [0; ELEMENT_HEAD],
            body: Vec::new(),
            got: 0,
        }
    }
}

// Cold path, once per file.
#[allow(clippy::disallowed_methods)]
impl profluens_core::fetch::Fetch for CuesAt {
    type Out = CueIndex;
    type Err = std::convert::Infallible;

    fn poll(&mut self) -> Result<Step<CueIndex>, Self::Err> {
        let Some(at) = self.at else { return Ok(Step::Done(CueIndex::default())) };
        match self.state {
            CuesState::Start => {
                self.state = CuesState::Head;
                Ok(Step::Need(Need { at, len: ELEMENT_HEAD }))
            }
            CuesState::Head => {
                self.state = CuesState::Start;
                let Ok(h) = ebml::read_element_header(&self.hd[..self.got], 0) else {
                    return Ok(Step::Done(CueIndex::default()));
                };
                let len = h.size.and_then(|s| (h.data_start as u64).checked_add(s));
                let Some(len) = len.filter(|&len| h.id == id::CUES && len <= self.max) else {
                    return Ok(Step::Done(CueIndex::default()));
                };
                self.body = vec![0; len as usize];
                self.state = CuesState::Body;
                Ok(Step::Need(Need { at, len: len as usize }))
            }
            CuesState::Body => {
                self.state = CuesState::Start;
                self.body.truncate(self.got);
                let cues =
                    parse_cue_tracks(&self.body, self.segment_data_start, self.timestamp_scale);
                self.body = Vec::new();
                Ok(Step::Done(CueIndex { at: (!cues.is_empty()).then_some(at), cues }))
            }
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        match self.state {
            CuesState::Body => &mut self.body,
            _ => &mut self.hd,
        }
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.buf().len());
    }
}

/// The largest span between cued Clusters bounds a block, `None` with fewer than two.
// Cold path, once per open.
#[allow(clippy::disallowed_methods)]
pub fn cues_demand(cues: &[Cue], cues_at: u64, file_len: u64) -> Demand {
    let mut at: Vec<u64> = cues.iter().map(|c| c.byte).collect();
    at.sort_unstable();
    at.dedup();
    let (Some(&last), true) = (at.last(), at.len() > 1) else {
        return Demand::unknown();
    };
    let end = if cues_at > last { cues_at.min(file_len) } else { file_len };
    let spans = at.windows(2).map(|w| w[1] - w[0]);
    let largest = spans.max().unwrap_or(0).max(end.saturating_sub(last));
    Demand { known: Known::Clusters, largest, peak: 0, window: 0 }
}

fn cue_point_tracks(data: &[u8], segment_data_start: u64, scale: u64, out: &mut Vec<Cue>) {
    let first = out.len();
    let mut time = None;
    let mut at = 0usize;
    while at < data.len() {
        let Ok(h) = ebml::read_element_header(data, at) else {
            break;
        };
        let end = match h.data_end() {
            Ok(Some(e)) if e <= data.len() => e,
            _ => break,
        };
        let body = &data[h.data_start..end];
        if h.id == id::CUE_TIME {
            time = Some(read_uint(body).saturating_mul(scale));
        } else if h.id == id::CUE_TRACK_POSITIONS {
            out.extend(cue_track_position(body, segment_data_start, scale));
        }
        at = end;
    }
    match time {
        Some(t) => out[first..].iter_mut().for_each(|c| c.time_ns = t),
        None => out.truncate(first),
    }
}

fn cue_track_position(data: &[u8], segment_data_start: u64, scale: u64) -> Option<Cue> {
    let (mut track, mut pos, mut duration_ns, mut relative) = (None, None, None, None);
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
            duration_ns = Some(value.saturating_mul(scale));
        } else if h.id == id::CUE_RELATIVE_POSITION {
            relative = Some(value);
        }
        at = end;
    }
    let byte = segment_data_start.saturating_add(pos?);
    Some(Cue { time_ns: 0, track: track?, byte, duration_ns, relative })
}

fn parse_cue_point(data: &[u8]) -> Option<(u64, u64)> {
    let mut at = 0usize;
    let mut time = None;
    let mut cluster_pos = None;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if end > data.len() {
            return None;
        }
        if h.id == id::CUE_TIME {
            time = Some(read_uint(&data[h.data_start..end]));
        } else if h.id == id::CUE_TRACK_POSITIONS && cluster_pos.is_none() {
            cluster_pos = cue_track_positions_cluster(&data[h.data_start..end]);
        }
        at = end;
    }
    match (time, cluster_pos) {
        (Some(t), Some(p)) => Some((t, p)),
        _ => None,
    }
}

fn cue_track_positions_cluster(data: &[u8]) -> Option<u64> {
    let mut at = 0usize;
    let mut pos = None;
    while at < data.len() {
        let h = ebml::read_element_header(data, at).ok()?;
        let end = h.data_end().ok()??;
        if end > data.len() {
            return None;
        }
        if h.id == id::CUE_CLUSTER_POSITION {
            pos = Some(read_uint(&data[h.data_start..end]));
        }
        at = end;
    }
    pos
}

/// A big-endian two's complement EBML signed integer (RFC 8794 section 7.1).
pub(crate) fn read_int(data: &[u8]) -> i64 {
    match data.len() {
        0 => 0,
        n @ 1..=8 => (read_uint(data) << (64 - 8 * n)) as i64 >> (64 - 8 * n),
        _ => read_uint(&data[..8]) as i64,
    }
}

pub(crate) fn read_uint(data: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in data.iter().take(8) {
        v = (v << 8) | b as u64;
    }
    v
}

fn read_float(data: &[u8]) -> Result<f64, ReadError> {
    match data.len() {
        0 => Ok(0.0),
        4 => Ok(f32::from_bits(u32::from_be_bytes([data[0], data[1], data[2], data[3]])) as f64),
        8 => Ok(f64::from_bits(u64::from_be_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]))),
        _ => Err(ReadError::Malformed("EBML float must be 0, 4, or 8 octets")),
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::writer::{MatroskaWriter, TrackConfig};

    #[test]
    fn a_track_entry_says_what_the_track_is_for() {
        let one = |id: &[u8], v: u8| [id, &[0x81, v]].concat();
        let entry = [
            one(&[0xD7], 1),
            one(&[0xB9], 0),
            one(&[0x55, 0xAF], 1),
            one(&[0x55, 0xAB], 1),
            one(&[0x55, 0xAC], 1),
            one(&[0x55, 0xAD], 1),
            one(&[0x55, 0xAE], 1),
        ]
        .concat();
        let track = parse_track_entry(&entry, &mut 0).unwrap();
        let all = Disposition {
            enabled: false,
            commentary: true,
            hearing_impaired: true,
            visual_impaired: true,
            descriptions: true,
            original: true,
        };
        assert_eq!(track.disposition, all);
        let plain = parse_track_entry(&one(&[0xD7], 1), &mut 0).unwrap();
        assert_eq!(plain.disposition, Disposition::MAIN);
    }

    /// A video TrackEntry of number 1 and uid 77 whose Video holds `video`.
    fn video_entry(video: &[u8]) -> Vec<u8> {
        let mut e = Vec::new();
        ebml::write_uint(&mut e, id::TRACK_NUMBER, 1);
        ebml::write_uint(&mut e, id::TRACK_UID, 77);
        ebml::write_binary(&mut e, id::VIDEO, video);
        e
    }

    fn projection(kind: u64, yaw: f64, pitch: f64, roll: f32) -> Vec<u8> {
        let mut p = Vec::new();
        ebml::write_uint(&mut p, id::PROJECTION_TYPE, kind);
        ebml::write_f64(&mut p, id::PROJECTION_POSE_YAW, yaw);
        ebml::write_f64(&mut p, id::PROJECTION_POSE_PITCH, pitch);
        ebml::write_f32(&mut p, id::PROJECTION_POSE_ROLL, roll);
        let mut v = Vec::new();
        ebml::write_binary(&mut v, id::PROJECTION, &p);
        v
    }

    #[test]
    fn a_video_track_gives_its_display_size_and_turn() {
        let mut video = Vec::new();
        ebml::write_uint(&mut video, id::PIXEL_WIDTH, 1440);
        ebml::write_uint(&mut video, id::PIXEL_HEIGHT, 1080);
        ebml::write_uint(&mut video, id::DISPLAY_WIDTH, 1920);
        ebml::write_uint(&mut video, id::DISPLAY_HEIGHT, 1080);
        ebml::write_uint(&mut video, id::DISPLAY_UNIT, 0);
        video.extend(projection(0, 0.0, 0.0, 90.0));
        let t = parse_track_entry(&video_entry(&video), &mut 0).unwrap();
        assert_eq!((t.pixel_width, t.display_width, t.display_height), (1440, 1920, 1080));
        assert_eq!((t.display_unit, t.uid, t.bitrate), (0, 77, 0));
        assert_eq!(t.rotation, 270, "a roll of 90 counter-clockwise is 270 clockwise");
        let turn = |kind, yaw, pitch, roll| {
            parse_track_entry(&video_entry(&projection(kind, yaw, pitch, roll)), &mut 0)
                .unwrap()
                .rotation
        };
        assert_eq!(turn(0, 0.0, 0.0, -90.0), 90);
        assert_eq!(turn(0, 0.0, 0.0, 180.0), 180);
        assert_eq!(turn(0, 0.0, 0.0, -180.0), 180);
        assert_eq!(turn(0, 0.0, 0.0, 0.0), 0);
        assert_eq!(turn(0, 0.0, 0.0, 45.0), 0, "no quarter turn");
        assert_eq!(turn(0, 180.0, 0.0, 0.0), 0, "a flip is no turn");
        assert_eq!(turn(0, 0.0, 10.0, 90.0), 0, "a pitch is no turn");
        assert_eq!(turn(1, 0.0, 0.0, 90.0), 0, "a sphere is no turn");
        assert_eq!(turn(0, 0.0, 0.0, f32::NAN), 0);
        let plain = parse_track_entry(&video_entry(&[]), &mut 0).unwrap();
        assert_eq!((plain.display_width, plain.display_height, plain.rotation), (0, 0, 0));
        let mut bad = Vec::new();
        ebml::write_binary(&mut bad, id::PROJECTION, &[0x76, 0x75, 0x83, 1, 2, 3]);
        let t = parse_track_entry(&video_entry(&bad), &mut 0).unwrap();
        assert_eq!(t.rotation, 0, "a 3 octet float is no turn and no refusal");
    }

    #[test]
    fn statistics_tags_in_the_head_give_each_track_its_bitrate() {
        let simple = |name: &str, value: &str| {
            let mut s = Vec::new();
            ebml::write_string(&mut s, id::TAG_NAME, name);
            ebml::write_string(&mut s, id::TAG_STRING, value);
            let mut out = Vec::new();
            ebml::write_binary(&mut out, id::SIMPLE_TAG, &s);
            out
        };
        let tag = |uid: u64, simples: &[Vec<u8>]| {
            let mut targets = Vec::new();
            ebml::write_uint(&mut targets, id::TAG_TRACK_UID, uid);
            let mut body = Vec::new();
            ebml::write_binary(&mut body, id::TARGETS, &targets);
            body.extend(simples.concat());
            let mut out = Vec::new();
            ebml::write_binary(&mut out, id::TAG, &body);
            out
        };
        let mut tags = Vec::new();
        let body = [
            tag(77, &[simple("DURATION", "00:01:00"), simple("BPS", "4500000")]),
            tag(78, &[simple("BPS-eng", " 128000 ")]),
            tag(0, &[simple("BPS", "1")]),
            tag(79, &[simple("BPS", "fast")]),
        ]
        .concat();
        ebml::write_binary(&mut tags, id::TAGS, &body);
        let mut a = parse_track_entry(&video_entry(&[]), &mut 0).unwrap();
        let mut b = a.clone();
        let mut c = a.clone();
        (b.uid, c.uid) = (78, 79);
        let mut tracks = [a.clone(), b, c];
        crate::tags::bitrates(&tags, &mut tracks);
        let got: Vec<u64> = tracks.iter().map(|t| t.bitrate).collect();
        assert_eq!(got, [4_500_000, 128_000, 0]);
        let of = |uid| crate::tags::bitrate_of(&tags, uid);
        assert_eq!(
            (of(77), of(78), of(79), of(0), of(5)),
            (Some(4_500_000), Some(128_000), None, None, None)
        );
        assert_eq!(crate::tags::bitrate_of(&body, 77), None, "the Tags element with its head");
        // Tags the reader walks in the head, after Tracks
        let mut tracks_el = Vec::new();
        ebml::write_binary(&mut tracks_el, id::TRACK_ENTRY, &video_entry(&[]));
        let mut head = Vec::new();
        ebml::write_binary(&mut head, id::EBML, &[]);
        ebml::write_id(&mut head, id::SEGMENT);
        ebml::write_unknown_size(&mut head);
        ebml::write_binary(&mut head, id::TRACKS, &tracks_el);
        head.extend_from_slice(&tags);
        let mut r = MatroskaReader::new();
        r.push(&head).unwrap();
        assert_eq!(r.tracks()[0].bitrate, 4_500_000);
        a.uid = 0;
        let mut zero = [a];
        crate::tags::bitrates(&tags, &mut zero);
        assert_eq!(zero[0].bitrate, 0, "a track with no uid is named by no tag");
    }

    #[test]
    fn cues_bound_a_block_by_the_largest_span_between_cued_clusters() {
        let cue = |byte: u64, track: u64| Cue {
            time_ns: byte,
            track,
            byte,
            duration_ns: None,
            relative: None,
        };
        let cues = [cue(100, 1), cue(1000, 1), cue(1000, 2), cue(5000, 1), cue(7000, 1)];
        let d = cues_demand(&cues, 9000, 10_000);
        assert_eq!(d, Demand { known: Known::Clusters, largest: 4000, peak: 0, window: 0 });
        assert_eq!(
            cues_demand(&cues, 12_000, 20_000).largest,
            5000,
            "the last span runs to the Cues"
        );
        assert_eq!(
            cues_demand(&cues, 50, 20_000).largest,
            13_000,
            "Cues up front, to the file's end"
        );
        assert_eq!(cues_demand(&[], 9000, 10_000), Demand::unknown());
        assert_eq!(
            cues_demand(&[cue(100, 1), cue(100, 2)], 9000, 10_000),
            Demand::unknown(),
            "one Cluster"
        );
    }

    /// A class is >= the request and at most 25 percent over it, and `class_index` round-trips.
    #[test]
    fn payload_size_classes_fit_and_round_trip() {
        let mut seen = std::collections::BTreeMap::new();
        let sizes = (0usize..=8192).chain((13..CLASS_MAX_SHIFT).flat_map(|k| {
            let b = 1usize << k;
            [b, b + 1, b + b / 3, b + b / 2, b + b - 1]
        }));
        for n in sizes {
            let size = class_size(n);
            assert!(size >= n.max(1 << CLASS_MIN_SHIFT), "class {size} too small for {n}");
            assert!(
                n <= 1 << CLASS_MIN_SHIFT || size * 4 <= n * 5,
                "class {size} more than 25% over {n}"
            );
            let idx = class_index(size);
            assert!(idx < CLASSES, "index {idx} out of range for class {size} (n={n})");
            let prev = seen.insert(idx, size);
            assert!(prev.is_none_or(|p| p == size), "two class sizes share bucket {idx}");
        }
        let mut pool = FreePool::new();
        for size in seen.values() {
            let b = pool.take(*size);
            assert!(b.capacity() >= *size);
            pool.put(b, usize::MAX);
        }
        for size in seen.values() {
            let b = pool.take(*size);
            assert_eq!(b.capacity(), *size, "recycled buffer changed class");
        }
    }

    #[test]
    fn payload_pool_honours_its_byte_budget() {
        let mut pool = FreePool::new();
        let budget = 4096;
        for _ in 0..100 {
            let mut b = pool.take(1000);
            b.extend_from_slice(&[0u8; 1000]);
            pool.put(b, budget);
        }
        assert!(pool.bytes <= budget, "retained {} bytes over budget {budget}", pool.bytes);
        let mut pool = FreePool::new();
        for _ in 0..10 {
            let b = pool.take(1000);
            pool.put(b, 0);
        }
        assert_eq!(pool.bytes, 0);
    }

    fn build_single(frames: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
        let codec_private = vec![b'f', b'L', b'a', b'C', 0x00, 0x00, 0x00, 0x22];
        let track = TrackConfig::flac(1, codec_private.clone(), 48_000.0, 2, 16);
        let mut w = MatroskaWriter::new(vec![track]);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        let dur = 1_000_000u64; // 1 ms/frame in ns (1 tick at the default scale)
        for (i, f) in frames.iter().enumerate() {
            w.write_frame(&mut out, 1, i as u64 * dur, f, true).unwrap();
        }
        w.finalize(&mut out);
        (out, codec_private)
    }

    #[test]
    fn discovers_track_and_decodes_frames() {
        let frames: Vec<&[u8]> = vec![&[0xAA, 0xBB], &[0xCC], &[0xDD, 0xEE, 0xFF]];
        let (stream, codec_private) = build_single(&frames);

        let mut r = MatroskaReader::new();
        r.push(&stream).unwrap();
        assert!(r.tracks_ready(), "tracks discovered");
        assert_eq!(r.tracks().len(), 1);
        let t = &r.tracks()[0];
        assert_eq!(t.track_number, 1);
        assert_eq!(t.codec_id, "A_FLAC");
        assert_eq!(t.codec_private, codec_private);
        assert_eq!(t.sampling_frequency, 48_000.0);
        assert_eq!(t.channels, 2);
        assert_eq!(t.bit_depth, 16);

        let mut got = Vec::new();
        while let Some(f) = r.next_frame() {
            assert_eq!(f.track_number, 1);
            assert!(f.keyframe);
            got.push(f.data);
        }
        let want: Vec<Vec<u8>> = frames.iter().map(|f| f.to_vec()).collect();
        assert_eq!(got, want, "frames decode bit-exact and in order");
    }

    #[test]
    fn null_terminated_track_strings_read_as_their_value() {
        let track = TrackConfig::audio(1, "A_AAC\0", vec![0x12, 0x10], 44_100.0, 2, 16);
        let mut w = MatroskaWriter::new(vec![track]);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        w.finalize(&mut out);
        let mut r = MatroskaReader::new();
        r.push(&out).unwrap();
        assert!(r.tracks_ready());
        let t = &r.tracks()[0];
        assert_eq!(t.codec_id, "A_AAC");
        assert_eq!(crate::codec::family_for(&t.codec_id), "aac");
        assert_eq!(t.codec_private, vec![0x12, 0x10]);
        assert_eq!(t.channels, 2);
    }

    #[test]
    fn incremental_byte_by_byte_matches_whole() {
        let frames: Vec<&[u8]> = vec![&[1, 2, 3, 4], &[5, 6], &[7, 8, 9]];
        let (stream, _) = build_single(&frames);

        let mut r = MatroskaReader::new();
        for &b in &stream {
            r.push(&[b]).unwrap();
        }
        let mut got = Vec::new();
        while let Some(f) = r.next_frame() {
            got.push(f.data);
        }
        let want: Vec<Vec<u8>> = frames.iter().map(|f| f.to_vec()).collect();
        assert_eq!(got, want, "byte-by-byte feeding decodes identically");
    }

    #[test]
    fn timestamps_scale_to_nanoseconds() {
        let frames: Vec<&[u8]> = vec![&[0], &[1], &[2]];
        let (stream, _) = build_single(&frames);
        let mut r = MatroskaReader::new();
        r.push(&stream).unwrap();
        let pts: Vec<u64> = std::iter::from_fn(|| r.next_frame().map(|f| f.pts_ns)).collect();
        assert_eq!(pts, vec![0, 1_000_000, 2_000_000], "pts in ns = tick * scale");
    }

    #[test]
    fn simple_block_has_no_duration() {
        let frames: Vec<&[u8]> = vec![b"Hello", b"World"];
        let (stream, _) = build_single(&frames);
        let mut r = MatroskaReader::new();
        r.push(&stream).unwrap();
        while let Some(f) = r.next_frame() {
            assert_eq!(f.duration_ns, None, "a SimpleBlock never declares a duration");
        }
    }

    #[test]
    fn block_group_carries_scaled_block_duration() {
        use crate::ebml;
        let track =
            TrackConfig::flac(1, vec![b'f', b'L', b'a', b'C', 0, 0, 0, 0x22], 48_000.0, 2, 16);
        let mut w = MatroskaWriter::new(vec![track]);
        let mut stream = Vec::new();
        w.write_header(&mut stream).unwrap();

        let cue_text = b"a subtitle cue";
        let mut block_body = Vec::new();
        block_body.push(0x81); // TrackNumber 1 as a 1-octet VINT
        block_body.extend_from_slice(&0i16.to_be_bytes()); // rel ts 0
        block_body.push(0x00); // flags: no keyframe/lacing (BlockGroup decides keyframe)
        block_body.extend_from_slice(cue_text);

        let mut group = Vec::new();
        ebml::write_binary(&mut group, id::BLOCK, &block_body);
        ebml::write_uint(&mut group, id::BLOCK_DURATION, 1500);

        let mut cluster_body = Vec::new();
        ebml::write_uint(&mut cluster_body, id::TIMESTAMP, 2);
        ebml::write_id(&mut cluster_body, id::BLOCK_GROUP);
        ebml::write_size(&mut cluster_body, group.len() as u64);
        cluster_body.extend_from_slice(&group);

        ebml::write_id(&mut stream, id::CLUSTER);
        ebml::write_size(&mut stream, cluster_body.len() as u64);
        stream.extend_from_slice(&cluster_body);

        let mut r = MatroskaReader::new();
        r.push(&stream).unwrap();
        let f = r.next_frame().expect("one cue frame");
        assert_eq!(f.track_number, 1);
        assert_eq!(f.data, cue_text);
        assert!(f.keyframe, "no ReferenceBlock → keyframe");
        assert_eq!(f.pts_ns, 2_000_000, "pts scaled by TimestampScale");
        assert_eq!(f.duration_ns, Some(1_500_000_000), "BlockDuration scaled to ns");
        assert!(r.next_frame().is_none(), "exactly one frame");
    }

    #[test]
    fn a_track_entry_gives_its_seek_pre_roll() {
        let mut opus = TrackConfig::audio(2, "A_OPUS", b"OpusHead".to_vec(), 48_000.0, 2, 0);
        opus.seek_pre_roll_ns = 80_000_000;
        let flac =
            TrackConfig::flac(1, vec![b'f', b'L', b'a', b'C', 0, 0, 0, 0x22], 48_000.0, 2, 16);
        let mut stream = Vec::new();
        MatroskaWriter::new(vec![flac, opus]).write_header(&mut stream).unwrap();
        let mut r = MatroskaReader::new();
        r.push(&stream).unwrap();
        let pre_rolls: Vec<u64> = r.tracks().iter().map(|t| t.seek_pre_roll_ns).collect();
        assert_eq!(pre_rolls, [0, 80_000_000]);
    }

    #[test]
    fn a_frame_put_back_comes_out_next() {
        let (stream, _) = build_single(&[&[1], &[2], &[3]]);
        let mut r = MatroskaReader::new();
        r.push(&stream).unwrap();
        let first = r.next_frame().unwrap();
        let second = r.next_frame().unwrap();
        r.unread(second);
        r.unread(first);
        let order: Vec<Vec<u8>> = std::iter::from_fn(|| r.next_frame()).map(|f| f.data).collect();
        assert_eq!(order, [vec![1], vec![2], vec![3]]);
    }

    /// A cue point's time and its (track, position, block) entries.
    type Point<'a> = (u64, &'a [(u64, u64, Option<u64>)]);

    fn cues_of(points: &[Point<'_>]) -> Vec<u8> {
        let mut body = Vec::new();
        for (time, positions) in points {
            let mut point = Vec::new();
            for (track, cluster, duration) in positions.iter() {
                let mut position = Vec::new();
                ebml::write_uint(&mut position, id::CUE_TRACK, *track);
                ebml::write_uint(&mut position, id::CUE_CLUSTER_POSITION, *cluster);
                if let Some(duration) = duration {
                    ebml::write_uint(&mut position, id::CUE_DURATION, *duration);
                }
                ebml::write_binary(&mut point, id::CUE_TRACK_POSITIONS, &position);
            }
            ebml::write_uint(&mut point, id::CUE_TIME, *time);
            ebml::write_binary(&mut body, id::CUE_POINT, &point);
        }
        let mut cues = Vec::new();
        ebml::write_binary(&mut cues, id::CUES, &body);
        cues
    }

    #[test]
    fn cue_positions_come_out_with_their_track_and_duration() {
        let cues = cues_of(&[
            (2_000, &[(1, 700, None)]),
            (1_900, &[(3, 100, Some(3_000))]),
            (0, &[(1, 100, None), (2, 100, None)]),
        ]);
        let all = parse_cue_tracks(&cues, 50, 1_000_000);
        let cue = |time_ns, track, byte, duration_ns| Cue {
            time_ns,
            track,
            byte,
            duration_ns,
            relative: None,
        };
        assert_eq!(
            all,
            [
                cue(0, 1, 150, None),
                cue(0, 2, 150, None),
                cue(1_900_000_000, 3, 150, Some(3_000_000_000)),
                cue(2_000_000_000, 1, 750, None),
            ]
        );
        assert_eq!(
            parse_cues(&cues, 50, 1_000_000),
            [(0, 150), (1_900_000_000, 150), (2_000_000_000, 750)]
        );
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_cued_block_reads_on_its_own() {
        let mut position = Vec::new();
        ebml::write_uint(&mut position, id::CUE_TRACK, 3);
        ebml::write_uint(&mut position, id::CUE_CLUSTER_POSITION, 100);
        ebml::write_uint(&mut position, id::CUE_RELATIVE_POSITION, 9);
        let mut point = Vec::new();
        ebml::write_uint(&mut point, id::CUE_TIME, 1_500);
        ebml::write_binary(&mut point, id::CUE_TRACK_POSITIONS, &position);
        let mut body = Vec::new();
        ebml::write_binary(&mut body, id::CUE_POINT, &point);
        let mut cues = Vec::new();
        ebml::write_binary(&mut cues, id::CUES, &body);
        let all = parse_cue_tracks(&cues, 50, 1_000_000);
        assert_eq!(all[0].relative, Some(9));
        assert_eq!(all[0].byte, 150);

        let mut data = Vec::new();
        ebml::write_binary(&mut data, &[0xEC], &[0, 0]);
        ebml::write_uint(&mut data, id::TIMESTAMP, 1_000);
        assert_eq!(cluster_timestamp(&data), Some(1_000));
        let block = [&[0x83, 0x01, 0xF4, 0x00][..], b"hi"].concat();
        let mut group = Vec::new();
        ebml::write_binary(&mut group, id::BLOCK, &block);
        ebml::write_uint(&mut group, id::BLOCK_DURATION, 2_000);
        let mut el = Vec::new();
        ebml::write_binary(&mut el, id::BLOCK_GROUP, &group);
        let b = cued_block(&el, 1_000, 1_000_000).expect("group");
        assert_eq!(
            (b.track, b.pts_ns, b.duration_ns, b.keyframe),
            (3, 1_500_000_000, Some(2_000_000_000), true)
        );
        assert_eq!(&el[b.payload], b"hi");
        let mut simple = Vec::new();
        ebml::write_binary(
            &mut simple,
            id::SIMPLE_BLOCK,
            &[&[0x83, 0xFF, 0xFF, 0x00][..], b"x"].concat(),
        );
        let b = cued_block(&simple, 1_000, 1_000_000).expect("simple");
        assert_eq!((b.pts_ns, b.duration_ns, b.keyframe), (999_000_000, None, false));
        assert_eq!(cued_block(&simple[..simple.len() - 1], 0, 1), Err(ReadError::Incomplete));
        let laced = [0xA3, 0x85, 0x83, 0, 0, 0x02, 0];
        assert!(cued_block(&laced, 0, 1).is_err(), "a lace is refused");
        assert_eq!(cluster_timestamp(&simple), None, "a block before any Timestamp");
    }

    #[test]
    fn malformed_cue_positions_are_skipped() {
        let mut no_track = Vec::new();
        ebml::write_uint(&mut no_track, id::CUE_CLUSTER_POSITION, 9);
        let mut no_time = Vec::new();
        let mut position = Vec::new();
        ebml::write_uint(&mut position, id::CUE_TRACK, 1);
        ebml::write_uint(&mut position, id::CUE_CLUSTER_POSITION, 9);
        ebml::write_binary(&mut no_time, id::CUE_TRACK_POSITIONS, &position);
        let mut timed = Vec::new();
        ebml::write_uint(&mut timed, id::CUE_TIME, 5);
        ebml::write_binary(&mut timed, id::CUE_TRACK_POSITIONS, &no_track);
        let mut body = Vec::new();
        ebml::write_binary(&mut body, id::CUE_POINT, &no_time);
        ebml::write_binary(&mut body, id::CUE_POINT, &timed);
        let mut cues = Vec::new();
        ebml::write_binary(&mut cues, id::CUES, &body);
        assert_eq!(parse_cue_tracks(&cues, 0, 1_000_000), []);
        assert_eq!(parse_cue_tracks(&body, 0, 1_000_000), [], "a payload alone is no Cues");
        let whole = cues_of(&[(0, &[(1, 100, Some(4))]), (7, &[(1, 200, None)])]);
        for cut in 0..whole.len() {
            let _ = parse_cue_tracks(&whole[..cut], 0, 1_000_000);
            let _ = parse_cue_tracks(&whole[cut..], u64::MAX, u64::MAX);
        }
        let far = parse_cue_tracks(&whole, u64::MAX, u64::MAX);
        assert_eq!(
            far[1],
            Cue { time_ns: u64::MAX, track: 1, byte: u64::MAX, duration_ns: None, relative: None }
        );
    }

    #[test]
    fn truncated_input_never_panics() {
        let frames: Vec<&[u8]> = vec![&[0xDE, 0xAD, 0xBE, 0xEF]];
        let (stream, _) = build_single(&frames);
        for n in 0..stream.len() {
            let mut r = MatroskaReader::new();
            let res = r.push(&stream[..n]);
            assert!(res.is_ok(), "prefix of length {n} must buffer, not error");
            while r.next_frame().is_some() {}
        }
    }

    fn build_cued_video(frames: &[(u64, bool, &[u8])]) -> Vec<u8> {
        let track = TrackConfig::vp8(1, 320, 240);
        let mut w = MatroskaWriter::new(vec![track]);
        w.enable_cues();
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        for &(ts, key, data) in frames {
            w.write_frame(&mut out, 1, ts, data, key).unwrap();
        }
        w.finalize(&mut out);
        out
    }

    fn header_prefix(stream: &[u8]) -> &[u8] {
        let cl = stream.windows(4).position(|w| w == id::CLUSTER).expect("a Cluster");
        &stream[..cl]
    }

    #[test]
    fn parse_seek_head_and_cues_roundtrip() {
        let stream = build_cued_video(&[
            (0, true, &[1, 2, 3]),
            (40_000_000, true, &[4, 5]),
            (60_000_000, false, &[6]), // delta in the same cluster as the 40ms keyframe
            (80_000_000, true, &[7, 8, 9]),
            (120_000_000, true, &[10]),
        ]);

        let info = parse_seek_head(header_prefix(&stream)).expect("SeekHead + Info");
        assert_eq!(info.timestamp_scale, 1_000_000, "default 1 ms/tick");
        let seg = stream.windows(4).position(|w| w == id::SEGMENT).unwrap();
        assert_eq!(info.segment_data_start, (seg + 4 + 1) as u64, "Segment data offset derived");
        let cues_pos = info.cues_pos.expect("SeekHead indexes Cues");

        let cues_abs = (info.segment_data_start + cues_pos) as usize;
        assert_eq!(&stream[cues_abs..cues_abs + 4], id::CUES, "Cues position lands on the Cues ID");
        let h = ebml::read_element_header(&stream, cues_abs).unwrap();
        let cues_end = h.data_end().unwrap().unwrap();
        let cues_bytes = &stream[cues_abs..cues_end];

        let index = parse_cues(cues_bytes, info.segment_data_start, info.timestamp_scale);
        assert_eq!(index.len(), 4, "one cue per keyframe cluster");
        let times: Vec<u64> = index.iter().map(|&(t, _)| t).collect();
        assert_eq!(times, vec![0, 40_000_000, 80_000_000, 120_000_000], "times ascending, in ns");
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "sorted ascending");
        for &(_, abs) in &index {
            let a = abs as usize;
            assert_eq!(stream[a], 0x1F, "cue byte points AT a Cluster ID (0x1F...)");
            assert_eq!(&stream[a..a + 4], id::CLUSTER, "cue byte is the full Cluster ID");
        }
    }

    #[test]
    fn cues_are_read_by_the_seek_head() {
        use profluens_core::fetch::{Fetch, drive_slice, drive_slice_log};
        let stream = build_cued_video(&[(0, true, &[1, 2, 3]), (40_000_000, true, &[4, 5])]);
        let head = header_prefix(&stream);
        let mut reads = Vec::new();
        let ix =
            drive_slice_log(&mut CuesAt::new(head, 1 << 20), &stream, &mut reads).expect("read");
        let info = parse_seek_head(head).expect("SeekHead");
        let at = info.segment_data_start + info.cues_pos.expect("Cues");
        assert_eq!(ix.at, Some(at));
        let times: Vec<u64> = ix.cues.iter().map(|c| c.time_ns).collect();
        assert_eq!(times, [0, 40_000_000]);
        assert!(ix.cues.iter().all(|c| c.track == 1));
        assert_eq!(reads.len(), 2, "the element head, then the Cues whole");
        assert!(reads.iter().all(|n| n.at == at), "nothing else is read");
        reads.clear();
        let past = drive_slice_log(&mut CuesAt::new(head, 8), &stream, &mut reads);
        assert_eq!(past, Ok(CueIndex::default()), "past the cap");
        assert_eq!(reads.len(), 1, "only the element head");
        let mut m = CuesAt::new(head, 1 << 20);
        let first = m.poll();
        assert_eq!(
            first,
            Ok(Step::Need(Need { at, len: ELEMENT_HEAD })),
            "the read is the caller's"
        );
        let short = drive_slice(&mut CuesAt::new(head, 1 << 20), &stream[..at as usize]);
        assert_eq!(short, Ok(CueIndex::default()), "past the end");
        let (plain, _) = build_single(&[&[1]]);
        reads.clear();
        let none =
            drive_slice_log(&mut CuesAt::new(header_prefix(&plain), 1 << 20), &plain, &mut reads);
        assert_eq!((none, reads.len()), (Ok(CueIndex::default()), 0));
        assert_eq!(drive_slice(&mut CuesAt::new(&[], 1 << 20), &stream), Ok(CueIndex::default()));
    }

    #[test]
    fn parse_seek_head_no_cues() {
        let (stream, _) = build_single(&[&[1], &[2], &[3]]);
        let info = parse_seek_head(header_prefix(&stream)).expect("Segment located");
        assert_eq!(info.cues_pos, None, "no SeekHead/Cues in an un-indexed file");
        assert_eq!(info.timestamp_scale, 1_000_000);
    }

    #[test]
    fn parse_seek_head_garbage_is_none() {
        assert_eq!(parse_seek_head(&[]), None);
        assert_eq!(parse_seek_head(&[0xFF; 32]), None);
        assert_eq!(parse_seek_head(&[0x1A, 0x45, 0xDF]), None, "truncated EBML id");
    }

    #[test]
    fn parse_cues_robust_to_bad_bytes() {
        assert!(parse_cues(&[], 0, 1_000_000).is_empty());
        assert!(parse_cues(id::CLUSTER, 0, 1_000_000).is_empty());
        let mut cues = Vec::new();
        ebml::write_id(&mut cues, id::CUES);
        ebml::write_size(&mut cues, 1000); // lies, no body follows
        assert!(parse_cues(&cues, 0, 1_000_000).is_empty());
    }

    #[test]
    fn resync_at_exact_cluster_offset() {
        let stream = build_cued_video(&[
            (0, true, &[1, 2, 3]),
            (40_000_000, true, &[4, 5]),
            (80_000_000, true, &[6, 7]),
        ]);
        let info = parse_seek_head(header_prefix(&stream)).unwrap();
        let cues_abs = (info.segment_data_start + info.cues_pos.unwrap()) as usize;
        let h = ebml::read_element_header(&stream, cues_abs).unwrap();
        let cues_bytes = &stream[cues_abs..h.data_end().unwrap().unwrap()];
        let index = parse_cues(cues_bytes, info.segment_data_start, info.timestamp_scale);
        let (want_time, seek_byte) = index[1];
        assert_eq!(want_time, 40_000_000);

        let mut r = MatroskaReader::new();
        r.push(header_prefix(&stream)).unwrap();
        assert!(r.tracks_ready(), "tracks discovered before seek");
        while r.next_frame().is_some() {} // drain the header probe (no frames yet)

        r.resync_streaming();
        r.push(&stream[seek_byte as usize..]).unwrap();
        let first = r.next_frame().expect("a frame after resync");
        assert_eq!(first.pts_ns, 40_000_000, "first post-seek frame at the cue timestamp");
        assert_eq!(first.data, vec![4, 5], "the 40 ms cluster's frame, bit-exact");
        assert_eq!(r.tracks().len(), 1, "the discovered track survived resync");
    }

    #[test]
    fn resync_from_garbage_offset_recovers_at_next_cluster() {
        let stream = build_cued_video(&[
            (0, true, &[1, 2, 3]),
            (40_000_000, true, &[9, 9, 9, 9]),
            (80_000_000, true, &[7, 7]),
        ]);
        let info = parse_seek_head(header_prefix(&stream)).unwrap();
        let cues_abs = (info.segment_data_start + info.cues_pos.unwrap()) as usize;
        let h = ebml::read_element_header(&stream, cues_abs).unwrap();
        let cues_bytes = &stream[cues_abs..h.data_end().unwrap().unwrap()];
        let index = parse_cues(cues_bytes, info.segment_data_start, info.timestamp_scale);
        let cue_byte = index[1].1 as usize;
        let garbage_start = cue_byte - 5;

        let mut r = MatroskaReader::new();
        r.push(header_prefix(&stream)).unwrap();
        while r.next_frame().is_some() {}
        r.resync_streaming();
        r.push(&stream[garbage_start..]).unwrap();

        let first = r.next_frame().expect("recovered a frame");
        assert_eq!(first.pts_ns, 40_000_000, "recovered at the next Cluster's timestamp");
        assert_eq!(first.data, vec![9, 9, 9, 9]);
    }

    #[test]
    fn resync_skips_false_positive_cluster_id() {
        let stream = build_cued_video(&[(0, true, &[1]), (40_000_000, true, &[5, 5, 5])]);
        let info = parse_seek_head(header_prefix(&stream)).unwrap();
        let cues_abs = (info.segment_data_start + info.cues_pos.unwrap()) as usize;
        let h = ebml::read_element_header(&stream, cues_abs).unwrap();
        let cues_bytes = &stream[cues_abs..h.data_end().unwrap().unwrap()];
        let index = parse_cues(cues_bytes, info.segment_data_start, info.timestamp_scale);
        let cluster40 = index[1].1 as usize;

        let mut r = MatroskaReader::new();
        r.push(header_prefix(&stream)).unwrap();
        while r.next_frame().is_some() {}
        r.resync_streaming();
        let mut fed = vec![0xDE, 0xAD];
        fed.extend_from_slice(id::CLUSTER); // false positive
        fed.push(0x00); // 0x00 as a size VINT is illegal (width > 8), not a valid header
        fed.extend_from_slice(&[0xBE, 0xEF]);
        fed.extend_from_slice(&stream[cluster40..]); // the real Cluster
        r.push(&fed).unwrap();

        let first = r.next_frame().expect("recovered past the false positive");
        assert_eq!(first.pts_ns, 40_000_000, "recovered at the real Cluster, not the fake ID");
        assert_eq!(first.data, vec![5, 5, 5]);
    }

    #[test]
    fn resync_scan_survives_split_cluster_id() {
        let stream = build_cued_video(&[(0, true, &[1]), (40_000_000, true, &[2, 2])]);
        let info = parse_seek_head(header_prefix(&stream)).unwrap();
        let cues_abs = (info.segment_data_start + info.cues_pos.unwrap()) as usize;
        let h = ebml::read_element_header(&stream, cues_abs).unwrap();
        let cues_bytes = &stream[cues_abs..h.data_end().unwrap().unwrap()];
        let index = parse_cues(cues_bytes, info.segment_data_start, info.timestamp_scale);
        let start = index[1].1 as usize - 7; // a few bytes before the cue

        let mut r = MatroskaReader::new();
        r.push(header_prefix(&stream)).unwrap();
        while r.next_frame().is_some() {}
        r.resync_streaming();
        for &b in &stream[start..] {
            r.push(&[b]).expect("byte-by-byte resync never errors");
        }
        let first = r.next_frame().expect("recovered a frame");
        assert_eq!(first.pts_ns, 40_000_000);
        assert_eq!(first.data, vec![2, 2]);
    }

    #[test]
    fn a_signed_integer_reads_as_twos_complement() {
        assert_eq!(read_int(&[]), 0);
        assert_eq!(read_int(&[0x07, 0xA1, 0x20]), 500_000);
        assert_eq!(read_int(&[0xFF, 0x0B, 0xDC]), -62_500);
        assert_eq!(read_int(&[0x80]), -128);
        assert_eq!(read_int(&[0xFF; 8]), -1);
    }

    #[test]
    fn held_counts_the_queue_as_frames_come_and_go() {
        let stream = build_cued_video(&[(0, true, &[1; 10]), (40_000_000, false, &[2; 20])]);
        let mut r = MatroskaReader::new();
        r.push(&stream).unwrap();
        let input = r.buf.len() - r.pos;
        assert_eq!(r.held(), input + 30);
        let first = r.next_frame().expect("a frame");
        assert_eq!(r.held(), input + 20);
        r.unread(first);
        assert_eq!(r.held(), input + 30);
        r.resync_streaming();
        assert_eq!(r.held(), 0);
    }

    #[test]
    fn a_scan_takes_a_cluster_id_without_its_timestamp_for_payload() {
        let stream = build_cued_video(&[(0, true, &[1]), (40_000_000, true, &[2, 2])]);
        let head = header_prefix(&stream);
        let mut r = MatroskaReader::new();
        r.push(head).unwrap();
        r.resync_streaming();
        // A Cluster ID and size in a payload, a SimpleBlock where its Timestamp should be.
        let mut fed =
            vec![0x55, 0x1F, 0x43, 0xB6, 0x75, 0x88, 0xA3, 0x86, 0x81, 0, 5, 0x80, 0xEE, 0xEE];
        fed.extend_from_slice(&stream[head.len()..]);
        r.push(&fed).unwrap();
        let first = r.next_frame().expect("the real Cluster");
        assert_eq!((first.pts_ns, first.data), (0, vec![1]));
        assert!(
            cluster_led(&[0x1F, 0x43, 0xB6, 0x75, 0x87, 0xBF, 0x84, 1, 2, 3, 4, 0xE7], 0).is_none()
        );
        let led = [0x1F, 0x43, 0xB6, 0x75, 0x89, 0xBF, 0x84, 1, 2, 3, 4, 0xE7, 0x81, 0];
        assert_eq!(cluster_led(&led, 0), Some(true), "a CRC-32 may come first");
    }

    #[test]
    fn a_resent_tracks_replaces_the_last() {
        let stream = build_cued_video(&[(0, true, &[1])]);
        let head = header_prefix(&stream);
        let tracks_at = head.windows(4).rposition(|w| w == id::TRACKS).expect("Tracks");
        let h = ebml::read_element_header(head, tracks_at).unwrap();
        let tracks = &head[tracks_at..h.data_end().unwrap().unwrap()];
        let mut r = MatroskaReader::new();
        r.push(head).unwrap();
        r.push(tracks).unwrap();
        assert_eq!(r.tracks().len(), 1, "no track listed twice");
    }

    #[test]
    fn an_audio_master_without_rate_or_channels_takes_the_defaults() {
        let t = parse_track_entry(&[0xD7, 0x81, 1, 0xE1, 0x80], &mut 0).unwrap();
        assert_eq!((t.sampling_frequency, t.channels), (8_000.0, 1));
        let t = parse_track_entry(&[0xD7, 0x81, 1, 0xE1, 0x83, 0x9F, 0x81, 6], &mut 0).unwrap();
        assert_eq!((t.sampling_frequency, t.channels), (8_000.0, 6));
        let t = parse_track_entry(&[0xD7, 0x81, 1], &mut 0).unwrap();
        assert_eq!((t.sampling_frequency, t.channels), (0.0, 0), "no Audio master states nothing");
    }

    /// A zlib stream of `n` zero octets in one fixed-Huffman block, 13 bits per 258 octets.
    fn zlib_zeros(n: usize) -> Vec<u8> {
        struct Bits {
            out: Vec<u8>,
            acc: u64,
            n: u32,
        }
        impl Bits {
            fn put(&mut self, v: u32, width: u32) {
                self.acc |= u64::from(v) << self.n;
                self.n += width;
                while self.n >= 8 {
                    self.out.push(self.acc as u8);
                    (self.acc, self.n) = (self.acc >> 8, self.n - 8);
                }
            }
            /// Huffman codes go in most significant bit first.
            fn code(&mut self, c: u32, width: u32) {
                self.put(c.reverse_bits() >> (32 - width), width);
            }
        }
        let mut b = Bits { out: vec![0x78, 0x01], acc: 0, n: 0 };
        b.put(1, 1);
        b.put(1, 2);
        let mut left = n;
        if left > 0 {
            b.code(0x30, 8); // literal 0
            left -= 1;
        }
        while left >= 258 {
            b.code(0xC5, 8); // length 258
            b.code(0, 5); // distance 1
            left -= 258;
        }
        for _ in 0..left {
            b.code(0x30, 8);
        }
        b.code(0, 7); // end of block
        if b.n > 0 {
            b.put(0, 8 - b.n);
        }
        let mut out = b.out;
        out.extend_from_slice(&((n as u32 % 65_521) << 16 | 1).to_be_bytes());
        out
    }

    fn zlib_private_entry(number: u8, private: &[u8]) -> Vec<u8> {
        let mut entry = vec![0xD7, 0x81, number];
        ebml::write_binary(&mut entry, id::CODEC_PRIVATE, private);
        let mut enc = Vec::new();
        ebml::write_uint(&mut enc, id::CONTENT_ENCODING_SCOPE, 2);
        let mut comp = Vec::new();
        ebml::write_uint(&mut comp, id::CONTENT_COMP_ALGO, 0);
        ebml::write_binary(&mut enc, id::CONTENT_COMPRESSION, &comp);
        let mut encs = Vec::new();
        ebml::write_binary(&mut encs, id::CONTENT_ENCODING, &enc);
        ebml::write_binary(&mut entry, id::CONTENT_ENCODINGS, &encs);
        entry
    }

    #[test]
    fn a_zlib_bomb_in_codec_private_is_dropped_at_the_cap() {
        let small = zlib_private_entry(1, &zlib_zeros(70_000));
        assert!(small.len() < 1_000, "a few hundred octets inflate to 70 KB");
        let t = parse_track_entry(&small, &mut 0).unwrap();
        assert_eq!(t.codec_private, vec![0u8; 70_000]);
        let bomb = zlib_private_entry(1, &zlib_zeros(encoding::MAX_PRIVATE + 1));
        assert!(bomb.len() < 8_000, "a few KB inflate past the per-track cap");
        let mut undone = 0;
        let t = parse_track_entry(&bomb, &mut undone).unwrap();
        assert!(t.codec_private.is_empty() && undone == 0, "dropped, the track stays");
        let near = zlib_zeros(encoding::MAX_PRIVATE);
        let mut undone = encoding::MAX_PRIVATE_TOTAL - encoding::MAX_PRIVATE + 1;
        let t = parse_track_entry(&zlib_private_entry(1, &near), &mut undone).unwrap();
        assert!(t.codec_private.is_empty(), "past the budget of the whole Tracks element");
        let mut tracks = Vec::new();
        for n in 0..=(encoding::MAX_PRIVATE_TOTAL / encoding::MAX_PRIVATE) as u8 {
            ebml::write_binary(&mut tracks, id::TRACK_ENTRY, &zlib_private_entry(n + 1, &near));
        }
        let parsed = parse_tracks(&tracks).unwrap();
        let kept = parsed.iter().filter(|t| !t.codec_private.is_empty()).count();
        assert_eq!(kept, encoding::MAX_PRIVATE_TOTAL / encoding::MAX_PRIVATE, "budget spent");
    }

    #[test]
    fn a_repeated_track_number_is_one_track_and_an_empty_tracks_is_refused() {
        let mut tracks = Vec::new();
        ebml::write_binary(&mut tracks, id::TRACK_ENTRY, &[0xD7, 0x81, 1, 0xE1, 0x80]);
        ebml::write_binary(&mut tracks, id::TRACK_ENTRY, &[0xD7, 0x81, 1]);
        ebml::write_binary(&mut tracks, id::TRACK_ENTRY, &[0xD7, 0x81, 2]);
        let parsed = parse_tracks(&tracks).unwrap();
        let numbers: Vec<u64> = parsed.iter().map(|t| t.track_number).collect();
        assert_eq!(numbers, [1, 2], "the first entry of a number is the track");
        assert_eq!(parsed[0].channels, 1, "the first entry's fields");
        let mut head = vec![0x18, 0x53, 0x80, 0x67, 0xFF];
        ebml::write_binary(&mut head, id::TRACKS, &[]);
        let mut r = MatroskaReader::new();
        assert!(r.push(&head).is_err(), "no TrackEntry is no header");
        assert!(!r.tracks_ready());
    }

    #[test]
    fn a_sampling_frequency_that_is_not_a_rate_keeps_the_default() {
        let audio = |f: f64| {
            let mut entry = vec![0xD7, 0x81, 1, 0xE1, 0x8A, 0xB5, 0x88];
            entry.extend_from_slice(&f.to_bits().to_be_bytes());
            parse_track_entry(&entry, &mut 0).unwrap().sampling_frequency
        };
        assert_eq!(audio(44_100.0), 44_100.0);
        assert_eq!(audio(f64::NAN), 8_000.0);
        assert_eq!(audio(-1.0), 8_000.0);
        assert_eq!(audio(0.0), 8_000.0);
        assert_eq!(audio(f64::INFINITY), 8_000.0);
    }

    #[test]
    fn a_tracks_element_past_the_track_cap_is_refused_as_it_is_counted() {
        let mut tracks = Vec::new();
        for n in 1..=MAX_TRACKS as u64 {
            let mut entry = Vec::new();
            ebml::write_uint(&mut entry, id::TRACK_NUMBER, n);
            ebml::write_binary(&mut tracks, id::TRACK_ENTRY, &entry);
        }
        assert_eq!(parse_tracks(&tracks).unwrap().len(), MAX_TRACKS);
        // One past the cap, an entry too short to parse, which the cap refuses first.
        tracks.extend_from_slice(&[0xAE, 0x81, 0xD7]);
        assert!(matches!(parse_tracks(&tracks), Err(ReadError::Malformed(_))));
    }
}
