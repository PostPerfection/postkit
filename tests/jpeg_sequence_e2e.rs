//! A JPEG image sequence through the encode pipeline, end to end.
//!
//! JPEG frames are the one sequence format that never reaches grk_compress
//! directly, so this proves the routing sends them through ffmpeg's concat
//! demuxer and that the codestreams come back at the source raster.

use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_with_options};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const FRAME_COUNT: u64 = 3;
const FRAME_SIZE: u32 = 128;

/// Raster of a codestream decoded to a 16-bit-per-channel PPM.
fn decoded_raster(grk_decompress: &Path, codestream: &Path, out: &Path) -> (u32, u32) {
    let output = std::process::Command::new(grk_decompress)
        .env("LD_LIBRARY_PATH", postkit::grok::grok_lib_path())
        .args(["-i", &codestream.to_string_lossy()])
        .args(["-o", &out.to_string_lossy()])
        .output()
        .expect("grk_decompress");
    assert!(
        output.status.success(),
        "grk_decompress failed on {}: {}",
        codestream.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(out).expect("decoded ppm");
    // P6 header: magic, width, height, maxval, each whitespace-separated.
    let mut at = 0usize;
    let mut fields: Vec<String> = Vec::new();
    while fields.len() < 3 {
        while bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes[at] == b'#' {
            while bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        let start = at;
        while !bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        fields.push(String::from_utf8_lossy(&bytes[start..at]).to_string());
    }
    (
        fields[1].parse().expect("ppm width"),
        fields[2].parse().expect("ppm height"),
    )
}

#[test]
fn a_jpeg_sequence_encodes_to_one_codestream_per_frame() {
    let grk_decompress =
        postkit::grok::find_grk_decompress().expect("grk_decompress is required for this test");

    let dir = tempfile::tempdir().unwrap();
    let frames_dir = dir.path().join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={FRAME_SIZE}x{FRAME_SIZE}:d=1:r={FRAME_COUNT}"),
            "-frames:v",
            &FRAME_COUNT.to_string(),
        ])
        .arg(frames_dir.join("frame_%03d.jpg"))
        .output()
        .expect("ffmpeg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        postkit::encode::detect_input_type(&frames_dir),
        postkit::encode::InputType::ImageSequence,
        "a directory of jpg frames has to be an image sequence"
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let result = run_encode_with_options(
        &frames_dir,
        &dir.path().join("out"),
        &EncodeRunOptions::default(),
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |message: &str| eprintln!("{message}"),
    )
    .expect("encode");

    assert_eq!(result.frames_encoded, FRAME_COUNT);
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&result.j2k_dir),
        FRAME_COUNT,
        "one codestream per jpeg frame in {}",
        result.j2k_dir.display()
    );

    let first = result.j2k_dir.join("frame_00000000.j2c");
    let header = postkit::j2k::parse_j2k_header(&std::fs::read(&first).expect("read codestream"))
        .expect("codestream header");
    assert_eq!((header.width, header.height), (FRAME_SIZE, FRAME_SIZE));
    assert_eq!(
        decoded_raster(&grk_decompress, &first, &dir.path().join("frame.ppm")),
        (FRAME_SIZE, FRAME_SIZE),
        "the decoded frame has to come back at the source raster"
    );
}
