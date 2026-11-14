//! `psdemux`, the program stream demuxer element over the sans-IO [`crate::demux`]. Video leaves
//! as access units, audio as PES payloads its decoder frames, DVD LPCM as `audio/raw` and a DVD
//! subpicture as one whole unit. Pushed it reads on from where a seek's byte lands, pulled it
//! bisects the packs for the target first.
//! Time is one line from the earliest stamp of the head on. A clock that restarts or jumps (SCR
//! going back or a step past 10 s, as at the join of concatenated VOBs) is made continuous, the
//! units behind it going on from the last one out, video from its next key. A seek takes the
//! file as one time base, so behind a restart it lands by the restarted clock.

use std::collections::VecDeque;
use std::fmt;

use pf_mpegts::pes::PH_SCRAMBLED;
use pf_mpegts::{Span, Spans, Step as Stepped, Unwrapper, congruent};
use pf_tsdemux::index::{Grown, Key, Placed};
use profluens_audio::{FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, Disposition, SeekTarget, Stream, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;
use profluens_core::pull::{Fed, Feed};
use profluens_core::rate;
use profluens_core::time::Timestamp;

use crate::demux::{self, Demux, Unit};
use crate::es::{Au, Framer, Spu, Stamp};
use crate::lpcm::{self, Lpcm};
use crate::pack::PRIVATE_1;
use crate::seek::{self, Base, Bisect, Landing, Line, Rate, Step};
use crate::streams::{self, Codec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsError {
    NoStreams,
}

impl fmt::Display for PsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PsError::NoStreams => {
                f.write_str("psdemux: no stream it reads in the bytes the element was made from")
            }
        }
    }
}

impl std::error::Error for PsError {}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SAMPLES: [ValueDesc; 2] = [ValueDesc::Id("s16"), ValueDesc::Id("s24")];
static PCM_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_SAMPLE, allowed: ConstraintDesc::Set(&SAMPLES), preferred: None },
];

pub(crate) static SRC_OFFERS: [OfferDesc; 11] = [
    OfferDesc::any("bytes"),
    OfferDesc::any("mpeg1"),
    OfferDesc::any("mpeg2"),
    OfferDesc::any("mpeg4/asp"),
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc::any("mp3"),
    OfferDesc::any("ac3"),
    OfferDesc::any("dts"),
    OfferDesc { family: profluens_audio::FAMILY, fields: &PCM_FIELDS },
    OfferDesc::any("subtitle/vobsub"),
];

/// Every family the src pads offer, `bytes` aside.
pub static EMITS: &[&str] = &[
    "mpeg1",
    "mpeg2",
    "mpeg4/asp",
    "h264/annexb",
    "h265/annexb",
    "mp3",
    "ac3",
    "dts",
    profluens_audio::FAMILY,
    "subtitle/vobsub",
];

static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];

pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "psdemux",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// Bytes pushed into the demuxer at a time, so output backs up before the input does.
const PIECE: usize = 64 << 10;
const WINDOW: u32 = 64 << 10;
const WINDOW_DEPTH: u32 = 2;
/// The end of a file read for its last stamps, some thirty DVD packs.
pub const TAIL_LEN: usize = 64 << 10;
const TAIL_TAG: u32 = 1;
const PROBE_TAG: u32 = 2;
/// Slack below a file's first stamp for a stamp behind a seek, streams start within 1 s.
const ORIGIN_SLACK: i64 = 10 * 90_000;
/// How far ahead of a target a bisection lands, the mux delay (H.222.0 2.7.9) and a few GOPs, as
/// FFmpeg's muxer stamps some keys only, 2.4 s apart in a 0.5 s GOP.
const SEEK_BACK: i64 = 5 * 90_000;
const AU_RESERVE: usize = 256 << 10;
/// Past this a video unit is dropped, a 4:2:2 HD I-picture is well below.
const AU_MAX: usize = 8 << 20;
const STREAMS_MAX: usize = 32;
const SNIFF: usize = 4096;
const PCM_OUT: usize = 0x1_0000 / 5 * 6 + 2 * lpcm::BLOCK_MAX;
/// An LPCM stamp this far behind the sample count, or this far past the widest packet ahead of
/// it, starts the count over. Ticks round to 11 us.
const COUNT_SLACK_NS: u64 = 1_000_000;

fn ticks_ns(ticks: i64) -> i64 {
    (i128::from(ticks) * 100_000 / 9).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

pub struct Seen {
    pub key: u16,
    pub first: Option<i64>,
    pub sniff: Vec<u8>,
    pub lpcm: Option<Lpcm>,
}

pub struct Head {
    pub mpeg2: bool,
    pub psm: Box<[u8; 256]>,
    pub seen: Vec<Seen>,
    /// The first decode time and the earliest, in ticks.
    pub opening: Option<(i64, i64)>,
    /// Units of streams past `STREAMS_MAX`, left out.
    pub capped: u32,
    /// The stream whose stamps tell the time bases, the first video else the first audio.
    pub key: Option<u16>,
    pub line: Line,
    /// The pack clock against the bytes.
    pub rate: Rate,
}

/// Video, then audio, tells the time bases of a file.
fn line_class(key: u16) -> Option<u8> {
    Some(streams::rank(key).0).filter(|class| *class < 2)
}

#[allow(clippy::disallowed_methods)] // once per element
pub fn scan_head(head: &[u8]) -> Head {
    let mut demux = Demux::new();
    let mut out = Head {
        mpeg2: true,
        psm: Box::new([0; 256]),
        seen: Vec::new(),
        opening: None,
        capped: 0,
        key: None,
        line: Line::default(),
        rate: Rate::default(),
    };
    let (mut clock, mut packed) = (Unwrapper::for_timestamps(), false);
    let mut sink = |event: demux::Event<'_>| match event {
        demux::Event::Pack { pack, offset } => {
            out.rate.note(offset, pack.scr as i64);
            if !packed {
                (packed, out.mpeg2) = (true, pack.mpeg2);
            }
        }
        demux::Event::Psm(psm) => out.psm.copy_from_slice(&psm.types),
        demux::Event::Unit(u) => {
            let class = line_class(u.key);
            let ahead = |key: u16| class < line_class(key);
            if u.pts.is_some() && class.is_some() && out.key.is_none_or(ahead) {
                (out.key, out.line) = (Some(u.key), Line::default());
            }
            if let (Some(pts), true) = (u.pts, out.key == Some(u.key)) {
                out.line.note(u.dts.unwrap_or(pts), pts);
            }
            let at = match out.seen.iter().position(|s| s.key == u.key) {
                Some(at) => at,
                None if out.seen.len() < STREAMS_MAX => {
                    let lpcm = u.header.get(4..).and_then(|h| Lpcm::parse(h).ok());
                    out.seen.push(Seen { key: u.key, first: None, sniff: Vec::new(), lpcm });
                    out.seen.len() - 1
                }
                None => {
                    out.capped += 1;
                    return;
                }
            };
            let seen = &mut out.seen[at];
            // video for its codec, audio for an opener's rate and channels
            if !(0xBD..=0xBF).contains(&u.stream_id()) || u.stream_id() == PRIVATE_1 {
                let take = SNIFF.saturating_sub(seen.sniff.len()).min(u.payload.len());
                seen.sniff.extend_from_slice(&u.payload[..take]);
            }
            if let (None, Some(raw)) = (seen.first, u.dts.or(u.pts)) {
                let ticks = clock.unwrap(raw).ticks;
                seen.first = Some(ticks);
                let (first, least) = out.opening.unwrap_or((ticks, ticks));
                out.opening = Some((first, least.min(ticks)));
            }
        }
        _ => {}
    };
    demux.push(head, &mut sink);
    demux.flush(&mut sink);
    out
}

/// How long a file lasts from its head's `line` of stream `key` and pack `rate`, and its last
/// bytes `tail`, ticks from `least`, the head's earliest decode time. `len` the file's length
/// when known. `None` when the stamps do not tell, a clock set back between head and tail.
pub fn lasts(
    head: (Option<u16>, &Line, Rate),
    least: i64,
    tail: &[u8],
    len: Option<u64>,
) -> Option<i64> {
    let (key, line, mut rate) = head;
    let mut demux = Demux::new();
    let mut end = Line::default();
    rate.restart();
    let mut sink = |event: demux::Event<'_>| match event {
        demux::Event::Pack { pack, offset } => rate.note(offset, pack.scr as i64),
        demux::Event::Unit(Unit { pts: Some(pts), dts, key: k, .. }) if Some(k) == key => {
            end.note(dts.unwrap_or(pts), pts)
        }
        _ => {}
    };
    demux.push(tail, &mut sink);
    demux.flush(&mut sink);
    // a file's clock starts near 0, it reads as wrapping when set back
    seek::extent(line, &end, least, len.and_then(|len| rate.lasts(len)), false)
}

/// The last stamp in `tail`, the end of a file, in ticks on the line of `base`.
#[allow(clippy::disallowed_methods)] // once per element
pub fn last_ticks(tail: &[u8], base: Base) -> Option<i64> {
    let mut demux = Demux::new();
    let mut last: Option<i64> = None;
    let mut sink = |event: demux::Event<'_>| {
        if let demux::Event::Unit(Unit { pts: Some(pts), .. }) = event {
            let ticks = base.ticks(pts);
            last = Some(last.map_or(ticks, |l| l.max(ticks)));
        }
    };
    demux.push(tail, &mut sink);
    demux.flush(&mut sink);
    last
}

struct Lane {
    key: u16,
    family: &'static str,
    kind: StreamKind,
    stream: u32,
    pad: Option<SrcPad>,
    started: bool,
    need_key: bool,
    broke: bool,
    lpcm: Option<Lpcm>,
    /// Last decode time in ticks, and the stretch it was read in.
    last_dts: (u32, i64),
    /// The offset of the last unit taken in, and behind a restart for another track the one
    /// units read again are dropped up to.
    read: Option<u64>,
    resume: Option<u64>,
}

struct Pcm {
    carry: [u8; lpcm::BLOCK_MAX],
    held: usize,
    out: Vec<u8>,
    count: Count,
}

/// An LPCM stream's sample count, which stamps its units. A muxer stamps the first frame that
/// starts in a packet, ahead of the packet's first sample by up to one of its frames, which
/// FFmpeg sizes as about a packet and its first access unit pointer never places. A stamp within
/// the widest packet ahead of the count keeps it, one outside starts it over.
#[derive(Clone, Copy, Default)]
struct Count {
    /// The anchor stamp in stream time ns and the sample frames out since, `None` unanchored.
    at: Option<(u64, u64)>,
    /// The most sample frames a packet held.
    widest: u64,
}

impl Count {
    /// The stamp of a unit of `frames` at `rate`, `present` its placed stamp.
    fn stamp(&mut self, present: Option<u64>, frames: u64, rate: u32) -> Option<u64> {
        let ns = |n: u64| (u128::from(n) * 1_000_000_000 / u128::from(rate.max(1))) as u64;
        self.widest = self.widest.max(frames);
        let ahead = ns(self.widest).saturating_add(COUNT_SLACK_NS);
        let fits = match (present, self.at) {
            (Some(p), Some((anchor, n))) => {
                let at = anchor.saturating_add(ns(n));
                p.saturating_add(COUNT_SLACK_NS) >= at && p <= at.saturating_add(ahead)
            }
            (None, at) => at.is_some(),
            (Some(_), None) => false,
        };
        if !fits {
            self.at = present.map(|p| (p, 0));
        }
        let (anchor, n) = self.at.as_mut()?;
        let at = anchor.saturating_add(ns(*n));
        *n = n.saturating_add(frames);
        Some(at)
    }
}

struct Track {
    lane: Lane,
    framer: Option<Framer>,
    spu: Option<Spu>,
    pcm: Option<Pcm>,
}

impl Track {
    fn restart(&mut self) {
        let lane = &mut self.lane;
        (lane.started, lane.need_key, lane.broke) = (false, lane.kind == StreamKind::Video, false);
        (lane.read, lane.resume) = (None, None);
        if let Some(f) = &mut self.framer {
            f.reset();
        }
        if let Some(s) = &mut self.spu {
            s.reset();
        }
        if let Some(p) = &mut self.pcm {
            (p.held, p.count) = (0, Count::default());
        }
    }
}

enum Parked {
    Unit { pad: SrcPad, pts: Option<u64>, flags: BufferFlags, from: usize, to: usize },
    Format { pad: SrcPad, family: &'static str, lpcm: Option<Lpcm> },
}

#[derive(Default)]
struct Park {
    bytes: Vec<u8>,
    rows: VecDeque<Parked>,
}

impl Park {
    fn clear(&mut self) {
        self.rows.clear();
        self.bytes.clear();
    }
}

/// The file's earliest stamp is zero. A new time base resumes where the last unit out was, and a
/// seek takes up the one read at its byte.
struct Clock {
    unwrap: Unwrapper,
    scr: Unwrapper,
    shift: i64,
    /// The time bases read in file bytes.
    spans: Spans,
    anchor: Option<u64>,
    origin: Option<i64>,
    sought: bool,
    /// Behind a restart for one track the next stamp steps back on the same line, no break.
    back: bool,
    last_ns: u64,
    stretch: u32,
    broke: bool,
}

impl Clock {
    /// `(decode, presentation)` in stream time ns. `at` is the unit's file byte, `None` when its
    /// offset is none.
    fn place(&mut self, stamp: Stamp, last_dts: &mut (u32, i64), at: Option<u64>) -> (u64, u64) {
        let dts = stamp.dts.unwrap_or(stamp.pts);
        if std::mem::take(&mut self.sought) {
            match at.and_then(|at| self.spans.at(at)) {
                Some(span) => {
                    self.unwrap.prime(congruent(span.ticks - ORIGIN_SLACK, dts));
                    self.shift = span.shift_ns;
                }
                None => {
                    if let Some(origin) = self.origin {
                        let base = Base { origin, slack: ORIGIN_SLACK };
                        self.unwrap.prime(origin + base.ticks(dts));
                        self.shift = ticks_ns(origin);
                    }
                }
            }
        }
        let placed = self.unwrap.unwrap(dts);
        let back = std::mem::take(&mut self.back);
        let jumped = placed.step == Stepped::Discontinuity && !back;
        let behind = *last_dts != (0, 0) && last_dts.0 == self.stretch && placed.ticks < last_dts.1;
        let broke = std::mem::take(&mut self.broke) || jumped || behind;
        if broke && self.anchor.is_none() {
            self.anchor = Some(self.last_ns);
        }
        let decode = ticks_ns(placed.ticks);
        if let Some(stream) = self.anchor.take() {
            self.shift = decode.saturating_sub(i64::try_from(stream).unwrap_or(i64::MAX));
            self.stretch = self.stretch.wrapping_add(1).max(1);
            if let Some(byte) = at {
                self.spans.note(Span { byte, ticks: placed.ticks, shift_ns: self.shift });
            }
        }
        *last_dts = (self.stretch, placed.ticks);
        let present = ticks_ns(self.unwrap.unwrap(stamp.pts).ticks);
        self.unwrap.prime(placed.ticks);
        let at = |ns: i64| ns.saturating_sub(self.shift).max(0) as u64;
        let (decode, present) = (at(decode), at(present));
        self.last_ns = self.last_ns.max(present);
        (decode, present)
    }
}

/// Units dropped since start, the first of each kind logged and the counts at the end.
#[derive(Clone, Copy, Default)]
struct Drops {
    anomalies: u64,
    scrambled: u64,
    lpcm: u64,
}

/// What a unit goes out through, borrowed apart from the tracks.
struct Emit<'a> {
    ctx: &'a mut Ctx,
    drops: &'a mut Drops,
    park: &'a mut Park,
    clock: &'a mut Clock,
    grown: &'a mut Grown,
    measured: &'a mut rate::ByteRate,
    teller: &'a mut rate::Teller,
    duration: Option<u64>,
    exact: bool,
}

impl Emit<'_> {
    /// With something parked already everything parks behind it, so the order holds.
    fn put(&mut self, pad: SrcPad, pts: Option<u64>, flags: BufferFlags, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.park.rows.is_empty() && write(self.ctx, self.exact, pad, pts, flags, bytes) {
            return;
        }
        let from = self.park.bytes.len();
        self.park.bytes.extend_from_slice(bytes);
        self.park.rows.push_back(Parked::Unit { pad, pts, flags, from, to: self.park.bytes.len() });
    }

    fn announce(&mut self, pad: SrcPad, family: &'static str, lpcm: Option<Lpcm>) {
        match self.park.rows.is_empty() {
            true => announce(self.ctx, pad, family, lpcm),
            false => self.park.rows.push_back(Parked::Format { pad, family, lpcm }),
        }
    }

    fn unit(&mut self, lane: &mut Lane, au: Au<'_>) {
        self.counted_unit(lane, au, None);
    }

    /// `count` is an LPCM unit's sample count, its frames and rate, which stamp it.
    fn counted_unit(&mut self, lane: &mut Lane, au: Au<'_>, count: Option<(&mut Count, u64, u32)>) {
        let Some(pad) = lane.pad else { return };
        let video = lane.kind == StreamKind::Video;
        // A start needs its time, and a muxer may stamp a key's neighbour and not the key.
        if video && lane.need_key && !(au.key && au.stamp.is_some()) {
            return;
        }
        let at = (self.grown.placed == Placed::File).then_some(au.offset);
        let mut stamps = au.stamp.map(|s| self.clock.place(s, &mut lane.last_dts, at));
        if let Some((count, frames, rate)) = count {
            stamps = count.stamp(stamps.map(|s| s.1), frames, rate).map(|ns| (ns, ns));
        }
        let mut flags = match std::mem::take(&mut lane.broke) {
            true => BufferFlags::DISCONT,
            false => BufferFlags::empty(),
        };
        if video {
            lane.need_key = false;
            if au.key {
                flags |= BufferFlags::KEYFRAME;
            }
        }
        let keyed = self.grown.key.is_some_and(|key| key.pid == lane.key);
        if let Some((_, present)) = stamps.filter(|_| keyed) {
            if (!video || au.key)
                && self.grown.keys(lane.key)
                && self.grown.keyframe(present, au.offset)
            {
                publish(self.ctx, self.grown, self.duration);
            }
            self.measured.note(au.offset, Timestamp(present));
            let rate = self.measured.rate();
            self.teller.tell_at(self.ctx, PsDemux::IN, Timestamp(present), |_| rate);
        }
        if !lane.started {
            lane.started = true;
            self.announce(pad, lane.family, lane.lpcm);
        }
        self.put(pad, stamps.map(|s| s.1), flags, au.bytes);
    }

    fn lpcm(&mut self, lane: &mut Lane, pcm: &mut Pcm, u: &Unit<'_>, stamp: Option<Stamp>) {
        let Some(format) = u.header.get(4..).and_then(|h| Lpcm::parse(h).ok()) else {
            if self.drops.lpcm == 0 {
                log!(&*self.ctx, Level::Warn, "lpcm_header_bad", key = lane.key, at = u.offset);
            }
            self.drops.lpcm += 1;
            return;
        };
        if lane.lpcm != Some(format) {
            (lane.lpcm, lane.started, pcm.held) = (Some(format), false, 0);
            pcm.count = Count::default();
        }
        let (bin, bout) = format.block();
        let (mut data, mut wrote) = (u.payload, 0);
        if pcm.held > 0 {
            let take = (bin - pcm.held).min(data.len());
            pcm.carry[pcm.held..pcm.held + take].copy_from_slice(&data[..take]);
            (pcm.held, data) = (pcm.held + take, &data[take..]);
            if pcm.held == bin {
                wrote = format.convert(&pcm.carry[..bin], &mut pcm.out[..bout]).1;
                pcm.held = 0;
            }
        }
        let (used, n) = format.convert(data, &mut pcm.out[wrote..]);
        let rest = &data[used..];
        let keep = rest.len().min(bin);
        pcm.carry[pcm.held..pcm.held + keep].copy_from_slice(&rest[..keep]);
        pcm.held += keep;
        let Pcm { out, count, .. } = pcm;
        let frames = ((wrote + n) / (format.width() * usize::from(format.channels))) as u64;
        let au = Au { bytes: &out[..wrote + n], stamp, key: true, offset: u.offset };
        self.counted_unit(lane, au, Some((count, frames, format.rate)));
    }
}

/// False when the budget is spent. A unit no pool budget serves is reported and dropped.
fn write(
    ctx: &mut Ctx,
    exact: bool,
    pad: SrcPad,
    pts: Option<u64>,
    flags: BufferFlags,
    bytes: &[u8],
) -> bool {
    let mut buf = match exact {
        true => match ctx.alloc_exact(bytes.len()) {
            Some(buf) => buf,
            None => {
                let (element, error) = (ctx.element(), ctx.exact_refused(bytes.len()));
                ctx.post(BusMessage::Warning { element, error });
                return true;
            }
        },
        false => match ctx.try_alloc_sized(bytes.len()) {
            Ok(Some(buf)) => buf,
            Ok(None) => return false,
            Err(e) => {
                let element = ctx.element();
                ctx.post(BusMessage::Warning { element, error: Error::element_failed(element, e) });
                return true;
            }
        },
    };
    buf.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
    buf.memory.set_len(bytes.len());
    buf.pts = pts.map_or(Timestamp::NONE, Timestamp);
    buf.flags = flags;
    ctx.out(pad).push(buf);
    true
}

fn announce(ctx: &mut Ctx, pad: SrcPad, family: &'static str, lpcm: Option<Lpcm>) {
    match lpcm {
        Some(l) => ctx.announce_format(
            pad,
            family,
            &[
                (FIELD_RATE, ValueDesc::Int(i64::from(l.rate))),
                (FIELD_CHANNELS, ValueDesc::Int(i64::from(l.channels))),
                (FIELD_SAMPLE, ValueDesc::Id(l.sample())),
            ],
        ),
        None => ctx.announce_format(pad, family, &[]),
    }
}

fn log_duration(ctx: &Ctx, duration: Option<u64>, tail: usize) {
    match duration {
        Some(ns) => log!(ctx, Level::Info, "duration", ns = Timestamp(ns)),
        None => log!(ctx, Level::Warn, "duration_unknown", tail = tail),
    }
}

fn publish(ctx: &Ctx, grown: &mut Grown, duration: Option<u64>) {
    let mut index = grown.index();
    index.duration = duration.map_or(Timestamp::NONE, Timestamp);
    ctx.publish_index(index);
}

fn on_unit(emit: &mut Emit<'_>, tracks: &mut [Track], u: &Unit<'_>) {
    let Some(track) = tracks.iter_mut().find(|t| t.lane.key == u.key && t.lane.pad.is_some())
    else {
        return;
    };
    // Read again behind a restart for another track, it went in the first time.
    let lane = &mut track.lane;
    if lane.resume.is_some_and(|last| u.offset <= last) {
        return;
    }
    (lane.resume, lane.read) = (None, Some(u.offset));
    // Not in the clear (CSS on a DVD), nothing downstream can use it.
    if u.flags & PH_SCRAMBLED != 0 {
        if emit.drops.scrambled == 0 {
            log!(&*emit.ctx, Level::Warn, "scrambled", key = u.key, at = u.offset);
        }
        emit.drops.scrambled += 1;
        return;
    }
    let stamp = u.pts.map(|pts| Stamp { pts, dts: u.dts });
    let Track { lane, framer, spu, pcm } = track;
    if let Some(f) = framer {
        return f.push(u.payload, stamp, u.offset, &mut |au| emit.unit(lane, au));
    }
    if let Some(s) = spu {
        return s.push(u.payload, stamp, u.offset, &mut |au| emit.unit(lane, au));
    }
    if let Some(pcm) = pcm {
        return emit.lpcm(lane, pcm, u, stamp);
    }
    emit.unit(lane, Au { bytes: u.payload, stamp, key: true, offset: u.offset });
}

/// What is in progress goes out whole, and each stream starts over at a key.
fn on_break(emit: &mut Emit<'_>, tracks: &mut [Track]) {
    for Track { lane, framer, spu, pcm } in tracks {
        if let Some(f) = framer {
            f.flush(&mut |au| emit.unit(lane, au));
        }
        if let Some(s) = spu {
            s.reset();
        }
        if let Some(p) = pcm {
            (p.held, p.count) = (0, Count::default());
        }
        (lane.need_key, lane.broke) = (lane.kind == StreamKind::Video, true);
    }
    emit.clock.broke = true;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tail {
    Absent,
    Want,
    Asked,
    In,
}

/// One sink pad, one src pad per elementary stream.
pub struct PsDemux {
    head: Vec<u8>,
    len: Option<u64>,
    tail: Vec<u8>,
    demux: Demux,
    tracks: Vec<Track>,
    clock: Clock,
    park: Park,
    drops: Drops,
    feeding: Option<(Memory, usize)>,
    /// At the end, what does not fit a slot goes out in its own allocation.
    ending: bool,
    /// First decode time in the head and the earliest stream's first one, in ticks.
    opening: Option<(i64, i64)>,
    /// The head's time bases and pack rate, for the duration with the tail.
    line: (Option<u16>, Line, Rate),
    duration: Option<u64>,
    grown: Grown,
    measured: rate::ByteRate,
    teller: rate::Teller,
    feed: Feed,
    paired: bool,
    bisect: Option<(Bisect, SeekTarget)>,
    probe: Option<u64>,
    tail_at: Tail,
    /// The pull reset ahead is a restart for one track, read from its byte without a bisection.
    rejoined: bool,
}

impl PsDemux {
    pub const IN: SinkPad = SinkPad(0);

    /// `head` has to reach past the first unit of each stream, a few hundred kilobytes of a DVD.
    #[allow(clippy::disallowed_methods)] // once per element
    pub fn new(head: Vec<u8>) -> Self {
        Self {
            head,
            len: None,
            tail: Vec::new(),
            demux: Demux::new(),
            tracks: Vec::new(),
            clock: Clock {
                unwrap: Unwrapper::for_timestamps(),
                scr: Unwrapper::for_timestamps(),
                shift: 0,
                spans: Spans::default(),
                anchor: None,
                origin: None,
                sought: false,
                back: false,
                last_ns: 0,
                stretch: 1,
                broke: false,
            },
            park: Park::default(),
            drops: Drops::default(),
            feeding: None,
            ending: false,
            opening: None,
            line: (None, Line::default(), Rate::default()),
            duration: None,
            grown: Grown::new(),
            measured: rate::ByteRate::default(),
            teller: rate::Teller::default(),
            feed: Feed::new(WINDOW, WINDOW_DEPTH),
            paired: false,
            bisect: None,
            probe: None,
            tail_at: Tail::Absent,
            rejoined: false,
        }
    }

    /// A source `len` bytes long, for the index's estimate and a pulled seek.
    #[must_use]
    pub fn with_len(mut self, len: Option<u64>) -> Self {
        (self.len, self.grown.file_len) = (len, len);
        self.feed.set_end(len);
        self
    }

    /// A source `len` bytes long that an opener found lasts `ns`, the duration published from the
    /// start and no tail asked.
    #[must_use]
    pub fn with_duration(self, len: u64, ns: u64) -> Self {
        let mut d = self.with_len(Some(len));
        (d.duration, d.tail_at) = ((ns > 0).then_some(ns), Tail::In);
        d
    }

    /// A source `len` bytes long whose last bytes are `tail`, read by the caller, which gives the
    /// duration from the last stamp there and asks no tail.
    #[must_use]
    pub fn with_extent(self, len: u64, tail: Vec<u8>) -> Self {
        let mut d = self.with_len(Some(len));
        (d.tail, d.tail_at) = (tail, Tail::In);
        d
    }

    /// The duration by the head and `tail`, the end of the file.
    fn lasts(&self, tail: &[u8]) -> Option<u64> {
        let (_, least) = self.opening?;
        let (key, line, rate) = &self.line;
        let ticks = lasts((*key, line, *rate), least, tail, self.len)?;
        Some(ticks_ns(ticks).max(0) as u64)
    }

    fn base(&self) -> Option<Base> {
        self.clock.origin.map(|origin| Base { origin, slack: ORIGIN_SLACK })
    }

    fn failed(ctx: &Ctx, error: PsError) -> Error {
        Error::element_failed(ctx.element(), error)
    }

    /// Bytes into the demuxer, or with `end` what it holds, its units out.
    fn read(&mut self, ctx: &mut Ctx, data: &[u8], end: bool) {
        let Self {
            demux,
            tracks,
            clock,
            park,
            drops,
            grown,
            measured,
            teller,
            duration,
            ending,
            ..
        } = self;
        let mut emit = Emit {
            ctx,
            drops,
            park,
            clock,
            grown,
            measured,
            teller,
            duration: *duration,
            exact: *ending,
        };
        let mut sink = |event: demux::Event<'_>| match event {
            demux::Event::Unit(u) => on_unit(&mut emit, tracks, &u),
            demux::Event::Pack { pack, offset } => {
                let step = emit.clock.scr.unwrap(pack.scr).step;
                if matches!(step, Stepped::Discontinuity | Stepped::Backward) {
                    let back = matches!(step, Stepped::Backward);
                    log!(&*emit.ctx, Level::Debug, "scr_break", at = offset, backward = back);
                    on_break(&mut emit, tracks);
                }
            }
            demux::Event::Anomaly(a) => {
                let level = if emit.drops.anomalies == 0 { Level::Warn } else { Level::Trace };
                log!(&*emit.ctx, level, "anomaly", kind = a.kind.name(), at = a.offset);
                emit.drops.anomalies += 1;
            }
            _ => {}
        };
        match end {
            false => demux.push(data, &mut sink),
            true => {
                demux.flush(&mut sink);
                for Track { lane, framer, .. } in tracks.iter_mut() {
                    if let Some(f) = framer {
                        f.flush(&mut |au| emit.unit(lane, au));
                    }
                }
            }
        }
    }

    /// Put out what is parked, in order. False while the pool is still dry.
    fn unpark(&mut self, ctx: &mut Ctx) -> bool {
        let exact = self.ending;
        while let Some(row) = self.park.rows.front() {
            match *row {
                Parked::Unit { pad, pts, flags, from, to } => {
                    if !write(ctx, exact, pad, pts, flags, &self.park.bytes[from..to]) {
                        return false;
                    }
                }
                Parked::Format { pad, family, lpcm } => announce(ctx, pad, family, lpcm),
            }
            self.park.rows.pop_front();
        }
        self.park.bytes.clear();
        true
    }

    /// Everything in progress drops, as behind a flush.
    fn restart_streams(&mut self) {
        for track in &mut self.tracks {
            track.restart();
        }
        self.park.clear();
        (self.feeding, self.ending) = (None, false);
        self.clock.scr.reset();
        (self.clock.sought, self.clock.back, self.clock.broke) = (true, false, false);
        self.clock.stretch = self.clock.stretch.wrapping_add(1).max(1);
    }

    /// With `restart` the source went back for the track and the demuxer reads on from there.
    fn relink(&mut self, ctx: &Ctx, pad: SrcPad, on: bool, restart: Option<SeekTarget>) {
        let Some(at) = self.tracks.iter().position(|t| ctx.stream_pad(t.lane.stream) == Some(pad))
        else {
            return;
        };
        if let Some(target) = restart.filter(|_| on) {
            self.rejoin(ctx, at, pad, target);
        }
        let track = &mut self.tracks[at];
        track.restart();
        track.lane.pad = on.then_some(pad);
        self.grown.rekey(key_of(&self.tracks));
    }

    /// The track drops what it parked for a full pool, it was read ahead of the restart and
    /// would go out first and again when read. The other tracks keep theirs, and what they had
    /// in progress, and drop what they read again up to their last unit.
    fn rejoin(&mut self, ctx: &Ctx, at: usize, pad: SrcPad, target: SeekTarget) {
        let old = self.tracks[at].lane.pad;
        self.park.rows.retain(|row| match *row {
            Parked::Unit { pad: of, .. } | Parked::Format { pad: of, .. } => {
                of != pad && Some(of) != old
            }
        });
        for (n, track) in self.tracks.iter_mut().enumerate() {
            let lane = &mut track.lane;
            lane.resume = lane.read.or(lane.resume).filter(|_| lane.pad.is_some() && n != at);
            lane.read = None;
        }
        self.feeding = None;
        self.feed.restart(target.to_byte);
        self.demux.set_position(target.to_byte);
        (self.bisect, self.probe) = (None, None);
        self.rejoined = ctx.pulls(Self::IN);
        // The packs and stamps read again go back, which is no break, and the track read again
        // is not behind itself. The line stays, a jump the other tracks made is kept.
        self.clock.scr.reset();
        (self.clock.back, self.clock.broke) = (true, false);
        self.clock.stretch = self.clock.stretch.wrapping_add(1).max(1);
    }

    fn publish(&mut self, ctx: &mut Ctx) {
        if let Some(ns) = self.duration {
            ctx.set_duration(ns);
        }
        publish(ctx, &mut self.grown, self.duration);
    }

    /// The duration from the tail, once the head gave the origin.
    fn tail_in(&mut self, ctx: &mut Ctx, tail: &[u8]) {
        self.tail_at = Tail::In;
        self.duration = self.lasts(tail);
        log_duration(ctx, self.duration, tail.len());
        self.publish(ctx);
    }

    /// The run's counts, and each drop the run kept quiet past its first.
    fn log_end(&self, ctx: &Ctx) {
        let st = self.demux.stats();
        let (packs, units, junk, anomalies) = (st.packs, st.units, st.junk, st.anomalies);
        log!(
            ctx,
            Level::Info,
            "eos",
            packs = packs,
            units = units,
            junk = junk,
            anomalies = anomalies
        );
        let d = self.drops;
        if d.scrambled > 0 || d.lpcm > 0 {
            log!(ctx, Level::Warn, "dropped", scrambled = d.scrambled, lpcm = d.lpcm);
        }
        for t in &self.tracks {
            let overflows = t.framer.as_ref().map_or(0, |f| f.overflows);
            let broken = t.spu.as_ref().map_or(0, |s| s.broken);
            if overflows > 0 || broken > 0 {
                let key = t.lane.key;
                log!(
                    ctx,
                    Level::Warn,
                    "units_lost",
                    key = key,
                    overflows = overflows,
                    spu = broken
                );
            }
        }
    }

    fn bisector(&self, ctx: &Ctx, from: SeekTarget) -> Option<Bisect> {
        let len = self.len.filter(|len| *len > 0 && ctx.pulls(Self::IN))?;
        let base = self.base()?;
        let target = ctx.presentation_point().unwrap_or(from.at).nanos()?;
        let ticks = (i128::from(target) * 9 / 100_000) as i64;
        Some(Bisect::new(base, ticks - SEEK_BACK, len))
    }

    fn ask_probe(&mut self, ctx: &mut Ctx) {
        let Some((bisect, from)) = &self.bisect else { return };
        if self.probe.is_some() {
            return;
        }
        let (step, from) = (bisect.step(), *from);
        match step {
            // A pool that cannot hold the probe never will, so the seek lands as planned.
            Step::Probe { len, .. } if u64::from(len) > ctx.pool_stats().bytes_budget => {
                log!(&*ctx, Level::Warn, "seek_probe_unread", bytes = len, at = from.to_byte);
                self.land(None, from)
            }
            Step::Probe { at, len } => match self.feed.aside(ctx, Self::IN, at, len, PROBE_TAG) {
                true => self.probe = Some(at),
                false => ctx.wake_within(std::time::Duration::from_millis(1)),
            },
            // Nothing at or before the target, which is ahead of the first pack.
            Step::Land(landing) => {
                let landing = landing.unwrap_or(Landing { at: 0, ticks: 0 });
                log!(&*ctx, Level::Debug, "seek_landed", at = landing.at, ticks = landing.ticks);
                self.land(Some(landing), from)
            }
        }
    }

    fn land(&mut self, landing: Option<Landing>, from: SeekTarget) {
        self.bisect = None;
        let at = landing.map_or(from.to_byte, |l| l.at);
        self.feed.restart(at);
        self.demux.set_position(at);
        self.grown.placed = Placed::File;
    }

    fn restart_pull(&mut self, ctx: &mut Ctx, from: SeekTarget) {
        self.feed.restart(from.to_byte);
        self.demux.set_position(from.to_byte);
        (self.bisect, self.probe) = (None, None);
        // The first pairing reads from the start, any later one is a seek or a restart.
        let rejoined = std::mem::take(&mut self.rejoined);
        if std::mem::replace(&mut self.paired, true) && !rejoined {
            self.bisect = self.bisector(ctx, from).map(|b| (b, from));
            let bisect = self.bisect.is_some();
            log!(&*ctx, Level::Debug, "seek", target = from.at, at = from.to_byte, bisect = bisect);
        }
        let wanted = self.len.is_some_and(|len| len > 0) && self.duration.is_none();
        self.tail_at = match self.tail_at {
            Tail::Absent | Tail::Want | Tail::Asked if wanted => Tail::Want,
            Tail::Want | Tail::Asked => Tail::Absent,
            t => t,
        };
    }

    fn ask_tail(&mut self, ctx: &mut Ctx) {
        let (Tail::Want, Some(len)) = (self.tail_at, self.len) else { return };
        let want = (TAIL_LEN as u64).min(len);
        // A pool that cannot hold the tail never will, so the duration stays unknown.
        if want > ctx.pool_stats().bytes_budget {
            log!(&*ctx, Level::Warn, "tail_unread", bytes = want);
            self.tail_at = Tail::In;
            return self.publish(ctx);
        }
        if self.feed.aside(ctx, Self::IN, len - want, want as u32, TAIL_TAG) {
            self.tail_at = Tail::Asked;
        }
    }

    fn pull(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Flow {
        self.ask_tail(ctx);
        self.ask_probe(ctx);
        let mut held = false;
        loop {
            if !self.unpark(ctx) {
                held = true;
                break;
            }
            match self.feed.take(ctx, Self::IN, inputs) {
                Some(Fed::Window { buf, .. }) if self.bisect.is_none() => {
                    self.read(ctx, buf.memory.data(), false)
                }
                Some(Fed::Window { .. }) => {}
                Some(Fed::Aside { tag: PROBE_TAG, buf }) => {
                    if let (Some((bisect, _)), Some(at)) = (&mut self.bisect, self.probe.take()) {
                        bisect.served(at, buf.memory.data());
                    }
                    self.ask_probe(ctx);
                }
                Some(Fed::Aside { buf, .. }) if self.tail_at == Tail::Asked => {
                    self.tail_in(ctx, buf.memory.data())
                }
                Some(Fed::Aside { .. }) => {}
                None => break,
            }
        }
        match (held, self.bisect.is_some()) {
            (_, true) => {}
            (true, false) => ctx.hold_end(),
            (false, false) => self.feed.ask(ctx, Self::IN),
        }
        Flow::Ok
    }

    fn push(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Flow {
        loop {
            // Fed only while output leaves, so a slow consumer backs up into the queue ahead.
            if !self.unpark(ctx) {
                ctx.hold_end();
                return Flow::Ok;
            }
            let (memory, at) = match self.feeding.take() {
                Some(feeding) => feeding,
                None => {
                    let Some(buf) = inputs.pop() else { return Flow::Ok };
                    if buf.flags.contains(BufferFlags::DISCONT) {
                        self.restart_streams();
                        self.grown.placed = Placed::Lost;
                    }
                    (buf.memory, 0)
                }
            };
            let end = (at + PIECE).min(memory.len());
            self.read(ctx, &memory.data()[at..end], false);
            if end < memory.len() {
                self.feeding = Some((memory, end));
            }
        }
    }
}

/// The first linked video stream, else the first linked audio stream.
fn key_of(tracks: &[Track]) -> Option<Key> {
    let linked =
        |kind: StreamKind| tracks.iter().find(|t| t.lane.pad.is_some() && t.lane.kind == kind);
    let track = linked(StreamKind::Video).or_else(|| linked(StreamKind::Audio))?;
    let lane = &track.lane;
    Some(Key { pid: lane.key, stream: lane.stream, video: lane.kind == StreamKind::Video })
}

impl Element for PsDemux {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    #[allow(clippy::disallowed_methods)] // once per element
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let head = scan_head(&self.head);
        self.head = Vec::new();
        self.opening = head.opening;
        self.line = (head.key, head.line.clone(), head.rate);
        self.clock.origin = head.opening.map(|(_, least)| least);
        let mut seen: Vec<&Seen> = head.seen.iter().collect();
        seen.sort_by_key(|s| streams::rank(s.key));
        self.tracks.clear();
        for s in seen {
            let psm_type = head.psm[usize::from(s.key >> 8)];
            let Some(codec) = streams::codec_of(s.key, psm_type, &s.sniff, head.mpeg2) else {
                log!(&*ctx, Level::Warn, "stream_unknown", key = s.key, psm_type = psm_type);
                continue;
            };
            let (family, kind) = streams::family_of(codec);
            log!(
                &*ctx,
                Level::Info,
                "stream",
                key = s.key,
                family = family,
                rate = s.lpcm.map_or(0, |l| l.rate),
                channels = s.lpcm.map_or(0, |l| l.channels),
            );
            let stream = ctx.add_stream(Stream {
                kind,
                codec: family,
                language: "".into(),
                title: "".into(),
                default: true,
                forced: false,
                disposition: Disposition::MAIN,
                channels: s.lpcm.map_or(0, |l| u16::from(l.channels)),
                duration: Timestamp::NONE,
                sparse: kind.sparse(),
                name: format!("src_{:04x}", s.key).into(),
                offers: &SRC_OFFERS,
            });
            let video = streams::video_of(codec);
            self.tracks.push(Track {
                lane: Lane {
                    key: s.key,
                    family,
                    kind,
                    stream,
                    pad: None,
                    started: false,
                    need_key: video.is_some(),
                    broke: false,
                    lpcm: None,
                    last_dts: (0, 0),
                    read: None,
                    resume: None,
                },
                framer: video.map(|v| Framer::new(v, AU_RESERVE, AU_MAX)),
                spu: (codec == Codec::Subpicture).then(Spu::new),
                pcm: (codec == Codec::Lpcm).then(|| Pcm {
                    carry: [0; lpcm::BLOCK_MAX],
                    held: 0,
                    out: vec![0; PCM_OUT],
                    count: Count::default(),
                }),
            });
        }
        if head.capped > 0 {
            log!(&*ctx, Level::Warn, "streams_capped", max = STREAMS_MAX, units = head.capped);
        }
        if self.tail_at == Tail::In && self.duration.is_none() {
            let tail = std::mem::take(&mut self.tail);
            self.duration = self.lasts(&tail);
            log_duration(ctx, self.duration, tail.len());
        }
        log!(
            &*ctx,
            Level::Info,
            "opened",
            streams = self.tracks.len(),
            mpeg2 = head.mpeg2,
            timed = head.opening.is_some(),
        );
        match self.tracks.is_empty() {
            true => Err(Self::failed(ctx, PsError::NoStreams)),
            false => Ok(()),
        }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        for track in &mut self.tracks {
            track.lane.pad = ctx.stream_pad(track.lane.stream).filter(|pad| ctx.pad_linked(*pad));
            track.restart();
        }
        self.demux = Demux::new();
        self.grown.placed = Placed::File;
        self.grown.rekey(key_of(&self.tracks));
        let clock = &mut self.clock;
        clock.unwrap.reset();
        clock.scr.reset();
        (clock.shift, clock.anchor, clock.sought, clock.last_ns, clock.broke) =
            (0, Some(0), false, 0, false);
        clock.back = false;
        clock.spans.clear();
        if let Some((first, least)) = self.opening {
            clock.unwrap.prime(first);
            (clock.shift, clock.anchor) = (ticks_ns(least), None);
            clock.spans.note(Span { byte: 0, ticks: least, shift_ns: ticks_ns(least) });
        }
        (self.feeding, self.ending, self.paired, self.bisect, self.probe) =
            (None, false, false, None, None);
        self.drops = Drops::default();
        self.park.clear();
        self.feed.restart(0);
        ctx.will_index();
        ctx.want_ranges(Self::IN);
        if self.tail_at == Tail::In || self.len.is_none() {
            self.publish(ctx);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(from) = ctx.pull_reset(Self::IN) {
            self.restart_pull(ctx, from);
        }
        Ok(match ctx.pulls(Self::IN) {
            true => self.pull(ctx, &mut inputs),
            false => self.push(ctx, &mut inputs),
        })
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => {
                self.ending = true;
                if let Some((memory, at)) = self.feeding.take() {
                    self.read(ctx, &memory.data()[at..], false);
                }
                self.read(ctx, &[], true);
                self.unpark(ctx);
                if self.grown.unpublished() {
                    self.publish(ctx);
                }
                self.log_end(ctx);
            }
            Event::FlushStart => {
                self.measured.clear();
                self.teller.restart();
                if self.grown.unpublished() {
                    self.publish(ctx);
                }
                self.rejoined = false;
                self.restart_streams();
                let target = ctx.seek_target();
                let at = target.map_or(0, |t| t.to_byte);
                log!(&*ctx, Level::Debug, "flush", seek = target.is_some(), at = at);
                self.grown.placed = if target.is_some() { Placed::File } else { Placed::Lost };
                if let Some(target) = target {
                    self.demux.set_position(target.to_byte);
                }
            }
            Event::SrcLinked { pad, on, restart } => self.relink(ctx, *pad, *on, *restart),
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        (self.feeding, self.bisect, self.probe) = (None, None, None);
        self.park.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = 90_000;

    /// Behind a restart for one track the stamps read again step back on the line the other
    /// tracks are on, even past the 10 s a step may make.
    #[test]
    fn a_restart_far_back_keeps_the_line() {
        let mut clock = Clock {
            unwrap: Unwrapper::for_timestamps(),
            scr: Unwrapper::for_timestamps(),
            shift: 0,
            spans: Spans::default(),
            anchor: None,
            origin: Some(0),
            sought: false,
            back: false,
            last_ns: 0,
            stretch: 1,
            broke: false,
        };
        let mut last = (0, 0);
        let at = |pts: u64| Stamp { pts, dts: None };
        // A jump the other tracks made moves the line.
        let first = clock.place(at(200 * SECOND), &mut last, None);
        let joined = clock.place(at(400 * SECOND), &mut last, None);
        assert_eq!(joined.0, first.0);
        clock.place(at(408 * SECOND), &mut last, None);
        (clock.back, clock.stretch) = (true, clock.stretch + 1);
        let again = clock.place(at(395 * SECOND), &mut last, None);
        assert_eq!(again.0 + 5_000_000_000, joined.0);
        // A near restart takes it too, a later jump is a break again.
        (clock.back, clock.stretch) = (true, clock.stretch + 1);
        let near = clock.place(at(390 * SECOND), &mut last, None);
        assert_eq!(near.0 + 5_000_000_000, again.0);
        let jumped = clock.place(at(600 * SECOND), &mut last, None);
        assert_eq!(jumped.0, 208_000_000_000, "on from the last stamp out");
    }

    /// A clock that starts 10 h on and restarts at byte 10 000. A seek back into either base
    /// after the read went past the restart stamps as the read did.
    #[test]
    fn a_seek_takes_the_base_of_its_landing() {
        let origin = (36_000 * SECOND) as i64;
        let mut clock = Clock {
            unwrap: Unwrapper::for_timestamps(),
            scr: Unwrapper::for_timestamps(),
            shift: ticks_ns(origin),
            spans: Spans::default(),
            anchor: None,
            origin: Some(origin),
            sought: false,
            back: false,
            last_ns: 0,
            stretch: 1,
            broke: false,
        };
        clock.unwrap.prime(origin);
        clock.spans.note(Span { byte: 0, ticks: origin, shift_ns: ticks_ns(origin) });
        let mut last = (0, 0);
        let at = |pts: u64| Stamp { pts, dts: None };
        // 20 s from 10 h, then 20 s from 1 s.
        let read: Vec<(u64, u64, u64)> = (0..20u64)
            .map(|n| (n * 500, 36_000 * SECOND + n * SECOND))
            .chain((0..20u64).map(|n| (10_000 + n * 500, SECOND + n * SECOND)))
            .map(|(byte, pts)| (byte, pts, clock.place(at(pts), &mut last, Some(byte)).1))
            .collect();
        assert_eq!((read[19].2, read[20].2, read[39].2), (19 * S, 19 * S, 38 * S));
        for &(byte, pts, stamp) in [read[5], read[25], read[39], read[0]].iter() {
            (clock.sought, clock.stretch) = (true, clock.stretch + 1);
            assert_eq!(clock.place(at(pts), &mut last, Some(byte)).1, stamp, "at byte {byte}");
        }
    }

    const S: u64 = 1_000_000_000;
}
