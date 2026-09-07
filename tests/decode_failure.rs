#![cfg(feature = "grok-ffi")]

use postkit::encode::{
    EncodeResult, FrameRate, SourceColour, StreamEncodeOptions, default_rsiz,
    stream_encode_inprocess,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const FRAME_RATE: u32 = 24;
const FRAMES: u64 = 1;

/// A one frame lossless clip, which ffprobe reads a size and a packet count
/// from whatever happens to its picture afterwards.
fn clip(dir: &Path) -> PathBuf {
    let clip = dir.join("clip.mkv");
    let run = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("testsrc=s={WIDTH}x{HEIGHT}:d=1:r={FRAME_RATE}"))
        .args(["-frames:v", &FRAMES.to_string()])
        .args(["-c:v", "ffv1", "-pix_fmt", "yuv420p"])
        .arg(&clip)
        .output()
        .expect("ffmpeg has to run");
    assert!(
        run.status.success(),
        "the fixture encode failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    clip
}

/// The same clip with its picture payload flipped, so the container still says
/// 128x72 and one packet while the codec can decode nothing.
fn clip_with_unreadable_picture(dir: &Path) -> PathBuf {
    let readable = clip(dir);
    let mut bytes = std::fs::read(&readable).expect("the fixture has to be readable");
    let payload_start = bytes.len() / 2;
    let payload_end = bytes.len() - 32;
    for byte in &mut bytes[payload_start..payload_end] {
        *byte ^= 0xFF;
    }
    let corrupt = dir.join("corrupt.mkv");
    std::fs::write(&corrupt, &bytes).expect("the corrupt fixture has to be written");
    let (width, height, frames) = postkit::encode::probe_video(&corrupt);
    assert_eq!(
        (width, height, frames),
        (WIDTH, HEIGHT, FRAMES),
        "the probe has to still read the source, or the encode fails before ffmpeg runs"
    );
    corrupt
}

fn encode(input: &Path, output_dir: &Path, source_colour: SourceColour) -> EncodeResult {
    stream_encode_inprocess(
        &StreamEncodeOptions {
            input: input.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
            fps: FrameRate::whole(FRAME_RATE),
            source_colour,
            rsiz: default_rsiz(),
            ..StreamEncodeOptions::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
}

#[test]
fn a_source_ffmpeg_cannot_decode_fails_the_encode() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let corrupt = clip_with_unreadable_picture(dir.path());

    let result = encode(&corrupt, &dir.path().join("j2k"), SourceColour::DisplayRgb);
    assert_eq!(result.frames_encoded, 0);
    assert!(
        !result.success,
        "an encode that wrote nothing cannot report success: {}",
        result.error
    );
    assert!(
        result.error.contains("ffmpeg failed"),
        "the error has to name ffmpeg's exit: {}",
        result.error
    );
    assert!(
        result.error.contains("Decoding error"),
        "the error has to carry what ffmpeg wrote to stderr: {}",
        result.error
    );
}

#[test]
fn a_filter_chain_ffmpeg_rejects_fails_the_encode() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let readable = clip(dir.path());
    let lut = dir.path().join("broken.cube");
    std::fs::write(&lut, "this is not a cube file\n").expect("the LUT has to be written");

    let result = encode(
        &readable,
        &dir.path().join("j2k"),
        SourceColour::DciLut(lut),
    );
    assert_eq!(result.frames_encoded, 0);
    assert!(
        !result.success,
        "an encode that wrote nothing cannot report success: {}",
        result.error
    );
    assert!(
        result.error.contains("ffmpeg failed"),
        "the error has to name ffmpeg's exit: {}",
        result.error
    );
    assert!(
        result.error.contains("3D LUT is empty"),
        "the error has to carry what ffmpeg wrote to stderr: {}",
        result.error
    );
}

#[test]
fn a_source_ffmpeg_reads_encodes_as_it_did() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let readable = clip(dir.path());

    let result = encode(&readable, &dir.path().join("j2k"), SourceColour::DisplayRgb);
    assert!(result.success, "the encode failed: {}", result.error);
    assert_eq!(result.frames_encoded, FRAMES);
    assert!(result.error.is_empty(), "{}", result.error);
}
