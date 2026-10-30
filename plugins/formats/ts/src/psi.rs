//! Program specific information (H.222.0 2.4.4) and stream identification, pure functions.

use crate::crc::crc32;
use crate::packet::{PID_FIRST_FREE, PID_NULL};

// Table ids, H.222.0 Table 2-31.
pub const TABLE_ID_PAT: u8 = 0x00;
pub const TABLE_ID_PMT: u8 = 0x02;
// Section size limits, H.222.0 2.4.4.
pub const PSI_SECTION_MAX: usize = 1024;
pub const SECTION_MAX: usize = 4096;
const LONG_HEADER: usize = 8;
const CRC_LEN: usize = 4;

// Descriptor tags, H.222.0 Table 2-45 and EN 300 468 table 12.
pub const DESC_REGISTRATION: u8 = 0x05;
pub const DESC_ISO_639_LANGUAGE: u8 = 0x0A;
/// H.222.0 2.6.28, tag value from memory.
pub const DESC_PRIVATE_DATA_INDICATOR: u8 = 0x0F;
pub const DESC_METADATA: u8 = 0x26;
pub const DESC_DVB_TELETEXT: u8 = 0x56;
pub const DESC_DVB_SUBTITLING: u8 = 0x59;
pub const DESC_DVB_AC3: u8 = 0x6A;
pub const DESC_DVB_EAC3: u8 = 0x7A;
/// H.222.0 Table 2-111, layout as TSDuck `HEVCVideoDescriptor` reads it.
pub const DESC_HEVC_VIDEO: u8 = 0x38;
/// Dolby's TS binding, layout as FFmpeg `libavformat/mpegts.c` reads it.
pub const DESC_DOVI_VIDEO: u8 = 0xB0;
/// Version, profile and flags, dependency PID and compatibility byte.
pub const DOVI_BODY_MAX: usize = 7;

// format_identifier values, A/52:2012 Annex A table A3.1 and Annex G 2, Apple HLS Timed Metadata.
pub const FORMAT_AC3: u32 = 0x4143_2D33;
pub const FORMAT_EAC3: u32 = 0x4541_4333;
pub const FORMAT_ID3: u32 = 0x4944_3320;

// Sample-AES stream_type and indicator values, Apple MPEG-2 Stream Encryption Format for HLS 3.0.
pub const STREAM_TYPE_SAMPLE_AES_H264: u8 = 0xDB;
pub const STREAM_TYPE_SAMPLE_AES_AAC: u8 = 0xCF;
pub const STREAM_TYPE_SAMPLE_AES_AC3: u8 = 0xC1;
pub const STREAM_TYPE_SAMPLE_AES_EAC3: u8 = 0xC2;
pub const INDICATOR_SAMPLE_AES_H264: u32 = 0x7A61_7663;
pub const INDICATOR_SAMPLE_AES_AAC: u32 = 0x6161_6364;
pub const INDICATOR_SAMPLE_AES_AC3: u32 = 0x6163_3364;
pub const INDICATOR_SAMPLE_AES_EAC3: u32 = 0x6563_3364;

pub const SF_DESCRIPTORS_TRUNCATED: u8 = 0x01;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec {
    Unknown,
    Mpeg1Video,
    Mpeg2Video,
    Mpeg4Visual,
    H264,
    Hevc,
    /// ISO/IEC 11172-3 or 13818-3, any layer.
    MpegAudio,
    AacAdts,
    AacLatm,
    Ac3,
    Eac3,
    DvbSubtitle,
    Teletext,
    Id3,
}

/// Encryption of the elementary stream itself, apart from packet scrambling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Encryption {
    None,
    /// HLS Sample-AES, PES in the clear, parts of frames AES-128 CBC. Not decrypted here.
    SampleAes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Media {
    Unknown,
    Video,
    Audio,
    Subtitle,
    Metadata,
}

pub fn media_of(codec: Codec) -> Media {
    match codec {
        Codec::Mpeg1Video | Codec::Mpeg2Video | Codec::Mpeg4Visual | Codec::H264 | Codec::Hevc => {
            Media::Video
        }
        Codec::MpegAudio | Codec::AacAdts | Codec::AacLatm | Codec::Ac3 | Codec::Eac3 => {
            Media::Audio
        }
        Codec::DvbSubtitle | Codec::Teletext => Media::Subtitle,
        Codec::Id3 => Media::Metadata,
        Codec::Unknown => Media::Unknown,
    }
}

/// One PMT elementary stream. Descriptors with several entries report their first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    /// format_identifier of the first registration descriptor, zero when absent.
    pub registration: u32,
    pub pid: u16,
    pub composition_page: u16,
    pub ancillary_page: u16,
    pub stream_type: u8,
    /// For an encrypted stream, the codec once decrypted, so check [`StreamInfo::encryption`].
    pub codec: Codec,
    pub encryption: Encryption,
    /// ISO 639-2 in ISO 8859-1, all zero when absent.
    pub language: [u8; 3],
    pub audio_type: u8,
    pub subtitling_type: u8,
    /// `SF_*` bits.
    pub flags: u8,
    /// `dovi_len` body bytes, for `pf_hdr::dovi::DoviConfig::from_ts_descriptor`.
    pub dovi: [u8; DOVI_BODY_MAX],
    pub dovi_len: u8,
    /// From the HEVC_video_descriptor, 0xFF without one, 3 is no indication.
    pub hdr_wcg_idc: u8,
}

/// `None` unless three ASCII letters.
pub fn language_str(info: &StreamInfo) -> Option<&str> {
    if info.language.iter().all(u8::is_ascii_alphabetic) {
        std::str::from_utf8(&info.language).ok()
    } else {
        None
    }
}

/// H.222.0 Table 2-34 unless noted.
pub fn codec_of_stream_type(stream_type: u8) -> Codec {
    match stream_type {
        0x01 => Codec::Mpeg1Video,
        0x02 => Codec::Mpeg2Video,
        0x03 | 0x04 => Codec::MpegAudio,
        0x0F => Codec::AacAdts,
        0x10 => Codec::Mpeg4Visual,
        0x11 => Codec::AacLatm,
        0x1B => Codec::H264,
        0x24 => Codec::Hevc,
        // ATSC A/52:2012 Annex A 4.1.
        0x81 => Codec::Ac3,
        // ATSC A/52:2012 Annex G 3.1.
        0x87 => Codec::Eac3,
        _ => Codec::Unknown,
    }
}

/// `None` unless the row declares Sample-AES. `indicator` is its private_data_indicator or zero.
/// 0xDB and 0xCF stand alone, 0xC1 and 0xC2 need the indicator, being in ATSC's user private range.
pub fn sample_aes_codec(stream_type: u8, indicator: u32) -> Option<Codec> {
    match (stream_type, indicator) {
        (STREAM_TYPE_SAMPLE_AES_H264, _) => Some(Codec::H264),
        (STREAM_TYPE_SAMPLE_AES_AAC, _) => Some(Codec::AacAdts),
        (STREAM_TYPE_SAMPLE_AES_AC3, INDICATOR_SAMPLE_AES_AC3) => Some(Codec::Ac3),
        (STREAM_TYPE_SAMPLE_AES_EAC3, INDICATOR_SAMPLE_AES_EAC3) => Some(Codec::Eac3),
        _ => None,
    }
}

/// Table 2-34 types 0x05, 0x13 and 0x16 carry sections, ISO/IEC 13818-6 types are not handled.
pub fn carries_pes(stream_type: u8) -> bool {
    !matches!(stream_type, 0x00 | 0x05 | 0x0A..=0x0D | 0x13 | 0x14 | 0x16..=0x19)
}

/// H.222.0 Table 2-3.
pub fn pid_is_assignable(pid: u16) -> bool {
    (PID_FIRST_FREE..PID_NULL).contains(&pid)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsiError {
    Short,
    Syntax,
    Length,
    Crc,
}

/// H.222.0 Tables 2-30 and 2-33.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    /// Used as a content fingerprint.
    pub crc: u32,
    /// transport_stream_id of a PAT, program_number of a PMT.
    pub id: u16,
    pub table_id: u8,
    pub version: u8,
    pub current: bool,
    pub section_number: u8,
    pub last_section_number: u8,
}

#[inline]
fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

#[inline]
fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?]))
}

/// 3 plus the 12-bit section_length.
#[inline]
pub fn section_total_len(b1: u8, b2: u8) -> usize {
    3 + ((usize::from(b1 & 0x0F) << 8) | usize::from(b2))
}

pub fn check_section(sec: &[u8]) -> Result<SectionHeader, PsiError> {
    if sec.len() < LONG_HEADER + CRC_LEN {
        return Err(PsiError::Short);
    }
    if sec[1] & 0x80 == 0 {
        return Err(PsiError::Syntax);
    }
    if section_total_len(sec[1], sec[2]) != sec.len() {
        return Err(PsiError::Length);
    }
    if crc32(sec) != 0 {
        return Err(PsiError::Crc);
    }
    let n = sec.len();
    Ok(SectionHeader {
        crc: u32::from_be_bytes([sec[n - 4], sec[n - 3], sec[n - 2], sec[n - 1]]),
        id: u16::from_be_bytes([sec[3], sec[4]]),
        table_id: sec[0],
        version: (sec[5] >> 1) & 0x1F,
        current: sec[5] & 0x01 != 0,
        section_number: sec[6],
        last_section_number: sec[7],
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatEntry {
    /// Zero marks the network PID.
    pub program_number: u16,
    pub pid: u16,
}

/// Raw rows of a checked PAT section, H.222.0 Table 2-30.
pub fn pat_rows(sec: &[u8]) -> Result<&[[u8; 4]], PsiError> {
    let end = sec.len().checked_sub(CRC_LEN).ok_or(PsiError::Short)?;
    let (rows, rest) = sec.get(LONG_HEADER..end).ok_or(PsiError::Short)?.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(PsiError::Length);
    }
    Ok(rows)
}

#[inline]
pub fn pat_entry(row: &[u8; 4]) -> PatEntry {
    PatEntry {
        program_number: u16::from_be_bytes([row[0], row[1]]),
        pid: u16::from_be_bytes([row[2], row[3]]) & 0x1FFF,
    }
}

/// H.222.0 Table 2-33.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmtHead<'a> {
    /// 0x1FFF when the program has no PCR.
    pub pcr_pid: u16,
    pub program_info: &'a [u8],
    /// The elementary stream loop, walked with [`next_es`].
    pub streams: &'a [u8],
}

/// Splits a checked PMT section, verifying that the stream loop is well formed.
pub fn pmt_head(sec: &[u8]) -> Result<PmtHead<'_>, PsiError> {
    let end = sec.len().checked_sub(CRC_LEN).ok_or(PsiError::Short)?;
    let pcr_pid = be16(sec, 8).ok_or(PsiError::Short)? & 0x1FFF;
    let info_len = usize::from(be16(sec, 10).ok_or(PsiError::Short)? & 0x0FFF);
    let info_end = 12 + info_len;
    if info_end > end {
        return Err(PsiError::Length);
    }
    let streams = &sec[info_end..end];
    let mut rest = streams;
    while !rest.is_empty() {
        let es_len = usize::from(be16(rest, 3).ok_or(PsiError::Length)? & 0x0FFF);
        rest = rest.get(5 + es_len..).ok_or(PsiError::Length)?;
    }
    Ok(PmtHead { pcr_pid, program_info: &sec[12..info_end], streams })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EsEntry<'a> {
    pub stream_type: u8,
    pub pid: u16,
    pub info: &'a [u8],
}

/// `None` at the end or at a malformed row.
pub fn next_es<'a>(streams: &mut &'a [u8]) -> Option<EsEntry<'a>> {
    let s = *streams;
    let stream_type = *s.first()?;
    let pid = be16(s, 1)? & 0x1FFF;
    let es_len = usize::from(be16(s, 3)? & 0x0FFF);
    let info = s.get(5..5 + es_len)?;
    *streams = &s[5 + es_len..];
    Some(EsEntry { stream_type, pid, info })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor<'a> {
    pub tag: u8,
    pub body: &'a [u8],
}

/// `None` at the end, or with bytes left when a descriptor_length overruns the loop.
pub fn next_descriptor<'a>(loop_bytes: &mut &'a [u8]) -> Option<Descriptor<'a>> {
    let d = *loop_bytes;
    let tag = *d.first()?;
    let len = usize::from(*d.get(1)?);
    let body = d.get(2..2 + len)?;
    *loop_bytes = &d[2 + len..];
    Some(Descriptor { tag, body })
}

pub fn registration_of(mut loop_bytes: &[u8]) -> u32 {
    while let Some(d) = next_descriptor(&mut loop_bytes) {
        if d.tag == DESC_REGISTRATION
            && let Some(id) = be32(d.body, 0)
        {
            return id;
        }
    }
    0
}

/// metadata_format 0xFF with metadata_format_identifier "ID3 ", H.222.0 Table 2-87.
fn metadata_is_id3(body: &[u8]) -> bool {
    let Some(application_format) = be16(body, 0) else { return false };
    let at = if application_format == 0xFFFF { 6 } else { 2 };
    body.get(at) == Some(&0xFF) && be32(body, at + 1) == Some(FORMAT_ID3)
}

fn language_at(body: &[u8]) -> Option<[u8; 3]> {
    Some([*body.first()?, *body.get(1)?, *body.get(2)?])
}

pub fn stream_info(stream_type: u8, pid: u16, info: &[u8]) -> StreamInfo {
    const SEEN_AC3: u8 = 0x01;
    const SEEN_EAC3: u8 = 0x02;
    const SEEN_SUBTITLE: u8 = 0x04;
    const SEEN_TELETEXT: u8 = 0x08;
    const SEEN_ID3: u8 = 0x10;
    const SEEN_LANGUAGE: u8 = 0x20;

    let mut s = StreamInfo {
        registration: 0,
        pid,
        composition_page: 0,
        ancillary_page: 0,
        stream_type,
        codec: codec_of_stream_type(stream_type),
        encryption: Encryption::None,
        language: [0; 3],
        audio_type: 0,
        subtitling_type: 0,
        flags: 0,
        dovi: [0; DOVI_BODY_MAX],
        dovi_len: 0,
        hdr_wcg_idc: 0xFF,
    };
    let mut seen = 0u8;
    let mut dvb_language = [0u8; 3];
    let mut indicator = 0u32;
    let mut rest = info;
    while let Some(d) = next_descriptor(&mut rest) {
        let b = d.body;
        match d.tag {
            DESC_REGISTRATION => {
                if s.registration == 0 {
                    s.registration = be32(b, 0).unwrap_or(0);
                }
            }
            DESC_ISO_639_LANGUAGE => {
                if seen & SEEN_LANGUAGE == 0
                    && let (Some(lang), Some(&audio_type)) = (language_at(b), b.get(3))
                {
                    s.language = lang;
                    s.audio_type = audio_type;
                    seen |= SEEN_LANGUAGE;
                }
            }
            DESC_METADATA => {
                if metadata_is_id3(b) {
                    seen |= SEEN_ID3;
                }
            }
            DESC_PRIVATE_DATA_INDICATOR => {
                if indicator == 0 {
                    indicator = be32(b, 0).unwrap_or(0);
                }
            }
            // Rows of language, type and magazine, page number. EN 300 468 table 101.
            DESC_DVB_TELETEXT => {
                if seen & SEEN_TELETEXT == 0 {
                    dvb_language = language_at(b).unwrap_or(dvb_language);
                }
                seen |= SEEN_TELETEXT;
            }
            // Rows of language, subtitling_type and two page ids. EN 300 468 table 99.
            DESC_DVB_SUBTITLING => {
                if seen & SEEN_SUBTITLE == 0
                    && let (Some(lang), Some(&kind), Some(composition), Some(ancillary)) =
                        (language_at(b), b.get(3), be16(b, 4), be16(b, 6))
                {
                    dvb_language = lang;
                    s.subtitling_type = kind;
                    s.composition_page = composition;
                    s.ancillary_page = ancillary;
                }
                seen |= SEEN_SUBTITLE;
            }
            DESC_DOVI_VIDEO if s.dovi_len == 0 && b.len() >= 4 => {
                let n = b.len().min(DOVI_BODY_MAX);
                s.dovi[..n].copy_from_slice(&b[..n]);
                s.dovi_len = n as u8;
            }
            // HDR_WCG_idc follows the profile and level fields.
            DESC_HEVC_VIDEO if b.len() >= 13 => s.hdr_wcg_idc = b[12] & 0x03,
            DESC_DVB_AC3 => seen |= SEEN_AC3,
            DESC_DVB_EAC3 => seen |= SEEN_EAC3,
            _ => {}
        }
    }
    if !rest.is_empty() {
        s.flags |= SF_DESCRIPTORS_TRUNCATED;
    }
    if seen & SEEN_LANGUAGE == 0 {
        s.language = dvb_language;
    }
    // Before the registration, so an encrypted AC-3 stream never reads as a clear one.
    if let Some(codec) = sample_aes_codec(stream_type, indicator) {
        s.codec = codec;
        s.encryption = Encryption::SampleAes;
        return s;
    }
    let by_registration = match s.registration {
        FORMAT_AC3 => Codec::Ac3,
        FORMAT_EAC3 => Codec::Eac3,
        _ => Codec::Unknown,
    };
    match stream_type {
        // DVB names content by descriptor tag (A/52 Annex A 5.1, EN 300 468 annex D, 6.2.41,
        // 6.2.43), other systems by registration (A/52 Annex A 3, Annex G 2).
        0x06 => {
            s.codec = if seen & SEEN_AC3 != 0 {
                Codec::Ac3
            } else if seen & SEEN_EAC3 != 0 {
                Codec::Eac3
            } else if seen & SEEN_SUBTITLE != 0 {
                Codec::DvbSubtitle
            } else if seen & SEEN_TELETEXT != 0 {
                Codec::Teletext
            } else {
                by_registration
            };
        }
        0x15 => {
            if seen & SEEN_ID3 != 0 {
                s.codec = Codec::Id3;
            }
        }
        0x80..=0xFF if s.codec == Codec::Unknown => s.codec = by_registration,
        _ => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_crc(body: &[u8]) -> Vec<u8> {
        let mut v = Vec::from(body);
        let crc = crc32(&v);
        v.extend_from_slice(&crc.to_be_bytes());
        v
    }

    #[test]
    fn stream_types() {
        assert_eq!(codec_of_stream_type(0x1B), Codec::H264);
        assert_eq!(codec_of_stream_type(0x24), Codec::Hevc);
        assert_eq!(codec_of_stream_type(0x02), Codec::Mpeg2Video);
        assert_eq!(codec_of_stream_type(0x01), Codec::Mpeg1Video);
        assert_eq!(codec_of_stream_type(0x10), Codec::Mpeg4Visual);
        assert_eq!(codec_of_stream_type(0x0F), Codec::AacAdts);
        assert_eq!(codec_of_stream_type(0x11), Codec::AacLatm);
        assert_eq!(codec_of_stream_type(0x03), Codec::MpegAudio);
        assert_eq!(codec_of_stream_type(0x04), Codec::MpegAudio);
        assert_eq!(codec_of_stream_type(0x81), Codec::Ac3);
        assert_eq!(codec_of_stream_type(0x87), Codec::Eac3);
        assert_eq!(codec_of_stream_type(0x06), Codec::Unknown);
        assert_eq!(codec_of_stream_type(0xEA), Codec::Unknown);
        assert_eq!(media_of(Codec::Hevc), Media::Video);
        assert_eq!(media_of(Codec::Eac3), Media::Audio);
        assert_eq!(media_of(Codec::Teletext), Media::Subtitle);
        assert_eq!(media_of(Codec::Id3), Media::Metadata);
        assert!(carries_pes(0x1B));
        assert!(carries_pes(0x06));
        assert!(!carries_pes(0x05));
        assert!(pid_is_assignable(0x10));
        assert!(pid_is_assignable(0x1FFE));
        assert!(!pid_is_assignable(0x0F));
        assert!(!pid_is_assignable(0x1FFF));
    }

    #[test]
    fn the_dovi_and_hevc_video_descriptors_are_kept() {
        let bits: u16 = 8 << 9 | 6 << 3 | 1 << 2 | 1;
        let mut d = vec![DESC_DOVI_VIDEO, 5, 1, 0];
        d.extend_from_slice(&bits.to_be_bytes());
        d.push(4 << 4);
        d.extend_from_slice(&[
            DESC_HEVC_VIDEO,
            13,
            0x02,
            0x20,
            0,
            0,
            0,
            0x90,
            0,
            0,
            0,
            0,
            0,
            153,
            0x02,
        ]);
        let s = stream_info(0x24, 0x1011, &d);
        assert_eq!(s.codec, Codec::Hevc);
        assert_eq!(&s.dovi[..usize::from(s.dovi_len)], &d[2..7]);
        assert_eq!(s.hdr_wcg_idc, 2);
        let plain = stream_info(0x24, 0x1011, &[]);
        assert_eq!((plain.dovi_len, plain.hdr_wcg_idc), (0, 0xFF));
        let short =
            stream_info(0x24, 0x1011, &[DESC_DOVI_VIDEO, 2, 1, 0, DESC_HEVC_VIDEO, 3, 0, 0, 0]);
        assert_eq!((short.dovi_len, short.hdr_wcg_idc), (0, 0xFF));
    }

    #[test]
    fn private_data_by_dvb_descriptor() {
        let s = stream_info(0x06, 0x101, &[0x6A, 0x01, 0x00]);
        assert_eq!(s.codec, Codec::Ac3);
        let s = stream_info(0x06, 0x101, &[0x7A, 0x01, 0x00, 0x0A, 0x04, b'f', b'r', b'a', 0x00]);
        assert_eq!(s.codec, Codec::Eac3);
        assert_eq!(language_str(&s), Some("fra"));
        let s =
            stream_info(0x06, 0x102, &[0x59, 0x08, b'e', b'n', b'g', 0x10, 0x00, 0x02, 0x00, 0x03]);
        assert_eq!(s.codec, Codec::DvbSubtitle);
        assert_eq!(language_str(&s), Some("eng"));
        assert_eq!((s.subtitling_type, s.composition_page, s.ancillary_page), (0x10, 2, 3));
        let s = stream_info(0x06, 0x103, &[0x56, 0x05, b's', b'w', b'e', 0x09, 0x00]);
        assert_eq!(s.codec, Codec::Teletext);
        assert_eq!(language_str(&s), Some("swe"));
        let s = stream_info(0x06, 0x104, &[0x52, 0x01, 0x07]);
        assert_eq!(s.codec, Codec::Unknown);
        assert_eq!(s.stream_type, 0x06);
        assert_eq!(language_str(&s), None);
    }

    #[test]
    fn private_data_by_registration() {
        let s = stream_info(0x06, 0x101, &[0x05, 0x04, b'A', b'C', b'-', b'3']);
        assert_eq!(s.codec, Codec::Ac3);
        assert_eq!(s.registration, FORMAT_AC3);
        let s = stream_info(0x06, 0x101, &[0x05, 0x04, b'E', b'A', b'C', b'3']);
        assert_eq!(s.codec, Codec::Eac3);
        let s = stream_info(0x90, 0x101, &[0x05, 0x04, b'E', b'A', b'C', b'3']);
        assert_eq!(s.codec, Codec::Eac3);
        let s = stream_info(0x06, 0x101, &[0x05, 0x04, b'B', b'S', b'S', b'D']);
        assert_eq!(s.codec, Codec::Unknown);
        assert_eq!(s.registration, u32::from_be_bytes(*b"BSSD"));
        let s = stream_info(0x1B, 0x101, &[0x05, 0x04, b'A', b'C', b'-', b'3']);
        assert_eq!(s.codec, Codec::H264);
    }

    fn indicator(value: &[u8; 4]) -> [u8; 6] {
        [0x0F, 0x04, value[0], value[1], value[2], value[3]]
    }

    #[test]
    fn sample_aes_streams_are_encrypted_and_named() {
        for (stream_type, value, codec) in [
            (0xDB, b"zavc", Codec::H264),
            (0xCF, b"aacd", Codec::AacAdts),
            (0xC1, b"ac3d", Codec::Ac3),
            (0xC2, b"ec3d", Codec::Eac3),
        ] {
            let s = stream_info(stream_type, 0x101, &indicator(value));
            assert_eq!((s.codec, s.encryption), (codec, Encryption::SampleAes), "{stream_type:#x}");
            assert_eq!((s.stream_type, s.flags), (stream_type, 0));
        }
        let mut info =
            [0x0F, 0x04, b'a', b'c', b'3', b'd', 0x05, 0x06, b'a', b'p', b'a', b'd', 0x00, 0x00];
        let s = stream_info(0xC1, 0x101, &info);
        assert_eq!((s.codec, s.encryption), (Codec::Ac3, Encryption::SampleAes));
        assert_eq!(s.registration, u32::from_be_bytes(*b"apad"));
        info[8..12].copy_from_slice(b"AC-3");
        assert_eq!(stream_info(0xC1, 0x101, &info).encryption, Encryption::SampleAes);
    }

    #[test]
    fn sample_aes_video_and_aac_need_no_indicator() {
        for (stream_type, codec) in [(0xDB, Codec::H264), (0xCF, Codec::AacAdts)] {
            for info in [&[][..], &indicator(b"what")[..], &[0x0F, 0x02, b'z', b'a'][..]] {
                let s = stream_info(stream_type, 0x101, info);
                assert_eq!(
                    (s.codec, s.encryption),
                    (codec, Encryption::SampleAes),
                    "{stream_type:#x}"
                );
            }
        }
    }

    #[test]
    fn the_atsc_range_is_not_encrypted_without_the_indicator() {
        for (stream_type, own, other) in [(0xC1u8, b"ac3d", b"ec3d"), (0xC2, b"ec3d", b"ac3d")] {
            for info in [
                &[][..],
                &indicator(other)[..],
                &[0x0F, 0x03, own[0], own[1], own[2]][..],
                &[0x0F, 0x00][..],
            ] {
                let s = stream_info(stream_type, 0x101, info);
                assert_eq!(
                    (s.codec, s.encryption),
                    (Codec::Unknown, Encryption::None),
                    "{stream_type:#x}"
                );
            }
            let s = stream_info(stream_type, 0x101, &[0x05, 0x04, b'A', b'C', b'-', b'3']);
            assert_eq!((s.codec, s.encryption), (Codec::Ac3, Encryption::None));
        }
        assert_eq!(sample_aes_codec(0xC1, 0), None);
        assert_eq!(sample_aes_codec(0xC2, INDICATOR_SAMPLE_AES_AC3), None);
        assert_eq!(sample_aes_codec(0xC2, INDICATOR_SAMPLE_AES_EAC3), Some(Codec::Eac3));
    }

    #[test]
    fn the_indicator_does_not_encrypt_other_stream_types() {
        for (stream_type, value, codec) in [
            (0x1Bu8, b"zavc", Codec::H264),
            (0x0F, b"aacd", Codec::AacAdts),
            (0x81, b"ac3d", Codec::Ac3),
            (0x87, b"ec3d", Codec::Eac3),
            (0x06, b"ac3d", Codec::Unknown),
            (0xC3, b"ec3d", Codec::Unknown),
        ] {
            let s = stream_info(stream_type, 0x101, &indicator(value));
            assert_eq!((s.codec, s.encryption), (codec, Encryption::None), "{stream_type:#x}");
        }
        assert_eq!(stream_info(0x1B, 0x100, &[]).encryption, Encryption::None);
        assert!(carries_pes(0xDB) && carries_pes(0xCF) && carries_pes(0xC1) && carries_pes(0xC2));
    }

    #[test]
    fn id3_metadata() {
        let desc = [
            0x26, 0x0D, 0xFF, 0xFF, b'I', b'D', b'3', b' ', 0xFF, b'I', b'D', b'3', b' ', 0x00,
            0x0F,
        ];
        assert_eq!(stream_info(0x15, 0x102, &desc).codec, Codec::Id3);
        let desc = [0x26, 0x09, 0x01, 0x00, 0xFF, b'I', b'D', b'3', b' ', 0x00, 0x0F];
        assert_eq!(stream_info(0x15, 0x102, &desc).codec, Codec::Id3);
        let desc = [0x26, 0x09, 0x01, 0x00, 0xFF, b'K', b'L', b'V', b'A', 0x00, 0x0F];
        assert_eq!(stream_info(0x15, 0x102, &desc).codec, Codec::Unknown);
        assert_eq!(stream_info(0x15, 0x102, &[]).codec, Codec::Unknown);
        let desc = [0x26, 0x09, 0x01, 0x00, 0xFF, b'I', b'D', b'3', b' ', 0x00, 0x0F];
        assert_eq!(stream_info(0x0F, 0x102, &desc).codec, Codec::AacAdts);
    }

    #[test]
    fn language_and_truncation() {
        let s = stream_info(0x0F, 0x101, &[0x0A, 0x04, b'n', b'o', b'r', 0x03]);
        assert_eq!(language_str(&s), Some("nor"));
        assert_eq!(s.audio_type, 3);
        assert_eq!(s.flags, 0);
        let s = stream_info(0x0F, 0x101, &[0x0A, 0x09, b'n', b'o', b'r', 0x03]);
        assert_eq!(s.flags, SF_DESCRIPTORS_TRUNCATED);
        assert_eq!(language_str(&s), None);
        let s = stream_info(0x0F, 0x101, &[0x0A, 0x02, b'n', b'o']);
        assert_eq!(s.flags, 0);
        assert_eq!(s.language, [0; 3]);
        let s = stream_info(
            0x06,
            0x101,
            &[0x59, 0x08, b'e', b'n', b'g', 0x10, 0, 1, 0, 1, 0x0A, 0x04, b'd', b'e', b'u', 0x00],
        );
        assert_eq!(language_str(&s), Some("deu"));
    }

    #[test]
    fn pat_section() {
        let sec = with_crc(&[
            0x00, 0xB0, 0x11, 0x12, 0x34, 0xC5, 0x00, 0x00, 0x00, 0x00, 0xE0, 0x10, 0x00, 0x01,
            0xF0, 0x00,
        ]);
        let h = check_section(&sec).unwrap();
        assert_eq!((h.table_id, h.id, h.version, h.current), (0, 0x1234, 2, true));
        let rows = pat_rows(&sec).unwrap();
        let entries: Vec<PatEntry> = rows.iter().map(pat_entry).collect();
        assert_eq!(
            entries,
            [
                PatEntry { program_number: 0, pid: 0x10 },
                PatEntry { program_number: 1, pid: 0x1000 }
            ]
        );
    }

    #[test]
    fn section_rejects() {
        let good =
            with_crc(&[0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00]);
        assert!(check_section(&good).is_ok());
        let mut bad = good.clone();
        bad[9] ^= 1;
        assert_eq!(check_section(&bad), Err(PsiError::Crc));
        assert_eq!(check_section(&good[..11]), Err(PsiError::Short));
        assert_eq!(check_section(&good[..15]), Err(PsiError::Length));
        let short_form =
            with_crc(&[0x00, 0x30, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00]);
        assert_eq!(check_section(&short_form), Err(PsiError::Syntax));
        let odd = with_crc(&[0x00, 0xB0, 0x0B, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01]);
        assert!(check_section(&odd).is_ok());
        assert_eq!(pat_rows(&odd), Err(PsiError::Length));
    }

    #[test]
    fn pmt_section() {
        let sec = with_crc(&[
            0x02, 0xB0, 0x1D, 0x00, 0x01, 0xC1, 0x00, 0x00, // header
            0xE1, 0x00, // PCR PID 0x100
            0xF0, 0x06, 0x05, 0x04, b'H', b'D', b'M', b'V', // program info
            0x1B, 0xE1, 0x00, 0xF0, 0x00, // H.264 on 0x100
            0x0F, 0xE1, 0x01, 0xF0, 0x00, // AAC on 0x101
        ]);
        check_section(&sec).unwrap();
        let head = pmt_head(&sec).unwrap();
        assert_eq!(head.pcr_pid, 0x100);
        assert_eq!(registration_of(head.program_info), u32::from_be_bytes(*b"HDMV"));
        let mut rest = head.streams;
        let a = next_es(&mut rest).unwrap();
        let b = next_es(&mut rest).unwrap();
        assert!(next_es(&mut rest).is_none());
        assert_eq!((a.stream_type, a.pid), (0x1B, 0x100));
        assert_eq!((b.stream_type, b.pid), (0x0F, 0x101));
    }

    #[test]
    fn pmt_length_errors() {
        let sec =
            with_crc(&[0x02, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x20]);
        assert_eq!(pmt_head(&sec), Err(PsiError::Length));
        let sec = with_crc(&[
            0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00, 0x1B, 0xE1,
            0x00, 0xF0, 0x09,
        ]);
        assert_eq!(pmt_head(&sec), Err(PsiError::Length));
        let sec = with_crc(&[
            0x02, 0xB0, 0x10, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00, 0x1B, 0xE1,
            0x00,
        ]);
        assert_eq!(pmt_head(&sec), Err(PsiError::Length));
        assert_eq!(pmt_head(&[0x02, 0xB0, 0x05, 0, 1, 0xC1, 0, 0]), Err(PsiError::Short));
    }
}
