//! `MkvMux` and `MkvDemux`, the Matroska elements around the writer and the reader.

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, SeekTarget, Stream, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{FormatId, SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::{Known, Memory, RESERVE_FLOOR, SIZED_TOP};
use profluens_core::pipeline::Index;
use profluens_core::pull::Ask;
use profluens_core::rate;
use profluens_core::time::Timestamp;

use crate::codec::{self, Reframer};
use crate::config::{self, ConfigError, NalCodec, ParamSets};
use crate::cued::Cued;
use crate::error::{DemuxError, MkvError, MuxError};
use crate::extras::{Extras, MkvExtras, parse_attachments, parse_chapters};
use crate::index::{Grown, cue_entries};
use crate::pull::{Plan, Planned, State, Step, Walk, Window};
use crate::reader::{
    Cue, Frame, MatroskaReader, PTS_NONE, Track, cues_demand, parse_cue_tracks, parse_seek_head,
    seek_head_pos,
};
use crate::writer::{MatroskaWriter, MuxOut, TrackConfig};

const TRACK: u64 = 1;

/// Largest FLAC head absorbed as CodecPrivate. A block is at most 16 MiB (RFC 9639 section 8.1) and
/// their count is open.
pub(crate) const MAX_FLAC_HEAD: usize = 64 << 20;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
/// The sink declares every field and value name its announcements carry, since declaring is what
/// interns them.
static MUX_SAMPLE_VALUES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];
static MUX_AUDIO_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: "sample",
        allowed: ConstraintDesc::Set(&MUX_SAMPLE_VALUES),
        preferred: None,
    },
];
static MUX_VIDEO_FIELDS: [FieldDesc; 13] = {
    let c = profluens_video::format::COLOR_OFFER_FIELDS;
    [
        FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc { field: "duration", allowed: ConstraintDesc::Any, preferred: None },
        c[0],
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[7],
        c[8],
        c[9],
    ]
};
static MUX_SINK_OFFERS: [OfferDesc; 9] = [
    OfferDesc { family: "flac", fields: &MUX_AUDIO_FIELDS },
    OfferDesc { family: "vp8", fields: &MUX_VIDEO_FIELDS },
    OfferDesc { family: "vp9", fields: &MUX_VIDEO_FIELDS },
    OfferDesc { family: "h264/annexb", fields: &MUX_VIDEO_FIELDS },
    OfferDesc { family: "h265/annexb", fields: &MUX_VIDEO_FIELDS },
    OfferDesc { family: "av1", fields: &MUX_VIDEO_FIELDS },
    // Length-prefixed samples led by the raw config record (RFC 9559 section 12).
    OfferDesc { family: "h264/avcc", fields: &MUX_VIDEO_FIELDS },
    OfferDesc { family: "h265/hvcc", fields: &MUX_VIDEO_FIELDS },
    OfferDesc::any("bytes"),
];

static MUX_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &MUX_SINK_OFFERS }];
static MUX_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static MUX_DESC: ElementDesc = ElementDesc {
    name: "mkvmux",
    sinks: &MUX_SINKS,
    srcs: &MUX_SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(MkvMux::from_caps())),
    io: false,
    couplings: &[],
};

enum Setup {
    Fixed,
    AwaitCaps,
    FlacHead {
        head: Vec<u8>,
        scanned: usize,
    },
    NalHead {
        codec_id: &'static str,
        width: u32,
        height: u32,
        duration_ns: Option<u64>,
        colour: Option<crate::writer::ColourConfig>,
    },
    Built {
        video: BuiltVideo,
        sets: Option<ParamSets>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct BuiltVideo {
    pub(crate) framing: Framing,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) duration_ns: Option<u64>,
    pub(crate) colour: Option<crate::writer::ColourConfig>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    Verbatim,
    AnnexB(NalCodec),
    Av1,
}

impl Framing {
    pub(crate) fn block(self, ctx: &mut Ctx, mem: Memory) -> Option<Memory> {
        match self {
            Framing::Verbatim => Some(mem),
            Framing::AnnexB(codec) => config::length_prefixed_sample(ctx, codec, mem),
            Framing::Av1 => config::av1_sample(mem),
        }
    }

    pub(crate) fn of(family: &str) -> Option<Framing> {
        match family {
            "h264/annexb" => Some(Framing::AnnexB(NalCodec::H264)),
            "h265/annexb" => Some(Framing::AnnexB(NalCodec::H265)),
            "av1" => Some(Framing::Av1),
            _ => None,
        }
    }
}

impl BuiltVideo {
    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn absorb(
        &self,
        sets: &mut Option<ParamSets>,
        track: u64,
        data: &[u8],
    ) -> Result<Option<TrackConfig>, MuxError> {
        let (codec_id, record, size) = match self.framing {
            Framing::AnnexB(codec) => {
                let sets = sets.get_or_insert_with(|| ParamSets::new(codec));
                sets.absorb(data).map_err(MuxError::Config)?;
                if !config::scan(codec, data).vcl {
                    return Ok(None);
                }
                let id =
                    if codec == NalCodec::H264 { "V_MPEG4/ISO/AVC" } else { "V_MPEGH/ISO/HEVC" };
                let record = match sets.record() {
                    // A capture that starts mid-GOP, pictures before the sets cannot be decoded.
                    Err(ConfigError::NoSps | ConfigError::NoPps) => return Ok(None),
                    record => record.map_err(MuxError::Config)?,
                };
                (id, record, sets.size())
            }
            Framing::Av1 => {
                let (record, seq) = config::av1c(data).map_err(MuxError::Config)?;
                ("V_AV1", record, Some((seq.width, seq.height)))
            }
            Framing::Verbatim => return Ok(None),
        };
        let (w, h) = match (self.width, self.height, size) {
            (0, _, Some(wh)) | (_, 0, Some(wh)) => wh,
            (w, h, _) => (w, h),
        };
        if w == 0 || h == 0 {
            // PixelWidth 0 is no Matroska, the caps or the stream must say the size.
            return Err(MuxError::VideoSizeMissing);
        }
        let mut tc = TrackConfig::video(track, codec_id, record, w, h);
        if let Some(v) = tc.video.as_mut() {
            v.colour = self.colour;
        }
        Ok(Some(tc))
    }
}

/// Muxes one track into Matroska, one encoded frame per input buffer.
pub struct MkvMux {
    track: Option<TrackConfig>,
    setup: Setup,
    writer: Option<MatroskaWriter>,
    done: bool,
    header_done: bool,
    next_ts_ns: u64,
    frame_dur_ns: u64,
    /// The end of the last frame, what the reserved Duration is patched to.
    end_ns: u64,
    out: MuxOut,
    out_off: usize,
    out_fmt: FormatId,
    framing: Framing,
    /// Frames the writer refused, lost from the file.
    lost: u64,
}

impl MkvMux {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    /// One `A_FLAC` track from its native FLAC head (`fLaC` + STREAMINFO).
    pub fn flac(
        codec_private: Vec<u8>,
        sampling_frequency: f64,
        channels: u32,
        bit_depth: u32,
    ) -> Self {
        Self::with_track(TrackConfig::flac(
            TRACK,
            codec_private,
            sampling_frequency,
            channels,
            bit_depth,
        ))
    }

    /// An explicit single-track config. The track number is forced to `TRACK`.
    pub fn with_track(mut track: TrackConfig) -> Self {
        track.track_number = TRACK;
        let frame_dur_ns = Self::frame_duration_ns(&track);
        Self {
            track: Some(track),
            setup: Setup::Fixed,
            writer: None,
            done: false,
            header_done: false,
            next_ts_ns: 0,
            frame_dur_ns,
            end_ns: 0,
            out: MuxOut::new(),
            out_off: 0,
            out_fmt: FormatId(0), // probed at `start`
            framing: Framing::Verbatim,
            lost: 0,
        }
    }

    /// A caps-driven muxer, the track built from the upstream announcement.
    pub fn from_caps() -> Self {
        Self {
            track: None,
            setup: Setup::AwaitCaps,
            writer: None,
            done: false,
            header_done: false,
            next_ts_ns: 0,
            frame_dur_ns: 0,
            end_ns: 0,
            out: MuxOut::new(),
            out_off: 0,
            out_fmt: FormatId(0), // probed at `start`
            framing: Framing::Verbatim,
            lost: 0,
        }
    }

    pub fn track(&self) -> Option<&TrackConfig> {
        self.track.as_ref()
    }

    fn configure(&mut self, mut track: TrackConfig, duration_ns: Option<u64>) {
        track.track_number = TRACK;
        self.frame_dur_ns = Self::frame_duration_ns(&track);
        let mut writer = Self::writer_for(&track);
        if let Some(ns) = duration_ns {
            writer.set_duration_ns(ns);
        }
        self.writer = Some(writer);
        self.track = Some(track);
        self.setup = Setup::Fixed;
    }

    /// Cues always on, without them players bisect the Clusters to seek, and a Duration to
    /// patch at the end unless the caps said one.
    // Cold path, once per stream.
    #[allow(clippy::disallowed_methods)]
    fn writer_for(track: &TrackConfig) -> MatroskaWriter {
        let mut writer = MatroskaWriter::new(vec![track.clone()]);
        writer.enable_cues();
        writer.reserve_duration();
        writer
    }

    /// Length of a complete native FLAC head (RFC 9639 section 8), `None` while `b` is a prefix of
    /// one. `scanned` is the resume cursor, zero for a fresh head.
    pub fn flac_head_len(b: &[u8], scanned: &mut usize) -> Result<Option<usize>, MuxError> {
        if b.len() >= 4 && &b[..4] != b"fLaC" {
            return Err(MuxError::FlacMagic);
        }
        if b.len() > MAX_FLAC_HEAD {
            return Err(MuxError::FlacHeadTooLarge { len: b.len() });
        }
        if b.len() < 4 {
            return Ok(None);
        }
        let mut off = (*scanned).max(4);
        loop {
            if b.len() < off.saturating_add(4) {
                *scanned = off;
                return Ok(None);
            }
            let last = b[off] & 0x80 != 0;
            let len = u32::from_be_bytes([0, b[off + 1], b[off + 2], b[off + 3]]) as usize;
            let next = off + 4 + len;
            if last {
                *scanned = off;
                return Ok(if b.len() >= next { Some(next) } else { None });
            }
            off = next;
        }
    }

    /// Audio params from the STREAMINFO of a native FLAC head (RFC 9639 section 8.2).
    pub fn flac_streaminfo_params(head: &[u8]) -> Result<(f64, u32, u32), MuxError> {
        let si = head.get(8..8 + 34).ok_or(MuxError::FlacHeadShort { len: head.len() })?;
        if head[4] & 0x7f != 0 {
            return Err(MuxError::FlacNotStreamInfo { block_type: head[4] & 0x7f });
        }
        let rate = ((si[10] as u32) << 12) | ((si[11] as u32) << 4) | ((si[12] as u32) >> 4);
        let channels = (((si[12] >> 1) & 0x7) as u32) + 1;
        let bits = ((((si[12] & 1) as u32) << 4) | ((si[13] as u32) >> 4)) + 1;
        if rate == 0 {
            return Err(MuxError::FlacZeroRate);
        }
        Ok((rate as f64, channels, bits))
    }

    /// The spacing of buffers that carry no PTS, one frame of the codec at the track rate. Video
    /// stays at 0, its frames are stamped upstream.
    pub(crate) fn frame_duration_ns(track: &TrackConfig) -> u64 {
        let samples = match track.codec_id.as_str() {
            CODEC_A_FLAC => 4096.0,
            "A_AAC" => 1024.0,
            "A_OPUS" => 960.0,
            _ => return 0,
        };
        let rate = track.audio.sampling_frequency;
        if rate > 0.0 { (samples * 1_000_000_000.0 / rate) as u64 } else { 0 }
    }

    /// Coalescing, a block header and its payload share a slot where they fit.
    fn drain_out(&mut self, ctx: &mut Ctx) -> bool {
        crate::drain::drain(&mut self.out, &mut self.out_off, ctx, Self::OUT, self.out_fmt)
    }

    fn drain_out_exact(&mut self, ctx: &mut Ctx) {
        crate::drain::drain_exact(&mut self.out, &mut self.out_off, ctx, Self::OUT, self.out_fmt);
    }

    fn finish_stream(&mut self, ctx: &mut Ctx) {
        self.done = true;
        let (seek, duration) = match self.writer.take() {
            Some(mut writer) => {
                let duration = writer.duration_patch(self.end_ns);
                (writer.finalize_scatter(&mut self.out), duration)
            }
            None => (None, None),
        };
        self.drain_out_exact(ctx);
        if self.lost > 0 {
            log!(&*ctx, Level::Error, "frames_lost", count = std::mem::take(&mut self.lost));
        }
        // The SeekHead and Duration patches go after every stream byte.
        for p in [seek, duration].into_iter().flatten() {
            let Some(mut buf) = ctx.alloc_exact(p.bytes.len()) else {
                log!(&*ctx, Level::Error, "exact_refused", bytes = p.bytes.len());
                continue;
            };
            buf.memory.as_mut_full()[..p.bytes.len()].copy_from_slice(&p.bytes);
            buf.memory.set_len(p.bytes.len());
            ctx.push_event(Self::OUT, Event::Patch { offset: p.offset, data: buf.memory });
        }
    }
}

impl Element for MkvMux {
    fn desc(&self) -> &'static ElementDesc {
        &MUX_DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let (Setup::Fixed, Some(track)) = (&self.setup, &self.track) {
            self.writer = Some(Self::writer_for(track));
        }
        self.done = false;
        self.header_done = false;
        (self.next_ts_ns, self.end_ns, self.lost) = (0, 0, 0);
        self.out.clear();
        self.out_off = 0;
        self.out_fmt = ctx.out_format();
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.drain_out(ctx) {
            return Ok(Flow::Ok);
        }
        while let Some(buf) = inputs.pop() {
            if self.done {
                break;
            }
            if self.writer.is_none() {
                match &mut self.setup {
                    Setup::FlacHead { head, scanned } => {
                        head.extend_from_slice(buf.memory.data());
                        match Self::flac_head_len(head, scanned).map_err(|e| mux_failed(ctx, e))? {
                            None => continue, // head still incomplete, keep absorbing
                            Some(len) => {
                                if len != head.len() {
                                    // The head must end on a buffer boundary, bytes past it would
                                    // be frames we cannot re-split.
                                    return Err(mux_failed(
                                        ctx,
                                        MuxError::FlacHeadSplit { head: len, buffered: head.len() },
                                    ));
                                }
                                let head = std::mem::take(head);
                                let (rate, channels, bits) = Self::flac_streaminfo_params(&head)
                                    .map_err(|e| mux_failed(ctx, e))?;
                                self.configure(
                                    TrackConfig::flac(TRACK, head, rate, channels, bits),
                                    None,
                                );
                                continue; // the head is CodecPrivate, never a block
                            }
                        }
                    }
                    Setup::NalHead { codec_id, width, height, duration_ns, colour } => {
                        // The first buffer is the raw config record (RFC 9559 section 12).
                        // Cold path, once per stream.
                        #[allow(clippy::disallowed_methods)]
                        let head = buf.memory.data().to_vec();
                        if head.first() != Some(&1) {
                            return Err(mux_failed(
                                ctx,
                                MuxError::ConfigRecord { version: head.first().copied() },
                            ));
                        }
                        let (codec_id, w, h, dur, col) =
                            (*codec_id, *width, *height, *duration_ns, *colour);
                        let mut tc = TrackConfig::video(TRACK, codec_id, head, w, h);
                        if let Some(v) = tc.video.as_mut() {
                            v.colour = col;
                        }
                        self.configure(tc, dur);
                        continue; // the record is CodecPrivate, never a block
                    }
                    Setup::Built { video, sets } => {
                        let video = *video;
                        let built = video
                            .absorb(sets, TRACK, buf.memory.data())
                            .map_err(|e| mux_failed(ctx, e))?;
                        let Some(tc) = built else { continue };
                        self.configure(tc, video.duration_ns);
                        self.framing = video.framing;
                    }
                    Setup::AwaitCaps => {
                        return Err(mux_failed(ctx, MuxError::DataBeforeFormat));
                    }
                    Setup::Fixed => break, // unreachable: Fixed builds the writer at start
                }
            }
            let (pts, flags, duration) = (buf.pts, buf.flags, buf.duration);
            let Some(block) = self.framing.block(ctx, buf.memory) else {
                log!(&*ctx, Level::Trace, "block_empty", pts = pts);
                continue;
            };

            let ts_ns = match pts.nanos() {
                None => {
                    let t = self.next_ts_ns;
                    self.next_ts_ns = t.saturating_add(self.frame_dur_ns);
                    t
                }
                Some(t) => {
                    // Keep the synth clock ahead of any real PTS. Saturating, the PTS is untrusted.
                    self.next_ts_ns = t.saturating_add(self.frame_dur_ns);
                    t
                }
            };
            // Untagged means unknown, so a block is a keyframe unless tagged DELTA.
            let keyframe = !flags.contains(BufferFlags::DELTA);
            let dur = duration.nanos().unwrap_or(self.frame_dur_ns);
            self.end_ns = self.end_ns.max(ts_ns.saturating_add(dur));

            let writer = self.writer.as_mut().expect("writer present");

            if !self.header_done {
                // Fails only on a track list it was not built from.
                if writer.write_header_scatter(&mut self.out).is_err() {
                    log!(&*ctx, Level::Error, "header_failed");
                }
                self.header_done = true;
            }

            if writer.write_frame_scatter(&mut self.out, TRACK, ts_ns, block, keyframe).is_err() {
                if self.lost == 0 {
                    log!(&*ctx, Level::Error, "frame_lost", pts = pts);
                }
                self.lost += 1;
            }

            if !self.drain_out(ctx) {
                return Ok(Flow::Ok); // pool dry mid-drain, stop consuming input
            }
        }
        Ok(Flow::Ok)
    }

    // Cold path, EOS finalize and once-per-stream track config.
    #[allow(clippy::disallowed_methods)]
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => self.finish_stream(ctx),
            // Only the first announcement configures, a track is fixed once the header is written.
            Event::FormatChange(f) => match &self.setup {
                Setup::AwaitCaps => {
                    let family = ctx.family_name(f.family).unwrap_or("?").to_owned();
                    let dim = |name: &str| {
                        ctx.field_id(name).and_then(|id| f.get(id)).and_then(|v| match v {
                            Value::Int(n) if n > 0 => Some(n as u32),
                            _ => None,
                        })
                    };
                    let duration_ns =
                        ctx.field_id("duration").and_then(|id| f.get(id)).and_then(|v| match v {
                            Value::Int(n) if n > 0 => Some(n as u64),
                            _ => None,
                        });
                    let colour = crate::color_map::colour_from_format(ctx, f);
                    match family.as_str() {
                        "flac" => self.setup = Setup::FlacHead { head: Vec::new(), scanned: 0 },
                        "vp8" | "vp9" => {
                            let (Some(w), Some(h)) = (dim("width"), dim("height")) else {
                                return Err(mux_failed(ctx, MuxError::VideoSizeMissing));
                            };
                            let id = if family == "vp8" { "V_VP8" } else { "V_VP9" };
                            let mut tc = TrackConfig::video(TRACK, id, Vec::new(), w, h);
                            if let Some(v) = tc.video.as_mut() {
                                v.colour = colour;
                            }
                            self.configure(tc, duration_ns);
                        }
                        "h264/avcc" | "h265/hvcc" => {
                            let (Some(w), Some(h)) = (dim("width"), dim("height")) else {
                                return Err(mux_failed(ctx, MuxError::VideoSizeMissing));
                            };
                            let codec_id = if family == "h264/avcc" {
                                "V_MPEG4/ISO/AVC"
                            } else {
                                "V_MPEGH/ISO/HEVC"
                            };
                            self.setup = Setup::NalHead {
                                codec_id,
                                width: w,
                                height: h,
                                duration_ns,
                                colour,
                            };
                        }
                        fam if Framing::of(fam).is_some() => {
                            let framing = Framing::of(fam).expect("matched");
                            let (width, height) =
                                (dim("width").unwrap_or(0), dim("height").unwrap_or(0));
                            let video = BuiltVideo { framing, width, height, duration_ns, colour };
                            self.setup = Setup::Built { video, sets: None };
                        }
                        other => {
                            return Err(mux_failed(
                                ctx,
                                MuxError::UnsupportedFamily { family: other.into() },
                            ));
                        }
                    }
                }
                Setup::Fixed
                | Setup::FlacHead { .. }
                | Setup::NalHead { .. }
                | Setup::Built { .. } => {
                    // A repeat of the negotiated format, a flush or seek re-announces, is harmless.
                    let same = ctx
                        .negotiated_sink(Self::IN)
                        .is_some_and(|g| crate::mux_multi::fixed_eq(g, f));
                    if self.header_done && !same {
                        return Err(mux_failed(ctx, MuxError::FormatChangeAfterHeader));
                    }
                }
            },
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.finish_stream(ctx);
        self.out = MuxOut::new();
        self.out_off = 0;
    }
}

const CODEC_A_FLAC: &str = "A_FLAC";

const FAMILY_FLAC: &str = "flac";

static DEMUX_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

static DEMUX_DESC: ElementDesc = ElementDesc {
    name: "mkvdemux",
    sinks: &DEMUX_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct PadTrack {
    track_number: u64,
    stream: u32,
    pad: SrcPad,
    linked: bool,
    last: Option<(u64, u16)>,
    family: &'static str,
    codec_head: Vec<u8>,
    reframer: Reframer,
    track: Track,
    started: bool,
    kind: Kind,
    /// Its packets are timed by their TOC, so a DiscardPadding reaches the decoder.
    opus: bool,
    pre_roll_ns: u64,
    gate: Gate,
    held: Vec<Out>,
    gap: Option<u64>,
}

const GAP_EVERY_NS: u64 = 500_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Video,
    Audio,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    Open,
    Keyframe {
        from: u64,
    },
    /// Holds the newest block at or before `line`, the presentation point less pre-roll.
    Line {
        line: u64,
    },
    /// Drops what was already put out, up to the `left`th frame of the run stamped `pts`, an
    /// unstamped laced frame counting with the stamped one before it. `inside` is in that run.
    Resume {
        pts: u64,
        left: u16,
        inside: bool,
    },
}

/// Pictures are stamped out of order by less than this.
const RESUME_SLACK: u64 = 10_000_000_000;

/// What a [`Gate`] does with a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pass {
    Out,
    Drop,
    /// Joins the held lace.
    Hold,
    /// Drops what is held and is held instead.
    Replace,
    /// Goes out after what is held, the gate open.
    Release,
}

/// The gate state machine. `held` is the first held frame's stamp and the count held.
fn pass(
    gate: &mut Gate,
    kind: Kind,
    (pts, key): (u64, bool),
    held: (Option<u64>, usize),
    tick: u64,
) -> Pass {
    let stamped = pts != PTS_NONE;
    let (verdict, next) = match *gate {
        Gate::Open => (Pass::Out, Gate::Open),
        // A tick of slack, for a muxer that rounded the cue and block times apart.
        Gate::Keyframe { from } if stamped && key && pts.saturating_add(tick) >= from => {
            (Pass::Out, Gate::Open)
        }
        Gate::Keyframe { .. } => (Pass::Drop, *gate),
        // A lace's unstamped frames go with its first, held or not.
        Gate::Line { .. } if !stamped => match held {
            (Some(_), n) if n < MAX_HELD => (Pass::Hold, *gate),
            _ => (Pass::Drop, *gate),
        },
        Gate::Line { line } if pts <= line => match held {
            (Some(first), n) if first == pts && n < MAX_HELD => (Pass::Hold, *gate),
            _ => (Pass::Replace, *gate),
        },
        Gate::Line { .. } => (Pass::Release, Gate::Open),
        // A sparse track whose last frame lies ahead of the restart byte never reads it again.
        Gate::Resume { pts: out, .. } if stamped && kind != Kind::Video && pts > out => {
            (Pass::Out, Gate::Open)
        }
        // Past every picture put out, so this one was not.
        Gate::Resume { pts: out, .. } if stamped && pts > out.saturating_add(RESUME_SLACK) => {
            (Pass::Out, Gate::Open)
        }
        Gate::Resume { pts: out, left, inside } => {
            let run = pts == out || (!stamped && inside);
            let next = match (run, left <= 1) {
                (true, true) => Gate::Open,
                (true, false) => Gate::Resume { pts: out, left: left - 1, inside: true },
                (false, _) => Gate::Resume { pts: out, left, inside: false },
            };
            (Pass::Drop, next)
        }
    };
    *gate = next;
    verdict
}

/// A lace is at most 256 frames (RFC 9559 section 10.3).
const MAX_HELD: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Emit {
    Bounded,
    Exact,
}

enum Bytes {
    Owned(Vec<u8>),
    View(Memory, bool),
}

struct Out {
    track_number: u64,
    pts_ns: u64,
    duration_ns: Option<u64>,
    keyframe: bool,
    bytes: Bytes,
    t35: Bytes,
    discard_ns: u64,
}

impl Out {
    fn owned(frame: Frame) -> Out {
        let Frame { track_number, pts_ns, duration_ns, keyframe, data, t35, discard_ns } = frame;
        Out {
            track_number,
            pts_ns,
            duration_ns,
            keyframe,
            bytes: Bytes::Owned(data),
            t35: Bytes::Owned(t35),
            discard_ns,
        }
    }

    /// Views into a window let go, the bytes copied into the reader's recycled buffers.
    fn owned_in(mut self, reader: &mut MatroskaReader) -> Out {
        if let Bytes::View(view, _) = &self.bytes {
            self.bytes = Bytes::Owned(reader.copy(view.data()));
        }
        if let Bytes::View(view, _) = &self.t35 {
            self.t35 = Bytes::Owned(reader.copy(view.data()));
        }
        self
    }
}

struct Pulled {
    walk: Walk,
    plan: Plan,
    win: Option<Window>,
    caps: (u64, u32),
    lace: Vec<usize>,
    stash: Option<Step>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CuesRead {
    Done,
    Want { at: u64, len: u32 },
    Asked { tag: u64, at: u64, len: u32 },
}

const CUES_FIRST: u32 = 64 << 10;

/// Cues past this are not read. A file of many subtitle tracks cues every block of each, 9 MB
/// for half an hour of 36 tracks.
const MAX_CUES: u64 = 32 << 20;

const SIDE_FIRST: u32 = 64 << 10;

const MAX_ATTACHED: u64 = 64 << 20;
const MAX_CHAPTERED: u64 = 1 << 20;

struct Side {
    read: [CuesRead; 2],
    now: Extras,
    publisher: profluens_core::publish::Publisher<Extras>,
    handle: MkvExtras,
}

impl Side {
    const IDS: [&'static [u8]; 2] = [crate::ebml::id::ATTACHMENTS, crate::ebml::id::CHAPTERS];
    const MAX: [u64; 2] = [MAX_ATTACHED, MAX_CHAPTERED];

    fn take(&mut self, kind: usize, element: &[u8]) {
        match kind {
            0 => self.now.attachments = parse_attachments(element).into(),
            _ => self.now.chapters = parse_chapters(element).into(),
        }
    }

    fn publish(&mut self) {
        self.now.settled = self.read.iter().all(|r| *r == CuesRead::Done);
        self.publisher.publish(self.now.clone());
    }
}

/// Bytes or entries the demuxer owns, or shares with the other runs over one file.
enum Held<T: 'static> {
    Own(Vec<T>),
    Shared(std::sync::Arc<[T]>),
}

impl<T> std::ops::Deref for Held<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            Held::Own(v) => v,
            Held::Shared(a) => a,
        }
    }
}

struct Seek {
    tracks: Vec<Track>,
    tick: u64,
    segment_data_start: u64,
    read: CuesRead,
    cues: Held<Cue>,
    whole: bool,
    grown: Grown,
    file_len: Option<u64>,
    duration: Timestamp,
    due: bool,
    reserve: Option<u64>,
    /// The read under way is of a SeekHead past the header, to find the Cues in.
    hop: bool,
}

#[derive(Clone, Copy)]
struct Stamp {
    pad: SrcPad,
    pts_ns: u64,
    duration_ns: Option<u64>,
    flags: BufferFlags,
}

/// Demultiplexes a Matroska/WebM byte stream into one src pad per track. Header bytes come
/// at construction since a mid-pipeline element gets no input during preroll.
pub struct MkvDemux {
    header: Held<u8>,
    /// Attachments and Chapters are read and parsed, off for a run that has them already.
    extras: bool,
    pad_tracks: Vec<PadTrack>,
    reader: MatroskaReader,
    scratch: Vec<u8>,
    reframe_buf: Vec<u8>,
    side_buf: Box<[u8]>,
    /// Emissions stalled on pool exhaustion. While non-empty no input is consumed.
    pending: std::collections::VecDeque<Carry>,
    /// Recycled [`Carry`] buffers, so the backpressure path allocates nothing in steady state.
    park_free: Vec<Vec<u8>>,
    posted_duration: bool,
    video_from: Option<u64>,
    front: std::collections::VecDeque<Out>,
    pull: Pulled,
    seek: Seek,
    rated: Option<Index>,
    teller: rate::Teller,
    taken: u64,
    measured: rate::ByteRate,
    key_track: Option<u64>,
    side: Side,
    cued: Option<Cued>,
    cued_off: bool,
    cued_from: u64,
    walk_from: u64,
    /// Frames that failed to reframe at the end, past the bus.
    lost: u64,
    /// The largest keyframe of a linked video track read on its own lately, decaying.
    key_bytes: u32,
    hold: Hold,
}

/// A pulled start that a seek follows reads nothing until that seek, the bytes at the head of
/// the file would only be thrown away and hold the disk from the seek's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hold {
    Off,
    /// The first pull restart from the start holds.
    Armed,
    Holding,
}

/// A seek's first read holds the Cluster's head and its keyframe, so the kernel reads them as
/// one random read instead of ramping its readahead behind a small window. The keyframes seen
/// size it, or the stream's rate over [`LEAD_NS`] before any was.
const LEAD_MIN: u32 = 512 << 10;
const LEAD_MAX: u32 = 4 << 20;
const LEAD_NS: u64 = 150_000_000;
/// Room past a seen keyframe for the Cluster's head and blocks before it.
const LEAD_SLACK: u32 = 64 << 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Read {
    Walk,
    Wait,
    Cued,
}

const MAX_PARK_FREE: usize = 8;

const GROWN_SLOTS: u64 = 1024;

fn grow_pool(ctx: &mut Ctx) -> Option<u32> {
    let stats = ctx.pool_stats();
    let more = (stats.max_slots + stats.max_slots / 2).min(GROWN_SLOTS) as u32;
    (ctx.behind() && ctx.grow_slot_cap(more)).then_some(more)
}

fn recycle_park(free: &mut Vec<Vec<u8>>, buf: Vec<u8>) {
    if free.len() < MAX_PARK_FREE {
        free.push(buf);
    }
}

struct Carry {
    stamp: Stamp,
    bytes: Vec<u8>,
    off: usize,
    view: Option<Memory>,
    side: usize,
}

impl Carry {
    fn payload(&self) -> (&[u8], &[u8]) {
        self.bytes.split_at(self.bytes.len() - self.side)
    }
}

// Cold path until the free-list warms.
#[allow(clippy::disallowed_methods)]
fn own_remainder(free: &mut Vec<Vec<u8>>, bytes: &[u8]) -> Vec<u8> {
    let mut buf = free.pop().unwrap_or_default();
    buf.clear();
    buf.extend_from_slice(bytes);
    buf
}

impl MkvDemux {
    pub const IN: SinkPad = SinkPad(0);

    /// `header` must hold everything up to the first Cluster. The whole stream then arrives on the
    /// sink pad.
    pub fn new(header: Vec<u8>) -> Self {
        Self::held(Held::Own(header))
    }

    /// [`MkvDemux::new`] over a head shared with the other runs over the file, not copied.
    pub fn with_shared_head(header: std::sync::Arc<[u8]>) -> Self {
        Self::held(Held::Shared(header))
    }

    // Cold path, construction.
    #[allow(clippy::disallowed_methods)]
    fn held(header: Held<u8>) -> Self {
        let (handle, publisher) = MkvExtras::new();
        Self {
            header,
            extras: true,
            pad_tracks: Vec::new(),
            reader: MatroskaReader::new(),
            scratch: Vec::new(),
            reframe_buf: Vec::new(),
            side_buf: vec![0u8; pf_hdr::side::SIDE_CAP].into_boxed_slice(),
            pending: std::collections::VecDeque::new(),
            park_free: Vec::new(),
            posted_duration: false,
            video_from: None,
            front: std::collections::VecDeque::new(),
            pull: Pulled {
                walk: Walk::new(),
                plan: Plan::default(),
                win: None,
                caps: (0, 0),
                lace: Vec::new(),
                stash: None,
            },
            rated: None,
            teller: rate::Teller::default(),
            taken: 0,
            measured: rate::ByteRate::default(),
            key_track: None,
            seek: Seek {
                tracks: Vec::new(),
                tick: crate::DEFAULT_TIMESTAMP_SCALE,
                segment_data_start: 0,
                read: CuesRead::Done,
                cues: Held::Own(Vec::new()),
                whole: false,
                grown: Grown::default(),
                file_len: None,
                duration: Timestamp::NONE,
                due: false,
                reserve: None,
                hop: false,
            },
            side: Side { read: [CuesRead::Done; 2], now: Extras::default(), publisher, handle },
            cued: None,
            cued_off: false,
            cued_from: 0,
            walk_from: 0,
            lost: 0,
            key_bytes: 0,
            hold: Hold::Off,
        }
    }

    pub fn extras(&self) -> MkvExtras {
        self.side.handle.clone()
    }

    fn side_head(&mut self) {
        let index = crate::tags::index_segment(&self.header).filter(|_| self.extras);
        let Some(index) = index else {
            return self.side.publish();
        };
        for (kind, at) in [index.attachments, index.chapters].into_iter().enumerate() {
            let Some(at) = at else { continue };
            let whole = self.header.get(at as usize..).and_then(|el| {
                match crate::tags::element_extent(el) {
                    crate::tags::Extent::Complete { len } => Some(&el[..len]),
                    _ => None,
                }
            });
            match whole {
                Some(el) => self.side.take(kind, el),
                None => self.side.read[kind] = CuesRead::Want { at, len: SIDE_FIRST },
            }
        }
        self.side.publish();
    }

    fn ask_side(&mut self, ctx: &mut Ctx) {
        for kind in 0..2 {
            let CuesRead::Want { at, len } = self.side.read[kind] else {
                continue;
            };
            if self.pull.plan.asked_all {
                self.side.read[kind] = CuesRead::Done;
                self.side.publish();
                continue;
            }
            if ctx.asks_left(Self::IN) == 0 {
                return;
            }
            // The length is the file's, a hostile one is refused and the read given up.
            let Some(buf) = ctx.alloc_exact(len as usize) else {
                log!(&*ctx, Level::Warn, "side_refused", at = at, bytes = len);
                self.side.read[kind] = CuesRead::Done;
                self.side.publish();
                continue;
            };
            let tag = self.pull.plan.next_tag;
            if ctx.ask(Self::IN, Ask { offset: at, len, tag, buf }).is_ok() {
                self.pull.plan.next_tag += 1;
                self.side.read[kind] = CuesRead::Asked { tag, at, len };
            }
        }
    }

    fn side_in(&mut self, ctx: &Ctx, tag: u64, data: &[u8]) -> bool {
        let found = self
            .side
            .read
            .iter()
            .position(|r| matches!(r, CuesRead::Asked { tag: t, .. } if *t == tag));
        let Some(kind) = found else { return false };
        let CuesRead::Asked { at, len, .. } = self.side.read[kind] else {
            return false;
        };
        self.side.read[kind] = CuesRead::Done;
        let end = crate::ebml::read_element_header(data, 0)
            .ok()
            .filter(|h| h.id == Side::IDS[kind])
            .and_then(|h| h.size.map(|size| size.saturating_add(h.data_start as u64)));
        match end {
            Some(end)
                if end > data.len() as u64 && data.len() == len as usize && len == SIDE_FIRST =>
            {
                if end <= Side::MAX[kind] {
                    self.side.read[kind] = CuesRead::Want { at, len: end as u32 };
                    return true;
                }
                log!(ctx, Level::Debug, "side_too_large", at = at, bytes = end);
            }
            Some(_) => self.side.take(kind, data),
            None => log!(ctx, Level::Debug, "side_missing", at = at),
        }
        self.side.publish();
        true
    }

    fn settle_side(&mut self) {
        if self.side.read.iter().any(|r| *r != CuesRead::Done) {
            self.side.read = [CuesRead::Done; 2];
            self.side.publish();
        }
    }

    fn warn_undecoded(&mut self, ctx: &mut Ctx) {
        if let Some((track, cause)) = self.reader.take_undecoded() {
            let element = ctx.element();
            let error = demux_failed(ctx, DemuxError::Undecoded { track, cause });
            ctx.post(BusMessage::Warning { element, error });
        }
        if let Some(cause) = self.reader.take_damage() {
            damaged(ctx, self.reader.damage_at(), cause);
        }
    }

    /// The Cues read already, every track's, so the demuxer reads none. `at` is where they lie in
    /// the file, the end of the Clusters the pool is sized for, as when it reads them itself.
    /// A pulled run asks nothing from the start until its first seek, for a run started only
    /// to seek. One whose first restart is already a seek's goes as any.
    pub fn with_held_start(mut self) -> Self {
        self.hold = Hold::Armed;
        self
    }

    pub fn with_cues(self, cues: Vec<Cue>, at: Option<u64>) -> Self {
        self.cued_by(Held::Own(cues), at)
    }

    /// [`MkvDemux::with_cues`] over Cues shared with the other runs over the file, not copied.
    pub fn with_shared_cues(self, cues: std::sync::Arc<[Cue]>, at: Option<u64>) -> Self {
        self.cued_by(Held::Shared(cues), at)
    }

    fn cued_by(mut self, cues: Held<Cue>, at: Option<u64>) -> Self {
        self.seek.whole = !cues.is_empty();
        self.seek.reserve = at.filter(|_| self.seek.whole);
        self.seek.cues = cues;
        self
    }

    /// Neither reads nor parses the Attachments and Chapters, for a run that has them from an
    /// earlier one. [`MkvDemux::extras`] then settles empty at preroll.
    pub fn without_extras(mut self) -> Self {
        self.extras = false;
        self
    }

    // Cold path, at the head and every few dozen keyframes.
    #[allow(clippy::disallowed_methods)]
    fn publish(&mut self, ctx: &Ctx) {
        let seek = &mut self.seek;
        seek.due = false;
        let tracks = &self.pad_tracks;
        let (entries, complete, key) = match seek.cues.is_empty() {
            false => {
                let linked = |n: u64| tracks.iter().any(|pt| pt.linked && pt.track_number == n);
                let key = crate::index::key_track(&seek.cues, &seek.tracks);
                (cue_entries(&seek.cues, &seek.tracks, &linked, seek.tick), seek.whole, key)
            }
            true => {
                seek.grown.published = seek.grown.entries.len();
                (seek.grown.entries.clone(), false, seek.grown.key)
            }
        };
        let key = key.and_then(|n| tracks.iter().find(|pt| pt.track_number == n));
        let (stream, grouped) =
            (key.map(|pt| pt.stream), key.is_some_and(|pt| pt.kind == Kind::Video));
        let (file_len, duration) = (seek.file_len, seek.duration);
        let index = Index { entries, file_len, duration, complete, stream, grouped };
        self.rated = Some(index.clone());
        ctx.publish_index(index);
    }

    fn ask_cues(&mut self, ctx: &mut Ctx) {
        let CuesRead::Want { at, len } = self.seek.read else {
            return;
        };
        if self.pull.plan.asked_all {
            self.seek.read = CuesRead::Done;
            return self.publish(ctx);
        }
        let buf = match ctx.try_alloc_sized(len as usize) {
            Ok(Some(buf)) => buf,
            Ok(None) => return,
            Err(_) => {
                log!(&*ctx, Level::Warn, "cues_unread", at = at, bytes = len);
                self.seek.read = CuesRead::Done;
                return self.publish(ctx);
            }
        };
        let tag = self.pull.plan.next_tag;
        if ctx.ask(Self::IN, Ask { offset: at, len, tag, buf }).is_ok() {
            self.pull.plan.next_tag += 1;
            self.seek.read = CuesRead::Asked { tag, at, len };
        }
    }

    fn cues_in(&mut self, ctx: &mut Ctx, data: &[u8]) {
        let CuesRead::Asked { at, len, .. } = self.seek.read else {
            return;
        };
        self.seek.read = CuesRead::Done;
        let short = data.len() < len as usize;
        let want = if self.seek.hop { crate::ebml::id::SEEK_HEAD } else { crate::ebml::id::CUES };
        let header = crate::ebml::read_element_header(data, 0).ok().filter(|h| h.id == want);
        let end = header.and_then(|h| h.size.map(|size| size.saturating_add(h.data_start as u64)));
        let whole = end.is_some_and(|end| end <= data.len() as u64);
        if let (Some(end), false, false) = (end, whole, short)
            && len == CUES_FIRST
            && end <= MAX_CUES
        {
            self.seek.read = CuesRead::Want { at, len: end as u32 };
            return;
        }
        if let Some(end) = end.filter(|e| *e > MAX_CUES) {
            log!(&*ctx, Level::Warn, "cues_too_large", at = at, bytes = end, max = MAX_CUES);
        }
        if std::mem::take(&mut self.seek.hop) {
            let body =
                header.and_then(|h| data.get(h.data_start..end?.min(data.len() as u64) as usize));
            let found = body.and_then(|b| seek_head_pos(b, crate::ebml::id::CUES));
            log!(&*ctx, Level::Debug, "seek_head_hop", at = at, cues = found.is_some());
            match found {
                Some(pos) => {
                    let at = self.seek.segment_data_start.saturating_add(pos);
                    self.seek.read = CuesRead::Want { at, len: CUES_FIRST };
                }
                None => {
                    log!(&*ctx, Level::Warn, "cues_absent", at = at);
                    self.publish(ctx)
                }
            }
            return;
        }
        let cues = parse_cue_tracks(data, self.seek.segment_data_start, self.seek.tick);
        log!(
            &*ctx,
            Level::Debug,
            "cues",
            at = at,
            bytes = data.len(),
            positions = cues.len(),
            whole = whole
        );
        if cues.is_empty() {
            log!(&*ctx, Level::Warn, "cues_empty", at = at, bytes = data.len());
            return self.publish(ctx);
        }
        log!(&*ctx, Level::Info, "cues", positions = cues.len(), whole = whole);
        self.seek.reserve = Some(at);
        (self.seek.cues, self.seek.whole) = (Held::Own(cues), whole);
        self.reader.mark(None);
        self.publish(ctx);
    }

    fn reserve(ctx: &Ctx, cues: &[Cue], cues_at: u64, file_len: Option<u64>) {
        let demand = cues_demand(cues, cues_at, file_len.unwrap_or(cues_at));
        let fits = (RESERVE_FLOOR as u64..=SIZED_TOP as u64).contains(&demand.largest);
        if demand.known != Known::Clusters || !fits {
            return;
        }
        let stats = ctx.pool_stats();
        let count = (stats.bytes_budget / demand.largest).clamp(1, stats.max_slots.max(1));
        if ctx.reserve_sized(demand.largest as usize, count.min(u32::MAX as u64) as u32).is_err() {
            log!(ctx, Level::Debug, "reserve_refused", bytes = demand.largest, count = count);
        }
    }

    fn reserve_when_held(&mut self, ctx: &Ctx) {
        if let Some(at) = self.seek.reserve.take() {
            Self::reserve(ctx, &self.seek.cues, at, self.seek.file_len);
        }
    }

    fn grow_pushed(&mut self) {
        let key = self.seek.grown.key;
        for (pts_ns, byte) in self.reader.marks() {
            self.seek.grown.cluster(byte);
            if let Some(key) = key {
                self.seek.due |= self.seek.grown.block(key, pts_ns, true);
            }
        }
    }

    pub fn tracks(&self) -> Vec<&Track> {
        self.pad_tracks.iter().map(|pt| &pt.track).collect()
    }

    pub fn pad_for(&self, track_number: u64) -> Option<SrcPad> {
        self.pad_tracks.iter().find(|pt| pt.track_number == track_number).map(|pt| pt.pad)
    }

    // Cold path, once per track.
    #[allow(clippy::disallowed_methods)]
    fn head_and_reframer(track: &Track) -> (Vec<u8>, Reframer) {
        if track.coding.refused.is_some() {
            return (Vec::new(), Reframer::Passthrough);
        }
        match track.codec_id.as_str() {
            CODEC_A_FLAC => (track.codec_private.clone(), Reframer::Passthrough),
            "A_AAC" => (track.codec_private.clone(), Reframer::Passthrough),
            // `opusdec` needs the `OpusHead` for the pre-skip.
            "A_OPUS" => (track.codec_private.clone(), Reframer::Passthrough),
            "A_VORBIS" | "A_ALAC" => (track.codec_private.clone(), Reframer::Passthrough),
            // The `.idx` text and DVB page ids (Matroska codec spec sections 3.5.13 and 6.7.1).
            "S_VOBSUB" | "S_DVBSUB" => (track.codec_private.clone(), Reframer::Passthrough),
            "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/AP" | "V_MPEG2" => {
                (track.codec_private.clone(), Reframer::Passthrough)
            }
            "V_MS/VFW/FOURCC" => {
                (codec::vfw_extradata(&track.codec_private).to_vec(), Reframer::Passthrough)
            }
            "A_PCM/INT/LIT" | "A_PCM/INT/BIG" | "A_PCM/FLOAT/IEEE" => {
                (Vec::new(), codec::pcm_reframer(track))
            }
            "V_MPEG4/ISO/AVC" | "V_MPEGH/ISO/HEVC" => {
                let is_hevc = track.codec_id == "V_MPEGH/ISO/HEVC";
                match codec::nal_head_from_config(&track.codec_private, is_hevc) {
                    Ok((head, length_size)) => (head, Reframer::Nal { length_size }),
                    // Untrusted CodecPrivate, a broken record falls back to a bare pad.
                    Err(_) => (Vec::new(), Reframer::Passthrough),
                }
            }
            _ => (Vec::new(), Reframer::Passthrough),
        }
    }

    /// One right-sized buffer within the pool's byte budget. `off` unchanged means the budget is
    /// spent.
    fn emit_bounded(ctx: &mut Ctx, stamp: Stamp, bytes: &[u8], off: usize, side: &[u8]) -> usize {
        let rest = &bytes[off..];
        if rest.is_empty() {
            return bytes.len();
        }
        let mut buf = match ctx.try_alloc_sized(rest.len()) {
            Ok(Some(buf)) => buf,
            Ok(None) if grow_pool(ctx).is_some() => match ctx.try_alloc_sized(rest.len()) {
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
        buf.pts = Timestamp::from_nanos(stamp.pts_ns);
        if let Some(d) = stamp.duration_ns {
            buf.duration = Timestamp::from_nanos(d);
        }
        buf.flags = stamp.flags;
        attach_side(&mut buf.memory, side);
        ctx.out(stamp.pad).push(buf);
        bytes.len()
    }

    fn emit_exact(ctx: &mut Ctx, stamp: Stamp, bytes: &[u8], side: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(mut buf) = ctx.alloc_exact(bytes.len()) else {
            let (element, error) = (ctx.element(), ctx.exact_refused(bytes.len()));
            ctx.post(BusMessage::Warning { element, error });
            return;
        };
        buf.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        buf.memory.set_len(bytes.len());
        buf.pts = Timestamp::from_nanos(stamp.pts_ns);
        if let Some(d) = stamp.duration_ns {
            buf.duration = Timestamp::from_nanos(d);
        }
        buf.flags = stamp.flags;
        attach_side(&mut buf.memory, side);
        ctx.out(stamp.pad).push(buf);
    }

    fn send(&mut self, ctx: &mut Ctx, stamp: Stamp, bytes: &[u8], how: Emit, side: &[u8]) {
        if how == Emit::Exact {
            return Self::emit_exact(ctx, stamp, bytes, side);
        }
        // Nothing overtakes a parked blob, or a pad's bytes would leave out of order.
        let off = match self.pending.is_empty() {
            true => Self::emit_bounded(ctx, stamp, bytes, 0, side),
            false => 0,
        };
        if off < bytes.len() {
            let mut bytes = own_remainder(&mut self.park_free, &bytes[off..]);
            bytes.extend_from_slice(side);
            self.pending.push_back(Carry { stamp, bytes, off: 0, view: None, side: side.len() });
        }
    }

    fn send_view(&mut self, ctx: &mut Ctx, stamp: Stamp, view: Memory, how: Emit) {
        if view.is_empty() {
            return;
        }
        if how == Emit::Exact || self.pending.is_empty() {
            return Self::emit_view(ctx, stamp, view);
        }
        let bytes = own_remainder(&mut self.park_free, &[]);
        self.pending.push_back(Carry { stamp, bytes, off: 0, view: Some(view), side: 0 });
    }

    fn emit_view(ctx: &mut Ctx, stamp: Stamp, memory: Memory) {
        let duration = stamp.duration_ns.map_or(Timestamp::NONE, Timestamp::from_nanos);
        let (pts, format) = (Timestamp::from_nanos(stamp.pts_ns), ctx.out_format());
        let buf = Buffer {
            memory,
            pts,
            dts: Timestamp::NONE,
            duration,
            flags: stamp.flags,
            format,
            sync: None,
        };
        ctx.out(stamp.pad).push(buf);
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> bool {
        while let Some(mut c) = self.pending.pop_front() {
            if let Some(view) = c.view.take() {
                Self::emit_view(ctx, c.stamp, view);
                recycle_park(&mut self.park_free, c.bytes);
                continue;
            }
            let (payload, side) = c.payload();
            let (off, len) =
                (Self::emit_bounded(ctx, c.stamp, payload, c.off, side), payload.len());
            c.off = off;
            if c.off < len {
                self.pending.push_front(c);
                return false;
            }
            recycle_park(&mut self.park_free, c.bytes);
        }
        true
    }

    fn pop_frame(&mut self, pulled: bool) -> Option<Out> {
        if let Some(out) = self.front.pop_front() {
            return Some(out);
        }
        if !pulled {
            return self.reader.next_frame().map(Out::owned);
        }
        if let Some(c) = self.cued.as_mut() {
            loop {
                let f = c.pop()?;
                let Some(view) = f.mem else {
                    if c.untrusted() {
                        return None;
                    }
                    continue;
                };
                let coded = self.seek.tracks.iter().find(|t| t.track_number == f.track);
                let bytes = match coded.map(|t| &t.coding).filter(|c| c.transforms()) {
                    None => Bytes::View(view, true),
                    Some(coding) => {
                        match self.reader.decode_frame(f.track, &coding.frames, view.data()) {
                            Some(data) => Bytes::Owned(data),
                            None => continue,
                        }
                    }
                };
                let (track_number, pts_ns, duration_ns) = (f.track, f.pts_ns, f.duration_ns);
                return Some(Out {
                    track_number,
                    pts_ns,
                    duration_ns,
                    keyframe: true,
                    bytes,
                    t35: Bytes::Owned(empty()),
                    discard_ns: 0,
                });
            }
        }
        loop {
            let p = self.pull.plan.pop_ready()?;
            let State::Ready(view, whole) = p.state else {
                continue;
            };
            let t35 = p.t35.map_or(Bytes::Owned(empty()), |v| Bytes::View(v, false));
            let (track_number, pts_ns, duration_ns, keyframe, discard_ns) =
                (p.track, p.pts_ns, p.duration_ns, p.keyframe, p.discard_ns);
            let coded = self.seek.tracks.iter().find(|t| t.track_number == track_number);
            let bytes = match coded.map(|t| &t.coding).filter(|c| c.transforms()) {
                None => Bytes::View(view, whole),
                Some(coding) => {
                    match self.reader.decode_frame(track_number, &coding.frames, view.data()) {
                        Some(data) => Bytes::Owned(data),
                        None => continue,
                    }
                }
            };
            return Some(Out {
                track_number,
                pts_ns,
                duration_ns,
                keyframe,
                bytes,
                t35,
                discard_ns,
            });
        }
    }

    fn recycle(reader: &mut MatroskaReader, out: Out) {
        if let Bytes::Owned(data) = out.bytes {
            reader.recycle(data);
        }
        if let Bytes::Owned(data) = out.t35 {
            reader.recycle(data);
        }
    }

    fn tick(&self, pulled: bool) -> u64 {
        if pulled { self.pull.walk.scale } else { self.reader.timestamp_scale() }
    }

    fn next_frame(&mut self, pulled: bool) -> Option<(usize, Out)> {
        let tick = self.tick(pulled);
        while let Some(frame) = self.pop_frame(pulled) {
            let number = frame.track_number;
            let of = |pt: &PadTrack| pt.linked && pt.track_number == number;
            let Some(idx) = self.pad_tracks.iter().position(of) else {
                Self::recycle(&mut self.reader, frame);
                continue;
            };
            let pt = &mut self.pad_tracks[idx];
            let held = (pt.held.first().map(|h| h.pts_ns), pt.held.len());
            match pass(&mut pt.gate, pt.kind, (frame.pts_ns, frame.keyframe), held, tick) {
                Pass::Out => return Some((idx, frame)),
                Pass::Drop => Self::recycle(&mut self.reader, frame),
                Pass::Hold => pt.held.push(frame.owned_in(&mut self.reader)),
                Pass::Replace => {
                    pt.held.drain(..).for_each(|old| Self::recycle(&mut self.reader, old));
                    pt.held.push(frame.owned_in(&mut self.reader));
                }
                Pass::Release => {
                    self.front.push_front(frame);
                    Self::release(pt, &mut self.front);
                }
            }
        }
        None
    }

    fn release(pt: &mut PadTrack, front: &mut std::collections::VecDeque<Out>) {
        pt.gate = Gate::Open;
        while let Some(held) = pt.held.pop() {
            front.push_front(held);
        }
    }

    /// A Block that fails to reframe is dropped, since input is untrusted.
    fn emit_frame(&mut self, ctx: &mut Ctx, idx: usize, frame: Out, how: Emit) {
        let Out { track_number, pts_ns, duration_ns, keyframe, mut bytes, t35, discard_ns } = frame;
        let (reframer, opus) = (self.pad_tracks[idx].reframer, self.pad_tracks[idx].opus);
        let mut side = std::mem::take(&mut self.side_buf);
        let side_len = side_records(&t35, &mut side);
        if side_len > 0
            && let Bytes::View(_, own) = &mut bytes
        {
            *own = false;
        }
        let as_is = match &mut bytes {
            Bytes::View(view, true) => {
                let n = view.len();
                reframer.reframe_in_place(&mut view.as_mut_full()[..n])
            }
            Bytes::View(_, false) | Bytes::Owned(_) => false,
        };
        let mut reframe_buf = std::mem::take(&mut self.reframe_buf);
        let data = match &bytes {
            Bytes::Owned(data) => &data[..],
            Bytes::View(view, _) => view.data(),
        };
        // An Opus packet times itself, less the Block's DiscardPadding, so the decoder cuts the
        // padding off the end.
        let duration_ns = match opus {
            true => codec::opus_ns(data).map(|ns| ns.saturating_sub(discard_ns)).or(duration_ns),
            false => duration_ns,
        };
        let reframed =
            if as_is { Ok(&data[..0]) } else { reframer.reframe_into(data, &mut reframe_buf) };
        match reframed {
            Ok(reframed) => {
                self.start_pad(ctx, idx, pts_ns, how);
                let pt = &mut self.pad_tracks[idx];
                // An unstamped laced frame counts with the stamped one before it.
                pt.last = match pt.last {
                    Some((pts, n)) if pts == pts_ns || pts_ns == PTS_NONE => {
                        Some((pts, n.saturating_add(1)))
                    }
                    last if pts_ns == PTS_NONE => last,
                    _ => Some((pts_ns, 1)),
                };
                // An untagged buffer reads as unknown, which a muxer takes for a keyframe.
                let flags = if keyframe { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
                let pad = self.pad_tracks[idx].pad;
                let stamp = Stamp { pad, pts_ns, duration_ns, flags };
                if !as_is {
                    self.send(ctx, stamp, reframed, how, &side[..side_len]);
                } else if let Bytes::View(view, _) =
                    std::mem::replace(&mut bytes, Bytes::Owned(empty()))
                {
                    self.send_view(ctx, stamp, view, how);
                }
                self.say_gaps(ctx, idx, pts_ns);
            }
            Err(cause) if how == Emit::Bounded => {
                let element = ctx.element();
                let error = DemuxError::FrameDropped { track: track_number, cause };
                ctx.post(BusMessage::Warning { element, error: demux_failed(ctx, error) });
            }
            Err(_) => self.lost += 1,
        }
        self.reframe_buf = reframe_buf;
        self.side_buf = side;
        if let Bytes::Owned(data) = bytes {
            self.reader.recycle(data);
        }
        if let Bytes::Owned(data) = t35 {
            self.reader.recycle(data);
        }
    }

    fn say_gaps(&mut self, ctx: &mut Ctx, idx: usize, pts_ns: u64) {
        if pts_ns == PTS_NONE {
            return;
        }
        for (i, pt) in self.pad_tracks.iter_mut().enumerate() {
            let sparse = i != idx && pt.linked && pt.kind == Kind::Other;
            if sparse && pt.gap.is_none_or(|said| pts_ns >= said.saturating_add(GAP_EVERY_NS)) {
                pt.gap = Some(pts_ns);
                ctx.push_event(pt.pad, Event::Gap { until: Timestamp::from_nanos(pts_ns) });
            }
        }
    }

    fn start_pad(&mut self, ctx: &mut Ctx, idx: usize, pts_ns: u64, how: Emit) {
        let pt = &mut self.pad_tracks[idx];
        if pt.started {
            return;
        }
        pt.started = true;
        let (pad, family) = (pt.pad, pt.family);
        let head = std::mem::take(&mut pt.codec_head);
        Self::announce(ctx, pad, family, &self.pad_tracks[idx].track);
        let stamp = Stamp { pad, pts_ns, duration_ns: None, flags: BufferFlags::HEADER };
        self.send(ctx, stamp, &head, how, &[]);
        self.pad_tracks[idx].codec_head = head;
    }

    fn greet(&mut self, ctx: &mut Ctx, pad: SrcPad) {
        let Some(idx) = self.pad_tracks.iter().position(|pt| pt.linked && pt.pad == pad) else {
            return;
        };
        let head = std::mem::take(&mut self.pad_tracks[idx].codec_head);
        let pts_ns = self.pad_tracks[idx].last.map_or(0, |(pts, _)| pts);
        let stamp = Stamp { pad, pts_ns, duration_ns: None, flags: BufferFlags::HEADER };
        self.send(ctx, stamp, &head, Emit::Exact, &[]);
        self.pad_tracks[idx].codec_head = head;
    }

    fn drain(&mut self, ctx: &mut Ctx) -> bool {
        self.warn_undecoded(ctx);
        if !self.drain_pending(ctx) {
            return false;
        }
        let pulled = ctx.pulls(Self::IN);
        while let Some((idx, frame)) = self.next_frame(pulled) {
            let (track, pts) = (frame.track_number, frame.pts_ns);
            self.emit_frame(ctx, idx, frame, Emit::Bounded);
            if !self.pending.is_empty() {
                self.warn_undecoded(ctx);
                return false;
            }
            if Some(track) == self.key_track && pts != PTS_NONE {
                self.key_out(ctx, pulled, pts);
            }
        }
        self.warn_undecoded(ctx);
        true
    }

    fn key_out(&mut self, ctx: &mut Ctx, pulled: bool, pts: u64) {
        if !pulled {
            let front: usize = self
                .front
                .iter()
                .map(|o| match &o.bytes {
                    Bytes::Owned(data) => data.len(),
                    Bytes::View(view, _) => view.len(),
                })
                .sum();
            let out = self.taken.saturating_sub((self.reader.held() + front) as u64);
            self.measured.note(out, Timestamp::from_nanos(pts));
        }
        let (index, measured, walk) = (&self.rated, self.measured, &mut self.pull.walk);
        self.teller.tell_at(ctx, Self::IN, Timestamp::from_nanos(pts), |at| {
            let rate =
                index.as_ref().and_then(|i| rate::of_index(i, at)).or_else(|| measured.rate());
            if let Some(b) = rate {
                walk.pace(b.bits_per_second);
            }
            rate
        });
    }

    fn flush_exact(&mut self, ctx: &mut Ctx) {
        while let Some(c) = self.pending.pop_front() {
            match c.view {
                Some(view) => Self::emit_view(ctx, c.stamp, view),
                None => {
                    let (payload, side) = c.payload();
                    Self::emit_exact(ctx, c.stamp, &payload[c.off..], side)
                }
            }
            recycle_park(&mut self.park_free, c.bytes);
        }
        let pulled = ctx.pulls(Self::IN);
        while let Some((idx, frame)) = self.next_frame(pulled) {
            self.emit_frame(ctx, idx, frame, Emit::Exact);
        }
        // A track that ended at or before its line ends with its last block held.
        for pt in &mut self.pad_tracks {
            Self::release(pt, &mut self.front);
        }
        while let Some((idx, frame)) = self.next_frame(pulled) {
            self.emit_frame(ctx, idx, frame, Emit::Exact);
        }
    }

    fn drop_staged(&mut self) {
        while let Some(c) = self.pending.pop_front() {
            recycle_park(&mut self.park_free, c.bytes);
        }
        for pt in &mut self.pad_tracks {
            pt.held.drain(..).for_each(|frame| Self::recycle(&mut self.reader, frame));
        }
        self.resync();
    }

    fn resync(&mut self) {
        self.reader.resync_streaming();
        while let Some(frame) = self.front.pop_front() {
            Self::recycle(&mut self.reader, frame);
        }
    }

    /// Video waits for a keyframe. Audio passes from the point less pre-roll and a tick (RFC 9559
    /// section 11.1).
    fn gate_tracks(&mut self, ctx: &Ctx) {
        let point = ctx.presentation_point().and_then(|point| point.nanos());
        let tick = self.tick(ctx.pulls(Self::IN));
        for pt in &mut self.pad_tracks {
            (pt.started, pt.last, pt.gap) = (false, None, None);
            pt.gate = match (pt.kind, point) {
                (Kind::Video, _) => Gate::Keyframe { from: 0 },
                (Kind::Audio, Some(point)) => {
                    Gate::Line { line: point.saturating_sub(pt.pre_roll_ns).saturating_sub(tick) }
                }
                _ => Gate::Open,
            };
        }
        self.video_from = ctx.seek_target().and_then(|target| target.at.nanos());
    }

    /// A relinked track drops what it reads again up to its last frame out, so nothing is seen
    /// twice.
    fn relink(&mut self, ctx: &Ctx, pad: SrcPad, on: bool, restart: Option<SeekTarget>) {
        let Some(idx) =
            self.pad_tracks.iter().position(|pt| ctx.stream_pad(pt.stream) == Some(pad))
        else {
            return;
        };
        self.leave_cued();
        let tick = self.tick(ctx.pulls(Self::IN));
        if restart.is_some() && on {
            self.resync();
            self.reader.restart_at(restart.map(|target| target.to_byte));
            for pt in self.pad_tracks.iter_mut().filter(|pt| pt.linked) {
                pt.held.drain(..).for_each(|frame| Self::recycle(&mut self.reader, frame));
                if let Some((pts, left)) = pt.last {
                    pt.gate = Gate::Resume { pts, left, inside: false };
                }
            }
        }
        let pt = &mut self.pad_tracks[idx];
        pt.held.drain(..).for_each(|frame| Self::recycle(&mut self.reader, frame));
        (pt.pad, pt.linked, pt.started, pt.last, pt.gap) = (pad, on, false, None, None);
        let from = restart.and_then(|target| target.at.nanos());
        pt.gate = match (pt.kind, from) {
            (Kind::Video, _) => Gate::Keyframe { from: 0 },
            (Kind::Audio, Some(point)) => {
                Gate::Line { line: point.saturating_sub(pt.pre_roll_ns).saturating_sub(tick) }
            }
            _ => Gate::Open,
        };
    }

    /// `stamp` is the stream time of the byte the source restarted at, not a media timestamp.
    fn hold_video(&mut self, from: u64, stamp: Timestamp) {
        if !stamp.nanos().is_some_and(|read_from| read_from < from) {
            return;
        }
        for pt in self.pad_tracks.iter_mut().filter(|pt| pt.kind == Kind::Video) {
            pt.gate = Gate::Keyframe { from };
        }
    }

    fn announce(ctx: &mut Ctx, pad: SrcPad, family: &'static str, track: &Track) {
        log!(
            &*ctx,
            Level::Debug,
            "announce",
            family = family,
            width = track.pixel_width,
            height = track.pixel_height,
        );
        // `CodecDelay` rides the format for the decoder to take off the stamps (RFC 9559 sections
        // 5.1.4.1.25 and 11.2).
        let delay = track.codec_delay_ns.min(i64::MAX as u64) as i64;
        let delayed = delay > 0 && codec::is_audio_family(family);
        let delay = delayed.then_some((codec::FIELD_DELAY, ValueDesc::Int(delay)));
        if family == FAMILY_FLAC {
            let mut fields = vec![
                ("rate", ValueDesc::Int(track.sampling_frequency as i64)),
                ("channels", ValueDesc::Int(track.channels as i64)),
                ("sample", ValueDesc::Id(sample_name(track.bit_depth))),
            ];
            fields.extend(delay);
            ctx.announce_format(pad, FAMILY_FLAC, &fields);
        } else if family == "audio/raw" {
            let mut fields = vec![
                (profluens_audio::FIELD_RATE, ValueDesc::Int(track.sampling_frequency as i64)),
                (profluens_audio::FIELD_CHANNELS, ValueDesc::Int(i64::from(track.channels.max(1)))),
                (
                    profluens_audio::FIELD_SAMPLE,
                    ValueDesc::Id(codec::pcm_sample(track).unwrap_or("s16")),
                ),
            ];
            fields.extend(delay);
            ctx.announce_format(pad, family, &fields);
        } else if family == "aac" && track.sampling_frequency > 0.0 {
            let mut fields = vec![
                ("rate", ValueDesc::Int(track.sampling_frequency as i64)),
                ("channels", ValueDesc::Int(track.channels as i64)),
            ];
            fields.extend(delay);
            ctx.announce_format(pad, "aac", &fields);
        } else if let Some(delay) = delay {
            ctx.announce_format(pad, family, &[delay]);
        } else if codec::is_video_family(family)
            && track.pixel_width != 0
            && track.pixel_height != 0
        {
            // Colour values are H.273 code points (RFC 9559 section 5.1.4.1.31), announced only
            // when declared.
            let mut fields: Vec<(&str, ValueDesc)> = vec![
                ("width", ValueDesc::Int(track.pixel_width as i64)),
                ("height", ValueDesc::Int(track.pixel_height as i64)),
            ];
            let colour = crate::color_map::colour_of(track);
            let mut stated = [("", ValueDesc::Int(0)); profluens_video::color::FIELDS.len()];
            let n = colour.write_fields(&mut stated);
            fields.extend_from_slice(&stated[..n]);
            ctx.announce_format(pad, family, &fields);
        } else {
            ctx.announce_format(pad, family, &[]);
        }
    }
}

impl MkvDemux {
    // Cold path, once at preroll.
    #[allow(clippy::disallowed_methods)]
    fn index_head(&mut self, probe: &MatroskaReader) {
        let seek = &mut self.seek;
        seek.tracks = probe.tracks().to_vec();
        seek.tick = probe.timestamp_scale();
        seek.duration = probe.duration_ns().map_or(Timestamp::NONE, Timestamp);
        // A restart before the walk passes Info still stamps by the file's scale.
        (self.pull.walk.scale, self.pull.walk.duration_ns) = (seek.tick, probe.duration_ns());
        seek.file_len = crate::index::segment_end(&self.header);
        self.pull.walk.end = seek.file_len;
        let info = parse_seek_head(&self.header);
        seek.segment_data_start = info.map_or(0, |info| info.segment_data_start);
        let at = |pos: Option<u64>| {
            info.and_then(|i| pos.map(|p| i.segment_data_start.saturating_add(p)))
        };
        let (cues_at, head_at) =
            (at(info.and_then(|i| i.cues_pos)), at(info.and_then(|i| i.next_head)));
        // A front SeekHead may name only another SeekHead, the one hop followed for Cues.
        seek.hop = cues_at.is_none() && head_at.is_some();
        seek.read = match (seek.cues.is_empty(), cues_at.or(head_at)) {
            (true, Some(at)) => CuesRead::Want { at, len: CUES_FIRST },
            _ => CuesRead::Done,
        };
        let first =
            |kind| self.pad_tracks.iter().find(|pt| pt.kind == kind).map(|pt| pt.track_number);
        seek.grown.key = first(Kind::Video).or_else(|| first(Kind::Audio));
    }

    /// A restart byte other than the file or Segment start may land inside an element, so the
    /// walk scans for a Cluster there. Coalesced resets leave no other clue.
    fn mid_segment(&self, byte: u64) -> bool {
        byte != 0 && byte != self.seek.segment_data_start
    }

    fn restart_pull(&mut self, from: SeekTarget) {
        if let CuesRead::Asked { at, len, .. } = self.seek.read {
            self.seek.read = CuesRead::Want { at, len };
        }
        self.side.read.iter_mut().for_each(asked_again);
        if let Some(c) = self.cued.as_mut() {
            c.restart(self.cued_from);
        }
        self.walk_from = from.to_byte;
        let resync = self.mid_segment(from.to_byte);
        let lead = self.lead(from.at);
        let pull = &mut self.pull;
        pull.plan.reset();
        (pull.win, pull.stash) = (None, None);
        pull.walk.restart(from.to_byte, resync);
        pull.walk.lead = lead;
        if let Some(video_from) = self.video_from.take() {
            self.hold_video(video_from, from.at);
        }
    }

    /// The lead of a seek to `at`, none without a linked video track.
    fn lead(&self, at: Timestamp) -> u32 {
        if !self.pad_tracks.iter().any(|pt| pt.linked && pt.kind == Kind::Video) {
            return 0;
        }
        if self.key_bytes > 0 {
            return self
                .key_bytes
                .saturating_add(self.key_bytes / 4)
                .saturating_add(LEAD_SLACK)
                .min(LEAD_MAX);
        }
        let bits = self
            .rated
            .as_ref()
            .and_then(|i| rate::of_index(i, at))
            .map_or(0, |b| b.bits_per_second);
        let bytes = u128::from(bits) * u128::from(LEAD_NS) / 8_000_000_000;
        (bytes.min(u128::from(LEAD_MAX)) as u32).max(LEAD_MIN)
    }

    fn pull(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        while let Some((tag, buf)) = ctx.next_served(Self::IN, inputs) {
            if matches!(self.seek.read, CuesRead::Asked { tag: t, .. } if t == tag) {
                self.cues_in(ctx, buf.memory.data());
                continue;
            }
            if self.side_in(ctx, tag, buf.memory.data()) {
                continue;
            }
            if let Some(c) = self.cued.as_mut().filter(|c| c.owns(tag)) {
                c.served(tag, buf.memory);
                continue;
            }
            if let Some((len, base)) = self.pull.plan.window_in(tag) {
                let eof = buf.memory.len() < len as usize;
                self.pull.win = Some(Window { base, mem: buf.memory, eof });
                self.pull.walk.entered();
                continue;
            }
            if let (Err((offset, want, got)), false) =
                (self.pull.plan.served(tag, buf.memory), self.pull.plan.warned)
            {
                self.pull.plan.warned = true;
                let element = ctx.element();
                let short = profluens_core::pull::RangeError::Short { offset, want, got };
                ctx.post(BusMessage::Warning {
                    element,
                    error: Error::element_failed(element, short),
                });
            }
        }
        self.ask_cues(ctx);
        self.ask_side(ctx);
        match self.read_mode(ctx) {
            Read::Wait => return Ok(Flow::Ok),
            Read::Cued => {
                let flow = self.pull_cued(ctx);
                if self.cued.is_some() {
                    return Ok(flow);
                }
            }
            Read::Walk => {}
        }
        if !self.drain(ctx) {
            self.reserve_when_held(ctx);
            ctx.hold_end();
            return Ok(Flow::Ok);
        }
        self.walk_on(ctx)?;
        if self.seek.due {
            self.publish(ctx);
        }
        if self.pull.stash.is_some() || self.pull.plan.asked_all {
            self.reserve_when_held(ctx);
        }
        if !self.posted_duration
            && let Some(ns) = self.pull.walk.duration_ns
        {
            self.posted_duration = true;
            ctx.set_duration(ns);
        }
        Ok(Flow::Ok)
    }

    /// Sparse tracks alone are read by their Cues when those place every block. Anything else is
    /// walked.
    // Cold path, once a run and per link change.
    #[allow(clippy::disallowed_methods)]
    fn read_mode(&mut self, ctx: &Ctx) -> Read {
        if self.cued.is_some() {
            return Read::Cued;
        }
        if self.cued_off {
            return Read::Walk;
        }
        let linked: Vec<&PadTrack> = self.pad_tracks.iter().filter(|pt| pt.linked).collect();
        if linked.is_empty() || linked.iter().any(|pt| pt.kind != Kind::Other) {
            self.cued_off = true;
            return Read::Walk;
        }
        if self.seek.read != CuesRead::Done {
            return Read::Wait;
        }
        let numbers: Vec<u64> = linked.iter().map(|pt| pt.track_number).collect();
        match Cued::refs(&self.seek.cues, &numbers).filter(|r| !r.is_empty() && self.seek.whole) {
            Some(refs) => {
                log!(ctx, Level::Debug, "cued", blocks = refs.len() as u64, from = self.cued_from);
                self.cued = Some(Cued::new(refs, self.seek.tick, self.cued_from));
                Read::Cued
            }
            None => {
                self.cued_off = true;
                Read::Walk
            }
        }
    }

    fn pull_cued(&mut self, ctx: &mut Ctx) -> Flow {
        self.ask_cued(ctx);
        if !self.drain(ctx) {
            ctx.hold_end();
            return Flow::Ok;
        }
        self.ask_cued(ctx);
        let Some(c) = self.cued.as_mut() else {
            return Flow::Ok;
        };
        if c.untrusted() {
            log!(&*ctx, Level::Warn, "cued_untrusted", from = self.walk_from);
            (self.cued, self.cued_off) = (None, true);
            self.pull.plan.reset();
            (self.pull.win, self.pull.stash) = (None, None);
            self.pull.walk.restart(self.walk_from, self.mid_segment(self.walk_from));
            return Flow::Ok;
        }
        if c.dropped > 0 {
            log!(&*ctx, Level::Warn, "cued_dropped", blocks = u64::from(c.dropped));
            c.dropped = 0;
        }
        if c.done() && !self.pull.plan.asked_all {
            self.pull.plan.asked_all = true;
            ctx.end_asks(Self::IN);
        }
        if !self.posted_duration
            && let Some(ns) = self.seek.duration.nanos()
        {
            self.posted_duration = true;
            ctx.set_duration(ns);
        }
        Flow::Ok
    }

    fn ask_cued(&mut self, ctx: &mut Ctx) {
        let Some(c) = self.cued.as_mut() else { return };
        let most = (self.pull.caps.1 / 2).max(1) as usize;
        while ctx.asks_left(Self::IN) > 0 && c.held() < most {
            let Some((offset, len)) = c.want() else { break };
            let Ok(Some(buf)) = ctx.try_alloc_sized(len as usize) else {
                c.unasked();
                break;
            };
            let tag = self.pull.plan.next_tag;
            if ctx.ask(Self::IN, Ask { offset, len, tag, buf }).is_err() {
                c.unasked();
                break;
            }
            self.pull.plan.next_tag += 1;
            c.asked(tag);
        }
    }

    fn leave_cued(&mut self) {
        self.cued_off = false;
        let Some(c) = self.cued.take() else { return };
        if let Some((byte, at)) = c.resume_byte().zip(c.resume_time()) {
            self.cued_from = at;
            self.pull.plan.reset();
            (self.pull.win, self.pull.stash) = (None, None);
            self.pull.walk.restart(byte, false);
        }
    }

    /// One Warning per drive names the last damage walked past, as the push path does. A run of
    /// junk is a spoilt element every few octets, a message each would flood the bus.
    fn walk_on(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let walked = self.walk_steps(ctx);
        if let Some(cause) = self.pull.walk.damage.take() {
            damaged(ctx, self.pull.walk.damage_at, cause);
        }
        walked
    }

    fn walk_steps(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let (bytes, ranges) = self.pull.caps;
        loop {
            let pull = &mut self.pull;
            let step = match pull.stash.take() {
                Some(step) => step,
                None => pull.walk.next(pull.win.as_ref()),
            };
            let seek = &mut self.seek;
            let grow = seek.cues.is_empty();
            if let Some(byte) = pull.walk.cluster_at.take().filter(|_| grow) {
                seek.grown.cluster(byte);
            }
            match step {
                Step::Skip { to } => pull.walk.pos = to,
                Step::Block { from, to, next, keyframe, duration_ns, additions, discard_ns } => {
                    let (Some(win), Some((tag, _))) = (pull.win.as_ref(), pull.plan.walking) else {
                        return Ok(());
                    };
                    let items = &mut pull.plan.items;
                    let mut put = |mut p: Planned| {
                        let stamped = p.pts_ns != PTS_NONE;
                        seek.due |=
                            grow && stamped && seek.grown.block(p.track, p.pts_ns, p.keyframe);
                        p.tag = tag;
                        items.push_back(p);
                    };
                    let tracks = &seek.tracks;
                    let decoded = pull.walk.frames(
                        win,
                        (from, to),
                        keyframe,
                        (duration_ns, discard_ns),
                        additions,
                        tracks,
                        &mut pull.lace,
                        &mut put,
                    );
                    if let Err(cause) = decoded {
                        (pull.walk.damage, pull.walk.damage_at) = (Some(cause), pull.walk.pos);
                    }
                    pull.walk.pos = next;
                }
                Step::End => {
                    if !pull.plan.asked_all {
                        pull.plan.asked_all = true;
                        ctx.end_asks(Self::IN);
                    }
                    return Ok(());
                }
                Step::Window { .. } if pull.plan.window.is_some() => return Ok(()),
                Step::Frame { track, pts_ns, keyframe, to, .. }
                    if !self.pad_tracks.iter().any(|pt| pt.linked && pt.track_number == track) =>
                {
                    // a frame no pad takes is walked past unread, its stamp still grows the index
                    pull.win = None;
                    pull.plan.leave_window();
                    seek.due |= grow && seek.grown.block(track, pts_ns, keyframe);
                    pull.walk.pos = to;
                }
                Step::Frame { offset, len, .. } | Step::Window { offset, len, .. } => {
                    pull.win = None;
                    pull.plan.leave_window();
                    let room = pull.plan.ranges < ranges
                        && (pull.plan.held == 0 || pull.plan.held + len as u64 <= bytes);
                    let buf = match room.then(|| ctx.try_alloc_sized(len as usize)) {
                        Some(Ok(Some(buf))) => buf,
                        None => {
                            pull.stash = Some(step);
                            return Ok(());
                        }
                        Some(Ok(None)) => {
                            pull.stash = Some(step);
                            if grow_pool(ctx).is_none() {
                                return Ok(());
                            }
                            continue;
                        }
                        Some(Err(e)) => {
                            let element = ctx.element();
                            ctx.post(BusMessage::Warning {
                                element,
                                error: Error::element_failed(element, e),
                            });
                            match step {
                                Step::Frame { to, .. } | Step::Window { whole: Some(to), .. } => {
                                    pull.walk.pos = to
                                }
                                _ => return Ok(()),
                            }
                            continue;
                        }
                    };
                    let tag = pull.plan.next_tag;
                    if ctx.ask(Self::IN, Ask { offset, len, tag, buf }).is_err() {
                        pull.stash = Some(step);
                        return Ok(());
                    }
                    pull.plan.asked(len);
                    let Step::Frame { track, pts_ns, keyframe, to, .. } = step else {
                        pull.plan.window = Some((tag, len, offset));
                        return Ok(());
                    };
                    let video = |pt: &PadTrack| {
                        pt.linked && pt.track_number == track && pt.kind == Kind::Video
                    };
                    if keyframe && self.pad_tracks.iter().any(video) {
                        self.key_bytes = len.max(self.key_bytes - self.key_bytes / 8);
                    }
                    let state = State::Waiting { offset, want: len };
                    let duration_ns = seek
                        .tracks
                        .iter()
                        .find(|t| t.track_number == track)
                        .and_then(|t| t.default_duration_ns);
                    seek.due |= grow && seek.grown.block(track, pts_ns, keyframe);
                    pull.plan.items.push_back(Planned {
                        track,
                        pts_ns,
                        duration_ns,
                        keyframe,
                        tag,
                        last_of: len,
                        state,
                        t35: None,
                        discard_ns: 0,
                    });
                    pull.walk.pos = to;
                }
            }
        }
    }
}

fn asked_again(read: &mut CuesRead) {
    if let CuesRead::Asked { at, len, .. } = *read {
        *read = CuesRead::Want { at, len };
    }
}

fn mux_failed(ctx: &Ctx, e: MuxError) -> Error {
    Error::element_failed(ctx.element(), MkvError::Mux(e))
}

fn demux_failed(ctx: &Ctx, e: DemuxError) -> Error {
    Error::element_failed(ctx.element(), MkvError::Demux(e))
}

fn damaged(ctx: &mut Ctx, at: u64, cause: crate::ebml::ReadError) {
    let element = ctx.element();
    let error = demux_failed(ctx, DemuxError::Damaged { at, cause });
    ctx.post(BusMessage::Warning { element, error });
}

fn log_track(ctx: &Ctx, track: &Track, family: &'static str, kind: Kind) {
    let number = track.track_number;
    match kind {
        _ if family == "bytes" => log!(ctx, Level::Warn, "track_unknown", track = number),
        Kind::Video => {
            let (w, h) = (track.pixel_width, track.pixel_height);
            log!(ctx, Level::Info, "track", track = number, family = family, width = w, height = h)
        }
        Kind::Audio => {
            let (r, c) = (track.sampling_frequency as u64, track.channels);
            log!(ctx, Level::Info, "track", track = number, family = family, rate = r, channels = c)
        }
        Kind::Other => log!(ctx, Level::Info, "track", track = number, family = family),
    }
}

fn sample_name(bit_depth: u32) -> &'static str {
    match bit_depth {
        8 => "s8",
        24 => "s24",
        32 => "s32",
        _ => "s16",
    }
}

impl Element for MkvDemux {
    fn desc(&self) -> &'static ElementDesc {
        &DEMUX_DESC
    }

    // Cold path, track discovery at preroll.
    #[allow(clippy::disallowed_methods)]
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let mut probe = MatroskaReader::new();
        probe.push(&self.header).map_err(|e| demux_failed(ctx, DemuxError::Header(e)))?;
        if !probe.tracks_ready() {
            return Err(demux_failed(ctx, DemuxError::TracksIncomplete));
        }
        for track in probe.tracks() {
            let family = codec::track_family(track);
            if let Some(why) = track.coding.refused {
                let (element, codec) = (ctx.element(), track.codec_id.as_str().into());
                let error = demux_failed(
                    ctx,
                    DemuxError::TrackRefused { track: track.track_number, codec, why },
                );
                ctx.post(BusMessage::Warning { element, error });
            }
            let (codec_head, reframer) = Self::head_and_reframer(track);
            let nal = matches!(track.codec_id.as_str(), "V_MPEG4/ISO/AVC" | "V_MPEGH/ISO/HEVC");
            if nal && track.coding.refused.is_none() && codec_head.is_empty() {
                // A broken record leaves the decoder to find parameter sets in band.
                let number = track.track_number;
                log!(&*ctx, Level::Warn, "codec_private_bad", track = number, family = family);
            }
            let name = format!("src_track{}", track.track_number);
            let kind = match family {
                f if codec::is_video_family(f) => Kind::Video,
                f if codec::is_audio_family(f) => Kind::Audio,
                _ => Kind::Other,
            };
            let stream = ctx.add_stream(Stream {
                kind: match kind {
                    Kind::Video => StreamKind::Video,
                    Kind::Audio => StreamKind::Audio,
                    Kind::Other if family.starts_with("subtitle/") => StreamKind::Text,
                    Kind::Other => StreamKind::Other,
                },
                codec: family,
                language: track.language.as_str().into(),
                title: track.name.as_str().into(),
                default: track.flag_default,
                forced: track.flag_forced,
                disposition: track.disposition,
                channels: track.channels.min(u32::from(u16::MAX)) as u16,
                duration: probe.duration_ns().map_or(Timestamp::NONE, Timestamp),
                sparse: matches!(kind, Kind::Other),
                name: name.into(),
                offers: codec::track_offers(track),
            });
            log_track(ctx, track, family, kind);
            self.pad_tracks.push(PadTrack {
                track_number: track.track_number,
                stream,
                pad: SrcPad(u32::MAX),
                linked: false,
                last: None,
                gap: None,
                family,
                codec_head,
                reframer,
                track: track.clone(),
                started: false,
                kind,
                opus: family == "opus",
                pre_roll_ns: codec::lead_of(track).pre_roll,
                gate: Gate::Open,
                held: Vec::new(),
            });
        }
        self.index_head(&probe);
        self.side_head();
        let cues = !self.seek.cues.is_empty() || self.seek.read != CuesRead::Done;
        if !cues {
            log!(&*ctx, Level::Warn, "cues_absent", at = self.seek.segment_data_start);
        }
        log!(
            &*ctx,
            Level::Info,
            "opened",
            tracks = self.pad_tracks.len(),
            duration = self.seek.duration,
            scale = self.seek.tick,
            cues = cues,
        );
        self.publish(ctx);
        if matches!(self.seek.read, CuesRead::Want { .. }) {
            ctx.will_index();
        }
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.reader = MatroskaReader::new();
        // A seek landing before the header was pushed still knows the tracks and scale.
        let duration = self.seek.duration.nanos();
        self.reader.seed(&self.seek.tracks, self.seek.tick, duration);
        self.reader.mark(self.seek.grown.key.filter(|_| self.seek.cues.is_empty()));
        if let CuesRead::Asked { at, len, .. } = self.seek.read {
            self.seek.read = CuesRead::Want { at, len };
        }
        self.side.read.iter_mut().for_each(asked_again);
        for pt in &mut self.pad_tracks {
            (pt.started, pt.gate, pt.last) = (false, Gate::Open, None);
            pt.held.clear();
            let pad = ctx.stream_pad(pt.stream);
            (pt.pad, pt.linked) =
                (pad.unwrap_or(SrcPad(u32::MAX)), pad.is_some_and(|pad| ctx.pad_linked(pad)));
        }
        let linked = || self.pad_tracks.iter().filter(|pt| pt.linked);
        let key = linked().find(|pt| pt.track.pixel_width > 0).or_else(|| linked().next());
        self.key_track = key.map(|pt| pt.track_number);
        self.scratch.clear();
        self.reframe_buf.clear();
        self.pending.clear();
        self.video_from = None;
        self.front.clear();
        ctx.want_ranges(Self::IN);
        let stats = ctx.pool_stats();
        let most = |n: u64| n - n / 4;
        let bytes = most(stats.bytes_budget).min(profluens_core::memory::IO_DEPTH_BYTES);
        self.pull.caps = (bytes.max(1), most(stats.max_slots).max(1) as u32);
        self.pull.walk.set_cap((stats.bytes_budget / 4).clamp(256, u32::MAX as u64) as u32);
        (self.cued, self.cued_off, self.cued_from, self.walk_from) = (None, false, 0, 0);
        self.lost = 0;
        if !self.seek.cues.is_empty() {
            self.publish(ctx);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.demux(ctx, &mut inputs)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        self.on_event(ctx, event)
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.on_stop(ctx);
    }
}

impl MkvDemux {
    fn demux(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        if let Some(from) = ctx.pull_reset(Self::IN) {
            let start = from.to_byte == 0 && from.at == Timestamp::ZERO;
            self.hold = match self.hold {
                Hold::Armed if start => Hold::Holding,
                _ => Hold::Off,
            };
            self.cued_from =
                ctx.presentation_point().or(Some(from.at)).and_then(|t| t.nanos()).unwrap_or(0);
            let (at, scan) = (from.to_byte, self.mid_segment(from.to_byte));
            log!(
                &*ctx,
                Level::Debug,
                "pull_restart",
                target = from.at,
                at = at,
                scan = scan,
                held = self.hold == Hold::Holding
            );
            self.restart_pull(from);
        }
        if self.hold == Hold::Holding && ctx.pulls(Self::IN) {
            return Ok(Flow::Ok);
        }
        if ctx.pulls(Self::IN) {
            return self.pull(ctx, inputs);
        }
        if let CuesRead::Want { .. } = self.seek.read {
            self.seek.read = CuesRead::Done;
            self.publish(ctx);
        }
        self.settle_side();
        // Consume input only while emission keeps up, so the scheduler's gates absorb a fast
        // source.
        if !self.drain(ctx) {
            ctx.hold_end();
            return Ok(Flow::Ok);
        }
        while let Some(buf) = inputs.pop() {
            self.taken += buf.memory.len() as u64;
            if let Some(from) = self.video_from.take() {
                self.hold_video(from, buf.pts);
            }
            if let Err(e) = self.reader.push(buf.memory.data()) {
                // What was read before the damage still goes out.
                self.flush_exact(ctx);
                return Err(demux_failed(ctx, DemuxError::Parse(e)));
            }
            self.grow_pushed();
            if self.seek.due {
                self.publish(ctx);
            }
            // Posted on the bus since decoders do not declare `duration`, so fixation drops it.
            if !self.posted_duration
                && let Some(ns) = self.reader.duration_ns()
            {
                self.posted_duration = true;
                ctx.set_duration(ns);
            }
            if !self.drain(ctx) {
                ctx.hold_end();
                return Ok(Flow::Ok);
            }
        }
        Ok(Flow::Ok)
    }

    fn on_event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => {
                self.flush_exact(ctx);
                if self.seek.cues.is_empty() && self.seek.grown.unpublished() {
                    self.publish(ctx);
                }
                let (tracks, cued) = (self.pad_tracks.len(), !self.seek.cues.is_empty());
                match self.lost {
                    0 => log!(&*ctx, Level::Info, "eos", tracks = tracks, cued = cued),
                    n => log!(&*ctx, Level::Warn, "eos", tracks = tracks, cued = cued, lost = n),
                }
            }
            // The reader keeps tracks and scale and scans on to the next Cluster, so an estimated
            // byte may land inside a block.
            Event::FlushStart => {
                let target = ctx.seek_target();
                let at = target.map_or(0, |t| t.to_byte);
                log!(&*ctx, Level::Debug, "flush", seek = target.is_some(), at = at);
                self.teller.restart();
                self.measured.clear();
                self.drop_staged();
                self.reader.restart_at(ctx.seek_target().map(|target| target.to_byte));
                self.gate_tracks(ctx);
                if self.seek.cues.is_empty() && self.seek.grown.unpublished() {
                    self.publish(ctx);
                }
            }
            Event::SrcLinked { pad, on, restart } => {
                self.relink(ctx, *pad, *on, *restart);
                if !self.seek.cues.is_empty() {
                    self.publish(ctx);
                }
            }
            Event::SrcJoined { pad } => self.greet(ctx, *pad),
            _ => {}
        }
        Ok(())
    }

    fn on_stop(&mut self, ctx: &mut Ctx) {
        self.flush_exact(ctx);
        self.pull.plan.reset();
        self.pull.win = None;
        self.cued = None;
        self.reader = MatroskaReader::new();
        self.scratch.clear();
        self.reframe_buf.clear();
        self.pending.clear();
        self.posted_duration = false;
    }
}

fn side_records(t35: &Bytes, out: &mut [u8]) -> usize {
    let msg = match t35 {
        Bytes::Owned(v) => &v[..],
        Bytes::View(m, _) => m.data(),
    };
    if matches!(pf_hdr::t35::classify(msg), pf_hdr::t35::T35Kind::Other) {
        return 0;
    }
    let mut w = pf_hdr::side::Writer::new(out);
    w.push(pf_hdr::side::Kind::T35, msg);
    w.len()
}

fn attach_side(memory: &mut Memory, side: &[u8]) {
    if !side.is_empty() {
        let _ = memory.set_side(side);
    }
}

/// The bytes of nothing. `Vec::new` allocates nothing.
#[allow(clippy::disallowed_methods)]
fn empty() -> Vec<u8> {
    Vec::new()
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const N: u64 = PTS_NONE;

    /// The verdicts of `frames` through `gate`, and the gate after, nothing ever held.
    fn run(mut gate: Gate, kind: Kind, frames: &[(u64, bool)]) -> (Vec<Pass>, Gate) {
        let verdicts = frames.iter().map(|&f| pass(&mut gate, kind, f, (None, 0), MS)).collect();
        (verdicts, gate)
    }

    #[test]
    fn annexb_pictures_before_the_parameter_sets_are_dropped_not_fatal() {
        let built = BuiltVideo {
            framing: Framing::AnnexB(NalCodec::H264),
            width: 320,
            height: 240,
            duration_ns: None,
            colour: None,
        };
        let mut sets = None;
        let idr = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00];
        assert!(matches!(built.absorb(&mut sets, 1, &idr), Ok(None)), "mid-GOP, dropped");
        let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xD9];
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCB, 0x83, 0xCB, 0x20]);
        au.extend_from_slice(&idr);
        let tc = built.absorb(&mut sets, 1, &au).unwrap().expect("the sets and a picture");
        assert_eq!(tc.codec_id, "V_MPEG4/ISO/AVC");
        assert_eq!(tc.codec_private[0], 1, "an avcC record");
        let no_size = BuiltVideo { width: 0, height: 0, ..built };
        let mut sets = None;
        assert_eq!(
            no_size.absorb(&mut sets, 1, &au).unwrap_err(),
            MuxError::VideoSizeMissing,
            "PixelWidth 0 is no Matroska, H.264 needs the caps to say"
        );
    }

    #[test]
    fn a_keyframe_gate_opens_on_the_first_keyframe_at_its_point() {
        let gate = Gate::Keyframe { from: 100 * MS };
        let frames =
            [(40 * MS, true), (60 * MS, false), (N, true), (99 * MS, true), (120 * MS, false)];
        let (verdicts, gate) = run(gate, Kind::Video, &frames);
        use Pass::{Drop, Out};
        assert_eq!(verdicts, [Drop, Drop, Drop, Out, Out], "a tick of slack, then open");
        assert_eq!(gate, Gate::Open);
    }

    /// Drives `pass` the way `next_frame` does, held frames kept by stamp.
    fn line(frames: &[u64]) -> (Vec<Pass>, Vec<u64>) {
        let (mut gate, mut held, mut verdicts) =
            (Gate::Line { line: 100 * MS }, Vec::new(), Vec::new());
        for &pts in frames {
            let v =
                pass(&mut gate, Kind::Audio, (pts, true), (held.first().copied(), held.len()), MS);
            match v {
                Pass::Hold => held.push(pts),
                Pass::Replace => held = vec![pts],
                _ => {}
            }
            verdicts.push(v);
        }
        (verdicts, held)
    }

    #[test]
    fn a_line_gate_holds_the_last_lace_at_or_before_its_line() {
        use Pass::{Drop, Hold, Out, Release, Replace};
        let (verdicts, held) = line(&[N, 60 * MS, N, N, 80 * MS, 80 * MS, N, 120 * MS, N]);
        assert_eq!(verdicts, [Drop, Replace, Hold, Hold, Replace, Hold, Hold, Release, Out]);
        assert_eq!(held, [80 * MS, 80 * MS, N], "the newest lace with its unstamped frames");
    }

    #[test]
    fn a_resume_gate_drops_what_went_out_and_passes_the_rest() {
        use Pass::{Drop, Out};
        // Two frames went out stamped 200 ms, the lace head and one unstamped frame after it.
        let gate = Gate::Resume { pts: 200 * MS, left: 2, inside: false };
        let frames = [(180 * MS, true), (N, false), (200 * MS, true), (N, false), (N, false)];
        let (verdicts, after) = run(gate, Kind::Video, &frames);
        assert_eq!(verdicts, [Drop, Drop, Drop, Drop, Out]);
        assert_eq!(after, Gate::Open);
        // Pictures run out of order, a later stamp within the slack was not proof of anything.
        let frames = [(260 * MS, false), (200 * MS, true), (220 * MS, false)];
        let (verdicts, _) =
            run(Gate::Resume { pts: 200 * MS, left: 1, inside: false }, Kind::Video, &frames);
        assert_eq!(verdicts, [Drop, Drop, Out]);
    }

    #[test]
    fn a_resume_gate_passes_the_first_frame_past_everything_put_out() {
        use Pass::{Drop, Out};
        let gate = Gate::Resume { pts: 200 * MS, left: 1, inside: false };
        let late = 200 * MS + RESUME_SLACK + 1;
        let (verdicts, after) = run(gate, Kind::Video, &[(100 * MS, true), (late, false)]);
        assert_eq!(verdicts, [Drop, Out], "the crossing picture goes out, its deltas need it");
        assert_eq!(after, Gate::Open);
        let (verdicts, _) =
            run(gate, Kind::Other, &[(150 * MS, true), (N, true), (201 * MS, true)]);
        assert_eq!(verdicts, [Drop, Drop, Out], "a sparse track passes anything later");
    }
}
