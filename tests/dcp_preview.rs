//! DCP-native preview: encrypted read fails loud without a key, and the full
//! decrypt → decode → colour-manage pipeline produces a real frame.
//!
//! The end-to-end decode test is `#[ignore]` because it shells out to ffmpeg
//! (encode a J2K fixture, decode it back); run with `cargo test -- --ignored`.

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
    let info = WriterInfo {
        asset_uuid: [8; 16],
        context_id: [0xc7; 16],
        cryptographic_key_id: [0xd4; 16],
        encrypted_essence: true,
        uses_hmac: true,
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
    let mut enc = AesEncContext::new();
    enc.init_key(&key).unwrap();
    enc.set_ivec(&[0x9c; 16]).unwrap();
    let mut hmac = HmacContext::new();
    hmac.init_key(&key, LabelSet::Smpte).unwrap();
    for f in frames {
        writer
            .write_frame(f, Some(&mut enc), Some(&mut hmac))
            .unwrap();
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

/// A real raw J2K codestream (as DCP essence stores it), or None if the tools
/// are unavailable. ffmpeg's jpeg2000 muxer only writes JP2 boxes, so we make
/// the raw codestream with opj_compress.
fn make_real_j2c(w: u32, h: u32) -> Option<Vec<u8>> {
    let tif = tmp("fixture").with_extension("tif");
    let j2c = tmp("fixture").with_extension("j2c");
    let made_tif = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={w}x{h}"),
            "-frames:v",
            "1",
            "-pix_fmt",
            "rgb48le",
        ])
        .arg(&tif)
        .status()
        .ok()?
        .success();
    if !made_tif {
        return None;
    }
    let made = std::process::Command::new("opj_compress")
        .arg("-i")
        .arg(&tif)
        .arg("-o")
        .arg(&j2c)
        .status()
        .ok()
        .is_some_and(|s| s.success());
    std::fs::remove_file(&tif).ok();
    if !made {
        return None;
    }
    let bytes = std::fs::read(&j2c).ok();
    std::fs::remove_file(&j2c).ok();
    bytes.filter(|b| !b.is_empty())
}

#[test]
#[ignore = "shells out to ffmpeg for a real J2K fixture"]
fn encrypted_frame_decodes_and_colour_manages_with_key() {
    let (w, h) = (128u32, 72u32);
    let Some(j2c) = make_real_j2c(w, h) else {
        eprintln!("ffmpeg unavailable, skipping");
        return;
    };
    let key = [0x2b; 16];
    let mxf = tmp("dec").with_extension("mxf");
    write_encrypted_mxf(&mxf, &[j2c.clone(), j2c], key, w, h);

    let opts = DcpPreviewOptions {
        source: mxf.clone(),
        key: Some(key),
        ..Default::default()
    };
    let out = tmp("dec").with_extension("png");
    preview::render_dcp_frame(&opts, 0, &out).expect("frame should decode with the key");
    let meta = std::fs::metadata(&out).expect("png written");
    assert!(meta.len() > 100, "png looks empty: {} bytes", meta.len());

    std::fs::remove_file(&mxf).ok();
    std::fs::remove_file(&out).ok();
}
