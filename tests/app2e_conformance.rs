//! What an IMF App 2E picture track file has to say about the essence it
//! carries: an IMF profile, RGB samples at 12 bits, and the colour signalled on
//! the RGBA essence descriptor.
//!
//! Every assertion reads the written MXF back through asdcplib and decodes the
//! frame it returns, so an encoder agreeing with itself cannot pass.

use asdcplib::jp2k::{
    COLOR_PRIMARIES_BT709, COLOR_PRIMARIES_P3D65, HdrMetadata, PICTURE_ESSENCE_CODING_CINEMA_2K,
    PICTURE_ESSENCE_CODING_IMF_2K_LOSSY, PICTURE_ESSENCE_CODING_IMF_4K_LOSSY_6_3,
    TRANSFER_CHARACTERISTIC_BT709, TRANSFER_CHARACTERISTIC_ST2084,
};
use postkit::j2k::{ImfProfile, J2kProfile, imf_levels, imf_rsiz};
use postkit::mxf_wrap::{
    EssenceType, MxfStandard, MxfTrackFile, MxfWrapOptions, mxf_wrap, rec709_sdr_picture_colour,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 128;
const HEIGHT: u32 = 72;
const FRAME_RATE: f64 = 24.0;
const BITS_PER_SECOND: u64 = 200_000_000;
/// The largest 12-bit sample, what pure red decodes back to.
const FULL_SCALE_12BIT: i32 = 4095;
/// Red has to come back close to full scale and green and blue close to zero,
/// with room for what a lossy encode moves.
const RED_FLOOR: i32 = 3900;
const GREEN_AND_BLUE_CEILING: i32 = 200;

const IMF_4K_FIXTURE: &str = "imf4k_black_3840x2160.j2c";
const CINEMA_2K_FIXTURE: &str = "cinema2k_64x64.j2c";
/// The Rsiz values the two fixtures declare, per tests/fixtures/README.md.
const IMF_4K_FIXTURE_RSIZ: u16 = 0x0536;
const CINEMA_2K_FIXTURE_RSIZ: u16 = 0x0003;

/// The 12-bit RGB pixel layout of SMPTE 377's RGBAValue_RGB_12, zero-terminated.
const PIXEL_LAYOUT_RGB_12: [u8; 16] = [b'R', 12, b'G', 12, b'B', 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// A UL names the profile family in its first 15 bytes and the level pair in the
/// last, so this prefix is what every IMF 2K lossy label shares.
const PROFILE_FAMILY_BYTES: usize = 15;

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
    result: MxfTrackFile,
}

impl Wrapped {
    fn reader(&self) -> asdcplib::as02::jp2k::MxfReader {
        assert!(self.result.success, "wrap failed: {}", self.result.error);
        let mut reader = asdcplib::as02::jp2k::MxfReader::new();
        reader.open_read(&self.output.to_string_lossy()).unwrap();
        reader
    }
}

fn wrap_frames(frames: &[Vec<u8>], standard: MxfStandard, hdr: Option<HdrMetadata>) -> Wrapped {
    let dir = tempfile::tempdir().unwrap();
    let input_files: Vec<PathBuf> = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let path = dir.path().join(format!("frame_{index:08}.j2c"));
            std::fs::write(&path, frame).unwrap();
            path
        })
        .collect();
    let output = dir.path().join("picture.mxf");
    let result = mxf_wrap(&MxfWrapOptions {
        input_files,
        output: output.clone(),
        essence_type: EssenceType::J2k,
        standard,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr,
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    Wrapped {
        _dir: dir,
        output,
        result,
    }
}

/// One flat rgb48be frame of pure red, the colour an X'Y'Z' transform would
/// spread over all three components.
fn red_frame() -> postkit::grok_encoder::RawFrame {
    let mut data = Vec::with_capacity((WIDTH * HEIGHT * 6) as usize);
    for _ in 0..(WIDTH * HEIGHT) {
        data.extend_from_slice(&[0xff, 0xff, 0x00, 0x00, 0x00, 0x00]);
    }
    postkit::grok_encoder::RawFrame::Packed {
        data,
        order: postkit::grok_encoder::SampleOrder::Big,
        width: WIDTH,
        height: HEIGHT,
        precision: 16,
        index: 0,
    }
}

/// The red frame compressed in process under the IMF Rsiz for this raster and
/// rate, so the test needs no external encoder.
fn encode_red_imf_frame() -> Vec<u8> {
    let profile = ImfProfile::for_raster(WIDTH, HEIGHT).unwrap();
    let levels = imf_levels(WIDTH, HEIGHT, FRAME_RATE, BITS_PER_SECOND).unwrap();
    let params = postkit::grok_encoder::CompressParams {
        compression_ratio: 1.0,
        num_resolutions: 3,
        profile: imf_rsiz(profile, levels),
        apply_xyz_transform: false,
        mct: true,
        ..postkit::grok_encoder::CompressParams::default()
    };
    postkit::grok_encoder::initialize(0);
    let dir = tempfile::tempdir().unwrap();
    let mut frame = Some(red_frame());
    let result = postkit::grok_encoder::encode_pipeline(
        dir.path(),
        &params,
        1,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(postkit::grok_encoder::PhaseClocks::default()),
        || frame.take(),
        |_| {},
    );
    assert!(result.success, "IMF encode failed: {}", result.error);
    std::fs::read(dir.path().join("frame_00000000.j2c")).expect("encoded codestream")
}

/// The centre pixel of a decoded frame, as (red, green, blue).
fn centre_pixel(decoded: &postkit::grok_decoder::DecodedFrame) -> (i32, i32, i32) {
    let centre =
        (decoded.height as usize / 2) * decoded.width as usize + decoded.width as usize / 2;
    (
        decoded.components[0][centre],
        decoded.components[1][centre],
        decoded.components[2][centre],
    )
}

#[test]
fn an_app2e_track_file_carries_rgb_imf_picture() {
    let wrapped = wrap_frames(
        &[encode_red_imf_frame()],
        MxfStandard::As02,
        Some(rec709_sdr_picture_colour()),
    );
    let mut reader = wrapped.reader();

    let rgba = reader.rgba_descriptor().unwrap();
    assert_eq!(
        rgba.pixel_layout, PIXEL_LAYOUT_RGB_12,
        "the RGBA essence descriptor has to describe 12-bit RGB"
    );
    assert_eq!(rgba.component_max_ref, Some(FULL_SCALE_12BIT as u32));
    assert_eq!(rgba.component_min_ref, Some(0));
    let coding = rgba
        .picture_essence_coding
        .expect("the descriptor has to carry a PictureEssenceCoding");
    assert_eq!(
        &coding[..PROFILE_FAMILY_BYTES],
        &PICTURE_ESSENCE_CODING_IMF_2K_LOSSY[..PROFILE_FAMILY_BYTES],
        "the coding label is {coding:02x?}, not an IMF 2K lossy one; a DCP encode of this \
         frame would have landed as the cinema 2K label {PICTURE_ESSENCE_CODING_CINEMA_2K:02x?}"
    );

    let hdr = reader.hdr_metadata().unwrap();
    assert_eq!(hdr.color_primaries, Some(COLOR_PRIMARIES_BT709));
    assert_eq!(
        hdr.transfer_characteristic,
        Some(TRANSFER_CHARACTERISTIC_BT709)
    );

    let codestream = reader.picture_descriptor().unwrap().codestream;
    let profile = J2kProfile::from(codestream.rsize);
    assert_eq!(
        profile,
        J2kProfile::Imf,
        "rsiz {:#06x} in the sub-descriptor is not an IMF profile",
        codestream.rsize
    );
    assert!(
        !profile.is_dci_cinema(),
        "rsiz {:#06x} is a cinema profile, so the samples would be X'Y'Z'",
        codestream.rsize
    );
    assert_eq!(codestream.components.len(), 3);
    for component in &codestream.components {
        assert_eq!(component.bit_depth(), 12);
        assert_eq!(component.x_rsize, 1);
        assert_eq!(component.y_rsize, 1);
    }

    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let size = reader.read_frame(0, &mut buf, None, None).unwrap();
    buf.truncate(size);
    let decoded = postkit::grok_decoder::decode(buf, 0).expect("the wrapped frame has to decode");
    let (red, green, blue) = centre_pixel(&decoded);
    assert!(
        red > RED_FLOOR && green < GREEN_AND_BLUE_CEILING && blue < GREEN_AND_BLUE_CEILING,
        "the centre pixel read back as {red},{green},{blue}: pure red in RGB is near \
         {FULL_SCALE_12BIT},0,0, while an X'Y'Z' encode of the same frame leaves all three \
         components large"
    );

    reader.close().unwrap();
}

#[test]
fn a_dcp_codestream_cannot_be_wrapped_as_imf() {
    let wrapped = wrap_frames(
        &[fixture(CINEMA_2K_FIXTURE)],
        MxfStandard::As02,
        Some(rec709_sdr_picture_colour()),
    );
    assert!(
        !wrapped.result.success,
        "a DCP codestream wrapped into an IMF track file"
    );
    assert!(
        wrapped
            .result
            .error
            .contains(&format!("{CINEMA_2K_FIXTURE_RSIZ:#06x}")),
        "the refusal has to name the Rsiz: {}",
        wrapped.result.error
    );
    assert!(
        !wrapped.output.exists(),
        "a refused wrap must not leave a file behind"
    );

    let wrapped = wrap_frames(&[fixture(IMF_4K_FIXTURE)], MxfStandard::AsDcp, None);
    assert!(
        !wrapped.result.success,
        "an IMF codestream wrapped into a DCP track file"
    );
    assert!(
        wrapped
            .result
            .error
            .contains(&format!("{IMF_4K_FIXTURE_RSIZ:#06x}")),
        "the refusal has to name the Rsiz: {}",
        wrapped.result.error
    );
}

#[test]
fn an_imf_wrap_without_colour_is_refused() {
    for hdr in [None, Some(HdrMetadata::default())] {
        let wrapped = wrap_frames(&[fixture(IMF_4K_FIXTURE)], MxfStandard::As02, hdr);
        assert!(
            !wrapped.result.success,
            "an App 2E picture wrapped with no colour on its descriptor"
        );
        assert!(
            wrapped.result.error.contains("ColorPrimaries")
                && wrapped.result.error.contains("TransferCharacteristic"),
            "the refusal has to name what is missing: {}",
            wrapped.result.error
        );
        assert!(
            !wrapped.output.exists(),
            "a refused wrap must not leave a file behind"
        );
    }
}

#[test]
fn the_real_netflix_codestream_decodes_as_12_bit_rgb() {
    let bytes = fixture(IMF_4K_FIXTURE);
    let header = postkit::j2k::parse_j2k_header(&bytes).expect("codestream header");
    assert_eq!(header.profile, IMF_4K_FIXTURE_RSIZ);
    assert_eq!(J2kProfile::from(header.profile), J2kProfile::Imf);
    assert_eq!(header.num_components, 3);
    assert_eq!(header.bit_depths, vec![12, 12, 12]);

    // reduce 2, a quarter-size decode is enough to prove the samples are there
    let decoded = postkit::grok_decoder::decode(bytes.clone(), 2).expect("quarter-size decode");
    assert_eq!(decoded.precision, 12);
    assert_eq!(decoded.components.len(), 3);

    let wrapped = wrap_frames(
        &[bytes],
        MxfStandard::As02,
        Some(HdrMetadata {
            color_primaries: Some(COLOR_PRIMARIES_P3D65),
            transfer_characteristic: Some(TRANSFER_CHARACTERISTIC_ST2084),
            ..Default::default()
        }),
    );
    let mut reader = wrapped.reader();
    assert_eq!(
        reader.rgba_descriptor().unwrap().picture_essence_coding,
        Some(PICTURE_ESSENCE_CODING_IMF_4K_LOSSY_6_3),
        "rsiz {IMF_4K_FIXTURE_RSIZ:#06x} has to land as the label the Netflix CPL carries"
    );
    reader.close().unwrap();
}
