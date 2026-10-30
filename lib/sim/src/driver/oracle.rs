//! The sweep of a stepped run. Each sink has a cursor over its declared stream that a flush moves
//! to the seek target. Matching it covers loss, duplication, replay, wrong start and early end.

use super::gop::{GopDecl, MAX_REORDER, Pictures};
use super::map::{Map, Rate};
use super::model::{App, Jump};
use super::offers::Plan;
use super::retime::{self, Retiming};
use super::scenario::{MAX_SINKS, MAX_WINDOW, Scenario};
use super::verdict::{Detail, Finding, Kind, NO_ROW, NO_SINK};
use crate::media::{self, Cursor, EndKind, MediaSpec, Slot, Step, StreamKind};
use crate::profile::SinkProfile;
use crate::record::{NO_STREAM, Row, RowKind, class, flag};
use crate::sweep::{self, Expect, Rule, SinkExpect};
use crate::time::{NONE, Ns};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Finished,
    Stopped,
    Failed,
    Refused,
    Rejected,
    BadSeek,
}

const WIN: usize = MAX_WINDOW as usize + 1;

const FILLER: Slot =
    Slot { seq: 0, pts: 0, dts: 0, duration: 0, release: 0, size: 0, flags: 0, marks: 0 };

#[derive(Clone, Copy)]
struct Lane {
    cursor: Cursor,
    win: [Slot; WIN],
    len: usize,
    seen: u8,
    end: Option<EndKind>,
    prev: Option<Slot>,
    at_match: Option<Slot>,
}

impl Lane {
    fn new(cursor: Cursor) -> Lane {
        let (win, none) = ([FILLER; WIN], None);
        Lane { cursor, win, len: 0, seen: 0, end: None, prev: none, at_match: none }
    }

    fn seek(&mut self, spec: &MediaSpec, to: u64) {
        media::seek(spec, &mut self.cursor, to, 0);
        (self.len, self.seen, self.end, self.prev) = (0, 0, None, None);
    }

    fn seek_to(&mut self, spec: &MediaSpec, seq: u64) {
        media::seek_to(spec, &mut self.cursor, seq, 0);
        (self.len, self.seen, self.end, self.prev) = (0, 0, None, None);
    }

    fn fill(&mut self, spec: &MediaSpec, want: usize) {
        for _ in 0..WIN {
            if self.len >= want.min(WIN) || self.end.is_some() {
                break;
            }
            match media::step(spec, &mut self.cursor) {
                Step::Slot(slot) => {
                    self.win[self.len] = slot;
                    self.len += 1;
                }
                Step::End { kind, .. } => self.end = Some(kind),
            }
        }
    }

    fn pop_front(&mut self) {
        self.prev = self.win[..self.len].first().copied().or(self.prev);
        self.win.copy_within(1..self.len.max(1), 0);
        self.len = self.len.saturating_sub(1);
        self.seen >>= 1;
    }

    fn take(
        &mut self,
        spec: &MediaSpec,
        row: &Row,
        window: usize,
        lossy: bool,
    ) -> Result<Slot, (Kind, u64)> {
        // Each round that does not return drops one declared buffer. The cap is for a stream gone
        // wrong.
        for _ in 0..1 << 24 {
            self.fill(spec, window + 1);
            let unseen = |n: usize| self.seen & (1 << n) == 0;
            let found = (0..self.len).find(|n| self.win[*n].seq == row.seq && unseen(*n));
            if let Some(n) = found {
                let slot = self.win[n];
                self.at_match = self.prev;
                self.seen |= 1 << n;
                while self.len > 0 && self.seen & 1 != 0 {
                    self.pop_front();
                }
                return Ok(slot);
            }
            let Some(front) = self.win[..self.len].first() else {
                return Err((Kind::Beyond, NONE));
            };
            if row.seq < front.seq || (0..self.len).any(|n| self.win[n].seq == row.seq) {
                return Err((Kind::Repeated, front.seq));
            }
            if !lossy {
                return Err((Kind::Missing, front.seq));
            }
            self.pop_front();
        }
        Err((Kind::Beyond, NONE))
    }

    /// [`Self::take`] behind a decoder, with no reordering. A picture before `high` may be missing.
    fn take_decoded(
        &mut self,
        spec: &MediaSpec,
        row: &Row,
        (spans, high): (bool, u64),
    ) -> Result<Slot, (Kind, u64)> {
        for _ in 0..1 << 24 {
            self.fill(spec, 1);
            let Some(front) = self.win[..self.len].first().copied() else {
                return Err((Kind::Beyond, NONE));
            };
            if front.seq == row.seq {
                self.at_match = self.prev;
                self.pop_front();
                return Ok(front);
            }
            if row.seq < front.seq {
                return Err((Kind::Repeated, front.seq));
            }
            if !before(&front, spans, high) {
                return Err((Kind::Missing, front.seq));
            }
            self.pop_front();
        }
        Err((Kind::Beyond, NONE))
    }

    fn owed(&mut self, spec: &MediaSpec, (spans, high): (bool, u64)) -> Option<u64> {
        for _ in 0..1 << 24 {
            self.fill(spec, 1);
            let front = self.win[..self.len].first()?;
            if !before(front, spans, high) {
                return Some(front.seq);
            }
            self.pop_front();
        }
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Pacing {
    pub profile: SinkProfile,
    pub latency: Ns,
    pub offset: i64,
    pub waited: bool,
}

/// Hold a buffer a sink took or dropped at clock reading `now` to the timeline.
pub fn on_the_map(map: &Map, sink: &Pacing, row: &Row, now: Ns) -> Result<(), (Kind, u64, u64)> {
    let running = map.running_now(now);
    if row.kind == RowKind::Buffer && row.aux != running {
        return Err((Kind::OffTimeline, running, row.aux));
    }
    if !sink.profile.paced || row.kind == RowKind::Before {
        return Ok(());
    }
    let at = map.due_at(row.pts, sink.latency, sink.offset);
    let deadline = map.deadline(at);
    if now < deadline {
        return Err((Kind::OffTimeline, deadline, now));
    }
    let late = running.saturating_sub(at);
    let bound = sink.profile.lateness_bound;
    let past = bound != NONE && row.pts != NONE && late > bound;
    match row.kind {
        RowKind::Drop if !past || row.aux != late => return Err((Kind::Paced, late, row.aux)),
        RowKind::Buffer if past => return Err((Kind::Paced, bound, late)),
        _ => {}
    }
    if sink.waited && now != deadline && map.changed_at != now {
        return Err((Kind::OffTimeline, deadline, now));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Outs {
    at: usize,
    k: u32,
}

impl Outs {
    fn front(&mut self, inner: &[Row], seek_gen: u32) -> Option<Slot> {
        for _ in 0..=inner.len() {
            let row = inner.get(self.at)?;
            if row.seek_gen > seek_gen {
                return None;
            }
            let (ours, n) =
                (row.kind == RowKind::Retimed && row.seek_gen == seek_gen, row.format as u32);
            if ours && self.k < n {
                let (pts, duration) = retime::stamp(row.pts, row.duration, self.k, n);
                let (seq, size, flags) = (row.seq, row.size, row.flags);
                return Some(Slot { seq, pts, dts: pts, duration, size, flags, ..FILLER });
            }
            (self.at, self.k) = (self.at + 1, 0);
        }
        None
    }

    fn take(&mut self, inner: &[Row], row: &Row, lossy: bool) -> Result<Slot, (Kind, u64)> {
        for _ in 0..=inner.len().saturating_mul(retime::MAX_OUT as usize) {
            let Some(slot) = self.front(inner, row.seek_gen) else {
                return Err((Kind::Unretimed, NONE));
            };
            self.k += 1;
            if (slot.seq, slot.pts) == (row.seq, row.pts) {
                return Ok(slot);
            }
            if !lossy {
                return Err((Kind::Missing, slot.seq));
            }
        }
        Err((Kind::Unretimed, NONE))
    }
}

#[derive(Clone, Copy)]
struct SinkState {
    lane: Option<Lane>,
    outs: Outs,
    told: Told,
    shifts: usize,
    started: bool,
    stopped: bool,
    paused: bool,
    seek_gen: u32,
    format: u64,
    first: bool,
    ended: bool,
    point: u64,
    high: u64,
    first_at: u64,
    jumps: usize,
    shown: u64,
    shown_at_skip: u64,
    ruled: bool,
}

const FRESH: SinkState = SinkState {
    lane: None,
    outs: Outs { at: 0, k: 0 },
    told: Told { at: 0, high: 0 },
    shifts: 0,
    started: false,
    stopped: false,
    paused: false,
    seek_gen: 0,
    format: 0,
    first: true,
    ended: false,
    point: 0,
    high: 0,
    first_at: NONE,
    jumps: 0,
    shown: NONE,
    shown_at_skip: NONE,
    ruled: true,
};

/// A picture is judged by its pts, a span by where it ends.
fn before(slot: &Slot, spans: bool, point: u64) -> bool {
    let end = slot.pts.saturating_add(if spans { slot.duration } else { 0 });
    slot.pts < point && end <= point
}

/// The decoder's group may learn a point before the sink's does, and what it dropped stays
/// dropped behind a skip back.
#[derive(Clone, Copy)]
struct Told {
    at: usize,
    high: u64,
}

impl Told {
    fn before_out(&mut self, inner: &[Row], (seq, seek_gen): (u64, u32), held: usize) -> u64 {
        let ours =
            |row: &Row| row.kind == RowKind::Decoded && (row.seq, row.seek_gen) == (seq, seek_gen);
        for _ in 0..inner.len() {
            let Some(row) = inner.get(self.at).filter(|row| row.seek_gen <= seek_gen && !ours(row))
            else {
                break;
            };
            match row.kind {
                RowKind::FlushStart => self.high = 0,
                RowKind::Skip => self.high = self.high.max(row.aux),
                _ => {}
            }
            self.at += 1;
        }
        if !inner.get(self.at).is_some_and(ours) {
            return self.high;
        }
        // A point it was told while it held the picture back counts too.
        let (mut high, mut left) = (self.high, held);
        for row in inner[self.at + 1..].iter().take_while(|row| row.seek_gen == seek_gen) {
            if left == 0 {
                break;
            }
            match row.kind {
                RowKind::Skip => high = high.max(row.aux),
                RowKind::Decoded => left -= 1,
                _ => {}
            }
        }
        high
    }
}

struct State<'a> {
    sc: &'a Scenario,
    spec: &'a MediaSpec,
    app: &'a App,
    plan: Plan,
    sinks: [SinkState; MAX_SINKS],
    inner: &'a [Row],
}

fn buffer_flags(flags: u16) -> u16 {
    flags & (flag::KEYFRAME | flag::DELTA | flag::GAP)
}

fn before_in_stream(sc: &Scenario, stream: usize, seq: u64) -> Option<Slot> {
    let grid = Pictures::of(&sc.source.streams[stream], &GopDecl::OFF);
    let at = seq.checked_sub(1)?;
    let (pts, next) = (grid.pts(at), grid.pts(seq));
    (pts != NONE).then(|| Slot { seq: at, pts, duration: next.saturating_sub(pts), ..FILLER })
}

fn check_content(row: &Row, slot: &Slot, ordered: bool) -> Result<(), (u64, u64)> {
    if row.pts != slot.pts {
        return Err((slot.pts, row.pts));
    }
    if ordered && row.dts != slot.dts {
        return Err((slot.dts, row.dts));
    }
    if row.duration != slot.duration {
        return Err((slot.duration, row.duration));
    }
    if row.size != slot.size {
        return Err((slot.size as u64, row.size as u64));
    }
    if buffer_flags(row.flags) != buffer_flags(slot.flags) {
        return Err((slot.flags as u64, row.flags as u64));
    }
    Ok(())
}

fn check_paced(sc: &Scenario, sink: usize, i: usize, row: &Row) -> Result<(), Finding> {
    let profile = &sc.sinks[sink].profile;
    let bad = |want, got| Err(Finding::at_row(Kind::Paced, i, row, want, got));
    match row.kind {
        RowKind::Drop if !profile.paced || profile.lateness_bound == NONE => bad(0, row.aux),
        RowKind::Drop if row.aux <= profile.lateness_bound => bad(profile.lateness_bound, row.aux),
        RowKind::Buffer if profile.paced && row.aux < row.pts => bad(row.pts, row.aux),
        RowKind::Buffer if profile.paced && row.aux - row.pts > profile.lateness_bound => {
            bad(profile.lateness_bound, row.aux - row.pts)
        }
        _ => Ok(()),
    }
}

impl State<'_> {
    fn arrival(&mut self, sink: usize, i: usize, row: &Row) -> Result<(), Finding> {
        let (sc, spec) = (self.sc, self.spec);
        let (window, lossy) = (sc.window(sink) as usize, sc.lossy(sink));
        let s = &mut self.sinks[sink];
        let at = |kind, want, got| Err(Finding::at_row(kind, i, row, want, got));
        if row.stream == NO_STREAM {
            return at(Kind::Content, sc.topology.stream_of(sink) as u64, NO_STREAM as u64);
        }
        if row.seek_gen != s.seek_gen {
            return at(Kind::Generation, s.seek_gen as u64, row.seek_gen as u64);
        }
        let declared = self.plan.format_of(row.stream, row.seq) as u64;
        if row.format as u64 != declared {
            return at(Kind::Format, declared, row.format as u64);
        }
        if s.format != declared {
            return at(Kind::Format, declared, s.format);
        }
        let Some(lane) = s.lane.as_mut() else {
            return at(Kind::Lifecycle, 0, 0);
        };
        let decoded = sc.decoded(sink);
        let held = sc.gop.reorder.min(MAX_REORDER) as usize;
        let high = match (lossy, decoded) {
            (true, _) => NONE,
            (false, true) => {
                s.high.max(s.told.before_out(self.inner, (row.seq, row.seek_gen), held))
            }
            (false, false) => s.high,
        };
        let taken = match (decoded, sc.retimed(sink)) {
            (true, _) => lane.take_decoded(spec, row, (sc.pictures().spans(), high)),
            (false, true) => s.outs.take(self.inner, row, lossy),
            (false, false) => lane.take(spec, row, window, lossy),
        };
        let slot = match taken {
            Ok(slot) => slot,
            Err((kind, want)) => return at(kind, want, row.seq),
        };
        if let Err((want, got)) = check_content(row, &slot, window == 0) {
            return at(Kind::Content, want, got);
        }
        // A decoder passes no flush flag on. Its first output may be any picture at or after the
        // point.
        let first = std::mem::take(&mut s.first);
        if !lossy && !decoded && row.has(flag::DISCONT) != first {
            return at(Kind::Content, first as u64, row.has(flag::DISCONT) as u64);
        }
        if sc.gop.on {
            let prev = lane.at_match;
            self.presented(sink, i, row, (&slot, prev))?;
        }
        // With a timeline a stamp is not a running time. The driver holds those rows to the map.
        if sc.time.on {
            return Ok(());
        }
        check_paced(sc, sink, i, row)
    }

    fn presented(
        &mut self,
        sink: usize,
        i: usize,
        row: &Row,
        (slot, prev): (&Slot, Option<Slot>),
    ) -> Result<(), Finding> {
        let kind = self.sc.source.streams[self.sc.topology.stream_of(sink) as usize].kind;
        let spans = !matches!(kind, StreamKind::Video { .. });
        let s = &mut self.sinks[sink];
        let at = |kind, want, got| Err(Finding::at_row(kind, i, row, want, got));
        let dropped = row.kind == RowKind::Before;
        if dropped != before(slot, spans, s.point) {
            return at(Kind::BeforePoint, s.point, row.pts);
        }
        if dropped {
            return Ok(());
        }
        let shown_at_skip = std::mem::replace(&mut s.shown_at_skip, NONE);
        s.shown = row.seq;
        if s.first_at == NONE {
            return Ok(());
        }
        // The one before it in the stream is before the point, or it is a span that held the point
        // when the skip came.
        let point = std::mem::replace(&mut s.first_at, NONE);
        let text = matches!(kind, StreamKind::Text { .. });
        let stream = self.sc.topology.stream_of(sink) as usize;
        match prev.or_else(|| before_in_stream(self.sc, stream, row.seq)) {
            Some(q) if text || self.sc.lossy(sink) || q.seq == shown_at_skip => Ok(()),
            Some(q) if !before(&q, spans, point) => at(Kind::PastPoint, q.seq, row.seq),
            _ => Ok(()),
        }
    }

    fn flush(&mut self, sink: usize, i: usize, row: &Row) -> Result<(), Finding> {
        let s = &mut self.sinks[sink];
        if row.seek_gen <= s.seek_gen || row.seek_gen > self.app.seek_gen {
            let want = s.seek_gen as u64 + 1;
            return Err(Finding::at_row(Kind::FlushOrder, i, row, want, row.seek_gen as u64));
        }
        let target = self.app.target(row.seek_gen);
        if row.aux != target {
            return Err(Finding::at_row(Kind::FlushTarget, i, row, target, row.aux));
        }
        s.seek_gen = row.seek_gen;
        (s.first, s.ended) = (true, false);
        let Some(lane) = s.lane.as_mut() else {
            return Ok(());
        };
        if self.sc.time.two_sources {
            let stream = self.sc.topology.stream_of(sink) as usize;
            lane.seek_to(self.spec, self.app.byte_of(row.seek_gen, stream));
            return Ok(());
        }
        if !self.sc.gop.on {
            lane.seek(self.spec, target);
            return Ok(());
        }
        let (byte, pictures) = (self.app.byte(row.seek_gen), self.sc.pictures());
        match self.sc.topology.stream_of(sink) {
            0 => lane.seek_to(self.spec, byte),
            _ => lane.seek(self.spec, pictures.pts(byte.min(pictures.count()))),
        }
        let jumps = self.app.jumps();
        let told = jumps.iter().position(|jump| jump.seek_gen == row.seek_gen);
        let planned = told.is_some_and(|n| jumps[n].planned);
        s.jumps = told.map_or(s.jumps, |n| n + 1);
        (s.point, s.high, s.ruled) = (target, target, planned);
        s.first_at = if planned { target } else { NONE };
        (s.shown, s.shown_at_skip) = (NONE, NONE);
        Ok(())
    }

    fn skip(&mut self, sink: usize, i: usize, row: &Row) -> Result<(), Finding> {
        let paced = self.sc.sinks[sink].profile.paced;
        let (s, jumps) = (&mut self.sinks[sink], self.app.jumps());
        let unsent = |at: usize| Err(Finding::at_row(Kind::SkipUnsent, i, row, at as u64, row.aux));
        let mut at = s.jumps;
        let jump = loop {
            match jumps.get(at) {
                Some(jump) if jump.seek_gen == 0 && jump.to_time == row.aux => break jump,
                Some(jump) if jump.seek_gen <= s.seek_gen => at += 1,
                _ => return unsent(at),
            }
        };
        s.jumps = at + 1;
        // Only a paced sink has presented nothing ahead of the play position, and only a point past
        // every earlier one still has everything from it to come.
        let owed = jump.planned && paced && s.ruled && row.aux >= s.high;
        (s.point, s.high) = (row.aux, s.high.max(row.aux));
        s.first_at = if owed { row.aux } else { NONE };
        s.shown_at_skip = s.shown;
        Ok(())
    }

    fn eos(&mut self, sink: usize, i: usize, row: &Row) -> Result<(), Finding> {
        let (lossy, decoded) = (self.sc.lossy(sink), self.sc.decoded(sink));
        let s = &mut self.sinks[sink];
        (s.ended, s.first_at) = (true, NONE);
        if self.sc.retimed(sink) {
            return match s.outs.front(self.inner, s.seek_gen) {
                Some(slot) if !lossy => {
                    Err(Finding::at_row(Kind::Incomplete, i, row, slot.seq, NONE))
                }
                _ => Ok(()),
            };
        }
        let Some(lane) = s.lane.as_mut() else {
            return Ok(());
        };
        let high = if decoded {
            s.high.max(s.told.before_out(self.inner, (NONE, s.seek_gen), 0))
        } else {
            0
        };
        let owed = lane.owed(self.spec, (self.sc.pictures().spans(), high));
        if lane.end == Some(EndKind::Error) {
            return Err(Finding::at_row(Kind::Beyond, i, row, NONE, 0));
        }
        match owed {
            Some(seq) if !lossy => Err(Finding::at_row(Kind::Incomplete, i, row, seq, NONE)),
            _ => Ok(()),
        }
    }

    fn offset(&mut self, sink: usize, i: usize, row: &Row) -> Result<(), Finding> {
        let (s, shifts) = (&mut self.sinks[sink], self.app.shifts());
        let next = (s.shifts..shifts.len()).find(|n| shifts[*n].sink as usize == sink);
        match next {
            Some(n) if shifts[n].by as u64 == row.aux => {
                s.shifts = n + 1;
                Ok(())
            }
            Some(n) => {
                Err(Finding::at_row(Kind::OffsetUnsent, i, row, shifts[n].by as u64, row.aux))
            }
            None => Err(Finding::at_row(Kind::OffsetUnsent, i, row, NONE, row.aux)),
        }
    }

    fn start(&mut self, sink: usize, i: usize, row: &Row) -> Result<(), Finding> {
        let stream = self.sc.topology.stream_of(sink);
        let s = &mut self.sinks[sink];
        if s.started {
            return Err(Finding::at_row(Kind::Lifecycle, i, row, 1, 2));
        }
        s.started = true;
        match media::cursor(self.spec, stream, self.sc.seed, 0) {
            Ok(cursor) => s.lane = Some(Lane::new(cursor)),
            Err(_) => return Err(Finding::at_row(Kind::BadScenario, i, row, stream as u64, 0)),
        }
        Ok(())
    }

    fn row(&mut self, i: usize, row: &Row) -> Result<(), Finding> {
        let sink = row.sink as usize;
        let sinks = self.sc.sink_count();
        // A row of a sink the scenario has not is misrouted or garbled, not to be passed over.
        let Some(s) = self.sinks.get_mut(sink).filter(|_| sink < sinks) else {
            return Err(Finding::at_row(Kind::Lifecycle, i, row, sinks as u64, sink as u64));
        };
        if row.kind == RowKind::Start {
            return self.start(sink, i, row);
        }
        if !s.started || s.stopped {
            return Err(Finding::at_row(Kind::Lifecycle, i, row, 1, s.stopped as u64));
        }
        let turned = |want: bool| Err(Finding::at_row(Kind::Transport, i, row, want as u64, 0));
        match row.kind {
            RowKind::Stop => s.stopped = true,
            RowKind::Paused if s.paused => return turned(false),
            RowKind::Resumed if !s.paused => return turned(true),
            RowKind::Paused => s.paused = true,
            RowKind::Resumed => s.paused = false,
            RowKind::Format if !self.plan.admits(sink, row.aux.min(u8::MAX as u64) as u8) => {
                let told = self.plan.told(sink) as u64;
                return Err(Finding::at_row(Kind::Unoffered, i, row, told, row.aux));
            }
            RowKind::Format => s.format = row.aux,
            RowKind::FlushStart => return self.flush(sink, i, row),
            RowKind::Skip => return self.skip(sink, i, row),
            RowKind::Offset => return self.offset(sink, i, row),
            RowKind::Eos => return self.eos(sink, i, row),
            kind if kind.is(class::ARRIVAL) => return self.arrival(sink, i, row),
            _ => {}
        }
        Ok(())
    }

    fn close(&self, end: End, rows: usize) -> Result<(), Finding> {
        for (sink, s) in self.sinks.iter().enumerate().take(self.sc.sink_count()) {
            let about = |kind, want, got| Finding {
                kind,
                sink: sink as u16,
                row: rows as u32,
                detail: Detail::Numbers { want, got },
            };
            if !s.started || !s.stopped {
                return Err(about(Kind::Lifecycle, 1, s.stopped as u64));
            }
            let newest = self.app.seek_gen;
            if end == End::Finished && (s.seek_gen != newest || !s.ended) {
                return Err(about(Kind::NotEnded, newest as u64, s.seek_gen as u64));
            }
            // An untold seek would have kept the run going. A skip holds no finish, so it may go
            // untold.
            let untold = self.app.jumps().get(s.jumps..).unwrap_or(&[]);
            if end == End::Finished && self.sc.gop.on && untold.iter().any(|j| j.seek_gen != 0) {
                let jumps = self.app.jumps().len() as u64;
                return Err(about(Kind::SkipUnsent, jumps, s.jumps as u64));
            }
        }
        Ok(())
    }
}

fn generic_rules(sc: &Scenario) -> [bool; Rule::COUNT] {
    let sinks = sc.sink_count();
    let lossy = (0..sinks).any(|s| sc.lossy(s));
    let reorders = (0..sinks).any(|s| sc.window(s) > 0);
    let mut on = [true; Rule::COUNT];
    on[Rule::SegmentBeforeBuffer as usize] = !lossy;
    // Behind a decoder the picture a flag rode on, and any picture before a point,
    // may never arrive.
    on[Rule::DiscontAfterFlush as usize] = !lossy && !sc.gop.on;
    // A retimer puts a buffer out several times, or not at all, as the rate says.
    let retimes = sc.time.retimer.on;
    on[Rule::NoDuplicateSeq as usize] = !lossy && !reorders && !retimes;
    on[Rule::NoUndeclaredGap as usize] = !lossy && !reorders && !sc.gop.on && !retimes;
    on
}

fn about(kind: Kind, row: &Row, want: u64, got: u64) -> Result<(), Finding> {
    let detail = Detail::Row { row: *row, want, got };
    Err(Finding { kind, sink: NO_SINK, row: NO_ROW, detail })
}

struct Decoding<'a> {
    sc: &'a Scenario,
    app: &'a App,
    pictures: Pictures,
    seek_gen: u32,
    point: u64,
    last: Option<u64>,
    told: usize,
    /// Pictures from the first unsent one up to the target's keyframe should not be decoded for a
    /// skip. `NONE` holds the decoder to nothing.
    unread: u64,
    group: u64,
    from: u64,
}

impl Decoding<'_> {
    fn flush(&mut self, row: &Row) -> Result<(), Finding> {
        if row.seek_gen <= self.seek_gen || row.seek_gen > self.app.seek_gen {
            let want = self.seek_gen as u64 + 1;
            return about(Kind::FlushOrder, row, want, row.seek_gen as u64);
        }
        (self.seek_gen, self.point, self.last, self.unread) = (row.seek_gen, row.aux, None, NONE);
        self.from = NONE;
        let flushed = self.app.jumps().iter().position(|jump| jump.seek_gen == row.seek_gen);
        self.told = flushed.map_or(self.told, |n| n + 1);
        Ok(())
    }

    fn skip(&mut self, row: &Row) {
        self.point = row.aux;
        let ours = |jump: &Jump| (jump.seek_gen, jump.to_time, jump.planned) == (0, row.aux, true);
        let untold = self.app.jumps().get(self.told..).unwrap_or(&[]);
        let chosen = untold.iter().position(ours);
        let grouped = !self.pictures.spans();
        self.from =
            chosen.and_then(|n| untold.get(n)).filter(|_| grouped).map_or(NONE, |jump| jump.from);
        self.told = chosen.map_or(self.told, |n| self.told + n + 1);
        // With a second stream the source position may be that stream's, and says nothing here.
        let had = self.last.map_or(self.app.byte(self.seek_gen), |seq| seq + 1);
        let held = chosen.is_some() && self.sc.topology.streams() == 1;
        self.unread = if held { had + self.sc.pool_slots as u64 } else { NONE };
        self.group = self.pictures.key_of(self.pictures.at_or_before(row.aux));
    }

    fn picture(&mut self, row: &Row) -> Result<(), Finding> {
        if row.seek_gen != self.seek_gen {
            return about(Kind::Generation, row, self.seek_gen as u64, row.seek_gen as u64);
        }
        if self.last.is_some_and(|had| row.seq <= had) {
            return about(Kind::DecodedTwice, row, self.last.map_or(0, |had| had + 1), row.seq);
        }
        self.last = Some(row.seq);
        let slot = Slot { pts: row.pts, duration: row.duration, ..FILLER };
        let early = before(&slot, self.pictures.spans(), self.point);
        if row.kind == RowKind::Decoded {
            return if early { about(Kind::BeforePoint, row, self.point, row.pts) } else { Ok(()) };
        }
        // Skipped is for what nothing refers to, decode-only for the rest.
        let skipped = row.kind == RowKind::Skipped;
        if !early || skipped == self.pictures.is_reference(row.seq) {
            return about(Kind::Withheld, row, self.point, row.pts);
        }
        if self.unread != NONE && row.seq < self.group && row.seq >= self.unread {
            return about(Kind::Overdecoded, row, self.unread, row.seq);
        }
        if self.from != NONE && row.seq < self.group && row.pts >= self.from {
            return about(Kind::Overdecoded, row, self.from, row.pts);
        }
        Ok(())
    }
}

fn decoder(sc: &Scenario, app: &App, decoded: &[Row], rows: &[Row]) -> Result<(), Finding> {
    let pictures = sc.pictures();
    let (none, unread) = (None, NONE);
    let mut at = Decoding {
        sc,
        app,
        pictures,
        seek_gen: 0,
        point: 0,
        last: none,
        told: 0,
        unread,
        group: 0,
        from: NONE,
    };
    for row in decoded {
        match row.kind {
            RowKind::FlushStart => at.flush(row)?,
            RowKind::Skip => at.skip(row),
            kind if kind.is(class::PICTURE) => at.picture(row)?,
            _ => {}
        }
    }
    put_out(sc, decoded, rows)
}

fn put_out(sc: &Scenario, decoded: &[Row], rows: &[Row]) -> Result<(), Finding> {
    let fed = (0..sc.sink_count()).find(|sink| sc.decoded(*sink));
    let arrived = |row: &&Row| Some(row.sink as usize) == fed && row.kind.is(class::ARRIVAL);
    let mut outs = decoded.iter().filter(|out| out.kind == RowKind::Decoded);
    for (i, row) in rows.iter().enumerate().filter(|(_, row)| arrived(row)) {
        let same = |out: &&Row| (out.seq, out.seek_gen) == (row.seq, row.seek_gen);
        if outs.find(same).is_none() {
            return Err(Finding::at_row(Kind::Undecoded, i, row, NONE, row.seq));
        }
    }
    Ok(())
}

struct Retimer<'a> {
    spec: &'a MediaSpec,
    app: &'a App,
    lane: Lane,
    state: Retiming,
    seek_gen: u32,
    first: bool,
    said: Option<Row>,
    asks: usize,
}

impl Retimer<'_> {
    fn flush(&mut self, row: &Row) -> Result<(), Finding> {
        if row.seek_gen <= self.seek_gen || row.seek_gen > self.app.seek_gen {
            return about(Kind::FlushOrder, row, self.seek_gen as u64 + 1, row.seek_gen as u64);
        }
        let target = self.app.target(row.seek_gen);
        if row.aux != target {
            return about(Kind::FlushTarget, row, target, row.aux);
        }
        self.lane.seek(self.spec, target);
        self.state.flush();
        (self.seek_gen, self.first) = (row.seek_gen, true);
        Ok(())
    }

    fn asked(&mut self, row: &Row) -> Result<(), Finding> {
        let want = self.app.asked().get(self.asks).copied();
        if want != Some(row.aux) {
            return about(Kind::RateUnsent, row, want.unwrap_or(NONE), row.aux);
        }
        self.asks += 1;
        let Some(rate) = Rate::unpack(row.aux) else {
            return about(Kind::Retimed, row, 0, row.aux);
        };
        self.state.ask(rate);
        Ok(())
    }

    fn buffer(&mut self, row: &Row) -> Result<(), Finding> {
        if row.seek_gen != self.seek_gen {
            return about(Kind::Generation, row, self.seek_gen as u64, row.seek_gen as u64);
        }
        let slot = match self.lane.take(self.spec, row, 0, false) {
            Ok(slot) => slot,
            Err((kind, want)) => return about(kind, row, want, row.seq),
        };
        if let Err((want, got)) = check_content(row, &slot, true) {
            return about(Kind::Content, row, want, got);
        }
        let first = std::mem::take(&mut self.first);
        if row.has(flag::DISCONT) != first {
            return about(Kind::Content, row, first as u64, row.has(flag::DISCONT) as u64);
        }
        let took = self.state.take();
        if (took.rate.pack(), took.out) != (row.aux, row.format as u32) {
            return about(Kind::Retimed, row, took.rate.pack(), row.aux);
        }
        let said = self.said.take();
        let as_due = match said {
            Some(word) => took.says && (word.pts, word.aux) == (row.pts, row.aux),
            None => !took.says,
        };
        if as_due {
            Ok(())
        } else {
            about(Kind::Retimed, row, took.says as u64, said.is_some() as u64)
        }
    }

    fn eos(&mut self, row: &Row) -> Result<(), Finding> {
        let owed = self.lane.owed(self.spec, (false, 0));
        match (self.lane.end, owed) {
            (Some(EndKind::Error), _) => about(Kind::Beyond, row, NONE, 0),
            (_, Some(seq)) => about(Kind::Incomplete, row, seq, NONE),
            _ => Ok(()),
        }
    }
}

fn retimer(sc: &Scenario, spec: &MediaSpec, app: &App, inner: &[Row]) -> Result<(), Finding> {
    let Ok(cursor) = media::cursor(spec, 0, sc.seed, 0) else {
        return Err(Finding::numbers(Kind::BadScenario, 0, 0));
    };
    let (lane, state) = (Lane::new(cursor), Retiming::new(sc.time.retimer));
    let mut at = Retimer { spec, app, lane, state, seek_gen: 0, first: true, said: None, asks: 0 };
    for row in inner {
        // A word to the timeline goes with the buffer taken next, and nothing else.
        if at.said.is_some() && row.kind != RowKind::Retimed {
            return about(Kind::Retimed, row, 0, 1);
        }
        match row.kind {
            RowKind::FlushStart => at.flush(row)?,
            RowKind::RateAsked => at.asked(row)?,
            RowKind::Retime => at.said = Some(*row),
            RowKind::Retimed => at.buffer(row)?,
            RowKind::Eos => at.eos(row)?,
            _ => {}
        }
    }
    match at.said {
        Some(word) => about(Kind::Retimed, &word, 0, 1),
        None => Ok(()),
    }
}

fn generic(sc: &Scenario, rows: &[Row]) -> Result<(), Finding> {
    let mut sinks = [SinkExpect { streams: 0 }; MAX_SINKS];
    for (sink, expect) in sinks.iter_mut().enumerate() {
        expect.streams = 1 << sc.topology.stream_of(sink);
    }
    let expect = Expect { sinks: &sinks[..sc.sink_count().min(MAX_SINKS)], gaps: &[] };
    let on = generic_rules(sc);
    let mut first: Option<sweep::Violation> = None;
    for rule in Rule::ALL {
        if !on[rule as usize] {
            continue;
        }
        let Err(found) = sweep::run(rule, rows, &expect) else {
            continue;
        };
        if first.is_none_or(|kept| found.index < kept.index) {
            first = Some(found);
        }
    }
    first.map_or(Ok(()), |found| Err(Finding::violation(found)))
}

pub fn sweep(
    sc: &Scenario,
    spec: &MediaSpec,
    app: &App,
    end: End,
    rows: &[Row],
) -> Result<(), Finding> {
    sweep_decoded(sc, spec, app, end, (rows, &[]))
}

pub fn sweep_decoded(
    sc: &Scenario,
    spec: &MediaSpec,
    app: &App,
    end: End,
    (rows, decoded): (&[Row], &[Row]),
) -> Result<(), Finding> {
    let plan = sc.plan();
    if plan.refused != (end == End::Refused) {
        let kind = if plan.refused { Kind::LinkMade } else { Kind::LinkRefused };
        return Err(Finding::numbers(kind, plan.made as u64, rows.len() as u64));
    }
    if plan.refused {
        // The run was never started, so it recorded nothing.
        return match rows.len() {
            0 => Ok(()),
            n => Err(Finding::numbers(Kind::RowAfterEnd, 0, n as u64)),
        };
    }
    let mut state = State { sc, spec, app, plan, sinks: [FRESH; MAX_SINKS], inner: decoded };
    let mut own = Ok(());
    for (i, row) in rows.iter().enumerate() {
        own = state.row(i, row);
        if own.is_err() {
            break;
        }
    }
    if own.is_ok() {
        own = state.close(end, rows.len());
    }
    if own.is_ok() && sc.gop.on {
        own = decoder(sc, app, decoded, rows);
    }
    if own.is_ok() && sc.time.retimer.on {
        own = retimer(sc, spec, app, decoded);
    }
    match (own, generic(sc, rows)) {
        (Err(a), Err(b)) => Err(if b.row < a.row { b } else { a }),
        (Err(found), Ok(())) | (Ok(()), Err(found)) => Err(found),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: RowKind, seq: u64, aux: u64, seek_gen: u32) -> Row {
        Row { seq, aux, seek_gen, ..Row::new(kind, 0, 0) }
    }

    #[test]
    fn the_decoder_point_counts_up_to_where_it_put_a_picture_out() {
        let (decoded, skip, flush) = (RowKind::Decoded, RowKind::Skip, RowKind::FlushStart);
        let rows = [
            row(decoded, 0, 0, 0),
            row(decoded, 1, 0, 0),
            row(skip, 0, 50, 0),
            row(skip, 0, 30, 0),
            row(decoded, 2, 0, 0),
            row(skip, 0, 90, 0),
            row(flush, 0, 7, 1),
            row(decoded, 3, 0, 1),
        ];
        let mut told = Told { at: 0, high: 0 };
        assert_eq!(told.before_out(&rows, (0, 0), 0), 0, "put out before any skip");
        assert_eq!(told.before_out(&rows, (1, 0), 1), 50, "held back past the skip");
        assert_eq!(Told { at: 0, high: 0 }.before_out(&rows, (1, 0), 0), 0, "out at once");
        assert_eq!(told.before_out(&rows, (2, 0), 0), 50, "a skip back keeps the furthest");
        assert_eq!(told.before_out(&rows, (NONE, 0), 0), 90, "the whole generation");
        assert_eq!(told.before_out(&rows, (3, 1), 4), 0, "a flush forgets them");
        assert_eq!(told.before_out(&rows, (9, 1), 0), 0, "a picture it never had");
    }
}
