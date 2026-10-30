use std::fmt;
use std::sync::Arc;

use crate::format::{Constraint, Value};
use crate::id::{ElementId, FormatId, LinkId, SinkPad, SrcPad};

/// Every error core raises, plus plugin failures.
#[derive(Clone, Debug)]
pub enum Error {
    /// Building or scheduling the graph.
    Graph(GraphError),
    /// Linking two pads.
    Link(LinkError),
    /// The whole-graph format solve, at run start or for a filtered link.
    Solve(SolveError),
    /// Changing a running pipeline.
    Topology(TopologyError),
    /// Setting a property.
    Prop(PropError),
    /// A runtime format announcement or change.
    Format(FormatError),
    /// A probed pad's offers were refused. `src` says which side `pad` indexes.
    Offer { element: ElementId, src: bool, pad: u32, fault: OfferFault },
    /// An OS resource could not be opened or used.
    Io { what: &'static str, kind: crate::io::IoError },
    /// A group thread panicked. `element` heads the group, `message` is the panic payload.
    GroupPanicked {
        element: ElementId,
        message: String,
    },
    /// The element does IO and the pipeline has no reactor factory.
    NoReactor { element: ElementId, element_name: &'static str },
    /// An element failed. `source` is the plugin crate's own error type.
    ElementFailed {
        element: ElementId,
        source: Arc<dyn std::error::Error + Send + Sync>,
        scope: Scope,
    },
}

/// How far a failure reaches, narrowest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Scope {
    /// What failed is dropped with a warning and the element goes on.
    Buffer,
    /// The element's group is sealed and the rest of the run goes on.
    Branch,
    /// The run fails.
    Pipeline,
}

pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    /// Wrap a plugin crate's own error as the failure of `element`.
    pub fn element_failed(
        element: ElementId,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Error {
        Error::ElementFailed { element, source: Arc::new(source), scope: Scope::Pipeline }
    }

    /// The same failure reaching as far as `to`. Only an element's own failure has a scope.
    pub fn scoped(mut self, to: Scope) -> Error {
        if let Error::ElementFailed { scope, .. } = &mut self {
            *scope = to;
        }
        self
    }

    /// How far this failure reaches, `Pipeline` unless the element said otherwise.
    pub fn scope(&self) -> Scope {
        match self {
            Error::ElementFailed { scope, .. } => *scope,
            _ => Scope::Pipeline,
        }
    }
}

/// The graph cannot be ordered or split into thread groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// Nothing to run.
    Empty,
    /// A warning that the element has no linked pad and takes no part in the run.
    Unlinked { element: ElementId, element_name: &'static str },
    /// A warning that the element takes input and nothing that runs feeds it.
    Starved { element: ElementId, element_name: &'static str },
    /// Every element has an input, so there is no entry point.
    NoSource,
    /// These elements sit on or behind a cycle and can never be ordered.
    Cycle { elements: Box<[ElementId]> },
    /// The element was moved into a previous run.
    ElementConsumed { element: ElementId },
    /// A passive element has more than one input. An aggregator must be active.
    PassiveMultipleInputs { element: ElementId, inputs: usize },
    /// A passive element cannot inline because `upstream` is no longer its group's tail.
    PassiveFanOut { element: ElementId, upstream: ElementId },
    /// A passive element was ordered before its upstream. Internal invariant.
    PassiveBeforeUpstream { element: ElementId, upstream: ElementId },
    /// An edge to another group leaves `src`, which is not its group's tail.
    NonTailBranch { src: ElementId, sink: ElementId },
    /// Two elements retime. A pipeline has one timeline and `first` writes it.
    TimelineClaimed { element: ElementId, first: ElementId },
    /// The element has a probed pad and was never prerolled, so its offers are unknown.
    NotPrerolled { element: ElementId, element_name: &'static str },
    /// A warning that the element's tracks lie `interleave` apart, so it rests on `pad` until that
    /// link drains.
    Interleaved { element: ElementId, pad: SrcPad, interleave: crate::time::Timestamp },
}

/// A change to a running pipeline was refused and the books are unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopologyError {
    /// No run to change.
    NotRunning,
    /// The change before this one has not spliced yet.
    Busy,
    /// Change `change` was taken back before its splice.
    Cancelled {
        change: u32,
    },
    /// Only whole thread groups leave. `element` stays, in the group `removed` is in.
    PartialGroup {
        element: ElementId,
        removed: ElementId,
    },
    /// The link ends on `sink`, which runs inline and so takes no new link.
    NotAHead {
        sink: ElementId,
    },
    /// The link is inside a running group, where there is no ring to cut.
    Inline {
        src: ElementId,
        sink: ElementId,
    },
    /// The pad has a link already.
    PadLinked {
        element: ElementId,
        src: bool,
        pad: u32,
    },
    /// The element was removed, or is of a run that is over.
    Gone {
        element: ElementId,
    },
    /// A restart for an element whose output crosses a ring.
    RestartAtRing {
        element: ElementId,
    },
    NoSuchLink {
        link: crate::id::LinkId,
    },
    NoSuchStream {
        element: ElementId,
        stream: u32,
    },
    /// A warning that `count` elements of one change log nowhere.
    LogsLost {
        count: u32,
    },
}

impl fmt::Display for TopologyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TopologyError::NotRunning => f.write_str("no run to change"),
            TopologyError::Busy => f.write_str("the change before this one has not spliced yet"),
            TopologyError::Cancelled { change } => {
                write!(f, "change {change} was cancelled before its splice")
            }
            TopologyError::PartialGroup { element, removed } => write!(
                f,
                "element {} leaves and element {} of its thread group stays",
                removed.0, element.0
            ),
            TopologyError::NotAHead { sink } => {
                write!(f, "element {} runs inline and takes no new link", sink.0)
            }
            TopologyError::Inline { src, sink } => {
                write!(f, "the link from element {} to element {} is inside a group", src.0, sink.0)
            }
            TopologyError::PadLinked { element, src, pad } => {
                let side = if *src { "src" } else { "sink" };
                write!(f, "{side} pad {pad} of element {} has a link already", element.0)
            }
            TopologyError::Gone { element } => write!(f, "element {} is not in the run", element.0),
            TopologyError::RestartAtRing { element } => {
                write!(f, "a restart of element {}, whose output crosses a ring", element.0)
            }
            TopologyError::NoSuchLink { link } => write!(f, "no link {}", link.0),
            TopologyError::NoSuchStream { element, stream } => {
                write!(f, "element {} describes no stream {stream}", element.0)
            }
            TopologyError::LogsLost { count } => {
                write!(f, "{count} elements that joined log nowhere")
            }
        }
    }
}

impl std::error::Error for TopologyError {}

/// `Pipeline::link` failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// No such element in this pipeline, or it was moved into a previous run.
    UnknownElement { element: ElementId },
    /// The element has no src pad of that name.
    UnknownSrcPad { element: ElementId, element_name: &'static str, pad: Box<str> },
    /// The element has no sink pad of that name.
    UnknownSinkPad { element: ElementId, element_name: &'static str, pad: Box<str> },
    /// The index is past the element's src pads, static and dynamic.
    NoSrcPad { element: ElementId, element_name: &'static str, pad: SrcPad },
    /// The index is past the element's sink pads, static and dynamic.
    NoSinkPad { element: ElementId, element_name: &'static str, pad: SinkPad },
    /// The two pads share no format.
    NoCommonFormat(Box<NoCommonFormat>),
    /// The pad's offers come from a probe and none were supplied. Preroll, then link.
    OffersPending { element: ElementId, element_name: &'static str, pad: Box<str> },
}

/// Why a probed pad's offers were refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfferFault {
    /// The pad does not list `OfferDesc::PROBED`, or the element has no such pad.
    NotProbed,
    /// The pad's offers were supplied already.
    Twice,
    /// Offers may only be supplied inside `preroll`.
    Frozen,
    /// An offer names the wildcard or the probe marker as its family.
    ReservedFamily,
    /// The probe found more than a limit in `format` allows.
    OverLimit { what: &'static str, found: u32, limit: u32 },
}

/// Both ends of a link whose offers do not intersect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoCommonFormat {
    pub src: LinkEnd,
    pub sink: LinkEnd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkEnd {
    pub element: ElementId,
    pub element_name: &'static str,
    pub pad: Box<str>,
    /// The pad's own offers in preference order, a static menu's by family alone.
    pub offers: Box<[Box<str>]>,
    /// The format a wildcard transport's other pad or a src pad's other links already fixed.
    pub bound: Option<Box<str>>,
}

/// `Pipeline::set` or `PropHandle::set` failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropError {
    /// No such element in this pipeline.
    UnknownElement { element: ElementId },
    /// The element declares no property of that name.
    UnknownProperty { element: ElementId, element_name: &'static str, prop: Box<str> },
    /// A structural (`live: false`) property was set while playing.
    Structural { element: ElementId, element_name: &'static str, prop: &'static str, index: usize },
    /// The value is outside the property's declared constraint.
    Rejected(Box<RejectedValue>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedValue {
    pub element: ElementId,
    pub element_name: &'static str,
    pub prop: &'static str,
    /// Position in the element's `desc().props`.
    pub index: usize,
    pub value: Value,
    pub allowed: Constraint<'static>,
}

/// The whole-graph solve found no format, or was given rules it cannot read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SolveError {
    /// Edge `link` has no format both pads admit once the graph's constraints apply.
    Empty(Box<EmptyIntersection>),
    /// A fixed format would need more than 16 fields.
    TooManyFields { link: LinkId },
    /// A `link_filtered` filter is what refused the link.
    FilterRejected(Box<FilterRejected>),
    /// A filter past the limits of an offer.
    BadFilter(OfferFault),
    /// Rule `index` of the element's couplings names a pad, field or property it has not.
    BadCoupling { element: ElementId, element_name: &'static str, index: u32, why: &'static str },
    /// The passes disagree, a solver bug named rather than looped on.
    Inconsistent { link: LinkId },
}

/// An edge whose domains met empty, and what narrowed them on the way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyIntersection {
    pub link: LinkId,
    /// `bound` is what the graph left the pad, where another pad of its element narrowed it.
    pub src: LinkEnd,
    pub sink: LinkEnd,
    /// The field whose domains met empty, `None` when no family was common.
    pub field: Option<Box<str>>,
    /// Every narrowing of that field's two domains, upstream first.
    pub path: Box<[Narrowing]>,
}

/// One rule or peer that narrowed a field on the way to a failed edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Narrowing {
    /// The edge it came over, `None` for a rule inside the element.
    pub link: Option<LinkId>,
    pub element: ElementId,
    pub element_name: &'static str,
    pub pad: Box<str>,
    /// "peer", "filter", "running format", "through", "same", "copy", "prop" or "fixed".
    pub rule: &'static str,
    pub constraint: Box<str>,
}

/// Both ends of a link its filter refused, which links without it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterRejected {
    pub src: LinkEnd,
    pub sink: LinkEnd,
    pub filter: Box<str>,
}

/// A runtime format announcement could not be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// The announced format failed re-validation against the receiving pad's offers.
    AnnouncementRejected(Box<RejectedFormat>),
    /// The receiving pad offers no format of the announced family. Not fatal, dropped up to a
    /// format it takes.
    FamilyRefused(Box<RejectedFormat>),
    /// A warning that the announcement names something no pad interned, so it was dropped.
    AnnouncementUnresolved { element: ElementId, family: &'static str, fields: Box<[&'static str]> },
}

/// A format announced while playing and the pad that does not admit it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedFormat {
    pub element: ElementId,
    pub element_name: &'static str,
    pub pad: SinkPad,
    pub pad_name: Box<str>,
    pub family: FormatId,
    pub announced: Box<str>,
    pub offers: Box<[Box<str>]>,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Graph(e) => e.fmt(f),
            Error::Link(e) => e.fmt(f),
            Error::Solve(e) => e.fmt(f),
            Error::Topology(e) => e.fmt(f),
            Error::Prop(e) => e.fmt(f),
            Error::Format(e) => e.fmt(f),
            Error::Offer { element, src, pad, fault } => {
                let side = if *src { "src" } else { "sink" };
                write!(f, "offers for {side} pad {pad} of element {} refused, {fault}", element.0)
            }
            Error::Io { what, kind } => write!(f, "{what}: {kind}"),
            Error::GroupPanicked { element, message } => {
                write!(f, "group thread panicked at element {}: {message}", element.0)
            }
            Error::NoReactor { element, element_name } => write!(
                f,
                "element '{element_name}' ({}) does IO and the pipeline has no reactor factory",
                element.0
            ),
            Error::ElementFailed { element, source, .. } => {
                write!(f, "element {} failed: {source}", element.0)
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::ElementFailed { source, .. } => Some(&**source),
            _ => None,
        }
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Empty => f.write_str("empty pipeline, no element takes part in a run"),
            GraphError::Unlinked { element_name, element } => write!(
                f,
                "element '{element_name}' ({}) has no linked pad and takes no part in the run",
                element.0
            ),
            GraphError::Starved { element_name, element } => write!(
                f,
                "element '{element_name}' ({}) is fed by no source and takes no part in the run",
                element.0
            ),
            GraphError::Interleaved { element, pad, interleave } => write!(
                f,
                "element {} interleaves its tracks {} ms apart, past what its links hold, and rests on pad {}",
                element.0,
                interleave.0 / 1_000_000,
                pad.0
            ),
            GraphError::NoSource => {
                f.write_str("no source, every element has an input (a cycle with no entry)")
            }
            GraphError::Cycle { elements } => {
                f.write_str("cycle in the graph, cannot order elements")?;
                for e in elements.iter() {
                    write!(f, " {}", e.0)?;
                }
                Ok(())
            }
            GraphError::ElementConsumed { element } => {
                write!(f, "element {} already consumed by a previous run", element.0)
            }
            GraphError::PassiveMultipleInputs { element, inputs } => write!(
                f,
                "passive element {} has {inputs} inputs (an aggregator must be active)",
                element.0
            ),
            GraphError::PassiveFanOut { element, upstream } => write!(
                f,
                "passive fan-out not supported, element {} cannot inline after {} \
                 (make the branch point active)",
                element.0, upstream.0
            ),
            GraphError::PassiveBeforeUpstream { element, upstream } => write!(
                f,
                "passive element {} scheduled before its upstream {} (internal)",
                element.0, upstream.0
            ),
            GraphError::NonTailBranch { src, sink } => write!(
                f,
                "inter-group branch from non-tail element {} to {} is unsupported \
                 (make the branch point active so it forms its own group)",
                src.0, sink.0
            ),
            GraphError::TimelineClaimed { element, first } => {
                write!(f, "element {} retimes and element {} already does", element.0, first.0)
            }
            GraphError::NotPrerolled { element, element_name } => write!(
                f,
                "element '{element_name}' ({}) has a probed pad and was never prerolled, preroll before the run",
                element.0
            ),
        }
    }
}

impl std::error::Error for GraphError {}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::UnknownElement { element } => {
                write!(f, "link: unknown element {}", element.0)
            }
            LinkError::UnknownSrcPad { element_name, pad, .. } => {
                write!(f, "link: element '{element_name}' has no src pad '{pad}'")
            }
            LinkError::UnknownSinkPad { element_name, pad, .. } => {
                write!(f, "link: element '{element_name}' has no sink pad '{pad}'")
            }
            LinkError::NoSrcPad { element_name, pad, .. } => {
                write!(f, "link: element '{element_name}' has no src pad {}", pad.0)
            }
            LinkError::NoSinkPad { element_name, pad, .. } => {
                write!(f, "link: element '{element_name}' has no sink pad {}", pad.0)
            }
            LinkError::NoCommonFormat(d) => d.fmt(f),
            LinkError::OffersPending { element_name, pad, .. } => {
                write!(f, "link: '{element_name}' pad '{pad}' has no offers yet, preroll first")
            }
        }
    }
}

impl std::error::Error for LinkError {}

impl fmt::Display for OfferFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OfferFault::NotProbed => f.write_str("the pad is not marked as probed"),
            OfferFault::Twice => f.write_str("they were supplied already"),
            OfferFault::Frozen => f.write_str("offers are supplied in preroll and frozen after"),
            OfferFault::ReservedFamily => f.write_str("an offer names a reserved family"),
            OfferFault::OverLimit { what, found, limit } => {
                write!(f, "{found} {what}, the limit is {limit}")
            }
        }
    }
}

impl fmt::Display for NoCommonFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (s, d) = (&self.src, &self.sink);
        write!(
            f,
            "link: no common format between {}.{} and {}.{} (offers {:?} vs {:?}), \
             negotiation failed",
            s.element_name, s.pad, d.element_name, d.pad, s.offers, d.offers
        )?;
        for (end, peer) in [(s, d), (d, s)] {
            if let Some(bound) = &end.bound {
                write!(
                    f,
                    ". {} already carries {bound} on its other wildcard pad (a pure \
                     transport hands buffers on unchanged, so it cannot also offer {}'s \
                     format)",
                    end.element_name, peer.pad
                )?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for SolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SolveError::Empty(e) => {
                let (s, d) = (&e.src, &e.sink);
                let what =
                    e.field.as_deref().map_or("format".into(), |field| format!("value of {field}"));
                write!(
                    f,
                    "solve: link {} {}.{} -> {}.{} has no common {what} (offers {:?} vs {:?})",
                    e.link.0, s.element_name, s.pad, d.element_name, d.pad, s.offers, d.offers
                )?;
                for end in [s, d] {
                    if let Some(bound) = &end.bound {
                        write!(f, ", {}.{} can carry {bound}", end.element_name, end.pad)?;
                    }
                }
                for (i, n) in e.path.iter().enumerate() {
                    f.write_str(if i == 0 { ", narrowed by " } else { ", " })?;
                    if let Some(link) = n.link {
                        write!(f, "link {} ", link.0)?;
                    }
                    write!(f, "{}.{} ({} = {})", n.element_name, n.pad, n.rule, n.constraint)?;
                }
                Ok(())
            }
            SolveError::TooManyFields { link } => {
                write!(f, "solve: link {} would fix more than 16 fields", link.0)
            }
            SolveError::FilterRejected(r) => write!(
                f,
                "link: the filter [{}] admits nothing {}.{} and {}.{} share (offers {:?} vs {:?})",
                r.filter,
                r.src.element_name,
                r.src.pad,
                r.sink.element_name,
                r.sink.pad,
                r.src.offers,
                r.sink.offers
            ),
            SolveError::BadFilter(fault) => write!(f, "link: filter refused, {fault}"),
            SolveError::BadCoupling { element, element_name, index, why } => {
                write!(
                    f,
                    "element '{element_name}' ({}) coupling {index} is refused, {why}",
                    element.0
                )
            }
            SolveError::Inconsistent { link } => {
                write!(f, "solve: link {} was fixed outside its pads' offers, a solver bug", link.0)
            }
        }
    }
}

impl std::error::Error for SolveError {}

impl fmt::Display for PropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropError::UnknownElement { element } => {
                write!(f, "set: unknown element {}", element.0)
            }
            PropError::UnknownProperty { element_name, prop, .. } => {
                write!(f, "set: element '{element_name}' has no property '{prop}'")
            }
            PropError::Structural { element_name, prop, .. } => write!(
                f,
                "set: property '{prop}' on '{element_name}' is structural (live: false), \
                 setting it while playing needs the element-restart machinery (not yet \
                 built), set it between runs"
            ),
            PropError::Rejected(r) => write!(
                f,
                "set: value {:?} rejected by '{}' property '{}' constraint {:?}",
                r.value, r.element_name, r.prop, r.allowed
            ),
        }
    }
}

impl std::error::Error for PropError {}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::AnnouncementRejected(r) => write!(
                f,
                "runtime format announcement {} not accepted by '{}' pad '{}' (offers {:?}), \
                 dynamic-caps re-validation failed",
                r.announced, r.element_name, r.pad_name, r.offers
            ),
            FormatError::FamilyRefused(r) => write!(
                f,
                "'{}' pad '{}' takes none of {}, it offers {:?}",
                r.element_name, r.pad_name, r.announced, r.offers
            ),
            FormatError::AnnouncementUnresolved { family, fields, .. } => write!(
                f,
                "format announcement '{family}' (fields {fields:?}) uses a \
                 family/field/value name never interned by any pad's offers, dropped. \
                 Declare the name on an offer, `Set` interns categorical values"
            ),
        }
    }
}

impl std::error::Error for FormatError {}

#[derive(Clone, Debug)]
pub struct ParseError {
    pub message: String,
}

#[cfg(test)]
// Test fixtures, not a hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    #[derive(Debug, PartialEq, Eq)]
    enum DemoError {
        BadHeader { offset: u64 },
    }

    impl fmt::Display for DemoError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                DemoError::BadHeader { offset } => write!(f, "bad header at {offset}"),
            }
        }
    }

    impl StdError for DemoError {}

    fn end(name: &'static str, pad: &str, offers: &[&'static str], bound: Option<&str>) -> LinkEnd {
        LinkEnd {
            element: ElementId(0),
            element_name: name,
            pad: pad.into(),
            offers: offers.iter().map(|o| (*o).into()).collect(),
            bound: bound.map(Into::into),
        }
    }

    fn offer(src: bool, fault: OfferFault) -> Error {
        Error::Offer { element: ElementId(3), src, pad: 1, fault }
    }

    fn samples() -> Vec<Error> {
        let (a, b) = (ElementId(3), ElementId(7));
        vec![
            Error::Graph(GraphError::Empty),
            Error::Graph(GraphError::NoSource),
            Error::Graph(GraphError::Cycle { elements: [a, b].into() }),
            Error::Graph(GraphError::ElementConsumed { element: a }),
            Error::Graph(GraphError::PassiveMultipleInputs { element: a, inputs: 2 }),
            Error::Graph(GraphError::PassiveFanOut { element: a, upstream: b }),
            Error::Graph(GraphError::PassiveBeforeUpstream { element: a, upstream: b }),
            Error::Graph(GraphError::NonTailBranch { src: a, sink: b }),
            Error::Graph(GraphError::TimelineClaimed { element: a, first: b }),
            Error::Graph(GraphError::Unlinked { element: a, element_name: "vadec" }),
            Error::Graph(GraphError::Starved { element: a, element_name: "vadec" }),
            Error::Link(LinkError::UnknownElement { element: a }),
            Error::Link(LinkError::UnknownSrcPad {
                element: a,
                element_name: "demux",
                pad: "video_9".into(),
            }),
            Error::Link(LinkError::UnknownSinkPad {
                element: a,
                element_name: "mux",
                pad: "audio_9".into(),
            }),
            Error::Link(LinkError::NoSrcPad { element: a, element_name: "demux", pad: SrcPad(4) }),
            Error::Link(LinkError::NoSinkPad { element: a, element_name: "mux", pad: SinkPad(8) }),
            Error::Link(LinkError::NoCommonFormat(
                NoCommonFormat {
                    src: end("queue", "src", &["*"], Some("audio/raw, rate=44100")),
                    sink: end("sink", "sink", &["audio/raw"], None),
                }
                .into(),
            )),
            Error::Prop(PropError::UnknownElement { element: a }),
            Error::Prop(PropError::UnknownProperty {
                element: a,
                element_name: "volume",
                prop: "gain".into(),
            }),
            Error::Prop(PropError::Structural {
                element: a,
                element_name: "filesrc",
                prop: "path",
                index: 0,
            }),
            Error::Prop(PropError::Rejected(
                RejectedValue {
                    element: a,
                    element_name: "volume",
                    prop: "volume",
                    index: 1,
                    value: Value::Int(11),
                    allowed: Constraint::Range {
                        min: Value::Int(0),
                        max: Value::Int(10),
                        step: Value::Int(1),
                    },
                }
                .into(),
            )),
            Error::Format(FormatError::AnnouncementRejected(
                RejectedFormat {
                    element: a,
                    element_name: "sink",
                    pad: SinkPad(0),
                    pad_name: "sink".into(),
                    family: FormatId(2),
                    announced: "audio/raw, rate=96000".into(),
                    offers: ["audio/raw, rate={44100, 48000}".into()].into(),
                }
                .into(),
            )),
            Error::Format(FormatError::AnnouncementUnresolved {
                element: a,
                family: "audio/raw",
                fields: ["rate"].into(),
            }),
            Error::Io { what: "reactor init", kind: crate::io::IoError::Denied },
            Error::GroupPanicked { element: a, message: "index out of range".into() },
            Error::NoReactor { element: a, element_name: "filesrc" },
            Error::element_failed(a, DemoError::BadHeader { offset: 12 }),
            Error::Link(LinkError::OffersPending {
                element: a,
                element_name: "vadec",
                pad: "src".into(),
            }),
            offer(true, OfferFault::NotProbed),
            offer(false, OfferFault::Twice),
            offer(true, OfferFault::Frozen),
            offer(true, OfferFault::ReservedFamily),
            offer(false, OfferFault::OverLimit { what: "values in a set", found: 200, limit: 128 }),
        ]
    }

    #[test]
    fn every_message_is_one_plain_line() {
        for e in samples() {
            let s = e.to_string();
            assert!(!s.is_empty(), "{e:?} renders empty");
            assert!(!s.contains('\n'), "{e:?} spans lines");
            assert!(!s.contains('\u{2014}'), "{e:?} has an em dash");
            assert!(!s.ends_with(' '), "{e:?} has trailing space");
            assert_eq!(e.clone().to_string(), s);
        }
    }

    #[test]
    fn messages_carry_what_the_variant_knows() {
        let text: Vec<String> = samples().iter().map(|e| e.to_string()).collect();
        let needles: &[&[&str]] = &[
            &["empty pipeline", "no element takes part in a run"],
            &["no source", "cycle"],
            &["cycle", " 3 7"],
            &["element 3", "consumed"],
            &["element 3", "2 inputs", "aggregator must be active"],
            &["passive fan-out", "element 3", "after 7"],
            &["element 3", "upstream 7", "internal"],
            &["non-tail element 3 to 7"],
            &["element 3 retimes", "element 7 already does"],
            &["'vadec' (3)", "no linked pad", "no part in the run"],
            &["'vadec' (3)", "fed by no source", "no part in the run"],
            &["link", "unknown element 3"],
            &["'demux'", "no src pad 'video_9'"],
            &["'mux'", "no sink pad 'audio_9'"],
            &["'demux'", "no src pad 4"],
            &["'mux'", "no sink pad 8"],
            &[
                "no common format between queue.src and sink.sink",
                "[\"*\"] vs [\"audio/raw\"]",
                "negotiation failed",
                "queue already carries audio/raw, rate=44100 on its other wildcard pad",
                "sink's format",
            ],
            &["set", "unknown element 3"],
            &["'volume'", "no property 'gain'"],
            &["'path'", "'filesrc'", "structural", "between runs"],
            &["Int(11)", "'volume'", "Range"],
            &[
                "announcement audio/raw, rate=96000 not accepted by 'sink' pad 'sink'",
                "(offers [\"audio/raw, rate={44100, 48000}\"])",
                "re-validation failed",
            ],
            &["'audio/raw'", "[\"rate\"]", "never interned"],
            &["reactor init", "permission denied"],
            &["group thread panicked"],
            &["'filesrc' (3)", "does IO", "no reactor factory"],
            &["element 3 failed", "bad header at 12"],
            &["'vadec' pad 'src'", "no offers yet", "preroll first"],
            &["src pad 1 of element 3", "not marked as probed"],
            &["sink pad 1 of element 3", "supplied already"],
            &["src pad 1", "in preroll", "frozen after"],
            &["src pad 1", "reserved family"],
            &["sink pad 1", "200 values in a set", "limit is 128"],
        ];
        assert_eq!(text.len(), needles.len(), "one needle row per sample");
        for (s, row) in text.iter().zip(needles) {
            for n in row.iter() {
                assert!(s.contains(n), "{s:?} should mention {n:?}");
            }
        }
    }

    #[test]
    fn both_bound_ends_are_named() {
        let d = NoCommonFormat {
            src: end("q1", "src", &["*"], Some("audio/raw, rate=44100")),
            sink: end("q2", "sink", &["*"], Some("audio/raw, rate=48000")),
        };
        let s = d.to_string();
        assert!(s.contains("q1 already carries audio/raw, rate=44100"), "{s}");
        assert!(s.contains("q2 already carries audio/raw, rate=48000"), "{s}");
    }

    #[test]
    fn element_failed_carries_the_plugin_error() {
        let e = Error::element_failed(ElementId(4), DemoError::BadHeader { offset: 9 });
        let c = e.clone();
        drop(e);
        assert_eq!(c.to_string(), "element 4 failed: bad header at 9");
        assert!(matches!(c, Error::ElementFailed { element: ElementId(4), .. }));
        let src = c.source().expect("the payload is the source");
        assert_eq!(src.downcast_ref::<DemoError>(), Some(&DemoError::BadHeader { offset: 9 }));
        assert!(src.downcast_ref::<GraphError>().is_none(), "downcast is by type");
    }

    #[test]
    fn only_the_plugin_payload_is_a_source() {
        for e in samples() {
            let is_plugin = matches!(e, Error::ElementFailed { .. });
            assert_eq!(e.source().is_some(), is_plugin, "{e:?}");
        }
    }

    #[test]
    fn error_is_send_sync_and_stays_small() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Error>();
        assert!(
            std::mem::size_of::<Error>() <= 56,
            "Error grew to {} bytes, box the fat payload",
            std::mem::size_of::<Error>()
        );
    }
}
