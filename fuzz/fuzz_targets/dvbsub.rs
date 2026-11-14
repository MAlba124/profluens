//! DVB subtitle Blocks through the `dvbsubdec` element. Input is `knob` then records of a
//! `u16` LE length, a PTS step in 100 ms (0 continues the set) and the Block. `knob & 1` makes
//! a five byte first record the CodecPrivate, failing that `knob & 0x20` a CodecPrivate of
//! composition page 1 and ancillary page `1 + (knob >> 6)`, so the page filter drops some
//! segments of a corpus written on page 1. `knob & 2` gives a
//! 10 s duration, `knob >> 2` the cuts. Every set's work stays in its budget and every bitmap's
//! pixels in the region budget.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods)]
// `fuzz_target!` opens libfuzzer-sys's debug output file.
#![allow(clippy::disallowed_types)]

use std::hash::{DefaultHasher, Hasher};
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use pf_sim::prng::Prng;
use pf_text::bitmap::{BITMAP_HEADER_LEN, BITMAP_RECT_LEN, BitmapRef};
use pf_text::dvbsubdec::{PIXEL_BUDGET, WORK_BUDGET};
use pf_text::pgs::decode_rects;
use pf_text::{BITMAP_FAMILY, DVB_FAMILY, DvbSubDec};
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

const SYNC: u8 = 0x0F;
const DURATION: Timestamp = Timestamp::from_secs(10);
const TIME_OUT: Timestamp = Timestamp::from_secs(255);

type Block<'a> = (&'a [u8], Timestamp);

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let mut blocks: Vec<Block<'_>> = records(rest).collect();
    let pages = [0, 1, 0, 1 + (knob >> 6), 0];
    let head = match knob & 1 != 0 && blocks.first().is_some_and(|b| b.0.len() == 5) {
        true => Some(blocks.remove(0).0),
        false => (knob & 0x20 != 0).then_some(&pages[..]),
    };
    let duration = if knob & 2 != 0 { DURATION } else { Timestamp::NONE };
    let check = |o: &Out, _: &[[u8; 4]]| bitmap(o, &blocks, duration);

    let mut rig = Rig::new(DvbSubDec::new(), DVB_FAMILY, check);
    play(&mut rig, head, &blocks, duration, None);
    let before_eos = rig.out.len();
    rig.event(Event::Eos);
    let once = std::mem::take(&mut rig.out);
    let faulted = rig.el.dropped > 0;

    let mut flushed = Rig::new(DvbSubDec::new(), DVB_FAMILY, check);
    play(&mut flushed, head, &blocks, duration, None);
    assert!(flushed.out[..] == once[..before_eos], "a run is not deterministic");
    flushed.event(Event::FlushStart);
    flushed.out.clear();
    play(&mut flushed, head, &blocks, duration, None);
    flushed.event(Event::Eos);
    assert!(flushed.out == once, "a flush did not start the stream over");

    if say() {
        let shows = once.iter().filter(|o| !o.rects.is_empty()).count();
        eprintln!(
            "dvbsub blocks {} put out {} shows {shows} dropped {}",
            blocks.len(),
            once.len(),
            rig.el.dropped
        );
    }
    if faulted {
        return;
    }
    let mut split = Rig::new(DvbSubDec::new(), DVB_FAMILY, check);
    play(&mut split, head, &blocks, duration, Some(Prng::new(u64::from(knob >> 2))));
    split.event(Event::Eos);
    assert_eq!(split.el.dropped, 0, "a Block in pieces faulted where whole it did not");
    assert!(split.out == once, "a Block in pieces decoded other than whole");
});

fn play<F: FnMut(&Out, &[[u8; 4]])>(
    rig: &mut Rig<DvbSubDec, F>,
    head: Option<&[u8]>,
    blocks: &[Block<'_>],
    d: Timestamp,
    mut cut: Option<Prng>,
) {
    if let Some(h) = head {
        rig.push(h, Timestamp::NONE, Timestamp::NONE);
    }
    for &(b, pts) in blocks {
        let mut at = 0;
        if let Some(rng) = &mut cut {
            for end in boundaries(b) {
                if rng.below(2) == 0 {
                    rig.push(&b[at..end], pts, d);
                    at = end;
                }
            }
        }
        // A Block may close the set before it, on that set's budget, then spend its own.
        let before = rig.el.work_done();
        rig.push(&b[at..], pts, d);
        assert!(rig.el.held_pixels() <= PIXEL_BUDGET, "the region arena past its budget");
        assert!(rig.el.work_spent() <= WORK_BUDGET, "a set past its work budget");
        assert!(rig.el.work_done() - before <= 2 * WORK_BUDGET, "a Block past two budgets");
    }
}

fn boundaries(b: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut at = usize::from(b.len() >= 2 && b[0] == 0x20 && b[1] == 0) * 2;
    std::iter::from_fn(move || {
        let h = b.get(at..at + 6).filter(|h| h[0] == SYNC)?;
        let next = at + 6 + usize::from(u16::from_be_bytes([h[4], h[5]]));
        (b.get(next) == Some(&SYNC)).then(|| {
            at = next;
            next
        })
    })
}

fn bitmap(o: &Out, blocks: &[Block<'_>], duration: Timestamp) {
    let [gw, gh] = o.grid;
    assert!((1..=4096).contains(&gw) && (1..=4096).contains(&gh), "a grid out of range");
    let mut pixels = 0usize;
    for &[w, h, x, y] in &o.rects {
        assert!(w > 0 && h > 0, "an empty rect");
        assert!(x + w <= gw && y + h <= gh, "a show past its grid");
        pixels += w as usize * h as usize;
    }
    assert!(pixels <= PIXEL_BUDGET, "a bitmap of more pixels than the regions hold");
    assert!(blocks.iter().any(|b| b.1 == o.pts), "a bitmap at a PTS no Block had");
    let most = if duration.is_some() { duration } else { TIME_OUT };
    assert!(o.duration.is_none() || o.duration <= most, "held past its duration");
    assert!(duration.is_none() || o.duration.is_some(), "a Block's duration lost");
}

fn records(mut data: &[u8]) -> impl Iterator<Item = Block<'_>> {
    let mut pts = 0u64;
    std::iter::from_fn(move || {
        let (len, step, rest) = match data.split_first_chunk::<3>() {
            Some((h, rest)) => (usize::from(u16::from_le_bytes([h[0], h[1]])), h[2], rest),
            None if data.is_empty() => return None,
            None => (data.len(), 1, std::mem::take(&mut data)),
        };
        let (rec, tail) = rest.split_at(len.min(rest.len()));
        data = tail;
        pts += u64::from(step);
        Some((rec, Timestamp::from_millis(pts * 100)))
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
    /// Width, height, x and y of each rect, one per region.
    rects: Vec<[u32; 4]>,
    grid: [u32; 2],
    hash: u64,
}

impl Out {
    fn of(pts: Timestamp, duration: Timestamp, p: &[u8]) -> Out {
        let image = BitmapRef::parse(p).expect("a bitmap payload the view refuses");
        let rects: Vec<[u32; 4]> =
            image.rects().iter().map(|r| [r.width, r.height, r.x, r.y]).collect();
        let px: usize = rects.iter().map(|r| r[0] as usize * r[1] as usize).sum();
        let table = BITMAP_HEADER_LEN + rects.len() * BITMAP_RECT_LEN;
        assert_eq!(p.len() - table, px * 4, "RGBA other than width * height");
        if p.len() <= 1 << 16 {
            assert!(decode_rects(p).is_some_and(|r| r.len() == rects.len()), "a payload refused");
        }
        let mut h = DefaultHasher::new();
        h.write(p);
        let grid = [image.video_width(), image.video_height()];
        Out { pts, duration, rects, grid, hash: h.finish() }
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
