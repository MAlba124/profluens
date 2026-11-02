//! The m3u8 texts (RFC 8216bis), written before any segment exists. Version 7 for the
//! `EXT-X-MAP` of fMP4, a variant per video rendition in the order given, audio as
//! renditions of one group, subtitles as WebVTT renditions of another. A player leaves out
//! the variants whose CODECS it cannot decode. Languages are BCP 47 tags, the caller maps a
//! container's ISO 639-2 codes to the tags a player names.

use crate::timeline::Timeline;
use std::fmt::{self, Write as _};

pub const AUDIO_GROUP: &str = "aud";
pub const SUBS_GROUP: &str = "subs";

/// A video variant of a master playlist.
pub struct Video<'a> {
    pub uri: &'a str,
    pub bandwidth: u64,
    pub codecs: &'a str,
    pub width: u16,
    pub height: u16,
    pub fps_milli: u32,
    /// `VIDEO-RANGE`, PQ or HLG, None for SDR.
    pub range: Option<&'static str>,
}

/// The `VIDEO-RANGE` of an H.273 transfer, None for SDR.
pub fn range(transfer: u8) -> Option<&'static str> {
    match transfer {
        16 => Some("PQ"),
        18 => Some("HLG"),
        _ => None,
    }
}

/// The I-frame variant, keyframes for scrubbing.
pub struct IFrame<'a> {
    pub uri: &'a str,
    pub bandwidth: u64,
    pub codecs: &'a str,
    pub width: u16,
    pub height: u16,
}

/// An audio rendition.
pub struct Audio<'a> {
    pub uri: &'a str,
    pub bandwidth: u64,
    pub codecs: &'a str,
    pub name: &'a str,
    /// A BCP 47 tag, empty or `und` for none.
    pub language: &'a str,
    pub channels: u8,
    pub default: bool,
}

/// A subtitle rendition, WebVTT.
pub struct Sub<'a> {
    pub uri: &'a str,
    pub name: &'a str,
    /// A BCP 47 tag, empty or `und` for none.
    pub language: &'a str,
    pub default: bool,
    /// Shown whatever the user picks, it carries what the audio leaves out.
    pub forced: bool,
}

/// A name or language as a quoted-string, quotes and line breaks are not allowed in one.
struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.split(['"', '\r', '\n']).try_for_each(|s| f.write_str(s))
    }
}

/// The `LANGUAGE` attribute of a tag, nothing for an empty one or `und`.
struct Lang<'a>(&'a str);

impl fmt::Display for Lang<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() || self.0.eq_ignore_ascii_case("und") {
            return Ok(());
        }
        write!(f, ",LANGUAGE=\"{}\"", Quoted(self.0))
    }
}

/// The `SUBTITLES` attribute of a variant, the line break and its URI.
fn variant_tail(out: &mut String, subs: bool, uri: &str) {
    if subs {
        let _ = write!(out, ",SUBTITLES=\"{SUBS_GROUP}\"");
    }
    let _ = writeln!(out, "\n{uri}");
}

/// The master playlist. Without video the default audio rendition is the variant.
pub fn master(
    out: &mut String,
    video: &[Video<'_>],
    audio: &[Audio<'_>],
    subs: &[Sub<'_>],
    iframe: Option<&IFrame<'_>>,
) {
    out.clear();
    out.push_str("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    for a in audio {
        let yes = if a.default { "YES" } else { "NO" };
        let _ = writeln!(
            out,
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{AUDIO_GROUP}\",NAME=\"{}\"{},DEFAULT={yes},AUTOSELECT=YES,CHANNELS=\"{}\",URI=\"{}\"",
            Quoted(a.name),
            Lang(a.language),
            a.channels.max(1),
            a.uri
        );
    }
    for t in subs {
        let yes = if t.default { "YES" } else { "NO" };
        let forced = if t.forced { "YES" } else { "NO" };
        let _ = writeln!(
            out,
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"{SUBS_GROUP}\",NAME=\"{}\"{},DEFAULT={yes},AUTOSELECT=YES,FORCED={forced},URI=\"{}\"",
            Quoted(t.name),
            Lang(t.language),
            t.uri
        );
    }
    let first = audio.iter().find(|a| a.default).or(audio.first());
    let (sep, audio_codecs, audio_bw) = first.map_or(("", "", 0), |a| (",", a.codecs, a.bandwidth));
    for v in video {
        let _ = write!(
            out,
            "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{}{sep}{audio_codecs}\",RESOLUTION={}x{}",
            v.bandwidth + audio_bw,
            v.codecs,
            v.width,
            v.height
        );
        if v.fps_milli > 0 {
            let _ = write!(out, ",FRAME-RATE={:.3}", f64::from(v.fps_milli) / 1000.0);
        }
        if let Some(r) = v.range {
            let _ = write!(out, ",VIDEO-RANGE={r}");
        }
        if first.is_some() {
            let _ = write!(out, ",AUDIO=\"{AUDIO_GROUP}\"");
        }
        variant_tail(out, !subs.is_empty(), v.uri);
    }
    if video.is_empty()
        && let Some(a) = first
    {
        let _ = write!(out, "#EXT-X-STREAM-INF:BANDWIDTH={},CODECS=\"{}\"", a.bandwidth, a.codecs);
        variant_tail(out, !subs.is_empty(), a.uri);
    }
    if let Some(i) = iframe {
        let _ = writeln!(
            out,
            "#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH={},CODECS=\"{}\",RESOLUTION={}x{},URI=\"{}\"",
            i.bandwidth.max(1),
            i.codecs,
            i.width,
            i.height,
            i.uri
        );
    }
}

/// The I-frame playlist of keyframes at `starts`, ns, each lasting to the next and the last
/// to `end`, segments `iframes/{n}.m4s` beside `iframes/init.mp4`.
pub fn iframes(out: &mut String, starts: &[i64], end: i64) {
    out.clear();
    let dur = |n: usize| starts.get(n + 1).copied().unwrap_or(end) - starts[n];
    let longest = (0..starts.len()).map(dur).max().unwrap_or(0);
    let _ = write!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-I-FRAMES-ONLY\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"iframes/init.mp4\"\n",
        ((longest + 999_999_999) / 1_000_000_000).max(1)
    );
    for n in 0..starts.len() {
        let _ = writeln!(out, "#EXTINF:{:.6},\niframes/{n}.m4s", dur(n).max(0) as f64 / 1e9);
    }
    out.push_str("#EXT-X-ENDLIST\n");
}

/// A rendition's media playlist, segments `{k}.m4s` beside `init.mp4`.
pub fn media(out: &mut String, tl: &Timeline) {
    out.clear();
    let _ = write!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4\"\n",
        tl.target_secs()
    );
    for k in 0..tl.len() {
        let _ = writeln!(out, "#EXTINF:{:.6},\n{k}.m4s", tl.duration(k) as f64 / 1e9);
    }
    out.push_str("#EXT-X-ENDLIST\n");
}

/// A subtitle rendition's media playlist, WebVTT segments `{k}.vtt` on the grid.
pub fn text(out: &mut String, tl: &Timeline) {
    out.clear();
    let _ = write!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n",
        tl.target_secs()
    );
    for k in 0..tl.len() {
        let _ = writeln!(out, "#EXTINF:{:.6},\n{k}.vtt", tl.duration(k) as f64 / 1e9);
    }
    out.push_str("#EXT-X-ENDLIST\n");
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn a_media_playlist_lists_every_segment_and_ends() {
        let tl = Timeline::frames(5_000_000_000, 25_000, 50).expect("grid");
        let mut s = String::new();
        media(&mut s, &tl);
        assert!(s.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n"), "{s}");
        assert!(s.contains("#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2.000000,\n0.m4s\n"));
        assert!(s.contains("#EXTINF:1.000000,\n2.m4s\n#EXT-X-ENDLIST\n"), "{s}");
    }

    #[test]
    fn the_master_names_video_and_the_default_audio_together() {
        let v = Video {
            uri: "0/index.m3u8",
            bandwidth: 4_000_000,
            codecs: "avc1.640028",
            width: 1920,
            height: 1080,
            fps_milli: 23_976,
            range: None,
        };
        let a = [
            Audio {
                uri: "1/index.m3u8",
                bandwidth: 128_000,
                codecs: "mp4a.40.2",
                name: "English",
                language: "en",
                channels: 2,
                default: true,
            },
            Audio {
                uri: "2/index.m3u8",
                bandwidth: 128_000,
                codecs: "opus",
                name: "Com\"ment",
                language: "",
                channels: 2,
                default: false,
            },
        ];
        let mut s = String::new();
        let f = Video {
            uri: "1/index.m3u8",
            bandwidth: 3_000_000,
            codecs: "vp09.00.40.08",
            width: 1280,
            height: 720,
            fps_milli: 0,
            range: Some("PQ"),
        };
        let i = IFrame {
            uri: "iframes.m3u8",
            bandwidth: 250_000,
            codecs: "hvc1.1.6.L120.90",
            width: 1920,
            height: 1080,
        };
        master(&mut s, &[f, v], &a, &[], Some(&i));
        assert!(
            s.ends_with("#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=250000,CODECS=\"hvc1.1.6.L120.90\",RESOLUTION=1920x1080,URI=\"iframes.m3u8\"\n"),
            "{s}"
        );
        assert!(s.contains("NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES"), "{s}");
        assert!(s.contains("NAME=\"Comment\",DEFAULT=NO"), "quotes dropped: {s}");
        assert!(s.contains("#EXT-X-STREAM-INF:BANDWIDTH=4128000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,FRAME-RATE=23.976,AUDIO=\"aud\"\n0/index.m3u8\n"), "{s}");
        let vp9 = "#EXT-X-STREAM-INF:BANDWIDTH=3128000,CODECS=\"vp09.00.40.08,mp4a.40.2\",RESOLUTION=1280x720,VIDEO-RANGE=PQ,AUDIO=\"aud\"\n1/index.m3u8\n";
        let order = s.find(vp9).zip(s.find("0/index.m3u8\n"));
        assert!(order.is_some_and(|(a, b)| a < b), "the variants in order: {s}");
    }

    #[test]
    fn an_iframe_playlist_lists_each_keyframe_to_the_next() {
        let ms = 1_000_000;
        let mut s = String::new();
        iframes(&mut s, &[0, 2_002 * ms, 9_500 * ms], 10_000 * ms);
        assert!(s.contains("#EXT-X-TARGETDURATION:8\n"), "the longest gap, rounded up: {s}");
        assert!(s.contains("#EXT-X-I-FRAMES-ONLY\n"));
        assert!(
            s.contains("#EXT-X-MAP:URI=\"iframes/init.mp4\"\n#EXTINF:2.002000,\niframes/0.m4s\n")
        );
        assert!(s.ends_with("#EXTINF:0.500000,\niframes/2.m4s\n#EXT-X-ENDLIST\n"), "{s}");
    }

    #[test]
    fn audio_alone_is_its_own_variant() {
        let a = [Audio {
            uri: "1/index.m3u8",
            bandwidth: 96_000,
            codecs: "mp4a.40.2",
            name: "a",
            language: "",
            channels: 1,
            default: true,
        }];
        let mut s = String::new();
        master(&mut s, &[], &a, &[], None);
        assert!(
            s.ends_with("#EXT-X-STREAM-INF:BANDWIDTH=96000,CODECS=\"mp4a.40.2\"\n1/index.m3u8\n"),
            "{s}"
        );
    }

    #[test]
    fn subtitles_are_a_group_every_variant_names() {
        let v = Video {
            uri: "0/index.m3u8",
            bandwidth: 4_000_000,
            codecs: "avc1.640028",
            width: 1920,
            height: 1080,
            fps_milli: 0,
            range: None,
        };
        let t = [
            Sub {
                uri: "2/index.m3u8",
                name: "English SDH",
                language: "en",
                default: true,
                forced: false,
            },
            Sub {
                uri: "3/index.m3u8",
                name: "Klingon",
                language: "tlh",
                default: false,
                forced: true,
            },
            Sub {
                uri: "4/index.m3u8",
                name: "unknown",
                language: "und",
                default: false,
                forced: false,
            },
        ];
        let mut s = String::new();
        master(&mut s, &[v], &[], &t, None);
        assert!(s.contains("#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English SDH\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"2/index.m3u8\"\n"), "{s}");
        assert!(
            s.contains("NAME=\"Klingon\",LANGUAGE=\"tlh\",DEFAULT=NO,AUTOSELECT=YES,FORCED=YES"),
            "a tag as it is: {s}"
        );
        assert!(s.contains("NAME=\"unknown\",DEFAULT=NO"), "und is no language: {s}");
        assert!(s.contains("RESOLUTION=1920x1080,SUBTITLES=\"subs\"\n0/index.m3u8\n"), "{s}");
    }

    #[test]
    fn a_subtitle_playlist_lists_vtt_segments_without_a_map() {
        let tl = Timeline::uniform(5_000_000_000, 2_000_000_000).expect("grid");
        let mut s = String::new();
        text(&mut s, &tl);
        assert!(!s.contains("EXT-X-MAP"), "{s}");
        assert!(s.contains("#EXTINF:2.000000,\n0.vtt\n"), "{s}");
        assert!(s.ends_with("#EXTINF:1.000000,\n2.vtt\n#EXT-X-ENDLIST\n"), "{s}");
    }
}
