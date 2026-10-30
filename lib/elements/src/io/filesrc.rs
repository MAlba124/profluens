//! `filesrc` reads a file into pooled buffers through the reactor, keeping up to
//! `credits` positioned reads in flight. A named pipe is read in order, one receive at a time.

// A caller's open file, see `FileSrc::from_file`.
#[allow(clippy::disallowed_types)]
use std::fs::File;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use profluens_core::batch::Inputs;
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, OfferDesc};
use profluens_core::id::SrcPad;
use profluens_core::io::{Handle, IoResult, NO_LEN, OpenMode};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::pipeline::Serves;
use profluens_core::time::Timestamp;

use super::error::FileError;

/// A file is an untyped byte stream.
static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

/// The file to read.
static PROPS: [PropDesc; 1] = [PropDesc { name: "path", allowed: Constraint::Any, live: false }];

// Cold, one box per registry-created element.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "filesrc",
    sinks: &[],
    srcs: &PADS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(FileSrc::new(""))),
    io: true,
    couplings: &[],
};

#[derive(Default)]
struct Slot {
    /// Offset the read was submitted at, used by the short-read repair.
    offset: u64,
    buf: Option<profluens_core::buffer::Buffer>,
}

const OPEN: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct Range {
    seq: u64,
    tag: u64,
    offset: u64,
    len: u32,
    got: u32,
}

pub struct FileSrc {
    /// The file to open, or only the name of `held` in logs and errors.
    path: PathBuf,
    /// A file opened by the caller, handed to the reactor on every start instead of a path.
    // Held only for the reactor to duplicate, a confined open must not be resolved again.
    #[allow(clippy::disallowed_types)]
    held: Option<File>,
    file: Handle,
    /// A pipe has no offsets, so it is read in order, one receive at a time.
    piped: bool,
    started: bool,
    offset: u64,
    eof: bool,
    stamp: Timestamp,
    in_flight: u32,
    seq: u64,
    /// Reads with a `seq` below this were submitted before a seek and are discarded.
    valid_from: u64,
    /// Re-sequencing window. The reactor completes reads in any order, so completions
    /// park here by `seq`. Sized once in `start()`, power-of-two for mask indexing.
    window: Vec<Slot>,
    wmask: u64,
    /// Filled slots. A fast-out for the many empty `process()` passes, measured at ~5%.
    parked: u32,
    next_emit: u64,
    /// Drop read pages from the page cache behind the read position.
    once: bool,
    offer: bool,
    ranges: Vec<Option<Range>>,
}

impl FileSrc {
    pub const OUT: SrcPad = SrcPad(0);

    // Cold, one-time construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            held: None,
            file: Handle::NONE,
            piped: false,
            started: false,
            offset: 0,
            eof: false,
            stamp: Timestamp::NONE,
            in_flight: 0,
            seq: 0,
            valid_from: 0,
            window: Vec::new(),
            wmask: 0,
            parked: 0,
            next_emit: 0,
            once: false,
            offer: true,
            ranges: Vec::new(),
        }
    }

    /// Reads `file`, already open, so no path is resolved again. `name` labels logs and errors.
    #[allow(clippy::disallowed_types)]
    pub fn from_file(file: File, name: impl AsRef<Path>) -> Self {
        Self { held: Some(file), ..Self::new(name) }
    }

    pub fn push_only(mut self) -> Self {
        self.offer = false;
        self
    }

    /// Drop read pages from the page cache, for stream-once workloads larger than memory.
    pub fn read_once(mut self) -> Self {
        self.once = true;
        self
    }

    #[inline]
    fn slot_of(&self, seq: u64) -> Option<usize> {
        let len = self.window.len() as u64;
        (len > 0 && seq >= self.next_emit && seq < self.next_emit + len)
            .then_some((seq & self.wmask) as usize)
    }

    /// Abandon every read from `seq` on and rewind submission. `in_flight` stays untouched
    /// until those completions arrive.
    fn abandon_from(&mut self, seq: u64) {
        for slot in &mut self.window {
            slot.buf = None; // buffers recycle on drop
        }
        self.parked = 0;
        self.valid_from = seq;
        self.next_emit = seq;
    }

    fn emit(
        &mut self,
        ctx: &mut Ctx,
        mut buf: profluens_core::buffer::Buffer,
        submitted_at: u64,
    ) -> bool {
        let n = buf.memory.len();
        let cap = buf.memory.capacity();
        if n == 0 {
            // EOF. Every later read was submitted past the end, so abandon them.
            drop(buf);
            self.abandon_from(self.seq);
            self.eof = true;
            log!(&*ctx, Level::Debug, "eof", offset = submitted_at);
            return true;
        }
        self.next_emit += 1;
        log!(&*ctx, Level::Trace, "read", bytes = n);
        buf.pts = std::mem::replace(&mut self.stamp, Timestamp::NONE);
        ctx.out(Self::OUT).push(buf);
        if n < cap {
            // A short read. Every later read was submitted too far ahead, so resume after it.
            log!(&*ctx, Level::Trace, "short_read", offset = submitted_at, bytes = n);
            self.abandon_from(self.seq);
            self.offset = submitted_at + n as u64;
            return true;
        }
        false
    }
}

impl FileSrc {
    fn open_done(&mut self, ctx: &mut Ctx, result: IoResult) -> Result<(), Error> {
        match result {
            IoResult::File { handle, len } => {
                (self.file, self.piped) = (handle, len == NO_LEN);
                log!(&*ctx, Level::Info, "open", len = len, piped = len == NO_LEN);
                Ok(())
            }
            IoResult::Err(kind) => {
                let path = self.path.as_path().into();
                Err(Error::element_failed(ctx.element(), FileError::Open { path, kind }))
            }
            _ => Ok(()),
        }
    }

    fn serve_ranges(&mut self, ctx: &mut Ctx) -> Result<Flow, Error> {
        while let Some(c) = ctx.io().next_completion() {
            if c.user == OPEN {
                self.open_done(ctx, c.result)?;
                if self.piped {
                    // A pipe has no offsets. Nothing was read, so the consumer takes the push.
                    ctx.withdraw_ranges(Self::OUT);
                    return Ok(Flow::Ok);
                }
                continue;
            }
            self.in_flight = self.in_flight.saturating_sub(1);
            let Some(at) = self.ranges.iter().position(|r| r.is_some_and(|r| r.seq == c.user))
            else {
                continue; // a read a seek left, its buffer goes back to the consumer's pool
            };
            let Some(mut r) = self.ranges[at] else {
                continue;
            };
            match (c.result, c.buf) {
                (IoResult::Ok(n), Some(buf)) => {
                    r.got += n as u32;
                    if n > 0 && r.got < r.len {
                        let off = r.offset + r.got as u64;
                        ctx.io().read_range(self.file, off, r.got, r.len - r.got, buf, r.seq);
                        self.in_flight += 1;
                        self.ranges[at] = Some(r);
                        continue;
                    }
                    self.ranges[at] = None;
                    log!(&*ctx, Level::Trace, "range", offset = r.offset, bytes = r.got);
                    ctx.serve(Self::OUT, r.tag, buf);
                }
                (IoResult::Err(kind), _) => {
                    let path = self.path.as_path().into();
                    let read =
                        FileError::Read { path, offset: Some(r.offset + r.got as u64), kind };
                    return Err(Error::element_failed(ctx.element(), read));
                }
                _ => {
                    log!(&*ctx, Level::Warn, "range_dropped", offset = r.offset, tag = r.tag);
                    self.ranges[at] = None;
                }
            }
        }
        let credits = ctx.io().credits();
        while !self.file.is_none() && self.in_flight < credits {
            let Some(free) = self.ranges.iter().position(Option::is_none) else {
                break;
            };
            let Some(ask) = ctx.next_ask(Self::OUT) else {
                break;
            };
            let seq = self.seq;
            self.seq += 1;
            ctx.io().read_range(self.file, ask.offset, 0, ask.len, ask.buf, seq);
            self.ranges[free] =
                Some(Range { seq, tag: ask.tag, offset: ask.offset, len: ask.len, got: 0 });
            self.in_flight += 1;
        }
        let idle = self.in_flight == 0 && self.ranges.iter().all(Option::is_none);
        if idle && ctx.asks_over(Self::OUT) {
            log!(&*ctx, Level::Info, "eos", served = self.seq);
            return Ok(Flow::Eos);
        }
        Ok(Flow::Ok)
    }
}

impl Element for FileSrc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        if self.held.is_none()
            && let Some(profluens_core::format::Value::Id(id)) = ctx.prop("path")
            && let Some(s) = ctx.value_name(id)
        {
            self.path = PathBuf::from(s);
        }
        // A restart after stop() reads from byte 0. Reads left by the old run fall below
        // the `seq` floor, and their completions no longer count against `in_flight`.
        self.abandon_from(self.seq);
        (self.file, self.offset, self.eof, self.stamp, self.in_flight) =
            (Handle::NONE, 0, false, Timestamp::NONE, 0);
        self.ranges.iter_mut().for_each(|r| *r = None);
        let mode = if self.once { OpenMode::ReadOnce } else { OpenMode::Read };
        match &self.held {
            Some(file) => ctx.io().adopt(file.as_fd(), mode, OPEN),
            None => ctx.io().open(&self.path, mode, OPEN),
        };
        // Cold, sized once per run to the in-flight read budget.
        #[allow(clippy::disallowed_methods)]
        {
            let depth = (ctx.io().credits() as usize).max(1).next_power_of_two();
            self.window.resize_with(depth, Slot::default);
            self.wmask = depth as u64 - 1;
            self.ranges.resize(ctx.io().credits() as usize, None);
        }
        if self.offer {
            ctx.offer_ranges(Self::OUT);
        }
        self.started = true;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.started {
            return Err(Error::element_failed(
                ctx.element(),
                FileError::NotStarted { element: "filesrc" },
            ));
        }

        if ctx.pulled(Self::OUT) {
            return self.serve_ranges(ctx);
        }

        loop {
            let completion = ctx.io().next_completion();
            let c = match completion {
                Some(c) => c,
                None => break,
            };
            if c.user == OPEN {
                match c.result {
                    IoResult::File { handle, len } => {
                        (self.file, self.piped) = (handle, len == NO_LEN);
                        log!(&*ctx, Level::Info, "open", len = len, piped = len == NO_LEN);
                    }
                    IoResult::Err(kind) => {
                        let path = self.path.as_path().into();
                        return Err(Error::element_failed(
                            ctx.element(),
                            FileError::Open { path, kind },
                        ));
                    }
                    _ => {}
                }
                continue;
            }
            self.in_flight = self.in_flight.saturating_sub(1);
            if c.user < self.valid_from {
                continue; // read submitted before a seek: wrong offset, discard (buf recycles)
            }
            match (c.result, c.buf) {
                (IoResult::Ok(_), Some(buf)) => {
                    let Some(slot) = self.slot_of(c.user) else {
                        return Err(Error::element_failed(
                            ctx.element(),
                            FileError::CompletionOutsideWindow { element: "filesrc", seq: c.user },
                        ));
                    };
                    if self.parked == 0 && c.user == self.next_emit {
                        let at = self.window[slot].offset;
                        if self.emit(ctx, buf, at) {
                            break; // EOF or short read: the rest of the mailbox is stale
                        }
                    } else {
                        self.window[slot].buf = Some(buf);
                        self.parked += 1;
                    }
                }
                (IoResult::Err(k), _) => {
                    let path = self.path.as_path().into();
                    return Err(Error::element_failed(
                        ctx.element(),
                        FileError::Read { path, offset: None, kind: k },
                    ));
                }
                _ => {
                    // A cancelled read would block its window slot forever, so resync from here.
                    // The scheduler does not cancel yet.
                    if let Some(slot) = self.slot_of(c.user) {
                        let at = self.window[slot].offset;
                        log!(&*ctx, Level::Warn, "read_cancelled", offset = at);
                        self.abandon_from(self.seq);
                        self.offset = at;
                        break;
                    }
                }
            }
        }

        while self.parked > 0 {
            let Some(slot) = self.slot_of(self.next_emit) else {
                break;
            };
            let Some(buf) = self.window[slot].buf.take() else {
                break;
            };
            self.parked -= 1;
            let at = self.window[slot].offset;
            if self.emit(ctx, buf, at) {
                break;
            }
        }

        // A read is submitted only while its window slot is free.
        let credits = if self.piped { 1 } else { ctx.io().credits() };
        let horizon = self.next_emit + self.window.len() as u64;
        while !self.file.is_none() && !self.eof && self.in_flight < credits && self.seq < horizon {
            let buf = match ctx.try_alloc() {
                Some(b) => b,
                None => break, // pool full, backpressure
            };
            let cap = buf.memory.capacity() as u64;
            let user = self.seq;
            let Some(slot) = self.slot_of(user) else {
                break;
            };
            self.window[slot].offset = self.offset;
            self.seq += 1;
            match self.piped {
                true => ctx.io().recv(self.file, buf, user),
                false => ctx.io().read(self.file, self.offset, buf, user),
            };
            self.offset += cap;
            self.in_flight += 1;
        }

        if self.eof && self.in_flight == 0 {
            log!(&*ctx, Level::Info, "eos", offset = self.offset);
            Ok(Flow::Eos)
        } else {
            Ok(Flow::Ok)
        }
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) && ctx.pulled(Self::OUT) {
            log!(&*ctx, Level::Debug, "flush", in_flight = self.in_flight);
            self.valid_from = self.seq;
            self.ranges.iter_mut().for_each(|r| *r = None);
            return Ok(());
        }
        if matches!(event, Event::FlushStart) {
            // Reads in flight complete at the old offset and fall below the new `seq` floor.
            let t = ctx.seek_target().unwrap_or(SeekTarget { to_byte: 0, at: Timestamp::ZERO });
            log!(&*ctx, Level::Debug, "seek", offset = t.to_byte, at = t.at);
            self.abandon_from(self.seq);
            (self.offset, self.eof, self.stamp) = (t.to_byte, false, t.at);
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.started = false; // the reactor closes the file when the group ends
    }
}
