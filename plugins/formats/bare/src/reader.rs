//! One session's position in a bare file. The frames are found as the session reads forward,
//! through the caller's callback into one reused window. A FLAC frame header numbers its
//! samples, so a FLAC seek bisects the file on them and lands exactly. MPEG audio and ADTS
//! count frames from the first, a CBR stream seeks on its line, a VBR one on the Xing table or
//! by bytes, then walks frame headers to the target, exact once it walked from a point it
//! counted to.

use crate::flac::{self, MAX_HEADER};
use crate::head::{BareHead, Codec, adts};
use crate::{BareError, ReadAt};
use pf_mp3::frame::Header as MpaHeader;

/// Read ahead at a time.
const CHUNK: u64 = 128 << 10;
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
}

impl Default for Win {
    // Cold, once per reader.
    #[allow(clippy::disallowed_methods)]
    fn default() -> Self {
        Win { buf: Vec::new(), at: 0, ahead: CHUNK }
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
    /// `from` let go.
    fn fill(
        &mut self,
        read: &mut ReadAt<'_>,
        from: u64,
        to: u64,
        end: u64,
    ) -> Result<(), BareError> {
        let to = to.min(end);
        if from >= to || (self.at <= from && to <= self.end()) {
            return Ok(());
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
        let mut got = have;
        while got < self.buf.len() {
            let n = read(from + got as u64, &mut self.buf[got..])
                .map_err(|e| BareError::Io(e.kind()))?;
            if n == 0 {
                break;
            }
            got += n.min(self.buf.len() - got);
        }
        self.buf.truncate(got);
        Ok(())
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

    fn hdr_at(
        &mut self,
        h: &BareHead,
        read: &mut ReadAt<'_>,
        at: u64,
    ) -> Result<Option<Hdr>, BareError> {
        self.win.fill(read, at, at + MAX_HEADER as u64, h.end)?;
        Ok(header(&h.codec, self.win.get(at, MAX_HEADER as u64)))
    }

    /// The next sync at or after `from` and before `to` whose header parses, the window kept
    /// from `anchor` on.
    fn sync(
        &mut self,
        h: &BareHead,
        read: &mut ReadAt<'_>,
        anchor: u64,
        from: u64,
        to: u64,
    ) -> Result<Option<(u64, Hdr)>, BareError> {
        let to = to.min(h.end);
        let mut pos = from;
        while pos + 1 < to {
            let held = self.win.at <= pos && pos + MAX_HEADER as u64 <= self.win.end();
            if !held && (pos < self.win.at || self.win.end() < h.end) {
                let ahead = (pos + self.win.ahead).min(to + MAX_HEADER as u64);
                self.win.fill(read, anchor.min(pos), ahead.max(pos + MAX_HEADER as u64), h.end)?;
            }
            let hay = self.win.get(pos, to - pos);
            if hay.len() < 2 {
                return Ok(None);
            }
            let found = hay.windows(2).position(|w| is_sync(&h.codec, w[0], w[1]));
            let spent = found.map_or(hay.len() as u64, |i| i as u64 + PARSE_COST);
            self.work = self.work.checked_sub(spent).ok_or(BareError::Malformed(pos))?;
            let Some(i) = found else {
                // the last byte may be a sync's first
                pos += hay.len() as u64 - 1;
                continue;
            };
            let at = pos + i as u64;
            if at + MAX_HEADER as u64 > self.win.end() && self.win.end() < h.end {
                self.win.fill(read, anchor.min(at), at + MAX_HEADER as u64, h.end)?;
            }
            if let Some(hd) = header(&h.codec, self.win.get(at, MAX_HEADER as u64)) {
                return Ok(Some((at, hd)));
            }
            pos = at + 1;
        }
        Ok(None)
    }

    /// Where the FLAC frame at `at` ends, the next header numbering the sample after it within
    /// a frame's size, or the file's end for the last. Walking on, past a lying max_frame the
    /// search goes on, and past a damaged header a later one its own successor confirms ends it.
    /// None when nothing near ends it.
    fn flac_end(
        &mut self,
        h: &BareHead,
        read: &mut ReadAt<'_>,
        at: u64,
        f: &Hdr,
        walk: bool,
    ) -> Result<Option<u64>, BareError> {
        let Codec::Flac { si, .. } = h.codec else { return Ok(None) };
        let start = f.sample.unwrap_or(0);
        let want = start + u64::from(f.samples);
        if si.total > 0 && want >= si.total {
            return Ok(Some(h.end));
        }
        // a header, a subframe and the CRC-16 at least
        let from = at + 9;
        let near = at
            + if si.max_frame > 0 { (u64::from(si.max_frame) + 1).min(MAX_SCAN) } else { MAX_SCAN };
        let far = at + MAX_SCAN;
        let passes: &[(u64, bool)] =
            if walk { &[(near, true), (far, true), (far, false)] } else { &[(near, true)] };
        for &(to, exact) in passes {
            let mut pos = from;
            while let Some((b, n)) = self.sync(h, read, at, pos, to)? {
                let s = n.sample.unwrap_or(0);
                if s == want
                    || (!exact && s > start && self.flac_end(h, read, b, &n, false)?.is_some())
                {
                    return Ok(Some(b));
                }
                pos = b + 1;
            }
        }
        // nothing ends it before the file does, it is the last
        let reach = if walk { far } else { near };
        Ok((reach >= h.end).then_some(h.end))
    }

    /// A frame header at or after `from` and before `to` a successor confirms, its byte.
    fn resync(
        &mut self,
        h: &BareHead,
        read: &mut ReadAt<'_>,
        from: u64,
        to: u64,
    ) -> Result<Option<(u64, Hdr)>, BareError> {
        let mut pos = from;
        let to = to.min(from.saturating_add(MAX_SCAN));
        while let Some((b, hd)) = self.sync(h, read, from.max(pos), pos, to)? {
            let ok = match h.codec {
                Codec::Flac { .. } => self.flac_end(h, read, b, &hd, false)?.is_some(),
                _ => {
                    let next = b + hd.len;
                    next + 4 > h.end || self.hdr_at(h, read, next)?.is_some()
                }
            };
            if ok {
                return Ok(Some((b, hd)));
            }
            pos = b + 1;
        }
        Ok(None)
    }

    /// The next unit, None at the end.
    pub fn next(&mut self, h: &BareHead, read: &mut ReadAt<'_>) -> Result<Option<Unit>, BareError> {
        self.work = MAX_WORK;
        loop {
            if self.at >= h.end {
                return Ok(None);
            }
            let at = self.at;
            let Some(hd) = self.hdr_at(h, read, at)? else {
                match self.resync(h, read, at + 1, h.end)? {
                    Some((b, _)) => self.at = b,
                    None => self.at = h.end,
                }
                continue;
            };
            let (end, n) = match hd.sample {
                Some(s) => match self.flac_end(h, read, at, &hd, true)? {
                    Some(e) => (e, s),
                    None => {
                        self.at = at + 1;
                        continue;
                    }
                },
                None if at + hd.len > h.end => return Ok(None),
                None => (at + hd.len, self.n),
            };
            self.win.fill(read, at, end + MAX_HEADER as u64, h.end)?;
            if self.win.end() < end {
                return Ok(None);
            }
            if self.exact
                && self.known.last().is_none_or(|k| n >= k.0 + u64::from(h.rate))
                && self.known.len() < MAX_KNOWN
                && hd.sample.is_none()
            {
                self.known.push((n, at));
            }
            let next = n + u64::from(hd.samples);
            let len = u32::try_from(end - at - hd.skip).map_err(|_| BareError::TooLarge)?;
            let dur_ns = u32::try_from(h.ns(next) - h.ns(n)).unwrap_or(u32::MAX);
            let u = Unit { off: at + hd.skip, len, pts_ns: h.ns(n), dur_ns };
            (self.at, self.n) = (end, next);
            return Ok(Some(u));
        }
    }

    /// To the frame that holds `ns`, the last one past the end.
    pub fn seek(&mut self, h: &BareHead, read: &mut ReadAt<'_>, ns: i64) -> Result<(), BareError> {
        self.work = MAX_WORK;
        let mut want = h.sample_at(ns);
        if h.samples > 0 {
            want = want.min(h.samples - 1);
        }
        match h.codec {
            Codec::Flac { .. } => self.seek_flac(h, read, want),
            _ => self.seek_frames(h, read, want),
        }
    }

    fn seek_flac(
        &mut self,
        h: &BareHead,
        read: &mut ReadAt<'_>,
        want: u64,
    ) -> Result<(), BareError> {
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
        let Codec::Flac { si, .. } = h.codec else { return Ok(()) };
        self.win.ahead =
            if si.max_frame > 0 { (2 * u64::from(si.max_frame) + 64).min(CHUNK) } else { CHUNK };
        for i in 0..64 {
            // a bracket the points left upside down is walked, never bisected
            if hi.1.saturating_sub(lo.1) <= BISECT_STOP {
                break;
            }
            // interpolate on the samples, every other step halves so a skewed file still narrows
            let guess = match hi.0 {
                s if i % 2 == 0 && s != u64::MAX && s > lo.0 => {
                    lo.1 + ((u128::from(want - lo.0) * u128::from(hi.1 - lo.1))
                        / u128::from(s - lo.0)) as u64
                }
                _ => lo.1 + (hi.1 - lo.1) / 2,
            };
            let guess = guess.clamp(lo.1 + 1, hi.1 - 1);
            match self.resync(h, read, guess, hi.1)? {
                Some((b, hd)) if hd.sample.unwrap_or(0) <= want => lo = (hd.sample.unwrap_or(0), b),
                Some((b, hd)) => hi = (hd.sample.unwrap_or(u64::MAX), b),
                None => hi.1 = guess,
            }
        }
        self.win.ahead = CHUNK;
        // walk from the bracket's low end to the frame holding the target
        let mut at = lo.1;
        loop {
            let Some(hd) = self.hdr_at(h, read, at)? else {
                match self.resync(h, read, at + 1, h.end)? {
                    Some((b, _)) => {
                        at = b;
                        continue;
                    }
                    None => break,
                }
            };
            let s = hd.sample.unwrap_or(0);
            if s + u64::from(hd.samples) > want {
                self.n = s;
                break;
            }
            match self.flac_end(h, read, at, &hd, true)? {
                Some(e) if e < h.end => at = e,
                _ => {
                    self.n = s;
                    break;
                }
            }
        }
        (self.at, self.exact) = (at, true);
        Ok(())
    }

    fn seek_frames(
        &mut self,
        h: &BareHead,
        read: &mut ReadAt<'_>,
        want: u64,
    ) -> Result<(), BareError> {
        let spf = u64::from(h.spf.max(1));
        let k = want / spf;
        if let Some((num, den)) = h.cbr {
            let along = u64::try_from(u128::from(k) * u128::from(num) / u128::from(den.max(1)))
                .unwrap_or(u64::MAX);
            let guess = h.first.saturating_add(along).min(h.end);
            let (b, _) = self
                .resync(h, read, guess.saturating_sub(2).max(h.first), h.end)?
                .unwrap_or((h.end, Hdr { len: 0, skip: 0, samples: 0, sample: None }));
            let along = u128::from(b.saturating_sub(h.first)) * u128::from(den);
            let k = ((along + u128::from(num) / 2) / u128::from(num.max(1))) as u64;
            (self.at, self.n, self.exact) = (b, k * spf, true);
            return Ok(());
        }
        let before = |list: &[(u64, u64)]| list.iter().rev().find(|p| p.0 <= want).copied();
        let counted = before(&self.known).unwrap_or((0, h.first));
        let near = want - counted.0 <= WALK_SECS * u64::from(h.rate);
        let table = before(&h.points).filter(|t| t.0 > counted.0);
        let (mut n, mut at, exact) = match (near, table) {
            (false, Some(t)) => (t.0, t.1, false),
            (false, None) if h.samples > 0 && h.end > h.first => {
                let b = h.first
                    + (u128::from(want) * u128::from(h.end - h.first) / u128::from(h.samples))
                        as u64;
                (want / spf * spf, b, false)
            }
            _ => (counted.0, counted.1, true),
        };
        if !exact {
            match self.resync(h, read, at, h.end)? {
                Some((b, _)) => at = b,
                None => at = h.end,
            }
        }
        // walk the headers to the target frame
        while n + spf <= want && at < h.end {
            let Some(hd) = self.hdr_at(h, read, at)? else {
                match self.resync(h, read, at + 1, h.end)? {
                    Some((b, _)) => {
                        at = b;
                        continue;
                    }
                    None => {
                        at = h.end;
                        break;
                    }
                }
            };
            if exact
                && self.known.last().is_none_or(|p| n >= p.0 + u64::from(h.rate))
                && self.known.len() < MAX_KNOWN
            {
                self.known.push((n, at));
            }
            at += hd.len;
            n += u64::from(hd.samples);
        }
        (self.at, self.n, self.exact) = (at, n, exact);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::fixture::{flac_file, reader};
    use crate::head::{BareKind, open};

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

    #[test]
    fn a_window_reads_ahead_and_keeps_the_anchor() {
        let file: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        let mut r = reader(&file);
        let mut w = Win::default();
        w.fill(&mut r, 1000, 1010, file.len() as u64).unwrap();
        assert_eq!((w.at, w.end()), (1000, 1000 + CHUNK));
        w.fill(&mut r, 1500, 1000 + CHUNK + 10, file.len() as u64).unwrap();
        assert_eq!(w.at, 1500, "the bytes before from let go");
        assert_eq!(w.get(1500, 4), &file[1500..1504]);
        w.fill(&mut r, 399_990, 400_100, file.len() as u64).unwrap();
        assert_eq!(w.get(399_990, 100), &file[399_990..], "short at the end");
    }

    #[test]
    fn flac_frames_come_out_whole_and_exact() {
        let f = flac_file(30, 1000, &[], false);
        let mut r = reader(&f.bytes);
        let (h, _) = open(&mut r, f.bytes.len() as u64, BareKind::Flac).unwrap();
        let mut rd = Reader::new(&h);
        let mut got = Vec::new();
        while let Some(u) = rd.next(&h, &mut r).unwrap() {
            assert_eq!(rd.bytes(&u), &f.bytes[u.off as usize..(u.off + u64::from(u.len)) as usize]);
            got.push((u.off, u.len, u.pts_ns));
        }
        let want: Vec<(u64, u32, i64)> = f.frames.iter().map(|x| (x.0, x.1, h.ns(x.2))).collect();
        assert_eq!(got, want);
    }
}
