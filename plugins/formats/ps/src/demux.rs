//! Sans-IO program stream demuxer. A state machine of two states, scanning for a start code and
//! gathering an item cut by the input into `carry`. Whole items in the input are read in place.
//! PES headers go through pf-mpegts, MPEG-2 syntax or MPEG-1 by the bits past the length.

use std::fmt;

use pf_mpegts::pes::{PH_BAD_TIMESTAMP, PesHeader, parse_mpeg1_pes_header, parse_pes_header};

use crate::pack::{
    self, END, ITEM_MAX, Need, PACK, PADDING, PEEK, PRIVATE_1, PRIVATE_2, PSM, Pack, Psm,
    SYSTEM_HEADER, SystemHeader,
};

/// `Length` is an item length running past where the next start code should be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnomalyKind {
    Junk,
    PackHeader,
    PesHeader,
    Psm,
    PsmCrc,
    BadTimestamp,
    Length,
    Truncated,
}

impl fmt::Display for AnomalyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AnomalyKind::Junk => "bytes between items, skipped to the next start code",
            AnomalyKind::PackHeader => "a pack header neither MPEG-1 nor MPEG-2",
            AnomalyKind::PesHeader => "a PES header that does not parse",
            AnomalyKind::Psm => "a program stream map cut short",
            AnomalyKind::PsmCrc => "a program stream map failing its CRC_32",
            AnomalyKind::BadTimestamp => "PES timestamps with broken marker bits",
            AnomalyKind::Length => "an item length running past the next start code",
            AnomalyKind::Truncated => "an item cut short by the end",
        })
    }
}

impl AnomalyKind {
    /// A short tag for a log field.
    pub const fn name(self) -> &'static str {
        match self {
            AnomalyKind::Junk => "junk",
            AnomalyKind::PackHeader => "pack_header",
            AnomalyKind::PesHeader => "pes_header",
            AnomalyKind::Psm => "psm",
            AnomalyKind::PsmCrc => "psm_crc",
            AnomalyKind::BadTimestamp => "bad_timestamp",
            AnomalyKind::Length => "length",
            AnomalyKind::Truncated => "truncated",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anomaly {
    pub kind: AnomalyKind,
    pub offset: u64,
}

/// One PES packet's payload. `key` is the stream id, and for private stream 1 the sub-stream id in
/// the low byte.
#[derive(Clone, Copy, Debug)]
pub struct Unit<'a> {
    pub key: u16,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// The sub-stream header of private stream 1, empty elsewhere.
    pub header: &'a [u8],
    pub payload: &'a [u8],
    pub offset: u64,
    pub flags: u8,
}

impl Unit<'_> {
    pub fn stream_id(&self) -> u8 {
        (self.key >> 8) as u8
    }
}

pub const fn key_of(stream_id: u8, sub: u8) -> u16 {
    (stream_id as u16) << 8 | sub as u16
}

#[derive(Debug)]
pub enum Event<'a> {
    Pack { offset: u64, pack: Pack },
    SystemHeader(SystemHeader),
    Psm(&'a Psm),
    Unit(Unit<'a>),
    End { offset: u64 },
    Anomaly(Anomaly),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    pub packs: u64,
    pub units: u64,
    pub skipped: u64,
    pub junk: u64,
    pub anomalies: u64,
}

pub struct Demux {
    carry: Vec<u8>,
    carry_at: u64,
    pos: u64,
    psm: Psm,
    in_junk: bool,
    stats: Stats,
}

impl Default for Demux {
    fn default() -> Self {
        Self::new()
    }
}

fn is_prefix(b: &[u8]) -> bool {
    b.iter().zip([0u8, 0, 1]).all(|(x, y)| *x == y)
}

/// The first start code of the system layer in `b`, or the start of a trailing partial one.
fn find_code(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 3 < b.len() {
        match b[i + 2] {
            2.. => i += 3,
            1 if b[i] == 0 && b[i + 1] == 0 && b[i + 3] >= END => return Some(i),
            _ => i += 1,
        }
    }
    (i..b.len()).find(|&p| is_prefix(&b[p..]))
}

impl Demux {
    #[allow(clippy::disallowed_methods)] // once per demuxer
    pub fn new() -> Demux {
        Demux {
            carry: Vec::with_capacity(ITEM_MAX + 3),
            carry_at: 0,
            pos: 0,
            psm: Psm::default(),
            in_junk: false,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn psm(&self) -> &Psm {
        &self.psm
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    /// The next byte pushed is at `at`, what is held goes.
    pub fn set_position(&mut self, at: u64) {
        self.carry.clear();
        (self.pos, self.in_junk) = (at, false);
    }

    fn note(&mut self, kind: AnomalyKind, offset: u64, sink: &mut impl FnMut(Event<'_>)) {
        self.stats.anomalies += 1;
        sink(Event::Anomaly(Anomaly { kind, offset }));
    }

    fn junk(&mut self, n: usize, offset: u64, sink: &mut impl FnMut(Event<'_>)) {
        if n == 0 {
            return;
        }
        self.stats.junk += n as u64;
        if !std::mem::replace(&mut self.in_junk, true) {
            self.note(AnomalyKind::Junk, offset, sink);
        }
    }

    pub fn push(&mut self, data: &[u8], sink: &mut impl FnMut(Event<'_>)) {
        let base = self.pos;
        self.pos += data.len() as u64;
        self.stats.bytes += data.len() as u64;
        let mut at = 0;
        while at < data.len() {
            if !self.carry.is_empty() {
                at += self.fill(&data[at..], sink);
                continue;
            }
            let rest = &data[at..];
            let Some(p) = find_code(rest) else {
                self.junk(rest.len(), base + at as u64, sink);
                return;
            };
            self.junk(p, base + at as u64, sink);
            let (start, offset) = (&rest[p..], base + (at + p) as u64);
            match pack::item_len(start) {
                // A start code has to follow, the end of the stream aside.
                Need::Len(n) if n + 3 <= start.len() => {
                    if !is_prefix(&start[n..n + 3]) {
                        self.note(AnomalyKind::Length, offset, sink);
                        self.junk(1, offset, sink);
                        at += p + 1;
                        continue;
                    }
                    self.in_junk = false;
                    self.item(&start[..n], offset, sink);
                    at += p + n;
                }
                Need::Junk => {
                    self.junk(1, offset, sink);
                    at += p + 1;
                }
                _ => {
                    self.carry.extend_from_slice(start);
                    self.carry_at = offset;
                    return;
                }
            }
        }
    }

    fn fill(&mut self, data: &[u8], sink: &mut impl FnMut(Event<'_>)) -> usize {
        match pack::item_len(&self.carry) {
            // The item and the three bytes past it, which start the next.
            Need::Len(n) => {
                let take = (n + 3 - self.carry.len().min(n + 3)).min(data.len());
                self.carry.extend_from_slice(&data[..take]);
                if self.carry.len() < n + 3 {
                    return take;
                }
                if !is_prefix(&self.carry[n..n + 3]) {
                    let offset = self.carry_at;
                    self.note(AnomalyKind::Length, offset, sink);
                    self.resync_carry(sink);
                    return take;
                }
                self.emit_carry(n, sink);
                take
            }
            Need::More => {
                let take = (PEEK - self.carry.len().min(PEEK)).max(1).min(data.len());
                self.carry.extend_from_slice(&data[..take]);
                take
            }
            Need::Junk => {
                self.resync_carry(sink);
                0
            }
        }
    }

    fn resync_carry(&mut self, sink: &mut impl FnMut(Event<'_>)) {
        let skip = 1 + find_code(&self.carry[1..]).unwrap_or(self.carry.len() - 1);
        let offset = self.carry_at;
        self.junk(skip, offset, sink);
        self.carry.drain(..skip);
        self.carry_at += skip as u64;
    }

    fn emit_carry(&mut self, n: usize, sink: &mut impl FnMut(Event<'_>)) {
        let mut carry = std::mem::take(&mut self.carry);
        self.in_junk = false;
        self.item(&carry[..n], self.carry_at, sink);
        carry.drain(..n);
        self.carry = carry;
        self.carry_at += n as u64;
    }

    /// The end of the input. A whole item carried is read, a partial one reported.
    pub fn flush(&mut self, sink: &mut impl FnMut(Event<'_>)) {
        while !self.carry.is_empty() {
            match pack::item_len(&self.carry) {
                Need::Len(n) if self.carry.len() >= n + 3 => _ = self.fill(&[], sink),
                Need::Len(n) if self.carry.len() >= n => self.emit_carry(n, sink),
                Need::Junk => self.resync_carry(sink),
                _ => break,
            }
        }
        if !self.carry.is_empty() {
            let offset = self.carry_at;
            self.carry.clear();
            self.note(AnomalyKind::Truncated, offset, sink);
        }
    }

    fn item(&mut self, b: &[u8], offset: u64, sink: &mut impl FnMut(Event<'_>)) {
        match b[3] {
            PACK => match pack::parse_pack(b) {
                Ok(pack) => {
                    self.stats.packs += 1;
                    sink(Event::Pack { offset, pack });
                }
                Err(_) => self.note(AnomalyKind::PackHeader, offset, sink),
            },
            SYSTEM_HEADER => match pack::parse_system_header(b) {
                Some(h) => sink(Event::SystemHeader(h)),
                None => self.stats.skipped += 1,
            },
            PSM => {
                let parsed = pack::parse_psm(b, &mut self.psm);
                match parsed {
                    Err(pack::PsmError::Short) => self.note(AnomalyKind::Psm, offset, sink),
                    Err(pack::PsmError::Crc) => self.note(AnomalyKind::PsmCrc, offset, sink),
                    Ok(()) => {}
                }
                if self.psm.present {
                    sink(Event::Psm(&self.psm));
                }
            }
            END => sink(Event::End { offset }),
            PADDING | PRIVATE_2 => self.stats.skipped += 1,
            _ => self.pes(b, offset, sink),
        }
    }

    fn pes(&mut self, b: &[u8], offset: u64, sink: &mut impl FnMut(Event<'_>)) {
        let header = match b.get(6) {
            Some(x) if x & 0xC0 == 0x80 => parse_pes_header(b),
            _ => parse_mpeg1_pes_header(b),
        };
        let Ok(Some(h)) = header else {
            return self.note(AnomalyKind::PesHeader, offset, sink);
        };
        let PesHeader { pts, dts, header_len, payload_len, stream_id, flags } = h;
        if flags & PH_BAD_TIMESTAMP != 0 {
            self.note(AnomalyKind::BadTimestamp, offset, sink);
        }
        let from = usize::from(header_len);
        let to = payload_len.map_or(b.len(), |n| from + n as usize).min(b.len());
        let payload = b.get(from..to).unwrap_or_default();
        let (key, header, payload) = match stream_id {
            PRIVATE_1 => match pack::split_sub(payload) {
                Some((sub, header, data)) => (key_of(PRIVATE_1, sub), header, data),
                None => return self.stats.skipped += 1,
            },
            id => (key_of(id, 0), &[][..], payload),
        };
        self.stats.units += 1;
        sink(Event::Unit(Unit { key, pts, dts: dts.or(pts), header, payload, offset, flags }));
    }
}

#[cfg(test)]
mod tests;
