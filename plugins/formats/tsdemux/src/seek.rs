//! Where a seek lands in a transport stream read by range, by bisecting the decode stamps of the
//! key stream's PES packets. The landing is the last packet of the key stream starting a PES
//! packet at or before the target, between the index entries around it.

use pf_mpegts::packet::{PACKET_LEN, PF_PAYLOAD, PF_UNIT_START, parse_packet, payload};
use pf_mpegts::pes::parse_pes_header;
use pf_mpegts::sync::{self, PacketSize, framed_len, only, sync_offset};

/// The bytes of a probe, a few hundred packets.
pub const PROBE_LEN: u32 = 64 << 10;
/// Probes a seek takes at most before landing on the best packet found.
pub const PROBES_MAX: u32 = 48;
/// Sync bytes in a row that frame a probe, the demuxer's default.
const CONFIRM: usize = pf_mpegts::SYNC_CONFIRM_DEFAULT as usize;

const MODULUS: i64 = 1 << 33;

/// Maps a raw 33-bit stamp to 90 kHz ticks since the file's first, the congruent value at or past
/// `slack` ticks ahead of the start, so a file starting near the wrap reads on across it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base {
    pub origin: i64,
    pub slack: i64,
}

impl Base {
    pub fn ticks(&self, raw: u64) -> i64 {
        let floor = self.origin - self.slack;
        floor + (raw as i64 - floor).rem_euclid(MODULUS) - self.origin
    }
}

/// A PES packet start of the key stream, the byte of its transport packet and its decode time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landing {
    pub at: u64,
    pub ticks: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Probe { at: u64, len: u32 },
    Land(Option<Landing>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bisect {
    base: Base,
    /// The lower bound it started from, where a target ahead of every unit in bounds lands.
    floor: u64,
    pid: u16,
    size: PacketSize,
    target: i64,
    len: u64,
    lo: u64,
    hi: u64,
    best: Option<Landing>,
    probes: u32,
}

impl Bisect {
    /// The landing for `target` ticks since the file's first, on PID `pid`, between bytes `lo`
    /// and `hi` of a file `len` bytes long.
    pub fn new(
        base: Base,
        pid: u16,
        size: PacketSize,
        target: i64,
        bounds: (u64, u64),
        len: u64,
    ) -> Self {
        let (lo, hi) = (bounds.0.min(len), bounds.1.min(len));
        let hi = hi.max(lo);
        Bisect { base, floor: lo, pid, size, target, len, lo, hi, best: None, probes: 0 }
    }

    pub fn floor(&self) -> u64 {
        self.floor
    }

    pub fn probes(&self) -> u32 {
        self.probes
    }

    pub fn step(&self) -> Step {
        if self.lo >= self.hi || self.probes >= PROBES_MAX {
            return Step::Land(self.best);
        }
        let probe = u64::from(PROBE_LEN);
        // a packet starting just short of `hi` and the packets that confirm its sync
        let over = (CONFIRM * framed_len(self.size)) as u64;
        let (at, len) = match self.hi - self.lo <= probe {
            true => (self.lo, self.hi - self.lo + over),
            false => (self.lo + (self.hi - self.lo) / 2, probe),
        };
        match len.min(probe).min(self.len.saturating_sub(at)) {
            0 => Step::Land(self.best),
            n => Step::Probe { at, len: n as u32 },
        }
    }

    /// The probe at byte `at` came back as `data`.
    pub fn served(&mut self, at: u64, data: &[u8]) {
        self.probes += 1;
        let mut found = false;
        let mut starts = Starts::new(data, self.size, self.pid);
        while let Some((pos, raw)) = starts.next() {
            let start = at + pos as u64;
            if start >= self.hi {
                break;
            }
            if start < self.lo {
                continue;
            }
            let ticks = self.base.ticks(raw);
            if ticks > self.target {
                self.hi = if found { start } else { at.max(self.lo) };
                return;
            }
            found = true;
            self.best = Some(Landing { at: start, ticks });
            self.lo = start + 1;
        }
        if !found {
            self.hi = self.hi.min(at.max(self.lo));
        }
    }
}

/// The PES packet starts of one PID in a window, each as the offset of its framed packet and its
/// raw decode stamp, else its presentation stamp. Framing is found again past a broken packet.
struct Starts<'a> {
    data: &'a [u8],
    /// Where the next sync byte is looked for while unframed.
    from: usize,
    /// The sync byte of the next packet once framed.
    framed: Option<usize>,
    size: PacketSize,
    pid: u16,
}

impl<'a> Starts<'a> {
    fn new(data: &'a [u8], size: PacketSize, pid: u16) -> Self {
        Starts { data, from: 0, framed: None, size, pid }
    }

    fn next(&mut self) -> Option<(usize, u64)> {
        let (stride, lead) = (framed_len(self.size), sync_offset(self.size));
        loop {
            let pos = match self.framed {
                Some(pos) => pos,
                None => {
                    let s =
                        sync::scan(self.data, self.from, CONFIRM, only(self.size), self.data.len());
                    s.size?;
                    s.at
                }
            };
            let raw: &[u8; PACKET_LEN] = self.data.get(pos..pos + PACKET_LEN)?.try_into().ok()?;
            let Ok(pkt) = parse_packet(raw) else {
                (self.framed, self.from) = (None, pos + 1);
                continue;
            };
            self.framed = Some(pos + stride);
            let start = pkt.flags & (PF_UNIT_START | PF_PAYLOAD) == PF_UNIT_START | PF_PAYLOAD;
            if pkt.pid != self.pid || !start || pos < lead {
                continue;
            }
            let stamp =
                parse_pes_header(payload(raw, &pkt)).ok().flatten().and_then(|h| h.dts.or(h.pts));
            if let Some(raw) = stamp {
                return Some((pos - lead, raw));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test vectors
mod tests {
    use super::*;

    const PID: u16 = 0x100;

    /// One transport packet of `pid` opening a PES packet stamped `dts`, or carrying its rest.
    fn packet(pid: u16, dts: Option<u64>) -> [u8; PACKET_LEN] {
        let mut p = [0xFFu8; PACKET_LEN];
        let unit = if dts.is_some() { 0x40 } else { 0 };
        p[..4].copy_from_slice(&[0x47, unit | (pid >> 8) as u8, pid as u8, 0x10]);
        if let Some(dts) = dts {
            let stamp = |prefix: u8, t: u64| {
                [
                    prefix << 4 | ((t >> 29) as u8 & 0x0E) | 1,
                    (t >> 22) as u8,
                    ((t >> 14) as u8 & 0xFE) | 1,
                    (t >> 7) as u8,
                    ((t << 1) as u8) | 1,
                ]
            };
            // a video PES header with PTS a frame past the DTS
            p[4..13].copy_from_slice(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0xC0, 10]);
            p[13..18].copy_from_slice(&stamp(3, (dts + 3600) % (1 << 33)));
            p[18..23].copy_from_slice(&stamp(1, dts));
        }
        p
    }

    /// `units` PES packets of PID 0x100 a frame apart from `first`, each two packets with an
    /// audio packet of PID 0x101 between, framed as `size`.
    fn file(units: u64, first: u64, size: PacketSize) -> Vec<u8> {
        let mut f = Vec::new();
        let mut put = |p: [u8; PACKET_LEN]| {
            if size == PacketSize::M2ts192 {
                f.extend_from_slice(&[0; 4]);
            }
            f.extend_from_slice(&p);
            if size == PacketSize::Fec204 {
                f.extend_from_slice(&[0; 16]);
            }
        };
        for n in 0..units {
            put(packet(PID, Some((first + n * 3600) % (1 << 33))));
            put(packet(0x101, Some((first + n * 3600) % (1 << 33))));
            put(packet(PID, None));
        }
        f
    }

    fn land(
        f: &[u8],
        base: Base,
        size: PacketSize,
        target: i64,
        bounds: (u64, u64),
    ) -> (Option<Landing>, u32) {
        let mut b = Bisect::new(base, PID, size, target, bounds, f.len() as u64);
        loop {
            match b.step() {
                Step::Probe { at, len } => {
                    let end = (at + u64::from(len)).min(f.len() as u64);
                    b.served(at, &f[at as usize..end as usize]);
                }
                Step::Land(l) => return (l, b.probes()),
            }
        }
    }

    #[test]
    fn lands_on_the_last_unit_of_the_pid_at_or_before_the_target() {
        for size in [PacketSize::Ts188, PacketSize::M2ts192, PacketSize::Fec204] {
            let unit = 3 * framed_len(size) as u64;
            let f = file(20_000, 50_000, size);
            let base = Base { origin: 50_000, slack: 90_000 };
            for target in [0, 1, 3599, 3600, 10_000_000, 19_999 * 3600, 4_000_000_000] {
                let (l, probes) = land(&f, base, size, target, (0, f.len() as u64));
                let n = (target as u64 / 3600).min(19_999);
                let want = Landing { at: n * unit, ticks: n as i64 * 3600 };
                assert_eq!(l, Some(want), "{size:?} target {target}");
                assert!(probes < 24, "{size:?} {probes} probes");
            }
            assert_eq!(land(&f, base, size, -1, (0, f.len() as u64)).0, None, "ahead of the first");
        }
    }

    #[test]
    fn a_stamp_rate_that_varies_lands_where_the_stamps_say() {
        // past the first thousand units each takes two packets, not three
        let size = PacketSize::Ts188;
        let mut f = file(1000, 0, size);
        let slow = file(10_000, 1000 * 3600, size);
        f.extend(slow.iter().enumerate().filter(|(i, _)| (i / 188) % 3 != 2).map(|(_, b)| *b));
        let base = Base { origin: 0, slack: 90_000 };
        let target = 4000 * 3600;
        let (l, _) = land(&f, base, size, target, (0, f.len() as u64));
        let l = l.expect("a landing");
        assert_eq!(l.ticks, target, "the unit stamped at the target");
        assert_eq!(l.at, 1000 * 3 * 188 + 3000 * 2 * 188);
    }

    #[test]
    fn the_bounds_keep_the_landing_inside_them() {
        let size = PacketSize::Ts188;
        let f = file(5000, 0, size);
        let base = Base { origin: 0, slack: 90_000 };
        let unit = 3 * 188;
        let (l, probes) = land(&f, base, size, 3000 * 3600, (2000 * unit, 4000 * unit));
        assert_eq!(l.map(|l| l.at), Some(3000 * unit));
        assert!(probes < 16, "{probes} probes");
        let (l, _) = land(&f, base, size, 1000 * 3600, (2000 * unit, 4000 * unit));
        assert_eq!(l, None, "the target behind the lower bound");
    }

    #[test]
    fn framing_is_found_again_past_a_broken_packet_and_a_cut_start() {
        let size = PacketSize::Ts188;
        let mut f = file(400, 0, size);
        // a broken sync byte in the 100th unit and a probe that starts mid packet
        f[100 * 3 * 188] = 0x00;
        let base = Base { origin: 0, slack: 90_000 };
        let mut s = Starts::new(&f[77..], size, PID);
        let mut seen = Vec::new();
        while let Some((pos, raw)) = s.next() {
            seen.push((pos + 77, base.ticks(raw)));
        }
        assert_eq!(seen.len(), 398, "every unit but the cut first and the broken one");
        assert_eq!(seen[0], (3 * 188, 3600));
        assert!(!seen.iter().any(|&(_, t)| t == 100 * 3600));
        assert_eq!(seen[99], (101 * 3 * 188, 101 * 3600));
    }

    #[test]
    fn the_clock_reads_on_across_the_wrap() {
        let first = (1 << 33) - 900_000;
        let size = PacketSize::Ts188;
        let f = file(2000, first, size);
        let base = Base { origin: first as i64, slack: 90_000 };
        assert_eq!(base.ticks(first), 0);
        assert_eq!(base.ticks(100), 900_100);
        let (l, _) = land(&f, base, size, 1_500_000, (0, f.len() as u64));
        assert_eq!(l.map(|l| l.at), Some(416 * 3 * 188));
    }
}
