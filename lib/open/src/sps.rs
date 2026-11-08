use pf_mkv::config::{NalCodec, nals};
use profluens_video::{h264parse, h265parse};

use crate::TrackDesc;

/// What the first SPS among the Annex B units of `head` states.
struct Stated {
    width: u32,
    height: u32,
    depth: u8,
    colour: profluens_video::color::StreamColor,
    sar: (u16, u16),
}

fn first_sps(codec: NalCodec, head: &[u8]) -> Option<Stated> {
    for (from, to) in nals(head) {
        let nal = &head[from..to];
        match codec {
            NalCodec::H264 if nal[0] & 0x1F == 7 => {
                let p = h264parse::parse_sps(nal)?;
                let depth = (p.bit_depth_luma_minus8 + 8).min(16) as u8;
                let (width, height, colour, sar) = (p.width(), p.height(), p.colour, p.sar);
                return Some(Stated { width, height, depth, colour, sar });
            }
            NalCodec::H265 if (nal[0] >> 1) & 0x3F == 33 => {
                let p = h265parse::parse_sps(nal)?;
                let (width, height) = (p.display_width(), p.display_height());
                let (depth, colour, sar) = (p.depth(), p.colour, p.sar);
                return Some(Stated { width, height, depth, colour, sar });
            }
            _ => {}
        }
    }
    None
}

/// `t` with what the first SPS of its head states over the container's colour, and its size,
/// depth and pixel aspect where the container states none. A Matroska file may leave the colour
/// to the bitstream, a PQ track would then read as SDR.
pub(crate) fn fill(t: &mut TrackDesc, head: &[u8]) {
    let codec = match t.family {
        "h264/annexb" => NalCodec::H264,
        "h265/annexb" => NalCodec::H265,
        _ => return,
    };
    let Some(s) = first_sps(codec, head) else { return };
    t.colour = s.colour.over(t.colour);
    if t.width == 0 || t.height == 0 {
        (t.width, t.height) = (s.width, s.height);
    }
    if t.depth == 0 {
        t.depth = s.depth;
    }
    if (t.par.0 == 0 || t.par.1 == 0) && s.sar.0 > 0 && s.sar.1 > 0 {
        t.par = crate::ratio(u64::from(s.sar.0), u64::from(s.sar.1));
    }
}
