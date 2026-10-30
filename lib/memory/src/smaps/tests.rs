use super::*;

const SAMPLE: &str = include_str!("sample.txt");

fn row<'a>(r: &'a Report<'a>, kind: Kind, name: &str) -> Option<&'a Row<'a>> {
    r.rows.iter().find(|row| row.kind == kind && row.name == name)
}

#[test]
fn paths_are_classed_by_their_names() {
    assert_eq!(classify(""), (Kind::Unnamed, "[anon]"));
    assert_eq!(classify("[anon:pf:pool:mkvdemux:4M]"), (Kind::Pf, "pf:pool:mkvdemux:4M"));
    assert_eq!(classify("[anon_shmem:pf:ring:0]"), (Kind::Pf, "pf:ring:0"));
    assert_eq!(classify("[anon:scudo:primary]"), (Kind::Named, "scudo:primary"));
    assert_eq!(
        classify("/memfd:pf:udmabuf:av1dec:3M (deleted)"),
        (Kind::Pf, "pf:udmabuf:av1dec:3M")
    );
    assert_eq!(classify("/memfd:wayland-shm (deleted)"), (Kind::Memfd, "wayland-shm"));
    assert_eq!(classify("/usr/lib/libLLVM.so.19.1"), (Kind::Lib, "libLLVM.so.19.1"));
    assert_eq!(classify("/dev/dri/renderD128"), (Kind::File, "/dev/dri/renderD128"));
    assert_eq!(classify("[stack]"), (Kind::Stack, "[stack]"));
    assert_eq!(classify("[vdso]"), (Kind::Kernel, "[vdso]"));
    assert_eq!(classify("[heap]"), (Kind::Heap, "[heap]"));
}

#[test]
fn resident_bytes_sum_by_kind_and_name() {
    let r = parse(SAMPLE);
    let pool = row(&r, Kind::Pf, "pf:pool:mkvdemux:4M").expect("the demuxer's pool");
    assert_eq!((pool.rss_kib, pool.maps), (32768 + 2048, 2), "two regions under one name");
    assert_eq!(row(&r, Kind::Pf, "pf:udmabuf:av1dec:3M").map(|r| r.rss_kib), Some(6144));
    assert_eq!(
        row(&r, Kind::Lib, "libLLVM.so.19.1").map(|r| (r.rss_kib, r.maps)),
        Some((100000, 2))
    );
    assert_eq!(row(&r, Kind::Unnamed, "[anon]").map(|r| (r.rss_kib, r.maps)), Some((32780, 2)));
    assert_eq!(row(&r, Kind::Heap, "[heap]").map(|r| r.rss_kib), Some(400));
    assert_eq!(row(&r, Kind::Memfd, "wayland-shm").map(|r| r.rss_kib), Some(512));
    assert_eq!(r.rows[0].name, "pf:pool:mkvdemux:4M", "pf rows first, largest first");
    assert!(r.rows.windows(2).all(|w| w[0].kind <= w[1].kind));
    let total: u64 = r.rows.iter().map(|r| r.rss_kib).sum();
    assert_eq!(
        total,
        44 + 400
            + 32768
            + 2048
            + 256
            + 6144
            + 512
            + 32768
            + 12
            + 100
            + 99000
            + 1000
            + 1200
            + 4
            + 8
            + 24
    );
    assert_eq!(r.unnamed.len(), 2);
    assert_eq!(
        r.unnamed[0],
        Unnamed { range: "7fffe4000000-7fffe6000000", size_kib: 32768, rss_kib: 32768 }
    );
}

#[test]
fn the_table_rolls_pf_names_up_by_prefix() {
    assert_eq!(prefix("pf:pool:mkvdemux:4M"), "pf:pool");
    assert_eq!(prefix("pf:udmabuf"), "pf:udmabuf");
    let text = table(&parse(SAMPLE), 10);
    let line = |name: &str| text.lines().find(|l| l.split_whitespace().any(|w| w == name));
    let pools = line("pf:pool:*").expect("the pool rollup");
    assert!(pools.contains("34.2"), "{pools}");
    assert!(line("pf:udmabuf:*").expect("the udmabuf rollup").contains("6.0"));
    assert!(text.lines().last().expect("a total").starts_with("total"), "{text}");
}
