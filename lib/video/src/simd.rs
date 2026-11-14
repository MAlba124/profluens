// Vector bodies for the decoder's pack loops. Each entry packs the leading part of a row it can do
// whole and returns how many samples or sites it wrote. The caller's scalar code does the rest, and
// all of it where the CPU has no AVX2.

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use fearless_simd::x86::Avx2;

/// The vector unit the pack loops run on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lanes {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    avx2: Option<Avx2>,
}

impl Lanes {
    pub(crate) const SCALAR: Lanes = Lanes {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        avx2: None,
    };

    // fearless_simd probes the CPU on first use and caches the level.
    pub(crate) fn detect() -> Lanes {
        Lanes {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            avx2: fearless_simd::Level::new().as_avx2(),
        }
    }

    pub(crate) fn has_avx2(self) -> bool {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        return self.avx2.is_some();
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        false
    }
}

/// The deepest source whose 1 2 1 sum over two rows, rounding included, fits in 16 bits.
pub(crate) const TAPS_MAX_BITS: u8 = 13;

/// A chroma row pair, as `Fit::rows_of` hands it.
pub(crate) type Rows<'a> = (&'a [u8], &'a [u8]);

macro_rules! on_avx2 {
    ($lanes:expr, $s:ident => $body:expr) => {{
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if let Some($s) = $lanes.avx2 {
            return $body;
        }
        0
    }};
}

/// LE 16-bit samples shifted up by `up`. Samples done.
pub(crate) fn shift(lanes: Lanes, dst: &mut [u8], src: &[u8], up: u32) -> usize {
    on_avx2!(lanes, s => avx2::shift(s, dst, src, up))
}

/// LE 16-bit U and V shifted up by `up` and interleaved. Sample pairs done.
pub(crate) fn interleave_shift(lanes: Lanes, dst: &mut [u8], u: &[u8], v: &[u8], up: u32) -> usize {
    on_avx2!(lanes, s => avx2::interleave_shift(s, dst, u, v, up))
}

/// 8-bit U and V interleaved. Sample pairs done.
pub(crate) fn interleave(lanes: Lanes, dst: &mut [u8], u: &[u8], v: &[u8]) -> usize {
    on_avx2!(lanes, s => avx2::interleave(s, dst, u, v))
}

/// LE 16-bit samples rounded down `down` bits to 8. Samples done.
pub(crate) fn narrow(lanes: Lanes, dst: &mut [u8], src: &[u8], down: u32) -> usize {
    on_avx2!(lanes, s => avx2::narrow(s, dst, src, down))
}

/// P010 chroma sites from deep U and V, `taps` for the 1 2 1 across, else the columns as they are.
/// Sites done.
pub(crate) fn fit_p010(
    lanes: Lanes,
    taps: bool,
    bits: u8,
    u: Rows<'_>,
    v: Rows<'_>,
    dst: &mut [u8],
    up: u32,
) -> usize {
    if taps && bits > TAPS_MAX_BITS {
        return 0;
    }
    on_avx2!(lanes, s => match taps {
        true => avx2::taps_p010(s, u, v, dst, up),
        false => avx2::avg_p010(s, u, v, dst, up),
    })
}

/// 8-bit chroma sites from 8-bit rows. Sites done.
pub(crate) fn fit8(lanes: Lanes, taps: bool, rows: Rows<'_>, dst: &mut [u8]) -> usize {
    on_avx2!(lanes, s => match taps {
        true => avx2::taps8(s, rows, dst),
        false => avx2::avg8(s, rows, dst),
    })
}

/// 8-bit chroma sites from deep rows, rounded down `down` bits. Sites done.
pub(crate) fn fit_narrow(
    lanes: Lanes,
    taps: bool,
    bits: u8,
    rows: Rows<'_>,
    dst: &mut [u8],
    down: u32,
) -> usize {
    if taps && bits > TAPS_MAX_BITS {
        return 0;
    }
    on_avx2!(lanes, s => match taps {
        true => avx2::taps_narrow(s, rows, dst, down),
        false => avx2::avg_narrow(s, rows, dst, down),
    })
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod avx2 {
    use fearless_simd::prelude::*;
    use fearless_simd::x86::Avx2;
    use fearless_simd::{u8x32, u16x16};

    #[cfg(target_arch = "x86")]
    use std::arch::x86::{_mm256_avg_epu8, _mm256_avg_epu16};
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::{_mm256_avg_epu8, _mm256_avg_epu16};

    use super::Rows;

    type W = u16x16<Avx2>;
    type B = u8x32<Avx2>;

    // (a + b + 1) >> 1 without the carry out, as the scalar code rounds the row mean.
    fearless_simd::kernel!(
        #[inline(always)]
        fn avg_w(s: Avx2, a: W, b: W) -> W {
            _mm256_avg_epu16(a.into(), b.into()).simd_into(s)
        }
    );

    fearless_simd::kernel!(
        #[inline(always)]
        fn avg_b(s: Avx2, a: B, b: B) -> B {
            _mm256_avg_epu8(a.into(), b.into()).simd_into(s)
        }
    );

    #[inline(always)]
    fn load_b(s: Avx2, bytes: &[u8]) -> B {
        B::from_slice(s, &bytes[..32])
    }

    #[inline(always)]
    fn load_w(s: Avx2, bytes: &[u8]) -> W {
        load_b(s, bytes).bitcast()
    }

    #[inline(always)]
    fn store_w(v: W, dst: &mut [u8]) {
        v.bitcast::<B>().store_slice(&mut dst[..32]);
    }

    #[inline(always)]
    fn store_pair(s: Avx2, u: W, v: W, dst: &mut [u8]) {
        let (lo, hi) = s.interleave_u16x16(u, v);
        let (a, b) = dst.split_at_mut(32);
        store_w(lo, a);
        store_w(hi, b);
    }

    // As `narrow` in the decoder. The saturating add stands in for the u32 sum, both clamp to 255.
    #[inline(always)]
    fn round_down(s: Avx2, lo: W, hi: W, down: u32) -> B {
        let half = W::splat(s, (1u16 << down) >> 1);
        s.saturating_narrow_u16x16(lo.saturating_add(half) >> down, hi.saturating_add(half) >> down)
    }

    // 16 sites from the row sums of 32 columns, `carry` holding the sum left of the first.
    #[inline(always)]
    fn taps16(s: Avx2, s0: W, s1: W, carry: &mut W) -> W {
        let (even, odd) = s.deinterleave_u16x16(s0, s1);
        let left = s.slide_u16x16::<15>(*carry, odd);
        *carry = odd;
        (left + odd + even + even + 4u16) >> 3
    }

    // The clamped left tap of site 0 is column 0 itself.
    #[inline(always)]
    fn first_sum(s: Avx2, (a, b): Rows<'_>, wide: bool) -> W {
        let (a, b) = match wide {
            true => (u16::from_le_bytes([a[0], a[1]]), u16::from_le_bytes([b[0], b[1]])),
            false => (u16::from(a[0]), u16::from(b[0])),
        };
        W::splat(s, a.wrapping_add(b))
    }

    #[inline(always)]
    fn sum_w(s: Avx2, a: &[u8], b: &[u8]) -> W {
        load_w(s, a) + load_w(s, b)
    }

    pub(super) fn shift(s: Avx2, dst: &mut [u8], src: &[u8], up: u32) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<32>();
                let (src, _) = src.as_chunks::<32>();
                for (d, x) in d.iter_mut().zip(src) {
                    store_w(load_w(s, x) << up, d);
                }
                16 * d.len().min(src.len())
            },
        )
    }

    pub(super) fn interleave_shift(s: Avx2, dst: &mut [u8], u: &[u8], v: &[u8], up: u32) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<64>();
                let (u, _) = u.as_chunks::<32>();
                let (v, _) = v.as_chunks::<32>();
                for ((d, u), v) in d.iter_mut().zip(u).zip(v) {
                    store_pair(s, load_w(s, u) << up, load_w(s, v) << up, d);
                }
                16 * d.len().min(u.len()).min(v.len())
            },
        )
    }

    pub(super) fn interleave(s: Avx2, dst: &mut [u8], u: &[u8], v: &[u8]) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<64>();
                let (u, _) = u.as_chunks::<32>();
                let (v, _) = v.as_chunks::<32>();
                for ((d, u), v) in d.iter_mut().zip(u).zip(v) {
                    let (lo, hi) = s.interleave_u8x32(load_b(s, u), load_b(s, v));
                    let (a, b) = d.split_at_mut(32);
                    lo.store_slice(a);
                    hi.store_slice(b);
                }
                32 * d.len().min(u.len()).min(v.len())
            },
        )
    }

    pub(super) fn narrow(s: Avx2, dst: &mut [u8], src: &[u8], down: u32) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<32>();
                let (src, _) = src.as_chunks::<64>();
                for (d, x) in d.iter_mut().zip(src) {
                    round_down(s, load_w(s, x), load_w(s, &x[32..]), down).store_slice(d);
                }
                32 * d.len().min(src.len())
            },
        )
    }

    pub(super) fn taps_p010(s: Avx2, u: Rows<'_>, v: Rows<'_>, dst: &mut [u8], up: u32) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<64>();
                let rows = [u.0, u.1, v.0, v.1].map(|r| r.as_chunks::<64>().0);
                let n = rows.iter().fold(d.len(), |n, r| n.min(r.len()));
                if n == 0 {
                    return 0;
                }
                let (mut cu, mut cv) = (first_sum(s, u, true), first_sum(s, v, true));
                for (i, d) in d[..n].iter_mut().enumerate() {
                    let (ua, ub, va, vb) = (&rows[0][i], &rows[1][i], &rows[2][i], &rows[3][i]);
                    let su = taps16(s, sum_w(s, ua, ub), sum_w(s, &ua[32..], &ub[32..]), &mut cu);
                    let sv = taps16(s, sum_w(s, va, vb), sum_w(s, &va[32..], &vb[32..]), &mut cv);
                    store_pair(s, su << up, sv << up, d);
                }
                16 * n
            },
        )
    }

    pub(super) fn avg_p010(s: Avx2, u: Rows<'_>, v: Rows<'_>, dst: &mut [u8], up: u32) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<64>();
                let rows = [u.0, u.1, v.0, v.1].map(|r| r.as_chunks::<32>().0);
                let n = rows.iter().fold(d.len(), |n, r| n.min(r.len()));
                for (i, d) in d[..n].iter_mut().enumerate() {
                    let u = avg_w(s, load_w(s, &rows[0][i]), load_w(s, &rows[1][i]));
                    let v = avg_w(s, load_w(s, &rows[2][i]), load_w(s, &rows[3][i]));
                    store_pair(s, u << up, v << up, d);
                }
                16 * n
            },
        )
    }

    pub(super) fn taps8(s: Avx2, (a, b): Rows<'_>, dst: &mut [u8]) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<32>();
                let (ra, rb) = (a.as_chunks::<64>().0, b.as_chunks::<64>().0);
                let n = d.len().min(ra.len()).min(rb.len());
                if n == 0 {
                    return 0;
                }
                let mut carry = first_sum(s, (a, b), false);
                for ((d, a), b) in d[..n].iter_mut().zip(ra).zip(rb) {
                    let (a0, a1) = s.widen_u8x32(load_b(s, a));
                    let (a2, a3) = s.widen_u8x32(load_b(s, &a[32..]));
                    let (b0, b1) = s.widen_u8x32(load_b(s, b));
                    let (b2, b3) = s.widen_u8x32(load_b(s, &b[32..]));
                    let lo = taps16(s, a0 + b0, a1 + b1, &mut carry);
                    let hi = taps16(s, a2 + b2, a3 + b3, &mut carry);
                    s.narrow_u16x16(lo, hi).store_slice(d);
                }
                32 * n
            },
        )
    }

    pub(super) fn avg8(s: Avx2, (a, b): Rows<'_>, dst: &mut [u8]) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<32>();
                let (ra, rb) = (a.as_chunks::<32>().0, b.as_chunks::<32>().0);
                for ((d, a), b) in d.iter_mut().zip(ra).zip(rb) {
                    avg_b(s, load_b(s, a), load_b(s, b)).store_slice(d);
                }
                32 * d.len().min(ra.len()).min(rb.len())
            },
        )
    }

    pub(super) fn taps_narrow(s: Avx2, (a, b): Rows<'_>, dst: &mut [u8], down: u32) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<32>();
                let (ra, rb) = (a.as_chunks::<128>().0, b.as_chunks::<128>().0);
                let n = d.len().min(ra.len()).min(rb.len());
                if n == 0 {
                    return 0;
                }
                let mut carry = first_sum(s, (a, b), true);
                for ((d, a), b) in d[..n].iter_mut().zip(ra).zip(rb) {
                    let lo = taps16(s, sum_w(s, a, b), sum_w(s, &a[32..], &b[32..]), &mut carry);
                    let hi = taps16(
                        s,
                        sum_w(s, &a[64..], &b[64..]),
                        sum_w(s, &a[96..], &b[96..]),
                        &mut carry,
                    );
                    round_down(s, lo, hi, down).store_slice(d);
                }
                32 * n
            },
        )
    }

    pub(super) fn avg_narrow(s: Avx2, (a, b): Rows<'_>, dst: &mut [u8], down: u32) -> usize {
        s.vectorize(
            #[inline(always)]
            || {
                let (d, _) = dst.as_chunks_mut::<32>();
                let (ra, rb) = (a.as_chunks::<64>().0, b.as_chunks::<64>().0);
                for ((d, a), b) in d.iter_mut().zip(ra).zip(rb) {
                    let lo = avg_w(s, load_w(s, a), load_w(s, b));
                    let hi = avg_w(s, load_w(s, &a[32..]), load_w(s, &b[32..]));
                    round_down(s, lo, hi, down).store_slice(d);
                }
                32 * d.len().min(ra.len()).min(rb.len())
            },
        )
    }
}

#[cfg(test)]
// Test pictures, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::Lanes;
    use crate::decoder::{HeldLayout, PlaneRef, pack_nv12_on, pack_p010_on, pack_reduced_on};
    use crate::format::PixelFormat;

    const BITS: [u8; 7] = [8, 9, 10, 12, 13, 14, 16];
    const SHAPES: [(u8, u8); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 24) as u32
        }
    }

    struct Plane {
        bytes: Vec<u8>,
        stride: usize,
    }

    // Samples below 1 << bits, random garbage in the row padding.
    fn plane(rng: &mut Rng, cols: usize, rows: usize, bits: u8) -> Plane {
        let size = 1 + usize::from(bits > 8);
        let stride = size * cols + (rng.next() % 41) as usize;
        let mask = ((1u64 << bits) - 1) as u32;
        let mut bytes: Vec<u8> = (0..stride * rows).map(|_| rng.next() as u8).collect();
        for row in bytes.chunks_exact_mut(stride) {
            for s in row[..size * cols].chunks_exact_mut(size) {
                s.copy_from_slice(&(rng.next() & mask).to_le_bytes()[..size]);
            }
        }
        Plane { bytes, stride }
    }

    fn picture(rng: &mut Rng, (w, h): (usize, usize), shift: (u8, u8), bits: u8) -> [Plane; 3] {
        let (cw, ch) = (w.div_ceil(1 << shift.0), h.div_ceil(1 << shift.1));
        [plane(rng, w, h, bits), plane(rng, cw, ch, bits), plane(rng, cw, ch, bits)]
    }

    fn refs(p: &[Plane; 3]) -> [PlaneRef<'_>; 3] {
        p.each_ref().map(|p| PlaneRef { data: &p.bytes, stride: p.stride })
    }

    // Every width to 70 and a few large odd ones, both row parities.
    fn sizes() -> impl Iterator<Item = (usize, usize)> {
        let small = (1..=70).flat_map(|w| (1..=4).map(move |h| (w, h)));
        let large = [127, 255, 1023, 1917, 3839].into_iter().flat_map(|w| [(w, 3), (w, 4)]);
        small.chain(large)
    }

    fn avx2() -> Option<Lanes> {
        let lanes = Lanes::detect();
        if !lanes.has_avx2() {
            eprintln!("no AVX2 on this CPU, the vector path goes untested");
        }
        lanes.has_avx2().then_some(lanes)
    }

    // Both paths write the same bytes and nothing past the picture.
    fn same<E: std::fmt::Debug>(
        avx2: Lanes,
        what: &str,
        len: usize,
        run: impl Fn(Lanes, &mut [u8]) -> Result<usize, E>,
    ) {
        let mut want = vec![0xA5; len + 64];
        let mut got = want.clone();
        let n = run(Lanes::SCALAR, &mut want).expect("scalar packs");
        assert_eq!(run(avx2, &mut got).expect("AVX2 packs"), n, "{what}: bytes written");
        if let Some(i) = (0..want.len()).find(|&i| want[i] != got[i]) {
            panic!("{what}: byte {i} of {n} is {} on AVX2, {} on scalar", got[i], want[i]);
        }
    }

    #[test]
    fn reduced_packs_match_scalar_for_every_shape_depth_and_width() {
        let Some(avx2) = avx2() else { return };
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for (w, h) in sizes() {
            let len = 2 * (w * h + 2 * w.div_ceil(2) * h.div_ceil(2));
            for shift in SHAPES {
                for bits in BITS {
                    let pic = picture(&mut rng, (w, h), shift, bits);
                    let planes = refs(&pic);
                    let tos = [PixelFormat::I420, PixelFormat::P010];
                    for to in tos.into_iter().filter(|&to| bits > 8 || to == PixelFormat::I420) {
                        let what = format!("{w}x{h} {shift:?} {bits}-bit to {to:?}");
                        same(avx2, &what, len, |lanes, d| {
                            let dims = (w as u32, h as u32);
                            pack_reduced_on(lanes, d, dims, shift, bits, to, &planes, None)
                        });
                    }
                }
            }
        }
    }

    #[test]
    fn p010_packs_match_scalar_for_every_width() {
        let Some(avx2) = avx2() else { return };
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for (w, h) in sizes() {
            let len = 2 * (w * h + 2 * w.div_ceil(2) * h.div_ceil(2));
            // Full 16-bit samples, the shift drops the same top bits on both paths.
            let pic = picture(&mut rng, (w, h), (1, 1), 16);
            let planes = refs(&pic);
            same(avx2, &format!("{w}x{h} P010"), len, |lanes, d| {
                pack_p010_on(lanes, d, w as u32, h as u32, &planes, None)
            });
        }
    }

    #[test]
    fn nv12_packs_match_scalar_for_every_layout_and_width() {
        let Some(avx2) = avx2() else { return };
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let layouts = [
            (HeldLayout::Native, (1, 1), 8),
            (HeldLayout::Reduce { chroma_shift: (0, 0), bits: 8 }, (0, 0), 8),
            (HeldLayout::Reduce { chroma_shift: (1, 0), bits: 8 }, (1, 0), 8),
            (HeldLayout::Reduce { chroma_shift: (0, 0), bits: 10 }, (0, 0), 10),
            (HeldLayout::Planar10, (1, 1), 10),
        ];
        for (w, h) in sizes() {
            let len = w.next_multiple_of(2) * h.next_multiple_of(2) * 3 / 2;
            for (layout, shift, bits) in layouts {
                let pic = picture(&mut rng, (w, h), shift, bits);
                let planes = refs(&pic);
                same(avx2, &format!("{w}x{h} {layout:?} to NV12"), len, |lanes, d| {
                    let mut scratch = Vec::new();
                    let f = (layout, PixelFormat::I420);
                    pack_nv12_on(
                        lanes,
                        d,
                        Some(&mut scratch),
                        f,
                        (w as u32, h as u32),
                        &planes,
                        None,
                    )
                });
            }
        }
    }
}
