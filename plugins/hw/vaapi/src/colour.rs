//! The colour an H.264 or H.265 bitstream states: the VUI signal type (H.264 E.1.1, H.265 E.2.1)
//! and the HDR SEI, mastering display (137) and content light level (144), H.265 D.2.28 and D.2.35.

use profluens_video::color::{
    ChromaSite, ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, TransferFn,
};
use profluens_video::decoder::Color;

use crate::h264parse::BitReader;

const SEI_MASTERING_DISPLAY: u32 = 137;
const SEI_CONTENT_LIGHT: u32 = 144;

/// The part of the VUI H.264 E.1.1 and H.265 E.2.1 share. An absent signal type states nothing.
/// The reader is left at `timing_info_present_flag` (H.264) or `neutral_chroma_indication_flag`
/// (H.265).
pub fn vui_signal(r: &mut BitReader) -> Color {
    let mut c = Color::default();
    if r.flag() && r.u(8) == 255 {
        r.u(16);
        r.u(16);
    }
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
    c
}

/// Each message found replaces its part whole, it persists until the next of its kind (H.265
/// D.3.28, D.3.35).
pub fn sei_hdr(rbsp: &[u8], hdr: &mut HdrStatic) -> bool {
    let mut found = false;
    let mut at = 0;
    // sei_message() (H.265 7.3.5, H.264 7.3.2.3.1). A 0x80 where the next message would start ends
    // it.
    while at < rbsp.len() && rbsp[at] != 0x80 {
        let (Some(kind), Some(size)) = (sei_number(rbsp, &mut at), sei_number(rbsp, &mut at))
        else {
            break;
        };
        let Some(payload) = rbsp.get(at..at + size as usize) else {
            break;
        };
        at += size as usize;
        match kind {
            SEI_MASTERING_DISPLAY => {
                if let Some(m) = Mastering::from_st2086(payload) {
                    (hdr.mastering, found) = (m, true);
                }
            }
            SEI_CONTENT_LIGHT => {
                if let Some(l) = ContentLight::from_cta861(payload) {
                    (hdr.light, found) = (l, true);
                }
            }
            _ => {}
        }
    }
    found
}

pub struct HdrCarry {
    carry: pf_hdr::carry::Carry,
    side: Box<[u8]>,
    scratch: Box<[u8]>,
    pub dovi: Option<profluens_video::color::DoviConfig>,
}

impl HdrCarry {
    // Once per decoder.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> HdrCarry {
        HdrCarry {
            carry: pf_hdr::carry::Carry::new(),
            side: vec![0u8; pf_hdr::side::SIDE_CAP].into_boxed_slice(),
            scratch: vec![0u8; 8192].into_boxed_slice(),
            dovi: None,
        }
    }

    pub fn gather(
        &mut self,
        codec: pf_hdr::h26x::Codec,
        unit: &profluens_core::memory::Memory,
        pts: u64,
        hdr: &mut HdrStatic,
    ) -> bool {
        let mut scan = pf_hdr::h26x::Scan::new(pf_hdr::side::Writer::new(&mut self.side));
        pf_hdr::h26x::scan_annexb(codec, unit.data(), &mut self.scratch, &mut scan);
        settle(&mut self.carry, &mut self.dovi, scan, unit, pts, hdr)
    }

    pub fn gather_av1(
        &mut self,
        unit: &profluens_core::memory::Memory,
        pts: u64,
        hdr: &mut HdrStatic,
    ) -> bool {
        let mut scan = pf_hdr::h26x::Scan::new(pf_hdr::side::Writer::new(&mut self.side));
        pf_hdr::av1::scan(unit.data(), &mut scan);
        settle(&mut self.carry, &mut self.dovi, scan, unit, pts, hdr)
    }

    pub fn keep(&mut self, unit: &profluens_core::memory::Memory, pts: u64) {
        self.carry.put(pts, unit.side());
    }

    pub fn attach(&mut self, pts: u64, memory: &mut profluens_core::memory::Memory) {
        if let Some(side) = self.carry.take(pts).filter(|s| !s.is_empty()) {
            let _ = memory.set_side(side);
        }
    }

    pub fn clear(&mut self) {
        self.carry.clear();
    }
}

impl Default for HdrCarry {
    fn default() -> Self {
        HdrCarry::new()
    }
}

fn settle(
    carry: &mut pf_hdr::carry::Carry,
    dovi: &mut Option<profluens_video::color::DoviConfig>,
    mut scan: pf_hdr::h26x::Scan<'_>,
    unit: &profluens_core::memory::Memory,
    pts: u64,
    hdr: &mut HdrStatic,
) -> bool {
    let before = *hdr;
    *hdr = HdrStatic {
        mastering: scan.mastering.unwrap_or(hdr.mastering),
        light: scan.light.unwrap_or(hdr.light),
    };
    if dovi.is_none() {
        let meta = pf_hdr::side::FrameMeta::read(scan.side.bytes()).ok();
        let rpu = meta.and_then(|m| Some((m.dovi_rpu_header()?.ok()?, m.dovi_el())));
        *dovi = rpu.and_then(|(h, el)| profluens_video::color::DoviConfig::derived(&h, el));
    }
    scan.side.merge_under(unit.side());
    carry.put(pts, scan.side.bytes());
    *hdr != before
}

fn sei_number(rbsp: &[u8], at: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    loop {
        let b = *rbsp.get(*at)?;
        *at += 1;
        v = v.checked_add(u32::from(b))?;
        if b != 0xFF {
            return Some(v);
        }
    }
}

#[cfg(test)]
// Test fixtures.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    /// The SEI x265 writes for `master-display=G(13250,34500)B(7500,3000)R(34000,16000)
    /// WP(15635,16450)L(10000000,50)` and `max-cll=1000,400`, both in one NAL.
    fn hdr10_sei() -> Vec<u8> {
        let mut p = vec![137, 24];
        for v in [13250u16, 34500, 7500, 3000, 34000, 16000, 15635, 16450] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.extend_from_slice(&10_000_000u32.to_be_bytes());
        p.extend_from_slice(&50u32.to_be_bytes());
        p.extend_from_slice(&[144, 4, 0x03, 0xE8, 0x01, 0x90, 0x80]);
        p
    }

    #[test]
    fn hdr10_sei_states_the_display_in_rgb_order_and_the_light_level() {
        let mut hdr = HdrStatic::default();
        assert!(sei_hdr(&hdr10_sei(), &mut hdr));
        let m = hdr.mastering;
        assert_eq!(m.primaries, [[34000, 16000], [13250, 34500], [7500, 3000]]);
        assert_eq!(m.white, [15635, 16450]);
        assert_eq!((m.max_luminance, m.min_luminance), (10_000_000, 50));
        assert_eq!(hdr.light, ContentLight { max_cll: 1000, max_fall: 400 });
        assert_eq!(hdr.max_mastering_nits(), 1000.0);
    }

    #[test]
    fn other_messages_and_a_cut_payload_leave_it_alone() {
        let mut hdr = HdrStatic::default();
        let mut p = vec![5, 0xFF, 45];
        p.extend(std::iter::repeat_n(0xAB, 300));
        p.push(0x80);
        assert!(!sei_hdr(&p, &mut hdr));
        assert!(hdr.is_empty());
        let cut = &hdr10_sei()[..20];
        assert!(!sei_hdr(cut, &mut hdr));
        assert!(hdr.is_empty());
    }

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
        assert_eq!(vui_signal(&mut BitReader::new(&[0x00])), Color::default());
    }
}
