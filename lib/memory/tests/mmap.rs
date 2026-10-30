use pf_memory::mmap::{self, HUGE_PAGE, page_size};
use profluens_core::memory::{Memory, Pool, PoolConfig, REGION_SLOTS};

#[test]
fn anonymous_regions_serve_slots_page_aligned() {
    let page = page_size();
    let config =
        PoolConfig { slot_size: 1000, slots: 4, bytes: 4000, align: page, allocator: mmap::ANON };
    let pool = Pool::with_config(config);
    let mut a = pool.try_acquire().expect("a slot");
    a.as_mut_full()[..5].copy_from_slice(b"hello");
    a.set_len(5);
    let b = pool.try_acquire().expect("another");
    assert_eq!(a.data(), b"hello");
    assert_eq!(a.data().as_ptr() as usize % page, 0, "page aligned");
    assert_eq!(b.data().as_ptr() as usize - a.data().as_ptr() as usize, page, "slots a page apart");
    assert_eq!(a.capacity(), 1000);
    let s = pool.stats();
    assert_eq!(
        (s.regions, s.bytes_mapped),
        (1, 4 * page as u64),
        "one region of the cap's four slots"
    );
    drop((a, b));
    for _ in 0..100 {
        drop(pool.try_acquire().expect("reused"));
    }
    assert_eq!(pool.stats().slot_allocations, 1);
    drop(pool);
}

#[test]
fn an_alignment_above_the_page_is_met_by_trimming() {
    let align = 64 * page_size();
    let config = PoolConfig { slot_size: 64, slots: 2, bytes: 128, align, allocator: mmap::ANON };
    let pool = Pool::with_config(config);
    let held: Vec<Memory> = (0..2).map(|_| pool.try_acquire().expect("a slot")).collect();
    for m in &held {
        assert_eq!(m.data().as_ptr() as usize % align, 0);
    }
    drop(held);
    drop(pool);
}

#[test]
fn huge_pages_are_mapped_or_refused_cleanly() {
    let config = PoolConfig {
        slot_size: 4096,
        slots: REGION_SLOTS,
        bytes: 4096 * REGION_SLOTS as u64,
        align: 4096,
        allocator: mmap::HUGE,
    };
    let pool = Pool::with_config(config);
    match pool.try_acquire() {
        Some(m) => {
            assert_eq!(
                m.data().as_ptr() as usize % HUGE_PAGE,
                0,
                "a huge page comes aligned to itself"
            );
            assert!(pool.stats().bytes_mapped >= HUGE_PAGE as u64);
            drop(m);
        }
        None => {
            eprintln!("skip: no huge pages reserved, MAP_HUGETLB refused");
            assert_eq!(pool.stats().bytes_mapped, 0);
        }
    }
    drop(pool);
}
