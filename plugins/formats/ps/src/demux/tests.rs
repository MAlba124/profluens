#![allow(clippy::disallowed_methods)] // test vectors

use super::*;

fn stamp(prefix: u8, v: u64) -> [u8; 5] {
    [
        (prefix << 4) | (((v >> 30) as u8 & 0x07) << 1) | 1,
        (v >> 22) as u8,
        (((v >> 15) as u8) << 1) | 1,
        (v >> 7) as u8,
        ((v as u8) << 1) | 1,
    ]
}

fn pack2(scr: u64) -> Vec<u8> {
    let v = 1u64 << 46
        | (scr >> 30 & 7) << 43
        | 1 << 42
        | (scr >> 15 & 0x7FFF) << 27
        | 1 << 26
        | (scr & 0x7FFF) << 11
        | 1 << 10
        | 1;
    let mut b = vec![0, 0, 1, PACK];
    b.extend_from_slice(&v.to_be_bytes()[2..]);
    b.extend_from_slice(&[0x01, 0x89, 0xC3, 0xF8]);
    b
}

fn pack1(scr: u64) -> Vec<u8> {
    let v = 2u64 << 36 | (scr >> 30 & 7) << 33 | 1 << 32 | (scr >> 15 & 0x7FFF) << 17 | 1 << 16;
    let v = v | (scr & 0x7FFF) << 1 | 1;
    let mut b = vec![0, 0, 1, PACK];
    b.extend_from_slice(&v.to_be_bytes()[3..]);
    b.extend_from_slice(&[0x80, 0x1B, 0x91]);
    b
}

fn pes2(id: u8, pts: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let fields = pts.map_or(Vec::new(), |p| stamp(0b0010, p).to_vec());
    let len = (3 + fields.len() + payload.len()) as u16;
    let mut b = vec![0, 0, 1, id, (len >> 8) as u8, len as u8, 0x81];
    b.push(if pts.is_some() { 0x80 } else { 0 });
    b.push(fields.len() as u8);
    b.extend_from_slice(&fields);
    b.extend_from_slice(payload);
    b
}

fn pes1(id: u8, pts: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let fields = pts.map_or(vec![0x0F], |p| stamp(0b0010, p).to_vec());
    let len = (fields.len() + payload.len()) as u16;
    let mut b = vec![0, 0, 1, id, (len >> 8) as u8, len as u8];
    b.extend_from_slice(&fields);
    b.extend_from_slice(payload);
    b
}

#[derive(Debug, PartialEq, Eq)]
enum Ev {
    Pack(u64, u64, bool),
    Psm(u8),
    Unit(u16, Option<u64>, Vec<u8>, Vec<u8>, u64),
    End,
    Anomaly(AnomalyKind),
}

fn run(chunks: &[&[u8]]) -> (Vec<Ev>, Stats) {
    let mut d = Demux::new();
    let mut evs = Vec::new();
    let mut sink = |e: Event<'_>| {
        evs.push(match e {
            Event::Pack { offset, pack } => Ev::Pack(offset, pack.scr, pack.mpeg2),
            Event::Psm(p) => Ev::Psm(p.types[0xE0]),
            Event::Unit(u) => {
                Ev::Unit(u.key, u.pts, u.header.to_vec(), u.payload.to_vec(), u.offset)
            }
            Event::End { .. } => Ev::End,
            Event::Anomaly(a) => Ev::Anomaly(a.kind),
            Event::SystemHeader(_) => return,
        })
    };
    for c in chunks {
        d.push(c, &mut sink);
    }
    d.flush(&mut sink);
    (evs, d.stats())
}

fn stream() -> Vec<u8> {
    let mut s = pack2(1000);
    s.extend(pes2(0xE0, Some(4000), &[0, 0, 1, 0xB3, 9, 9]));
    s.extend(pes2(PRIVATE_1, Some(4500), &[0x80, 1, 0, 1, 0x0B, 0x77, 5]));
    s.extend(pes2(PADDING, None, &[]));
    s.extend([0, 0, 1, PADDING, 0, 3, 0xFF, 0xFF, 0xFF]);
    s.extend(pack2(2000));
    s.extend(pes2(PRIVATE_1, None, &[0x21, 0, 8]));
    s.extend(pes2(0xC0, Some(5000), &[0xFF, 0xFD, 1]));
    s.extend([0, 0, 1, END]);
    s
}

#[test]
fn a_stream_reads_into_packs_and_units() {
    let s = stream();
    let (evs, stats) = run(&[&s]);
    let pes_at = 14 + 9 + 5 + 6;
    assert_eq!(
        evs,
        [
            Ev::Pack(0, 1000, true),
            Ev::Unit(0xE000, Some(4000), vec![], vec![0, 0, 1, 0xB3, 9, 9], 14),
            Ev::Unit(0xBD80, Some(4500), vec![0x80, 1, 0, 1], vec![0x0B, 0x77, 5], pes_at as u64),
            Ev::Pack(73, 2000, true),
            Ev::Unit(0xBD21, None, vec![0x21], vec![0, 8], 87),
            Ev::Unit(0xC000, Some(5000), vec![], vec![0xFF, 0xFD, 1], 99),
            Ev::End,
        ]
    );
    assert_eq!((stats.packs, stats.units, stats.skipped, stats.anomalies), (2, 4, 2, 0));
    assert_eq!(stats.bytes, s.len() as u64);
}

#[test]
fn any_cut_of_the_input_reads_the_same() {
    let s = stream();
    let (want, _) = run(&[&s]);
    for n in 1..s.len() {
        let chunks: Vec<&[u8]> = s.chunks(n).collect();
        assert_eq!(run(&chunks).0, want, "chunks of {n}");
    }
    for cut in 0..s.len() {
        assert_eq!(run(&[&s[..cut], &s[cut..]]).0, want, "cut at {cut}");
    }
}

#[test]
fn mpeg1_system_streams_read_as_well() {
    let mut s = pack1(0x1_0000_0000);
    s.extend(pes1(0xE0, Some(7), &[1, 2, 3]));
    s.extend(pes1(0xC0, None, &[4]));
    let (evs, _) = run(&[&s]);
    assert_eq!(
        evs,
        [
            Ev::Pack(0, 0x1_0000_0000, false),
            Ev::Unit(0xE000, Some(7), vec![], vec![1, 2, 3], 12),
            Ev::Unit(0xC000, None, vec![], vec![4], 12 + 14),
        ]
    );
}

#[test]
fn junk_is_skipped_to_the_next_start_code() {
    let s = stream();
    let mut dirty = vec![0x47, 0, 0, 1, 0xB3, 0, 0];
    dirty.extend_from_slice(&s[..40]);
    dirty.extend_from_slice(&[0xDE, 0xAD, 0, 0, 1, 0x00, 0xBE]);
    let back = s.windows(4).rposition(|w| w == [0, 0, 1, PACK]).unwrap();
    dirty.extend_from_slice(&s[back..]);
    for n in [1, 3, 7, dirty.len()] {
        let chunks: Vec<&[u8]> = dirty.chunks(n).collect();
        let (evs, stats) = run(&chunks);
        let units = evs.iter().filter(|e| matches!(e, Ev::Unit(..))).count();
        assert_eq!(units, 3, "chunks of {n}: {evs:?}");
        assert!(evs.contains(&Ev::Pack((dirty.len() - (s.len() - back)) as u64, 2000, true)));
        assert!(stats.junk > 0 && evs.contains(&Ev::Anomaly(AnomalyKind::Junk)), "chunks of {n}");
    }
}

#[test]
fn a_length_past_the_next_start_code_resyncs() {
    let mut s = pes2(0xE0, None, &[1, 2, 3, 4, 5, 6, 7, 8]);
    s[5] -= 4;
    s.extend(pes2(0xC0, Some(1), &[9]));
    let (evs, _) = run(&[&s]);
    assert_eq!(evs.first(), Some(&Ev::Anomaly(AnomalyKind::Length)));
    assert!(matches!(evs.last(), Some(Ev::Unit(0xC000, Some(1), ..))), "{evs:?}");
}

#[test]
fn a_cut_item_and_a_bad_header_are_reported() {
    let s = stream();
    let (evs, _) = run(&[&s[..50]]);
    assert_eq!(evs.last(), Some(&Ev::Anomaly(AnomalyKind::Truncated)));
    let mut bad = pes2(0xE0, None, &[1]);
    bad[6] = 0x30;
    let (evs, _) = run(&[&bad]);
    assert_eq!(evs, [Ev::Anomaly(AnomalyKind::PesHeader)]);
}

#[test]
fn a_program_stream_map_is_kept() {
    let mut psm = vec![0, 0, 1, PSM, 0, 0, 0x80, 0xFF, 0, 0, 0, 4, 0x1B, 0xE0, 0, 0];
    let len = (psm.len() - 6 + 4) as u16;
    psm[4..6].copy_from_slice(&len.to_be_bytes());
    let crc = pf_mpegts::crc32(&psm);
    psm.extend_from_slice(&crc.to_be_bytes());
    let (evs, stats) = run(&[&psm]);
    assert_eq!((evs, stats.anomalies), (vec![Ev::Psm(0x1B)], 0));
}

#[test]
fn an_end_that_looks_like_a_start_code_reads_the_same_in_any_cut() {
    // Found by ps_packs, a carried 00 00 FF reported as cut short where the whole input skips it.
    for tail in [&[0, 0, 0xFF][..], &[0, 0], &[0, 0, 1, PACK, 0x80], &[0, 0, 1, 0xE0, 0]] {
        let s = [stream(), tail.to_vec()].concat();
        let (want, _) = run(&[&s]);
        let chunks: Vec<&[u8]> = s.chunks(1).collect();
        assert_eq!(run(&chunks).0, want, "{tail:?}");
    }
}

#[test]
fn a_bad_length_found_in_what_was_carried_at_the_end_reads_the_same() {
    // Found by ps_packs, the end read a carried item whose length the whole input refuses.
    let mut inner = pes2(0xE0, None, &[7; 20]);
    inner[5] -= 4;
    let mut outer = pes2(0xC0, None, &[]);
    outer.extend_from_slice(&inner);
    outer.extend_from_slice(&[9; 9]);
    let len = (outer.len() - 6 - 3) as u16;
    outer[4..6].copy_from_slice(&len.to_be_bytes());
    outer.extend_from_slice(&[5, 5, 5]);
    let (want, _) = run(&[&outer]);
    let chunks: Vec<&[u8]> = outer.chunks(1).collect();
    assert_eq!(run(&chunks).0, want);
    assert_eq!(want.iter().filter(|e| matches!(e, Ev::Anomaly(AnomalyKind::Length))).count(), 2);
}
