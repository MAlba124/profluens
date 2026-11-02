//! Native FLAC (RFC 9639), the STREAMINFO, the SEEKTABLE and the frame header. Frames carry
//! their own sample or frame number, so a frame found anywhere has an exact stamp.

/// STREAMINFO body bytes, RFC 9639 8.2.
pub const STREAMINFO_LEN: usize = 34;
/// A frame header is at most this long, sync to CRC-8 with a 7 byte number and both
/// uncommon fields.
pub const MAX_HEADER: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamInfo {
    pub min_block: u16,
    pub max_block: u16,
    /// Frame sizes in bytes, 0 unknown.
    pub min_frame: u32,
    pub max_frame: u32,
    pub rate: u32,
    pub channels: u8,
    pub bits: u8,
    /// 0 unknown.
    pub total: u64,
}

pub fn stream_info(b: &[u8]) -> Option<StreamInfo> {
    let b = b.get(..STREAMINFO_LEN)?;
    let u24 = |i: usize| u32::from_be_bytes([0, b[i], b[i + 1], b[i + 2]]);
    let packed = u64::from_be_bytes([b[10], b[11], b[12], b[13], b[14], b[15], b[16], b[17]]);
    let s = StreamInfo {
        min_block: u16::from_be_bytes([b[0], b[1]]),
        max_block: u16::from_be_bytes([b[2], b[3]]),
        min_frame: u24(4),
        max_frame: u24(7),
        rate: (packed >> 44) as u32,
        channels: ((packed >> 41) & 7) as u8 + 1,
        bits: ((packed >> 36) & 0x1F) as u8 + 1,
        total: packed & 0xF_FFFF_FFFF,
    };
    (s.rate > 0 && s.max_block >= 16).then_some(s)
}

/// The track head, as Matroska's CodecPrivate holds it, `fLaC` and the STREAMINFO block
/// flagged last.
// Cold, once per open.
#[allow(clippy::disallowed_methods)]
pub fn codec_head(streaminfo: &[u8; STREAMINFO_LEN]) -> Vec<u8> {
    let mut h = Vec::with_capacity(8 + STREAMINFO_LEN);
    h.extend_from_slice(b"fLaC");
    h.extend_from_slice(&[0x80, 0, 0, STREAMINFO_LEN as u8]);
    h.extend_from_slice(streaminfo);
    h
}

/// A parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// The first sample of the frame.
    pub sample: u64,
    pub block: u32,
    /// Header bytes, sync through CRC-8.
    pub len: usize,
    /// Variable blocking, the number is a sample number.
    pub variable: bool,
}

const CRC8: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

pub fn crc8(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |c, &x| CRC8[(c ^ x) as usize])
}

/// The UTF-8 style coded number of RFC 9639 9.1.5, its value and length.
fn coded(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let n = match first.leading_ones() {
        0 => return Some((u64::from(first), 1)),
        1 | 8 => return None,
        n => n as usize,
    };
    // a 7 byte number keeps no bits of the first byte, a u8 shift by 8 would overflow
    let mut v = u64::from(first) & (0xFF >> (n + 1));
    for &c in b.get(1..n)? {
        if c & 0xC0 != 0x80 {
            return None;
        }
        v = (v << 6) | u64::from(c & 0x3F);
    }
    Some((v, n))
}

/// The frame header at the start of `b` when it is one of this stream, RFC 9639 9.1. The
/// CRC-8 must hold and the rate, channels and depth must be the STREAMINFO's.
pub fn frame(b: &[u8], si: &StreamInfo) -> Option<Frame> {
    if b.len() < 6 || b[0] != 0xFF || b[1] & 0xFE != 0xF8 {
        return None;
    }
    let variable = b[1] & 1 == 1;
    let (bs, sr) = (b[2] >> 4, b[2] & 0xF);
    let (ch, depth) = (b[3] >> 4, (b[3] >> 1) & 7);
    if bs == 0 || sr == 15 || b[3] & 1 != 0 || depth == 3 || ch > 10 {
        return None;
    }
    let channels = if ch < 8 { ch + 1 } else { 2 };
    if channels != si.channels {
        return None;
    }
    let bits = match depth {
        0 => si.bits,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        _ => 32,
    };
    if bits != si.bits {
        return None;
    }
    let (num, n) = coded(&b[4..])?;
    if !variable && num >= 1 << 31 {
        return None;
    }
    let mut at = 4 + n;
    let block = match bs {
        1 => 192,
        2..=5 => 576 << (bs - 2),
        6 => {
            at += 1;
            u32::from(*b.get(at - 1)?) + 1
        }
        7 => {
            at += 2;
            u32::from(u16::from_be_bytes([*b.get(at - 2)?, *b.get(at - 1)?])) + 1
        }
        _ => 256 << (bs - 8),
    };
    let rate = match sr {
        0 => si.rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => {
            at += 1;
            u32::from(*b.get(at - 1)?) * 1000
        }
        13 => {
            at += 2;
            u32::from(u16::from_be_bytes([*b.get(at - 2)?, *b.get(at - 1)?]))
        }
        _ => {
            at += 2;
            u32::from(u16::from_be_bytes([*b.get(at - 2)?, *b.get(at - 1)?])) * 10
        }
    };
    if rate != si.rate || block > u32::from(si.max_block) {
        return None;
    }
    if crc8(b.get(..at)?) != *b.get(at)? {
        return None;
    }
    let sample = if variable { num } else { num * u64::from(si.max_block) };
    Some(Frame { sample, block, len: at + 1, variable })
}

/// A SEEKTABLE body as (sample, offset from the first frame), placeholders and points that
/// do not move on left out, RFC 9639 8.5.
// Cold, once per open.
#[allow(clippy::disallowed_methods)]
pub fn seek_points(body: &[u8]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(body.len() / 18);
    for p in body.as_chunks::<18>().0 {
        let s = u64::from_be_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]]);
        let o = u64::from_be_bytes([p[8], p[9], p[10], p[11], p[12], p[13], p[14], p[15]]);
        if s == u64::MAX || out.last().is_some_and(|l| s <= l.0 || o <= l.1) {
            continue;
        }
        out.push((s, o));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn si() -> StreamInfo {
        StreamInfo {
            min_block: 4096,
            max_block: 4096,
            rate: 44_100,
            channels: 2,
            bits: 16,
            ..StreamInfo::default()
        }
    }

    #[test]
    fn crc8_matches_the_rfc_check_value() {
        // CRC-8 poly 0x07 init 0 of "123456789"
        assert_eq!(crc8(b"123456789"), 0xF4);
    }

    #[test]
    fn coded_numbers() {
        assert_eq!(coded(&[0x05]), Some((5, 1)));
        assert_eq!(coded(&[0xC2, 0x80]), Some((0x80, 2)));
        assert_eq!(coded(&[0xE0, 0xA0, 0x80]), Some((0x800, 3)));
        assert_eq!(coded(&[0x80]), None, "a continuation byte first");
        assert_eq!(coded(&[0xC2, 0x00]), None);
        assert_eq!(coded(&[0xFF]), None);
        assert_eq!(
            coded(&[0xFE, 0x81, 0x80, 0x80, 0x80, 0x80, 0x80]),
            Some((1 << 30, 7)),
            "7 bytes, no bits in the first"
        );
        assert_eq!(coded(&[0xFE, 0x80]), None, "7 bytes cut short");
    }

    #[test]
    fn headers_parse_and_refuse_what_is_not_this_stream() {
        let h = crate::fixture::flac_frame_header(&si(), 7, 4096);
        let f = frame(&h, &si()).unwrap();
        assert_eq!((f.sample, f.block, f.len, f.variable), (7 * 4096, 4096, h.len(), false));
        let short = crate::fixture::flac_frame_header(&si(), 9, 1000);
        assert_eq!(frame(&short, &si()).unwrap().block, 1000, "an uncommon size, the last frame");
        let mut bad = h.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(frame(&bad, &si()), None, "a broken CRC-8");
        let mono = StreamInfo { channels: 1, ..si() };
        assert_eq!(frame(&h, &mono), None, "other channels");
        let other = StreamInfo { rate: 48_000, ..si() };
        assert_eq!(frame(&h, &other), None, "another rate");
    }

    #[test]
    fn streaminfo_and_head() {
        let b: [u8; 34] = [
            0x10, 0, 0x10, 0, 0, 0, 0x0e, 0, 0x30, 0, 0x0a, 0xc4, 0x42, 0xf0, 0, 0x02, 0xb1, 0x10,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let s = stream_info(&b).unwrap();
        assert_eq!((s.min_block, s.max_block, s.min_frame, s.max_frame), (4096, 4096, 14, 0x3000));
        assert_eq!((s.rate, s.channels, s.bits, s.total), (44_100, 2, 16, 0x2b110));
        let h = codec_head(&b);
        assert_eq!(&h[..8], b"fLaC\x80\x00\x00\x22");
        assert_eq!(&h[8..], &b);
    }

    #[test]
    fn seek_points_skip_placeholders_and_disorder() {
        let mut t = Vec::new();
        for (s, o) in
            [(0u64, 0u64), (44_100, 9_000), (u64::MAX, 0), (30_000, 20_000), (88_200, 18_000)]
        {
            t.extend_from_slice(&s.to_be_bytes());
            t.extend_from_slice(&o.to_be_bytes());
            t.extend_from_slice(&4096u16.to_be_bytes());
        }
        assert_eq!(seek_points(&t), [(0, 0), (44_100, 9_000), (88_200, 18_000)]);
    }
}
