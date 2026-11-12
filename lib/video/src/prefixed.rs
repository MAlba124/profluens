//! NAL units stored length prefixed (ISO/IEC 14496-15), the samples of an `h264/avcc` or
//! `h265/hvcc` stream and the parameter sets of the configuration record that leads it.
//! Bounds only, no byte of a unit is read.

/// How a NAL decoder's input is framed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Framing {
    /// Start codes (Annex B).
    #[default]
    AnnexB,
    /// Length fields of `size` octets, 4 until a configuration record states it. The record
    /// is the first buffer behind the family's announcement, `record` until it came, and any
    /// buffer flagged a head.
    Lengths { size: u8, record: bool },
}

impl Framing {
    /// The framing of a stream linked as `family`, Annex B for a family that names none.
    pub fn of(family: Option<&str>) -> Framing {
        match family {
            Some("h264/avcc" | "h265/hvcc") => Framing::Lengths { size: 4, record: true },
            _ => Framing::AnnexB,
        }
    }

    /// The framing once `family` is announced. A length size already read holds until the
    /// record that follows the announcement states its own.
    pub fn turned(self, family: Option<&str>) -> Framing {
        match (self, Framing::of(family)) {
            (Framing::Lengths { size, .. }, Framing::Lengths { .. }) => {
                Framing::Lengths { size, record: true }
            }
            (_, to) => to,
        }
    }

    /// Whether units are led by lengths.
    pub fn is_lengths(self) -> bool {
        self != Framing::AnnexB
    }

    /// The framing once a buffer was read as the record, of `size` where it parsed.
    pub fn past_record(self, size: Option<u8>) -> Framing {
        match self {
            Framing::Lengths { size: old, .. } => {
                Framing::Lengths { size: size.unwrap_or(old), record: false }
            }
            Framing::AnnexB => Framing::AnnexB,
        }
    }
}

/// Calls `f` with the bounds of each NAL of a sample whose length fields are `length_size`
/// octets (ISO/IEC 14496-15 section 4.3.3). A length that runs past the sample ends the walk, so
/// a damaged sample gives the units ahead of the damage.
pub fn for_each(sample: &[u8], length_size: u8, mut f: impl FnMut(usize, usize)) {
    let size = usize::from(length_size);
    if !(1..=4).contains(&size) {
        return;
    }
    let mut at = 0usize;
    while let Some(field) = sample.get(at..at + size) {
        let len = field.iter().fold(0usize, |n, &b| (n << 8) | usize::from(b));
        let start = at + size;
        let Some(end) = start.checked_add(len).filter(|end| *end <= sample.len()) else {
            return;
        };
        f(start, end);
        at = end;
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<usize> {
    let field = b.get(at..at.checked_add(2)?)?;
    Some(usize::from(u16::from_be_bytes([field[0], field[1]])))
}

/// `count` units from `at`, each led by a 16-bit length, and where they end.
fn sets(
    record: &[u8],
    mut at: usize,
    count: usize,
    f: &mut impl FnMut(usize, usize),
) -> Option<usize> {
    for _ in 0..count {
        let start = at.checked_add(2)?;
        let end = start.checked_add(u16_at(record, at)?).filter(|end| *end <= record.len())?;
        f(start, end);
        at = end;
    }
    Some(at)
}

/// Calls `f` with the bounds of each parameter set of an `AVCDecoderConfigurationRecord`
/// (ISO/IEC 14496-15 section 5.3.3.1), the sequence sets first. The NAL length size, `None` for a
/// record of another version or cut short.
pub fn avcc(record: &[u8], mut f: impl FnMut(usize, usize)) -> Option<u8> {
    if *record.first()? != 1 {
        return None;
    }
    let length_size = (record.get(4)? & 3) + 1;
    let sps = usize::from(record.get(5)? & 0x1f);
    let at = sets(record, 6, sps, &mut f)?;
    let pps = usize::from(*record.get(at)?);
    sets(record, at + 1, pps, &mut f)?;
    Some(length_size)
}

/// Calls `f` with the bounds of each NAL of an `HEVCDecoderConfigurationRecord` (ISO/IEC
/// 14496-15 section 8.3.3.1), array by array. The NAL length size, `None` for a record of another
/// version or cut short.
pub fn hvcc(record: &[u8], mut f: impl FnMut(usize, usize)) -> Option<u8> {
    if *record.first()? != 1 {
        return None;
    }
    let length_size = (record.get(21)? & 3) + 1;
    let arrays = *record.get(22)?;
    let mut at = 23usize;
    for _ in 0..arrays {
        // array_completeness and the NAL type, then numNalus
        let count = u16_at(record, at.checked_add(1)?)?;
        at = sets(record, at + 3, count, &mut f)?;
    }
    Some(length_size)
}

#[cfg(test)]
// Test vectors built once.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn bounds(sample: &[u8], size: u8) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for_each(sample, size, |start, end| out.push((start, end)));
        out
    }

    fn sample(size: usize, nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - size..]);
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn a_sample_is_walked_by_every_length_size() {
        let nals: [&[u8]; 3] = [&[0x40, 1, 9], &[0x26, 1], &[0x02, 1, 7, 7, 7]];
        for size in 1..=4u8 {
            let s = sample(usize::from(size), &nals);
            let got = bounds(&s, size);
            let units: Vec<&[u8]> = got.iter().map(|&(a, b)| &s[a..b]).collect();
            assert_eq!(units, nals, "length size {size}");
        }
    }

    #[test]
    fn a_length_past_the_sample_ends_the_walk_with_what_came_before() {
        let mut s = sample(4, &[&[0x40, 1], &[0x26, 1, 5]]);
        s.truncate(s.len() - 1);
        assert_eq!(bounds(&s, 4), [(4, 6)], "the cut unit is dropped");
        assert_eq!(bounds(&[0xff, 0xff, 0xff, 0xff, 1], 4), [], "a length no sample holds");
        assert_eq!(bounds(&[0, 0, 1], 4), [], "a length field cut short");
        assert_eq!(bounds(&[0, 0, 0, 0], 4), [(4, 4)], "an empty unit is left to the caller");
    }

    #[test]
    fn a_length_size_no_record_states_walks_nothing() {
        let s = sample(4, &[&[0x40, 1]]);
        assert_eq!(bounds(&s, 0), []);
        assert_eq!(bounds(&s, 5), []);
    }

    #[test]
    fn any_bytes_walk_within_their_bounds() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..2000 {
            let len = (next() % 96) as usize;
            // short lengths mostly, so walks go several units deep
            let bytes: Vec<u8> = (0..len)
                .map(|_| if next() % 3 == 0 { next() as u8 } else { (next() % 6) as u8 })
                .collect();
            for size in 0..=5u8 {
                let mut last = 0;
                for_each(&bytes, size, |start, end| {
                    assert!(last <= start && start <= end && end <= bytes.len(), "round {round}");
                    last = end;
                });
            }
            let mut within = |start: usize, end: usize| assert!(start <= end && end <= bytes.len());
            let _ = avcc(&bytes, &mut within);
            let _ = hvcc(&bytes, &mut within);
        }
    }

    fn avcc_record(size: u8, sps: &[&[u8]], pps: &[&[u8]]) -> Vec<u8> {
        let mut r = vec![1, 100, 0, 31, 0xfc | (size - 1), 0xe0 | sps.len() as u8];
        for set in sps {
            r.extend_from_slice(&(set.len() as u16).to_be_bytes());
            r.extend_from_slice(set);
        }
        r.push(pps.len() as u8);
        for set in pps {
            r.extend_from_slice(&(set.len() as u16).to_be_bytes());
            r.extend_from_slice(set);
        }
        r
    }

    fn hvcc_record(size: u8, arrays: &[(u8, &[&[u8]])]) -> Vec<u8> {
        let mut r = vec![0u8; 23];
        (r[0], r[21], r[22]) = (1, 0xfc | (size - 1), arrays.len() as u8);
        for (nal_type, nals) in arrays {
            r.push(0x80 | nal_type);
            r.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for nal in *nals {
                r.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                r.extend_from_slice(nal);
            }
        }
        r
    }

    #[test]
    fn an_avcc_record_gives_its_sets_and_length_size() {
        let (sps, pps): ([&[u8]; 1], [&[u8]; 2]) = ([&[0x67, 1, 2]], [&[0x68, 3], &[0x68, 4, 5]]);
        let r = avcc_record(2, &sps, &pps);
        let mut got: Vec<&[u8]> = Vec::new();
        assert_eq!(avcc(&r, |a, b| got.push(&r[a..b])), Some(2));
        assert_eq!(got, [sps[0], pps[0], pps[1]]);
        // the High profile fields that may follow are not parameter sets
        let mut long = r.clone();
        long.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
        let mut n = 0;
        assert_eq!(avcc(&long, |_, _| n += 1), Some(2));
        assert_eq!(n, 3);
    }

    #[test]
    fn an_hvcc_record_gives_its_arrays_and_length_size() {
        let (vps, sps, pps): (&[u8], &[u8], &[u8]) = (&[0x40, 1, 9], &[0x42, 1, 8, 8], &[0x44, 1]);
        let r = hvcc_record(4, &[(32, &[vps]), (33, &[sps]), (34, &[pps, pps])]);
        let mut got: Vec<&[u8]> = Vec::new();
        assert_eq!(hvcc(&r, |a, b| got.push(&r[a..b])), Some(4));
        assert_eq!(got, [vps, sps, pps, pps]);
        assert_eq!(hvcc(&hvcc_record(1, &[]), |_, _| {}), Some(1), "a record without arrays");
    }

    #[test]
    fn a_record_cut_short_or_of_another_version_is_refused() {
        let a = avcc_record(4, &[&[0x67, 1, 2]], &[&[0x68, 3]]);
        let h = hvcc_record(4, &[(32, &[&[0x40, 1, 9]]), (33, &[&[0x42, 1]])]);
        for cut in 0..a.len() {
            assert_eq!(avcc(&a[..cut], |_, _| {}), None, "avcC cut at {cut}");
        }
        for cut in 0..h.len() {
            assert_eq!(hvcc(&h[..cut], |_, _| {}), None, "hvcC cut at {cut}");
        }
        let (mut a0, mut h0) = (a.clone(), h.clone());
        (a0[0], h0[0]) = (0, 2);
        assert_eq!(avcc(&a0, |_, _| {}), None);
        assert_eq!(hvcc(&h0, |_, _| {}), None);
    }

    #[test]
    fn a_family_names_its_framing_and_a_read_length_size_is_kept() {
        let due = Framing::Lengths { size: 4, record: true };
        assert_eq!(Framing::of(Some("h264/avcc")), due);
        assert_eq!(Framing::of(Some("h265/hvcc")), due);
        assert_eq!(Framing::of(Some("h265/annexb")), Framing::AnnexB);
        assert_eq!(Framing::of(None), Framing::AnnexB);
        assert!(due.is_lengths() && !Framing::AnnexB.is_lengths());
        let read = due.past_record(Some(2));
        assert_eq!(read, Framing::Lengths { size: 2, record: false });
        assert_eq!(due.past_record(None), Framing::Lengths { size: 4, record: false });
        let again = Framing::Lengths { size: 2, record: true };
        assert_eq!(read.turned(Some("h265/hvcc")), again, "announced again, a record follows");
        assert_eq!(again.past_record(None), read, "one that does not parse leaves the size");
        assert_eq!(read.turned(Some("h265/annexb")), Framing::AnnexB);
        assert_eq!(Framing::AnnexB.turned(Some("h264/avcc")), due);
        assert_eq!(Framing::AnnexB.past_record(Some(2)), Framing::AnnexB);
    }
}
