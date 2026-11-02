//! Scaling in the decode call, `VAConfigAttribDecProcessing`. Each decode surface has a twin at
//! the plan's size the driver writes as it decodes (the SFC of the video box on iHD), the decode
//! surface stays the reference. iHD keeps the parameters of the last picture for the next, so
//! every picture of the session names its own twin.

use std::marker::PhantomData;

use crate::error::VaapiErrorKind as K;
use crate::ffi;
use crate::va::{Buffer, Context, Display, Surfaces, VaResult};
use crate::vpp::format::{Kind, NV12, P010};
use crate::vpp::plan::{Plan, Rect, Target};

/// The scaler's bound each way, iHD's MHW_SFC_MIN_SCALINGFACTOR and MAX.
const RATIO: u32 = 8;
/// Twins are coded at a multiple of this, the encoders take it.
const ALIGN: u32 = 16;

/// The decoder's plan of a `width` x `height` picture of `depth` bits into `t`, fitted, or
/// placed in `t`'s box when `fixed`, at its top left with bars right and below. iHD 26.1 on
/// Gen12 drops the offset of an HEVC picture's region, the top left lands the same on any
/// path. None past the scaler's ratio.
pub fn plan(width: u32, height: u32, depth: u8, t: Target, fixed: bool) -> Option<Plan> {
    let input = if depth > 8 { P010 } else { NV12 };
    let mut p = Plan::turned(width, height, input, t, 0, fixed)?;
    p.place = (0, 0, p.place.2, p.place.3);
    let (pw, ph) = (p.place.2, p.place.3);
    let within = |a: u32, b: u32| a * RATIO >= b && b * RATIO >= a;
    (within(pw, width) && within(ph, height)).then_some(p)
}

/// The twin surfaces of one session, black where the plan places nothing. A plan without bars
/// leaves them as allocated, the alignment rows are cropped, a CPU fill of 13 1080p P010
/// surfaces holds a seek's start back by tens of ms.
pub struct Twins {
    surfaces: Surfaces,
    pub plan: Plan,
    pub coded: (u32, u32),
    /// The decoded picture's region the scaler reads and where it writes it.
    src: Rect,
    dst: Rect,
}

/// The scaler's source and destination for a picture shown as `crop` (x, y, width, height) of
/// the `full` size the decoder makes, placed at `place`. iHD 26.1 on Gen9 reads the full
/// picture whatever the source region says, Gen12 reads the region. A crop at the origin is
/// read whole on both, the destination stretched by the same ratio so the shown part lands on
/// `place` and the rest past it. A crop off the origin keeps the region, as does one whose rest
/// would land in the bars of a `bounds` box, every decode would write the padding rows there.
pub fn regions(crop: Rect, full: (u32, u32), place: Rect, bounds: (u32, u32)) -> (Rect, Rect) {
    let (cx, cy, cw, ch) = crop;
    let (fw, fh) = full;
    if cx != 0 || cy != 0 || cw == 0 || ch == 0 || fw < cw || fh < ch || (fw, fh) == (cw, ch) {
        return (crop, place);
    }
    let (px, py, pw, ph) = place;
    let stretch = |p: u32, f: u32, c: u32| ((u64::from(p) * u64::from(f) + u64::from(c) / 2) / u64::from(c)) as u32;
    let (dw, dh) = (stretch(pw, fw, cw), stretch(ph, fh, ch));
    let shown = |p: u32, s: u32, d: u32, b: u32| d > s && p + s < b;
    if shown(px, pw, dw, bounds.0) || shown(py, ph, dh, bounds.1) {
        return (crop, place);
    }
    ((0, 0, fw, fh), (px, py, dw, dh))
}

impl Twins {
    /// `crop` and `full` as [`regions`] takes them.
    // Cold: once per decoder session.
    #[allow(clippy::disallowed_methods)]
    pub fn new(display: &Display, plan: Plan, count: u32, crop: Rect, full: (u32, u32)) -> Result<Twins, K> {
        let (src, dst) = regions(crop, full, plan.place, (plan.width, plan.height));
        let extent = (plan.width.max(dst.0 + dst.2), plan.height.max(dst.1 + dst.3));
        let coded = (extent.0.next_multiple_of(ALIGN), extent.1.next_multiple_of(ALIGN));
        let surfaces = Surfaces::new(display, plan.out.surface(), coded.0, coded.1, count)
            .map_err(K::SessionSetup)?;
        if plan.bars() {
            for &s in surfaces.ids() {
                crate::vpp::element::black(display, s, plan.out, coded)?;
            }
        }
        Ok(Twins { surfaces, plan, coded, src, dst })
    }

    pub fn kind(&self) -> Kind {
        self.plan.out
    }

    /// The twin of `surface`, the one at its index in `decode`.
    pub fn of(&self, decode: &Surfaces, surface: ffi::VASurfaceID) -> Option<ffi::VASurfaceID> {
        let at = decode.ids().iter().position(|&s| s == surface)?;
        self.surfaces.ids().get(at).copied()
    }
}

/// One picture's rectangles and twin, read by the driver when the buffer is rendered.
pub struct Regions {
    src: ffi::VARectangle,
    dst: ffi::VARectangle,
    twin: ffi::VASurfaceID,
}

impl Regions {
    /// The session's regions, onto `twin`.
    pub fn new(twins: &Twins, twin: ffi::VASurfaceID) -> Regions {
        let rect = |(x, y, w, h): (u32, u32, u32, u32)| ffi::VARectangle {
            x: x.min(i16::MAX as u32) as i16,
            y: y.min(i16::MAX as u32) as i16,
            width: w.min(u32::from(u16::MAX)) as u16,
            height: h.min(u32::from(u16::MAX)) as u16,
        };
        Regions { src: rect(twins.src), dst: rect(twins.dst), twin }
    }
}

/// The processing buffer of one picture, it points into the [`Regions`] it borrows.
pub struct Pipeline<'a> {
    buf: Buffer,
    _regions: PhantomData<&'a mut Regions>,
}

impl Pipeline<'_> {
    pub fn id(&self) -> ffi::VABufferID {
        self.buf.id()
    }
}

/// The processing buffer of the picture decoded into `target`, rendered with its picture
/// parameters. The chroma siting is the one VPL's decoders state, iHD keeps the scaled chroma
/// aligned with it.
pub fn pipeline<'a>(
    display: &Display,
    context: &Context,
    target: ffi::VASurfaceID,
    r: &'a mut Regions,
) -> VaResult<Pipeline<'a>> {
    let mut p = ffi::VAProcPipelineParameterBuffer::zeroed();
    p.surface = target;
    p.surface_region = &r.src;
    p.output_region = &r.dst;
    p.additional_outputs = &mut r.twin;
    p.num_additional_outputs = 1;
    p.input_color_properties.chroma_sample_location =
        ffi::VA_CHROMA_SITING_HORIZONTAL_LEFT | ffi::VA_CHROMA_SITING_VERTICAL_CENTER;
    let buf = Buffer::new_struct(display, context, ffi::VAProcPipelineParameterBufferType, &p)?;
    Ok(Pipeline { buf, _regions: PhantomData })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(w: u32, h: u32, depth: u8) -> Target {
        Target { max_width: w, max_height: h, depth }
    }

    #[test]
    fn a_4k_picture_fits_1080p_and_keeps_its_depth_only_when_asked() {
        let p = plan(3840, 2160, 10, t(1920, 1080, 10), true).expect("a plan");
        assert_eq!((p.width, p.height, p.out, p.place), (1920, 1080, P010, (0, 0, 1920, 1080)));
        assert_eq!(plan(3840, 2160, 10, t(1920, 1080, 8), true).map(|p| p.out), Some(NV12));
        assert_eq!(plan(3840, 2160, 8, t(1920, 1080, 10), true).map(|p| p.out), Some(NV12));
    }

    #[test]
    fn a_wide_picture_sits_in_bars_when_fixed_and_alone_when_not() {
        let fixed = plan(3840, 1600, 8, t(1920, 1080, 8), true).expect("a plan");
        assert_eq!((fixed.width, fixed.height, fixed.place), (1920, 1080, (0, 0, 1920, 800)));
        let fit = plan(3840, 1600, 8, t(1920, 1080, 8), false).expect("a plan");
        assert_eq!((fit.width, fit.height, fit.place), (1920, 800, (0, 0, 1920, 800)));
    }

    /// A crop at the origin is read whole, the destination stretched so the shown rows land on
    /// the placement. Uncropped and off the origin the regions stay.
    #[test]
    fn a_crop_at_the_origin_is_read_whole_and_stretched() {
        // 1036 shown of 1040 decoded, 1:1
        assert_eq!(regions((0, 0, 1920, 1036), (1920, 1040), (0, 0, 1920, 1036), (1920, 1036)), ((0, 0, 1920, 1040), (0, 0, 1920, 1040)));
        // 2160 shown of 2176, to 1080p
        assert_eq!(regions((0, 0, 3840, 2160), (3840, 2176), (0, 0, 1920, 1080), (1920, 1080)), ((0, 0, 3840, 2176), (0, 0, 1920, 1088)));
        assert_eq!(regions((0, 0, 1920, 1080), (1920, 1080), (0, 0, 960, 540), (960, 540)), ((0, 0, 1920, 1080), (0, 0, 960, 540)));
        assert_eq!(regions((8, 0, 1904, 1080), (1920, 1080), (0, 0, 952, 540), (952, 540)), ((8, 0, 1904, 1080), (0, 0, 952, 540)));
    }

    /// In a fixed box the stretched rest would land in the bars below or right of the picture,
    /// the region is kept.
    #[test]
    fn a_crop_in_bars_keeps_its_region() {
        // 2.39:1, 804 shown of 816, letterboxed in 1080p
        let crop = (0, 0, 1920, 804);
        assert_eq!(regions(crop, (1920, 816), (0, 0, 1920, 804), (1920, 1080)), (crop, (0, 0, 1920, 804)));
        // pillarboxed, 1436 shown of 1440
        let crop = (0, 0, 1436, 1080);
        assert_eq!(regions(crop, (1440, 1080), (0, 0, 1436, 1080), (1920, 1080)), (crop, (0, 0, 1436, 1080)));
        // the same picture fitted, the rest lands past the box
        let (src, dst) = regions((0, 0, 1920, 804), (1920, 816), (0, 0, 1920, 804), (1920, 804));
        assert_eq!((src, dst), ((0, 0, 1920, 816), (0, 0, 1920, 816)));
    }

    #[test]
    fn the_scaler_goes_no_further_than_an_eighth() {
        assert!(plan(7680, 4320, 8, t(960, 540, 8), true).is_some(), "an eighth");
        assert!(plan(7680, 4320, 8, t(640, 360, 8), true).is_none(), "a twelfth");
        assert!(plan(1280, 720, 8, t(1920, 1080, 8), true).is_some(), "fixed, a smaller picture fills the box");
    }
}
