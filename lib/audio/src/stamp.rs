//! Timestamps through an element that makes PCM from PCM.

use profluens_core::batch::Inputs;
use profluens_core::buffer::Buffer;
use profluens_core::ctx::Ctx;
use profluens_core::id::SrcPad;
use profluens_core::time::Timestamp;

use crate::error::AudioErrorKind;
use crate::trim::span_at;

/// Put `buf` out unchanged, whole frames of `stride` bytes at `rate`.
pub fn forward_pcm(ctx: &mut Ctx, pad: SrcPad, mut buf: Buffer, (stride, rate): (usize, u32)) {
    (buf.pts, buf.duration) = span_at(buf.pts, 0, buf.memory.len() / stride.max(1), rate);
    ctx.out(pad).push(buf);
}

/// The next input buffer, once the pool has a slot for its output.
pub fn take_pcm(ctx: &mut Ctx, inputs: &mut Inputs<'_>) -> Option<Buffer> {
    let dry = |ctx: &Ctx| {
        let pool = ctx.pool_stats();
        pool.outstanding >= pool.max_slots
    };
    if inputs.is_empty() {
        return None;
    }
    if dry(ctx) {
        ctx.await_recycle();
        if dry(ctx) {
            return None;
        }
    }
    inputs.pop()
}

/// Push `pcm` stamped `pts` on `pad`, in pool slots of whole frames.
pub fn push_pcm(
    ctx: &mut Ctx,
    pad: SrcPad,
    pcm: &[u8],
    (stride, rate): (usize, u32),
    pts: Timestamp,
) -> Result<(), AudioErrorKind> {
    if stride == 0 {
        return Err(AudioErrorKind::OutputStrideUnknown);
    }
    let mut off = 0;
    while off < pcm.len() {
        let mut buf = ctx.alloc();
        let capacity = buf.memory.capacity();
        let room = capacity - capacity % stride;
        if room == 0 {
            return Err(AudioErrorKind::SlotTooSmall { capacity, stride });
        }
        let n = room.min(pcm.len() - off);
        buf.memory.as_mut_full()[..n].copy_from_slice(&pcm[off..off + n]);
        buf.memory.set_len(n);
        (buf.pts, buf.duration) = span_at(pts, off / stride, n / stride, rate);
        ctx.out(pad).push(buf);
        off += n;
    }
    Ok(())
}
