//! `filesrc` against the whole [`Reactor`] contract. Completions may arrive in any order
//! and a read may come up short. Both shims below are legal reactors.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods)]

use pf_reactor::OsReactor;
use profluens_core::id::ElementId;
use profluens_core::io::{Completion, Reactor, Submission};
use profluens_core::pipeline::Pipeline;
use profluens_elements::io::{FileSink, FileSrc};

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pf_contract_{}_{}.bin", tag, std::process::id()));
    p
}

/// Reverses each pass's completions.
struct ReverseReactor(OsReactor);

impl Reactor for ReverseReactor {
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]) {
        self.0.submit(subs, names)
    }
    fn due_in(&self) -> Option<std::time::Duration> {
        self.0.due_in()
    }
    fn is_idle(&self) -> bool {
        self.0.is_idle()
    }
    fn park(&mut self, timeout: std::time::Duration) -> bool {
        self.0.park(timeout)
    }
    fn set_waker(&mut self, waker: profluens_core::io::Waker) {
        self.0.set_waker(waker)
    }
    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        self.0.run_once(out);
        out.reverse();
    }
}

/// Truncates every read to half the buffer.
struct ShortReadReactor(OsReactor);

impl Reactor for ShortReadReactor {
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]) {
        self.0.submit(subs, names)
    }
    fn due_in(&self) -> Option<std::time::Duration> {
        self.0.due_in()
    }
    fn is_idle(&self) -> bool {
        self.0.is_idle()
    }
    fn park(&mut self, timeout: std::time::Duration) -> bool {
        self.0.park(timeout)
    }
    fn set_waker(&mut self, waker: profluens_core::io::Waker) {
        self.0.set_waker(waker)
    }
    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        self.0.run_once(out);
        for (_, c) in out.iter_mut() {
            if let profluens_core::io::IoResult::Ok(n) = c.result
                && n > 1
            {
                let half = n / 2;
                if let Some(buf) = c.buf.as_mut() {
                    buf.memory.set_len(half);
                    c.result = profluens_core::io::IoResult::Ok(half);
                }
            }
        }
    }
}

fn copy_with<R: Reactor + 'static>(
    tag: &str,
    data: &[u8],
    make: impl Fn() -> R + Send + Sync + 'static,
) -> Vec<u8> {
    let inp = temp_path(&format!("{tag}_in"));
    let outp = temp_path(&format!("{tag}_out"));
    std::fs::write(&inp, data).expect("write input");

    let mut p = Pipeline::new();
    p.set_reactor_factory(std::sync::Arc::new(move || Ok(Box::new(make()) as Box<dyn Reactor>)));
    let src = p.add(FileSrc::new(&inp));
    let sink = p.add(FileSink::new(&outp));
    p.link(src.src(FileSrc::OUT), sink.sink(FileSink::IN)).expect("link");
    p.run().expect("run");

    let got = std::fs::read(&outp).expect("read output");
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
    got
}

fn fixture() -> Vec<u8> {
    (0..1_000_003u32).map(|i| (i % 251) as u8).collect()
}

fn assert_same(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length differs");
    if let Some(i) = got.iter().zip(want).position(|(a, b)| a != b) {
        panic!("{what}: first difference at byte {i} (got {}, want {})", got[i], want[i]);
    }
}

#[test]
fn out_of_order_completions_do_not_permute_the_stream() {
    let data = fixture();
    let got = copy_with("reverse", &data, || ReverseReactor(OsReactor::new().expect("a reactor")));
    assert_same(&got, &data, "filesrc must re-sequence completions, not trust arrival order");
}

#[test]
fn short_reads_do_not_punch_holes() {
    let data = fixture();
    let got = copy_with("short", &data, || ShortReadReactor(OsReactor::new().expect("a reactor")));
    assert_same(&got, &data, "a short read must resume at off+n, not off+capacity");
}
