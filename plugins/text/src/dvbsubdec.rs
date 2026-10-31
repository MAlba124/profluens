//! `dvbsubdec`, decode a DVB subtitle track (ETSI EN 300 743 V1.6.1) to the `subtitle/bitmap`
//! family for the overlay.

use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;
use profluens_core::time::Timestamp;

use crate::BITMAP_FAMILY;
use crate::bitmap::{BitmapBuilder, payload_len};

/// The DVB subtitle family the mkv demuxer announces for an `S_DVBSUB` track.
pub const DVB_FAMILY: &str = "subtitle/dvb";

// Segment types, EN 300 743 table 7.
const SEG_PAGE: u8 = 0x10;
const SEG_REGION: u8 = 0x11;
const SEG_CLUT: u8 = 0x12;
const SEG_OBJECT: u8 = 0x13;
const SEG_DISPLAY: u8 = 0x14;
const SEG_END: u8 = 0x80;
const SYNC: u8 = 0x0F;
// The PES data field head, table 3.
const DATA_IDENTIFIER: u8 = 0x20;
const STREAM_ID: u8 = 0x00;
const END_OF_DATA: u8 = 0xFF;
/// Sync byte, type, page id and length, table 6.
const SEGMENT_HEADER: usize = 6;
/// The `S_DVBSUB` CodecPrivate, codec specification section 6.7.1.
const CODEC_PRIVATE: usize = 5;

/// `display_width` and `display_height` code 0 to 4095 plus one (section 7.2.1), and a region is no
/// larger than the display (section 7.2.3).
pub const MAX_SIDE: u32 = 4096;
/// Region index bytes held at once, a 4096x4096 region.
pub const PIXEL_BUDGET: usize = MAX_SIDE as usize * MAX_SIDE as usize;
/// Object list entries held at once over all regions.
pub const PLACEMENT_BUDGET: usize = 64 * 1024;
/// Runs walked plus pixels written for one object segment, so a small object placed thousands of
/// times cannot stall the pass.
pub const PAINT_BUDGET: u64 = 1 << 26;

/// No display definition, the SDTV grid (section 5.1.3).
const DEFAULT_DISPLAY: Display = Display { width: 720, height: 576, x: 0, y: 0 };

const PIX: usize = 0;
const OBJ: usize = 1;

type Rgba = [u8; 4];

/// Why a display set was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DvbError {
    Malformed(&'static str),
    Budget(&'static str),
}

impl DvbError {
    pub fn reason(self) -> &'static str {
        match self {
            DvbError::Malformed(r) | DvbError::Budget(r) => r,
        }
    }
}

impl std::fmt::Display for DvbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DvbError::Malformed(r) => write!(f, "dvb subtitle malformed, {r}"),
            DvbError::Budget(r) => write!(f, "dvb subtitle over budget, {r}"),
        }
    }
}

impl std::error::Error for DvbError {}

#[derive(Clone, Copy)]
struct Clut {
    two: [Rgba; 4],
    four: [Rgba; 16],
    eight: [Rgba; 256],
}

impl Clut {
    fn of_depth(&self, depth: u8) -> &[Rgba] {
        match depth {
            2 => &self.two,
            8 => &self.eight,
            _ => &self.four,
        }
    }
}

const fn on(i: usize, mask: usize, v: u8) -> u8 {
    if i & mask != 0 { v } else { 0 }
}

/// The default CLUTs, section 10.1 to 10.3. `b1` is the first bit received, the most significant.
const fn default_clut() -> Clut {
    let mut c = Clut { two: [[0; 4]; 4], four: [[0; 4]; 16], eight: [[0; 4]; 256] };
    // Table 38.
    c.two[1] = [255, 255, 255, 255];
    c.two[2] = [0, 0, 0, 255];
    c.two[3] = [127, 127, 127, 255];
    // Table 37, `b1` halves the intensity.
    let mut i = 1;
    while i < 16 {
        let v = if i & 8 == 0 { 255 } else { 127 };
        c.four[i] = [on(i, 1, v), on(i, 2, v), on(i, 4, v), 255];
        i += 1;
    }
    // Table 36, `b1` and `b5` pick the section.
    let mut i = 1;
    while i < 256 {
        let low = [
            on(i, 1, 85) + on(i, 0x10, 170),
            on(i, 2, 85) + on(i, 0x20, 170),
            on(i, 4, 85) + on(i, 0x40, 170),
        ];
        let dim = [
            on(i, 1, 43) + on(i, 0x10, 85),
            on(i, 2, 43) + on(i, 0x20, 85),
            on(i, 4, 43) + on(i, 0x40, 85),
        ];
        c.eight[i] = match (i & 0x80 != 0, i & 0x08 != 0) {
            (false, false) if i & 0x70 == 0 => [on(i, 1, 255), on(i, 2, 255), on(i, 4, 255), 63],
            (false, false) => [low[0], low[1], low[2], 255],
            (false, true) => [low[0], low[1], low[2], 127],
            (true, false) => [127 + dim[0], 127 + dim[1], 127 + dim[2], 255],
            (true, true) => [dim[0], dim[1], dim[2], 255],
        };
        i += 1;
    }
    c
}

static DEFAULT_CLUT: Clut = default_clut();

/// The map tables' defaults, section 10.4 to 10.6.
const MAP_2_TO_4: [u8; 4] = [0x0, 0x7, 0x8, 0xF];
const MAP_2_TO_8: [u8; 4] = [0x00, 0x77, 0x88, 0xFF];
const MAP_4_TO_8: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
];

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Span {
    at: u32,
    len: u32,
    cap: u32,
}

impl Span {
    fn range(self) -> std::ops::Range<usize> {
        self.at as usize..(self.at + self.len) as usize
    }
}

/// A region composition (section 7.2.3), indexed by `region_id`.
#[derive(Clone, Copy, Default)]
struct Region {
    defined: bool,
    width: u32,
    height: u32,
    depth: u8,
    clut: u8,
    spans: [Span; 2],
}

#[derive(Clone, Copy, Default)]
struct Placement {
    object: u16,
    x: u16,
    y: u16,
}

/// One page composition entry (section 7.2.2).
#[derive(Clone, Copy)]
struct PageEntry {
    region: u8,
    x: u16,
    y: u16,
}

/// The grid and the window offset within it (section 7.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Display {
    width: u32,
    height: u32,
    x: u32,
    y: u32,
}

#[derive(Clone, Copy)]
struct Run {
    line: u16,
    x: u16,
    len: u16,
    index: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closed {
    Blank,
    Clear { pts: Timestamp, duration: Timestamp },
    Show { pts: Timestamp, duration: Timestamp, rect: Rect },
}

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any(DVB_FAMILY), OfferDesc::any("bytes")];
static SRC_OFFERS: [OfferDesc; 2] = [OfferDesc::any(BITMAP_FAMILY), OfferDesc::any("bytes")];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

static DESC: ElementDesc = ElementDesc {
    name: "dvbsubdec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Cold, registry make_default, boxes one element at plugin registration.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(DvbSubDec::new())),
    io: false,
    couplings: &[],
};

/// Decodes a DVB subtitle track to `subtitle/bitmap`.
pub struct DvbSubDec {
    regions: [Region; 256],
    // 1-based index into `cluts`, 0 for none. u16 so 256 distinct ids cannot wrap it.
    clut_slot: [u16; 256],
    cluts: Vec<Clut>,
    pixels: Vec<u8>,
    placements: Vec<Placement>,
    page: Vec<PageEntry>,
    runs: Vec<Run>,
    page_time_out: u8,
    display: Display,
    composed: [u64; 4],
    open: bool,
    /// A page, region, CLUT or object segment arrived since the last close.
    touched: bool,
    saw_page: bool,
    open_pts: Timestamp,
    open_duration: Timestamp,
    pages: Option<(u16, u16)>,
    failing: bool,
    /// Bitmaps put out.
    pub decoded: u64,
    /// Clears put out.
    pub cleared: u64,
    /// Display sets dropped on a fault.
    pub dropped: u64,
    /// Of those, the ones past a budget.
    pub budget_faults: u64,
    /// Sets that closed with nothing to show and no clear, after a fault took the state.
    pub suppressed: u64,
}

impl Default for DvbSubDec {
    fn default() -> Self {
        Self::new()
    }
}

impl DvbSubDec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    // Cold, one-time constructor, the arenas grow to the stream's largest epoch, then are reused.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self {
            regions: [Region::default(); 256],
            clut_slot: [0; 256],
            cluts: Vec::new(),
            pixels: Vec::new(),
            placements: Vec::new(),
            page: Vec::new(),
            runs: Vec::new(),
            page_time_out: 0,
            display: DEFAULT_DISPLAY,
            composed: [0; 4],
            open: false,
            touched: false,
            saw_page: false,
            open_pts: Timestamp::NONE,
            open_duration: Timestamp::NONE,
            pages: None,
            failing: false,
            decoded: 0,
            cleared: 0,
            dropped: 0,
            budget_faults: 0,
            suppressed: 0,
        }
    }

    /// Region index bytes the arena holds, live and reclaimable.
    pub fn held_pixels(&self) -> usize {
        self.pixels.len()
    }

    /// Discards what a mode change discards (section 5.1.0). The capacity stays.
    fn reset_state(&mut self) {
        self.regions = [Region::default(); 256];
        self.clut_slot = [0; 256];
        self.cluts.clear();
        self.pixels.clear();
        self.placements.clear();
        self.page.clear();
        self.page_time_out = 0;
        self.composed = [0; 4];
        self.touched = false;
        self.saw_page = false;
    }

    fn fail(&mut self, ctx: &Ctx, e: DvbError) {
        self.dropped += 1;
        if matches!(e, DvbError::Budget(_)) {
            self.budget_faults += 1;
        }
        if self.failing {
            log!(ctx, Level::Debug, "dvb_drop", reason = e.reason());
        } else {
            log!(ctx, Level::Warn, "dvb_drop", reason = e.reason());
            self.failing = true;
        }
        self.reset_state();
        self.open = false;
    }

    #[cold]
    fn build_failed(&mut self, ctx: &Ctx, pts: Timestamp, stage: &'static str) {
        self.dropped += 1;
        log!(ctx, Level::Warn, "dvb_build_failed", stage = stage, pts = pts);
    }

    fn ingest(
        &mut self,
        ctx: &mut Ctx,
        data: &[u8],
        flags: BufferFlags,
        pts: Timestamp,
        duration: Timestamp,
    ) {
        // The page ids, wherever they come, never a Block.
        if flags.contains(BufferFlags::HEADER) {
            if data.len() == CODEC_PRIVATE {
                self.pages = Some((be16(&data[0..2]), be16(&data[2..4])));
                log!(
                    &*ctx,
                    Level::Debug,
                    "pages",
                    composition = be16(&data[0..2]),
                    ancillary = be16(&data[2..4])
                );
            } else {
                log!(&*ctx, Level::Warn, "head_ignored", bytes = data.len());
            }
            return;
        }
        if self.open && pts != self.open_pts {
            let set = self.close_set();
            self.emit(ctx, set);
        }
        self.feed(ctx, data, pts, duration);
    }

    fn feed(&mut self, ctx: &mut Ctx, data: &[u8], pts: Timestamp, duration: Timestamp) {
        let mut at =
            usize::from(data.len() >= 2 && data[0] == DATA_IDENTIFIER && data[1] == STREAM_ID) * 2;
        if at < data.len() && data[at] != SYNC && data[at] != END_OF_DATA {
            self.fail(ctx, DvbError::Malformed("a block that holds no subtitling segment"));
            return;
        }
        while at < data.len() && data[at] == SYNC {
            match self.segment(data, at, pts, duration) {
                Ok((next, closed)) => {
                    at = next;
                    if let Some(set) = closed {
                        self.emit(ctx, set);
                    }
                }
                Err(e) => {
                    self.fail(ctx, e);
                    return;
                }
            }
        }
    }

    fn segment(
        &mut self,
        data: &[u8],
        at: usize,
        pts: Timestamp,
        duration: Timestamp,
    ) -> Result<(usize, Option<Closed>), DvbError> {
        let head = data
            .get(at..at + SEGMENT_HEADER)
            .ok_or(DvbError::Malformed("a segment header past the end of its block"))?;
        let (kind, page, len) = (head[1], be16(&head[2..4]), usize::from(be16(&head[4..6])));
        let start = at + SEGMENT_HEADER;
        let payload = data
            .get(start..start + len)
            .ok_or(DvbError::Malformed("a segment longer than its block"))?;
        let next = start + len;
        // Another service's segment (section 7.2.0.1, page_id).
        if self
            .pages
            .is_some_and(|(composition, ancillary)| page != composition && page != ancillary)
        {
            return Ok((next, None));
        }
        self.open_pts = pts;
        self.open_duration = duration;
        match kind {
            SEG_END => return Ok((next, Some(self.close_set()))),
            SEG_PAGE => self.page_segment(payload)?,
            SEG_REGION => self.region_segment(payload)?,
            SEG_CLUT => self.clut_segment(payload)?,
            SEG_OBJECT => self.object_segment(payload)?,
            SEG_DISPLAY => self.display_segment(payload),
            // Unknown and unsupported types are ignored (section 7.2.0.1).
            _ => {}
        }
        self.touched |= matches!(kind, SEG_PAGE | SEG_REGION | SEG_CLUT | SEG_OBJECT);
        self.open = true;
        Ok((next, None))
    }

    /// Section 7.2.2.
    fn page_segment(&mut self, p: &[u8]) -> Result<(), DvbError> {
        if p.len() < 2 {
            return Err(DvbError::Malformed("a page composition shorter than its header"));
        }
        // Table 10, "mode change" starts an epoch.
        if (p[1] >> 2) & 3 == 2 {
            self.reset_state();
        }
        self.page_time_out = p[0];
        self.page.clear();
        let mut listed = [0u64; 4];
        for e in p[2..].as_chunks::<6>().0 {
            let id = usize::from(e[0]);
            if listed[id / 64] & (1 << (id % 64)) != 0 {
                break;
            }
            listed[id / 64] |= 1 << (id % 64);
            self.page.push(PageEntry { region: e[0], x: be16(&e[2..4]), y: be16(&e[4..6]) });
        }
        self.saw_page = true;
        Ok(())
    }

    /// Section 7.2.3.
    fn region_segment(&mut self, p: &[u8]) -> Result<(), DvbError> {
        if p.len() < 10 {
            return Err(DvbError::Malformed("a region composition shorter than its header"));
        }
        let id = usize::from(p[0]);
        let mut fill = (p[1] >> 3) & 1 == 1;
        let (width, height) = (u32::from(be16(&p[2..4])), u32::from(be16(&p[4..6])));
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            self.regions[id].defined = false;
            return Err(DvbError::Malformed("a region larger than any display"));
        }
        // Table 13.
        let depth = match (p[6] >> 2) & 7 {
            1 => 2,
            3 => 8,
            _ => 4,
        };
        let background = match depth {
            8 => p[8],
            4 => p[9] >> 4,
            _ => (p[9] >> 2) & 3,
        };
        // A character entry carries two more bytes, so entries are counted by walking.
        let (mut count, mut at) = (0usize, 10usize);
        while at + 6 <= p.len() {
            at += if matches!(p[at + 2] >> 6, 1 | 2) { 8 } else { 6 };
            count += 1;
        }

        // The pixels persist unless the size changed, which forces a fill.
        let old = self.regions[id];
        let pix = if old.defined && old.width == width && old.height == height {
            old.spans[PIX]
        } else {
            fill = true;
            self.take(PIX, id, (width * height) as usize)?
        };
        let obj = if old.defined && count <= old.spans[OBJ].cap as usize {
            Span { len: count as u32, ..old.spans[OBJ] }
        } else {
            self.take(OBJ, id, count)?
        };
        if fill {
            self.pixels[pix.range()].fill(background);
        }
        let mut at = 10usize;
        for slot in &mut self.placements[obj.range()] {
            let object_type = p[at + 2] >> 6;
            *slot = Placement {
                object: be16(&p[at..at + 2]),
                x: be16(&p[at + 2..at + 4]) & 0x0FFF,
                y: be16(&p[at + 4..at + 6]) & 0x0FFF,
            };
            at += if matches!(object_type, 1 | 2) { 8 } else { 6 };
        }
        self.regions[id] =
            Region { defined: true, width, height, depth, clut: p[7], spans: [pix, obj] };
        self.composed[id / 64] |= 1 << (id % 64);
        Ok(())
    }

    fn take(&mut self, k: usize, id: usize, need: usize) -> Result<Span, DvbError> {
        let (budget, what) = if k == PIX {
            (PIXEL_BUDGET, "region pixels past the decoder's budget")
        } else {
            (PLACEMENT_BUDGET, "object lists past the decoder's budget")
        };
        if need > budget {
            return Err(DvbError::Budget(what));
        }
        if self.arena_len(k) + need > budget {
            self.compact(k, id);
        }
        if self.arena_len(k) + need > budget {
            for (i, r) in self.regions.iter_mut().enumerate() {
                let kept = i == id
                    || self.composed[i / 64] & (1 << (i % 64)) != 0
                    || self.page.iter().any(|e| usize::from(e.region) == i);
                r.defined &= kept;
            }
            self.compact(k, id);
        }
        if self.arena_len(k) + need > budget {
            return Err(DvbError::Budget(what));
        }
        let at = self.arena_len(k);
        if k == PIX {
            self.pixels.reserve_exact(need);
            self.pixels.resize(at + need, 0);
        } else {
            self.placements.reserve_exact(need);
            self.placements.resize(at + need, Placement::default());
        }
        Ok(Span { at: at as u32, len: need as u32, cap: need as u32 })
    }

    fn arena_len(&self, k: usize) -> usize {
        if k == PIX { self.pixels.len() } else { self.placements.len() }
    }

    fn compact(&mut self, k: usize, skip: usize) {
        if k == PIX {
            compact(&mut self.pixels, &mut self.regions, PIX, skip);
        } else {
            compact(&mut self.placements, &mut self.regions, OBJ, skip);
        }
    }

    /// Section 7.2.4. Entries are converted to RGBA once, here.
    fn clut_segment(&mut self, p: &[u8]) -> Result<(), DvbError> {
        if p.len() < 2 {
            return Err(DvbError::Malformed("a CLUT definition shorter than its header"));
        }
        let id = usize::from(p[0]);
        if self.clut_slot[id] == 0 {
            self.cluts.push(DEFAULT_CLUT);
            self.clut_slot[id] = self.cluts.len() as u16;
        }
        let clut = &mut self.cluts[usize::from(self.clut_slot[id]) - 1];
        let mut at = 2usize;
        while at + 2 <= p.len() {
            let (entry, flags) = (usize::from(p[at]), p[at + 1]);
            if flags & 0xE0 == 0 {
                return Err(DvbError::Malformed("a CLUT entry for no CLUT"));
            }
            at += 2;
            // Full range is 8 bits each, else the 6, 4, 4 and 2 most significant bits.
            let (y, cr, cb, t) = if flags & 1 == 1 {
                let Some(e) = p.get(at..at + 4) else { break };
                at += 4;
                (e[0], e[1], e[2], e[3])
            } else {
                let Some(e) = p.get(at..at + 2) else { break };
                at += 2;
                (
                    e[0] & 0xFC,
                    ((e[0] << 6) | ((e[1] >> 2) & 0x30)) & 0xF0,
                    (e[1] << 2) & 0xF0,
                    (e[1] << 6) & 0xC0,
                )
            };
            // Y zero is full transparency, T zero none (section 7.2.4).
            let colour = ycbcr_to_rgba(y, cb, cr, if y == 0 { 0 } else { 255 - t });
            if flags & 0x80 != 0 && entry < 4 {
                clut.two[entry] = colour;
            }
            if flags & 0x40 != 0 && entry < 16 {
                clut.four[entry] = colour;
            }
            if flags & 0x20 != 0 {
                clut.eight[entry] = colour;
            }
        }
        Ok(())
    }

    /// Section 7.2.5. Each field is decoded once per region depth that places the object.
    fn object_segment(&mut self, p: &[u8]) -> Result<(), DvbError> {
        if p.len() < 3 {
            return Err(DvbError::Malformed("an object segment shorter than its header"));
        }
        let object = be16(&p[0..2]);
        let non_modifying = (p[2] >> 1) & 1 == 1;
        // Table 18, only coding of pixels is drawn.
        if (p[2] >> 2) & 3 != 0 {
            return Ok(());
        }
        if p.len() < 7 {
            return Err(DvbError::Malformed("an object segment with no field lengths"));
        }
        let (top_len, bottom_len) = (usize::from(be16(&p[3..5])), usize::from(be16(&p[5..7])));
        let body = &p[7..];
        if top_len + bottom_len > body.len() {
            return Err(DvbError::Malformed("an object's fields longer than its segment"));
        }
        let top = &body[..top_len];
        // An empty bottom field repeats the top one (section 7.2.5.1).
        let bottom = if bottom_len > 0 { &body[top_len..top_len + bottom_len] } else { top };
        let mut work = 0u64;
        for depth in [2u8, 4, 8] {
            let placed = self.regions.iter().any(|r| {
                r.defined
                    && r.depth == depth
                    && self.placements[r.spans[OBJ].range()].iter().any(|pl| pl.object == object)
            });
            if !placed {
                continue;
            }
            for field in 0..2u32 {
                if field == 0 || bottom_len > 0 {
                    decode_field(
                        if field == 0 { top } else { bottom },
                        depth,
                        non_modifying,
                        &mut self.runs,
                    );
                }
                work = paint(
                    &self.runs,
                    &self.regions,
                    &self.placements,
                    &mut self.pixels,
                    object,
                    depth,
                    field,
                    work,
                )?;
            }
        }
        Ok(())
    }

    /// Section 7.2.1. A definition past the coded range keeps the default grid.
    fn display_segment(&mut self, p: &[u8]) {
        if p.len() < 5 {
            return;
        }
        let (width, height) = (u32::from(be16(&p[1..3])) + 1, u32::from(be16(&p[3..5])) + 1);
        if width > MAX_SIDE || height > MAX_SIDE {
            self.display = DEFAULT_DISPLAY;
            return;
        }
        let (x, y) = if p[0] & 0x08 != 0 && p.len() >= 13 {
            (u32::from(be16(&p[5..7])), u32::from(be16(&p[9..11])))
        } else {
            (0, 0)
        };
        self.display = Display { width, height, x, y };
    }

    fn close_set(&mut self) -> Closed {
        self.open = false;
        self.composed = [0; 4];
        let (pts, duration) = (self.open_pts, held(self.open_duration, self.page_time_out));
        let saw_page = std::mem::take(&mut self.saw_page);
        // A bare END, or one after display segments only, changes nothing and emits nothing.
        if !std::mem::take(&mut self.touched) {
            return Closed::Blank;
        }
        if self.page.is_empty() {
            return if saw_page { Closed::Clear { pts, duration } } else { Closed::Blank };
        }
        let mut bounds: Option<Rect> = None;
        for e in &self.page {
            let Some(v) = self.visible(e) else { continue };
            bounds = Some(match bounds {
                None => v,
                Some(b) => {
                    let (x, y) = (b.x.min(v.x), b.y.min(v.y));
                    let (r, d) = ((b.x + b.w).max(v.x + v.w), (b.y + b.h).max(v.y + v.h));
                    Rect { x, y, w: r - x, h: d - y }
                }
            });
        }
        match bounds {
            Some(rect) => Closed::Show { pts, duration, rect },
            None => Closed::Blank,
        }
    }

    /// A page entry's region on the grid, clipped to it. Positions are relative to the
    /// display window when there is one (section 5.1.4).
    fn visible(&self, e: &PageEntry) -> Option<Rect> {
        let r = &self.regions[usize::from(e.region)];
        let (x, y) = (self.display.x + u32::from(e.x), self.display.y + u32::from(e.y));
        if !r.defined || x >= self.display.width || y >= self.display.height {
            return None;
        }
        Some(Rect {
            x,
            y,
            w: r.width.min(self.display.width - x),
            h: r.height.min(self.display.height - y),
        })
    }

    fn compose(&self, out: &mut [u8], rect: Rect) {
        out.fill(0);
        for e in &self.page {
            let Some(v) = self.visible(e) else { continue };
            let r = &self.regions[usize::from(e.region)];
            let slot = self.clut_slot[usize::from(r.clut)];
            let clut = if slot == 0 { &DEFAULT_CLUT } else { &self.cluts[usize::from(slot) - 1] };
            let palette = clut.of_depth(r.depth);
            let mask = palette.len() - 1;
            let src = &self.pixels[r.spans[PIX].range()];
            for row in 0..v.h {
                let s = &src[(row * r.width) as usize..][..v.w as usize];
                let d = (((v.y - rect.y + row) * rect.w + (v.x - rect.x)) * 4) as usize;
                for (px, &i) in
                    out[d..d + v.w as usize * 4].as_chunks_mut::<4>().0.iter_mut().zip(s)
                {
                    px.copy_from_slice(&palette[usize::from(i) & mask]);
                }
            }
        }
    }

    fn emit(&mut self, ctx: &mut Ctx, set: Closed) {
        let (pts, duration, rect) = match set {
            Closed::Blank => {
                self.suppressed += 1;
                log!(&*ctx, Level::Debug, "dvb_blank", reason = "a set with nothing to show");
                return;
            }
            Closed::Clear { pts, duration } => (pts, duration, Rect { x: 0, y: 0, w: 0, h: 0 }),
            Closed::Show { pts, duration, rect } => (pts, duration, rect),
        };
        let sizes: &[(u32, u32)] = if rect.w > 0 { &[(rect.w, rect.h)] } else { &[] };
        // The region budget bounds `w * h`, so the length fits.
        let n = payload_len(sizes).unwrap_or(0);
        let Some(mut buf) = ctx.alloc_exact(n) else {
            self.suppressed += 1;
            log!(&*ctx, Level::Warn, "dvb_refused", bytes = n);
            return;
        };
        let (gw, gh) = (self.display.width, self.display.height);
        let out = &mut buf.memory.as_mut_full()[..n];
        let Ok(mut b) = BitmapBuilder::new(out, gw, gh, sizes.len() as u32) else {
            return self.build_failed(ctx, pts, "bitmap header");
        };
        if rect.w > 0 {
            let Ok(px) = b.rect(rect.x, rect.y, rect.w, rect.h) else {
                return self.build_failed(ctx, pts, "bitmap rect");
            };
            self.compose(px, rect);
            self.decoded += 1;
        } else {
            self.cleared += 1;
        }
        let Ok(n) = b.finish() else {
            return self.build_failed(ctx, pts, "bitmap finish");
        };
        buf.memory.set_len(n);
        buf.pts = pts;
        buf.duration = duration;
        ctx.out(Self::OUT).push(buf);
        self.failing = false;
    }
}

impl Element for DvbSubDec {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.announce_format(Self::OUT, BITMAP_FAMILY, &[]);
        self.reset_state();
        self.display = DEFAULT_DISPLAY;
        (self.open, self.failing, self.pages) = (false, false, None);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.ingest(ctx, buf.memory.data(), buf.flags, buf.pts, buf.duration);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::Eos => {
                if self.open {
                    let set = self.close_set();
                    self.emit(ctx, set);
                }
                log!(
                    &*ctx,
                    Level::Info,
                    "eos",
                    decoded = self.decoded,
                    cleared = self.cleared,
                    dropped = self.dropped,
                    suppressed = self.suppressed,
                );
            }
            // The state is a delta on what came before the seek. The pages stay, the head may
            // come again.
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", open = self.open, dropped = self.dropped);
                self.reset_state();
                self.display = DEFAULT_DISPLAY;
                self.open = false;
            }
            Event::Gap { .. } => ctx.push_event(Self::OUT, event.clone()),
            _ => {}
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.reset_state();
        self.open = false;
    }
}

fn compact<T: Copy>(arena: &mut Vec<T>, regions: &mut [Region; 256], k: usize, skip: usize) {
    let mut order = [0u8; 256];
    let mut n = 0;
    for (i, r) in regions.iter().enumerate() {
        if r.defined && i != skip {
            order[n] = i as u8;
            n += 1;
        }
    }
    order[..n].sort_unstable_by_key(|&i| regions[usize::from(i)].spans[k].at);
    let mut to = 0usize;
    for &i in &order[..n] {
        let s = &mut regions[usize::from(i)].spans[k];
        arena.copy_within(s.range(), to);
        *s = Span { at: to as u32, len: s.len, cap: s.len };
        to += s.len as usize;
    }
    arena.truncate(to);
}

fn held(duration: Timestamp, time_out: u8) -> Timestamp {
    let cap =
        if time_out > 0 { Timestamp::from_secs(u64::from(time_out)) } else { Timestamp::NONE };
    match (duration.nanos(), cap.nanos()) {
        (Some(d), Some(c)) if d > 0 => Timestamp::from_nanos(d.min(c)),
        (Some(d), None) if d > 0 => duration,
        _ => cap,
    }
}

/// Field 0 starts on the object's first line, field 1 on its second (section 7.2.5.0).
#[allow(clippy::too_many_arguments)]
fn paint(
    runs: &[Run],
    regions: &[Region; 256],
    placements: &[Placement],
    pixels: &mut [u8],
    object: u16,
    depth: u8,
    field: u32,
    mut work: u64,
) -> Result<u64, DvbError> {
    for r in regions.iter().filter(|r| r.defined && r.depth == depth) {
        let px = &mut pixels[r.spans[PIX].range()];
        for pl in placements[r.spans[OBJ].range()].iter().filter(|pl| pl.object == object) {
            work += runs.len() as u64;
            for run in runs {
                let y = u32::from(pl.y) + field + 2 * u32::from(run.line);
                if y >= r.height {
                    break;
                }
                let x = u32::from(pl.x) + u32::from(run.x);
                if x >= r.width {
                    continue;
                }
                let n = u32::from(run.len).min(r.width - x);
                work += u64::from(n);
                let at = (y * r.width + x) as usize;
                px[at..at + n as usize].fill(run.index);
            }
            if work > PAINT_BUDGET {
                return Err(DvbError::Budget("an object placed past the paint budget"));
            }
        }
    }
    Ok(work)
}

struct Field<'a> {
    runs: &'a mut Vec<Run>,
    line: u16,
    x: u32,
    non_modifying: bool,
}

impl Field<'_> {
    /// `non_modifying_colour_flag` makes entry 1 leave the pixel as it is (section 7.2.5).
    fn put(&mut self, len: u32, index: u8) {
        if !(self.non_modifying && index == 1) && self.x < MAX_SIDE && len > 0 {
            let len = len.min(MAX_SIDE - self.x) as u16;
            self.runs.push(Run { line: self.line, x: self.x as u16, len, index });
        }
        self.x = self.x.saturating_add(len);
    }
}

/// One field's pixel-data sub-blocks (section 7.2.5.1, tables 20 and 21) into `runs`.
fn decode_field(data: &[u8], depth: u8, non_modifying: bool, runs: &mut Vec<Run>) {
    runs.clear();
    let (mut m24, mut m28, mut m48) = (MAP_2_TO_4, MAP_2_TO_8, MAP_4_TO_8);
    let mut f = Field { runs, line: 0, x: 0, non_modifying };
    let mut at = 0usize;
    while at < data.len() {
        let kind = data[at];
        at += 1;
        let rest = &data[at..];
        match kind {
            0x10 => {
                let map = match depth {
                    8 => Some(&m28),
                    4 => Some(&m24),
                    _ => None,
                };
                at += decode_2bit(rest, map, &mut f);
            }
            0x11 if depth >= 4 => at += decode_4bit(rest, (depth == 8).then_some(&m48), &mut f),
            0x12 if depth == 8 => at += decode_8bit(rest, &mut f),
            0x20 => {
                let Some(t) = rest.get(..2) else { break };
                m24 = [t[0] >> 4, t[0] & 0xF, t[1] >> 4, t[1] & 0xF];
                at += 2;
            }
            0x21 => {
                let Some(t) = rest.get(..4) else { break };
                m28.copy_from_slice(t);
                at += 4;
            }
            0x22 => {
                let Some(t) = rest.get(..16) else { break };
                m48.copy_from_slice(t);
                at += 16;
            }
            0xF0 => {
                f.line = f.line.saturating_add(1);
                f.x = 0;
            }
            _ => break,
        }
    }
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        if self.at + n > self.data.len() * 8 {
            return None;
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | u32::from((self.data[self.at >> 3] >> (7 - (self.at & 7))) & 1);
            self.at += 1;
        }
        Some(v)
    }

    /// Bytes used, every string ends byte aligned (table 20).
    fn bytes(&self) -> usize {
        self.at.div_ceil(8)
    }
}

/// Table 22, the 2-bit/pixel code string. Returns the bytes it used.
fn decode_2bit(data: &[u8], map: Option<&[u8; 4]>, f: &mut Field<'_>) -> usize {
    let mut b = Bits { data, at: 0 };
    while let Some(code) = b.read(2) {
        let (len, c) = if code != 0 {
            (1, code)
        } else if b.read(1) == Some(1) {
            let (Some(l), Some(c)) = (b.read(3), b.read(2)) else {
                break;
            };
            (l + 3, c)
        } else if b.read(1) == Some(1) {
            (1, 0)
        } else {
            match b.read(2) {
                Some(1) => (2, 0),
                Some(2) => {
                    let (Some(l), Some(c)) = (b.read(4), b.read(2)) else {
                        break;
                    };
                    (l + 12, c)
                }
                Some(3) => {
                    let (Some(l), Some(c)) = (b.read(8), b.read(2)) else {
                        break;
                    };
                    (l + 29, c)
                }
                // Table 23, `00` ends the string.
                _ => break,
            }
        };
        f.put(len, map.map_or(c as u8, |m| m[c as usize]));
    }
    b.bytes()
}

/// Table 24, the 4-bit/pixel code string.
fn decode_4bit(data: &[u8], map: Option<&[u8; 16]>, f: &mut Field<'_>) -> usize {
    let mut b = Bits { data, at: 0 };
    while let Some(code) = b.read(4) {
        let (len, c) = if code != 0 {
            (1, code)
        } else if b.read(1) == Some(0) {
            match b.read(3) {
                Some(l) if l > 0 => (l + 2, 0),
                _ => break,
            }
        } else if b.read(1) == Some(0) {
            let (Some(l), Some(c)) = (b.read(2), b.read(4)) else {
                break;
            };
            (l + 4, c)
        } else {
            // Table 25.
            match b.read(2) {
                Some(0) => (1, 0),
                Some(1) => (2, 0),
                Some(2) => {
                    let (Some(l), Some(c)) = (b.read(4), b.read(4)) else {
                        break;
                    };
                    (l + 9, c)
                }
                Some(_) => {
                    let (Some(l), Some(c)) = (b.read(8), b.read(4)) else {
                        break;
                    };
                    (l + 25, c)
                }
                None => break,
            }
        };
        f.put(len, map.map_or(c as u8, |m| m[c as usize]));
    }
    b.bytes()
}

/// Table 26, the 8-bit/pixel code string. It has no map table.
fn decode_8bit(data: &[u8], f: &mut Field<'_>) -> usize {
    let mut b = Bits { data, at: 0 };
    while let Some(code) = b.read(8) {
        let (len, c) = if code != 0 {
            (1, code)
        } else if b.read(1) == Some(0) {
            match b.read(7) {
                Some(l) if l > 0 => (l, 0),
                _ => break,
            }
        } else {
            let (Some(l), Some(c)) = (b.read(7), b.read(8)) else {
                break;
            };
            (l, c)
        };
        f.put(len, c as u8);
    }
    b.bytes()
}

/// Y'CbCr to RGB, ITU-R BT.601-7 at limited range as section 7.2.4 names. 8-bit fixed point.
fn ycbcr_to_rgba(y: u8, cb: u8, cr: u8, alpha: u8) -> Rgba {
    let (y, cb, cr) = (i32::from(y), i32::from(cb), i32::from(cr));
    let r = (298 * y + 409 * cr - 57_120) >> 8;
    let g = (298 * y - 100 * cb - 208 * cr + 34_656) >> 8;
    let b = (298 * y + 516 * cb - 70_816) >> 8;
    [r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8, alpha]
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// Hand-built DVB segments, for the tests here and in `tests/`.
#[doc(hidden)]
#[allow(clippy::disallowed_methods)] // test vectors, heap-built on purpose
pub mod fixtures {
    use super::{SEG_CLUT, SEG_DISPLAY, SEG_END, SEG_OBJECT, SEG_PAGE, SEG_REGION, SYNC};

    /// `[0x0f][type][page id 1][length][payload]`.
    pub fn segment(kind: u8, payload: &[u8]) -> Vec<u8> {
        segment_on(1, kind, payload)
    }

    pub fn segment_on(page: u16, kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![SYNC, kind];
        out.extend_from_slice(&page.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A Matroska Block, the segments back to back.
    pub fn block(segments: &[Vec<u8>]) -> Vec<u8> {
        segments.concat()
    }

    /// Time out in seconds, page state, and `(region, x, y)` entries.
    pub fn page(time_out: u8, state: u8, regions: &[(u8, u16, u16)]) -> Vec<u8> {
        let mut p = vec![time_out, (state & 3) << 2];
        for (id, x, y) in regions {
            p.extend_from_slice(&[*id, 0]);
            p.extend_from_slice(&x.to_be_bytes());
            p.extend_from_slice(&y.to_be_bytes());
        }
        segment(SEG_PAGE, &p)
    }

    /// A region composition with `(object, x, y)` bitmap objects.
    #[allow(clippy::too_many_arguments)]
    pub fn region(
        id: u8,
        w: u16,
        h: u16,
        depth: u8,
        clut: u8,
        background: u8,
        fill: bool,
        objects: &[(u16, u16, u16)],
    ) -> Vec<u8> {
        let code = match depth {
            2 => 1u8,
            8 => 3,
            _ => 2,
        };
        let mut p = vec![id, u8::from(fill) << 3];
        p.extend_from_slice(&w.to_be_bytes());
        p.extend_from_slice(&h.to_be_bytes());
        p.push(code << 2);
        p.push(clut);
        p.push(if depth == 8 { background } else { 0 });
        p.push(match depth {
            4 => background << 4,
            2 => background << 2,
            _ => 0,
        });
        for (object, x, y) in objects {
            p.extend_from_slice(&object.to_be_bytes());
            p.extend_from_slice(&x.to_be_bytes());
            p.extend_from_slice(&y.to_be_bytes());
        }
        segment(SEG_REGION, &p)
    }

    /// Full range entries `(entry, Y, Cr, Cb, T)` for all three CLUTs.
    pub fn clut(id: u8, entries: &[(u8, u8, u8, u8, u8)]) -> Vec<u8> {
        let mut p = vec![id, 0];
        for (e, y, cr, cb, t) in entries {
            p.extend_from_slice(&[*e, 0xE1, *y, *cr, *cb, *t]);
        }
        segment(SEG_CLUT, &p)
    }

    pub fn object(id: u16, top: &[u8], bottom: &[u8]) -> Vec<u8> {
        object_with(id, false, top, bottom)
    }

    /// Coding of pixels, `non_modifying` the colour flag.
    pub fn object_with(id: u16, non_modifying: bool, top: &[u8], bottom: &[u8]) -> Vec<u8> {
        let mut p = id.to_be_bytes().to_vec();
        p.push(u8::from(non_modifying) << 1);
        p.extend_from_slice(&(top.len() as u16).to_be_bytes());
        p.extend_from_slice(&(bottom.len() as u16).to_be_bytes());
        p.extend_from_slice(top);
        p.extend_from_slice(bottom);
        segment(SEG_OBJECT, &p)
    }

    /// The grid and optionally a window `(x min, x max, y min, y max)`.
    pub fn display(w: u16, h: u16, window: Option<(u16, u16, u16, u16)>) -> Vec<u8> {
        let mut p = vec![if window.is_some() { 0x08 } else { 0 }];
        p.extend_from_slice(&(w - 1).to_be_bytes());
        p.extend_from_slice(&(h - 1).to_be_bytes());
        if let Some((a, b, c, d)) = window {
            for v in [a, b, c, d] {
                p.extend_from_slice(&v.to_be_bytes());
            }
        }
        segment(SEG_DISPLAY, &p)
    }

    pub fn end() -> Vec<u8> {
        segment(SEG_END, &[])
    }

    /// Big-endian bit writer.
    #[derive(Default)]
    pub struct Bits {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl Bits {
        pub fn put(&mut self, v: u32, n: usize) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (v >> i) & 1 == 1 {
                    self.bytes[self.bits / 8] |= 1 << (7 - self.bits % 8);
                }
                self.bits += 1;
            }
        }

        pub fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// A 2-bit string of `(code, length)` runs and its end, behind data type 0x10, each run in a
    /// form table 22 has for it.
    pub fn two_bit(runs: &[(u8, u32)]) -> Vec<u8> {
        let mut b = Bits::default();
        for &(c, len) in runs {
            let c = u32::from(c & 3);
            let mut left = len;
            while left > 0 {
                if left >= 29 {
                    let n = left.min(284);
                    b.put(0, 4);
                    b.put(3, 2);
                    b.put(n - 29, 8);
                    b.put(c, 2);
                    left -= n;
                } else if left >= 12 {
                    let n = left.min(27);
                    b.put(0, 4);
                    b.put(2, 2);
                    b.put(n - 12, 4);
                    b.put(c, 2);
                    left -= n;
                } else if left >= 3 {
                    let n = left.min(10);
                    b.put(0, 2);
                    b.put(1, 1);
                    b.put(n - 3, 3);
                    b.put(c, 2);
                    left -= n;
                } else if c != 0 {
                    b.put(c, 2);
                    left -= 1;
                } else if left == 2 {
                    b.put(0, 4);
                    b.put(1, 2);
                    left = 0;
                } else {
                    b.put(0, 3);
                    b.put(1, 1);
                    left = 0;
                }
            }
        }
        b.put(0, 6);
        [vec![0x10], b.finish()].concat()
    }

    /// A 4-bit string behind data type 0x11 (table 24).
    pub fn four_bit(runs: &[(u8, u32)]) -> Vec<u8> {
        let mut b = Bits::default();
        for &(c, len) in runs {
            let c = u32::from(c & 15);
            let mut left = len;
            while left > 0 {
                if left >= 25 {
                    let n = left.min(280);
                    b.put(0, 4);
                    b.put(3, 2);
                    b.put(3, 2);
                    b.put(n - 25, 8);
                    b.put(c, 4);
                    left -= n;
                } else if left >= 9 {
                    let n = left.min(24);
                    b.put(0, 4);
                    b.put(3, 2);
                    b.put(2, 2);
                    b.put(n - 9, 4);
                    b.put(c, 4);
                    left -= n;
                } else if c != 0 && left >= 4 {
                    let n = left.min(7);
                    b.put(0, 4);
                    b.put(2, 2);
                    b.put(n - 4, 2);
                    b.put(c, 4);
                    left -= n;
                } else if c == 0 && left >= 3 {
                    b.put(0, 5);
                    b.put(left - 2, 3);
                    left = 0;
                } else if c != 0 {
                    b.put(c, 4);
                    left -= 1;
                } else {
                    b.put(0, 4);
                    b.put(3, 2);
                    b.put(left - 1, 2);
                    left = 0;
                }
            }
        }
        b.put(0, 8);
        [vec![0x11], b.finish()].concat()
    }

    /// An 8-bit string behind data type 0x12 (table 26).
    pub fn eight_bit(runs: &[(u8, u32)]) -> Vec<u8> {
        let mut b = Bits::default();
        for &(c, len) in runs {
            let mut left = len;
            while left > 0 {
                if c != 0 && left < 3 {
                    b.put(u32::from(c), 8);
                    left -= 1;
                } else {
                    let n = left.min(127);
                    b.put(0, 8);
                    b.put(u32::from(c != 0), 1);
                    b.put(n, 7);
                    if c != 0 {
                        b.put(u32::from(c), 8);
                    }
                    left -= n;
                }
            }
        }
        b.put(0, 16);
        [vec![0x12], b.finish()].concat()
    }

    /// The end of object line code.
    pub fn eol() -> Vec<u8> {
        vec![0xF0]
    }

    /// A 4x2 4-bit region at (100, 200), white on top and grey below, five seconds.
    pub fn minimal() -> Vec<u8> {
        block(&[
            page(5, 0, &[(1, 100, 200)]),
            region(1, 4, 2, 4, 1, 0, true, &[(7, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0), (2, 128, 128, 128, 0)]),
            object(7, &four_bit(&[(1, 4)]), &four_bit(&[(2, 4)])),
            end(),
        ])
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::pgs::{DisplaySet, decode_bitmap};
    use pf_harness::Harness;
    use profluens_core::bus::Bus;
    use profluens_core::id::{ElementId, FormatId};
    use profluens_core::memory::Pool;

    struct Rig {
        dec: DvbSubDec,
        ctx: Ctx,
        _bus: Bus,
    }

    struct Out {
        ds: DisplaySet,
        pts: Timestamp,
        duration: Timestamp,
    }

    impl Rig {
        fn new() -> Self {
            let (tx, bus) = Bus::channel();
            let mut ctx = Ctx::new(Pool::new(4096), tx, ElementId(0), FormatId(0), 1);
            let mut dec = DvbSubDec::new();
            dec.start(&mut ctx).expect("start");
            Rig { dec, ctx, _bus: bus }
        }

        fn push(&mut self, bytes: &[u8], ms: u64) -> Vec<Out> {
            self.push_for(bytes, ms, Timestamp::NONE)
        }

        fn push_for(&mut self, bytes: &[u8], ms: u64, duration: Timestamp) -> Vec<Out> {
            let flags = BufferFlags::empty();
            self.dec.ingest(&mut self.ctx, bytes, flags, Timestamp::from_millis(ms), duration);
            self.take()
        }

        fn push_head(&mut self, bytes: &[u8], ms: u64) -> Vec<Out> {
            let (pts, flags) = (Timestamp::from_millis(ms), BufferFlags::HEADER);
            self.dec.ingest(&mut self.ctx, bytes, flags, pts, Timestamp::NONE);
            self.take()
        }

        fn take(&mut self) -> Vec<Out> {
            let mut batch = self.ctx.take_output(DvbSubDec::OUT);
            let mut out = Vec::new();
            while let Some(b) = batch.pop_front() {
                let ds = decode_bitmap(b.memory.data()).expect("a well formed subtitle/bitmap");
                out.push(Out { ds, pts: b.pts, duration: b.duration });
            }
            out
        }
    }

    fn px(ds: &DisplaySet, x: u32, y: u32) -> Rgba {
        let i = ((y * ds.width + x) * 4) as usize;
        ds.rgba[i..i + 4].try_into().expect("four bytes")
    }

    const WHITE: Rgba = [254, 254, 254, 255];
    const GREY: Rgba = [130, 130, 130, 255];
    const RED: Rgba = [254, 0, 0, 255];
    const CLEAR: Rgba = [0, 0, 0, 0];

    #[test]
    fn a_minimal_display_set_is_one_bitmap() {
        let mut r = Rig::new();
        let out = r.push(&minimal(), 1_000);
        assert_eq!(out.len(), 1);
        let o = &out[0];
        assert_eq!((o.ds.x, o.ds.y, o.ds.width, o.ds.height), (100, 200, 4, 2));
        assert_eq!((o.ds.video_width, o.ds.video_height), (720, 576), "the default grid");
        assert_eq!(o.pts, Timestamp::from_millis(1_000));
        assert_eq!(o.duration, Timestamp::from_secs(5), "the page time out");
        assert_eq!(px(&o.ds, 0, 0), WHITE, "the top field");
        assert_eq!(px(&o.ds, 3, 1), GREY, "the bottom field");
        assert_eq!((r.dec.decoded, r.dec.dropped, r.dec.suppressed), (1, 0, 0));
    }

    #[test]
    fn repeated_end_segments_emit_the_page_once() {
        let mut r = Rig::new();
        let mut segs = vec![minimal()];
        segs.extend(std::iter::repeat_n(end(), 2_000));
        let out = r.push(&block(&segs), 1_000);
        assert_eq!(out.len(), 1, "a bare END changes nothing");
        assert_eq!(r.dec.decoded, 1);
    }

    #[test]
    fn two_hundred_fifty_six_clut_ids_do_not_wrap_the_slot() {
        let mut r = Rig::new();
        let mut segs =
            vec![page(5, 0, &[(1, 100, 200)]), region(1, 4, 2, 4, 255, 0, true, &[(7, 0, 0)])];
        segs.extend((0..=255u8).map(|id| clut(id, &[(1, 235, 128, 128, 0)])));
        segs.push(object(7, &four_bit(&[(1, 4)]), &[]));
        segs.push(end());
        let out = r.push(&block(&segs), 0);
        assert_eq!(out.len(), 1);
        assert_eq!(px(&out[0].ds, 0, 0), WHITE, "through CLUT 255, the 256th table");
    }

    #[test]
    fn two_regions_composite_into_their_bounding_box() {
        let mut r = Rig::new();
        let set = block(&[
            page(5, 0, &[(1, 10, 20), (2, 300, 400)]),
            region(1, 4, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            region(2, 6, 2, 4, 2, 0, true, &[(6, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            clut(2, &[(1, 81, 240, 90, 0)]),
            object(5, &four_bit(&[(1, 4)]), &[]),
            object(6, &four_bit(&[(1, 6)]), &[]),
            end(),
        ]);
        let out = r.push(&set, 0);
        assert_eq!(out.len(), 1, "one bitmap for the page");
        let ds = &out[0].ds;
        assert_eq!((ds.x, ds.y, ds.width, ds.height), (10, 20, 296, 382));
        assert_eq!(px(ds, 0, 0), WHITE, "region 1 through CLUT 1");
        assert_eq!(px(ds, 3, 1), WHITE, "an empty bottom field repeats the top one");
        assert_eq!(px(ds, 290, 380), RED, "region 2 through CLUT 2");
        assert_eq!(px(ds, 295, 381), RED);
        assert_eq!(px(ds, 100, 100), CLEAR, "between the regions");
        assert_eq!(px(ds, 4, 0), CLEAR, "right of region 1");
    }

    #[test]
    fn a_region_listed_twice_ends_the_page() {
        let mut r = Rig::new();
        let mut entries = vec![(1, 100, 200), (1, 0, 0), (2, 300, 400)];
        entries.extend(std::iter::repeat_n((1, 0, 0), 10_000));
        let set = block(&[
            page(5, 0, &entries),
            region(1, 4, 2, 4, 1, 0, true, &[(7, 0, 0)]),
            region(2, 4, 2, 4, 1, 0, true, &[(7, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(7, &four_bit(&[(1, 4)]), &[]),
            end(),
        ]);
        let out = r.push(&set, 0);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].ds.x, out[0].ds.y, out[0].ds.width, out[0].ds.height), (100, 200, 4, 2));
        assert_eq!(r.dec.page.len(), 1, "the entries behind the repeat");
        assert_eq!((r.dec.decoded, r.dec.dropped), (1, 0));
    }

    #[test]
    fn all_three_depths_decode_with_their_run_lengths() {
        let runs = [(1, 5), (3, 30)];
        for (depth, string) in [(2u8, two_bit(&runs)), (4, four_bit(&runs)), (8, eight_bit(&runs))]
        {
            let mut r = Rig::new();
            let set = block(&[
                page(2, 0, &[(1, 0, 0)]),
                region(1, 40, 2, depth, 1, 0, true, &[(5, 0, 0)]),
                clut(1, &[(1, 235, 128, 128, 0), (3, 81, 240, 90, 0)]),
                object(5, &string, &string),
                end(),
            ]);
            let mut out = r.push(&set, 0);
            assert_eq!(out.len(), 1, "{depth}-bit");
            let ds = out.remove(0).ds;
            assert_eq!(px(&ds, 4, 0), WHITE, "{depth}-bit, the fifth pixel");
            assert_eq!(px(&ds, 5, 1), RED, "{depth}-bit, the run after it");
            assert_eq!(px(&ds, 34, 0), RED, "{depth}-bit, the run's last pixel");
            assert_eq!(px(&ds, 35, 0), CLEAR, "{depth}-bit, past the run");
            assert_eq!(r.dec.dropped, 0);
        }
    }

    #[test]
    fn map_tables_take_their_defaults_and_their_redefinitions() {
        let mut r = Rig::new();
        let top = [vec![0x20, 0x01, 0x23], two_bit(&[(1, 2)])].concat();
        let set = block(&[
            page(2, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            object(5, &top, &two_bit(&[(1, 2)])),
            end(),
        ]);
        let ds = r.push(&set, 0).remove(0).ds;
        assert_eq!(px(&ds, 0, 0), DEFAULT_CLUT.four[1], "code 1 through the redefined map");
        assert_eq!(
            px(&ds, 0, 1),
            DEFAULT_CLUT.four[7],
            "the next field starts from the default map"
        );

        let mut r = Rig::new();
        let mut table = [0u8; 16];
        table[1] = 0x42;
        let top = [vec![0x22], table.to_vec(), four_bit(&[(1, 2)])].concat();
        let set = block(&[
            page(2, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 8, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(0x42, 81, 240, 90, 0), (0x11, 235, 128, 128, 0)]),
            object(5, &top, &four_bit(&[(1, 2)])),
            end(),
        ]);
        let ds = r.push(&set, 0).remove(0).ds;
        assert_eq!(px(&ds, 0, 0), RED, "entry 0x42 from the redefined map");
        assert_eq!(px(&ds, 0, 1), WHITE, "entry 0x11 from the default map");

        let mut r = Rig::new();
        let top = [vec![0x21, 0, 0x42, 0, 0], two_bit(&[(1, 2)])].concat();
        let set = block(&[
            page(2, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 8, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(0x42, 81, 240, 90, 0)]),
            object(5, &top, &[]),
            end(),
        ]);
        let ds = r.push(&set, 0).remove(0).ds;
        assert_eq!(px(&ds, 1, 0), RED);
    }

    #[test]
    fn the_non_modifying_colour_leaves_the_pixel_under_it() {
        let mut r = Rig::new();
        let string = four_bit(&[(1, 2), (3, 2)]);
        let set = block(&[
            page(2, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 4, 1, 2, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0), (2, 128, 128, 128, 0), (3, 81, 240, 90, 0)]),
            object_with(5, true, &string, &string),
            end(),
        ]);
        let ds = r.push(&set, 0).remove(0).ds;
        assert_eq!(px(&ds, 0, 0), GREY, "entry 1 keeps the background");
        assert_eq!(px(&ds, 2, 0), RED);
    }

    #[test]
    fn an_undefined_clut_is_the_default_one() {
        let mut r = Rig::new();
        let set = block(&[
            page(2, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 4, 9, 0, true, &[(5, 0, 0)]),
            object(5, &four_bit(&[(1, 4)]), &[]),
            end(),
        ]);
        let ds = r.push(&set, 0).remove(0).ds;
        assert_eq!(px(&ds, 0, 0), [255, 0, 0, 255], "table 37 entry 1 is red");
        let two = [[0, 0, 0, 0], [255, 255, 255, 255], [0, 0, 0, 255], [127, 127, 127, 255]];
        assert_eq!(DEFAULT_CLUT.two, two, "table 38");
        assert_eq!(DEFAULT_CLUT.four[8], [0, 0, 0, 255]);
        assert_eq!(DEFAULT_CLUT.four[9], [127, 0, 0, 255]);
        assert_eq!(DEFAULT_CLUT.eight[0], CLEAR);
        assert_eq!(DEFAULT_CLUT.eight[0x01], [255, 0, 0, 63], "75 % transparent red");
        assert_eq!(DEFAULT_CLUT.eight[0x11], [255, 0, 0, 255], "33 % plus 67 %");
        assert_eq!(DEFAULT_CLUT.eight[0x09], [85, 0, 0, 127], "50 % transparent");
        assert_eq!(DEFAULT_CLUT.eight[0x80], [127, 127, 127, 255], "50 % white");
        assert_eq!(DEFAULT_CLUT.eight[0x88], [0, 0, 0, 255]);
        assert_eq!(DEFAULT_CLUT.eight[0xFF], [128, 128, 128, 255]);
    }

    #[test]
    fn the_display_definition_sets_the_grid_and_the_window() {
        let mut r = Rig::new();
        let set = block(&[
            display(1920, 1080, Some((16, 1903, 8, 1071))),
            page(2, 0, &[(1, 100, 50)]),
            region(1, 8, 4, 4, 1, 0, true, &[(5, 0, 0)]),
            object(5, &four_bit(&[(1, 8)]), &[]),
            end(),
        ]);
        let ds = r.push(&set, 0).remove(0).ds;
        assert_eq!((ds.x, ds.y, ds.video_width, ds.video_height), (116, 58, 1920, 1080));
    }

    #[test]
    fn a_mode_change_empties_the_regions_and_an_acquisition_point_keeps_them() {
        let mut r = Rig::new();
        r.push(&minimal(), 0);
        assert!(r.dec.held_pixels() > 0);
        let acquisition = block(&[page(5, 1, &[(1, 100, 200)]), end()]);
        let out = r.push(&acquisition, 1_000);
        assert_eq!(out.len(), 1, "the page shows again");
        assert_eq!(px(&out[0].ds, 0, 0), WHITE, "from the kept region");

        let mode_change = block(&[page(5, 2, &[(1, 100, 200)]), end()]);
        assert!(r.push(&mode_change, 2_000).is_empty(), "region 1 went with the epoch");
        assert_eq!(r.dec.held_pixels(), 0);
        assert_eq!(r.dec.suppressed, 1);
        assert_eq!(r.dec.dropped, 0, "a mode change is not a fault");
    }

    #[test]
    fn a_mode_change_keeps_the_display_set_it_comes_in() {
        let mut r = Rig::new();
        r.push(&minimal(), 0);
        let set = block(&[
            display(1920, 1080, None),
            page(5, 2, &[(1, 100, 900)]),
            region(1, 8, 4, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(5, &four_bit(&[(1, 8)]), &[]),
            end(),
        ]);
        let out = r.push(&set, 1_000);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].ds.x, out[0].ds.y, out[0].ds.video_height), (100, 900, 1080));
        assert_eq!(out[0].duration, Timestamp::from_secs(5));
    }

    #[test]
    fn a_block_at_a_new_time_closes_the_open_set() {
        let mut r = Rig::new();
        let open = block(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            object(5, &four_bit(&[(1, 4)]), &[]),
        ]);
        assert!(r.push(&open, 1_000).is_empty(), "nothing closed it yet");
        let out = r.push(&block(&[page(5, 0, &[(1, 0, 0)])]), 2_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts, Timestamp::from_millis(1_000), "at the time it was built for");
        r.dec.event(&mut r.ctx, &Event::Eos).expect("eos");
        let out = r.take();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts, Timestamp::from_millis(2_000));
    }

    #[test]
    fn an_empty_page_is_a_clear() {
        let mut r = Rig::new();
        r.push(&minimal(), 0);
        let out = r.push(&block(&[page(5, 0, &[]), end()]), 4_000);
        assert_eq!(out.len(), 1);
        assert!(out[0].ds.is_clear());
        assert_eq!((out[0].ds.video_width, out[0].ds.video_height), (720, 576));
        assert_eq!(out[0].pts, Timestamp::from_millis(4_000));
        assert_eq!((r.dec.cleared, r.dec.suppressed), (1, 0));
    }

    #[test]
    fn regions_persist_and_a_set_paints_into_part_of_one() {
        let mut r = Rig::new();
        let first = block(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 8, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0), (2, 81, 240, 90, 0)]),
            object(5, &four_bit(&[(1, 8)]), &[]),
            end(),
        ]);
        r.push(&first, 0);
        let second = block(&[
            region(1, 8, 2, 4, 1, 0, false, &[(6, 0, 0)]),
            object(6, &four_bit(&[(2, 4)]), &[]),
            end(),
        ]);
        let ds = r.push(&second, 1_000).remove(0).ds;
        assert_eq!(px(&ds, 0, 0), RED, "the second set's object");
        assert_eq!(px(&ds, 7, 1), WHITE, "the first set's pixels persist");
    }

    #[test]
    fn the_block_duration_is_held_to_the_page_time_out() {
        let mut r = Rig::new();
        let out = r.push_for(&minimal(), 0, Timestamp::from_secs(2));
        assert_eq!(out[0].duration, Timestamp::from_secs(2), "the shorter of 2 s and 5 s");
        let out = r.push_for(&minimal(), 10_000, Timestamp::from_secs(9));
        assert_eq!(out[0].duration, Timestamp::from_secs(5));
        let no_time_out = block(&[page(0, 0, &[(1, 0, 0)]), end()]);
        let out = r.push(&no_time_out, 20_000);
        assert_eq!(out[0].duration, Timestamp::NONE, "open ended, the overlay's default");
    }

    #[test]
    fn the_codec_private_picks_the_pages_read() {
        let mut r = Rig::new();
        assert!(r.push_head(&[0, 2, 0, 3, 0x14], 0).is_empty());
        let other = block(&[
            segment_on(1, SEG_PAGE, &[5, 0, 1, 0, 0, 0, 0, 0]),
            segment_on(1, SEG_REGION, &region(1, 4, 2, 4, 1, 0, true, &[])[6..]),
            segment_on(1, SEG_END, &[]),
        ]);
        assert!(r.push(&other, 0).is_empty(), "page 1 belongs to another service");
        let mine = block(&[
            segment_on(2, SEG_PAGE, &[5, 0, 1, 0, 0, 0, 0, 0]),
            segment_on(2, SEG_REGION, &region(1, 4, 2, 4, 1, 1, true, &[])[6..]),
            segment_on(3, SEG_END, &[]),
        ]);
        let out = r.push(&mine, 1_000);
        assert_eq!(out.len(), 1, "the composition page, closed on the ancillary one");
        assert_eq!(px(&out[0].ds, 0, 0), [255, 0, 0, 255]);
    }

    #[test]
    fn the_head_is_its_flag_not_its_place() {
        let mine = |page_id| {
            block(&[
                segment_on(page_id, SEG_PAGE, &[5, 0, 1, 0, 0, 0, 0, 0]),
                segment_on(page_id, SEG_REGION, &region(1, 4, 2, 4, 1, 1, true, &[])[6..]),
                segment_on(page_id, SEG_END, &[]),
            ])
        };
        let mut r = Rig::new();
        assert_eq!(r.push(&mine(1), 0).len(), 1, "every page until a head says otherwise");
        // A head mid-stream, as a demuxer greets a decoder joining a running stream.
        assert!(r.push_head(&[0, 2, 0, 2, 0x10], 500).is_empty());
        assert!(r.push(&mine(1), 1_000).is_empty(), "page 1 is now another service's");
        assert_eq!(r.push(&mine(2), 2_000).len(), 1, "the head's page decodes on");
        assert_eq!(r.dec.dropped, 0, "the head was not read as a Block");
        // A first Block of five octets is a Block, refused as one, not a head.
        let mut r = Rig::new();
        assert!(r.push(&[0, 2, 0, 3, 0x14], 0).is_empty());
        assert_eq!((r.dec.pages, r.dec.dropped), (None, 1));
    }

    #[test]
    fn the_pes_data_field_form_is_read_too() {
        let mut r = Rig::new();
        let pes = [vec![0x20, 0x00], minimal(), vec![0xFF]].concat();
        assert_eq!(r.push(&pes, 0).len(), 1);
        assert_eq!(r.dec.dropped, 0);
    }

    #[test]
    fn hostile_blocks_are_counted_and_never_panic() {
        let past_end = [vec![SYNC, SEG_PAGE, 0, 1], 0xFFFFu16.to_be_bytes().to_vec()].concat();
        let huge = block(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 60_000, 60_000, 4, 1, 0, true, &[]),
            end(),
        ]);
        let cases: [(&str, Vec<u8>); 6] = [
            ("not a segment", vec![0x21, 0x00, 0x0F]),
            ("a header past the end", vec![SYNC, SEG_PAGE, 0, 1]),
            ("a segment past the end", past_end),
            ("a region past any display", huge),
            ("a CLUT entry for no CLUT", segment(SEG_CLUT, &[1, 0, 1, 0x01, 235, 128, 128, 0])),
            ("fields past the segment", segment(SEG_OBJECT, &[0, 5, 0, 0, 9, 0, 9, 0x11])),
        ];
        for (what, bytes) in cases {
            let mut r = Rig::new();
            assert!(r.push(&bytes, 0).is_empty(), "{what}");
            assert_eq!(r.dec.dropped, 1, "{what}");
            assert_eq!(r.push(&minimal(), 100).len(), 1, "{what}, the next set decodes");
        }

        let mut r = Rig::new();
        let mut character = vec![1u8, 1 << 3, 0, 8, 0, 4, 2 << 2, 1, 0, 0];
        character.extend_from_slice(&[0, 5, 0x40, 0, 0, 0]);
        let set = block(&[
            page(5, 0, &[(1, 0, 0)]),
            segment(SEG_REGION, &character),
            object(99, &four_bit(&[(1, 4)]), &[]),
            end(),
        ]);
        assert_eq!(r.push(&set, 0).len(), 1);
        assert_eq!(r.dec.dropped, 0);

        let mut r = Rig::new();
        let noise: Vec<u8> =
            (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        r.push(&[vec![SYNC], noise].concat(), 0);
        let fresh = [display(720, 576, None), minimal()].concat();
        assert_eq!(
            r.push(&fresh, 100).iter().filter(|o| !o.ds.is_clear()).count(),
            1,
            "it recovered"
        );
    }

    #[test]
    fn regions_past_the_budget_are_a_counted_fault() {
        let mut r = Rig::new();
        let set = block(&[
            page(5, 0, &[(1, 0, 0), (2, 0, 0)]),
            region(1, 4096, 4096, 4, 1, 0, true, &[]),
            region(2, 4096, 4096, 4, 1, 0, true, &[]),
            end(),
        ]);
        assert!(r.push(&set, 0).is_empty());
        assert_eq!((r.dec.dropped, r.dec.budget_faults), (1, 1));
        assert!(r.dec.held_pixels() <= PIXEL_BUDGET);
        assert_eq!(r.push(&minimal(), 100).len(), 1);
    }

    #[test]
    fn regions_nobody_shows_give_way_to_one_that_is_shown() {
        let mut r = Rig::new();
        for id in 10..14u8 {
            let set = block(&[page(5, 0, &[]), region(id, 4096, 1024, 4, 1, 0, true, &[]), end()]);
            r.push(&set, u64::from(id) * 100);
        }
        assert_eq!(r.dec.held_pixels(), PIXEL_BUDGET);
        assert_eq!(r.push(&minimal(), 5_000).len(), 1, "the shown region found room");
        assert_eq!(r.dec.dropped, 0, "giving way is not a fault");
        assert!(r.dec.held_pixels() < PIXEL_BUDGET);
    }

    #[test]
    fn an_object_placed_past_the_paint_budget_is_a_counted_fault() {
        let mut r = Rig::new();
        let objects: Vec<(u16, u16, u16)> = (0..10_000).map(|_| (5, 0, 0)).collect();
        let line = [two_bit(&[(1, 1), (2, 1)].repeat(32)), eol()].concat();
        let set = block(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 64, 64, 2, 1, 0, true, &objects),
            object(5, &line.repeat(32), &[]),
            end(),
        ]);
        r.push(&set, 0);
        assert_eq!((r.dec.dropped, r.dec.budget_faults), (1, 1));
    }

    #[test]
    fn a_resized_region_takes_a_new_span_and_compaction_keeps_the_others() {
        let mut r = Rig::new();
        let set = |w: u16| {
            block(&[
                page(5, 0, &[(1, 0, 0), (2, 0, 100)]),
                region(1, w, 2, 4, 1, 0, true, &[(5, 0, 0)]),
                region(2, 4, 2, 4, 1, 0, true, &[(6, 0, 0)]),
                clut(1, &[(1, 235, 128, 128, 0), (2, 81, 240, 90, 0)]),
                object(5, &four_bit(&[(1, w.into())]), &[]),
                object(6, &four_bit(&[(2, 4)]), &[]),
                end(),
            ])
        };
        r.push(&set(4), 0);
        for (i, w) in [8u16, 16, 4, 32].into_iter().enumerate() {
            let ds = r.push(&set(w), (i as u64 + 1) * 1_000).remove(0).ds;
            assert_eq!(px(&ds, u32::from(w) - 1, 0), WHITE, "width {w}");
            assert_eq!(px(&ds, 0, 100), RED, "region 2 survived");
        }
        let before = r.dec.held_pixels();
        compact(&mut r.dec.pixels, &mut r.dec.regions, PIX, 256);
        assert!(r.dec.held_pixels() < before, "the old spans were garbage");
        assert_eq!(r.dec.held_pixels(), 32 * 2 + 4 * 2);
        let ds =
            r.push(&block(&[page(5, 0, &[(1, 0, 0), (2, 0, 100)]), end()]), 9_000).remove(0).ds;
        assert_eq!(
            (px(&ds, 31, 1), px(&ds, 3, 101)),
            (WHITE, RED),
            "the pixels moved with their spans"
        );
    }

    #[test]
    fn a_flush_forgets_the_state_and_the_family_is_said_at_start() {
        let mut h = Harness::new(DvbSubDec::new());
        let mut b = h.alloc(&minimal());
        b.pts = Timestamp::from_millis(1_000);
        h.push(b).expect("push");
        assert_eq!(h.drain_outputs().len(), 1);
        h.push_event(Event::FlushStart).expect("flush");
        let mut b = h.alloc(&block(&[object(7, &four_bit(&[(1, 4)]), &[]), end()]));
        b.pts = Timestamp::from_millis(2_000);
        h.push(b).expect("push");
        assert!(h.drain_outputs().is_empty(), "no page survived the flush");
        let said = h.announced().expect("the family");
        assert_eq!(h.vocabulary().family_name(said.family), Some(BITMAP_FAMILY));
    }
}
