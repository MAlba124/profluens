//! `AviDemux`, the element over the [`crate::riff`] parser with one src pad per stream.

use std::collections::VecDeque;
use std::sync::Arc;

use profluens_audio::{AudioFormat, announce_raw};
use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::pipeline::Index;
use profluens_core::pull::{Fed, Feed};
use profluens_core::rate;
use profluens_core::time::Timestamp;

use crate::codec;
use crate::index::{self, AviIndex, Scan};
use crate::retime::{Act, Order, Retime};
use crate::riff::{self, AviError, ChunkHead, FourCc, MoviWalker, Stream, StreamKind};

/// Most of `idx1` asked for at a time, its 8 octet header in the first piece.
pub const IDX1_PIECE: u32 = 64 << 10;

/// Most of an `idx1` body read, a million chunks or about nine hours of film.
pub const MAX_IDX1: u64 = 16 << 20;

/// Chunks between the end of `movi` and `idx1` hopped over.
pub const MAX_HOPS: u8 = 4;

const WINDOW: u32 = 256 << 10;

const IDX1_TAG: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Idx1Read {
    Off,
    Want { at: u64, body: Option<Body>, hops: u8 },
    Asked { at: u64, len: u32, body: Option<Body>, hops: u8 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Body {
    end: u64,
    whole: bool,
}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static DEMUX_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];

pub(crate) static DEMUX_DESC: ElementDesc = ElementDesc {
    name: "avidemux",
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

struct PadStream {
    stream_index: usize,
    pad: SrcPad,
    family: &'static str,
    stream: Stream,
    sample_dur_ns: Option<u64>,
    ticks: u64,
    gate: u64,
    started: bool,
    /// The head owed ahead of the stream's next chunk, its pad linked mid-run.
    greet: bool,
    /// The file byte of the stream's last chunk taken, and up to where a restart drops what it
    /// reads again, the stream had it out already.
    read_to: Option<u64>,
    resume: Option<u64>,
    is_video: bool,
    warned_undecodable: bool,
    /// Bytes the framer skipped between frames.
    junk: u64,
    /// The stream's configuration sent as a HEADER buffer at each start, an MPEG-4 VOL, else
    /// empty.
    head: Vec<u8>,
    /// Video that presents out of decode order, its units held until stamped.
    reorder: Option<Reorder>,
}

/// A video stream's units on their way out, held in decode order until their presentation
/// stamps are known (`Retime`), a packed B-VOP stashed for the chunk after it. Held as copies
/// outside the pool, a pool buffer pinned while it waits would starve a small pool, the next
/// chunk could never be taken to free it.
struct Reorder {
    rt: Retime,
    /// The held units' bytes, `Span`s of it, compacted once its dead front outgrows the rest.
    arena: Vec<u8>,
    held: VecDeque<Span>,
    stash: Option<Span>,
    acts: Vec<Act>,
}

#[derive(Clone, Copy)]
struct Span {
    at: usize,
    len: usize,
    flags: BufferFlags,
}

impl Reorder {
    // Cold path, once per stream at preroll.
    #[allow(clippy::disallowed_methods)]
    fn new(rt: Retime) -> Self {
        Self {
            rt,
            arena: Vec::new(),
            held: VecDeque::with_capacity(crate::retime::MOST_HELD + 4),
            stash: None,
            acts: Vec::with_capacity(8),
        }
    }

    fn reset(&mut self) {
        self.rt.reset();
        self.arena.clear();
        self.held.clear();
        self.stash = None;
    }

    /// Takes the chunk `data` at `slot` ns, its key flag in `flags`.
    fn push(&mut self, data: &[u8], slot: u64, flags: BufferFlags) {
        self.acts.clear();
        self.rt.chunk(data, slot, &mut self.acts);
        let Self { arena, held, stash, acts, .. } = self;
        let mut keep = |from: usize, to: usize, flags: BufferFlags| {
            let at = arena.len();
            arena.extend_from_slice(data.get(from..to).unwrap_or_default());
            Span { at, len: arena.len() - at, flags }
        };
        for act in acts.iter() {
            match *act {
                Act::Unit { from, to, key } => {
                    let flags = match key {
                        Some(true) => BufferFlags::KEYFRAME,
                        Some(false) => BufferFlags::empty(),
                        None => flags,
                    };
                    held.push_back(keep(from, to, flags));
                }
                Act::Unstash => held.extend(stash.take()),
                Act::Stash { from, to } => *stash = Some(keep(from, to, BufferFlags::empty())),
                Act::Drop => *stash = None,
            }
        }
    }

    /// Whether a stamped unit waits for the pool.
    fn backed_up(&self) -> bool {
        self.rt.peek().is_some()
    }

    /// The stream ended, every unit is stamped.
    fn finish(&mut self) {
        self.acts.clear();
        self.rt.finish(&mut self.acts);
        if self.acts.contains(&Act::Unstash) {
            self.held.extend(self.stash.take());
        }
    }

    /// The stamped units out on `pad` in decode order, false when the pool ran dry first. An
    /// `exact` one takes buffers past the pool, at the end.
    fn out(&mut self, ctx: &mut Ctx, pad: SrcPad, exact: bool) -> bool {
        while let Some(pts) = self.rt.peek() {
            let Some(span) = self.held.front().copied() else { break };
            let bytes = self.arena.get(span.at..span.at + span.len).unwrap_or_default();
            let buf = match exact {
                true => ctx.alloc_exact(bytes.len()),
                false => match ctx.try_alloc_sized(bytes.len()) {
                    Ok(Some(buf)) => Some(buf),
                    Ok(None) => return false,
                    Err(e) => {
                        let element = ctx.element();
                        ctx.post(BusMessage::Warning {
                            element,
                            error: Error::element_failed(element, e),
                        });
                        None
                    }
                },
            };
            if let Some(mut buf) = buf {
                buf.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
                buf.memory.set_len(bytes.len());
                buf.pts = Timestamp::from_nanos(pts);
                buf.flags = span.flags;
                ctx.out(pad).push(buf);
            }
            self.rt.take();
            self.held.pop_front();
        }
        self.compact();
        true
    }

    fn compact(&mut self) {
        let live =
            self.held.front().map(|s| s.at).into_iter().chain(self.stash.map(|s| s.at)).min();
        let Some(live) = live else {
            self.arena.clear();
            return;
        };
        if live > 0 && live >= self.arena.len() / 2 {
            self.arena.copy_within(live.., 0);
            self.arena.truncate(self.arena.len() - live);
            self.held.iter_mut().chain(self.stash.as_mut()).for_each(|s| s.at -= live);
        }
    }
}

impl PadStream {
    fn next_pts_ns(&mut self, payload_size: usize) -> Option<u64> {
        let dur = self.sample_dur_ns?;
        let pts = self.ticks.saturating_mul(dur);
        self.ticks = self.ticks.saturating_add(index::ticks(self.stream.sample_size, payload_size));
        Some(pts)
    }
}

/// Where a unit's bytes stay until it is out, so they go straight into a pool buffer.
#[derive(Clone, Copy)]
enum Spot {
    /// The chunk at the walker's head.
    Walker(ChunkHead),
    /// The next frame of the framer of the stream at this place in `pad_streams`.
    Framer(usize),
}

#[derive(Clone, Copy)]
struct Unit {
    stream_index: usize,
    id: FourCc,
    spot: Spot,
}

/// A unit routed but not all out, the pool ran dry.
struct Carry {
    pad: SrcPad,
    pts_ns: Option<u64>,
    flags: BufferFlags,
    spot: Spot,
    off: usize,
}

fn reorder_of(streams: &mut [PadStream], pad: SrcPad) -> Option<&mut Reorder> {
    streams.iter_mut().find(|ps| ps.pad == pad)?.reorder.as_mut()
}

fn bytes_of<'a>(
    walker: &'a MoviWalker,
    framers: &'a [Option<codec::Framer>],
    spot: Spot,
) -> &'a [u8] {
    match spot {
        Spot::Walker(c) => walker.data(&c),
        Spot::Framer(i) => framers.get(i).and_then(|f| f.as_ref()?.frame()).unwrap_or_default(),
    }
}

/// Demultiplexes an AVI (RIFF) byte stream into one src pad per stream.
pub struct AviDemux {
    header: Vec<u8>,
    /// Codec heads given for streams by index, ahead of what the header holds.
    heads: Vec<(usize, Vec<u8>)>,
    pad_streams: Vec<PadStream>,
    movi_data_start: u64,
    skipped: u64,
    in_movi: bool,
    walker: MoviWalker,
    /// Each stream's framer by its place in `pad_streams`, for streams that slice by bytes.
    framers: Vec<Option<codec::Framer>>,
    /// The framer holding frames of the last chunk, with that chunk's stream and id.
    framing: Option<(usize, usize, FourCc)>,
    carry: Option<Carry>,
    posted_duration: bool,
    duration_ns: Option<u64>,
    file_len: Option<u64>,
    feed: Option<Feed>,
    read: Idx1Read,
    scan: Option<Scan>,
    /// The index read once for the file, so this run reads no `idx1`.
    shared: Option<Arc<AviIndex>>,
    whole: bool,
    piece: u32,
    rated: Index,
    key: usize,
    taken: u64,
    measured: rate::ByteRate,
    teller: rate::Teller,
    /// Chunks of no stream in the header, and the resync and framer junk counts last logged.
    strays: u64,
    seen: (u64, u64),
    /// The file byte at a walker count, where its chunks lie, unknown behind a flush to nowhere.
    origin: Option<(u64, u64)>,
}

impl AviDemux {
    pub const IN: SinkPad = SinkPad(0);

    /// A demuxer that discovers its streams from `header`, which must hold the whole `hdrl` list.
    // Cold path, one-time constructor state.
    #[allow(clippy::disallowed_methods)]
    pub fn new(header: Vec<u8>) -> Self {
        Self {
            header,
            heads: Vec::new(),
            pad_streams: Vec::new(),
            movi_data_start: 0,
            skipped: 0,
            in_movi: false,
            walker: MoviWalker::new(),
            framers: Vec::new(),
            framing: None,
            carry: None,
            posted_duration: false,
            duration_ns: None,
            file_len: None,
            feed: None,
            read: Idx1Read::Off,
            scan: None,
            shared: None,
            whole: false,
            piece: IDX1_PIECE,
            rated: Index::default(),
            key: 0,
            taken: 0,
            measured: rate::ByteRate::default(),
            teller: rate::Teller::default(),
            strays: 0,
            seen: (0, 0),
            origin: None,
        }
    }

    /// Tells the demuxer the source's length, `None` where it is unknown.
    pub fn with_len(mut self, len: Option<u64>) -> Self {
        self.file_len = len;
        self
    }

    /// The codec head of stream `stream_index`, what an opener read past the header, an MPEG-4
    /// VOL from the first key chunk. Sent as a HEADER buffer at each start of the stream.
    pub fn with_codec_head(mut self, stream_index: usize, head: Vec<u8>) -> Self {
        self.heads.retain(|(i, _)| *i != stream_index);
        if !head.is_empty() {
            self.heads.push((stream_index, head));
        }
        self
    }

    /// Publishes `index` at preroll and reads no `idx1`, one index shared by the runs over a
    /// file.
    pub fn with_index(mut self, index: Arc<AviIndex>) -> Self {
        self.shared = Some(index);
        self
    }

    /// The streams discovered during `preroll`.
    pub fn streams(&self) -> Vec<&Stream> {
        self.pad_streams.iter().map(|ps| &ps.stream).collect()
    }

    /// The src pad a given stream number routes to, if that stream was discovered.
    pub fn pad_for(&self, stream_index: usize) -> Option<SrcPad> {
        self.pad_streams.iter().find(|ps| ps.stream_index == stream_index).map(|ps| ps.pad)
    }

    /// Emits `bytes[off..]` on `pad`, through the stream's reorder when it has one. The offset
    /// reached, `off` when the pool is dry.
    fn emit_bounded(
        ctx: &mut Ctx,
        (pad, reorder): (SrcPad, Option<&mut Reorder>),
        pts_ns: Option<u64>,
        flags: BufferFlags,
        bytes: &[u8],
        off: usize,
    ) -> usize {
        let rest = &bytes[off..];
        if rest.is_empty() {
            return bytes.len();
        }
        if let (Some(r), Some(slot)) = (reorder, pts_ns) {
            r.push(rest, slot, flags);
            r.out(ctx, pad, false);
            return bytes.len();
        }
        let mut buf = match ctx.try_alloc_sized(rest.len()) {
            Ok(Some(buf)) => buf,
            Ok(None) => return off,
            Err(e) => {
                let element = ctx.element();
                ctx.post(BusMessage::Warning { element, error: Error::element_failed(element, e) });
                return bytes.len();
            }
        };
        buf.memory.as_mut_full()[..rest.len()].copy_from_slice(rest);
        buf.memory.set_len(rest.len());
        Self::send(ctx, pad, buf, pts_ns, flags);
        bytes.len()
    }

    fn send(ctx: &mut Ctx, pad: SrcPad, mut buf: Buffer, pts_ns: Option<u64>, flags: BufferFlags) {
        if let Some(t) = pts_ns {
            buf.pts = Timestamp::from_nanos(t);
        }
        buf.flags = flags;
        ctx.out(pad).push(buf);
    }

    fn emit_exact(
        ctx: &mut Ctx,
        (pad, reorder): (SrcPad, Option<&mut Reorder>),
        pts_ns: Option<u64>,
        flags: BufferFlags,
        bytes: &[u8],
        off: usize,
    ) {
        let rest = &bytes[off..];
        if rest.is_empty() {
            return;
        }
        if let (Some(r), Some(slot)) = (reorder, pts_ns) {
            r.push(rest, slot, flags);
            r.out(ctx, pad, true);
            return;
        }
        let Some(mut buf) = ctx.alloc_exact(rest.len()) else {
            let (element, error) = (ctx.element(), ctx.exact_refused(rest.len()));
            ctx.post(BusMessage::Warning { element, error });
            return;
        };
        buf.memory.as_mut_full()[..rest.len()].copy_from_slice(rest);
        buf.memory.set_len(rest.len());
        Self::send(ctx, pad, buf, pts_ns, flags);
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> bool {
        for ps in &mut self.pad_streams {
            if let Some(r) = &mut ps.reorder
                && !r.out(ctx, ps.pad, false)
            {
                return false;
            }
        }
        let Some(mut c) = self.carry.take() else {
            return true;
        };
        let bytes = bytes_of(&self.walker, &self.framers, c.spot);
        let to = (c.pad, reorder_of(&mut self.pad_streams, c.pad));
        c.off = Self::emit_bounded(ctx, to, c.pts_ns, c.flags, bytes, c.off);
        if c.off < bytes.len() {
            self.carry = Some(c);
            return false;
        }
        self.pass(c.spot);
        true
    }

    /// Steps past a unit [`next_unit`](Self::next_unit) gave.
    fn pass(&mut self, spot: Spot) {
        match spot {
            Spot::Walker(c) => self.walker.take(&c),
            Spot::Framer(i) => {
                if let Some(f) = self.framers.get_mut(i).and_then(Option::as_mut) {
                    f.take();
                }
            }
        }
    }

    fn route(
        streams: &mut [PadStream],
        strays: &mut u64,
        ctx: &mut Ctx,
        unit: &Unit,
        data: &[u8],
    ) -> Option<(SrcPad, Option<u64>, BufferFlags, bool)> {
        let Some(idx) = streams.iter().position(|ps| ps.stream_index == unit.stream_index) else {
            let level = if *strays == 0 { Level::Warn } else { Level::Trace };
            log!(&*ctx, level, "chunk_stray", id = unit.id, bytes = data.len());
            *strays += 1;
            return None;
        };
        let ps = &mut streams[idx];
        let held = ps.ticks < ps.gate;
        let pts = ps.next_pts_ns(data.len());
        if held {
            return None;
        }
        if ps.head.is_empty()
            && ps.family == codec::FAMILY_MPEG4_ASP
            && let Some(vol) = codec::vol_prefix(data)
        {
            ps.head.extend_from_slice(vol);
        }
        let fresh = !ps.started;
        if fresh {
            ps.started = true;
            let (pad, family) = (ps.pad, ps.family);
            Self::announce(ctx, pad, family, &ps.stream);
            if !codec::is_decodable(family) && !ps.warned_undecodable {
                ps.warned_undecodable = true;
                let element = ctx.element();
                ctx.post(BusMessage::Warning {
                    element,
                    error: Error::element_failed(
                        element,
                        AviError::NoDecoderFamily { stream: unit.stream_index },
                    ),
                });
            }
        }
        let flags = if ps.is_video && codec::chunk_is_key(ps.family, data) {
            BufferFlags::KEYFRAME
        } else {
            BufferFlags::empty()
        };
        let greet = std::mem::take(&mut ps.greet) || fresh;
        Some((ps.pad, pts, flags, greet))
    }

    /// The stream's head on `pad` ahead of its first chunk of the run, stamped with it.
    fn greet(&self, ctx: &mut Ctx, pad: SrcPad, pts_ns: Option<u64>) {
        let Some(ps) = self.pad_streams.iter().find(|ps| ps.pad == pad) else { return };
        if !ps.head.is_empty() {
            Self::emit_exact(ctx, (pad, None), pts_ns, BufferFlags::HEADER, &ps.head, 0);
        }
    }

    /// The configuration of `stream` sent ahead of its chunks, the head given for it, else the
    /// MPEG-4 VOL of its `strf` or of its first chunk when the header bytes reach it. Empty for
    /// other families, their units carry what they need.
    // Cold path, once per stream at preroll.
    #[allow(clippy::disallowed_methods)]
    fn head_of(&self, stream: &Stream, family: &str, movi_data_start: u64) -> Vec<u8> {
        if let Some((_, h)) = self.heads.iter().find(|(i, _)| *i == stream.index) {
            return h.clone();
        }
        if family != codec::FAMILY_MPEG4_ASP {
            return Vec::new();
        }
        let in_movi = usize::try_from(movi_data_start)
            .ok()
            .and_then(|at| self.header.get(at..))
            .and_then(|movi| riff::first_chunk_of(movi, stream.index));
        codec::vol_prefix(stream.codec_head())
            .or_else(|| in_movi.and_then(codec::vol_prefix))
            .map_or_else(Vec::new, <[u8]>::to_vec)
    }

    /// The walker's and framers' first resync at warn, later ones at trace.
    fn note(&mut self, ctx: &Ctx) {
        let (resyncs, junk) = (self.walker.resyncs, self.walker.junk);
        if resyncs > self.seen.0 {
            let level = if self.seen.0 == 0 { Level::Warn } else { Level::Trace };
            log!(ctx, level, "resync", count = resyncs, junk = junk, near = self.taken);
        }
        let framed: u64 = self.pad_streams.iter().map(|ps| ps.junk).sum();
        if framed > self.seen.1 {
            let level = if self.seen.1 == 0 { Level::Warn } else { Level::Trace };
            log!(ctx, level, "frame_resync", junk = framed, near = self.taken);
        }
        self.seen = (resyncs, framed);
    }

    fn drain(&mut self, ctx: &mut Ctx) -> bool {
        self.note(ctx);
        if !self.drain_pending(ctx) {
            return false;
        }
        while let Some(unit) = self.next_unit() {
            let data = bytes_of(&self.walker, &self.framers, unit.spot);
            let routed = Self::route(&mut self.pad_streams, &mut self.strays, ctx, &unit, data);
            let Some((pad, pts, flags, fresh)) = routed else {
                self.pass(unit.spot);
                continue;
            };
            if fresh {
                self.greet(ctx, pad, pts);
            }
            let to = (pad, reorder_of(&mut self.pad_streams, pad));
            let off = Self::emit_bounded(ctx, to, pts, flags, data, 0);
            if off < data.len() {
                self.carry = Some(Carry { pad, pts_ns: pts, flags, spot: unit.spot, off });
                return false; // pool dry, stop pulling walker chunks
            }
            self.pass(unit.spot);
            if let (true, Some(pts)) = (unit.stream_index == self.key, pts) {
                self.key_out(ctx, Timestamp::from_nanos(pts));
            }
            // stamped units wait for the pool, no more chunks are taken in meanwhile
            if reorder_of(&mut self.pad_streams, pad).is_some_and(|r| r.backed_up()) {
                return false;
            }
        }
        true
    }

    fn flush_exact(&mut self, ctx: &mut Ctx) {
        if let Some(c) = self.carry.take() {
            let bytes = bytes_of(&self.walker, &self.framers, c.spot);
            let to = (c.pad, reorder_of(&mut self.pad_streams, c.pad));
            Self::emit_exact(ctx, to, c.pts_ns, c.flags, bytes, c.off);
            self.pass(c.spot);
        }
        while let Some(unit) = self.next_unit() {
            let data = bytes_of(&self.walker, &self.framers, unit.spot);
            let routed = Self::route(&mut self.pad_streams, &mut self.strays, ctx, &unit, data);
            if let Some((pad, pts, flags, fresh)) = routed {
                if fresh {
                    self.greet(ctx, pad, pts);
                }
                let to = (pad, reorder_of(&mut self.pad_streams, pad));
                Self::emit_exact(ctx, to, pts, flags, data, 0);
            }
            self.pass(unit.spot);
        }
        for ps in &mut self.pad_streams {
            if let Some(r) = &mut ps.reorder {
                r.finish();
                r.out(ctx, ps.pad, true);
                r.reset();
            }
        }
    }

    /// The next unit, left where it is until [`pass`](Self::pass), a framed stream's chunk cut
    /// into its frames first.
    fn next_unit(&mut self) -> Option<Unit> {
        loop {
            if let Some((i, stream_index, id)) = self.framing {
                let framer = self.framers.get(i).and_then(Option::as_ref);
                if framer.and_then(codec::Framer::frame).is_some() {
                    return Some(Unit { stream_index, id, spot: Spot::Framer(i) });
                }
                self.framing = None;
            }
            let chunk = self.walker.peek_chunk()?;
            let (stream_index, id) = (chunk.stream_index, chunk.id);
            let unit = Unit { stream_index, id, spot: Spot::Walker(chunk) };
            let Some(i) = self.pad_streams.iter().position(|ps| ps.stream_index == stream_index)
            else {
                return Some(unit);
            };
            let consumed = self.walker.consumed();
            let at = self.origin.and_then(|(byte, from)| Some(byte + consumed.checked_sub(from)?));
            let ps = &mut self.pad_streams[i];
            match (at, ps.resume) {
                (Some(at), Some(to)) if at <= to => {
                    self.walker.take(&chunk);
                    continue;
                }
                _ => (ps.resume, ps.read_to) = (None, at),
            }
            let Some(framer) = self.framers.get_mut(i).and_then(Option::as_mut) else {
                return Some(unit);
            };
            let dropped = framer.push(ps.family, self.walker.data(&chunk));
            self.walker.take(&chunk);
            ps.junk += dropped as u64;
            ps.ticks = ps.ticks.saturating_add(index::ticks(ps.stream.sample_size, dropped));
            self.framing = Some((i, stream_index, id));
        }
    }

    fn announce(ctx: &mut Ctx, pad: SrcPad, family: &'static str, stream: &Stream) {
        log!(
            &*ctx,
            Level::Debug,
            "announce",
            family = family,
            width = stream.width,
            height = stream.height,
        );
        match family {
            codec::FAMILY_AUDIO_RAW => {
                match codec::raw_format(stream.format_tag, stream.bits_per_sample) {
                    Some(sample) => {
                        let fmt = AudioFormat::new(stream.samples_per_sec, stream.channels, sample);
                        let layout = stream.layout();
                        announce_raw(ctx, pad, fmt, (layout > 0).then_some(layout));
                    }
                    None => ctx.announce_format(pad, family, &[]),
                }
            }
            codec::FAMILY_AC3 | codec::FAMILY_DTS | codec::FAMILY_MP3
                if stream.samples_per_sec > 0 =>
            {
                ctx.announce_format(
                    pad,
                    family,
                    &[
                        ("rate", ValueDesc::Int(stream.samples_per_sec as i64)),
                        ("channels", ValueDesc::Int(stream.channels as i64)),
                    ],
                );
            }
            _ if codec::is_video_family(family) && stream.width != 0 && stream.height != 0 => {
                ctx.announce_format(
                    pad,
                    family,
                    &[
                        ("width", ValueDesc::Int(stream.width as i64)),
                        ("height", ValueDesc::Int(stream.height as i64)),
                    ],
                );
            }
            _ => ctx.announce_format(pad, family, &[]),
        }
    }

    fn walk_in(&mut self, data: &[u8]) {
        self.taken += data.len() as u64;
        if self.in_movi {
            self.walker.push(data);
            return;
        }
        let end = self.skipped.saturating_add(data.len() as u64);
        if end <= self.movi_data_start {
            self.skipped = end;
            return;
        }
        let cut = (self.movi_data_start.saturating_sub(self.skipped)) as usize;
        let cut = cut.min(data.len());
        self.walker.push(&data[cut..]);
        self.skipped = end;
        self.in_movi = true;
    }

    fn restart_at(&mut self, byte: Option<u64>) {
        // A chunk held for the pool still goes out, as one taken from the walker would.
        let kept = match self.carry.as_ref().map(|c| c.spot) {
            Some(Spot::Walker(c)) => {
                self.walker.resync_keeping(&c);
                c.total() as u64
            }
            _ => {
                self.walker.resync();
                0
            }
        };
        let from = self.walker.consumed() + kept;
        self.origin = byte.map(|byte| (byte.max(self.movi_data_start), from));
        match byte {
            Some(byte) if byte < self.movi_data_start => {
                (self.in_movi, self.skipped) = (false, byte)
            }
            _ => self.in_movi = true,
        }
    }

    fn restart_pull(&mut self, byte: u64) {
        if let Some(feed) = &mut self.feed {
            feed.restart(byte);
        }
        self.restart_at(Some(byte));
        if let Idx1Read::Asked { at, body, hops, .. } = self.read {
            self.read = Idx1Read::Want { at, body, hops };
        }
    }

    // Cold path, at preroll and when the `idx1` read ends.
    #[allow(clippy::disallowed_methods)]
    fn publish(&mut self, ctx: &Ctx) {
        let scan = self.shared.as_ref().map(|i| &i.scan).or(self.scan.as_ref());
        let entries = match (scan, self.read) {
            (Some(scan), Idx1Read::Off) => scan.entries.clone(),
            _ => Vec::new(),
        };
        let complete = self.whole && !entries.is_empty();
        let grouped = self.pad_streams.iter().any(|ps| ps.is_video);
        let duration = self.duration_ns.map_or(Timestamp::NONE, Timestamp);
        self.rated = Index {
            entries,
            file_len: self.file_len,
            duration,
            complete,
            grouped,
            ..Index::default()
        };
        ctx.publish_index(self.rated.clone());
    }

    fn key_out(&mut self, ctx: &mut Ctx, at: Timestamp) {
        self.measured.note(self.taken - self.walker.held() as u64, at);
        let (index, measured) = (&self.rated, self.measured);
        self.teller.tell_at(ctx, Self::IN, at, |at| index::rate_at(index, &measured, at));
    }

    fn ask_idx1(&mut self, ctx: &mut Ctx, feed: &mut Feed) {
        let Idx1Read::Want { at, body, hops } = self.read else {
            return;
        };
        let end = body.map_or(self.file_len.unwrap_or(u64::MAX), |body| body.end);
        let len = u64::from(self.piece).min(end.saturating_sub(at)) as u32;
        if len == 0 {
            return self.idx1_over(ctx, body.is_some_and(|b| b.whole));
        }
        if feed.aside(ctx, Self::IN, at, len, IDX1_TAG) {
            self.read = Idx1Read::Asked { at, len, body, hops };
        }
    }

    fn idx1_in(&mut self, ctx: &Ctx, data: &[u8]) {
        let Idx1Read::Asked { at, len, body, hops } = self.read else {
            return;
        };
        let (body, from) = match body {
            Some(body) => (body, 0),
            None => {
                let Some(head) = data.get(..8) else {
                    return self.idx1_over(ctx, false);
                };
                let size = u64::from(u32::from_le_bytes([head[4], head[5], head[6], head[7]]));
                if &head[..4] != b"idx1" {
                    let next = at.saturating_add(8).saturating_add(size + (size & 1));
                    let plausible =
                        head[..4].iter().all(|b| b.is_ascii_alphanumeric() || *b == b' ');
                    let fits = self.file_len.is_none_or(|len| next.saturating_add(8) <= len);
                    if plausible && fits && hops < MAX_HOPS {
                        self.read = Idx1Read::Want { at: next, body: None, hops: hops + 1 };
                        return;
                    }
                    log!(ctx, Level::Warn, "idx1_absent", at = at, hops = hops);
                    return self.idx1_over(ctx, false);
                }
                let start = at + 8;
                let declared = start.saturating_add(size);
                let end = declared.min(start + MAX_IDX1).min(self.file_len.unwrap_or(u64::MAX));
                (Body { end, whole: end == declared }, 8)
            }
        };
        let upto = body.end.saturating_sub(at).min(data.len() as u64) as usize;
        let last = at + upto as u64 >= body.end;
        let entries = upto.saturating_sub(from);
        let upto = from + if last { entries } else { entries - entries % riff::IDX1_ENTRY };
        if let Some(scan) = &mut self.scan {
            scan.push(&data[from.min(upto)..upto]);
        }
        let next = at + upto as u64;
        let short = data.len() < len as usize;
        if last || short || upto <= from {
            return self.idx1_over(ctx, body.whole && last);
        }
        self.read = Idx1Read::Want { at: next, body: Some(body), hops };
    }

    fn idx1_over(&mut self, ctx: &Ctx, whole: bool) {
        (self.read, self.whole) = (Idx1Read::Off, whole);
        if let Some(scan) = &self.scan {
            log!(
                ctx,
                Level::Debug,
                "idx1",
                read = scan.read,
                keyframes = scan.entries.len(),
                cut = scan.cut,
                whole = whole
            );
            match (scan.entries.len(), whole) {
                (0, _) => log!(ctx, Level::Warn, "index_empty", read = scan.read),
                (n, false) => {
                    log!(ctx, Level::Warn, "index_partial", entries = n, read = scan.read)
                }
                (n, true) => log!(ctx, Level::Info, "index", entries = n),
            }
        }
        self.publish(ctx);
    }

    fn pull(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) {
        let Some(mut feed) = self.feed.take() else {
            return;
        };
        let mut held = !self.drain(ctx);
        while let Some(fed) = feed.take(ctx, Self::IN, inputs) {
            match fed {
                Fed::Window { buf, .. } => {
                    self.walk_in(buf.memory.data());
                    drop(buf);
                    held = held || !self.drain(ctx);
                }
                Fed::Aside { buf, .. } => self.idx1_in(ctx, buf.memory.data()),
            }
        }
        self.ask_idx1(ctx, &mut feed);
        match held {
            true => ctx.hold_end(),
            false => feed.ask(ctx, Self::IN),
        }
        self.feed = Some(feed);
    }
}

fn log_stream(ctx: &Ctx, s: &Stream, family: &'static str) {
    let index = s.index;
    match s.kind {
        _ if family == codec::FAMILY_BYTES => {
            let tag = s.format_tag;
            log!(ctx, Level::Warn, "stream_unknown", index = index, fourcc = s.handler, tag = tag);
        }
        StreamKind::Video => {
            let (w, h) = (s.width, s.height);
            log!(ctx, Level::Info, "stream", index = index, family = family, width = w, height = h)
        }
        _ => {
            let (r, c) = (s.samples_per_sec, s.channels);
            log!(ctx, Level::Info, "stream", index = index, family = family, rate = r, channels = c)
        }
    }
}

impl Element for AviDemux {
    fn desc(&self) -> &'static ElementDesc {
        &DEMUX_DESC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let (header, movi) = riff::probe_header(&self.header)
            .map_err(|e| Error::element_failed(ctx.element(), e))?;
        let movi =
            movi.ok_or_else(|| Error::element_failed(ctx.element(), AviError::MoviNotReached))?;
        self.movi_data_start = movi.movi_data_start;
        self.duration_ns = header.duration_ns();

        for stream in &header.streams {
            let family = codec::family_for(stream);
            let name = format!("src_stream{}", stream.index);
            let pad = ctx.add_src_pad(&name, codec::offers_for(stream));
            log_stream(ctx, stream, family);
            let head = self.head_of(stream, family, movi.movi_data_start);
            let framer = codec::reframes(family, stream.sample_size).then(codec::Framer::default);
            let reorder = Order::of(family)
                .zip(stream.sample_duration_ns())
                .map(|(order, step)| Reorder::new(Retime::new(order, step, &head)));
            self.framers.push(framer);
            self.pad_streams.push(PadStream {
                stream_index: stream.index,
                pad,
                family,
                sample_dur_ns: stream.sample_duration_ns(),
                ticks: 0,
                gate: 0,
                started: false,
                greet: false,
                read_to: None,
                resume: None,
                is_video: matches!(stream.kind, StreamKind::Video),
                warned_undecodable: false,
                junk: 0,
                stream: stream.clone(),
                head,
                reorder,
            });
        }
        if self.pad_streams.is_empty() {
            return Err(Error::element_failed(ctx.element(), AviError::NoStreams));
        }
        let video = self.pad_streams.iter().find(|ps| ps.is_video);
        self.key = video.unwrap_or(&self.pad_streams[0]).stream_index;
        self.scan = match self.shared {
            Some(_) => None,
            None => index::scan_for(&header, &movi, &self.header, self.file_len),
        };
        let sized = movi.movi_end > movi.movi_data_start;
        let fits = self.file_len.is_none_or(|len| movi.movi_end.saturating_add(8) <= len);
        self.read = match (&self.scan, sized && fits) {
            _ if self.shared.is_some() => Idx1Read::Off,
            (Some(_), true) => Idx1Read::Want { at: movi.movi_end, body: None, hops: 0 },
            (scan, reachable) => {
                let timed = scan.is_some();
                log!(&*ctx, Level::Warn, "index_unread", timed = timed, reachable = reachable);
                Idx1Read::Off
            }
        };
        let duration = self.duration_ns.map_or(Timestamp::NONE, Timestamp);
        log!(
            &*ctx,
            Level::Info,
            "opened",
            streams = self.pad_streams.len(),
            duration = duration,
            movi = movi.movi_data_start,
        );
        self.whole = self.shared.as_ref().is_some_and(|i| i.whole);
        self.publish(ctx);
        if self.read != Idx1Read::Off {
            ctx.will_index();
        }
        Ok(())
    }

    // Once per run.
    #[allow(clippy::disallowed_methods)]
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.walker = MoviWalker::new();
        (self.skipped, self.origin) = (0, Some((self.movi_data_start, 0)));
        self.in_movi = false;
        self.carry = None;
        self.posted_duration = false;
        (self.taken, self.measured, self.teller) =
            (0, rate::ByteRate::default(), rate::Teller::default());
        for ps in &mut self.pad_streams {
            (ps.started, ps.greet, ps.ticks, ps.gate, ps.junk) = (false, false, 0, 0, 0);
            (ps.read_to, ps.resume) = (None, None);
            ps.reorder.iter_mut().for_each(Reorder::reset);
        }
        (self.strays, self.seen) = (0, (0, 0));
        ctx.want_ranges(Self::IN);
        let stats = ctx.pool_stats();
        let depth = (stats.max_slots / 4).clamp(1, 4) as u32;
        self.piece = (stats.bytes_budget / 4).clamp(256, u64::from(IDX1_PIECE)) as u32;
        let mut feed = Feed::new(WINDOW, depth);
        feed.set_end(self.file_len);
        self.feed = Some(feed);
        if let Idx1Read::Asked { at, body, hops, .. } = self.read {
            self.read = Idx1Read::Want { at, body, hops };
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.demux(ctx, inputs)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        self.on_event(ctx, event)
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.on_stop(ctx);
    }
}

impl AviDemux {
    fn demux(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(from) = ctx.pull_reset(Self::IN) {
            self.restart_pull(from.to_byte);
        }
        let pulled = ctx.pulls(Self::IN);
        if !pulled && self.read != Idx1Read::Off {
            self.idx1_over(ctx, false);
        }
        if !pulled && !self.drain(ctx) {
            ctx.hold_end();
            return Ok(Flow::Ok);
        }
        if !self.posted_duration
            && let Some(ns) = self.duration_ns
        {
            self.posted_duration = true;
            ctx.set_duration(ns);
        }
        if pulled {
            self.pull(ctx, &mut inputs);
            return Ok(Flow::Ok);
        }
        while let Some(buf) = inputs.pop() {
            self.walk_in(buf.memory.data());
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
                self.note(ctx);
                if self.read != Idx1Read::Off {
                    self.idx1_over(ctx, false);
                }
                let (resyncs, junk) = (self.walker.resyncs, self.walker.junk);
                let framed: u64 = self.pad_streams.iter().map(|ps| ps.junk).sum();
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    resyncs = resyncs,
                    junk = junk,
                    framed_junk = framed,
                    strays = self.strays,
                );
            }
            Event::FlushStart => {
                self.carry = None;
                self.measured.clear();
                self.teller.restart();
                let target = ctx.seek_target();
                self.restart_at(target.map(|t| t.to_byte));
                let to = target.and_then(|t| t.at.nanos());
                let scan = self.shared.as_ref().map(|i| &i.scan).or(self.scan.as_ref());
                let clocks = target.and_then(|t| scan?.clocks_at(t.to_byte));
                let at = target.map_or(0, |t| t.to_byte);
                let (seek, clocked) = (target.is_some(), clocks.is_some());
                log!(&*ctx, Level::Debug, "flush", seek = seek, at = at, clocked = clocked);
                self.framing = None;
                self.framers.iter_mut().flatten().for_each(codec::Framer::clear);
                for (n, ps) in self.pad_streams.iter_mut().enumerate() {
                    (ps.started, ps.greet, ps.read_to, ps.resume) = (false, false, None, None);
                    ps.reorder.iter_mut().for_each(Reorder::reset);
                    let tick = ps.sample_dur_ns.filter(|ns| *ns > 0);
                    let at = to.zip(tick).map_or(0, |(to, ns)| (to + ns / 2) / ns);
                    (ps.ticks, ps.gate) = match clocks.and_then(|c| c.get(n)) {
                        Some(&ticks) => (ticks, if ps.is_video { at } else { 0 }),
                        None => (at, 0),
                    };
                }
            }
            Event::SrcLinked { pad, on: true, restart } => self.relink(ctx, *pad, *restart),
            _ => {}
        }
        Ok(())
    }

    /// A link made on `pad` mid-run gets the stream's head ahead of what goes out on it next. A
    /// restart reads the stream again from its clock there, what it held from before would go
    /// out twice. The other streams drop what they read again up to where they had got.
    fn relink(&mut self, ctx: &mut Ctx, pad: SrcPad, restart: Option<SeekTarget>) {
        let Some(n) = self.pad_streams.iter().position(|ps| ps.pad == pad) else { return };
        let carried = self.carry.as_ref().filter(|c| c.pad == pad).map(|c| (c.spot, c.pts_ns));
        if let Some(target) = restart {
            if let Some((spot, _)) = carried {
                self.carry = None;
                self.pass(spot);
            }
            // a pushed source restarts with no pull reset to tell
            self.restart_at(Some(target.to_byte));
            self.framers.get_mut(n).into_iter().flatten().for_each(codec::Framer::clear);
            self.framing = self.framing.filter(|f| f.0 != n);
            let scan = self.shared.as_ref().map(|i| &i.scan).or(self.scan.as_ref());
            let clock = scan.and_then(|s| s.clocks_at(target.to_byte)?.get(n).copied());
            let placed = self.origin.is_some();
            for (m, ps) in self.pad_streams.iter_mut().enumerate() {
                if m != n {
                    ps.resume = ps.read_to.filter(|_| placed);
                    continue;
                }
                ps.reorder.iter_mut().for_each(Reorder::reset);
                let tick = ps.sample_dur_ns.filter(|ns| *ns > 0);
                let at = target.at.nanos().zip(tick).map_or(0, |(to, ns)| (to + ns / 2) / ns);
                (ps.ticks, ps.gate) = match clock {
                    Some(ticks) => (ticks, if ps.is_video { at } else { 0 }),
                    None => (at, 0),
                };
                (ps.read_to, ps.resume) = (None, None);
            }
        }
        let ps = &mut self.pad_streams[n];
        let held = ps.reorder.as_ref().filter(|r| !r.held.is_empty());
        let at = restart.map_or(u64::MAX, |t| t.to_byte);
        log!(&*ctx, Level::Debug, "relink", stream = ps.stream_index, restart = at);
        if restart.is_some() || (carried.is_none() && held.is_none()) {
            ps.greet = true;
            return;
        }
        // units read before the link go out first, the head goes ahead of them now
        let next = ps.sample_dur_ns.map(|ns| ps.ticks.saturating_mul(ns));
        let pts = held.and_then(|r| r.rt.peek()).or(carried.and_then(|c| c.1)).or(next);
        self.greet(ctx, pad, pts);
    }

    fn on_stop(&mut self, ctx: &mut Ctx) {
        self.flush_exact(ctx);
        self.walker = MoviWalker::new();
        self.carry = None;
        self.posted_duration = false;
    }
}
