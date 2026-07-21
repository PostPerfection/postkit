//! grok in-process encode perf bench (rerunnable check for grok pipeline changes).
//!
//! run (all physical cores):
//!   PKG_CONFIG_PATH=$HOME/bin/grok/lib64/pkgconfig LD_LIBRARY_PATH=$HOME/bin/grok/lib64 \
//!     taskset -c 0,2,4,6,8,10,12,14 \
//!     cargo run --release --features grok-ffi --bin bench_j2k -- both
//! run (single thread): same but taskset -c 0 and FRAMES_4K=24
//!
//! thread budget is set purely by taskset: the pipeline sizes its worker count
//! from available_parallelism(), which honours cpu affinity. grok's ratio is set
//! per resolution to a DCI-like bytes/frame so the encoded sizes are realistic.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use postkit::grok_encoder::{
    self, CompressParams, EncodeProgress, ProgressionOrder, RawFrame, encode_pipeline,
};
use postkit::j2k::parse_j2k_header;

/// DCI codestream cap (~250 Mbps / 8 / 24fps) used as the target bytes/frame so
/// the grok ratio yields realistic DCI-sized output.
const TARGET_BYTES_PER_FRAME: f64 = 1_250_000.0;

struct Res {
    name: &'static str,
    w: u32,
    h: u32,
    profile: u16,
    frames: u64,
}

struct RunStat {
    fps: f64,
    out_mbps: f64,
    avg_size: f64,
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn base_params(res: &Res) -> CompressParams {
    CompressParams {
        compression_ratio: 10.0,
        num_resolutions: 6,
        codeblock_size: 32,
        progression: ProgressionOrder::Cprl,
        num_layers: 1,
        profile: res.profile,
        num_guard_bits: 1,
        frame_rate: 24,
        irreversible: true,
        mct: true,
        apply_xyz_transform: false,
        threads_per_codec: 1,
    }
}

/// decode K unique rgb48be frames from ffmpeg testsrc2 into packed byte buffers.
fn gen_frames(w: u32, h: u32, k: usize) -> Vec<Vec<u8>> {
    let size = format!("{w}x{h}");
    let mut child = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={size}:rate=24"))
        .args(["-frames:v", &k.to_string(), "-pix_fmt", "rgb48be", "-f", "rawvideo", "pipe:1"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn ffmpeg");
    let mut out = child.stdout.take().unwrap();
    let frame_bytes = (w as usize) * (h as usize) * 6;
    let mut pool = Vec::with_capacity(k);
    for _ in 0..k {
        let mut buf = vec![0u8; frame_bytes];
        if out.read_exact(&mut buf).is_err() {
            break;
        }
        pool.push(buf);
    }
    let _ = child.wait();
    assert!(!pool.is_empty(), "ffmpeg produced no frames");
    pool
}

/// build 12-bit planar components (R,G,B) from a packed rgb48be frame.
fn packed_to_planar12(packed: &[u8], npix: usize) -> [Vec<i32>; 3] {
    let mut r = vec![0i32; npix];
    let mut g = vec![0i32; npix];
    let mut b = vec![0i32; npix];
    for i in 0..npix {
        let o = i * 6;
        r[i] = ((((packed[o] as u32) << 8) | packed[o + 1] as u32) >> 4) as i32;
        g[i] = ((((packed[o + 2] as u32) << 8) | packed[o + 3] as u32) >> 4) as i32;
        b[i] = ((((packed[o + 4] as u32) << 8) | packed[o + 5] as u32) >> 4) as i32;
    }
    [r, g, b]
}

fn clear_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
}

fn avg_output_size(dir: &Path) -> f64 {
    let mut total = 0u64;
    let mut n = 0u64;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().extension().and_then(|x| x.to_str()) == Some("j2c") {
            total += e.metadata().map(|m| m.len()).unwrap_or(0);
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { total as f64 / n as f64 }
}

fn run(dir: &Path, params: &CompressParams, planar: &[[Vec<i32>; 3]], w: u32, h: u32, n: u64) -> RunStat {
    clear_dir(dir);
    let cancel = Arc::new(AtomicBool::new(false));
    let k = planar.len() as u64;
    let mut i: u64 = 0;
    let mut producer = || {
        if i >= n {
            return None;
        }
        let idx = i;
        i += 1;
        Some(RawFrame::Planar {
            components: planar[(idx % k) as usize].clone(),
            width: w,
            height: h,
            precision: 12,
            index: idx,
        })
    };
    let start = Instant::now();
    encode_pipeline(dir, params, n, &cancel, &mut producer, |_p: EncodeProgress| {});
    let elapsed = start.elapsed().as_secs_f64();
    stat(dir, n, elapsed)
}

fn stat(dir: &Path, n: u64, elapsed: f64) -> RunStat {
    let avg = avg_output_size(dir);
    let total_out = avg * n as f64;
    RunStat {
        fps: n as f64 / elapsed,
        out_mbps: total_out / elapsed / 1e6,
        avg_size: avg,
    }
}

fn sanity(dir: &Path, label: &str) {
    let first = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("j2c"))
        .min();
    match first.and_then(|p| std::fs::read(p).ok()) {
        Some(bytes) => match parse_j2k_header(&bytes) {
            Some(h) => println!(
                "  {label}: valid codestream {}x{} comps={} depth={} profile={:#06x} first_frame_bytes={}",
                h.width, h.height, h.num_components, h.bit_depth, h.profile, bytes.len()
            ),
            None => println!("  {label}: INVALID codestream (header parse failed)"),
        },
        None => println!("  {label}: no output file"),
    }
}

fn spread(v: &[f64]) -> f64 {
    let max = v.iter().cloned().fold(f64::MIN, f64::max);
    let min = v.iter().cloned().fold(f64::MAX, f64::min);
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    if mean == 0.0 { 0.0 } else { (max - min) / mean * 100.0 }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "both".into());
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let repeats = env_u64("REPEATS", 2) as usize;
    let unique = env_u64("UNIQUE", 32) as usize;

    let all = [
        Res { name: "2K", w: 1998, h: 1080, profile: 0x0003, frames: env_u64("FRAMES_2K", 120) },
        Res { name: "4K", w: 3996, h: 2160, profile: 0x0004, frames: env_u64("FRAMES_4K", 48) },
    ];
    let sel: Vec<&Res> = all
        .iter()
        .filter(|r| arg == "both" || arg.eq_ignore_ascii_case(r.name))
        .collect();

    grok_encoder::initialize(cores as u32);
    println!("cores (affinity-visible) = {cores}, repeats = {repeats}, unique frames = {unique}");
    println!("content = ffmpeg testsrc2; 12-bit planar frames; grok ratio set to DCI-like bytes/frame\n");

    let out = PathBuf::from("/dev/shm/bench_j2k_out");

    for res in sel {
        let npix = (res.w as usize) * (res.h as usize);
        println!("=== {} ({}x{}), {} frames/run ===", res.name, res.w, res.h, res.frames);
        let packed = gen_frames(res.w, res.h, unique);
        let planar: Vec<[Vec<i32>; 3]> =
            packed.iter().map(|p| packed_to_planar12(p, npix)).collect();
        drop(packed);

        // grok rate control targets raw_bytes/ratio, so solve ratio for the DCI
        // target bytes/frame directly.
        let mut params = base_params(res);
        let raw12 = npix as f64 * 3.0 * 1.5; // 12-bit rgb bytes/frame
        let ratio = (raw12 / TARGET_BYTES_PER_FRAME).clamp(2.0, 200.0);
        params.compression_ratio = ratio;
        let cal_n = 8u64.min(res.frames);
        println!(
            "  target {:.0} B/frame, raw12 {:.0} B -> grok ratio {:.1}",
            TARGET_BYTES_PER_FRAME, raw12, ratio
        );

        // warmup (not timed)
        run(&out, &params, &planar, res.w, res.h, cal_n);

        let mut g_fps = Vec::new();
        let mut g_last = None;
        for r in 0..repeats {
            let g = run(&out, &params, &planar, res.w, res.h, res.frames);
            sanity(&out, &format!("grok r{r}"));
            println!("  r{r}: grok {:.2} fps {:.1} MB/s {:.0} B", g.fps, g.out_mbps, g.avg_size);
            g_fps.push(g.fps);
            g_last = Some(g);
        }
        let g = g_last.unwrap();
        let gm = g_fps.iter().sum::<f64>() / g_fps.len() as f64;
        println!(
            "  SUMMARY {} @ {} cores: grok {:.2} fps ({:.0}% spread) {:.1} MB/s {:.0} B\n",
            res.name, cores, gm, spread(&g_fps), g.out_mbps, g.avg_size
        );
    }

    grok_encoder::deinitialize();
    let _ = std::fs::remove_dir_all(&out);
}
