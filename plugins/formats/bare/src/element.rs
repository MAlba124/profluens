//! `baresrc`, the source of a bare FLAC, MP3, ADTS, AC-3, E-AC-3 or DTS file, or of a WAV file's
//! PCM. It drives a [`Reader`]'s machines over the file with positioned reads through the
//! reactor, a frame per buffer, and takes a seek's target time from the flush, the reader finds
//! the frame. A run opens with the codec head flagged HEADER as the Matroska demuxer does it. The
//! index it publishes has no entries, so the pipeline plans by time and a snap seek lands on its
//! target, every frame is a key.

// The caller's open file, handed to the reactor, see `BareSrc::new`.
#[allow(clippy::disallowed_types)]
use std::fs::File;
use std::os::fd::AsFd;

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::fetch::{Fetch, Need, Step};
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::SrcPad;
use profluens_core::io::{Handle, IoError, IoResult, OpenMode};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::pipeline::{Index, Serves};
use profluens_core::time::Timestamp;
use std::fmt;

use crate::BareError;
use crate::head::{BareHead, Codec};
use crate::reader::{CHUNK, NextPark, Reader, SeekPark, Unit};
use profluens_audio::layout::{FIELD_LAYOUT, LAYOUT_FIELD};
use profluens_audio::{FIELD_CHANNELS, FIELD_RATE, FIELD_SAMPLE};

/// Frames put out per process call at most, the scheduler comes back for more.
const BURST: usize = 64;
/// ADTS frames a decoder runs ahead of a seek, one for the MDCT overlap and one more for SBR.
const AAC_PREROLL: u64 = 2;
/// AC-3 and DTS frames overlap their neighbour's last block, one frame ahead fills it.
const SYNC_PREROLL: u64 = 1;
/// Read past the reader's window while frames go out, so the next fill is in flight.
const AHEAD: u64 = CHUNK;
const OPEN: u64 = u64::MAX;

static PCM_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: FIELD_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_SAMPLE, allowed: ConstraintDesc::Any, preferred: None },
    LAYOUT_FIELD,
];
const PCM_OFFER: OfferDesc = OfferDesc { family: profluens_audio::FAMILY, fields: &PCM_FIELDS };

static FLAC: [OfferDesc; 1] = [OfferDesc::any("flac")];
static MP3: [OfferDesc; 1] = [OfferDesc::any("mp3")];
static AAC: [OfferDesc; 1] = [OfferDesc::any("aac")];
static AC3: [OfferDesc; 1] = [OfferDesc::any("ac3")];
static EAC3: [OfferDesc; 1] = [OfferDesc::any("eac3")];
static DTS: [OfferDesc; 1] = [OfferDesc::any("dts")];
static PCM: [OfferDesc; 1] = [PCM_OFFER];
static ANY: [OfferDesc; 7] = [
    OfferDesc::any("flac"),
    OfferDesc::any("mp3"),
    OfferDesc::any("aac"),
    OfferDesc::any("ac3"),
    OfferDesc::any("eac3"),
    OfferDesc::any("dts"),
    PCM_OFFER,
];
static FLAC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &FLAC }];
static MP3_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &MP3 }];
static AAC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &AAC }];
static AC3_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &AC3 }];
static EAC3_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &EAC3 }];
static DTS_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &DTS }];
static PCM_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &PCM }];
static ANY_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &ANY }];

const fn desc(srcs: &'static [PadDesc]) -> ElementDesc {
    ElementDesc {
        name: "baresrc",
        sinks: &[],
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::None,
        latency: LatencyDesc::NONE,
        io: true,
        make_default: None,
        couplings: &[],
    }
}

static FLAC_DESC: ElementDesc = desc(&FLAC_PADS);
static MP3_DESC: ElementDesc = desc(&MP3_PADS);
static AAC_DESC: ElementDesc = desc(&AAC_PADS);
static AC3_DESC: ElementDesc = desc(&AC3_PADS);
static EAC3_DESC: ElementDesc = desc(&EAC3_PADS);
static DTS_DESC: ElementDesc = desc(&DTS_PADS);
static PCM_DESC: ElementDesc = desc(&PCM_PADS);
/// What the registry lists, a running element offers its file's one family.
pub(crate) static REGISTRY_DESC: ElementDesc = desc(&ANY_PADS);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareSrcError {
    /// Read before `start` or after `stop`.
    NotStarted,
    Read(BareError),
    /// The reactor refused the file.
    Open(IoError),
    /// The read at byte `offset` failed.
    Io {
        offset: u64,
        error: IoError,
    },
}

impl fmt::Display for BareSrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BareSrcError::NotStarted => write!(f, "baresrc: not started"),
            BareSrcError::Read(e) => write!(f, "baresrc: {e}"),
            BareSrcError::Open(e) => write!(f, "baresrc: the file cannot be served, {e}"),
            BareSrcError::Io { offset, error } => {
                write!(f, "baresrc: the read at byte {offset} failed, {error}")
            }
        }
    }
}

impl std::error::Error for BareSrcError {}

/// Whether the codec head went out since the start or the seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Head,
    Frames,
}

/// A Need of a parked machine and the bytes of it copied so far.
#[derive(Debug, Clone, Copy)]
struct Wait {
    need: Need,
    got: usize,
}

/// Where a run is.
#[derive(Debug, Clone, Copy)]
enum Run {
    /// A seek to the time, the reader not moved yet.
    Pending(i64),
    /// A seek waiting on a probe read.
    Seeking(SeekPark, Wait),
    /// Frames go out, the next one waiting on its read when parked.
    Ready(Option<(NextPark, Wait)>),
    Ended,
}

enum Pumped<T> {
    Done(T),
    Wait(Wait),
}

#[derive(Default)]
struct Span {
    off: u64,
    len: u32,
    got: u32,
    landed: bool,
    buf: Option<Buffer>,
}

/// The reads of a run, one contiguous stretch of the file from the oldest live span to `next`.
struct Reads {
    file: Handle,
    /// Spans by `seq`, the live ones `head..seq`. A power of two long.
    ring: Vec<Span>,
    mask: u64,
    /// Completions of a `seq` below this were issued before a seek or a jump and are dropped.
    head: u64,
    seq: u64,
    in_flight: u32,
    next: u64,
    /// Where the reader's window ends, the end of the last Need answered.
    frontier: u64,
    /// The audio ends here, nothing past it is read.
    end: u64,
    reads: u64,
    bytes: u64,
}

impl Reads {
    // Cold, empty until the start sizes it.
    #[allow(clippy::disallowed_methods)]
    fn new(end: u64) -> Reads {
        Reads {
            file: Handle::NONE,
            ring: Vec::new(),
            mask: 0,
            head: 0,
            seq: 0,
            in_flight: 0,
            next: 0,
            frontier: 0,
            end,
            reads: 0,
            bytes: 0,
        }
    }

    /// Lets go of every span, the reads in flight fall below `head`.
    fn drop_from(&mut self, at: u64) {
        for s in &mut self.ring {
            (s.buf, s.landed) = (None, false); // buffers recycle on drop
        }
        (self.head, self.next) = (self.seq, at);
    }

    /// Keeps the spans when `p` is in their stretch, else starts a new one at `p`.
    fn align(&mut self, p: u64) {
        let start = match self.head < self.seq {
            true => self.ring[(self.head & self.mask) as usize].off,
            false => self.next,
        };
        if p < start || p > self.next {
            self.drop_from(p);
        }
    }

    /// Copies what landed of `w` into `hole`, true once all of it is there or the file ended.
    fn serve(&mut self, w: &mut Wait, hole: &mut [u8]) -> bool {
        let to = w.need.at + w.need.len as u64;
        let mut p = w.need.at + w.got as u64;
        self.align(p);
        while p < to && self.head < self.seq {
            let s = &mut self.ring[(self.head & self.mask) as usize];
            let (stop, have) = (s.off + u64::from(s.len), s.off + u64::from(s.got));
            if p < stop && !s.landed {
                return false;
            }
            if p < have
                && let Some(buf) = &s.buf
            {
                let (from, k) = ((p - s.off) as usize, (have.min(to) - p) as usize);
                hole[w.got..w.got + k].copy_from_slice(&buf.memory.data()[from..from + k]);
                (w.got, p) = (w.got + k, p + k as u64);
            }
            if p >= stop {
                (s.buf, s.landed) = (None, false);
                self.head += 1;
            } else if p < to {
                // landed short, the file ends at `p`
                self.frontier = p;
                return true;
            }
        }
        if p < to {
            return false;
        }
        self.frontier = to;
        true
    }

    /// Reads from `from` to `limit`, whole buffers when `whole`, while credits and the pool last.
    fn issue(&mut self, ctx: &mut Ctx, from: u64, limit: u64, whole: bool) {
        let limit = limit.min(self.end);
        self.align(from);
        let credits = ctx.io().credits();
        while self.next < limit
            && self.in_flight < credits
            && self.seq - self.head < self.ring.len() as u64
        {
            let Some(buf) = ctx.try_alloc() else {
                break; // pool dry, backpressure
            };
            let cap = buf.memory.capacity() as u64;
            let want = if whole { cap } else { cap.min(limit - self.next) };
            let len = want.min(self.end - self.next).min(u64::from(u32::MAX)) as u32;
            let seq = self.seq;
            self.ring[(seq & self.mask) as usize] =
                Span { off: self.next, len, got: 0, landed: false, buf: None };
            ctx.io().read_range(self.file, self.next, 0, len, buf, seq);
            (self.seq, self.next, self.in_flight) =
                (seq + 1, self.next + len as u64, self.in_flight + 1);
            self.reads += 1;
        }
    }

    fn land(&mut self, ctx: &mut Ctx) -> Result<(), BareSrcError> {
        while let Some(c) = ctx.io().next_completion() {
            if c.user == OPEN {
                match c.result {
                    IoResult::File { handle, len } => {
                        self.file = handle;
                        log!(&*ctx, Level::Debug, "open", len = len);
                    }
                    IoResult::Err(e) => return Err(BareSrcError::Open(e)),
                    _ => {}
                }
                continue;
            }
            self.in_flight = self.in_flight.saturating_sub(1);
            if c.user < self.head || c.user >= self.seq {
                continue; // issued before a seek or a jump, its buffer recycles
            }
            let s = &mut self.ring[(c.user & self.mask) as usize];
            match (c.result, c.buf) {
                (IoResult::Ok(n), Some(buf)) => {
                    self.bytes += n as u64;
                    s.got = buf.memory.len().min(s.len as usize) as u32;
                    if n > 0 && s.got < s.len {
                        let (off, got) = (s.off + u64::from(s.got), s.got);
                        ctx.io().read_range(self.file, off, got, s.len - got, buf, c.user);
                        (self.in_flight, self.reads) = (self.in_flight + 1, self.reads + 1);
                    } else {
                        (s.buf, s.landed) = (Some(buf), true);
                    }
                }
                (IoResult::Err(error), _) => {
                    return Err(BareSrcError::Io { offset: s.off + u64::from(s.got), error });
                }
                _ => {
                    // the scheduler does not cancel yet, read the stretch again from here
                    log!(&*ctx, Level::Warn, "read_cancelled", offset = s.off);
                    let at = s.off;
                    self.drop_from(at);
                }
            }
        }
        Ok(())
    }
}

/// Polls `m`, answering its reads from the landed spans, until it is done or waits on a read.
fn pump<F: Fetch<Err = BareError>>(
    m: &mut F,
    reads: &mut Reads,
    mut wait: Option<Wait>,
) -> Result<Pumped<F::Out>, BareError> {
    loop {
        let mut w = match wait.take() {
            Some(w) => w,
            None => match m.poll()? {
                Step::Done(out) => return Ok(Pumped::Done(out)),
                Step::Need(need) => Wait { need, got: 0 },
            },
        };
        if !reads.serve(&mut w, m.buf()) {
            return Ok(Pumped::Wait(w));
        }
        m.filled(w.got);
    }
}

pub struct BareSrc {
    // Held for the reactor to duplicate, a confined open must not be resolved again.
    #[allow(clippy::disallowed_types)]
    file: File,
    head: BareHead,
    len: u64,
    started: bool,
    rd: Reader,
    phase: Phase,
    run: Run,
    reads: Reads,
    /// A frame read that no buffer took yet.
    held: Option<Unit>,
    /// See [`BareSrc::for_decoder`].
    decoder: bool,
}

impl BareSrc {
    pub const OUT: SrcPad = SrcPad(0);

    /// Reads `file`, already open and of `len` bytes, whose head [`crate::open`] read.
    #[allow(clippy::disallowed_types)]
    pub fn new(file: File, head: BareHead, len: u64) -> Self {
        let (rd, reads) = (Reader::new(&head), Reads::new(head.end));
        BareSrc {
            file,
            head,
            len,
            started: false,
            rd,
            phase: Phase::Head,
            run: Run::Ready(None),
            reads,
            held: None,
            decoder: false,
        }
    }

    /// A decoder takes the frames. The format declares the LAME delay and the decoder's own for
    /// it to cut, as a decoder reading the file from byte 0 does, a seek starts the frames back
    /// that the bit reservoir or the MDCT overlap needs, and the index is final and ends where
    /// the PCM does.
    pub fn for_decoder(mut self) -> Self {
        self.decoder = true;
        self
    }

    /// Samples the decoder cuts off the front.
    fn delay(&self) -> u64 {
        match self.head.codec {
            Codec::Mpa { .. } if self.decoder && self.head.gapless != (0, 0) => {
                u64::from(self.head.gapless.0) + pf_mp3::index::DECODER_DELAY
            }
            _ => 0,
        }
    }

    /// Frames a seek starts ahead of its target.
    fn lead_frames(&self) -> u64 {
        match self.head.codec {
            _ if !self.decoder => 0,
            Codec::Mpa { stream } => pf_mp3::index::preroll_frames(&stream, self.head.cbr),
            Codec::Adts { .. } => AAC_PREROLL,
            Codec::Sync { .. } => SYNC_PREROLL,
            Codec::Flac { .. } | Codec::Pcm { .. } => 0,
        }
    }

    /// How long the PCM a decoder puts out lasts, 0 unknown.
    fn duration_ns(&self) -> i64 {
        let (h, delay) = (&self.head, self.delay());
        match (delay, h.samples) {
            (0, _) => h.duration_ns,
            (d, 0) => h.duration_ns.saturating_sub(h.ns(d)).max(0),
            (d, n) => h.ns(n.saturating_sub(d)),
        }
    }

    fn fail(ctx: &Ctx, e: BareSrcError) -> Error {
        Error::element_failed(ctx.element(), e)
    }

    fn announce(&self, ctx: &mut Ctx) {
        let delay = self.delay();
        let mut fields = [
            (FIELD_RATE, ValueDesc::Int(i64::from(self.head.rate))),
            (FIELD_CHANNELS, ValueDesc::Int(i64::from(self.head.channels()))),
            (profluens_audio::FIELD_DELAY, ValueDesc::Int(self.head.ns(delay))),
            (FIELD_LAYOUT, ValueDesc::Int(0)),
        ];
        let n = match self.head.codec {
            Codec::Pcm { sample, channels, .. } => {
                fields[1].1 = ValueDesc::Int(i64::from(channels));
                fields[2] = (FIELD_SAMPLE, ValueDesc::Id(sample.caps_name()));
                let layout = self.head.layout();
                fields[3].1 = ValueDesc::Int(layout as i64);
                if layout > 0 { 4 } else { 3 }
            }
            _ if delay > 0 => 3,
            _ => 2,
        };
        ctx.announce_format(Self::OUT, self.head.family, &fields[..n]);
    }

    /// Moves the reader to a pending seek's frame, false while a probe read is out.
    fn seek(&mut self) -> Result<bool, BareError> {
        let (mut m, wait) = match self.run {
            Run::Pending(ns) => (self.rd.seek(&self.head, ns), None),
            Run::Seeking(p, w) => (self.rd.resume_seek(&self.head, p), Some(w)),
            _ => return Ok(true),
        };
        match pump(&mut m, &mut self.reads, wait)? {
            Pumped::Wait(w) => {
                self.run = Run::Seeking(m.park(), w);
                Ok(false)
            }
            Pumped::Done(()) => {
                self.run = Run::Ready(None);
                Ok(true)
            }
        }
    }

    /// The next frame, the held one first, None while its read is out.
    fn frame(&mut self) -> Result<Option<Unit>, BareError> {
        if let Some(u) = self.held.take() {
            return Ok(Some(u));
        }
        let (mut m, wait) = match self.run {
            Run::Ready(Some((p, w))) => (self.rd.resume_next(&self.head, p), Some(w)),
            Run::Ready(None) => (self.rd.next(&self.head), None),
            Run::Pending(_) | Run::Seeking(..) | Run::Ended => return Ok(None),
        };
        match pump(&mut m, &mut self.reads, wait)? {
            Pumped::Wait(w) => {
                self.run = Run::Ready(Some((m.park(), w)));
                Ok(None)
            }
            Pumped::Done(None) => {
                self.run = Run::Ended;
                self.reads.drop_from(self.head.end);
                Ok(None)
            }
            Pumped::Done(Some(u)) => {
                self.run = Run::Ready(None);
                Ok(Some(u))
            }
        }
    }

    fn frames(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        for _ in 0..BURST {
            let Some(u) = self.frame().map_err(|e| Self::fail(ctx, BareSrcError::Read(e)))? else {
                return Ok(());
            };
            if self.phase == Phase::Head {
                self.announce(ctx);
                if !self.head.head.is_empty() {
                    let n = self.head.head.len();
                    let Some(mut b) = ctx.alloc_exact(n) else { return Err(ctx.exact_refused(n)) };
                    b.memory.as_mut_full()[..n].copy_from_slice(&self.head.head);
                    b.memory.set_len(n);
                    (b.pts, b.flags) = (Timestamp(u.pts_ns.max(0) as u64), BufferFlags::HEADER);
                    ctx.out(Self::OUT).push(b);
                }
                self.phase = Phase::Frames;
            }
            let bytes = self.rd.bytes(&u);
            let mut b = match ctx.try_alloc_sized(bytes.len()) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    // the pool is dry, the frame waits
                    self.held = Some(u);
                    return Ok(());
                }
                Err(_) => match ctx.alloc_exact(bytes.len()) {
                    Some(b) => b,
                    None => return Err(ctx.exact_refused(bytes.len())),
                },
            };
            b.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
            b.memory.set_len(bytes.len());
            b.pts = Timestamp(u.pts_ns.max(0) as u64);
            b.duration = Timestamp(u64::from(u.dur_ns));
            b.flags = BufferFlags::KEYFRAME;
            ctx.out(Self::OUT).push(b);
        }
        Ok(())
    }

    /// The reads the run waits on, and the next chunk while frames go out.
    fn read_on(&mut self, ctx: &mut Ctx) {
        let r = &mut self.reads;
        match self.run {
            Run::Seeking(_, w) => {
                let to = w.need.at + w.need.len as u64;
                r.issue(ctx, w.need.at + w.got as u64, to, false);
            }
            Run::Ready(Some((_, w))) => {
                let to = w.need.at + w.need.len as u64;
                r.issue(ctx, w.need.at + w.got as u64, to + AHEAD, true);
            }
            Run::Ready(None) => {
                let at = r.frontier;
                r.issue(ctx, at, at + AHEAD, true);
            }
            Run::Pending(_) | Run::Ended => {}
        }
    }
}

impl Element for BareSrc {
    fn desc(&self) -> &'static ElementDesc {
        match self.head.family {
            "flac" => &FLAC_DESC,
            "mp3" => &MP3_DESC,
            "ac3" => &AC3_DESC,
            "eac3" => &EAC3_DESC,
            "dts" => &DTS_DESC,
            profluens_audio::FAMILY => &PCM_DESC,
            _ => &AAC_DESC,
        }
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        let duration = u64::try_from(self.duration_ns()).ok().filter(|&d| d > 0);
        ctx.publish_index(Index {
            file_len: Some(self.len),
            duration: duration.map_or(Timestamp::NONE, Timestamp),
            // nothing is added later, a reader of the index need not wait on it
            complete: self.decoder && duration.is_some(),
            ..Index::default()
        });
        if let Some(d) = duration {
            ctx.set_duration(d);
        }
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.started = true;
        (self.rd, self.phase, self.run, self.held) =
            (Reader::new(&self.head), Phase::Head, Run::Ready(None), None);
        // A restart reads from the first frame. Reads left by the old run fall below `head`
        // and no longer count against `in_flight`.
        let r = &mut self.reads;
        r.drop_from(self.head.first);
        (r.file, r.in_flight, r.frontier) = (Handle::NONE, 0, self.head.first);
        ctx.io().adopt(self.file.as_fd(), OpenMode::Read, OPEN);
        // Cold, sized once per run to the in-flight read budget.
        #[allow(clippy::disallowed_methods)]
        {
            let depth = (ctx.io().credits() as usize).max(1).next_power_of_two();
            r.ring.resize_with(depth, Span::default);
            r.mask = depth as u64 - 1;
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.started {
            return Err(Self::fail(ctx, BareSrcError::NotStarted));
        }
        self.reads.land(ctx).map_err(|e| Self::fail(ctx, e))?;
        if self.reads.file.is_none() {
            return Ok(Flow::Ok); // the adopt is in flight
        }
        if self.seek().map_err(|e| Self::fail(ctx, BareSrcError::Read(e)))? {
            self.frames(ctx)?;
        }
        self.read_on(ctx);
        match self.run {
            Run::Ended if self.reads.in_flight == 0 => {
                let (reads, bytes) = (self.reads.reads, self.reads.bytes);
                log!(&*ctx, Level::Info, "eos", reads = reads, bytes = bytes);
                Ok(Flow::Eos)
            }
            _ => Ok(Flow::Ok),
        }
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if !matches!(event, Event::FlushStart) || !self.started {
            return Ok(());
        }
        let at = ctx.seek_target().and_then(|t| t.at.nanos()).unwrap_or(0);
        let at = at.min(i64::MAX as u64) as i64;
        let back = self.lead_frames() * u64::from(self.head.spf);
        let at =
            if back > 0 { self.head.ns(self.head.sample_at(at).saturating_sub(back)) } else { at };
        // The reads in flight complete at the old offsets and fall below `head`.
        log!(&*ctx, Level::Debug, "seek", at = at, in_flight = self.reads.in_flight);
        self.reads.drop_from(self.reads.next);
        (self.run, self.phase, self.held) = (Run::Pending(at), Phase::Head, None);
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.started = false; // the reactor closes its file when the group ends
    }
}
