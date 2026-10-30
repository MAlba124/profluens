//! The group's bell, an eventfd whose read stays in flight on the queue. Its count latches a ring
//! that lands before the wait begins.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use rustix::event::{EventfdFlags, eventfd};
use rustix::io::Errno;

pub(crate) struct EventBell {
    fd: OwnedFd,
}

impl EventBell {
    pub(crate) fn new() -> Result<EventBell, Errno> {
        let fd = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        Ok(EventBell { fd })
    }

    pub(crate) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub(crate) fn ring(&self) {
        // A full counter is rung already, and a failed write leaves the next
        // backstop to find the work.
        let _ = rustix::io::write(&self.fd, &1u64.to_ne_bytes());
    }
}
