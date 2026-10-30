// Test code. The allocation ban is for element hot paths.
#![allow(clippy::disallowed_methods)]

use pf_av1::Av1Dec;
use pf_harness::Harness;
use pf_hdr::bits::BitWriter;
use pf_hdr::dovi::RpuForm;
use pf_hdr::side::FrameMeta;
use profluens_core::time::Timestamp;

fn ivf_units(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 32;
    while at + 12 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 12;
        out.push(bytes[at..at + n].to_vec());
        at += n;
    }
    out
}

fn t35_obu(t35: &[u8]) -> Vec<u8> {
    let mut payload = vec![4u8];
    payload.extend_from_slice(t35);
    payload.push(0x80);
    let mut obu = vec![5 << 3 | 0x02];
    let mut n = payload.len();
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        obu.push(if n > 0 { b | 0x80 } else { b });
        if n == 0 {
            break;
        }
    }
    obu.extend(payload);
    obu
}

fn hdr10plus(maxscl: u32) -> Vec<u8> {
    let mut m = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
    m.extend(pf_hdr::hdr10plus::sample_payload([maxscl, 1, 1], 1000));
    m
}

fn dovi_t35() -> Vec<u8> {
    let mut rpu = BitWriter::default();
    rpu.put(2, 6)
        .put(18, 11)
        .put(1, 4)
        .put(0, 4)
        .put(1, 1)
        .put(0, 1)
        .put(0, 2)
        .ue(23)
        .put(1, 2)
        .put(0, 1);
    rpu.ue(2).ue(2).ue(4).put(0, 1).put(0, 3).put(0, 1).put(1, 1).put(0, 9);
    let rpu = rpu.into_bytes();
    let mut w = BitWriter::default();
    w.put(0x01be_6841, 27).put(rpu.len() as u32, 8).put(0, 1);
    for b in &rpu {
        w.put(u32::from(*b), 8);
    }
    w.put(0x400, 17);
    let mut m = vec![0xB5, 0x00, 0x3B, 0x00, 0x00, 0x08, 0x00];
    m.extend(w.into_bytes());
    m
}

#[test]
fn t35_metadata_obus_ride_each_shown_picture() {
    let ivf =
        std::fs::read(format!("{}/tests/fixtures/inter_64x48.ivf", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture");
    let mut h = Harness::with_slot_size(Av1Dec::new(), 64 * 48 * 2);
    let mut out = Vec::new();
    let units = ivf_units(&ivf);
    for (k, tu) in units.iter().enumerate() {
        let mut unit = tu[..2].to_vec();
        unit.extend(t35_obu(&hdr10plus(1000 * (k as u32 + 1))));
        unit.extend(t35_obu(&dovi_t35()));
        unit.extend_from_slice(&tu[2..]);
        let mut b = h.alloc(&unit);
        b.pts = Timestamp::from_millis(k as u64 * 40);
        h.push(b).expect("decodes");
        out.extend(h.drain_outputs());
    }
    out.extend(h.eos().expect("eos"));
    assert_eq!(out.len(), units.len());
    for b in &out {
        let k = (b.pts.nanos().unwrap() / 40_000_000) as u32;
        let meta = FrameMeta::read(b.memory.side()).expect("valid");
        assert_eq!(
            meta.hdr10plus().unwrap().unwrap().windows[0].maxscl[0],
            1000 * (k + 1),
            "picture {k}"
        );
        let (form, _) = meta.dovi_rpu().expect("the RPU");
        assert_eq!(form, RpuForm::Av1Emdf);
        assert_eq!(meta.dovi_rpu_header().unwrap().unwrap().guess_profile(), Some(8));
    }
}
