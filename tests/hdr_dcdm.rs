#![cfg(feature = "grok-ffi")]

use postkit::colour::{HdrDcdmTransform, HdrSource};
use postkit::encode::{SourceColour, StreamEncodeOptions, default_rsiz, stream_encode_inprocess};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const WIDTH: u32 = 2048;
const HEIGHT: u32 = 1080;
const FRAMES: u64 = 2;
const FRAME_RATE: u32 = 24;

const REFERENCE_WHITE_NITS: f64 = 299.6;
const ADDENDUM_REFERENCE_WHITE_CODES: [u16; 3] = [2524, 2546, 2583];
// inside P3-D65 so no channel lands on the volume floor, where PQ turns a rounding into tens of codes
const COLOURED_PATCH_NITS: [f64; 3] = [200.0, 100.0, 50.0];
const TWELVE_BIT_MAX_CODE: u16 = 4095;
const DCI_PRECISION_BITS: u8 = 12;
const CODE_TOLERANCE: i32 = 2;

const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;
const PQ_C1: f64 = PQ_C3 - PQ_C2 + 1.0;
const PQ_PEAK_NITS: f64 = 10_000.0;

const BT2020_LUMINANCE_WEIGHTS: [f64; 3] = [0.2627, 0.6780, 0.0593];
const BT709_LUMINANCE_WEIGHTS: [f64; 3] = [0.2126, 0.7152, 0.0722];

// 10-bit studio range: luma 64 to 940, chroma 512 either way by 448
const TEN_BIT_LUMA_OFFSET: f64 = 64.0;
const TEN_BIT_LUMA_SPAN: f64 = 876.0;
const TEN_BIT_CHROMA_OFFSET: f64 = 512.0;
const TEN_BIT_CHROMA_SPAN: f64 = 896.0;

fn pq_signal_from_nits(nits: f64) -> f64 {
    let ratio = (nits / PQ_PEAK_NITS).max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * ratio) / (1.0 + PQ_C3 * ratio)).powf(PQ_M2)
}

fn studio_range_ycbcr(rgb_signal: [f64; 3], weights: [f64; 3]) -> [u16; 3] {
    let luma = weights[0] * rgb_signal[0] + weights[1] * rgb_signal[1] + weights[2] * rgb_signal[2];
    let blue_chroma = (rgb_signal[2] - luma) / (2.0 * (1.0 - weights[2]));
    let red_chroma = (rgb_signal[0] - luma) / (2.0 * (1.0 - weights[0]));
    [
        (TEN_BIT_LUMA_OFFSET + TEN_BIT_LUMA_SPAN * luma).round() as u16,
        (TEN_BIT_CHROMA_OFFSET + TEN_BIT_CHROMA_SPAN * blue_chroma).round() as u16,
        (TEN_BIT_CHROMA_OFFSET + TEN_BIT_CHROMA_SPAN * red_chroma).round() as u16,
    ]
}

fn rgb48_from_studio_range_ycbcr(ycbcr: [u16; 3], weights: [f64; 3]) -> [u16; 3] {
    let luma = (f64::from(ycbcr[0]) - TEN_BIT_LUMA_OFFSET) / TEN_BIT_LUMA_SPAN;
    let blue_chroma = (f64::from(ycbcr[1]) - TEN_BIT_CHROMA_OFFSET) / TEN_BIT_CHROMA_SPAN;
    let red_chroma = (f64::from(ycbcr[2]) - TEN_BIT_CHROMA_OFFSET) / TEN_BIT_CHROMA_SPAN;
    let red = luma + red_chroma * 2.0 * (1.0 - weights[0]);
    let blue = luma + blue_chroma * 2.0 * (1.0 - weights[2]);
    let green = (luma - weights[0] * red - weights[2] * blue) / weights[1];
    [red, green, blue].map(|signal| (signal.clamp(0.0, 1.0) * f64::from(u16::MAX)).round() as u16)
}

// ffv1 is lossless, so the encode reads the code values written here
fn hdr10_clip(dir: &Path, white: [u16; 3], patch: [u16; 3]) -> PathBuf {
    let raw = dir.join("hdr10.yuv");
    let luma_width = WIDTH as usize;
    let chroma_width = luma_width / 2;
    let mut frame = Vec::new();
    for _ in 0..HEIGHT {
        for column in 0..luma_width {
            let sample = if column < luma_width / 2 {
                white[0]
            } else {
                patch[0]
            };
            frame.extend_from_slice(&sample.to_le_bytes());
        }
    }
    for plane in 1..=2 {
        for _ in 0..HEIGHT / 2 {
            for column in 0..chroma_width {
                let sample = if column < chroma_width / 2 {
                    white[plane]
                } else {
                    patch[plane]
                };
                frame.extend_from_slice(&sample.to_le_bytes());
            }
        }
    }
    let mut raw_bytes = Vec::with_capacity(frame.len() * FRAMES as usize);
    for _ in 0..FRAMES {
        raw_bytes.extend_from_slice(&frame);
    }
    std::fs::write(&raw, &raw_bytes).expect("the raw planes have to be written");

    // the colour tags go on the input as well: an untagged rawvideo frame makes
    // ffmpeg convert the range on the way into the encoder and the codes move
    const COLOUR_TAGS: [&str; 8] = [
        "-color_primaries",
        "bt2020",
        "-color_trc",
        "smpte2084",
        "-colorspace",
        "bt2020nc",
        "-color_range",
        "tv",
    ];
    let clip = dir.join("hdr10.mkv");
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "rawvideo"])
        .args(["-pix_fmt", "yuv420p10le"])
        .args(["-s", &format!("{WIDTH}x{HEIGHT}")])
        .args(["-r", &FRAME_RATE.to_string()])
        .args(COLOUR_TAGS)
        .arg("-i")
        .arg(&raw)
        .args(["-c:v", "ffv1"])
        .args(COLOUR_TAGS)
        .arg(&clip)
        .status()
        .expect("ffmpeg has to run");
    assert!(
        status.success(),
        "ffmpeg could not write {}",
        clip.display()
    );
    clip
}

fn decoded_pixel(codestream: &Path, x: u32, y: u32) -> [i32; 3] {
    let bytes = std::fs::read(codestream).expect("the codestream has to be readable");
    let frame = postkit::grok_decoder::decode(bytes, 0).expect("the codestream has to decode");
    let at = (y * frame.width + x) as usize;
    let shift = i32::from(frame.precision) - i32::from(DCI_PRECISION_BITS);
    assert!(
        shift >= 0,
        "a {}-bit codestream is below the {DCI_PRECISION_BITS} bits a DCI picture carries",
        frame.precision
    );
    [0, 1, 2].map(|component| frame.components[component][at] >> shift)
}

fn first_codestream(dir: &Path) -> PathBuf {
    let mut frames: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the output directory has to be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    frames.sort();
    frames
        .into_iter()
        .next()
        .expect("the encode has to have written a codestream")
}

fn close_enough(got: [i32; 3], want: [u16; 3]) -> bool {
    got.iter()
        .zip(want)
        .all(|(have, expected)| (have - i32::from(expected)).abs() <= CODE_TOLERANCE)
}

#[test]
fn an_hdr10_master_encodes_to_the_addendum_reference_white() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let peak_signal = pq_signal_from_nits(REFERENCE_WHITE_NITS);
    let white_ycbcr = studio_range_ycbcr([peak_signal; 3], BT2020_LUMINANCE_WEIGHTS);
    let patch_ycbcr = studio_range_ycbcr(
        COLOURED_PATCH_NITS.map(pq_signal_from_nits),
        BT2020_LUMINANCE_WEIGHTS,
    );
    let clip = hdr10_clip(dir.path(), white_ycbcr, patch_ycbcr);

    let output_dir = dir.path().join("j2k");
    let result = stream_encode_inprocess(
        &StreamEncodeOptions {
            input: clip,
            output_dir: output_dir.clone(),
            compression_ratio: 2.0,
            fps: postkit::encode::FrameRate::whole(FRAME_RATE),
            source_colour: SourceColour::HdrDcdm {
                source: HdrSource::Hdr10,
                source_peak_nits: REFERENCE_WHITE_NITS as f32,
            },
            rsiz: default_rsiz(),
            ..StreamEncodeOptions::default()
        },
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    );
    assert!(result.success, "the encode failed: {}", result.error);
    assert_eq!(result.frames_encoded, FRAMES);

    let codestream = first_codestream(&output_dir);
    let white = decoded_pixel(&codestream, WIDTH / 4, HEIGHT / 2);
    assert!(
        close_enough(white, ADDENDUM_REFERENCE_WHITE_CODES),
        "D65 white at {REFERENCE_WHITE_NITS} cd/m² came out {white:?}, not the addendum's \
         {ADDENDUM_REFERENCE_WHITE_CODES:?}"
    );

    let transform = HdrDcdmTransform::new(HdrSource::Hdr10, REFERENCE_WHITE_NITS as f32).unwrap();
    let patch = decoded_pixel(&codestream, WIDTH * 3 / 4, HEIGHT / 2);
    let want_patch = transform.pixel(
        rgb48_from_studio_range_ycbcr(patch_ycbcr, BT2020_LUMINANCE_WEIGHTS),
        TWELVE_BIT_MAX_CODE,
    );
    assert!(
        close_enough(patch, want_patch),
        "the coloured patch came out {patch:?}, not the {want_patch:?} the BT.2020 matrix gives"
    );

    let as_if_bt709 = transform.pixel(
        rgb48_from_studio_range_ycbcr(patch_ycbcr, BT709_LUMINANCE_WEIGHTS),
        TWELVE_BIT_MAX_CODE,
    );
    assert!(
        !close_enough(patch, as_if_bt709),
        "the BT.709 reading of the same samples is {as_if_bt709:?}, too close to \
         {want_patch:?} for this to prove the matrix"
    );
}
