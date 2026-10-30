//! `MkvMuxN`, the multi-track Matroska muxer. One static sink pad per track, the linked pads in
//! order are tracks 1.., frames interleaved in global pts order across pads into one byte stream.

use std::collections::VecDeque;

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    AlignBy, Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, FixedFormat, OfferDesc, Value, ValueDesc};
use profluens_core::id::{FormatId, SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::config::ParamSets;
use crate::element::{BuiltVideo, Framing, MkvMux};
use crate::error::{MkvError, MuxError, SinkPads};
use crate::writer::{MatroskaWriter, MuxOut, TrackConfig};

/// The sink pad menu size, the most input tracks one `MkvMuxN` muxes.
pub const MAX_TRACKS: usize = 8;

/// Pre-header queue bounds, so a linked pad that never announces fails loudly.
pub const PRECONFIG_MAX_BYTES: usize = 64 << 20;
pub const PRECONFIG_MAX_BUFS: usize = 4096;

/// How far the others may lead an empty pad before it is no longer waited for, ns.
pub const LEAD_NS: u64 = 1_000_000_000;
/// Wall time a pad may go without delivering and still be waited for, ns.
pub const STALL_NS: u64 = 1_000_000_000;
/// Frames held across the pads past which no stalled pad is waited for, since what a
/// muxer holds may be what its upstream waits on.
pub const HELD_BUFS: usize = 48;
pub const HELD_BYTES: usize = 4 << 20;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

/// Declaring every field and value name is what interns them, `build_fixed` drops un-interned ones.
static MUXN_SAMPLE_VALUES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];
static MUXN_AUDIO_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc {
        field: "sample",
        allowed: ConstraintDesc::Set(&MUXN_SAMPLE_VALUES),
        preferred: None,
    },
];
static MUXN_AAC_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: "duration", allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: crate::codec::FIELD_DELAY, allowed: ConstraintDesc::Any, preferred: None },
];
static MUXN_VIDEO_FIELDS: [FieldDesc; 13] = {
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
static MUXN_SINK_OFFERS: [OfferDesc; 10] = [
    OfferDesc { family: "flac", fields: &MUXN_AUDIO_FIELDS },
    OfferDesc { family: "vp8", fields: &MUXN_VIDEO_FIELDS },
    OfferDesc { family: "vp9", fields: &MUXN_VIDEO_FIELDS },
    OfferDesc { family: "h264/avcc", fields: &MUXN_VIDEO_FIELDS },
    OfferDesc { family: "h265/hvcc", fields: &MUXN_VIDEO_FIELDS },
    OfferDesc { family: "h264/annexb", fields: &MUXN_VIDEO_FIELDS },
    OfferDesc { family: "h265/annexb", fields: &MUXN_VIDEO_FIELDS },
    OfferDesc { family: "av1", fields: &MUXN_VIDEO_FIELDS },
    OfferDesc { family: "aac", fields: &MUXN_AAC_FIELDS },
    OfferDesc { family: "opus", fields: &MUXN_AAC_FIELDS },
];

const fn sink_pad(name: &'static str) -> PadDesc {
    PadDesc { name, offers: &MUXN_SINK_OFFERS }
}

static MUXN_SINKS: [PadDesc; 8] = [
    sink_pad("sink_0"),
    sink_pad("sink_1"),
    sink_pad("sink_2"),
    sink_pad("sink_3"),
    sink_pad("sink_4"),
    sink_pad("sink_5"),
    sink_pad("sink_6"),
    sink_pad("sink_7"),
];
static MUXN_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

// Cold path, once at construction.
#[allow(clippy::disallowed_methods)]
static MUXN_DESC: ElementDesc = ElementDesc {
    name: "mkvmuxn",
    sinks: &MUXN_SINKS,
    srcs: &MUXN_SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::All { by: AlignBy::RunningTime },
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(MkvMuxN::new())),
    io: false,
    couplings: &[],
};

enum LaneState {
    AwaitCaps,
    FlacHead { head: Vec<u8>, scanned: usize },
    NalHead { codec_id: &'static str, width: u32, height: u32 },
    AacHead { rate: u32, channels: u32, delay_ns: u64 },
    Built { video: BuiltVideo, sets: Option<ParamSets> },
    OpusHead { channels: u32, delay_ns: u64 },
    Ready(TrackConfig),
}

struct Lane {
    pad: SinkPad,
    track: u64,
    state: LaneState,
    queue: VecDeque<(u64, Buffer)>,
    next_ts_ns: u64,
    frame_dur_ns: u64,
    colour: Option<crate::writer::ColourConfig>,
    gap: u64,
    last: Option<u64>,
    fed: Option<u64>,
    step: u64,
    framing: Framing,
}

impl Lane {
    fn alive(&self, now: u64) -> bool {
        self.fed.is_some_and(|t| now.saturating_sub(t) < STALL_NS)
    }

    fn known(&self) -> u64 {
        self.gap.max(self.last.unwrap_or(0))
    }

    /// Settled at enqueue, synthesised cadence kept ahead of real PTS when there is none.
    fn effective_ts(&mut self, buf: &Buffer) -> u64 {
        match buf.pts.nanos() {
            // Saturating, the PTS is untrusted.
            Some(t) => {
                self.next_ts_ns = t.saturating_add(self.frame_dur_ns);
                t
            }
            None => {
                let t = self.next_ts_ns;
                self.next_ts_ns = t.saturating_add(self.frame_dur_ns);
                t
            }
        }
    }
}

pub struct MkvMuxN {
    expect: Option<usize>,
    lanes: Vec<Lane>,
    writer: Option<MatroskaWriter>,
    header_done: bool,
    done: bool,
    duration_ns: u64,
    prebuf_bytes: usize,
    prebuf_bufs: usize,
    out: MuxOut,
    out_off: usize,
    out_fmt: FormatId,
    held_bufs: usize,
    held_bytes: usize,
    end_ns: u64,
    /// Frames the writer refused, lost from the file.
    lost: u64,
}

impl MkvMux {
    pub fn multi(n: usize) -> MkvMuxN {
        let mut m = MkvMuxN::new();
        m.expect = Some(n);
        m
    }
}

impl Default for MkvMuxN {
    fn default() -> Self {
        Self::new()
    }
}

impl MkvMuxN {
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

    // Cold path, construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self {
            expect: None,
            lanes: Vec::new(),
            writer: None,
            header_done: false,
            done: false,
            duration_ns: 0,
            prebuf_bytes: 0,
            prebuf_bufs: 0,
            out: MuxOut::new(),
            out_off: 0,
            out_fmt: FormatId(0), // probed at `start`
            held_bufs: 0,
            held_bytes: 0,
            end_ns: 0,
            lost: 0,
        }
    }

    pub fn tracks(&self) -> Vec<Option<&TrackConfig>> {
        self.lanes
            .iter()
            .map(|l| match &l.state {
                LaneState::Ready(cfg) => Some(cfg),
                _ => None,
            })
            .collect()
    }

    fn drain_out(&mut self, ctx: &mut Ctx) -> bool {
        crate::drain::drain(&mut self.out, &mut self.out_off, ctx, Self::OUT, self.out_fmt)
    }

    fn drain_out_exact(&mut self, ctx: &mut Ctx) {
        crate::drain::drain_exact(&mut self.out, &mut self.out_off, ctx, Self::OUT, self.out_fmt);
    }

    fn waiting_pads(&self) -> SinkPads {
        let mut pads = SinkPads::default();
        for l in self.lanes.iter().filter(|l| !matches!(l.state, LaneState::Ready(_))) {
            pads.insert(l.pad.0);
        }
        pads
    }

    fn accept(&mut self, ctx: &mut Ctx, lane_idx: usize, buf: Buffer) -> Result<(), MuxError> {
        if self.done {
            return Ok(()); // late buffer after finalize: dropped (matches `MkvMux`)
        }
        let track = self.lanes[lane_idx].track;
        let lane = &mut self.lanes[lane_idx];
        match &mut lane.state {
            LaneState::Ready(_) => {
                if !self.header_done {
                    self.prebuf_bytes += buf.memory.len();
                    self.prebuf_bufs += 1;
                    if self.prebuf_bytes > PRECONFIG_MAX_BYTES
                        || self.prebuf_bufs > PRECONFIG_MAX_BUFS
                    {
                        return Err(MuxError::PrebufferFull {
                            buffers: self.prebuf_bufs,
                            bytes: self.prebuf_bytes,
                            waiting: self.waiting_pads(),
                        });
                    }
                }
                self.enqueue(ctx, lane_idx, buf);
                Ok(())
            }
            LaneState::AwaitCaps => Err(MuxError::DataBeforeFormat),
            LaneState::Built { video, sets } => {
                let video = *video;
                let Some(tc) = video.absorb(sets, track, buf.memory.data())? else {
                    return Ok(());
                };
                Self::lane_ready(lane, tc);
                lane.framing = video.framing;
                self.enqueue(ctx, lane_idx, buf);
                Ok(())
            }
            LaneState::OpusHead { channels, delay_ns } => {
                let (channels, delay_ns) = (*channels, *delay_ns);
                let data = buf.memory.data();
                let led = data.len() >= 19 && &data[..8] == b"OpusHead";
                if !led && channels > 2 {
                    return Err(MuxError::OpusHeadMissing { channels });
                }
                // Cold path, once per lane.
                #[allow(clippy::disallowed_methods)]
                let head = if led { data.to_vec() } else { opus_head(channels, delay_ns) };
                let channels = u32::from(head[9]);
                let pre_skip = u64::from(u16::from_le_bytes([head[10], head[11]]));
                let mut tc = TrackConfig::audio(track, "A_OPUS", head, 48_000.0, channels, 0);
                // The decoder needs 80 ms ahead of a seek (Matroska codec mappings, A_OPUS).
                tc.codec_delay_ns =
                    if delay_ns != 0 { delay_ns } else { pre_skip * 1_000_000_000 / 48_000 };
                tc.seek_pre_roll_ns = 80_000_000;
                Self::lane_ready(lane, tc);
                if !led {
                    self.enqueue(ctx, lane_idx, buf);
                }
                Ok(())
            }
            LaneState::FlacHead { head, scanned } => {
                head.extend_from_slice(buf.memory.data());
                match MkvMux::flac_head_len(head, scanned)? {
                    None => Ok(()), // head still incomplete, keep absorbing
                    Some(len) => {
                        if len != head.len() {
                            return Err(MuxError::FlacHeadSplit {
                                head: len,
                                buffered: head.len(),
                            });
                        }
                        let head = std::mem::take(head);
                        let (rate, ch, bits) = MkvMux::flac_streaminfo_params(&head)?;
                        Self::lane_ready(lane, TrackConfig::flac(track, head, rate, ch, bits));
                        Ok(())
                    }
                }
            }
            LaneState::NalHead { codec_id, width, height } => {
                // The first buffer is the raw config record (RFC 9559 section 12).
                // configurationVersion 1 guards against sample data. Cold path,
                // once per lane.
                #[allow(clippy::disallowed_methods)]
                let head = buf.memory.data().to_vec();
                if head.first() != Some(&1) {
                    return Err(MuxError::ConfigRecord { version: head.first().copied() });
                }
                let (codec_id, w, h) = (*codec_id, *width, *height);
                let mut tc = TrackConfig::video(track, codec_id, head, w, h);
                if let Some(v) = tc.video.as_mut() {
                    v.colour = lane.colour;
                }
                Self::lane_ready(lane, tc);
                Ok(())
            }
            LaneState::AacHead { rate, channels, delay_ns } => {
                // The first buffer is the raw AudioSpecificConfig, >= 2 octets (ISO/IEC 14496-3
                // section 1.6.2.1). Cold path, once per lane.
                #[allow(clippy::disallowed_methods)]
                let head = buf.memory.data().to_vec();
                if head.len() < 2 {
                    return Err(MuxError::AacConfigShort { len: head.len() });
                }
                let (rate, channels) = (*rate, *channels);
                let mut tc = TrackConfig::audio(track, "A_AAC", head, rate as f64, channels, 0);
                // The stamps carry the delay, so the track declares it (RFC 9559 section
                // 5.1.4.1.25).
                tc.codec_delay_ns = *delay_ns;
                Self::lane_ready(lane, tc);
                Ok(())
            }
        }
    }

    fn enqueue(&mut self, ctx: &mut Ctx, lane_idx: usize, buf: Buffer) {
        let lane = &mut self.lanes[lane_idx];
        let ts = lane.effective_ts(&buf);
        let Buffer { memory, pts, dts, duration, flags, format, sync } = buf;
        let Some(memory) = lane.framing.block(ctx, memory) else {
            log!(&*ctx, Level::Trace, "block_empty", track = lane.track);
            return;
        };
        self.held_bufs += 1;
        self.held_bytes += memory.len();
        lane.queue.push_back((ts, Buffer { memory, pts, dts, duration, flags, format, sync }));
    }

    fn lane_ready(lane: &mut Lane, cfg: TrackConfig) {
        lane.frame_dur_ns = MkvMux::frame_duration_ns(&cfg);
        lane.state = LaneState::Ready(cfg);
    }

    /// The header goes once every lane is Ready or has closed short of it. A lane that ends
    /// before its caps or its head, an empty track or an AnnexB lane with no VCL, must not hold
    /// the others until the prebuffer fails.
    fn maybe_write_header(&mut self, ctx: &mut Ctx) {
        if self.header_done {
            return;
        }
        let ready = |l: &Lane| matches!(l.state, LaneState::Ready(_));
        if !self.lanes.iter().all(|l| ready(l) || ctx.is_pad_closed(l.pad)) {
            return;
        }
        let waiting = self.waiting_pads();
        if !waiting.is_empty() && self.lanes.len() > waiting.len() {
            let element = ctx.element();
            ctx.post(BusMessage::Warning {
                element,
                error: muxn_failed(ctx, MuxError::TracksDropped { pads: waiting }),
            });
        }
        self.build_writer();
    }

    /// The first video lane is the anchor whose keyframes open Clusters, else the first lane.
    fn build_writer(&mut self) {
        let mut configs: Vec<TrackConfig> = self
            .lanes
            .iter()
            .filter_map(|l| match &l.state {
                LaneState::Ready(cfg) => Some(cfg.clone()),
                _ => None,
            })
            .collect();
        if configs.is_empty() {
            return;
        }
        if let Some(anchor) = configs.iter().position(|c| c.video.is_some()) {
            let a = configs.remove(anchor);
            configs.insert(0, a);
        }
        let mut writer = MatroskaWriter::new(configs);
        // Cues always on, without them players bisect the Clusters to seek.
        writer.enable_cues();
        if self.duration_ns > 0 {
            writer.set_duration_ns(self.duration_ns);
        } else {
            writer.reserve_duration();
        }
        let _ = writer.write_header_scatter(&mut self.out);
        self.header_done = true;
        self.writer = Some(writer);
    }

    /// Feed queued frames to the writer in global timestamp order, ties to the earliest lane. An
    /// open pad with an empty queue may still deliver an earlier pts, so the merge waits for
    /// it.
    fn merge(&mut self, ctx: &mut Ctx, at_eos: bool) {
        if self.writer.is_none() {
            return; // header pending, frames stay queued, bounded by `accept`
        }
        let now = ctx.steady();
        loop {
            let mut best: Option<(usize, u64)> = None;
            let mut ahead = 0u64;
            for (i, lane) in self.lanes.iter().enumerate() {
                if let Some(&(ts, _)) = lane.queue.front()
                    && best.is_none_or(|(_, bts)| ts < bts)
                {
                    best = Some((i, ts));
                }
                ahead = ahead.max(lane.queue.back().map_or(0, |(ts, _)| *ts));
            }
            let pressed = self.held_bufs >= HELD_BUFS || self.held_bytes >= HELD_BYTES;
            // `None` waits for no pad. A `u64::MAX` sentinel would hold everything until the end.
            let mut floor: Option<u64> = None;
            if !at_eos {
                for lane in
                    self.lanes.iter().filter(|l| l.queue.is_empty() && !ctx.is_pad_closed(l.pad))
                {
                    let known = lane.known();
                    let near = !pressed && ahead <= known.saturating_add(LEAD_NS);
                    if near || lane.alive(now) {
                        floor = Some(floor.map_or(known, |f| f.min(known)));
                    }
                }
            }
            let Some((i, ts)) = best.filter(|(_, ts)| floor.is_none_or(|f| *ts < f)) else {
                return;
            };
            let (_, buf) = self.lanes[i].queue.pop_front().expect("head just seen");
            if let Some(prev) = self.lanes[i].last.filter(|p| ts > *p) {
                self.lanes[i].step = ts - prev;
            }
            self.lanes[i].last = Some(ts);
            self.held_bufs -= 1;
            self.held_bytes -= buf.memory.len();
            let keyframe = !buf.flags.contains(BufferFlags::DELTA);
            let track = self.lanes[i].track;
            let dur = buf.duration.nanos().unwrap_or(self.lanes[i].step);
            self.end_ns = self.end_ns.max(ts.saturating_add(dur));
            let writer = self.writer.as_mut().expect("writer present");
            // Fails only for a track the writer was not built with, a lane left out of the header.
            if writer.write_frame_scatter(&mut self.out, track, ts, buf.memory, keyframe).is_err() {
                if self.lost == 0 {
                    log!(&*ctx, Level::Error, "frame_lost", track = track, ts = ts);
                }
                self.lost += 1;
            }
            if !at_eos && !self.drain_out(ctx) {
                return; // pool dry mid-drain, resume next pass
            }
        }
    }

    fn finish_stream(&mut self, ctx: &mut Ctx) {
        self.done = true;
        // A second call must not rebuild the taken writer and emit a second header.
        if self.writer.is_none() && !self.header_done {
            let waiting = self.waiting_pads();
            if !waiting.is_empty() && self.lanes.len() > waiting.len() {
                let element = ctx.element();
                ctx.post(BusMessage::Warning {
                    element,
                    error: muxn_failed(ctx, MuxError::TracksDropped { pads: waiting }),
                });
            }
            self.build_writer(); // from the Ready lanes only (no-op when none)
        }
        self.merge(ctx, true);
        let (seek, duration) = match self.writer.take() {
            Some(mut writer) => {
                let duration = writer.duration_patch(self.end_ns);
                let tracks = self.lanes.len();
                let ready = self.lanes.iter().filter(|l| matches!(l.state, LaneState::Ready(_)));
                let ready = ready.count();
                let end = Timestamp::from_nanos(self.end_ns);
                log!(&*ctx, Level::Info, "eos", tracks = tracks, muxed = ready, end = end);
                (writer.finalize_scatter(&mut self.out), duration)
            }
            None => (None, None),
        };
        if self.lost > 0 {
            log!(&*ctx, Level::Error, "frames_lost", count = std::mem::take(&mut self.lost));
        }
        self.drain_out_exact(ctx);
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

    fn waited(&self, ctx: &Ctx, now: u64) -> Option<u64> {
        if !self.header_done || self.done {
            return None;
        }
        let waited = self
            .lanes
            .iter()
            .filter(|l| l.queue.is_empty() && !ctx.is_pad_closed(l.pad) && l.alive(now));
        waited.map(Lane::known).min()
    }

    fn leads(&self, i: usize, waited: Option<u64>) -> bool {
        let back = self.lanes[i].queue.back().map(|(ts, _)| *ts);
        waited.zip(back).is_some_and(|(w, b)| b > w.saturating_add(LEAD_NS))
    }

    fn wake_at_stall(&self, ctx: &mut Ctx, now: u64) {
        let waited = self
            .lanes
            .iter()
            .filter(|l| l.queue.is_empty() && !ctx.is_pad_closed(l.pad) && l.alive(now));
        let left = waited.filter_map(|l| l.fed).map(|t| STALL_NS - now.saturating_sub(t)).min();
        if let Some(ns) = left {
            ctx.wake_within(std::time::Duration::from_nanos(ns.max(1_000_000)));
        }
    }

    // Cold path, once per lane.
    #[allow(clippy::disallowed_methods)]
    fn on_format_change(&mut self, ctx: &mut Ctx, f: &FixedFormat) -> Result<(), Error> {
        let matches = |l: &Lane| ctx.negotiated_sink(l.pad).is_some_and(|g| fixed_eq(g, f));
        let by_pad = ctx.event_pad().and_then(|p| self.lanes.iter().position(|l| l.pad == p));
        let by_pad = by_pad.filter(|&i| matches!(self.lanes[i].state, LaneState::AwaitCaps));
        let Some(idx) = by_pad.or_else(|| {
            self.lanes.iter().position(|l| matches!(l.state, LaneState::AwaitCaps) && matches(l))
        }) else {
            // A repeat of a lane's own format is harmless, a flush or seek re-announces it.
            // Anything else is unattributable, guessing a pad could weld the wrong
            // CodecPrivate to a track, and a changed format past the header cannot be muxed.
            if self.lanes.iter().any(|l| !matches!(l.state, LaneState::AwaitCaps) && matches(l)) {
                return Ok(());
            }
            if self.header_done {
                return Err(muxn_failed(ctx, MuxError::FormatChangeAfterHeader));
            }
            return Err(muxn_failed(ctx, MuxError::FormatChangeUnattributed));
        };

        let family = ctx.family_name(f.family).unwrap_or("?").to_owned();
        let get = |name: &str| {
            ctx.field_id(name).and_then(|id| f.get(id)).and_then(|v| match v {
                Value::Int(n) if n > 0 => Some(n as u32),
                _ => None,
            })
        };
        let duration_ns = ctx.field_id("duration").and_then(|id| f.get(id)).and_then(|v| match v {
            Value::Int(n) if n > 0 => Some(n as u64),
            _ => None,
        });
        if let Some(d) = duration_ns {
            self.duration_ns = self.duration_ns.max(d);
        }

        let colour = crate::color_map::colour_from_format(ctx, f);
        let track = self.lanes[idx].track;
        let lane = &mut self.lanes[idx];
        lane.colour = colour.or(lane.colour);
        match family.as_str() {
            "flac" => lane.state = LaneState::FlacHead { head: Vec::new(), scanned: 0 },
            "vp8" | "vp9" => {
                let (Some(w), Some(h)) = (get("width"), get("height")) else {
                    return Err(muxn_failed(ctx, MuxError::VideoSizeMissing));
                };
                let id = if family == "vp8" { "V_VP8" } else { "V_VP9" };
                let mut tc = TrackConfig::video(track, id, Vec::new(), w, h);
                if let Some(v) = tc.video.as_mut() {
                    v.colour = lane.colour;
                }
                Self::lane_ready(lane, tc);
            }
            "h264/avcc" | "h265/hvcc" => {
                let (Some(w), Some(h)) = (get("width"), get("height")) else {
                    return Err(muxn_failed(ctx, MuxError::VideoSizeMissing));
                };
                let codec_id =
                    if family == "h264/avcc" { "V_MPEG4/ISO/AVC" } else { "V_MPEGH/ISO/HEVC" };
                lane.state = LaneState::NalHead { codec_id, width: w, height: h };
            }
            fam if Framing::of(fam).is_some() => {
                let framing = Framing::of(fam).expect("matched");
                let (width, height) = (get("width").unwrap_or(0), get("height").unwrap_or(0));
                let video = BuiltVideo { framing, width, height, duration_ns, colour: lane.colour };
                lane.state = LaneState::Built { video, sets: None };
            }
            "opus" => {
                let delay = ctx.field_id(crate::codec::FIELD_DELAY).and_then(|id| f.get(id));
                let delay_ns = match delay {
                    Some(Value::Int(ns)) if ns > 0 => ns as u64,
                    _ => 0,
                };
                lane.state =
                    LaneState::OpusHead { channels: get("channels").unwrap_or(2), delay_ns };
            }
            "aac" => {
                let (Some(rate), Some(channels)) = (get("rate"), get("channels")) else {
                    return Err(muxn_failed(ctx, MuxError::AacParamsMissing));
                };
                let delay = ctx.field_id(crate::codec::FIELD_DELAY).and_then(|id| f.get(id));
                let delay_ns = match delay {
                    Some(Value::Int(ns)) if ns > 0 => ns as u64,
                    _ => 0,
                };
                lane.state = LaneState::AacHead { rate, channels, delay_ns };
            }
            other => {
                return Err(muxn_failed(ctx, MuxError::UnsupportedFamily { family: other.into() }));
            }
        }
        Ok(())
    }
}

// Cold path, once per lane.
#[allow(clippy::disallowed_methods)]
fn opus_head(channels: u32, delay_ns: u64) -> Vec<u8> {
    let pre_skip =
        (delay_ns.saturating_mul(48_000) / 1_000_000_000).min(u64::from(u16::MAX)) as u16;
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(channels.clamp(1, 2) as u8);
    h.extend_from_slice(&pre_skip.to_le_bytes());
    h.extend_from_slice(&48_000u32.to_le_bytes());
    h.extend_from_slice(&[0, 0, 0]);
    h
}

fn muxn_failed(ctx: &Ctx, e: MuxError) -> Error {
    Error::element_failed(ctx.element(), MkvError::MuxN(e))
}

/// Structural [`FixedFormat`] equality, order-insensitive.
pub(crate) fn fixed_eq(a: &FixedFormat, b: &FixedFormat) -> bool {
    a.family == b.family
        && a.len() == b.len()
        && a.fields().iter().all(|(id, v)| b.get(*id) == Some(*v))
}

impl Element for MkvMuxN {
    fn desc(&self) -> &'static ElementDesc {
        &MUXN_DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.lanes.clear();
        for i in 0..MAX_TRACKS {
            let pad = Self::INS[i];
            if ctx.negotiated_sink(pad).is_some() {
                self.lanes.push(Lane {
                    pad,
                    track: self.lanes.len() as u64 + 1,
                    state: LaneState::AwaitCaps,
                    queue: VecDeque::new(),
                    next_ts_ns: 0,
                    gap: 0,
                    frame_dur_ns: 0,
                    colour: None,
                    last: None,
                    fed: None,
                    step: 0,
                    framing: Framing::Verbatim,
                });
            }
        }
        if self.lanes.is_empty() {
            return Err(muxn_failed(ctx, MuxError::NoSinkLinked));
        }
        if let Some(n) = self.expect
            && self.lanes.len() != n
        {
            return Err(muxn_failed(
                ctx,
                MuxError::LinkedCount { expected: n, linked: self.lanes.len() },
            ));
        }
        self.writer = None;
        self.header_done = false;
        self.done = false;
        self.duration_ns = 0;
        self.prebuf_bytes = 0;
        self.prebuf_bufs = 0;
        self.held_bufs = 0;
        self.held_bytes = 0;
        (self.end_ns, self.lost) = (0, 0);
        self.out.clear();
        self.out_off = 0;
        self.out_fmt = ctx.out_format();
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.drain_out(ctx) {
            return Ok(Flow::Ok);
        }
        // With one linked pad batches come through `inputs`, with several through `take_input_on`.
        if self.lanes.len() == 1 {
            while let Some(buf) = inputs.pop() {
                self.accept(ctx, 0, buf).map_err(|e| muxn_failed(ctx, e))?;
            }
        }
        let now = ctx.steady();
        let waited = self.waited(ctx, now);
        for i in 0..self.lanes.len() {
            if self.leads(i, waited) {
                continue;
            }
            let pad = self.lanes[i].pad;
            let mut batch = ctx.take_input_on(pad);
            if !batch.is_empty() {
                self.lanes[i].fed = Some(now);
            }
            while let Some(buf) = batch.pop_front() {
                self.accept(ctx, i, buf).map_err(|e| muxn_failed(ctx, e))?;
            }
            ctx.recycle_input(batch);
        }
        let was = self.header_done;
        self.maybe_write_header(ctx);
        if !was && self.header_done {
            log!(&*ctx, Level::Info, "header", tracks = self.lanes.len());
        }
        self.merge(ctx, false);
        self.wake_at_stall(ctx, now);
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => self.finish_stream(ctx),
            Event::FormatChange(f) => {
                if !self.done {
                    self.on_format_change(ctx, f)?;
                }
            }
            Event::Gap { until } => {
                let pad = ctx.event_pad();
                let now = ctx.steady();
                if let Some(lane) = self.lanes.iter_mut().find(|l| Some(l.pad) == pad) {
                    lane.gap = lane.gap.max(until.nanos().unwrap_or(0));
                    lane.fed = Some(now);
                }
            }
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
