//! End-to-end scans against generated fixtures. The decorator reactors perturb only what the
//! `Reactor` contract leaves free, completion order and short reads.

#![allow(clippy::disallowed_methods)] // test setup: fixture writing and result collection

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pf_reactor::OsReactor;
use pf_tags::{Format, ScanConfig, ScanError, Scanner, fixture};
use profluens_core::id::ElementId;
use profluens_core::io::{Completion, IoResult, Reactor, ReactorFactory, Submission};
use profluens_core::memory::Pool;

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let d = fixture::scratch_dir(tag);
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                std::fs::remove_file(e.path()).ok();
            }
        }
        Dir(d)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[derive(Clone, PartialEq, Debug)]
struct Row {
    name: String,
    format: Format,
    duration_ns: Option<u64>,
    exact: bool,
    rate: Option<u32>,
    channels: Option<u32>,
    tags: Vec<(String, String)>,
    pictures: Vec<(String, usize)>,
    error: Option<ScanError>,
}

fn collect(scanner: &mut Scanner, paths: &[PathBuf]) -> Vec<Row> {
    let mut rows = Vec::new();
    scanner.scan(paths.iter().map(|p| p.as_path()), |out| {
        let name = out.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        rows.push(match out.result {
            Ok(r) => Row {
                name,
                format: r.format,
                duration_ns: r.props.duration_ns,
                exact: r.props.duration_exact,
                rate: r.props.sample_rate,
                channels: r.props.channels,
                tags: r.tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                pictures: r
                    .tags
                    .pictures()
                    .iter()
                    .map(|p| (p.mime.to_string(), p.data.len()))
                    .collect(),
                error: None,
            },
            Err(e) => Row {
                name,
                format: Format::Unknown,
                duration_ns: None,
                exact: false,
                rate: None,
                channels: None,
                tags: Vec::new(),
                pictures: Vec::new(),
                error: Some(e),
            },
        });
    });
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

fn row<'a>(rows: &'a [Row], name: &str) -> &'a Row {
    rows.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row for {name}"))
}

fn tag<'a>(r: &'a Row, key: &str) -> Option<&'a str> {
    r.tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn small_pool() -> Pool {
    Pool::bounded(4096, 64)
}

struct ShortReadReactor(OsReactor);

impl Reactor for ShortReadReactor {
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]) {
        self.0.submit(subs, names)
    }
    fn due_in(&self) -> Option<std::time::Duration> {
        self.0.due_in()
    }
    fn is_idle(&self) -> bool {
        self.0.is_idle()
    }
    fn park(&mut self, timeout: std::time::Duration) -> bool {
        self.0.park(timeout)
    }
    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        self.0.run_once(out);
        for (_, c) in out.iter_mut() {
            if let IoResult::Ok(n) = c.result
                && n > 1
            {
                let half = n / 2;
                if let Some(buf) = c.buf.as_mut() {
                    buf.memory.set_len(half);
                    c.result = IoResult::Ok(half);
                }
            }
        }
    }
}

struct ReverseReactor(OsReactor);

impl Reactor for ReverseReactor {
    fn submit(&mut self, subs: &mut Vec<Submission>, names: &[u8]) {
        self.0.submit(subs, names)
    }
    fn due_in(&self) -> Option<std::time::Duration> {
        self.0.due_in()
    }
    fn is_idle(&self) -> bool {
        self.0.is_idle()
    }
    fn park(&mut self, timeout: std::time::Duration) -> bool {
        self.0.park(timeout)
    }
    fn run_once(&mut self, out: &mut Vec<(ElementId, Completion)>) {
        self.0.run_once(out);
        out.reverse();
    }
}

fn short_read_factory() -> ReactorFactory {
    Arc::new(|| Ok(Box::new(ShortReadReactor(OsReactor::new()?)) as Box<dyn Reactor>))
}

fn reverse_factory() -> ReactorFactory {
    Arc::new(|| Ok(Box::new(ReverseReactor(OsReactor::new()?)) as Box<dyn Reactor>))
}

fn build(dir: &Dir) -> Vec<PathBuf> {
    let info = [
        ("INAM", "Inline Title"),
        ("IART", "Inline Artist"),
        ("IPRD", "Inline Album"),
        ("ITRK", "7"),
        ("IGNR", "Ambient"),
        ("ICRD", "2015"),
        ("ICMT", "a comment"),
    ];
    vec![
        dir.write("a_inline.wav", &fixture::wav(44_100, 2, 16, 44_100, &info, false)),
        dir.write("b_tail.wav", &fixture::wav(44_100, 2, 16, 44_100, &info, true)),
        dir.write(
            "c_plain.flac",
            &fixture::flac(
                48_000,
                2,
                24,
                96_000,
                &["TITLE=Plain", "ARTIST=Someone", "ALBUM=Record", "TRACKNUMBER=3"],
                None,
            ),
        ),
        dir.write(
            "d_art.flac",
            &fixture::flac(
                44_100,
                1,
                16,
                22_050,
                &["TITLE=Art"],
                Some(("image/jpeg", &vec![0x5Au8; 20_000])),
            ),
        ),
        dir.write("e_notaudio.txt", b"this is not audio, it is a text file\n"),
        dir.write("f_empty.bin", b""),
        dir.write(
            "g_xing.mp3",
            &fixture::mp3(
                44_100,
                128,
                40,
                &[
                    ("TIT2", "Head Title"),
                    ("TPE1", "Head Artist"),
                    ("TALB", "Head Album"),
                    ("TRCK", "4"),
                    ("TXXX", "replaygain_track_gain=-7.25 dB"),
                ],
                None,
                true,
                None,
                &[],
            ),
        ),
        dir.write(
            "h_trailers.mp3",
            &fixture::mp3(
                44_100,
                128,
                30,
                &[],
                None,
                false,
                Some(("Tail Title", "Tail Artist", "Tail Album", "1998", 9)),
                &[("REPLAYGAIN_TRACK_GAIN", "-3.50 dB"), ("ALBUMARTIST", "Tail Band")],
            ),
        ),
        dir.write(
            "i_art.mp3",
            &fixture::mp3(
                44_100,
                128,
                12,
                &[("TIT2", "Art")],
                Some(("image/jpeg", &vec![0x6Bu8; 20_000])),
                false,
                None,
                &[],
            ),
        ),
        dir.write(
            "j_faststart.m4a",
            &fixture::m4a(
                44_100,
                2,
                88_200,
                &[("©nam", "Fast Title"), ("©ART", "Fast Artist"), ("©alb", "Fast Album")],
                Some(3),
                Some(("image/png", &vec![0x89u8; 1_500])),
                true,
            ),
        ),
        dir.write(
            "k_tailmoov.m4a",
            &fixture::m4a(48_000, 1, 144_000, &[("©nam", "Tail Moov")], None, None, false),
        ),
        dir.write(
            "l_opus.opus",
            &fixture::ogg_opus(
                2,
                312,
                44_100,
                &["TITLE=Opus Title", "ARTIST=Opus Artist", "ALBUM=Opus Album"],
                None,
                48_000 * 5 + 312,
            ),
        ),
        dir.write(
            "m_vorbis.ogg",
            &fixture::ogg_vorbis(
                2,
                44_100,
                &["TITLE=Vorbis Title", "ARTIST=Vorbis Artist"],
                44_100 * 4,
            ),
        ),
        dir.write(
            "n_oggflac.oga",
            &fixture::ogg_flac(
                48_000,
                2,
                288_000,
                &["TITLE=Ogg FLAC", "ARTIST=Chain"],
                Some(("image/jpeg", &vec![0x3Du8; 8_000])),
            ),
        ),
        dir.write(
            "o_aiff.aiff",
            &fixture::aiff_id3(
                44_100,
                2,
                44_100,
                &[("NAME", "AIFF Text Title"), ("AUTH", "AIFF Author"), ("ANNO", "an aiff note")],
                &[("TIT2", "AIFF Tag Title"), ("TPE1", "AIFF Tag Artist"), ("TRCK", "5")],
                true,
            ),
        ),
        dir.write(
            "p_ape.ape",
            &[
                fixture::ape_file(3990, 44_100, 2, 73_728, 3, 4_644),
                vec![0x71u8; 8_000],
                fixture::ape_tag(&[
                    ("Title", "Ape Title"),
                    ("Artist", "Ape Artist"),
                    ("Album", "Ape Album"),
                    ("Track", "2"),
                ]),
                fixture::id3v1("Ape V1", "Ape V1 Artist", "Ape V1 Album", "1996", 2),
            ]
            .concat(),
        ),
        dir.write(
            "q_apeold.ape",
            &[
                fixture::ape_file_old(3950, 2000, 0, 44_100, 2, 2, 294_912),
                fixture::ape_tag(&[("Title", "Old Ape"), ("Year", "1999")]),
            ]
            .concat(),
        ),
        dir.write(
            "r_wavpack.wv",
            &[
                fixture::wavpack(
                    44_100,
                    2,
                    88_200,
                    &[fixture::wv_sub_block(0x0A, &vec![0x33u8; 8_000])],
                ),
                fixture::ape_tag(&[
                    ("Title", "WavPack Title"),
                    ("Artist", "WavPack Artist"),
                    ("Replaygain_Track_Gain", "-6.75 dB"),
                ]),
                fixture::id3v1("Wv V1", "Wv V1 Artist", "Wv V1 Album", "2004", 6),
            ]
            .concat(),
        ),
        dir.write(
            "s_mpc8.mpc",
            &[
                fixture::mpc_sv8(0, 2, 88_200, 0, &[]),
                vec![0x2Eu8; 8_000],
                fixture::ape_tag(&[
                    ("Title", "MPC8 Title"),
                    ("Artist", "MPC8 Artist"),
                    ("Album", "MPC8 Album"),
                ]),
            ]
            .concat(),
        ),
        dir.write(
            "t_mpc7.mpc",
            &[
                fixture::mpc_sv7(0, 100, true, 500),
                fixture::ape_tag(&[("Title", "MPC7 Title"), ("Track", "11")]),
            ]
            .concat(),
        ),
        dir.write(
            "u_speex.spx",
            &fixture::ogg_speex(
                16_000,
                1,
                &["TITLE=Speex Title", "ARTIST=Speex Artist"],
                None,
                16_000 * 3,
            ),
        ),
        dir.write(
            "v_front.webm",
            &fixture::mkv(
                48_000.0,
                2,
                3_000.0,
                &[("TITLE", "WebM Title"), ("ARTIST", "WebM Artist"), ("PART_NUMBER", "9")],
                None,
            ),
        ),
        dir.write(
            "w_trailing.mkv",
            &fixture::mkv_trailing(
                44_100.0,
                1,
                1_250.0,
                &[("TITLE", "Trailing Title"), ("ALBUM", "Trailing Album")],
                Some(("image/jpeg", &vec![0x7Eu8; 6_000])),
                192 * 1024,
            ),
        ),
    ]
}

fn expect_reference(rows: &[Row]) {
    assert_eq!(rows.len(), 23, "one row per path");

    let a = row(rows, "a_inline.wav");
    assert_eq!(a.format, Format::Wav);
    assert_eq!(a.duration_ns, Some(1_000_000_000));
    assert!(a.exact);
    assert_eq!(a.rate, Some(44_100));
    assert_eq!(a.channels, Some(2));
    assert_eq!(tag(a, "TITLE"), Some("Inline Title"));
    assert_eq!(tag(a, "ARTIST"), Some("Inline Artist"));
    assert_eq!(tag(a, "ALBUM"), Some("Inline Album"));
    assert_eq!(tag(a, "TRACKNUMBER"), Some("7"));
    assert_eq!(tag(a, "GENRE"), Some("Ambient"));
    assert_eq!(tag(a, "DATE"), Some("2015"));
    assert_eq!(tag(a, "COMMENT"), Some("a comment"));

    let b = row(rows, "b_tail.wav");
    assert_eq!(b.format, Format::Wav);
    assert_eq!(b.duration_ns, Some(1_000_000_000));
    assert_eq!(tag(b, "TITLE"), Some("Inline Title"), "INFO after `data` needs the tail read");
    assert_eq!(tag(b, "COMMENT"), Some("a comment"));

    let c = row(rows, "c_plain.flac");
    assert_eq!(c.format, Format::Flac);
    assert_eq!(c.duration_ns, Some(2_000_000_000));
    assert!(c.exact);
    assert_eq!(c.rate, Some(48_000));
    assert_eq!(c.channels, Some(2));
    assert_eq!(tag(c, "TITLE"), Some("Plain"));
    assert_eq!(tag(c, "TRACKNUMBER"), Some("3"));
    assert!(c.pictures.is_empty());

    let d = row(rows, "d_art.flac");
    assert_eq!(d.format, Format::Flac);
    assert_eq!(d.duration_ns, Some(500_000_000));
    assert_eq!(d.channels, Some(1));
    assert_eq!(tag(d, "TITLE"), Some("Art"));
    assert_eq!(d.pictures, vec![("image/jpeg".to_string(), 20_000)]);

    let e = row(rows, "e_notaudio.txt");
    assert_eq!(e.format, Format::Unknown);
    assert!(e.tags.is_empty());
    assert_eq!(e.error, None, "not being audio is not an error");

    let f = row(rows, "f_empty.bin");
    assert_eq!(f.format, Format::Unknown);
    assert_eq!(f.error, None);

    let g = row(rows, "g_xing.mp3");
    assert_eq!(g.format, Format::Mp3);
    assert_eq!(g.duration_ns, Some(40 * 1152 * 1_000_000_000 / 44_100));
    assert!(g.exact, "a Xing frame count is authoritative, not an estimate");
    assert_eq!(g.rate, Some(44_100));
    assert_eq!(g.channels, Some(2));
    assert_eq!(tag(g, "TITLE"), Some("Head Title"));
    assert_eq!(tag(g, "ARTIST"), Some("Head Artist"));
    assert_eq!(tag(g, "ALBUM"), Some("Head Album"));
    assert_eq!(tag(g, "TRACKNUMBER"), Some("4"));
    assert_eq!(tag(g, "REPLAYGAIN_TRACK_GAIN"), Some("-7.25 dB"), "a TXXX description is the key");

    let h = row(rows, "h_trailers.mp3");
    assert_eq!(h.format, Format::Mp3);
    assert_eq!(tag(h, "TITLE"), Some("Tail Title"), "ID3v1 with no ID3v2 in front of it");
    assert_eq!(tag(h, "ALBUM"), Some("Tail Album"));
    assert_eq!(tag(h, "DATE"), Some("1998"));
    assert_eq!(tag(h, "TRACKNUMBER"), Some("9"), "the ID3v1.1 track byte");
    assert_eq!(tag(h, "REPLAYGAIN_TRACK_GAIN"), Some("-3.50 dB"), "APEv2, in front of ID3v1");
    assert_eq!(tag(h, "ALBUMARTIST"), Some("Tail Band"));
    // CBR estimate over the audio only. Counting the APE and ID3v1 bytes would inflate it.
    assert_eq!(h.duration_ns, Some(30 * 417 * 8_000_000 / 128));
    assert!(!h.exact, "a CBR estimate must not claim to be exact");

    let i = row(rows, "i_art.mp3");
    assert_eq!(i.format, Format::Mp3);
    assert_eq!(tag(i, "TITLE"), Some("Art"));
    assert_eq!(i.pictures, vec![("image/jpeg".to_string(), 20_000)], "the APIC extent path");
    assert_eq!(i.rate, Some(44_100), "props still come from the audio behind the tag");

    let j = row(rows, "j_faststart.m4a");
    assert_eq!(j.format, Format::Mp4);
    assert_eq!(j.duration_ns, Some(2_000_000_000));
    assert!(j.exact);
    assert_eq!(j.rate, Some(44_100));
    assert_eq!(j.channels, Some(2));
    assert_eq!(tag(j, "TITLE"), Some("Fast Title"));
    assert_eq!(tag(j, "ARTIST"), Some("Fast Artist"));
    assert_eq!(tag(j, "ALBUM"), Some("Fast Album"));
    assert_eq!(tag(j, "TRACKNUMBER"), Some("3"));
    assert_eq!(j.pictures, vec![("image/png".to_string(), 1_500)]);

    let k = row(rows, "k_tailmoov.m4a");
    assert_eq!(k.format, Format::Mp4);
    assert_eq!(k.duration_ns, Some(3_000_000_000), "a trailing moov needs the second read");
    assert_eq!(k.rate, Some(48_000));
    assert_eq!(k.channels, Some(1));
    assert_eq!(tag(k, "TITLE"), Some("Tail Moov"));

    let l = row(rows, "l_opus.opus");
    assert_eq!(l.format, Format::OggOpus);
    assert_eq!(l.duration_ns, Some(5_000_000_000));
    assert!(l.exact);
    assert_eq!(l.rate, Some(48_000), "Opus decodes at 48 kHz whatever its input rate was");
    assert_eq!(l.channels, Some(2));
    assert_eq!(tag(l, "TITLE"), Some("Opus Title"));
    assert_eq!(tag(l, "ARTIST"), Some("Opus Artist"));
    assert_eq!(tag(l, "ALBUM"), Some("Opus Album"));

    let m = row(rows, "m_vorbis.ogg");
    assert_eq!(m.format, Format::OggVorbis);
    assert_eq!(m.duration_ns, Some(4_000_000_000));
    assert!(m.exact);
    assert_eq!(m.rate, Some(44_100));
    assert_eq!(m.channels, Some(2));
    assert_eq!(tag(m, "TITLE"), Some("Vorbis Title"));
    assert_eq!(tag(m, "ARTIST"), Some("Vorbis Artist"));

    let n = row(rows, "n_oggflac.oga");
    assert_eq!(n.format, Format::OggFlac);
    assert_eq!(n.duration_ns, Some(6_000_000_000));
    assert!(n.exact);
    assert_eq!(n.rate, Some(48_000));
    assert_eq!(n.channels, Some(2));
    assert_eq!(tag(n, "TITLE"), Some("Ogg FLAC"));
    assert_eq!(n.pictures, vec![("image/jpeg".to_string(), 8_000)]);

    let o = row(rows, "o_aiff.aiff");
    assert_eq!(o.format, Format::Aiff);
    assert_eq!(o.duration_ns, Some(1_000_000_000), "44100 frames at 44.1 kHz");
    assert!(o.exact, "a declared sample-frame count, not an estimate");
    assert_eq!(o.rate, Some(44_100), "the 80-bit extended `COMM` rate");
    assert_eq!(o.channels, Some(2));
    assert_eq!(tag(o, "TITLE"), Some("AIFF Tag Title"));
    assert_eq!(tag(o, "ARTIST"), Some("AIFF Tag Artist"));
    assert_eq!(tag(o, "TRACKNUMBER"), Some("5"));
    assert_eq!(tag(o, "COMMENT"), Some("an aiff note"), "ANNO, which ID3 did not carry");

    let p = row(rows, "p_ape.ape");
    assert_eq!(p.format, Format::Ape);
    assert_eq!(p.duration_ns, Some(152_100 * 1_000_000_000 / 44_100));
    assert!(p.exact);
    assert_eq!(p.rate, Some(44_100));
    assert_eq!(p.channels, Some(2));
    assert_eq!(tag(p, "TITLE"), Some("Ape Title"), "APEv2 beats the ID3v1 behind it");
    assert_eq!(tag(p, "ARTIST"), Some("Ape Artist"));
    assert_eq!(tag(p, "TRACKNUMBER"), Some("2"), "`Track` is aliased to the canonical key");
    assert_eq!(tag(p, "DATE"), Some("1996"), "ID3v1 still fills what APEv2 lacked");

    let q = row(rows, "q_apeold.ape");
    assert_eq!(q.format, Format::Ape);
    assert_eq!(q.duration_ns, Some(589_824 * 1_000_000_000 / 44_100));
    assert!(q.exact);
    assert_eq!(tag(q, "TITLE"), Some("Old Ape"));
    assert_eq!(tag(q, "DATE"), Some("1999"), "`Year` is aliased to DATE");

    let r = row(rows, "r_wavpack.wv");
    assert_eq!(r.format, Format::WavPack);
    assert_eq!(r.duration_ns, Some(2_000_000_000), "88200 frames at rate index 9");
    assert!(r.exact);
    assert_eq!(r.rate, Some(44_100));
    assert_eq!(r.channels, Some(2), "the MONO_FLAG is clear");
    assert_eq!(tag(r, "TITLE"), Some("WavPack Title"));
    assert_eq!(tag(r, "REPLAYGAIN_TRACK_GAIN"), Some("-6.75 dB"));
    assert_eq!(tag(r, "DATE"), Some("2004"));

    let s = row(rows, "s_mpc8.mpc");
    assert_eq!(s.format, Format::Musepack);
    assert_eq!(s.duration_ns, Some(2_000_000_000), "88200 samples, no beginning silence");
    assert!(s.exact);
    assert_eq!(s.rate, Some(44_100));
    assert_eq!(s.channels, Some(2), "the SH channel field is stored biased by one");
    assert_eq!(tag(s, "TITLE"), Some("MPC8 Title"));
    assert_eq!(tag(s, "ALBUM"), Some("MPC8 Album"));

    let t = row(rows, "t_mpc7.mpc");
    assert_eq!(t.format, Format::Musepack);
    assert_eq!(t.duration_ns, Some((100 * 1152 - 652) * 1_000_000_000 / 44_100));
    assert!(t.exact, "TrueGapless puts the last frame's real length in the header");
    assert_eq!(t.channels, Some(2), "SV7 is always stereo");
    assert_eq!(tag(t, "TITLE"), Some("MPC7 Title"));
    assert_eq!(tag(t, "TRACKNUMBER"), Some("11"));

    let u = row(rows, "u_speex.spx");
    assert_eq!(u.format, Format::OggSpeex);
    assert_eq!(u.duration_ns, Some(3_000_000_000), "the last page's granule ÷ rate");
    assert!(!u.exact, "a Speex granule lacks the encoder lookahead");
    assert_eq!(u.rate, Some(16_000));
    assert_eq!(u.channels, Some(1));
    assert_eq!(tag(u, "TITLE"), Some("Speex Title"));
    assert_eq!(tag(u, "ARTIST"), Some("Speex Artist"));

    let v = row(rows, "v_front.webm");
    assert_eq!(v.format, Format::Mkv);
    assert_eq!(v.duration_ns, Some(3_000_000_000));
    assert!(v.exact, "a declared Duration, not an estimate");
    assert_eq!(v.rate, Some(48_000));
    assert_eq!(v.channels, Some(2));
    assert_eq!(tag(v, "TITLE"), Some("WebM Title"));
    assert_eq!(tag(v, "ARTIST"), Some("WebM Artist"));
    assert_eq!(tag(v, "TRACKNUMBER"), Some("9"), "PART_NUMBER, at the default track scope");
    assert!(v.pictures.is_empty());

    let w = row(rows, "w_trailing.mkv");
    assert_eq!(w.format, Format::Mkv);
    assert_eq!(w.duration_ns, Some(1_250_000_000));
    assert!(w.exact);
    assert_eq!(w.rate, Some(44_100));
    assert_eq!(w.channels, Some(1));
    assert_eq!(tag(w, "TITLE"), Some("Trailing Title"));
    assert_eq!(tag(w, "ALBUM"), Some("Trailing Album"));
    assert_eq!(w.pictures.len(), 1, "cover.jpg out of \\Segment\\Attachments");
    assert_eq!(w.pictures[0].0, "image/jpeg");
    assert_eq!(w.pictures[0].1, 6_000);
}

#[test]
fn reference_fixtures_sync_reactor() {
    let dir = Dir::new("reference");
    let paths = build(&dir);
    let mut sc = Scanner::with_pool(ScanConfig::default(), small_pool());
    expect_reference(&collect(&mut sc, &paths));
}

#[test]
fn short_reads_yield_identical_results() {
    let dir = Dir::new("shortread");
    let paths = build(&dir);
    let cfg = ScanConfig::default();

    let mut plain = Scanner::with_pool(cfg, small_pool());
    let expected = collect(&mut plain, &paths);

    let mut short = Scanner::with_reactor(cfg, small_pool(), &short_read_factory());
    let got = collect(&mut short, &paths);

    expect_reference(&got);
    assert_eq!(got, expected, "a reactor that halves every read must change nothing");
}

#[test]
fn reversed_completions_yield_identical_results() {
    let dir = Dir::new("reverse");
    let paths = build(&dir);
    let cfg = ScanConfig::default();

    let mut plain = Scanner::with_pool(cfg, small_pool());
    let expected = collect(&mut plain, &paths);

    let mut rev = Scanner::with_reactor(cfg, small_pool(), &reverse_factory());
    let got = collect(&mut rev, &paths);
    assert_eq!(got, expected, "completion order is not part of the contract");
}

#[test]
fn nonexistent_and_unreadable_paths_are_errors_not_panics() {
    let dir = Dir::new("missing");
    let good = dir.write("ok.wav", &fixture::wav(8_000, 1, 8, 800, &[("INAM", "x")], false));
    let paths = vec![dir.0.join("does_not_exist.flac"), good.clone(), dir.0.clone()];
    let mut sc = Scanner::with_pool(ScanConfig::default(), small_pool());
    let rows = collect(&mut sc, &paths);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        row(&rows, "does_not_exist.flac").error,
        Some(ScanError::Open(std::io::ErrorKind::NotFound))
    );
    assert!(row(&rows, "ok.wav").error.is_none());
    let dirname = dir.0.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(row(&rows, &dirname).error, Some(ScanError::Open(std::io::ErrorKind::InvalidInput)));
}

#[test]
fn truncated_files_never_panic() {
    let dir = Dir::new("truncated");
    let wav = fixture::wav(44_100, 2, 16, 4_000, &[("INAM", "T"), ("ICMT", "c")], true);
    let flac =
        fixture::flac(44_100, 2, 16, 44_100, &["TITLE=T"], Some(("image/png", &[9u8; 3000])));
    let mp3 = fixture::mp3(
        44_100,
        128,
        8,
        &[("TIT2", "T"), ("TXXX", "replaygain_track_gain=0.00 dB")],
        Some(("image/png", &[5u8; 2_000])),
        true,
        Some(("t", "a", "r", "2003", 2)),
        &[("REPLAYGAIN_ALBUM_GAIN", "1.00 dB")],
    );
    let m4a = fixture::m4a(
        44_100,
        2,
        44_100,
        &[("©nam", "T"), ("©ART", "A")],
        Some(1),
        Some(("image/jpeg", &[4u8; 1_200])),
        true,
    );
    let m4a_tail = fixture::m4a(48_000, 1, 48_000, &[("©nam", "T")], None, None, false);
    let opus =
        fixture::ogg_opus(2, 312, 48_000, &["TITLE=T"], Some(("image/png", &[6u8; 3_000])), 48_312);
    let vorbis = fixture::ogg_vorbis(1, 8_000, &["TITLE=T"], 8_000);
    let oggflac =
        fixture::ogg_flac(44_100, 2, 44_100, &["TITLE=T"], Some(("image/jpeg", &[8u8; 2_500])));
    let speex =
        fixture::ogg_speex(16_000, 1, &["TITLE=T"], Some(("image/png", &[3u8; 2_000])), 16_000);
    let aiff =
        fixture::aiff_id3(44_100, 2, 400, &[("NAME", "T")], &[("TIT2", "T"), ("TPE1", "A")], true);
    let ape = [
        fixture::ape_file(3990, 44_100, 2, 73_728, 3, 4_644),
        fixture::ape_tag(&[("Title", "T"), ("Replaygain_Track_Gain", "0.00 dB")]),
        fixture::id3v1("t", "a", "r", "2003", 2),
    ]
    .concat();
    let apeold = [
        fixture::ape_file_old(3950, 2000, 20, 44_100, 2, 2, 294_912),
        fixture::ape_tag(&[("Title", "T")]),
    ]
    .concat();
    let wavpack = [
        fixture::wavpack(44_100, 2, 88_200, &[fixture::wv_sub_block(0x0A, &[0x33u8; 600])]),
        fixture::ape_tag(&[("Title", "T")]),
        fixture::id3v1("t", "a", "r", "2004", 6),
    ]
    .concat();
    let mpc8 =
        [fixture::mpc_sv8(0, 2, 88_200, 0, &[]), fixture::ape_tag(&[("Title", "T")])].concat();
    let mpc7 = [fixture::mpc_sv7(0, 100, true, 500), fixture::ape_tag(&[("Title", "T")])].concat();
    let junk: Vec<u8> = (0..4096).map(|i| (i * 31 % 256) as u8).collect();

    let mut paths = Vec::new();
    for (tag, src) in [
        ("w", &wav),
        ("f", &flac),
        ("m", &mp3),
        ("p", &m4a),
        ("q", &m4a_tail),
        ("o", &opus),
        ("v", &vorbis),
        ("x", &oggflac),
        ("s", &speex),
        ("a", &aiff),
        ("e", &ape),
        ("d", &apeold),
        ("k", &wavpack),
        ("8", &mpc8),
        ("7", &mpc7),
        ("j", &junk),
    ] {
        for n in (0..src.len()).step_by(7).chain(0..64) {
            paths.push(dir.write(&format!("{tag}_{n}.bin"), &src[..n.min(src.len())]));
        }
    }
    let mut sc =
        Scanner::with_pool(ScanConfig { in_flight: 8, ..Default::default() }, small_pool());
    let rows = collect(&mut sc, &paths);
    assert_eq!(rows.len(), paths.len(), "every truncation reported exactly once");
    assert!(rows.iter().all(|r| r.error.is_none()), "truncation is not an IO error");
}

#[test]
fn hostile_headers_never_panic() {
    let dir = Dir::new("hostile");
    let mut paths = Vec::new();

    let mut w = b"RIFF\xff\xff\xff\xffWAVE".to_vec();
    w.extend_from_slice(b"fmt \x10\x00\x00\x00");
    w.extend_from_slice(&[1, 0, 2, 0, 0x44, 0xAC, 0, 0, 0x10, 0xB1, 2, 0, 4, 0, 16, 0]);
    w.extend_from_slice(b"data\xff\xff\xff\xff");
    w.extend_from_slice(b"LIST\xff\xff\xff\xffINFOINAM\xff\xff\xff\xffnope");
    paths.push(dir.write("lying.wav", &w));

    let mut f = b"fLaC".to_vec();
    f.extend_from_slice(&[0x00, 0xFF, 0xFF, 0xFF]);
    f.extend_from_slice(&[0u8; 32]);
    paths.push(dir.write("lying.flac", &f));

    let mut g = b"fLaC".to_vec();
    for _ in 0..500 {
        g.extend_from_slice(&[0x00, 0, 0, 0]);
    }
    paths.push(dir.write("chain.flac", &g));

    let mut m = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec();
    m.extend_from_slice(&[0u8; 64]);
    paths.push(dir.write("bigid3.mp3", &m));

    let mut ape = fixture::mp3_frames(44_100, 128, 2);
    ape.extend_from_slice(b"APETAGEX");
    ape.extend_from_slice(&2000u32.to_le_bytes());
    ape.extend_from_slice(&u32::MAX.to_le_bytes()); // size
    ape.extend_from_slice(&1_000_000u32.to_le_bytes()); // item count
    ape.extend_from_slice(&0u32.to_le_bytes());
    ape.extend_from_slice(&[0u8; 8]);
    paths.push(dir.write("lyingape.mp3", &ape));

    let mut mp4 = vec![0, 0, 0, 0x18];
    mp4.extend_from_slice(b"ftypM4A \x00\x00\x00\x00M4A mp42");
    mp4.extend_from_slice(b"\xff\xff\xff\xffmdat");
    paths.push(dir.write("lying.m4a", &mp4));

    let mut ogg = b"OggS\x00\x02".to_vec();
    ogg.extend_from_slice(&[0u8; 20]);
    ogg.push(255); // 255 lacing values
    ogg.extend_from_slice(&[255u8; 255]); // every one of them a continuation
    ogg.extend_from_slice(b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00");
    paths.push(dir.write("lying.opus", &ogg));

    let mut sc = Scanner::with_pool(ScanConfig::default(), small_pool());
    let rows = collect(&mut sc, &paths);
    assert_eq!(rows.len(), 7);
    assert_eq!(row(&rows, "lying.wav").format, Format::Wav);
    assert_eq!(row(&rows, "lying.flac").format, Format::Flac);
    assert_eq!(row(&rows, "bigid3.mp3").format, Format::Mp3);
    assert_eq!(row(&rows, "lyingape.mp3").format, Format::Mp3);
    assert_eq!(row(&rows, "lying.m4a").format, Format::Mp4);
    assert_eq!(row(&rows, "lying.opus").format, Format::OggOpus);
    assert!(rows.iter().all(|r| r.error.is_none()), "hostile input is not an IO error");
}

#[test]
fn many_small_files_recycle_slots() {
    let dir = Dir::new("recycle");
    let mut paths = Vec::new();
    for i in 0..137 {
        let title = format!("Track {i}");
        paths.push(dir.write(
            &format!("t{i:03}.wav"),
            &fixture::wav(8_000, 1, 8, 400 + i, &[("INAM", &title), ("ITRK", "1")], i % 2 == 0),
        ));
    }
    let cfg = ScanConfig { in_flight: 8, ..Default::default() };
    let mut sc = Scanner::with_pool(cfg, small_pool());
    let rows = collect(&mut sc, &paths);

    assert_eq!(rows.len(), 137);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.format, Format::Wav, "{}", r.name);
        assert_eq!(tag(r, "TITLE"), Some(format!("Track {i}").as_str()), "{}", r.name);
        assert_eq!(r.rate, Some(8_000));
        assert_eq!(r.duration_ns, Some((400 + i as u64) * 1_000_000_000 / 8_000));
    }

    let stats = sc.pool().stats();
    assert!(
        stats.slot_allocations < 64,
        "137 files should not allocate a slot each: {} allocations",
        stats.slot_allocations
    );
}

#[test]
fn scanning_twice_gives_the_same_answer() {
    let dir = Dir::new("twice");
    let paths = build(&dir);
    let mut sc = Scanner::with_pool(ScanConfig::default(), small_pool());
    let first = collect(&mut sc, &paths);
    let second = collect(&mut sc, &paths);
    assert_eq!(first, second, "a reused scanner must not carry state between scans");
}

#[test]
fn parallel_matches_single_threaded() {
    let dir = Dir::new("parallel");
    let mut paths = build(&dir);
    for i in 0..64 {
        let title = format!("P{i}");
        paths.push(dir.write(
            &format!("p{i:03}.wav"),
            &fixture::wav(44_100, 2, 16, 1_000 + i, &[("INAM", &title)], i % 3 == 0),
        ));
        paths.push(dir.write(
            &format!("q{i:03}.flac"),
            &fixture::flac(44_100, 2, 16, 44_100 + i as u64, &[&format!("TITLE=Q{i}")], None),
        ));
    }
    let cfg = ScanConfig::default();

    let mut single = Scanner::with_pool(cfg, small_pool());
    let expected = collect(&mut single, &paths);

    let seen = Mutex::new(Vec::new());
    let calls = AtomicUsize::new(0);
    pf_tags::scan_parallel(paths.clone(), 4, cfg, |out| {
        calls.fetch_add(1, Ordering::Relaxed);
        let name = out.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let entry = match out.result {
            Ok(r) => (
                name,
                r.format,
                r.props.duration_ns,
                r.tags.get("TITLE").map(str::to_string),
                r.tags.pictures().len(),
            ),
            Err(_) => (name, Format::Unknown, None, None, 0),
        };
        seen.lock().unwrap().push(entry);
    });

    assert_eq!(calls.load(Ordering::Relaxed), paths.len(), "one callback per path");
    let mut got = seen.into_inner().unwrap();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    let want: Vec<_> = expected
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.format,
                r.duration_ns,
                tag(r, "TITLE").map(str::to_string),
                r.pictures.len(),
            )
        })
        .collect();
    assert_eq!(got, want, "parallel results must match single-threaded, order aside");
}

/// Cover art already in the read buffer must not be copied, so the pool sees no second acquire.
#[test]
fn cover_art_inside_the_read_buffer_is_not_copied() {
    let dir = Dir::new("zerocopy");
    let art = vec![0x3Cu8; 64 * 1024]; // large, but the whole chain still fits one 128 KiB prefix
    let p = dir.write(
        "cover.flac",
        &fixture::flac(44_100, 2, 16, 44_100, &["TITLE=Cover"], Some(("image/jpeg", &art))),
    );

    let mut sc = Scanner::new(ScanConfig::default()); // default pool: 128 KiB slots
    let before = sc.pool().stats();
    let rows = collect(&mut sc, &[p]);
    let after = sc.pool().stats();

    let r = row(&rows, "cover.flac");
    assert_eq!(r.pictures, vec![("image/jpeg".to_string(), 64 * 1024)]);
    assert_eq!(
        after.acquires - before.acquires,
        1,
        "one buffer for the file's prefix, and nothing for the 64 KiB of art"
    );
    assert_eq!(after.slot_allocations, before.slot_allocations + 1, "one slot, first use");
}

#[test]
fn cover_art_larger_than_a_pool_slot() {
    let dir = Dir::new("bigart");
    let art = vec![0x7Eu8; 256 * 1024];
    let p = dir.write(
        "big.flac",
        &fixture::flac(
            44_100,
            2,
            16,
            441_000,
            &["TITLE=Big", "ALBUM=Heavy"],
            Some(("image/jpeg", &art)),
        ),
    );
    let mut sc = Scanner::new(ScanConfig::default());
    let rows = collect(&mut sc, &[p]);
    let r = row(&rows, "big.flac");
    assert_eq!(r.format, Format::Flac);
    assert_eq!(r.duration_ns, Some(10_000_000_000));
    assert_eq!(tag(r, "TITLE"), Some("Big"));
    assert_eq!(r.pictures, vec![("image/jpeg".to_string(), 256 * 1024)]);
}

#[test]
fn recognised_formats_without_tags_still_report() {
    let dir = Dir::new("stubs");
    let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
    mkv.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20]);
    let paths = vec![
        dir.write("s.mkv", &mkv),
        dir.write("s.m4a", &fixture::m4a(44_100, 2, 44_100, &[], None, None, true)),
        dir.write("s.opus", &fixture::ogg_opus(1, 312, 48_000, &[], None, 48_312)),
        dir.write("s.mp3", &fixture::mp3(44_100, 128, 4, &[], None, false, None, &[])),
        dir.write(
            "s.aac",
            &[[0xFFu8, 0xF1, 0x50, 0x80, 0x00, 0x1F, 0xFC].as_slice(), &[0u8; 200]].concat(),
        ),
    ];
    let mut sc = Scanner::with_pool(ScanConfig::default(), small_pool());
    let rows = collect(&mut sc, &paths);
    assert_eq!(row(&rows, "s.mkv").format, Format::Mkv);
    assert_eq!(row(&rows, "s.m4a").format, Format::Mp4);
    assert_eq!(row(&rows, "s.opus").format, Format::OggOpus);
    assert_eq!(row(&rows, "s.mp3").format, Format::Mp3);
    assert_eq!(row(&rows, "s.aac").format, Format::Unknown);
    assert!(rows.iter().all(|r| r.error.is_none() && r.tags.is_empty()));
    assert_eq!(row(&rows, "s.m4a").duration_ns, Some(1_000_000_000));
    assert_eq!(row(&rows, "s.opus").duration_ns, Some(1_000_000_000));
}

#[test]
fn opus_cover_art_spanning_the_default_prefix() {
    let dir = Dir::new("bigopusart");
    let art: Vec<u8> = (0..200 * 1024).map(|i| (i % 253) as u8).collect();
    let p = dir.write(
        "big.opus",
        &fixture::ogg_opus(
            2,
            312,
            48_000,
            &["TITLE=Spanning", "ARTIST=Pages"],
            Some(("image/jpeg", &art)),
            48_000 * 11 + 312,
        ),
    );
    let mut sc = Scanner::new(ScanConfig::default()); // default pool: 128 KiB slots
    let rows = collect(&mut sc, &[p]);
    let r = row(&rows, "big.opus");
    assert_eq!(r.format, Format::OggOpus);
    assert_eq!(r.duration_ns, Some(11_000_000_000));
    assert_eq!(tag(r, "TITLE"), Some("Spanning"));
    assert_eq!(tag(r, "ARTIST"), Some("Pages"));
    assert_eq!(r.pictures, vec![("image/jpeg".to_string(), 200 * 1024)]);
    assert_eq!(r.error, None);
}

#[test]
fn mp3_tag_precedence_across_all_three_side_cars() {
    let dir = Dir::new("mp3precedence");
    let p = dir.write(
        "all.mp3",
        &fixture::mp3(
            48_000,
            192,
            25,
            &[("TIT2", "From v2"), ("TPE1", "v2 Artist")],
            Some(("image/png", &vec![0x89u8; 900])),
            true,
            Some(("From v1", "v1 Artist", "v1 Album", "1977", 5)),
            &[("REPLAYGAIN_TRACK_GAIN", "-1.75 dB"), ("TITLE", "From APE")],
        ),
    );
    let mut sc = Scanner::with_pool(ScanConfig::default(), small_pool());
    let rows = collect(&mut sc, &[p]);
    let r = row(&rows, "all.mp3");
    assert_eq!(tag(r, "TITLE"), Some("From v2"), "ID3v2 outranks APEv2 and ID3v1");
    assert_eq!(tag(r, "ARTIST"), Some("v2 Artist"));
    assert_eq!(tag(r, "ALBUM"), Some("v1 Album"), "only ID3v1 had one");
    assert_eq!(tag(r, "REPLAYGAIN_TRACK_GAIN"), Some("-1.75 dB"), "only APEv2 had one");
    assert_eq!(tag(r, "TRACKNUMBER"), Some("5"));
    assert_eq!(r.pictures, vec![("image/png".to_string(), 900)]);
    assert_eq!(r.rate, Some(48_000));
    assert_eq!(r.duration_ns, Some(25 * 1152 * 1_000_000_000 / 48_000));
    assert!(r.exact);
}

/// An empty planned read must fall through to the parse, not strand the slot.
#[test]
fn a_zero_length_tail_still_completes_every_file() {
    let dir = Dir::new("zerotail");
    let paths = build(&dir);
    let cfg = ScanConfig { tail_len: 0, ..Default::default() };
    let mut sc = Scanner::with_pool(cfg, small_pool());
    let rows = collect(&mut sc, &paths);
    assert_eq!(rows.len(), paths.len(), "every path reported exactly once");
    assert!(rows.iter().all(|r| r.error.is_none()), "a zero tail is a config, not a failure");
    assert_eq!(tag(row(&rows, "g_xing.mp3"), "TITLE"), Some("Head Title"));
    assert_eq!(tag(row(&rows, "l_opus.opus"), "TITLE"), Some("Opus Title"));
    assert_eq!(row(&rows, "j_faststart.m4a").duration_ns, Some(2_000_000_000));
}

#[test]
fn in_flight_is_clamped_to_the_ring_bound() {
    let cfg = ScanConfig { in_flight: 4096, ..Default::default() };
    assert_eq!(cfg.slots(), pf_tags::MAX_IN_FLIGHT);
    let cfg = ScanConfig { in_flight: 0, ..Default::default() };
    assert_eq!(cfg.slots(), 1);

    let dir = Dir::new("serial");
    let paths = build(&dir);
    let mut sc =
        Scanner::with_pool(ScanConfig { in_flight: 0, ..Default::default() }, small_pool());
    expect_reference(&collect(&mut sc, &paths));
}
