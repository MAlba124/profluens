#![allow(dead_code)]

use pf_harness::Harness;
use profluens_core::buffer::Buffer;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::id::SinkPad;
use profluens_core::time::Timestamp;

/// A stream as its decoder is fed it, and the shape of what comes out.
pub struct Stream<'a> {
    pub pieces: &'a [&'a [u8]],
    pub rate: u32,
    pub stride: usize,
    /// Sets the pts of input buffer `index`, for a decoder that reads it.
    pub stamp: &'a dyn Fn(usize, &mut Buffer),
    /// The grid sample the first frame out is stamped as.
    pub first: u64,
}

/// How a decoder comes back from a restart at a packet.
#[derive(Clone, Copy)]
pub struct Settle {
    /// Frames past the restart that differ from an unbroken decode, for a stateful codec.
    pub frames: u64,
    /// How far an `s16` sample past that may be from the unbroken decode's.
    pub within: i32,
}

pub struct Pcm {
    pub pts: Timestamp,
    pub duration: Timestamp,
    pub bytes: Vec<u8>,
}

fn take(buffers: Vec<Buffer>) -> Vec<Pcm> {
    let pcm =
        |b: &Buffer| Pcm { pts: b.pts, duration: b.duration, bytes: b.memory.data().to_vec() };
    buffers.iter().map(pcm).collect()
}

pub fn grid(n: u64, rate: u32) -> Timestamp {
    Timestamp((u128::from(n) * 1_000_000_000 / u128::from(rate)) as u64)
}

pub fn sample_at_or_after(point: Timestamp, rate: u32) -> u64 {
    (u128::from(point.0) * u128::from(rate)).div_ceil(1_000_000_000) as u64
}

/// `(rate, channels)` a decoder announces for `pieces`, from a run of its own.
pub fn announced(
    new: &dyn Fn() -> Harness,
    pieces: &[&[u8]],
    stamp: &dyn Fn(usize, &mut Buffer),
) -> (u32, usize) {
    let probe = Stream { pieces, rate: 1, stride: 1, stamp, first: 0 };
    let mut h = new();
    probe.feed(&mut h, 0..pieces.len());
    let format = h.announced().expect("a format is announced");
    let int = |name| match format.get(h.vocabulary().field_id(name).expect("field")) {
        Some(Value::Int(v)) => v,
        other => panic!("{name} is {other:?}"),
    };
    (int("rate") as u32, int("channels") as usize)
}

impl Stream<'_> {
    /// Push pieces `range`, cranking after each until a call puts nothing out.
    pub fn feed(&self, h: &mut Harness, range: std::ops::Range<usize>) -> Vec<Pcm> {
        let mut out = Vec::new();
        for index in range {
            let mut buffer = h.alloc(self.pieces[index]);
            (self.stamp)(index, &mut buffer);
            h.push(buffer).expect("push");
            for _ in 0..self.pieces[index].len() + 8 {
                let step = take(h.drain_outputs());
                if step.is_empty() {
                    break;
                }
                out.extend(step);
                h.crank().expect("crank");
            }
        }
        out
    }

    pub fn finish(&self, h: &mut Harness) -> Vec<Pcm> {
        take(h.eos().expect("eos"))
    }

    pub fn full(&self, new: &dyn Fn() -> Harness) -> Vec<u8> {
        let mut h = new();
        let mut out = self.feed(&mut h, 0..self.pieces.len());
        out.extend(self.finish(&mut h));
        let mut at = self.first;
        for pcm in &out {
            assert_eq!(pcm.bytes.len() % self.stride, 0, "whole frames");
            assert_eq!(pcm.pts, grid(at, self.rate), "stamped by sample count");
            at += (pcm.bytes.len() / self.stride) as u64;
            self.ends_at(pcm, at, "full decode");
        }
        out.into_iter().flat_map(|pcm| pcm.bytes).collect()
    }

    fn skip(&self, new: &dyn Fn() -> Harness, full: &[u8], fed: usize, point: Timestamp) {
        let mut h = new();
        let before: Vec<u8> = self.feed(&mut h, 0..fed).into_iter().flat_map(|p| p.bytes).collect();
        assert!(before == full[..before.len()], "what was out before the skip stands");
        h.skip_to(point).expect("skip");
        let mut after = self.feed(&mut h, fed..self.pieces.len());
        after.extend(self.finish(&mut h));

        let end = self.first + (full.len() / self.stride) as u64;
        let position = self.first + (before.len() / self.stride) as u64;
        let start = sample_at_or_after(point, self.rate).max(position).min(end);
        let what = format!("{fed} pieces fed, skip to {point:?}, start at sample {start}");
        if let Some(first) = after.first() {
            assert!(first.pts >= point || start == position, "{what}");
        }
        let tail = self.counted_from(&after, start, &what);
        assert!(tail == full[self.offset(start)..], "{what}, the samples differ");
    }

    fn offset(&self, n: u64) -> usize {
        (n - self.first) as usize * self.stride
    }

    fn counted_from(&self, out: &[Pcm], start: u64, what: &str) -> Vec<u8> {
        let mut at = start;
        for pcm in out {
            assert_eq!(pcm.pts, grid(at, self.rate), "{what}");
            at += (pcm.bytes.len() / self.stride) as u64;
            self.ends_at(pcm, at, what);
        }
        out.iter().flat_map(|pcm| pcm.bytes.iter().copied()).collect()
    }

    fn pts_of(&self, h: &mut Harness, index: usize) -> Timestamp {
        let mut buffer = h.alloc(&[]);
        (self.stamp)(index, &mut buffer);
        buffer.pts
    }

    fn seek(
        &self,
        new: &dyn Fn() -> Harness,
        full: &[u8],
        (packet, at, running): (usize, u64, usize),
        (timed, point): (Timestamp, Timestamp),
        settle: Settle,
    ) {
        let mut h = new();
        drop(self.feed(&mut h, 0..running));
        h.seek(0, timed).expect("seek");
        h.skip_to(point).expect("skip");
        let mut out = self.feed(&mut h, packet..self.pieces.len());
        out.extend(self.finish(&mut h));

        let end = self.first + (full.len() / self.stride) as u64;
        let start = sample_at_or_after(point, self.rate).max(at).min(end);
        let what = format!("restart at piece {packet}, seek timed {timed:?}, skip to {point:?}");
        let got = self.counted_from(&out, start, &what);
        let owed = &full[self.offset(start)..];
        assert_eq!(got.len(), owed.len(), "{what}, from sample {start} to the end");
        let settled = self.offset((at + settle.frames).clamp(start, end)) - self.offset(start);
        if settle.within == 0 {
            assert!(got[settled..] == owed[settled..], "{what}, the samples differ");
            return;
        }
        let s16 = |b: &[u8]| -> Vec<i32> {
            b.as_chunks::<2>().0.iter().map(|s| i32::from(i16::from_le_bytes(*s))).collect()
        };
        let (got, owed) = (s16(&got[settled..]), s16(&owed[settled..]));
        let worst = got.iter().zip(&owed).map(|(a, b)| (a - b).abs()).max().unwrap_or(0);
        assert!(worst <= settle.within, "{what}, {worst} off once settled");
    }

    /// Restarts at `restarts`, with points on, inside, past and before each first sample.
    pub fn check_seeks(
        &self,
        new: &dyn Fn() -> Harness,
        (head, restarts): (usize, &[usize]),
        start_of: &dyn Fn(usize) -> u64,
        settle: Settle,
    ) {
        let full = self.full(new);
        assert!(self.pieces.len() >= head + 3, "three packets to be running on");
        for &packet in restarts {
            let (at, next) = (start_of(packet), start_of(packet + 1));
            let half = (next - at) / 2;
            let pts = self.pts_of(&mut new(), packet);
            let timings =
                [pts, grid(at.saturating_sub(half), self.rate), grid(at + half, self.rate)];
            for sample in
                [at, at + 1, at + half, next, next + 3 * half, at.saturating_sub(2 * half)]
            {
                for nudge in [0u64, 1] {
                    let point = Timestamp(grid(sample, self.rate).0 + nudge);
                    for timed in timings {
                        let places = (packet, at, head + 3);
                        self.seek(new, &full, places, (timed, point), settle);
                    }
                }
            }
        }
    }

    fn off_from_the_point(
        &self,
        new: &dyn Fn() -> Harness,
        full: &[u8],
        (packet, at, running): (usize, u64, usize),
        past: u64,
    ) -> Option<i32> {
        let point = grid(at + past, self.rate);
        let mut h = new();
        drop(self.feed(&mut h, 0..running));
        h.seek(0, self.pts_of(&mut new(), packet)).expect("seek");
        h.skip_to(point).expect("skip");
        let mut out = self.feed(&mut h, packet..self.pieces.len());
        out.extend(self.finish(&mut h));
        let end = self.first + (full.len() / self.stride) as u64;
        let start = (at + past).min(end);
        let what = format!("restart at piece {packet}, the point {past} frames past it");
        let got = self.counted_from(&out, start, &what);
        let owed = &full[self.offset(start)..];
        let s16 = |b: &[u8]| -> Vec<i32> {
            b.as_chunks::<2>().0.iter().map(|s| i32::from(i16::from_le_bytes(*s))).collect()
        };
        (got.len() == owed.len())
            .then(|| s16(&got).iter().zip(s16(owed)).map(|(a, b)| (a - b).abs()).max().unwrap_or(0))
    }

    /// How far past a restart the point must be for the decoder to be primed there.
    pub fn primed_after(
        &self,
        new: &dyn Fn() -> Harness,
        (head, restarts): (usize, &[usize]),
        start_of: &dyn Fn(usize) -> u64,
        (step, most, within): (u64, u64, i32),
    ) -> Option<u64> {
        let full = self.full(new);
        let mut needed = 0;
        for &packet in restarts {
            let places = (packet, start_of(packet), head + 3);
            for past in (0..=most).step_by(step as usize) {
                let off = self.off_from_the_point(new, &full, places, past);
                if off.is_none_or(|off| off > within) {
                    needed = needed.max(past + step);
                }
            }
        }
        (needed <= most).then_some(needed)
    }

    /// A container that declares a delay (`profluens_audio::place`).
    pub fn check_delays(&self, new: &dyn Fn() -> Harness, family: &str, delays: &[Timestamp]) {
        let full = self.full(new);
        let total = (full.len() / self.stride) as u64;
        for delay in delays {
            let frames =
                (u128::from(delay.0) * u128::from(self.rate) + 500_000_000) / 1_000_000_000;
            let frames = frames.min(u128::from(u64::MAX)) as u64;
            let mut h = new();
            let declared = [("delay", ValueDesc::Int(delay.0 as i64))];
            h.fix_sink_format(SinkPad(0), family, &declared);
            let mut out = self.feed(&mut h, 0..self.pieces.len());
            out.extend(self.finish(&mut h));
            let lead = frames.saturating_sub(self.first).min(total) as usize;
            let what = format!("a delay of {} ns, {frames} frames", delay.0);
            let got = self.counted_from(&out, self.first.saturating_sub(frames), &what);
            assert_eq!(got.len(), full.len() - lead * self.stride, "{what}");
            assert!(got == full[lead * self.stride..], "{what}, the samples differ");
        }
    }

    fn ends_at(&self, pcm: &Pcm, end: u64, what: &str) {
        let (want, got) = (grid(end, self.rate), pcm.pts.saturating_add(pcm.duration));
        assert!(got <= want && want.0 - got.0 <= 1, "{what}, ends {got:?} for {want:?}");
    }

    /// Skips from `places` places to `3 * spread` points all over the stream.
    pub fn check_skips(&self, new: &dyn Fn() -> Harness, places: usize, spread: u64) {
        let full = self.full(new);
        let total = (full.len() / self.stride) as u64;
        assert!(total > 0, "the stream decodes to something");
        let end = self.first + total;
        let mut points = vec![Timestamp::ZERO, grid(end, self.rate), grid(end + 999, self.rate)];
        for k in 1..spread {
            let on = grid(self.first + total * k / spread + k, self.rate);
            points.extend([on, Timestamp(on.0 + 1), Timestamp(on.0.saturating_sub(1))]);
        }
        let step = self.pieces.len().div_ceil(places).max(1);
        for fed in (0..=self.pieces.len()).step_by(step) {
            for &point in &points {
                self.skip(new, &full, fed, point);
            }
        }
    }
}

/// Delays worth declaring for a stream at `rate`.
pub fn delays(rate: u32) -> [Timestamp; 5] {
    let hours = Timestamp::from_secs(7_200);
    [grid(1105, rate), grid(1024, rate), grid(3_000, rate), hours, Timestamp(i64::MAX as u64)]
}
