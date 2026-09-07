use postkit::trailer::{TrailerOptions, package_trailer};
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

static ONE_FFMPEG_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_ffmpeg_at_a_time() -> MutexGuard<'static, ()> {
    ONE_FFMPEG_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const RATINGS_CARD_SECONDS: u32 = 5;
const COUNTDOWN_SECONDS: u32 = 2;
const CONTENT_SECONDS: u32 = 2;
const FRAME_RATE: u32 = 24;
const GREY_LEVEL: i32 = 0x80;
const GREY_TOLERANCE: i32 = 6;

fn run(step: &str, command: &mut Command) -> std::process::Output {
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("{step}: failed to run: {e}"));
    assert!(
        output.status.success(),
        "{step} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn make_content(path: &Path, size: &str, encoder_args: &[&str]) {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg(format!(
            "color=c=0x808080:s={size}:d={CONTENT_SECONDS}:r={FRAME_RATE}"
        ))
        .args(encoder_args)
        .arg(path);
    run("content clip", &mut command);
}

fn probe(path: &Path, entries: &str) -> String {
    let output = run(
        "ffprobe",
        Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-count_frames")
            .arg("-show_entries")
            .arg(entries)
            .arg("-of")
            .arg("csv=p=0")
            .arg(path),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn last_frame_grey_level(path: &Path, width: u32, height: u32) -> [i32; 3] {
    let raw = path.with_extension("last.rgb");
    run(
        "last frame",
        Command::new("ffmpeg")
            .arg("-y")
            .arg("-sseof")
            .arg("-0.1")
            .arg("-i")
            .arg(path)
            .arg("-vframes")
            .arg("1")
            .arg("-pix_fmt")
            .arg("rgb24")
            .arg("-f")
            .arg("rawvideo")
            .arg(&raw),
    );

    let pixels = std::fs::read(&raw).unwrap();
    assert_eq!(pixels.len(), (width * height * 3) as usize);
    let mut sums = [0i64; 3];
    for pixel in pixels.as_chunks::<3>().0 {
        for channel in 0..3 {
            sums[channel] += i64::from(pixel[channel]);
        }
    }
    let count = i64::from(width * height);
    [
        (sums[0] / count) as i32,
        (sums[1] / count) as i32,
        (sums[2] / count) as i32,
    ]
}

fn expected_frames() -> u32 {
    (RATINGS_CARD_SECONDS + COUNTDOWN_SECONDS + CONTENT_SECONDS) * FRAME_RATE
}

fn package(content: &Path, output_dir: &Path) -> postkit::trailer::TrailerResult {
    package_trailer(&TrailerOptions {
        content_dir: content.to_path_buf(),
        output_dir: output_dir.to_path_buf(),
        title: "Test Feature".into(),
        rating: "PG".into(),
        countdown_seconds: COUNTDOWN_SECONDS,
        fps_num: FRAME_RATE,
        fps_den: 1,
        ..Default::default()
    })
}

#[test]
fn a_prores_trailer_keeps_its_content() {
    let _guard = one_ffmpeg_at_a_time();
    let dir = tempfile::tempdir().unwrap();
    let content = dir.path().join("content.mov");
    make_content(
        &content,
        "2048x1080",
        &["-c:v", "prores_ks", "-profile:v", "2"],
    );

    let result = package(&content, &dir.path().join("out"));
    assert!(result.success, "{}", result.error);

    assert_eq!(
        probe(&result.output_file, "stream=nb_read_frames"),
        expected_frames().to_string()
    );
    assert_eq!(
        probe(&result.output_file, "stream=width,height"),
        "2048,1080"
    );

    let grey = last_frame_grey_level(&result.output_file, 2048, 1080);
    for channel in grey {
        assert!(
            (channel - GREY_LEVEL).abs() <= GREY_TOLERANCE,
            "last frame is not the grey content: {grey:?}"
        );
    }
}

#[test]
fn an_h264_trailer_at_1920x1080_still_joins() {
    let _guard = one_ffmpeg_at_a_time();
    let dir = tempfile::tempdir().unwrap();
    let content = dir.path().join("content.mp4");
    make_content(
        &content,
        "1920x1080",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
    );

    let result = package(&content, &dir.path().join("out"));
    assert!(result.success, "{}", result.error);

    assert_eq!(
        probe(&result.output_file, "stream=nb_read_frames"),
        expected_frames().to_string()
    );
    assert_eq!(
        probe(&result.output_file, "stream=width,height"),
        "1920,1080"
    );
}

#[test]
fn a_short_join_is_reported_not_swallowed() {
    let _guard = one_ffmpeg_at_a_time();
    let dir = tempfile::tempdir().unwrap();
    let content = dir.path().join("content.mov");
    std::fs::write(&content, b"this is not a video").unwrap();

    let result = package(&content, &dir.path().join("out"));
    assert!(!result.success, "expected failure, got {result:?}");
    assert!(result.error.contains("content probe"), "{}", result.error);
}
