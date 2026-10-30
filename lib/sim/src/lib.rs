//! The deterministic simulator. Without a feature it is data and functions over data, with no
//! dependency. The `driver` feature adds the run loop.

// Denied, not forbidden, so `corrupt::heap` can implement `GlobalAlloc`.
#![deny(unsafe_code)]

pub mod action;
pub mod corrupt;
pub mod driver;
pub mod ideal;
pub mod media;
pub mod prng;
pub mod profile;
pub mod record;
pub mod shrink;
pub mod sweep;
pub mod time;
