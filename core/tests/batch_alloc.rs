use profluens_core::batch::Batch;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::id::FormatId;
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;

use crate::link_leak::alloc_calls;

const N: usize = 32;

// The stash and the twin Vec are test scaffolding, built once.
#[allow(clippy::disallowed_methods)]
#[test]
fn cold_batch_allocates_like_one_growing_vec() {
    let pool = Pool::bounded(64, N as u32);
    let mut stash: Vec<Buffer> = Vec::with_capacity(N);
    for i in 0..N {
        stash.push(Buffer {
            memory: pool.acquire(),
            pts: Timestamp::from_nanos(i as u64),
            dts: Timestamp::NONE,
            duration: Timestamp::NONE,
            flags: BufferFlags::empty(),
            format: FormatId(0),
            sync: None,
        });
    }

    let mut twin: Vec<Buffer> = Vec::new();
    let before = alloc_calls();
    for b in stash.drain(..) {
        twin.push(b);
    }
    let vec_growth = alloc_calls() - before;
    assert!(vec_growth > 0, "a Vec grown from empty allocates");

    let mut batch = Batch::new(FormatId(0));
    let before = alloc_calls();
    for b in twin.drain(..) {
        batch.push(b);
    }
    let cold = alloc_calls() - before;
    assert_eq!(cold, vec_growth, "cold batch of {N} costs one Vec's growth");

    let before = alloc_calls();
    while let Some(b) = batch.pop_front() {
        stash.push(b);
    }
    for b in stash.drain(..) {
        batch.push(b);
    }
    assert_eq!(alloc_calls() - before, 0, "a warm shell allocates nothing");
    assert_eq!(batch.len(), N);
}

#[test]
fn empty_shell_allocates_nothing() {
    let before = alloc_calls();
    let batch = Batch::new(FormatId(0));
    assert_eq!(batch.cap_rows(), 0);
    drop(batch);
    assert_eq!(alloc_calls() - before, 0, "Batch::new is allocation free");
}
