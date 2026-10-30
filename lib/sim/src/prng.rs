//! Seeded splitmix64 with labelled substreams (Steele, Lea and Flood, OOPSLA 2014). A substream is
//! keyed by seed and label, never by draw count, so a new draw shifts nobody else's.

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

pub const fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const fn fnv1a(label: &str) -> u64 {
    let bytes = label.as_bytes();
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    let mut i = 0;
    while i < bytes.len() {
        hash = (hash ^ bytes[i] as u64).wrapping_mul(0x0000_0100_0000_01B3);
        i += 1;
    }
    hash
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prng {
    key: u64,
    state: u64,
}

impl Prng {
    pub const fn new(seed: u64) -> Self {
        Self { key: seed, state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        mix(self.state)
    }

    /// Uniform in `0..n` with no modulo bias (Lemire, ACM TOMACS 29(1), 2019). The rejection loop
    /// is capped so it cannot hang a run.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut product = self.next_u64() as u128 * n as u128;
        if (product as u64) < n {
            let threshold = n.wrapping_neg() % n;
            let mut retries = 0;
            while (product as u64) < threshold && retries < 64 {
                product = self.next_u64() as u128 * n as u128;
                retries += 1;
            }
        }
        (product >> 64) as u64
    }

    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.below(hi - lo)
    }

    /// Always one draw, so 0 and 100 keep the stream aligned with every other percentage.
    pub fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent as u64
    }

    pub const fn substream(&self, label: u64) -> Prng {
        Prng::new(mix(self.key ^ mix(label.wrapping_add(GAMMA))))
    }

    pub const fn substream_named(&self, label: &str) -> Prng {
        self.substream(fnv1a(label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_reference_vectors() {
        // First outputs of Vigna's splitmix64.c for seed 0 and for the seed below.
        let mut zero = Prng::new(0);
        assert_eq!(zero.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(zero.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(zero.next_u64(), 0x06C4_5D18_8009_454F);
        let mut seeded = Prng::new(1_234_567);
        assert_eq!(seeded.next_u64(), 6_457_827_717_110_365_317);
        assert_eq!(seeded.next_u64(), 3_203_168_211_198_807_973);
    }

    #[test]
    fn same_seed_same_sequence_and_other_seed_diverges() {
        let mut left = Prng::new(42);
        let mut right = Prng::new(42);
        let mut other = Prng::new(43);
        let mut differs = false;
        for _ in 0..64 {
            let value = left.next_u64();
            assert_eq!(value, right.next_u64());
            differs |= value != other.next_u64();
        }
        assert!(differs);
    }

    #[test]
    fn below_stays_in_bounds_and_covers_the_range() {
        let mut prng = Prng::new(0xDEAD_BEEF);
        for n in [1u64, 2, 3, 7, 100, 1 << 33, u64::MAX] {
            for _ in 0..2_000 {
                assert!(prng.below(n) < n);
            }
        }
        let mut seen = [0u32; 7];
        for _ in 0..7_000 {
            seen[prng.below(7) as usize] += 1;
        }
        // Each of 7 buckets expects 1000. A broken reduction misses by far more.
        assert!(seen.iter().all(|&count| (800..1200).contains(&count)), "{seen:?}");
    }

    #[test]
    fn range_handles_empty_and_inverted_bounds_without_drawing() {
        let mut prng = Prng::new(5);
        let before = prng;
        assert_eq!(prng.range(5, 5), 5);
        assert_eq!(prng.range(7, 3), 7);
        assert_eq!(prng.below(0), 0);
        assert_eq!(prng, before);
        for _ in 0..1_000 {
            assert!((10..20).contains(&prng.range(10, 20)));
        }
    }

    #[test]
    fn chance_follows_the_percent_and_always_draws_once() {
        let mut prng = Prng::new(9);
        assert!((0..1_000).all(|_| !prng.chance(0)));
        assert!((0..1_000).all(|_| prng.chance(100)));
        let hits = (0..10_000).filter(|_| prng.chance(30)).count();
        assert!((2_700..3_300).contains(&hits), "{hits} of 10000 at 30 percent");

        let mut a = Prng::new(11);
        let mut b = Prng::new(11);
        a.chance(0);
        b.next_u64();
        assert_eq!(a, b);
    }

    #[test]
    fn a_substream_ignores_draws_made_elsewhere() {
        let quiet = Prng::new(77);
        let mut expected = quiet.substream(3);
        let want: [u64; 8] = core::array::from_fn(|_| expected.next_u64());

        let mut busy = Prng::new(77);
        for _ in 0..1_000 {
            busy.next_u64();
        }
        let mut sibling = busy.substream(2);
        let mut named = busy.substream_named("jitter");
        for _ in 0..100 {
            sibling.next_u64();
            named.below(10);
        }
        let mut got = busy.substream(3);
        let have: [u64; 8] = core::array::from_fn(|_| got.next_u64());
        assert_eq!(want, have);
    }

    #[test]
    fn substreams_differ_by_label_seed_and_from_their_parent() {
        let root = Prng::new(9);
        let mut firsts = [0u64; 64];
        for (label, slot) in firsts.iter_mut().enumerate() {
            *slot = root.substream(label as u64).next_u64();
        }
        for i in 0..firsts.len() {
            for j in 0..i {
                assert_ne!(firsts[i], firsts[j], "labels {i} and {j} collide");
            }
        }
        assert_ne!(root.substream(0).next_u64(), Prng::new(9).next_u64());
        assert_ne!(root.substream(1).next_u64(), Prng::new(10).substream(1).next_u64());
        assert_ne!(
            root.substream_named("source").next_u64(),
            root.substream_named("sink").next_u64()
        );
    }

    #[test]
    fn nested_substreams_depend_on_the_whole_path() {
        let root = Prng::new(1);
        let a = root.substream(1).substream(2).next_u64();
        let b = root.substream(2).substream(1).next_u64();
        let again = root.substream(1).substream(2).next_u64();
        assert_ne!(a, b);
        assert_eq!(a, again);
    }
}
