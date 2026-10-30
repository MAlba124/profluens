//! The system layer of a program stream, H.222.0 2.5.3 and 2.5.4, ISO/IEC 11172-1 2.4.3. Pack
//! header, system header, program stream map and the private stream 1 sub-stream ids of DVD-Video.

use std::fmt;

pub const END: u8 = 0xB9;
pub const PACK: u8 = 0xBA;
pub const SYSTEM_HEADER: u8 = 0xBB;
pub const PSM: u8 = 0xBC;
pub const PRIVATE_1: u8 = 0xBD;
pub const PADDING: u8 = 0xBE;
pub const PRIVATE_2: u8 = 0xBF;

/// The longest item, a PES packet of the largest length.
pub const ITEM_MAX: usize = 6 + 0xFFFF;

/// Bytes of an item that always tell its length.
pub const PEEK: usize = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Len(usize),
    More,
    Junk,
}

/// The length of the item `b` starts with, from its start code and length fields.
pub fn item_len(b: &[u8]) -> Need {
    if b.iter().zip([0u8, 0, 1]).any(|(x, y)| *x != y) {
        return Need::Junk;
    }
    let Some(&id) = b.get(3) else { return Need::More };
    match id {
        END => Need::Len(4),
        PACK => match b.get(4) {
            None => Need::More,
            Some(x) if x & 0xC0 == 0x40 => {
                b.get(13).map_or(Need::More, |s| Need::Len(14 + usize::from(s & 7)))
            }
            Some(x) if x & 0xF0 == 0x20 => Need::Len(12),
            Some(_) => Need::Junk,
        },
        SYSTEM_HEADER..=0xFF => match b.get(4..6) {
            Some(l) => Need::Len(6 + usize::from(u16::from_be_bytes([l[0], l[1]]))),
            None => Need::More,
        },
        _ => Need::Junk,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pack {
    /// system_clock_reference_base, 90 kHz.
    pub scr: u64,
    /// system_clock_reference_extension, 27 MHz, zero in MPEG-1.
    pub scr_ext: u16,
    /// program_mux_rate in units of 50 bytes a second.
    pub mux_rate: u32,
    pub mpeg2: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackError {
    Short,
    Version,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackError::Short => f.write_str("pack header cut short"),
            PackError::Version => f.write_str("pack header neither MPEG-1 nor MPEG-2"),
        }
    }
}

impl std::error::Error for PackError {}

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |v, x| v << 8 | u64::from(*x))
}

/// H.222.0 Table 2-38 or ISO/IEC 11172-1 2.4.3.2. Marker bits are not checked, as FFmpeg does not.
pub fn parse_pack(b: &[u8]) -> Result<Pack, PackError> {
    let first = *b.get(4).ok_or(PackError::Short)?;
    if first & 0xC0 == 0x40 {
        let v = be(b.get(4..10).ok_or(PackError::Short)?);
        let rate = be(b.get(10..13).ok_or(PackError::Short)?);
        let scr = (v >> 43 & 0x7) << 30 | (v >> 27 & 0x7FFF) << 15 | (v >> 11 & 0x7FFF);
        let scr_ext = (v >> 1 & 0x1FF) as u16;
        return Ok(Pack { scr, scr_ext, mux_rate: (rate >> 2) as u32, mpeg2: true });
    }
    if first & 0xF0 == 0x20 {
        let v = be(b.get(4..9).ok_or(PackError::Short)?);
        let rate = be(b.get(9..12).ok_or(PackError::Short)?);
        let scr = (v >> 33 & 0x7) << 30 | (v >> 17 & 0x7FFF) << 15 | (v >> 1 & 0x7FFF);
        return Ok(Pack {
            scr,
            scr_ext: 0,
            mux_rate: (rate >> 1 & 0x3F_FFFF) as u32,
            mpeg2: false,
        });
    }
    Err(PackError::Version)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemHeader {
    pub rate_bound: u32,
    pub audio_bound: u8,
    pub video_bound: u8,
    pub streams: u8,
}

/// H.222.0 Table 2-40, the bounds and how many stream rows follow.
pub fn parse_system_header(b: &[u8]) -> Option<SystemHeader> {
    let f = b.get(6..12)?;
    let rows = b.get(12..)?;
    Some(SystemHeader {
        rate_bound: (be(&f[..3]) >> 1 & 0x3F_FFFF) as u32,
        audio_bound: f[3] >> 2,
        video_bound: f[4] & 0x1F,
        streams: rows.as_chunks::<3>().0.iter().take_while(|r| r[0] & 0x80 != 0).count().min(255)
            as u8,
    })
}

/// The stream types of a program stream map by elementary stream id, zero where unmapped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Psm {
    pub types: [u8; 256],
    pub version: u8,
    pub present: bool,
}

impl Default for Psm {
    fn default() -> Self {
        Psm { types: [0; 256], version: 0, present: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsmError {
    Short,
    Crc,
}

impl fmt::Display for PsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PsmError::Short => f.write_str("program stream map cut short"),
            PsmError::Crc => f.write_str("program stream map fails its CRC_32"),
        }
    }
}

impl std::error::Error for PsmError {}

/// H.222.0 Table 2-41 into `psm`. A map whose CRC_32 fails is still taken, the error reports it.
pub fn parse_psm(b: &[u8], psm: &mut Psm) -> Result<(), PsmError> {
    let be16 = |at: usize| b.get(at..at + 2).map(|v| usize::from(u16::from_be_bytes([v[0], v[1]])));
    let end = 6 + be16(4).ok_or(PsmError::Short)?;
    let map_at = 10 + be16(8).ok_or(PsmError::Short)?;
    let map_len = be16(map_at).ok_or(PsmError::Short)?;
    let b = b.get(..end).ok_or(PsmError::Short)?;
    let mut rows = b.get(map_at + 2..map_at + 2 + map_len).ok_or(PsmError::Short)?;
    if end < map_at + 2 + map_len + 4 {
        return Err(PsmError::Short);
    }
    psm.types = [0; 256];
    (psm.version, psm.present) = (b[6] & 0x1F, true);
    while let [stream_type, id, l0, l1, rest @ ..] = rows {
        psm.types[usize::from(*id)] = *stream_type;
        let skip = usize::from(u16::from_be_bytes([*l0, *l1])).min(rest.len());
        rows = &rest[skip..];
    }
    match pf_mpegts::crc32(b) {
        0 => Ok(()),
        _ => Err(PsmError::Crc),
    }
}

/// A DVD-Video private stream 1 sub-stream, by its first payload byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sub {
    Subpicture,
    Ac3,
    Dts,
    Lpcm,
    Unknown,
}

pub fn sub_of(id: u8) -> Sub {
    match id {
        0x20..=0x3F => Sub::Subpicture,
        0x80..=0x87 => Sub::Ac3,
        0x88..=0x8F => Sub::Dts,
        0xA0..=0xA7 => Sub::Lpcm,
        _ => Sub::Unknown,
    }
}

/// Bytes ahead of the data, the id, then frame count and first access unit pointer for audio, and
/// the three byte LPCM header past those.
pub fn sub_header_len(sub: Sub) -> usize {
    match sub {
        Sub::Subpicture | Sub::Unknown => 1,
        Sub::Ac3 | Sub::Dts => 4,
        Sub::Lpcm => 7,
    }
}

/// Raw AC-3 straight in private stream 1, as some muxers write it, counts as sub-stream 0x80.
pub const RAW_AC3: u8 = 0x80;

pub fn split_sub(payload: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    if payload.starts_with(&[0x0B, 0x77]) {
        return Some((RAW_AC3, &[], payload));
    }
    let id = *payload.first()?;
    let n = sub_header_len(sub_of(id));
    let (header, data) = payload.split_at_checked(n)?;
    Some((id, header, data))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test vectors
mod tests {
    use super::*;

    fn pack2(scr: u64, ext: u16, rate: u32, stuffing: u8) -> Vec<u8> {
        let v = 1u64 << 46
            | (scr >> 30 & 7) << 43
            | 1 << 42
            | (scr >> 15 & 0x7FFF) << 27
            | 1 << 26
            | (scr & 0x7FFF) << 11
            | 1 << 10
            | u64::from(ext) << 1
            | 1;
        let mut b = vec![0, 0, 1, PACK];
        b.extend_from_slice(&v.to_be_bytes()[2..]);
        b.extend_from_slice(&(rate << 2 | 3).to_be_bytes()[1..]);
        b.push(0xF8 | stuffing);
        b.extend(std::iter::repeat_n(0xFF, usize::from(stuffing)));
        b
    }

    fn pack1(scr: u64, rate: u32) -> Vec<u8> {
        let v = 2u64 << 36
            | (scr >> 30 & 7) << 33
            | 1 << 32
            | (scr >> 15 & 0x7FFF) << 17
            | 1 << 16
            | (scr & 0x7FFF) << 1
            | 1;
        let mut b = vec![0, 0, 1, PACK];
        b.extend_from_slice(&v.to_be_bytes()[3..]);
        b.extend_from_slice(&(1 << 23 | rate << 1 | 1).to_be_bytes()[1..]);
        b
    }

    #[test]
    fn both_pack_headers_give_their_clock_and_rate() {
        let b = pack2(0x1_2345_6789, 299, 25_200, 3);
        assert_eq!(item_len(&b), Need::Len(17));
        let p = parse_pack(&b).unwrap();
        assert_eq!(p, Pack { scr: 0x1_2345_6789, scr_ext: 299, mux_rate: 25_200, mpeg2: true });
        let b = pack1(0x1_FFFF_FFFF, 3528);
        assert_eq!(item_len(&b), Need::Len(12));
        let p = parse_pack(&b).unwrap();
        assert_eq!(p, Pack { scr: 0x1_FFFF_FFFF, scr_ext: 0, mux_rate: 3528, mpeg2: false });
        for bit in 0..33 {
            assert_eq!(parse_pack(&pack2(1 << bit, 0, 1, 0)).unwrap().scr, 1 << bit);
            assert_eq!(parse_pack(&pack1(1 << bit, 1)).unwrap().scr, 1 << bit);
        }
    }

    #[test]
    fn lengths_need_only_the_head_of_an_item() {
        let b = pack2(0, 0, 1, 7);
        for cut in 0..14 {
            assert_eq!(item_len(&b[..cut]), Need::More, "cut at {cut}");
        }
        assert_eq!(item_len(&[0, 0, 1, 0xE0, 0x07, 0xEC]), Need::Len(2034));
        assert_eq!(item_len(&[0, 0, 1, END]), Need::Len(4));
        assert_eq!(item_len(&[0, 0, 1, 0xB3]), Need::Junk, "a video start code");
        assert_eq!(item_len(&[0, 0, 2]), Need::Junk);
        assert_eq!(item_len(&[1]), Need::Junk);
        assert_eq!(item_len(&[0, 0, 1, PACK, 0x80]), Need::Junk);
        assert_eq!(parse_pack(&[0, 0, 1, PACK, 0x80]), Err(PackError::Version));
        assert_eq!(parse_pack(&[0, 0, 1, PACK, 0x44, 0]), Err(PackError::Short));
    }

    #[test]
    fn a_system_header_says_its_bounds() {
        let b = [
            0, 0, 1, 0xBB, 0, 12, 0x80, 0x9C, 0x41, 0x04, 0xE1, 0xFF, 0xB9, 0xE0, 0xE8, 0xB8, 0xC0,
            0x20,
        ];
        let h = parse_system_header(&b).unwrap();
        assert_eq!((h.rate_bound, h.audio_bound, h.video_bound, h.streams), (20_000, 1, 1, 2));
    }

    fn psm(rows: &[(u8, u8)], crc_ok: bool) -> Vec<u8> {
        let mut map = Vec::new();
        for &(t, id) in rows {
            map.extend_from_slice(&[t, id, 0, 2, 0xAA, 0xBB]);
        }
        let mut b = vec![0, 0, 1, PSM, 0, 0, 0x81, 0xFF, 0, 0];
        b.extend_from_slice(&(map.len() as u16).to_be_bytes());
        b.extend_from_slice(&map);
        let len = (b.len() - 6 + 4) as u16;
        b[4..6].copy_from_slice(&len.to_be_bytes());
        let crc = pf_mpegts::crc32(&b) ^ u32::from(!crc_ok);
        b.extend_from_slice(&crc.to_be_bytes());
        b
    }

    #[test]
    fn a_map_names_the_stream_types() {
        let mut m = Psm::default();
        parse_psm(&psm(&[(0x1B, 0xE0), (0x24, 0xE1), (0x10, 0xE2), (0x0F, 0xC0)], true), &mut m)
            .unwrap();
        assert!(m.present);
        assert_eq!((m.types[0xE0], m.types[0xE1], m.types[0xE2]), (0x1B, 0x24, 0x10));
        assert_eq!((m.types[0xC0], m.types[0xC1], m.version), (0x0F, 0, 1));
        assert_eq!(parse_psm(&psm(&[(2, 0xE0)], false), &mut m), Err(PsmError::Crc));
        assert_eq!(m.types[0xE0], 2, "taken all the same");
        let mut cut = psm(&[(2, 0xE0)], true);
        cut[11] = 200;
        assert_eq!(parse_psm(&cut, &mut m), Err(PsmError::Short));
    }

    #[test]
    fn private_stream_1_splits_by_sub_stream() {
        assert_eq!(split_sub(&[0x20, 1, 2]), Some((0x20, &[0x20][..], &[1, 2][..])));
        assert_eq!(
            split_sub(&[0x81, 1, 0, 2, 0x0B, 0x77]),
            Some((0x81, &[0x81, 1, 0, 2][..], &[0x0B, 0x77][..]))
        );
        assert_eq!(split_sub(&[0x0B, 0x77, 9]).map(|s| (s.0, s.1.len())), Some((RAW_AC3, 0)));
        assert_eq!(split_sub(&[0xA0, 1, 0, 2, 0x0C, 0x01, 0x80, 7]).map(|s| s.2), Some(&[7][..]));
        assert_eq!(split_sub(&[0xA0, 1]), None);
        assert_eq!(split_sub(&[]), None);
        assert_eq!(sub_of(0x89), Sub::Dts);
        assert_eq!(sub_of(0xA8), Sub::Unknown);
    }
}
