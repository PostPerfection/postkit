//! IMF App 2E preview: an AS-02 track file extracts through grok and the
//! Rec.709 pass-through, and any colour the pass-through would show wrong is
//! refused by name.
//!
//! The picture is encoded in process by the same grok that decodes it back, so
//! nothing here spawns a tool. Frames are written as PPM, which postkit writes
//! itself, because the runners have no ffmpeg.

use asdcplib::jp2k::{
    COLOR_PRIMARIES_BT709, COLOR_PRIMARIES_P3D65, HdrMetadata, TRANSFER_CHARACTERISTIC_ST2084,
};
use postkit::j2k::{ImfProfile, J2kProfile, imf_levels, imf_rsiz};
use postkit::mxf_wrap::{
    EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap, rec709_sdr_picture_colour,
};
use postkit::preview::{self, DcpPreviewOptions};
use std::path::{Path, PathBuf};

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const FRAME_RATE: f64 = 24.0;
const BITS_PER_SECOND: u64 = 200_000_000;
/// The largest 12-bit sample, what the red component is encoded at.
const FULL_SCALE_12BIT: i32 = 4095;
/// Full-scale red as 8-bit sRGB, the pass-through's whole job.
const RED_PIXEL: [u8; 3] = [255, 0, 0];

const IMF_4K_FIXTURE: &str = "imf4k_black_3840x2160.j2c";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .expect("fixture codestream")
}

/// A finished wrap, holding its temporary directory open so the MXF can be read.
struct Wrapped {
    _dir: tempfile::TempDir,
    output: PathBuf,
}

fn wrap_app2e(frame: Vec<u8>, hdr: HdrMetadata) -> Wrapped {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("frame_00000000.j2c");
    std::fs::write(&input, frame).unwrap();
    let output = dir.path().join("picture.mxf");
    let result = mxf_wrap(&MxfWrapOptions {
        input_files: vec![input],
        output: output.clone(),
        essence_type: EssenceType::J2k,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: Some(hdr),
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(result.success, "wrap failed: {}", result.error);
    Wrapped { _dir: dir, output }
}

/// Pure red at 12 bits, encoded losslessly under this raster's IMF Rsiz so the
/// decoded samples are exactly what went in.
fn encode_red_imf_frame() -> Vec<u8> {
    let samples = (WIDTH * HEIGHT) as usize;
    let components = [
        vec![FULL_SCALE_12BIT; samples],
        vec![0; samples],
        vec![0; samples],
    ];
    let profile = ImfProfile::for_raster(WIDTH, HEIGHT).unwrap();
    let levels = imf_levels(WIDTH, HEIGHT, FRAME_RATE, BITS_PER_SECOND).unwrap();
    let params = postkit::grok_encoder::CompressParams {
        irreversible: false,
        compression_ratio: 1.0,
        mct: false,
        apply_xyz_transform: false,
        profile: imf_rsiz(profile, levels),
        num_resolutions: 3,
        ..postkit::grok_encoder::CompressParams::default()
    };
    postkit::grok_encoder::initialize(0);
    let dir = tempfile::tempdir().unwrap();
    let mut frame = Some(postkit::grok_encoder::RawFrame::Planar {
        components,
        width: WIDTH,
        height: HEIGHT,
        precision: 12,
        index: 0,
    });
    let result = postkit::grok_encoder::encode_pipeline(
        dir.path(),
        &params,
        1,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &std::sync::Arc::new(postkit::grok_encoder::PhaseClocks::default()),
        || frame.take(),
        |_| {},
    );
    assert!(result.success, "IMF encode failed: {}", result.error);
    let j2c = std::fs::read(dir.path().join("frame_00000000.j2c")).expect("encoded codestream");
    let header = postkit::j2k::parse_j2k_header(&j2c).expect("codestream header");
    assert_eq!(
        J2kProfile::from(header.profile),
        J2kProfile::Imf,
        "rsiz {:#06x} is not an IMF profile, so extraction would not take the IMF route",
        header.profile
    );
    j2c
}

fn out_path(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "postkit-imf-{tag}-{}-{nanos}.ppm",
        std::process::id()
    ))
}

#[test]
fn extract_frame_shows_rec709_app2e_picture_through_grok() {
    let wrapped = wrap_app2e(encode_red_imf_frame(), rec709_sdr_picture_colour());
    let resolved = preview::resolve_picture(&wrapped.output).unwrap();
    assert!(
        resolved.as02,
        "the wrap has to read back as AS-02 or the AS-DCP reader would fail every read"
    );

    let out = out_path("red");
    assert_eq!(
        preview::extract_frame(&wrapped.output, 0, &out, None),
        0,
        "an App 2E frame has to extract"
    );

    let pixels = read_ppm(&out, WIDTH, HEIGHT);
    let dcdm = postkit::colour::XyzToSrgb::new().pixel(FULL_SCALE_12BIT as u16, 0, 0);
    assert!(
        pixels.chunks(3).all(|px| px == RED_PIXEL),
        "expected every pixel to be {RED_PIXEL:?}: full-scale red in Rec.709 RGB drops its low \
         four bits and reaches the screen unchanged. Found {:?}, and the DCDM inverse this frame \
         would take on the DCP route gives {dcdm:?}",
        pixels.chunks(3).find(|px| *px != RED_PIXEL)
    );
    std::fs::remove_file(&out).ok();
}

#[test]
fn extract_frame_shows_app2e_picture_with_no_transfer_characteristic() {
    // Netflix's Meridian signals nothing, so unsignalled colour reads as Rec.709
    let wrapped = wrap_app2e(
        encode_red_imf_frame(),
        HdrMetadata {
            color_primaries: Some(COLOR_PRIMARIES_BT709),
            transfer_characteristic: None,
            ..Default::default()
        },
    );
    let out = out_path("notransfer");
    assert_eq!(
        preview::extract_frame(&wrapped.output, 0, &out, None),
        0,
        "a missing transfer characteristic has to fall back to Rec.709, not refuse"
    );
    let pixels = read_ppm(&out, WIDTH, HEIGHT);
    assert!(
        pixels.chunks(3).all(|px| px == RED_PIXEL),
        "expected every pixel to be {RED_PIXEL:?}, found {:?}",
        pixels.chunks(3).find(|px| *px != RED_PIXEL)
    );
    std::fs::remove_file(&out).ok();
}

#[test]
fn pq_app2e_picture_is_refused_by_name() {
    let pq = HdrMetadata {
        color_primaries: Some(COLOR_PRIMARIES_P3D65),
        transfer_characteristic: Some(TRANSFER_CHARACTERISTIC_ST2084),
        ..Default::default()
    };
    // the encoded frame, and the real Netflix codestream its CPL calls P3D65 PQ
    for frame in [encode_red_imf_frame(), fixture(IMF_4K_FIXTURE)] {
        let wrapped = wrap_app2e(frame, pq.clone());
        let opts = DcpPreviewOptions {
            source: wrapped.output.clone(),
            ..Default::default()
        };
        let out = out_path("pq");
        let err = preview::render_imf_frame(&opts, 0, &out).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("ST 2084") && message.contains("PQ"),
            "the refusal has to name the transfer characteristic it cannot show: {message}"
        );
        assert_eq!(
            preview::extract_frame(&wrapped.output, 0, &out, None),
            -1,
            "PQ picture must not extract as if it were Rec.709"
        );
        assert!(!out.exists(), "a refused frame must not leave an image");
    }
}

/// The RGB bytes of a binary PPM, checking it is the size that was asked for.
fn read_ppm(path: &Path, width: u32, height: u32) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("ppm written");
    assert!(bytes.starts_with(b"P6"), "not a binary ppm");
    let mut fields = 0;
    let mut index = 2;
    while fields < 3 && index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let field: u32 = std::str::from_utf8(&bytes[start..index])
            .unwrap()
            .parse()
            .unwrap();
        match fields {
            0 => assert_eq!(field, width, "ppm width"),
            1 => assert_eq!(field, height, "ppm height"),
            _ => assert_eq!(field, 255, "8 bits per channel"),
        }
        fields += 1;
    }
    let pixels = bytes[index + 1..].to_vec();
    assert_eq!(
        pixels.len(),
        (width * height * 3) as usize,
        "the ppm holds the wrong number of samples"
    );
    pixels
}
