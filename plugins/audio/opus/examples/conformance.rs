//! RFC 6716 conformance of `LibopusDecoder` against the reference `.dec` of each vector from
//! https://opus-codec.org/testvectors/opus_testvectors.tar.gz. Args `<opus_testvectors dir>`.

#![allow(clippy::disallowed_methods)] // one-shot validation harness, not a hot path

fn main() {
    let dir = std::env::args().nth(1).expect("usage: conformance <opus_testvectors dir>");
    let mut worst = f64::INFINITY;
    for n in 1..=12 {
        let bit = format!("{dir}/testvector{n:02}.bit");
        let dec = format!("{dir}/testvector{n:02}.dec");
        let Ok(bytes) = std::fs::read(&bit) else { continue };
        let Ok(refb) = std::fs::read(&dec) else { continue };
        let packets = split_bit(&bytes);
        let out = decode_all(&packets);
        let reference: Vec<i16> =
            refb.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
        let (snr, off) = best_offset_snr(&reference, &out);
        worst = worst.min(snr);
        println!(
            "vec{n:02}  {:>6} packets  SNR {:>7.2} dB  (offset {off} samples)",
            packets.len(),
            snr
        );
    }
    println!("\nworst-vector SNR: {worst:.2} dB");
    // libopus is the reference, under 20 dB is a decode failure.
    if worst < 20.0 {
        eprintln!("FAIL: a vector decoded below 20 dB, the decoder is not conformant");
        std::process::exit(1);
    }
    println!("PASS");
}

/// Each packet is a 4 byte BE length, a 4 byte BE final range, then the bytes.
fn split_bit(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut i = 0;
    while i + 8 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 8;
        if i + len > data.len() {
            break;
        }
        packets.push(data[i..i + len].to_vec());
        i += len;
    }
    packets
}

fn decode_all(packets: &[Vec<u8>]) -> Vec<i16> {
    let mut dec = pf_opus::libopus::LibopusDecoder::new();
    dec.set_channels(2);
    let mut out = Vec::new();
    let mut pcm = Vec::new(); // reused across packets
    for p in packets {
        if let Ok(channels) = dec.decode_packet_into(p, &mut pcm) {
            if channels == 1 {
                for &s in &pcm {
                    out.push(s);
                    out.push(s); // upmix mono to stereo as the reference is
                }
            } else {
                out.extend_from_slice(&pcm);
            }
        }
    }
    out
}

fn best_offset_snr(reference: &[i16], test: &[i16]) -> (f64, usize) {
    const WIN: usize = 96_000; // 1 s of stereo @ 48 kHz
    let mut best_off = 0;
    let mut best_win = f64::NEG_INFINITY;
    for off in (0..9600).step_by(2) {
        if off + WIN >= test.len() || WIN >= reference.len() {
            break;
        }
        let snr = snr_at(&reference[..WIN], &test[off..off + WIN]);
        if snr > best_win {
            best_win = snr;
            best_off = off;
        }
    }
    if best_off >= test.len() {
        return (f64::NEG_INFINITY, 0);
    }
    let t = &test[best_off..];
    let n = reference.len().min(t.len());
    (snr_at(&reference[..n], &t[..n]), best_off)
}

fn snr_at(reference: &[i16], test: &[i16]) -> f64 {
    let n = reference.len().min(test.len());
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for i in 0..n {
        let r = reference[i] as f64;
        let e = r - test[i] as f64;
        sig += r * r;
        err += e * e;
    }
    if err <= 0.0 { 200.0 } else { 10.0 * (sig / err).log10() }
}
