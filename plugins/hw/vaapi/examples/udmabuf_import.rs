// Probe and bench, not a hot path.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

// Software pictures into vaapipostproc two ways, udmabuf slots imported as surfaces against the
// CPU upload of raw buffers. Checks the scaled pictures match and times both.
//
// cargo run --release -p pf-vaapi --example udmabuf_import -- [--frames N] [--slots S]

use std::sync::Arc;
use std::time::Instant;

use pf_harness::Harness;
use pf_memory::udmabuf::{self, Dmabuf, sync_write};
use profluens_core::bus::BusMessage;
use profluens_core::format::ValueDesc;
use profluens_core::memory::{Pool, PoolConfig};
use profluens_core::time::Timestamp;

use pf_vaapi::gpuframe::{
    DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_NV12, DRM_FORMAT_P010, DmabufFds, ExportedPlane, F_LAYOUT,
    F_MODIFIER, FAMILY_VIDEO_GPU, FrameRx, FrameTx, GpuFrame, GpuFrameChannel, GpuFrameHeader,
    LAYOUT_NV12_DMABUF, LAYOUT_P010_DMABUF, Planes,
};
use pf_vaapi::vpp::{Target, VaapiPostProc};

const OUT: (u32, u32) = (1280, 720);
const SMALL: (u32, u32) = (640, 360);
const CHECKED: u64 = 12;

#[derive(Clone, Copy)]
struct Case {
    w: u32,
    h: u32,
    wide: bool,
    out: (u32, u32),
}

impl Case {
    fn name(&self) -> &'static str {
        if self.wide { "p010" } else { "nv12" }
    }

    fn sample(&self) -> usize {
        if self.wide { 2 } else { 1 }
    }

    fn pitch(&self) -> usize {
        self.w as usize * self.sample()
    }

    fn len(&self) -> usize {
        self.pitch() * self.h as usize * 3 / 2
    }

    fn target(&self) -> Target {
        Target {
            max_width: self.out.0,
            max_height: self.out.1,
            depth: if self.wide { 10 } else { 8 },
        }
    }
}

// A moving diagonal ramp in Y and two crossing ramps in CbCr, frame `i`.
fn pattern(c: Case, i: u64, out: &mut [u8]) {
    let (w, h, pitch) = (c.w as usize, c.h as usize, c.pitch());
    let (y, uv) = out[..c.len()].split_at_mut(pitch * h);
    let put = |row: &mut [u8], x: usize, v: u32| match c.wide {
        true => row[2 * x..2 * x + 2].copy_from_slice(&(((v & 0x3FF) as u16) << 6).to_le_bytes()),
        false => row[x] = v as u8,
    };
    let scale = if c.wide { 4 } else { 1 };
    for (r, row) in y.chunks_exact_mut(pitch).enumerate() {
        for x in 0..w {
            let v = (x / 3 + r / 2 + i as usize * 5) % 220 + 16;
            put(row, x, v as u32 * scale);
        }
    }
    for (r, row) in uv.chunks_exact_mut(pitch).enumerate() {
        for x in 0..w / 2 {
            let cb = (x / 2 + i as usize * 3) % 224 + 16;
            let cr = (r / 2 + i as usize * 7) % 224 + 16;
            put(row, 2 * x, cb as u32 * scale);
            put(row, 2 * x + 1, cr as u32 * scale);
        }
    }
}

fn cpu_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: writes the one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

struct Timing {
    frames: u64,
    cpu_ns: u64,
    wall_ns: u64,
    imports: u64,
}

impl Timing {
    fn cpu_ms(&self) -> f64 {
        self.cpu_ns as f64 / self.frames as f64 / 1e6
    }

    fn wall_ms(&self) -> f64 {
        self.wall_ns as f64 / self.frames as f64 / 1e6
    }
}

fn raw_fix(h: &mut Harness, c: Case) {
    let pixfmt = if c.wide { "p010" } else { "nv12" };
    h.fix_sink_format(
        VaapiPostProc::IN,
        "video/raw",
        &[
            ("width", ValueDesc::Int(i64::from(c.w))),
            ("height", ValueDesc::Int(i64::from(c.h))),
            ("pixfmt", ValueDesc::Id(pixfmt)),
        ],
    );
}

fn gpu_fix(h: &mut Harness, c: Case) {
    let layout = if c.wide { LAYOUT_P010_DMABUF } else { LAYOUT_NV12_DMABUF };
    h.fix_sink_format(
        VaapiPostProc::IN,
        FAMILY_VIDEO_GPU,
        &[
            ("width", ValueDesc::Int(i64::from(c.w))),
            ("height", ValueDesc::Int(i64::from(c.h))),
            (F_LAYOUT, ValueDesc::Id(layout)),
            (F_MODIFIER, ValueDesc::Int(DRM_FORMAT_MOD_LINEAR as i64)),
        ],
    );
}

fn out_slot(c: Case) -> usize {
    c.out.0 as usize * c.out.1 as usize * 3 / 2 * c.sample() + 4096
}

// The raw path, a pool buffer filled as a decoder packs one, uploaded by the element.
fn patterns(c: Case) -> Vec<Vec<u8>> {
    (0..CHECKED)
        .map(|i| {
            let mut f = vec![0u8; c.len()];
            pattern(c, i, &mut f);
            f
        })
        .collect()
}

fn raw(
    c: Case,
    src: &[Vec<u8>],
    frames: u64,
    gpu_out: Option<&Arc<GpuFrameChannel>>,
) -> (Vec<Vec<u8>>, Timing) {
    let vpp = match gpu_out {
        Some(ch) => VaapiPostProc::new(c.target()).with_gpu_output(Arc::clone(ch)),
        None => VaapiPostProc::new(c.target()),
    };
    let stats = vpp.stats();
    let mut h = Harness::with_pool(vpp, c.len().max(out_slot(c)), 8);
    raw_fix(&mut h, c);
    h.start().expect("start");
    let mut rx = gpu_out.map(|ch| ch.take_rx().expect("the out end"));
    let mut outs = Vec::new();
    let (t0, c0) = (Instant::now(), cpu_ns());
    for i in 0..frames {
        let mut b = h.alloc(&src[(i % CHECKED) as usize]);
        b.pts = Timestamp(i * 40_000_000);
        h.push(b).expect("pushed");
        let out = h.pull(VaapiPostProc::OUT).expect("a picture out");
        match rx.as_mut() {
            Some(rx) => rx.clear(),
            None => outs.push(out.memory.data().to_vec()),
        }
    }
    let t = Timing {
        frames,
        cpu_ns: cpu_ns() - c0,
        wall_ns: t0.elapsed().as_nanos() as u64,
        imports: stats.imports.load(std::sync::atomic::Ordering::Relaxed),
    };
    (outs, t)
}

struct Slots {
    pool: Pool,
    tx: FrameTx,
    token: u64,
    released: Vec<u64>,
}

impl Slots {
    // One udmabuf slot filled with picture `i` as the decoder's dmabuf mode fills one.
    fn frame(&mut self, c: Case, src: &[u8]) -> (GpuFrameHeader, bool) {
        self.released.clear();
        self.tx.drain_released(&mut self.released);
        let mut m = self.pool.try_acquire().expect("a udmabuf slot");
        let d = Dmabuf::of(&m).expect("the slot is a dmabuf");
        sync_write(d.fd, true).expect("sync start");
        m.as_mut_full()[..c.len()].copy_from_slice(&src[..c.len()]);
        m.set_len(c.len());
        sync_write(d.fd, false).expect("sync end");
        let mut fds = DmabufFds::default();
        // SAFETY: the dup is this frame's alone.
        unsafe { fds.push(d.dup().expect("dup")) };
        let mut planes = Planes::default();
        let luma = c.pitch() * c.h as usize;
        for at in [0, luma] {
            planes.push(ExportedPlane {
                object_index: 0,
                offset: (d.offset + at) as u32,
                pitch: c.pitch() as u32,
            });
        }
        self.token += 1;
        let frame = GpuFrame {
            token: self.token,
            fds,
            drm_format: if c.wide { DRM_FORMAT_P010 } else { DRM_FORMAT_NV12 },
            coded_w: c.w,
            coded_h: c.h,
            disp_w: c.w,
            disp_h: c.h,
            crop_x: 0,
            crop_y: 0,
            planes,
            epoch: 0,
            lease: None,
        };
        let header = GpuFrameHeader {
            token: self.token,
            coded_w: c.w,
            coded_h: c.h,
            disp_w: c.w,
            disp_h: c.h,
            crop_x: 0,
            crop_y: 0,
        };
        (header, self.tx.push(frame, m))
    }
}

// The import path, udmabuf slots the element imports and caches as surfaces.
fn import(
    c: Case,
    src: &[Vec<u8>],
    frames: u64,
    slots: u32,
    gpu_out: Option<&Arc<GpuFrameChannel>>,
) -> Result<(Vec<Vec<u8>>, Timing), String> {
    let dev = udmabuf::device().map_err(|e| e.to_string())?;
    let page = udmabuf::align();
    let slot = c.len().next_multiple_of(page);
    let pool = Pool::with_config(PoolConfig {
        slot_size: slot,
        slots,
        bytes: slot as u64 * u64::from(slots),
        align: page,
        allocator: dev.allocator(),
    });
    let ch_in = GpuFrameChannel::new();
    let vpp = VaapiPostProc::new_zerocopy(c.target(), Arc::clone(&ch_in));
    let vpp = match gpu_out {
        Some(ch) => vpp.with_gpu_output(Arc::clone(ch)),
        None => vpp,
    };
    let stats = vpp.stats();
    let mut h = Harness::with_pool(vpp, out_slot(c), 8);
    gpu_fix(&mut h, c);
    let tx = ch_in.take_tx().expect("the in end");
    h.start().map_err(|e| e.to_string())?;
    let mut rx: Option<FrameRx> = gpu_out.map(|ch| ch.take_rx().expect("the out end"));
    let mut s = Slots { pool, tx, token: 0, released: Vec::with_capacity(64) };
    let mut outs = Vec::new();
    let (t0, c0) = (Instant::now(), cpu_ns());
    for i in 0..frames {
        let (header, pushed) = s.frame(c, &src[(i % CHECKED) as usize]);
        if !pushed {
            return Err(format!("frame {i} refused by the channel"));
        }
        let mut b = h.alloc(&header.to_bytes());
        b.pts = Timestamp(i * 40_000_000);
        h.push(b).map_err(|e| e.to_string())?;
        let Some(out) = h.pull(VaapiPostProc::OUT) else {
            let warn: Vec<String> = h
                .bus_messages()
                .iter()
                .filter_map(|m| match m {
                    BusMessage::Warning { error, .. } | BusMessage::Error { error, .. } => {
                        Some(error.to_string())
                    }
                    _ => None,
                })
                .collect();
            return Err(format!("no picture out of frame {i}, {warn:?}"));
        };
        match rx.as_mut() {
            Some(rx) => rx.clear(),
            None => outs.push(out.memory.data().to_vec()),
        }
    }
    let t = Timing {
        frames,
        cpu_ns: cpu_ns() - c0,
        wall_ns: t0.elapsed().as_nanos() as u64,
        imports: stats.imports.load(std::sync::atomic::Ordering::Relaxed),
    };
    Ok((outs, t))
}

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn errno() -> String {
    std::io::Error::last_os_error().to_string()
}

// What this process's SELinux domain allows of the steps the dmabuf handover takes.
fn confined_check() {
    // SAFETY: a NUL-terminated path, the fd closed below.
    let dev = unsafe { libc::open(c"/dev/udmabuf".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    println!("open /dev/udmabuf: {}", if dev >= 0 { "ok".into() } else { errno() });
    if dev >= 0 {
        // SAFETY: our fd.
        unsafe { libc::close(dev) };
    }
    // SAFETY: plain syscalls on a memfd made here and closed at the end.
    unsafe {
        let fd = libc::memfd_create(c"check".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
        println!("memfd_create: {}", if fd >= 0 { "ok".into() } else { errno() });
        let t = libc::ftruncate(fd, 4096);
        println!("ftruncate: {}", if t == 0 { "ok".into() } else { errno() });
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let m = libc::mmap(std::ptr::null_mut(), 4096, prot, libc::MAP_SHARED, fd, 0);
        println!("mmap memfd rw: {}", if m != libc::MAP_FAILED { "ok".into() } else { errno() });
        if m != libc::MAP_FAILED {
            libc::munmap(m, 4096);
        }
        libc::close(fd);
    }
    let Some(caps) = pf_vaapi::probe() else {
        println!("no VA-API device");
        return;
    };
    let d = pf_vaapi::va::Display::open(&caps.device).expect("display");
    let s = pf_vaapi::va::Surfaces::new(&d, pf_vaapi::va::SurfaceFormat::NV12, 64, 64, 1)
        .expect("a surface");
    match pf_vaapi::va::ExportedSurface::export(d.raw(), s.ids()[0]) {
        Ok(e) => {
            let fd = e.fds[0];
            let sync = sync_write(fd, true).and_then(|()| sync_write(fd, false));
            println!("dmabuf sync ioctl: {}", if sync.is_ok() { "ok".into() } else { errno() });
            // SAFETY: a read-only map of the export, unmapped at once.
            let m = unsafe {
                libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, fd, 0)
            };
            println!(
                "mmap dmabuf ro: {}",
                if m != libc::MAP_FAILED { "ok".into() } else { errno() }
            );
            if m != libc::MAP_FAILED {
                // SAFETY: the map above.
                unsafe { libc::munmap(m, 4096) };
            }
        }
        Err(e) => println!("export: {e}"),
    }
    match pf_vaapi::vpp::linear_imports() {
        Ok(f) => println!("linear imports: {} layouts", f.len()),
        Err(e) => println!("linear imports refused: {e}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--confined-check") {
        return confined_check();
    }
    let frames = arg(&args, "--frames", 300);
    let slots = arg(&args, "--slots", 4) as u32;
    let Some(caps) = pf_vaapi::probe() else {
        eprintln!("no VA-API device");
        std::process::exit(2);
    };
    if let Err(e) = udmabuf::device() {
        eprintln!("no udmabuf, {e}");
        std::process::exit(3);
    }
    println!("device {} frames {frames} slots {slots}", caps.device.display());
    let cases = [
        Case { w: 1920, h: 1080, wide: false, out: OUT },
        Case { w: 1920, h: 1080, wide: true, out: OUT },
        Case { w: 3840, h: 2160, wide: false, out: OUT },
        Case { w: 3840, h: 2160, wide: true, out: OUT },
        Case { w: 854, h: 480, wide: false, out: SMALL },
        Case { w: 1918, h: 1080, wide: false, out: OUT },
        Case { w: 1000, h: 562, wide: false, out: SMALL },
        Case { w: 1000, h: 562, wide: true, out: SMALL },
    ];
    let mut failed = false;
    for c in cases {
        let tag = format!("{} {}x{}", c.name(), c.w, c.h);
        let src = patterns(c);
        let (want, _) = raw(c, &src, CHECKED, None);
        let got = match import(c, &src, CHECKED, slots, None) {
            Ok((got, _)) => got,
            Err(e) => {
                println!("{tag}: import FAILED, {e}");
                failed = true;
                continue;
            }
        };
        let mut worst = 0u8;
        let mut differ = 0usize;
        for (a, b) in want.iter().zip(&got) {
            differ += a.iter().zip(b).filter(|(x, y)| x != y).count();
            worst = worst.max(a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0));
        }
        let same = want.len() == got.len() && differ == 0;
        failed |= !same;
        println!(
            "{tag}: {} pictures, {} bytes differ, largest difference {worst}, {}",
            got.len(),
            differ,
            if same { "identical" } else { "DIFFERENT" }
        );
        let out = GpuFrameChannel::new();
        let (_, up) = raw(c, &src, frames, Some(&out));
        let out = GpuFrameChannel::new();
        let im = match import(c, &src, frames, slots, Some(&out)) {
            Ok((_, t)) => t,
            Err(e) => {
                println!("{tag}: timed import FAILED, {e}");
                failed = true;
                continue;
            }
        };
        println!(
            "{tag}: upload cpu {:.3} ms wall {:.3} ms a frame, import cpu {:.3} ms wall {:.3} ms \
             a frame ({} imports), cpu saved {:.0}%",
            up.cpu_ms(),
            up.wall_ms(),
            im.cpu_ms(),
            im.wall_ms(),
            im.imports,
            100.0 * (1.0 - im.cpu_ms() / up.cpu_ms())
        );
    }
    std::process::exit(i32::from(failed));
}
