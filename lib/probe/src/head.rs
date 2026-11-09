//! The bytes a demuxer is built from. The `*_from` forms walk a window the caller already read,
//! the `*At` machines ask the caller for the bytes they need, so a growing or remote source is
//! never read past what it has.

// App-side setup before any pipeline, one head buffer per open.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use profluens_core::fetch::{Fetch, FileError, Need, Step, drive_file};

/// Matroska ids, RFC 9559 section 5.1, as the 32 bit values of their octets.
const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const CLUSTER: u32 = 0x1F43_B675;
const TRACKS: u32 = 0x1654_AE6B;
const ATTACHMENTS: u32 = 0x1941_A469;
const TAGS: u32 = 0x1254_C367;
const CHAPTERS: u32 = 0x1043_A770;
/// Top level elements walked for the first Cluster.
const MKV_MAX_TOP: usize = 4096;
const MKV_HEAD_MAX: usize = 8 << 20;
const AVI_HEAD_MAX: usize = 16 << 20;
const MP4_WINDOW: usize = 64 << 10;

#[derive(Debug)]
pub enum HeadError {
    Io(io::Error),
    NoCluster,
    NoMovi,
    Mp4(Mp4HeadError),
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeadError::Io(e) => write!(f, "read: {e}"),
            HeadError::NoCluster => f.write_str(
                "no Cluster in the bytes read, not a streamable Matroska file or its head is not read yet",
            ),
            HeadError::NoMovi => f.write_str(
                "no 'movi' list in the bytes read, not an AVI file or its head is not read yet",
            ),
            HeadError::Mp4(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for HeadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HeadError::Io(e) => Some(e),
            HeadError::Mp4(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for HeadError {
    fn from(e: io::Error) -> Self {
        HeadError::Io(e)
    }
}

fn read_file_at(file: &mut File, off: u64, want: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(off))?;
    let mut buf = Vec::with_capacity(want);
    file.take(want as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn read_prefix(path: &str, want: usize) -> io::Result<Vec<u8>> {
    read_file_at(&mut File::open(path)?, 0, want)
}

pub fn mkv_header_prefix(path: &str) -> Result<Vec<u8>, HeadError> {
    mkv_header_from(&read_prefix(path, MKV_HEAD_MAX)?)
}

/// `window` up to the first Cluster, walking the top level elements by their sizes. Once Tracks
/// is read, the Attachments or an element the walk cannot step past in the window end the head
/// before them, attached fonts are read through the SeekHead by what wants them. A head of a cut
/// window is then always the start of the whole one's.
pub fn mkv_header_from(window: &[u8]) -> Result<Vec<u8>, HeadError> {
    let (ebml, at) = ebml_header(window, 0).ok_or(HeadError::NoCluster)?;
    let at = match (ebml.0, ebml.1) {
        (EBML, Some(size)) => at.checked_add(size).ok_or(HeadError::NoCluster)?,
        _ => return Err(HeadError::NoCluster),
    };
    let (seg, mut at) = ebml_header(window, at).ok_or(HeadError::NoCluster)?;
    if seg.0 != SEGMENT {
        return Err(HeadError::NoCluster);
    }
    let mut tracks = false;
    for _ in 0..MKV_MAX_TOP {
        let Some(((id, size), data)) = ebml_header(window, at) else {
            break;
        };
        if id == CLUSTER || (tracks && id == ATTACHMENTS) {
            return Ok(window[..at as usize].to_vec());
        }
        let Some(end) =
            size.and_then(|s| data.checked_add(s)).filter(|&e| e <= window.len() as u64)
        else {
            break;
        };
        tracks |= id == TRACKS;
        at = end;
    }
    match tracks {
        true => Ok(window[..at.min(window.len() as u64) as usize].to_vec()),
        false => Err(HeadError::NoCluster),
    }
}

/// The id and size of the element at `at`, None for the unknown size, and where its data
/// starts. None when the header is cut or no EBML header (RFC 8794 section 4).
fn ebml_header(b: &[u8], at: u64) -> Option<((u32, Option<u64>), u64)> {
    let at = usize::try_from(at).ok()?;
    let first = *b.get(at)?;
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return None;
    }
    let id = b.get(at..at + id_len)?.iter().fold(0u32, |v, &x| v << 8 | u32::from(x));
    let s = at + id_len;
    let lead = *b.get(s)?;
    let len = lead.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let raw = b.get(s..s + len)?;
    let mask = if len == 8 { 0 } else { 0xFFu8 >> len };
    let size = raw[1..].iter().fold(u64::from(lead & mask), |v, &x| v << 8 | u64::from(x));
    // all ones is the unknown size
    let unknown = size == (1u64 << (7 * len)) - 1;
    Some(((id, (!unknown).then_some(size)), (s + len) as u64))
}

/// A Matroska file's head as [`MkvHeadAt`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MkvHead {
    /// The bytes before the first Cluster. Short of it at Attachments or an element past the
    /// cap that follow Tracks, attached fonts are read through the SeekHead by what wants them.
    pub head: Vec<u8>,
    pub first_cluster: u64,
    /// Where the walk passed the Attachments, for a head cut before them whose SeekHead names
    /// none.
    pub attachments: Option<u64>,
    /// Where the walk passed the Tags, for Tags behind such a cut.
    pub tags: Option<u64>,
    /// Where the walk passed the Chapters, likewise.
    pub chapters: Option<u64>,
}

/// `TooLarge` is an element ending past the cap before Tracks is read, `TooMany` no Cluster among
/// the first top level elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MkvHeadError {
    NotMatroska,
    TooLarge { at: u64, max: u64 },
    TooMany { at: u64 },
    Malformed { at: u64 },
}

impl fmt::Display for MkvHeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MkvHeadError::NotMatroska => f.write_str("no EBML header, not a Matroska file"),
            MkvHeadError::TooLarge { at, max } => {
                write!(f, "the element at byte {at} ends past the {max} a head may hold")
            }
            MkvHeadError::TooMany { at } => {
                write!(f, "no Cluster in the {MKV_MAX_TOP} top level elements before byte {at}")
            }
            MkvHeadError::Malformed { at } => write!(f, "no element header at byte {at}"),
        }
    }
}

impl std::error::Error for MkvHeadError {}

/// A 4 octet ID and an 8 octet size, the most an element header takes.
const EL_HEAD: usize = 12;

/// The read in flight of [`MkvHeadAt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MkvState {
    Start,
    /// The EBML header's head.
    Ebml,
    /// The Segment's head at `at`.
    Segment {
        at: u64,
    },
    /// A top level element's head at `at`.
    Top {
        at: u64,
    },
    /// The head, the bytes before the first Cluster or the cut.
    Head {
        first_cluster: u64,
    },
}

/// The head of a Matroska file. The top level elements are walked by their headers alone, so
/// attached fonts are never read. Once Tracks is read, the Attachments or an element ending past
/// `max` end the head before them and the walk goes on past for the first Cluster. An element
/// past `max` before Tracks is refused. A
/// walk that breaks past such a cut takes the cut as the first Cluster, the demuxer finds the real
/// one by its id.
#[derive(Debug)]
pub struct MkvHeadAt {
    max: u64,
    state: MkvState,
    hd: [u8; EL_HEAD],
    head: Vec<u8>,
    /// The file's first bytes read already, walked without reads and kept as the head.
    window: Vec<u8>,
    /// Where the head read in flight starts, past the window's bytes.
    head_from: usize,
    got: usize,
    tracks: bool,
    cut: Option<u64>,
    attachments: Option<u64>,
    tags: Option<u64>,
    chapters: Option<u64>,
    /// Top level elements walked.
    tops: usize,
}

impl MkvHeadAt {
    pub fn new(max: u64) -> Self {
        MkvHeadAt::with_window(max, Vec::new())
    }

    /// Walks `window`, the file's first bytes, without reads. A head inside it is the window
    /// cut to it, one past it reads only the bytes past the window. Consumed by the first run.
    pub fn with_window(max: u64, window: Vec<u8>) -> Self {
        MkvHeadAt {
            max,
            state: MkvState::Start,
            hd: [0; EL_HEAD],
            head: Vec::new(),
            window,
            head_from: 0,
            got: 0,
            tracks: false,
            cut: None,
            attachments: None,
            tags: None,
            chapters: None,
            tops: 0,
        }
    }

    /// The id, the size and where the data starts of the element whose head was read at `at`.
    fn element(&self, at: u64) -> Result<(u32, Option<u64>, u64), MkvHeadError> {
        let ((id, size), data) =
            ebml_header(&self.hd[..self.got], 0).ok_or(MkvHeadError::Malformed { at })?;
        Ok((id, size, at.checked_add(data).ok_or(MkvHeadError::Malformed { at })?))
    }

    /// An element head the window holds, answered from it. One cut at its end is read.
    fn windowed(&mut self, need: Need) -> bool {
        let from = usize::try_from(need.at).ok();
        let Some(have) = from.and_then(|at| self.window.get(at..)) else { return false };
        let n = have.len().min(EL_HEAD);
        if ebml_header(&have[..n], 0).is_none() {
            return false;
        }
        self.hd[..n].copy_from_slice(&have[..n]);
        self.got = n;
        true
    }

    fn ask(&mut self, state: MkvState, at: u64) -> Step<MkvHead> {
        self.state = state;
        Step::Need(Need { at, len: EL_HEAD })
    }

    /// The next top level element at `at`, or the head once the walk ends.
    fn top(&mut self, at: u64) -> Result<Step<MkvHead>, MkvHeadError> {
        if self.tops == MKV_MAX_TOP {
            return self.finish(None, at);
        }
        self.tops += 1;
        Ok(self.ask(MkvState::Top { at }, at))
    }

    /// Asks for the head once the walk ended at `at`, `bounds` the first Cluster and the cut.
    fn finish(
        &mut self,
        bounds: Option<(u64, u64)>,
        at: u64,
    ) -> Result<Step<MkvHead>, MkvHeadError> {
        let max = self.max;
        let (first_cluster, end) =
            bounds.or(self.cut.map(|c| (c, c))).ok_or(MkvHeadError::TooMany { at })?;
        if end > max {
            return Err(MkvHeadError::TooLarge { at: 0, max });
        }
        let len = usize::try_from(end).map_err(|_| MkvHeadError::TooLarge { at: 0, max })?;
        self.state = MkvState::Head { first_cluster };
        if self.window.len() >= len {
            let mut head = std::mem::take(&mut self.window);
            head.truncate(len);
            self.state = MkvState::Start;
            let (attachments, tags, chapters) = (self.attachments, self.tags, self.chapters);
            return Ok(Step::Done(MkvHead { head, first_cluster, attachments, tags, chapters }));
        }
        self.head = std::mem::take(&mut self.window);
        self.head_from = self.head.len();
        self.head.resize(len, 0);
        Ok(Step::Need(Need { at: self.head_from as u64, len: len - self.head_from }))
    }

    fn step(&mut self) -> Result<Step<MkvHead>, MkvHeadError> {
        match self.state {
            MkvState::Start => {
                (self.tracks, self.cut, self.tops) = (false, None, 0);
                (self.attachments, self.tags, self.chapters) = (None, None, None);
                Ok(self.ask(MkvState::Ebml, 0))
            }
            MkvState::Ebml => {
                let (id, size, data) = self.element(0)?;
                if id != EBML {
                    return Err(MkvHeadError::NotMatroska);
                }
                let seg_at = size
                    .and_then(|s| data.checked_add(s))
                    .ok_or(MkvHeadError::Malformed { at: 0 })?;
                Ok(self.ask(MkvState::Segment { at: seg_at }, seg_at))
            }
            MkvState::Segment { at } => {
                let (id, _, data) = self.element(at)?;
                if id != SEGMENT {
                    return Err(MkvHeadError::Malformed { at });
                }
                self.top(data)
            }
            MkvState::Top { at } => {
                let (id, size, data) = match self.element(at) {
                    Ok(h) => h,
                    Err(_) if self.cut.is_some() => return self.finish(None, at),
                    Err(e) => return Err(e),
                };
                if id == CLUSTER {
                    return self.finish(Some((at, self.cut.unwrap_or(at))), at);
                }
                let end = match size.and_then(|s| data.checked_add(s)) {
                    Some(end) => end,
                    None if self.cut.is_some() => return self.finish(None, at),
                    None => return Err(MkvHeadError::Malformed { at }),
                };
                match id {
                    ATTACHMENTS => self.attachments = self.attachments.or(Some(at)),
                    TAGS => self.tags = self.tags.or(Some(at)),
                    CHAPTERS => self.chapters = self.chapters.or(Some(at)),
                    _ => {}
                }
                let fonts = self.tracks && id == ATTACHMENTS;
                if self.cut.is_none() && (fonts || end > self.max) {
                    if !self.tracks {
                        return Err(MkvHeadError::TooLarge { at, max: self.max });
                    }
                    self.cut = Some(at);
                }
                self.tracks |= id == TRACKS;
                self.top(end)
            }
            MkvState::Head { first_cluster } => {
                if self.head_from + self.got != self.head.len() {
                    return Err(MkvHeadError::Malformed { at: (self.head_from + self.got) as u64 });
                }
                (self.state, self.head_from) = (MkvState::Start, 0);
                let (attachments, tags, chapters) = (self.attachments, self.tags, self.chapters);
                let head = std::mem::take(&mut self.head);
                Ok(Step::Done(MkvHead { head, first_cluster, attachments, tags, chapters }))
            }
        }
    }
}

impl Fetch for MkvHeadAt {
    type Out = MkvHead;
    type Err = MkvHeadError;

    fn poll(&mut self) -> Result<Step<MkvHead>, MkvHeadError> {
        loop {
            let step = self.step();
            match step {
                Ok(Step::Need(need)) if self.windowed(need) => continue,
                Err(_) => (self.state, self.head_from) = (MkvState::Start, 0),
                _ => {}
            }
            return step;
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        match self.state {
            MkvState::Head { .. } => &mut self.head[self.head_from..],
            _ => &mut self.hd,
        }
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.buf().len());
    }
}

pub fn avi_head(path: &str) -> Result<Vec<u8>, HeadError> {
    avi_head_from(&read_prefix(path, AVI_HEAD_MAX)?)
}

/// `window` through its `movi` four-CC, so `pf-avi` finds where the chunks start.
pub fn avi_head_from(window: &[u8]) -> Result<Vec<u8>, HeadError> {
    let at = window.windows(4).position(|w| w == b"movi").ok_or(HeadError::NoMovi)?;
    Ok(window[..at + 4].to_vec())
}

/// Read at a time while looking for the `movi` list.
const AVI_STEP: usize = 256 << 10;

/// No `movi` in the first `read` bytes, the whole file or the cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AviHeadError {
    NoMovi { read: u64 },
}

impl fmt::Display for AviHeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AviHeadError::NoMovi { read } => {
                write!(f, "no 'movi' list in the first {read} bytes, not an AVI file")
            }
        }
    }
}

impl std::error::Error for AviHeadError {}

/// [`avi_head_from`] read a step at a time until the `movi` four-CC shows or `max` bytes are
/// read.
#[derive(Debug)]
pub struct AviHeadAt {
    max: usize,
    head: Vec<u8>,
    /// Where the step in flight starts, None before the first.
    step: Option<usize>,
    /// The head holds a window not looked at yet.
    seeded: bool,
}

impl AviHeadAt {
    pub fn new(max: usize) -> Self {
        AviHeadAt::with_window(max, Vec::new())
    }

    /// Looks in `window`, the file's first bytes, before any read, and reads on past it.
    /// Consumed by the first run.
    pub fn with_window(max: usize, mut window: Vec<u8>) -> Self {
        window.truncate(max);
        let seeded = !window.is_empty();
        AviHeadAt { max, head: window, step: None, seeded }
    }

    fn fail(&mut self) -> AviHeadError {
        let read = self.head.len() as u64;
        (self.step, self.head, self.seeded) = (None, Vec::new(), false);
        AviHeadError::NoMovi { read }
    }
}

impl Fetch for AviHeadAt {
    type Out = Vec<u8>;
    type Err = AviHeadError;

    fn poll(&mut self) -> Result<Step<Vec<u8>>, AviHeadError> {
        if std::mem::take(&mut self.seeded)
            && let Some(p) = self.head.windows(4).position(|w| w == b"movi")
        {
            self.head.truncate(p + 4);
            return Ok(Step::Done(std::mem::take(&mut self.head)));
        }
        if let Some(at) = self.step.take() {
            // only the new bytes and the 3 before them can hold a match not looked at yet
            let from = at.saturating_sub(3);
            if let Some(p) = self.head[from..].windows(4).position(|w| w == b"movi") {
                self.head.truncate(from + p + 4);
                return Ok(Step::Done(std::mem::take(&mut self.head)));
            }
            if self.head.len() < at + AVI_STEP.min(self.max - at) {
                return Err(self.fail());
            }
        }
        if self.head.len() >= self.max {
            return Err(self.fail());
        }
        let at = self.head.len();
        let len = AVI_STEP.min(self.max - at);
        self.head.resize(at + len, 0);
        self.step = Some(at);
        Ok(Step::Need(Need { at: at as u64, len }))
    }

    fn buf(&mut self) -> &mut [u8] {
        let at = self.step.unwrap_or(self.head.len()).min(self.head.len());
        &mut self.head[at..]
    }

    fn filled(&mut self, n: usize) {
        let at = self.step.unwrap_or(self.head.len()).min(self.head.len());
        self.head.truncate(at + n.min(self.head.len() - at));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mp4HeadError {
    NoMoov,
    TooLarge { at: u64, size: u64, max: u64 },
    Truncated { at: u64, end: u64, len: u64 },
    Malformed { at: u64 },
}

impl fmt::Display for Mp4HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mp4HeadError::NoMoov => f.write_str("no moov box in the file"),
            Mp4HeadError::TooLarge { at, size, max } => {
                write!(
                    f,
                    "the box at byte {at} is {size} bytes, more than the {max} a head may hold"
                )
            }
            Mp4HeadError::Truncated { at, end, len } => {
                write!(f, "the box at byte {at} runs to {end}, past the end of the file at {len}")
            }
            Mp4HeadError::Malformed { at } => {
                write!(f, "the box at byte {at} is smaller than its header")
            }
        }
    }
}

impl std::error::Error for Mp4HeadError {}

/// More than any `moov` of a file that plays, and the most one read fetches.
pub const MAX_MOOV: u64 = 64 << 20;
/// Read behind the `mdat` in one go, so a trailing `moov` usually costs one request.
const TAIL_FIRST: usize = 1 << 20;
const MAX_BOXES: u32 = 256;
const MAX_MFRA: u64 = 16 << 20;
/// Read past what a step needs, so the free and skip boxes muxers put between the `moov` and
/// the `mdat` come in the same read.
const PAST_END: u64 = 4096;

/// What [`Mp4HeadAt`] does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mp4State {
    /// The box header at `at`, the `n`th.
    Boxes { at: u64, n: u32 },
    /// The bytes up to the `mdat`.
    Prefix,
    /// The `moov` behind the `mdat`.
    Moov,
}

/// The bytes up to the `mdat`, with the `moov` appended when it lies behind it, ISO/IEC 14496-12
/// section 8.2.1. Reads what `window`, the file's first bytes, does not hold, at least
/// [`TAIL_FIRST`] at a time into one reused tail.
#[derive(Debug)]
pub struct Mp4HeadAt<'w> {
    window: &'w [u8],
    walk: Mp4Walk,
}

impl<'w> Mp4HeadAt<'w> {
    /// `len` is the file's length.
    pub fn new(window: &'w [u8], len: u64) -> Self {
        Mp4HeadAt { window, walk: Mp4Walk::new(len) }
    }
}

impl Fetch for Mp4HeadAt<'_> {
    type Out = Vec<u8>;
    type Err = Mp4HeadError;

    fn poll(&mut self) -> Result<Step<Vec<u8>>, Mp4HeadError> {
        self.walk.poll(self.window)
    }

    fn buf(&mut self) -> &mut [u8] {
        self.walk.buf()
    }

    fn filled(&mut self, n: usize) {
        self.walk.filled(n);
    }
}

/// [`Mp4HeadAt`] owning its window, to sit inside a machine that owns its bytes.
#[derive(Debug)]
pub struct Mp4HeadOwned {
    window: Vec<u8>,
    walk: Mp4Walk,
}

impl Mp4HeadOwned {
    /// `len` is the file's length.
    pub fn new(window: Vec<u8>, len: u64) -> Self {
        Mp4HeadOwned { window, walk: Mp4Walk::new(len) }
    }

    /// The window back, for the next read into it.
    pub fn into_window(self) -> Vec<u8> {
        self.window
    }
}

impl Fetch for Mp4HeadOwned {
    type Out = Vec<u8>;
    type Err = Mp4HeadError;

    fn poll(&mut self) -> Result<Step<Vec<u8>>, Mp4HeadError> {
        self.walk.poll(&self.window)
    }

    fn buf(&mut self) -> &mut [u8] {
        self.walk.buf()
    }

    fn filled(&mut self, n: usize) {
        self.walk.filled(n);
    }
}

/// The walk of [`Mp4HeadAt`] and [`Mp4HeadOwned`], the window handed in at each poll.
#[derive(Debug)]
struct Mp4Walk {
    len: u64,
    state: Mp4State,
    /// File bytes from `tail_at`, the last read and the reads that grew it.
    tail: Vec<u8>,
    tail_at: u64,
    /// The read in flight, from its byte to the end of what the step needs.
    reading: Option<(u64, u64)>,
    /// Where in the tail the read in flight lands, past what it holds when it grows it.
    fill: usize,
    got: usize,
    mdat: Option<u64>,
    moov: Option<(u64, u64)>,
    head: Vec<u8>,
}

impl Mp4Walk {
    fn new(len: u64) -> Self {
        Mp4Walk {
            len,
            state: Mp4State::Boxes { at: 0, n: 0 },
            tail: Vec::new(),
            tail_at: 0,
            reading: None,
            fill: 0,
            got: 0,
            mdat: None,
            moov: None,
            head: Vec::new(),
        }
    }

    /// The read that brings `[at, end)` into the window and the tail, None when they hold it.
    fn ask(&mut self, window: &[u8], at: u64, end: u64) -> Option<Need> {
        let wlen = window.len() as u64;
        if end <= wlen {
            return None;
        }
        let p = at.max(wlen);
        let tail_end = self.tail_at + self.tail.len() as u64;
        if self.tail_at <= p && end <= tail_end {
            return None;
        }
        // Every byte before the mdat is the prefix's, so a read there runs on from the window or
        // the tail and the prefix is never read twice or after a read behind it.
        let prefix = self.mdat.is_none() && end <= MAX_MOOV;
        let grows = !self.tail.is_empty() && self.tail_at <= p && (p <= tail_end || prefix);
        let (from, fill) = match (grows, prefix) {
            (true, true) => (tail_end, self.tail.len()),
            (true, false) => {
                // Behind the prefix only `p` on is asked again. Kept whole, box headers that
                // each end past the last read would hold every box in the tail.
                self.tail.drain(..(p - self.tail_at) as usize);
                self.tail_at = p;
                (tail_end, self.tail.len())
            }
            (false, true) => (wlen, 0),
            (false, false) => (p, 0),
        };
        let to = end.saturating_add(PAST_END).max(from.saturating_add(TAIL_FIRST as u64));
        let len = (to.min(self.len) - from) as usize;
        if fill == 0 {
            self.tail_at = from;
            self.tail.clear();
        }
        self.tail.resize(fill + len, 0);
        (self.reading, self.fill) = (Some((from, end)), fill);
        Some(Need { at: from, len })
    }

    fn buf(&mut self) -> &mut [u8] {
        &mut self.tail[self.fill..]
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.tail.len() - self.fill);
    }

    /// `[at, end)` as the window and the tail hold it, in two pieces.
    fn held<'a>(&'a self, window: &'a [u8], at: u64, end: u64) -> (&'a [u8], &'a [u8]) {
        let wlen = window.len() as u64;
        let w = window.get(at as usize..end.min(wlen) as usize).unwrap_or(&[]);
        let p = at.max(wlen);
        if p >= end {
            return (w, &[]);
        }
        let from = (p - self.tail_at) as usize;
        (w, self.tail.get(from..from + (end - p) as usize).unwrap_or(&[]))
    }

    /// Appends `[at, end)` to the head.
    fn take(&mut self, window: &[u8], at: u64, end: u64) {
        let mut head = std::mem::take(&mut self.head);
        let (w, t) = self.held(window, at, end);
        head.extend_from_slice(w);
        head.extend_from_slice(t);
        self.head = head;
    }

    fn reset(&mut self) {
        (self.state, self.reading, self.mdat, self.moov) =
            (Mp4State::Boxes { at: 0, n: 0 }, None, None, None);
        (self.tail_at, self.fill, self.head) = (0, 0, Vec::new());
        self.tail.clear();
    }

    fn poll(&mut self, window: &[u8]) -> Result<Step<Vec<u8>>, Mp4HeadError> {
        let step = self.step(window);
        if matches!(step, Ok(Step::Done(_)) | Err(_)) {
            self.reset();
        }
        step
    }

    fn step(&mut self, window: &[u8]) -> Result<Step<Vec<u8>>, Mp4HeadError> {
        if let Some((p, end)) = self.reading.take() {
            if (self.got as u64) < end - p {
                return Err(Mp4HeadError::Truncated { at: p, end, len: p + self.got as u64 });
            }
            self.tail.truncate(self.fill + self.got);
        }
        let len = self.len;
        loop {
            match self.state {
                Mp4State::Boxes { at, n } => {
                    if n == MAX_BOXES || at + 8 > len {
                        self.state = Mp4State::Prefix;
                        continue;
                    }
                    let hdr_end = at + 16.min(len - at);
                    if let Some(need) = self.ask(window, at, hdr_end) {
                        return Ok(Step::Need(need));
                    }
                    let mut b = [0u8; 16];
                    let (w, t) = self.held(window, at, hdr_end);
                    b[..w.len()].copy_from_slice(w);
                    b[w.len()..w.len() + t.len()].copy_from_slice(t);
                    let hdr = &b[..w.len() + t.len()];
                    let size = u64::from(u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]));
                    // Size 0 runs to the end, size 1 is a 64-bit largesize after the header,
                    // section 4.2.
                    let (header, size) = match size {
                        0 => (8, len - at),
                        1 => match hdr.get(8..16) {
                            Some(&[a, b, c, d, e, f, g, h]) => {
                                (16, u64::from_be_bytes([a, b, c, d, e, f, g, h]))
                            }
                            _ => return Err(Mp4HeadError::Truncated { at, end: at + 16, len }),
                        },
                        n => (8, n),
                    };
                    if size < header {
                        return Err(Mp4HeadError::Malformed { at });
                    }
                    let Some(end) = at.checked_add(size).filter(|end| *end <= len) else {
                        let end = at.saturating_add(size);
                        return Err(Mp4HeadError::Truncated { at, end, len });
                    };
                    match &hdr[4..8] {
                        b"mdat" if self.mdat.is_none() => self.mdat = Some(at),
                        b"moov" if size > MAX_MOOV => {
                            return Err(Mp4HeadError::TooLarge { at, size, max: MAX_MOOV });
                        }
                        b"moov" => self.moov = Some((at, size)),
                        _ => {}
                    }
                    self.state = match self.mdat.is_some() && self.moov.is_some() {
                        true => Mp4State::Prefix,
                        false => Mp4State::Boxes { at: end, n: n + 1 },
                    };
                }
                Mp4State::Prefix => {
                    let Some((moov_at, moov_len)) = self.moov else {
                        return Err(Mp4HeadError::NoMoov);
                    };
                    let prefix = self.mdat.unwrap_or(len);
                    if prefix > MAX_MOOV {
                        return Err(Mp4HeadError::TooLarge { at: 0, size: prefix, max: MAX_MOOV });
                    }
                    if let Some(need) = self.ask(window, 0, prefix) {
                        return Ok(Step::Need(need));
                    }
                    let behind = if moov_at >= prefix { moov_len } else { 0 };
                    self.head = Vec::with_capacity((prefix + behind) as usize);
                    self.take(window, 0, prefix);
                    if behind == 0 {
                        return Ok(Step::Done(std::mem::take(&mut self.head)));
                    }
                    self.state = Mp4State::Moov;
                }
                Mp4State::Moov => {
                    let (at, size) = self.moov.unwrap_or((0, 0));
                    if let Some(need) = self.ask(window, at, at + size) {
                        return Ok(Step::Need(need));
                    }
                    self.take(window, at, at + size);
                    return Ok(Step::Done(std::mem::take(&mut self.head)));
                }
            }
        }
    }
}

/// The size of the `mfra` a file ends in, from the `mfro` in its last 16 bytes, section 8.8.11.
pub fn mfra_size(tail: &[u8]) -> Option<u64> {
    let mfro = tail.get(tail.len().checked_sub(16)?..)?;
    if mfro[..8] != [0, 0, 0, 16, b'm', b'f', b'r', b'o'] {
        return None;
    }
    Some(u64::from(u32::from_be_bytes([mfro[12], mfro[13], mfro[14], mfro[15]])))
}

/// The read in flight of [`Mp4MfraAt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MfraState {
    Start,
    /// The last 16 bytes, the `mfro`.
    Mfro,
    /// The `mfra` it names.
    Mfra,
}

/// The `mfra` a fragmented MP4 of `len` bytes ends in, section 8.8.9, empty when there is none.
/// The last 16 bytes are read, then the `mfra` they name.
#[derive(Debug)]
pub struct Mp4MfraAt {
    len: u64,
    state: MfraState,
    mfro: [u8; 16],
    mfra: Vec<u8>,
    got: usize,
}

impl Mp4MfraAt {
    pub fn new(len: u64) -> Self {
        Mp4MfraAt { len, state: MfraState::Start, mfro: [0; 16], mfra: Vec::new(), got: 0 }
    }
}

impl Fetch for Mp4MfraAt {
    type Out = Vec<u8>;
    type Err = std::convert::Infallible;

    fn poll(&mut self) -> Result<Step<Vec<u8>>, Self::Err> {
        let len = self.len;
        match self.state {
            MfraState::Start => match len.checked_sub(16) {
                Some(at) => {
                    self.state = MfraState::Mfro;
                    Ok(Step::Need(Need { at, len: 16 }))
                }
                None => Ok(Step::Done(Vec::new())),
            },
            MfraState::Mfro => {
                self.state = MfraState::Start;
                let size = mfra_size(&self.mfro[..self.got]);
                let Some(size) = size.filter(|s| (16..=MAX_MFRA.min(len)).contains(s)) else {
                    return Ok(Step::Done(Vec::new()));
                };
                self.mfra = vec![0; size as usize];
                self.state = MfraState::Mfra;
                Ok(Step::Need(Need { at: len - size, len: size as usize }))
            }
            MfraState::Mfra => {
                self.state = MfraState::Start;
                self.mfra.truncate(self.got);
                Ok(Step::Done(std::mem::take(&mut self.mfra)))
            }
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        match self.state {
            MfraState::Mfra => &mut self.mfra,
            _ => &mut self.mfro,
        }
    }

    fn filled(&mut self, n: usize) {
        self.got = n.min(self.buf().len());
    }
}

pub fn mp4_head(path: &str) -> Result<Vec<u8>, HeadError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let window = read_file_at(&mut file, 0, MP4_WINDOW)?;
    drive_file(&mut Mp4HeadAt::new(&window, len), &file).map_err(|e| match e {
        FileError::Fetch(e) => HeadError::Mp4(e),
        FileError::Io { kind, .. } => HeadError::Io(kind.into()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use profluens_core::fetch::{drive_slice, drive_slice_log};

    fn boxes(list: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        for (k, b) in list {
            v.extend_from_slice(&((8 + b.len()) as u32).to_be_bytes());
            v.extend_from_slice(*k);
            v.extend_from_slice(b);
        }
        v
    }

    /// The first read `m` hands the caller.
    fn first_need<F: Fetch>(m: &mut F) -> Need {
        match m.poll() {
            Ok(Step::Need(need)) => need,
            _ => panic!("no read first"),
        }
    }

    fn write_tmp(name: &str, bytes: &[u8]) -> String {
        let path = std::env::temp_dir().join(format!("pf-probe-{name}-{}", std::process::id()));
        std::fs::write(&path, bytes).expect("write tmp");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn mp4_head_stops_at_mdat() {
        let file = boxes(&[
            (b"ftyp", b"isom\x00\x00\x02\x00mp41"),
            (b"moov", b"MOOVDATA"),
            (b"mdat", b"media"),
        ]);
        let path = write_tmp("faststart", &file);
        let head = mp4_head(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        assert_eq!(head, file[..file.len() - 13]);
    }

    #[test]
    fn mp4_head_reads_a_trailing_moov_off_disk() {
        let media = vec![0x55u8; 200_000];
        let file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        let path = write_tmp("moovlast", &file);
        let head = mp4_head(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        let mut want = boxes(&[(b"ftyp", b"isom")]);
        want.extend_from_slice(&boxes(&[(b"moov", b"MOOVDATA")]));
        assert_eq!(head, want);
        assert!(matches!(mp4_head("/nonexistent/pf-probe"), Err(HeadError::Io(_))));
    }

    #[test]
    fn mp4_head_without_mdat_is_the_whole_file() {
        let file = boxes(&[(b"ftyp", b"isom"), (b"moov", b"meta")]);
        let mut log = Vec::new();
        let mut m = Mp4HeadAt::new(&file, file.len() as u64);
        let head = drive_slice_log(&mut m, &file, &mut log).expect("head");
        assert_eq!((head, log.len()), (file, 0));
    }

    #[test]
    fn mp4_head_reads_a_moov_behind_the_mdat_once() {
        let media = vec![0x55u8; 300_000];
        let file =
            boxes(&[(b"ftyp", b"isom"), (b"free", b""), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        let mut log = Vec::new();
        let mut m = Mp4HeadAt::new(&file[..4096], file.len() as u64);
        let head = drive_slice_log(&mut m, &file, &mut log).expect("head");
        let mut want = boxes(&[(b"ftyp", b"isom"), (b"free", b"")]);
        want.extend_from_slice(&boxes(&[(b"moov", b"MOOVDATA")]));
        assert_eq!((&head, log.len()), (&want, 1));
        log.clear();
        let mut m = Mp4HeadAt::new(&file, file.len() as u64);
        let head = drive_slice_log(&mut m, &file, &mut log).expect("head");
        assert_eq!((head, log.len()), (want, 0), "a window holding it all reads nothing");
    }

    /// A moov at the front past the window comes in one read on from the window, not its next
    /// header first and then a hop back for the moov.
    #[test]
    fn mp4_head_reads_a_moov_past_the_window_once() {
        let moov = vec![0x4Du8; 300_000];
        let media = vec![0x55u8; 2 << 20];
        // a free box past the first read grows it, the moov is not read again
        for (free, reads) in [(0, 1), (1_100_000, 2)] {
            let free = vec![0u8; free];
            let file =
                boxes(&[(b"ftyp", b"isom"), (b"moov", &moov), (b"free", &free), (b"mdat", &media)]);
            let mut log = Vec::new();
            let mut m = Mp4HeadAt::new(&file[..4096], file.len() as u64);
            let head = drive_slice_log(&mut m, &file, &mut log).expect("head");
            assert_eq!(head, file[..12 + 8 + moov.len() + 8 + free.len()]);
            assert_eq!(log.len(), reads, "{log:?}");
            assert_eq!(log[0].at, 4096);
            for w in log.windows(2) {
                assert_eq!(w[1].at, w[0].at + w[0].len as u64, "on from the last, {log:?}");
            }
        }
        // the owning machine reads the same
        let file = boxes(&[(b"ftyp", b"isom"), (b"moov", &moov), (b"mdat", &media)]);
        let mut log = Vec::new();
        let mut m = Mp4HeadOwned::new(file[..4096].to_vec(), file.len() as u64);
        let head = drive_slice_log(&mut m, &file, &mut log).expect("head");
        assert_eq!((head.len(), log.len()), (12 + 8 + moov.len(), 1));
    }

    /// Box headers behind the mdat that each sit at the end of the last read do not grow the
    /// tail past a moov's worth, the file is not held whole.
    #[test]
    fn mp4_head_tail_behind_the_mdat_stays_bounded() {
        let mib = TAIL_FIRST;
        let mut file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &[0u8; 8])]);
        let window = file.len();
        // the first header past the window straddles the end of the first read, the 1 MiB
        // boxes after it straddle the end of each read after
        let mut sizes = vec![mib - 8];
        sizes.extend(std::iter::repeat_n(mib, (MAX_MOOV as usize + 4 * mib) / mib));
        for size in sizes {
            file.extend_from_slice(&(size as u32).to_be_bytes());
            file.extend_from_slice(b"free");
            file.resize(file.len() + size - 8, 0);
        }
        let moov = boxes(&[(b"moov", &[0x4D; 64])]);
        file.extend_from_slice(&moov);
        let mut m = Mp4HeadAt::new(&file[..window], file.len() as u64);
        let head = drive_slice(&mut m, &file).expect("head");
        assert_eq!(head.len(), 12 + moov.len(), "the ftyp and the moov");
        let held = m.walk.tail.capacity() as u64;
        assert!(held <= MAX_MOOV + 2 * mib as u64, "the tail grew to {held}");
    }

    #[test]
    fn mp4_head_takes_a_largesize_box() {
        let mut file = boxes(&[(b"ftyp", b"isom")]);
        let body = [0xAAu8; 10];
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(b"free");
        file.extend_from_slice(&(16 + body.len() as u64).to_be_bytes());
        file.extend_from_slice(&body);
        file.extend_from_slice(&boxes(&[(b"moov", b"M"), (b"mdat", b"x")]));
        let head = drive_slice(&mut Mp4HeadAt::new(&file, file.len() as u64), &file).expect("head");
        assert_eq!(head, file[..file.len() - 9]);
    }

    #[test]
    fn mp4_head_refuses_what_it_cannot_resolve() {
        let media = vec![0x55u8; 1000];
        let file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media)]);
        let err = drive_slice(&mut Mp4HeadAt::new(&file[..64], file.len() as u64), &file);
        assert!(matches!(err, Err(Mp4HeadError::NoMoov)), "{err:?}");
        let mut file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        file.truncate(file.len() - 5);
        let err = drive_slice(&mut Mp4HeadAt::new(&file[..64], file.len() as u64), &file);
        assert!(matches!(err, Err(Mp4HeadError::Truncated { .. })), "{err:?}");
        // the box behind the window is the caller's read
        let file = boxes(&[(b"ftyp", b"isom"), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        let need = first_need(&mut Mp4HeadAt::new(&file[..64], file.len() as u64));
        assert_eq!(need, Need { at: 1020, len: 16 });
        let mut bad = boxes(&[(b"ftyp", b"isom")]);
        bad.extend_from_slice(&[0, 0, 0, 4, b'f', b'r', b'e', b'e']);
        let mut log = Vec::new();
        let err = drive_slice_log(&mut Mp4HeadAt::new(&bad, bad.len() as u64), &bad, &mut log);
        assert!(matches!(err, Err(Mp4HeadError::Malformed { at: 12 })));
        let mut big = boxes(&[(b"ftyp", b"isom")]);
        big.extend_from_slice(&1u32.to_be_bytes());
        big.extend_from_slice(b"moov");
        big.extend_from_slice(&(MAX_MOOV + 1).to_be_bytes());
        let err = drive_slice_log(&mut Mp4HeadAt::new(&big, MAX_MOOV + 64), &big, &mut log);
        assert!(matches!(err, Err(Mp4HeadError::TooLarge { at: 12, .. })), "{err:?}");
        assert!(log.is_empty(), "nothing read for either, {log:?}");
    }

    #[test]
    fn a_fragmented_file_gives_its_mfra() {
        let mfra_body = b"tfra-entries";
        let mfra_len = 8 + mfra_body.len() as u32 + 16;
        let mut file = boxes(&[(b"ftyp", b"isom"), (b"moof", b"x"), (b"mdat", b"y")]);
        file.extend_from_slice(&mfra_len.to_be_bytes());
        file.extend_from_slice(b"mfra");
        file.extend_from_slice(mfra_body);
        file.extend_from_slice(&[0, 0, 0, 16, b'm', b'f', b'r', b'o', 0, 0, 0, 0]);
        file.extend_from_slice(&mfra_len.to_be_bytes());
        let mfra_at = |file: &[u8]| drive_slice(&mut Mp4MfraAt::new(file.len() as u64), file);
        let mfra = mfra_at(&file).expect("mfra");
        assert_eq!(mfra, file[file.len() - mfra_len as usize..]);
        assert_eq!(&mfra[4..8], b"mfra");
        let plain = boxes(&[(b"ftyp", b"isom"), (b"moov", b"m"), (b"mdat", b"y")]);
        assert!(mfra_at(&plain).expect("mfra").is_empty());
        let mut lying = plain.clone();
        lying.extend_from_slice(&[0, 0, 0, 16, b'm', b'f', b'r', b'o', 0, 0, 0, 0, 0, 1, 0, 0]);
        assert!(mfra_at(&lying).expect("mfra").is_empty());
        assert_eq!(mfra_size(&[0; 15]), None);
        let mut log = Vec::new();
        let mfra = drive_slice_log(&mut Mp4MfraAt::new(14), &plain[..14], &mut log);
        assert!(mfra.expect("mfra").is_empty());
        assert!(log.is_empty(), "a file under 16 bytes is read past its end");
    }

    /// An element of `id` with an 8 octet size.
    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = id.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect::<Vec<_>>();
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    /// The EBML header and a Segment of unknown size, then Info and Tracks.
    fn mkv_lead() -> Vec<u8> {
        let mut f = el(EBML, &el(0x4282, b"matroska"));
        f.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        f.extend(el(0x1549_A966, &[0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40]));
        f.extend(el(TRACKS, &el(0xAE, &[0xD7, 0x81, 0x01])));
        f
    }

    #[test]
    fn mkv_head_stops_at_the_first_cluster() {
        let mut file = mkv_lead();
        // a Cluster id inside an element's data is no Cluster
        file.extend(el(0xEC, &CLUSTER.to_be_bytes()));
        let meta = file.len();
        file.extend(el(CLUSTER, b"media"));
        file.extend(el(CLUSTER, b"media"));
        assert_eq!(mkv_header_from(&file).expect("head"), file[..meta]);
        let path = write_tmp("cluster", &file);
        let head = mkv_header_prefix(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        assert_eq!(head, file[..meta]);
        // a window cut in the Cluster's header after Tracks ends the head there
        assert_eq!(mkv_header_from(&file[..meta + 3]).expect("head"), file[..meta]);
        let tracks_end = mkv_lead().len();
        let err = mkv_header_from(&file[..tracks_end - 1]).unwrap_err();
        assert!(matches!(err, HeadError::NoCluster) && err.to_string().contains("Cluster"));
        assert!(matches!(mkv_header_from(b"segment-metadata"), Err(HeadError::NoCluster)));
    }

    #[test]
    fn mkv_head_ends_before_attachments_past_the_window() {
        let mut file = mkv_lead();
        let tracks_end = file.len();
        // 118 MB of fonts as a header, the window holds a little of them
        file.extend([0x19, 0x41, 0xA4, 0x69, 0x01, 0, 0, 0, 0x07, 0x60, 0, 0]);
        file.extend([0; 4096]);
        assert_eq!(mkv_header_from(&file).expect("head"), file[..tracks_end]);
        // before Tracks the head is not read yet
        let mut early = el(EBML, &el(0x4282, b"matroska"));
        early.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        early.extend([0x19, 0x41, 0xA4, 0x69, 0x01, 0, 0, 0, 0x07, 0x60, 0, 0]);
        early.extend([0; 64]);
        assert!(matches!(mkv_header_from(&early), Err(HeadError::NoCluster)));
    }

    #[test]
    fn mkv_head_at_reads_element_heads_then_the_head() {
        let mut file = mkv_lead();
        let meta = file.len();
        file.extend(el(CLUSTER, &[7; 4096]));
        file.extend(el(CLUSTER, &[7; 4096]));
        let mut log = Vec::new();
        let h = drive_slice_log(&mut MkvHeadAt::new(1 << 20), &file, &mut log).expect("head");
        assert_eq!((h.head.as_slice(), h.first_cluster), (&file[..meta], meta as u64));
        let cluster_bytes = |n: &Need| n.at + n.len as u64 > meta as u64 && n.len > 12;
        assert!(!log.iter().any(cluster_bytes), "{log:?}");
        assert_eq!(log.last(), Some(&Need { at: 0, len: meta }), "the head read last");
        let need = first_need(&mut MkvHeadAt::new(1 << 20));
        assert_eq!(need, Need { at: 0, len: EL_HEAD }, "the EBML header's head first");
        let err = drive_slice(&mut MkvHeadAt::new(1 << 20), b"not a matroska file").unwrap_err();
        assert!(matches!(err, MkvHeadError::NotMatroska), "{err:?}");
        let err = drive_slice(&mut MkvHeadAt::new(1 << 20), &file[..meta - 3]).unwrap_err();
        assert!(matches!(err, MkvHeadError::Malformed { .. }), "a file cut in the head, {err:?}");
        let mut m = MkvHeadAt::new(1 << 20);
        let again = (drive_slice(&mut m, &file), drive_slice(&mut m, &file));
        assert_eq!(again.0, again.1, "a machine that is done starts over");
    }

    /// `m` over `prefix`, then `gap` bytes of zeros, then `rest`, read as a sparse file.
    fn sparse<F: Fetch>(m: &mut F, prefix: &[u8], gap: u64, rest: &[u8]) -> Result<F::Out, F::Err> {
        let behind = prefix.len() as u64 + gap;
        let byte = |at: u64| match at {
            at if at < prefix.len() as u64 => Some(prefix[at as usize]),
            at if at < behind => Some(0),
            at => rest.get((at - behind) as usize).copied(),
        };
        loop {
            match m.poll()? {
                Step::Done(out) => return Ok(out),
                Step::Need(need) => {
                    let buf = m.buf();
                    let mut n = 0;
                    while let Some(b) = (n < buf.len()).then(|| byte(need.at + n as u64)).flatten()
                    {
                        buf[n] = b;
                        n += 1;
                    }
                    m.filled(n);
                }
            }
        }
    }

    #[test]
    fn mkv_head_at_ends_before_attachments_past_the_cap() {
        let mut lead = mkv_lead();
        let tracks_end = lead.len();
        // 80 MB of fonts as a header, their body never read
        let size: u64 = 80 << 20;
        lead.extend([0x19, 0x41, 0xA4, 0x69, 0x01]);
        lead.extend_from_slice(&size.to_be_bytes()[1..]);
        let rest = el(CLUSTER, b"media");
        let h = sparse(&mut MkvHeadAt::new(64 << 20), &lead, size, &rest).expect("head");
        assert_eq!(h.head, lead[..tracks_end], "the head ends where the Attachments start");
        assert_eq!(h.first_cluster, lead.len() as u64 + size, "the Cluster behind them");
        // a walk that breaks behind the cut takes the cut as the first Cluster
        let h = sparse(&mut MkvHeadAt::new(64 << 20), &lead, size, &[0x00]).expect("head");
        assert_eq!(h.first_cluster, tracks_end as u64);
    }

    /// Attachments behind Tracks end the head under the cap too, so the fonts are never read and
    /// no head copy holds them. Ahead of Tracks they are part of the head.
    #[test]
    fn mkv_head_ends_before_attachments_of_any_size() {
        let mut file = mkv_lead();
        let tracks_end = file.len();
        file.extend(el(ATTACHMENTS, &[7; 4096]));
        let cluster = file.len();
        file.extend(el(CLUSTER, b"media"));
        assert_eq!(mkv_header_from(&file).expect("head"), file[..tracks_end]);
        let mut log = Vec::new();
        let h = drive_slice_log(&mut MkvHeadAt::new(1 << 20), &file, &mut log).expect("head");
        assert_eq!(h.head, file[..tracks_end]);
        assert_eq!((h.first_cluster, h.attachments), (cluster as u64, Some(tracks_end as u64)));
        assert!(log.iter().all(|n| n.len <= tracks_end), "the fonts unread, {log:?}");
        let mut early = el(EBML, &el(0x4282, b"matroska"));
        early.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        let at = early.len();
        early.extend(el(ATTACHMENTS, &[7; 64]));
        early.extend(el(TRACKS, &el(0xAE, &[0xD7, 0x81, 0x01])));
        let cluster = early.len();
        early.extend(el(CLUSTER, b"media"));
        let h = drive_slice(&mut MkvHeadAt::new(1 << 20), &early).expect("head");
        assert_eq!((h.head.len(), h.attachments), (cluster, Some(at as u64)), "ahead of Tracks");
        assert_eq!(mkv_header_from(&early).expect("head"), early[..cluster]);
    }

    #[test]
    fn mkv_head_at_refuses_a_head_past_the_cap_before_reading_it() {
        let mut f = el(EBML, &el(0x4282, b"matroska"));
        f.extend([0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        let mut big = f.clone();
        // a Void of 2^40 bytes before Tracks, the Cluster behind it
        big.extend([0xEC, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let rest = el(CLUSTER, b"media");
        let err = sparse(&mut MkvHeadAt::new(64 << 20), &big, 1 << 40, &rest).unwrap_err();
        assert!(matches!(err, MkvHeadError::TooLarge { .. }), "{err:?}");
        // more empty Voids than the walk takes
        for _ in 0..MKV_MAX_TOP + 10 {
            f.extend([0xEC, 0x80]);
        }
        let err = sparse(&mut MkvHeadAt::new(64 << 20), &f, 0, &rest).unwrap_err();
        assert!(matches!(err, MkvHeadError::TooMany { .. }), "{err:?}");
        // an EBML header that puts the Segment past the cap, a Cluster first in it
        let mut huge = vec![0x1A, 0x45, 0xDF, 0xA3, 0x01];
        huge.extend_from_slice(&(1u64 << 30).to_be_bytes()[1..]);
        let mut seg = vec![0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        seg.extend(el(CLUSTER, b"media"));
        let err = sparse(&mut MkvHeadAt::new(64 << 20), &huge, 1 << 30, &seg).unwrap_err();
        assert!(matches!(err, MkvHeadError::TooLarge { at: 0, .. }), "{err:?}");
    }

    #[test]
    fn avi_head_keeps_the_movi_fourcc() {
        let file =
            b"RIFF\x00\x00\x00\x00AVI LIST\x10\x00\x00\x00hdrlstrhLIST\x00\x00\x00\x00movi00dc";
        let head = avi_head_from(file).expect("head");
        assert_eq!(head, file[..file.len() - 4]);
        assert!(head.ends_with(b"movi"));
        let path = write_tmp("avi", file);
        let from_disk = avi_head(&path).expect("head");
        let _ = std::fs::remove_file(&path);
        assert_eq!(from_disk, head);
        assert!(matches!(avi_head_from(&file[..20]), Err(HeadError::NoMovi)));
        assert!(matches!(avi_head("/nonexistent/pf-probe"), Err(HeadError::Io(_))));
    }

    #[test]
    fn avi_head_at_reads_a_step_at_a_time_to_the_movi() {
        let mut file = vec![0u8; AVI_STEP + 64];
        file[..12].copy_from_slice(b"RIFF\x00\x00\x00\x00AVI ");
        file[AVI_STEP - 2..AVI_STEP + 2].copy_from_slice(b"movi");
        let mut log = Vec::new();
        let head = drive_slice_log(&mut AviHeadAt::new(16 << 20), &file, &mut log).expect("head");
        assert_eq!(head, file[..AVI_STEP + 2], "a movi across two reads is found");
        let at = |at: usize, len| Need { at: at as u64, len };
        assert_eq!(log, [at(0, AVI_STEP), at(AVI_STEP, AVI_STEP)]);
        let none = vec![7u8; 1000];
        let err = drive_slice(&mut AviHeadAt::new(16 << 20), &none).unwrap_err();
        assert!(matches!(err, AviHeadError::NoMovi { read: 1000 }), "{err:?}");
        let long = vec![7u8; 3 * AVI_STEP];
        log.clear();
        let err = drive_slice_log(&mut AviHeadAt::new(AVI_STEP + 10), &long, &mut log).unwrap_err();
        assert!(matches!(err, AviHeadError::NoMovi { read } if read == AVI_STEP as u64 + 10));
        assert_eq!(log[1], at(AVI_STEP, 10), "no read past the cap");
        assert_eq!(first_need(&mut AviHeadAt::new(16 << 20)), at(0, AVI_STEP));
    }

    #[test]
    fn mkv_head_in_a_window_reads_nothing() {
        let mut file = mkv_lead();
        let meta = file.len();
        file.extend(el(CLUSTER, &[7; 4096]));
        let mut log = Vec::new();
        let want = drive_slice_log(&mut MkvHeadAt::new(1 << 20), &file, &mut log).expect("head");
        let reads = log.len();
        assert_eq!(reads, 6, "EBML, Segment, Info, Tracks, Cluster, the head, {log:?}");
        log.clear();
        let window = file[..meta + 100].to_vec();
        let ptr = window.as_ptr();
        let mut m = MkvHeadAt::with_window(1 << 20, window);
        let h = drive_slice_log(&mut m, &file, &mut log).expect("head");
        assert_eq!((&h, log.len()), (&want, 0));
        assert_eq!(h.head.as_ptr(), ptr, "the window is the head, not a copy");
        let again = drive_slice_log(&mut m, &file, &mut log).expect("head");
        assert_eq!((again, log.len()), (want, reads), "the window is spent by the first run");
    }

    #[test]
    fn mkv_head_past_a_window_reads_only_past_it() {
        let mut file = mkv_lead();
        let meta = file.len();
        file.extend(el(CLUSTER, &[7; 4096]));
        let want = drive_slice(&mut MkvHeadAt::new(1 << 20), &file).expect("head");
        for cut in [1, 20, 30, meta - 3, meta, meta + 3] {
            let mut log = Vec::new();
            let mut m = MkvHeadAt::with_window(1 << 20, file[..cut].to_vec());
            let h = drive_slice_log(&mut m, &file, &mut log).expect("head");
            assert_eq!(h, want, "a window of {cut}");
            let past = |n: &Need| n.at + n.len as u64 > cut as u64;
            assert!(log.iter().all(past), "{cut}: {log:?}");
            if cut < meta {
                assert_eq!(log.last(), Some(&Need { at: cut as u64, len: meta - cut }), "{cut}");
            }
        }
        // a window that is the whole file, the head cut in it
        let short = &file[..meta - 3];
        let err = drive_slice(&mut MkvHeadAt::with_window(1 << 20, short.to_vec()), short);
        assert!(matches!(err, Err(MkvHeadError::Malformed { .. })), "{err:?}");
        let err = drive_slice(&mut MkvHeadAt::with_window(1 << 20, b"no ebml".to_vec()), &file);
        assert!(matches!(err, Err(MkvHeadError::NotMatroska)), "{err:?}");
    }

    #[test]
    fn mkv_head_window_stops_before_attachments_past_the_cap() {
        let mut lead = mkv_lead();
        let tracks_end = lead.len();
        let size: u64 = 80 << 20;
        lead.extend([0x19, 0x41, 0xA4, 0x69, 0x01]);
        lead.extend_from_slice(&size.to_be_bytes()[1..]);
        let rest = el(CLUSTER, b"media");
        let mut m = MkvHeadAt::with_window(64 << 20, lead.clone());
        let h = sparse(&mut m, &lead, size, &rest).expect("head");
        assert_eq!(h.head, lead[..tracks_end]);
        assert_eq!(h.first_cluster, lead.len() as u64 + size);
    }

    #[test]
    fn avi_head_in_a_window_reads_nothing() {
        let file = b"RIFF\x00\x00\x00\x00AVI LIST\x10\x00\x00\x00hdrlLIST\x00\x00\x00\x00movi00dc";
        let mut log = Vec::new();
        let mut m = AviHeadAt::with_window(16 << 20, file.to_vec());
        let head = drive_slice_log(&mut m, file, &mut log).expect("head");
        assert_eq!((head.as_slice(), log.len()), (&file[..file.len() - 4], 0));
        // past the window it reads on from its end, a movi across the edge is found
        let mut big = vec![0u8; AVI_STEP + 64];
        big[1000..1004].copy_from_slice(b"movi");
        let mut m = AviHeadAt::with_window(16 << 20, big[..1002].to_vec());
        let head = drive_slice_log(&mut m, &big, &mut log).expect("head");
        assert_eq!(head, big[..1004]);
        assert_eq!(log, [Need { at: 1002, len: AVI_STEP }]);
        log.clear();
        let mut m = AviHeadAt::with_window(16 << 20, big[..999].to_vec());
        let none = vec![7u8; 1000];
        let err = drive_slice_log(&mut m, &none, &mut log).unwrap_err();
        assert!(matches!(err, AviHeadError::NoMovi { read: 1000 }), "{err:?}");
        assert_eq!(log, [Need { at: 999, len: AVI_STEP }]);
        // a window past the cap is cut to it
        let mut m = AviHeadAt::with_window(8, big[..2000].to_vec());
        assert!(matches!(drive_slice(&mut m, &big), Err(AviHeadError::NoMovi { read: 8 })));
    }

    #[test]
    fn mp4_head_owned_reads_what_the_borrowing_one_does() {
        let media = vec![0x55u8; 300_000];
        let file =
            boxes(&[(b"ftyp", b"isom"), (b"free", b""), (b"mdat", &media), (b"moov", b"MOOVDATA")]);
        let fast = boxes(&[(b"ftyp", b"isom"), (b"moov", b"MOOVDATA"), (b"mdat", b"x")]);
        for (file, wlen) in [(&file, 4096), (&file, 20), (&fast, fast.len()), (&fast, 10)] {
            let len = file.len() as u64;
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let want = drive_slice_log(&mut Mp4HeadAt::new(&file[..wlen], len), file, &mut a);
            let mut m = Mp4HeadOwned::new(file[..wlen].to_vec(), len);
            assert_eq!(drive_slice_log(&mut m, file, &mut b), want);
            assert_eq!(a, b);
            assert_eq!(drive_slice(&mut m, file), want, "a machine that is done starts over");
            assert_eq!(m.into_window(), file[..wlen]);
        }
        let bad = [0, 0, 0, 4, b'f', b'r', b'e', b'e', 0, 0];
        let err = drive_slice(&mut Mp4HeadOwned::new(Vec::new(), 10), &bad);
        assert!(matches!(err, Err(Mp4HeadError::Malformed { at: 0 })), "{err:?}");
    }

    #[test]
    fn read_prefix_stops_at_the_end_of_a_short_file() {
        let path = write_tmp("short", b"0123456789");
        let (all, some) =
            (read_prefix(&path, 64).expect("read"), read_prefix(&path, 4).expect("read"));
        let _ = std::fs::remove_file(&path);
        assert_eq!((all.as_slice(), some.as_slice()), (&b"0123456789"[..], &b"0123"[..]));
    }
}
