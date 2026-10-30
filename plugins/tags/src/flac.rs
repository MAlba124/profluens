//! Native FLAC metadata chain extent (RFC 9639 8.1). Contents are [`flacmeta`](crate::flacmeta)'s.

use profluens_core::event::TagSink;

use crate::Props;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plan {
    Complete { meta_end: u64 },
    Need { end: u64 },
}

const BLOCK_HEADER: u64 = 4;

/// Ask for this much when a block header is cut off, so the walk needs no read per block.
const HEADER_LOOKAHEAD: u64 = 4096;

/// How far the metadata chain reaches. `window` starts at file offset 0 with `fLaC` at `body`.
pub(crate) fn plan(window: &[u8], body: u64, file_len: u64) -> Plan {
    let mut p = body + BLOCK_HEADER; // past "fLaC"
    loop {
        let Ok(at) = usize::try_from(p) else {
            return Plan::Complete { meta_end: p };
        };
        let Some(h) = window.get(at..at + 4) else {
            return Plan::Need { end: (p + BLOCK_HEADER + HEADER_LOOKAHEAD).min(file_len) };
        };
        let last = h[0] & 0x80 != 0;
        let len = u64::from(u32::from_be_bytes([0, h[1], h[2], h[3]]));
        let end = p + BLOCK_HEADER + len;

        if (window.len().saturating_sub(at + 4) as u64) < len {
            return Plan::Need { end: end.min(file_len) };
        }
        p = end;
        if last || end >= file_len {
            return Plan::Complete { meta_end: end };
        }
    }
}

/// A rate of 0 or a sample count of 0 yields no duration (RFC 9639 8.2).
pub(crate) fn props(window: &[u8], body: u64) -> Props {
    let mut props = Props::default();
    let Some(from) = window.get(usize::try_from(body).unwrap_or(usize::MAX)..) else {
        return props;
    };
    let Some(si) = crate::flacmeta::stream_info(from) else {
        return props;
    };
    props.channels = Some(u32::from(si.channels));
    if si.sample_rate > 0 {
        props.sample_rate = Some(si.sample_rate);
        if si.total_samples > 0 {
            // u128 since the 36-bit `total_samples` times 1e9 overflows u64.
            props.duration_ns = u64::try_from(
                u128::from(si.total_samples) * 1_000_000_000 / u128::from(si.sample_rate),
            )
            .ok();
            props.duration_exact = true;
        }
    }
    props
}

pub(crate) fn emit_tags(window: &[u8], body: u64, sink: &mut impl TagSink) {
    let Some(from) = window.get(usize::try_from(body).unwrap_or(usize::MAX)..) else {
        return;
    };
    crate::flacmeta::parse_into(from, sink);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // test fixtures

    use super::*;
    use profluens_core::event::TagListSink;
    use profluens_core::memory::Pool;

    use crate::fixture::{flac_block, streaminfo};

    fn file() -> Vec<u8> {
        let mut f = b"fLaC".to_vec();
        f.extend_from_slice(&flac_block(false, 0, &streaminfo(44_100, 2, 16, 44_100 * 3)));
        f.extend_from_slice(&flac_block(
            false,
            4,
            &crate::fixture::vorbis_comment(&["TITLE=Set Theory", "date=2015"]),
        ));
        f.extend_from_slice(&flac_block(
            true,
            6,
            &crate::fixture::picture_block("image/png", b"\x89PNG-bytes"),
        ));
        f
    }

    #[test]
    fn plan_and_streaminfo() {
        let f = file();
        let props = props(&f, 0);
        assert_eq!(plan(&f, 0, f.len() as u64), Plan::Complete { meta_end: f.len() as u64 });
        assert_eq!(props.sample_rate, Some(44_100));
        assert_eq!(props.channels, Some(2));
        assert_eq!(props.duration_ns, Some(3_000_000_000));
        assert!(props.duration_exact);
    }

    #[test]
    fn a_short_window_asks_for_the_chain_end() {
        let f = file();
        let cut = f.len() - 4;
        assert_eq!(plan(&f[..cut], 0, f.len() as u64), Plan::Need { end: f.len() as u64 });
        assert_eq!(props(&f[..cut], 0).sample_rate, Some(44_100));
    }

    #[test]
    fn tags_are_forwarded() {
        let f = file();
        let pool = Pool::bounded(4096, 4);
        let mut sink = TagListSink::new(&pool);
        emit_tags(&f, 0, &mut sink);
        let tags = sink.finish();
        assert_eq!(tags.get("TITLE"), Some("Set Theory"));
        assert_eq!(tags.get("DATE"), Some("2015"));
        assert_eq!(tags.pictures().len(), 1);
        assert_eq!(&*tags.pictures()[0].mime, "image/png");
        assert_eq!(tags.pictures()[0].data.data(), b"\x89PNG-bytes");
    }

    #[test]
    fn unknown_total_samples_yields_no_duration() {
        let mut f = b"fLaC".to_vec();
        f.extend_from_slice(&flac_block(true, 0, &streaminfo(48_000, 1, 24, 0)));
        let props = props(&f, 0);
        assert_eq!(props.sample_rate, Some(48_000));
        assert_eq!(props.channels, Some(1));
        assert_eq!(props.duration_ns, None);
        assert!(!props.duration_exact);
    }

    #[test]
    fn truncation_never_panics() {
        let f = file();
        let pool = Pool::bounded(4096, 8);
        for n in 0..f.len() {
            let _ = plan(&f[..n], 0, n as u64);
            let _ = props(&f[..n], 0);
            let mut sink = TagListSink::new(&pool);
            emit_tags(&f[..n], 0, &mut sink);
        }
    }

    #[test]
    fn a_lying_block_length_is_bounded_by_the_file() {
        let mut f = b"fLaC".to_vec();
        f.extend_from_slice(&[0x00, 0xFF, 0xFF, 0xFF]);
        f.extend_from_slice(&[0u8; 4]);
        assert_eq!(
            plan(&f, 0, f.len() as u64),
            Plan::Need { end: f.len() as u64 },
            "clamped to EOF, not 16 MiB"
        );
    }
}
