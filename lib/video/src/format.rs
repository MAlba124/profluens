//! POD video format vocabulary and the `video/raw` negotiation offer.
//! [`PixelFormat::caps_name`] is the one source of the categorical names.

use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::time::Rational;

pub const FAMILY: &str = "video/raw";
pub const FIELD_WIDTH: &str = "width";
pub const FIELD_HEIGHT: &str = "height";
pub const FIELD_PIXFMT: &str = "pixfmt";
/// Frame rate as an exact rational, e.g. `30000/1001`.
pub const FIELD_FPS: &str = "fps";
/// Degrees clockwise the coded picture turns to stand upright, 90, 180 or 270, absent for none.
pub const FIELD_ROTATION: &str = "rotation";

/// A raster pixel format. See [`crate::geometry`] for plane layouts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelFormat {
    I420,
    Nv12,
    /// 10-bit NV12 in 16-bit LE words, significant bits high (`VA_FOURCC_P010`, `DRM_FORMAT_P010`).
    P010,
    /// ffmpeg's `yuv422p`.
    I422,
    /// [`I422`](PixelFormat::I422) in 16-bit LE words, significant bits low. ffmpeg's
    /// `yuv422p10le`.
    I422P10,
    Rgb24,
    Gray8,
}

impl PixelFormat {
    /// The categorical name used in `video/raw` offers.
    pub const fn caps_name(self) -> &'static str {
        match self {
            PixelFormat::I420 => "i420",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::P010 => "p010",
            PixelFormat::I422 => "i422",
            PixelFormat::I422P10 => "i422_10",
            PixelFormat::Rgb24 => "rgb24",
            PixelFormat::Gray8 => "gray8",
        }
    }

    pub fn from_caps_name(s: &str) -> Option<PixelFormat> {
        Some(match s {
            "i420" => PixelFormat::I420,
            "nv12" => PixelFormat::Nv12,
            "p010" => PixelFormat::P010,
            "i422" => PixelFormat::I422,
            "i422_10" => PixelFormat::I422P10,
            "rgb24" => PixelFormat::Rgb24,
            "gray8" => PixelFormat::Gray8,
            _ => return None,
        })
    }

    pub const fn plane_count(self) -> usize {
        match self {
            PixelFormat::I420 => 3, // Y, Cb, Cr
            PixelFormat::Nv12 => 2, // Y, interleaved CbCr
            PixelFormat::P010 => 2, // Y, interleaved CbCr, 16-bit words
            PixelFormat::I422 | PixelFormat::I422P10 => 3,
            PixelFormat::Rgb24 => 1, // packed RGB
            PixelFormat::Gray8 => 1, // packed luma
        }
    }

    pub const fn is_subsampled_420(self) -> bool {
        matches!(self, PixelFormat::I420 | PixelFormat::Nv12 | PixelFormat::P010)
    }

    pub const fn is_subsampled_422(self) -> bool {
        matches!(self, PixelFormat::I422 | PixelFormat::I422P10)
    }

    /// The format of three planes as a software decoder lays them out. `None` where none is named.
    pub const fn planar(bits: u8, chroma_shift: (u8, u8)) -> Option<PixelFormat> {
        match (bits, chroma_shift) {
            (8, (1, 1)) => Some(PixelFormat::I420),
            (8, (1, 0)) => Some(PixelFormat::I422),
            (10, (1, 0)) => Some(PixelFormat::I422P10),
            _ => None,
        }
    }

    /// 2 for the 16-bit word formats, else 1.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            PixelFormat::P010 | PixelFormat::I422P10 => 2,
            PixelFormat::I420
            | PixelFormat::Nv12
            | PixelFormat::I422
            | PixelFormat::Rgb24
            | PixelFormat::Gray8 => 1,
        }
    }
}

/// A set of pixel formats, one bit each.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PixelFormats(u16);

impl PixelFormats {
    pub const NONE: PixelFormats = PixelFormats(0);

    pub const fn with(self, f: PixelFormat) -> PixelFormats {
        PixelFormats(self.0 | 1 << f as u16)
    }

    pub const fn has(self, f: PixelFormat) -> bool {
        self.0 & 1 << f as u16 != 0
    }

    pub fn of_names<'a>(names: impl IntoIterator<Item = &'a str>) -> PixelFormats {
        names
            .into_iter()
            .filter_map(PixelFormat::from_caps_name)
            .fold(PixelFormats::NONE, PixelFormats::with)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub pixfmt: PixelFormat,
    /// An invalid or zero rate means no timing.
    pub fps: Rational,
}

impl VideoFormat {
    pub const fn new(width: u32, height: u32, pixfmt: PixelFormat, fps: Rational) -> Self {
        Self { width, height, pixfmt, fps }
    }
}

// A parser with a data-dependent format offers RAW_ANY_OFFER and announces the exact one at
// runtime.

static PIXFMT_VALUES: [ValueDesc; 7] = [
    ValueDesc::Id("i420"),
    ValueDesc::Id("nv12"),
    ValueDesc::Id("p010"),
    ValueDesc::Id("i422"),
    ValueDesc::Id("i422_10"),
    ValueDesc::Id("rgb24"),
    ValueDesc::Id("gray8"),
];

pub static MATRIX_VALUES: [ValueDesc; crate::color::Matrix::ALL.len()] =
    crate::color::Matrix::VALUES;
pub static RANGE_VALUES: [ValueDesc; 2] = crate::color::Range::VALUES;
pub static TRANSFER_VALUES: [ValueDesc; crate::color::TransferFn::ALL.len()] =
    crate::color::TransferFn::VALUES;
pub static PRIMARIES_VALUES: [ValueDesc; crate::color::Primaries::ALL.len()] =
    crate::color::Primaries::VALUES;
pub static CHROMA_SITE_VALUES: [ValueDesc; 6] = crate::color::ChromaSite::VALUES;

/// The colour fields of a pad that states colour. Every name is listed since an announced name
/// resolves only if some offer interned it.
pub const COLOR_OFFER_FIELDS: [FieldDesc; 10] = {
    const fn set(field: &'static str, values: &'static [ValueDesc]) -> FieldDesc {
        FieldDesc { field, allowed: ConstraintDesc::Set(values), preferred: None }
    }
    const fn any(field: &'static str) -> FieldDesc {
        FieldDesc { field, allowed: ConstraintDesc::Any, preferred: None }
    }
    [
        set(crate::color::FIELD_MATRIX, &MATRIX_VALUES),
        set(crate::color::FIELD_RANGE, &RANGE_VALUES),
        set(crate::color::FIELD_TRANSFER, &TRANSFER_VALUES),
        set(crate::color::FIELD_PRIMARIES, &PRIMARIES_VALUES),
        any(crate::color::FIELD_MASTERING_RG),
        any(crate::color::FIELD_MASTERING_BW),
        any(crate::color::FIELD_MASTERING_LUMINANCE),
        any(crate::color::FIELD_CONTENT_LIGHT),
        set(crate::color::FIELD_CHROMA_SITE, &CHROMA_SITE_VALUES),
        any(crate::color::FIELD_DOVI),
    ]
};

static RAW_ANY_FIELDS: [FieldDesc; 14] = {
    const fn any(field: &'static str) -> FieldDesc {
        FieldDesc { field, allowed: ConstraintDesc::Any, preferred: None }
    }
    [
        any(FIELD_WIDTH),
        any(FIELD_HEIGHT),
        FieldDesc {
            field: FIELD_PIXFMT,
            allowed: ConstraintDesc::Set(&PIXFMT_VALUES),
            preferred: None,
        },
        any(FIELD_FPS),
        // Absent colour fields stay unspecified and the consumer defaults by resolution.
        FieldDesc {
            field: crate::color::FIELD_MATRIX,
            allowed: ConstraintDesc::Set(&MATRIX_VALUES),
            preferred: None,
        },
        FieldDesc {
            field: crate::color::FIELD_RANGE,
            allowed: ConstraintDesc::Set(&RANGE_VALUES),
            preferred: None,
        },
        FieldDesc {
            field: crate::color::FIELD_TRANSFER,
            allowed: ConstraintDesc::Set(&TRANSFER_VALUES),
            preferred: None,
        },
        FieldDesc {
            field: crate::color::FIELD_PRIMARIES,
            allowed: ConstraintDesc::Set(&PRIMARIES_VALUES),
            preferred: None,
        },
        any(crate::color::FIELD_MASTERING_RG),
        any(crate::color::FIELD_MASTERING_BW),
        any(crate::color::FIELD_MASTERING_LUMINANCE),
        any(crate::color::FIELD_CONTENT_LIGHT),
        FieldDesc {
            field: crate::color::FIELD_CHROMA_SITE,
            allowed: ConstraintDesc::Set(&CHROMA_SITE_VALUES),
            preferred: None,
        },
        any(crate::color::FIELD_DOVI),
    ]
};

/// Any raster video, for a pad that accepts whatever the graph fixates.
pub static RAW_ANY_OFFER: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &RAW_ANY_FIELDS }];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_names_round_trip_for_every_variant() {
        for f in [
            PixelFormat::I420,
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::I422,
            PixelFormat::I422P10,
            PixelFormat::Rgb24,
            PixelFormat::Gray8,
        ] {
            assert_eq!(PixelFormat::from_caps_name(f.caps_name()), Some(f));
            assert!(
                PIXFMT_VALUES.iter().any(|v| matches!(v, ValueDesc::Id(n) if *n == f.caps_name()))
            );
        }
        assert_eq!(PixelFormat::from_caps_name("i420"), Some(PixelFormat::I420));
        assert_eq!(PixelFormat::from_caps_name("nope"), None);
    }

    #[test]
    fn plane_counts_match_the_format() {
        assert_eq!(PixelFormat::I420.plane_count(), 3);
        assert_eq!(PixelFormat::Nv12.plane_count(), 2);
        assert_eq!(PixelFormat::P010.plane_count(), 2);
        assert_eq!(PixelFormat::I422.plane_count(), 3);
        assert_eq!(PixelFormat::I422P10.plane_count(), 3);
        assert_eq!(PixelFormat::Rgb24.plane_count(), 1);
        assert_eq!(PixelFormat::Gray8.plane_count(), 1);
    }

    #[test]
    fn only_yuv_420_is_subsampled() {
        assert!(PixelFormat::I420.is_subsampled_420());
        assert!(PixelFormat::Nv12.is_subsampled_420());
        assert!(PixelFormat::P010.is_subsampled_420());
        assert!(!PixelFormat::I422.is_subsampled_420());
        assert!(!PixelFormat::I422P10.is_subsampled_420());
        assert!(!PixelFormat::Rgb24.is_subsampled_420());
        assert!(!PixelFormat::Gray8.is_subsampled_420());
    }

    #[test]
    fn only_the_422_formats_are_422() {
        assert!(PixelFormat::I422.is_subsampled_422());
        assert!(PixelFormat::I422P10.is_subsampled_422());
        for f in [
            PixelFormat::I420,
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::Rgb24,
            PixelFormat::Gray8,
        ] {
            assert!(!f.is_subsampled_422(), "{f:?}");
        }
    }

    #[test]
    fn a_planar_layout_names_its_format_where_one_exists() {
        assert_eq!(PixelFormat::planar(8, (1, 1)), Some(PixelFormat::I420));
        assert_eq!(PixelFormat::planar(8, (1, 0)), Some(PixelFormat::I422));
        assert_eq!(PixelFormat::planar(10, (1, 0)), Some(PixelFormat::I422P10));
        // P010 interleaves its chroma, so no planar 10-bit 4:2:0 is named, nor 4:4:4 yet.
        for (bits, shift) in [(10, (1, 1)), (8, (0, 0)), (12, (1, 0)), (8, (0, 1))] {
            assert_eq!(PixelFormat::planar(bits, shift), None, "{bits} {shift:?}");
        }
    }

    #[test]
    fn a_format_set_holds_what_was_put_in() {
        let set = PixelFormats::of_names(["i422_10", "nope", "i420"]);
        assert!(set.has(PixelFormat::I422P10) && set.has(PixelFormat::I420));
        assert!(!set.has(PixelFormat::I422) && !set.has(PixelFormat::P010));
        assert!(!PixelFormats::NONE.has(PixelFormat::Gray8));
    }

    #[test]
    fn p010_and_i422p10_are_the_wide_sample_formats() {
        assert_eq!(PixelFormat::P010.bytes_per_sample(), 2);
        assert_eq!(PixelFormat::I422P10.bytes_per_sample(), 2);
        for f in [
            PixelFormat::I420,
            PixelFormat::Nv12,
            PixelFormat::I422,
            PixelFormat::Rgb24,
            PixelFormat::Gray8,
        ] {
            assert_eq!(f.bytes_per_sample(), 1, "{f:?}");
        }
    }
}
