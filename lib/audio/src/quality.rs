//! Masking-model perceptual quality metric (noise-to-mask ratio) for transcode QA.

use std::f64::consts::PI;

const N: usize = 2048;
const HOP: usize = N / 2;
const BARK_STEP: f64 = 0.5;
const LOUDNESS_EXP: f64 = 0.23;
const NMR_FLOOR_DB: f64 = -200.0;

/// The result of a perceptual comparison.
#[derive(Clone, Copy, Debug)]
pub struct QualityReport {
    /// Average Noise-to-Mask Ratio in dB.
    pub nmr_db: f64,
    /// Worst single-frame NMR in dB, catches localized artifacts an average hides.
    pub peak_nmr_db: f64,
    /// Mean per-frame bark-band loudness difference, 0 for identical signals.
    pub noise_loudness: f64,
    pub frames: usize,
}

impl QualityReport {
    /// Whether the transcode is perceptually transparent at `nmr_threshold_db`.
    pub fn is_transparent(&self, nmr_threshold_db: f64) -> bool {
        self.nmr_db < nmr_threshold_db
    }
}

fn hz_to_bark(f: f64) -> f64 {
    13.0 * (0.00076 * f).atan() + 3.5 * ((f / 7500.0).powi(2)).atan()
}

fn ath_energy(f: f64) -> f64 {
    let k = (f / 1000.0).max(0.02);
    let db = 3.64 * k.powf(-0.8) - 6.5 * (-0.6 * (k - 3.3).powi(2)).exp() + 1e-3 * k.powi(4);
    // cap so bands far above hearing stay finite and exc / ath stays a number
    10f64.powf(db.min(200.0) / 10.0)
}

fn spreading_db(dz: f64) -> f64 {
    15.81 + 7.5 * (dz + 0.474) - 17.5 * (1.0 + (dz + 0.474).powi(2)).sqrt()
}

/// Reusable perceptual analyzer, fixed tables and per-frame scratch allocated once.
pub struct PerceptualAnalyzer {
    sample_rate: u32,
    num_bands: usize,
    window: Vec<f64>,        // Hann window, len N
    band_of_bin: Vec<usize>, // band index for each bin 0..=N/2
    spread: Vec<f64>,        /* num_bands x num_bands spreading matrix, energy domain,
                              * column-normalized */
    ath: Vec<f64>,        // absolute-threshold energy per band
    offset_lin: Vec<f64>, // masking offset per band, linear, threshold = excitation / offset
    re_r: Vec<f64>,
    im_r: Vec<f64>,
    re_t: Vec<f64>,
    im_t: Vec<f64>,
    e_ref: Vec<f64>, // band energy of reference / test / error
    e_test: Vec<f64>,
    e_err: Vec<f64>,
    exc_ref: Vec<f64>, // spread excitation
    exc_test: Vec<f64>,
    mask: Vec<f64>,
}

impl PerceptualAnalyzer {
    /// Build the analyzer for a given sample rate (mono).
    #[allow(clippy::disallowed_methods)] // one-time fixed-table + scratch allocation
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f64;
        let half = N / 2;

        let mut window = vec![0.0; N];
        for (i, w) in window.iter_mut().enumerate() {
            *w = 0.5 - 0.5 * (2.0 * PI * i as f64 / N as f64).cos();
        }

        let mut band_of_bin = vec![0usize; half + 1];
        let bark_max = hz_to_bark(sr / 2.0);
        let num_bands = ((bark_max / BARK_STEP).ceil() as usize).max(1);
        let mut band_freq_sum = vec![0.0f64; num_bands];
        let mut band_bin_count = vec![0u32; num_bands];
        for (k, band) in band_of_bin.iter_mut().enumerate() {
            let f = k as f64 * sr / N as f64;
            let b = ((hz_to_bark(f) / BARK_STEP) as usize).min(num_bands - 1);
            *band = b;
            band_freq_sum[b] += f;
            band_bin_count[b] += 1;
        }

        let mut ath = vec![0.0f64; num_bands];
        let mut offset_lin = vec![0.0f64; num_bands];
        for b in 0..num_bands {
            let f = if band_bin_count[b] > 0 {
                band_freq_sum[b] / band_bin_count[b] as f64
            } else {
                let z = (b as f64 + 0.5) * BARK_STEP;
                600.0 * (z / 6.0).sinh() // inverse-Bark approximation
            };
            ath[b] = ath_energy(f);
            let z = b as f64 * BARK_STEP;
            let m_db = if z <= 12.0 { 3.0 } else { 0.25 * z }; // BS.1387 masking offset
            offset_lin[b] = 10f64.powf(m_db / 10.0);
        }

        let mut spread = vec![0.0f64; num_bands * num_bands];
        for src in 0..num_bands {
            let mut col_sum = 0.0;
            for dst in 0..num_bands {
                let dz = (dst as f64 - src as f64) * BARK_STEP;
                let v = 10f64.powf(spreading_db(dz) / 10.0);
                spread[dst * num_bands + src] = v;
                col_sum += v;
            }
            if col_sum > 0.0 {
                for dst in 0..num_bands {
                    spread[dst * num_bands + src] /= col_sum;
                }
            }
        }

        PerceptualAnalyzer {
            sample_rate,
            num_bands,
            window,
            band_of_bin,
            spread,
            ath,
            offset_lin,
            re_r: vec![0.0; N],
            im_r: vec![0.0; N],
            re_t: vec![0.0; N],
            im_t: vec![0.0; N],
            e_ref: vec![0.0; num_bands],
            e_test: vec![0.0; num_bands],
            e_err: vec![0.0; num_bands],
            exc_ref: vec![0.0; num_bands],
            exc_test: vec![0.0; num_bands],
            mask: vec![0.0; num_bands],
        }
    }

    /// Compare mono `reference` and `test` of the same length, already aligned and level-matched.
    pub fn analyze(&mut self, reference: &[f32], test: &[f32]) -> QualityReport {
        let len = reference.len().min(test.len());
        let num_bands = self.num_bands;
        let mut nmr_sum = 0.0f64; // Sum of per-frame mean NMR, linear
        let mut nmr_peak = 0.0f64;
        let mut loud_sum = 0.0f64;
        let mut frames = 0usize;

        let mut start = 0;
        while start + N <= len {
            for i in 0..N {
                let w = self.window[i];
                self.re_r[i] = reference[start + i] as f64 * w;
                self.re_t[i] = test[start + i] as f64 * w;
            }
            self.im_r.iter_mut().for_each(|x| *x = 0.0);
            self.im_t.iter_mut().for_each(|x| *x = 0.0);
            fft(&mut self.re_r, &mut self.im_r);
            fft(&mut self.re_t, &mut self.im_t);

            self.e_ref.iter_mut().for_each(|x| *x = 0.0);
            self.e_test.iter_mut().for_each(|x| *x = 0.0);
            self.e_err.iter_mut().for_each(|x| *x = 0.0);
            for k in 0..=N / 2 {
                let b = self.band_of_bin[k];
                let pr = self.re_r[k] * self.re_r[k] + self.im_r[k] * self.im_r[k];
                let pt = self.re_t[k] * self.re_t[k] + self.im_t[k] * self.im_t[k];
                let dr = self.re_r[k] - self.re_t[k];
                let di = self.im_r[k] - self.im_t[k];
                self.e_ref[b] += pr;
                self.e_test[b] += pt;
                self.e_err[b] += dr * dr + di * di;
            }

            for dst in 0..num_bands {
                let mut er = 0.0;
                let mut et = 0.0;
                let row = dst * num_bands;
                for src in 0..num_bands {
                    let s = self.spread[row + src];
                    er += self.e_ref[src] * s;
                    et += self.e_test[src] * s;
                }
                self.exc_ref[dst] = er + self.ath[dst];
                self.exc_test[dst] = et + self.ath[dst];
                self.mask[dst] = (er / self.offset_lin[dst]).max(self.ath[dst]);
            }

            let mut nmr_frame = 0.0;
            let mut loud_frame = 0.0;
            for b in 0..num_bands {
                nmr_frame += self.e_err[b] / self.mask[b];
                let nr = (self.exc_ref[b] / self.ath[b]).powf(LOUDNESS_EXP);
                let nt = (self.exc_test[b] / self.ath[b]).powf(LOUDNESS_EXP);
                loud_frame += (nr - nt).abs();
            }
            nmr_frame /= num_bands as f64;
            loud_frame /= num_bands as f64;

            nmr_sum += nmr_frame;
            nmr_peak = nmr_peak.max(nmr_frame);
            loud_sum += loud_frame;
            frames += 1;
            start += HOP;
        }

        let to_db = |lin: f64| {
            if lin <= 0.0 { NMR_FLOOR_DB } else { (10.0 * lin.log10()).max(NMR_FLOOR_DB) }
        };
        QualityReport {
            nmr_db: if frames == 0 { NMR_FLOOR_DB } else { to_db(nmr_sum / frames as f64) },
            peak_nmr_db: to_db(nmr_peak),
            noise_loudness: if frames == 0 { 0.0 } else { loud_sum / frames as f64 },
            frames,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Build an analyzer and compare, one shot.
pub fn perceptual_diff(reference: &[f32], test: &[f32], sample_rate: u32) -> QualityReport {
    PerceptualAnalyzer::new(sample_rate).analyze(reference, test)
}

/// Scale `test` in place so its RMS matches `reference`, so a gain change is not distortion.
pub fn rms_normalize(reference: &[f32], test: &mut [f32]) {
    let rms = |s: &[f32]| {
        (s.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / s.len().max(1) as f64).sqrt()
    };
    let (rr, rt) = (rms(reference), rms(test));
    if rt > 1e-12 && rr > 1e-12 {
        let g = (rr / rt) as f32;
        test.iter_mut().for_each(|x| *x *= g);
    }
}

/// Best integer shift of `test` against `reference` within `max_shift`, by normalized
/// cross-correlation.
pub fn best_offset(reference: &[f32], test: &[f32], max_shift: usize) -> isize {
    let win = reference.len().min(test.len()).min(48_000).saturating_sub(max_shift);
    if win == 0 {
        return 0;
    }
    let mut best = (f64::NEG_INFINITY, 0isize);
    let mut shift = -(max_shift as isize);
    while shift <= max_shift as isize {
        let mut acc = 0.0f64;
        for (i, &r) in reference[..win].iter().enumerate() {
            let ti = i as isize + shift;
            if ti >= 0 && (ti as usize) < test.len() {
                acc += r as f64 * test[ti as usize] as f64;
            }
        }
        if acc > best.0 {
            best = (acc, shift);
        }
        shift += 1;
    }
    best.1
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        let (wlr, wli) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let a = i + k;
                let b = i + k + len / 2;
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wlr - ci * wli;
                ci = cr * wli + ci * wlr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests build throwaway signal buffers
mod tests {
    use super::*;

    #[test]
    fn ath_stays_finite_up_to_hi_res_nyquist() {
        for f in [20_000.0, 44_100.0, 96_000.0, 192_000.0] {
            assert!(ath_energy(f).is_finite(), "{f}");
        }
    }

    fn sine(freq: f64, sr: u32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * freq * i as f64 / sr as f64).sin() as f32).collect()
    }

    #[test]
    fn identical_signals_are_transparent() {
        let s = sine(1000.0, 48_000, 48_000);
        let r = perceptual_diff(&s, &s, 48_000);
        assert!(r.frames > 0);
        assert!(r.nmr_db < -100.0, "identical NMR should be very low, got {}", r.nmr_db);
        assert!(
            r.noise_loudness < 1e-6,
            "identical loudness diff should be ~0, got {}",
            r.noise_loudness
        );
        assert!(r.is_transparent(0.0));
    }

    #[test]
    fn nmr_grows_with_added_noise() {
        let clean = sine(1000.0, 48_000, 48_000);
        let mut analyzer = PerceptualAnalyzer::new(48_000);
        let noisy = |amp: f32| -> Vec<f32> {
            let mut seed = 0x2545F4914F6CDD1Du64;
            clean
                .iter()
                .map(|&c| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let n = ((seed >> 40) as f32 / (1u32 << 24) as f32) - 0.5;
                    c + amp * n
                })
                .collect()
        };
        let low = analyzer.analyze(&clean, &noisy(0.001)).nmr_db;
        let high = analyzer.analyze(&clean, &noisy(0.05)).nmr_db;
        assert!(high > low, "more noise must raise NMR: {low} -> {high}");
        assert!(high > -20.0, "loud noise should be audible, NMR {high}");
    }

    #[test]
    fn rms_normalize_matches_levels() {
        let r = sine(440.0, 48_000, 4096);
        let mut t: Vec<f32> = r.iter().map(|&x| x * 3.0).collect(); // +9.5 dB
        rms_normalize(&r, &mut t);
        let rms = |s: &[f32]| (s.iter().map(|&x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        assert!((rms(&r) - rms(&t)).abs() < 1e-4, "levels should match after normalize");
    }

    #[test]
    fn best_offset_finds_a_known_shift() {
        let r = sine(300.0, 48_000, 20_000);
        let shift = 137usize;
        let mut t = vec![0.0f32; shift];
        t.extend_from_slice(&r); // `test` lags `reference` by `shift`
        assert_eq!(best_offset(&r, &t, 400), shift as isize);
    }

    #[test]
    fn fft_matches_naive_dft() {
        let n = 16;
        let sig: Vec<f64> =
            (0..n).map(|i| (i as f64 * 0.7).sin() + 0.3 * (i as f64 * 2.1).cos()).collect();
        let mut re = sig.clone();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        for k in 0..n {
            let (mut dr, mut di) = (0.0, 0.0);
            for (t, &x) in sig.iter().enumerate() {
                let a = -2.0 * PI * k as f64 * t as f64 / n as f64;
                dr += x * a.cos();
                di += x * a.sin();
            }
            assert!((re[k] - dr).abs() < 1e-9 && (im[k] - di).abs() < 1e-9, "bin {k}");
        }
    }
}
