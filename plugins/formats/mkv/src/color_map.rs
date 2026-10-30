//! The `Colour` element's H.273 code points to and from the pipeline's colour (RFC 9559 section
//! 5.1.4.1.31).

use profluens_video::color::{ChromaSite, Matrix, Primaries, Range, StreamColor, TransferFn};

use crate::reader::Track;

pub(crate) fn colour_of(track: &Track) -> StreamColor {
    StreamColor {
        matrix: Matrix::from_h273(track.colour_matrix),
        range: Range::from_mkv(track.colour_range),
        transfer: TransferFn::from_h273(track.colour_transfer),
        primaries: Primaries::from_h273(track.colour_primaries),
        hdr: track.hdr,
        chroma: ChromaSite::from_mkv(track.chroma_siting.0, track.chroma_siting.1),
        dovi: track.dovi,
    }
}

pub(crate) fn colour_from_format(
    ctx: &profluens_core::ctx::Ctx,
    f: &profluens_core::format::FixedFormat,
) -> Option<crate::writer::ColourConfig> {
    let c = StreamColor::read(ctx, f);
    let dflt = crate::writer::ColourConfig::default();
    let cfg = crate::writer::ColourConfig {
        matrix: c.matrix.map_or(dflt.matrix, Matrix::to_h273),
        range: c.range.map_or(dflt.range, Range::to_mkv),
        transfer: c.transfer.map_or(dflt.transfer, TransferFn::to_h273),
        primaries: c.primaries.map_or(dflt.primaries, Primaries::to_h273),
        hdr: c.hdr,
        chroma_siting: c.chroma.and_then(ChromaSite::to_mkv).unwrap_or(dflt.chroma_siting),
        dovi: c.dovi,
    };
    (!cfg.is_empty()).then_some(cfg)
}
