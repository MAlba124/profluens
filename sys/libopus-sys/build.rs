//! Builds libopus from `opus/` or `LIBOPUS_SRC` into a static archive, with the file lists of its
//! `.mk` files and meson's float `config.h` defines, and asserts `CONSTS` against its headers.

#![allow(clippy::disallowed_methods)] // build script, allocates once at build time

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const OPUS_RELEASE: &str = "1.6.1";

const SRC_VAR: &str = "LIBOPUS_SRC";

const CONSTS: &[(&str, i32)] = &[
    ("OPUS_OK", 0),
    ("OPUS_APPLICATION_VOIP", 2048),
    ("OPUS_APPLICATION_AUDIO", 2049),
    ("OPUS_APPLICATION_RESTRICTED_LOWDELAY", 2051),
    ("OPUS_SIGNAL_VOICE", 3001),
    ("OPUS_SIGNAL_MUSIC", 3002),
    ("OPUS_AUTO", -1000),
    ("OPUS_BITRATE_MAX", -1),
    ("OPUS_SET_BITRATE_REQUEST", 4002),
    ("OPUS_GET_BITRATE_REQUEST", 4003),
    ("OPUS_SET_VBR_REQUEST", 4006),
    ("OPUS_SET_COMPLEXITY_REQUEST", 4010),
    ("OPUS_SET_INBAND_FEC_REQUEST", 4012),
    ("OPUS_SET_PACKET_LOSS_PERC_REQUEST", 4014),
    ("OPUS_SET_VBR_CONSTRAINT_REQUEST", 4020),
    ("OPUS_SET_SIGNAL_REQUEST", 4024),
    ("OPUS_RESET_STATE", 4028),
];

const LIST_FILES: &[&str] = &["celt_sources.mk", "silk_sources.mk", "opus_sources.mk"];

const PLAIN: &[&str] =
    &["CELT_SOURCES", "SILK_SOURCES", "SILK_SOURCES_FLOAT", "OPUS_SOURCES", "OPUS_SOURCES_FLOAT"];
const X86_64_PLAIN: &[&str] =
    &["CELT_SOURCES_X86_RTCD", "SILK_SOURCES_X86_RTCD", "CELT_SOURCES_SSE", "CELT_SOURCES_SSE2"];
const X86_SSE4_1: &[&str] = &["CELT_SOURCES_SSE4_1", "SILK_SOURCES_SSE4_1"];
const X86_AVX2: &[&str] = &["CELT_SOURCES_AVX2", "SILK_SOURCES_AVX2", "SILK_SOURCES_FLOAT_AVX2"];
const ARM_NEON: &[&str] = &["CELT_SOURCES_ARM_NEON_INTR", "SILK_SOURCES_ARM_NEON_INTR"];

const SSE4_1_FLAGS: &[&str] = &["-msse4.1"];
const AVX2_FLAGS: &[&str] = &["-mavx", "-mfma", "-mavx2"];

const DEFINES: &[(&str, &str)] = &[
    ("OPUS_BUILD", "1"),
    ("ENABLE_HARDENING", "1"),
    ("VAR_ARRAYS", "1"),
    ("HAVE_LRINT", "1"),
    ("HAVE_LRINTF", "1"),
    ("HAVE_STDINT_H", "1"),
    ("DISABLE_DEBUG_FLOAT", "1"),
];

const X86_64_DEFINES: &[(&str, &str)] = &[
    ("OPUS_X86_MAY_HAVE_SSE", "1"),
    ("OPUS_X86_PRESUME_SSE", "1"),
    ("OPUS_X86_MAY_HAVE_SSE2", "1"),
    ("OPUS_X86_PRESUME_SSE2", "1"),
    ("OPUS_X86_MAY_HAVE_SSE4_1", "1"),
    ("OPUS_X86_MAY_HAVE_AVX2", "1"),
    ("OPUS_HAVE_RTCD", "1"),
    ("CPU_INFO_BY_ASM", "1"),
];

// configure.ac's PRESUME_AARCH64 name, meson misspells it `OPUS_X86_` and never sets it.
const AARCH64_DEFINES: &[(&str, &str)] = &[
    ("OPUS_ARM_MAY_HAVE_NEON_INTR", "1"),
    ("OPUS_ARM_PRESUME_NEON_INTR", "1"),
    ("OPUS_ARM_PRESUME_AARCH64_NEON_INTR", "1"),
];

enum BuildError {
    Env(&'static str),
    TreeMissing(PathBuf),
    Release { found: String },
    List { name: &'static str },
    Io { path: PathBuf, err: io::Error },
    Cc(cc::Error),
    Spawn { src: PathBuf, err: io::Error },
    Compile { src: PathBuf, status: ExitStatus },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(var) => write!(f, "{var} is not set, run the build through cargo"),
            Self::TreeMissing(p) => write!(
                f,
                "no libopus sources, {} is missing. Run `git submodule update --init sys/libopus-sys/opus` or set {SRC_VAR} to an unpacked opus {OPUS_RELEASE} tree",
                p.display()
            ),
            Self::Release { found } => write!(
                f,
                "the {SRC_VAR} tree is opus {found}, build.rs is set up for {OPUS_RELEASE}, use that release or update OPUS_RELEASE"
            ),
            Self::List { name } => {
                write!(f, "no {name} in the tree's .mk files, the lists moved upstream")
            }
            Self::Io { path, err } => write!(f, "{}, {err}", path.display()),
            Self::Cc(e) => write!(f, "cc, {e}"),
            Self::Spawn { src, err } => {
                write!(f, "running the compiler on {} failed, {err}", src.display())
            }
            Self::Compile { src, status } => {
                write!(f, "compiling {} failed with {status}, its output is above", src.display())
            }
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("libopus-sys: {e}");
        std::process::exit(1);
    }
}

fn env(var: &'static str) -> Result<String, BuildError> {
    std::env::var(var).map_err(|_| BuildError::Env(var))
}

fn run() -> Result<(), BuildError> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={SRC_VAR}");

    let root = PathBuf::from(env("CARGO_MANIFEST_DIR")?);
    let out = PathBuf::from(env("OUT_DIR")?);
    let arch = env("CARGO_CFG_TARGET_ARCH")?;
    let opus = std::env::var_os(SRC_VAR).map_or_else(|| root.join("opus"), PathBuf::from);
    println!("cargo:rerun-if-changed={}", opus.display());
    let header = opus.join("include/opus.h");
    if !header.is_file() {
        return Err(BuildError::TreeMissing(header));
    }
    // Only a release tarball ships package_version.
    if let Ok(version) = std::fs::read_to_string(opus.join("package_version")) {
        let found =
            version.trim().trim_start_matches("PACKAGE_VERSION=").trim_matches('"').to_owned();
        if found != OPUS_RELEASE {
            return Err(BuildError::Release { found });
        }
    }
    let lists = read_lists(&opus)?;
    let files = |names: &[&'static str]| -> Result<Vec<String>, BuildError> {
        let mut v = Vec::new();
        for &name in names {
            let (_, list) = lists.iter().find(|l| l.0 == name).ok_or(BuildError::List { name })?;
            v.extend(list.iter().cloned());
        }
        Ok(v)
    };

    let mut b = cc::Build::new();
    b.include(&opus)
        .include(opus.join("include"))
        .include(opus.join("celt"))
        .include(opus.join("silk"))
        .include(opus.join("silk/float"))
        .include(opus.join("silk/fixed"))
        .define("PACKAGE_VERSION", format!("\"{OPUS_RELEASE}\"").as_str())
        .std("gnu99")
        .opt_level(2) // as upstream releases, the codec is unusable at -O0
        .warnings(false)
        .flag_if_supported("-fvisibility=hidden");
    for (name, value) in DEFINES {
        b.define(name, *value);
    }

    let plain = |s: &String| Job { src: opus.join(s), flags: &[] };
    let mut jobs: Vec<Job> = files(PLAIN)?.iter().map(plain).collect();
    match arch.as_str() {
        "x86_64" => {
            for (name, value) in X86_64_DEFINES {
                b.define(name, *value);
            }
            jobs.extend(files(X86_64_PLAIN)?.iter().map(plain));
            jobs.extend(
                files(X86_SSE4_1)?.iter().map(|s| Job { src: opus.join(s), flags: SSE4_1_FLAGS }),
            );
            jobs.extend(
                files(X86_AVX2)?.iter().map(|s| Job { src: opus.join(s), flags: AVX2_FLAGS }),
            );
        }
        "aarch64" => {
            for (name, value) in AARCH64_DEFINES {
                b.define(name, *value);
            }
            jobs.extend(files(ARM_NEON)?.iter().map(plain));
        }
        _ => {}
    }

    let compiler = b.try_get_compiler().map_err(BuildError::Cc)?;
    let objects = run_jobs(&jobs, &compiler, &opus, &out)?;
    let mut archive = cc::Build::new();
    for o in &objects {
        archive.object(o);
    }
    archive.try_compile("opus").map_err(BuildError::Cc)?;
    println!("cargo:rustc-link-lib=m");

    check_consts(&opus, &out)
}

fn read_lists(opus: &Path) -> Result<Vec<(String, Vec<String>)>, BuildError> {
    let mut lists: Vec<(String, Vec<String>)> = Vec::new();
    for file in LIST_FILES {
        let path = opus.join(file);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| BuildError::Io { path: path.clone(), err })?;
        let mut open = false;
        for line in text.lines() {
            let mut rest = line;
            if !open {
                let Some((name, value)) = line.split_once('=') else { continue };
                let name = name.trim();
                let upper = |c: u8| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_';
                if name.is_empty() || !name.bytes().all(upper) {
                    continue;
                }
                lists.push((name.to_owned(), Vec::new()));
                rest = value;
            }
            open = rest.trim_end().ends_with('\\');
            if let Some((_, list)) = lists.last_mut() {
                let words =
                    rest.split(|c: char| c.is_whitespace() || c == '\\').filter(|t| !t.is_empty());
                list.extend(words.map(str::to_owned));
            }
        }
    }
    Ok(lists)
}

fn check_consts(opus: &Path, out: &Path) -> Result<(), BuildError> {
    let mut c = String::from(
        "/* generated by libopus-sys build.rs, the CONSTS table as C asserts */\n\
         #include <opus.h>\n#include <opus_multistream.h>\n",
    );
    for (name, value) in CONSTS {
        c.push_str(&format!("_Static_assert({name} == {value}, \"{name}\");\n"));
    }
    let path = out.join("consts_check.c");
    std::fs::write(&path, c).map_err(|err| BuildError::Io { path: path.clone(), err })?;
    cc::Build::new()
        .file(&path)
        .include(opus.join("include"))
        .cargo_metadata(false)
        .warnings(false)
        .try_compile_intermediates()
        .map_err(BuildError::Cc)?;
    Ok(())
}

struct Job {
    src: PathBuf,
    flags: &'static [&'static str],
}

fn run_jobs(
    jobs: &[Job],
    cc: &cc::Tool,
    opus: &Path,
    out: &Path,
) -> Result<Vec<PathBuf>, BuildError> {
    let dir = out.join("obj");
    std::fs::create_dir_all(&dir).map_err(|err| BuildError::Io { path: dir.clone(), err })?;
    let object = |j: &Job| {
        let rel = j.src.strip_prefix(opus).unwrap_or(&j.src).to_string_lossy().replace('/', "_");
        dir.join(format!("{rel}.o"))
    };
    let compile = |j: &Job| -> Result<(), BuildError> {
        let status = cc
            .to_command()
            .args(j.flags)
            .arg("-c")
            .arg("-o")
            .arg(object(j))
            .arg(&j.src)
            .status()
            .map_err(|err| BuildError::Spawn { src: j.src.clone(), err })?;
        if status.success() {
            Ok(())
        } else {
            Err(BuildError::Compile { src: j.src.clone(), status })
        }
    };
    // Largest first so the slowest does not start last.
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    order.sort_by_key(|&i| {
        std::cmp::Reverse(std::fs::metadata(&jobs[i].src).map_or(0, |m| m.len()))
    });
    let threads =
        std::env::var("NUM_JOBS").ok().and_then(|n| n.parse().ok()).unwrap_or(4usize).max(1);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(jobs.len()))
            .map(|_| {
                scope.spawn(|| -> Result<(), BuildError> {
                    while !stop.load(Ordering::Relaxed) {
                        let Some(&i) = order.get(next.fetch_add(1, Ordering::Relaxed)) else {
                            break;
                        };
                        if let Err(e) = compile(&jobs[i]) {
                            stop.store(true, Ordering::Relaxed);
                            return Err(e);
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        workers
            .into_iter()
            .try_for_each(|w| w.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
    })?;
    Ok(jobs.iter().map(object).collect())
}
