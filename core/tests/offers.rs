use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::{Error, LinkEnd, LinkError, NoCommonFormat, OfferFault};
use profluens_core::format::{
    ConstraintDesc, ConstraintSpec, FieldDesc, FieldSpec, MAX_PROBED_OFFERS, OfferDesc, OfferSpec,
    PROBED, Value, ValueDesc, ValueSpec,
};
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
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

static BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];
static PROBED_SRC: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];
static PROBED_SINK: [PadDesc; 1] = [PadDesc { name: "sink", offers: OfferDesc::PROBED }];
static BYTE_SINK: [PadDesc; 1] = [PadDesc { name: "in", offers: &BYTES }];

static B_MODS: [ValueDesc; 2] = [ValueDesc::Int(1), ValueDesc::Int(2)];
static B_FIELDS: [FieldDesc; 2] = [
    FieldDesc { field: "pixfmt", allowed: ConstraintDesc::Eq(ValueDesc::Id("b")), preferred: None },
    FieldDesc { field: "modifier", allowed: ConstraintDesc::Set(&B_MODS), preferred: None },
];
static B_OFFERS: [OfferDesc; 1] = [OfferDesc { family: "video/gpu", fields: &B_FIELDS }];
static B_SINK: [PadDesc; 1] = [PadDesc { name: "sink", offers: &B_OFFERS }];

static CAMERA: ElementDesc = desc("camera", &[], &PROBED_SRC);
static SCREEN: ElementDesc = desc("screen", &PROBED_SINK, &[]);
static FIXED_SCREEN: ElementDesc = desc("fixedscreen", &B_SINK, &[]);
static BYTE_SCREEN: ElementDesc = desc("bytescreen", &BYTE_SINK, &[]);
static DEMUX: ElementDesc = desc("splitter", &BYTE_SINK, &[]);

#[derive(Clone)]
struct Mode {
    pixfmt: String,
    modifiers: Vec<u64>,
}

fn mode(pixfmt: &str, modifiers: &[u64]) -> Mode {
    Mode { pixfmt: pixfmt.to_string(), modifiers: modifiers.to_vec() }
}

fn with_offers<R>(modes: &[Mode], f: impl FnOnce(&[OfferSpec<'_>]) -> R) -> R {
    let sets: Vec<Vec<ValueSpec<'_>>> = modes
        .iter()
        .map(|m| m.modifiers.iter().map(|&x| ValueSpec::Int(x as i64)).collect())
        .collect();
    let fields: Vec<[FieldSpec<'_>; 2]> = modes
        .iter()
        .zip(&sets)
        .map(|(m, set)| {
            let pixfmt = ConstraintSpec::Eq(ValueSpec::Id(&m.pixfmt));
            [
                FieldSpec { field: "pixfmt", allowed: pixfmt, preferred: None },
                FieldSpec { field: "modifier", allowed: ConstraintSpec::Set(set), preferred: None },
            ]
        })
        .collect();
    let offers: Vec<OfferSpec<'_>> =
        fields.iter().map(|f| OfferSpec { family: "video/gpu", fields: f }).collect();
    f(&offers)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quirk {
    None,
    Twice,
    WrongPad,
    NoSuchPad,
    FailOnce,
    AddPad,
    Silent,
    Reserved,
    TooMany,
}

#[derive(Default)]
struct Seen {
    prerolls: AtomicU32,
    negotiated: Mutex<Option<(String, i64)>>,
    late: Mutex<Option<Error>>,
}

struct Device {
    desc: &'static ElementDesc,
    modes: Vec<Mode>,
    quirk: Quirk,
    seen: Arc<Seen>,
}

impl Device {
    fn supply(&self, ctx: &mut Ctx) -> Result<(), Error> {
        let src = self.desc.sinks.is_empty() || self.quirk == Quirk::AddPad;
        with_offers(&self.modes, |offers| match (self.quirk, src) {
            (Quirk::WrongPad, _) => ctx.offer_sink(IN, offers),
            (Quirk::NoSuchPad, _) => ctx.offer_src(SrcPad(7), offers),
            (Quirk::AddPad, _) => {
                let pad = ctx.add_src_pad("video_0", OfferDesc::PROBED);
                ctx.offer_src(pad, offers)
            }
            (_, true) => ctx.offer_src(OUT, offers),
            (_, false) => ctx.offer_sink(IN, offers),
        })
    }
}

impl Element for Device {
    fn desc(&self) -> &'static ElementDesc {
        self.desc
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let nth = self.seen.prerolls.fetch_add(1, Ordering::Relaxed);
        match self.quirk {
            Quirk::Silent => return Ok(()),
            Quirk::Reserved => return ctx.offer_src(OUT, &[OfferSpec::any(PROBED)]),
            Quirk::TooMany => {
                return ctx.offer_src(OUT, &vec![OfferSpec::any("bytes"); MAX_PROBED_OFFERS + 1]);
            }
            _ => {}
        }
        self.supply(ctx)?;
        match self.quirk {
            Quirk::Twice => self.supply(ctx),
            Quirk::FailOnce if nth == 0 => Err(Error::element_failed(ctx.element(), NoDevice)),
            _ => Ok(()),
        }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        let format = match self.desc.sinks.is_empty() {
            true => ctx.negotiated_src(OUT),
            false => ctx.negotiated_sink(IN),
        };
        let field = |name| ctx.field_id(name).and_then(|id| format?.get(id));
        if let (Some(Value::Id(p)), Some(Value::Int(m))) = (field("pixfmt"), field("modifier")) {
            let pixfmt = ctx.value_name(p).unwrap_or("?").to_string();
            *self.seen.negotiated.lock().unwrap() = Some((pixfmt, m));
        }
        *self.seen.late.lock().unwrap() = self.supply(ctx).err();
        Ok(())
    }

    fn process(&mut self, _ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while inputs.pop().is_some() {}
        Ok(if self.desc.sinks.is_empty() { Flow::Eos } else { Flow::Ok })
    }
}

#[derive(Debug)]
struct NoDevice;

impl std::fmt::Display for NoDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no device")
    }
}

impl std::error::Error for NoDevice {}

fn add(
    p: &mut Pipeline,
    desc: &'static ElementDesc,
    modes: &[Mode],
    quirk: Quirk,
) -> (ElementId, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let device = Device { desc, modes: modes.to_vec(), quirk, seen: Arc::clone(&seen) };
    (p.add(device), seen)
}

fn link_err(r: Result<LinkId, Error>) -> LinkError {
    match r {
        Err(Error::Link(e)) => e,
        other => panic!("expected a link error, got {other:?}"),
    }
}

fn offer_err(e: Option<Error>) -> (ElementId, bool, u32, OfferFault) {
    match e {
        Some(Error::Offer { element, src, pad, fault }) => (element, src, pad, fault),
        other => panic!("expected an offer error, got {other:?}"),
    }
}

fn fixed(p: &Pipeline, link: LinkId) -> (String, i64) {
    let f = p.negotiated(link).expect("fixed");
    assert_eq!(p.family_name(f.family), Some("video/gpu"));
    assert_eq!(f.len(), 2, "the pixel format, the modifier and nothing else");
    let get = |name| f.get(p.field_id(name).expect("interned")).expect("fixed");
    match (get("pixfmt"), get("modifier")) {
        (Value::Id(pix), Value::Int(m)) => (p.value_name(pix).expect("named").to_string(), m),
        other => panic!("unexpected values {other:?}"),
    }
}

#[test]
fn a_pending_pad_refuses_to_link_and_names_itself() {
    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[mode("a", &[1])], Quirk::None);
    let fixed_screen = p.add(Device {
        desc: &FIXED_SCREEN,
        modes: Vec::new(),
        quirk: Quirk::Silent,
        seen: Arc::default(),
    });
    let (screen, _) = add(&mut p, &SCREEN, &[mode("a", &[1])], Quirk::None);

    let pending = |element, element_name, pad: &str| LinkError::OffersPending {
        element,
        element_name,
        pad: pad.into(),
    };
    let got = link_err(p.link(cam.src(OUT), fixed_screen.sink(IN)));
    assert_eq!(got, pending(cam, "camera", "src"));
    assert!(got.to_string().contains("'camera' pad 'src'"), "{got}");
    assert!(got.to_string().contains("no offers yet, preroll first"), "{got}");
    assert_eq!(link_err(p.link(cam.src(OUT), screen.sink(IN))), pending(cam, "camera", "src"));

    p.preroll().expect("preroll");
    p.link(cam.src(OUT), screen.sink(IN)).expect("supplied, so it links");
}

#[test]
fn a_pad_whose_element_supplied_nothing_stays_pending() {
    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[], Quirk::Silent);
    let (screen, _) = add(&mut p, &SCREEN, &[mode("a", &[1])], Quirk::None);
    p.preroll().expect("supplying nothing is not a preroll failure");
    assert!(matches!(
        link_err(p.link(cam.src(OUT), screen.sink(IN))),
        LinkError::OffersPending { element, .. } if element == cam
    ));
}

#[test]
fn a_link_after_preroll_fixes_from_what_was_supplied() {
    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[mode("a", &[5, 6]), mode("b", &[6])], Quirk::None);
    let (screen, _) = add(&mut p, &SCREEN, &[mode("b", &[6]), mode("a", &[6, 5])], Quirk::None);
    p.preroll().expect("preroll");
    let link = p.link(cam.src(OUT), screen.sink(IN)).expect("links");
    assert_eq!(fixed(&p, link), ("a".to_string(), 5));
}

#[test]
fn pairs_are_separate_offers() {
    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[mode("a", &[1, 2]), mode("b", &[2, 3])], Quirk::None);
    let (screen, _) = add(&mut p, &FIXED_SCREEN, &[], Quirk::Silent);
    p.preroll().expect("preroll");
    let link = p.link(cam.src(OUT), screen.sink(IN)).expect("b with 2 is common");
    assert_eq!(fixed(&p, link), ("b".to_string(), 2));

    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[mode("b", &[1, 2])], Quirk::None);
    let (screen, _) = add(&mut p, &SCREEN, &[mode("a", &[1, 2]), mode("b", &[2, 3])], Quirk::None);
    p.preroll().expect("preroll");
    let link = p.link(cam.src(OUT), screen.sink(IN)).expect("b with 2 is common");
    assert_eq!(fixed(&p, link), ("b".to_string(), 2));
}

#[test]
fn a_modifier_survives_as_its_bit_pattern() {
    let (invalid, vendor) = (0x00ff_ffff_ffff_ffff_u64, 0x8100_0000_0000_0002_u64);
    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[mode("a", &[vendor, invalid])], Quirk::None);
    let (screen, _) = add(&mut p, &SCREEN, &[mode("a", &[vendor])], Quirk::None);
    p.preroll().expect("preroll");
    let link = p.link(cam.src(OUT), screen.sink(IN)).expect("links");
    assert_eq!(fixed(&p, link).1 as u64, vendor);
}

#[test]
fn no_common_offer_names_both_lists() {
    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[mode("a", &[1, 2]), mode("b", &[3])], Quirk::None);
    let (screen, _) = add(&mut p, &SCREEN, &[mode("a", &[3]), mode("b", &[1])], Quirk::None);
    let (fixed_screen, _) = add(&mut p, &FIXED_SCREEN, &[], Quirk::Silent);
    p.preroll().expect("preroll");

    let end = |element, element_name, pad: &str, offers: &[&str]| LinkEnd {
        element,
        element_name,
        pad: pad.into(),
        offers: offers.iter().map(|o| (*o).into()).collect(),
        bound: None,
    };
    let cam_offers = ["video/gpu, pixfmt=a, modifier={1, 2}", "video/gpu, pixfmt=b, modifier={3}"];
    let got = link_err(p.link(cam.src(OUT), screen.sink(IN)));
    let want = NoCommonFormat {
        src: end(cam, "camera", "src", &cam_offers),
        sink: end(
            screen,
            "screen",
            "sink",
            &["video/gpu, pixfmt=a, modifier={3}", "video/gpu, pixfmt=b, modifier={1}"],
        ),
    };
    assert_eq!(got, LinkError::NoCommonFormat(want.into()));
    let text = got.to_string();
    assert!(text.contains("pixfmt=a, modifier={1, 2}"), "{text}");
    assert!(text.contains("pixfmt=b, modifier={1}"), "{text}");
    assert!(!text.contains('\n') && !text.contains(PROBED), "{text}");

    let got = link_err(p.link(cam.src(OUT), fixed_screen.sink(IN)));
    let want = NoCommonFormat {
        src: end(cam, "camera", "src", &cam_offers),
        sink: end(fixed_screen, "fixedscreen", "sink", &["video/gpu"]),
    };
    assert_eq!(got, LinkError::NoCommonFormat(want.into()));
    let (other, _) = add(&mut p, &SCREEN, &[mode("b", &[3])], Quirk::None);
    p.preroll().expect("preroll");
    p.link(cam.src(OUT), other.sink(IN)).expect("b with 3");
}

#[test]
fn an_element_is_prerolled_once_and_a_later_call_covers_what_was_added_since() {
    let mut p = Pipeline::new();
    let (cam, cam_seen) = add(&mut p, &CAMERA, &[mode("a", &[1])], Quirk::None);
    p.preroll().expect("first");
    p.preroll().expect("second, with nothing new");
    assert_eq!(cam_seen.prerolls.load(Ordering::Relaxed), 1);

    let (screen, screen_seen) = add(&mut p, &SCREEN, &[mode("a", &[1])], Quirk::None);
    assert!(matches!(
        link_err(p.link(cam.src(OUT), screen.sink(IN))),
        LinkError::OffersPending { element, .. } if element == screen
    ));
    p.preroll().expect("third, for the screen");
    p.preroll().expect("fourth");
    assert_eq!(cam_seen.prerolls.load(Ordering::Relaxed), 1, "not asked again");
    assert_eq!(screen_seen.prerolls.load(Ordering::Relaxed), 1);
    p.link(cam.src(OUT), screen.sink(IN)).expect("links");
}

#[test]
fn a_failed_preroll_keeps_nothing_and_is_asked_again() {
    let mut p = Pipeline::new();
    let (cam, cam_seen) = add(&mut p, &CAMERA, &[mode("a", &[1])], Quirk::FailOnce);
    let (screen, screen_seen) = add(&mut p, &SCREEN, &[mode("a", &[1])], Quirk::None);
    assert!(matches!(p.preroll(), Err(Error::ElementFailed { element, .. }) if element == cam));
    assert_eq!(screen_seen.prerolls.load(Ordering::Relaxed), 0, "the pass stopped at the failure");
    assert!(matches!(
        link_err(p.link(cam.src(OUT), screen.sink(IN))),
        LinkError::OffersPending { element, .. } if element == cam
    ));
    p.preroll().expect("the retry");
    assert_eq!(cam_seen.prerolls.load(Ordering::Relaxed), 2);
    assert_eq!(screen_seen.prerolls.load(Ordering::Relaxed), 1);
    p.link(cam.src(OUT), screen.sink(IN)).expect("links");
}

#[test]
fn offers_are_refused_twice_for_a_pad_not_probed_and_past_a_limit() {
    let cases = [
        (&CAMERA, Quirk::Twice, true, 0, OfferFault::Twice),
        (&CAMERA, Quirk::WrongPad, false, 0, OfferFault::NotProbed),
        (&BYTE_SCREEN, Quirk::None, false, 0, OfferFault::NotProbed),
        (&CAMERA, Quirk::NoSuchPad, true, 7, OfferFault::NotProbed),
        (&CAMERA, Quirk::Reserved, true, 0, OfferFault::ReservedFamily),
        (
            &CAMERA,
            Quirk::TooMany,
            true,
            0,
            OfferFault::OverLimit {
                what: "offers",
                found: MAX_PROBED_OFFERS as u32 + 1,
                limit: MAX_PROBED_OFFERS as u32,
            },
        ),
    ];
    for (desc, quirk, src, pad, fault) in cases {
        let mut p = Pipeline::new();
        let (el, _) = add(&mut p, desc, &[mode("a", &[1])], quirk);
        let got = p.preroll().expect_err("the element's preroll returns the refusal");
        let text = got.to_string();
        assert_eq!(offer_err(Some(got)), (el, src, pad, fault));
        assert!(text.contains(if src { "src pad" } else { "sink pad" }), "{text}");
        assert!(!text.contains('\n') && !text.ends_with(' '), "{text}");
        if desc.srcs.len() == 1 {
            let (screen, _) = add(&mut p, &BYTE_SCREEN, &[], Quirk::Silent);
            assert!(matches!(
                link_err(p.link(el.src(OUT), screen.sink(IN))),
                LinkError::OffersPending { .. }
            ));
        }
    }
}

#[test]
fn offers_survive_into_the_run_and_are_frozen_there() {
    let mut p = Pipeline::new();
    let (cam, cam_seen) =
        add(&mut p, &CAMERA, &[mode("a", &[1, 2]), mode("b", &[2, 3])], Quirk::None);
    let (screen, screen_seen) = add(&mut p, &SCREEN, &[mode("b", &[3, 9])], Quirk::None);
    p.preroll().expect("preroll");
    let link = p.link(cam.src(OUT), screen.sink(IN)).expect("links");
    assert_eq!(fixed(&p, link), ("b".to_string(), 3));
    p.run().expect("runs to the end");

    let want = Some(("b".to_string(), 3));
    assert_eq!(*cam_seen.negotiated.lock().unwrap(), want, "the src side reads it by name");
    assert_eq!(*screen_seen.negotiated.lock().unwrap(), want, "and so does the sink side");
    for (el, seen, src) in [(cam, cam_seen, true), (screen, screen_seen, false)] {
        let late = seen.late.lock().unwrap().take();
        assert_eq!(offer_err(late), (el, src, 0, OfferFault::Frozen));
    }
}

#[test]
fn a_pad_added_at_preroll_takes_runtime_offers() {
    let mut p = Pipeline::new();
    let (demux, _) = add(&mut p, &DEMUX, &[mode("a", &[4])], Quirk::AddPad);
    let (screen, _) = add(&mut p, &SCREEN, &[mode("a", &[4, 5])], Quirk::None);
    let added = p.preroll().expect("preroll");
    assert_eq!(added.srcs.len(), 1);
    assert_eq!(
        (added.srcs[0].element, added.srcs[0].pad, &*added.srcs[0].name),
        (demux, OUT, "video_0")
    );
    let announced = std::iter::from_fn(|| p.bus().try_recv()).find_map(|m| match m {
        BusMessage::SrcPadAdded { format, .. } => Some(format.family),
        _ => None,
    });
    assert_eq!(announced.and_then(|f| p.family_name(f)), Some("video/gpu"), "its first family");
    assert_eq!(p.src_families(demux, OUT), None, "a probed pad declares no families");
    let link = p.link(demux.src(added.srcs[0].pad), screen.sink(IN)).expect("links");
    assert_eq!(fixed(&p, link), ("a".to_string(), 4));
}

#[test]
fn the_dump_and_the_description_show_probed_offers_or_pending() {
    let mut p = Pipeline::new();
    let (cam, _) = add(&mut p, &CAMERA, &[mode("a", &[1, 2]), mode("b", &[2])], Quirk::None);
    let (screen, _) = add(&mut p, &SCREEN, &[mode("b", &[2])], Quirk::None);
    let dot = p.dump_dot();
    assert!(dot.contains("label=\"camera\\nsrc: pending\""), "{dot}");
    assert!(dot.contains("label=\"screen\\nsink: pending\""), "{dot}");

    p.preroll().expect("preroll");
    p.link(cam.src(OUT), screen.sink(IN)).expect("links");
    let dot = p.dump_dot();
    let cam_label = "label=\"camera\\nsrc: video/gpu, pixfmt=a, modifier={1, 2} | \
                     video/gpu, pixfmt=b, modifier={2}\"";
    assert!(dot.contains(cam_label), "{dot}");
    assert!(dot.contains("label=\"screen\\nsink: video/gpu, pixfmt=b, modifier={2}\""), "{dot}");
    assert!(dot.contains("e0 -> e1 [label=\"video/gpu, pixfmt=b, modifier=2\"]"), "{dot}");
    assert!(!dot.contains(PROBED), "the marker is never shown\n{dot}");
}
