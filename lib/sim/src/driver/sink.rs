//! The fake sink. Records every buffer, event and lifecycle call as a row in its SPSC tap, which
//! the driver pops after every pass.

use std::sync::Arc;

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::command::{ALL, Command, CommandError, Control};
use profluens_core::ctx::Ctx;
use profluens_core::element::{Element, ElementDesc, Flow};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::feedback::{Feedback, Qos};
use profluens_core::format::Value;
use profluens_core::id::FieldId;
use profluens_core::ring::Producer;
use profluens_core::time::{Rate, Timestamp};

use super::model::Answer;
use super::offers::{FIELD, PadOffers};
use super::pads::{FakeError, IN, SINK, supply};
use super::wire;
use crate::action::ActionKind;
use crate::profile::SinkProfile;
use crate::record::{NO_STREAM, Row, RowKind};
use crate::time::NONE;

pub const MAX_MEDDLES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meddle {
    pub flush: u32,
    pub action: u8,
    pub kind: ActionKind,
}

#[derive(Clone, Copy, Debug)]
pub struct SinkArgs {
    pub sink: u16,
    pub profile: SinkProfile,
    pub offers: PadOffers,
    pub meddles: [Meddle; MAX_MEDDLES],
    pub meddle_count: u8,
    pub point: Option<Judged>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Judged {
    Picture,
    Span,
}

impl SinkArgs {
    pub const fn new(sink: u16, profile: SinkProfile, offers: PadOffers) -> Self {
        let filler = Meddle { flush: 0, action: 0, kind: ActionKind::Pause };
        let (meddles, point) = ([filler; MAX_MEDDLES], None);
        Self { sink, profile, offers, meddles, meddle_count: 0, point }
    }

    pub fn meddle(&mut self, flush: u32, action: u8, kind: ActionKind) -> bool {
        let Some(slot) = self.meddles.get_mut(self.meddle_count as usize) else {
            return false;
        };
        *slot = Meddle { flush, action, kind };
        self.meddle_count += 1;
        true
    }
}

pub struct FakeSink {
    args: SinkArgs,
    tap: Producer<Row>,
    control: Arc<Control>,
    seek_gen: u32,
    flushes: u32,
    field: Option<FieldId>,
}

pub fn answer(result: Result<u64, CommandError>) -> Answer {
    match result {
        Ok(_) => Answer::Taken,
        Err(CommandError::Full) => Answer::Full,
        Err(CommandError::Redundant) => Answer::Redundant,
        Err(CommandError::NotRunning) => Answer::NotRunning,
        Err(CommandError::Finished) => Answer::Finished,
        Err(CommandError::NotASink) => Answer::NotASink,
        // Core's refusal, not the driver's hold-back, so the model has to explain it.
        Err(CommandError::NoSuchElement) => Answer::NoSuchElement,
        // An edit's refusal, which no appended command gets.
        Err(CommandError::OnRunThread) => Answer::NotDriven,
    }
}

/// An offset names an element, which only the driver knows, so it is not sent here.
pub fn send(control: &Control, kind: ActionKind) -> Answer {
    let command = match kind {
        ActionKind::Seek { to } => {
            return answer(control.seek_bytes(Timestamp(to), &[(ALL, 0, Timestamp(to))]));
        }
        ActionKind::Pause => Command::Pause,
        ActionKind::Resume => Command::Resume,
        ActionKind::Stop => Command::Stop,
        ActionKind::SkipTo { to } => Command::SkipTo { to_time: Timestamp(to) },
        ActionKind::SetRate { num, den } => match Rate::new(num as i64, den as i64) {
            Ok(rate) => Command::SetRate { rate },
            Err(_) => return Answer::NotDriven,
        },
        _ => return Answer::NotDriven,
    };
    answer(control.append(command))
}

fn due_at(ctx: &Ctx, pts: Timestamp) -> Timestamp {
    let at = ctx.running_time(pts).saturating_add(ctx.path_latency());
    match at.nanos() {
        Some(ns) => {
            Timestamp((ns as i128 + ctx.offset() as i128).clamp(0, (NONE - 1) as i128) as u64)
        }
        None => at,
    }
}

impl FakeSink {
    pub fn new(args: SinkArgs, tap: Producer<Row>, control: Arc<Control>) -> Self {
        Self { args, tap, control, seek_gen: 0, flushes: 0, field: None }
    }

    fn push(&mut self, ctx: &Ctx, row: Row) -> Result<(), Error> {
        let sink = self.args.sink;
        self.tap
            .try_push(row)
            .map_err(|_| Error::element_failed(ctx.element(), FakeError::TapFull { sink }))
    }

    fn note(&mut self, ctx: &Ctx, kind: RowKind, aux: u64) -> Result<(), Error> {
        let row = Row { aux, seek_gen: self.seek_gen, ..Row::new(kind, self.args.sink, NO_STREAM) };
        self.push(ctx, row)
    }

    fn before_point(&self, ctx: &Ctx, front: &Buffer) -> bool {
        match self.args.point {
            Some(Judged::Picture) => ctx.before_presentation(front.pts, Timestamp::ZERO),
            Some(Judged::Span) => ctx.before_presentation(front.pts, front.duration),
            None => false,
        }
    }

    fn take(&mut self, ctx: &mut Ctx, buf: &Buffer, before: bool) -> Result<(), Error> {
        let profile = &self.args.profile;
        let running = ctx.now().0;
        let late = running.saturating_sub(due_at(ctx, buf.pts).0);
        let bounded = profile.paced && profile.lateness_bound != NONE && buf.pts.is_some();
        let dropped = bounded && running != NONE && late > profile.lateness_bound;
        let kind = match (before, dropped) {
            (true, _) => RowKind::Before,
            (false, true) => RowKind::Drop,
            (false, false) => RowKind::Buffer,
        };
        if kind == RowKind::Drop {
            let lateness = ((ctx.applied_seek_gen() << 40) | late.min((1 << 40) - 1)) as i64;
            ctx.publish_feedback(IN, Feedback::Qos(Qos { lateness, earliest: Timestamp(running) }));
        }
        let mut row = Row {
            pts: buf.pts.0,
            dts: buf.dts.0,
            duration: buf.duration.0,
            aux: if dropped { late } else { running },
            seek_gen: self.seek_gen,
            flags: buf.flags.0 as u16,
            ..Row::new(kind, self.args.sink, NO_STREAM)
        };
        // Bytes that are no tag keep the row streamless, which the sweep reports.
        if let Some(tag) = wire::read(buf.memory.data()) {
            row.stream = tag.stream as u16;
            row.seq = tag.seq;
            row.seek_gen = tag.seek_gen;
            row.size = tag.size;
            row.format = tag.format;
        }
        self.push(ctx, row)
    }

    fn flush(&mut self, ctx: &Ctx) -> Result<(), Error> {
        let to = ctx.presentation_point().map_or(NONE, |at| at.0);
        self.seek_gen = ctx.applied_seek_gen() as u32;
        self.flushes += 1;
        self.note(ctx, RowKind::FlushStart, to)?;
        for i in 0..(self.args.meddle_count as usize).min(MAX_MEDDLES) {
            let meddle = self.args.meddles[i];
            if meddle.flush != self.flushes {
                continue;
            }
            let next_gen = ctx.seek_gen() + 1;
            let said = send(&self.control, meddle.kind);
            let row = Row {
                seq: meddle.action as u64,
                aux: said as u64,
                seek_gen: next_gen as u32,
                ..Row::new(RowKind::Command, self.args.sink, NO_STREAM)
            };
            self.push(ctx, row)?;
        }
        Ok(())
    }
}

impl Element for FakeSink {
    fn desc(&self) -> &'static ElementDesc {
        &SINK[self.args.offers.probed() as usize]
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        match self.args.offers.probed() {
            true => supply(&self.args.offers, |offers| ctx.offer_sink(IN, offers)),
            false => Ok(()),
        }
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.field = ctx.field_id(FIELD);
        self.note(ctx, RowKind::Start, 0)?;
        let linked = ctx.negotiated_sink(IN).and_then(|format| format.get(self.field?));
        match linked {
            Some(Value::Int(id)) => self.note(ctx, RowKind::Format, id as u64),
            _ => Ok(()),
        }
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        for _ in 0..inputs.len() {
            if self.args.point.is_some() && ctx.command_pending() {
                break;
            }
            let Some(front) = inputs.front() else { break };
            let before = self.before_point(ctx, front);
            // An early buffer stays in the input, and `due` armed the wake.
            if !before && self.args.profile.paced && !ctx.due(front.pts) {
                break;
            }
            let Some(buf) = inputs.pop() else { break };
            if before {
                ctx.count_drop();
            }
            self.take(ctx, &buf, before)?;
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FormatChange(format) => {
                let id = match self.field.and_then(|field| format.get(field)) {
                    Some(Value::Int(id)) => id as u64,
                    _ => 0,
                };
                self.note(ctx, RowKind::Format, id)
            }
            Event::Segment { base, .. } => self.note(ctx, RowKind::Segment, base.0),
            Event::Gap { until } => self.note(ctx, RowKind::Gap, until.0),
            Event::Tags(_) => self.note(ctx, RowKind::Tags, 0),
            Event::Patch { offset, .. } => self.note(ctx, RowKind::Patch, *offset),
            Event::Eos => self.note(ctx, RowKind::Eos, 0),
            Event::FlushStart => self.flush(ctx),
            Event::FlushStop => self.note(ctx, RowKind::FlushStop, 0),
            Event::Paused => self.note(ctx, RowKind::Paused, 0),
            Event::Resumed => self.note(ctx, RowKind::Resumed, 0),
            Event::SkippedTo { to_time } if self.args.point.is_some() => {
                self.note(ctx, RowKind::Skip, to_time.0)
            }
            Event::SkippedTo { .. } => Ok(()),
            Event::PropChanged { .. } => self.note(ctx, RowKind::PropChanged, 0),
            Event::Offset { by } => self.note(ctx, RowKind::Offset, *by as u64),
            Event::RateAsked { .. } => Ok(()),
            Event::SrcLinked { .. } | Event::SrcJoined { .. } | Event::SinkLinked { .. } => Ok(()),
            Event::Feedback { .. } => Ok(()),
        }
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        // A full tap at the very end loses the row, and the sweep then misses it.
        let _ = self.note(ctx, RowKind::Stop, 0);
    }
}
