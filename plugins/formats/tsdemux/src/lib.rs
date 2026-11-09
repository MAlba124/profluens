//! `tsdemux`, the MPEG-TS demuxer element over the sans-IO [`pf_mpegts`]. A video PES packet is
//! one access unit (RFC 8216 3.2), AAC in ADTS leaves as raw access units behind its config. A DVB
//! subtitle PES packet leaves whole, behind the page ids its decoder filters by.

#![deny(unsafe_code)]

pub mod es;
pub mod index;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use pf_mpegts::{
    Codec, Config, ConfigError, Demux, Encryption, Limits, PesUnit, Step, StreamInfo, Unwrapper,
};
use pf_mpegts::{UF_DISCONTINUITY, UF_ENCRYPTED, UF_RANDOM_ACCESS, UF_SCRAMBLED};
use pf_registry::{Elements, Registry};
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
use profluens_core::rate;
use profluens_core::time::Timestamp;

use es::Adts;
use index::{Grown, Key, Placed};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TsError {
    Config(ConfigError),
    NoProgram,
    NoSuchProgram {
        number: u16,
    },
    /// Every stream is HLS Sample-AES encrypted, which nothing here decrypts.
    Encrypted,
    NotPrerolled,
}

impl fmt::Display for TsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TsError::Config(e) => write!(f, "tsdemux: {e}"),
            TsError::NoProgram => {
                f.write_str("tsdemux: no program map in the bytes the element was made from")
            }
            TsError::NoSuchProgram { number } => {
                write!(f, "tsdemux: no program map for program {number}")
            }
            TsError::Encrypted => f.write_str("tsdemux: every stream of the program is encrypted"),
            TsError::NotPrerolled => {
                f.write_str("tsdemux: started before preroll found the streams")
            }
        }
    }
}

impl std::error::Error for TsError {}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static VIDEO_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: pf_hdr::color::FIELD_DOVI, allowed: ConstraintDesc::Any, preferred: None }];

/// Video families again with the record's field, after the bare ones so a link fixes none.
static SRC_OFFERS: [OfferDesc; 13] = [
    OfferDesc::any("bytes"),
    OfferDesc::any("h264/annexb"),
    OfferDesc::any("h265/annexb"),
    OfferDesc { family: "h264/annexb", fields: &VIDEO_FIELDS },
    OfferDesc { family: "h265/annexb", fields: &VIDEO_FIELDS },
    OfferDesc::any("mpeg1"),
    OfferDesc::any("mpeg2"),
    OfferDesc::any("mpeg4/asp"),
    OfferDesc::any("aac"),
    OfferDesc::any("ac3"),
    OfferDesc::any("eac3"),
    OfferDesc::any("mp3"),
    OfferDesc::any(DVB_FAMILY),
];

/// Every family the src pads offer, `bytes` aside.
pub static EMITS: &[&str] = &[
    "h264/annexb",
    "h265/annexb",
    "mpeg1",
    "mpeg2",
    "mpeg4/asp",
    "aac",
    "ac3",
    "eac3",
    "mp3",
    DVB_FAMILY,
];

/// As Matroska `S_DVBSUB` names it and `dvbsubdec` takes it.
const DVB_FAMILY: &str = "subtitle/dvb";

/// Composition and ancillary page ids and subtitling_type, the `S_DVBSUB` CodecPrivate layout
/// (Matroska subtitles, DVB section), which `dvbsubdec` reads from its first buffer.
type Pages = [u8; 5];

static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];

static DESC: ElementDesc = ElementDesc {
    name: "tsdemux",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Made from the first bytes of its stream, so not by name.
    make_default: None,
    io: false,
    couplings: &[],
};

/// A transport packet ends at most one unit, so a piece ends at most 22.
const PIECE: usize = 22 * 188;

/// Slack below a file's first decode time for a stamp behind a seek, streams start within 1 s.
const ORIGIN_SLACK: i64 = 10 * 90_000;

fn ticks_ns(ticks: i64) -> i64 {
    (i128::from(ticks) * 100_000 / 9).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// The families pf-ps and pf-mkv emit for the same codecs. MPEG-1 video is `mpeg1`, which
/// libavmpeg2dec takes beside `mpeg2`.
fn family_of(codec: Codec) -> (&'static str, StreamKind) {
    match codec {
        Codec::Mpeg1Video => ("mpeg1", StreamKind::Video),
        Codec::Mpeg2Video => ("mpeg2", StreamKind::Video),
        Codec::Mpeg4Visual => ("mpeg4/asp", StreamKind::Video),
        Codec::H264 => ("h264/annexb", StreamKind::Video),
        Codec::Hevc => ("h265/annexb", StreamKind::Video),
        Codec::AacAdts => ("aac", StreamKind::Audio),
        Codec::Ac3 => ("ac3", StreamKind::Audio),
        Codec::Eac3 => ("eac3", StreamKind::Audio),
        // Every layer, as Matroska and MP4 name it.
        Codec::MpegAudio => ("mp3", StreamKind::Audio),
        Codec::DvbSubtitle => (DVB_FAMILY, StreamKind::Text),
        Codec::Teletext => ("bytes", StreamKind::Text),
        _ => ("bytes", StreamKind::Other),
    }
}

struct Track {
    pid: u16,
    codec: Codec,
    family: &'static str,
    kind: StreamKind,
    stream: u32,
    pad: Option<SrcPad>,
    started: bool,
    /// Units are dropped until one a decoder can start at.
    need_key: bool,
    adts: Option<(u8, u8, u8)>,
    /// An ADTS frame start whose rest is in the next PES packet.
    carry: Vec<u8>,
    /// Stamp of the last PES that carried a frame and samples since, so rounding does not gather.
    run: Option<(u64, u64)>,
    /// Last decode time in ticks, and the stretch it was read in.
    last_dts: (u32, i64),
    /// Start lag behind the program's earliest stream in the head, ns.
    lead_ns: u64,
    dovi: Option<i64>,
    /// Due after a start, a flush or a relink, where a subtitle decoder waits for them, not past a
    /// spliced segment.
    pages: Option<Pages>,
    pages_due: bool,
    /// Units dropped as scrambled or encrypted, and bytes skipped between ADTS frames.
    scrambled: u64,
    adts_skipped: u64,
    /// The offset of the last unit taken in, and behind a restart for another track the one
    /// units read again are dropped up to.
    read: Option<u64>,
    resume: Option<u64>,
    /// The next unit's discontinuity is the restart's, not the stream's.
    quiet: bool,
}

enum Parked {
    Unit { pad: SrcPad, pts: Option<u64>, flags: BufferFlags, from: usize, to: usize },
    Format { pad: SrcPad, family: &'static str, rate: u32, channels: u32, dovi: Option<i64> },
}

#[derive(Default)]
struct Park {
    bytes: Vec<u8>,
    rows: VecDeque<Parked>,
}

/// A file's earliest decode time is zero across seeks. A segment mark with a `pts` anchors the next
/// decode time there. A new time base (PCR discontinuity, a 10 s step, DTS going back) resumes.
struct Clock {
    unwrap: Unwrapper,
    shift: i64,
    /// A stream time waiting for the first decode time behind a mark or a break.
    anchor: Option<u64>,
    /// The first decode time of a file in ticks, kept across seeks.
    origin: Option<i64>,
    sought: bool,
    last_ns: u64,
    stretch: u32,
    broke: bool,
    marked: bool,
}

impl Clock {
    /// `(decode, presentation)` in stream time ns, `None` for a unit without timestamps.
    fn place(
        &mut self,
        unit: &PesUnit<'_>,
        last_dts: &mut (u32, i64),
        lead_ns: u64,
    ) -> Option<(u64, u64)> {
        let pts = unit.pts?;
        let dts = unit.dts.unwrap_or(pts);
        if std::mem::take(&mut self.sought) {
            // The congruent value at or past the file's start, less the slack.
            if let Some(origin) = self.origin {
                let floor = origin - ORIGIN_SLACK;
                let modulus = 1i64 << 33;
                self.unwrap.prime(floor + (dts as i64 - floor).rem_euclid(modulus));
            }
        }
        let placed = self.unwrap.unwrap(dts);
        let behind = *last_dts != (0, 0) && last_dts.0 == self.stretch && placed.ticks < last_dts.1;
        let broke = std::mem::take(&mut self.broke) || placed.step == Step::Discontinuity || behind;
        if broke && self.anchor.is_none() {
            self.anchor = Some(self.last_ns);
        }
        let decode = ticks_ns(placed.ticks);
        if let Some(stream) = self.anchor.take() {
            let lands = stream.saturating_add(if self.marked { lead_ns } else { 0 });
            self.marked = false;
            self.shift = decode.saturating_sub(i64::try_from(lands).unwrap_or(i64::MAX));
            self.stretch = self.stretch.wrapping_add(1).max(1);
        }
        *last_dts = (self.stretch, placed.ticks);
        self.origin.get_or_insert(placed.ticks);
        let present = ticks_ns(self.unwrap.unwrap(pts).ticks);
        // Prime at the decode time, which ascends.
        self.unwrap.prime(placed.ticks);
        let stamp = |ns: i64| ns.saturating_sub(self.shift).max(0) as u64;
        let (decode, present) = (stamp(decode), stamp(present));
        self.last_ns = present;
        Some((decode, present))
    }
}

/// One sink pad, one src pad per elementary stream.
pub struct TsDemux {
    /// Read at preroll for the program map.
    head: Vec<u8>,
    /// The program to play. `None` is the first one mapped.
    program: Option<u16>,
    limits: Limits,
    demux: Option<Demux>,
    tracks: Vec<Track>,
    clock: Clock,
    park: Park,
    feeding: Option<(Memory, usize)>,
    /// At the end, what does not fit a slot goes out in its own allocation.
    ending: bool,
    /// First decode time in the head and the earliest stream's first one, in ticks.
    opening: Option<(i64, i64)>,
    grown: Grown,
    heads: Option<Arc<AacHeads>>,
    /// Rate off the key stream's units, bytes against stamps.
    measured: rate::ByteRate,
    teller: rate::Teller,
    /// Demuxer anomalies since start, the first logged at warn.
    anomalies: u64,
}

impl TsDemux {
    pub const IN: SinkPad = SinkPad(0);

    /// `head` has to reach past the first PAT and PMT, a few kilobytes, and may be cut anywhere.
    #[allow(clippy::disallowed_methods)] // once per element
    pub fn new(head: Vec<u8>) -> Self {
        Self {
            head,
            program: None,
            limits: limits(),
            demux: None,
            tracks: Vec::new(),
            clock: Clock {
                unwrap: Unwrapper::for_timestamps(),
                shift: 0,
                anchor: None,
                origin: None,
                sought: false,
                last_ns: 0,
                stretch: 1,
                broke: false,
                marked: false,
            },
            park: Park::default(),
            feeding: None,
            measured: rate::ByteRate::default(),
            teller: rate::Teller::default(),
            ending: false,
            opening: None,
            grown: Grown::new(),
            heads: None,
            anomalies: 0,
        }
    }

    #[must_use]
    pub fn with_heads(mut self, heads: Arc<AacHeads>) -> Self {
        self.heads = Some(heads);
        self
    }

    /// For the index's proportional estimate past its last entry.
    #[must_use]
    pub fn with_len(mut self, len: u64) -> Self {
        self.grown.file_len = Some(len);
        self
    }

    /// A file an opener found lasts `ns`, published from the start so a seek ahead of what was
    /// read is estimated on the file's mean rate.
    #[must_use]
    pub fn with_duration(mut self, ns: u64) -> Self {
        self.grown.duration = (ns > 0).then_some(ns);
        self
    }

    #[must_use]
    pub fn program(mut self, number: u16) -> Self {
        self.program = Some(number);
        self
    }

    fn failed(ctx: &Ctx, error: TsError) -> Error {
        Error::element_failed(ctx.element(), error)
    }

    /// With something parked already everything parks behind it, so the order holds.
    fn put(
        ctx: &mut Ctx,
        park: &mut Park,
        exact: bool,
        pad: SrcPad,
        pts: Option<u64>,
        flags: BufferFlags,
        bytes: &[u8],
    ) {
        if bytes.is_empty() {
            return;
        }
        if park.rows.is_empty() && Self::write(ctx, exact, pad, pts, flags, bytes) {
            return;
        }
        let from = park.bytes.len();
        park.bytes.extend_from_slice(bytes);
        park.rows.push_back(Parked::Unit { pad, pts, flags, from, to: park.bytes.len() });
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
                    ctx.post(BusMessage::Warning {
                        element,
                        error: Error::element_failed(element, e),
                    });
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

    fn announce(
        ctx: &mut Ctx,
        park: &mut Park,
        pad: SrcPad,
        family: &'static str,
        (rate, channels): (u32, u32),
        dovi: Option<i64>,
    ) {
        if !park.rows.is_empty() {
            return park.rows.push_back(Parked::Format { pad, family, rate, channels, dovi });
        }
        match (rate, channels) {
            (0, _) => match dovi {
                Some(d) => ctx.announce_format(
                    pad,
                    family,
                    &[(pf_hdr::color::FIELD_DOVI, ValueDesc::Int(d))],
                ),
                None => ctx.announce_format(pad, family, &[]),
            },
            (rate, 0) => {
                ctx.announce_format(pad, family, &[("rate", ValueDesc::Int(i64::from(rate)))])
            }
            (rate, channels) => ctx.announce_format(
                pad,
                family,
                &[
                    ("rate", ValueDesc::Int(i64::from(rate))),
                    ("channels", ValueDesc::Int(i64::from(channels))),
                ],
            ),
        }
    }

    /// Put out what is parked, in order. False while the pool is still dry.
    fn unpark(&mut self, ctx: &mut Ctx) -> bool {
        let exact = self.ending;
        while let Some(row) = self.park.rows.front() {
            match *row {
                Parked::Unit { pad, pts, flags, from, to } => {
                    if !Self::write(ctx, exact, pad, pts, flags, &self.park.bytes[from..to]) {
                        return false;
                    }
                }
                Parked::Format { pad, family, rate, channels, dovi } => {
                    self.park.rows.pop_front();
                    Self::announce(ctx, &mut Park::default(), pad, family, (rate, channels), dovi);
                    continue;
                }
            }
            self.park.rows.pop_front();
        }
        self.park.bytes.clear();
        true
    }

    fn publish(ctx: &Ctx, grown: &mut Grown) {
        ctx.publish_index(grown.index());
    }

    #[allow(clippy::too_many_arguments)]
    fn on_unit(
        ctx: &mut Ctx,
        tracks: &mut [Track],
        clock: &mut Clock,
        park: &mut Park,
        grown: &mut Grown,
        (measured, teller): (&mut rate::ByteRate, &mut rate::Teller),
        heads: Option<&AacHeads>,
        exact: bool,
        unit: &PesUnit<'_>,
    ) {
        let Some(track) = tracks.iter_mut().find(|track| track.pid == unit.pid) else { return };
        let Some(pad) = track.pad else { return };
        let mut bits = unit.flags;
        if std::mem::take(&mut track.quiet) {
            bits &= !UF_DISCONTINUITY;
        }
        // Read again behind a restart for another track, it went out the first time.
        if track.resume.is_some_and(|last| unit.offset <= last) {
            return;
        }
        (track.resume, track.read) = (None, Some(unit.offset));
        // Not in the clear, nothing downstream can use it.
        if unit.flags & (UF_SCRAMBLED | UF_ENCRYPTED) != 0 {
            if track.scrambled == 0 {
                log!(&*ctx, Level::Warn, "scrambled", pid = unit.pid, at = unit.offset);
            }
            track.scrambled += 1;
            return;
        }
        let stamps = clock.place(unit, &mut track.last_dts, track.lead_ns);
        let broke = bits & UF_DISCONTINUITY != 0;
        let mut flags = if broke { BufferFlags::DISCONT } else { BufferFlags::empty() };
        let keys = grown.keys(unit.pid);
        // Every unit of audio decodes alone.
        let mut entry = keys;
        if track.kind == StreamKind::Video {
            track.need_key |= broke;
            let (key, marked) = match track.codec {
                Codec::H264 | Codec::Hevc => {
                    let marked = bits & UF_RANDOM_ACCESS != 0;
                    // The first slice is read only where the flag does not say.
                    let hevc = track.codec == Codec::Hevc;
                    let key = marked
                        || ((track.need_key || keys)
                            && es::starts_with_key(unit.payload, hevc) == Some(true));
                    (key, marked)
                }
                // The picture header leads the unit, behind at most the sequence and GOP headers,
                // and says more than the optional flag muxers set unevenly.
                _ => {
                    let mpeg4 = track.codec == Codec::Mpeg4Visual;
                    let key = es::starts_intra(unit.payload, mpeg4) == Some(true);
                    (key, key)
                }
            };
            entry = keys && key;
            if track.need_key {
                if !key {
                    log!(&*ctx, Level::Trace, "unit_before_key", pid = unit.pid, at = unit.offset);
                    return;
                }
                track.need_key = false;
                flags |= BufferFlags::KEYFRAME;
            } else if marked {
                flags |= BufferFlags::KEYFRAME;
            }
        }
        if let Some((_, present)) = stamps.filter(|_| entry)
            && grown.keyframe(present, unit.offset)
        {
            Self::publish(ctx, grown);
        }
        // Measured at the key unit's byte, so neither feeding ahead nor other streams move it.
        if let Some((_, present)) =
            stamps.filter(|_| grown.key.is_some_and(|key| key.pid == unit.pid))
        {
            measured.note(unit.offset, Timestamp(present));
            let rate = measured.rate();
            teller.tell_at(ctx, Self::IN, Timestamp(present), |_| rate);
        }
        if track.codec == Codec::AacAdts {
            return Self::on_adts(
                ctx,
                track,
                park,
                heads,
                exact,
                pad,
                stamps.map(|s| s.1),
                flags,
                unit.payload,
            );
        }
        if !track.started {
            track.started = true;
            Self::announce(ctx, park, pad, track.family, (0, 0), track.dovi);
        }
        if let Some(pages) = track.pages.filter(|_| std::mem::take(&mut track.pages_due)) {
            Self::put(ctx, park, exact, pad, stamps.map(|s| s.1), BufferFlags::HEADER, &pages);
        }
        Self::put(ctx, park, exact, pad, stamps.map(|s| s.1), flags, unit.payload);
    }

    /// The PES stamp is the first frame starting in it (2.4.3.7), the rest follow by duration.
    #[allow(clippy::too_many_arguments)]
    fn on_adts(
        ctx: &mut Ctx,
        track: &mut Track,
        park: &mut Park,
        heads: Option<&AacHeads>,
        exact: bool,
        pad: SrcPad,
        pts: Option<u64>,
        flags: BufferFlags,
        mut data: &[u8],
    ) {
        if flags.contains(BufferFlags::DISCONT) {
            track.carry.clear();
            track.run = None;
        }
        if !track.carry.is_empty() {
            let need = Adts::parse(&track.carry).map(|h| h.frame_len);
            let need = match need {
                Some(need) => need,
                None => {
                    let take = (7 - track.carry.len().min(7)).min(data.len());
                    track.carry.extend_from_slice(&data[..take]);
                    data = &data[take..];
                    Adts::parse(&track.carry).map_or(0, |h| h.frame_len)
                }
            };
            let take = need.saturating_sub(track.carry.len()).min(data.len());
            track.carry.extend_from_slice(&data[..take]);
            data = &data[take..];
            let carry = std::mem::take(&mut track.carry);
            if need != 0 && carry.len() == need {
                Self::adts_frame(ctx, track, park, heads, exact, pad, flags, &carry);
            }
            track.carry = carry;
            if track.carry.len() < need {
                return;
            }
            track.carry.clear();
        }
        if let Some(pts) = pts {
            track.run = Some((pts, 0));
        }
        while !data.is_empty() {
            let Some(h) = Adts::parse(data) else {
                // The tail of a packet may hold the start of a header.
                if data.len() < 7 && data[0] == 0xFF {
                    track.carry.extend_from_slice(data);
                    return;
                }
                if track.adts_skipped == 0 {
                    log!(&*ctx, Level::Warn, "adts_resync", pid = track.pid);
                }
                track.adts_skipped += 1;
                data = &data[1..];
                continue;
            };
            if h.frame_len > data.len() {
                track.carry.extend_from_slice(data);
                return;
            }
            Self::adts_frame(ctx, track, park, heads, exact, pad, flags, &data[..h.frame_len]);
            data = &data[h.frame_len..];
        }
    }

    /// Payload goes out behind the AudioSpecificConfig on the first frame or a change.
    #[allow(clippy::too_many_arguments)]
    fn adts_frame(
        ctx: &mut Ctx,
        track: &mut Track,
        park: &mut Park,
        heads: Option<&AacHeads>,
        exact: bool,
        pad: SrcPad,
        flags: BufferFlags,
        frame: &[u8],
    ) {
        let Some(h) = Adts::parse(frame) else { return };
        if !track.started || track.adts != Some(h.key()) {
            (track.started, track.adts) = (true, Some(h.key()));
            Self::announce(ctx, park, pad, track.family, (h.rate(), u32::from(h.channels)), None);
            let from = track.run.map(|run| run.0);
            Self::put(ctx, park, exact, pad, from, BufferFlags::HEADER, &h.config());
            if let Some(heads) = heads {
                heads.put(track.pid, from, h.config());
            }
        }
        let stamp = track.run.map(|(base, samples)| {
            base.saturating_add((u128::from(samples) * 1_000_000_000 / u128::from(h.rate())) as u64)
        });
        let payload = frame.get(h.header_len..h.frame_len).unwrap_or(&[]);
        Self::put(ctx, park, exact, pad, stamp, flags | BufferFlags::KEYFRAME, payload);
        track.run = track.run.map(|(base, samples)| (base, samples + u64::from(h.blocks) * 1024));
    }

    /// With `ctx` the units in progress are whole and go out, without, behind a seek, they drop.
    fn break_stream(&mut self, ctx: Option<&mut Ctx>) {
        // A flush resets a subtitle decoder, a spliced segment does not.
        let flushed = ctx.is_none();
        match ctx {
            Some(ctx) => self.feed(ctx, &[], true),
            None => {
                if let Some(demux) = self.demux.as_mut() {
                    demux.discontinuity();
                }
            }
        }
        for track in &mut self.tracks {
            (track.started, track.run) = (false, None);
            track.need_key = track.kind == StreamKind::Video;
            track.pages_due |= flushed;
            track.carry.clear();
            (track.read, track.resume, track.quiet) = (None, None, false);
        }
    }

    fn feed(&mut self, ctx: &mut Ctx, data: &[u8], end: bool) {
        let Self {
            demux,
            tracks,
            clock,
            park,
            grown,
            ending,
            heads,
            measured,
            teller,
            anomalies,
            ..
        } = self;
        let heads = heads.as_deref();
        let Some(demux) = demux.as_mut() else { return };
        let exact = *ending;
        let mut sink = |event: pf_mpegts::Event<'_>| match event {
            pf_mpegts::Event::Pes(unit) => Self::on_unit(
                ctx,
                tracks,
                clock,
                park,
                grown,
                (measured, teller),
                heads,
                exact,
                &unit,
            ),
            pf_mpegts::Event::Pcr { discontinuity: true, pid, offset, .. } => {
                log!(&*ctx, Level::Debug, "pcr_break", pid = pid, at = offset);
                clock.broke = true
            }
            pf_mpegts::Event::Pat { version, programs, .. } => {
                log!(&*ctx, Level::Debug, "pat", version = version, programs = programs.len());
            }
            pf_mpegts::Event::Pmt { program, streams } => {
                let (number, version) = (program.number, program.version);
                let n = streams.len();
                log!(&*ctx, Level::Debug, "pmt", program = number, version = version, streams = n);
            }
            pf_mpegts::Event::Anomaly(a) => {
                // A unit without a stamp is legal and common, so neither warns nor counts.
                let legal = a.kind == pf_mpegts::AnomalyKind::PesTimestamp;
                let level = if legal || *anomalies > 0 { Level::Trace } else { Level::Warn };
                let (kind, pid, at, detail) = (a.kind.name(), a.pid, a.offset, a.detail);
                log!(&*ctx, level, "anomaly", kind = kind, pid = pid, at = at, detail = detail);
                *anomalies += u64::from(!legal);
            }
            _ => {}
        };
        match end {
            false => demux.push(data, &mut sink),
            true => demux.flush(&mut sink),
        }
    }

    /// With `restart` the source went back for the track and the demuxer reads on from there.
    fn relink(&mut self, ctx: &Ctx, pad: SrcPad, on: bool, restart: Option<SeekTarget>) {
        let Some(at) =
            self.tracks.iter().position(|track| ctx.stream_pad(track.stream) == Some(pad))
        else {
            return;
        };
        if let Some(target) = restart.filter(|_| on) {
            self.rejoin(at, pad, target);
        }
        let track = &mut self.tracks[at];
        track.pad = on.then_some(pad);
        (track.started, track.run) = (false, None);
        track.need_key = track.kind == StreamKind::Video;
        track.pages_due = on;
        track.carry.clear();
        if let Some(demux) = self.demux.as_mut() {
            demux.select(track.pid, on);
        }
        self.grown.rekey(key_of(&self.tracks));
    }

    /// The track drops what it parked for a full pool, it was read ahead of the restart and
    /// would go out first and again when read. The other tracks keep theirs and drop what they
    /// read again up to their last unit, what was in progress is read again whole.
    fn rejoin(&mut self, at: usize, pad: SrcPad, target: SeekTarget) {
        let old = self.tracks[at].pad;
        let gone = |row: &Parked| match *row {
            Parked::Unit { pad: of, .. } | Parked::Format { pad: of, .. } => {
                of == pad || Some(of) == old
            }
        };
        self.park.rows.retain(|row| !gone(row));
        for (n, track) in self.tracks.iter_mut().enumerate() {
            let on = track.pad.is_some();
            track.resume = track.read.or(track.resume).filter(|_| on && n != at);
            (track.read, track.quiet) = (None, on || n == at);
        }
        self.feeding = None;
        if let Some(demux) = self.demux.as_mut() {
            demux.discontinuity();
            if self.grown.placed != Placed::Segments {
                demux.set_position(target.to_byte);
            }
        }
        // Same time base, so the track read again is not behind itself.
        (self.clock.sought, self.clock.broke) = (true, false);
        self.clock.stretch = self.clock.stretch.wrapping_add(1).max(1);
    }
}

/// The first linked video stream, else the first linked audio stream.
fn key_of(tracks: &[Track]) -> Option<Key> {
    let linked = |kind: StreamKind| tracks.iter().find(|t| t.pad.is_some() && t.kind == kind);
    let track = linked(StreamKind::Video).or_else(|| linked(StreamKind::Audio))?;
    Some(Key { pid: track.pid, stream: track.stream, video: track.kind == StreamKind::Video })
}

/// From the subtitling_descriptor pf-mpegts read, `None` without one, so every page decodes.
fn pages_of(info: &StreamInfo) -> Option<Pages> {
    let (c, a) = (info.composition_page.to_be_bytes(), info.ancillary_page.to_be_bytes());
    let pages = [c[0], c[1], a[0], a[1], info.subtitling_type];
    (info.codec == Codec::DvbSubtitle && pages != [0; 5]).then_some(pages)
}

#[derive(Default)]
struct Found {
    mapped: bool,
    streams: Vec<StreamInfo>,
    seen: Vec<(u16, i64)>,
    clock: Option<Unwrapper>,
    opening: Option<(i64, i64)>,
    program: u16,
    /// MPEG-1 or MPEG-2 video by the first sequence header, under either stream type.
    mpeg12: Vec<(u16, Codec)>,
}

impl Element for TsDemux {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    #[allow(clippy::disallowed_methods)] // once per element
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let config = Config { limits: self.limits, select_all: true };
        let mut probe = Demux::new(config).map_err(|e| Self::failed(ctx, TsError::Config(e)))?;
        let mut found = Found::default();
        let want = self.program;
        let mut sink = |event: pf_mpegts::Event<'_>| match event {
            pf_mpegts::Event::Pmt { program, streams } if !found.mapped => {
                if want.is_none_or(|number| number == program.number) {
                    (found.mapped, found.program) = (true, program.number);
                    found.streams = streams.to_vec();
                }
            }
            pf_mpegts::Event::Pes(unit) => {
                let at = found.streams.iter().position(|s| s.pid == unit.pid);
                let mpeg12 = at
                    .map(|at| found.streams[at].codec)
                    .is_some_and(|codec| matches!(codec, Codec::Mpeg1Video | Codec::Mpeg2Video));
                if mpeg12
                    && !found.mpeg12.iter().any(|(pid, _)| *pid == unit.pid)
                    && let Some(mpeg1) = es::sequence_is_mpeg1(unit.payload)
                {
                    let codec = if mpeg1 { Codec::Mpeg1Video } else { Codec::Mpeg2Video };
                    found.mpeg12.push((unit.pid, codec));
                }
                // One unwrapper, so all streams share a timeline.
                let stamp = unit.dts.or(unit.pts).filter(|_| at.is_some());
                if let Some(raw) =
                    stamp.filter(|_| !found.seen.iter().any(|(pid, _)| *pid == unit.pid))
                {
                    let ticks =
                        found.clock.get_or_insert_with(Unwrapper::for_timestamps).unwrap(raw).ticks;
                    found.seen.push((unit.pid, ticks));
                    let (first, least) = found.opening.unwrap_or((ticks, ticks));
                    found.opening = Some((first, least.min(ticks)));
                }
            }
            _ => {}
        };
        probe.push(&self.head, &mut sink);
        probe.flush(&mut sink);
        if !found.mapped {
            return Err(Self::failed(
                ctx,
                match want {
                    Some(number) => TsError::NoSuchProgram { number },
                    None => TsError::NoProgram,
                },
            ));
        }
        self.head = Vec::new();
        self.opening = found.opening;
        self.tracks.clear();
        for info in &found.streams {
            let (pid, stream_type) = (info.pid, info.stream_type);
            if info.encryption != Encryption::None {
                log!(&*ctx, Level::Warn, "stream_encrypted", pid = pid, stream_type = stream_type);
                continue;
            }
            let told = found.mpeg12.iter().find(|(pid, _)| *pid == info.pid).map(|(_, c)| *c);
            let codec = told.unwrap_or(info.codec);
            let (family, kind) = family_of(codec);
            if info.codec == Codec::Unknown {
                log!(&*ctx, Level::Warn, "stream_unknown", pid = pid, stream_type = stream_type);
            } else {
                log!(
                    &*ctx,
                    Level::Info,
                    "stream",
                    pid = pid,
                    family = family,
                    stream_type = stream_type
                );
            }
            let language = pf_mpegts::language_str(info).unwrap_or("");
            // H.222.0 Table 2-61 audio_type, EN 300 468 Table 26 hard of hearing subtitling_type.
            let role = Disposition {
                hearing_impaired: info.audio_type == 2
                    || (0x20..=0x25).contains(&info.subtitling_type),
                visual_impaired: info.audio_type == 3,
                ..Disposition::MAIN
            };
            let stream = ctx.add_stream(Stream {
                kind,
                codec: family,
                language: language.into(),
                title: "".into(),
                default: true,
                forced: false,
                disposition: role,
                channels: 0,
                duration: Timestamp::NONE,
                sparse: kind.sparse(),
                name: format!("src_{}", info.pid).into(),
                offers: &SRC_OFFERS,
            });
            self.tracks.push(Track {
                pid: info.pid,
                codec,
                family,
                kind,
                stream,
                pad: None,
                started: false,
                need_key: kind == StreamKind::Video,
                adts: None,
                carry: Vec::with_capacity(es::ADTS_FRAME_MAX),
                run: None,
                last_dts: (0, 0),
                lead_ns: {
                    let first = found
                        .seen
                        .iter()
                        .find(|(pid, _)| *pid == info.pid)
                        .map(|(_, ticks)| *ticks);
                    let least = found.opening.map(|(_, least)| least);
                    first
                        .zip(least)
                        .map_or(0, |(first, least)| ticks_ns(first - least).max(0) as u64)
                },
                dovi: (kind == StreamKind::Video)
                    .then(|| {
                        pf_hdr::dovi::DoviConfig::from_ts_descriptor(
                            &info.dovi[..usize::from(info.dovi_len)],
                        )
                        .ok()
                    })
                    .flatten()
                    .map(pf_hdr::dovi::DoviConfig::pack),
                pages: pages_of(info),
                pages_due: true,
                scrambled: 0,
                adts_skipped: 0,
                read: None,
                resume: None,
                quiet: false,
            });
            if info.dovi_len > 0 && self.tracks.last().is_some_and(|t| t.dovi.is_none()) {
                log!(&*ctx, Level::Warn, "dovi_config_bad", pid = pid, bytes = info.dovi_len);
            }
        }
        log!(
            &*ctx,
            Level::Info,
            "opened",
            program = found.program,
            streams = found.streams.len(),
            tracks = self.tracks.len(),
            timed = found.opening.is_some(),
        );
        match self.tracks.is_empty() && !found.streams.is_empty() {
            true => Err(Self::failed(ctx, TsError::Encrypted)),
            false => Ok(()),
        }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let config = Config { limits: self.limits, select_all: false };
        let mut demux = Demux::new(config).map_err(|e| Self::failed(ctx, TsError::Config(e)))?;
        for track in &mut self.tracks {
            track.pad = ctx.stream_pad(track.stream).filter(|pad| ctx.pad_linked(*pad));
            demux.select(track.pid, track.pad.is_some());
        }
        self.demux = Some(demux);
        self.grown.placed = Placed::File;
        self.grown.rekey(key_of(&self.tracks));
        self.clock.unwrap.reset();
        (self.clock.shift, self.clock.anchor, self.clock.origin) = (0, Some(0), None);
        if let Some((first, least)) = self.opening {
            self.clock.unwrap.prime(first);
            (self.clock.shift, self.clock.anchor, self.clock.origin) =
                (ticks_ns(least), None, Some(least));
        }
        (self.clock.sought, self.clock.last_ns, self.clock.broke, self.clock.marked) =
            (false, 0, false, false);
        (self.feeding, self.ending, self.anomalies) = (None, false, 0);
        self.park.rows.clear();
        self.park.bytes.clear();
        self.break_stream(None);
        if let Some(ns) = self.grown.duration {
            ctx.set_duration(ns);
            Self::publish(ctx, &mut self.grown);
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.take_in(ctx, &mut inputs)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        self.on_event(ctx, event)
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        (self.demux, self.feeding) = (None, None);
        self.park.rows.clear();
        self.park.bytes.clear();
    }
}

impl TsDemux {
    /// The run's counts, and each drop kept quiet past its first.
    fn log_end(&self, ctx: &Ctx) {
        if let Some(st) = self.demux.as_ref().map(Demux::stats) {
            let (packets, units) = (st.packets, st.pes_units);
            let anomalies = pf_mpegts::anomaly_total(st);
            let scrambled = st.scrambled_packets;
            log!(
                ctx,
                Level::Info,
                "eos",
                packets = packets,
                units = units,
                anomalies = anomalies,
                scrambled = scrambled,
            );
        }
        for t in self.tracks.iter().filter(|t| t.scrambled > 0 || t.adts_skipped > 0) {
            let (pid, scrambled, skipped) = (t.pid, t.scrambled, t.adts_skipped);
            log!(
                ctx,
                Level::Warn,
                "dropped",
                pid = pid,
                scrambled = scrambled,
                adts_bytes = skipped
            );
        }
    }

    fn take_in(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        loop {
            // Fed only while output leaves, so a slow consumer backs up into the queue ahead.
            if !self.unpark(ctx) {
                ctx.hold_end();
                return Ok(Flow::Ok);
            }
            let (memory, at) = match self.feeding.take() {
                Some(feeding) => feeding,
                None => {
                    let Some(buf) = inputs.pop() else { return Ok(Flow::Ok) };
                    if buf.flags.contains(BufferFlags::DISCONT) {
                        let segment = buf.flags.contains(BufferFlags::SEGMENT);
                        log!(&*ctx, Level::Debug, "discont", segment = segment);
                        self.break_stream(segment.then_some(&mut *ctx));
                    }
                    if buf.flags.contains(BufferFlags::SEGMENT) {
                        log!(&*ctx, Level::Debug, "segment", pts = buf.pts);
                        self.grown.placed = Placed::Segments;
                        if let Some(stream) = buf.pts.nanos() {
                            self.clock.unwrap.reset();
                            (self.clock.anchor, self.clock.sought, self.clock.broke) =
                                (Some(stream), false, false);
                            self.clock.marked = true;
                        }
                    }
                    (buf.memory, 0)
                }
            };
            let end = (at + PIECE).min(memory.len());
            self.feed(ctx, &memory.data()[at..end], false);
            if end < memory.len() {
                self.feeding = Some((memory, end));
            }
        }
    }

    fn on_event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => {
                self.ending = true;
                if let Some((memory, at)) = self.feeding.take() {
                    self.feed(ctx, &memory.data()[at..], false);
                }
                self.feed(ctx, &[], true);
                self.unpark(ctx);
                if self.grown.unpublished() {
                    Self::publish(ctx, &mut self.grown);
                }
                self.log_end(ctx);
            }
            Event::FlushStart => {
                self.measured.clear();
                self.teller.restart();
                if self.grown.unpublished() {
                    Self::publish(ctx, &mut self.grown);
                }
                self.break_stream(None);
                if self.grown.placed != Placed::Segments {
                    let target = ctx.seek_target();
                    let at = target.map_or(0, |t| t.to_byte);
                    log!(&*ctx, Level::Debug, "flush", seek = target.is_some(), at = at);
                    self.grown.placed = if target.is_some() { Placed::File } else { Placed::Lost };
                    if let (Some(demux), Some(target)) = (self.demux.as_mut(), target) {
                        demux.set_position(target.to_byte);
                    }
                }
                (self.feeding, self.ending) = (None, false);
                self.park.rows.clear();
                self.park.bytes.clear();
                // Same time base, so no stream is behind itself.
                (self.clock.sought, self.clock.broke) = (true, false);
                self.clock.stretch = self.clock.stretch.wrapping_add(1).max(1);
            }
            Event::SrcLinked { pad, on, restart } => self.relink(ctx, *pad, *on, *restart),
            _ => {}
        }
        Ok(())
    }
}

/// What the demuxer and an opener of the same file read the head with, so both find the same
/// program and streams.
pub fn limits() -> Limits {
    Limits { max_programs: 16, max_streams_per_program: 32, max_pes_pids: 16, ..Limits::default() }
}

/// Each ADTS stream's AudioSpecificConfig in `head` by pad name `src_<pid>`, from its first frame.
#[allow(clippy::disallowed_methods)] // once per lane
pub fn aac_configs(head: &[u8]) -> Vec<(Box<str>, [u8; 2])> {
    let Ok(mut probe) = Demux::new(Config { limits: limits(), select_all: true }) else {
        return Vec::new();
    };
    let (mut aac, mut found): (Vec<u16>, Vec<(u16, [u8; 2])>) = (Vec::new(), Vec::new());
    let mut sink = |event: pf_mpegts::Event<'_>| match event {
        pf_mpegts::Event::Pmt { streams, .. } if aac.is_empty() => {
            aac = streams.iter().filter(|s| s.codec == Codec::AacAdts).map(|s| s.pid).collect();
        }
        pf_mpegts::Event::Pes(unit)
            if aac.contains(&unit.pid) && !found.iter().any(|(pid, _)| *pid == unit.pid) =>
        {
            if let Some(h) = Adts::parse(unit.payload) {
                found.push((unit.pid, h.config()));
            }
        }
        _ => {}
    };
    probe.push(head, &mut sink);
    probe.flush(&mut sink);
    found.into_iter().map(|(pid, config)| (format!("src_{pid}").into(), config)).collect()
}

const HEADS_KEPT: usize = 8;

/// Configs a demuxer put out with the stream time they apply from, seeded by [`aac_configs`],
/// so a decoder joining behind a branch gets the right one ([`AacHeads::at`]).
#[derive(Debug, Default)]
pub struct AacHeads {
    said: std::sync::Mutex<Vec<Said>>,
}

/// `None` stream time for a seed or an unstamped config.
type Said = (u16, Option<u64>, [u8; 2]);

impl AacHeads {
    #[allow(clippy::disallowed_methods)] // once per lane
    pub fn of(head: &[u8]) -> Arc<AacHeads> {
        let seeds = aac_configs(head)
            .into_iter()
            .filter_map(|(pad, config)| Some((AacHeads::pid(&pad)?, None, config)));
        Arc::new(AacHeads { said: std::sync::Mutex::new(seeds.collect()) })
    }

    pub fn pid(pad: &str) -> Option<u16> {
        pad.strip_prefix("src_")?.parse().ok()
    }

    fn put(&self, pid: u16, from: Option<u64>, config: [u8; 2]) {
        let mut said = self.said.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if said.iter().filter(|e| e.0 == pid).count() >= HEADS_KEPT
            && let Some(at) = said.iter().position(|e| e.0 == pid)
        {
            said.remove(at);
        }
        said.push((pid, from, config));
    }

    /// The last put applying at or before `pts`, the last of all when unstamped.
    pub fn at(&self, pid: u16, pts: Option<u64>) -> Option<[u8; 2]> {
        let said = self.said.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let applies = |from: Option<u64>| match (from, pts) {
            (Some(from), Some(pts)) => from <= pts,
            _ => true,
        };
        said.iter().rev().find(|e| e.0 == pid && applies(e.1)).map(|e| e.2)
    }
}

pub static ELEMENTS: Elements = &[&DESC];

pub fn register(registry: &mut Registry) {
    registry.register_all(ELEMENTS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_is_every_src_offer_family() {
        for o in &SRC_OFFERS {
            assert!(o.family == "bytes" || EMITS.contains(&o.family), "{}", o.family);
        }
        for (i, f) in EMITS.iter().enumerate() {
            assert!(SRC_OFFERS.iter().any(|o| o.family == *f), "{f} not offered");
            assert!(!EMITS[i + 1..].contains(f), "{f} twice");
        }
    }

    #[test]
    fn the_config_of_a_unit_is_the_last_put_from_at_or_before_its_stamp() {
        let (stereo, mono, other) = ([0x11, 0x90], [0x11, 0x88], [0x12, 0x10]);
        let heads = AacHeads::default();
        assert_eq!(heads.at(256, Some(0)), None, "nothing put");
        heads.put(256, None, stereo);
        heads.put(257, Some(0), other);
        heads.put(256, Some(3_000), mono);
        assert_eq!(heads.at(256, Some(2_999)), Some(stereo), "before the change");
        assert_eq!(heads.at(256, Some(3_000)), Some(mono), "from it");
        assert_eq!(heads.at(256, None), Some(mono), "the last, unstamped");
        assert_eq!(heads.at(257, Some(5_000)), Some(other), "a pid of its own");
        heads.put(256, Some(1_000), stereo);
        assert_eq!(heads.at(256, Some(2_000)), Some(stereo));
        assert_eq!(heads.at(256, Some(500)), Some(stereo), "the seed, before any stamp");
        for k in 0..20 {
            heads.put(256, Some(10_000 + k), if k % 2 == 0 { mono } else { stereo });
        }
        let kept = heads.said.lock().expect("heads").iter().filter(|e| e.0 == 256).count();
        assert_eq!(kept, HEADS_KEPT);
        assert_eq!(heads.at(256, Some(10_019)), Some(stereo));
        assert_eq!(heads.at(256, Some(10_018)), Some(mono));
        assert_eq!(heads.at(257, Some(0)), Some(other), "another pid's left alone");
        assert_eq!(AacHeads::pid("src_256"), Some(256));
        assert_eq!(AacHeads::pid("src_track2"), None);
    }
}
