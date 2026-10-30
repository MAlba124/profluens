//! VA-API encode capability tests. Skip without a device.

use pf_vaapi::ffi;
use pf_vaapi::va::{Config, Context, Display, Surfaces};

const H264_PROFILES: [ffi::VAProfile; 3] =
    [ffi::VAProfileH264High, ffi::VAProfileH264Main, ffi::VAProfileH264ConstrainedBaseline];

#[test]
fn probe_reports_encode_entrypoints() {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip probe_reports_encode_entrypoints: no VA-API device");
        return;
    };
    eprintln!("device={} encode_families={:?}", caps.device.display(), caps.encode_families);
    for p in &caps.profiles {
        eprintln!("  {:<28} vld={} enc={} enc_lp={}", p.name, p.vld, p.enc, p.enc_lp);
    }
    let any_enc = caps.profiles.iter().any(|p| p.enc || p.enc_lp);
    assert_eq!(
        any_enc,
        !caps.encode_families.is_empty(),
        "profile table and encode_families disagree"
    );
}

/// Allocation is the step queries cannot vouch for, a read-only DRM fd fails only here.
#[test]
fn h264_encode_context_allocates() {
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("skip h264_encode_context_allocates: no VA-API device");
        return;
    };
    if !caps.supports_encode("h264/annexb") {
        eprintln!("skip h264_encode_context_allocates: no H.264 encode entrypoint");
        return;
    }

    let display = Display::open(&caps.device).expect("probe said the device opens");

    let mut last_err = None;
    for profile in H264_PROFILES {
        for entrypoint in [ffi::VAEntrypointEncSlice, ffi::VAEntrypointEncSliceLP] {
            let config = match Config::new_encode(&display, profile, entrypoint) {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let surfaces =
                Surfaces::new_nv12(&display, 1280, 720, 4).expect("encode input surfaces");
            let context = Context::new(&display, &config, 1280, 720, &surfaces)
                .expect("encode context over NV12 surfaces");
            eprintln!(
                "encode context OK: profile={profile} entrypoint={entrypoint} ctx={}",
                context.id()
            );
            return;
        }
    }
    panic!(
        "probe advertised h264/annexb encode but no profile/entrypoint combination \
         created a config: {last_err:?}"
    );
}
