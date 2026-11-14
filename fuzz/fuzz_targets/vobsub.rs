//! VobSub sub-picture units through the `vobsubdec` element. Input is `knob` then `u16` LE
//! length prefixed records, 3 s apart. `knob & 1` makes the first record the `.idx` head,
//! `knob & 2` gives every Block a 2 s duration. Every picture lies on its grid, so a few bytes
//! never stand for more than the grid's pixels.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods)]
// `fuzz_target!` opens libfuzzer-sys's debug output file.
#![allow(clippy::disallowed_types)]

use std::hash::{DefaultHasher, Hasher};
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use pf_text::bitmap::{BITMAP_HEADER_LEN, BITMAP_RECT_LEN, BitmapRef};
use pf_text::pgs::decode_bitmap;
use pf_text::vobsubdec::MAX_BITMAP_BYTES;
use pf_text::{BITMAP_FAMILY, VOBSUB_FAMILY, VobSubDec};
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

const MAX_UNITS: usize = 64;
const STEP: u64 = 3;
const DURATION: Timestamp = Timestamp::from_secs(2);

type Unit<'a> = (&'a [u8], Timestamp);

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let mut recs: Vec<&[u8]> = records(rest).take(MAX_UNITS + 1).collect();
    let head = (knob & 1 != 0 && recs.first().is_some_and(|h| idx_like(h))).then(|| recs.remove(0));
    let duration = if knob & 2 != 0 { DURATION } else { Timestamp::NONE };
    let units: Vec<Unit<'_>> = recs
        .iter()
        .enumerate()
        .map(|(i, u)| (*u, Timestamp::from_secs(STEP * (i as u64 + 1))))
        .collect();
    let stray = units.iter().any(|u| idx_like(u.0));
    let tinted = head.is_some() || stray;

    let check = |o: &Out, px: &[[u8; 4]]| bitmap(o, px, tinted);
    let mut rig = Rig::new(VobSubDec::new(), VOBSUB_FAMILY, check);
    play(&mut rig, head, &units, duration);
    let once = std::mem::take(&mut rig.out);
    assert!(once.len() <= units.len(), "more bitmaps than units");
    let mut again = Rig::new(VobSubDec::new(), VOBSUB_FAMILY, check);
    play(&mut again, head, &units, duration);
    assert!(once == again.out, "a run is not deterministic");
    if say() {
        eprintln!(
            "vobsub head {} units {} put out {} stray {stray}",
            head.is_some(),
            units.len(),
            once.len()
        );
    }
    if stray {
        return;
    }
    let mut alone = Vec::with_capacity(once.len());
    for u in &units {
        let mut one = Rig::new(VobSubDec::new(), VOBSUB_FAMILY, check);
        play(&mut one, head, &[*u], duration);
        alone.append(&mut one.out);
    }
    assert!(once == alone, "a unit decodes other than alone");
    rig.event(Event::FlushStart);
    play(&mut rig, head, &units, duration);
    assert!(once == rig.out, "a flush did not start the stream over");
});

fn play<F: FnMut(&Out, &[[u8; 4]])>(
    rig: &mut Rig<VobSubDec, F>,
    head: Option<&[u8]>,
    units: &[Unit<'_>],
    d: Timestamp,
) {
    if let Some(h) = head {
        rig.push(h, Timestamp::NONE, Timestamp::NONE);
    }
    for &(u, pts) in units {
        rig.push(u, pts, d);
    }
    rig.event(Event::Eos);
}

fn idx_like(data: &[u8]) -> bool {
    std::str::from_utf8(data).is_ok_and(|t| {
        t.lines().map(str::trim_start).any(|l| l.starts_with("palette:") || l.starts_with("size:"))
    })
}

fn bitmap(o: &Out, px: &[[u8; 4]], tinted: bool) {
    let [w, h, x, y, gw, gh] = o.head;
    assert!(w >= 1 && h >= 1, "an empty picture put out");
    assert!(px.len() * 4 <= MAX_BITMAP_BYTES, "a picture past the budget");
    assert!((1..=4096).contains(&gw) && (1..=4096).contains(&gh), "a grid out of range");
    assert!(x + w <= gw && y + h <= gh, "a picture past its grid");
    if !tinted {
        assert_eq!((gw, gh), (720, if y + h > 480 { 576 } else { 480 }), "not the default grid");
    }
    let mut colours = [[0u8; 4]; 4];
    let mut n = 0;
    for p in px {
        assert!(p[3] % 17 == 0, "an alpha not widened from a nibble");
        if !tinted {
            assert!(p[0] == p[1] && p[1] == p[2], "a colour without a palette");
            assert!(p[3] != 0 || *p == [0; 4], "a transparent pixel with colour");
        }
        if !colours[..n].contains(p) {
            assert!(n < 4, "more than four colours");
            colours[n] = *p;
            n += 1;
        }
    }
    assert!(o.pts >= Timestamp::from_secs(STEP) && o.pts.is_some(), "a bitmap before its Block");
    assert!(o.duration.nanos() != Some(0), "a zero long bitmap");
}

fn records(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (len, rest) = match data.split_first_chunk::<2>() {
            Some((len, rest)) => (usize::from(u16::from_le_bytes(*len)), rest),
            None if data.is_empty() => return None,
            None => (data.len(), std::mem::take(&mut data)),
        };
        let (rec, tail) = rest.split_at(len.min(rest.len()));
        data = tail;
        Some(rec)
    })
}

fn say() -> bool {
    static SAY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SAY.get_or_init(|| std::env::var_os("PF_FUZZ_SAY").is_some())
}

#[derive(Debug, PartialEq, Eq)]
struct Out {
    pts: Timestamp,
    duration: Timestamp,
    head: [u32; 6],
    hash: u64,
}

impl Out {
    fn of(pts: Timestamp, duration: Timestamp, p: &[u8]) -> Out {
        let image = BitmapRef::parse(p).expect("a bitmap payload the view refuses");
        let (w, h, x, y) = match image.rects() {
            [] => (0, 0, 0, 0),
            [r] => (r.width, r.height, r.x, r.y),
            _ => panic!("a decoder put out more than one rect"),
        };
        let head = [w, h, x, y, image.video_width(), image.video_height()];
        let px = w as usize * h as usize;
        let table = BITMAP_HEADER_LEN + image.rects().len() * BITMAP_RECT_LEN;
        assert_eq!(p.len() - table, px * 4, "RGBA other than width * height");
        if p.len() <= 1 << 16 {
            assert!(decode_bitmap(p).is_some(), "a well formed bitmap payload refused");
        }
        let mut h = DefaultHasher::new();
        h.write(p);
        Out { pts, duration, head, hash: h.finish() }
    }
}

struct Rig<E, F> {
    el: E,
    ctx: Ctx,
    vocab: Arc<Vocabulary>,
    pool: Pool,
    _bus: Bus,
    check: F,
    out: Vec<Out>,
}

impl<E: Element, F: FnMut(&Out, &[[u8; 4]])> Rig<E, F> {
    fn new(mut el: E, sink: &str, check: F) -> Self {
        let mut v = Vocabulary {
            formats: Interner::new(),
            fields: Interner::new(),
            values: Interner::new(),
        };
        for f in ["bytes", sink, BITMAP_FAMILY] {
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
        Self { el, ctx, vocab, pool, _bus: bus, check, out: Vec::new() }
    }

    fn push(&mut self, data: &[u8], pts: Timestamp, duration: Timestamp) {
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
        process_once(&mut self.el, &mut self.ctx, true).expect("a Block failed the element");
        after_process(&mut self.ctx, &self.vocab, |_, _, _, _| {});
        self.drain();
    }

    fn event(&mut self, event: Event) {
        self.el.event(&mut self.ctx, &event).expect("an event refused");
        self.drain();
    }

    fn drain(&mut self) {
        let mut batch = self.ctx.take_output(SrcPad(0));
        while let Some(b) = batch.pop_front() {
            let p = b.memory.data();
            let o = Out::of(b.pts, b.duration, p);
            (self.check)(
                &o,
                p.get(BITMAP_HEADER_LEN + BITMAP_RECT_LEN..).unwrap_or(&[]).as_chunks::<4>().0,
            );
            self.out.push(o);
        }
    }
}
