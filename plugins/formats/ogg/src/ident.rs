//! Codec identification headers on an Ogg bos page, Opus, Vorbis and Speex.

/// The Opus identification header (RFC 7845 section 5.1), as much of it as a container needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusHead {
    /// Channel count, never zero (RFC 7845 section 5.1).
    pub channels: u8,
    /// Samples to discard from the decoder output at start of playback, at 48 kHz (section 5.1).
    pub pre_skip: u16,
    /// The original input sample rate in Hz (RFC 7845 section 5.1). Informational, Opus decodes at
    /// 48 kHz.
    pub input_sample_rate: u32,
}

const OPUS_HEAD_MIN: usize = 19;

/// Parse an Opus identification header packet (RFC 7845 section 5.1).
pub fn parse_opus_head(packet: &[u8]) -> Option<OpusHead> {
    let rest = packet.strip_prefix(b"OpusHead")?;
    if rest.len() < OPUS_HEAD_MIN - 8 {
        return None;
    }
    if rest[0] >> 4 != 0 {
        return None; // incompatible major version
    }
    let channels = rest[1];
    if channels == 0 {
        return None;
    }
    Some(OpusHead {
        channels,
        pre_skip: u16::from_le_bytes([rest[2], rest[3]]),
        input_sample_rate: u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]),
    })
}

/// The Vorbis identification header (Vorbis I section 4.2.2), as much of it as a container needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VorbisIdent {
    /// Channel count, never zero (Vorbis I section 4.2.2).
    pub channels: u8,
    /// Sample rate in Hz.
    pub sample_rate: u32,
}

const VORBIS_IDENT_LEN: usize = 30;

/// Parse a Vorbis identification header packet.
pub fn parse_vorbis_ident(packet: &[u8]) -> Option<VorbisIdent> {
    let rest = packet.strip_prefix(b"\x01vorbis")?;
    if packet.len() < VORBIS_IDENT_LEN {
        return None;
    }
    // Vorbis packs header integers LSB-first (Vorbis I section 1.2.2).
    if u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) != 0 {
        return None;
    }
    let channels = rest[4];
    let sample_rate = u32::from_le_bytes([rest[5], rest[6], rest[7], rest[8]]);
    if channels == 0 || sample_rate == 0 {
        return None;
    }
    Some(VorbisIdent { channels, sample_rate })
}

/// The Speex identification header (Speex manual section 7.3, Table 7.1), as much of it as a
/// container needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeexHead {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count (`nb_channels`).
    pub channels: u8,
}

/// `Speex` with three trailing spaces (Speex manual section 7.3).
pub const SPEEX_MAGIC: &[u8; 8] = b"Speex   ";

const SPEEX_HEAD_LEN: usize = 80;

const SPEEX_RATE_AT: usize = 36;

const SPEEX_CHANNELS_AT: usize = 48;

/// Parse a Speex identification header packet (Speex manual section 7.3, Table 7.1).
pub fn parse_speex_head(packet: &[u8]) -> Option<SpeexHead> {
    if !packet.starts_with(SPEEX_MAGIC) || packet.len() < SPEEX_HEAD_LEN {
        return None;
    }
    let le32 = |at: usize| -> i32 {
        let b = &packet[at..at + 4];
        i32::from_le_bytes([b[0], b[1], b[2], b[3]])
    };
    let sample_rate = u32::try_from(le32(SPEEX_RATE_AT)).ok().filter(|&r| r > 0)?;
    let channels = u8::try_from(le32(SPEEX_CHANNELS_AT)).ok().filter(|&c| c > 0)?;
    Some(SpeexHead { sample_rate, channels })
}

/// The codec mapping of a logical bitstream, from the identification packet on its bos page (RFC
/// 3533 section 6 puts exactly that packet there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    Opus,
    Flac,
    Vorbis,
    Speex,
    Theora,
    Skeleton,
    Unknown,
}

const FLAC_MAGIC: [u8; 5] = [0x7F, b'F', b'L', b'A', b'C'];

impl Mapping {
    /// The mapping whose identification packet is `packet`.
    pub fn identify(packet: &[u8]) -> Mapping {
        match packet {
            p if p.starts_with(b"OpusHead") => Mapping::Opus,
            p if p.starts_with(&FLAC_MAGIC) => Mapping::Flac,
            p if p.starts_with(b"\x01vorbis") => Mapping::Vorbis,
            p if p.starts_with(SPEEX_MAGIC) => Mapping::Speex,
            p if p.starts_with(b"\x80theora") => Mapping::Theora,
            p if p.starts_with(b"fishead\0") => Mapping::Skeleton,
            _ => Mapping::Unknown,
        }
    }

    /// The family its packets go out as.
    pub fn family(self) -> &'static str {
        match self {
            Mapping::Opus => "opus",
            Mapping::Flac => "flac/ogg",
            Mapping::Vorbis => "vorbis",
            Mapping::Speex => "speex",
            Mapping::Theora => "theora",
            Mapping::Skeleton => "skeleton",
            Mapping::Unknown => "bytes",
        }
    }

    /// The header packets the stream opens with, the identification packet among them, as its
    /// identification packet `ident` says.
    pub fn header_packets(self, ident: &[u8]) -> u32 {
        match self {
            Mapping::Opus | Mapping::Skeleton => 2,
            Mapping::Vorbis | Mapping::Theora => 3,
            Mapping::Speex => {
                let extra =
                    ident.get(68..72).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                2 + extra.min(16)
            }
            Mapping::Flac => {
                1 + ident.get(7..9).map_or(0, |b| u32::from(u16::from_be_bytes([b[0], b[1]])))
            }
            Mapping::Unknown => 1,
        }
    }

    /// The Vorbis comment body of `packet` where it is this mapping's comment header, the bytes
    /// behind its prefix.
    pub fn comment_body(self, packet: &[u8]) -> Option<&[u8]> {
        match self {
            Mapping::Opus => packet.strip_prefix(b"OpusTags"),
            Mapping::Vorbis => packet.strip_prefix(b"\x03vorbis"),
            Mapping::Theora => packet.strip_prefix(b"\x81theora"),
            Mapping::Speex => Some(packet),
            Mapping::Flac => packet.get(4..).filter(|_| packet[0] & 0x7F == 4),
            Mapping::Skeleton | Mapping::Unknown => None,
        }
    }
}

/// The samples one Opus packet decodes to at 48 kHz, from its TOC byte and frame count (RFC 6716
/// section 3.1).
pub fn opus_packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    // Table 2 of RFC 6716, frame sizes in 48 kHz samples.
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    // A packet holds at most 120 ms (RFC 6716 section 3.2.5).
    let samples = frame * frames;
    (frames > 0 && samples <= 5760).then_some(samples)
}

/// The samples of one FLAC frame from its header (RFC 9639 section 9.1), the block size code in the
/// high nibble of byte 2, the uncommon sizes behind the coded number.
pub fn flac_frame_samples(frame: &[u8]) -> Option<u32> {
    if frame.len() < 5 || frame[0] != 0xFF || frame[1] & 0xFE != 0xF8 {
        return None;
    }
    let code = frame[2] >> 4;
    // The coded frame or sample number is UTF-8 shaped, its length in the leading ones.
    let lead = frame[4].leading_ones() as usize;
    let coded = match lead {
        0 => 1,
        2..=7 => lead,
        _ => return None,
    };
    let at = 4 + coded;
    match code {
        1 => Some(192),
        2..=5 => Some(576 << (code - 2)),
        6 => frame.get(at).map(|b| u32::from(*b) + 1),
        7 => frame.get(at..at + 2).map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])) + 1),
        8..=15 => Some(256 << (code - 8)),
        _ => None,
    }
}

#[cfg(test)]
// Tests build header fixtures.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn opus_head(version: u8, channels: u8, pre_skip: u16, rate: u32) -> Vec<u8> {
        let mut p = b"OpusHead".to_vec();
        p.push(version);
        p.push(channels);
        p.extend_from_slice(&pre_skip.to_le_bytes());
        p.extend_from_slice(&rate.to_le_bytes());
        p.extend_from_slice(&0i16.to_le_bytes()); // output gain
        p.push(0); // channel mapping family
        p
    }

    fn vorbis_ident(version: u32, channels: u8, rate: u32) -> Vec<u8> {
        let mut p = b"\x01vorbis".to_vec();
        p.extend_from_slice(&version.to_le_bytes());
        p.push(channels);
        p.extend_from_slice(&rate.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes()); // bitrate maximum
        p.extend_from_slice(&192_000u32.to_le_bytes()); // bitrate nominal
        p.extend_from_slice(&0u32.to_le_bytes()); // bitrate minimum
        p.push(0xB8); // blocksize_0 = 2^8, blocksize_1 = 2^11
        p.push(0x01); // framing flag
        p
    }

    fn speex_head(rate: i32, channels: i32, extra_headers: i32) -> Vec<u8> {
        let mut p = SPEEX_MAGIC.to_vec();
        let mut version = [0u8; 20];
        version[..5].copy_from_slice(b"1.2rc");
        p.extend_from_slice(&version);
        for f in [
            1,             // speex_version_id
            80,            // header_size
            rate,          // rate
            1,             // mode: wideband
            4,             // mode_bitstream_version
            channels,      // nb_channels
            27_800,        // bitrate
            320,           // frame_size
            0,             // vbr
            1,             // frames_per_packet
            extra_headers, // extra_headers
            0,             // reserved1
            0,             // reserved2
        ] {
            p.extend_from_slice(&f.to_le_bytes());
        }
        p
    }

    #[test]
    fn identifies_every_mapping() {
        assert_eq!(Mapping::identify(&opus_head(1, 2, 312, 48_000)), Mapping::Opus);
        assert_eq!(Mapping::identify(&vorbis_ident(0, 2, 44_100)), Mapping::Vorbis);
        assert_eq!(Mapping::identify(&speex_head(16_000, 1, 0)), Mapping::Speex);
        assert_eq!(Mapping::identify(b"\x7FFLAC\x01\x00\x00\x03fLaC"), Mapping::Flac);
        assert_eq!(Mapping::identify(b"\x80theora\x03\x02"), Mapping::Theora);
        assert_eq!(Mapping::identify(b"fishead\0\x03\0"), Mapping::Skeleton);
        assert_eq!(Mapping::identify(b"BBCD\0"), Mapping::Unknown);
        assert_eq!(Mapping::identify(b""), Mapping::Unknown);
        assert_eq!(Mapping::Flac.header_packets(b"\x7FFLAC\x01\x00\x00\x03fLaC"), 4);
        assert_eq!(Mapping::Speex.header_packets(&speex_head(16_000, 1, 2)), 4);
        assert_eq!(Mapping::Opus.header_packets(b"OpusHead"), 2);
        assert_eq!(Mapping::Flac.comment_body(b"\x84\0\0\x02ab"), Some(&b"ab"[..]));
        assert_eq!(Mapping::Flac.comment_body(b"\x81\0\0\x02ab"), None);
        assert_eq!(Mapping::Opus.comment_body(b"OpusTagsxy"), Some(&b"xy"[..]));
    }

    #[test]
    fn opus_packet_durations() {
        assert_eq!(opus_packet_samples(&[31 << 3]), Some(960));
        assert_eq!(opus_packet_samples(&[(31 << 3) | 1]), Some(1920));
        assert_eq!(opus_packet_samples(&[(31 << 3) | 3, 3]), Some(2880));
        assert_eq!(opus_packet_samples(&[0]), Some(480));
        assert_eq!(opus_packet_samples(&[13 << 3]), Some(960));
        assert_eq!(opus_packet_samples(&[16 << 3]), Some(120));
        assert_eq!(opus_packet_samples(&[]), None);
        assert_eq!(opus_packet_samples(&[(31 << 3) | 3]), None);
        assert_eq!(opus_packet_samples(&[(31 << 3) | 3, 0]), None);
        assert_eq!(opus_packet_samples(&[(31 << 3) | 3, 7]), None);
    }

    #[test]
    fn flac_frame_durations() {
        assert_eq!(flac_frame_samples(&[0xFF, 0xF8, 0xC9, 0x18, 0x00, 0x00]), Some(4096));
        assert_eq!(
            flac_frame_samples(&[0xFF, 0xF9, 0x79, 0x18, 0xC2, 0x80, 0x01, 0x1F]),
            Some(288)
        );
        assert_eq!(flac_frame_samples(&[0xFF, 0xF8, 0x69, 0x18, 0x05, 0x0F]), Some(16));
        assert_eq!(flac_frame_samples(&[0xFF, 0xF8, 0x19, 0x18, 0x00]), Some(192));
        assert_eq!(flac_frame_samples(b"fLaC\0\0\0"), None);
        assert_eq!(flac_frame_samples(&[0xFF, 0xF8, 0x09, 0x18, 0x00]), None);
    }

    #[test]
    fn parses_speex_head() {
        let head = parse_speex_head(&speex_head(16_000, 1, 0)).expect("valid Speex header");
        assert_eq!(head, SpeexHead { sample_rate: 16_000, channels: 1 });
        assert_eq!(speex_head(16_000, 1, 0).len(), SPEEX_HEAD_LEN, "Table 7.1 is 80 octets");
        assert_eq!(
            parse_speex_head(&speex_head(8_000, 2, 3)),
            Some(SpeexHead { sample_rate: 8_000, channels: 2 })
        );
        let mut real = SPEEX_MAGIC.to_vec();
        real.extend_from_slice(b"speex-1.2rc1\0\0\0\0\0\0\0\0");
        real.extend_from_slice(&1i32.to_le_bytes());
        real.extend_from_slice(&80i32.to_le_bytes());
        real.extend_from_slice(&16_000i32.to_le_bytes());
        real.extend_from_slice(&[0u8; 44]);
        real[SPEEX_CHANNELS_AT..SPEEX_CHANNELS_AT + 4].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(parse_speex_head(&real), Some(SpeexHead { sample_rate: 16_000, channels: 1 }));
    }

    #[test]
    fn rejects_bad_speex_head() {
        assert!(parse_speex_head(&speex_head(0, 1, 0)).is_none()); // zero rate
        assert!(parse_speex_head(&speex_head(16_000, 0, 0)).is_none()); // zero channels
        assert!(parse_speex_head(&speex_head(16_000, -2, 0)).is_none());
        assert!(parse_speex_head(&speex_head(16_000, 4096, 0)).is_none());
        assert!(parse_speex_head(&speex_head(-8_000, 1, 0)).is_none());
        let mut wrong = speex_head(16_000, 1, 0);
        wrong[7] = b'!';
        assert!(parse_speex_head(&wrong).is_none());
        assert!(parse_speex_head(b"OpusHead\x01\x02").is_none());
        let full = speex_head(16_000, 2, 0);
        for n in 0..full.len() {
            assert!(parse_speex_head(&full[..n]).is_none(), "truncated to {n} must not parse");
        }
    }

    #[test]
    fn parses_opus_head() {
        let head = parse_opus_head(&opus_head(1, 2, 312, 44_100)).expect("valid OpusHead");
        assert_eq!(head, OpusHead { channels: 2, pre_skip: 312, input_sample_rate: 44_100 });
        assert!(parse_opus_head(&opus_head(0, 1, 0, 0)).is_some());
        assert!(parse_opus_head(&opus_head(0x0F, 1, 0, 0)).is_some());
    }

    #[test]
    fn rejects_bad_opus_head() {
        assert!(parse_opus_head(&opus_head(0x10, 2, 0, 48_000)).is_none()); // major version 1
        assert!(parse_opus_head(&opus_head(1, 0, 0, 48_000)).is_none()); // zero channels
        assert!(parse_opus_head(b"OpusTags\x01\x02").is_none()); // wrong magic
        assert!(parse_opus_head(b"").is_none());
        let full = opus_head(1, 2, 312, 48_000);
        for n in 0..full.len() {
            assert!(parse_opus_head(&full[..n]).is_none(), "truncated to {n} must not parse");
        }
    }

    #[test]
    fn parses_vorbis_ident() {
        let id = parse_vorbis_ident(&vorbis_ident(0, 2, 44_100)).expect("valid ident");
        assert_eq!(id, VorbisIdent { channels: 2, sample_rate: 44_100 });
        const PAYLOAD: [u8; 30] = [
            0x01, 0x76, 0x6f, 0x72, 0x62, 0x69, 0x73, 0x00, 0x00, 0x00, 0x00, 0x02, 0x44, 0xac,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xb5, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xb8, 0x01,
        ];
        assert_eq!(
            parse_vorbis_ident(&PAYLOAD),
            Some(VorbisIdent { channels: 2, sample_rate: 44_100 })
        );
    }

    #[test]
    fn rejects_bad_vorbis_ident() {
        assert!(parse_vorbis_ident(&vorbis_ident(1, 2, 44_100)).is_none()); // version != 0
        assert!(parse_vorbis_ident(&vorbis_ident(0, 0, 44_100)).is_none()); // zero channels
        assert!(parse_vorbis_ident(&vorbis_ident(0, 2, 0)).is_none()); // zero rate
        assert!(parse_vorbis_ident(b"\x03vorbis........................").is_none()); // comment hdr
        let full = vorbis_ident(0, 2, 48_000);
        for n in 0..full.len() {
            assert!(parse_vorbis_ident(&full[..n]).is_none(), "truncated to {n} must not parse");
        }
    }
}
