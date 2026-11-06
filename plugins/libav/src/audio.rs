//! libavcodec audio decoders (AAC, AC-3/E-AC-3, MP3, DTS, TrueHD/MLP, Vorbis, ALAC, Speex) as
//! elements.
//! Compressed audio in, interleaved `audio/raw` out in libavutil's native channel order.
//! Float to s16 matches `ffmpeg -f s16le` (libswresample audioconvert.c).

use std::fmt;

use profluens_audio::bytes_in::ByteIn;
use profluens_audio::{
    DELAY_FIELDS, FIELD_DELAY, FIELD_LAYOUT, LAYOUT_FIELD, Place, Rebase, SampleFormat,
    SampleFormats, convert_interleaved, converted_len, layout_or_default, link_samples,
};
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::lav::{self, Decoder, Frame, LavError, Received, Samples};

const SINK_PAD: SinkPad = SinkPad(0);
const SRC_PAD: SrcPad = SrcPad(0);

const FAMILY: &str = "audio/raw";
const F_RATE: &str = "rate";
const F_CHANNELS: &str = "channels";
const F_SAMPLE: &str = "sample";

static SAMPLE_VALUES: [ValueDesc; 5] = [
    ValueDesc::Id("u8"),
    ValueDesc::Id("s16"),
    ValueDesc::Id("s24"),
    ValueDesc::Id("s32"),
    ValueDesc::Id("f32"),
];
static SRC_FIELDS: [FieldDesc; 4] = [
    FieldDesc { field: F_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_SAMPLE, allowed: ConstraintDesc::Set(&SAMPLE_VALUES), preferred: None },
    LAYOUT_FIELD,
];
static SRC_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: FAMILY, fields: &SRC_FIELDS }, OfferDesc::any("bytes")];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static COUPLINGS: [Coupling; 1] = [Coupling::takes(0, F_SAMPLE)];

static AAC_FIELDS: [FieldDesc; 3] = [
    FieldDesc { field: F_RATE, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: F_CHANNELS, allowed: ConstraintDesc::Any, preferred: None },
    FieldDesc { field: FIELD_DELAY, allowed: ConstraintDesc::Any, preferred: None },
];
static AAC_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "aac", fields: &AAC_FIELDS }, OfferDesc::any("bytes")];
static AC3_SINK: [OfferDesc; 3] = [
    OfferDesc { family: "ac3", fields: &DELAY_FIELDS },
    OfferDesc { family: "eac3", fields: &DELAY_FIELDS },
    OfferDesc::any("bytes"),
];
static MP3_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "mp3", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static DTS_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "dts", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static TRUEHD_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "truehd", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static MLP_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "mlp", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static VORBIS_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "vorbis", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static AAC_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &AAC_SINK }];
static AC3_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &AC3_SINK }];
static MP3_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &MP3_SINK }];
static DTS_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &DTS_SINK }];
static TRUEHD_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TRUEHD_SINK }];
static MLP_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &MLP_SINK }];
static VORBIS_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &VORBIS_SINK }];
static ALAC_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "alac", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static ALAC_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ALAC_SINK }];
static SPEEX_SINK: [OfferDesc; 2] =
    [OfferDesc { family: "speex", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static SPEEX_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SPEEX_SINK }];

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    sched: SchedHint,
    make: fn() -> Box<dyn Element>,
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs: &SRCS,
        props: &[],
        sched,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: Some(make),
        io: false,
        couplings: &COUPLINGS,
    }
}

// One-time construction.
#[allow(clippy::disallowed_methods)]
pub(crate) static AAC_DESC: ElementDesc =
    desc("libavaacdec", &AAC_SINKS, SchedHint::Active, || Box::new(LibavAudioDec::aac()));
#[allow(clippy::disallowed_methods)]
pub(crate) static AC3_DESC: ElementDesc =
    desc("libavac3dec", &AC3_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::ac3()));
#[allow(clippy::disallowed_methods)]
pub(crate) static MP3_DESC: ElementDesc =
    desc("libavmp3dec", &MP3_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::mp3()));
#[allow(clippy::disallowed_methods)]
pub(crate) static DTS_DESC: ElementDesc =
    desc("libavdtsdec", &DTS_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::dts()));
#[allow(clippy::disallowed_methods)]
pub(crate) static TRUEHD_DESC: ElementDesc =
    desc("libavtruehddec", &TRUEHD_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::truehd()));
#[allow(clippy::disallowed_methods)]
pub(crate) static MLP_DESC: ElementDesc =
    desc("libavmlpdec", &MLP_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::mlp()));
#[allow(clippy::disallowed_methods)]
pub(crate) static VORBIS_DESC: ElementDesc =
    desc("libavvorbisdec", &VORBIS_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::vorbis()));
#[allow(clippy::disallowed_methods)]
pub(crate) static ALAC_DESC: ElementDesc =
    desc("libavalacdec", &ALAC_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::alac()));
#[allow(clippy::disallowed_methods)]
pub(crate) static SPEEX_DESC: ElementDesc =
    desc("libavspeexdec", &SPEEX_SINKS, SchedHint::Passive, || Box::new(LibavAudioDec::speex()));

const FRAMES_PER_BATCH: u32 = 2;

const HEAD_MAX: usize = 64;

const WARNINGS: u32 = 3;

/// Output buffers hold at least 1/50 s so tiny TrueHD and DTS frames do not cost a buffer each.
const MIN_OUT_PER_SEC: u32 = 50;

/// Empty comment header laced between the other two (Vorbis I 5.2.1). libavcodec skips it.
const VORBIS_COMMENT: [u8; 16] = *b"\x03vorbis\0\0\0\0\0\0\0\0\x01";

/// The Speex identification header, `Speex` and three spaces up front (Speex manual 7.3, Table
/// 7.1). libavcodec reads it as extradata.
const SPEEX_HEAD: usize = 80;
const SPEEX_MAGIC: &[u8; 8] = b"Speex   ";
const SPEEX_EXTRA_AT: usize = 68;
/// Extra headers past the comment a Speex stream may claim, as pf-ogg caps them.
const SPEEX_EXTRA_MAX: u32 = 16;

/// Why a libav audio decoder dropped something.
#[derive(Debug, PartialEq, Eq)]
pub enum LavAudioError {
    Open(LavError),
    Packet { index: u64, cause: LavError },
    Parse(LavError),
    Format { channels: usize },
    SlotTooSmall { need: usize, capacity: usize },
}

impl fmt::Display for LavAudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LavAudioError::Open(e) => write!(f, "libavcodec audio decoder not opened, {e}"),
            LavAudioError::Packet { index, cause } => write!(f, "packet {index} dropped, {cause}"),
            LavAudioError::Parse(e) => write!(f, "parser refused the stream, {e}"),
            LavAudioError::Format { channels } => {
                write!(f, "a frame of {channels} channels in a sample format not put out")
            }
            LavAudioError::SlotTooSmall { need, capacity } => {
                write!(f, "a sample frame needs {need} bytes but pool slots hold {capacity}")
            }
        }
    }
}

impl std::error::Error for LavAudioError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Framing {
    Parse,
    AacHead,
    AacRaw,
    VorbisHead,
    VorbisRaw,
    AlacHead,
    AlacRaw,
    SpeexHead,
    SpeexRaw,
}

impl Framing {
    fn first(codec: lav::Codec) -> Framing {
        match codec {
            lav::Codec::Aac => Framing::AacHead,
            lav::Codec::Vorbis => Framing::VorbisHead,
            lav::Codec::Alac => Framing::AlacHead,
            lav::Codec::Speex => Framing::SpeexHead,
            _ => Framing::Parse,
        }
    }

    /// Headers ride in band ahead of the packets and go unstamped.
    fn in_band_heads(self) -> bool {
        matches!(
            self,
            Framing::VorbisHead | Framing::VorbisRaw | Framing::SpeexHead | Framing::SpeexRaw
        )
    }
}

/// The Speex identification header the decoder opened with and the header packets behind it
/// still to drop, the comment and any extra headers.
struct SpeexHeads {
    ident: [u8; SPEEX_HEAD],
    skip: u32,
}

struct VorbisHeads {
    ident: Vec<u8>,
    laced: Vec<u8>,
}

pub struct LibavAudioDec {
    desc: &'static ElementDesc,
    codec: lav::Codec,
    framing: Framing,
    decoder: Option<Decoder>,
    buf: Vec<u8>,
    id3: Option<usize>,
    rate: u32,
    channels: usize,
    sample: SampleFormat,
    layout: u64,
    takes: SampleFormats,
    conv: Vec<u8>,
    frame_len: usize,
    at: Place,
    rebase: Rebase,
    /// Backpressure carry. Nothing is decoded while it holds anything.
    pending: Vec<u8>,
    pending_pos: usize,
    errors: u32,
    packets: u64,
    /// Packets that failed to decode and frames not put out, for the end of stream summary.
    failed: u64,
    drops: u64,
    head: ([u8; HEAD_MAX], usize),
    rehead: bool,
    reformat: bool,
    /// First frame of a new format, held until pending output of the old one is emitted.
    held: Option<Frame>,
    primed: bool,
    open_failed: bool,
    /// MP3 read from byte 0 with the source length. The decoder then publishes the seek index.
    indexing: Option<Option<u64>>,
    scan: Option<pf_mp3::index::HeadScan>,
    clock: Option<pf_mp3::index::Clock>,
    base: u64,
    coded: Option<u64>,
    input: ByteIn,
    vorbis: VorbisHeads,
    speex: SpeexHeads,
}

impl LibavAudioDec {
    pub const IN: SinkPad = SINK_PAD;
    pub const OUT: SrcPad = SRC_PAD;

    // One-time setup.
    #[allow(clippy::disallowed_methods)]
    fn with(desc: &'static ElementDesc, codec: lav::Codec, framing: Framing) -> Self {
        Self {
            desc,
            codec,
            framing,
            decoder: None,
            buf: Vec::new(),
            id3: None,
            rate: 0,
            channels: 0,
            sample: SampleFormat::S16,
            layout: 0,
            takes: SampleFormats::NONE,
            conv: Vec::new(),
            frame_len: 0,
            at: Place::default(),
            rebase: Rebase::Open,
            pending: Vec::new(),
            pending_pos: 0,
            errors: 0,
            packets: 0,
            failed: 0,
            drops: 0,
            head: ([0; HEAD_MAX], 0),
            rehead: false,
            reformat: false,
            held: None,
            primed: false,
            open_failed: false,
            indexing: None,
            scan: None,
            clock: None,
            base: 0,
            coded: None,
            input: ByteIn::new(SINK_PAD),
            vorbis: VorbisHeads { ident: Vec::new(), laced: Vec::new() },
            speex: SpeexHeads { ident: [0; SPEEX_HEAD], skip: 0 },
        }
    }

    /// MP3 only. Publish the file's seek index from its head.
    pub fn indexing(mut self, len: Option<u64>) -> Self {
        self.indexing = (self.codec == lav::Codec::Mp3).then_some(len);
        self
    }

    fn scan(&mut self, ctx: &mut Ctx, bytes: &[u8]) {
        if let Some(index) = self.scan.as_mut().and_then(|scan| scan.feed(bytes)) {
            self.clocked(ctx, index);
        }
    }

    fn end_scan(&mut self, ctx: &mut Ctx) {
        if let Some(index) = self.scan.as_mut().and_then(pf_mp3::index::HeadScan::end) {
            self.clocked(ctx, index);
        }
    }

    fn clocked(&mut self, ctx: &mut Ctx, index: profluens_core::pipeline::Index) {
        let layout = self.scan.as_ref().and_then(pf_mp3::index::HeadScan::layout);
        self.clock = layout.map(|layout| pf_mp3::index::Clock::new(layout, &index));
        log!(
            &*ctx,
            Level::Debug,
            "index",
            entries = index.entries.len(),
            duration = index.duration,
            clocked = self.clock.is_some(),
        );
        pf_mp3::index::publish(ctx, index);
    }

    fn publish_grown(&mut self, ctx: &mut Ctx) {
        if let Some(index) = self.clock.as_mut().and_then(pf_mp3::index::Clock::index) {
            pf_mp3::index::publish(ctx, index);
        }
    }

    pub fn aac() -> Self {
        Self::with(&AAC_DESC, lav::Codec::Aac, Framing::AacHead)
    }

    pub fn ac3() -> Self {
        Self::with(&AC3_DESC, lav::Codec::Ac3, Framing::Parse)
    }

    pub fn mp3() -> Self {
        Self::with(&MP3_DESC, lav::Codec::Mp3, Framing::Parse)
    }

    pub fn dts() -> Self {
        Self::with(&DTS_DESC, lav::Codec::Dts, Framing::Parse)
    }

    pub fn truehd() -> Self {
        Self::with(&TRUEHD_DESC, lav::Codec::TrueHd, Framing::Parse)
    }

    pub fn mlp() -> Self {
        Self::with(&MLP_DESC, lav::Codec::Mlp, Framing::Parse)
    }

    pub fn vorbis() -> Self {
        Self::with(&VORBIS_DESC, lav::Codec::Vorbis, Framing::VorbisHead)
    }

    pub fn alac() -> Self {
        Self::with(&ALAC_DESC, lav::Codec::Alac, Framing::AlacHead)
    }

    pub fn speex() -> Self {
        Self::with(&SPEEX_DESC, lav::Codec::Speex, Framing::SpeexHead)
    }

    fn tagged(&self) -> bool {
        matches!(self.codec, lav::Codec::Mp3 | lav::Codec::Aac)
    }

    fn reset(&mut self) {
        self.framing = Framing::first(self.codec);
        self.decoder = None;
        self.buf.clear();
        self.id3 = None;
        self.rate = 0;
        self.channels = 0;
        self.sample = SampleFormat::S16;
        self.layout = 0;
        self.frame_len = 0;
        self.at = Place::default();
        self.rebase = Rebase::Open;
        self.pending.clear();
        self.pending_pos = 0;
        self.errors = 0;
        self.packets = 0;
        self.failed = 0;
        self.drops = 0;
        self.head.1 = 0;
        self.rehead = false;
        self.reformat = false;
        self.held = None;
        self.primed = false;
        self.open_failed = false;
        self.scan = self.indexing.map(pf_mp3::index::HeadScan::new);
        self.clock = None;
        self.base = 0;
        self.coded = None;
        self.vorbis.ident.clear();
        self.vorbis.laced.clear();
        self.speex.skip = 0;
    }

    /// Count a frame or packet not put out, `true` for the first few, which are logged.
    fn drop_counted(&mut self) -> bool {
        self.drops += 1;
        self.drops <= u64::from(WARNINGS)
    }

    fn warn(ctx: &mut Ctx, error: LavAudioError) {
        let element = ctx.element();
        ctx.post(BusMessage::Warning { element, error: Error::element_failed(element, error) });
    }

    fn ensure_parser(&mut self, ctx: &mut Ctx) -> bool {
        if self.decoder.is_some() {
            return true;
        }
        if self.open_failed {
            return false;
        }
        match Decoder::open(self.codec, &[], None, true) {
            Ok(d) => {
                log!(&*ctx, Level::Info, "parser_open", codec = self.codec.name());
                self.decoder = Some(d);
                true
            }
            Err(e) => {
                self.open_failed = true;
                log!(&*ctx, Level::Error, "open_failed", codec = self.codec.name(), err = e.code());
                Self::warn(ctx, LavAudioError::Open(e));
                false
            }
        }
    }

    fn configure(&mut self, ctx: &mut Ctx, data: &[u8]) -> Result<(), Error> {
        if is_adts(data) || data.starts_with(b"ID3") {
            log!(&*ctx, Level::Debug, "aac_framed", adts = is_adts(data), bytes = data.len());
            self.framing = Framing::Parse;
            if self.ensure_parser(ctx) {
                self.take_bytes(data);
            }
            return Ok(());
        }
        match Decoder::open(lav::Codec::Aac, data, None, false) {
            Ok(d) => {
                log!(&*ctx, Level::Info, "decoder_open", codec = "aac", config = data.len());
                self.decoder = Some(d);
                self.framing = Framing::AacRaw;
                self.head.1 = 0;
                if let Some(room) = self.head.0.get_mut(..data.len()) {
                    room.copy_from_slice(data);
                    self.head.1 = data.len();
                }
                Ok(())
            }
            Err(e) => Err(Error::element_failed(ctx.element(), LavAudioError::Open(e))),
        }
    }

    fn open_alac(&mut self, ctx: &mut Ctx, data: &[u8]) -> Result<(), Error> {
        let fail = |e| Err(Error::element_failed(ctx.element(), LavAudioError::Open(e)));
        let Some(atom) = alac_atom(data) else { return fail(LavError::Argument) };
        match Decoder::open(lav::Codec::Alac, &atom, None, false) {
            Ok(d) => {
                log!(&*ctx, Level::Info, "decoder_open", codec = "alac", config = data.len());
                self.decoder = Some(d);
                self.framing = Framing::AlacRaw;
                self.head.1 = data.len().min(HEAD_MAX);
                self.head.0[..self.head.1].copy_from_slice(&data[..self.head.1]);
                Ok(())
            }
            Err(e) => fail(e),
        }
    }

    fn vorbis_audio(&mut self, ctx: &mut Ctx, data: &[u8]) -> bool {
        let changed = std::mem::take(&mut self.rehead);
        if data.is_empty() || data == self.vorbis.laced.as_slice() {
            return false;
        }
        if (self.framing == Framing::VorbisHead || changed)
            && let Some([ident, _, _]) = split_laced(data)
        {
            self.vorbis.ident.clear();
            self.vorbis.ident.extend_from_slice(ident);
            self.vorbis.laced.clear();
            self.vorbis.laced.extend_from_slice(data);
            self.open_vorbis(ctx);
            return false;
        }
        // Audio packets have bit 0 clear, headers an odd type (Vorbis I 4.2.1).
        if data[0] & 1 == 0 {
            if self.framing == Framing::VorbisRaw {
                return true;
            }
            if self.drop_counted() {
                log!(&*ctx, Level::Warn, "frame_drop", reason = "no_headers", bytes = data.len());
            }
            return false;
        }
        match vorbis_header(data) {
            Some(1) if data != self.vorbis.ident.as_slice() => {
                log!(&*ctx, Level::Debug, "vorbis_reheader", bytes = data.len());
                self.vorbis.ident.clear();
                self.vorbis.ident.extend_from_slice(data);
                self.vorbis.laced.clear();
                self.decoder = None;
                self.framing = Framing::VorbisHead;
            }
            Some(5) if !self.vorbis.ident.is_empty() && !self.vorbis.laced.ends_with(data) => {
                lace(&mut self.vorbis.laced, &self.vorbis.ident, data);
                self.open_vorbis(ctx);
            }
            _ => {}
        }
        false
    }

    fn open_vorbis(&mut self, ctx: &mut Ctx) {
        match Decoder::open(lav::Codec::Vorbis, &self.vorbis.laced, None, false) {
            Ok(d) => {
                log!(
                    &*ctx,
                    Level::Info,
                    "decoder_open",
                    codec = "vorbis",
                    config = self.vorbis.laced.len()
                );
                self.decoder = Some(d);
                self.framing = Framing::VorbisRaw;
                self.errors = 0;
            }
            Err(e) => {
                log!(&*ctx, Level::Warn, "open_failed", codec = "vorbis", err = e.code());
                self.decoder = None;
                self.framing = Framing::VorbisHead;
                self.vorbis.ident.clear();
                self.vorbis.laced.clear();
                Self::warn(ctx, LavAudioError::Open(e));
            }
        }
    }

    /// `true` for a Speex packet to decode. Ogg Speex has no magic on its comment and extra
    /// headers, so the count the identification header states drops them.
    fn speex_audio(&mut self, ctx: &mut Ctx, data: &[u8]) -> bool {
        if let Some(ident) = data.get(..SPEEX_HEAD).filter(|d| d.starts_with(SPEEX_MAGIC)) {
            let extra = u32::from_le_bytes(
                ident[SPEEX_EXTRA_AT..SPEEX_EXTRA_AT + 4].try_into().unwrap_or_default(),
            );
            self.speex.skip = 1 + extra.min(SPEEX_EXTRA_MAX);
            if self.framing == Framing::SpeexRaw && ident == self.speex.ident.as_slice() {
                return false;
            }
            match Decoder::open(lav::Codec::Speex, ident, None, false) {
                Ok(d) => {
                    log!(&*ctx, Level::Info, "decoder_open", codec = "speex", extra = extra);
                    self.decoder = Some(d);
                    self.framing = Framing::SpeexRaw;
                    self.speex.ident.copy_from_slice(ident);
                    self.errors = 0;
                }
                Err(e) => {
                    log!(&*ctx, Level::Warn, "open_failed", codec = "speex", err = e.code());
                    self.decoder = None;
                    self.framing = Framing::SpeexHead;
                    Self::warn(ctx, LavAudioError::Open(e));
                }
            }
            return false;
        }
        if self.speex.skip > 0 {
            self.speex.skip -= 1;
            return false;
        }
        if self.framing == Framing::SpeexRaw {
            return !data.is_empty();
        }
        if self.drop_counted() {
            log!(&*ctx, Level::Warn, "frame_drop", reason = "no_headers", bytes = data.len());
        }
        false
    }

    fn is_head(&self, data: &[u8]) -> bool {
        self.head.1 > 0 && data == &self.head.0[..self.head.1]
    }

    fn take_bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if !self.tagged() {
            return;
        }
        if self.id3.is_none() {
            match id3v2_len(&self.buf) {
                Some(len) => self.id3 = Some(len),
                None => return,
            }
        }
        if let Some(skip) = self.id3.filter(|s| *s > 0) {
            let n = skip.min(self.buf.len());
            self.buf.drain(..n);
            self.base += n as u64;
            self.id3 = Some(skip - n);
        }
    }

    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.buf.len());
        self.buf.drain(..n);
        self.base += n as u64;
    }

    fn resync(&mut self, ctx: &Ctx) -> bool {
        use pf_mp3::index::Resync;
        let Some(clock) = self.clock.as_mut().filter(|c| c.seeking()) else {
            return true;
        };
        match clock.resync(&self.buf, self.base) {
            Resync::Drop(n) => {
                self.drop_front(n);
                false
            }
            Resync::Frame { skip, k } => {
                log!(ctx, Level::Debug, "resync", skip = skip, exact = k.is_some(), at = self.base);
                self.drop_front(skip);
                if let Some(clock) = &mut self.clock {
                    self.coded =
                        k.map(|k| clock.layout().coded(k)).or_else(|| clock.estimate(self.base));
                    let _ = clock.walk(&self.buf, self.base);
                }
                true
            }
            Resync::GaveUp => {
                log!(ctx, Level::Warn, "resync_gave_up", at = self.base, held = self.buf.len());
                true
            }
        }
    }

    fn parse_one(&mut self, ctx: &mut Ctx) -> bool {
        if self.id3 != Some(0) && self.tagged() {
            return false;
        }
        if self.scan.as_ref().is_some_and(|scan| !scan.done()) || !self.resync(ctx) {
            return false;
        }
        loop {
            if self.buf.is_empty() {
                return false;
            }
            let Some(decoder) = &mut self.decoder else {
                return false;
            };
            let parsed = decoder.parse_send(&self.buf, lav::NONE, &mut |_| None);
            let parsed = match parsed {
                Ok(p) => p,
                Err(e) => {
                    log!(
                        &*ctx,
                        Level::Warn,
                        "parse_error",
                        err = e.code(),
                        dropped = self.buf.len()
                    );
                    Self::warn(ctx, LavAudioError::Parse(e));
                    self.drop_front(self.buf.len());
                    if let Some(clock) = &mut self.clock {
                        clock.lost();
                    }
                    return false;
                }
            };
            self.drop_front(parsed.used);
            match parsed.sent {
                Some((sent, _)) => {
                    self.after_send(ctx, sent);
                    return true;
                }
                None if parsed.used == 0 => return false,
                None => {}
            }
        }
    }

    fn decode_unit(&mut self, ctx: &mut Ctx, data: &[u8]) {
        let Some(decoder) = &mut self.decoder else {
            return;
        };
        let sent = decoder.send(data, lav::NONE, lav::NONE, 0, &mut |_| None);
        self.after_send(ctx, sent);
    }

    fn after_send(&mut self, ctx: &mut Ctx, sent: Result<lav::Sent, LavError>) {
        let index = self.packets;
        self.packets += 1;
        let before = self.pending.len();
        let failed = match sent {
            Ok(_) => self.receive(ctx).err(),
            Err(e) => Some(e),
        };
        match failed {
            None => self.errors = 0,
            Some(cause) => {
                self.errors = self.errors.saturating_add(1);
                self.failed += 1;
                let conceal = self.pending.len() == before && self.channels > 0;
                if self.errors <= WARNINGS {
                    log!(
                        &*ctx,
                        Level::Warn,
                        "packet_error",
                        index = index,
                        err = cause.code(),
                        run = self.errors,
                        conceal = conceal,
                    );
                    Self::warn(ctx, LavAudioError::Packet { index, cause });
                }
                if conceal {
                    let silence = if self.sample == SampleFormat::U8 { 0x80 } else { 0 };
                    self.pending.resize(before + self.frame_len * self.stride(), silence);
                }
            }
        }
    }

    fn receive(&mut self, ctx: &mut Ctx) -> Result<(), LavError> {
        loop {
            let Some(decoder) = &mut self.decoder else {
                return Ok(());
            };
            if !self.reformat
                && let Some(frame) = self.held.take()
            {
                self.take_frame(ctx, &frame);
                if self.reformat {
                    self.held = Some(frame);
                    return Ok(());
                }
                continue;
            }
            if self.reformat {
                return Ok(());
            }
            match decoder.receive(&mut |_| None)? {
                Received::Frame(frame) => {
                    self.take_frame(ctx, &frame);
                    if self.reformat {
                        self.held = Some(frame);
                        return Ok(());
                    }
                }
                Received::Again | Received::Ended => return Ok(()),
            }
        }
    }

    fn take_frame(&mut self, ctx: &mut Ctx, frame: &Frame) {
        let (rate, channels) = (frame.sample_rate(), frame.channels());
        if channels == 0 || rate == 0 || frame.samples() == 0 {
            if frame.samples() > 0 && self.drop_counted() {
                log!(
                    &*ctx,
                    Level::Warn,
                    "frame_drop",
                    reason = "no_shape",
                    rate = rate,
                    channels = channels
                );
            }
            return;
        }
        let Some((format, _)) = frame.sample_format() else {
            if self.drop_counted() {
                log!(
                    &*ctx,
                    Level::Warn,
                    "frame_drop",
                    reason = "sample_format",
                    channels = channels
                );
            }
            Self::warn(ctx, LavAudioError::Format { channels });
            return;
        };
        let sample = out_sample(own_sample(format), self.takes);
        let mask = frame.channel_mask();
        let layout = layout_or_default((mask != 0).then_some(mask), channels as u32);
        let changed = (rate, channels, sample, layout)
            != (self.rate, self.channels, self.sample, self.layout);
        if changed && self.pending_pos < self.pending.len() {
            log!(
                &*ctx,
                Level::Trace,
                "reformat_wait",
                held = self.pending.len() - self.pending_pos
            );
            self.reformat = true;
            return;
        }
        // Samples libavcodec asks to skip take no place in the stream.
        let skip = frame.skip();
        if skip > 0 {
            log!(&*ctx, Level::Debug, "skip", samples = skip, of = frame.samples());
            let delayed = profluens_audio::declared_delay(ctx, SINK_PAD).0 > 0;
            self.rebase = past_skip(self.rebase, skip, rate, delayed);
            self.primed = skip == frame.samples();
        }
        if changed {
            let first = self.rate == 0;
            log!(
                &*ctx,
                if first { Level::Info } else { Level::Debug },
                if first { "format" } else { "format_change" },
                rate = rate,
                channels = channels,
                sample = sample.caps_name(),
                layout = layout,
            );
            if self.rate != 0 && self.rate != rate {
                // Positions count samples, so they move to the new rate or the stamps jump.
                let old = u128::from(self.rate);
                let scale = |n: u64| (u128::from(n) * u128::from(rate) / old) as u64;
                (self.at.position, self.at.lead) = (scale(self.at.position), scale(self.at.lead));
                self.coded = self.coded.map(scale);
            }
            self.rate = rate;
            self.channels = channels;
            self.sample = sample;
            self.layout = layout;
            ctx.announce_format(
                SRC_PAD,
                FAMILY,
                &[
                    (F_RATE, ValueDesc::Int(i64::from(rate))),
                    (F_CHANNELS, ValueDesc::Int(channels as i64)),
                    (F_SAMPLE, ValueDesc::Id(sample.caps_name())),
                    (FIELD_LAYOUT, ValueDesc::Int(layout as i64)),
                ],
            );
        }
        self.frame_len = frame.samples();
        let start = self.pending.len();
        if !interleave(frame, sample, &mut self.pending, &mut self.conv) {
            if self.drop_counted() {
                log!(&*ctx, Level::Warn, "frame_drop", reason = "interleave", channels = channels);
            }
            Self::warn(ctx, LavAudioError::Format { channels });
        } else if skip > 0 {
            let end = (start + skip * self.stride()).min(self.pending.len());
            self.pending.drain(start..end);
        }
    }

    fn stride(&self) -> usize {
        self.sample.bytes() * self.channels.max(1)
    }

    fn place(&mut self, ctx: &Ctx) {
        if self.pending_pos >= self.pending.len() {
            if !std::mem::take(&mut self.primed) {
                self.rebase.dropped();
            }
            if let (Some(coded), Some(clock)) = (&mut self.coded, &self.clock) {
                *coded += u64::from(clock.layout().spf);
            }
        } else if let Some(coded) = self.coded.take() {
            let _ = self.rebase.first_frame(self.rate);
            self.at =
                profluens_audio::place(coded, self.clock.as_ref().map_or(0, |c| c.layout().lead));
        } else if let Some(at) = self.rebase.place_first(ctx, SINK_PAD, self.rate) {
            self.at = at;
        }
    }

    fn skip_before_point(&mut self, ctx: &Ctx) -> bool {
        let stride = self.stride();
        let frames = (self.pending.len() - self.pending_pos) / stride;
        let skip = self.at.cut(frames, self.rate, ctx.presentation_point());
        self.pending_pos += skip * stride;
        if skip > 0 && skip == frames {
            ctx.count_drop();
        }
        self.pending_pos < self.pending.len()
    }

    fn pts_now(&self) -> Timestamp {
        profluens_audio::time_at(self.at.position, self.rate)
    }

    fn emit_pending(&mut self, ctx: &mut Ctx, bounded: bool) -> bool {
        let _ = self.skip_before_point(ctx);
        let stride = self.stride();
        while self.pending_pos < self.pending.len() {
            let mut buf = if bounded {
                match ctx.try_alloc() {
                    Some(b) => b,
                    None => return false,
                }
            } else {
                ctx.alloc()
            };
            let capacity = buf.memory.capacity();
            if capacity < stride {
                if self.drop_counted() {
                    log!(
                        &*ctx,
                        Level::Warn,
                        "frame_drop",
                        reason = "slot_too_small",
                        need = stride,
                        capacity = capacity,
                        bytes = self.pending.len() - self.pending_pos,
                    );
                }
                Self::warn(ctx, LavAudioError::SlotTooSmall { need: stride, capacity });
                self.pending.clear();
                self.pending_pos = 0;
                return true;
            }
            let end = (self.pending_pos + capacity / stride * stride).min(self.pending.len());
            let n = end - self.pending_pos;
            buf.memory.as_mut_full()[..n].copy_from_slice(&self.pending[self.pending_pos..end]);
            buf.memory.set_len(n);
            buf.pts = self.pts_now();
            self.at.position += (n / stride) as u64;
            buf.duration = self.pts_now().saturating_sub(buf.pts);
            ctx.out(SRC_PAD).push(buf);
            self.pending_pos = end;
        }
        self.pending.clear();
        self.pending_pos = 0;
        true
    }

    fn gathering(&self) -> bool {
        let frames = (self.pending.len() - self.pending_pos) / self.stride();
        frames < (self.rate / MIN_OUT_PER_SEC) as usize && !self.reformat
    }

    fn after_packet(&mut self, ctx: &mut Ctx, emitted: &mut u32) -> bool {
        self.place(ctx);
        let produced = self.skip_before_point(ctx);
        if produced && self.gathering() {
            return true;
        }
        self.reformat = false;
        if !self.emit_pending(ctx, true) {
            return false;
        }
        *emitted += u32::from(produced);
        *emitted < FRAMES_PER_BATCH
    }

    fn finish(&mut self, ctx: &mut Ctx) {
        if self.framing == Framing::Parse {
            while self.parse_one(ctx) {
                self.place(ctx);
                self.emit_pending(ctx, false);
            }
            if let Some(decoder) = &mut self.decoder {
                match decoder.parse_send(&self.buf, lav::NONE, &mut |_| None) {
                    Ok(lav::Parsed { sent: Some((sent, _)), .. }) => {
                        self.after_send(ctx, sent);
                        self.place(ctx);
                        self.emit_pending(ctx, false);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log!(
                            &*ctx,
                            Level::Debug,
                            "tail_dropped",
                            err = e.code(),
                            bytes = self.buf.len()
                        );
                    }
                }
                self.buf.clear();
            }
        }
        let Some(decoder) = &mut self.decoder else {
            return;
        };
        match decoder.drain() {
            Ok(()) => {
                // A format change mid drain holds a frame until the old format is out.
                loop {
                    if let Err(e) = self.receive(ctx) {
                        log!(&*ctx, Level::Warn, "drain_error", err = e.code());
                    }
                    self.place(ctx);
                    self.emit_pending(ctx, false);
                    if self.held.is_none() || self.pending_pos < self.pending.len() {
                        break;
                    }
                    self.reformat = false;
                }
            }
            Err(e) => log!(&*ctx, Level::Warn, "drain_failed", err = e.code()),
        }
        if let Some(d) = &mut self.decoder {
            d.flush();
        }
    }

    fn pass(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<Flow, Error> {
        // Held data keeps EOS waiting. A source that hands the whole file over in one read
        // leaves no input behind to wake this up.
        if !self.gathering() && !self.emit_pending(ctx, true) {
            ctx.hold_end();
            return Ok(Flow::Ok);
        }
        let mut emitted = 0u32;
        loop {
            if self.framing == Framing::Parse && self.parse_one(ctx) {
                if !self.after_packet(ctx, &mut emitted) {
                    ctx.hold_end();
                    return Ok(Flow::Ok);
                }
                continue;
            }
            let Some(inbuf) = self.input.next(ctx, inputs) else {
                return Ok(Flow::Ok);
            };
            let data = inbuf.memory.data();
            if !self.framing.in_band_heads() {
                self.rebase.input(inbuf.pts);
            }
            self.scan(ctx, data);
            match self.framing {
                Framing::Parse => {
                    if self.ensure_parser(ctx) {
                        let held = self.buf.len();
                        self.take_bytes(data);
                        let walked =
                            self.clock.as_mut().filter(|c| !c.seeking()).is_some_and(|clock| {
                                let at = self.base + held as u64;
                                self.buf.get(held..).is_some_and(|new| clock.walk(new, at))
                            });
                        if walked {
                            self.publish_grown(ctx);
                        }
                    }
                }
                Framing::AacHead => self.configure(ctx, data)?,
                Framing::AacRaw => {
                    let changed = std::mem::take(&mut self.rehead);
                    if self.is_head(data) || data.is_empty() {
                        continue;
                    }
                    if changed
                        && data.len() <= HEAD_MAX
                        && let Ok(d) = Decoder::open(lav::Codec::Aac, data, None, false)
                    {
                        log!(&*ctx, Level::Debug, "aac_reconfig", config = data.len());
                        self.decoder = Some(d);
                        self.head.1 = data.len();
                        self.head.0[..data.len()].copy_from_slice(data);
                        self.errors = 0;
                        continue;
                    }
                    self.decode_unit(ctx, data);
                    if !self.after_packet(ctx, &mut emitted) {
                        ctx.hold_end();
                        return Ok(Flow::Ok);
                    }
                }
                Framing::AlacHead => self.open_alac(ctx, data)?,
                Framing::AlacRaw => {
                    let changed = std::mem::take(&mut self.rehead);
                    if self.is_head(data) || data.is_empty() {
                        continue;
                    }
                    if changed && alac_atom(data).is_some() {
                        self.open_alac(ctx, data)?;
                        continue;
                    }
                    self.decode_unit(ctx, data);
                    if !self.after_packet(ctx, &mut emitted) {
                        ctx.hold_end();
                        return Ok(Flow::Ok);
                    }
                }
                Framing::VorbisHead | Framing::VorbisRaw => {
                    if !self.vorbis_audio(ctx, data) {
                        continue;
                    }
                    self.rebase.input(inbuf.pts);
                    self.decode_unit(ctx, data);
                    if !self.after_packet(ctx, &mut emitted) {
                        ctx.hold_end();
                        return Ok(Flow::Ok);
                    }
                }
                Framing::SpeexHead | Framing::SpeexRaw => {
                    if !self.speex_audio(ctx, data) {
                        continue;
                    }
                    self.rebase.input(inbuf.pts);
                    self.decode_unit(ctx, data);
                    if !self.after_packet(ctx, &mut emitted) {
                        ctx.hold_end();
                        return Ok(Flow::Ok);
                    }
                }
            }
        }
    }
}

impl Element for LibavAudioDec {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.indexing.is_some() {
            ctx.will_index();
        }
        self.reset();
        self.takes = link_samples(ctx, SRC_PAD);
        self.input.start(ctx, self.indexing.flatten());
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let flow = self.pass(ctx, &mut inputs);
        self.input.ask(ctx);
        flow
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => {
                log!(
                    &*ctx,
                    Level::Debug,
                    "flush",
                    to_byte = ctx.seek_target().map_or(0, |t| t.to_byte),
                    held = self.buf.len(),
                    pending = self.pending.len() - self.pending_pos,
                );
                self.end_scan(ctx);
                self.publish_grown(ctx);
                if let Some(d) = &mut self.decoder {
                    d.flush();
                }
                self.buf.clear();
                self.pending.clear();
                self.pending_pos = 0;
                self.errors = 0;
                self.rebase = Rebase::Open;
                self.primed = false;
                self.at.lead = 0;
                self.base = ctx.seek_target().map_or(0, |t| t.to_byte);
                self.coded = None;
                if let Some(clock) = &mut self.clock {
                    clock.restart();
                }
            }
            Event::Eos => {
                self.end_scan(ctx);
                self.finish(ctx);
                self.publish_grown(ctx);
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    packets = self.packets,
                    failed = self.failed,
                    dropped = self.drops,
                    position = self.at.position,
                );
            }
            Event::FormatChange(_) => {
                self.rehead =
                    matches!(self.framing, Framing::AacRaw | Framing::VorbisRaw | Framing::AlacRaw);
                log!(&*ctx, Level::Debug, "format_event", rehead = self.rehead);
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.reset();
    }
}

/// The 36 byte `alac` atom libavcodec takes as extradata, from the atom itself or the bare 24 byte
/// ALACSpecificConfig MP4 and Matroska carry.
fn alac_atom(cookie: &[u8]) -> Option<[u8; 36]> {
    let mut atom = [0u8; 36];
    match cookie.len() {
        24 => {
            atom[..8].copy_from_slice(b"\0\0\0\x24alac");
            atom[12..].copy_from_slice(cookie);
        }
        36.. if &cookie[4..8] == b"alac" => atom.copy_from_slice(&cookie[..36]),
        _ => return None,
    }
    Some(atom)
}

/// ADTS sync word (ISO/IEC 14496-3 1.A.2.2.1).
fn is_adts(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0
}

fn vorbis_header(packet: &[u8]) -> Option<u8> {
    match packet {
        [t @ (1 | 3 | 5), b'v', b'o', b'r', b'b', b'i', b's', ..] => Some(*t),
        _ => None,
    }
}

/// The three headers of Matroska's Xiph laced `A_VORBIS` CodecPrivate.
fn split_laced(head: &[u8]) -> Option<[&[u8]; 3]> {
    let (&2, mut rest) = head.split_first()? else {
        return None;
    };
    let mut lens = [0usize; 2];
    for len in &mut lens {
        loop {
            let (&b, tail) = rest.split_first()?;
            rest = tail;
            *len += usize::from(b);
            if b != 0xFF {
                break;
            }
        }
    }
    let (ident, rest) = rest.split_at_checked(lens[0])?;
    let (comment, setup) = rest.split_at_checked(lens[1])?;
    (vorbis_header(ident) == Some(1) && vorbis_header(setup) == Some(5))
        .then_some([ident, comment, setup])
}

fn lace(out: &mut Vec<u8>, ident: &[u8], setup: &[u8]) {
    out.clear();
    out.push(2);
    for len in [ident.len(), VORBIS_COMMENT.len()] {
        out.extend(std::iter::repeat_n(0xFF, len / 255));
        out.push((len % 255) as u8);
    }
    out.extend_from_slice(ident);
    out.extend_from_slice(&VORBIS_COMMENT);
    out.extend_from_slice(setup);
}

/// The stamp after libavcodec drops `skip` front samples. A zero stamp is held.
fn past_skip(rebase: Rebase, skip: usize, rate: u32, delayed: bool) -> Rebase {
    let past = |pts: Timestamp| {
        profluens_audio::time_at(profluens_audio::position_at(pts, rate) + skip as u64, rate)
    };
    match rebase {
        Rebase::At(pts) | Rebase::Lost(pts) if rate == 0 => Rebase::Lost(pts),
        Rebase::At(pts) | Rebase::Lost(pts) if pts.0 > 0 => Rebase::Lost(past(pts)),
        Rebase::At(pts) | Rebase::Lost(pts) if delayed => Rebase::At(past(pts)),
        Rebase::At(pts) | Rebase::Lost(pts) => Rebase::At(pts),
        r => r,
    }
}

/// Leading ID3v2 tag length, 0 for none, `None` while undecided (ID3v2.4.0 3.1).
fn id3v2_len(buf: &[u8]) -> Option<usize> {
    const MAGIC: &[u8; 3] = b"ID3";
    if buf.len() < 3 {
        return if MAGIC.starts_with(buf) { None } else { Some(0) };
    }
    if &buf[..3] != MAGIC {
        return Some(0);
    }
    if buf.len() < 10 {
        return None;
    }
    let size = &buf[6..10];
    if size.iter().any(|b| b & 0x80 != 0) || buf[3] == 0xFF || buf[4] == 0xFF {
        return Some(0);
    }
    let body = size.iter().fold(0usize, |acc, b| (acc << 7) | usize::from(*b));
    let footer = if buf[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + body + footer)
}

/// As libswresample, `av_clip_int16(lrintf(x * 32768))`, rounding half to even.
#[inline]
fn f32_to_s16(x: f32) -> i16 {
    (x * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16
}

fn own_sample(format: Samples) -> SampleFormat {
    match format {
        Samples::U8 => SampleFormat::U8,
        Samples::S16 => SampleFormat::S16,
        Samples::S32 => SampleFormat::S32,
        Samples::F32 | Samples::F64 => SampleFormat::F32,
    }
}

fn out_sample(own: SampleFormat, takes: SampleFormats) -> SampleFormat {
    own.nearest_in(takes).unwrap_or(SampleFormat::S16)
}

fn interleave(frame: &Frame, to: SampleFormat, out: &mut Vec<u8>, conv: &mut Vec<u8>) -> bool {
    let Some((format, _)) = frame.sample_format() else {
        return false;
    };
    let own = own_sample(format);
    if to == SampleFormat::S16 {
        return interleave_s16(frame, out);
    }
    if to == own {
        return interleave_own(frame, out);
    }
    conv.clear();
    if !interleave_own(frame, conv) {
        return false;
    }
    let channels = frame.channels();
    let Some(n) = converted_len(own, to, channels, conv.len()) else {
        return false;
    };
    let start = out.len();
    out.resize(start + n, 0);
    if convert_interleaved(own, to, channels, conv, &mut out[start..]).is_none() {
        out.truncate(start);
        return false;
    }
    true
}

fn interleave_own(frame: &Frame, out: &mut Vec<u8>) -> bool {
    let Some((format, planar)) = frame.sample_format() else {
        return false;
    };
    match format {
        Samples::U8 => gather::<1, 1>(frame, planar, out, |s| s),
        Samples::S16 => gather::<2, 2>(frame, planar, out, |s| s),
        Samples::S32 | Samples::F32 => gather::<4, 4>(frame, planar, out, |s| s),
        Samples::F64 => {
            gather::<8, 4>(frame, planar, out, |s| (f64::from_ne_bytes(s) as f32).to_ne_bytes())
        }
    }
}

#[inline(always)]
fn gather<const I: usize, const O: usize>(
    frame: &Frame,
    planar: bool,
    out: &mut Vec<u8>,
    f: impl Fn([u8; I]) -> [u8; O],
) -> bool {
    let (channels, samples) = (frame.channels(), frame.samples());
    let start = out.len();
    out.resize(start + samples * channels * O, 0);
    let dst = out[start..].as_chunks_mut::<O>().0;
    if planar {
        for ch in 0..channels {
            let Some(plane) = frame.audio_plane(ch).get(..samples * I) else {
                out.truncate(start);
                return false;
            };
            for (d, s) in dst[ch..].iter_mut().step_by(channels).zip(plane.as_chunks::<I>().0) {
                *d = f(*s);
            }
        }
    } else {
        let Some(plane) = frame.audio_plane(0).get(..samples * channels * I) else {
            out.truncate(start);
            return false;
        };
        for (d, s) in dst.iter_mut().zip(plane.as_chunks::<I>().0) {
            *d = f(*s);
        }
    }
    true
}

fn interleave_s16(frame: &Frame, out: &mut Vec<u8>) -> bool {
    let Some((format, planar)) = frame.sample_format() else {
        return false;
    };
    let (channels, samples) = (frame.channels(), frame.samples());
    let start = out.len();
    out.resize(start + samples * channels * 2, 0);
    let dst = &mut out[start..];
    let width = match format {
        Samples::U8 => 1,
        Samples::S16 => 2,
        Samples::S32 | Samples::F32 => 4,
        Samples::F64 => 8,
    };
    let convert = |s: &[u8]| -> i16 {
        match format {
            Samples::U8 => (i16::from(s[0]) - 128) << 8,
            Samples::S16 => i16::from_le_bytes([s[0], s[1]]),
            Samples::S32 => (i32::from_le_bytes([s[0], s[1], s[2], s[3]]) >> 16) as i16,
            Samples::F32 => f32_to_s16(f32::from_le_bytes([s[0], s[1], s[2], s[3]])),
            Samples::F64 => {
                let v = f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
                (v * 32768.0).round_ties_even().clamp(-32768.0, 32767.0) as i16
            }
        }
    };
    if planar {
        for ch in 0..channels {
            let plane = frame.audio_plane(ch);
            if plane.len() < samples * width {
                out.truncate(start);
                return false;
            }
            for (i, s) in plane.chunks_exact(width).take(samples).enumerate() {
                let at = (i * channels + ch) * 2;
                dst[at..at + 2].copy_from_slice(&convert(s).to_le_bytes());
            }
        }
    } else {
        let plane = frame.audio_plane(0);
        if plane.len() < samples * channels * width {
            out.truncate(start);
            return false;
        }
        for (d, s) in dst.as_chunks_mut::<2>().0.iter_mut().zip(plane.chunks_exact(width)) {
            *d = convert(s).to_le_bytes();
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_decoders_own_format_goes_out_where_taken_else_the_nearest_else_s16() {
        let set = |fs: &[SampleFormat]| fs.iter().fold(SampleFormats::NONE, |s, f| s.with(*f));
        let all = set(&SampleFormat::ALL);
        assert_eq!(out_sample(own_sample(Samples::F32), all), SampleFormat::F32);
        assert_eq!(out_sample(own_sample(Samples::F64), all), SampleFormat::F32);
        assert_eq!(out_sample(own_sample(Samples::S32), all), SampleFormat::S32);
        assert_eq!(
            out_sample(own_sample(Samples::S32), set(&[SampleFormat::S16, SampleFormat::F32])),
            SampleFormat::F32
        );
        assert_eq!(
            out_sample(own_sample(Samples::F32), set(&[SampleFormat::S16])),
            SampleFormat::S16
        );
        assert_eq!(
            out_sample(own_sample(Samples::F32), SampleFormats::NONE),
            SampleFormat::S16,
            "nothing said"
        );
    }

    #[test]
    fn floats_become_s16_as_libswresample_rounds_them() {
        assert_eq!(f32_to_s16(0.0), 0);
        assert_eq!(f32_to_s16(1.0), 32767, "clipped");
        assert_eq!(f32_to_s16(-1.0), -32768);
        assert_eq!(f32_to_s16(-2.0), -32768);
        assert_eq!(f32_to_s16(0.5 / 32768.0), 0);
        assert_eq!(f32_to_s16(1.5 / 32768.0), 2);
        assert_eq!(f32_to_s16(-1.5 / 32768.0), -2);
        assert_eq!(f32_to_s16(f32::NAN), 0);
    }

    #[test]
    fn an_id3v2_tag_is_measured_once_its_header_is_in() {
        assert_eq!(id3v2_len(b"ID"), None);
        assert_eq!(id3v2_len(b"\xFF\xFB"), Some(0), "an MPEG sync word");
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00"), None);
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00\x02\x01"), Some(10 + 257));
        assert_eq!(id3v2_len(b"ID3\x04\x00\x10\x00\x00\x00\x05"), Some(10 + 5 + 10), "footer");
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x80\x00\x00\x00"), Some(0), "not synchsafe");
    }

    // Test code.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn vorbis_headers_lace_and_split_back() {
        let ident = [b"\x01vorbis".as_slice(), &[0; 23]].concat();
        let setup = [b"\x05vorbis".as_slice(), &[7; 300]].concat();
        let mut laced = Vec::new();
        lace(&mut laced, &ident, &setup);
        assert_eq!(&laced[..3], &[2, 30, 16]);
        assert_eq!(
            split_laced(&laced),
            Some([ident.as_slice(), VORBIS_COMMENT.as_slice(), setup.as_slice()])
        );
        let long = [b"\x01vorbis".as_slice(), &[0; 293]].concat();
        lace(&mut laced, &long, &setup);
        assert_eq!(&laced[..4], &[2, 255, 45, 16]);
        assert_eq!(split_laced(&laced).map(|[i, _, _]| i.len()), Some(300));
    }

    #[allow(clippy::disallowed_methods)]
    #[test]
    fn a_laced_head_that_is_not_vorbis_or_runs_short_is_refused() {
        let ident = [b"\x01vorbis".as_slice(), &[0; 23]].concat();
        let setup = [b"\x05vorbis".as_slice(), &[7; 30]].concat();
        let mut laced = Vec::new();
        lace(&mut laced, &ident, &setup);
        for n in 0..laced.len() - setup.len() + 7 {
            assert_eq!(split_laced(&laced[..n]), None, "cut at {n}");
        }
        assert_eq!(split_laced(&[2, 0xFF, 0xFF, 0xFF]), None, "lacing past the end");
        assert_eq!(split_laced(&[3, 1, 1, 1, 1, 1]), None, "four packets");
        let mut swapped = Vec::new();
        lace(&mut swapped, &setup, &ident);
        assert_eq!(split_laced(&swapped), None, "the setup first");
        assert_eq!(vorbis_header(b"\x03vorbis"), Some(3));
        assert_eq!(vorbis_header(b"\x02vorbis"), None);
        assert_eq!(vorbis_header(b"\x01vorbi"), None);
    }

    #[test]
    fn a_skip_moves_the_stamp_past_it_and_zero_stays() {
        let ms = |n: u64| Timestamp(n * 1_000_000);
        let past = profluens_audio::time_at(44_100 + 1024, 44_100);
        assert_eq!(past_skip(Rebase::At(ms(1000)), 1024, 44_100, false), Rebase::Lost(past));
        assert_eq!(past_skip(Rebase::Lost(ms(1000)), 1024, 44_100, true), Rebase::Lost(past));
        assert_eq!(
            past_skip(Rebase::At(Timestamp::ZERO), 1024, 44_100, false),
            Rebase::At(Timestamp::ZERO)
        );
        assert_eq!(
            past_skip(Rebase::Lost(Timestamp::ZERO), 1024, 44_100, false),
            Rebase::At(Timestamp::ZERO)
        );
        let n = profluens_audio::time_at(128, 48_000);
        assert_eq!(past_skip(Rebase::At(Timestamp::ZERO), 128, 48_000, true), Rebase::At(n));
        assert_eq!(past_skip(Rebase::Open, 1024, 44_100, false), Rebase::Open);
        assert_eq!(past_skip(Rebase::Counting, 1024, 44_100, true), Rebase::Counting);
    }

    #[test]
    fn an_adts_sync_word_is_told_from_a_config() {
        assert!(is_adts(&[0xFF, 0xF1, 0x50]));
        assert!(!is_adts(&[0x12, 0x10]), "AAC LC 44.1 kHz stereo config");
        assert!(!is_adts(&[0xFF]));
    }
}
