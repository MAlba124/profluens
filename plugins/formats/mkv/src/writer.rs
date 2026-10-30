//! `MatroskaWriter`, track configs and frames in, a Matroska byte stream out (RFC 9559, RFC 8794).
//! Finite masters get back-patched 8-octet sizes, the Segment is unknown-size (RFC 8794 section
//! 6.2). Cold module. The per-frame scatter path holds the payload as a `Memory` and allocates
//! nothing.
#![allow(clippy::disallowed_methods)]

use std::collections::VecDeque;

use profluens_core::memory::Memory;

use crate::ebml::{self, id};

pub const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;

pub const APP_NAME: &str = "pf-mkv";

const TRACK_TYPE_VIDEO: u64 = 1;
const TRACK_TYPE_AUDIO: u64 = 2;

const BLOCK_FLAG_KEYFRAME: u8 = 0x80;

const REL_TS_MAX: i64 = i16::MAX as i64;
const REL_TS_MIN: i64 = i16::MIN as i64;

/// Body bytes at which a Cluster closes, keyframe or not, as ffmpeg's matroskaenc does.
pub const CLUSTER_BYTES_MAX: u64 = 5 << 20;

/// Blocks at which a Cluster closes, so a muxer holds fewer buffers than an upstream default pool.
pub const CLUSTER_BLOCKS_MAX: usize = 32;
/// An audio anchor is all keyframes, so its Clusters split on time, not on every frame.
pub const AUDIO_CLUSTER_NS: u64 = 5_000_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioConfig {
    pub sampling_frequency: f64,
    pub channels: u32,
    pub bit_depth: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoConfig {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub colour: Option<ColourConfig>,
}

/// `Colour` children as H.273 code points (RFC 9559 section 5.1.4.1.31). A child at its default is
/// not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourConfig {
    pub matrix: u8,
    pub range: u8,
    pub transfer: u8,
    pub primaries: u8,
    pub hdr: profluens_video::color::HdrStatic,
    pub chroma_siting: (u8, u8),
    pub dovi: Option<profluens_video::color::DoviConfig>,
}

impl Default for ColourConfig {
    fn default() -> Self {
        Self {
            matrix: 2,
            range: 0,
            transfer: 2,
            primaries: 2,
            hdr: Default::default(),
            chroma_siting: (0, 0),
            dovi: None,
        }
    }
}

impl ColourConfig {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    fn colour_is_empty(self) -> bool {
        ColourConfig { dovi: None, ..self }.is_empty()
    }
}

/// One track to mux. A track is audio unless it carries a [`VideoConfig`].
#[derive(Clone, Debug)]
pub struct TrackConfig {
    pub track_number: u64,
    pub codec_id: String,
    pub codec_private: Vec<u8>,
    pub codec_delay_ns: u64,
    pub seek_pre_roll_ns: u64,
    pub audio: AudioConfig,
    pub video: Option<VideoConfig>,
}

impl TrackConfig {
    pub fn flac(
        track_number: u64,
        codec_private: Vec<u8>,
        sampling_frequency: f64,
        channels: u32,
        bit_depth: u32,
    ) -> Self {
        Self {
            track_number,
            codec_id: "A_FLAC".to_string(),
            codec_private,
            codec_delay_ns: 0,
            seek_pre_roll_ns: 0,
            audio: AudioConfig { sampling_frequency, channels, bit_depth },
            video: None,
        }
    }

    /// A generic audio track. For A_AAC `codec_private` is the raw AudioSpecificConfig, frames
    /// carry no ADTS.
    pub fn audio(
        track_number: u64,
        codec_id: &str,
        codec_private: Vec<u8>,
        sampling_frequency: f64,
        channels: u32,
        bit_depth: u32,
    ) -> Self {
        Self {
            track_number,
            codec_id: codec_id.to_string(),
            codec_private,
            codec_delay_ns: 0,
            seek_pre_roll_ns: 0,
            audio: AudioConfig { sampling_frequency, channels, bit_depth },
            video: None,
        }
    }

    pub fn video(
        track_number: u64,
        codec_id: &str,
        codec_private: Vec<u8>,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Self {
        Self {
            track_number,
            codec_id: codec_id.to_string(),
            codec_private,
            codec_delay_ns: 0,
            seek_pre_roll_ns: 0,
            audio: AudioConfig { sampling_frequency: 0.0, channels: 0, bit_depth: 0 },
            video: Some(VideoConfig { pixel_width, pixel_height, colour: None }),
        }
    }

    pub fn vp8(track_number: u64, pixel_width: u32, pixel_height: u32) -> Self {
        Self::video(track_number, "V_VP8", Vec::new(), pixel_width, pixel_height)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WriteError {
    UnknownTrack(u64),
    HeaderNotWritten,
    BadTracks,
}

struct OpenCluster {
    base_tick: i64,
    /// Staged until the Cluster closes so its real size is written. mpv rejects blocks in
    /// unknown-size Clusters.
    buf: Vec<u8>,
    pieces: Vec<(usize, Memory)>,
    piece_bytes: u64,
    blocks: usize,
    has_cue_key: bool,
}

impl OpenCluster {
    fn body_size(&self) -> u64 {
        self.buf.len() as u64 + self.piece_bytes
    }
}

/// Multi-track Matroska muxer. Write the header, then frames, then [`finalize`](Self::finalize).
pub struct MatroskaWriter {
    tracks: Vec<TrackConfig>,
    timestamp_scale: u64,
    header_written: bool,
    cluster: Option<OpenCluster>,
    anchor_track: u64,
    /// Ticks an anchor keyframe must be from the Cluster's base to open the next.
    anchor_span: i64,
    duration_ns: Option<u64>,
    cues: Option<CueState>,
    duration_reserved: bool,
    duration_at: Option<u64>,
    cluster_bytes_max: u64,
    spare: (Vec<u8>, Vec<(usize, Memory)>),
}

struct CueState {
    segment_data_start: u64,
    info_pos: u64,
    tracks_pos: u64,
    cue_track: u64,
    points: Vec<(u64, u64)>,
    text_tracks: Vec<u64>,
    blocks: Vec<(u64, u64, u64, u64)>,
}

/// SeekHead (4+8 octets) plus three Seek entries of <= 29 octets is <= 99, so 128 leaves room for
/// the Void header.
const SEEKHEAD_RESERVE: usize = 128;

/// The SeekHead back-patch, `bytes` replace the reservation at absolute stream offset `offset`.
pub struct SeekHeadPatch {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl MatroskaWriter {
    pub fn new(tracks: Vec<TrackConfig>) -> Self {
        Self::with_timestamp_scale(tracks, DEFAULT_TIMESTAMP_SCALE)
    }

    pub fn with_timestamp_scale(tracks: Vec<TrackConfig>, timestamp_scale: u64) -> Self {
        let anchor_track = tracks.first().map(|t| t.track_number).unwrap_or(1);
        let anchor_span = match tracks.first().is_some_and(|t| t.video.is_some()) {
            true => 1,
            false => (AUDIO_CLUSTER_NS / timestamp_scale.max(1)).clamp(1, REL_TS_MAX as u64) as i64,
        };
        Self {
            tracks,
            timestamp_scale,
            header_written: false,
            cluster: None,
            anchor_track,
            anchor_span,
            duration_ns: None,
            cues: None,
            duration_reserved: false,
            duration_at: None,
            cluster_bytes_max: CLUSTER_BYTES_MAX,
            spare: (Vec::new(), Vec::new()),
        }
    }

    pub fn set_cluster_bytes_max(&mut self, bytes: u64) {
        self.cluster_bytes_max = bytes.max(1);
    }

    /// Track cue points and emit `Cues` and a front `SeekHead` (RFC 9559 sections 5.1.1, 5.1.5).
    /// Call before the header.
    // Cold path, once before the header.
    #[allow(clippy::disallowed_methods)]
    pub fn enable_cues(&mut self) {
        if self.header_written {
            return; // too late, the reservation must precede Info/Tracks
        }
        let cue_track = self
            .tracks
            .iter()
            .find(|t| t.video.is_some())
            .map(|t| t.track_number)
            .unwrap_or(self.anchor_track);
        let text_tracks = self
            .tracks
            .iter()
            .filter(|t| t.codec_id.starts_with("S_"))
            .map(|t| t.track_number)
            .collect();
        self.cues = Some(CueState {
            segment_data_start: 0,
            info_pos: 0,
            tracks_pos: 0,
            cue_track,
            points: Vec::new(),
            text_tracks,
            blocks: Vec::new(),
        });
    }

    /// Declare the duration in ns. Must be called before the header.
    pub fn set_duration_ns(&mut self, ns: u64) {
        if !self.header_written {
            self.duration_ns = Some(ns);
        }
    }

    /// Reserve a back-patchable `Info\Duration` for a stream whose length is unknown until it ends.
    pub fn reserve_duration(&mut self) {
        if !self.header_written {
            self.duration_reserved = true;
        }
    }

    /// The positioned write replacing the reserved Duration with `ns`, `None` without a
    /// reservation.
    pub fn duration_patch(&self, ns: u64) -> Option<SeekHeadPatch> {
        let offset = self.duration_at?;
        let mut bytes = Vec::with_capacity(16);
        ebml::write_f64(&mut bytes, id::DURATION, ns as f64 / self.timestamp_scale.max(1) as f64);
        Some(SeekHeadPatch { offset, bytes })
    }

    pub fn timestamp_scale(&self) -> u64 {
        self.timestamp_scale
    }

    pub fn tracks(&self) -> &[TrackConfig] {
        &self.tracks
    }

    fn validate_tracks(&self) -> Result<(), WriteError> {
        if self.tracks.is_empty() {
            return Err(WriteError::BadTracks);
        }
        for (i, t) in self.tracks.iter().enumerate() {
            if t.track_number == 0 {
                return Err(WriteError::BadTracks);
            }
            if self.tracks[..i].iter().any(|o| o.track_number == t.track_number) {
                return Err(WriteError::BadTracks); // duplicate track number
            }
        }
        Ok(())
    }

    /// Rounds to the nearest tick, widened and clamped since the timestamp is untrusted.
    fn ns_to_ticks(&self, timestamp_ns: u64) -> i64 {
        let scale = self.timestamp_scale.max(1) as u128;
        let ticks = (timestamp_ns as u128 + scale / 2) / scale;
        ticks.min(i64::MAX as u128) as i64
    }

    pub fn write_header(&mut self, out: &mut Vec<u8>) -> Result<(), WriteError> {
        self.validate_tracks()?;
        self.write_ebml_header(out);
        ebml::write_id(out, id::SEGMENT);
        ebml::write_unknown_size(out);
        if let Some(cues) = &mut self.cues {
            // The SeekHead reservation is the Segment's first child (RFC 9559 section 5.1.1), a
            // Void finalize overwrites.
            cues.segment_data_start = out.len() as u64;
            ebml::write_id(out, id::VOID);
            ebml::write_size(out, (SEEKHEAD_RESERVE - 2) as u64); // 1-octet size VINT
            out.resize(out.len() + SEEKHEAD_RESERVE - 2, 0);
            cues.info_pos = out.len() as u64 - cues.segment_data_start;
        }
        self.write_info(out);
        if let Some(cues) = &mut self.cues {
            cues.tracks_pos = out.len() as u64 - cues.segment_data_start;
        }
        self.write_tracks(out);
        self.header_written = true;
        Ok(())
    }

    fn write_ebml_header(&self, out: &mut Vec<u8>) {
        ebml::write_id(out, id::EBML);
        let size_at = ebml::reserve_size(out);
        let body_start = out.len();
        ebml::write_uint(out, id::EBML_VERSION, 1);
        ebml::write_uint(out, id::EBML_READ_VERSION, 1);
        ebml::write_uint(out, id::EBML_MAX_ID_LENGTH, 4);
        ebml::write_uint(out, id::EBML_MAX_SIZE_LENGTH, 8);
        ebml::write_string(out, id::DOC_TYPE, "matroska");
        ebml::write_uint(out, id::DOC_TYPE_VERSION, 4);
        ebml::write_uint(out, id::DOC_TYPE_READ_VERSION, 2);
        let len = (out.len() - body_start) as u64;
        ebml::patch_size(out, size_at, len);
    }

    fn write_info(&mut self, out: &mut Vec<u8>) {
        ebml::write_id(out, id::INFO);
        let size_at = ebml::reserve_size(out);
        let body_start = out.len();
        ebml::write_uint(out, id::TIMESTAMP_SCALE, self.timestamp_scale);
        if let Some(ns) = self.duration_ns {
            ebml::write_f64(out, id::DURATION, ns as f64 / self.timestamp_scale.max(1) as f64);
        } else if self.duration_reserved {
            // Fixed-width placeholder, `write_f64` is byte-stable so the patch re-encodes in place.
            self.duration_at = Some(out.len() as u64);
            ebml::write_f64(out, id::DURATION, 0.0);
        }
        ebml::write_string(out, id::MUXING_APP, APP_NAME);
        ebml::write_string(out, id::WRITING_APP, APP_NAME);
        let len = (out.len() - body_start) as u64;
        ebml::patch_size(out, size_at, len);
    }

    fn write_tracks(&self, out: &mut Vec<u8>) {
        ebml::write_id(out, id::TRACKS);
        let size_at = ebml::reserve_size(out);
        let body_start = out.len();
        for track in &self.tracks {
            Self::write_track_entry(out, track);
        }
        let len = (out.len() - body_start) as u64;
        ebml::patch_size(out, size_at, len);
    }

    fn write_track_entry(out: &mut Vec<u8>, track: &TrackConfig) {
        ebml::write_id(out, id::TRACK_ENTRY);
        let size_at = ebml::reserve_size(out);
        let body_start = out.len();

        let track_type = if track.video.is_some() { TRACK_TYPE_VIDEO } else { TRACK_TYPE_AUDIO };
        ebml::write_uint(out, id::TRACK_NUMBER, track.track_number);
        ebml::write_uint(out, id::TRACK_UID, track.track_number); // stable nonzero UID
        ebml::write_uint(out, id::TRACK_TYPE, track_type);
        ebml::write_uint(out, id::FLAG_LACING, 0);
        ebml::write_string(out, id::CODEC_ID, &track.codec_id);
        if !track.codec_private.is_empty() {
            ebml::write_binary(out, id::CODEC_PRIVATE, &track.codec_private);
        }
        if track.codec_delay_ns != 0 {
            ebml::write_uint(out, id::CODEC_DELAY, track.codec_delay_ns);
        }
        if track.seek_pre_roll_ns != 0 {
            ebml::write_uint(out, id::SEEK_PRE_ROLL, track.seek_pre_roll_ns);
        }

        if let Some(dovi) = track.video.as_ref().and_then(|v| v.colour?.dovi) {
            Self::write_dovi_mapping(out, &dovi);
        }
        match &track.video {
            Some(video) => Self::write_video(out, video),
            None => Self::write_audio(out, &track.audio),
        }

        let len = (out.len() - body_start) as u64;
        ebml::patch_size(out, size_at, len);
    }

    /// The Dolby Vision mapping as FFmpeg's matroskaenc writes it. A record past profile 10 has no
    /// type and is left out.
    fn write_dovi_mapping(out: &mut Vec<u8>, dovi: &profluens_video::color::DoviConfig) {
        if dovi.profile > 10 {
            return;
        }
        let kind = if dovi.profile > 7 {
            crate::reader::ADD_ID_TYPE_DVVC
        } else {
            crate::reader::ADD_ID_TYPE_DVCC
        };
        ebml::write_id(out, id::BLOCK_ADDITION_MAPPING);
        let size_at = ebml::reserve_size(out);
        let start = out.len();
        ebml::write_string(out, id::BLOCK_ADD_ID_NAME, "Dolby Vision configuration");
        ebml::write_uint(out, id::BLOCK_ADD_ID_TYPE, kind);
        ebml::write_binary(out, id::BLOCK_ADD_ID_EXTRA_DATA, &dovi.to_record());
        let len = (out.len() - start) as u64;
        ebml::patch_size(out, size_at, len);
    }

    fn write_audio(out: &mut Vec<u8>, audio: &AudioConfig) {
        ebml::write_id(out, id::AUDIO);
        let audio_size_at = ebml::reserve_size(out);
        let audio_start = out.len();
        ebml::write_f64(out, id::SAMPLING_FREQUENCY, audio.sampling_frequency);
        ebml::write_uint(out, id::CHANNELS, audio.channels as u64);
        if audio.bit_depth != 0 {
            ebml::write_uint(out, id::BIT_DEPTH, audio.bit_depth as u64);
        }
        let audio_len = (out.len() - audio_start) as u64;
        ebml::patch_size(out, audio_size_at, audio_len);
    }

    fn write_video(out: &mut Vec<u8>, video: &VideoConfig) {
        ebml::write_id(out, id::VIDEO);
        let video_size_at = ebml::reserve_size(out);
        let video_start = out.len();
        ebml::write_uint(out, id::PIXEL_WIDTH, video.pixel_width as u64);
        ebml::write_uint(out, id::PIXEL_HEIGHT, video.pixel_height as u64);
        if let Some(c) = video.colour.filter(|c| !c.colour_is_empty()) {
            let dflt = ColourConfig::default();
            ebml::write_id(out, id::COLOUR);
            let colour_size_at = ebml::reserve_size(out);
            let colour_start = out.len();
            if c.matrix != dflt.matrix {
                ebml::write_uint(out, id::MATRIX_COEFFICIENTS, c.matrix as u64);
            }
            if c.range != dflt.range {
                ebml::write_uint(out, id::COLOUR_RANGE, c.range as u64);
            }
            if c.transfer != dflt.transfer {
                ebml::write_uint(out, id::TRANSFER_CHARACTERISTICS, c.transfer as u64);
            }
            if c.primaries != dflt.primaries {
                ebml::write_uint(out, id::PRIMARIES, c.primaries as u64);
            }
            if c.chroma_siting.0 != 0 {
                ebml::write_uint(out, id::CHROMA_SITING_HORZ, u64::from(c.chroma_siting.0));
            }
            if c.chroma_siting.1 != 0 {
                ebml::write_uint(out, id::CHROMA_SITING_VERT, u64::from(c.chroma_siting.1));
            }
            Self::write_hdr(out, &c.hdr);
            let colour_len = (out.len() - colour_start) as u64;
            ebml::patch_size(out, colour_size_at, colour_len);
        }
        let video_len = (out.len() - video_start) as u64;
        ebml::patch_size(out, video_size_at, video_len);
    }

    /// HDR static metadata back from H.265 SEI units (D.3.28) to cd/m2 and CIE 1931 floats.
    fn write_hdr(out: &mut Vec<u8>, hdr: &profluens_video::color::HdrStatic) {
        if hdr.light.max_cll != 0 {
            ebml::write_uint(out, id::MAX_CLL, u64::from(hdr.light.max_cll));
        }
        if hdr.light.max_fall != 0 {
            ebml::write_uint(out, id::MAX_FALL, u64::from(hdr.light.max_fall));
        }
        let m = &hdr.mastering;
        if *m == Default::default() {
            return;
        }
        ebml::write_id(out, id::MASTERING_METADATA);
        let size_at = ebml::reserve_size(out);
        let start = out.len();
        let xy = [
            (id::PRIMARY_R_X, m.primaries[0][0]),
            (id::PRIMARY_R_Y, m.primaries[0][1]),
            (id::PRIMARY_G_X, m.primaries[1][0]),
            (id::PRIMARY_G_Y, m.primaries[1][1]),
            (id::PRIMARY_B_X, m.primaries[2][0]),
            (id::PRIMARY_B_Y, m.primaries[2][1]),
            (id::WHITE_POINT_X, m.white[0]),
            (id::WHITE_POINT_Y, m.white[1]),
        ];
        for (element, v) in xy.into_iter().filter(|(_, v)| *v != 0) {
            ebml::write_f64(out, element, f64::from(v) / 50_000.0);
        }
        for (element, v) in
            [(id::LUMINANCE_MAX, m.max_luminance), (id::LUMINANCE_MIN, m.min_luminance)]
        {
            if v != 0 {
                ebml::write_f64(out, element, f64::from(v) / 10_000.0);
            }
        }
        let len = (out.len() - start) as u64;
        ebml::patch_size(out, size_at, len);
    }

    /// Append a frame as a `SimpleBlock`, opening a Cluster on a 16-bit window overflow or an
    /// anchor keyframe.
    pub fn write_frame(
        &mut self,
        out: &mut Vec<u8>,
        track: u64,
        timestamp_ns: u64,
        bytes: &[u8],
        keyframe: bool,
    ) -> Result<(), WriteError> {
        let tick = self.frame_tick(track, timestamp_ns)?;
        if self.need_new_cluster(tick, track, keyframe) {
            self.open_cluster(out, tick);
        }

        let cue_key = keyframe && self.cues.as_ref().is_some_and(|c| c.cue_track == track);
        let cluster = self.cluster.as_mut().expect("cluster open");
        if cue_key {
            cluster.has_cue_key = true;
        }
        let rel = (tick - cluster.base_tick) as i16; // within the checked window
        let at = cluster.buf.len() as u64;
        Self::write_simple_block(&mut cluster.buf, track, rel, bytes, keyframe);
        cluster.blocks += 1;
        self.cue_block(tick, track, at);
        let cluster = self.cluster.as_mut().expect("cluster open");
        if cluster.body_size() >= self.cluster_bytes_max || cluster.blocks >= CLUSTER_BLOCKS_MAX {
            self.spare = self.close_cluster(out);
        }
        Ok(())
    }

    /// Scatter-mode [`write_frame`](Self::write_frame). The frame rides as a refcounted [`Memory`],
    /// sizes are identical.
    pub fn write_frame_scatter(
        &mut self,
        out: &mut MuxOut,
        track: u64,
        timestamp_ns: u64,
        payload: Memory,
        keyframe: bool,
    ) -> Result<(), WriteError> {
        let tick = self.frame_tick(track, timestamp_ns)?;
        if self.need_new_cluster(tick, track, keyframe) {
            self.open_cluster_scatter(out, tick);
        }

        let cue_key = keyframe && self.cues.as_ref().is_some_and(|c| c.cue_track == track);
        let cluster = self.cluster.as_mut().expect("cluster open");
        if cue_key {
            cluster.has_cue_key = true;
        }
        let rel = (tick - cluster.base_tick) as i16; // within the checked window
        let at = cluster.body_size();
        Self::write_simple_block_header(&mut cluster.buf, track, rel, payload.len(), keyframe);
        cluster.piece_bytes += payload.len() as u64;
        cluster.pieces.push((cluster.buf.len(), payload));
        cluster.blocks += 1;
        self.cue_block(tick, track, at);
        let cluster = self.cluster.as_mut().expect("cluster open");
        if cluster.body_size() >= self.cluster_bytes_max || cluster.blocks >= CLUSTER_BLOCKS_MAX {
            self.spare = self.close_cluster_scatter(out);
        }
        Ok(())
    }

    pub fn write_header_scatter(&mut self, out: &mut MuxOut) -> Result<(), WriteError> {
        let stream_at = out.emitted();
        let mut local_start = 0u64;
        let r = out.append_bytes_with(|b| {
            local_start = b.len() as u64;
            self.write_header(b)
        });
        if r.is_ok() {
            if let Some(cues) = &mut self.cues {
                // Rebase the arena-local offset to the absolute stream offset.
                cues.segment_data_start = stream_at + (cues.segment_data_start - local_start);
            }
            if let Some(at) = &mut self.duration_at {
                *at = stream_at + (*at - local_start);
            }
        }
        r
    }

    /// Only Clusters holding a cue-track keyframe are cued, decoding must start there.
    fn record_cue(&mut self, c: &OpenCluster, cluster_at: u64) {
        let Some(cues) = &mut self.cues else { return };
        let pos = cluster_at.saturating_sub(cues.segment_data_start);
        for b in cues.blocks.iter_mut().rev().take_while(|b| b.2 == u64::MAX) {
            b.2 = pos;
        }
        if c.has_cue_key {
            cues.points.push((c.base_tick.max(0) as u64, pos));
        }
    }

    fn cue_block(&mut self, tick: i64, track: u64, at: u64) {
        let Some(cues) = &mut self.cues else { return };
        if cues.text_tracks.contains(&track) {
            cues.blocks.push((tick.max(0) as u64, track, u64::MAX, at));
        }
    }

    fn frame_tick(&self, track: u64, timestamp_ns: u64) -> Result<i64, WriteError> {
        if !self.header_written {
            return Err(WriteError::HeaderNotWritten);
        }
        if !self.tracks.iter().any(|t| t.track_number == track) {
            return Err(WriteError::UnknownTrack(track));
        }
        Ok(self.ns_to_ticks(timestamp_ns))
    }

    fn need_new_cluster(&self, tick: i64, track: u64, keyframe: bool) -> bool {
        match &self.cluster {
            None => true,
            Some(c) => {
                let rel = tick - c.base_tick;
                !(REL_TS_MIN..=REL_TS_MAX).contains(&rel)
                    || (keyframe && track == self.anchor_track && rel.abs() >= self.anchor_span)
            }
        }
    }

    fn opened(base_tick: i64, mut buf: Vec<u8>, pieces: Vec<(usize, Memory)>) -> OpenCluster {
        ebml::write_uint(&mut buf, id::TIMESTAMP, base_tick.max(0) as u64);
        OpenCluster { base_tick, buf, pieces, piece_bytes: 0, blocks: 0, has_cue_key: false }
    }

    fn open_cluster(&mut self, out: &mut Vec<u8>, base_tick: i64) {
        let (buf, pieces) = self.close_cluster(out);
        self.cluster = Some(Self::opened(base_tick, buf, pieces));
    }

    fn open_cluster_scatter(&mut self, out: &mut MuxOut, base_tick: i64) {
        let (buf, pieces) = self.close_cluster_scatter(out);
        self.cluster = Some(Self::opened(base_tick, buf, pieces));
    }

    fn close_cluster(&mut self, out: &mut Vec<u8>) -> (Vec<u8>, Vec<(usize, Memory)>) {
        let Some(mut c) = self.cluster.take() else {
            return std::mem::take(&mut self.spare);
        };
        self.record_cue(&c, out.len() as u64);
        ebml::write_id(out, id::CLUSTER);
        ebml::write_size(out, c.body_size());
        let mut cur = 0;
        for (at, mem) in c.pieces.drain(..) {
            out.extend_from_slice(&c.buf[cur..at]);
            out.extend_from_slice(mem.data());
            cur = at;
        }
        out.extend_from_slice(&c.buf[cur..]);
        c.buf.clear();
        (c.buf, c.pieces)
    }

    fn close_cluster_scatter(&mut self, out: &mut MuxOut) -> (Vec<u8>, Vec<(usize, Memory)>) {
        let Some(mut c) = self.cluster.take() else {
            return std::mem::take(&mut self.spare);
        };
        self.record_cue(&c, out.emitted());
        let body_size = c.body_size();
        out.append_bytes_with(|b| {
            ebml::write_id(b, id::CLUSTER);
            ebml::write_size(b, body_size);
        });
        let mut cur = 0;
        for (at, mem) in c.pieces.drain(..) {
            if at > cur {
                out.append_bytes_with(|b| b.extend_from_slice(&c.buf[cur..at]));
                cur = at;
            }
            out.push_payload(mem);
        }
        if cur < c.buf.len() {
            out.append_bytes_with(|b| b.extend_from_slice(&c.buf[cur..]));
        }
        c.buf.clear();
        (c.buf, c.pieces)
    }

    fn write_simple_block(
        out: &mut Vec<u8>,
        track: u64,
        rel_ts: i16,
        frame: &[u8],
        keyframe: bool,
    ) {
        Self::write_simple_block_header(out, track, rel_ts, frame.len(), keyframe);
        out.extend_from_slice(frame);
    }

    fn write_simple_block_header(
        out: &mut Vec<u8>,
        track: u64,
        rel_ts: i16,
        frame_len: usize,
        keyframe: bool,
    ) {
        ebml::write_id(out, id::SIMPLE_BLOCK);
        let track_vint_len = ebml::vint_size_len(track);
        let body_len = track_vint_len + 2 + 1 + frame_len;
        ebml::write_size(out, body_len as u64);

        ebml::write_size(out, track);
        out.extend_from_slice(&rel_ts.to_be_bytes());
        out.push(if keyframe { BLOCK_FLAG_KEYFRAME } else { 0 });
    }

    /// Emit the last Cluster. With cues it appends `Cues` and patches the SeekHead in place, so
    /// `out` must hold the whole file from the header on. A caller that drained it between
    /// calls gets no Cues, the reservation stays a valid Void.
    pub fn finalize(&mut self, out: &mut Vec<u8>) {
        let _ = self.close_cluster(out);
        if let Some(mut cues) = self.cues.take() {
            if cues.points.is_empty() && cues.blocks.is_empty() {
                return; // nothing cue-worthy, the reservation stays a valid Void
            }
            let Some(cues_pos) = (out.len() as u64).checked_sub(cues.segment_data_start) else {
                return;
            };
            Self::order_cues(&mut cues);
            write_cues(out, &cues);
            let sh = build_seekhead(&cues, cues_pos);
            let at = cues.segment_data_start as usize;
            if let Some(slot) = out.get_mut(at..at + sh.len()) {
                slot.copy_from_slice(&sh);
            }
        }
    }

    /// CuePoints go in time order (RFC 9559 section 5.1.5.1), a Cluster opened on a backward pts
    /// would otherwise put its point out of order.
    fn order_cues(cues: &mut CueState) {
        cues.points.sort_by_key(|p| p.0);
        cues.blocks.sort_by_key(|b| b.0);
    }

    /// With cues, returns the [`SeekHeadPatch`] for the caller's sink to apply.
    pub fn finalize_scatter(&mut self, out: &mut MuxOut) -> Option<SeekHeadPatch> {
        let _ = self.close_cluster_scatter(out);
        let mut cues = self.cues.take()?;
        if cues.points.is_empty() && cues.blocks.is_empty() {
            return None; // nothing cue-worthy, the reservation stays a valid Void
        }
        Self::order_cues(&mut cues);
        let cues_pos = out.emitted() - cues.segment_data_start;
        out.append_bytes_with(|b| write_cues(b, &cues));
        Some(SeekHeadPatch {
            offset: cues.segment_data_start,
            bytes: build_seekhead(&cues, cues_pos),
        })
    }
}

fn write_cues(out: &mut Vec<u8>, cues: &CueState) {
    ebml::write_id(out, id::CUES);
    let size_at = ebml::reserve_size(out);
    let body = out.len();
    let (mut k, mut b) = (0, 0);
    loop {
        let key = cues.points.get(k).map(|&(time, pos)| (time, cues.cue_track, pos, None));
        let block =
            cues.blocks.get(b).map(|&(time, track, pos, rel)| (time, track, pos, Some(rel)));
        let next = match (key, block) {
            (Some(p), Some(q)) if q.0 < p.0 => {
                b += 1;
                q
            }
            (Some(p), _) => {
                k += 1;
                p
            }
            (None, Some(q)) => {
                b += 1;
                q
            }
            (None, None) => break,
        };
        write_cue_point(out, next);
    }
    let len = (out.len() - body) as u64;
    ebml::patch_size(out, size_at, len);
}

fn write_cue_point(out: &mut Vec<u8>, (time, track, pos, rel): (u64, u64, u64, Option<u64>)) {
    ebml::write_id(out, id::CUE_POINT);
    let p_at = ebml::reserve_size(out);
    let p0 = out.len();
    ebml::write_uint(out, id::CUE_TIME, time);
    ebml::write_id(out, id::CUE_TRACK_POSITIONS);
    let tp_at = ebml::reserve_size(out);
    let tp0 = out.len();
    ebml::write_uint(out, id::CUE_TRACK, track);
    ebml::write_uint(out, id::CUE_CLUSTER_POSITION, pos);
    if let Some(rel) = rel {
        ebml::write_uint(out, id::CUE_RELATIVE_POSITION, rel);
    }
    let tp_len = (out.len() - tp0) as u64;
    ebml::patch_size(out, tp_at, tp_len);
    let p_len = (out.len() - p0) as u64;
    ebml::patch_size(out, p_at, p_len);
}

fn build_seekhead(cues: &CueState, cues_pos: u64) -> Vec<u8> {
    let mut sh = Vec::with_capacity(SEEKHEAD_RESERVE);
    ebml::write_id(&mut sh, id::SEEK_HEAD);
    let size_at = ebml::reserve_size(&mut sh);
    let body = sh.len();
    let targets: [(&[u8], u64); 3] =
        [(id::INFO, cues.info_pos), (id::TRACKS, cues.tracks_pos), (id::CUES, cues_pos)];
    for (target, pos) in targets {
        ebml::write_id(&mut sh, id::SEEK);
        let s_at = ebml::reserve_size(&mut sh);
        let s0 = sh.len();
        ebml::write_binary(&mut sh, id::SEEK_ID, target);
        ebml::write_uint(&mut sh, id::SEEK_POSITION, pos);
        let s_len = (sh.len() - s0) as u64;
        ebml::patch_size(&mut sh, s_at, s_len);
    }
    let len = (sh.len() - body) as u64;
    ebml::patch_size(&mut sh, size_at, len);
    let rest = SEEKHEAD_RESERVE - sh.len();
    debug_assert!(rest >= 2, "reservation sized for the SeekHead plus a Void header");
    ebml::write_id(&mut sh, id::VOID);
    ebml::write_size(&mut sh, (rest - 2) as u64);
    sh.resize(SEEKHEAD_RESERVE, 0);
    sh
}

/// Scatter muxer output, header byte runs interleaved with refcounted payload slices.
#[derive(Default)]
pub struct MuxOut {
    bytes: Vec<u8>,
    pieces: VecDeque<MuxPiece>,
    /// Total bytes ever queued, the absolute stream offset of whatever is queued next.
    emitted: u64,
}

pub enum MuxPiece {
    Bytes { start: usize, end: usize },
    Payload(Memory),
}

impl MuxOut {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append_bytes_with<R>(&mut self, f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
        let start = self.bytes.len();
        let r = f(&mut self.bytes);
        let end = self.bytes.len();
        if end > start {
            self.emitted += (end - start) as u64;
            if let Some(MuxPiece::Bytes { end: e, .. }) = self.pieces.back_mut()
                && *e == start
            {
                *e = end;
                return r;
            }
            self.pieces.push_back(MuxPiece::Bytes { start, end });
        }
        r
    }

    pub fn push_payload(&mut self, mem: Memory) {
        if !mem.is_empty() {
            self.emitted += mem.len() as u64;
            self.pieces.push_back(MuxPiece::Payload(mem));
        }
    }

    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    pub fn header_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn front(&self) -> Option<&MuxPiece> {
        self.pieces.front()
    }

    pub fn pieces(&self) -> impl Iterator<Item = &MuxPiece> {
        self.pieces.iter()
    }

    pub fn pop_front(&mut self) -> Option<MuxPiece> {
        self.pieces.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    pub fn reclaim(&mut self) {
        if self.pieces.is_empty() {
            self.bytes.clear();
        }
    }

    pub fn clear(&mut self) {
        self.pieces.clear();
        self.bytes.clear();
        self.emitted = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flac_track(n: u64) -> TrackConfig {
        TrackConfig::flac(n, vec![b'f', b'L', b'a', b'C', 0x80, 0, 0, 34], 48000.0, 2, 16)
    }

    fn vint_len(b: u8) -> usize {
        b.leading_zeros() as usize + 1
    }

    fn parse_size(buf: &[u8], at: &mut usize) -> u64 {
        let n = vint_len(buf[*at]);
        let mask = if n >= 8 { 0 } else { 0xFFu8 >> n }; // n=8: marker byte holds no data bits
        let mut v = (buf[*at] & mask) as u64;
        for i in 1..n {
            v = (v << 8) | buf[*at + i] as u64;
        }
        *at += n;
        v
    }

    fn parse_uint(buf: &[u8]) -> u64 {
        buf.iter().fold(0, |a, &b| (a << 8) | b as u64)
    }

    #[test]
    fn cues_and_seekhead_roundtrip() {
        let video = TrackConfig::video(1, "V_VP8", Vec::new(), 320, 240);
        let mut w = MatroskaWriter::new(vec![video, flac_track(2)]);
        w.enable_cues();
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        w.write_frame(&mut out, 1, 0, &[1, 2, 3], true).unwrap();
        w.write_frame(&mut out, 2, 5_000_000, &[9], true).unwrap();
        w.write_frame(&mut out, 1, 40_000_000, &[4, 5], false).unwrap();
        w.write_frame(&mut out, 1, 80_000_000, &[6], true).unwrap();
        w.finalize(&mut out);

        let seg = out.windows(4).position(|s| s == id::SEGMENT).expect("Segment");
        let seg_data = seg + 4 + 1;

        assert_eq!(&out[seg_data..seg_data + 4], id::SEEK_HEAD, "SeekHead is first");
        let mut at = seg_data + 4;
        let sh_len = parse_size(&out, &mut at) as usize;
        let sh_end = at + sh_len;
        let mut targets: Vec<(Vec<u8>, usize)> = Vec::new();
        while at < sh_end {
            assert_eq!(&out[at..at + 2], id::SEEK);
            at += 2;
            let len = parse_size(&out, &mut at) as usize;
            let end = at + len;
            assert_eq!(&out[at..at + 2], id::SEEK_ID);
            at += 2;
            let idlen = parse_size(&out, &mut at) as usize;
            let target = out[at..at + idlen].to_vec();
            at += idlen;
            assert_eq!(&out[at..at + 2], id::SEEK_POSITION);
            at += 2;
            let plen = parse_size(&out, &mut at) as usize;
            let pos = parse_uint(&out[at..at + plen]) as usize;
            at += plen;
            assert_eq!(at, end, "Seek entry consumed exactly");
            targets.push((target, seg_data + pos));
        }
        assert_eq!(out[sh_end], id::VOID[0], "Void filler follows the SeekHead");
        let find = |wanted: &[u8]| {
            targets.iter().find(|(t, _)| t == wanted).map(|&(_, p)| p).expect("seek target")
        };
        assert_eq!(&out[find(id::INFO)..][..4], id::INFO, "SeekHead → Info");
        assert_eq!(&out[find(id::TRACKS)..][..4], id::TRACKS, "SeekHead → Tracks");
        let cues_at = find(id::CUES);
        assert_eq!(&out[cues_at..][..4], id::CUES, "SeekHead → Cues");

        let mut at = cues_at + 4;
        let cues_len = parse_size(&out, &mut at) as usize;
        let cues_end = at + cues_len;
        let mut times = Vec::new();
        while at < cues_end {
            assert_eq!(&out[at..at + 1], id::CUE_POINT);
            at += 1;
            let plen = parse_size(&out, &mut at) as usize;
            let pend = at + plen;
            assert_eq!(&out[at..at + 1], id::CUE_TIME);
            at += 1;
            let tlen = parse_size(&out, &mut at) as usize;
            let time = parse_uint(&out[at..at + tlen]);
            at += tlen;
            assert_eq!(&out[at..at + 1], id::CUE_TRACK_POSITIONS);
            at += 1;
            let _tplen = parse_size(&out, &mut at);
            assert_eq!(&out[at..at + 1], id::CUE_TRACK);
            at += 1;
            let ctlen = parse_size(&out, &mut at) as usize;
            assert_eq!(parse_uint(&out[at..at + ctlen]), 1, "cues reference the video track");
            at += ctlen;
            assert_eq!(&out[at..at + 1], id::CUE_CLUSTER_POSITION);
            at += 1;
            let cplen = parse_size(&out, &mut at) as usize;
            let cpos = seg_data + parse_uint(&out[at..at + cplen]) as usize;
            at += cplen;
            assert_eq!(at, pend, "CuePoint consumed exactly");
            assert_eq!(&out[cpos..cpos + 4], id::CLUSTER, "cue lands on a Cluster ID");
            let mut cat = cpos + 4;
            let _csize = parse_size(&out, &mut cat);
            assert_eq!(&out[cat..cat + 1], id::TIMESTAMP);
            cat += 1;
            let tslen = parse_size(&out, &mut cat) as usize;
            assert_eq!(parse_uint(&out[cat..cat + tslen]), time, "CueTime == Cluster Timestamp");
            times.push(time);
        }
        assert_eq!(times, vec![0, 80], "both keyframe Clusters cued (1 ms ticks)");
    }

    #[test]
    fn reserved_duration_patches_in_scatter_stream() {
        use profluens_core::memory::Pool;
        let video = TrackConfig::video(1, "V_VP8", Vec::new(), 320, 240);
        let mut w = MatroskaWriter::new(vec![video]);
        w.enable_cues();
        w.reserve_duration();
        let pool = Pool::new(16);
        let mut out = MuxOut::new();
        w.write_header_scatter(&mut out).unwrap();
        for (ns, key) in [(0u64, true), (40_000_000, false), (2_000_000_000, true)] {
            let mut mem = pool.acquire_exact(3).expect("exact");
            mem.as_mut_full()[..3].copy_from_slice(&[7, 8, 9]);
            mem.set_len(3);
            w.write_frame_scatter(&mut out, 1, ns, mem, key).unwrap();
        }
        let sh_patch = w.finalize_scatter(&mut out).expect("cue-worthy clusters → patch");
        let dur_patch = w.duration_patch(2_040_000_000).expect("reserved → patch");

        let mut file = Vec::new();
        while let Some(piece) = out.pop_front() {
            match piece {
                MuxPiece::Bytes { start, end } => {
                    file.extend_from_slice(&out.header_bytes()[start..end])
                }
                MuxPiece::Payload(mem) => file.extend_from_slice(mem.data()),
            }
        }
        for p in [&sh_patch, &dur_patch] {
            file[p.offset as usize..p.offset as usize + p.bytes.len()].copy_from_slice(&p.bytes);
        }

        let mut r = crate::MatroskaReader::new();
        r.push(&file).expect("patched stream parses");
        assert_eq!(r.duration_ns(), Some(2_040_000_000), "patched Duration read back");
        assert_eq!(r.tracks().len(), 1, "Tracks parse past the patched Info");
    }

    #[test]
    fn header_requires_tracks() {
        let mut w = MatroskaWriter::new(vec![]);
        let mut out = Vec::new();
        assert_eq!(w.write_header(&mut out), Err(WriteError::BadTracks));
    }

    #[test]
    fn duplicate_track_number_rejected() {
        let mut w = MatroskaWriter::new(vec![flac_track(1), flac_track(1)]);
        let mut out = Vec::new();
        assert_eq!(w.write_header(&mut out), Err(WriteError::BadTracks));
    }

    #[test]
    fn frame_before_header_rejected() {
        let mut w = MatroskaWriter::new(vec![flac_track(1)]);
        let mut out = Vec::new();
        assert_eq!(
            w.write_frame(&mut out, 1, 0, &[1, 2, 3], true),
            Err(WriteError::HeaderNotWritten)
        );
    }

    #[test]
    fn frame_on_unknown_track_rejected() {
        let mut w = MatroskaWriter::new(vec![flac_track(1)]);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        assert_eq!(w.write_frame(&mut out, 9, 0, &[1], true), Err(WriteError::UnknownTrack(9)));
    }

    #[test]
    fn header_starts_with_ebml_id_and_matroska_doctype() {
        let mut w = MatroskaWriter::new(vec![flac_track(1)]);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        assert_eq!(&out[..4], id::EBML, "starts with the EBML Header ID 1A45DFA3");
        assert!(out.windows(8).any(|w| w == b"matroska"), "DocType matroska present");
        let seg = out.windows(4).position(|w| w == id::SEGMENT).expect("Segment id");
        assert_eq!(out[seg + 4], 0xFF, "Segment opened with unknown size");
    }

    #[test]
    fn simple_block_body_layout() {
        let mut out = Vec::new();
        MatroskaWriter::write_simple_block(&mut out, 1, 123, &[0xAA, 0xBB, 0xCC], true);
        assert_eq!(&out[..1], id::SIMPLE_BLOCK); // 0xA3
        assert_eq!(out[1], 0x87, "body length 7");
        assert_eq!(out[2], 0x81, "track number 1 as VINT");
        assert_eq!(&out[3..5], &123i16.to_be_bytes(), "relative timestamp big-endian");
        assert_eq!(out[5], 0x80, "keyframe flag");
        assert_eq!(&out[6..], &[0xAA, 0xBB, 0xCC], "frame verbatim");
    }

    #[test]
    fn negative_relative_timestamp_is_signed() {
        let mut out = Vec::new();
        MatroskaWriter::write_simple_block(&mut out, 2, -5, &[0x01], false);
        assert_eq!(out[2], 0x82);
        assert_eq!(&out[3..5], &(-5i16).to_be_bytes());
        assert_eq!(out[5], 0x00, "no keyframe flag");
    }

    #[test]
    fn multibyte_track_number_body_length() {
        let mut out = Vec::new();
        MatroskaWriter::write_simple_block(&mut out, 200, 0, &[0x09], true);
        assert_eq!(out[1], 0x86, "body length 6 with 2-octet track VINT");
        assert_eq!(&out[2..4], &[0x40, 0xC8]);
    }

    #[test]
    fn anchor_keyframe_opens_new_cluster() {
        let video = TrackConfig::video(1, "V_VP8", Vec::new(), 320, 240);
        let mut w = MatroskaWriter::new(vec![video]);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        w.write_frame(&mut out, 1, 0, &[1], true).unwrap(); // opens cluster #1 (staged)
        w.write_frame(&mut out, 1, 1_000_000, &[2], false).unwrap(); // same cluster (1 tick)
        assert_eq!(count_clusters(&out), 0, "open cluster is staged, not yet emitted");
        w.write_frame(&mut out, 1, 2_000_000, &[3], true).unwrap(); // keyframe opens a new cluster
        assert_eq!(count_clusters(&out), 1, "anchor keyframe closed+emitted cluster #1");
        w.finalize(&mut out);
        assert_eq!(count_clusters(&out), 2, "finalize emitted the last cluster");
    }

    #[test]
    fn an_audio_anchor_splits_its_clusters_on_time() {
        let mut w = MatroskaWriter::new(vec![flac_track(1)]);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        for n in 0..5u64 {
            w.write_frame(&mut out, 1, n * 1_000_000_000, &[n as u8], true).unwrap();
        }
        assert_eq!(count_clusters(&out), 0, "every frame a keyframe, one Cluster for five seconds");
        w.write_frame(&mut out, 1, AUDIO_CLUSTER_NS, &[5], true).unwrap();
        assert_eq!(count_clusters(&out), 1, "the keyframe five seconds on opens the next");
        w.finalize(&mut out);
        assert_eq!(count_clusters(&out), 2);
    }

    #[test]
    fn a_backward_pts_keeps_the_cue_points_in_time_order() {
        let video = TrackConfig::video(1, "V_VP8", Vec::new(), 320, 240);
        let mut w = MatroskaWriter::new(vec![video]);
        w.enable_cues();
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        w.write_frame(&mut out, 1, 40_000 * 1_000_000, &[1], true).unwrap();
        w.write_frame(&mut out, 1, 0, &[2], true).unwrap(); // past the i16 window, a lower base
        w.write_frame(&mut out, 1, 80_000 * 1_000_000, &[3], true).unwrap();
        w.finalize(&mut out);
        let times = cue_times(&out);
        assert_eq!(times, vec![0, 40_000, 80_000], "ascending, not in Cluster order");
    }

    #[test]
    fn timestamp_overflow_opens_new_cluster() {
        let mut w = MatroskaWriter::new(vec![flac_track(1)]);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        w.write_frame(&mut out, 1, 0, &[1], true).unwrap(); // cluster base tick 0
        w.write_frame(&mut out, 1, 40_000 * 1_000_000, &[2], false).unwrap();
        assert_eq!(count_clusters(&out), 1, "ts overflow closed+emitted the first cluster");
        w.finalize(&mut out);
        assert_eq!(count_clusters(&out), 2, "finalize emitted the overflow cluster");
    }

    #[test]
    fn long_gop_splits_at_the_cluster_cap() {
        use profluens_core::memory::Pool;
        const FRAME: usize = 1000;
        let video = TrackConfig::video(1, "V_VP8", Vec::new(), 320, 240);
        let mut w = MatroskaWriter::new(vec![video]);
        w.enable_cues();
        w.set_cluster_bytes_max(4 * FRAME as u64);
        let pool = Pool::new(FRAME);
        let mut out = MuxOut::new();
        w.write_header_scatter(&mut out).unwrap();
        for i in 0..20u64 {
            let mut mem = pool.acquire_exact(FRAME).expect("exact");
            mem.as_mut_full()[..FRAME].fill(i as u8);
            mem.set_len(FRAME);
            w.write_frame_scatter(&mut out, 1, i * 40_000_000, mem, i == 0).unwrap();
        }
        let patch = w.finalize_scatter(&mut out).expect("a keyframe Cluster is cued");
        let mut file = Vec::new();
        while let Some(piece) = out.pop_front() {
            match piece {
                MuxPiece::Bytes { start, end } => {
                    file.extend_from_slice(&out.header_bytes()[start..end])
                }
                MuxPiece::Payload(mem) => file.extend_from_slice(mem.data()),
            }
        }
        file[patch.offset as usize..patch.offset as usize + patch.bytes.len()]
            .copy_from_slice(&patch.bytes);

        let mut at = file.windows(4).position(|s| s == id::CLUSTER).expect("a Cluster");
        let mut sizes = Vec::new();
        while file[at..].starts_with(id::CLUSTER) {
            at += 4;
            let size = parse_size(&file, &mut at);
            sizes.push(size);
            at += size as usize;
        }
        assert_eq!(sizes.len(), 5, "20 frames of 1000 at a 4000 cap: {sizes:?}");
        assert!(
            sizes.iter().all(|&n| n < 5 * FRAME as u64 + 64),
            "a Cluster holds the cap and one frame: {sizes:?}"
        );
        assert!(file[at..].starts_with(id::CUES), "Cues follow the last Cluster");
        at += 4;
        let end = parse_size(&file, &mut at) as usize + at;
        let mut points = 0;
        while at < end {
            assert_eq!(&file[at..at + 1], id::CUE_POINT);
            at += 1;
            at += parse_size(&file, &mut at) as usize;
            points += 1;
        }
        assert_eq!(points, 1, "only the keyframe's Cluster is cued");

        let mut r = crate::MatroskaReader::new();
        r.push(&file).expect("parses");
        for i in 0..20u64 {
            let f = r.next_frame().expect("frame");
            assert_eq!(f.pts_ns, i * 40_000_000);
            assert_eq!(f.keyframe, i == 0, "frame {i} keyframe bit");
            assert!(
                f.data.iter().all(|&b| b == i as u8) && f.data.len() == FRAME,
                "frame {i} bytes"
            );
        }
        assert!(r.next_frame().is_none());
    }

    #[test]
    fn lifted_cap_keeps_one_cluster_per_gop() {
        let mut w = MatroskaWriter::new(vec![flac_track(1)]);
        w.set_cluster_bytes_max(u64::MAX);
        let mut out = Vec::new();
        w.write_header(&mut out).unwrap();
        let frame = vec![0u8; 1 << 20];
        for i in 0..8u64 {
            w.write_frame(&mut out, 1, i * 1_000_000, &frame, i == 0).unwrap();
        }
        w.finalize(&mut out);
        let at = out.windows(4).position(|s| s == id::CLUSTER).expect("a Cluster");
        let mut p = at + 4;
        let size = parse_size(&out, &mut p);
        assert!(size > 8 << 20, "one Cluster holds the GOP: {size}");
    }

    /// The CueTimes of the Cues element, which comes after every Cluster.
    fn cue_times(out: &[u8]) -> Vec<u64> {
        let cues_at = out.windows(4).rposition(|w| w == id::CUES).expect("a Cues element");
        let mut at = cues_at + 4;
        let len = parse_size(out, &mut at) as usize;
        let end = at + len;
        let mut times = Vec::new();
        while at < end {
            assert_eq!(&out[at..at + 1], id::CUE_POINT);
            at += 1;
            let plen = parse_size(out, &mut at) as usize;
            let pend = at + plen;
            assert_eq!(&out[at..at + 1], id::CUE_TIME);
            at += 1;
            let tlen = parse_size(out, &mut at) as usize;
            times.push(parse_uint(&out[at..at + tlen]));
            at = pend;
        }
        times
    }

    fn count_clusters(out: &[u8]) -> usize {
        out.windows(4).filter(|w| *w == id::CLUSTER).count()
    }
}
