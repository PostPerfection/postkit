//! DCP-native preview: encrypted read fails loud without a key, and the full
//! decrypt → decode → colour-manage pipeline produces a real frame.
//!
//! The fixture codestream is encoded in process by the same grok the decode
//! reads it back with, so the end-to-end test needs no external tool.

use asdcplib::crypto::{AesEncContext, HmacContext};
use asdcplib::jp2k::{MxfWriter, PictureDescriptor};
use asdcplib::{LabelSet, Rational, WriterInfo};
use postkit::preview::{self, DcpPreviewOptions};
use std::path::{Path, PathBuf};

fn tmp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("postkit-preview-{tag}-{}-{n}", std::process::id()))
}

fn descriptor(frames: u32, w: u32, h: u32) -> PictureDescriptor {
    PictureDescriptor {
        edit_rate: Rational::new(24, 1),
        sample_rate: Rational::new(24, 1),
        stored_width: w,
        stored_height: h,
        aspect_ratio: Rational::new(1998, 1080),
        container_duration: frames,
        component_count: 3,
    }
}

/// Write an encrypted JPEG 2000 MXF wrapping the given codestreams.
fn write_encrypted_mxf(path: &Path, frames: &[Vec<u8>], key: [u8; 16], w: u32, h: u32) {
    write_mxf(path, frames, Some(key), w, h)
}

/// Write a JPEG 2000 MXF, encrypted when a key is given.
fn write_mxf(path: &Path, frames: &[Vec<u8>], key: Option<[u8; 16]>, w: u32, h: u32) {
    let info = WriterInfo {
        asset_uuid: [8; 16],
        context_id: [0xc7; 16],
        cryptographic_key_id: [0xd4; 16],
        encrypted_essence: key.is_some(),
        uses_hmac: key.is_some(),
        label_set: LabelSet::Smpte,
        ..Default::default()
    };
    let mut writer = MxfWriter::new();
    writer
        .open_write(
            &path.to_string_lossy(),
            &info,
            &descriptor(frames.len() as u32, w, h),
            16_384,
        )
        .unwrap();
    let mut crypto = key.map(|key| {
        let mut enc = AesEncContext::new();
        enc.init_key(&key).unwrap();
        enc.set_ivec(&[0x9c; 16]).unwrap();
        let mut hmac = HmacContext::new();
        hmac.init_key(&key, LabelSet::Smpte).unwrap();
        (enc, hmac)
    });
    for f in frames {
        match crypto.as_mut() {
            Some((enc, hmac)) => writer.write_frame(f, Some(enc), Some(hmac)).unwrap(),
            None => writer.write_frame(f, None, None).unwrap(),
        }
    }
    writer.finalize().unwrap();
}

/// Fake but structurally valid J2K codestream (never decoded).
fn synthetic_j2c(seed: u8) -> Vec<u8> {
    let mut d = vec![0xff, 0x4f, 0xff, 0x51];
    d.extend((0..64).map(|i| seed.wrapping_add(i as u8)));
    d.extend([0xff, 0x93, 0xff, 0xd9]);
    d
}

#[test]
fn encrypted_essence_without_key_fails_loud() {
    let mxf = tmp("nodec").with_extension("mxf");
    let frames = vec![synthetic_j2c(1), synthetic_j2c(2)];
    write_encrypted_mxf(&mxf, &frames, [0x2b; 16], 2048, 1080);

    let resolved = preview::resolve_picture(&mxf).unwrap();
    assert!(resolved.encrypted, "essence must report as encrypted");

    let opts = DcpPreviewOptions {
        source: mxf.clone(),
        key: None,
        ..Default::default()
    };
    let out = tmp("nodec").with_extension("png");
    let err = preview::render_dcp_frame(&opts, 0, &out).unwrap_err();
    assert!(
        matches!(err, preview::PreviewError::MissingKey(_)),
        "expected MissingKey, got {err:?}"
    );
    assert!(!out.exists(), "no image should be written without a key");
    std::fs::remove_file(&mxf).ok();
}

/// A real raw J2K codestream, encoded in process by grok so the test needs no
/// external tool. Mid-grey so a colour-managed frame is plainly not black.
fn make_real_j2c(w: u32, h: u32, profile: u16) -> Vec<u8> {
    const MID_GREY_12BIT: i32 = 2048;
    let samples = (w * h) as usize;
    let components = [
        vec![MID_GREY_12BIT; samples],
        vec![MID_GREY_12BIT; samples],
        vec![MID_GREY_12BIT; samples],
    ];
    let params = postkit::grok_encoder::CompressParams {
        irreversible: false,
        compression_ratio: 1.0,
        mct: false,
        apply_xyz_transform: false,
        profile,
        num_resolutions: 3,
        ..postkit::grok_encoder::CompressParams::default()
    };
    postkit::grok_encoder::initialize(0);
    let dir = tempfile::tempdir().unwrap();
    let mut frame = Some(postkit::grok_encoder::RawFrame::Planar {
        components,
        width: w,
        height: h,
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
    assert!(result.success, "fixture encode failed: {}", result.error);
    std::fs::read(dir.path().join("frame_00000000.j2c")).expect("fixture codestream")
}

#[test]
fn encrypted_frame_decodes_and_colour_manages_with_key() {
    let (w, h) = (128u32, 72u32);
    let j2c = make_real_j2c(w, h, 0);
    let key = [0x2b; 16];
    let mxf = tmp("dec").with_extension("mxf");
    write_encrypted_mxf(&mxf, &[j2c.clone(), j2c], key, w, h);

    let opts = DcpPreviewOptions {
        source: mxf.clone(),
        key: Some(key),
        ..Default::default()
    };
    // ppm so the pixels can be read back without a decoder
    let out = tmp("dec").with_extension("ppm");
    preview::render_dcp_frame(&opts, 0, &out).expect("frame should decode with the key");

    let pixels = read_ppm(&out, w, h);
    let first = &pixels[..3];
    assert!(
        pixels.chunks(3).all(|px| px == first),
        "a flat field has to stay flat: found {:?} beside {first:?}",
        pixels.chunks(3).find(|px| *px != first)
    );
    assert!(
        first.iter().all(|&channel| channel > 8 && channel < 248),
        "mid grey came out at {first:?}, so the decode or the colour path is wrong"
    );

    std::fs::remove_file(&mxf).ok();
    std::fs::remove_file(&out).ok();
}

/// The RGB bytes of a binary PPM, checking it is the size that was asked for.
fn read_ppm(path: &Path, w: u32, h: u32) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("ppm written");
    assert!(bytes.starts_with(b"P6"), "not a binary ppm");
    // P6, width, height, maxval, then the samples, each field whitespace separated
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
            0 => assert_eq!(field, w, "ppm width"),
            1 => assert_eq!(field, h, "ppm height"),
            _ => assert_eq!(field, 255, "8 bits per channel"),
        }
        fields += 1;
    }
    let pixels = bytes[index + 1..].to_vec();
    assert_eq!(
        pixels.len(),
        (w * h * 3) as usize,
        "the ppm holds the wrong number of samples"
    );
    pixels
}

/// The DCI Cinema 2K profile, which routes an input to the grok decoder.
const CINEMA_2K_PROFILE: u16 = 0x0003;
/// The mid-grey the fixtures encode, at 12 bits.
const MID_GREY_12BIT: u16 = 2048;

#[test]
fn extract_frame_decodes_dcp_essence_through_grok() {
    let (w, h) = (128u32, 128u32);
    let j2c = make_real_j2c(w, h, CINEMA_2K_PROFILE);
    let header = postkit::j2k::parse_j2k_header(&j2c).expect("fixture is a codestream");
    assert!(
        postkit::j2k::J2kProfile::from(header.profile).is_dci_cinema(),
        "the fixture has to carry a cinema profile or extraction will not route to grok, \
         found rsiz {:#06x}",
        header.profile
    );

    let mxf = tmp("extract").with_extension("mxf");
    write_mxf(&mxf, &[j2c.clone(), j2c], None, w, h);

    // ppm so the pixels read back without a decoder
    let out = tmp("extract").with_extension("ppm");
    assert_eq!(
        preview::extract_frame(&mxf, 1, &out),
        0,
        "extraction should succeed"
    );

    let pixels = read_ppm(&out, w, h);
    // grok decodes the flat field losslessly, so every pixel is the colour
    // postkit's own X'Y'Z' transform gives that code. ffmpeg's conversion lands
    // elsewhere, so this pins the output to the grok path.
    let expected =
        postkit::colour::XyzToSrgb::new().pixel(MID_GREY_12BIT, MID_GREY_12BIT, MID_GREY_12BIT);
    assert!(
        pixels.chunks(3).all(|px| px == expected),
        "expected every pixel to be {expected:?}, found {:?}",
        pixels.chunks(3).find(|px| *px != expected)
    );

    std::fs::remove_file(&mxf).ok();
    std::fs::remove_file(&out).ok();
}

#[test]
fn extract_frame_refuses_encrypted_essence() {
    let (w, h) = (128u32, 128u32);
    let j2c = make_real_j2c(w, h, CINEMA_2K_PROFILE);
    let mxf = tmp("extractenc").with_extension("mxf");
    write_mxf(&mxf, &[j2c.clone(), j2c], Some([0x2b; 16]), w, h);

    // ffmpeg cannot decrypt this and renders the ciphertext as a picture, so
    // extraction has to refuse rather than write one
    let out = tmp("extractenc").with_extension("ppm");
    assert_ne!(
        preview::extract_frame(&mxf, 0, &out),
        0,
        "encrypted essence must not extract without a key"
    );
    assert!(!out.exists(), "no image should be written");

    std::fs::remove_file(&mxf).ok();
}
