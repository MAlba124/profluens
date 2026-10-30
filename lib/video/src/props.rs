//! Parse-path props of the fixed-format video elements, refined over the constructor format in
//! `start()`.

use profluens_core::couple::Coupling;
use profluens_core::ctx::Ctx;
use profluens_core::format::{Value, ValueDesc};
use profluens_core::time::Rational;

use crate::format::{FIELD_FPS, FIELD_HEIGHT, FIELD_PIXFMT, FIELD_WIDTH, PixelFormat, VideoFormat};

/// The src pad's format fields are its format properties, so the solve knows at link time what
/// start announces.
pub static FORMAT_COUPLINGS: [Coupling; 4] = [
    Coupling::prop(0, FIELD_WIDTH, "width"),
    Coupling::prop(0, FIELD_HEIGHT, "height"),
    Coupling::prop(0, FIELD_PIXFMT, "pixfmt"),
    Coupling::prop(0, FIELD_FPS, "fps"),
];

pub const fn presets_of(format: VideoFormat) -> [(&'static str, ValueDesc); 4] {
    [
        ("width", ValueDesc::Int(format.width as i64)),
        ("height", ValueDesc::Int(format.height as i64)),
        ("pixfmt", ValueDesc::Id(format.pixfmt.caps_name())),
        ("fps", ValueDesc::Rat(format.fps.num, format.fps.den)),
    ]
}

/// 320x240 I420 at 30 fps, before parse-path props refine it.
pub const DEFAULT_FORMAT: VideoFormat =
    VideoFormat::new(320, 240, PixelFormat::I420, Rational::new(30, 1));

/// Refine `format` from the `width`, `height`, `pixfmt` and `fps` props. Absent or malformed props
/// leave a field unchanged.
pub fn read_dims(ctx: &Ctx, mut format: VideoFormat) -> VideoFormat {
    if let Some(Value::Int(w)) = ctx.prop("width")
        && let Ok(w) = u32::try_from(w)
        && w > 0
    {
        format.width = w;
    }
    if let Some(Value::Int(h)) = ctx.prop("height")
        && let Ok(h) = u32::try_from(h)
        && h > 0
    {
        format.height = h;
    }
    if let Some(Value::Id(id)) = ctx.prop("pixfmt")
        && let Some(pf) = ctx.value_name(id).and_then(PixelFormat::from_caps_name)
    {
        format.pixfmt = pf;
    }
    if let Some(Value::Rat(num, den)) = ctx.prop("fps")
        && den != 0
    {
        format.fps = Rational::new(num, den);
    }
    format
}

/// A positive int prop `name`, else `default`.
pub fn read_u64(ctx: &Ctx, name: &str, default: u64) -> u64 {
    match ctx.prop(name) {
        Some(Value::Int(v)) if v >= 0 => v as u64,
        _ => default,
    }
}
