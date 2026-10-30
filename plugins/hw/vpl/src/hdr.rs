//! HDR SEI and OBU metadata the runtime does not hand back, carried by pts to the picture.

use pf_hdr::h26x::{Codec, Scan, scan_annexb};
use pf_hdr::side::{SIDE_CAP, Writer};
use profluens_core::memory::Memory;
use profluens_video::color::HdrStatic;

pub struct HdrCarry {
    carry: pf_hdr::carry::Carry,
    side: Box<[u8]>,
    scratch: Box<[u8]>,
}

impl HdrCarry {
    // Once per decoder.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> HdrCarry {
        HdrCarry {
            carry: pf_hdr::carry::Carry::new(),
            side: vec![0u8; SIDE_CAP].into_boxed_slice(),
            scratch: vec![0u8; 8192].into_boxed_slice(),
        }
    }

    /// `true` when `hdr` changed.
    pub fn gather(&mut self, codec: Codec, unit: &Memory, pts: u64, hdr: &mut HdrStatic) -> bool {
        let mut scan = Scan::new(Writer::new(&mut self.side));
        scan_annexb(codec, unit.data(), &mut self.scratch, &mut scan);
        settle(&mut self.carry, scan, unit, pts, hdr)
    }

    pub fn gather_av1(&mut self, unit: &Memory, pts: u64, hdr: &mut HdrStatic) -> bool {
        let mut scan = Scan::new(Writer::new(&mut self.side));
        pf_hdr::av1::scan(unit.data(), &mut scan);
        settle(&mut self.carry, scan, unit, pts, hdr)
    }

    /// For a codec without in-band HDR.
    pub fn keep(&mut self, unit: &Memory, pts: u64) {
        self.carry.put(pts, unit.side());
    }

    pub fn attach(&mut self, pts: u64, memory: &mut Memory) {
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
    mut scan: Scan<'_>,
    unit: &Memory,
    pts: u64,
    hdr: &mut HdrStatic,
) -> bool {
    let before = *hdr;
    *hdr = HdrStatic {
        mastering: scan.mastering.unwrap_or(hdr.mastering),
        light: scan.light.unwrap_or(hdr.light),
    };
    scan.side.merge_under(unit.side());
    carry.put(pts, scan.side.bytes());
    *hdr != before
}
