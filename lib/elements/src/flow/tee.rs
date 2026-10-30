//! `tee` fans one stream out to N branches by refcount, with no copy.
//! A branch that mutates in place pays a full-slot copy-on-write, so write to own output.

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

/// The static pad table's src pad budget.
pub const MAX_BRANCHES: usize = 8;

static OFFERS: [OfferDesc; 1] = [OfferDesc::wildcard()];

const fn src_pad(name: &'static str) -> PadDesc {
    PadDesc { name, offers: &OFFERS }
}

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 8] = [
    src_pad("src_0"),
    src_pad("src_1"),
    src_pad("src_2"),
    src_pad("src_3"),
    src_pad("src_4"),
    src_pad("src_5"),
    src_pad("src_6"),
    src_pad("src_7"),
];

static DESC: ElementDesc = ElementDesc {
    name: "tee",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    // Active, as a branch point feeds one downstream group per src pad.
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

pub struct Tee {
    n: usize,
}

impl Tee {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUTS: [SrcPad; MAX_BRANCHES] =
        [SrcPad(0), SrcPad(1), SrcPad(2), SrcPad(3), SrcPad(4), SrcPad(5), SrcPad(6), SrcPad(7)];

    /// A tee feeding `src_0..src_{n-1}`, `n` in `1..=MAX_BRANCHES`.
    pub fn new(n: usize) -> Tee {
        assert!(
            (1..=MAX_BRANCHES).contains(&n),
            "tee supports 1..={MAX_BRANCHES} branches, got {n}"
        );
        Tee { n }
    }
}

impl Element for Tee {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            // `Buffer` is not `Clone` (single-owner sync fences), so the shell is rebuilt per
            // branch and a fence stays on branch 0 only.
            for i in 1..self.n {
                let copy = profluens_core::buffer::Buffer {
                    memory: buf.memory.clone(),
                    pts: buf.pts,
                    dts: buf.dts,
                    duration: buf.duration,
                    flags: buf.flags,
                    format: buf.format,
                    sync: None,
                };
                ctx.out(Self::OUTS[i]).push(copy);
            }
            ctx.out(Self::OUTS[0]).push(buf);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        // A FormatChange terminates here, so re-announce it onward or every branch decodes the
        // new format as the old one.
        // Gaps go to every branch too.
        match event {
            Event::FormatChange(f) => {
                log!(&*ctx, Level::Debug, "format_change", branches = self.n);
                for &out in &Self::OUTS[..self.n] {
                    ctx.forward_format(out, f.clone());
                }
            }
            Event::Gap { .. } => {
                for &out in &Self::OUTS[..self.n] {
                    ctx.push_event(out, event.clone());
                }
            }
            _ => {}
        }
        Ok(())
    }
}
