//! `AviDemux`, the element over the [`crate::riff`] parser with one src pad per stream.

use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
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
use crate::riff::{self, AviError, MediaChunk, MoviWalker, Stream, StreamKind};

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
    is_video: bool,
    framer: Option<codec::Framer>,
    warned_undecodable: bool,
    /// Bytes the framer skipped between frames.
    junk: u64,
    /// The stream's configuration sent as a HEADER buffer at each start, an MPEG-4 VOL, else
    /// empty.
    head: Vec<u8>,
}

impl PadStream {
    fn next_pts_ns(&mut self, payload_size: usize) -> Option<u64> {
        let dur = self.sample_dur_ns?;
        let pts = self.ticks.saturating_mul(dur);
        self.ticks = self.ticks.saturating_add(index::ticks(self.stream.sample_size, payload_size));
        Some(pts)
    }
}

struct Carry {
    pad: SrcPad,
    pts_ns: Option<u64>,
    flags: BufferFlags,
    bytes: Vec<u8>,
    off: usize,
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
    framed: std::collections::VecDeque<MediaChunk>,
    pending: std::collections::VecDeque<Carry>,
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
            framed: std::collections::VecDeque::new(),
            pending: std::collections::VecDeque::new(),
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

    fn emit_bounded(
        ctx: &mut Ctx,
        pad: SrcPad,
        pts_ns: Option<u64>,
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
        buf.flags = flags;
        ctx.out(pad).push(buf);
        bytes.len()
    }

    fn emit_exact(
        ctx: &mut Ctx,
        pad: SrcPad,
        pts_ns: Option<u64>,
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
        buf.flags = flags;
        ctx.out(pad).push(buf);
    }

    fn drain_pending(&mut self, ctx: &mut Ctx) -> bool {
        while let Some(mut c) = self.pending.pop_front() {
            c.off = Self::emit_bounded(ctx, c.pad, c.pts_ns, c.flags, &c.bytes, c.off);
            if c.off < c.bytes.len() {
                self.pending.push_front(c);
                return false;
            }
        }
        true
    }

    fn route(
        &mut self,
        ctx: &mut Ctx,
        chunk: &MediaChunk,
    ) -> Option<(SrcPad, Option<u64>, BufferFlags, bool)> {
        let Some(idx) =
            self.pad_streams.iter().position(|ps| ps.stream_index == chunk.stream_index)
        else {
            let level = if self.strays == 0 { Level::Warn } else { Level::Trace };
            log!(&*ctx, level, "chunk_stray", id = chunk.id, bytes = chunk.data.len());
            self.strays += 1;
            return None;
        };
        let ps = &mut self.pad_streams[idx];
        let held = ps.ticks < ps.gate;
        let pts = ps.next_pts_ns(chunk.data.len());
        if held {
            return None;
        }
        if ps.head.is_empty()
            && ps.family == codec::FAMILY_MPEG4_ASP
            && let Some(vol) = codec::vol_prefix(&chunk.data)
        {
            ps.head.extend_from_slice(vol);
        }
        let fresh = !ps.started;
        if fresh {
            ps.started = true;
            let (pad, family) = (ps.pad, ps.family);
            Self::announce(ctx, pad, family, &self.pad_streams[idx].stream);
            if !codec::is_decodable(family) && !self.pad_streams[idx].warned_undecodable {
                self.pad_streams[idx].warned_undecodable = true;
                let element = ctx.element();
                ctx.post(BusMessage::Warning {
                    element,
                    error: Error::element_failed(
                        element,
                        AviError::NoDecoderFamily { stream: chunk.stream_index },
                    ),
                });
            }
        }
        let ps = &self.pad_streams[idx];
        let flags = if ps.is_video && codec::chunk_is_key(ps.family, &chunk.data) {
            BufferFlags::KEYFRAME
        } else {
            BufferFlags::empty()
        };
        Some((ps.pad, pts, flags, fresh))
    }

    /// The stream's head on `pad` ahead of its first chunk of the run, stamped with it.
    fn greet(&self, ctx: &mut Ctx, pad: SrcPad, pts_ns: Option<u64>) {
        let Some(ps) = self.pad_streams.iter().find(|ps| ps.pad == pad) else { return };
        if !ps.head.is_empty() {
            Self::emit_exact(ctx, pad, pts_ns, BufferFlags::HEADER, &ps.head, 0);
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
        while let Some(chunk) = self.next_unit() {
            let Some((pad, pts, flags, fresh)) = self.route(ctx, &chunk) else {
                continue;
            };
            if fresh {
                self.greet(ctx, pad, pts);
            }
            let off = Self::emit_bounded(ctx, pad, pts, flags, &chunk.data, 0);
            if off < chunk.data.len() {
                self.pending.push_back(Carry { pad, pts_ns: pts, flags, bytes: chunk.data, off });
                return false; // pool dry, stop pulling walker chunks
            }
            if let (true, Some(pts)) = (chunk.stream_index == self.key, pts) {
                self.key_out(ctx, Timestamp::from_nanos(pts));
            }
        }
        true
    }

    fn flush_exact(&mut self, ctx: &mut Ctx) {
        while let Some(c) = self.pending.pop_front() {
            Self::emit_exact(ctx, c.pad, c.pts_ns, c.flags, &c.bytes, c.off);
        }
        while let Some(chunk) = self.next_unit() {
            let Some((pad, pts, flags, fresh)) = self.route(ctx, &chunk) else {
                continue;
            };
            if fresh {
                self.greet(ctx, pad, pts);
            }
            Self::emit_exact(ctx, pad, pts, flags, &chunk.data, 0);
        }
    }

    fn next_unit(&mut self) -> Option<MediaChunk> {
        loop {
            if let Some(c) = self.framed.pop_front() {
                return Some(c);
            }
            let chunk = self.walker.next_chunk()?;
            let framed = &mut self.framed;
            let Some(ps) =
                self.pad_streams.iter_mut().find(|ps| ps.stream_index == chunk.stream_index)
            else {
                return Some(chunk);
            };
            let Some(framer) = ps.framer.as_mut() else {
                return Some(chunk);
            };
            let (stream_index, id) = (chunk.stream_index, chunk.id);
            let dropped = framer.push(ps.family, &chunk.data, |data| {
                framed.push_back(MediaChunk { stream_index, id, data })
            });
            ps.junk += dropped as u64;
            ps.ticks = ps.ticks.saturating_add(index::ticks(ps.stream.sample_size, dropped));
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
                ctx.announce_format(
                    pad,
                    family,
                    &[
                        ("rate", ValueDesc::Int(stream.samples_per_sec as i64)),
                        ("channels", ValueDesc::Int(stream.channels as i64)),
                        (
                            "sample",
                            ValueDesc::Id(codec::raw_sample(
                                stream.format_tag,
                                stream.bits_per_sample,
                            )),
                        ),
                    ],
                );
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
        self.walker.resync();
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
            self.pad_streams.push(PadStream {
                stream_index: stream.index,
                pad,
                family,
                sample_dur_ns: stream.sample_duration_ns(),
                ticks: 0,
                gate: 0,
                started: false,
                is_video: matches!(stream.kind, StreamKind::Video),
                framer: codec::reframes(family, stream.sample_size).then(codec::Framer::default),
                warned_undecodable: false,
                junk: 0,
                stream: stream.clone(),
                head,
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
        self.skipped = 0;
        self.in_movi = false;
        self.pending.clear();
        self.posted_duration = false;
        (self.taken, self.measured, self.teller) =
            (0, rate::ByteRate::default(), rate::Teller::default());
        for ps in &mut self.pad_streams {
            (ps.started, ps.ticks, ps.gate, ps.junk) = (false, 0, 0, 0);
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
                self.pending.clear();
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
                self.framed.clear();
                for (n, ps) in self.pad_streams.iter_mut().enumerate() {
                    ps.started = false;
                    if let Some(f) = ps.framer.as_mut() {
                        f.clear();
                    }
                    let tick = ps.sample_dur_ns.filter(|ns| *ns > 0);
                    let at = to.zip(tick).map_or(0, |(to, ns)| (to + ns / 2) / ns);
                    (ps.ticks, ps.gate) = match clocks.and_then(|c| c.get(n)) {
                        Some(&ticks) => (ticks, if ps.is_video { at } else { 0 }),
                        None => (at, 0),
                    };
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_stop(&mut self, ctx: &mut Ctx) {
        self.flush_exact(ctx);
        self.walker = MoviWalker::new();
        self.pending.clear();
        self.posted_duration = false;
    }
}
