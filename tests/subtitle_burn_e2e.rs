//! Subtitle burn-in through the real encode, end to end.
//!
//! Encodes the same clip twice, once plain and once with a bitmap cue burnt in,
//! decodes both with `grk_decompress`, and compares the two rasters. The cue is
//! a solid PNG at a known spot, so its region has to move by a lot while the
//! rest of the frame barely moves at all.
//!
//! Not "identical outside": the wavelet is lossy and spreads a change across
//! the whole picture, so untouched pixels drift by a fraction of a percent.
//! Exact before-and-after pixels are asserted in the `grok_encoder` unit tests,
//! which see the frame buffer itself.
//!
//! A bitmap cue rather than text, so the test does not depend on a system font.

use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_with_options};
use postkit::subtitle_formats::{StyledCue, VAlign};
use postkit::subtitle_raster::{BurnStyle, SubtitleBurn};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const FRAME_COUNT: u64 = 4;
const WIDTH: u32 = 128;
const HEIGHT: u32 = 128;
/// The cue PNG's size, and so the size of the region that must change.
const CUE_WIDTH: u32 = 64;
const CUE_HEIGHT: u32 = 32;

fn have_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn find_grk_decompress() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let path = PathBuf::from(home).join("bin/grok/bin/grk_decompress");
        if path.exists() {
            return Some(path);
        }
    }
    std::process::Command::new("which")
        .arg("grk_decompress")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
}

fn make_clip(video: &Path) {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=s={WIDTH}x{HEIGHT}:d=1:r={FRAME_COUNT}"),
            "-frames:v",
            &FRAME_COUNT.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(video)
        .output()
        .expect("ffmpeg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_solid_png(path: &Path, rgba: [u8; 4]) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), CUE_WIDTH, CUE_HEIGHT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = rgba
        .iter()
        .copied()
        .cycle()
        .take((CUE_WIDTH * CUE_HEIGHT * 4) as usize)
        .collect();
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&data)
        .unwrap();
}

/// A burn holding one full-strength bitmap cue over `start_ms..10s`.
fn burn_of(cue_png: &Path, start_ms: u64) -> Arc<SubtitleBurn> {
    let mut cue = StyledCue::text(start_ms, 10_000, Vec::new());
    cue.image = Some(cue_png.to_path_buf());
    cue.valign = Some(VAlign::Top);
    cue.vposition = Some(0.0);
    Arc::new(
        SubtitleBurn::new(vec![cue], None, BurnStyle::default(), FRAME_COUNT as f64).expect("burn"),
    )
}

fn encode(video: &Path, output: &Path, burn: Option<Arc<SubtitleBurn>>) -> PathBuf {
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    run_encode_with_options(
        video,
        output,
        &EncodeRunOptions {
            fps: FRAME_COUNT as u32,
            subtitle_burn: burn,
            ..Default::default()
        },
        &cancel,
        &pause,
        |_: &PipelineProgress| {},
        |_: &str| {},
    )
    .expect("encode")
    .j2k_dir
}

/// Decode one codestream to a 16-bit-per-channel PPM and return its samples.
fn decode_frame(grk_decompress: &Path, codestream: &Path, out: &Path) -> Vec<u16> {
    let output = std::process::Command::new(grk_decompress)
        .env("LD_LIBRARY_PATH", postkit::grok::grok_lib_path())
        .args(["-i", &codestream.to_string_lossy()])
        .args(["-o", &out.to_string_lossy()])
        .output()
        .expect("grk_decompress");
    assert!(
        output.status.success(),
        "grk_decompress failed on {}: {}",
        codestream.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(out).expect("decoded ppm");
    // P6 header: magic, width, height, maxval, each whitespace-separated, then
    // one whitespace byte before the raster.
    let mut at = 0usize;
    let mut fields = 0;
    while fields < 4 {
        while bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes[at] == b'#' {
            while bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        while !bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        fields += 1;
    }
    at += 1;
    bytes[at..]
        .chunks_exact(2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .collect()
}

#[test]
fn a_burnt_cue_changes_its_own_region_and_nothing_else() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let Some(grk_decompress) = find_grk_decompress() else {
        eprintln!("skipping: grk_decompress not found");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    make_clip(&video);

    let cue_png = dir.path().join("cue.png");
    write_solid_png(&cue_png, [255, 0, 0, 255]);
    let mut cue = StyledCue::text(0, 10_000, Vec::new());
    cue.image = Some(cue_png);
    cue.valign = Some(VAlign::Top);
    cue.vposition = Some(0.0);
    let burn = Arc::new(
        SubtitleBurn::new(vec![cue], None, BurnStyle::default(), FRAME_COUNT as f64).expect("burn"),
    );

    let plain_dir = encode(&video, &dir.path().join("plain"), None);
    let burnt_dir = encode(&video, &dir.path().join("burnt"), Some(burn));

    let plain = decode_frame(
        &grk_decompress,
        &plain_dir.join("frame_00000000.j2c"),
        &dir.path().join("plain.ppm"),
    );
    let burnt = decode_frame(
        &grk_decompress,
        &burnt_dir.join("frame_00000000.j2c"),
        &dir.path().join("burnt.ppm"),
    );
    assert_eq!(plain.len(), (WIDTH * HEIGHT * 3) as usize);
    assert_eq!(burnt.len(), plain.len());

    assert_cue_region_changed(&plain, &burnt);
}

/// The cue is top-anchored at vposition 0 and centred, so it covers the top
/// CUE_HEIGHT rows, centred horizontally. That region has to move by a lot and
/// come back flat; the rest of the frame only drifts by the wavelet's spread.
fn assert_cue_region_changed(plain: &[u16], burnt: &[u16]) {
    let left = ((WIDTH - CUE_WIDTH) / 2) as usize;
    let right = left + CUE_WIDTH as usize;
    let mut inside_total = 0u64;
    let mut inside_count = 0u64;
    let mut outside_total = 0u64;
    let mut outside_count = 0u64;
    let mut outside_worst = 0u32;
    for row in 0..HEIGHT as usize {
        for column in 0..WIDTH as usize {
            let at = (row * WIDTH as usize + column) * 3;
            let difference: u32 = (0..3)
                .map(|c| plain[at + c].abs_diff(burnt[at + c]) as u32)
                .sum();
            if row < CUE_HEIGHT as usize && (left..right).contains(&column) {
                inside_total += difference as u64;
                inside_count += 1;
            } else {
                outside_total += difference as u64;
                outside_count += 1;
                outside_worst = outside_worst.max(difference);
            }
        }
    }
    let inside = inside_total as f64 / inside_count as f64;
    let outside = outside_total as f64 / outside_count as f64;
    assert!(
        inside > 1000.0,
        "the cue region barely moved: mean sample change {inside:.1} of 65535"
    );
    assert!(
        outside < inside / 20.0,
        "pixels outside the cue moved too much: mean {outside:.1} against {inside:.1} inside"
    );
    assert!(
        (outside_worst as f64) < inside / 3.0,
        "one pixel outside the cue moved by {outside_worst}, against {inside:.1} mean inside"
    );

    // A flat overlay, so the covered region comes back near-uniform where the
    // test pattern under it was not.
    assert!(
        spread(burnt, left, right) * 4 < spread(plain, left, right),
        "the cue region is not flatter than the picture it replaced"
    );
}

/// Range of the first component over the cue region, as a stand-in for how
/// varied that patch of picture is.
fn spread(frame: &[u16], left: usize, right: usize) -> u32 {
    let mut low = u16::MAX;
    let mut high = 0u16;
    for row in 0..CUE_HEIGHT as usize {
        for column in left..right {
            let sample = frame[(row * WIDTH as usize + column) * 3];
            low = low.min(sample);
            high = high.max(sample);
        }
    }
    (high - low) as u32
}

/// An image sequence is compressed straight from file by `grk_compress`, which
/// never shows postkit a frame buffer. A burn takes it through ffmpeg's concat
/// demuxer instead, so this proves the cue actually lands on those frames.
#[test]
fn an_image_sequence_burns_through_the_concat_demuxer() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let Some(grk_decompress) = find_grk_decompress() else {
        eprintln!("skipping: grk_decompress not found");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let frames_dir = dir.path().join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    for index in 0..FRAME_COUNT {
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=s={WIDTH}x{HEIGHT}:d=1:r=1"),
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgb48be",
            ])
            .arg(frames_dir.join(format!("frame_{index:04}.tiff")))
            .output()
            .expect("ffmpeg");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let cue_png = dir.path().join("cue.png");
    write_solid_png(&cue_png, [255, 0, 0, 255]);
    // Every source image is the same picture, and the cue only starts at frame
    // 2, so any difference between frames 0 and 2 of one encode is the burn.
    let cue_from_frame = 2u64;
    let burnt_dir = encode(
        &frames_dir,
        &dir.path().join("burnt"),
        Some(burn_of(&cue_png, cue_from_frame * 1000 / FRAME_COUNT)),
    );
    assert_eq!(
        postkit::grok_encoder::contiguous_encoded_frames(&burnt_dir),
        FRAME_COUNT,
        "one codestream per source image in {}",
        burnt_dir.display()
    );

    let before = decode_frame(
        &grk_decompress,
        &burnt_dir.join("frame_00000000.j2c"),
        &dir.path().join("before.ppm"),
    );
    let after = decode_frame(
        &grk_decompress,
        &burnt_dir.join(format!("frame_{cue_from_frame:08}.j2c")),
        &dir.path().join("after.ppm"),
    );
    assert_cue_region_changed(&before, &after);
}
