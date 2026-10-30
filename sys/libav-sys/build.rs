// Build script, allocation is fine.
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const FFMPEG_RELEASE: &str = "9.0.2";

enum BuildError {
    Env(&'static str),
    NoConfig(String),
    SubmoduleMissing(PathBuf),
    Release { found: String },
    Io { path: PathBuf, err: io::Error },
    FewSources(usize),
    NasmMissing,
    Cc(cc::Error),
    Spawn { src: PathBuf, err: io::Error },
    Compile { src: PathBuf, status: ExitStatus },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(var) => write!(f, "{var} is not set, run the build through cargo"),
            Self::NoConfig(t) => write!(
                f,
                "no committed FFmpeg configuration for {t}, run cargo xtask libav-config {t} and commit config/{t}/"
            ),
            Self::SubmoduleMissing(p) => write!(
                f,
                "the ffmpeg submodule is not checked out ({} is missing), run `git submodule update --init --depth 1`",
                p.display()
            ),
            Self::Release { found } => write!(
                f,
                "the ffmpeg submodule is at {found}, config/ is for {FFMPEG_RELEASE}, run `git submodule update --init --depth 1` or regenerate config/"
            ),
            Self::Io { path, err } => write!(f, "{}, {err}", path.display()),
            Self::FewSources(n) => {
                write!(f, "sources.txt lists {n} C files, too few for the two libraries")
            }
            Self::NasmMissing => {
                write!(f, "the x86 assembly needs `nasm` on PATH or in $NASM (the devshell has it)")
            }
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
        eprintln!("libav-sys: {e}");
        std::process::exit(1);
    }
}

fn env(var: &'static str) -> Result<String, BuildError> {
    std::env::var(var).map_err(|_| BuildError::Env(var))
}

fn read(path: &Path) -> Result<String, BuildError> {
    std::fs::read_to_string(path).map_err(|err| BuildError::Io { path: path.to_owned(), err })
}

fn run() -> Result<(), BuildError> {
    let target = env("TARGET")?;
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim");
    println!("cargo:rerun-if-changed=config/{target}");
    println!("cargo:rerun-if-changed=ffmpeg");
    println!("cargo:rerun-if-env-changed=NASM");

    let root = PathBuf::from(env("CARGO_MANIFEST_DIR")?);
    let out = PathBuf::from(env("OUT_DIR")?);
    let config = root.join("config").join(&target);
    let ffmpeg = root.join("ffmpeg");
    if !config.is_dir() {
        return Err(BuildError::NoConfig(target));
    }
    let release_path = ffmpeg.join("RELEASE");
    if !release_path.is_file() {
        return Err(BuildError::SubmoduleMissing(release_path));
    }
    let release = read(&release_path)?.trim().to_owned();
    if release != FFMPEG_RELEASE {
        return Err(BuildError::Release { found: release });
    }

    let flags = read(&config.join("flags.txt"))?;
    let field = |name: &str| -> Vec<String> {
        let line = flags.lines().find(|l| l.starts_with(&format!("{name} "))).unwrap_or("");
        line.split_whitespace().skip(1).map(str::to_string).collect()
    };
    let sources = read(&config.join("sources.txt"))?;
    let c: Vec<&str> = sources.lines().filter(|s| s.ends_with(".c")).collect();
    let asm: Vec<&str> = sources.lines().filter(|s| s.ends_with(".asm")).collect();
    if c.len() <= 100 {
        return Err(BuildError::FewSources(c.len()));
    }

    let mut b = cc::Build::new();
    b.include(&config)
        .include(&ffmpeg)
        .include(root.join("shim"))
        .define("HAVE_AV_CONFIG_H", None)
        .opt_level(3)
        .warnings(false)
        .flag_if_supported("-fvisibility=hidden");
    for f in field("cppflags") {
        b.flag(&f);
    }
    for f in field("cflags") {
        if f.starts_with("-O") {
            continue;
        }
        b.flag_if_supported(&f);
    }

    // cargo fuzz sanitizes the Rust alone, so a fuzz build gives the C its ASan and coverage.
    let asan =
        std::env::var("CARGO_CFG_SANITIZE").is_ok_and(|s| s.split(',').any(|s| s == "address"));
    let fuzzing = std::env::var_os("CARGO_CFG_FUZZING").is_some();
    if asan || fuzzing {
        b.compiler("clang");
    }
    if asan {
        b.flag("-fsanitize=address");
    }
    if fuzzing {
        b.flag("-fsanitize=fuzzer-no-link");
    }

    let compiler = b.try_get_compiler().map_err(BuildError::Cc)?;
    let mut jobs: Vec<Job> = c.iter().map(|s| Job { src: ffmpeg.join(s), asm: false }).collect();
    jobs.push(Job { src: root.join("shim/pf_libav.c"), asm: false });
    jobs.extend(asm.iter().map(|s| Job { src: ffmpeg.join(s), asm: true }));
    let objects = run_jobs(&jobs, &compiler, &config, &ffmpeg, &out, &field("x86asmflags"))?;
    let mut archive = cc::Build::new();
    for o in &objects {
        archive.object(o);
    }
    archive.try_compile("avcodec").map_err(BuildError::Cc)?;

    println!("cargo:rustc-link-lib=m");
    println!("cargo:rustc-env=LIBAV_SYS_FFMPEG={release}");
    Ok(())
}

struct Job {
    src: PathBuf,
    asm: bool,
}

fn run_jobs(
    jobs: &[Job],
    cc: &cc::Tool,
    config: &Path,
    ffmpeg: &Path,
    out: &Path,
    asm_flags: &[String],
) -> Result<Vec<PathBuf>, BuildError> {
    let nasm = std::env::var("NASM").unwrap_or_else(|_| "nasm".to_string());
    if jobs.iter().any(|j| j.asm) {
        let ok = Command::new(&nasm).arg("-v").output().is_ok_and(|o| o.status.success());
        if !ok {
            return Err(BuildError::NasmMissing);
        }
    }
    let dir = out.join("obj");
    std::fs::create_dir_all(&dir).map_err(|err| BuildError::Io { path: dir.clone(), err })?;
    let object = |j: &Job| {
        let rel = j.src.strip_prefix(ffmpeg).unwrap_or(&j.src).to_string_lossy().replace('/', "_");
        dir.join(format!("{rel}.o"))
    };
    let compile = |j: &Job| -> Result<(), BuildError> {
        let mut cmd = if j.asm {
            let mut cmd = Command::new(&nasm);
            let here = j.src.parent().unwrap_or(ffmpeg);
            cmd.args(asm_flags)
                .arg(format!("-I{}/", config.display()))
                .arg(format!("-I{}/", ffmpeg.display()))
                .arg(format!("-I{}/", here.display()))
                .arg("-Pconfig.asm");
            cmd
        } else {
            let mut cmd = cc.to_command();
            cmd.arg("-c");
            cmd
        };
        let status = cmd
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
