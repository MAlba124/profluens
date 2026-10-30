//! ISO-BMFF box grammar, read side (ISO/IEC 14496-12). Every length is checked against
//! the remaining window, so bad input is a [`BoxError`], never a panic.

// Cold path, once per stream at preroll.
#![allow(clippy::disallowed_methods)]

pub type FourCc = [u8; 4];

pub const fn fourcc(s: &[u8; 4]) -> FourCc {
    [s[0], s[1], s[2], s[3]]
}

pub mod boxtype {
    use super::{FourCc, fourcc};

    pub const FTYP: FourCc = fourcc(b"ftyp"); // 4.3 File Type
    pub const MOOV: FourCc = fourcc(b"moov"); // 8.2.1 Movie
    pub const MVHD: FourCc = fourcc(b"mvhd"); // 8.2.2 Movie Header
    pub const TRAK: FourCc = fourcc(b"trak"); // 8.3.1 Track
    pub const TKHD: FourCc = fourcc(b"tkhd"); // 8.3.2 Track Header
    pub const EDTS: FourCc = fourcc(b"edts"); // 8.6.5 Edit
    pub const ELST: FourCc = fourcc(b"elst"); // 8.6.6 Edit List
    pub const MDIA: FourCc = fourcc(b"mdia"); // 8.4.1 Media
    pub const MDHD: FourCc = fourcc(b"mdhd"); // 8.4.2 Media Header
    pub const HDLR: FourCc = fourcc(b"hdlr"); // 8.4.3 Handler Reference
    pub const UDTA: FourCc = fourcc(b"udta"); // 8.10.1 User Data
    pub const NAME: FourCc = fourcc(b"name"); // a track's title, as QuickTime and ffmpeg write it
    pub const KIND: FourCc = fourcc(b"kind"); // 8.10.4 Track Kind
    pub const ELNG: FourCc = fourcc(b"elng"); // 8.4.6 Extended Language Tag
    pub const MINF: FourCc = fourcc(b"minf"); // 8.4.4 Media Information
    pub const STBL: FourCc = fourcc(b"stbl"); // 8.5.1 Sample Table

    pub const STSD: FourCc = fourcc(b"stsd"); // 8.5.2 Sample Description
    pub const STTS: FourCc = fourcc(b"stts"); // 8.6.1.2 Decoding Time to Sample
    pub const CTTS: FourCc = fourcc(b"ctts"); // 8.6.1.3 Composition Time to Sample
    pub const STSZ: FourCc = fourcc(b"stsz"); // 8.7.3.2 Sample Size
    pub const STZ2: FourCc = fourcc(b"stz2"); // 8.7.3.3 Compact Sample Size
    pub const STSC: FourCc = fourcc(b"stsc"); // 8.7.4 Sample to Chunk
    pub const STCO: FourCc = fourcc(b"stco"); // 8.7.5 Chunk Offset (32-bit)
    pub const CO64: FourCc = fourcc(b"co64"); // 8.7.5 Chunk Offset (64-bit)
    pub const STSS: FourCc = fourcc(b"stss"); // 8.6.2 Sync Sample

    pub const MVEX: FourCc = fourcc(b"mvex"); // 8.8.1 Movie Extends
    pub const MEHD: FourCc = fourcc(b"mehd"); // 8.8.2 Movie Extends Header
    pub const TREX: FourCc = fourcc(b"trex"); // 8.8.3 Track Extends
    pub const MOOF: FourCc = fourcc(b"moof"); // 8.8.4 Movie Fragment
    pub const TRAF: FourCc = fourcc(b"traf"); // 8.8.6 Track Fragment
    pub const TFHD: FourCc = fourcc(b"tfhd"); // 8.8.7 Track Fragment Header
    pub const TRUN: FourCc = fourcc(b"trun"); // 8.8.8 Track Fragment Run
    pub const MFRA: FourCc = fourcc(b"mfra"); // 8.8.9 Movie Fragment Random Access
    pub const TFRA: FourCc = fourcc(b"tfra"); // 8.8.10 Track Fragment Random Access
    pub const TFDT: FourCc = fourcc(b"tfdt"); // 8.8.12 Track Fragment Decode Time
    pub const SIDX: FourCc = fourcc(b"sidx"); // 8.16.3 Segment Index

    pub const MDAT: FourCc = fourcc(b"mdat"); // 8.1.1 Media Data
}

/// A parse failure. The box layer always parses a fully buffered slice.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BoxError {
    Truncated(&'static str),
    Malformed(&'static str),
}

impl std::fmt::Display for BoxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoxError::Truncated(what) => write!(f, "truncated, {what}"),
            BoxError::Malformed(what) => write!(f, "malformed, {what}"),
        }
    }
}

impl std::error::Error for BoxError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: FourCc,
    pub body_start: usize,
    pub end: usize,
}

impl BoxHeader {
    /// Cannot panic for a header [`read_box_header`] produced from `data`.
    pub fn body<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.body_start..self.end]
    }
}

/// Read one box header at `data[at]` (4.2). `size == 0` runs to the end of `data`.
pub fn read_box_header(data: &[u8], at: usize) -> Result<BoxHeader, BoxError> {
    let mut r = Cursor::at(data, at);
    let size32 = r.u32("box size")? as u64;
    let kind_bytes = r.take(4, "box type")?;
    let kind: FourCc = [kind_bytes[0], kind_bytes[1], kind_bytes[2], kind_bytes[3]];

    // 4.2, 1 means a 64-bit largesize, 0 means to end of container.
    let total = match size32 {
        1 => r.u64("box largesize")?,
        0 => (data.len() - at) as u64,
        n => n,
    };
    if kind == fourcc(b"uuid") {
        r.skip(16, "uuid usertype")?;
    }
    let body_start = r.at;
    let header_len = (body_start - at) as u64;
    if total < header_len {
        return Err(BoxError::Malformed("box size smaller than its own header"));
    }
    let end = (at as u64)
        .checked_add(total)
        .ok_or(BoxError::Malformed("box end overflows the address space"))?;
    if end > data.len() as u64 {
        return Err(BoxError::Truncated("box runs past the buffer"));
    }
    Ok(BoxHeader { kind, body_start, end: end as usize })
}

pub fn for_each_child<F: FnMut(&BoxHeader) -> Result<(), BoxError>>(
    data: &[u8],
    mut f: F,
) -> Result<(), BoxError> {
    let mut at = 0usize;
    while at + 8 <= data.len() {
        let h = read_box_header(data, at)?;
        // A box's size is at least its header, so `at` strictly increases.
        debug_assert!(h.end > at, "box header must advance the cursor");
        f(&h)?;
        at = h.end;
    }
    Ok(())
}

/// The first child of `kind`, a sibling malformed behind it is not read.
pub fn find_child(data: &[u8], kind: FourCc) -> Result<Option<BoxHeader>, BoxError> {
    let mut at = 0usize;
    while at + 8 <= data.len() {
        let h = read_box_header(data, at)?;
        if h.kind == kind {
            return Ok(Some(h));
        }
        at = h.end;
    }
    Ok(None)
}

pub(crate) struct FullBoxHead {
    pub(crate) version: u8,
    pub(crate) flags: u32,
}

pub(crate) fn read_full_box_head(r: &mut Cursor) -> Result<FullBoxHead, BoxError> {
    let version = r.u8("fullbox version")?;
    let hi = r.u8("fullbox flags")? as u32;
    let mid = r.u8("fullbox flags")? as u32;
    let lo = r.u8("fullbox flags")? as u32;
    Ok(FullBoxHead { version, flags: (hi << 16) | (mid << 8) | lo })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mvhd {
    /// Movie timescale, the unit of `mvhd.duration` and `elst` segment durations.
    pub timescale: u32,
    pub duration: u64,
}

pub fn parse_mvhd(body: &[u8]) -> Result<Mvhd, BoxError> {
    let mut r = Cursor::new(body);
    let head = read_full_box_head(&mut r)?;
    match head.version {
        0 => {
            r.skip(8, "mvhd v0 creation/modification time")?;
            let timescale = r.u32("mvhd timescale")?;
            let duration = r.u32("mvhd duration")? as u64;
            Ok(Mvhd { timescale, duration })
        }
        1 => {
            r.skip(16, "mvhd v1 creation/modification time")?;
            let timescale = r.u32("mvhd timescale")?;
            let duration = r.u64("mvhd duration")?;
            Ok(Mvhd { timescale, duration })
        }
        _ => Err(BoxError::Malformed("unsupported mvhd version")),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tkhd {
    pub track_id: u32,
    pub width: u32,
    pub height: u32,
    pub enabled: bool,
    pub alternate_group: i16,
}

pub fn parse_tkhd(body: &[u8]) -> Result<Tkhd, BoxError> {
    let mut r = Cursor::new(body);
    let head = read_full_box_head(&mut r)?;
    let track_id = match head.version {
        0 => {
            r.skip(8, "tkhd v0 creation/modification time")?;
            let id = r.u32("tkhd v0 track_ID")?;
            r.skip(4, "tkhd v0 reserved")?;
            r.skip(4, "tkhd v0 duration")?;
            id
        }
        1 => {
            r.skip(16, "tkhd v1 creation/modification time")?;
            let id = r.u32("tkhd v1 track_ID")?;
            r.skip(4, "tkhd v1 reserved")?;
            r.skip(8, "tkhd v1 duration")?;
            id
        }
        _ => return Err(BoxError::Malformed("unsupported tkhd version")),
    };
    // reserved[2] u32, layer, alternate_group, volume, reserved u16, matrix i32[9] (8.3.2).
    r.skip(10, "tkhd reserved and layer")?;
    let alternate_group = r.u16("tkhd alternate_group")? as i16;
    r.skip(40, "tkhd volume and matrix")?;
    let width = r.u32("tkhd width")? >> 16; // 16.16 fixed-point to integer pixels
    let height = r.u32("tkhd height")? >> 16;
    Ok(Tkhd { track_id, width, height, enabled: head.flags & 1 != 0, alternate_group })
}

pub fn parse_kind(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = body.get(4..)?;
    let end = rest.iter().position(|b| *b == 0)?;
    let (scheme, rest) = (&rest[..end], &rest[end + 1..]);
    let value = rest.split(|b| *b == 0).next().unwrap_or(rest);
    Some((scheme, value))
}

pub fn parse_elng(body: &[u8]) -> Option<&str> {
    let tag = body.get(4..)?.split(|b| *b == 0).next()?;
    std::str::from_utf8(tag).ok().filter(|t| !t.is_empty())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mdhd {
    pub timescale: u32,
    pub duration: u64,
    pub language: [u8; 3],
}

/// Three 5-bit letters, each less 0x60 (8.4.2). Anything else reads as `und`.
fn mdhd_language(packed: u16) -> [u8; 3] {
    let letter = |shift: u16| ((packed >> shift) & 0x1f) as u8 + 0x60;
    let code = [letter(10), letter(5), letter(0)];
    if code.iter().all(u8::is_ascii_lowercase) { code } else { *b"und" }
}

pub fn parse_hdlr_name(body: &[u8]) -> &str {
    let name = body.get(24..).unwrap_or(&[]);
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    std::str::from_utf8(&name[..end]).unwrap_or("")
}

pub fn parse_mdhd(body: &[u8]) -> Result<Mdhd, BoxError> {
    let mut r = Cursor::new(body);
    let head = read_full_box_head(&mut r)?;
    match head.version {
        0 => {
            r.skip(8, "mdhd v0 creation/modification time")?;
            let timescale = r.u32("mdhd timescale")?;
            // All ones is unknown (8.4.2), some fragmenters write it.
            let duration = match r.u32("mdhd duration")? {
                u32::MAX => 0,
                d => u64::from(d),
            };
            let language = mdhd_language(r.u16("mdhd language").unwrap_or(0));
            Ok(Mdhd { timescale, duration, language })
        }
        1 => {
            r.skip(16, "mdhd v1 creation/modification time")?;
            let timescale = r.u32("mdhd timescale")?;
            let duration = match r.u64("mdhd duration")? {
                u64::MAX => 0,
                d => d,
            };
            let language = mdhd_language(r.u16("mdhd language").unwrap_or(0));
            Ok(Mdhd { timescale, duration, language })
        }
        _ => Err(BoxError::Malformed("unsupported mdhd version")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElstEntry {
    pub segment_duration: u64,
    /// Start in media ticks, `-1` marks an empty edit.
    pub media_time: i64,
    pub media_rate: i32,
}

pub fn parse_elst(body: &[u8]) -> Result<Vec<ElstEntry>, BoxError> {
    let mut r = Cursor::new(body);
    let head = read_full_box_head(&mut r)?;
    let count = r.u32("elst entry_count")? as usize;
    let mut entries = Vec::with_capacity(r.rows(count, 12));
    for _ in 0..count {
        let (segment_duration, media_time) = match head.version {
            0 => (r.u32("elst v0 segment_duration")? as u64, r.i32("elst v0 media_time")? as i64),
            1 => (r.u64("elst v1 segment_duration")?, r.i64("elst v1 media_time")?),
            _ => return Err(BoxError::Malformed("unsupported elst version")),
        };
        let media_rate = r.i32("elst media_rate")?;
        entries.push(ElstEntry { segment_duration, media_time, media_rate });
    }
    Ok(entries)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SttsEntry {
    pub count: u32,
    pub delta: u32,
}

pub fn parse_stts(body: &[u8]) -> Result<Vec<SttsEntry>, BoxError> {
    let mut r = Cursor::new(body);
    let _head = read_full_box_head(&mut r)?;
    let count = r.u32("stts entry_count")? as usize;
    let mut out = Vec::with_capacity(r.rows(count, 8));
    for _ in 0..count {
        let c = r.u32("stts sample_count")?;
        let d = r.u32("stts sample_delta")?;
        out.push(SttsEntry { count: c, delta: d });
    }
    Ok(out)
}

/// One `ctts` run (8.6.1.3). Version 1 offsets are signed, so both versions are kept as i64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CttsEntry {
    pub count: u32,
    pub offset: i64,
}

pub fn parse_ctts(body: &[u8]) -> Result<Vec<CttsEntry>, BoxError> {
    let mut r = Cursor::new(body);
    let head = read_full_box_head(&mut r)?;
    let count = r.u32("ctts entry_count")? as usize;
    let mut out = Vec::with_capacity(r.rows(count, 8));
    for _ in 0..count {
        let c = r.u32("ctts sample_count")?;
        let offset = match head.version {
            0 => r.u32("ctts v0 sample_offset")? as i64,
            1 => r.i32("ctts v1 sample_offset")? as i64,
            _ => return Err(BoxError::Malformed("unsupported ctts version")),
        };
        out.push(CttsEntry { count: c, offset });
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SampleSizes {
    Constant { sample_size: u32, sample_count: u32 },
    Table(Vec<u32>),
}

impl SampleSizes {
    pub fn count(&self) -> usize {
        match self {
            SampleSizes::Constant { sample_count, .. } => *sample_count as usize,
            SampleSizes::Table(v) => v.len(),
        }
    }

    pub fn get(&self, i: usize) -> Option<u32> {
        match self {
            SampleSizes::Constant { sample_size, sample_count } => {
                (i < *sample_count as usize).then_some(*sample_size)
            }
            SampleSizes::Table(v) => v.get(i).copied(),
        }
    }
}

pub fn parse_stsz(body: &[u8]) -> Result<SampleSizes, BoxError> {
    let mut r = Cursor::new(body);
    let _head = read_full_box_head(&mut r)?;
    let sample_size = r.u32("stsz sample_size")?;
    let sample_count = r.u32("stsz sample_count")?;
    if sample_size != 0 {
        return Ok(SampleSizes::Constant { sample_size, sample_count });
    }
    let n = sample_count as usize;
    let mut sizes = Vec::with_capacity(r.rows(n, 4));
    for _ in 0..n {
        sizes.push(r.u32("stsz entry_size")?);
    }
    Ok(SampleSizes::Table(sizes))
}

pub fn parse_stz2(body: &[u8]) -> Result<SampleSizes, BoxError> {
    let mut r = Cursor::new(body);
    let _head = read_full_box_head(&mut r)?;
    r.skip(3, "stz2 reserved")?; // reserved u24
    let field_size = r.u8("stz2 field_size")?;
    let sample_count = r.u32("stz2 sample_count")? as usize;
    let mut sizes = Vec::with_capacity(r.rows(sample_count, 1).saturating_mul(2).min(sample_count));
    match field_size {
        16 => {
            for _ in 0..sample_count {
                sizes.push(r.u16("stz2 16-bit entry")? as u32);
            }
        }
        8 => {
            for _ in 0..sample_count {
                sizes.push(r.u8("stz2 8-bit entry")? as u32);
            }
        }
        4 => {
            // Two 4-bit sizes per octet, high nibble first (8.7.3.3).
            let mut i = 0;
            while i < sample_count {
                let byte = r.u8("stz2 4-bit pair")?;
                sizes.push((byte >> 4) as u32);
                i += 1;
                if i < sample_count {
                    sizes.push((byte & 0x0F) as u32);
                    i += 1;
                }
            }
        }
        _ => return Err(BoxError::Malformed("stz2 field_size must be 4, 8, or 16")),
    }
    Ok(SampleSizes::Table(sizes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StscEntry {
    pub first_chunk: u32,
    pub samples_per_chunk: u32,
    pub sample_description_index: u32,
}

pub fn parse_stsc(body: &[u8]) -> Result<Vec<StscEntry>, BoxError> {
    let mut r = Cursor::new(body);
    let _head = read_full_box_head(&mut r)?;
    let count = r.u32("stsc entry_count")? as usize;
    let mut out = Vec::with_capacity(r.rows(count, 12));
    for _ in 0..count {
        out.push(StscEntry {
            first_chunk: r.u32("stsc first_chunk")?,
            samples_per_chunk: r.u32("stsc samples_per_chunk")?,
            sample_description_index: r.u32("stsc sample_description_index")?,
        });
    }
    Ok(out)
}

pub fn parse_chunk_offsets(body: &[u8], is_64: bool) -> Result<Vec<u64>, BoxError> {
    let mut r = Cursor::new(body);
    let _head = read_full_box_head(&mut r)?;
    let count = r.u32("chunk-offset entry_count")? as usize;
    let mut out = Vec::with_capacity(r.rows(count, if is_64 { 8 } else { 4 }));
    for _ in 0..count {
        out.push(if is_64 { r.u64("co64 offset")? } else { r.u32("stco offset")? as u64 });
    }
    Ok(out)
}

/// Parse an `stss` payload (8.6.2). An absent `stss` means every sample is sync.
pub fn parse_stss(body: &[u8]) -> Result<Vec<u32>, BoxError> {
    let mut r = Cursor::new(body);
    let _head = read_full_box_head(&mut r)?;
    let count = r.u32("stss entry_count")? as usize;
    let mut out = Vec::with_capacity(r.rows(count, 4));
    for _ in 0..count {
        out.push(r.u32("stss sample_number")?);
    }
    Ok(out)
}

pub(crate) struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn at(data: &'a [u8], at: usize) -> Self {
        Self { data, at }
    }

    pub(crate) fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], BoxError> {
        let end = self.at.checked_add(n).ok_or(BoxError::Malformed(what))?;
        let s = self.data.get(self.at..end).ok_or(BoxError::Truncated(what))?;
        self.at = end;
        Ok(s)
    }

    pub(crate) fn skip(&mut self, n: usize, what: &'static str) -> Result<(), BoxError> {
        let _ = self.take(n, what)?;
        Ok(())
    }

    /// How many of `declared` rows of `width` octets the box can hold. Capacity is reserved
    /// from this, as an untrusted entry_count may claim four billion rows.
    pub(crate) fn rows(&self, declared: usize, width: usize) -> usize {
        declared.min((self.data.len() - self.at) / width.max(1))
    }

    pub(crate) fn u8(&mut self, what: &'static str) -> Result<u8, BoxError> {
        Ok(self.take(1, what)?[0])
    }

    pub(crate) fn u16(&mut self, what: &'static str) -> Result<u16, BoxError> {
        let b = self.take(2, what)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self, what: &'static str) -> Result<u32, BoxError> {
        let b = self.take(4, what)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn i32(&mut self, what: &'static str) -> Result<i32, BoxError> {
        Ok(self.u32(what)? as i32)
    }

    pub(crate) fn u64(&mut self, what: &'static str) -> Result<u64, BoxError> {
        let b = self.take(8, what)?;
        Ok(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    pub(crate) fn i64(&mut self, what: &'static str) -> Result<i64, BoxError> {
        Ok(self.u64(what)? as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = 8 + body.len();
        let mut out = (total as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn track_header_kind_and_extended_language() {
        let mut tkhd = vec![0, 0, 0, 3];
        tkhd.extend_from_slice(&[0; 8]);
        tkhd.extend_from_slice(&7u32.to_be_bytes());
        tkhd.extend_from_slice(&[0; 8]);
        tkhd.extend_from_slice(&[0; 10]);
        tkhd.extend_from_slice(&2u16.to_be_bytes());
        tkhd.extend_from_slice(&[0; 40]);
        tkhd.extend_from_slice(&(640u32 << 16).to_be_bytes());
        tkhd.extend_from_slice(&(360u32 << 16).to_be_bytes());
        let t = parse_tkhd(&tkhd).unwrap();
        assert_eq!(
            t,
            Tkhd { track_id: 7, width: 640, height: 360, enabled: true, alternate_group: 2 }
        );
        tkhd[3] = 2;
        assert!(!parse_tkhd(&tkhd).unwrap().enabled, "in the movie, not enabled");

        let kind = b"\0\0\0\0urn:mpeg:dash:role:2011\0commentary\0";
        assert_eq!(parse_kind(kind), Some((&b"urn:mpeg:dash:role:2011"[..], &b"commentary"[..])));
        assert_eq!(parse_kind(b"\0\0\0\0no terminator"), None);
        assert_eq!(parse_elng(b"\0\0\0\0fr-CA\0"), Some("fr-CA"));
        assert_eq!(parse_elng(b"\0\0\0\0\0"), None);
    }

    #[test]
    fn box_header_basic_and_largesize() {
        let b = boxed(b"moov", &[1, 2, 3, 4]);
        let h = read_box_header(&b, 0).unwrap();
        assert_eq!(h.kind, boxtype::MOOV);
        assert_eq!(h.body_start, 8);
        assert_eq!(h.end, b.len());
        assert_eq!(h.body(&b), &[1, 2, 3, 4]);

        let mut lb = 1u32.to_be_bytes().to_vec();
        lb.extend_from_slice(b"mdat");
        lb.extend_from_slice(&(16u64 + 2).to_be_bytes()); // total = 16 header + 2 body
        lb.extend_from_slice(&[0xAA, 0xBB]);
        let h = read_box_header(&lb, 0).unwrap();
        assert_eq!(h.kind, boxtype::MDAT);
        assert_eq!(h.body_start, 16);
        assert_eq!(h.body(&lb), &[0xAA, 0xBB]);
    }

    #[test]
    fn box_size_zero_runs_to_end() {
        let mut b = 0u32.to_be_bytes().to_vec(); // size 0 runs to end of buffer
        b.extend_from_slice(b"mdat");
        b.extend_from_slice(&[9, 9, 9]);
        let h = read_box_header(&b, 0).unwrap();
        assert_eq!(h.end, b.len());
        assert_eq!(h.body(&b), &[9, 9, 9]);
    }

    #[test]
    fn child_walk_visits_each_box_in_order() {
        let mut container = Vec::new();
        container.extend_from_slice(&boxed(b"mvhd", &[0; 4]));
        container.extend_from_slice(&boxed(b"trak", &[0; 8]));
        container.extend_from_slice(&boxed(b"udta", &[0; 2]));
        let mut kinds = Vec::new();
        for_each_child(&container, |h| {
            kinds.push(h.kind);
            Ok(())
        })
        .unwrap();
        assert_eq!(kinds, vec![boxtype::MVHD, boxtype::TRAK, fourcc(b"udta")]);
        assert_eq!(find_child(&container, boxtype::TRAK).unwrap().unwrap().kind, boxtype::TRAK);
        assert!(find_child(&container, boxtype::MOOV).unwrap().is_none());
    }

    #[test]
    fn stts_ctts_stsz_stsc_roundtrip() {
        let mut stts = vec![0u8; 4]; // version+flags
        stts.extend_from_slice(&2u32.to_be_bytes());
        stts.extend_from_slice(&3u32.to_be_bytes());
        stts.extend_from_slice(&100u32.to_be_bytes());
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&50u32.to_be_bytes());
        let e = parse_stts(&stts).unwrap();
        assert_eq!(e, vec![SttsEntry { count: 3, delta: 100 }, SttsEntry { count: 1, delta: 50 }]);

        let mut ctts = vec![1u8, 0, 0, 0]; // version 1
        ctts.extend_from_slice(&1u32.to_be_bytes());
        ctts.extend_from_slice(&2u32.to_be_bytes());
        ctts.extend_from_slice(&(-40i32).to_be_bytes());
        let c = parse_ctts(&ctts).unwrap();
        assert_eq!(c, vec![CttsEntry { count: 2, offset: -40 }]);

        let mut stsz = vec![0u8; 4];
        stsz.extend_from_slice(&7u32.to_be_bytes()); // sample_size
        stsz.extend_from_slice(&5u32.to_be_bytes()); // sample_count
        let s = parse_stsz(&stsz).unwrap();
        assert_eq!(s, SampleSizes::Constant { sample_size: 7, sample_count: 5 });
        assert_eq!(s.count(), 5);
        assert_eq!(s.get(4), Some(7));
        assert_eq!(s.get(5), None);

        let mut stsc = vec![0u8; 4];
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
        stsc.extend_from_slice(&4u32.to_be_bytes()); // samples_per_chunk
        stsc.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
        let sc = parse_stsc(&stsc).unwrap();
        assert_eq!(
            sc,
            vec![StscEntry { first_chunk: 1, samples_per_chunk: 4, sample_description_index: 1 }]
        );
    }

    #[test]
    fn stz2_packs_field_sizes() {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(&[0, 0, 0]); // reserved u24
        b.push(4); // field_size
        b.extend_from_slice(&3u32.to_be_bytes());
        b.extend_from_slice(&[0x12, 0x30]);
        let s = parse_stz2(&b).unwrap();
        assert_eq!(s, SampleSizes::Table(vec![1, 2, 3]));
    }

    #[test]
    fn malformed_boxes_error_not_panic() {
        assert!(read_box_header(&[0, 0, 0], 0).is_err());
        let mut b = 4u32.to_be_bytes().to_vec();
        b.extend_from_slice(b"moov");
        assert!(read_box_header(&b, 0).is_err(), "size 4 < 8-byte header");
        let mut c = 100u32.to_be_bytes().to_vec();
        c.extend_from_slice(b"trak");
        assert!(for_each_child(&c, |_| Ok(())).is_err());
        // stsz claiming a huge table with no data to error, not a 4 GiB alloc.
        let mut stsz = vec![0u8; 4];
        stsz.extend_from_slice(&0u32.to_be_bytes()); // per-sample table
        stsz.extend_from_slice(&u32::MAX.to_be_bytes()); // sample_count
        assert!(parse_stsz(&stsz).is_err());
    }

    #[test]
    fn an_all_ones_mdhd_duration_is_unknown() {
        let mut v0 = vec![0u8; 4];
        v0.extend([0u8; 8]);
        v0.extend(1000u32.to_be_bytes());
        v0.extend(u32::MAX.to_be_bytes());
        v0.extend([0u8; 4]);
        assert_eq!(parse_mdhd(&v0).unwrap().duration, 0);
        let mut v1 = vec![1u8, 0, 0, 0];
        v1.extend([0u8; 16]);
        v1.extend(1000u32.to_be_bytes());
        v1.extend(u64::MAX.to_be_bytes());
        v1.extend([0u8; 4]);
        assert_eq!(parse_mdhd(&v1).unwrap().duration, 0);
        v1[24..32].copy_from_slice(&7u64.to_be_bytes());
        assert_eq!(parse_mdhd(&v1).unwrap().duration, 7);
    }

    #[test]
    fn find_child_stops_at_the_first_match_before_a_bad_sibling() {
        let mut data = 12u32.to_be_bytes().to_vec();
        data.extend(b"abcd");
        data.extend([1, 2, 3, 4]);
        data.extend(4u32.to_be_bytes());
        data.extend(b"bad!");
        let h = find_child(&data, fourcc(b"abcd")).unwrap().expect("the first child");
        assert_eq!(h.body(&data), [1, 2, 3, 4]);
        assert!(find_child(&data, fourcc(b"none")).is_err(), "the bad sibling is still met");
    }
}
