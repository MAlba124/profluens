//! `OggDemux` and `OggMux`, the elements over the `OggReader` and `OggWriter` library (RFC 3533).

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, Disposition, SeekTarget, Stream, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::feedback::Bitrate;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::pipeline::Index;
use profluens_core::pull::{Fed, Feed, RangeError};
use profluens_core::rate;
use profluens_core::time::Timestamp;

use crate::duration::{
    GranuleClock, OGG_TAIL_FIRST, OGG_TAIL_LEN, OPUS_GRANULE_RATE, granule_clock, last_granule,
};
use crate::ident::{Mapping, flac_frame_samples, opus_packet_samples};
use crate::page::GRANULE_NONE;
use crate::reader::{OggReader, Packet};
use crate::seek::{Bisect, Landing, Step};
use crate::streams::scan_head;
use crate::writer::OggWriter;

/// Ogg has no concept of time (RFC 3533 section 4), so the muxer stamps granule 0.
const DEFAULT_GRANULE: u64 = 0;

/// The default serial number for [`OggMux::new`].
pub const DEFAULT_SERIAL: u32 = 0x5C_09_67_67; // "\x5C" + "Ogg"-ish, just a constant

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static DEMUX_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static DEMUX_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

pub(crate) static DEMUX_DESC: ElementDesc = ElementDesc {
    name: "oggdemux",
    sinks: &DEMUX_SINKS,
    srcs: &DEMUX_SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Cold, the make_default closure boxes one element at plugin/pipeline construction.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(OggDemux::new())),
    io: false,
    couplings: &[],
};

pub(crate) static STREAMS_DESC: ElementDesc = ElementDesc {
    name: "oggdemux",
    sinks: &DEMUX_SINKS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// Packets one `process()` emits before yielding. Ogg packets are small, so the byte budget alone
/// let one pass turn a whole buffered file into one batch. 32 measured best per allocation.
const EMIT_BUDGET: usize = 32;

const WINDOW: u32 = 256 * 1024;
const WINDOW_DEPTH: u32 = 2;
const WINDOW_FIRST: u32 = 16 << 10;
const WINDOW_TIME: u64 = 250_000_000;
const PACED_MIN: u32 = 4 << 10;

fn paced_window(bits_per_second: u64) -> u32 {
    let bytes = u128::from(bits_per_second) * u128::from(WINDOW_TIME) / 8_000_000_000;
    bytes.clamp(u128::from(PACED_MIN), u128::from(WINDOW)) as u32
}

const TAIL_TAG: u32 = 1;
const PROBE_TAG: u32 = 2;
/// How far ahead of a target Opus starts decoding to converge (RFC 7845 section 4.6).
const OPUS_PREROLL_NS: u64 = 80_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tail {
    Absent,
    Want,
    Asked,
    In,
}

/// A packet parked on the byte budget. At most one exists, the reader holds the rest.
struct Carry {
    data: Vec<u8>,
    track: usize,
    head: bool,
    pts: Timestamp,
}

struct Track {
    serial: Option<u32>,
    stream: u32,
    pad: Option<SrcPad>,
    stamp: Timestamp,
    heads: u32,
    run: Run,
}

/// The clock of a track whose packet lengths the demuxer reads, Opus and FLAC. A stamp anchors
/// it, the packets after go out at the anchor plus the samples between. Each page's granule
/// keeps it on time, a packet lost to a bad page costs no drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Samples a second, 0 for a mapping whose packet lengths are not read here.
    rate: u32,
    flac: bool,
    anchor: Timestamp,
    /// Samples since the anchor.
    samples: u64,
    /// The granule at the anchor, learnt on the first page ending after it.
    base: Option<u64>,
}

impl Default for Run {
    fn default() -> Self {
        Run { rate: 0, flac: false, anchor: Timestamp::NONE, samples: 0, base: None }
    }
}

impl Run {
    fn identify(&mut self, ident: &[u8]) {
        (self.rate, self.flac) = match (Mapping::identify(ident), granule_clock(ident)) {
            (Mapping::Opus, _) => (OPUS_GRANULE_RATE, false),
            (Mapping::Flac, Some(GranuleClock::Rate(rate))) => (rate, true),
            _ => (0, false),
        };
        self.stop();
    }

    /// Until the next stamp the packets go out unstamped.
    fn stop(&mut self) {
        (self.anchor, self.samples, self.base) = (Timestamp::NONE, 0, None);
    }

    fn length(&self, data: &[u8]) -> Option<u32> {
        match (self.rate, self.flac) {
            (0, _) => None,
            (_, true) => flac_frame_samples(data),
            (_, false) => opus_packet_samples(data),
        }
    }

    fn anchored(&self) -> bool {
        self.rate == 0 || self.anchor.is_some()
    }

    /// The granule where `packet` starts, the end of its page less the samples from it on
    /// there, `after` the packets queued behind it.
    fn start<'a>(
        &self,
        packet: &Packet,
        mut after: impl Iterator<Item = &'a Packet>,
    ) -> Option<u64> {
        let mut samples = u64::from(self.length(&packet.data)?);
        let mut end = packet;
        while !end.last {
            end = after.find(|p| p.serial == packet.serial)?;
            samples += u64::from(self.length(&end.data)?);
        }
        (end.granule != GRANULE_NONE).then_some(end.granule)?.checked_sub(samples)
    }

    /// Anchors the clock at the sample of `granule`, its container stamp, ahead of the stream
    /// time by an Opus pre-skip as a demuxer stamps.
    fn anchor_at(&mut self, granule: u64) {
        let ns = u128::from(granule) * 1_000_000_000 / u128::from(self.rate);
        self.anchor = Timestamp(u64::try_from(ns).unwrap_or(u64::MAX));
        (self.samples, self.base) = (0, Some(granule));
    }

    /// The stamp of a media packet the demuxer had `stamp` for.
    fn stamp(&mut self, stamp: Timestamp, data: &[u8], granule: u64, last: bool) -> Timestamp {
        if self.rate == 0 {
            return stamp;
        }
        if stamp.is_some() {
            self.stop();
            self.anchor = stamp;
        }
        let Some(anchor) = self.anchor.nanos() else {
            return stamp;
        };
        let ns = u128::from(self.samples) * 1_000_000_000 / u128::from(self.rate);
        let pts = Timestamp(anchor.saturating_add(u64::try_from(ns).unwrap_or(u64::MAX)));
        let Some(n) = self.length(data) else {
            self.stop();
            return pts;
        };
        self.samples += u64::from(n);
        if last && granule != GRANULE_NONE {
            match self.base {
                None => self.base = granule.checked_sub(self.samples),
                Some(base) => self.samples = granule.checked_sub(base).unwrap_or(self.samples),
            }
        }
        pts
    }
}

/// What a head that names no stream fails with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemuxError {
    NoStreams,
}

impl std::fmt::Display for DemuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DemuxError::NoStreams => f.write_str("oggdemux: the head holds no bos page"),
        }
    }
}

impl std::error::Error for DemuxError {}

/// Demultiplexes an Ogg byte stream into one reassembled codec packet per output buffer.
pub struct OggDemux {
    reader: OggReader,
    key: Option<u32>,
    tracks: Vec<Track>,
    head: Option<Vec<u8>>,
    streams: bool,
    pending: Option<Carry>,
    len: Option<u64>,
    tail: Vec<u8>,
    tail_at: Tail,
    tail_want: u32,
    /// The duration the caller knows, published without a tail read.
    known: Option<u64>,
    ident: Option<Vec<u8>>,
    published: bool,
    feed: Feed,
    paired: bool,
    clock: Option<GranuleClock>,
    taken: u64,
    at: Timestamp,
    measured: rate::ByteRate,
    average: Option<Bitrate>,
    teller: rate::Teller,
    learnt: Option<(u32, GranuleClock)>,
    bisect: Option<(Bisect, SeekTarget)>,
    probe: Option<u64>,
    skip: Option<u64>,
    primes: bool,
    lead: Option<Vec<u8>>,
    /// The reader's resync and drop counts last logged.
    seen: (u64, u64),
}

impl Default for OggDemux {
    // Cold, one-time constructor, the one track of the single stream demuxer.
    #[allow(clippy::disallowed_methods)]
    fn default() -> Self {
        Self {
            reader: OggReader::default(),
            key: None,
            tracks: vec![Track {
                serial: None,
                stream: 0,
                pad: Some(Self::OUT),
                stamp: Timestamp::NONE,
                heads: 0,
                run: Run::default(),
            }],
            head: None,
            streams: false,
            pending: None,
            len: None,
            // An empty Vec allocates nothing.
            #[allow(clippy::disallowed_methods)]
            tail: Vec::new(),
            tail_at: Tail::Absent,
            tail_want: OGG_TAIL_FIRST as u32,
            known: None,
            ident: None,
            published: false,
            feed: Feed::new(WINDOW_FIRST, WINDOW_DEPTH),
            paired: false,
            clock: None,
            taken: 0,
            at: Timestamp::NONE,
            measured: rate::ByteRate::default(),
            average: None,
            teller: rate::Teller::default(),
            learnt: None,
            bisect: None,
            probe: None,
            skip: None,
            primes: false,
            lead: None,
            seen: (0, 0),
        }
    }
}

impl OggDemux {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::default()
    }

    /// A demuxer of a source `len` bytes long.
    pub fn with_len(len: Option<u64>) -> Self {
        let mut d = Self { len, ..Self::default() };
        d.feed.set_end(len);
        d
    }

    /// A demuxer of a source `len` bytes long whose last bytes are `tail`, read by the caller,
    /// which publishes the duration from the last page there and asks no tail.
    pub fn with_extent(len: Option<u64>, tail: Vec<u8>) -> Self {
        let mut d = Self { len, tail, tail_at: Tail::In, ..Self::default() };
        d.feed.set_end(len);
        d
    }

    /// Publishes `ns` as the duration and asks no tail, for a file an earlier run timed.
    pub fn with_duration(mut self, ns: u64) -> Self {
        (self.known, self.tail_at) = (Some(ns), Tail::In);
        self
    }

    /// The demuxer of the file whose first bytes are `head`, enough of them to hold every bos page
    /// (RFC 3533 section 4 puts them first).
    pub fn with_head(mut self, head: Vec<u8>) -> Self {
        self.head = Some(head);
        self.streams = true;
        self.tracks.clear();
        self
    }

    /// The number in the collection of the stream of `serial`, once described.
    pub fn stream_of(&self, serial: u32) -> Option<u32> {
        self.tracks.iter().find(|t| t.serial == Some(serial)).map(|t| t.stream)
    }

    fn unlock(&mut self) {
        if !self.streams {
            self.tracks[0].serial = None;
            self.key = None;
        }
    }

    fn stamp_all(&mut self, stamp: Timestamp) {
        for t in &mut self.tracks {
            t.stamp = stamp;
        }
    }

    fn route(&mut self, serial: u32) -> Option<usize> {
        if let Some(at) = self.tracks.iter().position(|t| t.serial == Some(serial)) {
            return Some(at);
        }
        let first = self.tracks.first_mut().filter(|t| !self.streams && t.serial.is_none())?;
        first.serial = Some(serial);
        self.key = Some(serial);
        Some(0)
    }

    // Cold, once per open, a name and a description per stream.
    #[allow(clippy::disallowed_methods)]
    fn describe(&mut self, ctx: &mut Ctx, head: &[u8]) -> Result<(), Error> {
        self.tracks.clear();
        for s in scan_head(head) {
            let (serial, family) = (s.serial, s.mapping.family());
            let kind = match s.mapping {
                Mapping::Skeleton => {
                    log!(&*ctx, Level::Debug, "skeleton_skipped", serial = serial);
                    continue;
                }
                Mapping::Opus | Mapping::Flac | Mapping::Vorbis | Mapping::Speex => {
                    StreamKind::Audio
                }
                Mapping::Theora => StreamKind::Video,
                Mapping::Unknown => {
                    log!(&*ctx, Level::Warn, "stream_unknown", serial = serial);
                    StreamKind::Other
                }
            };
            let channels = s.channels;
            log!(
                &*ctx,
                Level::Info,
                "stream",
                serial = serial,
                family = family,
                channels = channels
            );
            let stream = ctx.add_stream(Stream {
                kind,
                codec: s.mapping.family(),
                language: s.language,
                title: s.title,
                default: self.tracks.is_empty(),
                forced: false,
                disposition: Disposition::MAIN,
                channels: s.channels,
                duration: Timestamp::NONE,
                sparse: kind.sparse(),
                name: format!("src_{:08x}", s.serial).into(),
                offers: &OFFERS,
            });
            self.key.get_or_insert(s.serial);
            self.tracks.push(Track {
                serial: Some(s.serial),
                stream,
                pad: None,
                stamp: Timestamp::NONE,
                heads: 0,
                run: Run::default(),
            });
        }
        log!(&*ctx, Level::Info, "opened", streams = self.tracks.len());
        match self.tracks.is_empty() {
            true => Err(Error::element_failed(ctx.element(), DemuxError::NoStreams)),
            false => Ok(()),
        }
    }

    // Cold, once per stream.
    #[allow(clippy::disallowed_methods)]
    fn ident(&mut self, ctx: &mut Ctx, bos: &[u8], serial: u32) {
        let waits = matches!(self.tail_at, Tail::Want | Tail::Asked);
        if !waits || crate::stream_duration_ns(bos, &[], serial).is_some() {
            self.publish(ctx, bos, serial);
        } else {
            self.ident = Some(bos.to_vec());
        }
    }

    fn publish_kept(&mut self, ctx: &mut Ctx) {
        if let (Some(bos), Some(serial)) = (self.ident.take(), self.key) {
            self.publish(ctx, &bos, serial);
        }
    }

    // Cold, once per stream, and the tail is let go.
    #[allow(clippy::disallowed_methods)]
    fn publish(&mut self, ctx: &mut Ctx, bos: &[u8], serial: u32) {
        if std::mem::replace(&mut self.published, true) {
            return;
        }
        let duration = self.known.or_else(|| crate::stream_duration_ns(bos, &self.tail, serial));
        let tail = self.tail.len();
        self.tail = Vec::new();
        match duration {
            Some(ns) => {
                log!(&*ctx, Level::Info, "duration", serial = serial, ns = Timestamp(ns));
                ctx.set_duration(ns);
            }
            // A live stream has no end to read.
            None if self.len.is_some() => {
                log!(&*ctx, Level::Warn, "duration_unknown", serial = serial, tail = tail);
            }
            None => {}
        }
        let duration = duration.map_or(Timestamp::NONE, Timestamp);
        let index = Index { file_len: self.len, duration, ..Index::default() };
        self.average = rate::average(&index);
        ctx.publish_index(index);
    }

    fn emit_packet(ctx: &mut Ctx, pad: SrcPad, data: &[u8], pts: Timestamp, head: bool) -> bool {
        let buf = match ctx.try_alloc_sized(data.len()) {
            Ok(Some(buf)) => buf,
            Ok(None) => return false,
            Err(e) => {
                let element = ctx.element();
                ctx.post(BusMessage::Warning { element, error: Error::element_failed(element, e) });
                return true;
            }
        };
        Self::put(ctx, pad, buf, data, pts, head);
        true
    }

    fn emit_exact(ctx: &mut Ctx, pad: SrcPad, data: &[u8], pts: Timestamp, head: bool) {
        let Some(buf) = ctx.alloc_exact(data.len()) else {
            let (element, error) = (ctx.element(), ctx.exact_refused(data.len()));
            ctx.post(BusMessage::Warning { element, error });
            return;
        };
        Self::put(ctx, pad, buf, data, pts, head);
    }

    fn put(ctx: &mut Ctx, pad: SrcPad, mut buf: Buffer, data: &[u8], pts: Timestamp, head: bool) {
        buf.memory.as_mut_full()[..data.len()].copy_from_slice(data);
        buf.memory.set_len(data.len());
        buf.pts = pts;
        buf.flags = if head { BufferFlags::HEADER } else { BufferFlags::empty() };
        ctx.out(pad).push(buf);
    }

    /// The stamp of `packet` on track `at`. A header goes out unstamped and leaves the track's
    /// stamp to the first media packet.
    fn pts(&mut self, at: usize, packet: &Packet, head: bool) -> Timestamp {
        if head {
            return Timestamp::NONE;
        }
        let t = &mut self.tracks[at];
        let stamp = std::mem::replace(&mut t.stamp, Timestamp::NONE);
        // no stamp from upstream, the page granule anchors it
        if stamp.is_none()
            && !t.run.anchored()
            && let Some(g) = t.run.start(packet, self.reader.queued())
        {
            t.run.anchor_at(g);
        }
        t.run.stamp(stamp, &packet.data, packet.granule, packet.last)
    }

    fn head(&mut self, at: usize, data: &[u8]) -> bool {
        let track = &mut self.tracks[at];
        let mapping = Mapping::identify(data);
        if mapping != Mapping::Unknown {
            track.heads = mapping.header_packets(data);
            track.run.identify(data);
        }
        let head = track.heads > 0;
        track.heads = track.heads.saturating_sub(1);
        head
    }

    fn place(
        &mut self,
        ctx: &mut Ctx,
        serial: u32,
        bos: bool,
        data: &[u8],
    ) -> Option<(usize, SrcPad)> {
        let at = self.route(serial);
        if at.is_none() && bos && Mapping::identify(data) != Mapping::Skeleton {
            log!(&*ctx, Level::Warn, "link_dropped", serial = serial, at = self.taken);
        }
        if !self.published && self.ident.is_none() && self.key == Some(serial) {
            self.ident(ctx, data, serial);
        }
        if self.clock.is_none() && self.key == Some(serial) {
            self.clock = granule_clock(data);
            if let Some(clock) = self.clock {
                if self.learnt.is_none() {
                    self.primes = Mapping::identify(data) == Mapping::Vorbis;
                }
                self.learnt.get_or_insert((serial, clock));
            }
        }
        let at = at?;
        Some((at, self.tracks[at].pad?))
    }

    /// The reader's first resync and drop at warn, later ones at trace.
    fn note_reader(&mut self, ctx: &Ctx) {
        let (resyncs, dropped) = (self.reader.resync_count(), self.reader.dropped_count());
        if resyncs > self.seen.0 {
            let level = if self.seen.0 == 0 { Level::Warn } else { Level::Trace };
            log!(ctx, level, "resync", count = resyncs, near = self.taken);
        }
        if dropped > self.seen.1 {
            let level = if self.seen.1 == 0 { Level::Warn } else { Level::Trace };
            log!(ctx, level, "packet_dropped", count = dropped, near = self.taken);
        }
        self.seen = (resyncs, dropped);
    }

    fn drain(&mut self, ctx: &mut Ctx) -> bool {
        self.note_reader(ctx);
        // A parked packet precedes everything queued behind it.
        if let Some(c) = self.pending.take() {
            let pad = self.tracks[c.track].pad.unwrap_or(Self::OUT);
            if !Self::emit_packet(ctx, pad, &c.data, c.pts, c.head) {
                self.pending = Some(c);
                return false; // budget still spent
            }
            self.reader.recycle(c.data);
        }
        let mut budget = EMIT_BUDGET;
        while let Some(packet) = self.reader.next_packet() {
            let placed = self.place(ctx, packet.serial, packet.bos, &packet.data);
            self.note(packet.serial, packet.granule, packet.data.len());
            self.tell_rate(ctx);
            let skipped = self.skips(packet.serial, packet.granule);
            let Some((at, pad)) = placed.filter(|_| !skipped) else {
                self.keep(skipped, packet.data);
                continue;
            };
            self.lead_out(ctx, packet.serial, pad);
            let head = self.head(at, &packet.data);
            let pts = self.pts(at, &packet, head);
            if !Self::emit_packet(ctx, pad, &packet.data, pts, head) {
                self.pending = Some(Carry { data: packet.data, track: at, head, pts });
                return false;
            }
            self.reader.recycle(packet.data);
            budget -= 1;
            if budget == 0 {
                return false; // yield, the rest stays queued for the next pass
            }
        }
        true
    }

    fn note(&mut self, serial: u32, granule: u64, len: usize) {
        if self.key != Some(serial) || granule == GRANULE_NONE {
            return;
        }
        let Some(ns) = self.clock.and_then(|c| c.ns(granule)) else {
            return;
        };
        if self.at.nanos() != Some(ns) {
            let before = self.taken.saturating_sub((self.reader.held() + len) as u64);
            self.measured.note(before, self.at);
            self.at = Timestamp(ns);
        }
    }

    fn tell_rate(&mut self, ctx: &mut Ctx) {
        let (measured, average) = (self.measured, self.average);
        let feed = &mut self.feed;
        self.teller.tell_at(ctx, Self::IN, self.at, |_| {
            let rate = measured.rate().or(average);
            if let Some(b) = rate {
                feed.set_window(paced_window(b.bits_per_second));
            }
            rate
        });
    }

    fn take_in(&mut self, data: &[u8]) {
        self.taken += data.len() as u64;
        self.reader.push(data);
    }

    fn flush(&mut self, ctx: &mut Ctx) {
        if let Some(c) = self.pending.take() {
            let pad = self.tracks[c.track].pad.unwrap_or(Self::OUT);
            Self::emit_exact(ctx, pad, &c.data, c.pts, c.head);
            self.reader.recycle(c.data);
        }
        while let Some(packet) = self.reader.next_packet() {
            let placed = self.place(ctx, packet.serial, packet.bos, &packet.data);
            let skipped = self.skips(packet.serial, packet.granule);
            match placed.filter(|_| !skipped) {
                Some((at, pad)) => {
                    self.lead_out(ctx, packet.serial, pad);
                    let head = self.head(at, &packet.data);
                    let pts = self.pts(at, &packet, head);
                    Self::emit_exact(ctx, pad, &packet.data, pts, head);
                    self.reader.recycle(packet.data);
                }
                None => self.keep(skipped, packet.data),
            }
        }
        self.note_reader(ctx);
        self.publish_kept(ctx);
    }

    fn skips(&mut self, serial: u32, granule: u64) -> bool {
        if self.skip.is_none() || self.learnt.map(|(key, _)| key) != Some(serial) {
            return false;
        }
        match self.skip == Some(granule) {
            true => true,
            false => {
                self.skip = None;
                false
            }
        }
    }

    fn keep(&mut self, skipped: bool, data: Vec<u8>) {
        match skipped && self.primes {
            true => {
                if let Some(old) = self.lead.replace(data) {
                    self.reader.recycle(old);
                }
            }
            false => self.reader.recycle(data),
        }
    }

    fn lead_out(&mut self, ctx: &mut Ctx, serial: u32, pad: SrcPad) {
        if self.learnt.map(|(key, _)| key) != Some(serial) {
            return;
        }
        if let Some(lead) = self.lead.take() {
            Self::emit_exact(ctx, pad, &lead, Timestamp::NONE, false);
            self.reader.recycle(lead);
        }
    }

    fn unland(&mut self) {
        (self.bisect, self.probe, self.skip) = (None, None, None);
        if let Some(lead) = self.lead.take() {
            self.reader.recycle(lead);
        }
    }

    fn bisector(&self, ctx: &Ctx, from: SeekTarget) -> Option<Bisect> {
        let len = self.len.filter(|len| *len > 0 && ctx.pulls(Self::IN))?;
        let (key, clock) = self.learnt?;
        let target = ctx.presentation_point().unwrap_or(from.at).nanos()?;
        let preroll = match clock {
            GranuleClock::Opus { .. } => OPUS_PREROLL_NS,
            GranuleClock::Rate(_) => 0,
        };
        Some(Bisect::new(key, clock, target.saturating_sub(preroll), len))
    }

    fn ask_probe(&mut self, ctx: &mut Ctx) {
        let Some((bisect, from)) = &self.bisect else {
            return;
        };
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
            Step::Land(landing) => {
                let (at, granule) =
                    landing.map_or((from.to_byte, GRANULE_NONE), |l| (l.at, l.granule));
                let found = landing.is_some();
                log!(&*ctx, Level::Debug, "seek_landed", at = at, granule = granule, found = found);
                self.land(landing, from)
            }
        }
    }

    fn probe_in(&mut self, data: &[u8]) {
        if let (Some((bisect, _)), Some(at)) = (&mut self.bisect, self.probe.take()) {
            bisect.served(at, data);
        }
    }

    fn land(&mut self, landing: Option<Landing>, from: SeekTarget) {
        self.bisect = None;
        match landing {
            Some(l) => {
                self.feed.restart(l.at);
                self.skip = Some(l.granule);
                let stamp = self.learnt.and_then(|(_, clock)| clock.stamp(l.granule));
                self.stamp_all(Timestamp(stamp.unwrap_or(l.ns)));
            }
            None => {
                self.feed.restart(from.to_byte);
                self.stamp_all(from.at);
            }
        }
    }

    fn restart_pull(&mut self, ctx: &mut Ctx, from: SeekTarget) {
        self.feed.restart(from.to_byte);
        self.unland();
        // A source stamps nothing on its first byte read unasked.
        if std::mem::replace(&mut self.paired, true) {
            let bisect = self.bisector(ctx, from);
            let planned = bisect.is_some();
            log!(
                &*ctx,
                Level::Debug,
                "seek",
                target = from.at,
                at = from.to_byte,
                bisect = planned
            );
            match bisect {
                Some(b) => self.bisect = Some((b, from)),
                None => self.stamp_all(from.at),
            }
        }
        let pulls = ctx.pulls(Self::IN);
        let wanted = pulls && !self.published && self.len.is_some_and(|len| len > 0);
        self.tail_at = match self.tail_at {
            Tail::Absent | Tail::Want | Tail::Asked if wanted => Tail::Want,
            Tail::Want | Tail::Asked => Tail::Absent,
            t => t,
        };
        if self.tail_at == Tail::Absent {
            self.publish_kept(ctx);
        }
    }

    fn ask_tail(&mut self, ctx: &mut Ctx) {
        let (Tail::Want, Some(len)) = (self.tail_at, self.len) else {
            return;
        };
        let want = u64::from(self.tail_want).min(len);
        // A pool that cannot hold the tail never will, so the duration stays unknown.
        if want > ctx.pool_stats().bytes_budget {
            log!(&*ctx, Level::Warn, "tail_unread", bytes = want);
            self.tail_at = Tail::In;
            self.publish_kept(ctx);
            return;
        }
        if self.feed.aside(ctx, Self::IN, len - want, want as u32, TAIL_TAG) {
            self.tail_at = Tail::Asked;
        }
    }

    // Cold, once per stream, copied out so its buffer goes back to the pool.
    #[allow(clippy::disallowed_methods)]
    fn tail_in(&mut self, ctx: &mut Ctx, data: &[u8]) {
        if self.tail_at != Tail::Asked {
            return;
        }
        self.tail_at = Tail::In;
        let Some(len) = self.len.filter(|_| !self.published) else {
            return;
        };
        let want = u64::from(self.tail_want).min(len);
        if (data.len() as u64) < want {
            let element = ctx.element();
            let short =
                RangeError::Short { offset: len - want, want: want as u32, got: data.len() as u32 };
            ctx.post(BusMessage::Warning { element, error: Error::element_failed(element, short) });
        } else if want < (OGG_TAIL_LEN as u64).min(len) && last_granule(data, self.key).is_none() {
            log!(&*ctx, Level::Debug, "tail_grow", from = want, to = OGG_TAIL_LEN);
            (self.tail_at, self.tail_want) = (Tail::Want, OGG_TAIL_LEN as u32);
            return;
        }
        self.tail = data.to_vec();
        self.publish_kept(ctx);
    }

    fn pull(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Flow {
        self.ask_tail(ctx);
        self.ask_probe(ctx);
        let mut held = false;
        loop {
            held = held || !self.drain(ctx);
            match self.feed.take(ctx, Self::IN, inputs) {
                Some(Fed::Window { buf, .. }) if self.bisect.is_none() => {
                    self.take_in(buf.memory.data())
                }
                Some(Fed::Window { .. }) => {}
                Some(Fed::Aside { tag: PROBE_TAG, buf }) => {
                    self.probe_in(buf.memory.data());
                    self.ask_probe(ctx);
                }
                Some(Fed::Aside { buf, .. }) => self.tail_in(ctx, buf.memory.data()),
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

    fn demux(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if let Some(from) = ctx.pull_reset(Self::IN) {
            self.restart_pull(ctx, from);
        }
        if ctx.pulls(Self::IN) {
            return Ok(self.pull(ctx, &mut inputs));
        }
        // Consume input only while emission keeps up, un-popped input stays buffered for the next
        // pass.
        if !self.drain(ctx) {
            return Ok(Flow::Ok);
        }
        while let Some(buf) = inputs.pop() {
            if buf.pts.is_some() {
                self.stamp_all(buf.pts);
            }
            self.take_in(buf.memory.data());
            if !self.drain(ctx) {
                return Ok(Flow::Ok);
            }
        }
        Ok(Flow::Ok)
    }
}

impl Element for OggDemux {
    fn desc(&self) -> &'static ElementDesc {
        match self.streams {
            true => &STREAMS_DESC,
            false => &DEMUX_DESC,
        }
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        match self.head.take() {
            Some(head) => self.describe(ctx, &head),
            None => Ok(()),
        }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if !self.published {
            ctx.will_index();
        }
        self.reader = OggReader::new();
        self.unlock();
        for t in &mut self.tracks {
            (t.stamp, t.heads) = (Timestamp::NONE, 0);
            t.run.stop();
            if self.streams {
                t.pad = ctx.stream_pad(t.stream).filter(|pad| ctx.pad_linked(*pad));
            }
        }
        self.pending = None;
        (self.clock, self.taken, self.at) = (None, 0, Timestamp::NONE);
        (self.measured, self.teller) = (rate::ByteRate::default(), rate::Teller::default());
        ctx.want_ranges(Self::IN);
        (self.paired, self.seen) = (false, (0, 0));
        self.unland();
        self.feed.set_window(WINDOW_FIRST);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.demux(ctx, inputs)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            // Drop everything pre-seek, the reader resyncs to the next page (RFC 3533 section 6).
            // The locked serial is kept so a grouped file does not switch streams mid-playback.
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", seek = ctx.seek_target().is_some());
                self.reader.reset();
                self.pending = None;
                self.stamp_all(Timestamp::NONE);
                self.tracks.iter_mut().for_each(|t| t.run.stop());
                self.unland();
                (self.at, self.measured) = (Timestamp::NONE, rate::ByteRate::default());
                self.teller.restart();
            }
            Event::Eos => {
                let left = self.reader.finish();
                self.flush(ctx);
                if left > 0 {
                    log!(&*ctx, Level::Warn, "trailing_bytes", bytes = left);
                }
                let r = &self.reader;
                let (pages, resyncs, dropped) =
                    (r.pages_parsed(), r.resync_count(), r.dropped_count());
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    pages = pages,
                    resyncs = resyncs,
                    dropped = dropped
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        // In case `event(Eos)` was not delivered. Idempotent.
        self.reader.finish();
        self.flush(ctx);
        self.reader = OggReader::new();
        self.unlock();
        self.pending = None;
    }
}

static MUX_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static MUX_SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

pub(crate) static MUX_DESC: ElementDesc = ElementDesc {
    name: "oggmux",
    sinks: &MUX_SINKS,
    srcs: &MUX_SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Cold, the make_default closure boxes one element at plugin/pipeline construction.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(OggMux::new())),
    io: false,
    couplings: &[],
};

/// Multiplexes a single logical bitstream, one Ogg packet per input buffer.
pub struct OggMux {
    serial: u32,
    writer: Option<OggWriter>,
    scratch: Vec<u8>,
    packets: u64,
    late: u64,
}

impl Default for OggMux {
    fn default() -> Self {
        Self::with_serial(DEFAULT_SERIAL)
    }
}

impl OggMux {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    /// A muxer with the [`DEFAULT_SERIAL`].
    pub fn new() -> Self {
        Self::default()
    }

    // Cold, one-time constructor.
    #[allow(clippy::disallowed_methods)]
    pub fn with_serial(serial: u32) -> Self {
        Self { serial, writer: None, scratch: Vec::new(), packets: 0, late: 0 }
    }

    pub fn serial(&self) -> u32 {
        self.serial
    }

    fn emit(ctx: &mut Ctx, bytes: &mut Vec<u8>) {
        let mut off = 0;
        while off < bytes.len() {
            let mut buf = ctx.alloc();
            let cap = buf.memory.capacity();
            debug_assert!(cap > 0, "oggmux: zero-capacity pool slot");
            let n = cap.min(bytes.len() - off);
            buf.memory.as_mut_full()[..n].copy_from_slice(&bytes[off..off + n]);
            buf.memory.set_len(n);
            ctx.out(Self::OUT).push(buf);
            off += n;
        }
        bytes.clear();
    }

    /// Flush the eos page (RFC 3533 section 6, flag 0x04) exactly once.
    fn finish_stream(&mut self, ctx: &mut Ctx) {
        if let Some(mut writer) = self.writer.take() {
            let (serial, packets) = (self.serial, self.packets);
            log!(&*ctx, Level::Info, "eos", serial = serial, packets = packets);
            // The pending page, else a nil eos page (RFC 3533 section 6 allows it).
            writer.finish(&mut self.scratch);
            let mut out = std::mem::take(&mut self.scratch);
            Self::emit(ctx, &mut out);
            self.scratch = out; // reclaim the allocation
        }
    }
}

impl Element for OggMux {
    fn desc(&self) -> &'static ElementDesc {
        &MUX_DESC
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.writer = Some(OggWriter::new(self.serial));
        self.scratch.clear();
        (self.packets, self.late) = (0, 0);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let writer = match self.writer.as_mut() {
                Some(w) => w,
                // A late buffer after EOS is dropped rather than reopening a closed stream.
                None => {
                    if self.late == 0 {
                        log!(&*ctx, Level::Warn, "after_eos", serial = self.serial);
                    }
                    self.late += 1;
                    break;
                }
            };
            self.packets += 1;
            // `AfterEos` cannot occur, only `finish_stream` drops the writer.
            let _ = writer.write_packet(&mut self.scratch, buf.memory.data(), DEFAULT_GRANULE);
            let mut out = std::mem::take(&mut self.scratch);
            Self::emit(ctx, &mut out);
            self.scratch = out;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::Eos) {
            self.finish_stream(ctx);
        }
        Ok(())
    }

    // Cold, `stop` runs once at end-of-stream.
    #[allow(clippy::disallowed_methods)]
    fn stop(&mut self, ctx: &mut Ctx) {
        self.finish_stream(ctx);
        self.scratch = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 20 ms CELT packet, config 31, one frame.
    const PACKET: [u8; 2] = [31 << 3, 0];
    const MS20: u64 = 20_000_000;

    fn opus() -> Run {
        let mut r = Run::default();
        r.identify(b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00");
        r
    }

    #[test]
    fn packets_after_a_stamp_are_timed_by_their_samples() {
        let mut r = opus();
        let mut stamp = |s: Timestamp, granule: u64, last: bool| r.stamp(s, &PACKET, granule, last);
        assert_eq!(stamp(Timestamp(5), 960, false), Timestamp(5));
        assert_eq!(stamp(Timestamp::NONE, 1920, true), Timestamp(5 + MS20));
        assert_eq!(stamp(Timestamp::NONE, 2880, true), Timestamp(5 + 2 * MS20));
        assert_eq!(stamp(Timestamp(1_000), 0, false), Timestamp(1_000), "a new stamp anchors");
    }

    #[test]
    fn a_page_granule_puts_the_clock_back_on_time_after_a_lost_packet() {
        let mut r = opus();
        // the first page learns the granule at the anchor
        r.stamp(Timestamp::ZERO, &PACKET, 1920, false);
        r.stamp(Timestamp::NONE, &PACKET, 1920, true);
        // the page ending at 4800 lost its first packet, two of three came
        r.stamp(Timestamp::NONE, &PACKET, 4800, false);
        r.stamp(Timestamp::NONE, &PACKET, 4800, true);
        assert_eq!(r.stamp(Timestamp::NONE, &PACKET, 5760, true), Timestamp(5 * MS20));
    }

    #[test]
    fn a_packet_running_past_its_page_waits_for_the_page_it_ends_on() {
        let mut r = opus();
        r.stamp(Timestamp::ZERO, &PACKET, 960, true);
        // ends on a page whose granule is a later packet's end
        r.stamp(Timestamp::NONE, &PACKET, GRANULE_NONE, false);
        assert_eq!(r.stamp(Timestamp::NONE, &PACKET, 2880, true), Timestamp(2 * MS20));
    }

    #[test]
    fn a_mapping_of_unread_lengths_stamps_only_what_it_was_given() {
        let mut r = Run::default();
        r.identify(b"\x01vorbis");
        assert_eq!(r.stamp(Timestamp(7), b"x", 100, true), Timestamp(7));
        assert_eq!(r.stamp(Timestamp::NONE, b"x", 200, true), Timestamp::NONE);
    }
}
