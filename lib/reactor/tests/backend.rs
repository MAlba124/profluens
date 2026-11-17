//! The shared backend suite in `suite/` on io_uring.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use pf_reactor::Want;
use profluens_core::io::{Reactor, ReactorFactory};

const NAME: &str = "uring";
const WANT: Want = Want::Uring;

fn make() -> Box<dyn Reactor> {
    Box::new(pf_reactor::OsReactor::on(WANT).expect("a ring on this kernel"))
}

/// Reads of each file one at a time in submission order, whatever the storage.
fn make_in_order() -> Box<dyn Reactor> {
    let mut r = pf_reactor::OsReactor::on(WANT).expect("a ring on this kernel");
    r.set_reads(pf_reactor::Reads::InOrder);
    Box::new(r)
}

/// Reads of each file as many at once as asked, whatever the storage.
fn make_free() -> Box<dyn Reactor> {
    let mut r = pf_reactor::OsReactor::on(WANT).expect("a ring on this kernel");
    r.set_reads(pf_reactor::Reads::Free);
    Box::new(r)
}

fn make_files() -> Box<dyn Reactor> {
    Box::new(pf_reactor::OsReactor::files(WANT, 32).expect("a ring on this kernel"))
}

fn factory() -> ReactorFactory {
    pf_reactor::factory_on(Want::Uring)
}

#[path = "suite/contract.rs"]
mod contract;
#[path = "suite/os.rs"]
mod os;
#[path = "suite/queue.rs"]
mod queue;
#[path = "suite/wake.rs"]
mod wake;
