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
}

impl Plan {
    pub fn is_identity(&self, width: u32, height: u32, input: Kind) -> bool {
        self.width == width && self.height == height && self.out == input && !self.bars()
    }

    pub fn bars(&self) -> bool {
        self.place != (0, 0, self.width, self.height)
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
    Some(Plan { width: ow, height: oh, out: out_kind(input, t), place: (0, 0, ow, oh) })
}

pub fn plan_fixed(width: u32, height: u32, input: Kind, t: Target) -> Option<Plan> {
    if width == 0 || height == 0 || t.max_width == 0 || t.max_height == 0 {
        return None;
    }
    let (ow, oh) = (even(u64::from(t.max_width)), even(u64::from(t.max_height)));
    let (pw, ph) = fit(u64::from(width), u64::from(height), u64::from(ow), u64::from(oh));
    let (pw, ph) = (even(pw).min(ow), even(ph).min(oh));
    let place = (((ow - pw) / 2) & !1, ((oh - ph) / 2) & !1, pw, ph);
    Some(Plan { width: ow, height: oh, out: out_kind(input, t), place })
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
