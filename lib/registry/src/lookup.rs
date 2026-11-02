use profluens_core::element::{ElementDesc, PadDesc};
use profluens_core::format::{PROBED, WILDCARD};

/// One plugin crate's descriptors, its `ELEMENTS`.
pub type Elements = &'static [&'static ElementDesc];

/// The family of opaque bytes, which every codec element takes as a fallback.
pub const BYTES: &str = "bytes";

/// Decoded media, the family a decoder puts out and an encoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raw {
    Audio,
    Video,
}

impl Raw {
    pub const ALL: [Raw; 2] = [Raw::Audio, Raw::Video];

    pub const fn family(self) -> &'static str {
        match self {
            Raw::Audio => "audio/raw",
            Raw::Video => "video/raw",
        }
    }

    pub fn of(family: &str) -> Option<Raw> {
        Raw::ALL.into_iter().find(|r| r.family() == family)
    }
}

/// Whether `family` names a coded format, the only kind a lookup answers for.
pub fn is_coded(family: &str) -> bool {
    !matches!(family, BYTES | WILDCARD | PROBED | "") && Raw::of(family).is_none()
}

fn offers(pads: &[PadDesc], family: &str) -> bool {
    pads.iter().any(|p| p.offers.iter().any(|o| o.family == family))
}

#[derive(Clone, Copy)]
enum Out<'a> {
    AnyRaw,
    Family(&'a str),
}

fn matching<'a>(
    sets: &'a [Elements],
    sink: &'a str,
    out: Out<'a>,
) -> impl Iterator<Item = &'static ElementDesc> + 'a {
    sets.iter().flat_map(|set| set.iter().copied()).filter(move |d| {
        d.make_default.is_some()
            && offers(d.sinks, sink)
            && match out {
                Out::AnyRaw => Raw::ALL.iter().any(|r| offers(d.srcs, r.family())),
                Out::Family(f) => offers(d.srcs, f),
            }
    })
}

/// Constructible elements taking `family` and putting out raw audio or video. Sets are searched
/// in the order given, each in its own order, so the caller puts the set it prefers first.
pub fn decoders_for<'a>(
    sets: &'a [Elements],
    family: &'a str,
) -> impl Iterator<Item = &'static ElementDesc> + 'a {
    // No offer names "", so an uncoded family matches nothing.
    let family = if is_coded(family) { family } else { "" };
    matching(sets, family, Out::AnyRaw)
}

pub fn decoder_for(sets: &[Elements], family: &str) -> Option<&'static ElementDesc> {
    decoders_for(sets, family).next()
}

/// Constructible elements taking `raw` and putting out `family`, in [`decoders_for`] order.
pub fn encoders_for<'a>(
    sets: &'a [Elements],
    raw: Raw,
    family: &'a str,
) -> impl Iterator<Item = &'static ElementDesc> + 'a {
    let family = if is_coded(family) { family } else { "" };
    matching(sets, raw.family(), Out::Family(family))
}

pub fn encoder_for(sets: &[Elements], raw: Raw, family: &str) -> Option<&'static ElementDesc> {
    encoders_for(sets, raw, family).next()
}

/// The raw kind a coded `family` decodes to, by its decoder's src offers.
pub fn raw_of(sets: &[Elements], family: &str) -> Option<Raw> {
    let dec = decoder_for(sets, family)?;
    Raw::ALL.into_iter().find(|r| offers(dec.srcs, r.family()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use profluens_core::batch::Inputs;
    use profluens_core::ctx::Ctx;
    use profluens_core::element::{Element, Flow, InputPolicy, LatencyDesc, SchedHint};
    use profluens_core::error::Error;
    use profluens_core::format::OfferDesc;

    struct Nop;
    impl Element for Nop {
        fn desc(&self) -> &'static ElementDesc {
            &DEC_A
        }
        fn process(&mut self, _c: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
            Ok(Flow::Eos)
        }
    }

    const fn desc(
        name: &'static str,
        sinks: &'static [PadDesc],
        srcs: &'static [PadDesc],
        make: bool,
    ) -> ElementDesc {
        ElementDesc {
            name,
            sinks,
            srcs,
            props: &[],
            sched: SchedHint::Passive,
            inputs: InputPolicy::Single,
            latency: LatencyDesc::NONE,
            #[allow(clippy::disallowed_methods)]
            make_default: if make { Some(|| Box::new(Nop)) } else { None },
            io: false,
            couplings: &[],
        }
    }

    static X: [OfferDesc; 2] = [OfferDesc::any("x"), OfferDesc::any(BYTES)];
    static PCM: [OfferDesc; 2] = [OfferDesc::any("audio/raw"), OfferDesc::any(BYTES)];
    static X_PAD: [PadDesc; 1] = [PadDesc { name: "sink", offers: &X }];
    static X_SRC: [PadDesc; 1] = [PadDesc { name: "src", offers: &X }];
    static PCM_SINK: [PadDesc; 1] = [PadDesc { name: "sink", offers: &PCM }];
    static PCM_SRC: [PadDesc; 1] = [PadDesc { name: "src", offers: &PCM }];

    static DEC_A: ElementDesc = desc("deca", &X_PAD, &PCM_SRC, true);
    static DEC_B: ElementDesc = desc("decb", &X_PAD, &PCM_SRC, true);
    static DEC_TYPED: ElementDesc = desc("dectyped", &X_PAD, &PCM_SRC, false);
    static ENC: ElementDesc = desc("enc", &PCM_SINK, &X_SRC, true);
    static CONVERT: ElementDesc = desc("convert", &PCM_SINK, &PCM_SRC, true);

    static FIRST: [&ElementDesc; 3] = [&DEC_TYPED, &DEC_A, &CONVERT];
    static SECOND: [&ElementDesc; 2] = [&DEC_B, &ENC];

    #[test]
    fn the_first_constructible_match_wins_in_set_order() {
        assert_eq!(decoder_for(&[&FIRST, &SECOND], "x").map(|d| d.name), Some("deca"));
        assert_eq!(decoder_for(&[&SECOND, &FIRST], "x").map(|d| d.name), Some("decb"));
        assert_eq!(encoder_for(&[&FIRST, &SECOND], Raw::Audio, "x").map(|d| d.name), Some("enc"));
        assert_eq!(raw_of(&[&FIRST], "x"), Some(Raw::Audio));
        let all: Vec<&str> = decoders_for(&[&FIRST, &SECOND], "x").map(|d| d.name).collect();
        assert_eq!(all, ["deca", "decb"]);
    }

    #[test]
    fn raw_and_opaque_families_have_no_codec() {
        for family in ["audio/raw", "video/raw", BYTES, WILDCARD, PROBED, "", "y"] {
            assert!(decoder_for(&[&FIRST, &SECOND], family).is_none(), "{family}");
            assert!(encoder_for(&[&FIRST, &SECOND], Raw::Audio, family).is_none(), "{family}");
        }
        assert!(encoder_for(&[&FIRST, &SECOND], Raw::Video, "x").is_none());
        assert_eq!(Raw::of("video/raw"), Some(Raw::Video));
        assert_eq!(Raw::of("x"), None);
    }
}
