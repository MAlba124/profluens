use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::{
    Error, GraphError, LinkEnd, LinkError, NoCommonFormat, PropError, RejectedValue,
};
use profluens_core::format::{Constraint, ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static WILD: [OfferDesc; 1] = [OfferDesc::wildcard()];

const fn rate(hz: i64) -> [FieldDesc; 1] {
    [FieldDesc { field: "rate", allowed: ConstraintDesc::Eq(ValueDesc::Int(hz)), preferred: None }]
}
static RATE_44K: [FieldDesc; 1] = rate(44_100);
static RATE_48K: [FieldDesc; 1] = rate(48_000);
static AUDIO_44K: [OfferDesc; 2] =
    [OfferDesc { family: "audio/raw", fields: &RATE_44K }, OfferDesc::any("audio/aac")];
static AUDIO_48K: [OfferDesc; 1] = [OfferDesc { family: "audio/raw", fields: &RATE_48K }];

const fn pad(name: &'static str, offers: &'static [OfferDesc]) -> PadDesc {
    PadDesc { name, offers }
}

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    props: &'static [PropDesc],
    sched: SchedHint,
) -> ElementDesc {
    ElementDesc {
        name,
        sinks,
        srcs,
        props,
        sched,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }
}

static SRC_PADS: [PadDesc; 1] = [pad("src", &BYTES)];
static SPLIT_PADS: [PadDesc; 2] = [pad("a", &BYTES), pad("b", &BYTES)];
static SINK_PADS: [PadDesc; 1] = [pad("sink", &BYTES)];
static FILTER_SINKS: [PadDesc; 2] = [pad("sink", &BYTES), pad("loop", &BYTES)];
static MIX_PADS: [PadDesc; 2] = [pad("a", &BYTES), pad("b", &BYTES)];
static SRC_44K_PADS: [PadDesc; 1] = [pad("src", &AUDIO_44K)];
static SINK_48K_PADS: [PadDesc; 1] = [pad("sink", &AUDIO_48K)];
static PIPE_SINKS: [PadDesc; 1] = [pad("sink", &WILD)];
static PIPE_SRCS: [PadDesc; 1] = [pad("src", &WILD)];

const IN: SinkPad = SinkPad(0);
const IN_B: SinkPad = SinkPad(1);
const OUT: SrcPad = SrcPad(0);
const OUT_B: SrcPad = SrcPad(1);

static KNOB_PROPS: [PropDesc; 2] = [
    PropDesc {
        name: "volume",
        allowed: Constraint::Range { min: Value::Int(0), max: Value::Int(10), step: Value::Int(1) },
        live: true,
    },
    PropDesc { name: "path", allowed: Constraint::Any, live: false },
];

static SRC: ElementDesc = desc("esrc", &[], &SRC_PADS, &[], SchedHint::Active);
static SPLIT: ElementDesc = desc("esplit", &[], &SPLIT_PADS, &[], SchedHint::Active);
static PASSIVE_SINK: ElementDesc = desc("epassive", &SINK_PADS, &[], &[], SchedHint::Passive);
static ACTIVE_SINK: ElementDesc = desc("eactive", &SINK_PADS, &[], &[], SchedHint::Active);
static FILTER: ElementDesc = desc("efilter", &FILTER_SINKS, &SRC_PADS, &[], SchedHint::Passive);
static MIX: ElementDesc = desc("emix", &MIX_PADS, &[], &[], SchedHint::Passive);
static SRC_44K: ElementDesc = desc("e44", &[], &SRC_44K_PADS, &[], SchedHint::Active);
static SINK_48K: ElementDesc = desc("e48", &SINK_48K_PADS, &[], &[], SchedHint::Active);
static PIPE: ElementDesc = desc("epipe", &PIPE_SINKS, &PIPE_SRCS, &[], SchedHint::Active);
static KNOB: ElementDesc = desc("eknob", &[], &SRC_PADS, &KNOB_PROPS, SchedHint::Active);
static DEMUX: ElementDesc = desc("edemux", &SINK_PADS, &[], &[], SchedHint::Active);
static IO_SRC: ElementDesc =
    ElementDesc { io: true, ..desc("eiosrc", &[], &SRC_PADS, &[], SchedHint::Active) };
static IO_SINK: ElementDesc =
    ElementDesc { io: true, ..desc("eiosink", &SINK_PADS, &[], &[], SchedHint::Active) };

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Drain,
    Panic,
    AddPads,
    Io,
}

struct Node {
    desc: &'static ElementDesc,
    act: Act,
}

fn node(desc: &'static ElementDesc) -> Node {
    Node { desc, act: Act::Drain }
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if self.act == Act::AddPads {
            ctx.add_src_pad("track_0", &BYTES);
            ctx.add_sink_pad("feed_0", &BYTES);
        }
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.act == Act::Panic {
            panic!("node told to panic");
        }
        if self.act == Act::Io {
            ctx.io().timer(std::time::Duration::ZERO, 0);
        }
        while inputs.pop().is_some() {}
        let is_source = self.desc.sinks.is_empty();
        Ok(if is_source { Flow::Eos } else { Flow::Ok })
    }
}

fn link_err(r: Result<LinkId, Error>) -> LinkError {
    match r {
        Err(e) => link_of(e),
        other => panic!("expected a link error, got {other:?}"),
    }
}

fn link_of(e: Error) -> LinkError {
    match e {
        Error::Link(e) => e,
        other => panic!("expected a link error, got {other:?}"),
    }
}

fn graph_err(r: Result<(), Error>) -> GraphError {
    match r {
        Err(Error::Graph(e)) => e,
        other => panic!("expected a graph error, got {other:?}"),
    }
}

fn prop_err(r: Result<(), Error>) -> PropError {
    match r {
        Err(Error::Prop(e)) => e,
        other => panic!("expected a property error, got {other:?}"),
    }
}

fn end(
    element: ElementId,
    desc: &'static ElementDesc,
    pad: &str,
    offers: &[&'static str],
    bound: Option<&str>,
) -> LinkEnd {
    LinkEnd {
        element,
        element_name: desc.name,
        pad: pad.into(),
        offers: offers.iter().map(|o| (*o).into()).collect(),
        bound: bound.map(Into::into),
    }
}

#[test]
fn link_to_an_unknown_element() {
    let mut p = Pipeline::new();
    let src = p.add(node(&SRC));
    let ghost = ElementId(99);
    assert_eq!(
        link_err(p.link(src.src(OUT), ghost.sink(IN))),
        LinkError::UnknownElement { element: ghost }
    );
    assert_eq!(
        link_err(p.link(ghost.src(OUT), src.sink(IN))),
        LinkError::UnknownElement { element: ghost },
        "the src end is checked first"
    );
    let unknown = LinkError::UnknownElement { element: ghost };
    assert_eq!(p.src_pad(ghost, "src").map_err(link_of), Err(unknown.clone()));
    assert_eq!(p.sink_pad(ghost, "sink").map_err(link_of), Err(unknown));
}

#[test]
fn link_a_pad_index_the_element_does_not_have() {
    let mut p = Pipeline::new();
    let src = p.add(node(&SRC));
    let snk = p.add(node(&ACTIVE_SINK));
    assert_eq!(
        link_err(p.link(src.src(OUT_B), snk.sink(IN))),
        LinkError::NoSrcPad { element: src, element_name: "esrc", pad: OUT_B }
    );
    assert_eq!(
        link_err(p.link(src.src(OUT), snk.sink(IN_B))),
        LinkError::NoSinkPad { element: snk, element_name: "eactive", pad: IN_B }
    );
    assert_eq!(
        link_err(p.link(snk.src(OUT), snk.sink(IN))),
        LinkError::NoSrcPad { element: snk, element_name: "eactive", pad: OUT }
    );
}

#[test]
fn a_pad_is_found_by_name_on_its_own_side_only() {
    let mut p = Pipeline::new();
    let split = p.add(node(&SPLIT));
    let filter = p.add(node(&FILTER));
    assert_eq!(p.src_pad(split, "a").map_err(link_of), Ok(OUT));
    assert_eq!(p.src_pad(split, "b").map_err(link_of), Ok(OUT_B));
    assert_eq!(p.sink_pad(filter, "sink").map_err(link_of), Ok(IN));
    assert_eq!(p.sink_pad(filter, "loop").map_err(link_of), Ok(IN_B));
    assert_eq!(p.src_pad(filter, "src").map_err(link_of), Ok(OUT));
    assert_eq!(
        p.sink_pad(filter, "video_9").map_err(link_of),
        Err(LinkError::UnknownSinkPad {
            element: filter,
            element_name: "efilter",
            pad: "video_9".into()
        })
    );
    assert_eq!(
        p.src_pad(filter, "loop").map_err(link_of),
        Err(LinkError::UnknownSrcPad {
            element: filter,
            element_name: "efilter",
            pad: "loop".into()
        })
    );
    assert_eq!(
        p.sink_pad(filter, "src").map_err(link_of),
        Err(LinkError::UnknownSinkPad {
            element: filter,
            element_name: "efilter",
            pad: "src".into()
        })
    );
}

#[test]
fn pads_added_at_preroll_get_typed_ids_past_the_static_ones() {
    let mut p = Pipeline::new();
    let src = p.add(node(&SRC));
    let demux = p.add(Node { desc: &DEMUX, act: Act::AddPads });
    let snk = p.add(node(&ACTIVE_SINK));
    let added = p.preroll().expect("preroll adds the pads");
    let pads = |a: &profluens_core::pipeline::AddedPads| {
        let srcs: Vec<_> = a.srcs.iter().map(|s| (s.element, s.pad, s.name.clone())).collect();
        let sinks: Vec<_> = a.sinks.iter().map(|s| (s.element, s.pad, s.name.clone())).collect();
        (srcs, sinks)
    };
    assert_eq!(
        pads(&added),
        (vec![(demux, OUT, "track_0".to_string())], vec![(demux, IN_B, "feed_0".to_string())])
    );
    assert_eq!(p.src_pad(demux, "track_0").map_err(link_of), Ok(OUT));
    assert_eq!(p.sink_pad(demux, "feed_0").map_err(link_of), Ok(IN_B));
    assert_eq!(
        p.sink_pad(demux, "track_0").map_err(link_of),
        Err(LinkError::UnknownSinkPad {
            element: demux,
            element_name: "edemux",
            pad: "track_0".into()
        })
    );
    let mut on_bus = (Vec::new(), Vec::new());
    while let Some(msg) = p.bus().try_recv() {
        match msg {
            BusMessage::SrcPadAdded { element, pad, .. } => on_bus.0.push((element, pad)),
            BusMessage::SinkPadAdded { element, pad, .. } => on_bus.1.push((element, pad)),
            _ => {}
        }
    }
    assert_eq!(on_bus, (vec![(demux, OUT)], vec![(demux, IN_B)]));

    p.link(src.src(OUT), demux.sink(added.sinks[0].pad)).expect("the added sink pad links");
    p.link(demux.src(added.srcs[0].pad), snk.sink(IN)).expect("the added src pad links");
    assert_eq!(
        link_err(p.link(demux.src(OUT_B), snk.sink(IN))),
        LinkError::NoSrcPad { element: demux, element_name: "edemux", pad: OUT_B }
    );
    let again = p.preroll().expect("preroll");
    assert!(again.srcs.is_empty() && again.sinks.is_empty());
    let late = p.add(Node { desc: &DEMUX, act: Act::AddPads });
    let again = p.preroll().expect("preroll");
    assert_eq!((again.srcs[0].element, again.srcs[0].pad, again.sinks[0].pad), (late, OUT, IN_B));
}

#[test]
fn link_with_no_common_format() {
    let mut p = Pipeline::new();
    let src = p.add(node(&SRC_44K));
    let snk = p.add(node(&SINK_48K));
    let got = link_err(p.link(src.src(OUT), snk.sink(IN)));
    let want = NoCommonFormat {
        src: end(src, &SRC_44K, "src", &["audio/raw", "audio/aac"], None),
        sink: end(snk, &SINK_48K, "sink", &["audio/raw"], None),
    };
    assert_eq!(got, LinkError::NoCommonFormat(want.into()));
}

#[test]
fn no_common_format_names_a_bound_transport_on_the_src_end() {
    let mut p = Pipeline::new();
    let src = p.add(node(&SRC_44K));
    let pipe = p.add(node(&PIPE));
    let snk = p.add(node(&SINK_48K));
    p.link(src.src(OUT), pipe.sink(IN)).expect("the wildcard adopts 44.1k");
    let got = link_err(p.link(pipe.src(OUT), snk.sink(IN)));
    let want = NoCommonFormat {
        src: end(pipe, &PIPE, "src", &["*"], Some("audio/raw, rate=44100")),
        sink: end(snk, &SINK_48K, "sink", &["audio/raw"], None),
    };
    assert_eq!(got, LinkError::NoCommonFormat(want.into()));
    let text = got.to_string();
    assert!(text.contains("epipe already carries audio/raw, rate=44100"), "{text}");
}

#[test]
fn no_common_format_names_a_bound_transport_on_the_sink_end() {
    let mut p = Pipeline::new();
    let src = p.add(node(&SRC_44K));
    let pipe = p.add(node(&PIPE));
    let snk = p.add(node(&SINK_48K));
    p.link(pipe.src(OUT), snk.sink(IN)).expect("the wildcard adopts 48k");
    let got = link_err(p.link(src.src(OUT), pipe.sink(IN)));
    let want = NoCommonFormat {
        src: end(src, &SRC_44K, "src", &["audio/raw", "audio/aac"], None),
        sink: end(pipe, &PIPE, "sink", &["*"], Some("audio/raw, rate=48000")),
    };
    assert_eq!(got, LinkError::NoCommonFormat(want.into()));
}

#[test]
fn run_an_empty_pipeline() {
    assert_eq!(graph_err(Pipeline::new().run()), GraphError::Empty);
}

#[test]
fn run_a_cycle_with_no_entry() {
    let mut p = Pipeline::new();
    let a = p.add(node(&FILTER));
    let b = p.add(node(&FILTER));
    p.link(a.src(OUT), b.sink(IN)).unwrap();
    p.link(b.src(OUT), a.sink(IN)).unwrap();
    assert_eq!(graph_err(p.run()), GraphError::NoSource);
}

#[test]
fn run_a_cycle_behind_a_source() {
    let mut p = Pipeline::new();
    let src = p.add(node(&SRC));
    let a = p.add(node(&FILTER));
    let b = p.add(node(&FILTER));
    p.link(src.src(OUT), a.sink(IN)).unwrap();
    p.link(a.src(OUT), b.sink(IN)).unwrap();
    p.link(b.src(OUT), a.sink(IN_B)).unwrap();
    assert_eq!(graph_err(p.run()), GraphError::Cycle { elements: [a, b].into() });
}

#[test]
fn run_a_passive_element_with_two_inputs() {
    let mut p = Pipeline::new();
    let s1 = p.add(node(&SRC));
    let s2 = p.add(node(&SRC));
    let mix = p.add(node(&MIX));
    p.link(s1.src(OUT), mix.sink(IN)).unwrap();
    p.link(s2.src(OUT), mix.sink(IN_B)).unwrap();
    assert_eq!(graph_err(p.run()), GraphError::PassiveMultipleInputs { element: mix, inputs: 2 });
}

#[test]
fn run_a_passive_fan_out() {
    let mut p = Pipeline::new();
    let split = p.add(node(&SPLIT));
    let a = p.add(node(&PASSIVE_SINK));
    let b = p.add(node(&PASSIVE_SINK));
    p.link(split.src(OUT), a.sink(IN)).unwrap();
    p.link(split.src(OUT_B), b.sink(IN)).unwrap();
    match graph_err(p.run()) {
        GraphError::PassiveFanOut { element, upstream } => {
            assert_eq!(upstream, split);
            assert!(element == a || element == b, "{element:?}");
        }
        other => panic!("expected a passive fan-out, got {other:?}"),
    }
}

#[test]
fn run_a_branch_from_a_non_tail_element() {
    let mut p = Pipeline::new();
    let split = p.add(node(&SPLIT));
    let inline = p.add(node(&PASSIVE_SINK));
    let own = p.add(node(&ACTIVE_SINK));
    p.link(split.src(OUT), inline.sink(IN)).unwrap();
    p.link(split.src(OUT_B), own.sink(IN)).unwrap();
    assert_eq!(graph_err(p.run()), GraphError::NonTailBranch { src: split, sink: own });
}

#[test]
fn a_run_that_failed_in_setup_does_not_hold_the_command_log() {
    let mut p = Pipeline::new();
    let split = p.add(node(&SPLIT));
    let inline = p.add(node(&PASSIVE_SINK));
    let own = p.add(node(&ACTIVE_SINK));
    p.link(split.src(OUT), inline.sink(IN)).unwrap();
    p.link(split.src(OUT_B), own.sink(IN)).unwrap();
    assert!(matches!(graph_err(p.run()), GraphError::NonTailBranch { .. }));

    let (pause, control) = (p.control(), p.control());
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..profluens_core::command::COMMAND_LOG_CAP {
            pause.pause();
            pause.resume();
        }
        let _ = done_tx.send(());
    });
    let waited = done.recv_timeout(std::time::Duration::from_secs(20));
    assert!(waited.is_ok(), "a handle waited on a group that never started");
    let last = 2 * profluens_core::command::COMMAND_LOG_CAP as u64 - 1;
    assert!(control.applied_by_all(last), "no group is live, so nothing is outstanding");
}

#[test]
fn set_on_an_unknown_element() {
    let mut p = Pipeline::new();
    p.add(node(&KNOB));
    let ghost = ElementId(99);
    let want = PropError::UnknownElement { element: ghost };
    assert_eq!(prop_err(p.set(ghost, "volume", Value::Int(1))), want);
    assert_eq!(prop_err(p.set_str(ghost, "path", "/x")), want);
    assert_eq!(prop_err(p.prop_handle().set(ghost, "volume", Value::Int(1))), want);
}

#[test]
fn set_an_unknown_property() {
    let mut p = Pipeline::new();
    let knob = p.add(node(&KNOB));
    assert_eq!(
        prop_err(p.set(knob, "gain", Value::Int(1))),
        PropError::UnknownProperty { element: knob, element_name: "eknob", prop: "gain".into() }
    );
}

#[test]
fn set_a_value_the_constraint_rejects() {
    let mut p = Pipeline::new();
    let knob = p.add(node(&KNOB));
    let want = RejectedValue {
        element: knob,
        element_name: "eknob",
        prop: "volume",
        index: 0,
        value: Value::Int(11),
        allowed: KNOB_PROPS[0].allowed.clone(),
    };
    assert_eq!(prop_err(p.set(knob, "volume", Value::Int(11))), PropError::Rejected(want.into()));
    assert_eq!(p.prop_value(knob, "volume"), None, "a rejected set stores nothing");
}

#[test]
fn set_a_structural_property_while_playing() {
    let mut p = Pipeline::new();
    let knob = p.add(node(&KNOB));
    assert_eq!(
        prop_err(p.prop_handle().set(knob, "path", Value::Int(1))),
        PropError::Structural { element: knob, element_name: "eknob", prop: "path", index: 1 }
    );
    p.set(knob, "path", Value::Int(1)).expect("between runs a structural set is fine");
}

fn src_to_sink(src: Node) -> (Pipeline, ElementId, ElementId) {
    let mut p = Pipeline::new();
    let s = p.add(src);
    let k = p.add(node(&ACTIVE_SINK));
    p.link(s.src(OUT), k.sink(IN)).unwrap();
    (p, s, k)
}

fn last_bus_error(p: &Pipeline) -> Option<(ElementId, Error)> {
    let mut last = None;
    while let Some(msg) = p.bus().try_recv() {
        if let BusMessage::Error { element, error } = msg {
            last = Some((element, error));
        }
    }
    last
}

#[test]
fn run_twice_finds_the_elements_consumed() {
    let (mut p, src, _) = src_to_sink(node(&SRC));
    p.run().expect("the first run drains to EOS");
    assert_eq!(graph_err(p.run()), GraphError::ElementConsumed { element: src });
}

#[test]
fn a_reactor_that_cannot_be_built_is_an_io_error() {
    let (mut p, _, _) = src_to_sink(node(&SRC));
    p.set_reactor_factory(Arc::new(|| Err(profluens_core::io::IoError::Denied)));
    p.run().expect("a group that submits nothing builds no reactor");
    let (mut p, _, _) = src_to_sink(Node { desc: &SRC, act: Act::Io });
    p.set_reactor_factory(Arc::new(|| Err(profluens_core::io::IoError::Denied)));
    let err = p.run().unwrap_err();
    assert!(
        matches!(
            err,
            Error::Io { what: "reactor init", kind: profluens_core::io::IoError::Denied }
        ),
        "{err:?}"
    );
}

#[test]
fn an_io_element_with_no_reactor_factory_fails_the_build_naming_the_first() {
    let mut p = Pipeline::new();
    let s = p.add(Node { desc: &IO_SRC, act: Act::Io });
    let k = p.add(node(&IO_SINK));
    p.link(s.src(OUT), k.sink(IN)).unwrap();
    let err = p.run().unwrap_err();
    assert!(
        matches!(err, Error::NoReactor { element, element_name: "eiosrc" } if element == s),
        "{err:?}"
    );
    assert!(err.to_string().contains("'eiosrc' (0) does IO"), "{err}");
    let clock: Arc<dyn profluens_core::clock::Clock> =
        Arc::new(profluens_core::clock::MockClock::new());
    let stepped = p.step(clock).err().expect("the stepped build refuses too");
    assert!(matches!(stepped, Error::NoReactor { element, .. } if element == s), "{stepped:?}");
}

#[test]
fn an_unlinked_io_element_needs_no_reactor_and_no_io_element_runs_without_one() {
    let (mut p, _, _) = src_to_sink(node(&SRC));
    p.add(node(&IO_SINK));
    p.run().expect("the io element takes no part, so no reactor is asked for");
    let (mut p, _, _) = src_to_sink(node(&SRC));
    p.run().expect("no io element, no factory");
}

#[test]
fn a_panicking_group_is_reported_as_such() {
    let (mut p, _, _) = src_to_sink(Node { desc: &SRC, act: Act::Panic });
    let err = p.run().unwrap_err();
    assert!(matches!(err, Error::GroupPanicked { .. }), "{err:?}");
    let (element, error) = last_bus_error(&p).expect("posted");
    assert_eq!(element, ElementId(u32::MAX));
    assert!(matches!(error, Error::GroupPanicked { .. }), "{error:?}");
}
