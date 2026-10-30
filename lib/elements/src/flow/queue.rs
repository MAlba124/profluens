//! `queue`, the explicit thread-decoupling boundary. An Active passthrough with wildcard
//! pads. The scheduler's rings on each side are the actual queue.

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

static OFFERS: [OfferDesc; 1] = [OfferDesc::wildcard()];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

// Cold, one box per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "queue",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    // Active so the scheduler gives it its own group and a ring on each side.
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    // Ring residency latency is computed by the pipeline, not declared here.
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(Queue::new())),
    io: false,
    couplings: &[],
};

#[derive(Default)]
pub struct Queue;

impl Queue {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self
    }
}

impl Element for Queue {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            ctx.out(Self::OUT).push(buf);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        // A FormatChange terminates here, so a pure transport re-announces it onward. Gaps go on
        // too.
        match event {
            Event::FormatChange(f) => {
                log!(&*ctx, Level::Debug, "format_change");
                ctx.forward_format(Self::OUT, f.clone());
            }
            Event::Gap { .. } => ctx.push_event(Self::OUT, event.clone()),
            _ => {}
        }
        Ok(())
    }
}
