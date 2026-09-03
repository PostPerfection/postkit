//! Subtitle burn-in through the real encode, end to end.
//!
//! Encodes the same clip twice, once plain and once with a bitmap cue burnt in,
//! decodes both in memory, and compares the two rasters. The cue is
//! a solid PNG at a known spot, so its region has to move by a lot while the
//! rest of the frame barely moves at all.
//!
//! Not "identical outside": the wavelet is lossy and spreads a change across
//! the whole picture, so untouched pixels drift by a fraction of a percent.
//! Exact before-and-after pixels are asserted in the `grok_encoder` unit tests,
//! which see the frame buffer itself.
//!
//! A bitmap cue rather than text, so the test does not depend on a system font.

use postkit::encode::FrameRate;
use postkit::grok_encoder::SampleOrder;
use postkit::pipeline::{EncodeRunOptions, PipelineProgress, run_encode_with_options};
use postkit::subtitle_formats::{Rgba, StyledCue, StyledRun, VAlign};
use postkit::subtitle_raster::{BurnEffect, BurnStyle, SubtitleBurn};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const FRAME_COUNT: u64 = 4;
const WIDTH: u32 = 128;
const HEIGHT: u32 = 128;
/// The cue PNG's size, and so the size of the region that must change.
const CUE_WIDTH: u32 = 64;
const CUE_HEIGHT: u32 = 32;

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
            fps: FrameRate::whole(FRAME_COUNT as u32),
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

/// Decode one codestream in memory and return its samples pixel-interleaved
/// at the codestream's own 12 bits.
fn decode_frame(codestream: &Path) -> Vec<u16> {
    let data = std::fs::read(codestream).expect("codestream");
    postkit::grok_decoder::decode(data, 0)
        .unwrap_or_else(|e| panic!("cannot decode {}: {e}", codestream.display()))
        .interleaved_samples()
        .expect("three components")
}

#[test]
fn a_burnt_cue_changes_its_own_region_and_nothing_else() {
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

    let plain = decode_frame(&plain_dir.join("frame_00000000.j2c"));
    let burnt = decode_frame(&burnt_dir.join("frame_00000000.j2c"));
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

/// A TIFF sequence is read by postkit's own loader rather than decoded by
/// ffmpeg, so this proves the cue lands on those frames too.
#[test]
fn a_tiff_sequence_burns_on_the_frames_postkit_reads() {
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

    let before = decode_frame(&burnt_dir.join("frame_00000000.j2c"));
    let after = decode_frame(&burnt_dir.join(format!("frame_{cue_from_frame:08}.j2c")));
    assert_cue_region_changed(&before, &after);
}

// The appearance controls below are checked on the frame buffer rather than
// through an encode: the wavelet is lossy, and an outline a few pixels wide
// would not survive it well enough to say which pixel was drawn in which
// colour. The burn itself is still the real one, cue list to packed rgb48.

const TEXT_WIDTH: u32 = 512;
const TEXT_HEIGHT: u32 = 512;
const TEXT_FPS: u64 = 8;
/// Text height as a fraction of the frame, big enough that an outline or a
/// shadow is several pixels wide on this small frame.
const TEXT_SIZE_RATIO: f32 = 0.12;
/// Flat grey the test frames start at, low enough that white text reads bright
/// against it and a black effect reads dark.
const BACKGROUND: u16 = 0x4000;
/// How far a sample has to move from the background to count as drawn on.
const STRONG: u16 = 0x2000;

fn text_style(effect: BurnEffect) -> BurnStyle {
    BurnStyle {
        effect,
        font_size_ratio: TEXT_SIZE_RATIO,
        ..BurnStyle::default()
    }
}

/// A burn of one centred text cue.
fn text_burn(start_ms: u64, end_ms: u64, style: BurnStyle) -> SubtitleBurn {
    let mut cue = StyledCue::text(start_ms, end_ms, vec![StyledRun::plain("Hello")]);
    cue.valign = Some(VAlign::Middle);
    SubtitleBurn::new(vec![cue], None, style, TEXT_FPS as f64).expect("burn")
}

/// Burn one frame over the flat background and return its 16-bit samples.
fn burnt_frame(burn: &SubtitleBurn, frame_index: u64) -> Vec<u16> {
    let mut frame = vec![0u8; (TEXT_WIDTH * TEXT_HEIGHT * 6) as usize];
    for sample in frame.as_chunks_mut::<2>().0 {
        *sample = BACKGROUND.to_be_bytes();
    }
    burn.burn_rgb48(
        &mut frame,
        TEXT_WIDTH,
        TEXT_HEIGHT,
        SampleOrder::Big,
        frame_index,
    )
    .expect("burn");
    frame
        .as_chunks::<2>()
        .0
        .iter()
        .map(|s| u16::from_be_bytes(*s))
        .collect()
}

fn red_at(frame: &[u16], row: usize, column: usize) -> u16 {
    frame[(row * TEXT_WIDTH as usize + column) * 3]
}

fn is_bright(sample: u16) -> bool {
    sample > BACKGROUND + STRONG
}

fn is_dark(sample: u16) -> bool {
    sample < BACKGROUND - STRONG
}

/// Row and column of every pixel whose red sample passes `matches`.
fn pixels_matching(frame: &[u16], matches: fn(u16) -> bool) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for row in 0..TEXT_HEIGHT as usize {
        for column in 0..TEXT_WIDTH as usize {
            if matches(red_at(frame, row, column)) {
                found.push((row, column));
            }
        }
    }
    found
}

fn centre_of(pixels: &[(usize, usize)]) -> (f64, f64) {
    let rows: usize = pixels.iter().map(|(row, _)| row).sum();
    let columns: usize = pixels.iter().map(|(_, column)| column).sum();
    let count = pixels.len() as f64;
    (rows as f64 / count, columns as f64 / count)
}

/// Largest distance any sample was moved from the flat background.
fn deviation(frame: &[u16]) -> u32 {
    frame
        .iter()
        .map(|s| s.abs_diff(BACKGROUND) as u32)
        .max()
        .unwrap_or(0)
}

#[test]
fn an_outline_draws_effect_colour_around_the_text() {
    let plain = text_burn(0, 1000, text_style(BurnEffect::None));
    let outlined = text_burn(0, 1000, text_style(BurnEffect::Outline));
    let plain = burnt_frame(&plain, 0);
    let outlined = burnt_frame(&outlined, 0);

    let mut bordering = 0usize;
    for (row, column) in pixels_matching(&outlined, is_dark) {
        // Only pixels the plain style never touched: the outline is what is new.
        if red_at(&plain, row, column) != BACKGROUND {
            continue;
        }
        let touches_text = [
            (row.wrapping_sub(1), column),
            (row + 1, column),
            (row, column.wrapping_sub(1)),
            (row, column + 1),
        ]
        .iter()
        .any(|&(r, c)| {
            r < TEXT_HEIGHT as usize
                && c < TEXT_WIDTH as usize
                && is_bright(red_at(&outlined, r, c))
        });
        if touches_text {
            bordering += 1;
        }
    }
    assert!(
        bordering > 20,
        "only {bordering} outline pixels sit against the text"
    );
    assert!(
        pixels_matching(&plain, is_dark).is_empty(),
        "the plain style drew effect-colour pixels of its own"
    );
}

#[test]
fn a_shadow_falls_below_and_right_of_the_text() {
    let burn = text_burn(0, 1000, text_style(BurnEffect::Shadow));
    let frame = burnt_frame(&burn, 0);
    let text = pixels_matching(&frame, is_bright);
    let shadow = pixels_matching(&frame, is_dark);
    assert!(!text.is_empty(), "no text was drawn");
    assert!(!shadow.is_empty(), "no shadow was drawn");

    let (text_row, text_column) = centre_of(&text);
    let (shadow_row, shadow_column) = centre_of(&shadow);
    assert!(
        shadow_row > text_row && shadow_column > text_column,
        "the shadow's centre is at ({shadow_row:.1}, {shadow_column:.1}), \
         the text's at ({text_row:.1}, {text_column:.1})"
    );

    let top_text = text.iter().map(|(row, _)| row).min().unwrap();
    let top_shadow = shadow.iter().map(|(row, _)| row).min().unwrap();
    let left_text = text.iter().map(|(_, column)| column).min().unwrap();
    let left_shadow = shadow.iter().map(|(_, column)| column).min().unwrap();
    assert!(
        top_shadow > top_text && left_shadow > left_text,
        "shadow pixels reach above or left of the text: shadow starts at row \
         {top_shadow} column {left_shadow}, text at row {top_text} column {left_text}"
    );
}

#[test]
fn a_fade_ramps_the_cue_up_and_leaves_the_frames_around_it_alone() {
    let style = BurnStyle {
        fade_up_ms: 500,
        ..text_style(BurnEffect::None)
    };
    // The cue runs 0..1000ms at 8fps, so the ramp covers frames 0 to 4.
    let burn = text_burn(0, 1000, style);
    let at_start = burnt_frame(&burn, 0);
    let ramping = burnt_frame(&burn, 1);
    let full = burnt_frame(&burn, 4);
    let after = burnt_frame(&burn, 8);

    assert_eq!(
        deviation(&at_start),
        0,
        "the cue is transparent at its own start"
    );
    assert!(
        deviation(&ramping) > 0,
        "the cue never appeared during the ramp"
    );
    assert!(
        deviation(&ramping) < deviation(&full) / 2,
        "mid-ramp deviation {} against {} once the ramp is done",
        deviation(&ramping),
        deviation(&full)
    );
    assert_eq!(deviation(&after), 0, "a frame past the cue was drawn on");
}

#[test]
fn x_scale_stretches_the_text_horizontally_only() {
    let plain = text_burn(0, 1000, text_style(BurnEffect::None));
    let stretched = text_burn(
        0,
        1000,
        BurnStyle {
            x_scale: 2.0,
            ..text_style(BurnEffect::None)
        },
    );
    let plain = pixels_matching(&burnt_frame(&plain, 0), is_bright);
    let stretched = pixels_matching(&burnt_frame(&stretched, 0), is_bright);

    let span = |pixels: &[(usize, usize)], axis: fn(&(usize, usize)) -> usize| {
        let low = pixels.iter().map(axis).min().unwrap();
        let high = pixels.iter().map(axis).max().unwrap();
        (high - low + 1) as f64
    };
    let width_ratio = span(&stretched, |p| p.1) / span(&plain, |p| p.1);
    let height_ratio = span(&stretched, |p| p.0) / span(&plain, |p| p.0);
    assert!(
        (1.8..2.2).contains(&width_ratio),
        "x_scale 2.0 widened the text by {width_ratio:.2}x"
    );
    assert!(
        (0.95..1.05).contains(&height_ratio),
        "x_scale 2.0 changed the text height by {height_ratio:.2}x"
    );
}

#[test]
fn frames_inside_one_cue_burn_identically_unless_a_fade_is_running() {
    let steady = text_burn(0, 1000, text_style(BurnEffect::Shadow));
    assert_eq!(
        burnt_frame(&steady, 1),
        burnt_frame(&steady, 2),
        "two frames of one unfading cue burnt differently"
    );

    let fading = text_burn(
        0,
        1000,
        BurnStyle {
            fade_down_ms: 1000,
            ..text_style(BurnEffect::Shadow)
        },
    );
    assert_ne!(
        burnt_frame(&fading, 1),
        burnt_frame(&fading, 2),
        "a fading cue burnt the same on two frames, so the render cache held a \
         stale alpha"
    );
}

#[test]
fn an_effect_is_drawn_in_the_colour_it_names() {
    let style = BurnStyle {
        effect_colour: Rgba {
            r: 0,
            g: 0,
            b: 255,
            a: 255,
        },
        ..text_style(BurnEffect::Outline)
    };
    let burn = text_burn(0, 1000, style);
    let frame = burnt_frame(&burn, 0);
    let blue = (0..TEXT_HEIGHT as usize)
        .flat_map(|row| (0..TEXT_WIDTH as usize).map(move |column| (row, column)))
        .filter(|&(row, column)| {
            let at = (row * TEXT_WIDTH as usize + column) * 3;
            is_dark(frame[at]) && is_dark(frame[at + 1]) && is_bright(frame[at + 2])
        })
        .count();
    assert!(blue > 20, "only {blue} pixels took the blue effect colour");
}
