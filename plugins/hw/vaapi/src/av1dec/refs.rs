//! The header side of the AV1 reference slots, sections 7.20 and 7.21.

use super::header::{DEFAULT_GM_PARAMS, FilmGrain, FrameHeader, FrameType, NUM_REF_FRAMES};
use super::seq::SequenceHeader;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefFrame {
    pub valid: bool,
    pub frame_id: u32,
    pub frame_type: FrameType,
    pub upscaled_width: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub order_hint: u8,
    pub saved_order_hints: [u8; 8],
    pub gm_params: [[i32; 6]; 8],
    pub lf_ref_deltas: [i8; 8],
    pub lf_mode_deltas: [i8; 2],
    pub feature_enabled: [u8; 8],
    pub feature_data: [[i16; 8]; 8],
    pub film_grain: FilmGrain,
    pub bit_depth: u8,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub showable_frame: bool,
}

impl RefFrame {
    pub const EMPTY: RefFrame = RefFrame {
        valid: false,
        frame_id: 0,
        frame_type: FrameType::Key,
        upscaled_width: 0,
        frame_width: 0,
        frame_height: 0,
        render_width: 0,
        render_height: 0,
        order_hint: 0,
        saved_order_hints: [0; 8],
        gm_params: DEFAULT_GM_PARAMS,
        lf_ref_deltas: [0; 8],
        lf_mode_deltas: [0; 2],
        feature_enabled: [0; 8],
        feature_data: [[0; 8]; 8],
        film_grain: FilmGrain::NONE,
        bit_depth: 8,
        subsampling_x: true,
        subsampling_y: true,
        showable_frame: false,
    };

    pub fn of(h: &FrameHeader, seq: &SequenceHeader) -> RefFrame {
        RefFrame {
            valid: true,
            frame_id: h.current_frame_id,
            frame_type: h.frame_type,
            upscaled_width: h.upscaled_width,
            frame_width: h.frame_width,
            frame_height: h.frame_height,
            render_width: h.render_width,
            render_height: h.render_height,
            order_hint: h.order_hint,
            saved_order_hints: h.order_hints,
            gm_params: h.gm.params,
            lf_ref_deltas: h.lf.ref_deltas,
            lf_mode_deltas: h.lf.mode_deltas,
            feature_enabled: h.seg.feature_enabled,
            feature_data: h.seg.feature_data,
            film_grain: h.film_grain,
            bit_depth: seq.color.bit_depth,
            subsampling_x: seq.color.subsampling_x,
            subsampling_y: seq.color.subsampling_y,
            showable_frame: h.showable_frame,
        }
    }
}

pub fn refresh(refs: &mut [RefFrame; NUM_REF_FRAMES], flags: u8, cur: &RefFrame) {
    for (i, slot) in refs.iter_mut().enumerate() {
        if flags >> i & 1 == 1 {
            *slot = *cur;
        }
    }
}

/// A shown key frame invalidates every slot first (section 5.9.2), then section 7.20 refreshes.
pub fn apply(refs: &mut [RefFrame; NUM_REF_FRAMES], h: &FrameHeader, seq: &SequenceHeader) {
    if h.show_existing_frame {
        if h.frame_type == FrameType::Key {
            let shown = refs[usize::from(h.frame_to_show_map_idx & 7)];
            refresh(refs, h.refresh_frame_flags, &shown);
        }
        return;
    }
    if h.frame_type == FrameType::Key && h.show_frame {
        for r in refs.iter_mut() {
            (r.valid, r.order_hint) = (false, 0);
        }
    }
    if let Some(hints) = h.ref_order_hint {
        for (r, &hint) in refs.iter_mut().zip(&hints) {
            if r.order_hint != hint {
                (r.valid, r.order_hint) = (false, hint);
            }
        }
    }
    refresh(refs, h.refresh_frame_flags, &RefFrame::of(h, seq));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_flags_name_the_slots() {
        let mut refs = [RefFrame::EMPTY; NUM_REF_FRAMES];
        let a = RefFrame { valid: true, order_hint: 3, ..RefFrame::EMPTY };
        refresh(&mut refs, 0b1000_0010, &a);
        assert!(refs[1].valid && refs[7].valid);
        assert!(!refs[0].valid && !refs[6].valid);
        assert_eq!(refs[7].order_hint, 3);
    }
}
