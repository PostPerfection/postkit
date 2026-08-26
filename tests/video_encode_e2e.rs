//! The video arm of the encode pipeline, end to end.
//!
//! This is the one place that proves `run_encode_with_options` actually produces
//! codestreams for a video source. That arm reaches grok through the FFI, so a
//! consumer that forgets the `grok-ffi` feature gets an error instead of an
//! encode, and nothing else catches it.

use postkit::encode::{FrameRange, FrameRate};
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

/// A `[TIMING]` line is only worth printing if the clocks behind it come from a
/// real encode, so this runs one and reads the last progress update.
#[test]
fn a_video_encode_reports_where_the_time_went() {
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

    let last_progress = std::sync::Mutex::new(None::<PipelineProgress>);
    let result = run_encode_with_options(
        &video,
        &dir.path().join("out"),
        &EncodeRunOptions {
            fps: FrameRate::whole(FRAME_COUNT as u32),
            ..Default::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |progress: &PipelineProgress| {
            *last_progress.lock().unwrap() = Some(progress.clone());
        },
        |_: &str| {},
    )
    .expect("video encode");
    assert_eq!(result.frames_encoded, FRAME_COUNT);

    let progress = last_progress
        .into_inner()
        .unwrap()
        .expect("the encode should report progress at least once");
    assert!(
        progress.encode_secs > 0.0,
        "four frames went through grok, so the j2k clock cannot be zero: {progress:?}"
    );
    for (phase, seconds) in [
        ("decoder wait", progress.decode_wait_secs),
        ("frame prep", progress.prepare_secs),
        ("j2k", progress.encode_secs),
        ("write", progress.write_secs),
    ] {
        assert!(seconds >= 0.0, "{phase} clock ran backwards: {seconds}");
    }
    let breakdown = progress.phase_breakdown();
    for phase in ["decoder wait", "frame prep", "j2k", "write"] {
        assert!(
            breakdown.contains(phase),
            "{phase} missing from {breakdown}"
        );
    }
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

/// The cap has to reach the writer thread from `EncodeRunOptions`, and the run
/// has to end at the frame that breaks it. The old sweep of the finished
/// directory encoded the whole clip first.
#[test]
fn a_codestream_over_the_cap_ends_the_run_before_the_clip_is_encoded() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    const CAPPED_FRAME_COUNT: u64 = 48;
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={WIDTH}x{HEIGHT}:d=1:r={CAPPED_FRAME_COUNT}"),
            "-frames:v",
            &CAPPED_FRAME_COUNT.to_string(),
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

    let options = |cap: Option<u64>| EncodeRunOptions {
        fps: FrameRate::whole(CAPPED_FRAME_COUNT as u32),
        codestream_byte_cap: cap,
        ..Default::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));

    // encode with no cap first, so the cap below is under what these frames
    // compress to by construction rather than by a guessed number
    let uncapped = run_encode_with_options(
        &video,
        &dir.path().join("uncapped"),
        &options(None),
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("uncapped encode");
    assert_eq!(uncapped.frames_encoded, CAPPED_FRAME_COUNT);
    let smallest = std::fs::read_dir(&uncapped.j2k_dir)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .min()
        .expect("the uncapped encode wrote no codestreams");
    let cap = smallest / 2;

    let capped_output = dir.path().join("capped");
    let outcome = run_encode_with_options(
        &video,
        &capped_output,
        &options(Some(cap)),
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    );
    let Err(error) = outcome else {
        panic!("every frame is over the {cap} byte cap, so the run has to fail");
    };
    assert!(
        error.contains(&format!(
            "over the {cap} byte per-frame cap: lower the bitrate"
        )),
        "{error}"
    );

    let written = std::fs::read_dir(capped_output.join("j2k"))
        .map(|entries| entries.count() as u64)
        .unwrap_or(0);
    assert!(
        written < CAPPED_FRAME_COUNT,
        "the run wrote {written} of {CAPPED_FRAME_COUNT} frames instead of stopping at the \
         first frame over the cap"
    );
}

/// A window is the whole point of `frame_range`: a wizard that keeps five
/// minutes of a two hour source must not compress the other one hour fifty five
/// and then throw the codestreams away.
#[test]
fn a_frame_range_encodes_only_its_window_and_the_frames_a_full_encode_wrote_there() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    const SOURCE_FRAMES: u64 = 48;
    const FIRST_FRAME: u64 = 10;
    const WINDOW_FRAMES: u64 = 5;

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=s={WIDTH}x{HEIGHT}:r=24"),
            "-frames:v",
            &SOURCE_FRAMES.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .expect("ffmpeg");
    assert!(
        ffmpeg.status.success(),
        "{}",
        String::from_utf8_lossy(&ffmpeg.stderr)
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let encode = |name: &str, frame_range: Option<FrameRange>| {
        run_encode_with_options(
            &video,
            &dir.path().join(name),
            &EncodeRunOptions {
                fps: FrameRate::whole(24),
                frame_range,
                ..Default::default()
            },
            &cancel,
            &pause,
            |_: &PipelineProgress| {},
            |_: &str| {},
        )
    };
    let codestream = |dir: &std::path::Path, index: u64| {
        let path = dir.join(format!("frame_{index:08}.j2c"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };

    let full = encode("full", None).expect("full encode");
    assert_eq!(full.frames_encoded, SOURCE_FRAMES);

    // the frame comparison below only means anything if the encoder writes the
    // same bytes for the same picture, so prove that first
    let repeat = encode("repeat", None).expect("second full encode");
    for index in 0..SOURCE_FRAMES {
        assert_eq!(
            codestream(&full.j2k_dir, index),
            codestream(&repeat.j2k_dir, index),
            "frame {index} differs between two encodes of the same source"
        );
    }

    let window = encode(
        "window",
        Some(FrameRange {
            first_frame: FIRST_FRAME,
            frame_count: WINDOW_FRAMES,
        }),
    )
    .expect("windowed encode");

    assert_eq!(window.frames_encoded, WINDOW_FRAMES);
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&window.j2k_dir),
        WINDOW_FRAMES,
        "the window's codestreams are numbered from zero"
    );
    assert_eq!(
        std::fs::read_dir(&window.j2k_dir).unwrap().count() as u64,
        WINDOW_FRAMES,
        "nothing outside the window may be compressed"
    );

    for offset in 0..WINDOW_FRAMES {
        assert_eq!(
            codestream(&window.j2k_dir, offset),
            codestream(&full.j2k_dir, FIRST_FRAME + offset),
            "frame {offset} of the window has to be the frame a full encode wrote as {}",
            FIRST_FRAME + offset
        );
    }
    assert_ne!(
        codestream(&window.j2k_dir, 0),
        codestream(&full.j2k_dir, 0),
        "a window starting at {FIRST_FRAME} that returned the head of the source would pass \
         every count above"
    );
}

#[test]
fn a_frame_range_past_the_end_of_the_source_fails_before_anything_is_encoded() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    const SOURCE_FRAMES: u64 = 48;

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=s={WIDTH}x{HEIGHT}:r=24"),
            "-frames:v",
            &SOURCE_FRAMES.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .expect("ffmpeg");
    assert!(ffmpeg.status.success());

    let output = dir.path().join("out");
    let outcome = run_encode_with_options(
        &video,
        &output,
        &EncodeRunOptions {
            fps: FrameRate::whole(24),
            frame_range: Some(FrameRange {
                first_frame: 46,
                frame_count: 5,
            }),
            ..Default::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_: &PipelineProgress| {},
        |_: &str| {},
    );
    let Err(error) = outcome else {
        panic!("a window running past the {SOURCE_FRAMES} frame source has to fail");
    };

    assert!(error.contains("46..51"), "{error}");
    assert!(error.contains("48 frames long"), "{error}");
    assert_eq!(
        std::fs::read_dir(output.join("j2k"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "nothing may be encoded before the window is refused"
    );
}

/// dcpwizard's CLI `create` encodes through `encode_video_pipeline_resumable`
/// rather than the pipeline above, so the window has to hold there too, resume
/// included.
#[test]
fn the_resumable_pipeline_encodes_a_window_and_resumes_inside_it() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    use postkit::grok_encoder::{
        CompressParams, EncodeProgress, contiguous_encoded_frames, encode_video_pipeline_resumable,
    };

    const SOURCE_FRAMES: u64 = 48;
    const FIRST_FRAME: u64 = 10;
    const WINDOW_FRAMES: u64 = 5;

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=s={WIDTH}x{HEIGHT}:r=24"),
            "-frames:v",
            &SOURCE_FRAMES.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .output()
        .expect("ffmpeg");
    assert!(
        ffmpeg.status.success(),
        "{}",
        String::from_utf8_lossy(&ffmpeg.stderr)
    );

    postkit::grok_encoder::initialize(0);
    let params = CompressParams {
        compression_ratio: 10.0,
        edit_rate: FrameRate::whole(24),
        ..CompressParams::default()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let encode =
        |output: &std::path::Path, frames: u64, resume: bool, range: Option<FrameRange>| {
            encode_video_pipeline_resumable(
                &video,
                output,
                &params,
                frames,
                WIDTH,
                HEIGHT,
                &cancel,
                resume,
                None,
                range,
                |_: EncodeProgress| {},
            )
        };
    let codestream = |dir: &std::path::Path, index: u64| {
        let path = dir.join(format!("frame_{index:08}.j2c"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };

    let full_dir = dir.path().join("full");
    let full = encode(&full_dir, SOURCE_FRAMES, false, None);
    assert!(full.success, "{}", full.error);
    assert_eq!(full.frames_encoded, SOURCE_FRAMES);

    let window = FrameRange {
        first_frame: FIRST_FRAME,
        frame_count: WINDOW_FRAMES,
    };
    let window_dir = dir.path().join("window");
    let windowed = encode(&window_dir, WINDOW_FRAMES, false, Some(window));
    assert!(windowed.success, "{}", windowed.error);
    assert_eq!(windowed.frames_encoded, WINDOW_FRAMES);
    assert_eq!(
        std::fs::read_dir(&window_dir).unwrap().count() as u64,
        WINDOW_FRAMES,
        "nothing outside the window may be compressed"
    );
    for offset in 0..WINDOW_FRAMES {
        assert_eq!(
            codestream(&window_dir, offset),
            codestream(&full_dir, FIRST_FRAME + offset),
            "frame {offset} of the window has to be the frame a full encode wrote as {}",
            FIRST_FRAME + offset
        );
    }
    assert_ne!(
        codestream(&window_dir, 0),
        codestream(&full_dir, 0),
        "a window starting at {FIRST_FRAME} that returned the head of the source would pass \
         every count above"
    );

    // an interrupted windowed encode: three codestreams on disk, the third
    // truncated the way a killed write leaves it
    let resume_dir = dir.path().join("resume");
    std::fs::create_dir_all(&resume_dir).unwrap();
    for index in 0..3u64 {
        let data = if index == 2 {
            codestream(&window_dir, index)[..8].to_vec()
        } else {
            codestream(&window_dir, index)
        };
        std::fs::write(resume_dir.join(format!("frame_{index:08}.j2c")), data).unwrap();
    }

    let resumed = encode(&resume_dir, WINDOW_FRAMES, true, Some(window));
    assert!(resumed.success, "{}", resumed.error);
    assert_eq!(
        resumed.frames_encoded,
        WINDOW_FRAMES - 2,
        "resume re-encodes the suspect frame and everything after it"
    );
    assert_eq!(
        contiguous_encoded_frames(&resume_dir),
        WINDOW_FRAMES,
        "the resumed window has to end up with every frame"
    );
    for offset in 0..WINDOW_FRAMES {
        assert_eq!(
            codestream(&resume_dir, offset),
            codestream(&full_dir, FIRST_FRAME + offset),
            "resumed frame {offset} has to be the frame a full encode wrote as {}",
            FIRST_FRAME + offset
        );
    }

    let past_end = encode(
        &dir.path().join("past_end"),
        WINDOW_FRAMES,
        false,
        Some(FrameRange {
            first_frame: 46,
            frame_count: 5,
        }),
    );
    assert!(!past_end.success);
    assert!(past_end.error.contains("46..51"), "{}", past_end.error);
    assert!(
        past_end.error.contains("48 frames long"),
        "{}",
        past_end.error
    );
}

const DETECTION_FPS: u32 = 24;
const DETECTION_SEGMENT_SECONDS: u64 = 3;
const DETECTION_SEGMENT_FRAMES: u64 = DETECTION_FPS as u64 * DETECTION_SEGMENT_SECONDS;
const DETECTION_WIDTH: u32 = 128;
const DETECTION_HEIGHT: u32 = 72;

fn within_one_frame(actual: u64, expected: u64) -> bool {
    actual.abs_diff(expected) <= 1
}

/// Three seconds of black, three of testsrc, then three of one still colour.
fn write_detection_clip(path: &std::path::Path) {
    let size = format!("{DETECTION_WIDTH}x{DETECTION_HEIGHT}");
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=black:s={size}:r={DETECTION_FPS}:d={DETECTION_SEGMENT_SECONDS}"),
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={size}:r={DETECTION_FPS}:d={DETECTION_SEGMENT_SECONDS}"),
            "-f",
            "lavfi",
            "-i",
            &format!("color=0x336699:s={size}:r={DETECTION_FPS}:d={DETECTION_SEGMENT_SECONDS}"),
            "-filter_complex",
            "[0:v][1:v][2:v]concat=n=3:v=1:a=0[v]",
            "-map",
            "[v]",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv444p",
        ])
        .arg(path)
        .output()
        .expect("ffmpeg");
    assert!(
        ffmpeg.status.success(),
        "{}",
        String::from_utf8_lossy(&ffmpeg.stderr)
    );
}

#[test]
fn a_black_head_and_a_frozen_tail_are_reported_by_the_encode() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mkv");
    write_detection_clip(&video);

    let output = dir.path().join("out");
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let result = run_encode_with_options(
        &video,
        &output,
        &EncodeRunOptions {
            fps: FrameRate::whole(DETECTION_FPS),
            ..Default::default()
        },
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("video encode");

    assert_eq!(result.frames_encoded, DETECTION_SEGMENT_FRAMES * 3);

    let findings = &result.picture_findings;
    let black = findings
        .black
        .first()
        .unwrap_or_else(|| panic!("no black run reported, findings: {findings:?}"));
    assert!(
        within_one_frame(black.first_frame, 0)
            && within_one_frame(black.last_frame, DETECTION_SEGMENT_FRAMES - 1),
        "the black head is frames 0..={}, got {black:?}",
        DETECTION_SEGMENT_FRAMES - 1
    );

    let last_frame = DETECTION_SEGMENT_FRAMES * 3 - 1;
    let frozen = findings
        .frozen
        .last()
        .unwrap_or_else(|| panic!("no frozen run reported, findings: {findings:?}"));
    assert!(
        within_one_frame(frozen.first_frame, DETECTION_SEGMENT_FRAMES * 2)
            && within_one_frame(frozen.last_frame, last_frame),
        "the still tail is frames {}..={last_frame}, got {frozen:?}",
        DETECTION_SEGMENT_FRAMES * 2
    );
}

const QUALITY_FPS: u32 = 24;
const QUALITY_FRAMES: u64 = 4;
const NOISE_WIDTH: u32 = 512;
const NOISE_HEIGHT: u32 = 270;
/// Well under what PSNR 60 needs for noise, so the fallback to rate allocation
/// is the only thing that can hold the run to it.
const NOISE_BYTE_CAP: u64 = 60_000;

fn write_lavfi_clip(path: &std::path::Path, source: &str, filters: &str) {
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            source,
            "-vf",
            filters,
            "-frames:v",
            &QUALITY_FRAMES.to_string(),
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv444p",
        ])
        .arg(path)
        .output()
        .expect("ffmpeg");
    assert!(
        ffmpeg.status.success(),
        "{}",
        String::from_utf8_lossy(&ffmpeg.stderr)
    );
}

fn encode_codestream_sizes(
    video: &std::path::Path,
    output: &std::path::Path,
    quality_psnr: Option<f64>,
    codestream_byte_cap: Option<u64>,
) -> Vec<u64> {
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let result = run_encode_with_options(
        video,
        output,
        &EncodeRunOptions {
            fps: FrameRate::whole(QUALITY_FPS),
            quality_psnr,
            codestream_byte_cap,
            ..Default::default()
        },
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("encode");
    assert_eq!(result.frames_encoded, QUALITY_FRAMES);

    let mut sizes: Vec<u64> = std::fs::read_dir(&result.j2k_dir)
        .expect("j2k dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "j2c"))
        .map(|entry| entry.metadata().expect("codestream").len())
        .collect();
    sizes.sort_unstable();
    assert_eq!(sizes.len() as u64, QUALITY_FRAMES);
    sizes
}

#[test]
fn a_higher_psnr_target_makes_larger_codestreams() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mkv");
    write_lavfi_clip(
        &video,
        &format!("testsrc=s={NOISE_WIDTH}x{NOISE_HEIGHT}:r={QUALITY_FPS}:d=1"),
        "format=yuv444p",
    );

    let at_40: u64 = encode_codestream_sizes(&video, &dir.path().join("psnr40"), Some(40.0), None)
        .iter()
        .sum();
    let at_50: u64 = encode_codestream_sizes(&video, &dir.path().join("psnr50"), Some(50.0), None)
        .iter()
        .sum();

    assert!(
        at_50 > at_40,
        "PSNR 50 should cost more than PSNR 40, got {at_50} and {at_40} bytes"
    );
}

#[test]
fn a_byte_cap_holds_where_the_psnr_target_cannot() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("noise.mkv");
    write_lavfi_clip(
        &video,
        &format!("nullsrc=s={NOISE_WIDTH}x{NOISE_HEIGHT}:r={QUALITY_FPS}:d=1"),
        "format=yuv444p,noise=alls=100:allf=t+u",
    );

    let uncapped = encode_codestream_sizes(&video, &dir.path().join("uncapped"), Some(60.0), None);
    assert!(
        uncapped[0] > NOISE_BYTE_CAP,
        "the noise clip has to be one PSNR 60 cannot fit under {NOISE_BYTE_CAP} bytes, \
         smallest codestream was {}",
        uncapped[0]
    );

    let capped = encode_codestream_sizes(
        &video,
        &dir.path().join("capped"),
        Some(60.0),
        Some(NOISE_BYTE_CAP),
    );
    assert!(
        capped.iter().all(|size| *size <= NOISE_BYTE_CAP),
        "every codestream should be at or under {NOISE_BYTE_CAP} bytes, got {capped:?}"
    );
}

/// dcpwizard's CLI `create` decodes through `encode_video_pipeline_resumable`,
/// which runs its own ffmpeg, so the findings have to come back from there too.
#[test]
fn the_resumable_pipeline_reports_its_own_findings() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    use postkit::grok_encoder::{CompressParams, EncodeProgress, encode_video_pipeline_resumable};

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mkv");
    write_detection_clip(&video);

    postkit::grok_encoder::initialize(0);
    let cancel = Arc::new(AtomicBool::new(false));
    let total_frames = DETECTION_SEGMENT_FRAMES * 3;
    let result = encode_video_pipeline_resumable(
        &video,
        &dir.path().join("j2k"),
        &CompressParams {
            edit_rate: FrameRate::whole(DETECTION_FPS),
            ..CompressParams::default()
        },
        total_frames,
        DETECTION_WIDTH,
        DETECTION_HEIGHT,
        &cancel,
        false,
        None,
        None,
        |_: EncodeProgress| {},
    );
    assert!(result.success, "{}", result.error);
    assert_eq!(result.frames_encoded, total_frames);

    let findings = &result.picture_findings;
    let black = findings
        .black
        .first()
        .unwrap_or_else(|| panic!("no black run reported, findings: {findings:?}"));
    assert!(
        within_one_frame(black.first_frame, 0)
            && within_one_frame(black.last_frame, DETECTION_SEGMENT_FRAMES - 1),
        "the black head is frames 0..={}, got {black:?}",
        DETECTION_SEGMENT_FRAMES - 1
    );

    let frozen = findings
        .frozen
        .last()
        .unwrap_or_else(|| panic!("no frozen run reported, findings: {findings:?}"));
    assert!(
        within_one_frame(frozen.first_frame, DETECTION_SEGMENT_FRAMES * 2)
            && within_one_frame(frozen.last_frame, total_frames - 1),
        "the still tail is frames {}..={}, got {frozen:?}",
        DETECTION_SEGMENT_FRAMES * 2,
        total_frames - 1
    );
}

/// 24000/1001 for as long as it takes the two conversions to disagree: the
/// black head ends at 25.025 s, which is 600.0 frames at the exact rate and
/// 600.6 at a rounded 24.
const NTSC_BLACK_FRAMES: u64 = 600;
const NTSC_TAIL_FRAMES: u64 = 24;

/// 600 frames of black then 24 of testsrc, all at 24000/1001. `d=25` stops the
/// black source at frame 600, whose pts is 25.025 s, so that is where the
/// testsrc starts.
fn write_ntsc_detection_clip(path: &std::path::Path) {
    let size = format!("{DETECTION_WIDTH}x{DETECTION_HEIGHT}");
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=black:s={size}:r=24000/1001:d=25"),
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={size}:r=24000/1001:d=1"),
            "-filter_complex",
            "[0:v][1:v]concat=n=2:v=1:a=0[v]",
            "-map",
            "[v]",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv444p",
        ])
        .arg(path)
        .output()
        .expect("ffmpeg");
    assert!(
        ffmpeg.status.success(),
        "{}",
        String::from_utf8_lossy(&ffmpeg.stderr)
    );
}

/// The findings come back as frame numbers, so the pipeline has to convert the
/// detection seconds at the rate the frames are actually at. Rounding
/// 24000/1001 to 24 puts the end of the black head one frame late, on the first
/// frame of the testsrc.
#[test]
fn the_resumable_pipeline_converts_findings_at_the_exact_rate() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }

    use postkit::grok_encoder::{CompressParams, EncodeProgress, encode_video_pipeline_resumable};

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("ntsc.mkv");
    write_ntsc_detection_clip(&video);

    let total_frames = NTSC_BLACK_FRAMES + NTSC_TAIL_FRAMES;
    assert_eq!(
        postkit::encode::probe_video(&video).2,
        total_frames,
        "the clip itself has to be {NTSC_BLACK_FRAMES} black frames then {NTSC_TAIL_FRAMES}"
    );

    postkit::grok_encoder::initialize(0);
    let cancel = Arc::new(AtomicBool::new(false));
    let result = encode_video_pipeline_resumable(
        &video,
        &dir.path().join("j2k"),
        &CompressParams {
            edit_rate: FrameRate::new(24000, 1001),
            ..CompressParams::default()
        },
        total_frames,
        DETECTION_WIDTH,
        DETECTION_HEIGHT,
        &cancel,
        false,
        None,
        None,
        |_: EncodeProgress| {},
    );
    assert!(result.success, "{}", result.error);
    assert_eq!(result.frames_encoded, total_frames);

    let findings = &result.picture_findings;
    let black = findings
        .black
        .first()
        .unwrap_or_else(|| panic!("no black run reported, findings: {findings:?}"));
    assert_eq!(black.first_frame, 0, "got {black:?}");
    assert_eq!(
        black.last_frame,
        NTSC_BLACK_FRAMES - 1,
        "the black head is frames 0..={}, got {black:?}",
        NTSC_BLACK_FRAMES - 1
    );
}
