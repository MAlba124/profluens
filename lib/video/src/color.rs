//! The colour vocabulary, re-exported from [`pf_hdr::color`].

pub use pf_hdr::color::*;
pub use pf_hdr::dovi::DoviConfig;

use crate::bits::BitReader;

/// Table E-1's sample aspect ratios, `aspect_ratio_idc` 1 to 16.
const SAR: [(u16, u16); 16] = [
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
];

/// The part of the VUI H.264 E.1.1 and H.265 E.2.1 share. An absent signal type states nothing.
/// The reader is left at `timing_info_present_flag` (H.264) or `neutral_chroma_indication_flag`
/// (H.265).
pub fn vui_signal(r: &mut BitReader) -> StreamColor {
    vui_head(r).1
}

/// As [`vui_signal`], with the sample aspect ratio first, 0 to 0 where none or an unknown one
/// is stated.
pub fn vui_head(r: &mut BitReader) -> ((u16, u16), StreamColor) {
    let mut sar = (0, 0);
    if r.flag() {
        sar = match r.u(8) {
            255 => (r.u(16) as u16, r.u(16) as u16),
            idc @ 1..=16 => SAR[idc as usize - 1],
            _ => (0, 0),
        };
        if sar.0 == 0 || sar.1 == 0 {
            sar = (0, 0);
        }
    }
    let mut c = StreamColor::default();
    if r.flag() {
        r.flag(); // overscan_appropriate_flag
    }
    if r.flag() {
        r.u(3); // video_format
        c.range = Some(if r.flag() { Range::Full } else { Range::Limited });
        if r.flag() {
            let cp = |v: u32| v as u8;
            c.primaries = Primaries::from_h273(cp(r.u(8)));
            c.transfer = TransferFn::from_h273(cp(r.u(8)));
            c.matrix = Matrix::from_h273(cp(r.u(8)));
        }
    }
    if r.flag() {
        c.chroma = ChromaSite::from_h26x(r.ue()); // chroma_sample_loc_type_top_field
        r.ue(); // chroma_sample_loc_type_bottom_field
    }
    (sar, c)
}

#[cfg(test)]
// Test fixtures.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn the_vui_signal_type_states_range_and_code_points() {
        // aspect 1 + 255 + 16 + 16, overscan 0, signal 1 + format 5 + full 0 + desc 1 +
        // 9, 16, 9, chroma loc 1 + ue 0 + ue 0.
        let mut bits = String::new();
        bits += "1";
        bits += &format!("{:08b}{:016b}{:016b}", 255, 1, 1);
        bits += "0";
        bits += "1";
        bits += "101";
        bits += "0";
        bits += "1";
        bits += &format!("{:08b}{:08b}{:08b}", 9, 16, 9);
        bits += "1";
        bits += "1";
        bits += "1";
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let bytes: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect();
        let c = vui_signal(&mut BitReader::new(&bytes));
        assert_eq!(c.range, Some(Range::Limited));
        assert_eq!(c.primaries, Some(Primaries::Bt2020));
        assert_eq!(c.transfer, Some(TransferFn::Pq));
        assert_eq!(c.matrix, Some(Matrix::Bt2020));
        assert_eq!(vui_signal(&mut BitReader::new(&[0x00])), StreamColor::default());
        assert_eq!(vui_head(&mut BitReader::new(&bytes)).0, (1, 1));
    }

    #[test]
    fn the_vui_states_a_table_or_extended_sample_aspect() {
        // idc 4 is 16:11, a DVD's 16:9 PAL picture as H.264 names it
        let (sar, _) = vui_head(&mut BitReader::new(&[0b1000_0010, 0b0000_0000]));
        assert_eq!(sar, (16, 11));
        // idc 255 with 32:27, an NTSC 16:9 DVD
        let mut bits = format!("1{:08b}{:016b}{:016b}0000", 255, 32, 27);
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let bytes: Vec<u8> = bits
            .as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect();
        assert_eq!(vui_head(&mut BitReader::new(&bytes)).0, (32, 27));
        // a reserved idc and an extended 0 state none
        assert_eq!(vui_head(&mut BitReader::new(&[0b1000_1000, 0b1000_0000])).0, (0, 0));
        let zero = [0b1111_1111, 0b1000_0000, 0, 0, 0, 0];
        assert_eq!(vui_head(&mut BitReader::new(&zero)).0, (0, 0));
        assert_eq!(vui_head(&mut BitReader::new(&[0x00])).0, (0, 0));
    }
}
