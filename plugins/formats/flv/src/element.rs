//! `flvdemux`, the element over the [`crate::tag`] reader, a stream for the video and one for the
//! audio. H.264 and HEVC leave in Annex B behind their parameter sets, AAC raw behind its config.

use std::collections::VecDeque;
use std::fmt;

use pf_mkv::Reframer;
use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, Disposition, SeekTarget, Stream, StreamKind};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;
use profluens_core::pipeline::{Index, IndexEntry};
use profluens_core::rate;
use profluens_core::time::Timestamp;

use crate::amf;
use crate::codec::{self, Audio, Packet, Video};
use crate::index::{self, Grown};
use crate::tag::{self, ENHANCED, Expect, Reader, Tag};

/// `Refused` names a codec nothing here decodes as the spec names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlvError {
    NotFlv,
    NoStreams,
    Refused { codec: &'static str },
}

impl fmt::Display for FlvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlvError::NotFlv => f.write_str("flvdemux: the head does not start with an FLV header"),
            FlvError::NoStreams => f.write_str("flvdemux: no audio or video stream in the head"),
            FlvError::Refused { codec } => {
                write!(f, "flvdemux: {codec} is not supported, its stream is refused")
            }
        }
    }
}

impl std::error::Error for FlvError {}

static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];

pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "flvdemux",
    sinks: &PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Made from the first bytes of its file, so not by name.
    make_default: None,
    io: false,
    couplings: &[],
};

/// A head to read, the header, `onMetaData` with some 18 octets a keyframe, the first tags.
pub const HEAD: usize = 1 << 20;

/// Input is fed in pieces of this, so what parks behind a dry pool stays small.
const PIECE: usize = 64 << 10;

/// The largest configuration record kept, larger ones are ignored.
const MAX_RECORD: usize = 64 << 10;

const NAL4: Reframer = Reframer::Nal { length_size: 4 };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    Video(Video),
    Audio(Audio),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Form {
    rate: u32,
    channels: u32,
    sample: Option<&'static str>,
    width: u32,
    height: u32,
}

struct Track {
    codec: Codec,
    family: &'static str,
    kind: StreamKind,
    stream: u32,
    pad: Option<SrcPad>,
    started: bool,
    need_key: bool,
    /// The configuration record as read, avcC, hvcC, av1C, vpcC or AudioSpecificConfig.
    record: Vec<u8>,
    head: Vec<u8>,
    nal: Option<Reframer>,
    form: Form,
    /// The byte of the last tag taken in, and behind a restart for another track the one tags
    /// read again are dropped up to.
    read: Option<u64>,
    resume: Option<u64>,
}

impl Track {
    /// Read again behind a restart for another track, it went out the first time.
    fn again(&mut self, pos: u64) -> bool {
        if self.resume.is_some_and(|last| pos <= last) {
            return true;
        }
        (self.resume, self.read) = (None, Some(pos));
        false
    }

    // Once per configuration record, rare in a stream.
    #[allow(clippy::disallowed_methods)]
    fn set_record(&mut self, ctx: &Ctx, record: &[u8]) -> bool {
        if record == self.record || record.len() > MAX_RECORD || record.is_empty() {
            if record.len() > MAX_RECORD {
                log!(
                    ctx,
                    Level::Warn,
                    "record_too_large",
                    family = self.family,
                    bytes = record.len()
                );
            }
            return false;
        }
        self.record.clear();
        self.record.extend_from_slice(record);
        match self.codec {
            Codec::Video(v) if v.nal() => {
                (self.head, self.nal) = match pf_mkv::nal_head_from_config(record, v == Video::Hevc)
                {
                    Ok((head, length)) => (head, Some(Reframer::Nal { length_size: length })),
                    // In band parameter sets may still come, so frames go on as 4 octet lengths.
                    Err(_) => {
                        log!(
                            ctx,
                            Level::Warn,
                            "record_bad",
                            family = self.family,
                            bytes = record.len()
                        );
                        (Vec::new(), Some(NAL4))
                    }
                };
            }
            Codec::Audio(Audio::Aac) => {
                self.head.clear();
                self.head.extend_from_slice(record);
                if let Some((rate, channels)) = codec::aac_form(record) {
                    (self.form.rate, self.form.channels) = (rate, channels);
                }
            }
            _ => {}
        }
        true
    }
}

#[derive(Clone, Copy)]
enum Parked {
    Unit { pad: SrcPad, pts: Option<u64>, flags: BufferFlags, from: usize, to: usize },
    Format { pad: SrcPad, family: &'static str, form: Form, video: bool },
}

#[derive(Default)]
struct Park {
    bytes: Vec<u8>,
    rows: VecDeque<Parked>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Conv {
    Copy,
    Nal4,
}

/// The part of the element a tag goes through, apart from the reader that reads it.
struct Live {
    tracks: Vec<Track>,
    park: Park,
    exact: bool,
    origin_ms: u32,
    file_len: Option<u64>,
    duration: Timestamp,
    keyframes: Option<Index>,
    grown: Grown,
    key: Option<usize>,
    reframed: Vec<u8>,
    dropped: u64,
    /// Tags whose body does not parse, and tags of a codec no track carries.
    bad: u64,
    strays: u64,
    measured: rate::ByteRate,
    teller: rate::Teller,
}

pub struct FlvDemux {
    head: Vec<u8>,
    reader: Reader,
    live: Live,
    feeding: Option<(Memory, usize)>,
    posted_duration: bool,
    /// The reader's resync count last logged.
    resyncs: u64,
}

impl FlvDemux {
    pub const IN: SinkPad = SinkPad(0);

    /// `head` is the file's first bytes, [`HEAD`] of them or the whole of a smaller file.
    // Once per element.
    #[allow(clippy::disallowed_methods)]
    pub fn new(head: Vec<u8>) -> Self {
        Self {
            head,
            reader: Reader::new(),
            live: Live {
                tracks: Vec::new(),
                park: Park::default(),
                exact: false,
                origin_ms: 0,
                file_len: None,
                duration: Timestamp::NONE,
                keyframes: None,
                grown: Grown::new(),
                key: None,
                reframed: Vec::new(),
                dropped: 0,
                bad: 0,
                strays: 0,
                measured: rate::ByteRate::default(),
                teller: rate::Teller::default(),
            },
            feeding: None,
            posted_duration: false,
            resyncs: 0,
        }
    }

    #[must_use]
    pub fn with_len(mut self, len: u64) -> Self {
        self.live.file_len = Some(len);
        self
    }

    pub fn keyframes(&self) -> Option<&Index> {
        self.live.keyframes.as_ref()
    }

    pub fn stats(&self) -> tag::Stats {
        self.reader.stats
    }

    fn failed(ctx: &Ctx, error: FlvError) -> Error {
        Error::element_failed(ctx.element(), error)
    }
}

/// What preroll reads of the head.
#[derive(Default)]
struct Found {
    meta: Option<amf::Meta>,
    meta_bad: bool,
    entries: Vec<IndexEntry>,
    /// The codec and the first body octet of the first tag of each kind.
    video: Option<(Video, u8)>,
    audio: Option<(Audio, u8)>,
    video_record: Option<(Video, Vec<u8>)>,
    audio_record: Option<Vec<u8>>,
    mp3: Option<(u32, u32)>,
    origin: Option<u32>,
}

impl Found {
    // Once at preroll.
    #[allow(clippy::disallowed_methods)]
    fn read(head: &[u8], file_len: Option<u64>) -> Found {
        let mut found = Found::default();
        let mut reader = Reader::new();
        reader.push(head, &mut |tag: Tag<'_>| found.tag(&tag, file_len));
        found
    }

    #[allow(clippy::disallowed_methods)]
    fn tag(&mut self, tag: &Tag<'_>, file_len: Option<u64>) {
        let ts = tag.header.ts_ms;
        match tag.header.kind {
            tag::SCRIPT if self.meta.is_none() => match amf::on_meta_data(tag.body) {
                Ok(meta) => {
                    self.entries = index::from_meta(tag.body, &meta, 0, file_len);
                    self.meta = Some(meta);
                }
                Err(_) => self.meta_bad = true,
            },
            tag::VIDEO => {
                let Some(v) = codec::video_tag(tag.body) else { return };
                if v.packet == Packet::Skip || v.codec == Video::Unknown {
                    return;
                }
                self.origin = Some(self.origin.map_or(ts, |o| o.min(ts)));
                let first = if v.enhanced { ENHANCED } else { tag.body[0] & 0x0F };
                let (codec, _) = *self.video.get_or_insert((v.codec, first));
                if v.packet == Packet::Config && codec == v.codec && self.video_record.is_none() {
                    let record = tag.body.get(v.from..v.to).unwrap_or_default();
                    self.video_record = Some((codec, record.to_vec()));
                }
            }
            tag::AUDIO => {
                let Some(a) = codec::audio_tag(tag.body) else { return };
                self.origin = Some(self.origin.map_or(ts, |o| o.min(ts)));
                let (codec, _) = *self.audio.get_or_insert((a.codec, tag.body[0]));
                let payload = tag.body.get(a.from..).unwrap_or_default();
                match a.packet {
                    Packet::Config if codec == Audio::Aac && self.audio_record.is_none() => {
                        self.audio_record = Some(payload.to_vec());
                    }
                    Packet::Frame if self.mp3.is_none() && codec.family() == Some("mp3") => {
                        self.mp3 = codec::mp3_form(payload);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn stamp(origin_ms: u32, ts_ms: u32, cts: i32, nanos: u32) -> u64 {
    let ms = i64::from(ts_ms) + i64::from(cts) - i64::from(origin_ms);
    (ms.max(0) as u64) * 1_000_000 + u64::from(nanos.min(999_999))
}

/// The first linked video stream, else the first linked audio one.
fn key_of(tracks: &[Track]) -> Option<usize> {
    let linked = |kind| tracks.iter().position(|t| t.pad.is_some() && t.kind == kind);
    linked(StreamKind::Video).or_else(|| linked(StreamKind::Audio))
}

impl Live {
    /// With something parked already everything parks behind it, so the order holds.
    #[allow(clippy::too_many_arguments)]
    fn put(
        ctx: &mut Ctx,
        park: &mut Park,
        exact: bool,
        pad: SrcPad,
        pts: Option<u64>,
        flags: BufferFlags,
        bytes: &[u8],
        conv: Conv,
    ) {
        if bytes.is_empty() {
            return;
        }
        if park.rows.is_empty() && Self::write(ctx, exact, pad, pts, flags, bytes, conv) {
            return;
        }
        let from = park.bytes.len();
        park.bytes.extend_from_slice(bytes);
        if conv == Conv::Nal4 {
            NAL4.reframe_in_place(&mut park.bytes[from..]);
        }
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
        conv: Conv,
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
                    let error = Error::element_failed(element, e);
                    ctx.post(BusMessage::Warning { element, error });
                    return true;
                }
            },
        };
        let out = &mut buf.memory.as_mut_full()[..bytes.len()];
        out.copy_from_slice(bytes);
        if conv == Conv::Nal4 {
            NAL4.reframe_in_place(out);
        }
        buf.memory.set_len(bytes.len());
        buf.pts = pts.map_or(Timestamp::NONE, Timestamp);
        buf.flags = flags;
        ctx.out(pad).push(buf);
        true
    }

    fn say(ctx: &mut Ctx, pad: SrcPad, family: &'static str, form: Form, video: bool) {
        let int = |v: u32| ValueDesc::Int(i64::from(v));
        let (rate, channels) = (("rate", int(form.rate)), ("channels", int(form.channels)));
        match (video, form.sample) {
            (true, _) if form.width > 0 && form.height > 0 => ctx.announce_format(
                pad,
                family,
                &[("width", int(form.width)), ("height", int(form.height))],
            ),
            (true, _) => ctx.announce_format(pad, family, &[]),
            (false, Some(s)) => {
                ctx.announce_format(pad, family, &[rate, channels, ("sample", ValueDesc::Id(s))])
            }
            (false, None) if form.rate > 0 && form.channels > 0 => {
                ctx.announce_format(pad, family, &[rate, channels])
            }
            (false, None) if form.rate > 0 => ctx.announce_format(pad, family, &[rate]),
            (false, None) => ctx.announce_format(pad, family, &[]),
        }
    }

    fn announce(ctx: &mut Ctx, park: &mut Park, t: &Track, pad: SrcPad) {
        let video = t.kind == StreamKind::Video;
        match park.rows.is_empty() {
            true => Self::say(ctx, pad, t.family, t.form, video),
            false => {
                park.rows.push_back(Parked::Format { pad, family: t.family, form: t.form, video })
            }
        }
    }

    /// Put out what is parked, in order. False while the pool is still dry.
    fn unpark(&mut self, ctx: &mut Ctx) -> bool {
        while let Some(row) = self.park.rows.front().copied() {
            match row {
                Parked::Unit { pad, pts, flags, from, to } => {
                    let bytes = &self.park.bytes[from..to];
                    if !Self::write(ctx, self.exact, pad, pts, flags, bytes, Conv::Copy) {
                        return false;
                    }
                }
                Parked::Format { pad, family, form, video } => {
                    Self::say(ctx, pad, family, form, video)
                }
            }
            self.park.rows.pop_front();
        }
        self.park.bytes.clear();
        true
    }

    // One copy per publication.
    #[allow(clippy::disallowed_methods)]
    fn publish(&mut self, ctx: &Ctx) {
        let stream = self.key.map(|k| self.tracks[k].stream);
        let grouped = self.tracks.iter().any(|t| t.kind == StreamKind::Video);
        let index = match &self.keyframes {
            Some(index) => index.clone(),
            None => {
                self.grown.published = self.grown.entries.len();
                let entries = self.grown.entries.clone();
                let (file_len, duration) = (self.file_len, self.duration);
                Index { entries, file_len, duration, stream, grouped, ..Index::default() }
            }
        };
        ctx.publish_index(index);
    }

    /// The key track's units grow the index and measure the rate.
    fn keyed(&mut self, ctx: &mut Ctx, n: usize, key: bool, pts: u64, pos: u64) {
        if self.key != Some(n) {
            return;
        }
        if key && self.keyframes.is_none() && self.grown.keyframe(pts, pos) {
            self.publish(ctx);
        }
        self.measured.note(pos, Timestamp(pts));
        let (index, measured) = (&self.keyframes, &self.measured);
        self.teller.tell_at(ctx, FlvDemux::IN, Timestamp(pts), |at| match index {
            Some(index) => rate::of_index(index, at),
            None => measured.rate(),
        });
    }

    fn on_tag(&mut self, ctx: &mut Ctx, tag: Tag<'_>) {
        if tag.header.filtered {
            return;
        }
        match tag.header.kind {
            tag::VIDEO => self.on_video(ctx, &tag),
            tag::AUDIO => self.on_audio(ctx, &tag),
            _ => {}
        }
    }

    fn start_track(&mut self, ctx: &mut Ctx, n: usize, pad: SrcPad, pts: u64) {
        let Live { tracks, park, exact, .. } = self;
        let t = &mut tracks[n];
        if t.started {
            return;
        }
        t.started = true;
        Self::announce(ctx, park, t, pad);
        let header = BufferFlags::HEADER;
        Self::put(ctx, park, *exact, pad, Some(pts), header, &t.head, Conv::Copy);
    }

    /// The first of each kind at warn, the rest counted for the end.
    fn bad_tag(&mut self, ctx: &Ctx, kind: &'static str, pos: u64) {
        let level = if self.bad == 0 { Level::Warn } else { Level::Trace };
        log!(ctx, level, "tag_malformed", kind = kind, pos = pos);
        self.bad += 1;
    }

    fn stray(&mut self, ctx: &Ctx, codec: &'static str, pos: u64) {
        let level = if self.strays == 0 { Level::Warn } else { Level::Trace };
        log!(ctx, level, "codec_untracked", codec = codec, pos = pos);
        self.strays += 1;
    }

    fn on_video(&mut self, ctx: &mut Ctx, tag: &Tag<'_>) {
        let Some(v) = codec::video_tag(tag.body) else {
            return self.bad_tag(ctx, "video", tag.pos);
        };
        let Some(n) = self.tracks.iter().position(|t| t.codec == Codec::Video(v.codec)) else {
            if v.packet != Packet::Skip {
                self.stray(ctx, v.codec.name(), tag.pos);
            }
            return;
        };
        let payload = tag.body.get(v.from..v.to).unwrap_or_default();
        let t = &mut self.tracks[n];
        if t.again(tag.pos) {
            return;
        }
        match v.packet {
            Packet::Config => {
                if t.set_record(ctx, payload) {
                    t.started = false;
                }
                return;
            }
            Packet::Frame => {}
            Packet::End | Packet::Skip => return,
        }
        let Some(pad) = t.pad else { return };
        let key = v.key();
        if t.need_key && !key {
            return;
        }
        t.need_key = false;
        let pts = stamp(self.origin_ms, tag.header.ts_ms, v.cts, v.nanos);
        self.keyed(ctx, n, key, pts, tag.pos);
        self.start_track(ctx, n, pad, pts);
        let Live { tracks, park, exact, reframed, dropped, .. } = self;
        let (bytes, conv) = match tracks[n].nal {
            None => (payload, Conv::Copy),
            Some(Reframer::Nal { length_size: 4 }) if codec::nal4_whole(payload) => {
                (payload, Conv::Nal4)
            }
            Some(r) => match r.reframe_into(payload, reframed) {
                Ok(bytes) => (bytes, Conv::Copy),
                Err(_) => {
                    let level = if *dropped == 0 { Level::Warn } else { Level::Trace };
                    log!(&*ctx, level, "frame_dropped", pos = tag.pos, bytes = payload.len());
                    *dropped += 1;
                    return;
                }
            },
        };
        let flags = if key { BufferFlags::KEYFRAME } else { BufferFlags::DELTA };
        Self::put(ctx, park, *exact, pad, Some(pts), flags, bytes, conv);
    }

    fn on_audio(&mut self, ctx: &mut Ctx, tag: &Tag<'_>) {
        let Some(a) = codec::audio_tag(tag.body) else {
            return self.bad_tag(ctx, "audio", tag.pos);
        };
        let Some(n) = a.codec.family().and_then(|f| self.tracks.iter().position(|t| t.family == f))
        else {
            if a.packet != Packet::Skip {
                self.stray(ctx, a.codec.name(), tag.pos);
            }
            return;
        };
        let payload = tag.body.get(a.from..).unwrap_or_default();
        let t = &mut self.tracks[n];
        if t.again(tag.pos) {
            return;
        }
        match a.packet {
            Packet::Config => {
                if t.set_record(ctx, payload) {
                    t.started = false;
                }
                return;
            }
            Packet::Frame if !payload.is_empty() => {}
            _ => return,
        }
        let Some(pad) = t.pad else { return };
        // Nothing decodes AAC before its config.
        if a.codec == Audio::Aac && t.head.is_empty() {
            log!(&*ctx, Level::Trace, "aac_before_config", pos = tag.pos);
            return;
        }
        let form = match a.codec {
            Audio::Pcm | Audio::PcmLe => {
                let (rate, channels, sample) = codec::pcm_form(a.flags);
                Form { rate, channels, sample: Some(sample), ..t.form }
            }
            Audio::Mp3 | Audio::Mp3At8k => match codec::mp3_form(payload) {
                Some((rate, channels)) => Form { rate, channels, ..t.form },
                None => t.form,
            },
            _ => t.form,
        };
        if form != t.form {
            (t.form, t.started) = (form, false);
        }
        let pts = stamp(self.origin_ms, tag.header.ts_ms, 0, 0);
        self.keyed(ctx, n, true, pts, tag.pos);
        self.start_track(ctx, n, pad, pts);
        let flags = BufferFlags::KEYFRAME;
        Self::put(ctx, &mut self.park, self.exact, pad, Some(pts), flags, payload, Conv::Copy);
    }

    fn reset_tracks(&mut self) {
        for t in &mut self.tracks {
            t.started = false;
            t.need_key = t.kind == StreamKind::Video;
            (t.read, t.resume) = (None, None);
        }
    }

    fn rekey(&mut self) {
        let key = key_of(&self.tracks);
        if key != self.key {
            self.key = key;
            self.grown.entries.clear();
            self.grown.published = 0;
            self.grown.audio = key.is_some_and(|k| self.tracks[k].kind == StreamKind::Audio);
        }
    }
}

impl FlvDemux {
    fn feed(&mut self, ctx: &mut Ctx, data: &[u8], end: bool) {
        let Self { reader, live, .. } = self;
        let mut on = |tag: Tag<'_>| live.on_tag(ctx, tag);
        match end {
            false => reader.push(data, &mut on),
            true => reader.finish(&mut on),
        }
        let s = reader.stats;
        if s.resyncs > self.resyncs {
            let level = if self.resyncs == 0 { Level::Warn } else { Level::Trace };
            log!(&*ctx, level, "resync", count = s.resyncs, skipped = s.skipped);
            self.resyncs = s.resyncs;
        }
    }

    /// A refused codec is warned of once, the first is the error if nothing else plays.
    #[allow(clippy::disallowed_methods)] // once per stream at preroll
    fn add_track(
        &mut self,
        ctx: &mut Ctx,
        codec: Codec,
        form: Form,
        refused: &mut Option<&'static str>,
    ) {
        let (family, name, kind) = match codec {
            Codec::Video(v) => (v.family(), v.name(), StreamKind::Video),
            Codec::Audio(a) => (a.family(), a.name(), StreamKind::Audio),
        };
        let Some(family) = family else {
            log!(&*ctx, Level::Warn, "stream_refused", codec = name);
            let element = ctx.element();
            let error = Error::element_failed(element, FlvError::Refused { codec: name });
            ctx.post(BusMessage::Warning { element, error });
            refused.get_or_insert(name);
            return;
        };
        let video = kind == StreamKind::Video;
        match video {
            true => log!(
                &*ctx,
                Level::Info,
                "stream",
                family = family,
                width = form.width,
                height = form.height,
            ),
            false => log!(
                &*ctx,
                Level::Info,
                "stream",
                family = family,
                rate = form.rate,
                channels = form.channels,
            ),
        }
        let stream = ctx.add_stream(Stream {
            kind,
            codec: family,
            language: "".into(),
            title: "".into(),
            default: true,
            forced: false,
            disposition: Disposition::MAIN,
            channels: form.channels.min(u32::from(u16::MAX)) as u16,
            duration: self.live.duration,
            sparse: false,
            name: if video { "src_video" } else { "src_audio" }.into(),
            offers: codec::offers(family),
        });
        self.live.tracks.push(Track {
            codec,
            family,
            kind,
            stream,
            pad: None,
            started: false,
            need_key: video,
            record: Vec::new(),
            head: Vec::new(),
            nal: matches!(codec, Codec::Video(v) if v.nal()).then_some(NAL4),
            form,
            read: None,
            resume: None,
        });
    }
}

impl Element for FlvDemux {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    // Once per element.
    #[allow(clippy::disallowed_methods)]
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let head = std::mem::take(&mut self.head);
        let Some(file) = tag::file_header(&head) else {
            return Err(Self::failed(ctx, FlvError::NotFlv));
        };
        let found = Found::read(&head, self.live.file_len);
        let meta = found.meta.unwrap_or_default();
        let live = &mut self.live;
        (live.origin_ms, live.keyframes) = (found.origin.unwrap_or(0), None);
        live.duration = meta
            .duration
            .filter(|d| d.is_finite() && *d > 0.0 && *d < 4.3e6)
            .map_or(Timestamp::NONE, |d| Timestamp((d * 1e9) as u64));
        live.tracks.clear();
        let pixels =
            |v: Option<f64>| v.filter(|v| (1.0..65536.0).contains(v)).map_or(0, |v| v as u32);
        let (width, height) = (pixels(meta.width), pixels(meta.height));
        let video = found.video.map(|v| v.0).or_else(|| meta.video_codec.map(Video::from_meta));
        let audio = found.audio.map(|a| a.0).or_else(|| meta.audio_codec.map(Audio::from_meta));
        let mut refused = None;
        if found.meta_bad {
            log!(&*ctx, Level::Warn, "metadata_bad");
        }
        if video == Some(Video::Unknown) {
            log!(&*ctx, Level::Warn, "video_unknown");
        }
        if audio == Some(Audio::Reserved) {
            log!(&*ctx, Level::Warn, "audio_unknown");
        }
        if let Some(v) = video.filter(|v| *v != Video::Unknown) {
            let form = Form { width, height, ..Form::default() };
            self.add_track(ctx, Codec::Video(v), form, &mut refused);
        }
        if let Some(a) = audio.filter(|a| *a != Audio::Reserved) {
            let flags = found.audio.map_or(0, |a| a.1);
            let form = match a {
                Audio::Pcm | Audio::PcmLe => {
                    let (rate, channels, sample) = codec::pcm_form(flags);
                    Form { rate, channels, sample: Some(sample), ..Form::default() }
                }
                Audio::Mp3 | Audio::Mp3At8k => {
                    let (rate, channels) = found.mp3.unwrap_or_default();
                    Form { rate, channels, ..Form::default() }
                }
                _ => Form::default(),
            };
            self.add_track(ctx, Codec::Audio(a), form, &mut refused);
        }
        let live = &mut self.live;
        for t in &mut live.tracks {
            match (t.codec, &found.video_record, &found.audio_record) {
                (Codec::Video(v), Some((codec, record)), _) if v == *codec => {
                    t.set_record(ctx, record)
                }
                (Codec::Audio(Audio::Aac), _, Some(record)) => t.set_record(ctx, record),
                _ => false,
            };
        }
        self.reader.expect =
            Expect { audio: found.audio.map(|a| a.1 >> 4), video: found.video.map(|v| v.1) };
        log!(
            &*ctx,
            Level::Debug,
            "head",
            version = file.version,
            origin_ms = live.origin_ms,
            keyframes = found.entries.len() as u64,
        );
        if live.tracks.is_empty() {
            let error = refused.map_or(FlvError::NoStreams, |codec| FlvError::Refused { codec });
            return Err(Self::failed(ctx, error));
        }
        // Pads come at start, the index names the video stream until then.
        live.key = Some(live.tracks.iter().position(|t| t.kind == StreamKind::Video).unwrap_or(0));
        live.grown.audio = live.key.is_some_and(|k| live.tracks[k].kind == StreamKind::Audio);
        let origin_ns = u64::from(live.origin_ms) * 1_000_000;
        let mut entries = found.entries;
        for e in &mut entries {
            *e = IndexEntry::key(e.time_ns.saturating_sub(origin_ns), e.byte);
        }
        entries.dedup_by(|b, a| b.time_ns <= a.time_ns);
        let listed = meta.filepositions.map_or(0, |a| a.count);
        match (listed, entries.len()) {
            (0, _) => log!(&*ctx, Level::Warn, "keyframes_absent"),
            (listed, kept) if (kept as u64) < u64::from(listed) => {
                log!(&*ctx, Level::Warn, "keyframes_untrusted", listed = listed, kept = kept)
            }
            (_, kept) => log!(&*ctx, Level::Info, "keyframes", entries = kept),
        }
        log!(
            &*ctx,
            Level::Info,
            "opened",
            version = file.version,
            tracks = live.tracks.len(),
            duration = live.duration,
        );
        if !entries.is_empty() {
            let stream = live.key.map(|k| live.tracks[k].stream);
            let grouped = live.tracks.iter().any(|t| t.kind == StreamKind::Video);
            let (file_len, duration) = (live.file_len, live.duration);
            live.keyframes = Some(Index {
                entries,
                file_len,
                duration,
                complete: true,
                stream,
                grouped,
                ..Index::default()
            });
        }
        live.publish(ctx);
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.reader.restart(0);
        self.reader.stats = tag::Stats::default();
        let live = &mut self.live;
        for t in &mut live.tracks {
            t.pad = ctx.stream_pad(t.stream).filter(|pad| ctx.pad_linked(*pad));
        }
        live.reset_tracks();
        live.key = None;
        live.rekey();
        (live.grown.lost, live.exact, live.dropped, live.bad, live.strays) =
            (false, false, 0, 0, 0);
        live.park.rows.clear();
        live.park.bytes.clear();
        live.measured.clear();
        live.teller.restart();
        (self.feeding, self.posted_duration, self.resyncs) = (None, false, 0);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.take_in(ctx, &mut inputs)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        self.on_event(ctx, event)
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.feeding = None;
        self.live.park.rows.clear();
        self.live.park.bytes.clear();
    }
}

impl FlvDemux {
    fn take_in(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        if !self.posted_duration {
            self.posted_duration = true;
            if let Some(ns) = self.live.duration.nanos() {
                ctx.set_duration(ns);
            }
        }
        loop {
            // Fed only while output leaves, so a slow consumer backs up into the queue ahead.
            if !self.live.unpark(ctx) {
                ctx.hold_end();
                return Ok(Flow::Ok);
            }
            let (memory, at) = match self.feeding.take() {
                Some(feeding) => feeding,
                None => {
                    let Some(buf) = inputs.pop() else { return Ok(Flow::Ok) };
                    if buf.flags.contains(BufferFlags::DISCONT) {
                        log!(&*ctx, Level::Debug, "discont");
                        self.lose_place();
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

    /// Input from somewhere unknown, read on from the next tag found.
    fn lose_place(&mut self) {
        self.reader.resync();
        self.live.grown.lost = true;
        self.live.reset_tracks();
    }

    /// With `restart` the source went back for the track and the reader reads on from there.
    fn relink(&mut self, ctx: &Ctx, pad: SrcPad, on: bool, restart: Option<SeekTarget>) {
        let Some(at) = self.live.tracks.iter().position(|t| ctx.stream_pad(t.stream) == Some(pad))
        else {
            return;
        };
        if let Some(target) = restart.filter(|_| on) {
            self.rejoin(at, pad, target);
        }
        let live = &mut self.live;
        let t = &mut live.tracks[at];
        t.pad = on.then_some(pad);
        t.started = false;
        t.need_key = t.kind == StreamKind::Video;
        live.rekey();
    }

    /// The track drops what it parked for a full pool, it was read ahead of the restart and
    /// would go out first and again when read. The other tracks keep theirs and drop what they
    /// read again up to their last tag.
    fn rejoin(&mut self, at: usize, pad: SrcPad, target: SeekTarget) {
        let live = &mut self.live;
        let old = live.tracks[at].pad;
        live.park.rows.retain(|row| match *row {
            Parked::Unit { pad: of, .. } | Parked::Format { pad: of, .. } => {
                of != pad && Some(of) != old
            }
        });
        for (n, t) in live.tracks.iter_mut().enumerate() {
            t.resume = t.read.or(t.resume).filter(|_| t.pad.is_some() && n != at);
            t.read = None;
        }
        self.feeding = None;
        self.reader.restart(target.to_byte);
    }

    fn on_event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => {
                self.live.exact = true;
                if let Some((memory, at)) = self.feeding.take() {
                    self.feed(ctx, &memory.data()[at..], false);
                }
                self.feed(ctx, &[], true);
                self.live.unpark(ctx);
                if self.live.keyframes.is_none() && self.live.grown.unpublished() {
                    self.live.publish(ctx);
                }
                let s = self.reader.stats;
                let (tags, resyncs, skipped, loose) = (s.tags, s.resyncs, s.skipped, s.loose);
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    tags = tags,
                    resyncs = resyncs,
                    skipped = skipped,
                    loose = loose
                );
                let (dropped, bad, strays) = (self.live.dropped, self.live.bad, self.live.strays);
                if dropped + bad + strays > 0 {
                    log!(&*ctx, Level::Warn, "lost", frames = dropped, bad = bad, strays = strays);
                }
            }
            Event::FlushStart => {
                let live = &mut self.live;
                live.measured.clear();
                live.teller.restart();
                if live.keyframes.is_none() && live.grown.unpublished() {
                    live.publish(ctx);
                }
                let target = ctx.seek_target();
                let at = target.map_or(0, |t| t.to_byte);
                log!(&*ctx, Level::Debug, "flush", seek = target.is_some(), at = at);
                match target {
                    Some(target) => {
                        self.reader.restart(target.to_byte);
                        self.live.grown.lost = false;
                    }
                    None => self.lose_place(),
                }
                let live = &mut self.live;
                live.reset_tracks();
                (live.exact, self.feeding) = (false, None);
                live.park.rows.clear();
                live.park.bytes.clear();
            }
            Event::SrcLinked { pad, on, restart } => self.relink(ctx, *pad, *on, *restart),
            _ => {}
        }
        Ok(())
    }
}
