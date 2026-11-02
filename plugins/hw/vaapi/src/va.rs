//! RAII wrappers over [`crate::ffi`]. Not Send or Sync, a display stays on its element's thread.

use std::ffi::CStr;
use std::fmt;
// A DRM device node, driven by ioctls, not a reactor op.
#[allow(clippy::disallowed_types)]
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr;

use crate::ffi;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VaError {
    pub status: ffi::VAStatus,
    pub ctx: &'static str,
}

impl VaError {
    /// Busy or still in progress, the same call on the next picture may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.status,
            ffi::VA_STATUS_ERROR_SURFACE_BUSY
                | ffi::VA_STATUS_ERROR_SURFACE_IN_DISPLAYING
                | ffi::VA_STATUS_ERROR_HW_BUSY
                | ffi::VA_STATUS_ERROR_TIMEDOUT
        )
    }

    pub fn message(&self) -> String {
        // SAFETY: vaErrorStr returns a static NUL-terminated string for any status.
        unsafe {
            let p = ffi::vaErrorStr(self.status);
            if p.is_null() {
                return format!("VAStatus {:#x}", self.status);
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

impl fmt::Display for VaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {} ({:#x})", self.ctx, self.message(), self.status)
    }
}

impl fmt::Debug for VaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for VaError {}

pub type VaResult<T> = Result<T, VaError>;

pub(crate) type ExportFn = fn(ffi::VADisplay, ffi::VASurfaceID) -> VaResult<ExportedSurface>;

fn check(status: ffi::VAStatus, ctx: &'static str) -> VaResult<()> {
    if status == ffi::VA_STATUS_SUCCESS { Ok(()) } else { Err(VaError { status, ctx }) }
}

// The DRM node the display holds open, see the import.
#[allow(clippy::disallowed_types)]
pub struct Display {
    // Field order is drop order, vaTerminate runs before the fd closes.
    dpy: ffi::VADisplay,
    _file: File,
    version: (i32, i32),
}

impl Display {
    // Opening the device is part of bringing the display up, off the media path.
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    pub fn open(path: &Path) -> VaResult<Display> {
        // Read-write, GEM allocation ioctls need it. A read-only node fails vaCreateContext with
        // ALLOCATION_FAILED.
        let file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| VaError { status: -1, ctx: "open(drm node)" })?;
        // SAFETY: fd is valid for the File's lifetime (kept in the struct).
        let dpy = unsafe { ffi::vaGetDisplayDRM(file.as_raw_fd()) };
        if dpy.is_null() {
            return Err(VaError { status: -1, ctx: "vaGetDisplayDRM" });
        }
        let mut major: std::ffi::c_int = 0;
        let mut minor: std::ffi::c_int = 0;
        // SAFETY: dpy is a fresh non-null display, out pointers are valid locals.
        let st = unsafe { ffi::vaInitialize(dpy, &mut major, &mut minor) };
        if let Err(e) = check(st, "vaInitialize") {
            // SAFETY: dpy is the display that failed init, terminated so the driver releases its
            // partial setup.
            unsafe {
                ffi::vaTerminate(dpy);
            }
            return Err(e);
        }
        Ok(Display { dpy, _file: file, version: (major, minor) })
    }

    pub fn raw(&self) -> ffi::VADisplay {
        self.dpy
    }

    pub fn version(&self) -> (i32, i32) {
        self.version
    }

    // Cold: driver banner, read once at probe/setup, not per-frame.
    #[allow(clippy::disallowed_methods)]
    pub fn vendor(&self) -> String {
        // SAFETY: dpy is live, the string is driver-owned and copied out at once.
        unsafe {
            let p = ffi::vaQueryVendorString(self.dpy);
            if p.is_null() {
                return String::new();
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: single owner, dpy is live until here.
        unsafe {
            ffi::vaTerminate(self.dpy);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SurfaceFormat {
    pub rt_format: u32,
    pub fourcc: u32,
    pub bits_per_pixel: u32,
}

impl SurfaceFormat {
    pub const NV12: SurfaceFormat = SurfaceFormat {
        rt_format: ffi::VA_RT_FORMAT_YUV420,
        fourcc: ffi::VA_FOURCC_NV12,
        bits_per_pixel: 12,
    };
    pub const P010: SurfaceFormat = SurfaceFormat {
        rt_format: ffi::VA_RT_FORMAT_YUV420_10,
        fourcc: ffi::VA_FOURCC_P010,
        bits_per_pixel: 24,
    };
    pub const P012: SurfaceFormat = SurfaceFormat {
        rt_format: ffi::VA_RT_FORMAT_YUV420_12,
        fourcc: ffi::VA_FOURCC_P012,
        bits_per_pixel: 24,
    };
}

pub struct Config {
    dpy: ffi::VADisplay,
    id: ffi::VAConfigID,
}

impl Config {
    pub fn new_decode(display: &Display, profile: ffi::VAProfile) -> VaResult<Config> {
        Self::new_decode_for(display, profile, SurfaceFormat::NV12)
    }

    pub fn new_decode_for(
        display: &Display,
        profile: ffi::VAProfile,
        format: SurfaceFormat,
    ) -> VaResult<Config> {
        let mut attrib =
            ffi::VAConfigAttrib { type_: ffi::VAConfigAttribRTFormat, value: format.rt_format };
        let mut id: ffi::VAConfigID = ffi::VA_INVALID_ID;
        // SAFETY: display live, attrib is a valid single-element array, id out ptr valid.
        let st = unsafe {
            ffi::vaCreateConfig(
                display.raw(),
                profile,
                ffi::VAEntrypointVLD,
                &mut attrib,
                1,
                &mut id,
            )
        };
        check(st, "vaCreateConfig")?;
        Ok(Config { dpy: display.raw(), id })
    }

    /// A decode config whose pictures are also scaled in the call, see [`crate::decproc`].
    pub fn new_decode_processing(
        display: &Display,
        profile: ffi::VAProfile,
        format: SurfaceFormat,
    ) -> VaResult<Config> {
        let mut attribs = [
            ffi::VAConfigAttrib { type_: ffi::VAConfigAttribRTFormat, value: format.rt_format },
            ffi::VAConfigAttrib {
                type_: ffi::VAConfigAttribDecProcessing,
                value: ffi::VA_DEC_PROCESSING,
            },
        ];
        let mut id: ffi::VAConfigID = ffi::VA_INVALID_ID;
        // SAFETY: display live, attribs a valid two-element array, id out ptr valid.
        let st = unsafe {
            ffi::vaCreateConfig(
                display.raw(),
                profile,
                ffi::VAEntrypointVLD,
                attribs.as_mut_ptr(),
                attribs.len() as i32,
                &mut id,
            )
        };
        check(st, "vaCreateConfig(dec processing)")?;
        Ok(Config { dpy: display.raw(), id })
    }

    pub fn new_encode(
        display: &Display,
        profile: ffi::VAProfile,
        entrypoint: ffi::VAEntrypoint,
    ) -> VaResult<Config> {
        let mut attrib = ffi::VAConfigAttrib {
            type_: ffi::VAConfigAttribRTFormat,
            value: ffi::VA_RT_FORMAT_YUV420,
        };
        let mut id: ffi::VAConfigID = ffi::VA_INVALID_ID;
        // SAFETY: display live, attrib is a valid single-element array, id out ptr valid.
        let st = unsafe {
            ffi::vaCreateConfig(display.raw(), profile, entrypoint, &mut attrib, 1, &mut id)
        };
        check(st, "vaCreateConfig(encode)")?;
        Ok(Config { dpy: display.raw(), id })
    }

    /// The driver validates the RC mode at config time. iHD refuses a Main 10 config of 8-bit
    /// surfaces.
    pub fn new_encode_rc(
        display: &Display,
        profile: ffi::VAProfile,
        entrypoint: ffi::VAEntrypoint,
        rt_format: u32,
        rc_mode: u32,
    ) -> VaResult<Config> {
        Self::new_encode_with(display, profile, entrypoint, rt_format, rc_mode, 0)
    }

    pub fn new_encode_with(
        display: &Display,
        profile: ffi::VAProfile,
        entrypoint: ffi::VAEntrypoint,
        rt_format: u32,
        rc_mode: u32,
        packed: u32,
    ) -> VaResult<Config> {
        let mut attribs = [
            ffi::VAConfigAttrib { type_: ffi::VAConfigAttribRTFormat, value: rt_format },
            ffi::VAConfigAttrib { type_: ffi::VAConfigAttribRateControl, value: rc_mode },
            ffi::VAConfigAttrib { type_: ffi::VAConfigAttribEncPackedHeaders, value: packed },
        ];
        let n = if packed == 0 { 2 } else { 3 };
        let mut id: ffi::VAConfigID = ffi::VA_INVALID_ID;
        // SAFETY: display live, attribs holds at least n valid entries, id out ptr valid.
        let st = unsafe {
            ffi::vaCreateConfig(
                display.raw(),
                profile,
                entrypoint,
                attribs.as_mut_ptr(),
                n,
                &mut id,
            )
        };
        check(st, "vaCreateConfig(encode+rc)")?;
        Ok(Config { dpy: display.raw(), id })
    }

    pub fn id(&self) -> ffi::VAConfigID {
        self.id
    }
}

/// Waits for the GPU to be done with `surface` in 1 ms steps (vaSyncSurface2), false when `stop`
/// says so first. A driver without it, or a failure, is left to the sync after.
// `dpy` outlives the call, the element owns it on its single thread.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn wait_surface(dpy: ffi::VADisplay, surface: ffi::VASurfaceID, stop: impl Fn() -> bool) -> bool {
    loop {
        // SAFETY: surface is a live surface on this display.
        let status = unsafe { ffi::vaSyncSurface2(dpy, surface, WAIT_STEP_NS) };
        if status != ffi::VA_STATUS_ERROR_TIMEDOUT {
            return true;
        }
        if stop() {
            return false;
        }
    }
}

/// How long [`wait_surface`] waits before it looks at `stop` again.
const WAIT_STEP_NS: u64 = 1_000_000;

pub fn sync_surface(display: &Display, surface: ffi::VASurfaceID) -> VaResult<()> {
    // SAFETY: the display is live, and the caller's surface is one of its own.
    check(unsafe { ffi::vaSyncSurface(display.raw(), surface) }, "vaSyncSurface")
}

pub fn config_attribute(
    display: &Display,
    profile: ffi::VAProfile,
    entrypoint: ffi::VAEntrypoint,
    attrib_type: ffi::VAConfigAttribType,
) -> VaResult<Option<u32>> {
    let mut attrib = ffi::VAConfigAttrib { type_: attrib_type, value: 0 };
    // SAFETY: display live, attrib is a valid single-element in/out array.
    let st =
        unsafe { ffi::vaGetConfigAttributes(display.raw(), profile, entrypoint, &mut attrib, 1) };
    check(st, "vaGetConfigAttributes")?;
    Ok(if attrib.value == ffi::VA_ATTRIB_NOT_SUPPORTED { None } else { Some(attrib.value) })
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: id valid until destroyed once, dpy outlives this (element drop order).
        unsafe {
            ffi::vaDestroyConfig(self.dpy, self.id);
        }
    }
}

pub const DECODE_SLACK: u32 = 4;

/// Every holder must fit at once. A pool that runs dry makes the exhaustion path emit below the
/// reorder budget and a later smaller-POC B-frame shows after a newer one.
pub fn decode_surface_count(refs: u32, reorder: u32, presentation: u32) -> u32 {
    refs.clamp(1, 16) + reorder.clamp(1, 16) + DECODE_SLACK + presentation
}

pub struct Surfaces {
    dpy: ffi::VADisplay,
    ids: Vec<ffi::VASurfaceID>,
}

impl Surfaces {
    pub fn new_nv12(display: &Display, width: u32, height: u32, count: u32) -> VaResult<Surfaces> {
        Self::new(display, SurfaceFormat::NV12, width, height, count)
    }

    pub fn new(
        display: &Display,
        format: SurfaceFormat,
        width: u32,
        height: u32,
        count: u32,
    ) -> VaResult<Surfaces> {
        let mut attrib = ffi::VASurfaceAttrib {
            type_: ffi::VASurfaceAttribPixelFormat,
            flags: ffi::VA_SURFACE_ATTRIB_SETTABLE,
            value: ffi::VAGenericValue {
                type_: ffi::VAGenericValueTypeInteger,
                value: ffi::VAGenericValueUnion { i: format.fourcc as i32 },
            },
        };
        let mut ids = vec![ffi::VA_INVALID_SURFACE; count as usize];
        // SAFETY: display live, ids has room for `count`, attrib is one valid entry.
        let st = unsafe {
            ffi::vaCreateSurfaces(
                display.raw(),
                format.rt_format,
                width,
                height,
                ids.as_mut_ptr(),
                count,
                &mut attrib,
                1,
            )
        };
        check(st, "vaCreateSurfaces")?;
        Ok(Surfaces { dpy: display.raw(), ids })
    }

    pub fn ids(&self) -> &[ffi::VASurfaceID] {
        &self.ids
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        // SAFETY: ids are live surfaces owned by this set, destroyed exactly once.
        unsafe {
            ffi::vaDestroySurfaces(self.dpy, self.ids.as_mut_ptr(), self.ids.len() as i32);
        }
    }
}

pub const RENDER_AT_ONCE: usize = 16;

pub struct Context {
    dpy: ffi::VADisplay,
    id: ffi::VAContextID,
}

impl Context {
    // Cold: context creation, done once per dimension at setup, not per-frame.
    #[allow(clippy::disallowed_methods)]
    pub fn new(
        display: &Display,
        config: &Config,
        width: i32,
        height: i32,
        surfaces: &Surfaces,
    ) -> VaResult<Context> {
        let mut targets = surfaces.ids().to_vec();
        let mut id: ffi::VAContextID = ffi::VA_INVALID_ID;
        // SAFETY: display/config live, targets is a valid array of that length.
        let st = unsafe {
            ffi::vaCreateContext(
                display.raw(),
                config.id(),
                width,
                height,
                ffi::VA_PROGRESSIVE,
                targets.as_mut_ptr(),
                targets.len() as i32,
                &mut id,
            )
        };
        check(st, "vaCreateContext")?;
        Ok(Context { dpy: display.raw(), id })
    }

    pub fn id(&self) -> ffi::VAContextID {
        self.id
    }

    pub fn begin(&self, target: ffi::VASurfaceID) -> VaResult<()> {
        // SAFETY: context + target are live for this call.
        check(unsafe { ffi::vaBeginPicture(self.dpy, self.id, target) }, "vaBeginPicture")
    }

    pub fn render(&self, buffers: &[ffi::VABufferID]) -> VaResult<()> {
        for chunk in buffers.chunks(RENDER_AT_ONCE) {
            let mut bufs = [ffi::VA_INVALID_ID; RENDER_AT_ONCE];
            bufs[..chunk.len()].copy_from_slice(chunk);
            // SAFETY: context live, bufs holds `chunk.len()` valid ids.
            let st = unsafe {
                ffi::vaRenderPicture(self.dpy, self.id, bufs.as_mut_ptr(), chunk.len() as i32)
            };
            check(st, "vaRenderPicture")?;
        }
        Ok(())
    }

    pub fn end(&self) -> VaResult<()> {
        // SAFETY: context live for this call.
        check(unsafe { ffi::vaEndPicture(self.dpy, self.id) }, "vaEndPicture")
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: id valid, destroyed once, before the Surfaces drop (element field order).
        unsafe {
            ffi::vaDestroyContext(self.dpy, self.id);
        }
    }
}

pub struct Buffer {
    dpy: ffi::VADisplay,
    id: ffi::VABufferID,
}

impl Buffer {
    pub fn new_struct<T: Copy>(
        display: &Display,
        context: &Context,
        type_: ffi::VABufferType,
        value: &T,
    ) -> VaResult<Buffer> {
        let mut id: ffi::VABufferID = ffi::VA_INVALID_ID;
        // SAFETY: value is a live &T of the exact size we pass, the driver copies it.
        let st = unsafe {
            ffi::vaCreateBuffer(
                display.raw(),
                context.id(),
                type_,
                std::mem::size_of::<T>() as u32,
                1,
                value as *const T as *mut std::ffi::c_void,
                &mut id,
            )
        };
        check(st, "vaCreateBuffer(struct)")?;
        Ok(Buffer { dpy: display.raw(), id })
    }

    pub fn new_array<T: Copy>(
        display: &Display,
        context: &Context,
        type_: ffi::VABufferType,
        values: &[T],
    ) -> VaResult<Buffer> {
        let mut id: ffi::VABufferID = ffi::VA_INVALID_ID;
        // SAFETY: values is a live slice of `len` elements of the size we pass, copied.
        let st = unsafe {
            ffi::vaCreateBuffer(
                display.raw(),
                context.id(),
                type_,
                std::mem::size_of::<T>() as u32,
                values.len() as u32,
                values.as_ptr() as *mut std::ffi::c_void,
                &mut id,
            )
        };
        check(st, "vaCreateBuffer(array)")?;
        Ok(Buffer { dpy: display.raw(), id })
    }

    pub fn new_data(display: &Display, context: &Context, data: &[u8]) -> VaResult<Buffer> {
        let mut id: ffi::VABufferID = ffi::VA_INVALID_ID;
        // SAFETY: data is a live slice, the driver copies `data.len()` bytes.
        let st = unsafe {
            ffi::vaCreateBuffer(
                display.raw(),
                context.id(),
                ffi::VASliceDataBufferType,
                data.len() as u32,
                1,
                data.as_ptr() as *mut std::ffi::c_void,
                &mut id,
            )
        };
        check(st, "vaCreateBuffer(data)")?;
        Ok(Buffer { dpy: display.raw(), id })
    }

    pub fn new_typed_bytes(
        display: &Display,
        context: &Context,
        type_: ffi::VABufferType,
        data: &[u8],
    ) -> VaResult<Buffer> {
        let mut id: ffi::VABufferID = ffi::VA_INVALID_ID;
        // SAFETY: data is a live slice, the driver copies `data.len()` bytes.
        let st = unsafe {
            ffi::vaCreateBuffer(
                display.raw(),
                context.id(),
                type_,
                data.len() as u32,
                1,
                data.as_ptr() as *mut std::ffi::c_void,
                &mut id,
            )
        };
        check(st, "vaCreateBuffer(typed bytes)")?;
        Ok(Buffer { dpy: display.raw(), id })
    }

    pub fn new_empty(
        display: &Display,
        context: &Context,
        type_: ffi::VABufferType,
        size: u32,
    ) -> VaResult<Buffer> {
        let mut id: ffi::VABufferID = ffi::VA_INVALID_ID;
        // SAFETY: NULL data + size is the documented "allocate, driver fills" form.
        let st = unsafe {
            ffi::vaCreateBuffer(
                display.raw(),
                context.id(),
                type_,
                size,
                1,
                ptr::null_mut(),
                &mut id,
            )
        };
        check(st, "vaCreateBuffer(empty)")?;
        Ok(Buffer { dpy: display.raw(), id })
    }

    /// The target surface must be synced first, some drivers block in the map otherwise.
    /// Whether the GPU is done with the buffer, without waiting (vaSyncBuffer at a zero
    /// timeout). A driver without it says done, the map then waits.
    pub fn idle(&self) -> VaResult<bool> {
        // SAFETY: self.id is a live buffer on this display.
        match unsafe { ffi::vaSyncBuffer(self.dpy, self.id, 0) } {
            ffi::VA_STATUS_SUCCESS | ffi::VA_STATUS_ERROR_UNIMPLEMENTED => Ok(true),
            ffi::VA_STATUS_ERROR_TIMEDOUT => Ok(false),
            status => Err(VaError { status, ctx: "vaSyncBuffer" }),
        }
    }

    pub fn read_coded(&self, out: &mut Vec<u8>) -> VaResult<()> {
        let mut base: *mut std::ffi::c_void = ptr::null_mut();
        // SAFETY: self.id is a live coded buffer on this display.
        check(unsafe { ffi::vaMapBuffer(self.dpy, self.id, &mut base) }, "vaMapBuffer(coded)")?;
        // Bounded walk, 64 segments means the driver handed us a cycle.
        let mut seg = base as *const ffi::VACodedBufferSegment;
        let mut hops = 0;
        let mut status = 0;
        while !seg.is_null() && hops < 64 {
            // SAFETY: seg and buf point into the mapped coded buffer for the map's lifetime.
            unsafe {
                let s = &*seg;
                status |= s.status;
                if !s.buf.is_null() && s.size > 0 {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        s.buf as *const u8,
                        s.size as usize,
                    ));
                }
                seg = s.next as *const ffi::VACodedBufferSegment;
            }
            hops += 1;
        }
        // SAFETY: mapped above, unmap exactly once.
        check(unsafe { ffi::vaUnmapBuffer(self.dpy, self.id) }, "vaUnmapBuffer(coded)")?;
        // A truncated or broken picture is no picture.
        if status & ffi::VA_CODED_BUF_STATUS_BROKEN != 0 {
            return Err(VaError { status: -1, ctx: "coded buffer status" });
        }
        Ok(())
    }

    pub fn id(&self) -> ffi::VABufferID {
        self.id
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: id is a live buffer, destroyed once, dpy outlives it.
        unsafe {
            ffi::vaDestroyBuffer(self.dpy, self.id);
        }
    }
}

pub struct MappedImage {
    dpy: ffi::VADisplay,
    image: ffi::VAImage,
    base: *mut u8,
}

impl MappedImage {
    pub fn acquire(
        display: &Display,
        surface: ffi::VASurfaceID,
        width: u32,
        height: u32,
    ) -> VaResult<MappedImage> {
        Self::acquire_for(display, surface, width, height, SurfaceFormat::NV12)
    }

    pub fn acquire_for(
        display: &Display,
        surface: ffi::VASurfaceID,
        width: u32,
        height: u32,
        format: SurfaceFormat,
    ) -> VaResult<MappedImage> {
        let dpy = display.raw();
        // SAFETY: surface is a live decode target on this display.
        check(unsafe { ffi::vaSyncSurface(dpy, surface) }, "vaSyncSurface")?;

        let mut image: ffi::VAImage = unsafe { std::mem::zeroed() };
        image.image_id = ffi::VA_INVALID_ID;
        // SAFETY: image is a valid out struct, surface is synced.
        let derive = unsafe { ffi::vaDeriveImage(dpy, surface, &mut image) };
        let derived_as_asked =
            derive == ffi::VA_STATUS_SUCCESS && image.format.fourcc == format.fourcc;

        if !derived_as_asked {
            if derive == ffi::VA_STATUS_SUCCESS {
                // SAFETY: image_id is the live derived image, in another layout, so fall through to
                // GetImage.
                unsafe {
                    ffi::vaDestroyImage(dpy, image.image_id);
                }
            }
            let mut fmt = ffi::VAImageFormat {
                fourcc: format.fourcc,
                byte_order: 1, // VA_LSB_FIRST
                bits_per_pixel: format.bits_per_pixel,
                depth: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                alpha_mask: 0,
                va_reserved: [0; ffi::VA_PADDING_LOW],
            };
            let mut created: ffi::VAImage = unsafe { std::mem::zeroed() };
            // SAFETY: fmt/created are valid out structs, dimensions are the picture's.
            check(
                unsafe {
                    ffi::vaCreateImage(dpy, &mut fmt, width as i32, height as i32, &mut created)
                },
                "vaCreateImage",
            )?;
            // SAFETY: created is a live NV12 image, surface synced, region == picture.
            let st =
                unsafe { ffi::vaGetImage(dpy, surface, 0, 0, width, height, created.image_id) };
            if let Err(e) = check(st, "vaGetImage") {
                // SAFETY: created is live, release before propagating.
                unsafe {
                    ffi::vaDestroyImage(dpy, created.image_id);
                }
                return Err(e);
            }
            image = created;
        }

        let mut base: *mut std::ffi::c_void = ptr::null_mut();
        // SAFETY: image.buf is the image's backing buffer, base out ptr valid.
        let st = unsafe { ffi::vaMapBuffer(dpy, image.buf, &mut base) };
        if let Err(e) = check(st, "vaMapBuffer") {
            // SAFETY: image is live, release before propagating.
            unsafe {
                ffi::vaDestroyImage(dpy, image.image_id);
            }
            return Err(e);
        }
        Ok(MappedImage { dpy, image, base: base as *mut u8 })
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.image.width as u32, self.image.height as u32)
    }

    pub fn num_planes(&self) -> u32 {
        self.image.num_planes
    }

    pub fn fourcc(&self) -> u32 {
        self.image.format.fourcc
    }

    pub fn pitch(&self, p: usize) -> u32 {
        self.image.pitches[p]
    }

    pub fn row(&self, plane: usize, row: u32, len: usize) -> Option<&[u8]> {
        if plane >= self.image.num_planes as usize || plane >= 3 {
            return None;
        }
        let off =
            self.image.offsets[plane] as usize + row as usize * self.image.pitches[plane] as usize;
        if off + len > self.image.data_size as usize {
            return None;
        }
        // SAFETY: [off, off+len) lies in the mapped `data_size` bytes, the map lives as long as
        // &self.
        Some(unsafe { std::slice::from_raw_parts(self.base.add(off), len) })
    }
}

impl Drop for MappedImage {
    fn drop(&mut self) {
        // SAFETY: buf is mapped and image live, unmap then destroy, exactly once.
        unsafe {
            ffi::vaUnmapBuffer(self.dpy, self.image.buf);
            ffi::vaDestroyImage(self.dpy, self.image.image_id);
        }
    }
}

/// On iHD a derived image of a tiled surface is de-tiled by the CPU on map and unmap, about 20 ms
/// a 4K frame on CometLake GT2. vaGetImage has the GPU copy it instead.
pub struct ReadbackImage {
    dpy: ffi::VADisplay,
    image: ffi::VAImage,
}

impl ReadbackImage {
    pub fn new(
        display: &Display,
        fourcc: u32,
        bits_per_pixel: u32,
        width: u32,
        height: u32,
    ) -> VaResult<ReadbackImage> {
        let mut fmt = ffi::VAImageFormat {
            fourcc,
            byte_order: 1, // VA_LSB_FIRST
            bits_per_pixel,
            depth: 0,
            red_mask: 0,
            green_mask: 0,
            blue_mask: 0,
            alpha_mask: 0,
            va_reserved: [0; ffi::VA_PADDING_LOW],
        };
        // SAFETY: plain integers, all zero is valid, the driver fills it.
        let mut image: ffi::VAImage = unsafe { std::mem::zeroed() };
        // SAFETY: display live, fmt and image valid out structs.
        let st = unsafe {
            ffi::vaCreateImage(display.raw(), &mut fmt, width as i32, height as i32, &mut image)
        };
        check(st, "vaCreateImage")?;
        Ok(ReadbackImage { dpy: display.raw(), image })
    }

    pub fn read(
        &mut self,
        surface: ffi::VASurfaceID,
        width: u32,
        height: u32,
    ) -> VaResult<ImageView<'_>> {
        let (w, h) =
            (width.min(u32::from(self.image.width)), height.min(u32::from(self.image.height)));
        // SAFETY: surface is a live decode target on this display, the image is ours.
        check(unsafe { ffi::vaSyncSurface(self.dpy, surface) }, "vaSyncSurface")?;
        // SAFETY: as above, the region lies within both.
        check(
            unsafe { ffi::vaGetImage(self.dpy, surface, 0, 0, w, h, self.image.image_id) },
            "vaGetImage",
        )?;
        let mut base: *mut std::ffi::c_void = ptr::null_mut();
        // SAFETY: image.buf is the image's backing buffer, base a valid out pointer.
        check(unsafe { ffi::vaMapBuffer(self.dpy, self.image.buf, &mut base) }, "vaMapBuffer")?;
        Ok(ImageView { image: &self.image, dpy: self.dpy, base: base as *mut u8 })
    }
}

impl Drop for ReadbackImage {
    fn drop(&mut self) {
        // SAFETY: the image is live and unmapped (a view borrows it), destroyed once.
        unsafe {
            ffi::vaDestroyImage(self.dpy, self.image.image_id);
        }
    }
}

pub struct ImageView<'a> {
    image: &'a ffi::VAImage,
    dpy: ffi::VADisplay,
    base: *mut u8,
}

impl ImageView<'_> {
    pub fn num_planes(&self) -> u32 {
        self.image.num_planes
    }

    pub fn row(&self, plane: usize, row: u32, len: usize) -> Option<&[u8]> {
        if plane >= self.image.num_planes as usize || plane >= 3 {
            return None;
        }
        let off =
            self.image.offsets[plane] as usize + row as usize * self.image.pitches[plane] as usize;
        if off + len > self.image.data_size as usize {
            return None;
        }
        // SAFETY: [off, off+len) lies in the mapped `data_size` bytes for the view's life.
        Some(unsafe { std::slice::from_raw_parts(self.base.add(off), len) })
    }
}

impl Drop for ImageView<'_> {
    fn drop(&mut self) {
        // SAFETY: the buffer is mapped, unmapped once.
        unsafe {
            ffi::vaUnmapBuffer(self.dpy, self.image.buf);
        }
    }
}

pub use profluens_video::gpuframe::ExportedPlane;
use profluens_video::gpuframe::{DmabufFds, MAX_PLANES, Planes};

// Every exported object and plane fits the frame's inline lists.
const _: () = assert!(
    ffi::VA_DRM_PRIME_MAX_OBJECTS <= MAX_PLANES && ffi::VA_DRM_PRIME_MAX_PLANES <= MAX_PLANES
);

/// A surface exported as dma-bufs (PRIME_2, composed layers). Owns its fds and closes them on drop.
/// The fds do not keep the surface from reuse, the decoder's in-flight marking does.
pub struct ExportedSurface {
    pub fds: DmabufFds,
    pub fourcc: u32,
    pub drm_format: u32,
    pub drm_modifier: u64,
    pub coded_w: u32,
    pub coded_h: u32,
    pub planes: Planes,
}

impl ExportedSurface {
    // `dpy` outlives the call, the element owns it on its single thread.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn export(dpy: ffi::VADisplay, surface: ffi::VASurfaceID) -> VaResult<ExportedSurface> {
        // SAFETY: surface is a live decode target on this display.
        check(unsafe { ffi::vaSyncSurface(dpy, surface) }, "vaSyncSurface(export)")?;

        let mut desc: ffi::VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
        // SAFETY: desc is a valid out struct, surface is synced, PRIME_2 fills a
        // VADRMPRIMESurfaceDescriptor.
        let st = unsafe {
            ffi::vaExportSurfaceHandle(
                dpy,
                surface,
                ffi::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                ffi::VA_EXPORT_SURFACE_READ_ONLY | ffi::VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                &mut desc as *mut _ as *mut std::ffi::c_void,
            )
        };
        check(st, "vaExportSurfaceHandle")?;

        let n_obj = (desc.num_objects as usize).min(ffi::VA_DRM_PRIME_MAX_OBJECTS);
        let mut fds = DmabufFds::default();
        for o in desc.objects.iter().take(n_obj) {
            // SAFETY: the export hands each object's fd to the caller. It fits, asserted above.
            unsafe { fds.push(o.fd as std::os::unix::io::RawFd) };
        }
        let drm_modifier = desc.objects.first().map(|o| o.drm_format_modifier).unwrap_or(0);
        let mut planes = Planes::default();
        let drm_format = if desc.num_layers >= 1 {
            let layer = &desc.layers[0];
            let np = (layer.num_planes as usize).min(ffi::VA_DRM_PRIME_MAX_PLANES);
            for i in 0..np {
                planes.push(ExportedPlane {
                    object_index: layer.object_index[i],
                    offset: layer.offset[i],
                    pitch: layer.pitch[i],
                });
            }
            layer.drm_format
        } else {
            ffi::DRM_FORMAT_NV12
        };

        Ok(ExportedSurface {
            fds,
            fourcc: desc.fourcc,
            drm_format,
            drm_modifier,
            coded_w: desc.width,
            coded_h: desc.height,
            planes,
        })
    }

    /// The driver states its modifier only on an export, so this exports one small surface.
    pub fn probe(display: &Display) -> VaResult<(u32, u64)> {
        Self::probe_for(display, SurfaceFormat::NV12)
    }

    pub fn probe_for(display: &Display, format: SurfaceFormat) -> VaResult<(u32, u64)> {
        let surfaces = Surfaces::new(display, format, 64, 64, 1)?;
        let exported = ExportedSurface::export(display.raw(), surfaces.ids()[0])?;
        Ok((exported.drm_format, exported.drm_modifier))
    }
}

/// A CPU-writable view over an encode input surface. Callers must call `finish()`, a plain drop
/// skips the vaPutImage upload of a staged image.
pub struct UploadImage {
    dpy: ffi::VADisplay,
    surface: ffi::VASurfaceID,
    image: ffi::VAImage,
    base: *mut u8,
    derived: bool,
    finished: bool,
}

impl UploadImage {
    pub fn acquire(
        display: &Display,
        surface: ffi::VASurfaceID,
        width: u32,
        height: u32,
    ) -> VaResult<UploadImage> {
        Self::acquire_for(display, surface, width, height, SurfaceFormat::NV12)
    }

    pub fn acquire_for(
        display: &Display,
        surface: ffi::VASurfaceID,
        width: u32,
        height: u32,
        format: SurfaceFormat,
    ) -> VaResult<UploadImage> {
        let dpy = display.raw();
        // SAFETY: surface is live, synced as it may still be an in-flight encode source.
        check(unsafe { ffi::vaSyncSurface(dpy, surface) }, "vaSyncSurface(upload)")?;

        let mut image: ffi::VAImage = unsafe { std::mem::zeroed() };
        image.image_id = ffi::VA_INVALID_ID;
        // SAFETY: image is a valid out struct, surface is synced.
        let derive = unsafe { ffi::vaDeriveImage(dpy, surface, &mut image) };
        let derived = derive == ffi::VA_STATUS_SUCCESS && image.format.fourcc == format.fourcc;

        if !derived {
            if derive == ffi::VA_STATUS_SUCCESS {
                // SAFETY: image_id is the live derived image, in another layout.
                unsafe {
                    ffi::vaDestroyImage(dpy, image.image_id);
                }
            }
            let mut fmt = ffi::VAImageFormat {
                fourcc: format.fourcc,
                byte_order: 1, // VA_LSB_FIRST
                bits_per_pixel: format.bits_per_pixel,
                depth: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                alpha_mask: 0,
                va_reserved: [0; ffi::VA_PADDING_LOW],
            };
            let mut created: ffi::VAImage = unsafe { std::mem::zeroed() };
            // SAFETY: fmt/created are valid out structs, dims are the surface's.
            check(
                unsafe {
                    ffi::vaCreateImage(dpy, &mut fmt, width as i32, height as i32, &mut created)
                },
                "vaCreateImage(upload)",
            )?;
            image = created;
        }

        let mut base: *mut std::ffi::c_void = ptr::null_mut();
        // SAFETY: image.buf is the image's backing buffer, base out ptr valid.
        let st = unsafe { ffi::vaMapBuffer(dpy, image.buf, &mut base) };
        if let Err(e) = check(st, "vaMapBuffer(upload)") {
            // SAFETY: image is live, release before propagating.
            unsafe {
                ffi::vaDestroyImage(dpy, image.image_id);
            }
            return Err(e);
        }
        Ok(UploadImage { dpy, surface, image, base: base as *mut u8, derived, finished: false })
    }

    pub fn pitch(&self, p: usize) -> u32 {
        self.image.pitches[p]
    }

    pub fn num_planes(&self) -> u32 {
        self.image.num_planes
    }

    pub fn row_mut(&mut self, plane: usize, row: u32, len: usize) -> Option<&mut [u8]> {
        if plane >= self.image.num_planes as usize || plane >= 3 {
            return None;
        }
        let off =
            self.image.offsets[plane] as usize + row as usize * self.image.pitches[plane] as usize;
        if off + len > self.image.data_size as usize {
            return None;
        }
        // SAFETY: [off, off+len) is in the mapped range, &mut self guards aliasing.
        Some(unsafe { std::slice::from_raw_parts_mut(self.base.add(off), len) })
    }

    pub fn finish(mut self) -> VaResult<()> {
        self.finished = true;
        // SAFETY: buf is mapped, unmapped once here, Drop skips finished views.
        let unmapped =
            check(unsafe { ffi::vaUnmapBuffer(self.dpy, self.image.buf) }, "vaUnmapBuffer(upload)");
        let put = if self.derived || unmapped.is_err() {
            Ok(())
        } else {
            let (w, h) = (self.image.width as u32, self.image.height as u32);
            // SAFETY: image holds staged pixels, surface is live, regions are the full extent.
            check(
                unsafe {
                    ffi::vaPutImage(
                        self.dpy,
                        self.surface,
                        self.image.image_id,
                        0,
                        0,
                        w,
                        h,
                        0,
                        0,
                        w,
                        h,
                    )
                },
                "vaPutImage",
            )
        };
        // SAFETY: image is live, destroyed exactly once whatever the unmap said (Drop skips).
        unsafe {
            ffi::vaDestroyImage(self.dpy, self.image.image_id);
        }
        unmapped.and(put)
    }
}

impl Drop for UploadImage {
    fn drop(&mut self) {
        if self.finished {
            return; // finish() already unmapped + destroyed
        }
        // SAFETY: buf is mapped and image live, error path, unmap then destroy once.
        unsafe {
            ffi::vaUnmapBuffer(self.dpy, self.image.buf);
            ffi::vaDestroyImage(self.dpy, self.image.image_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_surface_count_holds_every_holder_at_once() {
        assert_eq!(decode_surface_count(4, 2, 0), 10);
        assert_eq!(decode_surface_count(4, 2, 6), 16);
        assert_eq!(decode_surface_count(1, 0, 0), 1 + 1 + DECODE_SLACK);
        assert_eq!(decode_surface_count(0, 0, 0), decode_surface_count(1, 1, 0));
        assert_eq!(decode_surface_count(40, 40, 0), 32 + DECODE_SLACK);
        for refs in 1..=16u32 {
            for reorder in 1..=16u32 {
                assert!(decode_surface_count(refs, reorder, 0) >= refs + reorder + 2);
            }
        }
    }
}
