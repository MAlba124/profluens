//! `pgsdec`, decode a PGS subtitle track to `subtitle/bitmap` (RFC 9559 section 12.7,
//! `S_HDMV/PGS`).

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::BITMAP_FAMILY;
use crate::pgs;

/// The PGS subtitle family the mkv demuxer announces for an `S_HDMV/PGS` track.
pub const PGS_FAMILY: &str = "subtitle/pgs";

const SEG_END: u8 = 0x80;

/// Hard cap on the reassembly accumulator, against a track that never sends END.
const MAX_ACCUM: usize = 8 * 1024 * 1024;

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any(PGS_FAMILY), OfferDesc::any("bytes")];
static SRC_OFFERS: [OfferDesc; 2] = [OfferDesc::any(BITMAP_FAMILY), OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "pgsdec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Cold, registry make_default, boxes one element instance at plugin-registration time.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(PgsDec::new())),
    io: false,
    couplings: &[],
};

/// Decodes a PGS subtitle track to `subtitle/bitmap`.
pub struct PgsDec {
    accum: Vec<u8>,
    /// Where the next segment header in `accum` starts.
    scan: usize,
    /// The epoch's objects and palettes, kept from set to set.
    state: pgs::PgsState,
    accum_pts: Timestamp,
    accum_dur: Timestamp,
    accumulating: bool,
    /// Display sets decoded (health counter).
    pub decoded: u64,
    /// Display sets warned-and-dropped for malformed framing.
    pub dropped: u64,
}

impl Default for PgsDec {
    fn default() -> Self {
        Self::new()
    }
}

impl PgsDec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    // Cold, one-time constructor, the empty reassembly accumulator.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self {
            accum: Vec::new(),
            scan: 0,
            state: pgs::PgsState::new(),
            accum_pts: Timestamp::NONE,
            accum_dur: Timestamp::NONE,
            accumulating: false,
            decoded: 0,
            dropped: 0,
        }
    }

    fn ingest(&mut self, ctx: &mut Ctx, data: &[u8], pts: Timestamp, duration: Timestamp) {
        // A new PTS means a new Display Set. Flush a partial one (a dropped END) so the decoder
        // never wedges.
        if self.accumulating && pts != self.accum_pts {
            self.flush_set(ctx);
        }
        if !self.accumulating {
            (self.accum_pts, self.accum_dur, self.accumulating) = (pts, duration, true);
        }
        if self.accum.len().saturating_add(data.len()) > MAX_ACCUM {
            let level = if self.dropped == 0 { Level::Warn } else { Level::Debug };
            log!(
                &*ctx,
                level,
                "pgs_drop",
                reason = "display set exceeds the reassembly cap",
                pts = self.accum_pts,
            );
            self.dropped += 1;
            self.reset_accum();
            return;
        }
        self.accum.extend_from_slice(data);
        self.walk(ctx);
    }

    /// Decode what has gathered, a set its END never closed.
    fn flush_set(&mut self, ctx: &mut Ctx) {
        self.decode(ctx, self.accum.len());
        self.reset_accum();
    }

    /// Walk the segment headers gathered so far, the first END closes the set. Bytes behind it
    /// start the next set at the same time.
    fn walk(&mut self, ctx: &mut Ctx) {
        while let Some(h) = self.accum.get(self.scan..self.scan + 3) {
            let (kind, size) = (h[0], usize::from(u16::from_be_bytes([h[1], h[2]])));
            let end = self.scan + 3 + size;
            if end > self.accum.len() {
                return;
            }
            self.scan = end;
            if kind == SEG_END {
                self.decode(ctx, end);
                self.accum.drain(..end);
                self.scan = 0;
                if self.accum.is_empty() {
                    self.reset_accum();
                }
            }
        }
    }

    fn decode(&mut self, ctx: &mut Ctx, n: usize) {
        if n == 0 {
            return;
        }
        let set = &self.accum[..n];
        let written = pgs::parse_display_set_in(set, &mut self.state).and_then(|pic| {
            let n = pic.payload_len().ok_or(pgs::PgsError::Malformed("object size overflow"))?;
            let mut out =
                ctx.alloc_exact(n).ok_or(pgs::PgsError::Malformed("object past the exact cap"))?;
            let n = pic.write_bitmap(&mut out.memory.as_mut_full()[..n])?;
            out.memory.set_len(n);
            Ok(out)
        });
        match written {
            Ok(mut out) => {
                out.pts = self.accum_pts;
                out.duration = self.accum_dur;
                ctx.out(Self::OUT).push(out);
                self.decoded += 1;
            }
            Err(e) => {
                // The first drop warns, the rest count toward the end summary.
                let level = if self.dropped == 0 { Level::Warn } else { Level::Debug };
                log!(&*ctx, level, "pgs_drop", reason = e.reason(), pts = self.accum_pts);
                self.dropped += 1;
            }
        }
    }

    fn reset_accum(&mut self) {
        self.accum.clear();
        self.scan = 0;
        self.accumulating = false;
        self.accum_pts = Timestamp::NONE;
        self.accum_dur = Timestamp::NONE;
    }
}

impl Element for PgsDec {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.announce_format(Self::OUT, BITMAP_FAMILY, &[]);
        self.reset_accum();
        self.state.reset();
        self.decoded = 0;
        self.dropped = 0;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.ingest(ctx, buf.memory.data(), buf.pts, buf.duration);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => {
                self.flush_set(ctx);
                log!(&*ctx, Level::Info, "eos", decoded = self.decoded, dropped = self.dropped);
            }
            // A flush drops a half-accumulated Display Set, its remaining segments are past the
            // seek, and the epoch, the next set to draw starts one.
            Event::FlushStart => {
                log!(
                    &*ctx,
                    Level::Debug,
                    "flush",
                    partial = self.accum.len(),
                    dropped = self.dropped
                );
                self.reset_accum();
                self.state.reset();
            }
            Event::Gap { .. } => ctx.push_event(Self::OUT, event.clone()),
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.reset_accum();
        self.state.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_family_is_said_once_from_the_start() {
        let mut h = pf_harness::Harness::new(PgsDec::new());
        h.crank().expect("a pass with no input");
        let said = h.announced().expect("the family");
        assert_eq!(h.vocabulary().family_name(said.family), Some(BITMAP_FAMILY));
        h.push_event(Event::FlushStart).expect("flush");
        h.crank().expect("a pass behind the flush");
        assert_eq!(h.format_changes(), 1, "a downstream is told once");
    }

    use crate::pgs::fixtures::*;
    use crate::pgs::{DisplaySet, decode_rects};
    use pf_harness::Harness;
    use profluens_core::bus::Bus;
    use profluens_core::id::{ElementId, FormatId};
    use profluens_core::memory::Pool;

    /// Each Block at its time in seconds, the bitmaps put out and the drops.
    #[allow(clippy::disallowed_methods)]
    fn run(blocks: &[(Vec<u8>, u64)]) -> (Vec<Vec<DisplaySet>>, u64) {
        let (tx, _bus) = Bus::channel();
        let mut ctx = Ctx::new(Pool::new(4096), tx, ElementId(0), FormatId(0), 1);
        let mut dec = PgsDec::new();
        dec.start(&mut ctx).expect("start");
        for (b, secs) in blocks {
            dec.ingest(&mut ctx, b, Timestamp::from_secs(*secs), Timestamp::NONE);
        }
        dec.event(&mut ctx, &Event::Eos).expect("eos");
        let mut batch = ctx.take_output(PgsDec::OUT);
        let mut sets = Vec::new();
        while let Some(b) = batch.pop_front() {
            sets.push(decode_rects(b.memory.data()).expect("a bitmap"));
        }
        (sets, dec.dropped)
    }

    fn opaque(rgba: &[u8]) -> usize {
        rgba.as_chunks::<4>().0.iter().filter(|p| p[3] == 255).count()
    }

    #[test]
    fn a_segment_per_block_ending_in_literal_0x80_does_not_end_the_set() {
        // Four rows of four literal 0x80 pixels, the first fragment ends `80 00 00`.
        let rle = [0x80, 0x80, 0x80, 0x80, 0, 0].repeat(4);
        let (f1, f2) = rle.split_at(12);
        assert_eq!(&f1[9..], &[0x80, 0, 0]);
        let blocks = [
            pcs((1920, 1080), 0x80, 0, 0, &[(0, 0, 0, None)]),
            pds(0, &[(0x80, 235, 128, 128, 255)]),
            ods(0, 0x80, Some((4, 4)), f1),
            ods(0, 0x40, None, f2),
            end(),
        ];
        let (sets, dropped) = run(&blocks.map(|b| (b, 1)));
        assert_eq!((sets.len(), dropped), (1, 0), "one set, the object whole");
        assert_eq!(opaque(&sets[0][0].rgba), 16, "every row of the object");
    }

    #[test]
    fn a_palette_update_redraws_the_kept_object() {
        let rle = rle_rows(&[&[1, 1]]);
        let first = [
            pcs((1920, 1080), 0x80, 0, 0, &[(0, 100, 100, None)]),
            pds(0, &[(1, 235, 128, 128, 255)]),
            ods(0, 0xC0, Some((2, 1)), &rle),
            end(),
        ]
        .concat();
        // A fade step, a normal case set of a PCS and a PDS only.
        let fade = [
            pcs((1920, 1080), 0, 0x80, 0, &[(0, 100, 100, None)]),
            pds(0, &[(1, 235, 128, 128, 128)]),
            end(),
        ]
        .concat();
        // A new object in a normal case set draws through the kept palette.
        let next = [
            pcs((1920, 1080), 0, 0, 0, &[(1, 100, 900, None)]),
            ods(1, 0xC0, Some((2, 1)), &rle),
            end(),
        ]
        .concat();
        let (sets, dropped) = run(&[(first, 1), (fade, 2), (next, 3)]);
        assert_eq!((sets.len(), dropped), (3, 0));
        assert_eq!(sets[1].len(), 1, "the fade is not a clear");
        assert_eq!((sets[1][0].x, sets[1][0].y, sets[1][0].rgba[3]), (100, 100, 128));
        assert_eq!((sets[2][0].y, sets[2][0].rgba[3]), (900, 128), "the faded palette");
    }

    #[test]
    fn an_epoch_start_and_a_flush_forget_the_objects() {
        let rle = rle_rows(&[&[1, 1]]);
        let first = [
            pcs((1920, 1080), 0x80, 0, 0, &[(0, 0, 0, None)]),
            pds(0, &[(1, 235, 128, 128, 255)]),
            ods(0, 0xC0, Some((2, 1)), &rle),
            end(),
        ]
        .concat();
        let again = |state| [pcs((1920, 1080), state, 0, 0, &[(0, 0, 0, None)]), end()].concat();
        let (sets, _) = run(&[(first.clone(), 1), (again(0), 2), (again(0x80), 3)]);
        let shown: Vec<usize> = sets.iter().map(Vec::len).collect();
        assert_eq!(shown, [1, 1, 0], "kept in the epoch, gone with the next");

        let mut h = Harness::new(PgsDec::new());
        for (b, secs, flush) in [(first, 1, false), (again(0), 2, true)] {
            if flush {
                h.push_event(Event::FlushStart).expect("flush");
            }
            let mut buf = h.alloc(&b);
            buf.pts = Timestamp::from_secs(secs);
            h.push(buf).expect("push");
        }
        let out = h.drain_outputs();
        let last = decode_rects(out[out.len() - 1].memory.data()).expect("a bitmap");
        assert!(last.is_empty(), "the flush took the epoch");
    }

    #[test]
    fn families_are_pgs_in_bitmap_out() {
        assert_eq!(PGS_FAMILY, "subtitle/pgs");
        assert_eq!(BITMAP_FAMILY, "subtitle/bitmap");
        assert_eq!(SINK_OFFERS[0].family, "subtitle/pgs");
        assert_eq!(SRC_OFFERS[0].family, "subtitle/bitmap");
    }
}
