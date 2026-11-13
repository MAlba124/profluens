#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

// The event contract driver pf-bitstream's contract test runs on random streams.
#[path = "../../lib/bitstream/tests/drive/mod.rs"]
mod drive;

use libfuzzer_sys::fuzz_target;

// A knob byte picks the codec, the rest is 4 bytes per picture after one for the sequence.
fuzz_target!(|data: &[u8]| {
    let Some((&knob, rest)) = data.split_first() else {
        return;
    };
    let mut bytes = drive::Bytes::new(rest);
    if knob & 1 == 0 {
        drive::h264(&mut bytes);
    } else {
        drive::h265(&mut bytes);
    }
});
