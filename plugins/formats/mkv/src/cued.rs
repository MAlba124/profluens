//! The cued read of a sparse selection. When Cues place every block (`CueRelativePosition`,
//! RFC 9559 section 5.1.5.1.2.3) the blocks are read where they lie and nothing else is.
//! A block whose stamp or track is not the cue's is dropped. One dropped before any has read
//! says the Cues are untrusted, and the element walks.

use std::collections::VecDeque;

use profluens_core::memory::Memory;

use crate::ebml::{self, id};
use crate::reader::{Cue, cluster_timestamp, cued_block};

pub(crate) const HEAD: u32 = 64;
pub(crate) const BLOCK_FIRST: u32 = 4 << 10;
pub(crate) const BLOCK_MAX: u64 = 1 << 20;
pub(crate) const IN_FLIGHT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ref {
    pub(crate) track: u64,
    pub(crate) time_ns: u64,
    pub(crate) cluster: u64,
    pub(crate) relative: u64,
    pub(crate) duration_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Head {
    Unknown,
    Asked(u64),
    Known { data: u64, tick: Option<i64> },
    Bad,
}

enum Slot {
    Need(u32),
    Asked { tag: u64, len: u32 },
    Ready { mem: Memory, pts_ns: u64, duration_ns: Option<u64> },
    Dropped,
}

#[derive(Clone, Copy)]
enum Proposal {
    Head(usize),
    Block(usize),
}

pub(crate) struct CuedFrame {
    pub(crate) track: u64,
    pub(crate) pts_ns: u64,
    pub(crate) duration_ns: Option<u64>,
    pub(crate) mem: Option<Memory>,
}

pub(crate) struct Cued {
    refs: Vec<Ref>,
    heads: Vec<(u64, Head)>,
    first: usize,
    slots: VecDeque<Slot>,
    next: usize,
    proposal: Option<Proposal>,
    scale: u64,
    pub(crate) dropped: u32,
    read: u64,
    untrusted: bool,
    /// The head reads in flight, `(tag, index into heads)`, at most one per slot. `heads` is
    /// one per cued Cluster, a scan of it per served buffer is quadratic on a hostile Cues.
    asked_heads: Vec<(u64, usize)>,
}

impl Cued {
    // Cold path, once per cued read.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn refs(cues: &[Cue], tracks: &[u64]) -> Option<Vec<Ref>> {
        let mut refs = Vec::new();
        for &t in tracks {
            let of = || cues.iter().filter(|c| c.track == t);
            if of().next().is_none() || of().any(|c| c.relative.is_none()) {
                return None;
            }
            refs.extend(of().map(|c| Ref {
                track: t,
                time_ns: c.time_ns,
                cluster: c.byte,
                relative: c.relative.unwrap_or(0),
                duration_ns: c.duration_ns,
            }));
        }
        refs.sort_by_key(|r| (r.cluster, r.relative, r.time_ns));
        refs.dedup_by_key(|r| (r.cluster, r.relative));
        refs.sort_by_key(|r| r.time_ns);
        Some(refs)
    }

    // Cold path, once per cued read.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new(refs: Vec<Ref>, scale: u64, from_ns: u64) -> Cued {
        let mut heads: Vec<(u64, Head)> = refs.iter().map(|r| (r.cluster, Head::Unknown)).collect();
        heads.sort_by_key(|h| h.0);
        heads.dedup_by_key(|h| h.0);
        let mut c = Cued {
            refs,
            heads,
            first: 0,
            slots: VecDeque::with_capacity(IN_FLIGHT),
            next: 0,
            proposal: None,
            scale: scale.max(1),
            dropped: 0,
            read: 0,
            untrusted: false,
            asked_heads: Vec::with_capacity(IN_FLIGHT),
        };
        c.restart(from_ns);
        c
    }

    pub(crate) fn restart(&mut self, from_ns: u64) {
        self.slots.clear();
        self.proposal = None;
        for (_, h) in self.asked_heads.drain(..) {
            self.heads[h].1 = Head::Unknown;
        }
        let up = |r: &Ref| {
            r.time_ns.saturating_add(r.duration_ns.unwrap_or(0)) > from_ns || r.time_ns >= from_ns
        };
        self.next = self.refs.iter().position(up).unwrap_or(self.refs.len());
        self.first = self.next;
    }

    pub(crate) fn untrusted(&self) -> bool {
        self.untrusted
    }

    pub(crate) fn held(&self) -> usize {
        let blocks = self
            .slots
            .iter()
            .filter(|s| matches!(s, Slot::Asked { .. } | Slot::Ready { .. }))
            .count();
        self.asked_heads.len() + blocks
    }

    pub(crate) fn done(&self) -> bool {
        self.next >= self.refs.len() && self.slots.is_empty()
    }

    pub(crate) fn resume_byte(&self) -> Option<u64> {
        self.refs.get(self.first).map(|r| r.cluster)
    }

    pub(crate) fn resume_time(&self) -> Option<u64> {
        self.refs.get(self.first).map(|r| r.time_ns)
    }

    fn head(&self, cluster: u64) -> usize {
        self.heads.partition_point(|h| h.0 < cluster)
    }

    pub(crate) fn want(&mut self) -> Option<(u64, u32)> {
        while self.slots.len() < IN_FLIGHT && self.next < self.refs.len() {
            self.slots.push_back(Slot::Need(BLOCK_FIRST));
            self.next += 1;
        }
        for i in 0..self.slots.len() {
            let Slot::Need(len) = self.slots[i] else {
                continue;
            };
            let r = self.refs[self.first + i];
            let h = self.head(r.cluster);
            match self.heads[h].1 {
                Head::Known { data, .. } => {
                    self.proposal = Some(Proposal::Block(i));
                    return Some((data.saturating_add(r.relative), len));
                }
                Head::Unknown => {
                    self.proposal = Some(Proposal::Head(h));
                    return Some((r.cluster, HEAD));
                }
                Head::Bad => self.slots[i] = Slot::Dropped,
                Head::Asked(_) => {}
            }
        }
        None
    }

    pub(crate) fn asked(&mut self, tag: u64) {
        match self.proposal.take() {
            Some(Proposal::Head(h)) => {
                self.heads[h].1 = Head::Asked(tag);
                self.asked_heads.push((tag, h));
            }
            Some(Proposal::Block(i)) => {
                if let Some(Slot::Need(len)) = self.slots.get(i) {
                    let len = *len;
                    self.slots[i] = Slot::Asked { tag, len };
                }
            }
            None => {}
        }
    }

    pub(crate) fn unasked(&mut self) {
        self.proposal = None;
    }

    pub(crate) fn owns(&self, tag: u64) -> bool {
        self.asked_heads.iter().any(|a| a.0 == tag)
            || self.slots.iter().any(|s| matches!(s, Slot::Asked { tag: t, .. } if *t == tag))
    }

    pub(crate) fn served(&mut self, tag: u64, mem: Memory) {
        if let Some(i) = self.asked_heads.iter().position(|a| a.0 == tag) {
            let (_, h) = self.asked_heads.swap_remove(i);
            self.heads[h].1 = head_of(self.heads[h].0, mem.data());
            return;
        }
        let Some(i) =
            self.slots.iter().position(|s| matches!(s, Slot::Asked { tag: t, .. } if *t == tag))
        else {
            return;
        };
        let Slot::Asked { len, .. } = self.slots[i] else {
            return;
        };
        let r = self.refs[self.first + i];
        let tick = match self.heads[self.head(r.cluster)].1 {
            Head::Known { tick, .. } => tick,
            _ => None,
        };
        self.slots[i] = self.block_of(&r, tick, len, mem);
    }

    fn block_of(&self, r: &Ref, tick: Option<i64>, asked: u32, mem: Memory) -> Slot {
        let data = mem.data();
        let Ok(h) = ebml::read_element_header(data, 0) else {
            return Slot::Dropped;
        };
        let Some(total) = h.size.map(|s| s.saturating_add(h.data_start as u64)) else {
            return Slot::Dropped;
        };
        if total > data.len() as u64 {
            return match (data.len() as u64) < asked as u64 || total > BLOCK_MAX {
                true => Slot::Dropped,
                false => Slot::Need(total as u32),
            };
        }
        let Ok(b) = cued_block(&data[..total as usize], tick.unwrap_or(0), self.scale) else {
            return Slot::Dropped;
        };
        // A position that is off lands on another block, its track or stamp tells.
        let pts_ns = match tick {
            Some(_) if b.pts_ns.abs_diff(r.time_ns) > self.scale => return Slot::Dropped,
            Some(_) => b.pts_ns,
            None => r.time_ns,
        };
        if b.track != r.track {
            return Slot::Dropped;
        }
        match mem.slice(b.payload.start, b.payload.len()) {
            Some(frame) => {
                Slot::Ready { mem: frame, pts_ns, duration_ns: b.duration_ns.or(r.duration_ns) }
            }
            None => Slot::Dropped,
        }
    }

    pub(crate) fn pop(&mut self) -> Option<CuedFrame> {
        if !matches!(self.slots.front()?, Slot::Ready { .. } | Slot::Dropped) {
            return None;
        }
        let r = self.refs[self.first];
        self.first += 1;
        match self.slots.pop_front()? {
            Slot::Ready { mem, pts_ns, duration_ns } => {
                self.read += 1;
                Some(CuedFrame { track: r.track, pts_ns, duration_ns, mem: Some(mem) })
            }
            _ => {
                self.untrusted |= self.read == 0;
                self.dropped += 1;
                Some(CuedFrame {
                    track: r.track,
                    pts_ns: r.time_ns,
                    duration_ns: r.duration_ns,
                    mem: None,
                })
            }
        }
    }
}

fn head_of(at: u64, data: &[u8]) -> Head {
    match ebml::read_element_header(data, 0) {
        Ok(h) if h.id == id::CLUSTER => Head::Known {
            data: at + h.data_start as u64,
            tick: data.get(h.data_start..).and_then(cluster_timestamp),
        },
        _ => Head::Bad,
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use profluens_core::memory::Pool;

    fn file(clusters: &[(u64, &[(i16, &str)])], pad: usize) -> (Vec<u8>, Vec<Cue>) {
        let mut out = vec![0u8; pad];
        let mut cues = Vec::new();
        for (at, blocks) in clusters {
            let mut body = Vec::new();
            ebml::write_uint(&mut body, id::TIMESTAMP, *at);
            ebml::write_binary(&mut body, &[0xEC], &[0; 300]);
            for (rel, text) in blocks.iter() {
                let relative = body.len() as u64;
                let mut block = vec![0x83];
                block.extend_from_slice(&rel.to_be_bytes());
                block.push(0);
                block.extend_from_slice(text.as_bytes());
                let mut group = Vec::new();
                ebml::write_binary(&mut group, id::BLOCK, &block);
                ebml::write_uint(&mut group, id::BLOCK_DURATION, 500);
                ebml::write_binary(&mut body, id::BLOCK_GROUP, &group);
                let time_ns = (*at as i64 + *rel as i64) as u64 * 1_000_000;
                cues.push(Cue {
                    time_ns,
                    track: 3,
                    byte: out.len() as u64,
                    duration_ns: Some(500_000_000),
                    relative: Some(relative),
                });
            }
            ebml::write_binary(&mut out, id::CLUSTER, &body);
        }
        (out, cues)
    }

    fn read(c: &mut Cued, file: &[u8]) -> Vec<(u64, Option<String>)> {
        let pool = Pool::new(1 << 16);
        let mut tag = 0;
        let mut out = Vec::new();
        while !c.done() {
            let mut asked = Vec::new();
            while let Some((offset, len)) = c.want() {
                c.asked(tag);
                asked.push((tag, offset, len));
                tag += 1;
            }
            for (t, offset, len) in asked.into_iter().rev() {
                let from = (offset as usize).min(file.len());
                let bytes = &file[from..(from + len as usize).min(file.len())];
                let mut mem = pool.acquire_exact(len as usize).expect("exact");
                mem.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
                mem.set_len(bytes.len());
                assert!(c.owns(t));
                c.served(t, mem);
            }
            while let Some(f) = c.pop() {
                out.push((f.pts_ns, f.mem.map(|m| String::from_utf8_lossy(m.data()).into_owned())));
            }
        }
        out
    }

    #[test]
    fn blocks_are_read_at_their_cues_in_order() {
        let (f, cues) = file(&[(1_000, &[(0, "a"), (700, "b")]), (5_000, &[(10, "c")])], 100);
        let refs = Cued::refs(&cues, &[3]).expect("all placed");
        let mut c = Cued::new(refs, 1_000_000, 0);
        let got = read(&mut c, &f);
        let ms = 1_000_000;
        let want = [(1_000 * ms, "a"), (1_700 * ms, "b"), (5_010 * ms, "c")];
        assert_eq!(got, want.map(|(t, s)| (t, Some(s.to_string()))));
        assert_eq!(c.dropped, 0);
        c.restart(1_900 * ms);
        assert_eq!(read(&mut c, &f).len(), 2);
        c.restart(2_300 * ms);
        assert_eq!(read(&mut c, &f), [(5_010 * ms, Some("c".to_string()))]);
    }

    #[test]
    fn a_position_that_is_off_drops_its_block_and_a_long_block_reads_whole() {
        let long = "x".repeat(BLOCK_FIRST as usize * 2);
        let (f, mut cues) = file(&[(0, &[(0, "a"), (100, &long), (200, "c")])], 0);
        cues[2].relative = cues[2].relative.map(|r| r + 1);
        let mut c = Cued::new(Cued::refs(&cues, &[3]).expect("placed"), 1_000_000, 0);
        let got = read(&mut c, &f);
        assert_eq!(got[0].1.as_deref(), Some("a"));
        assert_eq!(got[1].1.as_ref().map(|s| s.len()), Some(long.len()), "read again whole");
        assert_eq!(got[2].1, None, "off by one octet");
        assert_eq!(c.dropped, 1);
        assert!(!c.untrusted(), "blocks read before it");
        cues[0].relative = cues[0].relative.map(|r| r + 1);
        let mut c = Cued::new(Cued::refs(&cues, &[3]).expect("placed"), 1_000_000, 0);
        read(&mut c, &f);
        assert!(c.untrusted(), "the first dropped");
    }

    /// Each bogus Cluster costs one head read, and finding the read among the in-flight asks
    /// must not scan every cued Cluster, or a hostile Cues makes the selection quadratic.
    #[test]
    fn a_run_of_cues_at_bogus_clusters_is_dropped_one_head_read_each() {
        let (f, mut cues) = file(&[(0, &[(0, "a")])], 0);
        let n = 50_000u64;
        cues.extend((1..=n).map(|k| Cue {
            time_ns: k * 1_000_000,
            track: 3,
            byte: f.len() as u64 + k * 64,
            duration_ns: None,
            relative: Some(0),
        }));
        let mut c = Cued::new(Cued::refs(&cues, &[3]).expect("placed"), 1_000_000, 0);
        let got = read(&mut c, &f);
        assert_eq!(got.len(), n as usize + 1);
        assert_eq!(got[0].1.as_deref(), Some("a"));
        assert_eq!(c.dropped, n as u32);
        assert!(!c.untrusted(), "the first block read");
        assert_eq!(c.held(), 0, "no head ask left in flight");
    }

    #[test]
    fn a_track_with_a_cue_that_does_not_place_its_block_is_walked() {
        let (_, mut cues) = file(&[(0, &[(0, "a"), (100, "b")])], 0);
        assert!(Cued::refs(&cues, &[3]).is_some());
        assert!(Cued::refs(&cues, &[4]).is_none(), "a track with no cue");
        let twice = cues[0];
        cues.push(twice);
        assert_eq!(Cued::refs(&cues, &[3]).map(|r| r.len()), Some(2), "a block cued twice once");
        cues[1].relative = None;
        assert!(Cued::refs(&cues, &[3]).is_none());
    }
}
