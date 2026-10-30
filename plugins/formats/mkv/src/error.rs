//! Typed failures of the Matroska elements, carried on `Error::ElementFailed` as [`MkvError`].

use std::fmt;

use crate::codec::ReframeError;
use crate::config::ConfigError;
use crate::ebml::ReadError;
use crate::encoding::Refusal;
use crate::inflate::InflateError;
use crate::writer::WriteError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MkvError {
    Demux(DemuxError),
    Mux(MuxError),
    MuxN(MuxError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DemuxError {
    Header(ReadError),
    TracksIncomplete,
    Parse(ReadError),
    /// Damage inside the Segment at stream offset `at`, skipped to the next Cluster.
    Damaged {
        at: u64,
        cause: ReadError,
    },
    FrameDropped {
        track: u64,
        cause: ReframeError,
    },
    Undecoded {
        track: u64,
        cause: InflateError,
    },
    TrackRefused {
        track: u64,
        codec: Box<str>,
        why: Refusal,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MuxError {
    FlacMagic,
    FlacHeadShort {
        len: usize,
    },
    FlacHeadTooLarge {
        len: usize,
    },
    FlacNotStreamInfo {
        block_type: u8,
    },
    FlacZeroRate,
    FlacHeadSplit {
        head: usize,
        buffered: usize,
    },
    ConfigRecord {
        version: Option<u8>,
    },
    Config(ConfigError),
    AacConfigShort {
        len: usize,
    },
    DataBeforeFormat,
    VideoSizeMissing,
    AacParamsMissing,
    /// An Opus lane of more than two channels led with no OpusHead, whose stream mapping only
    /// the encoder knows.
    OpusHeadMissing {
        channels: u32,
    },
    UnsupportedFamily {
        family: Box<str>,
    },
    FormatChangeAfterHeader,
    FormatChangeUnattributed,
    PrebufferFull {
        buffers: usize,
        bytes: usize,
        waiting: SinkPads,
    },
    TracksDropped {
        pads: SinkPads,
    },
    NoSinkLinked,
    LinkedCount {
        expected: usize,
        linked: usize,
    },
}

/// A set of `mkvmuxn` sink pads. Bit `i` is `sink_i`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkPads(pub u8);

impl SinkPads {
    pub fn insert(&mut self, pad: u32) {
        debug_assert!(pad < 8, "mkvmuxn has sink_0..sink_7");
        self.0 |= 1u8 << (pad & 7);
    }

    pub fn contains(self, pad: u32) -> bool {
        pad < 8 && self.0 & (1u8 << pad) != 0
    }

    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for SinkPads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for pad in (0..8).filter(|p| self.contains(*p)) {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "sink_{pad}")?;
        }
        Ok(())
    }
}

impl fmt::Display for MkvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MkvError::Demux(e) => write!(f, "mkvdemux: {e}"),
            MkvError::Mux(e) => write!(f, "mkvmux: {e}"),
            MkvError::MuxN(e) => write!(f, "mkvmuxn: {e}"),
        }
    }
}

impl fmt::Display for DemuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DemuxError::Header(e) => write!(f, "header parse failed, {e}"),
            DemuxError::TracksIncomplete => f.write_str(
                "constructor header does not contain a complete Tracks element, pass more \
                 of the stream head (through the first Cluster)",
            ),
            DemuxError::Parse(e) => write!(f, "parse error, {e}"),
            DemuxError::Damaged { at, cause } => {
                write!(f, "skipping damaged data at octet {at}, {cause}")
            }
            DemuxError::FrameDropped { track, cause } => {
                write!(f, "dropping malformed frame on track {track}, {cause}")
            }
            DemuxError::Undecoded { track, cause } => {
                write!(
                    f,
                    "dropping frame on track {track} whose content encoding does not undo, {cause}"
                )
            }
            DemuxError::TrackRefused { track, codec, why } => {
                write!(f, "track {track} ({codec}) is {why} and cannot be read")
            }
        }
    }
}

impl fmt::Display for MuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MuxError::FlacMagic => f.write_str("announced flac stream does not start with fLaC"),
            MuxError::FlacHeadShort { len } => {
                write!(f, "flac head of {len} bytes is too short for STREAMINFO")
            }
            MuxError::FlacHeadTooLarge { len } => {
                write!(f, "flac head reached {len} bytes with no last metadata block")
            }
            MuxError::FlacNotStreamInfo { block_type } => {
                write!(f, "first flac metadata block is type {block_type}, not STREAMINFO")
            }
            MuxError::FlacZeroRate => f.write_str("STREAMINFO sample rate is zero"),
            MuxError::FlacHeadSplit { head, buffered } => write!(
                f,
                "flac head not on a buffer boundary ({head} byte head, {buffered} buffered)"
            ),
            MuxError::ConfigRecord { version: Some(v) } => write!(
                f,
                "leading buffer is not an avcC/hvcC record (configurationVersion {v}, not 1)"
            ),
            MuxError::ConfigRecord { version: None } => {
                f.write_str("leading buffer is not an avcC/hvcC record (empty)")
            }
            MuxError::Config(e) => {
                write!(f, "the CodecPrivate could not be built from the stream, {e}")
            }
            MuxError::AacConfigShort { len } => write!(
                f,
                "leading aac buffer of {len} bytes is too short to be an AudioSpecificConfig"
            ),
            MuxError::DataBeforeFormat => f.write_str(
                "data before any format announcement, a caps-driven muxer cannot guess its \
                 codec (use MkvMux::with_track for bare byte streams)",
            ),
            MuxError::VideoSizeMissing => f.write_str("video announcement without width/height"),
            MuxError::OpusHeadMissing { channels } => write!(
                f,
                "opus lane of {channels} channels without a leading OpusHead, a synthesized \
                 head maps at most two"
            ),
            MuxError::AacParamsMissing => f.write_str("aac announcement without rate/channels"),
            MuxError::UnsupportedFamily { family } => write!(
                f,
                "cannot remux family '{family}' (av1 needs an av1C CodecPrivate, for \
                 h264/h265 use the demuxer's passthrough mode, which announces h264/avcc \
                 or h265/hvcc)"
            ),
            MuxError::FormatChangeAfterHeader => f.write_str(
                "mid-stream format change cannot be muxed (track is fixed once the header \
                 is written)",
            ),
            MuxError::FormatChangeUnattributed => f.write_str(
                "cannot attribute a FormatChange to any linked sink pad (no awaiting pad's \
                 negotiated format matches the announcement)",
            ),
            MuxError::PrebufferFull { buffers, bytes, waiting } => write!(
                f,
                "{buffers} buffers / {bytes} bytes queued but pad(s) {waiting} never \
                 announced a format, the header needs every linked track (unlink dead \
                 pads or fix the upstream announcement)"
            ),
            MuxError::TracksDropped { pads } => write!(
                f,
                "pad(s) {pads} never announced a format, their tracks are dropped from the \
                 finalized file"
            ),
            MuxError::NoSinkLinked => f.write_str("no sink pad linked (link sink_0..sink_{n-1})"),
            MuxError::LinkedCount { expected, linked } => {
                write!(f, "constructed for {expected} track(s) but {linked} sink pad(s) are linked")
            }
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Incomplete => f.write_str("element incomplete, more bytes needed"),
            ReadError::VintTooLong => f.write_str("VINT wider than 8 octets"),
            ReadError::Malformed(what) => write!(f, "malformed, {what}"),
        }
    }
}

impl fmt::Display for ReframeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReframeError::Truncated(what) => write!(f, "truncated, {what}"),
            ReframeError::Malformed(what) => write!(f, "malformed, {what}"),
        }
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::UnknownTrack(n) => write!(f, "frame for undeclared track {n}"),
            WriteError::HeaderNotWritten => f.write_str("frame before the header was written"),
            WriteError::BadTracks => f.write_str("no tracks, or a duplicate or zero track number"),
        }
    }
}

impl std::error::Error for MkvError {}
impl std::error::Error for DemuxError {}
impl std::error::Error for MuxError {}
impl std::error::Error for ReadError {}
impl std::error::Error for ReframeError {}
impl std::error::Error for WriteError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_pads_render_by_name() {
        let mut pads = SinkPads::default();
        assert!(pads.is_empty());
        pads.insert(1);
        pads.insert(3);
        assert!(pads.contains(1) && pads.contains(3) && !pads.contains(0));
        assert_eq!(pads.len(), 2);
        assert_eq!(format!("{pads}"), "sink_1, sink_3");
    }

    #[test]
    fn the_element_names_its_own_failures() {
        let e = MuxError::LinkedCount { expected: 2, linked: 1 };
        assert_eq!(
            format!("{}", MkvError::MuxN(e.clone())),
            "mkvmuxn: constructed for 2 track(s) but 1 sink pad(s) are linked"
        );
        assert!(format!("{}", MkvError::Mux(e)).starts_with("mkvmux: "));
        let d = DemuxError::FrameDropped { track: 2, cause: ReframeError::Truncated("NAL length") };
        assert_eq!(
            format!("{}", MkvError::Demux(d)),
            "mkvdemux: dropping malformed frame on track 2, truncated, NAL length"
        );
    }

    #[test]
    fn every_message_is_one_plain_line() {
        let waiting = SinkPads(0b10);
        let all = [
            MuxError::FlacMagic,
            MuxError::FlacHeadShort { len: 9 },
            MuxError::FlacNotStreamInfo { block_type: 4 },
            MuxError::FlacZeroRate,
            MuxError::FlacHeadSplit { head: 42, buffered: 100 },
            MuxError::ConfigRecord { version: Some(0) },
            MuxError::ConfigRecord { version: None },
            MuxError::AacConfigShort { len: 1 },
            MuxError::Config(ConfigError::NoSps),
            MuxError::DataBeforeFormat,
            MuxError::VideoSizeMissing,
            MuxError::AacParamsMissing,
            MuxError::UnsupportedFamily { family: "av1".into() },
            MuxError::FormatChangeAfterHeader,
            MuxError::FormatChangeUnattributed,
            MuxError::PrebufferFull { buffers: 4096, bytes: 1 << 20, waiting },
            MuxError::TracksDropped { pads: waiting },
            MuxError::NoSinkLinked,
            MuxError::LinkedCount { expected: 2, linked: 1 },
        ];
        for e in all {
            let s = format!("{}", MkvError::MuxN(e));
            assert!(!s.contains('\n') && !s.contains('\u{2014}'), "{s}");
            assert!(!s.ends_with(' ') && s.len() > "mkvmuxn: ".len(), "{s}");
        }
        let s = format!("{}", MuxError::PrebufferFull { buffers: 4096, bytes: 77, waiting });
        for n in ["4096 buffers", "77 bytes", "sink_1", "never announced"] {
            assert!(s.contains(n), "{s:?} should mention {n:?}");
        }
    }
}
