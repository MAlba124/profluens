//! The demultiplexer on hand-built packets, one behaviour per test.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{
    AF_DISCONTINUITY, AF_RANDOM_ACCESS, Fill, Owned, Ts, anomalies, anomaly_kinds, own,
    pat_section, pattern, pes_bytes, pmt_section, pmts, run, start, units, units_of,
};
use pf_mpegts::{
    AnomalyKind as K, Codec, Config, ConfigError, Demux, Encryption, Limits, UF_CORRUPT,
    UF_DISCONTINUITY, UF_ENCRYPTED, UF_RANDOM_ACCESS, UF_SCRAMBLED, UF_TRUNCATED, anomaly_count,
    anomaly_total, crc32,
};

const VIDEO: u16 = 0x100;
const AUDIO: u16 = 0x101;
const PMT: u16 = 0x1000;

fn av() -> Ts {
    start(&[(0x1B, VIDEO, &[]), (0x0F, AUDIO, &[])], VIDEO)
}

fn limits(change: impl FnOnce(&mut Limits)) -> Config {
    let mut cfg = Config::default();
    change(&mut cfg.limits);
    cfg
}

fn refresh_crc(section: &mut [u8]) {
    let n = section.len() - 4;
    let crc = crc32(&section[..n]);
    section[n..].copy_from_slice(&crc.to_be_bytes());
}

#[test]
fn tables_are_reported_once() {
    let mut ts = av();
    for _ in 0..3 {
        ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
        ts.section(PMT, &pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[]), (0x0F, AUDIO, &[])]));
    }
    ts.tail();
    let (events, stats) = run(Config::default(), &ts.out);
    assert_eq!(events.len(), 2);
    let Owned::Pat { transport_stream_id, version, programs } = &events[0] else {
        panic!("PAT first")
    };
    assert_eq!((*transport_stream_id, *version, programs.len()), (1, 0, 1));
    assert_eq!((programs[0].number, programs[0].pmt_pid, programs[0].mapped), (1, PMT, false));
    let Owned::Pmt { program, streams } = &events[1] else { panic!("PMT second") };
    assert_eq!(
        (program.number, program.pcr_pid, program.version, program.mapped),
        (1, VIDEO, 0, true)
    );
    assert_eq!(
        streams.iter().map(|s| (s.pid, s.codec)).collect::<Vec<_>>(),
        [(VIDEO, Codec::H264), (AUDIO, Codec::AacAdts)]
    );
    assert_eq!(stats.sections, 8);
    assert_eq!(anomaly_total(&stats), 0);
}

#[test]
fn bounded_unit_is_delivered_when_its_last_byte_arrives() {
    let mut ts = av();
    let body = pattern(1, 500);
    let at = ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(9000), None, &body, true));
    ts.packet(VIDEO, false, 0, Some(27_000_000), &[], Fill::Adaptation);
    ts.tail();
    let (events, stats) = run(Config::default(), &ts.out);
    assert_eq!(anomalies(&events), []);
    let Owned::Pes(u) = &events[2] else { panic!("unit before the PCR, got {:?}", events[2]) };
    assert_eq!((u.pid, u.stream_id, u.flags), (AUDIO, 0xC0, 0));
    assert_eq!((u.pts, u.dts), (Some(9000), None));
    assert_eq!(u.offset, at as u64);
    assert_eq!(u.payload, body);
    assert!(matches!(
        events[3],
        Owned::Pcr { pid: VIDEO, pcr: 27_000_000, discontinuity: false, .. }
    ));
    assert_eq!((stats.pes_units, stats.pes_bytes), (1, 500));
}

#[test]
fn unbounded_unit_ends_at_the_next_start_or_at_flush() {
    let mut ts = av();
    let first = pattern(2, 700);
    let second = pattern(3, 400);
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(6000), Some(3000), &first, false));
    ts.packet(AUDIO, false, 0, Some(1), &[], Fill::Adaptation);
    let at = ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(9000), None, &second, false));
    ts.tail();

    let mut demux = Demux::new(Config::default()).unwrap();
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    let got = units(&events);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].pts, got[0].dts), (Some(6000), Some(3000)));
    assert_eq!(got[0].payload, first);
    demux.flush(&mut |e| events.push(own(e)));
    let got = units(&events);
    assert_eq!(got.len(), 2);
    assert_eq!((got[1].pts, got[1].dts, got[1].flags), (Some(9000), None, 0));
    assert_eq!(got[1].offset, at as u64);
    assert_eq!(got[1].payload, second);
    assert_eq!(anomalies(&events), []);
}

#[test]
fn unit_inside_one_packet() {
    let mut ts = av();
    let a = pattern(4, 20);
    let b = pattern(5, 170);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(100), None, &a, true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(200), None, &b, true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(300), None, &[], true));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].payload, a);
    assert_eq!(got[1].payload, b);
    assert_eq!((got[2].pts, got[2].payload.len()), (Some(300), 0));
    assert_eq!(anomalies(&events), []);
}

#[test]
fn header_split_over_packets() {
    for bounded in [true, false] {
        let mut ts = av();
        let body = pattern(6, 300);
        let pes = pes_bytes(0xE0, Some(0x1_0000_0001), Some(0x1_0000_0000), &body, bounded);
        let at = ts.packet(VIDEO, true, 0, None, &pes[..5], Fill::Adaptation);
        ts.packet(VIDEO, false, 0, None, &pes[5..12], Fill::Adaptation);
        for chunk in pes[12..].chunks(184) {
            ts.packet(VIDEO, false, 0, None, chunk, Fill::Adaptation);
        }
        ts.tail();
        let (events, _) = run(Config::default(), &ts.out);
        assert_eq!(anomalies(&events), []);
        let got = units_of(&events, VIDEO);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].pts, got[0].dts), (Some(0x1_0000_0001), Some(0x1_0000_0000)));
        assert_eq!(got[0].offset, at as u64);
        assert_eq!(got[0].payload, body);
    }
}

#[test]
fn continuity_loss_delivers_the_prefix_and_marks_the_next_unit() {
    let mut ts = av();
    let a = pattern(7, 1000);
    let b = pattern(8, 300);
    let c = pattern(9, 100);
    let cc0 = ts.peek_cc(VIDEO);
    let at = ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1000), Some(900), &a, false));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(2000), None, &b, false));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(3000), None, &c, false));
    ts.tail();
    ts.out.drain(at + 2 * 188..at + 3 * 188);

    let (events, stats) = run(Config::default(), &ts.out);
    let a_list = anomalies(&events);
    assert_eq!(a_list.len(), 1);
    assert_eq!((a_list[0].kind, a_list[0].pid), (K::ContinuityLoss, VIDEO));
    let expected = u32::from((cc0 + 2) & 15);
    let found = u32::from((cc0 + 3) & 15);
    assert_eq!(a_list[0].detail, expected << 8 | found);
    assert_eq!(a_list[0].offset, (at + 2 * 188) as u64);
    assert_eq!(anomaly_count(&stats, K::ContinuityLoss), 1);

    let got = units_of(&events, VIDEO);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].flags, UF_CORRUPT);
    assert_eq!(got[0].payload, a[..349]);
    assert_eq!(got[1].flags, UF_DISCONTINUITY);
    assert_eq!(got[1].payload, b);
    assert_eq!(got[2].flags, 0);
    assert_eq!(got[2].payload, c);
}

#[test]
fn duplicate_packet_is_dropped() {
    let mut ts = av();
    let body = pattern(10, 500);
    let at = ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(100), None, &body, true));
    ts.tail();
    let second: Vec<u8> = ts.out[at + 188..at + 376].to_vec();

    let mut twice = ts.out.clone();
    twice.splice(at + 376..at + 376, second.iter().copied());
    let (events, stats) = run(Config::default(), &twice);
    assert_eq!(anomaly_kinds(&events), [K::Duplicate]);
    assert_eq!(anomaly_count(&stats, K::Duplicate), 1);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].flags, &got[0].payload), (0, &body));

    let mut thrice = twice.clone();
    thrice.splice(at + 376..at + 376, second.iter().copied());
    let (events, _) = run(Config::default(), &thrice);
    assert_eq!(anomaly_kinds(&events), [K::Duplicate, K::ContinuityLoss]);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].flags, UF_CORRUPT);
    assert_eq!(got[0].payload, body[..2 * 184 - 14]);
}

#[test]
fn an_equal_counter_with_different_content_is_a_loss() {
    let mut ts = av();
    let a = pattern(30, 100);
    let b = pattern(31, 100);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(1), None, &a, true));
    ts.skip_cc(AUDIO, 15);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &b, true));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::ContinuityLoss]);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 2, "the second unit is not mistaken for a duplicate");
    assert_eq!((got[1].pts, got[1].flags, &got[1].payload), (Some(2), UF_DISCONTINUITY, &b));
}

#[test]
fn leading_garbage_is_skipped() {
    let mut ts = av();
    let body = pattern(11, 400);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(100), None, &body, true));
    ts.tail();
    let mut data = pattern(3, 1000);
    assert!(data.contains(&0x47));
    data.extend_from_slice(&ts.out);

    let (events, stats) = run(Config::default(), &data);
    let a = anomalies(&events);
    assert_eq!(a.len(), 1);
    assert_eq!((a[0].kind, a[0].detail, a[0].offset), (K::SyncSkipped, 1000, 1000));
    assert_eq!(units_of(&events, AUDIO)[0].payload, body);
    assert_eq!(units_of(&events, AUDIO)[0].offset, 1000 + 2 * 188);
    assert_eq!(stats.packets, (ts.out.len() / 188) as u64);
}

#[test]
fn garbage_in_the_middle_is_skipped() {
    let mut ts = av();
    let a = pattern(12, 400);
    let b = pattern(13, 400);
    let c = pattern(14, 400);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(100), None, &a, true));
    let cut = ts.out.len();
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(200), None, &b, true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(300), None, &c, true));
    ts.tail();
    let mut data = ts.out[..cut].to_vec();
    data.extend_from_slice(&pattern(3, 333));
    data.extend_from_slice(&ts.out[cut..]);

    let (events, _) = run(Config::default(), &data);
    let list = anomalies(&events);
    assert_eq!(list.iter().map(|a| a.kind).collect::<Vec<_>>(), [K::SyncLost, K::SyncSkipped]);
    assert_eq!(list[0].offset, cut as u64);
    assert_eq!((list[1].detail, list[1].offset), (333, cut as u64 + 333));
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 3);
    assert_eq!((&got[0].payload, &got[1].payload, &got[2].payload), (&a, &b, &c));
    assert!(got.iter().all(|u| u.flags == 0));
}

#[test]
fn a_broken_sync_byte_costs_one_packet() {
    let mut ts = av();
    ts.tail();
    let a = pattern(15, 100);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(100), None, &a, true));
    let at = ts.out.len();
    ts.tail();
    let b = pattern(16, 100);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(200), None, &b, true));
    ts.tail();
    ts.out[at] = 0x46;

    let (events, stats) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(list.iter().map(|a| a.kind).collect::<Vec<_>>(), [K::SyncLost, K::SyncSkipped]);
    assert_eq!(list[1].detail, 188);
    assert_eq!(units_of(&events, AUDIO).len(), 2);
    assert_eq!(stats.packets, (ts.out.len() / 188 - 1) as u64);
}

#[test]
fn a_partial_last_packet_is_counted() {
    let mut ts = av();
    ts.tail();
    let whole = ts.out.len() as u64;
    ts.out.extend_from_slice(&[0x47; 100]);
    let (events, stats) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].kind, list[0].detail, list[0].offset), (K::SyncSkipped, 100, whole));
    assert_eq!(stats.bytes, whole + 100);
}

#[test]
fn input_without_any_packet() {
    let (events, stats) = run(Config::default(), &pattern(1, 5000));
    let list = anomalies(&events);
    assert_eq!(events.len(), 1);
    assert_eq!((list[0].kind, list[0].detail), (K::SyncSkipped, 5000));
    assert_eq!(stats.packets, 0);
    let (events, _) = run(Config::default(), &[]);
    assert_eq!(events, []);
}

#[test]
fn section_spanning_packets() {
    let rows: Vec<(u8, u16, &[u8])> = (0..60).map(|i| (0x1B, 0x200 + i, &[][..])).collect();
    let section = pmt_section(1, 0, 0x200, &[], &rows);
    assert!(section.len() > 184 && section.len() < 368);
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    ts.section(PMT, &section);
    ts.tail();
    let cfg = limits(|l| l.max_streams_per_program = 64);
    let (events, _) = run(cfg, &ts.out);
    assert_eq!(anomalies(&events), []);
    let (_, streams) = pmts(&events)[0];
    assert_eq!(streams.len(), 60);
    assert!(
        streams
            .iter()
            .enumerate()
            .all(|(i, s)| s.pid == 0x200 + i as u16 && s.codec == Codec::H264)
    );
}

#[test]
fn several_sections_in_one_packet_and_a_pointer_field() {
    let one = pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[])]);
    let two = pmt_section(2, 0, AUDIO, &[], &[(0x0F, AUDIO, &[])]);
    let mut ts = Ts::new();
    ts.section(0, &pat_section(7, 0, &[(1, PMT), (2, PMT)]));
    let mut payload = vec![0u8];
    payload.extend_from_slice(&one);
    payload.extend_from_slice(&two);
    ts.packet(PMT, true, 0, None, &payload, Fill::Payload);

    let rows: Vec<(u8, u16, &[u8])> = (0..40).map(|i| (0x02, 0x300 + i, &[][..])).collect();
    let long = pmt_section(1, 1, 0x300, &[], &rows);
    let two_again = pmt_section(2, 1, AUDIO, &[], &[(0x11, AUDIO, &[])]);
    let mut first = vec![0u8];
    first.extend_from_slice(&long[..183]);
    ts.packet(PMT, true, 0, None, &first, Fill::Payload);
    let mut second = vec![(long.len() - 183) as u8];
    second.extend_from_slice(&long[183..]);
    second.extend_from_slice(&two_again);
    ts.packet(PMT, true, 0, None, &second, Fill::Payload);
    ts.tail();

    let cfg = limits(|l| l.max_streams_per_program = 64);
    let (events, _) = run(cfg, &ts.out);
    assert_eq!(anomalies(&events), []);
    let maps = pmts(&events);
    let seen: Vec<(u16, u8, usize)> =
        maps.iter().map(|(p, s)| (p.number, p.version, s.len())).collect();
    assert_eq!(seen, [(1, 0, 1), (2, 0, 1), (1, 1, 40), (2, 1, 1)]);
    assert_eq!(maps[3].1[0].codec, Codec::AacLatm);
}

#[test]
fn crc_failure_is_counted_and_the_table_ignored() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    let good = pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[]), (0x0F, AUDIO, &[])]);
    let mut bad = good.clone();
    bad[13] ^= 0x04;
    ts.section(PMT, &bad);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(1), None, &pattern(1, 50), true));
    ts.section(PMT, &good);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &pattern(2, 50), true));
    ts.tail();
    let (events, stats) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].kind, list[0].pid, list[0].detail), (K::SectionCrc, PMT, 2));
    assert_eq!(anomaly_count(&stats, K::SectionCrc), 1);
    assert_eq!(pmts(&events).len(), 1);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].pts, Some(2));
}

#[test]
fn pmt_version_change_remaps_the_streams() {
    let mut ts = av();
    let v = pattern(20, 600);
    let cut = 184;
    let pes = pes_bytes(0xE0, Some(10), None, &v, false);
    ts.packet(VIDEO, true, 0, None, &pes[..cut], Fill::Adaptation);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(11), None, &pattern(21, 30), true));

    let next = pmt_section(1, 1, VIDEO, &[], &[(0x1B, VIDEO, &[]), (0x81, 0x102, &[])]);
    ts.section(PMT, &next);
    ts.section(PMT, &next);
    for chunk in pes[cut..].chunks(184) {
        ts.packet(VIDEO, false, 0, None, chunk, Fill::Adaptation);
    }
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(12), None, &pattern(22, 30), true));
    ts.pes(0x102, 0, &pes_bytes(0xBD, Some(13), None, &pattern(23, 30), true));
    ts.tail();

    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomalies(&events), []);
    let maps = pmts(&events);
    assert_eq!(maps.len(), 2);
    assert_eq!((maps[0].0.version, maps[1].0.version), (0, 1));
    assert_eq!(
        maps[1].1.iter().map(|s| (s.pid, s.codec)).collect::<Vec<_>>(),
        [(VIDEO, Codec::H264), (0x102, Codec::Ac3)]
    );
    assert_eq!(units_of(&events, AUDIO).iter().map(|u| u.pts).collect::<Vec<_>>(), [Some(11)]);
    assert_eq!(units_of(&events, 0x102).iter().map(|u| u.pts).collect::<Vec<_>>(), [Some(13)]);
    let video = units_of(&events, VIDEO);
    assert_eq!(video.len(), 1);
    assert_eq!(video[0].payload, v);
}

/// A selected stream a new map moves to another PID is read on there in the same push, the one
/// of its type at its place, and its unit in progress ends at the map. One unselected or told off
/// stays so.
#[test]
fn a_selected_stream_moved_by_a_new_map_is_followed() {
    let rows = [(0x1B, VIDEO, &[][..]), (0x0F, AUDIO, &[][..]), (0x0F, 0x102, &[][..])];
    let mut ts = start(&rows, VIDEO);
    let unit = |pts| pes_bytes(0xE0, Some(pts), None, &pattern(30, 40), true);
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1), None, &pattern(30, 40), false));
    let next = [(0x1B, 0x200, &[][..]), (0x0F, 0x201, &[][..]), (0x0F, 0x202, &[][..])];
    ts.section(PMT, &pmt_section(1, 1, 0x200, &[], &next));
    for pid in [0x200, 0x201, 0x202] {
        ts.pes(pid, 0, &unit(2));
    }
    ts.tail();
    let mut demux = Demux::new(Config { select_all: false, ..Config::default() }).expect("config");
    demux.select(VIDEO, true);
    demux.select(AUDIO, true);
    demux.select(0x202, false);
    let mut events = Vec::new();
    common::run_with(&mut demux, &ts.out, || usize::MAX, &mut events);
    let pts = |pid| units_of(&events, pid).iter().map(|u| u.pts).collect::<Vec<_>>();
    assert_eq!((pts(VIDEO), pts(0x200), pts(0x201)), (vec![Some(1)], vec![Some(2)], vec![Some(2)]));
    assert_eq!(pts(0x202), [], "told off");
    let at = |want: &dyn Fn(&common::Owned) -> bool| events.iter().position(want).expect("found");
    let ended = at(&|e| matches!(e, common::Owned::Pes(u) if u.pid == VIDEO));
    let mapped = at(&|e| matches!(e, common::Owned::Pmt { program, .. } if program.version == 1));
    assert!(ended < mapped, "the unit on the old PID ends at the map");
    // with nothing selected nothing follows
    let mut demux = Demux::new(Config { select_all: false, ..Config::default() }).expect("config");
    let mut events = Vec::new();
    common::run_with(&mut demux, &ts.out, || usize::MAX, &mut events);
    assert!(units(&events).is_empty());
}

#[test]
fn a_changed_table_with_the_same_version_is_still_a_change() {
    let mut ts = av();
    ts.section(PMT, &pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[]), (0x04, AUDIO, &[])]));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    let maps = pmts(&events);
    assert_eq!(maps.len(), 2);
    assert_eq!(maps[1].1[1].codec, Codec::MpegAudio);
}

#[test]
fn pat_change_keeps_surviving_programs_and_releases_the_rest() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(9, 0, &[(0, 0x10), (1, 0x1000), (2, 0x1001)]));
    ts.section(0x1000, &pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[])]));
    ts.section(0x1001, &pmt_section(2, 0, AUDIO, &[], &[(0x0F, AUDIO, &[])]));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1), None, &pattern(1, 40), true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &pattern(2, 40), true));
    ts.section(0, &pat_section(9, 1, &[(2, 0x1001)]));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(3), None, &pattern(3, 40), true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(4), None, &pattern(4, 40), true));
    ts.section(0x1000, &pmt_section(1, 1, VIDEO, &[], &[(0x1B, VIDEO, &[])]));
    ts.tail();

    let mut demux = Demux::new(Config::default()).unwrap();
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    assert_eq!(demux.network_pid(), None);
    assert_eq!(demux.transport_stream_id(), Some(9));
    assert_eq!(demux.pes_pids(), 1);
    let programs = demux.programs();
    assert_eq!(programs.len(), 1);
    assert_eq!((programs[0].number, programs[0].mapped), (2, true));
    assert_eq!(demux.streams(&programs[0]).len(), 1);
    assert_eq!(demux.streams(&programs[0])[0].pid, AUDIO);

    assert_eq!(anomalies(&events), []);
    let pats: Vec<(u8, usize)> = events
        .iter()
        .filter_map(|e| {
            if let Owned::Pat { version, programs, .. } = e {
                Some((*version, programs.len()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(pats, [(0, 2), (1, 1)]);
    assert_eq!(pmts(&events).len(), 2, "the surviving program is not mapped again");
    assert_eq!(units_of(&events, VIDEO).iter().map(|u| u.pts).collect::<Vec<_>>(), [Some(1)]);
    assert_eq!(
        units_of(&events, AUDIO).iter().map(|u| u.pts).collect::<Vec<_>>(),
        [Some(2), Some(4)]
    );
}

#[test]
fn network_pid_is_remembered() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(9, 0, &[(0, 0x10), (1, 0x1000)]));
    ts.tail();
    let mut demux = Demux::new(Config::default()).unwrap();
    demux.push(&ts.out, &mut |_| {});
    assert_eq!(demux.network_pid(), Some(0x10));
    assert_eq!(demux.programs().len(), 1);
}

#[test]
fn pes_larger_than_the_limit_is_dropped() {
    let cfg = limits(|l| {
        l.max_pes_bytes = 1024;
        l.pes_reserve_bytes = 512;
    });
    let mut ts = av();
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1), None, &pattern(1, 3000), false));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(2), None, &pattern(2, 1024), false));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(3), None, &pattern(3, 10), false));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(4), None, &pattern(4, 3000), true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(5), None, &pattern(5, 1024), true));
    ts.tail();

    let mut demux = Demux::new(cfg).unwrap();
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    demux.flush(&mut |e| events.push(own(e)));
    let list = anomalies(&events);
    assert_eq!(
        list.iter().map(|a| (a.kind, a.pid, a.detail)).collect::<Vec<_>>(),
        [(K::PesTooLarge, VIDEO, 1024), (K::PesTooLarge, AUDIO, 3000)]
    );
    assert_eq!(
        units_of(&events, VIDEO).iter().map(|u| (u.pts, u.payload.len())).collect::<Vec<_>>(),
        [(Some(2), 1024), (Some(3), 10)]
    );
    assert_eq!(
        units_of(&events, AUDIO).iter().map(|u| (u.pts, u.payload.len())).collect::<Vec<_>>(),
        [(Some(5), 1024)]
    );
    assert!(demux.pes_buffer_capacity() <= 1024);
}

#[test]
fn a_pat_swapping_every_program_at_the_limit_maps_the_new_ones() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, 0x1000)]));
    ts.section(0x1000, &pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[])]));
    ts.section(0, &pat_section(1, 1, &[(2, 0x1001)]));
    ts.section(0x1001, &pmt_section(2, 0, AUDIO, &[], &[(0x0F, AUDIO, &[])]));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(4), None, &pattern(4, 40), true));
    ts.tail();
    let mut demux = Demux::new(limits(|l| l.max_programs = 1)).unwrap();
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    assert_eq!(pmts(&events).len(), 2);
    assert_eq!(units_of(&events, AUDIO).iter().map(|u| u.pts).collect::<Vec<_>>(), [Some(4)]);
}

#[test]
fn program_limit() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, 0x1000), (2, 0x1001), (3, 0x1002), (4, 0x1003)]));
    ts.section(0x1003, &pmt_section(4, 0, VIDEO, &[], &[(0x1B, VIDEO, &[])]));
    ts.section(0x1001, &pmt_section(2, 0, AUDIO, &[], &[(0x0F, AUDIO, &[])]));
    ts.tail();
    let mut demux = Demux::new(limits(|l| l.max_programs = 2)).unwrap();
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    let list = anomalies(&events);
    assert_eq!(list.iter().map(|a| (a.kind, a.detail)).collect::<Vec<_>>(), [(K::ProgramLimit, 2)]);
    assert_eq!(demux.programs().iter().map(|p| p.number).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(pmts(&events).len(), 1);
    assert_eq!(demux.stats().other_sections, 0);
}

#[test]
fn stream_limit() {
    let rows: Vec<(u8, u16, &[u8])> = (0..5).map(|i| (0x1B, 0x200 + i, &[][..])).collect();
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    ts.section(PMT, &pmt_section(1, 0, 0x200, &[], &rows));
    ts.tail();
    let (events, _) = run(limits(|l| l.max_streams_per_program = 2), &ts.out);
    let list = anomalies(&events);
    assert_eq!(list.iter().map(|a| (a.kind, a.detail)).collect::<Vec<_>>(), [(K::StreamLimit, 3)]);
    assert_eq!(pmts(&events)[0].1.len(), 2);
}

#[test]
fn pes_pid_limit() {
    let mut ts = av();
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1), None, &pattern(1, 40), true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &pattern(2, 40), true));
    ts.tail();
    let mut demux = Demux::new(limits(|l| l.max_pes_pids = 1)).unwrap();
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    let list = anomalies(&events);
    assert_eq!(list.iter().map(|a| (a.kind, a.pid)).collect::<Vec<_>>(), [(K::PesPidLimit, AUDIO)]);
    assert_eq!(units(&events).len(), 1);
    assert_eq!(units(&events)[0].pid, VIDEO);
    assert_eq!(demux.pes_pids(), 1);
    assert_eq!(pmts(&events)[0].1.len(), 2);
}

#[test]
fn section_longer_than_the_limit_is_skipped() {
    let rows: Vec<(u8, u16, &[u8])> = (0..20).map(|i| (0x1B, 0x200 + i, &[][..])).collect();
    let long = pmt_section(1, 0, 0x200, &[], &rows);
    assert_eq!(long.len(), 116);
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    ts.section(PMT, &long);
    ts.section(PMT, &pmt_section(1, 1, 0x200, &[], &rows[..2]));
    ts.tail();
    let (events, _) = run(limits(|l| l.max_section_bytes = 64), &ts.out);
    let list = anomalies(&events);
    assert_eq!(
        list.iter().map(|a| (a.kind, a.pid, a.detail)).collect::<Vec<_>>(),
        [(K::SectionTooLong, PMT, 116)]
    );
    let maps = pmts(&events);
    assert_eq!(maps.len(), 1);
    assert_eq!((maps[0].0.version, maps[0].1.len()), (1, 2));
}

#[test]
fn config_is_validated() {
    let bad = |change: fn(&mut Limits)| Demux::new(limits(change)).err();
    assert_eq!(bad(|l| l.max_programs = 0), Some(ConfigError::Programs));
    assert_eq!(bad(|l| l.max_programs = 1025), Some(ConfigError::Programs));
    assert_eq!(bad(|l| l.max_streams_per_program = 0), Some(ConfigError::StreamsPerProgram));
    assert_eq!(bad(|l| l.max_streams_per_program = 257), Some(ConfigError::StreamsPerProgram));
    assert_eq!(bad(|l| l.max_pes_pids = 0), Some(ConfigError::PesPids));
    assert_eq!(bad(|l| l.max_pes_pids = 8193), Some(ConfigError::PesPids));
    assert_eq!(bad(|l| l.max_pes_bytes = 511), Some(ConfigError::PesBytes));
    assert_eq!(bad(|l| l.max_pes_bytes = (1 << 30) + 1), Some(ConfigError::PesBytes));
    assert_eq!(bad(|l| l.pes_reserve_bytes = l.max_pes_bytes + 1), Some(ConfigError::PesReserve));
    assert_eq!(bad(|l| l.max_section_bytes = 63), Some(ConfigError::SectionBytes));
    assert_eq!(bad(|l| l.max_section_bytes = 4097), Some(ConfigError::SectionBytes));
    assert_eq!(bad(|l| l.sync_confirm = 0), Some(ConfigError::SyncConfirm));
    assert_eq!(bad(|l| l.sync_confirm = 17), Some(ConfigError::SyncConfirm));
    assert!(!ConfigError::PesBytes.to_string().is_empty());
    let widest = limits(|l| {
        l.max_programs = 1024;
        l.max_streams_per_program = 256;
        l.max_pes_pids = 8192;
        l.max_section_bytes = 4096;
        l.sync_confirm = 16;
    });
    assert!(Demux::new(widest).is_ok());
}

#[test]
fn pcr_is_reported_for_the_pcr_pid_only() {
    let mut ts = Ts::new();
    ts.packet(VIDEO, false, 0, Some(5), &[], Fill::Adaptation);
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    ts.section(PMT, &pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[]), (0x0F, AUDIO, &[])]));
    let top = ((1u64 << 33) - 1) * 300 + 299;
    let at = ts.packet(VIDEO, false, 0, Some(top), &[], Fill::Adaptation);
    ts.packet(AUDIO, false, 0, Some(7), &[], Fill::Adaptation);
    ts.packet(VIDEO, false, AF_DISCONTINUITY, Some(0), &[], Fill::Adaptation);
    let pes = pes_bytes(0xE0, Some(1), None, &pattern(1, 20), true);
    ts.packet(VIDEO, true, 0, Some(600), &pes, Fill::Adaptation);
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    let pcrs: Vec<&Owned> = events.iter().filter(|e| matches!(e, Owned::Pcr { .. })).collect();
    assert_eq!(
        pcrs,
        [
            &Owned::Pcr {
                pid: VIDEO,
                pcr: top,
                discontinuity: false,
                offset: at as u64,
                arrival: None
            },
            &Owned::Pcr {
                pid: VIDEO,
                pcr: 0,
                discontinuity: true,
                offset: at as u64 + 376,
                arrival: None
            },
            &Owned::Pcr {
                pid: VIDEO,
                pcr: 600,
                discontinuity: false,
                offset: at as u64 + 564,
                arrival: None
            },
        ]
    );
    assert_eq!(units_of(&events, VIDEO).len(), 1);
    assert_eq!(anomalies(&events), []);
}

#[test]
fn invalid_pcr_extension_is_counted() {
    let mut ts = av();
    let at = ts.packet(VIDEO, false, 0, Some(0), &[], Fill::Adaptation);
    ts.tail();
    ts.out[at + 10] |= 0x01;
    ts.out[at + 11] = 44;
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::PcrInvalid]);
    assert!(!events.iter().any(|e| matches!(e, Owned::Pcr { .. })));
}

#[test]
fn random_access_and_discontinuity_indicators() {
    let mut ts = av();
    ts.pes(VIDEO, AF_RANDOM_ACCESS, &pes_bytes(0xE0, Some(1), None, &pattern(1, 300), true));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(2), None, &pattern(2, 300), true));
    ts.packet(VIDEO, false, AF_RANDOM_ACCESS, None, &[], Fill::Adaptation);
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(3), None, &pattern(3, 300), true));
    ts.skip_cc(VIDEO, 5);
    ts.pes(VIDEO, AF_DISCONTINUITY, &pes_bytes(0xE0, Some(4), None, &pattern(4, 300), true));
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(5), None, &pattern(5, 300), true));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomalies(&events), []);
    let flags: Vec<u8> = units_of(&events, VIDEO).iter().map(|u| u.flags).collect();
    assert_eq!(flags, [UF_RANDOM_ACCESS, 0, UF_RANDOM_ACCESS, UF_DISCONTINUITY, 0]);
}

#[test]
fn scrambled_packets_are_counted_and_skipped() {
    let mut ts = av();
    let at = ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(1), None, &pattern(1, 500), true));
    let clear = ts.out.len();
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &pattern(2, 500), true));
    ts.tail();
    for p in (at..clear).step_by(188) {
        ts.out[p + 3] |= 0x80;
    }
    let (events, stats) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(
        list.iter().map(|a| (a.kind, a.pid, a.detail)).collect::<Vec<_>>(),
        [(K::Scrambled, AUDIO, 2)]
    );
    assert_eq!(stats.scrambled_packets, 3);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].pts, got[0].flags), (Some(2), UF_DISCONTINUITY));
}

#[test]
fn pes_level_scrambling_is_flagged() {
    let mut ts = av();
    let mut pes = pes_bytes(0xC0, Some(1), None, &pattern(1, 50), true);
    pes[6] |= 0x10;
    ts.pes(AUDIO, 0, &pes);
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(units_of(&events, AUDIO)[0].flags, UF_SCRAMBLED);
}

#[test]
fn transport_error_packet_is_dropped() {
    let mut ts = av();
    let body = pattern(1, 500);
    let at = ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(1), None, &body, true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &pattern(2, 50), true));
    ts.tail();
    ts.out[at + 188 + 1] |= 0x80;
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::TransportError, K::ContinuityLoss]);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].flags, got[0].payload.len()), (UF_CORRUPT, 184 - 14));
    assert_eq!((got[1].pts, got[1].flags), (Some(2), UF_DISCONTINUITY));
}

#[test]
fn malformed_packets_are_counted() {
    let mut ts = av();
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(0), None, &pattern(0, 20), true));
    let a = ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(1), None, &pattern(1, 20), true));
    let b = ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &pattern(2, 20), true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(3), None, &pattern(3, 184 - 14), true));
    ts.tail();
    ts.out[a + 3] &= 0x0F;
    ts.out[b + 4] = 200;
    let (events, _) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(
        list.iter().map(|x| (x.kind, x.detail)).collect::<Vec<_>>(),
        [
            (K::AdaptationField, 0),
            (K::AdaptationField, 200),
            (
                K::ContinuityLoss,
                u32::from(ts.peek_cc(AUDIO).wrapping_sub(3) & 15) << 8
                    | u32::from(ts.peek_cc(AUDIO).wrapping_sub(1) & 15)
            ),
        ]
    );
    let got = units_of(&events, AUDIO);
    assert_eq!(
        got.iter().map(|u| (u.pts, u.flags)).collect::<Vec<_>>(),
        [(Some(0), 0), (Some(3), UF_DISCONTINUITY)]
    );
}

#[test]
fn bad_pes_start_is_skipped() {
    let mut ts = av();
    let mut pes = pes_bytes(0xC0, Some(1), None, &pattern(1, 400), true);
    pes[2] = 2;
    ts.pes(AUDIO, 0, &pes);
    let mut pes = pes_bytes(0xC0, Some(2), None, &pattern(2, 40), true);
    pes[6] = 0x40;
    ts.pes(AUDIO, 0, &pes);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(3), None, &pattern(3, 40), true));
    let mut pes = pes_bytes(0xC0, Some(4), None, &pattern(4, 40), true);
    pes[13] &= 0xFE;
    ts.pes(AUDIO, 0, &pes);
    ts.tail();
    let (events, stats) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::PesStartCode, K::PesHeader, K::PesTimestamp]);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.iter().map(|u| u.pts).collect::<Vec<_>>(), [Some(3), None]);
    assert_eq!(stats.orphan_packets, 2);
}

#[test]
fn short_bounded_unit_is_flagged_truncated() {
    let mut ts = av();
    let body = pattern(1, 1000);
    let pes = pes_bytes(0xC0, Some(1), None, &body, true);
    ts.packet(AUDIO, true, 0, None, &pes[..184], Fill::Adaptation);
    ts.packet(AUDIO, false, 0, None, &pes[184..368], Fill::Adaptation);
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &body, true)[..14 + 100]);
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(
        list.iter().map(|a| (a.kind, a.detail)).collect::<Vec<_>>(),
        [(K::PesTruncated, 1000 - 354), (K::PesTruncated, 900)]
    );
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].flags, &got[0].payload[..]), (UF_TRUNCATED, &body[..354]));
    assert_eq!((got[1].flags, &got[1].payload[..]), (UF_TRUNCATED, &body[..100]));
}

#[test]
fn padding_and_headerless_stream_ids() {
    let mut ts = av();
    let mut pad = vec![0, 0, 1, 0xBE, 0, 100];
    pad.extend_from_slice(&[0xFF; 100]);
    ts.pes(AUDIO, 0, &pad);
    let mut raw = vec![0, 0, 1, 0xBF, 0, 8];
    raw.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    ts.pes(AUDIO, 0, &raw);
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomalies(&events), []);
    let got = units_of(&events, AUDIO);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].stream_id, got[0].pts, &got[0].payload[..]),
        (0xBF, None, &[1u8, 2, 3, 4, 5, 6, 7, 8][..])
    );
}

#[test]
fn selection() {
    let mut ts = av();
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1), None, &pattern(1, 40), true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(2), None, &pattern(2, 40), true));
    ts.tail();

    let nothing = Config { select_all: false, ..Config::default() };
    let (events, _) = run(nothing, &ts.out);
    assert_eq!(units(&events).len(), 0);
    assert_eq!(pmts(&events)[0].1.len(), 2);

    let mut demux = Demux::new(nothing).unwrap();
    demux.select(AUDIO, true);
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    assert_eq!(units(&events).iter().map(|u| u.pid).collect::<Vec<_>>(), [AUDIO]);
    assert_eq!(demux.pes_pids(), 1);

    let mut demux = Demux::new(Config::default()).unwrap();
    demux.select(AUDIO, false);
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    assert_eq!(units(&events).iter().map(|u| u.pid).collect::<Vec<_>>(), [VIDEO]);

    demux.select(AUDIO, true);
    let mut more = Ts::new();
    for _ in 0..6 {
        more.null();
    }
    demux.discontinuity();
    demux.push(&more.out, &mut |_| {});
    assert_eq!(demux.pes_pids(), 2);
}

#[test]
fn discontinuity_and_reset() {
    let mut ts = av();
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1), None, &pattern(1, 900), false));
    ts.tail();
    let half = ts.out.len() - 8 * 188 - 2 * 188 - 50;

    let mut demux = Demux::new(Config::default()).unwrap();
    let mut events = Vec::new();
    demux.push(&ts.out[..half], &mut |e| events.push(own(e)));
    assert!(demux.is_locked());
    demux.discontinuity();
    assert!(!demux.is_locked());
    demux.set_position(1_000_000);
    assert_eq!(demux.position(), 1_000_000);
    let mut second = Ts::new();
    second.skip_cc(VIDEO, 9);
    second.section(0, &pat_section(1, 0, &[(1, PMT)]));
    let at = second.pes(VIDEO, 0, &pes_bytes(0xE0, Some(2), None, &pattern(2, 300), true));
    second.pes(VIDEO, 0, &pes_bytes(0xE0, Some(3), None, &pattern(3, 300), true));
    second.tail();
    demux.push(&second.out, &mut |e| events.push(own(e)));
    demux.flush(&mut |e| events.push(own(e)));
    assert_eq!(anomalies(&events), []);
    let got = units_of(&events, VIDEO);
    assert_eq!(
        got.iter().map(|u| (u.pts, u.flags)).collect::<Vec<_>>(),
        [(Some(2), UF_DISCONTINUITY), (Some(3), 0)]
    );
    assert_eq!(got[0].offset, 1_000_000 + at as u64);
    assert_eq!(pmts(&events).len(), 1);

    demux.reset();
    assert_eq!(demux.programs().len(), 0);
    assert_eq!(demux.stats().packets, 0);
    assert_eq!(demux.pes_pids(), 0);
    let mut again = Vec::new();
    demux.push(&ts.out, &mut |e| again.push(own(e)));
    demux.flush(&mut |e| again.push(own(e)));
    let (fresh, _) = run(Config::default(), &ts.out);
    assert_eq!(again, fresh);
}

#[test]
fn pid_misuse_in_tables() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT), (3, 0x0005), (1, 0x1005)]));
    ts.section(
        PMT,
        &pmt_section(
            1,
            0,
            0x1FFF,
            &[],
            &[(0x1B, PMT, &[]), (0x0F, 0x1FFF, &[]), (0x0F, AUDIO, &[])],
        ),
    );
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(1), None, &pattern(1, 40), true));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(
        anomaly_kinds(&events),
        [K::PsiMalformed, K::PsiMalformed, K::PsiMalformed, K::PidConflict]
    );
    let (program, streams) = pmts(&events)[0];
    assert_eq!(program.pcr_pid, 0x1FFF);
    assert_eq!(streams.iter().map(|s| s.pid).collect::<Vec<_>>(), [PMT, AUDIO]);
    assert_eq!(units(&events).len(), 1);
}

#[test]
fn tables_that_do_not_apply_are_ignored() {
    let rows: [(u8, u16, &[u8]); 1] = [(0x1B, VIDEO, &[])];
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    let mut next = pmt_section(1, 5, VIDEO, &[], &rows);
    next[5] &= 0xFE;
    refresh_crc(&mut next);
    ts.section(PMT, &next);
    let mut private = pmt_section(1, 6, VIDEO, &[], &rows);
    private[0] = 0x80;
    refresh_crc(&mut private);
    ts.section(PMT, &private);
    ts.section(PMT, &pmt_section(8, 0, VIDEO, &[], &rows));
    let mut extra = pat_section(1, 0, &[(2, 0x1001)]);
    extra[6] = 1;
    extra[7] = 1;
    refresh_crc(&mut extra);
    ts.section(0, &extra);
    let mut numbered = pmt_section(1, 7, VIDEO, &[], &rows);
    numbered[6] = 1;
    refresh_crc(&mut numbered);
    ts.section(PMT, &numbered);
    ts.tail();
    let (events, stats) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::PatSectionIgnored, K::PsiMalformed]);
    assert_eq!(pmts(&events).len(), 0);
    assert_eq!(stats.other_sections, 2);
}

#[test]
fn pointer_past_the_packet_is_counted() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    let mut payload = vec![200u8];
    payload.extend_from_slice(&pmt_section(1, 0, VIDEO, &[], &[(0x1B, VIDEO, &[])]));
    ts.packet(PMT, true, 0, None, &payload, Fill::Payload);
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(
        list.iter().map(|a| (a.kind, a.pid, a.detail)).collect::<Vec<_>>(),
        [(K::PsiPointer, PMT, 200)]
    );
}

#[test]
fn identification_through_the_demultiplexer() {
    let id3 =
        [0x26, 0x0D, 0xFF, 0xFF, b'I', b'D', b'3', b' ', 0xFF, b'I', b'D', b'3', b' ', 0x00, 0x0F];
    let rows: [(u8, u16, &[u8]); 9] = [
        (0x24, 0x200, &[]),
        (0x06, 0x201, &[0x6A, 0x01, 0x00, 0x0A, 0x04, b'e', b'n', b'g', 0x00]),
        (0x06, 0x202, &[0x7A, 0x01, 0x00]),
        (0x06, 0x203, &[0x59, 0x08, b's', b'w', b'e', 0x10, 0x00, 0x01, 0x00, 0x02]),
        (0x06, 0x204, &[0x56, 0x05, b'f', b'i', b'n', 0x09, 0x00]),
        (0x15, 0x205, &id3),
        (0x11, 0x206, &[]),
        (0xEA, 0x207, &[0x05, 0x04, b'V', b'C', b'-', b'1']),
        (0x05, 0x208, &[]),
    ];
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    ts.section(PMT, &pmt_section(1, 0, 0x200, &[0x05, 0x04, b'T', b'E', b'S', b'T'], &rows));
    ts.pes(0x207, 0, &pes_bytes(0xFD, Some(1), None, &pattern(1, 40), true));
    ts.pes(0x208, 0, &pes_bytes(0xBD, Some(2), None, &pattern(2, 40), true));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomalies(&events), []);
    let (program, streams) = pmts(&events)[0];
    assert_eq!(program.registration, u32::from_be_bytes(*b"TEST"));
    assert_eq!(
        streams.iter().map(|s| s.codec).collect::<Vec<_>>(),
        [
            Codec::Hevc,
            Codec::Ac3,
            Codec::Eac3,
            Codec::DvbSubtitle,
            Codec::Teletext,
            Codec::Id3,
            Codec::AacLatm,
            Codec::Unknown,
            Codec::Unknown,
        ]
    );
    assert_eq!(pf_mpegts::language_str(&streams[1]), Some("eng"));
    assert_eq!(pf_mpegts::language_str(&streams[3]), Some("swe"));
    assert_eq!((streams[3].composition_page, streams[3].ancillary_page), (1, 2));
    assert_eq!(
        (streams[7].stream_type, streams[7].registration),
        (0xEA, u32::from_be_bytes(*b"VC-1"))
    );
    assert_eq!(units(&events).iter().map(|u| u.pid).collect::<Vec<_>>(), [0x207]);
}

#[test]
fn truncated_descriptor_loop_is_counted() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    ts.section(
        PMT,
        &pmt_section(1, 0, VIDEO, &[], &[(0x0F, AUDIO, &[0x0A, 0x08, b'e', b'n', b'g', 0x00])]),
    );
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::DescriptorTruncated]);
    assert_eq!(pmts(&events)[0].1[0].codec, Codec::AacAdts);
}

#[test]
fn null_packets_and_unknown_pids_are_ignored() {
    let mut ts = av();
    for _ in 0..20 {
        ts.null();
        ts.packet(0x555, true, 0, None, &pattern(1, 184), Fill::Adaptation);
    }
    ts.tail();
    let (events, stats) = run(Config::default(), &ts.out);
    assert_eq!(events.len(), 2);
    assert_eq!(stats.null_packets, 28);
    assert_eq!(stats.packets, 2 + 40 + 8);
}

#[test]
fn single_confirmation_locks_at_once() {
    let mut ts = Ts::new();
    ts.section(0, &pat_section(1, 0, &[(1, PMT)]));
    let (events, _) = run(limits(|l| l.sync_confirm = 1), &ts.out);
    assert!(matches!(events[0], Owned::Pat { .. }));
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::SyncSkipped]);
}

#[test]
fn an_encrypted_stream_is_listed_and_not_reassembled() {
    let aacd = [0x0F, 0x04, b'a', b'a', b'c', b'd'];
    let rows: [(u8, u16, &[u8]); 2] = [(0x1B, VIDEO, &[]), (0xCF, AUDIO, &aacd)];
    let mut ts = start(&rows, VIDEO);
    let picture = pattern(40, 300);
    let frame = pattern(41, 300);
    ts.pes(VIDEO, 0, &pes_bytes(0xE0, Some(1), None, &picture, true));
    ts.pes(AUDIO, 0, &pes_bytes(0xC0, Some(1), None, &frame, true));
    ts.section(PMT, &pmt_section(1, 0, VIDEO, &[], &rows));
    ts.tail();

    let (events, stats) = run(Config::default(), &ts.out);
    let list = anomalies(&events);
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].kind, list[0].pid, list[0].detail), (K::EncryptedStream, AUDIO, 0xCF));
    assert_eq!(anomaly_count(&stats, K::EncryptedStream), 1);
    let streams = pmts(&events)[0].1;
    assert_eq!(
        streams.iter().map(|s| (s.pid, s.codec, s.encryption)).collect::<Vec<_>>(),
        [(VIDEO, Codec::H264, Encryption::None), (AUDIO, Codec::AacAdts, Encryption::SampleAes)]
    );
    let got = units(&events);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].pid, got[0].flags, &got[0].payload), (VIDEO, 0, &picture));

    let mut demux = Demux::new(Config::default()).unwrap();
    demux.select(AUDIO, true);
    let mut events = Vec::new();
    demux.push(&ts.out, &mut |e| events.push(own(e)));
    demux.flush(&mut |e| events.push(own(e)));
    assert_eq!(anomaly_kinds(&events), [K::EncryptedStream]);
    let audio = units_of(&events, AUDIO);
    assert_eq!(audio.len(), 1);
    assert_eq!((audio[0].flags, &audio[0].payload), (UF_ENCRYPTED, &frame));
    assert_eq!(units_of(&events, VIDEO)[0].flags, 0);
}

#[test]
fn a_private_stream_type_in_the_atsc_range_is_not_taken_for_encrypted() {
    let mut ts = start(&[(0x1B, VIDEO, &[]), (0xC2, AUDIO, &[])], VIDEO);
    let body = pattern(42, 100);
    ts.pes(AUDIO, 0, &pes_bytes(0xBD, Some(1), None, &body, true));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomalies(&events), []);
    let streams = pmts(&events)[0].1;
    assert_eq!((streams[1].codec, streams[1].encryption), (Codec::Unknown, Encryption::None));
    let got = units_of(&events, AUDIO);
    assert_eq!((got.len(), got[0].flags, &got[0].payload), (1, 0, &body));

    let ec3d = [0x0F, 0x04, b'e', b'c', b'3', b'd'];
    let rows: [(u8, u16, &[u8]); 2] = [(0x1B, VIDEO, &[]), (0xC2, AUDIO, &ec3d)];
    let mut ts = start(&rows, VIDEO);
    ts.pes(AUDIO, 0, &pes_bytes(0xBD, Some(1), None, &body, true));
    ts.tail();
    let (events, _) = run(Config::default(), &ts.out);
    assert_eq!(anomaly_kinds(&events), [K::EncryptedStream]);
    let streams = pmts(&events)[0].1;
    assert_eq!((streams[1].codec, streams[1].encryption), (Codec::Eac3, Encryption::SampleAes));
    assert_eq!(units(&events).len(), 0);
}
