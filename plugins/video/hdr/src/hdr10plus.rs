//! SMPTE ST 2094-40 (HDR10+) dynamic metadata, as FFmpeg reads it from T.35.

use crate::bits::{BitWriter, Bits};

/// Units of 1/100000 of 10000 cd/m2 (ST 2094-40).
pub const RGB_DEN: u32 = 100_000;
/// `fraction_bright_pixels` in units of 1 / 1000.
pub const FRACTION_DEN: u32 = 1000;
/// Knee point x and y in units of 1 / 4095.
pub const KNEE_DEN: u32 = 4095;
/// Bezier curve anchors in units of 1 / 1023.
pub const BEZIER_DEN: u32 = 1023;
/// `color_saturation_weight` in units of 1 / 8.
pub const SATURATION_DEN: u32 = 8;
/// An actual peak luminance grid value in units of 1 / 15.
pub const PEAK_DEN: u32 = 15;
/// FFmpeg's `AV_HDR_PLUS_MAX_PAYLOAD_SIZE`.
pub const MAX_PAYLOAD: usize = 907;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hdr10PlusError {
    Short,
    TooLarge(usize),
    Windows(u8),
    Grid { rows: u8, cols: u8 },
}

impl std::fmt::Display for Hdr10PlusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Hdr10PlusError::Short => write!(f, "the payload ends inside a field"),
            Hdr10PlusError::TooLarge(n) => write!(f, "{n} bytes, past {MAX_PAYLOAD}"),
            Hdr10PlusError::Windows(n) => write!(f, "num_windows {n}"),
            Hdr10PlusError::Grid { rows, cols } => {
                write!(f, "a {rows} by {cols} peak luminance grid")
            }
        }
    }
}

impl std::error::Error for Hdr10PlusError {}

/// The ellipse of a processing window past the first, which is the whole picture.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct WindowShape {
    pub upper_left: [u16; 2],
    pub lower_right: [u16; 2],
    pub center_of_ellipse: [u16; 2],
    pub rotation_angle: u8,
    pub semimajor_axis_internal: u16,
    pub semimajor_axis_external: u16,
    pub semiminor_axis_external: u16,
    pub overlap_process_option: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ToneMap {
    pub knee_point_x: u16,
    pub knee_point_y: u16,
    pub num_bezier_curve_anchors: u8,
    pub bezier_curve_anchors: [u16; 15],
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Window {
    /// `None` for window 0.
    pub shape: Option<WindowShape>,
    pub maxscl: [u32; 3],
    pub average_maxrgb: u32,
    pub num_distribution_maxrgb_percentiles: u8,
    /// `(percentage, percentile)`.
    pub distribution_maxrgb: [(u8, u32); 15],
    pub fraction_bright_pixels: u16,
    pub tone_mapping: Option<ToneMap>,
    pub color_saturation_weight: Option<u8>,
}

/// An actual peak luminance grid, 2 to 25 rows and columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PeakGrid {
    pub rows: u8,
    pub cols: u8,
    pub values: [[u8; 25]; 25],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hdr10Plus {
    pub application_version: u8,
    /// 1 to 3.
    pub num_windows: u8,
    pub windows: [Window; 3],
    /// `targeted_system_display_maximum_luminance`, cd/m2.
    pub targeted_system_display_maximum_luminance: u32,
    pub targeted_system_display_actual_peak_luminance: Option<PeakGrid>,
    pub mastering_display_actual_peak_luminance: Option<PeakGrid>,
}

impl Hdr10Plus {
    pub fn parse(payload: &[u8]) -> Result<Hdr10Plus, Hdr10PlusError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(Hdr10PlusError::TooLarge(payload.len()));
        }
        let mut b = Bits::new(payload);
        let s = Hdr10PlusError::Short;
        let mut h = Hdr10Plus {
            application_version: b.u(8).ok_or(s)? as u8,
            num_windows: b.u(2).ok_or(s)? as u8,
            windows: [Window::default(); 3],
            targeted_system_display_maximum_luminance: 0,
            targeted_system_display_actual_peak_luminance: None,
            mastering_display_actual_peak_luminance: None,
        };
        let windows = usize::from(h.num_windows);
        if windows == 0 {
            return Err(Hdr10PlusError::Windows(h.num_windows));
        }
        for w in &mut h.windows[1..windows] {
            let mut u16s = [0u16; 6];
            for v in &mut u16s {
                *v = b.u(16).ok_or(s)? as u16;
            }
            let rotation_angle = b.u(8).ok_or(s)? as u8;
            let mut axes = [0u16; 3];
            for v in &mut axes {
                *v = b.u(16).ok_or(s)? as u16;
            }
            w.shape = Some(WindowShape {
                upper_left: [u16s[0], u16s[1]],
                lower_right: [u16s[2], u16s[3]],
                center_of_ellipse: [u16s[4], u16s[5]],
                rotation_angle,
                semimajor_axis_internal: axes[0],
                semimajor_axis_external: axes[1],
                semiminor_axis_external: axes[2],
                overlap_process_option: b.flag().ok_or(s)?,
            });
        }
        h.targeted_system_display_maximum_luminance = b.u(27).ok_or(s)?;
        if b.flag().ok_or(s)? {
            h.targeted_system_display_actual_peak_luminance = Some(grid(&mut b)?);
        }
        for w in &mut h.windows[..windows] {
            for v in &mut w.maxscl {
                *v = b.u(17).ok_or(s)?;
            }
            w.average_maxrgb = b.u(17).ok_or(s)?;
            w.num_distribution_maxrgb_percentiles = b.u(4).ok_or(s)? as u8;
            let n = usize::from(w.num_distribution_maxrgb_percentiles);
            for p in &mut w.distribution_maxrgb[..n] {
                *p = (b.u(7).ok_or(s)? as u8, b.u(17).ok_or(s)?);
            }
            w.fraction_bright_pixels = b.u(10).ok_or(s)? as u16;
        }
        if b.flag().ok_or(s)? {
            h.mastering_display_actual_peak_luminance = Some(grid(&mut b)?);
        }
        for w in &mut h.windows[..windows] {
            if b.flag().ok_or(s)? {
                let mut t = ToneMap {
                    knee_point_x: b.u(12).ok_or(s)? as u16,
                    knee_point_y: b.u(12).ok_or(s)? as u16,
                    num_bezier_curve_anchors: b.u(4).ok_or(s)? as u8,
                    bezier_curve_anchors: [0; 15],
                };
                for a in &mut t.bezier_curve_anchors[..usize::from(t.num_bezier_curve_anchors)] {
                    *a = b.u(10).ok_or(s)? as u16;
                }
                w.tone_mapping = Some(t);
            }
            if b.flag().ok_or(s)? {
                w.color_saturation_weight = Some(b.u(6).ok_or(s)? as u8);
            }
        }
        Ok(h)
    }

    /// The brightest window 0 `maxscl` component in cd/m2, the scene peak.
    pub fn scene_peak_nits(&self) -> f32 {
        let m = self.windows[0].maxscl.iter().copied().max().unwrap_or(0);
        m as f32 * 10_000.0 / RGB_DEN as f32
    }
}

fn grid(b: &mut Bits<'_>) -> Result<PeakGrid, Hdr10PlusError> {
    let s = Hdr10PlusError::Short;
    let rows = b.u(5).ok_or(s)? as u8;
    let cols = b.u(5).ok_or(s)? as u8;
    if !(2..=25).contains(&rows) || !(2..=25).contains(&cols) {
        return Err(Hdr10PlusError::Grid { rows, cols });
    }
    let mut g = PeakGrid { rows, cols, values: [[0; 25]; 25] };
    for row in &mut g.values[..usize::from(rows)] {
        for v in &mut row[..usize::from(cols)] {
            *v = b.u(4).ok_or(s)? as u8;
        }
    }
    Ok(g)
}

/// A one window payload, the shape encoders write for a plain frame.
pub fn sample_payload(maxscl: [u32; 3], targeted_nits: u32) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(1, 8).put(1, 2); // application_version 1, one window
    w.put(targeted_nits, 27).put(0, 1);
    for v in maxscl {
        w.put(v, 17);
    }
    w.put(maxscl[0] / 4, 17); // average_maxrgb
    w.put(3, 4);
    for (pct, v) in [(1u32, 10u32), (50, 2000), (99, maxscl[0])] {
        w.put(pct, 7).put(v, 17);
    }
    w.put(5, 10); // fraction_bright_pixels
    w.put(0, 1); // mastering_display_actual_peak_luminance_flag
    w.put(1, 1).put(2048, 12).put(1024, 12).put(2, 4).put(300, 10).put(700, 10);
    w.put(0, 1); // color_saturation_mapping_flag
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_window_payload_reads_every_field() {
        let h = Hdr10Plus::parse(&sample_payload([40_000, 30_000, 20_000], 400)).unwrap();
        assert_eq!((h.application_version, h.num_windows), (1, 1));
        assert_eq!(h.targeted_system_display_maximum_luminance, 400);
        let w = h.windows[0];
        assert_eq!(w.shape, None);
        assert_eq!(w.maxscl, [40_000, 30_000, 20_000]);
        assert_eq!(w.average_maxrgb, 10_000);
        assert_eq!(w.num_distribution_maxrgb_percentiles, 3);
        assert_eq!(w.distribution_maxrgb[..3], [(1, 10), (50, 2000), (99, 40_000)]);
        assert_eq!(w.fraction_bright_pixels, 5);
        let t = w.tone_mapping.unwrap();
        assert_eq!((t.knee_point_x, t.knee_point_y, t.num_bezier_curve_anchors), (2048, 1024, 2));
        assert_eq!(t.bezier_curve_anchors[..2], [300, 700]);
        assert_eq!(w.color_saturation_weight, None);
        assert_eq!(h.scene_peak_nits(), 4000.0);
    }

    #[test]
    fn a_cut_payload_is_short_at_every_length() {
        let p = sample_payload([40_000, 30_000, 20_000], 400);
        for n in 0..p.len() - 1 {
            assert_eq!(Hdr10Plus::parse(&p[..n]), Err(Hdr10PlusError::Short), "cut at {n}");
        }
    }

    #[test]
    fn windows_grids_and_size_are_checked() {
        let mut w = BitWriter::default();
        w.put(1, 8).put(0, 2);
        assert_eq!(Hdr10Plus::parse(&w.into_bytes()), Err(Hdr10PlusError::Windows(0)));
        let mut w = BitWriter::default();
        w.put(1, 8).put(1, 2).put(1000, 27).put(1, 1).put(1, 5).put(3, 5);
        assert_eq!(
            Hdr10Plus::parse(&w.into_bytes()),
            Err(Hdr10PlusError::Grid { rows: 1, cols: 3 })
        );
        assert_eq!(
            Hdr10Plus::parse(&[0; MAX_PAYLOAD + 1]),
            Err(Hdr10PlusError::TooLarge(MAX_PAYLOAD + 1))
        );
    }

    #[test]
    fn a_second_window_reads_its_ellipse_and_a_grid_its_values() {
        let mut w = BitWriter::default();
        w.put(0, 8).put(2, 2);
        for v in [1u32, 2, 3, 4, 5, 6] {
            w.put(v, 16);
        }
        w.put(45, 8).put(7, 16).put(8, 16).put(9, 16).put(1, 1);
        w.put(1000, 27).put(1, 1).put(2, 5).put(2, 5);
        for v in [1u32, 2, 3, 4] {
            w.put(v, 4);
        }
        for _ in 0..2 {
            for v in [100u32, 200, 300, 150] {
                w.put(v, 17);
            }
            w.put(0, 4).put(1, 10);
        }
        w.put(0, 1);
        for _ in 0..2 {
            w.put(0, 1).put(1, 1).put(4, 6);
        }
        let h = Hdr10Plus::parse(&w.into_bytes()).unwrap();
        let shape = h.windows[1].shape.unwrap();
        assert_eq!(
            (shape.upper_left, shape.lower_right, shape.center_of_ellipse),
            ([1, 2], [3, 4], [5, 6])
        );
        assert_eq!(
            (shape.rotation_angle, shape.semiminor_axis_external, shape.overlap_process_option),
            (45, 9, true)
        );
        let g = h.targeted_system_display_actual_peak_luminance.unwrap();
        assert_eq!(
            (g.rows, g.cols, &g.values[0][..2], &g.values[1][..2]),
            (2, 2, &[1, 2][..], &[3, 4][..])
        );
        assert_eq!(h.windows[1].maxscl, [100, 200, 300]);
        assert_eq!(h.windows[1].color_saturation_weight, Some(4));
    }
}
