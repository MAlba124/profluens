//! `mkvdemux` made as the player makes it.

// Head copied once per input.
#![allow(clippy::disallowed_methods)]

use pf_mkv::MkvDemux;
use pf_mkv::encoding::Step;
use pf_mkv::reader::MatroskaReader;

use crate::rig::{Bounds, Made, Target};

pub const TARGET: Target = Target {
    make,
    bounds: Bounds {
        heap_per_byte: 192,
        heap_per_head: 8,
        heap_fixed: 0,
        out_per_byte: 3,
        out_per_head: 4,
    },
};

/// The head is everything before the first Cluster, at most 8 MiB (lib/play/src/head.rs).
fn make(file: &[u8], _variant: u8) -> Option<Made> {
    let cluster = file.windows(4).position(|w| w == pf_mkv::ebml::id::CLUSTER);
    let end = cluster.unwrap_or(file.len()).min(8 << 20);
    let element = Box::new(MkvDemux::new(file[..end].to_vec()));
    let out_scale = expansion(&file[..end]);
    Some(Made { element, sink: MkvDemux::IN, head_len: end, statics: &[], out_scale })
}

/// Deflate's ceiling, a 258 octet match costs a little over 2 bits (RFC 1951 3.2.5).
const DEFLATE_MAX_RATIO: usize = 1032;

/// How far a track of the head may expand its frames. zlib multiplies by its ratio, header
/// stripping adds its prefix to each frame, and a laced frame costs an input octet at least.
fn expansion(head: &[u8]) -> usize {
    let mut reader = MatroskaReader::new();
    let _ = reader.push(head);
    let coded = reader.tracks().iter().filter(|t| t.coding.transforms());
    let scale = |steps: &[Step]| {
        steps.iter().fold(1usize, |n, step| match step {
            Step::Zlib => n.saturating_mul(DEFLATE_MAX_RATIO),
            Step::Strip(prefix) => n.saturating_add(prefix.len()),
        })
    };
    coded.map(|t| scale(&t.coding.frames)).max().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_mkv::ebml::{self, id};

    fn el(out: &mut Vec<u8>, i: &[u8], body: &[u8]) {
        ebml::write_id(out, i);
        ebml::write_size(out, body.len() as u64);
        out.extend_from_slice(body);
    }

    /// The reviewer's input, a strip track of `prefix` then blocks of 256 empty laced frames.
    fn strip_laced(prefix: usize) -> Vec<u8> {
        let mut comp = Vec::new();
        ebml::write_uint(&mut comp, id::CONTENT_COMP_ALGO, 3);
        el(&mut comp, id::CONTENT_COMP_SETTINGS, &vec![0; prefix]);
        let mut enc = Vec::new();
        ebml::write_uint(&mut enc, id::CONTENT_ENCODING_ORDER, 0);
        ebml::write_uint(&mut enc, id::CONTENT_ENCODING_SCOPE, 1);
        ebml::write_uint(&mut enc, id::CONTENT_ENCODING_TYPE, 0);
        el(&mut enc, id::CONTENT_COMPRESSION, &comp);
        let mut encs = Vec::new();
        el(&mut encs, id::CONTENT_ENCODING, &enc);
        let mut te = Vec::new();
        ebml::write_uint(&mut te, id::TRACK_NUMBER, 1);
        ebml::write_uint(&mut te, id::TRACK_TYPE, 1);
        ebml::write_string(&mut te, id::CODEC_ID, "V_VP9");
        el(&mut te, id::CONTENT_ENCODINGS, &encs);
        let mut tracks = Vec::new();
        el(&mut tracks, id::TRACK_ENTRY, &te);
        let mut f = Vec::new();
        let mut head = Vec::new();
        ebml::write_string(&mut head, id::DOC_TYPE, "matroska");
        el(&mut f, id::EBML, &head);
        ebml::write_id(&mut f, id::SEGMENT);
        ebml::write_unknown_size(&mut f);
        el(&mut f, id::TRACKS, &tracks);
        ebml::write_id(&mut f, id::CLUSTER);
        ebml::write_unknown_size(&mut f);
        ebml::write_uint(&mut f, id::TIMESTAMP, 0);
        let mut block = vec![0x81, 0, 0, 0x82, 0xFF];
        block.extend([0; 255]);
        for _ in 0..16 {
            el(&mut f, id::SIMPLE_BLOCK, &block);
        }
        f
    }

    #[test]
    fn strip_counts_as_expanding_and_a_capped_prefix_stays_in_bound() {
        let cap = pf_mkv::encoding::MAX_STRIP;
        assert_eq!(expansion(&strip_laced(cap)), cap + 1);
        assert_eq!(expansion(&strip_laced(1 << 20)), 1, "a refused track passes as stored");
        // sb_18 and sb_19, pushed and pulled, and the same at the cap
        for (control, prefix) in [(0x18, 1 << 20), (0x19, 1 << 20), (0x18, cap), (0x19, cap)] {
            let plan = crate::Plan::decode(&[control, 5]);
            let ran = crate::run(&plan, &strip_laced(prefix), &TARGET);
            assert!(ran.is_ok(), "{control:#x} prefix {prefix}: {}", ran.err().unwrap());
        }
    }
}
