//! The pass trace of a run, a ring of the newest rows plus a digest over every row.

use core::fmt;

use super::model::Answer;
use super::verdict::Waits;
use crate::time::{NONE, Ns};

pub const TRACE_ROWS: usize = 1024;

pub const AUTO: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Did {
    Pass { group: u16, waits: Waits, failed: bool },
    Top { group: u16 },
    Jump { to: Ns },
    Sent { action: u16, answer: Answer },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceRow {
    pub step: u32,
    pub did: Did,
}

const EMPTY: TraceRow = TraceRow { step: 0, did: Did::Jump { to: 0 } };

fn words(row: &TraceRow) -> (u64, u64) {
    let (tag, a, b) = match row.did {
        Did::Pass { group, waits, failed } => {
            let (code, value) = match waits {
                Waits::Unrun => (0, 0),
                Waits::Progressed => (1, 0),
                Waits::Idle { until } => (2, until),
                Waits::BlockedRing { pad } => (3, pad as u64),
                Waits::BlockedPool { element } => (4, element as u64),
                Waits::BlockedIo => (5, 0),
                Waits::Exited => (6, 0),
            };
            (1u64, (group as u64) << 16 | code << 8 | failed as u64, value)
        }
        Did::Jump { to } => (2, 0, to),
        Did::Top { group } => (4, group as u64, 0),
        Did::Sent { action, answer } => (3, action as u64, answer as u64),
    };
    ((row.step as u64) << 32 | tag << 24 | a, b)
}

pub struct Trace {
    ring: Box<[TraceRow]>,
    count: u64,
    digest: u64,
}

impl Trace {
    pub fn new() -> Trace {
        Trace { ring: vec![EMPTY; TRACE_ROWS].into_boxed_slice(), count: 0, digest: 0 }
    }

    pub fn reset(&mut self) {
        // The FNV-1a offset basis (Fowler, Noll, Vo).
        (self.count, self.digest) = (0, 0xCBF2_9CE4_8422_2325);
    }

    #[inline]
    pub fn push(&mut self, step: u32, did: Did) {
        const PRIME: u64 = 0x0000_0100_0000_01B3;
        let row = TraceRow { step, did };
        let (a, b) = words(&row);
        self.digest = (self.digest ^ a).wrapping_mul(PRIME);
        self.digest = (self.digest ^ b).wrapping_mul(PRIME);
        let at = self.count as usize & (TRACE_ROWS - 1);
        if let Some(slot) = self.ring.get_mut(at) {
            *slot = row;
        }
        self.count += 1;
    }

    pub fn digest(&self) -> u64 {
        self.digest
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn tail(&self, n: usize) -> impl Iterator<Item = &TraceRow> {
        let kept = (self.count as usize).min(TRACE_ROWS).min(n);
        let first = self.count as usize - kept;
        (first..first + kept).filter_map(|i| self.ring.get(i & (TRACE_ROWS - 1)))
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TraceRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "step {:6} ", self.step)?;
        match self.did {
            Did::Pass { group, waits, failed: false } => write!(f, "pass group {group}, {waits}"),
            Did::Pass { group, .. } => write!(f, "pass group {group}, an element failed"),
            Did::Top { group } => write!(f, "pass group {group} up to its cut"),
            Did::Jump { to: NONE } => f.write_str("jump to no time"),
            Did::Jump { to } => write!(f, "jump to clock {to}ns"),
            Did::Sent { action: AUTO, answer } => {
                write!(f, "resume by the driver, {}", answer.name())
            }
            Did::Sent { action, answer } => write!(f, "action {action}, {}", answer.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(group: u16) -> Did {
        Did::Pass { group, waits: Waits::Progressed, failed: false }
    }

    #[test]
    fn the_digest_sees_order_and_every_field() {
        let digest = |rows: &[(u32, Did)]| {
            let mut trace = Trace::new();
            trace.reset();
            for (step, did) in rows {
                trace.push(*step, *did);
            }
            trace.digest()
        };
        let base = [(1, pass(0)), (2, Did::Jump { to: 5 })];
        assert_eq!(digest(&base), digest(&base));
        assert_ne!(digest(&base), digest(&[base[1], base[0]]));
        let idle = Did::Pass { group: 0, waits: Waits::Idle { until: 9 }, failed: false };
        let variants = [
            (2, pass(0)),
            (1, pass(1)),
            (1, idle),
            (1, Did::Pass { group: 0, waits: Waits::Progressed, failed: true }),
            (1, Did::Sent { action: 0, answer: Answer::Taken }),
            (1, Did::Sent { action: 0, answer: Answer::Redundant }),
            (1, Did::Sent { action: AUTO, answer: Answer::Taken }),
            (1, Did::Top { group: 0 }),
        ];
        for (n, variant) in variants.iter().enumerate() {
            assert_ne!(digest(&base), digest(&[*variant, base[1]]), "variant {n}");
        }
    }

    #[test]
    fn the_ring_keeps_the_newest_rows_in_order() {
        let mut trace = Trace::new();
        trace.reset();
        for step in 0..TRACE_ROWS as u32 + 10 {
            trace.push(step, pass(0));
        }
        assert_eq!(trace.count(), TRACE_ROWS as u64 + 10);
        let tail: Vec<u32> = trace.tail(3).map(|row| row.step).collect();
        assert_eq!(tail, [TRACE_ROWS as u32 + 7, TRACE_ROWS as u32 + 8, TRACE_ROWS as u32 + 9]);
        assert_eq!(trace.tail(usize::MAX).count(), TRACE_ROWS);
        trace.reset();
        assert_eq!(trace.tail(8).count(), 0);
        assert!(format!("{}", TraceRow { step: 3, did: pass(2) }).contains("pass group 2"));
    }
}
