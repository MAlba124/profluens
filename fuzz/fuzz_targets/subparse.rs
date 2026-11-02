//! Text subtitles as `subparse` reads them, a whole document, one Matroska Block or a
//! `subtitle/cue-ir` payload. Input is `mode`, `cut`, then the body. `mode % 17` is 0 autodetect,
//! 1-12 a `Format::ALL` entry, 13-15 an SRT, WebVTT or ASS Block, 16 a cue-ir payload.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods)]
// `fuzz_target!` opens libfuzzer-sys's debug output file.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use pf_sim::prng::Prng;
use pf_subparse::autodetect;
use pf_subparse::encoding::{Decoder, decode_all};
use pf_subparse::formats::ssa::framed_dialogue;
use pf_subparse::formats::webvtt::cue_text_to_pango_markup;
use pf_subparse::ir::{CUE_IR_FAMILY, CueBuilder, CueIr, EncodeError};
use pf_subparse::ssastyle::{SsaStyles, dialogue_into};
use pf_subparse::{CUE_SLOT, SubParse, document_ir};
use pf_subparse::{Cue, Format, ParseContext, ParseError, SubtitleFormat};
use profluens_core::batch::Batch;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::Bus;
use profluens_core::ctx::Ctx;
use profluens_core::element::Element;
use profluens_core::event::Event;
use profluens_core::format::Vocabulary;
use profluens_core::id::{ElementId, FormatId, Interner, SinkPad, SrcPad};
use profluens_core::memory::Pool;
use profluens_core::sched::{after_process, process_once};
use profluens_core::time::Timestamp;

const CEILINGS: [usize; 8] = [1, 2, 5, 16, 64, 512, 4096, usize::MAX];
const BIG: usize = 1 << 20;
const BLOCK_PTS: Timestamp = Timestamp::from_secs(1);
const BLOCK_DUR: Timestamp = Timestamp::from_secs(2);

fuzz_target!(|data: &[u8]| {
    let Some((&[mode, cut], body)) = data.split_first_chunk::<2>() else {
        return;
    };
    match mode % 17 {
        0 => document(body, None, cut),
        m @ 1..=12 => document(body, Format::ALL.get(usize::from(m) - 1).copied(), cut),
        m @ 13..=15 => block(body, m - 13, cut),
        _ => raw_wire(body),
    }
});

fn document(raw: &[u8], forced: Option<Format>, cut: u8) {
    let mut text = String::new();
    decode_all(raw, None, &mut text);
    let _ = autodetect::detect(&String::from_utf8_lossy(&raw[..raw.len().min(128)]));
    let Some(format) = forced.or_else(|| autodetect::detect_document(&text)) else {
        assert!(element_document(raw, cut).is_empty(), "an undetected document put out cues");
        return;
    };
    let ctx = ParseContext { fps: (cut & 8 != 0).then_some((24_000, 1_001)) };
    let mut parser = format.parser();
    let whole = parser.parse(&text, &ctx);
    assert!(whole == format.parser().parse(&text, &ctx), "{format:?} parse is not deterministic");
    let chunked = stream(format, &text, &ctx, cut);
    assert_eq!(
        whole.is_ok(),
        chunked.is_ok(),
        "{format:?} chunked and one shot disagree on refusal"
    );
    let (Ok(cues), Ok(chunked)) = (whole, chunked) else {
        return;
    };
    assert!(cues == chunked, "{format:?} chunked cues differ from one shot");
    if matches!(format, Format::SubRip | Format::WebVtt) {
        for c in &cues {
            assert!(
                c.end_ns.is_none_or(|e| e >= c.start_ns),
                "{format:?} cue ends before it starts"
            );
        }
    }
    let mut b = CueBuilder::new();
    let want: Vec<Out> = cues
        .iter()
        .filter_map(|cue| {
            document_ir(cue, &*parser, &mut b);
            let data = wire(&mut b)?;
            let duration = cue.duration_ns().map_or(Timestamp::NONE, Timestamp::from_nanos);
            Some(Out { pts: Timestamp::from_nanos(cue.start_ns), duration, data })
        })
        .collect();
    if say() {
        eprintln!("subparse {format:?} cues {} put out {}", cues.len(), want.len());
    }
    if forced.is_none() && ctx.fps.is_none() {
        assert!(
            element_document(raw, cut) == want,
            "the element put out other cues than the parser gave"
        );
    }
}

fn stream(format: Format, text: &str, ctx: &ParseContext, cut: u8) -> Result<Vec<Cue>, ParseError> {
    let ceiling = CEILINGS[usize::from(cut & 7)].min(text.len().max(1)) as u64;
    let mut rng = Prng::new(u64::from(cut >> 4) << 32 | text.len() as u64);
    let mut parser = format.parser();
    let (mut buf, mut cues, mut at) = (String::new(), Vec::new(), 0);
    loop {
        let mut end = (at + rng.range(1, ceiling + 1) as usize).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        let last = end == text.len();
        if !last && rng.below(8) == 0 {
            feed(&mut *parser, &mut buf, &mut cues, "", ctx, false)?;
        }
        feed(&mut *parser, &mut buf, &mut cues, &text[at..end], ctx, last)?;
        at = end;
        if last {
            return Ok(cues);
        }
    }
}

fn feed(
    parser: &mut (dyn SubtitleFormat + Send),
    buf: &mut String,
    cues: &mut Vec<Cue>,
    piece: &str,
    ctx: &ParseContext,
    eos: bool,
) -> Result<(), ParseError> {
    buf.push_str(piece);
    let p = parser.parse_incremental(buf, ctx, eos)?;
    assert!(p.consumed <= buf.len() && buf.is_char_boundary(p.consumed), "consumed off the body");
    assert!(!eos || p.consumed == buf.len(), "bytes left at eos");
    buf.drain(..p.consumed);
    cues.extend(p.cues);
    Ok(())
}

fn element_document(raw: &[u8], cut: u8) -> Vec<Out> {
    let mut rig = Rig::new(SubParse::new(), "bytes");
    let mut out = Vec::new();
    let mut rng = Prng::new(u64::from(cut));
    let ceiling = CEILINGS[usize::from(cut & 7)].min(raw.len().max(1)) as u64;
    let mut at = 0;
    while at < raw.len() {
        let n = (rng.range(1, ceiling + 1) as usize).min(raw.len() - at);
        assert!(rig.push(&raw[at..at + n], Timestamp::NONE, Timestamp::NONE, &mut out));
        at += n;
    }
    assert!(out.is_empty(), "a document put out cues before its end");
    rig.event(Event::Eos, &mut out);
    out
}

fn block(raw: &[u8], dialect: u8, cut: u8) {
    let ir = |b: &mut CueBuilder, body: &str, start_ns: u64| match dialect {
        0 => {
            b.srt_text(body.trim_end_matches(['\n', '\r']));
            true
        }
        1 => {
            b.pango_markup(&cue_text_to_pango_markup(body), None, None);
            true
        }
        _ => match framed_dialogue(body) {
            Some(d) => {
                dialogue_into(&d, &SsaStyles::default(), start_ns, b);
                true
            }
            None => false,
        },
    };
    let times = [BLOCK_PTS, Timestamp::from_secs(u64::from(cut))];
    let mut decoder = Decoder::new(None);
    let mut b = CueBuilder::new();
    let want: Vec<Out> = times
        .iter()
        .filter_map(|&pts| {
            let mut body = String::new();
            assert_eq!(decoder.decode(raw, true, &mut body), raw.len(), "a Block left bytes");
            if !ir(&mut b, &body, pts.0) {
                return None;
            }
            let data = wire(&mut b)?;
            assert!(ir(&mut b, &body, pts.0));
            let again = wire(&mut b);
            assert!(again.as_ref() == Some(&data), "a Block's IR is not deterministic");
            Some(Out { pts, duration: BLOCK_DUR, data })
        })
        .collect();
    let family = ["subtitle/srt", "subtitle/vtt", "subtitle/ass"][usize::from(dialect)];
    let mut rig = Rig::new(SubParse::new(), family);
    let mut out = Vec::new();
    for pts in times {
        assert!(rig.push(raw, pts, BLOCK_DUR, &mut out));
    }
    assert!(out == want, "a Block put out other than its IR");
    if say() {
        eprintln!("subparse {family} put out {}", out.len());
    }
}

/// A payload from outside, aligned the way a pool slot is.
fn raw_wire(p: &[u8]) {
    let mut aligned = vec![0u64; p.len().div_ceil(8)];
    let bytes = &mut bytemuck_free_cast(&mut aligned)[..p.len()];
    bytes.copy_from_slice(p);
    let view = CueIr::parse(bytes);
    if say() {
        eprintln!("subparse wire {:?}", view.as_ref().err());
    }
    let Ok(view) = view else {
        return;
    };
    touch(&view);
    let mut b = CueBuilder::new();
    b.copy_from(&view);
    let mut buf = vec![0xa5; BIG.max(p.len() * 4)];
    if let Ok(n) = b.finish(&mut buf) {
        let again = CueIr::parse(&buf[..n]).expect("a re-laid cue validates");
        assert!(again.same_as(&view), "a re-laid cue reads differently");
        payload(&mut b, &buf[..n]);
    }
}

/// `u64`s as bytes, 8 byte aligned without a dependency.
fn bytemuck_free_cast(words: &mut [u64]) -> &mut [u8] {
    let len = std::mem::size_of_val(words);
    // SAFETY: a `u64` slice is valid as bytes of the same length and the borrow is exclusive.
    unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), len) }
}

fn wire(b: &mut CueBuilder) -> Option<Vec<u8>> {
    let mut slot = vec![0xa5; CUE_SLOT];
    match b.finish(&mut slot) {
        Ok(n) => {
            assert!(n <= CUE_SLOT);
            assert_eq!(n, b.wire_len(), "a cue's stated length is not its written one");
            let p = &slot[..n];
            payload(b, p);
            let mut exact = vec![0x5a; n];
            assert_eq!(b.finish(&mut exact), Ok(n), "a cue does not fit its own length");
            assert!(exact == p, "a cue encodes to other bytes in another slice");
            let mut short = vec![0; n - 1];
            assert!(
                matches!(b.finish(&mut short), Err(EncodeError::DoesNotFit { .. })),
                "a cue fits a byte short of its length"
            );
            Some(p.to_vec())
        }
        Err(EncodeError::DoesNotFit { capacity }) => {
            assert_eq!(capacity, CUE_SLOT);
            let mut big = vec![0xa5; BIG];
            if let Ok(n) = b.finish(&mut big) {
                assert!(n > CUE_SLOT, "a cue refused by a slot it fits");
                payload(b, &big[..n]);
            }
            None
        }
        Err(EncodeError::TooMany) => None,
    }
}

/// `p` is `b`'s cue, it validates, reads back and re-lays out to a cue that reads the same.
fn payload(b: &mut CueBuilder, p: &[u8]) {
    let view = CueIr::parse(p).unwrap_or_else(|e| panic!("an encoded cue does not validate, {e}"));
    touch(&view);
    let mut again = CueBuilder::new();
    again.copy_from(&view);
    let bytes = again.to_vec();
    let back = CueIr::parse(&bytes).expect("a re-laid cue validates");
    assert!(back.same_as(&view), "the wire lost something\n{}\n{}", view.dump(), back.dump());
    again.copy_from(&back);
    assert!(again.to_vec() == bytes, "a re-laid cue is not byte stable");
    let _ = b;
}

fn touch(v: &CueIr<'_>) {
    let mut text = 0;
    for (i, line) in v.lines().iter().enumerate() {
        text += usize::from(i > 0);
        for s in v.line_spans(line) {
            text += v.span_text(s).len();
            let style = v.span_style(s);
            std::hint::black_box((v.span_voice(s), v.style_family(style), v.style_language(style)));
            std::hint::black_box(v.span_classes(s).count());
            if let Some(r) = v.span_ruby(s) {
                std::hint::black_box(v.ruby_text(r));
            }
        }
    }
    assert!(text <= v.text_len(), "plain text past the stated bound");
    for s in v.styles() {
        std::hint::black_box(s.canonical());
    }
    std::hint::black_box((v.layout().canonical(), v.base_style()));
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}

#[derive(Debug, PartialEq, Eq)]
struct Out {
    pts: Timestamp,
    duration: Timestamp,
    data: Vec<u8>,
}

struct Rig<E> {
    el: E,
    ctx: Ctx,
    vocab: Arc<Vocabulary>,
    pool: Pool,
    _bus: Bus,
}

impl<E: Element> Rig<E> {
    fn new(mut el: E, sink: &str) -> Self {
        let mut v = Vocabulary {
            formats: Interner::new(),
            fields: Interner::new(),
            values: Interner::new(),
        };
        for f in ["bytes", sink, CUE_IR_FAMILY] {
            v.formats.intern(f);
        }
        let fixed = v.build_fixed(sink, &[]).expect("an interned family");
        let vocab = Arc::new(v);
        let pool = Pool::new(64 << 10);
        let (tx, bus) = Bus::channel();
        let mut ctx = Ctx::new(pool.clone(), tx, ElementId(0), FormatId(0), 1);
        ctx.configure_pads(1, 1);
        ctx.set_vocabulary(Arc::clone(&vocab));
        ctx.set_negotiated_sink(SinkPad(0), fixed);
        el.start(&mut ctx).expect("start");
        after_process(&mut ctx, &vocab, |_, _, _, _| {});
        Self { el, ctx, vocab, pool, _bus: bus }
    }

    fn push(
        &mut self,
        data: &[u8],
        pts: Timestamp,
        duration: Timestamp,
        out: &mut Vec<Out>,
    ) -> bool {
        let mut memory = self.pool.acquire_exact(data.len().max(1)).expect("exact");
        memory.as_mut_full()[..data.len()].copy_from_slice(data);
        memory.set_len(data.len());
        let mut batch = Batch::new(FormatId(0));
        let flags = BufferFlags::empty();
        batch.push(Buffer {
            memory,
            pts,
            dts: Timestamp::NONE,
            duration,
            flags,
            format: FormatId(0),
            sync: None,
        });
        self.ctx.input_append(&mut batch);
        let flow = process_once(&mut self.el, &mut self.ctx, true);
        after_process(&mut self.ctx, &self.vocab, |_, _, _, _| {});
        self.drain(out);
        flow.is_ok()
    }

    fn event(&mut self, event: Event, out: &mut Vec<Out>) {
        self.el.event(&mut self.ctx, &event).expect("an event refused");
        self.drain(out);
    }

    fn drain(&mut self, out: &mut Vec<Out>) {
        let mut batch = self.ctx.take_output(SrcPad(0));
        while let Some(b) = batch.pop_front() {
            out.push(Out { pts: b.pts, duration: b.duration, data: b.memory.data().to_vec() });
        }
    }
}
