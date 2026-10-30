//! Library search paths for libvpl and libva from pkg-config. A missing entry only warns.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-changed=build.rs");
    for pkg in ["vpl", "libva", "libva-drm"] {
        match Command::new("pkg-config").args(["--libs", pkg]).output() {
            Ok(out) if out.status.success() => {
                let flags = String::from_utf8_lossy(&out.stdout);
                for tok in flags.split_whitespace() {
                    if let Some(dir) = tok.strip_prefix("-L") {
                        println!("cargo:rustc-link-search=native={dir}");
                    }
                }
            }
            _ => println!(
                "cargo:warning=pkg-config could not resolve {pkg}, relying on default linker search paths"
            ),
        }
    }
}
