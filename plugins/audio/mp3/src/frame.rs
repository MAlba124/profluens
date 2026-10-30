//! The MPEG audio frame header (ISO/IEC 11172-3 2.4.1.3, 13818-3 and MPEG-2.5) and the framer.
//! Reserved values are refused. A free format header parses with no bitrate or length.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub lsf: bool,
    pub layer: u8,
    pub crc: bool,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: u32,
    pub padding: bool,
    pub mono: bool,
}

/// kbit/s by bitrate index. MPEG-1 layers I, II, III, then the LSF ladders (13818-3 2.4.2.3).
const KBPS: [[u16; 15]; 5] = [
    [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
    [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

const RATES: [[u32; 3]; 3] =
    [[44_100, 48_000, 32_000], [22_050, 24_000, 16_000], [11_025, 12_000, 8_000]];

impl Header {
    pub fn parse(bytes: &[u8]) -> Option<Header> {
        let raw = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
        if raw >> 21 != 0x7FF {
            return None;
        }
        let version = match (raw >> 19) & 3 {
            3 => 0,
            2 => 1,
            0 => 2,
            _ => return None,
        };
        let layer = match (raw >> 17) & 3 {
            0 => return None,
            l => 4 - l as u8,
        };
        let index = ((raw >> 12) & 0xF) as usize;
        let rate = ((raw >> 10) & 3) as usize;
        if index == 15 || rate == 3 {
            return None;
        }
        let ladder = match (version, layer) {
            (0, l) => usize::from(l) - 1,
            (_, 1) => 3,
            _ => 4,
        };
        Some(Header {
            lsf: version != 0,
            layer,
            crc: (raw >> 16) & 1 == 0,
            bitrate_kbps: (index != 0).then(|| u32::from(KBPS[ladder][index])),
            sample_rate_hz: RATES[version][rate],
            padding: (raw >> 9) & 1 == 1,
            mono: (raw >> 6) & 3 == 3,
        })
    }

    pub fn channel_count(&self) -> u8 {
        if self.mono { 1 } else { 2 }
    }

    /// Samples per channel per frame (ISO/IEC 13818-3 2.4.2.1).
    pub fn samples_per_frame(&self) -> u32 {
        match (self.layer, self.lsf) {
            (1, _) => 384,
            (3, true) => 576,
            _ => 1152,
        }
    }

    pub fn frame_len(&self) -> Option<usize> {
        let bps = self.bitrate_kbps? * 1000;
        let pad = u32::from(self.padding);
        let len = match (self.layer, self.lsf) {
            (1, _) => (12 * bps / self.sample_rate_hz + pad) * 4,
            (3, true) => 72 * bps / self.sample_rate_hz + pad,
            _ => 144 * bps / self.sample_rate_hz + pad,
        };
        Some(len as usize)
    }

    /// Average frame bytes as a numerator over the sampling frequency. `None` for free format and
    /// layer I.
    pub fn frame_bytes(&self) -> Option<(u64, u64)> {
        let bps = u64::from(self.bitrate_kbps?) * 1000;
        let per = match (self.layer, self.lsf) {
            (1, _) => return None,
            (3, true) => 72,
            _ => 144,
        };
        Some((per * bps, u64::from(self.sample_rate_hz)))
    }

    /// Same version, layer and sampling frequency (FFmpeg's `SAME_HEADER_MASK`).
    pub fn same_stream(&self, other: &Header) -> bool {
        (self.lsf, self.layer, self.sample_rate_hz)
            == (other.lsf, other.layer, other.sample_rate_hz)
    }
}

pub(crate) enum Framed {
    Frame {
        offset: usize,
        header: Header,
    },
    /// A header at `offset` whose frame overruns the buffer. A shorter frame deeper in would be a
    /// false sync inside this one.
    NeedMore {
        offset: usize,
    },
    NoSync,
}

/// Frame the first MPEG audio frame of `buf`. A candidate needs the next frame's sync at its end,
/// except for a single whole frame at the end of the buffer.
pub(crate) fn next_frame(buf: &[u8]) -> Framed {
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        if buf[i] != 0xFF || buf[i + 1] & 0xE0 != 0xE0 {
            i += 1;
            continue;
        }
        let Some((header, len)) = Header::parse(&buf[i..]).and_then(|h| Some((h, h.frame_len()?)))
        else {
            i += 1;
            continue;
        };
        if len < 4 {
            i += 1;
            continue;
        }
        if i + len > buf.len() {
            return Framed::NeedMore { offset: i };
        }
        let next = i + len;
        if next + 2 > buf.len() || (buf[next] == 0xFF && buf[next + 1] & 0xE0 == 0xE0) {
            return Framed::Frame { offset: i, header };
        }
        i += 1;
    }
    Framed::NoSync
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn header(bytes: [u8; 4]) -> Header {
        Header::parse(&bytes).expect("a header")
    }

    #[test]
    fn mpeg1_layer3_fields_and_length() {
        let h = header([0xFF, 0xFB, 0x90, 0x64]);
        assert_eq!((h.lsf, h.layer, h.crc, h.mono), (false, 3, false, false));
        assert_eq!((h.bitrate_kbps, h.sample_rate_hz), (Some(128), 44_100));
        assert_eq!((h.samples_per_frame(), h.frame_len()), (1152, Some(417)));
        let h = header([0xFF, 0xFA, 0x92, 0xC0]);
        assert_eq!((h.crc, h.padding, h.channel_count()), (true, true, 1));
        assert_eq!(h.frame_len(), Some(418));
    }

    #[test]
    fn lsf_and_mpeg25_layer3() {
        let h = header([0xFF, 0xF3, 0x80, 0x00]);
        assert_eq!((h.lsf, h.bitrate_kbps, h.sample_rate_hz), (true, Some(64), 22_050));
        assert_eq!((h.samples_per_frame(), h.frame_len()), (576, Some(72 * 64_000 / 22_050)));
        let h = header([0xFF, 0xE3, 0x18, 0x00]);
        assert_eq!((h.lsf, h.bitrate_kbps, h.sample_rate_hz), (true, Some(8), 8_000));
        assert_eq!(h.frame_len(), Some(72));
    }

    #[test]
    fn layers_1_and_2() {
        // MPEG-1 layer I, 448 kbit/s, 32 kHz, padded: (12 * 448000 / 32000 + 1) * 4.
        let h = header([0xFF, 0xFF, 0xEA, 0x00]);
        assert_eq!((h.layer, h.bitrate_kbps, h.samples_per_frame()), (1, Some(448), 384));
        assert_eq!(h.frame_len(), Some((12 * 448_000 / 32_000 + 1) * 4));
        let h = header([0xFF, 0xFD, 0xE4, 0x00]);
        assert_eq!((h.layer, h.bitrate_kbps, h.samples_per_frame()), (2, Some(384), 1152));
        assert_eq!(h.frame_len(), Some(144 * 384_000 / 48_000));
        let h = header([0xFF, 0xF5, 0xE0, 0x00]);
        assert_eq!((h.bitrate_kbps, h.samples_per_frame()), (Some(160), 1152));
        let h = header([0xFF, 0xF7, 0xE0, 0x00]);
        assert_eq!(h.bitrate_kbps, Some(256));
    }

    #[test]
    fn reserved_fields_and_free_format() {
        assert_eq!(Header::parse(&[0xFF, 0xEB, 0x90, 0x00]), None, "version 01");
        assert_eq!(Header::parse(&[0xFF, 0xF9, 0x90, 0x00]), None, "layer 00");
        assert_eq!(Header::parse(&[0xFF, 0xFB, 0xF0, 0x00]), None, "bitrate 1111");
        assert_eq!(Header::parse(&[0xFF, 0xFB, 0x9C, 0x00]), None, "rate 11");
        assert_eq!(Header::parse(&[0xFF, 0xDB, 0x90, 0x00]), None, "no sync");
        assert_eq!(Header::parse(&[0xFF, 0xFB, 0x90]), None, "short");
        let free = header([0xFF, 0xFB, 0x00, 0x00]);
        assert_eq!((free.bitrate_kbps, free.frame_len()), (None, None));
    }

    #[test]
    fn the_framer_skips_junk_waits_and_refuses_a_false_sync() {
        let frame = |n: usize| {
            let mut f = vec![0u8; n];
            f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            f
        };
        let mut stream = vec![0x00, 0xFF, 0xFB];
        stream.extend(frame(417));
        stream.extend(frame(417));
        match next_frame(&stream) {
            Framed::Frame { offset, header } => {
                assert_eq!((offset, header.frame_len()), (3, Some(417)))
            }
            _ => panic!("a frame behind the junk"),
        }
        assert!(matches!(next_frame(&stream[..300]), Framed::NeedMore { offset: 3 }));
        let mut lone = frame(417);
        lone.extend([0x12; 4]);
        assert!(matches!(next_frame(&lone), Framed::NoSync));
        assert!(matches!(next_frame(&[0x00; 64]), Framed::NoSync));
        assert!(matches!(next_frame(&frame(417)), Framed::Frame { offset: 0, .. }));
    }
}
