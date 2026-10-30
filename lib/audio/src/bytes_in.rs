//! Input of an element that reads an elementary stream straight off a byte source.

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::ctx::Ctx;
use profluens_core::id::SinkPad;
use profluens_core::pull::{Fed, Feed};
use profluens_core::time::Timestamp;

pub const WINDOW: u32 = 64 * 1024;
/// Windows out at once at most.
pub const DEPTH: u32 = 2;
/// The fewest slots a pool has for its element to pull, one window and room beside it.
pub const MIN_SLOTS: u64 = 4;

/// Pulled windows or pushed buffers on one sink pad.
pub struct ByteIn {
    pad: SinkPad,
    feed: Feed,
    paired: bool,
    stamp: Timestamp,
}

impl Default for ByteIn {
    fn default() -> Self {
        Self::new(SinkPad(0))
    }
}

impl ByteIn {
    // Once per element, the feed's queues sized for its windows.
    #[allow(clippy::disallowed_methods)]
    pub fn new(pad: SinkPad) -> Self {
        Self { pad, feed: Feed::new(WINDOW, DEPTH), paired: false, stamp: Timestamp::NONE }
    }

    // Once per run, the feed sized for the pool.
    #[allow(clippy::disallowed_methods)]
    pub fn start(&mut self, ctx: &mut Ctx, len: Option<u64>) {
        let slots = ctx.pool_stats().max_slots;
        if slots < MIN_SLOTS {
            return;
        }
        ctx.want_ranges(self.pad);
        self.feed = Feed::new(WINDOW, (slots / 4).min(u64::from(DEPTH)) as u32);
        self.feed.set_end(len.filter(|len| *len > 0));
        (self.paired, self.stamp) = (false, Timestamp::NONE);
    }

    pub fn next(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Option<Buffer> {
        if let Some(from) = ctx.pull_reset(self.pad) {
            self.feed.restart(from.to_byte);
            if std::mem::replace(&mut self.paired, true) {
                self.stamp = from.at;
            }
        }
        if !ctx.pulls(self.pad) {
            return inputs.pop();
        }
        loop {
            match self.feed.take(ctx, self.pad, inputs)? {
                Fed::Window { mut buf, .. } => {
                    buf.pts = std::mem::replace(&mut self.stamp, Timestamp::NONE);
                    return Some(buf);
                }
                Fed::Aside { .. } => {}
            }
        }
    }

    pub fn ask(&mut self, ctx: &mut Ctx) {
        if ctx.pulls(self.pad) {
            self.feed.ask(ctx, self.pad);
        }
    }
}
