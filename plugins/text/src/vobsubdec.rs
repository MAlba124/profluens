//! `vobsubdec`, decode a VobSub (DVD sub-picture) track to the `subtitle/bitmap` family.

use profluens_core::batch::Inputs;
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

/// The VobSub family the mkv demuxer announces for an `S_VOBSUB` track.
pub const VOBSUB_FAMILY: &str = "subtitle/vobsub";

const CMD_FSTA_DSP: u8 = 0x00;
const CMD_STA_DSP: u8 = 0x01;
const CMD_STP_DSP: u8 = 0x02;
const CMD_SET_COLOR: u8 = 0x03;
const CMD_SET_CONTR: u8 = 0x04;
const CMD_SET_DAREA: u8 = 0x05;
const CMD_SET_DSPXA: u8 = 0x06;
const CMD_CHG_COLCON: u8 = 0x07;
const CMD_END: u8 = 0xFF;

const SPU_HEADER: usize = 4;
const DCSQ_HEADER: usize = 4;

const DEFAULT_GRID: (u32, u32) = (720, 480);
const TALL_GRID_HEIGHT: u32 = 576;

/// RGBA bytes one picture may take.
pub const MAX_BITMAP_BYTES: usize = 16 << 20;

type Rgba = [u8; 4];

/// Why a unit was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VobSubError {
    Malformed(&'static str),
    Budget(&'static str),
}

impl VobSubError {
    pub fn reason(self) -> &'static str {
        match self {
            VobSubError::Malformed(r) | VobSubError::Budget(r) => r,
        }
    }
}

impl std::fmt::Display for VobSubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VobSubError::Malformed(r) => write!(f, "vobsub malformed, {r}"),
            VobSubError::Budget(r) => write!(f, "vobsub over budget, {r}"),
        }
    }
}

impl std::error::Error for VobSubError {}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Area {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

#[derive(Clone, Copy, Default)]
struct Display {
    colour: [u8; 4],
    alpha: [u8; 4],
    area: Area,
    fields: [usize; 2],
}

struct Plan {
    start: u64,
    stop: Option<u64>,
    show: Display,
}

static SINK_OFFERS: [OfferDesc; 2] = [OfferDesc::any(VOBSUB_FAMILY), OfferDesc::any("bytes")];
static SRC_OFFERS: [OfferDesc; 2] = [OfferDesc::any(BITMAP_FAMILY), OfferDesc::any("bytes")];
static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &SINK_OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &SRC_OFFERS }];

static DESC: ElementDesc = ElementDesc {
    name: "vobsubdec",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    // Cold, registry make_default, boxes one element at plugin registration.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(VobSubDec::new())),
    io: false,
    couplings: &[],
};

/// Decodes a VobSub track to `subtitle/bitmap`.
pub struct VobSubDec {
    clut: Option<[Rgba; 16]>,
    grid: Option<(u32, u32)>,
    awaiting_head: bool,
    failing: bool,
    /// Bitmaps put out.
    pub decoded: u64,
    /// Units dropped as malformed or past the budget.
    pub dropped: u64,
    /// Of those, the ones past the budget.
    pub budget_faults: u64,
    /// Units whose schedule never showed a picture.
    pub blank: u64,
}

impl Default for VobSubDec {
    fn default() -> Self {
        Self::new()
    }
}

impl VobSubDec {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self {
            clut: None,
            grid: None,
            awaiting_head: true,
            failing: false,
            decoded: 0,
            dropped: 0,
            budget_faults: 0,
            blank: 0,
        }
    }

    /// Reads the `palette:` and `size:` lines of an `.idx` text.
    pub fn set_idx(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("palette:") {
                let mut clut = [[0, 0, 0, 255]; 16];
                let mut n = 0usize;
                for (slot, entry) in clut.iter_mut().zip(rest.split(',')) {
                    let Ok(v) = u32::from_str_radix(entry.trim(), 16) else {
                        break;
                    };
                    *slot = [(v >> 16) as u8, (v >> 8) as u8, v as u8, 255];
                    n += 1;
                }
                if n > 0 {
                    self.clut = Some(clut);
                }
            } else if let Some(rest) = line.strip_prefix("size:") {
                let mut parts = rest.trim().split(['x', 'X']).map(|v| v.trim().parse::<u32>());
                if let (Some(Ok(w)), Some(Ok(h))) = (parts.next(), parts.next())
                    && (1..=4096).contains(&w)
                    && (1..=4096).contains(&h)
                {
                    self.grid = Some((w, h));
                }
            }
        }
    }

    fn fail(&mut self, ctx: &Ctx, e: VobSubError) {
        self.dropped += 1;
        if matches!(e, VobSubError::Budget(_)) {
            self.budget_faults += 1;
        }
        if self.failing {
            log!(ctx, Level::Debug, "vobsub_drop", reason = e.reason());
        } else {
            log!(ctx, Level::Warn, "vobsub_drop", reason = e.reason());
            self.failing = true;
        }
    }

    /// The four colours of a picture. The 4-bit alpha widens to 8 bits as `a << 4 | a`.
    fn palette(&self, d: &Display) -> [Rgba; 4] {
        let mut out = [[0u8; 4]; 4];
        let mut visible = 0u32;
        for (e, px) in out.iter_mut().enumerate() {
            let a = d.alpha[e] & 0x0F;
            let alpha = (a << 4) | a;
            *px = match self.clut {
                Some(clut) => {
                    let [r, g, b, _] = clut[usize::from(d.colour[e] & 0x0F)];
                    [r, g, b, alpha]
                }
                None if alpha > 0 => {
                    let v = 255u32.saturating_sub(128 * visible) as u8;
                    visible += 1;
                    [v, v, v, alpha]
                }
                None => [0, 0, 0, 0],
            };
        }
        out
    }

    fn grid_for(&self, area: Area) -> (u32, u32) {
        match self.grid {
            Some(grid) => grid,
            None if area.bottom >= DEFAULT_GRID.1 => (DEFAULT_GRID.0, TALL_GRID_HEIGHT),
            None => DEFAULT_GRID,
        }
    }

    fn ingest(&mut self, ctx: &mut Ctx, data: &[u8], pts: Timestamp, duration: Timestamp) {
        if std::mem::take(&mut self.awaiting_head)
            && let Some(text) = idx_text(data)
        {
            self.set_idx(text);
            self.log_idx(ctx);
            return;
        }
        if let Err(e) = self.decode(ctx, data, pts, duration) {
            match idx_text(data) {
                Some(text) => {
                    self.set_idx(text);
                    self.log_idx(ctx);
                }
                None => self.fail(ctx, e),
            }
        }
    }

    fn log_idx(&self, ctx: &Ctx) {
        let (w, h) = self.grid.unwrap_or((0, 0));
        log!(ctx, Level::Debug, "idx", palette = self.clut.is_some(), width = w, height = h);
    }

    fn decode(
        &mut self,
        ctx: &mut Ctx,
        data: &[u8],
        pts: Timestamp,
        duration: Timestamp,
    ) -> Result<(), VobSubError> {
        if data.len() <= 2 {
            return Ok(());
        }
        let (unit, plan) = plan(data)?;
        let Some(plan) = plan else {
            self.blank += 1;
            log!(&*ctx, Level::Trace, "blank", pts = pts);
            return Ok(());
        };
        let a = plan.show.area;
        if plan.stop == Some(plan.start) || a.right < a.left || a.bottom < a.top {
            self.blank += 1;
            log!(&*ctx, Level::Trace, "blank", pts = pts);
            return Ok(());
        }
        if self.clut.is_none() && self.decoded == 0 {
            log!(&*ctx, Level::Warn, "no_palette", fallback = "grey ramp");
        }
        let (w, h) = ((a.right - a.left + 1) as usize, (a.bottom - a.top + 1) as usize);
        let bytes = w * h * 4;
        if bytes > MAX_BITMAP_BYTES {
            return Err(VobSubError::Budget("a picture past the decoder's budget"));
        }
        let start = pts.saturating_add(ticks(plan.start));
        let shown = match (plan.stop, duration.nanos()) {
            (Some(stop), _) => ticks(stop).saturating_sub(ticks(plan.start)),
            (None, Some(_)) => pts.saturating_add(duration).saturating_sub(start),
            (None, None) => Timestamp::NONE,
        };
        let (gw, gh) = self.grid_for(a);
        let palette = self.palette(&plan.show);
        let (w32, h32) = (w as u32, h as u32);
        let n = payload_len(&[(w32, h32)])
            .ok_or(VobSubError::Budget("a picture past the decoder's budget"))?;
        let mut buf =
            ctx.alloc_exact(n).ok_or(VobSubError::Budget("a picture past the exact cap"))?;
        let out = &mut buf.memory.as_mut_full()[..n];
        let slot = VobSubError::Budget("a picture past the slot");
        let mut b = BitmapBuilder::new(out, gw, gh, 1).map_err(|_| slot)?;
        let px = b.rect(a.left, a.top, w32, h32).map_err(|_| slot)?;
        render(unit, plan.show.fields, &palette, px, w, h);
        let n = b.finish().map_err(|_| slot)?;
        buf.memory.set_len(n);
        buf.pts = start;
        buf.duration = if shown.nanos() == Some(0) { Timestamp::NONE } else { shown };
        ctx.out(Self::OUT).push(buf);
        self.decoded += 1;
        self.failing = false;
        Ok(())
    }
}

impl Element for VobSubDec {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.announce_format(Self::OUT, BITMAP_FAMILY, &[]);
        (self.awaiting_head, self.failing) = (true, false);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            self.ingest(ctx, buf.memory.data(), buf.pts, buf.duration);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart | Event::FormatChange(_) => {
                let flush = matches!(event, Event::FlushStart);
                log!(&*ctx, Level::Debug, "rehead", flush = flush, dropped = self.dropped);
                self.awaiting_head = true;
            }
            Event::Eos => log!(
                &*ctx,
                Level::Info,
                "eos",
                decoded = self.decoded,
                dropped = self.dropped,
                budget_faults = self.budget_faults,
                blank = self.blank,
            ),
            Event::Gap { .. } => ctx.push_event(Self::OUT, event.clone()),
            _ => {}
        }
        Ok(())
    }
}

fn idx_text(data: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(data).ok()?;
    text.lines()
        .map(str::trim_start)
        .any(|l| l.starts_with("palette:") || l.starts_with("size:"))
        .then_some(text)
}

/// Delay ticks of 1024/90000 s to a time.
fn ticks(t: u64) -> Timestamp {
    Timestamp::from_nanos(t * 1024 * 1_000_000_000 / 90_000)
}

fn plan(data: &[u8]) -> Result<(&[u8], Option<Plan>), VobSubError> {
    if data.len() < SPU_HEADER {
        return Err(VobSubError::Malformed("a unit shorter than its header"));
    }
    // The declared size wins where shorter, container padding, and every offset is in it.
    let unit = match usize::from(be16(&data[0..2])) {
        0 => data,
        n if n < SPU_HEADER => {
            return Err(VobSubError::Malformed("a unit declaring less than its header"));
        }
        n if n <= data.len() => &data[..n],
        _ => {
            return Err(VobSubError::Malformed("a unit claiming more bytes than it has"));
        }
    };
    let mut at = usize::from(be16(&unit[2..4]));
    let mut d = Display::default();
    let (mut start, mut stop): (Option<(u64, Display)>, Option<u64>) = (None, None);
    let (mut late, mut blocks) = (0u64, 0usize);
    loop {
        let Some(head) = unit.get(at..at + DCSQ_HEADER) else {
            return Err(VobSubError::Malformed("a control sequence past the end of its unit"));
        };
        // More sequences than room for them is a loop.
        blocks += 1;
        if blocks > unit.len() / DCSQ_HEADER + 1 {
            return Err(VobSubError::Malformed("a control chain that never ends"));
        }
        let (delay, next) = (u64::from(be16(&head[0..2])), usize::from(be16(&head[2..4])));
        late = late.max(delay);
        // The chain only moves forward, and a sequence ends where the next one starts, so a
        // unit is scanned once however its offsets are arranged.
        let forward = next > at;
        let end = if forward { next.min(unit.len()) } else { unit.len() };
        let (show, hide) = commands(&unit[(at + DCSQ_HEADER).min(end)..end], &mut d)?;
        if hide {
            if start.is_some() {
                stop = Some(late);
            }
        } else if show && start.is_none() {
            start = Some((late, d));
        }
        if stop.is_some() || next == at || next == 0 {
            break;
        }
        if !forward {
            return Err(VobSubError::Malformed("a control chain that runs backwards"));
        }
        at = next;
    }
    Ok((unit, start.map(|(start, show)| Plan { start, stop, show })))
}

fn commands(block: &[u8], d: &mut Display) -> Result<(bool, bool), VobSubError> {
    let arg = |at: usize, n: usize| {
        block
            .get(at + 1..at + 1 + n)
            .ok_or(VobSubError::Malformed("a command past the end of its sequence"))
    };
    let (mut show, mut hide, mut at) = (false, false, 0usize);
    while at < block.len() {
        match block[at] {
            CMD_FSTA_DSP | CMD_STA_DSP => {
                show = true;
                at += 1;
            }
            CMD_STP_DSP => {
                hide = true;
                at += 1;
            }
            // Emphasis 2 and 1 in the first byte, pattern and background in the second.
            CMD_SET_COLOR | CMD_SET_CONTR => {
                let f = arg(at, 2)?;
                let v = [f[1] & 0x0F, f[1] >> 4, f[0] & 0x0F, f[0] >> 4];
                if block[at] == CMD_SET_COLOR {
                    d.colour = v;
                } else {
                    d.alpha = v;
                }
                at += 3;
            }
            // Left and right, then top and bottom, 12 bits each.
            CMD_SET_DAREA => {
                let f = arg(at, 6)?;
                let b = |i: usize| u32::from(f[i]);
                d.area = Area {
                    left: (b(0) << 4) | (b(1) >> 4),
                    right: ((b(1) & 0x0F) << 8) | b(2),
                    top: (b(3) << 4) | (b(4) >> 4),
                    bottom: ((b(4) & 0x0F) << 8) | b(5),
                };
                at += 7;
            }
            CMD_SET_DSPXA => {
                let f = arg(at, 4)?;
                d.fields = [usize::from(be16(&f[0..2])), usize::from(be16(&f[2..4]))];
                at += 5;
            }
            // Its size counts from its own first byte.
            CMD_CHG_COLCON => {
                let size = usize::from(be16(arg(at, 2)?));
                if size < 2 || at + 1 + size > block.len() {
                    return Err(VobSubError::Malformed(
                        "a colour change past the end of its sequence",
                    ));
                }
                at += 1 + size;
            }
            CMD_END => break,
            _ => break,
        }
    }
    Ok((show, hide))
}

fn render(
    unit: &[u8],
    fields: [usize; 2],
    palette: &[Rgba; 4],
    out: &mut [u8],
    w: usize,
    h: usize,
) {
    let mut nibble = [fields[0] * 2, fields[1] * 2];
    for (row, line) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
        let at = &mut nibble[row & 1];
        // Every line starts byte aligned.
        *at = at.div_ceil(2) * 2;
        let mut x = 0usize;
        while x < w {
            let code = rle_code(unit, at);
            let run = match code >> 2 {
                0 => w - x,
                n => (n as usize).min(w - x),
            };
            let c = palette[(code & 3) as usize];
            for px in line[x * 4..(x + run) * 4].as_chunks_mut::<4>().0 {
                px.copy_from_slice(&c);
            }
            x += run;
        }
    }
}

/// One code of one to four nibbles. A nibble of 4 or more ends it, else the next is appended,
/// until the value reaches 0x04, 0x10, 0x40.
fn rle_code(unit: &[u8], at: &mut usize) -> u32 {
    let mut code = u32::from(nibble(unit, at));
    for floor in [0x4, 0x10, 0x40] {
        if code >= floor {
            break;
        }
        code = (code << 4) | u32::from(nibble(unit, at));
    }
    code
}

fn nibble(unit: &[u8], at: &mut usize) -> u8 {
    let byte = unit.get(*at / 2).copied().unwrap_or(0);
    let n = if *at & 1 == 1 { byte & 0x0F } else { byte >> 4 };
    *at += 1;
    n
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// Hand-built sub-picture units, for the tests here and in `tests/`.
#[doc(hidden)]
#[allow(clippy::disallowed_methods)] // test vectors, heap-built on purpose
pub mod fixtures {
    use super::{
        CMD_CHG_COLCON, CMD_END, CMD_SET_COLOR, CMD_SET_CONTR, CMD_SET_DAREA, CMD_SET_DSPXA,
        CMD_STA_DSP, CMD_STP_DSP, SPU_HEADER,
    };

    /// An `.idx` CodecPrivate as a Matroska muxer writes it.
    pub const IDX: &[u8] =
        b"size: 720x480\npalette: 0d00ee, ee450d, 101010, ebebeb, 0ce60b, ec14ed, \
ebff0b, 0d637e, a1a1a1, c5c5c5, 0e640c, 89db89, 0e0089, a2bdd4, ebcf0b, 7e127e\nforced subs: OFF\n";

    /// One DCSQ, its delay in ticks and its commands, the 0xFF added.
    pub struct Dcsq {
        pub delay: u16,
        pub commands: Vec<u8>,
    }

    /// Where the last DCSQ points.
    pub enum Last {
        Itself,
        Zero,
        To(usize),
    }

    pub fn show() -> Vec<u8> {
        vec![CMD_STA_DSP]
    }

    pub fn hide() -> Vec<u8> {
        vec![CMD_STP_DSP]
    }

    /// Palette indices of background, pattern, emphasis 1 and 2.
    pub fn colours(e: [u8; 4]) -> Vec<u8> {
        vec![CMD_SET_COLOR, (e[3] << 4) | e[2], (e[1] << 4) | e[0]]
    }

    pub fn alphas(e: [u8; 4]) -> Vec<u8> {
        vec![CMD_SET_CONTR, (e[3] << 4) | e[2], (e[1] << 4) | e[0]]
    }

    /// The inclusive display area.
    pub fn area(left: u32, top: u32, right: u32, bottom: u32) -> Vec<u8> {
        vec![
            CMD_SET_DAREA,
            (left >> 4) as u8,
            (((left & 0x0F) << 4) | (right >> 8)) as u8,
            right as u8,
            (top >> 4) as u8,
            (((top & 0x0F) << 4) | (bottom >> 8)) as u8,
            bottom as u8,
        ]
    }

    /// The two fields' byte offsets.
    pub fn fields(top: u16, bottom: u16) -> Vec<u8> {
        let mut out = vec![CMD_SET_DSPXA];
        out.extend_from_slice(&top.to_be_bytes());
        out.extend_from_slice(&bottom.to_be_bytes());
        out
    }

    /// A colour change command around `body`, its size counting its own two bytes.
    pub fn colour_change(body: &[u8]) -> Vec<u8> {
        let mut out = vec![CMD_CHG_COLCON];
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    /// The nibbles of one run, `run << 2 | colour`, a zero run filling the line.
    pub fn code(colour: u8, run: u16) -> Vec<u8> {
        let v = (u32::from(run) << 2) | u32::from(colour & 3);
        let n = match run {
            1..=3 => 1,
            4..=15 => 2,
            16..=63 => 3,
            _ => 4,
        };
        (0..n).rev().map(|i| ((v >> (4 * i)) & 0xF) as u8).collect()
    }

    /// One field's lines of `(colour, run)` codes, each line padded to a byte.
    pub fn field(lines: &[&[(u8, u16)]]) -> Vec<u8> {
        let mut nibbles = Vec::new();
        for line in lines {
            for &(c, run) in line.iter() {
                nibbles.extend(code(c, run));
            }
            if nibbles.len() % 2 == 1 {
                nibbles.push(0);
            }
        }
        nibbles.chunks(2).map(|p| (p[0] << 4) | p[1]).collect()
    }

    /// The header, the two fields and the DCSQ chain.
    pub fn unit_with(top: &[u8], bottom: &[u8], chain: &[Dcsq], last: Last) -> Vec<u8> {
        let mut out = vec![0u8; SPU_HEADER];
        out.extend_from_slice(top);
        out.extend_from_slice(bottom);
        let control = out.len();
        let mut starts = Vec::new();
        let mut at = control;
        for d in chain {
            starts.push(at);
            at += 4 + d.commands.len() + 1;
        }
        for (i, d) in chain.iter().enumerate() {
            let next = match (i + 1 < chain.len(), &last) {
                (true, _) => starts[i + 1],
                (false, Last::Itself) => starts[i],
                (false, Last::Zero) => 0,
                (false, Last::To(j)) => starts[*j],
            };
            out.extend_from_slice(&d.delay.to_be_bytes());
            out.extend_from_slice(&(next as u16).to_be_bytes());
            out.extend_from_slice(&d.commands);
            out.push(CMD_END);
        }
        let size = out.len() as u16;
        out[0..2].copy_from_slice(&size.to_be_bytes());
        out[2..4].copy_from_slice(&(control as u16).to_be_bytes());
        out
    }

    /// The commands that set up a picture over `area`, its fields right after the header.
    pub fn setup(top: &[u8], alpha: [u8; 4], a: (u32, u32, u32, u32)) -> Vec<u8> {
        let at = SPU_HEADER as u16;
        [
            colours([0, 1, 2, 3]),
            alphas(alpha),
            area(a.0, a.1, a.2, a.3),
            fields(at, at + top.len() as u16),
        ]
        .concat()
    }

    /// A 4x2 picture at (100, 200), entry 1 on top and 2 below, hidden 45 ticks (512 ms) on.
    pub fn minimal() -> Vec<u8> {
        let (top, bottom) = (field(&[&[(1, 4)]]), field(&[&[(2, 4)]]));
        let chain = [
            Dcsq {
                delay: 0,
                commands: [setup(&top, [0, 15, 15, 15], (100, 200, 103, 201)), show()].concat(),
            },
            Dcsq { delay: 45, commands: hide() },
        ];
        unit_with(&top, &bottom, &chain, Last::Itself)
    }

    /// A 4x2 picture at (0, 0) that is never hidden.
    pub fn never_hidden() -> Vec<u8> {
        let top = field(&[&[(1, 4)]]);
        let chain = [Dcsq {
            delay: 0,
            commands: [setup(&top, [0, 15, 15, 15], (0, 0, 3, 1)), show()].concat(),
        }];
        unit_with(&top, &top, &chain, Last::Itself)
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
        dec: VobSubDec,
        ctx: Ctx,
        _bus: Bus,
    }

    struct Out {
        ds: DisplaySet,
        pts: Timestamp,
        duration: Timestamp,
    }

    impl Rig {
        fn new(idx: Option<&[u8]>) -> Self {
            let (tx, bus) = Bus::channel();
            let mut ctx = Ctx::new(Pool::new(4096), tx, ElementId(0), FormatId(0), 1);
            let mut dec = VobSubDec::new();
            dec.start(&mut ctx).expect("start");
            let mut rig = Rig { dec, ctx, _bus: bus };
            if let Some(idx) = idx {
                assert!(rig.push(idx, 0).is_empty(), "the head is no picture");
            }
            rig
        }

        fn push(&mut self, bytes: &[u8], ms: u64) -> Vec<Out> {
            self.push_for(bytes, ms, Timestamp::NONE)
        }

        fn push_for(&mut self, bytes: &[u8], ms: u64, duration: Timestamp) -> Vec<Out> {
            self.dec.ingest(&mut self.ctx, bytes, Timestamp::from_millis(ms), duration);
            let mut batch = self.ctx.take_output(VobSubDec::OUT);
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

    #[test]
    fn the_idx_palette_is_rgb_and_its_size_the_grid() {
        let mut dec = VobSubDec::new();
        dec.set_idx(std::str::from_utf8(IDX).expect("text"));
        assert_eq!(dec.grid, Some((720, 480)));
        let clut = dec.clut.expect("a palette");
        assert_eq!(clut[0], [0x0d, 0x00, 0xee, 0xff]);
        assert_eq!(clut[2], [0x10, 0x10, 0x10, 0xff]);
        assert_eq!(clut[15], [0x7e, 0x12, 0x7e, 0xff]);
        dec.set_idx("palette: zz\nsize: 0x0\n");
        assert_eq!((dec.clut, dec.grid), (Some(clut), Some((720, 480))));
    }

    #[test]
    fn show_and_hide_give_the_time_and_the_duration() {
        let mut r = Rig::new(Some(IDX));
        let out = r.push(&minimal(), 1_000);
        assert_eq!(out.len(), 1);
        let o = &out[0];
        assert_eq!(o.pts, Timestamp::from_millis(1_000));
        assert_eq!(o.duration, Timestamp::from_millis(512), "45 ticks of 1024/90000 s");
        assert_eq!((o.ds.x, o.ds.y, o.ds.width, o.ds.height), (100, 200, 4, 2));
        assert_eq!((o.ds.video_width, o.ds.video_height), (720, 480), "the size line");
        assert_eq!((r.dec.decoded, r.dec.dropped), (1, 0));
    }

    #[test]
    fn a_later_show_starts_later() {
        let mut r = Rig::new(Some(IDX));
        let top = field(&[&[(1, 4)]]);
        let chain = [
            Dcsq { delay: 0, commands: setup(&top, [0, 15, 15, 15], (0, 0, 3, 1)) },
            Dcsq { delay: 90, commands: show() },
            Dcsq { delay: 180, commands: hide() },
        ];
        let out = r.push(&unit_with(&top, &top, &chain, Last::Itself), 1_000);
        assert_eq!(out[0].pts, Timestamp::from_nanos(1_000_000_000 + 1_024_000_000));
        assert_eq!(out[0].duration, Timestamp::from_nanos(1_024_000_000));
    }

    #[test]
    fn the_fields_interleave_and_the_alpha_stays_straight() {
        let mut r = Rig::new(Some(IDX));
        let (top, bottom) = (field(&[&[(1, 4)], &[(1, 4)]]), field(&[&[(2, 4)], &[(2, 4)]]));
        let chain = [Dcsq {
            delay: 0,
            commands: [setup(&top, [0, 8, 15, 15], (0, 0, 3, 3)), show()].concat(),
        }];
        let ds = r.push(&unit_with(&top, &bottom, &chain, Last::Itself), 0).remove(0).ds;
        assert_eq!(
            px(&ds, 0, 0),
            [0xee, 0x45, 0x0d, 0x88],
            "entry 1 at alpha 8, not premultiplied"
        );
        assert_eq!(px(&ds, 0, 1), [0x10, 0x10, 0x10, 0xff], "line 1 from the second field");
        assert_eq!(px(&ds, 3, 2), [0xee, 0x45, 0x0d, 0x88]);
        assert_eq!(px(&ds, 3, 3), [0x10, 0x10, 0x10, 0xff]);
    }

    #[test]
    fn without_a_palette_the_colours_are_a_grey_ramp() {
        let mut r = Rig::new(None);
        let ds = r.push(&minimal(), 0).remove(0).ds;
        assert_eq!(px(&ds, 0, 0), [255, 255, 255, 255], "the first visible entry");
        assert_eq!(px(&ds, 0, 1), [127, 127, 127, 255], "the second");
        assert_eq!((ds.video_width, ds.video_height), (720, 480));
    }

    #[test]
    fn a_zero_run_ends_the_line_and_every_line_starts_on_a_byte() {
        let mut r = Rig::new(Some(IDX));
        let top = field(&[&[(1, 3), (2, 0)], &[(3, 10)]]);
        let bottom = field(&[&[(2, 0)], &[(1, 0)]]);
        let chain = [Dcsq {
            delay: 0,
            commands: [setup(&top, [15, 15, 15, 15], (0, 0, 9, 3)), show()].concat(),
        }];
        let ds = r.push(&unit_with(&top, &bottom, &chain, Last::Itself), 0).remove(0).ds;
        let clut = r.dec.clut.expect("palette");
        let c = |i: usize| [clut[i][0], clut[i][1], clut[i][2], 255];
        assert_eq!((px(&ds, 2, 0), px(&ds, 3, 0), px(&ds, 9, 0)), (c(1), c(2), c(2)), "line 0");
        assert_eq!((px(&ds, 0, 2), px(&ds, 9, 2)), (c(3), c(3)), "line 2, read from its byte");
        assert_eq!((px(&ds, 0, 1), px(&ds, 9, 3)), (c(2), c(1)), "the second field");
    }

    #[test]
    fn a_truncated_picture_finishes_its_lines_transparent() {
        let mut r = Rig::new(Some(IDX));
        let top = field(&[&[(1, 2)]]);
        let commands = [
            colours([0, 1, 2, 3]),
            alphas([0, 15, 15, 15]),
            area(0, 0, 7, 3),
            fields(4, 0x7FFF),
            show(),
        ];
        let chain = [Dcsq { delay: 0, commands: commands.concat() }];
        let ds = r.push(&unit_with(&top, &[], &chain, Last::Itself), 0).remove(0).ds;
        assert_eq!(px(&ds, 1, 0)[3], 255);
        assert_eq!(px(&ds, 7, 3), [0x0d, 0x00, 0xee, 0], "entry 0, alpha 0");
        assert_eq!(r.dec.dropped, 0);
    }

    #[test]
    fn the_chain_ends_at_itself_or_at_zero_and_a_loop_is_a_fault() {
        let top = field(&[&[(1, 4)]]);
        let first = || Dcsq {
            delay: 0,
            commands: [setup(&top, [0, 15, 15, 15], (0, 0, 3, 1)), show()].concat(),
        };
        let mut r = Rig::new(Some(IDX));
        let zero = unit_with(&top, &top, &[first()], Last::Zero);
        assert_eq!(r.push(&zero, 0).len(), 1, "offset zero ends the chain");
        let looped =
            unit_with(&top, &top, &[first(), Dcsq { delay: 5, commands: vec![] }], Last::To(0));
        assert!(r.push(&looped, 0).is_empty());
        assert_eq!(r.dec.dropped, 1, "a chain that never ends");
        assert_eq!(r.push(&minimal(), 100).len(), 1, "the next unit stands alone");
    }

    #[test]
    fn without_a_hide_the_block_duration_ends_it() {
        let mut r = Rig::new(Some(IDX));
        let out = r.push_for(&never_hidden(), 1_000, Timestamp::from_millis(800));
        assert_eq!(out[0].duration, Timestamp::from_millis(800));
        let out = r.push(&never_hidden(), 3_000);
        assert_eq!(out[0].duration, Timestamp::NONE, "open ended, the overlay's default");
    }

    #[test]
    fn a_schedule_that_runs_backwards_hides_at_the_show() {
        let mut r = Rig::new(Some(IDX));
        let top = field(&[&[(1, 4)]]);
        let chain = [
            Dcsq {
                delay: 900,
                commands: [setup(&top, [0, 15, 15, 15], (0, 0, 3, 1)), show()].concat(),
            },
            Dcsq { delay: 10, commands: hide() },
        ];
        assert!(r.push(&unit_with(&top, &top, &chain, Last::Itself), 0).is_empty());
        assert_eq!((r.dec.blank, r.dec.dropped), (1, 0));
    }

    #[test]
    fn a_colour_change_is_stepped_over() {
        let mut r = Rig::new(Some(IDX));
        let top = field(&[&[(1, 4)]]);
        let change =
            colour_change(&[0, 0, 0x10, 1, 0, 2, 0x30, 0x10, 0xFF, 0xF0, 0x0F, 0xFF, 0xFF, 0xFF]);
        let commands = [
            colours([0, 1, 2, 3]),
            change,
            alphas([0, 9, 15, 15]),
            area(0, 0, 3, 1),
            fields(4, 4),
            show(),
        ];
        let chain = [Dcsq { delay: 0, commands: commands.concat() }];
        let ds = r.push(&unit_with(&top, &[], &chain, Last::Itself), 0).remove(0).ds;
        assert_eq!(px(&ds, 0, 0)[3], 0x99, "the alpha set after the change applies");
    }

    #[test]
    fn the_grid_widens_to_pal_without_a_size() {
        let mut r = Rig::new(None);
        let top = field(&[&[(1, 4)]]);
        let chain = [Dcsq {
            delay: 0,
            commands: [setup(&top, [0, 15, 15, 15], (0, 500, 3, 501)), show()].concat(),
        }];
        let ds = r.push(&unit_with(&top, &top, &chain, Last::Itself), 0).remove(0).ds;
        assert_eq!((ds.video_width, ds.video_height), (720, 576));
    }

    #[test]
    fn hostile_units_are_counted_and_never_panic() {
        let cases: [(&str, Vec<u8>); 5] = [
            ("control past the end", vec![0, 8, 0xFF, 0xFF, 0, 0, 0, 0]),
            ("more bytes than it has", vec![0xFF, 0xFF, 0, 4, 0, 0]),
            ("less than its header", vec![0, 3, 0, 4, 0, 0, 0, 0]),
            ("a command cut short", vec![0, 9, 0, 4, 0, 0, 0, 4, CMD_SET_DAREA]),
            (
                "a colour change past its sequence",
                vec![0, 11, 0, 4, 0, 0, 0, 4, CMD_CHG_COLCON, 0xFF, 0xFF],
            ),
        ];
        for (what, bytes) in cases {
            let mut r = Rig::new(Some(IDX));
            assert!(r.push(&bytes, 0).is_empty(), "{what}");
            assert_eq!(r.dec.dropped, 1, "{what}");
            assert_eq!(r.push(&minimal(), 100).len(), 1, "{what}, the next unit decodes");
        }

        let mut r = Rig::new(Some(IDX));
        let top = field(&[&[(1, 4)]]);
        let chain = [Dcsq {
            delay: 0,
            commands: [setup(&top, [0, 15, 15, 15], (0, 0, 4095, 4095)), show()].concat(),
        }];
        assert!(r.push(&unit_with(&top, &[], &chain, Last::Itself), 0).is_empty());
        assert_eq!((r.dec.dropped, r.dec.budget_faults), (1, 1));

        let mut r = Rig::new(Some(IDX));
        assert!(r.push(&[0, 0], 0).is_empty());
        assert_eq!(r.dec.dropped, 0);

        let mut r = Rig::new(Some(IDX));
        let noise: Vec<u8> =
            (0..4096u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        r.push(&noise, 0);
        assert_eq!(r.push(&minimal(), 100).len(), 1, "a unit stands alone");
    }

    #[test]
    fn the_head_comes_first_and_again_behind_a_flush() {
        let mut h = Harness::new(VobSubDec::new());
        let tall = b"size: 720x576\npalette: 000000, ff0000, 00ff00, 0000ff\n";
        let b = h_alloc(&mut h, tall, 0);
        h.push(b).expect("head");
        let b = h_alloc(&mut h, &minimal(), 1_000);
        h.push(b).expect("unit");
        let out = h.drain_outputs();
        assert_eq!(out.len(), 1, "the head is no picture");
        let ds = decode_bitmap(out[0].memory.data()).expect("bitmap");
        assert_eq!((ds.video_width, ds.video_height), (720, 576));
        assert_eq!(px(&ds, 0, 0), [255, 0, 0, 255], "palette entry 1 from the head");

        h.push_event(Event::FlushStart).expect("flush");
        let b = h_alloc(&mut h, IDX, 2_000);
        h.push(b).expect("head again");
        let b = h_alloc(&mut h, &minimal(), 2_000);
        h.push(b).expect("unit");
        let ds = decode_bitmap(h.drain_outputs()[0].memory.data()).expect("bitmap");
        assert_eq!(px(&ds, 0, 0), [0xee, 0x45, 0x0d, 0xff], "the new head's palette");
        let said = h.announced().expect("the family");
        assert_eq!(h.vocabulary().family_name(said.family), Some(BITMAP_FAMILY));
    }

    fn h_alloc(h: &mut Harness, bytes: &[u8], ms: u64) -> profluens_core::buffer::Buffer {
        let mut b = h.alloc(bytes);
        b.pts = Timestamp::from_millis(ms);
        b
    }
}
