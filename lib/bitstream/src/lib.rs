pub mod av1;
pub mod h264;
pub mod h265;
pub mod vp9;

use std::fmt;

/// A picture as the caller knows it, an index into its own surfaces or pool. The DPB never
/// looks inside it, VA maps it to a VASurfaceID and n100 to a pool handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot(pub u16);

/// What a DPB call asks of the caller, in the order the spec's process produces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// put the picture out, the caller holds it until its consumer is done
    Output(Slot),
    /// the DPB is done with the picture, no reference and no output left waiting
    Release(Slot),
}

/// Events one call can make. At most 16 references and 17 pictures waiting for output are
/// distinct slots, each put out and released once.
pub const MAX_EVENTS: usize = 2 * (16 + 17);

/// Events of one call, each call replaces the last one's. Fixed capacity so the per picture
/// path does not allocate.
pub struct Events {
    buf: [Event; MAX_EVENTS],
    len: u8,
    overflowed: bool,
}

impl Events {
    pub const fn new() -> Self {
        Events { buf: [Event::Release(Slot(0)); MAX_EVENTS], len: 0, overflowed: false }
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.overflowed = false;
    }

    pub fn as_slice(&self) -> &[Event] {
        &self.buf[..self.len as usize]
    }

    /// Whether the call dropped events past `MAX_EVENTS`. No DPB gets there on its own, only a
    /// caller handing one slot to two live pictures, and a dropped `Release` leaks its slot.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub(crate) fn push(&mut self, e: Event) {
        if (self.len as usize) < MAX_EVENTS {
            self.buf[self.len as usize] = e;
            self.len += 1;
        } else {
            self.overflowed = true;
        }
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

/// What becomes of a picture, decided by `begin` before it is decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// not decoded, before the first key or a leading picture nothing can reference
    Skip,
    /// decoded for the references of later pictures, never put out
    Hidden,
    /// decoded and put out
    Shown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DpbError {
    /// a reference the picture uses is not in the DPB, it is dropped until the next key
    MissingReference,
    /// no room for the picture, more wait for output than the reorder bound (h264) or every
    /// frame store is a reference (h265), the DPB does not track the stream's dpb size
    Full,
    /// field pictures
    Fields,
    /// show_existing_frame of an AV1 frame not marked showable
    NotShowable,
}

impl fmt::Display for DpbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DpbError::MissingReference => "a reference picture is missing",
            DpbError::Full => "no room in the DPB for the picture",
            DpbError::Fields => "field pictures are not supported",
            DpbError::NotShowable => "a frame shown again is not showable",
        })
    }
}

impl std::error::Error for DpbError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_past_the_bound_are_dropped_and_flagged() {
        let mut ev = Events::new();
        for i in 0..MAX_EVENTS {
            ev.push(Event::Output(Slot(i as u16)));
        }
        assert!(!ev.overflowed());
        ev.push(Event::Release(Slot(0)));
        assert!(ev.overflowed());
        assert_eq!(ev.as_slice().len(), MAX_EVENTS);
        assert_eq!(ev.as_slice()[MAX_EVENTS - 1], Event::Output(Slot(MAX_EVENTS as u16 - 1)));
        ev.clear();
        assert!(!ev.overflowed() && ev.as_slice().is_empty());
    }
}
