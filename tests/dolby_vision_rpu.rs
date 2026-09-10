use dolby_vision::rpu::extension_metadata::blocks::ExtMetadataBlockLevel6;
use dolby_vision::utils::add_start_code_emulation_prevention_3_byte;
use postkit::dolby_vision::{
    DOLBY_VISION_FIXTURE_FRAMES, DolbyVisionFixtureProfile, DolbyVisionSummary, DvMode,
    convert_dv_mode, generate_profile81_rpu, parse_single_rpu, read_dolby_vision,
    refuse_undecodable_dolby_vision, write_dolby_vision_fixture,
};
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_CONTENT_LIGHT_LEVEL: u16 = 1000;
const MAX_FRAME_AVERAGE_LIGHT_LEVEL: u16 = 400;
const MASTERING_DISPLAY_MAX_NITS: u16 = 1000;
const MASTERING_DISPLAY_MIN_STEPS: u16 = 1;
// 12 bit PQ code for 600 cd/m²
const PQ_CODE_600_NITS: u16 = 2851;

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

#[test]
fn an_annex_b_stream_with_rpus_reports_profile_8_and_level_6() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_dolby_vision_fixture(
        directory.path(),
        "dv.hevc",
        DolbyVisionFixtureProfile::Profile81,
        Some(level6_block()),
        None,
    )
    .unwrap();

    let summary = read_dolby_vision(&path).unwrap().expect("rpus were found");

    assert_eq!(summary.profile, 8);
    assert_eq!(summary.frames, DOLBY_VISION_FIXTURE_FRAMES);
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
    let annex_b = write_dolby_vision_fixture(
        directory.path(),
        "dv.hevc",
        DolbyVisionFixtureProfile::Profile81,
        Some(level6_block()),
        None,
    )
    .unwrap();
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
    assert_eq!(summary.frames, DOLBY_VISION_FIXTURE_FRAMES);
    assert_eq!(summary.max_content_light_level_nits, Some(1000.0));
    assert_eq!(summary.max_frame_average_light_level_nits, Some(400.0));
    assert_eq!(summary.peak_luminance_nits, 1000.0);
}

#[test]
fn level_1_peak_is_used_when_level_6_is_absent() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_dolby_vision_fixture(
        directory.path(),
        "dv.hevc",
        DolbyVisionFixtureProfile::Profile81,
        None,
        Some(PQ_CODE_600_NITS),
    )
    .unwrap();

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
        frames: DOLBY_VISION_FIXTURE_FRAMES,
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

#[test]
fn a_profile_5_fixture_reads_back_and_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_dolby_vision_fixture(
        directory.path(),
        "dv5.hevc",
        DolbyVisionFixtureProfile::Profile5,
        Some(level6_block()),
        None,
    )
    .unwrap();

    let summary = read_dolby_vision(&path).unwrap().expect("rpus were found");

    assert_eq!(summary.profile, 5);
    assert_eq!(summary.frames, DOLBY_VISION_FIXTURE_FRAMES);

    let refusal = refuse_undecodable_dolby_vision(&summary).unwrap_err();

    assert!(refusal.contains("profile 5"), "{refusal}");
    assert!(refusal.contains("profile 8.1"), "{refusal}");
}

// the summary carries no field that separates 8.4 from 8.1, both read back as profile 8
#[test]
fn a_profile_8_4_fixture_reads_back_as_profile_8() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_dolby_vision_fixture(
        directory.path(),
        "dv84.hevc",
        DolbyVisionFixtureProfile::Profile84,
        Some(level6_block()),
        None,
    )
    .unwrap();

    let summary = read_dolby_vision(&path).unwrap().expect("rpus were found");

    assert_eq!(summary.profile, 8);
    assert_eq!(summary.frames, DOLBY_VISION_FIXTURE_FRAMES);
    assert_eq!(summary.mastering_display_max_nits, Some(1000.0));
    assert!(refuse_undecodable_dolby_vision(&summary).is_ok());
}

// the reshaping curve mode 5 writes, an eight segment polynomial over the luma
const PROFILE_84_LUMA_PIVOTS: [u16; 9] = [63, 69, 230, 256, 256, 37, 16, 8, 7];
const NAL_START_CODE: [u8; 4] = [0, 0, 0, 1];

// 00 00 03 is the escape itself, a 00, 01 or 02 in its place would read as a start code
fn emulated_start_code(data: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + 2 < data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] <= 3 {
            if data[index + 2] != 3 {
                return Some(index);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    None
}

#[test]
fn convert_dv_mode_reads_and_writes_an_escaped_rpu_bin() {
    let directory = tempfile::tempdir().unwrap();

    let mut escaped = generate_profile81_rpu().unwrap();
    let unescaped_length = escaped.len();
    add_start_code_emulation_prevention_3_byte(&mut escaped);
    assert!(
        escaped.len() > unescaped_length,
        "the generated RPU carries nothing to escape, so this proves no parsing"
    );

    let input = directory.path().join("profile81.bin");
    let mut bin = NAL_START_CODE.to_vec();
    bin.extend_from_slice(&escaped);
    std::fs::write(&input, &bin).unwrap();

    let output = directory.path().join("profile84.bin");
    convert_dv_mode(&input, &output, DvMode::To84).unwrap();

    let converted = std::fs::read(&output).unwrap();
    assert!(converted.starts_with(&NAL_START_CODE));
    let payload = &converted[NAL_START_CODE.len()..];
    assert_eq!(
        emulated_start_code(payload),
        None,
        "the written RPU emulates a start code"
    );

    let parsed = parse_single_rpu(&converted).unwrap();
    assert_ne!(
        payload,
        parsed.write_rpu().unwrap().as_slice(),
        "the written RPU carries no emulation prevention"
    );
    assert_eq!(parsed.dovi_profile, 8);
    assert_eq!(
        parsed.rpu_data_mapping.unwrap().curves[0].pivots,
        PROFILE_84_LUMA_PIVOTS
    );
}
