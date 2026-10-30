//! `fixtures-av1`, AV1 streams by aomenc and per-frame MD5 oracles by aomdec, both libaom,
//! so independent of dav1d. 10-bit, 4:2:2 and mono lines hash aomdec's raw planar output.

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Error;
use crate::util;

#[derive(Clone, Copy)]
enum Paint {
    /// Zooming Mandelbrot, every frame differs.
    Mandelbrot,
    /// Moving gradients, the low bits of 10-bit samples carry picture too.
    Gradients,
}

#[derive(Clone, Copy)]
enum Expect {
    I420,
    Y,
    I422(usize),
}

struct Stream {
    name: &'static str,
    w: usize,
    h: usize,
    frames: u32,
    paint: Paint,
    pix: &'static str,
    enc: &'static [&'static str],
    expect: Expect,
}

const LAG0: [&str; 2] = ["--lag-in-frames=0", "--auto-alt-ref=0"];

const STREAMS: &[Stream] = &[
    Stream {
        name: "inter_64x48",
        w: 64,
        h: 48,
        frames: 10,
        paint: Paint::Mandelbrot,
        pix: "yuv420p",
        enc: &[LAG0[0], LAG0[1], "--limit=10"],
        expect: Expect::I420,
    },
    // Hidden alt-ref, shown later by a show_existing_frame unit.
    Stream {
        name: "altref_64x48",
        w: 64,
        h: 48,
        frames: 16,
        paint: Paint::Mandelbrot,
        pix: "yuv420p",
        enc: &[
            "--lag-in-frames=16",
            "--auto-alt-ref=1",
            "--min-gf-interval=4",
            "--max-gf-interval=8",
            "--limit=16",
        ],
        expect: Expect::I420,
    },
    // Odd size, strides wider than rows and the chroma rounds up.
    Stream {
        name: "odd_61x37",
        w: 61,
        h: 37,
        frames: 8,
        paint: Paint::Mandelbrot,
        pix: "yuv420p",
        enc: &[LAG0[0], LAG0[1], "--limit=8"],
        expect: Expect::I420,
    },
    Stream {
        name: "mono_64x48",
        w: 64,
        h: 48,
        frames: 6,
        paint: Paint::Mandelbrot,
        pix: "yuv420p",
        enc: &[LAG0[0], LAG0[1], "--monochrome", "--limit=6"],
        expect: Expect::Y,
    },
    Stream {
        name: "tiles_256x128",
        w: 256,
        h: 128,
        frames: 6,
        paint: Paint::Mandelbrot,
        pix: "yuv420p",
        enc: &[LAG0[0], LAG0[1], "--tile-columns=2", "--tile-rows=1", "--limit=6"],
        expect: Expect::I420,
    },
    Stream {
        name: "grain_64x48",
        w: 64,
        h: 48,
        frames: 6,
        paint: Paint::Mandelbrot,
        pix: "yuv420p",
        enc: &[LAG0[0], LAG0[1], "--film-grain-test=1", "--limit=6"],
        expect: Expect::I420,
    },
    Stream {
        name: "tenbit_64x48",
        w: 64,
        h: 48,
        frames: 8,
        paint: Paint::Gradients,
        pix: "yuv420p10le",
        enc: &[LAG0[0], LAG0[1], "--bit-depth=10", "--input-bit-depth=10", "--limit=8"],
        expect: Expect::I420,
    },
    Stream {
        name: "tenbit_odd_61x37",
        w: 61,
        h: 37,
        frames: 4,
        paint: Paint::Gradients,
        pix: "yuv420p10le",
        enc: &[LAG0[0], LAG0[1], "--bit-depth=10", "--input-bit-depth=10", "--limit=4"],
        expect: Expect::I420,
    },
    // Profile 1, av1dec reduces it to p010.
    Stream {
        name: "tenbit444_64x48",
        w: 64,
        h: 48,
        frames: 2,
        paint: Paint::Gradients,
        pix: "yuv444p10le",
        enc: &[
            LAG0[0],
            LAG0[1],
            "--profile=1",
            "--bit-depth=10",
            "--input-bit-depth=10",
            "--limit=2",
        ],
        expect: Expect::I420,
    },
    Stream {
        name: "i422_64x48",
        w: 64,
        h: 48,
        frames: 4,
        paint: Paint::Mandelbrot,
        pix: "yuv422p",
        enc: &[LAG0[0], LAG0[1], "--profile=2", "--limit=4"],
        expect: Expect::I422(1),
    },
    Stream {
        name: "tenbit422_64x48",
        w: 64,
        h: 48,
        frames: 4,
        paint: Paint::Gradients,
        pix: "yuv422p10le",
        enc: &[
            LAG0[0],
            LAG0[1],
            "--profile=2",
            "--bit-depth=10",
            "--input-bit-depth=10",
            "--limit=4",
        ],
        expect: Expect::I422(2),
    },
];

/// Fastest good-quality one pass, a fixed quantiser so streams are small and deterministic.
const ENC: [&str; 7] = [
    "--ivf",
    "--cpu-used=6",
    "--end-usage=q",
    "--cq-level=24",
    "--threads=1",
    "--passes=1",
    "--kf-max-dist=999",
];

struct Ctx<'a> {
    dir: &'a Path,
    work: &'a Path,
}

fn aom(ctx: &Ctx, tool: &str) -> Command {
    let mut cmd = Command::new("nix");
    cmd.current_dir(ctx.dir).args(["shell", "nixpkgs#libaom", "-c", tool]);
    cmd
}

fn ffmpeg(ctx: &Ctx) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.current_dir(ctx.dir).env_remove("LD_LIBRARY_PATH");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    cmd
}

fn paint(
    ctx: &Ctx,
    name: &str,
    w: usize,
    h: usize,
    frames: u32,
    kind: Paint,
    pix: &str,
) -> Result<(), Error> {
    let mut cmd = ffmpeg(ctx);
    match kind {
        Paint::Mandelbrot => {
            cmd.args(["-f", "lavfi", "-i", &format!("mandelbrot=size={w}x{h}:rate=25")])
        }
        Paint::Gradients => {
            cmd.args(["-f", "lavfi", "-i", &format!("gradients=size={w}x{h}:rate=25:speed=0.05")])
        }
    };
    cmd.args(["-frames:v", &frames.to_string(), "-pix_fmt", pix]);
    if matches!(kind, Paint::Gradients) {
        cmd.args(["-strict", "-1"]);
    }
    cmd.args(["-f", "yuv4mpegpipe"]).arg(ctx.work.join(format!("{name}.y4m")));
    util::run("ffmpeg", &mut cmd)
}

fn encode(ctx: &Ctx, name: &str, out: &Path, opts: &[&str]) -> Result<(), Error> {
    let mut cmd = aom(ctx, "aomenc");
    cmd.args(ENC).args(opts).arg("-o").arg(out.join(format!("{name}.ivf")));
    cmd.arg(ctx.work.join(format!("{name}.y4m")));
    util::run_silent("aomenc", &mut cmd)
}

/// Frame lines from aomdec `--md5` with a `%w_%h_%3` output pattern.
fn md5_lines_from_aomdec(stdout: &str) -> Result<String, Error> {
    let mut out = String::new();
    for (n, line) in stdout.lines().enumerate() {
        let bad = || Error::Parse(format!("aomdec --md5 line {line:?}"));
        let (md5, path) = line.trim().split_once(char::is_whitespace).ok_or_else(bad)?;
        let file = path.trim().rsplit('/').next().unwrap_or("");
        let mut parts = file.split('_');
        let (Some(w), Some(h), Some(_), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let _ = writeln!(out, "{n} {w} {h} {md5}");
    }
    Ok(out)
}

fn md5_lines(data: &[u8], frame: usize, w: usize, h: usize) -> String {
    let mut out = String::new();
    if frame == 0 {
        return out;
    }
    for (n, chunk) in data.chunks_exact(frame).enumerate() {
        let _ = writeln!(out, "{n} {w} {h} {:x}", md5::compute(chunk));
    }
    out
}

fn i422_frame(w: usize, h: usize, bytes: usize) -> usize {
    bytes * (w * h + 2 * w.div_ceil(2) * h)
}

fn expect_i420(ctx: &Ctx, name: &str) -> Result<(), Error> {
    let frames = ctx.work.join(format!("{name}.frames"));
    util::mkdir_p(&frames)?;
    let mut cmd = aom(ctx, "aomdec");
    cmd.args(["--i420", "--md5", "-o"])
        .arg(frames.join("%w_%h_%3.i420"))
        .arg(format!("{name}.ivf"));
    let stdout = util::capture("aomdec", cmd.stderr(Stdio::inherit()))?;
    let lines = md5_lines_from_aomdec(&String::from_utf8_lossy(&stdout))?;
    util::write(&ctx.dir.join(format!("{name}.md5")), lines.as_bytes())
}

fn expect_raw(ctx: &Ctx, name: &str, w: usize, h: usize, frame: usize) -> Result<(), Error> {
    let yuv = ctx.work.join(format!("{name}.yuv"));
    let mut cmd = aom(ctx, "aomdec");
    cmd.args(["--rawvideo", "-o"]).arg(&yuv).arg(format!("{name}.ivf"));
    util::run_silent("aomdec", &mut cmd)?;
    let lines = md5_lines(&util::read(&yuv)?, frame, w, h);
    util::write(&ctx.dir.join(format!("{name}.md5")), lines.as_bytes())
}

fn expect(ctx: &Ctx, s: &Stream) -> Result<(), Error> {
    match s.expect {
        Expect::I420 => expect_i420(ctx, s.name),
        Expect::Y => expect_raw(ctx, s.name, s.w, s.h, s.w * s.h),
        Expect::I422(b) => expect_raw(ctx, s.name, s.w, s.h, i422_frame(s.w, s.h, b)),
    }
}

const IVF_FRAMES: usize = 24;
const IVF_HEADER: usize = 32;

fn splice_ivf(a: &[u8], b: &[u8], frames: u32) -> Result<Vec<u8>, Error> {
    if a.len() < IVF_HEADER || b.len() < IVF_HEADER {
        return Err(Error::Parse("an IVF stream shorter than its header".into()));
    }
    let mut out = Vec::with_capacity(a.len() + b.len() - IVF_HEADER);
    out.extend_from_slice(a);
    out[IVF_FRAMES..IVF_FRAMES + 4].copy_from_slice(&frames.to_le_bytes());
    out.extend_from_slice(&b[IVF_HEADER..]);
    Ok(out)
}

/// A resolution change mid-stream, two sequences under the first's IVF header.
fn resize(ctx: &Ctx) -> Result<(), Error> {
    let opts = [LAG0[0], LAG0[1], "--limit=5"];
    for (name, w, h) in [("resize_a", 64, 48), ("resize_b", 96, 64)] {
        paint(ctx, name, w, h, 5, Paint::Mandelbrot, "yuv420p")?;
        encode(ctx, name, ctx.work, &opts)?;
    }
    let a = util::read(&ctx.work.join("resize_a.ivf"))?;
    let b = util::read(&ctx.work.join("resize_b.ivf"))?;
    let name = "resize_64x48_96x64";
    util::write(&ctx.dir.join(format!("{name}.ivf")), &splice_ivf(&a, &b, 10)?)?;
    expect_i420(ctx, name)
}

/// Hinnant's civil_from_days.
fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn today_utc() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_date((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn skip_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        match *b.get(j)? {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
}

fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let (mut depth, mut j) = (0usize, i);
            loop {
                match *b.get(j)? {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        _ => {
            let mut j = i;
            while b
                .get(j)
                .is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace())
            {
                j += 1;
            }
            Some(j)
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn member<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let b = obj.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i)? != &b'"' {
            return None;
        }
        let key_end = skip_string(b, i)?;
        let name = &obj[i + 1..key_end - 1];
        i = skip_ws(b, key_end);
        if b.get(i)? != &b':' {
            return None;
        }
        let start = skip_ws(b, i + 1);
        let end = skip_value(b, start)?;
        if name == key {
            return Some(&obj[start..end]);
        }
        i = skip_ws(b, end);
        if b.get(i)? != &b',' {
            return None;
        }
        i += 1;
    }
}

fn json_str(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?).unwrap_or('?'));
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// The locked nixpkgs rev from `nix flake metadata --json`, else its narHash. Never the url,
/// a path flake's url is a local store path.
fn nixpkgs_rev(json: &str) -> String {
    let locked = member(json, "locked");
    let field = |k| locked.and_then(|l| member(l, k)).and_then(json_str).filter(|r| !r.is_empty());
    field("rev").or_else(|| field("narHash")).unwrap_or_else(|| "?".into())
}

fn text_of(cmd: &mut Command) -> String {
    match cmd.output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        }
        Err(_) => String::new(),
    }
}

fn versions(ctx: &Ctx) -> String {
    let help = text_of(aom(ctx, "aomenc").arg("--help"));
    const BANNER: &str = "AOMedia Project AV1 Encoder ";
    let banner = help.lines().find_map(|l| {
        let rest = &l[l.find(BANNER)?..];
        let end = rest[BANNER.len()..].find(' ').map_or(rest.len(), |e| e + BANNER.len());
        Some(rest[..end].to_string())
    });
    let mut cmd = Command::new("nix");
    let libaom =
        text_of(cmd.args(["eval", "--raw", "nixpkgs#libaom.version"]).stderr(Stdio::null()));
    let mut cmd = Command::new("ffmpeg");
    let ffmpeg = text_of(cmd.env_remove("LD_LIBRARY_PATH").arg("-version").stderr(Stdio::null()));
    let mut cmd = Command::new("nix");
    let meta = text_of(cmd.args(["flake", "metadata", "nixpkgs", "--json"]).stderr(Stdio::null()));
    format!(
        "made by cargo xtask fixtures-av1 on {}\naomenc/aomdec: {} libaom {}\nffmpeg: {}\nnixpkgs: {}\n",
        today_utc(),
        banner.unwrap_or_default(),
        libaom.trim_end_matches('\n'),
        ffmpeg.lines().next().unwrap_or(""),
        nixpkgs_rev(&meta),
    )
}

pub fn fixtures(root: &Path) -> Result<(), Error> {
    let dir = root.join("plugins/video/av1/tests/fixtures");
    let work = util::temp_dir("av1")?;
    let ctx = Ctx { dir: &dir, work: &work.path };
    for s in STREAMS {
        paint(&ctx, s.name, s.w, s.h, s.frames, s.paint, s.pix)?;
        encode(&ctx, s.name, &dir, s.enc)?;
        expect(&ctx, s)?;
    }
    resize(&ctx)?;
    util::write(&dir.join("VERSIONS"), versions(&ctx).as_bytes())?;
    let mut ls = Command::new("ls");
    ls.current_dir(&dir).arg("-la");
    for f in util::glob(&dir, ".ivf") {
        ls.arg(Path::new(".").join(util::file_name(&f)));
    }
    util::run_live("ls", &mut ls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_splice_keeps_the_first_header_with_the_summed_count() {
        let mut a = vec![0u8; 32];
        a[..4].copy_from_slice(b"DKIF");
        a[12..14].copy_from_slice(&64u16.to_le_bytes());
        a[24..28].copy_from_slice(&5u32.to_le_bytes());
        a.extend_from_slice(b"frames of a");
        let mut b = vec![0xffu8; 32];
        b.extend_from_slice(b"frames of b");
        let out = splice_ivf(&a, &b, 10);
        assert!(out.is_ok());
        let out = out.unwrap_or_default();
        assert_eq!(&out[..4], b"DKIF");
        assert_eq!(&out[12..14], &64u16.to_le_bytes());
        assert_eq!(&out[24..28], &10u32.to_le_bytes());
        assert_eq!(&out[28..32], &[0, 0, 0, 0]);
        assert_eq!(&out[32..], b"frames of aframes of b");
        assert!(splice_ivf(&a[..31], &b, 10).is_err());
    }

    #[test]
    fn md5_matches_the_rfc_1321_vectors() {
        let vectors: [(&[u8], &str); 7] = [
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (b"abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, want) in vectors {
            assert_eq!(format!("{:x}", md5::compute(input)), want);
        }
    }

    #[test]
    fn raw_frames_hash_whole_frames_only() {
        let data = b"abcabcab";
        assert_eq!(
            md5_lines(data, 3, 3, 1),
            "0 3 1 900150983cd24fb0d6963f7d28e17f72\n1 3 1 900150983cd24fb0d6963f7d28e17f72\n"
        );
        assert_eq!(md5_lines(data, 0, 0, 0), "");
        assert_eq!(i422_frame(64, 48, 1), 64 * 48 * 2);
        assert_eq!(i422_frame(61, 37, 2), 2 * (61 * 37 + 2 * 31 * 37));
    }

    #[test]
    fn aomdec_md5_lines_take_the_size_from_the_file_name() {
        let out = "0123abcd  /tmp/x_y/inter.frames/64_48_001.i420\nffee  /w/96_64_002.i420\n";
        let lines = md5_lines_from_aomdec(out).ok();
        assert_eq!(lines.as_deref(), Some("0 64 48 0123abcd\n1 96 64 ffee\n"));
        assert!(md5_lines_from_aomdec("nonsense\n").is_err());
        assert!(md5_lines_from_aomdec("ab /w/64_48.i420\n").is_err());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        assert_eq!(civil_date(20_755), (2026, 10, 29));
        assert_eq!(civil_date(-1), (1969, 12, 31));
    }

    #[test]
    fn the_nixpkgs_rev_is_the_locked_rev_else_the_nar_hash() {
        let github = r#"{"description":"x","locked":{"lastModified":1,"owner":"NixOS","repo":"nixpkgs","rev":"abc123","type":"github"},"locks":{"nodes":{"root":{"locked":{"rev":"nested"}}}},"url":"github:NixOS/nixpkgs/abc123"}"#;
        assert_eq!(nixpkgs_rev(github), "abc123");
        let path = r#"{ "lastModified": 0, "locked": { "narHash": "sha256-x", "path": "/nix/store/y", "type": "path" },
            "locks": {"nodes": {"root": {"rev": "nope"}}}, "url": "path:/nix/store/y?narHash=sha256-a%2Bb" }"#;
        assert_eq!(nixpkgs_rev(path), "sha256-x");
        assert_eq!(nixpkgs_rev("{}"), "?");
        assert_eq!(nixpkgs_rev(""), "?");
        assert_eq!(json_str(r#""a\"b\\cA""#).as_deref(), Some("a\"b\\cA"));
        assert_eq!(member(r#"{"a":[1,{"b":2}],"b":"]"}"#, "b"), Some(r#""]""#));
    }
}
