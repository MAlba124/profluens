use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::element::ElementDesc;
use crate::error::{Error, PropError, RejectedValue};
use crate::format::Value;
use crate::id::{ElementId, ValueId};
use crate::publish::Snapshot;

// Tag 0 is a cell never set, as a fresh snapshot reads.
const TAG_INT: u32 = 1;
const TAG_RAT: u32 = 2;
const TAG_ID: u32 = 3;

fn encode(v: Value) -> (u32, u64) {
    match v {
        Value::Int(i) => (TAG_INT, i as u64),
        Value::Rat(n, d) => (TAG_RAT, ((n as u32 as u64) << 32) | d as u32 as u64),
        Value::Id(ValueId(id)) => (TAG_ID, id as u64),
    }
}

fn decode(tag: u32, bits: u64) -> Option<Value> {
    match tag {
        TAG_INT => Some(Value::Int(bits as i64)),
        TAG_RAT => Some(Value::Rat((bits >> 32) as u32 as i32, bits as u32 as i32)),
        TAG_ID => Some(Value::Id(ValueId(bits as u32))),
        _ => None,
    }
}

/// A per-element property mailbox, read by the scheduler at batch boundaries.
pub struct PropTable {
    cells: Box<[Snapshot<2>]>,
    /// Bit per property set since the scheduler last took the mask.
    dirty: AtomicU64,
    writer: Mutex<()>,
}

impl PropTable {
    pub fn new(nprops: usize) -> Self {
        assert!(nprops <= 64, "an element may declare at most 64 properties");
        Self {
            cells: (0..nprops).map(|_| Snapshot::default()).collect(),
            dirty: AtomicU64::new(0),
            writer: Mutex::new(()),
        }
    }

    pub(crate) fn set(&self, idx: usize, v: Value) {
        let _g = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let (tag, bits) = encode(v);
        self.cells[idx].write([tag as u64, bits]);
        self.dirty.fetch_or(1 << idx, Ordering::Release);
    }

    pub(crate) fn preset(&self, idx: usize, v: Value) {
        let _g = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let (tag, bits) = encode(v);
        self.cells[idx].write([tag as u64, bits]);
    }

    pub(crate) fn get(&self, idx: usize) -> Option<Value> {
        let [tag, bits] = self.cells.get(idx)?.read().1;
        decode(tag as u32, bits)
    }

    pub(crate) fn take_dirty(&self) -> u64 {
        if self.dirty.load(Ordering::Relaxed) == 0 {
            0
        } else {
            self.dirty.swap(0, Ordering::Acquire)
        }
    }
}

/// Validate a set against the declared properties and park it. `live_only` refuses structural ones.
pub(crate) fn validate_and_set(
    el: ElementId,
    desc: &'static ElementDesc,
    table: &PropTable,
    prop: &str,
    v: Value,
    live_only: bool,
) -> Result<(), Error> {
    let idx = desc.props.iter().position(|p| p.name == prop).ok_or_else(|| {
        Error::Prop(PropError::UnknownProperty {
            element: el,
            element_name: desc.name,
            prop: prop.into(),
        })
    })?;
    let pd = &desc.props[idx];
    if live_only && !pd.live {
        return Err(Error::Prop(PropError::Structural {
            element: el,
            element_name: desc.name,
            prop: pd.name,
            index: idx,
        }));
    }
    if !pd.allowed.accepts(v) {
        return Err(Error::Prop(PropError::Rejected(
            RejectedValue {
                element: el,
                element_name: desc.name,
                prop: pd.name,
                index: idx,
                value: v,
                allowed: pd.allowed.clone(),
            }
            .into(),
        )));
    }
    table.set(idx, v);
    Ok(())
}

/// A cloneable handle setting live properties on a running pipeline from another thread.
#[derive(Clone)]
pub struct PropHandle {
    entries: crate::counters::Roster,
    wake: Arc<crate::command::Control>,
}

impl PropHandle {
    pub(crate) fn new(
        entries: crate::counters::Roster,
        wake: Arc<crate::command::Control>,
    ) -> Self {
        Self { entries, wake }
    }

    /// Validate and park a live-property set without blocking.
    pub fn set(&self, el: ElementId, prop: &str, v: Value) -> Result<(), Error> {
        let found =
            crate::counters::row(&self.entries, el, |row| (row.desc, Arc::clone(&row.props)));
        let (desc, table) = found.ok_or(Error::Prop(PropError::UnknownElement { element: el }))?;
        validate_and_set(el, desc, &table, prop, v, true)?;
        self.wake.signal_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Value;

    #[test]
    fn encode_decode_roundtrip() {
        for v in [
            Value::Int(0),
            Value::Int(-1),
            Value::Int(i64::MAX),
            Value::Int(i64::MIN),
            Value::Rat(30000, 1001),
            Value::Rat(-3, 4),
            Value::Rat(i32::MIN, i32::MAX),
            Value::Id(ValueId(0)),
            Value::Id(ValueId(u32::MAX)),
        ] {
            let (tag, bits) = encode(v);
            assert_eq!(decode(tag, bits), Some(v), "{v:?} must survive the cell");
        }
        assert_eq!(decode(0, 0), None);
    }

    #[test]
    fn unset_reads_none_then_set_reads_back() {
        let t = PropTable::new(2);
        assert_eq!(t.get(0), None);
        assert_eq!(t.get(1), None);
        t.set(1, Value::Int(42));
        assert_eq!(t.get(0), None, "sibling slot untouched");
        assert_eq!(t.get(1), Some(Value::Int(42)));
        t.set(1, Value::Rat(1, 2));
        assert_eq!(t.get(1), Some(Value::Rat(1, 2)), "overwrite wins");
    }

    #[test]
    fn dirty_mask_tracks_which_and_clears_on_take() {
        let t = PropTable::new(3);
        assert_eq!(t.take_dirty(), 0, "clean at rest");
        t.set(0, Value::Int(1));
        t.set(2, Value::Int(2));
        assert_eq!(t.take_dirty(), 0b101, "bits name the changed props");
        assert_eq!(t.take_dirty(), 0, "take clears");
        t.set(1, Value::Int(3));
        assert_eq!(t.take_dirty(), 0b010);
    }

    #[test]
    fn out_of_range_index_reads_none() {
        let t = PropTable::new(1);
        assert_eq!(t.get(5), None);
    }

    #[test]
    fn concurrent_reader_always_sees_a_written_value() {
        let t = Arc::new(PropTable::new(1));
        let a = Value::Int(-1);
        let b = Value::Rat(7, 9);
        t.set(0, a);
        let w = {
            let t = Arc::clone(&t);
            std::thread::spawn(move || {
                for i in 0..20_000u32 {
                    t.set(0, if i & 1 == 0 { b } else { a });
                }
            })
        };
        for _ in 0..20_000 {
            let v = t.get(0).expect("set at least once");
            assert!(v == a || v == b, "torn read: {v:?}");
        }
        w.join().unwrap();
    }
}
