//! The VPP of one session, run by `vplvpp` and by a fused decoder's fallback.

use profluens_video::color::{ContentLight, Mastering};
use profluens_video::decoder::Color;

use crate::error::{MfxError, VplErrorKind as K, check};
use crate::ffi;
use crate::format::{Layout, is_hdr, signal_of_color};
use crate::frames::Geometry;
use crate::probe::{Runtime, frame_info};
use profluens_core::log;
use profluens_core::log::{Level, Loggable};
use std::sync::Arc;

use crate::session::{Component, Owner, Session, Surface, attach, header};

/// A zero bound is no bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub max_width: u32,
    pub max_height: u32,
    pub depth: u8,
}

impl Default for Target {
    fn default() -> Self {
        Target { max_width: 0, max_height: 0, depth: 10 }
    }
}

/// x, y, width, height.
pub type Rect = (u32, u32, u32, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub width: u32,
    pub height: u32,
    pub out: Layout,
    pub place: Rect,
    /// Degrees clockwise the picture is turned, 0, 90, 180 or 270. The sides above are the
    /// turned picture's.
    pub rotation: u16,
}

impl Plan {
    pub fn is_identity(&self, width: u32, height: u32, input: Layout) -> bool {
        self.width == width
            && self.height == height
            && self.out == input
            && !self.bars()
            && self.rotation == 0
    }

    /// The plan of a `width` x `height` input turned `rotation` degrees clockwise, the box fit
    /// to the turned picture, `fixed` as [`plan_fixed`] else as [`plan`].
    pub fn turned(
        width: u32,
        height: u32,
        input: Layout,
        t: Target,
        rotation: u16,
        fixed: bool,
    ) -> Option<Plan> {
        let rotation = match rotation {
            90 | 180 | 270 => rotation,
            _ => 0,
        };
        let (w, h) = if rotation % 180 == 90 { (height, width) } else { (width, height) };
        let p = if fixed { plan_fixed(w, h, input, t) } else { plan(w, h, input, t) }?;
        Some(Plan { rotation, ..p })
    }

    pub fn bars(&self) -> bool {
        self.place != (0, 0, self.width, self.height)
    }
}

/// `w` by `h` scaled to touch the `bw` by `bh` box, aspect kept, rounded to nearest.
pub fn fit(w: u64, h: u64, bw: u64, bh: u64) -> (u64, u64) {
    if bw * h <= bh * w { (bw, (h * bw + w / 2) / w) } else { ((w * bh + h / 2) / h, bh) }
}

fn even(v: u64) -> u32 {
    (v & !1).max(2) as u32
}

fn out_layout(input: Layout, t: Target) -> Layout {
    if input.depth().min(t.depth) > 8 { Layout::P010 } else { Layout::Nv12 }
}

/// 4:2:0 at the lesser depth, fit to the box with aspect kept, never upscaled, even sides.
pub fn plan(width: u32, height: u32, input: Layout, t: Target) -> Option<Plan> {
    if width == 0 || height == 0 {
        return None;
    }
    let (w, h) = (u64::from(width), u64::from(height));
    let bw = if t.max_width == 0 { w } else { u64::from(t.max_width).min(w) };
    let bh = if t.max_height == 0 { h } else { u64::from(t.max_height).min(h) };
    let (ow, oh) = fit(w, h, bw, bh);
    let (ow, oh) = (even(ow), even(oh));
    Some(Plan {
        width: ow,
        height: oh,
        out: out_layout(input, t),
        place: (0, 0, ow, oh),
        rotation: 0,
    })
}

/// Exactly `t`'s box, the source scaled to fit and centred, the rest bars.
pub fn plan_fixed(width: u32, height: u32, input: Layout, t: Target) -> Option<Plan> {
    if width == 0 || height == 0 || t.max_width == 0 || t.max_height == 0 {
        return None;
    }
    let (ow, oh) = (even(u64::from(t.max_width)), even(u64::from(t.max_height)));
    let (pw, ph) = fit(u64::from(width), u64::from(height), u64::from(ow), u64::from(oh));
    let (pw, ph) = (even(pw).min(ow), even(ph).min(oh));
    let place = (((ow - pw) / 2) & !1, ((oh - ph) / 2) & !1, pw, ph);
    Some(Plan { width: ow, height: oh, out: out_layout(input, t), place, rotation: 0 })
}

/// Coded size aligned to 16. The runtime fills outside the crop black.
pub fn out_info(p: Plan, rate: (u32, u32)) -> ffi::mfxFrameInfo {
    let mut info = frame_info(p.out.fourcc(), p.width as u16, p.height as u16);
    let (x, y, w, h) = p.place;
    (info.CropX, info.CropY, info.CropW, info.CropH) = (x as u16, y as u16, w as u16, h as u16);
    info.FrameRateExtN = rate.0;
    info.FrameRateExtD = rate.1;
    info
}

#[derive(Clone, Copy)]
struct ToneMap {
    vsi_in: ffi::mfxExtVideoSignalInfo,
    vsi_out: ffi::mfxExtVideoSignalInfo,
    mdcv: ffi::mfxExtMasteringDisplayColourVolume,
    cll: ffi::mfxExtContentLightLevelInfo,
}

impl ToneMap {
    fn new(color: &Color) -> ToneMap {
        let sdr = Color {
            primaries: Some(profluens_video::color::Primaries::Bt709),
            transfer: Some(profluens_video::color::TransferFn::Bt709),
            matrix: Some(profluens_video::color::Matrix::Bt709),
            range: Some(profluens_video::color::Range::Limited),
            ..Color::default()
        };
        // No mastering display is taken as 1000 nit BT.2020. Zeros map everything to black.
        let m = if color.hdr.mastering == Mastering::default() {
            Mastering {
                primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
                white: [15635, 16450],
                max_luminance: 10_000_000,
                min_luminance: 1,
            }
        } else {
            color.hdr.mastering
        };
        let light = if color.hdr.light == ContentLight::default() {
            ContentLight { max_cll: 1000, max_fall: 400 }
        } else {
            color.hdr.light
        };
        // The runtime takes RGB order, the SEI gives GBR.
        ToneMap {
            vsi_in: signal_of_color(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO_IN, color),
            vsi_out: signal_of_color(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO_OUT, &sdr),
            mdcv: ffi::mfxExtMasteringDisplayColourVolume {
                Header: header::<ffi::mfxExtMasteringDisplayColourVolume>(
                    ffi::MFX_EXTBUFF_MASTERING_DISPLAY_COLOUR_VOLUME_IN,
                ),
                DisplayPrimariesX: [m.primaries[0][0], m.primaries[1][0], m.primaries[2][0]],
                DisplayPrimariesY: [m.primaries[0][1], m.primaries[1][1], m.primaries[2][1]],
                WhitePointX: m.white[0],
                WhitePointY: m.white[1],
                MaxDisplayMasteringLuminance: m.max_luminance / 10_000,
                MinDisplayMasteringLuminance: m.min_luminance,
                ..Default::default()
            },
            cll: ffi::mfxExtContentLightLevelInfo {
                Header: header::<ffi::mfxExtContentLightLevelInfo>(
                    ffi::MFX_EXTBUFF_CONTENT_LIGHT_LEVEL_INFO,
                ),
                MaxContentLightLevel: light.max_cll,
                MaxPicAverageLightLevel: light.max_fall,
                ..Default::default()
            },
        }
    }
}

/// Output surfaces preallocated, the floor under what the consumer declares it holds.
pub const OUT_SURFACES: u32 = 8;
/// Outputs the runtime may add past the preallocation.
pub const OUT_DELTA: u32 = 2;

/// The output pool, the consumer's hold plus the depth in flight plus one, at least
/// [`OUT_SURFACES`].
pub fn out_pool(hold: Option<u32>, async_depth: u16) -> u32 {
    let need = hold.unwrap_or(0).saturating_add(u32::from(async_depth)).saturating_add(1);
    need.max(OUT_SURFACES)
}

/// Bytes of one 4:2:0 surface at the coded size.
pub fn surface_bytes(info: &ffi::mfxFrameInfo) -> u64 {
    let bps: u64 = if info.FourCC == ffi::MFX_FOURCC_P010 { 2 } else { 1 };
    u64::from(info.Width) * u64::from(info.Height) * 3 / 2 * bps
}

/// The pool line at trace level, one per component init, `name` the component.
pub fn trace_pool(
    log: &dyn Loggable,
    name: &str,
    suggested: u32,
    preallocated: u32,
    info: &ffi::mfxFrameInfo,
) {
    if log.enabled(Level::Trace) {
        let bytes = surface_bytes(info);
        log!(
            log,
            Level::Trace,
            "pool";
            "{name} suggested={suggested} preallocated={preallocated} surface={}x{} fourcc={:#x} bytes={bytes} pool_bytes={}",
            info.Width,
            info.Height,
            info.FourCC,
            bytes * u64::from(preallocated)
        );
    }
}

pub struct VppCore {
    owner: Arc<Owner>,
    in_info: ffi::mfxFrameInfo,
    out: ffi::mfxFrameInfo,
    plan: Plan,
    async_depth: u16,
    pool: u32,
    pub tone_mapped: bool,
    pub tone_map_refused: Option<MfxError>,
}

/// How a [`VppCore`] runs: its async depth and output preallocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub async_depth: u16,
    pub pool: u32,
}

impl VppCore {
    /// On `session`, made and joined by the caller. A PQ or HLG input to an 8 bit plan is tone
    /// mapped to BT.709 when the runtime takes it. `log` takes the init ladder at trace level.
    pub fn new(
        runtime: &Runtime,
        session: Session,
        mut in_info: ffi::mfxFrameInfo,
        p: Plan,
        color: &Color,
        setup: Setup,
        log: &dyn Loggable,
    ) -> Result<VppCore, K> {
        let Setup { async_depth, pool } = setup;
        // Init refuses a zero rate on either side.
        let rate = rate_or_default(in_info.FrameRateExtN, in_info.FrameRateExtD);
        (in_info.FrameRateExtN, in_info.FrameRateExtD) = rate;
        let out = out_info(p, rate);
        let mut core = VppCore {
            owner: Owner::new(session),
            in_info,
            out,
            plan: p,
            async_depth: async_depth.max(1),
            pool,
            tone_mapped: false,
            tone_map_refused: None,
        };
        let want_tone_map = is_hdr(color) && p.out == Layout::Nv12 && runtime.caps.tone_map;
        if want_tone_map {
            match core.init(Some(&ToneMap::new(color)), log) {
                Ok(()) => {
                    core.tone_mapped = true;
                    return Ok(core);
                }
                Err(e) => core.tone_map_refused = Some(e),
            }
        }
        core.init(None, log).map_err(K::Init)?;
        Ok(core)
    }

    fn param(&self) -> ffi::mfxVideoParam {
        let mut p = ffi::mfxVideoParam::zeroed();
        p.AsyncDepth = self.async_depth;
        p.IOPattern = ffi::MFX_IOPATTERN_IN_VIDEO_MEMORY | ffi::MFX_IOPATTERN_OUT_VIDEO_MEMORY;
        let v = p.vpp_mut();
        v.In = self.in_info;
        v.Out = self.out;
        p
    }

    fn init(&mut self, tone: Option<&ToneMap>, log: &dyn Loggable) -> Result<(), MfxError> {
        let mut p = self.param();
        let mut scaling = ffi::mfxExtVPPScaling {
            Header: header::<ffi::mfxExtVPPScaling>(ffi::MFX_EXTBUFF_VPP_SCALING),
            ScalingMode: ffi::MFX_SCALING_MODE_QUALITY,
            InterpolationMethod: ffi::MFX_INTERPOLATION_DEFAULT,
            ..Default::default()
        };
        // Scale and convert only, no denoise or sharpening.
        let mut off = [ffi::MFX_EXTBUFF_VPP_DENOISE2, ffi::MFX_EXTBUFF_VPP_DETAIL];
        let mut not_use = ffi::mfxExtVPPDoNotUse {
            Header: header::<ffi::mfxExtVPPDoNotUse>(ffi::MFX_EXTBUFF_VPP_DONOTUSE),
            NumAlg: 2,
            AlgList: off.as_mut_ptr(),
        };
        let mut hints = ffi::mfxExtAllocationHints {
            Header: header::<ffi::mfxExtAllocationHints>(ffi::MFX_EXTBUFF_ALLOCATION_HINTS),
            AllocationPolicy: ffi::MFX_ALLOCATION_LIMITED,
            NumberToPreAllocate: self.pool,
            DeltaToAllocateOnTheFly: OUT_DELTA,
            VPPPoolType: ffi::MFX_VPP_POOL_OUT,
            Wait: 0,
            ..Default::default()
        };
        let mut tone = tone.copied();
        let mut turn = ffi::mfxExtVPPRotation {
            Header: header::<ffi::mfxExtVPPRotation>(ffi::MFX_EXTBUFF_VPP_ROTATION),
            Angle: self.plan.rotation,
            ..Default::default()
        };
        let raw = self.owner.session.raw();
        // Each ladder step leaves one hint out until the runtime takes the query.
        let mut last = MfxError { status: ffi::MFX_ERR_UNSUPPORTED, call: "MFXVideoVPP_Query" };
        for step in 0..4 {
            let mut list: [*mut ffi::mfxExtBuffer; 8] = [std::ptr::null_mut(); 8];
            let mut n = 0;
            // the turn is the picture's, never left out
            if self.plan.rotation != 0 {
                list[n] = (&raw mut turn).cast();
                n += 1;
            }
            if step < 1 {
                list[n] = (&raw mut scaling).cast();
                n += 1;
            }
            if step < 2 {
                list[n] = (&raw mut not_use).cast();
                n += 1;
            }
            if step < 3 {
                list[n] = (&raw mut hints).cast();
                n += 1;
            }
            if let Some(t) = tone.as_mut() {
                for b in [
                    (&raw mut t.vsi_in).cast::<ffi::mfxExtBuffer>(),
                    (&raw mut t.vsi_out).cast(),
                    (&raw mut t.mdcv).cast(),
                    (&raw mut t.cll).cast(),
                ] {
                    list[n] = b;
                    n += 1;
                }
            }
            attach(&mut p, &mut list[..n]);
            let mut out = p;
            // SAFETY: p and its buffers live for the calls, out shares the buffers.
            let st = unsafe { ffi::MFXVideoVPP_Query(raw, &mut p, &mut out) };
            log!(log, Level::Trace, "query";
                "step={step} st={} in={}x{} crop={}x{} fourcc={:#x} out={}x{} fourcc={:#x} turn={} async={} -> {}",
                MfxError { status: st, call: "" }.name(),
                self.in_info.Width,
                self.in_info.Height,
                self.in_info.CropW,
                self.in_info.CropH,
                self.in_info.FourCC,
                self.out.Width,
                self.out.Height,
                self.out.FourCC,
                self.plan.rotation,
                p.AsyncDepth,
                out.AsyncDepth
            );
            if let Err(e) = check(st, "MFXVideoVPP_Query") {
                last = e;
                continue;
            }
            // SAFETY: as above.
            let st = unsafe { ffi::MFXVideoVPP_Init(raw, &mut p) };
            log!(log, Level::Trace, "init";
                "step={step} st={}",
                MfxError { status: st, call: "" }.name()
            );
            match check(st, "MFXVideoVPP_Init") {
                Ok(()) => {
                    self.owner.set(Component::Vpp);
                    self.trace_pool(step, log);
                    return Ok(());
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// The pool line. Past step 2 the hints were dropped and the runtime sizes the pool itself.
    fn trace_pool(&self, step: u32, log: &dyn Loggable) {
        if !log.enabled(Level::Trace) {
            return;
        }
        let mut p = self.param();
        let mut req = [ffi::mfxFrameAllocRequest::default(); 2];
        // SAFETY: p valid, two requests, in and out.
        let st = unsafe {
            ffi::MFXVideoVPP_QueryIOSurf(self.owner.session.raw(), &mut p, req.as_mut_ptr())
        };
        let suggested = if st >= 0 { u32::from(req[1].NumFrameSuggested) } else { 0 };
        let preallocated = if step < 3 { self.pool } else { suggested };
        trace_pool(log, "vplvpp", suggested, preallocated, &self.out);
    }

    pub fn session(&self) -> &Session {
        &self.owner.session
    }

    pub fn owner(&self) -> &Arc<Owner> {
        &self.owner
    }

    pub fn in_info(&self) -> ffi::mfxFrameInfo {
        self.in_info
    }

    pub fn geometry(&self) -> Geometry {
        let o = self.out;
        Geometry {
            coded_w: u32::from(o.Width),
            coded_h: u32::from(o.Height),
            disp_w: self.plan.width,
            disp_h: self.plan.height,
            crop_x: 0,
            crop_y: 0,
            fourcc: o.FourCC,
        }
    }

    pub fn input_surface(&self) -> Result<Surface, K> {
        let mut raw: *mut ffi::mfxFrameSurface1 = std::ptr::null_mut();
        // SAFETY: the VPP is initialized, raw a valid out pointer.
        check(
            unsafe { ffi::MFXMemory_GetSurfaceForVPP(self.owner.session.raw(), &mut raw) },
            "MFXMemory_GetSurfaceForVPP",
        )
        .map_err(K::NoSurface)?;
        // SAFETY: one reference for us.
        unsafe { Surface::from_raw(raw) }.map(|s| s.owned_by(&self.owner)).ok_or(K::NoSurface(
            MfxError { status: ffi::MFX_ERR_NULL_PTR, call: "MFXMemory_GetSurfaceForVPP" },
        ))
    }

    /// `None` input drains. The output may carry an earlier input's stamp when the runtime kept
    /// one. It is not waited for, its consumer polls it.
    pub fn process(&mut self, input: Option<&Surface>, log: &dyn Loggable) -> Result<Processed, K> {
        const CALL: &str = "MFXVideoVPP_ProcessFrameAsync";
        let mut out: *mut ffi::mfxFrameSurface1 = std::ptr::null_mut();
        // SAFETY: initialized VPP, input live or null for the drain, out valid.
        let st = unsafe {
            ffi::MFXVideoVPP_ProcessFrameAsync(
                self.owner.session.raw(),
                input.map_or(std::ptr::null_mut(), |s| s.raw()),
                &mut out,
            )
        };
        if st != ffi::MFX_WRN_DEVICE_BUSY {
            log!(
                log,
                Level::Trace,
                "process";
                "st={} input={} out={:?}",
                MfxError { status: st, call: "" }.name(),
                input.is_some(),
                out
            );
        }
        match st {
            ffi::MFX_WRN_DEVICE_BUSY | ffi::MFX_WRN_IN_EXECUTION => return Ok(Processed::Busy),
            ffi::MFX_ERR_MORE_DATA => return Ok(Processed::Kept),
            ffi::MFX_WRN_ALLOC_TIMEOUT_EXPIRED => return Ok(Processed::Wait),
            _ => check(st, CALL).map_err(K::Process)?,
        }
        // SAFETY: the runtime handed the output with one reference for us.
        match unsafe { Surface::from_raw(out) } {
            Some(s) => Ok(Processed::Out(s.owned_by(&self.owner))),
            // A warning with no picture, named as it came.
            None if st != ffi::MFX_ERR_NONE => Err(K::Process(MfxError { status: st, call: CALL })),
            None => Err(K::Process(MfxError { status: ffi::MFX_ERR_NULL_PTR, call: CALL })),
        }
    }
}

pub enum Processed {
    Out(Surface),
    Kept,
    /// No free output surface.
    Wait,
    /// The device took nothing, the input goes again.
    Busy,
}

/// A stream rate, or 30 fps where it has none.
pub fn rate_or_default(n: u32, d: u32) -> (u32, u32) {
    if n > 0 && d > 0 { (n, d) } else { (30, 1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(max_width: u32, max_height: u32, depth: u8) -> Target {
        Target { max_width, max_height, depth }
    }

    #[test]
    fn fits_the_box_keeping_the_aspect() {
        assert_eq!(
            plan(3840, 2160, Layout::Nv12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((1920, 1080))
        );
        assert_eq!(
            plan(3840, 1606, Layout::Nv12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((1920, 802))
        );
        assert_eq!(
            plan(1080, 1920, Layout::Nv12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((608, 1080))
        );
        assert_eq!(
            plan(3840, 2160, Layout::Nv12, t(0, 720, 8)).map(|p| (p.width, p.height)),
            Some((1280, 720))
        );
    }

    #[test]
    fn never_upscales_and_stays_even() {
        assert_eq!(
            plan(1280, 720, Layout::Nv12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((1280, 720))
        );
        assert_eq!(
            plan(1919, 1079, Layout::Nv12, Target::default()).map(|p| (p.width, p.height)),
            Some((1918, 1078))
        );
        assert_eq!(
            plan(1, 1, Layout::Nv12, Target::default()).map(|p| (p.width, p.height)),
            Some((2, 2))
        );
        assert_eq!(plan(0, 1080, Layout::Nv12, Target::default()), None);
    }

    #[test]
    fn depth_comes_down_to_what_is_taken() {
        assert_eq!(plan(64, 64, Layout::P010, t(0, 0, 10)).map(|p| p.out), Some(Layout::P010));
        assert_eq!(plan(64, 64, Layout::P010, t(0, 0, 8)).map(|p| p.out), Some(Layout::Nv12));
        assert_eq!(plan(64, 64, Layout::Nv12, t(0, 0, 10)).map(|p| p.out), Some(Layout::Nv12));
        let p = plan(1920, 1080, Layout::Nv12, t(1920, 1080, 8));
        assert!(p.is_some_and(|p| p.is_identity(1920, 1080, Layout::Nv12)));
        let info = out_info(plan(1920, 1080, Layout::Nv12, t(1280, 720, 8)).unwrap(), (25, 1));
        assert_eq!(
            (info.Width, info.Height, info.CropW, info.CropH, info.FrameRateExtN),
            (1280, 720, 1280, 720, 25)
        );
    }

    #[test]
    fn the_output_pool_covers_the_consumer_and_the_depth() {
        assert_eq!(out_pool(None, 1), OUT_SURFACES);
        assert_eq!(out_pool(Some(2), 1), OUT_SURFACES, "a small consumer keeps the floor");
        assert_eq!(out_pool(Some(24), 1), 26, "lookahead and a pyramid at the encoder");
        assert_eq!(out_pool(Some(24), 4), 29);
        assert_eq!(out_pool(Some(u32::MAX), 4), u32::MAX);
        let nv12 = frame_info(ffi::MFX_FOURCC_NV12, 1920, 1080);
        assert_eq!(surface_bytes(&nv12), 1920 * 1088 * 3 / 2);
        let p010 = frame_info(ffi::MFX_FOURCC_P010, 3840, 2160);
        assert_eq!(surface_bytes(&p010), 3840 * 2160 * 3);
    }

    #[test]
    fn a_turned_plan_fits_the_turned_picture() {
        let size = |p: Plan| (p.width, p.height, p.place, p.rotation);
        let p = Plan::turned(1440, 1920, Layout::Nv12, t(1920, 1080, 8), 270, false).unwrap();
        assert_eq!(
            size(p),
            (1440, 1080, (0, 0, 1440, 1080), 270),
            "a portrait coded phone video shown landscape"
        );
        assert!(!p.is_identity(1440, 1080, Layout::Nv12));
        let p = Plan::turned(640, 480, Layout::Nv12, t(640, 480, 8), 90, true).unwrap();
        assert_eq!(
            size(p),
            (640, 480, (140, 0, 360, 480), 90),
            "a fixed box, the turned picture centred"
        );
        let p = Plan::turned(640, 480, Layout::Nv12, t(640, 480, 8), 180, false).unwrap();
        assert_eq!(size(p), (640, 480, (0, 0, 640, 480), 180));
        assert!(!p.is_identity(640, 480, Layout::Nv12), "a half turn still runs the VPP");
        assert_eq!(
            Plan::turned(640, 480, Layout::Nv12, t(640, 480, 8), 45, false).map(|p| p.rotation),
            Some(0)
        );
    }

    #[test]
    fn a_missing_rate_reads_as_thirty() {
        assert_eq!(rate_or_default(0, 0), (30, 1));
        assert_eq!(rate_or_default(25, 0), (30, 1));
        assert_eq!(rate_or_default(0, 1), (30, 1));
        assert_eq!(rate_or_default(30000, 1001), (30000, 1001));
    }

    #[test]
    fn a_fixed_box_is_always_the_output_and_the_picture_sits_centred() {
        let size = |p: Plan| (p.width, p.height, p.place);
        let p = plan_fixed(1920, 1080, Layout::Nv12, t(1080, 1920, 8)).unwrap();
        assert_eq!(size(p), (1080, 1920, (0, 656, 1080, 608)));
        assert!(p.bars() && !p.is_identity(1080, 1920, Layout::Nv12));
        let p = plan_fixed(1080, 1920, Layout::Nv12, t(1920, 1080, 8)).unwrap();
        assert_eq!(size(p), (1920, 1080, (656, 0, 608, 1080)));
        let p = plan_fixed(1920, 1080, Layout::Nv12, t(1920, 1080, 8)).unwrap();
        assert_eq!(size(p), (1920, 1080, (0, 0, 1920, 1080)));
        assert!(!p.bars() && p.is_identity(1920, 1080, Layout::Nv12));
        assert_eq!(
            plan_fixed(320, 180, Layout::Nv12, t(1920, 1080, 8)).map(size),
            Some((1920, 1080, (0, 0, 1920, 1080))),
            "scaled up to the box"
        );
        let p = plan_fixed(639, 361, Layout::Nv12, t(641, 361, 8)).unwrap();
        assert_eq!((p.width, p.height, p.place), (640, 360, (2, 0, 636, 360)));
        assert_eq!(plan_fixed(1920, 1080, Layout::Nv12, t(0, 1080, 8)), None);
        assert_eq!(plan_fixed(0, 1080, Layout::Nv12, t(1920, 1080, 8)), None);
        let info = out_info(plan_fixed(1280, 720, Layout::Nv12, t(640, 480, 8)).unwrap(), (25, 1));
        assert_eq!(
            (info.Width, info.Height, info.CropX, info.CropY, info.CropW, info.CropH),
            (640, 480, 0, 60, 640, 360)
        );
    }
}
