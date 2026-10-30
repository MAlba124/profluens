//! End-to-end format negotiation, including incompatible offers failing at `link()`.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, LinkError};
use profluens_core::format::{ConstraintDesc, FieldDesc, FixedFormat, OfferDesc, Value, ValueDesc};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_elements::flow::Queue;

static SRC_RATES: [ValueDesc; 2] = [ValueDesc::Int(44100), ValueDesc::Int(48000)];
static SRC_SAMPLES: [ValueDesc; 2] = [ValueDesc::Id("s16"), ValueDesc::Id("f32")];
static SRC_FIELDS: [FieldDesc; 2] = [
    FieldDesc {
        field: "rate",
        allowed: ConstraintDesc::Set(&SRC_RATES),
        preferred: Some(ValueDesc::Int(48000)),
    },
    FieldDesc { field: "sample", allowed: ConstraintDesc::Set(&SRC_SAMPLES), preferred: None },
];
static SRC_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SRC_FIELDS }];
static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];
static SRC_DESC: ElementDesc = ElementDesc {
    name: "audiotestsrc",
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

#[derive(Default)]
struct AudioSrc;

impl AudioSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for AudioSrc {
    fn desc(&self) -> &'static ElementDesc {
        &SRC_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Eos) // negotiation is the point here, not data
    }
}

static SINK_FIELDS: [FieldDesc; 2] = [
    FieldDesc {
        field: "rate",
        allowed: ConstraintDesc::Eq(ValueDesc::Int(48000)),
        preferred: None,
    },
    FieldDesc {
        field: "sample",
        allowed: ConstraintDesc::Eq(ValueDesc::Id("s16")),
        preferred: None,
    },
];
static SINK_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &SINK_FIELDS }];
static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SINK_DESC: ElementDesc = ElementDesc {
    name: "audiotestsink",
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

struct AudioSink {
    seen: Arc<Mutex<Option<FixedFormat>>>,
}

impl AudioSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for AudioSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK_DESC
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        *self.seen.lock().unwrap() = ctx.negotiated_sink(Self::IN).cloned();
        Ok(())
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

static BAD_FIELDS: [FieldDesc; 1] = [FieldDesc {
    field: "rate",
    allowed: ConstraintDesc::Eq(ValueDesc::Int(96000)), // src offers only 44100/48000
    preferred: None,
}];
static BAD_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &BAD_FIELDS }];
static BAD_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BAD_OFFERS }];
static BAD_DESC: ElementDesc = ElementDesc {
    name: "badsink",
    sinks: &BAD_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct BadSink;

impl BadSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for BadSink {
    fn desc(&self) -> &'static ElementDesc {
        &BAD_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

static TEXT_OFFERS: [OfferDesc; 1] = [OfferDesc::any("text/raw")];
static TEXT_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &TEXT_OFFERS }];
static TEXT_DESC: ElementDesc = ElementDesc {
    name: "textsink",
    sinks: &TEXT_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct TextSink;

impl TextSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for TextSink {
    fn desc(&self) -> &'static ElementDesc {
        &TEXT_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

static PIN_FIELDS: [FieldDesc; 2] = [
    FieldDesc {
        field: "rate",
        allowed: ConstraintDesc::Eq(ValueDesc::Int(44100)),
        preferred: None,
    },
    FieldDesc {
        field: "sample",
        allowed: ConstraintDesc::Eq(ValueDesc::Id("s16")),
        preferred: None,
    },
];
static PIN_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &PIN_FIELDS }];
static PIN_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &PIN_OFFERS }];
static PIN_DESC: ElementDesc = ElementDesc {
    name: "pinned44k",
    sinks: &[],
    srcs: &PIN_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct PinnedSrc;

impl PinnedSrc {
    const OUT: SrcPad = SrcPad(0);
}

impl Element for PinnedSrc {
    fn desc(&self) -> &'static ElementDesc {
        &PIN_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Eos)
    }
}

static FLEX_RATES: [ValueDesc; 2] = [ValueDesc::Int(44100), ValueDesc::Int(48000)];
static FLEX_FIELDS: [FieldDesc; 2] = [
    FieldDesc {
        field: "rate",
        allowed: ConstraintDesc::Set(&FLEX_RATES),
        preferred: Some(ValueDesc::Int(48000)),
    },
    FieldDesc {
        field: "sample",
        allowed: ConstraintDesc::Eq(ValueDesc::Id("s16")),
        preferred: None,
    },
];
static FLEX_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &FLEX_FIELDS }];
static FLEX_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &FLEX_OFFERS }];
static FLEX_DESC: ElementDesc = ElementDesc {
    name: "flexsink",
    sinks: &FLEX_PADS,
    srcs: &[],
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

#[derive(Default)]
struct FlexSink;

impl FlexSink {
    const IN: SinkPad = SinkPad(0);
}

impl Element for FlexSink {
    fn desc(&self) -> &'static ElementDesc {
        &FLEX_DESC
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

// A wildcard transport must not launder a contradiction. 44.1k samples on a 48k link
// is silent corruption.
#[test]
fn wildcard_transport_cannot_launder_a_contradiction() {
    {
        let mut p = Pipeline::new();
        let src = p.add(PinnedSrc);
        let snk = p.add(AudioSink { seen: Arc::new(Mutex::new(None)) });
        assert!(
            p.link(src.src(PinnedSrc::OUT), snk.sink(AudioSink::IN)).is_err(),
            "44.1k src vs 48k sink"
        );
    }

    let seen = Arc::new(Mutex::new(None));
    let mut p = Pipeline::new();
    let src = p.add(PinnedSrc);
    let q = p.add(Queue::new());
    let snk = p.add(AudioSink { seen: Arc::clone(&seen) });

    let a = p
        .link(src.src(PinnedSrc::OUT), q.sink(Queue::IN))
        .expect("wildcard adopts the src's 44.1k");
    let b = p.link(q.src(Queue::OUT), snk.sink(AudioSink::IN));

    let rate = p.field_id("rate").expect("rate field seen");
    match b {
        Ok(b) => {
            let up = p.negotiated(a).expect("upstream edge fixed").get(rate);
            let down = p.negotiated(b).expect("downstream edge fixed").get(rate);
            panic!(
                "queue laundered a contradiction: it carries {up:?} in and claims {down:?} out, \
                 yet copies buffers verbatim"
            );
        }
        Err(e) => {
            let Error::Link(LinkError::NoCommonFormat(d)) = &e else {
                panic!("expected no common format, got: {e:?}");
            };
            assert_eq!(d.src.element, q, "the queue is the src end of the failed link");
            assert_eq!(
                d.src.bound.as_deref(),
                Some("audio/raw, rate=44100, sample=s16"),
                "error should carry what the transport is already bound to, got: {e:?}"
            );
            assert!(
                e.to_string().contains("other wildcard pad"),
                "message should explain the transport is already bound, got: {e}"
            );
        }
    }
}

// A flexible peer behind a bound queue takes the queue's format, not its own preference.
#[test]
fn wildcard_transport_propagates_its_bound_format() {
    let mut p = Pipeline::new();
    let src = p.add(PinnedSrc); // 44.1k only
    let q = p.add(Queue::new());
    let snk = p.add(FlexSink); // 44.1k or 48k, prefers 48k

    let a = p.link(src.src(PinnedSrc::OUT), q.sink(Queue::IN)).expect("queue adopts 44.1k");
    let b = p.link(q.src(Queue::OUT), snk.sink(FlexSink::IN)).expect("flexible sink accepts 44.1k");

    let rate = p.field_id("rate").expect("rate field seen");
    assert_eq!(p.negotiated(a).unwrap().get(rate), Some(Value::Int(44100)), "upstream edge");
    assert_eq!(
        p.negotiated(b).unwrap().get(rate),
        Some(Value::Int(44100)),
        "the queue carries 44.1k through, the sink's 48k preference must not win, since the \
         queue does not resample"
    );
}

#[test]
fn typed_audio_negotiates_to_expected_fixed_format() {
    let seen = Arc::new(Mutex::new(None));
    let mut p = Pipeline::new();
    let src = p.add(AudioSrc);
    let snk = p.add(AudioSink { seen: Arc::clone(&seen) });
    let link = p
        .link(src.src(AudioSrc::OUT), snk.sink(AudioSink::IN))
        .expect("compatible offers negotiate");

    let rate = p.field_id("rate").expect("rate field seen");
    let sample = p.field_id("sample").expect("sample field seen");
    let s16 = p.value_id("s16").expect("s16 value seen");

    let fixed = p.negotiated(link).expect("edge has a fixed format");
    assert_eq!(p.family_name(fixed.family), Some("audio/raw"), "family is the one both pads share");
    assert_eq!(fixed.get(rate), Some(Value::Int(48000)), "rate fixated to 48k");
    assert_eq!(fixed.get(sample), Some(Value::Id(s16)), "sample fixated to s16");
    assert_eq!(fixed.fields().len(), 2, "exactly the two shared fields are fixed");

    p.run().expect("run drains immediately (src goes straight to EOS)");
    let element_saw = seen.lock().unwrap().clone().expect("sink recorded its format");
    assert_eq!(element_saw.get(rate), Some(Value::Int(48000)));
    assert_eq!(element_saw.get(sample), Some(Value::Id(s16)));
}

#[test]
fn incompatible_rate_fails_negotiation_at_link() {
    let mut p = Pipeline::new();
    let src = p.add(AudioSrc);
    let bad = p.add(BadSink);
    let err = p.link(src.src(AudioSrc::OUT), bad.sink(BadSink::IN)).unwrap_err();
    assert!(
        matches!(err, Error::Link(LinkError::NoCommonFormat(_))),
        "error should be the empty intersection, got: {err:?}"
    );
}

#[test]
fn family_mismatch_fails_negotiation_at_link() {
    let mut p = Pipeline::new();
    let src = p.add(AudioSrc);
    let txt = p.add(TextSink);
    assert!(
        p.link(src.src(AudioSrc::OUT), txt.sink(TextSink::IN)).is_err(),
        "different families must not link"
    );
}
