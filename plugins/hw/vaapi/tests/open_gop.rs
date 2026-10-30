//! vaapih265dec on an open GOP stream whose CRA pictures carry RASL leading pictures
//! (H.265 sections 3.1 and 8.1.3). Skips without a device.
//! Fixture made with ffmpeg 8.1, SEI NALs 39 and 40 dropped.
//!
//! ```sh
//! ffmpeg -f lavfi -i "testsrc2=size=320x240:rate=25:duration=2" -pix_fmt yuv420p \
//!   -c:v libx265 -crf 36 -x265-params \
//!   "keyint=24:min-keyint=24:scenecut=0:open-gop=1:bframes=4:b-adapt=0:repeat-headers=1" \
//!   -f hevc raw.hevc
//! ```

// Test code.
#![allow(clippy::disallowed_methods)]

use pf_harness::Harness;
use profluens_core::bus::BusMessage;
use profluens_core::time::Timestamp;

use pf_vaapi::VaapiH265Dec;

const OPEN_GOP: &[u8] = include_bytes!("fixtures/opengop_320x240.hevc");
const ORDER: [u64; 50] = [
    0, 5, 3, 1, 2, 4, 10, 8, 6, 7, 9, 15, 13, 11, 12, 14, 20, 18, 16, 17, 19, 24, 22, 21, 23, 29,
    27, 25, 26, 28, 34, 32, 30, 31, 33, 39, 37, 35, 36, 38, 44, 42, 40, 41, 43, 48, 46, 45, 47, 49,
];
const CRAS: [usize; 2] = [21, 45];
const FRAME_NS: u64 = 40_000_000;
const SLOT: usize = 320 * 240 * 2;

fn units() -> Vec<&'static [u8]> {
    let data = OPEN_GOP;
    let (mut cuts, mut open, mut head) = (vec![0], false, 0);
    for i in 0..data.len().saturating_sub(5) {
        if data[i..i + 3] != [0, 0, 1] {
            continue;
        }
        let code = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
        if (data[i + 3] >> 1) & 0x3F < 32 && data[i + 5] & 0x80 != 0 {
            if open {
                cuts.push(head.min(code));
            }
            (open, head) = (true, usize::MAX);
        } else if head == usize::MAX {
            head = code;
        }
    }
    let ends = cuts.iter().skip(1).copied().chain([data.len()]);
    cuts.iter().zip(ends).map(|(&a, b)| &data[a..b]).collect()
}

fn ffmpeg_decode() -> Option<Vec<u8>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/opengop_320x240.hevc");
    // The devshell LD_LIBRARY_PATH breaks a system ffmpeg.
    let out = std::process::Command::new("ffmpeg")
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-i", path, "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("skip the ffmpeg comparison, ffmpeg failed");
        return None;
    }
    Some(out.stdout)
}

fn at(frames: u64) -> Timestamp {
    Timestamp::from_nanos(frames * FRAME_NS)
}

fn play(from: usize, point: Option<Timestamp>) -> Vec<(Timestamp, Vec<u8>)> {
    decode(Harness::with_slot_size(VaapiH265Dec::new(), SLOT), from, point)
}

fn decode(mut h: Harness, from: usize, point: Option<Timestamp>) -> Vec<(Timestamp, Vec<u8>)> {
    if let Some(point) = point {
        h.seek(0, point).expect("seek");
    }
    for (unit, n) in units().iter().zip(ORDER).skip(from) {
        let mut buf = h.alloc(unit);
        buf.pts = at(n);
        h.push(buf).expect("push");
    }
    let mut out = h.drain_outputs();
    out.extend(h.eos().expect("eos"));
    let warned = h.bus_messages().into_iter().any(|m| matches!(m, BusMessage::Warning { .. }));
    assert!(!warned, "from unit {from}, a warning");
    out.iter().map(|b| (b.pts, b.memory.data().to_vec())).collect()
}

#[test]
fn rasl_pictures_show_played_through_and_not_after_a_seek_to_their_cra() {
    if !pf_vaapi::probe().is_some_and(|caps| caps.supports("h265/annexb")) {
        eprintln!("skip, no VA-API device decodes HEVC");
        return;
    }
    assert_eq!(units().len(), ORDER.len());
    let all = play(0, None);
    let pts: Vec<Timestamp> = all.iter().map(|s| s.0).collect();
    assert_eq!(pts, (0..50).map(at).collect::<Vec<_>>(), "played through, every picture once");
    if let Some(reference) = ffmpeg_decode() {
        let frames: Vec<&[u8]> = reference.chunks(320 * 240 * 3 / 2).collect();
        let off: Vec<usize> = (0..50).filter(|&n| frames.get(n) != Some(&&all[n].1[..])).collect();
        assert!(off.is_empty(), "played through, pictures {off:?} are not ffmpeg's");
    }
    for cra in CRAS {
        let key = at(ORDER[cra]);
        let got = play(cra, Some(key));
        let want: Vec<&(Timestamp, Vec<u8>)> = all.iter().filter(|s| s.0 >= key).collect();
        let stamps: Vec<Timestamp> = got.iter().map(|s| s.0).collect();
        assert_eq!(stamps, want.iter().map(|s| s.0).collect::<Vec<_>>(), "from the CRA at {key:?}");
        assert!(
            got.iter().zip(&want).all(|(a, b)| a == *b),
            "from the CRA at {key:?}, the pictures are the run's"
        );
    }
}
