// Verified copy of core/src/slots.rs, run with core/verify/run.sh. Keep it in step with
// slots.rs. `binary_search_by_key` has no Verus spec, so `find` is the same search by hand.
use vstd::prelude::*;

verus! {

const FREE: u32 = u32::MAX;

struct Slots {
    map: Vec<(u32, u32)>,
    ids: Vec<u32>,
    free: Vec<u32>,
    next: u32,
}

impl Slots {
    /// The table invariant: `map` is sorted by id and names the slot each live id holds,
    /// `ids` reads back the live id of each slot or FREE, and `free` lists every FREE slot once.
    spec fn wf(&self) -> bool {
        &&& self.next < FREE
        &&& self.ids@.len() <= self.next
        &&& forall|i: int, j: int| #![trigger self.map@[i], self.map@[j]]
            0 <= i < j < self.map@.len() ==> self.map@[i].0 < self.map@[j].0
        &&& forall|i: int| #![trigger self.map@[i]] 0 <= i < self.map@.len() ==> {
            &&& self.map@[i].0 < self.next
            &&& self.map@[i].1 < self.ids@.len()
            &&& self.ids@[self.map@[i].1 as int] == self.map@[i].0
        }
        &&& forall|s: int| #![trigger self.ids@[s]] 0 <= s < self.ids@.len() && self.ids@[s] != FREE
            ==> exists|i: int| 0 <= i < self.map@.len()
                && #[trigger] self.map@[i] == (self.ids@[s], s as u32)
        &&& forall|k: int| #![trigger self.free@[k]] 0 <= k < self.free@.len()
            ==> self.free@[k] < self.ids@.len() && self.ids@[self.free@[k] as int] == FREE
        &&& forall|k: int, l: int| #![trigger self.free@[k], self.free@[l]]
            0 <= k < l < self.free@.len() ==> self.free@[k] != self.free@[l]
        &&& forall|s: int| #![trigger self.ids@[s]] 0 <= s < self.ids@.len() && self.ids@[s] == FREE
            ==> self.free@.contains(s as u32)
    }

    /// `id` holds `slot`.
    spec fn holds(&self, id: u32, slot: u32) -> bool {
        exists|i: int| 0 <= i < self.map@.len() && #[trigger] self.map@[i] == (id, slot)
    }

    spec fn live(&self, id: u32) -> bool {
        exists|i: int| 0 <= i < self.map@.len() && #[trigger] self.map@[i].0 == id
    }

    fn new() -> (s: Self)
        ensures s.wf(), s.next == 0, s.map@.len() == 0,
    {
        Slots { map: Vec::new(), ids: Vec::new(), free: Vec::new(), next: 0 }
    }

    /// A new id and its slot, a free one first, and whether the slot is new.
    fn add(&mut self) -> (r: (u32, usize, bool))
        requires old(self).wf(), old(self).next < FREE - 1,
        ensures
            final(self).wf(),
            // never given out before: every id handed out so far is below the old next
            r.0 == old(self).next, final(self).next == r.0 + 1,
            forall|i: int| 0 <= i < old(self).map@.len() ==> old(self).map@[i].0 < r.0,
            final(self).holds(r.0, r.1 as u32),
            r.2 == (r.1 == old(self).ids@.len()),
            !r.2 ==> old(self).ids@[r.1 as int] == FREE,
            forall|id: u32, s: u32| old(self).holds(id, s) ==> final(self).holds(id, s),
            forall|id: u32| #[trigger] final(self).live(id) ==> old(self).live(id) || id == r.0,
    {
        let id = self.next;
        self.next = self.next + 1;
        let ghost old_free = self.free@;
        let (slot, new) = match self.free.pop() {
            Some(s) => (s, false),
            None => (self.ids.len() as u32, true),
        };
        let ghost mid_ids = self.ids@;
        proof {
            if !new {
                assert(old_free.len() > 0 && slot == old_free[old_free.len() - 1]);
                assert(mid_ids[slot as int] == FREE);
            }
        }
        match new {
            true => self.ids.push(id),
            false => self.ids.set(slot as usize, id),
        }
        // Ids only grow, so the map stays sorted by appending.
        let ghost old_map = self.map@;
        self.map.push((id, slot));
        proof {
            let (map, ids, free) = (self.map@, self.ids@, self.free@);
            assert(map[map.len() - 1] == (id, slot));
            assert forall|i: int| 0 <= i < map.len() implies map[i].1 < ids.len()
                && ids[map[i].1 as int] == map[i].0 by {
                if i < old_map.len() {
                    assert(old_map[i] == map[i]);
                    // an old live slot is not the one taken, that one was FREE
                    assert(mid_ids[old_map[i].1 as int] == old_map[i].0);
                    assert(old_map[i].0 != FREE);
                    assert(old_map[i].1 != slot);
                }
            }
            assert forall|s: int| 0 <= s < ids.len() && ids[s] != FREE
                implies exists|i: int| 0 <= i < map.len() && #[trigger] map[i] == (ids[s], s as u32)
            by {
                if s == slot as int {
                    assert(map[map.len() - 1] == (ids[s], s as u32));
                } else {
                    assert(mid_ids[s] == ids[s]);
                    let i = choose|i: int| 0 <= i < old_map.len()
                        && #[trigger] old_map[i] == (mid_ids[s], s as u32);
                    assert(map[i] == old_map[i]);
                }
            }
            assert forall|k: int| 0 <= k < free.len() implies free[k] < ids.len()
                && ids[free[k] as int] == FREE by {
                assert(old_free[k] == free[k]);
                if !new {
                    assert(old_free[k] != old_free[old_free.len() - 1]);
                }
            }
            assert forall|s: int| 0 <= s < ids.len() && ids[s] == FREE
                implies free.contains(s as u32) by {
                assert(s != slot as int);
                assert(mid_ids[s] == FREE);
                let k = choose|k: int| 0 <= k < old_free.len() && old_free[k] == s as u32;
                if !new {
                    assert(k != old_free.len() - 1);
                }
                assert(free[k] == s as u32);
            }
            assert forall|id2: u32, s2: u32| old(self).holds(id2, s2) implies self.holds(id2, s2)
            by {
                let i = choose|i: int| 0 <= i < old_map.len() && #[trigger] old_map[i] == (id2, s2);
                assert(map[i] == old_map[i]);
            }
            assert forall|id2: u32| #[trigger] self.live(id2) implies old(self).live(id2) || id2 == id
            by {
                let i = choose|i: int| 0 <= i < map.len() && #[trigger] map[i].0 == id2;
                if i < old_map.len() {
                    assert(old_map[i].0 == id2);
                }
            }
        }
        (id, slot as usize, new)
    }

    /// Where `id` sits in `map`, by binary search.
    fn find(&self, id: u32) -> (r: Option<usize>)
        requires self.wf(),
        ensures
            match r {
                Some(k) => k < self.map@.len() && self.map@[k as int].0 == id,
                None => !self.live(id),
            },
    {
        let (mut lo, mut hi) = (0usize, self.map.len());
        while lo < hi
            invariant
                self.wf(), lo <= hi <= self.map@.len(),
                forall|i: int| 0 <= i < lo ==> self.map@[i].0 < id,
                forall|i: int| hi <= i < self.map@.len() ==> self.map@[i].0 > id,
            decreases hi - lo,
        {
            let mid = lo + (hi - lo) / 2;
            let k = self.map[mid].0;
            if k == id {
                return Some(mid);
            } else if k < id {
                proof {
                    assert forall|i: int| 0 <= i <= mid implies self.map@[i].0 < id by {
                        if i < mid as int {
                            assert(self.map@[i].0 < self.map@[mid as int].0);
                        }
                    }
                }
                lo = mid + 1;
            } else {
                proof {
                    assert forall|i: int| mid <= i < self.map@.len() implies self.map@[i].0 > id
                    by {
                        if i > mid as int {
                            assert(self.map@[mid as int].0 < self.map@[i].0);
                        }
                    }
                }
                hi = mid;
            }
        }
        proof {
            assert forall|i: int| 0 <= i < self.map@.len() implies #[trigger] self.map@[i].0 != id
            by {}
        }
        None
    }

    fn get(&self, id: u32) -> (r: Option<usize>)
        requires self.wf(),
        ensures
            match r {
                Some(s) => self.holds(id, s as u32) && self.ids@[s as int] == id,
                None => !self.live(id),
            },
    {
        let k = self.find(id)?;
        proof {
            let e = self.map@[k as int];
            assert(e == (id, e.1));
            assert(self.holds(id, e.1));
            assert(self.ids@[e.1 as int] == e.0);
        }
        Some(self.map[k].1 as usize)
    }

    fn id(&self, slot: usize) -> (r: u32)
        requires self.wf(),
        ensures
            slot < self.ids@.len() ==> r == self.ids@[slot as int],
            slot >= self.ids@.len() ==> r == FREE,
            r != FREE ==> self.holds(r, slot as u32),
    {
        let r = match self.ids.get(slot) {
            Some(v) => *v,
            None => FREE,
        };
        r
    }

    fn free(&mut self, id: u32) -> (r: Option<usize>)
        requires old(self).wf(),
        ensures
            final(self).wf(),
            final(self).next == old(self).next,
            match r {
                Some(s) => old(self).holds(id, s as u32) && !final(self).live(id)
                    && final(self).ids@[s as int] == FREE && final(self).free@.contains(s as u32),
                None => !old(self).live(id) && final(self).map@ == old(self).map@
                    && final(self).ids@ == old(self).ids@ && final(self).free@ == old(self).free@,
            },
            forall|other: u32, s: u32| other != id && old(self).holds(other, s) ==> final(self).holds(other, s),
    {
        let k = self.find(id)?;
        let ghost (old_map, old_ids, old_free) = (self.map@, self.ids@, self.free@);
        let slot = self.map.remove(k).1;
        self.ids.set(slot as usize, FREE);
        self.free.push(slot);
        proof {
            let (map, ids, free) = (self.map@, self.ids@, self.free@);
            let ki = k as int;
            assert(old_map[ki] == (id, slot));
            assert forall|i: int| 0 <= i < map.len() implies map[i] == old_map[if i < ki { i } else { i + 1 }]
                by {}
            // the freed slot was held by id alone, so no other entry names it
            assert forall|i: int| 0 <= i < map.len() implies map[i].1 != slot by {
                let j = if i < ki { i } else { i + 1 };
                assert(old_ids[old_map[j].1 as int] == old_map[j].0);
                assert(old_map[j].0 != id);
            }
            assert forall|i: int, j: int| 0 <= i < j < map.len() implies map[i].0 < map[j].0 by {
                let (oi, oj) = (if i < ki { i } else { i + 1 }, if j < ki { j } else { j + 1 });
                assert(map[i] == old_map[oi] && map[j] == old_map[oj]);
            }
            assert forall|i: int| 0 <= i < map.len() implies map[i].0 < self.next
                && map[i].1 < ids.len() && ids[map[i].1 as int] == map[i].0 by {
                let j = if i < ki { i } else { i + 1 };
                assert(map[i] == old_map[j]);
            }
            assert forall|s: int| 0 <= s < ids.len() && ids[s] != FREE
                implies exists|i: int| 0 <= i < map.len() && #[trigger] map[i] == (ids[s], s as u32)
            by {
                assert(s != slot as int);
                let j = choose|j: int| 0 <= j < old_map.len()
                    && #[trigger] old_map[j] == (old_ids[s], s as u32);
                assert(j != ki);
                let i = if j < ki { j } else { j - 1 };
                assert(map[i] == old_map[j]);
            }
            assert forall|kk: int| 0 <= kk < free.len() implies free[kk] < ids.len()
                && ids[free[kk] as int] == FREE by {
                if kk < old_free.len() {
                    assert(free[kk] == old_free[kk]);
                }
            }
            assert forall|a: int, b: int| 0 <= a < b < free.len() implies free[a] != free[b] by {
                if b == free.len() - 1 {
                    // slot was held, so it was not FREE and not in the old free list
                    assert(old_ids[slot as int] == id);
                    assert(old_free[a] == free[a]);
                }
            }
            assert forall|s: int| 0 <= s < ids.len() && ids[s] == FREE implies free.contains(s as u32)
            by {
                if s == slot as int {
                    assert(free[free.len() - 1] == slot);
                } else {
                    let kk = choose|kk: int| 0 <= kk < old_free.len() && old_free[kk] == s as u32;
                    assert(free[kk] == old_free[kk]);
                }
            }
            assert forall|i: int| 0 <= i < map.len() implies #[trigger] map[i].0 != id by {
                let j = if i < ki { i } else { i + 1 };
                assert(map[i] == old_map[j]);
            }
            assert forall|other: u32, s: u32| other != id && old(self).holds(other, s)
                implies self.holds(other, s) by {
                let j = choose|j: int| 0 <= j < old_map.len() && #[trigger] old_map[j] == (other, s);
                assert(j != ki);
                let i = if j < ki { j } else { j - 1 };
                assert(map[i] == old_map[j]);
            }
            assert(old(self).holds(id, slot));
            assert(free[free.len() - 1] == slot);
        }
        Some(slot as usize)
    }

    fn next(&self) -> u32 {
        self.next
    }

    fn len(&self) -> usize {
        self.ids.len()
    }
}

fn main() {}

} // verus!
