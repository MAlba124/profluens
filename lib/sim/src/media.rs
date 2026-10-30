//! Declared media and the schedule a fake source serves from it. Media is described, never
//! stored, and a stream's schedule is a pure function of the spec, the stream and a seed.

use crate::prng::Prng;
use crate::record::{MAX_STREAMS, flag};
use crate::time::{NONE, Ns, Rational, frame_at, frame_pts, frames_in};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CueRange {
    pub first: u32,
    pub len: u32,
}

/// `keyframe_interval` 0 and 1 make every frame a keyframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Video { fps: Rational, keyframe_interval: u32 },
    Audio { packet: Ns },
    Text { cues: CueRange },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cue {
    pub start: Ns,
    pub end: Ns,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pacing {
    AsFastAsPossible,
    RealTime,
    Jitter { base: Ns, bound: Ns },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    After(Ns),
    OnSyncPoint(u16),
}

/// Buffer numbers are sequence numbers, so a fault fires again when a seek replays its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    StallAt { buffer: u64, hold: Ns },
    ErrorAt { buffer: u64 },
    EndEarlyAt { buffer: u64 },
    LatePad { delay: Ns },
    BufferingDip { buffer: u64, recovery: Recovery },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFault {
    pub stream: u16,
    pub fault: Fault,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    pub kind: StreamKind,
    pub duration: Ns,
    pub size: u32,
    pub pacing: Pacing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecError {
    Full { capacity: u32 },
    TooManyStreams,
    NoSuchStream { stream: u16 },
}

impl core::fmt::Display for SpecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full { capacity } => write!(f, "spec table full at {capacity} entries"),
            Self::TooManyStreams => write!(f, "a spec holds at most {MAX_STREAMS} streams"),
            Self::NoSuchStream { stream } => write!(f, "no stream {stream} in the spec"),
        }
    }
}

impl std::error::Error for SpecError {}

pub struct MediaSpec {
    pub streams: Vec<StreamSpec>,
    pub cues: Vec<Cue>,
    pub faults: Vec<StreamFault>,
}

impl MediaSpec {
    // One allocation per worker. `reset` and the `add_*` calls refill in place.
    #[allow(clippy::disallowed_methods)]
    pub fn with_capacity(cues: usize, faults: usize) -> Self {
        Self {
            streams: Vec::with_capacity(MAX_STREAMS),
            cues: Vec::with_capacity(cues),
            faults: Vec::with_capacity(faults),
        }
    }

    pub fn reset(&mut self) {
        self.streams.clear();
        self.cues.clear();
        self.faults.clear();
    }

    pub fn add_stream(&mut self, stream: StreamSpec) -> Result<u16, SpecError> {
        if self.streams.len() == MAX_STREAMS {
            return Err(SpecError::TooManyStreams);
        }
        self.streams.push(stream);
        Ok(self.streams.len() as u16 - 1)
    }

    pub fn add_text(&mut self, cues: &[Cue], pacing: Pacing) -> Result<u16, SpecError> {
        let capacity = self.cues.capacity();
        if capacity - self.cues.len() < cues.len() {
            return Err(SpecError::Full { capacity: capacity as u32 });
        }
        let range = CueRange { first: self.cues.len() as u32, len: cues.len() as u32 };
        let duration = cues.iter().map(|cue| cue.end).max().unwrap_or(0);
        let id = self.add_stream(StreamSpec {
            kind: StreamKind::Text { cues: range },
            duration,
            size: 0,
            pacing,
        })?;
        self.cues.extend_from_slice(cues);
        Ok(id)
    }

    pub fn add_fault(&mut self, stream: u16, fault: Fault) -> Result<(), SpecError> {
        if stream as usize >= self.streams.len() {
            return Err(SpecError::NoSuchStream { stream });
        }
        let capacity = self.faults.capacity();
        if self.faults.len() == capacity {
            return Err(SpecError::Full { capacity: capacity as u32 });
        }
        self.faults.push(StreamFault { stream, fault });
        Ok(())
    }
}

pub fn pad_delay(spec: &MediaSpec, stream: u16) -> Ns {
    let late = spec.faults.iter().find_map(|entry| match entry.fault {
        Fault::LatePad { delay } if entry.stream == stream => Some(delay),
        _ => None,
    });
    late.unwrap_or(0)
}

pub fn dip_at(spec: &MediaSpec, stream: u16, buffer: u64) -> Option<Recovery> {
    spec.faults.iter().find_map(|entry| match entry.fault {
        Fault::BufferingDip { buffer: at, recovery } if entry.stream == stream && at == buffer => {
            Some(recovery)
        }
        _ => None,
    })
}

pub mod mark {
    pub const STALLED: u16 = 1 << 0;
    pub const DIP: u16 = 1 << 1;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Slot {
    pub seq: u64,
    pub pts: Ns,
    pub dts: Ns,
    pub duration: Ns,
    pub release: Ns,
    pub size: u32,
    pub flags: u16,
    pub marks: u16,
}

const _: () = assert!(core::mem::size_of::<Slot>() == 48);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndKind {
    Eos,
    EarlyEos,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Slot(Slot),
    End { kind: EndKind, at: Ns },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    NoSuchStream { stream: u16 },
    BadFrameRate { stream: u16 },
    ZeroPacket { stream: u16 },
    CueRange { stream: u16 },
    BadCue { stream: u16, cue: u32 },
    TableFull { capacity: u32 },
}

impl core::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchStream { stream } => write!(f, "no stream {stream} in the spec"),
            Self::BadFrameRate { stream } => write!(f, "stream {stream} has no valid frame rate"),
            Self::ZeroPacket { stream } => write!(f, "stream {stream} has a zero packet duration"),
            Self::CueRange { stream } => write!(f, "stream {stream} names cues outside the table"),
            Self::BadCue { stream, cue } => {
                write!(f, "stream {stream} cue {cue} is unordered or ends before it starts")
            }
            Self::TableFull { capacity } => {
                write!(f, "schedule table full at {capacity} slots before the stream ended")
            }
        }
    }
}

impl std::error::Error for ScheduleError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub stream: u16,
    spec: StreamSpec,
    count: u64,
    next: u64,
    at: Ns,
    base_at: Ns,
    base_pts: Ns,
    discont: bool,
    ended: Option<(EndKind, Ns)>,
    jitter: Prng,
}

impl Cursor {
    pub const fn position(&self) -> u64 {
        self.next
    }
}

fn cues_of(spec: &MediaSpec, stream: u16, range: CueRange) -> Result<&[Cue], ScheduleError> {
    let first = range.first as usize;
    first
        .checked_add(range.len as usize)
        .and_then(|end| spec.cues.get(first..end))
        .ok_or(ScheduleError::CueRange { stream })
}

fn buffer_count(spec: &MediaSpec, stream: u16, s: &StreamSpec) -> Result<u64, ScheduleError> {
    match s.kind {
        StreamKind::Video { fps, .. } if !fps.is_valid() => {
            Err(ScheduleError::BadFrameRate { stream })
        }
        StreamKind::Video { fps, .. } => Ok(frames_in(s.duration, fps)),
        StreamKind::Audio { packet: 0 } => Err(ScheduleError::ZeroPacket { stream }),
        StreamKind::Audio { packet } => Ok(s.duration / packet),
        StreamKind::Text { cues: range } => {
            let cues = cues_of(spec, stream, range)?;
            let mut floor = 0;
            for (i, cue) in cues.iter().enumerate() {
                if cue.end < cue.start || cue.start < floor {
                    return Err(ScheduleError::BadCue { stream, cue: range.first + i as u32 });
                }
                floor = cue.start;
            }
            Ok(cues.len() as u64)
        }
    }
}

pub fn cursor(
    spec: &MediaSpec,
    stream: u16,
    seed: u64,
    start: Ns,
) -> Result<Cursor, ScheduleError> {
    let s = *spec.streams.get(stream as usize).ok_or(ScheduleError::NoSuchStream { stream })?;
    let count = buffer_count(spec, stream, &s)?;
    let at = start.saturating_add(pad_delay(spec, stream));
    Ok(Cursor {
        stream,
        spec: s,
        count,
        next: 0,
        at,
        base_at: at,
        base_pts: 0,
        discont: true,
        ended: None,
        jitter: Prng::new(seed).substream_named("media.jitter").substream(stream as u64),
    })
}

fn buffer_at(c: &Cursor, cues: &[Cue], index: u64) -> (Ns, Ns, u32, u16) {
    match c.spec.kind {
        StreamKind::Video { fps, keyframe_interval } => {
            let pts = frame_pts(index, fps);
            let key = keyframe_interval <= 1 || index.is_multiple_of(keyframe_interval as u64);
            let flags = if key { flag::KEYFRAME } else { flag::DELTA };
            (pts, frame_pts(index + 1, fps).saturating_sub(pts), c.spec.size, flags)
        }
        StreamKind::Audio { packet } => (index * packet, packet, c.spec.size, flag::KEYFRAME),
        StreamKind::Text { .. } => {
            let cue = cues[index as usize];
            (cue.start, cue.end.saturating_sub(cue.start), cue.size, flag::KEYFRAME)
        }
    }
}

/// The cue table may have shrunk since the cursor was made, and a cursor never indexes past it.
fn live_count(c: &Cursor, cues: &[Cue]) -> u64 {
    match c.spec.kind {
        StreamKind::Text { .. } => c.count.min(cues.len() as u64),
        _ => c.count,
    }
}

fn media_end(c: &Cursor, cues: &[Cue]) -> Ns {
    match c.spec.kind {
        StreamKind::Video { fps, .. } => frame_pts(c.count, fps),
        StreamKind::Audio { packet } => c.count * packet,
        StreamKind::Text { .. } => cues.iter().map(|cue| cue.end).max().unwrap_or(0),
    }
}

fn stream_cues<'a>(spec: &'a MediaSpec, c: &Cursor) -> &'a [Cue] {
    match c.spec.kind {
        // Validated when the cursor was made. A spec edited since degrades to no cues, which ends
        // the stream.
        StreamKind::Text { cues } => cues_of(spec, c.stream, cues).unwrap_or(&[]),
        _ => &[],
    }
}

struct Hit {
    end: Option<EndKind>,
    hold: Ns,
    marks: u16,
}

fn faults_at(spec: &MediaSpec, stream: u16, index: u64) -> Hit {
    let mut hit = Hit { end: None, hold: 0, marks: 0 };
    for entry in spec.faults.iter().filter(|entry| entry.stream == stream) {
        match entry.fault {
            Fault::StallAt { buffer, hold } if buffer == index => {
                hit.hold = hit.hold.saturating_add(hold);
                hit.marks |= mark::STALLED;
            }
            // An error wins over an early end declared at the same buffer.
            Fault::ErrorAt { buffer } if buffer == index => hit.end = Some(EndKind::Error),
            Fault::EndEarlyAt { buffer } if buffer == index && hit.end.is_none() => {
                hit.end = Some(EndKind::EarlyEos);
            }
            Fault::BufferingDip { buffer, .. } if buffer == index => hit.marks |= mark::DIP,
            _ => {}
        }
    }
    hit
}

fn end(c: &mut Cursor, kind: EndKind, at: Ns) -> Step {
    c.ended = Some((kind, at));
    Step::End { kind, at }
}

pub fn step(spec: &MediaSpec, c: &mut Cursor) -> Step {
    if let Some((kind, at)) = c.ended {
        return Step::End { kind, at };
    }
    let cues = stream_cues(spec, c);
    let index = c.next;
    if index >= live_count(c, cues) {
        let at = match c.spec.pacing {
            Pacing::RealTime => c.at.max(release_of(c, media_end(c, cues))),
            _ => c.at,
        };
        return end(c, EndKind::Eos, at);
    }
    let hit = faults_at(spec, c.stream, index);
    if let Some(kind) = hit.end {
        return end(c, kind, c.at);
    }
    let (pts, duration, size, mut flags) = buffer_at(c, cues, index);
    if c.discont {
        flags |= flag::DISCONT;
    }
    let ready = match c.spec.pacing {
        Pacing::AsFastAsPossible => c.at,
        Pacing::RealTime => c.at.max(release_of(c, pts)),
        Pacing::Jitter { base, bound } => {
            // Keyed by the buffer, so a seek or removed action leaves every other buffer's jitter
            // alone.
            let draw = c.jitter.substream(index).below(bound.saturating_add(1));
            c.at.saturating_add(base).saturating_add(draw)
        }
    };
    let release = ready.saturating_add(hit.hold);
    c.at = release;
    c.next = index + 1;
    c.discont = false;
    Step::Slot(Slot { seq: index, pts, dts: pts, duration, release, size, flags, marks: hit.marks })
}

fn release_of(c: &Cursor, pts: Ns) -> Ns {
    c.base_at.saturating_add(pts.saturating_sub(c.base_pts))
}

/// Video snaps back to the keyframe at or before `to`. Past the end yields the buffer count.
fn seek_index(c: &Cursor, cues: &[Cue], to: Ns) -> u64 {
    match c.spec.kind {
        StreamKind::Video { fps, keyframe_interval } => {
            let frame = frame_at(to, fps);
            if frame >= c.count {
                return c.count;
            }
            frame - frame % keyframe_interval.max(1) as u64
        }
        StreamKind::Audio { packet } => (to / packet).min(c.count),
        // Cues sort by start and may overlap, so ends are not ordered. A scan it is.
        StreamKind::Text { .. } => {
            cues.iter().position(|cue| cue.end > to).unwrap_or(cues.len()) as u64
        }
    }
}

/// Returns the pts the stream resumes from, or `NONE` when the target is past the end.
pub fn seek(spec: &MediaSpec, c: &mut Cursor, to: Ns, now: Ns) -> Ns {
    let cues = stream_cues(spec, c);
    let index = seek_index(c, cues, to);
    let pts = if index < live_count(c, cues) { buffer_at(c, cues, index).0 } else { NONE };
    c.next = index;
    c.at = now;
    c.base_at = now;
    c.base_pts = if pts == NONE { media_end(c, cues) } else { pts };
    c.discont = true;
    c.ended = None;
    pts
}

pub fn seek_to(spec: &MediaSpec, c: &mut Cursor, seq: u64, now: Ns) -> Ns {
    let cues = stream_cues(spec, c);
    let index = seq.min(live_count(c, cues));
    let pts = if index < live_count(c, cues) { buffer_at(c, cues, index).0 } else { NONE };
    c.next = index;
    c.at = now;
    c.base_at = now;
    c.base_pts = if pts == NONE { media_end(c, cues) } else { pts };
    c.discont = true;
    c.ended = None;
    pts
}

pub fn shift(c: &mut Cursor, by: Ns) {
    c.at = c.at.saturating_add(by);
    c.base_at = c.base_at.saturating_add(by);
    if let Some((kind, at)) = c.ended {
        c.ended = Some((kind, at.saturating_add(by)));
    }
}

pub struct Schedule {
    slots: Vec<Slot>,
}

impl Schedule {
    // One allocation per worker, refilled in place by `fill`.
    #[allow(clippy::disallowed_methods)]
    pub fn with_capacity(slots: usize) -> Self {
        Self { slots: Vec::with_capacity(slots) }
    }

    pub fn reset(&mut self) {
        self.slots.clear();
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn capacity(&self) -> usize {
        self.slots.capacity()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub buffers: u32,
    pub end: EndKind,
    pub end_at: Ns,
}

pub fn fill(
    spec: &MediaSpec,
    stream: u16,
    seed: u64,
    out: &mut Schedule,
) -> Result<Summary, ScheduleError> {
    out.reset();
    let mut c = cursor(spec, stream, seed, 0)?;
    let capacity = out.slots.capacity();
    // One more step than there are slots, so a table that fits exactly still sees the end.
    for _ in 0..=capacity {
        match step(spec, &mut c) {
            Step::Slot(_) if out.slots.len() == capacity => break,
            Step::Slot(slot) => out.slots.push(slot),
            Step::End { kind, at } => {
                return Ok(Summary { buffers: out.slots.len() as u32, end: kind, end_at: at });
            }
        }
    }
    Err(ScheduleError::TableFull { capacity: capacity as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{MS, SEC};

    const FPS25: Rational = Rational::new(25, 1);

    fn video(pacing: Pacing) -> StreamSpec {
        StreamSpec {
            kind: StreamKind::Video { fps: FPS25, keyframe_interval: 25 },
            duration: 10 * SEC,
            size: 1024,
            pacing,
        }
    }

    fn audio(pacing: Pacing) -> StreamSpec {
        StreamSpec {
            kind: StreamKind::Audio { packet: 20 * MS },
            duration: 10 * SEC,
            size: 256,
            pacing,
        }
    }

    fn spec_of(streams: &[StreamSpec]) -> MediaSpec {
        let mut spec = MediaSpec::with_capacity(16, 16);
        for stream in streams {
            spec.add_stream(*stream).unwrap();
        }
        spec
    }

    fn filled(spec: &MediaSpec, stream: u16, seed: u64) -> (Schedule, Summary) {
        let mut out = Schedule::with_capacity(1024);
        let summary = fill(spec, stream, seed, &mut out).unwrap();
        (out, summary)
    }

    #[test]
    fn video_schedule_has_frames_keyframes_and_a_discont_opening() {
        let spec = spec_of(&[video(Pacing::AsFastAsPossible)]);
        let (out, summary) = filled(&spec, 0, 1);
        let slots = out.slots();
        assert_eq!(slots.len(), 250);
        assert_eq!(summary, Summary { buffers: 250, end: EndKind::Eos, end_at: 0 });
        for (i, slot) in slots.iter().enumerate() {
            assert_eq!(slot.seq, i as u64);
            assert_eq!(slot.pts, i as u64 * 40 * MS);
            assert_eq!(slot.dts, slot.pts);
            assert_eq!(slot.duration, 40 * MS);
            assert_eq!(slot.size, 1024);
            assert_eq!(slot.release, 0);
            let key = i % 25 == 0;
            assert_eq!(slot.flags & flag::KEYFRAME != 0, key, "frame {i}");
            assert_eq!(slot.flags & flag::DELTA != 0, !key, "frame {i}");
            assert_eq!(slot.flags & flag::DISCONT != 0, i == 0, "frame {i}");
        }
    }

    #[test]
    fn real_time_releases_each_buffer_at_its_pts() {
        let spec = spec_of(&[video(Pacing::RealTime), audio(Pacing::RealTime)]);
        let (out, summary) = filled(&spec, 1, 1);
        assert_eq!(out.slots().len(), 500);
        assert!(out.slots().iter().all(|slot| slot.release == slot.pts));
        assert!(out.slots().iter().all(|slot| slot.flags & flag::KEYFRAME != 0));
        assert_eq!(summary.end_at, 10 * SEC);
    }

    #[test]
    fn text_serves_one_buffer_per_cue() {
        let mut spec = MediaSpec::with_capacity(8, 8);
        let cues = [
            Cue { start: SEC, end: 2 * SEC, size: 40 },
            Cue { start: 3 * SEC, end: 3 * SEC + 500 * MS, size: 12 },
        ];
        let id = spec.add_text(&cues, Pacing::RealTime).unwrap();
        let (out, summary) = filled(&spec, id, 0);
        let slots = out.slots();
        assert_eq!(slots.len(), 2);
        assert_eq!((slots[0].pts, slots[0].duration, slots[0].size), (SEC, SEC, 40));
        assert_eq!((slots[1].pts, slots[1].duration, slots[1].size), (3 * SEC, 500 * MS, 12));
        assert_eq!(slots[1].release, 3 * SEC);
        assert_eq!(summary.end_at, 3 * SEC + 500 * MS);
    }

    #[test]
    fn same_seed_same_schedule() {
        let spec = spec_of(&[video(Pacing::Jitter { base: 5 * MS, bound: 30 * MS })]);
        let (a, sa) = filled(&spec, 0, 0xC0FFEE);
        let (b, sb) = filled(&spec, 0, 0xC0FFEE);
        assert_eq!(a.slots(), b.slots());
        assert_eq!(sa, sb);
    }

    #[test]
    fn a_different_seed_moves_the_jitter_and_nothing_else() {
        let spec = spec_of(&[video(Pacing::Jitter { base: 5 * MS, bound: 30 * MS })]);
        let (a, _) = filled(&spec, 0, 1);
        let (b, _) = filled(&spec, 0, 2);
        let moved = a.slots().iter().zip(b.slots()).filter(|(x, y)| x.release != y.release).count();
        assert!(moved > 200, "the seed reached only {moved} of 250 releases");
        for (x, y) in a.slots().iter().zip(b.slots()) {
            assert_eq!((x.seq, x.pts, x.duration, x.flags), (y.seq, y.pts, y.duration, y.flags));
        }
        let plain = spec_of(&[video(Pacing::RealTime)]);
        assert_eq!(filled(&plain, 0, 1).0.slots(), filled(&plain, 0, 2).0.slots());
    }

    #[test]
    fn jitter_is_bounded_and_releases_never_go_back() {
        let (base, bound) = (5 * MS, 30 * MS);
        let spec = spec_of(&[audio(Pacing::Jitter { base, bound })]);
        let (out, _) = filled(&spec, 0, 99);
        let mut last = 0;
        let mut widest = 0;
        for slot in out.slots() {
            let gap = slot.release - last;
            assert!((base..=base + bound).contains(&gap), "gap {gap}");
            widest = widest.max(gap);
            last = slot.release;
        }
        assert!(widest > base + bound / 2, "500 draws never used the upper half");
    }

    #[test]
    fn streams_of_one_seed_draw_independent_jitter() {
        let jitter = Pacing::Jitter { base: 0, bound: 30 * MS };
        let two = spec_of(&[audio(jitter), audio(jitter)]);
        let (first, _) = filled(&two, 0, 7);
        let (second, _) = filled(&two, 1, 7);
        assert_ne!(first.slots(), second.slots());
        let one = spec_of(&[audio(jitter)]);
        assert_eq!(filled(&one, 0, 7).0.slots(), first.slots());
    }

    #[test]
    fn a_stall_delays_its_buffer_and_real_time_catches_up() {
        let mut spec = spec_of(&[video(Pacing::RealTime), video(Pacing::AsFastAsPossible)]);
        spec.add_fault(0, Fault::StallAt { buffer: 10, hold: 100 * MS }).unwrap();
        spec.add_fault(1, Fault::StallAt { buffer: 10, hold: 100 * MS }).unwrap();
        let (live, _) = filled(&spec, 0, 0);
        let live = live.slots();
        assert_eq!(live[9].release, 360 * MS);
        assert_eq!(live[10].release, 500 * MS);
        assert_eq!(live[10].marks, mark::STALLED);
        // Frames 11 and 12 were due during the stall and leave right after it.
        assert_eq!(live[11].release, 500 * MS);
        assert_eq!(live[12].release, 500 * MS);
        assert_eq!(live[13].release, 520 * MS);
        assert_eq!(live[11].marks, 0);

        let (fast, _) = filled(&spec, 1, 0);
        assert_eq!(fast.slots()[9].release, 0);
        assert!(fast.slots()[10..].iter().all(|slot| slot.release == 100 * MS));
    }

    #[test]
    fn error_and_early_end_cut_the_schedule_at_their_buffer() {
        let mut spec = spec_of(&[audio(Pacing::RealTime), audio(Pacing::RealTime)]);
        spec.add_fault(0, Fault::ErrorAt { buffer: 7 }).unwrap();
        spec.add_fault(1, Fault::EndEarlyAt { buffer: 3 }).unwrap();
        let (errored, summary) = filled(&spec, 0, 0);
        assert_eq!(errored.slots().len(), 7);
        assert_eq!(summary, Summary { buffers: 7, end: EndKind::Error, end_at: 120 * MS });
        let (short, summary) = filled(&spec, 1, 0);
        assert_eq!(short.slots().len(), 3);
        assert_eq!(summary.end, EndKind::EarlyEos);
    }

    #[test]
    fn a_late_pad_delays_the_whole_stream() {
        let mut spec = spec_of(&[audio(Pacing::RealTime)]);
        spec.add_fault(0, Fault::LatePad { delay: 250 * MS }).unwrap();
        assert_eq!(pad_delay(&spec, 0), 250 * MS);
        let (out, _) = filled(&spec, 0, 0);
        assert!(out.slots().iter().all(|slot| slot.release == slot.pts + 250 * MS));
    }

    #[test]
    fn a_buffering_dip_marks_its_buffer_and_keeps_the_schedule() {
        let mut spec = spec_of(&[audio(Pacing::RealTime)]);
        let (plain, _) = filled(&spec, 0, 0);
        let recovery = Recovery::OnSyncPoint(4);
        spec.add_fault(0, Fault::BufferingDip { buffer: 5, recovery }).unwrap();
        let (dipped, _) = filled(&spec, 0, 0);
        assert_eq!(dipped.slots()[5].marks, mark::DIP);
        assert_eq!(dip_at(&spec, 0, 5), Some(recovery));
        assert_eq!(dip_at(&spec, 0, 6), None);
        for (a, b) in plain.slots().iter().zip(dipped.slots()) {
            assert_eq!(Slot { marks: 0, ..*b }, *a);
        }
    }

    #[test]
    fn stepping_past_the_end_repeats_the_end() {
        let spec = spec_of(&[StreamSpec { duration: 60 * MS, ..audio(Pacing::RealTime) }]);
        let mut c = cursor(&spec, 0, 0, 0).unwrap();
        for _ in 0..3 {
            assert!(matches!(step(&spec, &mut c), Step::Slot(_)));
        }
        let ended = step(&spec, &mut c);
        assert_eq!(ended, Step::End { kind: EndKind::Eos, at: 60 * MS });
        assert_eq!(step(&spec, &mut c), ended);
    }

    #[test]
    fn a_seek_resumes_from_a_keyframe_and_is_discontinuous() {
        let spec = spec_of(&[video(Pacing::RealTime), audio(Pacing::RealTime)]);
        let mut v = cursor(&spec, 0, 0, 0).unwrap();
        let mut a = cursor(&spec, 1, 0, 0).unwrap();
        for _ in 0..5 {
            step(&spec, &mut v);
            step(&spec, &mut a);
        }
        // 4.5 s is frame 112. Its keyframe is frame 100 at 4 s.
        assert_eq!(seek(&spec, &mut v, 4_500 * MS, 200 * MS), 4 * SEC);
        assert_eq!(seek(&spec, &mut a, 4_500 * MS, 200 * MS), 4_500 * MS);
        let Step::Slot(first) = step(&spec, &mut v) else { panic!("video ended") };
        assert_eq!((first.seq, first.pts, first.release), (100, 4 * SEC, 200 * MS));
        assert_eq!(first.flags, flag::KEYFRAME | flag::DISCONT);
        let Step::Slot(next) = step(&spec, &mut v) else { panic!("video ended") };
        assert_eq!((next.seq, next.release, next.flags), (101, 240 * MS, flag::DELTA));
        let Step::Slot(packet) = step(&spec, &mut a) else { panic!("audio ended") };
        assert_eq!((packet.seq, packet.pts, packet.release), (225, 4_500 * MS, 200 * MS));
        assert!(packet.flags & flag::DISCONT != 0);
    }

    #[test]
    fn a_seek_revives_an_ended_stream_and_one_past_the_end_ends_it() {
        let spec = spec_of(&[audio(Pacing::AsFastAsPossible)]);
        let mut c = cursor(&spec, 0, 0, 0).unwrap();
        assert_eq!(seek(&spec, &mut c, 11 * SEC, 5), NONE);
        assert_eq!(step(&spec, &mut c), Step::End { kind: EndKind::Eos, at: 5 });
        assert_eq!(seek(&spec, &mut c, 0, 9), 0);
        let Step::Slot(slot) = step(&spec, &mut c) else { panic!("ended") };
        assert_eq!((slot.seq, slot.release), (0, 9));
    }

    #[test]
    fn a_text_seek_lands_on_the_cue_still_showing() {
        let mut spec = MediaSpec::with_capacity(8, 8);
        let cues = [
            Cue { start: SEC, end: 2 * SEC, size: 1 },
            Cue { start: 3 * SEC, end: 4 * SEC, size: 1 },
        ];
        let id = spec.add_text(&cues, Pacing::AsFastAsPossible).unwrap();
        let mut c = cursor(&spec, id, 0, 0).unwrap();
        assert_eq!(seek(&spec, &mut c, 1_500 * MS, 0), SEC);
        assert_eq!(seek(&spec, &mut c, 2 * SEC, 0), 3 * SEC);
        assert_eq!(seek(&spec, &mut c, 4 * SEC, 0), NONE);
    }

    #[test]
    fn shift_moves_pending_releases() {
        let spec = spec_of(&[audio(Pacing::RealTime)]);
        let mut c = cursor(&spec, 0, 0, 0).unwrap();
        step(&spec, &mut c);
        shift(&mut c, 300 * MS);
        let Step::Slot(slot) = step(&spec, &mut c) else { panic!("ended") };
        assert_eq!((slot.pts, slot.release), (20 * MS, 320 * MS));
    }

    #[test]
    fn bad_specs_are_typed_errors() {
        let mut spec = MediaSpec::with_capacity(3, 1);
        let bad_fps = StreamKind::Video { fps: Rational::new(0, 1), keyframe_interval: 1 };
        spec.add_stream(StreamSpec { kind: bad_fps, ..video(Pacing::RealTime) }).unwrap();
        spec.add_stream(StreamSpec {
            kind: StreamKind::Audio { packet: 0 },
            ..audio(Pacing::RealTime)
        })
        .unwrap();
        let unordered = [Cue { start: 5, end: 9, size: 0 }, Cue { start: 2, end: 3, size: 0 }];
        spec.add_text(&unordered, Pacing::RealTime).unwrap();
        let dangling = StreamKind::Text { cues: CueRange { first: 1, len: 9 } };
        spec.add_stream(StreamSpec { kind: dangling, ..audio(Pacing::RealTime) }).unwrap();

        let mut out = Schedule::with_capacity(8);
        assert_eq!(fill(&spec, 0, 0, &mut out), Err(ScheduleError::BadFrameRate { stream: 0 }));
        assert_eq!(fill(&spec, 1, 0, &mut out), Err(ScheduleError::ZeroPacket { stream: 1 }));
        assert_eq!(fill(&spec, 2, 0, &mut out), Err(ScheduleError::BadCue { stream: 2, cue: 1 }));
        assert_eq!(fill(&spec, 3, 0, &mut out), Err(ScheduleError::CueRange { stream: 3 }));
        assert_eq!(fill(&spec, 9, 0, &mut out), Err(ScheduleError::NoSuchStream { stream: 9 }));

        assert_eq!(
            spec.add_text(&unordered, Pacing::RealTime),
            Err(SpecError::Full { capacity: 3 })
        );
        assert_eq!(
            spec.add_fault(9, Fault::ErrorAt { buffer: 0 }),
            Err(SpecError::NoSuchStream { stream: 9 })
        );
        spec.add_fault(0, Fault::ErrorAt { buffer: 0 }).unwrap();
        assert_eq!(
            spec.add_fault(0, Fault::ErrorAt { buffer: 1 }),
            Err(SpecError::Full { capacity: 1 })
        );
    }

    #[test]
    fn a_table_too_small_is_reported_and_an_exact_fit_is_not() {
        let spec = spec_of(&[StreamSpec { duration: 100 * MS, ..audio(Pacing::RealTime) }]);
        let mut exact = Schedule::with_capacity(5);
        assert_eq!(fill(&spec, 0, 0, &mut exact).map(|s| s.buffers), Ok(5));
        let mut small = Schedule::with_capacity(4);
        assert_eq!(fill(&spec, 0, 0, &mut small), Err(ScheduleError::TableFull { capacity: 4 }));
    }

    #[test]
    fn a_spec_refills_in_place() {
        let mut spec = MediaSpec::with_capacity(4, 4);
        for _ in 0..MAX_STREAMS {
            spec.add_stream(audio(Pacing::RealTime)).unwrap();
        }
        assert_eq!(spec.add_stream(audio(Pacing::RealTime)), Err(SpecError::TooManyStreams));
        let base = spec.streams.as_ptr();
        spec.reset();
        assert_eq!(spec.add_stream(video(Pacing::RealTime)), Ok(0));
        assert_eq!(spec.streams.as_ptr(), base);
    }
}
