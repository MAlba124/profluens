//! 188, 192 and 204 byte packets are told apart, give equal output, and survive damage.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{
    FIXTURES, Fill, Owned, Rng, SIZES, Ts, anomalies, anomaly_kinds, arrival_of_packet, own,
    pattern, pes_bytes, piece, reframe, run, run_with, start, units, units_of,
};
use pf_mpegts::{
    AnomalyKind as K, Config, Demux, Limits, PacketSize, RESYNC_BUDGET, Stats, anomaly_count,
    anomaly_total, framed_len,
};

const VIDEO: u16 = 0x100;
const AUDIO: u16 = 0x101;

fn av() -> Ts {
    start(&[(0x1B, VIDEO, &[]), (0x0F, AUDIO, &[])], VIDEO)
}

fn small_stream() -> Vec<u8> {
    let mut ts = av();
    ts.packet(VIDEO, false, 0, Some(27_000_000), &[], Fill::Adaptation);
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(9000), None, &pattern(1, 500), true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(9000), None, &pattern(2, 500), true));
    ts.tail();
    ts.out
}

fn as_188(events: &[Owned], size: PacketSize) -> Vec<Owned> {
    let n = framed_len(size) as u64;
    let at = |offset: u64| {
        assert_eq!(offset % n, 0, "offset on the packet grid");
        offset / n * 188
    };
    events
        .iter()
        .cloned()
        .map(|e| match e {
            Owned::Pcr { pid, pcr, discontinuity, offset, .. } => {
                Owned::Pcr { pid, pcr, discontinuity, offset: at(offset), arrival: None }
            }
            Owned::Pes(mut u) => {
                u.offset = at(u.offset);
                u.arrival = None;
                Owned::Pes(u)
            }
            Owned::Anomaly(mut a) => {
                a.offset = at(a.offset);
                Owned::Anomaly(a)
            }
            other => other,
        })
        .collect()
}

fn run_pieces(
    data: &[u8],
    next_size: impl FnMut() -> usize,
) -> (Vec<Owned>, Stats, Option<PacketSize>) {
    let mut demux = Demux::new(Config::default()).unwrap();
    let mut events = Vec::new();
    run_with(&mut demux, data, next_size, &mut events);
    (events, *demux.stats(), demux.packet_size())
}

#[test]
fn every_packet_size_gives_the_output_of_the_188_byte_form() {
    for (n, f) in FIXTURES.iter().enumerate() {
        let (plain, plain_stats) = run(Config::default(), f.data);
        assert!(!units(&plain).is_empty(), "{} has units", f.name);
        for size in SIZES {
            let data = reframe(f.data, size);
            let extra = framed_len(size) as u64 - 188;
            let mut rng = Rng::new(n as u64 * 7 + extra);
            for round in 0..4 {
                let (events, stats, found) = match round {
                    0 => run_pieces(&data, || usize::MAX),
                    1 => run_pieces(&data, || 1),
                    _ => run_pieces(&data, || piece(&mut rng)),
                };
                let what = format!("{} as {size:?} round {round}", f.name);
                assert_eq!(found, Some(size), "{what}");
                assert!(as_188(&events, size) == plain, "{what} differs from the 188-byte form");
                assert_eq!(stats.framing_bytes, plain_stats.packets * extra, "{what}");
                assert_eq!(stats.bytes, plain_stats.bytes + plain_stats.packets * extra, "{what}");
                let same = Stats { bytes: plain_stats.bytes, framing_bytes: 0, ..stats };
                assert_eq!(same, plain_stats, "{what}");
            }
        }
    }
}

#[test]
fn arrival_time_stamps_reach_the_events() {
    let plain = small_stream();
    for size in SIZES {
        let (events, _) = run(Config::default(), &reframe(&plain, size));
        assert_eq!(anomalies(&events), [], "{size:?}");
        let n = framed_len(size) as u64;
        let stamp = |offset: u64| {
            (size == PacketSize::M2ts192).then(|| arrival_of_packet((offset / n) as usize))
        };
        let mut pcrs = 0;
        for e in &events {
            match e {
                Owned::Pcr { offset, arrival, .. } => {
                    assert_eq!(*arrival, stamp(*offset), "{size:?} clock reference");
                    pcrs += 1;
                }
                Owned::Pes(u) => assert_eq!(u.arrival, stamp(u.offset), "{size:?} unit"),
                _ => {}
            }
        }
        assert_eq!((pcrs, units(&events).len()), (1, 2));
        let video = units_of(&events, VIDEO)[0];
        assert_eq!(video.offset, 3 * n);
        assert_eq!(video.payload, pattern(1, 500));
    }
}

#[test]
fn the_packet_size_is_reported_once_locked() {
    let plain = small_stream();
    for size in SIZES {
        let data = reframe(&plain, size);
        let mut demux = Demux::new(Config::default()).unwrap();
        assert_eq!(demux.packet_size(), None);
        demux.push(&data[..4 * framed_len(size)], &mut |_| {});
        assert_eq!((demux.is_locked(), demux.packet_size()), (false, None));
        demux.push(&data[4 * framed_len(size)..], &mut |_| {});
        assert_eq!((demux.is_locked(), demux.packet_size()), (true, Some(size)));
        assert_eq!(demux.position(), data.len() as u64);
        demux.flush(&mut |_| {});
        assert_eq!((demux.is_locked(), demux.packet_size()), (false, Some(size)));
        demux.reset();
        assert_eq!(demux.packet_size(), None);
    }
}

#[test]
fn payload_sync_bytes_at_another_stride_do_not_change_the_size() {
    // A unit of only 0x47 also puts a sync byte every 204 bytes, both agree at zero and 188 wins.
    let mut ts = Ts::new();
    ts.pes(0x200, 0, &pes_bytes(0xC0, Some(1), None, &[0x47; 3000], false));
    let lead = ts.out.len();
    ts.out.extend_from_slice(&small_stream());
    for k in 0..5 {
        assert_eq!((ts.out[k * 188], ts.out[k * 204]), (0x47, 0x47));
    }
    let mut demux = Demux::new(Config::default()).unwrap();
    let mut events = Vec::new();
    run_with(&mut demux, &ts.out, || usize::MAX, &mut events);
    assert_eq!(demux.packet_size(), Some(PacketSize::Ts188));
    assert_eq!(anomalies(&events), []);
    assert_eq!(demux.stats().packets, (ts.out.len() / 188) as u64);
    assert_eq!(units_of(&events, VIDEO)[0].offset, (lead + 3 * 188) as u64);

    let mut other = ts.out.clone();
    other[4 * 188] = 0x00;
    let mut demux = Demux::new(Config::default()).unwrap();
    demux.push(&other[..6 * 204], &mut |_| {});
    assert_eq!(demux.packet_size(), Some(PacketSize::Fec204));
}

#[test]
fn a_stream_cut_inside_a_packet_starts_at_the_next_whole_one() {
    let plain = small_stream();
    let packets = (plain.len() / 188) as u64;
    for size in SIZES {
        let n = framed_len(size);
        for cut in [2, 10, n - 1] {
            let data = reframe(&plain, size);
            let (events, stats) = run(Config::default(), &data[cut..]);
            let list = anomalies(&events);
            let what = format!("{size:?} cut at {cut}");
            assert_eq!(list.len(), 1, "{what}");
            let skipped = (n - cut) as u32;
            assert_eq!(
                (list[0].kind, list[0].detail, list[0].offset),
                (K::SyncSkipped, skipped, u64::from(skipped))
            );
            assert_eq!(stats.packets, packets - 1, "{what}");
            assert_eq!(events.len(), 1, "{what}");
        }
    }
}

#[test]
fn a_slow_time_stamp_byte_is_not_taken_for_the_sync_byte() {
    // Packets 30 to 73 of the reframed stream carry 0x47 in the second header byte.
    let f = &FIXTURES[0];
    let data = reframe(f.data, PacketSize::M2ts192);
    assert!((30..74).all(|k| data[k * 192 + 1] == 0x47));
    let from = 40 * 192 + 2;
    let mut demux = Demux::new(Config::default()).unwrap();
    let mut events = Vec::new();
    run_with(&mut demux, &data[from..], || usize::MAX, &mut events);
    let list = anomalies(&events);
    assert_eq!((list[0].kind, list[0].detail, list[0].offset), (K::SyncSkipped, 190, 190));
    assert_eq!(anomaly_count(demux.stats(), K::AdaptationField), 0);
    assert_eq!(anomaly_count(demux.stats(), K::SyncLost), 0);
    assert_eq!(demux.stats().packets, (data.len() / 192 - 41) as u64);
}

fn junk_with_a_188_byte_run() -> Vec<u8> {
    let mut junk = vec![0u8; 1000];
    for k in 0..5 {
        junk[10 + k * 188] = 0x47;
    }
    junk
}

#[test]
fn damage_does_not_change_the_packet_size() {
    let f = &FIXTURES[0];
    let (clean_events, _) = run(Config::default(), f.data);
    for size in [PacketSize::M2ts192, PacketSize::Fec204] {
        let n = framed_len(size);
        let clean = reframe(f.data, size);
        let at = 35 * n;
        let mut data = clean[..at].to_vec();
        data.extend_from_slice(&junk_with_a_188_byte_run());
        data.extend_from_slice(&clean[at..]);

        let mut demux = Demux::new(Config::default()).unwrap();
        let mut events = Vec::new();
        run_with(&mut demux, &data, || usize::MAX, &mut events);
        let list = anomalies(&events);
        assert_eq!(
            list.iter().map(|a| a.kind).collect::<Vec<_>>(),
            [K::SyncLost, K::SyncSkipped],
            "{size:?}"
        );
        assert_eq!(list[0].offset, at as u64);
        assert_eq!((list[1].detail, list[1].offset), (1000, at as u64 + 1000));
        assert_eq!(demux.packet_size(), Some(size));
        assert_eq!(demux.stats().packets, (clean.len() / n) as u64);
        assert_eq!(units(&events).len(), units(&clean_events).len(), "{size:?} loses no unit");
    }
}

#[test]
fn the_same_run_at_the_start_is_a_lock_that_the_stream_then_replaces() {
    let f = &FIXTURES[0];
    for size in [PacketSize::M2ts192, PacketSize::Fec204] {
        let n = framed_len(size);
        let mut data = junk_with_a_188_byte_run();
        data.extend_from_slice(&reframe(f.data, size));
        let mut demux = Demux::new(Config::default()).unwrap();
        let mut events = Vec::new();
        demux.push(&data[..1000], &mut |e| events.push(own(e)));
        assert_eq!(demux.packet_size(), Some(PacketSize::Ts188));
        run_with(&mut demux, &data[1000..], || usize::MAX, &mut events);
        assert_eq!(demux.packet_size(), Some(size));
        let list = anomalies(&events);
        assert_eq!((list[0].kind, list[0].detail), (K::SyncSkipped, 10));
        let changes: Vec<_> = list.iter().filter(|a| a.kind == K::PacketSizeChanged).collect();
        assert_eq!(changes.len(), 1, "{size:?}");
        assert_eq!(changes[0].detail, n as u32);
        let lost = 10 + 5 * 188;
        let first = (lost + RESYNC_BUDGET as usize - 1000).div_ceil(n) * n + 1000;
        assert_eq!(changes[0].offset, first as u64, "{size:?}");
        assert!(!units(&events).is_empty());
    }
}

#[test]
fn a_new_packet_size_is_found_after_the_resync_budget() {
    let head = small_stream();
    let f = &FIXTURES[0];
    for (first, second) in [
        (PacketSize::Ts188, PacketSize::M2ts192),
        (PacketSize::M2ts192, PacketSize::Fec204),
        (PacketSize::M2ts192, PacketSize::Ts188),
        (PacketSize::Fec204, PacketSize::M2ts192),
    ] {
        let what = format!("{first:?} then {second:?}");
        let a = reframe(&head, first);
        let b = reframe(f.data, second);
        let mut data = a.clone();
        data.extend_from_slice(&b);
        let mut demux = Demux::new(Config::default()).unwrap();
        let mut events = Vec::new();
        run_with(&mut demux, &data, || usize::MAX, &mut events);

        let n = framed_len(second);
        let lead = if second == PacketSize::M2ts192 { 4 } else { 0 };
        let budget = RESYNC_BUDGET as usize;
        let skipped = (budget - lead).div_ceil(n) * n;
        let lost = a.len() as u64;
        let list = anomalies(&events);
        let got: Vec<_> = list.iter().map(|a| (a.kind, a.detail, a.offset)).collect();
        assert_eq!(
            got[..4],
            [
                (K::SyncLost, 0, lost),
                (K::SyncSkipped, RESYNC_BUDGET, lost + budget as u64),
                (K::SyncSkipped, (skipped - budget) as u32, lost + skipped as u64),
                (K::PacketSizeChanged, n as u32, lost + skipped as u64),
            ],
            "{what}"
        );
        assert_eq!(anomaly_count(demux.stats(), K::PacketSizeChanged), 1, "{what}");
        assert_eq!(demux.packet_size(), Some(second), "{what}");
        let packets = (a.len() / framed_len(first) + (b.len() - skipped) / n) as u64;
        assert_eq!(demux.stats().packets, packets, "{what}");
        let after = units(&events).iter().filter(|u| u.offset > lost).count();
        assert!(after > 10, "{what} delivers the second stream, {after} units");
    }
}

#[test]
fn a_declared_discontinuity_takes_any_packet_size_at_once() {
    let head = small_stream();
    let f = &FIXTURES[0];
    let (plain, _) = run(Config::default(), f.data);
    for second in [PacketSize::M2ts192, PacketSize::Fec204] {
        let b = reframe(f.data, second);
        let mut demux = Demux::new(Config::default()).unwrap();
        let mut events = Vec::new();
        demux.push(&head, &mut |e| events.push(own(e)));
        assert_eq!(demux.packet_size(), Some(PacketSize::Ts188));
        demux.discontinuity();
        events.clear();
        run_with(&mut demux, &b, || usize::MAX, &mut events);
        let got: Vec<_> = anomalies(&events).iter().map(|a| (a.kind, a.detail, a.offset)).collect();
        assert_eq!(got, [(K::PacketSizeChanged, framed_len(second) as u32, head.len() as u64)]);
        assert_eq!(units(&events).len(), units(&plain).len(), "{second:?} from its first packet");

        demux.reset();
        events.clear();
        run_with(&mut demux, &b, || usize::MAX, &mut events);
        assert_eq!(anomalies(&events), []);
        assert_eq!(demux.packet_size(), Some(second));
    }
}

#[test]
fn a_search_that_finds_nothing_reports_what_it_dropped() {
    let budget = u64::from(RESYNC_BUDGET);
    let junk = vec![0u8; 50_000];
    for pieces in [usize::MAX, 1, 777] {
        let mut demux = Demux::new(Config::default()).unwrap();
        let mut events = Vec::new();
        for chunk in junk.chunks(pieces.min(junk.len())) {
            demux.push(chunk, &mut |e| events.push(own(e)));
        }
        let got: Vec<_> = anomalies(&events).iter().map(|a| (a.kind, a.detail, a.offset)).collect();
        let report = |k: u64| (K::SyncSkipped, RESYNC_BUDGET, k * budget);
        assert_eq!(got, [report(1), report(2), report(3)], "pieces of {pieces}");
        assert!(!demux.is_locked());
        events.clear();
        demux.flush(&mut |e| events.push(own(e)));
        let rest = anomalies(&events);
        assert_eq!(rest.len(), 1);
        assert_eq!(u64::from(rest[0].detail), junk.len() as u64 - 3 * budget);
        assert_eq!(demux.position(), junk.len() as u64);
        assert_eq!(anomaly_total(demux.stats()), 4);
    }
}

#[test]
fn one_confirmation_never_finds_another_size() {
    let cfg = Config { limits: Limits { sync_confirm: 1, ..Limits::default() }, select_all: true };
    let plain = small_stream();
    for size in [PacketSize::M2ts192, PacketSize::Fec204] {
        let mut demux = Demux::new(cfg).unwrap();
        let mut events = Vec::new();
        run_with(&mut demux, &reframe(&plain, size), || usize::MAX, &mut events);
        assert_eq!(demux.packet_size(), Some(PacketSize::Ts188));
        assert_eq!(demux.stats().framing_bytes, 0);
        assert!(!anomaly_kinds(&events).contains(&K::PacketSizeChanged));
    }
}

#[test]
fn a_188_byte_lock_does_not_wait_for_the_larger_sizes() {
    let cfg = Config { limits: Limits { sync_confirm: 16, ..Limits::default() }, select_all: true };
    let mut ts = av();
    for _ in 0..14 {
        ts.null();
    }
    assert_eq!(ts.out.len(), 16 * 188);
    let mut demux = Demux::new(cfg).unwrap();
    let mut events = Vec::new();
    run_with(&mut demux, &ts.out, || 1, &mut events);
    assert_eq!(demux.packet_size(), Some(PacketSize::Ts188));
    assert_eq!((demux.stats().packets, events.len()), (16, 2));
    let (events, stats) = run(cfg, &ts.out[188..]);
    assert_eq!((stats.packets, anomaly_kinds(&events)), (0, vec![K::SyncSkipped]));
}
