use crate::id::FormatId;
use crate::memory::{Memory, SyncPoint};
use crate::time::Timestamp;

pub struct Buffer {
    pub memory: Memory,
    pub pts: Timestamp,
    pub dts: Timestamp,
    pub duration: Timestamp,
    pub flags: BufferFlags,
    pub format: FormatId,
    /// `None` for system memory.
    pub sync: Option<SyncPoint>,
}

// A batch row is a whole Buffer, kept at one cache line.
const _: () = assert!(core::mem::size_of::<Buffer>() == 64);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct BufferFlags(pub u32);

impl BufferFlags {
    pub const KEYFRAME: Self = Self(1 << 0);
    pub const DISCONT: Self = Self(1 << 1);
    pub const GAP: Self = Self(1 << 2);
    /// Explicitly not a random-access point, untagged flags mean unknown.
    pub const DELTA: Self = Self(1 << 3);
    /// The first bytes of a media segment from an adaptive source.
    pub const SEGMENT: Self = Self(1 << 4);
    /// Decoded for the units that refer to it and not presented.
    pub const DECODE_ONLY: Self = Self(1 << 5);
    /// The buffer is a codec head, not media.
    pub const HEADER: Self = Self(1 << 6);
    /// The first picture decoded behind a preview seek, shown until the one at the point comes.
    pub const PREVIEW: Self = Self(1 << 7);
    /// A decoded picture its source coded without reference to another, most often a scene
    /// change, where an encoder after the decoder does well to start a GOP.
    pub const INTRA: Self = Self(1 << 8);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl core::ops::BitOr for BufferFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl core::ops::BitOrAssign for BufferFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.insert(rhs);
    }
}

impl core::ops::BitAnd for BufferFlags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        self.intersection(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_clear() {
        let mut f = BufferFlags::empty();
        assert!(f.is_empty());
        f.insert(BufferFlags::KEYFRAME);
        assert!(f.contains(BufferFlags::KEYFRAME));
        assert!(!f.contains(BufferFlags::GAP));
        f |= BufferFlags::GAP;
        assert!(f.contains(BufferFlags::KEYFRAME));
        assert!(f.contains(BufferFlags::GAP));
        f.remove(BufferFlags::KEYFRAME);
        assert!(!f.contains(BufferFlags::KEYFRAME));
        assert!(f.contains(BufferFlags::GAP));
    }

    #[test]
    fn union_and_intersection() {
        let a = BufferFlags::KEYFRAME | BufferFlags::DISCONT;
        assert!(a.contains(BufferFlags::KEYFRAME));
        assert!(a.contains(BufferFlags::DISCONT));
        assert_eq!(a, BufferFlags::KEYFRAME.union(BufferFlags::DISCONT));
        assert_eq!(a & BufferFlags::KEYFRAME, BufferFlags::KEYFRAME);
    }

    #[test]
    fn contains_empty_is_always_true() {
        assert!(BufferFlags::KEYFRAME.contains(BufferFlags::empty()));
        assert!(BufferFlags::empty().contains(BufferFlags::empty()));
    }

    #[test]
    fn default_is_empty() {
        assert!(BufferFlags::default().is_empty());
    }
}
