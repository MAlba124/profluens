//! The VA video processor, its driver caps and the dma-buf import of a producer's frames.

use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::ptr;
use std::sync::OnceLock;

use crate::error::VaapiErrorKind as K;
use crate::ffi::{
    self, VAHdrMetaData, VAHdrMetaDataHDR10, VAProcColorProperties,
    VAProcFilterParameterBufferHDRToneMapping, VAProcPipelineCaps, VAProcPipelineParameterBuffer,
    VARectangle,
};
use crate::gpuframe::GpuFrame;
use crate::va::{Display, ReadbackImage, Surfaces, UploadImage, VaError, VaResult};
use crate::vpp::format::{Kind, OUTPUTS};
use crate::vpp::plan::Rect;

fn check(status: ffi::VAStatus, ctx: &'static str) -> VaResult<()> {
    if status == ffi::VA_STATUS_SUCCESS { Ok(()) } else { Err(VaError { status, ctx }) }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caps {
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub max_input: (u32, u32),
    pub min_input: (u32, u32),
    pub max_output: (u32, u32),
    pub min_output: (u32, u32),
    /// A bit for each `VA_ROTATION_*` the processor does.
    pub rotations: u32,
}

impl Caps {
    /// Whether the processor turns a picture `rotation` degrees clockwise, none always.
    pub fn turns(&self, rotation: u16) -> bool {
        rotation == 0 || self.rotations & (1 << (rotation / 90)) != 0
    }

    /// iHD 26.1 leaves 444P, P016 and Y416 out of its lists and converts them.
    pub fn takes(&self, input: Kind, output: Kind) -> bool {
        self.inputs.contains(&input.fourcc) && self.outputs.contains(&output.fourcc)
    }

    pub fn fits(&self, (w, h): (u32, u32), (ow, oh): (u32, u32)) -> bool {
        let within = |(w, h): (u32, u32), (lw, lh): (u32, u32), (hw, hh): (u32, u32)| {
            (hw == 0 || (w <= hw && h <= hh)) && w >= lw && h >= lh
        };
        within((w, h), self.min_input, self.max_input)
            && within((ow, oh), self.min_output, self.max_output)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Color {
    pub standard: ffi::VAProcColorStandardType,
    pub range: u8,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

impl Color {
    pub const fn explicit(primaries: u8, transfer: u8, matrix: u8, range: u8) -> Color {
        Color { standard: ffi::VAProcColorStandardExplicit, range, primaries, transfer, matrix }
    }

    pub const SDR: Color = Color::explicit(1, 1, 1, ffi::VA_SOURCE_RANGE_REDUCED);

    fn properties(self) -> VAProcColorProperties {
        VAProcColorProperties {
            color_range: self.range,
            colour_primaries: self.primaries,
            transfer_characteristics: self.transfer,
            matrix_coefficients: self.matrix,
            ..VAProcColorProperties::default()
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Src {
    pub surface: ffi::VASurfaceID,
    pub at: (u32, u32),
    pub size: (u32, u32),
    pub color: Color,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hdr10 {
    /// x and y of the red, green and blue primaries, 0.00002 steps.
    pub primaries: [[u16; 2]; 3],
    pub white: [u16; 2],
    /// 0.0001 cd/m2.
    pub max_luminance: u32,
    pub min_luminance: u32,
    /// cd/m2.
    pub max_cll: u16,
    pub max_fall: u16,
}

/// The metadata is boxed, the filter buffer points at it.
pub struct ToneMap {
    dpy: ffi::VADisplay,
    buf: ffi::VABufferID,
    _meta: Box<VAHdrMetaDataHDR10>,
}

impl ToneMap {
    pub fn id(&self) -> ffi::VABufferID {
        self.buf
    }
}

impl Drop for ToneMap {
    fn drop(&mut self) {
        // SAFETY: buf is ours, destroyed once.
        unsafe { ffi::vaDestroyBuffer(self.dpy, self.buf) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Out {
    pub surface: ffi::VASurfaceID,
    pub place: Rect,
    pub fill: bool,
    pub color: Color,
    /// `VA_ROTATION_*`, `place` lies in the turned picture.
    pub rotation: u32,
}

const BLACK: u32 = 0xff00_0000;

pub struct Proc {
    dpy: ffi::VADisplay,
    config: ffi::VAConfigID,
    context: ffi::VAContextID,
    pub caps: Caps,
}

const LIST: usize = 64;

impl Proc {
    pub fn new(display: &Display) -> VaResult<Proc> {
        let dpy = display.raw();
        let mut config = ffi::VA_INVALID_ID;
        // SAFETY: display live, no attributes, config a valid out pointer.
        check(
            unsafe {
                ffi::vaCreateConfig(
                    dpy,
                    ffi::VAProfileNone,
                    ffi::VAEntrypointVideoProc,
                    ptr::null_mut(),
                    0,
                    &mut config,
                )
            },
            "vaCreateConfig(vpp)",
        )?;
        let mut context = ffi::VA_INVALID_ID;
        // SAFETY: config live, a processing context takes its targets per picture.
        let st =
            unsafe { ffi::vaCreateContext(dpy, config, 0, 0, 0, ptr::null_mut(), 0, &mut context) };
        if let Err(e) = check(st, "vaCreateContext(vpp)") {
            // SAFETY: config is ours and unused.
            unsafe { ffi::vaDestroyConfig(dpy, config) };
            return Err(e);
        }
        let mut p = Proc { dpy, config, context, caps: Caps::default() };
        p.caps = p.query()?;
        Ok(p)
    }

    // Cold: once per session.
    #[allow(clippy::disallowed_methods)]
    fn query(&self) -> VaResult<Caps> {
        let (mut ins, mut outs) = ([0u32; LIST], [0u32; LIST]);
        // SAFETY: integers and pointers, zero is valid, the lists are set below.
        let mut c: VAProcPipelineCaps = unsafe { std::mem::zeroed() };
        c.input_pixel_format = ins.as_mut_ptr();
        c.num_input_pixel_formats = LIST as u32;
        c.output_pixel_format = outs.as_mut_ptr();
        c.num_output_pixel_formats = LIST as u32;
        // SAFETY: context live, no filters, the lists hold what their counts say.
        check(
            unsafe {
                ffi::vaQueryVideoProcPipelineCaps(
                    self.dpy,
                    self.context,
                    ptr::null_mut(),
                    0,
                    &mut c,
                )
            },
            "vaQueryVideoProcPipelineCaps",
        )?;
        let listed = |l: &[u32], k: u32| {
            l[..(k as usize).min(LIST)].iter().copied().filter(|&f| f != 0).collect::<Vec<u32>>()
        };
        let (mut inputs, mut outputs) =
            (listed(&ins, c.num_input_pixel_formats), listed(&outs, c.num_output_pixel_formats));
        // iHD leaves the caps' lists unfilled, the config's surface formats are both ways then
        if inputs.is_empty() || outputs.is_empty() {
            let surface = self.surface_formats()?;
            if inputs.is_empty() {
                inputs = surface.clone();
            }
            if outputs.is_empty() {
                outputs = surface;
            }
        }
        Ok(Caps {
            inputs,
            outputs,
            max_input: (c.max_input_width, c.max_input_height),
            min_input: (c.min_input_width, c.min_input_height),
            max_output: (c.max_output_width, c.max_output_height),
            min_output: (c.min_output_width, c.min_output_height),
            rotations: c.rotation_flags,
        })
    }

    // Cold: once per session.
    #[allow(clippy::disallowed_methods)]
    fn surface_formats(&self) -> VaResult<Vec<u32>> {
        let mut n = 0u32;
        // SAFETY: config live, a null list asks the count.
        check(
            unsafe {
                ffi::vaQuerySurfaceAttributes(self.dpy, self.config, ptr::null_mut(), &mut n)
            },
            "vaQuerySurfaceAttributes",
        )?;
        let empty = ffi::VASurfaceAttrib {
            type_: 0,
            flags: 0,
            value: ffi::VAGenericValue { type_: 0, value: ffi::VAGenericValueUnion { i: 0 } },
        };
        let mut list = vec![empty; n as usize];
        // SAFETY: list holds n entries, the driver writes at most n and sets how many.
        check(
            unsafe {
                ffi::vaQuerySurfaceAttributes(self.dpy, self.config, list.as_mut_ptr(), &mut n)
            },
            "vaQuerySurfaceAttributes",
        )?;
        list.truncate(n as usize);
        Ok(list
            .iter()
            .filter(|a| {
                a.type_ == ffi::VASurfaceAttribPixelFormat
                    && a.value.type_ == ffi::VAGenericValueTypeInteger
            })
            // SAFETY: an integer value, its tag says so
            .map(|a| unsafe { a.value.value.i } as u32)
            .collect())
    }

    // Cold: once per session.
    #[allow(clippy::disallowed_methods)]
    pub fn tone_maps(&self) -> bool {
        let mut filters = [0 as ffi::VAProcFilterType; ffi::VAProcFilterCount];
        let mut n = filters.len() as u32;
        // SAFETY: context live, the list holds n entries, the driver sets how many it wrote.
        if unsafe {
            ffi::vaQueryVideoProcFilters(self.dpy, self.context, filters.as_mut_ptr(), &mut n)
        } != ffi::VA_STATUS_SUCCESS
        {
            return false;
        }
        if !filters[..(n as usize).min(filters.len())]
            .contains(&ffi::VAProcFilterHighDynamicRangeToneMapping)
        {
            return false;
        }
        let mut caps = [ffi::VAProcFilterCapHighDynamicRange::default(); 4];
        let mut n = caps.len() as u32;
        // SAFETY: context live, caps holds n entries of the type the filter's query writes.
        let st = unsafe {
            ffi::vaQueryVideoProcFilterCaps(
                self.dpy,
                self.context,
                ffi::VAProcFilterHighDynamicRangeToneMapping,
                caps.as_mut_ptr().cast(),
                &mut n,
            )
        };
        st == ffi::VA_STATUS_SUCCESS
            && caps[..(n as usize).min(caps.len())].iter().any(|c| {
                c.metadata_type == ffi::VAProcHighDynamicRangeMetadataHDR10
                    && c.caps_flag & ffi::VA_TONE_MAPPING_HDR_TO_SDR != 0
            })
    }

    // Cold: once per session.
    #[allow(clippy::disallowed_methods)]
    pub fn tone_map(&self, h: &Hdr10) -> VaResult<ToneMap> {
        // VA takes the primaries green, blue, red
        let [r, g, b] = h.primaries;
        let mut meta = Box::new(VAHdrMetaDataHDR10 {
            display_primaries_x: [g[0], b[0], r[0]],
            display_primaries_y: [g[1], b[1], r[1]],
            white_point_x: h.white[0],
            white_point_y: h.white[1],
            max_display_mastering_luminance: h.max_luminance,
            min_display_mastering_luminance: h.min_luminance,
            max_content_light_level: h.max_cll,
            max_pic_average_light_level: h.max_fall,
            reserved: [0; ffi::VA_PADDING_HIGH],
        });
        let mut p = VAProcFilterParameterBufferHDRToneMapping {
            type_: ffi::VAProcFilterHighDynamicRangeToneMapping,
            data: VAHdrMetaData {
                metadata_type: ffi::VAProcHighDynamicRangeMetadataHDR10,
                metadata: (&raw mut *meta).cast(),
                metadata_size: std::mem::size_of::<VAHdrMetaDataHDR10>() as u32,
                reserved: [0; ffi::VA_PADDING_LOW],
            },
        };
        let mut buf = ffi::VA_INVALID_ID;
        let size = std::mem::size_of::<VAProcFilterParameterBufferHDRToneMapping>() as u32;
        // SAFETY: context live, the driver copies p, the metadata box lives as long as the ToneMap.
        check(
            unsafe {
                ffi::vaCreateBuffer(
                    self.dpy,
                    self.context,
                    ffi::VAProcFilterParameterBufferType,
                    size,
                    1,
                    (&raw mut p).cast(),
                    &mut buf,
                )
            },
            "vaCreateBuffer(hdr)",
        )?;
        Ok(ToneMap { dpy: self.dpy, buf, _meta: meta })
    }

    /// Whether the driver paints `output_background_color` over the whole output surface, out
    /// of the output region too. VA has no cap for it, a pass into a grey 64x64 surface of each
    /// output layout tells, once a process. One answer for every display, the elements open
    /// the probed device only.
    pub fn paints_background(&self, display: &Display) -> bool {
        static PAINTS: OnceLock<bool> = OnceLock::new();
        *PAINTS.get_or_init(|| OUTPUTS.iter().all(|&k| self.paints(display, k).unwrap_or(false)))
    }

    fn paints(&self, display: &Display, k: Kind) -> VaResult<bool> {
        const N: u32 = 64;
        let s = surfaces(display, k, N, N, 2)?;
        let (src, dst) = (s.ids()[0], s.ids()[1]);
        let row = N as usize * if k.depth > 8 { 2 } else { 1 };
        let mut img = UploadImage::acquire_for(display, dst, N, N, k.surface())?;
        for (plane, rows) in [(0, N), (1, N / 2)] {
            for r in 0..rows {
                if let Some(line) = img.row_mut(plane, r, row) {
                    line.fill(0xeb);
                }
            }
        }
        img.finish()?;
        let color = Color::SDR;
        let place = (0, N / 4, N, N / 2);
        let out = Out { surface: dst, place, fill: true, color, rotation: 0 };
        self.run(Src { surface: src, at: (0, 0), size: (N, N), color }, out, None)?;
        let mut img = ReadbackImage::new(display, k.fourcc, k.bpp, N, N)?;
        let view = img.read(dst, N, N)?;
        // the first and last rows lie outside the region, P010 holds a sample's 8 bits high
        let black = |plane: usize, r: u32, want: u8| {
            view.row(plane, r, row).is_some_and(|l| match k.depth > 8 {
                true => l.as_chunks::<2>().0.iter().all(|s| s[1].abs_diff(want) <= 2),
                false => l.iter().all(|&s| s.abs_diff(want) <= 2),
            })
        };
        Ok(black(0, 0, 16) && black(0, N - 1, 16) && black(1, 0, 128) && black(1, N / 2 - 1, 128))
    }

    pub fn run(&self, src: Src, out: Out, tone_map: Option<&ToneMap>) -> VaResult<()> {
        let clamp = |v: u32| v.min(u32::from(u16::MAX)) as u16;
        let rect = |(x, y): (u32, u32), (w, h): (u32, u32)| VARectangle {
            x: clamp(x).min(i16::MAX as u16) as i16,
            y: clamp(y).min(i16::MAX as u16) as i16,
            width: clamp(w),
            height: clamp(h),
        };
        let (px, py, pw, ph) = out.place;
        let (sr, dr) = (rect(src.at, src.size), rect((px, py), (pw, ph)));
        let mut filters = [tone_map.map_or(ffi::VA_INVALID_ID, ToneMap::id)];
        let mut p = VAProcPipelineParameterBuffer::zeroed();
        p.surface = src.surface;
        p.surface_region = &sr;
        p.output_region = &dr;
        if out.fill {
            p.output_background_color = BLACK;
        }
        p.surface_color_standard = src.color.standard;
        p.output_color_standard = out.color.standard;
        p.input_color_properties = src.color.properties();
        p.output_color_properties = out.color.properties();
        if tone_map.is_some() {
            p.filters = filters.as_mut_ptr();
            p.num_filters = 1;
        }
        p.filter_flags = ffi::VA_FILTER_SCALING_HQ;
        p.rotation_state = out.rotation;
        let mut buf = ffi::VA_INVALID_ID;
        let size = std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32;
        // SAFETY: context live, p a valid parameter buffer copied by the driver here.
        check(
            unsafe {
                ffi::vaCreateBuffer(
                    self.dpy,
                    self.context,
                    ffi::VAProcPipelineParameterBufferType,
                    size,
                    1,
                    (&raw mut p).cast(),
                    &mut buf,
                )
            },
            "vaCreateBuffer(vpp)",
        )?;
        // SAFETY: context and surfaces live, buf ours, the rectangles and filter list outlive the
        // calls.
        let r = (|| unsafe {
            check(ffi::vaBeginPicture(self.dpy, self.context, out.surface), "vaBeginPicture(vpp)")?;
            check(
                ffi::vaRenderPicture(self.dpy, self.context, &mut buf, 1),
                "vaRenderPicture(vpp)",
            )?;
            check(ffi::vaEndPicture(self.dpy, self.context), "vaEndPicture(vpp)")
        })();
        // SAFETY: buf is ours, destroyed once.
        unsafe { ffi::vaDestroyBuffer(self.dpy, buf) };
        r
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // SAFETY: both ours, destroyed once, context first.
        unsafe {
            ffi::vaDestroyContext(self.dpy, self.context);
            ffi::vaDestroyConfig(self.dpy, self.config);
        }
    }
}

pub struct Imported {
    dpy: ffi::VADisplay,
    pub id: ffi::VASurfaceID,
    pub key: (u64, u64),
}

impl Drop for Imported {
    fn drop(&mut self) {
        // SAFETY: the surface is ours, destroyed once.
        unsafe { ffi::vaDestroySurfaces(self.dpy, &mut self.id, 1) };
    }
}

// The fd is the frame's, borrowed as a File for the fstat and never closed here.
#[allow(clippy::disallowed_types)]
fn stat(frame: &GpuFrame, object: u32) -> Result<((u64, u64), u64), K> {
    let fd = frame.fd(object).ok_or(K::Dmabuf { errno: 9 })?;
    // SAFETY: fd is open for as long as the frame, ManuallyDrop keeps it open after.
    let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd.as_raw_fd()) });
    let m = file.metadata().map_err(|e| K::Dmabuf { errno: e.raw_os_error().unwrap_or(0) })?;
    Ok(((m.dev(), m.ino()), m.size()))
}

pub fn dmabuf_key(frame: &GpuFrame) -> Result<(u64, u64), K> {
    stat(frame, 0).map(|(key, _)| key)
}

pub fn import(
    display: &Display,
    frame: &GpuFrame,
    kind: Kind,
    modifier: u64,
) -> Result<Imported, K> {
    let key = dmabuf_key(frame)?;
    if frame.fds.len() > ffi::VA_DRM_PRIME_MAX_OBJECTS
        || frame.planes.len() > ffi::VA_DRM_PRIME_MAX_PLANES
    {
        return Err(K::DmabufLayout {
            objects: frame.fds.len() as u32,
            planes: frame.planes.len() as u32,
        });
    }
    // SAFETY: integers, zero is valid, filled below.
    let mut d: ffi::VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
    (d.fourcc, d.width, d.height) = (kind.fourcc, frame.coded_w, frame.coded_h);
    d.num_objects = frame.fds.len() as u32;
    for (i, &fd) in frame.fds.iter().enumerate() {
        let size = stat(frame, i as u32).map_or(0, |(_, s)| s);
        d.objects[i] = ffi::VADRMPRIMESurfaceDescriptorObject {
            fd,
            size: size.min(u64::from(u32::MAX)) as u32,
            drm_format_modifier: modifier,
        };
    }
    d.num_layers = 1;
    let layer = &mut d.layers[0];
    (layer.drm_format, layer.num_planes) = (frame.drm_format, frame.planes.len() as u32);
    for (i, p) in frame.planes.iter().enumerate() {
        (layer.object_index[i], layer.offset[i], layer.pitch[i]) =
            (p.object_index, p.offset, p.pitch);
    }
    let int = |i: i32| ffi::VAGenericValue {
        type_: ffi::VAGenericValueTypeInteger,
        value: ffi::VAGenericValueUnion { i },
    };
    let mut attribs = [
        ffi::VASurfaceAttrib {
            type_: ffi::VASurfaceAttribMemoryType,
            flags: ffi::VA_SURFACE_ATTRIB_SETTABLE,
            value: int(ffi::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32),
        },
        ffi::VASurfaceAttrib {
            type_: ffi::VASurfaceAttribExternalBufferDescriptor,
            flags: ffi::VA_SURFACE_ATTRIB_SETTABLE,
            value: ffi::VAGenericValue {
                type_: ffi::VAGenericValueTypePointer,
                value: ffi::VAGenericValueUnion { p: (&raw mut d).cast() },
            },
        },
    ];
    let mut id = ffi::VA_INVALID_SURFACE;
    // SAFETY: display live, the frame holds the fds open, the driver takes its own reference.
    let st = unsafe {
        ffi::vaCreateSurfaces(
            display.raw(),
            kind.rt_format,
            frame.coded_w,
            frame.coded_h,
            &mut id,
            1,
            attribs.as_mut_ptr(),
            attribs.len() as u32,
        )
    };
    check(st, "vaCreateSurfaces(import)").map_err(K::Import)?;
    Ok(Imported { dpy: display.raw(), id, key })
}

pub fn surfaces(
    display: &Display,
    kind: Kind,
    width: u32,
    height: u32,
    count: u32,
) -> VaResult<Surfaces> {
    Surfaces::new(display, kind.surface(), width, height, count)
}
