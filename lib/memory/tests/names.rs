use pf_memory::mmap::{self, page_size};
use pf_memory::smaps::{self, Kind};
use pf_memory::{MemoryError, name, udmabuf};
use profluens_core::memory::{Pool, PoolConfig};

// A test reading its own proc file.
#[allow(clippy::disallowed_methods)]
fn resident(name: &str) -> Option<(u64, u32)> {
    let text = std::fs::read_to_string("/proc/self/smaps").expect("smaps");
    let report = smaps::parse(&text);
    report.rows.iter().find(|r| r.kind == Kind::Pf && r.name == name).map(|r| (r.rss_kib, r.maps))
}

fn anon_names() -> bool {
    let len = page_size();
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    // SAFETY: a fresh anonymous page, unmapped below.
    let at = unsafe {
        libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
    };
    assert_ne!(at, libc::MAP_FAILED);
    let named = name::name_anon(at as usize, len, Some(c"pf:probe"));
    // SAFETY: mapped above.
    unsafe { libc::munmap(at, len) };
    match named {
        Ok(()) => true,
        Err(MemoryError::Name { errno: libc::EINVAL }) => {
            eprintln!("skipped, the kernel lacks CONFIG_ANON_VMA_NAME");
            false
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn an_anonymous_region_carries_its_pools_name() {
    if !anon_names() {
        return;
    }
    let page = page_size();
    let config =
        PoolConfig { slot_size: 64 << 10, slots: 4, bytes: 0, align: page, allocator: mmap::ANON };
    let pool = Pool::named(config, "anontest");
    let mut a = pool.try_acquire().expect("a slot");
    a.as_mut_full().fill(1);
    let (rss, maps) = resident("pf:pool:anontest:64K").expect("the region by name");
    assert_eq!(maps, 1, "one region");
    assert!(rss >= 64, "the written slot is resident, {rss} KiB");
    drop((a, pool));
    assert_eq!(resident("pf:pool:anontest:64K"), None, "gone with the pool");
}

#[test]
fn a_heap_region_is_named_while_mapped_and_cleared_when_freed() {
    if !anon_names() {
        return;
    }
    name::install();
    let pool = Pool::named(PoolConfig::heap(1 << 20, 4), "heap test");
    let mut a = pool.try_acquire().expect("a slot");
    a.as_mut_full().fill(1);
    let (rss, _) = resident("pf:pool:heap_test:1M").expect("the region's whole pages by name");
    assert!(rss >= 1000, "the written slot is resident, {rss} KiB");
    drop((a, pool));
    assert_eq!(resident("pf:pool:heap_test:1M"), None, "the name goes with the region");
    let small = Pool::named(PoolConfig::heap(64, 4), "smalltest");
    drop(small.try_acquire().expect("a slot"));
    assert_eq!(resident("pf:pool:smalltest:64"), None, "a region of a few bytes stays unnamed");
}

#[test]
fn a_udmabuf_regions_memfd_carries_its_pools_name() {
    let dev = match udmabuf::device() {
        Ok(dev) => dev,
        Err(e) => {
            eprintln!("skipped, {e}");
            return;
        }
    };
    let page = udmabuf::align();
    let config = PoolConfig {
        slot_size: 2 * page,
        slots: 2,
        bytes: 0,
        align: page,
        allocator: dev.allocator(),
    };
    let pool = Pool::named(config, "dmatest");
    let mut a = pool.try_acquire().expect("a slot");
    a.as_mut_full().fill(1);
    let class = format!("pf:udmabuf:dmatest:{}K", 2 * page / 1024);
    let (_, maps) = resident(&class).expect("the memfd by name");
    assert_eq!(maps, 1);
    drop((a, pool));
}
