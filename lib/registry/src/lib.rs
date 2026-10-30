//! Opt-in element registry and the gst-launch-style parse grammar.
//! Typed construction stays the primary path, this is "element by name" for `pf-launch` and tests.

pub mod convert;

use std::collections::HashMap;

use profluens_core::element::ElementDesc;
use profluens_core::error::ParseError;
use profluens_core::format::Value;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;

/// The opt-in "element by name" table. A plugin crate fills it via `register(&mut Registry)`.
#[derive(Default)]
pub struct Registry {
    by_name: HashMap<&'static str, &'static ElementDesc>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one descriptor under `desc.name`. A later registration with the same name replaces
    /// it.
    pub fn register(&mut self, desc: &'static ElementDesc) {
        self.by_name.insert(desc.name, desc);
    }

    /// The descriptor registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&'static ElementDesc> {
        self.by_name.get(name).copied()
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self.by_name.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Parse `elem prop=val ! elem` into elements and links on `into`, returning the added ids in
    /// chain order. Elements added before an error stay on the pipeline.
    // Cold path, once per launch string.
    #[allow(clippy::disallowed_methods)]
    pub fn parse(&self, into: &mut Pipeline, launch: &str) -> Result<Vec<ElementId>, ParseError> {
        let stages = lex_chain(launch)?;
        if stages.is_empty() {
            return Err(ParseError {
                message: "empty launch string, expected at least one element".into(),
            });
        }

        let mut ids: Vec<ElementId> = Vec::with_capacity(stages.len());
        let mut descs: Vec<&'static ElementDesc> = Vec::with_capacity(stages.len());
        for stage in &stages {
            let desc = self.get(stage.name).ok_or_else(|| ParseError {
                message: format!("unknown element '{}'", stage.name),
            })?;
            let make = desc.make_default.ok_or_else(|| ParseError {
                message: format!(
                    "element '{}' has no make_default factory, it cannot be built by name \
                     (construct it with its typed constructor instead)",
                    stage.name
                ),
            })?;
            let id = into.add_boxed(make());
            for prop in &stage.props {
                apply_prop(into, id, desc, prop)?;
            }
            ids.push(id);
            descs.push(desc);
        }

        for i in 0..ids.len() - 1 {
            let src_pad = descs[i].srcs.first().ok_or_else(|| ParseError {
                message: format!(
                    "element '{}' has no src pad to link to '{}'",
                    descs[i].name,
                    descs[i + 1].name
                ),
            })?;
            let sink_pad = descs[i + 1].sinks.first().ok_or_else(|| ParseError {
                message: format!(
                    "element '{}' has no sink pad to link from '{}'",
                    descs[i + 1].name,
                    descs[i].name
                ),
            })?;
            into.link(ids[i].src(SrcPad(0)), ids[i + 1].sink(SinkPad(0))).map_err(|e| {
                ParseError {
                    message: format!(
                        "cannot link '{}'.{} ! '{}'.{}: {e}",
                        descs[i].name,
                        src_pad.name,
                        descs[i + 1].name,
                        sink_pad.name
                    ),
                }
            })?;
        }

        Ok(ids)
    }
}

fn apply_prop(
    into: &mut Pipeline,
    id: ElementId,
    desc: &'static ElementDesc,
    prop: &ParsedProp<'_>,
) -> Result<(), ParseError> {
    match &prop.value {
        ParsedValue::Str(s) => into.set_str(id, prop.key, s).map_err(|e| ParseError {
            message: format!("set '{}' {}={s:?}: {e}", desc.name, prop.key),
        }),
        ParsedValue::Value(v) => into.set(id, prop.key, *v).map_err(|e| ParseError {
            message: format!("set '{}' {}={:?}: {e}", desc.name, prop.key, v),
        }),
    }
}

struct ParsedStage<'a> {
    name: &'a str,
    props: Vec<ParsedProp<'a>>,
}

struct ParsedProp<'a> {
    key: &'a str,
    value: ParsedValue<'a>,
}

enum ParsedValue<'a> {
    Value(Value),
    Str(&'a str),
}

// Cold path, once per launch string.
#[allow(clippy::disallowed_methods)]
fn lex_chain(launch: &str) -> Result<Vec<ParsedStage<'_>>, ParseError> {
    let mut stages = Vec::new();
    // `a !! b` splits into an empty middle segment, which is the error to name.
    for segment in launch.split('!') {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            if launch.trim().is_empty() {
                return Ok(Vec::new());
            }
            return Err(ParseError {
                message: "empty element between '!' separators (a stray '!'?)".into(),
            });
        }
        stages.push(lex_stage(trimmed)?);
    }
    Ok(stages)
}

// Cold path, once per launch string.
#[allow(clippy::disallowed_methods)]
fn lex_stage(stage: &str) -> Result<ParsedStage<'_>, ParseError> {
    let mut tokens = stage.split_whitespace();
    let name =
        tokens.next().ok_or_else(|| ParseError { message: "expected an element name".into() })?;
    if !is_ident(name) {
        return Err(ParseError { message: format!("'{name}' is not a valid element name") });
    }
    let mut props = Vec::new();
    for tok in tokens {
        props.push(lex_prop(tok)?);
    }
    Ok(ParsedStage { name, props })
}

fn lex_prop(tok: &str) -> Result<ParsedProp<'_>, ParseError> {
    let eq = tok.find('=').ok_or_else(|| ParseError {
        message: format!("property '{tok}' is missing '=' (expected key=value)"),
    })?;
    let key = &tok[..eq];
    let val = &tok[eq + 1..];
    if key.is_empty() {
        return Err(ParseError { message: format!("property '{tok}' has an empty key") });
    }
    if !is_ident(key) {
        return Err(ParseError { message: format!("'{key}' is not a valid property name") });
    }
    if val.is_empty() {
        return Err(ParseError { message: format!("property '{key}' has an empty value") });
    }
    Ok(ParsedProp { key, value: parse_value(val) })
}

/// A rational only matches when both sides parse as integers, so paths with `/` stay strings.
fn parse_value(s: &str) -> ParsedValue<'_> {
    if let Ok(i) = s.parse::<i64>() {
        return ParsedValue::Value(Value::Int(i));
    }
    if let Some((n, d)) = s.split_once('/')
        && let (Ok(n), Ok(d)) = (n.parse::<i32>(), d.parse::<i32>())
    {
        return ParsedValue::Value(Value::Rat(n, d));
    }
    ParsedValue::Str(s)
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
}

impl Registry {
    /// Human-readable description of the element registered under `name`. `None` if unregistered.
    pub fn describe(&self, name: &str) -> Option<String> {
        self.get(name).map(describe_desc)
    }
}

// Cold path, diagnostic only.
#[allow(clippy::disallowed_methods)]
pub fn describe_desc(desc: &ElementDesc) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "{}", desc.name);
    let _ = writeln!(
        s,
        "  scheduling: {:?}   inputs: {:?}   construct: {}",
        desc.sched,
        desc.inputs,
        if desc.make_default.is_some() {
            "typed or parse (make_default)"
        } else {
            "typed only (no make_default)"
        }
    );
    let _ = writeln!(
        s,
        "  latency: min {}  max {}  jitter {}  live: {}",
        fmt_ts(desc.latency.min),
        fmt_ts(desc.latency.max),
        fmt_ts(desc.latency.jitter),
        if desc.latency.is_live { "yes" } else { "no" },
    );

    let _ = writeln!(s, "  pads:");
    let sinks = desc.sinks.iter().map(|p| (p, "sink"));
    for (pad, side) in sinks.chain(desc.srcs.iter().map(|p| (p, "src"))) {
        let _ = writeln!(s, "    {} ({side})", pad.name);
        if profluens_core::format::OfferDesc::is_probed(pad.offers) {
            let _ = writeln!(s, "      pending (the element supplies its offers at preroll)");
            continue;
        }
        for offer in pad.offers {
            if offer.is_wildcard() {
                let _ = writeln!(s, "      * (any family)");
                continue;
            }
            if offer.fields.is_empty() {
                let _ = writeln!(s, "      {} (any)", offer.family);
                continue;
            }
            let _ = writeln!(s, "      {}", offer.family);
            for f in offer.fields {
                let pref = match &f.preferred {
                    Some(v) => format!("  (preferred: {})", profluens_core::format::value_line(v)),
                    None => String::new(),
                };
                let _ = writeln!(
                    s,
                    "        {}: {}{pref}",
                    f.field,
                    profluens_core::format::constraint_line(&f.allowed)
                );
            }
        }
    }

    if desc.props.is_empty() {
        let _ = writeln!(s, "  properties: (none)");
    } else {
        let _ = writeln!(s, "  properties:");
        let width = desc.props.iter().map(|p| p.name.len()).max().unwrap_or(0);
        for p in desc.props {
            let _ = writeln!(
                s,
                "    {:width$}  {}  ({})",
                p.name,
                fmt_constraint(&p.allowed),
                if p.live { "live" } else { "structural" },
            );
        }
    }
    describe_couplings(desc, &mut s);
    s
}

// Cold path, diagnostic only.
#[allow(clippy::disallowed_methods)]
fn describe_couplings(desc: &ElementDesc, s: &mut String) {
    use profluens_core::couple::{Coupling, Rule};
    use std::fmt::Write as _;
    if desc.couplings.is_empty() {
        let _ = writeln!(s, "  couplings: (none)");
        return;
    }
    let _ = writeln!(s, "  couplings:");
    let pad = |pads: &[profluens_core::element::PadDesc], i: u32| {
        pads.get(i as usize).map_or("?", |p| p.name)
    };
    for c in desc.couplings {
        let line = match *c {
            Coupling::Through { sink, src } => {
                format!("{} through from {}", pad(desc.srcs, src), pad(desc.sinks, sink))
            }
            Coupling::Field { sink, src, field, rule } => {
                let to = format!("{}.{field}", pad(desc.srcs, src));
                match rule {
                    Rule::Same => format!("{to} = {}.{field}", pad(desc.sinks, sink)),
                    Rule::Copy { from } => format!("{to} = {}.{from}", pad(desc.sinks, sink)),
                    Rule::Prop { prop } => format!("{to} = property {prop}, else its offer"),
                    Rule::Fixed(v) => format!("{to} = {}", profluens_core::format::value_line(&v)),
                    Rule::Takes => format!("{to} = what downstream takes of its offer"),
                }
            }
        };
        let _ = writeln!(s, "    {line}");
    }
    if let Err((index, why)) = profluens_core::couple::check(desc) {
        let _ = writeln!(s, "    INVALID: rule {index}, {why}");
    }
}

fn fmt_ts(t: profluens_core::time::Timestamp) -> String {
    match t.nanos() {
        None => "unset".into(),
        Some(0) => "0".into(),
        Some(n) if n % 1_000_000 == 0 => format!("{}ms", n / 1_000_000),
        Some(n) => format!("{n}ns"),
    }
}

// Cold path, diagnostic only.
#[allow(clippy::disallowed_methods)]
fn fmt_value(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Rat(n, d) => format!("{n}/{d}"),
        // Interned ids are pipeline-scoped, so a static constraint can only show one opaquely.
        Value::Id(id) => format!("#{}", id.0),
    }
}

fn fmt_constraint(c: &profluens_core::format::Constraint) -> String {
    use profluens_core::format::Constraint as C;
    match c {
        C::Any => "any".into(),
        C::Eq(v) => fmt_value(v),
        C::Range { min, max, step } => {
            format!("[{}, {}] step {}", fmt_value(min), fmt_value(max), fmt_value(step))
        }
        C::Set(vs) => {
            let items: Vec<String> = vs.iter().map(fmt_value).collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use profluens_core::batch::Inputs;
    use profluens_core::ctx::Ctx;
    use profluens_core::element::PropDesc;
    use profluens_core::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use profluens_core::error::Error;
    use profluens_core::format::{Constraint, OfferDesc};

    static ANY_BYTES: [OfferDesc; 1] = [OfferDesc::any("bytes")];

    static SRC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &ANY_BYTES }];
    static SINK_PADS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &ANY_BYTES }];

    static SRC_PROPS: [PropDesc; 2] = [
        PropDesc { name: "total", allowed: Constraint::Any, live: false },
        PropDesc { name: "path", allowed: Constraint::Any, live: false },
    ];

    static SRC_DESC: ElementDesc = ElementDesc {
        name: "tsrc",
        sinks: &[],
        srcs: &SRC_PADS,
        props: &SRC_PROPS,
        sched: SchedHint::Active,
        inputs: InputPolicy::None,
        latency: LatencyDesc::NONE,
        make_default: Some(|| Box::new(TSrc)),
        io: false,
        couplings: &[],
    };
    static SINK_DESC: ElementDesc = ElementDesc {
        name: "tsink",
        sinks: &SINK_PADS,
        srcs: &[],
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::Single,
        latency: LatencyDesc::NONE,
        make_default: Some(|| Box::new(TSink)),
        io: false,
        couplings: &[],
    };
    static NOFACTORY_DESC: ElementDesc = ElementDesc {
        name: "nofactory",
        sinks: &[],
        srcs: &SRC_PADS,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::None,
        latency: LatencyDesc::NONE,
        make_default: None,
        io: false,
        couplings: &[],
    };

    struct TSrc;
    impl Element for TSrc {
        fn desc(&self) -> &'static ElementDesc {
            &SRC_DESC
        }
        fn process(&mut self, _c: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
            Ok(Flow::Eos)
        }
    }
    struct TSink;
    impl Element for TSink {
        fn desc(&self) -> &'static ElementDesc {
            &SINK_DESC
        }
        fn process(&mut self, _c: &mut Ctx, _i: Inputs<'_>) -> Result<Flow, Error> {
            Ok(Flow::Ok)
        }
    }

    fn registry() -> Registry {
        let mut r = Registry::new();
        r.register(&SRC_DESC);
        r.register(&SINK_DESC);
        r.register(&NOFACTORY_DESC);
        r
    }

    #[test]
    fn register_get_and_names() {
        let r = registry();
        assert_eq!(r.get("tsrc").map(|d| d.name), Some("tsrc"));
        assert!(r.get("missing").is_none());
        assert_eq!(r.names(), vec!["nofactory", "tsink", "tsrc"]);
    }

    #[test]
    fn last_registration_wins() {
        let mut r = Registry::new();
        r.register(&SRC_DESC);
        r.register(&SRC_DESC);
        assert_eq!(r.get("tsrc").map(|d| d.name), Some("tsrc"));
    }

    #[test]
    fn parse_builds_linear_chain() {
        let r = registry();
        let mut p = Pipeline::new();
        let ids = r.parse(&mut p, "tsrc total=65536 ! tsink").expect("parses");
        assert_eq!(ids.len(), 2);
        assert_eq!(p.prop_value(ids[0], "total"), Some(Value::Int(65536)));
    }

    #[test]
    fn parse_whitespace_tolerant() {
        let r = registry();
        for s in ["tsrc!tsink", "  tsrc ! tsink  ", "tsrc total=1!tsink"] {
            let mut p = Pipeline::new();
            assert!(r.parse(&mut p, s).is_ok(), "{s:?} should parse");
        }
    }

    #[test]
    fn parse_string_prop_interns_via_value_vocab() {
        let r = registry();
        let mut p = Pipeline::new();
        let ids = r.parse(&mut p, "tsrc path=/tmp/in.wav ! tsink").expect("parses");
        let v = p.prop_value(ids[0], "path").expect("path set");
        match v {
            Value::Id(vid) => assert_eq!(p.value_name(vid), Some("/tmp/in.wav")),
            other => panic!("expected an interned id, got {other:?}"),
        }
    }

    #[test]
    fn parse_rational_prop() {
        let r = registry();
        let mut p = Pipeline::new();
        let ids = r.parse(&mut p, "tsrc total=30000/1001 ! tsink").expect("parses");
        assert_eq!(p.prop_value(ids[0], "total"), Some(Value::Rat(30000, 1001)));
    }

    #[test]
    fn parse_errors_name_the_offending_token() {
        let r = registry();
        let cases: &[(&str, &str)] = &[
            ("", "empty"),
            ("   ", "empty"),
            ("!", "empty element"),
            ("tsrc ! ! tsink", "empty element"),
            ("tsrc !", "empty element"),
            ("! tsink", "empty element"),
            ("bogus ! tsink", "bogus"),
            ("tsrc ! bogus", "bogus"),
            ("nofactory ! tsink", "make_default"),
            ("tsrc total ! tsink", "missing '='"),
            ("tsrc =5 ! tsink", "empty key"),
            ("tsrc total= ! tsink", "empty value"),
            ("tsrc nosuch=1 ! tsink", "nosuch"),
        ];
        for (input, needle) in cases {
            let mut p = Pipeline::new();
            match r.parse(&mut p, input) {
                Ok(_) => panic!("{input:?} should not parse"),
                Err(e) => assert!(
                    e.message.contains(needle),
                    "{input:?}: error {:?} should mention {needle:?}",
                    e.message
                ),
            }
        }
    }

    #[test]
    fn parse_never_panics_on_junk() {
        let r = registry();
        let junk = [
            "=",
            "==",
            "!=!",
            "a=b=c",
            "tsrc total=/",
            "/",
            "tsrc total=1/",
            "tsrc total=/1",
            "\t\n",
            "tsrc\0",
            "tsrc total=999999999999999999999999",
            "tsrc ! tsrc ! tsrc",
            "tsink ! tsrc",
        ];
        for s in junk {
            let mut p = Pipeline::new();
            let _ = r.parse(&mut p, s); // must return, never unwind
        }
    }

    #[test]
    fn value_parsing_int_rat_string() {
        assert!(matches!(parse_value("65536"), ParsedValue::Value(Value::Int(65536))));
        assert!(matches!(parse_value("-3"), ParsedValue::Value(Value::Int(-3))));
        assert!(matches!(parse_value("30000/1001"), ParsedValue::Value(Value::Rat(30000, 1001))));
        assert!(matches!(parse_value("/tmp/x.wav"), ParsedValue::Str("/tmp/x.wav")));
        assert!(matches!(parse_value("s16"), ParsedValue::Str("s16")));
        assert!(matches!(parse_value("999999999999999999999999"), ParsedValue::Str(_)));
    }

    static RICH_VALUES: [profluens_core::format::ValueDesc; 2] = [
        profluens_core::format::ValueDesc::Id("s16"),
        profluens_core::format::ValueDesc::Id("s32"),
    ];
    static RICH_FIELDS: [profluens_core::format::FieldDesc; 3] = [
        profluens_core::format::FieldDesc {
            field: "rate",
            allowed: profluens_core::format::ConstraintDesc::Range {
                min: profluens_core::format::ValueDesc::Int(8_000),
                max: profluens_core::format::ValueDesc::Int(192_000),
                step: profluens_core::format::ValueDesc::Int(1),
            },
            preferred: Some(profluens_core::format::ValueDesc::Int(48_000)),
        },
        profluens_core::format::FieldDesc {
            field: "sample",
            allowed: profluens_core::format::ConstraintDesc::Set(&RICH_VALUES),
            preferred: None,
        },
        profluens_core::format::FieldDesc {
            field: "fps",
            allowed: profluens_core::format::ConstraintDesc::Eq(
                profluens_core::format::ValueDesc::Rat(30, 1),
            ),
            preferred: None,
        },
    ];
    static RICH_OFFERS: [OfferDesc; 2] =
        [OfferDesc { family: "audio/raw", fields: &RICH_FIELDS }, OfferDesc::any("bytes")];
    static RICH_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &RICH_OFFERS }];
    static RICH_PROPS: [PropDesc; 2] = [
        PropDesc {
            name: "volume",
            allowed: Constraint::Range {
                min: Value::Int(0),
                max: Value::Int(10),
                step: Value::Int(1),
            },
            live: true,
        },
        PropDesc { name: "path", allowed: Constraint::Any, live: false },
    ];
    static RICH_DESC: ElementDesc = ElementDesc {
        name: "rich",
        sinks: &SINK_PADS,
        srcs: &RICH_PADS,
        props: &RICH_PROPS,
        sched: SchedHint::Passive,
        inputs: InputPolicy::Single,
        latency: LatencyDesc {
            min: profluens_core::time::Timestamp::ZERO,
            max: profluens_core::time::Timestamp::from_millis(5),
            is_live: true,
            jitter: profluens_core::time::Timestamp::ZERO,
        },
        make_default: None,
        io: false,
        couplings: &[],
    };

    #[test]
    fn describe_renders_pads_offers_and_props() {
        let mut r = Registry::new();
        r.register(&RICH_DESC);
        let s = r.describe("rich").expect("registered");
        for needle in [
            "rich",
            "scheduling: Passive",
            "inputs: Single",
            "typed only (no make_default)",
            "max 5ms",
            "live: yes",
            "sink (sink)",
            "src (src)",
            "audio/raw",
            "rate: [8000, 192000] step 1  (preferred: 48000)",
            "sample: {s16, s32}",
            "fps: 30/1",
            "bytes (any)",
            "volume  [0, 10] step 1  (live)",
            "path    any  (structural)",
        ] {
            assert!(s.contains(needle), "describe output missing {needle:?}:\n{s}");
        }
        assert!(r.describe("nope").is_none(), "unregistered name");
    }

    #[test]
    fn describe_lists_couplings_and_names_a_bad_one() {
        use profluens_core::couple::Coupling;
        use profluens_core::format::ValueDesc;
        static GOOD: [Coupling; 3] = [
            Coupling::same(0, 0, "rate"),
            Coupling::prop(0, "sample", "path"),
            Coupling::fixed(0, "channels", ValueDesc::Int(2)),
        ];
        static BAD: [Coupling; 2] =
            [Coupling::same(0, 0, "rate"), Coupling::prop(0, "gain", "volume")];
        let good = ElementDesc { name: "good", couplings: &GOOD, ..RICH_DESC };
        let s = describe_desc(&good);
        for needle in [
            "couplings:",
            "src.rate = sink.rate",
            "src.sample = property path, else its offer",
            "src.channels = 2",
        ] {
            assert!(s.contains(needle), "missing {needle:?}:\n{s}");
        }
        assert!(!s.contains("INVALID"), "{s}");
        let s = describe_desc(&ElementDesc { name: "bad", couplings: &BAD, ..RICH_DESC });
        assert!(s.contains("INVALID: rule 1, a live property"), "{s}");
        assert!(describe_desc(&RICH_DESC).contains("couplings: (none)"));
    }

    #[test]
    fn a_probed_pad_is_described_as_pending() {
        static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: OfferDesc::PROBED }];
        static CAMERA: ElementDesc = ElementDesc {
            name: "camera",
            sinks: &[],
            srcs: &SRCS,
            props: &[],
            sched: SchedHint::Active,
            inputs: InputPolicy::None,
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: &[],
        };
        let text = describe_desc(&CAMERA);
        assert!(text.contains("src (src)\n      pending"), "{text}");
        assert!(!text.contains(profluens_core::format::PROBED), "{text}");
    }
}
