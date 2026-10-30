//! Concurrent multiplexing (grouping, RFC 3533 section 4).

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_ogg::{OggWriter, Packet, demux_all};

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

type PacketSet = Vec<(Vec<u8>, u64)>;

fn split_pages(stream: &[u8]) -> Vec<Vec<u8>> {
    use pf_ogg::PageHeader;
    let mut pages = Vec::new();
    let mut off = 0;
    while off < stream.len() {
        let page = PageHeader::parse(&stream[off..]).expect("valid page in own output");
        let len = page.len();
        pages.push(stream[off..off + len].to_vec());
        off += len;
    }
    pages
}

fn stream_pages(serial: u32, packets: &[(Vec<u8>, u64)]) -> Vec<Vec<u8>> {
    let mut w = OggWriter::new(serial);
    let mut out = Vec::new();
    for (i, (pkt, gran)) in packets.iter().enumerate() {
        if i > 0 {
            w.flush(&mut out); // close the previous packet's page before starting a new one
        }
        w.write_packet(&mut out, pkt, *gran).unwrap();
    }
    w.finish(&mut out); // flushes the last packet's page with eos set
    split_pages(&out)
}

fn expected(serial: u32, packets: &[(Vec<u8>, u64)]) -> Vec<Packet> {
    packets
        .iter()
        .enumerate()
        .map(|(i, (data, gran))| Packet {
            serial,
            granule: *gran,
            bos: i == 0,
            eos: i == packets.len() - 1,
            data: data.clone(),
        })
        .collect()
}

#[test]
fn two_streams_interleaved_demux_independently() {
    let a: PacketSet = vec![
        (payload(0xA1, 40), 10),
        (payload(0xA2, 255), 20),
        (payload(0xA3, 300), 30),
        (payload(0xA4, 5), 40),
    ];
    let b: PacketSet = vec![
        (payload(0xB1, 66_000), 100), // spans multiple pages
        (payload(0xB2, 10), 200),
        (payload(0xB3, 0), 300), // nil packet
    ];

    let a_serial = 0x1111_1111;
    let b_serial = 0x2222_2222;
    let a_pages = stream_pages(a_serial, &a);
    let b_pages = stream_pages(b_serial, &b);

    let mut stream = Vec::new();
    stream.extend_from_slice(&a_pages[0]);
    stream.extend_from_slice(&b_pages[0]);
    let mut ai = 1;
    let mut bi = 1;
    while ai < a_pages.len() || bi < b_pages.len() {
        if ai < a_pages.len() {
            stream.extend_from_slice(&a_pages[ai]);
            ai += 1;
        }
        if bi < b_pages.len() {
            stream.extend_from_slice(&b_pages[bi]);
            bi += 1;
        }
    }

    let got = demux_all(&stream);
    let got_a: Vec<Packet> = got.iter().filter(|p| p.serial == a_serial).cloned().collect();
    let got_b: Vec<Packet> = got.iter().filter(|p| p.serial == b_serial).cloned().collect();

    assert_eq!(got_a, expected(a_serial, &a), "stream A packets");
    assert_eq!(got_b, expected(b_serial, &b), "stream B packets");
}

#[test]
fn three_streams_round_robin() {
    let streams: Vec<(u32, PacketSet)> = vec![
        (10, (0..6).map(|i| (payload(10_000 + i, 50 + i as usize * 7), i)).collect()),
        (20, (0..4).map(|i| (payload(20_000 + i, 255 * (i as usize + 1)), i * 2)).collect()),
        (30, (0..5).map(|i| (payload(30_000 + i, i as usize % 3), i * 3)).collect()),
    ];

    let paged: Vec<(u32, Vec<Vec<u8>>)> =
        streams.iter().map(|(s, pkts)| (*s, stream_pages(*s, pkts))).collect();

    let mut stream = Vec::new();
    for (_s, pages) in &paged {
        stream.extend_from_slice(&pages[0]);
    }
    let mut idx = vec![1usize; paged.len()];
    loop {
        let mut progressed = false;
        for (i, (_s, pages)) in paged.iter().enumerate() {
            if idx[i] < pages.len() {
                stream.extend_from_slice(&pages[idx[i]]);
                idx[i] += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    let got = demux_all(&stream);
    for (serial, pkts) in &streams {
        let got_s: Vec<Packet> = got.iter().filter(|p| p.serial == *serial).cloned().collect();
        assert_eq!(got_s, expected(*serial, pkts), "stream {serial}");
    }
}
