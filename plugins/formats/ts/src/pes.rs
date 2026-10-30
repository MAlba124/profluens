//! PES packet header, H.222.0 2.4.3.6 and 2.4.3.7.

pub const PES_HEADER_MAX: usize = 9 + 255;

pub const PH_DATA_ALIGNED: u8 = 0x01;
pub const PH_SCRAMBLED: u8 = 0x02;
/// The timestamp fields are present but malformed, so none is reported.
pub const PH_BAD_TIMESTAMP: u8 = 0x04;
pub const PH_PADDING: u8 = 0x08;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PesHeader {
    /// 90 kHz ticks.
    pub pts: Option<u64>,
    /// 90 kHz ticks.
    pub dts: Option<u64>,
    /// `None` when PES_packet_length is zero.
    pub payload_len: Option<u32>,
    pub header_len: u16,
    pub stream_id: u8,
    /// `PH_*` bits.
    pub flags: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PesError {
    StartCode,
    /// The two bits after PES_packet_length are not '10'.
    Syntax,
    Length,
}

/// Stream ids without the optional header, H.222.0 Table 2-21.
#[inline]
fn is_headerless(stream_id: u8) -> bool {
    matches!(stream_id, 0xBC | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF)
}

const STREAM_ID_PADDING: u8 = 0xBE;

#[inline]
fn timestamp(b: &[u8], prefix: u8) -> Option<u64> {
    let b: &[u8; 5] = b.get(..5)?.try_into().ok()?;
    if b[0] >> 4 != prefix || b[0] & b[2] & b[4] & 0x01 == 0 {
        return None;
    }
    Some(
        (u64::from(b[0] & 0x0E) << 29)
            | (u64::from(b[1]) << 22)
            | (u64::from(b[2] & 0xFE) << 14)
            | (u64::from(b[3]) << 7)
            | u64::from(b[4] >> 1),
    )
}

/// `Ok(None)` needs more bytes, which never happens once [`PES_HEADER_MAX`] are present.
pub fn parse_pes_header(data: &[u8]) -> Result<Option<PesHeader>, PesError> {
    let Some(start) = data.get(..6) else {
        let prefix = [0u8, 0, 1];
        let n = data.len().min(3);
        return if data[..n] == prefix[..n] { Ok(None) } else { Err(PesError::StartCode) };
    };
    if start[..3] != [0, 0, 1] {
        return Err(PesError::StartCode);
    }
    let stream_id = start[3];
    let packet_length = u16::from_be_bytes([start[4], start[5]]);
    if stream_id == STREAM_ID_PADDING || is_headerless(stream_id) {
        return Ok(Some(PesHeader {
            pts: None,
            dts: None,
            payload_len: Some(u32::from(packet_length)),
            header_len: 6,
            stream_id,
            flags: if stream_id == STREAM_ID_PADDING { PH_PADDING } else { 0 },
        }));
    }
    let Some(fixed) = data.get(6..9) else { return Ok(None) };
    if fixed[0] & 0xC0 != 0x80 {
        return Err(PesError::Syntax);
    }
    let data_len = usize::from(fixed[2]);
    let header_len = 9 + data_len;
    let Some(optional) = data.get(9..header_len) else { return Ok(None) };

    let mut flags = 0u8;
    if fixed[0] & 0x30 != 0 {
        flags |= PH_SCRAMBLED;
    }
    if fixed[0] & 0x04 != 0 {
        flags |= PH_DATA_ALIGNED;
    }
    let mut pts = None;
    let mut dts = None;
    // PTS_DTS_flags '01' is forbidden (2.4.3.7).
    match fixed[1] >> 6 {
        0b10 => {
            pts = timestamp(optional, 0b0010);
            if pts.is_none() {
                flags |= PH_BAD_TIMESTAMP;
            }
        }
        0b11 => {
            pts = timestamp(optional, 0b0011);
            dts = optional.get(5..).and_then(|b| timestamp(b, 0b0001));
            if pts.is_none() || dts.is_none() {
                pts = None;
                dts = None;
                flags |= PH_BAD_TIMESTAMP;
            }
        }
        0b01 => flags |= PH_BAD_TIMESTAMP,
        _ => {}
    }
    let payload_len = if packet_length == 0 {
        None
    } else {
        let after_length = 3 + data_len;
        let n = usize::from(packet_length).checked_sub(after_length).ok_or(PesError::Length)?;
        Some(n as u32)
    };
    Ok(Some(PesHeader { pts, dts, payload_len, header_len: header_len as u16, stream_id, flags }))
}

/// MPEG-1 packet header, ISO/IEC 11172-1 2.4.3.3. `Ok(None)` needs more bytes, which never happens
/// once `6 + packet_length` are present. Ids without the header parse as [`parse_pes_header`] does.
pub fn parse_mpeg1_pes_header(data: &[u8]) -> Result<Option<PesHeader>, PesError> {
    let Some(start) = data.get(..6) else { return parse_pes_header(data) };
    let (stream_id, packet_length) = (start[3], u16::from_be_bytes([start[4], start[5]]));
    if start[..3] != [0, 0, 1] || stream_id == STREAM_ID_PADDING || is_headerless(stream_id) {
        return parse_pes_header(data);
    }
    let end = 6 + usize::from(packet_length);
    let byte = |at: usize| match (at < end, data.get(at)) {
        (false, _) => Err(PesError::Length),
        (true, b) => Ok(b.copied()),
    };
    let mut at = 6;
    let mut b = loop {
        match byte(at)? {
            Some(0xFF) => at += 1,
            Some(b) => break b,
            None => return Ok(None),
        }
    };
    // STD_buffer_scale and STD_buffer_size.
    if b & 0xC0 == 0x40 {
        at += 2;
        let Some(next) = byte(at)? else { return Ok(None) };
        b = next;
    }
    let (stamps, header_len) = match b >> 4 {
        0b0010 => (1, at + 5),
        0b0011 => (2, at + 10),
        _ if b == 0x0F => (0, at + 1),
        _ => return Err(PesError::Syntax),
    };
    if header_len > end {
        return Err(PesError::Length);
    }
    let Some(fields) = data.get(at..header_len) else { return Ok(None) };
    let (mut pts, mut dts, mut flags) = (None, None, 0u8);
    if stamps > 0 {
        pts = timestamp(fields, if stamps == 1 { 0b0010 } else { 0b0011 });
        if stamps == 2 {
            dts = fields.get(5..).and_then(|b| timestamp(b, 0b0001));
        }
        if pts.is_none() || (stamps == 2 && dts.is_none()) {
            (pts, dts, flags) = (None, None, PH_BAD_TIMESTAMP);
        }
    }
    let payload_len = Some((end - header_len) as u32);
    Ok(Some(PesHeader { pts, dts, payload_len, header_len: header_len as u16, stream_id, flags }))
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn stamp(prefix: u8, v: u64) -> [u8; 5] {
        [
            (prefix << 4) | (((v >> 30) as u8 & 0x07) << 1) | 1,
            (v >> 22) as u8,
            (((v >> 15) as u8) << 1) | 1,
            (v >> 7) as u8,
            ((v as u8) << 1) | 1,
        ]
    }

    #[test]
    fn pts_only_bounded() {
        let mut h = vec![0, 0, 1, 0xC0, 0x00, 0x0C, 0x84, 0x80, 0x05];
        h.extend_from_slice(&stamp(0b0010, 0x1_FFFF_FFFF));
        h.extend_from_slice(&[1, 2, 3, 4]);
        let p = parse_pes_header(&h).unwrap().unwrap();
        assert_eq!(p.pts, Some(0x1_FFFF_FFFF));
        assert_eq!(p.dts, None);
        assert_eq!(p.header_len, 14);
        assert_eq!(p.payload_len, Some(4));
        assert_eq!(p.stream_id, 0xC0);
        assert_eq!(p.flags, PH_DATA_ALIGNED);
    }

    #[test]
    fn pts_and_dts_unbounded() {
        let mut h = vec![0, 0, 1, 0xE0, 0x00, 0x00, 0x80, 0xC0, 0x0A];
        h.extend_from_slice(&stamp(0b0011, 0x1_2345_6789));
        h.extend_from_slice(&stamp(0b0001, 0x0_FEDC_BA98));
        let p = parse_pes_header(&h).unwrap().unwrap();
        assert_eq!(p.pts, Some(0x1_2345_6789));
        assert_eq!(p.dts, Some(0x0_FEDC_BA98));
        assert_eq!(p.payload_len, None);
        assert_eq!(p.header_len, 19);
        assert_eq!(p.flags, 0);
    }

    #[test]
    fn every_timestamp_bit_round_trips() {
        for bit in 0..33 {
            let v = 1u64 << bit;
            assert_eq!(timestamp(&stamp(0b0010, v), 0b0010), Some(v));
        }
        assert_eq!(timestamp(&stamp(0b0010, 0), 0b0010), Some(0));
    }

    #[test]
    fn needs_more_bytes() {
        let mut h = vec![0, 0, 1, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05];
        h.extend_from_slice(&stamp(0b0010, 90_000));
        for cut in 0..h.len() {
            assert_eq!(parse_pes_header(&h[..cut]), Ok(None), "cut at {cut}");
        }
        assert!(parse_pes_header(&h).unwrap().is_some());
    }

    #[test]
    fn rejects() {
        assert_eq!(parse_pes_header(&[0, 0, 2, 0xE0, 0, 0, 0x80, 0, 0]), Err(PesError::StartCode));
        assert_eq!(parse_pes_header(&[0, 1]), Err(PesError::StartCode));
        assert_eq!(parse_pes_header(&[0x47]), Err(PesError::StartCode));
        assert_eq!(parse_pes_header(&[0, 0, 1, 0xE0, 0, 0, 0x40, 0, 0]), Err(PesError::Syntax));
        assert_eq!(parse_pes_header(&[0, 0, 1, 0xC0, 0, 2, 0x80, 0, 0]), Err(PesError::Length));
    }

    #[test]
    fn bad_timestamps_are_flagged() {
        let mut h = vec![0, 0, 1, 0xC0, 0x00, 0x00, 0x80, 0x80, 0x05];
        let mut s = stamp(0b0010, 1234);
        s[4] &= 0xFE;
        h.extend_from_slice(&s);
        let p = parse_pes_header(&h).unwrap().unwrap();
        assert_eq!((p.pts, p.flags), (None, PH_BAD_TIMESTAMP));
        let mut h = vec![0, 0, 1, 0xC0, 0x00, 0x00, 0x80, 0x80, 0x05];
        h.extend_from_slice(&stamp(0b0011, 1234));
        assert_eq!(parse_pes_header(&h).unwrap().unwrap().flags, PH_BAD_TIMESTAMP);
        let h = [0, 0, 1, 0xC0, 0x00, 0x00, 0x80, 0x80, 0x00];
        assert_eq!(parse_pes_header(&h).unwrap().unwrap().flags, PH_BAD_TIMESTAMP);
        let h = [0, 0, 1, 0xC0, 0x00, 0x00, 0x80, 0x40, 0x00];
        assert_eq!(parse_pes_header(&h).unwrap().unwrap().flags, PH_BAD_TIMESTAMP);
        let mut h = vec![0, 0, 1, 0xE0, 0x00, 0x00, 0x80, 0xC0, 0x0A];
        h.extend_from_slice(&stamp(0b0011, 1234));
        h.extend_from_slice(&stamp(0b0011, 1000));
        let p = parse_pes_header(&h).unwrap().unwrap();
        assert_eq!((p.pts, p.dts, p.flags), (None, None, PH_BAD_TIMESTAMP));
    }

    #[test]
    fn headerless_and_padding() {
        let p = parse_pes_header(&[0, 0, 1, 0xBF, 0x00, 0x10, 9, 9]).unwrap().unwrap();
        assert_eq!((p.header_len, p.payload_len, p.flags), (6, Some(16), 0));
        let p = parse_pes_header(&[0, 0, 1, 0xBE, 0x00, 0x04]).unwrap().unwrap();
        assert_eq!((p.header_len, p.payload_len, p.flags), (6, Some(4), PH_PADDING));
    }

    #[test]
    fn scrambling_is_reported() {
        let h = [0, 0, 1, 0xE0, 0x00, 0x00, 0x90, 0x00, 0x00];
        assert_eq!(parse_pes_header(&h).unwrap().unwrap().flags, PH_SCRAMBLED);
    }

    fn mpeg1(id: u8, fields: &[u8], payload: usize) -> Vec<u8> {
        let len = (fields.len() + payload) as u16;
        let mut h = vec![0, 0, 1, id, (len >> 8) as u8, len as u8];
        h.extend_from_slice(fields);
        h.resize(6 + usize::from(len), 0xAA);
        h
    }

    #[test]
    fn mpeg1_headers_with_stuffing_buffer_and_stamps() {
        let mut f = vec![0xFF, 0xFF, 0x60, 0xE8];
        f.extend_from_slice(&stamp(0b0010, 0x1_0000_0001));
        let p = parse_mpeg1_pes_header(&mpeg1(0xC0, &f, 3)).unwrap().unwrap();
        assert_eq!(
            (p.pts, p.dts, p.header_len, p.payload_len),
            (Some(0x1_0000_0001), None, 15, Some(3))
        );
        let mut f = stamp(0b0011, 9000).to_vec();
        f.extend_from_slice(&stamp(0b0001, 6000));
        let p = parse_mpeg1_pes_header(&mpeg1(0xE0, &f, 1)).unwrap().unwrap();
        assert_eq!(
            (p.pts, p.dts, p.header_len, p.payload_len),
            (Some(9000), Some(6000), 16, Some(1))
        );
        let p = parse_mpeg1_pes_header(&mpeg1(0xE0, &[0x0F], 4)).unwrap().unwrap();
        assert_eq!((p.pts, p.header_len, p.payload_len, p.flags), (None, 7, Some(4), 0));
        let p = parse_mpeg1_pes_header(&mpeg1(0xBF, &[], 4)).unwrap().unwrap();
        assert_eq!((p.header_len, p.payload_len), (6, Some(4)));
    }

    #[test]
    fn mpeg1_headers_cut_short_bad_or_overlong() {
        let mut f = vec![0xFF, 0x40, 0x10];
        f.extend_from_slice(&stamp(0b0010, 77));
        let whole = mpeg1(0xC0, &f, 2);
        for cut in 0..14 {
            assert_eq!(parse_mpeg1_pes_header(&whole[..cut]), Ok(None), "cut at {cut}");
        }
        assert_eq!(parse_mpeg1_pes_header(&whole).unwrap().unwrap().pts, Some(77));
        assert_eq!(
            parse_mpeg1_pes_header(&mpeg1(0xC0, &[0x50, 0, 0x8F], 0)),
            Err(PesError::Syntax)
        );
        assert_eq!(parse_mpeg1_pes_header(&mpeg1(0xC0, &[0xFF, 0xFF], 0)), Err(PesError::Length));
        let mut short = mpeg1(0xC0, &[0x21, 0, 1], 0);
        short.extend_from_slice(&[0, 1, 1]);
        assert_eq!(parse_mpeg1_pes_header(&short), Err(PesError::Length));
        let mut bad = stamp(0b0010, 5);
        bad[2] &= 0xFE;
        let p = parse_mpeg1_pes_header(&mpeg1(0xC0, &bad, 0)).unwrap().unwrap();
        assert_eq!((p.pts, p.flags), (None, PH_BAD_TIMESTAMP));
    }
}
