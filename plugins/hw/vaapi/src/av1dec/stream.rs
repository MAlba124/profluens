//! A temporal unit taken apart frame by frame (AV1 section 7.5). No device here.

use super::header::{FrameHeader, NUM_REF_FRAMES, parse_frame_header};
use super::obu::{self, Av1Error, Tile, in_operating_point, next_obu};
use super::refs::{self, RefFrame};
use super::seq::{SequenceHeader, parse_sequence_header};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub data_start: usize,
    pub data_end: usize,
    pub first: usize,
    pub count: usize,
    pub tg_start: u16,
    pub tg_end: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub temporal_id: u8,
    pub spatial_id: u8,
}

#[derive(Debug)]
pub struct Stream {
    pub seq: Option<SequenceHeader>,
    pub seq_changed: bool,
    pub refs: [RefFrame; NUM_REF_FRAMES],
    seen_frame_header: bool,
    pub tiles: Vec<Tile>,
    pub groups: Vec<Group>,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    // Once per decoder, sized for a frame of 64 tiles in up to 16 groups.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Stream {
        Stream {
            seq: None,
            seq_changed: false,
            refs: [RefFrame::EMPTY; NUM_REF_FRAMES],
            seen_frame_header: false,
            tiles: Vec::with_capacity(64),
            groups: Vec::with_capacity(16),
        }
    }

    pub fn reset(&mut self) {
        self.refs = [RefFrame::EMPTY; NUM_REF_FRAMES];
        self.seen_frame_header = false;
        self.seq_changed = false;
    }

    /// Call [`Stream::finish`] before the next frame, its header may read the references.
    pub fn next_frame(&mut self, tu: &[u8], at: &mut usize) -> Result<Option<Frame>, Av1Error> {
        let mut current: Option<Frame> = None;
        while *at < tu.len() {
            let start = *at;
            let (o, n) = next_obu(&tu[start..])?;
            *at = start + n;
            let payload_start = *at - o.payload.len();
            let idc = self.seq.as_ref().map_or(0, SequenceHeader::operating_point_idc);
            if !in_operating_point(&o, idc) {
                continue;
            }
            match o.obu_type {
                obu::OBU_SEQUENCE_HEADER => {
                    let s = parse_sequence_header(o.payload)?;
                    // The frame in hand was parsed under the old one.
                    if current.is_some() && self.seq != Some(s) {
                        return Err(Av1Error::Invalid("sequence header inside a frame"));
                    }
                    if self.seq.is_some_and(|old| !old.same_sequence(&s)) {
                        self.seq_changed = true;
                    }
                    self.seq = Some(s);
                }
                obu::OBU_TEMPORAL_DELIMITER => self.seen_frame_header = false,
                obu::OBU_FRAME_HEADER | obu::OBU_REDUNDANT_FRAME_HEADER | obu::OBU_FRAME => {
                    if self.seen_frame_header && o.obu_type != obu::OBU_FRAME {
                        // frame_header_copy(), the header already in hand.
                        continue;
                    }
                    let seq = self.seq.as_ref().ok_or(Av1Error::NoSequenceHeader)?;
                    let header = parse_frame_header(
                        o.payload,
                        seq,
                        &self.refs,
                        o.temporal_id,
                        o.spatial_id,
                    )?;
                    let frame =
                        Frame { header, temporal_id: o.temporal_id, spatial_id: o.spatial_id };
                    if header.show_existing_frame {
                        self.seen_frame_header = false;
                        return Ok(Some(frame));
                    }
                    self.seen_frame_header = true;
                    self.tiles.clear();
                    self.groups.clear();
                    current = Some(frame);
                    if o.obu_type == obu::OBU_FRAME {
                        let skip = header.header_bytes();
                        let tg = o.payload.get(skip..).ok_or(Av1Error::Truncated)?;
                        if self.tile_group(tg, payload_start + skip, &header)? {
                            self.seen_frame_header = false;
                            return Ok(current);
                        }
                    }
                }
                obu::OBU_TILE_GROUP => {
                    let header = current.as_ref().ok_or(Av1Error::NoFrameHeader)?.header;
                    if self.tile_group(o.payload, payload_start, &header)? {
                        self.seen_frame_header = false;
                        return Ok(current);
                    }
                }
                obu::OBU_TILE_LIST => return Err(Av1Error::LargeScaleTile),
                _ => {}
            }
        }
        match current {
            Some(_) => Err(Av1Error::Invalid("frame without its last tile group")),
            None => Ok(None),
        }
    }

    fn tile_group(
        &mut self,
        payload: &[u8],
        base: usize,
        h: &FrameHeader,
    ) -> Result<bool, Av1Error> {
        let layout = h.tile_info.layout();
        let first = self.tiles.len();
        let tg = obu::tile_group(payload, &layout, &mut self.tiles)?;
        let expected = self.groups.last().map_or(0, |g| g.tg_end + 1);
        if tg.tg_start != expected {
            return Err(Av1Error::Invalid("tg_start"));
        }
        self.groups.push(Group {
            data_start: base + tg.data_offset,
            data_end: base + payload.len(),
            first,
            count: self.tiles.len() - first,
            tg_start: tg.tg_start,
            tg_end: tg.tg_end,
        });
        Ok(u32::from(tg.tg_end) + 1 == u32::from(layout.cols) * u32::from(layout.rows))
    }

    pub fn finish(&mut self, frame: &Frame) {
        if let Some(seq) = &self.seq {
            refs::apply(&mut self.refs, &frame.header, seq);
        }
    }
}

pub fn decoded_frames(tu: &[u8], reduced_still: bool) -> usize {
    obu::obus(tu)
        .map_while(Result::ok)
        .filter(|o| matches!(o.obu_type, obu::OBU_FRAME | obu::OBU_FRAME_HEADER))
        .filter(|o| reduced_still || o.payload.first().is_some_and(|b| b & 0x80 == 0))
        .count()
}
