//! IMF profile encoding: the codestream declares the IMF Rsiz it was given, the
//! samples stay RGB, and they are written at 12 bits.
//!
//! One test per entry point an IMF caller has: the in-process encoder, the
//! pipeline over an image sequence, the parallel grk_compress encoder under it,
//! and a held still.

use postkit::encode::{FrameRate, SourceColour, encode_parallel};
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

/// One uncompressed 12-bit RGB TIFF of a single colour. grk_compress writes each
/// codestream at the precision of the file it read, so an IMF image sequence
/// starts at 12 bits.
fn write_12bit_tiff(path: &Path, colour: [u16; 3]) {
    const BITS_PER_SAMPLE: u32 = 12;
    const SAMPLES_PER_PIXEL: u32 = 3;
    const IFD_ENTRIES: u16 = 10;
    const SHORT: u16 = 3;
    const LONG: u16 = 4;

    let mut row: Vec<u8> = Vec::new();
    let mut accumulator: u32 = 0;
    let mut pending_bits = 0u32;
    for _ in 0..WIDTH {
        for sample in colour {
            accumulator = (accumulator << BITS_PER_SAMPLE) | u32::from(sample);
            pending_bits += BITS_PER_SAMPLE;
            while pending_bits >= 8 {
                pending_bits -= 8;
                row.push(((accumulator >> pending_bits) & 0xff) as u8);
            }
        }
    }
    assert_eq!(pending_bits, 0, "a TIFF row has to end on a byte");
    let pixels: Vec<u8> = row.repeat(HEIGHT as usize);

    let ifd_size = 2 + u32::from(IFD_ENTRIES) * 12 + 4;
    let bits_per_sample_offset = 8 + ifd_size;
    let pixels_offset = bits_per_sample_offset + 6;
    let entries: [(u16, u16, u32, u32); IFD_ENTRIES as usize] = [
        (256, SHORT, 1, WIDTH),
        (257, SHORT, 1, HEIGHT),
        (258, SHORT, 3, bits_per_sample_offset),
        (259, SHORT, 1, 1),
        (262, SHORT, 1, 2),
        (273, LONG, 1, pixels_offset),
        (277, SHORT, 1, SAMPLES_PER_PIXEL),
        (278, SHORT, 1, HEIGHT),
        (279, LONG, 1, pixels.len() as u32),
        (284, SHORT, 1, 1),
    ];

    let mut tiff: Vec<u8> = b"II".to_vec();
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&IFD_ENTRIES.to_le_bytes());
    for (tag, field_type, count, value) in entries {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&field_type.to_le_bytes());
        tiff.extend_from_slice(&count.to_le_bytes());
        if field_type == SHORT && count == 1 {
            tiff.extend_from_slice(&(value as u16).to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
        } else {
            tiff.extend_from_slice(&value.to_le_bytes());
        }
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..SAMPLES_PER_PIXEL {
        tiff.extend_from_slice(&(BITS_PER_SAMPLE as u16).to_le_bytes());
    }
    assert_eq!(tiff.len() as u32, pixels_offset);
    tiff.extend_from_slice(&pixels);
    std::fs::write(path, tiff).unwrap();
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
    assert!(
        postkit::grok::find_grk_compress().is_some(),
        "grk_compress is required for this test"
    );
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
    assert_imf_red(&result.j2k_dir.join("frame_000.j2k"), rsiz);
}

#[test]
fn the_parallel_encoder_writes_the_imf_rsiz_it_is_given() {
    assert!(
        postkit::grok::find_grk_compress().is_some(),
        "grk_compress is required for this test"
    );
    let rsiz = test_rsiz();
    let dir = tempfile::tempdir().unwrap();
    let frames = postkit::encode::find_source_frames(&red_sequence(dir.path())).unwrap();
    let output = dir.path().join("j2k");
    let result = encode_parallel(
        &frames,
        &output,
        10.0,
        None,
        None,
        rsiz,
        FrameRate::whole(24),
        &SourceColour::KeepRgb,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    );
    assert!(
        result.success,
        "IMF parallel encode failed: {}",
        result.error
    );
    assert_imf_red(&output.join("frame_000.j2k"), rsiz);
}

#[test]
fn the_parallel_encoder_refuses_the_xyz_transform_under_an_imf_rsiz() {
    let dir = tempfile::tempdir().unwrap();
    let frames = vec![dir.path().join("frame_000.tif")];
    let result = encode_parallel(
        &frames,
        &dir.path().join("j2k"),
        10.0,
        None,
        None,
        test_rsiz(),
        FrameRate::whole(24),
        &SourceColour::DisplayRgb,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    );
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
