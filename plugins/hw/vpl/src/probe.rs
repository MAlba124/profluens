//! The capability probe, once per process. Gates element registration and the report.
//! [`configure`] before the first probe decides whether it runs and on which node. Without it,
//! `PF_NO_VPL=1` forces no device and `PF_VPL_DEVICE` pins the render node.

// Cold, cached once.
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::error::{MfxError, check};
use crate::ffi;
use crate::group::{Join, VplGroup};
use crate::session::{Loader, Session, attach, header};
use crate::va::Device;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncQuery {
    pub codec: u32,
    pub low_power: bool,
    pub rc: u16,
    pub lookahead: u16,
    pub ext_brc: bool,
    pub status: i32,
    pub rc_out: u16,
    pub lookahead_out: u16,
    pub ext_brc_out: bool,
    pub mbbrc_out: bool,
    pub bpyramid_out: bool,
    pub adaptive_i_out: bool,
    pub adaptive_b_out: bool,
}

impl EncQuery {
    pub fn taken(&self) -> bool {
        self.status >= 0
            && self.rc_out == self.rc
            && (self.lookahead == 0 || self.lookahead_out > 0)
            && (!self.ext_brc || self.ext_brc_out)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncPlan {
    pub low_power: bool,
    pub rc: u16,
    pub lookahead: u16,
    pub ext_brc: bool,
}

#[derive(Debug, Clone)]
pub struct VplCaps {
    pub device: PathBuf,
    pub vendor: String,
    pub va_version: (i32, i32),
    pub api: (u16, u16),
    pub device_id: u16,
    pub integrated: bool,
    pub decode_families: Vec<&'static str>,
    pub decode_main10: bool,
    pub decode_vp9_profile2: bool,
    pub encode_families: Vec<&'static str>,
    pub enc_queries: Vec<EncQuery>,
    pub fused: i32,
    pub import: bool,
    pub tone_map: bool,
}

impl VplCaps {
    pub fn supports(&self, family: &str) -> bool {
        self.decode_families.contains(&family)
    }

    pub fn supports_encode(&self, family: &str) -> bool {
        self.encode_families.contains(&family)
    }

    pub fn encode_plan(&self, codec: u32, want_lookahead: bool) -> Option<EncPlan> {
        let order: &[(bool, u16, u16, bool)] = if want_lookahead {
            &[
                (false, ffi::MFX_RATECONTROL_LA_HRD, LOOKAHEAD, false),
                (false, ffi::MFX_RATECONTROL_LA, LOOKAHEAD, false),
                (true, ffi::MFX_RATECONTROL_VBR, LOOKAHEAD, true),
                (false, ffi::MFX_RATECONTROL_VBR, LOOKAHEAD, true),
                (false, ffi::MFX_RATECONTROL_VBR, 0, false),
                (true, ffi::MFX_RATECONTROL_VBR, 0, false),
            ]
        } else {
            &[
                (false, ffi::MFX_RATECONTROL_VBR, 0, false),
                (true, ffi::MFX_RATECONTROL_VBR, 0, false),
            ]
        };
        order.iter().find_map(|&(low_power, rc, lookahead, ext_brc)| {
            let q = self.enc_queries.iter().find(|q| {
                q.codec == codec
                    && q.low_power == low_power
                    && q.rc == rc
                    && q.lookahead == lookahead
                    && q.ext_brc == ext_brc
            })?;
            q.taken().then_some(EncPlan {
                low_power,
                rc,
                lookahead: q.lookahead_out.min(lookahead),
                ext_brc,
            })
        })
    }

    pub fn rc_taken(&self, codec: u32, low_power: bool, rc: u16) -> bool {
        self.enc_queries.iter().any(|q| {
            q.codec == codec
                && q.low_power == low_power
                && q.rc == rc
                && q.lookahead == 0
                && !q.ext_brc
                && q.taken()
        })
    }
}

pub const LOOKAHEAD: u16 = 20;

/// The loader, the device and the process wide group an element without a group of its own
/// joins under [`Join::Global`].
pub struct Runtime {
    pub loader: Loader,
    pub device: Arc<Device>,
    pub group: Arc<VplGroup>,
    pub join: Join,
    /// `PF_VPL_ASYNC`, the async depth of elements that name none.
    pub async_depth: Option<u16>,
    pub caps: VplCaps,
}

impl Runtime {
    /// The parent an element joins, its own group first, else what [`Join`] says.
    pub fn parent_for<'a>(&'a self, group: Option<&'a Arc<VplGroup>>) -> Option<&'a Arc<VplGroup>> {
        match (group, self.join) {
            (Some(g), _) => Some(g),
            (None, Join::Global) => Some(&self.group),
            (None, Join::None) => None,
        }
    }

    pub fn session(&self) -> Result<Session, MfxError> {
        Session::new(&self.loader, &self.device)
    }

    /// An element's async depth, its own, else the env's, else 1.
    pub fn async_depth(&self, own: Option<u16>) -> u16 {
        own.or(self.async_depth).unwrap_or(1).max(1)
    }
}

static PROBED: OnceLock<Option<Runtime>> = OnceLock::new();

pub fn probe() -> Option<&'static VplCaps> {
    runtime().map(|r| &r.caps)
}

pub fn runtime() -> Option<&'static Runtime> {
    PROBED.get_or_init(probe_uncached).as_ref()
}

pub fn codec_of(family: &str) -> Option<u32> {
    match family {
        "h264/annexb" => Some(ffi::MFX_CODEC_AVC),
        "h265/annexb" => Some(ffi::MFX_CODEC_HEVC),
        "vp9" => Some(ffi::MFX_CODEC_VP9),
        "av1" => Some(ffi::MFX_CODEC_AV1),
        _ => None,
    }
}

pub fn family_of(codec: u32) -> Option<&'static str> {
    match codec {
        ffi::MFX_CODEC_AVC => Some("h264/annexb"),
        ffi::MFX_CODEC_HEVC => Some("h265/annexb"),
        ffi::MFX_CODEC_VP9 => Some("vp9"),
        ffi::MFX_CODEC_AV1 => Some("av1"),
        _ => None,
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
            ConfigureError::Probed => "the VPL probe already ran",
            ConfigureError::Configured => "the VPL probe is already configured",
        })
    }
}

impl std::error::Error for ConfigureError {}

static CONFIG: OnceLock<ProbeConfig> = OnceLock::new();

/// Set once, before the first [`probe`], and then in place of `PF_NO_VPL` and `PF_VPL_DEVICE`.
pub fn configure(config: ProbeConfig) -> Result<(), ConfigureError> {
    if PROBED.get().is_some() {
        return Err(ConfigureError::Probed);
    }
    CONFIG.set(config).map_err(|_| ConfigureError::Configured)
}

/// The configuration, else the environment: whether to skip, and a pinned node.
fn choice(config: Option<&ProbeConfig>) -> (bool, Option<PathBuf>) {
    match config {
        Some(c) => (c.disabled, c.device.clone()),
        None => {
            let disabled = std::env::var("PF_NO_VPL").is_ok_and(|v| v == "1");
            let device = std::env::var("PF_VPL_DEVICE").ok().filter(|d| !d.is_empty());
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
            if name.strip_prefix("renderD").is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())) {
                nodes.push(entry.path());
            }
        }
    }
    nodes.sort();
    nodes
}

fn probe_uncached() -> Option<Runtime> {
    let (disabled, pinned) = choice(CONFIG.get());
    if disabled {
        return None;
    }
    let loader = Loader::new()?;
    for node in candidate_nodes(pinned) {
        let Ok(device) = Device::open(&node) else {
            continue;
        };
        let device = Arc::new(device);
        let Ok(session) = Session::new(&loader, &device) else {
            continue;
        };
        let caps = query(&session, &device);
        let join = Join::of_env(std::env::var("PF_VPL_JOIN").ok().as_deref());
        let async_depth = std::env::var("PF_VPL_ASYNC").ok().and_then(|v| v.parse().ok());
        return Some(Runtime {
            loader,
            device,
            group: VplGroup::of(session),
            join,
            async_depth,
            caps,
        });
    }
    None
}

pub fn frame_info(fourcc: u32, w: u16, h: u16) -> ffi::mfxFrameInfo {
    let ten = fourcc == ffi::MFX_FOURCC_P010;
    ffi::mfxFrameInfo {
        FourCC: fourcc,
        ChromaFormat: ffi::MFX_CHROMAFORMAT_YUV420,
        PicStruct: ffi::MFX_PICSTRUCT_PROGRESSIVE,
        BitDepthLuma: if ten { 10 } else { 8 },
        BitDepthChroma: if ten { 10 } else { 8 },
        Shift: u16::from(ten),
        Width: w.next_multiple_of(16),
        Height: h.next_multiple_of(16),
        CropW: w,
        CropH: h,
        FrameRateExtN: 30,
        FrameRateExtD: 1,
        ..ffi::mfxFrameInfo::default()
    }
}

/// The AV1 Query refuses level 0 while Init takes it, so AV1 names a level.
pub fn decode_param(codec: u32, profile: u16, fourcc: u32) -> ffi::mfxVideoParam {
    let mut p = ffi::mfxVideoParam::zeroed();
    p.IOPattern = ffi::MFX_IOPATTERN_OUT_VIDEO_MEMORY;
    let d = p.dec_mut();
    d.CodecId = codec;
    d.CodecProfile = profile;
    if codec == ffi::MFX_CODEC_AV1 {
        d.CodecLevel = ffi::MFX_LEVEL_AV1_53;
    }
    d.FrameInfo = frame_info(fourcc, 320, 240);
    p
}

fn decode_query(session: &Session, codec: u32, profile: u16, fourcc: u32) -> bool {
    let mut p = decode_param(codec, profile, fourcc);
    let mut out = p;
    // SAFETY: both params are valid, no ext buffers.
    unsafe { ffi::MFXVideoDECODE_Query(session.raw(), &mut p, &mut out) >= 0 }
}

fn encode_query(
    session: &Session,
    codec: u32,
    low_power: bool,
    rc: u16,
    lookahead: u16,
    ext_brc: bool,
) -> EncQuery {
    let on = ffi::MFX_CODINGOPTION_ON;
    let mut p = ffi::mfxVideoParam::zeroed();
    p.IOPattern = ffi::MFX_IOPATTERN_IN_VIDEO_MEMORY;
    let m = p.mfx_mut();
    m.CodecId = codec;
    m.FrameInfo = frame_info(ffi::MFX_FOURCC_NV12, 1280, 720);
    m.TargetUsage = ffi::MFX_TARGETUSAGE_BALANCED;
    m.GopPicSize = 60;
    m.GopRefDist = 4;
    m.RateControlMethod = rc;
    m.LowPower = if low_power { on } else { ffi::MFX_CODINGOPTION_OFF };
    match rc {
        ffi::MFX_RATECONTROL_CQP => {
            m.InitialDelayInKB = 26;
            m.TargetKbps = 26;
            m.MaxKbps = 26;
        }
        ffi::MFX_RATECONTROL_ICQ => m.TargetKbps = 24,
        _ => {
            m.TargetKbps = 3000;
            m.MaxKbps = 4500;
        }
    }
    let mut co2 = ffi::mfxExtCodingOption2 {
        Header: header::<ffi::mfxExtCodingOption2>(ffi::MFX_EXTBUFF_CODING_OPTION2),
        ..Default::default()
    };
    co2.LookAheadDepth = lookahead;
    co2.ExtBRC = if ext_brc { on } else { 0 };
    co2.MBBRC = on;
    co2.BRefType = ffi::MFX_B_REF_PYRAMID;
    co2.AdaptiveI = on;
    co2.AdaptiveB = on;
    let mut co3 = ffi::mfxExtCodingOption3 {
        Header: header::<ffi::mfxExtCodingOption3>(ffi::MFX_EXTBUFF_CODING_OPTION3),
        ..Default::default()
    };
    co3.QVBRQuality = if rc == ffi::MFX_RATECONTROL_QVBR { 24 } else { 0 };
    let mut list =
        [(&raw mut co2).cast::<ffi::mfxExtBuffer>(), (&raw mut co3).cast::<ffi::mfxExtBuffer>()];
    attach(&mut p, &mut list);
    let mut out_co2 = co2;
    let mut out_co3 = co3;
    let mut out_list = [
        (&raw mut out_co2).cast::<ffi::mfxExtBuffer>(),
        (&raw mut out_co3).cast::<ffi::mfxExtBuffer>(),
    ];
    let mut out = p;
    attach(&mut out, &mut out_list);
    // SAFETY: both params and their ext buffers are live for the call.
    let status = unsafe { ffi::MFXVideoENCODE_Query(session.raw(), &mut p, &mut out) };
    EncQuery {
        codec,
        low_power,
        rc,
        lookahead,
        ext_brc,
        status,
        rc_out: out.mfx().RateControlMethod,
        lookahead_out: out_co2.LookAheadDepth,
        ext_brc_out: out_co2.ExtBRC == on,
        mbbrc_out: out_co2.MBBRC == on,
        bpyramid_out: out_co2.BRefType == ffi::MFX_B_REF_PYRAMID,
        adaptive_i_out: out_co2.AdaptiveI == on,
        adaptive_b_out: out_co2.AdaptiveB == on,
    }
}

fn fused_query(session: &Session) -> i32 {
    let mut p = ffi::mfxVideoParam::zeroed();
    p.IOPattern = ffi::MFX_IOPATTERN_OUT_VIDEO_MEMORY;
    let d = p.dec_mut();
    d.CodecId = ffi::MFX_CODEC_HEVC;
    d.FrameInfo = frame_info(ffi::MFX_FOURCC_NV12, 320, 240);
    // SAFETY: zero is a value of every field.
    let mut ch: ffi::mfxVideoChannelParam = unsafe { std::mem::zeroed() };
    ch.VPP = frame_info(ffi::MFX_FOURCC_NV12, 160, 120);
    ch.VPP.ChannelId = 1;
    ch.IOPattern = ffi::MFX_IOPATTERN_OUT_VIDEO_MEMORY;
    let mut chs = [&raw mut ch];
    // SAFETY: the param and channel outlive the calls.
    let st = unsafe { ffi::MFXVideoDECODE_VPP_Init(session.raw(), &mut p, chs.as_mut_ptr(), 1) };
    if st >= 0 {
        // SAFETY: initialized above.
        unsafe { ffi::MFXVideoDECODE_VPP_Close(session.raw()) };
    }
    st
}

fn tone_map_query(session: &Session) -> bool {
    let mut p = ffi::mfxVideoParam::zeroed();
    p.IOPattern = ffi::MFX_IOPATTERN_IN_VIDEO_MEMORY | ffi::MFX_IOPATTERN_OUT_VIDEO_MEMORY;
    let v = p.vpp_mut();
    v.In = frame_info(ffi::MFX_FOURCC_P010, 1920, 1080);
    v.Out = frame_info(ffi::MFX_FOURCC_NV12, 1920, 1080);
    let signal = |id, primaries, transfer, matrix, full| ffi::mfxExtVideoSignalInfo {
        Header: header::<ffi::mfxExtVideoSignalInfo>(id),
        VideoFullRange: full,
        ColourDescriptionPresent: 1,
        ColourPrimaries: primaries,
        TransferCharacteristics: transfer,
        MatrixCoefficients: matrix,
        ..Default::default()
    };
    let mut vsi_in = signal(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO_IN, 9, 16, 9, 0);
    let mut vsi_out = signal(ffi::MFX_EXTBUFF_VIDEO_SIGNAL_INFO_OUT, 1, 1, 1, 0);
    let mut mdcv = ffi::mfxExtMasteringDisplayColourVolume {
        Header: header::<ffi::mfxExtMasteringDisplayColourVolume>(
            ffi::MFX_EXTBUFF_MASTERING_DISPLAY_COLOUR_VOLUME_IN,
        ),
        DisplayPrimariesX: [34000, 13250, 7500],
        DisplayPrimariesY: [16000, 34500, 3000],
        WhitePointX: 15635,
        WhitePointY: 16450,
        MaxDisplayMasteringLuminance: 1000,
        MinDisplayMasteringLuminance: 50,
        ..Default::default()
    };
    let mut cll = ffi::mfxExtContentLightLevelInfo {
        Header: header::<ffi::mfxExtContentLightLevelInfo>(
            ffi::MFX_EXTBUFF_CONTENT_LIGHT_LEVEL_INFO,
        ),
        MaxContentLightLevel: 1000,
        MaxPicAverageLightLevel: 400,
        ..Default::default()
    };
    let mut list = [
        (&raw mut vsi_in).cast::<ffi::mfxExtBuffer>(),
        (&raw mut vsi_out).cast::<ffi::mfxExtBuffer>(),
        (&raw mut mdcv).cast::<ffi::mfxExtBuffer>(),
        (&raw mut cll).cast::<ffi::mfxExtBuffer>(),
    ];
    attach(&mut p, &mut list);
    let mut out = p;
    // SAFETY: the param and its buffers are live for the call, the out param shares them.
    unsafe { ffi::MFXVideoVPP_Query(session.raw(), &mut p, &mut out) >= 0 }
}

fn query(session: &Session, device: &Device) -> VplCaps {
    let platform = session.platform().unwrap_or_default();
    let mut decode_families = Vec::new();
    let mut encode_families = Vec::new();
    for (codec, family, main) in [
        (ffi::MFX_CODEC_AVC, "h264/annexb", ffi::MFX_PROFILE_AVC_HIGH),
        (ffi::MFX_CODEC_HEVC, "h265/annexb", ffi::MFX_PROFILE_HEVC_MAIN),
        (ffi::MFX_CODEC_VP9, "vp9", ffi::MFX_PROFILE_VP9_0),
        (ffi::MFX_CODEC_AV1, "av1", ffi::MFX_PROFILE_AV1_MAIN),
    ] {
        if decode_query(session, codec, ffi::MFX_PROFILE_UNKNOWN, ffi::MFX_FOURCC_NV12)
            || decode_query(session, codec, main, ffi::MFX_FOURCC_NV12)
        {
            decode_families.push(family);
        }
    }
    let decode_main10 = decode_query(
        session,
        ffi::MFX_CODEC_HEVC,
        ffi::MFX_PROFILE_HEVC_MAIN10,
        ffi::MFX_FOURCC_P010,
    );
    let decode_vp9_profile2 =
        decode_query(session, ffi::MFX_CODEC_VP9, ffi::MFX_PROFILE_VP9_2, ffi::MFX_FOURCC_P010);
    let mut enc_queries = Vec::new();
    for (codec, family) in
        [(ffi::MFX_CODEC_AVC, "h264/annexb"), (ffi::MFX_CODEC_HEVC, "h265/annexb")]
    {
        let mut any = false;
        for low_power in [false, true] {
            for (rc, lookahead, ext_brc) in [
                (ffi::MFX_RATECONTROL_LA_HRD, LOOKAHEAD, false),
                (ffi::MFX_RATECONTROL_LA, LOOKAHEAD, false),
                (ffi::MFX_RATECONTROL_VBR, LOOKAHEAD, true),
                (ffi::MFX_RATECONTROL_VBR, 0, false),
                (ffi::MFX_RATECONTROL_CBR, 0, false),
                (ffi::MFX_RATECONTROL_QVBR, 0, false),
                (ffi::MFX_RATECONTROL_ICQ, 0, false),
                (ffi::MFX_RATECONTROL_CQP, 0, false),
            ] {
                let q = encode_query(session, codec, low_power, rc, lookahead, ext_brc);
                any |= q.status >= 0;
                enc_queries.push(q);
            }
        }
        if any {
            encode_families.push(family);
        }
    }
    VplCaps {
        device: device.path.clone(),
        vendor: device.vendor(),
        va_version: device.version,
        api: (session.version.Major, session.version.Minor),
        device_id: platform.DeviceId,
        integrated: platform.MediaAdapterType == 0,
        decode_families,
        decode_main10,
        decode_vp9_profile2,
        encode_families,
        enc_queries,
        fused: fused_query(session),
        import: session.memory_interface().is_ok(),
        tone_map: tone_map_query(session),
    }
}

pub fn describe(caps: &VplCaps) -> String {
    let mut s = format!(
        "device={} vendor={:?} va={}.{} api={}.{} pci=0x{:04x} integrated={} decode={:?} main10={} vp9p2={} encode={:?} fused={} import={} tonemap={}\n",
        caps.device.display(),
        caps.vendor,
        caps.va_version.0,
        caps.va_version.1,
        caps.api.0,
        caps.api.1,
        caps.device_id,
        caps.integrated,
        caps.decode_families,
        caps.decode_main10,
        caps.decode_vp9_profile2,
        caps.encode_families,
        MfxError { status: caps.fused, call: "" }.name(),
        caps.import,
        caps.tone_map,
    );
    for q in &caps.enc_queries {
        s.push_str(&format!(
            "  {} lp={} rc={} la={} extbrc={} -> {} rc={} la={} extbrc={} mbbrc={} bpyr={} adI={} adB={}\n",
            family_of(q.codec).unwrap_or("?"),
            q.low_power,
            rc_name(q.rc),
            q.lookahead,
            q.ext_brc,
            MfxError { status: q.status, call: "" }.name(),
            rc_name(q.rc_out),
            q.lookahead_out,
            q.ext_brc_out,
            q.mbbrc_out,
            q.bpyramid_out,
            q.adaptive_i_out,
            q.adaptive_b_out,
        ));
    }
    s
}

pub fn rc_name(rc: u16) -> &'static str {
    match rc {
        ffi::MFX_RATECONTROL_CBR => "CBR",
        ffi::MFX_RATECONTROL_VBR => "VBR",
        ffi::MFX_RATECONTROL_CQP => "CQP",
        ffi::MFX_RATECONTROL_LA => "LA",
        ffi::MFX_RATECONTROL_ICQ => "ICQ",
        ffi::MFX_RATECONTROL_LA_ICQ => "LA_ICQ",
        ffi::MFX_RATECONTROL_LA_HRD => "LA_HRD",
        ffi::MFX_RATECONTROL_QVBR => "QVBR",
        _ => "?",
    }
}

pub fn status(st: ffi::mfxStatus, call: &'static str) -> Result<(), MfxError> {
    check(st, call)
}

/// A hardware test that cannot run here says so. Under `PF_REQUIRE_VPL=1` it fails instead, so a
/// box with the hardware cannot pass by skipping.
pub fn skip(test: &str, why: &str) {
    eprintln!("skip {test}: {why}");
    if std::env::var("PF_REQUIRE_VPL").is_ok_and(|v| v == "1") {
        panic!("{test}: PF_REQUIRE_VPL=1 and the test skipped, {why}");
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
mod tests {
    use super::*;

    #[test]
    fn av1_decode_query_names_a_level_the_others_do_not() {
        let mut av1 =
            decode_param(ffi::MFX_CODEC_AV1, ffi::MFX_PROFILE_AV1_MAIN, ffi::MFX_FOURCC_NV12);
        let mut hevc =
            decode_param(ffi::MFX_CODEC_HEVC, ffi::MFX_PROFILE_HEVC_MAIN, ffi::MFX_FOURCC_NV12);
        assert_eq!(av1.dec_mut().CodecLevel, ffi::MFX_LEVEL_AV1_53);
        assert_eq!(hevc.dec_mut().CodecLevel, 0);
        assert_eq!(av1.dec_mut().CodecProfile, ffi::MFX_PROFILE_AV1_MAIN);
    }

    fn q(
        codec: u32,
        low_power: bool,
        rc: u16,
        lookahead: u16,
        ext_brc: bool,
        status: i32,
        rc_out: u16,
        lookahead_out: u16,
        ext_brc_out: bool,
    ) -> EncQuery {
        EncQuery {
            codec,
            low_power,
            rc,
            lookahead,
            ext_brc,
            status,
            rc_out,
            lookahead_out,
            ext_brc_out,
            mbbrc_out: true,
            bpyramid_out: true,
            adaptive_i_out: true,
            adaptive_b_out: true,
        }
    }

    fn caps(enc_queries: Vec<EncQuery>) -> VplCaps {
        VplCaps {
            device: PathBuf::new(),
            vendor: String::new(),
            va_version: (1, 24),
            api: (2, 16),
            device_id: 0,
            integrated: true,
            decode_families: vec![],
            decode_main10: false,
            decode_vp9_profile2: false,
            encode_families: vec!["h265/annexb"],
            enc_queries,
            fused: 0,
            import: true,
            tone_map: false,
        }
    }

    #[test]
    fn the_plan_takes_lookahead_rate_control_before_the_extended_brc() {
        let h = ffi::MFX_CODEC_HEVC;
        let c = caps(vec![
            q(
                h,
                false,
                ffi::MFX_RATECONTROL_LA_HRD,
                LOOKAHEAD,
                false,
                0,
                ffi::MFX_RATECONTROL_LA_HRD,
                LOOKAHEAD,
                false,
            ),
            q(
                h,
                true,
                ffi::MFX_RATECONTROL_VBR,
                LOOKAHEAD,
                true,
                0,
                ffi::MFX_RATECONTROL_VBR,
                LOOKAHEAD,
                true,
            ),
        ]);
        assert_eq!(
            c.encode_plan(h, true),
            Some(EncPlan {
                low_power: false,
                rc: ffi::MFX_RATECONTROL_LA_HRD,
                lookahead: LOOKAHEAD,
                ext_brc: false
            })
        );
    }

    #[test]
    fn a_low_power_runtime_without_lookahead_modes_falls_to_the_extended_brc_then_plain_vbr() {
        let h = ffi::MFX_CODEC_HEVC;
        let c = caps(vec![
            q(
                h,
                false,
                ffi::MFX_RATECONTROL_LA_HRD,
                LOOKAHEAD,
                false,
                5,
                ffi::MFX_RATECONTROL_VBR,
                0,
                false,
            ),
            q(h, false, ffi::MFX_RATECONTROL_LA, LOOKAHEAD, false, -3, 0, 0, false),
            q(
                h,
                true,
                ffi::MFX_RATECONTROL_VBR,
                LOOKAHEAD,
                true,
                5,
                ffi::MFX_RATECONTROL_VBR,
                8,
                true,
            ),
            q(h, true, ffi::MFX_RATECONTROL_VBR, 0, false, 0, ffi::MFX_RATECONTROL_VBR, 0, false),
        ]);
        assert_eq!(
            c.encode_plan(h, true),
            Some(EncPlan {
                low_power: true,
                rc: ffi::MFX_RATECONTROL_VBR,
                lookahead: 8,
                ext_brc: true
            })
        );
        let c = caps(vec![
            q(
                h,
                true,
                ffi::MFX_RATECONTROL_VBR,
                LOOKAHEAD,
                true,
                5,
                ffi::MFX_RATECONTROL_VBR,
                0,
                false,
            ),
            q(h, true, ffi::MFX_RATECONTROL_VBR, 0, false, 0, ffi::MFX_RATECONTROL_VBR, 0, false),
        ]);
        assert_eq!(
            c.encode_plan(h, true),
            Some(EncPlan {
                low_power: true,
                rc: ffi::MFX_RATECONTROL_VBR,
                lookahead: 0,
                ext_brc: false
            })
        );
        assert_eq!(c.encode_plan(ffi::MFX_CODEC_AVC, true), None);
    }

    #[test]
    fn families_map_both_ways_and_the_frame_info_aligns() {
        assert_eq!(codec_of("h265/annexb"), Some(ffi::MFX_CODEC_HEVC));
        assert_eq!(family_of(ffi::MFX_CODEC_AV1), Some("av1"));
        assert_eq!(codec_of("mpeg2"), None);
        let f = frame_info(ffi::MFX_FOURCC_P010, 1920, 1080);
        assert_eq!((f.Width, f.Height, f.CropW, f.CropH), (1920, 1088, 1920, 1080));
        assert_eq!((f.BitDepthLuma, f.Shift), (10, 1));
        assert_eq!(frame_info(ffi::MFX_FOURCC_NV12, 100, 100).Width, 112);
    }
}

#[cfg(test)]
mod config_tests {
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
