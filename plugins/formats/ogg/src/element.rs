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

use crate::chain::{self, ChainScan, Links};
use crate::duration::{
    GranuleClock, OGG_TAIL_FIRST, OGG_TAIL_LEN, OPUS_GRANULE_RATE, granule_clock, last_granule,
};
use crate::ident::{
    Mapping, flac_frame_samples, opus_packet_samples, speex_packet_samples, theora_frames,
};
use crate::page::GRANULE_NONE;
use crate::reader::{OggReader, Packet};
use crate::seek::{Bisect, Landing, Step};
use crate::streams::{channels_of, scan_head};
use crate::writer::OggWriter;

/// Ogg has no concept of time (RFC 3533 section 4), so the muxer stamps granule 0.
const DEFAULT_GRANULE: u64 = 0;

/// The default serial number for [`OggMux::new`].
pub const DEFAULT_SERIAL: u32 = 0x5C_09_67_67; // "\x5C" + "Ogg"-ish, just a constant

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
/// A video decoder takes its family alone, the audio ones take bytes too.
static THEORA_OFFERS: [OfferDesc; 2] = [OfferDesc::any("theora"), OfferDesc::any("bytes")];

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
/// A page of the largest size. Under about 2 Mbit/s a quarter second is less, read in reads that
/// small a disk shared with other readers seeks for each.
const PACED_MIN: u32 = 64 << 10;

fn paced_window(bits_per_second: u64) -> u32 {
    let bytes = u128::from(bits_per_second) * u128::from(WINDOW_TIME) / 8_000_000_000;
    bytes.clamp(u128::from(PACED_MIN), u128::from(WINDOW)) as u32
}

const TAIL_TAG: u32 = 1;
const PROBE_TAG: u32 = 2;
/// A read of the link scan, its count in the bits above the two of the tag.
const CHAIN_TAG: u32 = 3;
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
/// Whether `packet` opens with a FLAC frame sync code, a metadata block header byte never
/// does. A mapping header's count read wrong would otherwise keep every frame as a header.
fn flac_frame(packet: &[u8]) -> bool {
    packet.len() >= 2 && packet[0] == 0xFF && packet[1] & 0xFC == 0xF8
}

struct Carry {
    data: Vec<u8>,
    track: usize,
    head: bool,
    pts: Timestamp,
}

/// The most header bytes a track keeps for a later link. Cover art in a FLAC PICTURE block or
/// a comment header runs to a few MB, a mapping header count read wrong would keep the media.
const KEPT_MAX: usize = 4 << 20;

struct Track {
    serial: Option<u32>,
    stream: u32,
    pad: Option<SrcPad>,
    stamp: Timestamp,
    heads: u32,
    /// A FLAC mapping, its header count ends early at the first frame.
    flac: bool,
    run: Run,
    /// The header packets put out so far, back to back, for a decoder linked mid-run.
    kept: Vec<u8>,
    kept_ends: Vec<usize>,
    /// The headers ran past `KEPT_MAX`, none are kept and a later link gets none.
    kept_over: bool,
    /// A link made mid-run is owed `kept` ahead of the track's next packet.
    greet: bool,
    /// The serial of the file's first link, the one a start reads first.
    first: Option<u32>,
    /// What a chained link must match to go on in this track.
    config: Option<Config>,
    clock: Option<GranuleClock>,
    /// The stream time of the chained links before the one it is in, ns, added to what its
    /// granules give.
    offset: u64,
    /// Its link is the one being read, a bos page behind data clears it until the next takes it.
    renewed: bool,
}

/// What a chained link keeps of the one before for its packets to go on in the same track, the
/// decoder set up for that one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Config {
    mapping: Mapping,
    channels: u16,
    clock: Option<GranuleClock>,
}

impl Config {
    fn of(ident: &[u8]) -> Config {
        let mapping = Mapping::identify(ident);
        let clock = granule_clock(ident).map(|c| match c {
            GranuleClock::Opus { .. } => GranuleClock::Opus { pre_skip: 0 },
            c => c,
        });
        Config { mapping, channels: channels_of(mapping, ident), clock }
    }
}

/// The link scan in flight, `asked` while a read is out under the tag of `seq`.
struct Scan {
    m: ChainScan,
    asked: bool,
    seq: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chained {
    Unknown,
    Scanning,
    Known,
}

impl Track {
    // Cold, once per stream.
    #[allow(clippy::disallowed_methods)]
    fn new(serial: Option<u32>, stream: u32, pad: Option<SrcPad>) -> Self {
        Track {
            serial,
            stream,
            pad,
            stamp: Timestamp::NONE,
            heads: 0,
            flac: false,
            run: Run::default(),
            kept: Vec::new(),
            kept_ends: Vec::new(),
            kept_over: false,
            greet: false,
            first: serial,
            config: None,
            clock: None,
            offset: 0,
            renewed: true,
        }
    }

    /// Back on the file's first link.
    fn rewind(&mut self) {
        (self.serial, self.offset, self.renewed) = (self.first, 0, true);
    }

    fn keep_head(&mut self, head: bool, data: &[u8]) {
        if !head || self.kept_over {
            return;
        }
        if self.kept.len() + data.len() > KEPT_MAX {
            self.kept_over = true;
            drop(std::mem::take(&mut self.kept));
            drop(std::mem::take(&mut self.kept_ends));
            return;
        }
        self.kept.extend_from_slice(data);
        self.kept_ends.push(self.kept.len());
    }

    /// Sends the kept header packets on `pad`, unstamped and flagged as they first went out.
    fn send_kept(&self, ctx: &mut Ctx, pad: SrcPad) {
        let mut from = 0;
        for &end in &self.kept_ends {
            OggDemux::emit_exact(ctx, pad, &self.kept[from..end], Timestamp::NONE, true);
            from = end;
        }
    }
}

/// The clock of a track whose packet lengths the demuxer reads, Opus, FLAC, Speex and Theora. A
/// stamp anchors it, the packets after go out at the anchor plus the samples between. Each page's
/// granule keeps it on time, a packet lost to a bad page costs no drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Ticks, samples or Theora frames, `rate` over `den` seconds. 0 for a mapping whose packet
    /// lengths are not read here.
    rate: u32,
    den: u32,
    lengths: Lengths,
    anchor: Timestamp,
    /// Samples since the anchor.
    samples: u64,
    /// The granule at the anchor, learnt on the first page ending after it.
    base: Option<u64>,
}

/// How a packet's length in ticks is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lengths {
    Opus,
    Flac,
    /// The same samples every packet, Speex.
    Fixed(u32),
    /// A frame a packet. A granule is the keyframe's count shifted left by `shift` plus the
    /// frames since, `lag` short of a count through its own frame (Theora I section A.2.3).
    Theora {
        shift: u8,
        lag: u64,
    },
}

impl Default for Run {
    fn default() -> Self {
        Run {
            rate: 0,
            den: 1,
            lengths: Lengths::Opus,
            anchor: Timestamp::NONE,
            samples: 0,
            base: None,
        }
    }
}

impl Run {
    fn identify(&mut self, ident: &[u8]) {
        (self.rate, self.den, self.lengths) = match (Mapping::identify(ident), granule_clock(ident))
        {
            (Mapping::Opus, _) => (OPUS_GRANULE_RATE, 1, Lengths::Opus),
            (Mapping::Flac, Some(GranuleClock::Rate(rate))) => (rate, 1, Lengths::Flac),
            (Mapping::Speex, Some(GranuleClock::Rate(rate))) => match speex_packet_samples(ident) {
                Some(n) => (rate, 1, Lengths::Fixed(n)),
                None => (0, 1, Lengths::Opus),
            },
            (_, Some(GranuleClock::Theora(t))) => {
                (t.fps_num, t.fps_den, Lengths::Theora { shift: t.shift, lag: t.lag })
            }
            _ => (0, 1, Lengths::Opus),
        };
        self.stop();
    }

    fn ns(&self, ticks: u64) -> u64 {
        let ns = u128::from(ticks) * 1_000_000_000 * u128::from(self.den) / u128::from(self.rate);
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// The ticks through the packet a page granule ends on.
    fn linear(&self, granule: u64) -> u64 {
        match self.lengths {
            Lengths::Theora { shift, lag } => theora_frames(granule, shift, lag),
            _ => granule,
        }
    }

    /// Until the next stamp the packets go out unstamped.
    fn stop(&mut self) {
        (self.anchor, self.samples, self.base) = (Timestamp::NONE, 0, None);
    }

    fn length(&self, data: &[u8]) -> Option<u32> {
        match (self.rate, self.lengths) {
            (0, _) => None,
            (_, Lengths::Flac) => flac_frame_samples(data),
            (_, Lengths::Opus) => opus_packet_samples(data),
            (_, Lengths::Fixed(n)) => Some(n),
            (_, Lengths::Theora { .. }) => Some(1),
        }
    }

    fn anchored(&self) -> bool {
        self.rate == 0 || self.anchor.is_some()
    }

    /// The granule where `packet` starts, the end of its page less the samples from it on
    /// there, `after` the packets queued behind it. A start before 0, a Speex encoder's
    /// lookahead, is taken as 0.
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
        (end.granule != GRANULE_NONE).then(|| self.linear(end.granule).saturating_sub(samples))
    }

    /// Anchors the clock at the sample of `granule`, its container stamp, ahead of the stream
    /// time by an Opus pre-skip as a demuxer stamps, `offset` the links before it.
    fn anchor_at(&mut self, granule: u64, offset: u64) {
        self.anchor = Timestamp(offset.saturating_add(self.ns(granule)));
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
        let pts = Timestamp(anchor.saturating_add(self.ns(self.samples)));
        let Some(n) = self.length(data) else {
            self.stop();
            return pts;
        };
        self.samples += u64::from(n);
        if last && granule != GRANULE_NONE {
            let granule = self.linear(granule);
            match self.base {
                None => self.base = granule.checked_sub(self.samples),
                Some(base) => self.samples = granule.checked_sub(base).unwrap_or(self.samples),
            }
        }
        pts
    }
}

/// Where a seek pulled goes.
enum Plan {
    Bisect(Bisect),
    /// The start of the chained link of this number.
    Link(usize),
    /// The file's start, its first link, a chain of links not known having been followed.
    Head,
    /// Where the source restarts, stamped with the target.
    Stamp,
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
    /// The duration published, the stream time at the file's end a seek interpolates to.
    ends: Option<u64>,
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
    /// The serials of the first link's bos pages.
    group0: Vec<u32>,
    /// A packet off a bos page came since the last bos, a bos page now starts a chained link.
    data_seen: bool,
    /// The bos pages of a chained link are being read.
    switching: bool,
    /// The stream time of the links before the one being read, and where that one ends so far.
    chain_offset: u64,
    group_end: u64,
    /// The streams of the link being read no track takes, timed for where the link ends.
    strays: Vec<(u32, GranuleClock)>,
    chained: Chained,
    scan: Option<Scan>,
    /// The links of a chained file, two or more.
    links: Option<Links>,
    /// The stream time and first byte of the link a seek bisects in.
    seek_base: (u64, u64),
    /// The key stream and clock of the first link.
    learnt_first: Option<(u32, GranuleClock)>,
}

impl Default for OggDemux {
    // Cold, one-time constructor, the one track of the single stream demuxer.
    #[allow(clippy::disallowed_methods)]
    fn default() -> Self {
        Self {
            reader: OggReader::default(),
            key: None,
            tracks: vec![Track::new(None, 0, Some(Self::OUT))],
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
            ends: None,
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
            group0: Vec::new(),
            data_seen: false,
            switching: false,
            chain_offset: 0,
            group_end: 0,
            strays: Vec::new(),
            chained: Chained::Unknown,
            scan: None,
            links: None,
            seek_base: (0, 0),
            learnt_first: None,
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

    /// The links of a chained file a scan found, so the duration spans them all and a seek lands
    /// in the link of its time. One link changes nothing.
    pub fn with_links(mut self, links: Links) -> Self {
        if links.links.len() > 1 {
            (self.known, self.tail_at) = (Some(links.total_ns()), Tail::In);
            (self.links, self.chained) = (Some(links), Chained::Known);
        }
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
            (self.tracks[0].serial, self.tracks[0].first) = (None, None);
            self.key = None;
        }
    }

    fn stamp_all(&mut self, stamp: Timestamp) {
        for t in &mut self.tracks {
            t.stamp = stamp;
        }
    }

    fn route(&mut self, ctx: &Ctx, serial: u32, bos: bool, data: &[u8]) -> Option<usize> {
        if let Some(at) = self.tracks.iter().position(|t| t.serial == Some(serial)) {
            self.tracks[at].renewed |= bos;
            return Some(at);
        }
        if let Some(first) = self.tracks.first_mut().filter(|t| !self.streams && t.serial.is_none())
        {
            (first.serial, first.first) = (Some(serial), Some(serial));
            self.key = Some(serial);
            return Some(0);
        }
        if !bos || Mapping::identify(data) == Mapping::Skeleton {
            return None;
        }
        self.link_to(ctx, serial, data)
    }

    /// A bos page of a serial no track has, behind data, starts the next link of a chained file
    /// (RFC 3533 section 4). Its stream goes on in the track of the same configuration, stamped
    /// on from where the link before ended. One of another is dropped and the track ends.
    fn link_to(&mut self, ctx: &Ctx, serial: u32, ident: &[u8]) -> Option<usize> {
        if std::mem::take(&mut self.data_seen) {
            self.switching = true;
            self.tracks.iter_mut().for_each(|t| t.renewed = false);
            self.strays.clear();
            self.chain_offset = self.group_end;
        }
        if !self.switching {
            log!(ctx, Level::Debug, "stream_skipped", serial = serial);
            return None;
        }
        if let Some(links) = &self.links
            && let Some(i) = links.of_serial(serial)
        {
            self.chain_offset = links.links[i].start_ns;
        }
        self.group_end = self.group_end.max(self.chain_offset);
        let config = Config::of(ident);
        let Some(at) = self.tracks.iter().position(|t| !t.renewed && t.config == Some(config))
        else {
            let family = config.mapping.family();
            log!(ctx, Level::Warn, "link_changed", serial = serial, family = family);
            if let Some(clock) = granule_clock(ident).filter(|_| self.strays.len() < 64) {
                self.strays.push((serial, clock));
            }
            return None;
        };
        let t = &mut self.tracks[at];
        let old = t.serial.replace(serial);
        (t.offset, t.renewed) = (self.chain_offset, true);
        let offset = Timestamp(t.offset);
        if old.is_some() && self.learnt.map(|(key, _)| key) == old {
            self.learnt = granule_clock(ident).map(|clock| (serial, clock)).or(self.learnt);
        }
        log!(ctx, Level::Info, "link", serial = serial, at = self.taken, offset = offset);
        Some(at)
    }

    // Cold, once per open, a name and a description per stream.
    #[allow(clippy::disallowed_methods)]
    fn describe(&mut self, ctx: &mut Ctx, head: &[u8]) -> Result<(), Error> {
        self.tracks.clear();
        self.group0.clear();
        for s in scan_head(head) {
            let (serial, family) = (s.serial, s.mapping.family());
            self.group0.push(serial);
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
                offers: if s.mapping == Mapping::Theora { &THEORA_OFFERS } else { &OFFERS },
            });
            self.key.get_or_insert(s.serial);
            self.tracks.push(Track::new(Some(s.serial), stream, None));
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
        if self.published {
            return;
        }
        if self.scans(ctx, serial) {
            self.ident = Some(bos.to_vec());
            return;
        }
        self.published = true;
        let total = self.links.as_ref().map(Links::total_ns);
        let duration =
            total.or(self.known).or_else(|| crate::stream_duration_ns(bos, &self.tail, serial));
        self.ends = duration;
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

    /// Starts the link scan of a file whose tail ends on a page of a serial its first link does
    /// not have, a chained one. True while it runs, one running at the end is let go.
    fn scans(&mut self, ctx: &mut Ctx, serial: u32) -> bool {
        match self.chained {
            Chained::Known => return false,
            Chained::Scanning => {
                self.scan = None;
                self.chained = Chained::Known;
                log!(&*ctx, Level::Debug, "chain_scan_dropped");
                return false;
            }
            Chained::Unknown => {}
        }
        self.chained = Chained::Known;
        let Some(len) = self.len.filter(|_| self.known.is_none() && ctx.pulls(Self::IN)) else {
            return false;
        };
        let last = chain::last_page_serial(&self.tail);
        let first = |s: u32| s == serial || self.group0.contains(&s);
        if last.is_none_or(first) {
            return false;
        }
        log!(&*ctx, Level::Debug, "chain_scan", last = last.unwrap_or(0));
        let m = ChainScan::with_tail(len, &self.tail);
        (self.scan, self.chained) = (Some(Scan { m, asked: false, seq: 0 }), Chained::Scanning);
        true
    }

    fn ask_chain(&mut self, ctx: &mut Ctx) {
        let Some(scan) = &mut self.scan else {
            return;
        };
        if scan.asked {
            return;
        }
        match scan.m.step() {
            chain::Step::Read { len, .. } if u64::from(len) > ctx.pool_stats().bytes_budget => {
                log!(&*ctx, Level::Warn, "chain_scan_unread", bytes = len);
                self.end_scan(ctx, None);
            }
            chain::Step::Read { at, len } => {
                let tag = CHAIN_TAG | scan.seq << 2;
                match self.feed.aside(ctx, Self::IN, at, len, tag) {
                    true => scan.asked = true,
                    false => ctx.wake_within(std::time::Duration::from_millis(1)),
                }
            }
            chain::Step::Done => {
                let links = scan.m.links();
                self.end_scan(ctx, Some(links));
            }
        }
    }

    fn chain_in(&mut self, ctx: &mut Ctx, tag: u32, data: &[u8]) {
        let Some(scan) = self.scan.as_mut().filter(|s| s.asked && tag >> 2 == s.seq) else {
            return;
        };
        scan.m.served(data);
        (scan.asked, scan.seq) = (false, (scan.seq + 1) & (u32::MAX >> 2));
        self.ask_chain(ctx);
    }

    // Cold, once per file.
    #[allow(clippy::disallowed_methods)]
    fn end_scan(&mut self, ctx: &mut Ctx, found: Option<Option<Links>>) {
        self.scan = None;
        let links = found.flatten();
        let n = links.as_ref().map_or(0, |l| l.links.len());
        log!(&*ctx, Level::Info, "chain_links", links = n);
        self.links = links.filter(|l| l.links.len() > 1);
        self.publish_kept(ctx);
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
            t.run.anchor_at(g, t.offset);
        }
        if packet.last
            && packet.granule != GRANULE_NONE
            && let Some(ns) = t.clock.and_then(|c| c.ns(packet.granule))
        {
            self.group_end = self.group_end.max(t.offset.saturating_add(ns));
        }
        t.run.stamp(stamp, &packet.data, packet.granule, packet.last)
    }

    fn head(&mut self, at: usize, data: &[u8]) -> bool {
        let track = &mut self.tracks[at];
        let mapping = Mapping::identify(data);
        if mapping != Mapping::Unknown {
            track.heads = mapping.header_packets(data);
            track.flac = mapping == Mapping::Flac;
            track.run.identify(data);
            (track.config, track.clock) = (Some(Config::of(data)), granule_clock(data));
            // the stream sends its headers again itself
            track.greet = false;
            track.kept.clear();
            track.kept_ends.clear();
            track.kept_over = false;
        }
        if track.flac && track.heads > 0 && flac_frame(data) {
            track.heads = 0;
        }
        let head = track.heads > 0;
        track.heads = track.heads.saturating_sub(1);
        head
    }

    fn place(&mut self, ctx: &mut Ctx, packet: &Packet) -> Option<usize> {
        let (serial, bos, data) = (packet.serial, packet.bos, packet.data.as_slice());
        let at = self.route(ctx, serial, bos, data);
        if !bos {
            (self.data_seen, self.switching) = (true, false);
        }
        if at.is_none()
            && packet.granule != GRANULE_NONE
            && let Some((_, clock)) = self.strays.iter().find(|s| s.0 == serial)
            && let Some(ns) = clock.ns(packet.granule)
        {
            self.group_end = self.group_end.max(self.chain_offset.saturating_add(ns));
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
                self.learnt_first.get_or_insert((serial, clock));
            }
        }
        at
    }

    /// The pad of track `at`, owed headers sent first. An unlinked track reads its headers and
    /// keeps its clock for a later link.
    fn out_pad(&mut self, ctx: &mut Ctx, at: usize, head: bool, data: &[u8]) -> Option<SrcPad> {
        let t = &mut self.tracks[at];
        let Some(pad) = t.pad else {
            t.keep_head(head, data);
            return None;
        };
        if std::mem::take(&mut t.greet) {
            t.send_kept(ctx, pad);
        }
        Some(pad)
    }

    /// The track src pad `pad` carries.
    fn track_on(&self, ctx: &Ctx, pad: SrcPad) -> Option<usize> {
        self.tracks.iter().position(|t| t.pad == Some(pad) || ctx.stream_pad(t.stream) == Some(pad))
    }

    /// A link on `pad` made or cut mid-run. A new one gets the headers before what follows, one
    /// made with a restart nothing read ahead of it.
    fn relink(&mut self, ctx: &Ctx, pad: SrcPad, on: bool, restart: Option<SeekTarget>) {
        let Some(at) = self.track_on(ctx, pad) else {
            return;
        };
        let back = on && restart.is_some();
        if back {
            self.drop_read();
        }
        let t = &mut self.tracks[at];
        let (serial, kept) = (t.serial.unwrap_or(0), t.kept_ends.len());
        log!(ctx, Level::Debug, "relink", serial = serial, on = on, heads = kept, restart = back);
        if on && t.kept_over {
            log!(ctx, Level::Warn, "headers_over", serial = t.serial.unwrap_or(0), max = KEPT_MAX);
        }
        // the cut of a pad the track already moved off changes nothing
        if on || t.pad == Some(pad) {
            t.greet = on;
            if self.streams {
                t.pad = on.then_some(pad);
            }
        }
    }

    /// A link that joins `pad` beside the ones it feeds gets the headers at once, the others
    /// are not sent them again. One owed them already gets them with the others.
    fn join(&mut self, ctx: &mut Ctx, pad: SrcPad) {
        let Some(at) = self.track_on(ctx, pad) else {
            return;
        };
        let t = &self.tracks[at];
        if t.kept_over {
            log!(
                &*ctx,
                Level::Warn,
                "headers_over",
                serial = t.serial.unwrap_or(0),
                max = KEPT_MAX
            );
        }
        log!(
            &*ctx,
            Level::Debug,
            "joined",
            serial = t.serial.unwrap_or(0),
            heads = t.kept_ends.len()
        );
        if !t.greet {
            t.send_kept(ctx, pad);
        }
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
            if let Some(pad) = self.out_pad(ctx, c.track, c.head, &c.data) {
                if !Self::emit_packet(ctx, pad, &c.data, c.pts, c.head) {
                    self.pending = Some(c);
                    return false; // budget still spent
                }
                self.tracks[c.track].keep_head(c.head, &c.data);
            }
            self.reader.recycle(c.data);
        }
        let mut budget = EMIT_BUDGET;
        while let Some(packet) = self.reader.next_packet() {
            let placed = self.place(ctx, &packet);
            self.note(packet.serial, packet.granule, packet.data.len());
            self.tell_rate(ctx);
            let skipped = self.skips(packet.serial, packet.granule);
            let Some(at) = placed.filter(|_| !skipped) else {
                self.keep(skipped, packet.data);
                continue;
            };
            let head = self.head(at, &packet.data);
            let pts = self.pts(at, &packet, head);
            let Some(pad) = self.out_pad(ctx, at, head, &packet.data) else {
                self.reader.recycle(packet.data);
                continue;
            };
            self.lead_out(ctx, packet.serial, pad);
            if !Self::emit_packet(ctx, pad, &packet.data, pts, head) {
                self.pending = Some(Carry { data: packet.data, track: at, head, pts });
                return false;
            }
            self.tracks[at].keep_head(head, &packet.data);
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
            if let Some(pad) = self.out_pad(ctx, c.track, c.head, &c.data) {
                Self::emit_exact(ctx, pad, &c.data, c.pts, c.head);
                self.tracks[c.track].keep_head(c.head, &c.data);
            }
            self.reader.recycle(c.data);
        }
        while let Some(packet) = self.reader.next_packet() {
            let placed = self.place(ctx, &packet);
            let skipped = self.skips(packet.serial, packet.granule);
            let Some(at) = placed.filter(|_| !skipped) else {
                self.keep(skipped, packet.data);
                continue;
            };
            let head = self.head(at, &packet.data);
            let pts = self.pts(at, &packet, head);
            if let Some(pad) = self.out_pad(ctx, at, head, &packet.data) {
                self.lead_out(ctx, packet.serial, pad);
                Self::emit_exact(ctx, pad, &packet.data, pts, head);
                self.tracks[at].keep_head(head, &packet.data);
            }
            self.reader.recycle(packet.data);
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

    /// What was read before a flush or a restart goes, the packet parked on the budget too, else
    /// it goes out first and a decoder linked behind the restart places itself by it.
    fn drop_read(&mut self) {
        self.reader.reset();
        if let Some(c) = self.pending.take() {
            self.reader.recycle(c.data);
        }
        self.stamp_all(Timestamp::NONE);
        self.tracks.iter_mut().for_each(|t| t.run.stop());
        (self.data_seen, self.switching) = (false, false);
        self.unland();
        (self.at, self.measured) = (Timestamp::NONE, rate::ByteRate::default());
        self.teller.restart();
    }

    /// Where a seek pulled to `from` goes. In a chained file a seek in the link being read
    /// bisects its bytes, one in another reads that link from its bos pages, its headers going
    /// out again.
    fn plan(&mut self, ctx: &Ctx, from: SeekTarget) -> Plan {
        self.seek_base = (0, 0);
        let len = self.len.filter(|len| *len > 0 && ctx.pulls(Self::IN));
        let target = ctx.presentation_point().unwrap_or(from.at).nanos();
        let (Some(len), Some((key, clock)), Some(target)) = (len, self.learnt, target) else {
            return Plan::Stamp;
        };
        let target = target.saturating_sub(match clock {
            GranuleClock::Opus { .. } => OPUS_PREROLL_NS,
            GranuleClock::Rate(_) | GranuleClock::Theora(_) => 0,
        });
        let Some(links) = &self.links else {
            // a chain of links not known, the first is read again from the start
            if self.tracks.iter().any(|t| t.serial != t.first) {
                return Plan::Head;
            }
            return Plan::Bisect(Bisect::new(key, clock, target, len).ending(self.ends));
        };
        let k = links.at_time(target);
        if links.of_serial(key) != Some(k) {
            return Plan::Link(k);
        }
        let link = &links.links[k];
        self.seek_base = (link.start_ns, link.at);
        let b = Bisect::new(key, clock, target.saturating_sub(link.start_ns), len);
        Plan::Bisect(b.within(link.at, links.end(k)).ending(Some(link.ns)))
    }

    /// Reads from `at`, the first byte of a link starting at `start_ns`, each track stamped from
    /// its first packet on.
    fn land_at_link(&mut self, at: u64, start_ns: u64) {
        self.feed.restart(at);
        for t in &mut self.tracks {
            t.stamp = match t.run.rate {
                0 => Timestamp(start_ns),
                _ => Timestamp::NONE,
            };
        }
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
        let Some(l) = landing else {
            self.feed.restart(from.to_byte);
            self.stamp_all(from.at);
            return;
        };
        let (offset, link_at) = self.seek_base;
        let head = l == Landing::HEAD;
        if head && link_at > 0 {
            return self.land_at_link(link_at, offset);
        }
        self.feed.restart(l.at);
        self.skip = (!head).then_some(l.granule);
        let (key, clock) = self.learnt.unzip();
        let ns = clock.and_then(|c| c.stamp(l.granule)).unwrap_or(l.ns);
        let stamp = Timestamp(offset.saturating_add(ns));
        // The landing page's time is the key stream's alone. A track whose packet lengths are
        // read anchors on its own page granule, the others take the landing's, 0 at the head.
        for t in &mut self.tracks {
            t.stamp = match (t.run.rate, head) {
                (0, true) => Timestamp::ZERO,
                (0, false) => stamp,
                (_, false) if t.serial == key => stamp,
                _ => Timestamp::NONE,
            };
        }
    }

    fn restart_pull(&mut self, ctx: &mut Ctx, from: SeekTarget) {
        self.feed.restart(from.to_byte);
        self.drop_read();
        // a scan read out before the restart is asked again, a late one is told by its tag
        if let Some(scan) = self.scan.as_mut().filter(|s| s.asked) {
            (scan.asked, scan.seq) = (false, (scan.seq + 1) & (u32::MAX >> 2));
        }
        // A source stamps nothing on its first byte read unasked.
        if std::mem::replace(&mut self.paired, true) {
            let plan = self.plan(ctx, from);
            let planned = matches!(plan, Plan::Bisect(_));
            log!(
                &*ctx,
                Level::Debug,
                "seek",
                target = from.at,
                at = from.to_byte,
                bisect = planned
            );
            match plan {
                Plan::Bisect(b) => self.bisect = Some((b, from)),
                Plan::Stamp => self.stamp_all(from.at),
                Plan::Head => {
                    self.tracks.iter_mut().for_each(Track::rewind);
                    (self.chain_offset, self.group_end) = (0, 0);
                    self.learnt = self.learnt_first;
                    self.land_at_link(0, 0);
                }
                Plan::Link(k) => {
                    let (at, start) = self.links.as_ref().map_or((0, 0), |l| {
                        let link = &l.links[k];
                        (link.at, link.start_ns)
                    });
                    log!(&*ctx, Level::Debug, "seek_link", link = k, at = at);
                    self.data_seen = true;
                    self.land_at_link(at, start);
                }
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
        self.ask_chain(ctx);
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
                Some(Fed::Aside { tag, buf }) if tag & 3 == CHAIN_TAG => {
                    self.chain_in(ctx, tag, buf.memory.data())
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
        // the duration waits on the scan, the end with it
        if self.scan.is_some() {
            ctx.hold_end();
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
            (t.stamp, t.heads, t.greet) = (Timestamp::NONE, 0, false);
            t.run.stop();
            t.rewind();
            if self.streams {
                t.pad = ctx.stream_pad(t.stream).filter(|pad| ctx.pad_linked(*pad));
            }
        }
        self.pending = None;
        (self.clock, self.taken, self.at) = (None, 0, Timestamp::NONE);
        (self.measured, self.teller) = (rate::ByteRate::default(), rate::Teller::default());
        ctx.want_ranges(Self::IN);
        (self.paired, self.seen) = (false, (0, 0));
        (self.data_seen, self.switching) = (false, false);
        (self.chain_offset, self.group_end) = (0, 0);
        self.strays.clear();
        self.learnt = self.learnt_first.or(self.learnt);
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
            // The locked serial is kept so a grouped file does not switch streams mid-playback, and
            // a link still owed the headers gets them ahead of the first packet behind the seek.
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", seek = ctx.seek_target().is_some());
                self.drop_read();
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
            Event::SrcLinked { pad, on, restart } => self.relink(ctx, *pad, *on, *restart),
            Event::SrcJoined { pad } => self.join(ctx, *pad),
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

    #[test]
    fn headers_past_the_cap_are_dropped_and_a_later_link_gets_none() {
        let mut t = Track::new(None, 0, None);
        let block = vec![0u8; 1 << 20];
        for _ in 0..4 {
            t.keep_head(true, &block);
        }
        assert_eq!((t.kept_ends.len(), t.kept.len()), (4, KEPT_MAX));
        t.keep_head(false, &block);
        assert_eq!(t.kept_ends.len(), 4, "media is not kept");
        t.keep_head(true, &[0]);
        assert!(t.kept_over && t.kept.is_empty() && t.kept_ends.is_empty());
        t.keep_head(true, &[0]);
        assert!(t.kept.is_empty(), "nothing kept once over");
    }

    /// A mapping header naming 65535 header packets, the frames after STREAMINFO are media.
    #[test]
    fn a_flac_header_count_read_wrong_ends_at_the_first_frame() {
        let mut d = OggDemux::new();
        let mut mapping = [0u8; 17 + 34];
        mapping[..17].copy_from_slice(b"\x7fFLAC\x01\x00\xff\xfffLaC\x80\x00\x00\x22");
        assert!(d.head(0, &mapping), "the mapping packet");
        assert!(d.head(0, &[0x84, 0, 0, 4, 0, 0, 0, 0]), "a metadata block");
        assert!(!d.head(0, &[0xff, 0xf8, 0x69, 0x08]), "a frame");
        assert!(!d.head(0, &[0x84, 0, 0, 4, 0, 0, 0, 0]), "nothing after the first frame");
        assert!(flac_frame(&[0xff, 0xf9]) && !flac_frame(&[0xff, 0xfc]) && !flac_frame(&[0x84]));
    }

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

    /// 25/2 frames a second, KFGSHIFT 6, version 3.2.1.
    #[allow(clippy::disallowed_methods)]
    fn theora() -> Run {
        let mut ident = b"\x80theora\x03\x02\x01\0\x0a\0\x08\0\0\xa0\0\0\x78\0\0".to_vec();
        ident.extend_from_slice(&[0, 0, 0, 25, 0, 0, 0, 2, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0]);
        ident.extend_from_slice(&[0, 6 << 5]);
        let mut r = Run::default();
        r.identify(&ident);
        r
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn theora_frames_are_timed_by_the_frame_rate_and_a_granule_names_a_frame() {
        const FRAME: u64 = 80_000_000;
        let mut r = theora();
        assert_eq!((r.rate, r.den), (25, 2));
        // a keyframe at count 1 and the 3 frames behind it, through count 4
        assert_eq!(r.linear(1 << 6 | 3), 4);
        assert_eq!(r.linear(2 << 6 | 3), 5, "past a second keyframe");
        // a 3.2.0 granule names the frame's index, one short of the count through it
        let old = Run { lengths: Lengths::Theora { shift: 6, lag: 1 }, ..r };
        assert_eq!(old.linear(2 << 6 | 3), 6);
        let page = [b"f".to_vec(), b"f".to_vec(), b"f".to_vec()];
        let packet = |i: usize, granule: u64| Packet {
            data: page[i].clone(),
            granule,
            last: i == 2,
            serial: 1,
            bos: false,
            eos: false,
        };
        // frames 4 to 6 on a page ending at count 7, two past the keyframe at count 5
        let (first, rest) =
            (packet(0, GRANULE_NONE), [packet(1, GRANULE_NONE), packet(2, 5 << 6 | 2)]);
        assert_eq!(r.start(&first, rest.iter()), Some(4));
        r.anchor_at(4, 0);
        assert_eq!(r.stamp(Timestamp::NONE, b"f", GRANULE_NONE, false), Timestamp(4 * FRAME));
        // a zero byte packet repeats a frame and takes its slot
        assert_eq!(r.stamp(Timestamp::NONE, b"", GRANULE_NONE, false), Timestamp(5 * FRAME));
        assert_eq!(r.stamp(Timestamp::NONE, b"f", 5 << 6 | 2, true), Timestamp(6 * FRAME));
        // frame 7 lost, the granule of the page frame 8 ends puts the clock back
        r.stamp(Timestamp::NONE, b"f", 5 << 6 | 4, true);
        assert_eq!(r.stamp(Timestamp::NONE, b"f", 10 << 6, true), Timestamp(9 * FRAME));
    }

    /// A Speex page of 50 packets of 320 samples at 16 kHz, its granule 143 short of their sum
    /// as libspeex writes the first, the encoder's lookahead.
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn speex_packets_are_counted_back_from_their_page_granule() {
        let mut ident = b"Speex   ".to_vec();
        ident.resize(28, 0);
        for f in [1i32, 80, 16_000, 1, 4, 1, 27_800, 320, 0, 1, 0, 0, 0] {
            ident.extend_from_slice(&f.to_le_bytes());
        }
        let mut r = Run::default();
        r.identify(&ident);
        assert_eq!((r.rate, r.lengths), (16_000, Lengths::Fixed(320)));
        let packet = |last: bool, granule: u64| Packet {
            data: b"s".to_vec(),
            granule,
            last,
            serial: 1,
            bos: false,
            eos: false,
        };
        let page: Vec<Packet> = (0..50).map(|i| packet(i == 49, 15_857)).collect();
        assert_eq!(r.start(&page[0], page[1..].iter()), Some(0), "a start before 0 is 0");
        assert_eq!(r.start(&page[1], page[2..].iter()), Some(177));
        r.anchor_at(0, 0);
        for (i, p) in page.iter().enumerate() {
            let pts = r.stamp(Timestamp::NONE, &p.data, p.granule, p.last);
            assert_eq!(pts, Timestamp(i as u64 * MS20), "packet {i}");
        }
        // the page granule puts the next page on its own time, 15857 samples in
        let next = r.stamp(Timestamp::NONE, b"s", 31_857, false);
        assert_eq!(next, Timestamp(15_857 * 1_000_000_000 / 16_000));
    }

    #[test]
    fn a_mapping_of_unread_lengths_stamps_only_what_it_was_given() {
        let mut r = Run::default();
        r.identify(b"\x01vorbis");
        assert_eq!(r.stamp(Timestamp(7), b"x", 100, true), Timestamp(7));
        assert_eq!(r.stamp(Timestamp::NONE, b"x", 200, true), Timestamp::NONE);
    }
}
