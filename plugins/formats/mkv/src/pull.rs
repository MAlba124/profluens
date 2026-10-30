//! The range-pulled walk of a Matroska stream. Blocks in a window are sliced out of it, an unlaced
//! block past it is read on its own, and unread elements are walked past.

use std::collections::VecDeque;

use profluens_core::memory::Memory;

use crate::ebml::{self, ReadError, id};
use crate::reader::{
    MAX_HELD_ELEMENT, Track, block_addition, block_frames, block_group_parts, block_pts_ns,
    cluster_led, find_subslice, lace_stamp, parse_block_header, parse_info, read_uint,
};

pub(crate) const WINDOW: u32 = 16 << 10;
const WINDOW_MAX: u32 = 256 << 10;
/// The stream time a window holds at most once the rate is known, never under [`PACED_MIN`].
pub(crate) const WINDOW_TIME: u64 = 250_000_000;
const PACED_MIN: u32 = 4 << 10;

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
    pub(crate) window: u32,
    framed: bool,
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
}

impl Walk {
    pub(crate) fn new() -> Walk {
        let (pos, resync, cluster_base_tick, scale, duration_ns) = (0, false, 0, 1_000_000, None);
        let (window, framed, most, cluster_at) = (WINDOW, false, WINDOW_MAX, None);
        Walk {
            pos,
            resync,
            cluster_base_tick,
            scale,
            duration_ns,
            window,
            framed,
            cap: most,
            most,
            cluster_at,
            cluster_end: None,
            end: None,
            damage: None,
            damage_at: 0,
        }
    }

    pub(crate) fn restart(&mut self, pos: u64, resync: bool) {
        (self.pos, self.resync, self.cluster_base_tick, self.window, self.framed) =
            (pos, resync, 0, WINDOW, false);
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

    pub(crate) fn entered(&mut self) {
        self.framed = false;
    }

    fn ask(&mut self, offset: u64, whole: Option<u64>, need: u64) -> Step {
        let len = need.max(self.window.min(self.most) as u64).min(u32::MAX as u64) as u32;
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
            if self.pos >= end && !self.framed {
                self.window = (self.window * 2).min(WINDOW_MAX);
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
        let parsed = id == id::INFO
            || id == id::TIMESTAMP
            || id == id::SIMPLE_BLOCK
            || id == id::BLOCK_GROUP;
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

    /// An unlaced SimpleBlock frame of half a window or more is read on its own. Anything else is
    /// read again from its start in the next window.
    fn past_window(&mut self, win: &Window, simple: bool, ds: usize, elem_end: u64) -> Step {
        let header = if simple { parse_block_header(&win.mem.data()[ds..]).ok() } else { None };
        if let Some((track, rel_ts, flags, frames_start)) =
            header.filter(|h| (h.2 >> 1) & 0x03 == 0)
        {
            let offset = win.base + (ds + frames_start) as u64;
            let len = elem_end.saturating_sub(offset).min(u32::MAX as u64) as u32;
            if len >= self.window / 2 {
                let (pts_ns, keyframe) =
                    (block_pts_ns(self.cluster_base_tick, rel_ts, self.scale), flags & 0x80 != 0);
                (self.window, self.framed) = (WINDOW, true);
                return Step::Frame { offset, len, track, pts_ns, keyframe, to: elem_end };
            }
        }
        let total = elem_end - self.pos;
        match total <= self.window as u64 {
            true if !self.framed => self.window = (self.window * 2).min(WINDOW_MAX),
            true => {}
            false => self.window = WINDOW,
        }
        self.ask(self.pos, Some(elem_end), total)
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
    Waiting { offset: u64, want: u32 },
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
}

impl Plan {
    pub(crate) fn reset(&mut self) {
        self.items.clear();
        (self.held, self.ranges, self.window, self.walking, self.asked_all) =
            (0, 0, None, None, false);
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
        let State::Waiting { offset, want } = p.state else {
            return Ok(());
        };
        let got = buf.len() as u32;
        p.state = if got == want { State::Ready(buf, true) } else { State::Dropped };
        if got < want {
            return Err((offset, want, got));
        }
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

    fn walk(file: &[u8], from: u64, resync: bool) -> Vec<Step> {
        walk_paced(file, from, resync, None)
    }

    fn walk_paced(file: &[u8], from: u64, resync: bool, bits: Option<u64>) -> Vec<Step> {
        let pool = Pool::new(4096);
        let mut w = Walk::new();
        w.set_cap(WINDOW_MAX);
        if let Some(b) = bits {
            w.pace(b);
        }
        w.restart(from, resync);
        let (mut win, mut steps) = (None, Vec::new());
        for _ in 0..10_000 {
            let step = w.next(win.as_ref());
            steps.push(step);
            match step {
                Step::Skip { to } | Step::Block { next: to, .. } | Step::Frame { to, .. } => {
                    w.pos = to
                }
                Step::Window { offset, len, .. } => {
                    win = Some(window(&pool, file, offset, len as usize));
                    w.entered();
                }
                Step::End => return steps,
            }
        }
        panic!("the walk did not end");
    }

    #[test]
    fn a_block_past_the_window_is_read_on_its_own_and_the_rest_sliced() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100, 40_000, 200, 300]);
        let steps = walk(&file, 0, false);
        let frames: Vec<_> = steps
            .iter()
            .filter_map(|s| if let Step::Frame { len, .. } = s { Some(*len) } else { None })
            .collect();
        assert_eq!(frames, [40_000], "the big block alone");
        let blocks = steps.iter().filter(|s| matches!(s, Step::Block { .. })).count();
        assert_eq!(blocks, 3, "the small ones sliced out of windows");
    }

    #[test]
    fn a_window_of_whole_elements_is_followed_by_a_longer_one() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[300; 400]);
        let lens: Vec<u32> = walk(&file, 0, false)
            .iter()
            .filter_map(|s| if let Step::Window { len, .. } = s { Some(*len) } else { None })
            .collect();
        assert_eq!(&lens[..4], [WINDOW, 2 * WINDOW, 4 * WINDOW, 8 * WINDOW]);
        assert!(lens.iter().all(|l| *l <= WINDOW_MAX));
    }

    #[test]
    fn told_the_rate_a_window_holds_a_quarter_second_of_it() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[300; 3000]);
        let lens = |bits| -> Vec<u32> {
            let steps = walk_paced(&file, 0, false, bits);
            steps
                .iter()
                .filter_map(|s| if let Step::Window { len, .. } = s { Some(*len) } else { None })
                .collect()
        };
        let slow = lens(Some(168_000));
        assert_eq!(slow[0], 5_250, "the first window too");
        assert!(slow.iter().all(|l| *l == 5_250), "{slow:?}");
        assert!(lens(Some(16_000_000)).contains(&WINDOW_MAX));
        assert!(lens(Some(8_000)).iter().all(|l| *l == PACED_MIN));
        assert!(lens(None).contains(&WINDOW_MAX), "no rate, no bound");
    }

    #[test]
    fn behind_a_seek_the_walk_scans_across_windows_to_the_next_cluster() {
        let mut file = vec![0xA5; 50_000];
        let at = file.len() as u64;
        cluster(&mut file, 7, &[100, 100]);
        let steps = walk(&file, 10, true);
        let to =
            steps.iter().find_map(|s| if let Step::Skip { to } = s { Some(*to) } else { None });
        assert_eq!(to, Some(at), "the first place walked to is the Cluster");
        assert_eq!(steps.iter().filter(|s| matches!(s, Step::Block { .. })).count(), 2);
    }

    #[test]
    fn a_laced_block_or_a_group_past_the_window_is_read_whole() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100]);
        let start = file.len() as u64;
        let mut laced = vec![0x81, 0, 0, 0x84, 1];
        laced.extend(std::iter::repeat_n(9, 30_000));
        element(&mut file, id::SIMPLE_BLOCK, &laced);
        let group_at = file.len() as u64;
        let mut group = Vec::new();
        let mut block = vec![0x81, 0, 1, 0];
        block.extend(std::iter::repeat_n(3, 100_000));
        element(&mut group, id::BLOCK, &block);
        ebml::write_uint(&mut group, id::BLOCK_DURATION, 5);
        element(&mut file, id::BLOCK_GROUP, &group);
        let steps = walk(&file, 0, false);
        let whole: Vec<_> = steps
            .iter()
            .filter_map(|s| {
                if let Step::Window { offset, whole: Some(_), .. } = s {
                    Some(*offset)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(whole, [start, group_at], "each read whole into a window of its length");
        let grouped = steps.iter().any(|s| {
            matches!(s, Step::Block { keyframe: Some(true), duration_ns: Some(5_000_000), .. })
        });
        assert!(grouped, "the group's block, a keyframe with no reference, with its duration");
        assert!(!steps.iter().any(|s| matches!(s, Step::Frame { .. })));
    }

    #[test]
    fn an_element_nobody_reads_is_walked_past_unread() {
        let mut file = Vec::new();
        cluster(&mut file, 0, &[100]);
        element(&mut file, id::CUES, &vec![0; 1 << 20]);
        cluster(&mut file, 9, &[100]);
        let read: u64 = walk(&file, 0, false)
            .iter()
            .filter_map(|s| if let Step::Window { len, .. } = s { Some(*len as u64) } else { None })
            .sum();
        assert!(read < 1 << 18, "{read} bytes read for a file of {}", file.len());
    }
}
