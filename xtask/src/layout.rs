//! `layout <which>`, struct layouts as the C compiler lays them out, from a gen/ program.

use std::path::Path;
use std::process::Command;

use crate::error::Error;
use crate::util;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    VaapiAv1,
    VaapiDav1d,
    Vpl,
}

impl Layout {
    pub fn parse(name: &str) -> Option<Layout> {
        match name {
            "vaapi-av1" => Some(Layout::VaapiAv1),
            "vaapi-dav1d" => Some(Layout::VaapiDav1d),
            "vpl" => Some(Layout::Vpl),
            _ => None,
        }
    }
}

fn cflags(pkg: &str) -> Result<Vec<String>, Error> {
    let out = util::capture("pkg-config", Command::new("pkg-config").args(["--cflags", pkg]))?;
    Ok(String::from_utf8_lossy(&out).split_whitespace().map(str::to_string).collect())
}

pub fn generate(root: &Path, which: Layout) -> Result<(), Error> {
    let tmp = util::temp_dir("layout")?;
    let bin = tmp.path.join("gen");
    let mut cc = Command::new("cc");
    let (src, out) = match which {
        Layout::VaapiAv1 => {
            cc.arg("-Wno-overflow").args(cflags("libva")?);
            ("plugins/hw/vaapi/gen/av1_layout.c", "plugins/hw/vaapi/src/av1enc/layout.rs")
        }
        Layout::VaapiDav1d => {
            let inc = root.join("sys/libdav1d-sys/dav1d/include");
            if !inc.join("dav1d/headers.h").is_file() {
                return Err(Error::Missing(format!(
                    "{} is empty, run git submodule update --init sys/libdav1d-sys/dav1d",
                    inc.display()
                )));
            }
            cc.arg(format!("-I{}", inc.display()));
            ("plugins/hw/vaapi/gen/dav1d_layout.c", "plugins/hw/vaapi/tests/common/dav1d_layout.rs")
        }
        Layout::Vpl => {
            cc.args(["-O1", "-Wno-deprecated-declarations"]).args(cflags("vpl")?);
            ("plugins/hw/vpl/gen/layout.c", "plugins/hw/vpl/src/layout.rs")
        }
    };
    util::run("cc", cc.arg(root.join(src)).arg("-o").arg(&bin))?;
    // Written only once the generator succeeded, a failure leaves the old file.
    let text = util::capture("layout generator", &mut Command::new(&bin))?;
    util::write(&root.join(out), &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_names() {
        assert_eq!(Layout::parse("vaapi-av1"), Some(Layout::VaapiAv1));
        assert_eq!(Layout::parse("vaapi-dav1d"), Some(Layout::VaapiDav1d));
        assert_eq!(Layout::parse("vpl"), Some(Layout::Vpl));
        assert_eq!(Layout::parse("vaapi"), None);
    }
}
