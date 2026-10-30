//! VA-API encoders decoded back by independent software decoders, gated on PSNR.
//! Need a real device, skip without one.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::format::ValueDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::time::Timestamp;

use pf_vaapi::h264parse;

const IN: SinkPad = SinkPad(0);
const OUT: SrcPad = SrcPad(0);

const W: usize = 320;
const H: usize = 240;
const FRAMES: usize = 24;
const MIN_PSNR_DB: f64 = 30.0;

fn gradient_frame(t: usize) -> Vec<u8> {
    let (cw, ch) = (W / 2, H / 2);
    let mut v = Vec::with_capacity(W * H + 2 * cw * ch);
    let tri = |p: usize| -> u8 {
        let m = p % 510;
        if m < 255 { m as u8 } else { (510 - m) as u8 }
    };
    for y in 0..H {
        for x in 0..W {
            v.push(tri(x + y + 4 * t));
        }
    }
    for _y in 0..ch {
        for x in 0..cw {
            v.push(tri(2 * x + 3 * t + 60)); // Cb
        }
    }
    for _y in 0..ch {
        for x in 0..cw {
            v.push(tri(2 * x + 2 * t + 180)); // Cr
        }
    }
    v
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

fn encode_all(enc: &mut Harness) -> Vec<Buffer> {
    enc.fix_sink_format(
        IN,
        "video/raw",
        &[
            ("width", ValueDesc::Int(W as i64)),
            ("height", ValueDesc::Int(H as i64)),
            ("pixfmt", ValueDesc::Id("i420")),
            ("fps", ValueDesc::Rat(30, 1)),
        ],
    );
    let mut out = Vec::new();
    for t in 0..FRAMES {
        let mut buf = enc.alloc(&gradient_frame(t));
        buf.pts = Timestamp::from_millis((t as u64) * 33);
        enc.push(buf).expect("encode push");
        while let Some(b) = enc.pull(OUT) {
            out.push(b);
        }
    }
    out.extend(enc.eos().expect("encoder eos"));
    out
}

fn decode_all(dec: &mut Harness, encoded: &[Buffer]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    for b in encoded {
        let mut copy = dec.alloc(b.memory.data());
        copy.pts = b.pts;
        copy.flags = b.flags;
        dec.push(copy).expect("decode push");
        while let Some(f) = dec.pull(OUT) {
            frames.push(f.memory.data().to_vec());
        }
    }
    frames.extend(dec.eos().expect("decoder eos").into_iter().map(|f| f.memory.data().to_vec()));
    for m in dec.bus_messages() {
        use profluens_core::bus::BusMessage;
        match m {
            BusMessage::Warning { error, .. } => eprintln!("decoder warning: {error:?}"),
            BusMessage::Error { error, .. } => eprintln!("decoder error: {error:?}"),
            _ => {}
        }
    }
    frames
}

fn assert_round_trip_quality(decoded: &[Vec<u8>], tag: &str) {
    assert_eq!(decoded.len(), FRAMES, "{tag}: all frames decode");
    let frame_len = W * H + 2 * (W / 2) * (H / 2);
    let mut total = 0.0;
    for (t, got) in decoded.iter().enumerate() {
        assert_eq!(got.len(), frame_len, "{tag}: frame {t} is tight I420");
        let src = gradient_frame(t);
        let p = psnr(&src, got);
        assert!(
            p > MIN_PSNR_DB - 8.0,
            "{tag}: frame {t} PSNR {p:.1} dB collapsed (mean gate is {MIN_PSNR_DB})"
        );
        total += p.min(100.0); // cap inf (identical frames) for a sane mean
    }
    let mean = total / decoded.len() as f64;
    eprintln!("{tag}: mean round-trip PSNR {mean:.1} dB over {FRAMES} frames");
    assert!(mean > MIN_PSNR_DB, "{tag}: mean PSNR {mean:.1} < {MIN_PSNR_DB}");
}

fn keyframe_flags_sane(encoded: &[Buffer], tag: &str) {
    assert!(!encoded.is_empty(), "{tag}: encoder produced output");
    assert!(encoded[0].flags.contains(BufferFlags::KEYFRAME), "{tag}: first frame is a keyframe");
    for (i, b) in encoded.iter().enumerate().skip(1) {
        assert!(b.flags.contains(BufferFlags::DELTA), "{tag}: frame {i} tagged DELTA");
    }
}

#[test]
fn h264_hw_encode_sw_decode_round_trip() {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip h264 round trip: no VA-API device");
        return;
    };
    if !caps.supports_encode("h264/annexb") {
        eprintln!("skip h264 round trip: no H.264 encode entrypoint");
        return;
    }

    let mut enc = Harness::with_slot_size(pf_vaapi::VaapiH264Enc::new(), 512 * 1024);
    let encoded = encode_all(&mut enc);
    keyframe_flags_sane(&encoded, "h264");

    let first = encoded[0].memory.data();
    assert_eq!(&first[..4], &[0, 0, 0, 1], "h264: Annex-B start code");
    let types: Vec<u8> = h264parse::split_nals(first).iter().map(|n| n.unit_type).collect();
    assert!(types.contains(&h264parse::NAL_SPS), "h264: first AU has SPS, got {types:?}");
    assert!(types.contains(&h264parse::NAL_PPS), "h264: first AU has PPS, got {types:?}");
    assert!(
        types.contains(&h264parse::NAL_SLICE_IDR),
        "h264: first AU has an IDR slice, got {types:?}"
    );

    let mut dec = Harness::with_slot_size(pf_libav::LibavH264Dec::new(), 512 * 1024);
    let decoded = decode_all(&mut dec, &encoded);
    assert_round_trip_quality(&decoded, "h264");
}

#[test]
fn h265_hw_encode_sw_decode_round_trip() {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip h265 round trip: no VA-API device");
        return;
    };
    if !caps.supports_encode("h265/annexb") {
        eprintln!("skip h265 round trip: no HEVC encode entrypoint");
        return;
    }

    // All-intra because the reference decoder rejects real-world HEVC inter tooling.
    let mut enc = Harness::with_slot_size(pf_vaapi::VaapiH265Enc::new().with_gop(1), 512 * 1024);
    let encoded = encode_all(&mut enc);
    assert_eq!(encoded.len(), FRAMES, "h265: one AU per frame");
    for (i, b) in encoded.iter().enumerate() {
        assert!(
            b.flags.contains(BufferFlags::KEYFRAME),
            "h265: all-intra frame {i} tagged KEYFRAME"
        );
    }

    // First AU is VPS + SPS + PPS + IDR_W_RADL (H.265 section 7.3.1.2).
    let first = encoded[0].memory.data();
    assert_eq!(&first[..4], &[0, 0, 0, 1], "h265: Annex-B start code");
    let types: Vec<u8> =
        h264parse::split_nals(first).iter().map(|n| (n.raw(first)[0] >> 1) & 0x3F).collect();
    for (want, name) in [(32u8, "VPS"), (33, "SPS"), (34, "PPS"), (19, "IDR_W_RADL")] {
        assert!(types.contains(&want), "h265: first AU has {name}, got {types:?}");
    }

    let mut dec = Harness::with_slot_size(pf_libav::LibavH265Dec::new(), 512 * 1024);
    let decoded = decode_all(&mut dec, &encoded);
    assert_round_trip_quality(&decoded, "h265");

    let mut enc = Harness::with_slot_size(pf_vaapi::VaapiH265Enc::new(), 512 * 1024);
    let encoded = encode_all(&mut enc);
    keyframe_flags_sane(&encoded, "h265 (default gop)");
    let p_types: Vec<u8> = h264parse::split_nals(encoded[1].memory.data())
        .iter()
        .map(|n| (n.raw(encoded[1].memory.data())[0] >> 1) & 0x3F)
        .collect();
    assert_eq!(p_types, vec![1], "h265: P access unit is a single TRAIL_R NAL");
}

#[test]
fn h264_live_knobs_retarget_and_force_keyframe() {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip live knobs: no VA-API device");
        return;
    };
    if !caps.supports_encode("h264/annexb") {
        eprintln!("skip live knobs: no H.264 encode entrypoint");
        return;
    }

    use profluens_core::event::Event;
    use profluens_core::format::Value;

    let noise_frame = |t: usize| -> Vec<u8> {
        let len = W * H + 2 * (W / 2) * (H / 2);
        let mut v = Vec::with_capacity(len);
        let mut x = (t as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push((x >> 24) as u8);
        }
        v
    };

    let mut enc = Harness::with_slot_size(
        pf_vaapi::VaapiH264Enc::new().with_bitrate(4000).with_gop(200).with_hrd_ms(500),
        512 * 1024,
    );
    enc.fix_sink_format(
        pf_vaapi::VaapiH264Enc::IN,
        "video/raw",
        &[
            ("width", ValueDesc::Int(W as i64)),
            ("height", ValueDesc::Int(H as i64)),
            ("pixfmt", ValueDesc::Id("i420")),
            ("fps", ValueDesc::Rat(30, 1)),
        ],
    );

    let push_frame = |enc: &mut Harness, t: usize, out: &mut Vec<Buffer>| {
        let mut buf = enc.alloc(&noise_frame(t));
        buf.pts = Timestamp::from_millis((t as u64) * 33);
        enc.push(buf).expect("encode push");
        while let Some(b) = enc.pull(pf_vaapi::VaapiH264Enc::OUT) {
            out.push(b);
        }
    };

    let mut out = Vec::new();
    for t in 0..30 {
        push_frame(&mut enc, t, &mut out);
    }
    enc.push_event(Event::PropChanged { name: "bitrate", value: Value::Int(1000) })
        .expect("bitrate prop event");
    for t in 30..60 {
        push_frame(&mut enc, t, &mut out);
    }
    enc.push_event(Event::PropChanged { name: "force-keyframe", value: Value::Int(1) })
        .expect("force-keyframe prop event");
    for t in 60..64 {
        push_frame(&mut enc, t, &mut out);
    }
    out.extend(enc.eos().expect("encoder eos"));
    assert_eq!(out.len(), 64, "all frames encoded");

    // The full 4x drop is unreachable on noise since the controller saturates at QP 51.
    let avg = |bufs: &[Buffer]| -> f64 {
        bufs.iter().map(|b| b.memory.data().len() as f64).sum::<f64>() / bufs.len() as f64
    };
    let before = avg(&out[10..30]);
    let after = avg(&out[35..60]);
    eprintln!("live bitrate: avg frame {before:.0} B @4000kbps -> {after:.0} B @1000kbps");
    assert!(
        after < before / 1.5,
        "frame sizes must track the live bitrate drop (before {before:.0} B, after {after:.0} B)"
    );

    assert!(out[59].flags.contains(BufferFlags::DELTA), "frame 59 is a delta");
    assert!(out[60].flags.contains(BufferFlags::KEYFRAME), "frame 60 is the forced keyframe");
    assert!(out[61].flags.contains(BufferFlags::DELTA), "frame 61 is a delta");
    let types: Vec<u8> =
        h264parse::split_nals(out[60].memory.data()).iter().map(|n| n.unit_type).collect();
    assert!(
        types.contains(&h264parse::NAL_SLICE_IDR),
        "forced keyframe is a real IDR AU, got {types:?}"
    );

    // pf-h264 drops frames on GOPs longer than its DPB window, so decode starts at the IDR.
    let mut dec = Harness::with_slot_size(pf_libav::LibavH264Dec::new(), 512 * 1024);
    let decoded = decode_all(&mut dec, &out[60..]);
    assert_eq!(decoded.len(), 4, "the forced IDR opens a decodable stream from frame 60");
}

#[test]
fn vp8_hw_encode_sw_decode_round_trip() {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip vp8 round trip: no VA-API device");
        return;
    };
    if !caps.supports_encode("vp8") {
        eprintln!("skip vp8 round trip: no VP8 encode entrypoint");
        return;
    }

    let mut enc = Harness::with_slot_size(pf_vaapi::VaapiVp8Enc::new(), 512 * 1024);
    let encoded = encode_all(&mut enc);
    keyframe_flags_sane(&encoded, "vp8");

    // RFC 6386 section 9.1, bit 0 is 0 for a key frame.
    assert_eq!(encoded[0].memory.data()[0] & 1, 0, "vp8: first frame tagged key");
    assert_eq!(encoded[1].memory.data()[0] & 1, 1, "vp8: second frame tagged inter");

    let mut dec = Harness::with_slot_size(pf_libav::LibavVp8Dec::new(), 512 * 1024);
    let decoded = decode_all(&mut dec, &encoded);
    assert_round_trip_quality(&decoded, "vp8");
}
