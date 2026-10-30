//! Opt-in auto-conversion. [`link_or_convert`] inserts the shortest converter chain that makes a
//! refused link solve. [`replace_sink`] swaps a sink while playing and reruns the inserter.

use std::fmt;

use profluens_core::couple::{Coupling, Rule};
use profluens_core::element::ElementDesc;
use profluens_core::error::{Error, LinkError, SolveError};
use profluens_core::format::FieldSpec;
use profluens_core::id::{ElementId, LinkId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_core::topology::Leave;

/// Converters one family may chain, the bound on the search.
pub const MAX_CHAIN: usize = 3;

/// What an inserter may put between two pads, per family, in chain order.
#[derive(Clone, Copy)]
pub struct ConvertPolicy {
    pub converters: &'static [(&'static str, &'static [&'static ElementDesc])],
}

/// The converters [`link_or_convert`] inserted, upstream first, and the links it made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inserted {
    chain: [ElementId; MAX_CHAIN],
    links: [LinkId; MAX_CHAIN + 1],
    len: u8,
}

impl Inserted {
    pub fn elements(&self) -> &[ElementId] {
        &self.chain[..self.len as usize]
    }

    pub fn links(&self) -> &[LinkId] {
        &self.links[..self.len as usize + 1]
    }

    pub fn tail(&self) -> Option<ElementId> {
        self.elements().last().copied()
    }
}

/// Why [`link_or_convert`] made no edge.
#[derive(Clone, Debug)]
pub enum ConvertError {
    Refused { direct: Error, tried: u32 },
    Link(Error),
    NotConstructible { name: &'static str },
    TooMany { family: &'static str, count: usize },
    Prop { element: ElementId, prop: &'static str, error: Error },
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::Refused { direct, tried } => {
                write!(f, "no converter chain helps, {tried} tried; the direct link: {direct}")
            }
            ConvertError::Link(e) => write!(f, "{e}"),
            ConvertError::NotConstructible { name } => {
                write!(f, "converter '{name}' has no make_default")
            }
            ConvertError::TooMany { family, count } => {
                write!(f, "{count} converters for '{family}', {MAX_CHAIN} at most")
            }
            ConvertError::Prop { element, prop, error } => {
                write!(f, "converter #{} refused {prop}: {error}", element.0)
            }
        }
    }
}

impl std::error::Error for ConvertError {}

/// Link `src` to `sink`, inserting the shortest chain of `policy` converters when the solve
/// refuses or leaves a field unsettled. A failed chain is removed whole.
pub fn link_or_convert(
    p: &mut Pipeline,
    src: (ElementId, SrcPad),
    sink: (ElementId, SinkPad),
    filter: &[FieldSpec<'_>],
    policy: &ConvertPolicy,
) -> Result<Inserted, ConvertError> {
    let alone = |id| Inserted { chain: [src.0; MAX_CHAIN], links: [id; MAX_CHAIN + 1], len: 0 };
    // `None` is a direct link that solves and leaves a field unsettled.
    let direct = match link(p, src, sink, filter) {
        Ok(id) => match p.solve_check() {
            Err(e) if ours(&e, &[id]) => {
                p.unlink(id).map_err(ConvertError::Link)?;
                Some(e)
            }
            _ if p.unsettled(id).next().is_none() => return Ok(alone(id)),
            _ => {
                p.unlink(id).map_err(ConvertError::Link)?;
                None
            }
        },
        Err(e) if fixable(&e) => Some(e),
        Err(e) => return Err(ConvertError::Link(e)),
    };
    let (mut tried, mut solved) = (0, None);
    if let Some(made) = search(p, (src, sink), filter, policy, &mut tried, &mut solved)? {
        return Ok(made);
    }
    match (direct, solved) {
        (None, _) => link(p, src, sink, filter).map(alone).map_err(ConvertError::Link),
        (Some(direct), Some((family, chain))) => {
            match try_chain(p, (src, sink), filter, family, &chain, false)? {
                Trial::Kept(made) => Ok(made),
                Trial::Unsettled | Trial::Failed => Err(ConvertError::Refused { direct, tried }),
            }
        }
        (Some(direct), None) => Err(ConvertError::Refused { direct, tried }),
    }
}

/// Elements [`replace_sink`] may take out ahead of a sink, a full chain plus one caller `queue`.
pub const MAX_REMOVED: usize = MAX_CHAIN + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replaced {
    pub upstream: (ElementId, SrcPad),
    pub inserted: Inserted,
    removed: [ElementId; MAX_REMOVED],
    removed_len: u8,
}

impl Replaced {
    pub fn removed(&self) -> &[ElementId] {
        &self.removed[..self.removed_len as usize]
    }
}

/// Why [`replace_sink`] left the old sink in place.
#[derive(Clone, Debug)]
pub enum ReplaceError {
    Unfed {
        element: ElementId,
    },
    TooMany {
        sink: ElementId,
    },
    Preroll(Error),
    Convert(ConvertError),
    /// The new sink takes only some values `field` may be announced at, and no converter settles
    /// it.
    Unsettled {
        field: Box<str>,
    },
    Remove(Error),
}

impl fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplaceError::Unfed { element } => write!(f, "nothing feeds #{}", element.0),
            ReplaceError::TooMany { sink } => {
                write!(f, "more than {MAX_REMOVED} inserted elements ahead of #{}", sink.0)
            }
            ReplaceError::Preroll(e) => write!(f, "the new sink's preroll failed, {e}"),
            ReplaceError::Convert(e) => {
                write!(f, "the new sink takes nothing the stream converts to, {e}")
            }
            ReplaceError::Unsettled { field } => {
                write!(f, "the new sink may refuse the {field} the stream plays at")
            }
            ReplaceError::Remove(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReplaceError {}

/// Put the caller-added sink `new` in place of `old`. Auto-inserted elements ahead of `old` leave
/// with it. In a run the edits land at the caller's commit and must remove whole groups.
pub fn replace_sink(
    p: &mut Pipeline,
    old: (ElementId, SinkPad),
    new: (ElementId, SinkPad),
    leave: Leave,
    filter: &[FieldSpec<'_>],
    policy: &ConvertPolicy,
) -> Result<Replaced, ReplaceError> {
    let mut removed = [old.0; MAX_REMOVED];
    let mut n = 0;
    let (_, mut up, mut pad) =
        p.link_into(old.0, old.1).ok_or(ReplaceError::Unfed { element: old.0 })?;
    while p.is_auto(up) {
        let slot = removed.get_mut(n).ok_or(ReplaceError::TooMany { sink: old.0 })?;
        (*slot, n) = (up, n + 1);
        (_, up, pad) = p.link_into(up, SinkPad(0)).ok_or(ReplaceError::Unfed { element: up })?;
    }
    p.preroll().map_err(ReplaceError::Preroll)?;
    let inserted =
        link_or_convert(p, (up, pad), new, filter, policy).map_err(ReplaceError::Convert)?;
    let open = inserted.links().iter().find_map(|l| p.unsettled(*l).next());
    if let Some(field) = open {
        let field = p.field_name(field).unwrap_or("?").into();
        match inserted.elements() {
            [] => p.unlink(inserted.links()[0]).map_err(ReplaceError::Remove)?,
            els => els
                .iter()
                .try_for_each(|el| p.remove(*el, Leave::Cut))
                .map_err(ReplaceError::Remove)?,
        }
        return Err(ReplaceError::Unsettled { field });
    }
    for &el in removed[..n].iter().chain([&old.0]) {
        p.remove(el, leave).map_err(ReplaceError::Remove)?;
    }
    p.copy_link_settings(old.0, new.0);
    removed[..n].reverse();
    Ok(Replaced { upstream: (up, pad), inserted, removed, removed_len: n as u8 })
}

type Chain = (&'static str, [Option<&'static ElementDesc>; MAX_CHAIN]);

fn search(
    p: &mut Pipeline,
    ends: ((ElementId, SrcPad), (ElementId, SinkPad)),
    filter: &[FieldSpec<'_>],
    policy: &ConvertPolicy,
    tried: &mut u32,
    solved: &mut Option<Chain>,
) -> Result<Option<Inserted>, ConvertError> {
    let families = p.src_families(ends.0.0, ends.0.1);
    for &(family, convs) in policy.converters {
        if convs.len() > MAX_CHAIN {
            return Err(ConvertError::TooMany { family, count: convs.len() });
        }
        if families.as_ref().is_some_and(|f| !f.contains(&family)) {
            continue;
        }
        let n = convs.len();
        for len in 1..=n as u32 {
            for mask in (1u32..1 << n).filter(|m| m.count_ones() == len) {
                let mut chain = [None; MAX_CHAIN];
                let picked = (0..n).filter(|i| mask & (1 << i) != 0).map(|i| convs[i]);
                for (slot, desc) in chain.iter_mut().zip(picked) {
                    *slot = Some(desc);
                }
                *tried += 1;
                match try_chain(p, ends, filter, family, &chain[..len as usize], true)? {
                    Trial::Kept(made) => return Ok(Some(made)),
                    Trial::Unsettled => *solved = solved.or(Some((family, chain))),
                    Trial::Failed => {}
                }
            }
        }
    }
    Ok(None)
}

fn link(
    p: &mut Pipeline,
    src: (ElementId, SrcPad),
    sink: (ElementId, SinkPad),
    filter: &[FieldSpec<'_>],
) -> Result<LinkId, Error> {
    match filter.is_empty() {
        true => p.link(src, sink),
        false => p.link_filtered(src, sink, filter),
    }
}

fn fixable(e: &Error) -> bool {
    matches!(
        e,
        Error::Link(LinkError::NoCommonFormat(_))
            | Error::Solve(SolveError::Empty(_) | SolveError::FilterRejected(_))
    )
}

fn ours(e: &Error, links: &[LinkId]) -> bool {
    match e {
        Error::Solve(SolveError::Empty(x)) => {
            links.contains(&x.link)
                || x.path.iter().any(|n| n.link.is_some_and(|l| links.contains(&l)))
        }
        Error::Solve(SolveError::TooManyFields { link } | SolveError::Inconsistent { link }) => {
            links.contains(link)
        }
        _ => false,
    }
}

enum Trial {
    Kept(Inserted),
    Unsettled,
    Failed,
}

fn try_chain(
    p: &mut Pipeline,
    (src, sink): ((ElementId, SrcPad), (ElementId, SinkPad)),
    filter: &[FieldSpec<'_>],
    family: &str,
    descs: &[Option<&'static ElementDesc>],
    settle: bool,
) -> Result<Trial, ConvertError> {
    let mut made =
        Inserted { chain: [src.0; MAX_CHAIN], links: [LinkId(u32::MAX); MAX_CHAIN + 1], len: 0 };
    for desc in descs.iter().flatten() {
        let make = desc.make_default.ok_or(ConvertError::NotConstructible { name: desc.name })?;
        made.chain[made.len as usize] = p.add_boxed(make());
        made.len += 1;
    }
    let (ends, n) = (made.chain, made.len as usize);
    let ends = &ends[..n];
    let mut from = src;
    let mut linked = true;
    for (i, &el) in ends.iter().enumerate() {
        match p.link(from, el.sink(SinkPad(0))) {
            Ok(id) => made.links[i] = id,
            Err(_) => {
                linked = false;
                break;
            }
        }
        from = el.src(SrcPad(0));
    }
    if linked {
        match link(p, from, sink, filter) {
            Ok(id) => made.links[n] = id,
            Err(_) => linked = false,
        }
    }
    // Every link carries the family, so a `bytes` escape cannot launder a mismatch.
    let carried = |p: &Pipeline, l: &LinkId| {
        p.negotiated(*l).and_then(|f| p.family_name(f.family)) == Some(family)
    };
    let solved = linked
        && made.links().iter().all(|l| carried(p, l))
        && p.solve_check().map_or_else(|e| !ours(&e, made.links()), |()| true);
    let open = solved && settle && made.links().iter().any(|l| p.unsettled(*l).next().is_some());
    if !solved || open {
        take_away(ends, |el| p.remove(el, Leave::Cut))?;
        return Ok(if open { Trial::Unsettled } else { Trial::Failed });
    }
    let set = made
        .elements()
        .iter()
        .enumerate()
        .try_for_each(|(i, &el)| set_solved_props(p, el, made.links[i + 1]));
    if let Err(e) = set {
        let _ = take_away(ends, |el| p.remove(el, Leave::Cut));
        return Err(e);
    }
    for (i, &el) in made.elements().iter().enumerate() {
        p.mark_auto(el, made.links[i]);
    }
    Ok(Trial::Kept(made))
}

fn take_away(
    ends: &[ElementId],
    mut remove: impl FnMut(ElementId) -> Result<(), Error>,
) -> Result<(), ConvertError> {
    let mut first = Ok(());
    for &el in ends {
        if let Err(e) = remove(el) {
            first = first.and(Err(ConvertError::Link(e)));
        }
    }
    first
}

fn set_solved_props(p: &mut Pipeline, el: ElementId, out: LinkId) -> Result<(), ConvertError> {
    let Some(desc) = p.element_desc(el) else {
        return Ok(());
    };
    for c in desc.couplings {
        let Coupling::Field { src: 0, field, rule: Rule::Prop { prop }, .. } = *c else {
            continue;
        };
        if p.prop_value(el, prop).is_some() {
            continue;
        }
        let fixed = p.field_id(field).and_then(|f| p.negotiated(out)?.get(f));
        if let Some(v) = fixed {
            p.set(el, prop, v).map_err(|error| ConvertError::Prop { element: el, prop, error })?;
        }
    }
    Ok(())
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use profluens_core::error::{EmptyIntersection, GraphError, LinkEnd, Narrowing};

    fn end(el: u32) -> LinkEnd {
        LinkEnd {
            element: ElementId(el),
            element_name: "x",
            pad: "p".into(),
            offers: Box::new([]),
            bound: None,
        }
    }

    fn empty(link: u32, via: Option<u32>) -> Error {
        let path = via.map(|l| Narrowing {
            link: Some(LinkId(l)),
            element: ElementId(0),
            element_name: "x",
            pad: "src".into(),
            rule: "peer",
            constraint: "48000".into(),
        });
        let x = EmptyIntersection {
            link: LinkId(link),
            src: end(0),
            sink: end(1),
            field: Some("rate".into()),
            path: path.into_iter().collect(),
        };
        Error::Solve(SolveError::Empty(Box::new(x)))
    }

    #[test]
    fn a_chain_is_taken_away_whole_past_a_removal_refused() {
        let mut tried = Vec::new();
        let unknown = |el: u32| Error::Link(LinkError::UnknownElement { element: ElementId(el) });
        let got = take_away(&[ElementId(3), ElementId(4), ElementId(5)], |el| {
            tried.push(el.0);
            if el.0 == 5 { Ok(()) } else { Err(unknown(el.0)) }
        });
        assert_eq!(tried, [3, 4, 5], "each removed");
        let Err(ConvertError::Link(Error::Link(LinkError::UnknownElement { element }))) = got
        else {
            panic!("{got:?}")
        };
        assert_eq!(element, ElementId(3), "the first refusal");
        assert!(take_away(&[ElementId(1)], |_| Ok(())).is_ok());
    }

    #[test]
    fn a_failure_is_the_chains_when_it_names_one_of_its_links() {
        let mine = [LinkId(4), LinkId(5)];
        assert!(ours(&empty(5, None), &mine));
        assert!(ours(&empty(9, Some(4)), &mine));
        assert!(!ours(&empty(9, Some(8)), &mine));
        assert!(ours(&Error::Solve(SolveError::TooManyFields { link: LinkId(4) }), &mine));
        let elsewhere =
            Error::Graph(GraphError::NotPrerolled { element: ElementId(3), element_name: "x" });
        assert!(!ours(&elsewhere, &mine));
        assert!(fixable(&empty(1, None)));
        assert!(!fixable(&Error::Link(LinkError::UnknownElement { element: ElementId(9) })));
    }
}
