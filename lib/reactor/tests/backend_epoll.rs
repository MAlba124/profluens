//! The shared backend suite in `suite/` on epoll.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use pf_reactor::Want;
use profluens_core::io::{Reactor, ReactorFactory};

const NAME: &str = "epoll";
const WANT: Want = Want::Epoll;

fn make() -> Box<dyn Reactor> {
    Box::new(pf_reactor::OsReactor::on(WANT).expect("epoll"))
}

fn make_files() -> Box<dyn Reactor> {
    Box::new(pf_reactor::OsReactor::files(WANT, 32).expect("epoll"))
}

fn factory() -> ReactorFactory {
    pf_reactor::factory_on(Want::Epoll)
}

#[path = "suite/contract.rs"]
mod contract;
#[path = "suite/os.rs"]
mod os;
#[path = "suite/queue.rs"]
mod queue;
#[path = "suite/wake.rs"]
mod wake;
