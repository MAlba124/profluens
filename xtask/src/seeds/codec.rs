//! Seeds of the codec targets. ffmpeg's elementary streams, cut into the units, NALs, OBUs or
//! frames each target reads, behind its knob byte. A missing encoder skips its streams.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Error;
use crate::util;

const V: &str = "testsrc2=size=160x96:rate=25:duration=0.4";
const A: &str = "sine=frequency=440:sample_rate=48000:duration=0.5";

/// The longest elementary stream a seed takes whole.
const WHOLE_MAX: usize = 256 << 10;

struct Es {
    video: bool,
    name: &'static str,
    args: &'static [&'static str],
}

const fn v(name: &'static str, args: &'static [&'static str]) -> Es {
    Es { video: true, name, args }
}

const fn a(name: &'static str, args: &'static [&'static str]) -> Es {
    Es { video: false, name, args }
}

const X265: &str = "log-level=none";

const STREAMS: &[Es] = &[
    v("base.h264", &["-c:v", "libx264", "-profile:v", "baseline", "-f", "h264"]),
    v("main.h264", &["-c:v", "libx264", "-profile:v", "main", "-bf", "2", "-f", "h264"]),
    v(
        "high.h264",
        &[
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-bf",
            "3",
            "-x264-params",
            "weightp=2:b-pyramid=normal:ref=4",
            "-f",
            "h264",
        ],
    ),
    v("interlaced.h264", &["-c:v", "libx264", "-flags", "+ildct+ilme", "-f", "h264"]),
    v(
        "high10.h264",
        &["-c:v", "libx264", "-profile:v", "high10", "-pix_fmt", "yuv420p10le", "-f", "h264"],
    ),
    v(
        "main.hevc",
        &["-c:v", "libx265", "-preset", "ultrafast", "-x265-params", X265, "-f", "hevc"],
    ),
    v(
        "main10.hevc",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            X265,
            "-f",
            "hevc",
        ],
    ),
    v(
        "bframes.hevc",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "medium",
            "-x265-params",
            "log-level=none:bframes=3:b-pyramid=1:weightp=1:weightb=1",
            "-f",
            "hevc",
        ],
    ),
    v(
        "scaling.hevc",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none:scaling-list=default",
            "-f",
            "hevc",
        ],
    ),
    v(
        "wpp_slices.hevc",
        &[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none:wpp=1:slices=2",
            "-f",
            "hevc",
        ],
    ),
    // x265 has no tiles, kvazaar does when ffmpeg has it.
    v("tiles.hevc", &["-c:v", "libkvazaar", "-kvazaar-params", "tiles=2x2", "-f", "hevc"]),
    v("svt.obu", &["-c:v", "libsvtav1", "-preset", "12", "-f", "obu"]),
    v("aom.obu", &["-c:v", "libaom-av1", "-cpu-used", "8", "-f", "obu"]),
    v("aom_tiles.obu", &["-c:v", "libaom-av1", "-cpu-used", "8", "-tiles", "2x2", "-f", "obu"]),
    v(
        "aom10.obu",
        &["-c:v", "libaom-av1", "-cpu-used", "8", "-pix_fmt", "yuv420p10le", "-f", "obu"],
    ),
    v(
        "vp9_p0.ivf",
        &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-f", "ivf"],
    ),
    v(
        "vp9_p1.ivf",
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-pix_fmt",
            "yuv444p",
            "-f",
            "ivf",
        ],
    ),
    v(
        "vp9_p2.ivf",
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-pix_fmt",
            "yuv420p10le",
            "-f",
            "ivf",
        ],
    ),
    v(
        "vp9_altref.ivf",
        &[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "good",
            "-cpu-used",
            "4",
            "-auto-alt-ref",
            "1",
            "-lag-in-frames",
            "8",
            "-f",
            "ivf",
        ],
    ),
    v("vp8.ivf", &["-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-f", "ivf"]),
    v("mpeg2.m2v", &["-c:v", "mpeg2video", "-bf", "2", "-f", "mpeg2video"]),
    v("mpeg4.m4v", &["-c:v", "mpeg4", "-bf", "2", "-f", "m4v"]),
    a("aac.adts", &["-c:a", "aac", "-b:a", "64k", "-f", "adts"]),
    a("aac51.adts", &["-ac", "6", "-c:a", "aac", "-f", "adts"]),
    a("ac3.ac3", &["-c:a", "ac3", "-f", "ac3"]),
    a("eac3.eac3", &["-c:a", "eac3", "-f", "eac3"]),
    a("mp3.mp3", &["-c:a", "libmp3lame", "-f", "mp3"]),
    a("dts.dts", &["-c:a", "dca", "-strict", "-2", "-f", "dts"]),
    a("truehd.thd", &["-c:a", "truehd", "-strict", "-2", "-f", "truehd"]),
    a("mlp.mlp", &["-c:a", "mlp", "-strict", "-2", "-f", "mlp"]),
    a("vorbis.ogg", &["-c:a", "libvorbis", "-f", "ogg"]),
    a("vorbis51.ogg", &["-ac", "6", "-c:a", "libvorbis", "-f", "ogg"]),
    a("alac.caf", &["-c:a", "alac", "-f", "caf"]),
    a("opus.opus", &["-c:a", "libopus", "-b:a", "48k", "-f", "ogg"]),
    a("opus51.opus", &["-ac", "6", "-c:a", "libopus", "-b:a", "96k", "-f", "ogg"]),
];

/// The knob byte of each pf-libav decoder, `KINDS` of fuzz/fuzz_targets/libav_dec.rs.
mod lav {
    pub const H264: u8 = 0;
    pub const H265: u8 = 1;
    pub const VP8: u8 = 2;
    pub const VP9: u8 = 3;
    pub const MPEG2: u8 = 4;
    pub const MPEG4: u8 = 5;
    pub const AAC: u8 = 6;
    pub const AC3: u8 = 7;
    pub const MP3: u8 = 8;
    pub const MP3_INDEXED: u8 = 9;
    pub const DTS: u8 = 10;
    pub const TRUEHD: u8 = 11;
    pub const MLP: u8 = 12;
    pub const VORBIS: u8 = 13;
    pub const ALAC: u8 = 14;
    /// A second run that must put out the same.
    pub const AGAIN: u8 = 0x40;
}

pub const TARGETS: [&str; 7] =
    ["vaapi_h264", "vaapi_h265", "vaapi_av1", "vaapi_vp9", "libav_dec", "opus_dec", "probe"];

/// opus_dec knobs: s16 at 48 kHz, f32, s16 at 16 kHz with a sink delay, f32 at 24 kHz in batches
/// of two.
const OPUS_KNOBS: [u8; 4] = [0x00, 0x01, 0x14, 0x43];

/// probe knobs: the whole window with exact reads, a 64 KiB window read to the end, a 100 byte
/// window with half reads cut at 2/7.
const PROBE_KNOBS: [u8; 3] = [0x07, 0x0e, 0x53];

/// What the probe target gets of a container.
const PROBE_MAX: usize = 64 << 10;

fn encode(es: &Path) -> Result<(), Error> {
    util::mkdir_p(es)?;
    for s in STREAMS {
        let out = es.join(s.name);
        let mut cmd = Command::new("ffmpeg");
        cmd.env_remove("LD_LIBRARY_PATH").args(["-v", "error", "-y", "-f", "lavfi", "-i"]);
        cmd.arg(if s.video { V } else { A }).args(s.args).arg(&out);
        let ok = cmd.stdin(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("seeds: ffmpeg could not make {}, its seeds are skipped", s.name);
            let _ = std::fs::remove_file(&out);
        }
    }
    Ok(())
}

/// Writes seeds for the codec targets from the streams in `<raw>/es`, made there first.
pub fn seeds(raw: &Path, out: &Path) -> Result<(), Error> {
    let es = raw.join("es");
    if util::which("ffmpeg".as_ref()).is_some() {
        encode(&es)?;
    }
    let mut w = Writer { out, count: [0; TARGETS.len()] };
    for t in TARGETS {
        util::mkdir_p(&out.join(t))?;
    }
    for f in util::glob(&es, "") {
        let name = util::file_name(&f).to_string_lossy().into_owned();
        let b = util::read(&f)?;
        let ext = name.rsplit('.').next().unwrap_or("");
        match ext {
            "h264" => h264(&mut w, &name, &b)?,
            "hevc" => hevc(&mut w, &name, &b)?,
            "obu" => av1(&mut w, &name, &b)?,
            "ivf" => ivf(&mut w, &name, &b)?,
            "m2v" => w.lav(&name, lav::MPEG2, &start_code_units(&b, &[0x00], &[0xB3, 0xB8]))?,
            "m4v" => w.lav(&name, lav::MPEG4, &start_code_units(&b, &[0xB6], &[0xB0, 0xB3]))?,
            "adts" => adts(&mut w, &name, &b)?,
            "ac3" | "eac3" => w.lav(&name, lav::AC3, &chunks(&b))?,
            "mp3" => {
                w.lav(&name, lav::MP3, &chunks(&b))?;
                w.lav(&format!("{name}.indexed"), lav::MP3_INDEXED, &chunks(&b))?;
            }
            "dts" => w.lav(&name, lav::DTS, &chunks(&b))?,
            "thd" => w.lav(&name, lav::TRUEHD, &chunks(&b))?,
            "mlp" => w.lav(&name, lav::MLP, &chunks(&b))?,
            "ogg" => vorbis(&mut w, &name, &b)?,
            "opus" => opus(&mut w, &name, &b)?,
            "caf" => alac(&mut w, &name, &b)?,
            _ => {}
        }
    }
    probe(&mut w, raw)?;
    for (t, n) in TARGETS.iter().zip(w.count) {
        println!("seeds: {t} {n} inputs");
    }
    Ok(())
}

struct Writer<'a> {
    out: &'a Path,
    count: [usize; TARGETS.len()],
}

impl Writer<'_> {
    fn put(&mut self, target: &str, name: &str, knob: u8, body: &[u8]) -> Result<(), Error> {
        let i = TARGETS.iter().position(|t| *t == target).unwrap_or(0);
        let mut seed = Vec::with_capacity(1 + body.len());
        seed.push(knob);
        seed.extend_from_slice(body);
        util::write(&self.out.join(target).join(format!("{name}.{knob:02x}")), &seed)?;
        self.count[i] += 1;
        Ok(())
    }

    /// pf-libav units behind `u16` LE lengths, once plain and once run twice.
    fn lav(&mut self, name: &str, kind: u8, units: &[Vec<u8>]) -> Result<(), Error> {
        let body = records(units);
        if body.is_empty() {
            return Ok(());
        }
        self.put("libav_dec", name, kind, &body)?;
        self.put("libav_dec", name, kind | lav::AGAIN, &body)
    }
}

/// Units behind `u16` LE lengths, a longer unit dropped, at most [`WHOLE_MAX`] bytes.
fn records(units: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for u in units.iter().filter(|u| u.len() <= usize::from(u16::MAX)) {
        if out.len() + 2 + u.len() > WHOLE_MAX {
            break;
        }
        out.extend_from_slice(&(u.len() as u16).to_le_bytes());
        out.extend_from_slice(u);
    }
    out
}

fn chunks(b: &[u8]) -> Vec<Vec<u8>> {
    b[..b.len().min(WHOLE_MAX)].chunks(4096).map(<[u8]>::to_vec).collect()
}

/// Annex B NAL payloads, header included, the zero of a 4-byte start code trimmed.
fn nals(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(b.len(), |n| n - 3);
        if e > s && starts.get(k + 1).is_some() && b[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&b[s..e]);
        }
    }
    out
}

/// Access units of NALs, a new one at a parameter set, SEI or delimiter after a slice and at a
/// slice that starts a picture.
fn access_units(
    nals: &[&[u8]],
    header: usize,
    starts_au: impl Fn(&[u8]) -> Option<bool>,
) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut has_slice = false;
    for n in nals {
        // Some(true) a slice starting a picture, Some(false) a slice, None a non-VCL unit.
        let kind = if n.len() > header { starts_au(n) } else { None };
        let new = match kind {
            Some(first) => first && has_slice,
            None => has_slice,
        };
        if new {
            units.push(std::mem::take(&mut cur));
            has_slice = false;
        }
        has_slice |= kind.is_some();
        cur.extend_from_slice(&[0, 0, 0, 1]);
        cur.extend_from_slice(n);
    }
    if !cur.is_empty() {
        units.push(cur);
    }
    units
}

fn h264(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    w.put("vaapi_h264", name, 0, &b[..b.len().min(WHOLE_MAX)])?;
    w.put("vaapi_h264", name, 2, &b[..b.len().min(512)])?;
    let all = nals(b);
    let find = |t: u8| all.iter().find(|n| n[0] & 0x1F == t).copied();
    let slice = all.iter().find(|n| matches!(n[0] & 0x1F, 1 | 5)).copied();
    if let (Some(sps), Some(pps), Some(slice)) = (find(7), find(8), slice)
        && sps.len() <= 255
        && pps.len() <= 255
    {
        let mut u = vec![sps.len() as u8];
        u.extend_from_slice(sps);
        u.push(pps.len() as u8);
        u.extend_from_slice(pps);
        u.extend_from_slice(&slice[..slice.len().min(4096)]);
        w.put("vaapi_h264", name, 3, &u)?;
    }
    let starts = |n: &[u8]| matches!(n[0] & 0x1F, 1 | 5).then(|| n[1] & 0x80 != 0);
    let units = access_units(&all, 1, starts);
    w.lav(name, lav::H264, &units)
}

fn hevc(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    w.put("vaapi_h265", name, 0, &b[..b.len().min(WHOLE_MAX)])?;
    w.put("vaapi_h265", name, 2, &b[..b.len().min(512)])?;
    let all = nals(b);
    let ty = |n: &[u8]| n[0] >> 1 & 0x3F;
    let find = |t: u8| all.iter().find(|n| n.len() > 2 && ty(n) == t).copied();
    let slice = all.iter().find(|n| n.len() > 2 && ty(n) < 32).copied();
    if let (Some(sps), Some(pps), Some(slice)) = (find(33), find(34), slice) {
        let mut u = Vec::new();
        for p in [sps, pps] {
            u.extend_from_slice(&(p.len() as u16).to_le_bytes());
            u.extend_from_slice(p);
        }
        u.extend_from_slice(&slice[..slice.len().min(4096)]);
        w.put("vaapi_h265", name, 3, &u)?;
    }
    let starts = |n: &[u8]| (ty(n) < 32).then(|| n[2] & 0x80 != 0);
    let units = access_units(&all, 2, starts);
    w.lav(name, lav::H265, &units)
}

/// `(type, payload)` of each OBU of a low overhead stream, stopping at the first bad one.
fn obus(b: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let h = b[at];
        let ext = usize::from(h & 4 != 0);
        let mut p = at + 1 + ext;
        if h & 2 == 0 || p >= b.len() {
            break;
        }
        let mut size = 0usize;
        for i in 0..8 {
            let Some(&x) = b.get(p) else {
                return out;
            };
            p += 1;
            size |= usize::from(x & 0x7F) << (7 * i);
            if x & 0x80 == 0 {
                break;
            }
        }
        let Some(payload) = b.get(p..p + size) else {
            break;
        };
        out.push((h >> 3 & 15, payload));
        at = p + size;
    }
    out
}

fn av1(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    w.put("vaapi_av1", name, 0, &b[..b.len().min(WHOLE_MAX)])?;
    w.put("vaapi_av1", name, 2, &b[..b.len().min(512)])?;
    let all = obus(b);
    let seq = all.iter().find(|o| o.0 == 1).map(|o| o.1);
    let frame = all.iter().find(|o| matches!(o.0, 3 | 6)).map(|o| o.1);
    if let (Some(seq), Some(frame)) = (seq, frame) {
        let mut u = (seq.len() as u16).to_le_bytes().to_vec();
        u.extend_from_slice(seq);
        u.extend_from_slice(&frame[..frame.len().min(4096)]);
        w.put("vaapi_av1", name, 3, &u)?;
    }
    Ok(())
}

fn ivf_frames(b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = b.get(6..8).map_or(b.len(), |h| usize::from(u16::from_le_bytes([h[0], h[1]])));
    while let Some(h) = b.get(at..at + 12) {
        let n = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let Some(f) = b.get(at + 12..at + 12 + n) else {
            break;
        };
        out.push(f.to_vec());
        at += 12 + n;
    }
    out
}

fn ivf(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    let frames = ivf_frames(b);
    if name.starts_with("vp8") {
        return w.lav(name, lav::VP8, &frames);
    }
    let body = records(&frames);
    w.put("vaapi_vp9", name, 0, &body)?;
    w.put("vaapi_vp9", name, 1, &body)?;
    w.lav(name, lav::VP9, &frames)
}

/// MPEG-1/2 or MPEG-4 Part 2 units, a new one at a header start code after a picture and at a
/// picture after a picture.
fn start_code_units(b: &[u8], picture: &[u8], header: &[u8]) -> Vec<Vec<u8>> {
    let mut cuts = vec![0];
    let mut seen_picture = false;
    let mut i = 0;
    while i + 4 <= b.len() {
        if b[i..i + 3] != [0, 0, 1] {
            i += 1;
            continue;
        }
        let code = b[i + 3];
        let is_picture = picture.contains(&code);
        if seen_picture && (is_picture || header.contains(&code)) {
            cuts.push(i);
            seen_picture = false;
        }
        seen_picture |= is_picture;
        i += 4;
    }
    cuts.push(b.len());
    cuts.windows(2).filter(|c| c[1] > c[0]).map(|c| b[c[0]..c[1]].to_vec()).collect()
}

/// ADTS as is, and as an AudioSpecificConfig with raw access units.
fn adts(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    w.lav(name, lav::AAC, &chunks(b))?;
    let mut units = Vec::new();
    let mut at = 0;
    while let Some(h) = b.get(at..at + 7) {
        if h[0] != 0xFF || h[1] & 0xF0 != 0xF0 {
            break;
        }
        let head = if h[1] & 1 != 0 { 7 } else { 9 };
        let len = usize::from(h[3] & 3) << 11 | usize::from(h[4]) << 3 | usize::from(h[5] >> 5);
        let Some(frame) = b.get(at + head..at + len) else {
            break;
        };
        if units.is_empty() {
            let (profile, rate, ch) = (h[2] >> 6, h[2] >> 2 & 15, (h[2] & 1) << 2 | h[3] >> 6);
            units.push(vec![(profile + 1) << 3 | rate >> 1, (rate & 1) << 7 | ch << 3]);
        }
        units.push(frame.to_vec());
        at += len.max(head);
    }
    w.lav(&format!("{name}.raw"), lav::AAC, &units)
}

/// The packets of an Ogg stream's first logical bitstream.
fn ogg_packets(b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut at = 0;
    while let Some(h) = b.get(at..at + 27) {
        if &h[..4] != b"OggS" {
            break;
        }
        let segs = usize::from(h[26]);
        let Some(table) = b.get(at + 27..at + 27 + segs) else {
            break;
        };
        let mut p = at + 27 + segs;
        for &lace in table {
            let Some(d) = b.get(p..p + usize::from(lace)) else {
                return out;
            };
            cur.extend_from_slice(d);
            p += usize::from(lace);
            if lace < 255 {
                out.push(std::mem::take(&mut cur));
            }
        }
        at = p;
    }
    out
}

/// The three Vorbis headers Xiph laced as Matroska's CodecPrivate, then the audio packets.
fn vorbis(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    let packets = ogg_packets(b);
    let [ident, comment, setup, ..] = packets.as_slice() else {
        return Ok(());
    };
    let mut head = vec![2u8];
    for len in [ident.len(), comment.len()] {
        head.extend(std::iter::repeat_n(0xFF, len / 255));
        head.push((len % 255) as u8);
    }
    for p in [ident, comment, setup] {
        head.extend_from_slice(p);
    }
    let mut units = vec![head];
    units.extend(packets[3..].iter().cloned());
    w.lav(name, lav::VORBIS, &units)
}

/// Ogg Opus packets, head and tags first, behind `u16` LE lengths.
fn opus(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    let body = records(&ogg_packets(b));
    if body.is_empty() {
        return Ok(());
    }
    for knob in OPUS_KNOBS {
        w.put("opus_dec", name, knob, &body)?;
    }
    Ok(())
}

/// The head of each container the demuxer seeds came from, committed or made.
fn probe(w: &mut Writer<'_>, raw: &Path) -> Result<(), Error> {
    for dir in util::list(raw)? {
        let dir = raw.join(dir);
        if !dir.is_dir() || dir.ends_with("es") {
            continue;
        }
        for f in util::glob(&dir, "") {
            let b = util::read(&f)?;
            let name = util::file_name(&f).to_string_lossy().into_owned();
            for knob in PROBE_KNOBS {
                w.put("probe", &name, knob, &b[..b.len().min(PROBE_MAX)])?;
            }
        }
    }
    Ok(())
}

/// The 36 byte `alac` atom from CAF's `kuki` and the packets `pakt` sizes out of `data`.
fn alac(w: &mut Writer<'_>, name: &str, b: &[u8]) -> Result<(), Error> {
    let (mut kuki, mut pakt, mut data) = (None, None, None);
    let mut at = 8;
    while let Some(h) = b.get(at..at + 12) {
        let size = u64::from_be_bytes([h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11]]);
        let end =
            usize::try_from(size).map_or(b.len(), |s| (at + 12).saturating_add(s)).min(b.len());
        let body = &b[at + 12..end];
        match &h[..4] {
            b"kuki" => kuki = Some(body),
            b"pakt" => pakt = Some(body),
            b"data" => data = body.get(4..),
            _ => {}
        }
        at = end;
        if end == b.len() {
            break;
        }
    }
    let (Some(kuki), Some(pakt), Some(data)) = (kuki, pakt, data) else {
        return Ok(());
    };
    let Some(atom) = kuki.windows(8).position(|w| w == b"\0\0\0\x24alac") else {
        return Ok(());
    };
    let mut units = vec![kuki[atom..(atom + 36).min(kuki.len())].to_vec()];
    let (mut p, mut d) = (24usize, 0usize);
    while p < pakt.len() {
        let mut size = 0usize;
        while let Some(&x) = pakt.get(p) {
            p += 1;
            size = size << 7 | usize::from(x & 0x7F);
            if x & 0x80 == 0 {
                break;
            }
        }
        let Some(packet) = data.get(d..d + size) else {
            break;
        };
        units.push(packet.to_vec());
        d += size;
    }
    w.lav(name, lav::ALAC, &units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nals_trim_the_zero_of_a_four_byte_start_code() {
        let b = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0x88];
        assert_eq!(nals(&b), [&[0x67, 0xAA][..], &[0x68, 0xBB], &[0x65, 0x88]]);
    }

    #[test]
    fn an_access_unit_starts_at_a_parameter_set_after_a_slice() {
        let n: [&[u8]; 5] = [&[0x67, 1], &[0x68, 1], &[0x65, 0x88], &[0x41, 0x9A], &[0x67, 2]];
        let starts = |n: &[u8]| matches!(n[0] & 0x1F, 1 | 5).then(|| n[1] & 0x80 != 0);
        let units = access_units(&n, 1, starts);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 0x88]);
        assert_eq!(units[2], [0, 0, 0, 1, 0x67, 2]);
    }

    #[test]
    fn records_drop_a_unit_past_u16() {
        let units = vec![vec![1, 2], vec![0; 70_000], vec![3]];
        assert_eq!(records(&units), [2, 0, 1, 2, 1, 0, 3]);
    }

    #[test]
    fn mpeg2_units_cut_at_the_next_picture_or_header() {
        let b =
            [0, 0, 1, 0xB3, 9, 0, 0, 1, 0x00, 1, 0, 0, 1, 0x00, 2, 0, 0, 1, 0xB3, 3, 0, 0, 1, 0];
        let u = start_code_units(&b, &[0x00], &[0xB3, 0xB8]);
        assert_eq!(u.len(), 3);
        assert_eq!(u[0], [0, 0, 1, 0xB3, 9, 0, 0, 1, 0x00, 1]);
        assert_eq!(u[1], [0, 0, 1, 0x00, 2]);
        assert_eq!(u[2], [0, 0, 1, 0xB3, 3, 0, 0, 1, 0]);
    }

    #[test]
    fn obus_read_sized_headers_and_extensions() {
        let b = [0x12, 0, 0x0A, 2, 0xAA, 0xBB, 0x36, 0x20, 1, 0xCC];
        assert_eq!(obus(&b), [(2, &[][..]), (1, &[0xAA, 0xBB]), (6, &[0xCC])]);
    }

    #[test]
    fn ogg_packets_join_laces_of_255() {
        let mut page = b"OggS".to_vec();
        page.extend_from_slice(&[0; 22]);
        page.push(3);
        page.extend_from_slice(&[255, 1, 2]);
        page.extend(std::iter::repeat_n(7, 256));
        page.extend_from_slice(&[8, 9]);
        let p = ogg_packets(&page);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].len(), p[1].as_slice()), (256, &[8, 9][..]));
    }
}
