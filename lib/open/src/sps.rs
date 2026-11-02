use pf_mkv::config::{NalCodec, nals};
use profluens_video::{h264parse, h265parse};

use crate::TrackDesc;

/// The picture size, the bit depth and the colour of the first SPS among the Annex B units of
/// `head`.
fn first_sps(
    codec: NalCodec,
    head: &[u8],
) -> Option<(u32, u32, u8, profluens_video::color::StreamColor)> {
    for (from, to) in nals(head) {
        let nal = &head[from..to];
        match codec {
            NalCodec::H264 if nal[0] & 0x1F == 7 => {
                let p = h264parse::parse_sps(nal)?;
                let depth = (p.bit_depth_luma_minus8 + 8).min(16) as u8;
                return Some((p.width(), p.height(), depth, p.colour));
            }
            NalCodec::H265 if (nal[0] >> 1) & 0x3F == 33 => {
                let p = h265parse::parse_sps(nal)?;
                return Some((p.display_width(), p.display_height(), p.depth(), p.colour));
            }
            _ => {}
        }
    }
    None
}

/// `t` with what the first SPS of its head states over the container's colour, and its size and
/// depth where the container states none. A Matroska file may leave the colour to the
/// bitstream, a PQ track would then read as SDR.
pub(crate) fn fill(t: &mut TrackDesc, head: &[u8]) {
    let codec = match t.family {
        "h264/annexb" => NalCodec::H264,
        "h265/annexb" => NalCodec::H265,
        _ => return,
    };
    let Some((width, height, depth, colour)) = first_sps(codec, head) else { return };
    t.colour = colour.over(t.colour);
    if t.width == 0 || t.height == 0 {
        (t.width, t.height) = (width, height);
    }
    if t.depth == 0 {
        t.depth = depth;
    }
}
