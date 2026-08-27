//! IMF profile encoding: the codestream declares the IMF Rsiz it was given, the
//! samples stay RGB, and they are written at 12 bits.

use postkit::grok_encoder::{CompressParams, PipelineResult, RawFrame};
use postkit::j2k::{ImfProfile, imf_levels, imf_rsiz};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const FRAME_RATE: f64 = 24.0;
const BITS_PER_SECOND: u64 = 200_000_000;
/// The largest 12-bit sample, what pure red decodes back to.
const FULL_SCALE_12BIT: i32 = 4095;

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

#[test]
fn an_imf_encode_writes_12_bit_rgb_under_the_rsiz_it_was_given() {
    let rsiz = test_rsiz();
    let dir = tempfile::tempdir().unwrap();
    let result = encode_one(rsiz, false, dir.path());
    assert!(result.success, "IMF encode failed: {}", result.error);

    let codestream = std::fs::read(dir.path().join("frame_00000000.j2c")).unwrap();
    let header = postkit::j2k::parse_j2k_header(&codestream).expect("codestream header");
    assert_eq!(header.profile, rsiz, "the IMF profile was not declared");
    assert_eq!(header.bit_depth, 12, "IMF picture has to be 12-bit");
    assert_eq!(header.bit_depths, vec![12, 12, 12]);

    let decoded = postkit::grok_decoder::decode(codestream, 0).expect("decode");
    let centre =
        (decoded.height as usize / 2) * decoded.width as usize + decoded.width as usize / 2;
    let (red, green, blue) = (
        decoded.components[0][centre],
        decoded.components[1][centre],
        decoded.components[2][centre],
    );
    // an X'Y'Z' encode of red leaves all three components large
    assert!(
        red > FULL_SCALE_12BIT - 64 && green < 64 && blue < 64,
        "red came back as {red},{green},{blue}, so the samples are not RGB"
    );
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
