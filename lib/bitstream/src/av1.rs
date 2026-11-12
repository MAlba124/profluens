use parse::header::{FrameHeader, FrameType, NUM_REF_FRAMES, REFS_PER_FRAME};
use parse::obu::{Av1Error, OBU_SEQUENCE_HEADER, obus};
use parse::seq::{SequenceHeader, parse_sequence_header};
use parse::stream::{Frame, Stream};

use crate::{DpbError, Event, Events, Fate, Slot};

pub mod parse;
pub mod write;

/// The first sequence header of a temporal unit, None when it carries none or a broken one.
pub fn sequence_of(tu: &[u8]) -> Option<SequenceHeader> {
    let o = obus(tu).map_while(Result::ok).find(|o| o.obu_type == OBU_SEQUENCE_HEADER)?;
    parse_sequence_header(o.payload).ok()
}

/// Reference slots, NUM_REF_FRAMES.
pub const NUM_SLOTS: usize = NUM_REF_FRAMES;
/// LAST to ALTREF, REFS_PER_FRAME.
pub const NUM_REFS: usize = REFS_PER_FRAME;

/// A decoded frame in a reference slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ref {
    pub slot: Slot,
    /// show_existing_frame may put it out again
    pub showable: bool,
}

impl Ref {
    const EMPTY: Ref = Ref { slot: Slot(0), showable: false };
}

/// The parser and the reference slots of an AV1 stream. The parser keeps the header side of
/// the slots (7.20, 7.21), this the decoded frames in them. Frames go out in decode order, a
/// slot shown again goes out again. No hardware, the caller decodes.
pub struct Dpb {
    stream: Stream,
    /// the frame `next_frame` gave last, the parser's slots take it before the next one
    parsed: Option<Frame>,
    slots: [Option<Ref>; NUM_SLOTS],
    /// no key frame yet, or a discard, a parse error or a new sequence came
    awaiting_key: bool,
}

impl Dpb {
    pub fn new() -> Self {
        Dpb { stream: Stream::new(), parsed: None, slots: [None; NUM_SLOTS], awaiting_key: true }
    }

    /// The next frame of temporal unit `tu` from byte `at` on, None once none is left. The
    /// parser's slots take the frame before it first, decoded or not. A parse error resets
    /// the parser and waits for a key frame, so does a new sequence until its key frame.
    pub fn next_frame(&mut self, tu: &[u8], at: &mut usize) -> Result<Option<Frame>, Av1Error> {
        if let Some(f) = self.parsed.take() {
            self.stream.finish(&f);
        }
        match self.stream.next_frame(tu, at) {
            Ok(Some(f)) => {
                if std::mem::take(&mut self.stream.seq_changed)
                    && f.header.frame_type != FrameType::Key
                {
                    self.awaiting_key = true;
                }
                self.parsed = Some(f);
                Ok(Some(f))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                self.stream.reset();
                self.awaiting_key = true;
                Err(e)
            }
        }
    }

    /// The parser, its sequence header, slots before the current frame and tile groups.
    pub fn stream(&self) -> &Stream {
        &self.stream
    }

    /// Starts a frame `next_frame` gave. Frames before the first key frame are `Fate::Skip`,
    /// so is a frame shown again, with its `Output` in `ev`. A key frame shown again also takes
    /// every slot (7.21), not showable any more, and decoding starts at it. A slot the frame
    /// names empty is `MissingReference`, a frame shown again that is not showable
    /// `NotShowable`, nothing changes then. Otherwise `Shown` or `Hidden` by show_frame.
    pub fn begin(&mut self, h: &FrameHeader, ev: &mut Events) -> Result<Fate, DpbError> {
        ev.clear();
        let key = h.frame_type == FrameType::Key;
        if self.awaiting_key && !key {
            return Ok(Fate::Skip);
        }
        if h.show_existing_frame {
            let r = self.slots[usize::from(h.frame_to_show_map_idx & 7)]
                .ok_or(DpbError::MissingReference)?;
            if !r.showable {
                return Err(DpbError::NotShowable);
            }
            ev.push(Event::Output(r.slot));
            if key {
                let shown = Ref { showable: false, ..r };
                self.refresh(h.refresh_frame_flags, shown, ev);
                self.awaiting_key = false;
            }
            return Ok(Fate::Skip);
        }
        if !h.frame_type.is_intra() && self.refs(h).is_none() {
            return Err(DpbError::MissingReference);
        }
        self.awaiting_key = false;
        Ok(if h.show_frame { Fate::Shown } else { Fate::Hidden })
    }

    /// The seven references of an inter frame by ref_frame_idx, None for an intra frame or a
    /// slot empty.
    pub fn refs(&self, h: &FrameHeader) -> Option<[Ref; NUM_REFS]> {
        if h.frame_type.is_intra() {
            return None;
        }
        let mut out = [Ref::EMPTY; NUM_REFS];
        for (o, &i) in out.iter_mut().zip(&h.ref_frame_idx) {
            *o = self.slots[usize::from(i & 7)]?;
        }
        Some(out)
    }

    pub fn slots(&self) -> &[Option<Ref>; NUM_SLOTS] {
        &self.slots
    }

    /// Ends a decoded frame in `slot`. `Output` when shown, then the slots refresh_frame_flags
    /// names take it and a frame no slot holds any more is released, the frame itself too
    /// when no slot takes it.
    pub fn end(&mut self, h: &FrameHeader, slot: Slot, ev: &mut Events) {
        ev.clear();
        if h.show_frame {
            ev.push(Event::Output(slot));
        }
        let new = Ref { slot, showable: h.showable_frame };
        self.refresh(h.refresh_frame_flags, new, ev);
        if !self.holds(slot) {
            ev.push(Event::Release(slot));
        }
    }

    /// A seek or a stream error. Empties the slots and the parser's, decoding resumes at the
    /// next key frame.
    pub fn discard(&mut self, ev: &mut Events) {
        ev.clear();
        let old = std::mem::replace(&mut self.slots, [None; NUM_SLOTS]);
        self.release_unheld(&old, ev);
        self.stream.reset();
        self.parsed = None;
        self.awaiting_key = true;
    }

    fn refresh(&mut self, flags: u8, new: Ref, ev: &mut Events) {
        let mut old = [None; NUM_SLOTS];
        for (i, s) in self.slots.iter_mut().enumerate() {
            if flags >> i & 1 == 1 {
                old[i] = s.replace(new);
            }
        }
        self.release_unheld(&old, ev);
    }

    /// A frame that failed. Frames up to the next key frame are `Fate::Skip`, the slots and the
    /// parser stay so the frames between still parse.
    pub fn await_key(&mut self) {
        self.awaiting_key = true;
    }

    /// Frames up to the next key frame are `Fate::Skip`.
    pub fn awaiting_key(&self) -> bool {
        self.awaiting_key
    }

    /// The caller's frames are gone, as a decode session reopened at a key frame. Empties the
    /// slots, the parser keeps its own and the wait for a key stays as it was, so the frames
    /// after still parse and one naming an emptied slot is `MissingReference`.
    pub fn drop_frames(&mut self, ev: &mut Events) {
        ev.clear();
        let old = std::mem::replace(&mut self.slots, [None; NUM_SLOTS]);
        self.release_unheld(&old, ev);
    }

    fn holds(&self, slot: Slot) -> bool {
        self.slots.iter().flatten().any(|r| r.slot == slot)
    }

    /// Releases each frame of `old` once, unless a slot still holds it.
    fn release_unheld(&self, old: &[Option<Ref>; NUM_SLOTS], ev: &mut Events) {
        for (i, o) in old.iter().enumerate() {
            let Some(o) = o else {
                continue;
            };
            let seen = old[..i].iter().flatten().any(|p| p.slot == o.slot);
            if !seen && !self.holds(o.slot) {
                ev.push(Event::Release(o.slot));
            }
        }
    }
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
