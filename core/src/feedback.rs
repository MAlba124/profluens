#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::publish::Snapshot;
use crate::time::Timestamp;

/// What a sink says of its timing, after a frame it rendered or dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Qos {
    /// Nanoseconds past the deadline, zero or less when on time.
    pub lateness: i64,
    /// The earliest running time still worth sending, `ZERO` when all is.
    pub earliest: Timestamp,
}

/// Over what a demuxer read its [`Bitrate`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RateKind {
    /// The whole stream, its length over its duration or its tables summed.
    Average,
    Local,
}

/// What a demuxer says the stream it reads costs, so a source keeps its store in time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bitrate {
    pub bits_per_second: u64,
    pub kind: RateKind,
}

/// One kind of upstream feedback, of a closed set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Feedback {
    Qos(Qos),
    Bitrate(Bitrate),
}

/// The latest of each kind a pad was told, `None` while unset or behind a flush.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Upstream {
    pub qos: Option<Qos>,
    pub bitrate: Option<Bitrate>,
}

impl Upstream {
    pub(crate) fn set(&mut self, feedback: Feedback) {
        match feedback {
            Feedback::Qos(q) => self.qos = Some(q),
            Feedback::Bitrate(b) => self.bitrate = Some(b),
        }
    }

    pub(crate) fn news(&self, old: &Upstream) -> impl Iterator<Item = Feedback> + use<> {
        let qos = self.qos.filter(|q| old.qos != Some(*q)).map(Feedback::Qos);
        let bitrate = self.bitrate.filter(|b| old.bitrate != Some(*b)).map(Feedback::Bitrate);
        qos.into_iter().chain(bitrate)
    }
}

// The kinds set, in the second word.
const QOS: u64 = 1;
const BITRATE: u64 = 2;
const LOCAL: u64 = 4;

const WORDS: usize = 5;

/// A link's slot, written by its consumer, read by its producer and gone with the link.
#[derive(Default)]
pub(crate) struct Slot {
    snapshot: Snapshot<WORDS>,
    sealed: AtomicBool,
}

impl Slot {
    pub(crate) fn write(&self, up: &Upstream, seek_gen: u64) {
        let q = up.qos.unwrap_or(Qos { lateness: 0, earliest: Timestamp::ZERO });
        let b = up.bitrate.unwrap_or(Bitrate { bits_per_second: 0, kind: RateKind::Average });
        let local = if b.kind == RateKind::Local { LOCAL } else { 0 };
        let set = if up.qos.is_some() { QOS } else { 0 }
            | if up.bitrate.is_some() { BITRATE | local } else { 0 };
        self.snapshot.write([seek_gen, set, q.lateness as u64, q.earliest.0, b.bits_per_second]);
    }

    pub(crate) fn version(&self) -> u64 {
        self.snapshot.version()
    }

    /// What a producer at `seek_gen` is told and the version. A write of another seek tells
    /// nothing.
    pub(crate) fn read(&self, seek_gen: u64) -> (u64, Upstream) {
        let (v, [made_in, set, lateness, earliest, bps]) = self.snapshot.read();
        let qos = Qos { lateness: lateness as i64, earliest: Timestamp(earliest) };
        let kind = if set & LOCAL != 0 { RateKind::Local } else { RateKind::Average };
        let bitrate = Bitrate { bits_per_second: bps, kind };
        let mine = made_in == seek_gen;
        (
            v,
            Upstream {
                qos: (mine && set & QOS != 0).then_some(qos),
                bitrate: (mine && set & BITRATE != 0).then_some(bitrate),
            },
        )
    }

    pub(crate) fn seal(&self) {
        self.sealed.store(true, Ordering::Release);
    }

    pub(crate) fn sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }
}

pub(crate) const UNSEEN: u64 = 1;

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    const LATE: Qos = Qos { lateness: 40_000_000, earliest: Timestamp(2_000_000_000) };

    #[test]
    fn the_latest_write_wins_and_a_read_moves_nothing() {
        let slot = Slot::default();
        assert_eq!(slot.read(0), (0, Upstream::default()));
        let mut up = Upstream::default();
        up.set(Feedback::Qos(Qos { lateness: 1, ..LATE }));
        slot.write(&up, 0);
        up.set(Feedback::Qos(LATE));
        slot.write(&up, 0);
        assert_eq!(slot.read(0), (4, Upstream { qos: Some(LATE), bitrate: None }));
        assert_eq!(slot.version(), 4, "a read is no write");
    }

    #[test]
    fn a_write_of_another_seek_tells_the_producer_nothing() {
        let slot = Slot::default();
        slot.write(&Upstream { qos: Some(LATE), bitrate: None }, 3);
        assert_eq!(slot.read(3).1.qos, Some(LATE));
        assert_eq!(
            slot.read(2).1,
            Upstream::default(),
            "a producer behind the writer waits for its flush"
        );
        assert_eq!(slot.read(4).1, Upstream::default(), "stale lateness never survives a seek");
    }

    #[test]
    fn news_are_the_kinds_that_changed() {
        let (none, late) = (Upstream::default(), Upstream { qos: Some(LATE), bitrate: None });
        assert_eq!(late.news(&none).collect::<Vec<_>>(), [Feedback::Qos(LATE)]);
        assert_eq!(late.news(&late).count(), 0);
        assert_eq!(none.news(&late).count(), 0, "a clear is the flush's, no event");
    }

    const LOCAL_RATE: Bitrate = Bitrate { bits_per_second: 2_500_000, kind: RateKind::Local };

    #[test]
    fn a_bitrate_rides_the_slot_beside_the_qos_and_a_seek_clears_it() {
        let slot = Slot::default();
        let mut up = Upstream { qos: Some(LATE), ..Upstream::default() };
        up.set(Feedback::Bitrate(LOCAL_RATE));
        slot.write(&up, 2);
        assert_eq!(slot.read(2).1, Upstream { qos: Some(LATE), bitrate: Some(LOCAL_RATE) });
        let average = Bitrate { kind: RateKind::Average, ..LOCAL_RATE };
        slot.write(&Upstream { bitrate: Some(average), ..Upstream::default() }, 2);
        assert_eq!(
            slot.read(2).1,
            Upstream { qos: None, bitrate: Some(average) },
            "the kind rides too"
        );
        assert_eq!(slot.read(3).1, Upstream::default(), "a seek's flush tells no old rate");
    }

    #[test]
    fn a_new_rate_is_news_and_the_same_one_is_not() {
        let had = Upstream { bitrate: Some(LOCAL_RATE), ..Upstream::default() };
        let now =
            Upstream { bitrate: Some(Bitrate { bits_per_second: 3_000_000, ..LOCAL_RATE }), ..had };
        assert_eq!(now.news(&had).collect::<Vec<_>>(), [Feedback::Bitrate(now.bitrate.unwrap())]);
        assert_eq!(had.news(&had).count(), 0);
    }

    #[test]
    fn a_seal_is_seen_by_the_other_end() {
        let slot = Slot::default();
        assert!(!slot.sealed());
        slot.seal();
        assert!(slot.sealed());
        assert_ne!(UNSEEN & 1, 0, "odd, so no finished write has it");
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    #[test]
    fn loom_a_producer_that_sees_the_seals_write_sees_the_seal() {
        loom::model(|| {
            let slot = Arc::new(Slot::default());
            let consumer = {
                let slot = Arc::clone(&slot);
                loom::thread::spawn(move || {
                    slot.seal();
                    slot.write(&Upstream::default(), 0);
                })
            };
            if slot.version() == 2 {
                assert!(slot.sealed(), "the seal came before its write");
            }
            consumer.join().unwrap();
            assert_eq!(slot.read(0), (2, Upstream::default()));
        });
    }
}
