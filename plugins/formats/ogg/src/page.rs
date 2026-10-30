//! The Ogg page model, parse and serialise a single page (RFC 3533 section 6).

/// The capture pattern that begins a page (RFC 3533 section 6, field 1).
pub const CAPTURE_PATTERN: [u8; 4] = *b"OggS";

/// stream_structure_version 0 (section 6, field 2).
pub const STREAM_STRUCTURE_VERSION: u8 = 0;

/// Fixed header length before the segment table (section 6).
pub const HEADER_FIXED_LEN: usize = 27;

/// At most 255 segments of 255 bytes, so a page is at most 65307 bytes (section 6).
pub const MAX_SEGMENTS: usize = 255;

/// Largest possible payload of a single page (255 segments x 255 bytes).
pub const MAX_PAGE_PAYLOAD: usize = MAX_SEGMENTS * 255;

/// Largest possible on-the-wire page.
pub const MAX_PAGE_SIZE: usize = HEADER_FIXED_LEN + MAX_SEGMENTS + MAX_PAGE_PAYLOAD;

/// header_type flag bits (section 6, field 3).
pub mod flags {
    /// Continues a packet from the previous page.
    pub const CONTINUED: u8 = 0x01;
    /// First page of a logical bitstream.
    pub const BOS: u8 = 0x02;
    /// Last page of a logical bitstream.
    pub const EOS: u8 = 0x04;
}

/// The -1 granule_position, no packet finishes on this page (section 6, field 4).
pub const GRANULE_NONE: u64 = u64::MAX;

/// A parsed, integrity-checked view over one page borrowed from the input stream.
#[derive(Clone, Copy, Debug)]
pub struct PageHeader<'a> {
    bytes: &'a [u8],
    n_segments: usize,
}

/// Why a candidate page failed to parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageError {
    NeedMoreHeader,
    BadCapturePattern,
    BadVersion(u8),
    NeedMoreBody,
    BadCrc { stored: u32, computed: u32 },
}

impl<'a> PageHeader<'a> {
    /// Try to parse a page starting at `data[0]`.
    pub fn parse(data: &'a [u8]) -> Result<PageHeader<'a>, PageError> {
        if data.len() < HEADER_FIXED_LEN {
            return Err(PageError::NeedMoreHeader);
        }
        if data[0..4] != CAPTURE_PATTERN {
            return Err(PageError::BadCapturePattern);
        }
        let version = data[4];
        if version != STREAM_STRUCTURE_VERSION {
            return Err(PageError::BadVersion(version));
        }
        let n_segments = data[26] as usize;
        let header_len = HEADER_FIXED_LEN + n_segments;
        if data.len() < header_len {
            return Err(PageError::NeedMoreHeader);
        }
        let segment_table = &data[HEADER_FIXED_LEN..header_len];
        let payload_len: usize = segment_table.iter().map(|&b| b as usize).sum();
        let page_len = header_len + payload_len;
        if data.len() < page_len {
            return Err(PageError::NeedMoreBody);
        }
        let bytes = &data[..page_len];

        let computed = page_crc(bytes);
        let stored = u32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]);
        if stored != computed {
            return Err(PageError::BadCrc { stored, computed });
        }

        Ok(PageHeader { bytes, n_segments })
    }

    /// The full on-the-wire size of this page in bytes (header + table + payload).
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// header_type flags byte (section 6, field 3).
    pub fn header_type(&self) -> u8 {
        self.bytes[5]
    }

    /// True if this page continues a packet from the previous page (section 6, flag 0x01).
    pub fn is_continued(&self) -> bool {
        self.header_type() & flags::CONTINUED != 0
    }

    /// True if this is the first page of a logical bitstream (section 6, flag 0x02).
    pub fn is_bos(&self) -> bool {
        self.header_type() & flags::BOS != 0
    }

    /// True if this is the last page of a logical bitstream (section 6, flag 0x04).
    pub fn is_eos(&self) -> bool {
        self.header_type() & flags::EOS != 0
    }

    /// granule_position (section 6, field 4), little-endian.
    pub fn granule_position(&self) -> u64 {
        u64::from_le_bytes([
            self.bytes[6],
            self.bytes[7],
            self.bytes[8],
            self.bytes[9],
            self.bytes[10],
            self.bytes[11],
            self.bytes[12],
            self.bytes[13],
        ])
    }

    /// bitstream_serial_number (section 6, field 5), little-endian.
    pub fn serial(&self) -> u32 {
        u32::from_le_bytes([self.bytes[14], self.bytes[15], self.bytes[16], self.bytes[17]])
    }

    /// page_sequence_number (section 6, field 6), little-endian.
    pub fn sequence(&self) -> u32 {
        u32::from_le_bytes([self.bytes[18], self.bytes[19], self.bytes[20], self.bytes[21]])
    }

    /// The stored CRC field (section 6, field 7), little-endian.
    pub fn crc(&self) -> u32 {
        u32::from_le_bytes([self.bytes[22], self.bytes[23], self.bytes[24], self.bytes[25]])
    }

    /// Number of lacing values in the segment table (section 6, field 8).
    pub fn n_segments(&self) -> usize {
        self.n_segments
    }

    /// The lacing values (section 6, field 9), one byte per segment.
    pub fn segment_table(&self) -> &'a [u8] {
        &self.bytes[HEADER_FIXED_LEN..HEADER_FIXED_LEN + self.n_segments]
    }

    /// The concatenated segment data (the page payload), after the header.
    pub fn payload(&self) -> &'a [u8] {
        &self.bytes[HEADER_FIXED_LEN + self.n_segments..]
    }

    /// The raw page bytes (for cross-validation / passthrough).
    pub fn raw(&self) -> &'a [u8] {
        self.bytes
    }
}

/// The page CRC over `page` with the CRC field at offset 22 taken as zero (section 6, field 7).
pub fn page_crc(page: &[u8]) -> u32 {
    debug_assert!(page.len() >= HEADER_FIXED_LEN);
    let mut crc = crate::crc::Crc32::new();
    crc.update(&page[..22]);
    crc.update(&[0, 0, 0, 0]);
    crc.update(&page[26..]);
    crc.value()
}

/// Serialise one page into `out`, computing and stamping its CRC (section 6).
#[allow(clippy::too_many_arguments)]
pub fn write_page(
    out: &mut Vec<u8>,
    header_type: u8,
    granule_position: u64,
    serial: u32,
    sequence: u32,
    segment_table: &[u8],
    payload: &[u8],
) {
    debug_assert!(segment_table.len() <= MAX_SEGMENTS);
    debug_assert_eq!(
        payload.len(),
        segment_table.iter().map(|&b| b as usize).sum::<usize>(),
        "payload length must equal the sum of lacing values"
    );

    let start = out.len();
    out.extend_from_slice(&CAPTURE_PATTERN);
    out.push(STREAM_STRUCTURE_VERSION);
    out.push(header_type);
    out.extend_from_slice(&granule_position.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    let crc_at = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.push(segment_table.len() as u8);
    out.extend_from_slice(segment_table);
    out.extend_from_slice(payload);

    let crc = page_crc(&out[start..]);
    out[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn write_then_parse_roundtrips_all_fields() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut out = Vec::new();
        write_page(&mut out, flags::BOS, 0x0102_0304_0506_0708, 0xAABB_CCDD, 42, &[4], &payload);

        let page = PageHeader::parse(&out).expect("valid page");
        assert_eq!(page.len(), out.len());
        assert!(page.is_bos());
        assert!(!page.is_eos());
        assert!(!page.is_continued());
        assert_eq!(page.granule_position(), 0x0102_0304_0506_0708);
        assert_eq!(page.serial(), 0xAABB_CCDD);
        assert_eq!(page.sequence(), 42);
        assert_eq!(page.n_segments(), 1);
        assert_eq!(page.segment_table(), &[4]);
        assert_eq!(page.payload(), &payload);
        assert_eq!(page.crc(), page_crc(&out));
    }

    #[test]
    fn capture_pattern_is_oggs() {
        assert_eq!(&CAPTURE_PATTERN, b"OggS");
        assert_eq!(CAPTURE_PATTERN, [0x4f, 0x67, 0x67, 0x53]);
    }

    #[test]
    fn rejects_bad_capture_pattern() {
        let mut out = Vec::new();
        write_page(&mut out, 0, 0, 1, 0, &[0], &[]);
        out[1] = b'X'; // corrupt "OggS" to "OXgS"
        assert!(matches!(PageHeader::parse(&out), Err(PageError::BadCapturePattern)));
    }

    #[test]
    fn rejects_bad_version() {
        let mut out = Vec::new();
        write_page(&mut out, 0, 0, 1, 0, &[0], &[]);
        out[4] = 1; // version is checked before the CRC, so still BadVersion
        assert!(matches!(PageHeader::parse(&out), Err(PageError::BadVersion(1))));
    }

    #[test]
    fn detects_corrupt_payload_via_crc() {
        let mut out = Vec::new();
        write_page(&mut out, 0, 7, 1, 0, &[3], &[1, 2, 3]);
        let last = out.len() - 1;
        out[last] ^= 0xFF; // flip a payload byte
        match PageHeader::parse(&out) {
            Err(PageError::BadCrc { .. }) => {}
            other => panic!("expected BadCrc, got {other:?}"),
        }
    }

    #[test]
    fn truncated_header_and_body_report_need_more() {
        let mut out = Vec::new();
        write_page(&mut out, 0, 0, 1, 0, &[5], &[1, 2, 3, 4, 5]);
        assert!(matches!(PageHeader::parse(&out[..10]), Err(PageError::NeedMoreHeader)));
        let short = out.len() - 2;
        assert!(matches!(PageHeader::parse(&out[..short]), Err(PageError::NeedMoreBody)));
    }

    #[test]
    fn zero_length_packet_page() {
        let mut out = Vec::new();
        write_page(&mut out, 0, 0, 1, 0, &[0], &[]);
        let page = PageHeader::parse(&out).unwrap();
        assert_eq!(page.n_segments(), 1);
        assert_eq!(page.segment_table(), &[0]);
        assert!(page.payload().is_empty());
    }

    #[test]
    fn granule_none_sentinel() {
        let mut out = Vec::new();
        write_page(&mut out, 0, GRANULE_NONE, 1, 0, &[0], &[]);
        let page = PageHeader::parse(&out).unwrap();
        assert_eq!(page.granule_position(), GRANULE_NONE);
    }

    #[test]
    fn real_libogg_vorbis_bos_page_crc_and_fields() {
        const PAGE: [u8; 58] = [
            0x4f, 0x67, 0x67, 0x53, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x27, 0x70, 0xb3, 0x23, 0x00, 0x00, 0x00, 0x00, 0xce, 0x7f, 0x3c, 0x45, 0x01, 0x1e,
            0x01, 0x76, 0x6f, 0x72, 0x62, 0x69, 0x73, 0x00, 0x00, 0x00, 0x00, 0x02, 0x44, 0xac,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xb5, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xb8, 0x01,
        ];
        let page = PageHeader::parse(&PAGE).expect("real libogg page must parse");
        assert_eq!(page.len(), 58);
        assert!(page.is_bos());
        assert!(!page.is_eos());
        assert!(!page.is_continued());
        assert_eq!(page.serial(), 0x23b3_7027);
        assert_eq!(page.sequence(), 0);
        assert_eq!(page.granule_position(), 0); // no audio samples on the id-header page
        assert_eq!(page.n_segments(), 1);
        assert_eq!(page.segment_table(), &[30]);
        assert_eq!(&page.payload()[..7], &[0x01, b'v', b'o', b'r', b'b', b'i', b's']);
        assert_eq!(page.crc(), 0x453c_7fce);
        assert_eq!(page_crc(&PAGE), 0x453c_7fce);
    }
}
