//! Points the linker at libva via pkg-config. The `#[link]` attributes in `src/ffi.rs` emit the
//! `-l` flags.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-changed=build.rs");

    match Command::new("pkg-config").args(["--libs", "libva", "libva-drm"]).output() {
        Ok(out) if out.status.success() => {
            let flags = String::from_utf8_lossy(&out.stdout);
            for tok in flags.split_whitespace() {
                if let Some(dir) = tok.strip_prefix("-L") {
                    println!("cargo:rustc-link-search=native={dir}");
                }
                // `-l` comes from the #[link] attributes, emitting it here would double-link.
            }
        }
        _ => {
            println!(
                "cargo:warning=pkg-config could not resolve libva/libva-drm; relying on \
                 default linker search paths (set PKG_CONFIG_PATH or install libva-dev)"
            );
        }
    }
}
