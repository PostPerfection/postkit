//! ffmpeg writing the source's own planar YUV to the pipe and grok's
//! accelerator plugin taking those planes.
//!
//! Its own process, because the plugin is a switch for the whole process.

#![cfg(feature = "grok-gpu")]

use postkit::encode::{StreamEncodeOptions, stream_encode_inprocess};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 256;
const HEIGHT: u32 = 144;
const FRAMES: u64 = 24;

/// A yuv420p H.264 clip, the format a camera or a delivery master arrives in.
fn yuv420p_clip(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("source.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={WIDTH}x{HEIGHT}:rate=24:duration=1"))
        .args(["-frames:v", &FRAMES.to_string()])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "fixture encode failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    path
}

#[test]
fn a_yuv_source_reaches_the_plugin_as_planes() {
    postkit::grok_encoder::initialize(0);
    if let Err(reason) = postkit::grok_encoder::use_gpu() {
        panic!("{reason}");
    }

    let dir = tempfile::tempdir().unwrap();
    let input = yuv420p_clip(dir.path());
    let output_dir = dir.path().join("j2c");
    let result = stream_encode_inprocess(
        &StreamEncodeOptions {
            input,
            output_dir: output_dir.clone(),
            // a plain code stream: the cinema profiles constrain the raster
            rsiz: 0,
            num_resolutions: 3,
            ..StreamEncodeOptions::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    );

    assert!(result.success, "encode failed: {}", result.error);
    assert_eq!(
        result.pipe_pixel_format.as_deref(),
        Some("yuv420p"),
        "the planes have to reach the plugin unconverted"
    );
    assert_eq!(result.frames_encoded, FRAMES);
    for index in 0..FRAMES {
        let codestream = std::fs::read(output_dir.join(format!("frame_{index:08}.j2c")))
            .unwrap_or_else(|e| panic!("frame {index} was not written: {e}"));
        let frame = postkit::grok_decoder::decode(codestream, 0)
            .unwrap_or_else(|e| panic!("frame {index}: {e}"));
        assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
        assert_eq!(frame.components.len(), 3);
    }
}
