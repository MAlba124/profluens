//! The links of a chained Ogg file (RFC 3533 section 4), found by bisecting for where each link's
//! serials end, so a duration spans every link and a seek knows the link its time falls in.

use std::convert::Infallible;

use profluens_core::fetch::{Fetch, Need, Step as FetchStep};

use crate::duration::{
    GranuleClock, OGG_TAIL_LEN, TAIL_CRC_MAX, granule_clock, stream_duration_ns,
};
use crate::page::{CAPTURE_PATTERN, MAX_PAGE_SIZE, PageError, PageHeader};

/// The bytes read at a link's start for its bos pages, and of a probe, two pages of the largest
/// size, so the first page starting in it is whole.
const READ_LEN: u64 = 2 * MAX_PAGE_SIZE as u64;
/// Links found at most, a file of more is left untimed.
pub const LINKS_MAX: usize = 1024;
/// Reads a scan takes at most, about 20 for each of `LINKS_MAX` links.
const READS_MAX: u32 = 20 * LINKS_MAX as u32;
/// Serials a link's group holds at most.
const SERIALS_MAX: usize = 32;

/// One link, its group of logical bitstreams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The byte its first bos page starts at.
    pub at: u64,
    /// The serials of its group.
    pub serials: Vec<u32>,
    /// Its first stream with a granule clock, the one its time is read from.
    pub serial: u32,
    pub clock: GranuleClock,
    /// The stream time of the links before it, ns.
    pub start_ns: u64,
    /// Its own stream time, from its last granule of `serial`, ns.
    pub ns: u64,
}

/// The links of a file `len` bytes long, in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Links {
    pub links: Vec<Link>,
    pub len: u64,
}

impl Links {
    /// The stream time of every link, ns.
    pub fn total_ns(&self) -> u64 {
        self.links.last().map_or(0, |l| l.start_ns.saturating_add(l.ns))
    }

    /// The link stream time `ns` falls in, the last for one past the end.
    pub fn at_time(&self, ns: u64) -> usize {
        self.links.iter().rposition(|l| l.start_ns <= ns).unwrap_or(0)
    }

    /// The link of the stream `serial`.
    pub fn of_serial(&self, serial: u32) -> Option<usize> {
        self.links.iter().position(|l| l.serials.contains(&serial))
    }

    /// The byte link `i` ends at, where the next starts.
    pub fn end(&self, i: usize) -> u64 {
        self.links.get(i + 1).map_or(self.len, |l| l.at)
    }
}

/// What a scan wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Read { at: u64, len: u32 },
    Done,
}

/// The link being read.
#[derive(Clone, Debug)]
struct Cur {
    at: u64,
    serials: Vec<u32>,
    serial: u32,
    clock: GranuleClock,
    ident: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The file's last bytes, for the last page's serial.
    Tail,
    /// The bos pages of the link starting at `at`.
    Head {
        at: u64,
    },
    /// The first page of another link is past `lo`, a page boundary in the link, and at or
    /// before `hi`, the start of such a page or the file's end. `near` reads the pages right
    /// before `hi`, a probe having found none starting between it and `hi`.
    Search {
        lo: u64,
        hi: u64,
        near: bool,
    },
    /// The pages from `lo` on, read one by one up to the next link's first.
    Walk {
        lo: u64,
        hi: u64,
    },
    /// The bytes before `next`, for the link's last granule.
    End {
        next: u64,
    },
    Done,
}

/// A search for the links of a file read by range. Poll [`ChainScan::step`], answer each read
/// with [`ChainScan::served`]. Any page it cannot make sense of leaves the file untimed.
#[derive(Clone, Debug)]
pub struct ChainScan {
    len: u64,
    tail: Vec<u8>,
    last_serial: Option<u32>,
    phase: Phase,
    cur: Option<Cur>,
    links: Vec<Link>,
    reads: u32,
    failed: bool,
    /// The read buffer of the [`Fetch`] interface.
    buf: Vec<u8>,
}

impl ChainScan {
    /// The scan of a file `len` bytes long, its tail read first.
    // Cold, once per file.
    #[allow(clippy::disallowed_methods)]
    pub fn new(len: u64) -> ChainScan {
        ChainScan {
            len,
            tail: Vec::new(),
            last_serial: None,
            phase: Phase::Tail,
            cur: None,
            links: Vec::new(),
            reads: 0,
            failed: false,
            buf: Vec::new(),
        }
    }

    /// The scan of a file `len` bytes long whose last bytes, read already, are `tail`.
    pub fn with_tail(len: u64, tail: &[u8]) -> ChainScan {
        let mut s = ChainScan::new(len);
        s.take_tail(tail);
        s
    }

    pub fn step(&self) -> Step {
        let (at, len) = match self.phase {
            Phase::Done => return Step::Done,
            Phase::Tail => (self.len.saturating_sub(OGG_TAIL_LEN as u64), OGG_TAIL_LEN as u64),
            Phase::Head { at } => (at, READ_LEN),
            Phase::Search { lo, hi, near: false } => (lo + (hi - lo) / 2, READ_LEN),
            Phase::Search { lo, hi, near: true } => {
                let at = hi.saturating_sub(READ_LEN).max(lo);
                (at, hi - at)
            }
            Phase::Walk { lo, hi } => (lo, hi - lo + MAX_PAGE_SIZE as u64),
            Phase::End { next } => {
                let from = next.saturating_sub(OGG_TAIL_LEN as u64);
                let from = from.max(self.cur.as_ref().map_or(0, |c| c.at));
                (from, next - from)
            }
        };
        match len.min(self.len.saturating_sub(at)) {
            0 => Step::Done,
            n => Step::Read { at, len: n as u32 },
        }
    }

    /// The links, `None` for a file the scan could not time.
    pub fn links(&self) -> Option<Links> {
        let done = self.phase == Phase::Done && !self.failed && !self.links.is_empty();
        done.then(|| Links { links: self.links.clone(), len: self.len })
    }

    /// The bytes read for the last [`Step::Read`], fewer only at the end of the file.
    pub fn served(&mut self, data: &[u8]) {
        let Step::Read { at, .. } = self.step() else {
            return;
        };
        self.reads += 1;
        if self.reads > READS_MAX {
            return self.fail();
        }
        match self.phase {
            Phase::Tail => {
                self.take_tail(data);
            }
            Phase::Head { at } => self.head(at, data),
            Phase::Search { lo, hi, near } => self.probe(at, data, lo, hi, near),
            Phase::Walk { lo, .. } => self.walk(lo, data),
            Phase::End { next } => self.end(next, data),
            Phase::Done => {}
        }
    }

    fn fail(&mut self) {
        (self.failed, self.phase) = (true, Phase::Done);
    }

    // Cold, once per scan, the tail kept for the last link's end.
    #[allow(clippy::disallowed_methods)]
    fn take_tail(&mut self, tail: &[u8]) {
        self.last_serial = last_page_serial(tail);
        self.tail = tail.to_vec();
        self.phase = match self.last_serial {
            Some(_) => Phase::Head { at: 0 },
            None => {
                self.failed = true;
                Phase::Done
            }
        };
    }

    // Cold, once per link.
    #[allow(clippy::disallowed_methods)]
    fn head(&mut self, at: u64, data: &[u8]) {
        let mut serials = Vec::new();
        let mut key: Option<(u32, GranuleClock, Vec<u8>)> = None;
        let mut from = 0;
        let mut group_end = None;
        let mut first = true;
        while let Some((c, page)) = page_from(data, from) {
            // a boundary is a page start, the file's own start may have junk ahead
            if first && at > 0 && c != 0 {
                break;
            }
            first = false;
            if !page.is_bos() {
                group_end = Some(at + c as u64);
                break;
            }
            if serials.len() < SERIALS_MAX {
                serials.push(page.serial());
            }
            let ident = first_packet(&page);
            if key.is_none()
                && let Some(clock) = granule_clock(ident)
            {
                key = Some((page.serial(), clock, ident.to_vec()));
            }
            from = c + page.len();
        }
        let (Some(group_end), Some((serial, clock, ident))) = (group_end, key) else {
            return self.fail();
        };
        let link_at = if at == 0 { group_start(data) } else { at };
        self.cur = Some(Cur { at: link_at, serials, serial, clock, ident });
        let last = self
            .last_serial
            .is_some_and(|s| self.cur.as_ref().is_some_and(|c| c.serials.contains(&s)));
        match last {
            true => {
                let tail = std::mem::take(&mut self.tail);
                self.close(&tail);
                self.phase = Phase::Done;
            }
            false => self.search(group_end, self.len),
        }
    }

    fn search(&mut self, lo: u64, hi: u64) {
        self.phase = match hi.saturating_sub(lo) <= READ_LEN {
            true => Phase::Walk { lo, hi },
            false => Phase::Search { lo, hi, near: false },
        };
    }

    fn member(&self, serial: u32) -> bool {
        self.cur.as_ref().is_some_and(|c| c.serials.contains(&serial))
    }

    fn probe(&mut self, at: u64, data: &[u8], mut lo: u64, mut hi: u64, near: bool) {
        let mut from = 0;
        let mut found = false;
        while let Some((c, page)) = page_from(data, from) {
            let start = at + c as u64;
            if start >= hi {
                break;
            }
            found = true;
            if !self.member(page.serial()) {
                hi = start;
                break;
            }
            lo = lo.max(start + page.len() as u64);
            from = c + page.len();
        }
        match (found, near) {
            // the page holding the probe's start runs on to `hi`
            (false, false) => self.phase = Phase::Search { lo, hi, near: true },
            (false, true) if lo < hi => self.fail(),
            _ => self.search(lo, hi),
        }
    }

    fn walk(&mut self, lo: u64, data: &[u8]) {
        let mut from = 0;
        while let Some((c, page)) = page_from(data, from) {
            if !self.member(page.serial()) {
                return match page.is_bos() {
                    true => self.phase = Phase::End { next: lo + c as u64 },
                    false => self.fail(),
                };
            }
            from = c + page.len();
        }
        self.fail();
    }

    fn end(&mut self, next: u64, data: &[u8]) {
        if !self.close(data) {
            return self.fail();
        }
        self.phase = match self.links.len() < LINKS_MAX {
            true => Phase::Head { at: next },
            false => {
                self.failed = true;
                Phase::Done
            }
        };
    }

    /// Ends the link being read on the last granule of its key stream in `data`.
    fn close(&mut self, data: &[u8]) -> bool {
        let Some(cur) = self.cur.take() else {
            return false;
        };
        let Some(ns) = stream_duration_ns(&cur.ident, data, cur.serial) else {
            self.failed = true;
            return false;
        };
        let start_ns = self.links.last().map_or(0, |l| l.start_ns.saturating_add(l.ns));
        let Cur { at, serials, serial, clock, .. } = cur;
        self.links.push(Link { at, serials, serial, clock, start_ns, ns });
        true
    }
}

impl Fetch for ChainScan {
    type Out = Option<Links>;
    type Err = Infallible;

    fn poll(&mut self) -> Result<FetchStep<Option<Links>>, Infallible> {
        Ok(match self.step() {
            Step::Read { at, len } => {
                self.buf.resize(len as usize, 0);
                FetchStep::Need(Need { at, len: len as usize })
            }
            Step::Done => FetchStep::Done(self.links()),
        })
    }

    fn buf(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    fn filled(&mut self, n: usize) {
        let buf = std::mem::take(&mut self.buf);
        self.served(&buf[..n.min(buf.len())]);
        self.buf = buf;
    }
}

/// The first whole page at or after `from`, and where it starts.
fn page_from(data: &[u8], mut from: usize) -> Option<(usize, PageHeader<'_>)> {
    loop {
        let rest = data.get(from..)?;
        let c = from + rest.windows(CAPTURE_PATTERN.len()).position(|w| w == CAPTURE_PATTERN)?;
        match PageHeader::parse(&data[c..]) {
            Ok(page) => return Some((c, page)),
            Err(_) => from = c + 1,
        }
    }
}

/// Where the first page of `data` starts.
fn group_start(data: &[u8]) -> u64 {
    page_from(data, 0).map_or(0, |(c, _)| c as u64)
}

/// The first packet of a page, the identification packet on a bos page.
fn first_packet<'a>(page: &PageHeader<'a>) -> &'a [u8] {
    let table = page.segment_table();
    let n = table.iter().position(|&l| l < 255).map_or(table.len(), |i| i + 1);
    let len: usize = table[..n].iter().map(|&l| usize::from(l)).sum();
    &page.payload()[..len]
}

/// The serial of the last whole page in `tail`.
pub fn last_page_serial(tail: &[u8]) -> Option<u32> {
    let mut at = tail.len().checked_sub(CAPTURE_PATTERN.len())?;
    let mut spent = 0usize;
    loop {
        if tail[at..].starts_with(&CAPTURE_PATTERN) {
            match PageHeader::parse(&tail[at..]) {
                Ok(page) => return Some(page.serial()),
                Err(PageError::BadCrc { len, .. }) => {
                    spent += len;
                    if spent > TAIL_CRC_MAX {
                        return None;
                    }
                }
                Err(_) => {}
            }
        }
        at = at.checked_sub(1)?;
    }
}

#[cfg(test)]
mod tests;
