//! `subtitle/bitmap`, straight-alpha RGBA rectangles placed in a reference video frame. One
//! definition serves the three decoders, the wire and the overlay: a little-endian `#[repr(C)]`
//! header, a rect table and the pixels, 4 byte aligned.
//!
//! ```text
//! BitmapHeader               magic "BSUB", version, reference video size, rect count
//! [BitmapRect; n_rects]      placement, size and the byte range of each rect's RGBA
//! pixels                     width * height * 4 bytes per rect, row-major, in table order
//! ```
//!
//! A payload with no rect clears the caption.

use pf_subparse::pod::{self, Pod};

/// The 4-byte magic prefixing a payload.
pub const BITMAP_MAGIC: [u8; 4] = *b"BSUB";
/// Layout version.
pub const BITMAP_VERSION: u16 = 2;
/// Alignment of the payload start.
pub const BITMAP_ALIGN: usize = 4;
/// Header length in bytes, the rect table follows it.
pub const BITMAP_HEADER_LEN: usize = std::mem::size_of::<BitmapHeader>();
/// One rect record's length in bytes.
pub const BITMAP_RECT_LEN: usize = std::mem::size_of::<BitmapRect>();

/// The payload header.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BitmapHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub header_len: u16,
    pub total_len: u32,
    /// The composition's reference frame size. Zero means the video frame itself.
    pub video_width: u32,
    pub video_height: u32,
    pub n_rects: u32,
}

/// One rectangle of pixels and where it sits in the reference frame.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BitmapRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Byte offset of the RGBA from the payload start, `width * height * 4` bytes.
    pub rgba_off: u32,
    pub rgba_len: u32,
}

#[allow(unsafe_code)]
mod pod_impls {
    use super::*;
    // SAFETY: both are `#[repr(C)]` of integers and byte arrays with no implicit padding, which
    // the size assertions pin, and any bit pattern is a value.
    unsafe impl Pod for BitmapHeader {}
    unsafe impl Pod for BitmapRect {}
}

const _: () = {
    assert!(std::mem::size_of::<BitmapHeader>() == 24);
    assert!(std::mem::size_of::<BitmapRect>() == 24);
    assert!(std::mem::align_of::<BitmapHeader>() == BITMAP_ALIGN);
    assert!(std::mem::align_of::<BitmapRect>() == BITMAP_ALIGN);
};

/// Why a payload was refused, by the builder or by [`BitmapRef::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapError {
    Short {
        have: usize,
    },
    Misaligned,
    Magic,
    Version(u16),
    Length {
        stated: u32,
        have: usize,
    },
    Rects,
    Pixels,
    /// The builder's slot cannot hold the rects it was told of.
    DoesNotFit {
        capacity: usize,
    },
}

impl std::fmt::Display for BitmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BitmapError::Short { have } => write!(f, "bitmap payload of {have} bytes, no header"),
            BitmapError::Misaligned => {
                write!(f, "bitmap payload not {BITMAP_ALIGN} byte aligned")
            }
            BitmapError::Magic => f.write_str("bitmap magic missing"),
            BitmapError::Version(v) => write!(f, "bitmap version {v}, want {BITMAP_VERSION}"),
            BitmapError::Length { stated, have } => {
                write!(f, "bitmap states {stated} bytes, buffer has {have}")
            }
            BitmapError::Rects => f.write_str("bitmap rect table out of bounds"),
            BitmapError::Pixels => f.write_str("bitmap rect pixels out of bounds or mis-sized"),
            BitmapError::DoesNotFit { capacity } => {
                write!(f, "bitmap does not fit a {capacity} byte slot")
            }
        }
    }
}

impl std::error::Error for BitmapError {}

/// `width * height * 4`, `None` past `u32`.
pub fn rgba_len(width: u32, height: u32) -> Option<u32> {
    width.checked_mul(height)?.checked_mul(4)
}

/// The payload length for rects of these sizes, `None` past `u32`.
pub fn payload_len(sizes: &[(u32, u32)]) -> Option<usize> {
    let table = sizes.len().checked_mul(BITMAP_RECT_LEN)?;
    let mut total = BITMAP_HEADER_LEN.checked_add(table)?;
    for &(w, h) in sizes {
        total = total.checked_add(rgba_len(w, h)? as usize)?;
    }
    u32::try_from(total).ok().map(|_| total)
}

/// Writes one payload straight into a slot. Rects come in table order, each handing back its
/// pixel slice to render into.
pub struct BitmapBuilder<'a> {
    out: &'a mut [u8],
    video: (u32, u32),
    n_rects: u32,
    filled: u32,
    at: usize,
}

impl<'a> BitmapBuilder<'a> {
    /// A payload of `n_rects` rects for a `video_width` x `video_height` reference frame.
    pub fn new(
        out: &'a mut [u8],
        video_width: u32,
        video_height: u32,
        n_rects: u32,
    ) -> Result<BitmapBuilder<'a>, BitmapError> {
        if !out.as_ptr().addr().is_multiple_of(BITMAP_ALIGN) {
            return Err(BitmapError::Misaligned);
        }
        let table = (n_rects as usize).checked_mul(BITMAP_RECT_LEN).ok_or(BitmapError::Rects)?;
        let at = BITMAP_HEADER_LEN.checked_add(table).ok_or(BitmapError::Rects)?;
        if at > out.len() {
            return Err(BitmapError::DoesNotFit { capacity: out.len() });
        }
        Ok(BitmapBuilder { out, video: (video_width, video_height), n_rects, filled: 0, at })
    }

    /// Record the next rect and hand back its pixel bytes.
    pub fn rect(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<&mut [u8], BitmapError> {
        if self.filled >= self.n_rects {
            return Err(BitmapError::Rects);
        }
        let len = rgba_len(width, height).ok_or(BitmapError::Pixels)? as usize;
        let end = self.at.checked_add(len).ok_or(BitmapError::Pixels)?;
        if end > self.out.len() || u32::try_from(end).is_err() {
            return Err(BitmapError::DoesNotFit { capacity: self.out.len() });
        }
        let rec =
            BitmapRect { x, y, width, height, rgba_off: self.at as u32, rgba_len: len as u32 };
        let rec_at = BITMAP_HEADER_LEN + self.filled as usize * BITMAP_RECT_LEN;
        self.out[rec_at..rec_at + BITMAP_RECT_LEN].copy_from_slice(pod::as_bytes(&rec));
        self.filled += 1;
        let (start, at) = (self.at, end);
        self.at = at;
        Ok(&mut self.out[start..end])
    }

    /// Write the header and return the payload length. Every rect must have been recorded.
    pub fn finish(self) -> Result<usize, BitmapError> {
        if self.filled != self.n_rects {
            return Err(BitmapError::Rects);
        }
        let header = BitmapHeader {
            magic: BITMAP_MAGIC,
            version: BITMAP_VERSION,
            header_len: BITMAP_HEADER_LEN as u16,
            total_len: self.at as u32,
            video_width: self.video.0,
            video_height: self.video.1,
            n_rects: self.n_rects,
        };
        self.out[..BITMAP_HEADER_LEN].copy_from_slice(pod::as_bytes(&header));
        Ok(self.at)
    }
}

/// A validated, borrowed `subtitle/bitmap` payload. [`BitmapRef::parse`] checks the header, the
/// rect table and every rect's pixel range once, so the accessors cannot fail.
#[derive(Clone, Copy)]
pub struct BitmapRef<'a> {
    h: &'a BitmapHeader,
    rects: &'a [BitmapRect],
    bytes: &'a [u8],
}

impl<'a> BitmapRef<'a> {
    pub fn parse(p: &'a [u8]) -> Result<BitmapRef<'a>, BitmapError> {
        if p.len() < BITMAP_HEADER_LEN {
            return Err(BitmapError::Short { have: p.len() });
        }
        if !p.as_ptr().addr().is_multiple_of(BITMAP_ALIGN) {
            return Err(BitmapError::Misaligned);
        }
        if p[..4] != BITMAP_MAGIC {
            return Err(BitmapError::Magic);
        }
        let h: &BitmapHeader = pod::cast_ref(p).ok_or(BitmapError::Misaligned)?;
        if h.version != BITMAP_VERSION {
            return Err(BitmapError::Version(h.version));
        }
        if h.header_len as usize != BITMAP_HEADER_LEN {
            return Err(BitmapError::Rects);
        }
        if h.total_len as usize != p.len() {
            return Err(BitmapError::Length { stated: h.total_len, have: p.len() });
        }
        let table = (h.n_rects as usize).checked_mul(BITMAP_RECT_LEN).ok_or(BitmapError::Rects)?;
        let table_end = BITMAP_HEADER_LEN.checked_add(table).ok_or(BitmapError::Rects)?;
        if table_end > p.len() {
            return Err(BitmapError::Rects);
        }
        let rects: &[BitmapRect] =
            pod::cast_slice(&p[BITMAP_HEADER_LEN..table_end]).ok_or(BitmapError::Rects)?;
        // Every rect's pixels lie in the region behind the table, which they fill exactly.
        let mut pixels = 0usize;
        for r in rects {
            let want = rgba_len(r.width, r.height).ok_or(BitmapError::Pixels)?;
            let end = r.rgba_off.checked_add(r.rgba_len).ok_or(BitmapError::Pixels)?;
            if r.rgba_len != want || (r.rgba_off as usize) < table_end || end as usize > p.len() {
                return Err(BitmapError::Pixels);
            }
            pixels = pixels.checked_add(want as usize).ok_or(BitmapError::Pixels)?;
        }
        if table_end + pixels != p.len() {
            return Err(BitmapError::Length { stated: h.total_len, have: p.len() });
        }
        Ok(BitmapRef { h, rects, bytes: p })
    }

    pub fn header(&self) -> &'a BitmapHeader {
        self.h
    }
    pub fn video_width(&self) -> u32 {
        self.h.video_width
    }
    pub fn video_height(&self) -> u32 {
        self.h.video_height
    }
    pub fn rects(&self) -> &'a [BitmapRect] {
        self.rects
    }
    /// A clear, nothing to draw.
    pub fn is_clear(&self) -> bool {
        self.rects.is_empty()
    }
    /// The straight-alpha RGBA of `r`, row-major, `width * height * 4` bytes.
    pub fn rgba(&self, r: &BitmapRect) -> &'a [u8] {
        &self.bytes[r.rgba_off as usize..][..r.rgba_len as usize]
    }
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn one(w: u32, h: u32, at: (u32, u32), video: (u32, u32)) -> Vec<u8> {
        let mut v = vec![0u8; payload_len(&[(w, h)]).expect("small")];
        let mut b = BitmapBuilder::new(&mut v, video.0, video.1, 1).expect("fits");
        let px = b.rect(at.0, at.1, w, h).expect("a rect");
        for (i, p) in px.iter_mut().enumerate() {
            *p = (i * 37 % 256) as u8;
        }
        let n = b.finish().expect("complete");
        assert_eq!(n, v.len());
        v
    }

    #[test]
    fn round_trip_one_rect_and_a_clear() {
        let v = one(3, 2, (10, 20), (64, 48));
        let r = BitmapRef::parse(&v).expect("valid");
        assert_eq!((r.video_width(), r.video_height()), (64, 48));
        assert_eq!(r.rects().len(), 1);
        let rect = r.rects()[0];
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (10, 20, 3, 2));
        assert_eq!(r.rgba(&rect).len(), 24);
        assert_eq!(r.rgba(&rect)[1], 37);
        assert!(!r.is_clear());

        let mut v = vec![0u8; BITMAP_HEADER_LEN];
        let n = BitmapBuilder::new(&mut v, 1920, 1080, 0).expect("fits").finish().expect("done");
        assert_eq!(n, BITMAP_HEADER_LEN);
        let r = BitmapRef::parse(&v).expect("valid");
        assert!(r.is_clear());
        assert_eq!(r.video_width(), 1920);
    }

    #[test]
    fn two_rects_lie_back_to_back() {
        let mut v = vec![0u8; payload_len(&[(2, 2), (1, 1)]).expect("small")];
        let mut b = BitmapBuilder::new(&mut v, 0, 0, 2).expect("fits");
        b.rect(0, 0, 2, 2).expect("first").fill(1);
        b.rect(5, 5, 1, 1).expect("second").fill(2);
        let n = b.finish().expect("done");
        let r = BitmapRef::parse(&v[..n]).expect("valid");
        let [a, c] = r.rects() else { panic!("two rects") };
        assert_eq!(a.rgba_off as usize, BITMAP_HEADER_LEN + 2 * BITMAP_RECT_LEN);
        assert_eq!(c.rgba_off, a.rgba_off + 16);
        assert!(r.rgba(a).iter().all(|&p| p == 1));
        assert_eq!(r.rgba(c), &[2, 2, 2, 2]);
    }

    #[test]
    fn the_builder_refuses_what_does_not_fit_or_was_not_promised() {
        let mut v = vec![0u8; BITMAP_HEADER_LEN + BITMAP_RECT_LEN + 8];
        let mut b = BitmapBuilder::new(&mut v, 0, 0, 1).expect("fits");
        assert!(matches!(b.rect(0, 0, 2, 2), Err(BitmapError::DoesNotFit { .. })));
        assert!(matches!(b.rect(0, 0, u32::MAX, 2), Err(BitmapError::Pixels)));
        assert!(matches!(b.finish(), Err(BitmapError::Rects)), "a promised rect is missing");
        let mut v = vec![0u8; BITMAP_HEADER_LEN + 4];
        let mut b = BitmapBuilder::new(&mut v, 0, 0, 0).expect("fits");
        assert!(matches!(b.rect(0, 0, 1, 1), Err(BitmapError::Rects)));
        let mut short = [0u8; 8];
        assert!(matches!(
            BitmapBuilder::new(&mut short, 0, 0, 0),
            Err(BitmapError::DoesNotFit { capacity: 8 })
        ));
    }

    #[test]
    fn every_refusal_is_typed_and_none_panics() {
        let good = one(4, 3, (1, 2), (16, 9));
        assert!(matches!(BitmapRef::parse(&good[..10]), Err(BitmapError::Short { have: 10 })));
        for cut in 0..good.len() {
            assert!(BitmapRef::parse(&good[..cut]).is_err(), "cut at {cut}");
        }
        let mut shifted = vec![0u8; good.len() + 1];
        shifted[1..].copy_from_slice(&good);
        assert_eq!(BitmapRef::parse(&shifted[1..]).err(), Some(BitmapError::Misaligned));
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(BitmapRef::parse(&bad).err(), Some(BitmapError::Magic));
        let mut bad = good.clone();
        bad[4] = 9;
        assert_eq!(BitmapRef::parse(&bad).err(), Some(BitmapError::Version(9)));
        let mut bad = good.clone();
        bad[8] = 0;
        assert!(matches!(BitmapRef::parse(&bad), Err(BitmapError::Length { .. })));
        let mut bad = good.clone();
        bad[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(BitmapRef::parse(&bad).err(), Some(BitmapError::Rects));
        let mut bad = good.clone();
        bad[BITMAP_HEADER_LEN + 8..BITMAP_HEADER_LEN + 12].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(BitmapRef::parse(&bad).err(), Some(BitmapError::Pixels), "width without pixels");
        let mut bad = good.clone();
        bad[BITMAP_HEADER_LEN + 16..BITMAP_HEADER_LEN + 20].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            BitmapRef::parse(&bad).err(),
            Some(BitmapError::Pixels),
            "pixels over the table"
        );
    }

    #[test]
    fn random_bytes_never_panic_the_validator() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let good = one(2, 2, (0, 0), (4, 4));
        let (mut ok, mut bad) = (0, 0);
        for _ in 0..20_000 {
            let mut v = good.clone();
            for _ in 0..1 + next() % 4 {
                let at = next() as usize % v.len();
                v[at] ^= 1 << (next() % 8);
            }
            match BitmapRef::parse(&v) {
                Ok(r) => {
                    ok += 1;
                    for rect in r.rects() {
                        std::hint::black_box(r.rgba(rect));
                    }
                }
                Err(_) => bad += 1,
            }
        }
        assert!(ok > 0 && bad > 0, "{ok} accepted, {bad} refused");
        let mut noise = vec![0u8; 256];
        for _ in 0..2_000 {
            for b in &mut noise {
                *b = next() as u8;
            }
            noise[..4].copy_from_slice(&BITMAP_MAGIC);
            let len = 4 + next() as usize % 252;
            let _ = BitmapRef::parse(&noise[..len]);
        }
    }
}
