//! Cutting PCM to the presentation point.

use profluens_core::ctx::Ctx;
use profluens_core::format::Value;
use profluens_core::id::SinkPad;
use profluens_core::time::Timestamp;

use crate::format::{AudioFormat, FIELD_DELAY};

const NANOS: u128 = 1_000_000_000;

fn sample_at_or_after(t: Timestamp, rate: u32) -> u128 {
    (u128::from(t.0) * u128::from(rate)).div_ceil(NANOS)
}

fn stamp(n: u128, rate: u32) -> Timestamp {
    let ns = n * NANOS / u128::from(rate);
    Timestamp(ns.min(u128::from(Timestamp::MAX.0)) as u64)
}

/// The grid sample a counting decoder is at after a seek to `time`, the first at or after it.
pub fn position_at(time: Timestamp, rate: u32) -> u64 {
    sample_at_or_after(time, rate) as u64
}

/// The stamp of grid sample `position`, the inverse of [`position_at`].
pub fn time_at(position: u64, rate: u32) -> Timestamp {
    if rate == 0 {
        return Timestamp::NONE;
    }
    stamp(u128::from(position), rate)
}

/// Stamp and duration of `frames` frames starting `offset` frames into a span stamped `pts`.
pub fn span_at(pts: Timestamp, offset: usize, frames: usize, rate: u32) -> (Timestamp, Timestamp) {
    if pts.is_none() || rate == 0 {
        return (Timestamp::NONE, Timestamp::NONE);
    }
    let first = sample_at_or_after(pts, rate) + offset as u128;
    let start = if offset == 0 { pts } else { stamp(first, rate) };
    (start, stamp(first + frames as u128, rate).saturating_sub(start))
}

/// Where a decoder that stamps by counting samples takes up its count.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Rebase {
    #[default]
    Open,
    At(Timestamp),
    Lost(Timestamp),
    Counting,
}

impl Rebase {
    pub fn input(&mut self, pts: Timestamp) {
        if matches!(*self, Rebase::Open | Rebase::Lost(_)) && pts.is_some() {
            *self = Rebase::At(pts);
        }
    }

    /// The input was used up without output, so its stamp is not the next frame's.
    pub fn dropped(&mut self) {
        if let Rebase::At(pts) = *self {
            *self = Rebase::Lost(pts);
        }
    }

    pub fn first_frame(&mut self, rate: u32) -> Option<u64> {
        let at = match *self {
            Rebase::At(pts) | Rebase::Lost(pts) if rate != 0 => Some(position_at(pts, rate)),
            _ => None,
        };
        *self = Rebase::Counting;
        at
    }

    /// [`first_frame`](Self::first_frame) placed by the delay the format on `pad` declares.
    pub fn place_first(&mut self, ctx: &Ctx, pad: SinkPad, rate: u32) -> Option<Place> {
        let stamped = self.first_frame(rate)?;
        Some(place(stamped, delay_frames(declared_delay(ctx, pad), rate)))
    }
}

/// The delay the format on `pad` declares ([`FIELD_DELAY`]), zero for none.
pub fn declared_delay(ctx: &Ctx, pad: SinkPad) -> Timestamp {
    let declared = ctx.field_id(FIELD_DELAY).and_then(|id| ctx.negotiated_sink(pad)?.get(id));
    match declared {
        Some(Value::Int(ns)) if ns > 0 => Timestamp(ns as u64),
        _ => Timestamp::ZERO,
    }
}

pub fn delay_frames(delay: Timestamp, rate: u32) -> u64 {
    if delay.is_none() {
        return 0;
    }
    ((u128::from(delay.0) * u128::from(rate) + NANOS / 2) / NANOS) as u64
}

/// Where a decoder stands on its first frame when its input is stamped `delay` frames ahead.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Place {
    pub position: u64,
    /// Frames ahead of it that fall before time zero.
    pub lead: u64,
}

/// Place a frame decoded from a packet stamped as grid sample `stamped`.
pub fn place(stamped: u64, delay: u64) -> Place {
    Place { position: stamped.saturating_sub(delay), lead: delay.saturating_sub(stamped) }
}

impl Place {
    pub fn cut(&mut self, frames: usize, rate: u32, point: Option<Timestamp>) -> usize {
        let lead = self.lead.min(frames as u64) as usize;
        self.lead -= lead as u64;
        let before = frames_before(self.position, frames - lead, rate, point);
        self.position += before as u64;
        lead + before
    }
}

/// How many of `frames` frames at `rate`, starting at grid sample `position`, are before `point`.
pub fn frames_before(position: u64, frames: usize, rate: u32, point: Option<Timestamp>) -> usize {
    let Some(point) = point.filter(|p| p.is_some() && rate != 0) else {
        return 0;
    };
    let before = sample_at_or_after(point, rate).saturating_sub(u128::from(position));
    before.min(frames as u128) as usize
}

/// Where a span of PCM starts once what is before the presentation point is cut.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Trim {
    pub skip: usize,
    /// The stamp of the first frame kept, or of the end when none is.
    pub pts: Timestamp,
    pub duration: Timestamp,
}

/// Cut a span of `frames` frames at `rate`, stamped `pts`, to `point`.
pub fn trim_frames(pts: Timestamp, frames: usize, rate: u32, point: Option<Timestamp>) -> Trim {
    let whole = Trim { skip: 0, pts, duration: Timestamp::NONE };
    if pts.is_none() || rate == 0 {
        return whole;
    }
    let first = sample_at_or_after(pts, rate);
    let end = stamp(first + frames as u128, rate);
    let whole = Trim { duration: end.saturating_sub(pts), ..whole };
    let skip = frames_before(first as u64, frames, rate, point);
    if skip == 0 {
        return whole;
    }
    let pts = stamp(first + skip as u128, rate);
    Trim { skip, pts, duration: end.saturating_sub(pts) }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Trimmed<'a> {
    pub pcm: &'a [u8],
    pub pts: Timestamp,
    pub duration: Timestamp,
    pub skipped: usize,
}

/// Cut interleaved PCM in `format`, stamped `pts`, to `point`.
pub fn trim_pcm<'a>(
    pcm: &'a [u8],
    format: AudioFormat,
    pts: Timestamp,
    point: Option<Timestamp>,
) -> Trimmed<'a> {
    let stride = format.frame_stride();
    let frames = pcm.len().checked_div(stride).unwrap_or(0);
    let trim = trim_frames(pts, frames, format.sample_rate, point);
    let cut = trim.skip * stride;
    let pcm = if trim.skip == frames && trim.skip > 0 { &[][..] } else { &pcm[cut..] };
    Trimmed { pcm, pts: trim.pts, duration: trim.duration, skipped: trim.skip }
}

#[cfg(test)]
mod tests {
    // Test code. The allocation ban is for element hot paths.
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use crate::format::SampleFormat;

    fn grid(n: u64, rate: u32) -> Timestamp {
        Timestamp((u128::from(n) * NANOS / u128::from(rate)) as u64)
    }

    fn brute(first: u64, frames: usize, rate: u32, point: Timestamp) -> usize {
        (0..frames).find(|&k| grid(first + k as u64, rate) >= point).unwrap_or(frames)
    }

    const RATES: [u32; 7] = [8_000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000];

    #[test]
    fn a_span_is_cut_to_the_first_sample_at_or_after_the_point() {
        for rate in RATES {
            for first in [0u64, 1, 7, 44_099, 1_000_003] {
                let frames = 12;
                let pts = grid(first, rate);
                for k in 0..=frames as u64 + 1 {
                    let on = grid(first + k, rate);
                    for point in [on.saturating_sub(Timestamp(1)), on, Timestamp(on.0 + 1)] {
                        let trim = trim_frames(pts, frames, rate, Some(point));
                        let skip = brute(first, frames, rate, point);
                        assert_eq!(trim.skip, skip, "rate {rate} first {first} point {point:?}");
                        assert_eq!(frames_before(first, frames, rate, Some(point)), skip);
                        let start = first + skip as u64;
                        let end = grid(first + frames as u64, rate);
                        assert_eq!(trim.pts, if skip == 0 { pts } else { grid(start, rate) });
                        assert_eq!(trim.duration, end.saturating_sub(grid(start, rate)));
                        assert!(skip == frames || trim.pts >= point, "what is kept is not before");
                    }
                }
            }
        }
    }

    #[test]
    fn a_seek_to_the_stamp_of_a_sample_lands_on_that_sample() {
        for rate in RATES {
            for n in [0u64, 1, 2, 7, 1151, 44_099, 66_157, 1_000_003, 400_000_000_000] {
                let on = grid(n, rate);
                assert_eq!(position_at(on, rate), n, "rate {rate} sample {n}");
                assert_eq!(frames_before(position_at(on, rate), 64, rate, Some(on)), 0);
                let after = position_at(Timestamp(on.0 + 1), rate);
                assert!(grid(after, rate) > on && after <= n + 1);
            }
        }
    }

    #[test]
    fn a_count_restarts_from_the_stamp_of_the_input_the_first_frame_came_from() {
        let (rate, pts) = (44_100, grid(66_157, 44_100));
        let mut r = Rebase::default();
        r.input(Timestamp::NONE);
        r.input(pts);
        r.input(grid(70_000, rate));
        assert_eq!(r.first_frame(rate), Some(66_157));
        r.input(grid(1, rate));
        assert_eq!((r, r.first_frame(rate)), (Rebase::Counting, None));
        let mut r = Rebase::Open;
        r.input(Timestamp::NONE);
        assert_eq!(r.first_frame(rate), None);
        let mut r = Rebase::Open;
        r.input(pts);
        r.dropped();
        r.input(grid(67_309, rate));
        assert_eq!(r.first_frame(rate), Some(67_309));
        let mut r = Rebase::At(pts);
        assert_eq!(r.first_frame(0), None);
    }

    #[test]
    fn a_delay_is_the_count_of_samples_it_was_written_from() {
        for rate in RATES {
            for n in [0u64, 1, 312, 1024, 1105, 2112, 5186] {
                let exact = u128::from(n) * NANOS;
                let rate_wide = u128::from(rate);
                let (down, up) = ((exact / rate_wide) as u64, exact.div_ceil(rate_wide) as u64);
                for ns in [down, up] {
                    assert_eq!(delay_frames(Timestamp(ns), rate), n, "rate {rate}, {ns} ns");
                }
            }
        }
        assert_eq!(delay_frames(Timestamp::NONE, 48_000), 0, "no delay");
        assert_eq!(delay_frames(Timestamp::MAX, 1), (u64::MAX / 1_000_000_000) + 1);
        assert!(delay_frames(Timestamp::MAX, MAX_RATE) > 1 << 50);
    }

    #[test]
    fn a_packet_stands_its_delay_before_its_stamp_and_what_falls_before_zero_is_cut() {
        assert_eq!(place(0, 1105), Place { position: 0, lead: 1105 });
        assert_eq!(place(1000, 1105), Place { position: 0, lead: 105 });
        assert_eq!(place(1152, 1105), Place { position: 47, lead: 0 });
        assert_eq!(place(96_000, 0), Place { position: 96_000, lead: 0 });
        let mut at = place(0, 1105);
        assert_eq!(at.cut(1152, 48_000, None), 1105);
        assert_eq!(at, Place { position: 0, lead: 0 });
        let mut at = place(0, 1500);
        assert_eq!((at.cut(1152, 48_000, None), at.cut(1152, 48_000, None)), (1152, 348));
        assert_eq!(at, Place { position: 0, lead: 0 });
        let mut at = place(0, 100);
        assert_eq!(at.cut(200, 48_000, Some(grid(50, 48_000))), 150);
        assert_eq!(at, Place { position: 50, lead: 0 });
    }

    #[test]
    fn a_delay_longer_than_the_stream_cuts_all_of_it_and_overflows_nothing() {
        for delay in [delay_frames(Timestamp::from_secs(7_200), 48_000), u64::MAX] {
            let mut at = place(5, delay);
            for _ in 0..4 {
                assert_eq!(at.cut(1152, 48_000, Some(Timestamp::from_secs(1))), 1152);
            }
            assert_eq!(at.position, 0);
            assert_eq!(at.lead, delay - 5 - 4 * 1152);
        }
        assert_eq!(place(u64::MAX, u64::MAX), Place::default());
    }

    #[test]
    fn a_span_cut_into_buffers_is_stamped_as_one_count() {
        for rate in RATES {
            for first in [0u64, 7, 44_099, 1_000_003, 400_000_000_000] {
                let pts = grid(first, rate);
                assert_eq!(time_at(first, rate), pts);
                for (offset, frames) in [(0usize, 1usize), (0, 1024), (1, 1), (333, 691), (4096, 0)]
                {
                    let (at, lasts) = span_at(pts, offset, frames, rate);
                    let start = first + offset as u64;
                    assert_eq!(at, grid(start, rate), "rate {rate} first {first} offset {offset}");
                    assert_eq!(at.saturating_add(lasts), grid(start + frames as u64, rate));
                    assert_eq!(position_at(at, rate), start, "the next element counts on");
                }
            }
        }
        let pts = Timestamp::from_millis(23);
        assert_eq!(span_at(pts, 0, 1024, 44_100).0, pts);
        assert_eq!(span_at(pts, 5, 10, 44_100).0, grid(1020, 44_100));
        let none = (Timestamp::NONE, Timestamp::NONE);
        assert_eq!(span_at(Timestamp::NONE, 5, 10, 44_100), none, "no pts");
        assert_eq!(span_at(pts, 5, 10, 0), none, "no rate");
        assert_eq!(time_at(5, 0), Timestamp::NONE);
    }

    #[test]
    fn the_point_before_inside_at_the_end_and_after() {
        let (rate, frames) = (48_000, 480);
        let pts = Timestamp::from_millis(100);
        let ten_ms = Timestamp::from_millis(10);
        let cut =
            |point_us: u64| trim_frames(pts, frames, rate, Some(Timestamp::from_micros(point_us)));
        assert_eq!(cut(0), Trim { skip: 0, pts, duration: ten_ms }, "long before");
        assert_eq!(cut(100_000), Trim { skip: 0, pts, duration: ten_ms }, "at the start");
        let half = Trim {
            skip: 240,
            pts: Timestamp::from_millis(105),
            duration: Timestamp::from_millis(5),
        };
        assert_eq!(cut(105_000), half, "in the middle");
        let end = Timestamp::from_millis(110);
        assert_eq!(
            cut(110_000),
            Trim { skip: 480, pts: end, duration: Timestamp::ZERO },
            "at the end"
        );
        assert_eq!(cut(500_000), Trim { skip: 480, pts: end, duration: Timestamp::ZERO }, "after");
    }

    #[test]
    fn a_span_that_cannot_be_placed_is_kept_whole() {
        let point = Some(Timestamp::from_secs(9));
        let pts = Timestamp::from_secs(1);
        assert_eq!(trim_frames(Timestamp::NONE, 480, 48_000, point).skip, 0, "no pts");
        assert_eq!(
            trim_frames(pts, 480, 0, point),
            Trim { skip: 0, pts, duration: Timestamp::NONE }
        );
        assert_eq!(trim_frames(pts, 480, 48_000, None).skip, 0, "no point");
        assert_eq!(trim_frames(pts, 480, 48_000, Some(Timestamp::NONE)).skip, 0, "an unset point");
        assert_eq!(trim_frames(pts, 0, 48_000, point).skip, 0, "no frames");
        assert_eq!(frames_before(0, 480, 48_000, None), 0);
        assert_eq!(frames_before(0, 480, 0, point), 0);
        assert_eq!(frames_before(0, 480, 48_000, Some(Timestamp::NONE)), 0);
        assert_eq!(frames_before(u64::MAX, 480, 48_000, point), 0, "far past the point");
    }

    #[test]
    fn a_pts_off_the_grid_is_placed_on_the_next_sample() {
        let pts = Timestamp::from_millis(23);
        let trim = trim_frames(pts, 1024, 44_100, Some(grid(1020, 44_100)));
        assert_eq!((trim.skip, trim.pts), (5, grid(1020, 44_100)));
        assert_eq!(trim.duration, grid(1015 + 1024, 44_100).saturating_sub(grid(1020, 44_100)));
    }

    #[test]
    fn stamps_at_the_end_of_time_do_not_overflow() {
        let pts = Timestamp(Timestamp::MAX.0 - 5);
        let trim = trim_frames(pts, 1 << 20, MAX_RATE, Some(Timestamp::MAX));
        assert!(trim.pts.is_some() && trim.duration.is_some());
    }

    const MAX_RATE: u32 = crate::format::MAX_SAMPLE_RATE;

    fn pcm(frames: usize, format: AudioFormat) -> Vec<u8> {
        (0..frames).flat_map(|k| vec![k as u8; format.frame_stride()]).collect()
    }

    #[test]
    fn bytes_are_cut_in_whole_frames_for_every_format_and_layout() {
        let formats = [
            SampleFormat::U8,
            SampleFormat::S16,
            SampleFormat::S24,
            SampleFormat::S32,
            SampleFormat::F32,
        ];
        for sample in formats {
            for channels in [1u16, 2, 6] {
                let format = AudioFormat::new(48_000, channels, sample);
                let (frames, stride) = (10usize, format.frame_stride());
                let buf = pcm(frames, format);
                let pts = grid(96, 48_000);
                for k in 0..=frames + 2 {
                    let point = grid(96 + k as u64, 48_000);
                    let got = trim_pcm(&buf, format, pts, Some(point));
                    let skip = k.min(frames);
                    assert_eq!(got.skipped, skip, "{sample:?} x{channels} at frame {k}");
                    assert_eq!(got.pcm, &buf[skip * stride..]);
                    assert_eq!(got.pcm.first().copied(), (skip < frames).then_some(skip as u8));
                    assert_eq!(got.pts, grid(96 + skip as u64, 48_000));
                    assert_eq!(got.duration, grid(106, 48_000).saturating_sub(got.pts));
                }
                let whole = trim_pcm(&buf, format, pts, None);
                assert_eq!((whole.pcm, whole.pts, whole.skipped), (&buf[..], pts, 0));
            }
        }
    }

    #[test]
    fn a_partial_frame_stays_with_the_tail_and_goes_with_a_whole_cut() {
        let format = AudioFormat::new(48_000, 2, SampleFormat::S16);
        let mut buf = pcm(4, format);
        buf.extend([0xEE, 0xEE]);
        let pts = grid(0, 48_000);
        let inside = trim_pcm(&buf, format, pts, Some(grid(3, 48_000)));
        assert_eq!(inside.pcm, &[3, 3, 3, 3, 0xEE, 0xEE]);
        let after = trim_pcm(&buf, format, pts, Some(grid(4, 48_000)));
        assert!(after.pcm.is_empty() && after.skipped == 4);
        let none = AudioFormat::new(48_000, 0, SampleFormat::S16);
        assert_eq!(trim_pcm(&buf, none, pts, Some(grid(4, 48_000))).pcm, &buf[..]);
    }
}
