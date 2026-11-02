//! VA-API capability probe, cached for the process. [`configure`] before the first probe decides
//! whether it runs and on which node. Without it, `PF_NO_VAAPI=1` disables the probe and
//! `PF_VAAPI_DEVICE=/dev/dri/renderDNNN` pins the node.

// One-time probing, cached in a OnceLock.
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::ffi;
use crate::va::Display;

#[derive(Debug, Clone, Copy)]
pub struct ProfileCaps {
    pub name: &'static str,
    pub vld: bool,
    pub enc: bool,
    pub enc_lp: bool,
    /// The decoder scales in the decode call (`VAConfigAttribDecProcessing`).
    pub dec_proc: bool,
    /// What an encoder on EncSlice and on EncSliceLP takes, default where there is none.
    pub enc_refs: [EncRefs; 2],
}

/// The references an encode entrypoint takes a picture, and whether its inter pictures are low
/// delay B (VA_PREDICTION_DIRECTION_BI_NOT_EMPTY).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncRefs {
    pub l0: u32,
    pub l1: u32,
    pub gpb: bool,
}

impl EncRefs {
    pub(crate) fn query(display: &Display, profile: ffi::VAProfile, entrypoint: ffi::VAEntrypoint) -> EncRefs {
        let (l0, l1) = crate::enc::max_refs(display, profile, entrypoint);
        let direction = ffi::VAConfigAttribPredictionDirection;
        let gpb = crate::va::config_attribute(display, profile, entrypoint, direction)
            .ok()
            .flatten()
            .is_some_and(|v| v & ffi::VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0);
        EncRefs { l0, l1, gpb }
    }
}

#[derive(Debug, Clone)]
pub struct VaCaps {
    pub device: PathBuf,
    pub vendor: String,
    pub version: (i32, i32),
    pub decode_families: Vec<&'static str>,
    pub encode_families: Vec<&'static str>,
    pub profiles: Vec<ProfileCaps>,
    pub video_proc: bool,
}

impl VaCaps {
    pub fn supports(&self, family: &str) -> bool {
        self.decode_families.contains(&family)
    }

    pub fn supports_encode(&self, family: &str) -> bool {
        self.encode_families.contains(&family)
    }

    pub fn decodes_profile(&self, name: &str) -> bool {
        self.profiles.iter().any(|p| p.name == name && p.vld)
    }

    /// Whether every profile of `family` the hardware decodes also scales in the decode call.
    pub fn shrinks(&self, family: &str) -> bool {
        let mut decoded = self.profiles.iter().filter(|p| p.vld && family_named(p.name) == Some(family)).peekable();
        decoded.peek().is_some() && decoded.all(|p| p.dec_proc)
    }
}

/// What the probe takes in place of the environment: `disabled` skips it, `device` pins the
/// render node, `None` scans `/dev/dri`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeConfig {
    pub disabled: bool,
    pub device: Option<PathBuf>,
}

/// Why [`configure`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigureError {
    /// The probe already ran, its result is cached for the process.
    Probed,
    /// A configuration was already set.
    Configured,
}

impl fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ConfigureError::Probed => "the VA-API probe already ran",
            ConfigureError::Configured => "the VA-API probe is already configured",
        })
    }
}

impl std::error::Error for ConfigureError {}

static CONFIG: OnceLock<ProbeConfig> = OnceLock::new();
static CAPS: OnceLock<Option<VaCaps>> = OnceLock::new();

/// Set once, before the first [`probe`], and then in place of `PF_NO_VAAPI` and
/// `PF_VAAPI_DEVICE`.
pub fn configure(config: ProbeConfig) -> Result<(), ConfigureError> {
    if CAPS.get().is_some() {
        return Err(ConfigureError::Probed);
    }
    CONFIG.set(config).map_err(|_| ConfigureError::Configured)
}

pub fn probe() -> Option<&'static VaCaps> {
    CAPS.get_or_init(probe_uncached).as_ref()
}

/// The configuration, else the environment: whether to skip, and a pinned node.
fn choice(config: Option<&ProbeConfig>) -> (bool, Option<PathBuf>) {
    match config {
        Some(c) => (c.disabled, c.device.clone()),
        None => {
            let disabled = std::env::var("PF_NO_VAAPI").is_ok_and(|v| v == "1");
            let device = std::env::var("PF_VAAPI_DEVICE").ok().filter(|d| !d.is_empty());
            (disabled, device.map(PathBuf::from))
        }
    }
}

fn candidate_nodes(pinned: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(dev) = pinned {
        return vec![dev];
    }
    let mut nodes = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev/dri") {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = name.strip_prefix("renderD")
                && num.chars().all(|c| c.is_ascii_digit())
            {
                nodes.push(entry.path());
            }
        }
    }
    nodes.sort();
    nodes
}

fn probe_uncached() -> Option<VaCaps> {
    let (disabled, pinned) = choice(CONFIG.get());
    if disabled {
        return None;
    }
    for node in candidate_nodes(pinned) {
        if let Ok(display) = Display::open(&node) {
            return Some(query_caps(&display, node));
        }
    }
    None
}

fn family_of(profile: ffi::VAProfile) -> Option<&'static str> {
    match profile {
        ffi::VAProfileH264Main | ffi::VAProfileH264High | ffi::VAProfileH264ConstrainedBaseline => {
            Some("h264/annexb")
        }
        ffi::VAProfileHEVCMain | ffi::VAProfileHEVCMain10 | ffi::VAProfileHEVCMain12 => {
            Some("h265/annexb")
        }
        ffi::VAProfileVP8Version0_3 => Some("vp8"),
        ffi::VAProfileVP9Profile0 | ffi::VAProfileVP9Profile2 => Some("vp9"),
        ffi::VAProfileAV1Profile0 => Some("av1"),
        _ => None,
    }
}

fn family_named(name: &str) -> Option<&'static str> {
    match name {
        "H264ConstrainedBaseline" | "H264Main" | "H264High" => Some("h264/annexb"),
        "HEVCMain" | "HEVCMain10" | "HEVCMain12" => Some("h265/annexb"),
        "VP8Version0_3" => Some("vp8"),
        "VP9Profile0" | "VP9Profile2" => Some("vp9"),
        "AV1Profile0" => Some("av1"),
        _ => None,
    }
}

fn profile_name(profile: ffi::VAProfile) -> &'static str {
    match profile {
        ffi::VAProfileH264ConstrainedBaseline => "H264ConstrainedBaseline",
        ffi::VAProfileH264Main => "H264Main",
        ffi::VAProfileH264High => "H264High",
        ffi::VAProfileVP8Version0_3 => "VP8Version0_3",
        ffi::VAProfileHEVCMain => "HEVCMain",
        ffi::VAProfileHEVCMain10 => "HEVCMain10",
        ffi::VAProfileHEVCMain12 => "HEVCMain12",
        ffi::VAProfileVP9Profile0 => "VP9Profile0",
        ffi::VAProfileVP9Profile2 => "VP9Profile2",
        ffi::VAProfileAV1Profile0 => "AV1Profile0",
        _ => "other",
    }
}

fn query_caps(display: &Display, device: PathBuf) -> VaCaps {
    let mut decode_families: Vec<&'static str> = Vec::new();
    let mut encode_families: Vec<&'static str> = Vec::new();
    let mut profiles: Vec<ProfileCaps> = Vec::new();
    let mut video_proc = false;

    // SAFETY: display is live, buffers are sized by the driver-declared maxima.
    unsafe {
        let max_profiles = ffi::vaMaxNumProfiles(display.raw()).max(0) as usize;
        let mut profile_list = vec![ffi::VAProfileNone; max_profiles];
        let mut num_profiles: std::ffi::c_int = 0;
        if ffi::vaQueryConfigProfiles(display.raw(), profile_list.as_mut_ptr(), &mut num_profiles)
            != ffi::VA_STATUS_SUCCESS
        {
            return VaCaps {
                device,
                vendor: display.vendor(),
                version: display.version(),
                decode_families,
                encode_families,
                profiles,
                video_proc,
            };
        }
        profile_list.truncate(num_profiles.max(0) as usize);

        let max_entry = ffi::vaMaxNumEntrypoints(display.raw()).max(0) as usize;
        for &profile in &profile_list {
            let family = family_of(profile);
            if family.is_none() && profile != ffi::VAProfileNone {
                continue;
            }
            let mut entry_list = vec![0i32; max_entry];
            let mut num_entries: std::ffi::c_int = 0;
            let entries = if ffi::vaQueryConfigEntrypoints(
                display.raw(),
                profile,
                entry_list.as_mut_ptr(),
                &mut num_entries,
            ) == ffi::VA_STATUS_SUCCESS
            {
                &entry_list[..num_entries.max(0) as usize]
            } else {
                &[][..]
            };
            let Some(family) = family else {
                video_proc |= entries.contains(&ffi::VAEntrypointVideoProc);
                continue;
            };
            let vld = entries.contains(&ffi::VAEntrypointVLD);
            let dec_proc = vld
                && crate::va::config_attribute(
                    display,
                    profile,
                    ffi::VAEntrypointVLD,
                    ffi::VAConfigAttribDecProcessing,
                )
                .is_ok_and(|v| v == Some(ffi::VA_DEC_PROCESSING));
            let refs = |ep| match entries.contains(&ep) {
                true => EncRefs::query(display, profile, ep),
                false => EncRefs::default(),
            };
            let caps = ProfileCaps {
                name: profile_name(profile),
                vld,
                enc: entries.contains(&ffi::VAEntrypointEncSlice),
                enc_lp: entries.contains(&ffi::VAEntrypointEncSliceLP),
                dec_proc,
                enc_refs: [refs(ffi::VAEntrypointEncSlice), refs(ffi::VAEntrypointEncSliceLP)],
            };
            profiles.push(caps);
            if caps.vld && !decode_families.contains(&family) {
                decode_families.push(family);
            }
            if (caps.enc || caps.enc_lp) && !encode_families.contains(&family) {
                encode_families.push(family);
            }
        }
    }

    VaCaps {
        device,
        vendor: display.vendor(),
        version: display.version(),
        decode_families,
        encode_families,
        profiles,
        video_proc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configuration_decides_in_place_of_the_environment() {
        let pinned =
            ProbeConfig { disabled: true, device: Some(PathBuf::from("/dev/dri/renderD129")) };
        assert_eq!(choice(Some(&pinned)), (true, Some(PathBuf::from("/dev/dri/renderD129"))));
        assert_eq!(
            choice(Some(&ProbeConfig::default())),
            (false, None),
            "the default probes and scans"
        );
        assert_eq!(candidate_nodes(Some(PathBuf::from("/x"))), vec![PathBuf::from("/x")]);
    }
}
