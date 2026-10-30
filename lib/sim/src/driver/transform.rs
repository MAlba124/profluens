//! The fake transform, so a pipeline has groups in the middle. It delays, reorders within seeded
//! blocks with dts kept ascending, fails on a given input, or tees.

use profluens_core::batch::Inputs;
use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::ctx::Ctx;
use profluens_core::element::{Element, ElementDesc, Flow};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::feedback::Feedback;
use profluens_core::id::SrcPad;
use profluens_core::time::Timestamp;

use super::pads::{FakeError, INLINE, OUT, TEE, TRANSFORM};
use super::scenario::{MAX_DELAY, MAX_WINDOW, TransformDecl};
use crate::prng::Prng;

const HOLD: usize = MAX_DELAY as usize + MAX_WINDOW as usize + 1;
const BLOCK: usize = MAX_WINDOW as usize + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Active,
    Inline,
    Tee,
}

pub struct FakeTransform {
    decl: TransformDecl,
    shape: Shape,
    held: [Option<Buffer>; HOLD],
    held_count: usize,
    taken: u64,
    block: u64,
    fresh: bool,
    spare: Option<Buffer>,
}

impl FakeTransform {
    pub fn new(decl: TransformDecl, shape: Shape) -> Self {
        let plain = shape == Shape::Tee;
        let decl = TransformDecl {
            delay: if plain { 0 } else { decl.delay.min(MAX_DELAY) },
            window: if plain { 0 } else { decl.window.min(MAX_WINDOW) },
            ..decl
        };
        Self {
            decl,
            shape,
            held: std::array::from_fn(|_| None),
            held_count: 0,
            taken: 0,
            block: 0,
            fresh: true,
            spare: None,
        }
    }

    /// Durstenfeld, "Algorithm 235: Random permutation", CACM 7(7), 1964.
    fn order(&self, len: usize) -> [usize; BLOCK] {
        let mut order: [usize; BLOCK] = std::array::from_fn(|i| i);
        let mut rng = Prng::new(self.decl.seed).substream(self.block);
        for i in (1..len.min(BLOCK)).rev() {
            order.swap(i, rng.below(i as u64 + 1) as usize);
        }
        order
    }

    fn send_block(&mut self, ctx: &mut Ctx, len: usize) {
        let len = len.min(self.held_count).min(BLOCK);
        let order = self.order(len);
        let mut dts = [Timestamp::NONE; BLOCK];
        for (slot, held) in dts.iter_mut().zip(&self.held).take(len) {
            *slot = held.as_ref().map_or(Timestamp::NONE, |buf| buf.dts);
        }
        for (position, from) in order.iter().take(len).enumerate() {
            let Some(mut buf) = self.held[*from].take() else {
                continue;
            };
            buf.dts = dts[position];
            buf.flags.remove(BufferFlags::DISCONT);
            if std::mem::take(&mut self.fresh) {
                buf.flags.insert(BufferFlags::DISCONT);
            }
            self.send(ctx, buf);
        }
        self.held.rotate_left(len);
        self.held_count -= len;
        self.block += 1;
    }

    fn send(&mut self, ctx: &mut Ctx, buf: Buffer) {
        if let Some(mut copy) = self.spare.take() {
            let bytes = buf.memory.data();
            let room = copy.memory.as_mut_full();
            let n = bytes.len().min(room.len());
            room[..n].copy_from_slice(&bytes[..n]);
            copy.memory.set_len(n);
            (copy.pts, copy.dts, copy.duration, copy.flags) =
                (buf.pts, buf.dts, buf.duration, buf.flags);
            ctx.out(SrcPad(1)).push(copy);
        }
        ctx.out(OUT).push(buf);
    }

    fn drain(&mut self, ctx: &mut Ctx) {
        for _ in 0..HOLD {
            if self.held_count == 0 {
                break;
            }
            self.send_block(ctx, self.decl.window as usize + 1);
        }
    }

    fn discard(&mut self) {
        self.held = std::array::from_fn(|_| None);
        self.held_count = 0;
        self.block = 0;
        self.fresh = true;
    }

    fn outputs(&self) -> u32 {
        if self.shape == Shape::Tee { 2 } else { 1 }
    }
}

impl Element for FakeTransform {
    fn desc(&self) -> &'static ElementDesc {
        match self.shape {
            Shape::Active => &TRANSFORM,
            Shape::Inline => &INLINE,
            Shape::Tee => &TEE,
        }
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let need = self.decl.delay as usize + self.decl.window as usize + 1;
        for _ in 0..inputs.len() {
            // No buffer for the copy is backpressure. The input stays where it is.
            if self.shape == Shape::Tee && self.spare.is_none() {
                self.spare = ctx.try_alloc();
                if self.spare.is_none() {
                    break;
                }
            }
            let Some(buf) = inputs.pop() else { break };
            self.taken += 1;
            if self.taken == self.decl.error_at {
                let why = FakeError::DeclaredTransform { input: self.taken };
                return Err(Error::element_failed(ctx.element(), why));
            }
            // `need` is at most the table, so there is a slot unless a knob is off.
            if let Some(slot) = self.held.get_mut(self.held_count) {
                *slot = Some(buf);
                self.held_count += 1;
            }
            if self.held_count >= need {
                self.send_block(ctx, self.decl.window as usize + 1);
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => self.discard(),
            Event::Feedback { feedback: Feedback::Qos(q), .. } => {
                let (made_in, applied) = (q.lateness as u64 >> 40, ctx.applied_seek_gen());
                if made_in != applied {
                    return Err(Error::element_failed(
                        ctx.element(),
                        FakeError::StaleFeedback { made_in, applied },
                    ));
                }
            }
            Event::Eos => self.drain(ctx),
            Event::FormatChange(format) => {
                self.drain(ctx);
                for pad in 0..self.outputs() {
                    ctx.forward_format(SrcPad(pad), format.clone());
                }
            }
            Event::Segment { base, rate } => {
                self.drain(ctx);
                for pad in 0..self.outputs() {
                    ctx.push_event(SrcPad(pad), Event::Segment { base: *base, rate: *rate });
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.discard();
        self.spare = None;
    }
}
