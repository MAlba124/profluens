//! Builds the dav1d submodule with meson and ninja into a static library and checks the
//! bindings against its headers with generated `_Static_assert`s.

// Build script, allocation is fine.
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

const DAV1D_VERSION: &str = "1.5.4";

const API_MAJOR: u32 = 7;

const MESON_OPTIONS: &[&str] = &[
    "--default-library=static",
    "--buildtype=release",
    "--wrap-mode=nodownload",
    "-Db_staticpic=true",
    "-Dbitdepths=8,16",
    "-Denable_asm=true",
    "-Denable_tools=false",
    "-Denable_tests=false",
    "-Denable_examples=false",
    "-Denable_docs=false",
    "-Dxxhash_muxer=disabled",
];

enum BuildError {
    Env(&'static str),
    PointerWidth(String),
    CrossCompile { host: String, target: String },
    SubmoduleMissing(PathBuf),
    ToolMissing { tool: &'static str, why: &'static str },
    Spawn { tool: &'static str, err: io::Error },
    Failed { step: &'static str, status: ExitStatus },
    Io { path: PathBuf, err: io::Error },
    PcField { path: PathBuf, field: &'static str },
    PcToken { path: PathBuf, token: String },
    Version { found: String },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(var) => write!(f, "{var} is not set, run the build through cargo"),
            Self::PointerWidth(w) => {
                write!(f, "the LAYOUT table is for 64-bit targets, this one is {w}-bit")
            }
            Self::CrossCompile { host, target } => {
                write!(
                    f,
                    "cross builds ({host} to {target}) need a meson cross file, which is not wired"
                )
            }
            Self::SubmoduleMissing(p) => write!(
                f,
                "the dav1d submodule is not checked out ({} is missing), run `git submodule update --init`",
                p.display()
            ),
            Self::ToolMissing { tool, why } => {
                write!(f, "`{tool}` is not on the path, it {why} (the nix devshell has it)")
            }
            Self::Spawn { tool, err } => write!(f, "running `{tool}` failed, {err}"),
            Self::Failed { step, status } => {
                write!(f, "{step} failed with {status}, its output is above")
            }
            Self::Io { path, err } => write!(f, "{}, {err}", path.display()),
            Self::PcField { path, field } => write!(f, "{} has no `{field}` line", path.display()),
            Self::PcToken { path, token } => {
                write!(
                    f,
                    "{} lists `{token}` in its libs, which build.rs does not map",
                    path.display()
                )
            }
            Self::Version { found } => write!(
                f,
                "the dav1d submodule is at {found}, build.rs pins {DAV1D_VERSION}, run `git submodule update --init` or bump DAV1D_VERSION"
            ),
        }
    }
}

/// `(C type, Rust const stem, field or None for sizeof, bytes)` on an LP64 target.
const LAYOUT: &[(&str, &str, Option<&str>, usize)] = &[
    ("Dav1dDataProps", "DAV1D_DATA_PROPS", None, 48),
    ("Dav1dDataProps", "DAV1D_DATA_PROPS", Some("timestamp"), 0),
    ("Dav1dDataProps", "DAV1D_DATA_PROPS", Some("duration"), 8),
    ("Dav1dDataProps", "DAV1D_DATA_PROPS", Some("offset"), 16),
    ("Dav1dDataProps", "DAV1D_DATA_PROPS", Some("size"), 24),
    ("Dav1dDataProps", "DAV1D_DATA_PROPS", Some("user_data"), 32),
    ("Dav1dData", "DAV1D_DATA", None, 72),
    ("Dav1dData", "DAV1D_DATA", Some("data"), 0),
    ("Dav1dData", "DAV1D_DATA", Some("sz"), 8),
    ("Dav1dData", "DAV1D_DATA", Some("ref"), 16),
    ("Dav1dData", "DAV1D_DATA", Some("m"), 24),
    ("Dav1dPictureParameters", "DAV1D_PICTURE_PARAMETERS", None, 16),
    ("Dav1dPictureParameters", "DAV1D_PICTURE_PARAMETERS", Some("w"), 0),
    ("Dav1dPictureParameters", "DAV1D_PICTURE_PARAMETERS", Some("h"), 4),
    ("Dav1dPictureParameters", "DAV1D_PICTURE_PARAMETERS", Some("layout"), 8),
    ("Dav1dPictureParameters", "DAV1D_PICTURE_PARAMETERS", Some("bpc"), 12),
    ("Dav1dPicture", "DAV1D_PICTURE", None, 272),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("seq_hdr"), 0),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("frame_hdr"), 8),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("data"), 16),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("stride"), 40),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("p"), 56),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("m"), 72),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("content_light"), 120),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("mastering_display"), 128),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("itut_t35"), 136),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("n_itut_t35"), 144),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("reserved"), 152),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("frame_hdr_ref"), 184),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("seq_hdr_ref"), 192),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("content_light_ref"), 200),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("mastering_display_ref"), 208),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("itut_t35_ref"), 216),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("reserved_ref"), 224),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("ref"), 256),
    ("Dav1dPicture", "DAV1D_PICTURE", Some("allocator_data"), 264),
    ("Dav1dPicAllocator", "DAV1D_PIC_ALLOCATOR", None, 24),
    ("Dav1dPicAllocator", "DAV1D_PIC_ALLOCATOR", Some("cookie"), 0),
    ("Dav1dPicAllocator", "DAV1D_PIC_ALLOCATOR", Some("alloc_picture_callback"), 8),
    ("Dav1dPicAllocator", "DAV1D_PIC_ALLOCATOR", Some("release_picture_callback"), 16),
    ("Dav1dLogger", "DAV1D_LOGGER", None, 16),
    ("Dav1dLogger", "DAV1D_LOGGER", Some("cookie"), 0),
    ("Dav1dLogger", "DAV1D_LOGGER", Some("callback"), 8),
    ("Dav1dSettings", "DAV1D_SETTINGS", None, 96),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("n_threads"), 0),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("max_frame_delay"), 4),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("apply_grain"), 8),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("operating_point"), 12),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("all_layers"), 16),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("frame_size_limit"), 20),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("allocator"), 24),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("logger"), 48),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("strict_std_compliance"), 64),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("output_invisible_frames"), 68),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("inloop_filters"), 72),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("decode_frame_type"), 76),
    ("Dav1dSettings", "DAV1D_SETTINGS", Some("reserved"), 80),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("profile"), 0),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("max_width"), 4),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("max_height"), 8),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("layout"), 12),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("pri"), 16),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("trc"), 20),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("mtrx"), 24),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("chr"), 28),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("hbd"), 32),
    ("Dav1dSequenceHeader", "DAV1D_SEQUENCE_HEADER", Some("color_range"), 33),
    ("Dav1dContentLightLevel", "DAV1D_CONTENT_LIGHT_LEVEL", None, 4),
    (
        "Dav1dContentLightLevel",
        "DAV1D_CONTENT_LIGHT_LEVEL",
        Some("max_frame_average_light_level"),
        2,
    ),
    ("Dav1dMasteringDisplay", "DAV1D_MASTERING_DISPLAY", None, 24),
    ("Dav1dMasteringDisplay", "DAV1D_MASTERING_DISPLAY", Some("white_point"), 12),
    ("Dav1dMasteringDisplay", "DAV1D_MASTERING_DISPLAY", Some("max_luminance"), 16),
    ("Dav1dMasteringDisplay", "DAV1D_MASTERING_DISPLAY", Some("min_luminance"), 20),
];

const ERRNO: &[(&str, &str, i32, i32)] = &[
    // name, Rust const stem, linux value, macOS value
    ("EAGAIN", "EAGAIN", 11, 35),
    ("EINVAL", "EINVAL", 22, 22),
    ("ENOMEM", "ENOMEM", 12, 12),
    ("ENOENT", "ENOENT", 2, 2),
    ("ERANGE", "ERANGE", 34, 34),
    ("ENOPROTOOPT", "ENOPROTOOPT", 92, 42),
];

fn main() {
    if let Err(e) = run() {
        eprintln!("libdav1d-sys: {e}");
        std::process::exit(1);
    }
}

fn env(var: &'static str) -> Result<String, BuildError> {
    std::env::var(var).map_err(|_| BuildError::Env(var))
}

fn run() -> Result<(), BuildError> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=dav1d");

    let out = PathBuf::from(env("OUT_DIR")?);
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR")?);
    let os = env("CARGO_CFG_TARGET_OS")?;
    let arch = env("CARGO_CFG_TARGET_ARCH")?;
    let pointer_width = env("CARGO_CFG_TARGET_POINTER_WIDTH")?;
    let jobs = env("NUM_JOBS")?;
    let (host, target) = (env("HOST")?, env("TARGET")?);
    if pointer_width != "64" {
        return Err(BuildError::PointerWidth(pointer_width));
    }
    if host != target {
        return Err(BuildError::CrossCompile { host, target });
    }

    let src = manifest.join("dav1d");
    if !src.join("meson.build").is_file() {
        return Err(BuildError::SubmoduleMissing(src.join("meson.build")));
    }
    probe("meson", "--version", "configures dav1d")?;
    probe("ninja", "--version", "builds dav1d")?;
    if arch == "x86_64" || arch == "x86" {
        probe("nasm", "-v", "assembles dav1d's x86 routines")?;
    }

    let build = out.join("build");
    let mut setup = Command::new("meson");
    setup.arg("setup");
    if build.join("meson-private").join("coredata.dat").is_file() {
        setup.arg("--reconfigure");
    }
    setup.arg(&build).arg(&src).args(MESON_OPTIONS);
    exec(setup, "meson", "meson setup")?;

    let pc_path = build.join("meson-private").join("dav1d.pc");
    let pc = std::fs::read_to_string(&pc_path)
        .map_err(|err| BuildError::Io { path: pc_path.clone(), err })?;
    let version = pc_field(&pc, &pc_path, "Version")?;
    if version != DAV1D_VERSION {
        return Err(BuildError::Version { found: version.to_owned() });
    }

    let mut ninja = Command::new("ninja");
    ninja.arg("-C").arg(&build).arg("-j").arg(&jobs);
    exec(ninja, "ninja", "ninja")?;

    println!("cargo:rustc-link-search=native={}", build.join("src").display());
    println!("cargo:rustc-link-lib=static=dav1d");
    // A static-only build lists the system libs in `Libs`, a shared one in `Libs.private`.
    let private = pc_field(&pc, &pc_path, "Libs.private").unwrap_or("");
    for token in
        pc_field(&pc, &pc_path, "Libs")?.split_whitespace().chain(private.split_whitespace())
    {
        if token == "-ldav1d" || token.starts_with("-L${") {
            continue;
        }
        if token == "-pthread" {
            println!("cargo:rustc-link-lib=pthread");
        } else if let Some(lib) = token.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={lib}");
        } else if let Some(dir) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else {
            return Err(BuildError::PcToken { path: pc_path, token: token.to_owned() });
        }
    }
    println!("cargo:rustc-env=LIBDAV1D_SYS_VERSION={version}");

    write_layout_check(&out, &os)?;
    cc::Build::new()
        .file(out.join("layout_check.c"))
        .include(src.join("include"))
        .include(build.join("include"))
        .cargo_metadata(false)
        .warnings(false)
        .compile_intermediates();
    Ok(())
}

fn probe(tool: &'static str, version_arg: &str, why: &'static str) -> Result<(), BuildError> {
    match Command::new(tool).arg(version_arg).output() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(BuildError::ToolMissing { tool, why }),
        Err(err) => Err(BuildError::Spawn { tool, err }),
    }
}

fn exec(mut cmd: Command, tool: &'static str, step: &'static str) -> Result<(), BuildError> {
    let status = cmd.status().map_err(|err| BuildError::Spawn { tool, err })?;
    if status.success() { Ok(()) } else { Err(BuildError::Failed { step, status }) }
}

fn pc_field<'a>(pc: &'a str, path: &Path, field: &'static str) -> Result<&'a str, BuildError> {
    pc.lines()
        .find_map(|l| l.strip_prefix(field).and_then(|r| r.strip_prefix(':')))
        .map(str::trim)
        .ok_or_else(|| BuildError::PcField { path: path.to_owned(), field })
}

fn write_layout_check(out: &Path, os: &str) -> Result<(), BuildError> {
    let mut c = String::from(
        "/* generated by libdav1d-sys build.rs, the LAYOUT and ERRNO tables as C asserts */\n\
         #include <errno.h>\n#include <stddef.h>\n#include \"dav1d/dav1d.h\"\n",
    );
    c.push_str(&format!(
        "_Static_assert(DAV1D_API_VERSION_MAJOR == {API_MAJOR}, \"dav1d API major {API_MAJOR}\");\n"
    ));
    let mut rs = String::from(
        "// generated by libdav1d-sys build.rs, the LAYOUT and ERRNO tables as Rust consts\n",
    );
    for (ty, stem, field, value) in LAYOUT {
        match field {
            None => {
                c.push_str(&format!("_Static_assert(sizeof({ty}) == {value}, \"sizeof {ty}\");\n"));
                rs.push_str(&format!("pub const SIZEOF_{stem}: usize = {value};\n"));
            }
            Some(f) => {
                c.push_str(&format!(
                    "_Static_assert(offsetof({ty}, {f}) == {value}, \"offsetof {ty}.{f}\");\n"
                ));
                rs.push_str(&format!(
                    "pub const OFFSET_{stem}_{}: usize = {value};\n",
                    f.to_uppercase()
                ));
            }
        }
    }
    for (name, stem, linux, macos) in ERRNO {
        let value = if os == "macos" { macos } else { linux };
        c.push_str(&format!("_Static_assert({name} == {value}, \"errno {name}\");\n"));
        rs.push_str(&format!("pub const {stem}: i32 = {value};\n"));
    }
    for (name, text) in [("layout_check.c", c), ("layout.rs", rs)] {
        let path = out.join(name);
        std::fs::write(&path, text).map_err(|err| BuildError::Io { path, err })?;
    }
    Ok(())
}
