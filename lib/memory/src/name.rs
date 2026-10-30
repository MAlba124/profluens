use std::ffi::CStr;

use crate::error::{MemoryError, errno};
use crate::mmap::page_size;

pub const HEAP_NAMED_MIN: usize = 64 << 10;

pub fn name_anon(base: usize, len: usize, name: Option<&CStr>) -> Result<(), MemoryError> {
    let ptr = name.map_or(std::ptr::null(), CStr::as_ptr);
    let (set, anon) = (libc::PR_SET_VMA, libc::PR_SET_VMA_ANON_NAME as libc::c_ulong);
    // SAFETY: the kernel reads the name up to its NUL and only changes the mapping's label.
    let rc = unsafe { libc::prctl(set, anon, base as libc::c_ulong, len as libc::c_ulong, ptr) };
    if rc < 0 { Err(MemoryError::Name { errno: errno() }) } else { Ok(()) }
}

fn interior(base: usize, len: usize) -> (usize, usize) {
    let page = page_size();
    let start = base.next_multiple_of(page);
    let end = (base + len) / page * page;
    (start, end.saturating_sub(start))
}

unsafe fn heap_namer(base: *mut u8, len: usize, name: Option<&CStr>) -> bool {
    let (start, len) = interior(base as usize, len);
    len >= HEAP_NAMED_MIN && name_anon(start, len, name).is_ok()
}

pub fn install() {
    profluens_core::memory::set_namer(heap_namer);
}

pub(crate) fn udmabuf_name<'a>(buf: &'a mut [u8; 96], region: &CStr) -> &'a CStr {
    let rest = region.to_bytes().strip_prefix(b"pf:pool:").unwrap_or(b"");
    let head: &[u8] = if rest.is_empty() { b"pf:udmabuf" } else { b"pf:udmabuf:" };
    let take = rest.len().min(buf.len() - 1 - head.len());
    buf[..head.len()].copy_from_slice(head);
    buf[head.len()..head.len() + take].copy_from_slice(&rest[..take]);
    buf[head.len() + take] = 0;
    CStr::from_bytes_until_nul(&buf[..]).unwrap_or(c"pf:udmabuf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heap_region_names_only_its_whole_pages() {
        let page = page_size();
        assert_eq!(interior(page + 16, 4 * page), (2 * page, 3 * page));
        assert_eq!(interior(page, 4 * page), (page, 4 * page));
        assert_eq!(interior(page + 16, page), (2 * page, 0), "no whole page inside");
    }

    #[test]
    fn a_udmabuf_memfd_is_named_after_its_pool_region() {
        let mut buf = [0u8; 96];
        assert_eq!(udmabuf_name(&mut buf, c"pf:pool:av1dec:3M"), c"pf:udmabuf:av1dec:3M");
        assert_eq!(udmabuf_name(&mut buf, c""), c"pf:udmabuf");
    }
}
