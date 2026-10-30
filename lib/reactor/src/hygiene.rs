//! Page cache hygiene for files streamed once (`posix_fadvise`, `sync_file_range`). Without it a
//! 1.6 GB remux under a 3 GiB cgroup froze about 5 s while reclaim drained dirty pages.

pub const POSIX_FADV_SEQUENTIAL: i32 = libc::POSIX_FADV_SEQUENTIAL;
/// Free the cached pages of a range. Dirty pages stay, so writes kick writeback a window ahead.
pub const POSIX_FADV_DONTNEED: i32 = libc::POSIX_FADV_DONTNEED;
pub const SYNC_FILE_RANGE_WRITE: u32 = libc::SYNC_FILE_RANGE_WRITE;

/// Forward progress between hints, far above any readahead window so dropping never races it.
pub const HYGIENE_WINDOW: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    Sequential,
    Drop { at: u64, len: u64 },
    Sync { at: u64, len: u64 },
}

/// Hygiene of one file, fed completed op ranges in file order. An op below a side's high-water
/// mark turns that side off for good, so random access never thrashes.
pub struct StreamHygiene {
    window: u64,
    read: Side,
    write: Side,
}

#[derive(Default)]
struct Side {
    off: bool,
    started: bool,
    end: u64,
    synced: u64,
    dropped: u64,
}

impl Side {
    fn advance(&mut self, offset: u64, len: usize) -> bool {
        if self.off || len == 0 {
            return false;
        }
        if !self.started {
            (self.started, self.dropped, self.synced, self.end) = (true, offset, offset, offset);
        }
        if offset < self.end {
            self.off = true;
            return false;
        }
        self.end = offset + len as u64;
        true
    }
}

impl StreamHygiene {
    pub fn new() -> Self {
        Self::with_window(HYGIENE_WINDOW)
    }

    pub fn with_window(window: u64) -> Self {
        Self { window: window.max(1), read: Side::default(), write: Side::default() }
    }

    pub fn set_window(&mut self, window: u64) {
        self.window = window.max(1);
    }

    pub fn disable_read(&mut self) {
        self.read.off = true;
    }

    pub fn disable_write(&mut self) {
        self.write.off = true;
    }

    pub fn on_read(&mut self, offset: u64, len: usize) -> Option<Hint> {
        let r = &mut self.read;
        if !r.advance(offset, len) || r.end - r.dropped < self.window {
            return None;
        }
        let hint = Hint::Drop { at: r.dropped, len: r.end - r.dropped };
        r.dropped = r.end;
        Some(hint)
    }

    pub fn on_write(&mut self, offset: u64, len: usize) -> [Option<Hint>; 2] {
        let w = &mut self.write;
        if !w.advance(offset, len) || w.end - w.synced < self.window {
            return [None, None];
        }
        let sync = Hint::Sync { at: w.synced, len: w.end - w.synced };
        // Never drop pages that were just dirtied, the kernel leaves them.
        let mut drop = None;
        if w.synced > w.dropped {
            drop = Some(Hint::Drop { at: w.dropped, len: w.synced - w.dropped });
            w.dropped = w.synced;
        }
        w.synced = w.end;
        [Some(sync), drop]
    }
}

impl Default for StreamHygiene {
    fn default() -> Self {
        Self::new()
    }
}
