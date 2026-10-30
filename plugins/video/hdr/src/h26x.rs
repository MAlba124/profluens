//! HDR SEI messages and Dolby Vision NAL units of H.264 and H.265 access units.

use crate::bits::unescape_into;
use crate::color::{ContentLight, Mastering};
use crate::side::{Kind, Writer};
use crate::t35::{T35Kind, classify};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    H264,
    H265,
}

pub const SEI_T35: u32 = 4;
pub const SEI_MASTERING_DISPLAY: u32 = 137;
pub const SEI_CONTENT_LIGHT: u32 = 144;

pub const H265_PREFIX_SEI: u8 = 39;
pub const H265_SUFFIX_SEI: u8 = 40;
pub const H265_DOVI_RPU: u8 = 62;
pub const H265_DOVI_EL: u8 = 63;
pub const H264_SEI: u8 = 6;

pub struct Scan<'a> {
    pub mastering: Option<Mastering>,
    pub light: Option<ContentLight>,
    pub side: Writer<'a>,
}

impl<'a> Scan<'a> {
    pub fn new(side: Writer<'a>) -> Scan<'a> {
        Scan { mastering: None, light: None, side }
    }
}

/// The `sei_message()`s of one SEI RBSP, ending at the trailing bits or a truncated message.
pub fn sei_messages(rbsp: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= rbsp.len() || (rbsp[at] == 0x80 && at + 1 == rbsp.len()) {
            return None;
        }
        let kind = sei_number(rbsp, &mut at)?;
        let size = sei_number(rbsp, &mut at)? as usize;
        let payload = rbsp.get(at..at.checked_add(size)?)?;
        at += size;
        Some((kind, payload))
    })
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

pub fn sei(rbsp: &[u8], scan: &mut Scan<'_>) {
    for (kind, payload) in sei_messages(rbsp) {
        match kind {
            SEI_MASTERING_DISPLAY => {
                if let Some(m) = Mastering::from_st2086(payload) {
                    scan.mastering = Some(m);
                }
            }
            SEI_CONTENT_LIGHT => {
                if let Some(l) = ContentLight::from_cta861(payload) {
                    scan.light = Some(l);
                }
            }
            SEI_T35 if !matches!(classify(payload), T35Kind::Other) => {
                scan.side.push(Kind::T35, payload);
            }
            _ => {}
        }
    }
}

/// The NAL units of an Annex B access unit (Annex B.2), trailing zero bytes off each.
pub fn annexb_nals(unit: &[u8]) -> impl Iterator<Item = &[u8]> {
    let start = |d: &[u8]| d.windows(3).position(|w| w == [0, 0, 1]);
    let mut rest = start(unit).map_or(&[][..], |at| &unit[at + 3..]);
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (nal, next) = match start(rest) {
            Some(end) => (&rest[..end], &rest[end + 3..]),
            None => (rest, &[][..]),
        };
        rest = next;
        let trimmed = nal.iter().rposition(|&b| b != 0).map_or(&[][..], |last| &nal[..=last]);
        Some(trimmed)
    })
}

pub fn scan_annexb(codec: Codec, unit: &[u8], scratch: &mut [u8], scan: &mut Scan<'_>) {
    let mut rpu: Option<&[u8]> = None;
    let mut el = false;
    for nal in annexb_nals(unit) {
        let (sei_body, header) = match codec {
            Codec::H264 => match nal.first() {
                Some(b) if b & 0x1F == H264_SEI => (true, 1),
                _ => (false, 1),
            },
            Codec::H265 => {
                let [b0, b1, ..] = *nal else { continue };
                let kind = b0 >> 1 & 0x3F;
                let layer = (b0 & 1) << 5 | b1 >> 3;
                let tid_plus1 = b1 & 7;
                match kind {
                    H265_PREFIX_SEI | H265_SUFFIX_SEI => (true, 2),
                    H265_DOVI_RPU if layer == 0 && tid_plus1 == 1 && nal.len() > 2 => {
                        rpu = Some(&nal[2..]);
                        (false, 2)
                    }
                    H265_DOVI_EL => {
                        el = true;
                        (false, 2)
                    }
                    _ => (false, 2),
                }
            }
        };
        if sei_body && nal.len() > header {
            let n = unescape_into(&nal[header..], scratch);
            sei(&scratch[..n], scan);
        }
    }
    if let Some(rpu) = rpu {
        scan.side.push(Kind::DoviRpu, rpu);
    }
    if el {
        scan.side.push(Kind::DoviEl, &[]);
    }
}

/// The VUI video signal type and chroma location (H.264 E.1.1, H.265 E.2.1).
pub fn vui_colour(b: &mut crate::bits::Bits<'_>) -> Option<crate::color::StreamColor> {
    use crate::color::{ChromaSite, Matrix, Primaries, Range, StreamColor, TransferFn};
    let mut c = StreamColor::default();
    if b.flag()? && b.u(8)? == 255 {
        b.u(16)?;
        b.u(16)?;
    }
    if b.flag()? {
        b.flag()?; // overscan_appropriate_flag
    }
    if b.flag()? {
        b.u(3)?; // video_format
        c.range = Some(Range::from_flag(b.flag()?));
        if b.flag()? {
            c.primaries = Primaries::from_h273(b.u(8)? as u8);
            c.transfer = TransferFn::from_h273(b.u(8)? as u8);
            c.matrix = Matrix::from_h273(b.u(8)? as u8);
        }
    }
    if b.flag()? {
        c.chroma = ChromaSite::from_h26x(b.ue()?);
        b.ue()?;
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::side::FrameMeta;

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

    fn hdr10plus_t35() -> Vec<u8> {
        let mut m = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
        m.extend(crate::hdr10plus::sample_payload([60_000, 50_000, 40_000], 1000));
        m
    }

    fn sei_message(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        let mut n = payload.len();
        while n >= 255 {
            out.push(0xFF);
            n -= 255;
        }
        out.push(n as u8);
        out.extend_from_slice(payload);
        out
    }

    fn escape(rbsp: &[u8]) -> Vec<u8> {
        let (mut out, mut zeros) = (Vec::new(), 0);
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    fn unit(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn static_sei_reads_in_st2086_order() {
        let mut side = [0u8; 64];
        let mut scan = Scan::new(Writer::new(&mut side));
        sei(&hdr10_sei(), &mut scan);
        let m = scan.mastering.unwrap();
        assert_eq!(m.primaries, [[34000, 16000], [13250, 34500], [7500, 3000]], "red, green, blue");
        assert_eq!((m.white, m.max_luminance, m.min_luminance), ([15635, 16450], 10_000_000, 50));
        assert_eq!(scan.light, Some(ContentLight { max_cll: 1000, max_fall: 400 }));
        assert!(scan.side.is_empty());
    }

    #[test]
    fn an_h265_unit_gives_static_sei_hdr10plus_and_the_dovi_units() {
        let mut prefix = vec![H265_PREFIX_SEI << 1, 1];
        prefix.extend(escape(&hdr10_sei()));
        let mut suffix = vec![H265_SUFFIX_SEI << 1, 1];
        let mut body = sei_message(4, &hdr10plus_t35());
        body.push(0x80);
        suffix.extend(escape(&body));
        let rpu = vec![H265_DOVI_RPU << 1, 1, 0x19, 0x08, 0x09, 0x00, 0x00, 0x03, 0x01, 0x80];
        let el = vec![H265_DOVI_EL << 1, 1, 0x01, 0x02];
        let slice = vec![1 << 1, 1, 0xAF, 0x00];
        let u = unit(&[prefix, slice, rpu, el, suffix]);
        let (mut side, mut scratch) = ([0u8; 1024], [0u8; 1024]);
        let mut scan = Scan::new(Writer::new(&mut side));
        scan_annexb(Codec::H265, &u, &mut scratch, &mut scan);
        assert_eq!(scan.light.unwrap().max_cll, 1000);
        let n = scan.side.len();
        let m = FrameMeta::read(&side[..n]).unwrap();
        assert_eq!(m.hdr10plus().unwrap().unwrap().windows[0].maxscl[0], 60_000);
        let (form, bytes) = m.dovi_rpu().unwrap();
        assert_eq!(form, crate::dovi::RpuForm::Nal);
        assert_eq!(bytes, &[0x19, 0x08, 0x09, 0x00, 0x00, 0x03, 0x01, 0x80], "escaped, as carried");
        assert!(m.dovi_el());
    }

    #[test]
    fn an_rpu_off_layer_zero_or_a_higher_temporal_layer_is_not_the_frames() {
        let (mut side, mut scratch) = ([0u8; 64], [0u8; 64]);
        for header in [[H265_DOVI_RPU << 1 | 1, 1], [H265_DOVI_RPU << 1, 2]] {
            let mut nal = header.to_vec();
            nal.extend_from_slice(&[0x19, 1, 2, 3]);
            let mut scan = Scan::new(Writer::new(&mut side));
            scan_annexb(Codec::H265, &unit(&[nal]), &mut scratch, &mut scan);
            assert!(scan.side.is_empty(), "{header:?}");
        }
    }

    #[test]
    fn an_h264_unit_reads_its_sei_and_ignores_nal_62() {
        let mut sei_nal = vec![0x06];
        let mut body = sei_message(4, &hdr10plus_t35());
        body.extend(sei_message(144, &[0x00, 0x64, 0x00, 0x32]));
        body.push(0x80);
        sei_nal.extend(escape(&body));
        let (mut side, mut scratch) = ([0u8; 1024], [0u8; 1024]);
        let mut scan = Scan::new(Writer::new(&mut side));
        scan_annexb(
            Codec::H264,
            &unit(&[vec![0x09, 0x10], sei_nal, vec![0x65, 0x88]]),
            &mut scratch,
            &mut scan,
        );
        assert_eq!(scan.light, Some(ContentLight { max_cll: 100, max_fall: 50 }));
        let n = scan.side.len();
        assert!(FrameMeta::read(&side[..n]).unwrap().hdr10plus_payload().is_some());
    }

    #[test]
    fn other_t35_and_a_cut_message_are_passed_over() {
        let a53 = [0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03];
        let mut body = sei_message(4, &a53);
        body.extend_from_slice(&[137, 24, 0, 0]);
        let mut side = [0u8; 64];
        let mut scan = Scan::new(Writer::new(&mut side));
        sei(&body, &mut scan);
        assert!(scan.side.is_empty());
        assert_eq!(scan.mastering, None);
        assert_eq!(
            sei_messages(&sei_message(5, &[0; 300])).next().map(|(k, p)| (k, p.len())),
            Some((5, 300))
        );
    }

    #[test]
    fn the_vui_signal_type_and_chroma_location() {
        use crate::bits::{BitWriter, Bits};
        use crate::color::{ChromaSite, Matrix, Primaries, Range, TransferFn};
        let mut w = BitWriter::default();
        w.put(1, 1).put(255, 8).put(4, 16).put(3, 16); // aspect, extended SAR
        w.put(0, 1); // overscan_info_present_flag
        w.put(1, 1).put(5, 3).put(0, 1).put(1, 1).put(9, 8).put(16, 8).put(9, 8);
        w.put(1, 1).ue(2).ue(2);
        w.put(1, 8);
        let bytes = w.into_bytes();
        let c = vui_colour(&mut Bits::new(&bytes)).unwrap();
        assert_eq!(
            (c.primaries, c.transfer, c.matrix),
            (Some(Primaries::Bt2020), Some(TransferFn::Pq), Some(Matrix::Bt2020))
        );
        assert_eq!(c.range, Some(Range::Limited));
        assert_eq!(c.chroma, Some(ChromaSite::TopLeft));
    }
}
