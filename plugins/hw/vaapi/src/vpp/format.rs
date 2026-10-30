//! The surface layouts the processor reads and writes.

use crate::ffi;
use crate::va::SurfaceFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Chroma {
    C420,
    C422,
    C444,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    pub name: &'static str,
    pub fourcc: u32,
    pub rt_format: u32,
    pub bpp: u32,
    pub chroma: Chroma,
    pub depth: u8,
}

impl Kind {
    pub const fn surface(self) -> SurfaceFormat {
        SurfaceFormat { rt_format: self.rt_format, fourcc: self.fourcc, bits_per_pixel: self.bpp }
    }
}

const fn fourcc(b: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*b)
}

const fn kind(
    name: &'static str,
    fourcc: u32,
    rt_format: u32,
    bpp: u32,
    chroma: Chroma,
    depth: u8,
) -> Kind {
    Kind { name, fourcc, rt_format, bpp, chroma, depth }
}

pub const NV12: Kind = kind("NV12", fourcc(b"NV12"), ffi::VA_RT_FORMAT_YUV420, 12, Chroma::C420, 8);
pub const P010: Kind =
    kind("P010", fourcc(b"P010"), ffi::VA_RT_FORMAT_YUV420_10, 24, Chroma::C420, 10);
pub const P012: Kind =
    kind("P012", fourcc(b"P012"), ffi::VA_RT_FORMAT_YUV420_12, 24, Chroma::C420, 12);
pub const P016: Kind =
    kind("P016", fourcc(b"P016"), ffi::VA_RT_FORMAT_YUV420_12, 24, Chroma::C420, 16);
pub const YUY2: Kind = kind("YUY2", fourcc(b"YUY2"), ffi::VA_RT_FORMAT_YUV422, 16, Chroma::C422, 8);
pub const Y210: Kind =
    kind("Y210", fourcc(b"Y210"), ffi::VA_RT_FORMAT_YUV422_10, 32, Chroma::C422, 10);
pub const Y212: Kind =
    kind("Y212", fourcc(b"Y212"), ffi::VA_RT_FORMAT_YUV422_12, 32, Chroma::C422, 12);
pub const P444: Kind = kind("444P", fourcc(b"444P"), ffi::VA_RT_FORMAT_YUV444, 24, Chroma::C444, 8);
pub const AYUV: Kind = kind("AYUV", fourcc(b"AYUV"), ffi::VA_RT_FORMAT_YUV444, 32, Chroma::C444, 8);
pub const XYUV: Kind = kind("XYUV", fourcc(b"XYUV"), ffi::VA_RT_FORMAT_YUV444, 32, Chroma::C444, 8);
pub const Y410: Kind =
    kind("Y410", fourcc(b"Y410"), ffi::VA_RT_FORMAT_YUV444_10, 32, Chroma::C444, 10);
pub const Y412: Kind =
    kind("Y412", fourcc(b"Y412"), ffi::VA_RT_FORMAT_YUV444_12, 64, Chroma::C444, 12);
pub const Y416: Kind =
    kind("Y416", fourcc(b"Y416"), ffi::VA_RT_FORMAT_YUV444_12, 64, Chroma::C444, 16);

pub const ALL: [Kind; 13] =
    [NV12, P010, P012, P016, YUY2, Y210, Y212, P444, AYUV, XYUV, Y410, Y412, Y416];

pub const OUTPUTS: [Kind; 2] = [NV12, P010];

pub fn by_fourcc(fourcc: u32) -> Option<Kind> {
    ALL.into_iter().find(|k| k.fourcc == fourcc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourccs_match_libva() {
        assert_eq!(NV12.fourcc, 0x3231_564e);
        assert_eq!(P010.fourcc, 0x3031_3050);
        assert_eq!(P012.fourcc, 0x3231_3050);
        assert_eq!(P444.fourcc, 0x5034_3434);
        assert_eq!(XYUV.fourcc, 0x5655_5958);
        assert_eq!(Y410.fourcc, 0x3031_3459);
        assert_eq!(Y416.fourcc, 0x3631_3459);
        assert_eq!(NV12.fourcc, ffi::VA_FOURCC_NV12);
        assert_eq!(P010.rt_format, ffi::VA_RT_FORMAT_YUV420_10);
        assert_eq!(NV12.surface(), SurfaceFormat::NV12);
        assert_eq!(P010.surface(), SurfaceFormat::P010);
    }
}
