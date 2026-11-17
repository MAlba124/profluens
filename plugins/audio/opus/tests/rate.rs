//! `opusenc`'s output rate against its bitrate, with and without the VBR constraint.

#![allow(clippy::disallowed_methods)] // test fixtures, not a hot path

use pf_harness::Harness;
use pf_opus::OpusEnc;
use profluens_core::format::{Value, ValueDesc};

const NOMINAL: u32 = 128_000;
const SECONDS: usize = 10;

fn raw() -> [(&'static str, ValueDesc); 3] {
    [
        ("rate", ValueDesc::Int(48_000)),
        ("channels", ValueDesc::Int(2)),
        ("sample", ValueDesc::Id("s16")),
    ]
}

/// Three steady tones a side, a signal plain VBR spends far over its bitrate on.
fn tones() -> Vec<u8> {
    let tau = std::f64::consts::TAU;
    let side = |t: f64, k: f64| {
        [440.0, 1000.0, 3000.0].iter().map(|f| (tau * f * k * t).sin()).sum::<f64>() / 3.0
    };
    (0..48_000 * SECONDS)
        .flat_map(|i| {
            let t = i as f64 / 48_000.0;
            [side(t, 1.0), side(t, 1.37)].map(|v| ((v * 20_000.0) as i16).to_le_bytes())
        })
        .flatten()
        .collect()
}

/// The output's rate over the whole run and over its densest second, bit/s.
fn rates(mut h: Harness) -> (u64, u64) {
    h.fix_sink_format(OpusEnc::IN, "audio/raw", &raw());
    h.start().expect("start");
    let mut out = Vec::new();
    for c in tones().chunks(4096) {
        let b = h.alloc(c);
        h.push(b).expect("push");
        out.extend(h.drain_outputs());
    }
    out.extend(h.eos().expect("eos"));
    let sizes: Vec<u64> = out.iter().map(|b| b.memory.len() as u64).collect();
    // 20 ms packets, fifty a second
    let whole = sizes.iter().sum::<u64>() * 8 * 50 / sizes.len() as u64;
    let second = sizes.windows(50).map(|w| w.iter().sum::<u64>() * 8).max().unwrap_or(0);
    (whole, second)
}

#[test]
fn the_constraint_holds_a_tonal_signal_to_the_bitrate_that_plain_vbr_runs_over() {
    let nominal = u64::from(NOMINAL);
    let (plain, _) = rates(Harness::new(OpusEnc::new().with_bitrate(NOMINAL)));
    let (held, held_second) =
        rates(Harness::new(OpusEnc::new().with_bitrate(NOMINAL).constrained()));
    assert!(plain > nominal * 15 / 10, "plain VBR at {plain} bit/s is the case to bound");
    assert!(held <= nominal + nominal / 100, "{held} bit/s over the run");
    // a frame of the bitrate over at most, and the bytes a packet rounds up to
    assert!(held_second <= nominal + nominal * 3 / 100, "{held_second} bit/s in a second");
    assert!(held >= nominal * 9 / 10, "{held} bit/s, the bitrate is still spent");
}

#[test]
fn the_constrained_prop_sets_it_as_the_builder_does() {
    let mut h = Harness::new(OpusEnc::new().with_bitrate(NOMINAL));
    assert!(h.set_prop("constrained", Value::Int(1)));
    let by_prop = rates(h);
    let by_builder = rates(Harness::new(OpusEnc::new().with_bitrate(NOMINAL).constrained()));
    assert_eq!(by_prop, by_builder);
}
