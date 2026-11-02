//! `OggMuxN`, one logical bitstream per sink pad grouped into one Ogg stream (RFC 3533 section 4).

use std::collections::VecDeque;

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    AlignBy, Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;

use crate::element::DEFAULT_SERIAL;
use crate::ident::{
    Mapping, flac_frame_samples, opus_packet_samples, parse_speex_head, parse_vorbis_ident,
};
use crate::writer::OggWriter;

/// The most inputs one `OggMuxN` muxes.
pub const MAX_STREAMS: usize = 8;
/// How far the other pads may lead a pad with nothing queued before it is no longer waited for,
/// nanoseconds.
pub const LEAD_NS: u64 = 1_000_000_000;
/// Packets held across the pads past which no pad is waited for.
pub const HELD_BUFS: usize = 48;
/// Packets held before the header pages past which a pad that said nothing is dropped.
pub const HEAD_WAIT_BUFS: usize = 4096;
/// The time a data page spans at most before it is put out, nanoseconds.
pub const PAGE_NS: u64 = 500_000_000;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

const fn sink_pad(name: &'static str) -> PadDesc {
    PadDesc { name, offers: &OFFERS }
}

static SINKS: [PadDesc; MAX_STREAMS] = [
    sink_pad("sink_0"),
    sink_pad("sink_1"),
    sink_pad("sink_2"),
    sink_pad("sink_3"),
    sink_pad("sink_4"),
    sink_pad("sink_5"),
    sink_pad("sink_6"),
    sink_pad("sink_7"),
];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "oggmuxn",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::All { by: AlignBy::RunningTime },
    latency: LatencyDesc::NONE,
    // Cold, the make_default closure boxes one element at construction.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(OggMuxN::new())),
    io: false,
    couplings: &[],
};

/// What the muxer fails with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuxError {
    NoSinkLinked,
    Silent { pad: u32 },
}

impl std::fmt::Display for MuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MuxError::NoSinkLinked => f.write_str("oggmuxn: no sink pad is linked"),
            MuxError::Silent { pad } => {
                write!(f, "oggmuxn: sink_{pad} sent no packet before the bos pages, dropped")
            }
        }
    }
}

impl std::error::Error for MuxError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Heads,
    Data,
    Done,
}

struct Lane {
    pad: SinkPad,
    writer: OggWriter,
    mapping: Option<Mapping>,
    heads_left: u32,
    heads: Vec<Vec<u8>>,
    rate: u32,
    granule: u64,
    queue: VecDeque<(u64, u64, Buffer)>,
    gap: u64,
    last: Option<u64>,
    page_from: Option<u64>,
    over: bool,
}

impl Lane {
    // Cold, a handful of header packets per stream.
    #[allow(clippy::disallowed_methods)]
    fn head(&mut self, data: &[u8]) -> bool {
        let Some(mapping) = self.mapping else {
            let mapping = Mapping::identify(data);
            self.mapping = Some(mapping);
            self.heads_left = match (mapping, data.get(7..9)) {
                (Mapping::Flac, Some([0, 0])) => u32::MAX,
                _ => mapping.header_packets(data).saturating_sub(1),
            };
            self.rate = rate_of(mapping, data);
            self.heads.push(data.to_vec());
            return true;
        };
        let frame = mapping == Mapping::Flac && flac_frame_samples(data).is_some();
        if self.heads_left == 0 || frame {
            self.heads_left = 0;
            return false;
        }
        self.heads_left -= 1;
        self.heads.push(data.to_vec());
        true
    }

    fn heads_done(&self) -> bool {
        self.mapping.is_some() && self.heads_left == 0
    }

    fn stamp(&mut self, buf: &Buffer) -> (u64, u64) {
        let data = buf.memory.data();
        let samples = match self.mapping {
            Some(Mapping::Opus) => opus_packet_samples(data),
            Some(Mapping::Flac) => flac_frame_samples(data),
            _ => None,
        };
        if let (Some(n), true) = (samples, self.rate > 0) {
            let at = ns_of(self.granule, self.rate);
            self.granule += u64::from(n);
            return (at, self.granule);
        }
        let at = buf.pts.nanos().unwrap_or(self.last.unwrap_or(0));
        if let (Some(t), true) = (buf.pts.nanos(), self.rate > 0) {
            let end = t.saturating_add(buf.duration.nanos().unwrap_or(0));
            self.granule =
                self.granule.max((u128::from(end) * u128::from(self.rate) / 1_000_000_000) as u64);
        }
        (at, self.granule)
    }
}

fn rate_of(mapping: Mapping, ident: &[u8]) -> u32 {
    match mapping {
        Mapping::Opus => crate::duration::OPUS_GRANULE_RATE,
        Mapping::Vorbis => parse_vorbis_ident(ident).map_or(0, |h| h.sample_rate),
        Mapping::Speex => parse_speex_head(ident).map_or(0, |h| h.sample_rate),
        // STREAMINFO's 20 bit rate from bit 80 of the body (RFC 9639 section 8.2).
        Mapping::Flac => ident
            .get(27..30)
            .map_or(0, |b| u32::from(b[0]) << 12 | u32::from(b[1]) << 4 | u32::from(b[2]) >> 4),
        Mapping::Theora | Mapping::Skeleton | Mapping::Unknown => 0,
    }
}

fn ns_of(granule: u64, rate: u32) -> u64 {
    (u128::from(granule) * 1_000_000_000 / u128::from(rate)) as u64
}

/// Muxes each linked sink pad into its own logical bitstream, grouped into one Ogg stream.
pub struct OggMuxN {
    base: u32,
    lanes: Vec<Lane>,
    phase: Phase,
    held: usize,
    scratch: Vec<u8>,
}

impl Default for OggMuxN {
    fn default() -> Self {
        Self::with_serial(DEFAULT_SERIAL)
    }
}

impl OggMuxN {
    pub const INS: [SinkPad; MAX_STREAMS] = [
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

    pub fn new() -> Self {
        Self::default()
    }

    // Cold, one-time constructor.
    #[allow(clippy::disallowed_methods)]
    pub fn with_serial(base: u32) -> Self {
        Self { base, lanes: Vec::new(), phase: Phase::Heads, held: 0, scratch: Vec::new() }
    }

    fn emit(&mut self, ctx: &mut Ctx) {
        let mut off = 0;
        while off < self.scratch.len() {
            let mut buf = ctx.alloc();
            let n = buf.memory.capacity().min(self.scratch.len() - off);
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.scratch[off..off + n]);
            buf.memory.set_len(n);
            ctx.out(Self::OUT).push(buf);
            off += n;
        }
        self.scratch.clear();
    }

    fn accept(&mut self, i: usize, buf: Buffer) {
        let lane = &mut self.lanes[i];
        if lane.over {
            return;
        }
        if lane.head(buf.memory.data()) {
            if self.phase == Phase::Data {
                let _ =
                    lane.writer.write_packet(&mut self.scratch, buf.memory.data(), lane.granule);
            }
            return;
        }
        let (at, granule) = lane.stamp(&buf);
        lane.queue.push_back((at, granule, buf));
        self.held += 1;
    }

    fn heads(&mut self, ctx: &mut Ctx, at_eos: bool) {
        if self.phase != Phase::Heads {
            return;
        }
        let pressed = at_eos || self.held >= HEAD_WAIT_BUFS;
        let waiting = |l: &Lane| !l.over && !l.heads_done();
        let closed_silent = |l: &Lane| ctx.is_pad_closed(l.pad) && l.mapping.is_none();
        if self.lanes.iter().any(|l| waiting(l) && !closed_silent(l)) && !pressed {
            return;
        }
        for l in &mut self.lanes {
            if l.mapping.is_none() && !l.over {
                l.over = true;
                let element = ctx.element();
                let silent = MuxError::Silent { pad: l.pad.0 };
                ctx.post(BusMessage::Warning {
                    element,
                    error: Error::element_failed(element, silent),
                });
            }
        }
        for l in self.lanes.iter_mut().filter(|l| !l.over) {
            let mapping = l.mapping.unwrap_or(Mapping::Unknown);
            let (pad, heads) = (l.pad.0, l.heads.len());
            if mapping == Mapping::Unknown {
                log!(&*ctx, Level::Warn, "lane_unknown", pad = pad, heads = heads);
            }
            let (family, rate) = (mapping.family(), l.rate);
            log!(
                &*ctx,
                Level::Info,
                "lane",
                pad = pad,
                family = family,
                rate = rate,
                heads = heads
            );
            let _ = l.writer.write_packet(&mut self.scratch, &l.heads[0], 0);
            l.writer.flush(&mut self.scratch);
        }
        for l in self.lanes.iter_mut().filter(|l| !l.over) {
            for h in &l.heads[1..] {
                let _ = l.writer.write_packet(&mut self.scratch, h, 0);
            }
            l.writer.flush(&mut self.scratch);
            drop(std::mem::take(&mut l.heads));
        }
        self.phase = Phase::Data;
        self.emit(ctx);
    }

    fn merge(&mut self, ctx: &mut Ctx, at_eos: bool) {
        if self.phase != Phase::Data {
            return;
        }
        loop {
            let mut best: Option<(usize, u64)> = None;
            let mut ahead = 0u64;
            for (i, l) in self.lanes.iter().enumerate() {
                if let Some(&(at, _, _)) = l.queue.front()
                    && best.is_none_or(|(_, b)| at < b)
                {
                    best = Some((i, at));
                }
                ahead = ahead.max(l.queue.back().map_or(0, |q| q.0));
            }
            let mut floor: Option<u64> = None;
            if !at_eos && self.held < HELD_BUFS {
                for l in self
                    .lanes
                    .iter()
                    .filter(|l| !l.over && l.queue.is_empty() && !ctx.is_pad_closed(l.pad))
                {
                    let known = l.gap.max(l.last.unwrap_or(0));
                    if ahead <= known.saturating_add(LEAD_NS) {
                        floor = Some(floor.map_or(known, |f| f.min(known)));
                    }
                }
            }
            let Some((i, at)) = best.filter(|(_, at)| floor.is_none_or(|f| *at < f)) else {
                break;
            };
            let l = &mut self.lanes[i];
            let (_, granule, buf) = l.queue.pop_front().expect("head just seen");
            self.held -= 1;
            l.last = Some(at);
            let _ = l.writer.write_packet(&mut self.scratch, buf.memory.data(), granule);
            let from = *l.page_from.get_or_insert(at);
            if at.saturating_sub(from) >= PAGE_NS {
                l.writer.flush(&mut self.scratch);
                l.page_from = None;
            }
        }
        for l in self.lanes.iter_mut().filter(|l| !l.over && l.queue.is_empty()) {
            if at_eos || ctx.is_pad_closed(l.pad) {
                l.writer.finish(&mut self.scratch);
                l.over = true;
            }
        }
        if at_eos {
            self.phase = Phase::Done;
        }
        self.emit(ctx);
    }
}

impl Element for OggMuxN {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    // Cold, once per run, a lane per linked pad.
    #[allow(clippy::disallowed_methods)]
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.lanes.clear();
        for pad in Self::INS {
            if ctx.negotiated_sink(pad).is_none() {
                continue;
            }
            self.lanes.push(Lane {
                pad,
                writer: OggWriter::new(self.base.wrapping_add(pad.0)),
                mapping: None,
                heads_left: 0,
                heads: Vec::new(),
                rate: 0,
                granule: 0,
                queue: VecDeque::new(),
                gap: 0,
                last: None,
                page_from: None,
                over: false,
            });
        }
        if self.lanes.is_empty() {
            return Err(Error::element_failed(ctx.element(), MuxError::NoSinkLinked));
        }
        (self.phase, self.held) = (Phase::Heads, 0);
        self.scratch.clear();
        log!(&*ctx, Level::Debug, "lanes", count = self.lanes.len());
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.phase == Phase::Done {
            return Ok(Flow::Ok);
        }
        if self.lanes.len() == 1 {
            while let Some(buf) = inputs.pop() {
                self.accept(0, buf);
            }
        }
        for i in 0..self.lanes.len() {
            let mut batch = ctx.take_input_on(self.lanes[i].pad);
            while let Some(buf) = batch.pop_front() {
                self.accept(i, buf);
            }
            ctx.recycle_input(batch);
        }
        self.heads(ctx, false);
        self.merge(ctx, false);
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos if self.phase != Phase::Done => {
                self.heads(ctx, true);
                self.merge(ctx, true);
                let live = self.lanes.iter().filter(|l| l.mapping.is_some()).count();
                log!(&*ctx, Level::Info, "eos", lanes = self.lanes.len(), muxed = live);
            }
            Event::Gap { until } => {
                let pad = ctx.event_pad();
                if let Some(l) = self.lanes.iter_mut().find(|l| Some(l.pad) == pad) {
                    l.gap = l.gap.max(until.nanos().unwrap_or(0));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        if self.phase != Phase::Done && !self.lanes.is_empty() {
            self.heads(ctx, true);
            self.merge(ctx, true);
        }
    }
}
