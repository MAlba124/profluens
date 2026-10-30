//! Robustness, the demuxer recovers at the next valid page after garbage or truncation.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_ogg::{OggReader, demux_all, mux_packets};

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}
fn payload(seed: u64, n: usize) -> Vec<u8> {
    let mut r = Rng(seed);
    (0..n).map(|_| r.next_u64() as u8).collect()
}

fn good_stream() -> (Vec<u8>, Vec<Vec<u8>>) {
    let pkts: Vec<Vec<u8>> = vec![payload(1, 30), payload(2, 255), payload(3, 500), payload(4, 7)];
    let refs: Vec<(&[u8], u64)> =
        pkts.iter().enumerate().map(|(i, p)| (p.as_slice(), i as u64)).collect();
    (mux_packets(1, &refs), pkts)
}

#[test]
fn leading_garbage_is_skipped() {
    let (stream, pkts) = good_stream();
    let mut junk = vec![0u8; 137];
    junk.iter_mut().enumerate().for_each(|(i, b)| *b = (i as u8).wrapping_mul(53) ^ 0x5A);
    junk.retain(|_| true);
    let mut input = junk.clone();
    input.extend_from_slice(&stream);

    let got = demux_all(&input);
    let datas: Vec<Vec<u8>> = got.into_iter().map(|p| p.data).collect();
    assert_eq!(datas, pkts, "packets recovered after leading garbage");
}

#[test]
fn garbage_containing_fake_oggs_still_recovers() {
    let (stream, pkts) = good_stream();
    let mut input = Vec::new();
    input.extend_from_slice(b"....OggS not a real page, bad version and crc....");
    input.extend_from_slice(b"OggS\xFF\xFF\xFF\xFF garbage after a fake capture pattern");
    input.extend_from_slice(&stream);

    let got = demux_all(&input);
    let datas: Vec<Vec<u8>> = got.into_iter().map(|p| p.data).collect();
    assert_eq!(datas, pkts, "recovered past fake OggS markers");
}

#[test]
fn corruption_in_the_middle_drops_one_page_recovers_after() {
    let pkts: Vec<Vec<u8>> = (0..6u32).map(|i| payload(100 + i as u64, 60)).collect();
    let mut w = pf_ogg::OggWriter::new(1);
    let mut stream = Vec::new();
    for (i, p) in pkts.iter().enumerate() {
        if i > 0 {
            w.flush(&mut stream);
        }
        w.write_packet(&mut stream, p, i as u64).unwrap();
    }
    w.finish(&mut stream);

    use pf_ogg::PageHeader;
    let first = PageHeader::parse(&stream).unwrap();
    let second_off = first.len();
    let second = PageHeader::parse(&stream[second_off..]).unwrap();
    let corrupt_at = second_off + second.len() - 1;
    stream[corrupt_at] ^= 0xFF;

    let mut reader = OggReader::new();
    reader.push(&stream);
    reader.finish();
    let mut got = Vec::new();
    while let Some(p) = reader.next_packet() {
        got.push(p.data);
    }

    assert!(reader.resync_count() >= 1, "a resync must have occurred");
    let mut expected: Vec<Vec<u8>> = pkts.clone();
    expected.remove(1); // second page carried packet index 1
    assert_eq!(got, expected, "all packets except the corrupted page's survive");
}

#[test]
fn truncated_final_page_is_dropped_not_fatal() {
    let (stream, pkts) = good_stream();
    let cut = stream.len() - 3; // chop the tail
    let truncated = &stream[..cut];

    let mut reader = OggReader::new();
    reader.push(truncated);
    let leftover = reader.finish();
    let mut got = Vec::new();
    while let Some(p) = reader.next_packet() {
        got.push(p.data);
    }
    assert!(leftover > 0, "truncated tail is reported as leftover");
    assert!(got.len() < pkts.len(), "the truncated page's packet is not emitted");
    assert_eq!(got.as_slice(), &pkts[..got.len()], "recovered packets are a correct prefix");
}

#[test]
fn interior_garbage_between_pages() {
    let pkts: Vec<Vec<u8>> = (0..5u32).map(|i| payload(200 + i as u64, 80)).collect();
    let mut w = pf_ogg::OggWriter::new(3);
    let mut good = Vec::new();
    for (i, p) in pkts.iter().enumerate() {
        if i > 0 {
            w.flush(&mut good);
        }
        w.write_packet(&mut good, p, i as u64).unwrap();
    }
    w.finish(&mut good);

    use pf_ogg::PageHeader;
    let mut pages = Vec::new();
    let mut off = 0;
    while off < good.len() {
        let page = PageHeader::parse(&good[off..]).unwrap();
        pages.push(good[off..off + page.len()].to_vec());
        off += page.len();
    }
    let mut input = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        input.extend_from_slice(page);
        input.extend_from_slice(&[0xDE, 0xAD, i as u8, 0x00, 0x42, 0x13]);
    }

    let got = demux_all(&input);
    let datas: Vec<Vec<u8>> = got.into_iter().map(|p| p.data).collect();
    assert_eq!(datas, pkts, "all packets recovered despite inter-page junk");
}

#[test]
fn never_panics_on_random_input() {
    let mut r = Rng(0xF0F0_1234);
    for _ in 0..2000 {
        let len = (r.next_u64() % 512) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| r.next_u64() as u8).collect();
        if len > 8 {
            let at = (r.next_u64() as usize) % (len - 4);
            data[at..at + 4].copy_from_slice(b"OggS");
        }
        let _ = demux_all(&data);

        let mut reader = OggReader::new();
        for &b in &data {
            reader.push(&[b]);
            while reader.next_packet().is_some() {}
        }
        reader.finish();
        while reader.next_packet().is_some() {}
    }
}
