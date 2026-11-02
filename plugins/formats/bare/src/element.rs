//! `baresrc`, the source of a bare FLAC, MP3 or ADTS file. It reads the file itself with
//! positioned reads through a [`Reader`], a frame per buffer, and takes a seek's target time
//! from the flush, the reader finds the frame. A run opens with the codec head flagged HEADER
//! as the Matroska demuxer does it. The index it publishes has no entries, so the pipeline
//! plans by time and a snap seek lands on its target, every frame is a key.

// The caller's open file, read in place with positioned reads, see `BareSrc::new`.
#[allow(clippy::disallowed_types)]
use std::fs::File;
use std::os::unix::fs::FileExt as _;

use profluens_core::batch::Inputs;
use profluens_core::buffer::BufferFlags;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::{OfferDesc, ValueDesc};
use profluens_core::id::SrcPad;
use profluens_core::pipeline::{Index, Serves};
use profluens_core::time::Timestamp;
use std::fmt;

use crate::BareError;
use crate::head::BareHead;
use crate::reader::{Reader, Unit};

/// Frames put out per process call at most, the scheduler comes back for more.
const BURST: usize = 64;

static FLAC: [OfferDesc; 1] = [OfferDesc::any("flac")];
static MP3: [OfferDesc; 1] = [OfferDesc::any("mp3")];
static AAC: [OfferDesc; 1] = [OfferDesc::any("aac")];
static ANY: [OfferDesc; 3] = [OfferDesc::any("flac"), OfferDesc::any("mp3"), OfferDesc::any("aac")];
static FLAC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &FLAC }];
static MP3_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &MP3 }];
static AAC_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &AAC }];
static ANY_PADS: [PadDesc; 1] = [PadDesc { name: "src", offers: &ANY }];

const fn desc(srcs: &'static [PadDesc]) -> ElementDesc {
    ElementDesc {
        name: "baresrc",
        sinks: &[],
        srcs,
        props: &[],
        sched: SchedHint::Active,
        inputs: InputPolicy::None,
        latency: LatencyDesc::NONE,
        io: false,
        make_default: None,
        couplings: &[],
    }
}

static FLAC_DESC: ElementDesc = desc(&FLAC_PADS);
static MP3_DESC: ElementDesc = desc(&MP3_PADS);
static AAC_DESC: ElementDesc = desc(&AAC_PADS);
/// What the registry lists, a running element offers its file's one family.
pub(crate) static REGISTRY_DESC: ElementDesc = desc(&ANY_PADS);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareSrcError {
    /// Read before `start` or after `stop`.
    NotStarted,
    Read(BareError),
}

impl fmt::Display for BareSrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BareSrcError::NotStarted => write!(f, "baresrc: not started"),
            BareSrcError::Read(e) => write!(f, "baresrc: {e}"),
        }
    }
}

impl std::error::Error for BareSrcError {}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The head goes out before the first frame.
    Head,
    Frames,
    Ended,
}

pub struct BareSrc {
    // Read in place, a confined open must not be resolved again.
    #[allow(clippy::disallowed_types)]
    file: File,
    head: BareHead,
    len: u64,
    started: bool,
    rd: Reader,
    phase: Phase,
    /// A frame read that no buffer took yet.
    held: Option<Unit>,
}

impl BareSrc {
    pub const OUT: SrcPad = SrcPad(0);

    /// Reads `file`, already open and of `len` bytes, whose head [`crate::open`] read.
    #[allow(clippy::disallowed_types)]
    pub fn new(file: File, head: BareHead, len: u64) -> Self {
        let rd = Reader::new(&head);
        BareSrc { file, head, len, started: false, rd, phase: Phase::Head, held: None }
    }

    fn fail(ctx: &Ctx, e: BareSrcError) -> Error {
        Error::element_failed(ctx.element(), e)
    }

    fn announce(&self, ctx: &mut Ctx) {
        let fields = [
            ("rate", ValueDesc::Int(i64::from(self.head.rate))),
            ("channels", ValueDesc::Int(i64::from(self.head.channels()))),
        ];
        ctx.announce_format(Self::OUT, self.head.family, &fields);
    }

    /// The next frame, the held one first.
    fn frame(&mut self, ctx: &Ctx) -> Result<Option<Unit>, Error> {
        if let Some(u) = self.held.take() {
            return Ok(Some(u));
        }
        if !self.started {
            return Err(Self::fail(ctx, BareSrcError::NotStarted));
        }
        let f = &self.file;
        // A blocking pread of at most a window chunk, the reader holds what it read.
        #[allow(clippy::disallowed_methods)]
        let mut read = |off: u64, buf: &mut [u8]| f.read_at(buf, off);
        self.rd.next(&self.head, &mut read).map_err(|e| Self::fail(ctx, BareSrcError::Read(e)))
    }
}

impl Element for BareSrc {
    fn desc(&self) -> &'static ElementDesc {
        match self.head.family {
            "flac" => &FLAC_DESC,
            "mp3" => &MP3_DESC,
            _ => &AAC_DESC,
        }
    }

    fn preroll(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        ctx.publish_serves(Serves::FILE);
        let duration = u64::try_from(self.head.duration_ns).ok().filter(|&d| d > 0);
        ctx.publish_index(Index {
            file_len: Some(self.len),
            duration: duration.map_or(Timestamp::NONE, Timestamp),
            ..Index::default()
        });
        if let Some(d) = duration {
            ctx.set_duration(d);
        }
        Ok(())
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.started = true;
        (self.rd, self.phase, self.held) = (Reader::new(&self.head), Phase::Head, None);
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, _inputs: Inputs<'_>) -> Result<Flow, Error> {
        for _ in 0..BURST {
            if self.phase == Phase::Ended {
                return Ok(Flow::Eos);
            }
            let Some(u) = self.frame(ctx)? else {
                self.phase = Phase::Ended;
                return Ok(Flow::Eos);
            };
            if self.phase == Phase::Head {
                self.announce(ctx);
                if !self.head.head.is_empty() {
                    let n = self.head.head.len();
                    let Some(mut b) = ctx.alloc_exact(n) else { return Err(ctx.exact_refused(n)) };
                    b.memory.as_mut_full()[..n].copy_from_slice(&self.head.head);
                    b.memory.set_len(n);
                    (b.pts, b.flags) = (Timestamp(u.pts_ns.max(0) as u64), BufferFlags::HEADER);
                    ctx.out(Self::OUT).push(b);
                }
                self.phase = Phase::Frames;
            }
            let bytes = self.rd.bytes(&u);
            let mut b = match ctx.try_alloc_sized(bytes.len()) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    // the pool is dry, the frame waits
                    self.held = Some(u);
                    return Ok(Flow::Ok);
                }
                Err(_) => match ctx.alloc_exact(bytes.len()) {
                    Some(b) => b,
                    None => return Err(ctx.exact_refused(bytes.len())),
                },
            };
            b.memory.as_mut_full()[..bytes.len()].copy_from_slice(bytes);
            b.memory.set_len(bytes.len());
            b.pts = Timestamp(u.pts_ns.max(0) as u64);
            b.duration = Timestamp(u64::from(u.dur_ns));
            b.flags = BufferFlags::KEYFRAME;
            ctx.out(Self::OUT).push(b);
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        if !matches!(event, Event::FlushStart) || !self.started {
            return Ok(());
        }
        let at = ctx.seek_target().and_then(|t| t.at.nanos()).unwrap_or(0);
        let f = &self.file;
        // A seek probes a few frames at most per bisection step, blocking preads as in `frame`.
        #[allow(clippy::disallowed_methods)]
        let mut read = |off: u64, buf: &mut [u8]| f.read_at(buf, off);
        self.rd
            .seek(&self.head, &mut read, at.min(i64::MAX as u64) as i64)
            .map_err(|e| Self::fail(ctx, BareSrcError::Read(e)))?;
        (self.phase, self.held) = (Phase::Head, None);
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.started = false;
    }
}
