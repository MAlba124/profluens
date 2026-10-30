//! Tight per-plane frame geometry. 4:2:0 chroma rounds up for odd luma dimensions, as FFmpeg and
//! GStreamer do.

use crate::format::PixelFormat;

#[inline]
const fn half_up(n: u32) -> u32 {
    n.div_ceil(2)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlaneGeometry {
    pub stride: usize,
    pub height: usize,
    pub size: usize,
}

pub const fn plane_count(pixfmt: PixelFormat) -> usize {
    pixfmt.plane_count()
}

/// The geometry of plane `plane` of a `width`x`height` frame in `pixfmt`, `None` past the last
/// plane.
pub fn plane_geometry(
    pixfmt: PixelFormat,
    width: u32,
    height: u32,
    plane: usize,
) -> Option<PlaneGeometry> {
    let w = width as usize;
    let h = height as usize;
    let cw = half_up(width) as usize; // chroma width (4:2:0)
    let ch = half_up(height) as usize; // chroma height (4:2:0)

    let g = |stride: usize, height: usize| PlaneGeometry { stride, height, size: stride * height };

    Some(match (pixfmt, plane) {
        (PixelFormat::I420, 0) => g(w, h),
        (PixelFormat::I420, 1) | (PixelFormat::I420, 2) => g(cw, ch),
        (PixelFormat::Nv12, 0) => g(w, h),
        (PixelFormat::Nv12, 1) => g(2 * cw, ch),
        (PixelFormat::P010, 0) => g(2 * w, h),
        (PixelFormat::P010, 1) => g(4 * cw, ch),
        (PixelFormat::I422, 0) => g(w, h),
        (PixelFormat::I422, 1) | (PixelFormat::I422, 2) => g(cw, h),
        (PixelFormat::I422P10, 0) => g(2 * w, h),
        (PixelFormat::I422P10, 1) | (PixelFormat::I422P10, 2) => g(2 * cw, h),
        (PixelFormat::Rgb24, 0) => g(3 * w, h),
        (PixelFormat::Gray8, 0) => g(w, h),
        _ => return None,
    })
}

pub fn plane_geometries(pixfmt: PixelFormat, width: u32, height: u32) -> Vec<PlaneGeometry> {
    (0..plane_count(pixfmt))
        .map(|p| plane_geometry(pixfmt, width, height, p).expect("plane in range"))
        .collect()
}

/// The byte offset of plane `plane` in a tightly packed frame.
pub fn plane_offset(pixfmt: PixelFormat, width: u32, height: u32, plane: usize) -> Option<usize> {
    if plane >= plane_count(pixfmt) {
        return None;
    }
    let mut off = 0;
    for p in 0..plane {
        off += plane_geometry(pixfmt, width, height, p)?.size;
    }
    Some(off)
}

pub fn frame_size(pixfmt: PixelFormat, width: u32, height: u32) -> usize {
    (0..plane_count(pixfmt))
        .map(|p| plane_geometry(pixfmt, width, height, p).expect("plane in range").size)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        pixfmt: PixelFormat,
        w: u32,
        h: u32,
        planes: &'static [(usize, usize)], // (stride, height) per plane
    }

    const CASES: &[Case] = &[
        Case { pixfmt: PixelFormat::I420, w: 4, h: 4, planes: &[(4, 4), (2, 2), (2, 2)] },
        Case {
            pixfmt: PixelFormat::Nv12,
            w: 4,
            h: 4,
            planes: &[(4, 4), (4, 2)], // chroma stride = 2*ceil(4/2) = 4
        },
        Case {
            pixfmt: PixelFormat::P010,
            w: 4,
            h: 4,
            planes: &[(8, 4), (8, 2)], // 16-bit words, Y 2*4, chroma 4*ceil(4/2)
        },
        Case {
            pixfmt: PixelFormat::I422,
            w: 4,
            h: 4,
            planes: &[(4, 4), (2, 4), (2, 4)], // chroma half width, full height
        },
        Case {
            pixfmt: PixelFormat::I422P10,
            w: 4,
            h: 4,
            planes: &[(8, 4), (4, 4), (4, 4)], // I422 in 16-bit words
        },
        Case { pixfmt: PixelFormat::Rgb24, w: 4, h: 4, planes: &[(12, 4)] },
        Case { pixfmt: PixelFormat::Gray8, w: 4, h: 4, planes: &[(4, 4)] },
        // Odd dimensions, chroma rounds up.
        Case { pixfmt: PixelFormat::I420, w: 3, h: 3, planes: &[(3, 3), (2, 2), (2, 2)] },
        Case { pixfmt: PixelFormat::I420, w: 5, h: 3, planes: &[(5, 3), (3, 2), (3, 2)] },
        Case { pixfmt: PixelFormat::Nv12, w: 3, h: 5, planes: &[(3, 5), (4, 3)] },
        Case { pixfmt: PixelFormat::P010, w: 3, h: 5, planes: &[(6, 5), (8, 3)] },
        Case { pixfmt: PixelFormat::I422, w: 5, h: 3, planes: &[(5, 3), (3, 3), (3, 3)] },
        Case { pixfmt: PixelFormat::I422P10, w: 3, h: 5, planes: &[(6, 5), (4, 5), (4, 5)] },
        Case { pixfmt: PixelFormat::Rgb24, w: 3, h: 5, planes: &[(9, 5)] },
    ];

    #[test]
    fn plane_geometry_table() {
        for c in CASES {
            assert_eq!(
                plane_count(c.pixfmt),
                c.planes.len(),
                "{:?} {}x{} plane count",
                c.pixfmt,
                c.w,
                c.h
            );
            let mut total = 0usize;
            let mut off = 0usize;
            for (i, &(stride, height)) in c.planes.iter().enumerate() {
                let want = PlaneGeometry { stride, height, size: stride * height };
                let got = plane_geometry(c.pixfmt, c.w, c.h, i).expect("plane in range");
                assert_eq!(got, want, "{:?} {}x{} plane {i}", c.pixfmt, c.w, c.h);
                assert_eq!(
                    plane_offset(c.pixfmt, c.w, c.h, i),
                    Some(off),
                    "{:?} {}x{} plane {i} offset",
                    c.pixfmt,
                    c.w,
                    c.h
                );
                off += want.size;
                total += want.size;
            }
            assert_eq!(
                frame_size(c.pixfmt, c.w, c.h),
                total,
                "{:?} {}x{} total = sum of planes",
                c.pixfmt,
                c.w,
                c.h
            );
            assert_eq!(plane_geometry(c.pixfmt, c.w, c.h, c.planes.len()), None);
            assert_eq!(plane_offset(c.pixfmt, c.w, c.h, c.planes.len()), None);
        }
    }

    #[test]
    fn odd_420_chroma_rounds_up_not_down() {
        let cb = plane_geometry(PixelFormat::I420, 3, 3, 1).unwrap();
        assert_eq!((cb.stride, cb.height), (2, 2), "3x3 chroma rounds up to 2x2");
        let cb1 = plane_geometry(PixelFormat::I420, 1, 1, 1).unwrap();
        assert_eq!((cb1.stride, cb1.height), (1, 1), "1x1 chroma is 1x1, never empty");
    }

    #[test]
    fn known_total_sizes() {
        assert_eq!(frame_size(PixelFormat::I420, 4, 4), 16 + 4 + 4);
        assert_eq!(frame_size(PixelFormat::Nv12, 4, 4), 16 + 8);
        assert_eq!(frame_size(PixelFormat::P010, 4, 4), 2 * (16 + 8));
        assert_eq!(frame_size(PixelFormat::I422, 4, 4), 16 + 8 + 8);
        assert_eq!(frame_size(PixelFormat::I422P10, 4, 4), 2 * (16 + 8 + 8));
        assert_eq!(frame_size(PixelFormat::Rgb24, 4, 4), 48);
        assert_eq!(frame_size(PixelFormat::Gray8, 4, 4), 16);
        assert_eq!(frame_size(PixelFormat::I420, 16, 16), 256 + 64 + 64);
    }
}
