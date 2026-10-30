//! Dynamic element properties. Validation at the call site, delivery at batch boundaries,
//! structural props rejected while playing.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, OfferDesc, Value};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];

static KNOB_PROPS: [PropDesc; 2] = [
    PropDesc {
        name: "volume",
        allowed: Constraint::Range { min: Value::Int(0), max: Value::Int(10), step: Value::Int(1) },
        live: true,
    },
    PropDesc {
        name: "path",
        allowed: Constraint::Any,
        live: false, // structural: re-opens a resource, needs an element restart
    },
];

static KNOB_DESC: ElementDesc = ElementDesc {
    name: "knobsrc",
    sinks: &[],
    srcs: &SRC_PADS,
    props: &KNOB_PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

/// Emits `volume` as one byte per pass. The change is applied only in `event()`.
struct KnobSrc {
    volume: i64,
    stop_on_change: bool,
    changed: bool,
    done: bool,
}

impl KnobSrc {
    const OUT: SrcPad = SrcPad(0);

    fn new(volume: i64, stop_on_change: bool) -> Self {
        Self { volume, stop_on_change, changed: false, done: false }
    }
}

impl Element for KnobSrc {
    fn desc(&self) -> &'static ElementDesc {
        &KNOB_DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(Value::Int(v)) = ctx.prop("volume") {
            self.volume = v;
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.done {
            return Ok(Flow::Eos);
        }
        let mut buf = match ctx.try_alloc() {
            Some(b) => b,
            None => return Ok(Flow::Ok), // pool backpressure
        };
        buf.memory.as_mut_full()[0] = self.volume as u8;
        buf.memory.set_len(1);
        ctx.out(Self::OUT).push(buf);
        if self.changed || !self.stop_on_change {
            self.done = true; // one buffer at the final value, then EOS
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::PropChanged { name: "volume", value: Value::Int(v) } = event {
            self.volume = *v;
            self.changed = true;
        }
        Ok(())
    }
}

static RECORD_DESC: ElementDesc = ElementDesc {
    name: "recordsink",
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

struct RecordSink {
    shared: Arc<Mutex<Vec<u8>>>,
}

impl RecordSink {
    const IN: SinkPad = SinkPad(0);

    fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let shared = Arc::new(Mutex::new(Vec::new()));
        (Self { shared: Arc::clone(&shared) }, shared)
    }
}

impl Element for RecordSink {
    fn desc(&self) -> &'static ElementDesc {
        &RECORD_DESC
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut rec = self.shared.lock().unwrap();
        while let Some(buf) = inputs.pop() {
            rec.extend_from_slice(buf.memory.data());
        }
        Ok(Flow::Ok)
    }
}

fn knob_pipeline(src: KnobSrc) -> (Pipeline, ElementId, Arc<Mutex<Vec<u8>>>) {
    let mut p = Pipeline::new();
    let (sink, recorded) = RecordSink::new();
    let s = p.add(src);
    let k = p.add(sink);
    p.link(s.src(KnobSrc::OUT), k.sink(RecordSink::IN)).expect("link");
    (p, s, recorded)
}

#[test]
fn set_before_run_is_the_initial_value_in_start() {
    let (mut p, src, recorded) = knob_pipeline(KnobSrc::new(3, false));
    p.set(src, "volume", Value::Int(7)).expect("in range");
    p.run().expect("run");
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![7],
        "start() read the parked set, not the constructor value"
    );
}

#[test]
fn set_validates_loudly_at_the_call_site() {
    let (mut p, src, _recorded) = knob_pipeline(KnobSrc::new(3, false));
    assert!(p.set(src, "gain", Value::Int(1)).is_err(), "unknown property name");
    assert!(p.set(src, "volume", Value::Int(11)).is_err(), "out of range");
    assert!(p.set(src, "volume", Value::Int(-1)).is_err(), "below range");
    assert!(
        p.set(src, "volume", Value::Rat(1, 2)).is_err(),
        "wrong kind: an Int range admits no rational"
    );
    assert!(p.set(ElementId(99), "volume", Value::Int(1)).is_err(), "unknown element");
    p.set(src, "volume", Value::Int(10)).expect("boundary value admitted");
    p.set(src, "path", Value::Int(42)).expect("structural ok between runs");
}

#[test]
fn live_set_mid_run_applies_at_a_batch_boundary_in_order() {
    let (mut p, src, recorded) = knob_pipeline(KnobSrc::new(3, true));
    let props = p.prop_handle();

    let run = std::thread::spawn(move || p.run());
    props.set(src, "volume", Value::Int(9)).expect("live set");
    run.join().expect("joined").expect("run ok");

    let bytes = recorded.lock().unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(*bytes.last().unwrap(), 9, "the change arrived and was applied");
    let first_new = bytes.iter().position(|&b| b == 9).unwrap();
    assert!(bytes[..first_new].iter().all(|&b| b == 3), "before the boundary: only the old value");
    assert!(
        bytes[first_new..].iter().all(|&b| b == 9),
        "after the boundary: only the new value, no interleaving, per-link order held"
    );
}

#[test]
fn prop_handle_rejects_structural_and_unknown_while_playing() {
    let (p, src, _recorded) = knob_pipeline(KnobSrc::new(3, false));
    let props = p.prop_handle();
    assert!(
        props.set(src, "path", Value::Int(1)).is_err(),
        "structural (live: false) needs the element-restart machinery, loud error"
    );
    assert!(props.set(src, "volume", Value::Int(11)).is_err(), "handle validates too");
    assert!(props.set(ElementId(99), "volume", Value::Int(1)).is_err(), "unknown element");
    props.set(src, "volume", Value::Int(5)).expect("live in-range set ok");
}
