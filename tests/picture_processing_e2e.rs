//! Source picture processing through the real encode, end to end.
//!
//! The plan arithmetic is unit-tested inside the module; what these tests prove
//! is that ffmpeg accepts the chain the plan spells, that the encoder slices the
//! processed frame size rather than the source's, and that black border
//! detection reads back the bars a clip really has.

use postkit::encode::{DecodeSource, FrameRate};
use postkit::picture_processing::{Crop, Fit, PictureProcessing};
use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_with_options};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const FRAME_COUNT: u64 = 2;
const SOURCE_WIDTH: u32 = 1920;
const SOURCE_HEIGHT: u32 = 1080;
/// DCI scope, which is the box a flat-framed source is fitted into.
const BOX_WIDTH: u32 = 2048;
const BOX_HEIGHT: u32 = 858;
/// DCI 2K full container, the raster the codestream has to declare.
const RASTER_WIDTH: u32 = 2048;
const RASTER_HEIGHT: u32 = 1080;
/// Height of the black bar the detection clip is padded with, top and bottom.
const BAR_HEIGHT: u32 = 140;
/// cropdetect rounds to even, so a detected edge may miss by this much.
const DETECTION_TOLERANCE: u32 = 2;
/// How far below full scale a padded pixel has to come back. The wavelet is
/// lossy, so exact zero does not survive the round trip.
const BLACK_CEILING_DIVISOR: u16 = 16;

/// How much of full scale the test pattern has to reach where there is picture.
const CONTENT_FLOOR_DIVISOR: u16 = 2;

const SEQUENCE_SIZE: u32 = 128;
const SEQUENCE_CROP: u32 = 16;

/// A testsrc clip, optionally run through an extra filter chain.
fn make_clip(video: &Path, width: u32, height: u32, filters: Option<&str>) {
    let mut command = std::process::Command::new("ffmpeg");
    command.args([
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc=s={width}x{height}:d=1:r={FRAME_COUNT}"),
        "-frames:v",
        &FRAME_COUNT.to_string(),
    ]);
    if let Some(filters) = filters {
        command.args(["-vf", filters]);
    }
    let output = command
        .args(["-pix_fmt", "yuv420p"])
        .arg(video)
        .output()
        .expect("ffmpeg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn encode(input: &Path, output: &Path, picture: PictureProcessing) -> PathBuf {
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    run_encode_with_options(
        input,
        output,
        &EncodeRunOptions {
            fps: FrameRate::whole(FRAME_COUNT as u32),
            picture,
            ..Default::default()
        },
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |message: &str| eprintln!("{message}"),
    )
    .expect("encode")
    .j2k_dir
}

/// A decoded frame: its raster, its full-scale value (the codestreams are
/// 12-bit DCI, so the samples top out at 4095), and its samples.
struct DecodedFrame {
    width: u32,
    height: u32,
    full_scale: u16,
    samples: Vec<u16>,
}

impl DecodedFrame {
    /// Brightest sample on one row.
    fn row_peak(&self, row: u32) -> u16 {
        let start = (row * self.width * 3) as usize;
        let end = start + (self.width * 3) as usize;
        self.samples[start..end].iter().copied().max().unwrap_or(0)
    }
}

/// Decode one codestream in memory.
fn decode_frame(codestream: &Path) -> DecodedFrame {
    let data = std::fs::read(codestream).expect("codestream");
    let frame = postkit::grok_decoder::decode(data, 0)
        .unwrap_or_else(|e| panic!("cannot decode {}: {e}", codestream.display()));
    DecodedFrame {
        width: frame.width,
        height: frame.height,
        full_scale: ((1u32 << frame.precision) - 1) as u16,
        samples: frame.interleaved_samples().expect("three components"),
    }
}

#[test]
fn a_cropped_source_lands_centred_on_the_target_raster() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    make_clip(&video, SOURCE_WIDTH, SOURCE_HEIGHT, None);

    let picture = PictureProcessing {
        crop: Crop::to_aspect(SOURCE_WIDTH, SOURCE_HEIGHT, BOX_WIDTH, BOX_HEIGHT),
        fit: Some(Fit {
            box_width: BOX_WIDTH,
            box_height: BOX_HEIGHT,
            raster_width: RASTER_WIDTH,
            raster_height: RASTER_HEIGHT,
        }),
        ..PictureProcessing::default()
    };
    let plan = picture.plan(SOURCE_WIDTH, SOURCE_HEIGHT).expect("plan");
    eprintln!("plan: {}", plan.describe());
    eprintln!("filters: {}", plan.filters.join(","));
    assert_eq!(
        (plan.output_width, plan.output_height),
        (RASTER_WIDTH, RASTER_HEIGHT)
    );
    assert!(plan.pad_top > 0, "a scope picture pads onto full container");

    let j2k_dir = encode(&video, &dir.path().join("out"), picture);
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&j2k_dir),
        FRAME_COUNT,
        "one codestream per frame in {}",
        j2k_dir.display()
    );

    let first = j2k_dir.join("frame_00000000.j2c");
    let header = postkit::j2k::parse_j2k_header(&std::fs::read(&first).expect("read codestream"))
        .expect("codestream header");
    assert_eq!(
        (header.width, header.height),
        (RASTER_WIDTH, RASTER_HEIGHT),
        "the codestream has to declare the fitted raster, not the source"
    );

    let frame = decode_frame(&first);
    assert_eq!((frame.width, frame.height), (RASTER_WIDTH, RASTER_HEIGHT));
    assert_eq!(
        frame.samples.len(),
        (frame.width * frame.height * 3) as usize
    );

    let black_ceiling = frame.full_scale / BLACK_CEILING_DIVISOR;
    let content_floor = frame.full_scale / CONTENT_FLOOR_DIVISOR;
    let top_pad = frame.row_peak(plan.pad_top / 2);
    let bottom_pad = frame.row_peak(frame.height - plan.pad_top / 2 - 1);
    let picture_row = frame.row_peak(frame.height / 2);
    eprintln!(
        "row peaks of {}: top pad {top_pad}, bottom pad {bottom_pad}, middle {picture_row}",
        frame.full_scale
    );
    assert!(
        top_pad < black_ceiling && bottom_pad < black_ceiling,
        "the pad rows are not black: top peak {top_pad}, bottom peak {bottom_pad}, ceiling {black_ceiling}"
    );
    assert!(
        picture_row > content_floor,
        "the fitted picture is missing: peak {picture_row} on the middle row"
    );

    // the first rows of picture sit right under the pad, so this is the
    // boundary the pad geometry puts them at
    let first_picture_row = frame.row_peak(plan.pad_top + 2);
    assert!(
        first_picture_row > content_floor,
        "no content at row {}, so the picture is not where the plan says",
        plan.pad_top + 2
    );
}

#[test]
fn black_bars_are_detected_from_a_few_sampled_frames() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("barred.mp4");
    let content_height = SOURCE_HEIGHT - 2 * BAR_HEIGHT;
    make_clip(
        &video,
        SOURCE_WIDTH,
        content_height,
        Some(&format!(
            "pad={SOURCE_WIDTH}:{SOURCE_HEIGHT}:0:{BAR_HEIGHT}:black"
        )),
    );

    let crop =
        postkit::picture_processing::detect_black_borders(&video, DecodeSource::Video, 0.1, 3)
            .expect("detect black borders");
    eprintln!("detected {crop:?}");
    assert!(
        crop.top.abs_diff(BAR_HEIGHT) <= DETECTION_TOLERANCE,
        "top bar detected as {}",
        crop.top
    );
    assert!(
        crop.bottom.abs_diff(BAR_HEIGHT) <= DETECTION_TOLERANCE,
        "bottom bar detected as {}",
        crop.bottom
    );
    assert_eq!((crop.left, crop.right), (0, 0), "there are no side bars");

    let plan = PictureProcessing {
        crop,
        ..PictureProcessing::default()
    }
    .plan(SOURCE_WIDTH, SOURCE_HEIGHT)
    .expect("plan");
    assert!(
        plan.output_height.abs_diff(content_height) <= 2 * DETECTION_TOLERANCE,
        "cropping the detected bars should leave the content: {}",
        plan.describe()
    );
}

#[test]
fn an_image_sequence_with_a_crop_encodes_at_the_cropped_size() {
    let dir = tempfile::tempdir().unwrap();
    let frames_dir = dir.path().join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    for index in 0..FRAME_COUNT {
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=s={SEQUENCE_SIZE}x{SEQUENCE_SIZE}:d=1:r=1"),
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgb48be",
            ])
            .arg(frames_dir.join(format!("frame_{index:04}.tiff")))
            .output()
            .expect("ffmpeg");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let cropped = SEQUENCE_SIZE - 2 * SEQUENCE_CROP;
    let j2k_dir = encode(
        &frames_dir,
        &dir.path().join("out"),
        PictureProcessing {
            crop: Crop {
                left: SEQUENCE_CROP,
                right: SEQUENCE_CROP,
                top: SEQUENCE_CROP,
                bottom: SEQUENCE_CROP,
            },
            ..PictureProcessing::default()
        },
    );

    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&j2k_dir),
        FRAME_COUNT,
        "one codestream per source image in {}",
        j2k_dir.display()
    );
    for index in 0..FRAME_COUNT {
        let frame = j2k_dir.join(format!("frame_{index:08}.j2c"));
        let header =
            postkit::j2k::parse_j2k_header(&std::fs::read(&frame).expect("read codestream"))
                .unwrap_or_else(|| panic!("{} is not a J2K codestream", frame.display()));
        assert_eq!(
            (header.width, header.height),
            (cropped, cropped),
            "the sequence has to go through ffmpeg and come out cropped"
        );
    }
}
