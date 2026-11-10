//! The range-pulled walk of a Matroska stream. Blocks in a window are sliced out of it, an unlaced
//! block past it is joined from the windows of the chain after it and a read of any rest, and
//! unread elements are walked past.

use std::collections::VecDeque;

use profluens_core::memory::Memory;

use crate::ebml::{self, ReadError, id};
use crate::reader::{
    MAX_BLOCK, MAX_HELD_ELEMENT, Track, block_addition, block_frames, block_group_parts,
    block_pts_ns, cluster_led, find_subslice, lace_stamp, parse_block_header, parse_info,
    read_uint,
};

/// The first window after a start or a seek, so the first frames wait on no more.
pub(crate) const WINDOW: u32 = 16 << 10;
/// Each window doubles the last up to here. A seek on a disk costs about the time it reads 1 to 2
/// MiB, so a read this size spends at least half the disk's time reading, and [`AHEAD`] of them
/// stay under the bytes a demuxer keeps asked.
const WINDOW_MAX: u32 = 2 << 20;
/// The stream time a window holds at most once the rate is known, never under [`PACED_MIN`].
pub(crate) const WINDOW_TIME: u64 = 1_000_000_000;
/// A slow stream still reads in pieces a disk shared with other readers does not seek for each.
const PACED_MIN: u32 = 256 << 10;
/// Windows asked past the one walked, so the disk reads on while the walk parses.
pub(crate) const AHEAD: usize = 2;
/// A jump past the asked bytes shorter than this is read through. Reading it costs a disk about
/// what the seek would, and the kernel's readahead stays on a sequential stream.
pub(crate) const SKIP_MIN: u64 = 1 << 20;

/// What the walk passes unread at Segment or Cluster level (RFC 9559 section 5.1), the raw ones
/// CRC-32, Position, PrevSize, SilentTracks and EncryptedBlock, and a chained Segment's EBML head.
const PASSED: [&[u8]; 13] = [
    id::SEEK_HEAD,
    id::TRACKS,
    id::CUES,
    id::ATTACHMENTS,
    id::CHAPTERS,
    id::TAGS,
    id::VOID,
    &[0xBF],
    &[0xA7],
    &[0xAB],
    &[0x58, 0x54],
    &[0xAF],
    id::EBML,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    Window {
        offset: u64,
        len: u32,
        whole: Option<u64>,
    },
    Skip {
        to: u64,
    },
    Block {
        from: u64,
        to: u64,
        next: u64,
        keyframe: Option<bool>,
        duration_ns: Option<u64>,
        additions: Option<(u64, u64)>,
        discard_ns: u64,
    },
    Frame {
        offset: u64,
        len: u32,
        track: u64,
        pts_ns: u64,
        keyframe: bool,
        to: u64,
    },
    End,
}

pub(crate) struct Window {
    pub(crate) base: u64,
    pub(crate) mem: Memory,
    pub(crate) eof: bool,
}

pub(crate) struct Walk {
    pub(crate) pos: u64,
    pub(crate) resync: bool,
    pub(crate) cluster_base_tick: i64,
    pub(crate) scale: u64,
    pub(crate) duration_ns: Option<u64>,
    /// The next window's size, from [`WINDOW`] at a restart, kept across frames.
    pub(crate) window: u32,
    pub(crate) cap: u32,
    pub(crate) most: u32,
    pub(crate) cluster_at: Option<u64>,
    cluster_end: Option<u64>,
    /// End of a sized Segment, past which no element of it runs.
    pub(crate) end: Option<u64>,
    /// The last damage walked past since asked, the walk resynced after it.
    pub(crate) damage: Option<ReadError>,
    /// Where that damage was.
    pub(crate) damage_at: u64,
    /// The least the next ask reads, once. A seek's first read holds the keyframe with it.
    pub(crate) lead: u32,
}

impl Walk {
    pub(crate) fn new() -> Walk {
        let (pos, resync, cluster_base_tick, scale, duration_ns) = (0, false, 0, 1_000_000, None);
        let (window, most, cluster_at) = (WINDOW, WINDOW_MAX, None);
        Walk {
            pos,
            resync,
            cluster_base_tick,
            scale,
            duration_ns,
            window,
            cap: most,
            most,
            cluster_at,
            cluster_end: None,
            end: None,
            damage: None,
            damage_at: 0,
            lead: 0,
        }
    }

    pub(crate) fn restart(&mut self, pos: u64, resync: bool) {
        (self.pos, self.resync, self.cluster_base_tick, self.window) = (pos, resync, 0, WINDOW);
        (self.cluster_at, self.cluster_end) = (None, None);
    }

    /// Damage at `pos` is noted and the walk scans on from the next octet for a Cluster.
    fn lost(&mut self, e: ReadError) -> Step {
        (self.damage, self.damage_at) = (Some(e), self.pos);
        (self.resync, self.cluster_end) = (true, None);
        Step::Skip { to: self.pos + 1 }
    }

    /// A body that does not parse costs its element alone.
    fn spoilt(&mut self, e: ReadError, to: u64) -> Step {
        (self.damage, self.damage_at) = (Some(e), self.pos);
        Step::Skip { to }
    }

    pub(crate) fn set_cap(&mut self, cap: u32) {
        (self.cap, self.most) = (cap, cap);
    }

    pub(crate) fn pace(&mut self, bits_per_second: u64) {
        let bytes = u128::from(bits_per_second) * u128::from(WINDOW_TIME) / 8_000_000_000;
        self.most = (bytes.min(u128::from(self.cap)) as u32).max(PACED_MIN.min(self.cap));
    }

    fn next_len(&self) -> u32 {
        self.window.min(self.most)
    }

    /// A frame past the window grows it to hold the next one like it.
    pub(crate) fn fit(&mut self, len: u32) {
        self.window = self.window.max(len.min(WINDOW_MAX));
    }

    fn grow(&mut self) {
        self.window = self.window.saturating_mul(2).min(WINDOW_MAX);
    }

    fn ask(&mut self, offset: u64, whole: Option<u64>, need: u64) -> Step {
        let lead = std::mem::take(&mut self.lead).min(self.cap) as u64;
        let len = need.max(self.window.min(self.most) as u64).max(lead).min(u32::MAX as u64) as u32;
        Step::Window { offset, len, whole }
    }

    pub(crate) fn next(&mut self, win: Option<&Window>) -> Step {
        let Some(win) = win else {
            return self.ask(self.pos, None, 0);
        };
        let data = win.mem.data();
        let end = win.base + data.len() as u64;
        if self.pos < win.base || self.pos >= end {
            if win.eof && self.pos >= end {
                return Step::End;
            }
            return self.ask(self.pos, None, 0);
        }
        let at = (self.pos - win.base) as usize;
        if self.resync {
            return self.resync_in(win, at);
        }
        if self.cluster_end.is_some_and(|c| self.pos >= c) {
            self.cluster_end = None;
        }
        let header = match ebml::read_element_header(data, at) {
            Ok(h) => h,
            Err(ReadError::Incomplete) if win.eof => return Step::End,
            Err(ReadError::Incomplete) => return self.ask(self.pos, None, 0),
            Err(e) => return self.lost(e),
        };
        let id = header.id;
        let data_start = win.base + header.data_start as u64;
        if id == id::SEGMENT {
            return Step::Skip { to: data_start };
        }
        if id == id::CLUSTER {
            (self.cluster_base_tick, self.cluster_at) = (0, Some(self.pos));
            self.cluster_end = header.size.map(|size| data_start.saturating_add(size));
            return Step::Skip { to: data_start };
        }
        let Some(size) = header.size else {
            return self.lost(ReadError::Malformed("unexpected unknown-size element"));
        };
        let Some(elem_end) = data_start.checked_add(size) else {
            return self.lost(ReadError::Malformed("element size overflows the address space"));
        };
        if self.cluster_end.is_some_and(|c| elem_end > c) {
            return self.lost(ReadError::Malformed("element runs past its Cluster"));
        }
        if self.end.is_some_and(|e| elem_end > e) {
            return self.lost(ReadError::Malformed("element runs past its Segment"));
        }
        let block = id == id::SIMPLE_BLOCK || id == id::BLOCK_GROUP;
        if block && size > MAX_BLOCK {
            return self.lost(ReadError::Malformed("block larger than any the reader sends on"));
        }
        let parsed = block || id == id::INFO || id == id::TIMESTAMP;
        // Damage read as an element, its size would take the walk anywhere up to the end.
        if !parsed && !PASSED.contains(&id) {
            return self.lost(ReadError::Malformed("no Matroska element at this level"));
        }
        if !parsed {
            return Step::Skip { to: elem_end };
        }
        if elem_end > end {
            if size > MAX_HELD_ELEMENT {
                return self
                    .lost(ReadError::Malformed("element larger than the reader will buffer"));
            }
            if win.eof {
                return Step::End;
            }
            return self.past_window(win, id == id::SIMPLE_BLOCK, header.data_start, elem_end);
        }
        let (ds, de) = (header.data_start, (elem_end - win.base) as usize);
        let body = &data[ds..de];
        if id == id::INFO {
            let (scale, ticks) = match parse_info(body) {
                Ok(info) => info,
                Err(e) => return self.spoilt(e, elem_end),
            };
            if let Some(scale) = scale {
                self.scale = scale;
            }
            self.duration_ns = ticks.map(|t| (t * self.scale as f64).round() as u64);
        } else if id == id::TIMESTAMP {
            self.cluster_base_tick = read_uint(body) as i64;
        } else if id == id::SIMPLE_BLOCK {
            return Step::Block {
                from: data_start,
                to: elem_end,
                next: elem_end,
                keyframe: None,
                duration_ns: None,
                additions: None,
                discard_ns: 0,
            };
        } else {
            let parts = match block_group_parts(body, self.scale) {
                Ok(parts) => parts,
                Err(e) => return self.spoilt(e, elem_end),
            };
            let Some((s, e)) = parts.block else {
                return Step::Skip { to: elem_end };
            };
            let (from, to) = (data_start + s as u64, data_start + e as u64);
            let (keyframe, duration_ns) = (Some(parts.keyframe), parts.duration_ns);
            let additions =
                parts.additions.map(|(a, b)| (data_start + a as u64, data_start + b as u64));
            let discard_ns = parts.discard_ns;
            return Step::Block {
                from,
                to,
                next: elem_end,
                keyframe,
                duration_ns,
                additions,
                discard_ns,
            };
        }
        Step::Skip { to: elem_end }
    }

    /// An unlaced SimpleBlock frame is joined from what the chain holds of it, or passed unread
    /// when no pad takes it. Anything else is read again whole from its start.
    fn past_window(&mut self, win: &Window, simple: bool, ds: usize, elem_end: u64) -> Step {
        let header = if simple { parse_block_header(&win.mem.data()[ds..]).ok() } else { None };
        if let Some((track, rel_ts, flags, frames_start)) =
            header.filter(|h| (h.2 >> 1) & 0x03 == 0)
        {
            let offset = win.base + (ds + frames_start) as u64;
            let len = elem_end.saturating_sub(offset).min(u32::MAX as u64) as u32;
            let (pts_ns, keyframe) =
                (block_pts_ns(self.cluster_base_tick, rel_ts, self.scale), flags & 0x80 != 0);
            return Step::Frame { offset, len, track, pts_ns, keyframe, to: elem_end };
        }
        self.ask(self.pos, Some(elem_end), elem_end - self.pos)
    }

    fn resync_in(&mut self, win: &Window, at: usize) -> Step {
        const CID: &[u8] = id::CLUSTER;
        let data = win.mem.data();
        let mut from = at;
        while let Some(rel) = find_subslice(&data[from..], CID) {
            let cand = from + rel;
            match ebml::read_element_header(data, cand).map(|_| cluster_led(data, cand)) {
                Ok(Some(true)) => {
                    (self.resync, self.cluster_base_tick) = (false, 0);
                    return Step::Skip { to: win.base + cand as u64 };
                }
                Ok(Some(false)) => from = cand + 1,
                Ok(None) | Err(ReadError::Incomplete) if win.eof => from = cand + 1,
                Ok(None) | Err(ReadError::Incomplete) => {
                    self.pos = win.base + cand as u64;
                    return self.ask(self.pos, None, 0);
                }
                Err(_) => from = cand + 1,
            }
        }
        if win.eof {
            return Step::End;
        }
        // The last three octets may begin a Cluster ID the next window completes.
        let keep = (data.len() - at).min(CID.len() - 1);
        self.pos = win.base + (data.len() - keep) as u64;
        self.ask(self.pos, None, 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn frames(
        &self,
        win: &Window,
        (from, to): (u64, u64),
        keyframe: Option<bool>,
        (duration_ns, discard_ns): (Option<u64>, u64),
        additions: Option<(u64, u64)>,
        tracks: &[Track],
        lace: &mut Vec<usize>,
        frame: &mut dyn FnMut(Planned),
    ) -> Result<(), ReadError> {
        let (lo, hi) = ((from - win.base) as usize, (to - win.base) as usize);
        let body =
            win.mem.slice(lo, hi - lo).ok_or(ReadError::Malformed("block outside its window"))?;
        let flags = parse_block_header(body.data())?.2;
        let keyframe = keyframe.unwrap_or(flags & 0x80 != 0);
        let adds =
            additions.and_then(|(a, b)| win.mem.slice((a - win.base) as usize, (b - a) as usize));
        let mut slice = |track, pts_ns, k, n, r: std::ops::Range<usize>| {
            let t = tracks.iter().find(|t| t.track_number == track);
            let (pts_ns, duration_ns) =
                lace_stamp(pts_ns, k, n, duration_ns, t.and_then(|t| t.default_duration_ns));
            let view = body.slice(r.start, r.end - r.start);
            let state = view.map_or(State::Dropped, |v| State::Ready(v, false));
            let t35 = match (k, t, &adds) {
                (0, Some(t), Some(a)) => {
                    block_addition(a.data(), t.t35_add_id).and_then(|r| a.slice(r.start, r.len()))
                }
                _ => None,
            };
            let discard_ns = if k + 1 == n { discard_ns } else { 0 };
            let (tag, last_of) = (0, 0);
            frame(Planned {
                track,
                pts_ns,
                duration_ns,
                keyframe,
                tag,
                last_of,
                state,
                t35,
                discard_ns,
            });
        };
        block_frames(body.data(), self.cluster_base_tick, self.scale, lace, &mut slice)
    }
}

pub(crate) enum State {
    /// The read of `want` bytes at `offset`, joined to `head` when the chain held the frame's
    /// start.
    Waiting {
        offset: u64,
        want: u32,
        head: Option<Memory>,
    },
    Ready(Memory, bool),
    Dropped,
}

pub(crate) struct Planned {
    pub(crate) track: u64,
    pub(crate) pts_ns: u64,
    pub(crate) duration_ns: Option<u64>,
    pub(crate) keyframe: bool,
    pub(crate) tag: u64,
    pub(crate) last_of: u32,
    pub(crate) state: State,
    pub(crate) t35: Option<Memory>,
    pub(crate) discard_ns: u64,
}

/// A window of the chain, asked in file order ahead of the walk.
struct Link {
    tag: u64,
    offset: u64,
    len: u32,
    mem: Option<Memory>,
}

/// Where the bytes the walk asks for come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Need {
    /// The chain's front window, walked once served.
    Front,
    /// A range read on its own and walked, the chain kept for the walk after it.
    Alone { offset: u64, len: u32 },
    /// The chain's next window, a new chain unless `offset` is where the last one ended.
    Link { offset: u64, len: u32 },
}

/// Where the bytes of a frame past the walked window's end lie.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Span {
    /// A window of the chain holding some of them is still in flight.
    Wait,
    /// The walked window and the chain's served windows hold all of them.
    Held,
    /// They hold the frame up to `offset`, the rest is read on its own.
    Rest { offset: u64, len: u32 },
}

#[derive(Default)]
pub(crate) struct Plan {
    pub(crate) items: VecDeque<Planned>,
    pub(crate) next_tag: u64,
    pub(crate) held: u64,
    pub(crate) ranges: u32,
    pub(crate) window: Option<(u64, u32, u64)>,
    pub(crate) walking: Option<(u64, u32)>,
    pub(crate) asked_all: bool,
    pub(crate) warned: bool,
    links: VecDeque<Link>,
    /// Where the chain's next window starts, None before the first.
    chain_end: Option<u64>,
}

impl Plan {
    pub(crate) fn reset(&mut self) {
        self.items.clear();
        self.links.clear();
        (self.held, self.ranges, self.window, self.walking, self.asked_all) =
            (0, 0, None, None, false);
        self.chain_end = None;
    }

    /// What serves the walk's ask of `len` bytes at `offset`, `whole` the end of an element read
    /// again whole. Windows of the chain wholly behind `offset` were read through and go unwalked.
    pub(crate) fn need(&mut self, walk: &Walk, offset: u64, len: u32, whole: Option<u64>) -> Need {
        self.drop_behind(offset);
        if let Some(front) = self.links.front() {
            return match whole {
                Some(to) => Need::Alone { offset, len: (to - offset).min(u32::MAX as u64) as u32 },
                None if front.offset <= offset => Need::Front,
                // an element header across the end of the window before the front
                None => Need::Alone { offset, len: WINDOW.min(walk.most) },
            };
        }
        let gap = self.chain_end.filter(|_| whole.is_none()).and_then(|e| offset.checked_sub(e));
        let through = gap.map(|g| g + u64::from(walk.next_len()));
        match (gap, through) {
            (Some(gap), Some(n)) if gap < SKIP_MIN && n <= u64::from(walk.cap) => {
                Need::Link { offset: offset - gap, len: n as u32 }
            }
            _ => Need::Link { offset, len },
        }
    }

    fn drop_behind(&mut self, offset: u64) {
        while let Some(l) = self.links.front().filter(|l| l.offset + u64::from(l.len) <= offset) {
            (self.held, self.ranges) = (self.held - u64::from(l.len), self.ranges - 1);
            self.links.pop_front();
        }
    }

    /// Where the bytes of the frame up to `to` past `win`, the window walked, lie. Only windows
    /// that follow on with no gap count.
    pub(crate) fn span(&self, win: &Window, to: u64) -> Span {
        let mut at = win.base + win.mem.len() as u64;
        for l in &self.links {
            if at >= to || l.offset > at {
                break;
            }
            if l.offset + u64::from(l.len) <= at {
                continue;
            }
            let Some(mem) = &l.mem else {
                return Span::Wait;
            };
            at = at.max(l.offset + mem.len() as u64);
        }
        match to.checked_sub(at).filter(|n| *n > 0) {
            Some(n) => Span::Rest { offset: at, len: n.min(u32::MAX as u64) as u32 },
            None => Span::Held,
        }
    }

    /// Copies the bytes from `from` that `win` and the chain hold into `out`, returning how many.
    pub(crate) fn gather(&self, win: &Window, from: u64, out: &mut [u8]) -> usize {
        let served = self.links.iter().filter_map(|l| Some((l.offset, l.mem.as_ref()?.data())));
        let mut n = 0;
        for (base, data) in std::iter::once((win.base, win.mem.data())).chain(served) {
            let at = from + n as u64;
            if n == out.len() || base > at {
                break;
            }
            let Some(src) = data.get((at - base) as usize..) else {
                continue;
            };
            let k = src.len().min(out.len() - n);
            out[n..n + k].copy_from_slice(&src[..k]);
            n += k;
        }
        n
    }

    /// The walk is past a frame joined up to `to`. A rest read on its own carries the chain on
    /// from there, so the next window starts where it ends.
    pub(crate) fn joined(&mut self, to: u64) {
        self.drop_behind(to);
        if self.links.is_empty() {
            self.chain_end = self.chain_end.max(Some(to));
        }
    }

    /// The chain's next window asked under the next tag.
    pub(crate) fn linked(&mut self, walk: &mut Walk, offset: u64, len: u32) {
        let tag = self.asked(len);
        self.links.push_back(Link { tag, offset, len, mem: None });
        self.chain_end = Some(offset + u64::from(len));
        walk.grow();
    }

    /// The chain's next window to ask ahead of the walk, none with [`AHEAD`] out, before the
    /// first or at the Segment's end.
    pub(crate) fn ahead(&self, walk: &Walk) -> Option<(u64, u32)> {
        let end = self.chain_end.filter(|_| self.links.len() < AHEAD && !self.asked_all)?;
        let room = walk.end.map_or(u64::MAX, |e| e.saturating_sub(end));
        let len = u64::from(walk.next_len()).min(room) as u32;
        (len > 0).then_some((end, len))
    }

    /// Keeps a served window of the chain, handing back a buffer no window of it asked.
    pub(crate) fn link_in(&mut self, tag: u64, mem: Memory) -> Option<Memory> {
        match self.links.iter_mut().find(|l| l.tag == tag) {
            Some(l) => {
                l.mem = Some(mem);
                None
            }
            None => Some(mem),
        }
    }

    /// The chain's front window once served, now the one walked. It takes a tag of its own so
    /// the items stay in walk order behind frames asked after it was.
    pub(crate) fn enter_front(&mut self) -> Option<Window> {
        let mem = self.links.front_mut()?.mem.take()?;
        let l = self.links.pop_front()?;
        self.leave_window();
        let tag = self.next_tag;
        self.next_tag += 1;
        self.walking = Some((tag, l.len));
        Some(Window { base: l.offset, eof: mem.len() < l.len as usize, mem })
    }

    /// Room under `caps` for a range the walk needs. The chain's windows and the one walked are
    /// left out, the walk waiting on that range never reaches them to free theirs.
    pub(crate) fn room(&self, len: u32, (bytes, ranges): (u64, u32)) -> bool {
        let ahead: u64 = self.links.iter().map(|l| u64::from(l.len)).sum();
        let walked = self.walking.map_or(0, |w| u64::from(w.1));
        let held = self.held - ahead - walked;
        let others = self.links.len() as u32 + u32::from(self.walking.is_some());
        self.ranges - others < ranges && (held == 0 || held + u64::from(len) <= bytes)
    }

    pub(crate) fn asked(&mut self, len: u32) -> u64 {
        let tag = self.next_tag;
        self.next_tag += 1;
        (self.held, self.ranges) = (self.held + len as u64, self.ranges + 1);
        tag
    }

    pub(crate) fn leave_window(&mut self) {
        let Some((tag, len)) = self.walking.take() else {
            return;
        };
        match self.items.iter_mut().rev().take_while(|p| p.tag >= tag).find(|p| p.tag == tag) {
            Some(last) => last.last_of += len,
            None => (self.held, self.ranges) = (self.held - len as u64, self.ranges - 1),
        }
    }

    pub(crate) fn window_in(&mut self, tag: u64) -> Option<(u32, u64)> {
        let (t, len, offset) = self.window.filter(|w| w.0 == tag)?;
        self.leave_window();
        (self.window, self.walking) = (None, Some((t, len)));
        Some((len, offset))
    }

    pub(crate) fn served(&mut self, tag: u64, buf: Memory) -> Result<(), (u64, u32, u32)> {
        let at = self.items.partition_point(|p| p.tag < tag);
        let Some(p) = self.items.get_mut(at).filter(|p| p.tag == tag) else {
            return Ok(());
        };
        let (offset, want, head) = match std::mem::replace(&mut p.state, State::Dropped) {
            State::Waiting { offset, want, head } => (offset, want, head),
            other => {
                p.state = other;
                return Ok(());
            }
        };
        let got = buf.len() as u32;
        if got != want {
            return if got < want { Err((offset, want, got)) } else { Ok(()) };
        }
        p.state = match head {
            None => State::Ready(buf, true),
            Some(mut frame) => {
                let n = frame.len();
                frame.as_mut_full()[n..n + buf.len()].copy_from_slice(buf.data());
                frame.set_len(n + buf.len());
                State::Ready(frame, true)
            }
        };
        Ok(())
    }

    pub(crate) fn pop_ready(&mut self) -> Option<Planned> {
        if self.items.front().is_none_or(|p| matches!(p.state, State::Waiting { .. })) {
            return None;
        }
        let p = self.items.pop_front()?;
        if p.last_of > 0 {
            (self.held, self.ranges) = (self.held - p.last_of as u64, self.ranges - 1);
        }
        Some(p)
    }
}

#[cfg(test)]
// Test fixtures allocate freely.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use profluens_core::memory::Pool;

    const VIDEO: u64 = 2;

    fn window(pool: &Pool, file: &[u8], base: u64, len: usize) -> Window {
        let from = (base as usize).min(file.len());
        let bytes = &file[from..(from + len).min(file.len())];
        let mut mem = pool.acquire_exact(len.max(1)).expect("exact");
        mem.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
        mem.set_len(bytes.len());
        Window { base, mem, eof: bytes.len() < len }
    }

    fn element(out: &mut Vec<u8>, id: &[u8], body: &[u8]) {
        ebml::write_id(out, id);
        ebml::write_size(out, body.len() as u64);
        out.extend_from_slice(body);
    }

    fn block(out: &mut Vec<u8>, track: u8, len: usize) {
        let mut body = vec![0x80 | track, 0, 0, 0x80];
        body.extend(std::iter::repeat_n(track, len));
        element(out, id::SIMPLE_BLOCK, &body);
    }

    fn cluster(out: &mut Vec<u8>, at: u64, sizes: &[usize]) {
        ebml::write_id(out, id::CLUSTER);
        ebml::write_unknown_size(out);
        ebml::write_uint(out, id::TIMESTAMP, at);
        for (k, &n) in sizes.iter().enumerate() {
            let mut body = vec![0x81, 0, k as u8, 0x80];
            body.extend(std::iter::repeat_n(k as u8, n));
            element(out, id::SIMPLE_BLOCK, &body);
        }
    }

    /// Audio blocks of track 1 with a video frame of `video` bytes on track 2 after every
    /// `every`, as a muxer interleaves them, and where each video frame ends.
    fn interleaved(blocks: usize, every: usize, video: usize) -> (Vec<u8>, Vec<u64>) {
        let (mut file, mut ends) = (Vec::new(), Vec::new());
        ebml::write_id(&mut file, id::CLUSTER);
        ebml::write_unknown_size(&mut file);
        ebml::write_uint(&mut file, id::TIMESTAMP, 0);
        for k in 0..blocks {
            block(&mut file, 1, 400);
            if k % every == every - 1 {
                block(&mut file, VIDEO as u8, video);
                ends.push(file.len() as u64);
            }
        }
        (file, ends)
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Read {
        Link,
        Alone,
        Frame,
    }

    struct Run {
        steps: Vec<Step>,
        reads: Vec<(Read, u64, u32)>,
        /// The chain's windows out each time the walk entered one.
        out: Vec<usize>,
        /// Frames past a window joined from the chain with no read, their offset and length.
        joined: Vec<(u64, u32)>,
        /// Times the walk waited on a window of the chain in flight to join a frame.
        waits: usize,
    }

    impl Run {
        fn lens(&self, kind: Read) -> Vec<u32> {
            self.reads.iter().filter(|r| r.0 == kind).map(|r| r.2).collect()
        }

        /// The bytes served, none past the end of `file`.
        fn bytes(&self, file: &[u8]) -> u64 {
            let len = file.len() as u64;
            self.reads.iter().map(|r| (r.1 + u64::from(r.2)).min(len).saturating_sub(r.1)).sum()
        }

        /// The bytes served more than once.
        fn again(&self, file: &[u8]) -> u64 {
            let len = file.len() as u64;
            let mut spans: Vec<_> = self
                .reads
                .iter()
                .map(|r| (r.1.min(len), (r.1 + u64::from(r.2)).min(len)))
                .collect();
            spans.sort_unstable();
            let (mut end, mut again) = (0, 0);
            for (from, to) in spans {
                again += to.min(end).saturating_sub(from);
                end = end.max(to);
            }
            again
        }

        fn blocks(&self) -> usize {
            self.steps.iter().filter(|s| matches!(s, Step::Block { .. })).count()
        }
    }

    struct Drive<'a> {
        file: &'a [u8],
        pool: Pool,
        w: Walk,
        plan: Plan,
        /// Track numbers no pad takes, their frames passed unread.
        unlinked: &'a [u64],
        /// Windows of the chain stay in flight until the walk needs them.
        lazy: bool,
    }

    impl<'a> Drive<'a> {
        fn new(file: &'a [u8], bits: Option<u64>) -> Self {
            let mut w = Walk::new();
            w.set_cap(64 << 20);
            if let Some(b) = bits {
                w.pace(b);
            }
            let (pool, plan) = (Pool::new(4096), Plan::default());
            Drive { file, pool, w, plan, unlinked: &[], lazy: false }
        }

        fn serve(&mut self, run: &mut Run, offset: u64, len: u32) {
            self.plan.linked(&mut self.w, offset, len);
            run.reads.push((Read::Link, offset, len));
            if !self.lazy {
                self.land(self.plan.next_tag - 1, offset, len);
            }
        }

        fn land(&mut self, tag: u64, offset: u64, len: u32) {
            let mem = window(&self.pool, self.file, offset, len as usize).mem;
            assert!(self.plan.link_in(tag, mem).is_none());
        }

        /// Serves the first window of the chain in flight, false with none.
        fn land_one(&mut self) -> bool {
            let Some(l) = self.plan.links.iter().find(|l| l.mem.is_none()) else {
                return false;
            };
            let (tag, offset, len) = (l.tag, l.offset, l.len);
            self.land(tag, offset, len);
            true
        }

        /// Joins the frame at `offset` up to `to` as the element does, checking its bytes.
        fn join(&mut self, run: &mut Run, win: &Window, (offset, len, to): (u64, u32, u64)) {
            let span = loop {
                match self.plan.span(win, to) {
                    Span::Wait => {
                        assert!(self.land_one(), "a wait on no window in flight");
                        run.waits += 1;
                    }
                    span => break span,
                }
            };
            let mut frame = vec![0; len as usize];
            let n = self.plan.gather(win, offset, &mut frame);
            match span {
                Span::Rest { offset: at, len: rest } => {
                    assert_eq!(at, offset + n as u64, "the rest read from where the chain ends");
                    run.reads.push((Read::Frame, at, rest));
                    frame[n..].copy_from_slice(&self.file[at as usize..to as usize]);
                }
                _ => {
                    assert_eq!(n, len as usize);
                    run.joined.push((offset, len));
                }
            }
            assert!(frame == self.file[offset as usize..to as usize], "the frame at {offset}");
            self.plan.joined(to);
            self.w.fit(len);
        }

        fn ahead(&mut self, run: &mut Run) {
            while let Some((offset, len)) = self.plan.ahead(&self.w) {
                self.serve(run, offset, len);
            }
        }

        /// Walks from `from` as the element does, every range served at once.
        fn walk(&mut self, from: u64, resync: bool) -> Run {
            self.plan.reset();
            self.w.restart(from, resync);
            let mut run = Run {
                steps: Vec::new(),
                reads: Vec::new(),
                out: Vec::new(),
                joined: Vec::new(),
                waits: 0,
            };
            let mut win: Option<Window> = None;
            for _ in 0..1_000_000 {
                let step = self.w.next(win.as_ref());
                run.steps.push(step);
                match step {
                    Step::Skip { to } | Step::Block { next: to, .. } => self.w.pos = to,
                    Step::End => return run,
                    Step::Frame { track, offset, len, to, .. } => {
                        if !self.unlinked.contains(&track) {
                            let at = win.as_ref().expect("a frame starts in the window walked");
                            self.join(&mut run, at, (offset, len, to));
                        }
                        (win, self.w.pos) = (None, to);
                        self.plan.leave_window();
                    }
                    Step::Window { offset, len, whole } => {
                        match self.plan.need(&self.w, offset, len, whole) {
                            Need::Front => {
                                if self.lazy
                                    && self.plan.links.front().is_some_and(|l| l.mem.is_none())
                                {
                                    self.land_one();
                                }
                                win = Some(self.plan.enter_front().expect("served"));
                                self.ahead(&mut run);
                                run.out.push(self.plan.links.len());
                            }
                            Need::Alone { offset, len } => {
                                self.plan.leave_window();
                                let tag = self.plan.asked(len);
                                self.plan.window = Some((tag, len, offset));
                                self.plan.window_in(tag);
                                run.reads.push((Read::Alone, offset, len));
                                win = Some(window(&self.pool, self.file, offset, len as usize));
                                self.ahead(&mut run);
                            }
                            Need::Link { offset, len } => {
                                self.plan.leave_window();
                                win = None;
                                self.serve(&mut run, offset, len);
                                self.ahead(&mut run);
                            }
                        }
                    }
                }
            }
            panic!("the walk did not end");
        }
    }

    fn walk(file: &[u8], from: u64, resync: bool) -> Run {
        Drive::new(file, None).walk(from, resync)
    }

    #[test]
    fn a_frame_past_the_window_is_joined_from_the_chain_with_no_read() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100, 40_000, 200, 70_000, 300]);
        let run = walk(&file, 0, false);
        assert!(run.lens(Read::Frame).is_empty(), "{:?}", run.reads);
        let lens: Vec<_> = run.joined.iter().map(|j| j.1).collect();
        assert_eq!(lens, [40_000, 70_000], "the second across two windows");
        assert_eq!(run.blocks(), 3, "the small ones sliced out of windows");
        assert_eq!(run.again(&file), 0);
    }

    #[test]
    fn a_frame_past_the_window_waits_on_the_window_in_flight() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100, 40_000, 200, 70_000, 300]);
        let mut d = Drive::new(&file, None);
        d.lazy = true;
        let run = d.walk(0, false);
        assert!(run.waits >= 2, "{} waits", run.waits);
        assert!(run.lens(Read::Frame).is_empty(), "{:?}", run.reads);
        assert_eq!(run.joined.len(), 2);
        assert_eq!(run.again(&file), 0);
    }

    #[test]
    fn a_frame_past_the_chain_reads_its_rest_from_the_chain_end_once() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[3_000; 3_000]);
        let big = 10 << 20;
        block(&mut file, VIDEO as u8, big);
        let to = file.len() as u64;
        cluster(&mut file, 9, &[3_000; 1_000]);
        let run = walk(&file, 0, false);
        let rest: Vec<_> = run.reads.iter().filter(|r| r.0 == Read::Frame).collect();
        assert_eq!(rest.len(), 1, "{rest:?}");
        let (_, at, len) = *rest[0];
        assert_eq!(at + u64::from(len), to);
        assert!(len < big as u32 - (2 << 20), "the chain's windows not read again, {len}");
        assert!(run.reads.iter().any(|r| r.0 == Read::Link && r.1 == to), "the chain on from it");
        assert_eq!(run.again(&file), 0);
        assert_eq!(run.blocks() + run.joined.len(), 4_000, "the walk goes on past it");
    }

    #[test]
    fn room_leaves_out_the_window_walked_and_the_chain() {
        let (pool, file) = (Pool::new(4096), vec![0; 64]);
        let (mut p, mut w) = (Plan::default(), Walk::new());
        let caps = (8 << 20, 64);
        p.linked(&mut w, 0, 2 << 20);
        p.link_in(p.next_tag - 1, window(&pool, &file, 0, 64).mem);
        p.enter_front().expect("served");
        p.linked(&mut w, 2 << 20, 2 << 20);
        p.linked(&mut w, 4 << 20, 2 << 20);
        assert!(p.room(8 << 20, caps), "nothing else held");
        p.asked(1 << 20);
        assert!(p.room(7 << 20, caps));
        assert!(!p.room(7 << 20 | 1, caps), "a frame waiting counts");
    }

    #[test]
    fn each_window_doubles_the_last_up_to_the_most() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[3_000; 3_000]);
        let lens = walk(&file, 0, false).lens(Read::Link);
        assert_eq!(&lens[..4], [WINDOW, 2 * WINDOW, 4 * WINDOW, 8 * WINDOW]);
        assert!(lens.contains(&WINDOW_MAX), "{lens:?}");
        assert!(lens.iter().all(|l| *l <= WINDOW_MAX));
    }

    #[test]
    fn the_window_keeps_its_size_past_a_large_frame() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[300; 40]);
        block(&mut file, 1, 600_000);
        let after = file.len() as u64;
        cluster(&mut file, 9, &[300; 4_000]);
        let run = walk(&file, 0, false);
        let rest: Vec<_> = run.reads.iter().filter(|r| r.0 == Read::Frame).collect();
        assert_eq!(rest.len(), 1, "{rest:?}");
        assert_eq!(rest[0].1 + u64::from(rest[0].2), after, "the rest of the frame alone");
        let past = run.reads.iter().filter(|r| r.0 == Read::Link && r.1 >= after);
        assert!(past.clone().all(|r| r.2 >= 600_000), "{:?}", past.collect::<Vec<_>>());
    }

    #[test]
    fn the_chain_keeps_its_windows_ahead_end_to_end() {
        let (file, _) = interleaved(20_000, 2, 20_000);
        let run = walk(&file, 0, false);
        assert!(run.out.iter().all(|n| *n <= AHEAD));
        let most = run.out.iter().filter(|n| **n == AHEAD).count();
        assert!(most * 10 >= run.out.len() * 9, "{:?}", run.out);
        let links: Vec<_> = run.reads.iter().filter(|r| r.0 == Read::Link).collect();
        let joined = links.windows(2).all(|p| p[0].1 + u64::from(p[0].2) == p[1].1);
        assert!(joined, "the windows of the chain follow each other");
        assert!(links.last().is_some_and(|l| l.1 + u64::from(l.2) >= file.len() as u64));
    }

    #[test]
    fn told_the_rate_a_window_holds_a_second_of_it() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[3_000; 3_000]);
        let most = |bits| {
            let d = Drive::new(&file, Some(bits));
            d.w.most
        };
        assert_eq!(most(4_000_000), 500_000);
        assert_eq!(most(168_000), PACED_MIN, "a slow stream reads no smaller");
        assert_eq!(most(64_000_000), 8_000_000, "the rate's bound, a window stays under its own");
        let lens = Drive::new(&file, Some(4_000_000)).walk(0, false).lens(Read::Link);
        assert_eq!(lens[0], WINDOW, "the first window too");
        assert_eq!(lens.iter().max(), Some(&500_000), "{lens:?}");
    }

    #[test]
    fn behind_a_seek_the_walk_scans_across_windows_to_the_next_cluster() {
        let mut file = vec![0xA5; 50_000];
        let at = file.len() as u64;
        cluster(&mut file, 7, &[100, 100]);
        let run = walk(&file, 10, true);
        let to =
            run.steps.iter().find_map(|s| if let Step::Skip { to } = s { Some(*to) } else { None });
        assert_eq!(to, Some(at), "the first place walked to is the Cluster");
        assert_eq!(run.steps.iter().filter(|s| matches!(s, Step::Block { .. })).count(), 2);
    }

    /// Clusters of 100 blocks of 3000 octets, `count` of them from stamp `from`.
    fn clusters(file: &mut Vec<u8>, from: u64, count: u64) {
        for c in from..from + count {
            cluster(file, c, &[3_000; 100]);
        }
    }

    #[test]
    fn behind_a_seek_damage_read_as_an_element_no_level_holds_is_resynced_past() {
        let mut file = Vec::new();
        clusters(&mut file, 0, 2);
        let reach = 20 * 101 * 3_010;
        // ChapterTrack, an element of Chapters, its size reaching most of the file after it
        file.push(0x8F);
        ebml::write_size(&mut file, reach as u64);
        clusters(&mut file, 2, 30);
        let run = walk(&file, 10, true);
        assert_eq!(run.blocks() + run.joined.len(), 31 * 100, "every Cluster after the first");
    }

    #[test]
    fn a_block_larger_than_any_sent_on_is_damage_not_a_skip() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100]);
        // an unlaced block on track 9 whose size bit flipped to 17 MiB
        ebml::write_id(&mut file, id::SIMPLE_BLOCK);
        ebml::write_size_fixed(&mut file, 17 << 20, 4);
        file.extend_from_slice(&[0x89, 0, 0, 0x80]);
        clusters(&mut file, 1, 60);
        let mut d = Drive::new(&file, None);
        d.unlinked = &[9];
        let run = d.walk(0, false);
        assert_eq!(run.blocks() + run.joined.len(), 1 + 60 * 100, "every Cluster after it");
    }

    #[test]
    fn the_first_window_after_a_seek_is_small_again() {
        let (file, _) = interleaved(20_000, 2, 20_000);
        let mut d = Drive::new(&file, None);
        assert!(d.walk(0, false).lens(Read::Link).contains(&WINDOW_MAX));
        let run = d.walk(file.len() as u64 / 2, true);
        assert_eq!(run.reads[0], (Read::Link, file.len() as u64 / 2, WINDOW));
        d.w.lead = 1 << 20;
        let run = d.walk(file.len() as u64 / 2, true);
        assert_eq!(run.reads[0].2, 1 << 20, "a seek's lead read holds its keyframe");
        assert_eq!(run.reads[1].2, 2 * WINDOW, "and the chain still grows from the start");
    }

    #[test]
    fn a_laced_block_or_a_group_past_the_window_is_read_whole() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100]);
        let start = file.len() as u64;
        let mut laced = vec![0x81, 0, 0, 0x84, 1];
        laced.extend(std::iter::repeat_n(9, 30_000));
        element(&mut file, id::SIMPLE_BLOCK, &laced);
        let laced_end = file.len() as u64;
        let group_at = file.len() as u64;
        let mut group = Vec::new();
        let mut block = vec![0x81, 0, 1, 0];
        block.extend(std::iter::repeat_n(3, 100_000));
        element(&mut group, id::BLOCK, &block);
        ebml::write_uint(&mut group, id::BLOCK_DURATION, 5);
        element(&mut file, id::BLOCK_GROUP, &group);
        let run = walk(&file, 0, false);
        let alone: Vec<_> = run.reads.iter().filter(|r| r.0 == Read::Alone).collect();
        assert_eq!(alone[0], &(Read::Alone, start, (laced_end - start) as u32), "{alone:?}");
        assert_eq!(alone[1], &(Read::Alone, group_at, (file.len() as u64 - group_at) as u32));
        let grouped = run.steps.iter().any(|s| {
            matches!(s, Step::Block { keyframe: Some(true), duration_ns: Some(5_000_000), .. })
        });
        assert!(grouped, "the group's block, a keyframe with no reference, with its duration");
        assert!(run.lens(Read::Frame).is_empty());
    }

    #[test]
    fn an_element_nobody_reads_past_the_gap_is_walked_past_unread() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100]);
        element(&mut file, id::CUES, &vec![0; 4 << 20]);
        let next = file.len() as u64;
        cluster(&mut file, 9, &[100]);
        let run = walk(&file, 0, false);
        let read = run.bytes(&file);
        assert!(read < 1 << 18, "{read} bytes read for a file of {}", file.len());
        assert!(run.reads.iter().any(|r| r.1 == next), "a new chain where the walk went on");
    }

    #[test]
    fn an_element_nobody_reads_under_the_gap_is_read_through() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100]);
        element(&mut file, id::CUES, &vec![0; 600 << 10]);
        cluster(&mut file, 9, &[100]);
        let run = walk(&file, 0, false);
        let links: Vec<_> = run.reads.iter().filter(|r| r.0 == Read::Link).collect();
        assert!(links.windows(2).all(|p| p[0].1 + u64::from(p[0].2) == p[1].1), "{links:?}");
        assert_eq!(run.bytes(&file), file.len() as u64);
    }

    #[test]
    fn audio_alone_reads_through_small_video_frames_and_jumps_large_ones() {
        let (small, _) = interleaved(4_000, 2, 300_000);
        let mut d = Drive::new(&small, None);
        d.unlinked = &[VIDEO];
        let run = d.walk(0, false);
        assert!(run.lens(Read::Frame).iter().all(|l| *l <= 400), "audio across a window alone");
        let links: Vec<_> = run.reads.iter().filter(|r| r.0 == Read::Link).collect();
        assert!(links.windows(2).all(|p| p[0].1 + u64::from(p[0].2) == p[1].1));
        let (large, ends) = interleaved(400, 100, 6 << 20);
        let mut d = Drive::new(&large, None);
        d.unlinked = &[VIDEO];
        let run = d.walk(0, false);
        assert!(run.lens(Read::Frame).iter().all(|l| *l <= 400));
        let starts = |at: &u64| run.reads.iter().any(|r| r.0 == Read::Link && r.1 == *at);
        assert!(ends[..2].iter().all(starts), "a new chain past a frame the chain fell short of");
        let read = run.bytes(&large);
        assert!(read <= (large.len() - (6 << 20)) as u64, "{read} of {}", large.len());
        let frames = run.lens(Read::Frame).len() + run.joined.len();
        assert_eq!(run.blocks() + frames, 400, "every audio block");
    }
}
