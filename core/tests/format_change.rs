use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, FormatError, RejectedFormat};
use profluens_core::event::Event;
use profluens_core::format::{
    ConstraintDesc, ConstraintSpec, FieldDesc, FieldSpec, OfferDesc, OfferSpec, Value, ValueDesc,
    ValueSpec,
};
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
) -> ElementDesc {
    ElementDesc {
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
    }
}

const fn field(field: &'static str, allowed: ConstraintDesc) -> FieldDesc {
    FieldDesc { field, allowed, preferred: None }
}

const MOD_INVALID: i64 = 0x00ff_ffff_ffff_ffff;
const MOD_TILED: i64 = 0x0100_0000_0000_0002;

static ANY_RATE: [FieldDesc; 1] = [field("rate", ConstraintDesc::Any)];
static ANY_MOD: [FieldDesc; 1] = [field("modifier", ConstraintDesc::Any)];
static TALK: [OfferDesc; 3] = [
    OfferDesc { family: "audio/raw", fields: &ANY_RATE },
    OfferDesc { family: "video/gpu", fields: &ANY_MOD },
    OfferDesc::any("bytes"),
];
static TALK_PAD: [PadDesc; 1] = [PadDesc { name: "src", offers: &TALK }];

static RATES: [ValueDesc; 2] = [ValueDesc::Int(44_100), ValueDesc::Int(48_000)];
static MODS: [ValueDesc; 2] = [ValueDesc::Int(MOD_TILED), ValueDesc::Int(MOD_INVALID)];
static RATE_SET: [FieldDesc; 1] = [field("rate", ConstraintDesc::Set(&RATES))];
static MOD_SET: [FieldDesc; 1] = [field("modifier", ConstraintDesc::Set(&MODS))];
static MENU: [OfferDesc; 2] = [
    OfferDesc { family: "audio/raw", fields: &RATE_SET },
    OfferDesc { family: "video/gpu", fields: &MOD_SET },
];
static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static MENU_PAD: [PadDesc; 1] = [PadDesc { name: "sink", offers: &MENU }];
static PROBED_PAD: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static BYTES_PAD: [PadDesc; 1] = [PadDesc { name: "sink", offers: &BYTES }];

static TALKER: ElementDesc = desc("talker", &[], &TALK_PAD);
static STATIC: ElementDesc = desc("menu", &MENU_PAD, &[]);
static PROBED: ElementDesc = desc("device", &PROBED_PAD, &[]);
static BYTE_SINK: ElementDesc = desc("bytesink", &BYTES_PAD, &[]);
static MUX: ElementDesc = desc("mux", &[], &[]);

type Step = (&'static str, &'static str, i64);

struct Talker {
    plan: &'static [Step],
    next: usize,
}

impl Element for Talker {
    fn desc(&self) -> &'static ElementDesc {
        &TALKER
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        let Some(&(family, field, value)) = self.plan.get(self.next) else {
            return Ok(Flow::Eos);
        };
        ctx.announce_format(OUT, family, &[(field, ValueDesc::Int(value))]);
        let buf = ctx.alloc();
        ctx.out(OUT).push(buf);
        self.next += 1;
        Ok(Flow::Ok)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum How {
    Menu,
    Probe,
    Added(u32),
    AddedProbe(u32),
    Bytes,
}

#[derive(Default)]
struct Heard {
    formats: Vec<i64>,
    buffers: u32,
}

struct Listener {
    how: How,
    heard: Arc<Mutex<Heard>>,
}

impl Listener {
    fn pads(&self) -> u32 {
        match self.how {
            How::Added(n) | How::AddedProbe(n) => n,
            _ => 1,
        }
    }
}

fn probe(ctx: &mut Ctx, pad: SinkPad) -> Result<(), Error> {
    let rates = [ValueSpec::Int(44_100), ValueSpec::Int(48_000)];
    let mods = [ValueSpec::Int(MOD_TILED), ValueSpec::Int(MOD_INVALID)];
    let set = |field, vs| [FieldSpec { field, allowed: ConstraintSpec::Set(vs), preferred: None }];
    let (rate, modifier) = (set("rate", &rates), set("modifier", &mods));
    let offers = [
        OfferSpec { family: "audio/raw", fields: &rate },
        OfferSpec { family: "video/gpu", fields: &modifier },
    ];
    ctx.offer_sink(pad, &offers)
}

impl Element for Listener {
    fn desc(&self) -> &'static ElementDesc {
        match self.how {
            How::Menu => &STATIC,
            How::Probe => &PROBED,
            How::Bytes => &BYTE_SINK,
            How::Added(_) | How::AddedProbe(_) => &MUX,
        }
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        match self.how {
            How::Probe => probe(ctx, IN),
            How::Added(n) => {
                for i in 0..n {
                    ctx.add_sink_pad(&format!("in_{i}"), &MENU);
                }
                Ok(())
            }
            How::AddedProbe(n) => (0..n).try_for_each(|i| {
                let pad = ctx.add_sink_pad(&format!("in_{i}"), OfferDesc::PROBED);
                probe(ctx, pad)
            }),
            How::Menu | How::Bytes => Ok(()),
        }
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut heard = self.heard.lock().unwrap();
        while inputs.pop().is_some() {
            heard.buffers += 1;
        }
        for pad in (0..self.pads()).filter(|_| self.pads() > 1) {
            let batch = ctx.take_input_on(SinkPad(pad));
            heard.buffers += batch.len() as u32;
            ctx.recycle_input(batch);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        let Event::FormatChange(format) = event else {
            return Ok(());
        };
        let of = |name| ctx.field_id(name).and_then(|id| format.get(id));
        if let Some(Value::Int(v)) = of("rate").or(of("modifier")) {
            self.heard.lock().unwrap().formats.push(v);
        }
        Ok(())
    }
}

struct Played {
    result: Result<(), Error>,
    formats: Vec<i64>,
    buffers: u32,
    sink: ElementId,
    posted: Option<(ElementId, Error)>,
    family: Option<String>,
}

fn play(how: How, plans: &[&'static [Step]]) -> Played {
    let mut p = Pipeline::new();
    let heard = Arc::new(Mutex::new(Heard::default()));
    let sink = p.add(Listener { how, heard: Arc::clone(&heard) });
    let talkers: Vec<ElementId> =
        plans.iter().map(|plan| p.add(Talker { plan, next: 0 })).collect();
    p.preroll().expect("preroll");
    for (pad, talker) in talkers.iter().enumerate() {
        p.link(talker.src(OUT), sink.sink(SinkPad(pad as u32))).expect("link");
    }
    let result = p.run();
    let mut posted = None;
    while let Some(msg) = p.bus().try_recv() {
        if let BusMessage::Error { element, error } = msg {
            posted = posted.or(Some((element, error)));
        }
    }
    let family = match &result {
        Err(Error::Format(FormatError::AnnouncementRejected(r))) => {
            p.family_name(r.family).map(str::to_string)
        }
        _ => None,
    };
    let heard = heard.lock().unwrap();
    Played { result, formats: heard.formats.clone(), buffers: heard.buffers, sink, posted, family }
}

fn rejection(played: &Played) -> RejectedFormat {
    let rejected = match &played.result {
        Err(Error::Format(FormatError::AnnouncementRejected(r))) => (**r).clone(),
        other => panic!("expected a rejected format, got {other:?}"),
    };
    match &played.posted {
        Some((element, Error::Format(FormatError::AnnouncementRejected(r)))) => {
            assert_eq!((*element, &**r), (played.sink, &rejected));
        }
        other => panic!("the rejection is posted, got {other:?}"),
    }
    rejected
}

const FULL_MENU: [&str; 2] = [
    "audio/raw, rate={44100, 48000}",
    "video/gpu, modifier={72057594037927938, 72057594037927935}",
];
const MENU_FAMILIES: [&str; 2] = ["audio/raw", "video/gpu"];

const WAYS: [(How, (&str, &str), &[&str]); 4] = [
    (How::Probe, ("device", "sink"), &FULL_MENU),
    (How::Menu, ("menu", "sink"), &MENU_FAMILIES),
    (How::Added(1), ("mux", "in_0"), &MENU_FAMILIES),
    (How::AddedProbe(1), ("mux", "in_0"), &FULL_MENU),
];

#[test]
fn a_rate_inside_the_offers_is_delivered_and_one_outside_is_the_typed_rejection() {
    const INSIDE: &[Step] = &[
        ("audio/raw", "rate", 44_100),
        ("audio/raw", "rate", 48_000),
        ("audio/raw", "rate", 44_100),
    ];
    const OUTSIDE: &[Step] = &[
        ("audio/raw", "rate", 48_000),
        ("audio/raw", "rate", 48_000),
        ("audio/raw", "rate", 96_000),
    ];
    for (how, (name, pad_name), lines) in WAYS {
        let played = play(how, &[INSIDE]);
        played.result.as_ref().unwrap_or_else(|e| panic!("{how:?}: {e}"));
        assert_eq!((&played.formats[..], played.buffers), (&[48_000, 44_100][..], 3), "{how:?}");

        let played = play(how, &[OUTSIDE]);
        let r = rejection(&played);
        assert_eq!((r.element, r.element_name), (played.sink, name), "{how:?}");
        assert_eq!((r.pad, &*r.pad_name), (IN, pad_name), "{how:?}");
        assert_eq!(&*r.announced, "audio/raw, rate=96000", "{how:?}");
        assert_eq!(played.family.as_deref(), Some("audio/raw"), "{how:?}");
        let offers: Vec<&str> = r.offers.iter().map(|l| &**l).collect();
        assert_eq!(offers, lines, "{how:?}");
        assert_eq!(played.formats, [48_000], "{how:?}");
        assert!(played.buffers <= 2, "{how:?}: no buffer in the refused format arrived");
    }
}

#[test]
fn a_modifier_is_held_against_its_set_by_bit_pattern() {
    const INSIDE: &[Step] =
        &[("video/gpu", "modifier", MOD_INVALID), ("video/gpu", "modifier", MOD_TILED)];
    const OUTSIDE: &[Step] =
        &[("video/gpu", "modifier", MOD_INVALID), ("video/gpu", "modifier", 0)];
    for (how, (name, pad_name), lines) in WAYS {
        let played = play(how, &[INSIDE]);
        played.result.as_ref().unwrap_or_else(|e| panic!("{how:?}: {e}"));
        assert_eq!(played.formats, [MOD_INVALID, MOD_TILED], "{how:?}");

        let played = play(how, &[OUTSIDE]);
        let r = rejection(&played);
        assert_eq!((r.element_name, r.pad, &*r.pad_name), (name, IN, pad_name), "{how:?}");
        assert_eq!(&*r.announced, "video/gpu, modifier=0", "{how:?}");
        let offers: Vec<&str> = r.offers.iter().map(|l| &**l).collect();
        assert_eq!(offers, lines, "{how:?}");
        assert_eq!(played.formats, [MOD_INVALID], "{how:?}");
    }
}

#[test]
fn the_rejection_names_the_pad_of_a_fan_in_element_it_arrived_at() {
    const FINE: &[Step] = &[("audio/raw", "rate", 48_000), ("audio/raw", "rate", 44_100)];
    const BAD: &[Step] = &[("audio/raw", "rate", 22_050)];
    for how in [How::Added(2), How::AddedProbe(2)] {
        let played = play(how, &[FINE, FINE]);
        played.result.as_ref().unwrap_or_else(|e| panic!("{how:?}: {e}"));
        assert_eq!((played.formats.len(), played.buffers), (4, 4), "{how:?}");

        let played = play(how, &[FINE, BAD]);
        let r = rejection(&played);
        assert_eq!((r.pad, &*r.pad_name, r.element_name), (SinkPad(1), "in_1", "mux"), "{how:?}");
        assert_eq!(&*r.announced, "audio/raw, rate=22050", "{how:?}");
        assert!(played.formats.iter().all(|v| *v != 22_050), "{how:?}");
    }
}

#[test]
fn a_family_the_pad_does_not_offer_gates_nothing() {
    const REFINED: &[Step] = &[("audio/raw", "rate", 96_000), ("video/gpu", "modifier", 0)];
    let played = play(How::Bytes, &[REFINED]);
    played.result.as_ref().expect("nothing to hold it against");
    assert_eq!((played.formats, played.buffers), (vec![96_000, 0], 2));
}

/// A family the pad offers nothing of is refused once without ending the run, then dropped until
/// one it takes.
#[test]
fn a_family_the_pad_offers_nothing_of_is_refused_once_and_dropped_until_one_it_takes() {
    const SWITCHES: &[Step] = &[
        ("audio/raw", "rate", 48_000),
        ("bytes", "rate", 1),
        ("bytes", "rate", 2),
        ("audio/raw", "rate", 44_100),
    ];
    for how in [How::Menu, How::Probe] {
        let played = play(how, &[SWITCHES]);
        played.result.as_ref().unwrap_or_else(|e| panic!("{how:?}: the run goes on, {e}"));
        assert_eq!(played.formats, [48_000, 44_100], "{how:?}");
        assert_eq!(played.buffers, 2, "{how:?}: the buffers behind the refused family are dropped");
        match &played.posted {
            Some((element, Error::Format(FormatError::FamilyRefused(r)))) => {
                assert_eq!(*element, played.sink, "{how:?}");
                assert!(r.announced.starts_with("bytes"), "{how:?}: {}", r.announced);
            }
            other => panic!("{how:?}: the refusal is posted, got {other:?}"),
        }
    }
}
