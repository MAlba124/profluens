//! The little read of elementary streams, ADTS headers, the first Annex B slice or MPEG picture
//! header, MPEG audio layer.

/// ISO/IEC 14496-3 Table 1.18.
const RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

pub const ADTS_FRAME_MAX: usize = (1 << 13) - 1;

/// ISO/IEC 14496-3 1.A.2.2, same layout as ISO/IEC 13818-7 6.2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Adts {
    /// The audio object type, the profile field plus one.
    pub object: u8,
    pub rate_index: u8,
    /// Zero when the channels are in a program config element.
    pub channels: u8,
    pub frame_len: usize,
    pub header_len: usize,
    pub blocks: u8,
}

impl Adts {
    pub fn parse(data: &[u8]) -> Option<Adts> {
        let h = data.get(..7)?;
        if h[0] != 0xFF || h[1] & 0xF6 != 0xF0 {
            return None;
        }
        let rate_index = (h[2] >> 2) & 0x0F;
        let frame_len =
            (usize::from(h[3] & 0x03) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        let header_len = if h[1] & 0x01 != 0 { 7 } else { 9 };
        if usize::from(rate_index) >= RATES.len() || frame_len < header_len {
            return None;
        }
        Some(Adts {
            object: (h[2] >> 6) + 1,
            rate_index,
            channels: ((h[2] & 0x01) << 2) | (h[3] >> 6),
            frame_len,
            header_len,
            blocks: (h[6] & 0x03) + 1,
        })
    }

    pub fn rate(&self) -> u32 {
        RATES[usize::from(self.rate_index)]
    }

    /// ISO/IEC 14496-3 1.6.2.1, object type, rate index, channels, a zero GASpecificConfig.
    pub fn config(&self) -> [u8; 2] {
        [
            (self.object << 3) | (self.rate_index >> 1),
            ((self.rate_index & 1) << 7) | (self.channels << 3),
        ]
    }

    pub fn key(&self) -> (u8, u8, u8) {
        (self.object, self.rate_index, self.channels)
    }
}

/// H.264 IDR (type 5) or H.265 IRAP (16 to 23), Table 7-1 of each. `None` without a slice.
pub fn starts_with_key(unit: &[u8], hevc: bool) -> Option<bool> {
    let mut at = 0usize;
    while let Some(found) =
        unit.get(at..).and_then(|rest| rest.windows(3).position(|w| w == [0, 0, 1]))
    {
        let header = *unit.get(at + found + 3)?;
        at += found + 3;
        match hevc {
            false => match header & 0x1F {
                5 => return Some(true),
                1..=4 => return Some(false),
                _ => {}
            },
            true => match (header >> 1) & 0x3F {
                16..=23 => return Some(true),
                0..=31 => return Some(false),
                _ => {}
            },
        }
    }
    None
}

/// Whether the first picture of a unit is intra coded, `None` without a picture header. MPEG-1 and
/// MPEG-2 picture_coding_type 1 (ISO/IEC 13818-2 6.3.9 and Table 6-12), MPEG-4 Part 2
/// vop_coding_type 0 (ISO/IEC 14496-2 6.3.5). Headers ahead of it hold no start code emulation.
pub fn starts_intra(unit: &[u8], mpeg4: bool) -> Option<bool> {
    let mut at = 0usize;
    while let Some(found) =
        unit.get(at..).and_then(|rest| rest.windows(3).position(|w| w == [0, 0, 1]))
    {
        at += found + 3;
        match (mpeg4, *unit.get(at)?) {
            // Ten bits of temporal_reference, then the three of the type.
            (false, 0x00) => return Some((unit.get(at + 2)? >> 3) & 7 == 1),
            (true, 0xB6) => return Some(unit.get(at + 1)? >> 6 == 0),
            _ => {}
        }
    }
    None
}

/// Whether a unit's sequence header is MPEG-1's, `None` without one or the start code behind it.
/// MPEG-2 follows it with a sequence extension (ISO/IEC 13818-2 6.2.2, Table 6-2 id 1), MPEG-1
/// never does. Stream type 0x02 carries either (H.222.0 Table 2-34).
pub fn sequence_is_mpeg1(unit: &[u8]) -> Option<bool> {
    let codes = |from: usize| {
        unit.get(from..)?.windows(3).position(|w| w == [0, 0, 1]).map(|at| from + at + 3)
    };
    let mut at = 0usize;
    loop {
        at = codes(at)?;
        if *unit.get(at)? == 0xB3 {
            break;
        }
    }
    let next = codes(at + 1)?;
    Some(!(*unit.get(next)? == 0xB5 && unit.get(next + 1)? >> 4 == 1))
}

/// 1 to 3, ISO/IEC 11172-3 2.4.1.3.
pub fn mpeg_audio_layer(data: &[u8]) -> Option<u8> {
    let h = data.get(..2)?;
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    match (h[1] >> 1) & 0x03 {
        0 => None,
        bits => Some(4 - bits),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test fixtures
mod tests {
    use super::*;

    #[test]
    fn an_adts_header_says_what_a_decoder_is_configured_by() {
        let frame = [0xFF, 0xF1, 0x50, 0x80, 0x01, 0x3F, 0xFC, 0xDE, 0xAD];
        let h = Adts::parse(&frame).expect("a header");
        assert_eq!((h.object, h.rate(), h.channels), (2, 44_100, 2));
        assert_eq!((h.frame_len, h.header_len, h.blocks), (9, 7, 1));
        assert_eq!(h.config(), [0x12, 0x10]);
        assert_eq!(Adts::parse(&[0xFF, 0xFB, 0x50, 0x80, 0x01, 0x3F, 0xFC]), None);
        assert_eq!(Adts::parse(&frame[..6]), None);
    }

    #[test]
    fn the_first_slice_says_whether_a_decoder_can_start() {
        let sps_then_idr = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x65, 9];
        let delta = [0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x41, 9];
        assert_eq!(starts_with_key(&sps_then_idr, false), Some(true));
        assert_eq!(starts_with_key(&delta, false), Some(false));
        assert_eq!(starts_with_key(&[0, 0, 1, 0x09, 0xF0], false), None);
        assert_eq!(starts_with_key(&[0, 0, 1, 0x40, 1, 0, 0, 1, 0x26, 1], true), Some(true));
        assert_eq!(starts_with_key(&[0, 0, 1, 0x02, 1], true), Some(false));
    }

    #[test]
    fn the_first_picture_header_says_whether_a_decoder_can_start() {
        // Sequence header, GOP header, then an I, a P and a B picture header.
        let i =
            [0, 0, 1, 0xB3, 8, 6, 0, 0x13, 0, 0, 1, 0xB8, 0, 8, 0, 0, 0, 0, 1, 0, 0, 0x0F, 0xFF];
        assert_eq!(starts_intra(&i, false), Some(true));
        assert_eq!(starts_intra(&[0, 0, 1, 0, 0x01, 0x10, 0xFF], false), Some(false));
        assert_eq!(starts_intra(&[0, 0, 1, 0, 0x01, 0x18, 0xFF], false), Some(false));
        assert_eq!(starts_intra(&[0, 0, 1, 0xB3, 8, 6, 0, 0x13], false), None);
        assert_eq!(starts_intra(&[0, 0, 1, 0, 0x01], false), None, "cut short");
        // VOL ahead of an I-VOP, then a P-VOP and a B-VOP.
        let vol_i = [0, 0, 1, 0x20, 0, 0xC4, 0, 0, 1, 0xB6, 0x10, 0x60];
        assert_eq!(starts_intra(&vol_i, true), Some(true));
        assert_eq!(starts_intra(&[0, 0, 1, 0xB6, 0x50], true), Some(false));
        assert_eq!(starts_intra(&[0, 0, 1, 0xB6, 0x90], true), Some(false));
        assert_eq!(starts_intra(&i, true), None, "no VOP");
    }

    #[test]
    fn a_sequence_extension_tells_mpeg2_from_mpeg1() {
        let seq = [0, 0, 1, 0xB3, 0x14, 0, 0xF0, 0x13, 0xFF, 0xFF, 0xE0, 0x18];
        let ext = [0, 0, 1, 0xB5, 0x14, 0x8A];
        let gop = [0, 0, 1, 0xB8, 0, 8];
        assert_eq!(sequence_is_mpeg1(&[&seq[..], &ext].concat()), Some(false));
        assert_eq!(sequence_is_mpeg1(&[&gop[..], &seq, &gop].concat()), Some(true));
        assert_eq!(sequence_is_mpeg1(&seq), None, "cut");
        assert_eq!(sequence_is_mpeg1(&gop), None, "no sequence header");
        assert_eq!(sequence_is_mpeg1(&[&seq[..], &ext[..4]].concat()), None, "cut in the code");
    }

    #[test]
    fn an_mpeg_audio_frame_says_its_layer() {
        assert_eq!(mpeg_audio_layer(&[0xFF, 0xFB]), Some(3));
        assert_eq!(mpeg_audio_layer(&[0xFF, 0xFD]), Some(2));
        assert_eq!(mpeg_audio_layer(&[0xFF, 0xFF]), Some(1));
        assert_eq!(mpeg_audio_layer(&[0xFF, 0xF1]), None);
    }
}
