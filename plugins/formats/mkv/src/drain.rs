//! Draining a muxer's scatter output downstream, shared with `pf-mp4`'s `mp4mux`.

use profluens_core::buffer::{Buffer, BufferFlags};
use profluens_core::ctx::Ctx;
use profluens_core::id::{FormatId, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::memory::Memory;
use profluens_core::time::Timestamp;

use crate::writer::{MuxOut, MuxPiece};

/// Payloads at or below this are copied into the open slot. One buffer is one sink write, so small
/// AAC frames as their own buffers cost a write each. A page keeps video zero-copy.
pub const COALESCE_MAX: usize = 4096;

fn emit_payload(ctx: &mut Ctx, pad: SrcPad, memory: Memory, format: FormatId) {
    ctx.out(pad).push(Buffer {
        memory,
        pts: Timestamp::NONE,
        dts: Timestamp::NONE,
        duration: Timestamp::NONE,
        flags: BufferFlags::empty(),
        format,
        sync: None,
    });
}

pub fn drain(
    out: &mut MuxOut,
    off: &mut usize,
    ctx: &mut Ctx,
    pad: SrcPad,
    format: FormatId,
) -> bool {
    let mut slot: Option<Buffer> = None;
    fn flush(ctx: &mut Ctx, pad: SrcPad, slot: &mut Option<Buffer>) {
        if let Some(buf) = slot.take()
            && !buf.memory.is_empty()
        {
            ctx.out(pad).push(buf);
        }
    }
    loop {
        match out.front() {
            None => break,
            Some(&MuxPiece::Bytes { start, end }) => {
                let mut at = start + *off;
                while at < end {
                    if slot.as_ref().is_some_and(|b| b.memory.len() == b.memory.capacity()) {
                        flush(ctx, pad, &mut slot);
                    }
                    if slot.is_none() {
                        let Some(buf) = ctx.try_alloc() else {
                            *off = at - start;
                            return false;
                        };
                        debug_assert!(buf.memory.capacity() > 0, "zero-capacity slot");
                        slot = Some(buf);
                    }
                    let buf = slot.as_mut().expect("slot open");
                    let filled = buf.memory.len();
                    let n = (buf.memory.capacity() - filled).min(end - at);
                    buf.memory.as_mut_full()[filled..filled + n]
                        .copy_from_slice(&out.header_bytes()[at..at + n]);
                    buf.memory.set_len(filled + n);
                    at += n;
                }
                *off = 0;
                out.pop_front();
            }
            Some(MuxPiece::Payload(mem)) => {
                let len = mem.len();
                // A small payload is copied whole, never split, so the carry stays the byte run
                // cursor alone.
                let coalesce =
                    len <= COALESCE_MAX && slot.as_ref().is_none_or(|b| b.memory.capacity() >= len);
                if coalesce {
                    if slot.as_ref().is_some_and(|b| b.memory.capacity() - b.memory.len() < len) {
                        flush(ctx, pad, &mut slot);
                    }
                    if slot.is_none() {
                        let Some(buf) = ctx.try_alloc() else {
                            flush(ctx, pad, &mut slot);
                            return false;
                        };
                        slot = Some(buf);
                    }
                    let buf = slot.as_mut().expect("slot open");
                    if buf.memory.capacity() >= len {
                        let filled = buf.memory.len();
                        buf.memory.as_mut_full()[filled..filled + len].copy_from_slice(mem.data());
                        buf.memory.set_len(filled + len);
                        out.pop_front();
                        continue;
                    }
                }
                flush(ctx, pad, &mut slot);
                let Some(MuxPiece::Payload(mem)) = out.pop_front() else {
                    unreachable!("front was a payload")
                };
                emit_payload(ctx, pad, mem, format);
            }
        }
    }
    flush(ctx, pad, &mut slot);
    out.reclaim();
    true
}

pub fn drain_exact(
    out: &mut MuxOut,
    off_at: &mut usize,
    ctx: &mut Ctx,
    pad: SrcPad,
    format: FormatId,
) {
    const GATHER_MAX: usize = 256 << 10;
    let mut off = std::mem::take(off_at);
    loop {
        let mut total = 0usize;
        let mut o = off;
        for piece in out.pieces() {
            let n = match piece {
                MuxPiece::Bytes { start, end } => (end - start).saturating_sub(o),
                MuxPiece::Payload(mem) if mem.len() <= COALESCE_MAX => mem.len(),
                MuxPiece::Payload(_) => break,
            };
            o = 0;
            if total != 0 && total + n > GATHER_MAX {
                break;
            }
            total += n;
            if total >= GATHER_MAX {
                break;
            }
        }
        if total > 0 {
            let Some(mut buf) = ctx.alloc_exact(total) else {
                // Only a heap out of memory refuses a gather this small, the rest waits.
                log!(&*ctx, Level::Error, "exact_refused", bytes = total);
                *off_at = off;
                break;
            };
            let mut filled = 0usize;
            while filled < total {
                match out.front() {
                    Some(&MuxPiece::Bytes { start, end }) => {
                        let at = start + off;
                        off = 0;
                        let n = end - at;
                        if filled + n > total {
                            break;
                        }
                        buf.memory.as_mut_full()[filled..filled + n]
                            .copy_from_slice(&out.header_bytes()[at..end]);
                        filled += n;
                        out.pop_front();
                    }
                    Some(MuxPiece::Payload(mem))
                        if mem.len() <= COALESCE_MAX && filled + mem.len() <= total =>
                    {
                        let n = mem.len();
                        buf.memory.as_mut_full()[filled..filled + n].copy_from_slice(mem.data());
                        filled += n;
                        out.pop_front();
                    }
                    _ => break,
                }
            }
            buf.memory.set_len(filled);
            ctx.out(pad).push(buf);
            continue;
        }
        match out.pop_front() {
            Some(MuxPiece::Payload(mem)) => emit_payload(ctx, pad, mem, format),
            Some(MuxPiece::Bytes { start, end }) => {
                debug_assert!(start + off >= end, "a byte run with bytes left gathers");
                off = 0;
            }
            None => break,
        }
    }
    out.reclaim();
}
