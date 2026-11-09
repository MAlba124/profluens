use pf_hdr::h26x::{SEI_CONTENT_LIGHT, SEI_MASTERING_DISPLAY, sei_messages};
use pf_mkv::config::{NalCodec, nals};
use profluens_video::bits::de_emulate;
use profluens_video::color::{ContentLight, HdrStatic, Mastering};
use profluens_video::{h264parse, h265parse};

use crate::{Chroma, TrackDesc};

/// What the first SPS among the Annex B units of `head` states.
struct Stated {
    width: u32,
    height: u32,
    depth: u8,
    colour: profluens_video::color::StreamColor,
    sar: (u16, u16),
    chroma: Chroma,
}

/// SEI bytes read for HDR, the mastering and light payloads are 24 and 4, a longer SEI is read
/// up to here.
const SEI_READ: usize = 512;

fn first_sps(codec: NalCodec, head: &[u8]) -> Option<Stated> {
    for (from, to) in nals(head) {
        let nal = &head[from..to];
        match codec {
            NalCodec::H264 if nal[0] & 0x1F == 7 => {
                let p = h264parse::parse_sps(nal)?;
                let depth = (p.bit_depth_luma_minus8 + 8).min(16) as u8;
                let (width, height, colour, sar) = (p.width(), p.height(), p.colour, p.sar);
                let chroma = Chroma::from_idc(p.chroma_format_idc);
                return Some(Stated { width, height, depth, colour, sar, chroma });
            }
            NalCodec::H265 if (nal[0] >> 1) & 0x3F == 33 => {
                let p = h265parse::parse_sps(nal)?;
                let (width, height) = (p.display_width(), p.display_height());
                let (depth, colour, sar) = (p.depth(), p.colour, p.sar);
                let chroma = Chroma::from_idc(p.chroma_format_idc);
                return Some(Stated { width, height, depth, colour, sar, chroma });
            }
            _ => {}
        }
    }
    None
}

/// The mastering display and content light level the SEI units of Annex B `head` state, an
/// HEVC config record may carry them.
pub(crate) fn sei_hdr(codec: NalCodec, head: &[u8]) -> HdrStatic {
    let mut h = HdrStatic::default();
    let mut rbsp = [0u8; SEI_READ];
    for (from, to) in nals(head) {
        let nal = &head[from..to];
        let header = match codec {
            NalCodec::H264 if nal[0] & 0x1F == 6 => 1,
            NalCodec::H265 if matches!((nal[0] >> 1) & 0x3F, 39 | 40) => 2,
            _ => continue,
        };
        let Some(body) = nal.get(header..) else { continue };
        let n = de_emulate(body, &mut rbsp);
        for (kind, payload) in sei_messages(&rbsp[..n]) {
            match kind {
                SEI_MASTERING_DISPLAY => {
                    h.mastering = Mastering::from_st2086(payload).unwrap_or(h.mastering);
                }
                SEI_CONTENT_LIGHT => {
                    h.light = ContentLight::from_cta861(payload).unwrap_or(h.light);
                }
                _ => {}
            }
        }
    }
    h
}

/// The subsampling an AV1 codec configuration record states (AV1 ISOBMFF binding 2.3.3), its
/// third byte's mono_chrome and chroma_subsampling_x and _y.
pub(crate) fn av1c_chroma(head: &[u8]) -> Chroma {
    match head {
        [0x81, _, b, ..] => match (b >> 4 & 1, b >> 3 & 1, b >> 2 & 1) {
            (1, ..) => Chroma::Mono,
            (0, 1, 1) => Chroma::C420,
            (0, 1, 0) => Chroma::C422,
            (0, 0, 0) => Chroma::C444,
            _ => Chroma::Unknown,
        },
        _ => Chroma::Unknown,
    }
}

/// `t` with what the first SPS of its head states over the container's colour, its SEI's HDR
/// over the container's, and its size, depth, chroma and pixel aspect where the container states
/// none. A Matroska file may leave the colour to the bitstream, a PQ track would then read as
/// SDR. An AV1 head that is its config record states the chroma, an MP4's record set it before.
/// The NAL codec of a family whose heads are Annex B.
pub(crate) fn nal_codec(family: &str) -> Option<NalCodec> {
    match family {
        "h264/annexb" => Some(NalCodec::H264),
        "h265/annexb" => Some(NalCodec::H265),
        _ => None,
    }
}

pub(crate) fn fill(t: &mut TrackDesc, head: &[u8]) {
    let codec = match t.family {
        "h264/annexb" => NalCodec::H264,
        "h265/annexb" => NalCodec::H265,
        "av1" if t.chroma == Chroma::Unknown => {
            t.chroma = av1c_chroma(head);
            return;
        }
        _ => return,
    };
    t.colour.hdr = sei_hdr(codec, head).over(t.colour.hdr);
    let Some(s) = first_sps(codec, head) else { return };
    t.colour = s.colour.over(t.colour);
    t.chroma = s.chroma;
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
