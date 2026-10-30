//! The AV1 streams the vaapiav1dec tests share, as temporal units.
//! IVF fixtures, ffmpeg over `testsrc2`:
//! - `av1_tools_352x288.ivf`: libaom over a pan across a 640x480 `testsrc2`, `-vf
//!   crop=352:288:x='4*n':y='2*n'`, 24 frames, `-cpu-used 3 -crf 45 -tiles 2x2 -aq-mode 1
//!   -aom-params num-tile-groups=2`
//! - `av1_p10_160x96.ivf`: libaom, 16 frames, `-pix_fmt yuv420p10le -cpu-used 6 -crf 35`
//! - `av1_grain_176x144.ivf`: libaom, 12 frames, `-cpu-used 6 -crf 40 -aom-params
//!   film-grain-test=1`
//! - `av1_superres_256x144.ivf`: SVT-AV1, 12 frames, `-preset 8 -crf 40 -svtav1-params
//!   superres-mode=1:superres-denom=13:superres-kf-denom=13`
//! - `av1_resilient_176x144.ivf`: libaom, 16 frames, `-usage realtime -cpu-used 8 -crf 40
//!   -error-resilience default -lag-in-frames 0`
//! - `av1_resize_192x128.ivf`: SVT-AV1, 16 frames, `-preset 8 -crf 40 -svtav1-params
//!   resize-mode=2:enable-tf=0`
#![allow(dead_code)]

use std::path::PathBuf;

use pf_mkv::MatroskaReader;

pub const TOOLS: &[u8] = include_bytes!("../fixtures/av1_tools_352x288.ivf");
pub const P10: &[u8] = include_bytes!("../fixtures/av1_p10_160x96.ivf");
pub const GRAIN: &[u8] = include_bytes!("../fixtures/av1_grain_176x144.ivf");
pub const SUPERRES: &[u8] = include_bytes!("../fixtures/av1_superres_256x144.ivf");
pub const RESILIENT: &[u8] = include_bytes!("../fixtures/av1_resilient_176x144.ivf");
pub const RESIZE: &[u8] = include_bytes!("../fixtures/av1_resize_192x128.ivf");

pub const IVF: [(&str, &[u8]); 6] = [
    ("tools_352x288", TOOLS),
    ("p10_160x96", P10),
    ("grain_176x144", GRAIN),
    ("superres_256x144", SUPERRES),
    ("resilient_176x144", RESILIENT),
    ("resize_192x128", RESIZE),
];

pub fn demux_ivf(data: &[u8]) -> Vec<Vec<u8>> {
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

pub fn mkv_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PF_AV1_MKV") {
        return Some(PathBuf::from(p));
    }
    let checkout =
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/out/mkv_av1.mkv"));
    if checkout.exists() {
        return Some(checkout);
    }
    let here = PathBuf::from("mkv_av1.mkv");
    here.exists().then_some(here)
}

// Test setup reads the fixture.
#[allow(clippy::disallowed_methods)]
pub fn mkv_units() -> Option<Vec<Vec<u8>>> {
    let Some(path) = mkv_path() else {
        eprintln!("skip: mkv_av1.mkv absent (cargo xtask fixtures, or PF_AV1_MKV)");
        return None;
    };
    let data = std::fs::read(path).ok()?;
    let mut reader = MatroskaReader::new();
    reader.push(&data).ok()?;
    let track = reader.tracks().iter().find(|t| t.codec_id == "V_AV1")?.clone();
    // av1C is four bytes of its own, then the sequence header OBU
    let config = track.codec_private.get(4..).unwrap_or(&[]).to_vec();
    let mut units = Vec::new();
    while let Some(frame) = reader.next_frame() {
        if frame.track_number != track.track_number {
            continue;
        }
        let mut tu = if units.is_empty() { config.clone() } else { Vec::new() };
        tu.extend_from_slice(&frame.data);
        units.push(tu);
    }
    (!units.is_empty()).then_some(units)
}
