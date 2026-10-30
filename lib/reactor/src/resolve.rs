//! Names resolved by the system resolver on a thread per lookup, so hosts, nsswitch and
//! `/etc/resolv.conf` apply. The thread posts to a mailbox, raises a flag and wakes the group.

use std::ffi::CString;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use profluens_core::id::ElementId;
use profluens_core::io::{Completion, IoError, IoResult, Op, OpId, Submission, Waker};

const MAX_RESOLVING: usize = 16;

#[derive(Default)]
struct Shared {
    ready: AtomicBool,
    resolved: Mutex<Vec<(ElementId, Completion)>>,
}

pub(crate) struct Resolver {
    shared: Arc<Shared>,
    out: Vec<(OpId, ElementId, u64)>,
}

impl Resolver {
    // Once per reactor.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new() -> Resolver {
        Resolver { shared: Arc::default(), out: Vec::with_capacity(MAX_RESOLVING) }
    }

    // One thread and one string per name, off the data path.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn start(
        &mut self,
        s: Submission,
        names: &[u8],
        waker: Option<&Waker>,
    ) -> Option<(ElementId, Completion)> {
        let fail = |s: Submission, e: IoError| {
            (s.element, Completion { op: s.op, user: s.user, result: IoResult::Err(e), buf: s.buf })
        };
        let host = match s.kind {
            Op::Resolve { host, .. } | Op::Lookup { host, .. } => host,
            _ => return Some(fail(s, IoError::Unsupported)),
        };
        let Ok(host) = std::str::from_utf8(host.of(names)).map(str::to_owned) else {
            let none = if matches!(s.kind, Op::Lookup { .. }) {
                IoError::NoRecord
            } else {
                IoError::NoAddress
            };
            return Some(fail(s, none));
        };
        if self.out.len() >= MAX_RESOLVING {
            return Some(fail(s, IoError::Full));
        }
        let (shared, waker) = (Arc::clone(&self.shared), waker.cloned());
        let (op, element, user) = (s.op, s.element, s.user);
        let Submission { kind, mut buf, .. } = s;
        let spawned = std::thread::Builder::new().name("pf-reactor-dns".into()).spawn(move || {
            let result = match kind {
                Op::Resolve { port, skip, .. } => {
                    let found = (host.as_str(), port)
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut a| a.nth(skip as usize));
                    found.map_or(IoResult::Err(IoError::NoAddress), IoResult::Addr)
                }
                Op::Lookup { kind, .. } => lookup(&host, kind, buf.as_mut()),
                _ => IoResult::Err(IoError::Unsupported),
            };
            let done = (element, Completion { op, user, result, buf });
            shared.resolved.lock().unwrap_or_else(|e| e.into_inner()).push(done);
            // A swap, as core's loom model of the pair raises it.
            shared.ready.swap(true, Ordering::AcqRel);
            if let Some(w) = waker {
                w.wake();
            }
        });
        match spawned {
            Ok(_) => {
                self.out.push((op, element, user));
                None
            }
            Err(e) => Some((
                element,
                Completion { op, user, result: IoResult::Err(crate::io_error(&e)), buf: None },
            )),
        }
    }

    pub(crate) fn take(&mut self, done: &mut Vec<(ElementId, Completion)>) {
        // Drained even with nothing out, so a cancelled lookup's answer drops its buffer now.
        if !self.shared.ready.load(Ordering::Relaxed)
            || !self.shared.ready.swap(false, Ordering::AcqRel)
        {
            return;
        }
        let mut resolved = self.shared.resolved.lock().unwrap_or_else(|e| e.into_inner());
        for (element, c) in resolved.drain(..) {
            if let Some(i) = self.out.iter().position(|r| r.0 == c.op) {
                self.out.swap_remove(i);
                done.push((element, c));
            }
        }
    }

    pub(crate) fn cancel(&mut self, target: OpId) -> Option<(ElementId, Completion)> {
        let i = self.out.iter().position(|r| r.0 == target)?;
        let (op, element, user) = self.out.swap_remove(i);
        Some((element, Completion { op, user, result: IoResult::Cancelled, buf: None }))
    }
}

fn lookup(host: &str, kind: u16, buf: Option<&mut profluens_core::buffer::Buffer>) -> IoResult {
    let (Some(buf), Ok(name)) = (buf, CString::new(host)) else {
        return IoResult::Err(IoError::NoRecord);
    };
    match crate::sys::query(&name, kind, buf.memory.as_mut_full()) {
        Some(n) => {
            buf.memory.set_len(n);
            IoResult::Record(n)
        }
        None => IoResult::Err(IoError::NoRecord),
    }
}
