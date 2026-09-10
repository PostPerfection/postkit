//! IMF App 2E preview: an AS-02 track file renders through grok and the
//! display transform in `postkit::preview_colour`, and only a colour UL that
//! transform has no reading for is refused by name.
//!
//! The picture is encoded in process by the same grok that decodes it back, so
//! nothing here spawns a tool. Frames are written as PPM, which postkit writes
//! itself, because the runners have no ffmpeg.

use asdcplib::jp2k::{
    COLOR_PRIMARIES_BT709, COLOR_PRIMARIES_BT2020, COLOR_PRIMARIES_P3D65, HdrMetadata,
    TRANSFER_CHARACTERISTIC_BT709, TRANSFER_CHARACTERISTIC_BT2020, TRANSFER_CHARACTERISTIC_ST2084,
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
/// Half of the 12-bit range, the dim patch that shows the gamut matrix moving.
const HALF_SCALE_12BIT: i32 = 2048;
/// Full-scale red as 8-bit sRGB, the pass-through's whole job.
const RED_PIXEL: [u8; 3] = [255, 0, 0];
/// The white a reference SDR display shows, and what the tone map lands on.
const WHITE_PIXEL: [u8; 3] = [255, 255, 255];
/// ITU-R BT.2408 puts HDR reference white here.
const REFERENCE_WHITE_NITS: f64 = 203.0;
/// The peak the tone map maps down from when the descriptor signals no
/// mastering display, and the HLG nominal peak.
const NOMINAL_PEAK_NITS: f64 = 1000.0;
/// How far apart the channels of a patch that has to read neutral may sit.
const NEUTRAL_TOLERANCE: i32 = 2;
/// The band reference white lands in. It sits near the top because the BT.2390
/// EETF maps the source peak onto the SDR peak, so 203 nits is 0.88 of it.
const REFERENCE_WHITE_BAND: std::ops::RangeInclusive<u8> = 140..=245;
/// What a channel outside the Rec.709 gamut may reach after the clip.
const OUT_OF_GAMUT_CEILING: u8 = 8;
/// The 4:2:2 frame's flat luma, and the chroma midpoint that means no colour.
const NARROW_MID_GREY_12BIT: i32 = 2048;
const NARROW_CHROMA_MIDPOINT: i32 = 2048;
/// The red chroma the right half of the 4:2:2 frame steps up to.
const RED_CHROMA_HIGH_12BIT: i32 = 2500;
const YCBCR_TRIPLE: [i32; 3] = [2048, 1700, 2300];
const NARROW_LUMA_BLACK: f64 = 256.0;
const NARROW_LUMA_WHITE: f64 = 3760.0;
const NARROW_CHROMA_HALF_RANGE: f64 = 1792.0;
const BT709_LUMA_RED_BLUE: [f64; 2] = [0.2126, 0.0722];
const MATRIX_TOLERANCE: i32 = 2;
const MXF_HEADER_SIZE: u32 = 16384;

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

fn wrap_app2e_cdci(frame: &[u8], hdr: HdrMetadata) -> Wrapped {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("picture.mxf");
    let descriptor = asdcplib::jp2k::PictureDescriptor {
        edit_rate: asdcplib::Rational::new(24, 1),
        sample_rate: asdcplib::Rational::new(24, 1),
        stored_width: WIDTH,
        stored_height: HEIGHT,
        aspect_ratio: asdcplib::Rational::new(WIDTH as i32, HEIGHT as i32),
        container_duration: 1,
        codestream: asdcplib::jp2k::CodestreamHeader::parse(frame).expect("codestream header"),
    };
    let mut writer = asdcplib::as02::jp2k::MxfWriter::new();
    writer
        .open_write_cdci(
            &output.to_string_lossy(),
            &asdcplib::WriterInfo::default(),
            &descriptor,
            asdcplib::as02::jp2k::ChromaSubsampling {
                horizontal: 1,
                vertical: 1,
            },
            Some(&hdr),
            MXF_HEADER_SIZE,
        )
        .expect("a 4:4:4 CDCI wrap has to open");
    writer
        .write_frame(frame, None, None)
        .expect("frame written");
    writer.finalize().expect("wrap finalized");
    Wrapped { _dir: dir, output }
}

/// Pure red at 12 bits, encoded losslessly under this raster's IMF Rsiz so the
/// decoded samples are exactly what went in.
fn encode_red_imf_frame() -> Vec<u8> {
    let samples = (WIDTH * HEIGHT) as usize;
    encode_imf_frame([
        vec![FULL_SCALE_12BIT; samples],
        vec![0; samples],
        vec![0; samples],
    ])
}

/// Three 12-bit planes as one losslessly encoded App 2E codestream, so the
/// decoded samples are exactly what went in.
fn encode_imf_frame(components: [Vec<i32>; 3]) -> Vec<u8> {
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

/// A frame of equal-width vertical patches, each a flat 12-bit RGB triple.
fn patch_frame(patches: &[[i32; 3]]) -> [Vec<i32>; 3] {
    let patch_width = WIDTH as usize / patches.len();
    assert_eq!(
        patch_width * patches.len(),
        WIDTH as usize,
        "the patches have to divide the raster evenly"
    );
    std::array::from_fn(|channel| {
        let row: Vec<i32> = patches
            .iter()
            .flat_map(|patch| std::iter::repeat_n(patch[channel], patch_width))
            .collect();
        row.repeat(HEIGHT as usize)
    })
}

/// Wrap, render and read back the centre pixel of every patch.
fn render_patches(hdr: HdrMetadata, patches: &[[i32; 3]], tag: &str) -> Vec<[u8; 3]> {
    let wrapped = wrap_app2e(encode_imf_frame(patch_frame(patches)), hdr);
    let opts = DcpPreviewOptions {
        source: wrapped.output.clone(),
        ..Default::default()
    };
    let out = out_path(tag);
    preview::render_imf_frame(&opts, 0, &out).expect("the frame has to render");
    let pixels = read_ppm(&out, WIDTH, HEIGHT);
    std::fs::remove_file(&out).ok();

    let patch_width = WIDTH as usize / patches.len();
    let row = HEIGHT as usize / 2;
    (0..patches.len())
        .map(|patch| {
            let column = patch * patch_width + patch_width / 2;
            let offset = (row * WIDTH as usize + column) * 3;
            [pixels[offset], pixels[offset + 1], pixels[offset + 2]]
        })
        .collect()
}

/// SMPTE ST 2084's inverse EOTF written out from its own constants, so the
/// expected code values do not come from the transform under test.
fn pq_code(nits: f64) -> i32 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    const PEAK_NITS: f64 = 10000.0;
    let luminance = (nits / PEAK_NITS).clamp(0.0, 1.0).powf(M1);
    let signal = ((C1 + C2 * luminance) / (1.0 + C3 * luminance)).powf(M2);
    (signal * f64::from(FULL_SCALE_12BIT)).round() as i32
}

fn assert_neutral(pixel: [u8; 3], what: &str) {
    let spread = pixel.iter().map(|&c| i32::from(c)).collect::<Vec<_>>();
    let low = *spread.iter().min().unwrap();
    let high = *spread.iter().max().unwrap();
    assert!(
        high - low <= NEUTRAL_TOLERANCE,
        "{what} has to render neutral, found {pixel:?}"
    );
}

fn pq_bt2020() -> HdrMetadata {
    HdrMetadata {
        color_primaries: Some(COLOR_PRIMARIES_BT2020),
        transfer_characteristic: Some(TRANSFER_CHARACTERISTIC_ST2084),
        ..Default::default()
    }
}

#[test]
fn a_pq_frame_tone_maps_onto_the_sdr_range() {
    let flat = |code: i32| [code, code, code];
    let patches = [
        flat(0),
        flat(pq_code(10.0)),
        flat(pq_code(REFERENCE_WHITE_NITS)),
        flat(pq_code(NOMINAL_PEAK_NITS)),
    ];
    let rendered = render_patches(pq_bt2020(), &patches, "pq");
    let (black, dim, reference_white, peak) = (rendered[0], rendered[1], rendered[2], rendered[3]);

    assert_eq!(black, [0, 0, 0], "PQ black has to stay black");

    assert_neutral(reference_white, "BT.2408 reference white");
    assert!(
        REFERENCE_WHITE_BAND.contains(&reference_white[1]),
        "{REFERENCE_WHITE_NITS} nits rendered at {reference_white:?}, outside \
         {REFERENCE_WHITE_BAND:?}"
    );

    assert_eq!(
        peak, WHITE_PIXEL,
        "the source peak has to land exactly on the SDR peak, found {peak:?}"
    );
    assert!(
        peak[1] > reference_white[1],
        "the {NOMINAL_PEAK_NITS} nit patch has to outshine reference white"
    );

    assert!(
        dim[1] > 0 && dim[1] < reference_white[1],
        "10 nits has to render above black and below reference white, found {dim:?}"
    );
}

#[test]
fn a_bt2020_red_patch_clips_its_other_channels_into_rec709() {
    let rendered = render_patches(pq_bt2020(), &[[pq_code(100.0), 0, 0]], "bt2020red");
    let [red, green, blue] = rendered[0];
    assert!(
        red > green && red > blue,
        "BT.2020 red has to stay red, found {:?}",
        rendered[0]
    );
    assert!(
        green <= OUT_OF_GAMUT_CEILING && blue <= OUT_OF_GAMUT_CEILING,
        "BT.2020 red is outside Rec.709, so green and blue clip near zero, found {:?}",
        rendered[0]
    );
}

#[test]
fn an_hlg_frame_puts_its_reference_white_in_the_same_band() {
    /// ITU-R BT.2408 puts HLG reference white at 75% of the signal range.
    const HLG_REFERENCE_WHITE_SIGNAL: f64 = 0.75;
    let code = (HLG_REFERENCE_WHITE_SIGNAL * f64::from(FULL_SCALE_12BIT)).round() as i32;
    let hlg = HdrMetadata {
        color_primaries: Some(COLOR_PRIMARIES_BT2020),
        transfer_characteristic: Some(TRANSFER_CHARACTERISTIC_BT2020),
        ..Default::default()
    };
    let rendered = render_patches(hlg, &[[code; 3]], "hlg");
    assert_neutral(rendered[0], "HLG reference white");
    assert!(
        REFERENCE_WHITE_BAND.contains(&rendered[0][1]),
        "75% HLG is {REFERENCE_WHITE_NITS} nits and rendered at {:?}, outside \
         {REFERENCE_WHITE_BAND:?}",
        rendered[0]
    );

    let sdr = render_patches(rec709_sdr_picture_colour(), &[[code; 3]], "hlgsdr");
    assert!(
        rendered[0][1] > sdr[0][1],
        "HLG reference white is above 100 nits, so it has to render brighter than the same \
         code read as Rec.709: {:?} against {:?}",
        rendered[0],
        sdr[0]
    );
}

#[test]
fn a_p3d65_sdr_frame_keeps_its_white_and_clips_its_red() {
    let p3d65 = HdrMetadata {
        color_primaries: Some(COLOR_PRIMARIES_P3D65),
        transfer_characteristic: Some(TRANSFER_CHARACTERISTIC_BT709),
        ..Default::default()
    };
    let rendered = render_patches(
        p3d65,
        &[
            [FULL_SCALE_12BIT; 3],
            [FULL_SCALE_12BIT, 0, 0],
            [HALF_SCALE_12BIT, 0, 0],
            [0; 3],
        ],
        "p3d65",
    );
    assert_eq!(
        rendered[0], WHITE_PIXEL,
        "P3-D65 shares D65 with Rec.709, so its white must not shift"
    );
    let [red, green, blue] = rendered[1];
    assert!(red > 0, "P3 red rendered at {:?}", rendered[1]);
    assert_eq!(
        (green, blue),
        (0, 0),
        "P3 red is outside Rec.709, so green and blue clip to zero, found {:?}",
        rendered[1]
    );
    assert_eq!(rendered[3], [0, 0, 0], "P3-D65 black has to stay black");

    // a half-scale P3 red would come out at 128 if the gamut matrix were skipped
    let dim_red = rendered[2][0];
    assert!(
        dim_red > (HALF_SCALE_12BIT >> 4) as u8 + 8,
        "reaching P3 red needs more Rec.709 red than the code alone, found {:?}",
        rendered[2]
    );
}

#[test]
fn the_netflix_p3d65_pq_leader_frame_renders_black() {
    let wrapped = wrap_app2e(
        fixture(IMF_4K_FIXTURE),
        HdrMetadata {
            color_primaries: Some(COLOR_PRIMARIES_P3D65),
            transfer_characteristic: Some(TRANSFER_CHARACTERISTIC_ST2084),
            ..Default::default()
        },
    );
    let opts = DcpPreviewOptions {
        source: wrapped.output.clone(),
        ..Default::default()
    };
    let out = out_path("solleveante");
    preview::render_imf_frame(&opts, 0, &out).expect("a P3-D65 PQ frame has to render");
    let pixels = read_ppm(&out, 3840, 2160);
    std::fs::remove_file(&out).ok();
    assert!(
        pixels.iter().all(|&sample| sample == 0),
        "a black leader frame has to render black, found {:?}",
        pixels.iter().find(|&&sample| sample != 0)
    );
}

#[test]
fn an_unrecognised_colour_ul_is_still_refused_by_name() {
    let unknown = [
        0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x0d, 0x04, 0x01, 0x01, 0x01, 0x01, 0x09, 0x00,
        0x00,
    ];
    let wrapped = wrap_app2e(
        encode_red_imf_frame(),
        HdrMetadata {
            color_primaries: Some(COLOR_PRIMARIES_BT709),
            transfer_characteristic: Some(unknown),
            ..Default::default()
        },
    );
    let opts = DcpPreviewOptions {
        source: wrapped.output.clone(),
        ..Default::default()
    };
    let out = out_path("unknownul");
    let message = preview::render_imf_frame(&opts, 0, &out)
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("unrecognised transfer characteristic"),
        "the refusal has to name what it cannot read: {message}"
    );
    assert!(!out.exists(), "a refused frame must not leave an image");
}

/// A 4:2:2 YCbCr frame: flat narrow-range grey luma and no blue chroma, with the
/// red chroma stepping up halfway across so the two sides read differently.
fn encode_ycbcr422_frame() -> Vec<u8> {
    let chroma_width = WIDTH as usize / 2;
    let luma = vec![NARROW_MID_GREY_12BIT; (WIDTH * HEIGHT) as usize];
    let blue_chroma = vec![NARROW_CHROMA_MIDPOINT; chroma_width * HEIGHT as usize];
    let red_chroma_row: Vec<i32> = (0..chroma_width)
        .map(|column| {
            if column < chroma_width / 2 {
                NARROW_CHROMA_MIDPOINT
            } else {
                RED_CHROMA_HIGH_12BIT
            }
        })
        .collect();
    let red_chroma = red_chroma_row.repeat(HEIGHT as usize);

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
    postkit::grok_encoder::compress_yuv422_frame(
        [&luma, &blue_chroma, &red_chroma],
        WIDTH,
        HEIGHT,
        12,
        &params,
    )
    .expect("a 4:2:2 frame compresses")
}

#[test]
fn a_422_track_file_renders_its_ycbcr_as_rgb() {
    let wrapped = wrap_app2e(encode_ycbcr422_frame(), rec709_sdr_picture_colour());
    let opts = DcpPreviewOptions {
        source: wrapped.output.clone(),
        ..Default::default()
    };
    let out = out_path("ycbcr422");
    preview::render_imf_frame(&opts, 0, &out).expect("a 4:2:2 frame has to render");
    let pixels = read_ppm(&out, WIDTH, HEIGHT);
    std::fs::remove_file(&out).ok();

    let pixel = |column: usize| {
        let offset = ((HEIGHT as usize / 2) * WIDTH as usize + column) * 3;
        [pixels[offset], pixels[offset + 1], pixels[offset + 2]]
    };
    let grey = pixel(WIDTH as usize / 4);
    let tinted = pixel(WIDTH as usize * 3 / 4);

    assert_neutral(grey, "a zero chroma patch");
    assert!(
        grey[0] > 0 && grey[0] < 255,
        "narrow-range mid grey has to land between the ends, found {grey:?}"
    );
    assert!(
        tinted[0] > grey[0] && tinted[0] < 255,
        "the red chroma step has to lift red without clipping: {grey:?} against {tinted:?}"
    );
    assert!(
        tinted[1] < grey[1],
        "lifting red at a fixed luma has to pull green down: {grey:?} against {tinted:?}"
    );
    assert_eq!(
        tinted[2], grey[2],
        "the blue chroma did not step, so blue must not move: {grey:?} against {tinted:?}"
    );
}

fn rec709_rgb8_of_ycbcr(codes: [i32; 3]) -> [u8; 3] {
    let [red_luma, blue_luma] = BT709_LUMA_RED_BLUE;
    let green_luma = 1.0 - red_luma - blue_luma;
    let luma = (f64::from(codes[0]) - NARROW_LUMA_BLACK) / (NARROW_LUMA_WHITE - NARROW_LUMA_BLACK);
    let blue_chroma = f64::from(codes[1] - NARROW_CHROMA_MIDPOINT) / NARROW_CHROMA_HALF_RANGE;
    let red_chroma = f64::from(codes[2] - NARROW_CHROMA_MIDPOINT) / NARROW_CHROMA_HALF_RANGE;
    [
        luma + 2.0 * (1.0 - red_luma) * red_chroma,
        luma - 2.0 * blue_luma * (1.0 - blue_luma) / green_luma * blue_chroma
            - 2.0 * red_luma * (1.0 - red_luma) / green_luma * red_chroma,
        luma + 2.0 * (1.0 - blue_luma) * blue_chroma,
    ]
    .map(|channel| {
        let code = (channel.clamp(0.0, 1.0) * f64::from(FULL_SCALE_12BIT)).round() as i32;
        (code >> 4) as u8
    })
}

fn rgb8_of_codes(codes: [i32; 3]) -> [u8; 3] {
    codes.map(|code| (code >> 4) as u8)
}

fn centre_pixel(pixels: &[u8]) -> [u8; 3] {
    let offset = ((HEIGHT as usize / 2) * WIDTH as usize + WIDTH as usize / 2) * 3;
    [pixels[offset], pixels[offset + 1], pixels[offset + 2]]
}

fn assert_within_tolerance(rendered: [u8; 3], expected: [u8; 3], what: &str) {
    let apart = (0..3)
        .map(|channel| (i32::from(rendered[channel]) - i32::from(expected[channel])).abs())
        .max()
        .unwrap();
    assert!(
        apart <= MATRIX_TOLERANCE,
        "{what} rendered {rendered:?}, and the YCbCr matrix gives {expected:?}"
    );
}

#[test]
fn a_444_cdci_track_file_renders_its_ycbcr_as_rgb() {
    let samples = (WIDTH * HEIGHT) as usize;
    let frame = encode_imf_frame(YCBCR_TRIPLE.map(|code| vec![code; samples]));
    let matrixed = rec709_rgb8_of_ycbcr(YCBCR_TRIPLE);
    let raw = rgb8_of_codes(YCBCR_TRIPLE);
    assert_ne!(
        matrixed, raw,
        "the triple has to matrix to something other than itself or the test proves nothing"
    );

    let cdci = wrap_app2e_cdci(&frame, rec709_sdr_picture_colour());
    let resolved = preview::resolve_picture(&cdci.output).unwrap();
    assert!(resolved.as02, "the CDCI wrap has to read back as AS-02");
    assert!(
        resolved.descriptor_says_ycbcr,
        "the CDCI essence descriptor is the only thing saying these samples are YCbCr"
    );
    assert_eq!(
        (resolved.color_primaries, resolved.transfer_characteristic),
        (
            Some(COLOR_PRIMARIES_BT709),
            Some(TRANSFER_CHARACTERISTIC_BT709)
        ),
        "the colour ULs have to read off a CDCI descriptor as they do off an RGBA one"
    );

    let extracted_path = out_path("cdci444extract");
    assert_eq!(
        preview::extract_frame(&cdci.output, 0, &extracted_path, None),
        0,
        "a 4:4:4 CDCI frame has to extract"
    );
    let extracted = centre_pixel(&read_ppm(&extracted_path, WIDTH, HEIGHT));
    std::fs::remove_file(&extracted_path).ok();
    assert_within_tolerance(extracted, matrixed, "the extracted frame");
    assert_ne!(
        extracted, raw,
        "the chroma planes must not reach the screen as green and blue"
    );

    let previewed_path = out_path("cdci444preview");
    let opts = DcpPreviewOptions {
        source: cdci.output.clone(),
        ..Default::default()
    };
    preview::render_imf_frame(&opts, 0, &previewed_path).expect("a 4:4:4 CDCI frame has to render");
    let previewed = centre_pixel(&read_ppm(&previewed_path, WIDTH, HEIGHT));
    std::fs::remove_file(&previewed_path).ok();
    assert_within_tolerance(previewed, matrixed, "the previewed frame");
}

#[test]
fn the_same_444_planes_wrapped_as_rgba_stay_rgb() {
    let samples = (WIDTH * HEIGHT) as usize;
    let frame = encode_imf_frame(YCBCR_TRIPLE.map(|code| vec![code; samples]));
    let rgba = wrap_app2e(frame, rec709_sdr_picture_colour());
    let resolved = preview::resolve_picture(&rgba.output).unwrap();
    assert!(
        !resolved.descriptor_says_ycbcr,
        "an RGBA essence descriptor carries no chroma subsampling to read"
    );

    let out = out_path("rgba444");
    assert_eq!(
        preview::extract_frame(&rgba.output, 0, &out, None),
        0,
        "a 4:4:4 RGBA frame has to extract"
    );
    let rendered = centre_pixel(&read_ppm(&out, WIDTH, HEIGHT));
    std::fs::remove_file(&out).ok();
    assert_eq!(
        rendered,
        rgb8_of_codes(YCBCR_TRIPLE),
        "an RGBA Rec.709 SDR frame drops its low four bits and reaches the screen unchanged"
    );
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
