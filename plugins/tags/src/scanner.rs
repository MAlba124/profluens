//! The scan engine. A fixed array of state-machine slots driven against a reactor.
//! Slot `i` submits as `ElementId(i)`, so routing a completion is an array index.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::event::TagSink;
use profluens_core::id::{ElementId, FormatId};
use profluens_core::io::{
    Completion, Handle, IoError, IoResult, Name, Op, OpId, OpenMode, Reactor, ReactorFactory,
    Submission,
};
use profluens_core::memory::{Memory, Pool};
use profluens_core::scratch::Arena;
use profluens_core::time::Timestamp;

use crate::sniff::{Format, sniff};
use crate::store::ArenaSink;
use crate::{
    DEFAULT_PREFIX, FileMeta, Props, ScanConfig, ScanError, ScanOutcome, ScanResult, aiff, ape,
    flac, mkv, mp3, mp4, mpc, ogg, tail, wav, wv,
};

/// Ceiling on one metadata extent read. Bounds what a hostile file can make us allocate.
const MAX_METADATA: u64 = 32 << 20;

const MAX_EXTENTS: u32 = 4;

/// Byte budget for parked oversized extent buffers. Heap-exact pool buffers are freed on drop, not
/// recycled.
const SPARE_EXTENT_BYTES: usize = 8 << 20;
const SPARE_EXTENT_MAX: usize = 2 << 20;

/// Max oversize of a reused parked buffer as a shift of the request, 0 means up to 2x. Bounds
/// wasted IO.
const SPARE_SLACK_SHIFT: u32 = 0;

/// Which reactor backend a [`Scanner`] ended up with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReactorKind {
    /// `pf-reactor` on epoll, when no ring could be had.
    Sync,
    IoUring,
    Custom,
}

/// A parallel tag scanner, one per thread.
pub struct Scanner {
    cfg: ScanConfig,
    reactor: Box<dyn Reactor>,
    kind: ReactorKind,
    pool: Pool,
    /// Per-file scratch for interned tag text, reset after every callback.
    arena: Arena,
    slots: Vec<Slot>,
    subs: Vec<Submission>,
    names: Vec<u8>,
    comps: Vec<(ElementId, Completion)>,
    spare: Vec<Memory>,
    spare_bytes: usize,
    next_op: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Prefix,
    Extent,
    Tail,
}

#[derive(Clone, Copy, Default)]
struct Fill {
    base: u64,
    want: usize,
    got: usize,
    bouncing: bool,
}

struct Slot {
    phase: Phase,
    handle: Handle,
    path: PathBuf,
    meta: FileMeta,
    format: Format,
    body: u64,
    prefix: Option<Memory>,
    extent: Option<Memory>,
    extent_base: u64,
    tail: Option<Memory>,
    tail_base: u64,
    tail_done: bool,
    bounce: Option<Memory>,
    fill: Fill,
    extents: u32,
}

impl Slot {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            handle: Handle::NONE,
            path: PathBuf::new(),
            meta: FileMeta { len: 0, mtime: SystemTime::UNIX_EPOCH },
            format: Format::Unknown,
            body: 0,
            prefix: None,
            extent: None,
            extent_base: 0,
            tail: None,
            tail_base: 0,
            tail_done: false,
            bounce: None,
            fill: Fill::default(),
            extents: 0,
        }
    }

    fn release(&mut self) {
        self.phase = Phase::Idle;
        self.prefix = None;
        self.extent = None;
        self.extent_base = 0;
        self.tail = None;
        self.tail_base = 0;
        self.tail_done = false;
        self.format = Format::Unknown;
        self.body = 0;
        self.fill = Fill::default();
        self.extents = 0;
    }

    fn window(&self) -> Option<&Memory> {
        self.extent.as_ref().or(self.prefix.as_ref())
    }

    fn held(&self) -> Held<'_> {
        Held {
            format: self.format,
            body: self.body,
            file_len: self.meta.len,
            prefix: self.prefix.as_ref().map(|p| p.data()),
            extent: self.extent.as_ref().map(|e| (e.data(), self.extent_base)),
            tail: self.tail.as_ref().map(|t| (t.data(), self.tail_base)),
            extents: self.extents,
            tail_done: self.tail_done,
        }
    }

    fn dest(&mut self) -> Option<&mut Option<Memory>> {
        match self.phase {
            Phase::Prefix => Some(&mut self.prefix),
            Phase::Extent => Some(&mut self.extent),
            Phase::Tail => Some(&mut self.tail),
            Phase::Idle => None,
        }
    }
}

/// One file's slot as plain slices, so planner and parser run without a reactor.
#[derive(Clone, Copy)]
pub(crate) struct Held<'a> {
    pub format: Format,
    pub body: u64,
    pub file_len: u64,
    pub prefix: Option<&'a [u8]>,
    pub extent: Option<(&'a [u8], u64)>,
    pub tail: Option<(&'a [u8], u64)>,
    pub extents: u32,
    pub tail_done: bool,
}

impl<'a> Held<'a> {
    /// The head window. The extent when there is one, otherwise the prefix.
    fn window(&self) -> Option<&'a [u8]> {
        self.extent.map(|(e, _)| e).or(self.prefix)
    }

    fn view(&self) -> (&'a [u8], u64) {
        match self.extent {
            Some(e) => e,
            None => (self.prefix.unwrap_or(&[]), 0),
        }
    }

    fn tail_view(&self) -> (&'a [u8], u64) {
        if let Some(t) = self.tail {
            return t;
        }
        match self.view() {
            (w, 0) if w.len() as u64 >= self.file_len => (w, 0),
            _ => (&[], 0),
        }
    }

    /// Every head window held. Matroska needs both since its SeekHead is at the front.
    fn views(&self, out: &mut [(&'a [u8], u64); 2]) -> usize {
        out[0] = (self.prefix.unwrap_or(&[]), 0);
        match self.extent {
            Some(e) => {
                out[1] = e;
                2
            }
            None => 1,
        }
    }

    fn has_tail(&self) -> bool {
        self.tail.is_some() || matches!(self.view(), (w, 0) if w.len() as u64 >= self.file_len)
    }
}

/// Discards everything. The planning walk must not emit what the parse pass emits again.
struct NullSink;

impl TagSink for NullSink {
    fn text(&mut self, _key: &str, _value: &str) {}
    fn picture(&mut self, _mime: &str, _data: &[u8]) {}
}

impl Scanner {
    /// A scanner with its own pool.
    pub fn new(cfg: ScanConfig) -> Self {
        let pool = default_pool(cfg, 1);
        Self::with_pool(cfg, pool)
    }

    /// A scanner drawing buffers from a shared `pool`.
    pub fn with_pool(cfg: ScanConfig, pool: Pool) -> Self {
        let (reactor, kind) = default_reactor(cfg);
        Self::build(cfg, pool, reactor, kind)
    }

    /// A scanner whose reactor comes from `factory`. A failing factory falls back to the default.
    pub fn with_reactor(cfg: ScanConfig, pool: Pool, factory: &ReactorFactory) -> Self {
        match (**factory)() {
            Ok(r) => Self::build(cfg, pool, r, ReactorKind::Custom),
            Err(_) => Self::with_pool(cfg, pool),
        }
    }

    // One-time construction per thread.
    #[allow(clippy::disallowed_methods)]
    fn build(cfg: ScanConfig, pool: Pool, reactor: Box<dyn Reactor>, kind: ReactorKind) -> Self {
        let n = cfg.slots();
        let mut slots = Vec::with_capacity(n);
        slots.resize_with(n, Slot::new);
        Self {
            cfg,
            reactor,
            kind,
            pool,
            arena: Arena::default(),
            slots,
            subs: Vec::with_capacity(n),
            names: Vec::new(),
            comps: Vec::with_capacity(n),
            spare: Vec::with_capacity(n),
            spare_bytes: 0,
            next_op: 0,
        }
    }

    pub fn reactor_kind(&self) -> ReactorKind {
        self.kind
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Scan every path, calling `emit` once per path in completion order, not input order.
    /// Everything `emit` gets is borrowed for the call only.
    pub fn scan<P, E>(&mut self, paths: impl IntoIterator<Item = P>, mut emit: E)
    where
        P: AsRef<Path>,
        E: FnMut(ScanOutcome<'_>),
    {
        let mut paths = paths.into_iter().peekable();
        let mut drained = false;
        let mut stalls = 0u32;

        loop {
            while !drained {
                let Some(idx) = self.free_slot() else { break };
                if paths.peek().is_none() {
                    drained = true;
                    break;
                }
                let Some(mem) = self.acquire_prefix() else {
                    break;
                };
                let Some(path) = paths.next() else { break };
                self.admit(idx, path.as_ref(), mem, &mut emit);
            }

            let busy = self.busy();
            if busy == 0 {
                if drained {
                    break;
                }
                continue;
            }

            self.reactor.submit(&mut self.subs, &self.names);
            self.names.clear();
            let mut comps = std::mem::take(&mut self.comps);
            self.reactor.run_once(&mut comps);

            if comps.is_empty() && self.reactor.park(STALL) {
                self.comps = comps;
                continue;
            }
            if comps.is_empty() {
                // Ops outstanding but nothing returned twice running means no progress. Fail
                // in-flight files.
                stalls += 1;
                if stalls > 1 {
                    self.comps = comps;
                    for i in 0..self.slots.len() {
                        if self.slots[i].phase != Phase::Idle {
                            self.fail(i, ScanError::Cancelled, &mut emit);
                        }
                    }
                    stalls = 0;
                    continue;
                }
            } else {
                stalls = 0;
            }

            for (element, c) in comps.drain(..) {
                let idx = element.0 as usize;
                if idx < self.slots.len() {
                    self.complete(idx, c, &mut emit);
                }
            }
            self.comps = comps;
        }
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.phase == Phase::Idle)
    }

    fn busy(&self) -> usize {
        self.slots.iter().filter(|s| s.phase != Phase::Idle).count()
    }

    // Stat for mtime, no reactor op carries it. No scheduler group to stall.
    #[allow(clippy::disallowed_methods)]
    fn admit(
        &mut self,
        idx: usize,
        path: &Path,
        mem: Memory,
        emit: &mut impl FnMut(ScanOutcome<'_>),
    ) {
        self.slots[idx].path.clear();
        self.slots[idx].path.push(path);

        let md = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => return self.fail(idx, ScanError::Open(e.kind()), emit),
        };
        if !md.is_file() {
            return self.fail(idx, ScanError::Open(std::io::ErrorKind::InvalidInput), emit);
        }
        let meta =
            FileMeta { len: md.len(), mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH) };
        self.slots[idx].meta = meta;

        if meta.len == 0 {
            self.slots[idx].format = Format::Unknown;
            self.slots[idx].phase = Phase::Prefix; // occupied, so `finish` may release it
            return self.finish(idx, emit);
        }

        let bytes = path.as_os_str().as_encoded_bytes();
        let name = Name { off: self.names.len() as u32, len: bytes.len() as u32 };
        self.names.extend_from_slice(bytes);
        let op = self.op_id(idx);
        self.subs.push(Submission {
            op,
            element: ElementId(idx as u32),
            kind: Op::Open { path: name, mode: OpenMode::Read },
            buf: None,
            user: OPEN,
        });

        let want = mem.capacity().min(usize::try_from(meta.len).unwrap_or(usize::MAX));
        self.slots[idx].prefix = Some(mem);
        self.slots[idx].phase = Phase::Prefix;
        self.slots[idx].fill = Fill { base: 0, want, got: 0, bouncing: false };
    }

    fn op_id(&mut self, idx: usize) -> OpId {
        let op = OpId(((idx as u64) << 32) | (self.next_op & 0xFFFF_FFFF));
        self.next_op += 1;
        op
    }

    fn close(&mut self, idx: usize) {
        let handle = std::mem::replace(&mut self.slots[idx].handle, Handle::NONE);
        if !handle.is_none() {
            let op = self.op_id(idx);
            let element = ElementId(idx as u32);
            self.subs.push(Submission {
                op,
                element,
                kind: Op::Close { handle },
                buf: None,
                user: CLOSE,
            });
        }
    }

    /// A prefix buffer, or `None` while a file in flight will hand one back.
    /// With nothing in flight it uses `acquire_exact` so a drained pool cannot deadlock.
    fn acquire_prefix(&mut self) -> Option<Memory> {
        if let Some(m) = self.pool.try_acquire() {
            return Some(m);
        }
        if self.busy() == 0 {
            return self.pool.acquire_exact(self.pool.slot_size());
        }
        None
    }

    fn submit(&mut self, idx: usize, offset: u64, bounce: bool) {
        let mem = if bounce {
            let remaining = self.slots[idx].fill.want - self.slots[idx].fill.got;
            match self.slots[idx].bounce.take() {
                Some(m) if m.capacity() >= remaining => m,
                _ => match self.pool.acquire_exact(remaining.min(self.pool.slot_size())) {
                    Some(m) => m,
                    None => return,
                },
            }
        } else {
            match self.slots[idx].dest().and_then(Option::take) {
                Some(m) => m,
                None => return,
            }
        };
        self.slots[idx].fill.bouncing = bounce;
        let op = self.op_id(idx);
        self.subs.push(Submission {
            op,
            element: ElementId(idx as u32),
            kind: Op::Read { handle: self.slots[idx].handle, offset, at: 0, len: u32::MAX },
            buf: Some(wrap(mem)),
            user: u64::from(bounce),
        });
    }

    fn complete(&mut self, idx: usize, c: Completion, emit: &mut impl FnMut(ScanOutcome<'_>)) {
        // A close is of the file this slot had before, whatever it holds now.
        if c.user == CLOSE || self.slots[idx].phase == Phase::Idle {
            return; // or a straggler for a file already finished (a cancelled op, say)
        }
        if c.user == OPEN {
            return match c.result {
                IoResult::File { handle, .. } => {
                    self.slots[idx].handle = handle;
                    self.submit(idx, 0, false)
                }
                IoResult::Err(e) => self.fail(idx, ScanError::Open(error_kind(e)), emit),
                _ => self.fail(idx, ScanError::Cancelled, emit),
            };
        }
        let (n, mem) = match (c.result, c.buf) {
            (IoResult::Ok(n), Some(buf)) => (n, buf.memory),
            (IoResult::Err(e), _) => return self.fail(idx, ScanError::Io(error_kind(e)), emit),
            _ => return self.fail(idx, ScanError::Cancelled, emit),
        };

        if self.slots[idx].fill.bouncing {
            // Short read. The remainder went to the bounce buffer and is appended, the engine's
            // only copy.
            let got = self.slots[idx].fill.got;
            let want = self.slots[idx].fill.want;
            let take = n.min(want.saturating_sub(got));
            let src = mem.data();
            if let Some(dst) = self.slots[idx].dest().and_then(Option::as_mut) {
                dst.as_mut_full()[got..got + take].copy_from_slice(&src[..take]);
                dst.set_len(got + take);
            }
            self.slots[idx].fill.got = got + take;
            self.slots[idx].bounce = Some(mem);
        } else {
            self.slots[idx].fill.got = n;
            let phase = self.slots[idx].phase;
            match phase {
                Phase::Prefix => self.slots[idx].prefix = Some(mem),
                Phase::Extent => self.slots[idx].extent = Some(mem),
                Phase::Tail => self.slots[idx].tail = Some(mem),
                Phase::Idle => return,
            }
        }

        let fill = self.slots[idx].fill;
        let at_eof = fill.base + fill.got as u64 >= self.slots[idx].meta.len;
        if fill.got >= fill.want || at_eof || n == 0 {
            // `n == 0` is EOF even if `stat` said longer, which keeps a shrinking file from
            // looping.
            self.advance(idx, emit);
        } else {
            let next = fill.base + fill.got as u64;
            self.submit(idx, next, true);
        }
    }

    fn advance(&mut self, idx: usize, emit: &mut impl FnMut(ScanOutcome<'_>)) {
        match self.slots[idx].phase {
            Phase::Prefix => {
                let head = self.slots[idx].prefix.as_ref().map(|m| m.data()).unwrap_or(&[]);
                let s = sniff(head);
                self.slots[idx].format = s.format;
                self.slots[idx].body = s.body;
                self.plan(idx, emit);
            }
            Phase::Extent | Phase::Tail => self.plan(idx, emit),
            Phase::Idle => {}
        }
    }

    fn plan(&mut self, idx: usize, emit: &mut impl FnMut(ScanOutcome<'_>)) {
        // An empty planned read must fall through to `finish`, or the slot idles until the stall
        // detector fires.
        let started = match self.next_read(idx) {
            Next::Extent { at, len } => self.start_extent(idx, at, len),
            Next::Tail => self.start_tail(idx),
            Next::Done => false,
        };
        if !started {
            self.finish(idx, emit);
        }
    }

    fn next_read(&self, idx: usize) -> Next {
        self.slots[idx].held().next_read(&self.cfg)
    }
}

impl Held<'_> {
    fn next_extent(&self, cfg: &ScanConfig) -> Option<(u64, usize)> {
        if self.extents >= MAX_EXTENTS {
            return None;
        }
        let slot = self;
        let file_len = slot.file_len;
        match slot.format {
            Format::Wav => {
                let (window, base) = slot.view();
                match slot.extents {
                    0 => {
                        let mut props = Props::default();
                        wav::walk(window, base, file_len, &mut props, &mut NullSink)
                            .map(|at| (at, cfg.tail_len.min(64 << 20)))
                    }
                    _ => None,
                }
            }
            Format::Flac => {
                let window = slot.window();
                match window.map(|w| (w, flac::plan(w, slot.body, file_len))) {
                    // The loop guard. A block running past EOF would otherwise ask for the same
                    // bytes forever.
                    Some((w, flac::Plan::Need { end }))
                        if end > w.len() as u64 && end <= MAX_METADATA =>
                    {
                        Some((0u64, usize::try_from(end).unwrap_or(usize::MAX)))
                    }
                    _ => None,
                }
            }
            Format::Mp3 => {
                let (window, base) = slot.view();
                let end = mp3::head_end(window, file_len);
                // Both hold only when `end > window.len()`, so the request always advances.
                (base == 0 && mp3::head_short(window, file_len) && end <= MAX_METADATA)
                    .then(|| (0u64, usize::try_from(end).unwrap_or(usize::MAX)))
            }
            Format::Mp4 => {
                let (window, base) = slot.view();
                match mp4::locate(window, base, file_len) {
                    mp4::Plan::Need { at, len } => {
                        // Clamped. Each hop starts strictly later, so the walk advances.
                        let want = len.min(MAX_METADATA);
                        // A `moov` over `MAX_METADATA` answers `Beyond` forever, so only ask for
                        // bytes not yet held.
                        (at != base || want > window.len() as u64)
                            .then(|| (at, usize::try_from(want).unwrap_or(usize::MAX)))
                    }
                    _ => None,
                }
            }
            Format::Aiff => match slot.extents {
                0 => {
                    let (window, base) = slot.view();
                    aiff::plan(window, base, file_len).map(|at| (at, cfg.tail_len.min(64 << 20)))
                }
                _ => None,
            },
            Format::OggOpus | Format::OggVorbis | Format::OggFlac | Format::OggSpeex => {
                let (window, base) = slot.view();
                if base != 0 {
                    return None;
                }
                let want = ogg::want(window, slot.body, slot.format);
                match ogg::plan(window, slot.body, file_len, want) {
                    Some(end) if end > window.len() as u64 && end <= ogg::MAX_META => {
                        Some((0u64, usize::try_from(end).unwrap_or(usize::MAX)))
                    }
                    _ => None,
                }
            }
            Format::Mkv => {
                // Two reads, not four. A second extent replaces the first, so a third would chase
                // forever.
                if slot.extents >= 2 {
                    return None;
                }
                let mut buf = [(&[][..], 0u64); 2];
                let n = slot.views(&mut buf);
                let (at, len) = mkv::plan(&buf[..n], file_len)?;
                // Only when the request reaches bytes not yet held, or a bad SeekHead position
                // loops.
                let held = slot.extent.is_some_and(|(e, base)| {
                    at >= base && at.saturating_add(len) <= base.saturating_add(e.len() as u64)
                });
                (!held && len > 0).then(|| (at, usize::try_from(len).unwrap_or(usize::MAX)))
            }
            Format::Ape | Format::WavPack | Format::Musepack => None,
            Format::Unknown => None,
        }
    }

    /// What this file still needs. The tail is issued after the head so they never share the single
    /// `Fill`.
    pub(crate) fn next_read(&self, cfg: &ScanConfig) -> Next {
        if let Some((at, len)) = self.next_extent(cfg) {
            return Next::Extent { at, len };
        }
        let wanted = wants_tail(self.format) && !self.tail_done && !self.has_tail();
        if wanted && self.file_len > 0 && cfg.tail_len > 0 {
            return Next::Tail;
        }
        Next::Done
    }
}

impl Scanner {
    fn start_tail(&mut self, idx: usize) -> bool {
        // Marked before the size checks below, so no path can ask for the tail twice.
        self.slots[idx].tail_done = true;
        let file_len = self.slots[idx].meta.len;
        let want = u64::try_from(self.cfg.tail_len).unwrap_or(u64::MAX).min(file_len);
        let at = file_len - want;
        let Ok(want) = usize::try_from(want) else {
            return false;
        };
        if want == 0 {
            return false;
        }
        let Some(mem) = self.pool.acquire_exact(want) else {
            return false;
        };
        self.slots[idx].tail_base = at;
        self.slots[idx].tail = Some(mem);
        self.slots[idx].phase = Phase::Tail;
        self.slots[idx].fill = Fill { base: at, want, got: 0, bouncing: false };
        self.submit(idx, at, false);
        true
    }

    fn start_extent(&mut self, idx: usize, at: u64, len: usize) -> bool {
        let room =
            usize::try_from(self.slots[idx].meta.len.saturating_sub(at)).unwrap_or(usize::MAX);
        let want = len.min(room);
        if want == 0 {
            return false;
        }
        // Give up the superseded extent first so one file never holds two `MAX_METADATA` buffers.
        self.recycle_extent(idx);
        let Some(mem) = self.acquire_extent(want) else {
            return false;
        };
        self.slots[idx].extent_base = at;
        self.slots[idx].extent = Some(mem);
        self.slots[idx].phase = Phase::Extent;
        self.slots[idx].extents += 1;
        self.slots[idx].fill = Fill { base: at, want, got: 0, bouncing: false };
        self.submit(idx, at, false);
        true
    }

    /// A buffer for a `want`-byte extent. Best fit from the parked ones, else the pool.
    /// The reactor reads the whole capacity, so `SPARE_SLACK_SHIFT` bounds the oversize. `None`
    /// past the exact cap.
    fn acquire_extent(&mut self, want: usize) -> Option<Memory> {
        if want > self.pool.slot_size() {
            let ceiling = want.saturating_add(want >> SPARE_SLACK_SHIFT);
            let best = self
                .spare
                .iter()
                .enumerate()
                .filter(|(_, m)| m.capacity() >= want && m.capacity() <= ceiling)
                .min_by_key(|(_, m)| m.capacity())
                .map(|(i, _)| i);
            if let Some(i) = best {
                let mut mem = self.spare.swap_remove(i);
                self.spare_bytes -= mem.capacity();
                // Clear the stale length so no short path parses another file's bytes.
                mem.set_len(0);
                return Some(mem);
            }
        }
        self.pool.acquire_exact(want)
    }

    /// Park slot `idx`'s heap-exact extent buffer for reuse if the budget has room. Called after
    /// `emit` returns.
    fn recycle_extent(&mut self, idx: usize) {
        let Some(mem) = self.slots[idx].extent.take() else {
            return;
        };
        let cap = mem.capacity();
        if cap <= self.pool.slot_size()
            || cap > SPARE_EXTENT_MAX
            || self.spare.len() >= self.cfg.slots()
            || self.spare_bytes + cap > SPARE_EXTENT_BYTES
        {
            return;
        }
        self.spare_bytes += cap;
        self.spare.push(mem);
    }

    fn finish(&mut self, idx: usize, emit: &mut impl FnMut(ScanOutcome<'_>)) {
        {
            let slot = &self.slots[idx];
            let arena = &self.arena;
            // Pictures become sub-views of this window, so cover art never leaves the read buffer.
            let mut sink = ArenaSink::new(arena, &self.pool, slot.window());
            let props = slot.held().parse(arena, &mut sink);

            emit(ScanOutcome {
                path: &slot.path,
                result: Ok(ScanResult {
                    format: slot.format,
                    file: slot.meta,
                    props,
                    tags: sink.finish(),
                }),
            });
        }
        self.arena.reset();
        self.recycle_extent(idx);
        self.close(idx);
        self.slots[idx].release();
    }

    fn fail(&mut self, idx: usize, err: ScanError, emit: &mut impl FnMut(ScanOutcome<'_>)) {
        emit(ScanOutcome { path: &self.slots[idx].path, result: Err(err) });
        self.recycle_extent(idx);
        self.close(idx);
        self.slots[idx].release();
    }
}

impl Held<'_> {
    pub(crate) fn parse(&self, arena: &Arena, sink: &mut impl TagSink) -> Props {
        let slot = self;
        let mut props = Props::default();
        let file_len = slot.file_len;

        match slot.format {
            Format::Wav => {
                let mut carry = wav::Carry::default();
                if let Some(p) = slot.prefix {
                    wav::walk_carrying(p, 0, file_len, &mut carry, &mut props, sink);
                }
                if let Some((e, base)) = slot.extent {
                    wav::walk_carrying(e, base, file_len, &mut carry, &mut props, sink);
                }
            }
            // Pass is the outer loop. `Tags::get` returns the first value per key, so ID3 must
            // win across both windows.
            Format::Aiff => {
                for pass in aiff::Pass::ORDER {
                    if let Some(p) = slot.prefix {
                        aiff::walk_pass(p, 0, file_len, &mut props, pass, arena, sink);
                    }
                    if let Some((e, base)) = slot.extent {
                        aiff::walk_pass(e, base, file_len, &mut props, pass, arena, sink);
                    }
                }
            }
            Format::Ape | Format::WavPack | Format::Musepack => {
                let (head, base) = slot.view();
                let head = if base == 0 { head } else { &[][..] };
                props = match slot.format {
                    Format::Ape => ape::props(head, slot.body),
                    Format::WavPack => wv::props(head, slot.body),
                    _ => mpc::props(head, slot.body),
                };
                let (tail_window, tail_base) = slot.tail_view();
                let trailers = tail::measure(tail_window, tail_base, file_len);
                tail::emit(tail_window, tail_base, file_len, trailers, arena, sink);
            }
            Format::Flac => {
                if let Some(w) = slot.window() {
                    props = flac::props(w, slot.body);
                    flac::emit_tags(w, slot.body, sink);
                }
            }
            Format::Mp3 => {
                let (head, base) = slot.view();
                let head = if base == 0 { head } else { &[][..] };
                let (tail, tail_base) = slot.tail_view();
                let v2 = mp3::v2_len(head);
                mp3::emit_v2(head, v2, arena, sink);
                let trailers = tail::measure(tail, tail_base, file_len);
                tail::emit(tail, tail_base, file_len, trailers, arena, sink);
                // Tag bytes are not audio and would inflate the CBR estimate.
                let audio = file_len.saturating_sub(v2).saturating_sub(trailers.total());
                props = mp3::props(head, v2, audio);
            }
            Format::Mp4 => {
                let (w, base) = slot.view();
                if let mp4::Plan::Here { at, len } = mp4::locate(w, base, file_len) {
                    props = mp4::parse(w, at, len, arena, sink);
                }
            }
            Format::OggOpus | Format::OggVorbis | Format::OggFlac | Format::OggSpeex => {
                let (w, base) = slot.view();
                let w = if base == 0 { w } else { &[][..] };
                let head = ogg::scan(w, slot.body, slot.format, arena, sink);
                props = head.props;
                // The last granule is the only duration an Ogg file states (RFC 3533 section 6).
                let (tail, _) = slot.tail_view();
                if let Some(ns) = ogg::duration(tail, &head, slot.format) {
                    props.duration_ns = Some(ns);
                    // Speex lacks the encoder lookahead, so its value is a floor.
                    props.duration_exact = slot.format != Format::OggSpeex;
                }
            }
            Format::Mkv => {
                let mut buf = [(&[][..], 0u64); 2];
                let n = slot.views(&mut buf);
                props = mkv::parse(&buf[..n], sink);
            }
            Format::Unknown => {}
        }
        props
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Next {
    Extent { at: u64, len: usize },
    Tail,
    Done,
}

/// Whether a format's metadata also lives at EOF. WAV and AIFF trailers are extents found by the
/// chunk walk.
fn wants_tail(format: Format) -> bool {
    matches!(
        format,
        Format::Mp3
            | Format::Ape
            | Format::WavPack
            | Format::Musepack
            | Format::OggOpus
            | Format::OggVorbis
            | Format::OggFlac
            | Format::OggSpeex
    )
}

/// Pool for `scanners` scanners. Room for every slot's prefix, extent and tail plus bounce margin.
pub(crate) fn default_pool(cfg: ScanConfig, scanners: usize) -> Pool {
    let slots = scanners.max(1) * cfg.slots() * 3 + 4;
    Pool::bounded(DEFAULT_PREFIX, u32::try_from(slots).unwrap_or(u32::MAX))
}

// One-time reactor box per thread.
#[allow(clippy::disallowed_methods)]
fn default_reactor(cfg: ScanConfig) -> (Box<dyn Reactor>, ReactorKind) {
    // The ring may fail to set up (old kernel, seccomp, RLIMIT_MEMLOCK), then epoll.
    // With neither, every file fails with the reason.
    match pf_reactor::OsReactor::files(pf_reactor::Want::Any, 2 * cfg.slots()) {
        Ok(r) if r.backend() == pf_reactor::Backend::Uring => (Box::new(r), ReactorKind::IoUring),
        Ok(r) => (Box::new(r), ReactorKind::Sync),
        Err(e) => (Box::new(Refusing(e, Vec::new())), ReactorKind::Sync),
    }
}

/// Fails every op with why no reactor could be set up.
struct Refusing(IoError, Vec<(ElementId, Completion)>);

impl Reactor for Refusing {
    fn submit(&mut self, subs: &mut Vec<Submission>, _names: &[u8]) {
        for s in subs.drain(..).filter(|s| !matches!(s.kind, Op::Cancel { .. })) {
            let c =
                Completion { op: s.op, user: s.user, result: IoResult::Err(self.0), buf: s.buf };
            self.1.push((s.element, c));
        }
    }

    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        out.clear();
        out.append(&mut self.1);
    }

    fn due_in(&self) -> Option<std::time::Duration> {
        None
    }

    fn is_idle(&self) -> bool {
        self.1.is_empty()
    }
}

/// How long a scan waits in its reactor for an op before it counts a stall.
const STALL: std::time::Duration = std::time::Duration::from_millis(500);

/// `user` tags of the ops that are not reads. A read carries zero or one.
const OPEN: u64 = 2;
const CLOSE: u64 = 3;

fn error_kind(e: IoError) -> std::io::ErrorKind {
    use std::io::ErrorKind;
    match e {
        IoError::NotFound => ErrorKind::NotFound,
        IoError::Denied => ErrorKind::PermissionDenied,
        IoError::NotAFile => ErrorKind::InvalidInput,
        IoError::Os(n) => std::io::Error::from_raw_os_error(n).kind(),
        _ => ErrorKind::Other,
    }
}

fn wrap(memory: Memory) -> Buffer {
    Buffer {
        memory,
        pts: Timestamp::ZERO,
        dts: Timestamp::ZERO,
        duration: Timestamp::ZERO,
        flags: BufferFlags::empty(),
        format: FormatId(0),
        sync: None,
    }
}
