use asdcplib::jp2k::{CodestreamHeader, MxfWriter, PictureDescriptor};
use asdcplib::{LabelSet, Rational, WriterInfo};
use postkit::dolby_compliance::check_package;
use std::path::{Path, PathBuf};

const FIXTURE_WIDTH: u32 = 128;
const FIXTURE_HEIGHT: u32 = 72;

fn tmp(tag: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("postkit-dolby-{tag}-{}-{now}", std::process::id()))
}

fn make_real_j2c(width: u32, height: u32) -> Vec<u8> {
    const MID_GREY_12BIT: i32 = 2048;
    let samples = (width * height) as usize;
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
        profile: 0,
        num_resolutions: 3,
        ..postkit::grok_encoder::CompressParams::default()
    };
    postkit::grok_encoder::initialize(0);
    let directory = tempfile::tempdir().unwrap();
    let mut frame = Some(postkit::grok_encoder::RawFrame::Planar {
        components,
        width,
        height,
        precision: 12,
        index: 0,
    });
    let result = postkit::grok_encoder::encode_pipeline(
        directory.path(),
        &params,
        1,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &std::sync::Arc::new(postkit::grok_encoder::PhaseClocks::default()),
        || frame.take(),
        |_| {},
    );
    assert!(result.success, "fixture encode failed: {}", result.error);
    std::fs::read(directory.path().join("frame_00000000.j2c")).expect("fixture codestream")
}

// an AS-DCP wrap, which is what makes this a DCP picture track rather than an
// App 2E one
fn write_dcp_picture_mxf(path: &Path, frame: &[u8]) {
    let info = WriterInfo {
        asset_uuid: [8; 16],
        label_set: LabelSet::Smpte,
        ..Default::default()
    };
    let descriptor = PictureDescriptor {
        edit_rate: Rational::new(24, 1),
        sample_rate: Rational::new(24, 1),
        stored_width: FIXTURE_WIDTH,
        stored_height: FIXTURE_HEIGHT,
        aspect_ratio: Rational::new(1998, 1080),
        container_duration: 1,
        codestream: CodestreamHeader::parse(frame).expect("fixture is a codestream"),
    };
    let mut writer = MxfWriter::new();
    writer
        .open_write(&path.to_string_lossy(), &info, &descriptor, 16_384)
        .unwrap();
    writer.write_frame(frame, None, None).unwrap();
    writer.finalize().unwrap();
}

/// A DCP picture track is X'Y'Z' cinema essence and table 1 makes every Dolby
/// Vision base layer HEVC or AVC, so grading one against the tables would pass
/// a package the check does not apply to at all.
#[test]
fn a_dcp_picture_is_refused_rather_than_graded_as_a_base_layer() {
    let directory = tmp("dcp");
    std::fs::create_dir_all(&directory).unwrap();
    let mxf = directory.join("picture.mxf");
    write_dcp_picture_mxf(&mxf, &make_real_j2c(FIXTURE_WIDTH, FIXTURE_HEIGHT));

    let findings = check_package(&mxf);

    assert!(
        findings.errors.iter().any(|e| e.contains("DCP")),
        "a DCP has to be named as one, got {findings:?}"
    );
    assert!(
        findings.checked.is_empty(),
        "nothing about a DCP should be graded, got {:?}",
        findings.checked
    );

    std::fs::remove_dir_all(&directory).ok();
}
