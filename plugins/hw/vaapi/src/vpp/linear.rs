// Whether this driver takes udmabuf slots as surfaces, checked once a process on a small picture
// imported, scaled and read back.

use std::fmt;
use std::sync::OnceLock;

use pf_memory::MemoryError;
use pf_memory::udmabuf::{self, Dmabuf, sync_write};
use profluens_core::memory::{Allocator, Pool, PoolConfig};

use crate::error::VaapiErrorKind as K;
use crate::gpuframe::{
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_NV12, DRM_FORMAT_P010, DmabufFds, DmabufFormat,
    ExportedPlane, GpuFrame, Planes,
};
use crate::va::{Display, ReadbackImage, VaError, sync_surface};
use crate::vpp::format::{Kind, NV12, P010};
use crate::vpp::proc::{Color, Out, Proc, Src, import, surfaces};

const SIDE: u32 = 64;
const HALF: u32 = SIDE / 2;
/// The check's flat picture, Y and CbCr as 8 bit codes.
const FLAT: [u16; 3] = [0x60, 0x50, 0xB0];
/// What scaling a flat picture may move a sample by.
const SLACK: u16 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinearImportError {
    NoDevice,
    Udmabuf(MemoryError),
    NoSlot,
    Display(VaError),
    Refused { layout: &'static str, kind: K },
    Process { layout: &'static str, error: VaError },
    ShortRead { layout: &'static str, plane: usize },
    Mismatch { layout: &'static str, plane: usize, got: u16, want: u16 },
}

impl fmt::Display for LinearImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinearImportError::NoDevice => write!(f, "no VA-API device"),
            LinearImportError::Udmabuf(e) => write!(f, "no udmabuf, {e}"),
            LinearImportError::NoSlot => write!(f, "no udmabuf slot for the check picture"),
            LinearImportError::Display(e) => write!(f, "the VA display did not open, {e}"),
            LinearImportError::Refused { layout, kind } => {
                write!(f, "the driver refused a linear {layout} dmabuf, {kind}")
            }
            LinearImportError::Process { layout, error } => {
                write!(f, "an imported linear {layout} picture did not scale, {error}")
            }
            LinearImportError::ShortRead { layout, plane } => {
                write!(f, "the check's {layout} picture read back short in plane {plane}")
            }
            LinearImportError::Mismatch { layout, plane, got, want } => write!(
                f,
                "an imported linear {layout} picture read back {got:#x} in plane {plane}, not \
                 {want:#x}"
            ),
        }
    }
}

impl LinearImportError {
    /// A short name for a log field.
    pub fn name(&self) -> &'static str {
        match self {
            LinearImportError::NoDevice => "no_device",
            LinearImportError::Udmabuf(_) => "udmabuf",
            LinearImportError::NoSlot => "no_slot",
            LinearImportError::Display(_) => "display",
            LinearImportError::Refused { .. } => "refused",
            LinearImportError::Process { .. } => "process",
            LinearImportError::ShortRead { .. } => "short_read",
            LinearImportError::Mismatch { .. } => "mismatch",
        }
    }
}

impl std::error::Error for LinearImportError {}

impl From<MemoryError> for LinearImportError {
    fn from(e: MemoryError) -> Self {
        LinearImportError::Udmabuf(e)
    }
}

static CHECKED: OnceLock<Result<Vec<DmabufFormat>, LinearImportError>> = OnceLock::new();

/// The linear layouts vaapipostproc imports from udmabuf slots, NV12 and P010 where each
/// passed the check. Checked at the first call, the result kept for the process.
pub fn linear_imports() -> Result<&'static [DmabufFormat], LinearImportError> {
    CHECKED.get_or_init(check).as_ref().map(Vec::as_slice).map_err(Clone::clone)
}

/// The allocator and alignment of a pool whose slots [`linear_imports`] takes.
pub fn udmabuf_allocator() -> Result<(usize, Allocator), LinearImportError> {
    Ok((udmabuf::align(), udmabuf::device()?.allocator()))
}

// Once a process.
#[allow(clippy::disallowed_methods)]
fn check() -> Result<Vec<DmabufFormat>, LinearImportError> {
    let dev = udmabuf::device()?;
    let caps = crate::probe::probe().ok_or(LinearImportError::NoDevice)?;
    let display = Display::open(&caps.device).map_err(LinearImportError::Display)?;
    let page = udmabuf::align();
    let slot = (SIDE * SIDE * 3).next_multiple_of(page as u32) as usize;
    let config =
        PoolConfig { align: page, allocator: dev.allocator(), ..PoolConfig::heap(slot, 1) };
    let pool = Pool::with_config(config);
    let mut formats = Vec::with_capacity(2);
    for (kind, fourcc) in [(NV12, DRM_FORMAT_NV12), (P010, DRM_FORMAT_P010)] {
        match check_one(&display, &pool, kind, fourcc) {
            Ok(()) => formats.push(DmabufFormat { fourcc, modifiers: vec![DRM_FORMAT_MOD_LINEAR] }),
            // Without NV12 nothing goes, a 10-bit picture alone is no link.
            Err(e) if kind == NV12 => return Err(e),
            Err(_) => {}
        }
    }
    Ok(formats)
}

fn check_one(
    display: &Display,
    pool: &Pool,
    kind: Kind,
    fourcc: u32,
) -> Result<(), LinearImportError> {
    let layout = kind.name;
    let wide = kind.depth > 8;
    let sample = 1 + usize::from(wide);
    let pitch = SIDE as usize * sample;
    let luma = pitch * SIDE as usize;
    let mut m = pool.try_acquire().ok_or(LinearImportError::NoSlot)?;
    let d = Dmabuf::of(&m)?;
    sync_write(d.fd, true)?;
    let code = |c: u16| if wide { c << 8 } else { c };
    let bytes = m.as_mut_full();
    for (i, s) in bytes[..luma * 3 / 2].chunks_exact_mut(sample).enumerate() {
        let c = if i * sample < luma { FLAT[0] } else { FLAT[1 + i % 2] };
        match wide {
            true => s.copy_from_slice(&code(c).to_le_bytes()),
            false => s[0] = c as u8,
        }
    }
    m.set_len(luma * 3 / 2);
    sync_write(d.fd, false)?;
    let mut fds = DmabufFds::default();
    // SAFETY: the dup is this frame's alone.
    unsafe { fds.push(d.dup()?) };
    let mut planes = Planes::default();
    for at in [0, luma] {
        planes.push(ExportedPlane {
            object_index: 0,
            offset: (d.offset + at) as u32,
            pitch: pitch as u32,
        });
    }
    let frame = GpuFrame {
        token: 0,
        fds,
        drm_format: fourcc,
        coded_w: SIDE,
        coded_h: SIDE,
        disp_w: SIDE,
        disp_h: SIDE,
        crop_x: 0,
        crop_y: 0,
        planes,
        epoch: 0,
        lease: None,
    };
    let src = import(display, &frame, kind, DRM_FORMAT_MOD_LINEAR)
        .map_err(|kind| LinearImportError::Refused { layout, kind })?;
    let process = |error| LinearImportError::Process { layout, error };
    let proc = Proc::new(display).map_err(process)?;
    let dst = surfaces(display, kind, HALF, HALF, 1).map_err(process)?;
    let out = Out {
        surface: dst.ids()[0],
        place: (0, 0, HALF, HALF),
        fill: false,
        color: Color::SDR,
        rotation: 0,
    };
    let color = Color::SDR;
    proc.run(Src { surface: src.id, at: (0, 0), size: (SIDE, SIDE), color }, out, None)
        .map_err(process)?;
    sync_surface(display, dst.ids()[0]).map_err(process)?;
    let mut read =
        ReadbackImage::new(display, kind.fourcc, kind.bpp, HALF, HALF).map_err(process)?;
    let view = read.read(dst.ids()[0], HALF, HALF).map_err(process)?;
    let row = HALF as usize * sample;
    for plane in 0..2 {
        let rows = if plane == 0 { HALF } else { HALF / 2 };
        for r in 0..rows {
            let line =
                view.row(plane, r, row).ok_or(LinearImportError::ShortRead { layout, plane })?;
            for (i, s) in line.chunks_exact(sample).enumerate() {
                let got = match wide {
                    true => u16::from_le_bytes([s[0], s[1]]) >> 8,
                    false => u16::from(s[0]),
                };
                let want = if plane == 0 { FLAT[0] } else { FLAT[1 + i % 2] };
                if got.abs_diff(want) > SLACK {
                    return Err(LinearImportError::Mismatch { layout, plane, got, want });
                }
            }
        }
    }
    Ok(())
}
