//! How a fake sink should behave, as data. The fake sink acts on `paced` and `lateness_bound`.
//! Nothing acts on the rest yet.

use crate::prng::Prng;
use crate::time::{MS, NONE, Ns, US};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SinkProfile {
    pub open_delay: Ns,
    pub first_frame_delay: Ns,
    pub render_cost: Ns,
    pub ring_depth: Ns,
    pub device_latency: Ns,
    pub lateness_bound: Ns,
    pub paced: bool,
}

impl SinkProfile {
    pub const IDEAL: Self = Self {
        open_delay: 0,
        first_frame_delay: 0,
        render_cost: 0,
        ring_depth: 0,
        device_latency: 0,
        lateness_bound: NONE,
        paced: false,
    };
}

pub fn device(seed: u64) -> SinkProfile {
    let root = Prng::new(seed).substream_named("sink.profile");
    let draw = |name: &str, lo: Ns, hi: Ns| root.substream_named(name).range(lo, hi);
    SinkProfile {
        open_delay: draw("open_delay", 20 * MS, 200 * MS),
        first_frame_delay: draw("first_frame_delay", 0, 150 * MS),
        render_cost: draw("render_cost", 500 * US, 8 * MS),
        ring_depth: draw("ring_depth", 100 * MS, 400 * MS),
        device_latency: draw("device_latency", 20 * MS, 120 * MS),
        lateness_bound: draw("lateness_bound", 10 * MS, 40 * MS),
        paced: true,
    }
}

pub fn for_run(seed: u64, sink: u16, device_percent: u32) -> SinkProfile {
    let run = Prng::new(seed).substream_named("sink.kind");
    // One decision per run, so a run is all devices or all ideal.
    if !run.substream(0).chance(device_percent) {
        return SinkProfile::IDEAL;
    }
    device(run.substream(1 + sink as u64).next_u64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_draws_the_same_device_every_time() {
        assert_eq!(device(7), device(7));
        assert_ne!(device(7), device(8));
        assert_eq!(for_run(7, 1, 50), for_run(7, 1, 50));
    }

    #[test]
    fn device_parameters_stay_in_their_ranges() {
        for seed in 0..2_000u64 {
            let p = device(seed);
            assert!((20 * MS..200 * MS).contains(&p.open_delay));
            assert!(p.first_frame_delay < 150 * MS);
            assert!((500 * US..8 * MS).contains(&p.render_cost));
            assert!((100 * MS..400 * MS).contains(&p.ring_depth));
            assert!((20 * MS..120 * MS).contains(&p.device_latency));
            assert!((10 * MS..40 * MS).contains(&p.lateness_bound));
            assert!(p.paced);
        }
    }

    #[test]
    fn the_share_of_device_runs_follows_the_percent() {
        let devices =
            (0..1_000u64).filter(|seed| for_run(*seed, 0, 30) != SinkProfile::IDEAL).count();
        assert!((240..360).contains(&devices), "{devices} of 1000 at 30 percent");
        assert!((0..100u64).all(|seed| for_run(seed, 0, 0) == SinkProfile::IDEAL));
        assert!((0..100u64).all(|seed| for_run(seed, 0, 100) != SinkProfile::IDEAL));
    }

    #[test]
    fn sinks_of_one_run_agree_on_the_kind_and_differ_in_the_draw() {
        for seed in 0..200u64 {
            let (video, audio) = (for_run(seed, 0, 50), for_run(seed, 1, 50));
            assert_eq!(video == SinkProfile::IDEAL, audio == SinkProfile::IDEAL, "seed {seed}");
            if video != SinkProfile::IDEAL {
                assert_ne!(video, audio, "seed {seed}");
            }
        }
    }
}
