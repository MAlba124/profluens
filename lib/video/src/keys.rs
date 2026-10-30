//! Keyframe stamps an encoder is asked to key on, so its IDRs fall where another stream's do.

/// Whether `pts` is within `tol` ns of one of the sorted `keys`.
pub fn at_key(keys: &[i64], tol: i64, pts: i64) -> bool {
    let i = keys.partition_point(|&k| k < pts.saturating_sub(tol));
    keys.get(i).is_some_and(|&k| k <= pts.saturating_add(tol))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_near_a_listed_stamp_opens_an_idr() {
        let keys = [0i64, 2_002_000_000, 4_004_000_000];
        let tol = 20_000_000;
        assert!(at_key(&keys, tol, 0));
        assert!(at_key(&keys, tol, 2_001_999_999));
        assert!(at_key(&keys, tol, 4_024_000_000));
        assert!(!at_key(&keys, tol, 41_708_333));
        assert!(!at_key(&keys, tol, 4_024_000_001));
        assert!(!at_key(&[], tol, 0));
    }

    #[test]
    fn irregular_spacing_matches_each_key_and_nothing_between() {
        let keys = [1_000, 1_500, 9_000, 9_040, 30_000];
        let tol = 10;
        for k in keys {
            assert!(at_key(&keys, tol, k - tol) && at_key(&keys, tol, k + tol), "{k}");
            assert!(!at_key(&keys, tol, k - tol - 1), "{k} early");
        }
        assert!(!at_key(&keys, tol, 1_511) && !at_key(&keys, tol, 9_051));
        assert!(!at_key(&keys, tol, 9_020), "between two close keys");
        assert!(!at_key(&keys, tol, 5_250), "half way between two keys");
    }

    #[test]
    fn before_the_first_and_after_the_last_key_never_match() {
        let keys = [5_000, 10_000];
        assert!(!at_key(&keys, 100, 0) && !at_key(&keys, 100, -7) && !at_key(&keys, 100, 4_899));
        assert!(!at_key(&keys, 100, 10_101) && !at_key(&keys, 100, i64::MAX));
        assert!(at_key(&keys, 0, 10_000) && !at_key(&keys, 0, 10_001), "zero tolerance");
        assert!(!at_key(&[i64::MIN, i64::MAX], 5, 0), "no overflow at the ends");
        assert!(at_key(&[i64::MAX], 5, i64::MAX - 3) && at_key(&[i64::MIN], 5, i64::MIN));
    }
}
