// One allocation per change, at the commit. No pass allocates here.
#![allow(clippy::disallowed_methods)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::bus::{BusMessage, BusSender};
use crate::command::{Command, CommandError, Control, Cursor};
use crate::ctx::{PadOffers, PeerMenu};
use crate::error::{Error, TopologyError};
use crate::feedback::Upstream;
use crate::format::FixedFormat;
use crate::id::{ElementId, LinkId, SinkPad, SrcPad};
use crate::pipeline::Pipeline;
use crate::sched::{DownLink, GroupSetup, UpLink};
use crate::station::Station;
use crate::time::{Rate, Timestamp};

/// How a removed branch goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Leave {
    /// Process what the branch holds, end every element's stream in order, then stop.
    Drain,
    /// Stop at the next safe point and present nothing more.
    Cut,
}

pub(crate) struct Part {
    pub(crate) head: ElementId,
    pub(crate) down_add: Vec<DownLink>,
    pub(crate) up_add: Vec<UpLink>,
    pub(crate) down_cut: Vec<LinkId>,
    pub(crate) up_cut: Vec<SinkPad>,
    pub(crate) handover: Vec<SinkPad>,
    pub(crate) edits: Vec<(ElementId, PadEdit)>,
    pub(crate) leave: Option<Leave>,
    pub(crate) undo: bool,
    pub(crate) seek_gen: u64,
}

impl Part {
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new(head: ElementId, seek_gen: u64) -> Part {
        let (down_add, up_add, down_cut, up_cut) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (handover, edits) = (Vec::new(), Vec::new());
        Part {
            head,
            down_add,
            up_add,
            down_cut,
            up_cut,
            handover,
            edits,
            leave: None,
            undo: false,
            seek_gen,
        }
    }
}

pub(crate) enum PadEdit {
    Pads(usize, usize),
    StreamPad(u32, SrcPad),
    SrcFormat(SrcPad, FixedFormat),
    /// The menus of the pads a src pad feeds once the change is made.
    SrcPeers(SrcPad, Vec<PeerMenu>),
    SinkFormat(SinkPad, FixedFormat),
    SinkOffers(PadOffers),
    Latency(Timestamp),
    Restart,
}

pub(crate) struct Join {
    pub(crate) to_time: Timestamp,
    pub(crate) paused: bool,
    pub(crate) rate: Rate,
    pub(crate) joining: Arc<Joining>,
}

pub(crate) struct Joining {
    started: AtomicU32,
    /// The splice is on its way into the log, so a failing group fails the run.
    spliced: AtomicBool,
    failed: Mutex<Option<Error>>,
}

impl Joining {
    pub(crate) fn new() -> Arc<Joining> {
        let (started, spliced) = (AtomicU32::new(0), AtomicBool::new(false));
        Arc::new(Joining { started, spliced, failed: Mutex::new(None) })
    }

    pub(crate) fn started(&self) {
        self.started.fetch_add(1, Ordering::AcqRel);
    }

    /// A group of the change failed. True when the change rolls back instead of the run failing.
    pub(crate) fn fail(&self, error: &Error) -> bool {
        let mut failed = relock(&self.failed);
        let takes = !self.spliced.load(Ordering::Acquire);
        if takes && failed.is_none() {
            *failed = Some(error.clone());
        }
        takes
    }
}

/// An edit for the thread that is in `run()`, given the refusal where it was not queued.
pub(crate) type Edit = Box<dyn FnOnce(Result<&mut Pipeline, CommandError>) + Send>;

pub(crate) struct Flight {
    pub(crate) change: u32,
    pub(crate) attach: Option<u32>,
    pub(crate) splice: u32,
    pub(crate) groups: u32,
    pub(crate) cursors: Vec<Arc<Cursor>>,
    pub(crate) joining: Arc<Joining>,
    pub(crate) wait: Vec<(ElementId, SinkPad)>,
    pub(crate) rollback: Vec<Part>,
}

#[derive(Default)]
struct Staging {
    next: u32,
    steps: Vec<(u32, Vec<Part>)>,
    outbox: VecDeque<u32>,
    setups: Vec<GroupSetup>,
    flight: Option<Flight>,
    ended: Option<(u32, Result<(), Error>)>,
    edits: VecDeque<Edit>,
    serving: bool,
    runner: Option<std::thread::ThreadId>,
    exited: Vec<std::thread::ThreadId>,
    gone: Vec<ElementId>,
}

pub(crate) struct Topology {
    // Down means nothing staged or in flight, so a driver pass takes no lock.
    busy: AtomicBool,
    inner: Mutex<Staging>,
    pub(crate) driver: Station,
}

fn relock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn cancelled(change: u32) -> Error {
    Error::Topology(TopologyError::Cancelled { change })
}

/// Drop the parts whose group is gone, sealing each link such a part would have added.
fn drop_gone(parts: &mut Vec<Part>, gone: impl Fn(ElementId) -> bool) {
    let mut k = 0;
    while k < parts.len() {
        if !gone(parts[k].head) {
            k += 1;
            continue;
        }
        let part = parts.swap_remove(k);
        let ends = part
            .down_add
            .iter()
            .map(|d| (&d.slot, &d.peer))
            .chain(part.up_add.iter().map(|u| (&u.slot, &u.peer)));
        let peers: Vec<Arc<Station>> = ends
            .map(|(slot, peer)| {
                slot.seal();
                slot.write(&Upstream::default(), part.seek_gen);
                Arc::clone(peer)
            })
            .collect();
        // The ends close first, so a peer that wakes finds its ring closed.
        drop(part);
        peers.iter().for_each(|peer| peer.signal());
    }
}

impl Topology {
    pub(crate) fn new() -> Topology {
        let busy = AtomicBool::new(false);
        Topology { busy, inner: Mutex::new(Staging::default()), driver: Station::new() }
    }

    pub(crate) fn number(&self) -> u32 {
        let mut st = relock(&self.inner);
        st.next += 1;
        st.next
    }

    /// Stage a change and its new groups, posting `flight.attach` when the log has room.
    pub(crate) fn stage(
        &self,
        flight: Flight,
        mut steps: Vec<(u32, Vec<Part>)>,
        setups: Vec<GroupSetup>,
        heads: &[u32],
    ) {
        let mut st = relock(&self.inner);
        st.outbox.extend(flight.attach);
        let gone = &st.gone;
        steps.iter_mut().for_each(|(_, parts)| drop_gone(parts, |head| gone.contains(&head)));
        st.gone.retain(|head| heads.binary_search(&head.0).is_ok());
        st.steps.extend(steps.into_iter().filter(|(_, parts)| !parts.is_empty()));
        st.setups.extend(setups);
        st.flight = Some(flight);
        self.busy.store(true, Ordering::Release);
        drop(st);
        self.driver.signal();
    }

    pub(crate) fn take_part(&self, step: u32, head: ElementId) -> Option<Part> {
        let mut st = relock(&self.inner);
        let at = st.steps.iter().position(|(id, _)| *id == step)?;
        let parts = &mut st.steps[at].1;
        let part = parts.iter().position(|p| p.head == head).map(|i| parts.swap_remove(i));
        if parts.is_empty() {
            st.steps.swap_remove(at);
            self.driver.signal();
        }
        part
    }

    pub(crate) fn changing(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    pub(crate) fn forget(&self, head: ElementId) {
        let mut st = relock(&self.inner);
        for (_, parts) in st.steps.iter_mut() {
            drop_gone(parts, |h| h == head);
        }
        st.steps.retain(|(_, parts)| !parts.is_empty());
        st.gone.push(head);
        self.driver.signal();
    }

    // An empty Vec, which allocates nothing.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn take_setups(&self) -> Vec<GroupSetup> {
        if !self.busy.load(Ordering::Acquire) {
            return Vec::new();
        }
        std::mem::take(&mut relock(&self.inner).setups)
    }

    pub(crate) fn gone_with<R>(&self, read: impl FnOnce(&[ElementId]) -> R) -> R {
        read(&relock(&self.inner).gone)
    }

    pub(crate) fn in_flight(&self) -> bool {
        relock(&self.inner).flight.is_some()
    }

    pub(crate) fn take_ended(&self, change: u32) -> Option<Result<(), Error>> {
        relock(&self.inner).ended.take_if(|(ended, _)| *ended == change).map(|(_, how)| how)
    }

    pub(crate) fn cancel(&self) -> Option<u32> {
        let st = relock(&self.inner);
        let flight = st.flight.as_ref()?;
        flight.joining.fail(&cancelled(flight.change)).then_some(flight.change)
    }

    pub(crate) fn over(&self, bus: &BusSender) {
        let mut st = relock(&self.inner);
        if let Some(flight) = st.flight.take() {
            let error = cancelled(flight.change);
            bus.send(BusMessage::ChangeFailed { change: flight.change, error: error.clone() });
            st.ended = Some((flight.change, Err(error)));
        }
    }

    /// Move the change in flight forward without waiting. Both drivers call it between passes.
    pub(crate) fn advance(&self, control: &Control, bus: &BusSender) {
        if !self.busy.load(Ordering::Acquire) {
            return;
        }
        let mut st = relock(&self.inner);
        Self::post(&mut st, control);
        let idle = st.flight.is_none()
            && st.outbox.is_empty()
            && st.setups.is_empty()
            && st.steps.is_empty();
        self.busy.store(!idle, Ordering::Release);
        if idle {
            drop(st);
            return control.signal_all();
        }
        let Some(mut flight) = st.flight.take() else {
            return;
        };
        if control.over_but(&flight.cursors) {
            flight.joining.fail(&cancelled(flight.change));
        }
        let failed = relock(&flight.joining.failed).take();
        if let Some(error) = failed {
            let ours = |id: &u32| *id == flight.splice || Some(*id) == flight.attach;
            st.outbox.retain(|id| !ours(id));
            st.steps.retain(|(id, _)| *id != flight.splice);
            st.next += 1;
            let step = st.next;
            let mut rollback = std::mem::take(&mut flight.rollback);
            rollback.retain(|p| !st.gone.contains(&p.head));
            if !rollback.is_empty() {
                st.steps.push((step, rollback));
                st.outbox.push_back(step);
            }
            bus.send(BusMessage::ChangeFailed { change: flight.change, error: error.clone() });
            st.ended = Some((flight.change, Err(error)));
            return Self::post(&mut st, control);
        }
        let attached = flight.attach.is_none_or(|a| !st.outbox.contains(&a));
        let started = flight.joining.started.load(Ordering::Acquire) >= flight.groups;
        let ready = |(head, pad): &(ElementId, SinkPad)| {
            let parts = st.steps.iter().find(|(id, _)| *id == flight.splice).map(|(_, p)| p);
            let part = parts.and_then(|p| p.iter().find(|p| p.head == *head));
            part.is_none_or(|p| p.up_add.iter().all(|u| u.pad != *pad || !u.data.is_empty()))
        };
        if !(attached && started && flight.wait.iter().all(ready)) || control.is_stopped() {
            st.flight = Some(flight);
            return;
        }
        flight.joining.spliced.store(true, Ordering::Release);
        // Pairs with `fail` reading `spliced`, so a failure in between rolls back next call.
        if relock(&flight.joining.failed).is_some() {
            flight.joining.spliced.store(false, Ordering::Release);
            st.flight = Some(flight);
            return;
        }
        st.outbox.push_back(flight.splice);
        st.ended = Some((flight.change, Ok(())));
        bus.send(BusMessage::Changed { change: flight.change });
        Self::post(&mut st, control);
    }

    fn post(st: &mut Staging, control: &Control) {
        while let Some(step) = st.outbox.front().copied() {
            match control.append(Command::Splice { step }) {
                Err(CommandError::Full) => return,
                Ok(_) | Err(_) => st.outbox.pop_front(),
            };
        }
    }

    pub(crate) fn serve(&self, on: bool) {
        let mut st = relock(&self.inner);
        st.serving = on;
        st.runner = on.then(|| std::thread::current().id());
        if !on {
            *st = Staging { next: st.next, ended: st.ended.take(), ..Staging::default() };
            self.busy.store(false, Ordering::Release);
        }
    }

    pub(crate) fn push_edit(&self, edit: Edit) -> Result<(), (CommandError, Edit)> {
        let mut st = relock(&self.inner);
        if !st.serving {
            return Err((CommandError::NotRunning, edit));
        }
        // The run thread executes the edits, so one made from it would wait on itself.
        if st.runner == Some(std::thread::current().id()) {
            return Err((CommandError::OnRunThread, edit));
        }
        st.edits.push_back(edit);
        drop(st);
        self.driver.signal();
        Ok(())
    }

    pub(crate) fn take_edit(&self) -> Option<Edit> {
        relock(&self.inner).edits.pop_front()
    }

    pub(crate) fn exited(&self) {
        relock(&self.inner).exited.push(std::thread::current().id());
        self.driver.signal();
    }

    pub(crate) fn take_exited(&self) -> Vec<std::thread::ThreadId> {
        std::mem::take(&mut relock(&self.inner).exited)
    }
}
