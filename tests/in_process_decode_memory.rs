#![cfg(target_os = "linux")]

use postkit::encode::{FrameRange, FrameRate, StreamEncodeOptions, stream_encode_inprocess};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const SOURCE_FRAMES: u64 = 400;
const WARM_UP_FRAMES: u64 = 50;
const WIDTH: u64 = 640;
const HEIGHT: u64 = 360;
// one yuv420p rawvideo frame, which is one packet
const PACKET_BYTES: u64 = WIDTH * HEIGHT * 3 / 2;
// allocator growth across the first runs reaches 35 MB with no leak
const LEAKED_PACKETS_ALLOWED: u64 = SOURCE_FRAMES / 2;
const ENCODE_THREADS: u32 = 2;

fn make_source(path: &Path) {
    let test_pattern = format!("testsrc2=s={WIDTH}x{HEIGHT}:r=24");
    let frames = SOURCE_FRAMES.to_string();
    let run = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &test_pattern])
        .args(["-frames:v", &frames])
        .args(["-c:v", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(path)
        .output()
        .expect("ffmpeg has to run");
    assert!(
        run.status.success(),
        "fixture ffmpeg failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}

fn resident_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .expect("/proc/self/status has a VmRSS line");
    let kilobytes: u64 = line
        .trim_start_matches("VmRSS:")
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .unwrap();
    kilobytes * 1024
}

fn encode(source: &Path, output_dir: &Path, frame_range: Option<FrameRange>) {
    std::fs::create_dir_all(output_dir).unwrap();
    let options = StreamEncodeOptions {
        input: source.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        fps: FrameRate::whole(24),
        frame_range,
        encode_threads: ENCODE_THREADS,
        ..StreamEncodeOptions::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let result = stream_encode_inprocess(&options, &cancel, &pause, |_| {});
    assert!(result.success, "encode failed: {}", result.error);
}

#[test]
fn the_in_process_decode_frees_each_packet_it_reads() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("raw.nut");
    make_source(&source);

    encode(
        &source,
        &dir.path().join("warm_up"),
        Some(FrameRange {
            first_frame: 0,
            frame_count: WARM_UP_FRAMES,
        }),
    );
    let before = resident_bytes();
    encode(&source, &dir.path().join("full"), None);
    // a leaked packet is still resident after the run returns
    let after = resident_bytes();

    let growth = after.saturating_sub(before);
    let allowed = LEAKED_PACKETS_ALLOWED * PACKET_BYTES;
    println!("resident memory grew {growth} bytes over the full encode, allowed {allowed}");
    assert!(
        growth < allowed,
        "resident memory grew {growth} bytes over a {SOURCE_FRAMES} frame encode, \
         more than the {allowed} bytes {LEAKED_PACKETS_ALLOWED} leaked {PACKET_BYTES} byte packets would add"
    );
}
