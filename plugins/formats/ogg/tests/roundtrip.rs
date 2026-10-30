//! The correctness gate, mux then demux recovers the exact packets with bos/eos and granule.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_ogg::{OggReader, OggWriter, Packet, demux_all, mux_packets};

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

fn payload(seed: u64, n: usize) -> Vec<u8> {
    let mut r = Rng(seed);
    (0..n).map(|_| r.next_u64() as u8).collect()
}

fn check_roundtrip(serial: u32, packets: &[Vec<u8>]) {
    let refs: Vec<(&[u8], u64)> =
        packets.iter().enumerate().map(|(i, p)| (p.as_slice(), i as u64 + 1)).collect();
    let stream = mux_packets(serial, &refs);
    let got = demux_all(&stream);

    assert_eq!(got.len(), packets.len(), "packet count (serial {serial})");
    for (i, (g, want)) in got.iter().zip(packets).enumerate() {
        assert_eq!(g.serial, serial, "serial on packet {i}");
        assert_eq!(&g.data, want, "packet {i} bytes (len {})", want.len());
    }
    if !packets.is_empty() {
        assert!(got.first().unwrap().bos, "first packet is bos");
        assert!(got.last().unwrap().eos, "last packet is eos");
        for p in &got[1..] {
            assert!(!p.bos, "only the first packet may be bos");
        }
        for p in &got[..got.len() - 1] {
            assert!(!p.eos, "only the last packet may be eos");
        }
    }
}

#[test]
fn lacing_edge_case_sizes() {
    let sizes = [
        0usize, 1, 2, 100, 254, 255, 256, 509, 510, 511, 764, 765, 766, 65025, 65026, 70_000,
        200_000,
    ];
    for (i, &sz) in sizes.iter().enumerate() {
        check_roundtrip(1000 + i as u32, &[payload(sz as u64, sz)]);
    }
}

#[test]
fn multiples_of_255_need_trailing_zero() {
    for k in 0..8u32 {
        let sz = (k as usize) * 255;
        let pkt = payload(0xA5A5 + k as u64, sz);
        let got = demux_all(&mux_packets(1, &[(&pkt, 42)]));
        assert_eq!(got.len(), 1, "k={k}");
        assert_eq!(got[0].data.len(), sz, "recovered length k={k}");
        assert_eq!(got[0].data, pkt, "recovered bytes k={k}");
    }
}

#[test]
fn many_small_packets_filling_and_overflowing_segment_tables() {
    for count in [1usize, 100, 254, 255, 256, 510, 511, 700] {
        let pkts: Vec<Vec<u8>> = (0..count).map(|i| vec![(i as u8).wrapping_mul(37)]).collect();
        check_roundtrip(5, &pkts);
    }
}

#[test]
fn mixed_random_packet_sets() {
    let mut r = Rng(0xDEAD_BEEF_CAFE);
    for trial in 0..40 {
        let n = 1 + r.below(60);
        let mut pkts = Vec::with_capacity(n);
        for _ in 0..n {
            let size = match r.below(10) {
                0 => 0,                             // nil packet
                1 => 255 * (1 + r.below(4)),        // exact multiple of 255
                2 => 255,                           // exactly one full segment
                3..=4 => 200_000 + r.below(50_000), // huge, multi-page
                _ => r.below(400),                  // the common small case
            };
            pkts.push(payload(trial * 131 + size as u64, size));
        }
        check_roundtrip(0x9000 + trial as u32, &pkts);
    }
}

#[test]
fn single_packet_spanning_many_pages() {
    let pkt = payload(0x1234, 500_000);
    let got = demux_all(&mux_packets(77, &[(&pkt, 999)]));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].data, pkt);
    assert!(got[0].bos && got[0].eos);
    assert_eq!(got[0].granule, 999, "granule lands on the finishing page");
}

#[test]
fn granule_positions_are_reported_per_finishing_page() {
    let pkts: Vec<Vec<u8>> = (0..4).map(|i| payload(i, 66_000)).collect(); // >1 page each
    let refs: Vec<(&[u8], u64)> =
        pkts.iter().enumerate().map(|(i, p)| (p.as_slice(), (i as u64 + 1) * 1000)).collect();
    let got = demux_all(&mux_packets(1, &refs));
    assert_eq!(got.len(), 4);
    for (i, p) in got.iter().enumerate() {
        assert_eq!(p.granule, (i as u64 + 1) * 1000, "granule of packet {i}");
    }
}

#[test]
fn empty_stream_just_eos() {
    let mut w = OggWriter::new(1);
    let mut out = Vec::new();
    w.finish(&mut out);
    assert!(!out.is_empty(), "an eos page is emitted even with no packets");
    let got = demux_all(&out);
    assert_eq!(got.len(), 1, "the nil eos page reads back as one empty packet");
    assert!(got[0].data.is_empty(), "and that packet is zero-length");
    assert!(got[0].bos && got[0].eos, "it is the sole (bos+eos) page");
}

#[test]
fn streaming_reader_matches_oneshot_under_arbitrary_chunking() {
    let pkts: Vec<Vec<u8>> = vec![
        payload(1, 0),
        payload(2, 255),
        payload(3, 40),
        payload(4, 66_000),
        payload(5, 1),
        payload(6, 510),
    ];
    let refs: Vec<(&[u8], u64)> =
        pkts.iter().enumerate().map(|(i, p)| (p.as_slice(), i as u64)).collect();
    let stream = mux_packets(9, &refs);
    let oneshot = demux_all(&stream);

    for chunk in [1usize, 2, 3, 7, 13, 64, 997, 4096] {
        let mut reader = OggReader::new();
        let mut got: Vec<Packet> = Vec::new();
        for part in stream.chunks(chunk) {
            reader.push(part);
            while let Some(p) = reader.next_packet() {
                got.push(p);
            }
        }
        reader.finish();
        while let Some(p) = reader.next_packet() {
            got.push(p);
        }
        assert_eq!(got, oneshot, "chunk size {chunk} must match one-shot");
    }
}
