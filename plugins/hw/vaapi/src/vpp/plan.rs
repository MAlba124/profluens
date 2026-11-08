//! What a frame becomes, from its size and layout and what the far end takes. No device.

use crate::vpp::format::{Kind, NV12, P010};

/// What the far end takes. A zero bound is no bound.
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

pub type Rect = (u32, u32, u32, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub width: u32,
    pub height: u32,
    pub out: Kind,
    pub place: Rect,
    /// Degrees clockwise the picture is turned, 0, 90, 180 or 270. The sides above are the
    /// turned picture's.
    pub rotation: u16,
}

impl Plan {
    pub fn is_identity(&self, width: u32, height: u32, input: Kind) -> bool {
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
        input: Kind,
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

    /// The `rotation_state` of the turn, `VA_ROTATION_*`.
    pub fn va_rotation(&self) -> u32 {
        u32::from(self.rotation / 90)
    }

    pub fn bars(&self) -> bool {
        self.place != (0, 0, self.width, self.height)
    }
}

/// The sides a `width` x `height` picture of pixel aspect `par` is shown at, its short side
/// grown as players stretch it. A square or unstated ratio leaves it.
pub fn shown(width: u32, height: u32, par: (u32, u32)) -> (u32, u32) {
    let (n, d) = (u64::from(par.0), u64::from(par.1));
    let grow =
        |v: u32, a: u64, b: u64| ((u64::from(v) * a + b / 2) / b).min(u64::from(u32::MAX)) as u32;
    if n == 0 || d == 0 || n == d {
        (width, height)
    } else if n > d {
        (grow(width, n, d), height)
    } else {
        (width, grow(height, d, n))
    }
}

fn out_kind(input: Kind, t: Target) -> Kind {
    if input.depth.min(t.depth) > 8 { P010 } else { NV12 }
}

fn fit(w: u64, h: u64, bw: u64, bh: u64) -> (u64, u64) {
    if bw * h <= bh * w { (bw, (h * bw + w / 2) / w) } else { ((w * bh + h / 2) / h, bh) }
}

fn even(v: u64) -> u32 {
    (v & !1).max(2) as u32
}

pub fn plan(width: u32, height: u32, input: Kind, t: Target) -> Option<Plan> {
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
        out: out_kind(input, t),
        place: (0, 0, ow, oh),
        rotation: 0,
    })
}

pub fn plan_fixed(width: u32, height: u32, input: Kind, t: Target) -> Option<Plan> {
    if width == 0 || height == 0 || t.max_width == 0 || t.max_height == 0 {
        return None;
    }
    let (ow, oh) = (even(u64::from(t.max_width)), even(u64::from(t.max_height)));
    let (pw, ph) = fit(u64::from(width), u64::from(height), u64::from(ow), u64::from(oh));
    let (pw, ph) = (even(pw).min(ow), even(ph).min(oh));
    // a side short of the box by its rounding fills it, the box is the free plan of the shape
    let (pw, ph) = if ow - pw <= 2 && oh - ph <= 2 { (ow, oh) } else { (pw, ph) };
    let place = (((ow - pw) / 2) & !1, ((oh - ph) / 2) & !1, pw, ph);
    Some(Plan { width: ow, height: oh, out: out_kind(input, t), place, rotation: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vpp::format::{P012, P444, Y410};

    fn t(max_width: u32, max_height: u32, depth: u8) -> Target {
        Target { max_width, max_height, depth }
    }

    #[test]
    fn fits_the_box_keeping_the_aspect() {
        assert_eq!(
            plan(3840, 2160, NV12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((1920, 1080))
        );
        assert_eq!(
            plan(3840, 1606, NV12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((1920, 802))
        );
        assert_eq!(
            plan(1080, 1920, NV12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((608, 1080))
        );
        assert_eq!(
            plan(3840, 2160, NV12, t(0, 720, 8)).map(|p| (p.width, p.height)),
            Some((1280, 720))
        );
    }

    #[test]
    fn a_turned_plan_fits_the_turned_picture() {
        let p = Plan::turned(1440, 1920, NV12, t(1920, 1080, 8), 270, false).unwrap();
        assert_eq!((p.width, p.height, p.rotation, p.va_rotation()), (1440, 1080, 270, 3));
        assert!(!p.is_identity(1440, 1080, NV12));
        let p = Plan::turned(640, 480, NV12, t(640, 480, 8), 90, true).unwrap();
        assert_eq!(
            (p.width, p.height, p.place, p.va_rotation()),
            (640, 480, (140, 0, 360, 480), 1)
        );
        assert_eq!(
            Plan::turned(64, 64, NV12, t(0, 0, 8), 30, false).map(|p| p.va_rotation()),
            Some(0)
        );
    }

    #[test]
    fn a_pixel_aspect_grows_the_short_side() {
        // NTSC DVD 16:9 and 4:3, PAL 16:9, HDV 1440 wide
        assert_eq!(shown(720, 480, (32, 27)), (853, 480));
        assert_eq!(shown(720, 480, (8, 9)), (720, 540));
        assert_eq!(shown(720, 576, (64, 45)), (1024, 576));
        assert_eq!(shown(1440, 1080, (4, 3)), (1920, 1080));
        assert_eq!(shown(720, 480, (1, 1)), (720, 480));
        assert_eq!(shown(720, 480, (0, 0)), (720, 480), "unstated");
        // a shown size is fitted as any other, then stays within it
        let (w, h) = shown(720, 480, (32, 27));
        assert_eq!(
            plan(w, h, NV12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((852, 480))
        );
        assert_eq!(
            plan_fixed(w, h, NV12, t(852, 480, 8)).map(|p| p.place),
            Some((0, 0, 852, 480)),
            "no bars"
        );
    }

    #[test]
    fn never_upscales_and_stays_even() {
        assert_eq!(
            plan(1280, 720, NV12, t(1920, 1080, 8)).map(|p| (p.width, p.height)),
            Some((1280, 720))
        );
        assert_eq!(
            plan(1919, 1079, NV12, Target::default()).map(|p| (p.width, p.height)),
            Some((1918, 1078))
        );
        assert_eq!(plan(1, 1, NV12, Target::default()).map(|p| (p.width, p.height)), Some((2, 2)));
        assert_eq!(plan(0, 1080, NV12, Target::default()), None);
    }

    #[test]
    fn depth_and_chroma_come_down_to_what_is_taken() {
        assert_eq!(plan(64, 64, P012, t(0, 0, 10)).map(|p| p.out), Some(P010), "12 bits to 10");
        assert_eq!(plan(64, 64, Y410, t(0, 0, 10)).map(|p| p.out), Some(P010), "4:4:4 to 4:2:0");
        assert_eq!(plan(64, 64, P444, t(0, 0, 10)).map(|p| p.out), Some(NV12), "8 bits stay 8");
        assert_eq!(plan(64, 64, P010, t(0, 0, 8)).map(|p| p.out), Some(NV12), "10 bits to 8");
    }

    #[test]
    fn a_fixed_box_is_always_the_output_and_the_picture_sits_centred() {
        let size = |p: Plan| (p.width, p.height, p.place);
        let p = plan_fixed(1920, 1080, NV12, t(1080, 1920, 8)).expect("a plan");
        assert_eq!(size(p), (1080, 1920, (0, 656, 1080, 608)));
        assert!(p.bars() && !p.is_identity(1080, 1920, NV12));
        let p = plan_fixed(1080, 1920, NV12, t(1920, 1080, 8)).expect("a plan");
        assert_eq!(size(p), (1920, 1080, (656, 0, 608, 1080)));
        let p = plan_fixed(1920, 1080, NV12, t(1920, 1080, 8)).expect("a plan");
        assert_eq!(size(p), (1920, 1080, (0, 0, 1920, 1080)));
        assert!(!p.bars() && p.is_identity(1920, 1080, NV12));
        assert_eq!(
            plan_fixed(320, 180, NV12, t(1920, 1080, 8)).map(size),
            Some((1920, 1080, (0, 0, 1920, 1080)))
        );
        assert_eq!(
            plan(320, 180, NV12, t(1920, 1080, 8)).map(size),
            Some((320, 180, (0, 0, 320, 180)))
        );
    }

    #[test]
    fn a_fixed_box_keeps_every_edge_even() {
        let p = plan_fixed(639, 361, NV12, t(641, 361, 8)).expect("a plan");
        assert_eq!((p.width, p.height, p.place), (640, 360, (2, 0, 636, 360)));
        let p = plan_fixed(640, 1138, NV12, t(640, 360, 8)).expect("a plan");
        assert_eq!((p.width, p.height), (640, 360));
        let (x, y, w, h) = p.place;
        assert!(
            [x, y, w, h].iter().all(|v| v % 2 == 0) && x + w <= 640 && y + h <= 360,
            "{:?}",
            p.place
        );
        assert_eq!(plan_fixed(1, 1, NV12, t(2, 2, 8)).map(|p| p.place), Some((0, 0, 2, 2)));
    }

    #[test]
    fn a_fixed_box_needs_both_bounds_and_a_frame() {
        assert_eq!(plan_fixed(1920, 1080, NV12, t(0, 1080, 8)), None);
        assert_eq!(plan_fixed(1920, 1080, NV12, t(1920, 0, 8)), None);
        assert_eq!(plan_fixed(0, 1080, NV12, t(1920, 1080, 8)), None);
        assert_eq!(
            plan_fixed(64, 64, P010, t(64, 64, 8)).map(|p| p.out),
            Some(NV12),
            "10 bits to 8 as the bounded plan"
        );
    }

    #[test]
    fn a_frame_that_fits_is_left_alone() {
        let p = plan(1920, 1080, NV12, t(1920, 1080, 8));
        assert!(p.is_some_and(|p| p.is_identity(1920, 1080, NV12)));
        let p = plan(1920, 1080, P010, t(1920, 1080, 8));
        assert!(p.is_some_and(|p| !p.is_identity(1920, 1080, P010)), "10 bits to 8 is work");
    }
}
