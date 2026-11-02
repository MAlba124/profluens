//! One session's position in a bare file. The frames are found as the session reads forward,
//! [`NextAt`] and [`SeekAt`] ask the caller for the bytes, read into one reused window. A FLAC
//! frame header numbers its samples, so a FLAC seek bisects the file on them and lands exactly.
//! MPEG audio and ADTS count frames from the first, a CBR stream seeks on its line, a VBR one on
//! the Xing table or by bytes, then walks frame headers to the target, exact once it walked from a
//! point it counted to.

use crate::BareError;
use crate::flac::{self, MAX_HEADER};
use crate::head::{BareHead, Codec, adts};
use pf_mp3::frame::Header as MpaHeader;
use profluens_core::fetch::{Fetch, Need, Step};

/// Read ahead at a time.
pub(crate) const CHUNK: u64 = 128 << 10;
/// The window never holds more, a FLAC frame is far below it.
const MAX_WINDOW: usize = 32 << 20;
/// How far a FLAC frame's end or a resync is looked for.
const MAX_SCAN: u64 = 4 << 20;
/// A FLAC seek bisects until the bracket is this small, then walks.
const BISECT_STOP: u64 = 64 << 10;
/// A counted point this close before a target is walked from instead of estimating.
const WALK_SECS: u64 = 20;
/// Counted points kept for exact seeks back, about one a second.
const MAX_KNOWN: usize = 1 << 16;
/// Bytes one `next` or `seek` may scan for syncs, a file that keeps it looking past this is
/// refused.
const MAX_WORK: u64 = 64 << 20;
/// Scan bytes a header parse counts as.
const PARSE_COST: u64 = 64;

/// A frame header as the reader needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hdr {
    /// Frame length, 0 for FLAC where only the next header ends it.
    pub(crate) len: u64,
    /// Header bytes the unit leaves out, ADTS.
    skip: u64,
    samples: u32,
    /// The first sample as a FLAC header states it.
    sample: Option<u64>,
}

/// The frame header at the start of `b` when it is one of the stream `c`.
pub(crate) fn header(c: &Codec, b: &[u8]) -> Option<Hdr> {
    match *c {
        Codec::Flac { si, variable } => {
            let f = flac::frame(b, &si).filter(|f| f.variable == variable)?;
            Some(Hdr { len: 0, skip: 0, samples: f.block, sample: Some(f.sample) })
        }
        Codec::Mpa { stream } => {
            let h = MpaHeader::parse(b).filter(|h| h.same_stream(&stream))?;
            let len = h.frame_len().filter(|&n| n >= 4)? as u64;
            Some(Hdr { len, skip: 0, samples: h.samples_per_frame(), sample: None })
        }
        Codec::Adts { profile, sfi, channels } => {
            let (p, s, ch, len, skip, samples) = adts(b)?;
            ((p, s, ch) == (profile, sfi, channels)).then_some(Hdr {
                len,
                skip,
                samples,
                sample: None,
            })
        }
    }
}

fn is_sync(c: &Codec, b0: u8, b1: u8) -> bool {
    b0 == 0xFF
        && match c {
            Codec::Flac { .. } => b1 & 0xFE == 0xF8,
            Codec::Mpa { .. } => b1 & 0xE0 == 0xE0,
            Codec::Adts { .. } => b1 & 0xF6 == 0xF0,
        }
}

/// File bytes `[at, at + buf.len())`, one reused buffer.
#[derive(Debug)]
struct Win {
    buf: Vec<u8>,
    at: u64,
    /// Read at least this far ahead, less while a seek probes.
    ahead: u64,
    /// The bytes held before the fill in flight, None with none in flight.
    fill: Option<usize>,
}

impl Default for Win {
    // Cold, once per reader.
    #[allow(clippy::disallowed_methods)]
    fn default() -> Self {
        Win { buf: Vec::new(), at: 0, ahead: CHUNK, fill: None }
    }
}

impl Win {
    fn end(&self) -> u64 {
        self.at + self.buf.len() as u64
    }

    /// Up to `n` bytes held from `from`.
    fn get(&self, from: u64, n: u64) -> &[u8] {
        if from < self.at || from >= self.end() {
            return &[];
        }
        let s = (from - self.at) as usize;
        let e = (s as u64 + n).min(self.buf.len() as u64) as usize;
        &self.buf[s..e]
    }

    /// Holds `[from, to)` of a file ending at `end`, reading a chunk ahead, the bytes before
    /// `from` let go. The read that brings what it lacks into [`Win::hole`], None when held.
    fn want(&mut self, from: u64, to: u64, end: u64) -> Result<Option<Need>, BareError> {
        let to = to.min(end);
        if from >= to || (self.at <= from && to <= self.end()) {
            return Ok(None);
        }
        if self.at <= from && from <= self.end() {
            let drop = (from - self.at) as usize;
            self.buf.copy_within(drop.., 0);
            self.buf.truncate(self.buf.len() - drop);
        } else {
            self.buf.clear();
        }
        self.at = from;
        let want = to.max(from.saturating_add(self.ahead)).min(end) - from;
        if want > MAX_WINDOW as u64 {
            return Err(BareError::TooLarge);
        }
        let have = self.buf.len();
        self.buf.resize(want as usize, 0);
        self.fill = Some(have);
        Ok(Some(Need { at: from + have as u64, len: want as usize - have }))
    }

    /// Where the fill in flight lands.
    fn hole(&mut self) -> &mut [u8] {
        let have = self.fill.unwrap_or(self.buf.len()).min(self.buf.len());
        &mut self.buf[have..]
    }

    /// `n` bytes of the fill in flight came, fewer at the end of the file.
    fn filled(&mut self, n: usize) {
        if let Some(have) = self.fill.take() {
            let have = have.min(self.buf.len());
            self.buf.truncate(have + n.min(self.buf.len() - have));
        }
    }

    /// Lets go of a fill an op dropped half way and of a probing seek's read ahead.
    fn settle(&mut self) {
        self.filled(0);
        self.ahead = CHUNK;
    }
}

/// A unit the reader found, where its payload lies and when it plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    pub off: u64,
    pub len: u32,
    pub pts_ns: i64,
    pub dur_ns: u32,
}

/// One session's position in a bare file.
#[derive(Debug, Default)]
pub struct Reader {
    win: Win,
    /// The next frame's first byte.
    at: u64,
    /// Its first sample.
    n: u64,
    /// `n` was counted, not estimated.
    exact: bool,
    /// Counted points, sample and byte, ascending.
    known: Vec<(u64, u64)>,
    /// Scan bytes the current call has left.
    work: u64,
}

impl Reader {
    // Cold, once per run.
    #[allow(clippy::disallowed_methods)]
    pub fn new(h: &BareHead) -> Reader {
        Reader { at: h.first, exact: true, known: vec![(0, h.first)], ..Reader::default() }
    }

    /// Whether the stamps from here on are counted.
    pub fn exact(&self) -> bool {
        self.exact
    }

    /// The bytes of `u`, held until the next call.
    pub fn bytes(&self, u: &Unit) -> &[u8] {
        self.win.get(u.off, u64::from(u.len))
    }

    /// The next unit, None at the end. Polled again once done, the unit after it.
    pub fn next<'a>(&'a mut self, h: &'a BareHead) -> NextAt<'a> {
        self.win.settle();
        self.work = MAX_WORK;
        NextAt { rd: self, h, st: NextSt::Top }
    }

    /// To the frame that holds `ns`, the last one past the end.
    pub fn seek<'a>(&'a mut self, h: &'a BareHead, ns: i64) -> SeekAt<'a> {
        self.win.settle();
        self.work = MAX_WORK;
        let mut want = h.sample_at(ns);
        if h.samples > 0 {
            want = want.min(h.samples - 1);
        }
        SeekAt { rd: self, h, want, st: SeekSt::Start }
    }

    /// The machine `p` left, its fill still to come.
    pub(crate) fn resume_next<'a>(&'a mut self, h: &'a BareHead, p: NextPark) -> NextAt<'a> {
        NextAt { rd: self, h, st: p.0 }
    }

    pub(crate) fn resume_seek<'a>(&'a mut self, h: &'a BareHead, p: SeekPark) -> SeekAt<'a> {
        SeekAt { rd: self, h, want: p.want, st: p.st }
    }
}

/// What the read steps below work on, the window, the scan budget and the head.
struct Cx<'a> {
    win: &'a mut Win,
    work: &'a mut u64,
    h: &'a BareHead,
}

/// A step that is done, or the window's fill it waits on.
type Go<T> = Result<Step<T>, BareError>;

/// The frame header at `at`.
#[derive(Debug, Clone, Copy)]
struct HdrAt {
    at: u64,
    asked: bool,
}

impl HdrAt {
    fn new(at: u64) -> HdrAt {
        HdrAt { at, asked: false }
    }

    fn step(&mut self, cx: &mut Cx<'_>) -> Go<Option<Hdr>> {
        let mh = MAX_HEADER as u64;
        if !std::mem::replace(&mut self.asked, true)
            && let Some(need) = cx.win.want(self.at, self.at + mh, cx.h.end)?
        {
            return Ok(Step::Need(need));
        }
        Ok(Step::Done(header(&cx.h.codec, cx.win.get(self.at, mh))))
    }
}

/// Where [`Sync`] goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncPhase {
    /// Holds the bytes from `pos` on.
    Fill,
    /// Looks for a sync from `pos` on.
    Scan,
    /// Parses the header of the sync at `at`.
    Parse,
}

/// The next sync at or after `pos` and before `to` whose header parses, the window kept from
/// `anchor` on.
#[derive(Debug, Clone, Copy)]
struct Sync {
    anchor: u64,
    pos: u64,
    to: u64,
    at: u64,
    phase: SyncPhase,
}

impl Sync {
    fn new(h: &BareHead, anchor: u64, from: u64, to: u64) -> Sync {
        Sync { anchor, pos: from, to: to.min(h.end), at: 0, phase: SyncPhase::Fill }
    }

    fn step(&mut self, cx: &mut Cx<'_>) -> Go<Option<(u64, Hdr)>> {
        let (h, mh) = (cx.h, MAX_HEADER as u64);
        loop {
            match self.phase {
                SyncPhase::Fill => {
                    let pos = self.pos;
                    if pos + 1 >= self.to {
                        return Ok(Step::Done(None));
                    }
                    self.phase = SyncPhase::Scan;
                    let held = cx.win.at <= pos && pos + mh <= cx.win.end();
                    if !held && (pos < cx.win.at || cx.win.end() < h.end) {
                        let ahead = (pos + cx.win.ahead).min(self.to + mh);
                        let from = self.anchor.min(pos);
                        if let Some(need) = cx.win.want(from, ahead.max(pos + mh), h.end)? {
                            return Ok(Step::Need(need));
                        }
                    }
                }
                SyncPhase::Scan => {
                    let pos = self.pos;
                    let hay = cx.win.get(pos, self.to - pos);
                    if hay.len() < 2 {
                        return Ok(Step::Done(None));
                    }
                    let found = hay.windows(2).position(|w| is_sync(&h.codec, w[0], w[1]));
                    let spent = found.map_or(hay.len() as u64, |i| i as u64 + PARSE_COST);
                    *cx.work = cx.work.checked_sub(spent).ok_or(BareError::Malformed(pos))?;
                    let Some(i) = found else {
                        // the last byte may be a sync's first
                        self.pos += hay.len() as u64 - 1;
                        self.phase = SyncPhase::Fill;
                        continue;
                    };
                    let at = pos + i as u64;
                    (self.at, self.phase) = (at, SyncPhase::Parse);
                    if at + mh > cx.win.end()
                        && cx.win.end() < h.end
                        && let Some(need) = cx.win.want(self.anchor.min(at), at + mh, h.end)?
                    {
                        return Ok(Step::Need(need));
                    }
                }
                SyncPhase::Parse => {
                    if let Some(hd) = header(&h.codec, cx.win.get(self.at, mh)) {
                        return Ok(Step::Done(Some((self.at, hd))));
                    }
                    (self.pos, self.phase) = (self.at + 1, SyncPhase::Fill);
                }
            }
        }
    }
}

/// Where the FLAC frame at `at` may end, the next header numbering `want` within `near` bytes
/// or `far` once walking on. Its answer outright when no search is needed.
fn bounds(h: &BareHead, at: u64, f: &Hdr) -> Result<(u64, u64, u64, u64), Option<u64>> {
    let Codec::Flac { si, .. } = h.codec else { return Err(None) };
    let start = f.sample.unwrap_or(0);
    let want = start + u64::from(f.samples);
    if si.total > 0 && want >= si.total {
        return Err(Some(h.end));
    }
    let near =
        at + if si.max_frame > 0 { (u64::from(si.max_frame) + 1).min(MAX_SCAN) } else { MAX_SCAN };
    Ok((start, want, near, at + MAX_SCAN))
}

/// Where the FLAC frame at `at` ends, the next header numbering the sample after it within a
/// frame's size, or the file's end for the last. None when nothing near ends it.
#[derive(Debug, Clone, Copy)]
struct Near {
    at: u64,
    want: u64,
    near: u64,
    sync: Sync,
    ready: Option<Option<u64>>,
}

impl Near {
    fn new(h: &BareHead, at: u64, f: &Hdr) -> Near {
        let (want, near, ready) = match bounds(h, at, f) {
            Ok((_, want, near, _)) => (want, near, None),
            Err(r) => (0, 0, Some(r)),
        };
        // a header, a subframe and the CRC-16 at least
        Near { at, want, near, sync: Sync::new(h, at, at + 9, near), ready }
    }

    fn step(&mut self, cx: &mut Cx<'_>) -> Go<Option<u64>> {
        if let Some(r) = self.ready {
            return Ok(Step::Done(r));
        }
        loop {
            match self.sync.step(cx)? {
                Step::Need(need) => return Ok(Step::Need(need)),
                Step::Done(Some((b, n))) if n.sample.unwrap_or(0) == self.want => {
                    return Ok(Step::Done(Some(b)));
                }
                Step::Done(Some((b, _))) => self.sync = Sync::new(cx.h, self.at, b + 1, self.near),
                // nothing ends it before the file does, it is the last
                Step::Done(None) => {
                    return Ok(Step::Done((self.near >= cx.h.end).then_some(cx.h.end)));
                }
            }
        }
    }
}

/// [`Near`] walking on. Past a lying max_frame the search goes on, and past a damaged header a
/// later one its own successor confirms ends it.
#[derive(Debug, Clone, Copy)]
struct End {
    at: u64,
    start: u64,
    want: u64,
    near: u64,
    far: u64,
    /// The search to `near` with exact numbers, to `far`, then to `far` confirmed.
    pass: u8,
    sync: Sync,
    /// A header past the damage, its byte and the search for its own end.
    confirm: Option<(u64, Near)>,
    ready: Option<Option<u64>>,
}

impl End {
    fn new(h: &BareHead, at: u64, f: &Hdr) -> End {
        let ((start, want, near, far), ready) = match bounds(h, at, f) {
            Ok(b) => (b, None),
            Err(r) => ((0, 0, 0, 0), Some(r)),
        };
        let sync = Sync::new(h, at, at + 9, near);
        End { at, start, want, near, far, pass: 0, sync, confirm: None, ready }
    }

    fn to(&self) -> u64 {
        if self.pass == 0 { self.near } else { self.far }
    }

    fn step(&mut self, cx: &mut Cx<'_>) -> Go<Option<u64>> {
        if let Some(r) = self.ready {
            return Ok(Step::Done(r));
        }
        loop {
            if let Some((b, near)) = &mut self.confirm {
                let b = *b;
                match near.step(cx)? {
                    Step::Need(need) => return Ok(Step::Need(need)),
                    Step::Done(Some(_)) => return Ok(Step::Done(Some(b))),
                    Step::Done(None) => {
                        self.confirm = None;
                        self.sync = Sync::new(cx.h, self.at, b + 1, self.to());
                    }
                }
            }
            match self.sync.step(cx)? {
                Step::Need(need) => return Ok(Step::Need(need)),
                Step::Done(Some((b, n))) => {
                    let s = n.sample.unwrap_or(0);
                    if s == self.want {
                        return Ok(Step::Done(Some(b)));
                    }
                    if self.pass == 2 && s > self.start {
                        self.confirm = Some((b, Near::new(cx.h, b, &n)));
                    } else {
                        self.sync = Sync::new(cx.h, self.at, b + 1, self.to());
                    }
                }
                Step::Done(None) if self.pass < 2 => {
                    self.pass += 1;
                    self.sync = Sync::new(cx.h, self.at, self.at + 9, self.to());
                }
                // nothing ends it before the file does, it is the last
                Step::Done(None) => {
                    return Ok(Step::Done((self.far >= cx.h.end).then_some(cx.h.end)));
                }
            }
        }
    }
}

/// What [`Resync`] checks a header it found with.
#[derive(Debug, Clone, Copy)]
enum Check {
    None,
    /// A FLAC header, by where its frame ends.
    Flac(u64, Hdr, Near),
    /// An MPEG audio or ADTS header, by the header behind its frame.
    Next(u64, Hdr, HdrAt),
}

/// A frame header at or after `from` and before `to` a successor confirms, its byte.
#[derive(Debug, Clone, Copy)]
struct Resync {
    to: u64,
    sync: Sync,
    check: Check,
}

impl Resync {
    fn new(h: &BareHead, from: u64, to: u64) -> Resync {
        let to = to.min(from.saturating_add(MAX_SCAN));
        Resync { to, sync: Sync::new(h, from, from, to), check: Check::None }
    }

    fn step(&mut self, cx: &mut Cx<'_>) -> Go<Option<(u64, Hdr)>> {
        loop {
            let checked = match &mut self.check {
                Check::None => None,
                Check::Flac(b, hd, near) => match near.step(cx)? {
                    Step::Need(need) => return Ok(Step::Need(need)),
                    Step::Done(end) => Some((*b, *hd, end.is_some())),
                },
                Check::Next(b, hd, next) => match next.step(cx)? {
                    Step::Need(need) => return Ok(Step::Need(need)),
                    Step::Done(next) => Some((*b, *hd, next.is_some())),
                },
            };
            if let Some((b, hd, ok)) = checked {
                if ok {
                    return Ok(Step::Done(Some((b, hd))));
                }
                self.check = Check::None;
                self.sync = Sync::new(cx.h, b + 1, b + 1, self.to);
            }
            let (b, hd) = match self.sync.step(cx)? {
                Step::Need(need) => return Ok(Step::Need(need)),
                Step::Done(None) => return Ok(Step::Done(None)),
                Step::Done(Some(found)) => found,
            };
            self.check = match cx.h.codec {
                Codec::Flac { .. } => Check::Flac(b, hd, Near::new(cx.h, b, &hd)),
                _ if b + hd.len + 4 > cx.h.end => return Ok(Step::Done(Some((b, hd)))),
                _ => Check::Next(b, hd, HdrAt::new(b + hd.len)),
            };
        }
    }
}

/// Where [`NextAt`] is.
#[derive(Debug, Clone, Copy)]
enum NextSt {
    /// At the reader's next frame.
    Top,
    /// Its header.
    Hdr(HdrAt),
    /// A header after the one that did not parse.
    Resync(Resync),
    /// The end of the FLAC frame at `at`.
    End(u64, Hdr, End),
    /// The frame at `at` held in the window to its end, its first sample `n`.
    Fill { at: u64, hd: Hdr, end: u64, n: u64 },
}

/// [`Reader::next`], the next unit read into the reader's window.
pub struct NextAt<'a> {
    rd: &'a mut Reader,
    h: &'a BareHead,
    st: NextSt,
}

impl NextAt<'_> {
    /// Holds the frame at `at` to `end` and the header behind it.
    fn fill(&mut self, at: u64, hd: Hdr, end: u64, n: u64) -> Result<Option<Need>, BareError> {
        self.st = NextSt::Fill { at, hd, end, n };
        self.rd.win.want(at, end + MAX_HEADER as u64, self.h.end)
    }

    fn step(&mut self) -> Go<Option<Unit>> {
        let h = self.h;
        loop {
            let mut cx = Cx { win: &mut self.rd.win, work: &mut self.rd.work, h };
            let need = match &mut self.st {
                NextSt::Top if self.rd.at >= h.end => return Ok(Step::Done(None)),
                NextSt::Top => {
                    self.st = NextSt::Hdr(HdrAt::new(self.rd.at));
                    None
                }
                NextSt::Hdr(x) => {
                    let at = x.at;
                    match x.step(&mut cx)? {
                        Step::Need(need) => Some(need),
                        Step::Done(None) => {
                            self.st = NextSt::Resync(Resync::new(h, at + 1, h.end));
                            None
                        }
                        Step::Done(Some(hd)) => match hd.sample {
                            Some(_) => {
                                self.st = NextSt::End(at, hd, End::new(h, at, &hd));
                                None
                            }
                            None if at + hd.len > h.end => return Ok(Step::Done(None)),
                            None => self.fill(at, hd, at + hd.len, self.rd.n)?,
                        },
                    }
                }
                NextSt::Resync(x) => match x.step(&mut cx)? {
                    Step::Need(need) => Some(need),
                    Step::Done(found) => {
                        self.rd.at = found.map_or(h.end, |(b, _)| b);
                        self.st = NextSt::Top;
                        None
                    }
                },
                NextSt::End(at, hd, x) => {
                    let (at, hd) = (*at, *hd);
                    match x.step(&mut cx)? {
                        Step::Need(need) => Some(need),
                        Step::Done(Some(e)) => self.fill(at, hd, e, hd.sample.unwrap_or(0))?,
                        Step::Done(None) => {
                            self.rd.at = at + 1;
                            self.st = NextSt::Top;
                            None
                        }
                    }
                }
                NextSt::Fill { at, hd, end, n } => {
                    let (at, hd, end, n) = (*at, *hd, *end, *n);
                    return self.unit(at, hd, end, n);
                }
            };
            if let Some(need) = need {
                return Ok(Step::Need(need));
            }
        }
    }

    /// The frame at `at`, held to its end.
    fn unit(&mut self, at: u64, hd: Hdr, end: u64, n: u64) -> Go<Option<Unit>> {
        let (rd, h) = (&mut *self.rd, self.h);
        if rd.win.end() < end {
            return Ok(Step::Done(None));
        }
        if rd.exact
            && rd.known.last().is_none_or(|k| n >= k.0 + u64::from(h.rate))
            && rd.known.len() < MAX_KNOWN
            && hd.sample.is_none()
        {
            rd.known.push((n, at));
        }
        let next = n + u64::from(hd.samples);
        let len = u32::try_from(end - at - hd.skip).map_err(|_| BareError::TooLarge)?;
        let dur_ns = u32::try_from(h.ns(next) - h.ns(n)).unwrap_or(u32::MAX);
        let u = Unit { off: at + hd.skip, len, pts_ns: h.ns(n), dur_ns };
        (rd.at, rd.n) = (end, next);
        Ok(Step::Done(Some(u)))
    }
}

/// Where a [`NextAt`] waits on its read, see [`Reader::resume_next`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct NextPark(NextSt);

impl NextAt<'_> {
    /// Its place, kept while the read of its last Need is in flight.
    pub(crate) fn park(self) -> NextPark {
        NextPark(self.st)
    }
}

impl Fetch for NextAt<'_> {
    type Out = Option<Unit>;
    type Err = BareError;

    fn poll(&mut self) -> Result<Step<Option<Unit>>, BareError> {
        let step = self.step();
        if !matches!(step, Ok(Step::Need(_))) {
            self.st = NextSt::Top;
            self.rd.work = MAX_WORK;
        }
        step
    }

    fn buf(&mut self) -> &mut [u8] {
        self.rd.win.hole()
    }

    fn filled(&mut self, n: usize) {
        self.rd.win.filled(n);
    }
}

/// Where [`SeekAt`] is.
#[derive(Debug, Clone, Copy)]
enum SeekSt {
    Start,
    /// FLAC, the `i`th bisection probe from `guess` in the bracket `lo` to `hi`.
    Bisect {
        i: u32,
        guess: u64,
        lo: (u64, u64),
        hi: (u64, u64),
        probe: Resync,
    },
    /// FLAC, the walk from the bracket's low end, the header at `at`.
    Walk {
        at: u64,
        hdr: HdrAt,
    },
    /// FLAC, a header after the one at `at` that did not parse.
    WalkResync {
        at: u64,
        probe: Resync,
    },
    /// FLAC, the end of the frame at `at`, its first sample `s`.
    WalkEnd {
        at: u64,
        s: u64,
        end: End,
    },
    /// A CBR stream, the frame at the line's guess.
    Line {
        num: u64,
        den: u64,
        probe: Resync,
    },
    /// A frame near a table point or an estimate, its first sample `n`.
    Land {
        n: u64,
        probe: Resync,
    },
    /// The headers walked to the target, the one at `at` of sample `n`.
    Count {
        n: u64,
        at: u64,
        exact: bool,
        hdr: HdrAt,
    },
    /// A header after the one that did not parse.
    CountResync {
        n: u64,
        exact: bool,
        probe: Resync,
    },
}

/// [`Reader::seek`]. A FLAC seek bisects the file on the samples its headers number, MPEG audio
/// and ADTS go by the line, a table point or an estimate, then walk headers to the target.
pub struct SeekAt<'a> {
    rd: &'a mut Reader,
    h: &'a BareHead,
    want: u64,
    st: SeekSt,
}

impl SeekAt<'_> {
    /// The next bisection probe, or the walk once the bracket is small.
    fn bisect(&mut self, i: u32, lo: (u64, u64), hi: (u64, u64)) {
        let want = self.want;
        // a bracket the points left upside down is walked, never bisected
        if i == 64 || hi.1.saturating_sub(lo.1) <= BISECT_STOP {
            self.rd.win.ahead = CHUNK;
            self.st = SeekSt::Walk { at: lo.1, hdr: HdrAt::new(lo.1) };
            return;
        }
        // interpolate on the samples, every other step halves so a skewed file still narrows
        let guess = match hi.0 {
            s if i.is_multiple_of(2) && s != u64::MAX && s > lo.0 => {
                lo.1 + ((u128::from(want - lo.0) * u128::from(hi.1 - lo.1)) / u128::from(s - lo.0))
                    as u64
            }
            _ => lo.1 + (hi.1 - lo.1) / 2,
        };
        let guess = guess.clamp(lo.1 + 1, hi.1 - 1);
        self.st = SeekSt::Bisect { i, guess, lo, hi, probe: Resync::new(self.h, guess, hi.1) };
    }

    /// The headers walked from `at` of sample `n` to the target frame.
    fn count(&mut self, n: u64, at: u64, exact: bool) -> Option<Step<()>> {
        let spf = u64::from(self.h.spf.max(1));
        if n + spf <= self.want && at < self.h.end {
            self.st = SeekSt::Count { n, at, exact, hdr: HdrAt::new(at) };
            return None;
        }
        (self.rd.at, self.rd.n, self.rd.exact) = (at, n, exact);
        Some(Step::Done(()))
    }

    fn start(&mut self) -> Option<Step<()>> {
        let (h, want) = (self.h, self.want);
        if let Codec::Flac { si, .. } = h.codec {
            let (mut lo, mut hi) =
                ((0u64, h.first), (if h.samples > 0 { h.samples } else { u64::MAX }, h.end));
            for &p in &h.points {
                if p.0 <= want {
                    lo = p;
                } else {
                    hi = p;
                    break;
                }
            }
            // a probe needs a frame and the header after it
            self.rd.win.ahead = if si.max_frame > 0 {
                (2 * u64::from(si.max_frame) + 64).min(CHUNK)
            } else {
                CHUNK
            };
            self.bisect(0, lo, hi);
            return None;
        }
        let spf = u64::from(h.spf.max(1));
        let k = want / spf;
        if let Some((num, den)) = h.cbr {
            let along = u64::try_from(u128::from(k) * u128::from(num) / u128::from(den.max(1)))
                .unwrap_or(u64::MAX);
            let guess = h.first.saturating_add(along).min(h.end);
            let probe = Resync::new(h, guess.saturating_sub(2).max(h.first), h.end);
            self.st = SeekSt::Line { num, den, probe };
            return None;
        }
        let before = |list: &[(u64, u64)]| list.iter().rev().find(|p| p.0 <= want).copied();
        let counted = before(&self.rd.known).unwrap_or((0, h.first));
        let near = want - counted.0 <= WALK_SECS * u64::from(h.rate);
        let table = before(&h.points).filter(|t| t.0 > counted.0);
        let (n, at) = match (near, table) {
            (false, Some(t)) => (t.0, t.1),
            (false, None) if h.samples > 0 && h.end > h.first => {
                let b = h.first
                    + (u128::from(want) * u128::from(h.end - h.first) / u128::from(h.samples))
                        as u64;
                (want / spf * spf, b)
            }
            _ => return self.count(counted.0, counted.1, true),
        };
        self.st = SeekSt::Land { n, probe: Resync::new(h, at, h.end) };
        None
    }

    /// A FLAC seek's next step, the read it waits on or its end, None to go on.
    fn flac(&mut self) -> Result<Option<Step<()>>, BareError> {
        let (h, want) = (self.h, self.want);
        let mut cx = Cx { win: &mut self.rd.win, work: &mut self.rd.work, h };
        match &mut self.st {
            SeekSt::Bisect { i, guess, lo, hi, probe } => {
                let (i, guess, mut lo, mut hi) = (*i, *guess, *lo, *hi);
                match probe.step(&mut cx)? {
                    Step::Need(need) => return Ok(Some(Step::Need(need))),
                    Step::Done(Some((b, hd))) if hd.sample.unwrap_or(0) <= want => {
                        lo = (hd.sample.unwrap_or(0), b);
                    }
                    Step::Done(Some((b, hd))) => hi = (hd.sample.unwrap_or(u64::MAX), b),
                    Step::Done(None) => hi.1 = guess,
                }
                self.bisect(i + 1, lo, hi);
            }
            SeekSt::Walk { at, hdr } => {
                let at = *at;
                match hdr.step(&mut cx)? {
                    Step::Need(need) => return Ok(Some(Step::Need(need))),
                    Step::Done(None) => {
                        self.st = SeekSt::WalkResync { at, probe: Resync::new(h, at + 1, h.end) };
                    }
                    Step::Done(Some(hd)) => {
                        let s = hd.sample.unwrap_or(0);
                        if s + u64::from(hd.samples) > want {
                            (self.rd.at, self.rd.n, self.rd.exact) = (at, s, true);
                            return Ok(Some(Step::Done(())));
                        }
                        self.st = SeekSt::WalkEnd { at, s, end: End::new(h, at, &hd) };
                    }
                }
            }
            SeekSt::WalkResync { at, probe } => {
                let at = *at;
                match probe.step(&mut cx)? {
                    Step::Need(need) => return Ok(Some(Step::Need(need))),
                    Step::Done(Some((b, _))) => {
                        self.st = SeekSt::Walk { at: b, hdr: HdrAt::new(b) }
                    }
                    Step::Done(None) => {
                        (self.rd.at, self.rd.exact) = (at, true);
                        return Ok(Some(Step::Done(())));
                    }
                }
            }
            SeekSt::WalkEnd { at, s, end } => {
                let (at, s) = (*at, *s);
                match end.step(&mut cx)? {
                    Step::Need(need) => return Ok(Some(Step::Need(need))),
                    Step::Done(Some(e)) if e < h.end => {
                        self.st = SeekSt::Walk { at: e, hdr: HdrAt::new(e) };
                    }
                    Step::Done(_) => {
                        (self.rd.at, self.rd.n, self.rd.exact) = (at, s, true);
                        return Ok(Some(Step::Done(())));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    /// An MPEG audio or ADTS seek's next step, the read it waits on or its end, None to go on.
    fn frames(&mut self) -> Result<Option<Step<()>>, BareError> {
        let h = self.h;
        let mut cx = Cx { win: &mut self.rd.win, work: &mut self.rd.work, h };
        match &mut self.st {
            SeekSt::Line { num, den, probe } => {
                let (num, den) = (*num, *den);
                let b = match probe.step(&mut cx)? {
                    Step::Need(need) => return Ok(Some(Step::Need(need))),
                    Step::Done(found) => found.map_or(h.end, |(b, _)| b),
                };
                let spf = u64::from(h.spf.max(1));
                let along = u128::from(b.saturating_sub(h.first)) * u128::from(den);
                let k = ((along + u128::from(num) / 2) / u128::from(num.max(1))) as u64;
                (self.rd.at, self.rd.n, self.rd.exact) = (b, k * spf, true);
                Ok(Some(Step::Done(())))
            }
            SeekSt::Land { n, probe } => {
                let n = *n;
                match probe.step(&mut cx)? {
                    Step::Need(need) => Ok(Some(Step::Need(need))),
                    Step::Done(found) => Ok(self.count(n, found.map_or(h.end, |(b, _)| b), false)),
                }
            }
            SeekSt::Count { n, at, exact, hdr } => {
                let (n, at, exact) = (*n, *at, *exact);
                let hd = match hdr.step(&mut cx)? {
                    Step::Need(need) => return Ok(Some(Step::Need(need))),
                    Step::Done(Some(hd)) => hd,
                    Step::Done(None) => {
                        let probe = Resync::new(h, at + 1, h.end);
                        self.st = SeekSt::CountResync { n, exact, probe };
                        return Ok(None);
                    }
                };
                let rd = &mut *self.rd;
                if exact
                    && rd.known.last().is_none_or(|p| n >= p.0 + u64::from(h.rate))
                    && rd.known.len() < MAX_KNOWN
                {
                    rd.known.push((n, at));
                }
                Ok(self.count(n + u64::from(hd.samples), at + hd.len, exact))
            }
            SeekSt::CountResync { n, exact, probe } => {
                let (n, exact) = (*n, *exact);
                match probe.step(&mut cx)? {
                    Step::Need(need) => Ok(Some(Step::Need(need))),
                    Step::Done(Some((b, _))) => Ok(self.count(n, b, exact)),
                    Step::Done(None) => {
                        (self.rd.at, self.rd.n, self.rd.exact) = (h.end, n, exact);
                        Ok(Some(Step::Done(())))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    fn step(&mut self) -> Go<()> {
        loop {
            let step = match self.st {
                SeekSt::Start => self.start(),
                SeekSt::Bisect { .. }
                | SeekSt::Walk { .. }
                | SeekSt::WalkResync { .. }
                | SeekSt::WalkEnd { .. } => self.flac()?,
                SeekSt::Line { .. }
                | SeekSt::Land { .. }
                | SeekSt::Count { .. }
                | SeekSt::CountResync { .. } => self.frames()?,
            };
            if let Some(step) = step {
                return Ok(step);
            }
        }
    }
}

/// Where a [`SeekAt`] waits on its read, see [`Reader::resume_seek`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SeekPark {
    want: u64,
    st: SeekSt,
}

impl SeekAt<'_> {
    pub(crate) fn park(self) -> SeekPark {
        SeekPark { want: self.want, st: self.st }
    }
}

impl Fetch for SeekAt<'_> {
    type Out = ();
    type Err = BareError;

    fn poll(&mut self) -> Result<Step<()>, BareError> {
        let step = self.step();
        if !matches!(step, Ok(Step::Need(_))) {
            self.st = SeekSt::Start;
            self.rd.win.ahead = CHUNK;
            self.rd.work = MAX_WORK;
        }
        step
    }

    fn buf(&mut self) -> &mut [u8] {
        self.rd.win.hole()
    }

    fn filled(&mut self, n: usize) {
        self.rd.win.filled(n);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::fixture::flac_file;
    use crate::head::{BareKind, OpenAt};
    use profluens_core::fetch::{drive_slice, drive_slice_log};

    #[test]
    fn adts_headers() {
        // AAC LC, 48 kHz, stereo, 371 bytes, one block, no CRC
        let b = [0xFF, 0xF1, 0x4C, 0x80, 0x2E, 0x7F, 0xFC];
        assert_eq!(adts(&b), Some((1, 3, 2, 371, 7, 1024)));
        assert_eq!(adts(&[0xFF, 0xF1, 0x7C, 0x80, 0x2E, 0x7F, 0xFC]), None, "frequency index 15");
        let c = Codec::Adts { profile: 1, sfi: 3, channels: 2 };
        assert!(header(&c, &b).is_some());
        assert!(
            header(&Codec::Adts { profile: 1, sfi: 4, channels: 2 }, &b).is_none(),
            "another stream"
        );
    }

    /// Holds `[from, to)` of `file` in `w`, the read it took.
    fn fill(w: &mut Win, file: &[u8], from: u64, to: u64) -> Option<Need> {
        let need = w.want(from, to, file.len() as u64).unwrap()?;
        let at = (need.at as usize).min(file.len());
        let n = need.len.min(file.len() - at);
        w.hole()[..n].copy_from_slice(&file[at..at + n]);
        w.filled(n);
        Some(need)
    }

    #[test]
    fn a_window_reads_ahead_and_keeps_the_anchor() {
        let file: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        let mut w = Win::default();
        let need = fill(&mut w, &file, 1000, 1010);
        assert_eq!(need, Some(Need { at: 1000, len: CHUNK as usize }));
        assert_eq!((w.at, w.end()), (1000, 1000 + CHUNK));
        let need = fill(&mut w, &file, 1500, 1000 + CHUNK + 10);
        assert_eq!(need, Some(Need { at: 1000 + CHUNK, len: 500 }), "only what it lacks");
        assert_eq!(w.at, 1500, "the bytes before from let go");
        assert_eq!(w.get(1500, 4), &file[1500..1504]);
        assert_eq!(fill(&mut w, &file, 1600, 1700), None, "held, nothing read");
        fill(&mut w, &file, 399_990, 400_100);
        assert_eq!(w.get(399_990, 100), &file[399_990..], "short at the end");
    }

    #[test]
    fn a_fill_dropped_half_way_lets_go_of_its_bytes() {
        let file: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        let mut w = Win::default();
        fill(&mut w, &file, 0, 10);
        assert!(w.want(CHUNK - 10, CHUNK + 10, file.len() as u64).unwrap().is_some());
        w.ahead = 77;
        w.settle();
        assert_eq!((w.at, w.end(), w.ahead), (CHUNK - 10, CHUNK, CHUNK), "the bytes it had");
        assert_eq!(w.get(CHUNK - 10, 10), &file[CHUNK as usize - 10..CHUNK as usize]);
    }

    /// Answers `need` from `file` into `buf`, short past its end.
    fn answer(file: &[u8], need: Need, buf: &mut [u8]) -> usize {
        let at = (need.at as usize).min(file.len());
        let n = need.len.min(file.len() - at);
        buf[..n].copy_from_slice(&file[at..at + n]);
        n
    }

    #[test]
    fn a_machine_parked_on_every_read_goes_on_where_it_stopped() {
        let f = flac_file(400, 1000, &[], false);
        let file = &f.bytes[..];
        let (h, _) =
            drive_slice(&mut OpenAt::new(file.len() as u64, BareKind::Flac), file).unwrap();
        let (mut a, mut b) = (Reader::new(&h), Reader::new(&h));
        for ns in [h.ns(300 * 4096), 0, h.ns(123 * 4096 + 7), h.ns(399 * 4096)] {
            let (mut got, mut want) = (Vec::new(), Vec::new());
            let mut m = a.seek(&h, ns);
            while let Step::Need(need) = m.poll().unwrap() {
                got.push(need);
                let p = m.park();
                m = a.resume_seek(&h, p);
                let n = answer(file, need, m.buf());
                m.filled(n);
            }
            drive_slice_log(&mut b.seek(&h, ns), file, &mut want).unwrap();
            assert_eq!(got, want, "the same probes at {ns}");
            for _ in 0..40 {
                let mut m = a.next(&h);
                let u = loop {
                    match m.poll().unwrap() {
                        Step::Done(u) => break u,
                        Step::Need(need) => {
                            let p = m.park();
                            m = a.resume_next(&h, p);
                            let n = answer(file, need, m.buf());
                            m.filled(n);
                        }
                    }
                };
                let w = drive_slice(&mut b.next(&h), file).unwrap();
                assert_eq!(u, w);
                if let Some(u) = u {
                    assert_eq!(a.bytes(&u), b.bytes(&u));
                }
            }
        }
    }

    #[test]
    fn flac_frames_come_out_whole_and_exact() {
        let f = flac_file(30, 1000, &[], false);
        let (h, _) =
            drive_slice(&mut OpenAt::new(f.bytes.len() as u64, BareKind::Flac), &f.bytes).unwrap();
        let mut rd = Reader::new(&h);
        let mut got = Vec::new();
        let mut log = Vec::new();
        while let Some(u) = drive_slice_log(&mut rd.next(&h), &f.bytes, &mut log).unwrap() {
            assert_eq!(rd.bytes(&u), &f.bytes[u.off as usize..(u.off + u64::from(u.len)) as usize]);
            got.push((u.off, u.len, u.pts_ns));
        }
        let want: Vec<(u64, u32, i64)> = f.frames.iter().map(|x| (x.0, x.1, h.ns(x.2))).collect();
        assert_eq!(got, want);
        let ends: Vec<u64> = log.iter().map(|n| n.at + n.len as u64).collect();
        assert!(ends.windows(2).all(|w| w[0] <= w[1]), "read forward once, {log:?}");
        // one machine reads on unit after unit
        let mut rd = Reader::new(&h);
        let mut m = rd.next(&h);
        let units: Vec<_> = (0..30).map(|_| drive_slice(&mut m, &f.bytes).unwrap()).collect();
        assert!(
            units.iter().zip(&want).all(|(u, w)| u.map(|u| (u.off, u.len, u.pts_ns)) == Some(*w))
        );
        assert_eq!(drive_slice(&mut m, &f.bytes), Ok(None));
    }
}
