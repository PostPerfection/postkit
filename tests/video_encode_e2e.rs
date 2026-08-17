//! The video arm of the encode pipeline, end to end.
//!
//! This is the one place that proves `run_encode_with_options` actually produces
//! codestreams for a video source. That arm reaches grok through the FFI, so a
//! consumer that forgets the `grok-ffi` feature gets an error instead of an
//! encode, and nothing else catches it.

use postkit::encode::FrameRate;
use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_with_options};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const FRAME_COUNT: u64 = 4;
const WIDTH: u32 = 128;
const HEIGHT: u32 = 128;

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn a_video_source_encodes_to_one_codestream_per_frame() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={WIDTH}x{HEIGHT}:d=1:r={FRAME_COUNT}"),
            "-frames:v",
            &FRAME_COUNT.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .expect("ffmpeg");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );

    let output = dir.path().join("out");
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let result = run_encode_with_options(
        &video,
        &output,
        &EncodeRunOptions {
            fps: FrameRate::whole(FRAME_COUNT as u32),
            ..Default::default()
        },
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("video encode");

    assert_eq!(
        result.frames_encoded, FRAME_COUNT,
        "every source frame should reach the encoder"
    );
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&result.j2k_dir),
        FRAME_COUNT,
        "one codestream per frame should land in {}",
        result.j2k_dir.display()
    );

    // a file of the right name proves nothing on its own, so read one back as a
    // codestream and check it describes the raster that went in
    let first = result.j2k_dir.join("frame_00000000.j2c");
    let bytes = std::fs::read(&first).expect("read first codestream");
    let header = postkit::j2k::parse_j2k_header(&bytes)
        .unwrap_or_else(|| panic!("{} is not a J2K codestream", first.display()));
    assert_eq!((header.width, header.height), (WIDTH, HEIGHT));
    assert_eq!(header.num_components, 3);
    assert!(
        header.tlm_present,
        "Bv2.1 needs a TLM marker in every codestream"
    );
}

/// 500 is the shortest clip where the two rates disagree: 500 frames at
/// 24000/1001 run 20.854 s, which an `fps=24` filter resamples to 501 frames,
/// so a wrong-by-a-frame composition shows up as a codestream count.
#[test]
fn a_23_976_clip_encodes_500_codestreams_where_an_integer_24_would_make_501() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    const NTSC_FRAME_COUNT: u64 = 500;
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("ntsc.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={WIDTH}x{HEIGHT}:r=24000/1001"),
            "-frames:v",
            &NTSC_FRAME_COUNT.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .expect("ffmpeg");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );

    let output = dir.path().join("out");
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let result = run_encode_with_options(
        &video,
        &output,
        &EncodeRunOptions {
            fps: FrameRate::new(24000, 1001),
            ..Default::default()
        },
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("video encode");

    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&result.j2k_dir),
        NTSC_FRAME_COUNT,
        "the fractional rate should neither add nor drop a frame"
    );
}

/// The 23.976-to-24 conform: the same 500-frame NTSC clip declared 24 fps. Read
/// at 24 every source frame reaches the encoder once; left at its own rate the
/// `fps=24` filter duplicates one and the composition runs a frame long.
#[test]
fn reading_a_23_976_clip_at_24_encodes_every_frame_once_and_not_reading_it_duplicates_one() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    const NTSC_FRAME_COUNT: u64 = 500;
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("ntsc.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={WIDTH}x{HEIGHT}:r=24000/1001"),
            "-frames:v",
            &NTSC_FRAME_COUNT.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .expect("ffmpeg");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let encode = |output: &std::path::Path, read_source_at| {
        run_encode_with_options(
            &video,
            output,
            &EncodeRunOptions {
                fps: FrameRate::whole(24),
                read_source_at,
                ..Default::default()
            },
            &cancel,
            &pause,
            |_: &PipelineProgress| {},
            |_: &str| {},
        )
        .expect("video encode")
    };

    let conformed = encode(&dir.path().join("conformed"), Some(FrameRate::whole(24)));
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&conformed.j2k_dir),
        NTSC_FRAME_COUNT,
        "reading the source at 24 should neither duplicate nor drop a frame"
    );

    let resampled = encode(&dir.path().join("resampled"), None);
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&resampled.j2k_dir),
        NTSC_FRAME_COUNT + 1,
        "without the option the fps filter should still duplicate a frame"
    );
}

/// An image sequence carries its rate in the concat list it decodes from, so
/// there is no source timing to override.
#[test]
fn an_image_sequence_refuses_a_source_read_rate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("frame_0001.png"), b"not really a png").unwrap();

    let outcome = run_encode_with_options(
        dir.path(),
        &dir.path().join("out"),
        &EncodeRunOptions {
            fps: FrameRate::whole(24),
            read_source_at: Some(FrameRate::whole(24)),
            ..Default::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    );
    let Err(error) = outcome else {
        panic!("an image sequence has no timestamps to override");
    };
    assert!(error.contains("another rate"), "{error}");
}
