//! H.264 scaling lists through vaapih264dec against the software decoder. The fixture is an
//! x264 encode of testsrc2 with `cqm=jvt` and custom 4x4 and 8x8 intra luma lists, so the SPS
//! carries coded, default and fall-back lists. Flat matrices decode it visibly wrong.
//! Skips without a device.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::VecDeque;

use pf_harness::Harness;
use pf_libav::LibavH264Dec;
use pf_vaapi::VaapiH264Dec;
use profluens_core::buffer::Buffer;
use profluens_core::bus::BusMessage;
use profluens_core::format::Value;
use profluens_core::time::Timestamp;

const CQM: &[u8] = include_bytes!("fixtures/cqm_320x240.h264");
const SLOT: usize = 1 << 20;
const MIN_PSNR_DB: f64 = 50.0;
const FRAME_NS: u64 = 33_333_333;

fn nal_starts(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    for i in 0..data.len().saturating_sub(3) {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push((if i > 0 && data[i - 1] == 0 { i - 1 } else { i }, i + 3));
        }
    }
    starts
}

/// Access units of an Annex B stream, cut at the first slice of each picture.
fn access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut cuts = vec![0];
    let mut open = false;
    let mut head = 0;
    for &(code, at) in &nal_starts(data) {
        let slice = matches!(data[at] & 0x1F, 1 | 5);
        if slice && data[at + 1] & 0x80 != 0 {
            if open {
                cuts.push(if head == usize::MAX { code } else { head });
            }
            open = true;
            head = usize::MAX;
        } else if slice {
            head = usize::MAX;
        } else if head == usize::MAX {
            head = code;
        }
    }
    let ends = cuts.iter().skip(1).copied().chain([data.len()]);
    cuts.iter().zip(ends).map(|(&a, b)| data[a..b].to_vec()).collect()
}

fn announced_size(h: &Harness) -> (usize, usize) {
    let f = h.announced().expect("format announced");
    let int = |name: &str| match f.get(h.vocabulary().field_id(name).expect(name)) {
        Some(Value::Int(n)) => n as usize,
        other => panic!("{name} not an Int: {other:?}"),
    };
    (int("width"), int("height"))
}

fn pixfmt(h: &Harness) -> String {
    let f = h.announced().expect("format announced");
    let vocab = h.vocabulary();
    match f.get(vocab.field_id("pixfmt").expect("pixfmt")) {
        Some(Value::Id(id)) => vocab.value_name(id).expect("named").to_string(),
        other => panic!("pixfmt not an Id: {other:?}"),
    }
}

/// Y, Cb and Cr of a tight nv12 or i420 picture.
fn planes(bytes: &[u8], w: usize, h: usize, nv12: bool) -> [Vec<u8>; 3] {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    assert_eq!(bytes.len(), w * h + 2 * cw * ch, "a tight 4:2:0 picture");
    let y = bytes[..w * h].to_vec();
    let c = &bytes[w * h..];
    if nv12 {
        let cb = c.iter().step_by(2).copied().collect();
        let cr = c.iter().skip(1).step_by(2).copied().collect();
        return [y, cb, cr];
    }
    [y, c[..cw * ch].to_vec(), c[cw * ch..].to_vec()]
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sse: f64 = a.iter().zip(b).map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2)).sum();
    if sse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / (sse / a.len() as f64)).log10()
}

struct Decoded {
    frames: VecDeque<(Timestamp, Vec<u8>)>,
    warnings: Vec<String>,
}

fn take(h: &mut Harness, pad: profluens_core::id::SrcPad, into: &mut Decoded) {
    while let Some(b) = h.pull(pad) {
        into.frames.push_back((b.pts, b.memory.data().to_vec()));
    }
}

fn finish(h: &mut Harness, into: &mut Decoded) {
    let rest: Vec<Buffer> = h.eos().expect("eos");
    into.frames.extend(rest.into_iter().map(|b| (b.pts, b.memory.data().to_vec())));
    for m in h.bus_messages() {
        match m {
            BusMessage::Warning { error, .. } => into.warnings.push(error.to_string()),
            BusMessage::Error { error, .. } => panic!("error: {error}"),
            _ => {}
        }
    }
}

#[test]
fn custom_scaling_lists_match_the_software_decoder() {
    let Some(caps) = pf_vaapi::probe() else {
        return eprintln!("skip: no VA-API device");
    };
    if !caps.supports("h264/annexb") {
        return eprintln!("skip: no H.264 decode on {}", caps.vendor);
    }
    let aus = access_units(CQM);
    assert_eq!(aus.len(), 12, "the fixture's pictures");
    let nal_of = |unit_type: u8| {
        nal_starts(&aus[0])
            .iter()
            .find(|&&(_, at)| aus[0][at] & 0x1F == unit_type)
            .map(|&(_, at)| {
                let nal = &aus[0][at..];
                let end = nal_starts(nal).first().map_or(nal.len(), |&(code, _)| code);
                nal[..end].to_vec()
            })
            .expect("in the first access unit")
    };
    let sps = pf_vaapi::h264parse::parse_sps(&nal_of(7)).expect("sps parses");
    let pps = pf_vaapi::h264parse::parse_pps(&nal_of(8)).expect("pps parses");
    assert!(pps.scaling.is_some(), "x264 codes its lists in the PPS");
    let lists = pf_vaapi::h264parse::resolve_scaling(&sps, &pps);
    assert_ne!(lists.list4x4[0], [16; 16], "a custom 4x4 intra luma list");
    assert_ne!(lists.list8x8[0], [16; 64], "a custom 8x8 intra luma list");
    assert_ne!(lists.list4x4[3], [16; 16], "the JVT 4x4 inter list");

    let mut hw = Harness::with_slot_size(VaapiH264Dec::new(), SLOT);
    let mut sw = Harness::with_slot_size(LibavH264Dec::new(), SLOT);
    hw.start().expect("start vaapih264dec");
    sw.start().expect("start libavh264dec");
    let mut got = Decoded { frames: VecDeque::new(), warnings: Vec::new() };
    let mut want = Decoded { frames: VecDeque::new(), warnings: Vec::new() };
    for (i, au) in aus.iter().enumerate() {
        let pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        let mut b = hw.alloc(au);
        b.pts = pts;
        hw.push(b).expect("push vaapih264dec");
        take(&mut hw, VaapiH264Dec::OUT, &mut got);
        let mut b = sw.alloc(au);
        b.pts = pts;
        sw.push(b).expect("push libavh264dec");
        take(&mut sw, LibavH264Dec::OUT, &mut want);
    }
    finish(&mut hw, &mut got);
    finish(&mut sw, &mut want);
    assert!(got.warnings.is_empty(), "vaapih264dec warned {:?}", got.warnings);
    assert_eq!(got.frames.len(), want.frames.len(), "every picture out of both");
    assert_eq!(got.frames.len(), 12);

    let (w, h) = announced_size(&hw);
    assert_eq!((w, h), announced_size(&sw));
    let (hw_nv12, sw_nv12) = (pixfmt(&hw) == "nv12", pixfmt(&sw) == "nv12");
    let mut worst = [f64::INFINITY; 3];
    for (i, ((pg, g), (pw, wf))) in got.frames.iter().zip(&want.frames).enumerate() {
        assert_eq!(pg, pw, "frame {i} pts");
        let g = planes(g, w, h, hw_nv12);
        let r = planes(wf, w, h, sw_nv12);
        for k in 0..3 {
            let p = psnr(&g[k], &r[k]);
            worst[k] = worst[k].min(p);
            assert!(p >= MIN_PSNR_DB, "frame {i} plane {k} PSNR {p:.2} dB");
        }
    }
    eprintln!("12 frames, worst PSNR Y {:.2} Cb {:.2} Cr {:.2} dB", worst[0], worst[1], worst[2]);
}
