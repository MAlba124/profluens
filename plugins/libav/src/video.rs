//! libavcodec's H.264, H.265, VP8, VP9, MPEG-1/2, MPEG-4 Part 2, Sorenson Spark, VP6, VP7 and
//! Theora decoders in the shared [`VideoDec`] shell. VP6 in FLV (`vp6f`, `vp6a`) leads each frame
//! with the byte that crops its 16-pixel grid, libavcodec takes the first as extradata. An MPEG-4
//! Part 2 configuration unit (a demuxer's DSI or CodecPrivate) is extradata too, see
//! [`config_unit`]. The decoder opens at the first unit, never in [`Codec::new`], which builds
//! throwaway instances.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;
use std::sync::Arc;

use profluens_core::ctx::Ctx;
use profluens_core::element::{Element, ElementDesc, PadDesc, PropDesc};
use profluens_core::format::{Constraint, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;
use profluens_core::time::Timestamp;
use profluens_video::FAMILY;
use profluens_video::decoder::{self, Codec, Held, Out, PlaneRef, Scan, Sent, VideoDec};
use profluens_video::gpuframe::{DmabufFormat, GpuFrameChannel};
use profluens_video::h264parse;

use crate::lav::{self, Decoder, Frame, LavError, Received, Shape, Supply};

/// What [`Frame::place`] puts out, as the link takes it.
static PIXFMTS: [ValueDesc; 5] = [
    ValueDesc::Id("i420"),
    ValueDesc::Id("gray8"),
    ValueDesc::Id("p010"),
    ValueDesc::Id("i422"),
    ValueDesc::Id("i422_10"),
];
static SRC_FIELDS: [FieldDesc; 13] = decoder::src_fields_stating_color(&PIXFMTS);
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }];
static SRCS: [PadDesc; 1] = decoder::src_pad(&SRC_OFFERS);

static H264_SINK: [OfferDesc; 1] =
    [OfferDesc { family: "h264/annexb", fields: &decoder::COLOR_FIELDS }];
static H265_SINK: [OfferDesc; 1] =
    [OfferDesc { family: "h265/annexb", fields: &decoder::COLOR_FIELDS }];
static VP8_SINK: [OfferDesc; 1] = [OfferDesc { family: "vp8", fields: &decoder::COLOR_FIELDS }];
static VP9_SINK: [OfferDesc; 1] = [OfferDesc { family: "vp9", fields: &decoder::COLOR_FIELDS }];
/// FFmpeg's mpeg2video decoder reads an MPEG-1 sequence header as MPEG-1 (mpeg12dec.c
/// mpeg1_decode_sequence), so MPEG-1 video needs no decoder of its own.
static MPEG2_SINK: [OfferDesc; 2] = [
    OfferDesc { family: "mpeg2", fields: &decoder::COLOR_FIELDS },
    OfferDesc { family: "mpeg1", fields: &decoder::COLOR_FIELDS },
];
static MPEG4_SINK: [OfferDesc; 1] =
    [OfferDesc { family: "mpeg4/asp", fields: &decoder::COLOR_FIELDS }];
static H264_SINKS: [PadDesc; 1] = decoder::sink_pad(&H264_SINK);
static H265_SINKS: [PadDesc; 1] = decoder::sink_pad(&H265_SINK);
static VP8_SINKS: [PadDesc; 1] = decoder::sink_pad(&VP8_SINK);
static VP9_SINKS: [PadDesc; 1] = decoder::sink_pad(&VP9_SINK);
static MPEG2_SINKS: [PadDesc; 1] = decoder::sink_pad(&MPEG2_SINK);
static MPEG4_SINKS: [PadDesc; 1] = decoder::sink_pad(&MPEG4_SINK);
static FLV1_SINK: [OfferDesc; 1] = [OfferDesc { family: "flv1", fields: &decoder::COLOR_FIELDS }];
static FLV1_SINKS: [PadDesc; 1] = decoder::sink_pad(&FLV1_SINK);
static VP6_SINK: [OfferDesc; 1] = [OfferDesc { family: "vp6", fields: &decoder::COLOR_FIELDS }];
static VP6_SINKS: [PadDesc; 1] = decoder::sink_pad(&VP6_SINK);
static VP6F_SINK: [OfferDesc; 1] = [OfferDesc { family: "vp6f", fields: &decoder::COLOR_FIELDS }];
static VP6F_SINKS: [PadDesc; 1] = decoder::sink_pad(&VP6F_SINK);
static VP6A_SINK: [OfferDesc; 1] = [OfferDesc { family: "vp6a", fields: &decoder::COLOR_FIELDS }];
static VP6A_SINKS: [PadDesc; 1] = decoder::sink_pad(&VP6A_SINK);
static VP7_SINK: [OfferDesc; 1] = [OfferDesc { family: "vp7", fields: &decoder::COLOR_FIELDS }];
static VP7_SINKS: [PadDesc; 1] = decoder::sink_pad(&VP7_SINK);
static THEORA_SINK: [OfferDesc; 1] =
    [OfferDesc { family: "theora", fields: &decoder::COLOR_FIELDS }];
static THEORA_SINKS: [PadDesc; 1] = decoder::sink_pad(&THEORA_SINK);

/// `threads` past 1 runs libavcodec's frame threads where the codec has them. Each picture then
/// comes threads - 1 units later and is copied out of libavcodec's memory, not decoded into a slot.
static PROPS: [PropDesc; 1] = [PropDesc { name: "threads", allowed: Constraint::Any, live: false }];

/// Single threaded, the deterministic simulation rule.
pub const DEFAULT_THREADS: u32 = 1;

// One-time construction.
#[allow(clippy::disallowed_methods)]
pub(crate) static H264_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavh264dec", &H264_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavH264Dec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static H265_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavh265dec", &H265_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavH265Dec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP8_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavvp8dec", &VP8_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavVp8Dec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP9_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavvp9dec", &VP9_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavVp9Dec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static MPEG2_DESC: ElementDesc = decoder::picking(decoder::desc_with_props(
    "libavmpeg2dec",
    &MPEG2_SINKS,
    &SRCS,
    &PROPS,
    || Box::new(LibavMpeg2Dec::new()),
));
#[allow(clippy::disallowed_methods)]
pub(crate) static MPEG4_DESC: ElementDesc = decoder::picking(decoder::desc_with_props(
    "libavmpeg4dec",
    &MPEG4_SINKS,
    &SRCS,
    &PROPS,
    || Box::new(LibavMpeg4Dec::new()),
));
#[allow(clippy::disallowed_methods)]
pub(crate) static FLV1_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavflv1dec", &FLV1_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavFlv1Dec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP6_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavvp6dec", &VP6_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavVp6Dec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP6F_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavvp6fdec", &VP6F_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavVp6fDec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP6A_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavvp6adec", &VP6A_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavVp6aDec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP7_DESC: ElementDesc =
    decoder::picking(decoder::desc_with_props("libavvp7dec", &VP7_SINKS, &SRCS, &PROPS, || {
        Box::new(LibavVp7Dec::new())
    }));
#[allow(clippy::disallowed_methods)]
pub(crate) static THEORA_DESC: ElementDesc = decoder::picking(decoder::desc_with_props(
    "libavtheoradec",
    &THEORA_SINKS,
    &SRCS,
    &PROPS,
    || Box::new(LibavTheoraDec::new()),
));

pub const H264: u8 = 0;
pub const H265: u8 = 1;
pub const VP8: u8 = 2;
pub const VP9: u8 = 3;
pub const MPEG2: u8 = 4;
pub const MPEG4: u8 = 5;
pub const FLV1: u8 = 6;
pub const VP6: u8 = 7;
pub const VP6F: u8 = 8;
pub const VP6A: u8 = 9;
pub const VP7: u8 = 10;
pub const THEORA: u8 = 11;

const fn codec_of(kind: u8) -> lav::Codec {
    match kind {
        H264 => lav::Codec::H264,
        H265 => lav::Codec::Hevc,
        VP8 => lav::Codec::Vp8,
        MPEG2 => lav::Codec::Mpeg2,
        MPEG4 => lav::Codec::Mpeg4,
        FLV1 => lav::Codec::Flv1,
        VP6 => lav::Codec::Vp6,
        VP6F => lav::Codec::Vp6f,
        VP6A => lav::Codec::Vp6a,
        VP7 => lav::Codec::Vp7,
        THEORA => lav::Codec::Theora,
        _ => lav::Codec::Vp9,
    }
}

pub type LibavH264Dec = VideoDec<LavVideo<H264>>;
pub type LibavH265Dec = VideoDec<LavVideo<H265>>;
pub type LibavVp8Dec = VideoDec<LavVideo<VP8>>;
pub type LibavVp9Dec = VideoDec<LavVideo<VP9>>;
pub type LibavMpeg2Dec = VideoDec<LavVideo<MPEG2>>;
pub type LibavMpeg4Dec = VideoDec<LavVideo<MPEG4>>;
pub type LibavFlv1Dec = VideoDec<LavVideo<FLV1>>;
pub type LibavVp6Dec = VideoDec<LavVideo<VP6>>;
pub type LibavVp6fDec = VideoDec<LavVideo<VP6F>>;
pub type LibavVp6aDec = VideoDec<LavVideo<VP6A>>;
pub type LibavVp7Dec = VideoDec<LavVideo<VP7>>;
pub type LibavTheoraDec = VideoDec<LavVideo<THEORA>>;

/// The decoder of `desc` in dmabuf mode, see [`VideoDec::with_dmabuf`]. `None` for an element
/// that is not one of these.
// Once per pipeline build.
#[allow(clippy::disallowed_methods)]
pub fn with_dmabuf(
    desc: &ElementDesc,
    channel: &Arc<GpuFrameChannel>,
    formats: &[DmabufFormat],
) -> Option<Box<dyn Element>> {
    with_dmabuf_rows(desc, channel, formats, 1)
}

/// [`with_dmabuf`] with the rows of an NV12 or P010 slot a multiple of `row_align` bytes
/// apart, see [`VideoDec::with_row_align`].
// Once per pipeline build.
#[allow(clippy::disallowed_methods)]
pub fn with_dmabuf_rows(
    desc: &ElementDesc,
    channel: &Arc<GpuFrameChannel>,
    formats: &[DmabufFormat],
    row_align: usize,
) -> Option<Box<dyn Element>> {
    type Make = fn(&Arc<GpuFrameChannel>, &[DmabufFormat], usize) -> Box<dyn Element>;
    fn boxed<C: Codec>(
        channel: &Arc<GpuFrameChannel>,
        formats: &[DmabufFormat],
        row_align: usize,
    ) -> Box<dyn Element> {
        let dec = VideoDec::<C>::new().with_dmabuf(Arc::clone(channel), formats);
        Box::new(dec.with_row_align(row_align))
    }
    let made: [(&ElementDesc, Make); 12] = [
        (&H264_DESC, boxed::<LavVideo<H264>>),
        (&H265_DESC, boxed::<LavVideo<H265>>),
        (&VP8_DESC, boxed::<LavVideo<VP8>>),
        (&VP9_DESC, boxed::<LavVideo<VP9>>),
        (&MPEG2_DESC, boxed::<LavVideo<MPEG2>>),
        (&MPEG4_DESC, boxed::<LavVideo<MPEG4>>),
        (&FLV1_DESC, boxed::<LavVideo<FLV1>>),
        (&VP6_DESC, boxed::<LavVideo<VP6>>),
        (&VP6F_DESC, boxed::<LavVideo<VP6F>>),
        (&VP6A_DESC, boxed::<LavVideo<VP6A>>),
        (&VP7_DESC, boxed::<LavVideo<VP7>>),
        (&THEORA_DESC, boxed::<LavVideo<THEORA>>),
    ];
    made.iter()
        .find(|(d, _)| std::ptr::eq(*d, desc))
        .map(|(_, make)| make(channel, formats, row_align))
}

#[derive(Debug, PartialEq, Eq)]
pub enum LavVideoError {
    Open(LavError),
    Unit(LavError),
    Picture(LavError),
    Shape { width: u32, height: u32, bit_depth: u8, chroma_shift: (u8, u8) },
    SlotTooSmall { need: usize, capacity: usize },
    HeldFull { slots: usize },
}

impl fmt::Display for LavVideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LavVideoError::Open(e) => write!(f, "libavcodec decoder not opened, {e}"),
            LavVideoError::Unit(e) => write!(f, "access unit dropped, {e}"),
            LavVideoError::Picture(e) => write!(f, "picture not received, {e}"),
            LavVideoError::Shape { width, height, bit_depth, chroma_shift } => write!(
                f,
                "{width}x{height} {bit_depth}-bit picture with chroma subsampled by {}x{} is \
                 not decoded here, only planar YUV 4:2:0, 4:2:2 and 4:4:4 and monochrome are",
                1 << chroma_shift.0,
                1 << chroma_shift.1
            ),
            LavVideoError::SlotTooSmall { need, capacity } => {
                write!(f, "picture needs a {need} byte slot but the pool gave {capacity}")
            }
            LavVideoError::HeldFull { slots } => {
                write!(f, "picture dropped, all {slots} held picture handles are taken")
            }
        }
    }
}

impl std::error::Error for LavVideoError {}

/// A drain puts out a whole DPB, at most 16 frames (H.264 Table A-1, MaxDpbFrames). Frame threads
/// put out threads - 1 more, each held since none is in a slot, see `start`.
const HELD: usize = 20;

const SPARE: usize = 1;

/// Units and pictures lost that are logged one by one, the rest only counted.
const WARNINGS: u64 = 3;

pub struct LavVideo<const KIND: u8> {
    /// Declared before the decoder so they release first.
    held: Vec<Option<Frame>>,
    decoder: Option<Decoder>,
    /// Declared after the decoder, which releases into it as it closes.
    supply: Supply,
    open_failed: bool,
    late: bool,
    /// The configuration unit the decoder opened with, empty for none.
    config: Vec<u8>,
    sei_scratch: Box<[u8]>,
    /// Units refused and pictures lost since start, for the end of stream summary.
    failed: u64,
    lost: u64,
    /// The stamps of units sent and not yet given to a picture, smallest first, see
    /// [`decode_order`].
    stamps: BinaryHeap<Reverse<i64>>,
    /// Theora's identification header, held until its setup header comes.
    ident: Vec<u8>,
    laced: Vec<u8>,
    refs: Refs,
    threads: u32,
    /// Units of parameter sets alone held for the next unit under frame threads, see `decode`.
    lead: Vec<u8>,
}

/// Whether a Theora inter frame has its reference pictures. vp3.c decodes one with none against
/// a golden frame it never writes (vp3_decode_frame, "first frame not a keyframe"), where the
/// H.264, H.265, VP8 and VP9 decoders refuse it or hold its picture back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refs {
    Held,
    /// Opened or flushed, inter frames drop until an intra frame.
    Wanted,
    /// As `Wanted`, the first drop logged.
    Dropping,
}

/// Families whose units mostly come stamped in decode order, from AVI and FLV, which know no
/// presentation time. Pictures leave a decoder in display order, so the k-th picture takes the
/// k-th smallest stamp, right for decode order stamps and for presentation stamps alike. A unit
/// that makes no picture leaves its stamp behind, [`LavVideo::trim`] drops it.
const fn decode_order(kind: u8) -> bool {
    matches!(kind, MPEG4 | FLV1 | VP6 | VP6F | VP6A | VP7)
}

/// Stamps held past this many, a unit that made no picture, drop from the smallest.
const STAMPS: usize = 32;

impl<const KIND: u8> LavVideo<KIND> {
    /// Opens with the held configuration unit as extradata, else with `lead`. H.264 opens at the
    /// first `unit` with an SPS, whose reorder depth libavcodec starts from.
    fn ensure_open(&mut self, lead: &[u8], unit: &[u8], out: &mut Out<'_>) -> bool {
        if self.decoder.is_some() {
            return true;
        }
        if self.open_failed {
            return false;
        }
        let reorder = match KIND {
            H264 => match h264_head(unit) {
                Head::Missing => {
                    if self.count_failed() {
                        log!(out.ctx(), Level::Debug, "frame_drop", reason = "no_sps");
                    }
                    return false;
                }
                Head::Unparsed => None,
                Head::Reorder(n) => Some(n),
            },
            _ => None,
        };
        let LavVideo { decoder, supply, open_failed, late, config, threads, .. } = self;
        let codec = codec_of(KIND);
        let extradata = if config.is_empty() { lead } else { config.as_slice() };
        match Decoder::open(codec, extradata, Some(supply), false, *threads, reorder) {
            Ok(mut d) => {
                log!(
                    out.ctx(),
                    Level::Info,
                    "decoder_open",
                    codec = codec.name(),
                    extradata = extradata.len(),
                    late = *late,
                    threads = d.threads(),
                );
                if let Some(n) = reorder {
                    log!(out.ctx(), Level::Debug, "reorder_from_sps", frames = n);
                }
                d.skip_nonref(*late);
                *decoder = Some(d);
                true
            }
            Err(e) => {
                *open_failed = true;
                log!(out.ctx(), Level::Error, "open_failed", codec = codec.name(), err = e.code());
                out.warn(LavVideoError::Open(e));
                false
            }
        }
    }

    /// Opens the decoder with `unit` as extradata. The same unit again, as a demuxer sends it after
    /// a seek, changes nothing. Another drains the open decoder and reopens with it.
    fn configure(&mut self, unit: &[u8], out: &mut Out<'_>) {
        if let Some(d) = &mut self.decoder {
            if self.config == unit {
                log!(out.ctx(), Level::Trace, "config_same", bytes = unit.len());
                return;
            }
            log!(
                out.ctx(),
                Level::Info,
                "config_changed",
                bytes = unit.len(),
                was = self.config.len()
            );
            if let Err(e) = d.drain() {
                log!(out.ctx(), Level::Warn, "drain_failed", err = e.code());
            }
            self.receive_all(out);
            // Pictures held out outlive the decoder (pf_libav.c get_buffer2).
            self.decoder = None;
        }
        self.open_failed = false;
        self.config.clear();
        self.config.extend_from_slice(unit);
        self.ensure_open(&[], &[], out);
    }

    fn top_up(&self, out: &mut Out<'_>) -> bool {
        let want = self.supply.wanted();
        while want > 0 && self.supply.spare() < SPARE {
            match out.frame_memory(want) {
                Some(m) => self.supply.feed(m),
                None => {
                    log!(
                        out.ctx(),
                        Level::Trace,
                        "starve",
                        want = want,
                        spare = self.supply.spare()
                    );
                    out.starve();
                    return false;
                }
            }
        }
        true
    }

    fn receive_all(&mut self, out: &mut Out<'_>) {
        let LavVideo { held, decoder: Some(decoder), supply, lost, stamps, .. } = self else {
            return;
        };
        loop {
            let got = decoder.receive(&mut |bytes| out.frame_memory(bytes));
            check_short(supply, out);
            match got {
                Ok(Received::Frame(frame)) => {
                    let at = match decode_order(KIND) {
                        true => stamps.pop().map_or(frame.pts(), |Reverse(s)| s),
                        false => frame.pts(),
                    };
                    if !place(held, supply, frame, at, out) {
                        *lost += 1;
                        if *lost <= WARNINGS {
                            log!(
                                out.ctx(),
                                Level::Warn,
                                "held_full",
                                slots = held.len(),
                                lost = *lost
                            );
                        }
                    }
                }
                Ok(Received::Again | Received::Ended) => return,
                Err(e) => {
                    *lost += 1;
                    if *lost <= WARNINGS {
                        log!(out.ctx(), Level::Warn, "receive_error", err = e.code(), lost = *lost);
                    }
                    return out.warn(LavVideoError::Picture(e));
                }
            }
        }
    }

    /// Sends `unit` stamped `pts`, `stamp` for the log, and puts out the pictures ready.
    fn send(&mut self, unit: &[u8], stamp: Timestamp, pts: i64, duration: i64, out: &mut Out<'_>) {
        // A full decoder gives its pictures and takes the unit on the second try.
        let mut taken = false;
        for _ in 0..2 {
            let Some(decoder) = &mut self.decoder else {
                return;
            };
            let sent =
                decoder.send(unit, pts, lav::NONE, duration, &mut |bytes| out.frame_memory(bytes));
            check_short(&self.supply, out);
            match sent {
                Ok(lav::Sent::Full) => self.receive_all(out),
                Ok(lav::Sent::Taken | lav::Sent::Ended) => {
                    taken = true;
                    break;
                }
                // A unit of parameter sets alone is taken into the decoder's state and refused as
                // no frame.
                Err(e) if !codes_picture(KIND, unit) => {
                    log!(out.ctx(), Level::Trace, "no_picture", err = e.code(), bytes = unit.len());
                    taken = true;
                    break;
                }
                Err(e) => {
                    if self.count_failed() {
                        log!(
                            out.ctx(),
                            Level::Warn,
                            "unit_error",
                            err = e.code(),
                            bytes = unit.len(),
                            pts = stamp,
                            failed = self.failed,
                        );
                    }
                    out.warn(LavVideoError::Unit(e));
                    taken = true;
                    break;
                }
            }
        }
        if !taken && self.count_failed() {
            log!(out.ctx(), Level::Warn, "unit_refused", bytes = unit.len(), pts = stamp);
        }
        self.receive_all(out);
        self.trim();
    }

    /// Keeps the stamps of the pictures the decoder holds back and no more. One past them is
    /// of a unit that made no picture, skipped as late, refused, in error or a VOP not coded,
    /// and the smallest go, whose display slots passed.
    fn trim(&mut self) {
        if !decode_order(KIND) {
            return;
        }
        let delay = self.decoder.as_ref().map_or(0, Decoder::delay);
        while self.stamps.len() > delay {
            self.stamps.pop();
        }
    }

    /// Count a unit not decoded, `true` for the first few, which are logged.
    fn count_failed(&mut self) -> bool {
        self.failed += 1;
        self.failed <= WARNINGS
    }

    /// `true` for a Theora unit to decode. The setup header opens the decoder with the three
    /// headers laced, a header again as a demuxer sends it after a seek changes nothing.
    fn theora_unit(&mut self, unit: &[u8], out: &mut Out<'_>) -> bool {
        match unit {
            [] => true,
            // A header has the top bit of its first byte set, a frame clear (Theora I 6.1).
            [t, ..] if t & 0x80 == 0 => {
                let open = !self.config.is_empty();
                if !open && self.count_failed() {
                    log!(out.ctx(), Level::Warn, "frame_drop", reason = "no_headers");
                }
                open && self.theora_refs(t & 0x40 != 0, out)
            }
            [0x80, b't', b'h', b'e', b'o', b'r', b'a', ..] => {
                self.ident.clear();
                self.ident.extend_from_slice(unit);
                false
            }
            [0x82, b't', b'h', b'e', b'o', b'r', b'a', ..] if !self.ident.is_empty() => {
                let mut laced = std::mem::take(&mut self.laced);
                xiph_lace(&mut laced, &self.ident, unit);
                if laced != self.config {
                    self.refs = Refs::Wanted;
                }
                self.configure(&laced, out);
                self.laced = laced;
                false
            }
            _ => false,
        }
    }

    /// `false` for an inter frame that has no reference pictures to decode from (Theora I 7.1,
    /// FTYPE).
    fn theora_refs(&mut self, inter: bool, out: &mut Out<'_>) -> bool {
        if !inter {
            self.refs = Refs::Held;
            return true;
        }
        if self.refs == Refs::Wanted {
            log!(out.ctx(), Level::Debug, "frame_drop", reason = "no_keyframe");
            self.refs = Refs::Dropping;
        }
        self.refs == Refs::Held
    }
}

/// The extradata libavcodec splits into Xiph headers (xiph.c avpriv_split_xiph_headers). The
/// comment header goes empty, theora_decode_init skips it.
fn xiph_lace(out: &mut Vec<u8>, ident: &[u8], setup: &[u8]) {
    out.clear();
    out.push(2);
    out.extend(std::iter::repeat_n(0xFF, ident.len() / 255));
    out.extend_from_slice(&[(ident.len() % 255) as u8, 0]);
    out.extend_from_slice(ident);
    out.extend_from_slice(setup);
}

fn check_short(supply: &Supply, out: &mut Out<'_>) {
    if let Some((need, capacity)) = supply.take_short() {
        out.fail(LavVideoError::SlotTooSmall { need, capacity });
    }
}

/// Slice NAL types per H.264 and H.265 Table 7-1, MPEG-2 picture_start_code (H.262 Table 6-1),
/// MPEG-4 Part 2 vop_start_code (14496-2 Table 6-3).
fn codes_picture(kind: u8, unit: &[u8]) -> bool {
    let hevc = match kind {
        H264 => false,
        H265 => true,
        MPEG2 => return unit.windows(4).any(|w| w == [0, 0, 1, 0]),
        MPEG4 => return unit.windows(4).any(|w| w == [0, 0, 1, 0xB6]),
        _ => return true,
    };
    let mut i = 0;
    while i + 3 < unit.len() {
        if unit[i] == 0 && unit[i + 1] == 0 && unit[i + 2] == 1 {
            let h = unit[i + 3];
            let slice = if hevc { (h >> 1) & 0x3F < 32 } else { matches!(h & 0x1F, 1..=5) };
            if slice {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// A unit of stream configuration alone, which libavcodec takes only as extradata. An MPEG-4
/// Part 2 VOL with no VOP in its packet is parsed, then its size reverted (h263dec.c
/// ff_h263_decode_frame), so every VOP after it fails to get a buffer. A VOL is
/// video_object_layer_start_code 0x120 to 0x12F (14496-2 Table 6-3). H.264, H.265 and MPEG-1/2
/// keep parameter sets taken in band, VP6 in FLV is split per unit, the rest carry none.
fn config_unit(kind: u8, unit: &[u8]) -> bool {
    kind == MPEG4
        && !codes_picture(kind, unit)
        && unit.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0xF0 == 0x20)
}

/// What an H.264 unit holds for the open, see [`LavVideo::ensure_open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Head {
    /// No SPS, libavcodec decodes nothing before one.
    Missing,
    /// SPSs our parser refuses, libavcodec guesses the reorder depth.
    Unparsed,
    /// The deepest num_reorder_frames of the unit's SPSs (E.2.1). libavcodec otherwise starts
    /// at 0 without VUI bitstream_restriction and drops the pictures already out of order.
    Reorder(u32),
}

fn h264_head(unit: &[u8]) -> Head {
    let mut head = Head::Missing;
    h264parse::for_each_nal(unit, |nal| {
        if nal.unit_type != h264parse::NAL_SPS {
            return;
        }
        let n = h264parse::parse_sps(nal.raw(unit)).map(|s| s.num_reorder_frames());
        head = match (head, n) {
            (Head::Reorder(a), Some(b)) => Head::Reorder(a.max(b)),
            (Head::Reorder(a), None) => Head::Reorder(a),
            (_, Some(b)) => Head::Reorder(b),
            (_, None) => Head::Unparsed,
        };
    });
    head
}

fn from_lav(v: i64) -> Timestamp {
    if v < 0 { Timestamp::NONE } else { Timestamp::from_nanos(v as u64) }
}

fn to_lav(t: Timestamp) -> i64 {
    t.nanos().map_or(lav::NONE, |n| n as i64)
}

/// `false` when the picture is lost for want of a held handle.
/// Puts `frame` out stamped `pts`, libav's units.
fn place(
    held: &mut [Option<Frame>],
    supply: &Supply,
    frame: Frame,
    pts: i64,
    out: &mut Out<'_>,
) -> bool {
    let (width, height) = (frame.width(), frame.height());
    let Some((pixfmt, layout)) = frame.place(out.takes()) else {
        let (bit_depth, chroma_shift) = match frame.shape() {
            Shape::Planar(p) => (p.bits, p.chroma_shift),
            Shape::Gray => (8, (0, 0)),
            Shape::Other { bit_depth, chroma_shift } => (bit_depth, chroma_shift),
        };
        out.fail(LavVideoError::Shape { width, height, bit_depth, chroma_shift });
        return true;
    };
    let pts = from_lav(pts);
    let duration = from_lav(frame.duration());
    let placement = frame.placement();
    if let Some((memory, _, _)) = &placement {
        supply.want(memory.len());
    }
    if out.decode_only(pts) {
        return true;
    }
    let color = frame.color();
    if let Some((memory, offsets, strides)) = placement {
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
        return true;
    }
    let Some(handle) = held.iter().position(Option::is_none) else {
        out.warn(LavVideoError::HeldFull { slots: held.len() });
        return false;
    };
    held[handle] = Some(frame);
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
    true
}

impl<const KIND: u8> Codec for LavVideo<KIND> {
    const DESC: &'static ElementDesc = match KIND {
        H264 => &H264_DESC,
        H265 => &H265_DESC,
        VP8 => &VP8_DESC,
        MPEG2 => &MPEG2_DESC,
        MPEG4 => &MPEG4_DESC,
        FLV1 => &FLV1_DESC,
        VP6 => &VP6_DESC,
        VP6F => &VP6F_DESC,
        VP6A => &VP6A_DESC,
        VP7 => &VP7_DESC,
        THEORA => &THEORA_DESC,
        _ => &VP9_DESC,
    };

    // One-time setup.
    #[allow(clippy::disallowed_methods)]
    fn new() -> Self {
        Self {
            held: std::iter::repeat_with(|| None).take(HELD).collect(),
            decoder: None,
            supply: Supply::new(),
            open_failed: false,
            late: false,
            config: Vec::new(),
            sei_scratch: vec![0u8; 8192].into_boxed_slice(),
            failed: 0,
            lost: 0,
            stamps: BinaryHeap::with_capacity(STAMPS + 1),
            ident: Vec::new(),
            laced: Vec::new(),
            refs: Refs::Wanted,
            threads: DEFAULT_THREADS,
            lead: Vec::new(),
        }
    }

    fn start(&mut self, ctx: &Ctx) {
        let threads = match ctx.prop("threads") {
            Some(Value::Int(v)) => v.clamp(1, i64::from(lav::MAX_THREADS)) as u32,
            _ => DEFAULT_THREADS,
        };
        if threads != self.threads {
            log!(ctx, Level::Debug, "threads", threads = threads, was = self.threads);
            self.held.iter_mut().for_each(|h| *h = None);
            self.held.resize_with(HELD + threads as usize - 1, || None);
            self.decoder = None;
            self.stamps.clear();
            self.threads = threads;
        }
        self.open_failed = false;
        self.failed = 0;
        self.lost = 0;
    }

    fn reset(&mut self) {
        self.held.iter_mut().for_each(|h| *h = None);
        if let Some(d) = &mut self.decoder {
            d.flush();
        }
        self.open_failed = false;
        self.stamps.clear();
        self.lead.clear();
        self.refs = Refs::Wanted;
    }

    fn decode(&mut self, unit: &[u8], pts: Timestamp, duration: Timestamp, out: &mut Out<'_>) {
        if KIND == THEORA && !self.theora_unit(unit, out) {
            return;
        }
        let (extradata, unit) = match KIND {
            VP6F | VP6A => unit.split_at(unit.len().min(1)),
            _ => (&[][..], unit),
        };
        if config_unit(KIND, unit) {
            return self.configure(unit, out);
        }
        if unit.is_empty() || !self.ensure_open(extradata, unit, out) || !self.top_up(out) {
            return;
        }
        let stamp = pts;
        let (pts, duration) = (to_lav(pts), to_lav(duration));
        if decode_order(KIND) && pts != lav::NONE {
            self.stamps.push(Reverse(pts));
            if self.stamps.len() > STAMPS {
                self.stamps.pop();
            }
        }
        // Frame threads report a unit that codes no picture as failed on a later unit's call, so
        // parameter sets alone ride in front of the next unit instead.
        let threaded = self.decoder.as_ref().is_some_and(|d| d.threads() > 1);
        if threaded && !codes_picture(KIND, unit) {
            self.lead.extend_from_slice(unit);
            return self.trim();
        }
        let mut joined = std::mem::take(&mut self.lead);
        if joined.is_empty() {
            self.send(unit, stamp, pts, duration, out);
        } else {
            joined.extend_from_slice(unit);
            self.send(&joined, stamp, pts, duration, out);
            joined.clear();
        }
        self.lead = joined;
    }

    fn finish(&mut self, out: &mut Out<'_>) {
        self.lead.clear();
        let Some(decoder) = &mut self.decoder else {
            return;
        };
        if let Err(e) = decoder.drain() {
            log!(out.ctx(), Level::Warn, "drain_failed", err = e.code());
            return out.warn(LavVideoError::Unit(e));
        }
        self.receive_all(out);
        if let Some(d) = &mut self.decoder {
            log!(
                out.ctx(),
                Level::Info,
                "summary",
                failed = self.failed,
                lost = self.lost,
                fallbacks = d.fallbacks(),
            );
            d.flush();
        }
        self.stamps.clear();
    }

    fn scan(&mut self, unit: &[u8], scan: &mut Scan<'_>) {
        let codec = match KIND {
            H264 => pf_hdr::h26x::Codec::H264,
            H265 => pf_hdr::h26x::Codec::H265,
            _ => return,
        };
        pf_hdr::h26x::scan_annexb(codec, unit, &mut self.sei_scratch, scan);
    }

    fn late(&mut self, late: bool) {
        self.late = late;
        if let Some(d) = &mut self.decoder {
            d.skip_nonref(late);
        }
    }

    fn held_planes(&self, handle: u32) -> [PlaneRef<'_>; 3] {
        match self.held.get(handle as usize) {
            Some(Some(f)) => [f.plane(0), f.plane(1), f.plane(2)],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_of_parameter_sets_codes_no_picture() {
        let sps_pps = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xCE];
        assert!(!codes_picture(H264, &sps_pps));
        let idr = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65, 0x88];
        assert!(codes_picture(H264, &idr));
        let vps_sps = [0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x42, 0x01];
        assert!(!codes_picture(H265, &vps_sps));
        let idr_w_radl = [0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x26, 0x01];
        assert!(codes_picture(H265, &idr_w_radl));
        assert!(codes_picture(VP9, &[0x82]));
        let seq = [0, 0, 1, 0xB3, 0x2D, 0x01, 0xE0, 0x24, 0, 0, 1, 0xB5, 0x14];
        assert!(!codes_picture(MPEG2, &seq));
        let pic = [0, 0, 1, 0xB3, 0x2D, 0, 0, 1, 0xB8, 0x00, 0, 0, 1, 0x00, 0x00, 0x0F];
        assert!(codes_picture(MPEG2, &pic));
        let vol = [0, 0, 1, 0xB0, 0xF5, 0, 0, 1, 0xB5, 0x09, 0, 0, 1, 0x20, 0x00];
        assert!(!codes_picture(MPEG4, &vol));
        let vop = [0, 0, 1, 0xB0, 0xF5, 0, 0, 1, 0xB6, 0x10];
        assert!(codes_picture(MPEG4, &vop));
    }

    /// `ipb_novui.h264` is `ipb.h264` with its SPS cut before the VUI.
    #[test]
    fn the_open_takes_the_reorder_depth_of_the_units_sps() {
        let vui = include_bytes!("../tests/fixtures/ipb.h264");
        let novui = include_bytes!("../tests/fixtures/ipb_novui.h264");
        assert_eq!(h264_head(vui), Head::Reorder(2), "bitstream_restriction");
        // 64x48 at level 1.0, MaxDpbFrames 396 / 12 capped at 16
        assert_eq!(h264_head(novui), Head::Reorder(16), "E.2.1 inferred");
        assert_eq!(h264_head(&[0, 0, 0, 1, 0x65, 0x88, 0x84]), Head::Missing, "a slice alone");
        // seq_parameter_set_id 32
        let bad = [0, 0, 0, 1, 0x67, 66, 0, 30, 0x04, 0x20];
        assert_eq!(h264_head(&bad), Head::Unparsed);
        let both: Vec<u8> = bad.iter().chain(&vui[..]).copied().collect();
        assert_eq!(h264_head(&both), Head::Reorder(2), "the SPS that parses");
    }

    /// Lengths in 255s and the rest, the last header's left to the end (xiph.c).
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn theora_headers_lace_as_libavcodec_splits_them() {
        let mut out = Vec::new();
        xiph_lace(&mut out, &[0x80; 42], &[0x82; 9]);
        assert_eq!(out[..4], [2, 42, 0, 0x80]);
        assert_eq!(out.len(), 3 + 42 + 9);
        xiph_lace(&mut out, &[0x80; 300], &[0x82; 1]);
        assert_eq!(out[..4], [2, 0xFF, 45, 0]);
        assert_eq!(out.len(), 4 + 300 + 1);
    }

    #[test]
    fn only_an_mpeg4_vol_without_a_vop_is_config() {
        let vol = [0, 0, 1, 0xB0, 0xF5, 0, 0, 1, 0x00, 0, 0, 1, 0x20, 0x00];
        assert!(config_unit(MPEG4, &vol));
        let vol_vop = [0, 0, 1, 0x20, 0x00, 0, 0, 1, 0xB6, 0x10];
        assert!(!config_unit(MPEG4, &vol_vop));
        // A packed bitstream placeholder and a lone GOV code no picture yet configure nothing.
        assert!(!config_unit(MPEG4, &[0x7F]));
        assert!(!config_unit(MPEG4, &[0, 0, 1, 0xB3, 0x00]));
        let seq = [0, 0, 1, 0xB3, 0x2D, 0x01, 0xE0, 0x24];
        assert!(!config_unit(MPEG2, &seq));
        assert!(!config_unit(H264, &[0, 0, 0, 1, 0x67, 0x42]));
    }
}
