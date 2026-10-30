//! Typed failures of the VPL elements, carried as [`VplError`] in `Error::ElementFailed`.

use std::fmt;

use profluens_core::ctx::Ctx;
use profluens_core::error::Error;

use crate::ffi;

/// Negative is an error, positive a warning.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MfxError {
    pub status: ffi::mfxStatus,
    pub call: &'static str,
}

impl MfxError {
    pub fn name(&self) -> &'static str {
        match self.status {
            ffi::MFX_ERR_NONE => "MFX_ERR_NONE",
            ffi::MFX_ERR_UNKNOWN => "MFX_ERR_UNKNOWN",
            ffi::MFX_ERR_NULL_PTR => "MFX_ERR_NULL_PTR",
            ffi::MFX_ERR_UNSUPPORTED => "MFX_ERR_UNSUPPORTED",
            ffi::MFX_ERR_MEMORY_ALLOC => "MFX_ERR_MEMORY_ALLOC",
            ffi::MFX_ERR_NOT_ENOUGH_BUFFER => "MFX_ERR_NOT_ENOUGH_BUFFER",
            ffi::MFX_ERR_INVALID_HANDLE => "MFX_ERR_INVALID_HANDLE",
            ffi::MFX_ERR_LOCK_MEMORY => "MFX_ERR_LOCK_MEMORY",
            ffi::MFX_ERR_NOT_INITIALIZED => "MFX_ERR_NOT_INITIALIZED",
            ffi::MFX_ERR_NOT_FOUND => "MFX_ERR_NOT_FOUND",
            ffi::MFX_ERR_MORE_DATA => "MFX_ERR_MORE_DATA",
            ffi::MFX_ERR_MORE_SURFACE => "MFX_ERR_MORE_SURFACE",
            ffi::MFX_ERR_ABORTED => "MFX_ERR_ABORTED",
            ffi::MFX_ERR_DEVICE_LOST => "MFX_ERR_DEVICE_LOST",
            ffi::MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => "MFX_ERR_INCOMPATIBLE_VIDEO_PARAM",
            ffi::MFX_ERR_INVALID_VIDEO_PARAM => "MFX_ERR_INVALID_VIDEO_PARAM",
            ffi::MFX_ERR_UNDEFINED_BEHAVIOR => "MFX_ERR_UNDEFINED_BEHAVIOR",
            ffi::MFX_ERR_DEVICE_FAILED => "MFX_ERR_DEVICE_FAILED",
            ffi::MFX_ERR_MORE_BITSTREAM => "MFX_ERR_MORE_BITSTREAM",
            ffi::MFX_ERR_GPU_HANG => "MFX_ERR_GPU_HANG",
            ffi::MFX_ERR_REALLOC_SURFACE => "MFX_ERR_REALLOC_SURFACE",
            ffi::MFX_ERR_RESOURCE_MAPPED => "MFX_ERR_RESOURCE_MAPPED",
            ffi::MFX_ERR_NOT_IMPLEMENTED => "MFX_ERR_NOT_IMPLEMENTED",
            ffi::MFX_ERR_MORE_EXTBUFFER => "MFX_ERR_MORE_EXTBUFFER",
            ffi::MFX_WRN_IN_EXECUTION => "MFX_WRN_IN_EXECUTION",
            ffi::MFX_WRN_DEVICE_BUSY => "MFX_WRN_DEVICE_BUSY",
            ffi::MFX_WRN_VIDEO_PARAM_CHANGED => "MFX_WRN_VIDEO_PARAM_CHANGED",
            ffi::MFX_WRN_PARTIAL_ACCELERATION => "MFX_WRN_PARTIAL_ACCELERATION",
            ffi::MFX_WRN_INCOMPATIBLE_VIDEO_PARAM => "MFX_WRN_INCOMPATIBLE_VIDEO_PARAM",
            ffi::MFX_WRN_VALUE_NOT_CHANGED => "MFX_WRN_VALUE_NOT_CHANGED",
            ffi::MFX_WRN_OUT_OF_RANGE => "MFX_WRN_OUT_OF_RANGE",
            ffi::MFX_WRN_FILTER_SKIPPED => "MFX_WRN_FILTER_SKIPPED",
            ffi::MFX_ERR_NONE_PARTIAL_OUTPUT => "MFX_ERR_NONE_PARTIAL_OUTPUT",
            ffi::MFX_WRN_ALLOC_TIMEOUT_EXPIRED => "MFX_WRN_ALLOC_TIMEOUT_EXPIRED",
            _ => "mfxStatus",
        }
    }

    pub fn is_transient(&self) -> bool {
        matches!(
            self.status,
            ffi::MFX_WRN_DEVICE_BUSY
                | ffi::MFX_WRN_IN_EXECUTION
                | ffi::MFX_WRN_ALLOC_TIMEOUT_EXPIRED
        )
    }
}

impl fmt::Display for MfxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned {} ({})", self.call, self.name(), self.status)
    }
}

impl fmt::Debug for MfxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for MfxError {}

pub type MfxResult<T> = Result<T, MfxError>;

/// Warnings pass as `Ok`.
pub fn check(status: ffi::mfxStatus, call: &'static str) -> MfxResult<()> {
    if status >= ffi::MFX_ERR_NONE { Ok(()) } else { Err(MfxError { status, call }) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VaError {
    pub status: ffi::VAStatus,
    pub call: &'static str,
}

impl fmt::Display for VaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned VAStatus {:#x}", self.call, self.status)
    }
}

impl fmt::Debug for VaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

pub type VaResult<T> = Result<T, VaError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VplErrorKind {
    /// No render node, or `PF_NO_VPL` set.
    NoDevice,
    NoRuntime(MfxError),
    DisplayOpen(VaError),
    Session(MfxError),
    Header(MfxError),
    Unsupported(&'static str),
    Init(MfxError),
    Reset(MfxError),
    FusedInit(MfxError),
    Decode(MfxError),
    Process(MfxError),
    Encode(MfxError),
    Sync(MfxError),
    Map(MfxError),
    NoSurface(MfxError),
    NativeHandle(MfxError),
    Import(MfxError),
    Layout {
        fourcc: u32,
    },
    FrameSlot {
        layout: &'static str,
        width: u32,
        height: u32,
        need: usize,
        capacity: usize,
    },
    HeaderSlot {
        need: usize,
        capacity: usize,
    },
    PacketSlot {
        need: usize,
        capacity: usize,
    },
    ChannelTaken,
    ReceiverTaken,
    FrameHeader,
    NoFrame {
        token: u64,
    },
    FrameBeforeFormat,
    ShortFrame {
        len: usize,
        need: usize,
    },
    BitrateZero,
    BitrateOnFixedQuantizer,
    Export(VaError),
    ModifierChanged {
        negotiated: u64,
        exported: u64,
    },
    PlaneOverrun {
        plane: &'static str,
        row: u32,
    },
    OddDimensions {
        width: u32,
        height: u32,
    },
    NoToneMap(MfxError),
    /// A call made before the element opened its runtime session.
    NoSession,
    /// The runtime stayed busy, or an operation stayed in execution, past the stall limit.
    Stalled(MfxError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VplError {
    pub element: &'static str,
    pub kind: VplErrorKind,
}

impl fmt::Display for VplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?}", self.element, self.kind)
    }
}

impl std::error::Error for VplError {}

pub fn failed(ctx: &Ctx, element: &'static str, kind: VplErrorKind) -> Error {
    Error::element_failed(ctx.element(), VplError { element, kind })
}

#[cfg(test)]
// Test strings.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn statuses_name_themselves_and_warnings_pass() {
        assert!(check(ffi::MFX_WRN_VIDEO_PARAM_CHANGED, "x").is_ok());
        let e = check(ffi::MFX_ERR_MORE_DATA, "MFXVideoDECODE_DecodeFrameAsync").unwrap_err();
        assert_eq!(e.name(), "MFX_ERR_MORE_DATA");
        assert_eq!(
            e.to_string(),
            "MFXVideoDECODE_DecodeFrameAsync returned MFX_ERR_MORE_DATA (-10)"
        );
        assert!(MfxError { status: ffi::MFX_WRN_DEVICE_BUSY, call: "x" }.is_transient());
        assert!(!MfxError { status: ffi::MFX_ERR_DEVICE_FAILED, call: "x" }.is_transient());
    }
}
