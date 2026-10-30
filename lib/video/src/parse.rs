//! [`RawVideoParse`] cuts a raw video byte stream into one buffer per frame on the fps grid.

use std::fmt;

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Rational;

use crate::format::{
    FAMILY, FIELD_FPS, FIELD_HEIGHT, FIELD_PIXFMT, FIELD_WIDTH, PixelFormat, RAW_ANY_OFFER,
    VideoFormat,
};
use crate::geometry::frame_size;
use crate::props::{DEFAULT_FORMAT, read_dims};
use crate::testsrc::frame_pts;

// `bytes` first so a plain byte upstream such as `filesrc` links.
static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any("bytes"), RAW_ANY_OFFER[0]];
static SRC_OFFERS: [OfferDesc; 1] = [RAW_ANY_OFFER[0]];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc {
    name: "src",
    offers: &SRC_OFFERS, // concrete output format is announced at runtime
}];

/// The frame format for the parse path. A raw byte stream carries none.
static PROPS: [PropDesc; 4] = [
    PropDesc { name: "width", allowed: Constraint::Any, live: false },
    PropDesc { name: "height", allowed: Constraint::Any, live: false },
    PropDesc { name: "pixfmt", allowed: Constraint::Any, live: false },
    PropDesc { name: "fps", allowed: Constraint::Any, live: false },
];

// Cold, once per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "rawvideoparse",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(RawVideoParse::new(DEFAULT_FORMAT))),
    io: false,
    couplings: &crate::props::FORMAT_COUPLINGS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawVideoParseError {
    SlotTooSmall { frame: usize, capacity: usize },
    ZeroSizeFrame { format: VideoFormat },
}

impl fmt::Display for RawVideoParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RawVideoParseError::SlotTooSmall { frame, capacity } => write!(
                f,
                "rawvideoparse frame needs {frame} bytes but pool slots hold {capacity}, \
                 raise the pipeline pool slot size"
            ),
            RawVideoParseError::ZeroSizeFrame { format } => write!(
                f,
                "rawvideoparse frame format {}x{} {} is zero bytes",
                format.width,
                format.height,
                format.pixfmt.caps_name()
            ),
        }
    }
}

impl std::error::Error for RawVideoParseError {}

pub struct RawVideoParse {
    format: VideoFormat,
    frame_bytes: usize,
    carry: Vec<u8>,
    frames_out: u64,
    presets: [(&'static str, ValueDesc); 4],
}

impl RawVideoParse {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    // Cold constructor.
    #[allow(clippy::disallowed_methods)]
    pub fn new(format: VideoFormat) -> Self {
        Self {
            frame_bytes: frame_size(format.pixfmt, format.width, format.height),
            format,
            carry: Vec::new(),
            frames_out: 0,
            presets: crate::props::presets_of(format),
        }
    }

    pub fn with_dims(width: u32, height: u32, pixfmt: PixelFormat, fps: Rational) -> Self {
        Self::new(VideoFormat::new(width, height, pixfmt, fps))
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

    fn emit_frame(&mut self, ctx: &mut Ctx, frame: &[u8]) -> Result<(), Error> {
        debug_assert_eq!(frame.len(), self.frame_bytes);
        let mut buf = ctx.alloc();
        let capacity = buf.memory.capacity();
        if capacity < self.frame_bytes {
            let error = RawVideoParseError::SlotTooSmall { frame: self.frame_bytes, capacity };
            return Err(Error::element_failed(ctx.element(), error));
        }
        buf.memory.as_mut_full()[..self.frame_bytes].copy_from_slice(frame);
        buf.memory.set_len(self.frame_bytes);
        let idx = self.frames_out;
        buf.pts = frame_pts(self.format.fps, idx);
        buf.duration = frame_pts(self.format.fps, idx + 1).saturating_sub(buf.pts);
        self.frames_out += 1;
        ctx.out(Self::OUT).push(buf);
        Ok(())
    }

    fn feed(&mut self, ctx: &mut Ctx, bytes: &[u8]) -> Result<(), Error> {
        if self.frame_bytes == 0 {
            let error = RawVideoParseError::ZeroSizeFrame { format: self.format };
            return Err(Error::element_failed(ctx.element(), error));
        }
        // Fast path, nothing carried, no join copy.
        if self.carry.is_empty() {
            let whole = bytes.len() - (bytes.len() % self.frame_bytes);
            let mut off = 0;
            while off < whole {
                self.emit_frame(ctx, &bytes[off..off + self.frame_bytes])?;
                off += self.frame_bytes;
            }
            self.carry.extend_from_slice(&bytes[whole..]);
            return Ok(());
        }
        let mut joined = std::mem::take(&mut self.carry);
        joined.extend_from_slice(bytes);
        let whole = joined.len() - (joined.len() % self.frame_bytes);
        let mut off = 0;
        while off < whole {
            self.emit_frame(ctx, &joined[off..off + self.frame_bytes])?;
            off += self.frame_bytes;
        }
        self.carry.clear();
        self.carry.extend_from_slice(&joined[whole..]);
        Ok(())
    }
}

impl Element for RawVideoParse {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn presets(&self) -> &[(&'static str, ValueDesc)] {
        &self.presets
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.format = read_dims(ctx, self.format);
        self.frame_bytes = frame_size(self.format.pixfmt, self.format.width, self.format.height);
        self.carry.clear();
        self.frames_out = 0;
        let f = self.format;
        log!(
            &*ctx,
            Level::Info,
            "format",
            width = f.width,
            height = f.height,
            pixfmt = f.pixfmt.caps_name(),
            frame_bytes = self.frame_bytes,
        );
        self.announce(ctx);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.feed(ctx, buf.memory.data())?;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        // Drop the partial frame so output stays frame aligned across a seek.
        match event {
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", carry = self.carry.len());
                self.carry.clear();
            }
            Event::Eos => log!(&*ctx, Level::Info, "eos", frames = self.frames_out),
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        // A leftover carry is a ragged final frame, dropped.
        if !self.carry.is_empty() {
            log!(&*ctx, Level::Warn, "ragged_tail", bytes = self.carry.len());
        }
        self.carry.clear();
        self.frames_out = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_harness::Harness;

    #[test]
    fn frame_bytes_matches_geometry() {
        let p = RawVideoParse::with_dims(16, 16, PixelFormat::I420, Rational::new(25, 1));
        assert_eq!(p.frame_bytes, 256 + 64 + 64);
        assert_eq!(p.format().pixfmt, PixelFormat::I420);
    }

    fn failure(width: u32, height: u32, bytes: &[u8]) -> RawVideoParseError {
        let parse =
            RawVideoParse::with_dims(width, height, PixelFormat::I420, Rational::new(25, 1));
        let mut h = Harness::with_pool(parse, 64, 4);
        let buf = h.alloc(bytes);
        let err = h.push(buf).expect_err("the parser fails");
        let Error::ElementFailed { source, .. } = err else { panic!("typed element failure") };
        *source.downcast_ref::<RawVideoParseError>().expect("the parser's own error")
    }

    #[test]
    fn a_frame_larger_than_a_pool_slot_is_a_typed_failure() {
        let e = failure(16, 16, &[0; 384]);
        assert_eq!(e, RawVideoParseError::SlotTooSmall { frame: 384, capacity: 64 });
        let s = format!("{e}");
        assert!(s.contains("384 bytes") && s.contains("hold 64"), "{s}");
    }

    #[test]
    fn a_zero_size_frame_format_is_a_typed_failure() {
        let e = failure(0, 0, &[0; 8]);
        let RawVideoParseError::ZeroSizeFrame { format } = e else { panic!("zero size, got {e}") };
        assert_eq!((format.width, format.height, format.pixfmt), (0, 0, PixelFormat::I420));
        assert_eq!(format!("{e}"), "rawvideoparse frame format 0x0 i420 is zero bytes");
    }
}
