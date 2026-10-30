//! Hardware VA-API decode tests. Skip without a device or with PF_NO_VAAPI=1.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::path::Path;

use pf_harness::Harness;
use pf_mkv::{MatroskaReader, Reframer, nal_head_from_config};
use profluens_core::event::Event;
use profluens_core::format::Value;
use profluens_core::time::Timestamp;

use pf_vaapi::VaapiH264Dec;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../output.mkv");
const SLOT: usize = 8 * 1024 * 1024;

fn hw_available() -> bool {
    pf_vaapi::probe().is_some()
}

#[test]
fn probe_reports_h264() {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip probe_reports_h264: no VA-API device");
        return;
    };
    eprintln!(
        "device={} va={}.{} vendor={:?} families={:?}",
        caps.device.display(),
        caps.version.0,
        caps.version.1,
        caps.vendor,
        caps.decode_families
    );
    assert!(
        caps.supports("h264/annexb"),
        "expected h264/annexb in decode families, got {:?}",
        caps.decode_families
    );
}

fn read_annexb_aus(max_aus: usize) -> Option<Vec<(Vec<u8>, bool)>> {
    if !Path::new(FIXTURE).exists() {
        return None;
    }
    let data = std::fs::read(FIXTURE).ok()?;
    let mut reader = MatroskaReader::new();
    let mut fed = 0usize;
    let chunk = 1 << 20;
    while !reader.tracks_ready() && fed < data.len() {
        let end = (fed + chunk).min(data.len());
        reader.push(&data[fed..end]).ok()?;
        fed = end;
    }
    let track = reader.tracks().iter().find(|t| t.codec_id == "V_MPEG4/ISO/AVC")?.clone();
    let (head, length_size) = nal_head_from_config(&track.codec_private, false).ok()?;
    let reframer = Reframer::Nal { length_size };

    let mut aus: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut first = true;
    let mut reframe_buf = Vec::new();
    loop {
        while let Some(frame) = reader.next_frame() {
            if frame.track_number != track.track_number {
                continue;
            }
            let annexb = reframer.reframe_into(&frame.data, &mut reframe_buf).ok()?;
            let mut au = Vec::new();
            if first {
                au.extend_from_slice(&head);
                first = false;
            }
            au.extend_from_slice(annexb);
            aus.push((au, frame.keyframe));
            if aus.len() >= max_aus {
                return Some(aus);
            }
        }
        if fed >= data.len() {
            break;
        }
        let end = (fed + chunk).min(data.len());
        reader.push(&data[fed..end]).ok()?;
        fed = end;
    }
    if aus.is_empty() { None } else { Some(aus) }
}

#[test]
fn decode_output_mkv_first_aus() {
    if !hw_available() {
        eprintln!("skip decode_output_mkv_first_aus: no VA-API device");
        return;
    }
    let Some(aus) = read_annexb_aus(30) else {
        eprintln!("skip decode_output_mkv_first_aus: fixture {FIXTURE} absent or no AVC track");
        return;
    };
    eprintln!("fed {} access units", aus.len());

    let mut h = Harness::with_slot_size(VaapiH264Dec::new(), SLOT);
    h.start().expect("start");

    let mut frames: Vec<profluens_core::buffer::Buffer> = Vec::new();
    for (i, (au, _kf)) in aus.iter().enumerate() {
        let mut buf = h.alloc(au);
        buf.pts = Timestamp::from_nanos(i as u64 * 40_000_000);
        h.push(buf).expect("push");
        while let Some(f) = h.pull(VaapiH264Dec::OUT) {
            frames.push(f);
        }
    }
    for f in h.eos().expect("eos") {
        frames.push(f);
    }

    for m in h.bus_messages() {
        match m {
            profluens_core::bus::BusMessage::Warning { error, .. } => eprintln!("warn: {error:?}"),
            profluens_core::bus::BusMessage::Error { error, .. } => eprintln!("error: {error:?}"),
            _ => {}
        }
    }

    assert!(!frames.is_empty(), "expected at least one decoded frame");

    let announced = h.announced().expect("format announced");
    let vocab = h.vocabulary();
    let w_id = vocab.field_id("width").unwrap();
    let h_id = vocab.field_id("height").unwrap();
    let pf_id = vocab.field_id("pixfmt").unwrap();
    let width = match announced.get(w_id) {
        Some(Value::Int(w)) => w as u32,
        other => panic!("width not an Int: {other:?}"),
    };
    let height = match announced.get(h_id) {
        Some(Value::Int(hh)) => hh as u32,
        other => panic!("height not an Int: {other:?}"),
    };
    let pixfmt = match announced.get(pf_id) {
        Some(Value::Id(id)) => vocab.value_name(id).unwrap().to_string(),
        other => panic!("pixfmt not an Id: {other:?}"),
    };
    eprintln!("announced {width}x{height} {pixfmt}, {} frames", frames.len());
    assert_eq!(pixfmt, "nv12");
    assert!(width >= 16 && height >= 16, "implausible dims {width}x{height}");

    let need = (width * height + width * height.div_ceil(2)) as usize;
    let f0 = &frames[0];
    assert_eq!(f0.memory.data().len(), need, "NV12 tight length mismatch");
    let y = &f0.memory.data()[..(width * height) as usize];
    let first = y[0];
    assert!(y.iter().any(|&b| b != first), "Y plane is constant, surface likely not decoded");

    // Broken reference handling collapses P/B frames to near-zero luma while I frames stay fine.
    for (i, f) in frames.iter().enumerate() {
        let y = &f.memory.data()[..(width * height) as usize];
        let mean = y.iter().map(|&b| b as u64).sum::<u64>() / y.len() as u64;
        assert!(
            mean >= 10,
            "frame {i}: mean luma {mean} near black, inter prediction likely broken \
             (refs/weights), I-frames-only would pass the constant check"
        );
    }
}

#[test]
fn flush_then_continue() {
    if !hw_available() {
        eprintln!("skip flush_then_continue: no VA-API device");
        return;
    }
    let Some(aus) = read_annexb_aus(30) else {
        eprintln!("skip flush_then_continue: fixture absent");
        return;
    };

    let mut h = Harness::with_slot_size(VaapiH264Dec::new(), SLOT);
    h.start().expect("start");

    let split = aus.len().min(10);
    for (au, _) in &aus[..split] {
        let buf = h.alloc(au);
        let _ = h.push(buf);
        while h.pull(VaapiH264Dec::OUT).is_some() {}
    }

    h.push_event(Event::FlushStart).expect("flush");
    let resume: Vec<_> = aus
        .iter()
        .enumerate()
        .filter(|(i, (_, kf))| *i >= split && *kf)
        .map(|(_, (au, _))| au.clone())
        .collect();
    let resume = if resume.is_empty() { vec![aus[0].0.clone()] } else { resume };

    let mut got = 0;
    for au in &resume {
        let buf = h.alloc(au);
        let _ = h.push(buf);
        while h.pull(VaapiH264Dec::OUT).is_some() {
            got += 1;
        }
    }
    for _ in h.eos().expect("eos") {
        got += 1;
    }
    for m in h.bus_messages() {
        match m {
            profluens_core::bus::BusMessage::Warning { error, .. } => eprintln!("warn: {error:?}"),
            profluens_core::bus::BusMessage::Error { error, .. } => eprintln!("error: {error:?}"),
            _ => {}
        }
    }
    assert!(got >= 1, "no frames produced after flush, VA context wedged");
}
