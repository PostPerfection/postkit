//! IMF profile encoding: the codestream declares the IMF Rsiz it was given, the
//! samples stay RGB, and they are written at 12 bits.
//!
//! One test per entry point an IMF caller has: the in-process encoder, the
//! pipeline over an image sequence, the TIFF sequence encoder under it, and a
//! held still.

use postkit::encode::{
    FrameRate, SourceColour, StreamEncodeOptions, encode_tiff_sequence_inprocess,
};
use postkit::grok_encoder::{CompressParams, PipelineResult, RawFrame};
use postkit::j2k::{ImfProfile, imf_levels, imf_rsiz};
use postkit::pipeline::{EncodeRunOptions, run_encode_with_options};
use postkit::still::{StillHold, build_still_frames};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const FRAME_RATE: f64 = 24.0;
const BITS_PER_SECOND: u64 = 200_000_000;
/// The largest 12-bit sample, the red a saturated frame carries.
const FULL_SCALE_12BIT: u16 = 4095;
/// What red decodes back to once a lossy encode has been over it, and the most
/// the other two components may hold.
const LEAST_RED: i32 = 3900;
const MOST_GREEN_OR_BLUE: i32 = 200;

/// Rsiz for a 2K IMF picture at this test's raster and rate.
fn test_rsiz() -> u16 {
    let profile = ImfProfile::for_raster(WIDTH, HEIGHT).unwrap();
    assert_eq!(profile, ImfProfile::Imf2k);
    let levels = imf_levels(WIDTH, HEIGHT, FRAME_RATE, BITS_PER_SECOND).unwrap();
    imf_rsiz(profile, levels)
}

/// One flat rgb48be frame of pure red, the colour an X'Y'Z' transform would
/// spread over all three components.
fn red_frame() -> RawFrame {
    let mut data = Vec::with_capacity((WIDTH * HEIGHT * 6) as usize);
    for _ in 0..(WIDTH * HEIGHT) {
        data.extend_from_slice(&[0xff, 0xff, 0x00, 0x00, 0x00, 0x00]);
    }
    RawFrame::Packed {
        data,
        width: WIDTH,
        height: HEIGHT,
        precision: 16,
        index: 0,
    }
}

fn encode_one(rsiz: u16, apply_xyz_transform: bool, dir: &std::path::Path) -> PipelineResult {
    let params = CompressParams {
        compression_ratio: 1.0,
        num_resolutions: 3,
        profile: rsiz,
        apply_xyz_transform,
        ..CompressParams::default()
    };
    postkit::grok_encoder::initialize(0);
    let mut frame = Some(red_frame());
    postkit::grok_encoder::encode_pipeline(
        dir,
        &params,
        1,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(postkit::grok_encoder::PhaseClocks::default()),
        || frame.take(),
        |_| {},
    )
}

/// The codestream declares `rsiz`, carries 12-bit samples, and its centre pixel
/// decodes back as red.
fn assert_imf_red(codestream: &Path, rsiz: u16) {
    let data = std::fs::read(codestream)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", codestream.display()));
    let header = postkit::j2k::parse_j2k_header(&data).expect("codestream header");
    assert_eq!(header.profile, rsiz, "the IMF profile was not declared");
    assert_eq!(header.bit_depth, 12, "IMF picture has to be 12-bit");
    assert_eq!(header.bit_depths, vec![12, 12, 12]);

    let decoded = postkit::grok_decoder::decode(data, 0).expect("decode");
    let centre =
        (decoded.height as usize / 2) * decoded.width as usize + decoded.width as usize / 2;
    let (red, green, blue) = (
        decoded.components[0][centre],
        decoded.components[1][centre],
        decoded.components[2][centre],
    );
    assert!(
        red > LEAST_RED && green < MOST_GREEN_OR_BLUE && blue < MOST_GREEN_OR_BLUE,
        "red came back as {red},{green},{blue} from {}: an X'Y'Z' encode leaves all three \
         components large",
        codestream.display()
    );
}

#[test]
fn an_imf_encode_writes_12_bit_rgb_under_the_rsiz_it_was_given() {
    let rsiz = test_rsiz();
    let dir = tempfile::tempdir().unwrap();
    let result = encode_one(rsiz, false, dir.path());
    assert!(result.success, "IMF encode failed: {}", result.error);
    assert_imf_red(&dir.path().join("frame_00000000.j2c"), rsiz);
}

#[test]
fn an_imf_encode_refuses_the_xyz_transform() {
    let dir = tempfile::tempdir().unwrap();
    let result = encode_one(test_rsiz(), true, dir.path());
    assert!(!result.success, "an IMF X'Y'Z' encode has to be refused");
    assert!(
        result.error.contains("X'Y'Z'"),
        "the refusal has to name the transform: {}",
        result.error
    );
}

/// One uncompressed 12-bit RGB TIFF of a single colour, since an IMF sequence
/// is written at 12 bits.
fn write_12bit_tiff(path: &Path, colour: [u16; 3]) {
    let samples: Vec<u16> = colour.repeat((WIDTH * HEIGHT) as usize);
    postkit::grok::write_tiff_rgb(path, WIDTH, HEIGHT, 12, &samples).unwrap();
}

/// A directory holding one saturated red 12-bit TIFF.
fn red_sequence(dir: &Path) -> std::path::PathBuf {
    let frames = dir.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    write_12bit_tiff(&frames.join("frame_000.tif"), [FULL_SCALE_12BIT, 0, 0]);
    frames
}

#[test]
fn the_pipeline_encodes_an_image_sequence_under_the_imf_rsiz() {
    let rsiz = test_rsiz();
    let dir = tempfile::tempdir().unwrap();
    let frames = red_sequence(dir.path());
    let output = dir.path().join("out");
    let result = run_encode_with_options(
        &frames,
        &output,
        &EncodeRunOptions {
            rsiz,
            source_colour: SourceColour::KeepRgb,
            fps: FrameRate::whole(24),
            ..EncodeRunOptions::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
        |_| {},
    )
    .expect("IMF image sequence encode");
    assert_eq!(result.frames_encoded, 1);
    assert_imf_red(&result.j2k_dir.join("frame_00000000.j2c"), rsiz);
}

fn encode_tiffs(
    frames: &[std::path::PathBuf],
    output: &Path,
    source_colour: SourceColour,
) -> postkit::encode::EncodeResult {
    encode_tiff_sequence_inprocess(
        frames,
        &StreamEncodeOptions {
            output_dir: output.to_path_buf(),
            rsiz: test_rsiz(),
            source_colour,
            fps: FrameRate::whole(24),
            ..StreamEncodeOptions::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        None,
        |_| {},
    )
}

#[test]
fn the_tiff_encoder_writes_the_imf_rsiz_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let frames = postkit::encode::find_source_frames(&red_sequence(dir.path())).unwrap();
    let output = dir.path().join("j2k");
    let result = encode_tiffs(&frames, &output, SourceColour::KeepRgb);
    assert!(result.success, "IMF tiff encode failed: {}", result.error);
    assert_imf_red(&output.join("frame_00000000.j2c"), test_rsiz());
}

#[test]
fn the_tiff_encoder_refuses_the_xyz_transform_under_an_imf_rsiz() {
    let dir = tempfile::tempdir().unwrap();
    let frames = postkit::encode::find_source_frames(&red_sequence(dir.path())).unwrap();
    let result = encode_tiffs(&frames, &dir.path().join("j2k"), SourceColour::DisplayRgb);
    assert!(!result.success, "an IMF X'Y'Z' encode has to be refused");
    assert!(
        result.error.contains("X'Y'Z'"),
        "the refusal has to name the transform: {}",
        result.error
    );
}

#[test]
fn a_held_still_is_encoded_under_the_imf_rsiz() {
    let rsiz = test_rsiz();
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("card.png");
    let made = std::process::Command::new("ffmpeg")
        .args(["-y", "-f", "lavfi", "-i"])
        .arg(format!("color=c=red:s={WIDTH}x{HEIGHT}"))
        .args(["-frames:v", "1"])
        .arg(&image)
        .output()
        .expect("ffmpeg");
    assert!(
        made.status.success(),
        "could not write the still: {}",
        String::from_utf8_lossy(&made.stderr)
    );

    let held = dir.path().join("held");
    let hold = |apply_xyz_transform| StillHold {
        image: &image,
        frames: 2,
        fps: FrameRate::whole(24),
        width: WIDTH,
        height: HEIGHT,
        filters: &[],
        apply_xyz_transform,
        rsiz,
        colour_transform: None,
        burn: None,
        out_dir: &held,
    };
    build_still_frames(&hold(false)).expect("IMF still encode");
    assert_imf_red(&held.join("frame_00000000.j2c"), rsiz);
    assert_imf_red(&held.join("frame_00000001.j2c"), rsiz);

    let refused =
        build_still_frames(&hold(true)).expect_err("an IMF X'Y'Z' hold has to be refused");
    assert!(
        refused.contains("X'Y'Z'"),
        "the refusal has to name the transform: {refused}"
    );
}
