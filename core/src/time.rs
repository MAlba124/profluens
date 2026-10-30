/// Nanoseconds since an unspecified epoch, with `u64::MAX` as the unset sentinel that sorts last.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub const NONE: Self = Self(u64::MAX);
    pub const ZERO: Self = Self(0);
    /// Largest valid time, one below the sentinel.
    pub const MAX: Self = Self(u64::MAX - 1);

    pub const fn from_nanos(ns: u64) -> Self {
        Self(ns)
    }

    pub const fn from_micros(us: u64) -> Self {
        Self(us.saturating_mul(1_000))
    }

    pub const fn from_millis(ms: u64) -> Self {
        Self(ms.saturating_mul(1_000_000))
    }

    pub const fn from_secs(s: u64) -> Self {
        Self(s.saturating_mul(1_000_000_000))
    }

    pub const fn is_none(self) -> bool {
        self.0 == u64::MAX
    }

    pub const fn is_some(self) -> bool {
        self.0 != u64::MAX
    }

    pub const fn nanos(self) -> Option<u64> {
        if self.is_none() { None } else { Some(self.0) }
    }

    /// Saturating add that propagates `NONE` and never overflows into the sentinel.
    pub const fn saturating_add(self, rhs: Self) -> Self {
        if self.is_none() || rhs.is_none() {
            return Self::NONE;
        }
        match self.0.checked_add(rhs.0) {
            Some(sum) if sum < u64::MAX => Self(sum),
            _ => Self::MAX,
        }
    }

    /// Saturating subtract that propagates `NONE` and clamps at zero.
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        if self.is_none() || rhs.is_none() {
            return Self::NONE;
        }
        Self(self.0.saturating_sub(rhs.0))
    }
}

pub(crate) const BASE_UNSET: i64 = i64::MIN;

pub(crate) fn base_from_clock(ts: Timestamp) -> i64 {
    ts.0.min(i64::MAX as u64) as i64
}

pub(crate) fn running_time(clock_ns: u64, base: i64) -> Timestamp {
    Timestamp((clock_ns as i128 - base as i128).clamp(0, Timestamp::MAX.0 as i128) as u64)
}

/// `base + running + latency` on the raw clock, clamped to `[0, MAX]`, `NONE` propagates.
pub(crate) fn deadline(base: i64, running: Timestamp, latency: Timestamp) -> Timestamp {
    if running.is_none() || latency.is_none() {
        return Timestamp::NONE;
    }
    let d = base as i128 + running.0 as i128 + latency.0 as i128;
    Timestamp(d.clamp(0, Timestamp::MAX.0 as i128) as u64)
}

/// Exact rational, for rates like 30000/1001 fps.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub const fn new(num: i32, den: i32) -> Self {
        Self { num, den }
    }

    pub const fn is_valid(self) -> bool {
        self.den != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Rate {
    num: u16,
    den: u16,
}

/// Why [`Rate::new`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RateError {
    Zero,
    Negative,
    OutOfRange,
    TooFine,
}

impl std::fmt::Display for RateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RateError::Zero => "a rate of zero",
            RateError::Negative => "a negative rate, reverse playback is not built",
            RateError::OutOfRange => "a rate outside 1/64 to 64",
            RateError::TooFine => "a rate whose reduced terms pass 65535",
        })
    }
}

impl std::error::Error for RateError {}

impl Rate {
    pub const ONE: Rate = Rate { num: 1, den: 1 };
    /// The fastest rate, and one over the slowest.
    pub const LIMIT: u64 = 64;

    /// `num / den` in lowest terms.
    pub fn new(num: i64, den: i64) -> Result<Rate, RateError> {
        let (n, d) = (num.unsigned_abs(), den.unsigned_abs());
        let (mut a, mut b) = (n, d);
        for _ in 0..96 {
            (a, b) = if b == 0 { (a, b) } else { (b, a % b) };
        }
        let (n, d) = (n / a.max(1), d / a.max(1));
        match (u16::try_from(n), u16::try_from(d)) {
            _ if n == 0 || d == 0 => Err(RateError::Zero),
            _ if (num < 0) != (den < 0) => Err(RateError::Negative),
            _ if n.div_ceil(Self::LIMIT) > d || d.div_ceil(Self::LIMIT) > n => {
                Err(RateError::OutOfRange)
            }
            (Ok(num), Ok(den)) => Ok(Rate { num, den }),
            _ => Err(RateError::TooFine),
        }
    }

    pub const fn num(self) -> u16 {
        self.num
    }

    pub const fn den(self) -> u16 {
        self.den
    }

    pub(crate) const fn pack(self) -> u64 {
        self.num as u64 | (self.den as u64) << 16
    }

    /// A zero term reads as one so an unset word still divides.
    pub(crate) const fn unpack(word: u64) -> Rate {
        let (num, den) = (word as u16, (word >> 16) as u16);
        Rate { num: if num == 0 { 1 } else { num }, den: if den == 0 { 1 } else { den } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_is_a_positive_rational_in_lowest_terms_within_its_limits() {
        let r = Rate::new(3, 2).unwrap();
        assert_eq!((r.num(), r.den()), (3, 2));
        assert_eq!(Rate::new(150, 100), Ok(r), "reduced");
        assert_eq!(Rate::new(-3, -2), Ok(r), "two signs are none");
        assert_eq!(Rate::new(1, 1), Ok(Rate::ONE));
        assert_eq!(Rate::new(0, 1), Err(RateError::Zero));
        assert_eq!(Rate::new(1, 0), Err(RateError::Zero));
        assert_eq!(Rate::new(-1, 2), Err(RateError::Negative));
        assert_eq!(Rate::new(1, -2), Err(RateError::Negative));
        assert_eq!(Rate::new(64, 1).map(Rate::num), Ok(64));
        assert_eq!(Rate::new(1, 64).map(Rate::den), Ok(64));
        assert_eq!(Rate::new(65, 1), Err(RateError::OutOfRange));
        assert_eq!(Rate::new(1, 65), Err(RateError::OutOfRange));
        assert_eq!(Rate::new(65_537, 65_536), Err(RateError::TooFine));
        assert_eq!(Rate::new(i64::MAX, i64::MAX), Ok(Rate::ONE));
        assert_eq!(Rate::new(i64::MIN, i64::MIN), Ok(Rate::ONE));
        assert_eq!(Rate::new(i64::MIN, 1), Err(RateError::Negative));
        assert_eq!(Rate::new(i64::MAX, 1), Err(RateError::OutOfRange));
        for (n, d) in [(1, 1), (3, 2), (65_535, 65_534), (1, 64), (64, 1)] {
            let r = Rate::new(n, d).unwrap();
            assert_eq!(Rate::unpack(r.pack()), r);
        }
        assert_eq!(Rate::unpack(0), Rate::ONE, "an empty word still divides");
    }

    #[test]
    fn conversions() {
        assert_eq!(Timestamp::from_micros(1).0, 1_000);
        assert_eq!(Timestamp::from_millis(1).0, 1_000_000);
        assert_eq!(Timestamp::from_secs(1).0, 1_000_000_000);
    }

    #[test]
    fn none_sentinel() {
        assert!(Timestamp::NONE.is_none());
        assert!(!Timestamp::NONE.is_some());
        assert!(Timestamp::ZERO.is_some());
        assert_eq!(Timestamp::NONE.nanos(), None);
        assert_eq!(Timestamp::from_nanos(5).nanos(), Some(5));
    }

    #[test]
    fn saturating_add_propagates_none_and_never_hits_sentinel() {
        assert_eq!(
            Timestamp::from_nanos(2).saturating_add(Timestamp::from_nanos(3)),
            Timestamp::from_nanos(5)
        );
        assert!(Timestamp::NONE.saturating_add(Timestamp::ZERO).is_none());
        assert!(Timestamp::ZERO.saturating_add(Timestamp::NONE).is_none());
        let big = Timestamp::MAX.saturating_add(Timestamp::MAX);
        assert_eq!(big, Timestamp::MAX);
        assert!(big.is_some());
    }

    #[test]
    fn saturating_sub_clamps_at_zero() {
        assert_eq!(
            Timestamp::from_nanos(3).saturating_sub(Timestamp::from_nanos(5)),
            Timestamp::ZERO
        );
        assert_eq!(
            Timestamp::from_nanos(5).saturating_sub(Timestamp::from_nanos(3)),
            Timestamp::from_nanos(2)
        );
        assert!(Timestamp::from_nanos(5).saturating_sub(Timestamp::NONE).is_none());
    }

    #[test]
    fn signed_base_helpers() {
        let base = -(50_000_000i64);
        assert_eq!(running_time(0, base), Timestamp::from_millis(50));
        assert_eq!(deadline(base, Timestamp::from_millis(50), Timestamp::ZERO), Timestamp::ZERO);
        assert_eq!(running_time(10, 1_000), Timestamp::ZERO);
        assert!(deadline(0, Timestamp::NONE, Timestamp::ZERO).is_none());
        assert!(deadline(0, Timestamp::ZERO, Timestamp::NONE).is_none());
        assert_eq!(deadline(i64::MAX, Timestamp::MAX, Timestamp::MAX), Timestamp::MAX);
        assert_eq!(base_from_clock(Timestamp::from_secs(3)), 3_000_000_000);
    }

    #[test]
    fn rational() {
        assert!(Rational::new(30000, 1001).is_valid());
        assert!(!Rational::new(1, 0).is_valid());
    }
}
