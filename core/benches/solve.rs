// A bench's own fixtures, leaked descriptors and names, not an element.
#![allow(clippy::disallowed_methods)]

//! The whole-graph solve, warm `solve_check` per iteration and a relink churn. `--check` asserts
//! budgets.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use profluens_core::batch::Inputs;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::format::{Constraint, ConstraintDesc, FieldDesc, OfferDesc, Value, ValueDesc};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::Stepped;
use profluens_core::time::Timestamp;
use profluens_core::topology::Leave;

const TRIALS: usize = 7;
const WARMUP: usize = 2;

struct Node(&'static ElementDesc);

impl Element for Node {
    fn desc(&self) -> &'static ElementDesc {
        self.0
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Eos)
    }
}

fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

fn name(prefix: &str, i: usize) -> &'static str {
    Box::leak(format!("{prefix}{i}").into_boxed_str())
}

fn ints(vs: &[i64]) -> ConstraintDesc {
    ConstraintDesc::Set(leak(vs.iter().map(|v| ValueDesc::Int(*v)).collect()))
}

fn ids(vs: &[&'static str]) -> ConstraintDesc {
    ConstraintDesc::Set(leak(vs.iter().map(|v| ValueDesc::Id(v)).collect()))
}

fn offer(family: &'static str, fields: &[(&'static str, ConstraintDesc)]) -> OfferDesc {
    let fields = fields.iter().map(|(field, allowed)| FieldDesc {
        field,
        allowed: *allowed,
        preferred: None,
    });
    OfferDesc { family, fields: leak(fields.collect()) }
}

fn menu(offers: Vec<OfferDesc>) -> &'static [OfferDesc] {
    leak(offers)
}

static WILD: [OfferDesc; 1] = [OfferDesc::wildcard()];
static PROPS: [PropDesc; 2] = [
    PropDesc { name: "format", allowed: Constraint::Any, live: false },
    PropDesc { name: "rate", allowed: Constraint::Any, live: false },
];

fn add(
    p: &mut Pipeline,
    sinks: &[&'static [OfferDesc]],
    srcs: &[&'static [OfferDesc]],
    couplings: Vec<Coupling>,
) -> ElementId {
    let pads = |menus: &[&'static [OfferDesc]], side: &str| {
        leak(
            menus
                .iter()
                .enumerate()
                .map(|(i, m)| PadDesc { name: name(side, i), offers: m })
                .collect(),
        )
    };
    let desc = Box::leak(Box::new(ElementDesc {
        name: "node",
        sinks: pads(sinks, "sink"),
        srcs: pads(srcs, "src"),
        props: &PROPS,
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: leak(couplings),
    }));
    p.add(Node(desc))
}

fn audio(rate: ConstraintDesc) -> &'static [OfferDesc] {
    let samples = ids(&["u8", "s16", "s24", "s32", "f32"]);
    menu(vec![
        offer(
            "audio/raw",
            &[("rate", rate), ("channels", ConstraintDesc::Any), ("sample", samples)],
        ),
        OfferDesc::any("bytes"),
    ])
}

fn link(p: &mut Pipeline, a: ElementId, pad: u32, b: ElementId) {
    p.link(a.src(SrcPad(pad)), b.sink(SinkPad(0))).expect("the shape links");
}

/// `src -> 98 transforms -> sink`, the transforms alternating `through` and `same`.
fn chain() -> Pipeline {
    let mut p = Pipeline::new();
    let mut up = add(&mut p, &[], &[audio(ints(&[44_100, 48_000]))], vec![]);
    for i in 0..98 {
        let rules = match i % 2 {
            0 => vec![Coupling::through(0, 0)],
            _ => vec![
                Coupling::same(0, 0, "rate"),
                Coupling::same(0, 0, "channels"),
                Coupling::same(0, 0, "sample"),
            ],
        };
        let t = add(&mut p, &[audio(ConstraintDesc::Any)], &[audio(ConstraintDesc::Any)], rules);
        link(&mut p, up, 0, t);
        up = t;
    }
    let sink = add(&mut p, &[audio(ints(&[48_000]))], &[], vec![]);
    link(&mut p, up, 0, sink);
    p
}

fn fan_out() -> Pipeline {
    let mut p = Pipeline::new();
    let src = add(&mut p, &[], &[audio(ints(&[32_000, 44_100, 48_000, 96_000]))], vec![]);
    let outs: Vec<&'static [OfferDesc]> = (0..64).map(|_| &WILD[..]).collect();
    let tee = add(&mut p, &[&WILD], &outs, vec![]);
    link(&mut p, src, 0, tee);
    for k in 0..64 {
        let sink =
            add(&mut p, &[audio(ints(&[48_000, 96_000, 44_100 + (k as i64 % 3)]))], &[], vec![]);
        link(&mut p, tee, k, sink);
    }
    p
}

fn playback() -> Pipeline {
    let mut p = Pipeline::new();
    let file = add(&mut p, &[], &[menu(vec![OfferDesc::any("bytes")])], vec![]);
    let codecs = menu(vec![
        OfferDesc::any("bytes"),
        OfferDesc::any("video/h264"),
        OfferDesc::any("audio/aac"),
    ]);
    let demux = add(&mut p, &[menu(vec![OfferDesc::any("bytes")])], &[codecs, codecs], vec![]);
    let video = |w| {
        menu(vec![offer(
            "video/raw",
            &[("width", w), ("height", ConstraintDesc::Any), ("pixfmt", ids(&["i420", "nv12"]))],
        )])
    };
    let vdec = add(
        &mut p,
        &[menu(vec![OfferDesc::any("video/h264")])],
        &[video(ConstraintDesc::Any)],
        vec![],
    );
    let vsink = add(&mut p, &[video(ConstraintDesc::Any)], &[], vec![]);
    let adec = add(
        &mut p,
        &[menu(vec![OfferDesc::any("audio/aac")])],
        &[audio(ConstraintDesc::Any)],
        vec![],
    );
    let any = || audio(ConstraintDesc::Any);
    let conv = add(
        &mut p,
        &[any()],
        &[any()],
        vec![
            Coupling::same(0, 0, "rate"),
            Coupling::same(0, 0, "channels"),
            Coupling::prop(0, "sample", "format"),
        ],
    );
    let stereo = add(
        &mut p,
        &[any()],
        &[any()],
        vec![
            Coupling::same(0, 0, "rate"),
            Coupling::same(0, 0, "sample"),
            Coupling::fixed(0, "channels", ValueDesc::Int(2)),
        ],
    );
    let resample = add(
        &mut p,
        &[any()],
        &[any()],
        vec![
            Coupling::same(0, 0, "channels"),
            Coupling::same(0, 0, "sample"),
            Coupling::prop(0, "rate", "rate"),
        ],
    );
    let gain = add(&mut p, &[any()], &[any()], vec![Coupling::through(0, 0)]);
    let out = add(&mut p, &[audio(ints(&[48_000]))], &[], vec![]);
    p.set_str(conv, "format", "f32").expect("a declared prop");
    p.set(resample, "rate", Value::Int(48_000)).expect("a declared prop");
    link(&mut p, file, 0, demux);
    for (a, pad, b) in [
        (demux, 0, vdec),
        (vdec, 0, vsink),
        (demux, 1, adec),
        (adec, 0, conv),
        (conv, 0, stereo),
        (stereo, 0, resample),
        (resample, 0, gain),
        (gain, 0, out),
    ] {
        link(&mut p, a, pad, b);
    }
    p
}

/// One edge of 32 by 32 probed alternatives met on the diagonal alone.
fn probed_edge() -> Pipeline {
    let mut p = Pipeline::new();
    let alts = |reverse: bool| {
        let offers = (0..32).map(|k| {
            let layout = if reverse { 31 - k } else { k };
            let mut fields = vec![
                ("pixfmt", ConstraintDesc::Eq(ValueDesc::Int(layout))),
                ("modifier", ints(&[0, 1, 2, 3, 4, 5, 6, 7])),
            ];
            fields.push((
                "width",
                ConstraintDesc::Range {
                    min: ValueDesc::Int(16),
                    max: ValueDesc::Int(8192),
                    step: ValueDesc::Int(2),
                },
            ));
            fields.push((
                "height",
                ConstraintDesc::Range {
                    min: ValueDesc::Int(16),
                    max: ValueDesc::Int(8192),
                    step: ValueDesc::Int(2),
                },
            ));
            fields.extend((0..12).map(|f| (name("colour", f), ConstraintDesc::Any)));
            offer("video/gpu", &fields)
        });
        menu(offers.collect())
    };
    let src = add(&mut p, &[], &[alts(false)], vec![]);
    let sink = add(&mut p, &[alts(true)], &[], vec![]);
    link(&mut p, src, 0, sink);
    p
}

/// [`probed_edge`] with a `Same` on `width`, so the demand pass meets every pair.
fn probed_edge_coupled() -> Pipeline {
    let mut p = Pipeline::new();
    let alts = |reverse: bool| {
        let offers = (0..32).map(|k| {
            let layout = if reverse { 31 - k } else { k };
            let mut fields = vec![
                ("pixfmt", ConstraintDesc::Eq(ValueDesc::Int(layout))),
                ("modifier", ints(&[0, 1, 2, 3, 4, 5, 6, 7])),
            ];
            fields.push((
                "width",
                ConstraintDesc::Range {
                    min: ValueDesc::Int(16),
                    max: ValueDesc::Int(8192),
                    step: ValueDesc::Int(2),
                },
            ));
            fields.push((
                "height",
                ConstraintDesc::Range {
                    min: ValueDesc::Int(16),
                    max: ValueDesc::Int(8192),
                    step: ValueDesc::Int(2),
                },
            ));
            fields.extend((0..12).map(|f| (name("colour", f), ConstraintDesc::Any)));
            offer("video/gpu", &fields)
        });
        menu(offers.collect())
    };
    let width = || {
        menu(vec![offer(
            "video/gpu",
            &[(
                "width",
                ConstraintDesc::Range {
                    min: ValueDesc::Int(16),
                    max: ValueDesc::Int(8192),
                    step: ValueDesc::Int(2),
                },
            )],
        )])
    };
    let up = add(&mut p, &[], &[width()], vec![]);
    let dec = add(&mut p, &[width()], &[alts(false)], vec![Coupling::same(0, 0, "width")]);
    let sink = add(&mut p, &[alts(true)], &[], vec![]);
    link(&mut p, up, 0, dec);
    link(&mut p, dec, 0, sink);
    p
}

/// 32 by 32 alternatives of 16 sets each that meet on every field but the last, save one pair.
fn late_edge() -> Pipeline {
    let mut p = Pipeline::new();
    let alts = |shift: i64| {
        let offers = (0..32).map(|k| {
            let fields: Vec<(&'static str, ConstraintDesc)> = (0..16)
                .map(|f| {
                    let base = if f == 15 && k != 31 { 1_000 + shift * 100 + k } else { 0 };
                    (name("f", f as usize), ints(&(base..base + 8).collect::<Vec<_>>()))
                })
                .collect();
            offer("x/wide", &fields)
        });
        menu(offers.collect())
    };
    let src = add(&mut p, &[], &[alts(0)], vec![]);
    let sink = add(&mut p, &[alts(1)], &[], vec![]);
    link(&mut p, src, 0, sink);
    p
}

struct Plays(&'static ElementDesc);

impl Element for Plays {
    fn desc(&self) -> &'static ElementDesc {
        self.0
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        match (self.0.sinks.is_empty(), self.0.srcs.is_empty()) {
            (true, _) => {
                if let Some(mut buf) = ctx.try_alloc() {
                    buf.memory.set_len(8);
                    ctx.out(SrcPad(0)).push(buf);
                }
            }
            (false, false) => {
                while let Some(buf) = inputs.pop() {
                    ctx.out(SrcPad(0)).push(buf);
                }
            }
            (false, true) => while inputs.pop().is_some() {},
        }
        Ok(Flow::Ok)
    }
}

fn desc_of(
    sinks: &[&'static [OfferDesc]],
    srcs: &[&'static [OfferDesc]],
    couplings: Vec<Coupling>,
) -> &'static ElementDesc {
    let pads = |menus: &[&'static [OfferDesc]], side: &str| {
        leak(
            menus
                .iter()
                .enumerate()
                .map(|(i, m)| PadDesc { name: name(side, i), offers: m })
                .collect(),
        )
    };
    Box::leak(Box::new(ElementDesc {
        name: "node",
        sinks: pads(sinks, "sink"),
        srcs: pads(srcs, "src"),
        props: &PROPS,
        sched: SchedHint::Active,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: leak(couplings),
    }))
}

/// Pass the stepped run until the change in flight has spliced.
fn settle(p: &mut Pipeline, run: &mut Stepped, clock: &MockClock, ready: &mut Vec<usize>) {
    for k in 0..100_000usize {
        if !p.changing() {
            return;
        }
        run.service();
        ready.clear();
        ready.extend(run.running().iter().copied().filter(|g| run.ready(*g)));
        match ready.is_empty() {
            true => {
                let to = run.next_deadline().unwrap_or(Timestamp(clock.now().0 + 1_000_000));
                clock.advance(Timestamp(to.0.saturating_sub(clock.now().0).max(1)));
            }
            false => drop(run.pass(ready[k % ready.len()]).expect("a pass")),
        }
    }
    panic!("the change did not splice");
}

/// `commits` times a seven-element chain joins a running junction while the one before leaves.
fn churn(commits: usize, hot: usize) -> Churn {
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p =
        Pipeline::with(profluens_core::log::DebugSpec::parse(""), false, Arc::new(clock.clone()));
    p.set_pool(64, 64);
    let source = desc_of(&[], &[audio(ints(&[44_100, 48_000]))], vec![]);
    let any = audio(ConstraintDesc::Any);
    let through = desc_of(&[any], &[any], vec![Coupling::through(0, 0)]);
    let same = desc_of(
        &[any],
        &[any],
        vec![
            Coupling::same(0, 0, "rate"),
            Coupling::same(0, 0, "channels"),
            Coupling::same(0, 0, "sample"),
        ],
    );
    let sink = desc_of(&[audio(ints(&[48_000]))], &[], vec![]);
    let chain = |p: &mut Pipeline, into: ElementId| -> Vec<ElementId> {
        let mut made = vec![p.add(Plays(source))];
        for i in 0..6 {
            made.push(p.add(Plays(if i % 2 == 0 { through } else { same })));
            let (a, b) = (made[i], made[i + 1]);
            p.link(a.src(SrcPad(0)), b.sink(SinkPad(0))).expect("the chain links");
        }
        p.link(made[6].src(SrcPad(0)), into.sink(SinkPad(0))).expect("into the junction");
        made
    };
    let join = p.add(Plays(through));
    let out = p.add(Plays(sink));
    p.link(join.src(SrcPad(0)), out.sink(SinkPad(0))).expect("the junction links");
    let mut old = chain(&mut p, join);
    let mut run = p.step(Arc::new(clock.clone())).expect("a run");
    let mut ready = Vec::new();
    settle(&mut p, &mut run, &clock, &mut ready);
    let (mut solves, mut whole) = (Vec::with_capacity(commits), Vec::with_capacity(commits));
    for _ in 0..commits {
        old.iter().for_each(|el| p.remove(*el, Leave::Cut).expect("the chain leaves"));
        old = chain(&mut p, join);
        let start = Instant::now();
        black_box(p.solve_check()).expect("the change solves");
        solves.push(start.elapsed().as_nanos() as f64 / 1_000.0);
        let start = Instant::now();
        p.commit().expect("the change is taken");
        whole.push(start.elapsed().as_nanos() as f64 / 1_000.0);
        settle(&mut p, &mut run, &clock, &mut ready);
        // An application drains its bus, a full one costs every post a scan.
        while p.bus().try_recv().is_some() {}
    }
    if hot > 0 {
        old.iter().for_each(|el| p.remove(*el, Leave::Cut).expect("the chain leaves"));
        chain(&mut p, join);
        let start = Instant::now();
        for _ in 0..hot {
            black_box(p.solve_check()).expect("the change solves");
        }
        println!(
            "hot churn solve      : {:>8.2} us/solve",
            start.elapsed().as_nanos() as f64 / hot as f64 / 1_000.0
        );
    }
    let ends = |xs: &[f64]| (median(xs[..50].to_vec()), median(xs[xs.len() - 50..].to_vec()));
    Churn {
        solve: median(solves.clone()),
        solve_ends: ends(&solves),
        commit: median(whole.clone()),
        commit_ends: ends(&whole),
        meets: p.solve_meets(),
    }
}

struct Churn {
    solve: f64,
    solve_ends: (f64, f64),
    commit: f64,
    commit_ends: (f64, f64),
    meets: u64,
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    xs[xs.len() / 2]
}

fn bench(p: &mut Pipeline, iters: usize) -> (f64, u64) {
    p.solve_check().expect("the shape solves");
    let mut samples = Vec::with_capacity(TRIALS);
    for trial in 0..WARMUP + TRIALS {
        let start = Instant::now();
        for _ in 0..iters {
            black_box(p.solve_check()).expect("the shape solves");
        }
        if trial >= WARMUP {
            samples.push(start.elapsed().as_nanos() as f64 / iters as f64 / 1_000.0);
        }
    }
    (median(samples), p.solve_meets())
}

fn main() {
    let check = std::env::args().any(|a| a == "--check");
    println!("solve bench, {TRIALS} trials (median), {WARMUP} warmup, warm solve_check");
    type Shape = (&'static str, fn() -> Pipeline, usize, f64);
    let shapes: [Shape; 6] = [
        ("chain 100            ", chain, 2_000, 50.0),
        ("fan-out 64           ", fan_out, 2_000, 100.0),
        ("playback a/v         ", playback, 10_000, 50.0),
        ("probed edge 32x32    ", probed_edge, 2_000, 20.0),
        ("probed edge coupled  ", probed_edge_coupled, 2_000, 20.0),
        ("late edge 32x32x16   ", late_edge, 100, f64::INFINITY),
    ];
    let mut over = Vec::new();
    let only: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    for (what, build, iters, budget) in shapes {
        if !only.is_empty() && !only.iter().any(|o| what.contains(o.as_str())) {
            continue;
        }
        let mut p = build();
        let (us, meets) = bench(&mut p, iters);
        println!("{what}: {us:>8.2} us/solve  {meets:>6} meets  budget {budget} us");
        if check && us > budget * 3.0 {
            over.push(what.trim());
        }
    }
    if let Some(hot) = only.iter().find_map(|o| o.strip_prefix("hot=")?.parse().ok()) {
        churn(200, hot);
        return;
    }
    let commits = only.iter().find_map(|o| o.strip_prefix("churn=")?.parse().ok());
    if only.is_empty() || only.iter().any(|o| o == "churn") || commits.is_some() {
        let c = churn(commits.unwrap_or(1_000), 0);
        let (solve, (first, last), meets) = (c.solve, c.solve_ends, c.meets);
        println!(
            "relink churn 7 edges : {solve:>8.2} us/solve  {meets:>6} meets  budget 5 us, first and last 50 {first:.2} and {last:.2}"
        );
        let (commit, (first, last)) = (c.commit, c.commit_ends);
        println!(
            "relink churn commit  : {commit:>8.2} us/commit, first and last 50 {first:.2} and {last:.2}"
        );
        if check && solve > 15.0 {
            over.push("relink churn");
        }
        // A commit costs what changes, the last fifty as the first.
        if check && last > first * 3.0 {
            over.push("relink churn commit growth");
        }
    }
    assert!(over.is_empty(), "over budget by more than 3x: {over:?}");
}
