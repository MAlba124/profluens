//! Polyphase FIR resampler with a Kaiser-windowed sinc anti-aliasing low-pass.

use std::sync::Arc;

fn bessel_i0(x: f64) -> f64 {
    let half_x_sq = (x * 0.5) * (x * 0.5);
    let mut term = 1.0f64; // k = 0 term
    let mut sum = 1.0f64;
    let mut k = 1.0f64;
    loop {
        term *= half_x_sq / (k * k);
        sum += term;
        if term <= sum * 1e-12 {
            break;
        }
        k += 1.0;
    }
    sum
}

const LANES: usize = 16;

/// A designed prototype low-pass, decomposed into `L` polyphase branches.
#[derive(Clone, Debug)]
pub struct PolyphaseFilter {
    /// Upsampling factor `L` (interpolation ratio numerator).
    pub l: usize,
    /// Downsampling factor `M` (decimation ratio denominator).
    pub m: usize,
    /// Taps per polyphase branch (the per-output multiply count).
    pub taps_per_phase: usize,
    stride: usize,
    phases: Arc<[f32]>,
}

impl PolyphaseFilter {
    /// Design the anti-aliasing prototype for `in_rate` to `out_rate` and split it into branches.
    pub fn design(in_rate: u32, out_rate: u32, half_taps: usize, beta: f64) -> PolyphaseFilter {
        assert!(in_rate > 0 && out_rate > 0, "rates must be positive");
        assert!(half_taps >= 1, "need at least one tap per side");
        let g = gcd(in_rate, out_rate);
        let l = (out_rate / g) as usize;
        let m = (in_rate / g) as usize;

        let ratemax = l.max(m);
        let fc = 0.5 / ratemax as f64; // normalised cutoff (cycles/sample, upsampled)

        let taps_per_phase = 2 * half_taps;
        let n = taps_per_phase * l; // total prototype taps (kept a multiple of L)
        let center = (n - 1) as f64 / 2.0;

        let i0_beta = bessel_i0(beta);
        let mut proto = vec![0f32; n];
        for (j, tap) in proto.iter_mut().enumerate() {
            let x = j as f64 - center; // distance from centre, in upsampled samples
            let sinc = if x.abs() < 1e-9 {
                2.0 * fc
            } else {
                let a = std::f64::consts::PI * 2.0 * fc * x;
                (2.0 * fc) * (a.sin() / a)
            };
            let r = if n > 1 { (2 * j) as f64 / (n - 1) as f64 - 1.0 } else { 0.0 };
            let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            *tap = (sinc * w * l as f64) as f32;
        }

        let stride = taps_per_phase.next_multiple_of(LANES);
        let mut phases = vec![0f32; stride * l];
        for p in 0..l {
            for k in 0..taps_per_phase {
                phases[p * stride + (stride - 1 - k)] = proto[p + k * l];
            }
        }

        PolyphaseFilter { l, m, taps_per_phase, stride, phases: Arc::from(phases) }
    }

    /// Input samples one output reads, the branch length padded for the dot product.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Total prototype length, `taps_per_phase * L`.
    pub fn prototype_len(&self) -> usize {
        self.taps_per_phase * self.l
    }

    /// The full prototype coefficients, branches re-interleaved.
    pub fn prototype(&self) -> Vec<f32> {
        let mut proto = vec![0f32; self.prototype_len()];
        for p in 0..self.l {
            for k in 0..self.taps_per_phase {
                proto[p + k * self.l] = self.phases[p * self.stride + (self.stride - 1 - k)];
            }
        }
        proto
    }
}

#[inline(always)]
fn dot(c: &[f32], h: &[f32]) -> f32 {
    let (cb, _) = c.as_chunks::<LANES>();
    let (hb, _) = h.as_chunks::<LANES>();
    let mut acc = [0f32; LANES];
    for (cc, hh) in cb.iter().zip(hb) {
        for i in 0..LANES {
            acc[i] += cc[i] * hh[i];
        }
    }
    let mut a8 = [0f32; 8];
    for i in 0..8 {
        a8[i] = acc[i] + acc[i + 8];
    }
    let mut a4 = [0f32; 4];
    for i in 0..4 {
        a4[i] = a8[i] + a8[i + 4];
    }
    (a4[0] + a4[2]) + (a4[1] + a4[3])
}

/// Streaming single-channel resampler, a [`PolyphaseFilter`] plus delay line and phase state.
#[derive(Clone)]
pub struct ChannelResampler {
    filter: PolyphaseFilter,
    history: Vec<f32>,
    phase: usize,
    in_pos: usize,
    consumed: u64,
    produced: u64,
}

impl ChannelResampler {
    pub fn new(filter: PolyphaseFilter) -> ChannelResampler {
        let warmup = filter.stride.saturating_sub(1);
        ChannelResampler {
            filter,
            history: vec![0f32; warmup],
            phase: 0,
            in_pos: warmup, // first real input sits just after the warm-up zeros
            consumed: 0,
            produced: 0,
        }
    }

    pub fn filter(&self) -> &PolyphaseFilter {
        &self.filter
    }

    /// Return to the just-constructed state.
    pub fn reset(&mut self) {
        let warmup = self.filter.stride.saturating_sub(1);
        self.history.clear();
        self.history.resize(warmup, 0.0);
        self.phase = 0;
        self.in_pos = warmup;
        self.consumed = 0;
        self.produced = 0;
    }

    /// Feed `input` samples and append every output sample now computable to `out`.
    pub fn process<A: std::alloc::Allocator>(
        &mut self,
        input: &[f32],
        out: &mut Vec<f32, A>,
    ) -> usize {
        let l = self.filter.l;
        let m = self.filter.m;
        let stride = self.filter.stride;
        self.history.reserve(input.len());
        self.history.extend_from_slice(input);
        self.consumed += input.len() as u64;

        // Output count in closed form, `n = ceil((D*L - phase0) / M)`, for an exact reserve.
        let d = self.history.len().saturating_sub(self.in_pos);
        let n_out = if d == 0 { 0 } else { (d * l - self.phase).div_ceil(m) };
        out.reserve(n_out);

        {
            let phases = &self.filter.phases[..];
            let hist = &self.history[..];
            let state = (self.in_pos, self.phase);
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            let (pos, phase) = if std::is_x86_feature_detected!("avx2") {
                // SAFETY: gated on runtime AVX2 detection, the body is safe Rust with no
                // intrinsics.
                unsafe { run_avx2(phases, stride, l, m, hist, state, n_out, out) }
            } else {
                run_scalar(phases, stride, l, m, hist, state, n_out, out)
            };
            #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
            let (pos, phase) = run_scalar(phases, stride, l, m, hist, state, n_out, out);
            self.in_pos = pos;
            self.phase = phase;
        }

        let keep_from = self.in_pos.saturating_sub(stride - 1).min(self.history.len());
        if keep_from > 0 {
            self.history.drain(..keep_from);
            self.in_pos -= keep_from;
        }

        self.produced += n_out as u64;
        n_out
    }

    /// Next output position relative to the next input sample, in units of `1 / (2M)`.
    pub fn lead(&self) -> i64 {
        let (l, taps) = (self.filter.l as i64, self.filter.taps_per_phase as i64);
        let ahead = self.in_pos.saturating_sub(self.history.len()) as i64 * l + self.phase as i64;
        2 * ahead - (taps * l - 1)
    }

    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    pub fn produced(&self) -> u64 {
        self.produced
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn run_scalar<A: std::alloc::Allocator>(
    phases: &[f32],
    stride: usize,
    l: usize,
    m: usize,
    hist: &[f32],
    state: (usize, usize),
    n_out: usize,
    out: &mut Vec<f32, A>,
) -> (usize, usize) {
    let (mut pos, mut phase) = state;
    // `phase + M` never exceeds `L + M`, so a compare-and-subtract replaces the per-output div.
    let (m_div_l, m_mod_l) = (m / l, m % l);
    for _ in 0..n_out {
        let c = &phases[phase * stride..][..stride];
        let h = &hist[pos + 1 - stride..][..stride];
        out.push(dot(c, h));

        let mut ph = phase + m_mod_l;
        let mut step = m_div_l;
        if ph >= l {
            ph -= l;
            step += 1;
        }
        phase = ph;
        pos += step;
    }
    (pos, phase)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
unsafe fn run_avx2<A: std::alloc::Allocator>(
    phases: &[f32],
    stride: usize,
    l: usize,
    m: usize,
    hist: &[f32],
    state: (usize, usize),
    n_out: usize,
    out: &mut Vec<f32, A>,
) -> (usize, usize) {
    run_scalar(phases, stride, l, m, hist, state, n_out, out)
}

pub fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// Steady-state output count for `in_len` inputs at `L/M`, about `floor(in_len * L / M)`.
pub fn output_len(in_len: u64, in_rate: u32, out_rate: u32) -> u64 {
    let g = gcd(in_rate, out_rate) as u64;
    let l = (out_rate as u64) / g;
    let m = (in_rate as u64) / g;
    in_len * l / m
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    const PI: f64 = std::f64::consts::PI;

    #[test]
    fn bessel_i0_basics() {
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-12);
        assert!((bessel_i0(1.0) - 1.266_065_9).abs() < 1e-5, "I0(1)={}", bessel_i0(1.0));
        assert!((bessel_i0(2.0) - 2.279_585_3).abs() < 1e-5, "I0(2)={}", bessel_i0(2.0));
    }

    #[test]
    fn gcd_reduces_common_rates() {
        assert_eq!(gcd(44_100, 48_000), 300); // 147/160
        assert_eq!(gcd(48_000, 24_000), 24_000); // 2/1
        assert_eq!(gcd(48_000, 44_100), 300);
        assert_eq!(gcd(16_000, 16_000), 16_000);
    }

    #[test]
    fn design_shapes_and_symmetry() {
        let f = PolyphaseFilter::design(44_100, 48_000, 16, 8.6);
        assert_eq!(f.l, 160);
        assert_eq!(f.m, 147);
        assert_eq!(f.taps_per_phase, 32); // 2 * half_taps
        assert_eq!(f.phases.len(), 32 * 160); // flat, taps_per_phase * L
        assert_eq!(f.prototype_len(), 32 * 160);

        let proto = f.prototype();
        let n = proto.len();
        for j in 0..n / 2 {
            let a = proto[j];
            let b = proto[n - 1 - j];
            assert!((a - b).abs() <= 1e-6 * (1.0 + a.abs()), "asymmetry at {j}: {a} vs {b}");
        }
        let (argmax, _) = proto
            .iter()
            .enumerate()
            .max_by(|x, y| x.1.abs().partial_cmp(&y.1.abs()).unwrap())
            .unwrap();
        assert!(
            (argmax as i64 - (n as i64 - 1) / 2).abs() <= f.l as i64,
            "peak near centre, got {argmax} of {n}"
        );
    }

    #[test]
    fn impulse_response_is_the_designed_filter() {
        let filt = PolyphaseFilter::design(48_000, 48_000, 8, 8.0);
        assert_eq!(filt.l, 1);
        assert_eq!(filt.m, 1);
        let proto = filt.prototype(); // == phases[0]
        let tpp = filt.taps_per_phase;

        let mut r = ChannelResampler::new(filt);
        let mut sig = vec![1.0f32];
        sig.extend(std::iter::repeat_n(0.0, tpp * 2));
        let mut out = Vec::new();
        r.process(&sig, &mut out);

        assert!(out.len() >= tpp, "produced {} of >= {tpp}", out.len());
        for k in 0..tpp {
            assert!(
                (out[k] - proto[k]).abs() < 1e-6,
                "impulse response tap {k}: got {}, want {}",
                out[k],
                proto[k]
            );
        }
    }

    fn resample_sine(freq_hz: f64, in_rate: u32, out_rate: u32, in_len: usize) -> Vec<f32> {
        let filt = PolyphaseFilter::design(in_rate, out_rate, 32, 9.0);
        let mut r = ChannelResampler::new(filt);
        let input: Vec<f32> = (0..in_len)
            .map(|n| (2.0 * PI * freq_hz * n as f64 / in_rate as f64).sin() as f32)
            .collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        out
    }

    fn dominant_freq(sig: &[f32], rate: u32, f_lo: f64, f_hi: f64, step: f64) -> (f64, f64) {
        let n = sig.len();
        let mut best_f = f_lo;
        let mut best_mag = -1.0;
        let mut f = f_lo;
        while f <= f_hi {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            let w = 2.0 * PI * f / rate as f64;
            for (i, &s) in sig.iter().enumerate() {
                let ph = w * i as f64;
                re += s as f64 * ph.cos();
                im -= s as f64 * ph.sin();
            }
            let mag = (re * re + im * im).sqrt() / n as f64;
            if mag > best_mag {
                best_mag = mag;
                best_f = f;
            }
            f += step;
        }
        (best_f, best_mag)
    }

    fn mag_at(sig: &[f32], rate: u32, f: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let w = 2.0 * PI * f / rate as f64;
        for (i, &s) in sig.iter().enumerate() {
            let ph = w * i as f64;
            re += s as f64 * ph.cos();
            im -= s as f64 * ph.sin();
        }
        (re * re + im * im).sqrt() / sig.len() as f64
    }

    #[test]
    fn sine_44100_to_48000_stays_a_sine() {
        let (in_rate, out_rate) = (44_100u32, 48_000u32);
        let freq = 1_000.0; // well inside both Nyquist limits
        let out = resample_sine(freq, in_rate, out_rate, 44_100); // ~1 s

        let trim = 800usize;
        assert!(out.len() > 2 * trim, "output too short: {}", out.len());
        let core = &out[trim..out.len() - trim];

        let (peak, peak_mag) = dominant_freq(core, out_rate, 500.0, 1_500.0, 2.0);
        assert!((peak - freq).abs() <= 3.0, "dominant freq {peak} Hz, want {freq}");

        assert!(peak_mag > 0.35, "peak magnitude too low: {peak_mag}");
        assert!(peak_mag < 0.65, "peak magnitude too high (gain error?): {peak_mag}");

        let alias = mag_at(core, out_rate, 5_000.0);
        assert!(alias < peak_mag * 0.02, "alias image {alias} too large vs peak {peak_mag}");
    }

    #[test]
    fn downsample_48000_to_24000_antialiases() {
        let (in_rate, out_rate) = (48_000u32, 24_000u32);
        let filt = PolyphaseFilter::design(in_rate, out_rate, 48, 10.0);
        assert_eq!((filt.l, filt.m), (1, 2));

        let n = 48_000usize;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / in_rate as f64;
                (0.5 * (2.0 * PI * 1_000.0 * t).sin() + 0.5 * (2.0 * PI * 15_000.0 * t).sin())
                    as f32
            })
            .collect();
        let mut r = ChannelResampler::new(filt);
        let mut out = Vec::new();
        r.process(&input, &mut out);

        let trim = 1_000usize;
        let core = &out[trim..out.len() - trim];
        let good = mag_at(core, out_rate, 1_000.0);
        assert!(good > 0.15, "1 kHz tone lost: {good}");
        let alias = mag_at(core, out_rate, 9_000.0);
        assert!(alias < good * 0.05, "15 kHz aliased to 9 kHz image {alias} vs {good}");
    }

    #[test]
    fn output_length_matches_ratio() {
        for (inr, outr, inlen) in [
            (44_100u32, 48_000u32, 44_100usize),
            (48_000, 24_000, 50_000),
            (8_000, 16_000, 10_000),
            (48_000, 44_100, 96_000),
        ] {
            let filt = PolyphaseFilter::design(inr, outr, 8, 8.0);
            let mut r = ChannelResampler::new(filt);
            let input = vec![0f32; inlen];
            let mut out = Vec::new();
            r.process(&input, &mut out);

            let expect = output_len(inlen as u64, inr, outr);
            let got = out.len() as i64;
            let diff = (got - expect as i64).abs();
            assert!(diff <= 2, "len {inr}->{outr}: got {got}, expect ~{expect} (diff {diff})");
        }
    }

    fn reference_resample(filt: &PolyphaseFilter, input: &[f32], exact: bool) -> Vec<f32> {
        let (l, m, tpp) = (filt.l, filt.m, filt.taps_per_phase);
        let proto = filt.prototype();
        let branch: Vec<Vec<f32>> =
            (0..l).map(|p| (0..tpp).map(|k| proto[p + k * l]).collect()).collect();

        let mut hist = vec![0f32; tpp - 1];
        hist.extend_from_slice(input);
        let (mut pos, mut phase) = (tpp - 1, 0usize);
        let mut out = Vec::new();
        while pos < hist.len() {
            if exact {
                let mut acc = 0f64;
                for (k, &c) in branch[phase].iter().enumerate() {
                    acc += c as f64 * hist[pos - k] as f64;
                }
                out.push(acc as f32);
            } else {
                let mut acc = 0f32;
                for (k, &c) in branch[phase].iter().enumerate() {
                    acc += c * hist[pos - k];
                }
                out.push(acc);
            }
            let ph = phase + m;
            pos += ph / l;
            phase = ph % l;
        }
        out
    }

    fn diff_stats(got: &[f32], want: &[f32]) -> (f64, f64) {
        let (mut max_abs, mut sig, mut err) = (0f64, 0f64, 0f64);
        for (a, b) in got.iter().zip(want) {
            let d = (*a as f64) - (*b as f64);
            max_abs = max_abs.max(d.abs());
            sig += (*b as f64) * (*b as f64);
            err += d * d;
        }
        (max_abs, 10.0 * (sig / err.max(f64::MIN_POSITIVE)).log10())
    }

    #[test]
    fn simd_dot_matches_serial_reference() {
        let n = 40_000usize;
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let noise = (state >> 40) as f32 / 8_388_608.0 - 0.125;
                let tone = 0.8 * (2.0 * PI * 997.0 * i as f64 / 44_100.0).sin() as f32;
                (tone + noise).clamp(-1.0, 1.0)
            })
            .collect();

        for (inr, outr, ht) in
            [(44_100u32, 48_000u32, 32usize), (48_000, 44_100, 32), (48_000, 24_000, 16)]
        {
            let filt = PolyphaseFilter::design(inr, outr, ht, 9.0);
            let serial = reference_resample(&filt, &input, false);
            let exact = reference_resample(&filt, &input, true);
            let mut got = Vec::new();
            ChannelResampler::new(filt).process(&input, &mut got);
            assert_eq!(got.len(), serial.len(), "{inr}->{outr}: output count must be unchanged");

            let (d_max, d_snr) = diff_stats(&got, &serial); // new vs old
            let (n_max, n_snr) = diff_stats(&got, &exact); // new vs truth
            let (s_max, s_snr) = diff_stats(&serial, &exact); // old vs truth
            println!(
                "{inr}->{outr}  blocked-vs-serial: max|Δ| {d_max:.2e}, SNR {d_snr:.1} dB  |  \
                 vs exact f64, blocked: max|Δ| {n_max:.2e}, SNR {n_snr:.1} dB; \
                 serial: max|Δ| {s_max:.2e}, SNR {s_snr:.1} dB"
            );
            assert!(d_max < 2e-6, "{inr}->{outr}: blocked-vs-serial max |Δ| {d_max:e} too large");
            assert!(d_snr > 130.0, "{inr}->{outr}: blocked-vs-serial SNR {d_snr} dB too low");
            assert!(
                n_snr >= s_snr,
                "{inr}->{outr}: blocked SNR {n_snr} dB must beat serial {s_snr} dB vs exact f64"
            );
        }
    }

    #[test]
    fn chunked_equals_oneshot() {
        let (inr, outr) = (44_100u32, 48_000u32);
        let input: Vec<f32> =
            (0..20_000).map(|n| (2.0 * PI * 997.0 * n as f64 / inr as f64).sin() as f32).collect();

        let mut r1 = ChannelResampler::new(PolyphaseFilter::design(inr, outr, 16, 9.0));
        let mut one = Vec::new();
        r1.process(&input, &mut one);

        let mut r2 = ChannelResampler::new(PolyphaseFilter::design(inr, outr, 16, 9.0));
        let mut many = Vec::new();
        for chunk in input.chunks(333) {
            r2.process(chunk, &mut many);
        }

        assert_eq!(one.len(), many.len(), "chunked output count differs");
        for (i, (a, b)) in one.iter().zip(&many).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i} differs: {a} vs {b}");
        }
    }
}
