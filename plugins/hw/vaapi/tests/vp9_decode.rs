//! vaapivp9dec held against the bit exact software vp9dec, plane by plane at a PSNR floor.
//! Fixtures are libvpx-vp9 encodes of testsrc2. Needs a VP9 VA-API device, skips without one.
//! - `features_512x288.ivf`: 24 frames, `-crf 40 -g 12 -aq-mode 1 -tile-columns 1`
//! - `resilient_176x144.ivf`: 12 frames, `-error-resilient 1 -aq-mode 3 -deadline realtime`
//! - `lossless_96x64.ivf`: 6 frames, `-lossless 1`
//! - `p2_160x96.ivf`: 16 frames, `-pix_fmt yuv420p10le -profile:v 2 -crf 30 -g 8`
//! - `svc_1280x720.ivf`: the first 3 superframes of libvpx's vp90-2-22-svc_1280x720_3.ivf, spatial
//!   layers of 320x180, 640x360 and 1280x720 each

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::VecDeque;
use std::fs::File;

use pf_harness::Harness;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::format::Value;
use profluens_core::time::Timestamp;

use pf_libav::LibavVp9Dec as Vp9Dec;
use pf_vaapi::VaapiVp9Dec;

const FEATURES: &[u8] = include_bytes!("fixtures/features_512x288.ivf");
const RESILIENT: &[u8] = include_bytes!("fixtures/resilient_176x144.ivf");
const LOSSLESS: &[u8] = include_bytes!("fixtures/lossless_96x64.ivf");
const PROFILE2: &[u8] = include_bytes!("fixtures/p2_160x96.ivf");
const HIDDEN: &[u8] = include_bytes!("fixtures/alloc_352x288.ivf");
const SVC: &[u8] = include_bytes!("fixtures/svc_1280x720.ivf");

const MIN_PSNR_DB: f64 = 50.0;
const FRAME_NS: u64 = 16_666_667;

/// One VP9 session at a time. Profile 2 beside Profile 0 hangs the iHD CometLake video engine.
fn hw_turn() -> Option<File> {
    let node = File::open(&pf_vaapi::probe()?.device).ok()?;
    node.lock().expect("flock the render node");
    Some(node)
}

fn vp9_device() -> bool {
    match pf_vaapi::probe() {
        Some(caps) if caps.supports("vp9") => true,
        Some(caps) => {
            eprintln!("skip: no VP9 decode on {}", caps.vendor);
            false
        }
        None => {
            eprintln!("skip: no VA-API device");
            false
        }
    }
}

fn profile2_device() -> bool {
    pf_vaapi::probe().is_some_and(|caps| caps.decodes_profile("VP9Profile2"))
}

fn demux_ivf(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    if data.len() < 32 || &data[..4] != b"DKIF" {
        return frames;
    }
    let mut off = usize::from(u16::from_le_bytes([data[6], data[7]]));
    while off + 12 <= data.len() {
        let size =
            u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        let start = off + 12;
        let Some(frame) = data.get(start..start + size) else {
            break;
        };
        frames.push(frame.to_vec());
        off = start + size;
    }
    frames
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

fn psnr(a: &[u16], b: &[u16], peak: f64) -> f64 {
    assert_eq!(a.len(), b.len());
    let sse: f64 = a.iter().zip(b).map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2)).sum();
    if sse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (peak * peak / (sse / a.len() as f64)).log10()
}

fn planes(bytes: &[u8], w: usize, ten_bit: bool) -> Option<[Vec<u16>; 3]> {
    let cw = w.div_ceil(2);
    let bps = if ten_bit { 2 } else { 1 };
    let h = (1..=8192).find(|&h| bps * (w * h + 2 * cw * h.div_ceil(2)) == bytes.len())?;
    let ch = h.div_ceil(2);
    if !ten_bit {
        let y = bytes[..w * h].iter().map(|&v| u16::from(v)).collect();
        let cb = bytes[w * h..w * h + cw * ch].iter().map(|&v| u16::from(v)).collect();
        let cr = bytes[w * h + cw * ch..].iter().map(|&v| u16::from(v)).collect();
        return Some([y, cb, cr]);
    }
    // p010 is LE words with the ten bits high, chroma interleaved.
    let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]) >> 6;
    let y = (0..w * h).map(|i| word(2 * i)).collect();
    let c0 = 2 * w * h;
    let cb = (0..cw * ch).map(|i| word(c0 + 4 * i)).collect();
    let cr = (0..cw * ch).map(|i| word(c0 + 4 * i + 2)).collect();
    Some([y, cb, cr])
}

fn announced_width(h: &Harness) -> Option<usize> {
    let f = h.announced()?;
    match f.get(h.vocabulary().field_id("width")?) {
        Some(Value::Int(w)) => Some(w as usize),
        _ => None,
    }
}

fn compare(name: &str, packets: &[Vec<u8>], width: usize, ten_bit: bool, slot: usize) -> [f64; 3] {
    let mut hw = Harness::with_slot_size(VaapiVp9Dec::new(), slot);
    let mut sw = Harness::with_slot_size(Vp9Dec::new(), slot);
    hw.start().expect("start vaapivp9dec");
    sw.start().expect("start vp9dec");
    let mut got = Decoded { frames: VecDeque::new(), warnings: Vec::new() };
    let mut want = Decoded { frames: VecDeque::new(), warnings: Vec::new() };
    let mut worst = [f64::INFINITY; 3];
    let mut compared = 0usize;
    let peak = if ten_bit { 1023.0 } else { 255.0 };
    let mut check = |got: &mut Decoded, want: &mut Decoded| {
        while !got.frames.is_empty() && !want.frames.is_empty() {
            let (pg, g) = got.frames.pop_front().unwrap();
            let (pw, w) = want.frames.pop_front().unwrap();
            assert_eq!(pg, pw, "{name}: frame {compared} pts");
            assert_eq!(g.len(), w.len(), "{name}: frame {compared} size");
            if width == 0 {
                let bytes = |v: &[u8]| v.iter().map(|&b| u16::from(b)).collect::<Vec<_>>();
                let p = psnr(&bytes(&g), &bytes(&w), 255.0);
                worst[0] = worst[0].min(p);
                assert!(p >= MIN_PSNR_DB, "{name}: frame {compared} PSNR {p:.2} dB");
                compared += 1;
                continue;
            }
            let g = planes(&g, width, ten_bit)
                .unwrap_or_else(|| panic!("{name}: frame {compared} shape"));
            let w = planes(&w, width, ten_bit).unwrap();
            for k in 0..3 {
                let p = psnr(&g[k], &w[k], peak);
                worst[k] = worst[k].min(p);
                assert!(p >= MIN_PSNR_DB, "{name}: frame {compared} plane {k} PSNR {p:.2} dB");
            }
            compared += 1;
        }
    };
    for (i, packet) in packets.iter().enumerate() {
        let pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        let mut b = hw.alloc(packet);
        b.pts = pts;
        hw.push(b).expect("push vaapivp9dec");
        take(&mut hw, VaapiVp9Dec::OUT, &mut got);
        let mut b = sw.alloc(packet);
        b.pts = pts;
        sw.push(b).expect("push vp9dec");
        take(&mut sw, Vp9Dec::OUT, &mut want);
        check(&mut got, &mut want);
    }
    finish(&mut hw, &mut got);
    finish(&mut sw, &mut want);
    check(&mut got, &mut want);
    assert!(got.warnings.is_empty(), "{name}: vaapivp9dec warned {:?}", got.warnings);
    assert!(
        got.frames.is_empty() && want.frames.is_empty(),
        "{name}: {} hw, {} sw frames left over",
        got.frames.len(),
        want.frames.len()
    );
    assert!(compared > 0, "{name}: no frame out");
    if let (Some(w), true) = (announced_width(&hw), width > 0) {
        assert_eq!(w, width, "{name}: announced width");
    }
    eprintln!(
        "{name}: {compared} frames, worst PSNR Y {:.2} Cb {:.2} Cr {:.2} dB",
        worst[0], worst[1], worst[2]
    );
    worst
}

#[test]
fn segmentation_deltas_and_tiles_match_the_software_decoder() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    compare("features_512x288", &demux_ivf(FEATURES), 512, false, 1 << 20);
}

#[test]
fn error_resilient_frames_match_the_software_decoder() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    compare("resilient_176x144", &demux_ivf(RESILIENT), 176, false, 1 << 20);
}

#[test]
fn lossless_frames_match_the_software_decoder() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    compare("lossless_96x64", &demux_ivf(LOSSLESS), 96, false, 1 << 20);
}

#[test]
fn hidden_frames_and_show_existing_match_the_software_decoder() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    compare("alloc_352x288", &demux_ivf(HIDDEN), 352, false, 1 << 20);
}

#[test]
fn profile_2_decodes_to_p010() {
    let _turn = hw_turn();
    if !vp9_device() || !profile2_device() {
        return eprintln!("skip: no VP9 Profile 2 decode");
    }
    compare("p2_160x96", &demux_ivf(PROFILE2), 160, true, 1 << 20);
    let mut h = Harness::with_slot_size(VaapiVp9Dec::new(), 1 << 20);
    h.start().expect("start");
    for p in demux_ivf(PROFILE2).iter().take(2) {
        let b = h.alloc(p);
        h.push(b).expect("push");
    }
    drop(h.eos().expect("eos"));
    let f = h.announced().expect("announced");
    let v = h.vocabulary();
    let pixfmt = match f.get(v.field_id("pixfmt").unwrap()) {
        Some(Value::Id(id)) => v.value_name(id).unwrap().to_string(),
        other => panic!("pixfmt {other:?}"),
    };
    assert_eq!(pixfmt, "p010");
}

#[test]
fn key_frames_that_change_size_and_depth_rebuild_the_session() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    let mut packets = demux_ivf(LOSSLESS);
    packets.extend(demux_ivf(RESILIENT));
    if profile2_device() {
        packets.extend(demux_ivf(PROFILE2));
    }
    packets.extend(demux_ivf(LOSSLESS));
    let mut h = Harness::with_slot_size(VaapiVp9Dec::new(), 1 << 20);
    h.start().expect("start");
    let mut sizes = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        let mut b = h.alloc(p);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push");
        while let Some(f) = h.pull(VaapiVp9Dec::OUT) {
            sizes.push(f.memory.len());
        }
    }
    sizes.extend(h.eos().expect("eos").iter().map(|b| b.memory.len()));
    let mut warnings = Vec::new();
    for m in h.bus_messages() {
        if let BusMessage::Warning { error, .. } = m {
            warnings.push(error.to_string());
        }
    }
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(sizes.len(), packets.len(), "every frame shown");
    let i420 = |w: usize, h: usize| w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
    let mut want = vec![i420(96, 64); 6];
    want.extend([i420(176, 144); 12]);
    if profile2_device() {
        want.extend([2 * i420(160, 96); 16]);
    }
    want.extend([i420(96, 64); 6]);
    assert_eq!(sizes, want);
    compare("size and depth switch", &packets, 0, false, 1 << 20);
}

/// FNV-1a 64 of a frame.
fn fnv(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

#[test]
fn inter_frames_outgrowing_the_key_frame_decode_and_a_superframe_shows_its_top_layer() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    let mut h = Harness::with_slot_size(VaapiVp9Dec::new(), 1 << 22);
    h.start().expect("start");
    let mut got = Decoded { frames: VecDeque::new(), warnings: Vec::new() };
    for (i, p) in demux_ivf(SVC).iter().enumerate() {
        let mut b = h.alloc(p);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push");
        take(&mut h, VaapiVp9Dec::OUT, &mut got);
    }
    finish(&mut h, &mut got);
    assert!(got.warnings.is_empty(), "{:?}", got.warnings);
    assert_eq!(announced_width(&h), Some(1280));
    let pts: Vec<u64> = got.frames.iter().map(|(t, _)| t.0).collect();
    assert_eq!(pts, [0, FRAME_NS, 2 * FRAME_NS], "one frame per superframe");
    // libvpx's frames, iHD decodes VP9 bit exact
    let sums: Vec<u64> = got.frames.iter().map(|(_, f)| fnv(f)).collect();
    assert_eq!(sums, [0x179d_90ee_cb56_2302, 0x15d7_cae8_7036_0e13, 0x3335_03d2_898f_d9b2]);
}

struct Importer {
    frames: pf_vaapi::gpuframe::FrameRx,
    held: VecDeque<pf_vaapi::GpuFrame>,
    shown: Vec<Timestamp>,
}

impl Importer {
    fn take(&mut self, h: &mut Harness) {
        for buf in h.drain_outputs() {
            let token = pf_vaapi::GpuFrameHeader::from_bytes(buf.memory.data())
                .expect("a video/gpu header")
                .token;
            let mut frame = self.frames.take(token).expect("the frame of a token put out");
            frame.close_fds();
            self.shown.push(buf.pts);
            self.held.push_back(frame);
        }
    }
}

fn warnings(h: &mut Harness) -> Vec<String> {
    h.bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .filter(|w| !w.starts_with("format announcement 'video/gpu'"))
        .collect()
}

#[test]
fn zero_copy_with_every_surface_out_waits_and_drops_nothing() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    let channel = pf_vaapi::GpuFrameChannel::new();
    let mut h =
        Harness::with_pool(VaapiVp9Dec::new_zerocopy(std::sync::Arc::clone(&channel)), 4096, 64);
    let mut imp = Importer {
        frames: channel.take_rx().expect("the sink's end"),
        held: VecDeque::new(),
        shown: Vec::new(),
    };
    h.start().expect("start");
    let packets = demux_ivf(HIDDEN);
    let mut waited = 0;
    let mut n = 0u64;
    for _ in 0..3 {
        for p in &packets {
            let mut b = h.alloc(p);
            b.pts = Timestamp::from_nanos(n * FRAME_NS);
            n += 1;
            h.push(b).expect("push");
            imp.take(&mut h);
            waited = waited.max(h.input_len());
        }
    }
    assert!(waited > 0, "never ran out of surfaces, {} held", imp.held.len());
    let mut rounds = 0;
    while h.input_len() > 0 {
        drop(imp.held.pop_front().expect("input waits only for a surface that is out"));
        h.crank().expect("crank");
        imp.take(&mut h);
        rounds += 1;
        assert!(rounds < 1000, "takes no input though surfaces came back");
    }
    imp.held.clear();
    for buf in h.eos().expect("eos") {
        imp.shown.push(buf.pts);
    }
    let w = warnings(&mut h);
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(imp.shown.len(), 36, "{:?}", imp.shown);
    assert!(imp.shown.windows(2).all(|p| p[0] < p[1]), "in order, {:?}", imp.shown);
}

#[test]
fn a_preview_seek_shows_the_key_frame_before_the_point_once() {
    let _turn = hw_turn();
    if !vp9_device() {
        return;
    }
    let point = Timestamp::from_nanos(5 * FRAME_NS);
    let mut h = Harness::with_slot_size(VaapiVp9Dec::new(), 1 << 20);
    h.seek_preview(0, point).expect("seek");
    let mut got: Vec<(Timestamp, bool)> = Vec::new();
    let flagged = |b: &Buffer| (b.pts, b.flags.contains(BufferFlags::PREVIEW));
    for (i, packet) in demux_ivf(FEATURES).iter().enumerate() {
        let mut b = h.alloc(packet);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push vaapivp9dec");
        got.extend(h.drain_outputs().iter().map(flagged));
    }
    got.extend(h.eos().expect("eos").iter().map(flagged));
    assert!(warnings(&mut h).is_empty(), "vaapivp9dec warned");
    let Some((head, rest)) = got.split_first() else { panic!("nothing out") };
    assert_eq!(*head, (Timestamp::ZERO, true), "the preview, got {got:?}");
    assert!(!rest.is_empty() && rest[0].0 >= point, "the point follows the preview, got {got:?}");
    assert!(
        rest.iter().all(|&(pts, preview)| pts >= point && !preview),
        "from the point on, got {got:?}"
    );
}
