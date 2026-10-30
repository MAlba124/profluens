//! Every `unsafe` of the crate lives here.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};

use profluens_core::buffer::Buffer;
use stein_io::queue::{Group, Mem, Queue, RingPage};

pub fn fadvise(fd: RawFd, offset: u64, len: u64, advice: i32) -> io::Result<()> {
    // SAFETY: integers only.
    let ret = unsafe { libc::posix_fadvise(fd, offset as libc::off_t, len as libc::off_t, advice) };
    if ret != 0 { Err(io::Error::from_raw_os_error(ret)) } else { Ok(()) }
}

pub fn sync_file_range(fd: RawFd, offset: u64, len: u64, flags: u32) -> io::Result<()> {
    // SAFETY: integers only.
    let ret =
        unsafe { libc::sync_file_range(fd, offset as libc::off64_t, len as libc::off64_t, flags) };
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// A descriptor of its own on the open file description `fd` names, no path resolved.
pub(crate) fn dup(fd: RawFd) -> rustix::io::Result<OwnedFd> {
    // SAFETY: the element that submitted `fd` keeps it open until its op completes, and the op
    // completes within this submit.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    rustix::io::fcntl_dupfd_cloexec(fd, 0)
}

pub(crate) fn adopt(fd: RawFd) -> OwnedFd {
    // SAFETY: the kernel made it for this completion and nobody else holds it.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// The whole of a buffer, for the kernel to read or receive into. The op table keeps it until
/// the op's last completion, and moving a `Memory` view moves no byte.
pub(crate) fn into(buf: &mut Buffer) -> Mem {
    into_range(buf, 0, u32::MAX)
}

pub(crate) fn into_range(buf: &mut Buffer, at: u32, len: u32) -> Mem {
    let full = buf.memory.as_mut_full();
    let from = (at as usize).min(full.len());
    let to = from + (len as usize).min(full.len() - from);
    let part = &mut full[from..to];
    // SAFETY: as above, and `as_mut_full` made the view the slot's only one.
    unsafe { Mem::raw(part.as_mut_ptr(), part.len()) }
}

pub(crate) fn from(buf: &Buffer, skip: usize) -> Mem {
    let data = buf.memory.data();
    let rest = data.get(skip..).unwrap_or(&[]);
    // SAFETY: as for `into`. The kernel only reads, so shared views are not disturbed.
    unsafe { Mem::raw(rest.as_ptr().cast_mut(), rest.len()) }
}

pub(crate) fn block(room: &mut [u8], at: usize, len: usize) -> Mem {
    let (from, to) = (at.min(room.len()), (at + len).min(room.len()));
    let part = &mut room[from..to];
    // SAFETY: the reactor keeps the block past the queue's drop and lends it to one op at a time.
    unsafe { Mem::raw(part.as_mut_ptr(), part.len()) }
}

pub(crate) fn add_group(
    q: &mut Queue,
    page: &mut RingPage,
    entries: u16,
) -> Result<Group, stein_io::IoError> {
    // SAFETY: the page is boxed, zeroed, kept past the queue's drop and untouched until removal.
    unsafe { q.add_group(page.0.as_mut_ptr().cast(), entries) }
}

pub(crate) fn set_sockopt(fd: RawFd, level: i32, name: i32, value: i32) {
    let v = value.to_ne_bytes();
    // SAFETY: four bytes of option value and their length.
    let _ = unsafe { libc::setsockopt(fd, level, name, v.as_ptr().cast(), 4) };
}

pub(crate) fn word(w: &mut [u8; 8]) -> Mem {
    // SAFETY: the reactor drops its queue, which ends the read, before the box.
    unsafe { Mem::raw(w.as_mut_ptr(), 8) }
}

/// Class IN (RFC 1035 section 3.2.4).
const C_IN: libc::c_int = 1;

/// Opaque glibc `res_ninit` state (568 bytes on x86_64 in 2.42), over-allocated, no layout.
#[cfg(not(target_env = "musl"))]
#[repr(C, align(16))]
struct ResState([u8; 4096]);

// In libc.so.6 since glibc 2.34. `resolv.h` macros map `res_ninit` and `res_nclose` here.
#[cfg(not(target_env = "musl"))]
unsafe extern "C" {
    fn __res_ninit(state: *mut ResState) -> libc::c_int;
    fn res_nquery(
        state: *mut ResState,
        dname: *const libc::c_char,
        class: libc::c_int,
        kind: libc::c_int,
        answer: *mut u8,
        anslen: libc::c_int,
    ) -> libc::c_int;
    fn __res_nclose(state: *mut ResState);
}

/// One blocking query of `host` for `kind` records (RFC 1035 4.1.2). `None` on any failure.
#[cfg(not(target_env = "musl"))]
pub(crate) fn query(host: &std::ffi::CStr, kind: u16, out: &mut [u8]) -> Option<usize> {
    let mut state = ResState([0; 4096]);
    let len = libc::c_int::try_from(out.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: zeroed state larger than the struct, a terminated name and `len` bytes of room.
    let n = unsafe {
        if __res_ninit(&mut state) != 0 {
            return None;
        }
        let n = res_nquery(
            &mut state,
            host.as_ptr(),
            C_IN,
            libc::c_int::from(kind),
            out.as_mut_ptr(),
            len,
        );
        __res_nclose(&mut state);
        n
    };
    usize::try_from(n).ok().filter(|n| *n > 0).map(|n| n.min(out.len()))
}

#[cfg(target_env = "musl")]
pub(crate) fn query(host: &std::ffi::CStr, kind: u16, out: &mut [u8]) -> Option<usize> {
    extern "C" {
        fn res_query(
            dname: *const libc::c_char,
            class: libc::c_int,
            kind: libc::c_int,
            answer: *mut u8,
            anslen: libc::c_int,
        ) -> libc::c_int;
    }
    let len = libc::c_int::try_from(out.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: a terminated name and `len` bytes of answer room.
    let n =
        unsafe { res_query(host.as_ptr(), C_IN, libc::c_int::from(kind), out.as_mut_ptr(), len) };
    usize::try_from(n).ok().filter(|n| *n > 0).map(|n| n.min(out.len()))
}
