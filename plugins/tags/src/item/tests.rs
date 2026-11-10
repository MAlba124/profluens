use super::*;
use crate::store::ArenaSink;
use profluens_core::memory::Pool;

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 2, 0, 0, 0]);
    b
}

fn jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
    b.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    b.extend([0xFF, 0xDB, 0, 4, 0, 0]);
    b.extend([0xFF, 0xC0, 0, 17, 8]);
    b.extend(h.to_be_bytes());
    b.extend(w.to_be_bytes());
    b.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    b.extend([0xFF, 0xD9]);
    b
}

fn none() -> std::iter::Empty<&'static [u8]> {
    std::iter::empty()
}

fn pair(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

/// A VORBIS_COMMENT block body.
fn vorbis(vendor: &str, entries: &[&str]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend((vendor.len() as u32).to_le_bytes());
    b.extend(vendor.as_bytes());
    b.extend((entries.len() as u32).to_le_bytes());
    for e in entries {
        b.extend((e.len() as u32).to_le_bytes());
        b.extend(e.as_bytes());
    }
    b
}

/// A PICTURE block body of `kind` holding `img`.
fn flac_picture(kind: u32, mime: &str, img: &[u8]) -> Vec<u8> {
    stated_picture(kind, mime, (0, 0), img)
}

/// A PICTURE block body stating `size`.
fn stated_picture(kind: u32, mime: &str, size: (u32, u32), img: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(kind.to_be_bytes());
    b.extend((mime.len() as u32).to_be_bytes());
    b.extend(mime.as_bytes());
    b.extend(0u32.to_be_bytes());
    b.extend(size.0.to_be_bytes());
    b.extend(size.1.to_be_bytes());
    b.extend([0u8; 8]);
    b.extend((img.len() as u32).to_be_bytes());
    b.extend(img);
    b
}

fn synchsafe(n: usize) -> [u8; 4] {
    [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]
}

/// An ID3v2.3 tag of `frames` with header flags `flags`.
fn id3v23(flags: u8, frames: &[u8]) -> Vec<u8> {
    let mut tag = b"ID3\x03\0".to_vec();
    tag.push(flags);
    tag.extend(synchsafe(frames.len()));
    tag.extend(frames);
    tag
}

fn frame23(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut f = id.to_vec();
    f.extend((body.len() as u32).to_be_bytes());
    f.extend([0, 0]);
    f.extend(body);
    f
}

fn apic(kind: u8, img: &[u8]) -> Vec<u8> {
    let mut body = vec![0u8];
    body.extend(b"image/png\0");
    body.push(kind);
    body.extend(b"pic\0");
    body.extend(img);
    frame23(b"APIC", &body)
}

fn webp(chunk: &[u8; 4], head: &[u8]) -> Vec<u8> {
    let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
    b.extend(chunk);
    b.extend((head.len() as u32).to_le_bytes());
    b.extend(head);
    b
}

#[test]
fn webp_is_sized_by_its_first_chunk() {
    let mut lossy = vec![0x30, 0x01, 0x00, 0x9D, 0x01, 0x2A];
    lossy.extend((640u16 | 0x4000).to_le_bytes());
    lossy.extend(480u16.to_le_bytes());
    assert_eq!(
        image_info(&webp(b"VP8 ", &lossy)),
        Some(("image/webp", 640, 480)),
        "scale bits off"
    );
    let v = (300u32 - 1) | (200 - 1) << 14 | 1 << 28;
    let mut lossless = vec![0x2F];
    lossless.extend(v.to_le_bytes());
    assert_eq!(image_info(&webp(b"VP8L", &lossless)), Some(("image/webp", 300, 200)));
    let mut ext = vec![0x10, 0, 0, 0];
    ext.extend(&(4000u32 - 1).to_le_bytes()[..3]);
    ext.extend(&(100_000u32 - 1).to_le_bytes()[..3]);
    assert_eq!(image_info(&webp(b"VP8X", &ext)), Some(("image/webp", 4000, 65_535)), "clamped");
    lossy[3] = 0;
    assert_eq!(image_info(&webp(b"VP8 ", &lossy)), Some(("image/webp", 0, 0)), "no start code");
    lossless[0] = 0;
    assert_eq!(image_info(&webp(b"VP8L", &lossless)), Some(("image/webp", 0, 0)), "no signature");
    let whole = webp(b"VP8X", &ext);
    for n in 12..whole.len() {
        let (_, w, _) = image_info(&whole[..n]).unwrap();
        assert_eq!(w, 0, "cut to {n} bytes");
    }
    assert_eq!(image_info(&webp(b"ALPH", &ext)), Some(("image/webp", 0, 0)));
}

/// A JPEG with no SOF, its size only where a container states it.
const JPEG_NO_SOF: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xD9];

#[test]
fn a_stated_size_stands_in_where_the_bytes_tell_none() {
    let stated = stated_picture(3, "image/jpeg", (600, 600), JPEG_NO_SOF);
    let p = &flac(none(), [stated.as_slice()]).pictures[0];
    assert_eq!((p.mime.as_str(), p.width, p.height), ("image/jpeg", 600, 600));
    let told = stated_picture(3, "image/png", (600, 600), &png(4, 3));
    let p = &flac(none(), [told.as_slice()]).pictures[0];
    assert_eq!((p.width, p.height), (4, 3), "the bytes win");
    let huge = stated_picture(3, "image/jpeg", (70_000, 9), JPEG_NO_SOF);
    let p = &flac(none(), [huge.as_slice()]).pictures[0];
    assert_eq!((p.width, p.height), (65_535, 9), "clamped");
    let entry = format!("METADATA_BLOCK_PICTURE={}", base64(&stated));
    let t = ogg(&vorbis("v", &[&entry]), &Arena::default());
    assert_eq!((t.pictures[0].width, t.pictures[0].height), (600, 600), "and in an Ogg comment");
}

#[test]
fn a_comment_that_is_no_utf8_keeps_its_title() {
    let mut vc = 0u32.to_le_bytes().to_vec();
    vc.extend(2u32.to_le_bytes());
    for e in [&b"TITLE=Caf\xE9"[..], b"ARTIST=Me"] {
        vc.extend((e.len() as u32).to_le_bytes());
        vc.extend(e);
    }
    let t = flac([vc.as_slice()], none());
    assert_eq!(t.tags, [pair("TITLE", "Caf\u{FFFD}"), pair("ARTIST", "Me")]);
    let t = ogg(&vc, &Arena::default());
    assert_eq!(t.tags, [pair("TITLE", "Caf\u{FFFD}"), pair("ARTIST", "Me")], "and in Ogg");
}

#[test]
fn images_are_sized_by_their_headers() {
    assert_eq!(image_info(&png(600, 500)), Some(("image/png", 600, 500)));
    assert_eq!(image_info(&png(100_000, 2)), Some(("image/png", 65_535, 2)), "clamped");
    assert_eq!(image_info(&jpeg(1400, 1200)), Some(("image/jpeg", 1400, 1200)));
    assert_eq!(image_info(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 1]), Some(("image/jpeg", 0, 0)), "no SOF");
    assert_eq!(image_info(b"RIFF\0\0\0\0WEBPVP8 "), Some(("image/webp", 0, 0)));
    assert_eq!(image_info(b"not an image"), None);
    let p = picture(3, "image/jpg", png(4, 3));
    assert_eq!((p.mime.as_str(), p.width, p.height), ("image/png", 4, 3), "typed by the bytes");
}

#[test]
fn only_jpeg_png_and_webp_are_pictures() {
    assert_eq!(image_info(b"GIF89a\x40\x01\xf0\x00"), None, "a GIF");
    assert_eq!(image_info(b"\x89PNG\0\0\0\0"), None, "half a PNG signature");
    assert_eq!(image_info(b"RIFF\0\0\0\0WAVEfmt "), None, "a RIFF that is no WebP");
    assert_eq!(image_info(&[0xFF, 0xD8]), None, "two bytes of a JPEG");
    assert!(image(3, b"<html>".to_vec()).is_none());
    assert!(image(3, Vec::new()).is_none());
    let mut big = png(1, 1);
    big.resize(MAX_PICTURE + 1, 0);
    assert!(image(3, big).is_none(), "past the cap");
    assert_eq!(
        image(3, png(2, 3)).map(|p| (p.mime, p.width, p.height)),
        Some(("image/png".to_string(), 2, 3))
    );
    let junk = picture(3, &"x".repeat(10_000), b"MZ\x90\0".to_vec());
    assert_eq!(junk.mime.len(), MAX_MIME, "a stated type is cut");
    let t = capped(Vec::new(), vec![junk, picture(3, "", jpeg(5, 5))]);
    assert_eq!(t.pictures.len(), 1, "what is no image is dropped");
    assert_eq!(t.pictures[0].mime, "image/jpeg");
    let mut c = Collect::default();
    c.picture("image/png", b"#!/bin/sh\n");
    c.picture("image/png", &png(1, 1));
    c.picture_typed(3, "image/png", b"#!/bin/sh\n");
    assert_eq!(c.pictures.len(), 1, "the sink refuses what is no image");
    assert_eq!(c.pictures[0].kind, KIND_UNKNOWN);
}

#[test]
fn long_text_is_cut_as_it_is_read() {
    let mut c = Collect::default();
    c.text("TITLE", &"y".repeat(1 << 20));
    c.text(&"K".repeat(MAX_KEY + 1), "v");
    assert_eq!(c.text.len(), 1, "a key past the cap is dropped");
    assert_eq!(c.text[0].1.len(), MAX_VALUE);
    let long = format!("ARTIST={}", "\u{e6}".repeat(1 << 16));
    let key = format!("{}=v", "K".repeat(MAX_KEY + 1));
    let vc = vorbis("", &[&long, &key]);
    let mut c = Collect::default();
    flacmeta::vorbis_comment_into(&vc, &mut c);
    assert_eq!(c.text.len(), 1);
    assert_eq!(c.text[0].1, "\u{e6}".repeat(MAX_VALUE / 2), "cut on a char boundary");
    assert_eq!(flac([vc.as_slice()], none()).get("ARTIST"), Some(c.text[0].1.as_str()));
}

#[test]
fn chapter_comments_are_chapters_not_tags() {
    let mut entries = vec!["TITLE=Book"];
    let many: Vec<String> =
        (0..MAX_TAGS * 4).map(|i| format!("CHAPTER{i:03}=00:00:{:02}.000", i % 60)).collect();
    entries.extend(many.iter().map(String::as_str));
    entries.extend(["chapter001name=One", "CHAPTER001URL=x", "CHAPTERS=kept", "ARTIST=Late"]);
    let t = flac([&vorbis("v", &entries)[..]], none());
    assert_eq!(t.tags, [pair("TITLE", "Book"), pair("ARTIST", "Late"), pair("CHAPTERS", "kept")]);
    let c = capped(vec![pair("CHAPTER002NAME", "x"), pair("TITLE", "y")], Vec::new());
    assert_eq!(c.tags, [pair("TITLE", "y")]);
}

#[test]
fn a_year_stands_in_for_a_missing_date() {
    let c = capped(vec![pair("TITLE", "x"), pair("YEAR", "2005")], Vec::new());
    assert_eq!(c.get("DATE"), Some("2005"));
    assert_eq!(c.get("YEAR"), None);
    let c = capped(vec![pair("YEAR", "2005"), pair("DATE", "2005-09-12")], Vec::new());
    assert_eq!(
        (c.get("DATE"), c.get("YEAR")),
        (Some("2005-09-12"), Some("2005")),
        "a DATE wins, the YEAR stays"
    );
}

#[test]
fn caps_order_cut_and_bound() {
    let mut text: Vec<(String, String)> =
        (0..100).map(|i| (format!("X{i}"), format!("v{i}"))).collect();
    text.push(pair("GENRE", "Ambient"));
    text.push(pair("TITLE", &"\u{e6}".repeat(700)));
    text.push(pair("ARTIST", "A"));
    text.push(pair("ARTIST", "A"));
    text.push(pair("ALBUM", "  "));
    let t = capped(text, Vec::new());
    assert_eq!(t.tags.len(), MAX_TAGS);
    assert_eq!(t.tags[0].0, "TITLE");
    assert_eq!(t.tags[0].1.len(), 1024, "cut on a char boundary, 512 two byte chars");
    assert_eq!(
        (t.tags[1].0.as_str(), t.tags[2].0.as_str()),
        ("ARTIST", "GENRE"),
        "repeats and blanks gone"
    );
    assert_eq!(t.tags[3].0, "X0", "then file order");
    let big: Vec<(String, String)> = (0..64).map(|i| (format!("K{i}"), "x".repeat(1024))).collect();
    let t = capped(big, Vec::new());
    assert!(
        t.text_len() <= MAX_TEXT && t.tags.len() == 31,
        "{} tags {} bytes",
        t.tags.len(),
        t.text_len()
    );
    assert_eq!(cut("abc", 2), "ab");
}

#[test]
fn pictures_are_bounded_and_covers_lead() {
    let p = |kind, n: usize| {
        let mut bytes = png(1, 1);
        bytes.resize(n, 0);
        ItemPicture { kind, bytes, ..ItemPicture::default() }
    };
    let mut pics: Vec<ItemPicture> = (0..10).map(|_| p(0, 10)).collect();
    pics.push(p(FRONT_COVER, 10));
    pics.push(p(FRONT_COVER, MAX_PICTURE + 1));
    pics.push(p(4, 0));
    let t = capped(Vec::new(), pics);
    assert_eq!(t.pictures.len(), MAX_PICTURES);
    assert_eq!(t.pictures[0].kind, FRONT_COVER);
    assert!(t.pictures.iter().all(|p| !p.bytes.is_empty() && p.bytes.len() <= MAX_PICTURE));
}

#[test]
fn the_sink_keeps_a_late_cover_and_bounds_each_kind() {
    let mut c = Collect::default();
    for _ in 0..20 {
        c.picture_typed(4, "", &png(1, 1));
    }
    c.picture("", &png(2, 2));
    for i in 0..10 {
        c.picture_typed(FRONT_COVER, "", &png(10 + i, 1));
    }
    assert_eq!(c.pictures.len(), 2 * MAX_PICTURES, "first of each, covers and the rest");
    let t = c.finish();
    assert_eq!(t.pictures.len(), MAX_PICTURES);
    assert!(t.pictures.iter().all(|p| p.kind == FRONT_COVER));
    assert_eq!((t.pictures[0].width, t.pictures[7].width), (10, 17), "covers in file order");
}

#[test]
fn apic_frames_give_their_type() {
    let mut frames = frame23(b"TIT2", b"\0Hello");
    frames.extend(apic(4, &png(2, 2)));
    let tag = id3v23(0, &frames);
    let scratch = Arena::default();
    let t = id3(&tag, &[], &[], &scratch);
    assert_eq!(t.tags, [pair("TITLE", "Hello")]);
    assert_eq!(t.pictures.len(), 1);
    assert_eq!((t.pictures[0].kind, t.pictures[0].width), (4, 2), "typed by the frame it lies in");
}

#[test]
fn an_unsynchronised_tag_keeps_its_picture_types_and_covers_lead() {
    // no 0xFF in the frames, unsynchronising leaves them as they are
    let mut frames = apic(4, &png(1, 1));
    frames.extend(apic(FRONT_COVER, &png(3, 3)));
    let t = id3(&id3v23(0x80, &frames), &[], &[], &Arena::default());
    let kinds: Vec<(u8, u16)> = t.pictures.iter().map(|p| (p.kind, p.width)).collect();
    assert_eq!(kinds, [(FRONT_COVER, 3), (4, 1)]);
}

fn v1(title: &str) -> Vec<u8> {
    let mut b = b"TAG".to_vec();
    let mut field = [0u8; 30];
    field[..title.len()].copy_from_slice(title.as_bytes());
    b.extend(field);
    b.resize(128, 0);
    b[127] = 255;
    b
}

#[test]
fn trailing_tags_stand_in_for_an_id3v2_tag_without_text() {
    let scratch = Arena::default();
    assert_eq!(id3(&[], &[], &v1("Old"), &scratch).get("TITLE"), Some("Old"));
    let both = id3v23(0, &frame23(b"TIT2", b"\0New"));
    assert_eq!(id3(&both, &[], &v1("Old"), &scratch).tags, [pair("TITLE", "New")]);
    let art_only = id3v23(0, &apic(FRONT_COVER, &png(1, 1)));
    let t = id3(&art_only, &[], &v1("Old"), &scratch);
    assert_eq!((t.get("TITLE"), t.pictures.len()), (Some("Old"), 1), "the ID3v2 picture stays");
}

#[test]
fn flac_blocks_read() {
    let vc = vorbis("ref", &["title=Set", "Artist=Me"]);
    let img = jpeg(30, 20);
    let pb = flac_picture(3, "image/jpeg", &img);
    let t = flac([vc.as_slice()], [pb.as_slice()]);
    assert_eq!(t.tags, [pair("TITLE", "Set"), pair("ARTIST", "Me")]);
    let p = &t.pictures[0];
    assert_eq!(
        (p.kind, p.mime.as_str(), p.width, p.height, p.bytes.len()),
        (3, "image/jpeg", 30, 20, img.len())
    );
    assert!(flac(none(), [&pb[..pb.len() - 1]]).pictures.is_empty(), "cut short");
    let text = flac_picture(3, "image/png", b"hello");
    assert!(flac(none(), [text.as_slice()]).pictures.is_empty(), "bytes that are no image");
    // a length past the block, never read
    let mut long = pb[..pb.len() - img.len() - 4].to_vec();
    long.extend(u32::MAX.to_be_bytes());
    assert!(flac(none(), [long.as_slice()]).pictures.is_empty());
    let link = flac_picture(3, "-->", b"http://example.com/a.jpg");
    assert!(flac(none(), [link.as_slice()]).pictures.is_empty(), "a link is no picture");
    let back = flac_picture(4, "image/png", &png(1, 1));
    let t = flac(none(), [back.as_slice(), pb.as_slice()]);
    assert_eq!((t.pictures[0].kind, t.pictures[1].kind), (3, 4), "covers lead");
}

/// mkvmerge's per track statistics are no tags a tv shows, the rest keep their keys upper.
#[test]
fn the_track_statistics_are_left_out() {
    let mut c = Collect::default();
    let mut t = MkvText(&mut c);
    for (k, v) in [
        ("title", "A film"),
        ("BPS", "8000000"),
        ("DURATION", "01:52:00"),
        ("NUMBER_OF_FRAMES", "160000"),
        ("_STATISTICS_TAGS", "BPS DURATION"),
        ("ARTIST", "Someone"),
    ] {
        t.text(k, v);
    }
    assert_eq!(c.text, [pair("TITLE", "A film"), pair("ARTIST", "Someone")]);
}

fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut b = id.to_vec();
    b.push(0x80 | body.len() as u8);
    b.extend(body);
    b
}

fn mkv_tags(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut tag = Vec::new();
    for (k, v) in pairs {
        let mut simple = el(&[0x45, 0xA3], k.as_bytes());
        simple.extend(el(&[0x44, 0x87], v.as_bytes()));
        tag.extend(el(&[0x67, 0xC8], &simple));
    }
    el(&[0x12, 0x54, 0xC3, 0x67], &el(&[0x73, 0x73], &tag))
}

fn attachment(mime: &str, data: Vec<u8>) -> pf_mkv::Attachment {
    pf_mkv::Attachment { media_type: mime.to_string(), data, ..pf_mkv::Attachment::default() }
}

#[test]
fn matroska_tags_and_attachments_read_all_or_nothing() {
    let meta = pf_mkv::walk::Meta {
        tags: mkv_tags(&[("title", "A film"), ("BPS", "1"), ("year", "1999")]),
        pictures: vec![
            attachment("image/jpeg", png(5, 4)),
            attachment("text/plain", b"x".to_vec()),
        ],
        error: None,
        chapters: Vec::new(),
    };
    let t = mkv(meta.clone());
    assert_eq!(t.tags, [pair("TITLE", "A film"), pair("DATE", "1999")]);
    assert_eq!(t.pictures.len(), 1);
    assert_eq!(
        (t.pictures[0].kind, t.pictures[0].mime.as_str(), t.pictures[0].width),
        (KIND_UNKNOWN, "image/png", 5),
        "typed by the bytes"
    );
    let broken =
        pf_mkv::walk::Meta { error: Some(pf_mkv::walk::WalkError::Malformed { at: 7 }), ..meta };
    assert_eq!(mkv(broken), ItemTags::default());
}

/// The Segment's title stands in for the TITLE no tag states, a broken walk keeps it.
#[test]
fn a_matroska_segment_title_is_the_title_no_tag_states() {
    let meta = |tags: &[(&str, &str)]| pf_mkv::walk::Meta {
        tags: mkv_tags(tags),
        ..pf_mkv::walk::Meta::default()
    };
    let t = mkv_titled(meta(&[("ARTIST", "Band")]), Some("Segment"));
    assert_eq!(t.tags, [pair("TITLE", "Segment"), pair("ARTIST", "Band")]);
    let t = mkv_titled(meta(&[("title", "Tagged"), ("ARTIST", "Band")]), Some("Segment"));
    assert_eq!(t.tags, [pair("TITLE", "Tagged"), pair("ARTIST", "Band")]);
    assert_eq!(mkv_titled(meta(&[]), None), ItemTags::default());
    let broken = pf_mkv::walk::Meta {
        error: Some(pf_mkv::walk::WalkError::Malformed { at: 7 }),
        ..meta(&[("TITLE", "Tagged")])
    };
    assert_eq!(mkv_titled(broken, Some("Segment")).tags, [pair("TITLE", "Segment")]);
}

/// INFO subchunks with and without their NUL, of odd size with the pad byte, Latin-1 text.
#[test]
fn riff_info_reads_each_subchunk() {
    let sub = |id: &[u8; 4], v: &[u8]| {
        let mut c = id.to_vec();
        c.extend((v.len() as u32).to_le_bytes());
        c.extend(v);
        if v.len() % 2 == 1 {
            c.push(0);
        }
        c
    };
    let body = [
        sub(b"INAM", b"abc"),
        sub(b"IART", b"Bj\xf6rk\0"),
        sub(b"ICRD", b"2020\0\0"),
        sub(b"IXYZ", b"unknown"),
        sub(b"IPRD", b"Al"),
        sub(b"ITRK", b"3\0"),
        sub(b"IGNR", b"\0"),
    ]
    .concat();
    let t = riff_info(&body);
    assert_eq!(
        t.tags,
        [
            pair("TITLE", "abc"),
            pair("ARTIST", "Bj\u{f6}rk"),
            pair("ALBUM", "Al"),
            pair("TRACKNUMBER", "3"),
            pair("DATE", "2020"),
        ]
    );
    assert_eq!(riff_info(&body[..body.len() - 13]).tags.len(), 4, "a cut subchunk");
    assert_eq!(riff_info(b""), ItemTags::default());
}

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend(kind);
    b.extend(body);
    b
}

fn ilst_item(kind: &[u8; 4], ty: u32, value: &[u8]) -> Vec<u8> {
    let mut data = ty.to_be_bytes().to_vec();
    data.extend(0u32.to_be_bytes());
    data.extend(value);
    bx(kind, &bx(b"data", &data))
}

#[test]
fn moov_tags_read() {
    let mut ilst = ilst_item(b"\xA9nam", 1, b"Song");
    ilst.extend(ilst_item(b"covr", 14, &png(7, 7)));
    let mut meta = vec![0u8; 4];
    let mut hdlr = vec![0u8; 4];
    hdlr.extend(b"mdirappl");
    hdlr.extend([0u8; 9]);
    meta.extend(bx(b"hdlr", &hdlr));
    meta.extend(bx(b"ilst", &ilst));
    let moov_box = bx(b"moov", &bx(b"udta", &bx(b"meta", &meta)));
    let t = moov(&moov_box, &Arena::default());
    assert_eq!(t.get("TITLE"), Some("Song"));
    assert_eq!(
        t.pictures.iter().map(|p| (p.kind, p.width)).collect::<Vec<_>>(),
        [(KIND_UNKNOWN, 7)]
    );
    assert_eq!(moov(b"not a moov", &Arena::default()), ItemTags::default());
}

fn base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        s.push(A[(n >> 18) as usize & 63] as char);
        s.push(A[(n >> 12) as usize & 63] as char);
        s.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    s
}

#[test]
fn ogg_comments_read_with_typed_pictures() {
    let back =
        format!("METADATA_BLOCK_PICTURE={}", base64(&flac_picture(4, "image/png", &png(1, 1))));
    let front =
        format!("METADATA_BLOCK_PICTURE={}", base64(&flac_picture(3, "image/png", &png(2, 2))));
    let body = vorbis("v", &["title=Night", &back, &front]);
    let t = ogg(&body, &Arena::default());
    assert_eq!(t.tags, [pair("TITLE", "Night")]);
    assert_eq!(t.pictures.iter().map(|p| (p.kind, p.width)).collect::<Vec<_>>(), [(3, 2), (4, 1)]);
}

#[test]
fn scanned_tags_are_capped_and_untyped() {
    let arena = Arena::default();
    let pool = Pool::bounded(4096, 8);
    let mut sink = ArenaSink::new(&arena, &pool, None);
    sink.text("TITLE", " Spaced ");
    sink.text("YEAR", "2001");
    sink.picture("image/jpeg", &png(9, 8));
    sink.picture("image/png", b"not an image");
    let t = scanned(&sink.finish());
    assert_eq!(t.tags, [pair("TITLE", "Spaced"), pair("DATE", "2001")]);
    assert_eq!(t.pictures.len(), 1);
    let p = &t.pictures[0];
    assert_eq!((p.kind, p.mime.as_str(), p.width, p.height), (KIND_UNKNOWN, "image/png", 9, 8));
}
