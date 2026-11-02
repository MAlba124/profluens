//! The colour an H.264 or H.265 bitstream states: the VUI signal type (H.264 E.1.1, H.265 E.2.1)
//! and the HDR SEI, mastering display (137) and content light level (144), H.265 D.2.28 and D.2.35.

pub use profluens_video::color::vui_signal;
use profluens_video::color::{
    ContentLight, HdrStatic, Mastering, Matrix, Primaries, Range, StreamColor, TransferFn,
};

const SEI_MASTERING_DISPLAY: u32 = 137;
const SEI_CONTENT_LIGHT: u32 = 144;

/// What an encoder's VUI states of `c`, the full range flag and the primaries, transfer and
/// matrix as H.273, 2 where unstated. None when `c` states none of the three.
pub fn vui_colour(c: &StreamColor) -> Option<(bool, u8, u8, u8)> {
    if c.primaries.is_none() && c.transfer.is_none() && c.matrix.is_none() {
        return None;
    }
    Some((
        c.range == Some(Range::Full),
        c.primaries.map_or(2, Primaries::to_h273),
        c.transfer.map_or(2, TransferFn::to_h273),
        c.matrix.map_or(2, Matrix::to_h273),
    ))
}

/// The SEI RBSP of `h`'s mastering display (137) and content light level (144), the part each
/// states, trailing bits included (H.265 D.2.28, D.2.35). None when it states neither.
// Cold: once per encoder session.
#[allow(clippy::disallowed_methods)]
pub fn hdr_sei_rbsp(h: &HdrStatic) -> Option<Vec<u8>> {
    let (m, l) = (h.mastering, h.light);
    let (mastering, light) = (m != Mastering::default(), l != ContentLight::default());
    if !mastering && !light {
        return None;
    }
    let mut p = Vec::with_capacity(36);
    if mastering {
        p.extend_from_slice(&[SEI_MASTERING_DISPLAY as u8, 24]);
        // the SEI states green, blue, red
        let [r, g, b] = m.primaries;
        for v in [g, b, r, m.white] {
            p.extend_from_slice(&v[0].to_be_bytes());
            p.extend_from_slice(&v[1].to_be_bytes());
        }
        p.extend_from_slice(&m.max_luminance.to_be_bytes());
        p.extend_from_slice(&m.min_luminance.to_be_bytes());
    }
    if light {
        p.extend_from_slice(&[SEI_CONTENT_LIGHT as u8, 4]);
        p.extend_from_slice(&l.max_cll.to_be_bytes());
        p.extend_from_slice(&l.max_fall.to_be_bytes());
    }
    p.push(0x80);
    Some(p)
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
    fn the_written_sei_reads_back_as_it_was() {
        let mut hdr = HdrStatic::default();
        assert!(sei_hdr(&hdr10_sei(), &mut hdr));
        assert_eq!(hdr_sei_rbsp(&hdr).as_deref(), Some(&hdr10_sei()[..]), "x265's bytes");
        let light =
            HdrStatic { light: ContentLight { max_cll: 600, max_fall: 0 }, ..HdrStatic::default() };
        let mut back = HdrStatic::default();
        assert!(sei_hdr(&hdr_sei_rbsp(&light).expect("a light level"), &mut back));
        assert_eq!(back, light, "the light level alone");
        assert_eq!(hdr_sei_rbsp(&HdrStatic::default()), None);
    }

    #[test]
    fn the_vui_states_what_is_known_and_unspecified_for_the_rest() {
        let pq = StreamColor {
            transfer: Some(TransferFn::Pq),
            primaries: Some(Primaries::Bt2020),
            ..StreamColor::default()
        };
        assert_eq!(vui_colour(&pq), Some((false, 9, 16, 2)));
        let full = StreamColor {
            matrix: Some(Matrix::Bt709),
            range: Some(Range::Full),
            ..StreamColor::default()
        };
        assert_eq!(vui_colour(&full), Some((true, 2, 2, 1)));
        assert_eq!(
            vui_colour(&StreamColor { range: Some(Range::Full), ..StreamColor::default() }),
            None
        );
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
}
