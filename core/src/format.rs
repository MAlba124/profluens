use crate::error::OfferFault;
use crate::id::{FieldId, FormatId, Interner, ValueId};

/// The closed value universe with no heap. Rationals compare by value, `Rat(1, 2) == Rat(2, 4)`,
/// those with a zero denominator by their fields.
#[derive(Clone, Copy, Eq, Debug)]
pub enum Value {
    Int(i64),
    Rat(i32, i32),
    Id(ValueId),
}

/// Two rationals cross-multiplied, `None` with a zero denominator.
fn rat_cmp((an, ad): (i32, i32), (bn, bd): (i32, i32)) -> Option<core::cmp::Ordering> {
    if ad == 0 || bd == 0 {
        return None;
    }
    let (an, ad) = if ad < 0 { (-(an as i64), -(ad as i64)) } else { (an as i64, ad as i64) };
    let (bn, bd) = if bd < 0 { (-(bn as i64), -(bd as i64)) } else { (bn as i64, bd as i64) };
    Some((an as i128 * bd as i128).cmp(&(bn as i128 * ad as i128)))
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Rat(an, ad), Value::Rat(bn, bd)) => match rat_cmp((an, ad), (bn, bd)) {
                Some(order) => order.is_eq(),
                None => (an, ad) == (bn, bd),
            },
            (Value::Id(a), Value::Id(b)) => a == b,
            _ => false,
        }
    }
}

/// Partial order within a numeric kind, `None` across kinds and for ids.
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        match (*self, *other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(&b)),
            (Value::Rat(an, ad), Value::Rat(bn, bd)) => rat_cmp((an, ad), (bn, bd)),
            _ => None,
        }
    }
}

/// The closed constraint language a pad may declare per field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Constraint<'a> {
    Any,
    Eq(Value),
    Range { min: Value, max: Value, step: Value },
    Set(&'a [Value]),
}

impl Constraint<'_> {
    pub fn accepts(&self, v: Value) -> bool {
        use core::cmp::Ordering;
        match self {
            Constraint::Any => true,
            Constraint::Eq(x) => *x == v,
            Constraint::Set(vs) => vs.contains(&v),
            Constraint::Range { min, max, step } => {
                let ge_min =
                    matches!(v.partial_cmp(min), Some(Ordering::Greater | Ordering::Equal));
                let le_max = matches!(v.partial_cmp(max), Some(Ordering::Less | Ordering::Equal));
                if !(ge_min && le_max) {
                    return false;
                }
                // TODO: steps of Rat and Id ranges are ignored.
                match (v, *min, *step) {
                    (Value::Int(vi), Value::Int(mi), Value::Int(si)) => {
                        // vi >= mi here, so the u64 distance is exact even past i64.
                        si <= 0 || vi.abs_diff(mi) % si.unsigned_abs() == 0
                    }
                    _ => true,
                }
            }
        }
    }
}

pub struct FieldConstraint<'a> {
    pub field: FieldId,
    pub allowed: Constraint<'a>,
    pub preferred: Option<Value>,
}

pub struct FormatOffer<'a> {
    pub family: FormatId,
    pub fields: &'a [FieldConstraint<'a>],
    pub wildcard: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum ValueSpec<'a> {
    Int(i64),
    Rat(i32, i32),
    Id(&'a str),
}

#[derive(Clone, Copy, Debug)]
pub enum ConstraintSpec<'a> {
    Any,
    Eq(ValueSpec<'a>),
    Range { min: ValueSpec<'a>, max: ValueSpec<'a>, step: ValueSpec<'a> },
    Set(&'a [ValueSpec<'a>]),
}

#[derive(Clone, Copy, Debug)]
pub struct FieldSpec<'a> {
    pub field: &'a str,
    pub allowed: ConstraintSpec<'a>,
    pub preferred: Option<ValueSpec<'a>>,
}

#[derive(Clone, Copy, Debug)]
pub struct OfferSpec<'a> {
    pub family: &'a str,
    pub fields: &'a [FieldSpec<'a>],
}

pub type ValueDesc = ValueSpec<'static>;
pub type ConstraintDesc = ConstraintSpec<'static>;
pub type FieldDesc = FieldSpec<'static>;
pub type OfferDesc = OfferSpec<'static>;

/// The reserved family of a wildcard offer, which adopts the peer's family.
pub const WILDCARD: &str = "*";

pub const PROBED: &str = "?probed";

/// A link tries every src offer against every sink offer, 32 by 32 at most.
pub const MAX_PROBED_OFFERS: usize = 32;
/// A fixed format holds 16 fields, so an offer with more could never be fixed.
pub const MAX_OFFER_FIELDS: usize = 16;
/// Fixation scans a set once per candidate value, 128 by 128 at most per field.
pub const MAX_SET_VALUES: usize = 128;

impl OfferDesc {
    pub const PROBED: &'static [OfferDesc] = &[OfferSpec { family: PROBED, fields: &[] }];

    pub fn is_probed(menu: &[OfferDesc]) -> bool {
        menu.iter().any(|o| o.family == PROBED)
    }
}

impl<'a> OfferSpec<'a> {
    pub const fn any(family: &'a str) -> Self {
        OfferSpec { family, fields: &[] }
    }

    pub const fn wildcard() -> Self {
        OfferSpec { family: WILDCARD, fields: &[] }
    }

    pub fn is_wildcard(&self) -> bool {
        self.family == WILDCARD
    }
}

impl ValueSpec<'_> {
    fn lower(&self, values: &mut Interner) -> Value {
        match *self {
            ValueSpec::Int(n) => Value::Int(n),
            ValueSpec::Rat(n, d) => Value::Rat(n, d),
            ValueSpec::Id(s) => Value::Id(ValueId(values.intern(s))),
        }
    }
}

/// A pad's offers in id form, in preference order, interned once per pad.
#[derive(Clone)]
pub struct Interned {
    pub(crate) offers: Vec<(FormatId, usize, bool)>,
    pub(crate) fields: Vec<InternedField>,
    pub(crate) sets: Vec<Value>,
}

#[derive(Clone)]
pub(crate) struct InternedField {
    pub(crate) field: FieldId,
    /// A `Set` here is empty, its values are `sets[set]`.
    pub(crate) allowed: Constraint<'static>,
    pub(crate) set: core::ops::Range<usize>,
    pub(crate) preferred: Option<Value>,
}

impl Interned {
    // Once per pad, at its first link or at preroll.
    #[allow(clippy::disallowed_methods)]
    pub fn intern(offers: &[OfferSpec<'_>], names: [&mut Interner; 3]) -> Self {
        let [formats, fields, values] = names;
        let mut out = Self { offers: Vec::new(), fields: Vec::new(), sets: Vec::new() };
        for o in offers {
            out.offers.push((FormatId(formats.intern(o.family)), o.fields.len(), o.is_wildcard()));
            for f in o.fields {
                let mut lower = |v: ValueSpec<'_>| v.lower(values);
                let at = out.sets.len();
                let allowed = match f.allowed {
                    ConstraintSpec::Any => Constraint::Any,
                    ConstraintSpec::Eq(v) => Constraint::Eq(lower(v)),
                    ConstraintSpec::Range { min, max, step } => {
                        Constraint::Range { min: lower(min), max: lower(max), step: lower(step) }
                    }
                    ConstraintSpec::Set(vs) => {
                        out.sets.extend(vs.iter().map(|v| lower(*v)));
                        Constraint::Set(&[])
                    }
                };
                let preferred = f.preferred.map(lower);
                let field = FieldId(fields.intern(f.field));
                out.fields.push(InternedField {
                    field,
                    allowed,
                    set: at..out.sets.len(),
                    preferred,
                });
            }
        }
        out
    }

    /// Whether a format announced mid-stream fits, `None` when no offer is in its family.
    pub(crate) fn admits(&self, format: &FixedFormat) -> Option<bool> {
        let (mut at, mut gated) = (0, None);
        for &(family, n, wildcard) in &self.offers {
            let fields = &self.fields[at..at + n];
            at += n;
            if wildcard && n == 0 {
                return Some(true);
            }
            if wildcard || family != format.family {
                continue;
            }
            let fits = |c: &InternedField| match (format.get(c.field), &c.allowed) {
                (None, _) => true,
                (Some(v), Constraint::Set(_)) => self.sets[c.set.clone()].contains(&v),
                (Some(v), other) => other.accepts(v),
            };
            if fields.iter().all(fits) {
                return Some(true);
            }
            gated = Some(false);
        }
        gated
    }
}

pub(crate) fn admit(offers: &[OfferSpec<'_>]) -> Result<(), OfferFault> {
    let within = |what, found: usize, limit: usize| match found > limit {
        true => Err(OfferFault::OverLimit { what, found: found as u32, limit: limit as u32 }),
        false => Ok(()),
    };
    within("offers", offers.len(), MAX_PROBED_OFFERS)?;
    for o in offers {
        if o.family == PROBED || o.family == WILDCARD {
            return Err(OfferFault::ReservedFamily);
        }
        within("fields in an offer", o.fields.len(), MAX_OFFER_FIELDS)?;
        for f in o.fields {
            if let ConstraintSpec::Set(vs) = f.allowed {
                within("values in a set", vs.len(), MAX_SET_VALUES)?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum OfferSource<'a> {
    Offers(&'a Interned),
    Pinned(&'a FixedFormat),
}

impl OfferSource<'_> {
    pub fn offer_len(&self) -> usize {
        match self {
            OfferSource::Offers(p) => p.offers.len(),
            OfferSource::Pinned(_) => 1,
        }
    }

    fn field_len(&self) -> usize {
        match self {
            OfferSource::Offers(p) => p.fields.len(),
            OfferSource::Pinned(fixed) => fixed.len(),
        }
    }
}

/// Lowering pass 1, every field constraint of every offer, flat.
pub struct FieldScratch<'v> {
    fields: Vec<FieldConstraint<'v>>,
}

impl<'v> FieldScratch<'v> {
    // Link-time lowering, one exact-sized table per link call, never per buffer.
    #[allow(clippy::disallowed_methods)]
    pub fn lower(sides: &[OfferSource<'v>]) -> Self {
        let mut fields = Vec::with_capacity(sides.iter().map(OfferSource::field_len).sum());
        for side in sides {
            match *side {
                OfferSource::Offers(p) => fields.extend(p.fields.iter().map(|f| FieldConstraint {
                    field: f.field,
                    allowed: match &f.allowed {
                        Constraint::Set(_) => Constraint::Set(&p.sets[f.set.clone()]),
                        other => other.clone(),
                    },
                    preferred: f.preferred,
                })),
                OfferSource::Pinned(fixed) => {
                    fields.extend(fixed.fields().iter().map(|(field, v)| FieldConstraint {
                        field: *field,
                        allowed: Constraint::Eq(*v),
                        preferred: Some(*v),
                    }));
                }
            }
        }
        Self { fields }
    }

    // Link-time lowering, one exact-sized table per link call, never per buffer.
    #[allow(clippy::disallowed_methods)]
    pub fn offers<'f>(&'f self, sides: &[OfferSource<'_>]) -> Vec<FormatOffer<'f>> {
        let table: &'f [FieldConstraint<'f>] = &self.fields;
        let mut offers = Vec::with_capacity(sides.iter().map(OfferSource::offer_len).sum());
        let mut at = 0;
        let mut push = |family, n: usize, wildcard| {
            offers.push(FormatOffer { family, fields: &table[at..at + n], wildcard });
            at += n;
        };
        for side in sides {
            match side {
                OfferSource::Offers(p) => p.offers.iter().for_each(|&(f, n, w)| push(f, n, w)),
                OfferSource::Pinned(fixed) => push(fixed.family, fixed.len(), false),
            }
        }
        debug_assert_eq!(at, table.len(), "lowered from different sides than the fields");
        offers
    }
}

/// A link's negotiated format, flat POD with up to 16 fields inline.
#[derive(Clone)]
pub struct FixedFormat {
    pub family: FormatId,
    len: u8,
    fields: [(FieldId, Value); 16],
}

impl FixedFormat {
    pub fn new(family: FormatId) -> Self {
        Self { family, len: 0, fields: [(FieldId(0), Value::Int(0)); 16] }
    }

    pub fn get(&self, field: FieldId) -> Option<Value> {
        self.fields[..self.len as usize].iter().find(|(f, _)| *f == field).map(|(_, v)| *v)
    }

    pub fn set(&mut self, field: FieldId, value: Value) -> bool {
        for slot in &mut self.fields[..self.len as usize] {
            if slot.0 == field {
                slot.1 = value;
                return true;
            }
        }
        if (self.len as usize) < self.fields.len() {
            self.fields[self.len as usize] = (field, value);
            self.len += 1;
            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn reset(&mut self, family: FormatId) {
        (self.family, self.len) = (family, 0);
    }

    pub(crate) fn push_new(&mut self, field: FieldId, value: Value) -> bool {
        debug_assert!(self.get(field).is_none());
        match (self.len as usize) < self.fields.len() {
            true => {
                self.fields[self.len as usize] = (field, value);
                self.len += 1;
                true
            }
            false => false,
        }
    }

    pub(crate) fn copy_from(&mut self, other: &FixedFormat) {
        self.reset(other.family);
        self.len = other.len;
        self.fields[..other.len as usize].copy_from_slice(other.fields());
    }

    pub fn fields(&self) -> &[(FieldId, Value)] {
        &self.fields[..self.len as usize]
    }

    // For a log line or an error, never per buffer.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn render(&self, [formats, fields, values]: [&Interner; 3]) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = write!(out, "{}", formats.resolve(self.family.0).unwrap_or("?"));
        for (fid, v) in self.fields() {
            let _ = write!(out, ", {}=", fields.resolve(fid.0).unwrap_or("?"));
            let _ = match v {
                Value::Int(i) => write!(out, "{i}"),
                Value::Rat(n, d) => write!(out, "{n}/{d}"),
                Value::Id(id) => write!(out, "{}", values.resolve(id.0).unwrap_or("?")),
            };
        }
        out
    }
}

// Field order does not count, `set` keeps ids unique so equal lengths and inclusion suffice.
impl PartialEq for FixedFormat {
    fn eq(&self, other: &Self) -> bool {
        self.family == other.family
            && self.len == other.len
            && self.fields().iter().all(|&(field, value)| other.get(field) == Some(value))
    }
}

impl Eq for FixedFormat {}

#[cfg(test)]
fn find_field<'a>(offer: &FormatOffer<'a>, field: FieldId) -> Option<&'a FieldConstraint<'a>> {
    offer.fields.iter().find(|f| f.field == field)
}

/// Pick one value both sides accept, `Some(None)` when both are unconstrained.
// Link-time fixation, once per field, never per buffer.
#[cfg(test)]
fn fixate(
    a: Option<&FieldConstraint<'_>>,
    b: Option<&FieldConstraint<'_>>,
) -> Option<Option<Value>> {
    let mut s = crate::solve::Solver::default();
    let ca = a.map_or(crate::solve::DomC::Any, |f| s.lower_c(&f.allowed));
    let cb = b.map_or(crate::solve::DomC::Any, |f| s.lower_c(&f.allowed));
    let c = s.meet(ca, cb);
    if c == crate::solve::DomC::Empty {
        return None;
    }
    Some(s.choose(c, a.and_then(|f| f.preferred), b.and_then(|f| f.preferred)).map(|(v, _)| v))
}

#[cfg(test)]
pub(crate) fn intersect(a: &FormatOffer<'_>, b: &FormatOffer<'_>) -> Option<FixedFormat> {
    if (a.wildcard && !a.fields.is_empty()) || (b.wildcard && !b.fields.is_empty()) {
        return None;
    }

    match (a.wildcard, b.wildcard) {
        (true, true) => Some(FixedFormat::new(a.family)),
        (true, false) => fixate_solo(b),
        (false, true) => fixate_solo(a),
        (false, false) => {
            if a.family != b.family {
                return None;
            }
            let mut out = FixedFormat::new(a.family);

            for fc_a in a.fields {
                let value = fixate(Some(fc_a), find_field(b, fc_a.field))?;
                if let Some(v) = value
                    && !out.set(fc_a.field, v)
                {
                    return None;
                }
            }
            for fc_b in b.fields {
                if find_field(a, fc_b.field).is_some() {
                    continue;
                }
                if let Some(v) = fixate(None, Some(fc_b))?
                    && !out.set(fc_b.field, v)
                {
                    return None;
                }
            }

            Some(out)
        }
    }
}

#[cfg(test)]
fn fixate_solo(concrete: &FormatOffer<'_>) -> Option<FixedFormat> {
    let mut out = FixedFormat::new(concrete.family);
    for fc in concrete.fields {
        if let Some(v) = fixate(Some(fc), None)?
            && !out.set(fc.field, v)
        {
            return None;
        }
    }
    Some(out)
}

/// Negotiate a link from two preference-ordered offer lists, the first intersecting pair wins.
#[cfg(test)]
pub(crate) fn negotiate(src: &[FormatOffer<'_>], sink: &[FormatOffer<'_>]) -> Option<FixedFormat> {
    for a in src {
        for b in sink {
            if let Some(fixed) = intersect(a, b) {
                return Some(fixed);
            }
        }
    }
    None
}

pub struct Vocabulary {
    pub formats: Interner,
    pub fields: Interner,
    pub values: Interner,
}

impl Vocabulary {
    pub fn family_id(&self, name: &str) -> Option<FormatId> {
        self.formats.get(name).map(FormatId)
    }
    pub fn field_id(&self, name: &str) -> Option<FieldId> {
        self.fields.get(name).map(FieldId)
    }
    pub fn value_id(&self, name: &str) -> Option<ValueId> {
        self.values.get(name).map(ValueId)
    }
    pub fn family_name(&self, id: FormatId) -> Option<&str> {
        self.formats.resolve(id.0)
    }
    pub fn field_name(&self, id: FieldId) -> Option<&str> {
        self.fields.resolve(id.0)
    }
    pub fn value_name(&self, id: ValueId) -> Option<&str> {
        self.values.resolve(id.0)
    }

    pub fn build_fixed(
        &self,
        family: &str,
        fields: &[(&'static str, ValueDesc)],
    ) -> Option<FixedFormat> {
        let mut fixed = FixedFormat::new(self.family_id(family)?);
        for (name, vd) in fields {
            let field = self.field_id(name)?;
            let value = match vd {
                ValueDesc::Int(n) => Value::Int(*n),
                ValueDesc::Rat(n, d) => Value::Rat(*n, *d),
                ValueDesc::Id(s) => Value::Id(self.value_id(s)?),
            };
            if !fixed.set(field, value) {
                return None;
            }
        }
        Some(fixed)
    }
}

// Rendering for a failed link, the graph dump and `pf-registry`'s descriptions.
#[allow(clippy::disallowed_methods)]
pub fn value_line(v: &ValueSpec<'_>) -> String {
    match v {
        ValueSpec::Int(i) => i.to_string(),
        ValueSpec::Rat(n, d) => format!("{n}/{d}"),
        ValueSpec::Id(s) => (*s).to_string(),
    }
}

pub fn offer_line(o: &OfferSpec<'_>) -> String {
    let fields = o.fields.iter().map(|f| (f.field, constraint_line(&f.allowed)));
    format!("{}{}", o.family, fields.map(|(f, c)| format!(", {f}={c}")).collect::<String>())
}

pub fn constraint_line(c: &ConstraintSpec<'_>) -> String {
    use ConstraintSpec as C;
    match c {
        C::Any => "any".into(),
        C::Eq(v) => value_line(v),
        C::Range { min, max, step } => {
            format!("[{}, {}] step {}", value_line(min), value_line(max), value_line(step))
        }
        C::Set(vs) => {
            let items: Vec<String> = vs.iter().map(value_line).collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::id::{FieldId, FormatId, ValueId};

    const fn vi(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn int_ordering() {
        use core::cmp::Ordering;
        assert_eq!(vi(1).partial_cmp(&vi(2)), Some(Ordering::Less));
        assert_eq!(vi(2).partial_cmp(&vi(2)), Some(Ordering::Equal));
        assert_eq!(vi(1).partial_cmp(&Value::Id(ValueId(1))), None);
    }

    #[test]
    fn rational_ordering() {
        use core::cmp::Ordering;
        assert_eq!(Value::Rat(1, 2).partial_cmp(&Value::Rat(2, 3)), Some(Ordering::Less));
        assert_eq!(Value::Rat(2, 4).partial_cmp(&Value::Rat(1, 2)), Some(Ordering::Equal));
        assert_eq!(Value::Rat(1, -2).partial_cmp(&Value::Rat(-1, 2)), Some(Ordering::Equal));
        assert_eq!(Value::Rat(1, 0).partial_cmp(&Value::Rat(1, 2)), None);
        assert_eq!(Value::Rat(1, 2), Value::Rat(2, 4), "equal by value");
        assert_eq!(Value::Rat(1, -2), Value::Rat(-1, 2));
        assert_ne!(Value::Rat(1, 0), Value::Rat(2, 0), "a zero denominator compares by field");
        assert_ne!(Value::Rat(2, 1), Value::Int(2), "and never across kinds");
        let fps = [Value::Rat(30000, 1001)];
        assert!(Constraint::Set(&fps).accepts(Value::Rat(90000, 3003)));
        assert!(Constraint::Eq(Value::Rat(1, 2)).accepts(Value::Rat(2, 4)));
    }

    #[test]
    fn accepts_eq_and_set() {
        assert!(Constraint::Any.accepts(vi(5)));
        assert!(Constraint::Eq(vi(5)).accepts(vi(5)));
        assert!(!Constraint::Eq(vi(5)).accepts(vi(6)));

        let set = Constraint::Set(&[Value::Int(8), Value::Int(16), Value::Int(24)]);
        assert!(set.accepts(vi(16)));
        assert!(!set.accepts(vi(12)));
    }

    #[test]
    fn accepts_range_with_step() {
        let r = Constraint::Range { min: vi(16), max: vi(1920), step: vi(16) };
        assert!(r.accepts(vi(16)));
        assert!(r.accepts(vi(1920)));
        assert!(r.accepts(vi(1280)));
        assert!(!r.accepts(vi(1281)), "not step-aligned");
        assert!(!r.accepts(vi(8)), "below min");
        assert!(!r.accepts(vi(2048)), "above max");
    }

    #[test]
    fn a_range_wider_than_i64_still_holds_its_step() {
        let (lo, hi) = (vi(i64::MIN), vi(i64::MAX));
        let thirds = Constraint::Range { min: lo, max: hi, step: vi(3) };
        // i64::MAX - i64::MIN = 2^64 - 1, a multiple of 3
        assert!(thirds.accepts(vi(i64::MAX)));
        assert!(!thirds.accepts(vi(i64::MAX - 1)), "2^64 - 2 is off the step");
        assert!(thirds.accepts(vi(i64::MIN + 3)) && !thirds.accepts(vi(i64::MIN + 1)));
        let wide = Constraint::Range { min: lo, max: hi, step: vi(1 << 62) };
        assert!(wide.accepts(vi(1 << 62)), "3 * 2^62 past min");
        assert!(!wide.accepts(vi((1 << 62) + 1)));
        let unstepped = Constraint::Range { min: lo, max: hi, step: vi(0) };
        assert!(unstepped.accepts(vi(i64::MAX - 1)), "a step of 0 or less is none");
    }

    #[test]
    fn fixed_format_get_set_overwrite() {
        let mut f = FixedFormat::new(FormatId(1));
        let (w, h) = (FieldId(10), FieldId(11));
        assert!(f.get(w).is_none());
        assert!(f.set(w, vi(1920)));
        assert!(f.set(h, vi(1080)));
        assert_eq!(f.get(w), Some(vi(1920)));
        assert_eq!(f.get(h), Some(vi(1080)));
        assert!(f.set(w, vi(1280)), "overwrite in place");
        assert_eq!(f.get(w), Some(vi(1280)));
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn fixed_format_capacity() {
        let mut f = FixedFormat::new(FormatId(0));
        for i in 0..16 {
            assert!(f.set(FieldId(i), vi(i as i64)));
        }
        assert_eq!(f.len(), 16);
        assert!(!f.set(FieldId(100), vi(0)));
        assert!(f.set(FieldId(0), vi(42)));
        assert_eq!(f.get(FieldId(0)), Some(vi(42)));
    }

    static PIXFMTS: [Value; 3] =
        [Value::Id(ValueId(1)), Value::Id(ValueId(2)), Value::Id(ValueId(3))];

    fn fc(
        field: u32,
        allowed: Constraint<'static>,
        preferred: Option<Value>,
    ) -> FieldConstraint<'static> {
        FieldConstraint { field: FieldId(field), allowed, preferred }
    }

    fn offer(family: u32, fields: Vec<FieldConstraint<'static>>) -> FormatOffer<'static> {
        FormatOffer {
            family: FormatId(family),
            fields: Box::leak(fields.into_boxed_slice()),
            wildcard: false,
        }
    }

    fn wildcard(any_family: u32) -> FormatOffer<'static> {
        FormatOffer { family: FormatId(any_family), fields: &[], wildcard: true }
    }

    fn wildcard_with_fields(
        any_family: u32,
        fields: Vec<FieldConstraint<'static>>,
    ) -> FormatOffer<'static> {
        FormatOffer {
            family: FormatId(any_family),
            fields: Box::leak(fields.into_boxed_slice()),
            wildcard: true,
        }
    }

    // Leaks the tables so the offers can be returned, which only a test may do.
    #[allow(clippy::disallowed_methods)]
    fn lower(
        menu: &[OfferSpec<'_>],
        fmts: &mut Interner,
        flds: &mut Interner,
        vals: &mut Interner,
    ) -> Vec<FormatOffer<'static>> {
        let interned: &'static Interned =
            Box::leak(Box::new(Interned::intern(menu, [fmts, flds, vals])));
        let sides = [OfferSource::Offers(interned)];
        let fields: &'static FieldScratch<'static> =
            Box::leak(Box::new(FieldScratch::lower(&sides)));
        fields.offers(&sides)
    }

    #[test]
    fn intersect_family_mismatch() {
        assert!(intersect(&offer(1, vec![]), &offer(2, vec![])).is_none());
    }

    #[test]
    fn intersect_eq_eq() {
        let a = offer(1, vec![fc(10, Constraint::Eq(vi(48000)), None)]);
        let same = offer(1, vec![fc(10, Constraint::Eq(vi(48000)), None)]);
        assert_eq!(intersect(&a, &same).unwrap().get(FieldId(10)), Some(vi(48000)));
        let diff = offer(1, vec![fc(10, Constraint::Eq(vi(44100)), None)]);
        assert!(intersect(&a, &diff).is_none());
    }

    #[test]
    fn intersect_range_and_eq() {
        let range = offer(
            1,
            vec![fc(10, Constraint::Range { min: vi(16), max: vi(4096), step: vi(16) }, None)],
        );
        let good = offer(1, vec![fc(10, Constraint::Eq(vi(1920)), None)]);
        assert_eq!(intersect(&range, &good).unwrap().get(FieldId(10)), Some(vi(1920)));
        let misaligned = offer(1, vec![fc(10, Constraint::Eq(vi(1921)), None)]);
        assert!(intersect(&range, &misaligned).is_none());
    }

    #[test]
    fn intersect_set_set() {
        static A: [Value; 3] = [Value::Int(8), Value::Int(16), Value::Int(24)];
        static B: [Value; 2] = [Value::Int(16), Value::Int(32)];
        let a = offer(1, vec![fc(10, Constraint::Set(&A), None)]);
        let b = offer(1, vec![fc(10, Constraint::Set(&B), None)]);
        assert_eq!(intersect(&a, &b).unwrap().get(FieldId(10)), Some(vi(16)));
    }

    #[test]
    fn intersect_prefers_hint() {
        let wide = |pref| {
            offer(
                1,
                vec![fc(10, Constraint::Range { min: vi(0), max: vi(100), step: vi(1) }, pref)],
            )
        };
        assert_eq!(
            intersect(&wide(Some(vi(60))), &wide(Some(vi(30)))).unwrap().get(FieldId(10)),
            Some(vi(60))
        );
    }

    #[test]
    fn intersect_field_only_on_one_side() {
        let a = offer(1, vec![fc(10, Constraint::Eq(vi(2)), None)]);
        let b = offer(1, vec![fc(11, Constraint::Eq(vi(3)), None)]);
        let f = intersect(&a, &b).unwrap();
        assert_eq!(f.get(FieldId(10)), Some(vi(2)));
        assert_eq!(f.get(FieldId(11)), Some(vi(3)));
    }

    #[test]
    fn intersect_any_is_unconstrained() {
        let any = || offer(1, vec![fc(10, Constraint::Any, None)]);
        assert_eq!(intersect(&any(), &any()).unwrap().get(FieldId(10)), None);
        let eq = offer(1, vec![fc(10, Constraint::Eq(vi(7)), None)]);
        assert_eq!(intersect(&any(), &eq).unwrap().get(FieldId(10)), Some(vi(7)));
    }

    #[test]
    fn intersect_multi_field_video() {
        let (w, h, fmt) = (1u32, 2u32, 3u32);
        let src = offer(
            100,
            vec![
                fc(w, Constraint::Range { min: vi(0), max: vi(7680), step: vi(8) }, Some(vi(1920))),
                fc(h, Constraint::Range { min: vi(0), max: vi(7680), step: vi(8) }, Some(vi(1080))),
                fc(fmt, Constraint::Set(&PIXFMTS), None),
            ],
        );
        let sink = offer(
            100,
            vec![
                fc(w, Constraint::Eq(vi(1920)), None),
                fc(h, Constraint::Eq(vi(1080)), None),
                fc(fmt, Constraint::Eq(Value::Id(ValueId(2))), None),
            ],
        );
        let f = intersect(&src, &sink).unwrap();
        assert_eq!(f.get(FieldId(w)), Some(vi(1920)));
        assert_eq!(f.get(FieldId(h)), Some(vi(1080)));
        assert_eq!(f.get(FieldId(fmt)), Some(Value::Id(ValueId(2))));
    }

    #[test]
    fn offerdesc_wildcard_is_marked_and_lowers_to_wildcard() {
        let wc = OfferDesc::wildcard();
        assert!(wc.is_wildcard());
        assert!(wc.fields.is_empty(), "a wildcard offer carries no fields");
        assert!(!OfferDesc::any("bytes").is_wildcard(), "any() is a concrete family");

        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let lowered = &lower(&[wc], &mut fmts, &mut flds, &mut vals)[0];
        assert!(lowered.wildcard);
        assert_eq!(lowered.family, FormatId(fmts.get(WILDCARD).unwrap()));
        assert!(lowered.fields.is_empty());
    }

    #[test]
    fn intersect_wildcard_adopts_concrete_family_and_fields_both_orders() {
        static RATES: [Value; 2] = [Value::Int(44100), Value::Int(48000)];
        let concrete = || {
            offer(
                5,
                vec![
                    fc(10, Constraint::Set(&RATES), Some(vi(48000))),
                    fc(11, Constraint::Eq(vi(2)), None),
                ],
            )
        };
        for (a, b) in [(wildcard(0), concrete()), (concrete(), wildcard(0))] {
            let f = intersect(&a, &b).expect("wildcard adopts the peer");
            assert_eq!(f.family, FormatId(5), "adopted the concrete family");
            assert_eq!(f.get(FieldId(10)), Some(vi(48000)), "preferred fixation");
            assert_eq!(f.get(FieldId(11)), Some(vi(2)));
        }
    }

    #[test]
    fn intersect_wildcard_vs_wildcard_picks_any_family() {
        let f = intersect(&wildcard(7), &wildcard(7)).expect("two wildcards intersect");
        assert_eq!(f.family, FormatId(7), "the interned any family");
        assert!(f.is_empty(), "no fields to fixate");
    }

    #[test]
    fn intersect_wildcard_with_fields_is_rejected_loudly() {
        let bad = || wildcard_with_fields(0, vec![fc(10, Constraint::Eq(vi(1)), None)]);
        assert!(intersect(&bad(), &offer(5, vec![])).is_none(), "wildcard-with-fields (a)");
        assert!(intersect(&offer(5, vec![]), &bad()).is_none(), "wildcard-with-fields (b)");
        assert!(intersect(&bad(), &wildcard(0)).is_none(), "even vs a clean wildcard");
    }

    #[test]
    fn negotiate_wildcard_pad_links_against_any_family() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        static SINK_F: [FieldDesc; 1] = [FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Eq(ValueDesc::Int(48000)),
            preferred: None,
        }];
        let src = lower(&[OfferDesc::wildcard()], &mut fmts, &mut flds, &mut vals);
        let sink = lower(
            &[OfferDesc { family: "audio/raw", fields: &SINK_F }],
            &mut fmts,
            &mut flds,
            &mut vals,
        );
        let f = negotiate(&src, &sink).expect("wildcard links against audio/raw");
        assert_eq!(f.family, FormatId(fmts.get("audio/raw").unwrap()));
        assert_eq!(f.get(FieldId(flds.get("rate").unwrap())), Some(vi(48000)));
    }

    #[test]
    fn negotiate_picks_first_compatible_pair() {
        let src = vec![offer(1, vec![]), offer(2, vec![fc(10, Constraint::Eq(vi(48000)), None)])];
        let sink = vec![offer(2, vec![fc(10, Constraint::Eq(vi(48000)), None)]), offer(3, vec![])];
        let f = negotiate(&src, &sink).unwrap();
        assert_eq!(f.family, FormatId(2));
        assert_eq!(f.get(FieldId(10)), Some(vi(48000)));
    }

    #[test]
    fn negotiate_prefers_earlier_offer() {
        let src = vec![offer(1, vec![]), offer(2, vec![])];
        let sink = vec![offer(2, vec![]), offer(1, vec![])];
        assert_eq!(negotiate(&src, &sink).unwrap().family, FormatId(1));
    }

    #[test]
    fn negotiate_none_when_disjoint() {
        let src = vec![offer(1, vec![]), offer(2, vec![])];
        let sink = vec![offer(3, vec![]), offer(4, vec![])];
        assert!(negotiate(&src, &sink).is_none());
    }

    #[test]
    fn negotiate_empty_offer_list_matches_nothing() {
        assert!(negotiate(&[], &[offer(1, vec![])]).is_none());
        assert!(negotiate(&[offer(1, vec![])], &[]).is_none());
    }

    #[test]
    fn lower_interns_family_field_and_value() {
        let mut fmts = Interner::new();
        let mut flds = Interner::new();
        let mut vals = Interner::new();

        static FIELDS: [FieldDesc; 2] = [
            FieldDesc {
                field: "rate",
                allowed: ConstraintDesc::Eq(ValueDesc::Int(48000)),
                preferred: None,
            },
            FieldDesc {
                field: "layout",
                allowed: ConstraintDesc::Eq(ValueDesc::Id("interleaved")),
                preferred: None,
            },
        ];
        let desc = OfferDesc { family: "audio/raw", fields: &FIELDS };
        let lowered = &lower(&[desc], &mut fmts, &mut flds, &mut vals)[0];

        assert_eq!(lowered.family, FormatId(fmts.get("audio/raw").unwrap()));
        assert_eq!(lowered.fields.len(), 2);
        assert_eq!(lowered.fields[0].field, FieldId(flds.get("rate").unwrap()));
        assert!(matches!(lowered.fields[0].allowed, Constraint::Eq(Value::Int(48000))));
        let layout_id = vals.get("interleaved").unwrap();
        assert!(matches!(
            lowered.fields[1].allowed,
            Constraint::Eq(Value::Id(ValueId(id))) if id == layout_id
        ));
    }

    #[test]
    fn lower_is_consistent_across_offers() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        static F: [FieldDesc; 1] =
            [FieldDesc { field: "rate", allowed: ConstraintDesc::Any, preferred: None }];
        let menu = [OfferDesc { family: "audio/raw", fields: &F }];
        let a = &lower(&menu, &mut fmts, &mut flds, &mut vals)[0];
        let b = &lower(&menu, &mut fmts, &mut flds, &mut vals)[0];
        assert_eq!(a.family, b.family);
        assert_eq!(a.fields[0].field, b.fields[0].field);
    }

    #[test]
    fn lower_set_interns_every_value() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        static FMTS: [ValueDesc; 3] =
            [ValueDesc::Id("s16"), ValueDesc::Id("s24"), ValueDesc::Id("f32")];
        static F: [FieldDesc; 1] =
            [FieldDesc { field: "sample", allowed: ConstraintDesc::Set(&FMTS), preferred: None }];
        let menu = [OfferDesc { family: "audio/raw", fields: &F }];
        let lowered = &lower(&menu, &mut fmts, &mut flds, &mut vals)[0];
        let Constraint::Set(vs) = lowered.fields[0].allowed else {
            panic!("expected a Set");
        };
        assert_eq!(vs.len(), 3);
        for name in ["s16", "s24", "f32"] {
            let id = vals.get(name).expect("value interned");
            assert!(vs.contains(&Value::Id(ValueId(id))));
        }
    }

    #[test]
    fn descriptor_to_negotiated_audio() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        static SRC_RATES: [ValueDesc; 2] = [ValueDesc::Int(44100), ValueDesc::Int(48000)];
        static SRC_F: [FieldDesc; 1] = [FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Set(&SRC_RATES),
            preferred: Some(ValueDesc::Int(48000)),
        }];
        static SINK_F: [FieldDesc; 1] = [FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Eq(ValueDesc::Int(48000)),
            preferred: None,
        }];
        let src_menu = [OfferDesc { family: "audio/raw", fields: &SRC_F }];
        let sink_menu = [OfferDesc { family: "audio/raw", fields: &SINK_F }];
        let src = lower(&src_menu, &mut fmts, &mut flds, &mut vals);
        let sink = lower(&sink_menu, &mut fmts, &mut flds, &mut vals);

        let f = negotiate(&src, &sink).expect("compatible");
        assert_eq!(f.family, FormatId(fmts.get("audio/raw").unwrap()));
        assert_eq!(f.get(FieldId(flds.get("rate").unwrap())), Some(vi(48000)));
    }

    #[test]
    fn fixed_formats_compare_as_field_sets() {
        let (rate, channels) = (FieldId(3), FieldId(9));
        let build = |family, pairs: &[(FieldId, Value)]| {
            let mut f = FixedFormat::new(FormatId(family));
            for &(field, value) in pairs {
                assert!(f.set(field, value));
            }
            f
        };
        let a = build(1, &[(rate, vi(48_000)), (channels, vi(2))]);
        assert!(a == build(1, &[(channels, vi(2)), (rate, vi(48_000))]), "set order");
        assert!(a == build(1, &[(channels, vi(6)), (rate, vi(48_000)), (channels, vi(2))]));
        assert!(a != build(1, &[(rate, vi(44_100)), (channels, vi(2))]), "a value");
        assert!(a != build(2, &[(rate, vi(48_000)), (channels, vi(2))]), "the family");
        assert!(a != build(1, &[(rate, vi(48_000))]), "a missing field");
        assert!(a != build(1, &[(rate, vi(48_000)), (FieldId(4), vi(2))]), "another field");
        assert!(FixedFormat::new(FormatId(1)) == FixedFormat::new(FormatId(1)));
    }

    static MIX_FORMATS: [ValueDesc; 3] =
        [ValueDesc::Id("s16"), ValueDesc::Id("s24"), ValueDesc::Id("f32")];
    static MIX_LAYOUTS: [ValueDesc; 2] = [ValueDesc::Id("interleaved"), ValueDesc::Id("planar")];
    static MIX_FIELDS: [FieldDesc; 5] = [
        FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Range {
                min: ValueDesc::Int(8000),
                max: ValueDesc::Int(192_000),
                step: ValueDesc::Int(1),
            },
            preferred: Some(ValueDesc::Int(48_000)),
        },
        FieldDesc {
            field: "format",
            allowed: ConstraintDesc::Set(&MIX_FORMATS),
            preferred: Some(ValueDesc::Id("f32")),
        },
        FieldDesc { field: "meta", allowed: ConstraintDesc::Any, preferred: None },
        FieldDesc { field: "layout", allowed: ConstraintDesc::Set(&MIX_LAYOUTS), preferred: None },
        FieldDesc {
            field: "endian",
            allowed: ConstraintDesc::Eq(ValueDesc::Id("le")),
            preferred: None,
        },
    ];
    static MIX_MENU: [OfferDesc; 2] =
        [OfferDesc { family: "audio/raw", fields: &MIX_FIELDS }, OfferDesc::any("bytes")];

    static PEER_FORMATS: [ValueDesc; 2] = [ValueDesc::Id("f64"), ValueDesc::Id("s24")];
    static PEER_FIELDS: [FieldDesc; 2] = [
        FieldDesc { field: "format", allowed: ConstraintDesc::Set(&PEER_FORMATS), preferred: None },
        FieldDesc {
            field: "rate",
            allowed: ConstraintDesc::Eq(ValueDesc::Int(44_100)),
            preferred: None,
        },
    ];
    static PEER_MENU: [OfferDesc; 2] =
        [OfferDesc::any("video/raw"), OfferDesc { family: "audio/raw", fields: &PEER_FIELDS }];

    fn id_of(vals: &Interner, name: &str) -> Value {
        Value::Id(ValueId(vals.get(name).expect("value interned")))
    }

    #[test]
    fn lowering_two_menus_lines_up_every_slice() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let mix = Interned::intern(&MIX_MENU, [&mut fmts, &mut flds, &mut vals]);
        let peer = Interned::intern(&PEER_MENU, [&mut fmts, &mut flds, &mut vals]);
        let sides = [OfferSource::Offers(&mix), OfferSource::Offers(&peer)];
        let fields = FieldScratch::lower(&sides);
        let offers = fields.offers(&sides);
        let (src, sink) = offers.split_at(sides[0].offer_len());
        assert_eq!((src.len(), sink.len()), (2, 2));

        assert_eq!((mix.sets.len(), peer.sets.len()), (5, 2));
        assert_eq!(fields.fields.len(), 7);
        assert_eq!(fields.fields.capacity(), 7);

        let audio = &src[0];
        assert_eq!(audio.family, FormatId(fmts.get("audio/raw").unwrap()));
        assert!(!audio.wildcard);
        assert_eq!(audio.fields.len(), 5);
        for (lowered, desc) in audio.fields.iter().zip(&MIX_FIELDS) {
            assert_eq!(lowered.field, FieldId(flds.get(desc.field).unwrap()));
        }
        assert!(matches!(
            audio.fields[0].allowed,
            Constraint::Range {
                min: Value::Int(8000),
                max: Value::Int(192_000),
                step: Value::Int(1)
            }
        ));
        assert_eq!(audio.fields[0].preferred, Some(vi(48_000)));
        let Constraint::Set(formats) = audio.fields[1].allowed else { panic!("expected a Set") };
        assert_eq!(
            formats,
            [id_of(&vals, "s16"), id_of(&vals, "s24"), id_of(&vals, "f32")].as_slice()
        );
        assert_eq!(audio.fields[1].preferred, Some(id_of(&vals, "f32")));
        assert!(matches!(audio.fields[2].allowed, Constraint::Any));
        assert_eq!(audio.fields[2].preferred, None);
        let Constraint::Set(layouts) = audio.fields[3].allowed else { panic!("expected a Set") };
        assert_eq!(layouts, [id_of(&vals, "interleaved"), id_of(&vals, "planar")].as_slice());
        assert_eq!(audio.fields[3].preferred, None);
        assert!(matches!(audio.fields[4].allowed, Constraint::Eq(v) if v == id_of(&vals, "le")));

        assert_eq!(src[1].family, FormatId(fmts.get("bytes").unwrap()));
        assert!(src[1].fields.is_empty());

        assert_eq!(sink[0].family, FormatId(fmts.get("video/raw").unwrap()));
        assert!(sink[0].fields.is_empty());
        let peer = &sink[1];
        assert_eq!(peer.fields.len(), 2);
        let Constraint::Set(peer_formats) = peer.fields[0].allowed else {
            panic!("expected a Set")
        };
        assert_eq!(peer_formats, [id_of(&vals, "f64"), id_of(&vals, "s24")].as_slice());
        assert!(matches!(peer.fields[1].allowed, Constraint::Eq(Value::Int(44_100))));

        let f = negotiate(src, sink).expect("audio/raw is common");
        assert_eq!(f.family, FormatId(fmts.get("audio/raw").unwrap()));
        assert_eq!(f.get(FieldId(flds.get("rate").unwrap())), Some(vi(44_100)));
        assert_eq!(f.get(FieldId(flds.get("format").unwrap())), Some(id_of(&vals, "s24")));
        assert_eq!(f.get(FieldId(flds.get("layout").unwrap())), Some(id_of(&vals, "interleaved")));
        assert_eq!(f.get(FieldId(flds.get("endian").unwrap())), Some(id_of(&vals, "le")));
        assert_eq!(f.get(FieldId(flds.get("meta").unwrap())), None);
    }

    #[test]
    fn lowering_interns_in_declaration_order() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let _ = Interned::intern(&MIX_MENU, [&mut fmts, &mut flds, &mut vals]);
        let _ = Interned::intern(&PEER_MENU, [&mut fmts, &mut flds, &mut vals]);

        fn names(i: &Interner) -> Vec<&str> {
            (0..i.len() as u32).map(|id| i.resolve(id).unwrap()).collect()
        }
        assert_eq!(names(&fmts), ["audio/raw", "bytes", "video/raw"]);
        assert_eq!(names(&flds), ["rate", "format", "meta", "layout", "endian"]);
        assert_eq!(names(&vals), ["s16", "s24", "f32", "interleaved", "planar", "le", "f64"]);
    }

    #[test]
    fn pinned_side_admits_exactly_its_format() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let _ = Interned::intern(&MIX_MENU, [&mut fmts, &mut flds, &mut vals]);
        let peer = Interned::intern(&PEER_MENU, [&mut fmts, &mut flds, &mut vals]);
        let (rate, format) =
            (FieldId(flds.get("rate").unwrap()), FieldId(flds.get("format").unwrap()));

        let mut carried = FixedFormat::new(FormatId(fmts.get("audio/raw").unwrap()));
        carried.set(rate, vi(44_100));
        carried.set(format, id_of(&vals, "s24"));
        let mut other = carried.clone();
        other.set(rate, vi(96_000));

        for pinned_first in [true, false] {
            for (fixed, links) in [(&carried, true), (&other, false)] {
                let (pinned, menu) = (OfferSource::Pinned(fixed), OfferSource::Offers(&peer));
                let sides = if pinned_first { [pinned, menu] } else { [menu, pinned] };
                let fields = FieldScratch::lower(&sides);
                let offers = fields.offers(&sides);
                let (a, b) = offers.split_at(sides[0].offer_len());

                let pin = if pinned_first { &a[0] } else { &b[0] };
                assert_eq!(pin.family, fixed.family);
                assert!(!pin.wildcard);
                assert_eq!(pin.fields.len(), 2);
                for (fc, (field, v)) in pin.fields.iter().zip(fixed.fields()) {
                    assert_eq!(fc.field, *field);
                    assert!(matches!(fc.allowed, Constraint::Eq(x) if x == *v));
                    assert_eq!(fc.preferred, Some(*v));
                }

                let got = negotiate(a, b);
                assert_eq!(got.is_some(), links, "pinned_first={pinned_first}");
                if let Some(f) = got {
                    assert_eq!(f.fields().len(), 2);
                    assert_eq!(f.get(rate), Some(vi(44_100)));
                    assert_eq!(f.get(format), Some(id_of(&vals, "s24")));
                }
            }
        }
        assert_eq!((fmts.len(), flds.len(), vals.len()), (3, 5, 7));
    }

    #[test]
    fn negotiate_table() {
        let cases: &[(&[u32], &[u32], Option<u32>)] = &[
            (&[1], &[1], Some(1)),
            (&[1], &[2], None),
            (&[1, 2], &[2, 1], Some(1)),
            (&[3, 1], &[1, 3], Some(3)),
            (&[1, 2, 3], &[3], Some(3)),
            (&[], &[1], None),
            (&[1], &[], None),
            (&[5, 6], &[7, 8], None),
        ];
        for (i, (sa, sb, want)) in cases.iter().enumerate() {
            let src: Vec<_> = sa.iter().map(|&f| offer(f, vec![])).collect();
            let sink: Vec<_> = sb.iter().map(|&f| offer(f, vec![])).collect();
            let got = negotiate(&src, &sink).map(|f| f.family.0);
            assert_eq!(got, *want, "case {i}: src={sa:?} sink={sb:?}");
        }
    }

    fn admits(
        menu: &[OfferSpec<'_>],
        family: &str,
        fields: &[(&str, ValueSpec<'_>)],
    ) -> Option<bool> {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let interned = Interned::intern(menu, [&mut fmts, &mut flds, &mut vals]);
        let mut f = FixedFormat::new(FormatId(fmts.intern(family)));
        for (name, v) in fields {
            f.set(FieldId(flds.intern(name)), v.lower(&mut vals));
        }
        interned.admits(&f)
    }

    fn one<'a>(field: &'a str, allowed: ConstraintSpec<'a>) -> [FieldSpec<'a>; 1] {
        [FieldSpec { field, allowed, preferred: None }]
    }

    #[test]
    fn admits_holds_a_value_against_every_kind_of_constraint() {
        let rate = |hz| [("rate", ValueSpec::Int(hz))];
        let audio = |fields| [OfferSpec { family: "audio/raw", fields }];
        let eq = one("rate", ConstraintSpec::Eq(ValueSpec::Int(44_100)));
        assert_eq!(admits(&audio(&eq), "audio/raw", &rate(44_100)), Some(true));
        assert_eq!(admits(&audio(&eq), "audio/raw", &rate(48_000)), Some(false));

        let rates = [ValueSpec::Int(44_100), ValueSpec::Int(48_000)];
        let set = one("rate", ConstraintSpec::Set(&rates));
        assert_eq!(admits(&audio(&set), "audio/raw", &rate(48_000)), Some(true));
        assert_eq!(admits(&audio(&set), "audio/raw", &rate(96_000)), Some(false));

        let (min, max, step) =
            (ValueSpec::Int(8_000), ValueSpec::Int(48_000), ValueSpec::Int(4_000));
        let range = one("rate", ConstraintSpec::Range { min, max, step });
        assert_eq!(admits(&audio(&range), "audio/raw", &rate(44_000)), Some(true));
        assert_eq!(admits(&audio(&range), "audio/raw", &rate(44_100)), Some(false), "off the step");
        assert_eq!(admits(&audio(&range), "audio/raw", &rate(96_000)), Some(false));

        let any = one("rate", ConstraintSpec::Any);
        assert_eq!(admits(&audio(&any), "audio/raw", &rate(1)), Some(true));
        assert_eq!(admits(&[OfferSpec::any("audio/raw")], "audio/raw", &rate(1)), Some(true));
    }

    #[test]
    fn admits_reads_a_categorical_value_by_id() {
        let samples = [ValueSpec::Id("s16"), ValueSpec::Id("s24")];
        let set = one("sample", ConstraintSpec::Set(&samples));
        let menu = [OfferSpec { family: "audio/raw", fields: &set }];
        let sample = |name| [("sample", ValueSpec::Id(name))];
        assert_eq!(admits(&menu, "audio/raw", &sample("s24")), Some(true));
        assert_eq!(
            admits(&menu, "audio/raw", &sample("f32")),
            Some(false),
            "interned after the menu"
        );
    }

    #[test]
    fn admits_leaves_a_field_the_format_does_not_fix_to_the_peer() {
        let ch = one("channels", ConstraintSpec::Eq(ValueSpec::Int(2)));
        let menu = [OfferSpec { family: "audio/raw", fields: &ch }];
        assert_eq!(admits(&menu, "audio/raw", &[("rate", ValueSpec::Int(44_100))]), Some(true));
    }

    #[test]
    fn admits_needs_one_offer_of_the_family_and_gates_nothing_without_one() {
        let rate = [("rate", ValueSpec::Int(48_000))];
        let a = one("rate", ConstraintSpec::Eq(ValueSpec::Int(44_100)));
        let b = one("rate", ConstraintSpec::Eq(ValueSpec::Int(48_000)));
        let both = [
            OfferSpec { family: "audio/raw", fields: &a },
            OfferSpec::any("bytes"),
            OfferSpec { family: "audio/raw", fields: &b },
        ];
        assert_eq!(admits(&both, "audio/raw", &rate), Some(true), "the second alternative");
        assert_eq!(admits(&both[..2], "audio/raw", &rate), Some(false));
        assert_eq!(admits(&both[1..2], "audio/raw", &rate), None);
        assert_eq!(admits(&[], "audio/raw", &rate), None, "no offers gate nothing");
    }

    #[test]
    fn a_wildcard_admits_any_announced_format() {
        let wild = [OfferSpec::wildcard()];
        assert_eq!(admits(&wild, "audio/raw", &[("rate", ValueSpec::Int(48_000))]), Some(true));
        assert_eq!(admits(&wild, "video/raw", &[("width", ValueSpec::Int(1920))]), Some(true));
        let fields = one("rate", ConstraintSpec::Any);
        let bad = [OfferSpec { family: WILDCARD, fields: &fields }];
        assert_eq!(admits(&bad, "audio/raw", &[]), None);
    }

    #[test]
    fn a_format_renders_with_its_names() {
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let mut f = FixedFormat::new(FormatId(fmts.intern("audio/raw")));
        f.set(FieldId(flds.intern("rate")), vi(48_000));
        f.set(FieldId(flds.intern("sample")), Value::Id(ValueId(vals.intern("s16"))));
        f.set(FieldId(flds.intern("par")), Value::Rat(16, 15));
        assert_eq!(f.render([&fmts, &flds, &vals]), "audio/raw, rate=48000, sample=s16, par=16/15");
        assert_eq!(FixedFormat::new(FormatId(9)).render([&fmts, &flds, &vals]), "?");
    }

    #[test]
    fn offers_are_interned_from_borrowed_names_and_lower_like_a_menu() {
        let (family, pixfmt, nv12) =
            (String::from("video/gpu"), String::from("pixfmt"), String::from("nv12"));
        let mods = [ValueSpec::Int(7), ValueSpec::Int(-1), ValueSpec::Int(9)];
        let fields = [
            FieldSpec {
                field: &pixfmt,
                allowed: ConstraintSpec::Eq(ValueSpec::Id(&nv12)),
                preferred: None,
            },
            FieldSpec {
                field: "modifier",
                allowed: ConstraintSpec::Set(&mods),
                preferred: Some(ValueSpec::Int(9)),
            },
            FieldSpec {
                field: "width",
                allowed: ConstraintSpec::Range {
                    min: ValueSpec::Int(16),
                    max: ValueSpec::Int(4096),
                    step: ValueSpec::Int(2),
                },
                preferred: None,
            },
            FieldSpec { field: "height", allowed: ConstraintSpec::Any, preferred: None },
        ];
        let offers = [OfferSpec { family: &family, fields: &fields }, OfferSpec::any("bytes")];
        assert_eq!(admit(&offers), Ok(()));
        assert_eq!(
            offer_line(&offers[0]),
            "video/gpu, pixfmt=nv12, modifier={7, -1, 9}, width=[16, 4096] step 2, height=any"
        );
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let lowered = lower(&offers, &mut fmts, &mut flds, &mut vals);
        drop((family, pixfmt, nv12));

        let gpu = FormatId(fmts.get("video/gpu").unwrap());
        let id = |n: &str| FieldId(flds.get(n).unwrap());
        assert_eq!(lowered.len(), 2, "in the order supplied");
        assert_eq!(
            (lowered[0].family, lowered[0].fields.len(), lowered[0].wildcard),
            (gpu, 4, false)
        );
        assert_eq!(lowered[1].fields.len(), 0);
        let f = lowered[0].fields;
        assert_eq!(
            (f[0].field, &f[0].allowed),
            (id("pixfmt"), &Constraint::Eq(id_of(&vals, "nv12")))
        );
        assert_eq!(f[1].allowed, Constraint::Set(&[vi(7), vi(-1), vi(9)]), "a set keeps its order");
        assert_eq!(f[1].preferred, Some(vi(9)));
        assert_eq!(f[2].allowed, Constraint::Range { min: vi(16), max: vi(4096), step: vi(2) });
        assert_eq!((f[3].field, &f[3].allowed), (id("height"), &Constraint::Any));

        const PEER_MODS: &[Value] = &[vi(9), vi(7)];
        let peer_fields = [fc(id("modifier").0, Constraint::Set(PEER_MODS), None)];
        let peer = FormatOffer { family: gpu, fields: &peer_fields, wildcard: false };
        let fixed = intersect(&lowered[0], &peer).expect("the sets share 7 and 9");
        assert_eq!(fixed.get(id("modifier")), Some(vi(9)));
    }

    #[test]
    fn two_sets_in_one_pad_do_not_share_values() {
        let (a, b) = ([ValueSpec::Int(1), ValueSpec::Int(2)], [ValueSpec::Int(3)]);
        let set =
            |field, vs| FieldSpec { field, allowed: ConstraintSpec::Set(vs), preferred: None };
        let (fa, fb) = ([set("m", &a)], [set("m", &b)]);
        let offers =
            [OfferSpec { family: "x", fields: &fa }, OfferSpec { family: "y", fields: &fb }];
        let (mut fmts, mut flds, mut vals) = (Interner::new(), Interner::new(), Interner::new());
        let lowered = lower(&offers, &mut fmts, &mut flds, &mut vals);
        assert_eq!(lowered[0].fields[0].allowed, Constraint::Set(&[vi(1), vi(2)]));
        assert_eq!(lowered[1].fields[0].allowed, Constraint::Set(&[vi(3)]));
    }

    #[test]
    fn a_probe_past_a_limit_is_refused_and_one_at_it_is_not() {
        let over = |what, limit: usize| {
            Err(OfferFault::OverLimit { what, found: limit as u32 + 1, limit: limit as u32 })
        };

        let many = vec![OfferSpec::any("bytes"); MAX_PROBED_OFFERS + 1];
        assert_eq!(admit(&many[1..]), Ok(()));
        assert_eq!(admit(&many), over("offers", MAX_PROBED_OFFERS));

        let any = FieldSpec { field: "f", allowed: ConstraintSpec::Any, preferred: None };
        let fields = vec![any; MAX_OFFER_FIELDS + 1];
        assert_eq!(admit(&[OfferSpec { family: "x", fields: &fields[1..] }]), Ok(()));
        assert_eq!(
            admit(&[OfferSpec { family: "x", fields: &fields }]),
            over("fields in an offer", MAX_OFFER_FIELDS)
        );

        let values = vec![ValueSpec::Int(0); MAX_SET_VALUES + 1];
        let set =
            |vs| [FieldSpec { field: "f", allowed: ConstraintSpec::Set(vs), preferred: None }];
        assert_eq!(admit(&[OfferSpec { family: "x", fields: &set(&values[1..]) }]), Ok(()));
        assert_eq!(
            admit(&[OfferSpec { family: "x", fields: &set(&values) }]),
            over("values in a set", MAX_SET_VALUES)
        );
    }

    #[test]
    fn a_probe_may_not_name_a_reserved_family() {
        for family in [PROBED, WILDCARD] {
            assert_eq!(
                admit(&[OfferSpec::any(family)]),
                Err(OfferFault::ReservedFamily),
                "{family}"
            );
        }
        assert!(OfferDesc::is_probed(OfferDesc::PROBED));
        assert!(!OfferDesc::is_probed(&[OfferDesc::wildcard(), OfferDesc::any("bytes")]));
    }
}
