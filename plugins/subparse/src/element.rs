//! `subparse`, text subtitles in any form to one `subtitle/cue-ir` stream for the overlay.

use crate::autodetect;
use crate::encoding::{Charset, Decoder, decode_all};
use crate::formats::ssa::framed_dialogue;
use crate::formats::webvtt::{MarkupScratch, cue_text_to_pango_markup_into};
use crate::ir::{CUE_IR_FAMILY, CueBuilder, EncodeError};
use crate::ssastyle::{SsaStyles, dialogue_into};
use crate::{Cue, Format, ParseContext, ParseError, SubtitleFormat};
use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::PropDesc;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

/// The bytes a cue is encoded into. A 300-syllable karaoke line sharing one style fits.
pub const CUE_SLOT: usize = crate::ir::CUE_LIMIT;

/// The largest document taken.
pub const MAX_FILE: usize = 16 * 1024 * 1024;

/// Decoded bytes offered to the parser per step behind the end of a document, so a long file
/// never has every cue live at once.
const DOC_CHUNK: usize = 64 * 1024;

/// Cues a document hands out per pass before yielding to the downstream.
const DOC_CUES_PER_PASS: usize = 64;

static SINK_OFFERS: [OfferDesc; 4] = [
    OfferDesc::any("subtitle/srt"),
    OfferDesc::any("subtitle/vtt"),
    OfferDesc::any("subtitle/ass"),
    OfferDesc::any("bytes"),
];
static SRC_OFFERS: [OfferDesc; 2] = [OfferDesc::any(CUE_IR_FAMILY), OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

/// `charset`, a [`Charset`] label or `auto`. A byte-order mark still wins.
static PROPS: [PropDesc; 1] = [PropDesc { name: "charset", allowed: Constraint::Any, live: false }];

static PRESETS: [[(&str, ValueDesc); 1]; 6] = [
    [("charset", ValueDesc::Id(Charset::Utf8.label()))],
    [("charset", ValueDesc::Id(Charset::Utf16Le.label()))],
    [("charset", ValueDesc::Id(Charset::Utf16Be.label()))],
    [("charset", ValueDesc::Id(Charset::Utf32Le.label()))],
    [("charset", ValueDesc::Id(Charset::Utf32Be.label()))],
    [("charset", ValueDesc::Id(Charset::Windows1252.label()))],
];

pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "subparse",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &PROPS,
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Cold, registry make_default, boxes one element instance at plugin-registration time.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(SubParse::new())),
    io: false,
    couplings: &[],
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Srt,
    Vtt,
    Ass,
    Document,
}

impl Mode {
    fn from_family(name: &str) -> Mode {
        match name {
            "subtitle/srt" => Mode::Srt,
            "subtitle/vtt" => Mode::Vtt,
            "subtitle/ass" => Mode::Ass,
            _ => Mode::Document,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Srt => "srt",
            Mode::Vtt => "vtt",
            Mode::Ass => "ass",
            Mode::Document => "document",
        }
    }
}

fn format_name(f: Format) -> &'static str {
    match f {
        Format::SubRip => "subrip",
        Format::MicroDvd => "microdvd",
        Format::Mpl2 => "mpl2",
        Format::SubViewer => "subviewer",
        Format::Sami => "sami",
        Format::TmPlayer => "tmplayer",
        Format::MpSub => "mpsub",
        Format::QtText => "qttext",
        Format::Lrc => "lrc",
        Format::Dks => "dks",
        Format::WebVtt => "webvtt",
        Format::Ssa => "ssa",
    }
}

/// What `subparse` says on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubParseError {
    CueTooLarge { pts: Timestamp, capacity: usize },
    TooLarge { max: usize },
    UnknownFormat { bytes: usize },
    UnknownCharset,
    Parse(ParseError),
}

impl std::fmt::Display for SubParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubParseError::CueTooLarge { pts, capacity } => {
                write!(f, "cue at {pts:?} does not fit a {capacity} byte slot, dropped")
            }
            SubParseError::TooLarge { max } => write!(f, "a subtitle file past {max} bytes"),
            SubParseError::UnknownFormat { bytes } => {
                write!(f, "no subtitle format matched a {bytes} byte document")
            }
            SubParseError::UnknownCharset => f.write_str("charset label not known, sniffing"),
            SubParseError::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SubParseError {}

/// A document behind its end, parsed a chunk at a time and handed out cue by cue under the
/// pool's backpressure. Kept across a flush and walked again from its start, so a seek parses
/// no file again.
struct Document {
    text: String,
    /// Bytes of `text` the parser consumed.
    pos: usize,
    /// Bytes past `pos` offered before and left unconsumed, a record straddling the chunk.
    window: usize,
    format: Format,
    parser: Box<dyn SubtitleFormat + Send>,
    /// The last step's cues, handed out from `next`.
    cues: Vec<Cue>,
    next: usize,
    emitted: usize,
}

// Cold, once a document, behind its end.
#[allow(clippy::disallowed_methods)]
impl Document {
    /// Parse the next chunk into `cues`, growing the offer while a record straddles it.
    fn step(&mut self) -> Result<(), ParseError> {
        self.cues.clear();
        self.next = 0;
        loop {
            let len = self.text.len();
            let mut end = self.pos.saturating_add(self.window).saturating_add(DOC_CHUNK).min(len);
            while !self.text.is_char_boundary(end) {
                end += 1;
            }
            let at_eos = end == len;
            let body = &self.text[self.pos..end];
            let parsed = self.parser.parse_incremental(body, &ParseContext::default(), at_eos)?;
            self.pos = if at_eos { len } else { self.pos + parsed.consumed.min(body.len()) };
            self.window = end - self.pos;
            self.cues = parsed.cues;
            if !self.cues.is_empty() || at_eos {
                return Ok(());
            }
        }
    }

    fn done(&self) -> bool {
        self.next >= self.cues.len() && self.pos >= self.text.len()
    }

    /// Back to the start, a fresh parser as one keeps a header's state.
    fn rewind(&mut self) {
        (self.pos, self.window, self.next, self.emitted) = (0, 0, 0, 0);
        self.cues.clear();
        self.parser = self.format.parser();
    }
}

/// Text subtitles in any form to `subtitle/cue-ir`.
pub struct SubParse {
    mode: Mode,
    text: Vec<u8>,
    /// The document being handed out behind its end.
    doc: Option<Document>,
    charset: Option<Charset>,
    decoder: Decoder,
    /// A Block's text, reused across Blocks.
    block: String,
    /// The cue under construction, its tables reused across cues.
    builder: CueBuilder,
    /// The VTT Block's markup pipeline buffers, reused across cues.
    markup: MarkupScratch,
    /// An ASS Block is styled by its tags alone, the container forwards no header.
    ass_styles: SsaStyles,
    /// Cues dropped because they did not fit [`CUE_SLOT`] (health counter).
    pub dropped: u64,
    /// ASS Blocks with fewer than the nine fields of an event row (health counter).
    pub malformed: u64,
    warned_malformed: bool,
}

impl Default for SubParse {
    fn default() -> Self {
        Self::new()
    }
}

impl SubParse {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    // Cold, one-time constructor, the empty document (zero capacity, no heap yet).
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self::with_charset(None)
    }

    /// Decode as `charset` unless a byte-order mark says otherwise, `None` sniffs.
    // Cold, one-time constructor, the empty document (zero capacity, no heap yet).
    #[allow(clippy::disallowed_methods)]
    pub fn with_charset(charset: Option<Charset>) -> Self {
        Self {
            mode: Mode::Document,
            text: Vec::new(),
            doc: None,
            charset,
            decoder: Decoder::new(charset),
            block: String::new(),
            builder: CueBuilder::new(),
            markup: MarkupScratch::default(),
            ass_styles: SsaStyles::default(),
            dropped: 0,
            malformed: 0,
            warned_malformed: false,
        }
    }

    fn learn_from_sink(&mut self, ctx: &Ctx) {
        if let Some(fixed) = ctx.negotiated_sink(Self::IN)
            && let Some(name) = ctx.family_name(fixed.family)
        {
            let mode = Mode::from_family(name);
            if mode != self.mode {
                log!(ctx, Level::Debug, "mode", mode = mode.name(), was = self.mode.name());
            }
            self.mode = mode;
        }
    }

    /// Build one Block's cue into the builder, `false` for an ASS row with too few fields.
    fn block_ir(&mut self, body: &str, pts: Timestamp) -> bool {
        match self.mode {
            Mode::Srt => self.builder.srt_text(body.trim_end_matches(['\n', '\r'])),
            Mode::Vtt => {
                let markup = cue_text_to_pango_markup_into(body, &mut self.markup);
                self.builder.pango_markup(markup, None, None);
            }
            Mode::Ass => {
                let Some(d) = framed_dialogue(body) else { return false };
                dialogue_into(&d, &self.ass_styles, pts.nanos().unwrap_or(0), &mut self.builder);
            }
            Mode::Document => self.builder.plain_text(body),
        }
        true
    }

    /// Write the builder's cue into a pool slot and push it.
    fn emit(&mut self, ctx: &mut Ctx, pts: Timestamp, dts: Timestamp, duration: Timestamp) {
        let Some(out) = ctx.alloc_exact(CUE_SLOT) else {
            self.dropped += 1;
            let error = ctx.exact_refused(CUE_SLOT);
            ctx.post(BusMessage::Warning { element: ctx.element(), error });
            return;
        };
        self.emit_into(ctx, out, pts, dts, duration);
    }

    /// Write the builder's cue into `out`, at least [`CUE_SLOT`] bytes, and push it.
    fn emit_into(
        &mut self,
        ctx: &mut Ctx,
        mut out: Buffer,
        pts: Timestamp,
        dts: Timestamp,
        duration: Timestamp,
    ) {
        let written = self.builder.finish(&mut out.memory.as_mut_full()[..CUE_SLOT]);
        match written {
            Ok(n) => {
                out.memory.set_len(n);
                out.pts = pts;
                out.dts = dts;
                out.duration = duration;
                ctx.out(Self::OUT).push(out);
            }
            Err(e) => {
                self.dropped += 1;
                let capacity = match e {
                    EncodeError::DoesNotFit { capacity } => capacity,
                    EncodeError::TooMany => CUE_SLOT,
                };
                let error = Error::element_failed(
                    ctx.element(),
                    SubParseError::CueTooLarge { pts, capacity },
                );
                ctx.post(BusMessage::Warning { element: ctx.element(), error });
            }
        }
    }

    fn warn(&self, ctx: &mut Ctx, e: SubParseError) {
        let error = Error::element_failed(ctx.element(), e);
        ctx.post(BusMessage::Warning { element: ctx.element(), error });
    }

    // Cold, once a document, at its end.
    #[allow(clippy::disallowed_methods)]
    fn put_out(&mut self, ctx: &mut Ctx) {
        let mut text = String::with_capacity(self.text.len());
        decode_all(&self.text, self.charset, &mut text);
        self.text.clear();
        let Some(format) = autodetect::detect_document(&text) else {
            self.warn(ctx, SubParseError::UnknownFormat { bytes: text.len() });
            self.end_document(ctx, 0);
            return;
        };
        log!(&*ctx, Level::Info, "document", format = format_name(format), bytes = text.len());
        self.doc = Some(Document {
            text,
            pos: 0,
            window: 0,
            format,
            parser: format.parser(),
            cues: Vec::new(),
            next: 0,
            emitted: 0,
        });
        self.drain_document(ctx);
    }

    /// Hand out the document's next cues, holding the end while any remain. A full pool or the
    /// per-pass count yields, the scheduler returns here once the downstream drained. Behind a
    /// seek the cues wholly before the presentation point are passed over.
    fn drain_document(&mut self, ctx: &mut Ctx) {
        let Some(mut doc) = self.doc.take() else { return };
        let mut passed = 0usize;
        while !doc.done() {
            if doc.next >= doc.cues.len() {
                if let Err(e) = doc.step() {
                    self.warn(ctx, SubParseError::Parse(e));
                    break;
                }
                continue;
            }
            let (pts, duration) = {
                let cue = &doc.cues[doc.next];
                let pts = Timestamp::from_nanos(cue.start_ns);
                (pts, cue.duration_ns().map_or(Timestamp::NONE, Timestamp::from_nanos))
            };
            // A saturated start would go out untimed and show at once or forever.
            if pts.is_none() {
                self.dropped += 1;
                doc.next += 1;
                continue;
            }
            if ctx.before_presentation(pts, duration) {
                doc.next += 1;
                continue;
            }
            let out = match ctx.try_alloc_sized(CUE_SLOT) {
                Ok(Some(out)) if passed < DOC_CUES_PER_PASS => out,
                // Backpressure, the pool wakes this group when a buffer returns.
                Ok(_) => {
                    self.doc = Some(doc);
                    ctx.hold_end();
                    return;
                }
                // A pool too small for a cue slot, the cold path's own buffer.
                Err(_) => match ctx.alloc_exact(CUE_SLOT) {
                    Some(out) => out,
                    None => {
                        self.dropped += 1;
                        doc.next += 1;
                        continue;
                    }
                },
            };
            let cue = &doc.cues[doc.next];
            doc.next += 1;
            passed += 1;
            document_ir(cue, &*doc.parser, &mut self.builder);
            self.emit_into(ctx, out, pts, Timestamp::NONE, duration);
            doc.emitted += 1;
        }
        self.end_document(ctx, doc.emitted);
    }

    fn end_document(&mut self, ctx: &mut Ctx, emitted: usize) {
        // A document with no cue has nothing to the end.
        if emitted == 0 {
            ctx.push_event(Self::OUT, Event::Gap { until: Timestamp::MAX });
        }
        log!(&*ctx, Level::Info, "eos", cues = emitted, dropped = self.dropped);
    }
}

/// Build one document cue into `b`.
pub fn document_ir(cue: &Cue, parser: &(dyn SubtitleFormat + Send), b: &mut CueBuilder) {
    match (&cue.ssa, parser.ssa_styles()) {
        (Some(d), Some(styles)) => dialogue_into(&d.event(), styles, cue.start_ns, b),
        _ => b.cue(cue, parser.output_format(), parser.stylesheet()),
    }
}

impl Element for SubParse {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn presets(&self) -> &[(&'static str, ValueDesc)] {
        match self.charset {
            Some(c) => &PRESETS[Charset::ALL.iter().position(|&a| a == c).unwrap_or(0)],
            None => &[],
        }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Id(id)) = ctx.prop("charset") {
            let label = ctx.value_name(id).unwrap_or_default();
            let auto = label.eq_ignore_ascii_case("auto");
            match Charset::from_label(label) {
                Some(c) => self.charset = Some(c),
                None if auto => self.charset = None,
                None => self.warn(ctx, SubParseError::UnknownCharset),
            }
        }
        self.decoder = Decoder::new(self.charset);
        ctx.announce_format(Self::OUT, CUE_IR_FAMILY, &[]);
        self.learn_from_sink(ctx);
        let charset = self.charset.map_or("auto", Charset::label);
        log!(&*ctx, Level::Info, "start", mode = self.mode.name(), charset = charset);
        self.text.clear();
        self.doc = None;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.learn_from_sink(ctx);
        if self.doc.is_some() {
            self.drain_document(ctx);
        }
        while let Some(buf) = inputs.pop() {
            if self.mode == Mode::Document {
                // The source reading the file again behind a flush feeds the document held.
                if self.doc.is_some() {
                    continue;
                }
                if self.text.len() + buf.memory.len() > MAX_FILE {
                    return Err(Error::element_failed(
                        ctx.element(),
                        SubParseError::TooLarge { max: MAX_FILE },
                    ));
                }
                self.text.extend_from_slice(buf.memory.data());
                continue;
            }
            let mut body = std::mem::take(&mut self.block);
            body.clear();
            self.decoder.decode(buf.memory.data(), true, &mut body);
            let built = self.block_ir(&body, buf.pts);
            self.block = body;
            match built {
                true => self.emit(ctx, buf.pts, buf.dts, buf.duration),
                false => {
                    self.malformed += 1;
                    if !self.warned_malformed {
                        self.warned_malformed = true;
                        log!(&*ctx, Level::Warn, "ass_block_malformed", fields = 9u64);
                    }
                }
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(_) => self.learn_from_sink(ctx),
            Event::Eos if self.mode == Mode::Document && self.doc.is_none() => self.put_out(ctx),
            Event::Eos if self.mode == Mode::Document => self.drain_document(ctx),
            Event::Eos => log!(
                &*ctx,
                Level::Info,
                "eos",
                mode = self.mode.name(),
                dropped = self.dropped,
                malformed = self.malformed,
            ),
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", buffered = self.text.len());
                self.text.clear();
                if let Some(doc) = self.doc.as_mut() {
                    doc.rewind();
                }
            }
            // A cue comes out as it goes in, so the track's silence is this pad's.
            Event::Gap { .. } => ctx.push_event(Self::OUT, event.clone()),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::ir::{CueIr, FontStyle};
    use pf_harness::Harness;
    use profluens_core::buffer::Buffer;

    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,500\nfirst\n\n2\n00:00:04,000 --> 00:00:05,000\n<i>second</i>\n";

    fn ir_of(buf: &Buffer) -> CueIr<'_> {
        CueIr::parse(buf.memory.data()).expect("a valid cue-ir payload")
    }

    fn plain(buf: &Buffer) -> String {
        ir_of(buf).plain_text()
    }

    #[test]
    fn the_output_family_is_said_once_from_the_start() {
        let mut h = Harness::new(SubParse::new());
        h.crank().expect("a pass with no input");
        let said = h.announced().expect("the family");
        assert_eq!(h.vocabulary().family_name(said.family), Some(CUE_IR_FAMILY));
        let cue = h.alloc(b"<i>hi</i>");
        h.push(cue).expect("a cue");
        h.push_event(Event::FlushStart).expect("flush");
        h.crank().expect("a pass behind the flush");
        assert_eq!(h.format_changes(), 1, "a downstream is told once");
    }

    #[test]
    fn mode_from_family_maps_the_three_text_codecs() {
        assert_eq!(Mode::from_family("subtitle/srt"), Mode::Srt);
        assert_eq!(Mode::from_family("subtitle/vtt"), Mode::Vtt);
        assert_eq!(Mode::from_family("subtitle/ass"), Mode::Ass);
        assert_eq!(Mode::from_family("bytes"), Mode::Document);
        assert_eq!(Mode::from_family("subtitle/pgs"), Mode::Document);
    }

    #[test]
    fn each_block_dialect_becomes_a_styled_cue_with_its_timing() {
        for (family, body, text) in [
            ("subtitle/srt", "<i>hi</i> there", "hi there"),
            ("subtitle/vtt", "<c>hi</c> <i>there</i>", "hi there"),
            ("subtitle/ass", "0,0,Default,,0,0,0,,{\\i1}hi{\\i0} there", "hi there"),
        ] {
            let mut h = Harness::new(SubParse::new());
            h.fix_sink_format(SubParse::IN, family, &[]);
            let mut cue = h.alloc(body.as_bytes());
            cue.pts = Timestamp::from_millis(1_000);
            cue.duration = Timestamp::from_millis(2_000);
            h.push(cue).expect("a cue");
            let out = h.drain_outputs();
            assert_eq!(out.len(), 1, "{family}");
            assert_eq!(
                (out[0].pts, out[0].duration),
                (Timestamp::from_millis(1_000), Timestamp::from_millis(2_000))
            );
            let ir = ir_of(&out[0]);
            assert_eq!(ir.plain_text(), text, "{family}");
            let italic = ir
                .line_spans(&ir.lines()[0])
                .iter()
                .any(|s| ir.span_style(s).font_style() == Some(FontStyle::Italic));
            assert!(italic, "{family} keeps the italic span");
        }
    }

    #[test]
    fn an_ass_row_with_too_few_fields_is_counted_not_shown() {
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, "subtitle/ass", &[]);
        let cue = h.alloc(b"0,0,Default,,text with five fields");
        h.push(cue).expect("a buffer");
        assert!(h.drain_outputs().is_empty());
    }

    #[test]
    fn a_document_comes_out_as_its_cues_at_eos_and_again_behind_a_flush() {
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, "bytes", &[]);
        let (a, b) = SRT.as_bytes().split_at(30);
        for piece in [a, b] {
            let buf = h.alloc(piece);
            h.push(buf).expect("push");
            assert!(h.drain_outputs().is_empty(), "nothing before the end");
        }
        let out = h.eos().expect("eos");
        let cues: Vec<_> = out.iter().map(|b| (b.pts, b.duration, plain(b))).collect();
        let ms = Timestamp::from_millis;
        assert_eq!(
            cues,
            [(ms(1_000), ms(1_500), "first".into()), (ms(4_000), ms(1_000), "second".into())]
        );
        h.push_event(Event::FlushStart).expect("flush");
        let buf = h.alloc(SRT.as_bytes());
        h.push(buf).expect("push");
        assert_eq!(h.eos().expect("eos").len(), 2);
    }

    #[test]
    fn a_seek_walks_the_document_again_from_the_cue_up_at_the_point() {
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, "bytes", &[]);
        let buf = h.alloc(SRT.as_bytes());
        h.push(buf).expect("push");
        assert_eq!(h.eos().expect("eos").len(), 2);
        let ms = Timestamp::from_millis;
        for (point, want) in [(1_500, vec!["first", "second"]), (4_500, vec!["second"])] {
            h.seek_to(ms(point), None).expect("seek");
            // the source reads the file again, the bytes are not a second document
            let again = h.alloc(SRT.as_bytes());
            h.push(again).expect("push");
            let out = h.eos().expect("eos");
            let got: Vec<String> = out.iter().map(plain).collect();
            assert_eq!(got, want, "at {point} ms");
            if point == 1_500 {
                assert_eq!(out[0].pts, ms(1_000), "the cue up at the point keeps its start");
            }
        }
    }

    #[test]
    fn a_long_document_comes_out_under_the_pools_backpressure() {
        // A hundred slots, the first pass's buffers stay live in the harness, so every later
        // pass starves the pool part way and holds the end instead of materialising every cue.
        const CUES: usize = 5_000;
        let mut h = Harness::with_pool(SubParse::new(), 64 * 1024, 100);
        h.fix_sink_format(SubParse::IN, "bytes", &[]);
        let mut doc = String::new();
        for i in 0..CUES {
            doc.push_str(&format!("{{{}}}{{{}}}cue {i}\n", i * 10 + 1, i * 10 + 5));
        }
        let buf = h.alloc(doc.as_bytes());
        h.push(buf).expect("push");
        let out = h.eos().expect("eos");
        assert_eq!(out.len(), CUES);
        for (i, b) in out.iter().enumerate() {
            assert_eq!(plain(b), format!("cue {i}"));
            assert!(b.pts.is_some() && b.pts < out.get(i + 1).map_or(Timestamp::MAX, |n| n.pts));
        }
    }

    #[test]
    fn a_cue_whose_start_saturated_is_dropped_not_untimed() {
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, "bytes", &[]);
        let doc = "{1}{2}first\n{99999999999999999999}{99999999999999999999}never\n{3}{4}last\n";
        let buf = h.alloc(doc.as_bytes());
        h.push(buf).expect("push");
        let out = h.eos().expect("eos");
        let texts: Vec<_> = out.iter().map(plain).collect();
        assert_eq!(texts, ["first", "last"]);
        assert!(out.iter().all(|b| b.pts.is_some()));
    }

    #[test]
    fn every_document_format_is_detected_and_parsed() {
        let docs: [(&str, &str, &str); 6] = [
            ("srt", SRT, "first"),
            (
                "vtt",
                "WEBVTT\n\n00:01.000 --> 00:03.000\n<v Bob>hi</v> <c.yellow>there</c>\n",
                "hi there",
            ),
            (
                "ass",
                "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize\n\
                 Style: Default,Arial,20\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                 Dialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,{\\i1}hello{\\i0}\\Nsecond\n",
                "hello\nsecond",
            ),
            ("microdvd", "{10}{40}hello|world\n", "hello\nworld"),
            ("mpl2", "[10][40]hello|world\n", "hello\nworld"),
            ("lrc", "[00:12.00]hello\n[00:15.00]world\n", "hello"),
        ];
        for (name, doc, first) in docs {
            let mut h = Harness::new(SubParse::new());
            h.fix_sink_format(SubParse::IN, "bytes", &[]);
            let buf = h.alloc(doc.as_bytes());
            h.push(buf).expect("push");
            let out = h.eos().expect("eos");
            assert!(!out.is_empty(), "{name}: cues");
            assert_eq!(plain(&out[0]), first, "{name}");
            assert!(out[0].pts.is_some(), "{name}: stamped");
        }
    }

    #[test]
    fn a_named_charset_is_preset_and_each_document_is_decoded_afresh() {
        assert!(SubParse::new().presets().is_empty());
        for c in Charset::ALL {
            let el = SubParse::with_charset(Some(c));
            let [(name, ValueDesc::Id(label))] = el.presets() else { panic!("one id preset") };
            assert_eq!((*name, Charset::from_label(label)), ("charset", Some(c)));
        }
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, "bytes", &[]);
        let legacy = b"1\n00:00:01,000 --> 00:00:02,000\ncaf\xe9 cr\xe8me\n";
        let utf8 = "1\n00:00:01,000 --> 00:00:02,000\ncaf\u{e9} cr\u{e8}me\n";
        for doc in [&legacy[..], utf8.as_bytes(), &legacy[..]] {
            let buf = h.alloc(doc);
            h.push(buf).expect("push");
            let out = h.eos().expect("eos");
            assert_eq!(plain(&out[0]), "caf\u{e9} cr\u{e8}me");
            h.push_event(Event::FlushStart).expect("flush");
        }
    }

    #[test]
    fn a_document_no_format_matches_is_a_warning_and_a_gap() {
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, "bytes", &[]);
        let buf = h.alloc(b"just some prose, not a subtitle file\n");
        h.push(buf).expect("push");
        assert!(h.eos().expect("eos").is_empty());
        let warned = h.bus_messages().iter().any(|m| matches!(m, BusMessage::Warning { .. }));
        assert!(warned, "the bus hears why nothing shows");
    }

    #[test]
    fn a_cue_past_the_slot_is_a_typed_warning_and_dropped_not_a_stall() {
        let mut h = Harness::new(SubParse::new());
        h.fix_sink_format(SubParse::IN, "subtitle/ass", &[]);
        let mut row = String::from("0,0,Default,,0,0,0,,");
        for i in 0..400u32 {
            row.push_str(&format!("{{\\k10\\c&H{:06X}&}}la", i * 7919));
        }
        let mut cue = h.alloc(row.as_bytes());
        cue.pts = Timestamp::from_millis(0);
        h.push(cue).expect("a buffer");
        assert!(h.drain_outputs().is_empty(), "nothing truncated goes out");
        let warned = h.bus_messages().iter().any(|m| match m {
            BusMessage::Warning { error: Error::ElementFailed { source, .. }, .. } => {
                source.to_string().contains("does not fit")
            }
            _ => false,
        });
        assert!(warned, "a typed per-cue warning");
        let mut cue = h.alloc(b"0,0,Default,,0,0,0,,next");
        cue.pts = Timestamp::from_millis(1_000);
        h.push(cue).expect("a buffer");
        assert_eq!(h.drain_outputs().len(), 1);
    }

    #[test]
    fn the_slot_holds_a_long_karaoke_line_and_the_longest_test_cue() {
        let mut row = String::from("0,0,Default,,0,0,0,,");
        for _ in 0..300 {
            row.push_str("{\\k10}la");
        }
        let d = framed_dialogue(&row).expect("a row");
        let mut b = CueBuilder::new();
        dialogue_into(&d, &SsaStyles::default(), 0, &mut b);
        let mut slot = vec![0u8; CUE_SLOT];
        let n = b.finish(&mut slot).expect("300 syllables fit");
        assert!(n < CUE_SLOT, "{n} of {CUE_SLOT}");
        let ir = CueIr::parse(&slot[..n]).expect("valid");
        assert_eq!(ir.line_spans(&ir.lines()[0]).len(), 300);
        let longest = "x".repeat(342);
        b.srt_text(&longest);
        let n = b.finish(&mut slot).expect("fits");
        assert!(n < 1024, "the longest test cue is small on the wire: {n}");
    }
}
