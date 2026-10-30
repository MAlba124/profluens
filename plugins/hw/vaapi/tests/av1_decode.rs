//! vaapiav1dec held against software av1dec (dav1d). Without film grain the planes must match
//! exactly. With grain they are held to a PSNR floor. Skips without a VA-API AV1 device.

// Test code.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[path = "common/av1.rs"]
mod av1;

use std::collections::VecDeque;
use std::fs::File;

use pf_harness::Harness;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::bus::BusMessage;
use profluens_core::format::Value;
use profluens_core::time::Timestamp;

use pf_av1::Av1Dec;
use pf_vaapi::VaapiAv1Dec;

const GRAIN_MIN_PSNR_DB: f64 = 30.0;
const GRAIN_STRENGTH_DB: f64 = 1.0;
const FRAME_NS: u64 = 33_333_333;

fn hw_turn() -> Option<File> {
    let node = File::open(&pf_vaapi::probe()?.device).ok()?;
    node.lock().ok()?;
    Some(node)
}

fn av1_device() -> bool {
    match pf_vaapi::probe() {
        Some(caps) if caps.supports("av1") => true,
        Some(caps) => {
            eprintln!("skip: no AV1 decode on {}", caps.vendor);
            false
        }
        None => {
            eprintln!("skip: no VA-API device");
            false
        }
    }
}

#[derive(Default)]
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

fn samples(bytes: &[u8], ten_bit: bool) -> Vec<u16> {
    match ten_bit {
        true => bytes.as_chunks::<2>().0.iter().map(|&w| u16::from_le_bytes(w) >> 6).collect(),
        false => bytes.iter().map(|&v| u16::from(v)).collect(),
    }
}

fn announced(h: &Harness, field: &str) -> Option<Value> {
    let f = h.announced()?;
    f.get(h.vocabulary().field_id(field)?)
}

fn compare(name: &str, units: &[Vec<u8>], ten_bit: bool, grain: bool) -> (usize, f64) {
    let slot = 4 << 20;
    let mut hw = Harness::with_slot_size(VaapiAv1Dec::new(), slot);
    let mut sw = Harness::with_slot_size(Av1Dec::new(), slot);
    hw.start().expect("start vaapiav1dec");
    sw.start().expect("start av1dec");
    let mut got = Decoded::default();
    let mut want = Decoded::default();
    let mut worst = f64::INFINITY;
    let mut compared = 0usize;
    let peak = if ten_bit { 1023.0 } else { 255.0 };
    let mut check = |got: &mut Decoded, want: &mut Decoded| {
        while !got.frames.is_empty() && !want.frames.is_empty() {
            let (pg, g) = got.frames.pop_front().unwrap();
            let (pw, w) = want.frames.pop_front().unwrap();
            assert_eq!(pg, pw, "{name}: picture {compared} pts");
            assert_eq!(g.len(), w.len(), "{name}: picture {compared} size");
            if !grain {
                if g != w {
                    let p = psnr(&samples(&g, ten_bit), &samples(&w, ten_bit), peak);
                    let first = g.iter().zip(&w).position(|(a, b)| a != b).unwrap_or(0);
                    panic!(
                        "{name}: picture {compared} differs from byte {first} of {}, PSNR {p:.2} dB",
                        g.len()
                    );
                }
            } else {
                let p = psnr(&samples(&g, ten_bit), &samples(&w, ten_bit), peak);
                worst = worst.min(p);
                assert!(p >= GRAIN_MIN_PSNR_DB, "{name}: picture {compared} PSNR {p:.2} dB");
            }
            compared += 1;
        }
    };
    for (i, unit) in units.iter().enumerate() {
        let pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        let mut b = hw.alloc(unit);
        b.pts = pts;
        hw.push(b).expect("push vaapiav1dec");
        take(&mut hw, VaapiAv1Dec::OUT, &mut got);
        let mut b = sw.alloc(unit);
        b.pts = pts;
        sw.push(b).expect("push av1dec");
        take(&mut sw, Av1Dec::OUT, &mut want);
        check(&mut got, &mut want);
    }
    finish(&mut hw, &mut got);
    finish(&mut sw, &mut want);
    check(&mut got, &mut want);
    assert!(got.warnings.is_empty(), "{name}: vaapiav1dec warned {:?}", got.warnings);
    assert!(
        got.frames.is_empty() && want.frames.is_empty(),
        "{name}: {} hw, {} sw pictures left over",
        got.frames.len(),
        want.frames.len()
    );
    assert_eq!(compared, units.len(), "{name}: one picture a unit");
    match grain {
        false => eprintln!("{name}: {compared} pictures, every one equal to dav1d's"),
        true => eprintln!(
            "{name}: {compared} pictures with film grain, worst PSNR {worst:.2} dB against dav1d"
        ),
    }
    (compared, worst)
}

#[test]
fn the_mkv_fixture_matches_dav1d_exactly() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    let Some(units) = av1::mkv_units() else {
        return;
    };
    compare("mkv_av1", &units, false, false);
}

#[test]
fn tiles_segmentation_and_global_motion_match_dav1d_exactly() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    compare("tools_352x288", &av1::demux_ivf(av1::TOOLS), false, false);
}

#[test]
fn ten_bit_decodes_to_p010_equal_to_dav1d() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    compare("p10_160x96", &av1::demux_ivf(av1::P10), true, false);
    let mut h = Harness::with_slot_size(VaapiAv1Dec::new(), 1 << 20);
    h.start().expect("start");
    for u in av1::demux_ivf(av1::P10).iter().take(2) {
        let b = h.alloc(u);
        h.push(b).expect("push");
    }
    drop(h.eos().expect("eos"));
    let pixfmt = match announced(&h, "pixfmt") {
        Some(Value::Id(id)) => h.vocabulary().value_name(id).unwrap().to_string(),
        other => panic!("pixfmt {other:?}"),
    };
    assert_eq!(pixfmt, "p010");
}

fn ungrained(units: &[Vec<u8>]) -> Vec<Vec<u8>> {
    use libdav1d_sys as sys;
    // SAFETY: zero is a valid settings struct for dav1d_default_settings to fill
    let mut s: sys::Dav1dSettings = unsafe { std::mem::zeroed() };
    // SAFETY: s is a valid out pointer
    unsafe { sys::dav1d_default_settings(&mut s) };
    (s.n_threads, s.max_frame_delay, s.apply_grain) = (1, 1, 0);
    s.logger.callback = None;
    let mut c = std::ptr::null_mut();
    // SAFETY: c and s valid
    assert_eq!(unsafe { sys::dav1d_open(&mut c, &s) }, 0);
    let mut out = Vec::new();
    for tu in units {
        // SAFETY: zero is a valid empty Dav1dData
        let mut data: sys::Dav1dData = unsafe { std::mem::zeroed() };
        // SAFETY: dav1d allocates tu.len() bytes and hands them back
        let buf = unsafe { sys::dav1d_data_create(&mut data, tu.len()) };
        // SAFETY: buf holds tu.len() bytes
        unsafe { std::ptr::copy_nonoverlapping(tu.as_ptr(), buf, tu.len()) };
        while data.sz > 0 {
            // SAFETY: the context and the data are valid
            let r = unsafe { sys::dav1d_send_data(c, &mut data) };
            assert!(r == 0 || r == -11, "dav1d refused a unit, {r}");
            loop {
                // SAFETY: zero is a valid empty picture
                let mut pic: sys::Dav1dPicture = unsafe { std::mem::zeroed() };
                // SAFETY: the context and the picture are valid
                if unsafe { sys::dav1d_get_picture(c, &mut pic) } != 0 {
                    break;
                }
                let (w, h) = (pic.p.w as usize, pic.p.h as usize);
                let mut frame = Vec::with_capacity(w * h * 3 / 2);
                for (plane, (pw, ph)) in
                    [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))]
                        .into_iter()
                        .enumerate()
                {
                    let stride = pic.stride[usize::from(plane > 0)];
                    for row in 0..ph {
                        // SAFETY: a picture's plane holds ph rows of stride bytes
                        let line = unsafe {
                            std::slice::from_raw_parts(
                                pic.data[plane].cast::<u8>().offset(row as isize * stride),
                                pw,
                            )
                        };
                        frame.extend_from_slice(line);
                    }
                }
                out.push(frame);
                // SAFETY: pic is ours, released once
                unsafe { sys::dav1d_picture_unref(&mut pic) };
            }
        }
    }
    // SAFETY: the context is ours, closed once
    unsafe { sys::dav1d_close(&mut c) };
    out
}

fn plane_psnr(a: &[u8], b: &[u8], w: usize, h: usize) -> [f64; 3] {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let cuts = [0, w * h, w * h + cw * ch, w * h + 2 * cw * ch];
    let words = |v: &[u8]| v.iter().map(|&x| u16::from(x)).collect::<Vec<_>>();
    [0, 1, 2]
        .map(|p| psnr(&words(&a[cuts[p]..cuts[p + 1]]), &words(&b[cuts[p]..cuts[p + 1]]), 255.0))
}

#[test]
fn film_grain_is_applied_by_the_driver() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    let units = av1::demux_ivf(av1::GRAIN);
    let (_, worst) = compare("grain_176x144", &units, false, true);
    eprintln!("grain_176x144: film grain tolerance {GRAIN_MIN_PSNR_DB} dB, worst {worst:.2} dB");
    let plain = ungrained(&units);
    let mut hw = Harness::with_slot_size(VaapiAv1Dec::new(), 1 << 20);
    let mut sw = Harness::with_slot_size(Av1Dec::new(), 1 << 20);
    hw.start().expect("start");
    sw.start().expect("start");
    let mut got = Decoded::default();
    let mut want = Decoded::default();
    for u in &units {
        let b = hw.alloc(u);
        hw.push(b).expect("push");
        take(&mut hw, VaapiAv1Dec::OUT, &mut got);
        let b = sw.alloc(u);
        sw.push(b).expect("push");
        take(&mut sw, Av1Dec::OUT, &mut want);
    }
    finish(&mut hw, &mut got);
    finish(&mut sw, &mut want);
    for (i, ((g, w), p)) in got.frames.iter().zip(&want.frames).zip(&plain).enumerate() {
        let hw_grain = plane_psnr(&g.1, p, 176, 144);
        let sw_grain = plane_psnr(&w.1, p, 176, 144);
        let apart = plane_psnr(&g.1, &w.1, 176, 144);
        eprintln!(
            "grain picture {i}: Y Cb Cr dB, driver vs ungrained {:.2} {:.2} {:.2}, dav1d vs ungrained {:.2} {:.2} {:.2}, driver vs dav1d {:.2} {:.2} {:.2}",
            hw_grain[0],
            hw_grain[1],
            hw_grain[2],
            sw_grain[0],
            sw_grain[1],
            sw_grain[2],
            apart[0],
            apart[1],
            apart[2]
        );
        assert!(hw_grain.iter().all(|p| p.is_finite()), "grain {i}: the driver applied no grain");
        for k in 0..3 {
            assert!(
                (hw_grain[k] - sw_grain[k]).abs() < GRAIN_STRENGTH_DB,
                "grain {i} plane {k}: of another strength than dav1d's"
            );
        }
    }
}

#[test]
fn superres_matches_dav1d_exactly() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    compare("superres_256x144", &av1::demux_ivf(av1::SUPERRES), false, false);
}

#[test]
fn error_resilient_frames_match_dav1d_exactly() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    compare("resilient_176x144", &av1::demux_ivf(av1::RESILIENT), false, false);
}

#[test]
fn frames_of_changing_size_match_dav1d_exactly() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    compare("resize_192x128", &av1::demux_ivf(av1::RESIZE), false, false);
}

#[test]
fn sequence_changes_rebuild_the_session() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    let mut units = av1::demux_ivf(av1::RESILIENT);
    units.extend(av1::demux_ivf(av1::TOOLS));
    units.extend(av1::demux_ivf(av1::P10));
    units.extend(av1::demux_ivf(av1::RESILIENT));
    let mut h = Harness::with_slot_size(VaapiAv1Dec::new(), 1 << 20);
    h.start().expect("start");
    let mut sizes = Vec::new();
    for (i, u) in units.iter().enumerate() {
        let mut b = h.alloc(u);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push");
        while let Some(f) = h.pull(VaapiAv1Dec::OUT) {
            sizes.push(f.memory.len());
        }
    }
    sizes.extend(h.eos().expect("eos").iter().map(|b| b.memory.len()));
    let warnings: Vec<String> = h
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .collect();
    assert!(warnings.is_empty(), "{warnings:?}");
    let i420 = |w: usize, h: usize| w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
    let mut want = vec![i420(176, 144); 16];
    want.extend([i420(352, 288); 24]);
    want.extend([2 * i420(160, 96); 16]);
    want.extend([i420(176, 144); 16]);
    assert_eq!(sizes, want);
}

#[test]
fn zero_copy_with_every_surface_out_waits_and_drops_nothing() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    let Some(units) = av1::mkv_units() else {
        return;
    };
    let channel = pf_vaapi::GpuFrameChannel::new();
    let mut h =
        Harness::with_pool(VaapiAv1Dec::new_zerocopy(std::sync::Arc::clone(&channel)), 4096, 64);
    let mut frames = channel.take_rx().expect("the sink's end");
    let mut held = VecDeque::new();
    let mut shown = Vec::new();
    let mut importer =
        |h: &mut Harness, held: &mut VecDeque<pf_vaapi::GpuFrame>, shown: &mut Vec<Timestamp>| {
            for buf in h.drain_outputs() {
                let token = pf_vaapi::GpuFrameHeader::from_bytes(buf.memory.data())
                    .expect("a video/gpu header")
                    .token;
                let mut frame = frames.take(token).expect("the frame of a token put out");
                frame.close_fds();
                shown.push(buf.pts);
                held.push_back(frame);
            }
        };
    h.start().expect("start");
    let mut waited = 0;
    for (i, u) in units.iter().enumerate() {
        let mut b = h.alloc(u);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push");
        importer(&mut h, &mut held, &mut shown);
        waited = waited.max(h.input_len());
    }
    if waited == 0 {
        let msgs: Vec<String> = h
            .bus_messages()
            .into_iter()
            .filter_map(|m| match m {
                BusMessage::Warning { error, .. } | BusMessage::Error { error, .. } => {
                    Some(error.to_string())
                }
                _ => None,
            })
            .collect();
        panic!("never ran out of surfaces, {} held, {} shown, {msgs:?}", held.len(), shown.len());
    }
    let mut rounds = 0;
    while h.input_len() > 0 {
        drop(held.pop_front().expect("input waits only for a surface that is out"));
        h.crank().expect("crank");
        importer(&mut h, &mut held, &mut shown);
        rounds += 1;
        assert!(rounds < 1000, "takes no input though surfaces came back");
    }
    held.clear();
    for buf in h.eos().expect("eos") {
        shown.push(buf.pts);
    }
    let w: Vec<String> = h
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(error.to_string()),
            _ => None,
        })
        .filter(|w| !w.starts_with("format announcement 'video/gpu'"))
        .collect();
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(shown.len(), units.len(), "{shown:?}");
    assert!(shown.windows(2).all(|p| p[0] < p[1]), "in order, {shown:?}");
    eprintln!(
        "zero-copy mkv_av1: {} frames out in order, up to {waited} units waited for a surface",
        shown.len()
    );
}

#[test]
fn a_preview_seek_shows_the_key_frame_before_the_point_once() {
    let _turn = hw_turn();
    if !av1_device() {
        return;
    }
    let point = Timestamp::from_nanos(5 * FRAME_NS);
    let mut h = Harness::with_slot_size(VaapiAv1Dec::new(), 1 << 20);
    h.seek_preview(0, point).expect("seek");
    let mut got: Vec<(Timestamp, bool)> = Vec::new();
    let flagged = |b: &Buffer| (b.pts, b.flags.contains(BufferFlags::PREVIEW));
    for (i, unit) in av1::demux_ivf(av1::TOOLS).iter().enumerate() {
        let mut b = h.alloc(unit);
        b.pts = Timestamp::from_nanos(i as u64 * FRAME_NS);
        h.push(b).expect("push vaapiav1dec");
        got.extend(h.drain_outputs().iter().map(flagged));
    }
    got.extend(h.eos().expect("eos").iter().map(flagged));
    let warned = h.bus_messages().iter().any(|m| matches!(m, BusMessage::Warning { .. }));
    assert!(!warned, "vaapiav1dec warned");
    let Some((head, rest)) = got.split_first() else { panic!("nothing out") };
    assert_eq!(*head, (Timestamp::ZERO, true), "the preview, got {got:?}");
    assert!(!rest.is_empty() && rest[0].0 >= point, "the point follows the preview, got {got:?}");
    assert!(
        rest.iter().all(|&(pts, preview)| pts >= point && !preview),
        "from the point on, got {got:?}"
    );
}
