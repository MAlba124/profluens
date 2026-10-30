use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FormatId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FieldId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ValueId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ElementId(pub u32);

/// A source pad by position, descriptor pads first then those added at preroll.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SrcPad(pub u32);

/// A sink pad by position, descriptor pads first then those added at preroll.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SinkPad(pub u32);

impl ElementId {
    /// The producing end of a link, for [`Pipeline::link`](crate::pipeline::Pipeline::link).
    pub const fn src(self, pad: SrcPad) -> (ElementId, SrcPad) {
        (self, pad)
    }

    pub const fn sink(self, pad: SinkPad) -> (ElementId, SinkPad) {
        (self, pad)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LinkId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GroupId(pub u32);

/// A string to dense `u32` interning table, one per domain.
#[derive(Clone)]
pub struct Interner {
    map: HashMap<Box<str>, u32>,
    names: Vec<Box<str>>,
}

impl Interner {
    // Built once at pipeline setup, an empty Vec allocates nothing.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self { map: HashMap::new(), names: Vec::new() }
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.names.len() as u32;
        let boxed: Box<str> = s.into();
        self.names.push(boxed.clone());
        self.map.insert(boxed, id);
        id
    }

    pub fn get(&self, s: &str) -> Option<u32> {
        self.map.get(s).copied()
    }

    pub fn resolve(&self, id: u32) -> Option<&str> {
        self.names.get(id as usize).map(|s| &**s)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_and_dedups() {
        let mut i = Interner::new();
        let a = i.intern("video/raw");
        let b = i.intern("audio/raw");
        let a2 = i.intern("video/raw");
        assert_eq!(a, a2, "same string interns to same id");
        assert_ne!(a, b);
        assert_eq!(i.len(), 2, "duplicate did not grow the table");
    }

    #[test]
    fn ids_are_dense_and_ordered() {
        let mut i = Interner::new();
        assert_eq!(i.intern("a"), 0);
        assert_eq!(i.intern("b"), 1);
        assert_eq!(i.intern("c"), 2);
        assert_eq!(i.intern("b"), 1);
    }

    #[test]
    fn resolves_round_trip() {
        let mut i = Interner::new();
        let id = i.intern("h264/annexb");
        assert_eq!(i.resolve(id), Some("h264/annexb"));
        assert_eq!(i.get("h264/annexb"), Some(id));
        assert_eq!(i.resolve(999), None);
        assert_eq!(i.get("never-interned"), None);
    }

    #[test]
    fn empty_interner() {
        let i = Interner::new();
        assert!(i.is_empty());
        assert_eq!(i.len(), 0);
    }
}
