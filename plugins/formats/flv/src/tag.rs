//! The tag layer (Annex E.2, E.3), the file header, the 11 octet tag headers and the
//! PreviousTagSize back pointers. A sans-IO reader fed in any pieces that resyncs over damage.

pub const FILE_HEADER: usize = 9;
pub const TAG_HEADER: usize = 11;
pub const BACK: usize = 4;
pub const AUDIO: u8 = 8;
pub const VIDEO: u8 = 9;
pub const SCRIPT: u8 = 18;
/// The first octet of an Enhanced RTMP video tag body has its high bit set.
pub const ENHANCED: u8 = 0x80;
/// A DataOffset past this is junk and taken as 9.
const MAX_OFFSET: u32 = 1 << 16;
/// The largest tag a resync lands on, a larger one is taken for junk.
pub const RESYNC_MAX: u32 = 4 << 20;
/// A tag past four times the largest read and this goes to a resync before it is waited for.
pub const TRUSTED: u32 = 1 << 20;

pub fn be24(b: &[u8]) -> u32 {
    u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2])
}

pub fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub version: u8,
    pub audio: bool,
    pub video: bool,
    pub data_offset: u32,
}

/// `FLV`, the version and the type flags (E.2), `None` without the signature.
pub fn file_header(b: &[u8]) -> Option<FileHeader> {
    let h = b.get(..FILE_HEADER)?;
    if &h[..3] != b"FLV" {
        return None;
    }
    let (audio, video) = (h[4] & 4 != 0, h[4] & 1 != 0);
    Some(FileHeader { version: h[3], audio, video, data_offset: be32(&h[5..9]) })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagHeader {
    pub kind: u8,
    /// Encrypted (E.4.1 Filter), nothing here reads it.
    pub filtered: bool,
    pub size: u32,
    /// The 24 bit timestamp with the extension as its high octet, ms.
    pub ts_ms: u32,
    pub stream_id: u32,
}

impl TagHeader {
    pub fn parse(b: &[u8]) -> Option<TagHeader> {
        let h = b.get(..TAG_HEADER)?;
        Some(TagHeader {
            kind: h[0] & 0x1F,
            filtered: h[0] & 0x20 != 0,
            size: be24(&h[1..4]),
            ts_ms: be24(&h[4..7]) | u32::from(h[7]) << 24,
            stream_id: be24(&h[8..11]),
        })
    }

    pub fn sane(&self) -> bool {
        matches!(self.kind, AUDIO | VIDEO | SCRIPT) && self.stream_id == 0
    }

    pub fn whole(&self) -> usize {
        TAG_HEADER + self.size as usize + BACK
    }
}

/// The SoundFormat and the CodecID or [`ENHANCED`] the tags of the streams found start with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Expect {
    pub audio: Option<u8>,
    pub video: Option<u8>,
}

impl Expect {
    pub fn fits(&self, kind: u8, first: u8) -> bool {
        match kind {
            AUDIO => self.audio.is_none_or(|f| first >> 4 == f),
            VIDEO => {
                let frame = first >> 4 & 7;
                let codec = |v: u8| match first & ENHANCED {
                    0 => first & 0x0F == v,
                    _ => v == ENHANCED,
                };
                (1..=5).contains(&frame) && self.video.is_none_or(codec)
            }
            SCRIPT => first == 2,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Tag<'a> {
    pub header: TagHeader,
    /// File byte of the tag header.
    pub pos: u64,
    pub body: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    File,
    Tags,
    Resync,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub tags: u64,
    pub resyncs: u64,
    pub skipped: u64,
    /// Tags taken on the next header though their back pointer was off.
    pub loose: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Need(usize),
    Skip(usize),
    Go(State, usize),
    Tag(TagHeader, bool),
}

fn short(n: usize, b: &[u8], end: bool) -> Step {
    if end { Step::Skip(b.len()) } else { Step::Need(n) }
}

fn backed(b: &[u8], h: &TagHeader) -> bool {
    let at = TAG_HEADER + h.size as usize;
    b.get(at..at + BACK).is_some_and(|p| be32(p) == h.size + TAG_HEADER as u32)
}

/// A header a resync may land on, the reserved and filter bits clear and the body fitting.
fn candidate(b: &[u8], expect: &Expect) -> bool {
    let Some(h) = TagHeader::parse(b) else { return false };
    b[0] & 0xE0 == 0
        && h.sane()
        && (1..=RESYNC_MAX).contains(&h.size)
        && b.get(TAG_HEADER).is_none_or(|&f| expect.fits(h.kind, f))
}

fn file_step(b: &[u8], end: bool) -> Step {
    if b.len() < FILE_HEADER {
        return short(FILE_HEADER, b, end);
    }
    let Some(h) = file_header(b) else { return Step::Go(State::Resync, 0) };
    let offset = match h.data_offset {
        n @ 9..=MAX_OFFSET => n as usize,
        _ => FILE_HEADER,
    };
    let len = offset + BACK;
    if b.len() < len {
        return short(len, b, end);
    }
    Step::Go(State::Tags, len)
}

/// Taken on its back pointer, or where that is off on a next header a resync would take.
fn tag_step(b: &[u8], expect: &Expect, bound: u32, end: bool) -> Step {
    let Some(h) = TagHeader::parse(b) else { return short(TAG_HEADER, b, end) };
    let fits = b.get(TAG_HEADER).is_none_or(|&f| h.size == 0 || expect.fits(h.kind, f));
    if !h.sane() || !fits {
        return Step::Go(State::Resync, 1);
    }
    if h.size > bound {
        return Step::Go(State::Resync, 0);
    }
    let whole = h.whole();
    if b.len() < whole {
        return if end { Step::Go(State::Resync, 1) } else { Step::Need(whole) };
    }
    if backed(b, &h) {
        return Step::Tag(h, false);
    }
    match b.get(whole..whole + TAG_HEADER + 1) {
        Some(next) if candidate(next, expect) => Step::Tag(h, true),
        Some(_) => Step::Go(State::Resync, 1),
        None if end => match b.get(whole..) {
            Some(next) if next.len() >= TAG_HEADER && !candidate(next, expect) => {
                Step::Go(State::Resync, 1)
            }
            _ => Step::Tag(h, true),
        },
        None => Step::Need(whole + TAG_HEADER + 1),
    }
}

/// Two headers chained by size, or one whose back pointer holds, ends a resync.
fn resync_step(b: &[u8], expect: &Expect, end: bool) -> Step {
    let Some(i) = (0..=b.len().saturating_sub(TAG_HEADER)).find(|&i| candidate(&b[i..], expect))
    else {
        let keep = TAG_HEADER - 1;
        return match b.len() {
            _ if end => Step::Skip(b.len()),
            n if n > keep => Step::Skip(n - keep),
            _ => Step::Need(TAG_HEADER),
        };
    };
    if i > 0 {
        return Step::Skip(i);
    }
    let Some(h) = TagHeader::parse(b) else { return short(TAG_HEADER, b, end) };
    let whole = h.whole();
    if b.len() < whole {
        return if end { Step::Skip(1) } else { Step::Need(whole) };
    }
    if backed(b, &h) {
        return Step::Tag(h, false);
    }
    match b.get(whole..whole + TAG_HEADER + 1) {
        Some(next) if candidate(next, expect) => Step::Tag(h, true),
        Some(_) => Step::Skip(1),
        None if end => Step::Skip(1),
        None => Step::Need(whole + TAG_HEADER + 1),
    }
}

/// Bytes in, tags out. Only a tag or header cut across pieces is copied, into `carry`.
pub struct Reader {
    state: State,
    carry: Vec<u8>,
    need: usize,
    largest: u32,
    /// File byte of `carry[0]`, or of the next input when nothing is carried.
    pos: u64,
    pub expect: Expect,
    pub stats: Stats,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    // Once per element, an empty Vec allocates nothing.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Reader {
        Reader {
            state: State::File,
            carry: Vec::new(),
            need: 0,
            largest: 0,
            pos: 0,
            expect: Expect::default(),
            stats: Stats::default(),
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn next_byte(&self) -> u64 {
        self.pos + self.carry.len() as u64
    }

    /// Input resumes at file byte `at`, a resync unless that is the file's start.
    pub fn restart(&mut self, at: u64) {
        self.carry.clear();
        (self.need, self.pos) = (0, at);
        self.state = if at == 0 { State::File } else { State::Resync };
    }

    /// Input goes on from somewhere unknown, read on from the next tag a resync finds.
    pub fn resync(&mut self) {
        self.carry.clear();
        self.need = 0;
        self.state = State::Resync;
    }

    pub fn push(&mut self, mut data: &[u8], sink: &mut impl FnMut(Tag<'_>)) {
        loop {
            if self.carry.is_empty() {
                let used = self.run(data, false, sink);
                self.carry.extend_from_slice(&data[used..]);
                return;
            }
            let want = self.need.saturating_sub(self.carry.len()).min(data.len());
            self.carry.extend_from_slice(&data[..want]);
            data = &data[want..];
            if self.carry.len() < self.need {
                return;
            }
            let carry = std::mem::take(&mut self.carry);
            let used = self.run(&carry, false, sink);
            self.carry = carry;
            self.carry.drain(..used);
        }
    }

    /// The end of input, what is carried is read as the file's last bytes.
    pub fn finish(&mut self, sink: &mut impl FnMut(Tag<'_>)) {
        let carry = std::mem::take(&mut self.carry);
        self.run(&carry, true, sink);
        self.carry = carry;
        self.carry.clear();
        self.need = 0;
    }

    fn run(&mut self, buf: &[u8], end: bool, sink: &mut impl FnMut(Tag<'_>)) -> usize {
        let mut at = 0;
        while at < buf.len() {
            let rest = &buf[at..];
            let step = match self.state {
                State::File => file_step(rest, end),
                State::Tags => {
                    let bound = self.largest.saturating_mul(4).max(TRUSTED);
                    tag_step(rest, &self.expect, bound, end)
                }
                State::Resync => resync_step(rest, &self.expect, end),
            };
            let used = match step {
                Step::Need(n) if n > rest.len() => {
                    self.need = n;
                    return at;
                }
                Step::Need(_) | Step::Skip(_) => {
                    let n = match step {
                        Step::Skip(n) => n.clamp(1, rest.len()),
                        _ => 1,
                    };
                    self.stats.skipped += n as u64;
                    n
                }
                Step::Go(state, n) => {
                    if state == State::Resync && self.state != State::Resync {
                        self.stats.resyncs += 1;
                    }
                    if state == State::Resync {
                        self.stats.skipped += n as u64;
                    }
                    self.state = state;
                    n
                }
                Step::Tag(header, loose) => {
                    let body = &rest[TAG_HEADER..TAG_HEADER + header.size as usize];
                    sink(Tag { header, pos: self.pos, body });
                    self.state = State::Tags;
                    self.largest = self.largest.max(header.size);
                    self.stats.tags += 1;
                    self.stats.loose += u64::from(loose);
                    header.whole().min(rest.len())
                }
            };
            at += used;
            self.pos += used as u64;
        }
        self.need = 0;
        at
    }
}

#[cfg(test)]
mod tests;
