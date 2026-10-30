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

const MAX_OBJECTS: usize = 64;

const SUP_MAGIC: [u8; 2] = [0x50, 0x47];

#[derive(Clone, Copy)]
struct Palette {
    entries: [[u8; 4]; 256],
}

impl Palette {
    fn new() -> Self {
        // An index no PDS defined stays transparent, the safe default.
        Palette { entries: [[0, 128, 128, 0]; 256] }
    }

    fn apply_pds(&mut self, payload: &[u8]) -> Result<(), PgsError> {
        if payload.len() < 2 {
            return Err(PgsError::Truncated("PDS header"));
        }
        let body = &payload[2..];
        if !body.len().is_multiple_of(5) {
            return Err(PgsError::Malformed("PDS entry array not a multiple of 5"));
        }
        for e in body.as_chunks::<5>().0 {
            let idx = e[0] as usize;
            self.entries[idx] = [e[1], e[2], e[3], e[4]];
        }
        Ok(())
    }

    #[inline]
    fn rgba(&self, index: u8) -> [u8; 4] {
        let [y, cr, cb, a] = self.entries[index as usize];
        let [r, g, b] = ycrcb_to_rgb_bt709(y, cr, cb);
        [r, g, b, a]
    }
}

/// Limited-range BT.709 Y'CrCb to full-range RGB (ITU-R BT.709 sections 3.2 and 3.3).
#[inline]
fn ycrcb_to_rgb_bt709(y: u8, cr: u8, cb: u8) -> [u8; 3] {
    let yf = (y as f32 - 16.0) * 1.164_383_6;
    let crf = cr as f32 - 128.0;
    let cbf = cb as f32 - 128.0;
    let r = yf + 1.792_741_1 * crf;
    let g = yf - 0.213_248_6 * cbf - 0.532_909_3 * crf;
    let b = yf + 2.112_401_7 * cbf;
    [clamp_u8(r), clamp_u8(g), clamp_u8(b)]
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

#[derive(Clone, Copy)]
struct CompObject {
    object_id: u16,
    x: u32,
    y: u32,
}

/// The composition's size and its first object, the one composited.
#[derive(Clone, Copy)]
struct Composition {
    video_width: u32,
    video_height: u32,
    first: Option<CompObject>,
}

fn parse_pcs(payload: &[u8]) -> Result<Composition, PgsError> {
    let mut r = Reader::new(payload);
    let video_width = r.u16("PCS width")? as u32;
    let video_height = r.u16("PCS height")? as u32;
    r.skip(1, "PCS frame_rate")?;
    r.skip(2, "PCS composition_number")?;
    r.skip(1, "PCS composition_state")?;
    r.skip(1, "PCS palette_update_flag")?;
    r.skip(1, "PCS palette_id")?;
    let num_objects = r.u8("PCS number_of_composition_objects")?;
    let mut first = None;
    for _ in 0..num_objects {
        let object_id = r.u16("PCS object_id")?;
        r.skip(1, "PCS window_id")?;
        let cropped = r.u8("PCS object_cropped_flag")?;
        let x = r.u16("PCS x")? as u32;
        let y = r.u16("PCS y")? as u32;
        if cropped & 0x40 != 0 {
            r.skip(8, "PCS crop rectangle")?;
        }
        if first.is_none() {
            first = Some(CompObject { object_id, x, y });
        }
    }
    Ok(Composition { video_width, video_height, first })
}

/// One object's fragments reassembled. Lives in [`PgsScratch`], its buffer reused across sets.
#[derive(Default)]
struct ObjectAccum {
    id: u16,
    width: u32,
    height: u32,
    rle: Vec<u8>,
    complete: bool,
}

/// Reassembly tables a decoder keeps across Display Sets, so a warm decoder allocates nothing.
#[derive(Default)]
pub struct PgsScratch {
    objects: Vec<ObjectAccum>,
    /// Objects of the current set, the first `used` entries of `objects`.
    used: usize,
}

// Scratch grows to the largest set seen and is reused.
#[allow(clippy::disallowed_methods)]
impl PgsScratch {
    pub fn new() -> PgsScratch {
        PgsScratch::default()
    }

    fn reset(&mut self) {
        self.used = 0;
    }

    fn find(&mut self, id: u16) -> Option<&mut ObjectAccum> {
        self.objects[..self.used].iter_mut().find(|o| o.id == id)
    }

    /// The accumulator for `id`, a fresh one when the set has not seen it.
    fn upsert(&mut self, id: u16) -> Result<&mut ObjectAccum, PgsError> {
        let at = match self.objects[..self.used].iter().position(|o| o.id == id) {
            Some(at) => at,
            None if self.used >= MAX_OBJECTS => {
                return Err(PgsError::Malformed("display set defines too many objects"));
            }
            None => {
                if self.objects.len() == self.used {
                    self.objects.push(ObjectAccum::default());
                }
                self.used += 1;
                self.used - 1
            }
        };
        Ok(&mut self.objects[at])
    }
}

fn parse_ods(payload: &[u8], scratch: &mut PgsScratch) -> Result<(), PgsError> {
    let mut r = Reader::new(payload);
    let object_id = r.u16("ODS object_id")?;
    r.skip(1, "ODS object_version")?;
    let seq = r.u8("ODS sequence_flag")?;
    let is_first = seq & 0x80 != 0;
    let is_last = seq & 0x40 != 0;
    if is_first {
        // Validated by reassembly length, so data_length is skipped.
        r.skip(3, "ODS object_data_length")?;
        let width = r.u16("ODS width")? as u32;
        let height = r.u16("ODS height")? as u32;
        let rest = r.rest();
        let acc = scratch.upsert(object_id)?;
        acc.id = object_id;
        acc.width = width;
        acc.height = height;
        acc.rle.clear();
        // Reassembly scratch, reused across sets.
        #[allow(clippy::disallowed_methods)]
        acc.rle.extend_from_slice(rest);
        acc.complete = is_last;
    } else {
        r.skip(3, "ODS object_data_length (continuation)")?;
        let more = r.rest();
        match scratch.find(object_id) {
            Some(acc) => {
                #[allow(clippy::disallowed_methods)]
                acc.rle.extend_from_slice(more);
                acc.complete |= is_last;
            }
            None => {
                return Err(PgsError::Malformed("ODS continuation without a first fragment"));
            }
        }
    }
    Ok(())
}

/// A parsed Display Set borrowed from the scratch it was read into, ready to render.
pub struct Picture<'a> {
    palette: Palette,
    /// Object bitmap dimensions in pixels, zero for a clear.
    pub width: u32,
    pub height: u32,
    /// Top-left placement of the object in the reference video frame.
    pub x: u32,
    pub y: u32,
    /// The composition's reference video size.
    pub video_width: u32,
    pub video_height: u32,
    rle: &'a [u8],
}

impl Picture<'_> {
    /// A composition with nothing to draw.
    pub fn is_clear(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The `subtitle/bitmap` payload length this picture renders to.
    pub fn payload_len(&self) -> Option<usize> {
        if self.is_clear() { payload_len(&[]) } else { payload_len(&[(self.width, self.height)]) }
    }

    /// Write the picture as a `subtitle/bitmap` payload into `out`, the payload length back.
    pub fn write_bitmap(&self, out: &mut [u8]) -> Result<usize, PgsError> {
        let n_rects = u32::from(!self.is_clear());
        let mut b = BitmapBuilder::new(out, self.video_width, self.video_height, n_rects)
            .map_err(|_| PgsError::Malformed("bitmap slot too small"))?;
        if !self.is_clear() {
            let px = b
                .rect(self.x, self.y, self.width, self.height)
                .map_err(|_| PgsError::Malformed("bitmap slot too small"))?;
            self.render(px)?;
        }
        b.finish().map_err(|_| PgsError::Malformed("bitmap slot too small"))
    }

    /// Decode the run lengths straight into `rgba`, `width * height * 4` bytes.
    pub fn render(&self, rgba: &mut [u8]) -> Result<(), PgsError> {
        let (w, h) = (self.width as usize, self.height as usize);
        if rgba.len() != w * h * 4 {
            return Err(PgsError::Malformed("rgba slice is not width * height * 4"));
        }
        if w == 0 || h == 0 {
            return Ok(());
        }
        // A pixel no run reaches shows palette entry 0, as the index plane did.
        let zero = self.palette.rgba(0);
        for px in rgba.as_chunks_mut::<4>().0 {
            *px = zero;
        }
        decode_rle(self.rle, w, h, &self.palette, rgba)
    }

    /// The picture on the heap.
    #[allow(clippy::disallowed_methods)]
    pub fn to_display_set(&self) -> Result<DisplaySet, PgsError> {
        let mut rgba = vec![0u8; self.width as usize * self.height as usize * 4];
        self.render(&mut rgba)?;
        Ok(DisplaySet {
            rgba,
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            video_width: self.video_width,
            video_height: self.video_height,
        })
    }
}

/// Parse one **Display Set** (a run of bare segments, no `.sup` `PG` headers) into a decoded
/// [`DisplaySet`] on the heap. Tests, tools and the examples.
#[allow(clippy::disallowed_methods)]
pub fn parse_display_set(data: &[u8]) -> Result<DisplaySet, PgsError> {
    let mut scratch = PgsScratch::new();
    parse_display_set_in(data, &mut scratch)?.to_display_set()
}

/// Parse one Display Set into `scratch` and hand back the picture to render. The object's size
/// is checked against what its coded data can hold before anything is painted.
pub fn parse_display_set_in<'s>(
    data: &[u8],
    scratch: &'s mut PgsScratch,
) -> Result<Picture<'s>, PgsError> {
    let mut palette = Palette::new();
    let mut composition: Option<Composition> = None;
    scratch.reset();

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
            SEG_PCS => composition = Some(parse_pcs(payload)?),
            SEG_WDS => { /* window rectangles, read implicitly, placement is from the PCS */ }
            SEG_PDS => palette.apply_pds(payload)?,
            SEG_ODS => parse_ods(payload, scratch)?,
            SEG_END => break,
            // An unknown segment type is skipped, its length framed it.
            _ => {}
        }
    }

    let comp = composition.ok_or(PgsError::NoComposition)?;
    let clear = Picture {
        palette,
        width: 0,
        height: 0,
        x: 0,
        y: 0,
        video_width: comp.video_width,
        video_height: comp.video_height,
        rle: &[],
    };
    // Only the first composition object is composited. One with no ODS data is a clear.
    let Some(first) = comp.first else {
        return Ok(clear);
    };
    let Some(acc) = scratch.objects[..scratch.used].iter().find(|o| o.id == first.object_id) else {
        return Ok(clear);
    };
    let (w, h) = (acc.width as usize, acc.height as usize);
    if w == 0 || h == 0 {
        return Ok(clear);
    }
    // Reject an overflowing or ballooning object before a slot is sized.
    let pixels = w.checked_mul(h).ok_or(PgsError::Malformed("object size overflow"))?;
    if pixels > 8192 * 8192 {
        return Err(PgsError::Malformed("object size implausibly large"));
    }
    // Every three coded bytes stand for at most `MAX_RUN` pixels, so a size the data cannot code
    // is refused before a plane is painted.
    if pixels > MAX_RUN.saturating_mul(acc.rle.len().div_ceil(3)) {
        return Err(PgsError::Malformed("object size exceeds what its coded data can hold"));
    }
    // An object never exceeds its video, and no disc format exceeds UHD.
    let video = comp.video_width > 0 && comp.video_height > 0;
    if w > MAX_OBJECT_W || h > MAX_OBJECT_H {
        return Err(PgsError::Malformed("object larger than any supported video"));
    }
    if video && (w > comp.video_width as usize || h > comp.video_height as usize) {
        return Err(PgsError::Malformed("object larger than its video"));
    }
    Ok(Picture {
        palette,
        width: acc.width,
        height: acc.height,
        x: first.x,
        y: first.y,
        video_width: comp.video_width,
        video_height: comp.video_height,
        rle: &acc.rle,
    })
}

/// Decode the HDMV run lengths of a `width` x `height` object straight into RGBA.
fn decode_rle(
    rle: &[u8],
    width: usize,
    height: usize,
    palette: &Palette,
    out: &mut [u8],
) -> Result<(), PgsError> {
    let mut i = 0usize;
    let mut row = 0usize;
    let mut col = 0usize;

    let put = |out: &mut [u8], row: usize, col: usize, count: usize, colour: u8| -> usize {
        if row >= height || col >= width {
            return col; // row already full or no row left, wait for the EOL marker
        }
        let start = row * width + col;
        let end = (col + count).min(width);
        let n = end - col;
        if n > 0 {
            let px = palette.rgba(colour);
            for p in out[start * 4..(start + n) * 4].as_chunks_mut::<4>().0 {
                *p = px;
            }
        }
        end
    };

    // Rows advance only on the `00 00` end-of-line marker, so a run that fills a row and its EOL
    // do not advance twice.
    while i < rle.len() && row < height {
        let b0 = rle[i];
        i += 1;
        if b0 != 0 {
            // A single pixel of colour b0.
            col = put(out, row, col, 1, b0);
            continue;
        }
        let b1 = *rle.get(i).ok_or(PgsError::Truncated("RLE run byte"))?;
        i += 1;
        if b1 == 0 {
            row += 1;
            col = 0;
            continue;
        }
        let run_form = b1 >> 6; // top two bits
        let (count, colour) = match run_form {
            0b00 => {
                // 1..63 pixels of colour 0.
                ((b1 & 0x3F) as usize, 0u8)
            }
            0b01 => {
                // 64..16383 pixels of colour 0, length low 6 bits and the next byte.
                let lo = *rle.get(i).ok_or(PgsError::Truncated("RLE long-run length"))?;
                i += 1;
                ((((b1 & 0x3F) as usize) << 8) | lo as usize, 0u8)
            }
            0b10 => {
                // 3..63 pixels of the colour in the next byte.
                let c = *rle.get(i).ok_or(PgsError::Truncated("RLE short-colour byte"))?;
                i += 1;
                ((b1 & 0x3F) as usize, c)
            }
            _ => {
                // 64..16383 pixels of a colour, length and colour in two more bytes.
                let lo = *rle.get(i).ok_or(PgsError::Truncated("RLE long-run length"))?;
                i += 1;
                let c = *rle.get(i).ok_or(PgsError::Truncated("RLE long-colour byte"))?;
                i += 1;
                ((((b1 & 0x3F) as usize) << 8) | lo as usize, c)
            }
        };
        col = put(out, row, col, count, colour);
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

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn ycrcb_bt709_endpoints() {
        assert_eq!(ycrcb_to_rgb_bt709(16, 128, 128), [0, 0, 0]);
        let w = ycrcb_to_rgb_bt709(235, 128, 128);
        assert!(w.iter().all(|&c| c >= 254), "white ≈ 255: {w:?}");
        let g = ycrcb_to_rgb_bt709(126, 128, 128);
        assert!(g[0] == g[1] && g[1] == g[2], "neutral chroma is achromatic: {g:?}");
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

    /// A palette whose entry `i` has alpha `i`, so the painted alpha plane is the index plane.
    fn indices(rle: &[u8], w: usize, h: usize) -> Result<Vec<u8>, PgsError> {
        let mut palette = Palette::new();
        for (i, e) in palette.entries.iter_mut().enumerate() {
            *e = [0, 128, 128, i as u8];
        }
        let mut out = vec![0u8; w * h * 4];
        decode_rle(rle, w, h, &palette, &mut out)?;
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
    fn a_warm_scratch_renders_into_a_slot_without_allocating() {
        let set = display_set_bytes();
        let mut scratch = PgsScratch::new();
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
