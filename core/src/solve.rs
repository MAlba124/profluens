//! The whole-graph format solve, a demand pass in reverse topological order, a fix pass in order
//! and a verify pass.

use crate::format::{
    Constraint, FixedFormat, Interned, MAX_OFFER_FIELDS, MAX_PROBED_OFFERS, Value,
};
use crate::id::{FieldId, FormatId, Interner, LinkId};

pub(crate) const NONE: u32 = u32::MAX;

const KEPT_DOMS: usize = 1 << 15;

const KEPT_A: u8 = 1;
const KEPT_B: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Link(LinkId),
    Fresh,
}

/// A field's domain with a set's values and a range's bounds held in the solver's values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum DomC {
    Any,
    Eq(Value),
    Range(u32),
    Set(u32, u32),
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Reason {
    SrcPreferred,
    SinkPreferred,
    First,
    Min,
    #[default]
    Only,
}

impl Reason {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Reason::SrcPreferred => "src preferred",
            Reason::SinkPreferred => "sink preferred",
            Reason::First => "first offered",
            Reason::Min => "range min",
            Reason::Only => "only value",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum By {
    #[default]
    Peer,
    Filter,
    Pinned,
    Through,
    Same,
    Copy,
    Prop,
    Fixed,
}

impl By {
    pub(crate) fn name(self) -> &'static str {
        match self {
            By::Peer => "peer",
            By::Filter => "filter",
            By::Pinned => "running format",
            By::Through => "through",
            By::Same => "same",
            By::Copy => "copy",
            By::Prop => "prop",
            By::Fixed => "fixed",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Step {
    pub(crate) prev: u32,
    pub(crate) by: By,
    pub(crate) slot: u32,
    pub(crate) link: u32,
    pub(crate) c: DomC,
}

#[derive(Clone, Copy)]
struct Dom {
    field: FieldId,
    c: DomC,
    pref: u32,
    from: u32,
}

#[derive(Clone, Copy)]
struct Alt {
    family: FormatId,
    wild: bool,
    at: u32,
    n: u32,
    pa: u32,
    pb: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct List {
    at: u32,
    n: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct Slot {
    pub(crate) el: u32,
    pub(crate) src: bool,
    pub(crate) pad: u32,
    elem: u32,
    wild: bool,
    pub(crate) coupled: bool,
    own: List,
    dem: List,
    fwd: List,
    next: u32,
    edges: u32,
    out_edges: u32,
    opens: u32,
    /// The one format every typed edge of a src pad carries once known.
    pick: Option<List>,
    joint: bool,
    wide: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct EdgeSlot {
    pub(crate) link: LinkId,
    pub(crate) src: u32,
    pub(crate) sink: u32,
    next_src: u32,
    next_sink: u32,
    filter: Option<List>,
    pin: Option<List>,
    probe: bool,
    peer: List,
    skip: Option<FormatId>,
    /// Running, or its sink takes `bytes` alone, so it does not bind its src pad's format.
    free: bool,
    wide: bool,
    pub(crate) why: [Reason; MAX_OFFER_FIELDS],
    /// Bit per field of `out` no upstream element states, decided by the announcement.
    pub(crate) open: u16,
    pub(crate) risky: u16,
    opens: u32,
}

impl EdgeSlot {
    pub(crate) fn solved(&self) -> bool {
        self.pin.is_none() && !self.probe
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Through,
    Same { to: FieldId, from: FieldId, by: By },
}

#[derive(Clone, Copy)]
struct RuleSlot {
    sink: u32,
    src: u32,
    kind: Kind,
}

#[derive(Clone, Copy)]
struct Elem {
    first: u32,
    rules: (u32, u32),
    indeg: u32,
    fed: bool,
}

#[derive(Clone, Copy)]
struct Open {
    field: FieldId,
    origin: DomC,
    next: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailKind {
    Empty,
    Wide,
    Inconsistent,
    Cycle,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Fail {
    pub(crate) edge: u32,
    pub(crate) kind: FailKind,
    pub(crate) field: Option<FieldId>,
    pub(crate) from: [u32; 2],
    pub(crate) lists: [List; 2],
}

#[derive(Clone, Copy, Default)]
struct Why {
    by: By,
    slot: u32,
    link: u32,
}

enum Miss {
    Disjoint,
    Wide,
}

#[derive(Default)]
pub(crate) struct Solver {
    vals: Vec<Value>,
    doms: Vec<Dom>,
    alts: Vec<Alt>,
    pub(crate) slots: Vec<Slot>,
    pub(crate) edges: Vec<EdgeSlot>,
    outs: Vec<FixedFormat>,
    elems: Vec<Elem>,
    elem_ids: Vec<u32>,
    rules: Vec<RuleSlot>,
    order: Vec<u32>,
    stack: Vec<u32>,
    pub(crate) steps: Vec<Step>,
    el_at: Vec<u32>,
    opens: Vec<Open>,
    wild_family: Option<FormatId>,
    bytes_family: Option<FormatId>,
    wide: bool,
    pub(crate) scope: Vec<LinkId>,
    pub(crate) work: Vec<u64>,
    pub(crate) seen: Vec<u64>,
    pub(crate) ends: Vec<([u64; 2], u32, bool, bool)>,
    pub(crate) pads: Vec<(u64, u32)>,
    pub(crate) mask: Vec<u32>,
    pub(crate) fed: Vec<u32>,
    low: Vec<Option<(List, bool)>>,
    base: [usize; 3],
    base_sets: Vec<(u32, u32)>,
    bys: Vec<(Dom, Why)>,
    picks: Vec<(u32, FieldId)>,
    taken: Vec<Value>,
    pub(crate) missed: Vec<(u32, (bool, u32))>,
    pub(crate) families: (usize, [Option<FormatId>; 2]),
    pub(crate) meets: u64,
    pub(crate) trace: bool,
}

impl Solver {
    pub(crate) fn begin(&mut self, elements: usize, [wild, bytes]: [Option<FormatId>; 2]) {
        if self.base[1] > KEPT_DOMS {
            self.base = [0; 3];
            self.low.clear();
            self.base_sets.clear();
        }
        self.vals.truncate(self.base[0]);
        self.opens.clear();
        self.picks.clear();
        self.doms.truncate(self.base[1]);
        self.alts.truncate(self.base[2]);
        self.slots.clear();
        self.edges.clear();
        self.elems.clear();
        self.rules.clear();
        self.steps.clear();
        for &el in &self.elem_ids {
            self.el_at[el as usize] = NONE;
        }
        self.elem_ids.clear();
        self.el_at.resize(elements.max(self.el_at.len()), NONE);
        (self.wild_family, self.bytes_family, self.wide, self.meets) = (wild, bytes, false, 0);
    }

    pub(crate) fn elem_count(&self) -> usize {
        self.elems.len()
    }

    pub(crate) fn elem_id(&self, i: usize) -> u32 {
        self.elem_ids[i]
    }

    pub(crate) fn find(&self, el: u32, src: bool, pad: u32) -> Option<u32> {
        let elem = *self.el_at.get(el as usize).filter(|e| **e != NONE)?;
        let mut s = self.elems[elem as usize].first;
        while s != NONE {
            let slot = &self.slots[s as usize];
            if (slot.src, slot.pad) == (src, pad) {
                return Some(s);
            }
            s = slot.next;
        }
        None
    }

    /// Lower the offers of `menu` into the kept head once, ahead of anything a solve adds.
    pub(crate) fn keep(&mut self, menu: u32, offers: &Interned) {
        if menu == NONE || self.low.get(menu as usize).is_some_and(Option::is_some) {
            return;
        }
        debug_assert_eq!(self.base, [self.vals.len(), self.doms.len(), self.alts.len()]);
        let list = self.lower(offers, true, false);
        if self.low.len() <= menu as usize {
            self.low.resize(menu as usize + 1, None);
        }
        self.low[menu as usize] = Some((list, offers.offers.iter().any(|o| o.2)));
        self.base = [self.vals.len(), self.doms.len(), self.alts.len()];
    }

    pub(crate) fn slot_kept(&mut self, el: u32, (src, pad): (bool, u32), menu: u32) -> Option<u32> {
        let (own, wild) = self.low.get(menu as usize).copied().flatten()?;
        Some(match self.find(el, src, pad) {
            Some(s) => s,
            None => self.put_slot(el, (src, pad), own, wild),
        })
    }

    pub(crate) fn slot(
        &mut self,
        el: u32,
        (src, pad): (bool, u32),
        menu: u32,
        offers: &Interned,
    ) -> u32 {
        if let Some(s) = self.find(el, src, pad) {
            return s;
        }
        let (own, wild) = match self.low.get(menu as usize).copied().flatten() {
            Some(kept) => kept,
            None => {
                if menu != NONE {
                    self.missed.push((el, (src, pad)));
                }
                (self.lower(offers, false, false), offers.offers.iter().any(|o| o.2))
            }
        };
        self.put_slot(el, (src, pad), own, wild)
    }

    fn put_slot(&mut self, el: u32, (src, pad): (bool, u32), own: List, wild: bool) -> u32 {
        if self.el_at[el as usize] == NONE {
            self.el_at[el as usize] = self.elems.len() as u32;
            self.elems.push(Elem { first: NONE, rules: (0, 0), indeg: 0, fed: false });
            self.elem_ids.push(el);
        }
        let elem = self.el_at[el as usize];
        let at = self.slots.len() as u32;
        let (dem, fwd, coupled, next, edges) = (own, own, false, NONE, NONE);
        let (pick, joint, wide) = (None, false, false);
        self.slots.push(Slot {
            el,
            src,
            pad,
            elem,
            wild,
            coupled,
            own,
            dem,
            fwd,
            next,
            edges,
            out_edges: 0,
            opens: NONE,
            pick,
            joint,
            wide,
        });
        let key = |s: &Slot| (s.src, s.pad);
        let first = self.elems[elem as usize].first;
        if first == NONE || key(&self.slots[first as usize]) > (src, pad) {
            self.slots[at as usize].next = first;
            self.elems[elem as usize].first = at;
            return at;
        }
        let mut p = first;
        while let Some(n) = Some(self.slots[p as usize].next).filter(|n| *n != NONE) {
            if key(&self.slots[n as usize]) > (src, pad) {
                break;
            }
            p = n;
        }
        self.slots[at as usize].next = self.slots[p as usize].next;
        self.slots[p as usize].next = at;
        at
    }

    pub(crate) fn edge(
        &mut self,
        link: LinkId,
        (src, sink): (u32, u32),
        filter: Option<&Interned>,
        pinned: Option<(&FixedFormat, u16)>,
        probe: bool,
    ) {
        let filter = filter.map(|f| self.lower(f, false, true));
        let pin = pinned.map(|(f, _)| self.pinned(f));
        let at = self.edges.len() as u32;
        let (next_src, next_sink) =
            (self.slots[src as usize].edges, self.slots[sink as usize].edges);
        if self.outs.len() == at as usize {
            self.outs.push(FixedFormat::new(FormatId(0)));
        }
        match pinned {
            Some((f, _)) => self.outs[at as usize].copy_from(f),
            None => self.outs[at as usize].reset(FormatId(0)),
        }
        let (why, open) = ([Reason::default(); MAX_OFFER_FIELDS], pinned.map_or(0, |p| p.1));
        let (risky, opens) = (0, NONE);
        let (peer, skip, free, wide) = (List::default(), None, false, false);
        self.edges.push(EdgeSlot {
            link,
            src,
            sink,
            next_src,
            next_sink,
            filter,
            pin,
            probe,
            peer,
            skip,
            free,
            wide,
            why,
            open,
            risky,
            opens,
        });
        self.slots[src as usize].out_edges += !probe as u32;
        self.slots[src as usize].edges = at;
        self.slots[sink as usize].edges = at;
        if let Some((f, open)) = pinned.filter(|_| !probe) {
            self.pin_pad(src, f, open, link.0, pin);
        }
    }

    /// A src pad running at `f` carries it on every typed edge, its open fields open.
    pub(crate) fn pin_pad(
        &mut self,
        slot: u32,
        f: &FixedFormat,
        open: u16,
        link: u32,
        lowered: Option<List>,
    ) {
        let untyped = Some(f.family) == self.bytes_family
            || (Some(f.family) == self.wild_family && f.is_empty());
        if untyped || self.slots[slot as usize].pick.is_some() {
            return;
        }
        let list = match lowered.filter(|_| !self.trace) {
            Some(list) => list,
            None => self.pinned(f),
        };
        let why = Why { by: By::Pinned, slot, link };
        let (at, n) = (self.alts[list.at as usize].at, if self.trace { f.len() as u32 } else { 0 });
        for d in at..at + n {
            self.doms[d as usize].from = self.step(NONE, why, self.doms[d as usize].c);
        }
        let open = f.fields().iter().enumerate().filter(|(i, _)| open >> i & 1 == 1);
        for field in open.map(|(_, x)| x.0) {
            self.open_at(slot, false, field, DomC::Any);
        }
        self.slots[slot as usize].pick = Some(list);
    }

    pub(crate) fn begin_rules(&mut self, elem: usize, fed: bool) {
        let at = self.rules.len() as u32;
        (self.elems[elem].rules, self.elems[elem].fed) = ((at, at), fed);
    }

    pub(crate) fn through(&mut self, elem: usize, sink: u32, src: u32) {
        self.rule(elem, RuleSlot { sink, src, kind: Kind::Through });
    }

    pub(crate) fn same(
        &mut self,
        elem: usize,
        (sink, src): (u32, u32),
        to: FieldId,
        from: FieldId,
        copy: bool,
    ) {
        let by = if copy { By::Copy } else { By::Same };
        self.rule(elem, RuleSlot { sink, src, kind: Kind::Same { to, from, by } });
    }

    fn rule(&mut self, elem: usize, r: RuleSlot) {
        self.slots[r.sink as usize].coupled = true;
        self.slots[r.src as usize].coupled = true;
        self.rules.push(r);
        self.elems[elem].rules.1 += 1;
    }

    pub(crate) fn constant(&mut self, src: u32, field: FieldId, value: Value, prop: bool) {
        let by = if prop { By::Prop } else { By::Fixed };
        let own = self.slots[src as usize].own;
        let own = self.narrow_field(own, field, DomC::Eq(value), Why { by, slot: src, link: NONE });
        let slot = &mut self.slots[src as usize];
        (slot.own, slot.dem, slot.fwd) = (own, own, own);
    }

    /// Field `field` of `src` is what the solve fixes, an unset property taken from the solved src.
    pub(crate) fn chosen(&mut self, src: u32, field: FieldId) {
        self.open_at(src, false, field, DomC::Empty);
    }

    pub(crate) fn picks(&mut self, src: u32, field: FieldId) {
        self.picks.push((src, field));
    }

    /// `origin` held to what downstream of edge `e` takes, as is where it takes any or none of it.
    fn held_to_demand(&mut self, e: u32, field: FieldId, origin: DomC) -> DomC {
        let mut taken = std::mem::take(&mut self.taken);
        let mut held = origin;
        if self.takes(e as usize, field, &mut taken) {
            taken.retain(|v| self.accepts(origin, *v));
            if !taken.is_empty() {
                held = DomC::Set(self.vals.len() as u32, taken.len() as u32);
                self.vals.extend_from_slice(&taken);
            }
        }
        self.taken = taken;
        held
    }

    fn open_at(&mut self, at: u32, edge: bool, field: FieldId, origin: DomC) {
        let head = match edge {
            true => &mut self.edges[at as usize].opens,
            false => &mut self.slots[at as usize].opens,
        };
        let next = std::mem::replace(head, self.opens.len() as u32);
        self.opens.push(Open { field, origin, next });
    }

    fn open_of(&self, at: u32, edge: bool, field: FieldId) -> Option<DomC> {
        let mut o =
            if edge { self.edges[at as usize].opens } else { self.slots[at as usize].opens };
        while let Some(x) = self.opens.get(o as usize) {
            if x.field == field {
                return Some(x.origin);
            }
            o = x.next;
        }
        None
    }

    /// Close the rules of `elem`. With none stated each wildcard sink passes through to each
    /// wildcard src.
    pub(crate) fn end_rules(&mut self, elem: usize, implicit: bool) {
        let mut k = self.elems[elem].first;
        while implicit && k != NONE {
            let sink = self.slots[k as usize];
            let mut s = if !sink.src && sink.wild { sink.next } else { NONE };
            while s != NONE {
                let src = self.slots[s as usize];
                if src.src && src.wild {
                    self.through(elem, k, s);
                }
                s = src.next;
            }
            k = sink.next;
        }
    }

    /// `offers` as alternatives. An element's wildcard with fields meets nothing and is left out.
    fn lower(&mut self, offers: &Interned, kept: bool, filter: bool) -> List {
        let at = self.alts.len() as u32;
        let mut f = 0;
        for &(family, n, wild) in &offers.offers {
            if wild && n > 0 && !filter {
                f += n;
                continue;
            }
            let d = self.doms.len() as u32;
            for x in &offers.fields[f..f + n] {
                let c = match x.allowed {
                    Constraint::Any => DomC::Any,
                    Constraint::Eq(v) => DomC::Eq(v),
                    Constraint::Range { min, max, step } => self.range(min, max, step),
                    Constraint::Set(_) => self.lower_set(&offers.sets[x.set.clone()], kept),
                };
                let pref = x.preferred.map_or(NONE, |v| {
                    self.vals.push(v);
                    self.vals.len() as u32 - 1
                });
                self.doms.push(Dom { field: x.field, c, pref, from: NONE });
            }
            f += n;
            self.alts.push(Alt { family, wild, at: d, n: n as u32, pa: NONE, pb: NONE });
        }
        List { at, n: self.alts.len() as u32 - at }
    }

    fn lower_set(&mut self, vs: &[Value], kept: bool) -> DomC {
        let n = vs.len() as u32;
        let same =
            |&&(at, m): &&(u32, u32)| m == n && self.vals[at as usize..(at + m) as usize] == *vs;
        match vs {
            [] => DomC::Empty,
            [v] => DomC::Eq(*v),
            _ => match self.base_sets.iter().find(same).filter(|_| kept) {
                Some(&(at, _)) => DomC::Set(at, n),
                None => {
                    let at = self.vals.len() as u32;
                    self.vals.extend_from_slice(vs);
                    if kept {
                        self.base_sets.push((at, n));
                    }
                    DomC::Set(at, n)
                }
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn lower_c(&mut self, c: &Constraint<'_>) -> DomC {
        match *c {
            Constraint::Any => DomC::Any,
            Constraint::Eq(v) => DomC::Eq(v),
            Constraint::Range { min, max, step } => self.range(min, max, step),
            Constraint::Set(vs) => {
                let start = self.vals.len() as u32;
                self.vals.extend_from_slice(vs);
                self.set(start)
            }
        }
    }

    fn pinned(&mut self, f: &FixedFormat) -> List {
        let wild = Some(f.family) == self.wild_family && f.is_empty();
        let d = self.doms.len() as u32;
        for &(field, v) in f.fields() {
            self.doms.push(Dom { field, c: DomC::Eq(v), pref: NONE, from: NONE });
        }
        self.alts.push(Alt {
            family: f.family,
            wild,
            at: d,
            n: f.len() as u32,
            pa: NONE,
            pb: NONE,
        });
        List { at: self.alts.len() as u32 - 1, n: 1 }
    }

    fn pinned_out(&mut self, e: u32) -> List {
        let (family, n) = (self.outs[e as usize].family, self.outs[e as usize].len());
        let d = self.doms.len() as u32;
        for i in 0..n {
            let (field, v) = self.outs[e as usize].fields()[i];
            self.doms.push(Dom { field, c: DomC::Eq(v), pref: NONE, from: NONE });
        }
        let wild = Some(family) == self.wild_family && n == 0;
        self.alts.push(Alt { family, wild, at: d, n: n as u32, pa: NONE, pb: NONE });
        List { at: self.alts.len() as u32 - 1, n: 1 }
    }

    fn set(&mut self, start: u32) -> DomC {
        match self.vals.len() as u32 - start {
            0 => DomC::Empty,
            1 => self.vals.pop().map_or(DomC::Empty, DomC::Eq),
            n => DomC::Set(start, n),
        }
    }

    pub(crate) fn range(&mut self, min: Value, max: Value, step: Value) -> DomC {
        self.vals.extend_from_slice(&[min, max, step]);
        DomC::Range(self.vals.len() as u32 - 3)
    }

    fn bounds(&self, at: u32) -> (Value, Value, Value) {
        let at = at as usize;
        (self.vals[at], self.vals[at + 1], self.vals[at + 2])
    }

    #[inline]
    fn accepts(&self, c: DomC, v: Value) -> bool {
        match c {
            DomC::Any => true,
            DomC::Empty => false,
            DomC::Eq(x) => x == v,
            DomC::Set(at, n) => self.vals[at as usize..(at + n) as usize].contains(&v),
            DomC::Range(at) => {
                let (min, max, step) = self.bounds(at);
                Constraint::Range { min, max, step }.accepts(v)
            }
        }
    }

    #[inline]
    fn same_c(&self, a: DomC, b: DomC) -> bool {
        match (a, b) {
            (DomC::Set(x, n), DomC::Set(y, m)) => {
                n == m
                    && (x == y
                        || self.vals[x as usize..(x + n) as usize]
                            == self.vals[y as usize..(y + m) as usize])
            }
            (DomC::Range(x), DomC::Range(y)) => self.bounds(x) == self.bounds(y),
            _ => a == b,
        }
    }

    pub(crate) fn meet(&mut self, a: DomC, b: DomC) -> DomC {
        self.meet_which(a, b).0
    }

    #[inline]
    fn meet_which(&mut self, a: DomC, b: DomC) -> (DomC, u8) {
        use DomC::*;
        self.meets += 1;
        if self.same_c(a, b) {
            return (a, KEPT_A | KEPT_B);
        }
        let new = |c| (c, 0);
        match (a, b) {
            (Empty, _) | (_, Empty) => new(Empty),
            (Any, x) => (x, KEPT_B),
            (x, Any) => (x, KEPT_A),
            (Eq(v), x) | (x, Eq(v)) => match self.accepts(x, v) {
                true => (Eq(v), if a == Eq(v) { KEPT_A } else { KEPT_B }),
                false => new(Empty),
            },
            (Set(at, n), x) | (x, Set(at, n)) => {
                let start = self.vals.len() as u32;
                for i in at..at + n {
                    let v = self.vals[i as usize];
                    if self.accepts(x, v) {
                        self.vals.push(v);
                    }
                }
                if self.vals.len() as u32 - start == n {
                    self.vals.truncate(start as usize);
                    return (Set(at, n), if a == Set(at, n) { KEPT_A } else { KEPT_B });
                }
                new(self.set(start))
            }
            (Range(x), Range(y)) => new({
                let ((a0, a1, sa), (b0, b1, sb)) = (self.bounds(x), self.bounds(y));
                match (a0, a1, b0, b1) {
                    (Value::Int(a0), Value::Int(a1), Value::Int(b0), Value::Int(b1)) => {
                        let s = |v| match v {
                            Value::Int(s) if s > 0 => s as i128,
                            _ => 1,
                        };
                        match lattice(
                            (a0 as i128, a1 as i128, s(sa)),
                            (b0 as i128, b1 as i128, s(sb)),
                        ) {
                            Lattice::Empty => Empty,
                            Lattice::One(v) => Eq(Value::Int(v)),
                            Lattice::Two(x, y) => {
                                let at = self.vals.len() as u32;
                                self.vals.extend_from_slice(&[Value::Int(x), Value::Int(y)]);
                                self.set(at)
                            }
                            Lattice::Range(min, max, step) => {
                                self.range(Value::Int(min), Value::Int(max), Value::Int(step))
                            }
                        }
                    }
                    (Value::Rat(..), Value::Rat(..), Value::Rat(..), Value::Rat(..)) => {
                        use core::cmp::Ordering::{Equal, Greater, Less};
                        let (lo, hi) = match (a0.partial_cmp(&b0), a1.partial_cmp(&b1)) {
                            (Some(l), Some(h)) => (
                                if l == Less { b0 } else { a0 },
                                if h == Greater { b1 } else { a1 },
                            ),
                            _ => return (Empty, 0),
                        };
                        match lo.partial_cmp(&hi) {
                            Some(Less) => self.range(lo, hi, sa),
                            Some(Equal) => Eq(lo),
                            _ => Empty,
                        }
                    }
                    _ => Empty,
                }
            }),
        }
    }

    /// One value of a domain, a preference it admits first. `None` leaves the field open.
    #[inline]
    pub(crate) fn choose(
        &self,
        c: DomC,
        src: Option<Value>,
        sink: Option<Value>,
    ) -> Option<(Value, Reason)> {
        if matches!(c, DomC::Any | DomC::Empty) {
            return None;
        }
        let liked = [(src, Reason::SrcPreferred), (sink, Reason::SinkPreferred)];
        let liked =
            liked.into_iter().find_map(|(v, r)| v.filter(|v| self.accepts(c, *v)).map(|v| (v, r)));
        liked.or(match c {
            DomC::Eq(v) => Some((v, Reason::Only)),
            DomC::Set(at, _) => Some((self.vals[at as usize], Reason::First)),
            DomC::Range(at) => Some((self.vals[at as usize], Reason::Min)),
            DomC::Any | DomC::Empty => None,
        })
    }

    fn step(&mut self, prev: u32, why: Why, c: DomC) -> u32 {
        if !self.trace {
            return NONE;
        }
        self.steps.push(Step { prev, by: why.by, slot: why.slot, link: why.link, c });
        self.steps.len() as u32 - 1
    }

    fn meet_dom(&mut self, a: Dom, b: Dom, why: Why) -> Option<Dom> {
        self.meet_dom_kept(a, b, why).map(|(d, _)| d)
    }

    #[inline]
    fn meet_dom_kept(&mut self, a: Dom, b: Dom, why: Why) -> Option<(Dom, bool)> {
        let (c, which) = self.meet_which(a.c, b.c);
        if c == DomC::Empty {
            return None;
        }
        if which & KEPT_A != 0 && (a.pref != NONE || b.pref == NONE) {
            return Some((a, true));
        }
        let from = self.history(a, b, which, why, c);
        Some((
            Dom { field: a.field, c, pref: if a.pref != NONE { a.pref } else { b.pref }, from },
            false,
        ))
    }

    fn history(&mut self, a: Dom, b: Dom, which: u8, why: Why, c: DomC) -> u32 {
        match which {
            w if w & KEPT_A != 0 => a.from,
            KEPT_B => self.step(b.from, why, c),
            _ => self.step(a.from, why, c),
        }
    }

    #[inline]
    fn dom(&self, alt: Alt, field: FieldId) -> Option<Dom> {
        self.doms[alt.at as usize..(alt.at + alt.n) as usize]
            .iter()
            .find(|d| d.field == field)
            .copied()
    }

    /// Two alternatives met. A wildcard with fields is a filter met into a wildcard.
    fn meet_alt(&mut self, a: Alt, b: Alt, why: Why) -> Result<Alt, Miss> {
        match (a.wild, b.wild) {
            (true, _) if a.n == 0 => return Ok(b),
            (_, true) if b.n == 0 => return Ok(a),
            (true, true) => {
                let x = self.meet_alt(
                    Alt { wild: false, ..a },
                    Alt { wild: false, family: a.family, ..b },
                    why,
                )?;
                return Ok(Alt { wild: true, ..x });
            }
            (true, false) => {
                return self.meet_alt(b, Alt { family: b.family, wild: false, ..a }, why);
            }
            (false, true) => {
                return self.meet_alt(a, Alt { family: a.family, wild: false, ..b }, why);
            }
            _ if a.family != b.family => return Err(Miss::Disjoint),
            _ => {}
        }
        let plain = |x: Alt| Ok(Alt { pa: NONE, pb: NONE, ..x });
        if (a.at, a.n) == (b.at, b.n) {
            return plain(a);
        }
        if a.n != b.n {
            return self.meet_fields(a, b, why);
        }
        // While every met domain is one side's the result is that side, shared.
        let at = self.doms.len();
        let mut side = if self.trace { KEPT_A } else { KEPT_A | KEPT_B };
        for i in 0..a.n {
            let (da, db) = (self.doms[(a.at + i) as usize], self.doms[(b.at + i) as usize]);
            if da.field != db.field {
                self.doms.truncate(at);
                return self.meet_fields(a, b, why);
            }
            // Equal domains and two differing values, the most common pairs, need no meet.
            let (c, which) = match (da.c, db.c) {
                (x, y) if x == y => {
                    self.meets += 1;
                    (x, KEPT_A | KEPT_B)
                }
                (DomC::Eq(_), DomC::Eq(_)) => {
                    self.meets += 1;
                    (DomC::Empty, 0)
                }
                _ => self.meet_which(da.c, db.c),
            };
            if c == DomC::Empty {
                self.doms.truncate(at);
                return Err(Miss::Disjoint);
            }
            let pref = if da.pref != NONE { da.pref } else { db.pref };
            let kept_a = which & KEPT_A != 0 && (da.pref != NONE || db.pref == NONE);
            let kept_b = which & KEPT_B != 0 && (da.pref == NONE || da.pref == db.pref);
            let kept = (kept_a as u8 * KEPT_A) | (kept_b as u8 * KEPT_B);
            if side & kept != 0 {
                side &= kept;
                continue;
            }
            if side != 0 {
                let from = if side & KEPT_A != 0 { a.at } else { b.at };
                self.doms.extend_from_within(from as usize..(from + i) as usize);
                side = 0;
            }
            let from = self.history(da, db, which, why, c);
            self.doms.push(Dom { field: da.field, c, pref, from });
        }
        match side {
            0 => {
                Ok(Alt { family: a.family, wild: false, at: at as u32, n: a.n, pa: NONE, pb: NONE })
            }
            s if s & KEPT_A != 0 => plain(a),
            _ => plain(b),
        }
    }

    fn meet_fields(&mut self, a: Alt, b: Alt, why: Why) -> Result<Alt, Miss> {
        let at = self.doms.len();
        for i in a.at..a.at + a.n {
            let da = self.doms[i as usize];
            let d = match self.dom(b, da.field) {
                None => Some(da),
                Some(db) => self.meet_dom(da, db, why),
            };
            let Some(d) = d else {
                self.doms.truncate(at);
                return Err(Miss::Disjoint);
            };
            self.doms.push(d);
        }
        for i in b.at..b.at + b.n {
            let db = self.doms[i as usize];
            if self.dom(a, db.field).is_some() {
                continue;
            }
            if self.doms.len() - at == MAX_OFFER_FIELDS {
                self.doms.truncate(at);
                return Err(Miss::Wide);
            }
            self.doms.push(db);
        }
        Ok(Alt {
            family: a.family,
            wild: false,
            at: at as u32,
            n: (self.doms.len() - at) as u32,
            pa: NONE,
            pb: NONE,
        })
    }

    fn meet_lists(&mut self, a: List, b: List, why: Why) -> List {
        self.pairs(a, b, why, None, false)
    }

    fn pairs(&mut self, a: List, b: List, why: Why, skip: Option<FormatId>, first: bool) -> List {
        let at = self.alts.len() as u32;
        'all: for i in a.at..a.at + a.n {
            for j in b.at..b.at + b.n {
                if self.alts.len() as u32 - at == MAX_PROBED_OFFERS as u32 {
                    self.wide = true;
                    break 'all;
                }
                let (x, y) = (self.alts[i as usize], self.alts[j as usize]);
                match self.meet_alt(x, y, why) {
                    Ok(alt) if Some(alt.family) == skip && !alt.wild => {}
                    Ok(alt) => {
                        self.alts.push(Alt { pa: i, pb: j, ..alt });
                        if first {
                            break 'all;
                        }
                    }
                    Err(Miss::Wide) => self.wide = true,
                    Err(Miss::Disjoint) => {}
                }
            }
        }
        List { at, n: self.alts.len() as u32 - at }
    }

    /// What a sink pad may take given its typed `Through` src pad's demand.
    fn through_back(&mut self, own: List, out: List, why: Why) -> List {
        let at = self.alts.len() as u32;
        for i in own.at..own.at + own.n {
            let a = self.alts[i as usize];
            let free = (out.at..out.at + out.n).any(|j| {
                let d = self.alts[j as usize];
                !a.wild && (d.wild || d.family != a.family)
            });
            for j in (out.at..out.at + out.n).take(if free { 0 } else { out.n as usize }) {
                let d = self.alts[j as usize];
                if self.alts.len() as u32 - at == MAX_PROBED_OFFERS as u32 {
                    self.wide = true;
                    break;
                }
                match self.meet_alt(a, d, why) {
                    Ok(x) => self.alts.push(Alt { pa: i, pb: j, ..x }),
                    Err(Miss::Wide) => self.wide = true,
                    Err(Miss::Disjoint) => {}
                }
            }
            if free && (self.alts.len() as u32 - at) < MAX_PROBED_OFFERS as u32 {
                self.alts.push(a);
            }
        }
        List { at, n: self.alts.len() as u32 - at }
    }

    /// A `Through` src pad given its input's format.
    fn through_fwd(&mut self, pin: List, fwd: List, why: Why) -> List {
        let p = self.alts[pin.at as usize];
        let at = self.alts.len() as u32;
        for j in fwd.at..fwd.at + fwd.n {
            let d = self.alts[j as usize];
            if !p.wild && !d.wild && d.family != p.family {
                self.alts.push(d);
                continue;
            }
            // In the pad's own field order, which keeps the next meets aligned.
            let met = match self.trace {
                true => self.meet_alt(p, d, why),
                false => self.meet_alt(d, p, why),
            };
            match met {
                Ok(x) => self.alts.push(x),
                Err(Miss::Wide) => self.wide = true,
                Err(Miss::Disjoint) => {}
            }
        }
        List { at, n: self.alts.len() as u32 - at }
    }

    fn concat(&mut self, a: List, b: List) -> List {
        if a.n + b.n > MAX_PROBED_OFFERS as u32 {
            self.wide = true;
        }
        if a.at + a.n == b.at {
            return List { at: a.at, n: (a.n + b.n).min(MAX_PROBED_OFFERS as u32) };
        }
        let at = self.alts.len() as u32;
        for i in (a.at..a.at + a.n).chain(b.at..b.at + b.n).take(MAX_PROBED_OFFERS) {
            self.alts.push(self.alts[i as usize]);
        }
        List { at, n: self.alts.len() as u32 - at }
    }

    fn narrow_field(&mut self, list: List, field: FieldId, c: DomC, why: Why) -> List {
        self.narrow_dom(list, Dom { field, c, pref: NONE, from: NONE }, why)
    }

    fn narrow_dom(&mut self, list: List, by: Dom, why: Why) -> List {
        let (field, at) = (by.field, self.alts.len() as u32);
        for i in list.at..list.at + list.n {
            let alt = self.alts[i as usize];
            let Some(d) = self.dom(alt, field) else {
                self.alts.push(alt);
                continue;
            };
            let Some((met, kept)) = self.meet_dom_kept(d, by, why) else {
                continue;
            };
            if kept {
                self.alts.push(alt);
                continue;
            }
            let (d, start) = (met, self.doms.len() as u32);
            for j in alt.at..alt.at + alt.n {
                let x = self.doms[j as usize];
                self.doms.push(if x.field == field { d } else { x });
            }
            self.alts.push(Alt { at: start, ..alt });
        }
        List { at, n: self.alts.len() as u32 - at }
    }

    fn narrow_doms(&mut self, list: List) -> List {
        if self.bys.is_empty() {
            return list;
        }
        let at = self.alts.len() as u32;
        'alts: for i in list.at..list.at + list.n {
            let mut alt = self.alts[i as usize];
            let mut copied = false;
            for b in 0..self.bys.len() {
                let (by, why) = self.bys[b];
                let Some(k) =
                    (0..alt.n).find(|&k| self.doms[(alt.at + k) as usize].field == by.field)
                else {
                    continue;
                };
                let Some((met, kept)) =
                    self.meet_dom_kept(self.doms[(alt.at + k) as usize], by, why)
                else {
                    if copied {
                        self.doms.truncate(alt.at as usize);
                    }
                    continue 'alts;
                };
                if kept {
                    continue;
                }
                if !copied {
                    let start = self.doms.len() as u32;
                    self.doms.extend_from_within(alt.at as usize..(alt.at + alt.n) as usize);
                    (alt.at, copied) = (start, true);
                }
                self.doms[(alt.at + k) as usize] = met;
            }
            self.alts.push(alt);
        }
        self.bys.clear();
        List { at, n: self.alts.len() as u32 - at }
    }

    /// `list` met with a filter's fields, which an alternative lacking them gains.
    fn narrow_all(&mut self, list: List, filter: List, why: Why) -> List {
        let f = self.alts[filter.at as usize];
        let at = self.alts.len() as u32;
        for i in list.at..list.at + list.n {
            match self.meet_alt(self.alts[i as usize], f, why) {
                Ok(x) => self.alts.push(x),
                Err(Miss::Wide) => self.wide = true,
                Err(Miss::Disjoint) => {}
            }
        }
        List { at, n: self.alts.len() as u32 - at }
    }

    /// The one domain every alternative of `list` gives `field`, `None` when they differ.
    #[inline]
    fn agree(&self, list: List, field: FieldId) -> (Option<Dom>, u32) {
        let Some(first) =
            (list.n > 0).then(|| self.dom(self.alts[list.at as usize], field)).flatten()
        else {
            return (None, NONE);
        };
        let (pref, mut last) = (first.pref, first);
        for i in list.at + 1..list.at + list.n {
            match self.dom(self.alts[i as usize], field) {
                Some(d) if self.same_c(first.c, d.c) => last = d,
                _ => return (None, pref),
            }
        }
        (Some(Dom { pref, ..last }), pref)
    }

    /// Kahn's algorithm over the elements in scope.
    fn sorted(&mut self) -> bool {
        self.order.clear();
        self.stack.clear();
        self.elems.iter_mut().for_each(|e| e.indeg = 0);
        for e in self.edges.iter().filter(|e| !e.probe) {
            self.elems[self.slots[e.sink as usize].elem as usize].indeg += 1;
        }
        let ready =
            (0..self.elems.len() as u32).rev().filter(|&i| self.elems[i as usize].indeg == 0);
        self.stack.extend(ready);
        while let Some(u) = self.stack.pop() {
            self.order.push(u);
            let mut s = self.elems[u as usize].first;
            while s != NONE {
                let Slot { src, edges, next: after, .. } = self.slots[s as usize];
                let mut e = if src { edges } else { NONE };
                while e != NONE {
                    let edge = &self.edges[e as usize];
                    let (v, next) = (self.slots[edge.sink as usize].elem as usize, edge.next_src);
                    if !edge.probe {
                        self.elems[v].indeg -= 1;
                        if self.elems[v].indeg == 0 {
                            self.stack.push(v as u32);
                        }
                    }
                    e = next;
                }
                s = after;
            }
        }
        self.order.len() == self.elems.len()
    }

    fn fail(&mut self, edge: u32, kind: FailKind, lists: [List; 2]) -> Fail {
        let (field, from) = match kind {
            FailKind::Empty => self.explain(lists),
            _ => (None, [NONE; 2]),
        };
        Fail { edge, kind, field, from, lists }
    }

    fn explain(&mut self, [a, b]: [List; 2]) -> (Option<FieldId>, [u32; 2]) {
        for i in a.at..a.at + a.n {
            for j in b.at..b.at + b.n {
                let (x, y) = (self.alts[i as usize], self.alts[j as usize]);
                if (x.wild && x.n == 0)
                    || (y.wild && y.n == 0)
                    || (!x.wild && !y.wild && x.family != y.family)
                {
                    continue;
                }
                for k in x.at..x.at + x.n {
                    let dx = self.doms[k as usize];
                    if let Some(dy) =
                        self.dom(y, dx.field).filter(|dy| self.meet(dx.c, dy.c) == DomC::Empty)
                    {
                        return (Some(dx.field), [dx.from, dy.from]);
                    }
                }
            }
        }
        (None, [NONE; 2])
    }

    /// Solve every edge in scope. On success every solved edge holds its format.
    pub(crate) fn solve(&mut self) -> Result<(), Fail> {
        if !self.sorted() {
            let lists = [List::default(); 2];
            return Err(Fail {
                edge: 0,
                kind: FailKind::Cycle,
                field: None,
                from: [NONE; 2],
                lists,
            });
        }
        for o in (0..self.order.len()).rev() {
            self.demand(self.order[o])?;
        }
        for s in &mut self.slots {
            s.fwd = s.own;
        }
        for o in 0..self.order.len() {
            let mut s = self.elems[self.order[o] as usize].first;
            while s != NONE {
                let Slot { src, edges, next, .. } = self.slots[s as usize];
                let mut e = if src { edges } else { NONE };
                while e != NONE {
                    self.fix(e)?;
                    e = self.edges[e as usize].next_src;
                }
                s = next;
            }
        }
        self.verify()
    }

    fn demand(&mut self, elem: u32) -> Result<(), Fail> {
        let mut s = self.elems[elem as usize].first;
        while s != NONE {
            let Slot { src, next: after, .. } = self.slots[s as usize];
            if src {
                self.demand_src(s)?;
            }
            s = after;
        }
        let mut k = self.elems[elem as usize].first;
        while k != NONE {
            let Slot { src, edges, next: after, own, .. } = self.slots[k as usize];
            let mut list = own;
            self.wide = false;
            let mut e = if src { NONE } else { edges };
            while e != NONE {
                let edge = &self.edges[e as usize];
                let (pin, link, next) =
                    (edge.pin.filter(|_| !edge.probe), edge.link.0, edge.next_sink);
                if let Some(pin) = pin {
                    list = self.meet_lists(list, pin, Why { by: By::Pinned, slot: k, link });
                }
                e = next;
            }
            let (at, end) = if src { (0, 0) } else { self.elems[elem as usize].rules };
            self.bys.clear();
            for r in at..end {
                let rule = self.rules[r as usize];
                if rule.sink != k {
                    continue;
                }
                let why = |by| Why { by, slot: k, link: NONE };
                let from = self.slots[rule.src as usize].dem;
                if matches!(rule.kind, Kind::Through) {
                    list = self.narrow_doms(list);
                }
                list = match rule.kind {
                    Kind::Through if self.slots[rule.src as usize].wild => {
                        self.meet_lists(list, from, why(By::Through))
                    }
                    Kind::Through => self.through_back(list, from, why(By::Through)),
                    // What downstream prefers goes back with what it takes, alone where
                    // alternatives disagree.
                    Kind::Same { to, from: f, by } => {
                        match self.agree(from, to) {
                            (Some(d), _) => self.bys.push((Dom { field: f, ..d }, why(by))),
                            (None, NONE) => {}
                            (None, pref) => self
                                .bys
                                .push((Dom { field: f, c: DomC::Any, pref, from: NONE }, why(by))),
                        }
                        list
                    }
                };
            }
            list = self.narrow_doms(list);
            if !src {
                (self.slots[k as usize].dem, self.slots[k as usize].wide) = (list, self.wide);
            }
            k = after;
        }
        Ok(())
    }

    /// The demand of src pad `s`. A pad puts out one format at a time, so its typed edges meet into
    /// one list.
    fn demand_src(&mut self, s: u32) -> Result<(), Fail> {
        let Slot { edges, own, coupled, pick, .. } = self.slots[s as usize];
        let (mut typed, mut e) = (0, edges);
        while e != NONE {
            let edge = self.edges[e as usize];
            if !edge.probe {
                self.wide = false;
                let peer = self.peer(e);
                let wide = self.wide || (edge.pin.is_none() && self.slots[edge.sink as usize].wide);
                let skip = self.escape(e, peer);
                let free = edge.pin.is_some() || self.untyped(peer);
                let slot = &mut self.edges[e as usize];
                (slot.peer, slot.skip, slot.free, slot.wide) = (peer, skip, free, wide);
                typed += !free as u32;
            }
            e = edge.next_src;
        }
        let joint = pick.is_some() || typed > 1;
        let (mut list, mut any, mut tainted) = (pick.unwrap_or(own), None, false);
        let mut e = edges;
        self.wide = false;
        while e != NONE {
            let (at, edge) = (e, self.edges[e as usize]);
            e = edge.next_src;
            if edge.probe || (joint && edge.pin.is_some()) {
                continue;
            }
            let by = if edge.pin.is_some() { By::Pinned } else { By::Peer };
            let (binds, why) = (joint && !edge.free, Why { by, slot: s, link: edge.link.0 });
            let base = if binds { list } else { own };
            let met = self.pairs(base, edge.peer, why, edge.skip, !coupled && !binds);
            if met.n == 0 {
                let kind = if self.wide || edge.wide { FailKind::Wide } else { FailKind::Empty };
                return Err(self.fail(at, kind, [base, edge.peer]));
            }
            match binds {
                true => (list, tainted) = (met, tainted || edge.wide),
                false => any = Some(any.map_or(met, |l| self.concat(l, met))),
            }
        }
        let wide = tainted || (joint && self.wide);
        let slot = &mut self.slots[s as usize];
        (slot.dem, slot.joint, slot.wide) =
            (if joint { list } else { any.unwrap_or(own) }, joint, wide);
        Ok(())
    }

    fn untyped(&self, list: List) -> bool {
        let bytes = |i: u32| {
            !self.alts[i as usize].wild && Some(self.alts[i as usize].family) == self.bytes_family
        };
        list.n > 0 && (list.at..list.at + list.n).all(bytes)
    }

    fn peer(&mut self, e: u32) -> List {
        let edge = &self.edges[e as usize];
        let (pin, filter, sink, link) = (edge.pin, edge.filter, edge.sink, edge.link.0);
        if let Some(pin) = pin {
            return pin;
        }
        let dem = self.slots[sink as usize].dem;
        match filter {
            Some(f) => self.narrow_all(dem, f, Why { by: By::Filter, slot: sink, link }),
            None => dem,
        }
    }

    /// The `bytes` family when edge `e` may not carry it, an escape that would only hide a refusal.
    fn escape(&self, e: u32, peer: List) -> Option<FormatId> {
        let (bytes, edge) = (self.bytes_family?, &self.edges[e as usize]);
        if edge.pin.is_some() {
            return None;
        }
        let (own, sink) = (self.slots[edge.src as usize].own, self.slots[edge.sink as usize].own);
        let names = |l: List, f: FormatId| {
            (l.at..l.at + l.n)
                .any(|j| !self.alts[j as usize].wild && self.alts[j as usize].family == f)
        };
        let mut shared = false;
        for i in own.at..own.at + own.n {
            let a = self.alts[i as usize];
            match (a.wild, a.family == bytes) {
                (true, _) => {}
                (false, true) => return shared.then_some(bytes),
                (false, false) => shared = shared || names(sink, a.family) || names(peer, a.family),
            }
        }
        None
    }

    fn fix(&mut self, e: u32) -> Result<(), Fail> {
        let edge = &self.edges[e as usize];
        let (probe, pin, src_slot, sink_slot, link) =
            (edge.probe, edge.pin, edge.src, edge.sink, edge.link.0);
        if probe {
            return Ok(());
        }
        if pin.is_none() {
            let Slot { own, fwd, dem, pick, joint, out_edges, wide, .. } =
                self.slots[src_slot as usize];
            let edge = self.edges[e as usize];
            // A typed edge of a joint pad carries its pick, so every typed edge has one format.
            let binds = joint && !edge.free;
            let (src, sink, skip) = match (binds, pick) {
                (true, Some(p)) => (p, edge.peer, edge.skip),
                (true, None) => (fwd, dem, None),
                (false, _) => (fwd, edge.peer, edge.skip),
            };
            let lone = out_edges == 1 && !joint;
            let met = (src == own && dem.n > 0 && lone).then(|| self.alts[dem.at as usize]);
            let (sink, skip) = if met.is_some() { (List::default(), None) } else { (sink, skip) };
            let tainted = wide || edge.wide;
            self.wide = false;
            let why = Why { by: By::Peer, slot: src_slot, link };
            let mut found = met.map(|x| (self.alts[x.pa as usize], self.alts[x.pb as usize], x));
            'pairs: for i in (src.at..src.at + src.n).filter(|_| found.is_none()) {
                for j in sink.at..sink.at + sink.n {
                    let (a, b) = (self.alts[i as usize], self.alts[j as usize]);
                    match self.meet_alt(a, b, why) {
                        Ok(x) if Some(x.family) == skip && !x.wild => {}
                        Ok(x) => {
                            found = Some((a, b, x));
                            break 'pairs;
                        }
                        Err(Miss::Wide) => self.wide = true,
                        Err(Miss::Disjoint) => {}
                    }
                }
            }
            let Some((a, b, x)) = found else {
                let kind = if self.wide || tainted { FailKind::Wide } else { FailKind::Empty };
                return Err(self.fail(e, kind, [src, sink]));
            };
            self.outs[e as usize].reset(x.family);
            let mut why = [Reason::default(); MAX_OFFER_FIELDS];
            for k in (0..x.n).filter(|_| !x.wild) {
                let d = self.doms[(x.at + k) as usize];
                let pref = |alt: Alt| {
                    let at = match self
                        .doms
                        .get((alt.at + k) as usize)
                        .filter(|o| k < alt.n && o.field == d.field)
                    {
                        Some(o) => o.pref,
                        None => self.dom(alt, d.field).map_or(NONE, |d| d.pref),
                    };
                    self.vals.get(at as usize).copied()
                };
                if let Some((v, r)) = self.choose(d.c, pref(a), pref(b)) {
                    let out = &mut self.outs[e as usize];
                    why[out.len()] = r;
                    out.push_new(d.field, v);
                }
            }
            let (open, risky) = self.openness(e, a);
            let slot = &mut self.edges[e as usize];
            (slot.why, slot.open, slot.risky) = (why, open, risky);
            if binds && pick.is_none() {
                self.slots[src_slot as usize].pick = Some(self.pinned_out(e));
                let mut o = self.edges[e as usize].opens;
                while let Some(x) = self.opens.get(o as usize).copied() {
                    self.open_at(src_slot, false, x.field, x.origin);
                    o = x.next;
                }
            }
        } else {
            for i in 0..self.outs[e as usize].len() {
                if self.edges[e as usize].open >> i & 1 == 1 {
                    let field = self.outs[e as usize].fields()[i].0;
                    self.open_at(e, true, field, DomC::Any);
                }
            }
        }
        let elem = self.slots[sink_slot as usize].elem;
        let (at, end) = self.elems[elem as usize].rules;
        // `Same` rules into one src pad narrow it in one pass, before anything reads it.
        let mut pending = NONE;
        self.bys.clear();
        for r in at..end {
            let rule = self.rules[r as usize];
            if rule.sink != sink_slot {
                continue;
            }
            if pending != NONE && (pending != rule.src || matches!(rule.kind, Kind::Through)) {
                self.slots[pending as usize].fwd =
                    self.narrow_doms(self.slots[pending as usize].fwd);
            }
            (pending, self.wide) = (NONE, false);
            let fwd = self.slots[rule.src as usize].fwd;
            let why = |by| Why { by, slot: rule.src, link };
            let fwd = match rule.kind {
                Kind::Through => {
                    let mut o = self.edges[e as usize].opens;
                    while let Some(x) = self.opens.get(o as usize).copied() {
                        self.open_at(rule.src, false, x.field, x.origin);
                        o = x.next;
                    }
                    let pin = self.pinned_out(e);
                    match self.slots[rule.src as usize].wild {
                        true => self.meet_lists(pin, fwd, why(By::Through)),
                        false => self.through_fwd(pin, fwd, why(By::Through)),
                    }
                }
                Kind::Same { to, from, by } => {
                    if let Some(v) = self.outs[e as usize].get(from) {
                        if let Some(origin) = self.open_of(e, true, from) {
                            self.open_at(rule.src, false, to, origin);
                        }
                        self.bys.push((
                            Dom { field: to, c: DomC::Eq(v), pref: NONE, from: NONE },
                            why(by),
                        ));
                    }
                    pending = rule.src;
                    fwd
                }
            };
            self.slots[rule.src as usize].fwd = fwd;
            self.slots[rule.src as usize].wide |= self.wide;
        }
        if pending != NONE {
            self.slots[pending as usize].fwd = self.narrow_doms(self.slots[pending as usize].fwd);
        }
        Ok(())
    }

    /// Bits of the fields of `out` on edge `e` no upstream element states, and those the sink does
    /// not take at every value.
    fn openness(&mut self, e: u32, a: Alt) -> (u16, u16) {
        let (src, sink) = (self.edges[e as usize].src, self.edges[e as usize].sink);
        let fed = self.elems[self.slots[src as usize].elem as usize].fed;
        let (mut open, mut risky) = (0u16, 0u16);
        let family = self.outs[e as usize].family;
        for i in 0..self.outs[e as usize].len() {
            let field = self.outs[e as usize].fields()[i].0;
            let origin = match self.open_of(src, false, field) {
                Some(DomC::Empty) => continue,
                Some(origin) => origin,
                None => match self.dom(a, field).map(|d| d.c) {
                    None | Some(DomC::Any) => DomC::Any,
                    Some(DomC::Eq(_) | DomC::Empty) => continue,
                    Some(c) if fed => c,
                    Some(_) => continue,
                },
            };
            let origin = match self.picks.contains(&(src, field)) {
                true => self.held_to_demand(e, field, origin),
                false => origin,
            };
            open |= 1 << i;
            self.open_at(e, true, field, origin);
            if !self.covers(self.slots[sink as usize].own, family, field, origin) {
                risky |= 1 << i;
            }
        }
        (open, risky)
    }

    fn covers(&self, list: List, family: FormatId, field: FieldId, origin: DomC) -> bool {
        let step = |v: Value| match v {
            Value::Int(s) if s > 1 => s,
            _ => 1,
        };
        (list.at..list.at + list.n).any(|i| {
            let alt = self.alts[i as usize];
            let c = match self.dom(alt, field) {
                _ if alt.wild => return true,
                _ if alt.family != family => return false,
                None => return true,
                Some(d) => d.c,
            };
            match (c, origin) {
                (DomC::Any, _) => true,
                (_, DomC::Any | DomC::Empty) => false,
                (_, DomC::Eq(v)) => self.accepts(c, v),
                (_, DomC::Set(at, n)) => {
                    (at..at + n).all(|k| self.accepts(c, self.vals[k as usize]))
                }
                (DomC::Range(b), DomC::Range(at)) => {
                    let ((lo, hi, s), (_, _, t)) = (self.bounds(at), self.bounds(b));
                    self.accepts(c, lo) && self.accepts(c, hi) && step(s) % step(t) == 0
                }
                _ => false,
            }
        })
    }

    fn admits(&self, list: List, f: &FixedFormat) -> bool {
        (list.at..list.at + list.n).any(|i| {
            let alt = self.alts[i as usize];
            let fits = |d: &Dom| f.get(d.field).is_none_or(|v| self.accepts(d.c, v));
            let doms = &self.doms[alt.at as usize..(alt.at + alt.n) as usize];
            (alt.wild && alt.n == 0)
                || (!alt.wild && alt.family == f.family && doms.iter().all(fits))
        })
    }

    fn verify(&self) -> Result<(), Fail> {
        for (i, e) in self.edges.iter().enumerate().filter(|(_, e)| e.solved()) {
            let (ends, out) =
                ([self.slots[e.src as usize].own, self.slots[e.sink as usize].own], &self.outs[i]);
            let bare = Some(out.family) == self.wild_family && out.is_empty();
            let filtered = bare
                || e.filter.is_none_or(|f| {
                    let f = self.alts[f.at as usize];
                    let doms = &self.doms[f.at as usize..(f.at + f.n) as usize];
                    doms.iter().all(|d| {
                        out.get(d.field).map_or(d.c == DomC::Any, |v| self.accepts(d.c, v))
                    })
                });
            if !filtered || !ends.iter().all(|l| self.admits(*l, out)) {
                let lists = [List::default(); 2];
                return Err(Fail {
                    edge: i as u32,
                    kind: FailKind::Inconsistent,
                    field: None,
                    from: [NONE; 2],
                    lists,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn out(&self, e: usize) -> &FixedFormat {
        &self.outs[e]
    }

    /// The values of `field` downstream of edge `e` takes, into `out`. `false` where it takes any.
    pub(crate) fn takes(&self, e: usize, field: FieldId, out: &mut Vec<Value>) -> bool {
        out.clear();
        let (Some(edge), Some(family)) = (self.edges.get(e), self.outs.get(e).map(|f| f.family))
        else {
            return false;
        };
        if edge.free {
            return false;
        }
        for i in edge.peer.at..edge.peer.at + edge.peer.n {
            let alt = self.alts[i as usize];
            if !alt.wild && alt.family != family {
                continue;
            }
            let c = if alt.wild { None } else { self.dom(alt, field).map(|d| d.c) };
            match c {
                Some(DomC::Eq(v)) => {
                    if !out.contains(&v) {
                        out.push(v);
                    }
                }
                Some(DomC::Set(at, n)) => {
                    for &v in &self.vals[at as usize..(at + n) as usize] {
                        if !out.contains(&v) {
                            out.push(v);
                        }
                    }
                }
                Some(DomC::Empty) => {}
                None | Some(DomC::Any | DomC::Range(_)) => {
                    out.clear();
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn narrowed(&self, slot: u32) -> List {
        let s = &self.slots[slot as usize];
        if s.src { s.pick.unwrap_or(s.fwd) } else { s.dem }
    }

    pub(crate) fn carried(&mut self, slot: u32) -> Option<List> {
        let s = self.slots[slot as usize];
        if s.pick.is_some() || !s.src {
            return s.pick;
        }
        let mut e = s.edges;
        while let Some(edge) = self.edges.get(e as usize).copied() {
            if !edge.probe && !edge.free {
                return Some(self.pinned_out(e));
            }
            e = edge.next_src;
        }
        None
    }

    // Rendering for an error, never on success.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn render_c(&self, c: DomC, values: &Interner) -> String {
        let v = |v: Value| match v {
            Value::Int(i) => i.to_string(),
            Value::Rat(n, d) => format!("{n}/{d}"),
            Value::Id(id) => values.resolve(id.0).unwrap_or("?").to_string(),
        };
        match c {
            DomC::Any => "any".into(),
            DomC::Empty => "nothing".into(),
            DomC::Eq(x) => v(x),
            DomC::Range(at) => {
                let (min, max, step) = self.bounds(at);
                format!("[{}, {}] step {}", v(min), v(max), v(step))
            }
            DomC::Set(at, n) => {
                let items: Vec<String> =
                    self.vals[at as usize..(at + n) as usize].iter().map(|x| v(*x)).collect();
                format!("{{{}}}", items.join(", "))
            }
        }
    }

    // Rendering for an error, never on success.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn render(&self, list: List, [formats, fields, values]: [&Interner; 3]) -> String {
        let alt = |i: u32| {
            let a = self.alts[i as usize];
            let mut out = String::from(formats.resolve(a.family.0).unwrap_or("?"));
            for d in &self.doms[a.at as usize..(a.at + a.n) as usize] {
                let name = fields.resolve(d.field.0).unwrap_or("?");
                out.push_str(&format!(", {name}={}", self.render_c(d.c, values)));
            }
            out
        };
        (list.at..list.at + list.n).map(alt).collect::<Vec<_>>().join(" | ")
    }
}

enum Lattice {
    Empty,
    One(i64),
    Two(i64, i64),
    Range(i64, i64, i64),
}

/// Two stepped integer ranges met on both lattices, Chinese remainder over the gcd.
fn lattice((a0, a1, sa): (i128, i128, i128), (b0, b1, sb): (i128, i128, i128)) -> Lattice {
    let (lo, hi) = (a0.max(b0), a1.min(b1));
    let (g, p) = gcd(sa, sb);
    let d = b0 - a0;
    if lo > hi || d % g != 0 {
        return Lattice::Empty;
    }
    let (m, l) = (sb / g, sa / g * sb);
    let t = ((p % m) * ((d / g) % m)).rem_euclid(m);
    let first = lo + (a0 + sa * t - lo).rem_euclid(l);
    if first > hi {
        return Lattice::Empty;
    }
    let last = first + (hi - first) / l * l;
    let int = |v: i128| i64::try_from(v).ok();
    match (int(first), int(last), int(l)) {
        (Some(f), _, _) if first == last => Lattice::One(f),
        (Some(min), Some(max), Some(step)) => Lattice::Range(min, max, step),
        // A step past i64 fits once in an i64 span, the meet is its two ends.
        (Some(min), Some(max), None) => Lattice::Two(min, max),
        _ => Lattice::Empty,
    }
}

fn gcd(a: i128, b: i128) -> (i128, i128) {
    let (mut r0, mut r1, mut p0, mut p1) = (a, b, 1i128, 0i128);
    while r1 != 0 {
        let q = r0 / r1;
        (r0, r1) = (r1, r0 - q * r1);
        (p0, p1) = (p1, p0 - q * p1);
    }
    (r0, p0)
}

#[cfg(test)]
// Test fixtures leak descriptors and allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::couple::Coupling;
    use crate::ctx::Ctx;
    use crate::element::{
        Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
    };
    use crate::error::Error;
    use crate::format::{ConstraintDesc, FieldDesc, OfferDesc, ValueDesc};
    use crate::id::{ElementId, SinkPad, SrcPad, ValueId};
    use crate::pipeline::Pipeline;

    const fn i(v: i64) -> Value {
        Value::Int(v)
    }

    fn set(s: &mut Solver, vs: &[i64]) -> DomC {
        let at = s.vals.len() as u32;
        s.vals.extend(vs.iter().map(|v| i(*v)));
        s.set(at)
    }

    #[derive(Clone, Debug, PartialEq)]
    enum D {
        Any,
        Eq(Value),
        R(Value, Value, Value),
        S(Vec<Value>),
        Empty,
    }

    fn r(min: i64, max: i64, step: i64) -> D {
        D::R(i(min), i(max), i(step))
    }

    fn put(s: &mut Solver, d: &D) -> DomC {
        match d {
            D::Any => DomC::Any,
            D::Eq(v) => DomC::Eq(*v),
            D::R(min, max, step) => s.range(*min, *max, *step),
            D::S(vs) => {
                let at = s.vals.len() as u32;
                s.vals.extend_from_slice(vs);
                s.set(at)
            }
            D::Empty => DomC::Empty,
        }
    }

    fn view(s: &Solver, c: DomC) -> D {
        match c {
            DomC::Any => D::Any,
            DomC::Eq(v) => D::Eq(v),
            DomC::Range(at) => {
                let (min, max, step) = s.bounds(at);
                D::R(min, max, step)
            }
            DomC::Set(at, n) => D::S(s.vals[at as usize..(at + n) as usize].to_vec()),
            DomC::Empty => D::Empty,
        }
    }

    fn met(s: &mut Solver, a: &D, b: &D) -> D {
        let (a, b) = (put(s, a), put(s, b));
        let c = s.meet(a, b);
        view(s, c)
    }

    fn range(s: &mut Solver, min: i64, max: i64, step: i64) -> DomC {
        s.range(i(min), i(max), i(step))
    }

    #[test]
    fn meet_keeps_the_first_sides_order_and_aligns_stepped_ranges() {
        let mut s = Solver::default();
        let ints = |vs: &[i64]| D::S(vs.iter().map(|v| i(*v)).collect());
        assert_eq!(met(&mut s, &ints(&[3, 1, 2]), &ints(&[2, 3])), ints(&[3, 2]), "a's order");
        assert_eq!(met(&mut s, &ints(&[2, 3]), &ints(&[3, 1, 2])), ints(&[2, 3]), "b's order");
        assert_eq!(
            met(&mut s, &r(15, 40, 1), &ints(&[30, 10, 20])),
            ints(&[30, 20]),
            "a set keeps its order against a range"
        );
        let rat = |a, b| D::R(Value::Rat(a, 2), Value::Rat(b, 2), i(0));
        let ids = |a, b| D::R(Value::Id(ValueId(a)), Value::Id(ValueId(b)), i(1));
        let rows: Vec<(D, D, D)> = vec![
            (D::Any, D::Eq(i(7)), D::Eq(i(7))),
            (D::Eq(i(7)), D::Any, D::Eq(i(7))),
            (D::Eq(i(7)), D::Eq(i(8)), D::Empty),
            (D::Eq(i(32)), r(0, 100, 16), D::Eq(i(32))),
            (r(0, 100, 16), D::Eq(i(33)), D::Empty),
            (r(0, 100, 16), r(8, 200, 24), r(32, 80, 48)),
            (r(1, 100, 2), r(0, 100, 2), D::Empty),
            (r(0, 10, 0), r(5, 20, 1), r(5, 10, 1)),
            (r(0, 10, 5), r(10, 20, 3), D::Eq(i(10))),
            (r(0, 10, 1), r(11, 20, 1), D::Empty),
            (r(0, i64::MAX, i64::MAX), r(1, i64::MAX, i64::MAX - 1), D::Eq(i(i64::MAX))),
            (r(i64::MIN, i64::MAX, 3), r(i64::MIN, i64::MAX, 7), r(i64::MIN, i64::MAX - 15, 21)),
            // The lcm 3 * 2^62 is past i64, the two common values come back as a set.
            (r(i64::MIN, i64::MAX, 1 << 62), r(i64::MIN, i64::MAX, 3), ints(&[i64::MIN, 1 << 62])),
            (
                r(i64::MIN, i64::MAX, 3),
                r(i64::MIN + 1, i64::MAX, 7),
                r(i64::MIN + 15, i64::MAX, 21),
            ),
            (r(0, 10, 1), D::R(Value::Rat(0, 1), Value::Rat(10, 1), i(1)), D::Empty),
            (D::Eq(i(1)), D::Eq(Value::Id(ValueId(1))), D::Empty),
            (D::Empty, D::Any, D::Empty),
            (rat(1, 3), rat(2, 4), D::R(Value::Rat(2, 2), Value::Rat(3, 2), i(0))),
            (rat(1, 2), rat(2, 4), D::Eq(Value::Rat(2, 2))),
            (rat(1, 2), rat(3, 4), D::Empty),
            (ids(1, 5), ids(2, 6), D::Empty),
            (ints(&[5, 6]), r(6, 9, 1), D::Eq(i(6))),
        ];
        for (n, (a, b, want)) in rows.iter().enumerate() {
            assert_eq!(met(&mut s, a, b), *want, "row {n}: {a:?} with {b:?}");
        }
    }

    #[test]
    fn every_met_value_is_on_both_lattices() {
        let mut s = Solver::default();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut draw = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n) as i64
        };
        for _ in 0..20_000 {
            let (a0, a1, sa) = (draw(60), 40 + draw(80), draw(12) - 1);
            let (b0, b1, sb) = (draw(60), 40 + draw(80), draw(12) - 1);
            let (a, b) = (range(&mut s, a0, a1, sa), range(&mut s, b0, b1, sb));
            let met = s.meet(a, b);
            for v in -5..130 {
                let both = s.accepts(a, i(v)) && s.accepts(b, i(v));
                assert_eq!(s.accepts(met, i(v)), both, "{a:?} with {b:?} at {v} gave {met:?}");
            }
        }
    }

    #[test]
    fn choose_takes_a_preference_it_admits_then_the_domains_own() {
        let mut s = Solver::default();
        let a = set(&mut s, &[3, 1]);
        let sixteens = range(&mut s, 16, 64, 16);
        type Row = (DomC, Option<Value>, Option<Value>, Option<(Value, Reason)>);
        let rows: &[Row] = &[
            (a, Some(i(1)), None, Some((i(1), Reason::SrcPreferred))),
            (a, Some(i(9)), Some(i(1)), Some((i(1), Reason::SinkPreferred))),
            (a, Some(i(1)), Some(i(3)), Some((i(1), Reason::SrcPreferred))),
            (a, Some(i(9)), Some(i(8)), Some((i(3), Reason::First))),
            (sixteens, None, None, Some((i(16), Reason::Min))),
            (sixteens, Some(i(17)), None, Some((i(16), Reason::Min))),
            (DomC::Eq(i(7)), None, Some(i(7)), Some((i(7), Reason::SinkPreferred))),
            (DomC::Eq(i(7)), None, None, Some((i(7), Reason::Only))),
            (DomC::Any, Some(i(7)), None, None),
        ];
        for (n, (c, src, sink, want)) in rows.iter().enumerate() {
            assert_eq!(s.choose(*c, *src, *sink), *want, "row {n}");
        }
    }

    struct Live {
        s: Solver,
        names: [Interner; 3],
    }

    impl Live {
        fn new(elements: usize) -> Live {
            let mut names = [Interner::new(), Interner::new(), Interner::new()];
            let wild = FormatId(names[0].intern(crate::format::WILDCARD));
            let mut s = Solver { trace: true, ..Solver::default() };
            s.begin(elements, [Some(wild), None]);
            Live { s, names }
        }

        fn pad(&mut self, (el, src, pad): (u32, bool, u32), rates: Option<&[i64]>) -> u32 {
            use crate::format::{ConstraintSpec, FieldSpec, OfferSpec, ValueSpec};
            let set: Vec<ValueSpec> =
                rates.unwrap_or(&[]).iter().map(|r| ValueSpec::Int(*r)).collect();
            let allowed =
                if set.is_empty() { ConstraintSpec::Any } else { ConstraintSpec::Set(&set) };
            let fields = [FieldSpec { field: "rate", allowed, preferred: None }];
            let offer = match rates {
                Some(_) => OfferSpec { family: "audio/raw", fields: &fields },
                None => OfferSpec { family: crate::format::WILDCARD, fields: &[] },
            };
            let [a, b, c] = &mut self.names;
            let offers = Interned::intern(&[offer], [a, b, c]);
            self.s.slot(el, (src, pad), NONE, &offers)
        }

        fn running(&mut self, rate: i64) -> FixedFormat {
            let [formats, fields, _] = &mut self.names;
            let mut f = FixedFormat::new(FormatId(formats.intern("audio/raw")));
            f.set(FieldId(fields.intern("rate")), i(rate));
            f
        }

        fn rate(&self, edge: usize) -> Option<Value> {
            self.s.out(edge).get(FieldId(self.names[1].get("rate")?))
        }

        fn rules(&mut self, through: &[(usize, u32, u32)]) {
            for elem in 0..self.s.elem_count() {
                self.s.begin_rules(elem, false);
                let stated: Vec<_> = through.iter().filter(|t| t.0 == elem).collect();
                stated.iter().for_each(|&&(_, k, o)| self.s.through(elem, k, o));
                self.s.end_rules(elem, stated.is_empty());
            }
        }
    }

    #[test]
    fn a_branch_behind_a_running_tee_takes_the_pinned_format_and_the_pins_stay() {
        let mut g = Live::new(4);
        let (src, tee_in) =
            (g.pad((0, true, 0), Some(&[44_100, 48_000])), g.pad((1, false, 0), None));
        let (tee_a, tee_b) = (g.pad((1, true, 0), None), g.pad((1, true, 1), None));
        let (a, b) =
            (g.pad((2, false, 0), Some(&[48_000])), g.pad((3, false, 0), Some(&[44_100, 48_000])));
        let pinned = g.running(48_000);
        g.s.edge(LinkId(0), (src, tee_in), None, Some((&pinned, 0)), false);
        g.s.edge(LinkId(1), (tee_a, a), None, Some((&pinned, 0)), false);
        g.s.edge(LinkId(2), (tee_b, b), None, None, false);
        g.rules(&[]);
        g.s.solve().expect("solves");
        assert_eq!(g.rate(2), Some(i(48_000)), "alone b would take 44.1k");
        for e in 0..2 {
            assert!(!g.s.edges[e].solved() && *g.s.out(e) == pinned, "edge {e} is pinned and kept");
        }
        assert!(g.s.meets < 64, "{} meets", g.s.meets);
    }

    #[test]
    fn a_source_into_a_running_junction_meets_its_pin_back_through_the_rule_or_fails_by_it() {
        for (offer, want) in [(&[44_100, 48_000][..], Some(48_000)), (&[44_100][..], None)] {
            let mut g = Live::new(3);
            let dec = g.pad((0, true, 0), Some(offer));
            let (join_in, join_out) =
                (g.pad((1, false, 0), Some(&[])), g.pad((1, true, 0), Some(&[])));
            let sink = g.pad((2, false, 0), Some(&[44_100, 48_000]));
            let pinned = g.running(48_000);
            g.s.edge(LinkId(0), (dec, join_in), None, None, false);
            g.s.edge(LinkId(1), (join_out, sink), None, Some((&pinned, 0)), false);
            g.rules(&[(1, join_in, join_out)]);
            match (g.s.solve(), want) {
                (Ok(()), Some(rate)) => assert_eq!(g.rate(0), Some(i(rate))),
                (Err(fail), None) => {
                    assert_eq!((fail.edge, fail.kind), (0, FailKind::Empty));
                    let mut at = fail.from[1];
                    let mut by = Vec::new();
                    while let Some(step) = g.s.steps.get(at as usize) {
                        by.push(step.by);
                        at = step.prev;
                    }
                    assert!(by.contains(&By::Pinned) && by.contains(&By::Through), "{by:?}");
                }
                (got, want) => panic!("{offer:?}: {got:?}, wanted {want:?}"),
            }
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.below(100) < pct
        }
    }

    struct Node(&'static ElementDesc);

    impl Element for Node {
        fn desc(&self) -> &'static ElementDesc {
            self.0
        }
        fn process(
            &mut self,
            _ctx: &mut Ctx,
            _inputs: crate::batch::Inputs<'_>,
        ) -> Result<Flow, Error> {
            Ok(Flow::Eos)
        }
    }

    fn leak<T>(v: Vec<T>) -> &'static [T] {
        Box::leak(v.into_boxed_slice())
    }

    fn constraint(rng: &mut Rng) -> ConstraintDesc {
        match rng.below(4) {
            0 => ConstraintDesc::Any,
            1 => ConstraintDesc::Eq(ValueDesc::Int(1 + rng.below(6) as i64)),
            2 => {
                let mut ids: Vec<i64> = (1..=6).collect();
                for k in (1..ids.len()).rev() {
                    ids.swap(k, rng.below(k as u64 + 1) as usize);
                }
                ids.truncate(1 + rng.below(4) as usize);
                ConstraintDesc::Set(leak(ids.into_iter().map(ValueDesc::Int).collect()))
            }
            _ => {
                let lo = rng.below(4) as i64;
                let (min, max, step) = (
                    ValueDesc::Int(lo),
                    ValueDesc::Int(lo + 2 + rng.below(6) as i64),
                    ValueDesc::Int(rng.below(3) as i64),
                );
                ConstraintDesc::Range { min, max, step }
            }
        }
    }

    fn menu(rng: &mut Rng, wild: bool) -> &'static [OfferDesc] {
        if wild {
            return leak(vec![OfferDesc::wildcard()]);
        }
        let offers = (0..1 + rng.below(2)).map(|_| {
            let family = ["a", "b"][rng.chance(30) as usize];
            let mut fields =
                vec![FieldDesc { field: "id", allowed: constraint(rng), preferred: None }];
            if rng.chance(40) {
                let preferred = rng.chance(50).then(|| ValueDesc::Int(1 + rng.below(6) as i64));
                fields.push(FieldDesc { field: "w", allowed: constraint(rng), preferred });
            }
            OfferDesc { family, fields: leak(fields) }
        });
        leak(offers.collect())
    }

    fn element(
        rng: &mut Rng,
        name: &'static str,
        sinks: usize,
        srcs: usize,
        coupled: bool,
    ) -> &'static ElementDesc {
        let wild = sinks > 0 && srcs > 0 && rng.chance(20);
        let pads = |n: usize, names: [&'static str; 2], rng: &mut Rng| {
            leak((0..n).map(|k| PadDesc { name: names[k], offers: menu(rng, wild) }).collect())
        };
        let (sinks, srcs) =
            (pads(sinks, ["sink", "sink_1"], rng), pads(srcs, ["src", "src_1"], rng));
        let rules = match (coupled && !wild && !sinks.is_empty() && !srcs.is_empty(), rng.below(4))
        {
            (false, _) | (true, 0) => vec![],
            (true, 1) => vec![Coupling::through(0, 0)],
            (true, 2) => vec![Coupling::same(0, 0, "id"), Coupling::copy(0, 0, "w", "id")],
            (true, _) => vec![Coupling::fixed(0, "id", ValueDesc::Int(1 + rng.below(6) as i64))],
        };
        Box::leak(Box::new(ElementDesc {
            name,
            sinks,
            srcs,
            props: &[],
            sched: SchedHint::Active,
            inputs: if sinks.is_empty() { InputPolicy::None } else { InputPolicy::Single },
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: leak(rules),
        }))
    }

    type Made =
        Vec<(crate::id::LinkId, &'static ElementDesc, &'static ElementDesc, u32, u32, usize)>;

    fn run(seed: u64, coupled: bool) -> (Pipeline, Made, String) {
        let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
        let mut p = Pipeline::new();
        let n = 2 + rng.below(11) as usize;
        let mut descs = Vec::new();
        for k in 0..n {
            let (sinks, srcs) = match (k, k + 1 == n || rng.chance(25)) {
                (0, _) => (0, 1 + rng.below(2) as usize),
                (_, true) => (1, 0),
                _ => (1, 1 + rng.below(2) as usize),
            };
            let desc = element(&mut rng, "n", sinks, srcs, coupled);
            p.add(Node(desc));
            descs.push(desc);
        }
        let mut made = Vec::new();
        let mut log = String::new();
        for k in 1..n {
            let ups: Vec<usize> = (0..k).filter(|u| !descs[*u].srcs.is_empty()).collect();
            let up = ups[rng.below(ups.len() as u64) as usize];
            let pad = rng.below(descs[up].srcs.len() as u64) as u32;
            let (src, sink) =
                (ElementId(up as u32).src(SrcPad(pad)), ElementId(k as u32).sink(SinkPad(0)));
            match p.link(src, sink) {
                Ok(link) => {
                    log.push_str(&format!(
                        "{link:?}={:?};",
                        p.negotiated(link).map(|f| f.fields().to_vec())
                    ));
                    made.push((link, descs[up], descs[k], pad, 0, up));
                }
                Err(e) => log.push_str(&format!("refused {};", matches!(e, Error::Link(_)))),
            }
        }
        let solved = p.solve_check();
        log.push_str(&format!("{:?}", solved.as_ref().map_err(|e| e.to_string())));
        (p, made, log)
    }

    fn admitted(p: &Pipeline, f: &FixedFormat, menu: &[OfferDesc]) -> bool {
        let fits = |c: &ConstraintDesc, v: i64| match *c {
            ConstraintDesc::Any => true,
            ConstraintDesc::Eq(ValueDesc::Int(x)) => x == v,
            ConstraintDesc::Set(vs) => vs.iter().any(|x| matches!(x, ValueDesc::Int(x) if *x == v)),
            ConstraintDesc::Range {
                min: ValueDesc::Int(lo),
                max: ValueDesc::Int(hi),
                step: ValueDesc::Int(s),
            } => lo <= v && v <= hi && (s <= 0 || (v - lo) % s == 0),
            _ => false,
        };
        menu.iter().any(|o| {
            o.is_wildcard()
                || (p.family_name(f.family) == Some(o.family)
                    && o.fields.iter().all(|fd| {
                        match p.field_id(fd.field).and_then(|id| f.get(id)) {
                            None => true,
                            Some(Value::Int(v)) => fits(&fd.allowed, v),
                            Some(_) => false,
                        }
                    }))
        })
    }

    #[test]
    fn random_graphs_solve_deterministically_inside_every_offer() {
        let (mut solved, mut refused) = (0, 0);
        for seed in 0..3_000u64 {
            let coupled = seed % 4 != 0;
            let (mut p, made, log) = run(seed, coupled);
            assert_eq!(log, run(seed, coupled).2, "seed {seed} is a function of its input");
            refused += log.matches("refused").count();
            let fixed = p.step(std::sync::Arc::new(crate::clock::MockClock::new()));
            match fixed {
                Ok(_) => {}
                Err(Error::Graph(_)) => continue,
                Err(e) => panic!("seed {seed}: {log} then {e}"),
            }
            solved += 1;
            for (link, up, down, pad, k, _) in &made {
                let Some(f) = p.negotiated(*link) else {
                    continue;
                };
                assert!(
                    admitted(&p, f, up.srcs[*pad as usize].offers),
                    "seed {seed} link {link:?} src side"
                );
                assert!(
                    admitted(&p, f, down.sinks[*k as usize].offers),
                    "seed {seed} link {link:?} sink side"
                );
            }
            let pad_of = |m: &(LinkId, &ElementDesc, &ElementDesc, u32, u32, usize)| (m.5, m.3);
            for (a, b) in made
                .iter()
                .flat_map(|a| made.iter().map(move |b| (a, b)))
                .filter(|(a, b)| a.0.0 < b.0.0 && pad_of(a) == pad_of(b))
            {
                let fields = |l| p.negotiated(l).map(|f| (f.family, f.fields().to_vec()));
                assert_eq!(
                    fields(a.0),
                    fields(b.0),
                    "seed {seed} links {:?} and {:?} of one pad",
                    a.0,
                    b.0
                );
            }
            if coupled {
                continue;
            }
            let alone = |m: &&(LinkId, &ElementDesc, &ElementDesc, u32, u32, usize)| {
                made.iter().filter(|o| pad_of(o) == pad_of(m)).count() == 1
            };
            let plain = |m: &&(LinkId, &ElementDesc, &ElementDesc, u32, u32, usize)| {
                m.1.couplings.is_empty() && m.2.couplings.is_empty()
            };
            for (link, up, down, pad, k, _) in made.iter().filter(plain).filter(alone) {
                let wild = |m: &[OfferDesc]| m.iter().any(|o| o.is_wildcard());
                let (theirs, ours) =
                    (up.srcs[*pad as usize].offers, down.sinks[*k as usize].offers);
                if wild(theirs) || wild(ours) {
                    continue;
                }
                let mut q = Pipeline::new();
                let a = q.add(Node(element_with(theirs, false)));
                let b = q.add(Node(element_with(ours, true)));
                let alone =
                    q.link(a.src(SrcPad(0)), b.sink(SinkPad(0))).expect("linked in the graph");
                let render = |p: &Pipeline, l| {
                    let f = p.negotiated(l).unwrap();
                    let mut v: Vec<(String, Value)> = f
                        .fields()
                        .iter()
                        .map(|(id, v)| (p.field_name(*id).unwrap().to_string(), *v))
                        .collect();
                    v.sort_by(|x, y| x.0.cmp(&y.0));
                    (p.family_name(f.family).unwrap().to_string(), v)
                };
                assert_eq!(render(&p, *link), render(&q, alone), "seed {seed} link {link:?}");
            }
        }
        assert!(solved > 1_500 && refused > 300, "{solved} solved, {refused} links refused");
    }

    fn element_with(menu: &'static [OfferDesc], sink: bool) -> &'static ElementDesc {
        let pad = leak(vec![PadDesc { name: if sink { "sink" } else { "src" }, offers: menu }]);
        Box::leak(Box::new(ElementDesc {
            name: "pair",
            sinks: if sink { pad } else { &[] },
            srcs: if sink { &[] } else { pad },
            props: &[],
            sched: SchedHint::Active,
            inputs: if sink { InputPolicy::Single } else { InputPolicy::None },
            latency: LatencyDesc::NONE,
            make_default: None,
            io: false,
            couplings: &[],
        }))
    }
}
