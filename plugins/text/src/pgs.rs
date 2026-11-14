//! PGS (HDMV Presentation Graphics Stream) decode, the image-based Blu-ray subtitle format
//! (RFC 9559 section 12.7, Matroska `S_HDMV/PGS`). [`parse_display_set_in`] reads a set into
//! reused scratch and [`Picture::render`] paints it straight into a `subtitle/bitmap` slot.

use crate::bitmap::{BitmapBuilder, BitmapRef, payload_len};

/// A decoded PGS Display Set on the heap, tests, tools and the examples. The element path uses
/// [`Picture`] and renders into the pool slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplaySet {
    /// Straight-alpha RGBA8888, row-major, `width*height*4` bytes.
    pub rgba: Vec<u8>,
    /// Object bitmap dimensions in pixels.
    pub width: u32,
    pub height: u32,
    /// Top-left placement of the object in the reference video frame (from the PCS composition
    /// object).
    pub x: u32,
    pub y: u32,
    /// The composition's reference video size (PCS width/height).
    pub video_width: u32,
    pub video_height: u32,
}

// Cold, the owned picture is a Vec by definition.
#[allow(clippy::disallowed_methods)]
impl DisplaySet {
    /// A clear Display Set (a composition with no objects).
    pub fn clear(video_width: u32, video_height: u32) -> Self {
        DisplaySet { rgba: Vec::new(), width: 0, height: 0, x: 0, y: 0, video_width, video_height }
    }

    /// True when this Display Set clears the current caption (no bitmap).
    pub fn is_clear(&self) -> bool {
        self.rgba.is_empty()
    }
}

/// A [`DisplaySet`] as a `subtitle/bitmap` payload on the heap, one rect or none. Tests and tools.
#[allow(clippy::disallowed_methods)]
pub fn encode_bitmap(ds: &DisplaySet) -> Vec<u8> {
    let sizes: &[(u32, u32)] = if ds.is_clear() { &[] } else { &[(ds.width, ds.height)] };
    let mut out = vec![0u8; payload_len(sizes).unwrap_or(0)];
    let Ok(mut b) =
        BitmapBuilder::new(&mut out, ds.video_width, ds.video_height, sizes.len() as u32)
    else {
        return Vec::new();
    };
    if !ds.is_clear() {
        match b.rect(ds.x, ds.y, ds.width, ds.height) {
            Ok(px) if px.len() == ds.rgba.len() => px.copy_from_slice(&ds.rgba),
            _ => return Vec::new(),
        }
    }
    let n = b.finish().unwrap_or(0);
    out.truncate(n);
    out
}

/// A `subtitle/bitmap` payload back to a [`DisplaySet`], its first rect. Tests and tools.
#[allow(clippy::disallowed_methods)]
pub fn decode_bitmap(payload: &[u8]) -> Option<DisplaySet> {
    let r = BitmapRef::parse(payload).ok()?;
    let (video_width, video_height) = (r.video_width(), r.video_height());
    Some(match r.rects().first() {
        None => DisplaySet::clear(video_width, video_height),
        Some(rect) => DisplaySet {
            rgba: r.rgba(rect).to_vec(),
            width: rect.width,
            height: rect.height,
            x: rect.x,
            y: rect.y,
            video_width,
            video_height,
        },
    })
}

/// Every rect of a `subtitle/bitmap` payload as its own [`DisplaySet`], none for a clear. Tests
/// and tools.
#[allow(clippy::disallowed_methods)]
pub fn decode_rects(payload: &[u8]) -> Option<Vec<DisplaySet>> {
    let r = BitmapRef::parse(payload).ok()?;
    let (video_width, video_height) = (r.video_width(), r.video_height());
    let one = |rect: &crate::bitmap::BitmapRect| DisplaySet {
        rgba: r.rgba(rect).to_vec(),
        width: rect.width,
        height: rect.height,
        x: rect.x,
        y: rect.y,
        video_width,
        video_height,
    };
    Some(r.rects().iter().map(one).collect())
}

/// A parse error in a PGS Display Set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PgsError {
    Truncated(&'static str),
    Malformed(&'static str),
    NoComposition,
}

impl PgsError {
    /// A `&'static` one-line reason, for a `log!` field (which takes `&'static str`, not an owned
    /// `String`).
    pub fn reason(&self) -> &'static str {
        match self {
            PgsError::Truncated(what) => what,
            PgsError::Malformed(what) => what,
            PgsError::NoComposition => "display set has no PCS",
        }
    }
}

const SEG_PDS: u8 = 0x14;
const SEG_ODS: u8 = 0x15;
const SEG_PCS: u8 = 0x16;
const SEG_WDS: u8 = 0x17;
const SEG_END: u8 = 0x80;

/// The longest run the HDMV scheme codes. Its 3-byte transparent form is the densest coding.
const MAX_RUN: usize = 16_383;
/// The largest object a disc format codes, UHD Blu-ray.
const MAX_OBJECT_W: usize = 3840;
const MAX_OBJECT_H: usize = 2160;

/// Objects an epoch holds, ffmpeg's bound.
const MAX_OBJECTS: usize = 64;
/// Palettes an epoch holds.
const MAX_PALETTES: usize = 8;
/// Composition objects a set draws, one per window.
const MAX_COMPOSED: usize = 2;
/// Coded object bytes an epoch holds at once, two full UHD objects of literal pixels.
pub const MAX_HELD: usize = 16 << 20;
/// A cleared object buffer past this goes back to the allocator, so slots that once held a large
/// object do not pin it.
const KEEP_CAPACITY: usize = 256 << 10;

const SUP_MAGIC: [u8; 2] = [0x50, 0x47];

/// One PDS's entries by palette id, Y, Cr, Cb and alpha as coded.
#[derive(Clone, Copy)]
struct Palette {
    id: u8,
    entries: [[u8; 4]; 256],
}

/// An index no PDS defined stays transparent, the safe default.
const UNSET_PALETTE: Palette = Palette { id: 0, entries: [[0, 128, 128, 0]; 256] };

/// Limited-range Y'CrCb to full-range RGB. BT.709 (sections 3.2 and 3.3) unless the video is
/// 576 lines or less, then BT.601, as ffmpeg picks.
#[inline]
fn ycrcb_to_rgb(y: u8, cr: u8, cb: u8, bt601: bool) -> [u8; 3] {
    let yf = (y as f32 - 16.0) * 1.164_383_6;
    let crf = cr as f32 - 128.0;
    let cbf = cb as f32 - 128.0;
    let (rv, gu, gv, bu) = if bt601 {
        (1.596_026_8, 0.391_762_3, 0.812_967_6, 2.017_232_1)
    } else {
        (1.792_741_1, 0.213_248_6, 0.532_909_3, 2.112_401_7)
    };
    [clamp_u8(yf + rv * crf), clamp_u8(yf - gu * cbf - gv * crf), clamp_u8(yf + bu * cbf)]
}

#[inline]
fn clamp_u8(v: f32) -> u8 {
    if v <= 0.0 {
        0
    } else if v >= 255.0 {
        255
    } else {
        (v + 0.5) as u8
    }
}

/// One composition object, its object cropped to `crop` (x, y, width, height) when it is.
#[derive(Clone, Copy, Default)]
struct Placed {
    object_id: u16,
    x: u32,
    y: u32,
    crop: Option<[u32; 4]>,
}

/// A PCS, its size, epoch state, palette and the objects it draws.
#[derive(Clone, Copy)]
struct Composition {
    video_width: u32,
    video_height: u32,
    state: u8,
    palette_id: u8,
    n: usize,
    placed: [Placed; MAX_COMPOSED],
}

fn parse_pcs(payload: &[u8]) -> Result<Composition, PgsError> {
    let mut r = Reader::new(payload);
    let video_width = r.u16("PCS width")? as u32;
    let video_height = r.u16("PCS height")? as u32;
    r.skip(1, "PCS frame_rate")?;
    r.skip(2, "PCS composition_number")?;
    let state = r.u8("PCS composition_state")? >> 6;
    r.skip(1, "PCS palette_update_flag")?;
    let palette_id = r.u8("PCS palette_id")?;
    let num_objects = r.u8("PCS number_of_composition_objects")?;
    let mut c = Composition {
        video_width,
        video_height,
        state,
        palette_id,
        n: 0,
        placed: [Placed::default(); MAX_COMPOSED],
    };
    for _ in 0..num_objects {
        let object_id = r.u16("PCS object_id")?;
        r.skip(1, "PCS window_id")?;
        // Bit 7 is object_cropped_flag, bit 6 forced_on_flag.
        let flags = r.u8("PCS object_cropped_flag")?;
        let x = r.u16("PCS x")? as u32;
        let y = r.u16("PCS y")? as u32;
        let crop = if flags & 0x80 != 0 {
            let mut v = [0u32; 4];
            for slot in &mut v {
                *slot = r.u16("PCS crop rectangle")? as u32;
            }
            Some(v)
        } else {
            None
        };
        // Past the two windows' objects the rest are framing only, as ffmpeg keeps two.
        if c.n < MAX_COMPOSED {
            c.placed[c.n] = Placed { object_id, x, y, crop };
            c.n += 1;
        }
    }
    Ok(c)
}

/// Whether a run-length plane was walked for its size yet.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Cover {
    #[default]
    Unknown,
    Full,
    Short,
}

/// One object's fragments reassembled. Lives in [`PgsState`], its buffer reused across sets.
#[derive(Default)]
struct Object {
    id: u16,
    width: u32,
    height: u32,
    rle: Vec<u8>,
    cover: Cover,
}

/// What a decoder keeps across Display Sets, the epoch's objects and palettes, which a set that
/// only updates a palette or moves an object draws again. An epoch start, acquisition point or
/// epoch continue drops them, as ffmpeg does. A warm decoder allocates nothing.
pub struct PgsState {
    objects: Vec<Object>,
    /// The epoch's objects, the first `used` entries of `objects`.
    used: usize,
    palettes: [Palette; MAX_PALETTES],
    n_palettes: usize,
    /// Coded bytes the epoch's objects hold.
    held: usize,
}

impl Default for PgsState {
    fn default() -> Self {
        Self::new()
    }
}

// The object table grows to the epoch's largest and is reused.
#[allow(clippy::disallowed_methods)]
impl PgsState {
    pub fn new() -> PgsState {
        PgsState {
            objects: Vec::new(),
            used: 0,
            palettes: [UNSET_PALETTE; MAX_PALETTES],
            n_palettes: 0,
            held: 0,
        }
    }

    /// Forget the epoch, a new one starts or the stream was flushed.
    pub fn reset(&mut self) {
        for o in &mut self.objects[..self.used] {
            clear_rle(&mut o.rle);
        }
        (self.used, self.n_palettes, self.held) = (0, 0, 0);
    }

    fn apply_pds(&mut self, payload: &[u8]) -> Result<(), PgsError> {
        if payload.len() < 2 {
            return Err(PgsError::Truncated("PDS header"));
        }
        let body = &payload[2..];
        if !body.len().is_multiple_of(5) {
            return Err(PgsError::Malformed("PDS entry array not a multiple of 5"));
        }
        let id = payload[0];
        let at = match self.palettes[..self.n_palettes].iter().position(|p| p.id == id) {
            Some(at) => at,
            None if self.n_palettes >= MAX_PALETTES => {
                return Err(PgsError::Malformed("epoch defines too many palettes"));
            }
            None => {
                self.palettes[self.n_palettes] = Palette { id, ..UNSET_PALETTE };
                self.n_palettes += 1;
                self.n_palettes - 1
            }
        };
        let p = &mut self.palettes[at];
        for e in body.as_chunks::<5>().0 {
            p.entries[e[0] as usize] = [e[1], e[2], e[3], e[4]];
        }
        Ok(())
    }

    fn apply_ods(&mut self, payload: &[u8]) -> Result<(), PgsError> {
        let mut r = Reader::new(payload);
        let object_id = r.u16("ODS object_id")?;
        r.skip(1, "ODS object_version")?;
        let seq = r.u8("ODS sequence_flag")?;
        // Validated by the walk of the plane, so data_length is skipped.
        r.skip(3, "ODS object_data_length")?;
        let at = self.objects[..self.used].iter().position(|o| o.id == object_id);
        if seq & 0x80 == 0 {
            let at = at.ok_or(PgsError::Malformed("ODS continuation without a first fragment"))?;
            return self.append(at, r.rest());
        }
        let width = r.u16("ODS width")? as u32;
        let height = r.u16("ODS height")? as u32;
        let at = match at {
            Some(at) => at,
            None if self.used >= MAX_OBJECTS => {
                return Err(PgsError::Malformed("display set defines too many objects"));
            }
            None => {
                if self.objects.len() == self.used {
                    self.objects.push(Object::default());
                }
                self.used += 1;
                self.used - 1
            }
        };
        let o = &mut self.objects[at];
        self.held -= o.rle.len();
        clear_rle(&mut o.rle);
        (o.id, o.width, o.height) = (object_id, width, height);
        self.append(at, r.rest())
    }

    fn append(&mut self, at: usize, more: &[u8]) -> Result<(), PgsError> {
        if self.held + more.len() > MAX_HELD {
            return Err(PgsError::Malformed("objects past the epoch's coded byte budget"));
        }
        let o = &mut self.objects[at];
        o.rle.extend_from_slice(more);
        o.cover = Cover::Unknown;
        self.held += more.len();
        Ok(())
    }

    /// The composition over the epoch's objects, checked before anything is sized or painted.
    fn picture(&mut self, comp: &Composition) -> Result<Picture<'_>, PgsError> {
        let mut drawn = [Drawn::default(); MAX_COMPOSED];
        let mut n = 0;
        for p in &comp.placed[..comp.n] {
            // One with no ODS data draws nothing, as ffmpeg leaves its rect empty.
            let Some(at) = self.objects[..self.used].iter().position(|o| o.id == p.object_id)
            else {
                continue;
            };
            let o = &mut self.objects[at];
            let (w, h) = (o.width as usize, o.height as usize);
            if w == 0 || h == 0 {
                continue;
            }
            check_size(o, comp)?;
            let [cx, cy, cw, ch] = p.crop.unwrap_or([0, 0, o.width, o.height]);
            // The crop is in the object's own coordinates, its top left goes to (x, y).
            if cx >= o.width || cy >= o.height {
                continue;
            }
            let (cw, ch) = (cw.min(o.width - cx), ch.min(o.height - cy));
            if cw == 0 || ch == 0 {
                continue;
            }
            drawn[n] = Drawn { object: at, x: p.x, y: p.y, crop: [cx, cy, cw, ch] };
            n += 1;
        }
        let mut lut = [[0u8; 4]; 256];
        if n > 0 {
            let palette =
                self.palettes[..self.n_palettes].iter().find(|p| p.id == comp.palette_id).ok_or(
                    PgsError::Malformed("composition names a palette the epoch never defined"),
                )?;
            let bt601 = comp.video_height > 0 && comp.video_height <= 576;
            for (px, &[y, cr, cb, a]) in lut.iter_mut().zip(&palette.entries) {
                let [r, g, b] = ycrcb_to_rgb(y, cr, cb, bt601);
                *px = [r, g, b, a];
            }
        }
        Ok(Picture {
            video_width: comp.video_width,
            video_height: comp.video_height,
            lut,
            n,
            drawn,
            objects: &self.objects[..self.used],
        })
    }
}

/// Empties an object buffer, freeing it when it grew past what a caption needs.
#[allow(clippy::disallowed_methods)]
fn clear_rle(rle: &mut Vec<u8>) {
    if rle.capacity() > KEEP_CAPACITY {
        *rle = Vec::new();
    } else {
        rle.clear();
    }
}

/// Refuse an overflowing, ballooning or under-coded object before a slot is sized.
fn check_size(o: &mut Object, comp: &Composition) -> Result<(), PgsError> {
    let (w, h) = (o.width as usize, o.height as usize);
    let pixels = w.checked_mul(h).ok_or(PgsError::Malformed("object size overflow"))?;
    if pixels > 8192 * 8192 {
        return Err(PgsError::Malformed("object size implausibly large"));
    }
    // Every three coded bytes stand for at most `MAX_RUN` pixels, the cheap bound first.
    let short = PgsError::Malformed("object size exceeds what its coded data can hold");
    if pixels > MAX_RUN.saturating_mul(o.rle.len().div_ceil(3)) {
        return Err(short);
    }
    // An object never exceeds its video, and no disc format exceeds UHD.
    if w > MAX_OBJECT_W || h > MAX_OBJECT_H {
        return Err(PgsError::Malformed("object larger than any supported video"));
    }
    let video = comp.video_width > 0 && comp.video_height > 0;
    if video && (w > comp.video_width as usize || h > comp.video_height as usize) {
        return Err(PgsError::Malformed("object larger than its video"));
    }
    // Its runs must reach every pixel of every line, as ffmpeg asks, so a few end of line codes
    // cannot stand for a screen of pixels.
    if o.cover == Cover::Unknown {
        o.cover = if covered(&o.rle, w, h)? >= pixels { Cover::Full } else { Cover::Short };
    }
    if o.cover == Cover::Short {
        return Err(short);
    }
    Ok(())
}

/// One drawn object, the index of its object and the crop of it shown at (`x`, `y`).
#[derive(Clone, Copy, Default)]
struct Drawn {
    object: usize,
    x: u32,
    y: u32,
    crop: [u32; 4],
}

/// A parsed Display Set over the state it was read into, ready to render. One rect per drawn
/// object, none for a clear.
pub struct Picture<'a> {
    /// The composition's reference video size.
    pub video_width: u32,
    pub video_height: u32,
    lut: [[u8; 4]; 256],
    n: usize,
    drawn: [Drawn; MAX_COMPOSED],
    objects: &'a [Object],
}

impl Picture<'_> {
    /// A composition with nothing to draw.
    pub fn is_clear(&self) -> bool {
        self.n == 0
    }

    /// The rects drawn, (x, y, width, height) each.
    pub fn rects(&self) -> impl Iterator<Item = (u32, u32, u32, u32)> + '_ {
        self.drawn[..self.n].iter().map(|d| (d.x, d.y, d.crop[2], d.crop[3]))
    }

    /// The `subtitle/bitmap` payload length this picture renders to.
    pub fn payload_len(&self) -> Option<usize> {
        let mut sizes = [(0u32, 0u32); MAX_COMPOSED];
        for (s, d) in sizes.iter_mut().zip(&self.drawn[..self.n]) {
            *s = (d.crop[2], d.crop[3]);
        }
        payload_len(&sizes[..self.n])
    }

    /// Write the picture as a `subtitle/bitmap` payload into `out`, the payload length back.
    pub fn write_bitmap(&self, out: &mut [u8]) -> Result<usize, PgsError> {
        let slot = PgsError::Malformed("bitmap slot too small");
        let mut b = BitmapBuilder::new(out, self.video_width, self.video_height, self.n as u32)
            .map_err(|_| slot.clone())?;
        for d in &self.drawn[..self.n] {
            let px = b.rect(d.x, d.y, d.crop[2], d.crop[3]).map_err(|_| slot.clone())?;
            self.render(d, px)?;
        }
        b.finish().map_err(|_| slot)
    }

    /// Decode one object's run lengths straight into `rgba`, its crop's `w * h * 4` bytes.
    fn render(&self, d: &Drawn, rgba: &mut [u8]) -> Result<(), PgsError> {
        let o = &self.objects[d.object];
        // A pixel no run reaches shows palette entry 0, as the index plane did.
        let zero = self.lut[0];
        for px in rgba.as_chunks_mut::<4>().0 {
            *px = zero;
        }
        decode_rle(&o.rle, o.width as usize, d.crop, &self.lut, rgba)
    }

    /// The first drawn object on the heap, a clear when none. Tools and tests, the payload
    /// carries every object.
    #[allow(clippy::disallowed_methods)]
    pub fn to_display_set(&self) -> Result<DisplaySet, PgsError> {
        let Some(d) = self.drawn[..self.n].first() else {
            return Ok(DisplaySet::clear(self.video_width, self.video_height));
        };
        let [_, _, w, h] = d.crop;
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        self.render(d, &mut rgba)?;
        Ok(DisplaySet {
            rgba,
            width: w,
            height: h,
            x: d.x,
            y: d.y,
            video_width: self.video_width,
            video_height: self.video_height,
        })
    }
}

/// Parse one **Display Set** (a run of bare segments, no `.sup` `PG` headers) on its own into a
/// [`DisplaySet`] on the heap, its first drawn object. Tests, tools and the examples.
#[allow(clippy::disallowed_methods)]
pub fn parse_display_set(data: &[u8]) -> Result<DisplaySet, PgsError> {
    let mut state = PgsState::new();
    parse_display_set_in(data, &mut state)?.to_display_set()
}

/// Parse one Display Set into `state`, which carries the epoch's objects and palettes from the
/// sets before it, and hand back the picture to render. Each drawn object's size is checked
/// against what its coded data holds before anything is painted.
pub fn parse_display_set_in<'s>(
    data: &[u8],
    state: &'s mut PgsState,
) -> Result<Picture<'s>, PgsError> {
    let mut composition: Option<Composition> = None;
    let mut at = 0usize;
    while at < data.len() {
        let seg_type = *data.get(at).ok_or(PgsError::Truncated("segment type"))?;
        let size = u16::from_be_bytes([
            *data.get(at + 1).ok_or(PgsError::Truncated("segment size hi"))?,
            *data.get(at + 2).ok_or(PgsError::Truncated("segment size lo"))?,
        ]) as usize;
        let body_start = at + 3;
        let body_end =
            body_start.checked_add(size).ok_or(PgsError::Malformed("segment size overflow"))?;
        let payload =
            data.get(body_start..body_end).ok_or(PgsError::Truncated("segment payload"))?;
        at = body_end;

        match seg_type {
            SEG_PCS => {
                let c = parse_pcs(payload)?;
                // Any state but normal case releases the epoch's objects and palettes.
                if c.state != 0 {
                    state.reset();
                }
                composition = Some(c);
            }
            SEG_WDS => { /* window rectangles, placement is from the PCS */ }
            SEG_PDS => state.apply_pds(payload)?,
            SEG_ODS => state.apply_ods(payload)?,
            SEG_END => break,
            // An unknown segment type is skipped, its length framed it.
            _ => {}
        }
    }
    let comp = composition.ok_or(PgsError::NoComposition)?;
    state.picture(&comp)
}

/// One HDMV run, its pixel count and colour, or `None` for the end of a line.
#[inline]
fn next_run(rle: &[u8], i: &mut usize) -> Result<Option<(usize, u8)>, PgsError> {
    let b0 = rle[*i];
    *i += 1;
    if b0 != 0 {
        // A single pixel of colour b0.
        return Ok(Some((1, b0)));
    }
    let mut byte = |what| {
        let b = *rle.get(*i).ok_or(PgsError::Truncated(what))?;
        *i += 1;
        Ok::<u8, PgsError>(b)
    };
    let b1 = byte("RLE run byte")?;
    if b1 == 0 {
        return Ok(None);
    }
    let low = (b1 & 0x3F) as usize;
    Ok(Some(match b1 >> 6 {
        // 1..63 pixels of colour 0.
        0b00 => (low, 0),
        // 64..16383 pixels of colour 0, length low 6 bits and the next byte.
        0b01 => ((low << 8) | byte("RLE long-run length")? as usize, 0),
        // 3..63 pixels of the colour in the next byte.
        0b10 => (low, byte("RLE short-colour byte")?),
        // 64..16383 pixels of a colour, length and colour in two more bytes.
        _ => {
            let lo = byte("RLE long-run length")? as usize;
            ((low << 8) | lo, byte("RLE long-colour byte")?)
        }
    }))
}

/// Pixels of a `width` x `height` plane the runs reach, each line's runs counted to its width.
fn covered(rle: &[u8], width: usize, height: usize) -> Result<usize, PgsError> {
    let (mut i, mut row, mut line, mut total) = (0usize, 0usize, 0usize, 0usize);
    while i < rle.len() && row < height {
        match next_run(rle, &mut i)? {
            Some((count, _)) => line = line.saturating_add(count),
            None => {
                total += line.min(width);
                (row, line) = (row + 1, 0);
            }
        }
    }
    if row < height {
        total += line.min(width);
    }
    Ok(total)
}

/// Decode the HDMV run lengths of an object `width` pixels wide straight into RGBA, the `crop`
/// (x, y, width, height) of it only.
fn decode_rle(
    rle: &[u8],
    width: usize,
    crop: [u32; 4],
    lut: &[[u8; 4]; 256],
    out: &mut [u8],
) -> Result<(), PgsError> {
    let [cx, cy, cw, ch] = crop.map(|v| v as usize);
    let (mut i, mut row, mut col) = (0usize, 0usize, 0usize);
    // Rows advance only on the `00 00` end-of-line marker, so a run that fills a row and its EOL
    // do not advance twice.
    while i < rle.len() && row < cy + ch {
        let Some((count, colour)) = next_run(rle, &mut i)? else {
            (row, col) = (row + 1, 0);
            continue;
        };
        // The part of the run inside the line and the crop, a full row waits for its EOL.
        let end = col.saturating_add(count).min(width);
        let (from, to) = (col.max(cx), end.min(cx + cw));
        if row >= cy && from < to {
            let at = (row - cy) * cw + (from - cx);
            for p in out[at * 4..(at + to - from) * 4].as_chunks_mut::<4>().0 {
                *p = lut[usize::from(colour)];
            }
        }
        col = end;
    }
    Ok(())
}

/// Peel the raw `.sup` framing off a whole SUP file into the concatenated bare segments of each
/// Display Set, grouped by the segments' presentation timestamp. A tool's whole-file pass.
#[allow(clippy::disallowed_methods)]
pub fn split_sup(mut data: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
    let mut sets = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_pts = 0u32;
    let mut cur_dts = 0u32;
    let mut have_pcs = false;
    while data.len() >= 13 {
        if data[..2] != SUP_MAGIC {
            break; // lost sync, keep what we have
        }
        let pts = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
        let dts = u32::from_be_bytes([data[6], data[7], data[8], data[9]]);
        let seg_type = data[10];
        let size = u16::from_be_bytes([data[11], data[12]]) as usize;
        let record_end = 13 + size;
        if data.len() < record_end {
            break; // truncated tail
        }
        let seg = &data[10..record_end]; // bare segment: type, size, payload
        if seg_type == SEG_PCS {
            cur.clear();
            cur_pts = pts;
            cur_dts = dts;
            have_pcs = true;
        }
        if have_pcs {
            cur.extend_from_slice(seg);
        }
        if seg_type == SEG_END && have_pcs {
            sets.push((cur_pts, cur_dts, std::mem::take(&mut cur)));
            have_pcs = false;
        }
        data = &data[record_end..];
    }
    sets
}

/// Strip a single `.sup` record's 10-byte `PG`+timestamps header, returning the bare segment bytes
/// and the 90 kHz PTS.
pub fn strip_sup_header(record: &[u8]) -> Option<(u32, &[u8])> {
    if record.len() < 13 || record[..2] != SUP_MAGIC {
        return None;
    }
    let pts = u32::from_be_bytes([record[2], record[3], record[4], record[5]]);
    let size = u16::from_be_bytes([record[11], record[12]]) as usize;
    let end = 13 + size;
    let seg = record.get(10..end)?;
    Some((pts, seg))
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, PgsError> {
        let b = *self.data.get(self.at).ok_or(PgsError::Truncated(what))?;
        self.at += 1;
        Ok(b)
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, PgsError> {
        let hi = self.u8(what)? as u16;
        let lo = self.u8(what)? as u16;
        Ok((hi << 8) | lo)
    }

    fn skip(&mut self, n: usize, what: &'static str) -> Result<(), PgsError> {
        let end = self.at.checked_add(n).ok_or(PgsError::Malformed(what))?;
        if end > self.data.len() {
            return Err(PgsError::Truncated(what));
        }
        self.at = end;
        Ok(())
    }

    fn rest(&self) -> &'a [u8] {
        &self.data[self.at.min(self.data.len())..]
    }
}

/// Hand-built PGS segments, for the tests here, in `tests/` and the fuzz targets.
#[doc(hidden)]
#[allow(clippy::disallowed_methods)] // test vectors, heap-built on purpose
pub mod fixtures {
    use super::{SEG_END, SEG_ODS, SEG_PCS, SEG_PDS};

    /// `[type][size][payload]`.
    pub fn segment(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    /// A PCS, the state byte as coded (0x80 epoch start, 0 normal case), and `(object, x, y,
    /// crop)` composition objects.
    pub fn pcs(
        video: (u16, u16),
        state: u8,
        palette_update: u8,
        palette_id: u8,
        objects: &[(u16, u16, u16, Option<[u16; 4]>)],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&video.0.to_be_bytes());
        b.extend_from_slice(&video.1.to_be_bytes());
        b.extend_from_slice(&[0x10, 0, 0, state, palette_update, palette_id, objects.len() as u8]);
        for &(id, x, y, crop) in objects {
            b.extend_from_slice(&id.to_be_bytes());
            b.extend_from_slice(&[0, if crop.is_some() { 0x80 } else { 0 }]);
            b.extend_from_slice(&x.to_be_bytes());
            b.extend_from_slice(&y.to_be_bytes());
            for v in crop.into_iter().flatten() {
                b.extend_from_slice(&v.to_be_bytes());
            }
        }
        segment(SEG_PCS, &b)
    }

    /// A PDS of `(index, Y, Cr, Cb, alpha)` entries.
    pub fn pds(id: u8, entries: &[(u8, u8, u8, u8, u8)]) -> Vec<u8> {
        let mut b = vec![id, 0];
        for e in entries {
            b.extend_from_slice(&[e.0, e.1, e.2, e.3, e.4]);
        }
        segment(SEG_PDS, &b)
    }

    /// An ODS fragment, `seq` 0x80 first, 0x40 last, the size on a first one.
    pub fn ods(id: u16, seq: u8, size: Option<(u16, u16)>, rle: &[u8]) -> Vec<u8> {
        let mut b = id.to_be_bytes().to_vec();
        b.extend_from_slice(&[0, seq]);
        b.extend_from_slice(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
        if let Some((w, h)) = size {
            b.extend_from_slice(&w.to_be_bytes());
            b.extend_from_slice(&h.to_be_bytes());
        }
        b.extend_from_slice(rle);
        segment(SEG_ODS, &b)
    }

    pub fn end() -> Vec<u8> {
        segment(SEG_END, &[])
    }

    /// Rows of palette indices as HDMV runs, each row closed by its end of line.
    pub fn rle_rows(rows: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows {
            let mut i = 0;
            while i < row.len() {
                let c = row[i];
                let n = row[i..].iter().take_while(|&&v| v == c).count().min(16_383);
                match (c, n) {
                    (0, 1..=63) => out.extend([0, n as u8]),
                    (0, _) => out.extend([0, 0x40 | (n >> 8) as u8, n as u8]),
                    (_, 1 | 2) => out.extend(std::iter::repeat_n(c, n)),
                    (_, 3..=63) => out.extend([0, 0x80 | n as u8, c]),
                    _ => out.extend([0, 0xC0 | (n >> 8) as u8, n as u8, c]),
                }
                i += n;
            }
            out.extend([0, 0]);
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::fixtures::*;
    use super::*;

    /// One set through a fresh state, rendered to a payload and read back rect by rect.
    fn rects(set: &[u8]) -> Result<Vec<DisplaySet>, PgsError> {
        rects_in(set, &mut PgsState::new())
    }

    fn rects_in(set: &[u8], state: &mut PgsState) -> Result<Vec<DisplaySet>, PgsError> {
        let pic = parse_display_set_in(set, state)?;
        let mut slot = vec![0u8; pic.payload_len().expect("fits")];
        let n = pic.write_bitmap(&mut slot)?;
        Ok(decode_rects(&slot[..n]).expect("a bitmap"))
    }

    #[test]
    fn two_composition_objects_are_two_rects_with_their_palette_and_crop() {
        let top = rle_rows(&[&[1, 1, 1, 1], &[1, 2, 2, 1]]);
        let bottom = rle_rows(&[&[2, 2, 2]]);
        let set = [
            pcs((1920, 1080), 0x80, 0, 1, &[(0, 100, 50, Some([1, 1, 2, 1])), (1, 100, 900, None)]),
            pds(0, &[(1, 16, 128, 128, 255), (2, 16, 128, 128, 255)]),
            pds(1, &[(1, 235, 128, 128, 255), (2, 126, 128, 128, 200)]),
            ods(0, 0xC0, Some((4, 2)), &top),
            ods(1, 0xC0, Some((3, 1)), &bottom),
            end(),
        ]
        .concat();
        let r = rects(&set).expect("decodes");
        assert_eq!(r.len(), 2, "top and bottom dialogue");
        assert_eq!((r[0].x, r[0].y, r[0].width, r[0].height), (100, 50, 2, 1), "the crop");
        assert_eq!((r[1].x, r[1].y, r[1].width, r[1].height), (100, 900, 3, 1));
        // Row 1 columns 1 and 2 of the first object, index 2, through palette 1.
        assert!(r[0].rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 200 && p[0] == p[1]));
        assert!(r[0].rgba[0] > 100, "palette 1's grey, not palette 0's black");
        assert!(r[1].rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 200));
        assert_eq!(parse_display_set(&set).map(|d| (d.x, d.y)), Ok((100, 50)), "the first object");
    }

    #[test]
    fn a_crop_outside_its_object_draws_nothing() {
        let set = [
            pcs((1920, 1080), 0x80, 0, 0, &[(0, 0, 0, Some([9, 0, 2, 1]))]),
            pds(0, &[(1, 235, 128, 128, 255)]),
            ods(0, 0xC0, Some((4, 1)), &rle_rows(&[&[1; 4]])),
            end(),
        ]
        .concat();
        assert!(rects(&set).expect("decodes").is_empty());
    }

    #[test]
    fn a_composition_naming_an_undefined_palette_is_dropped() {
        let set = [
            pcs((1920, 1080), 0x80, 0, 3, &[(0, 0, 0, None)]),
            pds(0, &[(1, 235, 128, 128, 255)]),
            ods(0, 0xC0, Some((1, 1)), &rle_rows(&[&[1]])),
            end(),
        ]
        .concat();
        assert_eq!(
            rects(&set).map(|r| r.len()),
            Err(PgsError::Malformed("composition names a palette the epoch never defined"))
        );
    }

    #[test]
    fn an_object_of_end_of_lines_only_is_refused_before_a_slot_is_sized() {
        // Enough bytes to pass the run bound, but no run reaches a pixel.
        let need = (3840usize * 2160).div_ceil(MAX_RUN) * 3;
        let set = [
            pcs((3840, 2160), 0x80, 0, 0, &[(0, 0, 0, None)]),
            pds(0, &[(0, 235, 128, 128, 255)]),
            ods(0, 0xC0, Some((3840, 2160)), &vec![0u8; need + need % 2]),
            end(),
        ]
        .concat();
        let mut state = PgsState::new();
        let refused = parse_display_set_in(&set, &mut state).map(|p| p.payload_len());
        assert!(
            matches!(refused, Err(PgsError::Malformed(r)) if r.contains("coded data")),
            "an empty plane sized to UHD"
        );
        // A plane its runs fill passes however short its coding.
        let row = [0u8; 3840];
        let wide = rle_rows(&[&row, &row, &row, &row]);
        let ok = [
            pcs((3840, 2160), 0x80, 0, 0, &[(0, 0, 0, None)]),
            pds(0, &[(0, 235, 128, 128, 255)]),
            ods(0, 0xC0, Some((3840, 4)), &wide),
            end(),
        ]
        .concat();
        let r = rects(&ok).expect("decodes");
        assert_eq!((r[0].width, r[0].height), (3840, 4));
    }

    #[test]
    fn sd_video_reads_its_palette_as_bt601() {
        let set = |video| {
            [
                pcs(video, 0x80, 0, 0, &[(0, 0, 0, None)]),
                pds(0, &[(1, 81, 240, 90, 255)]),
                ods(0, 0xC0, Some((1, 1)), &rle_rows(&[&[1]])),
                end(),
            ]
            .concat()
        };
        let sd = rects(&set((720, 576))).expect("decodes");
        assert!(sd[0].rgba[0] >= 254 && sd[0].rgba[1] == 0, "{:?}", sd[0].rgba);
        let hd = rects(&set((1920, 1080))).expect("decodes");
        assert!(hd[0].rgba[1] > 16, "{:?}", hd[0].rgba);
    }

    #[test]
    fn the_epoch_bounds_the_bytes_its_objects_hold() {
        // A continuation set after set, each 60 kB, never let the object grow without end.
        let mut state = PgsState::new();
        let chunk = vec![1u8; 60_000];
        let first = [pcs((1920, 1080), 0x80, 0, 0, &[]), ods(0, 0x80, Some((1, 1)), &chunk)];
        parse_display_set_in(&first.concat(), &mut state).expect("the first fragment");
        let more = [pcs((1920, 1080), 0, 0, 0, &[]), ods(0, 0, None, &chunk)].concat();
        let sets = (0..MAX_HELD / chunk.len() + 2)
            .take_while(|_| parse_display_set_in(&more, &mut state).is_ok())
            .count();
        assert_eq!(sets, MAX_HELD / chunk.len() - 1);
        assert_eq!(
            parse_display_set_in(&more, &mut state).err(),
            Some(PgsError::Malformed("objects past the epoch's coded byte budget"))
        );
        assert!(state.held <= MAX_HELD);
    }

    #[test]
    fn ycrcb_endpoints_in_both_matrices() {
        for bt601 in [false, true] {
            assert_eq!(ycrcb_to_rgb(16, 128, 128, bt601), [0, 0, 0]);
            let w = ycrcb_to_rgb(235, 128, 128, bt601);
            assert!(w.iter().all(|&c| c >= 254), "white ≈ 255: {w:?}");
            let g = ycrcb_to_rgb(126, 128, 128, bt601);
            assert!(g[0] == g[1] && g[1] == g[2], "neutral chroma is achromatic: {g:?}");
        }
        // Pure red in BT.601 (Y 81, Cb 90, Cr 240) reads as red there, not in BT.709.
        let [r, g, b] = ycrcb_to_rgb(81, 240, 90, true);
        assert!(r >= 254 && g == 0 && b == 0, "{r} {g} {b}");
        assert!(ycrcb_to_rgb(81, 240, 90, false)[1] > 16, "green in BT.709");
    }

    #[test]
    fn rle_decodes_each_run_form() {
        let rle = [
            0x00, 0x04, // 00b: 4 px of colour 0
            0x00, 0x84, 0x05, // 10b: 4 px of colour 5
            0x00, 0x00, // EOL
            0x07, // literal 1 px colour 7
            0x00, 0x0A, // 10 px colour 0, clamps to the 7 remaining of an 8-wide row
            0x00, 0x00, // EOL
        ];
        let idx = indices(&rle, 8, 2).expect("rle");
        assert_eq!(&idx[0..8], &[0, 0, 0, 0, 5, 5, 5, 5], "row 0: zeros then fives");
        assert_eq!(idx[8], 7, "row 1 first pixel is the literal 7");
        assert_eq!(&idx[9..16], &[0, 0, 0, 0, 0, 0, 0], "row 1 rest zero (clamped run)");
    }

    /// A lookup whose entry `i` has alpha `i`, so the painted alpha plane is the index plane.
    fn indices(rle: &[u8], w: usize, h: usize) -> Result<Vec<u8>, PgsError> {
        let mut lut = [[0u8; 4]; 256];
        for (i, e) in lut.iter_mut().enumerate() {
            *e = [0, 0, 0, i as u8];
        }
        let mut out = vec![0u8; w * h * 4];
        decode_rle(rle, w, [0, 0, w as u32, h as u32], &lut, &mut out)?;
        Ok(out.as_chunks::<4>().0.iter().map(|p| p[3]).collect())
    }

    #[test]
    fn rle_long_runs() {
        let rle = [0x00, 0x40, 0x64, 0x00, 0x00];
        let idx = indices(&rle, 100, 1).expect("rle");
        assert!(idx.iter().all(|&c| c == 0));
        assert_eq!(idx.len(), 100);

        let rle = [0x00, 0xC0, 0x64, 0x09, 0x00, 0x00];
        let idx = indices(&rle, 100, 1).expect("rle");
        assert!(idx.iter().all(|&c| c == 9));
    }

    #[test]
    fn rle_truncation_errors() {
        assert!(indices(&[0x00, 0x83], 8, 1).is_err());
        assert!(indices(&[0x00, 0x40], 8, 1).is_err());
    }

    #[test]
    fn a_warm_state_renders_into_a_slot_without_allocating() {
        let set = display_set_bytes();
        let mut scratch = PgsState::new();
        let mut slot = vec![0u8; 256];
        let n = {
            let pic = parse_display_set_in(&set, &mut scratch).expect("parses");
            pic.write_bitmap(&mut slot).expect("fits")
        };
        let ds = decode_bitmap(&slot[..n]).expect("a bitmap payload");
        assert_eq!(ds, parse_display_set(&set).expect("decodes"));
        let (ok, grew) = pf_sim::corrupt::heap_during(|| {
            let pic = parse_display_set_in(&set, &mut scratch).expect("parses");
            pic.write_bitmap(&mut slot).is_ok()
        });
        assert!(ok);
        assert_eq!(grew, 0, "a warm decoder allocated {grew} bytes");
    }

    fn display_set_bytes() -> Vec<u8> {
        let pds = {
            let mut p = vec![
                SEG_PDS, 0x00, 0x00, // type, size placeholder
            ];
            let body = [
                0x00, 0x00, // palette_id, version
                0x01, 235, 128, 128, 255, // idx1 white
                0x02, 82, 240, 90, 255, // idx2 red
            ];
            let len = body.len() as u16;
            p[1..3].copy_from_slice(&len.to_be_bytes());
            p.extend_from_slice(&body);
            p
        };
        let ods = {
            let rle = [0x01u8, 0x02, 0x00, 0x00];
            let mut body = vec![
                0x00, 0x00, // object_id
                0x00, // version
                0xC0, // first+last
            ];
            let data_len = (4 + rle.len()) as u32; // 4 = width+height bytes
            body.extend_from_slice(&data_len.to_be_bytes()[1..]); // u24
            body.extend_from_slice(&2u16.to_be_bytes()); // width
            body.extend_from_slice(&1u16.to_be_bytes()); // height
            body.extend_from_slice(&rle);
            let mut o = vec![SEG_ODS, 0x00, 0x00];
            let len = body.len() as u16;
            o[1..3].copy_from_slice(&len.to_be_bytes());
            o.extend_from_slice(&body);
            o
        };
        let pcs = {
            let mut body = vec![];
            body.extend_from_slice(&1920u16.to_be_bytes()); // width
            body.extend_from_slice(&1080u16.to_be_bytes()); // height
            body.push(0x10); // frame_rate
            body.extend_from_slice(&0u16.to_be_bytes()); // composition_number
            body.push(0x80); // composition_state (epoch start)
            body.push(0x00); // palette_update_flag
            body.push(0x00); // palette_id
            body.push(0x01); // number_of_composition_objects
            body.extend_from_slice(&0u16.to_be_bytes()); // object_id
            body.push(0x00); // window_id
            body.push(0x00); // object_cropped_flag
            body.extend_from_slice(&100u16.to_be_bytes()); // x
            body.extend_from_slice(&200u16.to_be_bytes()); // y
            let mut p = vec![SEG_PCS, 0x00, 0x00];
            let len = body.len() as u16;
            p[1..3].copy_from_slice(&len.to_be_bytes());
            p.extend_from_slice(&body);
            p
        };
        let end = [SEG_END, 0x00, 0x00];

        let mut ds = Vec::new();
        ds.extend_from_slice(&pcs);
        ds.extend_from_slice(&pds);
        ds.extend_from_slice(&ods);
        ds.extend_from_slice(&end);
        ds
    }

    #[test]
    fn display_set_decodes_to_expected_rgba() {
        let out = parse_display_set(&display_set_bytes()).expect("display set decodes");
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!((out.x, out.y), (100, 200));
        assert_eq!((out.video_width, out.video_height), (1920, 1080));
        assert!(!out.is_clear());
        assert_eq!(&out.rgba[0..4], &[255, 255, 255, 255], "white pixel");
        assert_eq!(out.rgba[7], 255, "second pixel opaque");
        assert!(
            out.rgba[4] > out.rgba[5] && out.rgba[4] > out.rgba[6],
            "second pixel red-dominant"
        );
    }

    #[test]
    fn empty_composition_is_a_clear() {
        let pcs = {
            let mut body = vec![];
            body.extend_from_slice(&1920u16.to_be_bytes());
            body.extend_from_slice(&1080u16.to_be_bytes());
            body.push(0x10);
            body.extend_from_slice(&1u16.to_be_bytes());
            body.push(0x00); // normal state
            body.push(0x00);
            body.push(0x00);
            body.push(0x00); // zero composition objects
            let mut p = vec![SEG_PCS, 0x00, 0x00];
            let len = body.len() as u16;
            p[1..3].copy_from_slice(&len.to_be_bytes());
            p.extend_from_slice(&body);
            p
        };
        let mut ds = pcs;
        ds.extend_from_slice(&[SEG_END, 0x00, 0x00]);
        let out = parse_display_set(&ds).expect("clear parses");
        assert!(out.is_clear(), "no objects → clear");
        assert_eq!((out.video_width, out.video_height), (1920, 1080));
    }

    #[test]
    fn missing_pcs_errors() {
        let ds = [SEG_END, 0x00, 0x00];
        assert_eq!(parse_display_set(&ds), Err(PgsError::NoComposition));
    }

    #[test]
    fn truncated_segments_error() {
        assert!(parse_display_set(&[SEG_PCS, 0x00]).is_err(), "truncated size field");
        assert!(parse_display_set(&[SEG_PCS, 0x00, 0xC8]).is_err(), "payload runs past end");
    }

    #[test]
    fn bitmap_wire_round_trips() {
        let ds = DisplaySet {
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
            width: 2,
            height: 1,
            x: 100,
            y: 200,
            video_width: 1920,
            video_height: 1080,
        };
        let wire = encode_bitmap(&ds);
        assert_eq!(
            wire.len(),
            crate::bitmap::BITMAP_HEADER_LEN + crate::bitmap::BITMAP_RECT_LEN + 8
        );
        assert_eq!(decode_bitmap(&wire), Some(ds));

        let clear = DisplaySet::clear(1920, 1080);
        let wire = encode_bitmap(&clear);
        assert_eq!(wire.len(), crate::bitmap::BITMAP_HEADER_LEN);
        let back = decode_bitmap(&wire).expect("clear decodes");
        assert!(back.is_clear());
        assert_eq!((back.video_width, back.video_height), (1920, 1080));

        assert!(decode_bitmap(b"XXXX").is_none());
        assert!(decode_bitmap(&wire[..4]).is_none());
        let mut bad = encode_bitmap(&DisplaySet {
            rgba: vec![0; 8],
            width: 2,
            height: 1,
            x: 0,
            y: 0,
            video_width: 1,
            video_height: 1,
        });
        bad.pop(); // now the RGBA is one byte short
        assert!(decode_bitmap(&bad).is_none());
    }

    #[test]
    fn strip_sup_header_peels_the_pg_frame() {
        let record = [
            0x50, 0x47, // PG
            0x00, 0x0C, 0x7C, 0x68, // pts
            0x00, 0x00, 0x00, 0x00, // dts
            SEG_END, 0x00, 0x00, // segment
        ];
        let (pts, seg) = strip_sup_header(&record).expect("peels");
        assert_eq!(pts, 0x000C_7C68);
        assert_eq!(seg, &[SEG_END, 0x00, 0x00]);
        assert!(strip_sup_header(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }
}
