#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

// The event contract driver pf-bitstream's contract test runs on random streams.
#[path = "../../lib/bitstream/tests/drive/mod.rs"]
mod drive;

use libfuzzer_sys::fuzz_target;

// A knob byte picks the codec, and for H.264 and HEVC order counts within the reorder depth.
// The rest is 4 bytes per picture, after one for the sequence with H.264 and HEVC.
fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let mut bytes = drive::Bytes::new(rest);
    let conform = knob & 4 != 0;
    match knob & 3 {
        0 if conform => drive::h264_conform(&mut bytes),
        0 => drive::h264(&mut bytes),
        1 if conform => drive::h265_conform(&mut bytes),
        1 => drive::h265(&mut bytes),
        2 => drive::vp9(&mut bytes),
        _ => drive::av1(&mut bytes),
    };
});
