//! [`VideoTestSrc`], an active source of seeded deterministic frames on the fps grid, then EOS.

use std::fmt;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{Constraint, ValueDesc};
use profluens_core::id::SrcPad;
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::{Rational, Timestamp};

use crate::format::{
    FAMILY, FIELD_FPS, FIELD_HEIGHT, FIELD_PIXFMT, FIELD_WIDTH, PixelFormat, RAW_ANY_OFFER,
    VideoFormat,
};
use crate::geometry::frame_size;
use crate::props::{DEFAULT_FORMAT, read_dims, read_u64};

static PADS: [PadDesc; 1] = [PadDesc {
    name: "src",
    offers: &RAW_ANY_OFFER, // the concrete format is announced at runtime
}];

/// The construction parameters for the parse path.
static PROPS: [PropDesc; 6] = [
    PropDesc { name: "width", allowed: Constraint::Any, live: false },
    PropDesc { name: "height", allowed: Constraint::Any, live: false },
    PropDesc { name: "pixfmt", allowed: Constraint::Any, live: false },
    PropDesc { name: "fps", allowed: Constraint::Any, live: false },
    PropDesc { name: "frames", allowed: Constraint::Any, live: false },
    PropDesc { name: "seed", allowed: Constraint::Any, live: false },
];

// Cold, once per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "videotestsrc",
    sinks: &[],
    srcs: &PADS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(VideoTestSrc::new(DEFAULT_FORMAT, 0, 90))),
    io: false,
    couplings: &crate::props::FORMAT_COUPLINGS,
};

/// The byte at offset `pos` of frame `frame_index` for `seed`. Pure, so a sink can regenerate any
/// frame.
#[inline]
pub fn frame_pattern_byte(seed: u64, frame_index: u64, pos: usize) -> u8 {
    // SplitMix-style avalanche over the three inputs.
    let mut x = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(frame_index.wrapping_mul(0xD1B5_4A32_D192_ED03))
        .wrapping_add(pos as u64);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    (x >> 24) as u8
}

/// The PTS of frame `i` on an `fps` grid, `round(i * den / num * 1e9)` in i128 so it never drifts.
/// [`Timestamp::NONE`] for an invalid rate.
pub fn frame_pts(fps: Rational, i: u64) -> Timestamp {
    if fps.den == 0 || fps.num <= 0 {
        return Timestamp::NONE;
    }
    let num = fps.num as i128;
    let den = fps.den.unsigned_abs() as i128;
    let numer = i as i128 * den * 1_000_000_000i128;
    let ns = (numer + num / 2) / num;
    Timestamp::from_nanos(ns as u64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoTestSrcError {
    SlotTooSmall { frame: usize, capacity: usize },
}

impl fmt::Display for VideoTestSrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoTestSrcError::SlotTooSmall { frame, capacity } => write!(
                f,
                "videotestsrc frame needs {frame} bytes but pool slots hold {capacity}, \
                 raise the pipeline pool slot size"
            ),
        }
    }
}

impl std::error::Error for VideoTestSrcError {}

/// A deterministic raw video source of `count` frames seeded by `seed`.
pub struct VideoTestSrc {
    format: VideoFormat,
    seed: u64,
    count: u64,
    produced: u64,
    frame_bytes: usize,
    presets: [(&'static str, ValueDesc); 4],
}

impl VideoTestSrc {
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new(format: VideoFormat, seed: u64, count: u64) -> Self {
        Self {
            frame_bytes: frame_size(format.pixfmt, format.width, format.height),
            format,
            seed,
            count,
            produced: 0,
            presets: crate::props::presets_of(format),
        }
    }

    pub fn with_dims(
        width: u32,
        height: u32,
        pixfmt: PixelFormat,
        fps: Rational,
        seed: u64,
        count: u64,
    ) -> Self {
        Self::new(VideoFormat::new(width, height, pixfmt, fps), seed, count)
    }

    pub fn format(&self) -> VideoFormat {
        self.format
    }

    fn announce(&self, ctx: &mut Ctx) {
        let f = self.format;
        ctx.announce_format(
            Self::OUT,
            FAMILY,
            &[
                (FIELD_WIDTH, ValueDesc::Int(f.width as i64)),
                (FIELD_HEIGHT, ValueDesc::Int(f.height as i64)),
                (FIELD_PIXFMT, ValueDesc::Id(f.pixfmt.caps_name())),
                (FIELD_FPS, ValueDesc::Rat(f.fps.num, f.fps.den)),
            ],
        );
    }
}

impl Element for VideoTestSrc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn presets(&self) -> &[(&'static str, ValueDesc)] {
        &self.presets
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.format = read_dims(ctx, self.format);
        self.count = read_u64(ctx, "frames", self.count);
        self.seed = read_u64(ctx, "seed", self.seed);
        self.frame_bytes = frame_size(self.format.pixfmt, self.format.width, self.format.height);
        self.produced = 0;
        let f = self.format;
        log!(
            &*ctx,
            Level::Info,
            "format",
            width = f.width,
            height = f.height,
            pixfmt = f.pixfmt.caps_name(),
            frames = self.count,
        );
        self.announce(ctx);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.produced >= self.count {
            return Ok(Flow::Eos);
        }
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok), // pool full, backpressure
        };
        let capacity = buf.memory.capacity();
        if capacity < self.frame_bytes {
            let error = VideoTestSrcError::SlotTooSmall { frame: self.frame_bytes, capacity };
            return Err(Error::element_failed(ctx.element(), error));
        }

        let idx = self.produced;
        let dst = &mut buf.memory.as_mut_full()[..self.frame_bytes];
        for (pos, b) in dst.iter_mut().enumerate() {
            *b = frame_pattern_byte(self.seed, idx, pos);
        }
        buf.memory.set_len(self.frame_bytes);
        buf.pts = frame_pts(self.format.fps, idx);
        buf.duration = frame_pts(self.format.fps, idx + 1).saturating_sub(buf.pts);
        self.produced += 1;
        ctx.out(Self::OUT).push(buf);
        if self.produced == self.count {
            log!(&*ctx, Level::Info, "eos", produced = self.produced);
        }
        Ok(Flow::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_harness::Harness;

    #[test]
    fn a_frame_larger_than_a_pool_slot_is_a_typed_failure() {
        let src = VideoTestSrc::with_dims(16, 16, PixelFormat::I420, Rational::new(25, 1), 1, 3);
        let mut h = Harness::with_pool(src, 64, 4);
        let err = h.crank().expect_err("a 384 byte frame cannot fit a 64 byte slot");
        let Error::ElementFailed { source, .. } = err else { panic!("typed element failure") };
        let e = source.downcast_ref::<VideoTestSrcError>().expect("the source's own error");
        assert_eq!(*e, VideoTestSrcError::SlotTooSmall { frame: 384, capacity: 64 });
        let s = format!("{e}");
        assert!(s.contains("384 bytes") && s.contains("hold 64"), "{s}");
        assert!(h.drain_outputs().is_empty(), "nothing truncated got out");
    }

    #[test]
    fn frame_pts_is_exact_on_the_grid() {
        let fps = Rational::new(30, 1);
        assert_eq!(frame_pts(fps, 0), Timestamp::ZERO);
        assert_eq!(frame_pts(fps, 30), Timestamp::from_secs(1));
        assert_eq!(frame_pts(fps, 1), Timestamp::from_nanos(33_333_333));

        // 29.97 fps, the drift-prone case. Frame 30000 is exactly 1001 s.
        let ntsc = Rational::new(30000, 1001);
        assert_eq!(frame_pts(ntsc, 30000), Timestamp::from_secs(1001));

        assert!(frame_pts(Rational::new(0, 0), 5).is_none());
    }

    #[test]
    fn pattern_is_deterministic_and_seed_sensitive() {
        assert_eq!(frame_pattern_byte(1, 2, 3), frame_pattern_byte(1, 2, 3));
        let a: Vec<u8> = (0..64).map(|p| frame_pattern_byte(1, 0, p)).collect();
        let b: Vec<u8> = (0..64).map(|p| frame_pattern_byte(2, 0, p)).collect();
        assert_ne!(a, b, "different seeds produce different frames");
        let f0: Vec<u8> = (0..64).map(|p| frame_pattern_byte(1, 0, p)).collect();
        let f1: Vec<u8> = (0..64).map(|p| frame_pattern_byte(1, 1, p)).collect();
        assert_ne!(f0, f1, "different frame indices differ");
    }
}
