//! A runtime announcement the downstream pad forbids must fail loudly.

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, FormatError};
use profluens_core::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static SRC_RATES: [ValueDesc; 2] = [ValueDesc::Int(44100), ValueDesc::Int(48000)];
static SRC_FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: "rate", allowed: ConstraintDesc::Set(&SRC_RATES), preferred: None }];
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SRC_FIELDS }];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "announcesrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct AnnounceSrc {
    rate: i64,
    announced: bool,
}

impl AnnounceSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new(rate: i64) -> Self {
        Self { rate, announced: false }
    }
}

impl Element for AnnounceSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.announced {
            ctx.announce_format(Self::OUT, "audio/raw", &[("rate", ValueDesc::Int(self.rate))]);
            self.announced = true;
            return Ok(Flow::Ok); // let the FormatChange batch flush downstream first
        }
        Ok(Flow::Eos)
    }
}

static SINK_FIELDS: [FieldDesc; 1] = [FieldDesc {
    field: "rate",
    allowed: ConstraintDesc::Eq(ValueDesc::Int(44100)),
    preferred: None,
}];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SINK_FIELDS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "strictsink",
    sinks: &SINK_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

struct StrictSink;

impl StrictSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for StrictSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }
}

#[test]
fn incompatible_runtime_announcement_fails_loudly() {
    let mut p = Pipeline::new();
    let src = p.add(AnnounceSrc::new(48000));
    let snk = p.add(StrictSink);
    p.link(src.src(AnnounceSrc::OUT), snk.sink(StrictSink::IN)).expect("links at 44100");

    let res = p.run();
    assert!(
        matches!(
            &res,
            Err(Error::Format(FormatError::AnnouncementRejected(r)))
                if r.element == snk && &*r.pad_name == "sink"
        ),
        "an incompatible announcement must fail the run as a typed rejection, got: {res:?}"
    );

    let mut saw_sink_error = false;
    while let Some(msg) = p.bus().try_recv() {
        if let BusMessage::Error {
            element,
            error: Error::Format(FormatError::AnnouncementRejected(_)),
        } = msg
            && element == snk
        {
            saw_sink_error = true;
        }
    }
    assert!(saw_sink_error, "the sink posted a re-validation error to the bus");
}

#[test]
fn compatible_runtime_announcement_is_accepted() {
    let mut p = Pipeline::new();
    let src = p.add(AnnounceSrc::new(44100));
    let snk = p.add(StrictSink);
    p.link(src.src(AnnounceSrc::OUT), snk.sink(StrictSink::IN)).expect("links at 44100");
    p.run().expect("a compatible announcement runs cleanly");
}
