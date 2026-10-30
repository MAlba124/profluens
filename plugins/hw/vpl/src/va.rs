//! The process-wide VA display every session shares, and dma-buf export for the display path.

use std::ffi::{CStr, c_int};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use profluens_video::gpuframe::{
    DRM_FORMAT_MOD_INVALID, DmabufFds, ExportedPlane, MAX_PLANES, Planes,
};

use crate::error::{VaError, VaResult};
use crate::ffi;

fn check(status: ffi::VAStatus, call: &'static str) -> VaResult<()> {
    if status == ffi::VA_STATUS_SUCCESS { Ok(()) } else { Err(VaError { status, call }) }
}

// The render node held open, as the VA-API plugin's Display does.
#[allow(clippy::disallowed_types)]
pub struct Device {
    dpy: ffi::VADisplay,
    _file: std::fs::File,
    pub path: PathBuf,
    pub version: (i32, i32),
}

// SAFETY: libva serializes display state, the runtime drives it from its own threads anyway.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    // Cold, device open. The allocation ioctls need a writable node.
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    pub fn open(path: &Path) -> VaResult<Device> {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| VaError { status: -1, call: "open(render node)" })?;
        // SAFETY: the fd is open for as long as the file, which the device keeps.
        let dpy = unsafe { ffi::vaGetDisplayDRM(file.as_raw_fd()) };
        if dpy.is_null() {
            return Err(VaError { status: -1, call: "vaGetDisplayDRM" });
        }
        let (mut major, mut minor): (c_int, c_int) = (0, 0);
        // SAFETY: a fresh display and valid out pointers.
        let st = unsafe { ffi::vaInitialize(dpy, &mut major, &mut minor) };
        if let Err(e) = check(st, "vaInitialize") {
            // SAFETY: the display that failed to initialize, released once.
            unsafe { ffi::vaTerminate(dpy) };
            return Err(e);
        }
        Ok(Device { dpy, _file: file, path: path.to_path_buf(), version: (major, minor) })
    }

    pub fn raw(&self) -> ffi::VADisplay {
        self.dpy
    }

    // Once at probe.
    #[allow(clippy::disallowed_methods)]
    pub fn vendor(&self) -> String {
        // SAFETY: the display is live, the string is the driver's static storage.
        unsafe {
            let p = ffi::vaQueryVendorString(self.dpy);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

impl Device {
    /// No display, for the fake sessions of the group tests.
    #[cfg(test)]
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    pub fn fake() -> Device {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        Device { dpy: std::ptr::null_mut(), _file: file, path: PathBuf::new(), version: (0, 0) }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.dpy.is_null() {
            return;
        }
        // SAFETY: the one owner, terminated once.
        unsafe { ffi::vaTerminate(self.dpy) };
    }
}

// Every exported object and plane fits the frame's inline lists.
const _: () = assert!(
    ffi::VA_DRM_PRIME_MAX_OBJECTS <= MAX_PLANES && ffi::VA_DRM_PRIME_MAX_PLANES <= MAX_PLANES
);

/// A surface exported as dma-bufs. Owns its fds, they close on drop.
pub struct Exported {
    pub fds: DmabufFds,
    pub drm_format: u32,
    pub drm_modifier: u64,
    pub coded_w: u32,
    pub coded_h: u32,
    pub planes: Planes,
}

impl Exported {
    // `dpy` is kept live by the owning Device.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn export(dpy: ffi::VADisplay, surface: ffi::VASurfaceID) -> VaResult<Exported> {
        // SAFETY: the surface is live on this display.
        check(unsafe { ffi::vaSyncSurface(dpy, surface) }, "vaSyncSurface")?;
        let mut d = ffi::VADRMPRIMESurfaceDescriptor::default();
        // SAFETY: d is a valid out struct of the type PRIME_2 fills.
        let st = unsafe {
            ffi::vaExportSurfaceHandle(
                dpy,
                surface,
                ffi::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                ffi::VA_EXPORT_SURFACE_READ_ONLY | ffi::VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                (&raw mut d).cast(),
            )
        };
        check(st, "vaExportSurfaceHandle")?;
        let num_fds = (d.num_objects as usize).min(ffi::VA_DRM_PRIME_MAX_OBJECTS);
        let mut fds = DmabufFds::default();
        for o in &d.objects[..num_fds] {
            // SAFETY: the export hands each object's fd to the caller. It fits, asserted above.
            unsafe { fds.push(o.fd) };
        }
        let drm_modifier =
            if num_fds > 0 { d.objects[0].drm_format_modifier } else { DRM_FORMAT_MOD_INVALID };
        let mut planes = Planes::default();
        let drm_format = if d.num_layers >= 1 {
            let layer = &d.layers[0];
            let n = (layer.num_planes as usize).min(ffi::VA_DRM_PRIME_MAX_PLANES);
            for i in 0..n {
                planes.push(ExportedPlane {
                    object_index: layer.object_index[i],
                    offset: layer.offset[i],
                    pitch: layer.pitch[i],
                });
            }
            layer.drm_format
        } else {
            d.fourcc
        };
        Ok(Exported { fds, drm_format, drm_modifier, coded_w: d.width, coded_h: d.height, planes })
    }
}

/// The DRM FourCC and modifier this driver exports `fourcc` as, by exporting one small surface.
pub fn probe_export(device: &Device, fourcc: u32) -> VaResult<(u32, u64)> {
    let rt = if fourcc == ffi::VA_FOURCC_P010 {
        ffi::VA_RT_FORMAT_YUV420_10
    } else {
        ffi::VA_RT_FORMAT_YUV420
    };
    let mut attrib = ffi::VASurfaceAttrib {
        type_: ffi::VASurfaceAttribPixelFormat,
        flags: ffi::VA_SURFACE_ATTRIB_SETTABLE,
        value: ffi::VAGenericValue {
            type_: ffi::VAGenericValueTypeInteger,
            value: ffi::VAGenericValueUnion { i: fourcc as i32 },
        },
    };
    let mut id = ffi::VA_INVALID_SURFACE;
    // SAFETY: the display is live, one attribute, one surface out.
    let st = unsafe { ffi::vaCreateSurfaces(device.raw(), rt, 64, 64, &mut id, 1, &mut attrib, 1) };
    check(st, "vaCreateSurfaces")?;
    let exported = Exported::export(device.raw(), id);
    // SAFETY: the surface made above, destroyed once.
    unsafe { ffi::vaDestroySurfaces(device.raw(), &mut id, 1) };
    let e = exported?;
    Ok((e.drm_format, e.drm_modifier))
}
