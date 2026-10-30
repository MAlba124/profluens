//! RFC 7845 channel mapping family 1, its layout and the reorder from Vorbis to mask order.

use crate::head::OpusHead;

pub const MAX_CHANNELS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub channels: u8,
    pub streams: u8,
    pub coupled: u8,
    pub mapping: [u8; MAX_CHANNELS],
}

impl Layout {
    pub fn from_head(head: &OpusHead) -> Option<Layout> {
        let table = head.mapping();
        let n = table.len();
        if head.mapping_family != 1 || n == 0 || n > MAX_CHANNELS {
            return None;
        }
        let mut mapping = [255u8; MAX_CHANNELS];
        mapping[..n].copy_from_slice(table);
        Some(Layout {
            channels: n as u8,
            streams: head.stream_count,
            coupled: head.coupled_count,
            mapping,
        })
    }

    pub fn mapping(&self) -> &[u8] {
        &self.mapping[..self.channels as usize]
    }
}

/// `CANONICAL[n][c]` is the Vorbis order channel (RFC 7845 section 5.1.1.2) that channel `c`
/// of `n` in channel mask order (`profluens_audio::layout`) takes.
const CANONICAL: [[u8; MAX_CHANNELS]; MAX_CHANNELS + 1] = [
    [0, 1, 2, 3, 4, 5, 6, 7],
    [0, 1, 2, 3, 4, 5, 6, 7],
    [0, 1, 2, 3, 4, 5, 6, 7],
    [0, 2, 1, 3, 4, 5, 6, 7],
    [0, 1, 2, 3, 4, 5, 6, 7],
    [0, 2, 1, 3, 4, 5, 6, 7],
    [0, 2, 1, 5, 3, 4, 6, 7],
    [0, 2, 1, 6, 5, 3, 4, 7],
    [0, 2, 1, 7, 5, 6, 3, 4],
];

pub fn canonical_order(n: usize) -> Option<[u8; MAX_CHANNELS]> {
    let order = *CANONICAL.get(n)?;
    let identity = order[..n].iter().enumerate().all(|(c, &v)| usize::from(v) == c);
    if identity { None } else { Some(order) }
}

pub fn reorder<T: Copy + Default>(pcm: &mut [T], channels: usize, order: &[u8; MAX_CHANNELS]) {
    if channels == 0 || channels > MAX_CHANNELS {
        return;
    }
    let order = &order[..channels];
    for frame in pcm.chunks_exact_mut(channels) {
        let mut old = [T::default(); MAX_CHANNELS];
        old[..channels].copy_from_slice(frame);
        for (d, &o) in frame.iter_mut().zip(order) {
            *d = old[usize::from(o)];
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test heads, not a hot path
mod tests {
    use super::*;

    #[test]
    fn vorbis_order_lands_in_the_mask_order() {
        const L: i16 = 1;
        const R: i16 = 2;
        const C: i16 = 3;
        const LFE: i16 = 4;
        const LS: i16 = 5;
        const RS: i16 = 6;
        const LB: i16 = 7;
        const RB: i16 = 8;
        const CS: i16 = 9;
        let rows: [(&[i16], &[i16]); 8] = [
            (&[C], &[C]),
            (&[L, R], &[L, R]),
            (&[L, C, R], &[L, R, C]),
            (&[L, R, LS, RS], &[L, R, LS, RS]),
            (&[L, C, R, LS, RS], &[L, R, C, LS, RS]),
            (&[L, C, R, LS, RS, LFE], &[L, R, C, LFE, LS, RS]),
            (&[L, C, R, LS, RS, CS, LFE], &[L, R, C, LFE, CS, LS, RS]),
            (&[L, C, R, LS, RS, LB, RB, LFE], &[L, R, C, LFE, LB, RB, LS, RS]),
        ];
        for (vorbis, canonical) in rows {
            let n = vorbis.len();
            // The second frame scaled, so a frame boundary slip shows.
            let mut pcm = [0i16; 2 * MAX_CHANNELS];
            pcm[..n].copy_from_slice(vorbis);
            for (d, &v) in pcm[n..2 * n].iter_mut().zip(vorbis) {
                *d = v * 10;
            }
            match canonical_order(n) {
                Some(order) => reorder(&mut pcm[..2 * n], n, &order),
                None => assert_eq!(vorbis, canonical, "{n} channels need no reorder"),
            }
            assert_eq!(&pcm[..n], canonical, "{n} channels");
            for (got, want) in pcm[n..2 * n].iter().zip(canonical) {
                assert_eq!(*got, want * 10, "{n} channels, second frame");
            }
        }
    }

    #[test]
    fn a_family_1_head_gives_its_layout_and_others_none() {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 6, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 1, 4, 2]);
        head.extend_from_slice(&[0, 4, 1, 2, 3, 5]);
        let parsed = OpusHead::parse(&head).expect("a valid 5.1 head");
        let layout = Layout::from_head(&parsed).expect("family 1");
        assert_eq!((layout.channels, layout.streams, layout.coupled), (6, 4, 2));
        assert_eq!(layout.mapping(), &[0, 4, 1, 2, 3, 5]);
        let mut stereo = b"OpusHead".to_vec();
        stereo.extend_from_slice(&[1, 2, 0, 0, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        assert_eq!(Layout::from_head(&OpusHead::parse(&stereo).expect("family 0")), None);
    }
}
