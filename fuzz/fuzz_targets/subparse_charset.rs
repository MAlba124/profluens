//! The subparse charset decoder. Input is `knob`, then the bytes. `knob % 7` is 0 sniffing, else
//! a forced `Charset::ALL` entry, the rest seeds the push cuts. One shot and chunked must agree,
//! clean UTF-8 must come out as is, and Blocks fed as units must consume every byte.

#![no_main]
// Fuzz target, not an element.
#![allow(clippy::disallowed_methods)]
// `fuzz_target!` opens libfuzzer-sys's debug output file.
#![allow(clippy::disallowed_types)]

use libfuzzer_sys::fuzz_target;
use pf_sim::prng::Prng;
use pf_subparse::encoding::{Charset, Decoder, SNIFF_LIMIT, decode_all};

const CEILINGS: [u64; 4] = [3, 64, 4096, SNIFF_LIMIT as u64 + 7];

fuzz_target!(|data: &[u8]| {
    let Some((&knob, body)) = data.split_first() else {
        return;
    };
    let forced = match knob % 7 {
        0 => None,
        n => Some(Charset::ALL[usize::from(n) - 1]),
    };
    let mut whole = String::new();
    decode_all(body, forced, &mut whole);
    assert!(whole.len() <= 3 * body.len() + 3, "{} bytes of text from {}", whole.len(), body.len());
    let marked = [&[0xef, 0xbb, 0xbf][..], &[0xff, 0xfe], &[0xfe, 0xff], &[0, 0, 0xfe, 0xff]]
        .iter()
        .any(|m| body.starts_with(m));
    let utf8_ok = matches!(forced, None | Some(Charset::Utf8));
    if let Ok(clean) = std::str::from_utf8(body)
        && utf8_ok
        && !marked
    {
        assert!(whole == clean, "clean UTF-8 did not come out as is");
    }

    let mut rng = Prng::new(u64::from(knob) << 32 | body.len() as u64);
    let ceiling = CEILINGS[usize::from(knob >> 6)].min(body.len().max(1) as u64);
    let mut d = Decoder::new(forced);
    let (mut pending, mut chunked, mut at) = (Vec::new(), String::new(), 0);
    loop {
        let n = (rng.range(1, ceiling + 1) as usize).min(body.len() - at);
        pending.extend_from_slice(&body[at..at + n]);
        at += n;
        let last = at == body.len();
        let used = d.decode(&pending, last, &mut chunked);
        assert!(used <= pending.len(), "consumed past the input");
        pending.drain(..used);
        if last {
            break;
        }
    }
    assert!(pending.is_empty(), "bytes left after the last push");
    assert!(chunked == whole, "chunked and one shot disagree");

    let mut units = Decoder::new(forced);
    let mut text = String::new();
    for unit in body.split(|&b| b == b'\n') {
        assert_eq!(units.decode(unit, true, &mut text), unit.len(), "a unit left bytes");
    }
});
