//! What the fake elements share. Descriptors, declared pads as offers core takes, and their error.

use core::fmt;

use profluens_core::element::{ElementDesc, InputPolicy, LatencyDesc, PadDesc, SchedHint};
use profluens_core::error::Error;
use profluens_core::format::{
    ConstraintDesc, ConstraintSpec, FieldDesc, FieldSpec, OfferDesc, OfferSpec, ValueSpec,
};
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

use super::offers::{BYTES, FAMILIES, FIELD, MAX_IDS, MAX_OFFERS, PadOffers};
use super::retime::LATENCIES;
use crate::time::Ns;

static FIELDS: [FieldDesc; 1] =
    [FieldDesc { field: FIELD, allowed: ConstraintDesc::Any, preferred: None }];
pub(crate) static MEDIA: [OfferDesc; 1] = [OfferDesc { family: FAMILIES[0], fields: &FIELDS }];
static WILD: [OfferDesc; 1] = [OfferDesc::wildcard()];

const fn pad(name: &'static str, probed: bool) -> PadDesc {
    PadDesc { name, offers: if probed { OfferDesc::PROBED } else { &MEDIA } }
}

const fn wild(name: &'static str) -> PadDesc {
    PadDesc { name, offers: &WILD }
}

/// An offer that takes any id still names the field, so the name is interned for later
/// announcements.
pub fn supply(
    pad: &PadOffers,
    give: impl FnOnce(&[OfferSpec<'_>]) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut ids = [[ValueSpec::Int(0); MAX_IDS]; MAX_OFFERS];
    for (slot, offer) in ids.iter_mut().zip(&pad.list) {
        for (value, id) in slot.iter_mut().zip(offer.ids()) {
            *value = ValueSpec::Int(*id as i64);
        }
    }
    let fields: [[FieldSpec<'_>; 1]; MAX_OFFERS] = core::array::from_fn(|n| {
        let offer = &pad.list[n];
        let int = |id: u8| ValueSpec::Int(id as i64);
        let allowed = match (offer.ids().len(), offer.step) {
            (0, _) => ConstraintSpec::Any,
            (_, 0) => ConstraintSpec::Set(&ids[n][..offer.ids().len()]),
            (_, step) => ConstraintSpec::Range {
                min: int(offer.ids[0]),
                max: int(offer.ids[1]),
                step: int(step),
            },
        };
        [FieldSpec { field: FIELD, allowed, preferred: None }]
    });
    let offers: [OfferSpec<'_>; MAX_OFFERS] = core::array::from_fn(|n| {
        let family = FAMILIES[(pad.list[n].family as usize).min(FAMILIES.len() - 1)];
        let fields = if pad.list[n].family == BYTES { &[][..] } else { &fields[n][..] };
        OfferSpec { family, fields }
    });
    give(&offers[..(pad.count as usize).min(MAX_OFFERS)])
}

const fn desc(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    sched: SchedHint,
) -> ElementDesc {
    slow(name, sinks, srcs, sched, 0)
}

const fn slow(
    name: &'static str,
    sinks: &'static [PadDesc],
    srcs: &'static [PadDesc],
    sched: SchedHint,
    latency: Ns,
) -> ElementDesc {
    let (held, none) = (Timestamp(latency), Timestamp::ZERO);
    ElementDesc {
        name,
        sinks,
        srcs,
        props: &[],
        sched,
        inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
        latency: LatencyDesc { min: held, max: held, is_live: false, jitter: none },
        make_default: None,
        io: false,
        couplings: &[],
    }
}

// Indexed by which pads are probed, bit `n` for pad `n`.
static ONE_IN: [[PadDesc; 1]; 2] = [[pad("sink", false)], [pad("sink", true)]];
static ONE_OUT: [[PadDesc; 1]; 2] = [[pad("src", false)], [pad("src", true)]];
static TWO_OUT: [[PadDesc; 2]; 4] = [
    [pad("a", false), pad("b", false)],
    [pad("a", true), pad("b", false)],
    [pad("a", false), pad("b", true)],
    [pad("a", true), pad("b", true)],
];
static WILD_IN: [PadDesc; 1] = [wild("sink")];
static WILD_OUT: [PadDesc; 1] = [wild("src")];
static WILD_TWO_OUT: [PadDesc; 2] = [wild("a"), wild("b")];

const fn net(d: ElementDesc) -> ElementDesc {
    ElementDesc { io: true, ..d }
}

pub static SOURCE_1: [[ElementDesc; 2]; 2] = [
    [
        desc("fakesrc", &[], &ONE_OUT[0], SchedHint::Active),
        desc("fakesrc", &[], &ONE_OUT[1], SchedHint::Active),
    ],
    [
        net(desc("fakesrc", &[], &ONE_OUT[0], SchedHint::Active)),
        net(desc("fakesrc", &[], &ONE_OUT[1], SchedHint::Active)),
    ],
];
pub static SOURCE_2: [[ElementDesc; 4]; 2] = [
    [
        desc("fakesrc2", &[], &TWO_OUT[0], SchedHint::Active),
        desc("fakesrc2", &[], &TWO_OUT[1], SchedHint::Active),
        desc("fakesrc2", &[], &TWO_OUT[2], SchedHint::Active),
        desc("fakesrc2", &[], &TWO_OUT[3], SchedHint::Active),
    ],
    [
        net(desc("fakesrc2", &[], &TWO_OUT[0], SchedHint::Active)),
        net(desc("fakesrc2", &[], &TWO_OUT[1], SchedHint::Active)),
        net(desc("fakesrc2", &[], &TWO_OUT[2], SchedHint::Active)),
        net(desc("fakesrc2", &[], &TWO_OUT[3], SchedHint::Active)),
    ],
];
pub static SOURCE_STREAMS: ElementDesc = desc("fakedemux", &[], &[], SchedHint::Active);
pub static TRANSFORM: ElementDesc = desc("fakexf", &WILD_IN, &WILD_OUT, SchedHint::Active);
pub static INLINE: ElementDesc = desc("fakexfinline", &WILD_IN, &WILD_OUT, SchedHint::Passive);
pub static TEE: ElementDesc = desc("fakexf2", &WILD_IN, &WILD_TWO_OUT, SchedHint::Active);
pub static DECODER: ElementDesc = desc("fakedec", &WILD_IN, &WILD_OUT, SchedHint::Active);
pub static DECODER_INLINE: ElementDesc =
    desc("fakedecinline", &WILD_IN, &WILD_OUT, SchedHint::Passive);
static RETIMERS: [[ElementDesc; LATENCIES.len()]; 2] = [
    [
        slow("fakeretimer", &WILD_IN, &WILD_OUT, SchedHint::Active, LATENCIES[0]),
        slow("fakeretimer", &WILD_IN, &WILD_OUT, SchedHint::Active, LATENCIES[1]),
        slow("fakeretimer", &WILD_IN, &WILD_OUT, SchedHint::Active, LATENCIES[2]),
    ],
    [
        slow("fakeretimerinline", &WILD_IN, &WILD_OUT, SchedHint::Passive, LATENCIES[0]),
        slow("fakeretimerinline", &WILD_IN, &WILD_OUT, SchedHint::Passive, LATENCIES[1]),
        slow("fakeretimerinline", &WILD_IN, &WILD_OUT, SchedHint::Passive, LATENCIES[2]),
    ],
];

pub fn retimer_desc(inline: bool, latency: Ns) -> &'static ElementDesc {
    let known = LATENCIES.iter().position(|held| *held == latency).unwrap_or(0);
    &RETIMERS[inline as usize][known]
}

pub static SINK: [ElementDesc; 2] = [
    desc("fakesink", &ONE_IN[0], &[], SchedHint::Active),
    desc("fakesink", &ONE_IN[1], &[], SchedHint::Active),
];

pub const IN: SinkPad = SinkPad(0);
pub const OUT: SrcPad = SrcPad(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeError {
    DeclaredSource {
        stream: u16,
        buffer: u64,
    },
    DeclaredTransform {
        input: u64,
    },
    TapFull {
        sink: u16,
    },
    SlotTooSmall {
        capacity: usize,
    },
    MissingReference {
        picture: u64,
        needs: u64,
    },
    Untagged,
    NetBytes {
        at: u64,
    },
    /// A receive completed with a count that is not the buffer's length.
    NetShort {
        said: usize,
        got: usize,
    },
    NetStale,
    NetDead {
        attempts: u32,
    },
    StaleFeedback {
        made_in: u64,
        applied: u64,
    },
}

impl FakeError {
    pub const fn declared(self) -> bool {
        matches!(self, Self::DeclaredSource { .. } | Self::DeclaredTransform { .. })
    }
}

impl fmt::Display for FakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeclaredSource { stream, buffer } => {
                write!(f, "declared source error at buffer {buffer} of stream {stream}")
            }
            Self::DeclaredTransform { input } => {
                write!(f, "declared transform error at input {input}")
            }
            Self::TapFull { sink } => write!(f, "the tap of sink {sink} is full"),
            Self::SlotTooSmall { capacity } => {
                write!(f, "a pool slot of {capacity} bytes holds no tag")
            }
            Self::MissingReference { picture, needs } => {
                write!(
                    f,
                    "picture {picture} is decoded from picture {needs}, which was not decoded"
                )
            }
            Self::Untagged => f.write_str("the decoder was fed a buffer with no tag"),
            Self::NetBytes { at } => {
                write!(f, "byte {at} of the network stream is not the stream's")
            }
            Self::NetShort { said, got } => {
                write!(f, "a receive completed with {said} bytes and a buffer of {got}")
            }
            Self::NetStale => f.write_str("a cancelled receive completed with bytes"),
            Self::NetDead { attempts } => write!(f, "the network failed {attempts} times in a row"),
            Self::StaleFeedback { made_in, applied } => {
                write!(f, "told the lateness of seek {made_in} behind seek {applied}")
            }
        }
    }
}

impl std::error::Error for FakeError {}
