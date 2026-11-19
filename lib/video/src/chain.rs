//! A transcode chain as data, backend neutral: what decodes, what scales and tone maps, what
//! encodes, and the pools and scheduling its elements get. A backend builds the elements from it.

use crate::color::StreamColor;
use crate::gpuframe::{DmabufFormat, GpuFrameChannel};
use profluens_core::element::Element;
use profluens_core::id::ElementId;
use profluens_core::memory::Allocator;
use std::sync::Arc;

/// A picture box, the largest a picture is fitted into, and its bits a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub depth: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChainSpec {
    pub from: &'static str,
    pub decode: Decode,
    /// The scaler between decoder and encoder, none where the decoder makes the picture.
    pub scale: Option<Scale>,
    pub encode: Encode,
    /// Slot size and count of each element's pool.
    pub dec_pool: (usize, u32),
    pub vpp_pool: (usize, u32),
    pub enc_pool: (usize, u32),
    /// The scaler and the encoder run inline on the decoder's thread.
    pub one_group: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decode {
    /// The device's decoder, scaling into `shrink` in its decode pass where given. An AV1
    /// decoder that synthesizes film grain keeps it with `film_grain`.
    Device { shrink: Option<Shrink>, film_grain: bool },
    /// A software decoder, `threads` frame threads where it takes them.
    Software { handover: Handover, threads: Option<u32> },
}

/// A decode pass's scale, the box and whether the picture fills it with bars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shrink {
    pub to: Picture,
    pub fixed: bool,
}

/// How a software decoder's pictures reach the scaler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handover {
    /// Copied up by the scaler.
    Upload,
    /// Written into udmabuf slots the scaler imports, `align` the slots' page, rows at
    /// `row_align` where the scaler reads them in place, else the decoder's own pitch.
    Udmabuf { align: usize, row_align: Option<usize> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub to: Picture,
    /// Every picture is the box, a smaller one fitted in with bars.
    pub fixed_size: bool,
    /// Degrees it turns the picture and the pixel aspect it squares, where it is told them.
    pub turn: Option<(u16, (u32, u32))>,
    /// The tone map runs once at start, off the first picture's path.
    pub warm_tone_map: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Encode {
    pub to: &'static str,
    pub fps: (u32, u32),
    pub gop: u32,
    pub rate: Rate,
    pub colour: StreamColor,
    /// Dolby Vision RPUs carried from a source of this size, turned this many quarters.
    pub dovi: Option<((u32, u32), u8)>,
    /// Keyframes on every frame within the tolerance, ns, of one of the stamps.
    pub keys: Option<(Arc<[i64]>, i64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// A constant quantizer, no rate control.
    Constant { qp: u8 },
    /// Rate control at `kbps` within a decoder buffer of `hrd_ms`, frames past `max_fps`
    /// dropped ((0, 1) none), the session opened ahead at `ahead`, `gpu_depth` pictures queued
    /// on the GPU, the low power entrypoint where the backend has one.
    Target {
        kbps: u32,
        hrd_ms: u32,
        max_fps: (u32, u32),
        ahead: (u32, u32),
        gpu_depth: usize,
        low_power: bool,
    },
}

/// The elements a backend built from a spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainIds {
    pub dec: ElementId,
    pub vpp: Option<ElementId>,
    pub enc: ElementId,
}

/// The udmabuf layouts a software decoder writes and the row alignment where it is told one,
/// None for pictures to be uploaded.
pub type Dmabufs = Option<(&'static [DmabufFormat], Option<usize>)>;

/// The software decoder of a family writing into the channel as [`Dmabufs`] has it. None
/// without one.
pub type SoftDecoder<'a> = &'a dyn Fn(&Arc<GpuFrameChannel>, Dmabufs) -> Option<Box<dyn Element>>;

/// The page and allocator of a pool of udmabuf slots, as [`Handover::Udmabuf`] states them.
// Cold, once a chain.
#[allow(clippy::disallowed_methods)]
pub fn udmabuf_allocator() -> Result<(usize, Allocator), ChainError> {
    let device = pf_memory::udmabuf::device().map_err(|e| ChainError::Udmabuf(Box::new(e)))?;
    Ok((pf_memory::udmabuf::align(), device.allocator()))
}

/// Why a backend built no chain.
#[derive(Debug)]
pub enum ChainError {
    /// No decoder of the family, the device's or a software one.
    NoDecoder(&'static str),
    /// The udmabuf device or the scaler's import of it refused.
    Udmabuf(Box<dyn std::error::Error + Send + Sync>),
    Pipeline(profluens_core::error::Error),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::NoDecoder(family) => write!(f, "no decoder for {family}"),
            ChainError::Udmabuf(e) => write!(f, "udmabuf {e}"),
            ChainError::Pipeline(e) => write!(f, "pipeline {e}"),
        }
    }
}

impl std::error::Error for ChainError {}
