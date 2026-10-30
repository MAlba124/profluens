//! Hand-written FFI to the dav1d AV1 decoder, built from the `dav1d/` submodule and linked
//! static. Every struct is checked against the C headers by build.rs layout asserts.

#![allow(non_camel_case_types)]

use std::mem::{offset_of, size_of};
use std::os::raw::{c_char, c_int, c_uint, c_void};

pub mod layout {
    include!(concat!(env!("OUT_DIR"), "/layout.rs"));
}

pub const VERSION: &str = env!("LIBDAV1D_SYS_VERSION");

#[repr(C)]
pub struct Dav1dContext {
    _private: [u8; 0],
}

#[repr(C)]
pub struct Dav1dRef {
    _private: [u8; 0],
}

#[repr(C)]
pub struct Dav1dFrameHeader {
    _private: [u8; 0],
}

#[repr(C)]
pub struct Dav1dSequenceHeader {
    _private: [u8; 0],
}

/// Leading fields of `Dav1dSequenceHeader` up to `color_range`, read through a cast pointer.
#[repr(C)]
pub struct Dav1dSequenceHeaderHead {
    pub profile: u8,
    pub max_width: c_int,
    pub max_height: c_int,
    pub layout: c_uint,
    pub pri: c_uint,
    pub trc: c_uint,
    pub mtrx: c_uint,
    pub chr: c_uint,
    /// 0, 1, 2 for 8, 10, 12 bits per component.
    pub hbd: u8,
    pub color_range: u8,
}

pub const DAV1D_PIXEL_LAYOUT_I400: c_uint = 0;
pub const DAV1D_PIXEL_LAYOUT_I420: c_uint = 1;
pub const DAV1D_PIXEL_LAYOUT_I422: c_uint = 2;
pub const DAV1D_PIXEL_LAYOUT_I444: c_uint = 3;

pub const DAV1D_INLOOPFILTER_ALL: c_uint = 7;
pub const DAV1D_DECODEFRAMETYPE_ALL: c_uint = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dav1dUserData {
    pub data: *const u8,
    pub r#ref: *mut Dav1dRef,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dav1dDataProps {
    /// `i64::MIN` when unknown.
    pub timestamp: i64,
    pub duration: i64,
    pub offset: i64,
    pub size: usize,
    pub user_data: Dav1dUserData,
}

#[repr(C)]
pub struct Dav1dData {
    pub data: *const u8,
    pub sz: usize,
    pub r#ref: *mut Dav1dRef,
    pub m: Dav1dDataProps,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dav1dPictureParameters {
    pub w: c_int,
    pub h: c_int,
    pub layout: c_uint,
    pub bpc: c_int,
}

/// 8 bpc planes are bytes, 10 and 12 bpc are `u16` words.
#[repr(C)]
pub struct Dav1dPicture {
    pub seq_hdr: *mut Dav1dSequenceHeader,
    pub frame_hdr: *mut Dav1dFrameHeader,
    pub data: [*mut c_void; 3],
    pub stride: [isize; 2],
    pub p: Dav1dPictureParameters,
    pub m: Dav1dDataProps,
    pub content_light: *mut c_void,
    pub mastering_display: *mut c_void,
    pub itut_t35: *mut c_void,
    pub n_itut_t35: usize,
    pub reserved: [usize; 4],
    pub frame_hdr_ref: *mut Dav1dRef,
    pub seq_hdr_ref: *mut Dav1dRef,
    pub content_light_ref: *mut Dav1dRef,
    pub mastering_display_ref: *mut Dav1dRef,
    pub itut_t35_ref: *mut Dav1dRef,
    pub reserved_ref: [usize; 4],
    pub r#ref: *mut Dav1dRef,
    pub allocator_data: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dav1dPicAllocator {
    pub cookie: *mut c_void,
    pub alloc_picture_callback:
        Option<unsafe extern "C" fn(*mut Dav1dPicture, *mut c_void) -> c_int>,
    pub release_picture_callback: Option<unsafe extern "C" fn(*mut Dav1dPicture, *mut c_void)>,
}

/// A null `callback` turns logging off.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dav1dLogger {
    pub cookie: *mut c_void,
    /// Only ever null, so the variadic `va_list` signature need not be spelled.
    pub callback: Option<unsafe extern "C" fn()>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dav1dSettings {
    pub n_threads: c_int,
    pub max_frame_delay: c_int,
    pub apply_grain: c_int,
    pub operating_point: c_int,
    pub all_layers: c_int,
    pub frame_size_limit: c_uint,
    pub allocator: Dav1dPicAllocator,
    pub logger: Dav1dLogger,
    pub strict_std_compliance: c_int,
    pub output_invisible_frames: c_int,
    pub inloop_filters: c_uint,
    pub decode_frame_type: c_uint,
    pub reserved: [u8; 16],
}

const _: () = {
    use layout::*;
    assert!(size_of::<Dav1dDataProps>() == SIZEOF_DAV1D_DATA_PROPS);
    assert!(offset_of!(Dav1dDataProps, timestamp) == OFFSET_DAV1D_DATA_PROPS_TIMESTAMP);
    assert!(offset_of!(Dav1dDataProps, duration) == OFFSET_DAV1D_DATA_PROPS_DURATION);
    assert!(offset_of!(Dav1dDataProps, offset) == OFFSET_DAV1D_DATA_PROPS_OFFSET);
    assert!(offset_of!(Dav1dDataProps, size) == OFFSET_DAV1D_DATA_PROPS_SIZE);
    assert!(offset_of!(Dav1dDataProps, user_data) == OFFSET_DAV1D_DATA_PROPS_USER_DATA);
    assert!(size_of::<Dav1dData>() == SIZEOF_DAV1D_DATA);
    assert!(offset_of!(Dav1dData, data) == OFFSET_DAV1D_DATA_DATA);
    assert!(offset_of!(Dav1dData, sz) == OFFSET_DAV1D_DATA_SZ);
    assert!(offset_of!(Dav1dData, r#ref) == OFFSET_DAV1D_DATA_REF);
    assert!(offset_of!(Dav1dData, m) == OFFSET_DAV1D_DATA_M);
    assert!(size_of::<Dav1dPictureParameters>() == SIZEOF_DAV1D_PICTURE_PARAMETERS);
    assert!(offset_of!(Dav1dPictureParameters, w) == OFFSET_DAV1D_PICTURE_PARAMETERS_W);
    assert!(offset_of!(Dav1dPictureParameters, h) == OFFSET_DAV1D_PICTURE_PARAMETERS_H);
    assert!(offset_of!(Dav1dPictureParameters, layout) == OFFSET_DAV1D_PICTURE_PARAMETERS_LAYOUT);
    assert!(offset_of!(Dav1dPictureParameters, bpc) == OFFSET_DAV1D_PICTURE_PARAMETERS_BPC);
    assert!(size_of::<Dav1dPicture>() == SIZEOF_DAV1D_PICTURE);
    assert!(offset_of!(Dav1dPicture, seq_hdr) == OFFSET_DAV1D_PICTURE_SEQ_HDR);
    assert!(offset_of!(Dav1dPicture, frame_hdr) == OFFSET_DAV1D_PICTURE_FRAME_HDR);
    assert!(offset_of!(Dav1dPicture, data) == OFFSET_DAV1D_PICTURE_DATA);
    assert!(offset_of!(Dav1dPicture, stride) == OFFSET_DAV1D_PICTURE_STRIDE);
    assert!(offset_of!(Dav1dPicture, p) == OFFSET_DAV1D_PICTURE_P);
    assert!(offset_of!(Dav1dPicture, m) == OFFSET_DAV1D_PICTURE_M);
    assert!(offset_of!(Dav1dPicture, content_light) == OFFSET_DAV1D_PICTURE_CONTENT_LIGHT);
    assert!(offset_of!(Dav1dPicture, mastering_display) == OFFSET_DAV1D_PICTURE_MASTERING_DISPLAY);
    assert!(offset_of!(Dav1dPicture, itut_t35) == OFFSET_DAV1D_PICTURE_ITUT_T35);
    assert!(offset_of!(Dav1dPicture, n_itut_t35) == OFFSET_DAV1D_PICTURE_N_ITUT_T35);
    assert!(offset_of!(Dav1dPicture, reserved) == OFFSET_DAV1D_PICTURE_RESERVED);
    assert!(offset_of!(Dav1dPicture, frame_hdr_ref) == OFFSET_DAV1D_PICTURE_FRAME_HDR_REF);
    assert!(offset_of!(Dav1dPicture, seq_hdr_ref) == OFFSET_DAV1D_PICTURE_SEQ_HDR_REF);
    assert!(offset_of!(Dav1dPicture, content_light_ref) == OFFSET_DAV1D_PICTURE_CONTENT_LIGHT_REF);
    assert!(
        offset_of!(Dav1dPicture, mastering_display_ref)
            == OFFSET_DAV1D_PICTURE_MASTERING_DISPLAY_REF
    );
    assert!(offset_of!(Dav1dPicture, itut_t35_ref) == OFFSET_DAV1D_PICTURE_ITUT_T35_REF);
    assert!(offset_of!(Dav1dPicture, reserved_ref) == OFFSET_DAV1D_PICTURE_RESERVED_REF);
    assert!(offset_of!(Dav1dPicture, r#ref) == OFFSET_DAV1D_PICTURE_REF);
    assert!(offset_of!(Dav1dPicture, allocator_data) == OFFSET_DAV1D_PICTURE_ALLOCATOR_DATA);
    assert!(size_of::<Dav1dPicAllocator>() == SIZEOF_DAV1D_PIC_ALLOCATOR);
    assert!(offset_of!(Dav1dPicAllocator, cookie) == OFFSET_DAV1D_PIC_ALLOCATOR_COOKIE);
    assert!(
        offset_of!(Dav1dPicAllocator, alloc_picture_callback)
            == OFFSET_DAV1D_PIC_ALLOCATOR_ALLOC_PICTURE_CALLBACK
    );
    assert!(
        offset_of!(Dav1dPicAllocator, release_picture_callback)
            == OFFSET_DAV1D_PIC_ALLOCATOR_RELEASE_PICTURE_CALLBACK
    );
    assert!(size_of::<Dav1dLogger>() == SIZEOF_DAV1D_LOGGER);
    assert!(offset_of!(Dav1dLogger, cookie) == OFFSET_DAV1D_LOGGER_COOKIE);
    assert!(offset_of!(Dav1dLogger, callback) == OFFSET_DAV1D_LOGGER_CALLBACK);
    assert!(size_of::<Dav1dSettings>() == SIZEOF_DAV1D_SETTINGS);
    assert!(offset_of!(Dav1dSettings, n_threads) == OFFSET_DAV1D_SETTINGS_N_THREADS);
    assert!(offset_of!(Dav1dSettings, max_frame_delay) == OFFSET_DAV1D_SETTINGS_MAX_FRAME_DELAY);
    assert!(offset_of!(Dav1dSettings, apply_grain) == OFFSET_DAV1D_SETTINGS_APPLY_GRAIN);
    assert!(offset_of!(Dav1dSettings, operating_point) == OFFSET_DAV1D_SETTINGS_OPERATING_POINT);
    assert!(offset_of!(Dav1dSettings, all_layers) == OFFSET_DAV1D_SETTINGS_ALL_LAYERS);
    assert!(offset_of!(Dav1dSettings, frame_size_limit) == OFFSET_DAV1D_SETTINGS_FRAME_SIZE_LIMIT);
    assert!(offset_of!(Dav1dSettings, allocator) == OFFSET_DAV1D_SETTINGS_ALLOCATOR);
    assert!(offset_of!(Dav1dSettings, logger) == OFFSET_DAV1D_SETTINGS_LOGGER);
    assert!(
        offset_of!(Dav1dSettings, strict_std_compliance)
            == OFFSET_DAV1D_SETTINGS_STRICT_STD_COMPLIANCE
    );
    assert!(
        offset_of!(Dav1dSettings, output_invisible_frames)
            == OFFSET_DAV1D_SETTINGS_OUTPUT_INVISIBLE_FRAMES
    );
    assert!(offset_of!(Dav1dSettings, inloop_filters) == OFFSET_DAV1D_SETTINGS_INLOOP_FILTERS);
    assert!(
        offset_of!(Dav1dSettings, decode_frame_type) == OFFSET_DAV1D_SETTINGS_DECODE_FRAME_TYPE
    );
    assert!(offset_of!(Dav1dSettings, reserved) == OFFSET_DAV1D_SETTINGS_RESERVED);
    assert!(offset_of!(Dav1dSequenceHeaderHead, profile) == OFFSET_DAV1D_SEQUENCE_HEADER_PROFILE);
    assert!(
        offset_of!(Dav1dSequenceHeaderHead, max_width) == OFFSET_DAV1D_SEQUENCE_HEADER_MAX_WIDTH
    );
    assert!(
        offset_of!(Dav1dSequenceHeaderHead, max_height) == OFFSET_DAV1D_SEQUENCE_HEADER_MAX_HEIGHT
    );
    assert!(offset_of!(Dav1dSequenceHeaderHead, layout) == OFFSET_DAV1D_SEQUENCE_HEADER_LAYOUT);
    assert!(offset_of!(Dav1dSequenceHeaderHead, pri) == OFFSET_DAV1D_SEQUENCE_HEADER_PRI);
    assert!(offset_of!(Dav1dSequenceHeaderHead, trc) == OFFSET_DAV1D_SEQUENCE_HEADER_TRC);
    assert!(offset_of!(Dav1dSequenceHeaderHead, mtrx) == OFFSET_DAV1D_SEQUENCE_HEADER_MTRX);
    assert!(offset_of!(Dav1dSequenceHeaderHead, chr) == OFFSET_DAV1D_SEQUENCE_HEADER_CHR);
    assert!(offset_of!(Dav1dSequenceHeaderHead, hbd) == OFFSET_DAV1D_SEQUENCE_HEADER_HBD);
    assert!(
        offset_of!(Dav1dSequenceHeaderHead, color_range)
            == OFFSET_DAV1D_SEQUENCE_HEADER_COLOR_RANGE
    );
};

pub const fn dav1d_err(errno: i32) -> c_int {
    -errno
}

unsafe extern "C" {
    pub fn dav1d_version() -> *const c_char;
    pub fn dav1d_default_settings(s: *mut Dav1dSettings);
    pub fn dav1d_open(c_out: *mut *mut Dav1dContext, s: *const Dav1dSettings) -> c_int;
    pub fn dav1d_get_frame_delay(s: *const Dav1dSettings) -> c_int;
    pub fn dav1d_close(c_out: *mut *mut Dav1dContext);
    pub fn dav1d_flush(c: *mut Dav1dContext);
    /// 0 takes the reference. `DAV1D_ERR(EAGAIN)` means pictures must be taken first.
    pub fn dav1d_send_data(c: *mut Dav1dContext, r#in: *mut Dav1dData) -> c_int;
    /// A call with no new data after `dav1d_send_data` starts a drain.
    pub fn dav1d_get_picture(c: *mut Dav1dContext, out: *mut Dav1dPicture) -> c_int;
    pub fn dav1d_picture_unref(p: *mut Dav1dPicture);
    pub fn dav1d_data_create(data: *mut Dav1dData, sz: usize) -> *mut u8;
    /// `free_callback` may run on a frame thread, so it must be thread-safe.
    pub fn dav1d_data_wrap(
        data: *mut Dav1dData,
        buf: *const u8,
        sz: usize,
        free_callback: Option<unsafe extern "C" fn(*const u8, *mut c_void)>,
        cookie: *mut c_void,
    ) -> c_int;
    pub fn dav1d_data_unref(data: *mut Dav1dData);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn version_string_links() {
        // SAFETY: a static NUL-terminated string.
        let v = unsafe { std::ffi::CStr::from_ptr(dav1d_version()) };
        let v = v.to_str().unwrap();
        let tagged = v.strip_prefix(VERSION).is_some_and(|r| r.is_empty() || r.starts_with('-'));
        let commit = v.len() >= 7 && v.bytes().all(|b| b.is_ascii_hexdigit());
        assert!(tagged || commit, "{v} is neither {VERSION} nor a commit");
    }

    #[test]
    fn open_feed_garbage_and_close() {
        unsafe {
            let mut s: Dav1dSettings = std::mem::zeroed();
            dav1d_default_settings(&mut s);
            assert_eq!(s.apply_grain, 1);
            assert_eq!(s.all_layers, 1);
            assert!(s.logger.callback.is_some(), "the default logger writes to stderr");
            s.n_threads = 1;
            s.logger.callback = None;
            let mut c: *mut Dav1dContext = ptr::null_mut();
            assert_eq!(dav1d_open(&mut c, &s), 0);
            assert!(!c.is_null());

            let mut data: Dav1dData = std::mem::zeroed();
            let p = dav1d_data_create(&mut data, 16);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0xDE, 16);
            let sent = dav1d_send_data(c, &mut data);
            if sent < 0 && sent != dav1d_err(layout::EAGAIN) {
                dav1d_data_unref(&mut data);
            }
            let mut pic: Dav1dPicture = std::mem::zeroed();
            let got = dav1d_get_picture(c, &mut pic);
            assert!(got < 0, "garbage decoded to a picture");
            dav1d_flush(c);
            dav1d_close(&mut c);
            assert!(c.is_null());
        }
    }
}
