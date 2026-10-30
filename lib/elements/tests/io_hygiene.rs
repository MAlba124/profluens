//! Page-cache hygiene. Advice from the reactor must never perturb the data.

// A test's own fixtures and sockets, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use pf_reactor::{
    Hint, OsReactor, POSIX_FADV_DONTNEED, POSIX_FADV_SEQUENTIAL, SYNC_FILE_RANGE_WRITE,
    StreamHygiene, fadvise, sync_file_range,
};
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::id::{ElementId, FormatId};
use profluens_core::io::{Handle, IoResult, Name, Op, OpId, OpenMode, Reactor, Submission};
use profluens_core::memory::Pool;
use profluens_core::time::Timestamp;

fn tmp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pf_io_hygiene_{tag}_{}", std::process::id()))
}

#[test]
fn shims_propagate_errors_without_panicking() {
    let fa = fadvise(-1, 0, 0, POSIX_FADV_SEQUENTIAL);
    let sfr = sync_file_range(-1, 0, 0, SYNC_FILE_RANGE_WRITE);
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        assert_eq!(fa.unwrap_err().raw_os_error(), Some(9), "fadvise(-1) => EBADF");
        assert_eq!(sfr.unwrap_err().raw_os_error(), Some(9), "sync_file_range(-1) => EBADF");
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        assert!(fa.is_ok() && sfr.is_ok(), "no-op shims report success");
    }
}

#[test]
fn shims_succeed_on_a_real_file() {
    let path = tmp_path("shims");
    let mut f = File::create(&path).expect("create temp file");
    f.write_all(&vec![7u8; 256 * 1024]).expect("write");
    let fd = f.as_raw_fd();
    fadvise(fd, 0, 0, POSIX_FADV_SEQUENTIAL).expect("SEQUENTIAL");
    sync_file_range(fd, 0, 128 * 1024, SYNC_FILE_RANGE_WRITE).expect("writeback kick");
    fadvise(fd, 0, 128 * 1024, POSIX_FADV_DONTNEED).expect("DONTNEED behind");
    drop(f);
    std::fs::remove_file(&path).ok();
}

#[test]
fn hygiene_tracker_hints_by_window_and_stops_on_a_backward_op() {
    let mut h = StreamHygiene::with_window(1024);
    assert_eq!(h.on_read(0, 2048), Some(Hint::Drop { at: 0, len: 2048 }));
    assert_eq!(h.on_read(1024, 100), None, "backward: read hygiene off for good");
    assert_eq!(h.on_read(1 << 40, 4096), None);

    let mut w = StreamHygiene::with_window(1024);
    assert_eq!(w.on_write(0, 512), [None, None]);
    assert_eq!(w.on_write(512, 1024), [Some(Hint::Sync { at: 0, len: 1536 }), None]);
    let both = [Some(Hint::Sync { at: 1536, len: 1024 }), Some(Hint::Drop { at: 0, len: 1536 })];
    assert_eq!(w.on_write(1536, 1024), both, "the window before the last kicked is dropped");
    assert_eq!(w.on_write(0, 4096), [None, None], "overwrite: write hygiene off for good");
    assert_eq!(w.on_write(u64::MAX - 4096, 4096), [None, None]);
}

fn buf_from(pool: &Pool, data: &[u8]) -> Buffer {
    let mut m = pool.acquire();
    m.as_mut_full()[..data.len()].copy_from_slice(data);
    m.set_len(data.len());
    Buffer {
        memory: m,
        pts: Timestamp::ZERO,
        dts: Timestamp::ZERO,
        duration: Timestamp::ZERO,
        flags: BufferFlags::empty(),
        format: FormatId(0),
        sync: None,
    }
}

fn open(r: &mut OsReactor, path: &std::path::Path, mode: OpenMode) -> Handle {
    let names = path.as_os_str().as_encoded_bytes();
    let kind = Op::Open { path: Name { off: 0, len: names.len() as u32 }, mode };
    r.submit(
        &mut vec![Submission {
            op: OpId(u64::MAX),
            element: ElementId(0),
            kind,
            buf: None,
            user: 0,
        }],
        names,
    );
    let mut done = Vec::new();
    let start = std::time::Instant::now();
    while done.is_empty() && start.elapsed() < std::time::Duration::from_secs(10) {
        r.run_once(&mut done);
        r.park(std::time::Duration::from_millis(1));
    }
    match done.pop().map(|(_, c)| c.result) {
        Some(IoResult::File { handle, .. }) => handle,
        other => panic!("open gave {other:?}"),
    }
}

#[test]
fn sync_reactor_hygiene_preserves_data() {
    const SLOT: usize = 16 * 1024;
    const SLOTS: usize = 24; // 384 KiB total, 12 windows deep

    let path = tmp_path("reactor");
    let pool = Pool::new(SLOT);
    // Epoll file ops run at submission, so one pass completes what was submitted.
    let mut r = OsReactor::on(pf_reactor::Want::Epoll).expect("a reactor");
    r.set_hygiene_window(32 * 1024); // trip the windows every other op

    let writer = ElementId(1);
    let file = open(&mut r, &path, OpenMode::Create);
    let expect: Vec<u8> = (0..SLOT * SLOTS).map(|i| (i % 251) as u8).collect();
    for (i, chunk) in expect.chunks(SLOT).enumerate() {
        r.submit(
            &mut vec![Submission {
                op: OpId(i as u64),
                element: writer,
                kind: Op::Write { handle: file, offset: (i * SLOT) as u64 },
                buf: Some(buf_from(&pool, chunk)),
                user: i as u64,
            }],
            &[],
        );
        if i % 3 == 2 {
            for (_, c) in {
                let mut completions = Vec::new();
                r.run_once(&mut completions);
                completions
            } {
                assert!(matches!(c.result, IoResult::Ok(SLOT)), "write completion ok");
            }
        }
    }
    for (_, c) in {
        let mut completions = Vec::new();
        r.run_once(&mut completions);
        completions
    } {
        assert!(matches!(c.result, IoResult::Ok(SLOT)), "write completion ok");
    }
    assert!(r.is_idle());

    let mut on_disk = Vec::new();
    File::open(&path).expect("reopen").read_to_end(&mut on_disk).expect("read back");
    assert_eq!(on_disk, expect, "hygiene must not perturb written data");

    let reader = ElementId(2);
    let file = open(&mut r, &path, OpenMode::ReadOnce);
    let mut got = vec![0u8; 0];
    for i in 0..SLOTS {
        r.submit(
            &mut vec![Submission {
                op: OpId(1000 + i as u64),
                element: reader,
                kind: Op::Read { handle: file, offset: (i * SLOT) as u64, at: 0, len: u32::MAX },
                buf: Some(buf_from(&pool, &[])),
                user: i as u64,
            }],
            &[],
        );
    }
    let mut completions = Vec::new();
    r.run_once(&mut completions);
    completions.sort_by_key(|(_, c)| c.user);
    for (_, c) in completions {
        assert!(matches!(c.result, IoResult::Ok(SLOT)), "read completion ok");
        got.extend_from_slice(c.buf.expect("the buffer rides back").memory.data());
    }
    assert_eq!(got, expect, "hygiene must not perturb read data");

    std::fs::remove_file(&path).ok();
}

#[allow(unsafe_code)]
fn resident(path: &std::path::Path) -> (usize, usize) {
    let f = File::open(path).expect("open");
    let len = f.metadata().expect("meta").len() as usize;
    let page = 4096;
    let pages = len.div_ceil(page);
    let mut vec = vec![0u8; pages];
    // SAFETY: a private read-only map of a file this test made, unmapped below.
    let at = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0)
    };
    assert_ne!(at, libc::MAP_FAILED);
    // SAFETY: `vec` holds a byte for each page of the map.
    assert_eq!(unsafe { libc::mincore(at, len, vec.as_mut_ptr()) }, 0);
    // SAFETY: mapped above with this length.
    unsafe { libc::munmap(at, len) };
    (vec.iter().filter(|b| **b & 1 != 0).count(), pages)
}

// tmpfs ignores DONTNEED, its pages are the file.
#[allow(unsafe_code)]
fn drops_pages(path: &std::path::Path) -> bool {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
    // SAFETY: a zeroed struct the call fills, and a terminated path.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::statfs(c.as_ptr(), &mut st) == 0 && st.f_type != libc::TMPFS_MAGIC }
}

fn read_through(
    r: &mut OsReactor,
    pool: &Pool,
    path: &std::path::Path,
    mode: OpenMode,
    slot: usize,
    slots: usize,
) {
    let file = open(r, path, mode);
    for i in 0..slots {
        r.submit(
            &mut vec![Submission {
                op: OpId(5000 + i as u64),
                element: ElementId(3),
                kind: Op::Read { handle: file, offset: (i * slot) as u64, at: 0, len: u32::MAX },
                buf: Some(buf_from(pool, &[])),
                user: i as u64,
            }],
            &[],
        );
        let mut done = Vec::new();
        r.run_once(&mut done);
        assert!(matches!(done[0].1.result, IoResult::Ok(n) if n == slot));
    }
}

#[test]
fn only_a_file_read_once_is_dropped_from_the_page_cache() {
    // Several MiB, as large folios drop only the folios a range covers whole.
    const SLOT: usize = 1 << 20;
    const SLOTS: usize = 32; // 32 MiB, 8 windows deep
    let path = tmp_path("residency");
    let data: Vec<u8> = (0..SLOT * SLOTS).map(|i| (i % 253) as u8).collect();
    let mut f = File::create(&path).expect("create");
    f.write_all(&data).expect("write");
    // DONTNEED drops only clean pages.
    f.sync_all().expect("sync");
    drop(f);
    let pool = Pool::new(SLOT);
    let mut r = OsReactor::on(pf_reactor::Want::Epoll).expect("a reactor");
    r.set_hygiene_window(4 << 20);

    read_through(&mut r, &pool, &path, OpenMode::Read, SLOT, SLOTS);
    let (kept, pages) = resident(&path);
    assert!(kept * 10 >= pages * 9, "a plain read dropped its pages, {kept} of {pages} cached");

    read_through(&mut r, &pool, &path, OpenMode::ReadOnce, SLOT, SLOTS);
    let (left, _) = resident(&path);
    if drops_pages(&path) {
        assert!(left * 4 <= pages, "a read once kept its pages, {left} of {pages} cached");
    }
    std::fs::remove_file(&path).ok();
}
