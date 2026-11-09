//! `Mp4Demux`, the ISO-BMFF demuxer element. Tracks resolve from a head given at
//! construction, since a mid-pipeline element gets no input during preroll.

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, SeekTarget, Stream, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{FormatId, SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;
use profluens_core::pull::Ask;
use profluens_core::rate;
use profluens_core::time::Timestamp;
use profluens_video::color::StreamColor;
use profluens_video::format::COLOR_OFFER_FIELDS;

use crate::boxes::FourCc;
use crate::codec::{self, Reframer};
use crate::pull::{Plan, RUN, SMALL, State};
use crate::reader::{Limits, Mp4Error, Mp4Reader, ResolvedSample, SamplePayload};

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

const FAMILY_BYTES: &str = "bytes";

/// What an audio pad names to declare a track's delay, `pf_mkv::codec::FIELD_DELAY` in ns.
static DELAY_FIELDS: [FieldDesc; 1] = [FieldDesc {
    field: pf_mkv::codec::FIELD_DELAY,
    allowed: ConstraintDesc::Any,
    preferred: None,
}];

static RAW_SAMPLES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];
static RAW_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: profluens_audio::FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: profluens_audio::FIELD_CHANNELS,
        allowed: ConstraintDesc::Any,
        preferred: None,
    },
    FieldDesc {
        field: profluens_audio::FIELD_SAMPLE,
        allowed: ConstraintDesc::Set(&RAW_SAMPLES),
        preferred: None,
    },
];

pub(crate) static DEMUX_SRC_OFFERS: [OfferDesc; 28] = [
    OfferDesc::any(FAMILY_BYTES),
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc { family: "h264/annexb", fields: &COLOR_OFFER_FIELDS },
    OfferDesc { family: "h265/annexb", fields: &COLOR_OFFER_FIELDS },
    OfferDesc { family: "vp9", fields: &COLOR_OFFER_FIELDS },
    OfferDesc { family: "av1", fields: &COLOR_OFFER_FIELDS },
    // `mp4v` by its esds, led in band by the DecoderSpecificInfo.
    OfferDesc { family: "mpeg4/asp", fields: &COLOR_OFFER_FIELDS },
    OfferDesc { family: "mpeg2", fields: &COLOR_OFFER_FIELDS },
    OfferDesc { family: "mpeg1", fields: &COLOR_OFFER_FIELDS },
    // Remux framings. Samples stay length-prefixed, led by the raw record (RFC 9559 12).
    OfferDesc::any("h264/avcc"),
    OfferDesc::any("h265/hvcc"),
    OfferDesc::any("vp9"),
    OfferDesc::any("av1"),
    OfferDesc::any("mpeg4/asp"),
    OfferDesc::any("mpeg2"),
    OfferDesc::any("mpeg1"),
    OfferDesc { family: "opus", fields: &DELAY_FIELDS },
    // Raw AAC access units led in-band by the ASC, no ADTS (RFC 9559 12).
    OfferDesc { family: "aac", fields: &DELAY_FIELDS },
    OfferDesc { family: "ac3", fields: &DELAY_FIELDS },
    OfferDesc { family: "eac3", fields: &DELAY_FIELDS },
    OfferDesc { family: "flac", fields: &DELAY_FIELDS },
    OfferDesc { family: "mp3", fields: &DELAY_FIELDS },
    OfferDesc::any("alac"),
    OfferDesc { family: "audio/raw", fields: &RAW_FIELDS },
    OfferDesc::any("subtitle/srt"),
    OfferDesc::any("subtitle/vtt"),
    OfferDesc::any("subtitle/ttml"),
];

/// Every family the src pads offer, `bytes` aside.
pub static EMITS: &[&str] = &[
    "h264/annexb",
    "h265/annexb",
    "vp9",
    "av1",
    "mpeg4/asp",
    "mpeg2",
    "mpeg1",
    "h264/avcc",
    "h265/hvcc",
    "opus",
    "aac",
    "ac3",
    "eac3",
    "flac",
    "mp3",
    "alac",
    "audio/raw",
    "subtitle/srt",
    "subtitle/vtt",
    "subtitle/ttml",
];

static DEMUX_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];

pub(crate) static DEMUX_DESC: ElementDesc = ElementDesc {
    name: "mp4demux",
    sinks: &DEMUX_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Not name-constructible: needs the file head at construction (see `register`).
    make_default: None,
    io: false,
    couplings: &[],
};

struct PadTrack {
    stream: u32,
    pad: SrcPad,
    /// Cached so pts can be stamped while a sample still borrows the reader.
    timescale: u32,
    family: &'static str,
    codec_head: Vec<u8>,
    reframer: Reframer,
    announce: Announce,
    started: bool,
    delay_ticks: i64,
    text: Option<FourCc>,
    duration_ns: u64,
}

fn text_of(track: &crate::reader::Track) -> Option<FourCc> {
    matches!(&track.entry.kind, b"tx3g" | b"wvtt").then_some(track.entry.kind)
}

#[derive(Clone, Copy)]
enum Announce {
    /// `duration_ns` is set only in passthrough. A decode pipeline never interns the field
    /// and an announcement with an un-interned name is dropped whole.
    Video {
        width: u32,
        height: u32,
        duration_ns: u64,
        colour: StreamColor,
        rotation: u16,
    },
    Audio {
        rate: u32,
        channels: u32,
        duration_ns: u64,
        delay_ns: u64,
        sample: Option<&'static str>,
    },
    Bare,
}

struct Carry {
    pad: SrcPad,
    pts_ns: Option<u64>,
    dur_ns: Option<u64>,
    flags: BufferFlags,
    payload: CarryPayload,
    off: usize,
}

enum CarryPayload {
    Owned(Vec<u8>),
    Slice(Memory),
}

/// The most buffers the pool grows to for a track starving behind another in a far-apart file.
const GROWN_SLOTS: u64 = 1024;

fn grow(ctx: &mut Ctx) -> Option<u32> {
    let stats = ctx.pool_stats();
    let more = (stats.max_slots + stats.max_slots / 2).min(GROWN_SLOTS) as u32;
    (ctx.behind() && ctx.grow_slot_cap(more)).then_some(more)
}

pub struct Mp4Demux {
    head: Vec<u8>,
    limits: Limits,
    /// Remux mode. NAL tracks keep their stored framing and lead with the raw config record.
    passthrough: bool,
    pad_tracks: Vec<PadTrack>,
    reader: Option<Mp4Reader>,
    /// Emissions stalled on pool exhaustion. While non-empty no input is consumed.
    pending: std::collections::VecDeque<Carry>,
    /// Probed once at `start` so zero-copy slices match pooled buffers.
    out_fmt: FormatId,
    reframe_buf: Vec<u8>,
    epoch: u32,
    plan: Plan,
    ask_caps: AskCaps,
    rated: Option<profluens_core::pipeline::Index>,
    teller: rate::Teller,
    /// Samples dropped as malformed since start.
    dropped: u64,
}

#[derive(Clone, Copy, Default)]
struct AskCaps {
    bytes: u64,
    ranges: u32,
    small: u32,
}

/// The most the pool charges for a buffer of `n` bytes, its size class. Powers of two up
/// to 256, then at most a quarter past `n`.
fn charge_of(n: u32) -> u64 {
    let n = u64::from(n);
    if n <= 256 { n.next_power_of_two().max(64) } else { n + n / 4 }
}

fn within(n: u64) -> u32 {
    (n - n / 5).min(u64::from(u32::MAX)) as u32
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Own {
    Pushed,
    Whole,
    Shared,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Emit {
    Bounded,
    Exact,
}

fn shifted(ns: u64, shift: i64) -> u64 {
    (i128::from(ns) - i128::from(shift)).clamp(0, i128::from(u64::MAX)) as u64
}

impl Mp4Demux {
    pub const IN: SinkPad = SinkPad(0);

    /// `head` must hold `ftyp` and the whole `moov`, or for a `moov` after the `mdat` those
    /// bytes with the `moov` appended. A malformed head errors in `preroll`.
    // Cold path, once per element.
    #[allow(clippy::disallowed_methods)]
    pub fn new(head: Vec<u8>) -> Self {
        Self {
            head,
            limits: Limits::default(),
            passthrough: false,
            pad_tracks: Vec::new(),
            reader: None, // resolved in `preroll`
            pending: std::collections::VecDeque::new(),
            out_fmt: FormatId(0), // probed at `start`
            reframe_buf: Vec::new(),
            epoch: 0,
            plan: Plan::default(),
            ask_caps: AskCaps::default(),
            rated: None,
            teller: rate::Teller::default(),
            dropped: 0,
        }
    }

    // Cold path, once per element.
    #[allow(clippy::disallowed_methods)]
    pub fn resolved(reader: Mp4Reader) -> Self {
        let mut d = Self::new(Vec::new());
        d.reader = Some(reader);
        d
    }

    /// Remux mode. NAL tracks go out length-prefixed as stored, led by the raw `avcC`/`hvcC`
    /// record, the shape Matroska wants (RFC 9559 12).
    pub fn passthrough(head: Vec<u8>) -> Self {
        let mut d = Self::new(head);
        d.passthrough = true;
        d
    }

    /// The sample index `preroll` resolves is held to `limits`. Pass the file length when known.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The tracks discovered during `preroll`, empty before it.
    pub fn track_count(&self) -> usize {
        self.pad_tracks.len()
    }

    /// The src pad a track (0-based index) routes to, if discovered.
    pub fn pad_for(&self, track_index: usize) -> Option<SrcPad> {
        self.pad_tracks.get(track_index).map(|pt| pt.pad)
    }

    /// Returns the new offset. `off` unchanged means the pool budget is spent and the caller
    /// parks the blob.
    fn emit_bounded(
        ctx: &mut Ctx,
        pad: SrcPad,
        (pts_ns, dur_ns): (Option<u64>, Option<u64>),
        flags: BufferFlags,
        bytes: &[u8],
        off: usize,
    ) -> usize {
        let rest = &bytes[off..];
        if rest.is_empty() {
            return bytes.len();
        }
        let mut buf = match ctx.try_alloc_sized(rest.len()) {
            Ok(Some(buf)) => buf,
            Ok(None) if grow(ctx).is_some() => match ctx.try_alloc_sized(rest.len()) {
                Ok(Some(buf)) => buf,
                _ => return off,
            },
            Ok(None) => return off,
            Err(e) => {
                let element = ctx.element();
                ctx.post(BusMessage::Warning { element, error: Error::element_failed(element, e) });
                return bytes.len();
            }
        };
        buf.memory.as_mut_full()[..rest.len()].copy_from_slice(rest);
        buf.memory.set_len(rest.len());
        if let Some(t) = pts_ns {
            buf.pts = Timestamp::from_nanos(t);
        }
        if let Some(d) = dur_ns {
            buf.duration = Timestamp::from_nanos(d);
        }
        buf.flags = flags;
        ctx.out(pad).push(buf);
        bytes.len()
    }

    fn emit_exact(
        ctx: &mut Ctx,
        pad: SrcPad,
        (pts_ns, dur_ns): (Option<u64>, Option<u64>),
        flags: BufferFlags,
        bytes: &[u8],
        off: usize,
    ) {
        let rest = &bytes[off..];
        if rest.is_empty() {
            return;
        }
        let Some(mut buf) = ctx.alloc_exact(rest.len()) else {
            let (element, error) = (ctx.element(), ctx.exact_refused(rest.len()));
            ctx.post(BusMessage::Warning { element, error });
            return;
        };
        buf.memory.as_mut_full()[..rest.len()].copy_from_slice(rest);
        buf.memory.set_len(rest.len());
        if let Some(t) = pts_ns {
            buf.pts = Timestamp::from_nanos(t);
        }
        if let Some(d) = dur_ns {
            buf.duration = Timestamp::from_nanos(d);
        }
        buf.flags = flags;
        ctx.out(pad).push(buf);
    }

    fn emit_slice(
        ctx: &mut Ctx,
        pad: SrcPad,
        pts_ns: Option<u64>,
        flags: BufferFlags,
        memory: Memory,
        format: FormatId,
    ) {
        ctx.out(pad).push(Buffer {
            memory,
            pts: pts_ns.map_or(Timestamp::NONE, Timestamp::from_nanos),
            dts: Timestamp::NONE,
            duration: Timestamp::NONE,
            flags,
            format,
            sync: None,
        });
    }

    fn emit_or_park(
        pending: &mut std::collections::VecDeque<Carry>,
        ctx: &mut Ctx,
        pad: SrcPad,
        pts_ns: Option<u64>,
        flags: BufferFlags,
        bytes: Vec<u8>,
    ) {
        let off = Self::emit_bounded(ctx, pad, (pts_ns, None), flags, &bytes, 0);
        if off < bytes.len() {
            pending.push_back(Carry {
                pad,
                pts_ns,
                dur_ns: None,
                flags,
                payload: CarryPayload::Owned(bytes),
                off,
            });
        }
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> bool {
        while let Some(c) = self.pending.pop_front() {
            match c.payload {
                CarryPayload::Owned(bytes) => {
                    let off = Self::emit_bounded(
                        ctx,
                        c.pad,
                        (c.pts_ns, c.dur_ns),
                        c.flags,
                        &bytes,
                        c.off,
                    );
                    if off < bytes.len() {
                        self.pending.push_front(Carry {
                            pad: c.pad,
                            pts_ns: c.pts_ns,
                            dur_ns: c.dur_ns,
                            flags: c.flags,
                            payload: CarryPayload::Owned(bytes),
                            off,
                        });
                        return false;
                    }
                }
                CarryPayload::Slice(mem) => {
                    Self::emit_slice(ctx, c.pad, c.pts_ns, c.flags, mem, self.out_fmt);
                }
            }
        }
        true
    }

    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    fn track_wiring(
        track: &crate::reader::Track,
        passthrough: bool,
    ) -> Result<(&'static str, Vec<u8>, Reframer), Mp4Error> {
        if passthrough
            && let k @ (b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" | b"dva1"
            | b"dvav") = &track.entry.kind
        {
            if track.entry.config_record.is_empty() {
                return Err(Mp4Error::NoConfigRecord { entry: *k });
            }
            let is_hevc = crate::codec::is_hevc_entry(k);
            let family = if is_hevc { "h265/hvcc" } else { "h264/avcc" };
            return Ok((family, track.entry.config_record.clone(), Reframer::Passthrough));
        }
        Ok((track.family(), track.codec_head().to_vec(), *track.reframer()))
    }

    fn announce_of(
        track: &crate::reader::Track,
        family: &'static str,
        passthrough: bool,
    ) -> Announce {
        let duration_ns = if passthrough { track.duration_ns } else { 0 };
        let audio = matches!(family, "opus" | "aac" | "ac3" | "eac3" | "flac" | "mp3" | "alac")
            || track.entry.sample_rate != 0
            || track.entry.channels != 0;
        if codec::is_video_family(track.family()) {
            Announce::Video {
                width: track.width,
                height: track.height,
                duration_ns,
                colour: track.entry.colour,
                rotation: track.rotation,
            }
        } else if audio {
            Announce::Audio {
                rate: track.entry.sample_rate,
                channels: track.entry.channels,
                duration_ns,
                delay_ns: crate::reader::ticks_to_ns(track.delay_ticks(), track.timescale),
                sample: track.entry.pcm.map(|p| p.sample),
            }
        } else {
            Announce::Bare
        }
    }

    fn rewire(
        ctx: &Ctx,
        pad_tracks: &mut [PadTrack],
        tracks: &[crate::reader::Track],
        passthrough: bool,
    ) {
        for (pt, track) in pad_tracks.iter_mut().zip(tracks) {
            let Ok((family, codec_head, reframer)) = Self::track_wiring(track, passthrough) else {
                let (id, fourcc) = (track.track_id, track.entry.kind);
                log!(ctx, Level::Warn, "rewire_failed", track = id, fourcc = fourcc);
                continue;
            };
            (pt.family, pt.codec_head, pt.reframer) = (family, codec_head, reframer);
            pt.announce = Self::announce_of(track, family, passthrough);
            (pt.timescale, pt.delay_ticks, pt.started) =
                (track.timescale, track.delay_ticks(), false);
            (pt.text, pt.duration_ns) = (text_of(track), track.duration_ns);
        }
    }

    fn check(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(());
        };
        let Some(error) = reader.take_error() else {
            return Ok(());
        };
        let element = ctx.element();
        let error = Error::element_failed(element, error);
        if reader.is_lost() {
            return Err(error);
        }
        ctx.post(BusMessage::Warning { element, error });
        Ok(())
    }

    fn next_sample(&mut self, ctx: &Ctx, pulled: bool) -> Option<(ResolvedSample, Own)> {
        if pulled {
            return loop {
                let p = self.plan.pop_ready()?;
                let s = p.sample;
                let (payload, own) = match p.state {
                    State::Ready(mem, true) => (SamplePayload::Slice(mem), Own::Whole),
                    State::Ready(mem, false) => (SamplePayload::Slice(mem), Own::Shared),
                    State::Empty => (SamplePayload::Copied(empty()), Own::Pushed),
                    State::Dropped | State::Waiting => continue,
                };
                let (track_index, pts, dts, sync, hidden) =
                    (s.track(), s.pts, s.dts, s.sync, s.hidden);
                let span = self.reader.as_ref().and_then(|r| r.span_at(p.index));
                break Some((
                    ResolvedSample { track_index, pts, dts, sync, hidden, span, payload },
                    own,
                ));
            };
        }
        let reader = self.reader.as_mut()?;
        let s = reader.next_sample()?;
        if reader.init_epoch() != self.epoch {
            self.epoch = reader.init_epoch();
            log!(ctx, Level::Debug, "init_changed", epoch = self.epoch);
            Self::rewire(ctx, &mut self.pad_tracks, reader.tracks(), self.passthrough);
        }
        Some((s, Own::Pushed))
    }

    fn drain(&mut self, ctx: &mut Ctx) -> bool {
        if !self.drain_pending(ctx) {
            return false;
        }
        let pulled = ctx.pulls(Self::IN);
        while let Some((s, own)) = self.next_sample(ctx, pulled) {
            if !self.put_sample(ctx, s, own, Emit::Bounded) {
                return false;
            }
        }
        true
    }

    /// While anything is parked a sample parks behind it, so a pad's bytes never leave out of
    /// order.
    fn put_sample(&mut self, ctx: &mut Ctx, s: ResolvedSample, own: Own, how: Emit) -> bool {
        let fmt = self.out_fmt;
        let Self { reader, pad_tracks, pending, reframe_buf, passthrough, dropped, .. } = self;
        let shift = reader.as_ref().map_or(0, |r| r.shift_ns());
        let (idx, sync) = (s.track_index, s.sync);
        let Some(pt) = pad_tracks.get_mut(idx) else {
            return true;
        };
        let pts_ns = crate::reader::ticks_to_ns(s.pts.saturating_add(pt.delay_ticks), pt.timescale);
        let pts_ns = Some(shifted(pts_ns, shift));
        // DELTA is explicit. Untagged reads as unknown, which a muxer defaults to keyframe.
        let mut flags = if sync { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
        if s.hidden {
            flags |= BufferFlags::DECODE_ONLY;
        }
        if !pt.started {
            pt.started = true;
            let (pad, family, ann) = (pt.pad, pt.family, pt.announce);
            let head = std::mem::take(&mut pt.codec_head);
            Self::announce(ctx, pad, family, ann);
            if !head.is_empty() && how == Emit::Exact {
                Self::emit_exact(ctx, pad, (pts_ns, None), BufferFlags::HEADER, &head, 0);
            } else if !head.is_empty() {
                Self::emit_or_park(pending, ctx, pad, pts_ns, BufferFlags::HEADER, head);
            }
        }
        let (pad, reframer) = (pt.pad, pt.reframer);
        if let Some(kind) = pt.text {
            // A cue shows to the next sample of its track, the last to the track's end.
            let span = s.span.map(|ticks| crate::reader::ticks_to_ns(ticks, pt.timescale));
            let end =
                pts_ns.filter(|_| pt.duration_ns != 0).map(|at| pt.duration_ns.saturating_sub(at));
            let dur_ns = span.or(end).filter(|d| *d != 0);
            return Self::put_cues(
                ctx,
                pending,
                (pad, kind),
                (pts_ns, dur_ns),
                s.payload.data(),
                (how, reframe_buf),
            );
        }
        let mut payload = s.payload;
        let as_is = match (&payload, own) {
            (SamplePayload::Slice(_), Own::Pushed) => matches!(reframer, Reframer::Passthrough),
            (SamplePayload::Slice(_), Own::Shared) => {
                *passthrough && matches!(reframer, Reframer::Passthrough)
            }
            (SamplePayload::Copied(_), _) => false,
            (SamplePayload::Slice(_), Own::Whole) => {
                let SamplePayload::Slice(mem) = &mut payload else { unreachable!() };
                let n = mem.len();
                reframer.reframe_in_place(&mut mem.as_mut_full()[..n])
            }
        };
        let copy = !as_is && matches!(payload, SamplePayload::Slice(_));
        let payload = match (reframer, payload) {
            (_, payload) if as_is => payload,
            (_, payload) if copy => match reframer.reframe_into(payload.data(), reframe_buf) {
                Ok(bytes) if how == Emit::Exact => {
                    Self::emit_exact(ctx, pad, (pts_ns, None), flags, bytes, 0);
                    return true;
                }
                Ok(bytes) => {
                    let off = if pending.is_empty() {
                        Self::emit_bounded(ctx, pad, (pts_ns, None), flags, bytes, 0)
                    } else {
                        0
                    };
                    if off < bytes.len() {
                        let payload = CarryPayload::Owned(own_remainder(bytes, off));
                        pending.push_back(Carry {
                            pad,
                            pts_ns,
                            dur_ns: None,
                            flags,
                            payload,
                            off: 0,
                        });
                    }
                    return pending.is_empty();
                }
                Err(e) => return Self::reframe_failed(ctx, dropped, idx, e, how),
            },
            (
                Reframer::Nal { .. }
                | Reframer::Swap { .. }
                | Reframer::Narrow { .. }
                | Reframer::Flip { .. },
                payload,
            ) => match reframer.reframe_into(payload.data(), reframe_buf) {
                Ok(bytes) if how == Emit::Exact => {
                    Self::emit_exact(ctx, pad, (pts_ns, None), flags, bytes, 0);
                    return true;
                }
                Ok(bytes) => {
                    let off = if pending.is_empty() {
                        Self::emit_bounded(ctx, pad, (pts_ns, None), flags, bytes, 0)
                    } else {
                        0
                    };
                    if off < bytes.len() {
                        let payload = CarryPayload::Owned(own_remainder(bytes, off));
                        pending.push_back(Carry {
                            pad,
                            pts_ns,
                            dur_ns: None,
                            flags,
                            payload,
                            off: 0,
                        });
                    }
                    return pending.is_empty();
                }
                Err(e) => return Self::reframe_failed(ctx, dropped, idx, e, how),
            },
            (Reframer::Passthrough, payload) => payload,
        };
        match payload {
            SamplePayload::Slice(mem) if how == Emit::Exact || pending.is_empty() => {
                Self::emit_slice(ctx, pad, pts_ns, flags, mem, fmt);
            }
            SamplePayload::Slice(mem) => {
                pending.push_back(Carry {
                    pad,
                    pts_ns,
                    dur_ns: None,
                    flags,
                    payload: CarryPayload::Slice(mem),
                    off: 0,
                });
            }
            SamplePayload::Copied(owned) if how == Emit::Exact => {
                Self::emit_exact(ctx, pad, (pts_ns, None), flags, &owned, 0);
            }
            SamplePayload::Copied(owned) => {
                let off = if pending.is_empty() {
                    Self::emit_bounded(ctx, pad, (pts_ns, None), flags, &owned, 0)
                } else {
                    0
                };
                if off < owned.len() {
                    pending.push_back(Carry {
                        pad,
                        pts_ns,
                        dur_ns: None,
                        flags,
                        payload: CarryPayload::Owned(owned),
                        off,
                    });
                }
            }
        }
        pending.is_empty()
    }

    fn put_cues(
        ctx: &mut Ctx,
        pending: &mut std::collections::VecDeque<Carry>,
        (pad, kind): (SrcPad, FourCc),
        (pts_ns, dur_ns): (Option<u64>, Option<u64>),
        data: &[u8],
        (how, vtt): (Emit, &mut Vec<u8>),
    ) -> bool {
        let mut cues: [core::ops::Range<usize>; codec::MAX_CUES] = Default::default();
        let n = codec::text_cues(kind, data, &mut cues);
        let flags = BufferFlags::KEYFRAME;
        for cue in &cues[..n] {
            let text = match &kind {
                b"wvtt" => {
                    codec::vtt_cue(&data[cue.clone()], vtt);
                    &vtt[..]
                }
                _ => &data[cue.clone()],
            };
            if how == Emit::Exact {
                Self::emit_exact(ctx, pad, (pts_ns, dur_ns), flags, text, 0);
                continue;
            }
            let off = if pending.is_empty() {
                Self::emit_bounded(ctx, pad, (pts_ns, dur_ns), flags, text, 0)
            } else {
                0
            };
            if off < text.len() {
                let payload = CarryPayload::Owned(own_remainder(text, off));
                pending.push_back(Carry { pad, pts_ns, dur_ns, flags, payload, off: 0 });
            }
        }
        pending.is_empty()
    }

    fn reframe_failed(
        ctx: &mut Ctx,
        dropped: &mut u64,
        track: usize,
        cause: pf_mkv::ReframeError,
        how: Emit,
    ) -> bool {
        *dropped += 1;
        if how == Emit::Bounded {
            let element = ctx.element();
            let error = Error::element_failed(element, Mp4Error::SampleDropped { track, cause });
            ctx.post(BusMessage::Warning { element, error });
        }
        true
    }

    fn flush_exact(&mut self, ctx: &mut Ctx) {
        let fmt = self.out_fmt;
        while let Some(c) = self.pending.pop_front() {
            match c.payload {
                CarryPayload::Owned(bytes) => {
                    Self::emit_exact(ctx, c.pad, (c.pts_ns, c.dur_ns), c.flags, &bytes, c.off)
                }
                CarryPayload::Slice(mem) => {
                    Self::emit_slice(ctx, c.pad, c.pts_ns, c.flags, mem, fmt)
                }
            }
        }
        let pulled = ctx.pulls(Self::IN);
        while let Some((s, own)) = self.next_sample(ctx, pulled) {
            self.put_sample(ctx, s, own, Emit::Exact);
        }
    }

    fn plan_ranges(&mut self, ctx: &mut Ctx) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let AskCaps { bytes, ranges, small: most } = self.ask_caps;
        let pad_tracks = &self.pad_tracks;
        let small = |s: &crate::reader::Sample| {
            let passthrough = pad_tracks
                .get(s.track())
                .is_some_and(|pt| matches!(pt.reframer, Reframer::Passthrough));
            s.size <= most && passthrough
        };
        while ctx.asks_left(Self::IN) > 0 && self.plan.held < bytes && self.plan.ranges < ranges {
            let Some((index, first)) = reader.next_wanted() else {
                if !self.plan.asked_all {
                    self.plan.asked_all = true;
                    ctx.end_asks(Self::IN);
                }
                return;
            };
            self.plan.push(index, first, 0);
            let mut len = first.size;
            let run = RUN.min(within(bytes - self.plan.held));
            while small(&first) {
                let Some((i, s)) = reader.next_wanted() else {
                    break;
                };
                if s.offset != first.offset + len as u64 || !small(&s) || len + s.size > run {
                    reader.rewind_to(i);
                    break;
                }
                self.plan.push(i, s, len);
                len += s.size;
            }
            if len == 0 {
                self.plan.asked(0);
                continue;
            }
            // A range past what the plan may hold waits for the plan to empty, so the rest of
            // the pool always holds a codec head or a reframed copy.
            if self.plan.held > 0 && self.plan.held + charge_of(len) > bytes {
                self.plan.unplan().inspect(|at| reader.rewind_to(*at));
                return;
            }
            match ctx.try_alloc_sized(len as usize) {
                Ok(Some(buf)) => {
                    let charge = buf.memory.capacity().min(u32::MAX as usize) as u32;
                    let ask = Ask { offset: first.offset, len, tag: self.plan.next_tag, buf };
                    if ctx.ask(Self::IN, ask).is_err() {
                        log!(&*ctx, Level::Debug, "ask_refused", at = first.offset, bytes = len);
                        self.plan.unplan().inspect(|at| reader.rewind_to(*at));
                        return;
                    }
                    self.plan.asked(charge);
                }
                Ok(None) => {
                    self.plan.unplan().inspect(|at| reader.rewind_to(*at));
                    let Some(more) = grow(ctx) else { return };
                    log!(&*ctx, Level::Debug, "pool_grown", slots = more);
                    self.ask_caps.ranges = (more - more / 4).max(1);
                }
                Err(e) => {
                    let element = ctx.element();
                    ctx.post(BusMessage::Warning {
                        element,
                        error: Error::element_failed(element, e),
                    });
                    self.plan.drop_range();
                }
            }
        }
    }

    fn pull(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        while let Some((tag, buf)) = ctx.next_served(Self::IN, inputs) {
            if let (Err(short), false) = (self.plan.served(tag, buf.memory), self.plan.warned) {
                self.plan.warned = true;
                let element = ctx.element();
                ctx.post(BusMessage::Warning {
                    element,
                    error: Error::element_failed(element, short),
                });
            }
        }
        if !self.drain(ctx) {
            ctx.hold_end();
            return Ok(Flow::Ok);
        }
        self.plan_ranges(ctx);
        Ok(Flow::Ok)
    }

    fn unplan_all(&mut self) {
        if let (Some(at), Some(reader)) = (self.plan.reset(), self.reader.as_mut()) {
            reader.rewind_to(at);
        }
    }

    // Cold path, once per pad.
    #[allow(clippy::disallowed_methods)]
    fn announce(ctx: &mut Ctx, pad: SrcPad, family: &'static str, announce: Announce) {
        match announce {
            Announce::Video { width, height, duration_ns, colour, rotation }
                if width != 0 && height != 0 =>
            {
                let mut fields = vec![
                    ("width", ValueDesc::Int(width as i64)),
                    ("height", ValueDesc::Int(height as i64)),
                ];
                if duration_ns != 0 {
                    // Passthrough only, the muxer writes `Info\Duration` from it.
                    fields.push(("duration", ValueDesc::Int(duration_ns as i64)));
                }
                let mut colour_fields =
                    [("", ValueDesc::Int(0)); profluens_video::color::FIELDS.len()];
                let n = colour.write_fields(&mut colour_fields);
                fields.extend_from_slice(&colour_fields[..n]);
                // only where an element declares it, an un-interned name drops the whole
                // announcement
                let turn = profluens_video::format::FIELD_ROTATION;
                if rotation != 0 && ctx.field_id(turn).is_some() {
                    fields.push((turn, ValueDesc::Int(i64::from(rotation))));
                }
                ctx.announce_format(pad, family, &fields);
            }
            Announce::Audio { rate, channels, duration_ns, delay_ns, sample }
                if rate != 0 || channels != 0 =>
            {
                let mut fields = vec![
                    ("rate", ValueDesc::Int(rate as i64)),
                    ("channels", ValueDesc::Int(channels as i64)),
                ];
                fields.extend(sample.map(|s| (profluens_audio::FIELD_SAMPLE, ValueDesc::Id(s))));
                if duration_ns != 0 {
                    fields.push(("duration", ValueDesc::Int(duration_ns as i64)));
                }
                if delay_ns != 0 {
                    let delay = delay_ns.min(i64::MAX as u64) as i64;
                    fields.push((pf_mkv::codec::FIELD_DELAY, ValueDesc::Int(delay)));
                }
                ctx.announce_format(pad, family, &fields);
            }
            _ => ctx.announce_format(pad, family, &[]),
        }
    }

    /// Restarts the walk where the source restarted. Video from the sync sample at or before T,
    /// audio from T less its pre-roll, anything else whole.
    // Cold path, once per start and seek.
    #[allow(clippy::disallowed_methods)]
    fn tell_rate(&mut self, ctx: &mut Ctx) {
        let Some(index) = &self.rated else { return };
        self.teller.tell(ctx, Self::IN, |at| rate::of_index(index, at));
    }

    fn restart(&mut self, ctx: &Ctx, target: Option<SeekTarget>) -> Result<(), Error> {
        let Some(reader) = self.reader.as_mut() else {
            return Err(Error::element_failed(ctx.element(), Mp4Error::NotPrerolled));
        };
        match target {
            None => reader.reset_stream(),
            Some(target) => {
                let point = ctx.presentation_point().and_then(|point| point.nanos());
                let video_from = target.at.nanos().or(point).unwrap_or(0);
                let r = reader.seek_stream(target.to_byte, point, video_from);
                let (at, from) = (target.to_byte, r.to_byte);
                log!(ctx, Level::Debug, "seek", target = target.at, at = at, from = from);
            }
        }
        for pt in &mut self.pad_tracks {
            pt.started = false;
        }
        let heads: Vec<Vec<u8>> = reader
            .tracks()
            .iter()
            .map(|t| Self::track_wiring(t, self.passthrough).map(|(_, h, _)| h).unwrap_or_default())
            .collect();
        for (pt, head) in self.pad_tracks.iter_mut().zip(heads) {
            pt.codec_head = head;
        }
        self.pending.clear();
        self.plan.reset();
        Ok(())
    }
}

impl Mp4Demux {
    /// The seek index going back for the caption tracks `on`, kept for the rate teller.
    fn indexed(
        &mut self,
        tables: &crate::reader::Tables,
        on: &dyn Fn(usize) -> bool,
    ) -> profluens_core::pipeline::Index {
        let mut index = tables.seek_index(on);
        let stream = |track: u32| self.pad_tracks.get(track as usize).map(|pt| pt.stream);
        index.stream = index.stream.and_then(stream);
        index.captions.retain_mut(|c| match stream(c.stream) {
            Some(s) => {
                c.stream = s;
                true
            }
            None => false,
        });
        self.rated = Some(index.clone());
        index
    }

    /// Publishes the index again for the tracks selected now.
    fn republish(&mut self, ctx: &Ctx) {
        let Some(reader) = self.reader.as_ref() else {
            return;
        };
        let on: Vec<bool> = (0..self.pad_tracks.len()).map(|i| reader.is_on(i)).collect();
        let tables = reader.tables();
        let index = self.indexed(&tables, &|i| on.get(i).is_some_and(|on| *on));
        ctx.publish_index(index);
    }

    /// A restart drops what the track parked for a full pool, it was read ahead of the restart
    /// and would go out ahead of the codec head and again when read. The other tracks keep
    /// theirs, their gates resume past them.
    fn relink(&mut self, ctx: &Ctx, pad: SrcPad, on: bool, restart: Option<SeekTarget>) {
        let of = |pt: &PadTrack| ctx.stream_pad(pt.stream) == Some(pad);
        let Some(idx) = self.pad_tracks.iter().position(of) else {
            return;
        };
        self.unplan_all();
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        match restart.filter(|_| on) {
            Some(target) => {
                let old = self.pad_tracks[idx].pad;
                self.pending.retain(|c| c.pad != pad && c.pad != old);
                reader.rejoin(idx, target.to_byte, target.at.nanos().unwrap_or(0));
            }
            None => reader.select(idx, on),
        }
        let head =
            reader.tracks().get(idx).and_then(|t| Self::track_wiring(t, self.passthrough).ok());
        let pt = &mut self.pad_tracks[idx];
        (pt.pad, pt.started) = (pad, false);
        pt.codec_head = head.map(|(_, head, _)| head).unwrap_or_default();
    }
}

impl Mp4Demux {
    fn greet(&mut self, ctx: &mut Ctx, pad: SrcPad) {
        let Some(idx) = self.pad_tracks.iter().position(|pt| pt.pad == pad) else {
            return;
        };
        let track = self.reader.as_ref().and_then(|reader| reader.tracks().get(idx));
        if let Some((_, head, _)) = track.and_then(|t| Self::track_wiring(t, self.passthrough).ok())
        {
            Self::emit_exact(ctx, pad, (None, None), BufferFlags::HEADER, &head, 0);
        }
    }
}

impl Element for Mp4Demux {
    fn desc(&self) -> &'static ElementDesc {
        &DEMUX_DESC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let reader = match self.reader.take() {
            Some(reader) => reader,
            None => {
                let r = Mp4Reader::bounded(&self.head, &[], self.limits)
                    .map_err(|e| Error::element_failed(ctx.element(), e))?;
                // the tables hold all the run needs, a big moov stays only once
                drop(std::mem::take(&mut self.head));
                r
            }
        };
        for (i, track) in reader.tracks().iter().enumerate() {
            let (family, codec_head, reframer) = Self::track_wiring(track, self.passthrough)
                .map_err(|e| Error::element_failed(ctx.element(), e))?;
            let announce = Self::announce_of(track, family, self.passthrough);
            log_track(ctx, track, family, announce);
            let name = format!("src_track{}", track.track_id);
            let stream = ctx.add_stream(Stream {
                kind: match track.kind() {
                    crate::reader::TrackKind::Video => StreamKind::Video,
                    crate::reader::TrackKind::Audio => StreamKind::Audio,
                    _ if family.starts_with("subtitle/") => StreamKind::Text,
                    _ => StreamKind::Other,
                },
                codec: track.family(),
                language: match track.language_tag.is_empty() {
                    true => std::str::from_utf8(&track.language).unwrap_or("und").into(),
                    false => track.language_tag.as_str().into(),
                },
                title: track.title.as_str().into(),
                // ffmpeg marks the default track of each kind enabled and clears the
                // others, so `track_enabled` is the default flag here.
                default: track.enabled,
                forced: track.forced,
                disposition: track.disposition,
                channels: track.entry.channels.min(u32::from(u16::MAX)) as u16,
                duration: if track.duration_ns == 0 {
                    Timestamp::NONE
                } else {
                    Timestamp(track.duration_ns)
                },
                sparse: !matches!(
                    track.kind(),
                    crate::reader::TrackKind::Video | crate::reader::TrackKind::Audio
                ),
                name: name.into(),
                offers: &DEMUX_SRC_OFFERS,
            });
            self.pad_tracks.push(PadTrack {
                stream,
                pad: SrcPad(u32::MAX),
                timescale: track.timescale,
                family,
                codec_head,
                reframer,
                announce,
                started: false,
                delay_ticks: track.delay_ticks(),
                text: text_of(track),
                duration_ns: track.duration_ns,
            });
            if let Some(refused) = track.edit_refused {
                let element = ctx.element();
                let error = Error::element_failed(
                    element,
                    Mp4Error::EditList { track: track.track_id, refused },
                );
                ctx.post(BusMessage::Warning { element, error });
            }
            let _ = i;
        }
        let tables = reader.tables();
        if tables.skipped_tracks() > 0 {
            log!(&*ctx, Level::Warn, "tracks_skipped", count = tables.skipped_tracks());
        }
        // No pad is linked at the open, the start publishes again for the caption tracks that are
        let index = self.indexed(&tables, &|_| false);
        log!(
            &*ctx,
            Level::Info,
            "opened",
            tracks = self.pad_tracks.len(),
            samples = tables.samples().len(),
            fragmented = reader.is_fragmented(),
            duration = index.duration,
        );
        match index.entries.len() {
            0 => log!(&*ctx, Level::Warn, "index_empty", complete = index.complete),
            n => log!(&*ctx, Level::Info, "index", entries = n, complete = index.complete),
        }
        ctx.publish_index(index);
        self.reader = Some(reader);
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.out_fmt = ctx.out_format();
        self.dropped = 0;
        if self.reader.as_ref().is_some_and(|r| !r.is_fragmented()) {
            ctx.want_ranges(Self::IN);
            let stats = ctx.pool_stats();
            let most = |n: u64| n - n / 4;
            let bytes = most(stats.bytes_budget).clamp(1, profluens_core::memory::IO_DEPTH_BYTES);
            let beside = within(stats.bytes_budget.saturating_sub(bytes));
            let small = if self.passthrough { SMALL } else { SMALL.min(beside) };
            self.ask_caps = AskCaps { bytes, ranges: most(stats.max_slots).max(1) as u32, small };
        }
        for (i, pt) in self.pad_tracks.iter_mut().enumerate() {
            let pad = ctx.stream_pad(pt.stream);
            pt.pad = pad.unwrap_or(SrcPad(u32::MAX));
            if let Some(reader) = self.reader.as_mut() {
                reader.select(i, pad.is_some_and(|pad| ctx.pad_linked(pad)));
            }
        }
        self.republish(ctx);
        self.restart(ctx, None)
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if ctx.pull_reset(Self::IN).is_some() {
            self.unplan_all();
            if !ctx.pulls(Self::IN) {
                self.restart(ctx, ctx.seek_target())?;
            }
        }
        if ctx.pulls(Self::IN) {
            let flow = self.pull(ctx, &mut inputs);
            self.check(ctx)?;
            self.tell_rate(ctx);
            return flow;
        }
        // Backpressure. Consume input only while emission keeps up.
        let drained = self.drain(ctx);
        self.check(ctx)?;
        if !drained {
            ctx.hold_end();
            return Ok(Flow::Ok);
        }
        while let Some(buf) = inputs.pop() {
            // Retaining the chunk keeps its pool slot outstanding, the demuxer's backpressure.
            if let Some(reader) = self.reader.as_mut() {
                if buf.flags.contains(BufferFlags::SEGMENT) {
                    let reset = buf.flags.contains(BufferFlags::DISCONT);
                    log!(&*ctx, Level::Debug, "segment", reset = reset, pts = buf.pts);
                    reader.mark(reset, buf.pts.nanos(), buf.dts.nanos());
                }
                reader.push(buf.memory);
            }
            let drained = self.drain(ctx);
            self.check(ctx)?;
            if !drained {
                ctx.hold_end();
                break;
            }
        }
        self.tell_rate(ctx);
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => {
                self.flush_exact(ctx);
                match self.dropped {
                    0 => log!(&*ctx, Level::Info, "eos", tracks = self.pad_tracks.len()),
                    n => {
                        log!(&*ctx, Level::Warn, "eos", tracks = self.pad_tracks.len(), dropped = n)
                    }
                }
            }
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", seek = ctx.seek_target().is_some());
                self.teller.restart();
                self.restart(ctx, ctx.seek_target())?;
            }
            Event::SrcLinked { pad, on, restart } => {
                self.relink(ctx, *pad, *on, *restart);
                self.republish(ctx);
            }
            Event::SrcJoined { pad } => self.greet(ctx, *pad),
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.flush_exact(ctx);
        if let Some(reader) = self.reader.as_mut() {
            reader.reset_stream();
        }
        self.pending.clear();
        self.plan.reset();
    }
}

fn log_track(ctx: &Ctx, track: &crate::reader::Track, family: &'static str, ann: Announce) {
    let (id, fourcc) = (track.track_id, track.entry.kind);
    if family == FAMILY_BYTES {
        log!(ctx, Level::Warn, "track_unknown", track = id, fourcc = fourcc);
        return;
    }
    match ann {
        Announce::Video { width, height, .. } => {
            log!(
                ctx,
                Level::Info,
                "track",
                track = id,
                fourcc = fourcc,
                width = width,
                height = height
            )
        }
        Announce::Audio { rate, channels, .. } => {
            log!(
                ctx,
                Level::Info,
                "track",
                track = id,
                fourcc = fourcc,
                rate = rate,
                channels = channels
            )
        }
        Announce::Bare => {
            log!(ctx, Level::Info, "track", track = id, family = family, fourcc = fourcc)
        }
    }
}

// Cold path, only on pool exhaustion.
#[allow(clippy::disallowed_methods)]
fn own_remainder(bytes: &[u8], off: usize) -> Vec<u8> {
    bytes[off..].to_vec()
}

/// The payload of a sample of no bytes. `Vec::new` allocates nothing.
#[allow(clippy::disallowed_methods)]
fn empty() -> Vec<u8> {
    Vec::new()
}
