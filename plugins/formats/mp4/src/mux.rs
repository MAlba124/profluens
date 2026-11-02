//! `Mp4Mux`, the ISO-BMFF muxer element (`mp4mux`), one sink pad per track. Progressive mode
//! patches the `mdat` size at the end, fragmented mode writes `moof`/`mdat` pairs and patches
//! nothing.

use std::collections::VecDeque;

use pf_mkv::MkvMux;
use pf_mkv::config::{self, ConfigError, NalCodec, ParamSets};
use pf_mkv::drain;
use pf_mkv::writer::MuxOut;
use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    AlignBy, Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::{Error, Scope};
use profluens_core::event::Event;
use profluens_core::format::{
    Constraint, ConstraintDesc, FieldDesc, FixedFormat, OfferDesc, Value, ValueDesc,
};
use profluens_core::id::{FormatId, SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;

use crate::writer::{
    self, Edit, Entry, Handler, Run, Sample, Table, TrackHead, TrackMoov, WriteError,
};

/// The most tracks one muxer takes, its static sink pads.
pub const MAX_TRACKS: usize = 8;
pub const CHUNK_NS: u64 = 500_000_000;
pub const CHUNK_BYTES: usize = 1 << 20;
/// Bounds of the wait on an empty pad, shared with `mkvmuxn`.
pub use pf_mkv::mux_multi::{HELD_BUFS, HELD_BYTES, LEAD_NS};
use pf_mkv::mux_multi::{PRECONFIG_MAX_BUFS, PRECONFIG_MAX_BYTES};
pub const FRAG_NS: u64 = 2_000_000_000;
pub const FRAG_AUDIO_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One `mdat`, the `moov` at the end, the `mdat` size patched.
    Progressive,
    /// An init `moov`, then `moof` and `mdat` pairs. Nothing is patched.
    Fragmented,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MuxError {
    NoSinkLinked,
    LinkedCount {
        expected: usize,
        linked: usize,
    },
    DataBeforeFormat {
        pad: u32,
    },
    UnsupportedFamily {
        pad: u32,
        family: Box<str>,
    },
    FormatChanged {
        pad: u32,
    },
    Config {
        pad: u32,
        cause: ConfigError,
    },
    ConfigRecord {
        pad: u32,
    },
    AacConfigShort {
        pad: u32,
    },
    FlacHead {
        pad: u32,
        cause: pf_mkv::MuxError,
    },
    Table(WriteError),
    /// `pads` had no track when the file needed one. A warning, their tracks are left out.
    TracksDropped {
        pads: u8,
    },
}

impl std::fmt::Display for MuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSinkLinked => f.write_str("mp4mux: no sink pad is linked"),
            Self::LinkedCount { expected, linked } => {
                write!(f, "mp4mux: made for {expected} tracks, {linked} pads linked")
            }
            Self::DataBeforeFormat { pad } => {
                write!(f, "mp4mux: data on sink_{pad} before its format")
            }
            Self::UnsupportedFamily { pad, family } => {
                write!(f, "mp4mux: sink_{pad} announced {family}, which has no MP4 mapping here")
            }
            Self::FormatChanged { pad } => {
                write!(f, "mp4mux: sink_{pad} changed family after its track was set")
            }
            Self::Config { pad, cause } => write!(f, "mp4mux: sink_{pad}, {cause}"),
            Self::ConfigRecord { pad } => {
                write!(f, "mp4mux: sink_{pad} does not lead with an avcC or hvcC")
            }
            Self::AacConfigShort { pad } => {
                write!(f, "mp4mux: sink_{pad} leads with an AudioSpecificConfig too short")
            }
            Self::FlacHead { pad, cause } => write!(f, "mp4mux: sink_{pad} FLAC head, {cause}"),
            Self::Table(e) => write!(f, "mp4mux: {e}"),
            Self::TracksDropped { pads } => {
                write!(f, "mp4mux: pads {pads:#010b} had no track and are left out")
            }
        }
    }
}

impl std::error::Error for MuxError {}

fn failed(ctx: &Ctx, e: MuxError) -> Error {
    Error::element_failed(ctx.element(), e)
}

static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

const TEXT: [&str; 3] = ["subtitle/srt", "subtitle/vtt", "subtitle/text"];

static VIDEO_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "width", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "height", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "duration", allowed: ConstraintDesc::Any, preferred: None },
];
static SAMPLE_VALUES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];
static AUDIO_FIELDS: [FieldDesc; 5] = [
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "sample", allowed: ConstraintDesc::Set(&SAMPLE_VALUES), preferred: None },
    FieldDesc { field: "duration", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: pf_mkv::codec::FIELD_DELAY, allowed: ConstraintDesc::Any, preferred: None },
];
/// The track families, then `bytes` so an upstream that announces its family on the pad links.
static SINK_OFFERS: [OfferDesc; 13] = [
    OfferDesc { family: "h264/annexb", fields: &VIDEO_FIELDS },
    OfferDesc { family: "h265/annexb", fields: &VIDEO_FIELDS },
    OfferDesc { family: "h264/avcc", fields: &VIDEO_FIELDS },
    OfferDesc { family: "h265/hvcc", fields: &VIDEO_FIELDS },
    OfferDesc { family: "av1", fields: &VIDEO_FIELDS },
    OfferDesc { family: "aac", fields: &AUDIO_FIELDS },
    OfferDesc { family: "opus", fields: &AUDIO_FIELDS },
    OfferDesc { family: "flac", fields: &AUDIO_FIELDS },
    OfferDesc { family: "mp3", fields: &AUDIO_FIELDS },
    OfferDesc::any("subtitle/srt"),
    OfferDesc::any("subtitle/vtt"),
    OfferDesc::any("subtitle/text"),
    OfferDesc::any("bytes"),
];

const fn sink_pad(name: &'static str) -> PadDesc {
    PadDesc { name, offers: &SINK_OFFERS }
}

static SINKS: [PadDesc; MAX_TRACKS] = [
    sink_pad("sink_0"),
    sink_pad("sink_1"),
    sink_pad("sink_2"),
    sink_pad("sink_3"),
    sink_pad("sink_4"),
    sink_pad("sink_5"),
    sink_pad("sink_6"),
    sink_pad("sink_7"),
];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

static PROPS: [PropDesc; 1] =
    [PropDesc { name: "fragmented", allowed: Constraint::Any, live: false }];

// Cold path, once per construction.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "mp4mux",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::All { by: AlignBy::RunningTime },
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(Mp4Mux::new(Mode::Progressive))),
    io: false,
    couplings: &[],
};

enum Setup {
    AwaitCaps,
    AnnexB(ParamSets),
    Record { hevc: bool },
    Av1,
    Aac,
    Opus,
    Flac { head: Vec<u8>, scanned: usize },
    Mp3,
    Ready,
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Verbatim,
    AnnexB(NalCodec),
    Av1,
    Adts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hint {
    None,
    Fixed(u32),
    Opus,
    Mp3,
    Flac,
    Cue,
}

struct Held {
    key: u64,
    pts: u64,
    mem: Option<Memory>,
    sync: bool,
    hint: u32,
}

struct Lane {
    pad: SinkPad,
    id: u32,
    setup: Setup,
    family: &'static str,
    head: Option<TrackHead>,
    framing: Framing,
    hint: Hint,
    width: u32,
    height: u32,
    rate: u32,
    channels: u32,
    delay_ns: u64,
    queue: VecDeque<Held>,
    bytes: usize,
    key: u64,
    last: Option<u64>,
    gap: u64,
    first_ns: Option<u64>,
    next_ns: u64,
    cue_end: Option<u64>,
    table: Option<Table>,
    shift: Option<i64>,
    last_dts: Option<i64>,
    last_dur: u32,
}

impl Lane {
    fn timescale(&self) -> u32 {
        self.head.as_ref().map_or(1000, |h| h.timescale)
    }

    fn handler(&self) -> Handler {
        self.head.as_ref().map_or(Handler::Audio, |h| h.handler)
    }

    fn known(&self) -> u64 {
        self.gap.max(self.last.unwrap_or(0))
    }
}

pub struct Mp4Mux {
    mode: Mode,
    chunk_len: u64,
    expect: Option<usize>,
    lanes: Vec<Lane>,
    out: MuxOut,
    out_off: usize,
    out_fmt: FormatId,
    started: bool,
    done: bool,
    patch_at: u64,
    mdat_at: u64,
    held_bufs: usize,
    held_bytes: usize,
    zero_ns: u64,
    sequence: u32,
    sizes: Vec<u32>,
    pts: Vec<i64>,
    hints: Vec<u32>,
    dts: Vec<i64>,
    durations: Vec<u32>,
    cts: Vec<i64>,
    sync: Vec<bool>,
}

impl Mp4Mux {
    pub const INS: [SinkPad; MAX_TRACKS] = [
        SinkPad(0),
        SinkPad(1),
        SinkPad(2),
        SinkPad(3),
        SinkPad(4),
        SinkPad(5),
        SinkPad(6),
        SinkPad(7),
    ];
    pub const OUT: SrcPad = SrcPad(0);

    // Cold path, once per construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            chunk_len: CHUNK_NS,
            expect: None,
            lanes: Vec::new(),
            out: MuxOut::new(),
            out_off: 0,
            out_fmt: FormatId(0),
            started: false,
            done: false,
            patch_at: 0,
            mdat_at: 0,
            held_bufs: 0,
            held_bytes: 0,
            zero_ns: 0,
            sequence: 0,
            sizes: Vec::new(),
            pts: Vec::new(),
            hints: Vec::new(),
            dts: Vec::new(),
            durations: Vec::new(),
            cts: Vec::new(),
            sync: Vec::new(),
        }
    }

    pub fn tracks(mode: Mode, n: usize) -> Self {
        let mut m = Self::new(mode);
        m.expect = Some(n);
        m
    }

    pub fn chunk_ns(mut self, ns: u64) -> Self {
        self.chunk_len = ns.max(1);
        self
    }

    fn scale(&self) -> usize {
        (self.chunk_len / CHUNK_NS).max(1) as usize
    }

    fn lead(&self) -> u64 {
        LEAD_NS.max(self.chunk_len.saturating_mul(2))
    }

    pub fn entries(&self) -> impl Iterator<Item = Option<&Entry>> {
        self.lanes.iter().map(|l| l.head.as_ref().map(|h| &h.entry))
    }

    fn drain_out(&mut self, ctx: &mut Ctx) -> bool {
        drain::drain(&mut self.out, &mut self.out_off, ctx, Self::OUT, self.out_fmt)
    }

    fn on_format(&mut self, ctx: &mut Ctx, f: &FixedFormat) -> Result<(), Error> {
        let idx = match ctx.event_pad().and_then(|p| self.lanes.iter().position(|l| l.pad == p)) {
            Some(i) => i,
            None if self.lanes.len() == 1 => 0,
            None => return Ok(()),
        };
        let pad = self.lanes[idx].pad.0;
        let name = ctx.family_name(f.family).unwrap_or("?");
        let Some(family) =
            SINK_OFFERS.iter().map(|o| o.family).find(|fam| *fam == name && *fam != "bytes")
        else {
            // Cold path, once per error.
            #[allow(clippy::disallowed_methods)]
            let family = name.into();
            return Err(failed(ctx, MuxError::UnsupportedFamily { pad, family }));
        };
        let int = |field: &str| match ctx.field_id(field).and_then(|id| f.get(id)) {
            Some(Value::Int(n)) if n > 0 => Some(n as u64),
            _ => None,
        };
        let (width, height) = (int("width").unwrap_or(0) as u32, int("height").unwrap_or(0) as u32);
        let (rate, channels) =
            (int("rate").unwrap_or(0) as u32, int("channels").unwrap_or(0) as u32);
        let delay_ns = int(pf_mkv::codec::FIELD_DELAY).unwrap_or(0);
        let lane = &mut self.lanes[idx];
        if !matches!(lane.setup, Setup::AwaitCaps) {
            return match lane.family == family {
                true => Ok(()),
                false => Err(failed(ctx, MuxError::FormatChanged { pad })),
            };
        }
        (lane.family, lane.width, lane.height, lane.rate, lane.channels, lane.delay_ns) =
            (family, width, height, rate, channels, delay_ns);
        lane.setup = match family {
            "h264/annexb" => Setup::AnnexB(ParamSets::new(NalCodec::H264)),
            "h265/annexb" => Setup::AnnexB(ParamSets::new(NalCodec::H265)),
            "h264/avcc" => Setup::Record { hevc: false },
            "h265/hvcc" => Setup::Record { hevc: true },
            "av1" => Setup::Av1,
            "aac" => Setup::Aac,
            "opus" => Setup::Opus,
            // Cold path, once per track.
            #[allow(clippy::disallowed_methods)]
            "flac" => Setup::Flac { head: Vec::new(), scanned: 0 },
            "mp3" => Setup::Mp3,
            _ => {
                Self::ready(lane, text_head(lane.id), Framing::Verbatim, Hint::Cue, self.mode)
                    .map_err(|e| failed(ctx, e))?;
                Setup::Ready
            }
        };
        Ok(())
    }

    fn ready(
        lane: &mut Lane,
        head: TrackHead,
        framing: Framing,
        hint: Hint,
        mode: Mode,
    ) -> Result<(), MuxError> {
        if mode == Mode::Progressive {
            lane.table = Some(Table::new(head.id).map_err(MuxError::Table)?);
        }
        (lane.framing, lane.hint) = (framing, hint);
        lane.head = Some(head);
        lane.setup = Setup::Ready;
        Ok(())
    }

    fn audio_head(lane: &Lane, entry: Entry, rate: u32, channels: u32) -> TrackHead {
        writer::audio_head(lane.id, entry, rate, channels.min(u16::MAX as u32) as u16)
    }

    fn video_head(lane: &Lane, entry: Entry, width: u32, height: u32) -> TrackHead {
        writer::video_head(lane.id, entry, width, height)
    }

    // Per buffer, the setup arms that allocate run once per track.
    #[allow(clippy::disallowed_methods)]
    fn accept(&mut self, ctx: &mut Ctx, idx: usize, buf: Buffer) -> Result<(), MuxError> {
        if self.done {
            return Ok(());
        }
        let mode = self.mode;
        let lane = &mut self.lanes[idx];
        let pad = lane.pad.0;
        let data = buf.memory.data();
        match &mut lane.setup {
            // A head sent again (OpusTags behind OpusHead, a demuxer's greet) is no sample.
            Setup::Ready if buf.flags.contains(BufferFlags::HEADER) => {
                log!(&*ctx, Level::Trace, "head_again", track = lane.id);
                return Ok(());
            }
            Setup::Ready => {}
            Setup::Dropped => return Ok(()),
            // `flacenc` announces no family. A native FLAC head says what the stream is.
            Setup::AwaitCaps if data.starts_with(b"fLaC") => {
                lane.family = "flac";
                lane.setup = Setup::Flac { head: Vec::new(), scanned: 0 };
                return self.accept(ctx, idx, buf);
            }
            Setup::AwaitCaps => return Err(MuxError::DataBeforeFormat { pad }),
            Setup::AnnexB(sets) => {
                sets.absorb(data).map_err(|cause| MuxError::Config { pad, cause })?;
                let codec = sets.codec();
                if !config::scan(codec, data).vcl {
                    return Ok(());
                }
                let record = sets.record().map_err(|cause| MuxError::Config { pad, cause })?;
                let (mut w, mut h) = (lane.width, lane.height);
                let entry = match codec {
                    NalCodec::H264 => Entry::Avc(record),
                    NalCodec::H265 => {
                        if w == 0 || h == 0 {
                            let sps = config::nals(data)
                                .map(|(a, b)| &data[a..b])
                                .find(|n| codec.kind(n[0]) == config::NalKind::Sps);
                            if let Some(s) = sps.and_then(|s| config::hevc_sps(s).ok()) {
                                (w, h) = (s.width, s.height);
                            }
                        }
                        Entry::Hevc { record, inband: true }
                    }
                };
                let head = Self::video_head(lane, entry, w, h);
                Self::ready(lane, head, Framing::AnnexB(codec), Hint::None, mode)?;
            }
            Setup::Record { hevc } => {
                if data.first() != Some(&1) {
                    return Err(MuxError::ConfigRecord { pad });
                }
                let entry = match hevc {
                    true => Entry::Hevc { record: data.to_vec(), inband: false },
                    false => Entry::Avc(data.to_vec()),
                };
                let head = Self::video_head(lane, entry, lane.width, lane.height);
                return Self::ready(lane, head, Framing::Verbatim, Hint::None, mode);
            }
            Setup::Av1 => {
                let (record, seq) =
                    config::av1c(data).map_err(|cause| MuxError::Config { pad, cause })?;
                let (w, h) = match (lane.width, lane.height) {
                    (0, _) | (_, 0) => (seq.width, seq.height),
                    wh => wh,
                };
                let head = Self::video_head(lane, Entry::Av1(record), w, h);
                Self::ready(lane, head, Framing::Av1, Hint::None, mode)?;
            }
            Setup::Aac => {
                if let Some((asc, rate, channels)) = adts_config(data) {
                    let rate = if lane.rate != 0 { lane.rate } else { rate };
                    let channels = if lane.channels != 0 { lane.channels } else { channels };
                    let hint = aac_hint(&asc, rate);
                    let head = Self::audio_head(lane, Entry::Aac(asc), rate, channels);
                    Self::ready(lane, head, Framing::Adts, hint, mode)?;
                } else {
                    if data.len() < 2 {
                        return Err(MuxError::AacConfigShort { pad });
                    }
                    let asc = data.to_vec();
                    let (r, c) = asc_rate_channels(&asc);
                    let rate = if lane.rate != 0 { lane.rate } else { r };
                    let channels = if lane.channels != 0 { lane.channels } else { c };
                    let hint = aac_hint(&asc, rate);
                    let head = Self::audio_head(lane, Entry::Aac(asc), rate, channels);
                    return Self::ready(lane, head, Framing::Verbatim, hint, mode);
                }
            }
            Setup::Opus => {
                let pre_skip =
                    writer::ticks(lane.delay_ns as i64, 48_000).clamp(0, u16::MAX as i64) as u16;
                if let Some(dops) = writer::dops_from_opus_head(data) {
                    let channels = u32::from(dops[1]);
                    if lane.delay_ns == 0 {
                        let skip = u16::from_be_bytes([dops[2], dops[3]]);
                        lane.delay_ns = u64::from(skip) * 1_000_000_000 / 48_000;
                    }
                    let head = Self::audio_head(lane, Entry::Opus(dops), 48_000, channels);
                    return Self::ready(lane, head, Framing::Verbatim, Hint::Opus, mode);
                }
                let channels = if lane.channels == 0 { 2 } else { lane.channels.min(2) };
                let dops = writer::dops(channels as u8, pre_skip, lane.rate.max(48_000));
                let head = Self::audio_head(lane, Entry::Opus(dops), 48_000, channels);
                Self::ready(lane, head, Framing::Verbatim, Hint::Opus, mode)?;
            }
            Setup::Flac { head, scanned } => {
                head.extend_from_slice(data);
                let len = MkvMux::flac_head_len(head, scanned)
                    .map_err(|cause| MuxError::FlacHead { pad, cause })?;
                let Some(len) = len else { return Ok(()) };
                if len != head.len() {
                    let cause = pf_mkv::MuxError::FlacHeadSplit { head: len, buffered: head.len() };
                    return Err(MuxError::FlacHead { pad, cause });
                }
                let head = std::mem::take(head);
                let (rate, channels, _) = MkvMux::flac_streaminfo_params(&head)
                    .map_err(|cause| MuxError::FlacHead { pad, cause })?;
                let fixed = u16::from_be_bytes([head[8], head[9]]);
                let hint = if fixed != 0 && fixed == u16::from_be_bytes([head[10], head[11]]) {
                    Hint::Fixed(u32::from(fixed))
                } else {
                    Hint::Flac
                };
                let entry = Entry::Flac(head[4..].to_vec());
                let head = Self::audio_head(lane, entry, rate as u32, channels);
                return Self::ready(lane, head, Framing::Verbatim, hint, mode);
            }
            Setup::Mp3 => {
                let (r, c) = mp3_rate_channels(data).unwrap_or((44_100, 2));
                let rate = if lane.rate != 0 { lane.rate } else { r };
                let channels = if lane.channels != 0 { lane.channels } else { c };
                let head = Self::audio_head(lane, Entry::Mp3, rate, channels);
                Self::ready(lane, head, Framing::Verbatim, Hint::Mp3, mode)?;
            }
        }
        self.enqueue(ctx, idx, buf);
        Ok(())
    }

    fn enqueue(&mut self, ctx: &mut Ctx, idx: usize, buf: Buffer) {
        let lane = &mut self.lanes[idx];
        let mut mem = buf.memory;
        match lane.framing {
            Framing::Verbatim => {}
            Framing::AnnexB(codec) => match config::length_prefixed_sample(ctx, codec, mem) {
                Some(m) => mem = m,
                None => {
                    // Parameter sets again, a head sent once more.
                    log!(&*ctx, Level::Trace, "sample_empty", track = lane.id);
                    return;
                }
            },
            Framing::Av1 => match config::av1_sample(mem) {
                Some(m) => mem = m,
                None => {
                    log!(&*ctx, Level::Trace, "sample_empty", track = lane.id);
                    return;
                }
            },
            Framing::Adts => {
                let d = mem.data();
                let head = if d.len() >= 7 && d[1] & 1 == 0 { 9 } else { 7 };
                let Some(rest) = mem.slice(head.min(mem.len()), mem.len().saturating_sub(head))
                else {
                    return;
                };
                mem = rest;
            }
        }
        if mem.is_empty() {
            return;
        }
        let ts = lane.timescale();
        let pts = match buf.pts.nanos() {
            Some(p) => p,
            None => lane.next_ns,
        };
        let hint = match lane.hint {
            Hint::None => 0,
            Hint::Fixed(n) => n,
            Hint::Opus => opus_samples(mem.data()),
            Hint::Mp3 => mp3_samples(mem.data()),
            Hint::Flac => flac_samples(mem.data()),
            Hint::Cue => buf
                .duration
                .nanos()
                .map_or(0, |d| writer::ticks(d as i64, ts).clamp(0, u32::MAX as i64) as u32),
        };
        let step = match hint {
            0 => 40_000_000,
            h => u64::from(h) * 1_000_000_000 / u64::from(ts.max(1)),
        };
        lane.next_ns = pts.saturating_add(step);
        if lane.hint == Hint::Cue {
            if let Some(end) = lane.cue_end.filter(|&e| pts > e) {
                let fill = writer::ticks((pts - end) as i64, ts).clamp(0, u32::MAX as i64) as u32;
                lane.key = lane.key.max(end);
                lane.queue.push_back(Held {
                    key: lane.key,
                    pts: end,
                    mem: None,
                    sync: true,
                    hint: fill,
                });
                self.held_bufs += 1;
            }
            // An untimed cue ends at the next one, a fill at its own pts would show it one
            // tick.
            lane.cue_end = buf.duration.nanos().map(|d| pts.saturating_add(d));
        }
        lane.key = lane.key.max(pts);
        lane.first_ns = Some(lane.first_ns.map_or(pts, |f| f.min(pts)));
        let sync = !buf.flags.contains(BufferFlags::DELTA);
        lane.bytes += mem.len();
        self.held_bytes += mem.len();
        self.held_bufs += 1;
        lane.queue.push_back(Held { key: lane.key, pts, mem: Some(mem), sync, hint });
    }

    fn ahead(&self) -> Option<u64> {
        self.lanes.iter().filter_map(|l| l.queue.back().map(|h| h.key)).max()
    }

    fn pressed(&self) -> bool {
        self.held_bufs >= HELD_BUFS * self.scale() || self.held_bytes >= HELD_BYTES * self.scale()
    }

    /// The key a sample must be below to go out ahead of what empty open pads may still bring.
    /// `None` when no pad is waited for. Not a `u64::MAX` sentinel, which held such samples to EOS.
    fn floor(&self, ctx: &Ctx, ahead: u64) -> Option<u64> {
        self.lanes
            .iter()
            .filter(|l| {
                l.queue.is_empty()
                    && !ctx.is_pad_closed(l.pad)
                    && !matches!(l.setup, Setup::Dropped)
            })
            .map(Lane::known)
            .filter(|&k| ahead <= k.saturating_add(self.lead()))
            .min()
    }

    fn step(&mut self, ctx: &mut Ctx, at_eos: bool) -> Result<(), MuxError> {
        let was = self.started;
        let stepped = match self.mode {
            Mode::Progressive => self.step_progressive(ctx, at_eos),
            Mode::Fragmented => self.step_fragmented(ctx, at_eos),
        };
        if !was && self.started {
            self.log_tracks(ctx);
        }
        stepped
    }

    /// Each track as its head is written, once per file.
    fn log_tracks(&self, ctx: &Ctx) {
        let fragmented = self.mode == Mode::Fragmented;
        log!(ctx, Level::Info, "opened", tracks = self.lanes.len(), fragmented = fragmented);
        for l in &self.lanes {
            // Dropped ones went to the bus, a progressive one may still get its head.
            let Some(h) = &l.head else {
                let dropped = matches!(l.setup, Setup::Dropped);
                log!(ctx, Level::Debug, "track_headless", pad = l.pad.0, dropped = dropped);
                continue;
            };
            let (id, family) = (h.id, l.family);
            match h.handler {
                Handler::Video => {
                    let (w, h) = (h.width, h.height);
                    log!(
                        ctx,
                        Level::Info,
                        "track",
                        track = id,
                        family = family,
                        width = w,
                        height = h
                    )
                }
                Handler::Audio => {
                    let (r, c) = (h.rate, h.channels);
                    log!(
                        ctx,
                        Level::Info,
                        "track",
                        track = id,
                        family = family,
                        rate = r,
                        channels = c
                    )
                }
                Handler::Text => log!(ctx, Level::Info, "track", track = id, family = family),
            }
        }
    }

    fn step_progressive(&mut self, ctx: &mut Ctx, at_eos: bool) -> Result<(), MuxError> {
        loop {
            let Some(ahead) = self.ahead() else {
                return Ok(());
            };
            let pressed = self.pressed();
            let floor = if at_eos || pressed { None } else { self.floor(ctx, ahead) };
            let Some((i, front)) = self
                .lanes
                .iter()
                .enumerate()
                .filter_map(|(i, l)| l.queue.front().map(|h| (i, h.key)))
                .min_by_key(|&(_, k)| k)
            else {
                return Ok(());
            };
            if floor.is_some_and(|f| front >= f) {
                return Ok(());
            }
            let l = &self.lanes[i];
            let span = l.queue.back().map_or(0, |h| h.key) - front;
            let ready = at_eos
                || pressed
                || ctx.is_pad_closed(l.pad)
                || span >= self.chunk_len
                || l.bytes >= CHUNK_BYTES * self.scale()
                || front.saturating_add(self.lead()) < ahead;
            if !ready {
                return Ok(());
            }
            self.chunk(i, front.saturating_add(self.chunk_len), floor)?;
            if !at_eos && !self.drain_out(ctx) {
                return Ok(());
            }
        }
    }

    fn progressive_head(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let before = self.out.emitted();
        let patch = self.out.append_bytes_with(|b| {
            let base = b.len();
            writer::ftyp(b, false);
            writer::mdat_head(b) - base
        });
        self.patch_at = before + patch as u64;
        self.mdat_at = self.patch_at - 8;
    }

    fn chunk(&mut self, i: usize, end: u64, floor: Option<u64>) -> Result<(), MuxError> {
        self.progressive_head();
        let (mut bytes, max) = (0usize, CHUNK_BYTES * self.scale());
        let mut first = true;
        loop {
            let lane = &mut self.lanes[i];
            let Some(h) = lane.queue.front() else { break };
            if !first && (h.key >= end || floor.is_some_and(|f| h.key >= f) || bytes >= max) {
                break;
            }
            let h = lane.queue.pop_front().expect("front just seen");
            let at = self.out.emitted();
            let size = Self::put_sample(&mut self.out, lane.handler(), h.mem.as_ref());
            let (len, ts, id) = (h.mem.as_ref().map_or(0, Memory::len), lane.timescale(), lane.id);
            let sample =
                Sample { size, pts: writer::ticks(h.pts as i64, ts), hint: h.hint, sync: h.sync };
            if let Some(t) = lane.table.as_mut() {
                t.push(id, sample, first.then_some(at)).map_err(MuxError::Table)?;
            }
            lane.last = Some(h.key);
            lane.bytes -= len;
            self.held_bytes -= len;
            self.held_bufs -= 1;
            bytes += size as usize;
            first = false;
        }
        Ok(())
    }

    fn put_sample(out: &mut MuxOut, handler: Handler, mem: Option<&Memory>) -> u32 {
        let len = mem.map_or(0, Memory::len);
        if handler == Handler::Text {
            let n = len.min(u16::MAX as usize - 2) as u16;
            out.append_bytes_with(|b| b.extend_from_slice(&n.to_be_bytes()));
            if let Some(m) = mem.filter(|m| !m.is_empty()) {
                out.push_payload(m.slice(0, n as usize).unwrap_or_else(|| m.clone()));
            }
            return 2 + u32::from(n);
        }
        if let Some(m) = mem {
            out.push_payload(m.clone());
        }
        len as u32
    }

    fn step_fragmented(&mut self, ctx: &mut Ctx, at_eos: bool) -> Result<(), MuxError> {
        loop {
            let Some(ahead) = self.ahead() else {
                return Ok(());
            };
            let pressed = self.pressed();
            let cut = match (at_eos, pressed) {
                (true, _) => None,
                (_, true) => self.clean_cut(),
                _ => match self.cut(ctx, ahead) {
                    Some(cut) => Some(cut),
                    None => return Ok(()),
                },
            };
            if !self.started && !self.init_segment(ctx, at_eos) {
                return Ok(());
            }
            self.fragment(cut);
            if !at_eos && !self.drain_out(ctx) {
                return Ok(());
            }
        }
    }

    /// Where a fragment ends when the muxer holds too much to wait for a keyframe. The last place
    /// in the first video queue where everything behind presents after everything ahead, so
    /// decode times stay the track's.
    fn clean_cut(&self) -> Option<u64> {
        let v = self.lanes.iter().find(|l| l.handler() == Handler::Video && l.head.is_some())?;
        let mut after = u64::MAX;
        for j in (1..v.queue.len()).rev() {
            after = after.min(v.queue[j].pts);
            if after > v.queue[j - 1].key {
                return Some(v.queue[j].key);
            }
        }
        None
    }

    fn cut(&self, ctx: &Ctx, ahead: u64) -> Option<u64> {
        let anchor = self
            .lanes
            .iter()
            .position(|l| l.handler() == Handler::Video && l.head.is_some())
            .unwrap_or(0);
        let start = self.lanes.iter().filter_map(|l| l.queue.front().map(|h| h.key)).min()?;
        let a = &self.lanes[anchor];
        let key = match a.handler() {
            Handler::Video => {
                a.queue.iter().skip(1).find(|h| h.sync && h.key > start).map(|h| h.key)
            }
            _ => None,
        };
        let long = if a.handler() == Handler::Video { FRAG_NS } else { FRAG_AUDIO_NS };
        let cut = key.or_else(|| {
            (ahead >= start.saturating_add(long)).then(|| start.saturating_add(long))
        })?;
        let covered = self.lanes.iter().all(|l| match l.queue.back() {
            _ if ctx.is_pad_closed(l.pad) || matches!(l.setup, Setup::Dropped) => true,
            Some(h) => h.key >= cut,
            None => l.known() >= cut || ahead > l.known().saturating_add(LEAD_NS),
        });
        covered.then_some(cut)
    }

    /// Write the init segment once every pad has its track. `false` while it waits.
    // Cold path, once per file.
    #[allow(clippy::disallowed_methods)]
    fn init_segment(&mut self, ctx: &mut Ctx, now: bool) -> bool {
        let waiting = self.lanes.iter().any(|l| {
            l.head.is_none() && !matches!(l.setup, Setup::Dropped) && !ctx.is_pad_closed(l.pad)
        });
        let full = self.held_bufs > PRECONFIG_MAX_BUFS || self.held_bytes > PRECONFIG_MAX_BYTES;
        if waiting && !now && !full {
            return false;
        }
        self.drop_unset(ctx);
        self.started = true;
        self.zero_ns = self.lanes.iter().filter_map(|l| l.first_ns).min().unwrap_or(0);
        // The reorder shift from the first fragment, fixed from here on as the edit.
        for l in self.lanes.iter_mut().filter(|l| l.handler() == Handler::Video) {
            let ts = l.timescale();
            let pts: Vec<i64> = l.queue.iter().map(|h| writer::ticks(h.pts as i64, ts)).collect();
            let mut dts = vec![0i64; pts.len()];
            let hints = vec![0u32; pts.len()];
            l.shift =
                Some(writer::decode_times(Handler::Video, ts, &pts, &hints, None, None, &mut dts));
        }
        let edits: Vec<Edit> = self
            .lanes
            .iter()
            .map(|l| {
                let media_time = match l.handler() {
                    Handler::Video => l.shift.unwrap_or(0),
                    Handler::Audio => writer::ticks(l.delay_ns as i64, l.timescale()),
                    Handler::Text => 0,
                };
                Edit { empty: 0, media_time, duration: 0 }
            })
            .collect();
        let tracks: Vec<TrackMoov<'_>> = self
            .lanes
            .iter()
            .zip(&edits)
            .filter_map(|(l, e)| {
                Some(TrackMoov {
                    head: l.head.as_ref()?,
                    edit: *e,
                    table: None,
                    dts: &[],
                    media_duration: 0,
                })
            })
            .collect();
        let mut init = Vec::new();
        writer::ftyp(&mut init, true);
        writer::moov(&mut init, &tracks, true);
        self.out.append_bytes_with(|b| b.extend_from_slice(&init));
        true
    }

    /// They stay lanes, so what they bring later is dropped and never holds their upstream.
    fn drop_unset(&mut self, ctx: &mut Ctx) {
        let mut pads = 0u8;
        for l in
            self.lanes.iter_mut().filter(|l| l.head.is_none() && !matches!(l.setup, Setup::Dropped))
        {
            pads |= 1 << l.pad.0.min(7);
            l.setup = Setup::Dropped;
        }
        if pads != 0 && self.lanes.iter().any(|l| l.head.is_some()) {
            let element = ctx.element();
            ctx.post(BusMessage::Warning {
                element,
                error: failed(ctx, MuxError::TracksDropped { pads }),
            });
        }
    }

    // Cold path, the columns grow to the largest fragment once.
    #[allow(clippy::disallowed_methods)]
    fn fragment(&mut self, cut: Option<u64>) {
        self.sequence += 1;
        for c in [&mut self.sizes, &mut self.hints, &mut self.durations] {
            c.clear();
        }
        for c in [&mut self.pts, &mut self.dts, &mut self.cts] {
            c.clear();
        }
        self.sync.clear();
        let mut ranges = [(0usize, 0usize, 0u64); MAX_TRACKS];
        for (k, l) in self.lanes.iter_mut().enumerate() {
            let n = l.queue.iter().take_while(|h| cut.is_none_or(|c| h.key < c)).count();
            let at = self.sizes.len();
            if n == 0 {
                ranges[k] = (at, at, 0);
                continue;
            }
            let (ts, handler) = (l.timescale(), l.handler());
            for h in l.queue.iter().take(n) {
                let len = h.mem.as_ref().map_or(0, Memory::len);
                let size =
                    if handler == Handler::Text { 2 + len.min(u16::MAX as usize - 2) } else { len };
                self.sizes.push(size as u32);
                self.pts.push(writer::ticks(h.pts as i64, ts));
                self.hints.push(h.hint);
                self.sync.push(h.sync);
            }
            self.dts.resize(at + n, 0);
            let prev = l.last_dts.map(|d| (d, l.last_dur));
            let shift = writer::decode_times(
                handler,
                ts,
                &self.pts[at..],
                &self.hints[at..],
                l.shift,
                prev,
                &mut self.dts[at..],
            );
            l.shift.get_or_insert(shift);
            let fallback = if l.last_dur != 0 { l.last_dur } else { ts / 25 };
            for i in 0..n {
                let d = writer::duration_at(&self.dts[at..], &self.hints[at..], i, fallback);
                self.durations.push(d);
                self.cts.push(if handler == Handler::Video {
                    self.pts[at + i] - self.dts[at + i]
                } else {
                    0
                });
            }
            l.last_dts = self.dts.last().copied();
            l.last_dur = self.durations.last().copied().unwrap_or(fallback);
            let zero = writer::ticks(self.zero_ns as i64, ts);
            let lift = if handler == Handler::Video { l.shift.unwrap_or(0) } else { 0 };
            ranges[k] = (at, at + n, (self.dts[at] + lift - zero).max(0) as u64);
        }
        let none = Run { track: 0, base_dts: 0, sizes: &[], durations: &[], cts: &[], sync: &[] };
        let mut runs = [none; MAX_TRACKS];
        let mut m = 0usize;
        for (k, l) in self.lanes.iter().enumerate() {
            let (a, b, base) = ranges[k];
            if a == b {
                continue;
            }
            runs[m] = Run {
                track: l.id,
                base_dts: base,
                sizes: &self.sizes[a..b],
                durations: &self.durations[a..b],
                cts: &self.cts[a..b],
                sync: &self.sync[a..b],
            };
            m += 1;
        }
        let seq = self.sequence;
        self.out.append_bytes_with(|b| {
            writer::moof(b, seq, &runs[..m]);
        });
        for (k, l) in self.lanes.iter_mut().enumerate() {
            let (a, b, _) = ranges[k];
            let handler = l.handler();
            for _ in a..b {
                let h = l.queue.pop_front().expect("counted above");
                let len = h.mem.as_ref().map_or(0, Memory::len);
                Self::put_sample(&mut self.out, handler, h.mem.as_ref());
                l.last = Some(h.key);
                l.bytes -= len;
                self.held_bytes -= len;
                self.held_bufs -= 1;
            }
        }
    }

    // Cold path, once per file.
    #[allow(clippy::disallowed_methods)]
    fn finish(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        if self.mode == Mode::Fragmented {
            self.step(ctx, true).map_err(|e| failed(ctx, e))?;
            drain::drain_exact(&mut self.out, &mut self.out_off, ctx, Self::OUT, self.out_fmt);
            let (fragments, bytes) = (self.sequence, self.out.emitted());
            log!(&*ctx, Level::Info, "eos", fragments = fragments, bytes = bytes);
            return Ok(());
        }
        self.step(ctx, true).map_err(|e| failed(ctx, e))?;
        if !self.started {
            log!(&*ctx, Level::Warn, "eos_empty", tracks = self.lanes.len());
            return Ok(());
        }
        self.drop_unset(ctx);
        let zero = self.lanes.iter().filter_map(|l| l.first_ns).min().unwrap_or(0);
        let mut dts: Vec<Vec<i64>> = Vec::with_capacity(self.lanes.len());
        let mut edits = Vec::with_capacity(self.lanes.len());
        for l in &self.lanes {
            let (Some(head), Some(table)) = (l.head.as_ref(), l.table.as_ref()) else {
                dts.push(Vec::new());
                edits.push((Edit::default(), 0));
                continue;
            };
            let mut d = vec![0i64; table.len()];
            writer::decode_times(
                head.handler,
                head.timescale,
                &table.pts,
                &table.hints,
                None,
                None,
                &mut d,
            );
            let delay = match head.handler {
                Handler::Audio => writer::ticks(l.delay_ns as i64, head.timescale),
                _ => 0,
            };
            let empty_ns = l.first_ns.unwrap_or(zero) - zero;
            let empty = writer::ticks(empty_ns as i64, writer::MOVIE_TIMESCALE) as u64;
            edits.push(writer::progressive_edit(head, table, &d, delay, empty));
            dts.push(d);
        }
        let tracks: Vec<TrackMoov<'_>> = self
            .lanes
            .iter()
            .zip(&dts)
            .zip(&edits)
            .filter_map(|((l, d), (e, media))| {
                Some(TrackMoov {
                    head: l.head.as_ref()?,
                    edit: *e,
                    table: l.table.as_ref(),
                    dts: d,
                    media_duration: *media,
                })
            })
            .collect();
        let moov_at = self.out.emitted();
        self.out.append_bytes_with(|b| writer::moov(b, &tracks, false));
        drain::drain_exact(&mut self.out, &mut self.out_off, ctx, Self::OUT, self.out_fmt);
        let size = moov_at - self.mdat_at;
        let Some(mut buf) = ctx.alloc_exact(8) else {
            return Err(ctx.exact_refused(8).scoped(Scope::Pipeline));
        };
        buf.memory.as_mut_full()[..8].copy_from_slice(&size.to_be_bytes());
        buf.memory.set_len(8);
        ctx.push_event(Self::OUT, Event::Patch { offset: self.patch_at, data: buf.memory });
        log!(&*ctx, Level::Info, "eos", mdat = size, bytes = self.out.emitted());
        Ok(())
    }
}

impl Element for Mp4Mux {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    // Cold path, once per run.
    #[allow(clippy::disallowed_methods)]
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(n)) = ctx.prop("fragmented") {
            self.mode = if n != 0 { Mode::Fragmented } else { Mode::Progressive };
        }
        self.lanes.clear();
        for pad in Self::INS {
            if ctx.negotiated_sink(pad).is_none() {
                continue;
            }
            let id = self.lanes.len() as u32 + 1;
            self.lanes.push(Lane {
                pad,
                id,
                setup: Setup::AwaitCaps,
                family: "",
                head: None,
                framing: Framing::Verbatim,
                hint: Hint::None,
                width: 0,
                height: 0,
                rate: 0,
                channels: 0,
                delay_ns: 0,
                queue: VecDeque::with_capacity(HELD_BUFS * 2),
                bytes: 0,
                key: 0,
                last: None,
                gap: 0,
                first_ns: None,
                next_ns: 0,
                cue_end: None,
                table: None,
                shift: None,
                last_dts: None,
                last_dur: 0,
            });
        }
        // A text pad is a track from its link, since a demuxer announces it with its first cue,
        // which may come late.
        for l in self.lanes.iter_mut() {
            let linked = ctx.negotiated_sink(l.pad).and_then(|f| ctx.family_name(f.family));
            if let Some(fam) = linked.and_then(|n| TEXT.iter().copied().find(|t| *t == n)) {
                l.family = fam;
                Self::ready(l, text_head(l.id), Framing::Verbatim, Hint::Cue, self.mode)
                    .map_err(|e| failed(ctx, e))?;
            }
        }
        if self.lanes.is_empty() {
            return Err(failed(ctx, MuxError::NoSinkLinked));
        }
        if let Some(expected) = self.expect.filter(|&n| n != self.lanes.len()) {
            return Err(failed(ctx, MuxError::LinkedCount { expected, linked: self.lanes.len() }));
        }
        (self.started, self.done, self.held_bufs, self.held_bytes, self.sequence) =
            (false, false, 0, 0, 0);
        self.out.clear();
        self.out_off = 0;
        self.out_fmt = ctx.out_format();
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.drain_out(ctx) {
            return Ok(Flow::Ok);
        }
        if self.lanes.len() == 1 {
            while let Some(buf) = inputs.pop() {
                self.accept(ctx, 0, buf).map_err(|e| failed(ctx, e))?;
            }
        }
        for i in 0..self.lanes.len() {
            let mut batch = ctx.take_input_on(self.lanes[i].pad);
            while let Some(buf) = batch.pop_front() {
                self.accept(ctx, i, buf).map_err(|e| failed(ctx, e))?;
            }
            ctx.recycle_input(batch);
        }
        if !self.done {
            self.step(ctx, false).map_err(|e| failed(ctx, e))?;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => self.finish(ctx)?,
            Event::FormatChange(f) if !self.done => self.on_format(ctx, f)?,
            Event::Gap { until } => {
                let pad = ctx.event_pad();
                let one = self.lanes.len() == 1;
                if let Some(l) = self.lanes.iter_mut().find(|l| Some(l.pad) == pad || one) {
                    l.gap = l.gap.max(until.nanos().unwrap_or(0));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        // Nothing past stop reports it, so the file is left unfinished.
        if self.finish(ctx).is_err() {
            log!(&*ctx, Level::Error, "finish_failed", tracks = self.lanes.len());
        }
        self.out = MuxOut::new();
        self.out_off = 0;
    }
}

fn text_head(id: u32) -> TrackHead {
    TrackHead {
        id,
        handler: Handler::Text,
        entry: Entry::Tx3g,
        timescale: writer::TEXT_TIMESCALE,
        width: 0,
        height: 0,
        channels: 0,
        rate: 0,
    }
}

/// ISO/IEC 14496-3 1.6.3.4.
const AAC_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// ISO/IEC 13818-7 6.2. `None` for no ADTS sync word.
// Cold path, once per track.
#[allow(clippy::disallowed_methods)]
fn adts_config(d: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if d.len() < 7 || d[0] != 0xFF || d[1] & 0xF6 != 0xF0 {
        return None;
    }
    let aot = (d[2] >> 6) + 1;
    let sfi = (d[2] >> 2) & 0x0F;
    let ch = ((d[2] & 1) << 2) | (d[3] >> 6);
    let rate = *AAC_RATES.get(usize::from(sfi))?;
    let asc = vec![(aot << 3) | (sfi >> 1), ((sfi & 1) << 7) | (ch << 3)];
    Some((asc, rate, u32::from(ch)))
}

fn asc_rate_channels(asc: &[u8]) -> (u32, u32) {
    if asc.len() < 2 {
        return (0, 0);
    }
    let sfi = ((asc[0] & 7) << 1) | (asc[1] >> 7);
    let ch = (asc[1] >> 3) & 0x0F;
    (AAC_RATES.get(usize::from(sfi)).copied().unwrap_or(0), u32::from(ch))
}

/// 1024 samples, or 960 where the GASpecificConfig says so.
fn aac_hint(asc: &[u8], timescale: u32) -> Hint {
    let (core, _) = asc_rate_channels(asc);
    if core == 0 {
        return Hint::None;
    }
    let frame: u32 = if asc.len() >= 2 && asc[1] & 0x04 != 0 { 960 } else { 1024 };
    match (u64::from(frame) * u64::from(timescale)).checked_div(u64::from(core)) {
        Some(n) if n * u64::from(core) == u64::from(frame) * u64::from(timescale) => {
            Hint::Fixed(n as u32)
        }
        _ => Hint::None,
    }
}

/// RFC 6716 3.1.
fn opus_samples(p: &[u8]) -> u32 {
    let Some(&toc) = p.first() else { return 0 };
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => p.get(1).map_or(0, |c| u32::from(c & 0x3F)),
    };
    frame * count
}

fn mp3_samples(d: &[u8]) -> u32 {
    if d.len() < 4 || d[0] != 0xFF || d[1] & 0xE0 != 0xE0 {
        return 0;
    }
    let version = (d[1] >> 3) & 3;
    match (d[1] >> 1) & 3 {
        3 => 384,
        2 => 1152,
        1 if version == 3 => 1152,
        1 => 576,
        _ => 0,
    }
}

fn mp3_rate_channels(d: &[u8]) -> Option<(u32, u32)> {
    if d.len() < 4 || d[0] != 0xFF || d[1] & 0xE0 != 0xE0 {
        return None;
    }
    let base = [44_100, 48_000, 32_000].get(usize::from((d[2] >> 2) & 3)).copied()?;
    let rate = match (d[1] >> 3) & 3 {
        3 => base,
        2 => base / 2,
        0 => base / 4,
        _ => return None,
    };
    Some((rate, if d[3] >> 6 == 3 { 1 } else { 2 }))
}

/// RFC 9639 9.1.
fn flac_samples(d: &[u8]) -> u32 {
    if d.len() < 5 || d[0] != 0xFF || d[1] & 0xFE != 0xF8 {
        return 0;
    }
    let code = d[2] >> 4;
    // The coded number, UTF-8 style, one to seven bytes.
    let lead = d[4].leading_ones() as usize;
    let at = 4 + if lead == 0 { 1 } else { lead };
    match code {
        1 => 192,
        2..=5 => 576 << (code - 2),
        6 => d.get(at).map_or(0, |&b| u32::from(b) + 1),
        7 => d.get(at..at + 2).map_or(0, |b| u32::from(u16::from_be_bytes([b[0], b[1]])) + 1),
        8..=15 => 256 << (code - 8),
        _ => 0,
    }
}

#[cfg(test)]
mod tests;
