// The caller's open file goes to the source element, once per run.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::fmt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use pf_avi::AviDemux;
use pf_bare::BareSrc;
use pf_flv::FlvDemux;
use pf_mkv::MkvDemux;
use pf_mp4::Mp4Demux;
use pf_ogg::OggDemux;
use profluens_core::error::Error;
use profluens_core::id::{ElementId, SinkPad, SrcPad};
use profluens_core::pipeline::Pipeline;
use profluens_elements::io::FileSrc;

use crate::{Index, Kind, Opened, TrackDesc, TrackKind};

/// The bytes a demuxer's buffers may hold at once. A sample past it never goes out, an intra
/// coded 4K frame of 10 MB stalls the default 8 MB.
pub const DEMUX_BYTES: u64 = 64 << 20;
const POOL_SLOT: usize = 128 << 10;
const POOL_SLOTS: u32 = 64;

/// What [`demuxer`] refuses.
#[derive(Clone, Debug)]
pub enum BuildError {
    /// No demuxer is built here for the container, or the open carries no head for it.
    Unsupported(Kind),
    Pipeline(Error),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Unsupported(k) => write!(f, "no demuxer for {}", k.label()),
            BuildError::Pipeline(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<Error> for BuildError {
    fn from(e: Error) -> Self {
        BuildError::Pipeline(e)
    }
}

/// Where a track comes out of the demuxer, a stream to select or a pad that is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    Stream(u32),
    Pad(SrcPad),
}

impl Out {
    /// The src pad, the stream selected.
    pub fn pad(self, p: &mut Pipeline, demux: ElementId) -> Result<SrcPad, Error> {
        match self {
            Out::Stream(i) => p.select(demux, i),
            Out::Pad(pad) => Ok(pad),
        }
    }
}

/// `filesrc ! demuxer` for `o`, or the `baresrc` that reads a bare file itself, the element the
/// tracks come out of. The demuxer starts from the head, the Cues, the idx1 index and the
/// duration the open read, so it reads none of them. A `held` Matroska demuxer reads nothing
/// until the seek that follows its start.
pub fn demuxer(
    p: &mut Pipeline,
    file: File,
    path: &Path,
    o: &Opened,
    held: bool,
) -> Result<ElementId, BuildError> {
    let (demux, input) = match (o.kind, &o.index) {
        (Kind::Flac | Kind::Mp3 | Kind::AdtsAac, Index::Bare(h)) => {
            return Ok(p.add(BareSrc::new(file, (**h).clone(), o.len)));
        }
        (Kind::Mkv, index) => {
            // tags come from the open, attachments and chapters have no user
            let mut d = MkvDemux::with_shared_head(Arc::clone(&o.head)).without_extras();
            if let Index::Mkv { cues, at, .. } = index
                && !cues.is_empty()
            {
                d = d.with_shared_cues(Arc::clone(cues), *at);
            }
            if held {
                d = d.with_held_start();
            }
            (p.add(d), MkvDemux::IN)
        }
        (Kind::Mp4, _) => (p.add(Mp4Demux::new(o.head.to_vec())), Mp4Demux::IN),
        (Kind::Avi, index) => {
            let mut d = AviDemux::new(o.head.to_vec()).with_len(Some(o.len));
            if let Index::Avi(ix) = index {
                d = d.with_index(Arc::clone(ix));
            }
            (p.add(d), AviDemux::IN)
        }
        (Kind::Ogg, _) => {
            let mut d = OggDemux::with_len(Some(o.len)).with_head(o.head.to_vec());
            if o.duration_ns > 0 {
                d = d.with_duration(o.duration_ns);
            }
            (p.add(d), OggDemux::IN)
        }
        (Kind::Flv, _) => (p.add(FlvDemux::new(o.head.to_vec()).with_len(o.len)), FlvDemux::IN),
        (k, _) => return Err(BuildError::Unsupported(k)),
    };
    link(p, file, path, demux, input)?;
    Ok(demux)
}

fn link(
    p: &mut Pipeline,
    file: File,
    path: &Path,
    demux: ElementId,
    input: SinkPad,
) -> Result<(), BuildError> {
    let src = p.add(FileSrc::from_file(file, path));
    p.link(src.src(FileSrc::OUT), demux.sink(input))?;
    p.set_element_pool_bytes(demux, POOL_SLOT, POOL_SLOTS, DEMUX_BYTES);
    Ok(())
}

/// The number at the end of a stream's name behind `prefix`, read in `radix`.
fn named(name: &str, prefix: &str, radix: u32) -> Option<u64> {
    u64::from_str_radix(name.strip_prefix(prefix)?, radix).ok()
}

/// Where `t` comes out of `demux` once prerolled. Matroska and MP4 name streams by the track
/// number, Ogg by the serial, FLV by the kind, AVI has a pad per stream ordinal, `baresrc` its
/// one pad.
pub fn route(p: &Pipeline, demux: ElementId, o: &Opened, t: &TrackDesc) -> Option<Out> {
    let streams = p.streams(demux);
    let find = |prefix: &str, radix: u32, want: u64| {
        let at = streams.iter().position(|s| named(&s.name, prefix, radix) == Some(want));
        at.map(|i| Out::Stream(i as u32))
    };
    match o.kind {
        Kind::Mkv | Kind::Mp4 => find("src_track", 10, u64::from(t.id)),
        Kind::Ogg => find("src_", 16, u64::from(t.stream)),
        Kind::Flv => {
            let name = match t.kind {
                TrackKind::Video => "src_video",
                TrackKind::Audio => "src_audio",
                _ => return None,
            };
            let at = streams.iter().position(|s| &*s.name == name)?;
            Some(Out::Stream(at as u32))
        }
        Kind::Avi => p.src_pad(demux, &format!("src_stream{}", t.stream)).ok().map(Out::Pad),
        Kind::Flac | Kind::Mp3 | Kind::AdtsAac => (t.id == 1).then_some(Out::Pad(BareSrc::OUT)),
        _ => None,
    }
}
