// Tests own their fixtures and leak their descriptors.
#![allow(clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};

use pf_registry::convert::{ConvertError, link_or_convert};
use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::clock::{Clock, MockClock};
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, LinkError};
use profluens_core::event::Event;
use profluens_core::format::{
    ConstraintDesc, ConstraintSpec, FieldDesc, FieldSpec, FixedFormat, OfferDesc, Value, ValueDesc,
    ValueSpec,
};
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::sched::{Outcome, Stepped};
use profluens_core::time::Timestamp;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);
const FRAMES: usize = 480;

fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

struct Shape {
    rates: &'static [i64],
    channels: &'static [i64],
    samples: &'static [&'static str],
}

fn offers(s: &Shape) -> &'static [OfferDesc] {
    let set = |vs: Vec<ValueDesc>| ConstraintDesc::Set(leak(vs));
    let fields = leak(vec![
        FieldDesc {
            field: "rate",
            allowed: set(s.rates.iter().map(|r| ValueDesc::Int(*r)).collect()),
            preferred: None,
        },
        FieldDesc {
            field: "channels",
            allowed: set(s.channels.iter().map(|c| ValueDesc::Int(*c)).collect()),
            preferred: None,
        },
        FieldDesc {
            field: "sample",
            allowed: set(s.samples.iter().map(|n| ValueDesc::Id(n)).collect()),
            preferred: None,
        },
    ]);
    leak(vec![OfferDesc { family: "audio/raw", fields }])
}

fn desc(
    name: &'static str,
    sink: Option<&'static [OfferDesc]>,
    src: Option<&'static [OfferDesc]>,
) -> &'static ElementDesc {
    let pad = |name, offers| leak(vec![PadDesc { name, offers }]);
    Box::leak(Box::new(ElementDesc {
        name,
        sinks: sink.map_or(&[][..], |o| pad("sink", o)),
        srcs: src.map_or(&[][..], |o| pad("src", o)),
        props: &[],
        sched: SchedHint::Active,
        inputs: if sink.is_some() { InputPolicy::Single } else { InputPolicy::None },
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    }))
}

struct Src {
    desc: &'static ElementDesc,
    rate: i64,
    channels: i64,
    sample: &'static str,
    left: u32,
}

fn bytes_of(sample: &str) -> usize {
    match sample {
        "u8" => 1,
        "s16" => 2,
        "s24" => 3,
        _ => 4,
    }
}

impl Element for Src {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let fields = [
            ("rate", ValueDesc::Int(self.rate)),
            ("channels", ValueDesc::Int(self.channels)),
            ("sample", ValueDesc::Id(self.sample)),
        ];
        ctx.announce_format(OUT, "audio/raw", &fields);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.left == 0 {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.set_len(FRAMES * self.channels as usize * bytes_of(self.sample));
        self.left -= 1;
        ctx.out(OUT).push(buf);
        Ok(Flow::Ok)
    }
}

#[derive(Default)]
struct Heard {
    formats: Vec<(Option<i64>, Option<String>)>,
    bytes: usize,
}

struct Sink {
    desc: &'static ElementDesc,
    heard: Arc<Mutex<Heard>>,
}

fn read(ctx: &Ctx, f: &FixedFormat) -> (Option<i64>, Option<String>) {
    let rate = match ctx.field_id("rate").and_then(|id| f.get(id)) {
        Some(Value::Int(r)) => Some(r),
        _ => None,
    };
    let sample = match ctx.field_id("sample").and_then(|id| f.get(id)) {
        Some(Value::Id(v)) => ctx.value_name(v).map(str::to_string),
        _ => None,
    };
    (rate, sample)
}

impl Element for Sink {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        if let Some(f) = ctx.negotiated_sink(IN) {
            let seen = read(ctx, f);
            self.heard.lock().unwrap().formats.push(seen);
        }
        Ok(())
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.heard.lock().unwrap().bytes += buf.memory.len();
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if let Event::FormatChange(f) = event {
            let seen = read(ctx, f);
            self.heard.lock().unwrap().formats.push(seen);
        }
        Ok(())
    }
}

struct Rig {
    p: Pipeline,
    clock: MockClock,
    src: ElementId,
    sink: ElementId,
    heard: Arc<Mutex<Heard>>,
}

fn rig(from: Shape, to: Shape, buffers: u32) -> Rig {
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p =
        Pipeline::with(profluens_core::log::DebugSpec::parse(""), false, Arc::new(clock.clone()));
    p.set_pool(64 * 1024, 32);
    let (rate, channels, sample) = (from.rates[0], from.channels[0], from.samples[0]);
    let src = p.add(Src {
        desc: desc("rawsrc", None, Some(offers(&from))),
        rate,
        channels,
        sample,
        left: buffers,
    });
    let heard = Arc::new(Mutex::new(Heard::default()));
    let sink =
        p.add(Sink { desc: desc("rawsink", Some(offers(&to)), None), heard: Arc::clone(&heard) });
    Rig { p, clock, src, sink, heard }
}

impl Rig {
    fn convert(
        &mut self,
        filter: &[FieldSpec<'_>],
    ) -> Result<pf_registry::convert::Inserted, ConvertError> {
        link_or_convert(
            &mut self.p,
            self.src.src(OUT),
            self.sink.sink(IN),
            filter,
            &profluens_audio::CONVERT,
        )
    }

    fn names(&self, els: &[ElementId]) -> Vec<&'static str> {
        els.iter().map(|e| self.p.element_desc(*e).map_or("?", |d| d.name)).collect()
    }

    fn prop(&self, el: ElementId, prop: &str) -> Option<String> {
        match self.p.prop_value(el, prop)? {
            Value::Int(n) => Some(n.to_string()),
            Value::Id(v) => self.p.value_name(v).map(str::to_string),
            Value::Rat(n, d) => Some(format!("{n}/{d}")),
        }
    }

    fn inserted(&self) -> Vec<(ElementId, LinkId)> {
        std::iter::from_fn(|| self.p.bus().try_recv())
            .filter_map(|m| match m {
                BusMessage::Inserted { element, link } => Some((element, link)),
                _ => None,
            })
            .collect()
    }

    fn play(&mut self) -> bool {
        let mut run: Stepped = self.p.step(Arc::new(self.clock.clone())).expect("step");
        for k in 0..100_000u32 {
            run.service();
            let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
            if ready.is_empty() {
                let Some(to) = run.next_deadline() else { break };
                self.clock.advance(Timestamp(to.0.saturating_sub(self.clock.now().0)));
                continue;
            }
            run.pass(ready[k as usize % ready.len()]).expect("pass");
        }
        (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
    }
}

const S16_48K: Shape = Shape { rates: &[48_000], channels: &[2], samples: &["s16"] };

#[test]
fn a_direct_link_that_solves_inserts_nothing() {
    let mut r = rig(Shape { rates: &[48_000], channels: &[2], samples: &["s16"] }, S16_48K, 0);
    let before = r.p.element_count();
    let made = r.convert(&[]).expect("links");
    assert!(made.elements().is_empty());
    assert_eq!(made.links().len(), 1);
    assert_eq!(r.p.element_count(), before, "nothing was added");
    assert!(!r.p.dump_dot().contains("(auto)"));
    assert!(r.inserted().is_empty());
}

#[test]
fn a_sample_mismatch_takes_a_converter_set_to_what_the_sink_takes() {
    let mut r = rig(Shape { rates: &[48_000], channels: &[2], samples: &["s24"] }, S16_48K, 0);
    let made = r.convert(&[]).expect("converts");
    assert_eq!(r.names(made.elements()), ["audioconvert"]);
    let conv = made.elements()[0];
    assert_eq!(r.prop(conv, "format").as_deref(), Some("s16"), "the sink's demand");
    assert!(r.p.dump_dot().contains("audioconvert (auto)"), "{}", r.p.dump_dot());
    assert_eq!(r.inserted(), [(conv, made.links()[0])]);
    r.p.solve_check().expect("the run solves");
}

#[test]
fn a_rate_mismatch_takes_a_resampler_alone_its_rate_from_the_sink() {
    let mut r = rig(Shape { rates: &[44_100], channels: &[2], samples: &["s16"] }, S16_48K, 0);
    let made = r.convert(&[]).expect("converts");
    assert_eq!(r.names(made.elements()), ["audioresample"]);
    assert_eq!(r.prop(made.elements()[0], "rate").as_deref(), Some("48000"));
    assert!(r.p.dump_dot().contains("audioresample (auto)"));
}

#[test]
fn a_rate_and_sample_mismatch_takes_a_converter_then_a_resampler() {
    let mut r = rig(Shape { rates: &[44_100], channels: &[2], samples: &["s24"] }, S16_48K, 0);
    let made = r.convert(&[]).expect("converts");
    assert_eq!(r.names(made.elements()), ["audioconvert", "audioresample"]);
    let (conv, res) = (made.elements()[0], made.elements()[1]);
    assert_eq!(r.prop(conv, "format").as_deref(), Some("s16"));
    assert_eq!(r.prop(res, "rate").as_deref(), Some("48000"));
    assert_eq!(made.links().len(), 3);
    assert_eq!(made.tail(), Some(res));
    let dot = r.p.dump_dot();
    assert!(dot.contains("audioconvert (auto)") && dot.contains("audioresample (auto)"), "{dot}");
    assert_eq!(r.inserted(), [(conv, made.links()[0]), (res, made.links()[1])]);
}

#[test]
fn no_chain_that_helps_is_refused_and_leaves_nothing_linked() {
    let mut r = rig(Shape { rates: &[48_000], channels: &[6], samples: &["s16"] }, S16_48K, 0);
    let err = r.convert(&[]).expect_err("refused");
    let ConvertError::Refused { direct, tried } = err else { panic!("{err}") };
    assert!(matches!(direct, Error::Link(LinkError::NoCommonFormat(_))), "{direct}");
    assert_eq!(tried, 3, "convert, resample, both");
    assert!(r.inserted().is_empty());
    assert!(!r.p.dump_dot().contains("(auto)"));
    let made =
        rig(Shape { rates: &[48_000], channels: &[2], samples: &["s16"] }, S16_48K, 0).convert(&[]);
    assert!(made.is_ok());
    assert!(r.p.dump_dot().matches("->").count() == 0, "{}", r.p.dump_dot());
}

#[test]
fn a_filter_that_refuses_the_direct_link_lands_on_the_sinks_link() {
    let to = Shape { rates: &[44_100, 48_000], channels: &[2], samples: &["s16"] };
    let mut r = rig(Shape { rates: &[44_100], channels: &[2], samples: &["s16"] }, to, 0);
    let filter = [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(48_000)),
        preferred: None,
    }];
    let made = r.convert(&filter).expect("converts");
    assert_eq!(r.names(made.elements()), ["audioresample"]);
    assert_eq!(r.prop(made.elements()[0], "rate").as_deref(), Some("48000"));
    let dot = r.p.dump_dot();
    assert!(dot.contains("[rate=48000]"), "{dot}");
}

#[test]
fn a_filter_into_a_wildcard_sink_links_direct_or_converts() {
    static WILD: [OfferDesc; 1] = [OfferDesc::wildcard()];
    let filter = [FieldSpec {
        field: "rate",
        allowed: ConstraintSpec::Eq(ValueSpec::Int(48_000)),
        preferred: None,
    }];
    for (rates, chain) in
        [(&[44_100, 48_000][..], &[][..]), (&[44_100][..], &["audioresample"][..])]
    {
        let mut r = rig(Shape { rates, channels: &[2], samples: &["s16"] }, S16_48K, 0);
        let queue =
            r.p.add(Sink { desc: desc("queue", Some(&WILD), Some(&WILD)), heard: Arc::default() });
        let made = link_or_convert(
            &mut r.p,
            r.src.src(OUT),
            queue.sink(IN),
            &filter,
            &profluens_audio::CONVERT,
        );
        let made = made.unwrap_or_else(|e| panic!("{rates:?} refused: {e}"));
        assert_eq!(r.names(made.elements()), chain);
        let last = *made.links().last().unwrap();
        let rate = r.p.negotiated(last).and_then(|f| f.get(r.p.field_id("rate")?));
        assert_eq!(rate, Some(Value::Int(48_000)), "{rates:?}");
        r.p.solve_check().expect("the run solves");
    }
}

#[test]
fn the_inserted_chain_plays_on_the_stepped_driver() {
    let mut r = rig(Shape { rates: &[44_100], channels: &[2], samples: &["s24"] }, S16_48K, 20);
    let made = r.convert(&[]).expect("converts");
    assert_eq!(made.elements().len(), 2);
    assert!(r.play(), "every group exited");
    let heard = r.heard.lock().unwrap();
    assert!(
        heard.formats.iter().all(|f| *f == (Some(48_000), Some("s16".into()))),
        "{:?}",
        heard.formats
    );
    assert!(!heard.formats.is_empty());
    // 20 buffers of 480 frames at 44.1k is about 10,449 frames at 48k, less the filter's lead.
    let frames = heard.bytes / 4;
    assert!((9_500..=10_500).contains(&frames), "{frames} frames");
}

struct ByteSrc {
    left: u32,
}

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static BYTE_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &BYTES }];
static BYTE_SRC: ElementDesc = ElementDesc {
    name: "bytesrc",
    sinks: &[],
    srcs: &BYTE_PADS,
    props: &[],
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: None,
    io: false,
    couplings: &[],
};

impl Element for ByteSrc {
    fn desc(&self) -> &'static ElementDesc {
        &BYTE_SRC
    }
    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if self.left == 0 {
            return Ok(Flow::Eos);
        }
        let Some(mut buf) = ctx.try_alloc() else {
            return Ok(Flow::Ok);
        };
        buf.memory.set_len(FRAMES * 4);
        self.left -= 1;
        ctx.out(OUT).push(buf);
        Ok(Flow::Ok)
    }
}

struct Dec {
    desc: &'static ElementDesc,
    rate: i64,
}

impl Element for Dec {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let fields = [
            ("rate", ValueDesc::Int(self.rate)),
            ("channels", ValueDesc::Int(2)),
            ("sample", ValueDesc::Id("s16")),
        ];
        ctx.announce_format(OUT, "audio/raw", &fields);
        Ok(())
    }
    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            ctx.out(OUT).push(buf);
        }
        Ok(Flow::Ok)
    }
}

fn decoded(rate: i64, buffers: u32) -> Rig {
    let clock = MockClock::starting_at(Timestamp::from_secs(1));
    let mut p =
        Pipeline::with(profluens_core::log::DebugSpec::parse(""), false, Arc::new(clock.clone()));
    p.set_pool(64 * 1024, 32);
    let fields = leak(vec![
        FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc {
            field: "channels",
            allowed: ConstraintDesc::Set(&[ValueDesc::Int(2)]),
            preferred: None,
        },
        FieldDesc {
            field: "sample",
            allowed: ConstraintDesc::Set(&[ValueDesc::Id("s16")]),
            preferred: None,
        },
    ]);
    let out = leak(vec![OfferDesc { family: "audio/raw", fields }, OfferDesc::any("bytes")]);
    let file = p.add(ByteSrc { left: buffers });
    let src = p.add(Dec { desc: desc("rawdec", Some(&BYTES), Some(out)), rate });
    p.link(file.src(OUT), src.sink(IN)).unwrap();
    let heard = Arc::new(Mutex::new(Heard::default()));
    let to = Shape { rates: &[44_100], channels: &[2], samples: &["s16"] };
    let sink =
        p.add(Sink { desc: desc("rawsink", Some(offers(&to)), None), heard: Arc::clone(&heard) });
    Rig { p, clock, src, sink, heard }
}

#[test]
fn a_rate_left_to_the_announcement_takes_a_resampler_that_settles_it() {
    // The direct link solves, at 44.1 kHz, and a 48 kHz announcement would be refused.
    let mut direct = decoded(48_000, 0);
    let l = direct.p.link(direct.src.src(OUT), direct.sink.sink(IN)).expect("solves");
    let open: Vec<&str> = direct.p.unsettled(l).map(|f| direct.p.field_name(f).unwrap()).collect();
    assert_eq!(open, ["rate"]);
    for (rate, frames) in [(48_000, 8_500..=8_820), (44_100, 9_600..=9_600)] {
        let mut r = decoded(rate, 20);
        let made = r.convert(&[]).expect("settles");
        assert_eq!(
            r.names(made.elements()),
            ["audioresample"],
            "a converter hands the rate on, a resampler picks it"
        );
        assert_eq!(r.prop(made.elements()[0], "rate").as_deref(), Some("44100"));
        assert!(made.links().iter().all(|l| r.p.unsettled(*l).next().is_none()));
        assert!(r.play(), "every group exited");
        let heard = r.heard.lock().unwrap();
        assert!(heard.formats.iter().all(|f| f.0 == Some(44_100)), "{:?}", heard.formats);
        assert!(frames.contains(&(heard.bytes / 4)), "{rate}: {} frames", heard.bytes / 4);
    }
}

#[test]
fn an_unsettled_link_no_chain_settles_stays_direct() {
    let mut r = decoded(44_100, 0);
    let fields = leak(vec![
        FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Set(&[ValueDesc::Int(44_100)]),
            preferred: None,
        },
        FieldDesc { field: "channels", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc {
            field: "sample",
            allowed: ConstraintDesc::Set(&[ValueDesc::Id("s16")]),
            preferred: None,
        },
    ]);
    let out = leak(vec![OfferDesc { family: "audio/raw", fields }]);
    let file = r.p.add(ByteSrc { left: 0 });
    let dec = r.p.add(Dec { desc: desc("rawdec", Some(&BYTES), Some(out)), rate: 44_100 });
    r.p.link(file.src(OUT), dec.sink(IN)).unwrap();
    let to = Shape { rates: &[44_100], channels: &[2], samples: &["s16"] };
    let strict =
        r.p.add(Sink { desc: desc("rawsink", Some(offers(&to)), None), heard: Arc::default() });
    let made =
        link_or_convert(&mut r.p, dec.src(OUT), strict.sink(IN), &[], &profluens_audio::CONVERT)
            .expect("links");
    assert!(made.elements().is_empty(), "{}", r.p.dump_dot());
    let open: Vec<&str> =
        r.p.unsettled(made.links()[0]).map(|f| r.p.field_name(f).unwrap()).collect();
    assert_eq!(open, ["channels"]);
    assert!(!r.p.dump_dot().contains("(auto)"));
}

#[test]
fn a_downmix_puts_out_one_or_two_channels_which_a_stereo_only_sink_leaves_unsettled() {
    use profluens_audio::AudioDownmix;
    for (channels, open) in [(&[1, 2][..], false), (&[2][..], true)] {
        let mut r = decoded(44_100, 0);
        let mix = r.p.add(AudioDownmix::new());
        r.p.link(r.src.src(OUT), mix.sink(AudioDownmix::IN)).unwrap();
        let to = Shape { rates: &[44_100], channels, samples: &["s16"] };
        let sink =
            r.p.add(Sink { desc: desc("rawsink", Some(offers(&to)), None), heard: Arc::default() });
        let l = r.p.link(mix.src(AudioDownmix::OUT), sink.sink(IN)).unwrap();
        let unsettled: Vec<&str> = r.p.unsettled(l).map(|f| r.p.field_name(f).unwrap()).collect();
        assert_eq!(unsettled.contains(&"channels"), open, "{channels:?}: {unsettled:?}");
        let ch = r.p.negotiated(l).and_then(|f| f.get(r.p.field_id("channels")?));
        assert_eq!(ch, Some(Value::Int(2)), "two where the solve has the choice");
    }
}

fn advance(r: &Rig, run: &mut Stepped, passes: u32) -> bool {
    for k in 0..passes {
        run.service();
        let ready: Vec<usize> = (0..run.groups()).filter(|g| run.ready(*g)).collect();
        if ready.is_empty() {
            let Some(to) = run.next_deadline() else { break };
            r.clock.advance(Timestamp(to.0.saturating_sub(r.clock.now().0)));
            continue;
        }
        run.pass(ready[k as usize % ready.len()]).expect("pass");
    }
    (0..run.groups()).all(|g| run.outcome(g) == Some(Outcome::Exit))
}

fn sink_of(r: &mut Rig, shape: &Shape) -> (ElementId, Arc<Mutex<Heard>>) {
    let heard = Arc::new(Mutex::new(Heard::default()));
    let sink = r
        .p
        .add(Sink { desc: desc("rawsink", Some(offers(shape)), None), heard: Arc::clone(&heard) });
    (sink, heard)
}

#[test]
fn a_sink_replaced_while_playing_takes_the_converters_its_format_needs() {
    use pf_registry::convert::{ReplaceError, replace_sink};
    use profluens_core::topology::Leave;
    let f32_48k = Shape { rates: &[48_000], channels: &[2], samples: &["f32"] };
    let mut r = rig(Shape { rates: &[48_000], channels: &[2], samples: &["f32"] }, f32_48k, 400);
    let first = Arc::clone(&r.heard);
    r.p.link(r.src.src(OUT), r.sink.sink(IN)).unwrap();
    r.p.set_link_limit(r.sink, profluens_core::level::Limit::time(Timestamp::from_millis(40)));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    assert!(!advance(&r, &mut run, 60));
    let (b, heard_b) = sink_of(&mut r, &S16_48K);
    let made =
        replace_sink(&mut r.p, (r.sink, IN), (b, IN), Leave::Cut, &[], &profluens_audio::CONVERT)
            .expect("replaced");
    assert_eq!(made.upstream, r.src.src(OUT));
    assert!(made.removed().is_empty(), "nothing was inserted ahead of the first");
    assert_eq!(r.names(made.inserted.elements()), ["audioconvert"]);
    let conv = made.inserted.elements()[0];
    assert_eq!(r.prop(conv, "format").as_deref(), Some("s16"));
    r.p.commit().expect("commit");
    assert!(!advance(&r, &mut run, 120));
    let (c, heard_c) =
        sink_of(&mut r, &Shape { rates: &[44_100], channels: &[2], samples: &["s16"] });
    let made =
        replace_sink(&mut r.p, (b, IN), (c, IN), Leave::Drain, &[], &profluens_audio::CONVERT)
            .expect("replaced");
    assert_eq!(made.removed(), [conv]);
    assert_eq!(r.names(made.inserted.elements()), ["audioconvert", "audioresample"]);
    let chain = made.inserted;
    assert_eq!(r.prop(chain.elements()[1], "rate").as_deref(), Some("44100"));
    let dot = r.p.dump_dot();
    assert!(dot.contains("audioconvert (auto)") && dot.contains("audioresample (auto)"), "{dot}");
    r.p.commit().expect("commit");
    assert!(!advance(&r, &mut run, 120));
    let (d, heard_d) =
        sink_of(&mut r, &Shape { rates: &[48_000], channels: &[6], samples: &["s16"] });
    let refused =
        replace_sink(&mut r.p, (c, IN), (d, IN), Leave::Cut, &[], &profluens_audio::CONVERT);
    assert!(
        matches!(refused, Err(ReplaceError::Convert(ConvertError::Refused { .. }))),
        "{refused:?}"
    );
    assert_eq!(r.p.link_into(c, IN).map(|(_, el, _)| el), chain.tail());
    r.p.abandon();
    let before = heard_c.lock().unwrap().bytes;
    assert!(!advance(&r, &mut run, 120));
    assert!(heard_c.lock().unwrap().bytes > before, "the 44.1 kHz sink plays on");
    let (e, heard_e) =
        sink_of(&mut r, &Shape { rates: &[48_000], channels: &[2], samples: &["f32"] });
    let made = replace_sink(&mut r.p, (c, IN), (e, IN), Leave::Cut, &[], &profluens_audio::CONVERT)
        .expect("replaced");
    assert_eq!(made.removed(), chain.elements());
    assert!(made.inserted.elements().is_empty());
    assert_eq!(r.p.link_into(e, IN).map(|(_, el, pad)| (el, pad)), Some(r.src.src(OUT)));
    r.p.commit().expect("commit");
    assert!(advance(&r, &mut run, 100_000), "every group exited");
    run.finish().expect("finish");
    let formats = |h: &Arc<Mutex<Heard>>| h.lock().unwrap().formats.clone();
    let f32_at = (Some(48_000), Some("f32".to_string()));
    assert!(formats(&first).iter().all(|f| *f == f32_at));
    assert!(
        !formats(&heard_b).is_empty()
            && formats(&heard_b).iter().all(|f| *f == (Some(48_000), Some("s16".into())))
    );
    assert!(
        !formats(&heard_c).is_empty()
            && formats(&heard_c).iter().all(|f| *f == (Some(44_100), Some("s16".into())))
    );
    assert!(formats(&heard_d).is_empty() && heard_d.lock().unwrap().bytes == 0, "never ran");
    assert!(!formats(&heard_e).is_empty() && formats(&heard_e).iter().all(|f| *f == f32_at));
    let heard = [&first, &heard_b, &heard_c, &heard_e].map(|h| h.lock().unwrap().bytes);
    assert!(heard.iter().all(|b| *b > 0), "each played, {heard:?}");
}

struct Hold {
    desc: &'static ElementDesc,
}

impl Element for Hold {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

#[test]
fn the_rings_into_a_replacing_sink_are_set_as_into_the_old() {
    use pf_registry::convert::replace_sink;
    use profluens_core::level::Limit;
    use profluens_core::topology::Leave;
    let mut r = rig(Shape { rates: &[48_000], channels: &[2], samples: &["s16"] }, S16_48K, 10_000);
    r.p.link(r.src.src(OUT), r.sink.sink(IN)).unwrap();
    r.p.set_link_marks(r.sink, Limit::bytes(1920), Limit::bytes(4 * 1920));
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    assert!(!advance(&r, &mut run, 40));
    let held = r.p.add(Hold { desc: desc("holdsink", Some(offers(&S16_48K)), None) });
    replace_sink(&mut r.p, (r.sink, IN), (held, IN), Leave::Cut, &[], &profluens_audio::CONVERT)
        .expect("replaced");
    r.p.commit().expect("commit");
    let _ = std::iter::from_fn(|| r.p.bus().try_recv()).count();
    assert!(!advance(&r, &mut run, 2_000));
    let said = std::iter::from_fn(|| r.p.bus().try_recv())
        .any(|m| matches!(m, BusMessage::Buffering { element, .. } if element == held));
    assert!(said, "the new sink's link has the old one's marks");
}

#[test]
fn a_sink_replaced_after_the_source_read_its_whole_stream_takes_the_end() {
    use pf_registry::convert::replace_sink;
    use profluens_core::topology::Leave;
    for leave in [Leave::Drain, Leave::Cut] {
        let mut r = rig(Shape { rates: &[48_000], channels: &[2], samples: &["s16"] }, S16_48K, 6);
        r.p.link(r.src.src(OUT), r.sink.sink(IN)).unwrap();
        let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
        for _ in 0..40 {
            run.service();
            if run.ready(0) {
                run.pass(0).expect("pass");
            }
        }
        let (b, heard_b) =
            sink_of(&mut r, &Shape { rates: &[44_100], channels: &[2], samples: &["s16"] });
        let made =
            replace_sink(&mut r.p, (r.sink, IN), (b, IN), leave, &[], &profluens_audio::CONVERT)
                .expect("replaced");
        assert_eq!(r.names(made.inserted.elements()), ["audioresample"]);
        let change = r.p.commit().expect("commit");
        assert!(advance(&r, &mut run, 100_000), "{leave:?}: every group exited");
        r.p.settle();
        let changed = std::iter::from_fn(|| r.p.bus().try_recv())
            .any(|m| matches!(m, BusMessage::Changed { change: c } if c == change));
        assert!(changed && !r.p.changing(), "{leave:?}: the change spliced");
        run.finish().expect("finish");
        assert_eq!(
            heard_b.lock().unwrap().formats,
            [(Some(44_100), Some("s16".to_string()))],
            "{leave:?}: the new sink started"
        );
        let (old, new) = (
            r.heard.lock().unwrap().bytes / (FRAMES * 4),
            heard_b.lock().unwrap().bytes / (441 * 4),
        );
        assert!(new > 0, "{leave:?}: the new sink took what was left");
        match leave {
            Leave::Drain => assert_eq!(old + new, 6, "every buffer heard once"),
            Leave::Cut => assert_eq!(old, 0, "the cut sink played nothing"),
        }
    }
}

#[test]
fn a_sink_replaced_behind_a_source_of_two_rates_takes_the_rate_it_runs_at() {
    use pf_registry::convert::replace_sink;
    use profluens_core::topology::Leave;
    let mut r =
        rig(Shape { rates: &[48_000, 44_100], channels: &[2], samples: &["s16"] }, S16_48K, 200);
    let running = r.p.link(r.src.src(OUT), r.sink.sink(IN)).unwrap();
    let mut run = r.p.step(Arc::new(r.clock.clone())).expect("step");
    assert!(!advance(&r, &mut run, 60));
    let (b, heard_b) =
        sink_of(&mut r, &Shape { rates: &[44_100], channels: &[2], samples: &["s16"] });
    let made =
        replace_sink(&mut r.p, (r.sink, IN), (b, IN), Leave::Cut, &[], &profluens_audio::CONVERT)
            .expect("replaced");
    assert_eq!(r.names(made.inserted.elements()), ["audioresample"]);
    assert_eq!(r.prop(made.inserted.elements()[0], "rate").as_deref(), Some("44100"));
    let into = made.inserted.links()[0];
    let rate = |r: &Rig, l| r.p.negotiated(l).and_then(|f| f.get(r.p.field_id("rate")?));
    assert_eq!(rate(&r, into), Some(Value::Int(48_000)), "the running rate into the resampler");
    r.p.commit().expect("commit");
    assert_eq!(rate(&r, into), Some(Value::Int(48_000)));
    assert!(advance(&r, &mut run, 100_000), "every group exited");
    run.finish().expect("finish");
    let rejected =
        std::iter::from_fn(|| r.p.bus().try_recv()).any(|m| matches!(m, BusMessage::Error { .. }));
    assert!(!rejected, "no announcement was refused");
    let heard = heard_b.lock().unwrap();
    assert!(
        !heard.formats.is_empty() && heard.formats.iter().all(|f| f.0 == Some(44_100)),
        "{:?}",
        heard.formats
    );
    assert!(heard.bytes > 0);
    let _ = running;
}

struct Narrow;

impl Element for Narrow {
    fn desc(&self) -> &'static ElementDesc {
        &NARROW
    }
    fn process(&mut self, _ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        Ok(Flow::Ok)
    }
}

static ANY_RAW: [OfferDesc; 1] = [OfferDesc::any("audio/raw")];
static NARROW_SINK: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY_RAW }];
static NARROW_SRC: [PadDesc; 1] = [PadDesc { name: "src", offers: &ANY_RAW }];
static ONLY_96K: [Value; 1] = [Value::Int(96_000)];
static NARROW_PROPS: [profluens_core::element::PropDesc; 1] = [profluens_core::element::PropDesc {
    name: "rate",
    allowed: profluens_core::format::Constraint::Set(&ONLY_96K),
    live: false,
}];
static NARROW_RULES: [profluens_core::couple::Coupling; 3] = [
    profluens_core::couple::Coupling::prop(0, "rate", "rate"),
    profluens_core::couple::Coupling::same(0, 0, "channels"),
    profluens_core::couple::Coupling::same(0, 0, "sample"),
];
static NARROW: ElementDesc = ElementDesc {
    name: "narrowresample",
    sinks: &NARROW_SINK,
    srcs: &NARROW_SRC,
    props: &NARROW_PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(Narrow)),
    io: false,
    couplings: &NARROW_RULES,
};
static NARROW_POLICY: pf_registry::convert::ConvertPolicy =
    pf_registry::convert::ConvertPolicy { converters: &[("audio/raw", &[&NARROW])] };

#[test]
fn a_converter_that_refuses_its_solved_property_leaves_the_pipeline_as_it_was() {
    let mut r = rig(Shape { rates: &[44_100], channels: &[2], samples: &["s16"] }, S16_48K, 0);
    let before = r.p.element_count();
    let err = link_or_convert(&mut r.p, r.src.src(OUT), r.sink.sink(IN), &[], &NARROW_POLICY)
        .expect_err("refused");
    let ConvertError::Prop { element, prop, .. } = err else { panic!("{err}") };
    assert!(element.0 as usize >= before && prop == "rate", "{element:?} {prop}");
    assert!(!r.p.is_auto(element), "never marked");
    assert!(r.inserted().is_empty(), "nothing said inserted");
    assert!(r.p.link_into(r.sink, IN).is_none(), "the sink is not fed");
    assert!(r.p.dump_dot().matches("->").count() == 0, "{}", r.p.dump_dot());
}
