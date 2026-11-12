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
}

impl Events {
    pub const fn new() -> Self {
        Events { buf: [Event::Release(Slot(0)); MAX_EVENTS], len: 0 }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn as_slice(&self) -> &[Event] {
        &self.buf[..self.len as usize]
    }

    pub(crate) fn push(&mut self, e: Event) {
        // only a caller handing one slot to two pictures goes past the bound
        debug_assert!((self.len as usize) < MAX_EVENTS, "events past the bound");
        if (self.len as usize) < MAX_EVENTS {
            self.buf[self.len as usize] = e;
            self.len += 1;
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
    /// more pictures than the DPB holds
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
            DpbError::Full => "more pictures than the DPB holds",
            DpbError::Fields => "field pictures are not supported",
            DpbError::NotShowable => "a frame shown again is not showable",
        })
    }
}

impl std::error::Error for DpbError {}
