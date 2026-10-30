//! Seeded damage, random bytes and hostile tables. No panic, every byte accounted, limits and
//! chunking invariance hold, and units starting after a damaged region match the clean run.

#![allow(clippy::disallowed_methods)] // test fixtures

mod common;

use common::{
    Digest, FIXTURES, Fill, Rng, SIZES, Ts, UnitMark, corrupt_framed, digest_with, pat_section,
    pes_bytes, piece, pmt_section, random_bytes, reframe,
};
use pf_mpegts::{Config, Demux, Limits, PacketSize, framed_len};

const RUNS: u64 = 2000;
const RUNS_FRAMED: u64 = 700;
/// Every table and stream of a fixture shows up within its first 120 packets.
const HEAD: usize = 120 * 188;

fn head(data: &'static [u8], size: PacketSize) -> Vec<u8> {
    reframe(&data[..data.len().min(HEAD)], size)
}

fn salt(size: PacketSize) -> u64 {
    (framed_len(size) as u64 - 188) << 48
}

fn tight() -> Config {
    Config {
        limits: Limits {
            max_programs: 1,
            max_streams_per_program: 1,
            max_pes_pids: 1,
            max_pes_bytes: 2048,
            pes_reserve_bytes: 0,
            max_section_bytes: 64,
            sync_confirm: 2,
        },
        select_all: true,
    }
}

struct Pair {
    whole: Demux,
    pieces: Demux,
}

fn pair(cfg: Config) -> Pair {
    Pair { whole: Demux::new(cfg).unwrap(), pieces: Demux::new(cfg).unwrap() }
}

fn checked(pair: &mut Pair, data: &[u8], rng: &mut Rng, what: &str, in_pieces: bool) -> Digest {
    let Pair { whole: demux, pieces: again } = pair;
    demux.reset();
    again.reset();
    let whole = digest_with(demux, data, || usize::MAX);
    let l = demux.config().limits;

    let stats = *demux.stats();
    assert_eq!(stats.bytes, data.len() as u64, "{what} bytes");
    let accounted = stats.packets * 188 + stats.framing_bytes + whole.skipped;
    assert_eq!(accounted, data.len() as u64, "{what} every byte accounted for");
    assert_eq!(demux.position(), data.len() as u64, "{what} position");

    assert!(whole.largest_unit <= l.max_pes_bytes as usize, "{what} unit size");
    assert!(demux.pes_buffer_capacity() <= l.max_pes_bytes as usize, "{what} buffer size");
    assert!(demux.pes_pids() <= usize::from(l.max_pes_pids), "{what} PES PIDs");
    assert!(demux.programs().len() <= usize::from(l.max_programs), "{what} programs");
    assert!(whole.most_programs <= usize::from(l.max_programs), "{what} programs in an event");
    assert!(whole.most_streams <= usize::from(l.max_streams_per_program), "{what} streams");
    for p in demux.programs() {
        assert!(demux.streams(p).len() <= usize::from(l.max_streams_per_program));
    }

    if in_pieces {
        let pieces = digest_with(again, data, || piece(rng));
        assert!(whole == pieces, "{what} differs in random pieces");
        assert_eq!(*again.stats(), stats, "{what} counters differ in random pieces");
    }
    whole
}

type Mark = (u16, Option<u64>, Option<u64>, usize, u64);

fn tail(units: &[UnitMark], from: i64) -> Vec<Mark> {
    units
        .iter()
        .filter(|u| u.offset as i64 >= from)
        .map(|u| (u.pid, u.pts, u.dts, u.len, u.hash))
        .collect()
}

fn damaged(size: PacketSize, per_fixture: u64) {
    let packet = framed_len(size);
    let mut runs = 0u64;
    let mut recovered_units = 0usize;
    let mut wide = pair(Config::default());
    let mut narrow = pair(tight());
    for (n, f) in FIXTURES.iter().enumerate() {
        let mut clean_rng = Rng::new(n as u64);
        let stream = &head(f.data, size)[..];
        let clean = checked(&mut wide, stream, &mut clean_rng, f.name, true);
        assert_eq!(wide.whole.packet_size(), Some(size), "{} is found as {size:?}", f.name);
        // Damage starts once tables and alignment are known, before that there is nothing to
        // recover.
        let first_unit =
            clean.units.iter().map(|u| u.offset).min().expect("fixture has units") as usize;
        let tables_end = first_unit.max(6 * packet);
        let last = stream.len() * 6 / 10;
        assert!(tables_end < last);

        for seed in 0..per_fixture {
            let mut rng = Rng::new(salt(size) | (n as u64) << 32 | seed);
            let what = format!("{} as {size:?} seed {seed}", f.name);
            let a = tables_end + rng.below(last - tables_end);
            let b = (a + 1 + rng.below(2500)).min(last);
            let data = corrupt_framed(&mut rng, stream, a, b, size);
            runs += 1;

            if seed % 4 == 3 {
                checked(&mut narrow, &data, &mut rng, &what, seed % 8 == 3);
                continue;
            }
            let got = checked(&mut wide, &data, &mut rng, &what, seed % 4 == 0);
            // Covers a packet straddling the region end and one a false sync byte may swallow.
            let margin = 4 * packet;
            let shift = data.len() as i64 - stream.len() as i64;
            let want = tail(&clean.units, (b + margin) as i64);
            let have = tail(&got.units, (b + margin) as i64 + shift);
            assert!(!want.is_empty(), "{what} leaves a tail to check");
            assert_eq!(have, want, "{what} tail after damage at {a}..{b}");
            assert_eq!(wide.whole.packet_size(), Some(size), "{what} packet size");
            recovered_units += want.len();
        }
    }
    println!(
        "{runs} damaged streams as {size:?}, {recovered_units} units checked after the damage"
    );
}

#[test]
fn damaged_fixtures() {
    damaged(PacketSize::Ts188, RUNS);
}

#[test]
fn damaged_fixtures_in_192_byte_packets() {
    damaged(PacketSize::M2ts192, RUNS_FRAMED);
}

#[test]
fn damaged_fixtures_in_204_byte_packets() {
    damaged(PacketSize::Fec204, RUNS_FRAMED);
}

#[test]
fn truncated_fixtures() {
    for size in SIZES {
        truncated(size, if size == PacketSize::Ts188 { 300 } else { 100 });
    }
}

fn truncated(size: PacketSize, per_fixture: u64) {
    let mut runs = 0u64;
    let mut wide = pair(Config::default());
    let mut narrow = pair(tight());
    for (n, f) in FIXTURES.iter().enumerate() {
        let mut clean_rng = Rng::new(n as u64);
        let stream = &head(f.data, size)[..];
        let clean = checked(&mut wide, stream, &mut clean_rng, f.name, true);
        for seed in 0..per_fixture {
            let mut rng = Rng::new(salt(size) | 0x7B00_0000 | (n as u64) << 16 | seed);
            let cut = rng.below(stream.len() + 1);
            let what = format!("{} as {size:?} cut at {cut}", f.name);
            let got = checked(&mut wide, &stream[..cut], &mut rng, &what, true);
            let starts: Vec<u64> = got.units.iter().map(|u| u.offset).collect();
            let clean_starts: Vec<u64> =
                clean.units.iter().map(|u| u.offset).filter(|&o| starts.contains(&o)).collect();
            assert_eq!(
                starts.len(),
                clean_starts.len(),
                "{what} delivers only units of the clean run"
            );
            checked(&mut narrow, &stream[..cut], &mut rng, &what, true);
            runs += 2;
        }
    }
    println!("{runs} truncated streams as {size:?}");
}

#[test]
fn random_bytes_never_panic() {
    let mut total = 0usize;
    let mut pairs = [pair(Config::default()), pair(tight())];
    for seed in 0..600u64 {
        let mut rng = Rng::new(0xBAD_5EED + seed);
        let len = rng.below(30_000);
        let mut data = random_bytes(&mut rng, len);
        if seed % 3 != 0 {
            let grid = framed_len(SIZES[(seed % 7 % 3) as usize]);
            for at in (rng.below(grid)..data.len()).step_by(grid) {
                data[at] = 0x47;
            }
        }
        checked(
            &mut pairs[(seed % 2) as usize],
            &data,
            &mut rng,
            &format!("random seed {seed}"),
            true,
        );
        total += len;
    }
    println!("600 random inputs, {total} bytes");
}

const PIDS: [u16; 10] =
    [0x0000, 0x0005, 0x0010, 0x0020, 0x0021, 0x0100, 0x0101, 0x0102, 0x1000, 0x1FFF];
const TYPES: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x05, 0x06, 0x0F, 0x15, 0x1B, 0x24, 0x81, 0xEA, 0xC1, 0xC2, 0xCF, 0xDB,
];
const TAGS: [u8; 9] = [0x05, 0x0A, 0x26, 0x56, 0x59, 0x6A, 0x7A, 0x52, 0x0F];
const INDICATORS: [&[u8; 4]; 4] = [b"zavc", b"aacd", b"ac3d", b"ec3d"];

fn random_run(rng: &mut Rng, max: usize) -> Vec<u8> {
    let n = rng.below(max);
    random_bytes(rng, n)
}

fn pick<T: Copy>(rng: &mut Rng, from: &[T]) -> T {
    from[rng.below(from.len())]
}

fn descriptors(rng: &mut Rng) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..rng.below(4) {
        let mut body = random_run(rng, 14);
        let tag = pick(rng, &TAGS);
        if tag == 0x0F && rng.below(2) == 0 {
            body = pick(rng, &INDICATORS).to_vec();
        }
        v.push(tag);
        v.push(if rng.below(10) == 0 { rng.next() as u8 } else { body.len() as u8 });
        v.extend_from_slice(&body);
    }
    v
}

fn hostile_stream(rng: &mut Rng) -> Vec<u8> {
    let mut ts = Ts::new();
    for _ in 0..30 + rng.below(70) {
        match rng.below(12) {
            0 | 1 => {
                let rows: Vec<(u16, u16)> =
                    (0..rng.below(7)).map(|_| (rng.below(6) as u16, pick(rng, &PIDS))).collect();
                ts.section(0, &pat_section(rng.below(3) as u16, rng.below(4) as u8, &rows));
            }
            2..=4 => {
                let infos: Vec<Vec<u8>> = (0..rng.below(7)).map(|_| descriptors(rng)).collect();
                let rows: Vec<(u8, u16, &[u8])> = infos
                    .iter()
                    .map(|d| (pick(rng, &TYPES), pick(rng, &PIDS), d.as_slice()))
                    .collect();
                let program_info = descriptors(rng);
                let section = pmt_section(
                    rng.below(6) as u16,
                    rng.below(4) as u8,
                    pick(rng, &PIDS),
                    &program_info,
                    &rows,
                );
                ts.section(pick(rng, &PIDS), &section);
            }
            5 => {
                let mut section = vec![pick(rng, &[0x00u8, 0x02, 0x42]), 0, 0];
                section.extend_from_slice(&[0, 0, 0, 0, 0]);
                section.extend_from_slice(&random_run(rng, 300));
                let length = section.len() - 3 + 4;
                section[1] = 0xB0 | (length >> 8) as u8;
                section[2] = length as u8;
                section[5] |= 0x01;
                let crc = pf_mpegts::crc32(&section);
                section.extend_from_slice(&crc.to_be_bytes());
                ts.section(pick(rng, &PIDS), &section);
            }
            6..=8 => {
                let body = random_run(rng, 1200);
                let pts = (rng.below(3) != 0).then(|| rng.next() & 0x1_FFFF_FFFF);
                let dts = (pts.is_some() && rng.below(2) == 0).then(|| rng.next() & 0x1_FFFF_FFFF);
                let id = pick(rng, &[0xE0u8, 0xC0, 0xBD, 0xBE, 0xBF, 0xFD, 0x00]);
                let mut pes = pes_bytes(id, pts, dts, &body, rng.below(2) == 0);
                if rng.below(6) == 0 {
                    let at = rng.below(pes.len().min(20));
                    pes[at] = rng.next() as u8;
                }
                if rng.below(6) == 0 {
                    pes.truncate(rng.below(pes.len() + 1));
                }
                let flags = pick(rng, &[0u8, 0, 0x40, 0x80, 0xC0]);
                ts.pes(pick(rng, &PIDS), flags, &pes);
            }
            9 => {
                let pcr = rng.next() % ((1 << 33) * 300);
                ts.packet(
                    pick(rng, &PIDS),
                    false,
                    pick(rng, &[0u8, 0x80]),
                    Some(pcr),
                    &[],
                    Fill::Adaptation,
                );
            }
            10 => {
                let mut p = random_bytes(rng, 188);
                p[0] = 0x47;
                if rng.below(2) == 0 {
                    let pid = pick(rng, &PIDS);
                    p[1] = (p[1] & 0x60) | (pid >> 8) as u8;
                    p[2] = pid as u8;
                }
                ts.out.extend_from_slice(&p);
            }
            _ => {
                if rng.below(4) == 0 {
                    let junk = random_run(rng, 400);
                    ts.out.extend_from_slice(&junk);
                } else {
                    ts.skip_cc(pick(rng, &PIDS), 1 + rng.below(3) as u8);
                }
            }
        }
    }
    ts.out
}

fn loosely_framed(data: &[u8], size: PacketSize) -> Vec<u8> {
    let (chunks, rest) = data.as_chunks::<188>();
    let mut out = reframe(chunks.as_flattened(), size);
    out.extend_from_slice(rest);
    out
}

#[test]
fn hostile_but_well_formed_streams_never_panic() {
    let mut bytes = 0usize;
    let mut events = 0u64;
    let loose = Config {
        limits: Limits { sync_confirm: 1, max_pes_pids: 3, ..Limits::default() },
        select_all: true,
    };
    let mut pairs = [pair(Config::default()), pair(tight()), pair(loose)];
    for seed in 0..2400u64 {
        let mut rng = Rng::new(0x5EED_0000 + seed);
        let mut data = hostile_stream(&mut rng);
        if seed % 4 == 3 {
            data = loosely_framed(&data, SIZES[1 + (seed / 4 % 2) as usize]);
        }
        let d = checked(
            &mut pairs[(seed % 3) as usize],
            &data,
            &mut rng,
            &format!("hostile seed {seed}"),
            true,
        );
        bytes += data.len();
        events += d.events;
    }
    println!("2400 hostile streams, {bytes} bytes, {events} events");
    assert!(events > 60_000, "the generator reaches the table and unit paths");
}
