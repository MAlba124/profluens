//! Whether the driver scales and converts in the decode call itself (VAConfigAttribDecProcessing),
//! per decode profile. `cargo run --example dec_processing -- [/dev/dri/renderD128]`
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pf_vaapi::ffi;
use pf_vaapi::va::{Display, config_attribute};
use std::path::PathBuf;

/// VAConfigAttribDecProcessing, va.h.
const DEC_PROCESSING: ffi::VAConfigAttribType = 8;

fn main() {
    let path =
        std::env::args().nth(1).map_or_else(|| PathBuf::from("/dev/dri/renderD128"), PathBuf::from);
    let d = Display::open(&path).expect("open the display");
    println!("vendor {}", d.vendor());
    let profiles = [
        ("H264Main", ffi::VAProfileH264Main),
        ("H264High", ffi::VAProfileH264High),
        ("HEVCMain", ffi::VAProfileHEVCMain),
        ("HEVCMain10", ffi::VAProfileHEVCMain10),
        ("HEVCMain12", 23),
        ("VP9Profile0", ffi::VAProfileVP9Profile0),
        ("VP9Profile2", ffi::VAProfileVP9Profile2),
        ("AV1Profile0", ffi::VAProfileAV1Profile0),
    ];
    for (name, p) in profiles {
        match config_attribute(&d, p, ffi::VAEntrypointVLD, DEC_PROCESSING) {
            Ok(Some(v)) => {
                println!("{name} dec_processing={}", if v & 1 != 0 { "yes" } else { "no" })
            }
            Ok(None) => println!("{name} dec_processing=not reported"),
            Err(e) => println!("{name} error {}", e.message()),
        }
    }
    // the encode entrypoints, and the prediction directions (VAConfigAttribPredictionDirection
    // 39) and reference counts (EncMaxRefFrames 13) each takes
    for (name, p) in [
        ("H264High", ffi::VAProfileH264High),
        ("HEVCMain", ffi::VAProfileHEVCMain),
        ("HEVCMain10", ffi::VAProfileHEVCMain10),
    ] {
        for (ep_name, ep) in
            [("EncSlice", ffi::VAEntrypointEncSlice), ("EncSliceLP", ffi::VAEntrypointEncSliceLP)]
        {
            let dir = config_attribute(&d, p, ep, 39);
            let refs = config_attribute(&d, p, ep, 13);
            match (dir, refs) {
                (Ok(dir), Ok(refs)) => {
                    println!("{name} {ep_name} prediction_direction={dir:?} max_refs={refs:?}")
                }
                (Err(e), _) | (_, Err(e)) => println!("{name} {ep_name} error {}", e.message()),
            }
        }
    }
}
