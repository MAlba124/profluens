use parse::{
    ColorConfig, ColorSpace, FrameType, MAX_SEGMENTS, RefFrameState, SEG_LVL_MAX, Vp9FrameHeader,
};
pub use quant::{ac_q, dc_q};

use crate::{DpbError, Event, Events, Fate, Slot};

pub mod parse;
mod quant;

/// Reference slots, NUM_REF_FRAMES.
pub const NUM_SLOTS: usize = 8;
/// LAST, GOLDEN and ALTREF.
pub const NUM_REFS: usize = 3;
pub const SEG_LVL_ALT_Q: usize = 0;
pub const SEG_LVL_ALT_L: usize = 1;
pub const SEG_LVL_REF_FRAME: usize = 2;
pub const SEG_LVL_SKIP: usize = 3;
const MAX_LOOP_FILTER: i32 = 63;

/// A frame in a reference slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ref {
    pub slot: Slot,
    pub width: u32,
    pub height: u32,
}

impl Ref {
    const EMPTY: Ref = Ref { slot: Slot(0), width: 0, height: 0 };
}

/// Filter levels (8.8.1) and dequantiser scales (8.6.1) of one segment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    /// SEG_LVL_REF_FRAME, the reference frame every block of the segment uses
    pub reference: Option<u8>,
    pub skipped: bool,
    /// LvlLookup by reference frame and mode
    pub filter_level: [[u8; 2]; 4],
    pub luma_ac_quant_scale: u16,
    pub luma_dc_quant_scale: u16,
    pub chroma_ac_quant_scale: u16,
    pub chroma_dc_quant_scale: u16,
}

/// Loop filter deltas, segment features and the colour config carried across frames (7.2,
/// 8.4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamState {
    /// of the last intra frame, what inter frames inherit
    pub color: ColorConfig,
    pub lf_ref_deltas: [i8; 4],
    pub lf_mode_deltas: [i8; 2],
    pub feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
    pub abs_or_delta_update: bool,
    pub tree_probs: [u8; 7],
    pub pred_probs: [u8; 3],
}

impl StreamState {
    pub fn new() -> Self {
        let mut s = StreamState {
            color: ColorConfig {
                bit_depth: 8,
                color_space: ColorSpace::Unknown,
                color_range_full: false,
                subsampling_x: true,
                subsampling_y: true,
            },
            lf_ref_deltas: [0; 4],
            lf_mode_deltas: [0; 2],
            feature_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
            feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
            abs_or_delta_update: false,
            tree_probs: [255; 7],
            pred_probs: [255; 3],
        };
        s.setup_past_independence();
        s
    }

    fn setup_past_independence(&mut self) {
        self.feature_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
        self.feature_data = [[0; SEG_LVL_MAX]; MAX_SEGMENTS];
        self.abs_or_delta_update = false;
        self.lf_ref_deltas = [1, 0, -1, -1];
        self.lf_mode_deltas = [0, 0];
    }

    /// Takes what a frame header codes, an intra or error resilient frame resets first.
    pub fn fold(&mut self, h: &Vp9FrameHeader) {
        let intra = h.frame_type == FrameType::KeyFrame || h.intra_only;
        if intra {
            self.color = h.color_config;
        }
        if intra || h.error_resilient_mode {
            self.setup_past_independence();
        }
        for (held, coded) in self.lf_ref_deltas.iter_mut().zip(h.loop_filter.ref_deltas) {
            if let Some(d) = coded {
                *held = d;
            }
        }
        for (held, coded) in self.lf_mode_deltas.iter_mut().zip(h.loop_filter.mode_deltas) {
            if let Some(d) = coded {
                *held = d;
            }
        }
        let seg = &h.segmentation;
        if !seg.enabled {
            return;
        }
        if let Some(p) = seg.tree_probs {
            self.tree_probs = p;
        }
        if let Some(p) = seg.pred_prob {
            self.pred_probs = p;
        }
        if seg.update_data {
            self.abs_or_delta_update = seg.abs_or_delta_update;
            self.feature_enabled = seg.feature_enabled;
            self.feature_data = seg.feature_data;
        }
    }

    fn active(&self, h: &Vp9FrameHeader, segment: usize, feature: usize) -> bool {
        h.segmentation.enabled && self.feature_enabled[segment][feature]
    }

    /// The segment's qindex (8.6.1).
    pub fn qindex(&self, h: &Vp9FrameHeader, segment: usize) -> i32 {
        let base = i32::from(h.quantization.base_q_idx);
        if !self.active(h, segment, SEG_LVL_ALT_Q) {
            return base;
        }
        let data = i32::from(self.feature_data[segment][SEG_LVL_ALT_Q]);
        let q = if self.abs_or_delta_update { data } else { base + data };
        q.clamp(0, 255)
    }

    /// LvlLookup (8.8.1), all zero when the frame is not filtered.
    pub fn filter_levels(&self, h: &Vp9FrameHeader, segment: usize) -> [[u8; 2]; 4] {
        let lf = &h.loop_filter;
        let level = i32::from(lf.level);
        if level == 0 {
            return [[0; 2]; 4];
        }
        let mut seg_level = level;
        if self.active(h, segment, SEG_LVL_ALT_L) {
            let data = i32::from(self.feature_data[segment][SEG_LVL_ALT_L]);
            seg_level = if self.abs_or_delta_update { data } else { level + data }
                .clamp(0, MAX_LOOP_FILTER);
        }
        if !lf.delta_enabled {
            return [[seg_level as u8; 2]; 4];
        }
        let shift = level >> 5;
        let clip = |v: i32| v.clamp(0, MAX_LOOP_FILTER) as u8;
        let mut out = [[0u8; 2]; 4];
        // INTRA_FRAME reads mode 0 only, mode 1 gets the same level
        let intra = clip(seg_level + (i32::from(self.lf_ref_deltas[0]) << shift));
        out[0] = [intra, intra];
        for (r, row) in out.iter_mut().enumerate().skip(1) {
            for (m, cell) in row.iter_mut().enumerate() {
                *cell = clip(
                    seg_level
                        + (i32::from(self.lf_ref_deltas[r]) << shift)
                        + (i32::from(self.lf_mode_deltas[m]) << shift),
                );
            }
        }
        out
    }

    /// Filter levels and dequantiser scales of one segment, folded with `h` already.
    pub fn segment(&self, h: &Vp9FrameHeader, segment: usize) -> Segment {
        let bd = h.color_config.bit_depth;
        let q = self.qindex(h, segment);
        let quant = &h.quantization;
        Segment {
            reference: self
                .active(h, segment, SEG_LVL_REF_FRAME)
                .then(|| (self.feature_data[segment][SEG_LVL_REF_FRAME] & 3) as u8),
            skipped: self.active(h, segment, SEG_LVL_SKIP),
            filter_level: self.filter_levels(h, segment),
            luma_ac_quant_scale: ac_q(bd, q) as u16,
            luma_dc_quant_scale: dc_q(bd, q + i32::from(quant.delta_q_y_dc)) as u16,
            chroma_ac_quant_scale: ac_q(bd, q + i32::from(quant.delta_q_uv_ac)) as u16,
            chroma_dc_quant_scale: dc_q(bd, q + i32::from(quant.delta_q_uv_dc)) as u16,
        }
    }
}

impl Default for StreamState {
    fn default() -> Self {
        Self::new()
    }
}

/// Reference slots and the state carried across frames of a VP9 stream. Frames go out in
/// decode order, a slot shown again goes out again. No allocation, no hardware, the caller
/// decodes.
pub struct Dpb {
    slots: [Option<Ref>; NUM_SLOTS],
    state: StreamState,
    /// no key frame yet, or a discard came
    awaiting_key: bool,
}

impl Dpb {
    pub fn new() -> Self {
        Dpb { slots: [None; NUM_SLOTS], state: StreamState::new(), awaiting_key: true }
    }

    /// The uncompressed header (6.2) of one frame of a superframe, an inter frame's size
    /// from the slots and its colour config the held one.
    pub fn parse(&self, frame: &[u8]) -> Result<Vp9FrameHeader, parse::Error> {
        let mut dims = [(0u32, 0u32); NUM_SLOTS];
        for (d, s) in dims.iter_mut().zip(&self.slots) {
            if let Some(s) = s {
                *d = (s.width, s.height);
            }
        }
        let refs = RefFrameState { ref_dims: &dims, color_config: self.state.color };
        parse::parse_uncompressed_header_with_refs(frame, Some(refs))
    }

    /// Starts a frame. Frames before the first key frame are `Fate::Skip`, so is a shown
    /// existing frame, with its `Output` in `ev`. A slot the frame names empty is
    /// `MissingReference` and nothing changes. Otherwise the held state takes the header,
    /// `Shown` or `Hidden` by show_frame.
    pub fn begin(&mut self, h: &Vp9FrameHeader, ev: &mut Events) -> Result<Fate, DpbError> {
        ev.clear();
        if self.awaiting_key && h.frame_type != FrameType::KeyFrame {
            return Ok(Fate::Skip);
        }
        if h.show_existing_frame {
            let i = usize::from(h.frame_to_show_map_idx.unwrap_or(0) & 7);
            let r = self.slots[i].ok_or(DpbError::MissingReference)?;
            ev.push(Event::Output(r.slot));
            return Ok(Fate::Skip);
        }
        if h.ref_frame_idx.is_some() && self.refs(h).is_none() {
            return Err(DpbError::MissingReference);
        }
        self.state.fold(h);
        self.awaiting_key = false;
        Ok(if h.show_frame { Fate::Shown } else { Fate::Hidden })
    }

    /// LAST, GOLDEN and ALTREF of an inter frame, None for an intra frame or a slot empty.
    pub fn refs(&self, h: &Vp9FrameHeader) -> Option<[Ref; NUM_REFS]> {
        let idx = h.ref_frame_idx?;
        let mut out = [Ref::EMPTY; NUM_REFS];
        for (o, &i) in out.iter_mut().zip(&idx) {
            *o = self.slots[usize::from(i & 7)]?;
        }
        Some(out)
    }

    pub fn slots(&self) -> &[Option<Ref>; NUM_SLOTS] {
        &self.slots
    }

    /// The state `begin` folded the current frame into.
    pub fn state(&self) -> &StreamState {
        &self.state
    }

    /// Ends a decoded frame in `slot`. `Output` when shown, then the slots refresh_frame_flags
    /// names take it and a frame no slot holds any more is released, the frame itself too
    /// when no slot takes it.
    pub fn end(&mut self, h: &Vp9FrameHeader, slot: Slot, ev: &mut Events) {
        ev.clear();
        if h.show_frame {
            ev.push(Event::Output(slot));
        }
        let new = Ref { slot, width: h.frame_width, height: h.frame_height };
        let mut old = [None; NUM_SLOTS];
        for (i, s) in self.slots.iter_mut().enumerate() {
            if h.refresh_frame_flags >> i & 1 == 1 {
                old[i] = s.replace(new);
            }
        }
        self.release_unheld(&old, ev);
        if !self.holds(slot) {
            ev.push(Event::Release(slot));
        }
    }

    /// A seek or a stream error. Empties the slots and the held state, decoding resumes at
    /// the next key frame.
    pub fn discard(&mut self, ev: &mut Events) {
        ev.clear();
        let old = std::mem::replace(&mut self.slots, [None; NUM_SLOTS]);
        self.release_unheld(&old, ev);
        self.state = StreamState::new();
        self.awaiting_key = true;
    }

    /// Whether frames wait for a key frame, after a discard, a failed frame or at the start.
    pub fn awaits_key(&self) -> bool {
        self.awaiting_key
    }

    /// A frame that failed. Frames up to the next key frame are `Fate::Skip`, the slots and the
    /// held state stay so the frames between still parse.
    pub fn await_key(&mut self) {
        self.awaiting_key = true;
    }

    /// Empties the slots, the held state stays. For a key frame `begin` took that refreshes
    /// every slot anyway, when its frames must go first.
    pub fn drop_slots(&mut self, ev: &mut Events) {
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
