//! Per-frame side data records. Each is a kind byte, a little endian `u16` length and the bytes.

use crate::dovi::{RpuForm, RpuHeader};
use crate::hdr10plus::{Hdr10Plus, Hdr10PlusError};
use crate::t35::{T35Kind, classify};

/// The side area of one slot, core's `memory::SIDE_CAP`.
pub const SIDE_CAP: usize = profluens_core::memory::SIDE_CAP;

const HEAD: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    T35 = 1,
    DoviRpu = 2,
    DoviEl = 3,
}

impl Kind {
    pub const fn from_u8(b: u8) -> Option<Kind> {
        match b {
            1 => Some(Kind::T35),
            2 => Some(Kind::DoviRpu),
            3 => Some(Kind::DoviEl),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SideError {
    Truncated { at: usize },
    Kind { at: usize, kind: u8 },
}

impl std::fmt::Display for SideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SideError::Truncated { at } => write!(f, "a record at {at} runs past the end"),
            SideError::Kind { at, kind } => write!(f, "unknown record kind {kind} at {at}"),
        }
    }
}

impl std::error::Error for SideError {}

/// Appends records into a caller's buffer, refusing what does not fit.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
    pub dropped: u32,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Writer<'a> {
        Writer { buf, len: 0, dropped: 0 }
    }

    /// Append one record, `false` when it does not fit.
    pub fn push(&mut self, kind: Kind, bytes: &[u8]) -> bool {
        let end = self.len + HEAD + bytes.len();
        if bytes.len() > usize::from(u16::MAX) || end > self.buf.len() {
            self.dropped += 1;
            return false;
        }
        self.buf[self.len] = kind as u8;
        self.buf[self.len + 1..self.len + HEAD]
            .copy_from_slice(&(bytes.len() as u16).to_le_bytes());
        self.buf[self.len + HEAD..end].copy_from_slice(bytes);
        self.len = end;
        true
    }

    fn has_hdr10plus(&self) -> bool {
        records(&self.buf[..self.len])
            .any(|(k, b)| k == Kind::T35 as u8 && matches!(classify(b), T35Kind::Hdr10Plus { .. }))
    }

    /// Append container records under the bitstream's. A bitstream HDR10+ message wins.
    pub fn merge_under(&mut self, container: &[u8]) {
        let bitstream_hdr10plus = self.has_hdr10plus();
        for (kind, bytes) in records(container) {
            let Some(kind) = Kind::from_u8(kind) else {
                continue;
            };
            if kind == Kind::T35
                && bitstream_hdr10plus
                && matches!(classify(bytes), T35Kind::Hdr10Plus { .. })
            {
                continue;
            }
            self.push(kind, bytes);
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// Walk the records of `side`, stopping at the first that runs past the end.
pub fn records(side: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = side;
    std::iter::from_fn(move || {
        let head = rest.get(..HEAD)?;
        let len = usize::from(u16::from_le_bytes([head[1], head[2]]));
        let body = rest.get(HEAD..HEAD + len)?;
        let kind = head[0];
        rest = &rest[HEAD + len..];
        Some((kind, body))
    })
}

#[derive(Clone, Copy, Debug)]
pub struct FrameMeta<'a> {
    side: &'a [u8],
}

impl<'a> FrameMeta<'a> {
    pub fn read(side: &'a [u8]) -> Result<FrameMeta<'a>, SideError> {
        let mut at = 0;
        while at < side.len() {
            let head = side.get(at..at + HEAD).ok_or(SideError::Truncated { at })?;
            if Kind::from_u8(head[0]).is_none() {
                return Err(SideError::Kind { at, kind: head[0] });
            }
            let len = usize::from(u16::from_le_bytes([head[1], head[2]]));
            if at + HEAD + len > side.len() {
                return Err(SideError::Truncated { at });
            }
            at += HEAD + len;
        }
        Ok(FrameMeta { side })
    }

    pub fn is_empty(&self) -> bool {
        self.side.is_empty()
    }

    pub fn records(&self) -> impl Iterator<Item = (Kind, &'a [u8])> + 'a + use<'a> {
        records(self.side).filter_map(|(k, b)| Some((Kind::from_u8(k)?, b)))
    }

    pub fn t35(&self) -> impl Iterator<Item = &'a [u8]> + 'a + use<'a> {
        self.records().filter(|(k, _)| *k == Kind::T35).map(|(_, b)| b)
    }

    /// The ST 2094-40 payload of the frame's HDR10+ message, after the T.35 codes.
    pub fn hdr10plus_payload(&self) -> Option<&'a [u8]> {
        self.t35().find_map(|m| match classify(m) {
            T35Kind::Hdr10Plus { payload } => Some(payload),
            _ => None,
        })
    }

    pub fn hdr10plus(&self) -> Option<Result<Hdr10Plus, Hdr10PlusError>> {
        self.hdr10plus_payload().map(Hdr10Plus::parse)
    }

    /// The frame's raw Dolby Vision RPU and its wrapping.
    pub fn dovi_rpu(&self) -> Option<(RpuForm, &'a [u8])> {
        self.records().find_map(|(k, b)| match k {
            Kind::DoviRpu => Some((RpuForm::Nal, b)),
            Kind::T35 => match classify(b) {
                T35Kind::DoviRpu { payload } => Some((RpuForm::Av1Emdf, payload)),
                _ => None,
            },
            Kind::DoviEl => None,
        })
    }

    pub fn dovi_rpu_header(&self) -> Option<Result<RpuHeader, crate::dovi::DoviError>> {
        self.dovi_rpu().map(|(form, b)| RpuHeader::parse(form, b))
    }

    pub fn dovi_el(&self) -> bool {
        self.records().any(|(k, _)| k == Kind::DoviEl)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;

    fn hdr10plus_t35(maxscl: u32) -> Vec<u8> {
        let mut m = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
        m.extend(crate::hdr10plus::sample_payload([maxscl, maxscl / 2, maxscl / 4], 1000));
        m
    }

    #[test]
    fn records_round_trip_and_read_back_typed() {
        let mut buf = [0u8; 256];
        let mut w = Writer::new(&mut buf);
        assert!(w.push(Kind::T35, &hdr10plus_t35(50_000)));
        assert!(w.push(Kind::DoviEl, &[]));
        let n = w.len();
        let m = FrameMeta::read(&buf[..n]).unwrap();
        assert!(m.dovi_el());
        assert_eq!(m.hdr10plus().unwrap().unwrap().windows[0].maxscl[0], 50_000);
        assert_eq!(m.dovi_rpu(), None);
        assert_eq!(m.records().count(), 2);
    }

    #[test]
    fn a_full_writer_drops_and_counts() {
        let mut buf = [0u8; 8];
        let mut w = Writer::new(&mut buf);
        assert!(w.push(Kind::DoviEl, &[]));
        assert!(!w.push(Kind::T35, &[1, 2, 3, 4, 5, 6]));
        assert_eq!((w.len(), w.dropped), (3, 1));
    }

    #[test]
    fn the_bitstream_hdr10plus_wins_over_the_container() {
        let container = {
            let mut buf = [0u8; 256];
            let mut w = Writer::new(&mut buf);
            w.push(Kind::T35, &hdr10plus_t35(10_000));
            let n = w.len();
            buf[..n].to_vec()
        };
        let mut buf = [0u8; 512];
        let mut w = Writer::new(&mut buf);
        w.push(Kind::T35, &hdr10plus_t35(90_000));
        w.merge_under(&container);
        let n = w.len();
        let m = FrameMeta::read(&buf[..n]).unwrap();
        assert_eq!(m.t35().count(), 1);
        assert_eq!(m.hdr10plus().unwrap().unwrap().windows[0].maxscl[0], 90_000);
        let mut buf = [0u8; 512];
        let mut w = Writer::new(&mut buf);
        w.merge_under(&container);
        let n = w.len();
        assert_eq!(
            FrameMeta::read(&buf[..n]).unwrap().hdr10plus().unwrap().unwrap().windows[0].maxscl[0],
            10_000
        );
    }

    #[test]
    fn a_bad_record_is_refused_once() {
        assert_eq!(FrameMeta::read(&[1, 5, 0, 1]).unwrap_err(), SideError::Truncated { at: 0 });
        assert_eq!(FrameMeta::read(&[3, 0, 0, 1]).unwrap_err(), SideError::Truncated { at: 3 });
        assert_eq!(FrameMeta::read(&[9, 0, 0]).unwrap_err(), SideError::Kind { at: 0, kind: 9 });
        assert!(FrameMeta::read(&[]).unwrap().is_empty());
    }

    #[test]
    fn an_av1_dolby_t35_is_an_rpu() {
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        w.push(Kind::T35, &[0xB5, 0x00, 0x3B, 0x00, 0x00, 0x08, 0x00, 0xAA]);
        let n = w.len();
        let m = FrameMeta::read(&buf[..n]).unwrap();
        assert_eq!(m.dovi_rpu(), Some((RpuForm::Av1Emdf, &[0xAA][..])));
        assert_eq!(m.hdr10plus(), None);
    }
}
