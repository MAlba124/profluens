//! Workspace tasks, run as `cargo xtask <command>`.
// A dev tool, the element IO and allocation bans do not apply.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod av1;
mod error;
mod fixtures;
mod layout;
mod libav;
mod ps;
mod seeds;
mod streams;
mod util;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use error::Error;
use layout::Layout;

const USAGE: &str = "usage: cargo xtask <command> [args]

commands:
  fixtures                  the pfplay corpus into fixtures/out
                            (FORCE=1 rebuilds, FFMPEG and FFPROBE pick the tools)
  fixtures-opus             plugins/audio/opus/tests/fixtures/sines_5_1.mka
  fixtures-ts <out>         the ts fixtures and their ffprobe oracles into <out>
  fixtures-tsdemux <out>    the tsdemux fixture into <out>
  fixtures-ps <out>         the program stream fixtures and their ffprobe oracles into <out>
  fixtures-flv <out>        the flv fixtures and their ffprobe packet tables into <out>
  fixtures-av1              the AV1 streams, their per-frame MD5s and VERSIONS
                            (nix and ffmpeg)
  seeds <out>               demuxer fuzz seed corpora into <out>/<format>_demux
                            (ffmpeg on PATH, else only the committed fixtures)
  layout <vaapi-av1|vaapi-dav1d|vpl>
                            regenerate a struct layout .rs file from its C generator
  libav-config <target>     sys/libav-sys/config/<target> from the FFmpeg submodule
  help                      this text
";

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Help,
    Fixtures,
    FixturesOpus,
    FixturesTs(PathBuf),
    FixturesTsdemux(PathBuf),
    FixturesPs(PathBuf),
    FixturesFlv(PathBuf),
    FixturesAv1,
    Seeds(PathBuf),
    Layout(Layout),
    LibavConfig(String),
}

fn usage(msg: &str) -> Error {
    Error::Usage(msg.to_string())
}

fn one(cmd: &str, rest: &[OsString]) -> Result<OsString, Error> {
    match rest {
        [arg] => Ok(arg.clone()),
        [] => Err(usage(&format!("{cmd} takes one argument"))),
        _ => Err(usage(&format!("{cmd} takes one argument, got {}", rest.len()))),
    }
}

fn none(cmd: &str, rest: &[OsString], value: Cmd) -> Result<Cmd, Error> {
    if rest.is_empty() { Ok(value) } else { Err(usage(&format!("{cmd} takes no arguments"))) }
}

fn parse(args: &[OsString]) -> Result<Cmd, Error> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Cmd::Help);
    };
    let Some(cmd) = first.to_str() else {
        return Err(usage(&format!("unknown command {}", first.to_string_lossy())));
    };
    match cmd {
        "help" | "-h" | "--help" => Ok(Cmd::Help),
        "fixtures" => none(cmd, rest, Cmd::Fixtures),
        "fixtures-opus" => none(cmd, rest, Cmd::FixturesOpus),
        "fixtures-av1" => none(cmd, rest, Cmd::FixturesAv1),
        "fixtures-ts" => Ok(Cmd::FixturesTs(one(cmd, rest)?.into())),
        "fixtures-tsdemux" => Ok(Cmd::FixturesTsdemux(one(cmd, rest)?.into())),
        "fixtures-ps" => Ok(Cmd::FixturesPs(one(cmd, rest)?.into())),
        "fixtures-flv" => Ok(Cmd::FixturesFlv(one(cmd, rest)?.into())),
        "seeds" => Ok(Cmd::Seeds(one(cmd, rest)?.into())),
        "layout" => {
            let which = one(cmd, rest)?;
            let name = which.to_string_lossy();
            Layout::parse(&name)
                .map(Cmd::Layout)
                .ok_or_else(|| usage(&format!("unknown layout {name}")))
        }
        "libav-config" => {
            let target = one(cmd, rest)?;
            target.into_string().map(Cmd::LibavConfig).map_err(|_| usage("the target is not UTF-8"))
        }
        _ => Err(usage(&format!("unknown command {cmd}"))),
    }
}

fn run(cmd: Cmd) -> Result<(), Error> {
    let root = util::root();
    match cmd {
        Cmd::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Cmd::Fixtures => fixtures::corpus(&root),
        Cmd::FixturesOpus => streams::opus(&root),
        Cmd::FixturesTs(out) => streams::ts(&out),
        Cmd::FixturesTsdemux(out) => streams::tsdemux(&out),
        Cmd::FixturesPs(out) => ps::fixtures(&out),
        Cmd::FixturesFlv(out) => streams::flv(&out),
        Cmd::FixturesAv1 => av1::fixtures(&root),
        Cmd::Seeds(out) => seeds::seeds(&root, &out),
        Cmd::Layout(which) => layout::generate(&root, which),
        Cmd::LibavConfig(target) => libav::config(&root, &target),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse(&args).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(msg)) => {
            eprintln!("xtask: {msg}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_is_help() {
        assert_eq!(parse(&[]).ok(), Some(Cmd::Help));
        assert_eq!(parse(&args(&["help"])).ok(), Some(Cmd::Help));
        assert_eq!(parse(&args(&["--help"])).ok(), Some(Cmd::Help));
    }

    #[test]
    fn commands_take_their_arguments() {
        assert_eq!(parse(&args(&["fixtures"])).ok(), Some(Cmd::Fixtures));
        assert_eq!(parse(&args(&["seeds", "out"])).ok(), Some(Cmd::Seeds("out".into())));
        assert_eq!(parse(&args(&["fixtures-ts", "/t"])).ok(), Some(Cmd::FixturesTs("/t".into())));
        assert_eq!(parse(&args(&["fixtures-ps", "/p"])).ok(), Some(Cmd::FixturesPs("/p".into())));
        assert_eq!(parse(&args(&["fixtures-flv", "/f"])).ok(), Some(Cmd::FixturesFlv("/f".into())));
        assert_eq!(parse(&args(&["layout", "vpl"])).ok(), Some(Cmd::Layout(Layout::Vpl)));
        assert_eq!(
            parse(&args(&["libav-config", "x86_64-unknown-linux-gnu"])).ok(),
            Some(Cmd::LibavConfig("x86_64-unknown-linux-gnu".into()))
        );
    }

    #[test]
    fn bad_command_lines_are_usage_errors() {
        let is_usage = |a: &[&str]| matches!(parse(&args(a)), Err(Error::Usage(_)));
        assert!(is_usage(&["seeds"]));
        assert!(is_usage(&["seeds", "a", "b"]));
        assert!(is_usage(&["fixtures", "x"]));
        assert!(is_usage(&["layout", "nope"]));
        assert!(is_usage(&["frobnicate"]));
    }
}
