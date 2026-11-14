//! Sans-IO MPEG-TS demuxer (ISO/IEC 13818-1, EN 300 468, A/52), no dependencies, IO or clock.
//! Events go to a callback so a unit borrows its PID's buffer instead of being copied twice.
//! The earliest sync byte with `sync_confirm` repeats locks, ties going 188, then 192, then 204.
//! Not covered are RS correction, PAT sections past 0, tables past PAT and PMT, decryption and
//! ES parsing.

#![forbid(unsafe_code)]

mod crc;
mod demux;
pub mod packet;
pub mod pes;
pub mod psi;
pub mod sync;
pub mod time;

pub use crc::crc32;
pub use demux::{
    ANOMALY_KINDS, Anomaly, AnomalyKind, Config, ConfigError, Demux, Event, Limits, MIN_PES_BYTES,
    PesUnit, Program, RESYNC_BUDGET, SYNC_CONFIRM_DEFAULT, SYNC_CONFIRM_MAX, Stats, UF_CORRUPT,
    UF_DATA_ALIGNED, UF_DISCONTINUITY, UF_ENCRYPTED, UF_RANDOM_ACCESS, UF_SCRAMBLED, UF_TRUNCATED,
    anomaly_count, anomaly_total,
};
pub use psi::{Codec, Encryption, Media, StreamInfo, language_str, media_of};
pub use sync::{PacketSize, framed_len};
pub use time::{Span, Spans, Step, Unwrapped, Unwrapper, congruent};
