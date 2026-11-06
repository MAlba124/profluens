//! Typed failures of the VA-API elements, carried in `Error::ElementFailed`.

use std::fmt;

use profluens_core::ctx::Ctx;
use profluens_core::error::Error;
use profluens_core::log;
use profluens_core::log::Level;

use crate::va::VaError;

/// A failure of one VA-API element. `element` is its registered name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaapiError {
    pub element: &'static str,
    pub kind: VaapiErrorKind,
}

/// Fatal variants say so, the rest are bus warnings and the unit is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaapiErrorKind {
    NoDevice,
    DisplayOpen(VaError),
    SessionSetup(VaError),
    PpsIdUnreadable,
    UnknownPps,
    UnknownSps,
    /// `extension` is an HEVC range or screen-content extension.
    UnsupportedSps {
        chroma_format_idc: u32,
        bit_depth_luma: u32,
        bit_depth_chroma: u32,
        separate_colour_plane: bool,
        extension: bool,
    },
    SliceHeader,
    SliceSegment,
    /// More slice NALs than the picture has macroblocks or CTBs, or than its level allows.
    TooManySlices {
        slices: usize,
        most: u32,
    },
    /// A tile grid with more columns or rows than the picture has CTBs.
    PpsTiles {
        columns: u32,
        rows: u32,
    },
    Interlaced,
    NoFreeSurface,
    Submit(VaError),
    Readback(VaError),
    NotNv12 {
        planes: u32,
    },
    NoMain10Profile,
    Vp9Header,
    UnsupportedVp9 {
        profile: u8,
        bit_depth: u8,
        subsampling_x: bool,
        subsampling_y: bool,
    },
    NoVp9Profile2,
    EmptyRefSlot {
        slot: u8,
    },
    /// Only a key frame can rebuild the surfaces.
    Vp9Resize {
        width: u32,
        height: u32,
    },
    Av1Stream(crate::av1dec::Av1Error),
    UnsupportedAv1 {
        profile: u8,
        bit_depth: u8,
        mono_chrome: bool,
        subsampling_x: bool,
        subsampling_y: bool,
    },
    Av1EmptyRefSlot {
        slot: u8,
    },
    Av1Resize {
        width: u32,
        height: u32,
    },
    PlaneOverrun {
        plane: &'static str,
        row: u32,
    },
    Export(VaError),
    ExportProbe(VaError),
    ModifierChanged {
        negotiated: u64,
        exported: u64,
    },
    DupFd {
        errno: i32,
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
    ChannelTaken,
    OddDimensions {
        width: u32,
        height: u32,
    },
    Encode(VaError),
    PacketSlot {
        len: usize,
        capacity: usize,
    },
    FrameBeforeFormat,
    ShortFrame {
        len: usize,
        need: usize,
    },
    BitrateOnFixedQuantizer,
    /// It would switch a running session to CQP.
    BitrateZero,
    NoEncodeEntrypoint {
        profile: &'static str,
    },
    DriverLacks(&'static str),
    NoConversion {
        input: &'static str,
        output: &'static str,
    },
    ProcSize {
        width: u32,
        height: u32,
    },
    Process(VaError),
    Upload(VaError),
    Dmabuf {
        errno: i32,
    },
    DmabufLayout {
        objects: u32,
        planes: u32,
    },
    Import(VaError),
    SurfaceExport(VaError),
    ReceiverTaken,
    ChannelFull,
    GpuHeader,
    NoGpuFrame {
        token: u64,
    },
    NoGpuLink,
    GpuLayout {
        layout: &'static str,
    },
    CodedSize {
        coded: (u32, u32),
        session: (u32, u32),
    },
}

/// Eight in a row, a third of a second at 24 fps, is no longer a busy moment.
pub(crate) const TRANSIENT_LIMIT: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Drop,
    Fail,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Transient(u8);

impl Transient {
    /// A refused picture is dropped like a busy one. Hostile input makes the driver refuse
    /// pictures, and only a run of them says the device is gone.
    pub(crate) fn judge(&mut self, _e: &VaError) -> Verdict {
        self.0 = self.0.saturating_add(1);
        if self.0 >= TRANSIENT_LIMIT { Verdict::Fail } else { Verdict::Drop }
    }

    pub(crate) fn ok(&mut self) {
        self.0 = 0;
    }
}

pub(crate) fn failed(ctx: &Ctx, element: &'static str, kind: VaapiErrorKind) -> Error {
    Error::element_failed(ctx.element(), VaapiError { element, kind })
}

impl VaapiErrorKind {
    /// A stable snake_case name for log records.
    pub fn name(&self) -> &'static str {
        use VaapiErrorKind as K;
        match self {
            K::NoDevice => "no_device",
            K::DisplayOpen(_) => "display_open",
            K::SessionSetup(_) => "session_setup",
            K::PpsIdUnreadable => "pps_id_unreadable",
            K::UnknownPps => "unknown_pps",
            K::UnknownSps => "unknown_sps",
            K::UnsupportedSps { .. } => "unsupported_sps",
            K::SliceHeader => "slice_header",
            K::SliceSegment => "slice_segment",
            K::TooManySlices { .. } => "too_many_slices",
            K::PpsTiles { .. } => "pps_tiles",
            K::Interlaced => "interlaced",
            K::NoFreeSurface => "no_free_surface",
            K::Submit(_) => "submit",
            K::Readback(_) => "readback",
            K::NotNv12 { .. } => "not_nv12",
            K::NoMain10Profile => "no_main10_profile",
            K::Vp9Header => "vp9_header",
            K::UnsupportedVp9 { .. } => "unsupported_vp9",
            K::NoVp9Profile2 => "no_vp9_profile2",
            K::EmptyRefSlot { .. } => "empty_ref_slot",
            K::Vp9Resize { .. } => "vp9_resize",
            K::Av1Stream(_) => "av1_stream",
            K::UnsupportedAv1 { .. } => "unsupported_av1",
            K::Av1EmptyRefSlot { .. } => "av1_empty_ref_slot",
            K::Av1Resize { .. } => "av1_resize",
            K::PlaneOverrun { .. } => "plane_overrun",
            K::Export(_) => "export",
            K::ExportProbe(_) => "export_probe",
            K::ModifierChanged { .. } => "modifier_changed",
            K::DupFd { .. } => "dup_fd",
            K::FrameSlot { .. } => "frame_slot",
            K::HeaderSlot { .. } => "header_slot",
            K::ChannelTaken => "channel_taken",
            K::OddDimensions { .. } => "odd_dimensions",
            K::Encode(_) => "encode",
            K::PacketSlot { .. } => "packet_slot",
            K::FrameBeforeFormat => "frame_before_format",
            K::ShortFrame { .. } => "short_frame",
            K::BitrateOnFixedQuantizer => "bitrate_on_fixed_quantizer",
            K::BitrateZero => "bitrate_zero",
            K::NoEncodeEntrypoint { .. } => "no_encode_entrypoint",
            K::DriverLacks(_) => "driver_lacks",
            K::NoConversion { .. } => "no_conversion",
            K::ProcSize { .. } => "proc_size",
            K::Process(_) => "process",
            K::Upload(_) => "upload",
            K::Dmabuf { .. } => "dmabuf",
            K::DmabufLayout { .. } => "dmabuf_layout",
            K::Import(_) => "import",
            K::SurfaceExport(_) => "surface_export",
            K::ReceiverTaken => "receiver_taken",
            K::ChannelFull => "channel_full",
            K::GpuHeader => "gpu_header",
            K::NoGpuFrame { .. } => "no_gpu_frame",
            K::NoGpuLink => "no_gpu_link",
            K::GpuLayout { .. } => "gpu_layout",
            K::CodedSize { .. } => "coded_size",
        }
    }

    /// The VA status a variant carries, 0 (`VA_STATUS_SUCCESS`) for none.
    pub fn va_status(&self) -> i32 {
        use VaapiErrorKind as K;
        match self {
            K::DisplayOpen(e)
            | K::SessionSetup(e)
            | K::Submit(e)
            | K::Readback(e)
            | K::Export(e)
            | K::ExportProbe(e)
            | K::Encode(e)
            | K::Process(e)
            | K::Upload(e)
            | K::Import(e)
            | K::SurfaceExport(e) => e.status,
            _ => 0,
        }
    }
}

/// Bus warnings posted before the count backs off to the powers of two.
const POSTED_EVERY_ONE: u64 = 4;

/// Warnings since the last summary. The first logs at Warn, the rest at Debug.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Warned(u64);

impl Warned {
    /// True when the warning should reach the bus: the first few, then the powers of two,
    /// so a stream that fails every AU does not post a message per AU.
    pub(crate) fn note(&mut self, ctx: &Ctx, kind: VaapiErrorKind) -> bool {
        self.note_name(ctx, kind.name(), kind.va_status());
        self.0 <= POSTED_EVERY_ONE || self.0.is_power_of_two()
    }

    /// A warning with no bus message of its own.
    pub(crate) fn note_name(&mut self, ctx: &Ctx, kind: &'static str, status: i32) {
        let level = if self.0 == 0 { Level::Warn } else { Level::Debug };
        self.0 += 1;
        log!(ctx, level, "warning", kind = kind, status = status);
    }

    /// Logs the count at a flush or the end, then starts over.
    pub(crate) fn summary(&mut self, ctx: &Ctx, at: &'static str) {
        let n = std::mem::take(&mut self.0);
        if n > 0 {
            log!(ctx, Level::Warn, "warnings", count = n, at = at);
        }
    }
}

/// A fatal failure posted to the bus, logged once beside it.
pub(crate) fn log_fatal(ctx: &Ctx, kind: VaapiErrorKind) {
    log!(ctx, Level::Error, "failed", kind = kind.name(), status = kind.va_status());
}

impl fmt::Display for VaapiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.element, self.kind)
    }
}

impl fmt::Display for VaapiErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use VaapiErrorKind as K;
        match self {
            K::NoDevice => f.write_str("no VA-API device available"),
            K::DisplayOpen(e) => write!(f, "cannot open VA display, {e}"),
            K::SessionSetup(e) => write!(f, "VA session setup failed, {e}"),
            K::PpsIdUnreadable => f.write_str("could not read slice pps_id, AU dropped"),
            K::UnknownPps => f.write_str("slice references unknown PPS, dropped"),
            K::UnknownSps => f.write_str("PPS references unknown SPS, dropped"),
            K::UnsupportedSps {
                chroma_format_idc,
                bit_depth_luma,
                bit_depth_chroma,
                separate_colour_plane,
                extension,
            } => {
                write!(
                    f,
                    "unsupported SPS (chroma_format_idc {chroma_format_idc}, \
                     {bit_depth_luma}-bit luma, {bit_depth_chroma}-bit chroma"
                )?;
                if *separate_colour_plane {
                    f.write_str(", separate colour planes")?;
                }
                if *extension {
                    f.write_str(", range or screen-content extension")?;
                }
                f.write_str("), only 8-bit and 10-bit 4:2:0 are wired, dropped")
            }
            K::SliceHeader => f.write_str("malformed slice header, AU dropped"),
            K::SliceSegment => f.write_str("malformed slice segment header, AU dropped"),
            K::TooManySlices { slices, most } => {
                write!(f, "{slices} slices in one AU, at most {most} fit the picture, dropped")
            }
            K::PpsTiles { columns, rows } => {
                write!(f, "a {columns}x{rows} tile grid past the picture's CTBs, dropped")
            }
            K::Interlaced => f.write_str("field/interlaced coding not supported, dropped"),
            K::NoFreeSurface => f.write_str("no free surface, AU dropped"),
            K::Submit(e) => write!(f, "VA submit failed, {e}, dropped"),
            K::Readback(e) => write!(f, "readback failed, {e}, frame dropped"),
            K::NotNv12 { planes } => {
                write!(f, "mapped image has {planes} plane(s), not 2-plane NV12 or P010, dropped")
            }
            K::NoMain10Profile => f.write_str(
                "10-bit stream, but the driver has no HEVC Main 10 decode entry point, dropped",
            ),
            K::Vp9Header => {
                f.write_str("malformed VP9 frame header, dropped up to the next key frame")
            }
            K::UnsupportedVp9 { profile, bit_depth, subsampling_x, subsampling_y } => {
                let chroma = match (subsampling_x, subsampling_y) {
                    (true, true) => "4:2:0",
                    (true, false) => "4:2:2",
                    (false, true) => "4:4:0",
                    (false, false) => "4:4:4",
                };
                write!(
                    f,
                    "unsupported VP9 stream (profile {profile}, {bit_depth}-bit {chroma}), only \
                     8-bit and 10-bit 4:2:0 are wired, dropped"
                )
            }
            K::NoVp9Profile2 => f.write_str(
                "10-bit VP9 stream, but the driver has no VP9 Profile 2 decode entry point, dropped",
            ),
            K::EmptyRefSlot { slot } => {
                write!(f, "VP9 frame names empty reference slot {slot}, dropped")
            }
            K::Vp9Resize { width, height } => write!(
                f,
                "VP9 inter frame of {width}x{height} outgrows the surfaces, dropped up to the \
                 next key frame"
            ),
            K::Av1Stream(e) => {
                write!(f, "malformed AV1 temporal unit, {e}, dropped up to the next key frame")
            }
            K::UnsupportedAv1 { profile, bit_depth, mono_chrome, subsampling_x, subsampling_y } => {
                let chroma = match (mono_chrome, subsampling_x, subsampling_y) {
                    (true, _, _) => "4:0:0",
                    (false, true, true) => "4:2:0",
                    (false, true, false) => "4:2:2",
                    (false, false, _) => "4:4:4",
                };
                write!(
                    f,
                    "unsupported AV1 stream (profile {profile}, {bit_depth}-bit {chroma}), only \
                     8-bit and 10-bit 4:2:0 are wired, dropped"
                )
            }
            K::Av1EmptyRefSlot { slot } => {
                write!(f, "AV1 frame names empty reference slot {slot}, dropped")
            }
            K::Av1Resize { width, height } => write!(
                f,
                "AV1 frame of {width}x{height} outgrows the surfaces, dropped up to the next \
                 key frame"
            ),
            K::PlaneOverrun { plane, row } => {
                write!(f, "{plane} plane geometry overrun at row {row}, dropped")
            }
            K::Export(e) => write!(
                f,
                "DMA-BUF export failed ({e}), falling back to CPU readback for the rest of \
                 the run"
            ),
            K::ExportProbe(e) => {
                write!(f, "DMA-BUF export probe failed ({e}), the pad offers no video/gpu")
            }
            K::ModifierChanged { negotiated, exported } => write!(
                f,
                "surface exported with DRM modifier {exported:#018x}, the link fixed \
                 {negotiated:#018x}"
            ),
            K::DupFd { errno } => write!(
                f,
                "dup(dma-buf fd) failed, {}, frame dropped",
                std::io::Error::from_raw_os_error(*errno)
            ),
            K::FrameSlot { layout, width, height, need, capacity } => write!(
                f,
                "{width}x{height} {layout} frame needs {need} bytes but pool slots hold \
                 {capacity}, raise the pipeline pool slot size"
            ),
            K::HeaderSlot { need, capacity } => write!(
                f,
                "video/gpu header needs {need} bytes but pool slots hold {capacity}"
            ),
            K::ChannelTaken => f.write_str("the frame channel's sending end is another decoder's"),
            K::OddDimensions { width, height } => write!(
                f,
                "odd dimensions {width}x{height} unsupported (4:2:0 crop and conformance \
                 window units are 2 samples)"
            ),
            K::Encode(e) => write!(f, "encode failed, {e}, frame dropped"),
            K::PacketSlot { len, capacity } => write!(
                f,
                "encoded frame of {len} bytes exceeds pool slot capacity {capacity}, raise \
                 the pipeline pool slot size"
            ),
            K::FrameBeforeFormat => f.write_str("raw frame before any format, dropped"),
            K::ShortFrame { len, need } => {
                write!(f, "short frame ({len} < {need} bytes), dropped")
            }
            K::BitrateOnFixedQuantizer => f.write_str(
                "live bitrate change ignored, session is constant-quantizer (start with \
                 bitrate > 0 for a rate-controlled session)",
            ),
            K::BitrateZero => f.write_str("live bitrate 0 ignored (cannot switch to CQP)"),
            K::NoEncodeEntrypoint { profile } => {
                write!(f, "the driver encodes no {profile}, frames dropped")
            }
            K::DriverLacks(what) => write!(f, "the driver does not take {what}"),
            K::NoConversion { input, output } => {
                write!(f, "the driver does not convert {input} to {output}")
            }
            K::ProcSize { width, height } => {
                write!(f, "the video processor does not take {width}x{height}")
            }
            K::Process(e) => write!(f, "video processing failed, {e}"),
            K::Upload(e) => write!(f, "upload into a surface failed, {e}"),
            K::Dmabuf { errno } => write!(
                f,
                "a frame's dma-buf could not be read, {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            K::DmabufLayout { objects, planes } => write!(
                f,
                "a dma-buf of {objects} objects and {planes} planes, more than a VA import takes"
            ),
            K::Import(e) => write!(f, "dma-buf import failed, {e}"),
            K::SurfaceExport(e) => write!(f, "exporting an output surface failed, {e}"),
            K::ReceiverTaken => {
                f.write_str("the frame channel's receiving end is another element's")
            }
            K::ChannelFull => f.write_str("no room on the frame channel"),
            K::GpuHeader => f.write_str("a video/gpu buffer without a frame header, dropped"),
            K::NoGpuFrame { token } => write!(f, "no frame for token {token}, dropped"),
            K::NoGpuLink => f.write_str("no dma-buf layout fixed on the link, dropped"),
            K::GpuLayout { layout } => {
                write!(f, "a {layout} frame this encoder does not code, dropped")
            }
            K::CodedSize { coded, session } => write!(
                f,
                "frame coded {}x{} under the session's {}x{}, dropped",
                coded.0, coded.1, session.0, session.1
            ),
        }
    }
}

impl std::error::Error for VaapiError {}
impl std::error::Error for VaapiErrorKind {}

#[cfg(test)]
mod tests {
    use super::*;
    use VaapiErrorKind as K;

    const VA: VaError = VaError { status: 0x12, ctx: "vaCreateContext" };

    #[test]
    fn a_busy_or_unfinished_device_is_transient_and_a_refusal_is_not() {
        let of = |status| VaError { status, ctx: "vaSyncSurface" }.is_transient();
        for status in [0x10, 0x15, 0x22, 0x26] {
            assert!(of(status), "{status:#x} is the device not done yet");
        }
        // OPERATION_FAILED, INVALID_SURFACE, MAX_NUM_EXCEEDED, INVALID_PARAMETER,
        // DECODING_ERROR, UNKNOWN.
        for status in [0x01, 0x06, 0x0b, 0x12, 0x17, -1] {
            assert!(!of(status), "{status:#x} is the device refusing the picture");
        }
    }

    #[test]
    fn transient_failures_fail_the_decoder_only_many_in_a_row() {
        let busy = VaError { status: 0x22, ctx: "vaSyncSurface" };
        let refused = VaError { status: 0x17, ctx: "vaSyncSurface" };
        let mut t = Transient::default();
        assert_eq!(t.judge(&refused), Verdict::Drop, "a refused picture is dropped");
        t.ok();
        for _ in 0..3 {
            for n in 1..TRANSIENT_LIMIT {
                assert_eq!(t.judge(&busy), Verdict::Drop, "busy {n} in a row");
            }
            t.ok();
        }
        for _ in 1..TRANSIENT_LIMIT {
            t.judge(&busy);
        }
        assert_eq!(t.judge(&busy), Verdict::Fail, "{TRANSIENT_LIMIT} in a row");
    }

    #[test]
    fn the_element_names_its_own_failures() {
        let e = VaapiError { element: "vaapih264dec", kind: K::NoFreeSurface };
        assert_eq!(format!("{e}"), "vaapih264dec: no free surface, AU dropped");
    }

    #[test]
    fn messages_carry_the_variant_data() {
        let rows: [(K, &[&str]); 29] = [
            (
                K::Av1Stream(crate::av1dec::Av1Error::MissingRef(3)),
                &["AV1 temporal unit", "slot 3", "next key frame"],
            ),
            (
                K::UnsupportedAv1 {
                    profile: 1,
                    bit_depth: 8,
                    mono_chrome: false,
                    subsampling_x: false,
                    subsampling_y: false,
                },
                &["profile 1", "8-bit 4:4:4"],
            ),
            (K::Av1EmptyRefSlot { slot: 6 }, &["AV1 frame", "slot 6"]),
            (K::Av1Resize { width: 4096, height: 2176 }, &["4096x2176"]),
            (K::NoEncodeEntrypoint { profile: "AV1Profile0" }, &["encodes no AV1Profile0"]),
            (K::NoConversion { input: "Y416", output: "P010" }, &["convert Y416 to P010"]),
            (K::ProcSize { width: 16384, height: 2 }, &["16384x2"]),
            (K::Process(VA), &["video processing failed", "0x12"]),
            // EBADF.
            (K::Dmabuf { errno: 9 }, &["dma-buf", "os error 9"]),
            (K::DmabufLayout { objects: 5, planes: 2 }, &["5 objects", "2 planes"]),
            (K::NoGpuFrame { token: 77 }, &["token 77"]),
            (K::CodedSize { coded: (256, 192), session: (320, 240) }, &["256x192", "320x240"]),
            (
                K::UnsupportedVp9 {
                    profile: 1,
                    bit_depth: 8,
                    subsampling_x: false,
                    subsampling_y: false,
                },
                &["profile 1", "8-bit 4:4:4", "only 8-bit and 10-bit 4:2:0"],
            ),
            (K::NoVp9Profile2, &["10-bit VP9", "Profile 2"]),
            (K::EmptyRefSlot { slot: 5 }, &["slot 5"]),
            (K::Vp9Resize { width: 1920, height: 1088 }, &["1920x1088", "next key frame"]),
            (K::NoMain10Profile, &["10-bit stream", "no HEVC Main 10 decode"]),
            (K::SessionSetup(VA), &["session setup failed", "vaCreateContext", "0x12"]),
            (K::ExportProbe(VA), &["export probe failed", "0x12", "offers no video/gpu"]),
            (
                K::ModifierChanged { negotiated: 0x0100_0000_0000_0002, exported: 9 },
                &["0x0000000000000009", "the link fixed 0x0100000000000002"],
            ),
            (
                K::UnsupportedSps {
                    chroma_format_idc: 2,
                    bit_depth_luma: 10,
                    bit_depth_chroma: 10,
                    separate_colour_plane: false,
                    extension: true,
                },
                &["chroma_format_idc 2", "10-bit luma", "extension", "only 8-bit and 10-bit 4:2:0"],
            ),
            (K::NotNv12 { planes: 1 }, &["1 plane(s)"]),
            (K::PlaneOverrun { plane: "CbCr", row: 7 }, &["CbCr plane", "row 7"]),
            // EMFILE.
            (K::DupFd { errno: 24 }, &["dup(dma-buf fd)", "os error 24"]),
            (
                K::FrameSlot { layout: "NV12", width: 64, height: 48, need: 4608, capacity: 4096 },
                &["64x48 NV12", "4608", "4096"],
            ),
            (K::HeaderSlot { need: 40, capacity: 16 }, &["40 bytes", "hold 16"]),
            (K::OddDimensions { width: 641, height: 480 }, &["641x480"]),
            (K::PacketSlot { len: 9000, capacity: 4096 }, &["9000 bytes", "capacity 4096"]),
            (K::ShortFrame { len: 10, need: 20 }, &["10 < 20 bytes"]),
        ];
        for (kind, needles) in rows {
            let s = format!("{}", VaapiError { element: "vaapitest", kind });
            assert!(s.starts_with("vaapitest: "), "{s}");
            assert!(!s.contains('\n') && !s.contains('\u{2014}'), "{s}");
            for n in needles {
                assert!(s.contains(n), "{s:?} should mention {n:?}");
            }
        }
    }
}
