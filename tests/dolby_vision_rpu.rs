use dolby_vision::rpu::extension_metadata::blocks::{
    ExtMetadataBlock, ExtMetadataBlockLevel1, ExtMetadataBlockLevel6,
};
use dolby_vision::rpu::generate::{GenerateConfig, GenerateProfile, VideoShot};
use dolby_vision::rpu::vdr_dm_data::CmVersion;
use postkit::dolby_vision::{
    DolbyVisionSummary, read_dolby_vision, refuse_undecodable_dolby_vision,
};
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE_FRAMES: usize = 6;
const NAL_START_CODE: &[u8] = &[0, 0, 0, 1];
const NAL_START_CODE_PREFIX: &[u8] = &[0, 0, 1];
const LAST_VIDEO_CODING_NAL_TYPE: u8 = 21;

const MAX_CONTENT_LIGHT_LEVEL: u16 = 1000;
const MAX_FRAME_AVERAGE_LIGHT_LEVEL: u16 = 400;
const MASTERING_DISPLAY_MAX_NITS: u16 = 1000;
const MASTERING_DISPLAY_MIN_STEPS: u16 = 1;
// 12 bit PQ code for 600 cd/m²
const PQ_CODE_600_NITS: u16 = 2851;
const LEVEL_1_AVG_PQ: u16 = 1229;

fn run(program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn encode_plain_hevc(directory: &Path) -> PathBuf {
    let output = directory.join("plain.hevc");
    run(
        "ffmpeg",
        &[
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=gray:s=320x180:r=25",
            "-frames:v",
            "6",
            "-pix_fmt",
            "yuv420p10le",
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=none",
            "-f",
            "hevc",
            output.to_str().unwrap(),
        ],
    );
    output
}

fn level6_block() -> ExtMetadataBlockLevel6 {
    ExtMetadataBlockLevel6 {
        max_display_mastering_luminance: MASTERING_DISPLAY_MAX_NITS,
        min_display_mastering_luminance: MASTERING_DISPLAY_MIN_STEPS,
        max_content_light_level: MAX_CONTENT_LIGHT_LEVEL,
        max_frame_average_light_level: MAX_FRAME_AVERAGE_LIGHT_LEVEL,
    }
}

fn generate_rpu_nalus(
    level6: Option<ExtMetadataBlockLevel6>,
    level1_max_pq: Option<u16>,
) -> Vec<Vec<u8>> {
    let metadata_blocks = level1_max_pq
        .map(|max_pq| {
            vec![ExtMetadataBlock::Level1(ExtMetadataBlockLevel1::new(
                0,
                max_pq,
                LEVEL_1_AVG_PQ,
            ))]
        })
        .unwrap_or_default();

    let mut config = GenerateConfig {
        cm_version: CmVersion::V40,
        profile: GenerateProfile::Profile81,
        length: FIXTURE_FRAMES,
        level6,
        shots: vec![VideoShot {
            start: 0,
            duration: FIXTURE_FRAMES,
            metadata_blocks,
            ..Default::default()
        }],
        ..Default::default()
    };
    config.fixup_l1();

    config
        .generate_rpu_list()
        .expect("the generate config is valid")
        .iter()
        .map(|rpu| rpu.write_hevc_unspec62_nalu().expect("the rpu serialises"))
        .collect()
}

fn nal_start_offsets(stream: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut index = 0;
    while index + NAL_START_CODE_PREFIX.len() <= stream.len() {
        if &stream[index..index + NAL_START_CODE_PREFIX.len()] == NAL_START_CODE_PREFIX {
            offsets.push(index);
            index += NAL_START_CODE_PREFIX.len();
        } else {
            index += 1;
        }
    }
    offsets
}

// the rpu is suffixed to the slices of its access unit, so it lands after each slice nal
fn insert_rpus_after_slices(stream: &[u8], rpus: &[Vec<u8>]) -> Vec<u8> {
    let offsets = nal_start_offsets(stream);
    let mut out = Vec::with_capacity(stream.len());
    let mut next_rpu = 0;

    for (index, offset) in offsets.iter().enumerate() {
        let end = offsets.get(index + 1).copied().unwrap_or(stream.len());
        out.extend_from_slice(&stream[*offset..end]);

        let nal_type = stream[offset + NAL_START_CODE_PREFIX.len()] >> 1;
        if nal_type <= LAST_VIDEO_CODING_NAL_TYPE {
            let rpu = rpus.get(next_rpu).expect("one rpu per coded picture");
            out.extend_from_slice(NAL_START_CODE);
            out.extend_from_slice(rpu);
            next_rpu += 1;
        }
    }

    assert_eq!(next_rpu, rpus.len(), "every rpu was inserted");
    out
}

fn write_stream_with_rpus(
    directory: &Path,
    name: &str,
    level6: Option<ExtMetadataBlockLevel6>,
    level1_max_pq: Option<u16>,
) -> PathBuf {
    let plain = encode_plain_hevc(directory);
    let stream = std::fs::read(&plain).unwrap();
    let rpus = generate_rpu_nalus(level6, level1_max_pq);
    let output = directory.join(name);
    std::fs::write(&output, insert_rpus_after_slices(&stream, &rpus)).unwrap();
    output
}

#[test]
fn an_annex_b_stream_with_rpus_reports_profile_8_and_level_6() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_stream_with_rpus(directory.path(), "dv.hevc", Some(level6_block()), None);

    let summary = read_dolby_vision(&path).unwrap().expect("rpus were found");

    assert_eq!(summary.profile, 8);
    assert_eq!(summary.frames, FIXTURE_FRAMES);
    assert_eq!(summary.shots, 1);
    assert_eq!(summary.max_content_light_level_nits, Some(1000.0));
    assert_eq!(summary.max_frame_average_light_level_nits, Some(400.0));
    assert_eq!(summary.peak_luminance_nits, 1000.0);
    assert_eq!(summary.mastering_display_max_nits, Some(1000.0));
    assert_eq!(summary.mastering_display_min_nits, Some(0.0001));
}

#[test]
fn an_mp4_wrapped_stream_reads_the_same() {
    let directory = tempfile::tempdir().unwrap();
    let annex_b = write_stream_with_rpus(directory.path(), "dv.hevc", Some(level6_block()), None);
    let mp4 = directory.path().join("dv.mp4");
    run(
        "ffmpeg",
        &[
            "-y",
            "-v",
            "error",
            "-i",
            annex_b.to_str().unwrap(),
            "-c",
            "copy",
            mp4.to_str().unwrap(),
        ],
    );

    let summary = read_dolby_vision(&mp4).unwrap().expect("rpus were found");

    assert_eq!(summary.profile, 8);
    assert_eq!(summary.frames, FIXTURE_FRAMES);
    assert_eq!(summary.max_content_light_level_nits, Some(1000.0));
    assert_eq!(summary.max_frame_average_light_level_nits, Some(400.0));
    assert_eq!(summary.peak_luminance_nits, 1000.0);
}

#[test]
fn level_1_peak_is_used_when_level_6_is_absent() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_stream_with_rpus(directory.path(), "dv.hevc", None, Some(PQ_CODE_600_NITS));

    let summary = read_dolby_vision(&path).unwrap().expect("rpus were found");

    assert_eq!(summary.max_content_light_level_nits, None);
    assert!(
        (summary.peak_luminance_nits - 600.0).abs() < 1.0,
        "level 1 peak was {} nits",
        summary.peak_luminance_nits
    );
}

#[test]
fn a_plain_hevc_stream_has_no_dolby_vision() {
    let directory = tempfile::tempdir().unwrap();
    let path = encode_plain_hevc(directory.path());

    assert_eq!(read_dolby_vision(&path).unwrap(), None);
}

#[test]
fn a_non_hevc_file_returns_none() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("plain.mp4");
    run(
        "ffmpeg",
        &[
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=gray:s=320x180:r=25",
            "-frames:v",
            "6",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            path.to_str().unwrap(),
        ],
    );

    assert_eq!(read_dolby_vision(&path).unwrap(), None);
}

#[test]
fn profile_5_is_refused() {
    let summary = DolbyVisionSummary {
        profile: 5,
        frames: FIXTURE_FRAMES,
        shots: 1,
        max_content_light_level_nits: None,
        max_frame_average_light_level_nits: None,
        peak_luminance_nits: 4000.0,
        mastering_display_max_nits: None,
        mastering_display_min_nits: None,
    };

    let refusal = refuse_undecodable_dolby_vision(&summary).unwrap_err();

    assert!(refusal.contains("profile 5"), "{refusal}");
    assert!(refusal.contains("profile 8.1"), "{refusal}");
}
