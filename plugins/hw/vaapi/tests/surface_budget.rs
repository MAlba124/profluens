//! vaapih264dec with MMCO 1 unmarking references across frame_num wraps, picNumX vs FrameNumWrap.
//! Fixture is 160 pictures, max_num_ref_frames 16, MaxFrameNum 64, made with ffmpeg 8.1 x264.
//!
//! ```sh
//! ffmpeg -f lavfi -i "testsrc2=size=176x144:rate=25:duration=8" -pix_fmt yuv420p \
//!   -c:v libx264 -preset medium -qp 46 -x264-params \
//!   "keyint=250:min-keyint=250:scenecut=0:bframes=3:b-pyramid=normal:b-adapt=0:ref=16:aud=1" \
//!   -vframes 160 -bsf:v h264_mp4toannexb -f h264 raw.h264
//! ffmpeg -i raw.h264 -c copy -bsf:v filter_units=remove_types=6 -f h264 mmco_wrap_176x144.h264
//! ```

// Test code.
#![allow(clippy::disallowed_methods)]

use std::collections::VecDeque;

use pf_harness::Harness;
use profluens_core::bus::BusMessage;
use profluens_core::time::Timestamp;

use pf_vaapi::VaapiH264Dec;

const STREAM: &[u8] = include_bytes!("fixtures/mmco_wrap_176x144.h264");
const PICTURES: usize = 160;
const FRAME_NS: u64 = 40_000_000;
const NV12: usize = 176 * 144 * 3 / 2;
const HOLD: usize = 16 + 2;

fn units() -> Vec<&'static [u8]> {
    let data = STREAM;
    let mut cuts = Vec::new();
    for i in 0..data.len().saturating_sub(4) {
        if data[i..i + 3] == [0, 0, 1] && data[i + 3] & 0x1F == 9 {
            cuts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
        }
    }
    let ends = cuts.iter().skip(1).copied().chain([data.len()]);
    cuts.iter().zip(ends).map(|(&a, b)| &data[a..b]).collect()
}

fn ffmpeg_decode() -> Option<Vec<u8>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mmco_wrap_176x144.h264");
    // The devshell LD_LIBRARY_PATH breaks a system ffmpeg.
    let out = std::process::Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-i", path, "-f", "rawvideo", "-pix_fmt", "nv12", "-"])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("skip the ffmpeg comparison, ffmpeg failed");
        return None;
    }
    Some(out.stdout)
}

fn sink(
    h: &mut Harness,
    held: &mut VecDeque<profluens_core::buffer::Buffer>,
    shown: &mut Vec<Vec<u8>>,
    hold: usize,
) {
    held.extend(h.drain_outputs());
    while held.len() > hold {
        let buf = held.pop_front().expect("held");
        shown.push(buf.memory.data().to_vec());
    }
}

#[test]
fn every_picture_comes_out_across_frame_num_wraps_with_downstream_holding() {
    if !pf_vaapi::probe().is_some_and(|caps| caps.supports("h264/annexb")) {
        eprintln!("skip, no VA-API device decodes H.264");
        return;
    }
    let units = units();
    assert_eq!(units.len(), PICTURES);
    let mut h = Harness::with_pool(VaapiH264Dec::new(), NV12, HOLD as u32 + 2);
    let (mut held, mut shown) = (VecDeque::new(), Vec::new());
    for (n, unit) in units.iter().enumerate() {
        let mut buf = h.alloc(unit);
        buf.pts = Timestamp::from_nanos(n as u64 * FRAME_NS);
        h.push(buf).expect("push");
        sink(&mut h, &mut held, &mut shown, HOLD);
    }
    for _ in 0..PICTURES {
        if h.input_len() == 0 {
            break;
        }
        h.crank().expect("crank");
        sink(&mut h, &mut held, &mut shown, HOLD);
    }
    assert_eq!(h.input_len(), 0, "every unit taken");
    sink(&mut h, &mut held, &mut shown, 0);
    held.extend(h.eos().expect("eos"));
    shown.extend(held.drain(..).map(|b| b.memory.data().to_vec()));

    let warnings: Vec<String> = h
        .bus_messages()
        .into_iter()
        .filter_map(|m| match m {
            BusMessage::Warning { error, .. } => Some(format!("{error}")),
            _ => None,
        })
        .collect();
    assert!(warnings.is_empty(), "warnings {warnings:?}");
    assert_eq!(shown.len(), PICTURES, "every picture out once");
    if let Some(reference) = ffmpeg_decode() {
        let frames: Vec<&[u8]> = reference.chunks(NV12).collect();
        assert_eq!(frames.len(), PICTURES);
        let off: Vec<usize> = (0..PICTURES).filter(|&n| frames[n] != &shown[n][..]).collect();
        assert!(off.is_empty(), "pictures {off:?} are not ffmpeg's");
    }
}
