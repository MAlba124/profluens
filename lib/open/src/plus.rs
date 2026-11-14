//! Whether a video track's first key unit carries an HDR10+ (ST 2094-40) T.35 message, in an
//! H.264 or H.265 SEI, an AV1 metadata OBU or a Matroska BlockAdditional.

use pf_hdr::h26x::{Codec, Scan, annexb_nals, scan_nals};
use pf_hdr::side::{FrameMeta, SIDE_CAP, Writer};
use pf_hdr::t35::{T35Kind, classify};
use profluens_video::prefixed;

/// The most of a key unit read, a longer one is scanned on its lead. HDR10+ leads a unit, an SEI
/// before its slices, a metadata OBU before its frame.
pub(crate) const KEY_READ: usize = 64 << 10;
/// The lead of a Matroska block scanned, a read starts at its element head and holds it.
pub(crate) const LEAD: usize = KEY_READ / 2;
/// SEI bytes unescaped at once, an HDR10+ message is under 1 KiB but may follow an encoder's
/// options string in the same NAL.
const SEI_READ: usize = 16 << 10;

/// An MP4 first key unit to read, `len` bytes from `at`, of the track at `track` in the
/// description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) track: usize,
    pub(crate) at: u64,
    pub(crate) len: usize,
}

/// Whether a stored unit of a `family` track carries an HDR10+ message, its NAL units led by
/// `nal_length` octet lengths, Annex B at 0. Families without in-band HDR10+ carry none.
pub(crate) fn in_unit(family: &str, nal_length: u8, unit: &[u8]) -> bool {
    let codec = match family {
        "h264/annexb" => Codec::H264,
        "h265/annexb" => Codec::H265,
        "av1" => {
            let mut side = [0u8; SIDE_CAP];
            let mut scan = Scan::new(Writer::new(&mut side));
            pf_hdr::av1::scan(unit, &mut scan);
            return has_plus(scan.side.bytes());
        }
        _ => return false,
    };
    let (mut side, mut scratch) = ([0u8; SIDE_CAP], [0u8; SEI_READ]);
    let mut scan = Scan::new(Writer::new(&mut side));
    match nal_length {
        0 => scan_nals(codec, annexb_nals(unit), &mut scratch, &mut scan),
        n => prefixed::for_each(unit, n, |from, to| {
            scan_nals(codec, std::iter::once(&unit[from..to]), &mut scratch, &mut scan);
        }),
    }
    has_plus(scan.side.bytes())
}

/// [`crate::Hdr10Plus::Stream`] when [`in_unit`] finds HDR10+, else none.
pub(crate) fn stream(family: &str, nal_length: u8, unit: &[u8]) -> crate::Hdr10Plus {
    match in_unit(family, nal_length, unit) {
        true => crate::Hdr10Plus::Stream,
        false => crate::Hdr10Plus::None,
    }
}

/// Whether a T.35 message from its country code on, a Matroska BlockAdditional, is HDR10+.
pub(crate) fn is_plus(msg: &[u8]) -> bool {
    matches!(classify(msg), T35Kind::Hdr10Plus { .. })
}

fn has_plus(side: &[u8]) -> bool {
    FrameMeta::read(side).is_ok_and(|m| m.hdr10plus_payload().is_some())
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
pub(crate) mod tests {
    use super::*;

    /// A T.35 HDR10+ message, the US country code, Samsung's provider codes and a payload whose
    /// zeros take emulation prevention bytes in an SEI.
    pub(crate) const PLUS_MSG: [u8; 12] =
        [0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x01, 0x40, 0x00, 0x00, 0x00, 0x01];
    /// A Dolby Vision RPU T.35 message.
    pub(crate) const RPU_MSG: [u8; 9] = [0xB5, 0x00, 0x3B, 0x00, 0x00, 0x08, 0x00, 0x37, 0xCD];

    /// An SEI NAL unit with one `user_data_registered_itu_t_t35` message, escaped.
    pub(crate) fn sei_nal(h265: bool, msg: &[u8]) -> Vec<u8> {
        let mut rbsp = vec![4, msg.len() as u8];
        rbsp.extend_from_slice(msg);
        rbsp.push(0x80);
        let mut nal = match h265 {
            true => vec![39 << 1, 0x01],
            false => vec![0x06],
        };
        profluens_video::bits::push_ebsp(&rbsp, &mut nal);
        nal
    }

    /// A slice NAL unit's header and filler.
    pub(crate) fn slice_nal(h265: bool) -> Vec<u8> {
        let mut nal = match h265 {
            true => vec![19 << 1, 0x01],
            false => vec![0x65],
        };
        nal.extend_from_slice(&[0x88, 0x84, 0x21, 0x43]);
        nal
    }

    pub(crate) fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
    }

    pub(crate) fn prefixed(nals: &[&[u8]], size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&(n.len() as u32).to_be_bytes()[4 - size..]);
            out.extend_from_slice(n);
        }
        out
    }

    /// An AV1 OBU with its size field.
    pub(crate) fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind << 3 | 0x02, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    /// An AV1 temporal unit, a temporal delimiter, a T.35 metadata OBU of `msg` and a frame.
    pub(crate) fn av1_tu(msg: &[u8]) -> Vec<u8> {
        let mut meta = vec![4];
        meta.extend_from_slice(msg);
        meta.push(0x80);
        [obu(2, &[]), obu(5, &meta), obu(6, &[0x10, 0x20, 0x30])].concat()
    }

    #[test]
    fn an_h265_sei_is_found_in_either_framing() {
        let (sei, slice) = (sei_nal(true, &PLUS_MSG), slice_nal(true));
        // the payload's three zeros took an emulation prevention byte
        assert!(sei.windows(3).any(|w| w == [0, 0, 3]), "{sei:x?}");
        let nals = [&sei[..], &slice[..]];
        assert!(in_unit("h265/annexb", 0, &annexb(&nals)));
        for size in [1, 2, 4] {
            assert!(in_unit("h265/annexb", size as u8, &prefixed(&nals, size)), "{size}");
        }
        assert!(!in_unit("h265/annexb", 4, &prefixed(&[&slice], 4)), "a slice alone");
        assert!(!in_unit("h265/annexb", 0, &annexb(&[&slice])));
    }

    #[test]
    fn an_h264_sei_is_found() {
        let (sei, slice) = (sei_nal(false, &PLUS_MSG), slice_nal(false));
        assert!(in_unit("h264/annexb", 4, &prefixed(&[&sei, &slice], 4)));
        assert!(in_unit("h264/annexb", 0, &annexb(&[&sei, &slice])));
        // an H.265 SEI header reads as another unit type in H.264
        assert!(!in_unit("h264/annexb", 0, &annexb(&[&sei_nal(true, &PLUS_MSG)])));
    }

    #[test]
    fn a_dolby_rpu_message_is_not_hdr10plus() {
        let sei = sei_nal(true, &RPU_MSG);
        assert!(!in_unit("h265/annexb", 0, &annexb(&[&sei, &slice_nal(true)])));
        assert!(!in_unit("av1", 0, &av1_tu(&RPU_MSG)));
        assert!((is_plus(&PLUS_MSG), is_plus(&RPU_MSG)) == (true, false));
    }

    #[test]
    fn an_av1_metadata_obu_is_found() {
        assert!(in_unit("av1", 0, &av1_tu(&PLUS_MSG)));
        let bare = [obu(2, &[]), obu(6, &[0x10, 0x20])].concat();
        assert!(!in_unit("av1", 0, &bare));
    }

    #[test]
    fn a_cut_or_foreign_unit_carries_none() {
        let unit = prefixed(&[&sei_nal(true, &PLUS_MSG)], 4);
        assert!(!in_unit("h265/annexb", 4, &unit[..unit.len() - 1]), "the unit cut");
        assert!(!in_unit("vp9", 0, &PLUS_MSG), "no in-band HDR10+");
        assert!(!in_unit("h265/annexb", 0, &[]));
        assert!(!in_unit("h265/annexb", 7, &unit), "a length size past 4");
    }
}
