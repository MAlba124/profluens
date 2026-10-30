//! The fake decoder, doing with each picture of stream 0 what a real decoder does around a seek
//! to a time.

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::ctx::Ctx;
use profluens_core::element::{Element, ElementDesc, Flow};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::ring::Producer;
use profluens_core::time::Timestamp;

use super::gop::{GopDecl, MAX_REORDER, Pictures};
use super::pads::{DECODER, DECODER_INLINE, FakeError, OUT};
use super::wire;
use crate::record::{Row, RowKind};
use crate::time::NONE;

pub const DECODER_ROWS: u16 = u16::MAX - 1;

const HOLD: usize = MAX_REORDER as usize + 1;

pub struct FakeDecoder {
    pictures: Pictures,
    reorder: usize,
    inline: bool,
    tap: Producer<Row>,
    held: [Option<Buffer>; HOLD],
    held_count: usize,
    reference: Option<u64>,
    seek_gen: u32,
}

impl FakeDecoder {
    pub fn new(pictures: Pictures, gop: &GopDecl, inline: bool, tap: Producer<Row>) -> Self {
        Self {
            pictures,
            reorder: (gop.reorder as usize).min(MAX_REORDER as usize),
            inline,
            tap,
            held: std::array::from_fn(|_| None),
            held_count: 0,
            reference: None,
            seek_gen: 0,
        }
    }

    fn push(&mut self, ctx: &Ctx, row: Row) -> Result<(), Error> {
        let full =
            |_| Error::element_failed(ctx.element(), FakeError::TapFull { sink: DECODER_ROWS });
        self.tap.try_push(Row { seek_gen: self.seek_gen, ..row }).map_err(full)
    }

    fn note(&mut self, ctx: &Ctx, kind: RowKind, to: Timestamp) -> Result<(), Error> {
        self.push(ctx, Row { aux: to.0, ..Row::new(kind, DECODER_ROWS, 0) })
    }

    fn did(
        &mut self,
        ctx: &Ctx,
        kind: RowKind,
        seq: u64,
        at: (Timestamp, Timestamp),
    ) -> Result<(), Error> {
        let row = Row { seq, pts: at.0.0, duration: at.1.0, ..Row::new(kind, DECODER_ROWS, 0) };
        self.push(ctx, row)
    }

    fn before(&self, ctx: &Ctx, at: (Timestamp, Timestamp)) -> bool {
        let length = if self.pictures.spans() { at.1 } else { Timestamp::ZERO };
        ctx.before_presentation(at.0, length)
    }

    fn hold(&mut self, ctx: &mut Ctx, buf: Buffer) {
        // `reorder` is below the table's length, so there is a slot.
        if let Some(slot) = self.held.get_mut(self.held_count) {
            *slot = Some(buf);
            self.held_count += 1;
        }
        if self.held_count > self.reorder {
            self.release(ctx, 1);
        }
    }

    fn release(&mut self, ctx: &mut Ctx, count: usize) {
        let count = count.min(self.held_count);
        for slot in self.held.iter_mut().take(count) {
            if let Some(buf) = slot.take() {
                ctx.out(OUT).push(buf);
            }
        }
        self.held.rotate_left(count);
        self.held_count -= count;
    }

    fn drop_before(&mut self, ctx: &Ctx) {
        let mut kept = 0;
        for at in 0..self.held_count {
            let Some(buf) = self.held[at].take() else {
                continue;
            };
            if self.before(ctx, (buf.pts, buf.duration)) {
                ctx.count_drop();
                continue;
            }
            self.held[kept] = Some(buf);
            kept += 1;
        }
        self.held_count = kept;
    }

    fn discard(&mut self) {
        self.held = std::array::from_fn(|_| None);
        self.held_count = 0;
    }

    fn decode(&mut self, ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Result<bool, Error> {
        let Some(front) = inputs.front() else {
            return Ok(false);
        };
        let failed = |why| Error::element_failed(ctx.element(), why);
        let Some(tag) = wire::read(front.memory.data()) else {
            return Err(failed(FakeError::Untagged));
        };
        let (seq, at) = (tag.seq, (front.pts, front.duration));
        let before = self.before(ctx, at);
        let reference = self.pictures.is_reference(seq);
        if before && !reference {
            inputs.pop();
            ctx.count_drop();
            return self.did(ctx, RowKind::Skipped, seq, at).map(|()| true);
        }
        match self.pictures.needs(seq) {
            Some(needs) if self.reference != Some(needs) => {
                return Err(failed(FakeError::MissingReference { picture: seq, needs }));
            }
            _ => {}
        }
        let Some(buf) = inputs.pop() else {
            return Ok(false);
        };
        if reference {
            self.reference = Some(seq);
        }
        if before {
            ctx.count_drop();
            return self.did(ctx, RowKind::DecodeOnly, seq, at).map(|()| true);
        }
        self.did(ctx, RowKind::Decoded, seq, at)?;
        self.hold(ctx, buf);
        Ok(true)
    }
}

impl Element for FakeDecoder {
    fn desc(&self) -> &'static ElementDesc {
        if self.inline { &DECODER_INLINE } else { &DECODER }
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        for _ in 0..inputs.len() {
            // The point may be about to move, and what is decoded here is judged by it.
            if ctx.command_pending() || !self.decode(ctx, &mut inputs)? {
                break;
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => {
                self.discard();
                self.reference = None;
                self.seek_gen = ctx.applied_seek_gen() as u32;
                let to = ctx.presentation_point().unwrap_or(Timestamp(NONE));
                self.note(ctx, RowKind::FlushStart, to)
            }
            Event::SkippedTo { to_time } => {
                self.drop_before(ctx);
                self.note(ctx, RowKind::Skip, *to_time)
            }
            Event::Eos => {
                self.release(ctx, HOLD);
                Ok(())
            }
            // A pure transport for events. What it holds from before one goes out ahead of it.
            Event::FormatChange(format) => {
                self.release(ctx, HOLD);
                ctx.forward_format(OUT, format.clone());
                Ok(())
            }
            Event::Segment { base, rate } => {
                self.release(ctx, HOLD);
                ctx.push_event(OUT, Event::Segment { base: *base, rate: *rate });
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.discard();
    }
}
