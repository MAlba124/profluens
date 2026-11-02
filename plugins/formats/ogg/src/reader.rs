//! `OggReader`, the demuxer of verified pages into reassembled packets (RFC 3533 sections 5 and 6).

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::page::{CAPTURE_PATTERN, HEADER_FIXED_LEN, PageError, PageHeader};

/// A reassembled packet handed out by the demuxer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Packet {
    /// Which logical bitstream this packet belongs to (section 6, field 5).
    pub serial: u32,
    /// The granule position of the page this packet finished on (section 6, field 4).
    pub granule: u64,
    /// True if this packet is the first packet of its logical bitstream.
    pub bos: bool,
    /// True if this packet finished on the eos page of its bitstream (section 6, flag 0x04).
    pub eos: bool,
    /// True for the last packet finishing on its page, the one `granule` ends.
    pub last: bool,
    /// The packet payload (concatenated segment bytes).
    pub data: Vec<u8>,
}

struct StreamState {
    partial: Vec<u8>,
    in_progress: bool,
    pending_bos: bool,
    discarding: bool,
    last_page: u64,
}

/// A packet has no length field (RFC 3533 section 5), so one that never ends is dropped past this.
const MAX_PACKET_BYTES: usize = 64 << 20;

/// Streams with reassembly state at once. Only streams that never send eos count, the stalest goes.
const MAX_STREAMS: usize = 256;

impl StreamState {
    // Cold, one per logical bitstream (created at its bos, not per page).
    #[allow(clippy::disallowed_methods)]
    fn new() -> Self {
        Self {
            partial: Vec::new(),
            in_progress: false,
            pending_bos: false,
            discarding: false,
            last_page: 0,
        }
    }
}

/// What [`OggReader::recycle`] retains, a byte budget plus a count backstop for tiny packets.
const SPARE_MAX_BYTES: usize = 256 * 1024;
const SPARE_MAX_COUNT: usize = 512;

/// How far the page parser runs ahead of the consumer, in queued packets.
const READY_AHEAD: usize = 32;

// Cold, only reached when the recycle list is empty.
#[allow(clippy::disallowed_methods)]
fn take_spare(spare: &mut Spare, want: usize) -> Vec<u8> {
    match spare.free.pop() {
        Some(mut v) => {
            spare.bytes -= v.capacity();
            v.clear(); // capacity retained
            v
        }
        None => Vec::with_capacity(want),
    }
}

#[derive(Default)]
struct Spare {
    free: Vec<Vec<u8>>,
    bytes: usize,
}

/// A streaming Ogg demuxer.
pub struct OggReader {
    buf: Vec<u8>,
    pos: usize,
    streams: HashMap<u32, StreamState>,
    ready: VecDeque<Packet>,
    spare: Spare,
    resyncs: u64,
    pages_parsed: u64,
    dropped: u64,
    max_packet: usize,
}

impl OggReader {
    // Cold, one-time constructor.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            streams: HashMap::new(),
            ready: VecDeque::new(),
            spare: Spare::default(),
            resyncs: 0,
            pages_parsed: 0,
            dropped: 0,
            max_packet: MAX_PACKET_BYTES,
        }
    }

    /// Feed more input.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        self.drive(READY_AHEAD);
    }

    /// Signal end of input.
    pub fn finish(&mut self) -> usize {
        // Unbounded, or the look-ahead cap would silently discard queued input.
        self.drive(usize::MAX);
        let leftover = self.buf.len() - self.pos;
        self.buf.clear();
        self.pos = 0;
        leftover
    }

    /// Discard every byte and packet fragment held, the seek/flush entry point. The recycle list
    /// is kept, it holds only capacity.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.streams.clear();
        while let Some(p) = self.ready.pop_front() {
            self.recycle(p.data);
        }
    }

    /// Pull the next ready packet, or `None` if none is currently assembled.
    pub fn next_packet(&mut self) -> Option<Packet> {
        if self.ready.is_empty() {
            self.drive(READY_AHEAD);
        }
        self.ready.pop_front()
    }

    /// Hand a drained packet's buffer back so the **next** packet reuses it instead of allocating.
    pub fn recycle(&mut self, mut data: Vec<u8>) {
        let cap = data.capacity();
        if cap == 0
            || self.spare.bytes + cap > SPARE_MAX_BYTES
            || self.spare.free.len() >= SPARE_MAX_COUNT
        {
            return;
        }
        data.clear();
        self.spare.bytes += cap;
        self.spare.free.push(data);
    }

    /// Number of resynchronisation events so far (bad capture pattern, version, or CRC).
    pub fn resync_count(&self) -> u64 {
        self.resyncs
    }

    /// Packets dropped so far, orphaned by a lost page or past the size limit.
    pub fn dropped_count(&self) -> u64 {
        self.dropped
    }

    /// Number of pages successfully parsed so far.
    pub fn pages_parsed(&self) -> u64 {
        self.pages_parsed
    }

    /// The packets assembled and not handed out yet, in order. A page's packets are queued
    /// together, so the ones after a packet handed out are here up to its page's end.
    pub fn queued(&self) -> impl Iterator<Item = &Packet> {
        self.ready.iter()
    }

    /// Bytes pushed and not handed out, parsed or not.
    pub fn held(&self) -> usize {
        self.buf.len() - self.pos + self.ready.iter().map(|p| p.data.len()).sum::<usize>()
    }

    fn drive(&mut self, ahead: usize) {
        loop {
            if self.ready.len() >= ahead {
                break; // look-ahead full, the rest stays buffered
            }
            let remaining = &self.buf[self.pos..];
            if remaining.len() < HEADER_FIXED_LEN {
                break; // wait for more bytes
            }
            match PageHeader::parse(remaining) {
                Ok(page) => {
                    let len = page.len();
                    // A free function since `page` borrows `self.buf`.
                    self.pages_parsed += 1;
                    handle_page(
                        &mut self.streams,
                        &mut self.ready,
                        &mut self.spare,
                        &page,
                        self.pages_parsed,
                        self.max_packet,
                        &mut self.dropped,
                    );
                    self.pos += len;
                }
                Err(PageError::NeedMoreHeader) | Err(PageError::NeedMoreBody) => {
                    // Wait only when byte 0 really is a capture pattern, else resync.
                    if remaining.len() >= 4 && remaining[0..4] == CAPTURE_PATTERN {
                        break; // genuine partial page: wait for more data
                    }
                    self.resync_one();
                }
                Err(PageError::BadCapturePattern)
                | Err(PageError::BadVersion(_))
                | Err(PageError::BadCrc { .. }) => {
                    self.resync_one();
                }
            }
            self.maybe_compact();
        }
        self.maybe_compact();
    }

    fn resync_one(&mut self) {
        self.resyncs += 1;
        let start = self.pos + 1;
        let hay = &self.buf[start..];
        match find_capture(hay) {
            Some(rel) => self.pos = start + rel,
            None => {
                let keep_from = self.buf.len().saturating_sub(CAPTURE_PATTERN.len() - 1);
                self.pos = keep_from.max(start);
            }
        }
    }

    /// Reclaim consumed bytes, only when the moved tail is small so a large push stays O(n).
    fn maybe_compact(&mut self) {
        const CONSUMED_TRIGGER: usize = 64 * 1024;
        const MAX_MOVE: usize = 64 * 1024;
        if self.pos == 0 {
            return;
        }
        let tail = self.buf.len() - self.pos;
        if tail == 0 {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos >= CONSUMED_TRIGGER && tail <= MAX_MOVE {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }
}

impl Default for OggReader {
    fn default() -> Self {
        Self::new()
    }
}

fn handle_page(
    streams: &mut HashMap<u32, StreamState>,
    ready: &mut VecDeque<Packet>,
    spare: &mut Spare,
    page: &PageHeader<'_>,
    page_number: u64,
    max_packet: usize,
    dropped: &mut u64,
) {
    let serial = page.serial();
    let granule = page.granule_position();
    let continued = page.is_continued();
    let bos = page.is_bos();
    let eos = page.is_eos();

    if streams.len() >= MAX_STREAMS && !streams.contains_key(&serial) {
        let stalest = streams.iter().min_by_key(|(_, s)| s.last_page).map(|(k, _)| *k);
        if let Some(stalest) = stalest {
            streams.remove(&stalest);
        }
    }
    let state = streams.entry(serial).or_insert_with(StreamState::new);
    state.last_page = page_number;
    if bos {
        state.pending_bos = true;
    }

    // A partial orphaned by a lost page. Drop it rather than splice across a gap.
    if !continued && state.in_progress {
        *dropped += 1;
        state.partial.clear();
        state.in_progress = false;
        state.discarding = false;
    }
    // The tail of a packet whose start was never seen, after a reset or a lost page.
    if continued && !state.in_progress {
        *dropped += 1;
        state.discarding = true;
    }

    // Lacing 255 continues the packet, < 255 ends it (RFC 3533 section 5).
    let table = page.segment_table();
    let payload = page.payload();
    let mut off = 0usize;
    let mut run_start = 0usize;
    let last_idx = table.len().wrapping_sub(1);
    // the granule is the end of the last packet finishing here, a packet may run on past it
    let last_end = table.iter().rposition(|&l| l != 255);

    for (i, &lacing) in table.iter().enumerate() {
        off += lacing as usize;
        if lacing == 255 {
            continue;
        }
        // A packet takes the granule of the page it finishes on (RFC 3533 section 6).
        let tail = &payload[run_start..off];
        run_start = off;
        if state.discarding {
            state.discarding = false;
            state.in_progress = false;
            continue;
        }
        let data = if state.partial.is_empty() {
            let mut v = take_spare(spare, tail.len());
            v.extend_from_slice(tail);
            v
        } else {
            state.partial.extend_from_slice(tail);
            std::mem::replace(&mut state.partial, take_spare(spare, 0))
        };
        state.in_progress = false;
        let is_last_on_page = i == last_idx;
        let packet = Packet {
            serial,
            granule,
            bos: state.pending_bos,
            // Only the packet completing on the eos page's last segment is the stream's final
            // packet.
            eos: eos && is_last_on_page,
            last: last_end == Some(i),
            data,
        };
        state.pending_bos = false;
        ready.push_back(packet);
    }

    if run_start < off {
        let run = &payload[run_start..off];
        if !state.discarding && state.partial.len() + run.len() > max_packet {
            drop(std::mem::take(&mut state.partial));
            state.discarding = true;
            *dropped += 1;
        }
        if !state.discarding {
            state.partial.extend_from_slice(run);
        }
        state.in_progress = true;
    }

    // The eos page is the last of its bitstream (RFC 3533 section 6), so its state goes with it.
    if eos {
        streams.remove(&serial);
    }
}

fn find_capture(hay: &[u8]) -> Option<usize> {
    if hay.len() < CAPTURE_PATTERN.len() {
        return None;
    }
    hay.windows(CAPTURE_PATTERN.len()).position(|w| w == CAPTURE_PATTERN)
}

// Cold, one-shot whole-stream convenience API (not the element's per-page path).
#[allow(clippy::disallowed_methods)]
pub fn demux_all(stream: &[u8]) -> Vec<Packet> {
    let mut reader = OggReader::new();
    reader.push(stream);
    reader.finish();
    let mut out = Vec::new();
    while let Some(p) = reader.next_packet() {
        out.push(p);
    }
    out
}

#[cfg(test)]
// Test vectors, not an element hot path.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::writer::mux_packets;

    #[test]
    fn find_capture_locates_pattern() {
        assert_eq!(find_capture(b"OggS"), Some(0));
        assert_eq!(find_capture(b"xxOggS"), Some(2));
        assert_eq!(find_capture(b"no pattern here"), None);
        assert_eq!(find_capture(b"Ogg"), None); // too short
        assert_eq!(find_capture(b"OggOggS"), Some(3)); // false start then real one
    }

    #[test]
    fn roundtrip_single_packet() {
        let stream = mux_packets(7, &[(&[1, 2, 3, 4], 100)]);
        let packets = demux_all(&stream);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].serial, 7);
        assert_eq!(packets[0].granule, 100);
        assert!(packets[0].bos);
        assert!(packets[0].eos);
        assert_eq!(packets[0].data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn streaming_push_in_tiny_chunks() {
        let src: Vec<Vec<u8>> = vec![vec![9; 300], vec![], vec![1, 2], vec![7; 600]];
        let refs: Vec<(&[u8], u64)> =
            src.iter().enumerate().map(|(i, p)| (p.as_slice(), i as u64)).collect();
        let stream = mux_packets(1, &refs);

        let mut reader = OggReader::new();
        let mut got = Vec::new();
        for &b in &stream {
            reader.push(&[b]);
            while let Some(p) = reader.next_packet() {
                got.push(p);
            }
        }
        reader.finish();
        while let Some(p) = reader.next_packet() {
            got.push(p);
        }

        assert_eq!(got.len(), src.len());
        for (i, p) in got.iter().enumerate() {
            assert_eq!(p.data, src[i], "packet {i}");
        }
    }

    #[test]
    fn bos_eos_flags_on_first_and_last_packet() {
        let src: Vec<Vec<u8>> = (0..5u32).map(|i| vec![i as u8; 10]).collect();
        let refs: Vec<(&[u8], u64)> =
            src.iter().enumerate().map(|(i, p)| (p.as_slice(), i as u64 * 10)).collect();
        let stream = mux_packets(2, &refs);
        let packets = demux_all(&stream);
        assert_eq!(packets.len(), 5);
        assert!(packets.first().unwrap().bos);
        assert!(!packets.first().unwrap().eos);
        assert!(packets.last().unwrap().eos);
        assert!(!packets.last().unwrap().bos);
        for p in &packets[1..4] {
            assert!(!p.bos);
            assert!(!p.eos);
        }
    }

    fn page(out: &mut Vec<u8>, serial: u32, flags: u8, len: u8) {
        crate::page::write_page(out, flags, 0, serial, 0, &[len], &vec![0x5A; len as usize]);
    }

    // Test code builds its pages on the heap.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn a_finished_stream_leaves_no_state_behind() {
        let mut stream = Vec::new();
        for serial in 0..1000u32 {
            page(&mut stream, serial, crate::page::flags::BOS, 3);
            page(&mut stream, serial, crate::page::flags::EOS, 4);
        }
        let mut reader = OggReader::new();
        reader.push(&stream);
        let mut packets = 0;
        while reader.next_packet().is_some() {
            packets += 1;
        }
        assert_eq!(packets, 2000);
        assert!(reader.streams.is_empty(), "{} streams still held", reader.streams.len());
    }

    // Test code builds its pages on the heap.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn unterminated_streams_are_capped_and_the_stalest_is_evicted() {
        let mut stream = Vec::new();
        crate::page::write_page(&mut stream, crate::page::flags::BOS, 0, 7, 0, &[255], &[1; 255]);
        for serial in 1000..1400u32 {
            page(&mut stream, serial, crate::page::flags::BOS, 2);
            crate::page::write_page(
                &mut stream,
                crate::page::flags::CONTINUED,
                0,
                7,
                0,
                &[255],
                &[2; 255],
            );
        }
        crate::page::write_page(&mut stream, crate::page::flags::CONTINUED, 9, 7, 0, &[0], &[]);
        let mut reader = OggReader::new();
        reader.push(&stream);
        let mut long_packet = None;
        while let Some(packet) = reader.next_packet() {
            if packet.serial == 7 {
                long_packet = Some(packet.data.len());
            }
        }
        assert!(reader.streams.len() <= MAX_STREAMS, "{} streams held", reader.streams.len());
        assert_eq!(long_packet, Some(255 * 401), "the live stream lost its packet");
    }

    // Test code builds its pages on the heap.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn a_packet_that_never_ends_is_dropped_at_the_size_limit() {
        let mut stream = Vec::new();
        let full = [255u8; 4];
        let body = [0xEE; 4 * 255];
        crate::page::write_page(&mut stream, crate::page::flags::BOS, 0, 1, 0, &full, &body);
        for _ in 0..40 {
            crate::page::write_page(
                &mut stream,
                crate::page::flags::CONTINUED,
                0,
                1,
                0,
                &full,
                &body,
            );
        }
        crate::page::write_page(
            &mut stream,
            crate::page::flags::CONTINUED,
            5,
            1,
            0,
            &[1, 2],
            &[7, 8, 9],
        );

        let mut reader = OggReader::new();
        reader.max_packet = 8 * 1024;
        let mut held = 0;
        let mut packets = Vec::new();
        for chunk in stream.chunks(1000) {
            reader.push(chunk);
            held = held.max(reader.streams.get(&1).map_or(0, |s| s.partial.capacity()));
            while let Some(packet) = reader.next_packet() {
                packets.push(packet.data);
            }
        }
        assert!(held <= 2 * 8 * 1024, "{held} octets of a runaway packet were held");
        assert_eq!(packets, vec![vec![8, 9]], "only the packet after the runaway one");
    }

    // Test code builds its pages on the heap.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn the_tail_of_a_packet_never_started_is_dropped() {
        use crate::page::flags::{BOS, CONTINUED};
        let (mut head, mut tail) = (Vec::new(), Vec::new());
        crate::page::write_page(&mut head, BOS, 0, 3, 0, &[255], &[1; 255]);
        // Ends the 255 octet packet with 10 more, then a whole one of 3.
        crate::page::write_page(
            &mut tail,
            CONTINUED,
            4,
            3,
            1,
            &[10, 3],
            &[[2u8; 10].as_slice(), &[0; 3]].concat(),
        );
        let mut spanning = Vec::new();
        crate::page::write_page(&mut spanning, CONTINUED, 0, 3, 1, &[255], &[4; 255]);
        crate::page::write_page(&mut spanning, CONTINUED, 4, 3, 2, &[5, 2], &[5, 5, 5, 5, 5, 6, 6]);

        let mut reader = OggReader::new();
        let drain = |reader: &mut OggReader| {
            let mut out = Vec::new();
            while let Some(p) = reader.next_packet() {
                out.push(p.data);
            }
            out
        };
        reader.push(&head);
        reader.push(&tail);
        assert_eq!(drain(&mut reader), vec![[[1; 255].as_slice(), &[2; 10]].concat(), vec![0; 3]]);
        assert_eq!(reader.dropped_count(), 0);

        // A seek lands on the continued page, its leading run belongs to no packet.
        reader.push(&head);
        reader.reset();
        reader.push(&tail);
        assert_eq!(drain(&mut reader), vec![vec![0; 3]]);
        assert_eq!(reader.dropped_count(), 1);

        // An orphaned run spanning a whole page is dropped up to where its packet ends.
        reader.reset();
        reader.push(&spanning);
        assert_eq!(drain(&mut reader), vec![vec![6, 6]]);
        assert_eq!(reader.dropped_count(), 2);
    }
}
