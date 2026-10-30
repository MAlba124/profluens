//! Builds libFLAC into a static library and checks the `src/lib.rs` constants and layouts
//! against its headers with a generated, never linked, C file of `_Static_assert`s.

// Build script, allocation is fine.
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

const FLAC_RELEASE: &str = "1.5.0";

const SRC_ENV: &str = "LIBFLAC_SRC";

const DECODER_SOURCES: &[&str] = &[
    "bitmath.c",
    "bitreader.c",
    "cpu.c",
    "crc.c",
    "fixed.c",
    "format.c",
    "lpc.c",
    "md5.c",
    "memory.c",
    "stream_decoder.c",
];

const ENCODER_SOURCES: &[&str] = &[
    "bitwriter.c",
    "float.c",
    "stream_encoder.c",
    "stream_encoder_framing.c",
    "window.c",
    "fixed_intrin_sse2.c",
    "fixed_intrin_ssse3.c",
    "fixed_intrin_sse42.c",
    "fixed_intrin_avx2.c",
    "lpc_intrin_sse2.c",
    "lpc_intrin_sse41.c",
    "lpc_intrin_avx2.c",
    "lpc_intrin_fma.c",
    "lpc_intrin_neon.c",
    "stream_encoder_intrin_sse2.c",
    "stream_encoder_intrin_ssse3.c",
    "stream_encoder_intrin_avx2.c",
];

const CONSTS: &[(&str, i64)] = &[
    ("sizeof(FLAC__bool)", 4),
    ("FLAC__MAX_CHANNELS", 8),
    ("FLAC__MAX_BLOCK_SIZE", 65535),
    ("FLAC__METADATA_TYPE_STREAMINFO", 0),
    ("FLAC__FRAME_NUMBER_TYPE_FRAME_NUMBER", 0),
    ("FLAC__FRAME_NUMBER_TYPE_SAMPLE_NUMBER", 1),
    ("FLAC__STREAM_DECODER_SEARCH_FOR_METADATA", 0),
    ("FLAC__STREAM_DECODER_READ_METADATA", 1),
    ("FLAC__STREAM_DECODER_SEARCH_FOR_FRAME_SYNC", 2),
    ("FLAC__STREAM_DECODER_READ_FRAME", 3),
    ("FLAC__STREAM_DECODER_END_OF_STREAM", 4),
    ("FLAC__STREAM_DECODER_OGG_ERROR", 5),
    ("FLAC__STREAM_DECODER_SEEK_ERROR", 6),
    ("FLAC__STREAM_DECODER_ABORTED", 7),
    ("FLAC__STREAM_DECODER_MEMORY_ALLOCATION_ERROR", 8),
    ("FLAC__STREAM_DECODER_UNINITIALIZED", 9),
    ("FLAC__STREAM_DECODER_END_OF_LINK", 10),
    ("FLAC__STREAM_DECODER_INIT_STATUS_OK", 0),
    ("FLAC__STREAM_DECODER_READ_STATUS_CONTINUE", 0),
    ("FLAC__STREAM_DECODER_READ_STATUS_END_OF_STREAM", 1),
    ("FLAC__STREAM_DECODER_READ_STATUS_ABORT", 2),
    ("FLAC__STREAM_DECODER_SEEK_STATUS_OK", 0),
    ("FLAC__STREAM_DECODER_SEEK_STATUS_ERROR", 1),
    ("FLAC__STREAM_DECODER_LENGTH_STATUS_UNSUPPORTED", 2),
    ("FLAC__STREAM_DECODER_TELL_STATUS_OK", 0),
    ("FLAC__STREAM_DECODER_TELL_STATUS_ERROR", 1),
    ("FLAC__STREAM_DECODER_WRITE_STATUS_CONTINUE", 0),
    ("FLAC__STREAM_DECODER_WRITE_STATUS_ABORT", 1),
    ("FLAC__STREAM_DECODER_ERROR_STATUS_LOST_SYNC", 0),
    ("FLAC__STREAM_DECODER_ERROR_STATUS_BAD_HEADER", 1),
    ("FLAC__STREAM_DECODER_ERROR_STATUS_FRAME_CRC_MISMATCH", 2),
    ("FLAC__STREAM_DECODER_ERROR_STATUS_UNPARSEABLE_STREAM", 3),
    ("FLAC__STREAM_DECODER_ERROR_STATUS_BAD_METADATA", 4),
    ("FLAC__STREAM_DECODER_ERROR_STATUS_OUT_OF_BOUNDS", 5),
    ("FLAC__STREAM_DECODER_ERROR_STATUS_MISSING_FRAME", 6),
    ("FLAC__STREAM_ENCODER_OK", 0),
    ("FLAC__STREAM_ENCODER_UNINITIALIZED", 1),
    ("FLAC__STREAM_ENCODER_MEMORY_ALLOCATION_ERROR", 8),
    ("FLAC__STREAM_ENCODER_INIT_STATUS_OK", 0),
    ("FLAC__STREAM_ENCODER_INIT_STATUS_NOT_STREAMABLE", 11),
    ("FLAC__STREAM_ENCODER_WRITE_STATUS_OK", 0),
    ("FLAC__STREAM_ENCODER_WRITE_STATUS_FATAL_ERROR", 1),
];

const LAYOUT: &[(&str, i64)] = &[
    ("sizeof(FLAC__FrameHeader)", 40),
    ("offsetof(FLAC__FrameHeader, blocksize)", 0),
    ("offsetof(FLAC__FrameHeader, sample_rate)", 4),
    ("offsetof(FLAC__FrameHeader, channels)", 8),
    ("offsetof(FLAC__FrameHeader, channel_assignment)", 12),
    ("offsetof(FLAC__FrameHeader, bits_per_sample)", 16),
    ("offsetof(FLAC__FrameHeader, number_type)", 20),
    ("offsetof(FLAC__FrameHeader, number)", 24),
    ("offsetof(FLAC__FrameHeader, crc)", 32),
    ("offsetof(FLAC__Frame, header)", 0),
    ("sizeof(FLAC__StreamMetadata_StreamInfo)", 56),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, min_blocksize)", 0),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, max_blocksize)", 4),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, min_framesize)", 8),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, max_framesize)", 12),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, sample_rate)", 16),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, channels)", 20),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, bits_per_sample)", 24),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, total_samples)", 32),
    ("offsetof(FLAC__StreamMetadata_StreamInfo, md5sum)", 40),
    ("offsetof(FLAC__StreamMetadata, type)", 0),
    ("offsetof(FLAC__StreamMetadata, is_last)", 4),
    ("offsetof(FLAC__StreamMetadata, length)", 8),
    ("offsetof(FLAC__StreamMetadata, data)", 16),
];

enum BuildError {
    Env(&'static str),
    NoSources { env: Option<PathBuf>, submodule: PathBuf },
    Missing(PathBuf),
    Release { root: PathBuf, found: String },
    Io { path: PathBuf, err: io::Error },
    Cc(cc::Error),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(var) => write!(f, "{var} is not set, run the build through cargo"),
            Self::NoSources { env: Some(dir), .. } => write!(
                f,
                "{SRC_ENV}={} holds no libFLAC sources (src/libFLAC/stream_decoder.c), point it at an unpacked flac-{FLAC_RELEASE} tree or unset it to use the submodule",
                dir.display()
            ),
            Self::NoSources { env: None, submodule } => write!(
                f,
                "the flac submodule is not checked out ({} is empty), run `git submodule update --init sys/libflac-sys/flac`, or set {SRC_ENV} to an unpacked flac-{FLAC_RELEASE} source tree",
                submodule.display()
            ),
            Self::Missing(p) => write!(f, "{} is missing from the libFLAC sources", p.display()),
            Self::Release { root, found } => write!(
                f,
                "the libFLAC sources in {} are FLAC {found}, this crate is for {FLAC_RELEASE}, check out the {FLAC_RELEASE} tag or point {SRC_ENV} at that release",
                root.display()
            ),
            Self::Io { path, err } => write!(f, "{}, {err}", path.display()),
            Self::Cc(e) => write!(f, "cc, {e}"),
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("libflac-sys: {e}");
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
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=config");
    println!("cargo:rerun-if-changed=flac");
    println!("cargo:rerun-if-env-changed={SRC_ENV}");

    let root = PathBuf::from(env("CARGO_MANIFEST_DIR")?);
    let out = PathBuf::from(env("OUT_DIR")?);
    let encoder = std::env::var_os("CARGO_FEATURE_ENCODER").is_some();
    let config = root.join("config");
    let flac = sources(&root)?;
    println!("cargo:rerun-if-changed={}", flac.display());
    let src = flac.join("src/libFLAC");

    let config_h = config.join("config.h");
    if !config_h.is_file() {
        return Err(BuildError::Missing(config_h));
    }
    // Both a git checkout and a tarball carry `project(FLAC VERSION x.y.z)` in CMakeLists.txt.
    let cmake = flac.join("CMakeLists.txt");
    if cmake.is_file() {
        let found = read(&cmake)?.lines().find_map(|l| {
            l.trim()
                .strip_prefix("project(FLAC VERSION ")
                .map(|v| v.split(')').next().unwrap_or("").trim().to_owned())
        });
        if let Some(found) = found.filter(|v| v != FLAC_RELEASE) {
            return Err(BuildError::Release { root: flac, found });
        }
    }

    let mut files: Vec<PathBuf> = DECODER_SOURCES.iter().map(|s| src.join(s)).collect();
    if encoder {
        files.extend(ENCODER_SOURCES.iter().map(|s| src.join(s)));
    }
    if let Some(gone) = files.iter().find(|f| !f.is_file()) {
        return Err(BuildError::Missing(gone.clone()));
    }

    let mut b = cc::Build::new();
    common(&mut b, &config, &flac);
    b.files(&files)
        .opt_level(3)
        .flag_if_supported("-fvisibility=hidden")
        // CMake's HAVE_ASSOC_MATH, read only by the encoder's float analysis.
        .flag_if_supported("-fassociative-math")
        .flag_if_supported("-fno-signed-zeros")
        .flag_if_supported("-fno-trapping-math")
        .flag_if_supported("-freciprocal-math");
    b.define("PACKAGE_VERSION", Some(format!("\"{FLAC_RELEASE}\"").as_str()));
    b.try_compile("FLAC").map_err(BuildError::Cc)?;
    println!("cargo:rustc-link-lib=m");

    check_consts(&out, &config, &flac)?;
    println!("cargo:rustc-env=LIBFLAC_SYS_RELEASE={FLAC_RELEASE}");
    Ok(())
}

fn sources(root: &Path) -> Result<PathBuf, BuildError> {
    let has = |dir: &Path| dir.join("src/libFLAC/stream_decoder.c").is_file();
    let submodule = root.join("flac");
    match std::env::var_os(SRC_ENV).map(PathBuf::from) {
        Some(dir) if has(&dir) => Ok(dir),
        Some(dir) => Err(BuildError::NoSources { env: Some(dir), submodule }),
        None if has(&submodule) => Ok(submodule),
        None => Err(BuildError::NoSources { env: None, submodule }),
    }
}

fn common(b: &mut cc::Build, config: &Path, flac: &Path) {
    b.include(config)
        .include(flac.join("include"))
        .include(flac.join("src/libFLAC/include"))
        .define("HAVE_CONFIG_H", None)
        .define("FLAC__NO_DLL", None)
        .define("NDEBUG", None)
        .warnings(false);
}

fn check_consts(out: &Path, config: &Path, flac: &Path) -> Result<(), BuildError> {
    let mut c = String::from(
        "/* generated by libflac-sys build.rs, the CONSTS and LAYOUT tables as C asserts */\n\
         #include <config.h>\n#include <stddef.h>\n#include \"FLAC/stream_decoder.h\"\n#include \"FLAC/stream_encoder.h\"\n",
    );
    for (expr, value) in CONSTS.iter().chain(LAYOUT) {
        c.push_str(&format!("_Static_assert({expr} == {value}, \"{expr}\");\n"));
    }
    let path = out.join("consts_check.c");
    std::fs::write(&path, c).map_err(|err| BuildError::Io { path: path.clone(), err })?;
    let mut b = cc::Build::new();
    common(&mut b, config, flac);
    b.file(&path).cargo_metadata(false);
    b.try_compile_intermediates().map_err(BuildError::Cc)?;
    Ok(())
}
