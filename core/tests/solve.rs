//! The whole-graph format solve through the pipeline's API.

use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::clock::MockClock;
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::{Error, GraphError, LinkError, SolveError};
use profluens_core::event::Event;
use profluens_core::format::{
    Constraint, ConstraintDesc, ConstraintSpec, FieldDesc, FieldSpec, OfferDesc, Value, ValueDesc,
    ValueSpec,
};
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

use crate::link_leak::alloc_calls;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

type Menu = &'static [OfferDesc];

fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

fn ints(vs: &[i64]) -> ConstraintDesc {
    ConstraintDesc::Set(leak(vs.iter().map(|v| ValueDesc::Int(*v)).collect()))
}

fn range(min: i64, max: i64, step: i64) -> ConstraintDesc {
    let (min, max, step) = (ValueDesc::Int(min), ValueDesc::Int(max), ValueDesc::Int(step));
    ConstraintDesc::Range { min, max, step }
}

fn offer(family: &'static str, fields: &[(&'static str, ConstraintDesc)]) -> OfferDesc {
    let fields = fields.iter().map(|(field, allowed)| FieldDesc {
        field,
        allowed: *allowed,
        preferred: None,
    });
    OfferDesc { family, fields: leak(fields.collect()) }
}

fn menu(offers: &[OfferDesc]) -> Menu {
    leak(offers.to_vec())
}

fn audio(fields: &[(&'static str, ConstraintDesc)]) -> Menu {
    menu(&[offer("audio/raw", fields)])
}

static WILD: [OfferDesc; 1] = [OfferDesc::wildcard()];
static RATE_PROP: [PropDesc; 1] =
    [PropDesc { name: "rate", allowed: Constraint::Any, live: false }];

struct Node {
    desc: &'static ElementDesc,
    presets: &'static [(&'static str, ValueDesc)],
    announce: Option<i64>,
    heard: Arc<Mutex<Vec<i64>>>,
}

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn presets(&self) -> &[(&'static str, ValueDesc)] {
        self.presets
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.desc.sinks.is_empty() {
            let Some(rate) = self.announce.take() else {
                return Ok(Flow::Eos);
            };
            ctx.announce_format(OUT, "audio/raw", &[("rate", ValueDesc::Int(rate))]);
            let buf = ctx.alloc();
            ctx.out(OUT).push(buf);
            return Ok(Flow::Ok);
        }
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        let Event::FormatChange(format) = event else {
            return Ok(());
        };
        if let Some(Value::Int(r)) = ctx.field_id("rate").and_then(|f| format.get(f)) {
            self.heard.lock().unwrap().push(r);
        }
        Ok(())
    }
}

fn add(
    p: &mut Pipeline,
    name: &'static str,
    sinks: &[Menu],
    srcs: &[Menu],
    couplings: &[Coupling],
) -> ElementId {
    add_with(p, name, (sinks, srcs), couplings, (&[], &[]))
}

fn add_with(
    p: &mut Pipeline,
    name: &'static str,
    (sinks, srcs): (&[Menu], &[Menu]),
    couplings: &[Coupling],
    (props, presets): (&'static [PropDesc], &'static [(&'static str, ValueDesc)]),
) -> ElementId {
    let names = ["sink", "sink_1", "sink_2", "src", "src_1", "src_2", "src_3", "src_4"];
    let pads = |menus: &[Menu], at: usize| {
        leak(
            menus
                .iter()
                .enumerate()
                .map(|(i, m)| PadDesc { name: names[at + i], offers: m })
                .collect(),
        )
    };
    let desc = Box::leak(Box::new(ElementDesc {
        name,
        sinks: pads(sinks, 0),
        srcs: pads(srcs, 3),
        props,
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: leak(couplings.to_vec()),
    }));
    p.add(Node { desc, presets, announce: None, heard: Arc::default() })
}

fn int(p: &Pipeline, link: LinkId, field: &str) -> Option<i64> {
    match p.negotiated(link)?.get(p.field_id(field)?)? {
        Value::Int(i) => Some(i),
        _ => None,
    }
}

fn family(p: &Pipeline, link: LinkId) -> &str {
    p.negotiated(link).and_then(|f| p.family_name(f.family)).unwrap_or("?")
}

fn solve_err(r: Result<(), Error>) -> SolveError {
    match r {
        Err(Error::Solve(e)) => e,
        other => panic!("expected a solve error, got {other:?}"),
    }
}

#[test]
fn a_sink_two_hops_down_fixes_the_field_a_same_rule_carries() {
    // An encoder whose width follows its input, behind a scaler that takes any width.
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("width", range(16, 4096, 16))])], &[]);
    let enc = add(
        &mut p,
        "enc",
        &[audio(&[("width", ConstraintDesc::Any)])],
        &[audio(&[("width", ConstraintDesc::Any)])],
        &[Coupling::same(0, 0, "width")],
    );
    let pay = add(&mut p, "pay", &[audio(&[("width", ints(&[1280]))])], &[], &[]);
    let a = p.link(src.src(OUT), enc.sink(IN)).unwrap();
    assert_eq!(int(&p, a, "width"), Some(16), "alone, the range's min");
    let b = p.link(enc.src(OUT), pay.sink(IN)).unwrap();
    assert_eq!(
        (int(&p, a, "width"), int(&p, b, "width")),
        (Some(1280), Some(1280)),
        "the sink's width reaches the source"
    );
    p.solve_check().unwrap();
}

#[test]
fn a_tee_meets_its_branches_at_its_sink() {
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[32_000, 44_100, 48_000]))])], &[]);
    let tee = add(&mut p, "tee", &[&WILD], &[&WILD, &WILD], &[]);
    let a = add(&mut p, "a", &[audio(&[("rate", ints(&[44_100, 48_000]))])], &[], &[]);
    let b = add(&mut p, "b", &[audio(&[("rate", ints(&[48_000, 96_000]))])], &[], &[]);
    let up = p.link(src.src(OUT), tee.sink(IN)).unwrap();
    let left = p.link(tee.src(SrcPad(0)), a.sink(IN)).unwrap();
    assert_eq!(int(&p, up, "rate"), Some(44_100));
    let right = p.link(tee.src(SrcPad(1)), b.sink(IN)).unwrap();
    let rates = [up, left, right].map(|l| int(&p, l, "rate"));
    assert_eq!(rates, [Some(48_000); 3], "the one rate both branches take");
    let c = add(&mut p, "c", &[audio(&[("rate", ints(&[32_000]))])], &[], &[]);
    let refused = p.link(tee.src(SrcPad(1)), c.sink(IN));
    assert!(matches!(refused, Err(Error::Link(LinkError::NoCommonFormat(_)))), "{refused:?}");
}

#[test]
fn a_source_with_two_families_links_through_a_transport_by_its_second() {
    let mut p = Pipeline::new();
    let src = add(
        &mut p,
        "src",
        &[],
        &[menu(&[OfferDesc::any("video/raw"), OfferDesc::any("audio/raw")])],
        &[],
    );
    let q = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    let sink = add(&mut p, "sink", &[menu(&[OfferDesc::any("audio/raw")])], &[], &[]);
    let a = p.link(src.src(OUT), q.sink(IN)).unwrap();
    assert_eq!(family(&p, a), "video/raw");
    let b = p.link(q.src(OUT), sink.sink(IN)).expect("the source's second family reaches the sink");
    assert_eq!((family(&p, a), family(&p, b)), ("audio/raw", "audio/raw"));
}

#[test]
fn a_prop_rule_narrows_by_the_property_once_set_and_not_before() {
    let mut p = Pipeline::new();
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let any = || audio(&[("rate", ConstraintDesc::Any)]);
    let rs = add_with(
        &mut p,
        "resample",
        (&[any()], &[any()]),
        &[Coupling::prop(0, "rate", "rate")],
        (&RATE_PROP, &[]),
    );
    let sink = add(&mut p, "sink", &[rates()], &[], &[]);
    p.link(src.src(OUT), rs.sink(IN)).unwrap();
    let out = p.link(rs.src(OUT), sink.sink(IN)).unwrap();
    assert_eq!(int(&p, out, "rate"), Some(44_100), "unset, the pad's own offer");
    p.set(rs, "rate", Value::Int(48_000)).unwrap();
    assert_eq!(int(&p, out, "rate"), Some(44_100), "link time is done");
    let dot = p.dump_dot();
    assert!(dot.contains("provisional"), "{dot}");
    p.run().unwrap();
    assert_eq!(int(&p, out, "rate"), Some(48_000), "the run solved with the property");
    let dot = p.dump_dot();
    assert!(dot.contains("rate: only value") && !dot.contains("provisional"), "{dot}");
    // A value no peer takes fails the next run.
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let rs = add_with(
        &mut p,
        "resample",
        (&[any()], &[any()]),
        &[Coupling::prop(0, "rate", "rate")],
        (&RATE_PROP, &[]),
    );
    let sink = add(&mut p, "sink", &[rates()], &[], &[]);
    p.link(src.src(OUT), rs.sink(IN)).unwrap();
    p.link(rs.src(OUT), sink.sink(IN)).unwrap();
    p.set(rs, "rate", Value::Int(96_000)).unwrap();
    let e = solve_err(p.run());
    assert!(matches!(&e, SolveError::Empty(x) if x.field.as_deref() == Some("rate")), "{e}");
}

#[test]
fn a_preset_is_the_constructors_value_and_a_set_wins_over_it() {
    static PRESET: [(&str, ValueDesc); 1] = [("rate", ValueDesc::Int(48_000))];
    let mut p = Pipeline::new();
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let any = || audio(&[("rate", ConstraintDesc::Any)]);
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let rs = add_with(
        &mut p,
        "resample",
        (&[any()], &[any()]),
        &[Coupling::prop(0, "rate", "rate")],
        (&RATE_PROP, &PRESET),
    );
    let sink = add(&mut p, "sink", &[rates()], &[], &[]);
    assert_eq!(p.prop_value(rs, "rate"), Some(Value::Int(48_000)));
    p.link(src.src(OUT), rs.sink(IN)).unwrap();
    let out = p.link(rs.src(OUT), sink.sink(IN)).unwrap();
    assert_eq!(int(&p, out, "rate"), Some(48_000), "the preset narrows at link time");
    p.set(rs, "rate", Value::Int(44_100)).unwrap();
    p.solve_check().unwrap();
    p.run().unwrap();
    assert_eq!(int(&p, out, "rate"), Some(44_100), "the set won");
}

#[test]
fn fixed_and_copy_rules_write_their_field() {
    let mut p = Pipeline::new();
    let src = add(
        &mut p,
        "src",
        &[],
        &[audio(&[("channels", ints(&[6])), ("rate", ints(&[44_100]))])],
        &[],
    );
    let fields = || {
        audio(&[
            ("channels", ints(&[1, 2, 6])),
            ("rate", ConstraintDesc::Any),
            ("clock", ConstraintDesc::Any),
        ])
    };
    let rules =
        [Coupling::fixed(0, "channels", ValueDesc::Int(2)), Coupling::copy(0, 0, "clock", "rate")];
    let mix = add(&mut p, "stereo", &[fields()], &[fields()], &rules);
    let sink = add(&mut p, "sink", &[fields()], &[], &[]);
    p.link(src.src(OUT), mix.sink(IN)).unwrap();
    let out = p.link(mix.src(OUT), sink.sink(IN)).unwrap();
    assert_eq!((int(&p, out, "channels"), int(&p, out, "clock")), (Some(2), Some(44_100)));
    let mono = add(&mut p, "mono", &[audio(&[("channels", ints(&[1]))])], &[], &[]);
    let mix2 = add(&mut p, "stereo", &[fields()], &[fields()], &rules);
    p.link(src.src(OUT), mix2.sink(IN)).unwrap();
    assert!(p.link(mix2.src(OUT), mono.sink(IN)).is_err(), "a fixed 2 is not 1");
}

#[test]
fn a_filter_pins_the_edge_and_gates_no_announcement() {
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let sink = add(&mut p, "sink", &[rates()], &[], &[]);
    let pin = [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(48_000)),
        preferred: None,
    }];
    let link = p.link_filtered(src.src(OUT), sink.sink(IN), &pin).unwrap();
    assert_eq!(int(&p, link, "rate"), Some(48_000), "without the filter 44.1k");
    assert!(p.dump_dot().contains("[rate=48000]"));
    let heard = Arc::new(Mutex::new(Vec::new()));
    let mut p = Pipeline::new();
    let desc =
        |name, sinks: &'static [PadDesc], srcs: &'static [PadDesc]| -> &'static ElementDesc {
            Box::leak(Box::new(ElementDesc {
                name,
                sinks,
                srcs,
                props: &[],
                sched: SchedHint::Active,
                inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
                latency: LatencyDesc::NONE,
                make_default: None,
                io: false,
                couplings: &[],
            }))
        };
    let pads = |name| leak(vec![PadDesc { name, offers: rates() }]);
    let talker = Node {
        desc: desc("src", &[], pads("src")),
        presets: &[],
        announce: Some(44_100),
        heard: Arc::default(),
    };
    let src = p.add(talker);
    let sink = p.add(Node {
        desc: desc("sink", pads("sink"), &[]),
        presets: &[],
        announce: None,
        heard: Arc::clone(&heard),
    });
    p.link_filtered(src.src(OUT), sink.sink(IN), &pin).unwrap();
    p.run().expect("an announcement outside the filter still plays");
    assert_eq!(*heard.lock().unwrap(), [44_100]);
}

#[test]
fn a_filter_that_admits_nothing_is_refused_by_name() {
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[44_100, 48_000]))])], &[]);
    let sink = add(&mut p, "sink", &[audio(&[("rate", ints(&[44_100, 48_000]))])], &[], &[]);
    let pin = [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(96_000)),
        preferred: None,
    }];
    let e = p.link_filtered(src.src(OUT), sink.sink(IN), &pin).unwrap_err();
    let Error::Solve(SolveError::FilterRejected(r)) = &e else { panic!("{e:?}") };
    assert_eq!((r.src.element, r.sink.element, &*r.filter), (src, sink, "rate=96000"));
    assert!(e.to_string().contains("filter [rate=96000]"), "{e}");
    assert!(p.negotiated(LinkId(0)).is_none(), "nothing was linked");
}

#[test]
fn a_relink_keeps_its_filter_and_an_abandoned_unlink_restores_it() {
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let a = add(&mut p, "a", &[rates()], &[], &[]);
    let b = add(&mut p, "b", &[rates()], &[], &[]);
    let pin = [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(48_000)),
        preferred: None,
    }];
    let link = p.link_filtered(src.src(OUT), a.sink(IN), &pin).unwrap();
    p.relink(link, b.sink(IN)).unwrap();
    assert_eq!(int(&p, link, "rate"), Some(48_000), "relinked under its filter");
    let _run = p.step(Arc::new(MockClock::new())).expect("a run");
    p.unlink(link).unwrap();
    assert!(p.negotiated(link).is_none());
    p.abandon();
    assert_eq!(int(&p, link, "rate"), Some(48_000));
    assert!(p.dump_dot().contains("[rate=48000]"), "the filter came back with the edge");
}

fn pin_48k() -> [FieldSpec<'static>; 1] {
    [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(48_000)),
        preferred: None,
    }]
}

#[test]
fn a_filter_into_a_wildcard_sink_pins_the_edge() {
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let queue = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    let pin = [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(48_000)),
        preferred: None,
    }];
    let link = p
        .link_filtered(src.src(OUT), queue.sink(IN), &pin)
        .unwrap_or_else(|e| panic!("refused: {e:?}"));
    assert_eq!(int(&p, link, "rate"), Some(48_000));
    let sink = add(&mut p, "sink", &[rates()], &[], &[]);
    let out = p.link(queue.src(OUT), sink.sink(IN)).unwrap();
    assert_eq!([link, out].map(|l| int(&p, l, "rate")), [Some(48_000); 2]);
    p.solve_check().unwrap();
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let queue = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    let sink = add(&mut p, "sink", &[rates()], &[], &[]);
    let out = p.link(queue.src(OUT), sink.sink(IN)).unwrap();
    let link = p.link_filtered(src.src(OUT), queue.sink(IN), &pin_48k()).unwrap();
    assert_eq!([link, out].map(|l| int(&p, l, "rate")), [Some(48_000); 2]);
    p.solve_check().unwrap();
    assert_eq!(
        [link, out].map(|l| int(&p, l, "rate")),
        [Some(48_000); 2],
        "the run start solve agrees"
    );
}

#[test]
fn a_filter_into_a_tee_pins_both_branches() {
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    for late in [true, false] {
        let mut p = Pipeline::new();
        let src = add(&mut p, "src", &[], &[rates()], &[]);
        let tee = add(&mut p, "tee", &[&WILD], &[&WILD, &WILD], &[]);
        let (a, b) =
            (add(&mut p, "a", &[rates()], &[], &[]), add(&mut p, "b", &[rates()], &[], &[]));
        let up = late.then(|| p.link_filtered(src.src(OUT), tee.sink(IN), &pin_48k()).unwrap());
        let left = p.link(tee.src(SrcPad(0)), a.sink(IN)).unwrap();
        let right = p.link(tee.src(SrcPad(1)), b.sink(IN)).unwrap();
        let up =
            up.unwrap_or_else(|| p.link_filtered(src.src(OUT), tee.sink(IN), &pin_48k()).unwrap());
        assert_eq!([up, left, right].map(|l| int(&p, l, "rate")), [Some(48_000); 3], "late {late}");
        p.solve_check().unwrap();
    }
}

#[test]
fn a_filter_past_a_wildcard_transport_reaches_the_source() {
    // The source's edge takes the filter's rate through the queue.
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let queue = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    let tee = add(&mut p, "tee", &[&WILD], &[&WILD], &[]);
    let up = p.link(src.src(OUT), queue.sink(IN)).unwrap();
    assert_eq!(int(&p, up, "rate"), Some(44_100), "alone, the first offered");
    let mid = p
        .link_filtered(queue.src(OUT), tee.sink(IN), &pin_48k())
        .unwrap_or_else(|e| panic!("refused: {e:?}"));
    assert_eq!([up, mid].map(|l| int(&p, l, "rate")), [Some(48_000); 2]);
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let queue = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    let tee = add(&mut p, "tee", &[&WILD], &[&WILD], &[]);
    let mid = p
        .link_filtered(queue.src(OUT), tee.sink(IN), &pin_48k())
        .unwrap_or_else(|e| panic!("refused: {e:?}"));
    assert_eq!(int(&p, mid, "rate"), None, "nothing to fix yet");
    let up = p.link(src.src(OUT), queue.sink(IN)).unwrap();
    assert_eq!([up, mid].map(|l| int(&p, l, "rate")), [Some(48_000); 2]);
    let only = add(&mut p, "only", &[], &[audio(&[("rate", ints(&[44_100]))])], &[]);
    let (q2, t2) = (
        add(&mut p, "queue", &[&WILD], &[&WILD], &[]),
        add(&mut p, "tee", &[&WILD], &[&WILD], &[]),
    );
    p.link_filtered(q2.src(OUT), t2.sink(IN), &pin_48k()).unwrap();
    let e = p.link(only.src(OUT), q2.sink(IN)).unwrap_err();
    assert!(matches!(e, Error::Link(LinkError::NoCommonFormat(_))), "{e:?}");
}

#[test]
fn two_filters_across_wildcards_both_reach_the_source() {
    let mut p = Pipeline::new();
    let src = add(
        &mut p,
        "src",
        &[],
        &[audio(&[("rate", ints(&[44_100, 48_000])), ("channels", ints(&[1, 2]))])],
        &[],
    );
    let (q1, q2) = (
        add(&mut p, "queue", &[&WILD], &[&WILD], &[]),
        add(&mut p, "queue", &[&WILD], &[&WILD], &[]),
    );
    let tee = add(&mut p, "tee", &[&WILD], &[&WILD], &[]);
    let stereo = [FieldSpec {
        field: "channels",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(2)),
        preferred: None,
    }];
    let a = p.link_filtered(q1.src(OUT), q2.sink(IN), &pin_48k()).unwrap();
    let b = p.link_filtered(q2.src(OUT), tee.sink(IN), &stereo).unwrap();
    let up = p.link(src.src(OUT), q1.sink(IN)).unwrap();
    let fixed = [up, a, b].map(|l| (int(&p, l, "rate"), int(&p, l, "channels")));
    assert_eq!(fixed, [(Some(48_000), Some(2)); 3]);
}

#[test]
fn a_filter_a_property_breaks_past_a_wildcard_names_the_field_at_run_start() {
    let mut p = Pipeline::new();
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let any = || audio(&[("rate", ConstraintDesc::Any)]);
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let rs = add_with(
        &mut p,
        "resample",
        (&[any()], &[any()]),
        &[Coupling::prop(0, "rate", "rate")],
        (&RATE_PROP, &[]),
    );
    let queue = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    p.link(src.src(OUT), rs.sink(IN)).unwrap();
    let out = p.link_filtered(rs.src(OUT), queue.sink(IN), &pin_48k()).unwrap();
    assert_eq!(int(&p, out, "rate"), Some(48_000));
    p.set(rs, "rate", Value::Int(44_100)).unwrap();
    let e = solve_err(p.solve_check());
    assert!(
        matches!(&e, SolveError::Empty(x) if x.link == out && x.field.as_deref() == Some("rate")),
        "{e}"
    );
}

#[test]
fn an_element_wildcard_that_names_fields_meets_nothing() {
    static BAD: [OfferDesc; 1] = [OfferDesc {
        family: "*",
        fields: &[FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None }],
    }];
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[48_000]))])], &[]);
    let bad = add(&mut p, "bad", &[&BAD], &[], &[]);
    let queue = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    for up in [src.src(OUT), queue.src(OUT)] {
        let e = p.link(up, bad.sink(IN)).unwrap_err();
        assert!(matches!(e, Error::Link(LinkError::NoCommonFormat(_))), "not a filter, {e:?}");
    }
}

#[test]
fn a_run_start_failure_names_the_link_the_field_and_the_path() {
    let mut p = Pipeline::new();
    let rates = || audio(&[("rate", ints(&[44_100, 48_000]))]);
    let any = || audio(&[("rate", ConstraintDesc::Any)]);
    let src = add(&mut p, "src", &[], &[rates()], &[]);
    let rs = add_with(
        &mut p,
        "resample",
        (&[any()], &[any()]),
        &[Coupling::prop(0, "rate", "rate")],
        (&RATE_PROP, &[]),
    );
    let gain = add(&mut p, "gain", &[any()], &[any()], &[Coupling::same(0, 0, "rate")]);
    let sink = add(&mut p, "sink", &[rates()], &[], &[]);
    p.link(src.src(OUT), rs.sink(IN)).unwrap();
    let mid = p.link(rs.src(OUT), gain.sink(IN)).unwrap();
    p.link(gain.src(OUT), sink.sink(IN)).unwrap();
    p.set(rs, "rate", Value::Int(96_000)).unwrap();
    let e = solve_err(p.solve_check());
    let SolveError::Empty(x) = &e else { panic!("{e}") };
    assert_eq!((x.link, x.field.as_deref()), (mid, Some("rate")));
    let rules: Vec<(&str, &str, &str)> =
        x.path.iter().map(|n| (n.element_name, n.rule, &*n.constraint)).collect();
    assert_eq!(
        rules,
        [
            ("resample", "prop", "96000"),
            ("gain", "same", "{44100, 48000}"),
            ("gain", "peer", "{44100, 48000}")
        ],
        "{e}"
    );
    let text = e.to_string();
    assert!(
        text.contains("no common value of rate") && text.contains("resample.src (prop = 96000)"),
        "{text}"
    );
    assert!(
        matches!(p.run(), Err(Error::Solve(SolveError::Empty(_)))),
        "the run is refused the same way"
    );
}

#[test]
fn too_many_fields_and_no_common_family_are_told_apart() {
    let names: Vec<&'static str> =
        (0..9).map(|i| &*Box::leak(format!("f{i}").into_boxed_str())).collect();
    let one = |names: &[&'static str]| {
        audio(
            &names.iter().map(|n| (*n, ConstraintDesc::Eq(ValueDesc::Int(1)))).collect::<Vec<_>>(),
        )
    };
    let mut p = Pipeline::new();
    let wide_a: Vec<&'static str> = names.clone();
    let wide_b: Vec<&'static str> =
        (0..9).map(|i| &*Box::leak(format!("g{i}").into_boxed_str())).collect();
    let src = add(&mut p, "src", &[], &[one(&wide_a)], &[]);
    let sink = add(&mut p, "sink", &[one(&wide_b)], &[], &[]);
    let e = p.link(src.src(OUT), sink.sink(IN)).unwrap_err();
    assert!(
        matches!(e, Error::Solve(SolveError::TooManyFields { .. })),
        "18 fields cannot be fixed, {e:?}"
    );
    let video = add(&mut p, "video", &[menu(&[OfferDesc::any("video/raw")])], &[], &[]);
    let e = p.link(src.src(OUT), video.sink(IN)).unwrap_err();
    assert!(matches!(e, Error::Link(LinkError::NoCommonFormat(_))), "{e:?}");
}

#[test]
fn downstream_alternatives_that_disagree_leave_the_choice_to_list_order() {
    // The converter keeps the rate, so the solve picks by order.
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[48_000, 44_100]))])], &[]);
    let any = audio(&[("rate", ConstraintDesc::Any)]);
    let two = menu(&[
        offer("audio/raw", &[("rate", ConstraintDesc::Any)]),
        offer("audio/float", &[("rate", ConstraintDesc::Any)]),
    ]);
    let conv = add(&mut p, "conv", &[any], &[two], &[Coupling::same(0, 0, "rate")]);
    let sink = add(
        &mut p,
        "sink",
        &[menu(&[
            offer("audio/raw", &[("rate", ints(&[44_100]))]),
            offer("audio/float", &[("rate", ints(&[48_000]))]),
        ])],
        &[],
        &[],
    );
    let a = p.link(src.src(OUT), conv.sink(IN)).unwrap();
    let b = p.link(conv.src(OUT), sink.sink(IN)).unwrap();
    assert_eq!(
        (int(&p, a, "rate"), family(&p, b), int(&p, b, "rate")),
        (Some(48_000), "audio/float", Some(48_000))
    );
}

#[test]
fn a_bad_coupling_is_refused_naming_the_rule() {
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[])], &[]);
    let bad = add(
        &mut p,
        "bad",
        &[audio(&[])],
        &[audio(&[])],
        &[Coupling::through(0, 0), Coupling::prop(0, "rate", "gain")],
    );
    let e = p.link(src.src(OUT), bad.sink(IN)).unwrap_err();
    let Error::Solve(SolveError::BadCoupling { element, index, why, .. }) = e else {
        panic!("{e:?}")
    };
    assert_eq!((element, index, why), (bad, 1, "two rules write one field"));
}

#[test]
fn a_probed_element_never_prerolled_is_refused_at_run_start() {
    let mut p = Pipeline::new();
    let src = add(&mut p, "cam", &[], &[audio(&[]), OfferDesc::PROBED], &[]);
    let sink = add(&mut p, "sink", &[audio(&[])], &[], &[]);
    p.link(src.src(OUT), sink.sink(IN)).unwrap();
    assert_eq!(
        solve_err_graph(p.solve_check()),
        GraphError::NotPrerolled { element: src, element_name: "cam" }
    );
    p.preroll().unwrap();
    p.run().expect("prerolled, the probe that offered nothing is left unlinked");
}

fn solve_err_graph(r: Result<(), Error>) -> GraphError {
    match r {
        Err(Error::Graph(g)) => g,
        other => panic!("expected a graph error, got {other:?}"),
    }
}

/// A chain of `n` elements alternating `through` and `same` rules.
pub(crate) fn chain(n: usize) -> Pipeline {
    let mut p = Pipeline::new();
    let fields = |rate| {
        audio(&[("rate", rate), ("channels", ints(&[1, 2])), ("sample", ConstraintDesc::Any)])
    };
    let mut up = add(&mut p, "src", &[], &[fields(ints(&[44_100, 48_000]))], &[]);
    for i in 1..n - 1 {
        let rule = if i % 2 == 0 { Coupling::through(0, 0) } else { Coupling::same(0, 0, "rate") };
        let t = add(
            &mut p,
            "t",
            &[fields(ConstraintDesc::Any)],
            &[fields(ConstraintDesc::Any)],
            &[rule],
        );
        p.link(up.src(OUT), t.sink(IN)).unwrap();
        up = t;
    }
    let sink = add(&mut p, "sink", &[fields(ints(&[48_000]))], &[], &[]);
    p.link(up.src(OUT), sink.sink(IN)).unwrap();
    p
}

#[test]
fn the_solve_allocates_nothing_after_the_first() {
    let mut p = chain(100);
    p.solve_check().unwrap();
    p.solve_check().unwrap();
    let before = alloc_calls();
    p.solve_check().unwrap();
    assert_eq!(alloc_calls() - before, 0, "a warm solve of a 100-element chain allocates");
    assert!(p.solve_meets() < 5_000, "{} meets", p.solve_meets());
}

#[test]
fn a_typed_passthrough_hands_its_input_on_or_its_bytes_escape() {
    let raw = || menu(&[offer("audio/raw", &[("rate", ints(&[48_000]))]), OfferDesc::any("bytes")]);
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[48_000]))])], &[]);
    let gain = add(&mut p, "gain", &[raw()], &[raw()], &[Coupling::through(0, 0)]);
    let file = add(&mut p, "file", &[menu(&[OfferDesc::any("bytes")])], &[], &[]);
    let a = p.link(src.src(OUT), gain.sink(IN)).unwrap();
    let b = p.link(gain.src(OUT), file.sink(IN)).expect("raw audio leaves as bytes");
    assert_eq!((family(&p, a), family(&p, b)), ("audio/raw", "bytes"));
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[48_000]))])], &[]);
    let gain = add(&mut p, "gain", &[raw()], &[raw()], &[Coupling::through(0, 0)]);
    let sink = add(&mut p, "sink", &[audio(&[("rate", ints(&[44_100]))])], &[], &[]);
    p.link(src.src(OUT), gain.sink(IN)).unwrap();
    assert!(p.link(gain.src(OUT), sink.sink(IN)).is_err(), "a passthrough does not resample");
}

/// A decoder's offer, `rate` as given, then the `bytes` escape.
fn decoder(p: &mut Pipeline, rate: ConstraintDesc) -> ElementId {
    let bytes = menu(&[OfferDesc::any("bytes")]);
    let file = add(p, "file", &[], &[bytes], &[]);
    let dec = add(
        p,
        "dec",
        &[bytes],
        &[menu(&[offer("audio/raw", &[("rate", rate)]), OfferDesc::any("bytes")])],
        &[],
    );
    p.link(file.src(OUT), dec.sink(IN)).unwrap();
    dec
}

fn mix(p: &mut Pipeline) -> ElementId {
    let raw =
        || menu(&[offer("audio/raw", &[("rate", ConstraintDesc::Any)]), OfferDesc::any("bytes")]);
    add(p, "mix", &[raw()], &[raw()], &[Coupling::same(0, 0, "rate")])
}

#[test]
fn a_bytes_escape_does_not_carry_a_refusal_past_a_family_both_pads_name() {
    // Both ends of the first link name `audio/raw`, so it may not fall back to `bytes`.
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, ints(&[48_000]));
    let (m, sink) =
        (mix(&mut p), add(&mut p, "sink", &[audio(&[("rate", ints(&[44_100]))])], &[], &[]));
    let a = p.link(dec.src(OUT), m.sink(IN)).unwrap();
    assert_eq!((family(&p, a), int(&p, a, "rate")), ("audio/raw", Some(48_000)));
    let refused = p.link(m.src(OUT), sink.sink(IN));
    assert!(matches!(refused, Err(Error::Link(LinkError::NoCommonFormat(_)))), "{refused:?}");
    assert_eq!(family(&p, a), "audio/raw", "the refused link moved nothing");
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, ints(&[48_000]));
    let q = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    let (m, sink) =
        (mix(&mut p), add(&mut p, "sink", &[audio(&[("rate", ints(&[44_100]))])], &[], &[]));
    p.link(dec.src(OUT), q.sink(IN)).unwrap();
    p.link(q.src(OUT), m.sink(IN)).unwrap();
    assert!(p.link(m.src(OUT), sink.sink(IN)).is_err(), "a transport carries the refusal");
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, ints(&[48_000]));
    let q = add(&mut p, "queue", &[&WILD], &[&WILD], &[]);
    let file = add(&mut p, "file", &[menu(&[OfferDesc::any("bytes")])], &[], &[]);
    let a = p.link(dec.src(OUT), q.sink(IN)).unwrap();
    p.link(q.src(OUT), file.sink(IN)).expect("raw audio leaves as bytes");
    assert_eq!(family(&p, a), "bytes");
    let mut p = Pipeline::new();
    let demux =
        add(&mut p, "demux", &[], &[menu(&[OfferDesc::any("bytes"), OfferDesc::any("opus")])], &[]);
    let opus = add(
        &mut p,
        "opusdec",
        &[menu(&[OfferDesc::any("opus"), OfferDesc::any("bytes")])],
        &[],
        &[],
    );
    let a = p.link(demux.src(OUT), opus.sink(IN)).unwrap();
    assert_eq!(family(&p, a), "bytes");
    p.solve_check().unwrap();
}

#[test]
fn a_preference_the_peer_lacks_goes_back_with_the_peers_domain() {
    // The meet with the sink keeps the transform's preference, which reaches the source back.
    let liking = leak(vec![FieldDesc {
        field: "rate",
        allowed: ConstraintDesc::Any,
        preferred: Some(ValueDesc::Int(44_100)),
    }]);
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[48_000, 44_100]))])], &[]);
    let t = add(
        &mut p,
        "t",
        &[audio(&[("rate", ConstraintDesc::Any)])],
        &[menu(&[OfferDesc { family: "audio/raw", fields: liking }])],
        &[Coupling::through(0, 0)],
    );
    let sink = add(&mut p, "sink", &[audio(&[("rate", ints(&[48_000, 44_100]))])], &[], &[]);
    let a = p.link(src.src(OUT), t.sink(IN)).unwrap();
    let b = p.link(t.src(OUT), sink.sink(IN)).unwrap();
    p.solve_check().unwrap();
    let rate = |l| p.negotiated(l).and_then(|f| f.get(p.field_id("rate")?));
    assert_eq!((rate(a), rate(b)), (Some(Value::Int(44_100)), Some(Value::Int(44_100))));
}

#[test]
fn a_preference_goes_back_through_a_same_rule() {
    static FORMAT: [PropDesc; 1] =
        [PropDesc { name: "format", allowed: Constraint::Any, live: false }];
    let samples = || {
        ConstraintDesc::Set(leak(["u8", "s16", "s32"].iter().map(|s| ValueDesc::Id(s)).collect()))
    };
    let raw = || audio(&[("sample", samples())]);
    let device = |prefer: &'static str, broad: bool| {
        let fields = leak(vec![FieldDesc {
            field: "sample",
            allowed: ConstraintDesc::Set(&[ValueDesc::Id("s16"), ValueDesc::Id("s32")]),
            preferred: Some(ValueDesc::Id(prefer)),
        }]);
        let mut offers = vec![OfferDesc { family: "audio/raw", fields }];
        if broad {
            offers.push(offer("audio/raw", &[("sample", samples())]));
        }
        menu(&offers)
    };
    for broad in [false, true] {
        let mut p = Pipeline::new();
        let src = add(&mut p, "src", &[], &[audio(&[("sample", ints(&[1]))])], &[]);
        let conv = add_with(
            &mut p,
            "conv",
            (&[menu(&[OfferDesc::any("audio/raw")])], &[raw()]),
            &[Coupling::prop(0, "sample", "format")],
            (&FORMAT, &[]),
        );
        let res = add(&mut p, "res", &[raw()], &[raw()], &[Coupling::same(0, 0, "sample")]);
        let dev = add(&mut p, "dev", &[device("s32", broad)], &[], &[]);
        p.link(src.src(OUT), conv.sink(IN)).unwrap();
        let a = p.link(conv.src(OUT), res.sink(IN)).unwrap();
        let b = p.link(res.src(OUT), dev.sink(IN)).unwrap();
        p.solve_check().unwrap();
        let name = |l| {
            p.negotiated(l).and_then(|f| f.get(p.field_id("sample")?)).and_then(|v| match v {
                Value::Id(v) => p.value_name(v).map(str::to_string),
                _ => None,
            })
        };
        assert_eq!(
            (name(a).as_deref(), name(b).as_deref()),
            (Some("s32"), Some("s32")),
            "broad {broad}: two hops from the device"
        );
    }
}

#[test]
fn a_field_nothing_upstream_states_is_unsettled_where_the_sink_takes_some_values() {
    let names = |p: &Pipeline, l| {
        p.unsettled(l).map(|f| p.field_name(f).unwrap().to_string()).collect::<Vec<_>>()
    };
    let strict = |p: &mut Pipeline, rates: &[i64]| {
        add(p, "sink", &[audio(&[("rate", ints(rates))])], &[], &[])
    };
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, ConstraintDesc::Any);
    let (m, sink) = (mix(&mut p), strict(&mut p, &[44_100]));
    let a = p.link(dec.src(OUT), m.sink(IN)).unwrap();
    let b = p.link(m.src(OUT), sink.sink(IN)).unwrap();
    assert_eq!(
        (names(&p, a), names(&p, b)),
        (vec![], vec!["rate".to_string()]),
        "the mix takes any rate, the sink one"
    );
    p.solve_check().unwrap();
    assert_eq!(names(&p, b), ["rate"], "and the run's solve keeps it");
    assert!(p.dump_dot().contains("unsettled: rate"), "{}", p.dump_dot());
    for (rates, want) in [(&[44_100, 48_000][..], 0), (&[44_100][..], 1)] {
        let mut p = Pipeline::new();
        let dec = decoder(&mut p, ints(&[44_100, 48_000]));
        let sink = strict(&mut p, rates);
        let l = p.link(dec.src(OUT), sink.sink(IN)).unwrap();
        assert_eq!(names(&p, l).len(), want, "{rates:?}");
    }
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[44_100, 48_000]))])], &[]);
    let sink = strict(&mut p, &[44_100]);
    let l = p.link(src.src(OUT), sink.sink(IN)).unwrap();
    assert!(names(&p, l).is_empty(), "a source puts out what it is told");
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, ints(&[44_100]));
    let sink = strict(&mut p, &[44_100]);
    let l = p.link(dec.src(OUT), sink.sink(IN)).unwrap();
    assert!(names(&p, l).is_empty(), "a stated rate");
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, ConstraintDesc::Any);
    let raw =
        || menu(&[offer("audio/raw", &[("rate", ConstraintDesc::Any)]), OfferDesc::any("bytes")]);
    let res = add_with(
        &mut p,
        "res",
        (&[raw()], &[raw()]),
        &[Coupling::prop(0, "rate", "rate")],
        (&RATE_PROP, &[]),
    );
    let gain = add(&mut p, "gain", &[raw()], &[raw()], &[Coupling::through(0, 0)]);
    let (s1, s2) = (strict(&mut p, &[44_100]), strict(&mut p, &[44_100]));
    let tee = add(&mut p, "tee", &[&WILD], &[&WILD, &WILD], &[]);
    p.link(dec.src(OUT), tee.sink(IN)).unwrap();
    p.link(tee.src(SrcPad(0)), res.sink(IN)).unwrap();
    p.link(tee.src(SrcPad(1)), gain.sink(IN)).unwrap();
    let (a, b) =
        (p.link(res.src(OUT), s1.sink(IN)).unwrap(), p.link(gain.src(OUT), s2.sink(IN)).unwrap());
    assert_eq!((names(&p, a), names(&p, b)), (vec![], vec!["rate".to_string()]));
}

/// A decoder that picks `rate` from what downstream takes.
fn picker(p: &mut Pipeline, rate: ConstraintDesc) -> ElementId {
    let bytes = menu(&[OfferDesc::any("bytes")]);
    let file = add(p, "file", &[], &[bytes], &[]);
    let srcs = [menu(&[offer("audio/raw", &[("rate", rate)]), OfferDesc::any("bytes")])];
    let dec = add(p, "dec", &[bytes], &srcs, &[Coupling::takes(0, "rate")]);
    p.link(file.src(OUT), dec.sink(IN)).unwrap();
    dec
}

#[test]
fn a_field_its_element_picks_from_what_downstream_takes_is_settled_where_downstream_takes_one() {
    let names = |p: &Pipeline, l| {
        p.unsettled(l).map(|f| p.field_name(f).unwrap().to_string()).collect::<Vec<_>>()
    };
    let strict = |p: &mut Pipeline, rates: &[i64]| {
        add(p, "sink", &[audio(&[("rate", ints(rates))])], &[], &[])
    };
    let offered = || ints(&[32_000, 44_100, 48_000]);
    let mut p = Pipeline::new();
    let dec = picker(&mut p, offered());
    let sink = strict(&mut p, &[44_100]);
    let l = p.link(dec.src(OUT), sink.sink(IN)).unwrap();
    assert!(names(&p, l).is_empty(), "the decoder announces the one taken");
    let mut p = Pipeline::new();
    let dec = picker(&mut p, offered());
    let (m, sink) = (mix(&mut p), strict(&mut p, &[44_100, 96_000]));
    p.link(dec.src(OUT), m.sink(IN)).unwrap();
    let b = p.link(m.src(OUT), sink.sink(IN)).unwrap();
    p.solve_check().unwrap();
    assert!(names(&p, b).is_empty(), "held to what the sink takes through the mix");
    let mut p = Pipeline::new();
    let dec = picker(&mut p, offered());
    let sink = strict(&mut p, &[96_000]);
    assert!(p.link(dec.src(OUT), sink.sink(IN)).is_err(), "no rate meets, a converter goes in");
    let mut p = Pipeline::new();
    let dec = picker(&mut p, ConstraintDesc::Any);
    let sink = strict(&mut p, &[44_100]);
    let l = p.link(dec.src(OUT), sink.sink(IN)).unwrap();
    assert!(names(&p, l).is_empty(), "any rate, held to the one taken");
}

#[test]
fn a_decoder_link_says_what_downstream_takes_of_a_field_it_announces() {
    let rates = |p: &Pipeline, l| -> Option<Vec<i64>> {
        let vals = p.takes(l, p.field_id("rate")?)?;
        Some(
            vals.iter()
                .map(|v| match v {
                    Value::Int(i) => *i,
                    _ => panic!("{v:?}"),
                })
                .collect(),
        )
    };
    let strict = |p: &mut Pipeline, rates: &[i64]| {
        add(p, "sink", &[audio(&[("rate", ints(rates))])], &[], &[])
    };
    let offered = || ints(&[32_000, 44_100, 48_000]);
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, offered());
    let (m, sink) = (mix(&mut p), strict(&mut p, &[48_000, 44_100]));
    let a = p.link(dec.src(OUT), m.sink(IN)).unwrap();
    p.link(m.src(OUT), sink.sink(IN)).unwrap();
    p.solve_check().unwrap();
    assert_eq!(rates(&p, a), Some(vec![48_000, 44_100]), "the sink's rates reach the decoder");
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, offered());
    let tee = add(&mut p, "tee", &[&WILD], &[&WILD, &WILD], &[]);
    let (s1, s2) = (strict(&mut p, &[44_100, 48_000]), strict(&mut p, &[48_000, 32_000]));
    let a = p.link(dec.src(OUT), tee.sink(IN)).unwrap();
    p.link(tee.src(SrcPad(0)), s1.sink(IN)).unwrap();
    p.link(tee.src(SrcPad(1)), s2.sink(IN)).unwrap();
    p.solve_check().unwrap();
    assert_eq!(rates(&p, a), Some(vec![48_000]), "both branches take 48 kHz alone");
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, offered());
    let any = add(&mut p, "sink", &[audio(&[("rate", ConstraintDesc::Any)])], &[], &[]);
    let a = p.link(dec.src(OUT), any.sink(IN)).unwrap();
    p.solve_check().unwrap();
    assert_eq!(rates(&p, a), None, "any rate");
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", offered())])], &[]);
    let sink = strict(&mut p, &[44_100]);
    let a = p.link(src.src(OUT), sink.sink(IN)).unwrap();
    p.solve_check().unwrap();
    assert_eq!(rates(&p, a), None, "a source puts out what the solve fixed");
}

struct Taker {
    desc: &'static ElementDesc,
    took: Arc<Mutex<Option<Vec<i64>>>>,
}

impl Element for Taker {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let vals = ctx.field_id("rate").and_then(|f| ctx.src_takes(OUT, f));
        let ints = vals.map(|v| {
            v.iter()
                .filter_map(|v| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                })
                .collect()
        });
        *self.took.lock().unwrap() = ints;
        Ok(())
    }
    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {}
        Ok(Flow::Ok)
    }
}

#[test]
fn a_running_decoder_reads_what_every_link_of_its_pad_takes() {
    let bytes = menu(&[OfferDesc::any("bytes")]);
    let mut p = Pipeline::new();
    let file = add(&mut p, "file", &[], &[bytes], &[]);
    let srcs = leak(vec![PadDesc {
        name: "src",
        offers: menu(&[offer("audio/raw", &[("rate", ints(&[32_000, 44_100, 48_000]))])]),
    }]);
    let desc = Box::leak(Box::new(ElementDesc {
        name: "dec",
        sinks: leak(vec![PadDesc { name: "sink", offers: bytes }]),
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }));
    let took = Arc::new(Mutex::new(None));
    let dec = p.add(Taker { desc, took: Arc::clone(&took) });
    p.link(file.src(OUT), dec.sink(IN)).unwrap();
    let tee = add(&mut p, "tee", &[&WILD], &[&WILD, &WILD], &[]);
    let strict = |p: &mut Pipeline, rates: &[i64]| {
        add(p, "sink", &[audio(&[("rate", ints(rates))])], &[], &[])
    };
    let (s1, s2) = (strict(&mut p, &[44_100, 48_000]), strict(&mut p, &[32_000, 48_000, 44_100]));
    p.link(dec.src(OUT), tee.sink(IN)).unwrap();
    p.link(tee.src(SrcPad(0)), s1.sink(IN)).unwrap();
    p.link(tee.src(SrcPad(1)), s2.sink(IN)).unwrap();
    p.run().unwrap();
    assert_eq!(*took.lock().unwrap(), Some(vec![44_100, 48_000]), "what both branches take");
}

fn liking(rates: &[i64], liked: i64) -> Menu {
    let field =
        FieldDesc { field: "rate", allowed: ints(rates), preferred: Some(ValueDesc::Int(liked)) };
    menu(&[OfferDesc { family: "audio/raw", fields: leak(vec![field]) }])
}

#[test]
fn every_typed_link_of_one_src_pad_carries_one_format() {
    // Every link of a pad is handed one format, so a second link taking another rate is refused.
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[44_100, 48_000]))])], &[]);
    let a = add(&mut p, "a", &[audio(&[("rate", ints(&[44_100]))])], &[], &[]);
    let b = add(&mut p, "b", &[audio(&[("rate", ints(&[48_000]))])], &[], &[]);
    let first = p.link(src.src(OUT), a.sink(IN)).unwrap();
    let e = p.link(src.src(OUT), b.sink(IN)).unwrap_err();
    let Error::Link(LinkError::NoCommonFormat(x)) = &e else { panic!("{e:?}") };
    assert!(x.src.bound.as_deref().is_some_and(|b| b.contains("rate=44100")), "{e}");
    assert_eq!(int(&p, first, "rate"), Some(44_100));
    p.solve_check().unwrap();
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[audio(&[("rate", ints(&[44_100, 48_000]))])], &[]);
    let a = add(&mut p, "a", &[liking(&[44_100, 48_000], 48_000)], &[], &[]);
    let b = add(&mut p, "b", &[liking(&[44_100, 48_000], 44_100)], &[], &[]);
    let (l, r) =
        (p.link(src.src(OUT), a.sink(IN)).unwrap(), p.link(src.src(OUT), b.sink(IN)).unwrap());
    assert_eq!(int(&p, l, "rate"), int(&p, r, "rate"));
    p.solve_check().unwrap();
    assert_eq!(int(&p, l, "rate"), int(&p, r, "rate"), "{}", p.dump_dot());
    assert!(int(&p, l, "rate").is_some());
}

#[test]
fn a_bytes_tap_on_a_typed_pad_binds_nothing() {
    // A sink of `bytes` takes an announcement of any family.
    let mut p = Pipeline::new();
    let dec = decoder(&mut p, ints(&[44_100, 48_000]));
    let a = add(&mut p, "a", &[audio(&[("rate", ints(&[48_000]))])], &[], &[]);
    let file = add(&mut p, "file", &[menu(&[OfferDesc::any("bytes")])], &[], &[]);
    let b = add(&mut p, "b", &[audio(&[("rate", ints(&[44_100, 48_000]))])], &[], &[]);
    let typed = p.link(dec.src(OUT), a.sink(IN)).unwrap();
    let tap = p.link(dec.src(OUT), file.sink(IN)).expect("a tap takes the bytes");
    let other = p.link(dec.src(OUT), b.sink(IN)).unwrap();
    p.solve_check().unwrap();
    assert_eq!(
        (family(&p, tap), int(&p, typed, "rate"), int(&p, other, "rate")),
        ("bytes", Some(48_000), Some(48_000))
    );
}

#[test]
fn a_filter_past_sixteen_fields_is_too_many_fields() {
    let names: Vec<&'static str> =
        (0..17).map(|i| &*Box::leak(format!("f{i}").into_boxed_str())).collect();
    let sixteen = audio(&names[..16].iter().map(|n| (*n, ConstraintDesc::Any)).collect::<Vec<_>>());
    let mut p = Pipeline::new();
    let src = add(&mut p, "src", &[], &[sixteen], &[]);
    let sink = add(&mut p, "sink", &[sixteen], &[], &[]);
    let filter = [FieldSpec {
        field: names[16],
        allowed: ConstraintSpec::Eq(ValueSpec::Int(1)),
        preferred: None,
    }];
    let e = p.link_filtered(src.src(OUT), sink.sink(IN), &filter).unwrap_err();
    assert!(matches!(e, Error::Solve(SolveError::TooManyFields { .. })), "{e:?}");
    let mut p = Pipeline::new();
    let one = audio(&[(names[16], ConstraintDesc::Any)]);
    let src = add(&mut p, "src", &[], &[audio(&[(names[16], ints(&[1]))])], &[]);
    let pass = add(&mut p, "pass", &[one], &[sixteen], &[Coupling::through(0, 0)]);
    let sink = add(&mut p, "sink", &[sixteen], &[], &[]);
    p.link(pass.src(OUT), sink.sink(IN)).unwrap();
    let e = p.link(src.src(OUT), pass.sink(IN)).unwrap_err();
    assert!(matches!(e, Error::Solve(SolveError::TooManyFields { .. })), "{e:?}");
}
