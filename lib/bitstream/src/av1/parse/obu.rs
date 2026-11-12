use std::fmt;

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_TILE_GROUP: u8 = 4;
pub const OBU_METADATA: u8 = 5;
pub const OBU_FRAME: u8 = 6;
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;
pub const OBU_TILE_LIST: u8 = 8;
pub const OBU_PADDING: u8 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Av1Error {
    Truncated,
    ForbiddenBit,
    Leb128,
    ObuSize,
    NoSequenceHeader,
    NoFrameHeader,
    Profile(u8),
    Invalid(&'static str),
    MissingRef(u8),
    TileSize,
    LargeScaleTile,
}

impl fmt::Display for Av1Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Av1Error::Truncated => f.write_str("data ends inside a syntax element"),
            Av1Error::ForbiddenBit => f.write_str("obu_forbidden_bit set"),
            Av1Error::Leb128 => f.write_str("leb128 too long or too large"),
            Av1Error::ObuSize => f.write_str("obu_size past the end of the unit"),
            Av1Error::NoSequenceHeader => f.write_str("frame before any sequence header"),
            Av1Error::NoFrameHeader => f.write_str("tile group without a frame header"),
            Av1Error::Profile(p) => write!(f, "seq_profile {p}"),
            Av1Error::Invalid(what) => write!(f, "{what} out of range"),
            Av1Error::MissingRef(slot) => write!(f, "reference slot {slot} holds no frame"),
            Av1Error::TileSize => f.write_str("tile sizes past the tile group"),
            Av1Error::LargeScaleTile => f.write_str("large scale tile"),
        }
    }
}

impl std::error::Error for Av1Error {}

#[derive(Clone, Copy, Debug)]
pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    fn bit(&mut self) -> Result<u32, Av1Error> {
        let byte = *self.data.get(self.pos >> 3).ok_or(Av1Error::Truncated)?;
        let b = u32::from(byte >> (7 - (self.pos & 7)) & 1);
        self.pos += 1;
        Ok(b)
    }

    pub fn f(&mut self, n: u32) -> Result<u32, Av1Error> {
        let mut v = 0u32;
        for _ in 0..n {
            v = v << 1 | self.bit()?;
        }
        Ok(v)
    }

    pub fn flag(&mut self) -> Result<bool, Av1Error> {
        Ok(self.bit()? == 1)
    }

    pub fn u8(&mut self, n: u32) -> Result<u8, Av1Error> {
        Ok(self.f(n)? as u8)
    }

    pub fn su(&mut self, n: u32) -> Result<i32, Av1Error> {
        let v = self.f(n)? as i32;
        let sign = 1i32 << (n - 1);
        Ok(if v & sign != 0 { v - 2 * sign } else { v })
    }

    pub fn ns(&mut self, n: u32) -> Result<u32, Av1Error> {
        if n <= 1 {
            return Ok(0);
        }
        let w = 32 - n.leading_zeros();
        let m = (1u32 << w) - n;
        let v = self.f(w - 1)?;
        if v < m {
            return Ok(v);
        }
        Ok((v << 1) - m + self.bit()?)
    }

    pub fn uvlc(&mut self) -> Result<u32, Av1Error> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Ok(u32::MAX);
            }
        }
        Ok(self.f(zeros)?.saturating_add((1u32 << zeros) - 1))
    }

    pub fn byte_align(&mut self) {
        self.pos = self.pos.next_multiple_of(8);
    }
}

pub fn le(data: &[u8], n: usize) -> Option<u32> {
    let b = data.get(..n)?;
    Some(b.iter().enumerate().fold(0u32, |v, (i, &x)| v | u32::from(x) << (8 * i)))
}

pub fn leb128(data: &[u8]) -> Result<(u32, usize), Av1Error> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *data.get(i).ok_or(Av1Error::Truncated)?;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            let v = u32::try_from(v).map_err(|_| Av1Error::Leb128)?;
            return Ok((v, i + 1));
        }
    }
    Err(Av1Error::Leb128)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obu<'a> {
    pub obu_type: u8,
    pub extension: bool,
    pub temporal_id: u8,
    pub spatial_id: u8,
    pub payload: &'a [u8],
}

/// An OBU with no size field runs to the end of `data`.
pub fn next_obu(data: &[u8]) -> Result<(Obu<'_>, usize), Av1Error> {
    let h = *data.first().ok_or(Av1Error::Truncated)?;
    if h & 0x80 != 0 {
        return Err(Av1Error::ForbiddenBit);
    }
    let obu_type = h >> 3 & 0x0F;
    let extension = h & 0x04 != 0;
    let has_size = h & 0x02 != 0;
    let mut at = 1;
    let (mut temporal_id, mut spatial_id) = (0, 0);
    if extension {
        let e = *data.get(1).ok_or(Av1Error::Truncated)?;
        temporal_id = e >> 5;
        spatial_id = e >> 3 & 3;
        at = 2;
    }
    let size = if has_size {
        let (v, n) = leb128(data.get(at..).ok_or(Av1Error::Truncated)?)?;
        at += n;
        v as usize
    } else {
        data.len().checked_sub(at).ok_or(Av1Error::Truncated)?
    };
    let end = at.checked_add(size).filter(|&e| e <= data.len()).ok_or(Av1Error::ObuSize)?;
    Ok((Obu { obu_type, extension, temporal_id, spatial_id, payload: &data[at..end] }, end))
}

pub fn obus(mut data: &[u8]) -> impl Iterator<Item = Result<Obu<'_>, Av1Error>> {
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        match next_obu(data) {
            Ok((obu, n)) => {
                data = &data[n..];
                Some(Ok(obu))
            }
            Err(e) => {
                data = &[];
                Some(Err(e))
            }
        }
    })
}

/// The drop rule of section 6.2.1.
pub fn in_operating_point(obu: &Obu<'_>, idc: u16) -> bool {
    if obu.obu_type == OBU_SEQUENCE_HEADER
        || obu.obu_type == OBU_TEMPORAL_DELIMITER
        || idc == 0
        || !obu.extension
    {
        return true;
    }
    let temporal = idc >> obu.temporal_id & 1 == 1;
    let spatial = idc >> (u16::from(obu.spatial_id) + 8) & 1 == 1;
    temporal && spatial
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    pub offset: u32,
    pub size: u32,
    pub row: u16,
    pub col: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileGroup {
    pub tg_start: u16,
    pub tg_end: u16,
    pub data_offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileLayout {
    pub cols: u16,
    pub rows: u16,
    pub cols_log2: u8,
    pub rows_log2: u8,
    pub size_bytes: u8,
}

pub fn tile_group(
    payload: &[u8],
    t: &TileLayout,
    tiles: &mut Vec<Tile>,
) -> Result<TileGroup, Av1Error> {
    let num_tiles = u32::from(t.cols) * u32::from(t.rows);
    let mut r = Bits::new(payload);
    let present = num_tiles > 1 && r.flag()?;
    let (tg_start, tg_end) = if present {
        let bits = u32::from(t.cols_log2) + u32::from(t.rows_log2);
        (r.f(bits)?, r.f(bits)?)
    } else {
        (0, num_tiles.saturating_sub(1))
    };
    if tg_end < tg_start || tg_end >= num_tiles {
        return Err(Av1Error::Invalid("tg_end"));
    }
    r.byte_align();
    let data_offset = r.pos() / 8;
    let data = payload.get(data_offset..).ok_or(Av1Error::Truncated)?;
    let n = usize::from(t.size_bytes);
    let mut at = 0usize;
    for num in tg_start..=tg_end {
        let (row, col) = ((num / u32::from(t.cols)) as u16, (num % u32::from(t.cols)) as u16);
        let size = if num == tg_end {
            data.len().checked_sub(at).ok_or(Av1Error::TileSize)?
        } else {
            let s = le(data.get(at..).ok_or(Av1Error::TileSize)?, n).ok_or(Av1Error::TileSize)?
                as usize
                + 1;
            at += n;
            s
        };
        if at + size > data.len() || size == 0 {
            return Err(Av1Error::TileSize);
        }
        tiles.push(Tile { offset: at as u32, size: size as u32, row, col });
        at += size;
    }
    Ok(TileGroup { tg_start: tg_start as u16, tg_end: tg_end as u16, data_offset })
}

#[cfg(test)]
// Test vectors.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_read_as_section_4_10_says() {
        // 1, 0101, su(4) of 1101 is -3, ns(5) of 11 then 1 is 3*2-3+1 = 4.
        let data = [0b1010_1110, 0b1111_0000];
        let mut r = Bits::new(&data);
        assert_eq!(r.f(1), Ok(1));
        assert_eq!(r.f(4), Ok(0b0101));
        assert_eq!(r.su(4), Ok(-3));
        // ns(5): w 3, m 3, v = f(2) = 0b11 = 3, not below m, so 2v - m + bit.
        assert_eq!(r.ns(5), Ok(4));
        assert_eq!(r.pos(), 12);
        r.byte_align();
        assert_eq!(r.pos(), 16);
        assert_eq!(r.f(1), Err(Av1Error::Truncated));
        // uvlc of 001 01 is 2^2 - 1 + 1 = 4.
        assert_eq!(Bits::new(&[0b0010_1000]).uvlc(), Ok(4));
        assert_eq!(le(&[0x34, 0x12, 0xFF], 2), Some(0x1234));
    }

    #[test]
    fn leb128_per_4_10_5() {
        assert_eq!(leb128(&[0xE5, 0x8E, 0x26]), Ok((624_485, 3)));
        assert_eq!(leb128(&[0x80, 0x80, 0x80, 0x80, 0x00]), Ok((0, 5)), "padded zero");
        assert_eq!(leb128(&[0x80]), Err(Av1Error::Truncated));
        assert_eq!(leb128(&[0xFF; 9]), Err(Av1Error::Leb128));
        assert_eq!(leb128(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]), Err(Av1Error::Leb128), "past 32 bits");
    }

    #[test]
    fn obus_split_a_temporal_unit() {
        let tu = [0x12, 0x00, 0x7E, 0b0100_1000, 0x02, 0xAA, 0xBB, 0x30, 1, 2, 3];
        let got: Vec<_> = obus(&tu).collect();
        assert_eq!(got.len(), 3);
        let td = got[0].unwrap();
        assert_eq!((td.obu_type, td.payload.len()), (OBU_TEMPORAL_DELIMITER, 0));
        let pad = got[1].unwrap();
        assert_eq!(
            (pad.obu_type, pad.temporal_id, pad.spatial_id, pad.payload),
            (OBU_PADDING, 2, 1, &[0xAA, 0xBB][..])
        );
        let frame = got[2].unwrap();
        assert_eq!((frame.obu_type, frame.payload), (OBU_FRAME, &[1, 2, 3][..]));
        assert_eq!(obus(&[0x12, 0x05, 0]).next(), Some(Err(Av1Error::ObuSize)));
        assert_eq!(obus(&[0x92, 0x00]).next(), Some(Err(Av1Error::ForbiddenBit)));
    }

    #[test]
    fn the_operating_point_drops_other_layers() {
        let obu = |t, s| Obu {
            obu_type: OBU_FRAME,
            extension: true,
            temporal_id: t,
            spatial_id: s,
            payload: &[],
        };
        let idc = 0b1_0000_0011;
        assert!(in_operating_point(&obu(1, 0), idc));
        assert!(!in_operating_point(&obu(2, 0), idc));
        assert!(!in_operating_point(&obu(0, 1), idc));
        assert!(in_operating_point(&obu(5, 3), 0), "idc 0 takes all");
    }

    #[test]
    fn a_tile_group_gives_its_tiles() {
        let layout = TileLayout { cols: 2, rows: 2, cols_log2: 1, rows_log2: 1, size_bytes: 2 };
        // flag 1, tg_start 01, tg_end 11, aligned: 1011 1000.
        let mut p = vec![0b1011_1000];
        p.extend_from_slice(&[2, 0, 9, 9, 9]);
        p.extend_from_slice(&[0, 0, 7]);
        p.extend_from_slice(&[5, 5]);
        let mut tiles = Vec::new();
        let tg = tile_group(&p, &layout, &mut tiles).unwrap();
        assert_eq!((tg.tg_start, tg.tg_end, tg.data_offset), (1, 3, 1));
        assert_eq!(
            tiles,
            [
                Tile { offset: 2, size: 3, row: 0, col: 1 },
                Tile { offset: 7, size: 1, row: 1, col: 0 },
                Tile { offset: 8, size: 2, row: 1, col: 1 },
            ]
        );
        let mut short = p.clone();
        short[1] = 40;
        assert_eq!(tile_group(&short, &layout, &mut tiles), Err(Av1Error::TileSize));
        let one = TileLayout { cols: 1, rows: 1, cols_log2: 0, rows_log2: 0, size_bytes: 4 };
        tiles.clear();
        let tg = tile_group(&[1, 2, 3], &one, &mut tiles).unwrap();
        assert_eq!((tg.tg_start, tg.tg_end, tg.data_offset), (0, 0, 0));
        assert_eq!(tiles, [Tile { offset: 0, size: 3, row: 0, col: 0 }]);
    }
}
