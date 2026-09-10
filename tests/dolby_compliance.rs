use asdcplib::jp2k::{
    COLOR_PRIMARIES_BT2020, CodestreamHeader, HdrMetadata, MxfWriter, PictureDescriptor,
    TRANSFER_CHARACTERISTIC_ST2084,
};
use asdcplib::{LabelSet, Rational, WriterInfo};
use postkit::dolby_compliance::check_package;
use postkit::mxf_wrap::{EssenceType, MxfStandard, MxfWrapOptions, mxf_wrap};
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

const IMF_FIXTURE: &str = "imf4k_black_3840x2160.j2c";
const IMF_FIXTURE_SIZE: (u32, u32) = (3840, 2160);

const FIXTURE_MAX_CLL: u32 = 993;
const FIXTURE_MAX_FALL: u32 = 362;

fn write_pq_bt2020_track_file(mxf: &Path) {
    let source = tempfile::tempdir().unwrap();
    let frame = source.path().join("frame_00000000.j2c");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(IMF_FIXTURE),
        &frame,
    )
    .expect("fixture codestream");

    let result = mxf_wrap(&MxfWrapOptions {
        input_files: vec![frame],
        output: mxf.to_path_buf(),
        essence_type: EssenceType::J2k,
        standard: MxfStandard::As02,
        fps_num: 24,
        fps_den: 1,
        partition_size: 1,
        encryption: None,
        mca_config: None,
        resource_ids: vec![],
        hdr: Some(HdrMetadata {
            color_primaries: Some(COLOR_PRIMARIES_BT2020),
            transfer_characteristic: Some(TRANSFER_CHARACTERISTIC_ST2084),
            ..Default::default()
        }),
        asset_uuid: None,
        timed_text_duration_frames: None,
    });
    assert!(result.success, "wrap failed: {}", result.error);
}

const APP2E_2020_NAMESPACE: &str = "http://www.smpte-ra.org/ns/2067-21/2020";

fn write_cpl(directory: &Path, light_levels: Option<(u32, u32)>) {
    let extension_properties = match light_levels {
        Some((max_content, max_frame_average)) => format!(
            "  <app2e:MaxCLL xmlns:app2e=\"{APP2E_2020_NAMESPACE}\">{max_content}\
             </app2e:MaxCLL>\n  \
             <app2e:MaxFALL xmlns:app2e=\"{APP2E_2020_NAMESPACE}\">{max_frame_average}\
             </app2e:MaxFALL>\n"
        ),
        None => String::new(),
    };
    let cpl = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <CompositionPlaylist>\n{extension_properties}</CompositionPlaylist>\n"
    );
    std::fs::write(directory.join("CPL.xml"), cpl).unwrap();
}

#[test]
fn a_pq_bt2020_package_is_graded_against_table_1_and_its_cpl_light_levels() {
    let directory = tmp("pq");
    std::fs::create_dir_all(&directory).unwrap();
    write_pq_bt2020_track_file(&directory.join("picture.mxf"));
    write_cpl(&directory, Some((FIXTURE_MAX_CLL, FIXTURE_MAX_FALL)));

    let findings = check_package(&directory);

    assert!(findings.errors.is_empty(), "{findings:?}");
    assert!(
        findings
            .checked
            .iter()
            .any(|line| line.contains("16,9,9,0")),
        "the base layer VUI has to be named, got {:?}",
        findings.checked
    );
    assert!(
        findings
            .checked
            .iter()
            .any(|line| line.contains(&format!("MaxCLL {FIXTURE_MAX_CLL}"))),
        "the CPL light levels have to be read, got {:?}",
        findings.checked
    );
    assert!(
        findings
            .checked
            .iter()
            .any(|line| line.contains(&format!("{}x{}", IMF_FIXTURE_SIZE.0, IMF_FIXTURE_SIZE.1))),
        "the raster has to be graded, got {:?}",
        findings.checked
    );

    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn a_pq_bt2020_package_without_cpl_light_levels_warns_that_it_is_pq10() {
    let directory = tmp("pq10");
    std::fs::create_dir_all(&directory).unwrap();
    write_pq_bt2020_track_file(&directory.join("picture.mxf"));
    write_cpl(&directory, None);

    let findings = check_package(&directory);

    assert!(findings.errors.is_empty(), "{findings:?}");
    assert_eq!(findings.warnings.len(), 1, "{findings:?}");
    assert!(findings.warnings[0].contains("PQ10"), "{findings:?}");

    std::fs::remove_dir_all(&directory).ok();
}
