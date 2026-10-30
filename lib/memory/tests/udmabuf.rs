use pf_memory::MemoryError;
use pf_memory::udmabuf::{self, Dmabuf, sync_write};
use profluens_core::memory::{Pool, PoolConfig};

fn device() -> Option<&'static udmabuf::Udmabuf> {
    match udmabuf::device() {
        Ok(dev) => Some(dev),
        Err(e @ MemoryError::DeviceOpen { .. }) => {
            eprintln!("skip: {e}");
            None
        }
        Err(other) => panic!("{other}"),
    }
}

#[test]
fn a_slot_is_a_dmabuf_the_cpu_writes_and_a_mapping_of_it_reads() {
    let Some(dev) = device() else { return };
    let page = udmabuf::align();
    let config = PoolConfig {
        slot_size: 3 * page / 2,
        slots: 3,
        bytes: 0,
        align: page,
        allocator: dev.allocator(),
    };
    let pool = Pool::with_config(config);
    let mut first = pool.try_acquire().unwrap_or_else(|| panic!("a slot ({:?})", dev.last_error()));
    let second = pool.try_acquire().expect("another");
    let a = Dmabuf::of(&first).expect("a dmabuf");
    let b = Dmabuf::of(&second).expect("a dmabuf");
    assert_ne!(a.fd, b.fd, "one dmabuf a slot");
    assert_eq!((a.offset, a.size), (0, 2 * page), "the slot rounded to pages");
    let s = pool.stats();
    assert_eq!((s.regions, s.bytes_mapped), (1, 3 * 2 * page as u64));

    sync_write(a.fd, true).expect("sync start");
    first.as_mut_full()[..8].copy_from_slice(b"dmabuf!!");
    first.as_mut_full()[page + 1] = 0x5A;
    first.set_len(page + 2);
    sync_write(a.fd, false).expect("sync end");

    let dup = a.dup().expect("dup");
    // SAFETY: a dmabuf fd maps read-only for the check, and is unmapped and closed below.
    let view = unsafe {
        libc::mmap(std::ptr::null_mut(), a.size, libc::PROT_READ, libc::MAP_SHARED, dup, 0)
    };
    assert_ne!(view, libc::MAP_FAILED, "the dmabuf maps ({})", std::io::Error::last_os_error());
    let bytes = unsafe { std::slice::from_raw_parts(view as *const u8, a.size) };
    assert_eq!(&bytes[..8], b"dmabuf!!");
    assert_eq!(bytes[page + 1], 0x5A);
    unsafe {
        libc::munmap(view, a.size);
        libc::close(dup);
    }

    let tail = first.slice(page, 2).expect("in bounds");
    assert_eq!(Dmabuf::of(&tail).unwrap(), Dmabuf { fd: a.fd, offset: page, size: 2 * page });
    let heap = Pool::new(64).acquire();
    assert_eq!(Dmabuf::of(&heap), Err(MemoryError::NotDmabuf));
    drop((first, second, tail));
    drop(pool);
}

#[test]
fn a_stride_that_is_not_whole_pages_is_refused() {
    let Some(dev) = device() else { return };
    let config =
        PoolConfig { slot_size: 100, slots: 2, bytes: 200, align: 64, allocator: dev.allocator() };
    let pool = Pool::with_config(config);
    assert!(pool.try_acquire().is_none(), "no region at a 128 byte stride");
    assert!(
        matches!(dev.last_error(), Some(MemoryError::Unaligned { .. })),
        "{:?}",
        dev.last_error()
    );
    assert_eq!(pool.stats().bytes_mapped, 0, "the mapping was undone");
}
