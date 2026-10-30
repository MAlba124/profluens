//! QuickTime and ISO/IEC 23003-5 uncompressed audio against ffmpeg's decode of the same file,
//! byte for byte after the reframer. Skips if ffmpeg is absent.

// Test setup, not an element hot path.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::process::Command;

use pf_mp4::Mp4Reader;

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| PathBuf::from(dir).join(tool))
        .find(|p| p.is_file())
}

fn ffmpeg(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(which("ffmpeg")?)
        .env_remove("LD_LIBRARY_PATH")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Some(out.stdout)
}

/// Two seconds of stereo at 48 kHz in `codec`, so a chunk holds more than one group. `test`
/// keeps the tests running in parallel off each other's files.
fn make(test: &str, codec: &str, ext: &str) -> Option<(PathBuf, Vec<u8>)> {
    let path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("pf_mp4_{test}_{codec}.{ext}"));
    let src = "sine=frequency=440:sample_rate=48000:duration=2";
    ffmpeg(&["-f", "lavfi", "-i", src, "-ac", "2", "-c:a", codec, path.to_str()?])?;
    let bytes = std::fs::read(&path).expect("read the made file");
    Some((path, bytes))
}

/// The track's samples reframed and joined, the sample format and how many samples.
fn demux(file: &[u8]) -> (Vec<u8>, &'static str, usize) {
    let reader = Mp4Reader::new(file).expect("resolves");
    let tables = reader.tables();
    let track = &tables.tracks()[0];
    let pcm = track.entry.pcm.expect("an audio/raw track");
    assert_eq!(track.family(), "audio/raw");
    let (mut pcm_out, mut scratch) = (Vec::new(), Vec::new());
    for s in tables.samples() {
        let block = &file[s.offset as usize..][..s.size as usize];
        pcm_out
            .extend_from_slice(track.reframer().reframe_into(block, &mut scratch).expect("whole"));
    }
    (pcm_out, pcm.sample, tables.samples().len())
}

#[test]
fn every_pcm_entry_matches_ffmpeg() {
    let cases = [
        ("pcm_s16be", "mov", "s16le", "s16"),
        ("pcm_s16le", "mov", "s16le", "s16"),
        ("pcm_s24be", "mov", "s24le", "s24"),
        ("pcm_s24le", "mov", "s24le", "s24"),
        ("pcm_s32be", "mov", "s32le", "s32"),
        ("pcm_s32le", "mov", "s32le", "s32"),
        ("pcm_f32be", "mov", "f32le", "f32"),
        ("pcm_f32le", "mov", "f32le", "f32"),
        ("pcm_u8", "mov", "u8", "u8"),
        ("pcm_s8", "mov", "u8", "u8"),
        ("pcm_f64be", "mov", "f32le", "f32"),
        ("pcm_f64le", "mov", "f32le", "f32"),
        ("pcm_s16be", "mp4", "s16le", "s16"),
        ("pcm_s24le", "mp4", "s24le", "s24"),
        ("pcm_f32le", "mp4", "f32le", "f32"),
    ];
    for (codec, ext, raw, sample) in cases {
        let Some((path, bytes)) = make("every", codec, ext) else { return };
        let want =
            ffmpeg(&["-i", path.to_str().unwrap(), "-f", raw, "-c:a", &format!("pcm_{raw}"), "-"])
                .expect("ffmpeg decodes what it made");
        let (got, named, samples) = demux(&bytes);
        assert_eq!(named, sample, "{codec}.{ext}");
        assert!(got == want, "{codec}.{ext}: {} bytes against {}", got.len(), want.len());
        assert!(
            samples <= 96_000 / 1024 + 2,
            "{codec}.{ext}: {samples} samples, frames not grouped"
        );
    }
}

#[test]
fn an_old_quicktime_sample_size_of_one_counts_frames() {
    let Some((path, mut bytes)) = make("stsz", "pcm_s16be", "mov") else { return };
    let want = demux(&bytes).0;
    let at = bytes.windows(4).position(|w| w == b"stsz").expect("an stsz") + 8;
    bytes[at..at + 4].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(demux(&bytes).0, want, "{}", path.display());
}
