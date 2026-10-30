//! Pool regions a device imports, a sealed memfd whose page-rounded slots are each a dmabuf.

use std::ffi::CStr;
use std::os::unix::io::RawFd;
use std::sync::{Mutex, OnceLock};

use profluens_core::memory::{Allocator, Memory};

use crate::error::{MemoryError, errno};
use crate::mmap::page_size;
use crate::name::udmabuf_name;

const DEVICE_PATH: &str = "/dev/udmabuf";

const fn iow(ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    (1 << 30) | ((size as libc::c_ulong) << 16) | ((ty as libc::c_ulong) << 8) | nr as libc::c_ulong
}

#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

const UDMABUF_CREATE: libc::c_ulong = iow(b'u', 0x42, std::mem::size_of::<UdmabufCreate>());
const UDMABUF_FLAGS_CLOEXEC: u32 = 0x01;

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

const DMA_BUF_IOCTL_SYNC: libc::c_ulong = iow(b'b', 0, std::mem::size_of::<DmaBufSync>());
const DMA_BUF_SYNC_WRITE: u64 = 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

const MAGIC: u64 = 0x7564_6d61_6275_6621;

struct Export {
    magic: u64,
    stride: usize,
    fds: Box<[RawFd]>,
}

pub struct Udmabuf {
    dev: RawFd,
    mappings: Mutex<Vec<(usize, RawFd)>>,
    last_error: Mutex<Option<MemoryError>>,
}

static DEVICE: OnceLock<Result<Udmabuf, MemoryError>> = OnceLock::new();

pub fn device() -> Result<&'static Udmabuf, MemoryError> {
    DEVICE.get_or_init(Udmabuf::open).as_ref().map_err(Clone::clone)
}

pub fn align() -> usize {
    page_size()
}

impl Udmabuf {
    // Once per process, the device and the empty tables.
    #[allow(clippy::disallowed_methods)]
    fn open() -> Result<Udmabuf, MemoryError> {
        // SAFETY: a NUL-terminated literal path.
        let dev = unsafe { libc::open(c"/dev/udmabuf".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if dev < 0 {
            return Err(MemoryError::DeviceOpen { path: DEVICE_PATH, errno: errno() });
        }
        Ok(Udmabuf { dev, mappings: Mutex::new(Vec::new()), last_error: Mutex::new(None) })
    }

    pub fn allocator(&'static self) -> Allocator {
        // SAFETY: `map` gives zeroed udmabuf pages nothing else touches, `unmap` returns them
        // once, and `&'static self` outlives every view.
        unsafe { Allocator::new(self as *const Udmabuf as *mut (), map, unmap, Some(register)) }
    }

    pub fn last_error(&self) -> Option<MemoryError> {
        self.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn fail<T>(&self, error: MemoryError) -> Option<T> {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        None
    }

    fn map(&self, len: usize, name: &CStr) -> Option<*mut u8> {
        let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
        // SAFETY: a NUL-terminated name, memfd_create(2).
        let memfd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
        if memfd < 0 {
            return self.fail(MemoryError::Memfd { errno: errno() });
        }
        let close = |e| {
            // SAFETY: our own fd, closed once.
            unsafe { libc::close(memfd) };
            self.fail(e)
        };
        // SAFETY: plain fds and integers, ftruncate(2) and fcntl(2).
        if unsafe { libc::ftruncate(memfd, len as libc::off_t) } < 0 {
            return close(MemoryError::Truncate { len, errno: errno() });
        }
        if unsafe { libc::fcntl(memfd, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) } < 0 {
            return close(MemoryError::Seal { errno: errno() });
        }
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        // SAFETY: mmap(2) of the memfd, MAP_SHARED so the device sees what the CPU writes.
        let base =
            unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, memfd, 0) };
        if base == libc::MAP_FAILED {
            return close(MemoryError::Map { len, errno: errno() });
        }
        self.mappings.lock().unwrap_or_else(|e| e.into_inner()).push((base as usize, memfd));
        Some(base as *mut u8)
    }

    // One table per region.
    #[allow(clippy::disallowed_methods)]
    fn register(&self, base: usize, len: usize, stride: usize) -> Option<u64> {
        let page = page_size();
        if !stride.is_multiple_of(page) {
            return self.fail(MemoryError::Unaligned { stride, page });
        }
        let memfd = {
            let mappings = self.mappings.lock().unwrap_or_else(|e| e.into_inner());
            mappings.iter().find(|(b, _)| *b == base).map(|(_, fd)| *fd)?
        };
        let slots = (len / stride) as u32;
        let mut fds = Vec::with_capacity(slots as usize);
        for slot in 0..slots {
            let create = UdmabufCreate {
                memfd: memfd as u32,
                flags: UDMABUF_FLAGS_CLOEXEC,
                offset: (slot as usize * stride) as u64,
                size: stride as u64,
            };
            // SAFETY: the ioctl reads the struct, which is laid out as the header says.
            let fd =
                unsafe { libc::ioctl(self.dev, UDMABUF_CREATE, &create as *const UdmabufCreate) };
            if fd < 0 {
                let error = MemoryError::Create { slot, errno: errno() };
                close_all(&fds);
                return self.fail(error);
            }
            fds.push(fd);
        }
        let export = Box::new(Export { magic: MAGIC, stride, fds: fds.into_boxed_slice() });
        Some(Box::into_raw(export) as u64)
    }

    fn unmap(&self, base: usize, len: usize, token: u64) {
        if token != 0 {
            // SAFETY: the token `register` made for this region, taken back once here.
            let export = unsafe { Box::from_raw(token as *mut Export) };
            close_all(&export.fds);
        }
        // SAFETY: the mapping `map` made.
        unsafe { libc::munmap(base as *mut libc::c_void, len) };
        let mut mappings = self.mappings.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = mappings.iter().position(|(b, _)| *b == base) {
            let (_, memfd) = mappings.swap_remove(at);
            // SAFETY: our own fd, closed once.
            unsafe { libc::close(memfd) };
        }
    }
}

fn close_all(fds: &[RawFd]) {
    for &fd in fds {
        // SAFETY: fds this module made, each closed once.
        unsafe { libc::close(fd) };
    }
}

unsafe fn map(ctx: *mut (), len: usize, align: usize, name: &CStr) -> *mut u8 {
    // SAFETY: `ctx` is the `'static` device `allocator` was made from.
    let dev = unsafe { &*(ctx as *const Udmabuf) };
    let page = page_size();
    if align > page {
        return dev
            .fail(MemoryError::Unaligned { stride: align, page })
            .unwrap_or(std::ptr::null_mut());
    }
    let mut buf = [0u8; 96];
    dev.map(len.div_ceil(page) * page, udmabuf_name(&mut buf, name)).unwrap_or(std::ptr::null_mut())
}

unsafe fn register(ctx: *mut (), base: *mut u8, len: usize, stride: usize) -> Option<u64> {
    // SAFETY: as in `map`.
    let dev = unsafe { &*(ctx as *const Udmabuf) };
    dev.register(base as usize, len, stride)
}

unsafe fn unmap(ctx: *mut (), base: *mut u8, len: usize, _align: usize, token: u64) {
    // SAFETY: as in `map`.
    let dev = unsafe { &*(ctx as *const Udmabuf) };
    dev.unmap(base as usize, len.div_ceil(page_size()) * page_size(), token)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dmabuf {
    pub fd: RawFd,
    pub offset: usize,
    pub size: usize,
}

impl Dmabuf {
    pub fn of(memory: &Memory) -> Result<Dmabuf, MemoryError> {
        let (ctx, token, offset) = memory.device().ok_or(MemoryError::NotDmabuf)?;
        let dev = device()?;
        if ctx != dev as *const Udmabuf as *mut () || token == 0 {
            return Err(MemoryError::NotDmabuf);
        }
        // SAFETY: a token of this device is a live `Export` until its region unmaps, which `memory`
        // prevents.
        let export = unsafe { &*(token as *const Export) };
        if export.magic != MAGIC {
            return Err(MemoryError::NotDmabuf);
        }
        let slot = offset / export.stride;
        let fd = *export.fds.get(slot).ok_or(MemoryError::NotDmabuf)?;
        Ok(Dmabuf { fd, offset: offset % export.stride, size: export.stride })
    }

    pub fn dup(&self) -> Result<RawFd, MemoryError> {
        // SAFETY: a live fd.
        let fd = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 { Err(MemoryError::Dup { errno: errno() }) } else { Ok(fd) }
    }
}

pub fn sync_write(fd: RawFd, start: bool) -> Result<(), MemoryError> {
    let flags = DMA_BUF_SYNC_WRITE | if start { 0 } else { DMA_BUF_SYNC_END };
    let arg = DmaBufSync { flags };
    // SAFETY: the ioctl reads the one-word struct.
    if unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &arg as *const DmaBufSync) } < 0 {
        return Err(MemoryError::Sync { errno: errno() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_headers() {
        assert_eq!(std::mem::size_of::<UdmabufCreate>(), 24);
        assert_eq!(UDMABUF_CREATE, 0x4018_7542);
        assert_eq!(DMA_BUF_IOCTL_SYNC, 0x4008_6200);
    }
}
