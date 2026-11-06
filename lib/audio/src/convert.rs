//! Allocation-free interleaved PCM sample format conversion, channel remap and downmix.

use crate::format::SampleFormat;
use crate::layout::positions;

#[inline(always)]
fn decode_i32_scale32(fmt: SampleFormat, b: &[u8]) -> i32 {
    match fmt {
        SampleFormat::U8 => ((b[0] as i32) - 128) << 24,
        SampleFormat::S16 => (i16::from_le_bytes([b[0], b[1]]) as i32) << 16,
        SampleFormat::S24 => {
            let v = b[0] as i32 | (b[1] as i32) << 8 | (b[2] as i32) << 16;
            ((v << 8) >> 8) << 8
        }
        SampleFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::F32 => 0,
    }
}

#[inline(always)]
fn encode_i32_scale32(fmt: SampleFormat, v: i32, out: &mut [u8]) {
    match fmt {
        SampleFormat::U8 => {
            let s = round_shift(v, 24).clamp(-128, 127);
            out[0] = (s + 128) as u8;
        }
        SampleFormat::S16 => {
            let s = round_shift(v, 16).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            out[..2].copy_from_slice(&s.to_le_bytes());
        }
        SampleFormat::S24 => {
            let s = round_shift(v, 8).clamp(-(1 << 23), (1 << 23) - 1);
            let le = s.to_le_bytes();
            out[..3].copy_from_slice(&le[..3]);
        }
        SampleFormat::S32 => {
            out[..4].copy_from_slice(&v.to_le_bytes());
        }
        SampleFormat::F32 => {
            out[..4].copy_from_slice(&0f32.to_le_bytes());
        }
    }
}

#[inline(always)]
fn round_shift(v: i32, shift: u32) -> i32 {
    if shift == 0 {
        return v;
    }
    let half = 1i32 << (shift - 1);
    // Saturating so a value near i32::MAX rounds up without wrapping.
    (v.saturating_add(half)) >> shift
}

#[inline(always)]
fn decode_f32(b: &[u8]) -> f32 {
    f32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline(always)]
fn int_scale(bits: u32) -> f32 {
    ((1u64 << (bits - 1)) - 1) as f32
}

#[inline(always)]
fn encode_float_to_int(fmt: SampleFormat, f: f32, out: &mut [u8]) {
    let bits = fmt.bits();
    let scaled = (f.clamp(-1.0, 1.0) * int_scale(bits)).round();
    let native = scaled as i32; // in range by construction (clamped, |scale| < 2^(bits-1))
    match fmt {
        SampleFormat::U8 => out[0] = (native.clamp(-128, 127) + 128) as u8,
        SampleFormat::S16 => out[..2].copy_from_slice(
            &(native.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes(),
        ),
        SampleFormat::S24 => {
            let s = native.clamp(-(1 << 23), (1 << 23) - 1);
            out[..3].copy_from_slice(&s.to_le_bytes()[..3]);
        }
        SampleFormat::S32 => {
            out[..4].copy_from_slice(&native.to_le_bytes());
        }
        SampleFormat::F32 => unreachable!("encode_float_to_int called with F32 target"),
    }
}

#[inline(always)]
fn decode_int_to_float(fmt: SampleFormat, b: &[u8]) -> f32 {
    let bits = fmt.bits();
    let native = match fmt {
        SampleFormat::U8 => (b[0] as i32) - 128,
        SampleFormat::S16 => i16::from_le_bytes([b[0], b[1]]) as i32,
        SampleFormat::S24 => {
            let v = b[0] as i32 | (b[1] as i32) << 8 | (b[2] as i32) << 16;
            (v << 8) >> 8
        }
        SampleFormat::S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::F32 => unreachable!("decode_int_to_float called with F32 source"),
    };
    native as f32 / (1u64 << (bits - 1)) as f32
}

#[inline(always)]
fn convert_sample(from: SampleFormat, to: SampleFormat, src: &[u8], dst: &mut [u8]) {
    match (from.is_float(), to.is_float()) {
        (false, false) => encode_i32_scale32(to, decode_i32_scale32(from, src), dst),
        (false, true) => {
            let f = decode_int_to_float(from, src);
            dst[..4].copy_from_slice(&f.to_le_bytes());
        }
        (true, false) => encode_float_to_int(to, decode_f32(src), dst),
        (true, true) => dst[..4].copy_from_slice(&src[..4]),
    }
}

#[inline(always)]
fn convert_pair<const IN: usize, const OUT: usize>(
    from: SampleFormat,
    to: SampleFormat,
    input: &[u8],
    output: &mut [u8],
) {
    let (src, _) = input.as_chunks::<IN>();
    let (dst, _) = output.as_chunks_mut::<OUT>();
    for (s, d) in src.iter().zip(dst) {
        convert_sample(from, to, s, d);
    }
}

macro_rules! by_target {
    ($from:path, $to:expr, $inp:expr, $outp:expr) => {
        match $to {
            SampleFormat::U8 => {
                convert_pair::<{ $from.bytes() }, 1>($from, SampleFormat::U8, $inp, $outp)
            }
            SampleFormat::S16 => {
                convert_pair::<{ $from.bytes() }, 2>($from, SampleFormat::S16, $inp, $outp)
            }
            SampleFormat::S24 => {
                convert_pair::<{ $from.bytes() }, 3>($from, SampleFormat::S24, $inp, $outp)
            }
            SampleFormat::S32 => {
                convert_pair::<{ $from.bytes() }, 4>($from, SampleFormat::S32, $inp, $outp)
            }
            SampleFormat::F32 => {
                convert_pair::<{ $from.bytes() }, 4>($from, SampleFormat::F32, $inp, $outp)
            }
        }
    };
}

#[inline(always)]
fn dispatch_pair(from: SampleFormat, to: SampleFormat, input: &[u8], output: &mut [u8]) {
    match from {
        SampleFormat::U8 => by_target!(SampleFormat::U8, to, input, output),
        SampleFormat::S16 => by_target!(SampleFormat::S16, to, input, output),
        SampleFormat::S24 => by_target!(SampleFormat::S24, to, input, output),
        SampleFormat::S32 => by_target!(SampleFormat::S32, to, input, output),
        SampleFormat::F32 => by_target!(SampleFormat::F32, to, input, output),
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn dispatch_pair_avx2(
    from: SampleFormat,
    to: SampleFormat,
    input: &[u8],
    output: &mut [u8],
) {
    dispatch_pair(from, to, input, output);
}

/// Convert interleaved PCM `input` from `from` to `to`, writing to `output`.
pub fn convert_interleaved(
    from: SampleFormat,
    to: SampleFormat,
    channels: usize,
    input: &[u8],
    output: &mut [u8],
) -> Option<usize> {
    let in_bps = from.bytes();
    let out_bps = to.bytes();
    let in_stride = in_bps.checked_mul(channels)?;
    if in_stride == 0 || !input.len().is_multiple_of(in_stride) {
        return None;
    }
    let samples = input.len() / in_bps; // total samples across all channels
    let out_len = samples.checked_mul(out_bps)?;
    if output.len() < out_len {
        return None;
    }

    if from == to {
        output[..input.len()].copy_from_slice(input);
        return Some(input.len());
    }

    let output = &mut output[..out_len];
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: gated on runtime AVX2 detection, the body is safe Rust with no intrinsics.
        unsafe { dispatch_pair_avx2(from, to, input, output) };
        return Some(out_len);
    }
    dispatch_pair(from, to, input, output);
    Some(out_len)
}

/// The exact output byte length [`convert_interleaved`] produces for `input_len` bytes.
pub fn converted_len(
    from: SampleFormat,
    to: SampleFormat,
    channels: usize,
    input_len: usize,
) -> Option<usize> {
    let in_bps = from.bytes();
    let stride = in_bps.checked_mul(channels)?;
    if stride == 0 || !input_len.is_multiple_of(stride) {
        return None;
    }
    (input_len / in_bps).checked_mul(to.bytes())
}

/// Convert interleaved PCM into a freshly-allocated `Vec`, sizing the output exactly.
pub fn convert_interleaved_vec(
    from: SampleFormat,
    to: SampleFormat,
    channels: usize,
    input: &[u8],
) -> Option<Vec<u8>> {
    let out_len = converted_len(from, to, channels, input.len())?;
    let mut out = vec![0u8; out_len];
    convert_interleaved(from, to, channels, input, &mut out)?;
    Some(out)
}

/// Remap interleaved PCM from `in_channels` to `out_channels`, writing to `output`.
pub fn remap_channels(
    fmt: SampleFormat,
    in_channels: usize,
    out_channels: usize,
    input: &[u8],
    output: &mut [u8],
) -> Option<usize> {
    let bps = fmt.bytes();
    let in_stride = bps.checked_mul(in_channels)?;
    if in_stride == 0 || !input.len().is_multiple_of(in_stride) {
        return None;
    }
    let frames = input.len() / in_stride;
    let out_len = frames.checked_mul(bps.checked_mul(out_channels)?)?;
    if output.len() < out_len {
        return None;
    }

    match (in_channels, out_channels) {
        (i, o) if i == o => {
            output[..input.len()].copy_from_slice(input);
            Some(input.len())
        }
        (1, 2) => {
            for (s, d) in input
                .chunks_exact(bps)
                .zip(output[..out_len].chunks_exact_mut(2 * bps))
                .take(frames)
            {
                let (dl, dr) = d.split_at_mut(bps);
                dl.copy_from_slice(s);
                dr.copy_from_slice(s);
            }
            Some(out_len)
        }
        (2, 1) => {
            let output = &mut output[..out_len];
            match fmt {
                SampleFormat::U8 => stereo_to_mono::<1>(fmt, frames, input, output),
                SampleFormat::S16 => stereo_to_mono::<2>(fmt, frames, input, output),
                SampleFormat::S24 => stereo_to_mono::<3>(fmt, frames, input, output),
                SampleFormat::S32 => stereo_to_mono::<4>(fmt, frames, input, output),
                SampleFormat::F32 => stereo_to_mono::<4>(fmt, frames, input, output),
            }
            Some(out_len)
        }
        _ => None,
    }
}

#[inline(always)]
fn stereo_to_mono<const BPS: usize>(
    fmt: SampleFormat,
    frames: usize,
    input: &[u8],
    output: &mut [u8],
) {
    for (fr, d) in input.chunks_exact(2 * BPS).zip(output.as_chunks_mut::<BPS>().0).take(frames) {
        let (l, r) = fr.split_at(BPS);
        if fmt.is_float() {
            let avg = (decode_f32(l) + decode_f32(r)) * 0.5;
            d[..4].copy_from_slice(&avg.to_le_bytes());
        } else {
            let a = decode_i32_scale32(fmt, l) as i64;
            let b = decode_i32_scale32(fmt, r) as i64;
            let avg = ((a + b) / 2) as i32;
            encode_i32_scale32(fmt, avg, d);
        }
    }
}

const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2; // 0.70710677

/// Fold multichannel PCM to stereo with the ITU-R BS.775-3 Lo/Ro matrix.
/// Centre and surrounds go in at -3 dB, LFE is dropped.
pub fn downmix_to_stereo(
    fmt: SampleFormat,
    in_channels: usize,
    input: &[u8],
    output: &mut [u8],
) -> Option<usize> {
    let bps = fmt.bytes();
    let in_stride = bps.checked_mul(in_channels)?;
    if in_stride == 0 || in_channels < 2 || !input.len().is_multiple_of(in_stride) {
        return None;
    }
    let frames = input.len() / in_stride;
    let out_len = frames.checked_mul(bps.checked_mul(2)?)?;
    if output.len() < out_len {
        return None;
    }

    let output = &mut output[..out_len];
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: gated on runtime AVX2 detection, the body is safe Rust with no intrinsics.
        unsafe { downmix_dispatch_avx2(fmt, in_channels, frames, input, output) };
        return Some(out_len);
    }
    downmix_dispatch(fmt, in_channels, frames, input, output);
    Some(out_len)
}

/// The gain that keeps a full scale input of every channel of `layout` within full scale
/// after the fold, one over the larger side's coefficient sum, 1 when nothing folds or the
/// layout does not name `in_channels` positions.
pub fn downmix_clip_gain(layout: u64, in_channels: usize) -> f32 {
    if layout == 0 || layout.count_ones() as usize != in_channels {
        return 1.0;
    }
    let (mut lo, mut ro) = (0f32, 0f32);
    for p in positions(layout) {
        let (l, r) = stereo_feed(p);
        (lo, ro) = (lo + l, ro + r);
    }
    let peak = lo.max(ro);
    if peak > 1.0 { 1.0 / peak } else { 1.0 }
}

/// [`downmix_layout_to_stereo`] with every coefficient scaled by `gain` before the fold
/// clamps, so [`downmix_clip_gain`] keeps a full scale input within full scale. The layout
/// must name `in_channels` positions.
pub fn downmix_layout_to_stereo_scaled(
    fmt: SampleFormat,
    layout: u64,
    in_channels: usize,
    input: &[u8],
    output: &mut [u8],
    gain: f32,
) -> Option<usize> {
    if layout == 0 || layout.count_ones() as usize != in_channels || in_channels > 64 {
        return None;
    }
    let bps = fmt.bytes();
    let in_stride = bps * in_channels;
    if in_stride == 0 || !input.len().is_multiple_of(in_stride) {
        return None;
    }
    let frames = input.len() / in_stride;
    let out_len = frames * bps * 2;
    if output.len() < out_len {
        return None;
    }
    let (mut lo, mut ro) = ([0f32; 64], [0f32; 64]);
    for (c, p) in positions(layout).enumerate() {
        let (l, r) = stereo_feed(p);
        (lo[c], ro[c]) = (l * gain, r * gain);
    }
    let (lo, ro) = (&lo[..in_channels], &ro[..in_channels]);
    let output = &mut output[..out_len];
    match fmt {
        SampleFormat::U8 => fold_by_feed::<1>(fmt, lo, ro, input, output),
        SampleFormat::S16 => fold_by_feed::<2>(fmt, lo, ro, input, output),
        SampleFormat::S24 => fold_by_feed::<3>(fmt, lo, ro, input, output),
        SampleFormat::S32 | SampleFormat::F32 => fold_by_feed::<4>(fmt, lo, ro, input, output),
    }
    Some(out_len)
}

/// Folds interleaved stereo PCM to mono, the mean of both sides, clamped at full scale.
pub fn fold_stereo_to_mono(fmt: SampleFormat, input: &[u8], output: &mut [u8]) -> Option<usize> {
    let bps = fmt.bytes();
    let stride = bps * 2;
    if stride == 0 || !input.len().is_multiple_of(stride) {
        return None;
    }
    let out_len = input.len() / 2;
    if output.len() < out_len {
        return None;
    }
    let out = &mut output[..out_len];
    for (s, d) in input.chunks_exact(stride).zip(out.chunks_exact_mut(bps)) {
        let v = (downmix_get(fmt, &s[..bps]) + downmix_get(fmt, &s[bps..])) * 0.5;
        downmix_put(fmt, v, d);
    }
    Some(out_len)
}

#[inline(always)]
fn downmix_get(fmt: SampleFormat, b: &[u8]) -> f32 {
    if fmt.is_float() { decode_f32(b) } else { decode_i32_scale32(fmt, b) as f32 / 2_147_483_648.0 }
}

#[inline(always)]
fn downmix_put(fmt: SampleFormat, v: f32, d: &mut [u8]) {
    if fmt.is_float() {
        d[..4].copy_from_slice(&v.clamp(-1.0, 1.0).to_le_bytes());
    } else {
        let scaled = (v * 2_147_483_648.0).clamp(i32::MIN as f32, i32::MAX as f32);
        encode_i32_scale32(fmt, scaled as i32, d);
    }
}

#[inline(always)]
fn downmix_fmt<const BPS: usize>(
    fmt: SampleFormat,
    in_channels: usize,
    frames: usize,
    input: &[u8],
    output: &mut [u8],
) {
    macro_rules! fold {
        ($nch:expr, $mix:expr) => {{
            const NCH: usize = $nch;
            let mix = $mix;
            for (fr, dst) in
                input.chunks_exact(BPS * NCH).zip(output.chunks_exact_mut(2 * BPS)).take(frames)
            {
                let mut s = [0f32; NCH];
                for (c, sc) in s.iter_mut().enumerate() {
                    *sc = downmix_get(fmt, &fr[c * BPS..][..BPS]);
                }
                let (lo, ro) = mix(&s);
                let (dl, dr) = dst.split_at_mut(BPS);
                downmix_put(fmt, lo, dl);
                downmix_put(fmt, ro, dr);
            }
        }};
    }

    match in_channels {
        3 => fold!(3, |s: &[f32; 3]| (s[0] + M3DB * s[2], s[1] + M3DB * s[2])),
        4 => fold!(4, |s: &[f32; 4]| (s[0] + M3DB * s[2], s[1] + M3DB * s[3])),
        5 => fold!(5, |s: &[f32; 5]| (s[0] + M3DB * (s[2] + s[3]), s[1] + M3DB * (s[2] + s[4]))),
        6 => fold!(6, |s: &[f32; 6]| (s[0] + M3DB * (s[2] + s[4]), s[1] + M3DB * (s[2] + s[5]))),
        8 => fold!(8, |s: &[f32; 8]| {
            (s[0] + M3DB * (s[2] + s[4] + s[6]), s[1] + M3DB * (s[2] + s[5] + s[7]))
        }),
        n => {
            let in_stride = BPS * n;
            for (fr, dst) in
                input.chunks_exact(in_stride).zip(output.chunks_exact_mut(2 * BPS)).take(frames)
            {
                let (lo, ro) = (downmix_get(fmt, &fr[..BPS]), downmix_get(fmt, &fr[BPS..][..BPS]));
                let (dl, dr) = dst.split_at_mut(BPS);
                downmix_put(fmt, lo, dl);
                downmix_put(fmt, ro, dr);
            }
        }
    }
}

#[inline(always)]
fn downmix_dispatch(
    fmt: SampleFormat,
    in_channels: usize,
    frames: usize,
    input: &[u8],
    output: &mut [u8],
) {
    match fmt {
        SampleFormat::U8 => downmix_fmt::<1>(fmt, in_channels, frames, input, output),
        SampleFormat::S16 => downmix_fmt::<2>(fmt, in_channels, frames, input, output),
        SampleFormat::S24 => downmix_fmt::<3>(fmt, in_channels, frames, input, output),
        SampleFormat::S32 => downmix_fmt::<4>(fmt, in_channels, frames, input, output),
        SampleFormat::F32 => downmix_fmt::<4>(fmt, in_channels, frames, input, output),
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn downmix_dispatch_avx2(
    fmt: SampleFormat,
    in_channels: usize,
    frames: usize,
    input: &[u8],
    output: &mut [u8],
) {
    downmix_dispatch(fmt, in_channels, frames, input, output);
}

fn stereo_feed(pos: u64) -> (f32, f32) {
    use crate::layout::*;
    match pos {
        FL => (1.0, 0.0),
        FR => (0.0, 1.0),
        FC => (M3DB, M3DB),
        BL | SL | FLC | TFL | TBL => (M3DB, 0.0),
        BR | SR | FRC | TFR | TBR => (0.0, M3DB),
        BC | TC | TFC | TBC => (0.5, 0.5),
        _ => (0.0, 0.0),
    }
}

fn fixed_fold_fits(layout: u64, channels: usize) -> bool {
    use crate::layout::*;
    let role = |p: u64| match p {
        FL => 0u8,
        FR => 1,
        FC => 2,
        LFE => 3,
        BL | SL => 4,
        BR | SR => 5,
        _ => 6,
    };
    let table: &[u8] = match channels {
        3 => &[0, 1, 2],
        4 => &[0, 1, 4, 5],
        5 => &[0, 1, 2, 4, 5],
        6 => &[0, 1, 2, 3, 4, 5],
        8 => &[0, 1, 2, 3, 4, 5, 4, 5],
        _ => return false,
    };
    positions(layout).map(role).eq(table.iter().copied())
}

/// [`downmix_to_stereo`] by the channel layout.
pub fn downmix_layout_to_stereo(
    fmt: SampleFormat,
    layout: u64,
    in_channels: usize,
    input: &[u8],
    output: &mut [u8],
) -> Option<usize> {
    if layout == 0
        || layout.count_ones() as usize != in_channels
        || fixed_fold_fits(layout, in_channels)
    {
        return downmix_to_stereo(fmt, in_channels, input, output);
    }
    let bps = fmt.bytes();
    let in_stride = bps * in_channels;
    if !input.len().is_multiple_of(in_stride) {
        return None;
    }
    let frames = input.len() / in_stride;
    let out_len = frames * bps * 2;
    if output.len() < out_len {
        return None;
    }
    let (mut lo, mut ro) = ([0f32; 64], [0f32; 64]);
    for (c, p) in positions(layout).enumerate() {
        (lo[c], ro[c]) = stereo_feed(p);
    }
    let (lo, ro) = (&lo[..in_channels], &ro[..in_channels]);
    let output = &mut output[..out_len];
    match fmt {
        SampleFormat::U8 => fold_by_feed::<1>(fmt, lo, ro, input, output),
        SampleFormat::S16 => fold_by_feed::<2>(fmt, lo, ro, input, output),
        SampleFormat::S24 => fold_by_feed::<3>(fmt, lo, ro, input, output),
        SampleFormat::S32 | SampleFormat::F32 => fold_by_feed::<4>(fmt, lo, ro, input, output),
    }
    Some(out_len)
}

#[inline(always)]
fn fold_by_feed<const BPS: usize>(
    fmt: SampleFormat,
    lo: &[f32],
    ro: &[f32],
    input: &[u8],
    output: &mut [u8],
) {
    let ch = lo.len();
    for (fr, dst) in input.chunks_exact(BPS * ch).zip(output.chunks_exact_mut(2 * BPS)) {
        let (mut l, mut r) = (0f32, 0f32);
        for ((s, a), b) in fr.as_chunks::<BPS>().0.iter().zip(lo).zip(ro) {
            let v = downmix_get(fmt, s);
            l += a * v;
            r += b * v;
        }
        let (dl, dr) = dst.split_at_mut(BPS);
        downmix_put(fmt, l, dl);
        downmix_put(fmt, r, dr);
    }
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::format::AudioFormat;

    const INT_FORMATS: [SampleFormat; 4] =
        [SampleFormat::U8, SampleFormat::S16, SampleFormat::S24, SampleFormat::S32];
    const ALL_FORMATS: [SampleFormat; 5] = [
        SampleFormat::U8,
        SampleFormat::S16,
        SampleFormat::S24,
        SampleFormat::S32,
        SampleFormat::F32,
    ];

    fn enc_native(fmt: SampleFormat, native: i32) -> Vec<u8> {
        match fmt {
            SampleFormat::U8 => vec![(native + 128) as u8],
            SampleFormat::S16 => (native as i16).to_le_bytes().to_vec(),
            SampleFormat::S24 => native.to_le_bytes()[..3].to_vec(),
            SampleFormat::S32 => native.to_le_bytes().to_vec(),
            SampleFormat::F32 => (native as f32).to_le_bytes().to_vec(),
        }
    }

    fn enc_f32(v: f32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    fn one(from: SampleFormat, to: SampleFormat, src: &[u8]) -> Vec<u8> {
        convert_interleaved_vec(from, to, 1, src).expect("convert one sample")
    }

    #[test]
    fn identity_is_byte_exact_for_every_format() {
        for f in ALL_FORMATS {
            let src: Vec<u8> = (0..(f.bytes() * 4) as u8).collect();
            let out = convert_interleaved_vec(f, f, 2, &src).unwrap();
            assert_eq!(out, src, "identity {f:?} must be a byte-exact copy");
        }
    }

    #[test]
    fn widen_shifts_align_msb() {
        assert_eq!(
            one(SampleFormat::S16, SampleFormat::S24, &1i16.to_le_bytes()),
            enc_native(SampleFormat::S24, 1i32 << 8)
        );
        assert_eq!(
            one(SampleFormat::S16, SampleFormat::S32, &100i16.to_le_bytes()),
            ((100i32) << 16).to_le_bytes().to_vec()
        );
        let s24 = enc_native(SampleFormat::S24, 1234);
        assert_eq!(
            one(SampleFormat::S24, SampleFormat::S32, &s24),
            ((1234i32) << 8).to_le_bytes().to_vec()
        );
        assert_eq!(
            one(SampleFormat::U8, SampleFormat::S16, &[200]),
            (((200i32 - 128) << 8) as i16).to_le_bytes().to_vec()
        );
    }

    #[test]
    fn widen_extremes_map_to_target_extremes() {
        assert_eq!(
            one(SampleFormat::S16, SampleFormat::S32, &i16::MIN.to_le_bytes()),
            ((i16::MIN as i32) << 16).to_le_bytes().to_vec()
        );
        assert_eq!(
            one(SampleFormat::U8, SampleFormat::S32, &[0]),
            ((-128i32) << 24).to_le_bytes().to_vec()
        );
        assert_eq!(
            one(SampleFormat::U8, SampleFormat::S32, &[255]),
            ((127i32) << 24).to_le_bytes().to_vec()
        );
    }

    #[test]
    fn widen_then_narrow_roundtrips_losslessly_all_pairs() {
        for narrow in INT_FORMATS {
            for wide in INT_FORMATS {
                if wide.bits() < narrow.bits() {
                    continue;
                }
                let (lo, hi) = int_range(narrow);
                let step = ((hi as i64 - lo as i64) / 4096).max(1);
                let mut v = lo as i64;
                while v <= hi as i64 {
                    let src = enc_native(narrow, v as i32);
                    let wide_bytes = one(narrow, wide, &src);
                    let back = one(wide, narrow, &wide_bytes);
                    assert_eq!(
                        back, src,
                        "round-trip {narrow:?}->{wide:?}->{narrow:?} at {v} must be lossless"
                    );
                    v += step;
                }
            }
        }
    }

    fn int_range(fmt: SampleFormat) -> (i32, i32) {
        match fmt {
            SampleFormat::U8 => (-128, 127),
            SampleFormat::S16 => (i16::MIN as i32, i16::MAX as i32),
            SampleFormat::S24 => (-(1 << 23), (1 << 23) - 1),
            SampleFormat::S32 => (i32::MIN, i32::MAX),
            SampleFormat::F32 => unreachable!(),
        }
    }

    #[test]
    fn narrow_rounds_to_nearest() {
        let v = (5i32 << 16) | 0x8000; // 5.5 at s16 scale
        assert_eq!(
            one(SampleFormat::S32, SampleFormat::S16, &v.to_le_bytes()),
            6i16.to_le_bytes().to_vec(),
            "0x8000 fractional part rounds up"
        );
        let v = (5i32 << 16) | 0x7FFF;
        assert_eq!(
            one(SampleFormat::S32, SampleFormat::S16, &v.to_le_bytes()),
            5i16.to_le_bytes().to_vec(),
            "0x7FFF fractional part rounds down"
        );
        let v = (-5i32 << 16) - 0x8000; // == -(5.5) at s16 scale
        let got = one(SampleFormat::S32, SampleFormat::S16, &v.to_le_bytes());
        assert_eq!(got, (-5i16).to_le_bytes().to_vec(), "-5.5 ties toward +inf => -5");
    }

    #[test]
    fn narrow_clamps_at_positive_full_scale() {
        assert_eq!(
            one(SampleFormat::S32, SampleFormat::S16, &i32::MAX.to_le_bytes()),
            i16::MAX.to_le_bytes().to_vec(),
            "narrowing the max value clamps instead of wrapping"
        );
        assert_eq!(
            one(SampleFormat::S32, SampleFormat::S24, &i32::MAX.to_le_bytes()),
            enc_native(SampleFormat::S24, (1 << 23) - 1),
        );
        assert_eq!(one(SampleFormat::S32, SampleFormat::U8, &i32::MAX.to_le_bytes()), vec![255u8],);
    }

    #[test]
    fn int_to_float_maps_full_scale_near_plus_minus_one() {
        let max_f = f32::from_le_bytes(
            one(SampleFormat::S16, SampleFormat::F32, &i16::MAX.to_le_bytes()).try_into().unwrap(),
        );
        assert!((max_f - 32767.0 / 32768.0).abs() < 1e-6, "s16 max -> {max_f}");
        let min_f = f32::from_le_bytes(
            one(SampleFormat::S16, SampleFormat::F32, &i16::MIN.to_le_bytes()).try_into().unwrap(),
        );
        assert!((min_f + 1.0).abs() < 1e-6, "s16 min -> {min_f}");
        assert_eq!(one(SampleFormat::S16, SampleFormat::F32, &0i16.to_le_bytes()), enc_f32(0.0));
        assert_eq!(one(SampleFormat::U8, SampleFormat::F32, &[128]), enc_f32(0.0));
    }

    #[test]
    fn float_to_int_scales_and_clamps() {
        assert_eq!(
            one(SampleFormat::F32, SampleFormat::S16, &enc_f32(1.0)),
            32767i16.to_le_bytes().to_vec()
        );
        assert_eq!(
            one(SampleFormat::F32, SampleFormat::S16, &enc_f32(-1.0)),
            (-32767i16).to_le_bytes().to_vec()
        );
        assert_eq!(
            one(SampleFormat::F32, SampleFormat::S16, &enc_f32(2.0)),
            32767i16.to_le_bytes().to_vec()
        );
        assert_eq!(
            one(SampleFormat::F32, SampleFormat::S16, &enc_f32(-9.0)),
            (-32767i16).to_le_bytes().to_vec()
        );
        assert_eq!(
            one(SampleFormat::F32, SampleFormat::S16, &enc_f32(0.0)),
            0i16.to_le_bytes().to_vec()
        );
        assert_eq!(one(SampleFormat::F32, SampleFormat::U8, &enc_f32(0.0)), vec![128u8]);
    }

    #[test]
    fn float_int_float_roundtrip_within_tolerance() {
        for to in INT_FORMATS {
            let bits = to.bits();
            let lsb = 1.0 / (1u64 << (bits - 1)) as f32;
            let mut f = -0.999f32;
            while f <= 0.999 {
                let i = one(SampleFormat::F32, to, &enc_f32(f));
                let back = f32::from_le_bytes(one(to, SampleFormat::F32, &i).try_into().unwrap());
                assert!(
                    (back - f).abs() <= lsb * 1.5,
                    "f32->{to:?}->f32 at {f}: got {back}, err {} > {}",
                    (back - f).abs(),
                    lsb * 1.5
                );
                f += 0.013;
            }
        }
    }

    #[test]
    fn float_to_float_is_exact_copy() {
        let src = 0.123456f32.to_le_bytes();
        assert_eq!(one(SampleFormat::F32, SampleFormat::F32, &src), src.to_vec());
    }

    #[test]
    fn every_pair_converts_and_lengths_are_exact() {
        for from in ALL_FORMATS {
            for to in ALL_FORMATS {
                let channels = 2usize;
                let frames = 2usize;
                let src = vec![0u8; frames * channels * from.bytes()];
                let want_len = frames * channels * to.bytes();
                assert_eq!(
                    converted_len(from, to, channels, src.len()),
                    Some(want_len),
                    "converted_len {from:?}->{to:?}"
                );
                let out = convert_interleaved_vec(from, to, channels, &src)
                    .unwrap_or_else(|| panic!("convert {from:?}->{to:?}"));
                assert_eq!(out.len(), want_len, "output length {from:?}->{to:?}");
                let silence_ok = match to {
                    SampleFormat::U8 => out.iter().all(|&b| b == 128),
                    _ => out.iter().all(|&b| b == 0),
                };
                let src_silence = match from {
                    SampleFormat::U8 => src.iter().all(|&b| b == 128),
                    _ => src.iter().all(|&b| b == 0),
                };
                if src_silence {
                    assert!(silence_ok, "silence preserved {from:?}->{to:?}: {out:?}");
                }
            }
        }
    }

    #[test]
    fn u8_zero_byte_is_full_scale_negative_not_silence() {
        assert_eq!(
            one(SampleFormat::U8, SampleFormat::S16, &[0]),
            (-32768i16).to_le_bytes().to_vec()
        );
        assert_eq!(one(SampleFormat::U8, SampleFormat::S16, &[128]), 0i16.to_le_bytes().to_vec());
    }

    #[test]
    fn rejects_ragged_input_and_small_output() {
        assert_eq!(
            convert_interleaved_vec(SampleFormat::S16, SampleFormat::U8, 2, &[0, 0, 0]),
            None
        );
        let src = [0u8; 8]; // 4 s16 samples
        let mut tiny = [0u8; 1];
        assert_eq!(
            convert_interleaved(SampleFormat::S16, SampleFormat::S32, 1, &src, &mut tiny),
            None
        );
        assert_eq!(converted_len(SampleFormat::S24, SampleFormat::S16, 2, 5), None);
    }

    #[test]
    fn interleaving_is_preserved_across_channels() {
        let mut src = Vec::new();
        for _ in 0..2 {
            src.extend_from_slice(&1000i16.to_le_bytes());
            src.extend_from_slice(&(-2000i16).to_le_bytes());
        }
        let out = convert_interleaved_vec(SampleFormat::S16, SampleFormat::S32, 2, &src).unwrap();
        let mut got = Vec::new();
        for c in out.as_chunks::<4>().0 {
            got.push(i32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        }
        assert_eq!(got, vec![1000i32 << 16, -2000i32 << 16, 1000i32 << 16, -2000i32 << 16]);
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        let mut src = Vec::new();
        src.extend_from_slice(&111i16.to_le_bytes());
        src.extend_from_slice(&(-222i16).to_le_bytes());
        let mut out = vec![0u8; 8];
        let n = remap_channels(SampleFormat::S16, 1, 2, &src, &mut out).unwrap();
        assert_eq!(n, 8);
        let s: Vec<i16> =
            out.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(s, vec![111, 111, -222, -222]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let mut src = Vec::new();
        src.extend_from_slice(&1000i16.to_le_bytes());
        src.extend_from_slice(&2000i16.to_le_bytes());
        src.extend_from_slice(&(-100i16).to_le_bytes());
        src.extend_from_slice(&100i16.to_le_bytes());
        let mut out = vec![0u8; 4];
        remap_channels(SampleFormat::S16, 2, 1, &src, &mut out).unwrap();
        let s: Vec<i16> =
            out.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(s, vec![1500, 0]);
    }

    #[test]
    fn stereo_to_mono_float_averages() {
        let mut src = Vec::new();
        src.extend_from_slice(&0.5f32.to_le_bytes());
        src.extend_from_slice(&(-0.5f32).to_le_bytes());
        let mut out = vec![0u8; 4];
        remap_channels(SampleFormat::F32, 2, 1, &src, &mut out).unwrap();
        let v = f32::from_le_bytes([out[0], out[1], out[2], out[3]]);
        assert!(v.abs() < 1e-6, "average of 0.5 and -0.5 is 0.0, got {v}");
    }

    #[test]
    fn remap_identity_and_unsupported() {
        let src = [1u8, 2, 3, 4];
        let mut out = [0u8; 4];
        assert_eq!(remap_channels(SampleFormat::U8, 2, 2, &src, &mut out), Some(4));
        assert_eq!(out, src);
        let src3 = vec![0u8; 6];
        let mut out3 = vec![0u8; 4];
        assert_eq!(remap_channels(SampleFormat::U8, 3, 2, &src3, &mut out3), None);
    }

    #[test]
    fn convert_uses_audioformat_channels() {
        let f = AudioFormat::new(48_000, 2, SampleFormat::S16);
        let src = vec![0u8; 4 * f.channels as usize]; // 4 bytes per frame here
        let out = convert_interleaved_vec(f.format, SampleFormat::S32, f.channels as usize, &src);
        assert!(out.is_some());
    }

    #[test]
    fn downmix_51_to_stereo_matrix() {
        let frame: [i16; 6] = [1000, 2000, 3000, 32000 /* LFE, dropped */, 400, 500];
        let mut input = Vec::new();
        for s in frame {
            input.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = vec![0u8; 4]; // 1 frame, 2 ch, 2 bytes
        assert_eq!(downmix_to_stereo(SampleFormat::S16, 6, &input, &mut out), Some(4));
        let lo = i16::from_le_bytes([out[0], out[1]]);
        let ro = i16::from_le_bytes([out[2], out[3]]);
        let m = std::f32::consts::FRAC_1_SQRT_2;
        let exp_lo = (1000.0 + m * (3000.0 + 400.0)).round() as i16;
        let exp_ro = (2000.0 + m * (3000.0 + 500.0)).round() as i16;
        assert!((lo - exp_lo).abs() <= 1, "Lo {lo} vs {exp_lo}");
        assert!((ro - exp_ro).abs() <= 1, "Ro {ro} vs {exp_ro}");
    }

    fn s16_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn a_layout_the_fixed_fold_reads_goes_through_it() {
        use crate::layout::*;
        let side = s16_bytes(&[1000, 2000, 3000, 32000, 400, 500]);
        let (mut a, mut b) = ([0u8; 4], [0u8; 4]);
        downmix_to_stereo(SampleFormat::S16, 6, &side, &mut a).unwrap();
        downmix_layout_to_stereo(SampleFormat::S16, FL | FR | FC | LFE | SL | SR, 6, &side, &mut b)
            .unwrap();
        assert_eq!(a, b);
        let wide = s16_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]);
        downmix_to_stereo(SampleFormat::S16, 8, &wide, &mut a).unwrap();
        downmix_layout_to_stereo(SampleFormat::S16, default_layout(8), 8, &wide, &mut b).unwrap();
        assert_eq!(a, b);
        assert!(fixed_fold_fits(default_layout(6), 6));
        assert!(!fixed_fold_fits(default_layout(7), 7), "no 7 channel table");
    }

    #[test]
    fn a_layout_past_the_table_folds_each_channel_by_its_position() {
        use crate::layout::*;
        let frame = [1000, 2000, 3000, 32000, 800, 400, 600];
        let input: Vec<u8> =
            frame.iter().flat_map(|s: &i16| (f32::from(*s) / 32768.0).to_le_bytes()).collect();
        let mut out = [0u8; 8];
        assert_eq!(
            downmix_layout_to_stereo(SampleFormat::F32, default_layout(7), 7, &input, &mut out),
            Some(8)
        );
        let lo = f32::from_le_bytes(out[..4].try_into().unwrap()) * 32768.0;
        let ro = f32::from_le_bytes(out[4..].try_into().unwrap()) * 32768.0;
        let m = M3DB;
        assert!((lo - (1000.0 + m * 3000.0 + 0.5 * 800.0 + m * 400.0)).abs() < 0.01, "Lo {lo}");
        assert!((ro - (2000.0 + m * 3000.0 + 0.5 * 800.0 + m * 600.0)).abs() < 0.01, "Ro {ro}");
        let input = s16_bytes(&[1000, 2000, 400, 600]);
        let mut out = [0u8; 4];
        downmix_layout_to_stereo(SampleFormat::S16, FL | FR | FLC | FRC, 4, &input, &mut out)
            .unwrap();
        let lo = i16::from_le_bytes([out[0], out[1]]);
        assert!((f32::from(lo) - (1000.0 + m * 400.0)).abs() <= 1.0, "Lo {lo}");
    }

    #[test]
    fn downmix_clamps_instead_of_wrapping() {
        let frame: [i16; 6] = [i16::MAX, i16::MAX, i16::MAX, 0, i16::MAX, i16::MAX];
        let mut input = Vec::new();
        for s in frame {
            input.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = vec![0u8; 4];
        downmix_to_stereo(SampleFormat::S16, 6, &input, &mut out).unwrap();
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), i16::MAX); // clamped, not wrapped
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), i16::MAX);
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 29) as u8
            })
            .collect()
    }

    fn reference_convert(from: SampleFormat, to: SampleFormat, input: &[u8]) -> Vec<u8> {
        let (ib, ob) = (from.bytes(), to.bytes());
        let mut out = vec![0u8; (input.len() / ib) * ob];
        let (mut si, mut di) = (0usize, 0usize);
        for _ in 0..input.len() / ib {
            convert_sample(from, to, &input[si..si + ib], &mut out[di..di + ob]);
            si += ib;
            di += ob;
        }
        out
    }

    #[test]
    fn dispatch_hoist_is_byte_exact_for_every_pair() {
        let src = noise(48_000);
        for from in ALL_FORMATS {
            for to in ALL_FORMATS {
                let n = (src.len() / (from.bytes() * 2)) * from.bytes() * 2; // whole stereo frames
                let got = convert_interleaved_vec(from, to, 2, &src[..n]).unwrap();
                let want = if from == to {
                    src[..n].to_vec()
                } else {
                    reference_convert(from, to, &src[..n])
                };
                assert_eq!(got, want, "{from:?}->{to:?} must be byte-identical after the hoist");
            }
        }
    }

    fn reference_downmix(fmt: SampleFormat, in_channels: usize, input: &[u8]) -> Vec<u8> {
        let bps = fmt.bytes();
        let in_stride = bps * in_channels;
        let frames = input.len() / in_stride;
        let mut output = vec![0u8; frames * bps * 2];
        let sample = |f: usize, c: usize| -> f32 {
            let b = &input[f * in_stride + c * bps..][..bps];
            if fmt.is_float() {
                decode_f32(b)
            } else {
                decode_i32_scale32(fmt, b) as f32 / 2_147_483_648.0
            }
        };
        for f in 0..frames {
            let (lo, ro) = match in_channels {
                3 => (sample(f, 0) + M3DB * sample(f, 2), sample(f, 1) + M3DB * sample(f, 2)),
                4 => (sample(f, 0) + M3DB * sample(f, 2), sample(f, 1) + M3DB * sample(f, 3)),
                5 | 6 => {
                    let (ls_i, rs_i) = if in_channels == 6 { (4, 5) } else { (3, 4) };
                    let c = sample(f, 2);
                    (
                        sample(f, 0) + M3DB * (c + sample(f, ls_i)),
                        sample(f, 1) + M3DB * (c + sample(f, rs_i)),
                    )
                }
                8 => {
                    let c = sample(f, 2);
                    (
                        sample(f, 0) + M3DB * (c + sample(f, 4) + sample(f, 6)),
                        sample(f, 1) + M3DB * (c + sample(f, 5) + sample(f, 7)),
                    )
                }
                _ => (sample(f, 0), sample(f, 1)),
            };
            for (oc, v) in [lo, ro].into_iter().enumerate() {
                let d = &mut output[(f * 2 + oc) * bps..][..bps];
                if fmt.is_float() {
                    d[..4].copy_from_slice(&v.clamp(-1.0, 1.0).to_le_bytes());
                } else {
                    let scaled = (v * 2_147_483_648.0).clamp(i32::MIN as f32, i32::MAX as f32);
                    encode_i32_scale32(fmt, scaled as i32, d);
                }
            }
        }
        output
    }

    fn pcm_eq(fmt: SampleFormat, a: &[u8], b: &[u8]) -> Result<(), usize> {
        if a.len() != b.len() {
            return Err(a.len().min(b.len()));
        }
        if !fmt.is_float() {
            return a.iter().zip(b).position(|(x, y)| x != y).map_or(Ok(()), Err);
        }
        let nan = |s: &[u8; 4]| f32::from_le_bytes(*s).is_nan();
        let same = |(x, y): (&[u8; 4], &[u8; 4])| x == y || (nan(x) && nan(y));
        let (fa, fb) = (a.as_chunks::<4>().0, b.as_chunks::<4>().0);
        fa.iter().zip(fb).position(|p| !same(p)).map_or(Ok(()), Err)
    }

    #[test]
    fn pcm_eq_tolerates_only_nan_payloads() {
        let f = |v: &[u32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let ok = f(&[0x3f80_0000, 0x7fea_f4fa]);
        assert_eq!(pcm_eq(SampleFormat::F32, &ok, &f(&[0x3f80_0000, 0x7fd4_48b2])), Ok(()));
        assert_eq!(pcm_eq(SampleFormat::F32, &ok, &f(&[0x3f80_0001, 0x7fea_f4fa])), Err(0));
        assert_eq!(pcm_eq(SampleFormat::F32, &ok, &f(&[0x3f80_0000, 0x3f80_0000])), Err(1));
        assert_eq!(pcm_eq(SampleFormat::S32, &ok, &f(&[0x3f80_0000, 0x7fd4_48b2])), Err(4));
    }

    #[test]
    fn downmix_and_remap_hoists_are_byte_exact() {
        let src = noise(60_000);
        for fmt in ALL_FORMATS {
            for ch in [3usize, 4, 5, 6, 7, 8] {
                let stride = fmt.bytes() * ch;
                let n = (src.len() / stride) * stride;
                let mut got = vec![0u8; (n / stride) * fmt.bytes() * 2];
                downmix_to_stereo(fmt, ch, &src[..n], &mut got).unwrap();
                assert_eq!(
                    pcm_eq(fmt, &got, &reference_downmix(fmt, ch, &src[..n])),
                    Ok(()),
                    "downmix {fmt:?} {ch}ch must be byte-identical after the hoist"
                );
            }
            let stride = fmt.bytes() * 2;
            let n = (src.len() / stride) * stride;
            let mut mono = vec![0u8; n / 2];
            remap_channels(fmt, 2, 1, &src[..n], &mut mono).unwrap();
            let mut want = vec![0u8; n / 2];
            for (f, d) in want.chunks_exact_mut(fmt.bytes()).enumerate() {
                let base = f * stride;
                let (l, r) =
                    (&src[base..][..fmt.bytes()], &src[base + fmt.bytes()..][..fmt.bytes()]);
                if fmt.is_float() {
                    d.copy_from_slice(&((decode_f32(l) + decode_f32(r)) * 0.5).to_le_bytes());
                } else {
                    let a = decode_i32_scale32(fmt, l) as i64;
                    let b = decode_i32_scale32(fmt, r) as i64;
                    encode_i32_scale32(fmt, ((a + b) / 2) as i32, d);
                }
            }
            let same = pcm_eq(fmt, &mono, &want);
            assert_eq!(same, Ok(()), "stereo->mono {fmt:?} must be byte-identical");

            let mut stereo = vec![0u8; n * 2];
            remap_channels(fmt, 1, 2, &src[..n], &mut stereo).unwrap();
            for (f, d) in stereo.chunks_exact(stride).enumerate() {
                let s = &src[f * fmt.bytes()..][..fmt.bytes()];
                assert_eq!(&d[..fmt.bytes()], s, "mono->stereo L at {f}");
                assert_eq!(&d[fmt.bytes()..], s, "mono->stereo R at {f}");
            }
        }
    }

    #[test]
    fn downmix_unknown_layout_takes_front_pair() {
        let frame: [i16; 7] = [111, 222, 333, 444, 555, 666, 777];
        let mut input = Vec::new();
        for s in frame {
            input.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = vec![0u8; 4];
        downmix_to_stereo(SampleFormat::S16, 7, &input, &mut out).unwrap();
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 111);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 222);
    }

    #[test]
    fn the_clip_gain_holds_a_full_scale_fold_within_full_scale() {
        use crate::layout::*;
        let five_one = FL | FR | FC | LFE | BL | BR;
        let g = downmix_clip_gain(five_one, 6);
        assert!((g - 1.0 / (1.0 + 2.0 * M3DB)).abs() < 1e-6, "{g}");
        let seven_one = FL | FR | FC | LFE | BL | BR | SL | SR;
        let g7 = downmix_clip_gain(seven_one, 8);
        assert!((g7 - 1.0 / (1.0 + 3.0 * M3DB)).abs() < 1e-6, "{g7}");
        assert_eq!(downmix_clip_gain(FL | FR, 2), 1.0);
        assert_eq!(downmix_clip_gain(0, 6), 1.0, "no layout, no gain");
        // full scale on every channel clamps unscaled and lands at full scale scaled
        let loud: Vec<u8> = (0..6).flat_map(|_| (-32768i16).to_le_bytes()).collect();
        let mut out = vec![0u8; 4];
        downmix_layout_to_stereo(SampleFormat::S16, five_one, 6, &loud, &mut out).unwrap();
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), -32768);
        let mut out = vec![0u8; 4];
        downmix_layout_to_stereo_scaled(SampleFormat::S16, five_one, 6, &loud, &mut out, g)
            .unwrap();
        let (l, r) = (i16::from_le_bytes([out[0], out[1]]), i16::from_le_bytes([out[2], out[3]]));
        assert!((-32768..=-32700).contains(&l) && l == r, "{l} {r}");
        // the fronts alone come out at the gain
        let front: Vec<u8> =
            [32767i16, 0, 0, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = vec![0u8; 4];
        downmix_layout_to_stereo_scaled(SampleFormat::S16, five_one, 6, &front, &mut out, g)
            .unwrap();
        let l = i16::from_le_bytes([out[0], out[1]]);
        assert!((l - (32767.0 * g) as i16).abs() <= 1, "{l}");
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 0);
    }

    #[test]
    fn a_stereo_fold_to_mono_is_the_mean_of_both_sides() {
        let frames: Vec<u8> = [(32767i16, -32768i16), (10000, 20000), (-32768, -32768)]
            .iter()
            .flat_map(|(l, r)| [l.to_le_bytes(), r.to_le_bytes()].concat())
            .collect();
        let mut out = vec![0u8; 6];
        assert_eq!(fold_stereo_to_mono(SampleFormat::S16, &frames, &mut out), Some(6));
        let got: Vec<i16> = out.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
        assert!(got[0].abs() <= 1, "{got:?}");
        assert!((got[1] - 15000).abs() <= 1, "{got:?}");
        assert_eq!(got[2], -32768);
        assert_eq!(fold_stereo_to_mono(SampleFormat::S16, &frames[..3], &mut out), None);
    }
}
