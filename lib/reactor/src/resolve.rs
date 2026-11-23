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

/// A worker's lookup, `Live` until cancelled. Either way it holds its worker's place until the
/// worker answers, since a cancel cannot stop the blocking resolver.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ask {
    Live,
    Cancelled,
}

struct Lookup {
    op: OpId,
    element: ElementId,
    user: u64,
    ask: Ask,
}

pub(crate) struct Resolver {
    shared: Arc<Shared>,
    /// One per running worker, so its length is the admission count.
    out: Vec<Lookup>,
    /// Holds every worker before it resolves, for tests of a slow resolver.
    #[cfg(test)]
    stall: Option<Arc<tests::Stall>>,
}

impl Resolver {
    // Once per reactor.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new() -> Resolver {
        Resolver {
            shared: Arc::default(),
            out: Vec::with_capacity(MAX_RESOLVING),
            #[cfg(test)]
            stall: None,
        }
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
        #[cfg(test)]
        let stall = self.stall.clone();
        let spawned = std::thread::Builder::new().name("pf-reactor-dns".into()).spawn(move || {
            #[cfg(test)]
            if let Some(stall) = stall {
                stall.wait();
            }
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
                self.out.push(Lookup { op, element, user, ask: Ask::Live });
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
            let Some(i) = self.out.iter().position(|r| r.op == c.op) else {
                continue;
            };
            if self.out.swap_remove(i).ask == Ask::Live {
                done.push((element, c));
            }
        }
    }

    /// Completes a live lookup as cancelled now. Its worker keeps its place until it answers.
    pub(crate) fn cancel(&mut self, target: OpId) -> Option<(ElementId, Completion)> {
        let r = self.out.iter_mut().find(|r| r.op == target && r.ask == Ask::Live)?;
        r.ask = Ask::Cancelled;
        let c = Completion { op: r.op, user: r.user, result: IoResult::Cancelled, buf: None };
        Some((r.element, c))
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

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::disallowed_methods)]

    use std::sync::Condvar;
    use std::time::{Duration, Instant};

    use profluens_core::io::Name;

    use super::*;

    /// A gate the workers wait at until a test opens it.
    #[derive(Default)]
    pub(crate) struct Stall {
        open: Mutex<bool>,
        moved: Condvar,
    }

    impl Stall {
        pub(crate) fn wait(&self) {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            while !*open {
                open = self.moved.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.moved.notify_all();
        }
    }

    const HOST: &[u8] = b"127.0.0.1";

    fn resolve(op: u64) -> Submission {
        let host = Name { off: 0, len: HOST.len() as u32 };
        Submission {
            op: OpId(op),
            element: ElementId(1),
            kind: Op::Resolve { host, port: 80, skip: 0 },
            buf: None,
            user: op,
        }
    }

    #[test]
    fn a_cancelled_lookup_holds_its_worker_until_the_worker_answers() {
        let mut r = Resolver::new();
        let stall = Arc::new(Stall::default());
        r.stall = Some(Arc::clone(&stall));
        for op in 0..MAX_RESOLVING as u64 {
            assert!(r.start(resolve(op), HOST, None).is_none(), "lookup {op} admitted");
        }
        for op in 0..MAX_RESOLVING as u64 {
            let c = r.cancel(OpId(op)).expect("a cancel completes at once").1;
            assert_eq!(c.result, IoResult::Cancelled);
        }
        assert!(r.cancel(OpId(0)).is_none(), "a second cancel is nothing");
        let over = r.start(resolve(100), HOST, None).expect("refused");
        assert_eq!(over.1.result, IoResult::Err(IoError::Full), "every worker still runs");

        stall.release();
        let (mut done, start) = (Vec::new(), Instant::now());
        while r.start(resolve(101), HOST, None).is_some() {
            assert!(start.elapsed() < Duration::from_secs(10), "no worker ever answered");
            std::thread::sleep(Duration::from_millis(1));
            r.take(&mut done);
        }
        while !done.iter().any(|(_, c)| c.op == OpId(101)) {
            assert!(start.elapsed() < Duration::from_secs(10), "the new lookup never answered");
            std::thread::sleep(Duration::from_millis(1));
            r.take(&mut done);
        }
        assert_eq!(done.len(), 1, "a cancelled lookup's answer was delivered");
        assert_eq!(done[0].1.result, IoResult::Addr(([127, 0, 0, 1], 80).into()));
    }
}
