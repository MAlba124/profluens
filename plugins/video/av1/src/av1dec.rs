//! `av1dec`, the AV1 side of the shared `VideoDec` shell.

use std::fmt;

use profluens_core::ctx::Ctx;
use profluens_core::element::{ElementDesc, PadDesc, PropDesc};
use profluens_core::format::{Constraint, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;
use profluens_core::time::Timestamp;
use profluens_video::FAMILY;
use profluens_video::decoder::{self, Codec, Held, Out, PlaneRef, Scan, Sent, VideoDec};

use crate::dav1d::{self, Dav1dError, Decoder, OpenError, Picture, PictureSupply, Settings};

static PIXFMTS: [ValueDesc; 5] = [
    ValueDesc::Id("i420"),
    ValueDesc::Id("gray8"),
    ValueDesc::Id("p010"),
    ValueDesc::Id("i422"),
    ValueDesc::Id("i422_10"),
];
static SRC_FIELDS: [FieldDesc; 13] = decoder::src_fields_stating_color(&PIXFMTS);
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "av1", fields: &decoder::COLOR_FIELDS }];
static SINKS: [PadDesc; 1] = decoder::sink_pad(&SINK_OFFERS);
static SRCS: [PadDesc; 1] = decoder::src_pad(&SRC_OFFERS);

static PROPS: [PropDesc; 2] = [
    PropDesc { name: "threads", allowed: Constraint::Any, live: false },
    PropDesc { name: "frame_delay", allowed: Constraint::Any, live: false },
];

// One-time. `make_default` boxes an instance at pipeline construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("av1dec", &SINKS, &SRCS, &PROPS, || {
        Box::new(Av1Dec::new())
    }));

pub const DEFAULT_THREADS: u32 = 4;

/// dav1d's own choice for the thread count, `ceil(sqrt(4))`.
pub const DEFAULT_FRAME_DELAY: u32 = 0;

/// The most pixels a frame may have, 35,651,584 (8192x4352).
pub const FRAME_SIZE_LIMIT: u32 = 8192 * 4352;

/// Decodes AV1 temporal units to packed 8-bit `i420` or `gray8`, or 10-bit `p010`.
pub type Av1Dec = VideoDec<Av1>;

#[derive(Debug, PartialEq, Eq)]
pub enum Av1DecError {
    Open(OpenError),
    Unit(Dav1dError),
    Shape { width: u32, height: u32, bit_depth: u8, layout: &'static str },
    HeldFull { slots: usize },
}

impl fmt::Display for Av1DecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Av1DecError::Open(e) => write!(f, "av1 decoder not opened, {e}"),
            Av1DecError::Unit(e) => write!(f, "av1 temporal unit dropped, {e}"),
            Av1DecError::Shape { width, height, bit_depth, layout } => write!(
                f,
                "av1 {width}x{height} {bit_depth}-bit {layout} is not decoded here, only 8-bit \
                 4:2:0 and monochrome and 10-bit 4:2:0 are"
            ),
            Av1DecError::HeldFull { slots } => {
                write!(f, "av1 frame dropped, all {slots} held picture slots are taken")
            }
        }
    }
}

impl std::error::Error for Av1DecError {}

pub struct Av1 {
    held: Vec<Option<Picture>>,
    decoder: Option<Decoder>,
    supply: PictureSupply,
    settings: Settings,
    open_failed: bool,
}

impl Av1 {
    fn ensure_open(&mut self, out: &mut Out<'_>) -> bool {
        if self.decoder.is_some() {
            return true;
        }
        if self.open_failed {
            return false;
        }
        match Decoder::open(&self.settings, &self.supply) {
            Ok(decoder) => {
                let slots = decoder.frame_delay() as usize + 2;
                // Once, at the first unit after an open.
                #[allow(clippy::disallowed_methods)]
                let held = std::iter::repeat_with(|| None).take(slots).collect();
                self.held = held;
                self.decoder = Some(decoder);
                let s = &self.settings;
                log!(
                    out.ctx(),
                    Level::Info,
                    "open",
                    threads = s.threads,
                    frame_delay = s.frame_delay,
                    held = slots,
                );
                true
            }
            Err(e) => {
                self.open_failed = true;
                out.warn(Av1DecError::Open(e));
                false
            }
        }
    }

    fn top_up(&self, out: &mut Out<'_>) -> bool {
        let want = self.supply.wanted();
        while want > 0 && self.supply.spare() < dav1d::SPARE {
            match out.frame_memory(want) {
                Some(m) => self.supply.feed(m),
                None => {
                    out.starve();
                    return false;
                }
            }
        }
        true
    }

    fn place(
        held: &mut [Option<Picture>],
        supply: &PictureSupply,
        picture: Picture,
        out: &mut Out<'_>,
    ) {
        let (width, height) = (picture.width(), picture.height());
        let Some((pixfmt, layout)) = picture.pixfmt(out.takes()) else {
            out.fail(Av1DecError::Shape {
                width,
                height,
                bit_depth: picture.bit_depth(),
                layout: picture.layout_name(),
            });
            return;
        };
        let pts = from_dav1d(picture.timestamp());
        let duration = from_dav1d(picture.duration());
        if let Some(bytes) = picture.bytes() {
            supply.want(bytes);
        }
        if out.decode_only(pts) {
            return;
        }
        let color = picture.color();
        if let Some((memory, offsets, strides)) = picture.placement() {
            out.push_sent(Sent {
                memory,
                width,
                height,
                pixfmt,
                layout,
                offsets,
                strides,
                pts,
                duration,
                color,
            });
            return;
        }
        let Some(handle) = held.iter().position(Option::is_none) else {
            out.warn(Av1DecError::HeldFull { slots: held.len() });
            return;
        };
        held[handle] = Some(picture);
        out.push_held(Held {
            width,
            height,
            pixfmt,
            pts,
            duration,
            handle: handle as u32,
            color,
            layout,
        });
    }
}

fn to_dav1d(t: Timestamp, none: i64) -> i64 {
    t.nanos().map_or(none, |n| n as i64)
}

fn from_dav1d(v: i64) -> Timestamp {
    if v < 0 { Timestamp::NONE } else { Timestamp::from_nanos(v as u64) }
}

impl Codec for Av1 {
    const DESC: &'static ElementDesc = &DESC;

    // Once, sized at the first open.
    #[allow(clippy::disallowed_methods)]
    fn new() -> Self {
        Self {
            held: Vec::new(),
            decoder: None,
            supply: PictureSupply::new(),
            settings: Settings {
                threads: DEFAULT_THREADS,
                frame_delay: DEFAULT_FRAME_DELAY,
                frame_size_limit: FRAME_SIZE_LIMIT,
            },
            open_failed: false,
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let knob = |name: &str, default: u32, most: u32| match ctx.prop(name) {
            Some(Value::Int(v)) if v >= 0 => (v as u64).min(most as u64) as u32,
            _ => default,
        };
        let settings = Settings {
            threads: knob("threads", DEFAULT_THREADS, dav1d::MAX_THREADS),
            frame_delay: knob("frame_delay", DEFAULT_FRAME_DELAY, dav1d::MAX_FRAME_DELAY),
            frame_size_limit: FRAME_SIZE_LIMIT,
        };
        if settings != self.settings {
            log!(
                ctx,
                Level::Debug,
                "reopen",
                threads = settings.threads,
                frame_delay = settings.frame_delay
            );
            self.held.iter_mut().for_each(|h| *h = None);
            self.decoder = None;
        }
        self.settings = settings;
        self.open_failed = false;
    }

    fn scan(&mut self, unit: &[u8], scan: &mut Scan<'_>) {
        pf_hdr::av1::scan(unit, scan);
    }

    fn reset(&mut self) {
        self.held.iter_mut().for_each(|h| *h = None);
        if let Some(d) = &mut self.decoder {
            d.flush();
        }
        self.open_failed = false;
    }

    fn decode(&mut self, unit: &[u8], pts: Timestamp, duration: Timestamp, out: &mut Out<'_>) {
        if !self.ensure_open(out) || !self.top_up(out) {
            return;
        }
        let Av1 { held, decoder: Some(decoder), supply, .. } = self else {
            return;
        };
        let (timestamp, duration) = (to_dav1d(pts, i64::MIN), to_dav1d(duration, -1));
        if let Err(e) =
            decoder.decode(unit, timestamp, duration, |p| Self::place(held, supply, p, out))
        {
            out.warn(Av1DecError::Unit(e));
        }
    }

    fn decode_memory(
        &mut self,
        unit: &Memory,
        pts: Timestamp,
        duration: Timestamp,
        out: &mut Out<'_>,
    ) {
        if !self.ensure_open(out) || !self.top_up(out) {
            return;
        }
        let Av1 { held, decoder: Some(decoder), supply, .. } = self else {
            return;
        };
        let (timestamp, duration) = (to_dav1d(pts, i64::MIN), to_dav1d(duration, -1));
        if let Err(e) =
            decoder.decode_memory(unit, timestamp, duration, |p| Self::place(held, supply, p, out))
        {
            out.warn(Av1DecError::Unit(e));
        }
    }

    fn finish(&mut self, out: &mut Out<'_>) {
        let Av1 { held, decoder: Some(decoder), supply, .. } = self else {
            return;
        };
        if let Err(e) = decoder.drain(|p| Self::place(held, supply, p, out)) {
            out.warn(Av1DecError::Unit(e));
        }
    }

    fn held_planes(&self, handle: u32) -> [PlaneRef<'_>; 3] {
        match self.held.get(handle as usize) {
            Some(Some(p)) => [p.plane(0), p.plane(1), p.plane(2)],
            _ => [PlaneRef::EMPTY; 3],
        }
    }

    fn release_held(&mut self, handle: u32) {
        if let Some(slot) = self.held.get_mut(handle as usize) {
            *slot = None;
        }
    }

    fn for_each_held(&self, f: &mut dyn FnMut(&Memory)) {
        self.supply.for_each_held(f);
    }
}
