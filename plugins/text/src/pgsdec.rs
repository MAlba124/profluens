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
    scratch: pgs::PgsScratch,
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
            scratch: pgs::PgsScratch::new(),
            accum_pts: Timestamp::NONE,
            accum_dur: Timestamp::NONE,
            accumulating: false,
            decoded: 0,
            dropped: 0,
        }
    }

    fn flush_set(&mut self, ctx: &mut Ctx) {
        if self.accum.is_empty() {
            self.reset_accum();
            return;
        }
        let written = pgs::parse_display_set_in(&self.accum, &mut self.scratch).and_then(|pic| {
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
        self.reset_accum();
    }

    fn reset_accum(&mut self) {
        self.accum.clear();
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
        self.decoded = 0;
        self.dropped = 0;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            let data = buf.memory.data();
            // A new PTS means a new Display Set. Flush a partial one (a dropped END) so the decoder
            // never wedges.
            if self.accumulating && buf.pts != self.accum_pts {
                self.flush_set(ctx);
            }
            if !self.accumulating {
                self.accum_pts = buf.pts;
                self.accum_dur = buf.duration;
                self.accumulating = true;
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
                continue;
            }
            self.accum.extend_from_slice(data);
            if ends_with_end_segment(&self.accum) {
                self.flush_set(ctx);
            }
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
            // seek.
            Event::FlushStart => {
                log!(
                    &*ctx,
                    Level::Debug,
                    "flush",
                    partial = self.accum.len(),
                    dropped = self.dropped
                );
                self.reset_accum();
            }
            Event::Gap { .. } => ctx.push_event(Self::OUT, event.clone()),
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.reset_accum();
    }
}

/// Whether `data` ends on a well-framed `END` segment (`0x80, 0x00, 0x00`).
fn ends_with_end_segment(data: &[u8]) -> bool {
    let n = data.len();
    n >= 3 && data[n - 3] == SEG_END && data[n - 2] == 0 && data[n - 1] == 0
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

    #[test]
    fn detects_terminal_end_segment() {
        assert!(ends_with_end_segment(&[0x16, 0x00, 0x00, 0x80, 0x00, 0x00]));
        assert!(!ends_with_end_segment(&[0x16, 0x00, 0x00]), "no END");
        assert!(!ends_with_end_segment(&[0x80]), "too short");
        assert!(!ends_with_end_segment(&[0x80, 0x00, 0x05]));
    }

    #[test]
    fn families_are_pgs_in_bitmap_out() {
        assert_eq!(PGS_FAMILY, "subtitle/pgs");
        assert_eq!(BITMAP_FAMILY, "subtitle/bitmap");
        assert_eq!(SINK_OFFERS[0].family, "subtitle/pgs");
        assert_eq!(SRC_OFFERS[0].family, "subtitle/bitmap");
    }
}
