//! Virtual time as plain nanoseconds, mirroring core's `Timestamp` and `Rational`.

pub type Ns = u64;

pub const NONE: Ns = u64::MAX;

pub const US: Ns = 1_000;
pub const MS: Ns = 1_000_000;
pub const SEC: Ns = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub const fn new(num: i32, den: i32) -> Self {
        Self { num, den }
    }

    pub const fn is_valid(self) -> bool {
        self.num > 0 && self.den > 0
    }
}

/// Computed from the index, so a fractional frame length never drifts.
pub const fn frame_pts(index: u64, fps: Rational) -> Ns {
    if !fps.is_valid() {
        return NONE;
    }
    let ns = index as u128 * SEC as u128 * fps.den as u128 / fps.num as u128;
    if ns >= NONE as u128 {
        return NONE;
    }
    ns as Ns
}

pub const fn frame_at(t: Ns, fps: Rational) -> u64 {
    if !fps.is_valid() || t == NONE {
        return 0;
    }
    let mut index = (t as u128 * fps.num as u128 / (SEC as u128 * fps.den as u128)) as u64;
    // `frame_pts` floors, so the estimate can be short by a few frames, under 3 for an i32 rate.
    let mut steps = 0;
    while steps < 4 && frame_pts(index + 1, fps) <= t {
        index += 1;
        steps += 1;
    }
    index
}

pub const fn frames_in(duration: Ns, fps: Rational) -> u64 {
    frame_at(duration, fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NTSC: Rational = Rational::new(30_000, 1_001);

    #[test]
    fn frame_pts_does_not_drift() {
        // 30000 frames of 30000/1001 fps are exactly 1001 seconds.
        assert_eq!(frame_pts(30_000, NTSC), 1_001 * SEC);
        assert_eq!(frame_pts(0, NTSC), 0);
        assert_eq!(frame_pts(25, Rational::new(25, 1)), SEC);
    }

    #[test]
    fn an_invalid_rate_is_not_a_division_by_zero() {
        assert_eq!(frame_pts(3, Rational::new(0, 1)), NONE);
        assert_eq!(frame_pts(3, Rational::new(25, 0)), NONE);
        assert_eq!(frames_in(SEC, Rational::new(-1, 1)), 0);
        assert_eq!(frame_at(SEC, Rational::new(0, 0)), 0);
    }

    #[test]
    fn frame_at_inverts_frame_pts() {
        for index in [0u64, 1, 2, 29, 30, 1_000, 123_457] {
            let pts = frame_pts(index, NTSC);
            assert_eq!(frame_at(pts, NTSC), index);
            assert_eq!(frame_at(pts + 1, NTSC), index);
            if index > 0 {
                assert_eq!(frame_at(pts - 1, NTSC), index - 1);
            }
        }
    }

    #[test]
    fn frames_in_counts_whole_frames() {
        assert_eq!(frames_in(10 * SEC, Rational::new(25, 1)), 250);
        assert_eq!(frames_in(10 * SEC - 1, Rational::new(25, 1)), 249);
        assert_eq!(frames_in(1_001 * SEC, NTSC), 30_000);
        assert_eq!(frames_in(0, NTSC), 0);
    }
}
