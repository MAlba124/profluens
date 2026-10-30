//! Coverage-guided fuzzing of the demuxers under the stepped driver. The rig is
//! [`rig`], and each demuxer is a module that makes it as the player does.

pub mod avi;
pub mod flv;
pub mod mkv;
pub mod mp4;
pub mod ogg;
pub mod ps;
pub mod rig;
pub mod ts;

pub use rig::{Bounds, Broke, CONTROL, Made, Plan, Ran, Target, fuzz, run};
