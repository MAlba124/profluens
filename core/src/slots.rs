//! Element ids to reused slots of the pipeline's per-element tables. Ids are never given out twice.

/// What a slot that holds no element reads as its id.
pub(crate) const FREE: u32 = u32::MAX;

#[derive(Default)]
pub(crate) struct Slots {
    map: Vec<(u32, u32)>,
    ids: Vec<u32>,
    free: Vec<u32>,
    next: u32,
}

impl Slots {
    /// A new id and its slot, a free one first, and whether the slot is new.
    pub(crate) fn add(&mut self) -> (u32, usize, bool) {
        let id = self.next;
        self.next += 1;
        let (slot, new) = match self.free.pop() {
            Some(s) => (s, false),
            None => (self.ids.len() as u32, true),
        };
        match new {
            true => self.ids.push(id),
            false => self.ids[slot as usize] = id,
        }
        // Ids only grow, so the map stays sorted by appending.
        self.map.push((id, slot));
        (id, slot as usize, new)
    }

    pub(crate) fn get(&self, id: u32) -> Option<usize> {
        let k = self.map.binary_search_by_key(&id, |x| x.0).ok()?;
        Some(self.map[k].1 as usize)
    }

    pub(crate) fn id(&self, slot: usize) -> u32 {
        self.ids.get(slot).copied().unwrap_or(FREE)
    }

    pub(crate) fn free(&mut self, id: u32) -> Option<usize> {
        let k = self.map.binary_search_by_key(&id, |x| x.0).ok()?;
        let slot = self.map.remove(k).1;
        self.ids[slot as usize] = FREE;
        self.free.push(slot);
        Some(slot as usize)
    }

    pub(crate) fn next(&self) -> u32 {
        self.next
    }

    pub(crate) fn map(&self) -> &[(u32, u32)] {
        &self.map
    }

    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_grow_and_slots_are_reused() {
        let mut s = Slots::default();
        assert_eq!([s.add(), s.add(), s.add()], [(0, 0, true), (1, 1, true), (2, 2, true)]);
        assert_eq!(s.free(1), Some(1));
        assert_eq!(s.free(1), None, "freed once");
        assert_eq!((s.get(1), s.id(1)), (None, FREE));
        assert_eq!(s.add(), (3, 1, false), "a new id in the freed slot");
        assert_eq!((s.get(3), s.get(2), s.id(1), s.next(), s.len()), (Some(1), Some(2), 3, 4, 3));
        assert_eq!(s.map(), [(0, 0), (2, 2), (3, 1)]);
        assert_eq!(s.get(9), None);
    }
}
