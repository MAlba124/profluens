//! `growingfilesrc` reads a file another writer is still appending to.
//! A [`FrontierHandle`] publishes how many leading bytes are safely readable.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use profluens_core::batch::Inputs;
use profluens_core::bus::BusMessage;
use profluens_core::ctx::{Ctx, SeekTarget};
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, PropDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{Constraint, OfferDesc};
use profluens_core::id::SrcPad;
use profluens_core::io::{Handle, IoResult, OpenMode};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::pipeline::Serves;
use profluens_core::time::Timestamp;

use super::error::FileError;

/// A file is an untyped byte stream.
static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

/// The file to read. A parsed instance gets no reachable [`FrontierHandle`].
static PROPS: [PropDesc; 1] = [PropDesc { name: "path", allowed: Constraint::Any, live: false }];

// Cold, one box per registry-created element. Its handle is dropped, so it ends as Abandoned.
#[allow(clippy::disallowed_methods)]
static DESC: ElementDesc = ElementDesc {
    name: "growingfilesrc",
    sinks: &[],
    srcs: &PADS,
    props: &PROPS,
    sched: SchedHint::Active,
    inputs: InputPolicy::None,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(GrowingFileSrc::new("").0)),
    io: true,
    couplings: &[],
};

struct FrontierShared {
    /// Safely readable bytes from 0. Monotonic.
    downloaded: AtomicU64,
    total: AtomicU64,
    finished: AtomicBool,
    aborted: AtomicBool,
}

/// The writer's end of the frontier. Call [`advance`](Self::advance) only after the
/// bytes are visible to other threads. An over-promise degrades to a short read.
#[derive(Clone)]
pub struct FrontierHandle(Arc<FrontierShared>);

impl FrontierHandle {
    /// Publish that the first `downloaded` bytes are readable. Lower values are ignored.
    pub fn advance(&self, downloaded: u64) {
        self.0.downloaded.fetch_max(downloaded, Ordering::Release);
    }

    /// The file is complete at `total` bytes. Idempotent.
    pub fn finish(&self, total: u64) {
        self.0.total.store(total, Ordering::Relaxed);
        self.0.downloaded.fetch_max(total, Ordering::Relaxed);
        // Release last, so a reader that sees `finished` sees `total` and the watermark.
        self.0.finished.store(true, Ordering::Release);
    }

    pub fn abort(&self) {
        self.0.aborted.store(true, Ordering::Release);
    }

    pub fn downloaded(&self) -> u64 {
        self.0.downloaded.load(Ordering::Acquire)
    }

    pub fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }
}

impl FrontierShared {
    fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Acquire)
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Complete,
    Aborted,
    /// Every [`FrontierHandle`] dropped without `finish()` or `abort()`.
    Abandoned,
    Truncated,
}

#[derive(Default)]
struct Slot {
    offset: u64,
    /// The watermark when this read was submitted. Emission is clamped to it, not to the
    /// watermark at completion.
    limit: u64,
    buf: Option<profluens_core::buffer::Buffer>,
}

pub struct GrowingFileSrc {
    path: PathBuf,
    shared: Arc<FrontierShared>,
    file: Handle,
    started: bool,
    offset: u64,
    /// One past the last byte pushed downstream.
    done_upto: u64,
    end: Option<End>,
    end_reported: bool,
    in_flight: u32,
    seq: u64,
    /// Reads with a `seq` below this were submitted before a seek and are discarded.
    valid_from: u64,
    /// Re-sequencing window for out-of-order completions, power-of-two sized.
    window: Vec<Slot>,
    wmask: u64,
    parked: u32,
    next_emit: u64,
    /// Set when a read returns nothing below the watermark (writer over-promised). Blocks
    /// submission so the group parks, and clears on that parking pass, one tick later.
    stalled: bool,
    stamp: Timestamp,
}

const OPEN: u64 = u64::MAX;

impl GrowingFileSrc {
    pub const OUT: SrcPad = SrcPad(0);

    // Cold, one-time construction.
    #[allow(clippy::disallowed_methods)]
    pub fn new(path: impl AsRef<Path>) -> (Self, FrontierHandle) {
        let shared = Arc::new(FrontierShared {
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            finished: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
        });
        let handle = FrontierHandle(Arc::clone(&shared));
        let me = Self {
            path: path.as_ref().to_path_buf(),
            shared,
            file: Handle::NONE,
            started: false,
            offset: 0,
            done_upto: 0,
            end: None,
            end_reported: false,
            in_flight: 0,
            seq: 0,
            valid_from: 0,
            window: Vec::new(),
            wmask: 0,
            parked: 0,
            next_emit: 0,
            stalled: false,
            stamp: Timestamp::NONE,
        };
        (me, handle)
    }

    #[inline]
    fn slot_of(&self, seq: u64) -> Option<usize> {
        let len = self.window.len() as u64;
        (len > 0 && seq >= self.next_emit && seq < self.next_emit + len)
            .then_some((seq & self.wmask) as usize)
    }

    /// Abandon every read from `seq` on and rewind submission to the first unemitted byte.
    /// `in_flight` stays untouched until those completions arrive.
    fn abandon_from(&mut self, seq: u64) {
        for slot in &mut self.window {
            slot.buf = None; // buffers recycle on drop
        }
        self.parked = 0;
        self.valid_from = seq;
        self.next_emit = seq;
        self.offset = self.done_upto;
    }

    fn emit(
        &mut self,
        ctx: &mut Ctx,
        mut buf: profluens_core::buffer::Buffer,
        at: u64,
        limit: u64,
    ) -> bool {
        let cap = buf.memory.capacity();
        // The reactor reads the whole capacity, so clamp to the frontier and to what the
        // kernel transferred.
        let allowed = limit.saturating_sub(at).min(cap as u64) as usize;
        let n = buf.memory.len().min(allowed);

        if n == 0 {
            // An empty read is not EOS for a growing file.
            drop(buf);
            self.abandon_from(self.seq); // rewinds `offset` to `done_upto` == `at`
            if self.shared.is_finished() && at < self.shared.total() {
                self.end = Some(End::Truncated);
            } else {
                log!(&*ctx, Level::Debug, "stall", offset = at, limit = limit);
                self.stalled = true;
            }
            return true;
        }

        self.next_emit += 1;
        self.done_upto = at + n as u64;
        buf.memory.set_len(n);
        log!(&*ctx, Level::Trace, "read", bytes = n);
        buf.pts = std::mem::replace(&mut self.stamp, Timestamp::NONE);
        ctx.out(Self::OUT).push(buf);
        if n < cap {
            self.abandon_from(self.seq);
            return true;
        }
        false
    }

    fn report_end(&mut self, ctx: &mut Ctx, end: End) {
        let element = ctx.element();
        match end {
            End::Complete => {
                log!(&*ctx, Level::Info, "eos", offset = self.done_upto);
            }
            End::Aborted => {
                let error = Error::element_failed(
                    element,
                    FileError::WriterAborted {
                        path: self.path.as_path().into(),
                        emitted: self.done_upto,
                        downloaded: self.shared.downloaded(),
                    },
                );
                ctx.post(BusMessage::Error { element, error });
            }
            End::Abandoned => {
                let error = Error::element_failed(
                    element,
                    FileError::WriterAbandoned {
                        path: self.path.as_path().into(),
                        emitted: self.done_upto,
                    },
                );
                ctx.post(BusMessage::Warning { element, error });
            }
            End::Truncated => {
                let error = Error::element_failed(
                    element,
                    FileError::Truncated {
                        path: self.path.as_path().into(),
                        declared: self.shared.total(),
                        readable: self.done_upto,
                    },
                );
                ctx.post(BusMessage::Warning { element, error });
            }
        }
    }
}

impl Element for GrowingFileSrc {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        Ok(())
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        if let Some(profluens_core::format::Value::Id(id)) = ctx.prop("path")
            && let Some(s) = ctx.value_name(id)
        {
            self.path = PathBuf::from(s);
        }
        // A restart after stop() reads from byte 0. Reads left by the old run fall below
        // the `seq` floor, and their completions no longer count against `in_flight`.
        (self.file, self.done_upto, self.end, self.end_reported, self.in_flight) =
            (Handle::NONE, 0, None, false, 0);
        (self.stalled, self.stamp) = (false, Timestamp::NONE);
        self.abandon_from(self.seq);
        // The file must exist but may be empty. The frontier, not its size, bounds reads.
        ctx.io().open(&self.path, OpenMode::Read, OPEN);
        // Cold, sized once per run to the in-flight read budget.
        #[allow(clippy::disallowed_methods)]
        {
            let depth = (ctx.io().credits() as usize).max(1).next_power_of_two();
            self.window.resize_with(depth, Slot::default);
            self.wmask = depth as u64 - 1;
        }
        self.started = true;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        if !self.started {
            return Err(Error::element_failed(
                ctx.element(),
                FileError::NotStarted { element: "growingfilesrc" },
            ));
        }
        let (done_before, seq_before) = (self.done_upto, self.seq);

        if self.end.is_none() {
            if self.shared.is_aborted() {
                self.end = Some(End::Aborted);
            } else if !self.shared.is_finished() && Arc::strong_count(&self.shared) == 1 {
                // Only this element holds the cell, so no handle can ever appear again.
                self.end = Some(End::Abandoned);
            }
        }

        loop {
            let Some(c) = ctx.io().next_completion() else {
                break;
            };
            if c.user == OPEN {
                match c.result {
                    IoResult::File { handle, .. } => {
                        self.file = handle;
                        log!(&*ctx, Level::Info, "open", frontier = self.shared.downloaded());
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
                continue; // read submitted before a seek: wrong offset, discard
            }
            if self.end.is_some() {
                continue; // winding down: drain the mailbox, emit nothing (buf recycles)
            }
            match (c.result, c.buf) {
                (IoResult::Ok(_), Some(buf)) => {
                    let Some(slot) = self.slot_of(c.user) else {
                        return Err(Error::element_failed(
                            ctx.element(),
                            FileError::CompletionOutsideWindow {
                                element: "growingfilesrc",
                                seq: c.user,
                            },
                        ));
                    };
                    if self.parked == 0 && c.user == self.next_emit {
                        let (at, limit) = (self.window[slot].offset, self.window[slot].limit);
                        if self.emit(ctx, buf, at, limit) {
                            break; // clamped, short or empty: the rest of the mailbox is stale
                        }
                    } else {
                        self.window[slot].buf = Some(buf);
                        self.parked += 1;
                    }
                }
                (IoResult::Err(k), _) => {
                    let element = ctx.element();
                    let err = Error::element_failed(
                        element,
                        FileError::Read {
                            path: self.path.as_path().into(),
                            offset: Some(self.done_upto),
                            kind: k,
                        },
                    );
                    ctx.post(BusMessage::Error { element, error: err.clone() });
                    return Err(err);
                }
                _ => {
                    // A cancelled read would block its window slot forever. The scheduler does not
                    // cancel yet.
                    if self.slot_of(c.user).is_some() {
                        log!(&*ctx, Level::Warn, "read_cancelled", offset = self.done_upto);
                        self.abandon_from(self.seq);
                        break;
                    }
                }
            }
        }

        while self.parked > 0 && self.end.is_none() {
            let Some(slot) = self.slot_of(self.next_emit) else {
                break;
            };
            let Some(buf) = self.window[slot].buf.take() else {
                break;
            };
            self.parked -= 1;
            let (at, limit) = (self.window[slot].offset, self.window[slot].limit);
            if self.emit(ctx, buf, at, limit) {
                break;
            }
        }

        let frontier = self.shared.downloaded();
        let finished = self.shared.is_finished();

        if self.end.is_none() && !self.stalled {
            let credits = ctx.io().credits();
            let horizon = self.next_emit + self.window.len() as u64;
            let open = !self.file.is_none();
            while open && self.in_flight < credits && self.seq < horizon && self.offset < frontier {
                let Some(buf) = ctx.try_alloc() else {
                    break; // pool full, backpressure
                };
                let cap = buf.memory.capacity() as u64;
                let user = self.seq;
                let Some(slot) = self.slot_of(user) else {
                    break;
                };
                self.window[slot].offset = self.offset;
                self.window[slot].limit = frontier;
                self.seq += 1;
                ctx.io().read(self.file, self.offset, buf, user);
                // A straddling read gets clamped and invalidates everything behind it, so stop
                // here.
                let straddles = self.offset + cap > frontier;
                self.offset += cap;
                self.in_flight += 1;
                if straddles {
                    break;
                }
            }
        }

        // Matches the scheduler's idle-pass test, so the retry lands one backstop tick later.
        let idle_pass =
            self.done_upto == done_before && self.seq == seq_before && self.in_flight == 0;
        if self.stalled && idle_pass {
            self.stalled = false;
        }

        if self.end.is_none() && finished && self.done_upto >= self.shared.total() {
            self.end = Some(End::Complete);
        }
        let Some(end) = self.end else {
            return Ok(Flow::Ok);
        };
        if !self.end_reported {
            self.end_reported = true;
            self.report_end(ctx, end);
        }
        // EOS only once every completion is reaped.
        if self.in_flight == 0 { Ok(Flow::Eos) } else { Ok(Flow::Ok) }
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if matches!(event, Event::FlushStart) {
            // A target past the frontier is not an error. The element waits there until the
            // downloader writes those bytes.
            let t = ctx.seek_target().unwrap_or(SeekTarget { to_byte: 0, at: Timestamp::ZERO });
            log!(&*ctx, Level::Debug, "seek", offset = t.to_byte, at = t.at);
            (self.done_upto, self.stamp) = (t.to_byte, t.at);
            self.abandon_from(self.seq); // rewinds `offset` to the target
            self.stalled = false;
            self.end = None;
            self.end_reported = false;
            if self.shared.is_finished() && t.to_byte >= self.shared.total() {
                self.end = Some(End::Complete);
            }
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.started = false; // the reactor closes the file when the group ends
    }
}
