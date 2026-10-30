use crate::element::ElementDesc;
use crate::format::ValueDesc;

/// The sink pad index a rule reads no sink pad for, a property's or a fixed value's.
pub const NO_PAD: u32 = u32::MAX;

/// How one field of a src pad follows the element's input or configuration.
#[derive(Clone, Copy, Debug)]
pub enum Rule {
    /// `src.field` takes `sink.field`.
    Same,
    /// `src.field` takes `sink.from`.
    Copy { from: &'static str },
    /// `src.field` is the structural property `prop` when set, the pad's own offer otherwise.
    Prop { prop: &'static str },
    /// `src.field` is this value whatever arrives.
    Fixed(ValueDesc),
    /// `src.field` is announced as one of its values that everything downstream takes.
    Takes,
}

/// One relation a transform states between its static pads, read by the solver only.
#[derive(Clone, Copy, Debug)]
pub enum Coupling {
    /// Family and every field pass from sink pad `sink` to src pad `src` unchanged.
    Through { sink: u32, src: u32 },
    /// One field of src pad `src` follows `rule`, `sink` is `NO_PAD` for `Prop` and `Fixed`.
    Field { sink: u32, src: u32, field: &'static str, rule: Rule },
}

impl Coupling {
    pub const fn through(sink: u32, src: u32) -> Self {
        Coupling::Through { sink, src }
    }

    pub const fn same(sink: u32, src: u32, field: &'static str) -> Self {
        Coupling::Field { sink, src, field, rule: Rule::Same }
    }

    pub const fn copy(sink: u32, src: u32, field: &'static str, from: &'static str) -> Self {
        Coupling::Field { sink, src, field, rule: Rule::Copy { from } }
    }

    pub const fn prop(src: u32, field: &'static str, prop: &'static str) -> Self {
        Coupling::Field { sink: NO_PAD, src, field, rule: Rule::Prop { prop } }
    }

    pub const fn fixed(src: u32, field: &'static str, value: ValueDesc) -> Self {
        Coupling::Field { sink: NO_PAD, src, field, rule: Rule::Fixed(value) }
    }

    pub const fn takes(src: u32, field: &'static str) -> Self {
        Coupling::Field { sink: NO_PAD, src, field, rule: Rule::Takes }
    }

    pub const fn src(&self) -> u32 {
        match *self {
            Coupling::Through { src, .. } | Coupling::Field { src, .. } => src,
        }
    }
}

/// The first rule of `desc` that names a pad, a field or a property it has not, and why.
pub fn check(desc: &ElementDesc) -> Result<(), (usize, &'static str)> {
    let (sinks, srcs) = (desc.sinks.len() as u32, desc.srcs.len() as u32);
    for (i, c) in desc.couplings.iter().enumerate() {
        let bad = |why| Err((i, why));
        if c.src() >= srcs {
            return bad("no such static src pad");
        }
        let earlier = &desc.couplings[..i];
        match *c {
            Coupling::Through { sink, src } => {
                if sink >= sinks {
                    return bad("no such static sink pad");
                }
                if earlier.iter().any(|o| o.src() == src) {
                    return bad("another rule writes this src pad");
                }
            }
            Coupling::Field { sink, src, field, rule } => {
                let piped = matches!(rule, Rule::Same | Rule::Copy { .. });
                match (piped, sink) {
                    (true, s) if s >= sinks => return bad("no such static sink pad"),
                    (false, s) if s != NO_PAD => {
                        return bad("a prop, fixed or takes rule reads no sink pad");
                    }
                    _ => {}
                }
                if field.is_empty() || matches!(rule, Rule::Copy { from: "" }) {
                    return bad("an empty field name");
                }
                let twice = |o: &Coupling| match *o {
                    Coupling::Through { src: s, .. } => s == src,
                    Coupling::Field { src: s, field: f, .. } => s == src && f == field,
                };
                if earlier.iter().any(twice) {
                    return bad("two rules write one field");
                }
                if let Rule::Prop { prop } = rule {
                    match desc.props.iter().find(|p| p.name == prop) {
                        None => return bad("no such property"),
                        Some(p) if p.live => return bad("a live property"),
                        Some(_) => {}
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint};
    use crate::format::{Constraint, OfferDesc};

    static PADS: [PadDesc; 1] = [PadDesc { name: "p", offers: &[OfferDesc::wildcard()] }];
    static PROPS: [PropDesc; 2] = [
        PropDesc { name: "rate", allowed: Constraint::Any, live: false },
        PropDesc { name: "gain", allowed: Constraint::Any, live: true },
    ];

    // Leaks the rules so the descriptor borrows them for good, which only a test may do.
    #[allow(clippy::disallowed_methods)]
    fn desc(couplings: &[Coupling]) -> ElementDesc {
        let couplings = Box::leak(couplings.to_vec().into_boxed_slice());
        ElementDesc {
            name: "t",
            sinks: &PADS,
            srcs: &PADS,
            props: &PROPS,
            sched: SchedHint::Passive,
            inputs: InputPolicy::Single,
            latency: LatencyDesc::NONE,
            io: false,
            make_default: None,
            couplings,
        }
    }

    #[test]
    fn every_bad_rule_is_named_by_its_index() {
        type Row<'a> = (&'a [Coupling], Result<(), (usize, &'a str)>);
        let rows: &[Row] = &[
            (&[], Ok(())),
            (&[Coupling::through(0, 0)], Ok(())),
            (&[Coupling::same(0, 0, "rate"), Coupling::copy(0, 0, "w", "h")], Ok(())),
            (
                &[Coupling::prop(0, "rate", "rate"), Coupling::fixed(0, "ch", ValueDesc::Int(2))],
                Ok(()),
            ),
            (&[Coupling::takes(0, "sample")], Ok(())),
            (
                &[Coupling::Field { sink: 0, src: 0, field: "s", rule: Rule::Takes }],
                Err((0, "a prop, fixed or takes rule reads no sink pad")),
            ),
            (&[Coupling::through(1, 0)], Err((0, "no such static sink pad"))),
            (&[Coupling::through(0, 1)], Err((0, "no such static src pad"))),
            (&[Coupling::same(1, 0, "rate")], Err((0, "no such static sink pad"))),
            (&[Coupling::same(NO_PAD, 0, "rate")], Err((0, "no such static sink pad"))),
            (
                &[Coupling::Field {
                    sink: 0,
                    src: 0,
                    field: "r",
                    rule: Rule::Prop { prop: "rate" },
                }],
                Err((0, "a prop, fixed or takes rule reads no sink pad")),
            ),
            (&[Coupling::same(0, 0, "")], Err((0, "an empty field name"))),
            (&[Coupling::copy(0, 0, "w", "")], Err((0, "an empty field name"))),
            (
                &[Coupling::same(0, 0, "rate"), Coupling::prop(0, "rate", "rate")],
                Err((1, "two rules write one field")),
            ),
            (
                &[Coupling::through(0, 0), Coupling::same(0, 0, "rate")],
                Err((1, "two rules write one field")),
            ),
            (
                &[Coupling::same(0, 0, "rate"), Coupling::through(0, 0)],
                Err((1, "another rule writes this src pad")),
            ),
            (&[Coupling::prop(0, "rate", "volume")], Err((0, "no such property"))),
            (&[Coupling::prop(0, "rate", "gain")], Err((0, "a live property"))),
        ];
        for (i, (rules, want)) in rows.iter().enumerate() {
            assert_eq!(check(&desc(rules)), *want, "row {i}");
        }
    }
}
