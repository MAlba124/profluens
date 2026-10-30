//! HDR metadata OBUs of an AV1 temporal unit (AV1 5.8).

use crate::color::{ContentLight, Mastering};
use crate::h26x::Scan;
use crate::side::Kind;
use crate::t35::{T35Kind, classify};

pub const OBU_METADATA: u8 = 5;
pub const METADATA_HDR_CLL: u64 = 1;
pub const METADATA_HDR_MDCV: u64 = 2;
pub const METADATA_ITUT_T35: u64 = 4;

/// `leb128()` (AV1 4.10.5), `None` past the end or past 32 bits.
pub fn leb128(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *data.get(*at)?;
        *at += 1;
        v |= u64::from(b & 0x7F) << (i * 7);
        if b & 0x80 == 0 {
            return (v <= u64::from(u32::MAX)).then_some(v);
        }
    }
    None
}

/// The OBUs of a temporal unit as `(obu_type, payload)`.
pub fn obus(tu: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let header = *tu.get(at)?;
        let (kind, ext, has_size) = (header >> 3 & 0x0F, header & 0x04 != 0, header & 0x02 != 0);
        at += 1 + usize::from(ext);
        let size = match has_size {
            true => leb128(tu, &mut at)? as usize,
            false => tu.len().checked_sub(at)?,
        };
        let payload = tu.get(at..at.checked_add(size)?)?;
        at += size;
        Some((kind, payload))
    })
}

pub fn metadata(payload: &[u8], scan: &mut Scan<'_>) {
    let mut at = 0;
    let Some(kind) = leb128(payload, &mut at) else {
        return;
    };
    let body = &payload[at..];
    match kind {
        METADATA_HDR_CLL => {
            if let Some(l) = ContentLight::from_cta861(body) {
                scan.light = Some(l);
            }
        }
        METADATA_HDR_MDCV => {
            if let Some(p) = body.get(..24) {
                let u16at = |o: usize| u16::from_be_bytes([p[o], p[o + 1]]);
                let u32at = |o: usize| u32::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
                let xy = |o: usize| [u16at(o), u16at(o + 2)];
                scan.mastering =
                    Some(Mastering::from_av1([xy(0), xy(4), xy(8)], xy(12), u32at(16), u32at(20)));
            }
        }
        METADATA_ITUT_T35 => {
            let Some(last) = body.iter().rposition(|&b| b != 0) else {
                return;
            };
            if body[last] != 0x80 {
                return;
            }
            let msg = &body[..last];
            if !matches!(classify(msg), T35Kind::Other) {
                scan.side.push(Kind::T35, msg);
            }
        }
        _ => {}
    }
}

/// Scans OBUs in order and stops at the first malformed one, later OBUs are not seen.
pub fn scan(tu: &[u8], scan: &mut Scan<'_>) {
    for (kind, payload) in obus(tu) {
        if kind == OBU_METADATA {
            metadata(payload, scan);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::side::{FrameMeta, Writer};

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind << 3 | 0x02];
        let mut n = payload.len();
        loop {
            let b = (n & 0x7F) as u8;
            n >>= 7;
            out.push(if n > 0 { b | 0x80 } else { b });
            if n == 0 {
                break;
            }
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn leb128_per_4_10_5() {
        let mut at = 0;
        assert_eq!(leb128(&[0xE5, 0x8E, 0x26], &mut at), Some(624_485));
        assert_eq!(at, 3);
        assert_eq!(leb128(&[0x80], &mut 0), None, "cut");
    }

    #[test]
    fn a_temporal_unit_gives_cll_mdcv_and_hdr10plus() {
        let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
        t35.extend(crate::hdr10plus::sample_payload([70_000, 1, 1], 1000));
        let mut itut = vec![4];
        itut.extend_from_slice(&t35);
        itut.extend_from_slice(&[0x80, 0x00]);
        let mut mdcv = vec![2];
        for v in [46399u16, 19136, 11141, 52100, 8650, 3020, 20493, 21561] {
            mdcv.extend_from_slice(&v.to_be_bytes());
        }
        mdcv.extend_from_slice(&(1000u32 << 8).to_be_bytes());
        mdcv.extend_from_slice(&(50u32 << 10).to_be_bytes());
        let tu = [
            obu(2, &[]),
            obu(OBU_METADATA, &[1, 0x03, 0xE8, 0x01, 0x90]),
            obu(OBU_METADATA, &mdcv),
            obu(OBU_METADATA, &itut),
            obu(6, &[0x10, 0x20]),
        ]
        .concat();
        let mut side = [0u8; 256];
        let mut s = Scan::new(Writer::new(&mut side));
        scan(&tu, &mut s);
        assert_eq!(s.light, Some(ContentLight { max_cll: 1000, max_fall: 400 }));
        let m = s.mastering.unwrap();
        assert_eq!(
            (m.primaries[0][0], m.max_luminance, m.min_luminance),
            (35400, 10_000_000, 31_250)
        );
        let n = s.side.len();
        let meta = FrameMeta::read(&side[..n]).unwrap();
        assert_eq!(meta.hdr10plus().unwrap().unwrap().windows[0].maxscl[0], 70_000);
    }

    #[test]
    fn a_t35_without_its_trailing_bit_or_of_another_kind_is_dropped() {
        let mut side = [0u8; 64];
        let mut s = Scan::new(Writer::new(&mut side));
        metadata(&[4, 0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x01, 0x40], &mut s);
        metadata(&[4, 0xB5, 0x00, 0x31, 0x47, 0x41, 0x39, 0x34, 0x80], &mut s);
        assert!(s.side.is_empty());
    }

    #[test]
    fn an_obu_with_an_extension_and_no_size_runs_to_the_end() {
        let tu = [OBU_METADATA << 3 | 0x04, 0x00, 1, 0x00, 0x10, 0x00, 0x08];
        let all: Vec<_> = obus(&tu).collect();
        assert_eq!(all, [(OBU_METADATA, &[1, 0x00, 0x10, 0x00, 0x08][..])]);
    }
}
