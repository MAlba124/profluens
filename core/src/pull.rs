use std::collections::VecDeque;
use std::fmt;

use crate::batch::Inputs;
use crate::buffer::Buffer;
use crate::ctx::{Ctx, SeekTarget};
use crate::id::SinkPad;
use crate::time::Timestamp;

/// A range a consumer asks of the source its sink pad pulls from, `len` bytes at `offset` into
/// `buf`.
pub struct Ask {
    pub offset: u64,
    pub len: u32,
    pub tag: u64,
    pub buf: Buffer,
}

/// Why a range was refused or came back incomplete.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RangeError {
    /// The pad does not pull.
    NotPulled,
    /// The pad has its credits of ranges out.
    Full,
    /// The buffer's window is smaller than the range.
    Small { len: u32, capacity: usize },
    /// The source served fewer bytes than asked, the end of the file is inside the range.
    Short { offset: u64, want: u32, got: u32 },
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RangeError::NotPulled => f.write_str("the pad does not pull"),
            RangeError::Full => f.write_str("the pad has its credits of ranges out"),
            RangeError::Small { len, capacity } => {
                write!(f, "a range of {len} bytes into a window of {capacity}")
            }
            RangeError::Short { offset, want, got } => {
                write!(
                    f,
                    "the range of {want} bytes at {offset} ends after {got}, at the end of the file"
                )
            }
        }
    }
}

impl std::error::Error for RangeError {}

pub(crate) const START: SeekTarget = SeekTarget { to_byte: 0, at: Timestamp::ZERO };

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Mode {
    #[default]
    Off,
    Offered,
    Wanted,
    On,
    /// The source took its offer back, the consumer is told at the next sync.
    Withdrawn,
}

/// One side of a pull, its `asks` waiting and its `tags` naming the rows served or input.
#[derive(Default)]
pub(crate) struct Port {
    pub(crate) mode: Mode,
    pub(crate) asks: VecDeque<Ask>,
    // One tag per row of the link's batch while on, kept in step where rows are dropped.
    pub(crate) tags: VecDeque<u64>,
    pub(crate) out: u32,
    pub(crate) over: bool,
    pub(crate) reset: Option<SeekTarget>,
}

impl Port {
    pub(crate) fn clear(&mut self) {
        self.asks.clear();
        self.tags.clear();
        (self.out, self.over) = (0, false);
    }

    #[allow(clippy::disallowed_methods)]
    pub(crate) fn pair(&mut self, on: bool, depth: usize) {
        self.mode = if on { Mode::On } else { Mode::Off };
        if on {
            self.asks.reserve(depth + 1);
            self.tags.reserve(depth + 1);
        }
    }

    pub(crate) fn is_on(&self) -> bool {
        self.mode == Mode::On
    }
}

const ASIDE: u64 = 1 << 63;

pub enum Fed {
    /// The window at `at`, in file order, short only at the end of the file.
    Window { at: u64, buf: Buffer },
    /// A range asked with [`Feed::aside`], under its tag, as served.
    Aside { tag: u32, buf: Buffer },
}

/// The input of an element that parses a pulled byte stream, windows handed back in file order.
pub struct Feed {
    asked: u64,
    next: u64,
    window: u32,
    depth: u32,
    out: u32,
    end: Option<u64>,
    early: VecDeque<(u64, Buffer)>,
    asks: VecDeque<(u64, u32)>,
    over: bool,
}

impl Feed {
    // Once per element, with room for every window out.
    #[allow(clippy::disallowed_methods)]
    pub fn new(window: u32, depth: u32) -> Feed {
        let depth = depth.max(1);
        let (early, asks) =
            (VecDeque::with_capacity(depth as usize), VecDeque::with_capacity(depth as usize));
        Feed {
            asked: 0,
            next: 0,
            window: window.max(1),
            depth,
            out: 0,
            end: None,
            early,
            asks,
            over: false,
        }
    }

    pub fn restart(&mut self, at: u64) {
        (self.asked, self.next, self.out, self.over) = (at, at, 0, false);
        self.early.clear();
        self.asks.clear();
    }

    /// The bytes a window asked from now on takes at most, a told rate's share of time.
    pub fn set_window(&mut self, window: u32) {
        self.window = window.max(1);
    }

    /// The file ends at `len`, so nothing past it is asked.
    pub fn set_end(&mut self, len: Option<u64>) {
        self.end = len;
    }

    pub fn pos(&self) -> u64 {
        self.next
    }

    pub fn ended(&self) -> bool {
        self.end.is_some_and(|end| self.next >= end)
    }

    /// Asks windows while the pad has credits, the pool a slot and fewer than the depth are out.
    pub fn ask(&mut self, ctx: &mut Ctx, pad: SinkPad) {
        while self.out < self.depth
            && self.end.is_none_or(|end| self.asked < end)
            && ctx.asks_left(pad) > 0
        {
            let Some(buf) = ctx.try_alloc() else {
                // the pool refused a window, the first and every 64th time with its state
                static TIMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = TIMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n.is_multiple_of(64) {
                    let st = ctx.pool_stats();
                    crate::log!(
                        &*ctx,
                        crate::log::Level::Debug,
                        "window_refused",
                        out = st.outstanding,
                        max_slots = st.max_slots,
                        bytes_out = st.bytes_out,
                        budget = st.bytes_budget,
                    );
                }
                return;
            };
            let room = self.end.map_or(u64::MAX, |end| end - self.asked);
            let len = (self.window as u64).min(buf.memory.capacity() as u64).min(room) as u32;
            if ctx.ask(pad, Ask { offset: self.asked, len, tag: self.asked, buf }).is_err() {
                return;
            }
            self.asks.push_back((self.asked, len));
            (self.asked, self.out) = (self.asked + len as u64, self.out + 1);
        }
        if !self.over && self.end.is_some_and(|end| self.asked >= end) {
            self.over = true;
            ctx.end_asks(pad);
        }
    }

    /// Asks `len` bytes at `offset` beside the windows, handed back as [`Fed::Aside`]. False to
    /// retry.
    pub fn aside(&mut self, ctx: &mut Ctx, pad: SinkPad, offset: u64, len: u32, tag: u32) -> bool {
        if ctx.asks_left(pad) == 0 {
            return false;
        }
        let Ok(Some(buf)) = ctx.try_alloc_sized(len as usize) else {
            return false;
        };
        ctx.ask(pad, Ask { offset, len, tag: ASIDE | tag as u64, buf }).is_ok()
    }

    /// The next window in file order or a range asked aside, as it comes.
    pub fn take(&mut self, ctx: &mut Ctx, pad: SinkPad, inputs: &mut Inputs<'_>) -> Option<Fed> {
        loop {
            if let Some(i) = self.early.iter().position(|(at, _)| *at == self.next) {
                let (at, buf) = self.early.remove(i)?;
                return Some(self.hand(at, buf));
            }
            let (tag, buf) = ctx.next_served(pad, inputs)?;
            if tag & ASIDE != 0 {
                return Some(Fed::Aside { tag: tag as u32, buf });
            }
            self.out = self.out.saturating_sub(1);
            if tag == self.next {
                return Some(self.hand(tag, buf));
            }
            self.early.push_back((tag, buf));
        }
    }

    fn hand(&mut self, at: u64, buf: Buffer) -> Fed {
        let len = buf.memory.len() as u64;
        let want = match self.asks.iter().position(|(o, _)| *o == at) {
            Some(i) => self.asks.remove(i).map_or(0, |(_, want)| want as u64),
            None => len,
        };
        if len < want && self.end.is_none_or(|end| at + len < end) {
            self.end = Some(at + len);
        }
        self.next = at + len;
        Fed::Window { at, buf }
    }
}
