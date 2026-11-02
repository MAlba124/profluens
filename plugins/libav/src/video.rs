//! libavcodec's H.264, H.265, VP8, VP9, MPEG-1/2, MPEG-4 Part 2, Sorenson Spark, VP6 and VP7
//! decoders in the shared [`VideoDec`] shell. VP6 in FLV (`vp6f`, `vp6a`) leads each frame with
//! the byte that crops its 16-pixel grid, libavcodec takes the first as extradata. An MPEG-4
//! Part 2 configuration unit (a demuxer's DSI or CodecPrivate) is extradata too, see
//! [`config_unit`]. The decoder opens at the first unit, never in [`Codec::new`], which builds
//! throwaway instances.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;

use profluens_core::element::{ElementDesc, PadDesc};
use profluens_core::format::{FieldDesc, OfferDesc, ValueDesc};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;
use profluens_core::time::Timestamp;
use profluens_video::FAMILY;
use profluens_video::decoder::{self, Codec, Held, Out, PlaneRef, Scan, Sent, VideoDec};

use crate::lav::{self, Decoder, Frame, LavError, Received, Shape, Supply};

static PIXFMTS: [ValueDesc; 3] =
    [ValueDesc::Id("i420"), ValueDesc::Id("gray8"), ValueDesc::Id("p010")];
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

// One-time construction.
#[allow(clippy::disallowed_methods)]
pub(crate) static H264_DESC: ElementDesc =
    decoder::desc("libavh264dec", &H264_SINKS, &SRCS, || Box::new(LibavH264Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static H265_DESC: ElementDesc =
    decoder::desc("libavh265dec", &H265_SINKS, &SRCS, || Box::new(LibavH265Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP8_DESC: ElementDesc =
    decoder::desc("libavvp8dec", &VP8_SINKS, &SRCS, || Box::new(LibavVp8Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP9_DESC: ElementDesc =
    decoder::desc("libavvp9dec", &VP9_SINKS, &SRCS, || Box::new(LibavVp9Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static MPEG2_DESC: ElementDesc =
    decoder::desc("libavmpeg2dec", &MPEG2_SINKS, &SRCS, || Box::new(LibavMpeg2Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static MPEG4_DESC: ElementDesc =
    decoder::desc("libavmpeg4dec", &MPEG4_SINKS, &SRCS, || Box::new(LibavMpeg4Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static FLV1_DESC: ElementDesc =
    decoder::desc("libavflv1dec", &FLV1_SINKS, &SRCS, || Box::new(LibavFlv1Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP6_DESC: ElementDesc =
    decoder::desc("libavvp6dec", &VP6_SINKS, &SRCS, || Box::new(LibavVp6Dec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP6F_DESC: ElementDesc =
    decoder::desc("libavvp6fdec", &VP6F_SINKS, &SRCS, || Box::new(LibavVp6fDec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP6A_DESC: ElementDesc =
    decoder::desc("libavvp6adec", &VP6A_SINKS, &SRCS, || Box::new(LibavVp6aDec::new()));
#[allow(clippy::disallowed_methods)]
pub(crate) static VP7_DESC: ElementDesc =
    decoder::desc("libavvp7dec", &VP7_SINKS, &SRCS, || Box::new(LibavVp7Dec::new()));

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

/// A drain puts out a whole DPB, at most 16 frames (H.264 Table A-1, MaxDpbFrames).
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
    /// Opens with the held configuration unit as extradata, else with `lead`.
    fn ensure_open(&mut self, lead: &[u8], out: &mut Out<'_>) -> bool {
        let LavVideo { decoder, supply, open_failed, late, config, .. } = self;
        if decoder.is_some() {
            return true;
        }
        if *open_failed {
            return false;
        }
        let codec = codec_of(KIND);
        let extradata = if config.is_empty() { lead } else { config.as_slice() };
        match Decoder::open(codec, extradata, Some(supply), false) {
            Ok(mut d) => {
                log!(
                    out.ctx(),
                    Level::Info,
                    "decoder_open",
                    codec = codec.name(),
                    extradata = extradata.len(),
                    late = *late,
                );
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
        self.ensure_open(&[], out);
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
    let (pixfmt, layout) = match frame.shape() {
        Shape::Named(p, l) => (p, l),
        Shape::Other { bit_depth, chroma_shift } => {
            out.fail(LavVideoError::Shape { width, height, bit_depth, chroma_shift });
            return true;
        }
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
        }
    }

    fn start(&mut self, _ctx: &profluens_core::ctx::Ctx) {
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
    }

    fn decode(&mut self, unit: &[u8], pts: Timestamp, duration: Timestamp, out: &mut Out<'_>) {
        let (extradata, unit) = match KIND {
            VP6F | VP6A => unit.split_at(unit.len().min(1)),
            _ => (&[][..], unit),
        };
        if config_unit(KIND, unit) {
            return self.configure(unit, out);
        }
        if unit.is_empty() || !self.ensure_open(extradata, out) || !self.top_up(out) {
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

    fn finish(&mut self, out: &mut Out<'_>) {
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
