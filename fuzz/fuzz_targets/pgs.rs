//! PGS Display Sets through `pgs::parse_display_set` and the `pgsdec` element. Input is `knob`
//! then the stream. `knob & 1` 0 for `u16` LE length prefixed Blocks, 1 for a `.sup` file.

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
use pf_text::pgs::{decode_bitmap, encode_bitmap, parse_display_set, split_sup, strip_sup_header};
use pf_text::{BITMAP_FAMILY, PGS_FAMILY, PgsDec};
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

const CEILINGS: [usize; 8] = [1, 2, 3, 7, 16, 64, 512, usize::MAX];
const SEG_PDS: u8 = 0x14;
const SEG_PCS: u8 = 0x16;
const SEG_END: u8 = 0x80;
const MAX_RUN: usize = 16_383;

type Block = (Vec<u8>, Timestamp, Timestamp);

fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let blocks: Vec<Block> = if knob & 1 == 0 {
        records(rest)
            .enumerate()
            .map(|(i, b)| (b.to_vec(), Timestamp::from_secs(i as u64), Timestamp::from_secs(1)))
            .collect()
    } else {
        sup(rest)
    };
    for (set, _, _) in &blocks {
        display_set(set);
    }
    element(&blocks, knob);
});

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

fn sup(file: &[u8]) -> Vec<Block> {
    if let Some((pts, seg)) = strip_sup_header(file) {
        assert_eq!(pts, u32::from_be_bytes([file[2], file[3], file[4], file[5]]));
        assert_eq!(seg.len(), 3 + usize::from(u16::from_be_bytes([file[11], file[12]])));
    }
    let sets = split_sup(file);
    let mut out = Vec::with_capacity(sets.len());
    for (pts, _, set) in sets {
        let (end, _, _) = walk(&set);
        assert_eq!(set.first(), Some(&SEG_PCS), "a sup set that does not open with a PCS");
        assert_eq!(end, Some(set.len()), "a sup set that does not end on its END");
        let pts = Timestamp::from_nanos(u64::from(pts) * 100_000 / 9);
        out.push((set, pts, Timestamp::NONE));
    }
    out
}

fn walk(set: &[u8]) -> (Option<usize>, [bool; 256], bool) {
    let mut alpha = [0u8; 256];
    let (mut at, mut pcs) = (0usize, false);
    let mut end = None;
    while let Some(h) = set.get(at..at + 3) {
        let body = at + 3..at + 3 + usize::from(u16::from_be_bytes([h[1], h[2]]));
        let Some(p) = set.get(body.clone()) else {
            break;
        };
        at = body.end;
        match h[0] {
            SEG_PCS => pcs = true,
            SEG_PDS if p.len() >= 2 && (p.len() - 2).is_multiple_of(5) => {
                for e in p[2..].as_chunks::<5>().0 {
                    alpha[usize::from(e[0])] = e[4];
                }
            }
            SEG_END => {
                end = Some(at);
                break;
            }
            _ => {}
        }
    }
    let mut held = [false; 256];
    for a in alpha {
        held[usize::from(a)] = true;
    }
    (end, held, pcs)
}

fn display_set(set: &[u8]) {
    let got = parse_display_set(set);
    assert!(got == parse_display_set(set), "a parse is not deterministic");
    let (end, alphas, pcs) = walk(set);
    if let Some(end) = end {
        assert!(parse_display_set(&set[..end]) == got, "bytes behind END changed the set");
    }
    let Ok(ds) = got else {
        return;
    };
    assert!(pcs, "a set without a PCS decoded");
    let px = ds.width as usize * ds.height as usize;
    assert_eq!(ds.rgba.len(), px * 4, "RGBA other than width * height");
    assert!(
        px <= 8192 * 8192 && px <= MAX_RUN * set.len().div_ceil(3),
        "an object past its bounds"
    );
    for p in ds.rgba.as_chunks::<4>().0 {
        assert!(alphas[usize::from(p[3])], "an alpha the palette does not hold");
    }
    if ds.rgba.len() <= 1 << 20 {
        assert!(
            decode_bitmap(&encode_bitmap(&ds)) == Some(ds),
            "a bitmap payload does not read back"
        );
    }
}

fn ends_with_end(b: &[u8]) -> bool {
    b.len() >= 3 && b[b.len() - 3..] == [SEG_END, 0, 0]
}

fn element(blocks: &[Block], knob: u8) {
    let whole = run(blocks, None);
    let split = run(blocks, Some(knob));
    assert!(whole == split, "a Block in pieces decoded other than whole");
    if blocks.iter().all(|b| ends_with_end(&b.0)) {
        let want: Vec<Out> = blocks
            .iter()
            .filter_map(|&(ref b, pts, duration)| {
                parse_display_set(b).ok().map(|ds| Out::of(pts, duration, &encode_bitmap(&ds)))
            })
            .collect();
        assert!(whole == want, "the element put out other than the sets that decode");
    }
    if say() {
        let ok = blocks.iter().filter(|b| parse_display_set(&b.0).is_ok()).count();
        eprintln!("pgs blocks {} decode {ok} put out {}", blocks.len(), whole.len());
    }
}

fn run(blocks: &[Block], cut: Option<u8>) -> Vec<Out> {
    let mut rig = Rig::new(PgsDec::new(), PGS_FAMILY, |_: &Out, _: &[[u8; 4]]| {});
    let knob = cut.unwrap_or(0);
    let ceiling = CEILINGS[usize::from(knob >> 1 & 7)];
    let mut rng = Prng::new(u64::from(knob >> 4));
    for &(ref b, pts, duration) in blocks {
        let mut at = 0;
        loop {
            let mut end = match cut {
                None => b.len(),
                Some(_) => {
                    (at + rng.range(1, ceiling.min(b.len()) as u64 + 1) as usize).min(b.len())
                }
            };
            // A piece ending in an END header closes the set early, so no cut lands there.
            while end < b.len() && (end < 3 || ends_with_end(&b[..end])) {
                end += 1;
            }
            rig.push(&b[at..end], pts, duration);
            at = end;
            if at >= b.len() {
                break;
            }
        }
    }
    rig.event(Event::Eos);
    rig.out
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
