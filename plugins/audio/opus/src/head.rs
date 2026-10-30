//! The `OpusHead` identification header, RFC 7845 section 5.1 and 5.1.1.

use std::fmt;

pub const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";

/// A family 0 head's length, the others append the mapping table.
pub const OPUS_HEAD_MIN_LEN: usize = 19;

pub const MAX_MAPPING: usize = 255;

/// Each variant is a MUST of RFC 7845 section 5.1 or 5.1.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpusHeadError {
    TooShort { got: usize, need: usize },
    BadMagic,
    IncompatibleVersion { version: u8 },
    ZeroChannels,
    ChannelCountForFamily { family: u8, channels: u8 },
    ZeroStreams,
    CoupledExceedsStreams { streams: u8, coupled: u8 },
    TooManyDecodedChannels { streams: u8, coupled: u8 },
    MappingIndexOutOfRange { output_channel: u8, index: u8, decoded_channels: u8 },
}

impl fmt::Display for OpusHeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            OpusHeadError::TooShort { got, need } => {
                write!(f, "OpusHead too short, {got} bytes, need {need}")
            }
            OpusHeadError::BadMagic => f.write_str("OpusHead magic missing"),
            OpusHeadError::IncompatibleVersion { version } => {
                write!(f, "OpusHead major version {} is incompatible", version >> 4)
            }
            OpusHeadError::ZeroChannels => f.write_str("OpusHead output channel count is zero"),
            OpusHeadError::ChannelCountForFamily { family, channels } => {
                write!(f, "OpusHead channel count {channels} invalid for mapping family {family}")
            }
            OpusHeadError::ZeroStreams => f.write_str("OpusHead stream count is zero"),
            OpusHeadError::CoupledExceedsStreams { streams, coupled } => {
                write!(f, "OpusHead coupled count {coupled} exceeds stream count {streams}")
            }
            OpusHeadError::TooManyDecodedChannels { streams, coupled } => {
                write!(f, "OpusHead decoded channels {coupled} + {streams} exceed 255")
            }
            OpusHeadError::MappingIndexOutOfRange { output_channel, index, decoded_channels } => {
                write!(
                    f,
                    "OpusHead mapping index {index} of output channel {output_channel} is neither \
                     below {decoded_channels} nor 255"
                )
            }
        }
    }
}

impl std::error::Error for OpusHeadError {}

impl OpusHeadError {
    /// A static tag for a log field.
    pub const fn reason(&self) -> &'static str {
        match self {
            OpusHeadError::TooShort { .. } => "too_short",
            OpusHeadError::BadMagic => "bad_magic",
            OpusHeadError::IncompatibleVersion { .. } => "incompatible_version",
            OpusHeadError::ZeroChannels => "zero_channels",
            OpusHeadError::ChannelCountForFamily { .. } => "channel_count_for_family",
            OpusHeadError::ZeroStreams => "zero_streams",
            OpusHeadError::CoupledExceedsStreams { .. } => "coupled_exceeds_streams",
            OpusHeadError::TooManyDecodedChannels { .. } => "too_many_decoded_channels",
            OpusHeadError::MappingIndexOutOfRange { .. } => "mapping_index_out_of_range",
        }
    }
}

/// Family 0 gets the table the RFC pins, one stream, `channels - 1` coupled, identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusHead {
    pub version: u8,
    pub channel_count: u8,
    /// 48 kHz samples to discard at the start (RFC 7845 section 4.2).
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    /// Q7.8 dB.
    pub output_gain_q7_8: i16,
    pub mapping_family: u8,
    pub stream_count: u8,
    pub coupled_count: u8,
    mapping: [u8; MAX_MAPPING],
}

impl OpusHead {
    pub fn parse(packet: &[u8]) -> Result<Self, OpusHeadError> {
        let Some(fixed) = packet.first_chunk::<OPUS_HEAD_MIN_LEN>() else {
            return Err(OpusHeadError::TooShort { got: packet.len(), need: OPUS_HEAD_MIN_LEN });
        };
        if fixed[..8] != OPUS_HEAD_MAGIC[..] {
            return Err(OpusHeadError::BadMagic);
        }
        let version = fixed[8];
        if version >> 4 > 0 {
            return Err(OpusHeadError::IncompatibleVersion { version });
        }
        let channels = fixed[9];
        if channels == 0 {
            return Err(OpusHeadError::ZeroChannels);
        }
        let family = fixed[18];
        let mut head = OpusHead {
            version,
            channel_count: channels,
            pre_skip: u16::from_le_bytes([fixed[10], fixed[11]]),
            input_sample_rate: u32::from_le_bytes([fixed[12], fixed[13], fixed[14], fixed[15]]),
            output_gain_q7_8: i16::from_le_bytes([fixed[16], fixed[17]]),
            mapping_family: family,
            stream_count: 1,
            coupled_count: 0,
            mapping: [0; MAX_MAPPING],
        };
        let n = usize::from(channels);
        let too_wide = match family {
            0 => channels > 2,
            1 => channels > 8,
            _ => false,
        };
        if too_wide {
            return Err(OpusHeadError::ChannelCountForFamily { family, channels });
        }
        if family == 0 {
            head.coupled_count = channels - 1;
            for (i, m) in head.mapping[..n].iter_mut().enumerate() {
                *m = i as u8;
            }
            return Ok(head);
        }
        let need = OPUS_HEAD_MIN_LEN + 2 + n;
        let Some(table) = packet.get(OPUS_HEAD_MIN_LEN..need) else {
            return Err(OpusHeadError::TooShort { got: packet.len(), need });
        };
        let (streams, coupled) = (table[0], table[1]);
        if streams == 0 {
            return Err(OpusHeadError::ZeroStreams);
        }
        if coupled > streams {
            return Err(OpusHeadError::CoupledExceedsStreams { streams, coupled });
        }
        let Some(decoded) = streams.checked_add(coupled) else {
            return Err(OpusHeadError::TooManyDecodedChannels { streams, coupled });
        };
        for (c, &index) in table[2..].iter().enumerate() {
            if index != 255 && index >= decoded {
                return Err(OpusHeadError::MappingIndexOutOfRange {
                    output_channel: c as u8,
                    index,
                    decoded_channels: decoded,
                });
            }
        }
        head.stream_count = streams;
        head.coupled_count = coupled;
        head.mapping[..n].copy_from_slice(&table[2..]);
        Ok(head)
    }

    /// The decoded channel per output channel, 255 is silence.
    pub fn mapping(&self) -> &[u8] {
        &self.mapping[..usize::from(self.channel_count)]
    }
}

/// RFC 7845 section 5.1 item 6, rounded and saturated.
pub fn apply_output_gain(pcm: &mut [i16], gain_q7_8: i16) {
    if gain_q7_8 == 0 {
        return;
    }
    let scale = 10f64.powf(f64::from(gain_q7_8) / (20.0 * 256.0));
    for s in pcm.iter_mut() {
        let scaled = (f64::from(*s) * scale).round();
        *s = scaled.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
    }
}

pub fn apply_output_gain_f32(pcm: &mut [f32], gain_q7_8: i16) {
    if gain_q7_8 == 0 {
        return;
    }
    let scale = 10f64.powf(f64::from(gain_q7_8) / (20.0 * 256.0)) as f32;
    for s in pcm.iter_mut() {
        *s *= scale;
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test heads, not a hot path
mod tests {
    use super::*;

    fn head(version: u8, channels: u8, family: u8, table: &[u8]) -> Vec<u8> {
        let mut h = OPUS_HEAD_MAGIC.to_vec();
        h.extend_from_slice(&[version, channels, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0x00, 0xFF, family]);
        h.extend_from_slice(table);
        h
    }

    #[test]
    fn family_0_fields_and_synthesized_table() {
        let h = OpusHead::parse(&head(1, 2, 0, &[])).expect("stereo");
        assert_eq!((h.version, h.channel_count, h.pre_skip), (1, 2, 312));
        assert_eq!((h.input_sample_rate, h.output_gain_q7_8, h.mapping_family), (48_000, -256, 0));
        assert_eq!((h.stream_count, h.coupled_count, h.mapping()), (1, 1, &[0u8, 1][..]));
        let h = OpusHead::parse(&head(0x0F, 1, 0, &[])).expect("mono, minor version 15");
        assert_eq!((h.stream_count, h.coupled_count, h.mapping()), (1, 0, &[0u8][..]));
    }

    #[test]
    fn family_1_5_1_table() {
        // RFC 7845 section 5.1.1.2.
        let mut raw = OPUS_HEAD_MAGIC.to_vec();
        raw.extend_from_slice(&[1, 6, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 1, 4, 2]);
        raw.extend_from_slice(&[0, 4, 1, 2, 3, 5]);
        let h = OpusHead::parse(&raw).expect("5.1");
        assert_eq!(
            (h.channel_count, h.mapping_family, h.stream_count, h.coupled_count),
            (6, 1, 4, 2)
        );
        assert_eq!(h.mapping(), &[0, 4, 1, 2, 3, 5]);
    }

    #[test]
    fn family_255_takes_any_width_and_silence() {
        let mut table = vec![2, 0];
        table.extend((0..200u16).map(|i| if i % 3 == 0 { 255 } else { (i % 2) as u8 }));
        let h = OpusHead::parse(&head(1, 200, 255, &table)).expect("200 channels");
        assert_eq!(h.mapping().len(), 200);
        assert_eq!(h.mapping()[..4], [255, 1, 0, 255]);
        let mut long = head(1, 2, 1, &[1, 1, 0, 1]);
        long.extend_from_slice(&[9, 9]);
        assert_eq!(OpusHead::parse(&long).expect("trailing").mapping(), &[0, 1]);
    }

    #[test]
    fn refusals() {
        use OpusHeadError as E;
        let rows: [(Vec<u8>, E); 11] = [
            (head(1, 2, 0, &[])[..10].to_vec(), E::TooShort { got: 10, need: 19 }),
            (b"OpusHeaX\x01\x02\x38\x01\x80\xBB\0\0\0\0\0".to_vec(), E::BadMagic),
            (head(0x10, 2, 0, &[]), E::IncompatibleVersion { version: 0x10 }),
            (head(1, 0, 0, &[]), E::ZeroChannels),
            (head(1, 3, 0, &[]), E::ChannelCountForFamily { family: 0, channels: 3 }),
            (head(1, 9, 1, &[]), E::ChannelCountForFamily { family: 1, channels: 9 }),
            (head(1, 6, 1, &[4, 2, 0, 4, 1]), E::TooShort { got: 24, need: 27 }),
            (head(1, 2, 1, &[0, 0, 0, 0]), E::ZeroStreams),
            (head(1, 2, 1, &[1, 2, 0, 1]), E::CoupledExceedsStreams { streams: 1, coupled: 2 }),
            (
                head(1, 1, 255, &[200, 100, 0]),
                E::TooManyDecodedChannels { streams: 200, coupled: 100 },
            ),
            (
                head(1, 3, 1, &[2, 0, 0, 2, 1]),
                E::MappingIndexOutOfRange { output_channel: 1, index: 2, decoded_channels: 2 },
            ),
        ];
        for (raw, want) in rows {
            assert_eq!(OpusHead::parse(&raw), Err(want), "{raw:?}");
            let s = want.to_string();
            assert!(s.starts_with("OpusHead ") && !s.contains('\u{2014}'), "{s}");
        }
        assert_eq!(OpusHead::parse(&[]), Err(E::TooShort { got: 0, need: 19 }));
    }

    #[test]
    fn output_gain_scales_rounds_and_saturates() {
        let mut pcm = [1000i16, -1000, 0, 30_000, -30_000, 1];
        apply_output_gain(&mut pcm, 0);
        assert_eq!(pcm, [1000, -1000, 0, 30_000, -30_000, 1]);
        apply_output_gain(&mut pcm, 6 * 256);
        assert_eq!(pcm, [1995, -1995, 0, i16::MAX, i16::MIN, 2]);
        let mut pcm = [1000i16, -1005, 4];
        apply_output_gain(&mut pcm, -20 * 256);
        assert_eq!(pcm, [100, -101, 0]);
        for gain in [-32768i16, -1, 1, 77, 32767] {
            let scale = 10f64.powf(f64::from(gain) / 5120.0);
            for s in [i16::MIN, -12345, -1, 0, 1, 12345, i16::MAX] {
                let mut one = [s];
                apply_output_gain(&mut one, gain);
                let want = (f64::from(s) * scale).round().clamp(-32768.0, 32767.0) as i16;
                assert_eq!(one[0], want, "{s} at {gain}");
            }
        }
    }
}
