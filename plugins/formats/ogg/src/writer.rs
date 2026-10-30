//! `OggWriter`, the muxer of packets into pages (RFC 3533 sections 5 and 6).

use crate::page::{self, GRANULE_NONE, MAX_SEGMENTS, flags};

/// Muxes packets of a single logical bitstream (one serial number) into pages.
pub struct OggWriter {
    serial: u32,
    next_sequence: u32,
    seg_table: Vec<u8>,
    seg_payload: Vec<u8>,
    pending_granule: u64,
    pending_continued: bool,
    at_bos: bool,
    finished: bool,
}

/// Errors from the writer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteError {
    AfterEos,
}

impl OggWriter {
    // Cold, one-time constructor.
    #[allow(clippy::disallowed_methods)]
    pub fn new(serial: u32) -> Self {
        Self {
            serial,
            next_sequence: 0,
            seg_table: Vec::new(),
            seg_payload: Vec::new(),
            pending_granule: GRANULE_NONE,
            pending_continued: false,
            at_bos: true,
            finished: false,
        }
    }

    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// Append one packet with the given granule position, flushing full pages into `out` (section 5
    /// segmentation, section 6 paging).
    pub fn write_packet(
        &mut self,
        out: &mut Vec<u8>,
        packet: &[u8],
        granule: u64,
    ) -> Result<(), WriteError> {
        if self.finished {
            return Err(WriteError::AfterEos);
        }

        // Split into lacing values (section 5). A multiple of 255 ends with a 0 lacing value.
        let n_full = packet.len() / 255;
        let remainder = (packet.len() % 255) as u8;
        let mut off = 0usize;

        for _ in 0..n_full {
            self.push_segment(out, 255, &packet[off..off + 255]);
            off += 255;
        }
        self.push_segment(out, remainder, &packet[off..off + remainder as usize]);
        self.pending_granule = granule;

        Ok(())
    }

    fn push_segment(&mut self, out: &mut Vec<u8>, lacing: u8, bytes: &[u8]) {
        if self.seg_table.len() == MAX_SEGMENTS {
            self.flush_page(out, false);
        }
        self.seg_table.push(lacing);
        self.seg_payload.extend_from_slice(bytes);
    }

    /// Flush the pending page to `out` if it holds any segments (section 6).
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        if !self.seg_table.is_empty() {
            self.flush_page(out, false);
        }
    }

    fn flush_page(&mut self, out: &mut Vec<u8>, is_eos: bool) {
        let mut header_type = 0u8;
        if self.pending_continued {
            header_type |= flags::CONTINUED;
        }
        if self.at_bos {
            header_type |= flags::BOS;
        }
        if is_eos {
            header_type |= flags::EOS;
        }

        page::write_page(
            out,
            header_type,
            self.pending_granule,
            self.serial,
            self.next_sequence,
            &self.seg_table,
            &self.seg_payload,
        );

        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.at_bos = false;
        // A last lacing value of 255 means the next page continues the packet (section 6, flag
        // 0x01).
        self.pending_continued = self.seg_table.last() == Some(&255);
        self.seg_table.clear();
        self.seg_payload.clear();
        self.pending_granule = GRANULE_NONE;
    }

    /// Close the logical bitstream.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        if self.seg_table.is_empty() {
            // Nothing pending, so a nil eos page (section 6 allows it).
            self.seg_table.push(0);
        }
        self.flush_page(out, true);
        self.finished = true;
    }

    /// True once [`finish`](Self::finish) has emitted the eos page.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

// Cold, one-shot whole-stream convenience API (not the element's per-packet path).
#[allow(clippy::disallowed_methods)]
pub fn mux_packets(serial: u32, packets: &[(&[u8], u64)]) -> Vec<u8> {
    let mut w = OggWriter::new(serial);
    let mut out = Vec::new();
    for &(pkt, granule) in packets {
        w.write_packet(&mut out, pkt, granule).expect("mux");
    }
    w.finish(&mut out);
    out
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::page::PageHeader;

    type ParsedPage = (u8, u64, u32, Vec<u8>, Vec<u8>);

    fn parse_all(stream: &[u8]) -> Vec<ParsedPage> {
        let mut pages = Vec::new();
        let mut off = 0;
        while off < stream.len() {
            let page = PageHeader::parse(&stream[off..]).expect("valid page");
            pages.push((
                page.header_type(),
                page.granule_position(),
                page.sequence(),
                page.segment_table().to_vec(),
                page.payload().to_vec(),
            ));
            off += page.len();
        }
        assert_eq!(off, stream.len(), "stream fully consumed");
        pages
    }

    #[test]
    fn single_small_packet_one_page() {
        let stream = mux_packets(7, &[(&[1, 2, 3, 4], 100)]);
        let pages = parse_all(&stream);
        assert_eq!(pages.len(), 1);
        let (ht, gran, seq, table, payload) = &pages[0];
        assert_eq!(*ht, flags::BOS | flags::EOS);
        assert_eq!(*gran, 100);
        assert_eq!(*seq, 0);
        assert_eq!(table, &vec![4]);
        assert_eq!(payload, &vec![1, 2, 3, 4]);
    }

    #[test]
    fn packet_length_multiple_of_255_gets_terminating_zero() {
        let pkt: Vec<u8> = (0..510u32).map(|i| i as u8).collect();
        let stream = mux_packets(1, &[(&pkt, 5)]);
        let pages = parse_all(&stream);
        assert_eq!(pages.len(), 1);
        let (_ht, _gran, _seq, table, payload) = &pages[0];
        assert_eq!(table, &vec![255, 255, 0]);
        assert_eq!(payload.len(), 510);
        assert_eq!(payload, &pkt);
    }

    #[test]
    fn zero_length_packet_is_single_zero_lacing() {
        let stream = mux_packets(1, &[(&[], 0)]);
        let pages = parse_all(&stream);
        assert_eq!(pages.len(), 1);
        let (_ht, _gran, _seq, table, payload) = &pages[0];
        assert_eq!(table, &vec![0]);
        assert!(payload.is_empty());
    }

    #[test]
    fn packet_spanning_multiple_pages_sets_continued() {
        let pkt: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
        let stream = mux_packets(9, &[(&pkt, 42)]);
        let pages = parse_all(&stream);
        assert!(pages.len() >= 2, "packet must span >1 page, got {}", pages.len());

        let (ht0, gran0, seq0, table0, _) = &pages[0];
        assert_eq!(*ht0 & flags::BOS, flags::BOS);
        assert_eq!(*ht0 & flags::CONTINUED, 0, "bos page is not continued");
        assert_eq!(table0.len(), MAX_SEGMENTS);
        assert!(table0.iter().all(|&l| l == 255));
        assert_eq!(*gran0, GRANULE_NONE, "no packet finishes on page 0");
        assert_eq!(*seq0, 0);

        let (ht1, _gran1, seq1, _t1, _p1) = &pages[1];
        assert_eq!(*ht1 & flags::CONTINUED, flags::CONTINUED, "page 1 continues packet");
        assert_eq!(*seq1, 1);

        let mut reassembled = Vec::new();
        for (_ht, _g, _s, _t, payload) in &pages {
            reassembled.extend_from_slice(payload);
        }
        assert_eq!(reassembled, pkt);

        let last = pages.last().unwrap();
        assert_eq!(last.1, 42, "granule on the finishing page");
        assert_eq!(last.0 & flags::EOS, flags::EOS, "last page is eos");
    }

    #[test]
    fn many_small_packets_pack_into_one_segment_table() {
        let pkts: Vec<Vec<u8>> = (0..200u32).map(|i| vec![i as u8]).collect();
        let refs: Vec<(&[u8], u64)> =
            pkts.iter().enumerate().map(|(i, p)| (p.as_slice(), i as u64)).collect();
        let stream = mux_packets(3, &refs);
        let pages = parse_all(&stream);
        assert_eq!(pages.len(), 1);
        let (ht, gran, _seq, table, _payload) = &pages[0];
        assert_eq!(table.len(), 200);
        assert!(table.iter().all(|&l| l == 1));
        assert_eq!(*ht, flags::BOS | flags::EOS);
        assert_eq!(*gran, 199);
    }

    #[test]
    fn writing_after_finish_is_rejected() {
        let mut w = OggWriter::new(1);
        let mut out = Vec::new();
        w.write_packet(&mut out, &[1], 0).unwrap();
        w.finish(&mut out);
        assert_eq!(w.write_packet(&mut out, &[2], 1), Err(WriteError::AfterEos));
    }

    #[test]
    fn exactly_255_segments_then_more_packets_spills_pages() {
        let pkts: Vec<Vec<u8>> = (0..256u32).map(|i| vec![i as u8]).collect();
        let refs: Vec<(&[u8], u64)> =
            pkts.iter().enumerate().map(|(i, p)| (p.as_slice(), i as u64)).collect();
        let stream = mux_packets(4, &refs);
        let pages = parse_all(&stream);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].3.len(), 255); // first page: 255 lacing values
        assert_eq!(pages[0].1, 254);
        assert_eq!(pages[0].0 & flags::CONTINUED, 0);
        assert_eq!(pages[1].0 & flags::CONTINUED, 0);
        assert_eq!(pages[1].3, vec![1]); // one segment: packet 255
        assert_eq!(pages[1].1, 255);
    }
}
