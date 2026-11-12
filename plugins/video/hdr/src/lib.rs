//! Colour, HDR static metadata, HDR10+ and Dolby Vision.

#![deny(unsafe_code)]

pub mod av1;
pub mod bits;
pub mod carry;
pub mod color;
pub mod dovi;
pub mod emit;
pub mod h26x;
pub mod hdr10plus;
pub mod rpu;
pub mod side;
pub mod t35;
pub mod vp9;

pub use color::StreamColor;
pub use dovi::DoviConfig;
pub use side::FrameMeta;
