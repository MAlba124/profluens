//! A join group. Joined sessions share one scheduler and its threads, so the stages of one chain
//! join one idle parent and chains stay apart. A child keeps its group alive, the parent closes
//! after the last child disjoined.

use std::sync::Arc;

use crate::error::MfxResult;
use crate::session::Session;

/// Which parent an element without a group of its own joins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Join {
    /// The process wide parent of the probe, every element's session on one scheduler.
    #[default]
    Global,
    /// No join, each session its own scheduler. Foreign surfaces go through the import.
    None,
}

impl Join {
    /// `PF_VPL_JOIN=global|none`, anything else is `Global`.
    pub fn of_env(v: Option<&str>) -> Join {
        match v {
            Some("none") => Join::None,
            _ => Join::Global,
        }
    }
}

/// The idle parent session of one chain. No component is ever initialized on it.
pub struct VplGroup {
    parent: Session,
}

// SAFETY: the parent is only joined to and disjoined from, under the session lock.
unsafe impl Sync for VplGroup {}

impl VplGroup {
    /// A group on the probed runtime, `None` without one or when the session fails.
    // Cold, one per chain.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Option<Arc<VplGroup>> {
        let rt = crate::probe::runtime()?;
        Session::new(&rt.loader, &rt.device).ok().map(VplGroup::of)
    }

    pub fn of(parent: Session) -> Arc<VplGroup> {
        Arc::new(VplGroup { parent })
    }

    pub fn parent(&self) -> &Session {
        &self.parent
    }
}

/// A child session on its way into its group. The join is retried while the runtime reports
/// MFX_WRN_IN_EXECUTION, the session kept here between tries, never waited for.
#[derive(Default)]
pub struct Joining {
    session: Option<Session>,
}

impl Joining {
    /// `Ok(None)` while the join is busy, the caller parks and calls again. `make` runs once.
    pub fn open(
        &mut self,
        parent: Option<&Arc<VplGroup>>,
        make: impl FnOnce() -> MfxResult<Session>,
    ) -> MfxResult<Option<Session>> {
        let mut s = match self.session.take() {
            Some(s) => s,
            None => make()?,
        };
        if let Some(g) = parent
            && !s.join_to(g)?
        {
            self.session = Some(s);
            return Ok(None);
        }
        Ok(Some(s))
    }

    pub fn pending(&self) -> bool {
        self.session.is_some()
    }

    pub fn clear(&mut self) {
        self.session = None;
    }
}

#[cfg(test)]
// Fakes behind the session wrapper, no runtime.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::ffi;
    use crate::session::fake::{self, Op};
    use crate::session::{Component, Owner};

    #[test]
    fn join_mode_reads_its_env() {
        assert_eq!(Join::of_env(None), Join::Global);
        assert_eq!(Join::of_env(Some("global")), Join::Global);
        assert_eq!(Join::of_env(Some("none")), Join::None);
        assert_eq!(Join::of_env(Some("chain")), Join::Global);
    }

    #[test]
    fn the_parent_closes_after_the_last_child_disjoined() {
        let (p, a, b) = (100, 101, 102);
        let group = VplGroup::of(Session::fake(p));
        let mut sa = Session::fake(a);
        let mut sb = Session::fake(b);
        assert_eq!(sa.join_to(&group), Ok(true));
        assert_eq!(sb.join_to(&group), Ok(true));
        let oa = Owner::new(sa);
        oa.set(Component::Decoder);
        let kept = Arc::clone(&oa);
        drop(group);
        drop(oa);
        assert_eq!(fake::took(&[p, a, b]), [(Op::Join, a), (Op::Join, b)], "a child holds it");
        drop(kept);
        assert_eq!(
            fake::took(&[p, a, b]),
            [(Op::CloseComponent, a), (Op::Disjoin, a), (Op::Close, a)]
        );
        drop(sb);
        assert_eq!(fake::took(&[p, a, b]), [(Op::Disjoin, b), (Op::Close, b), (Op::Close, p)]);
    }

    #[test]
    fn a_producer_closes_after_the_consumer_that_read_its_surfaces() {
        let (p, c) = (130, 131);
        let producer = Owner::new(Session::fake(p));
        producer.set(Component::Decoder);
        let consumer = Owner::new(Session::fake(c));
        consumer.set(Component::Vpp);
        consumer.keep(&producer);
        consumer.keep(&producer);
        consumer.keep(&consumer);
        assert_eq!(consumer.kept(), 1, "once each, never itself");
        drop(producer);
        assert_eq!(fake::took(&[p, c]), [], "the consumer holds it open");
        drop(consumer);
        assert_eq!(
            fake::took(&[p, c]),
            [(Op::CloseComponent, c), (Op::Close, c), (Op::CloseComponent, p), (Op::Close, p)],
            "the consumer's tasks are done before the producer's pool goes"
        );
    }

    #[test]
    fn settling_lets_earlier_producers_go_and_keeps_the_current() {
        let (old, cur, c) = (140, 141, 142);
        let (o, k) = (Owner::new(Session::fake(old)), Owner::new(Session::fake(cur)));
        let consumer = Owner::new(Session::fake(c));
        consumer.keep(&o);
        consumer.keep(&k);
        drop(o);
        assert_eq!(fake::took(&[old, cur, c]), []);
        consumer.settle(Some(&k));
        assert_eq!(fake::took(&[old, cur, c]), [(Op::Close, old)]);
        assert_eq!(consumer.kept(), 1);
        consumer.settle(None);
        assert_eq!(consumer.kept(), 0);
        assert_eq!(fake::took(&[old, cur, c]), [], "the current one is still held by its element");
        drop(k);
        drop(consumer);
        assert_eq!(fake::took(&[old, cur, c]), [(Op::Close, cur), (Op::Close, c)]);
    }

    #[test]
    fn a_session_never_joined_closes_without_a_disjoin() {
        let id = 110;
        drop(Session::fake(id));
        assert_eq!(fake::took(&[id]), [(Op::Close, id)]);
    }

    #[test]
    fn a_busy_join_keeps_the_session_for_the_next_try() {
        let (p, c) = (120, 121);
        let group = VplGroup::of(Session::fake(p));
        let mut j = Joining::default();
        fake::busy_joins(2);
        let mut made = 0;
        let mut make = || {
            made += 1;
            Ok(Session::fake(c))
        };
        assert!(matches!(j.open(Some(&group), &mut make), Ok(None)));
        assert!(j.pending());
        assert!(matches!(j.open(Some(&group), &mut make), Ok(None)));
        let s = j.open(Some(&group), &mut make).expect("joins").expect("the third try joins");
        assert!(!j.pending() && made == 1, "one session made over three tries");
        drop(s);
        assert_eq!(
            fake::took(&[p, c]),
            [(Op::Join, c), (Op::Join, c), (Op::Join, c), (Op::Disjoin, c), (Op::Close, c)]
        );
        let mut j = Joining::default();
        let s = j.open(None, || Ok(Session::fake(c))).expect("no parent").expect("no join");
        drop(s);
        assert_eq!(fake::took(&[c]), [(Op::Close, c)], "no parent, no join and no disjoin");
        let e = j.open(Some(&group), || {
            Err(crate::error::MfxError { status: ffi::MFX_ERR_MEMORY_ALLOC, call: "x" })
        });
        assert!(e.is_err() && !j.pending(), "a session that cannot be made leaves nothing");
    }
}
