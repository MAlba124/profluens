//! `oggflacdeframe` turns a FLAC-in-Ogg stream into native FLAC bytes by stripping the 9-byte
//! mapping prefix from the first packet (<https://xiph.org/flac/ogg_mapping.html>).
//! `oggflacdec` does the same inside `flacdec`, so the `flac/ogg` lookup finds one element.

use profluens_audio::DELAY_FIELDS;
use profluens_core::batch::Inputs;
use profluens_core::ctx::Ctx;
use profluens_core::element::{
    Element, ElementDesc, Flow, InputPolicy, LatencyDesc, PadDesc, SchedHint,
};
use profluens_core::error::Error;
use profluens_core::event::Event;
use profluens_core::format::OfferDesc;
use profluens_core::id::{SinkPad, SrcPad};
use profluens_core::log;
use profluens_core::log::Level;

use crate::error::FlacError;
use crate::flacdec::{self, FlacDec};

const MAPPING_PACKET_TYPE: u8 = 0x7F;
const MAPPING_SIGNATURE: &[u8; 4] = b"FLAC";
/// Type (1), `FLAC` (4), major (1), minor (1), header packet count be16 (2).
const MAPPING_PREFIX_LEN: usize = 9;

static OFFERS: [OfferDesc; 1] = [OfferDesc::any("bytes")];

static SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &OFFERS }];
static SRCS: [PadDesc; 1] = [PadDesc { name: "src", offers: &OFFERS }];

// Boxed once at registry or parse time.
#[allow(clippy::disallowed_methods)]
pub(crate) static DESC: ElementDesc = ElementDesc {
    name: "oggflacdeframe",
    sinks: &SINKS,
    srcs: &SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(OggFlacDeframe::new())),
    io: false,
    couplings: &[],
};

// The lookup matches `flac/ogg`, the link solves on `bytes`, all an Ogg demuxer pad offers.
static DEC_SINK_OFFERS: [OfferDesc; 2] =
    [OfferDesc { family: "flac/ogg", fields: &DELAY_FIELDS }, OfferDesc::any("bytes")];
static DEC_SINKS: [PadDesc; 1] = [PadDesc { name: "sink", offers: &DEC_SINK_OFFERS }];

// Boxed once at registry or parse time.
#[allow(clippy::disallowed_methods)]
pub(crate) static DEC_DESC: ElementDesc = ElementDesc {
    name: "oggflacdec",
    sinks: &DEC_SINKS,
    srcs: &flacdec::SRCS,
    props: &[],
    sched: SchedHint::Passive,
    inputs: InputPolicy::Single,
    latency: LatencyDesc::NONE,
    make_default: Some(|| Box::new(OggFlacDec::new())),
    io: false,
    couplings: &[],
};

/// Whether `packet` is the mapping packet that opens a stream.
pub(crate) fn is_mapping(packet: &[u8]) -> bool {
    packet.first() == Some(&MAPPING_PACKET_TYPE) && packet.get(1..5) == Some(&MAPPING_SIGNATURE[..])
}

/// Whether `packet` opens with a frame sync code, which no metadata block header byte does.
pub(crate) fn is_frame(packet: &[u8]) -> bool {
    packet.len() >= 2 && packet[0] == 0xFF && packet[1] & 0xFE == 0xF8
}

/// The native FLAC bytes of the first packet, past its mapping prefix.
pub(crate) fn native<'a>(ctx: &Ctx, packet: &'a [u8]) -> Result<&'a [u8], Error> {
    let native = OggFlacDeframe::strip_mapping_header(packet)
        .map_err(|e| Error::element_failed(ctx.element(), e))?;
    // Only mapping 1.0 exists, a later major is passed on unchecked.
    let level = if packet[5] == 1 { Level::Debug } else { Level::Warn };
    log!(ctx, level, "mapping", major = packet[5], minor = packet[6]);
    Ok(native)
}

#[derive(Default)]
pub struct OggFlacDeframe {
    /// The strip applies to the first packet only, so a split later packet is forwarded whole.
    seen_header: bool,
}

impl OggFlacDeframe {
    pub const IN: SinkPad = SinkPad(0);
    pub const OUT: SrcPad = SrcPad(0);

    pub fn new() -> Self {
        Self::default()
    }

    fn strip_mapping_header(data: &[u8]) -> Result<&[u8], FlacError> {
        if data.len() < MAPPING_PREFIX_LEN {
            return Err(FlacError::MappingHeaderShort {
                len: data.len(),
                need: MAPPING_PREFIX_LEN,
            });
        }
        if data[0] != MAPPING_PACKET_TYPE || &data[1..5] != MAPPING_SIGNATURE {
            return Err(FlacError::MappingSignature);
        }
        // Version and the advisory packet count are not enforced, only mapping v1 exists.
        Ok(&data[MAPPING_PREFIX_LEN..])
    }

    fn emit(ctx: &mut Ctx, bytes: &[u8]) {
        let mut off = 0;
        while off < bytes.len() {
            let mut buf = ctx.alloc();
            let cap = buf.memory.capacity();
            debug_assert!(cap > 0, "oggflacdeframe: zero-capacity pool slot");
            let n = cap.min(bytes.len() - off);
            buf.memory.as_mut_full()[..n].copy_from_slice(&bytes[off..off + n]);
            buf.memory.set_len(n);
            ctx.out(Self::OUT).push(buf);
            off += n;
        }
    }
}

impl Element for OggFlacDeframe {
    fn desc(&self) -> &'static ElementDesc {
        &DESC
    }

    fn start(&mut self, _ctx: &mut Ctx) -> Result<(), Error> {
        self.seen_header = false;
        Ok(())
    }

    fn process(&mut self, ctx: &mut Ctx, mut inputs: Inputs<'_>) -> Result<Flow, Error> {
        while let Some(buf) = inputs.pop() {
            if self.seen_header {
                Self::emit(ctx, buf.memory.data());
            } else {
                let native = native(ctx, buf.memory.data())?;
                Self::emit(ctx, native);
                self.seen_header = true;
            }
        }
        Ok(Flow::Ok)
    }

    fn event(&mut self, _ctx: &mut Ctx, _event: &Event) -> Result<(), Error> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut Ctx) {
        self.seen_header = false;
    }
}

/// `oggflacdeframe ! flacdec` in one element, the decoder the `flac/ogg` lookup returns.
pub struct OggFlacDec(FlacDec);

impl Default for OggFlacDec {
    fn default() -> Self {
        Self(FlacDec::ogg())
    }
}

impl OggFlacDec {
    pub const IN: SinkPad = FlacDec::IN;
    pub const OUT: SrcPad = FlacDec::OUT;

    pub fn new() -> Self {
        Self::default()
    }
}

impl Element for OggFlacDec {
    fn desc(&self) -> &'static ElementDesc {
        &DEC_DESC
    }

    fn start(&mut self, ctx: &mut Ctx) -> Result<(), Error> {
        self.0.start(ctx)
    }

    fn process(&mut self, ctx: &mut Ctx, inputs: Inputs<'_>) -> Result<Flow, Error> {
        self.0.process(ctx, inputs)
    }

    fn event(&mut self, ctx: &mut Ctx, event: &Event) -> Result<(), Error> {
        self.0.event(ctx, event)
    }

    fn stop(&mut self, ctx: &mut Ctx) {
        self.0.stop(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_nine_byte_mapping_prefix() {
        let mut pkt = vec![0x7F];
        pkt.extend_from_slice(b"FLAC");
        pkt.extend_from_slice(&[0x01, 0x00]); // major.minor
        pkt.extend_from_slice(&0u16.to_be_bytes()); // header packet count
        pkt.extend_from_slice(b"fLaCnative-streaminfo-bytes");
        let native = OggFlacDeframe::strip_mapping_header(&pkt).expect("valid header");
        assert_eq!(native, b"fLaCnative-streaminfo-bytes");
    }

    #[test]
    fn rejects_a_short_first_packet() {
        let err = OggFlacDeframe::strip_mapping_header(&[0x7F, b'F', b'L']);
        assert_eq!(
            err,
            Err(FlacError::MappingHeaderShort { len: 3, need: 9 }),
            "short first packet must error, not panic"
        );
    }

    #[test]
    fn rejects_a_bad_signature() {
        let bogus = [0u8; 16];
        let err = OggFlacDeframe::strip_mapping_header(&bogus);
        assert_eq!(err, Err(FlacError::MappingSignature), "missing 0x7F \"FLAC\" must error");
    }
}
