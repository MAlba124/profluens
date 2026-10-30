//! Formats shared by the elements, surface layouts and H.273 colour mapping.

use profluens_core::ctx::Ctx;
use profluens_core::format::{ConstraintDesc, FieldDesc, FixedFormat, Value, ValueDesc};
use profluens_video::color::{Matrix, Primaries, Range, TransferFn};
use profluens_video::decoder::Color;
use profluens_video::format::{COLOR_OFFER_FIELDS, PixelFormat};

use crate::ffi;
use crate::frames::{F_FPS, F_HEIGHT, F_PIXFMT, F_WIDTH, FAMILY_VIDEO_VPL};

pub const FAMILY_RAW: &str = "video/raw";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Nv12,
    P010,
}

impl Layout {
    pub const fn fourcc(self) -> u32 {
        match self {
            Layout::Nv12 => ffi::MFX_FOURCC_NV12,
            Layout::P010 => ffi::MFX_FOURCC_P010,
        }
    }

    pub fn of_fourcc(fourcc: u32) -> Option<Layout> {
        match fourcc {
            ffi::MFX_FOURCC_NV12 => Some(Layout::Nv12),
            ffi::MFX_FOURCC_P010 => Some(Layout::P010),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Layout::Nv12 => "nv12",
            Layout::P010 => "p010",
        }
    }

    pub const fn depth(self) -> u8 {
        match self {
            Layout::Nv12 => 8,
            Layout::P010 => 10,
        }
    }

    pub const fn bps(self) -> usize {
        match self {
            Layout::Nv12 => 1,
            Layout::P010 => 2,
        }
    }

    pub const fn pixfmt(self) -> PixelFormat {
        match self {
            Layout::Nv12 => PixelFormat::Nv12,
            Layout::P010 => PixelFormat::P010,
        }
    }
}

pub static VPL_PIXFMTS: [ValueDesc; 2] = [ValueDesc::Id("nv12"), ValueDesc::Id("p010")];
pub static RAW_IN_PIXFMTS: [ValueDesc; 3] =
    [ValueDesc::Id("i420"), ValueDesc::Id("nv12"), ValueDesc::Id("p010")];
pub static RAW_OUT_PIXFMTS: [ValueDesc; 2] = [ValueDesc::Id("i420"), ValueDesc::Id("p010")];

pub const fn fields(pixfmts: &'static [ValueDesc]) -> [FieldDesc; 14] {
    const fn any(field: &'static str) -> FieldDesc {
        FieldDesc { field, allowed: ConstraintDesc::Any, preferred: None }
    }
    let c = COLOR_OFFER_FIELDS;
    [
        any(F_WIDTH),
        any(F_HEIGHT),
        FieldDesc { field: F_PIXFMT, allowed: ConstraintDesc::Set(pixfmts), preferred: None },
        any(F_FPS),
        c[0],
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[7],
        c[8],
        c[9],
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InFormat {
    pub width: u32,
    pub height: u32,
    pub pixfmt: PixelFormat,
    pub fps: Option<(i32, i32)>,
}

impl InFormat {
    /// i420 interleaves into NV12.
    pub fn layout(&self) -> Layout {
        if self.pixfmt == PixelFormat::P010 { Layout::P010 } else { Layout::Nv12 }
    }

    /// 30 when none is stated.
    pub fn rate(&self) -> (u32, u32) {
        match self.fps {
            Some((n, d)) if n > 0 && d > 0 => (n as u32, d as u32),
            _ => (30, 1),
        }
    }
}

pub fn read_format(ctx: &Ctx, f: &FixedFormat) -> Option<InFormat> {
    let family = ctx.family_name(f.family)?;
    if family != FAMILY_RAW && family != FAMILY_VIDEO_VPL {
        return None;
    }
    let get = |field: &str| f.get(ctx.field_id(field)?);
    let int = |field| match get(field)? {
        Value::Int(v) if v > 0 => u32::try_from(v).ok(),
        _ => None,
    };
    let pixfmt = match get(F_PIXFMT)? {
        Value::Id(id) => PixelFormat::from_caps_name(ctx.value_name(id)?)?,
        _ => return None,
    };
    if !matches!(pixfmt, PixelFormat::I420 | PixelFormat::Nv12 | PixelFormat::P010) {
        return None;
    }
    let fps = match get(F_FPS) {
        Some(Value::Rat(n, d)) if n > 0 && d > 0 => Some((n, d)),
        _ => None,
    };
    Some(InFormat { width: int(F_WIDTH)?, height: int(F_HEIGHT)?, pixfmt, fps })
}

pub fn is_vpl(ctx: &Ctx, f: Option<&FixedFormat>) -> bool {
    f.is_some_and(|f| ctx.family_name(f.family) == Some(FAMILY_VIDEO_VPL))
}

pub fn color_of_signal(s: &ffi::mfxExtVideoSignalInfo, present: bool) -> Color {
    let mut c = Color::default();
    if !present {
        return c;
    }
    c.range = Some(if s.VideoFullRange != 0 { Range::Full } else { Range::Limited });
    if s.ColourDescriptionPresent != 0 {
        let cp = |v: u16| u8::try_from(v).unwrap_or(2);
        c.primaries = Primaries::from_h273(cp(s.ColourPrimaries));
        c.transfer = TransferFn::from_h273(cp(s.TransferCharacteristics));
        c.matrix = Matrix::from_h273(cp(s.MatrixCoefficients));
    }
    c
}

/// Unspecified (2) where `c` says nothing.
pub fn signal_of_color(id: u32, c: &Color) -> ffi::mfxExtVideoSignalInfo {
    let present = c.primaries.is_some() || c.transfer.is_some() || c.matrix.is_some();
    ffi::mfxExtVideoSignalInfo {
        Header: crate::session::header::<ffi::mfxExtVideoSignalInfo>(id),
        VideoFormat: 5,
        VideoFullRange: u16::from(c.range == Some(Range::Full)),
        ColourDescriptionPresent: u16::from(present),
        ColourPrimaries: c.primaries.map_or(2, |p| u16::from(p.to_h273())),
        TransferCharacteristics: c.transfer.map_or(2, |t| u16::from(t.to_h273())),
        MatrixCoefficients: c.matrix.map_or(2, |m| u16::from(m.to_h273())),
    }
}

pub fn is_hdr(c: &Color) -> bool {
    matches!(c.transfer, Some(TransferFn::Pq) | Some(TransferFn::Hlg))
}

/// i420 for NV12, p010 as it is.
pub fn raw_frame_len(layout: Layout, w: u32, h: u32) -> usize {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let bps = layout.bps();
    (w * h) as usize * bps + 2 * (cw * ch) as usize * bps
}

pub const fn raw_name(layout: Layout) -> &'static str {
    match layout {
        Layout::Nv12 => "i420",
        Layout::P010 => "p010",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_and_lengths() {
        assert_eq!(Layout::of_fourcc(ffi::MFX_FOURCC_P010), Some(Layout::P010));
        assert_eq!(Layout::of_fourcc(0), None);
        assert_eq!(raw_frame_len(Layout::Nv12, 320, 240), 320 * 240 * 3 / 2);
        assert_eq!(raw_frame_len(Layout::P010, 64, 64), 64 * 64 * 3);
        assert_eq!(raw_frame_len(Layout::Nv12, 61, 37), 61 * 37 + 2 * 31 * 19);
    }

    #[test]
    fn signal_info_round_trips_the_colour() {
        let s = ffi::mfxExtVideoSignalInfo {
            VideoFullRange: 0,
            ColourDescriptionPresent: 1,
            ColourPrimaries: 9,
            TransferCharacteristics: 16,
            MatrixCoefficients: 9,
            ..Default::default()
        };
        let c = color_of_signal(&s, true);
        assert_eq!(
            (c.range, c.primaries, c.transfer, c.matrix),
            (
                Some(Range::Limited),
                Some(Primaries::Bt2020),
                Some(TransferFn::Pq),
                Some(Matrix::Bt2020)
            )
        );
        assert!(is_hdr(&c));
        let back = signal_of_color(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO_IN, &c);
        assert_eq!(
            (
                back.ColourPrimaries,
                back.TransferCharacteristics,
                back.MatrixCoefficients,
                back.VideoFullRange
            ),
            (9, 16, 9, 0)
        );
        assert_eq!(color_of_signal(&s, false), Color::default());
        let none = signal_of_color(0, &Color::default());
        assert_eq!((none.ColourDescriptionPresent, none.ColourPrimaries), (0, 2));
    }
}
