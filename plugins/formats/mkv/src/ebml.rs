//! Hand-written EBML primitives (RFC 8794). Multi-byte scalars are big-endian (section 7.1).

/// Matroska Element IDs pre-encoded with their VINT length bits (RFC 8794 section 5).
pub mod id {
    pub const EBML: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
    pub const EBML_VERSION: &[u8] = &[0x42, 0x86];
    pub const EBML_READ_VERSION: &[u8] = &[0x42, 0xF7];
    pub const EBML_MAX_ID_LENGTH: &[u8] = &[0x42, 0xF2];
    pub const EBML_MAX_SIZE_LENGTH: &[u8] = &[0x42, 0xF3];
    pub const DOC_TYPE: &[u8] = &[0x42, 0x82];
    pub const DOC_TYPE_VERSION: &[u8] = &[0x42, 0x87];
    pub const DOC_TYPE_READ_VERSION: &[u8] = &[0x42, 0x85];

    pub const SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
    pub const INFO: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
    pub const TIMESTAMP_SCALE: &[u8] = &[0x2A, 0xD7, 0xB1];
    pub const DURATION: &[u8] = &[0x44, 0x89];
    pub const MUXING_APP: &[u8] = &[0x4D, 0x80];
    pub const WRITING_APP: &[u8] = &[0x57, 0x41];

    pub const TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
    pub const TRACK_ENTRY: &[u8] = &[0xAE];
    pub const TRACK_NUMBER: &[u8] = &[0xD7];
    pub const TRACK_UID: &[u8] = &[0x73, 0xC5];
    pub const TRACK_TYPE: &[u8] = &[0x83];
    pub const CODEC_ID: &[u8] = &[0x86];
    pub const CODEC_PRIVATE: &[u8] = &[0x63, 0xA2];
    pub const CODEC_DELAY: &[u8] = &[0x56, 0xAA];
    pub const SEEK_PRE_ROLL: &[u8] = &[0x56, 0xBB];
    pub const FLAG_LACING: &[u8] = &[0x9C];
    pub const FLAG_DEFAULT: &[u8] = &[0x88];
    pub const FLAG_FORCED: &[u8] = &[0x55, 0xAA];
    pub const FLAG_ENABLED: &[u8] = &[0xB9];
    pub const FLAG_HEARING_IMPAIRED: &[u8] = &[0x55, 0xAB];
    pub const FLAG_VISUAL_IMPAIRED: &[u8] = &[0x55, 0xAC];
    pub const FLAG_TEXT_DESCRIPTIONS: &[u8] = &[0x55, 0xAD];
    pub const FLAG_ORIGINAL: &[u8] = &[0x55, 0xAE];
    pub const FLAG_COMMENTARY: &[u8] = &[0x55, 0xAF];
    pub const NAME: &[u8] = &[0x53, 0x6E];
    pub const LANGUAGE: &[u8] = &[0x22, 0xB5, 0x9C];
    pub const LANGUAGE_BCP47: &[u8] = &[0x22, 0xB5, 0x9D];
    pub const DEFAULT_DURATION: &[u8] = &[0x23, 0xE3, 0x83];
    pub const CONTENT_ENCODINGS: &[u8] = &[0x6D, 0x80];
    pub const CONTENT_ENCODING: &[u8] = &[0x62, 0x40];
    pub const CONTENT_ENCODING_ORDER: &[u8] = &[0x50, 0x31];
    pub const CONTENT_ENCODING_SCOPE: &[u8] = &[0x50, 0x32];
    pub const CONTENT_ENCODING_TYPE: &[u8] = &[0x50, 0x33];
    pub const CONTENT_COMPRESSION: &[u8] = &[0x50, 0x34];
    pub const CONTENT_COMP_ALGO: &[u8] = &[0x42, 0x54];
    pub const CONTENT_COMP_SETTINGS: &[u8] = &[0x42, 0x55];
    pub const CONTENT_ENCRYPTION: &[u8] = &[0x50, 0x35];
    pub const AUDIO: &[u8] = &[0xE1];
    pub const SAMPLING_FREQUENCY: &[u8] = &[0xB5];
    pub const CHANNELS: &[u8] = &[0x9F];
    pub const BIT_DEPTH: &[u8] = &[0x62, 0x64];

    pub const VIDEO: &[u8] = &[0xE0];
    pub const PIXEL_WIDTH: &[u8] = &[0xB0];
    pub const PIXEL_HEIGHT: &[u8] = &[0xBA];
    pub const DISPLAY_WIDTH: &[u8] = &[0x54, 0xB0];
    pub const DISPLAY_HEIGHT: &[u8] = &[0x54, 0xBA];
    pub const DISPLAY_UNIT: &[u8] = &[0x54, 0xB2];
    pub const PROJECTION: &[u8] = &[0x76, 0x70];
    pub const PROJECTION_TYPE: &[u8] = &[0x76, 0x71];
    pub const PROJECTION_POSE_YAW: &[u8] = &[0x76, 0x73];
    pub const PROJECTION_POSE_PITCH: &[u8] = &[0x76, 0x74];
    pub const PROJECTION_POSE_ROLL: &[u8] = &[0x76, 0x75];

    pub const COLOUR: &[u8] = &[0x55, 0xB0];
    pub const MATRIX_COEFFICIENTS: &[u8] = &[0x55, 0xB1];
    pub const COLOUR_RANGE: &[u8] = &[0x55, 0xB9];
    pub const TRANSFER_CHARACTERISTICS: &[u8] = &[0x55, 0xBA];
    pub const PRIMARIES: &[u8] = &[0x55, 0xBB];
    pub const MAX_CLL: &[u8] = &[0x55, 0xBC];
    pub const MAX_FALL: &[u8] = &[0x55, 0xBD];
    pub const MASTERING_METADATA: &[u8] = &[0x55, 0xD0];
    pub const PRIMARY_R_X: &[u8] = &[0x55, 0xD1];
    pub const PRIMARY_R_Y: &[u8] = &[0x55, 0xD2];
    pub const PRIMARY_G_X: &[u8] = &[0x55, 0xD3];
    pub const PRIMARY_G_Y: &[u8] = &[0x55, 0xD4];
    pub const PRIMARY_B_X: &[u8] = &[0x55, 0xD5];
    pub const PRIMARY_B_Y: &[u8] = &[0x55, 0xD6];
    pub const WHITE_POINT_X: &[u8] = &[0x55, 0xD7];
    pub const WHITE_POINT_Y: &[u8] = &[0x55, 0xD8];
    pub const LUMINANCE_MAX: &[u8] = &[0x55, 0xD9];
    pub const LUMINANCE_MIN: &[u8] = &[0x55, 0xDA];
    pub const CHROMA_SITING_HORZ: &[u8] = &[0x55, 0xB7];
    pub const CHROMA_SITING_VERT: &[u8] = &[0x55, 0xB8];

    pub const BLOCK_ADDITION_MAPPING: &[u8] = &[0x41, 0xE4];
    pub const BLOCK_ADD_ID_VALUE: &[u8] = &[0x41, 0xF0];
    pub const BLOCK_ADD_ID_NAME: &[u8] = &[0x41, 0xA4];
    pub const BLOCK_ADD_ID_TYPE: &[u8] = &[0x41, 0xE7];
    pub const BLOCK_ADD_ID_EXTRA_DATA: &[u8] = &[0x41, 0xED];
    pub const BLOCK_ADDITIONS: &[u8] = &[0x75, 0xA1];
    pub const DISCARD_PADDING: &[u8] = &[0x75, 0xA2];
    pub const BLOCK_MORE: &[u8] = &[0xA6];
    pub const BLOCK_ADD_ID: &[u8] = &[0xEE];
    pub const BLOCK_ADDITIONAL: &[u8] = &[0xA5];

    pub const CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
    pub const TIMESTAMP: &[u8] = &[0xE7];
    pub const SIMPLE_BLOCK: &[u8] = &[0xA3];

    pub const BLOCK_GROUP: &[u8] = &[0xA0];
    pub const BLOCK: &[u8] = &[0xA1];
    pub const BLOCK_DURATION: &[u8] = &[0x9B];
    pub const REFERENCE_BLOCK: &[u8] = &[0xFB];

    pub const SEEK_HEAD: &[u8] = &[0x11, 0x4D, 0x9B, 0x74];
    pub const SEEK: &[u8] = &[0x4D, 0xBB];
    pub const SEEK_ID: &[u8] = &[0x53, 0xAB];
    pub const SEEK_POSITION: &[u8] = &[0x53, 0xAC];

    pub const CUES: &[u8] = &[0x1C, 0x53, 0xBB, 0x6B];
    pub const CUE_POINT: &[u8] = &[0xBB];
    pub const CUE_TIME: &[u8] = &[0xB3];
    pub const CUE_TRACK_POSITIONS: &[u8] = &[0xB7];
    pub const CUE_TRACK: &[u8] = &[0xF7];
    pub const CUE_CLUSTER_POSITION: &[u8] = &[0xF1];
    pub const CUE_RELATIVE_POSITION: &[u8] = &[0xF0];
    pub const CUE_DURATION: &[u8] = &[0xB2];

    pub const VOID: &[u8] = &[0xEC];

    pub const TAGS: &[u8] = &[0x12, 0x54, 0xC3, 0x67];
    pub const TAG: &[u8] = &[0x73, 0x73];
    pub const TARGETS: &[u8] = &[0x63, 0xC0];
    /// 70 COLLECTION down to 10 SHOT, default 50 (RFC 9559 section 5.1.8.1.1.1).
    pub const TARGET_TYPE_VALUE: &[u8] = &[0x68, 0xCA];
    pub const TARGET_TYPE: &[u8] = &[0x63, 0xCA];
    pub const TAG_TRACK_UID: &[u8] = &[0x63, 0xC5];
    pub const TAG_EDITION_UID: &[u8] = &[0x63, 0xC9];
    pub const TAG_CHAPTER_UID: &[u8] = &[0x63, 0xC4];
    pub const TAG_ATTACHMENT_UID: &[u8] = &[0x63, 0xC6];
    pub const SIMPLE_TAG: &[u8] = &[0x67, 0xC8];
    pub const TAG_NAME: &[u8] = &[0x45, 0xA3];
    pub const TAG_LANGUAGE: &[u8] = &[0x44, 0x7A];
    pub const TAG_LANGUAGE_BCP47: &[u8] = &[0x44, 0x7B];
    pub const TAG_DEFAULT: &[u8] = &[0x44, 0x84];
    pub const TAG_STRING: &[u8] = &[0x44, 0x87];
    pub const TAG_BINARY: &[u8] = &[0x44, 0x85];

    pub const ATTACHMENTS: &[u8] = &[0x19, 0x41, 0xA4, 0x69];
    pub const ATTACHED_FILE: &[u8] = &[0x61, 0xA7];
    pub const FILE_DESCRIPTION: &[u8] = &[0x46, 0x7E];
    pub const FILE_NAME: &[u8] = &[0x46, 0x6E];
    pub const FILE_MEDIA_TYPE: &[u8] = &[0x46, 0x60];
    pub const FILE_DATA: &[u8] = &[0x46, 0x5C];
    pub const FILE_UID: &[u8] = &[0x46, 0xAE];

    // `EditionFlagHidden` and `ChapterFlagEnabled` are from the Matroska v4 schema RFC 9559
    // dropped.
    pub const CHAPTERS: &[u8] = &[0x10, 0x43, 0xA7, 0x70];
    pub const EDITION_ENTRY: &[u8] = &[0x45, 0xB9];
    pub const EDITION_UID: &[u8] = &[0x45, 0xBC];
    pub const EDITION_FLAG_HIDDEN: &[u8] = &[0x45, 0xBD];
    pub const EDITION_FLAG_DEFAULT: &[u8] = &[0x45, 0xDB];
    pub const EDITION_FLAG_ORDERED: &[u8] = &[0x45, 0xDD];
    pub const CHAPTER_ATOM: &[u8] = &[0xB6];
    pub const CHAPTER_UID: &[u8] = &[0x73, 0xC4];
    pub const CHAPTER_STRING_UID: &[u8] = &[0x56, 0x54];
    pub const CHAPTER_TIME_START: &[u8] = &[0x91];
    pub const CHAPTER_TIME_END: &[u8] = &[0x92];
    pub const CHAPTER_FLAG_HIDDEN: &[u8] = &[0x98];
    pub const CHAPTER_FLAG_ENABLED: &[u8] = &[0x45, 0x98];
    pub const CHAPTER_DISPLAY: &[u8] = &[0x80];
    pub const CHAP_STRING: &[u8] = &[0x85];
    pub const CHAP_LANGUAGE: &[u8] = &[0x43, 0x7C];
    pub const CHAP_LANGUAGE_BCP47: &[u8] = &[0x43, 0x7D];
}

/// Fixed-width size, avoiding the `2^(7n)-1` shortest-length edge case (RFC 8794 section 6.3).
pub const BACKPATCH_SIZE_LEN: usize = 8;

/// `2^56 - 1` is the all-ones unknown pattern, so the largest definite length is one less.
pub const MAX_VINT_DATA: u64 = (1u64 << 56) - 2;

#[inline]
pub fn write_id(out: &mut Vec<u8>, element_id: &[u8]) {
    out.extend_from_slice(element_id);
}

/// Smallest VINT width whose all-ones value exceeds `value` (RFC 8794 sections 4, 6.3).
pub fn vint_size_len(value: u64) -> usize {
    debug_assert!(value <= MAX_VINT_DATA, "value too large for a VINT size");
    let mut len = 1usize;
    while len < 8 && value >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    len
}

pub fn write_size(out: &mut Vec<u8>, value: u64) {
    let len = vint_size_len(value);
    write_size_fixed(out, value, len);
}

pub fn write_size_fixed(out: &mut Vec<u8>, value: u64, len: usize) {
    debug_assert!((1..=8).contains(&len));
    debug_assert!(value < (1u64 << (7 * len)) || len == 8, "value overflows a {len}-octet size");
    let marker = 1u64 << (7 * len);
    let v = marker | value;
    for i in (0..len).rev() {
        out.push((v >> (8 * i)) as u8);
    }
}

pub fn reserve_size(out: &mut Vec<u8>) -> usize {
    let at = out.len();
    out.push(0x01); // 8-octet marker, data bits all zero for now
    out.extend_from_slice(&[0u8; 7]);
    at
}

pub fn patch_size(out: &mut [u8], at: usize, data_len: u64) {
    debug_assert!(data_len <= MAX_VINT_DATA, "element too large for an 8-octet size");
    debug_assert_eq!(out[at], 0x01, "patching a slot that is not an 8-octet size marker");
    for i in 0..7 {
        out[at + 1 + i] = (data_len >> (8 * (6 - i))) as u8;
    }
}

#[inline]
pub fn write_unknown_size(out: &mut Vec<u8>) {
    out.push(0xFF);
}

pub fn write_uint(out: &mut Vec<u8>, element_id: &[u8], value: u64) {
    write_id(out, element_id);
    let n = uint_len(value);
    write_size(out, n as u64);
    for i in (0..n).rev() {
        out.push((value >> (8 * i)) as u8);
    }
}

pub fn uint_len(value: u64) -> usize {
    let mut n = 1usize;
    while n < 8 && (value >> (8 * n)) != 0 {
        n += 1;
    }
    n
}

pub fn write_f32(out: &mut Vec<u8>, element_id: &[u8], value: f32) {
    write_id(out, element_id);
    write_size(out, 4);
    out.extend_from_slice(&value.to_bits().to_be_bytes());
}

pub fn write_f64(out: &mut Vec<u8>, element_id: &[u8], value: f64) {
    write_id(out, element_id);
    write_size(out, 8);
    out.extend_from_slice(&value.to_bits().to_be_bytes());
}

pub fn write_string(out: &mut Vec<u8>, element_id: &[u8], value: &str) {
    write_id(out, element_id);
    write_size(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

pub fn write_binary(out: &mut Vec<u8>, element_id: &[u8], value: &[u8]) {
    write_id(out, element_id);
    write_size(out, value.len() as u64);
    out.extend_from_slice(value);
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReadError {
    Incomplete,
    VintTooLong,
    Malformed(&'static str),
}

pub fn read_vint(data: &[u8], at: usize) -> Result<(u64, usize, bool), ReadError> {
    let first = *data.get(at).ok_or(ReadError::Incomplete)?;
    if first == 0x00 {
        return Err(ReadError::VintTooLong); // width would exceed 8 octets (RFC 8794 section 4.4)
    }
    let len = first.leading_zeros() as usize + 1; // marker is the first 1 bit
    if at + len > data.len() {
        return Err(ReadError::Incomplete);
    }
    // `0xFF >> len` in u16 avoids the u8 overflow at len 8.
    let mask = (0xFFu16 >> len) as u64;
    let mut value = (first as u64) & mask;
    let mut all_ones = value == mask;
    for &b in &data[at + 1..at + len] {
        value = (value << 8) | b as u64;
        all_ones &= b == 0xFF;
    }
    Ok((value, len, all_ones))
}

/// An ID includes its length bits, so it is returned verbatim (RFC 8794 section 5).
pub fn read_id(data: &[u8], at: usize) -> Result<(&[u8], usize), ReadError> {
    let (_v, len, _ao) = read_vint(data, at)?;
    Ok((&data[at..at + len], len))
}

pub fn read_size(data: &[u8], at: usize) -> Result<(Option<u64>, usize), ReadError> {
    let (value, len, all_ones) = read_vint(data, at)?;
    Ok((if all_ones { None } else { Some(value) }, len))
}

#[derive(Clone, Copy, Debug)]
pub struct ElementHeader<'a> {
    pub id: &'a [u8],
    pub data_start: usize,
    pub size: Option<u64>,
}

impl ElementHeader<'_> {
    pub fn data_end(&self) -> Result<Option<usize>, ReadError> {
        match self.size {
            None => Ok(None),
            Some(sz) => {
                let end = (self.data_start as u64)
                    .checked_add(sz)
                    .ok_or(ReadError::Malformed("element size overflows the address space"))?;
                Ok(Some(end as usize))
            }
        }
    }
}

pub fn read_element_header(data: &[u8], at: usize) -> Result<ElementHeader<'_>, ReadError> {
    let (id, id_len) = read_id(data, at)?;
    let (size, size_len) = read_size(data, at + id_len)?;
    Ok(ElementHeader { id, data_start: at + id_len + size_len, size })
}

/// The value ends at the first NUL (RFC 8794 section 13), dropping padding other muxers write.
pub fn read_string(data: &[u8]) -> &[u8] {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    &data[..end]
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn vint_size_len_boundaries() {
        assert_eq!(vint_size_len(0), 1);
        assert_eq!(vint_size_len(1), 1);
        assert_eq!(vint_size_len(126), 1);
        assert_eq!(vint_size_len(127), 2);
        assert_eq!(vint_size_len((1 << 14) - 2), 2);
        assert_eq!(vint_size_len((1 << 14) - 1), 3); // 2^14-1 all-ones at width 2
        assert_eq!(vint_size_len((1 << 21) - 1), 4);
        assert_eq!(vint_size_len((1 << 28) - 1), 5);
        assert_eq!(vint_size_len(MAX_VINT_DATA), 8);
    }

    fn decode_size(bytes: &[u8]) -> (u64, usize) {
        let first = bytes[0];
        let len = first.leading_zeros() as usize + 1; // marker is the first 1 bit
        let mut value = (first as u64) & ((0xFFu16 >> len) as u64);
        for &b in &bytes[1..len] {
            value = (value << 8) | b as u64;
        }
        (value, len)
    }

    #[test]
    fn write_size_roundtrip_and_width() {
        let cases: &[(u64, usize)] = &[
            (0, 1),
            (1, 1),
            (126, 1),
            (127, 2),
            (128, 2),
            (0x3FFE, 2),
            (0x3FFF, 3),
            (100_000, 3),
            ((1 << 21) - 2, 3),
            ((1 << 21) - 1, 4),
            (1_000_000, 3),
            (16_000_000, 4),
        ];
        for &(value, want_len) in cases {
            let mut out = Vec::new();
            write_size(&mut out, value);
            assert_eq!(out.len(), want_len, "width for {value}");
            let (got, len) = decode_size(&out);
            assert_eq!(got, value, "value round-trip for {value}");
            assert_eq!(len, want_len);
        }
    }

    #[test]
    fn reserve_then_patch_roundtrips() {
        for &len in &[0u64, 1, 255, 256, 65_535, 1_000_000, MAX_VINT_DATA] {
            let mut out = Vec::new();
            let at = reserve_size(&mut out);
            assert_eq!(out.len(), BACKPATCH_SIZE_LEN);
            assert_eq!(out[at], 0x01, "8-octet marker");
            patch_size(&mut out, at, len);
            let (got, width) = decode_size(&out);
            assert_eq!(width, 8, "back-patch slot stays 8 octets");
            assert_eq!(got, len, "patched length");
        }
    }

    #[test]
    fn unknown_size_is_ff() {
        let mut out = Vec::new();
        write_unknown_size(&mut out);
        assert_eq!(out, vec![0xFF]);
        let (value, len) = decode_size(&out);
        assert_eq!(len, 1);
        assert_eq!(value, (1 << 7) - 1);
    }

    #[test]
    fn uint_len_minimal() {
        assert_eq!(uint_len(0), 1);
        assert_eq!(uint_len(255), 1);
        assert_eq!(uint_len(256), 2);
        assert_eq!(uint_len(65_535), 2);
        assert_eq!(uint_len(65_536), 3);
        assert_eq!(uint_len(u64::MAX), 8);
    }

    #[test]
    fn write_uint_bytes() {
        let mut out = Vec::new();
        write_uint(&mut out, id::TRACK_TYPE, 2); // 0x83, size 1, value 0x02
        assert_eq!(out, vec![0x83, 0x81, 0x02]);

        let mut out = Vec::new();
        write_uint(&mut out, id::CHANNELS, 0); // zero is one 0x00 octet, not empty
        assert_eq!(out, vec![0x9F, 0x81, 0x00]);

        let mut out = Vec::new();
        write_uint(&mut out, id::TIMESTAMP_SCALE, 1_000_000); // 0x0F4240, 3 octets
        assert_eq!(out, vec![0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40]);
    }

    #[test]
    fn write_float_bytes_big_endian() {
        let mut out = Vec::new();
        write_f64(&mut out, id::SAMPLING_FREQUENCY, 48000.0);
        assert_eq!(&out[..1], id::SAMPLING_FREQUENCY);
        assert_eq!(out[1], 0x88); // size = 8
        assert_eq!(&out[2..], &48000.0f64.to_bits().to_be_bytes());

        let mut out = Vec::new();
        write_f32(&mut out, id::SAMPLING_FREQUENCY, 44100.0);
        assert_eq!(out[1], 0x84); // size = 4
        assert_eq!(&out[2..], &44100.0f32.to_bits().to_be_bytes());
    }

    #[test]
    fn write_string_and_binary_bytes() {
        let mut out = Vec::new();
        write_string(&mut out, id::CODEC_ID, "A_FLAC");
        assert_eq!(out, vec![0x86, 0x86, b'A', b'_', b'F', b'L', b'A', b'C']);

        let mut out = Vec::new();
        write_binary(&mut out, id::CODEC_PRIVATE, &[0xDE, 0xAD]);
        assert_eq!(out, vec![0x63, 0xA2, 0x82, 0xDE, 0xAD]);
    }

    #[test]
    fn read_string_ends_at_the_first_null_octet() {
        let mut out = Vec::new();
        write_binary(&mut out, id::CODEC_ID, b"A_AAC\0");
        let h = read_element_header(&out, 0).unwrap();
        assert_eq!(h.size, Some(6));
        assert_eq!(read_string(&out[h.data_start..]), b"A_AAC");
        assert_eq!(read_string(b"matroska\0"), b"matroska");
        assert_eq!(read_string(b"matroska\0\0\0"), b"matroska");
        assert_eq!(read_string(b"A_FLAC"), b"A_FLAC");
        assert_eq!(read_string(b""), b"");
        assert_eq!(read_string(b"\0"), b"");
        assert_eq!(read_string(b"AB\0C"), b"AB");
    }

    #[test]
    fn read_size_inverts_write_size() {
        for &v in &[0u64, 1, 126, 127, 128, 0x3FFF, 100_000, 1_000_000, MAX_VINT_DATA] {
            let mut buf = Vec::new();
            write_size(&mut buf, v);
            let (got, len) = read_size(&buf, 0).expect("read");
            assert_eq!(got, Some(v), "value round-trip for {v}");
            assert_eq!(len, buf.len(), "width round-trip for {v}");
        }
    }

    #[test]
    fn read_unknown_size_and_id() {
        let (size, len) = read_size(&[0xFF], 0).expect("read");
        assert_eq!((size, len), (None, 1), "0xFF is the unknown-size marker");

        let (id, len) = read_id(id::EBML, 0).expect("read id");
        assert_eq!(id, id::EBML, "ID octets returned verbatim");
        assert_eq!(len, 4);
    }

    #[test]
    fn read_vint_incomplete_vs_too_long() {
        assert_eq!(read_vint(&[0x1A, 0x45], 0), Err(ReadError::Incomplete));
        assert_eq!(read_vint(&[], 0), Err(ReadError::Incomplete));
        assert_eq!(read_vint(&[0x00, 0x01], 0), Err(ReadError::VintTooLong));
    }

    #[test]
    fn read_element_header_offsets() {
        let mut buf = Vec::new();
        write_uint(&mut buf, id::TIMESTAMP_SCALE, 1_000_000);
        let h = read_element_header(&buf, 0).expect("header");
        assert_eq!(h.id, id::TIMESTAMP_SCALE);
        assert_eq!(h.size, Some(3), "value 0x0F4240 is 3 octets");
        assert_eq!(h.data_end().unwrap(), Some(buf.len()));

        let mut seg = Vec::new();
        write_id(&mut seg, id::SEGMENT);
        write_unknown_size(&mut seg);
        let h = read_element_header(&seg, 0).expect("header");
        assert_eq!(h.id, id::SEGMENT);
        assert_eq!(h.size, None, "unknown-size streamed master");
        assert_eq!(h.data_end().unwrap(), None);
    }
}
