//! A list of at most `N` items held inline, for spec-bounded lists such as slice header tables
//! and the planes of a dmabuf frame.

use std::fmt;
use std::ops::Deref;

#[derive(Clone)]
pub struct FixedList<T: Copy + Default, const N: usize> {
    items: [T; N],
    len: usize,
}

impl<T: Copy + Default, const N: usize> FixedList<T, N> {
    /// `false` when full, the item is dropped.
    pub fn push(&mut self, v: T) -> bool {
        let Some(slot) = self.items.get_mut(self.len) else {
            return false;
        };
        *slot = v;
        self.len += 1;
        true
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl<T: Copy + Default, const N: usize> Default for FixedList<T, N> {
    fn default() -> Self {
        FixedList { items: [T::default(); N], len: 0 }
    }
}

impl<T: Copy + Default, const N: usize> Deref for FixedList<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items[..self.len]
    }
}

impl<'a, T: Copy + Default, const N: usize> IntoIterator for &'a FixedList<T, N> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: Copy + Default + fmt::Debug, const N: usize> fmt::Debug for FixedList<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::FixedList;

    #[test]
    fn push_stops_at_capacity() {
        let mut l = FixedList::<u32, 2>::default();
        assert!(l.push(1) && l.push(2));
        assert!(!l.push(3), "a third push fits nowhere");
        assert_eq!(&*l, &[1, 2]);
        l.clear();
        assert!(l.is_empty());
        assert!(l.push(7));
        assert_eq!(&*l, &[7]);
    }
}
