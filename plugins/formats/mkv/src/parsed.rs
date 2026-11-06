//! The head before the first Cluster parsed once, for every demuxer over one file.

use std::sync::Arc;

use crate::error::DemuxError;
use crate::reader::{Cue, MatroskaReader, SeekHeadInfo, Track, parse_seek_head};
use crate::tags::{SegmentIndex, index_segment};

/// The reader takes the head in pieces, so an element it skips, attached fonts as a rule, is
/// never copied into it.
const PIECE: usize = 64 << 10;

/// What a Matroska head states and where the rest of the Segment's metadata lies, with the Cues
/// once read. Parsed once at open and shared by the demuxer of every run, so none parses the head
/// again or holds a copy of it.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub tracks: Vec<Track>,
    /// Nanoseconds a tick, the TimestampScale.
    pub tick: u64,
    pub duration_ns: Option<u64>,
    /// The front SeekHead, None without a Segment.
    pub seek_head: Option<SeekHeadInfo>,
    /// Where the Tags, Attachments and Chapters lie, the head's own or its SeekHead's.
    pub index: Option<SegmentIndex>,
    /// The Segment's end, None for an unknown size.
    pub segment_end: Option<u64>,
    /// The head's length unless whoever read the head walked to the Cluster past a cut.
    pub first_cluster: u64,
    /// Every track's CuePoints, empty when not read or absent.
    pub cues: Arc<[Cue]>,
    /// Where the Cues were read, None for none.
    pub cued_at: Option<u64>,
}

impl Parsed {
    /// The head before the first Cluster, or one cut short of it after Tracks.
    // Cold path, once per open.
    #[allow(clippy::disallowed_methods)]
    pub fn of(head: &[u8]) -> Result<Parsed, DemuxError> {
        let mut r = MatroskaReader::new();
        for piece in head.chunks(PIECE) {
            r.push(piece).map_err(DemuxError::Header)?;
        }
        if !r.tracks_ready() {
            return Err(DemuxError::TracksIncomplete);
        }
        Ok(Parsed {
            tracks: r.tracks().to_vec(),
            tick: r.timestamp_scale(),
            duration_ns: r.duration_ns(),
            seek_head: parse_seek_head(head),
            index: index_segment(head),
            segment_end: crate::index::segment_end(head),
            first_cluster: head.len() as u64,
            cues: Arc::from([]),
            cued_at: None,
        })
    }

    /// Absolute, a Segment Position of the front SeekHead.
    fn at(&self, pos: impl Fn(&SeekHeadInfo) -> Option<u64>) -> Option<u64> {
        let sh = self.seek_head.as_ref()?;
        pos(sh).map(|p| sh.segment_data_start.saturating_add(p))
    }

    /// Where the front SeekHead puts the Cues.
    pub fn cues_pos(&self) -> Option<u64> {
        self.at(|sh| sh.cues_pos)
    }

    /// Where the front SeekHead puts a SeekHead past the head.
    pub fn next_head(&self) -> Option<u64> {
        self.at(|sh| sh.next_head)
    }

    pub fn segment_data_start(&self) -> u64 {
        self.seek_head.map_or(0, |sh| sh.segment_data_start)
    }

    /// About the heap it holds, the Cues and the tracks' strings and codec heads.
    pub fn bytes(&self) -> usize {
        let track = |t: &Track| {
            size_of::<Track>()
                + t.codec_id.len()
                + t.codec_private.len()
                + t.language.len()
                + t.name.len()
        };
        size_of_val(&*self.cues) + self.tracks.iter().map(track).sum::<usize>()
    }
}
