//! `suboverlay` burns the active subtitle cues onto decoded video frames, bottom-centre.
//! It never blocks on the text or image pads, the video pad is the metronome.

use profluens_core::batch::Inputs;
use profluens_core::couple::Coupling;
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
use profluens_video::format::PixelFormat;
use profluens_video::frame::VideoFrameMut;

use crate::bitmap::{BitmapRect, BitmapRef};
use crate::font;
use crate::font_data;
use pf_subparse::CueIr;

/// On-screen span for a cue whose container dropped its duration.
pub const DEFAULT_CUE_NS: u64 = 3_000_000_000;

/// Slack past a cue's end before eviction, so a slightly early frame still shows it.
const EVICT_SLACK_NS: u64 = 200_000_000;
/// Spare caption bytes kept across captions regardless of the caption size.
const SPARE_BITMAP_KEEP: usize = 1 << 20;

/// How far a picture may lead a subtitle stream that reports gaps before it waits.
/// Above the gap spacing `mkvdemux` leaves, half a second.
const SUB_SLACK: Timestamp = Timestamp::from_millis(1_000);

const MAX_PENDING_CUES: usize = 4096;

const RESEND_SCAN: usize = 32;

const TEXT_HEIGHT_FRAC: f32 = 1.0 / 12.0;

static VIDEO_OFFERS: [OfferDesc; 1] = [OfferDesc::any("video/raw")];
static TEXT_OFFERS: [OfferDesc; 2] =
    [OfferDesc::any(pf_subparse::CUE_IR_FAMILY), OfferDesc::any("bytes")];
static IMAGE_OFFERS: [OfferDesc; 2] =
    [OfferDesc::any(crate::BITMAP_FAMILY), OfferDesc::any("bytes")];

static SINKS: [PadDesc; 3] = [
    PadDesc {
        name: "video",
        offers: &VIDEO_OFFERS, // upstream decoder announces its dimensions/pixfmt at runtime
    },
    PadDesc { name: "text", offers: &TEXT_OFFERS },
    PadDesc { name: "image", offers: &IMAGE_OFFERS },
];
static SRCS: [PadDesc; 1] = [PadDesc {
    name: "src",
    offers: &VIDEO_OFFERS, // same format as the video sink, announced/forwarded at runtime
}];

static DESC: ElementDesc = ElementDesc {
    name: "suboverlay",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    // A per-frame CPU blit exceeds the passive budget.
    sched: SchedHint::Active,
    // Never wait for a cue that may be seconds away.
    inputs: InputPolicy::Any,
    latency: LatencyDesc::NONE,
    // Cold, registry make_default, boxes one element instance at plugin-registration time.
    #[allow(clippy::disallowed_methods)]
    make_default: Some(|| Box::new(SubtitleOverlay::new())),
    io: false,
    couplings: &[Coupling::through(0, 0)],
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct RingCue {
    start: Timestamp,
    end: Timestamp,
    text: String,
}

/// The latest bitmap caption, its validated `subtitle/bitmap` payload copied out of the buffer.
#[derive(Clone, Debug)]
struct BitmapCue {
    start: Timestamp,
    end: Timestamp,
    bytes: Vec<u8>,
}

/// Spare cue strings kept past this many are freed.
const SPARE_TEXTS: usize = 64;

#[derive(Default)]
struct CaptionCache {
    key: String,
    key_scratch: String,
    size_px: f32,
    line_h: usize,
    lines: Vec<font::Bitmap>,
    clip: (usize, usize),
}

/// Burns active subtitle cues onto decoded video frames, bottom-centre.
pub struct SubtitleOverlay {
    cues: Vec<RingCue>,
    /// Strings of evicted cues, reused so a cue costs no allocation in steady state.
    spare_texts: Vec<String>,
    bitmap: Option<BitmapCue>,
    /// The last bitmap's buffer, reused by the next.
    spare_bitmap: Vec<u8>,
    warned_no_geom: bool,
    warned_ring_full: bool,
    warned_malformed: bool,
    warned_short: bool,
    caption: CaptionCache,
    /// Cues dropped because the timeline already held `MAX_PENDING_CUES` (health counter).
    pub dropped_cues: u64,
    /// Text payloads the `subtitle/cue-ir` view refused (health counter).
    pub malformed_cues: u64,
    /// Frames composited / passed through (health counters, mirroring the mosaic's).
    pub emitted: u64,
    pub with_caption: u64,
    /// Frames onto which a bitmap caption was composited (health counter).
    pub with_bitmap: u64,
    shown: Timestamp,
    cut: Option<Timestamp>,
    sub_until: Timestamp,
}

impl Default for SubtitleOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl SubtitleOverlay {
    /// The video frames, which are the metronome.
    pub const VIDEO: SinkPad = SinkPad(0);
    /// The cue feed.
    pub const TEXT: SinkPad = SinkPad(1);
    /// The bitmap caption feed.
    pub const IMAGE: SinkPad = SinkPad(2);
    pub const OUT: SrcPad = SrcPad(0);

    // Cold, one-time constructor, the empty cue ring (Vec::new() = zero capacity, no heap yet).
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self {
            cues: Vec::new(),
            spare_texts: Vec::new(),
            bitmap: None,
            spare_bitmap: Vec::new(),
            warned_no_geom: false,
            warned_ring_full: false,
            warned_malformed: false,
            warned_short: false,
            caption: CaptionCache::default(),
            dropped_cues: 0,
            malformed_cues: 0,
            emitted: 0,
            with_caption: 0,
            with_bitmap: 0,
            shown: Timestamp::NONE,
            cut: None,
            sub_until: Timestamp::NONE,
        }
    }

    /// A cue's text buffer, recycled from an evicted cue when one is spare.
    fn text_buffer(&mut self) -> String {
        let mut text = self.spare_texts.pop().unwrap_or_default();
        text.clear();
        text
    }

    fn spare(&mut self, text: String) {
        if self.spare_texts.len() < SPARE_TEXTS {
            self.spare_texts.push(text);
        }
    }

    fn push_cue(&mut self, text: String, pts: Timestamp, duration: Timestamp) {
        let Some(start_ns) = pts.nanos() else {
            self.spare(text);
            return;
        };
        if text.trim().is_empty() {
            self.spare(text);
            return;
        }
        let dur_ns = match duration.nanos() {
            Some(d) if d > 0 => d,
            _ => DEFAULT_CUE_NS, // container dropped BlockDuration, bounded fallback
        };
        let start = Timestamp::from_nanos(start_ns);
        let end = Timestamp::from_nanos(start_ns.saturating_add(dur_ns));
        let cue = RingCue { start, end, text };
        let at = self.cues.partition_point(|c| c.start <= cue.start);
        // Drop an exact re-send (a replayed Block, a seek, a looping source) so a line is not
        // stacked twice.
        let resent = self.cues[..at]
            .iter()
            .rev()
            .take(RESEND_SCAN)
            .take_while(|c| c.start == cue.start)
            .any(|c| *c == cue);
        if resent || self.cues.len() >= MAX_PENDING_CUES {
            self.dropped_cues += u64::from(!resent);
            self.spare(cue.text);
            return;
        }
        self.cues.insert(at, cue);
    }

    fn flush(&mut self) {
        for c in self.cues.drain(..) {
            if self.spare_texts.len() < SPARE_TEXTS {
                self.spare_texts.push(c.text);
            }
        }
        self.clear_bitmap();
        self.caption.key.clear();
        self.caption.lines.clear();
    }

    fn clear_bitmap(&mut self) {
        if let Some(b) = self.bitmap.take() {
            self.spare_bitmap = b.bytes;
        }
    }

    fn evict_stale(&mut self, now: Timestamp) {
        let Some(now_ns) = now.nanos() else { return };
        let cutoff = now_ns.saturating_sub(EVICT_SLACK_NS);
        let spare = &mut self.spare_texts;
        self.cues.retain_mut(|c| {
            let live = c.end.nanos().map(|e| e > cutoff).unwrap_or(true);
            if !live && spare.len() < SPARE_TEXTS {
                spare.push(std::mem::take(&mut c.text));
            }
            live
        });
    }

    #[cfg(test)]
    fn active_text(&self, pts: Timestamp) -> Vec<&str> {
        self.cues
            .iter()
            .filter(|c| pts >= c.start && pts < c.end)
            .map(|c| c.text.as_str())
            .collect()
    }

    #[cfg(test)]
    fn refresh_caption(&mut self, pts: Timestamp, frame_h: usize) -> bool {
        self.refresh_caption_within(pts, usize::MAX, frame_h)
    }

    fn refresh_caption_within(&mut self, pts: Timestamp, frame_w: usize, frame_h: usize) -> bool {
        let px = (frame_h as f32 * TEXT_HEIGHT_FRAC).max(8.0);
        let size = font::size_nearest(px);
        let clip = caption_clip(size, frame_w, frame_h);
        // Moved out so `self.cues` can be read while it is written, moved back below.
        let mut key = std::mem::take(&mut self.caption.key_scratch);
        key.clear();
        for c in self.cues.iter().filter(|c| pts >= c.start && pts < c.end) {
            if !key.is_empty() {
                key.push('\n');
            }
            key.push_str(&c.text);
        }
        if key != self.caption.key || self.caption.size_px != size.px || self.caption.clip != clip {
            let (max_chars, max_lines) = clip;
            self.caption.lines.clear();
            // The lines nearest the bottom margin are the ones that fit.
            let lines = key.rsplit('\n').filter(|line| !line.trim().is_empty());
            for line in lines.take(max_lines) {
                let shown = match line.char_indices().nth(max_chars) {
                    Some((cut, _)) => &line[..cut],
                    None => line,
                };
                self.caption.lines.push(font::rasterize_line(size, shown));
            }
            self.caption.lines.reverse();
            self.caption.key.clear();
            self.caption.key.push_str(&key);
            self.caption.size_px = size.px;
            self.caption.clip = clip;
            self.caption.line_h = font::line_height(size);
        }
        self.caption.key_scratch = key;
        !self.caption.lines.is_empty()
    }

    /// Keep a validated bitmap payload as the caption, a clear drops the current one.
    fn push_bitmap(&mut self, image: &BitmapRef<'_>, pts: Timestamp, duration: Timestamp) {
        if image.is_clear() {
            self.clear_bitmap();
            return;
        }
        let Some(start_ns) = pts.nanos() else { return };
        let dur_ns = match duration.nanos() {
            Some(d) if d > 0 => d,
            _ => DEFAULT_CUE_NS,
        };
        let start = Timestamp::from_nanos(start_ns);
        let end = Timestamp::from_nanos(start_ns.saturating_add(dur_ns));
        self.clear_bitmap();
        let mut bytes = std::mem::take(&mut self.spare_bitmap);
        bytes.clear();
        // A spare far larger than the caption is one big caption's memory, let it go.
        if bytes.capacity() > image.as_bytes().len().saturating_mul(4).max(SPARE_BITMAP_KEEP) {
            bytes.shrink_to(0);
        }
        // The one copy of a sparse caption, into a buffer kept across captions.
        #[allow(clippy::disallowed_methods)]
        bytes.extend_from_slice(image.as_bytes());
        self.bitmap = Some(BitmapCue { start, end, bytes });
    }

    /// The bitmap on screen at `pts`, validated when it came in.
    fn active_bitmap(&mut self, pts: Timestamp) -> Option<BitmapRef<'_>> {
        if let Some(b) = &self.bitmap {
            let past = pts
                .nanos()
                .zip(b.end.nanos())
                .map(|(now, end)| now > end.saturating_add(EVICT_SLACK_NS))
                .unwrap_or(false);
            if past {
                self.clear_bitmap();
            }
        }
        let b = self.bitmap.as_ref().filter(|b| pts >= b.start && pts < b.end)?;
        BitmapRef::parse(&b.bytes).ok()
    }
}

impl SubtitleOverlay {
    fn heard(&mut self, pts: Timestamp) {
        if self.sub_until.is_some() && pts.is_some() {
            self.sub_until = self.sub_until.max(pts);
        }
    }

    /// One `subtitle/cue-ir` payload from the `text` pad. Its plain text goes into a recycled
    /// string, so a cue allocates nothing once the ring has seen its size.
    pub fn ingest_text(&mut self, payload: &[u8], pts: Timestamp, duration: Timestamp) {
        let view = match CueIr::parse(payload) {
            Ok(view) => view,
            Err(_) => {
                self.malformed_cues += 1;
                return;
            }
        };
        let mut text = self.text_buffer();
        for line in 0..view.line_count() {
            if line != 0 {
                text.push('\n');
            }
            text.extend(view.line_text(line));
        }
        self.push_cue(text, pts, duration);
    }

    /// One `subtitle/bitmap` payload from the `image` pad.
    pub fn ingest_bitmap(&mut self, payload: &[u8], pts: Timestamp, duration: Timestamp) {
        match BitmapRef::parse(payload) {
            Ok(image) => self.push_bitmap(&image, pts, duration),
            Err(_) => self.malformed_cues += 1,
        }
    }

    /// Text cues held in the timeline, received and not yet expired.
    pub fn pending_cues(&self) -> usize {
        self.cues.len()
    }

    /// Composite the captions on screen at `pts` onto one `w` x `h` 4:2:0 frame held in `data`,
    /// evicting expired cues first.
    pub fn composite_frame(
        &mut self,
        data: &mut [u8],
        w: u32,
        h: u32,
        pixfmt: PixelFormat,
        pts: Timestamp,
    ) {
        self.evict_stale(pts);
        self.composite(pts, w, h, pixfmt, move || data);
    }

    /// `true` when a caption was due but the frame is too short to draw on.
    fn composite<'a>(
        &mut self,
        pts: Timestamp,
        w: u32,
        h: u32,
        pixfmt: PixelFormat,
        frame: impl FnOnce() -> &'a mut [u8],
    ) -> bool {
        let text = self.refresh_caption_within(pts, w as usize, h as usize);
        let bitmap = self.active_bitmap(pts).is_some();
        if !text && !bitmap {
            return false;
        }
        let data = frame();
        // Checked before the plane sizes are summed, which a geometry near `u32::MAX` squared would
        // overflow.
        if (w as u64).saturating_mul(h as u64) > data.len() as u64 {
            return true;
        }
        let need = profluens_video::geometry::frame_size(pixfmt, w, h);
        if data.len() < need {
            return true;
        }
        let Some(mut frame) = VideoFrameMut::new(&mut data[..need], w, h, pixfmt) else {
            return true;
        };
        if text {
            draw_caption(&mut frame, &self.caption.lines, self.caption.line_h);
            self.with_caption += 1;
        }
        if bitmap
            && let Some(b) = self.bitmap.as_ref()
            && let Ok(image) = BitmapRef::parse(&b.bytes)
            && draw_bitmap(&mut frame, &image)
        {
            self.with_bitmap += 1;
        }
        false
    }
}

impl Element for SubtitleOverlay {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.flush();
        self.warned_no_geom = false;
        self.warned_ring_full = false;
        self.warned_malformed = false;
        self.warned_short = false;
        self.caption = CaptionCache::default();
        self.dropped_cues = 0;
        self.emitted = 0;
        self.with_caption = 0;
        self.with_bitmap = 0;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        let mut text_batch = ctx.take_input_on(Self::TEXT);
        while let Some(buf) = text_batch.pop_front() {
            self.ingest_text(buf.memory.data(), buf.pts, buf.duration);
            self.heard(buf.pts);
        }
        ctx.recycle_input(text_batch);
        if self.dropped_cues > 0 && !self.warned_ring_full {
            self.warned_ring_full = true;
            log!(&*ctx, Level::Warn, "cue_timeline_full", limit = MAX_PENDING_CUES as u64);
        }

        let mut image_batch = ctx.take_input_on(Self::IMAGE);
        while let Some(buf) = image_batch.pop_front() {
            self.ingest_bitmap(buf.memory.data(), buf.pts, buf.duration);
            self.heard(buf.pts);
        }
        ctx.recycle_input(image_batch);
        if self.malformed_cues > 0 && !self.warned_malformed {
            self.warned_malformed = true;
            log!(&*ctx, Level::Warn, "cue_malformed", count = self.malformed_cues);
        }

        let mut forwarded = false;
        // Wait for a subtitle stream more than `SUB_SLACK` behind.
        while let Some(pts) =
            ctx.front_input_on(Self::VIDEO).or_else(|| inputs.front()).map(|b| b.pts)
        {
            let open = !ctx.is_pad_closed(Self::TEXT) || !ctx.is_pad_closed(Self::IMAGE);
            if open
                && self.sub_until.is_some()
                && pts.is_some()
                && pts > self.sub_until.saturating_add(SUB_SLACK)
            {
                break;
            }
            let Some(mut buf) = ctx.pop_input_on(Self::VIDEO).or_else(|| inputs.pop()) else {
                break;
            };
            // After a video track switch the pictures up to the last one of the old track are not
            // shown again.
            if let Some(cut) = self.cut.filter(|_| buf.pts.is_some()) {
                if buf.pts <= cut {
                    log!(&*ctx, Level::Trace, "cut", pts = buf.pts);
                    ctx.count_drop();
                    continue;
                }
                self.cut = None;
            }
            if buf.pts.is_some() {
                self.shown = buf.pts;
            }
            if !std::mem::replace(&mut forwarded, true) {
                let came = ctx
                    .negotiated_sink(Self::VIDEO)
                    .filter(|f| Some(*f) != ctx.negotiated_src(Self::OUT));
                if let Some(fixed) = came.cloned() {
                    log!(&*ctx, Level::Debug, "format_forward", pts = buf.pts);
                    ctx.forward_format(Self::OUT, fixed);
                }
            }

            self.evict_stale(buf.pts);

            let geom = negotiated_video(ctx);
            // Without geometry the element is a silent passthrough, so say so once.
            if geom.is_none()
                && !self.warned_no_geom
                && (!self.cues.is_empty() || self.bitmap.is_some())
            {
                self.warned_no_geom = true;
                log!(
                    &*ctx,
                    Level::Warn,
                    "no_video_geometry",
                    detail = "video sink caps carry no usable width/height/pixfmt, \
                              subtitles cannot be composited and frames pass through unchanged",
                );
            }

            // Made writable only when a caption is on screen, since `as_mut_full` copies a shared
            // backing.
            if let Some((w, h, pixfmt)) = geom {
                let pts = buf.pts;
                let memory = &mut buf.memory;
                let short = self.composite(pts, w, h, pixfmt, move || memory.as_mut_full());
                if short && !std::mem::replace(&mut self.warned_short, true) {
                    log!(&*ctx, Level::Warn, "frame_short", width = w, height = h, pts = pts);
                }
            }
            let out_pts = buf.pts;
            let mut out = buf;
            out.pts = out_pts;
            ctx.out(Self::OUT).push(out);
            self.emitted += 1;
        }
        Ok(Flow::Ok)
    }

    /// A flush drops the cue timeline, the demuxer replays Blocks from the new position.
    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        match event {
            Event::FlushStart => {
                log!(&*ctx, Level::Debug, "flush", cues = self.cues.len());
                self.flush();
                (self.shown, self.cut, self.sub_until) = (Timestamp::NONE, None, Timestamp::NONE);
            }
            Event::Gap { until } => {
                self.sub_until =
                    if self.sub_until.is_some() { self.sub_until.max(*until) } else { *until };
            }
            Event::SinkLinked { pad, on: true } if *pad == Self::VIDEO => {
                log!(&*ctx, Level::Debug, "video_relinked", cut = self.shown);
                self.cut = self.shown.is_some().then_some(self.shown);
            }
            Event::SinkLinked { pad, .. } if *pad == Self::TEXT => {
                log!(&*ctx, Level::Debug, "text_relinked", cues = self.cues.len());
                self.cues.clear();
                self.caption.key.clear();
                self.caption.lines.clear();
                self.sub_until = Timestamp::NONE;
            }
            Event::SinkLinked { pad, .. } if *pad == Self::IMAGE => {
                log!(&*ctx, Level::Debug, "image_relinked");
                (self.bitmap, self.sub_until) = (None, Timestamp::NONE);
            }
            Event::Eos => log!(
                &*ctx,
                Level::Info,
                "eos",
                emitted = self.emitted,
                with_caption = self.with_caption,
                with_bitmap = self.with_bitmap,
                dropped_cues = self.dropped_cues,
            ),
            _ => {}
        }
        Ok(())
    }
}

fn negotiated_video(ctx: &Ctx) -> Option<(u32, u32, PixelFormat)> {
    use profluens_core::format::Value;
    let fixed = ctx.negotiated_sink(SubtitleOverlay::VIDEO)?;
    let int = |name: &str| -> Option<i64> {
        ctx.field_id(name).and_then(|id| fixed.get(id)).and_then(|v| match v {
            Value::Int(n) => Some(n),
            _ => None,
        })
    };
    let (w, h) = (int("width")?, int("height")?);
    // A size past `u32` must not wrap into a plausible one.
    if w <= 0 || h <= 0 || w > u32::MAX as i64 || h > u32::MAX as i64 {
        return None;
    }
    let pixfmt = ctx
        .field_id("pixfmt")
        .and_then(|id| fixed.get(id))
        .and_then(|v| match v {
            Value::Id(vid) => ctx.value_name(vid),
            _ => None,
        })
        .and_then(PixelFormat::from_caps_name)?;
    // 8-bit 4:2:0 only. A packed format would mis-address chroma, p010 would split a sample.
    if !pixfmt.is_subsampled_420() || pixfmt.bytes_per_sample() != 1 {
        return None;
    }
    Some((w as u32, h as u32, pixfmt))
}

/// Nominal white in BT.601/709 limited range. The outline is nominal black, 16.
const TEXT_LUMA: u8 = 235;
const OUTLINE_LUMA: u8 = 16;
const NEUTRAL_CHROMA: u8 = 128;

fn caption_margin(line_h: usize) -> usize {
    line_h / 2 + 2
}

fn caption_clip(size: &font_data::SizeMetrics, frame_w: usize, frame_h: usize) -> (usize, usize) {
    let line_h = font::line_height(size).max(1);
    let chars = ((frame_w as f32 / size.advance).ceil() as usize).saturating_add(1);
    let lines = (frame_h.saturating_sub(caption_margin(line_h)) / line_h).max(1);
    (chars, lines)
}

fn draw_caption(frame: &mut VideoFrameMut<'_>, lines: &[font::Bitmap], line_h: usize) {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    if fw == 0 || fh == 0 || line_h == 0 {
        return;
    }
    let margin = caption_margin(line_h);
    let n = lines.len();
    for (i, bm) in lines.iter().enumerate() {
        if bm.w == 0 || bm.h == 0 {
            continue;
        }
        let x0 = ((fw as i64 - bm.w as i64) / 2).max(0) as usize;
        let lines_from_bottom = n - 1 - i;
        let y_bottom = fh.saturating_sub(margin + lines_from_bottom * line_h);
        let y0 = y_bottom.saturating_sub(bm.h);
        blit_coverage(frame, bm, x0, y0);
    }
}

fn blit_coverage(frame: &mut VideoFrameMut<'_>, bm: &font::Bitmap, x0: usize, y0: usize) {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let pixfmt = frame.pixfmt();

    if let Some((y_plane, y_stride)) = frame.plane_mut(0) {
        for dy in 0..bm.h {
            let py = y0 + dy;
            if py >= fh {
                break;
            }
            for dx in 0..bm.w {
                let px = x0 + dx;
                if px >= fw {
                    break;
                }
                let mut dil = 0u8;
                for oy in -1i32..=1 {
                    for ox in -1i32..=1 {
                        let sx = dx as i32 + ox;
                        let sy = dy as i32 + oy;
                        if sx >= 0 && sy >= 0 {
                            dil = dil.max(bm.at(sx as usize, sy as usize));
                        }
                    }
                }
                if dil == 0 {
                    continue;
                }
                let idx = py * y_stride + px;
                y_plane[idx] = over(y_plane[idx], OUTLINE_LUMA, dil);
            }
        }
        for dy in 0..bm.h {
            let py = y0 + dy;
            if py >= fh {
                break;
            }
            for dx in 0..bm.w {
                let px = x0 + dx;
                if px >= fw {
                    break;
                }
                let a = bm.at(dx, dy);
                if a == 0 {
                    continue;
                }
                let idx = py * y_stride + px;
                y_plane[idx] = over(y_plane[idx], TEXT_LUMA, a);
            }
        }
    }

    let (cx0, cx1, cy0, cy1) = chroma_box(x0, y0, bm.w, bm.h, fw, fh);
    match pixfmt {
        PixelFormat::I420 => {
            for plane in 1..=2 {
                if let Some((c_plane, c_stride)) = frame.plane_mut(plane) {
                    paint_chroma_i420(c_plane, c_stride, bm, x0, y0, (cx0, cx1, cy0, cy1));
                }
            }
        }
        PixelFormat::Nv12 => {
            if let Some((c_plane, c_stride)) = frame.plane_mut(1) {
                paint_chroma_nv12(c_plane, c_stride, bm, x0, y0, (cx0, cx1, cy0, cy1));
            }
        }
        _ => {}
    }
}

fn chroma_box(
    x0: usize,
    y0: usize,
    bw: usize,
    bh: usize,
    fw: usize,
    fh: usize,
) -> (usize, usize, usize, usize) {
    let (cw, ch) = (fw.div_ceil(2), fh.div_ceil(2));
    let cx0 = x0.saturating_sub(2) / 2;
    let cy0 = y0.saturating_sub(2) / 2;
    let cx1 = (x0 + bw + 2).div_ceil(2).saturating_add(1).min(cw);
    let cy1 = (y0 + bh + 2).div_ceil(2).saturating_add(1).min(ch);
    (cx0.min(cx1), cx1, cy0.min(cy1), cy1)
}

fn paint_chroma_i420(
    plane: &mut [u8],
    stride: usize,
    bm: &font::Bitmap,
    x0: usize,
    y0: usize,
    bounds: (usize, usize, usize, usize),
) {
    let (cx0, cx1, cy0, cy1) = bounds;
    for cy in cy0..cy1 {
        for cx in cx0..cx1 {
            let a = chroma_coverage(bm, x0, y0, cx, cy);
            if a == 0 {
                continue;
            }
            let idx = cy * stride + cx;
            if idx < plane.len() {
                plane[idx] = over(plane[idx], NEUTRAL_CHROMA, a);
            }
        }
    }
}

fn paint_chroma_nv12(
    plane: &mut [u8],
    stride: usize,
    bm: &font::Bitmap,
    x0: usize,
    y0: usize,
    bounds: (usize, usize, usize, usize),
) {
    let (cx0, cx1, cy0, cy1) = bounds;
    for cy in cy0..cy1 {
        for cx in cx0..cx1 {
            let a = chroma_coverage(bm, x0, y0, cx, cy);
            if a == 0 {
                continue;
            }
            let base = cy * stride + cx * 2; // Cb then Cr
            if base + 1 < plane.len() {
                plane[base] = over(plane[base], NEUTRAL_CHROMA, a);
                plane[base + 1] = over(plane[base + 1], NEUTRAL_CHROMA, a);
            }
        }
    }
}

fn chroma_coverage(bm: &font::Bitmap, x0: usize, y0: usize, cx: usize, cy: usize) -> u8 {
    let mut a = 0u8;
    for ly in (2 * cy)..(2 * cy + 2) {
        for lx in (2 * cx)..(2 * cx + 2) {
            let bx = lx as i64 - x0 as i64;
            let by = ly as i64 - y0 as i64;
            if bx < 0 || by < 0 {
                continue;
            }
            for oy in -1i32..=1 {
                for ox in -1i32..=1 {
                    let sx = bx + ox as i64;
                    let sy = by + oy as i64;
                    if sx >= 0 && sy >= 0 {
                        a = a.max(bm.at(sx as usize, sy as usize));
                    }
                }
            }
        }
    }
    a
}

#[inline]
fn over(dst: u8, src: u8, a: u8) -> u8 {
    let a = a as u32;
    let out = (src as u32 * a + dst as u32 * (255 - a) + 127) / 255;
    out as u8
}

/// Every rect of a validated bitmap, `true` when any pixel landed.
fn draw_bitmap(frame: &mut VideoFrameMut<'_>, image: &BitmapRef<'_>) -> bool {
    let mut drew = false;
    for rect in image.rects() {
        drew |= draw_rect(frame, image.video_width(), image.video_height(), rect, image.rgba(rect));
    }
    drew
}

fn draw_rect(
    frame: &mut VideoFrameMut<'_>,
    video_width: u32,
    video_height: u32,
    rect: &BitmapRect,
    rgba: &[u8],
) -> bool {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    if fw == 0 || fh == 0 || rect.width == 0 || rect.height == 0 || rgba.is_empty() {
        return false;
    }
    // A composition with no reference size is taken to be in frame space.
    let ref_w = if video_width == 0 { fw } else { video_width as usize };
    let ref_h = if video_height == 0 { fh } else { video_height as usize };
    if ref_w == 0 || ref_h == 0 {
        return false;
    }

    let dst_x0 = rect.x as usize * fw / ref_w;
    let dst_y0 = rect.y as usize * fh / ref_h;
    let dst_w = (rect.width as usize * fw).div_ceil(ref_w);
    let dst_h = (rect.height as usize * fh).div_ceil(ref_h);
    if dst_w == 0 || dst_h == 0 {
        return false;
    }

    let sample = |dx: usize, dy: usize| -> Option<[u8; 4]> {
        let sx = dx * rect.width as usize / dst_w;
        let sy = dy * rect.height as usize / dst_h;
        let si = (sy * rect.width as usize + sx) * 4;
        rgba.get(si..si + 4).map(|p| [p[0], p[1], p[2], p[3]])
    };

    let mut drew = false;
    let pixfmt = frame.pixfmt();

    if let Some((y_plane, y_stride)) = frame.plane_mut(0) {
        for dy in 0..dst_h {
            let py = dst_y0 + dy;
            if py >= fh {
                break;
            }
            for dx in 0..dst_w {
                let px = dst_x0 + dx;
                if px >= fw {
                    break;
                }
                let Some([r, g, b, a]) = sample(dx, dy) else {
                    continue;
                };
                if a == 0 {
                    continue;
                }
                let (yv, _, _) = rgb_to_ycbcr_bt709(r, g, b);
                let idx = py * y_stride + px;
                if idx < y_plane.len() {
                    y_plane[idx] = over(y_plane[idx], yv, a);
                    drew = true;
                }
            }
        }
    }

    match pixfmt {
        PixelFormat::I420 => {
            blit_bitmap_chroma_i420(frame, dst_x0, dst_y0, dst_w, dst_h, &sample);
        }
        PixelFormat::Nv12 => {
            blit_bitmap_chroma_nv12(frame, dst_x0, dst_y0, dst_w, dst_h, &sample);
        }
        _ => {}
    }
    drew
}

fn bitmap_chroma_box(
    dst_x0: usize,
    dst_y0: usize,
    dst_w: usize,
    dst_h: usize,
    fw: usize,
    fh: usize,
) -> (usize, usize, usize, usize) {
    let (cw, ch) = (fw.div_ceil(2), fh.div_ceil(2));
    let cx0 = (dst_x0 / 2).min(cw);
    let cy0 = (dst_y0 / 2).min(ch);
    let cx1 = dst_x0.saturating_add(dst_w).div_ceil(2).min(cw);
    let cy1 = dst_y0.saturating_add(dst_h).div_ceil(2).min(ch);
    (cx0.min(cx1), cx1, cy0.min(cy1), cy1)
}

#[allow(clippy::too_many_arguments)]
fn blit_bitmap_chroma_i420(
    frame: &mut VideoFrameMut<'_>,
    dst_x0: usize,
    dst_y0: usize,
    dst_w: usize,
    dst_h: usize,
    sample: &impl Fn(usize, usize) -> Option<[u8; 4]>,
) {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let (cx0, cx1, cy0, cy1) = bitmap_chroma_box(dst_x0, dst_y0, dst_w, dst_h, fw, fh);
    for plane in 1..=2 {
        let is_cr = plane == 2;
        if let Some((c_plane, c_stride)) = frame.plane_mut(plane) {
            for cy in cy0..cy1 {
                for cx in cx0..cx1 {
                    let (lx, ly) = (2 * cx, 2 * cy);
                    if lx < dst_x0 || ly < dst_y0 {
                        continue;
                    }
                    let (dx, dy) = (lx - dst_x0, ly - dst_y0);
                    if dx >= dst_w || dy >= dst_h {
                        continue;
                    }
                    let Some([r, g, b, a]) = sample(dx, dy) else {
                        continue;
                    };
                    if a == 0 {
                        continue;
                    }
                    let (_, cb, cr) = rgb_to_ycbcr_bt709(r, g, b);
                    let c = if is_cr { cr } else { cb };
                    let idx = cy * c_stride + cx;
                    if idx < c_plane.len() {
                        c_plane[idx] = over(c_plane[idx], c, a);
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_bitmap_chroma_nv12(
    frame: &mut VideoFrameMut<'_>,
    dst_x0: usize,
    dst_y0: usize,
    dst_w: usize,
    dst_h: usize,
    sample: &impl Fn(usize, usize) -> Option<[u8; 4]>,
) {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let (cx0, cx1, cy0, cy1) = bitmap_chroma_box(dst_x0, dst_y0, dst_w, dst_h, fw, fh);
    if let Some((c_plane, c_stride)) = frame.plane_mut(1) {
        for cy in cy0..cy1 {
            for cx in cx0..cx1 {
                let (lx, ly) = (2 * cx, 2 * cy);
                if lx < dst_x0 || ly < dst_y0 {
                    continue;
                }
                let (dx, dy) = (lx - dst_x0, ly - dst_y0);
                if dx >= dst_w || dy >= dst_h {
                    continue;
                }
                let Some([r, g, b, a]) = sample(dx, dy) else {
                    continue;
                };
                if a == 0 {
                    continue;
                }
                let (_, cb, cr) = rgb_to_ycbcr_bt709(r, g, b);
                let base = cy * c_stride + cx * 2; // Cb then Cr
                if base + 1 < c_plane.len() {
                    c_plane[base] = over(c_plane[base], cb, a);
                    c_plane[base + 1] = over(c_plane[base + 1], cr, a);
                }
            }
        }
    }
}

/// Full-range RGB to limited-range BT.709 Y'CbCr (ITU-R BT.709 section 3).
#[inline]
fn rgb_to_ycbcr_bt709(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (rf, gf, bf) = (r as f32, g as f32, b as f32);
    let y = 16.0 + 0.182_585_9 * rf + 0.614_230_6 * gf + 0.062_007_1 * bf;
    let cb = 128.0 - 0.100_643_7 * rf - 0.338_572 * gf + 0.439_215_7 * bf;
    let cr = 128.0 + 0.439_215_7 * rf - 0.398_942_2 * gf - 0.040_273_5 * bf;
    (clamp_u8(y), clamp_u8(cb), clamp_u8(cr))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pgs;
    use profluens_video::geometry::frame_size;

    fn raster(frame_h: u32, lines: &[&str]) -> (Vec<font::Bitmap>, usize) {
        let size = font::size_nearest((frame_h as f32 * TEXT_HEIGHT_FRAC).max(8.0));
        (lines.iter().map(|l| font::rasterize_line(size, l)).collect(), font::line_height(size))
    }

    fn overlay_with_cue(text: &str, start_ns: u64, dur_ns: u64) -> SubtitleOverlay {
        let mut o = SubtitleOverlay::new();
        o.push_cue(text.into(), Timestamp::from_nanos(start_ns), Timestamp::from_nanos(dur_ns));
        o
    }

    #[test]
    fn alpha_over_endpoints() {
        assert_eq!(over(100, 235, 0), 100, "zero coverage leaves dst");
        assert_eq!(over(100, 235, 255), 235, "full coverage replaces with src");
        let mid = over(0, 200, 128);
        assert!((99..=101).contains(&(mid as i32)), "half coverage ~= halfway: {mid}");
    }

    #[test]
    fn cue_activity_window_and_default_duration() {
        let o = overlay_with_cue("hi", 1_000_000_000, 2_000_000_000);
        assert!(o.active_text(Timestamp::from_nanos(1_500_000_000)) == vec!["hi"]);
        assert!(o.active_text(Timestamp::from_nanos(999_000_000)).is_empty(), "before start");
        assert!(
            o.active_text(Timestamp::from_nanos(3_000_000_000)).is_empty(),
            "at end (half-open)"
        );

        let mut o2 = SubtitleOverlay::new();
        o2.push_cue("x".into(), Timestamp::from_nanos(0), Timestamp::NONE);
        assert!(o2.active_text(Timestamp::from_nanos(DEFAULT_CUE_NS - 1)) == vec!["x"]);
        assert!(o2.active_text(Timestamp::from_nanos(DEFAULT_CUE_NS)).is_empty());
    }

    #[test]
    fn eviction_bounds_the_ring() {
        let mut o = SubtitleOverlay::new();
        for i in 0..100u64 {
            o.push_cue(
                format!("cue {i}"),
                Timestamp::from_nanos(i * 1_000_000_000),
                Timestamp::from_nanos(500_000_000),
            );
        }
        assert_eq!(o.cues.len(), 100);
        o.evict_stale(Timestamp::from_nanos(200 * 1_000_000_000));
        assert!(o.cues.is_empty(), "stale cues evicted, ring bounded");
    }

    #[test]
    fn empty_and_untimed_cues_are_dropped() {
        let mut o = SubtitleOverlay::new();
        o.push_cue("   ".into(), Timestamp::from_nanos(0), Timestamp::from_nanos(1_000_000_000));
        o.push_cue("real".into(), Timestamp::NONE, Timestamp::from_nanos(1_000_000_000));
        assert!(o.cues.is_empty(), "blank text and untimed (no PTS) cues are not held");
    }

    #[test]
    fn caption_changes_bottom_region_luma_i420() {
        let (w, h) = (320u32, 180u32);
        let mut buf = vec![128u8; frame_size(PixelFormat::I420, w, h)];
        let baseline = buf.clone();
        {
            let mut frame = VideoFrameMut::new(&mut buf, w, h, PixelFormat::I420).unwrap();
            let (bms, lh) = raster(h, &["Hello, subtitles!"]);
            draw_caption(&mut frame, &bms, lh);
        }
        assert_ne!(buf, baseline, "drawing a caption changes the frame");

        let y_stride = w as usize;
        let top_rows = &buf[..(h as usize / 3) * y_stride];
        let base_top = &baseline[..(h as usize / 3) * y_stride];
        assert_eq!(top_rows, base_top, "the top of the frame is untouched");

        let bottom_start = (2 * h as usize / 3) * y_stride;
        let bottom = &buf[bottom_start..(h as usize) * y_stride];
        let base_bottom = &baseline[bottom_start..(h as usize) * y_stride];
        assert_ne!(bottom, base_bottom, "the bottom (caption) region's luma changed");
        assert!(bottom.iter().any(|&p| p > 200), "white glyph ink present");
    }

    #[test]
    fn caption_handles_nv12_layout() {
        let (w, h) = (320u32, 180u32);
        let y_len = (w * h) as usize;
        let mut buf = vec![128u8; frame_size(PixelFormat::Nv12, w, h)];
        buf[y_len..].fill(200); // interleaved Cb/Cr away from neutral
        let baseline = buf.clone();
        {
            let mut frame = VideoFrameMut::new(&mut buf, w, h, PixelFormat::Nv12).unwrap();
            let (bms, lh) = raster(h, &["NV12 caption"]);
            draw_caption(&mut frame, &bms, lh);
        }
        assert_ne!(buf, baseline, "NV12 caption is drawn (luma + interleaved chroma)");
        assert_ne!(buf[..y_len], baseline[..y_len], "NV12 luma painted");
        let chroma_neutralised = buf[y_len..].iter().any(|&c| c < 200);
        assert!(chroma_neutralised, "NV12 interleaved chroma pulled toward neutral under the text");
    }

    #[test]
    fn bitmap_paints_only_its_rectangle_i420() {
        let (w, h) = (64u32, 48u32);
        let mut buf = vec![128u8; frame_size(PixelFormat::I420, w, h)];
        let baseline = buf.clone();
        let ds = pgs::DisplaySet {
            rgba: vec![255u8; 10 * 6 * 4], // solid white, fully opaque
            width: 10,
            height: 6,
            x: 20,
            y: 30,
            video_width: w,
            video_height: h,
        };
        let wire = pgs::encode_bitmap(&ds);
        let drew = {
            let mut frame = VideoFrameMut::new(&mut buf, w, h, PixelFormat::I420).unwrap();
            draw_bitmap(&mut frame, &BitmapRef::parse(&wire).unwrap())
        };
        assert!(drew, "an opaque bitmap draws");

        let ys = w as usize;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let inside = (20..30).contains(&x) && (30..36).contains(&y);
                let idx = y * ys + x;
                if inside {
                    assert!(buf[idx] > 200, "caption pixel ({x},{y}) brightened: {}", buf[idx]);
                } else {
                    assert_eq!(buf[idx], baseline[idx], "outside-caption luma ({x},{y}) untouched");
                }
            }
        }
    }

    #[test]
    fn transparent_bitmap_draws_nothing() {
        let (w, h) = (32u32, 32u32);
        let mut buf = vec![100u8; frame_size(PixelFormat::I420, w, h)];
        let baseline = buf.clone();
        let ds = pgs::DisplaySet {
            rgba: vec![0u8; 8 * 8 * 4], // RGBA all zero, alpha 0
            width: 8,
            height: 8,
            x: 4,
            y: 4,
            video_width: w,
            video_height: h,
        };
        let wire = pgs::encode_bitmap(&ds);
        let drew = {
            let mut frame = VideoFrameMut::new(&mut buf, w, h, PixelFormat::I420).unwrap();
            draw_bitmap(&mut frame, &BitmapRef::parse(&wire).unwrap())
        };
        assert!(!drew, "a fully-transparent bitmap draws nothing");
        assert_eq!(buf, baseline, "transparent caption leaves the frame untouched");
    }

    #[test]
    fn bitmap_scales_from_reference_size() {
        let (w, h) = (96u32, 54u32);
        let mut buf = vec![64u8; frame_size(PixelFormat::I420, w, h)];
        let ds = pgs::DisplaySet {
            rgba: vec![255u8; 20 * 4 * 4],
            width: 20,
            height: 4,
            x: 960,
            y: 540,
            video_width: 1920,
            video_height: 1080,
        };
        let wire = pgs::encode_bitmap(&ds);
        {
            let mut frame = VideoFrameMut::new(&mut buf, w, h, PixelFormat::I420).unwrap();
            assert!(
                draw_bitmap(&mut frame, &BitmapRef::parse(&wire).unwrap()),
                "scaled bitmap draws"
            );
        }
        let ys = w as usize;
        let mid_row = 27usize; // y=540/1080*54 = 27
        let first_bright = (0..w as usize).find(|&x| buf[mid_row * ys + x] > 200);
        assert!(
            matches!(first_bright, Some(x) if (40..56).contains(&x)),
            "caption scaled to mid-frame, first bright x = {first_bright:?}"
        );
    }

    #[test]
    fn bitmap_cue_timeline_and_clear() {
        let mut o = SubtitleOverlay::new();
        let ds = pgs::DisplaySet {
            rgba: vec![255u8; 4],
            width: 1,
            height: 1,
            x: 0,
            y: 0,
            video_width: 100,
            video_height: 100,
        };
        let wire = pgs::encode_bitmap(&ds);
        o.push_bitmap(
            &BitmapRef::parse(&wire).unwrap(),
            Timestamp::from_nanos(1_000_000_000),
            Timestamp::from_nanos(2_000_000_000),
        );
        assert!(o.active_bitmap(Timestamp::from_nanos(1_500_000_000)).is_some(), "shown mid-span");
        assert!(o.active_bitmap(Timestamp::from_nanos(999_000_000)).is_none(), "before start");
        let clear = pgs::encode_bitmap(&pgs::DisplaySet::clear(100, 100));
        o.push_bitmap(
            &BitmapRef::parse(&clear).unwrap(),
            Timestamp::from_nanos(1_600_000_000),
            Timestamp::NONE,
        );
        assert!(o.active_bitmap(Timestamp::from_nanos(1_500_000_000)).is_none(), "cleared");
    }

    #[test]
    fn bounded_chroma_matches_a_whole_plane_scan() {
        fn reference_i420(
            plane: &mut [u8],
            stride: usize,
            bm: &font::Bitmap,
            x0: usize,
            y0: usize,
            fw: usize,
            fh: usize,
        ) {
            for cy in 0..fh.div_ceil(2) {
                for cx in 0..fw.div_ceil(2) {
                    let a = chroma_coverage(bm, x0, y0, cx, cy);
                    if a == 0 {
                        continue;
                    }
                    let idx = cy * stride + cx;
                    if idx < plane.len() {
                        plane[idx] = over(plane[idx], NEUTRAL_CHROMA, a);
                    }
                }
            }
        }
        fn reference_nv12(
            plane: &mut [u8],
            stride: usize,
            bm: &font::Bitmap,
            x0: usize,
            y0: usize,
            fw: usize,
            fh: usize,
        ) {
            for cy in 0..fh.div_ceil(2) {
                for cx in 0..fw.div_ceil(2) {
                    let a = chroma_coverage(bm, x0, y0, cx, cy);
                    if a == 0 {
                        continue;
                    }
                    let idx = cy * stride + cx * 2;
                    if idx + 1 < plane.len() {
                        plane[idx] = over(plane[idx], NEUTRAL_CHROMA, a);
                        plane[idx + 1] = over(plane[idx + 1], NEUTRAL_CHROMA, a);
                    }
                }
            }
        }

        let s = font::size_nearest(14.0);
        let mut checked = 0usize;
        for text in ["Wg|_", "i", "The quick brown fox"] {
            let bm = font::rasterize_line(s, text);
            for &(fw, fh) in &[(24usize, 20usize), (25, 21), (26, 20), (25, 22)] {
                let (cw, ch) = (fw.div_ceil(2), fh.div_ceil(2));
                for y0 in 0..fh + 4 {
                    for x0 in 0..fw + 4 {
                        let bounds = chroma_box(x0, y0, bm.w, bm.h, fw, fh);
                        let mut got = vec![90u8; cw * ch];
                        let mut want = got.clone();
                        paint_chroma_i420(&mut got, cw, &bm, x0, y0, bounds);
                        reference_i420(&mut want, cw, &bm, x0, y0, fw, fh);
                        assert_eq!(
                            got, want,
                            "I420 bounded chroma differs from the whole-plane scan at frame \
                             {fw}x{fh} caption ({x0},{y0}) size {}x{}",
                            bm.w, bm.h
                        );
                        let mut got = vec![90u8; cw * 2 * ch];
                        let mut want = got.clone();
                        paint_chroma_nv12(&mut got, cw * 2, &bm, x0, y0, bounds);
                        reference_nv12(&mut want, cw * 2, &bm, x0, y0, fw, fh);
                        assert_eq!(
                            got, want,
                            "NV12 bounded chroma differs from the whole-plane scan at frame \
                             {fw}x{fh} caption ({x0},{y0}) size {}x{}",
                            bm.w, bm.h
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 3000, "the sweep should be broad, got {checked} placements");

        let bm = font::rasterize_line(s, "Wg|_");
        for &(fw, fh) in &[(64usize, 48usize), (65, 49), (320, 180)] {
            for &(x0, y0) in &[
                (0usize, 0usize),
                (1, 1),
                (3, 5),
                (fw / 2, fh / 2),
                (fw.saturating_sub(2), fh.saturating_sub(2)),
                (fw + 10, fh + 10), // entirely off-frame
            ] {
                let cw = fw.div_ceil(2);
                let ch = fh.div_ceil(2);
                let mut got = vec![90u8; cw * ch];
                let mut want = got.clone();
                let bounds = chroma_box(x0, y0, bm.w, bm.h, fw, fh);
                paint_chroma_i420(&mut got, cw, &bm, x0, y0, bounds);
                reference_i420(&mut want, cw, &bm, x0, y0, fw, fh);
                assert_eq!(
                    got, want,
                    "bounded chroma differs from the whole-plane scan at frame {fw}x{fh} \
                     caption ({x0},{y0}) size {}x{}",
                    bm.w, bm.h
                );
                if fw >= 320 && x0 < fw {
                    let (cx0, cx1, cy0, cy1) = bounds;
                    assert!(
                        (cx1 - cx0) * (cy1 - cy0) < cw * ch,
                        "the caption box should be smaller than the whole chroma plane"
                    );
                }
            }
        }
    }

    #[test]
    fn bounded_bitmap_chroma_matches_a_whole_plane_scan() {
        let (w, h) = (128u32, 96u32);
        let ds = pgs::DisplaySet {
            rgba: (0..20 * 10 * 4).map(|i| (i % 251) as u8).collect(),
            width: 20,
            height: 10,
            x: 100,
            y: 80,
            video_width: w,
            video_height: h,
        };
        let mut got = vec![70u8; frame_size(PixelFormat::I420, w, h)];
        let mut want = got.clone();
        let wire = pgs::encode_bitmap(&ds);
        {
            let mut f = VideoFrameMut::new(&mut got, w, h, PixelFormat::I420).unwrap();
            assert!(draw_bitmap(&mut f, &BitmapRef::parse(&wire).unwrap()));
        }
        {
            let mut f = VideoFrameMut::new(&mut want, w, h, PixelFormat::I420).unwrap();
            let (fw, fh) = (w as usize, h as usize);
            let (ref_w, ref_h) = (fw, fh);
            let dst_x0 = ds.x as usize * fw / ref_w;
            let dst_y0 = ds.y as usize * fh / ref_h;
            let dst_w = (ds.width as usize * fw).div_ceil(ref_w);
            let dst_h = (ds.height as usize * fh).div_ceil(ref_h);
            let sample = |dx: usize, dy: usize| -> Option<[u8; 4]> {
                let sx = dx * ds.width as usize / dst_w;
                let sy = dy * ds.height as usize / dst_h;
                let si = (sy * ds.width as usize + sx) * 4;
                ds.rgba.get(si..si + 4).map(|p| [p[0], p[1], p[2], p[3]])
            };
            if let Some((y_plane, y_stride)) = f.plane_mut(0) {
                for dy in 0..dst_h {
                    let py = dst_y0 + dy;
                    if py >= fh {
                        break;
                    }
                    for dx in 0..dst_w {
                        let px = dst_x0 + dx;
                        if px >= fw {
                            break;
                        }
                        let Some([r, g, b, a]) = sample(dx, dy) else {
                            continue;
                        };
                        if a == 0 {
                            continue;
                        }
                        let (yv, _, _) = rgb_to_ycbcr_bt709(r, g, b);
                        let idx = py * y_stride + px;
                        if idx < y_plane.len() {
                            y_plane[idx] = over(y_plane[idx], yv, a);
                        }
                    }
                }
            }
            for plane in 1..=2 {
                let is_cr = plane == 2;
                if let Some((c_plane, c_stride)) = f.plane_mut(plane) {
                    for cy in 0..fh.div_ceil(2) {
                        for cx in 0..fw.div_ceil(2) {
                            let (lx, ly) = (2 * cx, 2 * cy);
                            if lx < dst_x0 || ly < dst_y0 {
                                continue;
                            }
                            let (dx, dy) = (lx - dst_x0, ly - dst_y0);
                            if dx >= dst_w || dy >= dst_h {
                                continue;
                            }
                            let Some([r, g, b, a]) = sample(dx, dy) else {
                                continue;
                            };
                            if a == 0 {
                                continue;
                            }
                            let (_, cb, cr) = rgb_to_ycbcr_bt709(r, g, b);
                            let c = if is_cr { cr } else { cb };
                            let idx = cy * c_stride + cx;
                            if idx < c_plane.len() {
                                c_plane[idx] = over(c_plane[idx], c, a);
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(got, want, "bounded bitmap chroma differs from the whole-plane scan");
    }

    #[test]
    fn flush_clears_the_cue_timeline() {
        let mut o = SubtitleOverlay::new();
        for i in 0..10u64 {
            o.push_cue(
                format!("cue {i}"),
                Timestamp::from_nanos(i * 1_000_000_000),
                Timestamp::from_nanos(900_000_000),
            );
        }
        let wire = pgs::encode_bitmap(&pgs::DisplaySet {
            rgba: vec![255; 4],
            width: 1,
            height: 1,
            x: 0,
            y: 0,
            video_width: 8,
            video_height: 8,
        });
        o.push_bitmap(
            &BitmapRef::parse(&wire).unwrap(),
            Timestamp::from_nanos(0),
            Timestamp::from_nanos(1_000_000_000),
        );
        assert_eq!(o.cues.len(), 10);
        assert!(o.bitmap.is_some());
        o.flush();
        assert!(o.cues.is_empty(), "seek drops the stale cue ring");
        assert!(o.bitmap.is_none(), "seek drops the stale bitmap caption");
    }

    #[test]
    fn a_resent_cue_is_not_shown_twice() {
        let mut o = SubtitleOverlay::new();
        let (t, d) = (Timestamp::from_nanos(1_000_000_000), Timestamp::from_nanos(2_000_000_000));
        o.push_cue("Hello".into(), t, d);
        o.push_cue("Hello".into(), t, d);
        assert_eq!(o.cues.len(), 1, "the identical re-send is dropped");
        assert_eq!(o.active_text(Timestamp::from_nanos(1_500_000_000)), vec!["Hello"]);
        o.push_cue("- Who's there?".into(), t, d);
        assert_eq!(o.active_text(Timestamp::from_nanos(1_500_000_000)).len(), 2);
    }

    #[test]
    fn a_multi_line_cue_becomes_two_stacked_lines() {
        let mut o = SubtitleOverlay::new();
        o.push_cue(
            "line one\nline two".into(),
            Timestamp::from_nanos(0),
            Timestamp::from_nanos(1_000_000_000),
        );
        assert!(o.refresh_caption(Timestamp::from_nanos(0), 360));
        assert_eq!(o.caption.lines.len(), 2, "one coverage bitmap per physical line");
        assert_eq!(o.caption.lines[0].h, o.caption.lines[1].h, "same line box height");

        let (w, h) = (320u32, 180u32);
        let mut buf = vec![128u8; frame_size(PixelFormat::I420, w, h)];
        {
            let mut f = VideoFrameMut::new(&mut buf, w, h, PixelFormat::I420).unwrap();
            draw_caption(&mut f, &o.caption.lines, o.caption.line_h);
        }
        let ys = w as usize;
        let painted: Vec<usize> = (0..h as usize)
            .filter(|&y| buf[y * ys..(y + 1) * ys].iter().any(|&p| p != 128))
            .collect();
        let (first, last) = (painted[0], *painted.last().unwrap());
        assert!(
            last - first >= o.caption.line_h,
            "the two lines stack at least a line height apart (rows {first}..={last}, line_h {})",
            o.caption.line_h
        );
    }

    #[test]
    fn caption_size_saturates_on_tall_frames() {
        let target_720 = 720.0 * TEXT_HEIGHT_FRAC;
        let target_1080 = 1080.0 * TEXT_HEIGHT_FRAC;
        let s720 = font::size_nearest(target_720);
        let s1080 = font::size_nearest(target_1080);
        assert_eq!(s720.px, s1080.px, "720p and 1080p both clamp to the largest baked size");
        assert!(
            s1080.px < target_1080 / 2.0,
            "the drawn size ({}) is less than half the intended {target_1080} px",
            s1080.px
        );
    }

    #[test]
    fn an_unchanged_caption_is_not_re_rasterised() {
        let mut o = SubtitleOverlay::new();
        o.push_cue("Steady".into(), Timestamp::from_nanos(0), Timestamp::from_nanos(2_000_000_000));
        assert!(o.refresh_caption(Timestamp::from_nanos(0), 360));
        let first = o.caption.lines[0].cov.as_ptr();
        for f in 1..50u64 {
            assert!(o.refresh_caption(Timestamp::from_nanos(f * 40_000_000), 360));
            assert_eq!(
                o.caption.lines[0].cov.as_ptr(),
                first,
                "frame {f} re-rasterised an unchanged caption"
            );
        }
        o.push_cue(
            "Different".into(),
            Timestamp::from_nanos(1_000_000_000),
            Timestamp::from_nanos(1_000_000_000),
        );
        assert!(o.refresh_caption(Timestamp::from_nanos(1_000_000_000), 360));
        assert_eq!(o.caption.lines.len(), 2, "both cues are on screen now");
        let before = o.caption.size_px;
        assert!(o.refresh_caption(Timestamp::from_nanos(1_000_000_000), 120));
        assert_ne!(o.caption.size_px, before, "a shorter frame picks a different baked size");
        assert!(!o.refresh_caption(Timestamp::from_nanos(9_000_000_000), 120));
    }

    #[test]
    fn a_picture_waits_for_a_subtitle_stream_that_says_where_it_is() {
        use pf_harness::Harness;
        let ms = Timestamp::from_millis;
        let mut h = Harness::new(SubtitleOverlay::new());
        let frame = |h: &mut Harness, at: u64| {
            let mut buf = h.alloc(&[0u8; 16]);
            buf.pts = ms(at);
            h.push(buf).expect("process");
        };
        let shown = |h: &mut Harness| -> Vec<Timestamp> {
            h.drain_outputs().iter().map(|b| b.pts).collect()
        };
        frame(&mut h, 5_000);
        assert_eq!(shown(&mut h), [ms(5_000)]);
        h.push_event(Event::Gap { until: ms(1_000) }).expect("event");
        for at in [500, 1_900, 2_100, 2_140] {
            frame(&mut h, at);
        }
        assert_eq!(shown(&mut h), [ms(500), ms(1_900)], "within a second of what was said");
        assert_eq!(h.input_len(), 2, "the pictures past it wait");
        h.push_event(Event::Gap { until: ms(1_120) }).expect("event");
        h.crank().expect("process");
        assert_eq!(shown(&mut h), [ms(2_100)], "in order, as far as the gap reaches");
        h.push_event(Event::Gap { until: ms(1_500) }).expect("event");
        h.crank().expect("process");
        assert_eq!(shown(&mut h), [ms(2_140)]);
        frame(&mut h, 9_000);
        assert_eq!(h.input_len(), 1);
        h.push_event(Event::FlushStart).expect("event");
        frame(&mut h, 60_000);
        assert_eq!(shown(&mut h), [ms(9_000), ms(60_000)], "nothing is waited for");
    }
}
